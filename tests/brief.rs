//! BRIEF-1: the short conclusion `wait`, `task result`, `review show`,
//! `spec review show`, `submit`, the run-settlement note and the MCP tools
//! lead with, while the full delivery and judgement stay readable. Always the
//! fakes, a temporary THOTH_HOME and the CLI or `thoth mcp`; nothing here
//! opens the state store.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const NORMAL: &str = r#"{"goal":"say hello twice","acceptance_criteria":["AC1: prints hello","AC2: exits zero"],"constraints":["no new crate"],"non_goals":["a third greeting","a farewell"],"excluded_alternatives":[],"risk":"normal"}"#;
const REASON: &str = "brief test: the fake worker stands in for a real one";
const SHORT_BRIEF: &str = "Delivered the greeting; both checks pass. No decision needed.";

/// Whether an inherited variable must not reach thoth: its name contains
/// KEY, TOKEN, SECRET or PASSWORD, in any case. Names only are read.
fn secret_named(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w))
}

fn inherited_secret_names() -> Vec<std::ffi::OsString> {
    std::env::vars_os()
        .map(|(name, _)| name)
        .filter(|name| name.to_str().is_some_and(secret_named))
        .collect()
}

/// A github-format token, joined so no whole one is in the source.
fn token() -> String {
    format!("{}{}", ["gh", "p_"].concat(), "dummyDUMMY0123456789dummyDUMMY012345")
}

/// A piece of the token's body that no marker contains.
fn token_piece() -> String {
    token()[4..16].to_string()
}

/// The value of the synthetic secret variable, joined likewise.
fn exact_value() -> String {
    ["brief1", "-fixture-", "exact-", "value-77"].concat()
}

const SECRET_VAR: &str = "BRIEF1_FIXTURE_SECRET";

struct T {
    f: Fixture,
    /// Variables every thoth started here gets, after the inherited secret
    /// ones are dropped.
    env: Vec<(String, String)>,
}

impl T {
    fn new() -> T {
        T::with_env(Vec::new())
    }

    fn with_env(env: Vec<(String, String)>) -> T {
        let t = T { f: common::fixture(), env };
        t.ok(&["init"], &[]);
        for id in ["claude-a", "claude-b"] {
            t.profile(id, "anthropic", "claude-code");
        }
        t
    }

    fn profile(&self, id: &str, provider: &str, harness: &str) -> std::path::PathBuf {
        let cfg = self.f.home_path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        let cfg_text = cfg.display().to_string();
        self.ok(
            &["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir", &cfg_text],
            &[],
        );
        cfg
    }

    fn cmd(&self, envs: &[(&str, &str)]) -> assert_cmd::Command {
        let mut c = self.f.cmd();
        for name in inherited_secret_names() {
            c.env_remove(name);
        }
        for (k, v) in &self.env {
            c.env(k, v);
        }
        for (k, v) in envs {
            c.env(k, v);
        }
        c
    }

    /// (exit code, stdout, stderr).
    fn run(&self, args: &[&str], envs: &[(&str, &str)]) -> (i32, String, String) {
        let mut c = self.cmd(envs);
        c.args(args);
        let out = c.output().expect("spawn thoth");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }

    /// stdout of a command that must exit 0.
    fn ok(&self, args: &[&str], envs: &[(&str, &str)]) -> String {
        let mut c = self.cmd(envs);
        c.args(args);
        String::from_utf8(self.f.run_ok(c, &args.join(" "))).unwrap()
    }

    fn json(&self, args: &[&str]) -> Value {
        let text = self.ok(args, &[]);
        serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{args:?}: {e}: {text}"))
    }

    /// A `--json` command that must exit `code`.
    fn json_exit(&self, args: &[&str], code: i32) -> Value {
        let (got, out, err) = self.run(args, &[]);
        assert_eq!(got, code, "{args:?}\nstdout: {out}\nstderr: {err}");
        serde_json::from_str(out.trim()).unwrap_or_else(|e| panic!("{args:?}: {e}: {out}"))
    }

    fn create(&self, spec: &str) -> String {
        let path = self.f.home_path().join("brief-spec.json");
        std::fs::write(&path, spec).unwrap();
        let v = self.json(&["task", "create", "-f", &path.display().to_string(), "--json"]);
        v["task_id"].as_str().unwrap().to_string()
    }

    fn dispatch(&self, task: &str, profile: &str, model: &str, scenario: &str, envs: &[(&str, &str)]) -> String {
        let mut all = vec![("FAKE_SCENARIO", scenario), ("FAKE_TASK_ID", task)];
        all.extend_from_slice(envs);
        let out = self.ok(
            &["dispatch", task, "--profile", profile, "--model", model, "--effort", "medium", "--reason", REASON, "--json"],
            &all,
        );
        let v: Value = serde_json::from_str(out.trim()).unwrap();
        v["run_id"].as_str().unwrap().to_string()
    }

    fn wait_run(&self, run: &str) -> Value {
        self.json(&["wait", "--run", run, "--timeout", "180s", "--json"])
    }

    fn submit(&self, run: &str, doc: &Value, envs: &[(&str, &str)]) -> Value {
        let path = self.f.home_path().join("brief-result.json");
        std::fs::write(&path, doc.to_string()).unwrap();
        let out = self.ok(&["submit", "-f", &path.display().to_string(), "--run", run, "--json"], envs);
        serde_json::from_str(out.trim()).unwrap()
    }

