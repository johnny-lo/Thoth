//! `.agent/review.json` — the reviewer's only reporting channel, and the
//! reviewer's half of v6 §10.3's Single Writer rule.
//!
//! The document shape is v6 §10.5's `findings` comment block plus the `verdict`
//! brief §4 requires. Validation is deliberately as strict as the worker's: a
//! report the control plane cannot check is not a verdict, and lands the task
//! `unverified` rather than silently becoming a PASS.

use crate::spec::TaskSpec;
use serde::{Deserialize, Serialize};

/// v6 §4: the only three answers a reviewer may give.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Reject,
    Unverified,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Pass => "PASS",
            Verdict::Reject => "REJECT",
            Verdict::Unverified => "UNVERIFIED",
        }
    }

    pub fn parse(s: &str) -> Option<Verdict> {
        match s.trim() {
            "PASS" => Some(Verdict::Pass),
            "REJECT" => Some(Verdict::Reject),
            "UNVERIFIED" => Some(Verdict::Unverified),
            _ => None,
        }
    }

    /// v6 §6.1: which task status this verdict settles on.
    pub fn task_status(&self) -> crate::model::TaskStatus {
        match self {
            Verdict::Pass => crate::model::TaskStatus::Pass,
            Verdict::Reject => crate::model::TaskStatus::Reject,
            Verdict::Unverified => crate::model::TaskStatus::Unverified,
        }
    }
}

/// The two per-criterion answers v6 §10.5's comment block spells out.
pub const CRITERION_VERDICTS: [&str; 2] = ["met", "unmet"];

/// How big the smallest fix for a finding is. `thoth retro` counts it, and an
/// edit review's fix may be bound only to an S or M finding
/// (`review_fix_edits::check_size`).
pub const FIX_SIZES: [&str; 3] = ["S", "M", "L"];

/// What the codex / agy schema offers for "no `fix_size` or `category`";
/// `parse_review` reads it as absent.
const NO_LABEL: &str = "none";

/// The fixed list a reviewer labels each finding with, so `thoth retro` can
/// group repeated findings without a model. The reviewer prompts, the codex /
/// agy schema and `retro` all take the names and wording from here.
pub const CATEGORIES: [(&str, &str); 12] = [
    ("spec_mismatch", "the change does not do what a criterion says, or does what a constraint or non-goal rules out"),
    ("spec_error", "the spec itself is wrong, contradictory, or cannot be met as written"),
    ("test_missing", "a behaviour change has no test, or its test would also pass on the old code"),
    ("test_interface", "a test bypasses the interface (reads the database or internal state, only compares a fixed string or the exit code, or repeats the implementation's logic)"),
    ("error_path", "a failure is swallowed or mis-reported, or leaves partial state behind"),
    ("concurrency", "a race, check-then-act, lock or transaction problem"),
    ("platform", "behaviour differs between Windows and Linux (paths, shells, encodings, processes)"),
    ("docs", "docs, help text or comments do not match the behaviour"),
    ("evidence", "a required command was not run or not cited, or a claim has no evidence"),
    ("files", "a file outside the allowed set changed, a dependency was added, or a protected file was touched"),
    ("security", "a secret, credential or permission is exposed"),
    ("other", "none of the above"),
];

/// The reviewer rule every review prompt's `## Rules` carries right before
/// `SUITE_EVIDENCE_RULE`.
pub const INTERFACE_TEST_RULE: &str ="New or changed tests must check behaviour through the interface: a CLI command, its --json output, or a public function. A test that reads the database or other internal state directly, only compares a fixed string, only checks the exit code, or repeats the implementation's logic is a finding: report it with category test_interface.";

/// TESTS-1 (AC6): the rule every review prompt ends its `## Rules` with, the
/// number after `INTERFACE_TEST_RULE`. The worker prompt's `SUITE_NOTE` tells
/// the implementer to run only the tests the spec names; this tells the
/// reviewer where the whole-suite evidence is instead (the `verify: ` rows the
/// control plane collected) and what does not count as it.
pub const SUITE_EVIDENCE_RULE: &str = "The implementer is told to run only the tests the spec names; the whole suite is run by the control plane's verify, which appears in the evidence index as rows labelled verify: with collected_by control_plane. Do not judge a criterion unmet or UNVERIFIED because the implementer did not run the whole suite itself, and do not count an implementer's cut-off or empty whole-suite output as missing evidence when a verify row covers the same check. Use only a candidate verify row for the same run and check, with successful completion and usable output; a verify@base: row, a failed, refused, timed-out or incomplete verify does not prove that check passed. When no usable verify row exists for a check, report what is missing in an extra finding with category evidence; do not treat an existing row as success or waive an explicit evidence requirement.";

/// The category names alone, in `CATEGORIES` order.
pub fn category_names() -> Vec<&'static str> {
    CATEGORIES.iter().map(|(name, _)| *name).collect()
}

