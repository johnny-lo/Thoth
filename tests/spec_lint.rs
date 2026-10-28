//! LESS-3 (H6): `thoth spec lint`, the same lint run first by `task create`,
//! `task update-spec` and `spec review start`, and the MCP `thoth_spec_lint`.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Every rule the lint has, each once: four errors, five warnings.
fn bad() -> Value {
    json!({
        "goal": "lint fixture",
        "acceptance_criteria": ["AC1: x", "  ", "AC1:  x", "改 .claude/skills/x/SKILL.md"],
        "checks": ["c1", "ghost"],
        "constraints": [
            "see src/present.rs:3",
            "第 12 行",
            "src/gone.rs keeps its name",
            "src/present.rs 的 `present_fn`、`absent_fn`",
            "compare with git diff master..HEAD",
            "set it with `$env:FOO = 1`"
        ]
    })
}

const CLEAN: &str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"]}"#;
const LINE_WARNING: &str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":["see 第 3 行"]}"#;

fn json_of(out: &[u8]) -> Value {
    serde_json::from_str(String::from_utf8_lossy(out).trim()).unwrap()
}

/// init, check c1 in the catalogue, and `src/present.rs` in the main tree.
fn lint_fixture() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.cmd().args(["checks", "add", "c1", "--cmd", "git --version"]).assert().success();
    std::fs::create_dir_all(f.repo.path().join("src")).unwrap();
    std::fs::write(f.repo.path().join("src").join("present.rs"), "fn present_fn() {}").unwrap();
    f
}

fn write_spec(f: &Fixture, name: &str, contents: &str) -> PathBuf {
    let p = f.home_path().join(name);
    std::fs::write(&p, contents).unwrap();
    p
}

fn rules(list: &Value) -> BTreeSet<String> {
    list.as_array().unwrap().iter().map(|x| x["rule"].as_str().unwrap().to_string()).collect()
}

fn messages_of<'a>(list: &'a Value, rule: &str) -> Vec<&'a str> {
    list.as_array()
        .unwrap()
        .iter()
        .filter(|x| x["rule"] == rule)
        .map(|x| x["message"].as_str().unwrap())
        .collect()
}

fn location_of(list: &Value, rule: &str) -> Value {
    list.as_array().unwrap().iter().find(|x| x["rule"] == rule).unwrap()["location"].clone()
}

fn bad_errors() -> BTreeSet<String> {
    ["ac_blank", "ac_duplicate", "unknown_check", "claude_write"].iter().map(|s| s.to_string()).collect()
}

fn task_count(f: &Fixture) -> usize {
    let out = f.cmd().args(["status", "--json"]).assert().success().get_output().stdout.clone();
    json_of(&out)["tasks"].as_array().unwrap().len()
}

#[test]
fn spec_lint_reports_every_rule_as_json_and_text() {
    let f = lint_fixture();
    let file = write_spec(&f, "bad.json", &bad().to_string());
    let out = f.cmd().args(["spec", "lint"]).arg(&file).arg("--json").assert().code(1).get_output().clone();
    let v = json_of(&out.stdout);
    assert_eq!(v["ok"], false);
    assert_eq!(v["bytes"], bad().to_string().len());
    assert_eq!(rules(&v["errors"]), bad_errors());
    assert_eq!(location_of(&v["errors"], "ac_blank"), "acceptance_criteria[1]");
    assert_eq!(location_of(&v["errors"], "ac_duplicate"), "acceptance_criteria[2]");
    assert_eq!(location_of(&v["errors"], "unknown_check"), "checks[1]");
    assert_eq!(location_of(&v["errors"], "claude_write"), "acceptance_criteria[3]");
    let warned = rules(&v["warnings"]);
    for rule in ["line_number", "missing_path", "missing_name", "two_dot_diff", "powershell_in_bash"] {
        assert!(warned.contains(rule), "{rule} missing from {}", v["warnings"]);
    }
    assert!(messages_of(&v["warnings"], "missing_path").iter().any(|m| m.contains("src/gone.rs")));
    let names = messages_of(&v["warnings"], "missing_name");
    assert!(names.iter().any(|m| m.contains("absent_fn")), "{names:?}");
    assert!(!names.iter().any(|m| m.contains("present_fn")), "{names:?}");
    assert_eq!(location_of(&v["warnings"], "powershell_in_bash"), "constraints[5]");

    let out = f.cmd().args(["spec", "lint"]).arg(&file).assert().code(1).get_output().clone();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("error: ac_duplicate (acceptance_criteria[2])"), "{text}");
    assert!(text.contains("warning: two_dot_diff (constraints[4])"), "{text}");
    assert!(text.trim_end().ends_with(&format!(
        "4 error(s), {} warning(s)",
        v["warnings"].as_array().unwrap().len()
    )), "{text}");
}

