//! SR-3: the structured edits a spec reviewer proposes, and the author's
//! check of them.
//!
//! In edit mode (`spec review start --edit`) the reviewer's document carries
//! an `edits` array besides `verdict` and `criteria`. Thoth applies the edits
//! here, in order, to the stored spec and keeps the result (the "proposed
//! spec") in the `spec_review` event; the reviewer itself never writes a
//! file. `spec review confirm` then launches the author, who answers `ok` or
//! `revise` per edit; an APPROVE makes Thoth replace the task's spec with the
//! proposed one. SR-4: an edit that breaks a rule is refused alone and the
//! others still apply; the author answers only the applied ones. Everything
//! in this module is a pure function over strings and JSON, so the rules are
//! testable without a database.

use crate::spec::TaskSpec;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The fields an edit may change; anything else (`risk`, `checks`, the
/// labels, unknown keys) is refused.
pub const EDIT_FIELDS: [&str; 5] = [
    "goal",
    "acceptance_criteria",
    "constraints",
    "non_goals",
    "excluded_alternatives",
];

pub const EDIT_OPS: [&str; 3] = ["replace", "insert", "delete"];

/// The fixed key order of a serialized proposed spec; every other key follows
/// in alphabetical order.
pub const KEY_ORDER: [&str; 9] = [
    "goal",
    "acceptance_criteria",
    "constraints",
    "non_goals",
    "excluded_alternatives",
    "task_type",
    "difficulty",
    "checks",
    "risk",
];

/// How a spec reviewer run was started, recorded in
/// `spec_review_started.detail.mode` and copied into the `spec_review` event
/// it writes (`spec review record` writes `external`, which is not a run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewMode {
    /// Judge only (SR-2).
    Review,
    /// Judge and propose edits.
    Edit,
    /// The author checks proposed edits.
    Confirm,
}

impl ReviewMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ReviewMode::Review => "review",
            ReviewMode::Edit => "edit",
            ReviewMode::Confirm => "confirm",
        }
    }

    pub fn parse(s: &str) -> Option<ReviewMode> {
        match s {
            "review" => Some(ReviewMode::Review),
            "edit" => Some(ReviewMode::Edit),
            "confirm" => Some(ReviewMode::Confirm),
            _ => None,
        }
    }
}

/// One edit as the reviewer wrote it. The shape is the strict one
/// `edit_output_schema` describes: every property present (`""` or `0`
/// when it does not apply) and no other, so a property left out is a
/// refusal, not a default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct SpecEdit {
    pub field: String,
    pub op: String,
    pub at: usize,
    pub anchor: String,
    pub text: String,
    pub criterion: String,
    pub reason: String,
}

/// One edit as Thoth records it after applying it: what was there before and
/// what is there now.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedEdit {
    pub index: usize,
    pub field: String,
    pub op: String,
    pub at: usize,
    pub criterion: String,
    pub anchor: String,
    /// `None` for an insert.
    pub before: Option<String>,
    /// `None` for a delete.
    pub after: Option<String>,
    pub reason: String,
}

/// SR-4: one edit Thoth did not apply, and why. The edits after it were
/// still applied; this one changed nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefusedEdit {
    /// The edit's position in the reviewer's `edits`, from 0.
    pub index: usize,
    pub field: String,
    pub reason: String,
}

/// The outcome of applying every edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// The edits applied, in document order, each with its original index.
    pub edits: Vec<AppliedEdit>,
    /// SR-4: the edits refused, in document order.
    pub refused: Vec<RefusedEdit>,
    /// The proposed spec, serialized by `serialize_spec`; `None` when every
    /// edit was refused.
    pub proposed: Option<String>,
}

/// What refuses the whole review rather than one edit: the stored spec is
/// not a JSON object, or the proposed spec is not a spec.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    #[error("the proposed spec is not a spec: {0}")]
    Proposed(String),
}

/// SR-4: the entry a `replace` or `delete` names. Both the anchor and the
/// entries are trimmed at both ends and compared literally (no
/// normalization, no case or whitespace folding); a blank anchor names
/// nothing. A prefix of the entry at `at`, of any length, names that entry
/// even when it occurs elsewhere too. Otherwise the anchor has to occur at
/// exactly one starting position across all `entries` (overlapping
/// occurrences count separately), and names the entry it occurs in whatever
/// `at` says. Never guesses between two places. The reason quotes the
/// anchor whole and no entry text, so no cut of an entry can carry part of a
/// secret past the masker.
fn resolve_anchor(anchor: &str, entries: &[String], at: usize, field: &str) -> Result<usize, String> {
    let a = anchor.trim();
    if a.is_empty() {
        return Err(
            "anchor is blank; write the start of the entry at that position, or a piece of text that occurs once in the field".to_string(),
        );
    }
    if entries.get(at).is_some_and(|e| e.trim().starts_with(a)) {
        return Ok(at);
    }
    let mut found: Vec<usize> = Vec::new();
    for (i, e) in entries.iter().enumerate() {
        let e = e.trim();
        for (pos, _) in e.char_indices() {
            if e[pos..].starts_with(a) {
                found.push(i);
            }
        }
    }
    let at_says = if at < entries.len() {
        format!("does not start {field} entry {at}")
    } else {
        format!("at {at} is past the end of {field} ({} entries)", entries.len())
    };
    match found.as_slice() {
        [one] => Ok(*one),
        [] => Err(format!("anchor '{a}' {at_says} and occurs nowhere in {field}")),
        many => {
            let mut where_: Vec<String> = many.iter().map(|i| i.to_string()).collect();
            where_.dedup();
            Err(format!(
                "anchor '{a}' {at_says} and occurs {} times in {field} (entries {}), so it does not say which entry; write the start of the entry at at, or more text",
                many.len(),
                where_.join(", ")
            ))
        }
    }
}