/// The `## Output` lines every review prompt uses to explain `fix_size` and
/// `category`; the caller adds how a met criterion carries them.
pub fn finding_labels_prompt() -> String {
    let mut s = String::new();
    s.push_str("Every unmet criterion and every extra finding carries \"fix_size\" and \"category\". \"fix_size\" is the smallest fix that would resolve it: S (at most 10 changed lines in one file), M (at most about 50 changed lines, or more than one file, no design change), L (larger, or it needs a design or spec change).\n");
    s.push_str(&format!("Every \"fix_size\" must be one of: {}.\n", FIX_SIZES.join(", ")));
    s.push_str(&format!(
        "Every \"category\" must be one of: {}.\n",
        category_names().join(", ")
    ));
    for (name, about) in CATEGORIES {
        s.push_str(&format!("- {name}: {about}\n"));
    }
    s
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewDocument {
    #[serde(default)]
    pub verdict: String,
    #[serde(default)]
    pub findings: Findings,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Findings {
    #[serde(default)]
    pub criteria: Vec<CriterionFinding>,
    #[serde(default)]
    pub extra_findings: Vec<ExtraFinding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<SourceFinding>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceFinding {
    pub id: String,
    pub url: String,
    pub verdict: String,
    pub reason: String,
}

/// Stable identifiers shared by the prompt, validation and rework. `claim`
/// counts every claim in the delivery, just like ingest's diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceToCheck {
    pub id: String,
    pub claim: usize,
    pub claim_text: String,
    pub url: String,
    pub quote: String,
}

pub fn sources_to_check(result: &serde_json::Value) -> Vec<SourceToCheck> {
    let mut out = Vec::new();
    if let Some(claims) = result["claims"].as_array() {
        for (i, claim) in claims.iter().enumerate() {
            if claim["kind"] != "research" {
                continue;
            }
            if let Some(sources) = claim["sources"].as_array() {
                for source in sources {
                    out.push(SourceToCheck {
                        id: format!("S{}", out.len() + 1),
                        claim: i + 1,
                        claim_text: claim["text"].as_str().unwrap_or_default().to_string(),
                        url: source["url"].as_str().unwrap_or_default().trim().to_string(),
                        quote: source["quote"].as_str().unwrap_or_default().to_string(),
                    });
                }
            }
        }
    }
    out
}

/// Unsupported sources take precedence over unavailable ones.
pub fn source_verdict(sources: &[SourceFinding]) -> Option<Verdict> {
    if sources.is_empty() {
        None
    } else if sources.iter().any(|s| s.verdict == "not_supported") {
        Some(Verdict::Reject)
    } else if sources.iter().any(|s| s.verdict == "unreachable") {
        Some(Verdict::Unverified)
    } else {
        Some(Verdict::Pass)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CriterionFinding {
    pub id: String,
    pub verdict: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default)]
    pub note: Option<String>,
    /// One of `FIX_SIZES`, upper-cased by `parse_review`. Left out of the
    /// stored JSON when absent, so an unlabelled document is stored as it
    /// always was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_size: Option<String>,
    /// One of `CATEGORIES`, lower-cased by `parse_review`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExtraFinding {
    #[serde(default)]
    pub severity: String,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix_size: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewError {
    #[error("review.json is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("review.json verdict must be PASS, REJECT or UNVERIFIED, got '{0}'")]
    BadVerdict(String),
    #[error("review.json criterion '{id}' verdict must be met or unmet, got '{verdict}'")]
    BadCriterionVerdict { id: String, verdict: String },
    #[error("review.json does not judge acceptance criterion '{0}'")]
    MissingCriterion(String),
    #[error("review.json judges '{0}', which is not an acceptance criterion of this task")]
    UnknownCriterion(String),
    #[error("review.json criterion '{0}' is unmet but cites no evidence and gives no note")]
    UnsupportedUnmet(String),
    #[error("review.json criterion '{0}' is met but cites no evidence")]
    UncitedMet(String),
    #[error("review.json judges criterion '{0}' more than once")]
    DuplicateCriterion(String),
    #[error("review.json says PASS but marks criterion '{id}' unmet")]
    PassWithUnmetCriterion { id: String },
    #[error("review.json {at} fix_size must be S, M or L, got '{value}'")]
    BadFixSize { at: String, value: String },
    #[error("review.json {at} category must be one of: {}, got '{value}'", category_names().join(", "))]
    BadCategory { at: String, value: String },
    #[error("review.json source check must not judge criteria")]
    SourceCheckCriteria,
    #[error("review.json source check has no sources to check")]
    NoSources,
    #[error("review.json does not judge source '{0}'")]
    MissingSource(String),
    #[error("review.json judges unknown source '{0}'")]
    UnknownSource(String),
    #[error("review.json judges source '{0}' more than once")]
    DuplicateSource(String),
    #[error("review.json source '{id}' verdict must be supported, not_supported or unreachable, got '{verdict}'")]
    BadSourceVerdict { id: String, verdict: String },
    #[error("review.json source '{id}' url must be '{expected}', got '{actual}'")]
    SourceUrl { id: String, expected: String, actual: String },
    #[error("review.json source '{0}' needs a reason for not_supported or unreachable")]
    SourceReason(String),
    #[error("review.json says {declared} but sources compute {computed}")]
    SourceConsistency { declared: String, computed: String },
}

/// Source documents have no criterion judgements or implicit model verdict.
pub fn parse_source_check(
    json: &str,
    expected: &[SourceToCheck],
) -> Result<ReviewDocument, ReviewError> {
    let mut doc: ReviewDocument = serde_json::from_str(json)?;
    let declared = Verdict::parse(&doc.verdict)
        .ok_or_else(|| ReviewError::BadVerdict(doc.verdict.clone()))?;
    if !doc.findings.criteria.is_empty() {
        return Err(ReviewError::SourceCheckCriteria);
    }
    if expected.is_empty() || doc.findings.sources.is_empty() {
        return Err(ReviewError::NoSources);
    }
    for expected in expected {
        if !doc.findings.sources.iter().any(|s| s.id == expected.id) {
            return Err(ReviewError::MissingSource(expected.id.clone()));
        }
    }
    for source in &mut doc.findings.sources {
        let target = expected.iter().find(|e| e.id == source.id)
            .ok_or_else(|| ReviewError::UnknownSource(source.id.clone()))?;
        if !["supported", "not_supported", "unreachable"].contains(&source.verdict.as_str()) {
            return Err(ReviewError::BadSourceVerdict {
                id: source.id.clone(), verdict: source.verdict.clone(),
            });
        }
        source.url = source.url.trim().to_string();
        if source.url != target.url.trim() {
            return Err(ReviewError::SourceUrl {
                id: source.id.clone(), expected: target.url.trim().to_string(), actual: source.url.clone(),
            });
        }
        if source.verdict != "supported" && source.reason.trim().is_empty() {
            return Err(ReviewError::SourceReason(source.id.clone()));
        }
    }
    for (i, source) in doc.findings.sources.iter().enumerate() {
        if doc.findings.sources[..i].iter().any(|s| s.id == source.id) {
            return Err(ReviewError::DuplicateSource(source.id.clone()));
        }
    }
    for (i, e) in doc.findings.extra_findings.iter_mut().enumerate() {
        normalise_labels(&format!("extra_findings[{i}]"), &mut e.fix_size, &mut e.category)?;
    }
    let computed = source_verdict(&doc.findings.sources).expect("nonempty sources");
    if declared != computed {
        return Err(ReviewError::SourceConsistency {
            declared: doc.verdict.clone(), computed: computed.as_str().to_string(),
        });
    }
    Ok(doc)
}

/// Strict structured output for codex and agy, sharing extra finding labels
/// and fields with ordinary reviews.
pub fn source_check_schema() -> serde_json::Value {
    let mut schema = output_schema();
    let extra = schema["properties"]["findings"]["properties"]["extra_findings"].clone();
    schema["properties"]["findings"] = serde_json::json!({
        "type": "object", "additionalProperties": false,
        "required": ["sources", "extra_findings"],
        "properties": {
            "sources": {"type": "array", "items": {
                "type": "object", "additionalProperties": false,
                "required": ["id", "url", "verdict", "reason"],
                "properties": {
                    "id": {"type": "string"}, "url": {"type": "string"},
                    "verdict": {"type": "string", "enum": ["supported", "not_supported", "unreachable"]},
                    "reason": {"type": "string"}
                }
            }},
            "extra_findings": extra
        }
    });
    schema
}

/// Shape and self-consistency only. Coverage against the task's criteria is
/// `check_coverage`, and citation existence is the caller's (it needs the DB).
pub fn parse_review(json: &str) -> Result<ReviewDocument, ReviewError> {
    let mut doc: ReviewDocument = serde_json::from_str(json)?;
    for c in &mut doc.findings.criteria {
        let at = c.id.clone();
        normalise_labels(&at, &mut c.fix_size, &mut c.category)?;
    }
    for (i, e) in doc.findings.extra_findings.iter_mut().enumerate() {
        let at = format!("extra_findings[{i}]");
        normalise_labels(&at, &mut e.fix_size, &mut e.category)?;
    }
    if Verdict::parse(&doc.verdict).is_none() {
        return Err(ReviewError::BadVerdict(doc.verdict.clone()));
    }
    for c in &doc.findings.criteria {
        if !CRITERION_VERDICTS.contains(&c.verdict.trim()) {
            return Err(ReviewError::BadCriterionVerdict {
                id: c.id.clone(),
                verdict: c.verdict.clone(),
            });
        }
        // An unmet criterion with neither a citation nor a note is not
        // actionable, and brief §9 folds exactly these into the next attempt's
        // spec - an empty one would make the rework a guess.
        if c.verdict.trim() == "unmet"
            && c.evidence.is_empty()
            && c.note
                .as_deref()
                .map(|n| n.trim().is_empty())
                .unwrap_or(true)
        {
            return Err(ReviewError::UnsupportedUnmet(c.id.clone()));
        }
        // Whole-branch review Important 2: the reviewer prompt's rule 2 says
        // "cite at least one evidence code for each judgement", and this was
        // the one rule the prompt stated and the control plane did not check.
        // An all-`met`, zero-citation document used to settle the task `pass`
        // - terminal, trusted, hidden from `status` - from a reviewer that
        // need never have opened a single record. `unmet` keeps its own,
        // looser rule above: the absence of evidence cannot be cited, so a
        // note is an acceptable substitute there and is not here.
        // A blank string is not a citation either: `[""]` counts as `[]`.
        if c.verdict.trim() == "met" && c.evidence.iter().all(|e| e.trim().is_empty()) {
            return Err(ReviewError::UncitedMet(c.id.clone()));
        }
    }
    // Fix round 1 (I2): the reviewer prompt states this as rule 5 ("PASS only
    // if every criterion is met") and nothing downstream re-checks it, so a
    // report that contradicted itself used to settle the task `pass` - a
    // terminal, trusted status - on its own admission that a criterion is
    // unmet. Rejecting it here routes it through `review_invalid` like any
    // other unusable report, which lands the task `unverified` instead.
    if Verdict::parse(&doc.verdict) == Some(Verdict::Pass) {
        if let Some(c) = doc
            .findings
            .criteria
            .iter()
            .find(|c| c.verdict.trim() == "unmet")
        {
            return Err(ReviewError::PassWithUnmetCriterion { id: c.id.clone() });
        }
    }
    Ok(doc)
}

/// A finding's `fix_size` and `category` in their stored form: blank or
/// `NO_LABEL` is absent (the strict schema makes codex and agy send
/// `NO_LABEL` on a met criterion), sizes upper-case, categories lower-case,
/// anything else refused. Absent is accepted: the labels feed statistics, and
/// a whole review is not worth losing over them.
fn normalise_labels(
    at: &str,
    fix_size: &mut Option<String>,
    category: &mut Option<String>,
) -> Result<(), ReviewError> {
    let blank_is_none = |v: &Option<String>| {
        v.as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case(NO_LABEL))
            .map(str::to_string)
    };
    *fix_size = match blank_is_none(fix_size) {
        None => None,
        Some(v) => match FIX_SIZES.iter().find(|s| s.eq_ignore_ascii_case(&v)) {
            Some(s) => Some(s.to_string()),
            None => return Err(ReviewError::BadFixSize { at: at.to_string(), value: v }),
        },
    };
    *category = match blank_is_none(category) {
        None => None,
        Some(v) => match CATEGORIES.iter().find(|(name, _)| name.eq_ignore_ascii_case(&v)) {
            Some((name, _)) => Some(name.to_string()),
            None => return Err(ReviewError::BadCategory { at: at.to_string(), value: v }),
        },
    };
    Ok(())
}

/// The JSON Schema a codex reviewer's final message is held to
/// (`codex exec --output-schema`, scope §2.4 / E3b). Strict, as the API's
/// structured output requires: no extra keys anywhere and every property
/// required, so `note` is always present and may be `""`. It only shapes the
/// document; `parse_review` and `check_coverage` still decide whether it is a
/// verdict.
pub fn output_schema() -> serde_json::Value {
    let codes = serde_json::json!({"type": "array", "items": {"type": "string"}});
    // Strict means required, so NO_LABEL stands for "no label" (a met
    // criterion). Not "": Gemini refuses an empty enum value.
    let fix_size = serde_json::json!({"type": "string", "enum": FIX_SIZES.iter().chain([&NO_LABEL]).collect::<Vec<_>>()});
    let mut category_enum = category_names();
    category_enum.push(NO_LABEL);
    let category = serde_json::json!({"type": "string", "enum": category_enum});
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdict", "findings"],
        "properties": {
            "verdict": {"type": "string", "enum": ["PASS", "REJECT", "UNVERIFIED"]},
            "findings": {
                "type": "object",
                "additionalProperties": false,
                "required": ["criteria", "extra_findings"],
                "properties": {
                    "criteria": {"type": "array", "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["id", "verdict", "evidence", "note", "fix_size", "category"],
                        "properties": {
                            "id": {"type": "string"},
                            "verdict": {"type": "string", "enum": CRITERION_VERDICTS},
                            "evidence": codes,
                            "note": {"type": "string"},
                            "fix_size": fix_size,
                            "category": category,
                        },
                    }},
                    "extra_findings": {"type": "array", "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["severity", "note", "evidence", "fix_size", "category"],
                        "properties": {
                            "severity": {"type": "string"},
                            "note": {"type": "string"},
                            "evidence": codes,
                            "fix_size": fix_size,
                            "category": category,
                        },
                    }},
                },
            },
        },
    })
}

