//! MEAS-1: the reviewer of the next attempt marks, for every finding of the
//! previous REJECT, whether it was fixed by changing behaviour, in the review
//! document's `findings.prior_rejection`; Thoth checks each mark when the
//! review arrives and stores the result next to it as
//! `findings.prior_rejection_check`.
//!
//! This is measurement only. Nothing here ever makes a document invalid,
//! refuses a review or touches the verdict, the task status or the
//! UNVERIFIED rules: whatever `prior_rejection` holds, the review is accepted
//! or refused exactly as the same document without it. The marks' codes are
//! checked here, per entry, and never by the document's citation stage, so a
//! bad mark cannot make the review invalid.
//!
//! The typed `review_json::Findings` deliberately has no field for either
//! key: every reader of stored findings (the stuck rule, the rework fold,
//! `review show`, the confirm, the brief) parses them into that struct, and
//! a key it does not know is ignored, so no shape of `prior_rejection` can
//! make that parse fail.

use crate::db::state;
use crate::review_json::Findings;
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

/// The prompt section's heading.
pub const SECTION_HEADING: &str = "## Previous rejection to mark";

/// The stored findings' key for the reviewer's marks, as written.
pub const KEY: &str = "prior_rejection";

/// The stored findings' key for Thoth's check of them.
pub const CHECK_KEY: &str = "prior_rejection_check";

/// The three answers `fixed_by_behaviour` may give.
pub const VALUES: [&str; 3] = ["yes", "no", "not_fixed"];

/// AC2: whether a review has findings to mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// The newest record of a lower attempt is a REJECT.
    Checked,
    /// A first attempt, or no earlier attempt was reviewed.
    NoPreviousReject,
    /// The newest record of a lower attempt is a PASS or UNVERIFIED.
    PreviousNotReject,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Checked => "checked",
            Status::NoPreviousReject => "no_previous_reject",
            Status::PreviousNotReject => "previous_not_reject",
        }
    }
}

/// One finding of the previous REJECT the reviewer has to mark.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpectedFinding {
    /// An unmet criterion's id, or `extra_findings[N]`.
    pub reference: String,
    pub category: Option<String>,
    pub fix_size: Option<String>,
    /// An extra finding's severity; `None` for a criterion.
    pub severity: Option<String>,
    pub note: String,
    pub evidence: Vec<String>,
}

/// AC2: the review a new review marks, worked out for its run under review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub status: Status,
    /// The previous record's id; `None` for `NoPreviousReject`.
    pub review_id: Option<String>,
    pub previous_verdict: Option<String>,
    /// The attempt of the previous record's target run.
    pub previous_attempt: Option<i64>,
    /// The findings to mark, `[]` unless `Checked`.
    pub expected: Vec<ExpectedFinding>,
}

impl Target {
    pub fn no_previous() -> Target {
        Target {
            status: Status::NoPreviousReject,
            review_id: None,
            previous_verdict: None,
            previous_attempt: None,
            expected: Vec::new(),
        }
    }
}

/// One review record of the task, as AC2 sees it.
#[derive(Debug, Clone, Copy)]
pub struct RecordRef<'a> {
    pub review_id: &'a str,
    pub verdict: &'a str,
    /// The attempt of the record's target run; `None` when it is unknown.
    pub attempt: Option<i64>,
    /// The stored findings JSON.
    pub findings: &'a str,
}

/// AC2 (D1): among records in age order, the newest whose target run has a
/// lower attempt than `attempt`.
pub fn previous_index(attempts: &[Option<i64>], attempt: i64) -> Option<usize> {
    attempts.iter().rposition(|a| a.is_some_and(|a| a < attempt))
}