    fn events(&self, task: &str) -> Vec<Value> {
        self.json(&["events", "--since", "0", "--task", task, "--json"])["events"]
            .as_array()
            .unwrap()
            .clone()
    }

    /// The newest event of `ty` written on `run`.
    fn newest(&self, task: &str, run: &str, ty: &str) -> Value {
        self.events(task)
            .into_iter()
            .rev()
            .find(|e| e["type"] == ty && e["run_id"] == run)
            .unwrap_or_else(|| panic!("run {run} has no {ty} event\n{}", self.f.explain_recent()))
    }

    fn count(&self, task: &str, ty: &str) -> usize {
        self.events(task).iter().filter(|e| e["type"] == ty).count()
    }

    fn first_code(&self, task: &str) -> String {
        let list = self.json(&["evidence", "list", task, "--json"]);
        list["evidence"][0]["code"].as_str().expect("an evidence code").to_string()
    }

    fn record_review(&self, task: &str, doc: &Value) -> Value {
        let path = self.f.home_path().join("brief-review.json");
        std::fs::write(&path, doc.to_string()).unwrap();
        self.json(&["review", "record", task, "-f", &path.display().to_string(), "--json"])
    }

    fn mcp(&self) -> Server {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(self.f.repo.path())
            .env("THOTH_HOME", self.f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_ARENA_URL", common::offline_arena_url(self.f.home.path()))
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for name in inherited_secret_names() {
            c.env_remove(name);
        }
        for (k, v) in &self.env {
            c.env(k, v);
        }
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Server { child, input, output }
    }
}

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn send(&mut self, v: &Value) -> Value {
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    }

