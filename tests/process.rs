fn fake() -> &'static str {
    env!("CARGO_BIN_EXE_fake-claude")
}

#[cfg(windows)]
fn tasklist_has(pid: u32) -> bool {
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .expect("tasklist");
    String::from_utf8_lossy(&out.stdout).contains(&format!("\"{pid}\""))
}

#[test]
fn fake_claude_ok_prints_three_event_kinds_and_writes_result_json() {
    let ws = tempfile::tempdir().unwrap();
    let out = std::process::Command::new(fake())
        .args([
            "-p", "do the thing",
            "--session-id", "11111111-2222-3333-4444-555555555555",
            "--model", "fake",
            "--output-format", "stream-json",
            "--verbose",
            "--allowedTools", "Bash",
        ])
        .env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", "01HZTASKFORFAKECLAUDE01")
        .current_dir(ws.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    let kinds: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: serde_json::Value = serde_json::from_str(l).expect("each line must be JSON");
            match (v["type"].as_str(), v["subtype"].as_str()) {
                (Some(t), Some(s)) => format!("{t}/{s}"),
                (Some(t), None) => t.to_string(),
                _ => "?".to_string(),
            }
        })
        .collect();
    assert!(kinds.contains(&"system/init".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"assistant".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"result/success".to_string()), "{kinds:?}");

    let rj = ws.path().join(".agent").join("result.json");
    assert!(rj.is_file(), "fake-claude must write .agent/result.json");
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&rj).unwrap()).unwrap();
    assert_eq!(v["task_id"].as_str(), Some("01HZTASKFORFAKECLAUDE01"));
    assert_eq!(v["status"].as_str(), Some("completed"));

    // the init and result events must echo the session id we passed in
    let first: serde_json::Value =
        serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(
        first["session_id"].as_str(),
        Some("11111111-2222-3333-4444-555555555555")
    );
}

#[test]
fn fake_claude_auth_status_prints_the_identity_fields() {
    let out = std::process::Command::new(fake())
        .args(["auth", "status", "--json"])
        .env("CLAUDE_CONFIG_DIR", "C:/fake/config")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["loggedIn"].as_bool(), Some(true));
    assert_eq!(v["subscriptionType"].as_str(), Some("pro"));
    assert_eq!(v["email"].as_str(), Some("fake@example.com"));
    assert_eq!(v["configDirectory"].as_str(), Some("C:/fake/config"));
}

// `kill_tree_takes_out_the_grandchild_too` (the fake-claude two-level-tree
// kill test) was removed here in Task 6 fix round 2: it duplicated the
// coverage now in `src/process/windows.rs`'s own `#[cfg(test)]` tests
// (`spawn_child_then_kill_tree_clears_root_and_grandchild`,
// `kill_tree_clears_orphans_when_root_already_exited`), and this file - a
// separate crate from the `thoth` bin - has no way to call
// `ProcessController::kill_tree`/`is_alive` without a `src/lib.rs` split,
// which the controller ruled against (dead_code semantics for the
// scaffolding-allow gate). See task-6-report.md's "Fix round 2" section.

/// Windows-only: pins `taskkill`'s own exit code for a missing pid, which
/// `kill_tree` maps to success. Linux's `kill_tree` has no such tool to pin;
/// its "a gone pid is not an error" is `process::linux`'s
/// `kill_tree_on_a_pid_that_is_already_gone_succeeds`.
#[test]
#[cfg(windows)]
fn taskkill_on_a_dead_pid_is_not_an_error_for_us() {
    // exit 128 = "The process ... not found", which kill_tree maps to success
    let out = std::process::Command::new("taskkill")
        .args(["/PID", "999999", "/T", "/F"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(128));
    assert!(!tasklist_has(999999));
}
