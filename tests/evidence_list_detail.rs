//! TESTS-1 (AC2): `evidence list TASK --json` carries `detail` (parsed as
//! `show` parses it) and `blob_path` on every row, next to the keys it always
//! had, writes no event, and the MCP tool `thoth_evidence_list` returns the
//! same document.

mod common;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

fn json_of(out: Vec<u8>) -> Value {
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn cli_json(f: &common::Fixture, args: &[&str]) -> Value {
    json_of(f.cmd().args(args).assert().success().get_output().stdout.clone())
}

fn last_seq(f: &common::Fixture) -> i64 {
    cli_json(f, &["events", "--json"])["last_seq"].as_i64().unwrap()
}

/// A `thoth mcp` server over stdio, the way tests/mcp.rs drives one.
struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &common::Fixture) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output }
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        let v = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}});
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        v
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn evidence_list_json_carries_detail_and_blob_path_and_writes_no_event() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task, "claude-b", "hooks", &[]);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s"), (0, "in_review".to_string()));
    let waited = cli_json(&f, &["wait", "--run", &run, "--timeout", "180s", "--json"]);
    assert_eq!(waited["status"], "completed", "{waited}");

    // the list, with no event written by it
    let before = last_seq(&f);
    let doc = cli_json(&f, &["evidence", "list", &task, "--json"]);
    assert_eq!(last_seq(&f), before, "evidence list must write no event");
    assert_eq!(doc["task_id"], task.as_str());
    let rows = doc["evidence"].as_array().unwrap();
    assert_eq!(rows.len(), 5, "three commands, the diff and the commit: {doc:#?}");

    // every existing contract key, value and type
    let codes: Vec<&str> = rows.iter().map(|r| r["code"].as_str().unwrap()).collect();
    assert_eq!(codes, ["E001", "E002", "E003", "E004", "E005"]);
    let kinds: Vec<&str> = rows.iter().map(|r| r["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["command", "command", "command", "diff", "commit"]);
    for r in rows {
        assert_eq!(r["collected_by"], "control_plane", "{r}");
        assert!(r["label"].is_string() && !r["label"].as_str().unwrap().is_empty(), "{r}");
        assert!(r["bytes"].is_i64(), "{r}");
        let sha = r["sha256"].as_str().unwrap_or_else(|| panic!("{r}"));
        assert_eq!(sha.len(), 64, "{r}");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()), "{r}");
        assert_eq!(r["run_id"], run.as_str(), "{r}");
        assert!(r["created_at"].as_str().unwrap().ends_with('Z'), "{r}");
        if r["kind"] == "command" {
            assert!(r["exit_code"].is_i64(), "{r}");
        } else {
            assert!(r["exit_code"].is_null(), "{r}");
        }
        // the additive keys
        assert!(r.get("detail").is_some(), "{r}");
        assert!(r.get("blob_path").is_some(), "{r}");
        assert!(r["detail"].is_object(), "{r}");
        assert!(r["blob_path"].is_string(), "{r}");
        assert!(std::path::Path::new(r["blob_path"].as_str().unwrap()).is_file(), "{r}");
    }
    assert_eq!(rows[0]["label"], "echo hello-stdout");
    assert_eq!(rows[0]["exit_code"], 0);
    assert_eq!(rows[0]["bytes"], 12);
    assert_eq!(rows[1]["exit_code"], 3);

    // command rows: detail.exit_code is the row's exit_code, detail.cmd a string
    for r in rows.iter().filter(|r| r["kind"] == "command") {
        assert_eq!(r["detail"]["exit_code"], r["exit_code"], "{r}");
        assert!(r["detail"]["cmd"].is_string(), "{r}");
    }
    assert_eq!(rows[0]["detail"]["cmd"], "echo hello-stdout");
    // the diff row: protected_changes is an array
    let diff = rows.iter().find(|r| r["kind"] == "diff").unwrap();
    assert!(diff["detail"]["protected_changes"].is_array(), "{diff}");
    // the commit row: exactly the three detail keys the docs promise
    let commit = rows.iter().find(|r| r["kind"] == "commit").unwrap();
    let mut keys: Vec<&str> = commit["detail"].as_object().unwrap().keys().map(|k| k.as_str()).collect();
    keys.sort();
    assert_eq!(keys, ["author_date", "sha", "subject"], "{commit}");

    // the human output has not changed: one line per row, code first,
    // label last, nothing of the detail or the blob path
    let human = f
        .cmd()
        .args(["evidence", "list", &task])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let human = String::from_utf8(human).unwrap();
    let lines: Vec<&str> = human.lines().collect();
    assert_eq!(lines.len(), 5, "{human}");
    for (line, r) in lines.iter().zip(rows) {
        assert!(line.starts_with(r["code"].as_str().unwrap()), "{line}");
        assert!(line.ends_with(r["label"].as_str().unwrap()), "{line}");
        assert!(!line.contains(r["blob_path"].as_str().unwrap()), "{line}");
    }
    assert!(!human.contains("protected_changes"), "{human}");
    assert_eq!(last_seq(&f), before, "the human list writes no event either");

    // the MCP tool returns the same document, and writes no event
    {
        let mut s = Server::start(&f);
        let listed = s.call(1, "thoth_evidence_list", json!({"task": task}));
        assert_eq!(listed["result"]["isError"], false, "{listed}");
        assert_eq!(listed["result"]["structuredContent"], doc);
    }
    assert_eq!(last_seq(&f), before, "thoth_evidence_list must write no event");

    // only now `show`, which does write an event: its detail and blob_path
    // are what list gave
    for r in rows {
        let code = r["code"].as_str().unwrap();
        let shown = cli_json(&f, &["evidence", "show", code, "--task", &task, "--json"]);
        assert_eq!(shown["detail"], r["detail"], "{code}");
        assert_eq!(shown["blob_path"], r["blob_path"], "{code}");
        assert_eq!(shown["sha256"], r["sha256"], "{code}");
        assert_eq!(shown["bytes"], r["bytes"], "{code}");
    }
    assert_eq!(last_seq(&f), before + 5, "show records one evidence_opened per call");
    let kinds = f.events_of(&task);
    assert_eq!(kinds.iter().filter(|k| *k == "evidence_opened").count(), 5);
}
