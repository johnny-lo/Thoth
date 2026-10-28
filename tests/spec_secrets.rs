//! RD-3 (RD-2, H14): `task create` and `task update-spec` refuse a spec or a
//! title that holds what looks like a secret, store nothing, and name only
//! kinds, fields and lines. Every fake secret is built by concatenation, so
//! no full key format appears in this file.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};

const ALNUM36: &str = "dummyDUMMY0123456789dummyDUMMY012345";

fn github_token() -> String {
    format!("{}{ALNUM36}", ["gh", "p_"].concat())
}

fn openai_key() -> String {
    format!("{}{}", ["s", "k-"].concat(), "dummyproj0123456789dummy")
}

/// `f.cmd()` without any variable of the test process whose name holds KEY,
/// TOKEN, SECRET or PASSWORD (only the names are read).
fn clean(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c
}

/// `text` holds no 8 consecutive characters of `secret` from `prefix_len` on.
fn no_piece(text: &str, secret: &str, prefix_len: usize) {
    let tail = &secret[prefix_len..];
    for i in 0..=tail.len().saturating_sub(8) {
        let piece = &tail[i..(i + 8).min(tail.len())];
        assert!(!text.contains(piece), "8 characters of a secret, from offset {}, are in the output", prefix_len + i);
    }
}

fn spec_file(f: &Fixture, text: &str) -> std::path::PathBuf {
    let path = f.home_path().join(format!("spec-{}.json", unique()));
    std::fs::write(&path, text).unwrap();
    path
}

/// A file name part that differs between calls.
fn unique() -> u128 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
        + N.fetch_add(1, Ordering::SeqCst) as u128
}

/// `task create -f FILE [--title T] --json` with `env` set.
fn create(f: &Fixture, spec_text: &str, title: Option<&str>, env: &[(&str, &str)]) -> Output {
    let path = spec_file(f, spec_text);
    let mut c = clean(f);
    c.args(["task", "create", "-f"]).arg(&path);
    if let Some(t) = title {
        c.args(["--title", t]);
    }
    c.arg("--json");
    for (k, v) in env {
        c.env(k, v);
    }
    c.output().unwrap()
}

fn json_stdout(f: &Fixture, args: &[&str]) -> Value {
    let mut c = clean(f);
    c.args(args);
    let out = c.output().unwrap();
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

fn task_count(f: &Fixture) -> usize {
    json_stdout(f, &["status", "--json"])["tasks"].as_array().unwrap().len()
}

fn event_count(f: &Fixture) -> usize {
    json_stdout(f, &["events", "--since", "0", "--json"])["events"].as_array().unwrap().len()
}

/// The `error` of the JSON document on the last stderr line.
fn error_of(out: &Output) -> String {
    let stderr = String::from_utf8_lossy(&out.stderr);
    let last = stderr.lines().rev().find(|l| !l.trim().is_empty()).expect("a stderr line");
    let v: Value = serde_json::from_str(last).expect("stderr ends in a JSON document");
    v["error"].as_str().unwrap().to_string()
}

fn init(f: &Fixture) {
    clean(f).arg("init").assert().success();
}

fn spec_with(goal: &str, acs: &[&str], constraints: &[&str]) -> Value {
    json!({"goal": goal, "acceptance_criteria": acs, "constraints": constraints,
        "non_goals": [], "excluded_alternatives": [], "risk": "normal"})
}

/// The pretty spec with a token in its second criterion, and the 1-based
/// line the token is on.
fn pretty_spec_with_token() -> (String, usize) {
    let token = github_token();
    let ac2 = format!("AC2: uses {token}");
    let text = serde_json::to_string_pretty(&spec_with("say hello", &["AC1: prints hello", &ac2], &[])).unwrap();
    let line = text.lines().position(|l| l.contains(&token)).unwrap() + 1;
    (text, line)
}

/// AC12 spec_secrets (t1).
#[test]
fn spec_secrets_create_refuses_a_format_secret_and_names_line_and_field() {
    let f = common::fixture();
    init(&f);
    let (text, line) = pretty_spec_with_token();
    let (tasks, events) = (task_count(&f), event_count(&f));

    let out = create(&f, &text, None, &[]);
    assert_eq!(out.status.code(), Some(1));
    let err = error_of(&out);
    assert!(err.starts_with("refusing: the spec contains what looks like a secret"), "{err}");
    assert!(err.contains("github-token in acceptance_criteria[1]"), "{err}");
    assert!(err.contains(&format!("; line {line})")), "{err}");
    no_piece(&String::from_utf8_lossy(&out.stderr), &github_token(), 4);
    no_piece(&String::from_utf8_lossy(&out.stdout), &github_token(), 4);
    assert_eq!(task_count(&f), tasks);
    assert_eq!(event_count(&f), events);

    let path = spec_file(&f, &text);
    let mut c = clean(&f);
    c.args(["task", "create", "-f"]).arg(&path);
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("refusing: the spec contains what looks like a secret"), "{stderr}");
    no_piece(&stderr, &github_token(), 4);
    assert_eq!(task_count(&f), tasks);
}

