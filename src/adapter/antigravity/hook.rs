//! `thoth hook --agy-pre` (`run_agy_pre`): agy's PreToolUse tool lock
//! and the one exception to `thoth hook`'s stdout rule: it prints exactly
//! one decision line.

use crate::ids;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// `thoth hook --agy-pre`: agy's PreToolUse hook (antigravity scope G2),
/// the one documented exception to "never print to stdout". agy reads the
/// hook's stdout as its decision, so this prints exactly one line, allow or
/// deny with a reason, and never `{}` or nothing: a payload or policy it
/// cannot use is a deny that says why. The other rules still hold: exit 0,
/// no database, no environment variables, no network.
pub const AGY_ALLOW_LINE: &str = r#"{"decision":"allow"}"#;

/// `<workspacePaths[0]>/.agent/agy-hook.json`, or `None` when the payload
/// names no usable workspace.
pub fn agy_policy_path(payload: &str) -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_str(payload.trim()).ok()?;
    let workspace = v["workspacePaths"][0].as_str().filter(|w| !w.is_empty())?;
    Some(
        Path::new(workspace)
            .join(".agent")
            .join(super::POLICY_FILE),
    )
}

/// The whole decision, minus stdin, the clock and the file system: the
/// payload text and the policy file's text (`None` when it could not be read)
/// in; the stdout line and, for a deny, the record to append out.
///
/// AGY-1 (AC2): a denied name Thoth does not know at all
/// (`AGY_KNOWN_TOOLS`) gets a reason saying agy may have renamed its tools,
/// so a rename shows up on the first denied call instead of after an
/// UNVERIFIED review; the record carries `known_to_thoth` either way
/// (`false` when there is no usable name). A payload or policy the hook
/// cannot use keeps its own reason.
pub fn agy_pre_decision(
    payload: &str,
    policy: Option<&str>,
    received_at: &str,
) -> (String, Option<serde_json::Value>) {
    let parsed = serde_json::from_str::<serde_json::Value>(payload.trim());
    let v = parsed.as_ref().ok();
    let tool = v.and_then(|v| v["toolCall"]["name"].as_str()).filter(|t| !t.is_empty());
    let known_to_thoth = tool.is_some_and(super::is_known_tool);
    let policy_path = agy_policy_path(payload);
    let policy_doc = policy.and_then(|p| serde_json::from_str::<serde_json::Value>(p).ok());
    let run_id = policy_doc.as_ref().and_then(|p| p["run_id"].as_str()).map(String::from);

    let reason = match (&parsed, tool, &policy_path) {
        (Err(e), _, _) => format!("Thoth tool lock: stdin was not a JSON payload ({e}), so the tool call is denied"),
        (Ok(_), None, _) => "Thoth tool lock: the payload has no toolCall.name, so the tool call is denied".to_string(),
        (Ok(_), Some(_), None) => "Thoth tool lock: the payload has no non-empty workspacePaths[0], so the policy file cannot be found and the tool call is denied".to_string(),
        (Ok(_), Some(tool), Some(path)) => {
            let allowed = policy_doc
                .as_ref()
                .and_then(|p| p["allowed_tools"].as_array())
                .map(|a| a.iter().filter_map(|t| t.as_str()).collect::<Vec<_>>());
            match (policy, &policy_doc, allowed) {
                (None, _, _) => format!("Thoth tool lock: cannot read the policy file {}, so the tool call is denied", path.display()),
                (Some(_), None, _) => format!("Thoth tool lock: the policy file {} is not valid JSON, so the tool call is denied", path.display()),
                (Some(_), Some(_), None) => format!("Thoth tool lock: the policy file {} has no allowed_tools array, so the tool call is denied", path.display()),
                (Some(_), Some(_), Some(allowed)) => {
                    if allowed.contains(&tool) {
                        return (AGY_ALLOW_LINE.to_string(), None);
                    }
                    if known_to_thoth {
                        format!(
                            "Thoth tool lock: the tool {tool} is not on this run's allow-list ({}); do the work with the allowed tools",
                            allowed.join(", ")
                        )
                    } else {
                        format!(
                            "Thoth tool lock: the tool {tool} is not a tool Thoth knows; agy may have renamed its tools, so update Thoth's agy tool table (AGY-1); this run's allow-list is ({})",
                            allowed.join(", ")
                        )
                    }
                }
            }
        }
    };
    let line = serde_json::json!({"decision": "deny", "reason": reason}).to_string();
    let record = serde_json::json!({
        "received_at": received_at,
        "run_id": run_id,
        "tool": tool,
        "known_to_thoth": known_to_thoth,
        "reason": reason,
        "step_idx": v.map(|v| v["stepIdx"].clone()).unwrap_or(serde_json::Value::Null),
        "conversation_id": v.map(|v| v["conversationId"].clone()).unwrap_or(serde_json::Value::Null),
    });
    (line, Some(record))
}

