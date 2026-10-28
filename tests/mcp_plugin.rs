mod common;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const REPO_PLUGIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/plugins/thoth");
const SPEC: &str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low"}"#;
const STATE_CHANGING: [&str; 17] = [
    "thoth_init",
    "thoth_task_create",
    "thoth_task_update_spec",
    "thoth_task_close",
    "thoth_task_accept",
    "thoth_dispatch",
    "thoth_review_start",
    "thoth_review_record",
    "thoth_spec_review_record",
    "thoth_spec_review_start",
    "thoth_spec_review_confirm",
    "thoth_resume",
    "thoth_cancel",
    "thoth_verify_start",
    "thoth_checks_add",
    "thoth_checks_approve",
    "thoth_checks_remove",
];

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

fn command(f: &common::Fixture, extra: &[&str], env_root: Option<&Path>) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
    c.arg("mcp")
        .args(extra)
        .current_dir(f.repo.path())
        .env("THOTH_HOME", f.home.path())
        .env_remove("THOTH_RUN_ID")
        .env_remove("THOTH_INBOX_DIR")
        .env_remove("THOTH_EXE")
        .env_remove("CLAUDE_PLUGIN_ROOT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(p) = env_root {
        c.env("CLAUDE_PLUGIN_ROOT", p);
    }
    c
}

impl Server {
    fn start(f: &common::Fixture, extra: &[&str]) -> Self {
        Self::start_env(f, extra, None)
    }
    fn start_env(f: &common::Fixture, extra: &[&str], env_root: Option<&Path>) -> Self {
        let mut child = command(f, extra, env_root).spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
        }
    }
    fn send(&mut self, v: &Value) -> Value {
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    }
    fn initialize(&mut self) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}))
    }
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn repo_file(rel: &str) -> String {
    std::fs::read_to_string(Path::new(REPO_PLUGIN).join(rel)).unwrap()
}

fn fake_plugin(f: &common::Fixture, manifest: &str, skill: Option<&str>) -> PathBuf {
    let root = f.home.path().join("plugin-root");
    std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
    std::fs::write(root.join(".claude-plugin/plugin.json"), manifest).unwrap();
    if let Some(s) = skill {
        std::fs::create_dir_all(root.join("skills/thoth-dispatch")).unwrap();
        std::fs::write(root.join("skills/thoth-dispatch/SKILL.md"), s).unwrap();
    }
    root
}

fn stale_plugin(f: &common::Fixture) -> PathBuf {
    let skill = format!("{}\nextra line\n", repo_file("skills/thoth-dispatch/SKILL.md"));
    fake_plugin(f, &repo_file(".claude-plugin/plugin.json"), Some(&skill))
}

fn task_create() -> Value {
    json!({"title":"pv1","spec":serde_json::from_str::<Value>(SPEC).unwrap()})
}

fn root_arg(p: &Path) -> String {
    p.display().to_string()
}