/// AC2: the review a review of `attempt` marks, from the task's records in
/// age order (the review being worked out is not among them).
pub fn target_from(records: &[RecordRef<'_>], attempt: i64) -> Target {
    let attempts: Vec<Option<i64>> = records.iter().map(|r| r.attempt).collect();
    let Some(i) = previous_index(&attempts, attempt) else {
        return Target::no_previous();
    };
    let r = records[i];
    let verdict = r.verdict.trim().to_string();
    let checked = verdict == crate::review_json::Verdict::Reject.as_str();
    Target {
        status: if checked { Status::Checked } else { Status::PreviousNotReject },
        review_id: Some(r.review_id.to_string()),
        previous_verdict: Some(verdict),
        previous_attempt: r.attempt,
        expected: if checked { expected_of(r.findings) } else { Vec::new() },
    }
}

/// AC2: a REJECT's findings to mark: its unmet criteria by id in document
/// order, then `extra_findings[N]` for each extra finding, as stored.
pub fn expected_of(findings_json: &str) -> Vec<ExpectedFinding> {
    let findings: Findings = serde_json::from_str(findings_json).unwrap_or_default();
    let criteria = findings.criteria.iter().filter(|c| c.verdict.trim() == "unmet").map(|c| ExpectedFinding {
        reference: c.id.trim().to_string(),
        category: c.category.clone(),
        fix_size: c.fix_size.clone(),
        severity: None,
        note: c.note.clone().unwrap_or_default(),
        evidence: c.evidence.clone(),
    });
    let extras = findings.extra_findings.iter().enumerate().map(|(i, e)| ExpectedFinding {
        reference: format!("extra_findings[{i}]"),
        category: e.category.clone(),
        fix_size: e.fix_size.clone(),
        severity: Some(e.severity.clone()),
        note: e.note.clone(),
        evidence: e.evidence.clone(),
    });
    criteria.chain(extras).collect()
}

/// AC2: the review a review of `target_run_id` marks, from the task's review
/// records as they are now. A run that is not found has nothing to mark.
pub fn target_for(conn: &Connection, task_id: &str, target_run_id: &str) -> Result<Target> {
    let Some(run) = state::get_run(conn, target_run_id)? else {
        return Ok(Target::no_previous());
    };
    let reviews = state::list_reviews_for_task(conn, task_id)?;
    let mut attempts: std::collections::HashMap<String, Option<i64>> = Default::default();
    for r in &reviews {
        if !attempts.contains_key(&r.target_run_id) {
            let attempt = if r.target_run_id.is_empty() {
                None
            } else {
                state::get_run(conn, &r.target_run_id)?.map(|t| t.attempt)
            };
            attempts.insert(r.target_run_id.clone(), attempt);
        }
    }
    let records: Vec<RecordRef<'_>> = reviews
        .iter()
        .map(|r| RecordRef {
            review_id: &r.review_id,
            verdict: &r.verdict,
            attempt: attempts.get(&r.target_run_id).copied().flatten(),
            findings: &r.findings,
        })
        .collect();
    Ok(target_from(&records, run.attempt))
}

/// One line of prompt text.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn label(v: &Option<String>) -> &str {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty()).unwrap_or("none")
}

