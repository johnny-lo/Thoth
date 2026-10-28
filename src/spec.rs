use serde::{Deserialize, Serialize};

fn default_risk() -> String {
    "normal".to_string()
}

/// v6 section 6.3 task spec template. Unknown keys are preserved by the caller
/// (the raw JSON text is what gets stored), not by this struct.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSpec {
    pub goal: String,
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub non_goals: Vec<String>,
    #[serde(default)]
    pub excluded_alternatives: Vec<String>,
    #[serde(default = "default_risk")]
    pub risk: String,
    /// Ids of approved checks (`thoth checks`) this task needs to pass.
    #[serde(default)]
    pub checks: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    #[error("spec is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("spec.goal is required and must not be blank")]
    MissingGoal,
    #[error("spec.acceptance_criteria must contain at least one entry")]
    EmptyAcceptanceCriteria,
    #[error("spec.risk must be one of low/normal/important/high, got '{0}'")]
    BadRisk(String),
    #[error("spec.checks is invalid: {0}")]
    BadChecks(String),
    /// S3-8: `task_type` or `difficulty` holds something other than one of
    /// its listed strings (null counts as absent).
    #[error("spec.{key} must be one of {allowed} (or left out), got {value}")]
    BadLabel {
        key: &'static str,
        allowed: String,
        value: String,
    },
}

/// S3-8: the optional `task_type` label. Read from the raw JSON by
/// `labels` and by `recommend`'s selection; `TaskSpec` has no field for it.
pub const TASK_TYPES: &[&str] = &["code", "research", "review", "docs", "mechanical"];
/// S3-8: the optional `difficulty` label, read the same way.
pub const DIFFICULTIES: &[&str] = &["low", "normal", "high"];

fn check_label(
    raw: &serde_json::Value,
    key: &'static str,
    allowed: &'static [&'static str],
) -> Result<(), SpecError> {
    match raw.get(key) {
        None | Some(serde_json::Value::Null) => Ok(()),
        Some(serde_json::Value::String(s)) if allowed.contains(&s.as_str()) => Ok(()),
        Some(other) => Err(SpecError::BadLabel {
            key,
            allowed: allowed.join(", "),
            value: other.to_string(),
        }),
    }
}

/// The spec's `task_type` and `difficulty` as written; `None` when a key is
/// absent, null or not a string. For event details, so it does not validate.
pub fn labels(json: &str) -> (Option<String>, Option<String>) {
    let Ok(raw) = serde_json::from_str::<serde_json::Value>(json) else {
        return (None, None);
    };
    let read = |key: &str| raw.get(key).and_then(|v| v.as_str()).map(str::to_string);
    (read("task_type"), read("difficulty"))
}

/// The `task_type` values whose delivery is a document (a spec, a research
/// report, a review verdict) rather than code: `task accept` closes these, and
/// `task close` takes them from `in_review` (DOC-1).
pub const DOCUMENT_TASK_TYPES: &[&str] = &["docs", "research", "review"];

/// Whether the spec's `task_type` is one of `DOCUMENT_TASK_TYPES`. `false` for
/// any other value, an absent or non-string key, and JSON that does not parse;
/// like `labels`, it reads the raw JSON and does not validate. Deliberately not
/// "the spec has no checks": a document task may have its own checks.
pub fn is_document_task(json: &str) -> bool {
    labels(json)
        .0
        .is_some_and(|t| DOCUMENT_TASK_TYPES.contains(&t.as_str()))
}

/// Whether the spec's `task_type` is exactly `research` (RES-1a): only these
/// tasks get web tools and must deliver sourced `research` claims. `false` for
/// every other value, including the other document types `docs` and `review`,
/// an absent or non-string key, and JSON that does not parse. Reads the raw
/// JSON like `is_document_task`, so a run's spec copy works too.
pub fn is_research_task(json: &str) -> bool {
    labels(json).0.as_deref() == Some("research")
}