#[test]
fn a_matching_plugin_changes_nothing() {
    let f = common::fixture();
    let mut s = Server::start(&f, &["--plugin-root", REPO_PLUGIN]);
    let init = s.initialize();
    assert!(!init["result"]["instructions"]
        .as_str()
        .unwrap()
        .contains("does not match"));
    let r = s.call("thoth_init", json!({}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(r["result"]["content"].as_array().unwrap().len(), 1);
    let r = s.call("thoth_task_create", task_create());
    assert_eq!(r["result"]["isError"], false, "{r}");
    assert_eq!(r["result"]["structuredContent"]["ok"], true);
}

#[test]
fn crlf_in_the_installed_skill_is_not_a_difference() {
    let f = common::fixture();
    let skill = repo_file("skills/thoth-dispatch/SKILL.md")
        .replace("\r\n", "\n")
        .replace('\n', "\r\n");
    let root = fake_plugin(&f, &repo_file(".claude-plugin/plugin.json"), Some(&skill));
    let mut s = Server::start(&f, &["--plugin-root", &root_arg(&root)]);
    let r = s.call("thoth_init", json!({}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let r = s.call("thoth_task_create", task_create());
    assert_eq!(r["result"]["isError"], false, "{r}");
}

#[test]
fn a_stale_skill_refuses_state_changing_tools_and_warns_on_read_only_ones() {
    let f = common::fixture();
    let root = stale_plugin(&f);
    let mut s = Server::start(&f, &["--plugin-root", &root_arg(&root)]);
    let init = s.initialize();
    let instructions = init["result"]["instructions"].as_str().unwrap();
    assert!(instructions.starts_with("Before first use, call thoth_guide"));
    assert!(instructions.contains("\n\nwarning: the installed Thoth plugin at "));
    assert!(instructions
        .contains("skill file differs from the one built into the executable: "));

    f.cmd().args(["init", "--json"]).assert().success();

    let r = s.call("thoth_status", json!({}));
    let res = &r["result"];
    assert_eq!(res["isError"], false, "{r}");
    let content = res["content"].as_array().unwrap();
    assert_eq!(content.len(), 2);
    let warning = content[0]["text"].as_str().unwrap();
    assert!(warning.starts_with("warning: the installed Thoth plugin at "));
    assert!(warning.ends_with("thoth doctor shows both versions."));
    let doc: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    // thoth status --json has no `ok` key on success; its document is {last_verify, tasks}.
    assert!(doc["tasks"].is_array());
    assert!(res["structuredContent"]["tasks"].is_array());

    let r = s.call("thoth_task_create", task_create());
    let res = &r["result"];
    assert_eq!(res["isError"], true, "{r}");
    let sc = &res["structuredContent"];
    assert_eq!(sc["ok"], false);
    assert!(sc["exit_code"].is_null());
    let err = sc["error"].as_str().unwrap();
    assert!(err.starts_with("refused: the installed Thoth plugin at "));
    assert!(err.contains("claude plugin uninstall thoth@thoth"));
    let ed = &sc["error_document"];
    assert_eq!(ed["reason"], "plugin_mismatch");
    assert_eq!(ed["differences"], json!(["skill"]));
    assert_eq!(ed["installed_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(ed["expected_version"], env!("CARGO_PKG_VERSION"));

    let out = f.cmd().args(["status", "--json"]).output().unwrap();
    let status: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(status["tasks"].as_array().unwrap().len(), 0);

    for name in STATE_CHANGING {
        let r = s.call(name, json!({}));
        assert_eq!(r["result"]["isError"], true, "{name}: {r}");
        assert!(
            r["result"]["structuredContent"]["error"]
                .as_str()
                .unwrap()
                .starts_with("refused: the installed Thoth plugin"),
            "{name}"
        );
    }

    let r = s.call("thoth_guide", json!({}));
    let content = r["result"]["content"].as_array().unwrap();
    assert!(content[0]["text"].as_str().unwrap().starts_with("warning: "));
    let doc: Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(doc["ok"], true);

    let r = s.call("thoth_status", json!({"task":"01DOESNOTEXIST"}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .starts_with("warning: "));

    let r = s.call("thoth_checks_approve", json!({}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert!(r["result"]["structuredContent"]["error"]
        .as_str()
        .unwrap()
        .starts_with("refused: the installed Thoth plugin"));
}

#[test]
fn a_different_version_is_reported_with_both_versions() {
    let f = common::fixture();
    let root = fake_plugin(
        &f,
        r#"{"name":"thoth","version":"0.0.1"}"#,
        Some(&repo_file("skills/thoth-dispatch/SKILL.md")),
    );
    let mut s = Server::start(&f, &["--plugin-root", &root_arg(&root)]);
    let r = s.call("thoth_task_create", task_create());
    let sc = &r["result"]["structuredContent"];
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(sc["error_document"]["differences"], json!(["version"]));
    assert!(sc["error"].as_str().unwrap().contains(&format!(
        "plugin version 0.0.1, executable version {}",
        env!("CARGO_PKG_VERSION")
    )));
    assert_eq!(sc["error_document"]["installed_version"], "0.0.1");
}

#[test]
fn a_missing_manifest_or_skill_counts_as_a_difference() {
    let f = common::fixture();
    let empty = f.home.path().join("empty-plugin");
    std::fs::create_dir_all(&empty).unwrap();
    let missing = f.home.path().join("no-such-dir");
    for dir in [&empty, &missing] {
        let mut s = Server::start(&f, &["--plugin-root", &root_arg(dir)]);
        let r = s.call("thoth_init", json!({}));
        let sc = &r["result"]["structuredContent"];
        assert_eq!(r["result"]["isError"], true, "{r}");
        assert_eq!(
            sc["error_document"]["differences"],
            json!(["manifest_missing", "skill_missing"])
        );
        assert!(sc["error_document"]["installed_version"].is_null());
        let err = sc["error"].as_str().unwrap();
        assert!(err.contains("plugin manifest missing or unreadable: "));
        assert!(err.contains("skill file missing: "));
    }
}

#[test]
fn the_plugin_root_also_comes_from_the_environment_claude_code_exports_and_the_flag_wins() {
    let f = common::fixture();
    let root = stale_plugin(&f);
    let mut s = Server::start_env(&f, &[], Some(&root));
    let r = s.call("thoth_init", json!({}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(
        r["result"]["structuredContent"]["error_document"]["plugin_root"],
        json!(root)
    );
    drop(s);
    let mut s = Server::start_env(&f, &["--plugin-root", REPO_PLUGIN], Some(&root));
    let r = s.call("thoth_init", json!({}));
    assert_eq!(r["result"]["isError"], false, "{r}");
}

#[test]
fn an_unexpanded_plugin_root_is_ignored_with_a_stderr_warning() {
    let f = common::fixture();
    let mut child = command(&f, &["--plugin-root", "${CLAUDE_PLUGIN_ROOT}"], None)
        .spawn()
        .unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        writeln!(input, r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{"protocolVersion":"2025-06-18"}}}}"#).unwrap();
        writeln!(input, r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"thoth_init","arguments":{{}}}}}}"#).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let second: Value = serde_json::from_str(stdout.lines().nth(1).unwrap()).unwrap();
    assert_eq!(second["result"]["isError"], false, "{second}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(stderr.contains(
        "warning: ignoring invalid --plugin-root ${CLAUDE_PLUGIN_ROOT}; using CLAUDE_PLUGIN_ROOT if set"
    ));
}

#[test]
fn the_check_repeats_on_every_call_so_a_fixed_plugin_needs_no_restart() {
    let f = common::fixture();
    let root = stale_plugin(&f);
    let skill_file = root.join("skills/thoth-dispatch/SKILL.md");
    let mut s = Server::start(&f, &["--plugin-root", &root_arg(&root)]);
    let r = s.call("thoth_init", json!({}));
    assert_eq!(r["result"]["isError"], true, "{r}");
    std::fs::write(&skill_file, repo_file("skills/thoth-dispatch/SKILL.md")).unwrap();
    let r = s.call("thoth_init", json!({}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    std::fs::remove_file(&skill_file).unwrap();
    let r = s.call("thoth_task_create", task_create());
    assert_eq!(r["result"]["isError"], true, "{r}");
    assert_eq!(
        r["result"]["structuredContent"]["error_document"]["differences"],
        json!(["skill_missing"])
    );
}

#[test]
fn startup_prints_one_warning_line_on_stderr_only_when_mismatched() {
    let f = common::fixture();
    let root = stale_plugin(&f);
    let stderr_of = |extra: &[&str]| {
        let mut child = command(&f, extra, None).spawn().unwrap();
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        String::from_utf8(out.stderr).unwrap()
    };
    let stale = stderr_of(&["--plugin-root", &root_arg(&root)]);
    assert_eq!(stale.lines().count(), 1, "{stale}");
    assert!(stale.starts_with("warning: the installed Thoth plugin at "));
    assert_eq!(stderr_of(&["--plugin-root", REPO_PLUGIN]), "");
    assert_eq!(stderr_of(&[]), "");
}

#[test]
fn help_lists_plugin_root() {
    let f = common::fixture();
    let out = f.cmd().args(["mcp", "--help"]).output().unwrap();
    assert!(String::from_utf8(out.stdout).unwrap().contains("--plugin-root"));
}
