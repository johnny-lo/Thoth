//! PANEL-3 AC7: `thoth profile login NAME` for a NAME not registered yet
//! takes its harness from the prefix (`claude-`, `codex-`, `agy-`, with
//! something after it) when `--harness` is not given. Every run uses the fake
//! CLIs and a temporary THOTH_HOME, the way tests/profile_login.rs does:
//! `THOTH_LOGIN_SKIP_TTY_CHECK=1` stands in for a terminal and
//! `FAKE_LOGIN_BY_FILE=1` makes the fakes logged in only after their login.

mod common;

use common::Fixture;
use serde_json::Value;

/// `profile login` as a test runs it, without any inherited variable whose
/// name holds KEY, TOKEN, SECRET or PASSWORD (names only are read).
fn login(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for name in std::env::vars_os().map(|(name, _)| name) {
        let upper = name.to_string_lossy().to_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c.env("THOTH_LOGIN_SKIP_TTY_CHECK", "1")
        .env("FAKE_LOGIN_BY_FILE", "1")
        .env_remove("THOTH_LANG")
        .env_remove("FAKE_LOGIN_FAIL")
        .args(["profile", "login"]);
    c
}

/// Exit code, stdout, stderr.
fn texts(c: &mut assert_cmd::Command) -> (i32, String, String) {
    let out = c.output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

/// Exit code and stdout as one JSON object; panics with stderr when it is not.
fn run(c: &mut assert_cmd::Command) -> (i32, Value) {
    let (code, stdout, stderr) = texts(c);
    let v = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout is not JSON ({e}): {stdout}\nstderr: {stderr}"));
    (code, v)
}

fn profiles(f: &Fixture) -> Vec<Value> {
    let out = f.cmd().args(["profile", "list", "--json"]).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["profiles"].as_array().unwrap().clone()
}

fn harness_of(f: &Fixture, id: &str) -> Option<String> {
    profiles(f)
        .iter()
        .find(|p| p["profile_id"] == id)
        .map(|p| p["harness"].as_str().unwrap().to_string())
}

/// A user home for agy's fixed directory, so no test touches the real one.
fn agy_home() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

#[test]
fn infer_claude_and_codex_from_the_name() {
    let f = common::fixture();
    let (code, v) = run(login(&f).args(["claude-x", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["profile_id"], "claude-x");
    assert_eq!(v["harness"], "claude-code");
    assert_eq!(v["provider"], "anthropic");
    assert_eq!(v["new"], true);
    assert_eq!(harness_of(&f, "claude-x").as_deref(), Some("claude-code"));

    let (code, v) = run(login(&f).args(["codex-x", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["harness"], "codex");
    assert_eq!(v["provider"], "openai");
    assert_eq!(v["new"], true);
    assert_eq!(harness_of(&f, "codex-x").as_deref(), Some("codex"));
}

#[test]
fn infer_agy_and_the_one_agy_rule_still_holds() {
    let f = common::fixture();
    let uh = agy_home();
    let agy = |f: &Fixture| {
        let mut c = login(f);
        c.env("USERPROFILE", uh.path()).env("HOME", uh.path());
        c
    };
    let (code, v) = run(agy(&f).args(["agy-x", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["harness"], "antigravity");
    assert_eq!(v["provider"], "google");
    assert_eq!(v["new"], true);
    assert_eq!(harness_of(&f, "agy-x").as_deref(), Some("antigravity"));

    // A second agy account is refused as with --harness antigravity.
    let (code, stdout, stderr) = texts(agy(&f).args(["agy-y", "--json"]));
    assert_eq!(code, 1, "{stdout}\n{stderr}");
    assert_eq!(stdout, "");
    assert!(stderr.contains("thoth profile login agy-x"), "{stderr}");
    assert_eq!(harness_of(&f, "agy-y"), None);
}

#[test]
fn infer_a_given_harness_wins_over_the_name() {
    let f = common::fixture();
    let (code, v) = run(login(&f).args(["claude-x", "--harness", "codex", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["harness"], "codex");
    assert_eq!(v["provider"], "openai");
    assert_eq!(harness_of(&f, "claude-x").as_deref(), Some("codex"));
}

#[test]
fn infer_other_names_still_need_harness() {
    let f = common::fixture();
    for name in ["work-x", "claude-", "Claude-x", "codex", "agy-"] {
        let (code, stdout, stderr) = texts(login(&f).args([name, "--json"]));
        assert_eq!(code, 1, "{name}: {stdout}\n{stderr}");
        assert_eq!(stdout, "", "{name}");
        assert!(stderr.contains("--harness"), "{name}: {stderr}");
        assert!(stderr.contains("is not a registered profile yet"), "{name}: {stderr}");
    }
    assert!(profiles(&f).is_empty(), "{:?}", profiles(&f));
}