pub fn parse_spec(json: &str) -> Result<TaskSpec, SpecError> {
    let spec: TaskSpec = serde_json::from_str(json)?;
    let raw: serde_json::Value = serde_json::from_str(json)?;
    check_label(&raw, "task_type", TASK_TYPES)?;
    check_label(&raw, "difficulty", DIFFICULTIES)?;
    if spec.goal.trim().is_empty() {
        return Err(SpecError::MissingGoal);
    }
    if spec.acceptance_criteria.is_empty() {
        return Err(SpecError::EmptyAcceptanceCriteria);
    }
    if crate::model::Risk::parse(&spec.risk).is_err() {
        return Err(SpecError::BadRisk(spec.risk.clone()));
    }
    let mut seen = std::collections::HashSet::new();
    for id in &spec.checks {
        if !crate::checks::valid_check_id(id) {
            return Err(SpecError::BadChecks(format!(
                "'{id}' is not a check id ([a-z0-9_-]{{1,32}})"
            )));
        }
        if !seen.insert(id.as_str()) {
            return Err(SpecError::BadChecks(format!("'{id}' is listed twice")));
        }
    }
    Ok(spec)
}

/// brief §9: what a rework dispatch adds to the **run's** copy of the spec.
///
/// The task's own spec is never touched by a rework - only `task update-spec`
/// edits that, and only a Supervisor runs it. `TaskSpec` ignores this key
/// (serde drops unknown fields), so every existing reader keeps working and
/// only `parse_rework` looks at it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ReworkAddendum {
    pub attempt: i64,
    pub previous_run: String,
    /// The attempt number of `previous_run` - the run whose commits the new
    /// worktree starts from and whose review produced `unmet`. **Not**
    /// `attempt - 1`: `latest_completed_run_for_role` skips runs that never
    /// completed, so after a crashed attempt and an `--allow-attempt` override
    /// the findings can be two or more attempts old (R2, fix round 1). `0`
    /// means there was no completed previous run to point at.
    #[serde(default)]
    pub previous_attempt: i64,
    #[serde(default)]
    pub previous_candidate: String,
    /// One line per unmet acceptance criterion, already flattened by
    /// `commands::dispatch::fold_findings`.
    #[serde(default)]
    pub unmet: Vec<String>,
    #[serde(default)]
    pub extra_findings: Vec<String>,
}

/// Reads the `rework` key back out of a run's spec, if there is one. Never
/// fails: a spec with no addendum, an unparseable spec and a `rework` value of
/// the wrong shape are all simply "no addendum".
pub fn parse_rework(spec_json: &str) -> Option<ReworkAddendum> {
    let v: serde_json::Value = serde_json::from_str(spec_json).ok()?;
    let r = v.get("rework")?;
    serde_json::from_value(r.clone()).ok()
}

/// What a dispatch out of `unverified` adds to the **run's** copy of the spec
/// when the newest implementer run's `result.json` was rejected at ingest (a
/// `result_invalid` event). Same rules as `ReworkAddendum`: the task's own spec
/// is untouched, `TaskSpec` ignores the key, and only `parse_reroute` reads it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RerouteAddendum {
    pub previous_run: String,
    /// The event's `detail.reason`, verbatim.
    pub reason: String,
    /// The event's `detail.rule` (v6 §2.5), when it recorded one.
    #[serde(default)]
    pub rule: Option<i64>,
}

/// Reads the `reroute` key back out of a run's spec, if there is one. Never
/// fails: a missing key, an unparseable spec and a value of the wrong shape are
/// all simply "no addendum".
pub fn parse_reroute(spec_json: &str) -> Option<RerouteAddendum> {
    let v: serde_json::Value = serde_json::from_str(spec_json).ok()?;
    let r = v.get("reroute")?;
    serde_json::from_value(r.clone()).ok()
}

