//! `thoth profile login` (SETUP-2). Every run uses the fake CLIs and a
//! temporary THOTH_HOME; nothing here reaches a real claude, codex or agy, or
//! the network. `THOTH_LOGIN_SKIP_TTY_CHECK=1` stands in for a terminal,
//! because assert_cmd's stdio is piped.

mod common;

use serde_json::Value;
use std::path::{Path, PathBuf};

/// `profile login` as a test runs it: no terminal check, the fakes report
/// logged in only after their login wrote their file, English.
fn login(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("THOTH_LOGIN_SKIP_TTY_CHECK", "1")
        .env("FAKE_LOGIN_BY_FILE", "1")
        .env_remove("THOTH_LANG")
        .env_remove("FAKE_LOGIN_FAIL")
        .args(["profile", "login"]);
    c
}

/// Exit code and the whole stdout as one JSON object.
fn run(c: &mut assert_cmd::Command) -> (i32, Value) {
    let out = c.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let v: Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {stdout}\nstderr: {}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap(), v)
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

/// The stderr of a refused `--json` run, parsed as one JSON object.
fn err_json(stderr: &str) -> Value {
    serde_json::from_str(stderr.trim())
        .unwrap_or_else(|e| panic!("stderr is not JSON ({e}): {stderr}"))
}

fn profiles(f: &common::Fixture) -> Vec<Value> {
    let out = f.cmd().args(["profile", "list", "--json"]).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["profiles"].as_array().unwrap().clone()
}

fn ids(f: &common::Fixture) -> Vec<String> {
    profiles(f).iter().map(|p| p["profile_id"].as_str().unwrap().to_string()).collect()
}

fn canon(p: &Path) -> String {
    let s = std::fs::canonicalize(p).unwrap().to_string_lossy().to_string();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

fn log_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn quota_events(f: &common::Fixture, id: &str, kind: Option<&str>) -> i64 {
    let conn = f.global_conn();
    match kind {
        Some(k) => conn
            .query_row(
                "SELECT COUNT(*) FROM quota_events WHERE profile_id = ?1 AND kind = ?2",
                [id, k],
                |r| r.get(0),
            )
            .unwrap(),
        None => conn
            .query_row("SELECT COUNT(*) FROM quota_events", [], |r| r.get(0))
            .unwrap(),
    }
}

#[test]
fn new_claude_auto_named() {
    let f = common::fixture();
    let home = f.home_path();
    assert!(!home.join("profiles").exists());
    let log = home.join("claude-args.jsonl");
    let (code, v) = run(login(&f)
        .env("FAKE_CLAUDE_ARGS_LOG", &log)
        .args(["--harness", "claude-code", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["ok"], true);
    assert_eq!(v["profile_id"], "claude-a");
    assert_eq!(v["harness"], "claude-code");
    assert_eq!(v["provider"], "anthropic");
    assert_eq!(v["new"], true);
    assert_eq!(v["logged_in"], true);
    assert_eq!(v["registered"], true);
    assert_eq!(v["subscription_type"], "pro");
    assert_eq!(v["email"], "fake@example.com");
    assert_eq!(v["account_label"], "pro");
    assert_eq!(v["config_dir"], canon(&home.join("profiles").join("claude-a")));
    assert_eq!(v["cli_exit"], 0);
    assert!(home.join("profiles").join("claude-a").join("fake-login.ok").is_file());
    let lines = log_lines(&log);
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["args"], serde_json::json!([]));
    assert_eq!(lines[0]["claude_config_dir"], v["config_dir"]);
    let p = profiles(&f);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0]["profile_id"], "claude-a");
    assert_eq!(p[0]["account_label"], "pro");
    assert_eq!(quota_events(&f, "claude-a", Some("note")), 1);
}

