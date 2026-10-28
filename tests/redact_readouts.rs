//! RD-3 (RD-1b): every other readout - the stdout of each command, the
//! transcript, MCP results, the report page and the shared records file -
//! leaves masked, while what is stored stays as it was and no
//! `secret_redacted` is written. Every fake secret is built by
//! concatenation, so no full key format appears in this file.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const RD_TOKEN: &str = "dummy-rd-env-secret-1";
const RD_PASSWORD: &str = "dummy&<pass\"word\\1";
const ALNUM36: &str = "dummyDUMMY0123456789dummyDUMMY012345";

fn github_token() -> String {
    format!("{}{ALNUM36}", ["gh", "p_"].concat())
}

fn openai_key() -> String {
    format!("{}{}", ["s", "k-"].concat(), "dummyproj0123456789dummy")
}

/// The password as it appears inside a JSON string.
fn password_json() -> String {
    let quoted = serde_json::to_string(RD_PASSWORD).unwrap();
    quoted[1..quoted.len() - 1].to_string()
}

/// `f.cmd()` without any variable of the test process whose name holds KEY,
/// TOKEN, SECRET or PASSWORD (only the names are read), so the host's own
/// values never take part.
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

/// `clean` with the two test secrets set.
fn with_env(f: &Fixture) -> assert_cmd::Command {
    let mut c = clean(f);
    c.env("RD_TEST_TOKEN", RD_TOKEN).env("RD_TEST_PASSWORD", RD_PASSWORD);
    c
}

/// stdout of a command that must exit 0.
fn stdout_of(mut c: assert_cmd::Command, what: &str) -> String {
    let out = c.output().unwrap();
    assert!(out.status.success(), "{what} exited {:?}: {}", out.status.code(), String::from_utf8_lossy(&out.stderr));
    String::from_utf8(out.stdout).unwrap()
}

/// init, a claude profile, a two-criterion task whose goal is `goal`, and the
/// `hooks` fake worker's candidate at in_review. Returns (task_id,
/// implementer run_id).
fn candidate(f: &Fixture, goal: &str, extra: &[&str]) -> (String, String) {
    clean(f).arg("init").assert().success();
    f.add_profile("claude-b");
    let mut spec: Value = serde_json::from_str(Fixture::SPEC_TWO_AC).unwrap();
    spec["goal"] = json!(goal);
    let (task, _) = f.create_task(&spec.to_string());
    let d = f.dispatch(&task, "claude-b", "hooks", extra);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");
    (task, run)
}

/// The first command row with a stored blob.
fn command_row(f: &Fixture, task: &str) -> Value {
    f.evidence_rows(task)
        .into_iter()
        .find(|r| r["kind"] == "command" && r["blob_path"].is_string())
        .expect("a command row with a blob")
}

fn redactions(f: &Fixture, task: &str) -> usize {
    f.events_of(task).iter().filter(|t| *t == "secret_redacted").count()
}

/// `text` holds neither `secret` nor any 8 consecutive characters of it
/// from `prefix_len` on.
fn no_secret(text: &str, secret: &str, prefix_len: usize) {
    assert!(!text.contains(secret), "the whole secret is in the output");
    let tail = &secret[prefix_len..];
    for i in 0..=tail.len().saturating_sub(8) {
        let piece = &tail[i..(i + 8).min(tail.len())];
        assert!(!text.contains(piece), "8 characters of a secret, from offset {}, are in the output", prefix_len + i);
    }
}

/// `no_secret` for every secret these tests plant.
fn no_secrets(text: &str) {
    no_secret(text, &github_token(), 4);
    no_secret(text, &openai_key(), 3);
    no_secret(text, RD_TOKEN, 0);
    no_secret(text, RD_PASSWORD, 0);
    no_secret(text, &password_json(), 0);
}

fn run_dir(f: &Fixture, task: &str, run: &str) -> PathBuf {
    PathBuf::from(f.run_of(task, run)["run_dir"].as_str().unwrap())
}