/// RD-2 (H14): the refusal message when `raw` holds what the masker takes for
/// a secret, `None` when it holds none. Lines are numbered from the raw text,
/// fields from the parsed string values, so a value escaped in the file is
/// still found. The message names kinds, field paths and line numbers only.
pub(crate) fn secret_refusal(raw: &str, masker: &crate::redact::Masker) -> Option<String> {
    // kind -> the lines it is on, kinds in the order they were first found
    let mut line_kinds: Vec<(String, Vec<usize>)> = Vec::new();
    let mut lines: Vec<usize> = Vec::new();
    for (i, line) in raw.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        for span in masker.find(line) {
            lines.push(i + 1);
            match line_kinds.iter_mut().find(|(k, _)| *k == span.kind) {
                Some((_, ns)) => ns.push(i + 1),
                None => line_kinds.push((span.kind, vec![i + 1])),
            }
        }
    }
    let mut fields: Vec<String> = Vec::new();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(raw) {
        walk_strings(&v, String::new(), masker, &mut fields);
    }
    if lines.is_empty() && fields.is_empty() {
        return None;
    }
    let numbers = |mut ns: Vec<usize>| {
        ns.sort_unstable();
        ns.dedup();
        let word = if ns.len() == 1 { "line" } else { "lines" };
        let list: Vec<String> = ns.iter().map(usize::to_string).collect();
        format!("{word} {}", list.join(", "))
    };
    let items = if fields.is_empty() {
        line_kinds
            .into_iter()
            .map(|(kind, ns)| format!("{kind} at {}", numbers(ns)))
            .collect::<Vec<_>>()
            .join(", ")
    } else if lines.is_empty() {
        format!("{}; line unknown (the value is escaped in the file)", fields.join(", "))
    } else {
        format!("{}; {}", fields.join(", "), numbers(lines))
    };
    Some(format!(
        "refusing: the spec contains what looks like a secret ({items}); nothing was stored, because a spec goes to the worker and the reviewer as written. Remove it, refer to an environment variable by its name instead, and run the command again."
    ))
}