#[test]
fn spec_lint_a_clean_spec_has_no_problems() {
    let f = lint_fixture();
    let file = write_spec(&f, "clean.json", CLEAN);
    let out = f.cmd().args(["spec", "lint"]).arg(&file).arg("--json").assert().code(0).get_output().clone();
    let v = json_of(&out.stdout);
    assert_eq!(v["ok"], true);
    assert_eq!(v["errors"], json!([]));
    assert_eq!(v["warnings"], json!([]));
    let out = f.cmd().args(["spec", "lint"]).arg(&file).assert().code(0).get_output().clone();
    assert_eq!(String::from_utf8(out.stdout).unwrap().trim(), "no problems found");

    // a file that cannot be read fails the way task create does
    let missing = f.home_path().join("nope.json");
    let err = f.cmd().args(["spec", "lint"]).arg(&missing).assert().code(1).get_output().stderr.clone();
    assert!(String::from_utf8(err).unwrap().contains("reading spec"));
}

#[test]
fn spec_lint_gates_task_create_and_update_spec() {
    let f = lint_fixture();
    let bad_file = write_spec(&f, "bad.json", &bad().to_string());
    let before = task_count(&f);
    let out = f.cmd().args(["task", "create", "-f"]).arg(&bad_file).arg("--json").assert().code(1).get_output().clone();
    let last = String::from_utf8(out.stderr).unwrap();
    let e = json_of(last.trim().lines().last().unwrap().as_bytes());
    let error = e["error"].as_str().unwrap();
    assert!(error.starts_with("spec lint: "), "{error}");
    assert!(error.contains("ac_duplicate"), "{error}");
    assert_eq!(task_count(&f), before, "nothing was created");

    let warn_file = write_spec(&f, "warn.json", LINE_WARNING);
    let out = f.cmd().args(["task", "create", "-f"]).arg(&warn_file).arg("--json").assert().success().get_output().clone();
    let v = json_of(&out.stdout);
    let task = v["task_id"].as_str().unwrap().to_string();
    let lint_lines: Vec<&str> = v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|w| w.starts_with("spec lint: "))
        .collect();
    assert_eq!(lint_lines.len(), 1, "{}", v["warnings"]);
    assert!(lint_lines[0].starts_with("spec lint: line_number"), "{}", lint_lines[0]);

    // without --json the warning goes to stderr
    let out = f.cmd().args(["task", "create", "-f"]).arg(&warn_file).assert().success().get_output().clone();
    assert!(String::from_utf8(out.stderr).unwrap().contains("warning: spec lint: line_number"));

    let shown = |f: &Fixture| {
        let out = f.cmd().args(["task", "show", &task, "--json"]).assert().success().get_output().stdout.clone();
        json_of(&out)["spec"].clone()
    };
    let spec_before = shown(&f);
    let out = f.cmd().args(["task", "update-spec", &task, "-f"]).arg(&bad_file).arg("--json").assert().code(1).get_output().clone();
    let last = String::from_utf8(out.stderr).unwrap();
    let e = json_of(last.trim().lines().last().unwrap().as_bytes());
    assert!(e["error"].as_str().unwrap().starts_with("spec lint: "), "{e}");
    assert_eq!(shown(&f), spec_before);
}

