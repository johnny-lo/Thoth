//! REV-3: the checks a task's spec requires that did not run on the run
//! under review, and the refusal of a code review report that contradicts
//! them.
//!
//! The only inputs are the task's current spec `checks` field and the
//! `verify_ran` events of the run under review that are not `--at-base`
//! (no `at_commit`), read through each `results[]` item's `check_id`,
//! `exit_code` and `error`. A worker's own commands, its
//! `verification_requests` and `worker_declared` never count as a run.

use crate::model::event_type;
use crate::review_json::{ReviewDocument, Verdict};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

/// The category of the finding a reviewer writes for each required check
/// that did not run (blocking; see `review_json::CATEGORIES`).
pub const CATEGORY: &str = "check_not_run";

/// No `results[]` item of a non-baseline verify of the run names the check:
/// never verified, verified only with `--at-base`, or only another run was.
pub const NO_VERIFY_ROW: &str = "no_verify_row";

/// Items name the check, but none completed: refused (for example not
/// approved), failed to run, timed out, no exit code, or incomplete output.
pub const DID_NOT_COMPLETE: &str = "did_not_complete";

/// The `review_invalid` stage of a report this rule refuses.
pub const STAGE: &str = "required_checks";

/// One required check that did not run, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotRun {
    pub check_id: String,
    pub reason: &'static str,
}

/// The ids a spec's `checks` field lists, in order, each once; read as
/// leniently as `verify` reads it, so the two never disagree on what the
/// spec requires. No field, or an empty one, requires nothing.
pub fn required(task_spec: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let spec: Value = serde_json::from_str(task_spec).unwrap_or(Value::Null);
    for id in spec["checks"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if !out.iter().any(|o| o == id) {
            out.push(id.to_string());
        }
    }
    out
}

/// Which of `required` did not run, given the `detail`s of the run's
/// `verify_ran` events. A detail with an `at_commit` (a `--at-base` verify)
/// is ignored. A check ran when at least one item for it has an integer
/// `exit_code` and a null `error`, whatever the exit code: a check that ran
/// and failed is not this rule's business.
pub fn not_run(required: &[String], verify_details: &[Value]) -> Vec<NotRun> {
    let items: Vec<&Value> = verify_details
        .iter()
        .filter(|d| d.get("at_commit").is_none())
        .flat_map(|d| d["results"].as_array().into_iter().flatten())
        .collect();
    required
        .iter()
        .filter_map(|id| {
            let mine: Vec<&&Value> = items.iter().filter(|r| r["check_id"].as_str() == Some(id)).collect();
            let reason = if mine.is_empty() {
                NO_VERIFY_ROW
            } else if mine.iter().any(|r| r["exit_code"].as_i64().is_some() && r.get("error") == Some(&Value::Null)) {
                return None;
            } else {
                DID_NOT_COMPLETE
            };
            Some(NotRun { check_id: id.clone(), reason })
        })
        .collect()
}

/// `not_run` for the run `run_id` of task `task_id`, read from the events
/// table now. No run (`None`) means no verify row for anything.
pub fn for_run(conn: &Connection, task_id: &str, task_spec: &str, run_id: Option<&str>) -> Result<Vec<NotRun>> {
    let required = required(task_spec);
    if required.is_empty() {
        return Ok(Vec::new());
    }
    let mut details = Vec::new();
    if let Some(run_id) = run_id {
        let mut stmt = conn.prepare(
            "SELECT detail FROM events WHERE task_id = ?1 AND run_id = ?2 AND type = ?3 ORDER BY seq",
        )?;
        let rows = stmt.query_map(rusqlite::params![task_id, run_id, event_type::VERIFY_RAN], |r| {
            r.get::<_, String>(0)
        })?;
        for row in rows {
            details.push(serde_json::from_str(&row?).unwrap_or(Value::Null));
        }
    }
    Ok(not_run(&required, &details))
}