#[test]
fn second_claude_gets_b_and_label_flag_wins() {
    let f = common::fixture();
    f.add_profile("claude-a");
    let (code, v) = run(login(&f).args(["--harness", "claude-code", "--label", "second", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["profile_id"], "claude-b");
    assert_eq!(v["account_label"], "second");
    assert!(f.home_path().join("profiles").join("claude-b").is_dir());
    f.add_profile("claude-c");
    let (code, v) = run(login(&f).args(["--harness", "claude-code", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["profile_id"], "claude-d");
    assert_eq!(ids(&f), ["claude-a", "claude-b", "claude-c", "claude-d"]);
}

#[test]
fn codex_creates_the_dir() {
    let f = common::fixture();
    let home = f.home_path();
    assert!(!home.join("profiles").join("codex-a").exists());
    let log = home.join("codex-args.jsonl");
    let (code, v) = run(login(&f)
        .env("FAKE_CODEX_ARGS_LOG", &log)
        .args(["--harness", "codex", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["profile_id"], "codex-a");
    assert_eq!(v["provider"], "openai");
    assert_eq!(v["subscription_type"], Value::Null);
    assert_eq!(v["account_label"], Value::Null);
    assert_eq!(v["email"], Value::Null);
    assert!(home.join("profiles").join("codex-a").join("fake-login.ok").is_file());
    let lines = log_lines(&log);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["args"], serde_json::json!(["login"]));
    assert_eq!(lines[1]["args"], serde_json::json!(["login", "status"]));
    for l in &lines {
        assert_eq!(l["codex_home"], v["config_dir"]);
    }
}

#[test]
fn agy_uses_the_fixed_dir_and_a_second_agy_is_refused() {
    let f = common::fixture();
    let userhome = tempfile::tempdir().unwrap();
    let uh = userhome.path();
    let agy = |f: &common::Fixture| {
        let mut c = login(f);
        c.env("USERPROFILE", uh).env("HOME", uh);
        c
    };
    let (code, v) = run(agy(&f).args(["--harness", "antigravity", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["profile_id"], "agy-a");
    assert_eq!(v["provider"], "google");
    assert_eq!(v["new"], true);
    let fixed = uh.join(".gemini").join("antigravity-cli");
    assert_eq!(v["config_dir"], canon(&fixed));
    assert!(fixed.join("fake-login.ok").is_file());
    assert!(!f.home_path().join("profiles").join("agy-a").exists());

    let (code, v) = run(agy(&f).args(["--harness", "antigravity", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["profile_id"], "agy-a");
    assert_eq!(v["new"], false);

    let (code, stdout, stderr) = texts(agy(&f).args(["agy-b", "--harness", "antigravity", "--json"]));
    assert_eq!(code, 1);
    assert_eq!(stdout, "");
    let e = err_json(&stderr);
    assert_eq!(e["ok"], false);
    let msg = e["error"].as_str().unwrap();
    assert!(msg.contains("agy-a") && msg.contains("thoth profile login agy-a"), "{msg}");
    assert_eq!(ids(&f), ["agy-a"]);
    assert!(!f.home_path().join("profiles").join("agy-b").exists());

    let (code, _, stderr) = texts(agy(&f)
        .env("THOTH_LANG", "zh-TW")
        .args(["agy-b", "--harness", "antigravity"]));
    assert_eq!(code, 1);
    assert!(stderr.contains("每個機器使用者只能有一個 agy 帳號"), "{stderr}");
}

#[test]
fn existing_id_keeps_its_config_dir() {
    let f = common::fixture();
    let cfg = f.add_codex_profile("codex-x");
    let log = f.home_path().join("codex-args.jsonl");
    let (code, v) = run(login(&f).env("FAKE_CODEX_ARGS_LOG", &log).args(["codex-x", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["new"], false);
    assert_eq!(v["harness"], "codex");
    assert_eq!(v["config_dir"], canon(&cfg));
    assert_eq!(v["registered"], true);
    let lines = log_lines(&log);
    assert_eq!(lines[0]["args"], serde_json::json!(["login"]));
    assert_eq!(lines[0]["codex_home"], canon(&cfg));
    let p = profiles(&f);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0]["profile_id"], "codex-x");
    assert_eq!(p[0]["account_label"], Value::Null);

    let (code, _, stderr) = texts(login(&f).args(["codex-x", "--harness", "claude-code", "--json"]));
    assert_eq!(code, 1);
    let msg = err_json(&stderr)["error"].as_str().unwrap().to_string();
    assert!(msg.contains("codex") && msg.contains("--harness"), "{msg}");

    let (code, v) = run(login(&f).args(["codex-x", "--label", "relabelled", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    let p = profiles(&f);
    assert_eq!(p[0]["profile_id"], "codex-x");
    assert_eq!(p[0]["account_label"], "relabelled");
    assert_eq!(p[0]["config_dir"], canon(&cfg));
}

/// Steps 1-4 of `not_logged_in_registers_nothing`.
fn fail_once(f: &common::Fixture) {
    let (code, v) = run(login(f)
        .env("FAKE_LOGIN_FAIL", "1")
        .args(["--harness", "claude-code", "--json"]));
    assert_eq!(code, 1, "{v:#}");
    assert_eq!(v["ok"], false);
    assert_eq!(v["profile_id"], "claude-a");
    assert_eq!(v["new"], true);
    assert_eq!(v["logged_in"], false);
    assert_eq!(v["registered"], false);
    assert_eq!(v["cli_exit"], 0);
    assert!(profiles(f).is_empty());
    let dir = f.home_path().join("profiles").join("claude-a");
    assert!(dir.is_dir());
    assert!(!dir.join("fake-login.ok").exists());
}

#[test]
fn not_logged_in_registers_nothing() {
    let f = common::fixture();
    fail_once(&f);
    let (code, stdout, _) = texts(login(&f)
        .env("FAKE_LOGIN_FAIL", "1")
        .args(["--harness", "claude-code"]));
    assert_eq!(code, 1);
    assert!(stdout.contains("is not logged in"), "{stdout}");
    assert!(stdout.contains("thoth profile login claude-a"), "{stdout}");
    assert_eq!(quota_events(&f, "", None), 0);
}

#[test]
fn rerun_after_a_failure_reuses_the_dir() {
    let f = common::fixture();
    fail_once(&f);
    let (code, v) = run(login(&f).args(["--harness", "claude-code", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["profile_id"], "claude-a");
    assert_eq!(v["new"], true);
    assert_eq!(v["config_dir"], canon(&f.home_path().join("profiles").join("claude-a")));
    assert_eq!(ids(&f), ["claude-a"]);

    // The same, rerun with the id given.
    let f = common::fixture();
    fail_once(&f);
    let (code, v) = run(login(&f).args(["claude-a", "--harness", "claude-code", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["profile_id"], "claude-a");
}

#[test]
fn refuses_without_a_tty() {
    let f = common::fixture();
    let home = f.home_path();
    let log = home.join("claude-args.jsonl");
    let (code, stdout, stderr) = texts(f
        .cmd()
        .env_remove("THOTH_LOGIN_SKIP_TTY_CHECK")
        .env("FAKE_CLAUDE_ARGS_LOG", &log)
        .args(["profile", "login", "--harness", "claude-code", "--json"]));
    assert_eq!(code, 1);
    assert_eq!(stdout, "");
    let e = err_json(&stderr);
    assert_eq!(e["ok"], false);
    let msg = e["error"].as_str().unwrap();
    assert!(msg.contains("terminal") && msg.contains("thoth profile login --harness claude-code"), "{msg}");
    assert!(!home.join("profiles").exists());
    assert!(!log.exists());
    assert!(!home.join("global.db").exists());

    let (code, _, stderr) = texts(f
        .cmd()
        .env_remove("THOTH_LOGIN_SKIP_TTY_CHECK")
        .env("THOTH_LANG", "zh-TW")
        .args(["profile", "login", "--harness", "claude-code"]));
    assert_eq!(code, 1);
    assert!(stderr.contains("需要真的終端機"), "{stderr}");
}

#[test]
fn refuses_inside_a_run() {
    let f = common::fixture();
    let home = f.home_path();
    let log = home.join("claude-args.jsonl");
    let (code, _, stderr) = texts(login(&f)
        .env("THOTH_RUN_ID", "01RUN")
        .env("FAKE_CLAUDE_ARGS_LOG", &log)
        .args(["--harness", "claude-code", "--json"]));
    assert_eq!(code, 1);
    let msg = err_json(&stderr)["error"].as_str().unwrap().to_string();
    assert!(msg.contains("THOTH_RUN_ID"), "{msg}");
    assert!(!home.join("profiles").exists());
    assert!(!home.join("global.db").exists());
    assert!(!log.exists());

    let (code, _, stderr) = texts(login(&f)
        .env("THOTH_RUN_ID", "01RUN")
        .env("THOTH_LANG", "zh-TW")
        .args(["--harness", "claude-code"]));
    assert_eq!(code, 1);
    assert!(stderr.contains("登入是人做的事"), "{stderr}");
}

#[test]
fn zh_tw_messages_and_json_is_language_independent() {
    let f = common::fixture();
    let (code, stdout, _) = texts(login(&f).env("THOTH_LANG", "zh-TW").args(["--harness", "claude-code"]));
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("正在為 claude-a 打開 claude"), "{stdout}");
    assert!(stdout.contains("已登入") && stdout.contains("已登記"), "{stdout}");
    assert!(!stdout.contains("logged in"), "{stdout}");

    let (code, stdout, _) = texts(login(&f).args(["claude-a"]));
    assert_eq!(code, 0, "{stdout}");
    assert!(stdout.contains("Opening claude for claude-a"), "{stdout}");
    assert!(stdout.contains("already registered"), "{stdout}");

    let zh = login(&f).env("THOTH_LANG", "zh-TW").args(["claude-a", "--json"]).output().unwrap();
    let en = login(&f).args(["claude-a", "--json"]).output().unwrap();
    assert_eq!(zh.status.code(), Some(0));
    assert_eq!(en.status.code(), Some(0));
    assert_eq!(zh.stdout, en.stdout);
    let zh_err = String::from_utf8_lossy(&zh.stderr);
    let en_err = String::from_utf8_lossy(&en.stderr);
    assert!(zh_err.contains("正在為"), "{zh_err}");
    assert!(en_err.contains("Opening"), "{en_err}");
}

#[test]
fn missing_or_bad_arguments() {
    let f = common::fixture();
    let (code, _, stderr) = texts(&mut login(&f));
    assert_eq!(code, 1);
    assert!(stderr.contains("--harness"), "{stderr}");

    let (code, _, stderr) = texts(login(&f).arg("newbie"));
    assert_eq!(code, 1);
    assert!(stderr.contains("newbie") && stderr.contains("--harness"), "{stderr}");

    let (code, _, _) = texts(login(&f).args(["--harness", "gemini"]));
    assert_eq!(code, 2);

    let (code, _, stderr) = texts(login(&f).args(["bad/id", "--harness", "claude-code", "--json"]));
    assert_eq!(code, 1);
    let msg = err_json(&stderr)["error"].as_str().unwrap().to_string();
    assert!(msg.contains("cannot be a profile id"), "{msg}");
    assert!(!f.home_path().join("profiles").exists());

    let (code, _, stderr) = texts(login(&f).args(["auto", "--harness", "claude-code"]));
    assert_eq!(code, 1);
    assert!(stderr.contains("reserved"), "{stderr}");

    let (code, v) = run(login(&f).args(["--harness", "codex", "--provider", "acme", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["provider"], "acme");
}

#[test]
fn cli_that_cannot_start() {
    let f = common::fixture();
    let missing: PathBuf = f.home_path().join("no-such-dir").join("claude-missing");
    let (code, _, stderr) = texts(login(&f)
        .env("THOTH_CLAUDE_BIN", &missing)
        .args(["--harness", "claude-code", "--json"]));
    assert_eq!(code, 1);
    let last = stderr.trim_end().lines().last().unwrap_or("");
    let e = err_json(last);
    assert_eq!(e["ok"], false);
    assert!(e["error"].as_str().unwrap().contains("could not run"), "{e}");
    assert!(profiles(&f).is_empty());
}

#[test]
fn doctor_profiles_fix_points_at_login() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let mut c = f.cmd();
    for v in ["MSYSTEM", "SHLVL", "PROMPT", "PSModulePath", "PSExecutionPolicyPreference", "SHELL", "THOTH_LANG"] {
        c.env_remove(v);
    }
    let (_, v) = run(c.args(["doctor", "--json"]));
    let item = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "profiles")
        .unwrap()
        .clone();
    assert_eq!(item["status"], "fail");
    let fix = item["fix"].as_str().unwrap();
    assert!(fix.contains("thoth profile login --harness"), "{fix}");
}

#[cfg(target_os = "linux")]
#[test]
fn ctrl_c_while_the_cli_is_open_does_not_kill_thoth() {
    use std::process::{Command, Stdio};
    let f = common::fixture();
    let home = f.home_path();
    let log = home.join("n-args.jsonl");
    let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
    // `f.cmd()`'s environment, by hand.
    for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE"] {
        c.env_remove(var);
    }
    c.env_remove("CLAUDE_CONFIG_DIR");
    c.env_remove("CODEX_HOME");
    c.env_remove("ARTIFICIALANALYSIS_API_KEY");
    c.env_remove("AISTUPIDLEVEL_API_KEY");
    c.env("THOTH_HOME", &home);
    c.env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"));
    c.env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"));
    c.env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"));
    c.current_dir(f.repo_path());
    // `login`'s additions.
    c.env("THOTH_LOGIN_SKIP_TTY_CHECK", "1")
        .env("FAKE_LOGIN_BY_FILE", "1")
        .env_remove("THOTH_LANG")
        .env_remove("FAKE_LOGIN_FAIL");
    c.env("FAKE_LOGIN_SLEEP_MS", "3000").env("FAKE_CLAUDE_ARGS_LOG", &log);
    c.args(["profile", "login", "--harness", "claude-code", "--json"]);
    c.stdout(Stdio::piped());
    let child = c.spawn().unwrap();

    let start = std::time::Instant::now();
    loop {
        let started = std::fs::read_to_string(&log).map(|s| s.lines().count() >= 1).unwrap_or(false);
        if started {
            break;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "fake-claude never started");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    // SAFETY: plain kill(2) on the thoth child this test spawned.
    let rc = unsafe { libc::kill(child.id() as i32, libc::SIGINT) };
    assert_eq!(rc, 0);

    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["profile_id"], "claude-a");
    assert_eq!(ids(&f), ["claude-a"]);
}

#[test]
fn an_id_that_differs_only_in_case_is_refused() {
    let f = common::fixture();
    f.add_profile("claude-a");
    let before = profiles(&f);

    let (code, stdout, stderr) = texts(login(&f).args(["Claude-A", "--harness", "claude-code", "--json"]));
    assert_eq!(code, 1, "{stderr}");
    let error = err_json(&stderr)["error"].as_str().unwrap().to_string();
    assert!(error.contains("claude-a") && error.contains("letter case"), "{error}");
    assert!(stdout.is_empty(), "{stdout}");

    assert_eq!(profiles(&f), before);
    assert!(!f.home_path().join("profiles").join("claude-a").join("fake-login.ok").exists());

    let (code, _stdout, stderr) =
        texts(login(&f).env("THOTH_LANG", "zh-TW").args(["Claude-A", "--harness", "claude-code"]));
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("大小寫"), "{stderr}");

    // a registered `Claude-B` makes `claude-b` taken for the automatic name
    f.add_profile("Claude-B");
    let (code, v) = run(login(&f).args(["--harness", "claude-code", "--json"]));
    assert_eq!(code, 0, "{v:#}");
    assert_eq!(v["profile_id"], "claude-c");
}