/// The criterion ids a review must judge (Open question 13).
///
/// v6 §10.5 writes `findings.criteria[].id` as `"AC1"`, but
/// `spec.acceptance_criteria` is free text. The text before the first `:` is
/// used when it looks like an id - non-empty, at most 16 characters, no
/// whitespace - and otherwise the position decides. So both
/// `"AC1: prints hello"` and `"prints hello"` become `AC1`.
pub fn ac_ids(spec: &TaskSpec) -> Vec<String> {
    spec.acceptance_criteria
        .iter()
        .enumerate()
        .map(|(i, ac)| {
            let head = ac.split(':').next().unwrap_or("").trim();
            if !head.is_empty()
                && head.chars().count() <= 16
                && !head.chars().any(char::is_whitespace)
            {
                head.to_string()
            } else {
                format!("AC{}", i + 1)
            }
        })
        .collect()
}

/// brief §8: every acceptance criterion must appear **exactly once**, and
/// nothing else may.
///
/// The second half matters as much as the first: a reviewer that invents `AC3`
/// is either reading a different spec or making things up, and either way its
/// verdict cannot be trusted.
///
/// Whole-branch review Minor 4: "at least once" let a document judge the same
/// criterion twice with opposite answers (`[{AC1 met},{AC1 unmet}]`), which
/// passed coverage and then had its verdict decided by whichever rule read the
/// list first. This is the reviewer side of the guarantee; the spec side (two
/// criteria whose ids collide, e.g. `"Build: a"` and `"Build: b"`, which
/// `ac_ids` both derive `Build` from) is a documented Supervisor
/// responsibility, not a code check.
pub fn check_coverage(doc: &ReviewDocument, expected: &[String]) -> Result<(), ReviewError> {
    for id in expected {
        if !doc.findings.criteria.iter().any(|c| c.id.trim() == id) {
            return Err(ReviewError::MissingCriterion(id.clone()));
        }
    }
    for (i, c) in doc.findings.criteria.iter().enumerate() {
        if !expected.iter().any(|id| id == c.id.trim()) {
            return Err(ReviewError::UnknownCriterion(c.id.clone()));
        }
        if doc.findings.criteria[..i]
            .iter()
            .any(|earlier| earlier.id.trim() == c.id.trim())
        {
            return Err(ReviewError::DuplicateCriterion(c.id.clone()));
        }
    }
    Ok(())
}

