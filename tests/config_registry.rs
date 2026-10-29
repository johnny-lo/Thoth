//! CFG-1: the settings registry. Every run uses a temporary THOTH_HOME, the
//! fake CLIs and a temporary repo; nothing here reaches a real CLI or the
//! network.

mod common;

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const FAKE_CLAUDE: &str = env!("CARGO_BIN_EXE_fake-claude");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// The shell-detection variables, cleared so each test decides them.
const SHELL_VARS: [&str; 7] = [
    "MSYSTEM",
    "SHLVL",
    "PROMPT",
    "PSModulePath",
    "PSExecutionPolicyPreference",
    "SHELL",
    "THOTH_LANG",
];

fn json_of(out: &std::process::Output) -> Value {
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout is not JSON ({e}): {}\nstderr: {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    })
}

fn stdout_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr_of(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn file_names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    names.sort();
    names
}

fn setting<'a>(v: &'a Value, key: &str) -> &'a Value {
    v["settings"].as_array().unwrap().iter().find(|s| s["key"] == key).unwrap()
}

fn config_json(f: &common::Fixture) -> std::path::PathBuf {
    f.home_path().join("config.json")
}

fn plugin_list() -> String {
    json!([{"id":"thoth@thoth","version":format!("{VERSION}-abc123"),"scope":"user","enabled":true,"installPath":"x"}])
        .to_string()
}

/// A `doctor` command with the fakes, a plugin of this version and a clean
/// shell environment.
fn doctor_cmd(f: &common::Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_CLAUDE_PLUGINS", plugin_list());
    for v in SHELL_VARS {
        c.env_remove(v);
    }
    c
}

/// `init`, profile `claude-a` and the approved check `t1`.
fn healthy_repo(f: &common::Fixture) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    f.cmd().args(["checks", "add", "t1", "--cmd", "git --version"]).assert().success();
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
}

fn item<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["items"].as_array().unwrap().iter().find(|i| i["id"] == id).unwrap()
}