/// AC3: the `## Previous rejection to mark` section of a code review prompt.
/// `None` when Thoth could not work out the target; `mask` masks each note.
pub fn prompt_section(target: Option<&Target>, mask: &mut dyn FnMut(&str) -> String) -> String {
    let mut s = format!("\n{SECTION_HEADING}\n");
    let Some(t) = target else {
        s.push_str("Thoth could not work out whether an earlier attempt of this task was rejected. Leave findings.prior_rejection as []. Thoth still checks any marks when it receives your report; they never decide this review's verdict.\n");
        return s;
    };
    match t.status {
        Status::NoPreviousReject => {
            s.push_str("Nothing to mark: no earlier attempt of this task was reviewed. Leave findings.prior_rejection as [].\n");
            return s;
        }
        Status::PreviousNotReject => {
            s.push_str(&format!(
                "Nothing to mark: the newest review of an earlier attempt ({}) was {}, not a REJECT. Leave findings.prior_rejection as [].\n",
                t.review_id.as_deref().unwrap_or("-"),
                t.previous_verdict.as_deref().unwrap_or("-")
            ));
            return s;
        }
        Status::Checked => {}
    }
    let id = t.review_id.as_deref().unwrap_or("-");
    let attempt = t.previous_attempt.map(|a| a.to_string()).unwrap_or_else(|| "-".to_string());
    s.push_str(&format!(
        "Review {id} rejected attempt {attempt} of this task. For each of its findings below, mark whether the candidate under review fixed it:\n"
    ));
    for e in &t.expected {
        let mut labels = format!("category {}, fix_size {}", label(&e.category), label(&e.fix_size));
        if let Some(sev) = &e.severity {
            labels.push_str(&format!(", severity {}", label(&Some(sev.clone()))));
        }
        let codes: Vec<String> = e.evidence.iter().map(|c| c.trim().to_string()).filter(|c| !c.is_empty()).collect();
        let codes = if codes.is_empty() { "none".to_string() } else { codes.join(", ") };
        s.push_str(&format!("- {} [{labels}] {} (evidence {codes})\n", e.reference, one_line(&mask(&e.note))));
    }
    s.push_str("Rules for findings.prior_rejection:\n");
    s.push_str(&format!("- Write exactly one entry per line above, with \"review_id\": \"{id}\" and \"ref\" set to the line's reference; add no other entries.\n"));
    s.push_str("- \"fixed_by_behaviour\" is \"yes\" when the finding was fixed by changing what the program does; \"no\" when it was fixed without a behaviour change (evidence or a verify added, docs, a test-only change, or the finding no longer applies; the note says which); \"not_fixed\" when it is still there.\n");
    s.push_str("- \"yes\" and \"no\" need at least one evidence code in \"evidence\" or a \"diff_at\"; \"not_fixed\" needs a \"note\" or at least one evidence code.\n");
    s.push_str("- Evidence codes come from this task's evidence index.\n");
    s.push_str("- \"diff_at\" is PATH:LINE, PATH a file in this candidate's diff evidence and LINE a line number from 1; write \"\" when there is none, and \"\" for no note.\n");
    s.push_str("- \"ref\" names a finding of the previous review above, not of this review; an edit's \"criterion\" still names a finding of this review.\n");
    s.push_str("- These marks never decide this review's verdict: judge the criteria and give the verdict by the rules above, as if this section were not here.\n");
    s.push_str(&format!(
        "Entry shape: {{\"review_id\":\"{id}\",\"ref\":\"AC1\",\"fixed_by_behaviour\":\"yes\",\"evidence\":[\"E004\"],\"diff_at\":\"src/lib.rs:12\",\"note\":\"\"}}\n"
    ));
    s
}

/// What a JSON value is, for `array_error`.
fn kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// A trimmed string that is not blank.
fn text(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// AC4's checks after `missing` and `duplicate_ref`, on one entry, in order:
/// the lower-case mark, or the reason and detail of the first that fails.
fn judge(
    entry: &serde_json::Map<String, Value>,
    target: &Target,
    is_evidence: &dyn Fn(&str) -> bool,
    diff_paths: &[String],
) -> std::result::Result<String, (&'static str, Option<String>)> {
    // bad_shape (AC4: a null `evidence` is not an array of strings;
    // only the other fields may be null)
    match entry.get("evidence") {
        None => {}
        Some(Value::Array(a)) if a.iter().all(Value::is_string) => {}
        Some(_) => return Err(("bad_shape", Some("evidence is not an array of strings".to_string()))),
    }
    for field in ["review_id", "fixed_by_behaviour", "diff_at", "note"] {
        if let Some(v) = entry.get(field) {
            if !v.is_string() && !v.is_null() {
                return Err(("bad_shape", Some(format!("{field} is {}", kind(v)))));
            }
        }
    }
    // wrong_review_id
    let review_id = text(entry.get("review_id"));
    if review_id.as_deref() != target.review_id.as_deref() {
        return Err(("wrong_review_id", Some(review_id.unwrap_or_else(|| "none".to_string()))));
    }
    // bad_value
    let value = entry.get("fixed_by_behaviour").and_then(Value::as_str).map(|v| v.trim().to_lowercase()).unwrap_or_default();
    if !VALUES.contains(&value.as_str()) {
        let shown = entry.get("fixed_by_behaviour").and_then(Value::as_str).unwrap_or("none");
        return Err(("bad_value", Some(shown.to_string())));
    }
    // no_support
    let codes: Vec<String> = entry
        .get("evidence")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(|c| c.trim().to_uppercase())
        .filter(|c| !c.is_empty())
        .collect();
    let diff_at = text(entry.get("diff_at"));
    let note = text(entry.get("note"));
    let supported = if value == "not_fixed" {
        note.is_some() || !codes.is_empty()
    } else {
        !codes.is_empty() || diff_at.is_some()
    };
    if !supported {
        let need = if value == "not_fixed" { "a note or an evidence code" } else { "an evidence code or a diff_at" };
        return Err(("no_support", Some(format!("{value} needs {need}"))));
    }
    // foreign_evidence
    if let Some(code) = codes.iter().find(|c| !is_evidence(c)) {
        return Err(("foreign_evidence", Some(code.clone())));
    }
    if let Some(at) = diff_at {
        // bad_diff_at
        let parsed = at.rsplit_once(':').and_then(|(path, line)| {
            let path = path.trim();
            let line = line.trim();
            let whole = !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit());
            (whole && !path.is_empty() && line.parse::<u64>().is_ok_and(|n| n >= 1)).then(|| path.to_string())
        });
        let Some(path) = parsed else {
            return Err(("bad_diff_at", Some(at)));
        };
        // diff_at_not_in_diff (D3: the path only, against every diff row)
        let path = path.replace('\\', "/");
        if !diff_paths.iter().any(|p| p.replace('\\', "/") == path) {
            return Err(("diff_at_not_in_diff", Some(path)));
        }
    }
    Ok(value)
}