/// Every code the review cites anywhere, upper-cased, de-duplicated, in
/// first-seen order.
pub fn cited_codes(doc: &ReviewDocument) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let push = |code: &String, out: &mut Vec<String>| {
        let code = code.trim().to_uppercase();
        if !code.is_empty() && !out.contains(&code) {
            out.push(code);
        }
    };
    for c in &doc.findings.criteria {
        for code in &c.evidence {
            push(code, &mut out);
        }
    }
    for e in &doc.findings.extra_findings {
        for code in &e.evidence {
            push(code, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with(criteria: &[&str]) -> TaskSpec {
        TaskSpec {
            goal: "g".to_string(),
            acceptance_criteria: criteria.iter().map(|s| s.to_string()).collect(),
            constraints: vec![],
            non_goals: vec![],
            excluded_alternatives: vec![],
            risk: "normal".to_string(),
            checks: vec![],
        }
    }

    const PASS: &str = r#"{
      "verdict": "PASS",
      "findings": {
        "criteria": [
          {"id": "AC1", "verdict": "met", "evidence": ["E001", "E004"]},
          {"id": "AC2", "verdict": "met", "evidence": ["E004"], "note": "the diff covers it"}
        ],
        "extra_findings": [
          {"severity": "minor", "note": "a stray TODO", "evidence": ["E004"]}
        ]
      }
    }"#;

    /// Every object in the schema: no extra keys, every property required.
    fn assert_strict(v: &serde_json::Value, at: &str) {
        if v["type"] == "object" {
            assert_eq!(v["additionalProperties"], false, "{at}");
            let mut keys: Vec<&str> = v["properties"].as_object().unwrap().keys().map(|k| k.as_str()).collect();
            let mut required: Vec<&str> = v["required"]
                .as_array()
                .unwrap()
                .iter()
                .map(|k| k.as_str().unwrap())
                .collect();
            keys.sort();
            required.sort();
            assert_eq!(keys, required, "{at}: every property is required");
            for (k, sub) in v["properties"].as_object().unwrap() {
                assert_strict(sub, &format!("{at}.{k}"));
            }
        }
        if let Some(items) = v.get("items") {
            assert_strict(items, &format!("{at}[]"));
        }
    }

    #[test]
    fn the_output_schema_is_strict_and_is_the_one_measured_in_e3b() {
        let s = output_schema();
        assert_strict(&s, "schema");
        assert_eq!(s["properties"]["verdict"]["enum"], serde_json::json!(["PASS", "REJECT", "UNVERIFIED"]));
        let criterion = &s["properties"]["findings"]["properties"]["criteria"]["items"];
        assert_eq!(criterion["properties"]["verdict"]["enum"], serde_json::json!(CRITERION_VERDICTS));
        assert_eq!(criterion["properties"]["note"]["type"], "string", "note is always present");
        let measured: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/codex/review-output-schema.json"
        )))
        .unwrap();
        assert_eq!(s, measured);
    }

    // TESTS-1 (AC6): `SUITE_EVIDENCE_RULE` has no test of its own here. The
    // rule is checked where it is used, as English, through the public
    // prompt builders: `build_review_prompt_asks_for_fix_size_category_and_
    // interface_tests` in src/adapter/claude_code.rs, src/adapter/codex.rs
    // and src/adapter/antigravity/mod.rs (review round 1, test_interface).

    /// Whether `fix_size` or `category` appears as a key anywhere in `v`.
    fn has_label_key(v: &serde_json::Value) -> bool {
        match v {
            serde_json::Value::Object(m) => m
                .iter()
                .any(|(k, sub)| k == "fix_size" || k == "category" || has_label_key(sub)),
            serde_json::Value::Array(a) => a.iter().any(has_label_key),
            _ => false,
        }
    }

    #[test]
    fn an_unlabelled_review_is_stored_without_fix_size_or_category_keys() {
        let doc = parse_review(PASS).unwrap();
        let stored = serde_json::to_value(&doc.findings).unwrap();
        assert!(!has_label_key(&stored), "{stored}");
    }

    #[test]
    fn fix_size_and_category_are_normalised_where_the_review_is_read() {
        let doc = parse_review(r#"{"verdict":"REJECT","findings":{"criteria":[
            {"id":"AC1","verdict":"unmet","note":"n","fix_size":" s ","category":"Test_Interface"},
            {"id":"AC2","verdict":"met","evidence":["E001"],"fix_size":"","category":"  "}],
            "extra_findings":[{"severity":"minor","note":"x","fix_size":"l","category":" DOCS "}]}}"#)
        .unwrap();
        let c = &doc.findings.criteria;
        assert_eq!(c[0].fix_size.as_deref(), Some("S"));
        assert_eq!(c[0].category.as_deref(), Some("test_interface"));
        assert_eq!(c[1].fix_size, None, "an empty string is no value");
        assert_eq!(c[1].category, None, "a blank string is no value");
        let e = &doc.findings.extra_findings[0];
        assert_eq!(e.fix_size.as_deref(), Some("L"));
        assert_eq!(e.category.as_deref(), Some("docs"));
        // a met criterion carrying valid labels is accepted too
        let met = r#"{"verdict":"PASS","findings":{"criteria":[
            {"id":"AC1","verdict":"met","evidence":["E001"],"fix_size":"M","category":"other"}]}}"#;
        assert!(parse_review(met).is_ok());
    }

    #[test]
    fn an_unknown_fix_size_or_category_is_rejected_and_says_where() {
        let big = r#"{"verdict":"REJECT","findings":{"criteria":[
            {"id":"AC1","verdict":"unmet","note":"n","fix_size":"XL"}]}}"#;
        match parse_review(big) {
            Err(ReviewError::BadFixSize { at, value }) => {
                assert_eq!(at, "AC1");
                assert_eq!(value, "XL");
            }
            other => panic!("expected BadFixSize, got {other:?}"),
        }
        assert_eq!(
            parse_review(big).unwrap_err().to_string(),
            "review.json AC1 fix_size must be S, M or L, got 'XL'"
        );
        let typo = r#"{"verdict":"REJECT","findings":{"criteria":[
            {"id":"AC1","verdict":"unmet","note":"n"}],
            "extra_findings":[{"severity":"minor","note":"x","category":"typo"}]}}"#;
        match parse_review(typo) {
            Err(ReviewError::BadCategory { at, value }) => {
                assert_eq!(at, "extra_findings[0]");
                assert_eq!(value, "typo");
            }
            other => panic!("expected BadCategory, got {other:?}"),
        }
        assert_eq!(
            parse_review(typo).unwrap_err().to_string(),
            "review.json extra_findings[0] category must be one of: spec_mismatch, spec_error, test_missing, test_interface, error_path, concurrency, platform, docs, evidence, files, security, other, got 'typo'"
        );
    }

    #[test]
    fn the_output_schema_offers_the_fix_sizes_and_categories_or_none() {
        let s = output_schema();
        let findings = &s["properties"]["findings"]["properties"];
        let sizes = serde_json::json!(["S", "M", "L", "none"]);
        let categories = serde_json::json!([
            "spec_mismatch", "spec_error", "test_missing", "test_interface", "error_path",
            "concurrency", "platform", "docs", "evidence", "files", "security", "other", "none"
        ]);
        for items in [&findings["criteria"]["items"], &findings["extra_findings"]["items"]] {
            assert_eq!(items["properties"]["fix_size"]["type"], "string");
            assert_eq!(items["properties"]["fix_size"]["enum"], sizes);
            assert_eq!(items["properties"]["category"]["type"], "string");
            assert_eq!(items["properties"]["category"]["enum"], categories);
        }
        assert_strict(&s, "schema");
    }

    /// Gemini's function declarations refuse an empty enum value, and agy's
    /// reviewer is held to the same schemas as codex's.
    #[test]
    fn no_schema_offers_an_empty_enum_value() {
        fn empty_enum_paths(v: &serde_json::Value, at: &str, found: &mut Vec<String>) {
            match v {
                serde_json::Value::Object(map) => {
                    for (k, child) in map {
                        let here = format!("{at}.{k}");
                        if k == "enum" {
                            for (i, e) in child.as_array().into_iter().flatten().enumerate() {
                                if e.as_str().is_some_and(|s| s.is_empty()) {
                                    found.push(format!("{here}[{i}]"));
                                }
                            }
                        }
                        empty_enum_paths(child, &here, found);
                    }
                }
                serde_json::Value::Array(items) => {
                    for (i, child) in items.iter().enumerate() {
                        empty_enum_paths(child, &format!("{at}[{i}]"), found);
                    }
                }
                _ => {}
            }
        }
        for (name, schema) in [
            ("review", output_schema()),
            ("spec review", crate::spec_review_json::output_schema()),
            ("source check", source_check_schema()),
        ] {
            let mut found = Vec::new();
            empty_enum_paths(&schema, name, &mut found);
            assert!(found.is_empty(), "{name} schema has empty enum values at {found:?}");
        }
    }

    #[test]
    fn source_check_schema_is_strict() {
        let s = source_check_schema();
        assert_strict(&s, "schema");
        let findings = &s["properties"]["findings"]["properties"];
        assert!(findings.get("criteria").is_none());
        assert_eq!(findings["sources"]["items"]["properties"]["verdict"]["enum"],
            serde_json::json!(["supported", "not_supported", "unreachable"]));
        assert_eq!(findings["extra_findings"], output_schema()["properties"]["findings"]["properties"]["extra_findings"]);
    }

    #[test]
    fn source_verdict_covers_every_priority_and_empty_input() {
        let source = |verdict: &str| SourceFinding { id: "S1".into(), url: "file:///a".into(),
            verdict: verdict.into(), reason: "why".into() };
        assert_eq!(source_verdict(&[]), None);
        assert_eq!(source_verdict(&[source("supported"), source("supported")]), Some(Verdict::Pass));
        assert_eq!(source_verdict(&[source("supported"), source("unreachable")]), Some(Verdict::Unverified));
        assert_eq!(source_verdict(&[source("not_supported")]), Some(Verdict::Reject));
        assert_eq!(source_verdict(&[source("unreachable"), source("not_supported"), source("supported")]), Some(Verdict::Reject));
    }

    #[test]
    fn source_numbering_uses_all_claim_positions_and_preserves_source_order() {
        let result = serde_json::json!({"claims": [
            {"kind":"completion", "text":"done"},
            {"kind":"research", "text":"one", "sources":[{"url":" file:///a ","quote":"A"}, {"url":"file:///b","quote":"B"}]},
            {"kind":"completion", "text":"done too"},
            {"kind":"research", "text":"two", "sources":[{"url":"file:///c","quote":"C"}]}
        ]});
        assert_eq!(serde_json::to_value(sources_to_check(&result)).unwrap(), serde_json::json!([
            {"id":"S1", "claim":2, "claim_text":"one", "url":"file:///a", "quote":"A"},
            {"id":"S2", "claim":2, "claim_text":"one", "url":"file:///b", "quote":"B"},
            {"id":"S3", "claim":4, "claim_text":"two", "url":"file:///c", "quote":"C"}
        ]));
    }

    #[test]
    fn source_check_validates_shape_coverage_urls_reasons_labels_and_consistency() {
        let expected = sources_to_check(&serde_json::json!({"claims":[{"kind":"research", "text":"claim", "sources":[{"url":" file:///a ", "quote":"A"}]}]}));
        let base = serde_json::json!({"verdict":"PASS", "findings":{"sources":[{"id":"S1", "url":" file:///a ", "verdict":"supported", "reason":""}], "extra_findings":[]}});
        let parse = |v: serde_json::Value| parse_source_check(&v.to_string(), &expected);
        let doc = parse(base.clone()).unwrap();
        assert_eq!(doc.findings.sources[0].url, "file:///a");
        assert!(doc.findings.criteria.is_empty());
        assert!(cited_codes(&doc).is_empty());
        assert!(matches!(parse_source_check("{not json", &expected), Err(ReviewError::Json(_))));
        assert!(matches!(parse_source_check(&base.to_string(), &[]), Err(ReviewError::NoSources)));
        let mutate = |path: &[&str], value: serde_json::Value| {
            let mut v = base.clone();
            let mut at = &mut v;
            for key in path { at = &mut at[*key]; }
            *at = value;
            v
        };
        assert!(matches!(parse(mutate(&["verdict"], serde_json::json!("MAYBE"))), Err(ReviewError::BadVerdict(_))));
        assert!(matches!(parse(mutate(&["findings", "criteria"], serde_json::json!([{"id":"AC1","verdict":"met"}]))), Err(ReviewError::SourceCheckCriteria)));
        assert!(matches!(parse(mutate(&["findings", "sources"], serde_json::json!([]))), Err(ReviewError::NoSources)));
        let mut v = base.clone(); v["findings"]["sources"][0]["id"] = serde_json::json!("S2");
        assert!(matches!(parse(v), Err(ReviewError::MissingSource(_))));
        let mut v = base.clone(); v["findings"]["sources"].as_array_mut().unwrap().push(serde_json::json!({"id":"S2", "url":"file:///b", "verdict":"supported", "reason":""}));
        assert!(matches!(parse(v), Err(ReviewError::UnknownSource(_))));
        let mut v = base.clone(); let s = v["findings"]["sources"][0].clone(); v["findings"]["sources"].as_array_mut().unwrap().push(s);
        assert!(matches!(parse(v), Err(ReviewError::DuplicateSource(_))));
        for (key, val) in [("url", "file:///b"), ("verdict", "met")] {
            let mut v = base.clone(); v["findings"]["sources"][0][key] = serde_json::json!(val);
            let error = parse(v).unwrap_err();
            assert!(matches!(error, ReviewError::SourceUrl {..} | ReviewError::BadSourceVerdict {..}));
        }
        for verdict in ["not_supported", "unreachable"] {
            let mut v = base.clone(); v["findings"]["sources"][0]["verdict"] = serde_json::json!(verdict);
            v["findings"]["sources"][0]["reason"] = serde_json::json!(" \n ");
            assert!(matches!(parse(v.clone()), Err(ReviewError::SourceReason(_))));
            v["findings"]["sources"][0]["reason"] = serde_json::json!("quote missing");
            assert!(matches!(parse(v.clone()), Err(ReviewError::SourceConsistency {..})));
            v["verdict"] = serde_json::json!(if verdict == "not_supported" {"REJECT"} else {"UNVERIFIED"});
            assert!(parse(v).is_ok());
        }
        let mut v = base.clone(); v["findings"]["extra_findings"] = serde_json::json!([{"note":"n", "evidence":[" e001 "], "fix_size":" s ", "category":" DOCS "}]);
        let doc = parse(v.clone()).unwrap();
        assert_eq!(doc.findings.extra_findings[0].fix_size.as_deref(), Some("S"));
        assert_eq!(doc.findings.extra_findings[0].category.as_deref(), Some("docs"));
        assert_eq!(cited_codes(&doc), vec!["E001"]);
        v["findings"]["extra_findings"][0]["fix_size"] = serde_json::json!("XL");
        assert!(matches!(parse(v), Err(ReviewError::BadFixSize {..})));
        assert!(serde_json::to_value(Findings::default()).unwrap().get("sources").is_none());
    }

    /// "none" is what the schema offers for "no label"; it reads as no label,
    /// as does "" or leaving the key out.
    #[test]
    fn none_or_blank_fix_size_and_category_parse_as_no_label() {
        for v in ["none", " NONE ", ""] {
            let doc = parse_review(&format!(
                r#"{{"verdict":"PASS","findings":{{"criteria":[
                {{"id":"AC1","verdict":"met","evidence":["E001"],"fix_size":"{v}","category":"{v}"}}],
                "extra_findings":[{{"severity":"minor","note":"x","fix_size":"{v}","category":"{v}"}}]}}}}"#
            ))
            .unwrap_or_else(|e| panic!("{v:?}: {e}"));
            assert_eq!(doc.findings.criteria[0].fix_size, None, "{v:?}");
            assert_eq!(doc.findings.criteria[0].category, None, "{v:?}");
            assert_eq!(doc.findings.extra_findings[0].fix_size, None, "{v:?}");
            assert_eq!(doc.findings.extra_findings[0].category, None, "{v:?}");
        }
        let other = r#"{"verdict":"REJECT","findings":{"criteria":[
            {"id":"AC1","verdict":"unmet","note":"n","fix_size":"XL","category":"other"}]}}"#;
        assert!(matches!(parse_review(other), Err(ReviewError::BadFixSize { .. })));
        let other = r#"{"verdict":"REJECT","findings":{"criteria":[
            {"id":"AC1","verdict":"unmet","note":"n","fix_size":"S","category":"other"}]}}"#;
        assert_eq!(parse_review(other).unwrap().findings.criteria[0].category.as_deref(), Some("other"));
    }

    /// The strict schema makes `note` required, so a codex reviewer's met
    /// criterion carries `"note": ""`; that is as good as no note.
    #[test]
    fn a_document_shaped_by_the_strict_schema_parses() {
        let doc = r#"{"verdict":"PASS","findings":{"criteria":[
            {"id":"AC1","verdict":"met","evidence":["E001"],"note":""}],"extra_findings":[]}}"#;
        assert!(parse_review(doc).is_ok());
        // and an empty note is still no excuse for an unsupported unmet
        let bare = r#"{"verdict":"REJECT","findings":{"criteria":[
            {"id":"AC1","verdict":"unmet","evidence":[],"note":""}],"extra_findings":[]}}"#;
        assert!(matches!(parse_review(bare), Err(ReviewError::UnsupportedUnmet(_))));
    }

    #[test]
    fn verdicts_round_trip_and_map_onto_task_statuses() {
        for v in [Verdict::Pass, Verdict::Reject, Verdict::Unverified] {
            assert_eq!(Verdict::parse(v.as_str()), Some(v));
        }
        assert_eq!(Verdict::parse(" PASS "), Some(Verdict::Pass));
        assert_eq!(Verdict::parse("pass"), None, "the vocabulary is uppercase");
        assert_eq!(Verdict::parse("MAYBE"), None);
        // v6 §6.1's state machine
        assert_eq!(Verdict::Pass.task_status(), crate::model::TaskStatus::Pass);
        assert_eq!(Verdict::Reject.task_status(), crate::model::TaskStatus::Reject);
        assert_eq!(
            Verdict::Unverified.task_status(),
            crate::model::TaskStatus::Unverified
        );
    }

    #[test]
    fn a_well_formed_review_parses_with_every_field() {
        let doc = parse_review(PASS).unwrap();
        assert_eq!(doc.verdict, "PASS");
        assert_eq!(doc.findings.criteria.len(), 2);
        assert_eq!(doc.findings.criteria[0].id, "AC1");
        assert_eq!(doc.findings.criteria[0].verdict, "met");
        assert_eq!(doc.findings.criteria[0].evidence, vec!["E001", "E004"]);
        assert_eq!(doc.findings.criteria[0].note, None);
        assert_eq!(
            doc.findings.criteria[1].note.as_deref(),
            Some("the diff covers it")
        );
        assert_eq!(doc.findings.extra_findings.len(), 1);
        assert_eq!(doc.findings.extra_findings[0].severity, "minor");
    }

    #[test]
    fn a_bad_verdict_or_criterion_verdict_is_rejected() {
        let bad = r#"{"verdict":"MAYBE","findings":{"criteria":[]}}"#;
        match parse_review(bad) {
            Err(ReviewError::BadVerdict(v)) => assert_eq!(v, "MAYBE"),
            other => panic!("expected BadVerdict, got {other:?}"),
        }
        let bad = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"sort of","evidence":["E001"]}]}}"#;
        match parse_review(bad) {
            Err(ReviewError::BadCriterionVerdict { id, verdict }) => {
                assert_eq!(id, "AC1");
                assert_eq!(verdict, "sort of");
            }
            other => panic!("expected BadCriterionVerdict, got {other:?}"),
        }
        assert!(matches!(parse_review("{not json"), Err(ReviewError::Json(_))));
        // a missing verdict is an empty string, which is not in the vocabulary
        assert!(matches!(
            parse_review(r#"{"findings":{"criteria":[]}}"#),
            Err(ReviewError::BadVerdict(_))
        ));
    }

    #[test]
    fn an_unmet_criterion_must_carry_evidence_or_a_note() {
        // v6 §4: a REJECT has to be actionable, and brief §9 folds exactly these
        // notes back into the next attempt's spec.
        let bare = r#"{"verdict":"REJECT","findings":{"criteria":[{"id":"AC1","verdict":"unmet"}]}}"#;
        match parse_review(bare) {
            Err(ReviewError::UnsupportedUnmet(id)) => assert_eq!(id, "AC1"),
            other => panic!("expected UnsupportedUnmet, got {other:?}"),
        }
        let with_note = r#"{"verdict":"REJECT","findings":{"criteria":[{"id":"AC1","verdict":"unmet","note":"the retry never fires"}]}}"#;
        assert!(parse_review(with_note).is_ok());
        let with_evidence = r#"{"verdict":"REJECT","findings":{"criteria":[{"id":"AC1","verdict":"unmet","evidence":["E002"]}]}}"#;
        assert!(parse_review(with_evidence).is_ok());
        // Whole-branch review Important 2 (edit): a `met` criterion used to
        // need neither. It now needs a citation - see
        // `a_met_criterion_must_cite_evidence` below - so this case moved
        // there and what is left here is the `unmet` rule alone.
        let met_uncited = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met"}]}}"#;
        assert!(
            parse_review(met_uncited).is_err(),
            "a met criterion now needs a citation too"
        );
    }

    /// Whole-branch review Important 2: the reviewer prompt's own rule 2
    /// ("cite at least one evidence code for each judgement") is now enforced
    /// for `met`, so an all-met, zero-citation PASS cannot settle a task
    /// `pass` from a reviewer that opened nothing.
    #[test]
    fn a_met_criterion_must_cite_evidence() {
        let uncited = r#"{"verdict":"PASS","findings":{"criteria":[
            {"id":"AC1","verdict":"met","evidence":["E001"]},
            {"id":"AC2","verdict":"met","evidence":[]}]}}"#;
        match parse_review(uncited) {
            Err(ReviewError::UncitedMet(id)) => assert_eq!(id, "AC2"),
            other => panic!("expected UncitedMet, got {other:?}"),
        }
        assert_eq!(
            ReviewError::UncitedMet("AC2".into()).to_string(),
            "review.json criterion 'AC2' is met but cites no evidence"
        );
        // A note is not a substitute: the absence of evidence can be
        // described, its presence has to be pointed at.
        let noted = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","note":"looked fine"}]}}"#;
        assert!(matches!(
            parse_review(noted),
            Err(ReviewError::UncitedMet(_))
        ));
        let cited = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":["E004"]}]}}"#;
        assert!(parse_review(cited).is_ok());
    }

    /// A blank citation is not a citation: `[""]` and `[" "]` are rejected like
    /// `[]`, and a blank entry next to a real code is never looked up.
    #[test]
    fn a_blank_citation_does_not_count_as_evidence() {
        for blank in [r#"[""]"#, r#"[" "]"#, r#"["", "  "]"#] {
            let doc = format!(
                r#"{{"verdict":"PASS","findings":{{"criteria":[{{"id":"AC1","verdict":"met","evidence":{blank}}}]}}}}"#
            );
            match parse_review(&doc) {
                Err(ReviewError::UncitedMet(id)) => assert_eq!(id, "AC1"),
                other => panic!("expected UncitedMet for {blank}, got {other:?}"),
            }
        }
        let mixed = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":["", "E001"]}]}}"#;
        let doc = parse_review(mixed).unwrap();
        assert_eq!(cited_codes(&doc), vec!["E001"]);
    }

    /// Fix round 1 (I2): `pass` is terminal and trusted, so a PASS that admits
    /// a criterion is unmet must not become one. This is the reviewer prompt's
    /// own rule 5, which nothing downstream was re-checking.
    #[test]
    fn a_pass_that_marks_a_criterion_unmet_is_rejected() {
        let contradictory = r#"{"verdict":"PASS","findings":{"criteria":[
            {"id":"AC1","verdict":"met","evidence":["E001"]},
            {"id":"AC2","verdict":"unmet","note":"the retry never fires"}]}}"#;
        match parse_review(contradictory) {
            Err(ReviewError::PassWithUnmetCriterion { id }) => assert_eq!(id, "AC2"),
            other => panic!("expected PassWithUnmetCriterion, got {other:?}"),
        }
        // `review_invalid` puts this text in the event's `reason`, so it has to
        // name both halves of the contradiction on its own.
        let msg = parse_review(contradictory).unwrap_err().to_string();
        assert!(msg.contains("PASS"), "got {msg}");
        assert!(msg.contains("AC2"), "got {msg}");

        // an unmet criterion is exactly what REJECT and UNVERIFIED are for
        let rejecting = r#"{"verdict":"REJECT","findings":{"criteria":[
            {"id":"AC1","verdict":"unmet","note":"the retry never fires"}]}}"#;
        assert!(parse_review(rejecting).is_ok());
        let unverified = r#"{"verdict":"UNVERIFIED","findings":{"criteria":[
            {"id":"AC1","verdict":"unmet","evidence":["E002"]}]}}"#;
        assert!(parse_review(unverified).is_ok());
        // and a PASS whose criteria are all met is still fine
        assert!(parse_review(PASS).is_ok());
    }

    #[test]
    fn ac_ids_come_from_the_criterion_text_with_a_positional_fallback() {
        // Open question 13
        assert_eq!(
            ac_ids(&spec_with(&["AC1: retries three times", "AC2: logs each attempt"])),
            vec!["AC1".to_string(), "AC2".to_string()]
        );
        // no colon at all, or a prefix with whitespace, falls back to position
        assert_eq!(
            ac_ids(&spec_with(&["prints hello", "exits zero"])),
            vec!["AC1".to_string(), "AC2".to_string()]
        );
        assert_eq!(
            ac_ids(&spec_with(&["the uploader: retries"])),
            vec!["AC1".to_string()]
        );
        // a bare id is already an id
        assert_eq!(ac_ids(&spec_with(&["AC1"])), vec!["AC1".to_string()]);
        // an implausibly long prefix falls back too
        let long = format!("{}: something", "X".repeat(20));
        assert_eq!(ac_ids(&spec_with(&[&long])), vec!["AC1".to_string()]);
    }

    #[test]
    fn coverage_requires_every_criterion_and_forbids_invented_ones() {
        let doc = parse_review(PASS).unwrap();
        let two = vec!["AC1".to_string(), "AC2".to_string()];
        assert!(check_coverage(&doc, &two).is_ok());

        let three = vec!["AC1".to_string(), "AC2".to_string(), "AC3".to_string()];
        match check_coverage(&doc, &three) {
            Err(ReviewError::MissingCriterion(id)) => assert_eq!(id, "AC3"),
            other => panic!("expected MissingCriterion, got {other:?}"),
        }

        let one = vec!["AC1".to_string()];
        match check_coverage(&doc, &one) {
            Err(ReviewError::UnknownCriterion(id)) => assert_eq!(id, "AC2"),
            other => panic!("expected UnknownCriterion, got {other:?}"),
        }
    }

    /// Whole-branch review Minor 4: "at least once" let one criterion be
    /// judged twice, with opposite answers, and still pass coverage.
    #[test]
    fn a_criterion_judged_twice_is_rejected() {
        let twice = r#"{"verdict":"REJECT","findings":{"criteria":[
            {"id":"AC1","verdict":"met","evidence":["E001"]},
            {"id":"AC1","verdict":"unmet","note":"actually not"}]}}"#;
        let doc = parse_review(twice).expect("each entry is individually well-formed");
        match check_coverage(&doc, &["AC1".to_string()]) {
            Err(ReviewError::DuplicateCriterion(id)) => assert_eq!(id, "AC1"),
            other => panic!("expected DuplicateCriterion, got {other:?}"),
        }
        assert_eq!(
            ReviewError::DuplicateCriterion("AC1".into()).to_string(),
            "review.json judges criterion 'AC1' more than once"
        );
    }

    #[test]
    fn cited_codes_collects_from_criteria_and_extras_without_duplicates() {
        let doc = parse_review(PASS).unwrap();
        assert_eq!(
            cited_codes(&doc),
            vec!["E001".to_string(), "E004".to_string()],
            "first-seen order, de-duplicated across criteria and extra findings"
        );

        let lower = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":[" e007 ",""]}]}}"#;
        let doc = parse_review(lower).unwrap();
        assert_eq!(cited_codes(&doc), vec!["E007".to_string()]);

        // Whole-branch review Important 2 (edit): this used to use an
        // all-`met`, zero-citation document, which `parse_review` now
        // rejects. A REJECT whose single `unmet` criterion carries a note
        // instead of a citation is the remaining shape of "a valid document
        // that cites nothing at all", which is what this case is about.
        let none = r#"{"verdict":"REJECT","findings":{"criteria":[{"id":"AC1","verdict":"unmet","note":"never runs"}]}}"#;
        assert!(cited_codes(&parse_review(none).unwrap()).is_empty());
    }
}
