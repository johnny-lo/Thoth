//! SR-3: `thoth spec review start --edit` (the reviewer proposes structured
//! edits, Thoth applies them into a proposed spec kept in the REVISE event),
//! `thoth spec review confirm` (the author judges only those edits; an
//! APPROVE replaces the spec), `spec review show`'s `pending_edits` and
//! `--proposed-out`, and the dispatch gate's hint.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

/// Two constraints' worth of lists to edit: the fakes replace the first
/// criterion, append a constraint and delete the last non_goal.
const EDITABLE: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":["no new crate"],"non_goals":["rewriting the transport","changing the log format"],"excluded_alternatives":[],"risk":"important"}"#;
const REWRITTEN: &str = r#"{"goal":"guard the ledger, rejecting a double spend before it is written","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":["no new crate"],"non_goals":["rewriting the transport","changing the log format"],"excluded_alternatives":[],"risk":"important"}"#;
/// The first criterion holds a run of spaces and a newline, and the edit
/// fake-claude builds from it is longer than the 60 characters a human edit
/// line shows.
const WHITESPACE: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a  double\nspend  before the write","AC2: logs the rejection"],"constraints":["no new crate"],"non_goals":["rewriting the transport"],"excluded_alternatives":[],"risk":"low"}"#;
/// A spec review document for `spec review record` on EDITABLE.
const EXTERNAL_APPROVE: &str = r#"{"verdict":"APPROVE","criteria":[{"id":"AC1","verdict":"ok","findings":[]},{"id":"AC2","verdict":"ok","findings":[]}]}"#;

// --- everything below reads Thoth through its CLI, never the database ------