#[test]
fn list_shows_every_setting_with_its_source_and_what_lives_elsewhere_without_writing() {
    let f = common::fixture();
    let before = file_names(&f.home_path());
    let out = f.cmd().env_remove("THOTH_LANG").args(["config", "list", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json_of(&out);
    let settings = v["settings"].as_array().unwrap();
    assert_eq!(settings.len(), 16);
    for (i, key) in ["live-quota", "scores-auto-refresh", "require-reason"]
        .iter()
        .enumerate()
    {
        assert_eq!(settings[i]["key"], *key);
        assert_eq!(settings[i]["value"], "off");
        assert_eq!(settings[i]["source"], "default");
        assert!(settings[i]["env"].is_null());
    }
    assert_eq!(settings[3]["key"], "language");
    assert_eq!(settings[3]["value"], "en");
    assert_eq!(settings[3]["source"], "default");
    let claude = setting(&v, "claude-bin");
    assert_eq!(claude["value"], FAKE_CLAUDE);
    assert_eq!(claude["source"], "env");
    assert_eq!(claude["env"], "THOTH_CLAUDE_BIN");
    assert_eq!(claude["settable"], true);
    assert_eq!(claude["kind"], "text");
    let home = setting(&v, "home");
    assert_eq!(home["value"], f.home_path().to_string_lossy().as_ref());
    assert_eq!(home["source"], "env");
    assert_eq!(home["settable"], false);
    assert_eq!(home["kind"], "path");
    let key = setting(&v, "aistupidlevel-api-key");
    assert_eq!(key["value"], "unset");
    assert_eq!(key["source"], "default");
    assert_eq!(key["kind"], "secret");
    assert_eq!(key["settable"], false);
    assert_eq!(v["config_file"]["exists"], false);
    assert_eq!(v["config_file"]["ok"], true);
    assert!(v["config_file"]["error"].is_null());
    assert_eq!(v["config_file"]["unknown_keys"], json!([]));
    assert!(v["elsewhere"]["profiles"]["count"].is_null());
    assert!(v["elsewhere"]["checks"].is_null());
    assert_eq!(before, file_names(&f.home_path()));

    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    f.cmd().args(["checks", "add", "t1", "--cmd", "git --version", "--shell", "argv"]).assert().success();
    f.cmd().args(["checks", "approve", "--reason", "test approval"]).assert().success();
    f.cmd().args(["checks", "protect", "*.lock"]).assert().success();
    let out = f.cmd().args(["config", "list", "--json"]).output().unwrap();
    let w = json_of(&out);
    assert_eq!(w["elsewhere"]["profiles"]["count"], 1);
    assert_eq!(w["elsewhere"]["profiles"]["command"], "thoth profile list");
    assert_eq!(w["elsewhere"]["checks"]["total"], 1);
    assert_eq!(w["elsewhere"]["checks"]["approved"], 1);
    assert_eq!(w["elsewhere"]["checks"]["protected_patterns"], 1);
    assert_eq!(w["elsewhere"]["checks"]["command"], "thoth checks list");

    let out = f.cmd().args(["config", "list"]).output().unwrap();
    let h = stdout_of(&out);
    assert!(h.contains("live-quota: off (default)"), "{h}");
    assert!(h.contains(&format!("claude-bin: {FAKE_CLAUDE} (env THOTH_CLAUDE_BIN)")), "{h}");
    assert!(h.contains("elsewhere:"), "{h}");
    assert!(h.contains("profiles: 1 registered (thoth profile list)"), "{h}");
    assert!(h.contains("checks: 1 (1 approved), protected patterns: 1 (thoth checks list)"), "{h}");
    assert_eq!(stderr_of(&out), "");

    let out = f.cmd().args(["config", "get", "home", "--json"]).output().unwrap();
    assert_eq!(json_of(&out)["value"], f.home_path().to_string_lossy().as_ref());
    let out = f
        .cmd()
        .env("AISTUPIDLEVEL_API_KEY", "sekrit")
        .args(["config", "get", "aistupidlevel-api-key", "--json"])
        .output()
        .unwrap();
    assert_eq!(json_of(&out)["value"], "set");
    assert!(!stdout_of(&out).contains("sekrit"));
    let out = f.cmd().args(["config", "get", "nope"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let e = stderr_of(&out);
    for word in ["require-reason", "live-quota", "language", "claude-bin"] {
        assert!(e.contains(word), "{e}");
    }
}

#[test]
fn set_and_unset_store_text_integer_and_language_keys_and_refuse_bad_values() {
    let f = common::fixture();
    let out = f
        .cmd()
        .env_remove("THOTH_LIVE_QUOTA_TIMEOUT_MS")
        .args(["config", "set", "live-quota-timeout-ms", "5000", "--json"])
        .output()
        .unwrap();
    let a = json_of(&out);
    assert_eq!(a["ok"], true);
    assert_eq!(a["value"], "5000");
    assert!(a["previous"].is_null());
    assert!(a["shadowed_by"].is_null());
    let stored: Value = serde_json::from_str(&fs::read_to_string(config_json(&f)).unwrap()).unwrap();
    assert_eq!(stored["live_quota_timeout_ms"], json!(5000));
    let out = f
        .cmd()
        .env_remove("THOTH_LIVE_QUOTA_TIMEOUT_MS")
        .args(["config", "get", "live-quota-timeout-ms", "--json"])
        .output()
        .unwrap();
    assert_eq!(json_of(&out), json!({"key":"live-quota-timeout-ms","value":"5000"}));
    let list = |env: Option<&str>| {
        let mut c = f.cmd();
        c.env_remove("THOTH_LIVE_QUOTA_TIMEOUT_MS");
        if let Some(v) = env {
            c.env("THOTH_LIVE_QUOTA_TIMEOUT_MS", v);
        }
        json_of(&c.args(["config", "list", "--json"]).output().unwrap())
    };
    assert_eq!(setting(&list(None), "live-quota-timeout-ms")["source"], "file");
    let w = list(Some("7000"));
    assert_eq!(setting(&w, "live-quota-timeout-ms")["value"], "7000");
    assert_eq!(setting(&w, "live-quota-timeout-ms")["source"], "env");

    let out = f.cmd().args(["config", "set", "scores-url", "https://example.test/x", "--json"]).output().unwrap();
    assert_eq!(json_of(&out)["ok"], true);

    let out = f.cmd().args(["config", "set", "claude-bin", "/tmp/x", "--json"]).output().unwrap();
    let b = json_of(&out);
    assert_eq!(b["shadowed_by"], "THOTH_CLAUDE_BIN");
    assert_eq!(stderr_of(&out), "");
    let out = f.cmd().args(["config", "set", "claude-bin", "/tmp/x"]).output().unwrap();
    assert!(stderr_of(&out).contains("note: THOTH_CLAUDE_BIN is set"), "{}", stderr_of(&out));
    let v = json_of(&f.cmd().args(["config", "list", "--json"]).output().unwrap());
    assert_eq!(setting(&v, "claude-bin")["value"], FAKE_CLAUDE);
    assert_eq!(setting(&v, "claude-bin")["source"], "env");
    let v = json_of(&f.cmd().env_remove("THOTH_CLAUDE_BIN").args(["config", "list", "--json"]).output().unwrap());
    assert_eq!(setting(&v, "claude-bin")["value"], "/tmp/x");
    assert_eq!(setting(&v, "claude-bin")["source"], "file");

    let out = f.cmd().env_remove("THOTH_CLAUDE_BIN").args(["config", "unset", "claude-bin", "--json"]).output().unwrap();
    let c = json_of(&out);
    assert_eq!(c["ok"], true);
    assert_eq!(c["previous"], "/tmp/x");
    assert_eq!(c["value"], "claude");
    assert_eq!(c["source"], "default");
    let out = f.cmd().env_remove("THOTH_CLAUDE_BIN").args(["config", "unset", "claude-bin", "--json"]).output().unwrap();
    assert!(json_of(&out)["previous"].is_null());
    let stored: Value = serde_json::from_str(&fs::read_to_string(config_json(&f)).unwrap()).unwrap();
    assert!(stored.get("claude_bin").is_none());
    assert!(stored.get("scores_url").is_some());
    assert!(stored.get("live_quota_timeout_ms").is_some());

    let out = f.cmd().args(["config", "unset", "scores-url"]).output().unwrap();
    let h = stdout_of(&out);
    assert!(h.contains("unset scores-url: now "), "{h}");
    assert!(h.contains(" (default)"), "{h}");

    let out = f.cmd().args(["config", "set", "language", "ZH-tw", "--json"]).output().unwrap();
    assert_eq!(json_of(&out)["ok"], true);
    let stored: Value = serde_json::from_str(&fs::read_to_string(config_json(&f)).unwrap()).unwrap();
    assert_eq!(stored["language"], "zh-TW");
    let out = f.cmd().env_remove("THOTH_LANG").args(["config", "get", "language", "--json"]).output().unwrap();
    assert_eq!(json_of(&out)["value"], "zh-TW");

    let before = fs::read_to_string(config_json(&f)).unwrap();
    // (arguments, text stderr must contain, extra environment variable)
    type Bad<'a> = (Vec<&'a str>, Option<&'a str>, Option<(&'a str, &'a str)>);
    let bad: Vec<Bad> = vec![
        (vec!["config", "set", "live-quota-timeout-ms", "0"], None, None),
        (vec!["config", "set", "live-quota-timeout-ms", "abc"], None, None),
        (vec!["config", "set", "claude-bin", "   "], None, None),
        (vec!["config", "set", "language", "fr"], None, None),
        (vec!["config", "set", "home", "/x"], Some("THOTH_HOME"), None),
        (vec!["config", "unset", "home"], None, None),
        (vec!["config", "set", "aistupidlevel-api-key", "k"], Some("AISTUPIDLEVEL_API_KEY"), None),
        (vec!["config", "unset", "artificialanalysis-api-key"], None, None),
        (vec!["config", "unset", "nope"], None, None),
        (vec!["config", "unset", "language"], Some("THOTH_RUN_ID"), Some(("THOTH_RUN_ID", "01RUN"))),
    ];
    for (args, needle, env) in bad {
        let mut cmd = f.cmd();
        if let Some((k, val)) = env {
            cmd.env(k, val);
        }
        let out = cmd.args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", stderr_of(&out));
        if let Some(n) = needle {
            assert!(stderr_of(&out).contains(n), "{args:?}: {}", stderr_of(&out));
        }
        assert_eq!(before, fs::read_to_string(config_json(&f)).unwrap(), "{args:?}");
    }

    let g = common::fixture();
    let out = g.cmd().args(["config", "unset", "language", "--json"]).output().unwrap();
    let u = json_of(&out);
    assert_eq!(u["ok"], true);
    assert!(u["previous"].is_null());
    assert!(!config_json(&g).exists());
}

#[test]
fn env_beats_file_beats_default_for_the_cli_paths_and_the_language() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    f.cmd().args(["config", "set", "claude-bin", FAKE_CLAUDE]).assert().success();
    let out = doctor_cmd(&f).env_remove("THOTH_CLAUDE_BIN").args(["doctor", "--json"]).output().unwrap();
    let v = json_of(&out);
    let cli = item(&v, "cli_claude");
    assert_eq!(cli["status"], "ok", "{v:#}");
    assert_eq!(cli["data"]["raw"], FAKE_CLAUDE);
    assert_eq!(cli["data"]["source"], "file");

    f.cmd().args(["config", "set", "claude-bin", "/definitely/missing/claude"]).assert().success();
    let out = doctor_cmd(&f).env_remove("THOTH_CLAUDE_BIN").args(["doctor", "--json"]).output().unwrap();
    let v = json_of(&out);
    assert_eq!(item(&v, "cli_claude")["status"], "fail", "{v:#}");
    assert_eq!(item(&v, "cli_claude")["data"]["source"], "file");

    let out = doctor_cmd(&f).args(["doctor", "--json"]).output().unwrap();
    let v = json_of(&out);
    assert_eq!(item(&v, "cli_claude")["status"], "ok", "{v:#}");
    assert_eq!(item(&v, "cli_claude")["data"]["source"], "env");

    f.cmd().args(["config", "set", "language", "zh-TW"]).assert().success();
    let out = doctor_cmd(&f).arg("doctor").output().unwrap();
    assert!(stdout_of(&out).contains("Thoth 狀態目錄"), "{}", stdout_of(&out));
    let out = doctor_cmd(&f).env("THOTH_LANG", "en").arg("doctor").output().unwrap();
    let h = stdout_of(&out);
    assert!(h.contains("Thoth state directory"), "{h}");
    assert!(!h.contains("Thoth 狀態目錄"), "{h}");
}

#[test]
fn a_broken_or_odd_config_file_is_named_by_list_and_doctor_and_warned_once_elsewhere() {
    let f = common::fixture();
    let path = config_json(&f);
    fs::write(&path, "broken").unwrap();
    let out = f.cmd().args(["config", "list", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let v = json_of(&out);
    assert_eq!(v["config_file"]["ok"], false);
    assert!(v["config_file"]["error"].as_str().is_some_and(|e| !e.is_empty()));
    for i in 0..3 {
        assert_eq!(v["settings"][i]["value"], "off");
        assert_eq!(v["settings"][i]["source"], "default");
    }
    assert_eq!(stderr_of(&out), "");
    let out = f.cmd().args(["config", "list"]).output().unwrap();
    assert!(
        stdout_of(&out).contains(&format!("warning: {} cannot be used (", path.display())),
        "{}",
        stdout_of(&out)
    );
    let out = f.cmd().args(["config", "get", "live-quota", "--json"]).output().unwrap();
    assert_eq!(json_of(&out), json!({"key":"live-quota","value":"off"}));
    let out = f.cmd().args(["config", "unset", "live-quota"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(fs::read_to_string(&path).unwrap(), "broken");

    healthy_repo(&f);
    fs::write(&path, "broken").unwrap();
    let out = doctor_cmd(&f).args(["doctor", "--json"]).output().unwrap();
    let d = json_of(&out);
    assert_eq!(item(&d, "config")["status"], "warn", "{d:#}");
    assert!(!stderr_of(&out).contains("warning:"), "{}", stderr_of(&out));

    let out = f.cmd().args(["recommend", "--role", "implementer", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let e = stderr_of(&out);
    let needle = format!("warning: {} cannot be used (", path.display());
    assert_eq!(e.matches(&needle).count(), 1, "{e}");

    fs::write(&path, r#"{"live_quota":true,"typo":1,"claude_bin":5}"#).unwrap();
    let out = f.cmd().args(["config", "list", "--json"]).output().unwrap();
    let w = json_of(&out);
    assert_eq!(setting(&w, "live-quota")["value"], "on");
    assert_eq!(setting(&w, "live-quota")["source"], "file");
    assert_eq!(w["config_file"]["ok"], true);
    assert_eq!(w["config_file"]["unknown_keys"], json!(["typo"]));
    assert_eq!(stderr_of(&out), "");
    let out = f.cmd().env_remove("THOTH_CLAUDE_BIN").args(["config", "list", "--json"]).output().unwrap();
    let w = json_of(&out);
    assert_eq!(setting(&w, "claude-bin")["value"], "claude");
    assert_eq!(setting(&w, "claude-bin")["source"], "default");
    let out = f.cmd().args(["config", "list"]).output().unwrap();
    assert!(
        stdout_of(&out).contains(&format!("warning: {} has unknown keys: typo (ignored)", path.display())),
        "{}",
        stdout_of(&out)
    );
}

#[test]
fn doctor_knows_every_stored_key_and_flags_only_unknown_ones() {
    let f = common::fixture();
    healthy_repo(&f);
    fs::write(
        config_json(&f),
        r#"{"require_reason":true,"scores_auto_refresh":true,"language":"zh-TW","claude_bin":"x","codex_bin":"x","agy_bin":"x","live_quota_timeout_ms":1,"scores_url":"u","aa_url":"u","same_provider_rotation":false,"live_quota":false}"#,
    )
    .unwrap();
    let out = doctor_cmd(&f).args(["doctor", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stdout_of(&out));
    let v = json_of(&out);
    let c = item(&v, "config");
    assert_eq!(c["status"], "ok", "{v:#}");
    assert_eq!(c["data"]["unknown_keys"], json!([]));
    // RJ-1: same_provider_rotation is retired: neither unknown nor stored
    let mut want = vec![
        "require_reason", "scores_auto_refresh", "language", "claude_bin", "codex_bin", "agy_bin",
        "live_quota_timeout_ms", "scores_url", "aa_url", "live_quota",
    ];
    want.sort_unstable();
    assert_eq!(c["data"]["stored_keys"], json!(want));
    assert!(c["data"].get("same_provider_rotation").is_none(), "{c}");
    assert!(c["detail"].as_str().unwrap().contains("10 known setting(s) stored"), "{c}");

    fs::write(config_json(&f), r#"{"require_reason":true,"typo":1}"#).unwrap();
    let out = doctor_cmd(&f).args(["doctor", "--json"]).output().unwrap();
    let v = json_of(&out);
    let c = item(&v, "config");
    assert_eq!(c["status"], "warn", "{v:#}");
    assert_eq!(c["data"]["unknown_keys"], json!(["typo"]));
    assert_eq!(c["data"]["stored_keys"], json!(["require_reason"]));
}

#[test]
fn set_previous_for_a_switch_stays_on_or_off() {
    let f = common::fixture();
    fs::write(config_json(&f), r#"{"live_quota":7}"#).unwrap();
    let out = f.cmd().args(["config", "set", "live-quota", "on", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr_of(&out));
    let v = json_of(&out);
    assert_eq!(v["previous"], "off", "{v}");
}

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &common::Fixture, extra_env: &[(&str, &str)]) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", FAKE_CLAUDE)
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("ARTIFICIALANALYSIS_API_KEY")
            .env_remove("AISTUPIDLEVEL_API_KEY")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in extra_env {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output }
    }

    fn send(&mut self, v: &Value) -> Value {
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        v
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_lists_settings_read_only_without_secret_values_and_has_no_set_tool() {
    let f = common::fixture();
    let mut s = Server::start(&f, &[("AISTUPIDLEVEL_API_KEY", "sekrit-value")]);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}));
    let list = s.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 36);
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    let at = names.iter().position(|n| *n == "thoth_config_list").unwrap();
    assert_eq!(names[at + 1], "thoth_doctor");
    let tool = &tools[at];
    assert_eq!(tool["annotations"]["readOnlyHint"], true);
    let props: Vec<&String> = tool["inputSchema"]["properties"].as_object().unwrap().keys().collect();
    assert_eq!(props, ["repo"]);
    for n in &names {
        assert!(!n.contains("config_set") && !n.contains("config_unset"), "{n}");
    }
    let res = s.call(3, "thoth_config_list", json!({}));
    assert_eq!(res["result"]["isError"], false, "{res}");
    let sc = &res["result"]["structuredContent"];
    assert_eq!(sc["settings"].as_array().unwrap().len(), 16);
    assert!(sc["elsewhere"].is_object());
    let key = sc["settings"].as_array().unwrap().iter().find(|x| x["key"] == "aistupidlevel-api-key").unwrap();
    assert_eq!(key["value"], "set");
    assert!(!res.to_string().contains("sekrit-value"));
}