/// The ids `review_json::ac_ids` gives a list of criteria.
fn ids_of(acs: &[String]) -> Vec<String> {
    crate::review_json::ac_ids(&TaskSpec {
        goal: String::new(),
        acceptance_criteria: acs.to_vec(),
        constraints: Vec::new(),
        non_goals: Vec::new(),
        excluded_alternatives: Vec::new(),
        risk: "normal".to_string(),
        checks: Vec::new(),
    })
}

/// Applies `edits` in order to the spec `spec` (its stored bytes). Every
/// `at` refers to the list as the earlier applied edits left it. `ac_ids` are
/// the task's criterion ids (`review_json::ac_ids` of the spec), which every
/// edit binds to and which the proposed spec must keep exactly. SR-4: an
/// edit that breaks a rule is refused alone (it changes nothing) and the
/// later edits still apply; only a stored spec that is not an object, or a
/// proposed spec that is not a spec, is an `Err`.
pub fn apply_edits(spec: &str, edits: &[SpecEdit], ac_ids: &[String]) -> Result<Applied, EditError> {
    let mut root: Map<String, Value> = match serde_json::from_str::<Value>(spec) {
        Ok(Value::Object(m)) => m,
        _ => return Err(EditError::Proposed("the stored spec is not a JSON object".to_string())),
    };
    // RD-1: every string value of the stored spec, before any edit. A mask
    // marker in an edit's text that none of them carries was copied from the
    // masked spec the reviewer read.
    fn strings_of(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) => out.push(s.clone()),
            Value::Array(items) => items.iter().for_each(|i| strings_of(i, out)),
            Value::Object(map) => map.values().for_each(|i| strings_of(i, out)),
            Value::Null | Value::Bool(_) | Value::Number(_) => {}
        }
    }
    let mut stored_strings = Vec::new();
    root.values().for_each(|v| strings_of(v, &mut stored_strings));
    let mut applied = Vec::with_capacity(edits.len());
    let mut refused = Vec::new();
    for (index, e) in edits.iter().enumerate() {
        match apply_one(&mut root, &stored_strings, ac_ids, index, e) {
            Ok(a) => applied.push(a),
            Err(reason) => refused.push(RefusedEdit {
                index,
                field: e.field.clone(),
                reason,
            }),
        }
    }
    if applied.is_empty() {
        return Ok(Applied {
            edits: applied,
            refused,
            proposed: None,
        });
    }
    let proposed = serialize_spec(&root);
    let parsed = crate::spec::parse_spec(&proposed).map_err(|e| EditError::Proposed(e.to_string()))?;
    let now = crate::review_json::ac_ids(&parsed);
    if now != ac_ids {
        return Err(EditError::Proposed(format!(
            "the criterion ids changed from {} to {}",
            ac_ids.join(", "),
            now.join(", ")
        )));
    }
    Ok(Applied {
        edits: applied,
        refused,
        proposed: Some(proposed),
    })
}

/// One edit against the proposal the earlier applied edits left. `root` is
/// changed only when the edit is applied; `Err` is why it was refused.
fn apply_one(
    root: &mut Map<String, Value>,
    stored_strings: &[String],
    ac_ids: &[String],
    index: usize,
    e: &SpecEdit,
) -> Result<AppliedEdit, String> {
    let field = e.field.as_str();
    if !EDIT_FIELDS.contains(&field) {
        return Err(format!("field must be one of {}", EDIT_FIELDS.join(", ")));
    }
    if !EDIT_OPS.contains(&e.op.as_str()) {
        return Err(format!("op '{}' must be one of {}", e.op, EDIT_OPS.join(", ")));
    }
    if !ac_ids.iter().any(|id| id == e.criterion.trim()) {
        return Err(format!(
            "criterion '{}' is not an acceptance criterion of this task (one of: {})",
            e.criterion,
            ac_ids.join(", ")
        ));
    }
    if e.reason.trim().is_empty() {
        return Err("reason must not be blank".to_string());
    }
    let text_required = e.op != "delete";
    if text_required && e.text.trim().is_empty() {
        return Err(format!("text must not be blank for op {}", e.op));
    }
    let new_marker = text_required
        && crate::redact::marker_spans(&e.text)
            .into_iter()
            .any(|r| !stored_strings.iter().any(|s| s.contains(&e.text[r.clone()])));
    if new_marker {
        return Err(
            "text carries a mask marker the stored spec does not have (the reviewer read a masked spec); write the original wording instead".to_string(),
        );
    }
    if (field == "goal" || field == "acceptance_criteria") && e.op != "replace" {
        return Err(format!("{field} allows only op replace, not {}", e.op));
    }
    let criterion = e.criterion.trim().to_string();
    if field == "goal" {
        // The goal is one string: the only entry there is to name.
        let goal = root.get("goal").and_then(Value::as_str).unwrap_or("").to_string();
        resolve_anchor(&e.anchor, std::slice::from_ref(&goal), 0, field)?;
        root.insert("goal".to_string(), Value::String(e.text.clone()));
        return Ok(AppliedEdit {
            index,
            field: field.to_string(),
            op: e.op.clone(),
            at: 0,
            criterion,
            anchor: e.anchor.clone(),
            before: Some(goal),
            after: Some(e.text.clone()),
            reason: e.reason.clone(),
        });
    }
    // A list field; one the spec does not have is an empty list.
    let mut list: Vec<String> = match root.get(field) {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item.as_str() {
                    Some(s) => out.push(s.to_string()),
                    None => return Err(format!("{field} holds an entry that is not a string, so it cannot be edited")),
                }
            }
            out
        }
        Some(_) => return Err(format!("{field} is not a list in the stored spec, so it cannot be edited")),
    };
    let len = list.len();
    let (at, before, after) = match e.op.as_str() {
        "replace" | "delete" => {
            let at = resolve_anchor(&e.anchor, &list, e.at, field)?;
            let before = list[at].clone();
            if e.op == "replace" {
                list[at] = e.text.clone();
                (at, Some(before), Some(e.text.clone()))
            } else {
                list.remove(at);
                (at, Some(before), None)
            }
        }
        _ => {
            if e.at > len {
                return Err(format!(
                    "at {} is past the end: {field} has {len} entries at this point, so insert allows 0 to {len}",
                    e.at
                ));
            }
            list.insert(e.at, e.text.clone());
            (e.at, None, Some(e.text.clone()))
        }
    };
    if field == "acceptance_criteria" {
        let now = ids_of(&list);
        if now != ac_ids {
            return Err(format!(
                "the criterion ids changed from {} to {}; a replaced acceptance criterion keeps the id before its colon",
                ac_ids.join(", "),
                now.join(", ")
            ));
        }
    }
    root.insert(field.to_string(), Value::Array(list.into_iter().map(Value::String).collect()));
    Ok(AppliedEdit {
        index,
        field: field.to_string(),
        op: e.op.clone(),
        at,
        criterion,
        anchor: e.anchor.clone(),
        before,
        after,
        reason: e.reason.clone(),
    })
}

