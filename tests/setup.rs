//! `thoth setup` (SETUP-3). Every run uses the fake CLIs, a temporary
//! THOTH_HOME, a temporary repo and a temporary USERPROFILE / HOME; nothing
//! here reaches a real claude, codex or agy, the network, or the operator's
//! own ~/.gemini.

mod common;

use serde_json::{json, Value};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The shell-detection variables, cleared so each test decides them (the
/// same list as tests/doctor.rs).
const SHELL_VARS: [&str; 7] = [
    "MSYSTEM",
    "SHLVL",
    "PROMPT",
    "PSModulePath",
    "PSExecutionPolicyPreference",
    "SHELL",
    "THOTH_LANG",
];

fn userhome(f: &common::Fixture) -> PathBuf {
    f.home_path().join("userhome")
}

fn plugins_file(f: &common::Fixture) -> PathBuf {
    f.home_path().join("plugins.json")
}

fn args_log(f: &common::Fixture) -> PathBuf {
    f.home_path().join("claude-args.jsonl")
}

/// `f.cmd()` with setup's test environment, without a subcommand.
fn setup_env(f: &common::Fixture) -> assert_cmd::Command {
    let uh = userhome(f);
    std::fs::create_dir_all(&uh).unwrap();
    let mut c = f.cmd();
    c.env("THOTH_LOGIN_SKIP_TTY_CHECK", "1")
        .env("FAKE_LOGIN_BY_FILE", "1")
        .env("USERPROFILE", &uh)
        .env("HOME", &uh)
        .env("FAKE_CLAUDE_PLUGINS_FILE", plugins_file(f))
        .env("FAKE_CLAUDE_ARGS_LOG", args_log(f));
    for v in SHELL_VARS {
        c.env_remove(v);
    }
    c.env_remove("FAKE_LOGIN_FAIL").env_remove("FAKE_PLUGIN_FAIL");
    c
}

fn setup(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = setup_env(f);
    c.arg("setup");
    c
}

fn run(mut c: assert_cmd::Command, answers: &str) -> (i32, String, String) {
    let out = c.write_stdin(answers.to_string()).output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn prompts(stdout: &str) -> usize {
    stdout.matches("[Y/n]").count() + stdout.matches("[y/N]").count()
}

#[track_caller]
fn in_order(stdout: &str, parts: &[&str]) {
    let mut rest = stdout;
    for p in parts {
        match rest.find(p) {
            Some(i) => rest = &rest[i + p.len()..],
            None => panic!("{p:?} missing or out of order in:\n{stdout}"),
        }
    }
}

fn tree(dir: &Path) -> Vec<String> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        for e in std::fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            out.push(p.strip_prefix(root).unwrap().display().to_string());
            if p.is_dir() {
                walk(root, &p, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The directory of the cargo running these tests, first on PATH, so every
/// thoth below resolves the same cargo and a check's pin stays the same.
fn cargo_path() -> OsString {
    let dir = Path::new(env!("CARGO")).parent().unwrap().to_path_buf();
    let mut dirs = vec![dir];
    if let Some(p) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(dirs).unwrap()
}

fn ok_json(mut c: assert_cmd::Command) -> Value {
    let out = c.output().unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

fn init(f: &common::Fixture) {
    f.cmd().arg("init").assert().success();
}

/// `checks list --json` as check id -> row.
fn checks(f: &common::Fixture) -> std::collections::BTreeMap<String, Value> {
    let mut c = f.cmd();
    c.env("PATH", cargo_path()).args(["checks", "list", "--json"]);
    ok_json(c)["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["check_id"].as_str().unwrap().to_string(), r.clone()))
        .collect()
}

fn check_status(f: &common::Fixture, id: &str) -> String {
    checks(f)[id]["status"].as_str().unwrap().to_string()
}

fn add_check(f: &common::Fixture, id: &str, cmd: &str) {
    f.cmd().env("PATH", cargo_path()).args(["checks", "add", id, "--cmd", cmd]).assert().success();
}

fn log_args(f: &common::Fixture) -> Vec<Value> {
    match std::fs::read_to_string(args_log(f)) {
        Ok(text) => text
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap()["args"].clone())
            .collect(),
        Err(_) => Vec::new(),
    }
}

fn write_plugins(f: &common::Fixture, version: &str, enabled: bool) {
    let v = json!([{"id":"thoth@thoth","version":version,"scope":"user","enabled":enabled,"installPath":"x"}]);
    std::fs::write(plugins_file(f), v.to_string()).unwrap();
}

fn read_plugins(f: &common::Fixture) -> Value {
    serde_json::from_str(&std::fs::read_to_string(plugins_file(f)).unwrap()).unwrap()
}

fn canonical_repo(f: &common::Fixture) -> String {
    let s = std::fs::canonicalize(f.repo.path()).unwrap().display().to_string();
    s.strip_prefix(r"\\?\").map(str::to_string).unwrap_or(s)
}

/// Makes the fixture repo look like Thoth's own (a local marketplace).
fn make_local_marketplace(f: &common::Fixture) {
    let r = f.repo.path();
    std::fs::create_dir_all(r.join(".claude-plugin")).unwrap();
    std::fs::write(
        r.join(".claude-plugin").join("marketplace.json"),
        r#"{"name":"thoth","owner":{"name":"t"},"plugins":[]}"#,
    )
    .unwrap();
    let manifest_dir = r.join("plugins").join("thoth").join(".claude-plugin");
    std::fs::create_dir_all(&manifest_dir).unwrap();
    std::fs::write(manifest_dir.join("plugin.json"), r#"{"name":"thoth","version":"0.1.0"}"#).unwrap();
}

const YES12: &str = "y\ny\ny\ny\ny\ny\ny\ny\ny\ny\ny\ny\n";
const NO12: &str = "n\nn\nn\nn\nn\nn\nn\nn\nn\nn\nn\nn\n";

/// AC16 (a) and (b): from nothing to all set, then a rerun that asks and
/// writes nothing.
#[test]
fn from_zero_all_yes_then_rerun_asks_nothing() {
    let f = common::fixture();
    std::fs::write(f.repo.path().join("Cargo.toml"), "[package]\nname = \"x\"\nversion = \"0.0.0\"\n").unwrap();
    let home = f.home_path();
    assert!(!home.join("global.db").exists());
    assert!(!plugins_file(&f).exists());

    let mut c = setup(&f);
    c.env("PATH", cargo_path());
    let (code, out, err) = run(c, YES12);
    assert_eq!(code, 0, "{out}\n{err}");
    in_order(
        &out,
        &[
            "Add an account for Claude",
            "Add an account for Codex",
            "Add an account for agy",
            "Register this repository",
            "test: cargo test",
            "clippy: cargo clippy --all-targets -- -D warnings",
            "Add these 2 check(s)",
            "clippy  pending",
            "test  pending",
            "Approve these 2 check(s)",
            "Turn it on?",
            "Install the Thoth plugin",
            "All set",
            "Restart Claude Code",
        ],
    );
    assert_eq!(prompts(&out), 8, "{out}");
    // an empty catalogue with proposals: doctor's hint is not printed as well
    assert!(!out.contains("Add a check, for example"), "{out}");
    // AC16 (o) 3: the skipped terminal check is said on stderr.
    assert!(err.contains("terminal check skipped"), "{err}");

    let mut ids: Vec<String> = ok_json({
        let mut c = f.cmd();
        c.args(["profile", "list", "--json"]);
        c
    })["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["profile_id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, ["agy-a", "claude-a", "codex-a"]);

    assert!(f.state_db().exists());
    assert_eq!(check_status(&f, "test"), "approved");
    assert_eq!(check_status(&f, "clippy"), "approved");
    // the checks setup proposes pin their program by its bytes
    for (id, row) in checks(&f) {
        assert_eq!(row["exe_pin"], "hash", "{id}");
    }

    let config: Value = serde_json::from_str(&std::fs::read_to_string(home.join("config.json")).unwrap()).unwrap();
    assert_eq!(config["live_quota"], true);

    let plugins = read_plugins(&f);
    assert_eq!(plugins[0]["id"], "thoth@thoth");

    let logged = log_args(&f);
    let add = logged
        .iter()
        .position(|a| *a == json!(["plugin", "marketplace", "add", "johnny-lo/Thoth"]))
        .expect("marketplace add logged");
    let install = logged
        .iter()
        .position(|a| *a == json!(["plugin", "install", "thoth@thoth"]))
        .expect("install logged");
    assert!(add < install);
    let n = logged.len();
    let without_wal = |t: Vec<String>| -> Vec<String> {
        t.into_iter().filter(|p| !p.ends_with("-wal") && !p.ends_with("-shm")).collect()
    };
    let t = without_wal(tree(&home));

    // (b) The rerun.
    let mut c = setup(&f);
    c.env("PATH", cargo_path());
    let (code, out, err) = run(c, "");
    assert_eq!(code, 0, "{out}\n{err}");
    assert_eq!(prompts(&out), 0, "{out}");
    assert!(out.contains("All set"), "{out}");
    assert!(!out.contains('✗'), "{out}");
    assert!(!out.contains("Restart Claude Code"), "{out}");
    assert_eq!(log_args(&f).len(), n);
    assert_eq!(without_wal(tree(&home)), t);
}

/// Init plus a pending check `t1`, for the refusal tests.
fn refusal_base(f: &common::Fixture) {
    init(f);
    add_check(f, "t1", "git --version");
}

fn assert_nothing_written(f: &common::Fixture) {
    assert_eq!(check_status(f, "t1"), "pending");
    let home = f.home_path();
    assert!(!home.join("profiles").exists());
    assert!(!home.join("config.json").exists());
    assert!(!plugins_file(f).exists());
    assert!(!args_log(f).exists());
}

/// AC16 (c): the deliberately wrong case, a yes on stdin without a terminal.
#[test]
fn refuses_without_a_terminal_even_with_yes_on_stdin() {
    let f = common::fixture();
    refusal_base(&f);

    let mut c = setup(&f);
    c.env_remove("THOTH_LOGIN_SKIP_TTY_CHECK");
    let (code, out, err) = run(c, YES12);
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("terminal") && err.contains("thoth setup"), "{err}");
    assert_nothing_written(&f);

    let mut c = setup(&f);
    c.env_remove("THOTH_LOGIN_SKIP_TTY_CHECK").env("THOTH_LANG", "zh-TW");
    let (code, out, err) = run(c, YES12);
    assert_eq!(code, 1);
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("請在終端機打 thoth setup"), "{err}");

    // Only `1` skips the check.
    let mut c = setup(&f);
    c.env("THOTH_LOGIN_SKIP_TTY_CHECK", "true");
    let (code, out, err) = run(c, YES12);
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(out.is_empty(), "{out}");
    assert_nothing_written(&f);
}

/// AC16 (d).
#[test]
fn refuses_inside_a_run_even_with_the_skip_variable() {
    let f = common::fixture();
    refusal_base(&f);
    let mut c = setup(&f);
    c.env("THOTH_RUN_ID", "01RUN");
    let (code, out, err) = run(c, YES12);
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(out.is_empty(), "{out}");
    assert!(err.contains("THOTH_RUN_ID"), "{err}");
    assert_nothing_written(&f);
}

/// AC16 (e): a JSON-RPC exchange of its own, not tests/mcp.rs's helper.
#[test]
fn mcp_has_no_setup_tool() {
    let f = common::fixture();
    let input = [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}),
    ]
    .iter()
    .map(|v| format!("{v}\n"))
    .collect::<String>();
    let out = f.cmd().arg("mcp").write_stdin(input).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let list = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["id"] == 2)
        .unwrap_or_else(|| panic!("no tools/list answer in {stdout}"));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 35);
    for t in tools {
        assert!(!t["name"].as_str().unwrap().contains("setup"), "{t}");
    }
}

/// AC16 (f).
#[test]
fn no_answer_means_no_and_changes_nothing() {
    for answers in ["", NO12] {
        let f = common::fixture();
        let c = setup(&f);
        let before = tree(&f.home_path());
        let (code, out, err) = run(c, answers);
        assert_eq!(code, 1, "{out}\n{err}");
        assert!(out.contains("still need attention"), "{out}");
        assert!(prompts(&out) >= 3, "{out}");
        assert_eq!(tree(&f.home_path()), before, "{answers:?}");
        assert!(!f.home_path().join("global.db").exists());
    }
}

fn logged_in(f: &common::Fixture, id: &str) -> (i32, bool) {
    let out = f
        .cmd()
        .env("FAKE_LOGIN_BY_FILE", "1")
        .args(["profile", "check", id, "--json"])
        .output()
        .unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    (out.status.code().unwrap(), v["logged_in"].as_bool().unwrap())
}

/// AC16 (g).
#[test]
fn logged_out_profile_login_and_retry() {
    let f = common::fixture();
    init(&f);
    let cfg = f.add_profile("claude-a");

    let mut c = setup(&f);
    c.env("FAKE_LOGIN_FAIL", "1");
    let (code, out, err) = run(c, "y\nn\nn\nn\nn\nn\nn\n");
    assert_eq!(code, 1, "{out}\n{err}");
    in_order(&out, &["Log in to claude-a now?", "Try again?"]);
    assert_eq!(logged_in(&f, "claude-a"), (1, false));

    let (_, out, err) = run(setup(&f), "y\nn\nn\nn\nn\nn\n");
    assert!(out.contains("Log in to claude-a now?"), "{out}\n{err}");
    assert!(!out.contains("Try again?"), "{out}");
    assert_eq!(logged_in(&f, "claude-a"), (0, true));

    let (_, out, _) = run(setup(&f), "");
    assert!(!out.contains("Log in to claude-a"), "{out}");

    // A profile that is not active is shown, never asked about.
    f.global_conn()
        .execute("UPDATE profiles SET status = 'rate_limited' WHERE profile_id = 'claude-a'", [])
        .unwrap();
    std::fs::remove_file(cfg.join("fake-login.ok")).unwrap();
    let (_, out, _) = run(setup(&f), "");
    assert!(out.contains("Profile claude-a"), "{out}");
    assert!(!out.contains("Log in to claude-a"), "{out}");
}

/// AC16 (h).
#[test]
fn pending_checks_are_listed_then_approved() {
    let f = common::fixture();
    init(&f);
    add_check(&f, "t1", "git --version");
    add_check(&f, "t2", "git status");
    let (code, out, err) = run(setup(&f), "n\nn\nn\ny\nn\nn\n");
    in_order(
        &out,
        &["t1  pending  [argv] git --version", "t2  pending  [argv] git status", "Approve these 2 check(s)"],
    );
    let _ = (code, err);
    assert_eq!(check_status(&f, "t1"), "approved");
    assert_eq!(check_status(&f, "t2"), "approved");

    // A pending check whose program is not on PATH: nothing is approved.
    let f = common::fixture();
    missing_program_base(&f);
    let (_, out, _) = run(setup(&f), "");
    assert!(out.contains("nosuchexe-zz"), "{out}");
    assert!(out.contains("is not on PATH"), "{out}");
    assert!(out.contains("then run thoth setup again"), "{out}");
    assert!(!out.contains("Approve these"), "{out}");
    assert!(!out.contains("rerun thoth doctor"), "{out}");
    assert_eq!(check_status(&f, "t3"), "pending");
    assert_eq!(check_status(&f, "t1"), "pending");
}

fn missing_program_base(f: &common::Fixture) {
    init(f);
    add_check(f, "t1", "git --version");
    add_check(f, "t3", "nosuchexe-zz a");
}

/// AC16 (i).
#[test]
fn detection_skips_existing_ids() {
    let f = common::fixture();
    std::fs::write(f.repo.path().join("Cargo.toml"), "[package]\nname = \"x\"\nversion = \"0.0.0\"\n").unwrap();
    init(&f);
    add_check(&f, "test", "cargo test");
    f.cmd().env("PATH", cargo_path()).args(["checks", "approve", "--reason", "test approval"]).assert().success();
    let approved_at = checks(&f)["test"]["approved_at"].clone();

    let mut c = setup(&f);
    c.env("PATH", cargo_path());
    let (_, out, err) = run(c, "n\nn\nn\ny\ny\nn\nn\n");
    assert!(out.contains("clippy: cargo clippy"), "{out}\n{err}");
    assert!(!out.contains("test: cargo test"), "{out}");
    let now = checks(&f);
    assert_eq!(now["clippy"]["status"], "approved");
    assert_eq!(now["test"]["status"], "approved");
    assert_eq!(now["test"]["approved_at"], approved_at);
}

/// AC16 (j).
#[test]
fn plugin_enable_update_fail_and_local_source() {
    let answers = "n\nn\nn\nn\ny\n";
    let f = common::fixture();
    init(&f);

    // 1. Disabled -> enable.
    write_plugins(&f, VERSION, false);
    let (_, out, err) = run(setup(&f), answers);
    assert!(out.contains("Enable it now?"), "{out}\n{err}");
    assert!(log_args(&f).contains(&json!(["plugin", "enable", "thoth@thoth"])));
    assert_eq!(read_plugins(&f)[0]["enabled"], true);

    // 2. Another version -> update.
    write_plugins(&f, "0.0.1", true);
    let (_, out, _) = run(setup(&f), answers);
    assert!(out.contains("Update it"), "{out}");
    assert!(log_args(&f).contains(&json!(["plugin", "update", "thoth@thoth"])));
    assert_eq!(read_plugins(&f)[0]["version"], VERSION);

    // 2b. The update leaves another version still.
    write_plugins(&f, "0.0.1", true);
    let mut c = setup(&f);
    c.env("FAKE_CLAUDE_PLUGIN_VERSION", "0.0.2");
    let (code, out, _) = run(c, answers);
    assert!(out.contains("Update it now"), "{out}");
    assert!(out.contains("is still 0.0.2"), "{out}");
    assert_eq!(code, 1);

    // 3. Not installed, and the install fails.
    std::fs::remove_file(plugins_file(&f)).unwrap();
    let mut c = setup(&f);
    c.env("FAKE_PLUGIN_FAIL", "1");
    let (code, out, _) = run(c, answers);
    assert!(out.contains("Install the Thoth plugin"), "{out}");
    assert!(out.contains("fake-claude: plugin command failed"), "{out}");
    assert!(out.contains("claude plugin install thoth@thoth"), "{out}");
    assert_eq!(code, 1);

    // 4. Thoth's own repository installs from the local folder.
    let _ = std::fs::remove_file(plugins_file(&f));
    std::fs::remove_file(args_log(&f)).unwrap();
    make_local_marketplace(&f);
    let (_, out, err) = run(setup(&f), answers);
    let logged = log_args(&f);
    assert_eq!(logged.len(), 2, "{logged:?}\n{out}\n{err}");
    assert_eq!(logged[0], json!(["plugin", "marketplace", "add", canonical_repo(&f)]));
    assert!(!std::fs::read_to_string(args_log(&f)).unwrap().contains("johnny-lo"));

    // 5. The install brings another version: not asked to update in the same run.
    std::fs::remove_file(plugins_file(&f)).unwrap();
    std::fs::remove_file(args_log(&f)).unwrap();
    let mut c = setup(&f);
    c.env("FAKE_CLAUDE_PLUGIN_VERSION", "0.0.2");
    let (_, out, _) = run(c, answers);
    assert!(out.contains("Install the Thoth plugin"), "{out}");
    assert!(!out.contains("Update it now"), "{out}");
    assert_eq!(prompts(&out), 5, "{out}");
    assert!(!log_args(&f).iter().any(|a| a[1] == "update"));
    assert!(out.contains("Restart Claude Code"), "{out}");
}

/// AC16 (k).
#[test]
fn live_quota_switch() {
    let f = common::fixture();
    init(&f);
    let (_, out, _) = run(setup(&f), "n\nn\nn\ny\nn\n");
    assert!(out.contains("Turn it on?"), "{out}");
    let v = ok_json({
        let mut c = f.cmd();
        c.args(["config", "get", "live-quota", "--json"]);
        c
    });
    assert_eq!(v["value"], "on");

    let (_, out, _) = run(setup(&f), "");
    assert!(!out.contains("Turn it on?"), "{out}");
    assert!(out.contains("live quota: on"), "{out}");

    std::fs::write(f.home_path().join("config.json"), "broken").unwrap();
    let (_, out, _) = run(setup(&f), "");
    assert!(!out.contains("Turn it on?"), "{out}");
    assert!(out.contains("Fix or delete this file"), "{out}");
}

/// AC16 (l).
#[test]
fn zh_tw_and_shell_flag() {
    let f = common::fixture();
    missing_program_base(&f);
    let mut c = setup(&f);
    c.env("THOTH_LANG", "zh-TW").args(["--shell", "cmd"]);
    let (_, out, _) = run(c, "");
    assert!(out.contains("Thoth 狀態目錄"), "{out}");
    assert!(out.contains("set PATH="), "{out}");
    assert!(!out.contains("state directory"), "{out}");

    let mut c = setup(&f);
    c.args(["--shell", "bash"]);
    let (_, out, _) = run(c, "");
    assert!(out.contains("export PATH=\""), "{out}");
}

/// A thoth with only the old `~/.agentctl` in a fresh home.
fn old_dir_only_cmd(f: &common::Fixture, home: &Path) -> assert_cmd::Command {
    std::fs::create_dir(home.join(".agentctl")).unwrap();
    let mut c = assert_cmd::Command::new(env!("CARGO_BIN_EXE_thoth"));
    c.current_dir(f.repo.path())
        .env("USERPROFILE", home)
        .env("HOME", home)
        .env_remove("THOTH_HOME")
        .env_remove("AGENTCTL_HOME")
        .env_remove("THOTH_RUN_ID")
        .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
        .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
        .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"));
    for v in SHELL_VARS {
        c.env_remove(v);
    }
    c
}

fn item(doc: &Value, id: &str) -> Value {
    doc["items"].as_array().unwrap().iter().find(|i| i["id"] == id).unwrap().clone()
}

fn doctor_json(mut c: assert_cmd::Command) -> Value {
    let out = c.output().unwrap();
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)))
}