#[test]
fn spec_lint_gates_spec_review_start() {
    let f = lint_fixture();
    f.add_profile("claude-b");
    f.cmd().args(["checks", "add", "c2", "--cmd", "git --version"]).assert().success();
    let with_c1 = write_spec(&f, "c1.json", r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"checks":["c1"]}"#);
    let out = f.cmd().args(["task", "create", "-f"]).arg(&with_c1).arg("--json").assert().success().get_output().clone();
    let task = json_of(&out.stdout)["task_id"].as_str().unwrap().to_string();
    f.cmd().args(["checks", "remove", "c1"]).assert().success();
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", &task)
        .args(["spec", "review", "start", &task, "--profile", "claude-b", "--model", "haiku", "--json"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("spec lint: "), "{err}");
    assert!(err.contains("unknown_check"), "{err}");
    let out = f.cmd().args(["status", &task, "--json"]).assert().success().get_output().stdout.clone();
    assert_eq!(json_of(&out)["runs"], json!([]));

    let warn_file = write_spec(&f, "warn.json", LINE_WARNING);
    let out = f.cmd().args(["task", "create", "-f"]).arg(&warn_file).arg("--json").assert().success().get_output().clone();
    let task = json_of(&out.stdout)["task_id"].as_str().unwrap().to_string();
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", &task)
        .args(["spec", "review", "start", &task, "--profile", "claude-b", "--model", "haiku", "--json"]);
    let v = json_of(&f.run_ok(c, "spec review start"));
    let lint_lines: Vec<&str> = v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .filter(|w| w.starts_with("spec lint: "))
        .collect();
    assert_eq!(lint_lines.len(), 1, "{}", v["warnings"]);
    assert!(lint_lines[0].starts_with("spec lint: line_number"), "{}", lint_lines[0]);
    let run = v["run_id"].as_str().unwrap();
    let mut c = f.cmd();
    c.args(["wait", "--run", run, "--timeout", "180s", "--json"]);
    f.run_ok(c, "wait --run");
}

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &Fixture) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("CLAUDE_PLUGIN_ROOT")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
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
        serde_json::from_str(&s).unwrap()
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
fn spec_lint_through_mcp() {
    let f = lint_fixture();
    let mut s = Server::start(&f);
    let list = s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}));
    let tool = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "thoth_spec_lint")
        .cloned()
        .expect("thoth_spec_lint is listed");
    assert_eq!(tool["annotations"]["readOnlyHint"], true);

    let r = s.call(2, "thoth_spec_lint", json!({"spec": bad()}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let doc = &r["result"]["structuredContent"];
    assert_eq!(doc["ok"], false);
    assert_eq!(rules(&doc["errors"]), bad_errors());

    let file = write_spec(&f, "bad.json", &bad().to_string());
    let r = s.call(3, "thoth_spec_lint", json!({"file": file.display().to_string()}));
    assert_eq!(r["result"]["isError"], false, "{r}");
    let out = f.cmd().args(["spec", "lint"]).arg(&file).arg("--json").assert().code(1).get_output().stdout.clone();
    assert_eq!(r["result"]["structuredContent"], json_of(&out));

    let r = s.call(4, "thoth_spec_lint", json!({"spec": bad(), "file": file.display().to_string()}));
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("exactly one is required"));
    let r = s.call(5, "thoth_spec_lint", json!({}));
    assert_eq!(r["result"]["isError"], true);
    let r = s.call(6, "thoth_spec_lint", json!({"file": "bad.json"}));
    assert_eq!(r["result"]["isError"], true);
    assert!(r["result"]["content"][0]["text"].as_str().unwrap().contains("absolute path of an existing file"));
}
