mod common;

use serde_json::{json, Value};
use std::path::PathBuf;

fn line(v: Value) -> String {
    format!("{}\n", v)
}

fn run_dir(f: &common::Fixture, run_id: &str) -> PathBuf {
    let dir: String = f
        .state_conn()
        .query_row(
            "SELECT run_dir FROM runs WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .unwrap();
    PathBuf::from(dir)
}

fn transcript(f: &common::Fixture, args: &[&str]) -> Value {
    let out = f
        .cmd()
        .arg("transcript")
        .args(args)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn event_count(f: &common::Fixture) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_claude_stream_is_translated_and_paged_by_offset() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (_task, run_id, _wt) = f.seed_run("claude-b", common::Fixture::SPEC);
    let stream = run_dir(&f, &run_id).join("stream.jsonl");
    let asst = |block: Value, parent: Value| {
        line(json!({"type":"assistant","message":{"content":[block]},"parent_tool_use_id":parent}))
    };
    let tool = |name: &str, input: Value| {
        json!({"type":"tool_use","id":"t","name":name,"input":input})
    };
    let mut body = String::new();
    body += &line(json!({"type":"system","subtype":"init","model":"claude-sonnet-4-6"}));
    body += &asst(json!({"type":"text","text":"Starting"}), Value::Null);
    body += &asst(tool("Read", json!({"file_path":"/tmp/wt/src/a.rs"})), Value::Null);
    body += &line(json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"t","content":"ok","is_error":false}]}}));
    body += &asst(tool("Bash", json!({"command":"cargo test"})), Value::Null);
    body += &line(json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"t","content":"error: boom\nmore","is_error":true}]}}));
    body += &asst(tool("Edit", json!({"file_path":"/tmp/wt/src/b.rs"})), Value::Null);
    body += &asst(tool("Grep", json!({"pattern":"todo"})), json!("toolu_9"));
    body += &line(json!({"type":"system","subtype":"api_retry","attempt":1,
        "max_retries":10,"error":"rate_limit"}));
    body += &line(json!({"type":"result","subtype":"success","is_error":false,"result":"done"}));
    std::fs::write(&stream, &body).unwrap();

    let events_before = event_count(&f);
    let v1 = transcript(&f, &[&run_id]);
    assert_eq!(v1["exists"], true);
    assert_eq!(v1["harness"], "claude-code");
    assert_eq!(v1["role"], "implementer");
    assert_eq!(v1["offset"], 0);
    assert_eq!(v1["reset"], false);
    assert_eq!(v1["next_offset"], body.len() as u64);
    assert_eq!(v1["lines"], 10);
    assert_eq!(v1["skipped"], 0);
    let steps = v1["steps"].as_array().unwrap();
    let kinds: Vec<&str> = steps.iter().map(|s| s["kind"].as_str().unwrap()).collect();
    assert_eq!(
        kinds,
        ["start", "message", "tool", "tool", "error", "tool", "tool", "note", "result"]
    );
    for s in steps {
        for k in [
            "at", "kind", "nested", "text", "tool", "action", "target", "exit_code", "is_error",
        ] {
            assert!(s.get(k).is_some(), "step lacks {k}: {s}");
        }
    }
    assert_eq!(steps[0]["text"], "claude-sonnet-4-6");
    assert_eq!(steps[2]["action"], "read");
    assert_eq!(steps[2]["target"], "/tmp/wt/src/a.rs");
    assert_eq!(steps[2]["nested"], false);
    assert_eq!(steps[3]["action"], "run");
    assert_eq!(steps[3]["target"], "cargo test");
    assert_eq!(steps[4]["text"], "error: boom more");
    assert_eq!(steps[4]["is_error"], true);
    assert_eq!(steps[5]["action"], "edit");
    assert_eq!(steps[6]["action"], "search");
    assert_eq!(steps[6]["target"], "todo");
    assert_eq!(steps[6]["nested"], true);
    assert_eq!(steps[7]["text"], "api retry 1/10: rate_limit");
    assert_eq!(steps[8]["is_error"], false);
    assert_eq!(steps[8]["text"], "done");

    let next = v1["next_offset"].as_u64().unwrap();
    let mut more = String::new();
    more += &asst(tool("Write", json!({"file_path":"/tmp/wt/c.rs"})), Value::Null);
    more += &line(json!({"type":"result","subtype":"success","is_error":false,"result":"ok"}));
    let mut all = body.clone();
    all += &more;
    std::fs::write(&stream, &all).unwrap();
    let v2 = transcript(&f, &[&run_id, "--offset", &next.to_string()]);
    assert_eq!(v2["offset"], next);
    assert_eq!(v2["steps"].as_array().unwrap().len(), 2);
    assert_eq!(v2["steps"][0]["action"], "write");
    assert_eq!(v2["next_offset"], all.len() as u64);

    let v3 = transcript(&f, &[&run_id, "--offset", "999999"]);
    assert_eq!(v3["reset"], true);
    assert_eq!(v3["offset"], 0);
    assert_eq!(v3["steps"].as_array().unwrap().len(), 11);

    let h = String::from_utf8(
        f.cmd()
            .args(["transcript", &run_id])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(h.contains("run cargo test"), "{h}");
    assert!(h.contains("read /tmp/wt/src/a.rs"), "{h}");
    assert!(h.contains("harness=claude-code"), "{h}");
    assert!(h.contains("stream: "), "{h}");

    assert_eq!(event_count(&f), events_before, "transcript writes no events");
}

