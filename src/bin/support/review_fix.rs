//! RFIX-1 scenarios shared by the three fake harnesses. `review_fix` reads
//! the JSON file `FAKE_FIX_PLAN` names: `document` is the review.json the
//! reviewer hands in (edits included), `write` is a list of `{path,
//! content}` files it writes itself (relative to its cwd), and `git` is a
//! list of git argument lists it runs there (to move HEAD). The
//! `fix_confirm_*` scenarios read `.agent/review-fix-input/edits.json` and
//! answer every edit; they honour `write` and `git` too, so a test can make
//! a confirmer that changes files. Nothing here opens the network.

use std::path::Path;

/// The parsed `FAKE_FIX_PLAN`, `null` when it is unset or unreadable.
pub fn plan() -> serde_json::Value {
    std::env::var_os("FAKE_FIX_PLAN")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null)
}

/// The plan's `write` and `git` side effects, in the current directory.
pub fn side_effects(plan: &serde_json::Value) {
    for w in plan["write"].as_array().into_iter().flatten() {
        let (Some(path), Some(content)) = (w["path"].as_str(), w["content"].as_str()) else { continue };
        if let Some(parent) = Path::new(path).parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(path, content);
    }
    for args in plan["git"].as_array().into_iter().flatten() {
        let args: Vec<String> = args.as_array().into_iter().flatten().filter_map(|a| a.as_str().map(String::from)).collect();
        let _ = std::process::Command::new("git")
            .args(["-c", "user.name=fake reviewer", "-c", "user.email=fake@example.com"])
            .args(&args)
            .output();
    }
}

/// The edit-mode document the plan hands in.
pub fn document(plan: &serde_json::Value) -> serde_json::Value {
    plan["document"].clone()
}

/// What strict structured output demands of every object in a schema:
/// `additionalProperties: false` and every property in `required`. Claude
/// Code gets no schema, so fake-claude does not call it.
#[allow(dead_code)]
pub fn strict_violation(v: &serde_json::Value, at: &str) -> Option<String> {
    if v["type"] == "object" {
        if v["additionalProperties"] != serde_json::Value::Bool(false) {
            return Some(format!("{at}: additionalProperties must be false"));
        }
        let required: Vec<&str> = v["required"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        for (key, sub) in v["properties"].as_object().into_iter().flatten() {
            if !required.contains(&key.as_str()) {
                return Some(format!("{at}: '{key}' is missing from required"));
            }
            if let Some(bad) = strict_violation(sub, &format!("{at}.{key}")) {
                return Some(bad);
            }
        }
    }
    if let Some(items) = v.get("items") {
        return strict_violation(items, &format!("{at}[]"));
    }
    None
}

/// The four confirmer input files exist, and `edits.json` is a list.
pub fn read_edits() -> Option<Vec<serde_json::Value>> {
    let dir = Path::new(".agent").join("review-fix-input");
    for f in ["spec.json", "findings.json", "fix.patch"] {
        if !dir.join(f).is_file() {
            return None;
        }
    }
    std::fs::read_to_string(dir.join("edits.json")).ok().and_then(|t| serde_json::from_str(&t).ok())
}

/// The confirmer's document: every edit ok, or (`revise`) edit 0 revise.
pub fn confirm_document(edits: &[serde_json::Value], revise: bool) -> serde_json::Value {
    let answers: Vec<serde_json::Value> = edits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let index = e["index"].as_u64().unwrap_or(i as u64);
            if revise && i == 0 {
                serde_json::json!({"index": index, "verdict": "revise", "note": "fake confirmer: this edit weakens the check it touches"})
            } else {
                serde_json::json!({"index": index, "verdict": "ok", "note": ""})
            }
        })
        .collect();
    serde_json::json!({"verdict": if revise { "REVISE" } else { "APPROVE" }, "edits": answers})
}

/// `FAKE_CONFIRM_DOC`, when set, replaces the confirmer's document (raw
/// JSON), for the coverage and consistency cases.
pub fn confirm_override() -> Option<serde_json::Value> {
    std::env::var("FAKE_CONFIRM_DOC").ok().and_then(|t| serde_json::from_str(&t).ok())
}