/// AC4/AC5: Thoth's check of `raw` (the document's `findings.prior_rejection`
/// as written, `None` when absent) against `target`. `is_evidence` says
/// whether an upper-cased code is evidence of this task; `diff_paths` are the
/// `files_changed` of the run under review's diff evidence.
pub fn check(target: &Target, raw: Option<&Value>, is_evidence: &dyn Fn(&str) -> bool, diff_paths: &[String]) -> Value {
    let (items, array_error): (Vec<&Value>, Option<String>) = match raw {
        None | Some(Value::Null) => (Vec::new(), None),
        Some(Value::Array(a)) => (a.iter().collect(), None),
        Some(other) => (Vec::new(), Some(format!("findings.prior_rejection is {}, not an array", kind(other)))),
    };
    let reference = |v: &Value| v.as_object().and_then(|o| o.get("ref")).and_then(Value::as_str).map(|r| r.trim().to_string());
    let checked = target.status == Status::Checked;
    let mut entries = Vec::new();
    let (mut yes, mut no, mut not_fixed, mut unmarked) = (0usize, 0usize, 0usize, 0usize);
    if checked {
        for e in &target.expected {
            let hits: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(_, v)| reference(v).as_deref() == Some(e.reference.as_str()))
                .map(|(i, _)| i)
                .collect();
            let outcome: std::result::Result<String, (&str, Option<String>)> = match hits.as_slice() {
                [] => Err(("missing", None)),
                [i] => judge(items[*i].as_object().expect("a ref means an object"), target, is_evidence, diff_paths),
                many => Err((
                    "duplicate_ref",
                    Some(format!("entries {}", many.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(", "))),
                )),
            };
            let index = hits.first().copied();
            let (mark, reason, detail) = match outcome {
                Ok(v) => {
                    match v.as_str() {
                        "yes" => yes += 1,
                        "no" => no += 1,
                        _ => not_fixed += 1,
                    }
                    (Some(v), None, None)
                }
                Err((r, d)) => {
                    unmarked += 1;
                    (None, Some(r), d)
                }
            };
            entries.push(json!({
                "ref": e.reference,
                "category": e.category,
                "fix_size": e.fix_size,
                "fixed_by_behaviour": mark,
                "marked": mark.is_some(),
                "reason": reason,
                "detail": detail,
                "index": index,
            }));
        }
    }
    let mut extra_entries = Vec::new();
    for (i, v) in items.iter().enumerate() {
        let r = reference(v);
        let reason = if !checked {
            "nothing_to_mark"
        } else if !v.is_object() || r.is_none() {
            "bad_shape"
        } else if target.expected.iter().any(|e| Some(&e.reference) == r.as_ref()) {
            continue;
        } else {
            "unknown_ref"
        };
        extra_entries.push(json!({"index": i, "ref": r, "reason": reason}));
    }
    json!({
        "status": target.status.as_str(),
        "review_id": target.review_id,
        "previous_verdict": target.previous_verdict,
        "expected": target.expected.iter().map(|e| e.reference.clone()).collect::<Vec<_>>(),
        "entries": entries,
        "extra_entries": extra_entries,
        "array_error": array_error,
        "counts": {"yes": yes, "no": no, "not_fixed": not_fixed, "unmarked": unmarked},
    })
}