/// The proposed spec's bytes: `KEY_ORDER` first, every other key after them
/// in alphabetical order, two-space indent, LF line ends, one trailing
/// newline. Unknown keys keep their values.
pub fn serialize_spec(root: &Map<String, Value>) -> String {
    let mut keys: Vec<&String> = KEY_ORDER
        .iter()
        .filter_map(|k| root.get_key_value(*k).map(|(key, _)| key))
        .collect();
    let mut rest: Vec<&String> = root.keys().filter(|k| !KEY_ORDER.contains(&k.as_str())).collect();
    rest.sort();
    keys.extend(rest);
    let mut out = String::from("{\n");
    let n = keys.len();
    for (i, key) in keys.into_iter().enumerate() {
        let value = serde_json::to_string_pretty(&root[key]).unwrap_or_else(|_| "null".to_string());
        let indented: Vec<String> = value
            .lines()
            .enumerate()
            .map(|(j, line)| if j == 0 { line.to_string() } else { format!("  {line}") })
            .collect();
        out.push_str("  ");
        out.push_str(&serde_json::to_string(key).unwrap_or_default());
        out.push_str(": ");
        out.push_str(&indented.join("\n"));
        if i + 1 < n {
            out.push(',');
        }
        out.push('\n');
    }
    out.push_str("}\n");
    out
}

/// The document an edit-mode reviewer hands in: today's verdict and
/// criteria, plus the edits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditDocument {
    pub review: crate::spec_review_json::SpecReviewDocument,
    pub edits: Vec<SpecEdit>,
}

/// Shape and self-consistency of an edit-mode document. The criteria rules
/// are `spec_review_json`'s, except that a REVISE may mark no criterion
/// `revise` when it proposes edits; an APPROVE proposes none. `edits` is
/// always there, an empty array when nothing is proposed, and every edit
/// has the strict shape of `SpecEdit`. Whether each edit is applicable is
/// `apply_edits`.
pub fn parse_edit_document(json: &str) -> Result<EditDocument, crate::spec_review_json::SpecReviewError> {
    use crate::spec_review_json::{check_document, SpecReviewDocument, SpecReviewError, SpecVerdict};
    let v: Value = serde_json::from_str(json)?;
    let review: SpecReviewDocument = serde_json::from_value(v.clone())?;
    let edits: Vec<SpecEdit> = match v.get("edits") {
        None | Some(Value::Null) => return Err(SpecReviewError::NoEdits),
        Some(e) => serde_json::from_value(e.clone()).map_err(|err| SpecReviewError::BadEdits(err.to_string()))?,
    };
    let verdict = check_document(&review, !edits.is_empty())?;
    if verdict == SpecVerdict::Approve && !edits.is_empty() {
        return Err(SpecReviewError::ApproveWithEdits(edits.len()));
    }
    Ok(EditDocument { review, edits })
}

/// The JSON Schema of an edit-mode report: `output_schema` plus `edits`.
/// Strict, as the API's structured output requires: every property
/// required and no extra keys; a property that does not apply holds `""`
/// or `0`.
pub fn edit_output_schema() -> Value {
    let mut schema = crate::spec_review_json::output_schema();
    schema["required"] = serde_json::json!(["verdict", "criteria", "edits"]);
    schema["properties"]["edits"] = serde_json::json!({"type": "array", "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["field", "op", "at", "anchor", "text", "criterion", "reason"],
        "properties": {
            "field": {"type": "string", "enum": EDIT_FIELDS},
            "op": {"type": "string", "enum": EDIT_OPS},
            "at": {"type": "integer"},
            "anchor": {"type": "string"},
            "text": {"type": "string"},
            "criterion": {"type": "string"},
            "reason": {"type": "string"},
        },
    }});
    schema
}

/// The author's answer to one edit. Strict like `confirm_output_schema`:
/// `index`, `verdict` and `note` are all there (`note` may be `""` on an
/// `ok`) and nothing else, so an answer without `index` is refused rather
/// than read as an answer to edit 0.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmEdit {
    pub index: usize,
    pub verdict: String,
    pub note: String,
}