#[test]
fn a_codex_stream_is_translated() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_codex_profile("codex-a");
    let (_task, run_id, _wt) = f.seed_run("codex-a", common::Fixture::SPEC);
    std::fs::write(
        run_dir(&f, &run_id).join("stream.jsonl"),
        include_str!("fixtures/codex/exec-ok-and-failing-command.jsonl"),
    )
    .unwrap();
    let v = transcript(&f, &[&run_id]);
    assert_eq!(v["harness"], "codex");
    assert_eq!(v["lines"], 11);
    assert_eq!(v["skipped"], 0);
    let got: Vec<(String, Value, Value, Value)> = v["steps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["kind"].as_str().unwrap().to_string(),
                s["action"].clone(),
                s["target"].clone(),
                s["exit_code"].clone(),
            )
        })
        .collect();
    let row = |k: &str, a: Value, t: Value, c: Value| (k.to_string(), a, t, c);
    let want = vec![
        row("start", Value::Null, Value::Null, Value::Null),
        row("message", Value::Null, Value::Null, Value::Null),
        row("tool", json!("run"), json!("/bin/bash -lc 'echo hello-stdout'"), json!(0)),
        row(
            "tool",
            json!("run"),
            json!("/bin/bash -lc 'sh -c \"echo oops >&2; exit 3\"'"),
            json!(3),
        ),
        row("tool", json!("write"), json!("/tmp/repo/a.txt"), Value::Null),
        row("message", Value::Null, Value::Null, Value::Null),
        row("result", Value::Null, Value::Null, Value::Null),
    ];
    assert_eq!(got, want);
    let steps = v["steps"].as_array().unwrap();
    assert_eq!(steps[3]["is_error"], true);
    assert_eq!(steps[2]["is_error"], false);
    assert_eq!(steps[6]["is_error"], false);
    let h = String::from_utf8(
        f.cmd()
            .args(["transcript", &run_id])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(h.contains("(exit 3)"), "{h}");
}

#[test]
fn a_missing_stream_a_partial_line_and_foreign_lines_are_calm() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (_task, run_id, _wt) = f.seed_run("claude-b", common::Fixture::SPEC);
    let stream = run_dir(&f, &run_id).join("stream.jsonl");

    let v0 = transcript(&f, &[&run_id]);
    assert_eq!(v0["exists"], false);
    assert_eq!(v0["steps"].as_array().unwrap().len(), 0);
    assert_eq!(v0["next_offset"], 0);
    let h = String::from_utf8(
        f.cmd()
            .args(["transcript", &run_id])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
    .unwrap();
    assert!(h.contains("no stream yet for "), "{h}");

    let first = line(json!({"type":"thread.started","thread_id":"x"}));
    let partial = r#"{"type":"item.completed""#;
    std::fs::write(&stream, format!("{first}{partial}")).unwrap();
    let v1 = transcript(&f, &[&run_id]);
    assert_eq!(v1["lines"], 1);
    assert_eq!(v1["next_offset"], first.len() as u64);
    assert_eq!(v1["steps"].as_array().unwrap().len(), 1);

    let rest = format!(
        "{}\n",
        r#","item":{"type":"agent_message","text":"hi"}}"#
    );
    let full = format!(
        "{first}{partial}{rest}not json\n{}\n",
        r#"{"event":"init","init":{"model":"gemini"}}"#
    );
    std::fs::write(&stream, &full).unwrap();
    let off = v1["next_offset"].as_u64().unwrap().to_string();
    let v2 = transcript(&f, &[&run_id, "--offset", &off]);
    assert_eq!(v2["lines"], 3);
    assert_eq!(v2["skipped"], 1);
    assert_eq!(v2["steps"].as_array().unwrap().len(), 1);
    assert_eq!(v2["next_offset"], full.len() as u64);
}

#[test]
fn transcript_refuses_an_unknown_run() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let out = f
        .cmd()
        .args(["transcript", "01NOPE", "--json"])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .stderr
        .clone();
    assert!(!out.is_empty());
}