/// AC11 (t1): status, events and transcript, JSON and human, carry no
/// secret; the stream and the events are unchanged; stored values are not.
#[test]
fn redact_status_events_and_transcript_outputs_carry_no_secret() {
    let f = common::fixture();
    let reason = github_token();
    let (task, run) = candidate(&f, &format!("say {RD_PASSWORD}"), &["--reason", &reason]);
    let stream = run_dir(&f, &task, &run).join("stream.jsonl");
    let text = format!("{} {}", "a".repeat(290), openai_key());
    let line = json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}});
    let mut file = std::fs::OpenOptions::new().append(true).open(&stream).unwrap();
    writeln!(file, "{line}").unwrap();
    drop(file);
    let stored = std::fs::read(&stream).unwrap();
    let before = redactions(&f, &task);

    let outputs: Vec<(&str, Vec<&str>)> = vec![
        ("status --json", vec!["status", &task, "--json"]),
        ("status", vec!["status", &task]),
        ("events --json", vec!["events", "--since", "0", "--task", &task, "--json"]),
        ("events", vec!["events", "--since", "0", "--task", &task]),
        ("transcript --json", vec!["transcript", &run, "--json"]),
    ];
    for (what, args) in outputs {
        let mut c = with_env(&f);
        c.args(&args);
        let out = stdout_of(c, what);
        no_secrets(&out);
        if what != "status" && what != "transcript --json" {
            assert!(out.contains("[redacted:"), "{what} has no marker");
        }
        if what == "transcript --json" {
            let v: Value = serde_json::from_str(out.trim()).unwrap();
            let last = v["steps"].as_array().unwrap().last().unwrap()["text"].as_str().unwrap().to_string();
            assert!(!last.contains("sk-"), "the cut kept the key's prefix");
            assert!(last.contains("[redacted"), "the last step has no marker");
        }
    }
    assert_eq!(std::fs::read(&stream).unwrap(), stored, "stream.jsonl is unchanged");
    assert_eq!(redactions(&f, &task), before, "no secret_redacted for these readouts");

    let mut c = clean(&f);
    c.args(["status", &task, "--json"]);
    let v: Value = serde_json::from_str(stdout_of(c, "status --json").trim()).unwrap();
    assert!(v["title"].as_str().unwrap().contains(RD_PASSWORD), "the stored title changed");
}

/// AC11 (t2): the report page and the records file carry no secret.
#[test]
fn redact_report_and_records_carry_no_secret() {
    let f = common::fixture();
    let reason = format!("deploy {}", openai_key());
    let (task, _) = candidate(&f, &format!("say {RD_PASSWORD}"), &["--reason", &reason]);
    let doc = json!({"verdict": "REJECT", "findings": {"criteria": [
        {"id": "AC1", "verdict": "unmet", "evidence": ["E001"], "note": github_token()},
        {"id": "AC2", "verdict": "met", "evidence": ["E001"]}
    ], "extra_findings": []}});
    let review = f.home_path().join("review-reject.json");
    std::fs::write(&review, doc.to_string()).unwrap();
    let mut c = clean(&f);
    c.args(["review", "record", &task, "-f"]).arg(&review).arg("--json");
    stdout_of(c, "review record");

    let mut c = with_env(&f);
    c.args(["report", "--json"]);
    stdout_of(c, "report");
    let page = std::fs::read_to_string(f.home_path().join("report.html")).unwrap();
    no_secrets(&page);
    let html = RD_PASSWORD.replace('&', "&amp;").replace('<', "&lt;").replace('"', "&quot;");
    no_secret(&page, &html, 0);
    assert!(page.contains("[redacted:"), "the report has no marker");

    let dir = f.home_path().join("records-d");
    std::fs::create_dir_all(&dir).unwrap();
    let mut c = clean(&f);
    c.args(["config", "set", "records-dir"]).arg(&dir).arg("--json");
    stdout_of(c, "config set");
    let mut c = with_env(&f);
    c.args(["records", "export", "--json"]);
    let v: Value = serde_json::from_str(stdout_of(c, "records export").trim()).unwrap();
    let written = std::fs::read_to_string(v["path"].as_str().unwrap()).unwrap();
    no_secrets(&written);
    assert!(written.contains("[redacted:openai-key]"), "the records file has no openai-key marker");
}

/// A `thoth mcp` server over stdio, as in tests/config_registry.rs, with
/// extra arguments.
struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &Fixture, extra_env: &[(&str, &str)], extra_args: &[&str]) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .args(extra_args)
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
        for (name, _) in std::env::vars_os() {
            let upper = name.to_string_lossy().to_ascii_uppercase();
            if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
                c.env_remove(&name);
            }
        }
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