fn parse_stdout(out: Vec<u8>) -> Value {
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// `thoth events --since 0 --task TASK --json`: the task's events, seq order.
fn events(f: &Fixture, task_id: &str) -> Vec<Value> {
    let mut c = f.cmd();
    c.args(["events", "--since", "0", "--task", task_id, "--json"]);
    let v = parse_stdout(f.run_ok(c, "events"));
    v["events"].as_array().unwrap().clone()
}

fn events_of_type(f: &Fixture, task_id: &str, event_type: &str) -> Vec<Value> {
    events(f, task_id)
        .into_iter()
        .filter(|e| e["type"] == event_type)
        .collect()
}

/// The task's event types in seq order (what `events --json` lists).
fn event_types(f: &Fixture, task_id: &str) -> Vec<String> {
    events(f, task_id).iter().map(|e| s(&e["type"])).collect()
}

fn count_events(f: &Fixture, task_id: &str, event_type: &str) -> usize {
    events_of_type(f, task_id, event_type).len()
}

/// (`run_id`, `detail`) of the newest event of `event_type`.
fn last_event(f: &Fixture, task_id: &str, event_type: &str) -> (Option<String>, Value) {
    let e = events_of_type(f, task_id, event_type)
        .pop()
        .unwrap_or_else(|| panic!("task {task_id} has no {event_type} event\n{}", f.explain_recent()));
    (e["run_id"].as_str().map(str::to_string), e["detail"].clone())
}

/// `thoth status TASK --json`.
fn status_json(f: &Fixture, task_id: &str) -> Value {
    let mut c = f.cmd();
    c.args(["status", task_id, "--json"]);
    parse_stdout(f.run_ok(c, "status"))
}

fn task_status(f: &Fixture, task_id: &str) -> String {
    s(&status_json(f, task_id)["status"])
}

/// The task's runs as `status --json` lists them.
fn runs(f: &Fixture, task_id: &str) -> Vec<Value> {
    status_json(f, task_id)["runs"].as_array().unwrap().clone()
}

/// The task's spec bytes as stored, read back through `thoth task show`
/// (its human output prints them after `spec:`, then one newline).
fn stored_spec(f: &Fixture, task_id: &str) -> String {
    let mut c = f.cmd();
    c.args(["task", "show", task_id]);
    let text = String::from_utf8(f.run_ok(c, "task show")).unwrap();
    let (_, spec) = text
        .split_once("  spec:\n")
        .unwrap_or_else(|| panic!("task show prints no spec: {text}"));
    spec.strip_suffix('\n').unwrap_or(spec).to_string()
}

fn write_json(f: &Fixture, name: &str, contents: &str) -> PathBuf {
    let p = f.home_path().join(name);
    std::fs::write(&p, contents).unwrap();
    p
}

fn show_json(f: &Fixture, task: &str) -> Value {
    let mut c = f.cmd();
    c.args(["spec", "review", "show", task, "--json"]);
    let out = f.run_ok(c, "spec review show");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn dispatch_err(f: &Fixture, task: &str, extra: &[&str]) -> String {
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "crash")
        .env("FAKE_TASK_ID", task)
        .args(["dispatch", task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .args(extra)
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(out).unwrap()
}

fn run_count(f: &Fixture, task: &str) -> usize {
    runs(f, task).len()
}

fn implementer_run_count(f: &Fixture, task: &str) -> usize {
    runs(f, task).iter().filter(|r| r["role"] == "implementer").count()
}

fn sha(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn sha_file(p: &Path) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(std::fs::read(p).unwrap())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn update_spec_json(f: &Fixture, task: &str, file: &Path) -> Value {
    let mut c = f.cmd();
    c.args(["task", "update-spec", task, "-f"]).arg(file).arg("--json");
    parse_stdout(f.run_ok(c, "task update-spec"))
}

/// Makes the task `failed` the way a task becomes failed: its implementer
/// runs past its timeout (the same route as tests/handoff.rs). The spec
/// review gate is skipped for that one dispatch (the newest spec review is
/// an edit REVISE), which writes no `spec_review` event, so the pending
/// edits are still pending afterwards.
fn fail_task(f: &Fixture, task: &str) {
    f.dispatch(
        task,
        "claude-b",
        "slow",
        &["--timeout", "3s", "--skip-spec-review", "fake: let the implementer time out"],
    );
    let (_, status) = f.wait_for(task, "120s");
    assert_eq!(status, "failed", "{}", f.explain_recent());
}

trait AgyFixture {
    fn add_agy_profile(&self, id: &str) -> std::path::PathBuf;
}

impl AgyFixture for common::Fixture {
    fn add_agy_profile(&self, id: &str) -> std::path::PathBuf {
        let cfg = self.home.path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        self.cmd().args(["profile", "add", id, "--provider", "google", "--harness",
            "antigravity", "--config-dir"]).arg(&cfg).assert().success();
        cfg
    }
}

fn agy_data(f: &common::Fixture) -> std::path::PathBuf {
    f.home_path().join("profiles").join("agy-a")
}

fn standard() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    f.add_codex_profile("codex-a");
    f
}

fn launch_cmd(
    f: &Fixture,
    verb: &str,
    task: &str,
    profile: &str,
    scenario: &str,
    extra: &[&str],
    envs: &[(&str, &str)],
) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task)
        .args(["spec", "review", verb, task, "--profile", profile])
        .args(extra);
    for (k, v) in envs {
        c.env(k, v);
    }
    c
}

/// `spec review start TASK --profile P --json` plus `extra`, exit 0.
fn spec_start(f: &Fixture, task: &str, profile: &str, scenario: &str, extra: &[&str], envs: &[(&str, &str)]) -> Value {
    let mut c = launch_cmd(f, "start", task, profile, scenario, extra, envs);
    c.arg("--json");
    let out = f.run_ok(c, "spec review start");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// `spec review confirm TASK --profile P --json` plus `extra`, exit 0.
fn spec_confirm(f: &Fixture, task: &str, profile: &str, scenario: &str, extra: &[&str], envs: &[(&str, &str)]) -> Value {
    let mut c = launch_cmd(f, "confirm", task, profile, scenario, extra, envs);
    c.arg("--json");
    let out = f.run_ok(c, "spec review confirm");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// `spec review confirm` that must fail; returns its stderr.
fn spec_confirm_err(f: &Fixture, task: &str, profile: &str, extra: &[&str]) -> String {
    let out = launch_cmd(f, "confirm", task, profile, "crash", extra, &[])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    String::from_utf8(out).unwrap()
}

fn wait_run(f: &Fixture, run: &str) -> Value {
    let mut c = f.cmd();
    c.args(["wait", "--run", run, "--timeout", "180s", "--json"]);
    let out = f.run_ok(c, "wait --run");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// One run's status as `status TASK --json` lists it.
fn run_status(f: &Fixture, task: &str, run: &str) -> String {
    let rows = runs(f, task);
    let row = rows
        .iter()
        .find(|r| r["run_id"] == run)
        .unwrap_or_else(|| panic!("status {task} lists no run {run}: {rows:?}"));
    s(&row["status"])
}

fn wait_run_status(f: &Fixture, task: &str, run: &str, wanted: &str, secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let s = run_status(f, task, run);
        if s == wanted {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "run {run} never became '{wanted}'; it is '{s}'\n{}",
            f.explain_run(run)
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap().to_string()
}

/// `spec review show TASK --proposed-out FILE`: (exit code, stderr).
fn proposed_out(f: &Fixture, task: &str, file: &Path) -> (i32, String) {
    let out = f
        .cmd()
        .args(["spec", "review", "show", task, "--proposed-out"])
        .arg(file)
        .output()
        .unwrap();
    (out.status.code().unwrap_or(-1), String::from_utf8(out.stderr).unwrap())
}

/// The ids before the first `:` of each acceptance criterion.
fn ac_heads(spec: &Value) -> Vec<String> {
    spec["acceptance_criteria"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().split(':').next().unwrap().trim().to_string())
        .collect()
}

/// The edit-mode REVISE of fake-claude on EDITABLE: (run id, the spec_review
/// event's detail).
fn edit_review(f: &Fixture, task: &str, profile: &str, scenario: &str, extra: &[&str], envs: &[(&str, &str)]) -> (String, Value) {
    let mut args = vec!["--edit"];
    args.extend_from_slice(extra);
    let r = spec_start(f, task, profile, scenario, &args, envs);
    assert_eq!(r["mode"], "edit", "{r}");
    let run = s(&r["run_id"]);
    let w = wait_run(f, &run);
    assert_eq!(w["verdict"], "REVISE", "{}", f.explain_run(&run));
    let (ev_run, d) = last_event(f, task, "spec_review");
    assert_eq!(ev_run.as_deref(), Some(run.as_str()));
    (run, d)
}

// --- the MCP server, with extra environment --------------------------------

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &common::Fixture, envs: &[(&str, &str)]) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("FAKE_TASK_ID")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in envs {
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
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        v
    }
    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
    }
    fn wait_run(&mut self, from_id: u64, run: &str) -> Value {
        let mut waited = Value::Null;
        for i in 0..8 {
            waited = self.call(from_id + i, "thoth_wait", json!({"run":run,"timeout_seconds":25}));
            if waited["result"]["structuredContent"]["timed_out"] == false {
                break;
            }
        }
        assert_eq!(waited["result"]["structuredContent"]["timed_out"], false, "{waited}");
        waited["result"]["structuredContent"].clone()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// --- the tests ----------------------------------------------------------------

/// AC1, AC3, AC4, AC5, AC6 (the APPROVE path): a Claude edit review, the
/// proposed spec taken out, the gate's hint, the author's APPROVE replacing
/// the spec, and the dispatch that follows.
#[test]
fn a_claude_edit_review_and_the_authors_approve_replace_the_spec() {
    let f = standard();
    let (task, _) = f.create_task(EDITABLE);

    // The run itself is a spec review in edit mode.
    let r = spec_start(&f, &task, "claude-a", "spec_review_edit", &["--model", "haiku", "--edit"], &[]);
    assert_eq!(r["mode"], "edit");
    assert_eq!(r["spec_sha256"], sha(EDITABLE));
    let run = s(&r["run_id"]);
    let run_dir = PathBuf::from(s(&r["run_dir"]));
    let w = wait_run(&f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["verdict"], "REVISE");
    assert_eq!(w["task_status"], "queued");
    let (_, started) = last_event(&f, &task, "spec_review_started");
    assert_eq!(started["mode"], "edit");
    let schema: Value =
        serde_json::from_str(&std::fs::read_to_string(run_dir.join("spec-review-schema.json")).unwrap()).unwrap();
    assert!(schema["properties"]["edits"].is_object(), "{schema}");
    assert_eq!(schema["required"], json!(["verdict", "criteria", "edits"]));
    let prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    assert!(prompt.contains("## You may edit the spec"), "{prompt}");

    // AC3: the REVISE carries the applied edits and the proposed spec.
    let (ev_run, d) = last_event(&f, &task, "spec_review");
    assert_eq!(ev_run.as_deref(), Some(run.as_str()));
    assert_eq!(d["verdict"], "REVISE");
    assert_eq!(d["mode"], "edit");
    assert_eq!(d["source"], "reviewer_run");
    assert_eq!(d["spec_sha256"], sha(EDITABLE));
    assert_eq!(d["edit_count"], 3);
    assert_eq!(d["revise_streak"], 1);
    assert_eq!(d["bytes_before"], EDITABLE.len());
    let edits = d["edits"].as_array().unwrap();
    assert_eq!(edits.len(), 3, "{d}");
    assert_eq!(edits[0]["index"], 0);
    assert_eq!(edits[0]["field"], "acceptance_criteria");
    assert_eq!(edits[0]["op"], "replace");
    assert_eq!(edits[0]["at"], 0);
    assert_eq!(edits[0]["criterion"], "AC1");
    assert_eq!(edits[0]["before"], "AC1: rejects a double spend");
    assert!(s(&edits[0]["after"]).starts_with("AC1: rejects a double spend"), "{d}");
    assert_eq!(edits[1]["op"], "insert");
    assert!(edits[1]["before"].is_null());
    assert_eq!(edits[2]["op"], "delete");
    assert!(edits[2]["after"].is_null());
    assert_eq!(edits[2]["before"], "changing the log format");
    assert_eq!(d["document"]["edits"].as_array().unwrap().len(), 3, "the reviewer's own document is kept");
    let proposed_text = s(&d["proposed_spec"]);
    assert_eq!(d["proposed_sha256"], sha(&proposed_text));
    assert_eq!(d["bytes_after"], proposed_text.len());
    let proposed: Value = serde_json::from_str(&proposed_text).unwrap();
    let original: Value = serde_json::from_str(EDITABLE).unwrap();
    assert_eq!(proposed["constraints"].as_array().unwrap().len(), 2);
    assert_eq!(proposed["non_goals"].as_array().unwrap().len(), 1);
    assert_eq!(proposed["acceptance_criteria"].as_array().unwrap().len(), 2);
    assert_eq!(ac_heads(&proposed), ac_heads(&original));
    assert_eq!(proposed["risk"], "important");
    assert!(proposed_text.starts_with("{\n  \"goal\": "), "{proposed_text}");

    // AC4: show, the proposed spec written out, and the gate's hint.
    let shown = show_json(&f, &task);
    assert_eq!(shown["revise_streak"], 1);
    assert_eq!(shown["approved"], false);
    assert_eq!(shown["pending_edits"], true);
    assert_eq!(shown["spec_sha256"], sha(EDITABLE));
    assert_eq!(shown["newest"]["mode"], "edit");
    assert_eq!(shown["newest"]["edit_count"], 3);
    assert_eq!(shown["newest"]["proposed_sha256"], d["proposed_sha256"]);
    let edit_id = s(&shown["newest"]["spec_review_id"]);
    let human = String::from_utf8(
        f.cmd().args(["spec", "review", "show", &task]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(human.contains("edit 0  acceptance_criteria[replace 0]  AC1"), "{human}");
    assert!(human.contains("edit 2  non_goals[delete 1]"), "{human}");
    assert!(human.contains("await the author check"), "{human}");
    let out_file = f.home_path().join("proposed.json");
    let (code, err) = proposed_out(&f, &task, &out_file);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains(&format!("wrote {} ({} bytes)", out_file.display(), proposed_text.len())), "{err}");
    assert_eq!(sha_file(&out_file), s(&d["proposed_sha256"]));
    let refused = dispatch_err(&f, &task, &[]);
    assert!(refused.contains("has no APPROVE spec review (the newest spec review "), "{refused}");
    assert!(
        refused.contains(&format!(
            " is REVISE; its 3 edits await the author check: thoth spec review confirm {task} --profile P --model M --effort E)"
        )),
        "{refused}"
    );
    assert_eq!(implementer_run_count(&f, &task), 0);

    // AC5: the author's check starts in the same worktree.
    let c = spec_confirm(&f, &task, "claude-a", "spec_confirm_approve", &["--model", "haiku"], &[]);
    assert_eq!(c["mode"], "confirm");
    assert_eq!(c["confirms_spec_review_id"], edit_id);
    assert_eq!(c["proposed_sha256"], d["proposed_sha256"]);
    assert_eq!(c["worktree"], r["worktree"]);
    assert_eq!(c["worktree_reused"], true);
    assert_eq!(c["cleared_stale_report"], true);
    let confirm_run = s(&c["run_id"]);
    assert_ne!(confirm_run, run);
    let (_, started) = last_event(&f, &task, "spec_review_started");
    assert_eq!(started["mode"], "confirm");
    assert_eq!(started["confirms_spec_review_id"], edit_id);
    assert_eq!(started["proposed_sha256"], d["proposed_sha256"]);
    let (_, choice) = last_event(&f, &task, "launch_choice");
    assert_eq!(choice["source"], "spec_review_confirm");
    let w = wait_run(&f, &confirm_run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&confirm_run));
    assert_eq!(w["verdict"], "APPROVE");
    assert_eq!(w["role"], "spec_reviewer");
    let input = PathBuf::from(s(&c["worktree"])).join(".agent").join("spec-review-input");
    let edits_file: Value = serde_json::from_str(&std::fs::read_to_string(input.join("edits.json")).unwrap()).unwrap();
    assert_eq!(edits_file, d["edits"]);
    assert_eq!(std::fs::read(input.join("proposed.json")).unwrap(), proposed_text.as_bytes());
    assert_eq!(std::fs::read(input.join("spec.json")).unwrap(), EDITABLE.as_bytes());
    let confirm_prompt = std::fs::read_to_string(PathBuf::from(s(&c["run_dir"])).join("prompt.txt")).unwrap();
    assert!(confirm_prompt.starts_with("You are the author of this task's spec"), "{confirm_prompt}");
    assert!(!confirm_prompt.contains("rejects a double spend"), "{confirm_prompt}");
    let confirm_schema: Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(s(&c["run_dir"])).join("spec-review-schema.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(confirm_schema["required"], json!(["verdict", "edits"]));

    // AC6: the spec is replaced, the APPROVE is bound to the new bytes.
    let shown = show_json(&f, &task);
    assert_eq!(shown["approved"], true, "{shown}");
    assert_eq!(shown["revise_streak"], 0);
    assert_eq!(shown["stalled"], false);
    assert_eq!(shown["pending_edits"], false);
    assert_eq!(shown["spec_sha256"], d["proposed_sha256"]);
    assert_eq!(shown["review_count"], 2);
    assert_eq!(shown["newest"]["mode"], "confirm");
    assert_eq!(shown["newest"]["source"], "author_check");
    assert_eq!(shown["newest"]["current"], true);
    assert_eq!(shown["newest"]["edit_count"], 3);
    assert_eq!(shown["newest"]["document"]["verdict"], "APPROVE");
    assert_eq!(shown["newest"]["document"]["edits"].as_array().unwrap().len(), 3);
    assert_eq!(stored_spec(&f, &task), proposed_text);
    assert_eq!(task_status(&f, &task), "queued");
    assert_eq!(count_events(&f, &task, "spec_updated"), 1);
    let (upd_run, upd) = last_event(&f, &task, "spec_updated");
    assert_eq!(upd_run.as_deref(), Some(confirm_run.as_str()));
    assert_eq!(upd["source"], "author_check");
    assert_eq!(upd["spec_sha256"], d["proposed_sha256"]);
    assert_eq!(upd["confirms_spec_review_id"], edit_id);
    assert_eq!(upd["spec_review_id"], shown["newest"]["spec_review_id"]);
    assert_eq!(upd["risk_from"], "important");
    assert_eq!(upd["risk_to"], "important");
    assert_eq!(upd["goal_changed"], false);
    assert_eq!(upd["acceptance_criteria_to"], 2);
    assert_eq!(upd["bytes"], proposed_text.len());
    let (_, approve) = last_event(&f, &task, "spec_review");
    assert_eq!(approve["verdict"], "APPROVE");
    assert_eq!(approve["mode"], "confirm");
    assert_eq!(approve["source"], "author_check");
    assert_eq!(approve["reviewer_run_id"], confirm_run);
    assert_eq!(approve["spec_sha256"], d["proposed_sha256"]);
    assert_eq!(approve["spec_sha256_before"], sha(EDITABLE));
    assert_eq!(approve["confirms_spec_review_id"], edit_id);
    assert_eq!(approve["applied"], true);
    assert_eq!(approve["revise_streak"], 0);
    assert_eq!(approve["stalled"], false);
    assert_eq!(approve["findings"], 0);
    assert_eq!(
        approve["criteria"],
        json!([{"id":"AC1","verdict":"ok","findings":0},{"id":"AC2","verdict":"ok","findings":0}])
    );
    assert_eq!(approve["edits"], d["edits"]);
    assert_eq!(approve["edit_count"], 3);
    assert_eq!(approve["proposed_spec"], d["proposed_spec"]);
    assert_eq!(approve["proposed_sha256"], d["proposed_sha256"]);
    assert_eq!(approve["bytes_before"], EDITABLE.len());
    assert_eq!(approve["bytes_after"], proposed_text.len());
    assert_eq!(count_events(&f, &task, "spec_review_invalid"), 0);
    // The human output of an author's check: one line per criterion (from
    // the summary Thoth wrote), then one per edit with the author's answer.
    let human = String::from_utf8(
        f.cmd().args(["spec", "review", "show", &task]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(human.contains("  AC1    ok      0 finding(s)"), "{human}");
    assert!(human.contains("  AC2    ok      0 finding(s)"), "{human}");
    assert!(human.contains("edit 0  acceptance_criteria[replace 0]  AC1"), "{human}");
    assert!(human.contains("author: ok"), "{human}");
    assert!(human.find("  AC2    ok").unwrap() < human.find("edit 0  ").unwrap(), "{human}");
    assert!(!human.contains("await the author check"), "{human}");

    // The gate lets the task go.
    let disp = f.dispatch(&task, "claude-b", "crash", &[]);
    assert_eq!(disp["spec_review"]["approved"], true, "{disp}");
    assert_eq!(disp["spec_review"]["required"], true);
    assert_eq!(disp["spec_review"]["spec_review_id"], shown["newest"]["spec_review_id"]);
    f.wait_for(&task, "120s");
}

/// AC6 (the REVISE path) and AC4: a codex edit review, an agy author who
/// refuses the first edit; the spec stays, the proposed spec is still
/// available to the commander.
#[test]
fn a_codex_edit_review_and_an_agy_authors_revise_keep_the_spec() {
    let f = standard();
    f.add_agy_profile("agy-a");
    let (task, _) = f.create_task(EDITABLE);
    let (run, d) = edit_review(
        &f,
        &task,
        "codex-a",
        "spec_review_edit",
        &["--model", "gpt-5.6-sol", "--effort", "high"],
        &[],
    );
    assert_eq!(d["edit_count"], 3, "{d}");
    assert_eq!(d["document"]["criteria"].as_array().unwrap().len(), 2);
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(
            PathBuf::from(s(&last_event(&f, &task, "spec_review_started").1["worktree"]))
                .join(".agent")
                .join("spec-review.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(report["edits"].as_array().unwrap().len(), 3, "codex handed the document through -o");
    let shown = show_json(&f, &task);
    assert_eq!(shown["pending_edits"], true, "{shown}");
    assert_eq!(shown["revise_streak"], 1);
    let edit_id = s(&shown["newest"]["spec_review_id"]);

    let data = agy_data(&f).display().to_string();
    let c = spec_confirm(
        &f,
        &task,
        "agy-a",
        "spec_confirm_revise",
        &["--model", "gemini-3.8-flash-high"],
        &[("FAKE_AGY_DATA_DIR", &data)],
    );
    assert_eq!(c["mode"], "confirm");
    // AGY-1 (master): `read_file` beside `view_file`; still no write or shell name.
    assert_eq!(c["allowed_tools"], "read_file,view_file,list_dir,find_by_name,grep_search,finish,wait,wait_5_seconds");
    let confirm_run = s(&c["run_id"]);
    let w = wait_run(&f, &confirm_run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&confirm_run));
    assert_eq!(w["verdict"], "REVISE");

    let shown = show_json(&f, &task);
    assert_eq!(shown["approved"], false);
    assert_eq!(shown["revise_streak"], 2);
    assert_eq!(shown["stalled"], false);
    assert_eq!(shown["pending_edits"], false, "an author's REVISE is not an edit review");
    assert_eq!(shown["spec_sha256"], sha(EDITABLE));
    assert_eq!(shown["newest"]["mode"], "confirm");
    assert_eq!(shown["newest"]["verdict"], "REVISE");
    assert_eq!(shown["newest"]["source"], "author_check");
    assert_eq!(shown["newest"]["reviewer_run_id"], confirm_run);
    assert_eq!(shown["newest"]["document"]["edits"][0]["verdict"], "revise");
    assert_eq!(
        shown["newest"]["document"]["edits"][0]["note"],
        "fake author: keep the original wording of this entry"
    );
    assert_eq!(shown["newest"]["document"]["edits"][1]["verdict"], "ok");
    assert_eq!(shown["newest"]["proposed_sha256"], d["proposed_sha256"]);
    let (_, revise) = last_event(&f, &task, "spec_review");
    assert_eq!(revise["applied"], false);
    assert_eq!(revise["spec_sha256"], sha(EDITABLE));
    assert_eq!(revise["spec_sha256_before"], sha(EDITABLE));
    assert_eq!(revise["confirms_spec_review_id"], edit_id);
    assert_eq!(revise["findings"], 1);
    assert_eq!(
        revise["criteria"],
        json!([{"id":"AC1","verdict":"revise","findings":1},{"id":"AC2","verdict":"ok","findings":0}])
    );
    assert_eq!(revise["edits"], d["edits"]);
    assert_eq!(revise["proposed_spec"], d["proposed_spec"]);
    assert_eq!(count_events(&f, &task, "spec_updated"), 0);
    assert_eq!(stored_spec(&f, &task), EDITABLE);
    assert_eq!(task_status(&f, &task), "queued");
    let human = String::from_utf8(
        f.cmd().args(["spec", "review", "show", &task]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(human.contains("  AC1    revise  1 finding(s)"), "{human}");
    assert!(human.contains("  AC2    ok      0 finding(s)"), "{human}");
    assert!(human.contains("author: revise (fake author: keep the original wording of this entry)"), "{human}");
    assert!(human.find("  AC2    ok").unwrap() < human.find("edit 0  ").unwrap(), "{human}");

    // AC4: the proposed spec of the edit review is still there to take.
    let out_file = f.home_path().join("proposed-after-revise.json");
    let (code, err) = proposed_out(&f, &task, &out_file);
    assert_eq!(code, 0, "{err}");
    assert_eq!(sha_file(&out_file), s(&d["proposed_sha256"]));
    // The gate names a plain REVISE now: nothing awaits the author.
    let refused = dispatch_err(&f, &task, &[]);
    assert!(refused.contains(" is REVISE)"), "{refused}");
    assert!(!refused.contains("await the author check"), "{refused}");
    let _ = run;
}

/// AC4, AC5, AC6: what `confirm` and `--proposed-out` refuse, writing nothing.
#[test]
fn confirm_refuses_without_pending_edits_a_changed_spec_and_a_failed_task() {
    let f = standard();
    let (task, _) = f.create_task(EDITABLE);

    // No spec review at all.
    let err = spec_confirm_err(&f, &task, "claude-a", &["--model", "haiku"]);
    assert!(err.contains("has no spec review"), "{err}");
    assert!(err.contains("--edit"), "{err}");
    assert_eq!(run_count(&f, &task), 0);
    assert_eq!(event_types(&f, &task), ["task_created"]);
    let missing = f.home_path().join("none.json");
    let (code, err) = proposed_out(&f, &task, &missing);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("has no proposed spec to take"), "{err}");
    assert!(!missing.exists());

    // A review-mode REVISE: no edits to confirm.
    let r = spec_start(&f, &task, "claude-a", "spec_review_revise", &["--model", "haiku"], &[]);
    assert_eq!(r["mode"], "review");
    wait_run(&f, &s(&r["run_id"]));
    assert_eq!(show_json(&f, &task)["pending_edits"], false);
    let err = spec_confirm_err(&f, &task, "claude-a", &["--model", "haiku"]);
    assert!(err.contains("is not an edit review with edits"), "{err}");
    assert!(err.contains("mode review"), "{err}");
    assert_eq!(run_count(&f, &task), 1);
    let (code, err) = proposed_out(&f, &task, &missing);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("proposes no edits"), "{err}");
    assert!(!missing.exists());

    // Edits proposed, then the spec rewritten by hand: the edits are stale.
    let (_, d) = edit_review(&f, &task, "claude-a", "spec_review_edit", &["--model", "haiku"], &[]);
    assert_eq!(show_json(&f, &task)["pending_edits"], true);
    let rewritten = write_json(&f, "rewritten.json", REWRITTEN);
    update_spec_json(&f, &task, &rewritten);
    let shown = show_json(&f, &task);
    assert_eq!(shown["pending_edits"], false);
    assert_eq!(shown["newest"]["current"], false);
    let runs_before = run_count(&f, &task);
    let err = spec_confirm_err(&f, &task, "claude-a", &["--model", "haiku"]);
    assert!(err.contains("spec changed after spec review"), "{err}");
    assert!(err.contains("--edit"), "{err}");
    assert_eq!(run_count(&f, &task), runs_before);
    let (code, err) = proposed_out(&f, &task, &missing);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("judged an older spec"), "{err}");
    assert!(!missing.exists());
    let _ = d;

    // A failed task: `start --edit` may run, `confirm` may not.
    let (task2, _) = f.create_task(EDITABLE);
    edit_review(&f, &task2, "claude-a", "spec_review_edit", &["--model", "haiku"], &[]);
    fail_task(&f, &task2);
    assert_eq!(show_json(&f, &task2)["pending_edits"], true, "the crash wrote no spec review");
    let runs_before = run_count(&f, &task2);
    let err = spec_confirm_err(&f, &task2, "claude-a", &["--model", "haiku"]);
    assert!(err.contains("is 'failed'; spec review confirm needs draft, queued, reject or unverified"), "{err}");
    assert_eq!(run_count(&f, &task2), runs_before);
    assert_eq!(task_status(&f, &task2), "failed");

    // Inside a run, and `auto`: refused like `start`, before the status is
    // looked at.
    let out = launch_cmd(&f, "confirm", &task2, "claude-a", "crash", &["--model", "haiku"], &[])
        .env("THOTH_RUN_ID", "X")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("spec review confirm refuses to run with THOTH_RUN_ID set"), "{err}");
    let err = spec_confirm_err(&f, &task2, "auto", &["--model", "haiku"]);
    assert!(err.contains("auto"), "{err}");
    assert_eq!(run_count(&f, &task2), runs_before);
}

/// AC7: an edit that makes the spec too large is `spec_review_invalid` and
/// leaves no `spec_review`. SR-4 changed the first half: an edit on a field
/// that cannot be edited used to refuse the whole review at stage `edits`;
/// now it is refused alone, and the REVISE (here with no edit left) is kept
/// with that refusal in `refused_edits`. The oversized proposal keeps its
/// whole-review refusal at stage `lint`.
#[test]
fn bad_and_oversized_edits_are_refused_without_a_spec_review_event() {
    let f = standard();
    let (task, _) = f.create_task(EDITABLE);

    let r = spec_start(&f, &task, "claude-a", "spec_review_edit_bad", &["--model", "haiku", "--edit"], &[]);
    let run = s(&r["run_id"]);
    let w = wait_run(&f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["verdict"], "REVISE");
    assert_eq!(count_events(&f, &task, "spec_review_invalid"), 0);
    let (ev_run, d) = last_event(&f, &task, "spec_review");
    assert_eq!(ev_run.as_deref(), Some(run.as_str()));
    assert_eq!(d["mode"], "edit");
    assert_eq!(d["edit_count"], 0);
    assert_eq!(d["edits"], json!([]));
    assert!(d["proposed_spec"].is_null(), "{d}");
    assert_eq!(
        d["refused_edits"],
        json!([{"index": 0, "field": "risk", "reason":
            "field must be one of goal, acceptance_criteria, constraints, non_goals, excluded_alternatives"}])
    );
    assert_eq!(task_status(&f, &task), "queued");
    let shown = show_json(&f, &task);
    assert_eq!(shown["review_count"], 1);
    assert_eq!(shown["pending_edits"], false);
    assert_eq!(stored_spec(&f, &task), EDITABLE);

    let r = spec_start(&f, &task, "claude-a", "spec_review_edit_big", &["--model", "haiku", "--edit"], &[]);
    let run = s(&r["run_id"]);
    let w = wait_run(&f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["verdict"], Value::Null);
    assert_eq!(count_events(&f, &task, "spec_review_invalid"), 1);
    let (_, d) = last_event(&f, &task, "spec_review_invalid");
    assert_eq!(d["stage"], "lint");
    assert!(s(&d["reason"]).contains("too_large"), "{d}");
    assert_eq!(count_events(&f, &task, "spec_review"), 1);
    assert_eq!(task_status(&f, &task), "queued");
    let refused = dispatch_err(&f, &task, &[]);
    assert!(refused.contains(" is REVISE)"), "{refused}");
}

/// AC6: an edit REVISE, an author's REVISE and another edit REVISE are three
/// in a row; the author's APPROVE ends the streak.
#[test]
fn edit_and_author_revises_count_toward_the_streak_and_an_approve_ends_it() {
    let f = standard();
    let (task, _) = f.create_task(EDITABLE);
    edit_review(&f, &task, "claude-a", "spec_review_edit", &["--model", "haiku"], &[]);
    assert_eq!(show_json(&f, &task)["revise_streak"], 1);

    let c = spec_confirm(&f, &task, "claude-a", "spec_confirm_revise", &["--model", "haiku"], &[]);
    let w = wait_run(&f, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "REVISE", "{}", f.explain_run(&s(&c["run_id"])));
    let shown = show_json(&f, &task);
    assert_eq!(shown["revise_streak"], 2);
    assert_eq!(shown["stalled"], false);
    assert_eq!(shown["pending_edits"], false);

    let (_, d3) = edit_review(&f, &task, "claude-a", "spec_review_edit", &["--model", "haiku"], &[]);
    assert_eq!(d3["revise_streak"], 3);
    assert_eq!(d3["stalled"], true);
    let shown = show_json(&f, &task);
    assert_eq!(shown["stalled"], true);
    assert_eq!(shown["revise_streak"], 3);
    assert_eq!(shown["pending_edits"], true);
    let refused = dispatch_err(&f, &task, &[]);
    assert!(refused.contains("is stuck: its spec was sent back by 3 spec reviews in a row"), "{refused}");
    let refused = dispatch_err(&f, &task, &["--skip-spec-review", "x"]);
    assert!(refused.contains("is stuck"), "{refused}");
    assert_eq!(implementer_run_count(&f, &task), 0);

    let c = spec_confirm(&f, &task, "claude-a", "spec_confirm_approve", &["--model", "haiku"], &[]);
    let w = wait_run(&f, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "APPROVE", "{}", f.explain_run(&s(&c["run_id"])));
    let shown = show_json(&f, &task);
    assert_eq!(shown["stalled"], false);
    assert_eq!(shown["revise_streak"], 0);
    assert_eq!(shown["approved"], true);
    assert_eq!(shown["spec_sha256"], d3["proposed_sha256"]);
    assert_eq!(shown["review_count"], 4);
    let disp = f.dispatch(&task, "claude-b", "crash", &[]);
    assert_eq!(disp["spec_review"]["approved"], true);
    f.wait_for(&task, "120s");
}

/// AC7, AC8: the MCP round trip, and the tool list.
#[test]
fn mcp_edit_review_and_author_confirm() {
    let f = common::fixture();
    f.add_profile("claude-a");
    // One server, two scenarios: the first launch is the edit review, the
    // later ones the author's APPROVE (fake-claude's FAKE_FIRST_SCENARIO).
    let attempts = f.home_path().join("attempts.txt");
    let attempts_s = attempts.display().to_string();
    let mut srv = Server::start(
        &f,
        &[
            ("FAKE_SCENARIO", "spec_confirm_approve"),
            ("FAKE_FIRST_SCENARIO", "spec_review_edit"),
            ("FAKE_FIRST_TIMES", "1"),
            ("FAKE_ATTEMPTS_FILE", &attempts_s),
        ],
    );
    assert_eq!(srv.call(1, "thoth_init", json!({}))["result"]["isError"], false);
    let spec: Value = serde_json::from_str(EDITABLE).unwrap();
    let created = srv.call(2, "thoth_task_create", json!({"title":"gate","spec":spec}));
    let task = s(&created["result"]["structuredContent"]["task_id"]);

    let list = srv.send(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    // thoth_review_confirm sits after thoth_review_start, one place before
    // these.
    // AUTO-1a: no hard-coded total (the pipeline tools were added); every
    // tool is listed once and thoth_guide stays last.
    let unique: std::collections::BTreeSet<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(unique.len(), tools.len());
    assert_eq!(tools.last().unwrap()["name"], "thoth_guide");
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names[20], "thoth_spec_review_start");
    assert_eq!(names[21], "thoth_spec_review_confirm");
    assert_eq!(names[22], "thoth_resume");
    assert_eq!(*names.last().unwrap(), "thoth_guide");
    let tool = |name: &str| tools.iter().find(|t| t["name"] == name).unwrap().clone();
    assert_eq!(tool("thoth_spec_review_start")["inputSchema"]["properties"]["edit"], json!({"type":"boolean"}));
    assert!(!tool("thoth_spec_review_show")["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .contains_key("edit"));
    assert_eq!(tool("thoth_spec_review_show")["annotations"]["readOnlyHint"], true);
    assert_eq!(tool("thoth_spec_review_confirm")["annotations"]["readOnlyHint"], false);
    let confirm_tool = tool("thoth_spec_review_confirm");
    let confirm_params: Vec<&String> = confirm_tool["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .collect();
    assert_eq!(confirm_params, ["effort", "model", "profile", "reason", "repo", "task", "timeout"]);

    let started = srv.call(
        4,
        "thoth_spec_review_start",
        json!({"task":task,"profile":"claude-a","model":"haiku","edit":true}),
    );
    assert_eq!(started["result"]["isError"], false, "{started}");
    assert_eq!(started["result"]["structuredContent"]["mode"], "edit");
    let run = s(&started["result"]["structuredContent"]["run_id"]);
    let waited = srv.wait_run(5, &run);
    assert_eq!(waited["verdict"], "REVISE", "{waited}\n{}", f.explain_run(&run));
    let shown = srv.call(20, "thoth_spec_review_show", json!({"task":task}));
    assert_eq!(shown["result"]["structuredContent"]["pending_edits"], true, "{shown}");
    assert_eq!(shown["result"]["structuredContent"]["newest"]["edit_count"], 3);

    let confirmed = srv.call(
        21,
        "thoth_spec_review_confirm",
        json!({"task":task,"profile":"claude-a","model":"haiku"}),
    );
    assert_eq!(confirmed["result"]["isError"], false, "{confirmed}");
    assert_eq!(confirmed["result"]["structuredContent"]["mode"], "confirm");
    let confirm_run = s(&confirmed["result"]["structuredContent"]["run_id"]);
    let waited = srv.wait_run(22, &confirm_run);
    assert_eq!(waited["verdict"], "APPROVE", "{waited}\n{}", f.explain_run(&confirm_run));
    let shown = srv.call(40, "thoth_spec_review_show", json!({"task":task}));
    assert_eq!(shown["result"]["structuredContent"]["approved"], true, "{shown}");
    assert_eq!(shown["result"]["structuredContent"]["pending_edits"], false);
    assert_eq!(shown["result"]["structuredContent"]["newest"]["source"], "author_check");

    // A non-edit start through MCP is still mode review.
    let (task2, _) = f.create_task(EDITABLE);
    let plain = srv.call(
        41,
        "thoth_spec_review_start",
        json!({"task":task2,"profile":"claude-a","model":"haiku","edit":false}),
    );
    assert_eq!(plain["result"]["isError"], false, "{plain}");
    assert_eq!(plain["result"]["structuredContent"]["mode"], "review");
    srv.wait_run(42, &s(&plain["result"]["structuredContent"]["run_id"]));
}

/// AC7: a cancelled author check leaves the task and the pending edits
/// alone, and the worktree is reused by the next check.
#[test]
fn cancelling_an_author_check_leaves_the_pending_edits_alone() {
    let f = standard();
    let (task, _) = f.create_task(EDITABLE);
    let (_, d) = edit_review(&f, &task, "claude-a", "spec_review_edit", &["--model", "haiku"], &[]);

    let c = spec_confirm(&f, &task, "claude-a", "slow", &["--model", "haiku"], &[]);
    let run = s(&c["run_id"]);
    wait_run_status(&f, &task, &run, "running", 60);
    let mut cancel = f.cmd();
    cancel.args(["cancel", &run, "--json"]);
    f.run_ok(cancel, "cancel");
    wait_run_status(&f, &task, &run, "cancelled", 60);
    assert_eq!(task_status(&f, &task), "queued");
    let shown = show_json(&f, &task);
    assert_eq!(shown["pending_edits"], true, "{shown}");
    assert_eq!(shown["spec_sha256"], sha(EDITABLE));
    assert_eq!(shown["review_count"], 1);
    assert_eq!(count_events(&f, &task, "spec_updated"), 0);
    let out = f
        .cmd()
        .args(["resume", &run])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8(out).unwrap().contains("spec reviews are not resumable"));

    let c2 = spec_confirm(&f, &task, "claude-a", "spec_confirm_approve", &["--model", "haiku"], &[]);
    assert_eq!(c2["worktree_reused"], true);
    assert_eq!(c2["worktree"], c["worktree"]);
    let w = wait_run(&f, &s(&c2["run_id"]));
    assert_eq!(w["verdict"], "APPROVE", "{}", f.explain_run(&s(&c2["run_id"])));
    assert_eq!(show_json(&f, &task)["spec_sha256"], d["proposed_sha256"]);
}

/// `spec review show TASK` without `--json`: the human lines.
fn show_human(f: &Fixture, task: &str) -> String {
    let out = f
        .cmd()
        .args(["spec", "review", "show", task])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    String::from_utf8(out).unwrap()
}

/// AC3, AC4: a review-mode run and `spec review record` say their mode and
/// carry no edits and no proposed spec, in the event and in `show --json`'s
/// `newest`.
#[test]
fn a_review_mode_run_and_a_recorded_review_show_their_mode_without_edits() {
    let f = standard();
    let (task, _) = f.create_task(EDITABLE);
    let no_edits = |d: &Value| {
        assert_eq!(d["edits"], json!([]), "{d}");
        assert_eq!(d["edit_count"], 0, "{d}");
        assert!(d["proposed_spec"].is_null(), "{d}");
        assert!(d["proposed_sha256"].is_null(), "{d}");
        assert_eq!(d["bytes_before"], EDITABLE.len(), "{d}");
        assert_eq!(d["bytes_after"], EDITABLE.len(), "{d}");
    };

    // A start without --edit: mode review.
    let r = spec_start(&f, &task, "claude-a", "spec_review_revise", &["--model", "haiku"], &[]);
    assert_eq!(r["mode"], "review");
    let run = s(&r["run_id"]);
    assert_eq!(wait_run(&f, &run)["verdict"], "REVISE", "{}", f.explain_run(&run));
    let (_, started) = last_event(&f, &task, "spec_review_started");
    assert_eq!(started["mode"], "review");
    let (ev_run, d) = last_event(&f, &task, "spec_review");
    assert_eq!(ev_run.as_deref(), Some(run.as_str()));
    assert_eq!(d["mode"], "review");
    assert_eq!(d["source"], "reviewer_run");
    no_edits(&d);
    let shown = show_json(&f, &task);
    assert_eq!(shown["newest"]["mode"], "review");
    assert_eq!(shown["newest"]["edit_count"], 0);
    assert!(shown["newest"]["proposed_sha256"].is_null(), "{shown}");
    assert_eq!(shown["pending_edits"], false);
    assert_eq!(shown["revise_streak"], 1);
    let human = show_human(&f, &task);
    assert!(!human.contains("edit 0"), "{human}");
    assert!(!human.contains("await the author check"), "{human}");

    // A document recorded from outside: mode external.
    let doc = write_json(&f, "external-approve.json", EXTERNAL_APPROVE);
    let mut c = f.cmd();
    c.args(["spec", "review", "record", &task, "-f"]).arg(&doc).arg("--json");
    let recorded = parse_stdout(f.run_ok(c, "spec review record"));
    assert_eq!(recorded["source"], "external");
    assert_eq!(recorded["verdict"], "APPROVE");
    let (ev_run, d) = last_event(&f, &task, "spec_review");
    assert_eq!(ev_run, None);
    assert_eq!(d["mode"], "external");
    assert_eq!(d["source"], "external");
    no_edits(&d);
    let shown = show_json(&f, &task);
    assert_eq!(shown["newest"]["mode"], "external");
    assert_eq!(shown["newest"]["edit_count"], 0);
    assert!(shown["newest"]["proposed_sha256"].is_null(), "{shown}");
    assert_eq!(shown["pending_edits"], false);
    assert_eq!(shown["approved"], true);
    assert_eq!(shown["review_count"], 2);
    let (code, err) = proposed_out(&f, &task, &f.home_path().join("none.json"));
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("proposes no edits"), "{err}");
}

/// The debug-build switch under which every writer of a `spec_review` event
/// leaves out the keys SR-3 added, as a thoth from before SR-3 did
/// (`review_detail` in src/commands/spec_review.rs).
const PRE_SR3: (&str, &str) = ("THOTH_TEST_SPEC_REVIEW_PRE_SR3", "1");

/// AC4: a `spec_review` event written before SR-3 has no `mode` and none of
/// SR-3's other keys. `show` takes its mode from its `source` - `review` for
/// a reviewer run's, `external` for a recorded one - with no edits and
/// nothing proposed, so it never awaits an author check.
#[test]
fn an_event_without_a_mode_reads_its_mode_from_its_source() {
    let f = standard();
    let (task, _) = f.create_task(EDITABLE);
    let written_before_sr3 = |d: &Value| {
        // SR-4: `refused_edits` is newer still, so such an event lacks it too
        for k in ["mode", "edits", "edit_count", "proposed_spec", "proposed_sha256", "bytes_before", "bytes_after", "refused_edits"] {
            assert!(d.get(k).is_none(), "{k} in {d}");
        }
    };
    let no_edits = |shown: &Value| {
        assert_eq!(shown["newest"]["edit_count"], 0, "{shown}");
        // SR-4 (AC3): an older review reads as no refused edits
        assert_eq!(shown["newest"]["refused_edits"], json!([]), "{shown}");
        assert!(shown["newest"]["proposed_sha256"].is_null(), "{shown}");
        assert_eq!(shown["pending_edits"], false, "{shown}");
    };

    // A reviewer run's REVISE: source reviewer_run, read as mode review.
    let r = spec_start(&f, &task, "claude-a", "spec_review_revise", &["--model", "haiku"], &[PRE_SR3]);
    let run = s(&r["run_id"]);
    assert_eq!(wait_run(&f, &run)["verdict"], "REVISE", "{}", f.explain_run(&run));
    let (ev_run, d) = last_event(&f, &task, "spec_review");
    assert_eq!(ev_run.as_deref(), Some(run.as_str()));
    assert_eq!(d["source"], "reviewer_run");
    written_before_sr3(&d);
    let shown = show_json(&f, &task);
    assert_eq!(shown["newest"]["source"], "reviewer_run");
    assert_eq!(shown["newest"]["mode"], "review", "{shown}");
    no_edits(&shown);
    assert_eq!(shown["revise_streak"], 1);
    let human = show_human(&f, &task);
    assert!(!human.contains("edit 0"), "{human}");
    assert!(!human.contains("await the author check"), "{human}");
    let err = spec_confirm_err(&f, &task, "claude-a", &["--model", "haiku"]);
    assert!(err.contains("is not an edit review with edits"), "{err}");
    assert!(err.contains("mode review"), "{err}");

    // A recorded review: no run, source external, read as mode external.
    let doc = write_json(&f, "external-approve.json", EXTERNAL_APPROVE);
    let mut c = f.cmd();
    c.env(PRE_SR3.0, PRE_SR3.1)
        .args(["spec", "review", "record", &task, "-f"])
        .arg(&doc)
        .arg("--json");
    assert_eq!(parse_stdout(f.run_ok(c, "spec review record"))["source"], "external");
    let (ev_run, d) = last_event(&f, &task, "spec_review");
    assert_eq!(ev_run, None);
    assert_eq!(d["source"], "external");
    written_before_sr3(&d);
    let shown = show_json(&f, &task);
    assert_eq!(shown["newest"]["source"], "external");
    assert_eq!(shown["newest"]["mode"], "external", "{shown}");
    no_edits(&shown);
    assert_eq!(shown["approved"], true);
    assert_eq!(shown["review_count"], 2);
    let (code, err) = proposed_out(&f, &task, &f.home_path().join("none.json"));
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("proposes no edits"), "{err}");
}

/// AC4: the human `show` prints each edit's `after` folded onto one line and
/// cut at 60 characters; a shorter one whole, a delete as `(deleted)`. The
/// event keeps the text as the reviewer wrote it.
#[test]
fn show_folds_and_clips_a_long_edit_text_on_its_human_line() {
    let f = standard();
    let (task, _) = f.create_task(WHITESPACE);
    let (_, d) = edit_review(&f, &task, "claude-a", "spec_review_edit", &["--model", "haiku"], &[]);
    assert_eq!(d["edit_count"], 3, "{d}");
    assert_eq!(
        d["edits"][0]["after"],
        "AC1: rejects a  double\nspend  before the write (fake edit: names the function to call)"
    );
    assert_eq!(d["edits"][1]["after"], "fake edit: do not add a crate");
    assert!(d["edits"][2]["after"].is_null(), "{d}");

    let human = show_human(&f, &task);
    assert!(
        human.contains(
            "  edit 0  acceptance_criteria[replace 0]  AC1  fake: the criterion did not name what to call  \
             AC1: rejects a double spend before the write (fake edit: nam…\n"
        ),
        "{human}"
    );
    assert!(!human.contains("rejects a  double"), "{human}");
    assert!(!human.contains("double\nspend"), "{human}");
    assert!(!human.contains("names the function"), "{human}");
    assert!(
        human.contains("  edit 1  constraints[insert 1]  AC1  fake: the spec never said so  fake edit: do not add a crate\n"),
        "{human}"
    );
    assert!(
        human.contains("  edit 2  non_goals[delete 0]  AC1  fake: this non-goal repeats a constraint  (deleted)\n"),
        "{human}"
    );
    assert_eq!(human.matches('…').count(), 1, "{human}");
    assert!(human.contains("3 edit(s) await the author check"), "{human}");
}

#[test]
fn help_lists_the_edit_flag_confirm_and_proposed_out() {
    let f = common::fixture();
    let help = |args: &[&str]| {
        let out = f.cmd().args(args).assert().success().get_output().stdout.clone();
        String::from_utf8(out).unwrap()
    };
    let spec_review = help(&["spec", "review", "--help"]);
    assert!(spec_review.contains("confirm"), "{spec_review}");
    let start = help(&["spec", "review", "start", "--help"]);
    assert!(start.contains("--edit"), "{start}");
    let confirm = help(&["spec", "review", "confirm", "--help"]);
    for flag in ["--profile", "--model", "--effort", "--reason", "--timeout", "--json"] {
        assert!(confirm.contains(flag), "{confirm}");
    }
    let show = help(&["spec", "review", "show", "--help"]);
    assert!(show.contains("--proposed-out"), "{show}");
}

// --- SR-4: short anchors and edits refused one by one ---------------------
//
// Every command below goes through `sr4_cmd`, which drops each inherited
// variable whose name holds KEY, TOKEN, SECRET or PASSWORD (names only), and
// reads Thoth through its CLI and the files the fakes consume.

/// Five criteria, so a short `AC5: Docs` prefix and a criterion found by a
/// middle substring are both there; constraints with two entries starting
/// with `n` and one holding `zzz` (overlapping `zz`); two non_goals that both
/// hold `the`.
const SR4_SPEC: &str = r#"{"goal":"guard the ledger against a double spend","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection","AC3: exits zero","AC4: names the rejected entry","AC5: Docs describe the rejection in docs/usage.md"],"constraints":["no new crate","nothing else","keep the log format","a zzz marker stays"],"non_goals":["rewriting the transport","renaming the ledger"],"excluded_alternatives":[],"risk":"important"}"#;
const SR4_IDS: [&str; 5] = ["AC1", "AC2", "AC3", "AC4", "AC5"];

fn sr4_cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c
}

fn sr4_ok(f: &Fixture, args: &[&str]) -> Vec<u8> {
    let mut c = sr4_cmd(f);
    c.args(args);
    f.run_ok(c, &args.join(" "))
}

fn sr4_json(f: &Fixture, args: &[&str]) -> Value {
    parse_stdout(sr4_ok(f, args))
}

/// init and one Claude profile, `claude-b`.
fn sr4_fixture() -> Fixture {
    let f = common::fixture();
    sr4_ok(&f, &["init"]);
    let cfg = f.home_path().join("profiles").join("claude-b");
    std::fs::create_dir_all(&cfg).unwrap();
    let cfg = cfg.display().to_string();
    sr4_ok(
        &f,
        &["profile", "add", "claude-b", "--provider", "anthropic", "--harness", "claude-code", "--config-dir", &cfg, "--json"],
    );
    f
}

fn sr4_task(f: &Fixture, spec: &str) -> String {
    let file = write_json(f, "sr4-spec.json", spec);
    let file = file.display().to_string();
    s(&sr4_json(f, &["task", "create", "-f", &file, "--json"])["task_id"])
}

/// The task's stored spec, from `thoth task show TASK`.
fn sr4_task_spec(f: &Fixture, task: &str) -> String {
    let text = String::from_utf8(sr4_ok(f, &["task", "show", task])).unwrap();
    let (_, spec) = text
        .split_once("  spec:\n")
        .unwrap_or_else(|| panic!("task show prints no spec: {text}"));
    spec.strip_suffix('\n').unwrap_or(spec).to_string()
}

fn sr4_show(f: &Fixture, task: &str) -> Value {
    sr4_json(f, &["spec", "review", "show", task, "--json"])
}

fn sr4_show_human(f: &Fixture, task: &str) -> String {
    String::from_utf8(sr4_ok(f, &["spec", "review", "show", task])).unwrap()
}

/// `thoth events --task TASK --json`, the events of run `run` only.
fn sr4_run_events(f: &Fixture, task: &str, run: &str) -> Vec<Value> {
    sr4_json(f, &["events", "--since", "0", "--task", task, "--json"])["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["run_id"] == run)
        .cloned()
        .collect()
}

fn sr4_one(events: &[Value], ty: &str) -> Value {
    let found: Vec<&Value> = events.iter().filter(|e| e["type"] == ty).collect();
    assert_eq!(found.len(), 1, "one {ty} expected: {events:#?}");
    found[0]["detail"].clone()
}

fn sr4_wait(f: &Fixture, run: &str) -> Value {
    sr4_json(f, &["wait", "--run", run, "--timeout", "180s", "--json"])
}

/// One edit; `criterion` AC1 and a reason unless the test overrides them.
fn sr4_edit(field: &str, op: &str, at: usize, anchor: &str, text: &str) -> Value {
    json!({"field": field, "op": op, "at": at, "anchor": anchor, "text": text,
           "criterion": "AC1", "reason": "fixture: says what to change"})
}

/// A REVISE with every criterion of `ids` ok, and `edits`.
fn sr4_doc(ids: &[&str], edits: Vec<Value>) -> Value {
    let criteria: Vec<Value> = ids.iter().map(|id| json!({"id": id, "verdict": "ok", "findings": []})).collect();
    json!({"verdict": "REVISE", "criteria": criteria, "edits": edits})
}

/// `spec review start TASK --edit` (or `confirm TASK`) with the fake handing
/// in `doc` as it is; the prompt it was handed goes to `prompt.txt` in the
/// fixture's home under `name`. Returns the start's `--json`.
fn sr4_launch(f: &Fixture, verb: &str, task: &str, name: &str, doc: &Value, envs: &[(&str, &str)]) -> Value {
    let doc_file = write_json(f, &format!("{name}-doc.json"), &serde_json::to_string_pretty(doc).unwrap());
    let prompt_file = f.home_path().join(format!("{name}-prompt.txt"));
    let mut c = sr4_cmd(f);
    c.env("FAKE_SCENARIO", "spec_review_doc_file")
        .env("FAKE_TASK_ID", task)
        .env("FAKE_SPEC_REVIEW_DOC", &doc_file)
        .env("FAKE_PROMPT_OUT", &prompt_file)
        .args(["spec", "review", verb, task]);
    if verb == "start" {
        c.arg("--edit");
    }
    c.args(["--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--reason", "fixture", "--json"]);
    for (k, v) in envs {
        c.env(k, v);
    }
    parse_stdout(f.run_ok(c, &format!("spec review {verb}")))
}

/// An edit review the ingest keeps: (run id, its `spec_review` detail, the
/// prompt the fake was handed). No `spec_review_invalid` on the run.
fn sr4_review(f: &Fixture, task: &str, name: &str, doc: &Value) -> (String, Value, String) {
    let r = sr4_launch(f, "start", task, name, doc, &[]);
    assert_eq!(r["mode"], "edit", "{r}");
    let run = s(&r["run_id"]);
    let w = sr4_wait(f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["verdict"], "REVISE", "{w}");
    let events = sr4_run_events(f, task, &run);
    assert!(!events.iter().any(|e| e["type"] == "spec_review_invalid"), "{events:#?}");
    let d = sr4_one(&events, "spec_review");
    let prompt = std::fs::read_to_string(f.home_path().join(format!("{name}-prompt.txt"))).unwrap();
    (run, d, prompt)
}

/// A review the ingest refuses whole: the `spec_review_invalid` detail. No
/// `spec_review` on the run, and `show --json` is as it was before.
fn sr4_refused_whole(f: &Fixture, task: &str, name: &str, doc: &Value) -> Value {
    let before = sr4_show(f, task);
    let r = sr4_launch(f, "start", task, name, doc, &[]);
    let run = s(&r["run_id"]);
    let w = sr4_wait(f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["verdict"], Value::Null, "{w}");
    let events = sr4_run_events(f, task, &run);
    assert!(!events.iter().any(|e| e["type"] == "spec_review"), "{events:#?}");
    assert_eq!(sr4_show(f, task), before, "{name}: show changed");
    sr4_one(&events, "spec_review_invalid")
}

/// (index, field, reason) of each refused edit.
fn sr4_refused(d: &Value) -> Vec<(u64, String, String)> {
    d["refused_edits"]
        .as_array()
        .unwrap_or_else(|| panic!("no refused_edits array: {d}"))
        .iter()
        .map(|r| (r["index"].as_u64().unwrap(), s(&r["field"]), s(&r["reason"])))
        .collect()
}

fn sr4_indices(d: &Value) -> Vec<u64> {
    d["edits"].as_array().unwrap().iter().map(|e| e["index"].as_u64().unwrap()).collect()
}

fn sr4_proposed(d: &Value) -> Value {
    let text = s(&d["proposed_spec"]);
    assert_eq!(d["proposed_sha256"], sha(&text));
    serde_json::from_str(&text).unwrap()
}

/// SR-4 AC1 (and AC6 for the reviewer's prompt): a short prefix of the entry
/// at `at`, a one-character prefix shared by two entries, a unique middle
/// substring with a stale `at`, a unique substring with `at` out of range,
/// and an insert. Each edit replaces or deletes the whole entry and records
/// the position it resolved to.
#[test]
fn sr4_short_and_unique_anchors_name_whole_entries() {
    let f = sr4_fixture();
    let task = sr4_task(&f, SR4_SPEC);
    let doc = sr4_doc(
        &SR4_IDS,
        vec![
            sr4_edit("acceptance_criteria", "replace", 4, "AC5: Docs", "AC5: Docs describe the rejection and its exit code in docs/usage.md"),
            sr4_edit("constraints", "delete", 1, "n", ""),
            sr4_edit("non_goals", "replace", 0, "the ledger", "renaming the ledger file"),
            sr4_edit("acceptance_criteria", "replace", 17, "logs the", "AC2: logs the rejection once"),
            sr4_edit("constraints", "insert", 0, "", "no network calls"),
        ],
    );
    let (_, d, prompt) = sr4_review(&f, &task, "ac1", &doc);
    assert_eq!(d["mode"], "edit");
    assert_eq!(d["refused_edits"], json!([]), "{d}");
    assert_eq!(d["edit_count"], 5);
    let summary: Vec<(u64, String, u64, Value, Value)> = d["edits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| (e["index"].as_u64().unwrap(), s(&e["op"]), e["at"].as_u64().unwrap(), e["before"].clone(), e["after"].clone()))
        .collect();
    assert_eq!(
        summary,
        vec![
            (0, "replace".to_string(), 4, json!("AC5: Docs describe the rejection in docs/usage.md"),
                json!("AC5: Docs describe the rejection and its exit code in docs/usage.md")),
            (1, "delete".to_string(), 1, json!("nothing else"), Value::Null),
            (2, "replace".to_string(), 1, json!("renaming the ledger"), json!("renaming the ledger file")),
            (3, "replace".to_string(), 1, json!("AC2: logs the rejection"), json!("AC2: logs the rejection once")),
            (4, "insert".to_string(), 0, Value::Null, json!("no network calls")),
        ]
    );
    let mut expected: Value = serde_json::from_str(SR4_SPEC).unwrap();
    expected["acceptance_criteria"][4] = json!("AC5: Docs describe the rejection and its exit code in docs/usage.md");
    expected["acceptance_criteria"][1] = json!("AC2: logs the rejection once");
    expected["constraints"] = json!(["no network calls", "no new crate", "keep the log format", "a zzz marker stays"]);
    expected["non_goals"][1] = json!("renaming the ledger file");
    assert_eq!(sr4_proposed(&d), expected);
    let shown = sr4_show(&f, &task);
    assert_eq!(shown["newest"]["edit_count"], 5);
    assert_eq!(shown["newest"]["refused_edits"], json!([]));
    assert_eq!(shown["pending_edits"], true);
    assert_eq!(sr4_task_spec(&f, &task), SR4_SPEC, "an edit review never writes the spec");
    // AC6: the reviewer was told the rule it was judged by
    assert!(prompt.contains("A prefix of the entry at \"at\" names that entry, however short."), "{prompt}");
    assert!(prompt.contains("but only when it occurs at exactly one place"), "{prompt}");
    assert!(prompt.contains("no Unicode normalization"), "{prompt}");
    assert!(prompt.contains("An edit that breaks a rule is refused on its own"), "{prompt}");
    assert!(!prompt.contains("10 characters"), "{prompt}");
}

/// SR-4 AC2: anchors are counted in Unicode scalar values and compared as
/// written. One-character Chinese prefixes, a Chinese prefix of 4 scalar
/// values and 12 UTF-8 bytes, a supplementary character and an exact
/// combining-character anchor all name their entries; the precomposed
/// spelling and a whitespace-only anchor are refused alone.
#[test]
fn sr4_unicode_anchors_count_scalar_values_without_normalizing() {
    let f = sr4_fixture();
    let spec = serde_json::to_string(&json!({
        "goal": "守住帳本不讓重複花費",
        "acceptance_criteria": ["AC1: 拒絕重複花費", "AC2: 記錄拒絕原因"],
        "constraints": ["不加新套件", "不改日誌格式", "😀 表情開頭的限制", "cafe\u{301} 帶結合重音"],
        "non_goals": ["重寫傳輸層"],
        "excluded_alternatives": [],
        "risk": "important",
    }))
    .unwrap();
    let task = sr4_task(&f, &spec);
    // "守住帳本": 4 scalar values, 12 UTF-8 bytes
    let doc = sr4_doc(
        &["AC1", "AC2"],
        vec![
            sr4_edit("constraints", "delete", 1, "不", ""),
            sr4_edit("constraints", "replace", 2, "caf\u{e9}", "café 改寫"),
            sr4_edit("constraints", "replace", 0, "  \t ", "不加任何新套件"),
            sr4_edit("goal", "replace", 0, "守住帳本", "守住帳本，拒絕重複花費"),
            sr4_edit("constraints", "replace", 1, "😀", "😀 表情開頭的限制（改）"),
            sr4_edit("constraints", "delete", 2, "cafe\u{301}", ""),
            sr4_edit("acceptance_criteria", "replace", 1, "AC2: 記", "AC2: 記錄拒絕原因與時間"),
            sr4_edit("non_goals", "replace", 0, "重", "重寫傳輸層與重試"),
        ],
    );
    let (_, d, prompt) = sr4_review(&f, &task, "ac2", &doc);
    assert_eq!(sr4_indices(&d), [0, 3, 4, 5, 6, 7]);
    assert_eq!(d["edit_count"], 6);
    let refused = sr4_refused(&d);
    assert_eq!(refused.iter().map(|r| (r.0, r.1.as_str())).collect::<Vec<_>>(), [(1, "constraints"), (2, "constraints")]);
    assert!(refused[0].2.contains("occurs nowhere in constraints"), "{refused:?}");
    assert!(refused[1].2.starts_with("anchor is blank"), "{refused:?}");
    // AC6: the reviewer was told the rules these outcomes follow: the
    // whitespace-only anchor (edit 2) is refused, the one-character and
    // four-character Chinese prefixes (edits 0, 3, 7) and the emoji (edit 4)
    // are long enough, and the precomposed spelling (edit 1) matches nothing
    assert!(prompt.contains("A blank or whitespace-only anchor names nothing."), "{prompt}");
    assert!(prompt.contains("There is no minimum length"), "{prompt}");
    assert!(prompt.contains("length is counted in Unicode scalar values, not bytes"), "{prompt}");
    assert!(prompt.contains("a Chinese character, an emoji and a combining mark each count one"), "{prompt}");
    assert!(prompt.contains("no Unicode normalization"), "{prompt}");
    let before: Vec<Value> = d["edits"].as_array().unwrap().iter().map(|e| e["before"].clone()).collect();
    assert_eq!(
        before,
        [
            json!("不改日誌格式"),
            json!("守住帳本不讓重複花費"),
            json!("😀 表情開頭的限制"),
            json!("cafe\u{301} 帶結合重音"),
            json!("AC2: 記錄拒絕原因"),
            json!("重寫傳輸層"),
        ]
    );
    let p = sr4_proposed(&d);
    assert_eq!(p["goal"], "守住帳本，拒絕重複花費");
    assert_eq!(p["constraints"], json!(["不加新套件", "😀 表情開頭的限制（改）"]));
    assert_eq!(p["acceptance_criteria"], json!(["AC1: 拒絕重複花費", "AC2: 記錄拒絕原因與時間"]));
    assert_eq!(p["non_goals"], json!(["重寫傳輸層與重試"]));
    let shown = sr4_show(&f, &task);
    assert_eq!(shown["newest"]["refused_edits"], d["refused_edits"]);
    assert_eq!(shown["newest"]["edit_count"], 6);
}

/// The AC3 document: a valid replacement, an ambiguous non-prefix anchor, a
/// valid insertion, a deletion whose anchor is nowhere, a later valid
/// replacement whose `at` counts only the applied edits, and an overlapping
/// match.
fn sr4_mixed_doc() -> Value {
    sr4_doc(
        &SR4_IDS,
        vec![
            sr4_edit("constraints", "replace", 0, "no new", "no new crate, not even a small one"),
            sr4_edit("non_goals", "delete", 0, "the", ""),
            sr4_edit("non_goals", "insert", 0, "", "changing the exit codes"),
            sr4_edit("non_goals", "delete", 9, "no such words", ""),
            sr4_edit("non_goals", "replace", 2, "renaming", "renaming the ledger file"),
            sr4_edit("constraints", "delete", 0, "zz", ""),
        ],
    )
}

/// SR-4 AC3: refused edits change nothing and are listed, in input order,
/// in the event, `show --json` and the human `show`; the applied ones keep
/// their original indices across the gaps.
#[test]
fn sr4_refused_edits_are_listed_and_the_rest_keep_their_indices() {
    let f = sr4_fixture();
    let task = sr4_task(&f, SR4_SPEC);
    let (_, d, prompt) = sr4_review(&f, &task, "ac3", &sr4_mixed_doc());
    assert_eq!(sr4_indices(&d), [0, 2, 4]);
    assert_eq!(d["edit_count"], 3);
    assert_eq!(d["edits"][2]["at"], 2, "at counts the insert, not the refused delete");
    let refused = sr4_refused(&d);
    assert_eq!(
        refused,
        vec![
            (1, "non_goals".to_string(),
                "anchor 'the' does not start non_goals entry 0 and occurs 2 times in non_goals (entries 0, 1), so it does not say which entry; write the start of the entry at at, or more text".to_string()),
            (3, "non_goals".to_string(),
                "anchor 'no such words' at 9 is past the end of non_goals (3 entries) and occurs nowhere in non_goals".to_string()),
            (5, "constraints".to_string(),
                "anchor 'zz' does not start constraints entry 0 and occurs 2 times in constraints (entries 3), so it does not say which entry; write the start of the entry at at, or more text".to_string()),
        ]
    );
    // AC6: the reviewer was told that overlapping occurrences count one by
    // one ('zz' in 'a zzz marker stays', edit 5) and that a repeated or
    // missing anchor names nothing (edits 1 and 3), and that a later at
    // counts only applied edits (edit 4)
    assert!(prompt.contains("overlapping occurrences count separately (\"zz\" occurs twice in \"zzz\")"), "{prompt}");
    assert!(prompt.contains("text that occurs twice or more, or nowhere, names nothing"), "{prompt}");
    assert!(prompt.contains("A later \"at\" counts only the edits that were applied"), "{prompt}");
    let mut expected: Value = serde_json::from_str(SR4_SPEC).unwrap();
    expected["constraints"][0] = json!("no new crate, not even a small one");
    expected["non_goals"] = json!(["changing the exit codes", "rewriting the transport", "renaming the ledger file"]);
    assert_eq!(sr4_proposed(&d), expected);
    // the reviewer's document is kept as written, all six edits
    assert_eq!(d["document"]["edits"].as_array().unwrap().len(), 6);

    let shown = sr4_show(&f, &task);
    assert_eq!(shown["newest"]["refused_edits"], d["refused_edits"]);
    assert_eq!(shown["newest"]["edit_count"], 3);
    assert_eq!(shown["pending_edits"], true);
    let human = sr4_show_human(&f, &task);
    for line in [
        "  edit 0  constraints[replace 0]  AC1",
        "  edit 2  non_goals[insert 0]  AC1",
        "  edit 4  non_goals[replace 2]  AC1",
        "  refused edit 1  non_goals  anchor 'the' does not start non_goals entry 0",
        "  refused edit 3  non_goals  anchor 'no such words' at 9 is past the end",
        "  refused edit 5  constraints  anchor 'zz' does not start constraints entry 0",
        "  3 edit(s) await the author check",
    ] {
        assert!(human.lines().any(|l| l.starts_with(line)), "{line}\n{human}");
    }
    assert!(!human.contains("  edit 1  "), "{human}");
}

/// SR-4 AC4 (and AC6 for the author's prompt): a REVISE whose every edit is
/// refused is kept with nothing pending; a mixed one goes to the author with
/// only its applied indices, a confirmation naming a refused index is
/// refused at stage `coverage`, and the author's APPROVE writes only the
/// applied edits.
#[test]
fn sr4_an_all_refused_review_is_kept_and_a_mixed_one_confirms_only_its_applied_edits() {
    let f = sr4_fixture();
    let task = sr4_task(&f, SR4_SPEC);

    // every edit refused
    let mut all_refused = sr4_doc(
        &SR4_IDS,
        vec![sr4_edit("non_goals", "delete", 0, "the", ""), sr4_edit("risk", "replace", 0, "important", "low")],
    );
    all_refused["criteria"][0] = json!({"id": "AC1", "verdict": "revise", "findings": [
        {"check": "steps", "location": "spec", "note": "fixture: say which command shows the rejection"}
    ]});
    let (run, d, _) = sr4_review(&f, &task, "all-refused", &all_refused);
    assert_eq!(d["edit_count"], 0);
    assert_eq!(d["edits"], json!([]));
    assert!(d["proposed_spec"].is_null() && d["proposed_sha256"].is_null(), "{d}");
    assert_eq!(sr4_refused(&d).iter().map(|r| r.0).collect::<Vec<_>>(), [0, 1]);
    let shown = sr4_show(&f, &task);
    assert_eq!(shown["pending_edits"], false);
    assert_eq!(shown["spec_sha256"], sha(SR4_SPEC));
    assert_eq!(shown["newest"]["reviewer_run_id"], run);
    assert_eq!(shown["newest"]["edit_count"], 0);
    assert!(shown["newest"]["proposed_sha256"].is_null());
    assert_eq!(shown["newest"]["refused_edits"], d["refused_edits"]);
    assert_eq!(
        shown["newest"]["document"]["criteria"][0]["findings"][0]["note"],
        "fixture: say which command shows the rejection"
    );
    assert_eq!(sr4_task_spec(&f, &task), SR4_SPEC);
    let human = sr4_show_human(&f, &task);
    assert!(human.contains("fixture: say which command shows the rejection"), "{human}");
    assert!(!human.contains("await the author check"), "{human}");

    // mixed: applied 0, 2, 4; refused 1, 3, 5
    let (_, mixed, _) = sr4_review(&f, &task, "mixed", &sr4_mixed_doc());
    assert_eq!(sr4_indices(&mixed), [0, 2, 4]);
    assert_eq!(sr4_show(&f, &task)["pending_edits"], true);
    assert_eq!(sr4_task_spec(&f, &task), SR4_SPEC);

    // a confirmation that answers the refused index 1 is refused whole
    let answers: Vec<Value> = [0, 1, 2, 4].iter().map(|i| json!({"index": i, "verdict": "ok", "note": ""})).collect();
    let naming_refused = json!({"verdict": "APPROVE", "edits": answers});
    let before = sr4_show(&f, &task);
    let c = sr4_launch(&f, "confirm", &task, "confirm-refused-index", &naming_refused, &[]);
    let run = s(&c["run_id"]);
    let w = sr4_wait(&f, &run);
    assert_eq!(w["verdict"], Value::Null, "{w}");
    let events = sr4_run_events(&f, &task, &run);
    let invalid = sr4_one(&events, "spec_review_invalid");
    assert_eq!(invalid["stage"], "coverage");
    assert_eq!(invalid["source"], "author_check");
    assert!(s(&invalid["reason"]).contains("answers edit 1, which the edit review does not have"), "{invalid}");
    assert!(!events.iter().any(|e| e["type"] == "spec_review" || e["type"] == "spec_updated"));
    assert_eq!(sr4_show(&f, &task), before);

    // the fake author answers exactly the indices in edits.json: APPROVE
    let mut c = sr4_cmd(&f);
    c.env("FAKE_SCENARIO", "spec_confirm_approve")
        .env("FAKE_TASK_ID", &task)
        .args(["spec", "review", "confirm", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .args(["--reason", "fixture", "--json"]);
    let c = parse_stdout(f.run_ok(c, "spec review confirm"));
    let run = s(&c["run_id"]);
    let w = sr4_wait(&f, &run);
    assert_eq!(w["verdict"], "APPROVE", "{}", f.explain_run(&run));
    let input = PathBuf::from(s(&c["worktree"])).join(".agent").join("spec-review-input");
    let edits_file: Value = serde_json::from_str(&std::fs::read_to_string(input.join("edits.json")).unwrap()).unwrap();
    let listed: Vec<u64> = edits_file.as_array().unwrap().iter().map(|e| e["index"].as_u64().unwrap()).collect();
    assert_eq!(listed, [0, 2, 4]);
    let author_prompt = std::fs::read_to_string(PathBuf::from(s(&c["run_dir"])).join("prompt.txt")).unwrap();
    assert!(author_prompt.contains("edits.json lists only the edits thoth applied"), "{author_prompt}");
    assert!(author_prompt.contains("the indices may have gaps"), "{author_prompt}");
    let events = sr4_run_events(&f, &task, &run);
    let approve = sr4_one(&events, "spec_review");
    let updated = sr4_one(&events, "spec_updated");
    assert_eq!(approve["applied"], true);
    assert_eq!(approve["edit_count"], 3);
    assert_eq!(approve["edits"], mixed["edits"]);
    assert_eq!(approve["refused_edits"], json!([]), "the author's check judged only the applied edits");
    assert_eq!(updated["spec_sha256"], mixed["proposed_sha256"]);
    let answered: Vec<u64> = approve["document"]["edits"].as_array().unwrap().iter().map(|e| e["index"].as_u64().unwrap()).collect();
    assert_eq!(answered, [0, 2, 4]);
    let shown = sr4_show(&f, &task);
    assert_eq!(shown["approved"], true);
    assert_eq!(shown["newest"]["mode"], "confirm");
    assert_eq!(shown["newest"]["refused_edits"], json!([]));
    assert_eq!(sr4_task_spec(&f, &task), s(&mixed["proposed_spec"]));
}

/// SR-4 AC5: each individual failure beside valid edits is listed in
/// `refused_edits` while the valid edits survive; a malformed document, an
/// APPROVE with edits, a criteria list that misses an id, an oversized final
/// proposal and a spec changed under the run still refuse the whole review
/// at their old stage and leave `show` as it was.
#[test]
fn sr4_individual_refusals_leave_document_and_proposal_safeguards_whole() {
    let f = sr4_fixture();
    let task = sr4_task(&f, SR4_SPEC);

    let marker = ["[redacted:", "github-token]"].concat();
    let doc = sr4_doc(
        &SR4_IDS,
        vec![
            sr4_edit("constraints", "insert", 0, "", "no network calls"),
            sr4_edit("risk", "replace", 0, "important", "low"),
            sr4_edit("constraints", "move", 0, "no new", "no new crate"),
            sr4_edit("constraints", "replace", 9, "nowhere at all", "x"),
            json!({"field": "non_goals", "op": "insert", "at": 0, "anchor": "", "text": "y", "criterion": "AC1", "reason": "  "}),
            sr4_edit("constraints", "replace", 1, "no new", "   "),
            sr4_edit("acceptance_criteria", "replace", 0, "AC1:", "AC9: rejects a double spend"),
            sr4_edit("constraints", "insert", 0, "", &format!("mask {marker} here")),
            sr4_edit("non_goals", "delete", 1, "renaming", ""),
        ],
    );
    let (_, d, _) = sr4_review(&f, &task, "individual", &doc);
    assert_eq!(sr4_indices(&d), [0, 8]);
    let refused = sr4_refused(&d);
    let expect: [(u64, &str, &str); 7] = [
        (1, "risk", "field must be one of"),
        (2, "constraints", "op 'move' must be one of"),
        (3, "constraints", "occurs nowhere in constraints"),
        (4, "non_goals", "reason must not be blank"),
        (5, "constraints", "text must not be blank for op replace"),
        (6, "acceptance_criteria", "the criterion ids changed"),
        (7, "constraints", "carries a mask marker the stored spec does not have"),
    ];
    assert_eq!(refused.len(), expect.len(), "{refused:?}");
    for ((index, field, reason), (want_index, want_field, want)) in refused.iter().zip(expect) {
        assert_eq!((*index, field.as_str()), (want_index, want_field), "{refused:?}");
        assert!(reason.contains(want), "{index}: {reason}");
    }
    let p = sr4_proposed(&d);
    assert_eq!(p["constraints"][0], "no network calls");
    assert_eq!(p["non_goals"], json!(["rewriting the transport"]));
    assert_eq!(p["risk"], "important");
    assert!(!s(&d["proposed_spec"]).contains("[redacted:"), "{d}");

    // whole-review refusals: each leaves show as it was
    let valid = sr4_edit("constraints", "insert", 0, "", "no network calls");
    let mut missing = valid.clone();
    missing.as_object_mut().unwrap().remove("anchor");
    let invalid = sr4_refused_whole(&f, &task, "missing-anchor", &sr4_doc(&SR4_IDS, vec![valid.clone(), missing]));
    assert_eq!(invalid["stage"], "schema");
    assert!(s(&invalid["reason"]).contains("edits array is malformed"), "{invalid}");

    let mut approve = sr4_doc(&SR4_IDS, vec![valid.clone()]);
    approve["verdict"] = json!("APPROVE");
    assert_eq!(sr4_refused_whole(&f, &task, "approve-with-edits", &approve)["stage"], "schema");

    let short = sr4_doc(&SR4_IDS[..4], vec![valid.clone()]);
    let invalid = sr4_refused_whole(&f, &task, "coverage", &short);
    assert_eq!(invalid["stage"], "coverage");
    assert_eq!(invalid["expected"], json!(SR4_IDS));

    let big = sr4_doc(
        &SR4_IDS,
        vec![valid.clone(), sr4_edit("risk", "replace", 0, "important", "low"), sr4_edit("constraints", "insert", 0, "", &"x".repeat(41_000))],
    );
    let invalid = sr4_refused_whole(&f, &task, "too-large", &big);
    assert_eq!(invalid["stage"], "lint");
    assert!(s(&invalid["reason"]).contains("too_large"), "{invalid}");

    // the spec changed while the fake waited at its gate
    let before = sr4_show(&f, &task);
    let gate = f.home_path().join("sr4-gate");
    let gate_s = gate.display().to_string();
    let changing = sr4_doc(&SR4_IDS, vec![valid.clone(), sr4_edit("non_goals", "delete", 0, "the", "")]);
    let r = sr4_launch(&f, "start", &task, "spec-changed", &changing, &[("FAKE_REVIEW_GATE", &gate_s)]);
    let run = s(&r["run_id"]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let st = sr4_json(&f, &["status", &task, "--json"]);
        let running = st["runs"].as_array().unwrap().iter().any(|x| x["run_id"] == run.as_str() && x["status"] == "running");
        if running {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "run {run} never ran\n{}", f.explain_run(&run));
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    let rewritten = SR4_SPEC.replace("guard the ledger against a double spend", "guard the ledger against every double spend");
    let file = write_json(&f, "sr4-rewritten.json", &rewritten);
    let file = file.display().to_string();
    sr4_json(&f, &["task", "update-spec", &task, "-f", &file, "--json"]);
    std::fs::write(&gate, b"").unwrap();
    let w = sr4_wait(&f, &run);
    assert_eq!(w["verdict"], Value::Null, "{w}");
    let events = sr4_run_events(&f, &task, &run);
    assert!(!events.iter().any(|e| e["type"] == "spec_review"), "{events:#?}");
    assert_eq!(sr4_one(&events, "spec_review_invalid")["stage"], "spec_changed");
    let after = sr4_show(&f, &task);
    assert_eq!(after["newest"]["spec_review_id"], before["newest"]["spec_review_id"]);
    assert_eq!(after["review_count"], before["review_count"]);
    assert_eq!(after["spec_sha256"], sha(&rewritten));
}