/// The subcommand behind `--agy-pre`. Returns 0 unconditionally.
pub fn run_agy_pre() -> i32 {
    let mut buf: Vec<u8> = Vec::new();
    let raw = match std::io::stdin().read_to_end(&mut buf) {
        Ok(_) => String::from_utf8_lossy(&buf).into_owned(),
        Err(_) => String::new(),
    };
    let policy_path = agy_policy_path(&raw);
    let policy = policy_path.as_ref().and_then(|p| std::fs::read_to_string(p).ok());
    let (line, denial) = agy_pre_decision(&raw, policy.as_deref(), &ids::now_rfc3339());
    // The decision is printed first: a failed append must not change it.
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
    if let (Some(record), Some(dir)) = (denial, policy_path.as_ref().and_then(|p| p.parent())) {
        let _ = append_agy_denial(dir, &record);
    }
    0
}

fn append_agy_denial(agent_dir: &Path, record: &serde_json::Value) -> std::io::Result<()> {
    std::fs::create_dir_all(agent_dir)?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(agent_dir.join(super::DENIALS_FILE))?;
    let mut bytes = record.to_string().into_bytes();
    bytes.push(b'\n');
    f.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agy_payload(tool: &str) -> String {
        serde_json::json!({"conversationId": "c-1", "stepIdx": 4,
            "toolCall": {"name": tool, "args": {}}, "workspacePaths": ["/ws"]}).to_string()
    }

    const AGY_POLICY: &str = r#"{"run_id":"R1","role":"implementer","allowed_tools":["read_file","view_file","run_command"]}"#;

    #[test]
    fn agy_pre_allows_a_listed_tool_and_records_nothing() {
        for tool in ["read_file", "view_file"] {
            let (line, denial) = agy_pre_decision(&agy_payload(tool), Some(AGY_POLICY), "T");
            assert_eq!(line, r#"{"decision":"allow"}"#, "{tool}");
            assert!(denial.is_none(), "{tool}");
        }
        assert_eq!(agy_policy_path(&agy_payload("read_file")),
            Some(Path::new("/ws").join(".agent").join("agy-hook.json")));
        // a name on the policy's list is allowed whether or not Thoth knows it
        let custom = r#"{"run_id":"R1","role":"implementer","allowed_tools":["frobnicate_file"]}"#;
        let (line, denial) = agy_pre_decision(&agy_payload("frobnicate_file"), Some(custom), "T");
        assert_eq!(line, r#"{"decision":"allow"}"#);
        assert!(denial.is_none());
    }

    #[test]
    fn agy_pre_denies_an_unlisted_tool_with_a_reason_and_a_record() {
        let (line, denial) = agy_pre_decision(&agy_payload("search_web"), Some(AGY_POLICY), "T");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["decision"], "deny");
        let reason = v["reason"].as_str().unwrap();
        assert!(reason.contains("search_web") && reason.contains("allow-list"), "{reason}");
        assert!(!reason.contains("not a tool Thoth knows"), "{reason}");
        assert_eq!(denial.unwrap(), serde_json::json!({"received_at": "T", "run_id": "R1",
            "tool": "search_web", "known_to_thoth": true, "reason": reason, "step_idx": 4, "conversation_id": "c-1"}));
    }

    /// AGY-1 (AC2): a name outside `AGY_KNOWN_TOOLS` is still a deny, but
    /// the reason says the table may be stale and the record says
    /// `known_to_thoth: false`.
    #[test]
    fn agy_pre_denies_an_unknown_tool_naming_the_rename_and_records_it_as_unknown() {
        let (line, denial) = agy_pre_decision(&agy_payload("frobnicate_file"), Some(AGY_POLICY), "T");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["decision"], "deny");
        assert_eq!(v.as_object().unwrap().len(), 2);
        let reason = v["reason"].as_str().unwrap();
        assert_eq!(reason, "Thoth tool lock: the tool frobnicate_file is not a tool Thoth knows; agy may have renamed its tools, so update Thoth's agy tool table (AGY-1); this run's allow-list is (read_file, view_file, run_command)");
        assert_eq!(denial.unwrap(), serde_json::json!({"received_at": "T", "run_id": "R1",
            "tool": "frobnicate_file", "known_to_thoth": false, "reason": reason, "step_idx": 4, "conversation_id": "c-1"}));
        // names are matched exactly: a case or whitespace variant is unknown
        for variant in ["Read_File", " read_file", "read_file "] {
            let (_, denial) = agy_pre_decision(&agy_payload(variant), Some(AGY_POLICY), "T");
            let d = denial.unwrap();
            assert_eq!(d["known_to_thoth"], false, "{variant:?}");
            assert!(d["reason"].as_str().unwrap().contains("not a tool Thoth knows"), "{variant:?}");
        }
    }

    #[test]
    fn agy_pre_denies_an_unparsable_payload() {
        let (line, denial) = agy_pre_decision("half {", Some(AGY_POLICY), "T");
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["decision"], "deny");
        assert!(v["reason"].as_str().unwrap().contains("stdin"), "{v}");
        assert!(!v["reason"].as_str().unwrap().contains("Thoth knows"), "{v}");
        let d = denial.unwrap();
        assert!(d["tool"].is_null() && d["step_idx"].is_null() && d["conversation_id"].is_null(), "{d}");
        assert_eq!(d["known_to_thoth"], false, "no usable name is not a known one: {d}");
        assert_eq!(d["run_id"], "R1");
        assert_eq!(agy_policy_path("half {"), None);
        let (empty, _) = agy_pre_decision("", None, "T");
        assert!(empty.contains("stdin"), "{empty}");
    }

    #[test]
    fn agy_pre_denies_when_the_policy_is_missing_or_unusable() {
        let path = Path::new("/ws").join(".agent").join("agy-hook.json").display().to_string();
        // the policy's own reason wins, for a known and an unknown name alike
        for tool in ["read_file", "frobnicate_file"] {
            for (policy, needle) in [(None, "cannot read"), (Some("nope"), "not valid JSON"),
                (Some(r#"{"run_id":"R1"}"#), "allowed_tools")] {
                let (line, denial) = agy_pre_decision(&agy_payload(tool), policy, "T");
                let v: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(v["decision"], "deny");
                let reason = v["reason"].as_str().unwrap();
                assert!(reason.contains(&path) && reason.contains(needle), "{reason}");
                assert!(!reason.contains("Thoth knows"), "{reason}");
                let d = denial.unwrap();
                assert_eq!(d["tool"], tool);
                assert_eq!(d["known_to_thoth"], tool == "read_file", "{d}");
            }
        }
        let nameless = serde_json::json!({"toolCall": {}, "workspacePaths": ["/ws"]}).to_string();
        let (line, denial) = agy_pre_decision(&nameless, Some(AGY_POLICY), "T");
        assert!(line.contains("toolCall.name"));
        assert_eq!(denial.unwrap()["known_to_thoth"], false);
        let blank = serde_json::json!({"toolCall": {"name": ""}, "workspacePaths": ["/ws"]}).to_string();
        let (line, denial) = agy_pre_decision(&blank, Some(AGY_POLICY), "T");
        assert!(line.contains("toolCall.name"));
        assert_eq!(denial.unwrap()["known_to_thoth"], false);
        let homeless = serde_json::json!({"toolCall": {"name": "read_file"}, "workspacePaths": [""]}).to_string();
        let (line, denial) = agy_pre_decision(&homeless, Some(AGY_POLICY), "T");
        assert!(line.contains("workspacePaths"));
        assert_eq!(denial.unwrap()["known_to_thoth"], true);
    }
}