/// AC12 spec_secrets (t2).
#[test]
fn spec_secrets_env_and_stored_values_are_refused_but_short_ones_pass() {
    let f = common::fixture();
    init(&f);
    let spec = spec_with("say dummy-rd2-env-1 now", &["AC1: prints hello"], &[]).to_string();
    let out = create(&f, &spec, None, &[("RD2_TEST_TOKEN", "dummy-rd2-env-1")]);
    assert_eq!(out.status.code(), Some(1));
    let err = error_of(&out);
    assert!(err.contains("env:RD2_TEST_TOKEN in goal"), "{err}");
    no_piece(&String::from_utf8_lossy(&out.stderr), "dummy-rd2-env-1", 0);

    let spec = spec_with("say dummy-1 now", &["AC1: prints hello"], &[]).to_string();
    let out = create(&f, &spec, None, &[("RD2_TEST_TOKEN", "dummy-1")]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));

    std::fs::write(f.home_path().join("secrets.json"), r#"{"aistupidlevel_api_key":"dummy-rd2-stored-1"}"#).unwrap();
    let spec = spec_with("say hello", &["AC1: prints hello"], &["use dummy-rd2-stored-1 here"]).to_string();
    let out = create(&f, &spec, None, &[]);
    assert_eq!(out.status.code(), Some(1));
    let err = error_of(&out);
    assert!(err.contains("thoth:aistupidlevel-api-key in constraints[0]"), "{err}");
    no_piece(&String::from_utf8_lossy(&out.stderr), "dummy-rd2-stored-1", 0);
}

/// AC12 spec_secrets (t3).
#[test]
fn spec_secrets_update_spec_refuses_and_keeps_the_old_spec() {
    let f = common::fixture();
    init(&f);
    let out = create(&f, &spec_with("say hello", &["AC1: prints hello"], &[]).to_string(), None, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let task = serde_json::from_slice::<Value>(&out.stdout).unwrap()["task_id"].as_str().unwrap().to_string();
    let spec_before = json_stdout(&f, &["task", "show", &task, "--json"])["spec"].clone();
    let events = event_count(&f);

    let (text, line) = pretty_spec_with_token();
    let path = spec_file(&f, &text);
    let mut c = clean(&f);
    c.args(["task", "update-spec", &task, "-f"]).arg(&path).arg("--json");
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = error_of(&out);
    assert!(err.starts_with("refusing: the spec contains what looks like a secret"), "{err}");
    assert!(err.contains("github-token in acceptance_criteria[1]"), "{err}");
    assert!(err.contains(&format!("; line {line})")), "{err}");
    no_piece(&String::from_utf8_lossy(&out.stderr), &github_token(), 4);
    assert_eq!(json_stdout(&f, &["task", "show", &task, "--json"])["spec"], spec_before);
    assert_eq!(event_count(&f), events);
    let updated = json_stdout(&f, &["events", "--since", "0", "--json"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "spec_updated")
        .count();
    assert_eq!(updated, 0);

    let path = spec_file(&f, &spec_with("say hello again", &["AC1: prints hello"], &[]).to_string());
    let mut c = clean(&f);
    c.args(["task", "update-spec", &task, "-f"]).arg(&path).arg("--json");
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
}

/// AC12 spec_secrets (t4).
#[test]
fn spec_secrets_title_is_checked_too() {
    let f = common::fixture();
    init(&f);
    let spec = spec_with("say hello", &["AC1: prints hello"], &[]).to_string();
    let tasks = task_count(&f);
    let title = format!("deploy {}", openai_key());
    let out = create(&f, &spec, Some(&title), &[]);
    assert_eq!(out.status.code(), Some(1));
    let err = error_of(&out);
    assert!(err.starts_with("refusing: the title contains what looks like a secret"), "{err}");
    assert!(err.contains("openai-key"), "{err}");
    no_piece(&String::from_utf8_lossy(&out.stderr), &openai_key(), 3);
    assert_eq!(task_count(&f), tasks);

    let out = create(&f, &spec, Some("deploy"), &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
}

/// AC12 spec_secrets (t5).
#[test]
fn spec_secrets_clean_specs_with_lookalikes_pass() {
    let f = common::fixture();
    init(&f);
    let ghp10 = format!("{}dummy01234", ["gh", "p_"].concat());
    let sk = format!("{}abc", ["s", "k-"].concat());
    let ac2 = format!("AC2: mentions {sk} and {ghp10}");
    let spec = spec_with(
        "keep task-1-attempt-2 working",
        &["AC1: reads AISTUPIDLEVEL_API_KEY by name", &ac2],
        &["the header -----BEGIN CERTIFICATE----- is fine"],
    )
    .to_string();
    let out = create(&f, &spec, None, &[]);
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
}

/// A `thoth mcp` server over stdio, as in tests/config_registry.rs.
struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &Fixture, extra_env: &[(&str, &str)]) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("CLAUDE_PLUGIN_ROOT")
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

/// AC12 spec_secrets (t6).
#[test]
fn spec_secrets_mcp_task_create_reports_the_refusal_without_the_value() {
    let f = common::fixture();
    init(&f);
    let tasks = task_count(&f);
    let mut s = Server::start(&f, &[]);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}));
    let ac = format!("AC1: uses {}", github_token());
    let res = s.call(2, "thoth_task_create", json!({"title": "x", "spec": spec_with("say hello", &[&ac], &[])}));
    assert_eq!(res["result"]["isError"], true, "{}", res["result"]["structuredContent"]["error"]);
    let text = res.to_string();
    assert!(text.contains("github-token"), "{}", res["result"]["structuredContent"]["error"]);
    assert!(text.contains("line 1"), "{}", res["result"]["structuredContent"]["error"]);
    no_piece(&text, &github_token(), 4);
    let tmp = f.home_path().join("mcp").join("tmp");
    let left = std::fs::read_dir(&tmp).map(|d| d.count()).unwrap_or(0);
    assert_eq!(left, 0, "a temporary spec file was left in mcp/tmp");
    drop(s);
    assert_eq!(task_count(&f), tasks);
}