/// AC16 (m).
#[test]
fn doctor_small_fixes() {
    // 1. The old directory's detail names the default location.
    let f = common::fixture();
    let home = tempfile::tempdir().unwrap();
    let mut c = old_dir_only_cmd(&f, home.path());
    c.args(["doctor", "--json"]);
    let doc = doctor_json(c);
    let detail = item(&doc, "state_dir")["detail"].as_str().unwrap().to_string();
    assert!(detail.contains("(default location)"), "{detail}");

    // 2. A missing program found in ~/.cargo/bin: the fix names that folder.
    let f = common::fixture();
    let bin = userhome(&f).join(".cargo").join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    #[cfg(windows)]
    std::fs::write(bin.join("nosuchexe-zz.exe"), "").unwrap();
    #[cfg(not(windows))]
    {
        use std::os::unix::fs::PermissionsExt;
        let p = bin.join("nosuchexe-zz");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    init(&f);
    add_check(&f, "t3", "nosuchexe-zz a");
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    let d = bin.display().to_string();
    let mut c = setup_env(&f);
    #[cfg(windows)]
    c.args(["doctor", "--json", "--shell", "powershell"]);
    #[cfg(not(windows))]
    c.args(["doctor", "--json", "--shell", "bash"]);
    let fix = item(&doctor_json(c), "checks")["fix"].as_str().unwrap().to_string();
    #[cfg(windows)]
    assert!(fix.contains(&format!("$env:Path = \"{d};$env:Path\"")), "{fix}");
    #[cfg(not(windows))]
    assert!(fix.contains(&format!("export PATH=\"{d}:$PATH\"")), "{fix}");
    assert!(!fix.contains("DIR"), "{fix}");

    // 3. The plugin's install command: local folder in Thoth's own repo.
    let f = common::fixture();
    make_local_marketplace(&f);
    let mut c = f.cmd();
    c.env("FAKE_CLAUDE_PLUGINS", "[]").env_remove("FAKE_CLAUDE_PLUGINS_FILE").args(["doctor", "--json"]);
    let fix = item(&doctor_json(c), "plugin")["fix"].as_str().unwrap().to_string();
    assert!(fix.contains(&canonical_repo(&f)), "{fix}");
    assert!(!fix.contains("johnny-lo"), "{fix}");

    let f = common::fixture();
    let mut c = f.cmd();
    c.env("FAKE_CLAUDE_PLUGINS", "[]").env_remove("FAKE_CLAUDE_PLUGINS_FILE").args(["doctor", "--json"]);
    let fix = item(&doctor_json(c), "plugin")["fix"].as_str().unwrap().to_string();
    assert!(fix.contains("claude plugin marketplace add johnny-lo/Thoth"), "{fix}");
}

/// AC16 (n).
#[test]
fn cli_surface() {
    let f = common::fixture();
    let out = f.cmd().arg("--help").output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("setup"));
    let out = setup(&f).arg("--json").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// AC16 (o) 1 and 2 ((o) 3 is in the first test).
#[test]
fn state_dir_stop_and_cli_rerun_text() {
    let f = common::fixture();
    let home = tempfile::tempdir().unwrap();
    let mut c = old_dir_only_cmd(&f, home.path());
    c.env("THOTH_LOGIN_SKIP_TTY_CHECK", "1").arg("setup");
    let (code, out, err) = run(c, YES12);
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(out.contains("Settle the state directory first"), "{out}");
    assert_eq!(prompts(&out), 0, "{out}");
    assert!(!home.path().join(".thoth").exists());

    let f = common::fixture();
    let missing = f.home_path().join("no-such-dir").join("agy-missing");
    let mut c = setup(&f);
    c.env("THOTH_AGY_BIN", &missing);
    let (_, out, _) = run(c, "");
    assert!(out.contains("Then run thoth setup again."), "{out}");
    assert!(!out.contains("Then rerun thoth doctor"), "{out}");
    assert!(!out.contains("Add an account for agy"), "{out}");
}

#[test]
fn a_failed_enable_lists_one_command_in_zh_tw() {
    let f = common::fixture();
    init(&f);
    write_plugins(&f, VERSION, false);
    let mut c = setup(&f);
    c.env("FAKE_PLUGIN_FAIL", "1").env("THOTH_LANG", "zh-TW");
    let (code, out, err) = run(c, "n\nn\nn\nn\ny\n");
    assert_eq!(code, 1, "{out}\n{err}");
    assert!(out.contains("請自己執行下面的指令："), "{out}");
    assert!(!out.contains("這兩行"), "{out}");
    let lines: Vec<&str> = out.lines().collect();
    let at = lines.iter().position(|l| l.contains("請自己執行下面的指令：")).expect("failure line");
    let arrows: Vec<&str> = lines[at + 1..].iter().copied().filter(|l| l.starts_with("  → ")).collect();
    assert_eq!(arrows, ["  → claude plugin enable thoth@thoth"], "{out}");
}

#[test]
fn an_empty_catalogue_without_proposals_keeps_the_doctor_hint() {
    let f = common::fixture();
    init(&f);
    let (_, out, _) = run(setup(&f), "");
    assert_eq!(out.matches("Add a check, for example").count(), 1, "{out}");
    assert!(!out.contains("Add these"), "{out}");
    assert!(!out.contains("Approve these"), "{out}");
}

#[test]
fn declining_enable_ends_the_plugin_step() {
    let f = common::fixture();
    init(&f);
    write_plugins(&f, "0.0.1", false);
    let _ = std::fs::remove_file(args_log(&f));
    let (_, out, err) = run(setup(&f), "n\nn\nn\nn\nn\n");
    assert!(out.contains("Enable it now?"), "{out}\n{err}");
    assert!(!out.contains("Update it now"), "{out}");
    assert_eq!(prompts(&out), 5, "{out}");
    assert!(!log_args(&f).iter().any(|a| a[1] == "enable" || a[1] == "update"), "{:?}", log_args(&f));
    assert_eq!(read_plugins(&f)[0]["enabled"], false);
    assert_eq!(read_plugins(&f)[0]["version"], "0.0.1");

    // the other way round: yes to enable, and the update is asked about
    write_plugins(&f, "0.0.1", false);
    let _ = std::fs::remove_file(args_log(&f));
    let mut c = setup(&f);
    c.env("FAKE_CLAUDE_PLUGIN_VERSION", "0.0.1");
    let (_, out, err) = run(c, "n\nn\nn\nn\ny\ny\n");
    assert!(out.contains("Enable it now?"), "{out}\n{err}");
    assert!(out.contains("Update it now"), "{out}");
    assert_eq!(prompts(&out), 6, "{out}");
    let logged = log_args(&f);
    let enable = logged.iter().position(|a| *a == json!(["plugin", "enable", "thoth@thoth"])).expect("enable logged");
    let update = logged.iter().position(|a| *a == json!(["plugin", "update", "thoth@thoth"])).expect("update logged");
    assert!(enable < update, "{logged:?}");
    assert_eq!(read_plugins(&f)[0]["enabled"], true);
    assert_eq!(read_plugins(&f)[0]["version"], "0.0.1");
    assert!(out.contains("is still 0.0.1"), "{out}");
}

#[test]
fn setup_lists_a_path_only_check_as_such() {
    let f = common::fixture();
    init(&f);
    f.cmd()
        .args(["checks", "add", "t", "--cmd", "git --version", "--exe-pin", "path"])
        .assert()
        .success();
    let (_, out, err) = run(setup(&f), "n\nn\nn\nn\nn\nn\n");
    assert!(out.contains("t  pending  [argv] git --version  (exe pinned by path only)"), "{out}\n{err}");
    assert!(out.contains("Approve these 1 check(s)"), "{out}");
    assert_eq!(checks(&f)["t"]["status"], "pending");
}

/// A folder `dir` holding the program `pinme`, a copy of fake-claude.
fn pinme_in(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    let to = dir.join(if cfg!(windows) { "pinme.exe" } else { "pinme" });
    std::fs::copy(env!("CARGO_BIN_EXE_fake-claude"), &to).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&to, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

fn path_with(dirs: &[&Path]) -> OsString {
    let mut all: Vec<PathBuf> = dirs.iter().map(|d| d.to_path_buf()).collect();
    if let Some(p) = std::env::var_os("PATH") {
        all.extend(std::env::split_paths(&p));
    }
    std::env::join_paths(all).unwrap()
}

#[test]
fn setup_does_not_approve_a_path_only_check_that_resolves_inside_the_repo() {
    let f = common::fixture();
    init(&f);
    let home_bin = f.home_path().join("bin");
    pinme_in(&home_bin);
    let repo_bin = f.repo_path().join("bin");
    pinme_in(&repo_bin);
    f.cmd()
        .env("PATH", path_with(&[&home_bin]))
        .args(["checks", "add", "t", "--cmd", "pinme --version", "--exe-pin", "path"])
        .assert()
        .success();
    let mut c = setup(&f);
    c.env("PATH", path_with(&[&repo_bin, &home_bin]));
    let (_, out, err) = run(c, "n\nn\nn\ny\nn\nn\n");
    assert!(out.contains("Approve these 1 check(s)"), "{out}\n{err}");
    assert!(out.contains("t was not approved:"), "{out}");
    assert!(out.contains("inside the repository tree"), "{out}");
    assert_eq!(checks(&f)["t"]["status"], "pending");
}