/// AC5: a check's summary for the `verdict` event.
pub fn summary(check: &Value) -> Value {
    let n = |k: &str| check["counts"][k].as_u64().unwrap_or(0);
    json!({
        "status": check["status"],
        "review_id": check["review_id"],
        "expected": check["expected"].as_array().map_or(0, Vec::len),
        "yes": n("yes"),
        "no": n("no"),
        "not_fixed": n("not_fixed"),
        "unmarked": n("unmarked"),
    })
}

/// What a review record stores besides its typed findings (AC1, AC5).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Marks {
    /// `findings.prior_rejection` as written; `None` when it was absent,
    /// null or an empty array.
    pub stored: Option<Value>,
    /// Thoth's check; `None` when there is nothing to record (D5).
    pub check: Option<Value>,
}

impl Marks {
    /// The stored findings JSON: exactly the typed findings as before when
    /// there is nothing to add, so such a record keeps its bytes.
    pub fn findings_json(&self, findings: &Findings) -> String {
        if self.stored.is_none() && self.check.is_none() {
            return serde_json::to_string(findings).unwrap_or_else(|_| "{}".to_string());
        }
        let mut v = serde_json::to_value(findings).unwrap_or_else(|_| json!({}));
        self.add_to(&mut v);
        v.to_string()
    }

    /// Adds the two keys (when set) to a findings object.
    pub fn add_to(&self, findings: &mut Value) {
        if let Some(o) = findings.as_object_mut() {
            if let Some(s) = &self.stored {
                o.insert(KEY.to_string(), s.clone());
            }
            if let Some(c) = &self.check {
                o.insert(CHECK_KEY.to_string(), c.clone());
            }
        }
    }

    /// AC5: the `verdict` event's `prior_rejection`: the check's summary,
    /// null when no check is stored.
    pub fn event_value(&self) -> Value {
        self.check.as_ref().map(summary).unwrap_or(Value::Null)
    }

    /// AC5 (D6): the two keys of stored findings JSON, unchanged, for a
    /// confirm's record to copy.
    pub fn from_stored(findings_json: &str) -> Marks {
        let v: Value = serde_json::from_str(findings_json).unwrap_or(Value::Null);
        Marks {
            stored: v.get(KEY).cloned().filter(|s| !s.is_null()),
            check: v.get(CHECK_KEY).cloned().filter(Value::is_object),
        }
    }
}

/// The document's `findings.prior_rejection` as written, `None` when the
/// document or the key is absent.
pub fn raw_marks(json_text: &str) -> Option<Value> {
    let v: Value = serde_json::from_str(json_text.trim_start_matches('\u{feff}')).ok()?;
    v.get("findings")?.get(KEY).cloned()
}

/// AC1: what is stored for `raw`: as written, unless absent, null or `[]`.
pub fn stored_marks(raw: Option<&Value>) -> Option<Value> {
    raw.filter(|v| !v.is_null() && v.as_array().is_none_or(|a| !a.is_empty())).cloned()
}

