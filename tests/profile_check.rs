mod common;

use predicates::prelude::*;

#[test]
fn profile_check_reports_the_fake_identity() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");

    f.cmd()
        .args(["profile", "check", "claude-b", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"logged_in\":true"))
        .stdout(predicate::str::contains("fake@example.com"))
        .stdout(predicate::str::contains("\"subscription_type\":\"pro\""));
}

#[test]
fn profile_check_flips_a_rate_limited_profile_back_to_active() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");

    // simulate what the runner does when it detects a quota interruption
    {
        let g = f.global_conn();
        g.execute(
            "UPDATE profiles SET status='rate_limited' WHERE profile_id='claude-b'",
            [],
        )
        .unwrap();
    }

    f.cmd()
        .args(["profile", "check", "claude-b"])
        .assert()
        .success();

    let g = f.global_conn();
    let status: String = g
        .query_row(
            "SELECT status FROM profiles WHERE profile_id='claude-b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "active");

    let kinds: Vec<String> = g
        .prepare("SELECT kind FROM quota_events WHERE profile_id='claude-b' ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    assert!(kinds.contains(&"window_reset".to_string()), "{kinds:?}");
}

#[test]
fn profile_check_on_an_unknown_profile_fails() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.cmd()
        .args(["profile", "check", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such profile"));
}

/// A `.cmd` that is not an npm shim is refused, never run: it would go through
/// cmd.exe, which rewrites the arguments. The file it would write proves it
/// did not run.
#[test]
#[cfg(windows)]
fn profile_check_refuses_to_run_a_non_shim_cmd() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-a");
    let bin_dir = tempfile::tempdir().unwrap();
    let marker = bin_dir.path().join("ran.txt");
    let cmd_file = bin_dir.path().join("codex.cmd");
    std::fs::write(&cmd_file, format!("@echo off\r\necho hi > \"{}\"\r\n", marker.display())).unwrap();

    let before: String = f
        .global_conn()
        .query_row("SELECT status || config_dir FROM profiles WHERE profile_id='codex-a'", [], |r| r.get(0))
        .unwrap();
    f.cmd()
        .env("THOTH_CODEX_BIN", &cmd_file)
        .args(["profile", "check", "codex-a"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(cmd_file.display().to_string()))
        .stderr(predicate::str::contains("THOTH_CODEX_BIN"));
    assert!(!marker.exists(), "the .cmd must not have been executed");
    let after: String = f
        .global_conn()
        .query_row("SELECT status || config_dir FROM profiles WHERE profile_id='codex-a'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(before, after);
    assert!(cfg.exists());
}