/// The author's document (`spec review confirm`): `verdict` and `edits`,
/// nothing else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmDocument {
    pub verdict: String,
    pub edits: Vec<ConfirmEdit>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfirmError {
    #[error("spec-review.json is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("spec-review.json does not have the author's shape (verdict, edits of index, verdict, note): {0}")]
    Shape(String),
    #[error("spec-review.json verdict must be APPROVE or REVISE, got '{0}'")]
    BadVerdict(String),
    #[error("spec-review.json edit {index} verdict must be ok or revise, got '{verdict}'")]
    BadEditVerdict { index: usize, verdict: String },
    #[error("spec-review.json edit {0} is revise but has no note")]
    ReviseWithoutNote(usize),
    #[error("spec-review.json says APPROVE but marks edit {0} revise")]
    ApproveWithRevise(usize),
    #[error("spec-review.json says REVISE but marks no edit revise")]
    ReviseWithoutRevise,
    #[error("spec-review.json does not answer edit {0}")]
    MissingEdit(usize),
    #[error("spec-review.json answers edit {0}, which the edit review does not have")]
    UnknownEdit(usize),
    #[error("spec-review.json answers edit {0} more than once")]
    DuplicateEdit(usize),
}

/// Shape and self-consistency of the author's document. Coverage of the
/// edit review's edits is `check_confirm_coverage`.
pub fn parse_confirm_document(json: &str) -> Result<ConfirmDocument, ConfirmError> {
    use crate::spec_review_json::SpecVerdict;
    let v: Value = serde_json::from_str(json)?;
    let doc: ConfirmDocument = serde_json::from_value(v).map_err(|e| ConfirmError::Shape(e.to_string()))?;
    let Some(verdict) = SpecVerdict::parse(&doc.verdict) else {
        return Err(ConfirmError::BadVerdict(doc.verdict.clone()));
    };
    for e in &doc.edits {
        match e.verdict.trim() {
            "ok" => {}
            "revise" => {
                if e.note.trim().is_empty() {
                    return Err(ConfirmError::ReviseWithoutNote(e.index));
                }
            }
            other => {
                return Err(ConfirmError::BadEditVerdict {
                    index: e.index,
                    verdict: other.to_string(),
                })
            }
        }
    }
    let first_revise = doc.edits.iter().find(|e| e.verdict.trim() == "revise");
    match (verdict, first_revise) {
        (SpecVerdict::Approve, Some(e)) => Err(ConfirmError::ApproveWithRevise(e.index)),
        (SpecVerdict::Revise, None) => Err(ConfirmError::ReviseWithoutRevise),
        _ => Ok(doc),
    }
}

/// Every index `0..edit_count` exactly once, and nothing else.
pub fn check_confirm_coverage(doc: &ConfirmDocument, edit_count: usize) -> Result<(), ConfirmError> {
    check_confirm_indices(doc, &(0..edit_count).collect::<Vec<_>>())
}

/// SR-4: every index of `indices` exactly once, and nothing else. An edit
/// review's applied edits keep the reviewer's indices, so after a refused
/// edit the list has gaps (`[0, 2]`); a refused index is not one to answer.
pub fn check_confirm_indices(doc: &ConfirmDocument, indices: &[usize]) -> Result<(), ConfirmError> {
    for &index in indices {
        if !doc.edits.iter().any(|e| e.index == index) {
            return Err(ConfirmError::MissingEdit(index));
        }
    }
    for (i, e) in doc.edits.iter().enumerate() {
        if !indices.contains(&e.index) {
            return Err(ConfirmError::UnknownEdit(e.index));
        }
        if doc.edits[..i].iter().any(|earlier| earlier.index == e.index) {
            return Err(ConfirmError::DuplicateEdit(e.index));
        }
    }
    Ok(())
}