/// The list as JSON: `[{"check_id", "reason"}]`.
pub fn to_json(list: &[NotRun]) -> Value {
    Value::Array(
        list.iter()
            .map(|n| json!({"check_id": n.check_id, "reason": n.reason}))
            .collect(),
    )
}

/// One `CHECK_ID (REASON)` per entry, the way the packet and the reviewer
/// prompt list them.
pub fn lines(list: &[NotRun]) -> Vec<String> {
    list.iter().map(|n| format!("{} ({})", n.check_id, n.reason)).collect()
}

/// Whether the report carries a `check_not_run` finding: an unmet criterion
/// or an extra finding with that category. A met criterion is no finding.
pub fn reports_not_run(doc: &ReviewDocument) -> bool {
    doc.findings
        .criteria
        .iter()
        .any(|c| c.verdict.trim() == "unmet" && c.category.as_deref() == Some(CATEGORY))
        || doc.findings.extra_findings.iter().any(|e| e.category.as_deref() == Some(CATEGORY))
}

/// Why a code review report contradicts the rule, or `None` when it does
/// not: (a) a `check_not_run` finding with a verdict other than REJECT;
/// (b) required checks that did not run and a verdict other than REJECT;
/// (c) required checks that did not run and no `check_not_run` finding.
pub fn refusal(doc: &ReviewDocument, list: &[NotRun]) -> Option<String> {
    let verdict = Verdict::parse(&doc.verdict).unwrap_or(Verdict::Unverified);
    let reported = reports_not_run(doc);
    let named = if list.is_empty() { String::new() } else { lines(list).join(", ") };
    if reported && verdict != Verdict::Reject {
        return Some(format!(
            "review.json says {} but has a {CATEGORY} finding, which is blocking; a required check that did not run means REJECT",
            verdict.as_str()
        ));
    }
    if !list.is_empty() && verdict != Verdict::Reject {
        return Some(format!(
            "review.json says {} but required checks did not run on the reviewed run: {named}; that means REJECT with one {CATEGORY} finding per check",
            verdict.as_str()
        ));
    }
    if !list.is_empty() && !reported {
        return Some(format!(
            "review.json says REJECT but has no {CATEGORY} finding although required checks did not run on the reviewed run: {named}; write one {CATEGORY} extra finding per check"
        ));
    }
    None
}