/// AC1-AC5 (D8): the marks of a code review of `target_run_id` arriving now
/// (a reviewer run, plain or edit mode, or `review record`). A research
/// task's source check gets none. A failure to read what the check needs
/// stores the marks as written without a check; it never fails the review.
pub fn for_review(conn: &Connection, task: &state::Task, target_run_id: &str, json_text: &str) -> Marks {
    if crate::spec::is_research_task(&task.spec) {
        return Marks::default();
    }
    let raw = raw_marks(json_text);
    let stored = stored_marks(raw.as_ref());
    let worked_out = (|| -> Result<Option<Value>> {
        let target = target_for(conn, &task.task_id, target_run_id)?;
        if target.status == Status::NoPreviousReject && stored.is_none() {
            return Ok(None);
        }
        let evidence = state::list_evidence_for_task(conn, &task.task_id)?;
        let codes: std::collections::HashSet<String> = evidence.iter().map(|e| e.code.clone()).collect();
        let diff_paths: Vec<String> = evidence
            .iter()
            .filter(|e| e.kind == "diff" && e.run_id.as_deref() == Some(target_run_id))
            .flat_map(|e| {
                let d: Value = serde_json::from_str(&e.detail).unwrap_or(Value::Null);
                d["files_changed"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .collect();
        Ok(Some(check(&target, raw.as_ref(), &|c| codes.contains(c), &diff_paths)))
    })();
    Marks { stored, check: worked_out.ok().flatten() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REJECTED: &str = r#"{"criteria":[
        {"id":" AC1 ","verdict":"unmet","evidence":["E001"],"note":"n","fix_size":"S","category":"docs"},
        {"id":"AC2","verdict":"met","evidence":["E001"]}],
        "extra_findings":[{"severity":"minor","note":"x","evidence":[]}]}"#;

    #[test]
    fn prior_rejection_target_is_the_newest_record_of_a_lower_attempt() {
        let rec = |id, verdict, attempt| RecordRef { review_id: id, verdict, attempt, findings: REJECTED };
        let records = [rec("R1", "REJECT", Some(1)), rec("R2", "UNVERIFIED", Some(2)), rec("R0", "PASS", None)];
        assert_eq!(target_from(&records, 1), Target::no_previous());
        let t = target_from(&records, 2);
        assert_eq!((t.status, t.review_id.as_deref()), (Status::Checked, Some("R1")));
        let refs: Vec<&str> = t.expected.iter().map(|e| e.reference.as_str()).collect();
        assert_eq!(refs, ["AC1", "extra_findings[0]"]);
        let t = target_from(&records, 3);
        assert_eq!((t.status, t.review_id.as_deref(), t.expected.len()), (Status::PreviousNotReject, Some("R2"), 0));
    }

    #[test]
    fn prior_rejection_check_never_marks_a_null_field_as_bad_shape() {
        let t = target_from(&[RecordRef { review_id: "R1", verdict: "REJECT", attempt: Some(1), findings: REJECTED }], 2);
        let raw = json!([
            {"review_id": " R1 ", "ref": "AC1", "fixed_by_behaviour": " YES ", "evidence": [" e001 ", " "], "diff_at": null, "note": null},
            {"review_id": "R1", "ref": "extra_findings[0]", "fixed_by_behaviour": "no", "diff_at": "src\\a.rs:7"}
        ]);
        let c = check(&t, Some(&raw), &|code| code == "E001", &["src/a.rs".to_string()]);
        assert_eq!(c["counts"], json!({"yes": 1, "no": 1, "not_fixed": 0, "unmarked": 0}), "{c}");
        assert_eq!(c["entries"][0]["fixed_by_behaviour"], "yes", "{c}");
        assert_eq!(summary(&c)["expected"], 2);
    }

    #[test]
    fn prior_rejection_check_reads_a_null_evidence_as_bad_shape() {
        let t = target_from(&[RecordRef { review_id: "R1", verdict: "REJECT", attempt: Some(1), findings: REJECTED }], 2);
        let raw = json!([{"review_id": "R1", "ref": "AC1", "fixed_by_behaviour": "yes", "evidence": null, "diff_at": "src/a.rs:7", "note": null}]);
        let c = check(&t, Some(&raw), &|code| code == "E001", &["src/a.rs".to_string()]);
        assert_eq!(c["entries"][0]["reason"], "bad_shape", "{c}");
        assert_eq!(c["counts"], json!({"yes": 0, "no": 0, "not_fixed": 0, "unmarked": 2}), "{c}");
    }
}
