//! BRIEF-1: the short conclusion a readout leads with. The full delivery or
//! judgement stays where it was (the `result_ingested` event, the review row,
//! the `spec_review` event); a brief is derived from it whenever it is shown.
//!
//! Every brief is masked before it is cut (RD-1): the whole source text, and
//! each note or location before its own preview, go through the masker
//! first, so a secret that a cut would leave unrecognisable is a marker by
//! then. Lengths count Unicode scalar values, never bytes.

use crate::redact::Masker;
use crate::review_json::{CriterionFinding, Findings};
use serde_json::Value;

/// The longest brief, in characters.
pub const BRIEF_CHARS: usize = 1000;
/// The longest note or location inside a judgement brief, in characters.
pub const NOTE_CHARS: usize = 200;

/// `brief_source` values.
pub const SOURCE_WORKER: &str = "worker";
pub const SOURCE_SUMMARY: &str = "summary";
pub const SOURCE_REVIEW: &str = "review";
pub const SOURCE_SPEC_REVIEW: &str = "spec_review";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brief {
    pub text: String,
    pub source: &'static str,
    pub truncated: bool,
}

/// `text` cut to `max` characters with trailing whitespace trimmed after the
/// cut, and whether it was cut.
fn cut(text: &str, max: usize) -> (String, bool) {
    match text.char_indices().nth(max) {
        Some((at, _)) => (text[..at].trim_end().to_string(), true),
        None => (text.to_string(), false),
    }
}

/// `text` masked whole, then cut to `max` characters.
pub fn shorten(masker: &Masker, text: &str, max: usize) -> (String, bool) {
    cut(&masker.mask_text(text).0, max)
}

impl Brief {
    fn from_text(masker: &Masker, text: &str, source: &'static str) -> Brief {
        let (text, truncated) = shorten(masker, text, BRIEF_CHARS);
        Brief { text, source, truncated }
    }
}

/// The three keys of an optional brief, all `null` without one.
pub fn keys(brief: Option<&Brief>) -> [(&'static str, Value); 3] {
    [
        ("brief", brief.map(|b| Value::from(b.text.clone())).unwrap_or(Value::Null)),
        ("brief_source", brief.map(|b| Value::from(b.source)).unwrap_or(Value::Null)),
        ("brief_truncated", brief.map(|b| Value::from(b.truncated)).unwrap_or(Value::Null)),
    ]
}

/// Adds `keys(brief)` to the object `v`.
pub fn insert_keys(v: &mut Value, brief: Option<&Brief>) {
    if let Some(o) = v.as_object_mut() {
        for (k, val) in keys(brief) {
            o.insert(k.to_string(), val);
        }
    }
}

/// A worker delivery's brief: its own `brief` when that is a string that is
/// not blank, otherwise its summary; trimmed either way. A `brief` that is
/// not a string counts as absent.
pub fn for_delivery(masker: &Masker, result_json: &Value, summary: &str) -> Brief {
    match result_json.get("brief").and_then(Value::as_str).map(str::trim) {
        Some(b) if !b.is_empty() => Brief::from_text(masker, b, SOURCE_WORKER),
        _ => Brief::from_text(masker, summary.trim(), SOURCE_SUMMARY),
    }
}

/// The brief of one `result_ingested` event's detail, from the document it
/// kept; the same rule whether or not the event carries the brief keys.
pub fn for_result_event(masker: &Masker, detail: &Value) -> Brief {
    let doc = &detail["result_json"];
    let summary = detail["summary"]
        .as_str()
        .or_else(|| doc["summary"].as_str())
        .unwrap_or("");
    for_delivery(masker, doc, summary)
}

/// A note or location as it appears inside a judgement brief: trimmed,
/// masked whole, then cut to `NOTE_CHARS`; `(no note)` when blank.
fn preview(masker: &Masker, text: Option<&str>) -> String {
    match text.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => shorten(masker, t, NOTE_CHARS).0,
        None => "(no note)".to_string(),
    }
}