/// The JSON Schema of the author's answer. Strict: `note` is required and
/// may be `""` on an `ok`.
pub fn confirm_output_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["verdict", "edits"],
        "properties": {
            "verdict": {"type": "string", "enum": crate::spec_review_json::SPEC_REVIEW_VERDICTS},
            "edits": {"type": "array", "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["index", "verdict", "note"],
                "properties": {
                    "index": {"type": "integer"},
                    "verdict": {"type": "string", "enum": crate::spec_review_json::CRITERION_VERDICTS},
                    "note": {"type": "string"},
                },
            }},
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SPEC: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection","AC3: exits zero"],"constraints":["no new crate"],"non_goals":["rewriting the transport","changing the log format"],"excluded_alternatives":[],"risk":"important","rework":{"attempt":2},"alpha":1}"#;

    fn ids() -> Vec<String> {
        ["AC1", "AC2", "AC3"].iter().map(|s| s.to_string()).collect()
    }

    fn edit(field: &str, op: &str, at: usize, anchor: &str, text: &str) -> SpecEdit {
        SpecEdit {
            field: field.into(),
            op: op.into(),
            at,
            anchor: anchor.into(),
            text: text.into(),
            criterion: "AC1".into(),
            reason: "because".into(),
        }
    }

    /// SR-4: a single edit that is refused alone; (its index, why).
    fn refused_alone(spec: &str, e: SpecEdit) -> (usize, String) {
        let a = apply_edits(spec, &[e], &ids()).unwrap();
        assert!(a.edits.is_empty(), "{a:?}");
        assert_eq!(a.proposed, None, "nothing applied, nothing proposed");
        assert_eq!(a.refused.len(), 1, "{a:?}");
        (a.refused[0].index, a.refused[0].reason.clone())
    }

    fn proposed_of(a: &Applied) -> Value {
        serde_json::from_str(a.proposed.as_deref().expect("a proposed spec")).unwrap()
    }

    #[test]
    fn apply_edits_refuses_a_marker_the_spec_does_not_have() {
        let e = edit(
            "acceptance_criteria",
            "replace",
            0,
            "AC1: rejects",
            "AC1: rejects a double spend signed with [redacted:github-token]",
        );
        let (index, why) = refused_alone(SPEC, e);
        assert_eq!(index, 0);
        assert!(why.contains("carries a mask marker the stored spec does not have"), "{why}");

        // a marker the stored spec quotes itself is not new
        let quoting = SPEC.replace(r#""constraints":["no new crate"]"#, r#""constraints":["no new crate","write [redacted:KIND] for a secret"]"#);
        assert_ne!(quoting, SPEC);
        let e = edit("constraints", "insert", 0, "", "keep the [redacted:KIND] form");
        assert_eq!(apply_edits(&quoting, &[e], &ids()).unwrap().edits.len(), 1);

        // an opening without its closing is not a marker
        let e = edit("constraints", "insert", 0, "", "mention [redacted:github and stop");
        assert_eq!(apply_edits(SPEC, &[e], &ids()).unwrap().edits.len(), 1);
    }

    /// SR-4 (AC1, AC2): any prefix of the entry at `at` names it, however
    /// short and wherever else it occurs; otherwise one occurrence in the
    /// whole field names the entry it is in, whatever `at` says.
    #[test]
    fn a_prefix_at_at_or_a_unique_occurrence_names_the_entry() {
        // one character: "r" starts entry 0 and occurs in entry 1 too; at
        // picks entry 0
        let e = edit("non_goals", "delete", 0, "r", "");
        let a = apply_edits(SPEC, &[e], &ids()).unwrap();
        assert_eq!(a.edits[0].at, 0);
        assert_eq!(a.edits[0].before.as_deref(), Some("rewriting the transport"));
        // at picks entry 1 with the same kind of short prefix
        let e = edit("non_goals", "replace", 1, "c", "changing the log level");
        let a = apply_edits(SPEC, &[e], &ids()).unwrap();
        assert_eq!(a.edits[0].at, 1);
        assert_eq!(proposed_of(&a)["non_goals"], json!(["rewriting the transport", "changing the log level"]));
        // trimmed on both sides: a padded prefix is still a prefix
        let e = edit("non_goals", "replace", 0, "  rewriting  ", "rewriting the transport layer");
        assert_eq!(apply_edits(SPEC, &[e], &ids()).unwrap().edits[0].at, 0);
        // a unique middle substring with a stale at: the entry it is in
        let e = edit("non_goals", "delete", 0, "log format", "");
        let a = apply_edits(SPEC, &[e], &ids()).unwrap();
        assert_eq!(a.edits[0].at, 1);
        assert_eq!(a.edits[0].before.as_deref(), Some("changing the log format"));
        assert_eq!(proposed_of(&a)["non_goals"], json!(["rewriting the transport"]));
        // a unique substring with an at past the end
        let e = edit("acceptance_criteria", "replace", 9, "logs the", "AC2: logs the rejection once");
        let a = apply_edits(SPEC, &[e], &ids()).unwrap();
        assert_eq!(a.edits[0].at, 1);
        assert_eq!(a.edits[0].before.as_deref(), Some("AC2: logs the rejection"));
        // the goal: its one string, prefix or unique substring, at ignored
        for anchor in ["g", "ledger"] {
            let e = edit("goal", "replace", 3, anchor, "guard the ledger well");
            let a = apply_edits(SPEC, &[e], &ids()).unwrap();
            assert_eq!(a.edits[0].at, 0, "{anchor}");
            assert_eq!(proposed_of(&a)["goal"], "guard the ledger well");
        }
        // case-sensitive, literal
        let (_, why) = refused_alone(SPEC, edit("non_goals", "delete", 0, "Rewriting", ""));
        assert!(why.contains("occurs nowhere in non_goals"), "{why}");
    }

    #[test]
    fn an_ambiguous_overlapping_missing_or_blank_anchor_is_refused() {
        // "the" starts neither entry at at=0 and occurs in both non_goals
        let (index, why) = refused_alone(SPEC, edit("non_goals", "delete", 0, "the", ""));
        assert_eq!(index, 0);
        assert!(why.contains("anchor 'the' does not start non_goals entry 0"), "{why}");
        assert!(why.contains("occurs 2 times in non_goals (entries 0, 1)"), "{why}");
        // overlapping occurrences count one by one: "aa" is twice in "aaa"
        let spec = SPEC.replace(r#""constraints":["no new crate"]"#, r#""constraints":["no new crate","xaaa"]"#);
        let (_, why) = refused_alone(&spec, edit("constraints", "delete", 0, "aa", ""));
        assert!(why.contains("occurs 2 times in constraints (entries 1)"), "{why}");
        // ... while "aaa" occurs once and names entry 1
        let a = apply_edits(&spec, &[edit("constraints", "delete", 0, "aaa", "")], &ids()).unwrap();
        assert_eq!(a.edits[0].at, 1);
        // past the end and nowhere
        let (_, why) = refused_alone(SPEC, edit("constraints", "replace", 4, "no old crate", "x"));
        assert!(why.contains("at 4 is past the end of constraints (1 entries)"), "{why}");
        assert!(why.contains("occurs nowhere"), "{why}");
        // blank and whitespace-only
        for blank in ["", "  \t "] {
            let (_, why) = refused_alone(SPEC, edit("constraints", "replace", 0, blank, "x"));
            assert!(why.starts_with("anchor is blank"), "{why}");
        }
        // never across fields: the goal's words do not name a constraint
        let (_, why) = refused_alone(SPEC, edit("constraints", "delete", 0, "ledger", ""));
        assert!(why.contains("occurs nowhere in constraints"), "{why}");
    }

    /// SR-4 (AC2): scalar values, no normalization, no whitespace folding.
    #[test]
    fn unicode_anchors_are_compared_as_written() {
        let spec = r#"{"goal":"守住帳本不讓重複花費","acceptance_criteria":["AC1: 拒絕重複花費"],"constraints":["不加新套件","不改日誌格式","😀 emoji 開頭","café with a combining accent","two  spaces here"],"non_goals":[],"excluded_alternatives":[],"risk":"low"}"#;
        let ids = vec!["AC1".to_string()];
        let one = |e: SpecEdit| apply_edits(spec, &[e], &ids).unwrap();
        // one Chinese character shared by two entries; at picks
        let a = one(edit("constraints", "delete", 1, "不", ""));
        assert_eq!(a.edits[0].before.as_deref(), Some("不改日誌格式"));
        // under 10 scalar values, over 10 UTF-8 bytes
        // 4 scalar values, 12 UTF-8 bytes
        let a = one(edit("goal", "replace", 0, "守住帳本", "守住帳本，不重複"));
        assert_eq!(proposed_of(&a)["goal"], "守住帳本，不重複");
        // a supplementary character is one scalar value
        let a = one(edit("constraints", "replace", 2, "😀", "😀 emoji first"));
        assert_eq!(a.edits[0].at, 2);
        // the combining spelling as written matches; the precomposed one does not
        let a = one(edit("constraints", "delete", 0, "cafe\u{301}", ""));
        assert_eq!(a.edits[0].at, 3);
        let r = one(edit("constraints", "delete", 0, "caf\u{e9}", ""));
        assert!(r.edits.is_empty() && r.refused[0].reason.contains("occurs nowhere"), "{r:?}");
        // internal whitespace is not folded
        let r = one(edit("constraints", "delete", 4, "two spaces", ""));
        assert!(r.edits.is_empty(), "{r:?}");
        let a = one(edit("constraints", "delete", 4, "two  spaces", ""));
        assert_eq!(a.edits[0].at, 4);
    }

    #[test]
    fn an_insert_past_the_end_is_refused_and_appending_is_not() {
        let (index, why) = refused_alone(SPEC, edit("constraints", "insert", 2, "", "another"));
        assert_eq!(index, 0);
        assert!(why.contains("insert allows 0 to 1"), "{why}");
        // appending is an insert at the length; a missing list is empty
        let e = edit("excluded_alternatives", "insert", 0, "", "a crate we looked at");
        let a = apply_edits(SPEC, &[e], &ids()).unwrap();
        assert_eq!(proposed_of(&a)["excluded_alternatives"], json!(["a crate we looked at"]));
        let without: String = SPEC.replace(r#""constraints":["no new crate"],"#, "");
        let e = edit("constraints", "insert", 0, "", "the first constraint");
        let a = apply_edits(&without, &[e], &ids()).unwrap();
        assert_eq!(proposed_of(&a)["constraints"], json!(["the first constraint"]));
    }

    #[test]
    fn a_criterion_replace_must_keep_its_id_and_cannot_insert_or_delete() {
        let (index, why) = refused_alone(SPEC, edit("acceptance_criteria", "replace", 2, "AC3: exits", "AC4: exits zero quietly"));
        assert_eq!(index, 0);
        assert!(why.contains("AC1, AC2, AC4"), "{why}");
        let e = edit("acceptance_criteria", "replace", 2, "AC3: exits", "AC3: exits zero quietly");
        let a = apply_edits(SPEC, &[e], &ids()).unwrap();
        assert_eq!(proposed_of(&a)["acceptance_criteria"][2], "AC3: exits zero quietly");
        for op in ["insert", "delete"] {
            let (_, why) = refused_alone(SPEC, edit("acceptance_criteria", op, 0, "AC1: rejec", "AC9: x"));
            assert!(why.contains("allows only op replace"), "{op}: {why}");
        }
        let (_, why) = refused_alone(SPEC, edit("goal", "insert", 0, "guard the ", "guard the ledger better"));
        assert!(why.contains("allows only op replace"), "{why}");
    }

    #[test]
    fn only_the_five_fields_a_known_criterion_and_a_reason_are_accepted() {
        let (_, why) = refused_alone(SPEC, edit("risk", "replace", 0, "important", "low"));
        assert_eq!(
            why,
            "field must be one of goal, acceptance_criteria, constraints, non_goals, excluded_alternatives"
        );
        let (_, why) = refused_alone(SPEC, SpecEdit { criterion: "AC9".into(), ..edit("constraints", "insert", 0, "", "x") });
        assert!(why.contains("'AC9' is not an acceptance criterion"), "{why}");
        let (_, why) = refused_alone(SPEC, SpecEdit { reason: "  ".into(), ..edit("constraints", "insert", 0, "", "x") });
        assert!(why.contains("reason must not be blank"), "{why}");
        let (_, why) = refused_alone(SPEC, edit("constraints", "insert", 0, "", "   "));
        assert!(why.contains("text must not be blank"), "{why}");
        let (_, why) = refused_alone(SPEC, edit("constraints", "move", 0, "", "x"));
        assert!(why.contains("op 'move'"), "{why}");
    }

    #[test]
    fn three_edits_apply_in_order_and_each_at_counts_the_earlier_ones() {
        let edits = vec![
            edit("non_goals", "insert", 0, "", "first now"),
            // index 1 is what was index 0 before the insert
            edit("non_goals", "replace", 1, "rewriting t", "rewriting the transport layer"),
            edit("non_goals", "delete", 2, "changing t", ""),
        ];
        let a = apply_edits(SPEC, &edits, &ids()).unwrap();
        assert_eq!(proposed_of(&a)["non_goals"], json!(["first now", "rewriting the transport layer"]));
        assert_eq!(a.edits.len(), 3);
        assert!(a.refused.is_empty());
        assert_eq!(a.edits[1].index, 1);
        assert_eq!(a.edits[1].at, 1);
        assert_eq!(a.edits[1].before.as_deref(), Some("rewriting the transport"));
        assert_eq!(a.edits[1].after.as_deref(), Some("rewriting the transport layer"));
        assert_eq!(a.edits[0].before, None);
        assert_eq!(a.edits[2].after, None);
    }

    /// SR-4 (AC3): a refused edit changes nothing, the later ones still
    /// apply against the proposal the applied ones left, and every edit keeps
    /// its index in the reviewer's list.
    #[test]
    fn a_refused_edit_is_skipped_and_the_rest_keep_their_indices() {
        let edits = vec![
            edit("constraints", "replace", 0, "no new", "no new crate, ever"),
            // "the" is in both non_goals and starts neither: refused
            edit("non_goals", "delete", 0, "the", ""),
            edit("non_goals", "insert", 0, "", "first now"),
            // at 9 and nowhere: refused
            edit("non_goals", "delete", 9, "no such text", ""),
            // the refused delete above did not happen, so the transport
            // non-goal is still at 1 after the insert
            edit("non_goals", "replace", 1, "rewriting", "rewriting the transport layer"),
        ];
        let a = apply_edits(SPEC, &edits, &ids()).unwrap();
        let indices: Vec<usize> = a.edits.iter().map(|e| e.index).collect();
        assert_eq!(indices, [0, 2, 4]);
        let refused: Vec<(usize, &str)> = a.refused.iter().map(|r| (r.index, r.field.as_str())).collect();
        assert_eq!(refused, [(1, "non_goals"), (3, "non_goals")]);
        let v = proposed_of(&a);
        assert_eq!(v["constraints"], json!(["no new crate, ever"]));
        assert_eq!(v["non_goals"], json!(["first now", "rewriting the transport layer", "changing the log format"]));
        assert_eq!(a.edits[2].at, 1);

        // every edit refused: nothing is proposed, which is not an error
        let a = apply_edits(SPEC, &edits[1..2], &ids()).unwrap();
        assert!(a.edits.is_empty());
        assert_eq!(a.proposed, None);
        assert_eq!(a.refused[0].index, 0);
    }

    #[test]
    fn the_proposed_spec_keeps_unknown_keys_in_a_fixed_order() {
        let e = edit("constraints", "insert", 1, "", "second");
        let a = apply_edits(SPEC, &[e], &ids()).unwrap();
        let proposed = a.proposed.clone().unwrap();
        let keys: Vec<&str> = proposed
            .lines()
            .filter(|l| l.starts_with("  \""))
            .map(|l| l.trim().split('"').nth(1).unwrap())
            .collect();
        assert_eq!(
            keys,
            ["goal", "acceptance_criteria", "constraints", "non_goals", "excluded_alternatives", "risk", "alpha", "rework"]
        );
        let v = proposed_of(&a);
        assert_eq!(v["rework"], json!({"attempt": 2}));
        assert_eq!(v["alpha"], 1);
        assert!(proposed.ends_with("}\n"));
        assert!(!proposed.contains('\r'));
        assert!(proposed.starts_with("{\n  \"goal\": \"guard the ledger\",\n  \"acceptance_criteria\": [\n    \"AC1"));
        // the same input serializes the same way twice
        let b = apply_edits(SPEC, &[edit("constraints", "insert", 1, "", "second")], &ids()).unwrap();
        assert_eq!(a.proposed, b.proposed);
    }

    #[test]
    fn an_approve_with_edits_is_refused_and_a_revise_of_only_edits_is_accepted() {
        let approve = r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}],"edits":[{"field":"constraints","op":"insert","at":0,"anchor":"","text":"x","criterion":"AC1","reason":"r"}]}"#;
        let err = parse_edit_document(approve).unwrap_err();
        assert!(
            matches!(err, crate::spec_review_json::SpecReviewError::ApproveWithEdits(1)),
            "{err}"
        );
        let revise = r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}],"edits":[{"field":"constraints","op":"insert","at":0,"anchor":"","text":"x","criterion":"AC1","reason":"r"}]}"#;
        let doc = parse_edit_document(revise).unwrap();
        assert_eq!(doc.edits.len(), 1);
        assert_eq!(doc.review.criteria.len(), 1);
        // without edits the old rule holds
        let bare = r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}],"edits":[]}"#;
        assert!(matches!(
            parse_edit_document(bare),
            Err(crate::spec_review_json::SpecReviewError::ReviseWithoutRevise)
        ));
        // an APPROVE with an empty edits array is fine
        let plain = r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}],"edits":[]}"#;
        assert!(parse_edit_document(plain).unwrap().edits.is_empty());
    }

    /// Second code review, AC2: the edit document has the strict shape the
    /// schema describes. `edits` is always there (empty when nothing is
    /// proposed), and every edit carries all seven properties and no other.
    #[test]
    fn an_edit_document_without_edits_or_with_a_malformed_edit_is_refused() {
        let no_edits = r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}]}"#;
        let err = parse_edit_document(no_edits).unwrap_err().to_string();
        assert!(err.contains("no edits array"), "{err}");
        let null_edits = r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}],"edits":null}"#;
        let err = parse_edit_document(null_edits).unwrap_err().to_string();
        assert!(err.contains("no edits array"), "{err}");
        // a property left out is not defaulted
        let missing = r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}],"edits":[{"field":"constraints","op":"insert","at":0,"text":"x","criterion":"AC1","reason":"r"}]}"#;
        let err = parse_edit_document(missing).unwrap_err().to_string();
        assert!(err.contains("edits array is malformed"), "{err}");
        assert!(err.contains("anchor"), "{err}");
        // a property the schema does not have is refused, not ignored
        let extra = r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}],"edits":[{"field":"constraints","op":"insert","at":0,"anchor":"","text":"x","criterion":"AC1","reason":"r","position":1}]}"#;
        let err = parse_edit_document(extra).unwrap_err().to_string();
        assert!(err.contains("edits array is malformed"), "{err}");
        assert!(err.contains("position"), "{err}");
        // not an array
        let object = r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}],"edits":{}}"#;
        let err = parse_edit_document(object).unwrap_err().to_string();
        assert!(err.contains("edits array is malformed"), "{err}");
        // the complete shape still parses
        let complete = r#"{"verdict":"REVISE","criteria":[{"id":"AC1","verdict":"ok","findings":[]}],"edits":[{"field":"constraints","op":"insert","at":0,"anchor":"","text":"x","criterion":"AC1","reason":"r"}]}"#;
        assert_eq!(parse_edit_document(complete).unwrap().edits.len(), 1);
    }

    #[test]
    fn the_author_document_is_checked_for_shape_and_coverage() {
        let ok = parse_confirm_document(
            r#"{"verdict":"REVISE","edits":[{"index":0,"verdict":"ok","note":""},{"index":1,"verdict":"revise","note":"keep it"}]}"#,
        )
        .unwrap();
        assert!(check_confirm_coverage(&ok, 2).is_ok());
        assert!(matches!(check_confirm_coverage(&ok, 3), Err(ConfirmError::MissingEdit(2))));
        assert!(matches!(check_confirm_coverage(&ok, 1), Err(ConfirmError::UnknownEdit(1))));
        let twice = parse_confirm_document(
            r#"{"verdict":"APPROVE","edits":[{"index":0,"verdict":"ok","note":""},{"index":0,"verdict":"ok","note":""}]}"#,
        )
        .unwrap();
        assert!(matches!(check_confirm_coverage(&twice, 1), Err(ConfirmError::DuplicateEdit(0))));
        // SR-4: applied indices with a gap; the refused index 1 is unknown
        assert!(check_confirm_indices(&ok, &[0, 1]).is_ok());
        assert!(matches!(check_confirm_indices(&ok, &[0, 2]), Err(ConfirmError::MissingEdit(2))));
        let gap = parse_confirm_document(
            r#"{"verdict":"APPROVE","edits":[{"index":0,"verdict":"ok","note":""},{"index":2,"verdict":"ok","note":""}]}"#,
        )
        .unwrap();
        assert!(check_confirm_indices(&gap, &[0, 2]).is_ok());
        assert!(matches!(check_confirm_indices(&gap, &[0]), Err(ConfirmError::UnknownEdit(2))));
        assert!(matches!(
            parse_confirm_document(r#"{"verdict":"REVISE","edits":[{"index":0,"verdict":"revise","note":" "}]}"#),
            Err(ConfirmError::ReviseWithoutNote(0))
        ));
        assert!(matches!(
            parse_confirm_document(r#"{"verdict":"APPROVE","edits":[{"index":0,"verdict":"revise","note":"n"}]}"#),
            Err(ConfirmError::ApproveWithRevise(0))
        ));
        assert!(matches!(
            parse_confirm_document(r#"{"verdict":"REVISE","edits":[{"index":0,"verdict":"ok","note":""}]}"#),
            Err(ConfirmError::ReviseWithoutRevise)
        ));
        assert!(matches!(
            parse_confirm_document(r#"{"verdict":"PASS","edits":[]}"#),
            Err(ConfirmError::BadVerdict(_))
        ));
    }

    /// Second code review, AC5: the author's document has the strict shape
    /// too. An answer without `index` or `note` is not defaulted into one
    /// that covers edit 0, and a key the schema does not have is refused.
    #[test]
    fn an_author_answer_missing_a_property_or_carrying_an_extra_one_is_refused() {
        let no_index = r#"{"verdict":"APPROVE","edits":[{"verdict":"ok"}]}"#;
        let err = parse_confirm_document(no_index).unwrap_err().to_string();
        assert!(err.contains("author's shape"), "{err}");
        assert!(err.contains("index"), "{err}");
        let no_note = r#"{"verdict":"APPROVE","edits":[{"index":0,"verdict":"ok"}]}"#;
        let err = parse_confirm_document(no_note).unwrap_err().to_string();
        assert!(err.contains("note"), "{err}");
        let extra = r#"{"verdict":"APPROVE","edits":[{"index":0,"verdict":"ok","note":"","comment":"x"}]}"#;
        let err = parse_confirm_document(extra).unwrap_err().to_string();
        assert!(err.contains("comment"), "{err}");
        let no_edits = r#"{"verdict":"APPROVE"}"#;
        let err = parse_confirm_document(no_edits).unwrap_err().to_string();
        assert!(err.contains("edits"), "{err}");
        let top_extra = r#"{"verdict":"APPROVE","edits":[],"criteria":[]}"#;
        let err = parse_confirm_document(top_extra).unwrap_err().to_string();
        assert!(err.contains("criteria"), "{err}");
        // text that is not JSON at all is still reported as such
        let err = parse_confirm_document("{").unwrap_err();
        assert!(matches!(err, ConfirmError::Json(_)), "{err}");
    }

    #[test]
    fn both_schemas_are_strict() {
        for (name, s) in [("edit", edit_output_schema()), ("confirm", confirm_output_schema())] {
            fn strict(v: &Value, at: &str) {
                if v["type"] == "object" {
                    assert_eq!(v["additionalProperties"], json!(false), "{at}");
                    let required: Vec<&str> =
                        v["required"].as_array().unwrap().iter().map(|x| x.as_str().unwrap()).collect();
                    for (k, sub) in v["properties"].as_object().unwrap() {
                        assert!(required.contains(&k.as_str()), "{at}.{k} not required");
                        strict(sub, &format!("{at}.{k}"));
                    }
                }
                if let Some(items) = v.get("items") {
                    strict(items, &format!("{at}[]"));
                }
            }
            strict(&s, name);
        }
        let e = edit_output_schema();
        assert_eq!(e["required"], json!(["verdict", "criteria", "edits"]));
        assert_eq!(e["properties"]["edits"]["items"]["properties"]["field"]["enum"], json!(EDIT_FIELDS));
        let c = confirm_output_schema();
        assert_eq!(c["required"], json!(["verdict", "edits"]));
        assert_eq!(c["properties"]["edits"]["items"]["required"], json!(["index", "verdict", "note"]));
    }

    #[test]
    fn review_modes_round_trip() {
        for m in [ReviewMode::Review, ReviewMode::Edit, ReviewMode::Confirm] {
            assert_eq!(ReviewMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(ReviewMode::parse("external"), None);
    }
}