/// Every `KIND in PATH` for the string values and (RD-4) the object keys
/// under `v`, in walk order (array index, then object keys by name), without
/// repeats. A key is checked as parsed, so a `\u` escape in the file hides
/// nothing; in a path, a key stands masked.
fn walk_strings(v: &serde_json::Value, path: String, masker: &crate::redact::Masker, out: &mut Vec<String>) {
    match v {
        serde_json::Value::String(s) => {
            for span in masker.find(s) {
                let item = format!("{} in {path}", span.kind);
                if !out.contains(&item) {
                    out.push(item);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk_strings(item, format!("{path}[{i}]"), masker, out);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map {
                // a key may itself be a secret
                let spans = masker.find(key);
                let key = masker.mask_text(key).0;
                let child = if path.is_empty() { key } else { format!("{path}.{key}") };
                for span in spans {
                    let found = format!("{} in {child}", span.kind);
                    if !out.contains(&found) {
                        out.push(found);
                    }
                }
                walk_strings(item, child, masker, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
      "goal": "Add a retry to the uploader",
      "acceptance_criteria": ["AC1: retries three times", "AC2: logs each attempt"],
      "constraints": ["no new dependencies"],
      "non_goals": ["rewriting the transport"],
      "excluded_alternatives": ["exponential backoff crate - evaluated, too heavy"],
      "risk": "normal"
    }"#;

    #[test]
    fn secret_refusal_names_lines_fields_and_kinds_only() {
        let value = "dummy-rd2-x-value";
        let token = format!("{}{}", ["gh", "p_"].concat(), "dummyDUMMY0123456789dummyDUMMY012345");
        let m = crate::redact::Masker::from_pairs(&[(value, "env:RD2_X")]);
        let raw = format!(
            "{{\n  \"goal\": \"deploy with {value} now\",\n  \"acceptance_criteria\": [\n    \"AC1: plain\",\n    \"AC2: uses {token}\"\n  ],\n  \"risk\": \"normal\"\n}}"
        );
        let msg = secret_refusal(&raw, &m).unwrap();
        assert!(msg.starts_with("refusing: the spec contains what looks like a secret ("), "{msg}");
        assert!(msg.contains("env:RD2_X in goal"), "{msg}");
        assert!(msg.contains("github-token in acceptance_criteria[1]"), "{msg}");
        assert!(msg.contains("lines 2, 5"), "{msg}");
        for secret in [value, token.as_str()] {
            for i in 0..=secret.len() - 8 {
                assert!(!msg.contains(&secret[i..i + 8]), "a piece of a secret at {i}");
            }
        }

        let one_line = serde_json::to_string(&serde_json::from_str::<serde_json::Value>(&raw).unwrap()).unwrap();
        let msg = secret_refusal(&one_line, &m).unwrap();
        assert!(msg.contains("; line 1)"), "{msg}");

        let clean = format!(
            r#"{{"goal":"see task-1-attempt-2","acceptance_criteria":["AC1: read AISTUPIDLEVEL_API_KEY by name","AC2: {}abcde"]}}"#,
            ["s", "k-"].concat()
        );
        assert!(secret_refusal(&clean, &m).is_none());
    }

    #[test]
    fn secret_refusal_checks_object_keys_even_when_escaped() {
        let token = format!("{}{}", ["gh", "p_"].concat(), "dummyDUMMY0123456789dummyDUMMY012345");
        // the token's first letter written as a \u escape
        let escaped = format!("\\u{:04x}{}", u32::from('g'), &token[1..]);
        let m = crate::redact::Masker::from_pairs(&[("dummy-rd4-key-value", "env:RD4_X")]);
        let raw = format!(
            "{{\"goal\":\"g\",\"acceptance_criteria\":[\"AC1: x\"],\"metadata\":{{\"{escaped}\":true,\"list\":[{{\"dummy-rd4-key-value\":1}}]}}}}"
        );
        let formats = crate::redact::Masker::from_pairs(&[]);
        assert!(formats.find(&raw).is_empty(), "the raw text alone hides the token");
        let msg = secret_refusal(&raw, &m).unwrap();
        assert!(msg.starts_with("refusing: the spec contains what looks like a secret ("), "{msg}");
        assert!(msg.contains("github-token in metadata.[redacted:github-token]"), "{msg}");
        assert!(msg.contains("env:RD4_X in metadata.list[0].[redacted:env:RD4_X]"), "{msg}");
        assert!(!msg.contains(&token[4..20]), "{msg}");
        assert!(!msg.contains("dummy-rd4"), "{msg}");

        let clean = r#"{"goal":"g","acceptance_criteria":["AC1: x"],"metadata":{"owner":true}}"#;
        assert!(secret_refusal(clean, &m).is_none());
    }

    #[test]
    fn a_full_spec_parses_with_every_field_preserved() {
        let s = parse_spec(FULL).unwrap();
        assert_eq!(s.goal, "Add a retry to the uploader");
        assert_eq!(s.acceptance_criteria.len(), 2);
        assert_eq!(s.constraints, vec!["no new dependencies".to_string()]);
        assert_eq!(s.non_goals.len(), 1);
        assert_eq!(s.excluded_alternatives.len(), 1);
        assert_eq!(s.risk, "normal");
    }

    #[test]
    fn optional_lists_default_to_empty_and_risk_defaults_to_normal() {
        let s = parse_spec(r#"{"goal":"g","acceptance_criteria":["AC1: x"]}"#).unwrap();
        assert!(s.constraints.is_empty());
        assert!(s.non_goals.is_empty());
        assert!(s.excluded_alternatives.is_empty());
        assert_eq!(s.risk, "normal");
    }

    #[test]
    fn an_empty_goal_is_rejected() {
        let e = parse_spec(r#"{"goal":"   ","acceptance_criteria":["AC1: x"]}"#).unwrap_err();
        assert!(matches!(e, SpecError::MissingGoal));
    }

    #[test]
    fn an_empty_acceptance_criteria_list_is_rejected() {
        let e = parse_spec(r#"{"goal":"g","acceptance_criteria":[]}"#).unwrap_err();
        assert!(matches!(e, SpecError::EmptyAcceptanceCriteria));
    }

    #[test]
    fn an_unknown_risk_is_rejected_and_names_the_value() {
        let e = parse_spec(r#"{"goal":"g","acceptance_criteria":["AC1: x"],"risk":"spicy"}"#)
            .unwrap_err();
        match e {
            SpecError::BadRisk(v) => assert_eq!(v, "spicy"),
            other => panic!("expected BadRisk, got {other}"),
        }
    }

    #[test]
    fn checks_default_to_empty_and_are_validated() {
        let base = |c: &str| {
            format!(r#"{{"goal":"g","acceptance_criteria":["AC1: x"],"checks":{c}}}"#)
        };
        assert!(parse_spec(r#"{"goal":"g","acceptance_criteria":["AC1: x"]}"#)
            .unwrap()
            .checks
            .is_empty());
        assert_eq!(
            parse_spec(&base(r#"["test","clippy"]"#)).unwrap().checks,
            vec!["test".to_string(), "clippy".to_string()]
        );
        for bad in [r#"["test","test"]"#, r#"[""]"#, r#"["Test"]"#] {
            assert!(
                matches!(parse_spec(&base(bad)), Err(SpecError::BadChecks(_))),
                "{bad}"
            );
        }
    }

    fn with_labels(extra: &str) -> String {
        format!(r#"{{"goal":"g","acceptance_criteria":["AC1: x"]{extra}}}"#)
    }

    // S3-8 AC1: the two optional labels.

    #[test]
    fn a_task_type_of_coding_is_rejected_and_the_message_lists_the_five_values() {
        let e = parse_spec(&with_labels(r#","task_type":"coding""#)).unwrap_err();
        assert!(matches!(e, SpecError::BadLabel { key: "task_type", .. }), "{e}");
        let text = e.to_string();
        for v in ["code", "research", "review", "docs", "mechanical", "coding"] {
            assert!(text.contains(v), "{text} lacks {v}");
        }
    }

    #[test]
    fn a_difficulty_of_hard_is_rejected_and_the_message_lists_the_three_values() {
        let e = parse_spec(&with_labels(r#","difficulty":"hard""#)).unwrap_err();
        assert!(matches!(e, SpecError::BadLabel { key: "difficulty", .. }), "{e}");
        let text = e.to_string();
        for v in ["low", "normal", "high", "hard"] {
            assert!(text.contains(v), "{text} lacks {v}");
        }
    }

    #[test]
    fn a_difficulty_that_is_the_number_3_is_rejected() {
        let e = parse_spec(&with_labels(r#","difficulty":3"#)).unwrap_err();
        assert!(matches!(e, SpecError::BadLabel { key: "difficulty", .. }), "{e}");
        assert!(e.to_string().contains("low, normal, high"), "{e}");
    }

    #[test]
    fn labels_that_are_absent_or_null_are_accepted() {
        assert!(parse_spec(&with_labels("")).is_ok());
        assert!(parse_spec(&with_labels(r#","task_type":null,"difficulty":null"#)).is_ok());
        assert_eq!(labels(&with_labels(r#","task_type":null"#)), (None, None));
    }

    #[test]
    fn every_listed_label_value_is_accepted_and_read_back() {
        for t in TASK_TYPES {
            for d in DIFFICULTIES {
                let json = with_labels(&format!(r#","task_type":"{t}","difficulty":"{d}""#));
                assert!(parse_spec(&json).is_ok(), "{json}");
                assert_eq!(labels(&json), (Some(t.to_string()), Some(d.to_string())));
            }
        }
    }

    #[test]
    fn only_docs_research_and_review_are_document_tasks() {
        for t in ["docs", "research", "review"] {
            let json = with_labels(&format!(r#","task_type":"{t}""#));
            assert!(is_document_task(&json), "{json}");
        }
        for extra in [
            r#","task_type":"code""#,
            r#","task_type":"mechanical""#,
            "",
            r#","task_type":null"#,
            r#","task_type":3"#,
        ] {
            let json = with_labels(extra);
            assert!(!is_document_task(&json), "{json}");
        }
        assert!(!is_document_task(r#"{"task_type":"docs""#), "broken JSON");
    }

    #[test]
    fn only_research_is_a_research_task() {
        assert!(is_research_task(&with_labels(r#","task_type":"research""#)));
        for extra in [
            r#","task_type":"docs""#,
            r#","task_type":"review""#,
            r#","task_type":"code""#,
            r#","task_type":"mechanical""#,
            r#","task_type":"Research""#,
            "",
            r#","task_type":null"#,
            r#","task_type":3"#,
            r#","task_type":["research"]"#,
        ] {
            let json = with_labels(extra);
            assert!(!is_research_task(&json), "{json}");
        }
        assert!(!is_research_task(r#"{"task_type":"research""#), "broken JSON");
        let run_copy = r#"{"goal":"g","acceptance_criteria":["AC1: x"],"task_type":"research",
          "rework":{"attempt":2,"previous_run":"R1"},"launch":{"effort":"high"}}"#;
        assert!(is_research_task(run_copy), "a run's spec copy keeps the key");
    }

    #[test]
    fn a_run_copy_with_rework_reroute_and_launch_keys_still_parses() {
        let run_copy = r#"{"goal":"g","acceptance_criteria":["AC1: x"],
          "task_type":"code","difficulty":"high",
          "rework":{"attempt":2,"previous_run":"R1"},
          "reroute":{"previous_run":"R1","reason":"x"},
          "launch":{"effort_requested":"high","effort":"high","effort_note":null,
            "profile_auto":true,"model_auto":true,"effort_auto":true,"reasons":["rule strongest"]}}"#;
        assert!(parse_spec(run_copy).is_ok());
    }

    #[test]
    fn malformed_json_is_rejected() {
        assert!(matches!(parse_spec("{not json"), Err(SpecError::Json(_))));
    }

    #[test]
    fn missing_goal_key_is_a_json_error_not_a_panic() {
        assert!(parse_spec(r#"{"acceptance_criteria":["AC1: x"]}"#).is_err());
    }

    #[test]
    fn parse_reroute_reads_the_addendum_and_a_malformed_one_is_absent() {
        let with = r#"{"goal":"g","acceptance_criteria":["AC1: x"],
          "reroute":{"previous_run":"R1","reason":"claim 2 cites nothing","rule":2}}"#;
        assert!(parse_spec(with).is_ok(), "TaskSpec ignores the key");
        let r = parse_reroute(with).unwrap();
        assert_eq!(r.previous_run, "R1");
        assert_eq!(r.reason, "claim 2 cites nothing");
        assert_eq!(r.rule, Some(2));
        assert!(parse_reroute(FULL).is_none());
        assert!(parse_reroute("{not json").is_none());
        assert!(parse_reroute(r#"{"reroute": 7}"#).is_none());
        assert_eq!(
            parse_reroute(r#"{"reroute":{"previous_run":"R","reason":"x"}}"#).unwrap().rule,
            None
        );
    }

    #[test]
    fn parse_rework_reads_the_addendum_and_task_spec_ignores_it() {
        let reworked = r#"{
          "goal": "Add a retry to the uploader",
          "acceptance_criteria": ["AC1: retries three times"],
          "risk": "normal",
          "rework": {
            "attempt": 2,
            "previous_run": "01RUNPREVIOUS",
            "previous_attempt": 1,
            "previous_candidate": "cafebabe",
            "unmet": ["AC1: the retry never fires (evidence E001)"],
            "extra_findings": ["minor: a stray TODO"]
          }
        }"#;

        // `TaskSpec` drops the unknown key, so every existing reader still works
        let s = parse_spec(reworked).unwrap();
        assert_eq!(s.goal, "Add a retry to the uploader");
        assert_eq!(s.risk, "normal");

        let r = parse_rework(reworked).expect("the rework key is readable");
        assert_eq!(r.attempt, 2);
        assert_eq!(r.previous_run, "01RUNPREVIOUS");
        assert_eq!(r.previous_attempt, 1);
        assert_eq!(r.previous_candidate, "cafebabe");
        assert_eq!(r.unmet.len(), 1);
        assert!(r.unmet[0].contains("the retry never fires"));
        assert_eq!(r.extra_findings, vec!["minor: a stray TODO".to_string()]);

        // a plain spec has no addendum, and junk is not a panic
        assert!(parse_rework(FULL).is_none());
        assert!(parse_rework("{not json").is_none());
        assert!(parse_rework(r#"{"rework": 7}"#).is_none());
        // a partial addendum fills in the defaults rather than failing
        let partial = parse_rework(r#"{"rework":{"attempt":3,"previous_run":"R"}}"#).unwrap();
        assert_eq!(partial.attempt, 3);
        assert!(partial.unmet.is_empty());
        assert!(partial.previous_candidate.is_empty());
        assert_eq!(
            partial.previous_attempt, 0,
            "R2: an addendum written before this field existed still reads back"
        );
    }
}
