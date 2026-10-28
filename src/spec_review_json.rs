//! `spec-review.json` — a review of a task's spec before its first dispatch
//! (SR-1, decision H9).
//!
//! The reviewer judges every acceptance criterion `ok` or `revise` against
//! four checks, and binds each finding to a criterion and a `file:line` (or
//! `spec`) location. The verdict is `APPROVE` (the spec can be dispatched) or
//! `REVISE` (it has to change first). Validation is as strict as
//! `review_json`'s: a document the control plane cannot check is not a
//! verdict.

use serde::{Deserialize, Serialize};

/// The only two answers a spec reviewer may give.
pub const SPEC_REVIEW_VERDICTS: [&str; 2] = ["APPROVE", "REVISE"];

/// The two per-criterion answers.
pub const CRITERION_VERDICTS: [&str; 2] = ["ok", "revise"];

/// H9's four checks: every criterion can be verified, everything the spec
/// names exists, the criteria do not contradict each other, and "how to test"
/// is written as step-by-step actions.
pub const CHECKS: [&str; 4] = ["verifiable", "exists", "consistent", "steps"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecVerdict {
    Approve,
    Revise,
}

impl SpecVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            SpecVerdict::Approve => SPEC_REVIEW_VERDICTS[0],
            SpecVerdict::Revise => SPEC_REVIEW_VERDICTS[1],
        }
    }

    pub fn parse(s: &str) -> Option<SpecVerdict> {
        match s.trim() {
            "APPROVE" => Some(SpecVerdict::Approve),
            "REVISE" => Some(SpecVerdict::Revise),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecReviewDocument {
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub criteria: Vec<SpecCriterion>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecCriterion {
    pub id: String,
    pub verdict: String,
    #[serde(default)]
    pub findings: Vec<SpecFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpecFinding {
    #[serde(default)]
    pub check: String,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub note: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SpecReviewError {
    #[error("spec-review.json is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("spec-review.json verdict must be APPROVE or REVISE, got '{0}'")]
    BadVerdict(String),
    #[error("spec-review.json criterion '{id}' verdict must be ok or revise, got '{verdict}'")]
    BadCriterionVerdict { id: String, verdict: String },
    #[error("spec-review.json does not judge acceptance criterion '{0}'")]
    MissingCriterion(String),
    #[error("spec-review.json judges '{0}', which is not an acceptance criterion of this task")]
    UnknownCriterion(String),
    #[error("spec-review.json judges criterion '{0}' more than once")]
    DuplicateCriterion(String),
    #[error("spec-review.json criterion '{0}' is revise but lists no finding")]
    ReviseWithoutFinding(String),
    #[error("spec-review.json criterion '{id}' has a finding whose check '{check}' is not one of verifiable, exists, consistent, steps")]
    BadCheck { id: String, check: String },
    #[error("spec-review.json criterion '{0}' has a finding with no location (write file:line, or spec for the spec text itself)")]
    BlankLocation(String),
    #[error("spec-review.json criterion '{0}' has a finding with no note")]
    BlankNote(String),
    #[error("spec-review.json says APPROVE but marks criterion '{id}' revise")]
    ApproveWithRevise { id: String },
    #[error("spec-review.json says REVISE but marks no criterion revise")]
    ReviseWithoutRevise,
    /// SR-3: an APPROVE proposes no edits.
    #[error("spec-review.json says APPROVE but proposes {0} edit(s); an APPROVE proposes none")]
    ApproveWithEdits(usize),
    /// SR-3: an edit review's document always lists its edits.
    #[error("spec-review.json has no edits array; an edit review lists its edits (an empty array when it proposes none)")]
    NoEdits,
    /// SR-3: an edit without one of its seven properties, or with another.
    #[error("spec-review.json edits array is malformed (every edit has exactly field, op, at, anchor, text, criterion, reason): {0}")]
    BadEdits(String),
}

/// Shape and self-consistency only. Coverage against the task's criteria is
/// `check_coverage`.
pub fn parse_spec_review(json: &str) -> Result<SpecReviewDocument, SpecReviewError> {
    let doc: SpecReviewDocument = serde_json::from_str(json)?;
    check_document(&doc, false)?;
    Ok(doc)
}

/// The self-consistency rules of a parsed document, returning its verdict.
/// `edits_present` (SR-3, edit mode) lets a REVISE mark no criterion
/// `revise`: the edits are what it asks to change.
pub fn check_document(
    doc: &SpecReviewDocument,
    edits_present: bool,
) -> Result<SpecVerdict, SpecReviewError> {
    let Some(verdict) = SpecVerdict::parse(&doc.verdict) else {
        return Err(SpecReviewError::BadVerdict(doc.verdict.clone()));
    };
    for c in &doc.criteria {
        if !CRITERION_VERDICTS.contains(&c.verdict.trim()) {
            return Err(SpecReviewError::BadCriterionVerdict {
                id: c.id.clone(),
                verdict: c.verdict.clone(),
            });
        }
        // An `ok` criterion may carry findings too (suggestions); they are
        // held to the same rules.
        for f in &c.findings {
            if !CHECKS.contains(&f.check.trim()) {
                return Err(SpecReviewError::BadCheck {
                    id: c.id.clone(),
                    check: f.check.clone(),
                });
            }
            if f.location.trim().is_empty() {
                return Err(SpecReviewError::BlankLocation(c.id.clone()));
            }
            if f.note.trim().is_empty() {
                return Err(SpecReviewError::BlankNote(c.id.clone()));
            }
        }
        // A `revise` with nothing to change is not actionable.
        if c.verdict.trim() == "revise" && c.findings.is_empty() {
            return Err(SpecReviewError::ReviseWithoutFinding(c.id.clone()));
        }
    }
    let first_revise = doc.criteria.iter().find(|c| c.verdict.trim() == "revise");
    match (verdict, first_revise) {
        (SpecVerdict::Approve, Some(c)) => {
            Err(SpecReviewError::ApproveWithRevise { id: c.id.clone() })
        }
        (SpecVerdict::Revise, None) if !edits_present => Err(SpecReviewError::ReviseWithoutRevise),
        _ => Ok(verdict),
    }
}

/// Every expected criterion exactly once, and nothing else - the same rules
/// as `review_json::check_coverage`. `expected` comes from
/// `review_json::ac_ids`.
pub fn check_coverage(doc: &SpecReviewDocument, expected: &[String]) -> Result<(), SpecReviewError> {
    for id in expected {
        if !doc.criteria.iter().any(|c| c.id.trim() == id) {
            return Err(SpecReviewError::MissingCriterion(id.clone()));
        }
    }
    for (i, c) in doc.criteria.iter().enumerate() {
        if !expected.iter().any(|id| id == c.id.trim()) {
            return Err(SpecReviewError::UnknownCriterion(c.id.clone()));
        }
        if doc.criteria[..i]
            .iter()
            .any(|earlier| earlier.id.trim() == c.id.trim())
        {
            return Err(SpecReviewError::DuplicateCriterion(c.id.clone()));
        }
    }
    Ok(())
}

/// The JSON Schema a spec reviewer's final message is held to (SR-2: codex
/// `--output-schema`, antigravity `--json-schema`). Strict, as the API's
/// structured output requires: no extra keys anywhere and every property
/// required. It only shapes the document; `parse_spec_review` and
/// `check_coverage` still decide whether it is a verdict.
pub fn output_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdict", "criteria"],
        "properties": {
            "verdict": {"type": "string", "enum": SPEC_REVIEW_VERDICTS},
            "criteria": {"type": "array", "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["id", "verdict", "findings"],
                "properties": {
                    "id": {"type": "string"},
                    "verdict": {"type": "string", "enum": CRITERION_VERDICTS},
                    "findings": {"type": "array", "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["check", "location", "note"],
                        "properties": {
                            "check": {"type": "string", "enum": CHECKS},
                            "location": {"type": "string"},
                            "note": {"type": "string"},
                        },
                    }},
                },
            }},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const APPROVE: &str = r#"{"verdict":"APPROVE","criteria":[
        {"id":"AC1","verdict":"ok","findings":[]},
        {"id":"AC2","verdict":"ok","findings":[{"check":"verifiable","location":"spec","note":"name the log line"}]}]}"#;

    fn ids(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn revise_ac1_with(finding: &str) -> String {
        format!(
            r#"{{"verdict":"REVISE","criteria":[{{"id":"AC1","verdict":"revise","findings":[{finding}]}}]}}"#
        )
    }

    #[test]
    fn u1_a_valid_approve_parses() {
        let doc = parse_spec_review(APPROVE).unwrap();
        assert_eq!(doc.criteria.len(), 2);
    }

    #[test]
    fn u2_a_lowercase_verdict_is_refused_with_the_whole_sentence() {
        let err = parse_spec_review(r#"{"verdict":"approve","criteria":[]}"#).unwrap_err();
        assert!(matches!(err, SpecReviewError::BadVerdict(ref v) if v == "approve"));
        assert_eq!(
            err.to_string(),
            "spec-review.json verdict must be APPROVE or REVISE, got 'approve'"
        );
    }

    #[test]
    fn u3_a_bad_criterion_verdict_is_refused() {
        let doc = r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"maybe","findings":[]}]}"#;
        match parse_spec_review(doc) {
            Err(SpecReviewError::BadCriterionVerdict { id, verdict }) => {
                assert_eq!(id, "AC1");
                assert_eq!(verdict, "maybe");
            }
            other => panic!("expected BadCriterionVerdict, got {other:?}"),
        }
    }

    #[test]
    fn u4_findings_are_checked() {
        let bare = r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"revise","findings":[]}]}"#;
        match parse_spec_review(bare) {
            Err(SpecReviewError::ReviseWithoutFinding(id)) => assert_eq!(id, "AC1"),
            other => panic!("expected ReviseWithoutFinding, got {other:?}"),
        }
        match parse_spec_review(&revise_ac1_with(
            r#"{"check":"vibes","location":"spec","note":"n"}"#,
        )) {
            Err(SpecReviewError::BadCheck { id, check }) => {
                assert_eq!(id, "AC1");
                assert_eq!(check, "vibes");
            }
            other => panic!("expected BadCheck, got {other:?}"),
        }
        match parse_spec_review(&revise_ac1_with(
            r#"{"check":"exists","location":"  ","note":"n"}"#,
        )) {
            Err(SpecReviewError::BlankLocation(id)) => assert_eq!(id, "AC1"),
            other => panic!("expected BlankLocation, got {other:?}"),
        }
        match parse_spec_review(&revise_ac1_with(
            r#"{"check":"exists","location":"src/a.rs:1","note":""}"#,
        )) {
            Err(SpecReviewError::BlankNote(id)) => assert_eq!(id, "AC1"),
            other => panic!("expected BlankNote, got {other:?}"),
        }
    }

    #[test]
    fn u5_the_verdict_must_agree_with_the_criteria() {
        let contradictory = r#"{"verdict":"APPROVE","criteria":[
            {"id":"AC1","verdict":"ok","findings":[]},
            {"id":"AC2","verdict":"revise","findings":[{"check":"steps","location":"spec","note":"write the steps"}]}]}"#;
        match parse_spec_review(contradictory) {
            Err(SpecReviewError::ApproveWithRevise { id }) => assert_eq!(id, "AC2"),
            other => panic!("expected ApproveWithRevise, got {other:?}"),
        }
        let empty_revise = r#"{"verdict":"REVISE","criteria":[
            {"id":"AC1","verdict":"ok","findings":[]},
            {"id":"AC2","verdict":"ok","findings":[]}]}"#;
        assert!(matches!(
            parse_spec_review(empty_revise),
            Err(SpecReviewError::ReviseWithoutRevise)
        ));
    }

    #[test]
    fn u6_coverage_needs_every_criterion_exactly_once() {
        let expected = ids(&["AC1", "AC2"]);
        let only_ac1 = parse_spec_review(
            r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok"}]}"#,
        )
        .unwrap();
        match check_coverage(&only_ac1, &expected) {
            Err(SpecReviewError::MissingCriterion(id)) => assert_eq!(id, "AC2"),
            other => panic!("expected MissingCriterion, got {other:?}"),
        }
        let three = parse_spec_review(
            r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok"},{"id":"AC2","verdict":"ok"},{"id":"AC3","verdict":"ok"}]}"#,
        )
        .unwrap();
        match check_coverage(&three, &expected) {
            Err(SpecReviewError::UnknownCriterion(id)) => assert_eq!(id, "AC3"),
            other => panic!("expected UnknownCriterion, got {other:?}"),
        }
        let twice = parse_spec_review(
            r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok"},{"id":"AC2","verdict":"ok"},{"id":"AC1","verdict":"ok"}]}"#,
        )
        .unwrap();
        match check_coverage(&twice, &expected) {
            Err(SpecReviewError::DuplicateCriterion(id)) => assert_eq!(id, "AC1"),
            other => panic!("expected DuplicateCriterion, got {other:?}"),
        }
        let full = parse_spec_review(APPROVE).unwrap();
        assert!(check_coverage(&full, &expected).is_ok());
    }

    #[test]
    fn u7_verdicts_parse_strictly() {
        assert_eq!(SpecVerdict::parse(" APPROVE "), Some(SpecVerdict::Approve));
        assert_eq!(SpecVerdict::parse("PASS"), None);
        assert_eq!(SpecVerdict::Approve.as_str(), "APPROVE");
    }

    #[test]
    fn u8_the_output_schema_is_strict_and_names_the_four_checks() {
        use serde_json::json;
        let s = output_schema();
        assert_eq!(s["type"], json!("object"));
        assert_eq!(s["properties"]["criteria"]["type"], json!("array"));
        assert_eq!(s["additionalProperties"], json!(false));
        assert_eq!(s["required"], json!(["verdict", "criteria"]));
        assert_eq!(s["properties"]["verdict"]["enum"], json!(["APPROVE", "REVISE"]));
        let c = &s["properties"]["criteria"]["items"];
        assert_eq!(c["type"], json!("object"));
        assert_eq!(c["additionalProperties"], json!(false));
        assert_eq!(c["required"], json!(["id", "verdict", "findings"]));
        assert_eq!(c["properties"]["verdict"]["enum"], json!(["ok", "revise"]));
        let f = &c["properties"]["findings"]["items"];
        assert_eq!(f["type"], json!("object"));
        assert_eq!(f["additionalProperties"], json!(false));
        assert_eq!(f["required"], json!(["check", "location", "note"]));
        assert_eq!(
            f["properties"]["check"]["enum"],
            json!(["verifiable", "exists", "consistent", "steps"])
        );
        assert!(parse_spec_review(
            &serde_json::to_string(&json!({"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}]}))
                .unwrap()
        )
        .is_ok());
    }
}
