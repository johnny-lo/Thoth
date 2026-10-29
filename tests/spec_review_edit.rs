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

/// AC7: an edit on a field that cannot be edited, and an edit that makes the
/// spec too large, are `spec_review_invalid` and leave no `spec_review`.
#[test]
fn bad_and_oversized_edits_are_refused_without_a_spec_review_event() {
    let f = standard();
    let (task, _) = f.create_task(EDITABLE);

    let r = spec_start(&f, &task, "claude-a", "spec_review_edit_bad", &["--model", "haiku", "--edit"], &[]);
    let run = s(&r["run_id"]);
    let w = wait_run(&f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["verdict"], Value::Null);
    assert_eq!(count_events(&f, &task, "spec_review_invalid"), 1);
    let (ev_run, d) = last_event(&f, &task, "spec_review_invalid");
    assert_eq!(ev_run.as_deref(), Some(run.as_str()));
    assert_eq!(d["stage"], "edits");
    assert_eq!(d["source"], "reviewer_run");
    assert_eq!(d["reviewer_run_id"], run);
    assert_eq!(
        d["reason"],
        "edit 0 (risk): field must be one of goal, acceptance_criteria, constraints, non_goals, excluded_alternatives"
    );
    assert_eq!(count_events(&f, &task, "spec_review"), 0);
    assert_eq!(task_status(&f, &task), "queued");
    let shown = show_json(&f, &task);
    assert_eq!(shown["review_count"], 0);
    assert_eq!(shown["pending_edits"], false);

    let r = spec_start(&f, &task, "claude-a", "spec_review_edit_big", &["--model", "haiku", "--edit"], &[]);
    let run = s(&r["run_id"]);
    let w = wait_run(&f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["verdict"], Value::Null);
    assert_eq!(count_events(&f, &task, "spec_review_invalid"), 2);
    let (_, d) = last_event(&f, &task, "spec_review_invalid");
    assert_eq!(d["stage"], "lint");
    assert!(s(&d["reason"]).contains("too_large"), "{d}");
    assert_eq!(count_events(&f, &task, "spec_review"), 0);
    assert_eq!(task_status(&f, &task), "queued");
    let refused = dispatch_err(&f, &task, &[]);
    assert!(refused.contains("no spec review is recorded"), "{refused}");
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
    assert_eq!(tools.len(), 36);
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
        for k in ["mode", "edits", "edit_count", "proposed_spec", "proposed_sha256", "bytes_before", "bytes_after"] {
            assert!(d.get(k).is_none(), "{k} in {d}");
        }
    };
    let no_edits = |shown: &Value| {
        assert_eq!(shown["newest"]["edit_count"], 0, "{shown}");
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