fn mcp_no_secrets(text: &str) {
    no_secret(text, &github_token(), 4);
    no_secret(text, RD_TOKEN, 0);
}

/// AC11 (t3): MCP results, inline or saved under mcp/out, and the packet
/// file carry no secret.
#[test]
fn redact_mcp_results_and_saved_results_carry_no_secret() {
    let f = common::fixture();
    let (task, _) = candidate(&f, "say hello twice", &[]);
    let row = command_row(&f, &task);
    let code = row["code"].as_str().unwrap().to_string();
    std::fs::write(row["blob_path"].as_str().unwrap(), format!("out {} and {RD_TOKEN}\n", github_token())).unwrap();

    let mut s = Server::start(&f, &[("RD_TEST_TOKEN", RD_TOKEN)], &["--max-result-bytes", "200"]);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}));
    let res = s.call(2, "thoth_evidence_show", json!({"code": code, "task": task, "bytes": 100000}));
    mcp_no_secrets(&res.to_string());
    let sc = &res["result"]["structuredContent"];
    if sc["too_large"] == true {
        let saved = std::fs::read_to_string(sc["path"].as_str().unwrap()).unwrap();
        mcp_no_secrets(&saved);
        assert!(saved.contains("[redacted:"), "the saved result has no marker");
    } else {
        assert!(res.to_string().contains("[redacted:"), "the result has no marker");
    }

    let res = s.call(3, "thoth_review_packet", json!({"task": task}));
    mcp_no_secrets(&res.to_string());
    let path = res["result"]["structuredContent"]["path"].as_str().expect("a packet path").to_string();
    let packet = std::fs::read_to_string(path).unwrap();
    mcp_no_secrets(&packet);
    assert!(packet.contains("[redacted:"), "the packet has no marker");
}

/// Every line of the records file a `records export --json` wrote to a new
/// directory `name` under the state directory.
fn export_to(f: &Fixture, name: &str) -> String {
    let dir: PathBuf = f.home_path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let mut c = clean(f);
    c.args(["config", "set", "records-dir"]).arg(Path::new(&dir)).arg("--json");
    stdout_of(c, "config set");
    let mut c = clean(f);
    c.args(["records", "export", "--json"]);
    let v: Value = serde_json::from_str(stdout_of(c, "records export").trim()).unwrap();
    std::fs::read_to_string(v["path"].as_str().unwrap()).unwrap()
}

/// AC11 (t4): with nothing to mask, no readout carries a marker, no event is
/// written, and the records file keeps `Line`'s field order.
#[test]
fn redact_a_clean_task_shows_no_marker_and_records_no_event() {
    let f = common::fixture();
    let (task, run) = candidate(&f, "say hello twice", &[]);
    let mut commands: Vec<Vec<String>> = Vec::new();
    for row in f.evidence_rows(&task) {
        let code = row["code"].as_str().unwrap().to_string();
        commands.push(vec!["evidence".into(), "show".into(), code.clone(), "--task".into(), task.clone()]);
        commands.push(vec!["evidence".into(), "show".into(), code, "--task".into(), task.clone(), "--json".into()]);
    }
    for args in [
        vec!["review", "packet", &task],
        vec!["review", "packet", &task, "--json"],
        vec!["status", &task],
        vec!["status", &task, "--json"],
        vec!["events", "--since", "0", "--task", &task],
        vec!["events", "--since", "0", "--task", &task, "--json"],
        vec!["transcript", &run],
        vec!["transcript", &run, "--json"],
        vec!["report", "--json"],
    ] {
        commands.push(args.into_iter().map(str::to_string).collect());
    }
    for args in &commands {
        let mut c = clean(&f);
        c.args(args);
        let out = stdout_of(c, &args.join(" "));
        assert!(!out.contains("[redacted:"), "{} printed a marker", args.join(" "));
    }
    let page = std::fs::read_to_string(f.home_path().join("report.html")).unwrap();
    assert!(!page.contains("[redacted:"), "the report has a marker");
    assert_eq!(redactions(&f, &task), 0);

    let written = export_to(&f, "records-d2");
    assert!(!written.is_empty(), "the records file is empty");
    for line in written.lines() {
        assert!(line.starts_with("{\"v\":"), "a line does not keep Line's field order");
        assert!(!line.contains("[redacted:"), "a records line has a marker");
    }
}