/// ` (fix S, category)` with only the labels that are there; empty for none.
fn labels(fix_size: Option<&str>, category: Option<&str>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(s) = fix_size.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(format!("fix {s}"));
    }
    if let Some(c) = category.map(str::trim).filter(|c| !c.is_empty()) {
        parts.push(c.to_string());
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

fn is_unmet(c: &CriterionFinding) -> bool {
    c.verdict.trim() == "unmet"
}

/// A criterion-based review's brief: `VERDICT: m/t criteria met`, each unmet
/// criterion and each extra finding. `None` for a research source check,
/// whose findings are sources, not criteria.
pub fn for_review(masker: &Masker, verdict: &str, findings: &Findings) -> Option<Brief> {
    if !findings.sources.is_empty() {
        return None;
    }
    let total = findings.criteria.len();
    let met = findings.criteria.iter().filter(|c| c.verdict.trim() == "met").count();
    let mut s = format!("{}: {met}/{total} criteria met", verdict.trim());
    for c in findings.criteria.iter().filter(|c| is_unmet(c)) {
        s.push_str(&format!(
            "; {} unmet{}: {}",
            c.id.trim(),
            labels(c.fix_size.as_deref(), c.category.as_deref()),
            preview(masker, c.note.as_deref())
        ));
    }
    if !findings.extra_findings.is_empty() {
        s.push_str(&format!("; {} extra finding(s)", findings.extra_findings.len()));
        for e in &findings.extra_findings {
            s.push_str(&format!(
                "; {}{}: {}",
                e.severity.trim(),
                labels(e.fix_size.as_deref(), e.category.as_deref()),
                preview(masker, Some(&e.note))
            ));
        }
    }
    Some(Brief::from_text(masker, &s, SOURCE_REVIEW))
}

/// The brief of a stored review row: its verdict and findings JSON.
pub fn for_review_row(masker: &Masker, verdict: &str, findings_json: &str) -> Option<Brief> {
    let findings: Findings = serde_json::from_str(findings_json).ok()?;
    for_review(masker, verdict, &findings)
}

/// A spec review's brief. An author's check (`mode` confirm) counts its
/// revise answers; any other review counts the criteria that need revision,
/// lists each of their findings and the edits it proposed. `None` when the
/// document cannot be read.
pub fn for_spec_review(
    masker: &Masker,
    verdict: &str,
    mode: &str,
    document: &Value,
    edit_count: usize,
) -> Option<Brief> {
    let verdict = verdict.trim();
    if mode == crate::spec_edits::ReviewMode::Confirm.as_str() {
        let doc: crate::spec_edits::ConfirmDocument = serde_json::from_value(document.clone()).ok()?;
        let revise = doc.edits.iter().filter(|e| e.verdict.trim() == "revise").count();
        let s = format!("{verdict}: {revise} of {} edits revise", doc.edits.len());
        return Some(Brief::from_text(masker, &s, SOURCE_SPEC_REVIEW));
    }
    let doc: crate::spec_review_json::SpecReviewDocument = serde_json::from_value(document.clone()).ok()?;
    let revise: Vec<_> = doc.criteria.iter().filter(|c| c.verdict.trim() == "revise").collect();
    let mut s = format!(
        "{verdict}: {}/{} criteria need revision",
        revise.len(),
        doc.criteria.len()
    );
    for c in &revise {
        for f in &c.findings {
            s.push_str(&format!(
                "; {} ({}, {}): {}",
                c.id.trim(),
                f.check.trim(),
                preview(masker, Some(&f.location)),
                preview(masker, Some(&f.note))
            ));
        }
    }
    if edit_count > 0 {
        s.push_str(&format!("; {edit_count} edit(s) proposed"));
    }
    Some(Brief::from_text(masker, &s, SOURCE_SPEC_REVIEW))
}

/// The human `brief:` lines: the first line after `brief: `, continuation
/// lines indented two spaces, and when it was cut, where the full text is.
pub fn human_lines(brief: &Brief, full_text: &str) -> String {
    let mut out = String::from("brief:");
    let mut lines = brief.text.lines();
    if let Some(first) = lines.next() {
        out.push(' ');
        out.push_str(first);
    }
    for line in lines {
        out.push_str("\n  ");
        out.push_str(line);
    }
    if brief.truncated {
        out.push_str(&format!("\n  (cut at {BRIEF_CHARS} chars; full text: {full_text})"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn none() -> Masker {
        Masker::from_pairs(&[])
    }

    #[test]
    fn cut_counts_characters_and_trims_only_after_a_cut() {
        assert_eq!(cut("abc ", 10), ("abc ".to_string(), false));
        assert_eq!(cut(&"中".repeat(1001), 1000), ("中".repeat(1000), true));
        assert_eq!(cut(&"😀".repeat(1001), 1000), ("😀".repeat(1000), true));
        assert_eq!(cut(&format!("{} b", "a".repeat(999)), 1000), ("a".repeat(999), true));
        assert_eq!(cut(&"a".repeat(1000), 1000), ("a".repeat(1000), false));
    }

    #[test]
    fn a_delivery_brief_falls_back_to_the_summary() {
        let m = none();
        let b = for_delivery(&m, &json!({"brief": "  short \n two "}), "long");
        assert_eq!((b.text.as_str(), b.source, b.truncated), ("short \n two", SOURCE_WORKER, false));
        for doc in [json!({}), json!({"brief": "   "}), json!({"brief": 7}), json!({"brief": null}),
            json!({"brief": ["x"]}), json!({"brief": {"a": 1}})] {
            let b = for_delivery(&m, &doc, " the summary ");
            assert_eq!((b.text.as_str(), b.source), ("the summary", SOURCE_SUMMARY), "{doc}");
        }
    }

    #[test]
    fn a_secret_across_the_cut_is_masked_before_it() {
        let token = format!("{}{}", ["gh", "p_"].concat(), "dummyDUMMY0123456789dummyDUMMY012345");
        let text = format!("{} {token}", "a".repeat(990));
        let (out, cut_) = shorten(&none(), &text, BRIEF_CHARS);
        assert!(cut_);
        assert_eq!(out.chars().count(), 1000);
        // 991 characters before the marker leave room for 9 of its own
        assert!(out.ends_with(" [redacted"), "{out}");
        assert!(!out.contains("dummyDUMMY"));
    }

    #[test]
    fn a_review_brief_lists_unmet_criteria_and_extra_findings() {
        let findings: Findings = serde_json::from_value(json!({
            "criteria": [
                {"id": "AC1", "verdict": "met", "evidence": ["E001"]},
                {"id": "AC2", "verdict": "unmet", "note": " retry never fires ", "fix_size": "S", "category": "logic"},
                {"id": "AC3", "verdict": "unmet", "evidence": ["E002"]}
            ],
            "extra_findings": [{"severity": "minor", "note": "n", "category": "docs"}]
        }))
        .unwrap();
        let b = for_review(&none(), "REJECT", &findings).unwrap();
        assert_eq!(
            b.text,
            "REJECT: 1/3 criteria met; AC2 unmet (fix S, logic): retry never fires; AC3 unmet: (no note); \
             1 extra finding(s); minor (docs): n"
        );
        assert_eq!(b.source, SOURCE_REVIEW);
        let sources: Findings = serde_json::from_value(json!({
            "sources": [{"id": "S1", "url": "https://a.example", "verdict": "supported", "reason": ""}]
        }))
        .unwrap();
        assert!(for_review(&none(), "PASS", &sources).is_none());
    }

    #[test]
    fn spec_review_briefs_count_revisions_and_answers() {
        let doc = json!({"verdict": "REVISE", "criteria": [
            {"id": "AC1", "verdict": "revise", "findings": [{"check": "exists", "location": "src/a.rs:1", "note": "gone"}]},
            {"id": "AC2", "verdict": "ok", "findings": []}
        ], "edits": []});
        let b = for_spec_review(&none(), "REVISE", "edit", &doc, 2).unwrap();
        assert_eq!(b.text, "REVISE: 1/2 criteria need revision; AC1 (exists, src/a.rs:1): gone; 2 edit(s) proposed");
        let confirm = json!({"verdict": "REVISE", "edits": [
            {"index": 0, "verdict": "revise", "note": "x"}, {"index": 1, "verdict": "ok", "note": ""}
        ]});
        let b = for_spec_review(&none(), "REVISE", "confirm", &confirm, 2).unwrap();
        assert_eq!(b.text, "REVISE: 1 of 2 edits revise");
        assert!(for_spec_review(&none(), "APPROVE", "review", &Value::Null, 0).is_none());
    }

    #[test]
    fn human_lines_indent_continuations_and_name_the_full_text() {
        let b = Brief { text: "one\ntwo".into(), source: SOURCE_WORKER, truncated: true };
        assert_eq!(
            human_lines(&b, "thoth task result T"),
            "brief: one\n  two\n  (cut at 1000 chars; full text: thoth task result T)"
        );
    }
}