/// The `review_invalid` detail of a refusal, before its `reason`.
pub fn invalid_detail(list: &[NotRun]) -> Value {
    json!({"stage": STAGE, "checks_not_run": to_json(list)})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn item(id: &str, exit: Value, error: Value) -> Value {
        json!({"check_id": id, "exit_code": exit, "error": error})
    }

    #[test]
    fn required_reads_the_spec_checks_in_order_once_and_leniently() {
        assert_eq!(required(r#"{"checks":["b","a","b"]}"#), ids(&["b", "a"]));
        assert!(required(r#"{"goal":"g"}"#).is_empty());
        assert!(required(r#"{"checks":[]}"#).is_empty());
        assert!(required("{not json").is_empty());
    }

    #[test]
    fn each_required_check_is_judged_from_the_non_baseline_items() {
        let req = ids(&["ok", "failed", "never", "base_only", "refused", "mixed"]);
        let details = vec![
            json!({"results": [
                item("ok", json!(0), Value::Null),
                item("failed", json!(101), Value::Null),
                item("refused", Value::Null, json!("check refused is not approved")),
                item("mixed", Value::Null, json!("timed out after 1s")),
            ]}),
            json!({"at_commit": "abc", "results": [item("base_only", json!(0), Value::Null)]}),
            json!({"results": [item("mixed", json!(0), Value::Null)]}),
        ];
        assert_eq!(
            not_run(&req, &details),
            vec![
                NotRun { check_id: "never".into(), reason: NO_VERIFY_ROW },
                NotRun { check_id: "base_only".into(), reason: NO_VERIFY_ROW },
                NotRun { check_id: "refused".into(), reason: DID_NOT_COMPLETE },
            ]
        );
        // exit 0 with an error (incomplete output) did not complete either
        let incomplete = vec![json!({"results": [item("x", json!(0), json!("output may be incomplete"))]})];
        assert_eq!(not_run(&ids(&["x"]), &incomplete)[0].reason, DID_NOT_COMPLETE);
    }

    #[test]
    fn missing_error_and_present_null_at_commit_do_not_prove_a_run() {
        let missing_error = vec![json!({"results": [{"check_id": "x", "exit_code": 0}]})];
        assert_eq!(not_run(&ids(&["x"]), &missing_error)[0].reason, DID_NOT_COMPLETE);
        let null_base = vec![json!({"at_commit": null, "results": [item("x", json!(0), Value::Null)]})];
        assert_eq!(not_run(&ids(&["x"]), &null_base)[0].reason, NO_VERIFY_ROW);
    }

    /// The prompt section lists each check with its reason, or says there
    /// are none; `check_not_run` is a blocking category the parser takes.
    #[test]
    fn the_prompt_section_lists_what_did_not_run() {
        let list = vec![
            NotRun { check_id: "test".into(), reason: NO_VERIFY_ROW },
            NotRun { check_id: "clippy".into(), reason: DID_NOT_COMPLETE },
        ];
        let s = crate::review_json::required_checks_prompt_section(&lines(&list));
        assert!(s.starts_with("\n## Required checks not run\n"), "{s}");
        assert!(s.contains("\n- test (no_verify_row)\n- clippy (did_not_complete)\n"), "{s}");
        let none = crate::review_json::required_checks_prompt_section(&lines(&[]));
        assert!(!none.contains("\n- "), "{none}");
        assert!(none.contains("None"), "{none}");
        assert!(crate::review_json::category_blocks(Some(CATEGORY)));
        assert!(!crate::review_json::category_blocks(Some("evidence")));
        assert_eq!(
            to_json(&list),
            json!([{"check_id": "test", "reason": "no_verify_row"}, {"check_id": "clippy", "reason": "did_not_complete"}])
        );
    }

    fn doc(verdict: &str, extra_category: Option<&str>) -> ReviewDocument {
        let mut extras = vec![];
        if let Some(c) = extra_category {
            extras.push(json!({"severity": "major", "note": "n", "evidence": [], "category": c}));
        }
        serde_json::from_value(json!({"verdict": verdict, "findings": {
            "criteria": [{"id": "AC1", "verdict": "met", "evidence": ["E001"]}],
            "extra_findings": extras}}))
        .unwrap()
    }

    #[test]
    fn refusal_applies_the_three_clauses() {
        let list = vec![NotRun { check_id: "test".into(), reason: NO_VERIFY_ROW }];
        // (b)
        assert!(refusal(&doc("PASS", None), &list).unwrap().contains("test (no_verify_row)"));
        assert!(refusal(&doc("UNVERIFIED", Some(CATEGORY)), &list).is_some());
        // (c)
        assert!(refusal(&doc("REJECT", Some("evidence")), &list).is_some());
        // accepted
        assert_eq!(refusal(&doc("REJECT", Some(CATEGORY)), &list), None);
        // (a) with an empty list
        assert!(refusal(&doc("PASS", Some(CATEGORY)), &[]).is_some());
        assert_eq!(refusal(&doc("REJECT", Some(CATEGORY)), &[]), None);
        assert_eq!(refusal(&doc("PASS", Some("evidence")), &[]), None);
        // an unmet criterion labelled check_not_run counts as the finding
        let unmet: ReviewDocument = serde_json::from_value(json!({"verdict": "REJECT", "findings": {
            "criteria": [{"id": "AC1", "verdict": "unmet", "note": "test did not run", "category": CATEGORY}],
            "extra_findings": []}}))
        .unwrap();
        assert_eq!(refusal(&unmet, &list), None);
    }
}
