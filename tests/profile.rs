use assert_cmd::Command;
use predicates::prelude::*;

fn thoth(home: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
    c.env("THOTH_HOME", home);
    c
}

#[test]
fn profile_add_then_list_shows_the_profile() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("profiles").join("claude-b");
    std::fs::create_dir_all(&cfg).unwrap();

    thoth(home.path())
        .args([
            "profile",
            "add",
            "claude-b",
            "--provider",
            "anthropic",
            "--harness",
            "claude-code",
            "--config-dir",
        ])
        .arg(&cfg)
        .args(["--label", "second account"])
        .assert()
        .success()
        .stdout(predicate::str::contains("claude-b"));

    thoth(home.path())
        .args(["profile", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"profile_id\":\"claude-b\""))
        .stdout(predicate::str::contains("\"status\":\"active\""))
        .stdout(predicate::str::contains("\"harness\":\"claude-code\""));

    // The stored path becomes CLAUDE_CONFIG_DIR for the worker, so it must be the
    // plain path, not the `\\?\` verbatim form std::fs::canonicalize returns on
    // Windows - otherwise it would not match what `claude auth status --json`
    // reports for the same directory.
    let g = rusqlite::Connection::open(home.path().join("global.db")).unwrap();
    let stored: String = g
        .query_row(
            "SELECT config_dir FROM profiles WHERE profile_id='claude-b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        !stored.starts_with(r"\\?\"),
        "config_dir kept the verbatim prefix: {stored}"
    );
    assert!(
        std::path::Path::new(&stored).is_dir(),
        "config_dir must still resolve: {stored}"
    );
}

#[test]
fn profile_add_rejects_a_duplicate_id() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();
    let mut first = thoth(home.path());
    first
        .args(["profile", "add", "claude-a", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();

    thoth(home.path())
        .args(["profile", "add", "claude-a", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&cfg)
        .assert()
        .failure()
        .stderr(predicate::str::contains("already exists"));
}

#[test]
fn profile_add_rejects_a_missing_config_dir() {
    let home = tempfile::tempdir().unwrap();
    thoth(home.path())
        .args([
            "profile",
            "add",
            "claude-c",
            "--provider",
            "anthropic",
            "--harness",
            "claude-code",
            "--config-dir",
            "C:/definitely/not/here/at/all",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains("config dir does not exist"));
}

#[test]
fn profile_list_on_an_empty_home_prints_an_empty_array() {
    let home = tempfile::tempdir().unwrap();
    thoth(home.path())
        .args(["profile", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"profiles\":[]"));
}

#[test]
fn profile_add_refuses_a_harness_other_than_the_supported_values() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("cfg");
    std::fs::create_dir_all(&cfg).unwrap();

    thoth(home.path())
        .args(["profile", "add", "other-a", "--provider", "google", "--harness", "gemini", "--config-dir"])
        .arg(&cfg)
        .assert()
        .failure()
        .stderr(predicate::str::contains("only claude-code, codex"))
        .stderr(predicate::str::contains("are supported"))
        .stderr(predicate::str::contains("gemini"));

    thoth(home.path())
        .args(["profile", "list", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"profiles\":[]"));

    thoth(home.path())
        .args(["profile", "add", "codex-a", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();
}

#[test]
fn profile_set_config_dir_repoints_an_existing_profile() {
    let home = tempfile::tempdir().unwrap();
    let old = home.path().join("old");
    let new = home.path().join("new");
    std::fs::create_dir_all(&old).unwrap();
    std::fs::create_dir_all(&new).unwrap();
    thoth(home.path())
        .args(["profile", "add", "p1", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&old)
        .assert()
        .success();

    let out = thoth(home.path())
        .args(["profile", "set-config-dir", "p1"])
        .arg(&new)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["profile_id"], "p1");
    let want = std::fs::canonicalize(&new).unwrap();
    let got = v["config_dir"].as_str().unwrap().to_string();
    assert!(got.ends_with("new"), "{got}");
    assert!(std::path::Path::new(&got).is_dir());
    assert_eq!(
        std::fs::canonicalize(&got).unwrap(),
        want,
        "stored path must resolve to the new dir"
    );

    // Unknown profile and missing directory both fail and leave the row alone.
    thoth(home.path())
        .args(["profile", "set-config-dir", "nope"])
        .arg(&new)
        .assert()
        .failure();
    thoth(home.path())
        .args(["profile", "set-config-dir", "p1"])
        .arg(home.path().join("missing"))
        .assert()
        .failure();
    let g = rusqlite::Connection::open(home.path().join("global.db")).unwrap();
    let stored: String = g
        .query_row("SELECT config_dir FROM profiles WHERE profile_id='p1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(stored, got);
}

#[test]
fn profile_add_rejects_an_id_that_differs_only_in_case() {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join("cfg");
    let cfg2 = home.path().join("cfg2");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::create_dir_all(&cfg2).unwrap();
    thoth(home.path())
        .args(["profile", "add", "claude-a", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();

    thoth(home.path())
        .args(["profile", "add", "CLAUDE-A", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&cfg2)
        .assert()
        .failure()
        .code(1)
        .stderr(predicate::str::contains("claude-a").and(predicate::str::contains("letter case")));

    let out = thoth(home.path()).args(["profile", "list", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["profiles"].as_array().unwrap().len(), 1, "{v}");
}