    /// The structuredContent of a successful tools/call.
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let v = self.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}}));
        assert_eq!(v["result"]["isError"], false, "{name}: {v}");
        v["result"]["structuredContent"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn delivery(task: &str, summary: &str, brief: Option<Value>) -> Value {
    let mut d = json!({
        "task_id": task,
        "status": "completed",
        "summary": summary,
        "claims": [{"kind": "completion", "text": "the greeting is delivered", "evidence": []}],
    });
    if let Some(b) = brief {
        d["brief"] = b;
    }
    d
}

const BRIEF_KEYS: [&str; 3] = ["brief", "brief_source", "brief_truncated"];

/// One delivery of the AC1 table: what is submitted and the brief it gets.
struct Case {
    name: &'static str,
    summary: String,
    brief: Option<Value>,
    want: String,
    source: &'static str,
    cut: bool,
}

fn assert_same_brief(a: &Value, b: &Value, what: &str) {
    for k in BRIEF_KEYS {
        assert_eq!(a[k], b[k], "{what}: {k}\n{a}\n{b}");
    }
}

fn assert_null_keys(v: &Value, keys: &[&str]) {
    for k in keys {
        assert!(v.get(*k).is_some_and(Value::is_null), "{k} is not a null key: {v}");
    }
}

/// AC1, AC4, AC7: an otherwise valid delivery's `brief` is its own when it
/// is a non-blank string, otherwise its summary; trimmed, at most 1,000
/// characters, and never a reason to refuse the delivery.
#[test]
fn a_delivery_brief_is_the_workers_or_the_summary_cut_at_1000_characters() {
    let t = T::new();
    let task = t.create(NORMAL);
    let run = t.dispatch(&task, "claude-b", "sonnet", "ok", &[]);
    assert_eq!(t.wait_run(&run)["status"], "completed");

    let case = |name: &'static str, summary: String, brief: Option<Value>, want: String, source: &'static str, cut: bool| {
        Case { name, summary, brief, want, source, cut }
    };
    let cases = vec![
        case("short explicit", "the full summary of the work".into(), Some(json!(SHORT_BRIEF)), SHORT_BRIEF.into(), "worker", false),
        case("padded explicit", "the full summary".into(), Some(json!("  \n Delivered x.\nline two \n ")), "Delivered x.\nline two".into(), "worker", false),
        case("no brief", "the full summary".into(), None, "the full summary".into(), "summary", false),
        case("blank brief", "  the full summary  ".into(), Some(json!(" \n\t ")), "the full summary".into(), "summary", false),
        case("summary of 1500", "s".repeat(1500), None, "s".repeat(1000), "summary", true),
        case("exactly 1000", "sum".into(), Some(json!("e".repeat(1000))), "e".repeat(1000), "worker", false),
        case("1001 Chinese", "sum".into(), Some(json!("中".repeat(1001))), "中".repeat(1000), "worker", true),
        case("1001 emoji", "sum".into(), Some(json!("😀".repeat(1001))), "😀".repeat(1000), "worker", true),
        case("number", "sum".into(), Some(json!(7)), "sum".into(), "summary", false),
        case("null", "sum".into(), Some(Value::Null), "sum".into(), "summary", false),
        case("array", "sum".into(), Some(json!(["x"])), "sum".into(), "summary", false),
        case("object", "sum".into(), Some(json!({"a": 1})), "sum".into(), "summary", false),
    ];
    for Case { name, summary, brief, want, source, cut } in cases {
        let doc = delivery(&task, &summary, brief);
        let sub = t.submit(&run, &doc, &[]);
        assert_eq!(sub["ok"], true, "{name}: {sub}");
        assert_eq!(sub["task_status"], "in_review", "{name}");
        assert_eq!(sub["note"], format!("ingested 1 claim(s): {want}"), "{name}");

        let r = t.json(&["task", "result", &task, "--run", &run, "--json"]);
        assert_eq!(r["brief"], want.as_str(), "{name}");
        assert_eq!(r["brief_source"], source, "{name}");
        assert_eq!(r["brief_truncated"], cut, "{name}");
        assert_eq!(r["summary"], summary.as_str(), "{name}: the summary is whole");
        assert_eq!(r["claim_count"], 1, "{name}");

        let e = t.newest(&task, &run, "result_ingested");
        let d = &e["detail"];
        assert_same_brief(d, &r, name);
        assert_eq!(d["summary"], summary.as_str(), "{name}");
        assert_eq!(d["claim_count"], 1, "{name}");
        assert_eq!(d["evidence_refs"].as_object().map(|o| o.len()), Some(1), "{name}: {d}");
        assert_eq!(d["result_json"], doc, "{name}: the whole delivery is kept");

        let w = t.json(&["wait", &task, "--json"]);
        assert_eq!(w["ok"], true);
        assert_eq!(w["status"], "in_review");
        assert_eq!(w["timed_out"], false);
        assert_eq!(w["run_id"], run.as_str());
        assert_eq!(w["result_status"], "completed");
        assert_eq!(w["claim_count"], 1);
        assert_same_brief(&w, &r, name);
    }

    // The human forms: continuation lines indented, and where a cut one is.
    t.submit(&run, &delivery(&task, "sum", Some(json!("line one\nline two"))), &[]);
    let human = t.ok(&["wait", &task], &[]);
    assert!(human.starts_with(&format!("{task} in_review\nbrief: line one\n  line two")), "{human}");
    assert!(!human.contains("(cut at"), "{human}");

    let long = "s".repeat(1500);
    t.submit(&run, &delivery(&task, &long, None), &[]);
    let human = t.ok(&["wait", &task], &[]);
    assert!(human.contains(&format!("brief: {}\n", "s".repeat(1000))), "{human}");
    assert!(human.contains(&format!("(cut at 1000 chars; full text: thoth task result {task})")), "{human}");
    let human = t.ok(&["wait", "--run", &run], &[]);
    assert!(
        human.contains(&format!("(cut at 1000 chars; full text: thoth task result {task} --run {run})")),
        "{human}"
    );
    let human = t.ok(&["task", "result", &task], &[]);
    let brief_at = human.find("\nbrief: ").expect("a brief line");
    let summary_at = human.find("\nsummary:\n").expect("a summary line");
    assert!(brief_at < summary_at, "{human}");
    assert!(human.contains(&format!("\nsummary:\n  {long}\n")), "{human}");
    assert!(
        human.contains(&format!("(cut at 1000 chars; full text: thoth task result {task} --run {run})")),
        "{human}"
    );

    // AC2: an event written without the brief keys (as before BRIEF-1) is
    // read from its kept document; reading it rewrites nothing.
    let legacy = [("THOTH_TEST_RESULT_PRE_BRIEF", "1")];
    t.submit(&run, &delivery(&task, "the full summary", Some(json!(SHORT_BRIEF))), &legacy);
    let e = t.newest(&task, &run, "result_ingested");
    for k in BRIEF_KEYS {
        assert!(e["detail"].get(k).is_none(), "{k}: {e}");
    }
    let ingested_before = t.count(&task, "result_ingested");
    let r = t.json(&["task", "result", &task, "--json"]);
    assert_eq!((r["brief"].as_str(), r["brief_source"].as_str(), &r["brief_truncated"]),
        (Some(SHORT_BRIEF), Some("worker"), &json!(false)));
    assert_same_brief(&t.json(&["wait", &task, "--json"]), &r, "legacy wait");
    t.submit(&run, &delivery(&task, &long, Some(json!("  "))), &legacy);
    let r = t.json(&["task", "result", &task, "--json"]);
    assert_eq!((r["brief"].as_str(), r["brief_source"].as_str(), &r["brief_truncated"]),
        (Some("s".repeat(1000).as_str()), Some("summary"), &json!(true)));
    assert_same_brief(&t.json(&["wait", "--run", &run, "--json"]), &r, "legacy wait --run");
    assert_eq!(t.count(&task, "result_ingested"), ingested_before + 1, "reading wrote no result_ingested");
    assert!(t.newest(&task, &run, "result_ingested")["detail"].get("brief").is_none());

    // A delivery that breaks an existing rule is still refused for that rule.
    let mut bad = delivery(&task, "sum", Some(json!(SHORT_BRIEF)));
    bad["claims"] = json!([{"kind": "test_result", "text": "green", "evidence": []}]);
    let path = t.f.home_path().join("brief-bad.json");
    std::fs::write(&path, bad.to_string()).unwrap();
    let v = t.json_exit(&["submit", "-f", &path.display().to_string(), "--run", &run, "--json"], 1);
    assert_eq!(v["ok"], false);
    assert!(v["note"].as_str().unwrap().contains("cites no evidence"), "{v}");
}

/// AC2, AC4, AC5, AC6, AC8: the run's settlement note, `wait` on the task
/// and on each run, a fake reviewer's REJECT, `review show` with and
/// without --full, and the same keys through MCP.
#[test]
fn settlement_notes_waits_and_a_reviewer_judgement_lead_with_the_brief() {
    let t = T::new();
    let task = t.create(NORMAL);
    let long = "The full explanation of the change. ".repeat(40);
    let run = t.dispatch(
        &task,
        "claude-b",
        "sonnet",
        "ok",
        &[("FAKE_RESULT_SUMMARY", &long), ("FAKE_RESULT_BRIEF", SHORT_BRIEF)],
    );
    let w = t.json(&["wait", &task, "--timeout", "180s", "--json"]);
    assert_eq!(w["status"], "in_review", "{}", t.f.explain_recent());
    assert_eq!(w["run_id"], run.as_str());
    assert_eq!((w["brief"].as_str(), w["brief_source"].as_str()), (Some(SHORT_BRIEF), Some("worker")));
    assert_eq!(w["brief_truncated"], false);
    assert_eq!(w["result_status"], "completed");
    assert_eq!(w["claim_count"], 1);

    let settled = t
        .events(&task)
        .into_iter()
        .find(|e| e["type"] == "status_change" && e["run_id"] == run.as_str() && e["detail"]["entity"] == "run")
        .unwrap_or_else(|| panic!("no run status_change\n{}", t.f.explain_recent()));
    assert_eq!(settled["detail"]["note"], format!("ingested 1 claim(s): {SHORT_BRIEF}"));
    let r = t.json(&["task", "result", &task, "--run", &run, "--json"]);
    assert_eq!(r["brief"], SHORT_BRIEF);
    assert_eq!(r["summary"], long.as_str(), "the summary is whole");
    assert_same_brief(&t.newest(&task, &run, "result_ingested")["detail"], &r, "event");

    let wr = t.wait_run(&run);
    assert_eq!(wr["role"], "implementer");
    assert!(wr["verdict"].is_null());
    assert_eq!(wr["result_status"], "completed");
    assert_eq!(wr["claim_count"], 1);
    assert_same_brief(&wr, &r, "wait --run");

    // AC8: the MCP tools carry the same keys.
    let mut s = t.mcp();
    let list = s.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let tool = |name: &str| tools.iter().find(|x| x["name"] == name).unwrap_or_else(|| panic!("no {name}")).clone();
    let show_schema = tool("thoth_review_show")["inputSchema"].clone();
    assert_eq!(show_schema["properties"]["full"]["type"], "boolean", "{show_schema}");
    let required = show_schema["required"].as_array().cloned().unwrap_or_default();
    assert!(!required.contains(&json!("full")), "{show_schema}");
    assert_eq!(tool("thoth_spec_review_start")["inputSchema"]["properties"]["edit"]["type"], "boolean");
    // thoth_spec_review_confirm has no `edit`; its existing parameters stay.
    let confirm = tool("thoth_spec_review_confirm")["inputSchema"]["properties"].clone();
    for p in ["task", "profile", "model", "timeout", "effort", "reason", "repo"] {
        assert!(confirm.get(p).is_some(), "thoth_spec_review_confirm keeps {p}: {confirm}");
    }
    assert!(confirm.get("full").is_none(), "{confirm}");
    let m = s.call("thoth_wait", json!({"task": task, "timeout_seconds": 5}));
    for k in ["run_id", "result_status", "claim_count", "brief", "brief_source", "brief_truncated"] {
        assert_eq!(m[k], w[k], "thoth_wait task: {k}");
    }
    let m = s.call("thoth_wait", json!({"run": run, "timeout_seconds": 5}));
    for k in ["result_status", "claim_count", "brief", "brief_source", "brief_truncated"] {
        assert_eq!(m[k], wr[k], "thoth_wait run: {k}");
    }
    let m = s.call("thoth_task_result", json!({"task": task, "run": run}));
    assert_same_brief(&m, &r, "thoth_task_result");
    assert_eq!(m["summary"], long.as_str());

    // A fake reviewer rejects, with a long Unicode note on the unmet criterion.
    let note = "測".repeat(250);
    let started = t.json_with(
        &["review", "start", &task, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--reason", REASON, "--json"],
        &[("FAKE_SCENARIO", "review_reject"), ("FAKE_TASK_ID", &task), ("FAKE_REVIEW_NOTE", &note)],
    );
    let reviewer = started["run_id"].as_str().unwrap().to_string();
    let rv = t.wait_run(&reviewer);
    assert_eq!(rv["verdict"], "REJECT", "{}", t.f.explain_run(&reviewer));
    assert_eq!(rv["role"], "reviewer");
    assert_eq!(rv["brief_source"], "review");
    assert_eq!(rv["brief_truncated"], false);
    assert_null_keys(&rv, &["result_status", "claim_count"]);
    let brief = rv["brief"].as_str().unwrap().to_string();
    assert!(brief.starts_with("REJECT: 1/2 criteria met; AC1 unmet (fix S, "), "{brief}");
    assert!(
        brief.contains(&format!("): {}; 1 extra finding(s); minor (fix S, ", "測".repeat(200))),
        "{brief}"
    );
    assert!(!brief.contains(&"測".repeat(201)), "the note preview is 200 characters");
    assert!(brief.ends_with("): the new test asserts a constant rather than the behaviour"), "{brief}");
    let human = t.ok(&["wait", "--run", &reviewer], &[]);
    assert!(human.contains("\nbrief: REJECT: 1/2 criteria met; AC1 unmet"), "{human}");

    // AC6: review show leads with the same brief; claims only with --full.
    let shown = t.json(&["review", "show", &task, "--json"]);
    assert_eq!(shown["brief"], brief.as_str());
    assert!(shown["claims"].is_null(), "{shown}");
    assert_eq!(shown["verdict"], "REJECT");
    assert_eq!(shown["review_count"], 1);
    assert_eq!(shown["findings"]["criteria"][0]["note"], note.as_str(), "findings are whole");
    assert!(shown["cited"].is_array() && shown["opened"].is_array(), "{shown}");
    let full = t.json(&["review", "show", &task, "--full", "--json"]);
    assert_eq!(full["claims"].as_array().map(|a| a.len()), Some(1), "{full}");
    assert_eq!(full["claims"][0]["kind"], "completion");
    assert_eq!(full["claims"][0]["sources"], json!([]));
    assert_eq!(full["findings"], shown["findings"]);
    let human = t.ok(&["review", "show", &task], &[]);
    assert!(human.contains(&format!("\nbrief: {brief}")), "{human}");
    assert!(!human.contains("  claim  "), "{human}");
    let human = t.ok(&["review", "show", &task, "--full"], &[]);
    assert!(human.contains("  claim  completion"), "{human}");
    for (args, claims) in [(json!({"task": task}), Value::Null), (json!({"task": task, "full": false}), Value::Null),
        (json!({"task": task, "full": true}), full["claims"].clone())] {
        let m = s.call("thoth_review_show", args.clone());
        assert_eq!(m["brief"], brief.as_str(), "{args}");
        assert_eq!(m["claims"], claims, "{args}");
    }
    let m = s.call("thoth_wait", json!({"run": reviewer, "timeout_seconds": 5}));
    for k in ["result_status", "claim_count", "brief", "brief_source", "brief_truncated"] {
        assert_eq!(m[k], rv[k], "thoth_wait reviewer: {k}");
    }
    drop(s);

    // AC4: the newest implementer run has no delivery of its own, so the
    // first run's result does not stand in for it.
    let second = t.dispatch(&task, "claude-b", "sonnet", "crash", &[]);
    assert_ne!(second, run);
    let w = t.json(&["wait", &task, "--timeout", "180s", "--json"]);
    assert_eq!(w["status"], "interrupted", "{}", t.f.explain_recent());
    assert_eq!(w["run_id"], second.as_str());
    assert_null_keys(&w, &["result_status", "claim_count", "brief", "brief_source", "brief_truncated"]);
    let human = t.ok(&["wait", &task], &[]);
    assert!(!human.contains("brief:"), "{human}");
    assert_null_keys(&t.wait_run(&second), &["result_status", "claim_count", "brief", "brief_source", "brief_truncated"]);
}

/// AC4, AC5: a timed-out wait reports every new key as null and still exits
/// 3; a task that never ran has no run to name.
#[test]
fn timeouts_and_a_task_without_runs_report_null_keys() {
    let t = T::new();
    let queued = t.create(NORMAL);
    let w = t.json_exit(&["wait", &queued, "--timeout", "1", "--json"], 3);
    assert_eq!((w["ok"].as_bool(), w["timed_out"].as_bool(), w["status"].as_str()), (Some(false), Some(true), Some("queued")));
    assert_null_keys(&w, &["run_id", "result_status", "claim_count", "brief", "brief_source", "brief_truncated"]);

    let slow = t.dispatch(&queued, "claude-b", "sonnet", "slow", &[]);
    let w = t.json_exit(&["wait", "--run", &slow, "--timeout", "1", "--json"], 3);
    assert_eq!(w["timed_out"], true);
    assert_null_keys(&w, &["verdict", "result_status", "claim_count", "brief", "brief_source", "brief_truncated"]);
    t.ok(&["cancel", &slow, "--json"], &[]);
    assert_eq!(t.wait_run(&slow)["status"], "cancelled");

    let never = t.create(NORMAL);
    t.ok(&["task", "close", &never, "--reason", "never dispatched", "--json"], &[]);
    let w = t.json(&["wait", &never, "--json"]);
    assert_eq!(w["status"], "cancelled");
    assert_null_keys(&w, &["run_id", "result_status", "claim_count", "brief", "brief_source", "brief_truncated"]);
}

/// AC5, AC6, AC9: external reviews: two unmet criteria (one without a note),
/// labels, extra findings, a PASS whose brief is cut at 1,000 characters,
/// and secrets across the 200- and 1,000-character cuts.
#[test]
fn review_briefs_list_unmet_criteria_and_mask_notes_before_cutting() {
    let t = T::with_env(vec![(SECRET_VAR.to_string(), exact_value())]);
    let tok = token();

    // No review yet: brief null, claims null, [] with --full.
    let fresh = t.create(NORMAL);
    let none = t.json(&["review", "show", &fresh, "--json"]);
    assert_null_keys(&none, &["brief", "claims", "verdict"]);
    assert_eq!(t.json(&["review", "show", &fresh, "--full", "--json"])["claims"], json!([]));

    let task = t.create(NORMAL);
    let run = t.dispatch(&task, "claude-b", "sonnet", "ok", &[]);
    t.wait_run(&run);
    let code = t.first_code(&task);
    let unmet_note = format!("{} {tok} tail", "n".repeat(190));
    let extra_note = format!("{}{} tail", "x".repeat(195), exact_value());
    let doc = json!({"verdict": "REJECT", "findings": {
        "criteria": [
            {"id": "AC1", "verdict": "unmet", "evidence": [code], "fix_size": "M"},
            {"id": "AC2", "verdict": "unmet", "note": unmet_note, "category": "docs"}
        ],
        "extra_findings": [
            {"severity": "major", "note": "first extra", "evidence": [], "fix_size": "S", "category": "test_missing"},
            {"severity": "minor", "note": extra_note, "evidence": []}
        ]}});
    let secrets_before = t.count(&task, "secret_redacted");
    t.record_review(&task, &doc);
    let want = format!(
        "REJECT: 0/2 criteria met; AC1 unmet (fix M): (no note); AC2 unmet (docs): {} [redacted; \
         2 extra finding(s); major (fix S, test_missing): first extra; minor: {}[reda",
        "n".repeat(190),
        "x".repeat(195)
    );
    let shown = t.json(&["review", "show", &task, "--json"]);
    assert_eq!(shown["brief"], want.as_str());
    assert_eq!(shown["source"], "external");
    let raw = t.ok(&["review", "show", &task, "--json"], &[]);
    assert!(!raw.contains(&token_piece()) && !raw.contains(&exact_value()), "{raw}");
    let note = shown["findings"]["criteria"][1]["note"].as_str().unwrap();
    assert!(note.contains("[redacted:github-token] tail"), "the whole note is there, masked: {note}");
    let human = t.ok(&["review", "show", &task], &[]);
    assert!(human.contains(&format!("\nbrief: {want}")), "{human}");
    assert!(!human.contains(&token_piece()) && !human.contains(&exact_value()), "{human}");
    let mut s = t.mcp();
    assert_eq!(s.call("thoth_review_show", json!({"task": task}))["brief"], want.as_str());
    drop(s);
    assert_eq!(t.count(&task, "secret_redacted"), secrets_before, "readouts record no secret_redacted");

    // A PASS with five extra findings: the brief is cut at 1,000 characters,
    // and a token right at that cut is a marker first.
    let pass_task = t.create(NORMAL);
    let pass_run = t.dispatch(&pass_task, "claude-b", "sonnet", "ok", &[]);
    t.wait_run(&pass_run);
    let code = t.first_code(&pass_task);
    let mut extras: Vec<Value> = (0..4).map(|_| json!({"severity": "minor", "note": "w".repeat(200), "evidence": []})).collect();
    extras.push(json!({"severity": "minor", "note": format!("{} {tok} {}", "y".repeat(104), "z".repeat(30)), "evidence": []}));
    let doc = json!({"verdict": "PASS", "findings": {
        "criteria": [
            {"id": "AC1", "verdict": "met", "evidence": [code]},
            {"id": "AC2", "verdict": "met", "evidence": [code]}
        ],
        "extra_findings": extras}});
    t.record_review(&pass_task, &doc);
    let brief = t.json(&["review", "show", &pass_task, "--json"])["brief"].as_str().unwrap().to_string();
    assert!(brief.starts_with(&format!("PASS: 2/2 criteria met; 5 extra finding(s); minor: {}", "w".repeat(200))), "{brief}");
    assert_eq!(brief.chars().count(), 1000, "{brief}");
    assert!(brief.ends_with(&format!("; minor: {} [redacte", "y".repeat(104))), "{brief}");
    assert!(!brief.contains(&token_piece()));
}

/// AC5, AC6, AC8, AC9: spec reviewer runs (review, edit, the author's check)
/// and an external spec review with secrets in a location and a note.
#[test]
fn spec_review_briefs_follow_each_mode() {
    let t = T::with_env(vec![(SECRET_VAR.to_string(), exact_value())]);
    let task = t.create(NORMAL);
    let start = |scenario: &str, extra: &[&str]| -> String {
        let mut args = vec!["spec", "review", "start", &task, "--profile", "claude-a", "--model", "haiku", "--reason", REASON, "--json"];
        args.extend_from_slice(extra);
        let v = t.json_with(&args, &[("FAKE_SCENARIO", scenario), ("FAKE_TASK_ID", &task)]);
        v["run_id"].as_str().unwrap().to_string()
    };
    let check = |run: &str, verdict: &str, want: &str| {
        let w = t.wait_run(run);
        assert_eq!(w["role"], "spec_reviewer");
        assert_eq!(w["verdict"], verdict, "{}", t.f.explain_run(run));
        assert_eq!((w["brief"].as_str(), w["brief_source"].as_str()), (Some(want), Some("spec_review")), "{w}");
        assert_eq!(w["brief_truncated"], false);
        assert_null_keys(&w, &["result_status", "claim_count"]);
        let shown = t.json(&["spec", "review", "show", &task, "--json"]);
        assert_eq!(shown["brief"], want, "{shown}");
        let human = t.ok(&["spec", "review", "show", &task], &[]);
        assert!(human.contains(&format!("\nbrief: {want}")), "{human}");
        shown
    };

    let r = start("spec_review_revise", &[]);
    check(&r, "REVISE", "REVISE: 1/2 criteria need revision; AC1 (exists, src/lib.rs:1): fake: the function the spec names does not exist");
    let r = start("spec_review_approve", &[]);
    check(&r, "APPROVE", "APPROVE: 0/2 criteria need revision");
    let r = start("spec_review_edit", &["--edit"]);
    let shown = check(&r, "REVISE", "REVISE: 0/2 criteria need revision; 3 edit(s) proposed");
    assert_eq!(shown["newest"]["mode"], "edit");
    assert_eq!(shown["newest"]["edit_count"], 3);
    assert_eq!(shown["pending_edits"], true);
    assert_eq!(shown["newest"]["document"]["edits"].as_array().map(|a| a.len()), Some(3), "the document is whole");
    assert!(shown["newest"]["proposed_sha256"].is_string());

    let v = t.json_with(
        &["spec", "review", "confirm", &task, "--profile", "claude-a", "--model", "haiku", "--reason", REASON, "--json"],
        &[("FAKE_SCENARIO", "spec_confirm_revise"), ("FAKE_TASK_ID", &task)],
    );
    let confirm = v["run_id"].as_str().unwrap().to_string();
    let shown = check(&confirm, "REVISE", "REVISE: 1 of 3 edits revise");
    assert_eq!(shown["newest"]["mode"], "confirm");
    let human = t.ok(&["spec", "review", "show", &task], &[]);
    assert!(human.contains("author: revise (fake author: keep the original wording of this entry)"), "{human}");

    let mut s = t.mcp();
    assert_eq!(s.call("thoth_spec_review_show", json!({"task": task}))["brief"], "REVISE: 1 of 3 edits revise");
    let m = s.call("thoth_wait", json!({"run": confirm, "timeout_seconds": 5}));
    assert_eq!(m["brief"], "REVISE: 1 of 3 edits revise");
    drop(s);

    // An external spec review: a token in a location and the exact value in
    // a note, each across its 200-character preview.
    let location = format!("src/{} {}", "l".repeat(185), token());
    let note = format!("{}{} tail", "q".repeat(196), exact_value());
    let doc = json!({"verdict": "REVISE", "criteria": [
        {"id": "AC1", "verdict": "ok", "findings": []},
        {"id": "AC2", "verdict": "revise", "findings": [{"check": "verifiable", "location": location, "note": note}]}
    ]});
    let path = t.f.home_path().join("brief-spec-review.json");
    std::fs::write(&path, doc.to_string()).unwrap();
    t.json(&["spec", "review", "record", &task, "-f", &path.display().to_string(), "--json"]);
    let want = format!(
        "REVISE: 1/2 criteria need revision; AC2 (verifiable, src/{} [redacted:): {}[red",
        "l".repeat(185),
        "q".repeat(196)
    );
    let shown = t.json(&["spec", "review", "show", &task, "--json"]);
    assert_eq!(shown["brief"], want.as_str(), "{shown}");
    let raw = t.ok(&["spec", "review", "show", &task, "--json"], &[]);
    assert!(!raw.contains(&token_piece()) && !raw.contains(&exact_value()), "{raw}");
    let human = t.ok(&["spec", "review", "show", &task], &[]);
    assert!(human.contains(&format!("\nbrief: {want}")), "{human}");
}

/// AC9: a worker brief and summary with a secret across the 1,000-character
/// cut read the same, masked first, in every readout.
#[test]
fn a_secret_across_the_cut_is_masked_before_every_brief() {
    let t = T::with_env(vec![(SECRET_VAR.to_string(), exact_value())]);
    let task = t.create(NORMAL);
    let run = t.dispatch(&task, "claude-b", "sonnet", "ok", &[]);
    t.wait_run(&run);
    let secrets_before = t.count(&task, "secret_redacted");

    let cases = [
        // (brief, summary, wanted brief, source)
        (
            Some(json!(format!("{} {} end", "b".repeat(990), token()))),
            "the summary".to_string(),
            format!("{} [redacted", "b".repeat(990)),
            "worker",
        ),
        (
            None,
            format!("{}{} end", "c".repeat(995), exact_value()),
            format!("{}[reda", "c".repeat(995)),
            "summary",
        ),
    ];
    for (brief, summary, want, source) in cases {
        let sub = t.submit(&run, &delivery(&task, &summary, brief), &[]);
        assert_eq!(sub["note"], format!("ingested 1 claim(s): {want}"));

        let readouts = [
            t.json(&["wait", &task, "--json"]),
            t.json(&["wait", "--run", &run, "--json"]),
            t.json(&["task", "result", &task, "--json"]),
            t.newest(&task, &run, "result_ingested")["detail"].clone(),
        ];
        for r in &readouts {
            assert_eq!((r["brief"].as_str(), r["brief_source"].as_str()), (Some(want.as_str()), Some(source)), "{r}");
            assert_eq!(r["brief_truncated"], true);
            assert_eq!(r["brief"].as_str().unwrap().chars().count(), 1000);
        }
        let mut s = t.mcp();
        assert_same_brief(&s.call("thoth_wait", json!({"task": task, "timeout_seconds": 5})), &readouts[0], "mcp wait");
        assert_same_brief(&s.call("thoth_wait", json!({"run": run, "timeout_seconds": 5})), &readouts[1], "mcp wait run");
        assert_same_brief(&s.call("thoth_task_result", json!({"task": task})), &readouts[2], "mcp task result");
        drop(s);

        for args in [
            vec!["wait", &task],
            vec!["wait", "--run", &run],
            vec!["task", "result", &task],
            vec!["wait", &task, "--json"],
            vec!["task", "result", &task, "--json"],
            vec!["events", "--since", "0", "--task", &task, "--json"],
        ] {
            let out = t.ok(&args, &[]);
            assert!(!out.contains(&token_piece()) && !out.contains(&exact_value()), "{args:?}: {out}");
            if !args.contains(&"--json") {
                assert!(out.contains(&format!("brief: {want}\n")), "{args:?}: {out}");
            }
        }
        // The full sources are shown whole, masked.
        let r = &readouts[2];
        let full = format!("{}{}", r["summary"], t.newest(&task, &run, "result_ingested")["detail"]["result_json"]);
        assert!(full.contains("[redacted:github-token]") || full.contains(&format!("[redacted:env:{SECRET_VAR}]")), "{full}");
    }
    assert_eq!(t.count(&task, "secret_redacted"), secrets_before, "readouts record no secret_redacted");
}

/// AC3: every worker prompt asks for a brief before the summary and says
/// what goes in it.
#[test]
fn worker_prompts_ask_for_a_brief_before_the_summary() {
    let t = T::new();
    t.profile("codex-a", "openai", "codex");
    let agy = t.profile("agy-a", "google", "antigravity");
    let agy_data = agy.display().to_string();
    let launches = vec![
        ("claude-b", "sonnet", vec!["--effort", "medium"], vec![]),
        ("codex-a", "gpt-5.6-luna", vec!["--effort", "medium"], vec![]),
        // An agy model whose name ends in its level takes no --effort.
        ("agy-a", "gemini-3.8-flash-low", vec![], vec![("FAKE_AGY_DATA_DIR", agy_data.as_str())]),
    ];
    for (profile, model, effort, extra_env) in launches {
        let task = t.create(NORMAL);
        let mut envs = vec![("FAKE_SCENARIO", "ok"), ("FAKE_TASK_ID", task.as_str())];
        envs.extend(extra_env);
        let mut args = vec!["dispatch", &task, "--profile", profile, "--model", model, "--reason", REASON, "--json"];
        args.extend(effort);
        let d: Value = serde_json::from_str(t.ok(&args, &envs).trim()).unwrap();
        let run = d["run_id"].as_str().unwrap().to_string();
        t.wait_run(&run);
        let status = t.json(&["status", &task, "--json"]);
        let row = status["runs"].as_array().unwrap().iter().find(|r| r["run_id"] == run.as_str()).unwrap().clone();
        let dir = std::path::PathBuf::from(row["run_dir"].as_str().unwrap());
        let prompt = std::fs::read_to_string(dir.join("prompt.txt")).unwrap();
        assert!(
            prompt.contains("\"status\":\"completed\",\"brief\":\"<short conclusion>\",\"summary\":\"<one paragraph>\""),
            "{profile}: {prompt}"
        );
        for words in [
            "`brief` is your short conclusion",
            "name the paths of any documents you wrote",
            "the outcome of your checks in one sentence",
            "any decision you need from the commander",
            "1,000 characters",
            "marked as cut",
            "without a brief the start of your summary is shown instead",
            "Put the full explanation in `summary`",
            "Do not list the files you changed or the commands you ran",
            "already recorded as evidence",
        ] {
            assert!(prompt.contains(words), "{profile}: `{words}` missing:\n{prompt}");
        }
    }
}

/// AC7: a docs, research or review task's delivery stays readable, whole,
/// after `task accept`; a task with no ingested result keeps its error.
#[test]
fn an_accepted_document_delivery_keeps_its_brief_and_full_result() {
    let t = T::new();
    for task_type in ["docs", "research", "review"] {
        let spec = format!(
            r#"{{"goal":"write the {task_type} document","acceptance_criteria":["AC1: the document is written"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low","checks":[],"task_type":"{task_type}"}}"#
        );
        let task = t.create(&spec);
        let scenario = if task_type == "research" { "research_ok" } else { "ok" };
        let run = t.dispatch(&task, "claude-b", "sonnet", scenario, &[("FAKE_RESULT_BRIEF", "Wrote docs/guide.md; nothing to decide.")]);
        assert_eq!(t.wait_run(&run)["task_status"], "in_review", "{task_type}: {}", t.f.explain_recent());
        let before = t.json(&["task", "result", &task, "--json"]);
        let (want, source) = if task_type == "research" {
            ("fake worker completed the task", "summary")
        } else {
            ("Wrote docs/guide.md; nothing to decide.", "worker")
        };
        assert_eq!((before["brief"].as_str(), before["brief_source"].as_str()), (Some(want), Some(source)), "{task_type}");
        let a = t.json(&["task", "accept", &task, "--reason", "read and used", "--json"]);
        assert_eq!(a["status"], "accepted");
        let after = t.json(&["task", "result", &task, "--json"]);
        assert_eq!(after["task_status"], "accepted");
        for k in ["brief", "brief_source", "brief_truncated", "summary", "claims", "claim_count", "run_id", "result_status"] {
            assert_eq!(after[k], before[k], "{task_type}: {k}");
        }
        assert_eq!(t.count(&task, "verdict"), 0, "{task_type}: acceptance is no verdict");
        assert_eq!(t.json(&["review", "show", &task, "--json"])["review_count"], 0);
    }

    let task = t.create(r#"{"goal":"write the docs document","acceptance_criteria":["AC1: the document is written"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low","checks":[],"task_type":"docs"}"#);
    let run = t.dispatch(&task, "claude-b", "sonnet", "bad_result", &[]);
    assert_eq!(t.wait_run(&run)["task_status"], "unverified");
    let (code, _, err) = t.run(&["task", "result", &task, "--json"], &[]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("result_invalid") && err.contains(&format!("thoth events --task {task} --json")), "{err}");
}

impl T {
    /// `json` with extra variables for this one command.
    fn json_with(&self, args: &[&str], envs: &[(&str, &str)]) -> Value {
        let text = self.ok(args, envs);
        serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{args:?}: {e}: {text}"))
    }
}
