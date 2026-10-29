//! SR-2: `thoth spec review start`, Thoth's own read-only spec reviewer run
//! (role `spec_reviewer`), its verdict recorded as a `spec_review` event the
//! first-dispatch gate reads, and a task status it never touches.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const IMPORTANT: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#;
const REWRITTEN: &str = r#"{"goal":"guard the ledger, rejecting a double spend before it is written","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"important"}"#;

// --- copied from tests/spec_review.rs -----------------------------------------

/// `COUNT(*)` of `task_id`'s events of exactly one `type`.
fn count_events(f: &Fixture, task_id: &str, event_type: &str) -> i64 {
    f.state_conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE task_id = ?1 AND type = ?2",
            [task_id, event_type],
            |r| r.get(0),
        )
        .unwrap()
}

/// `(run_id, detail)` of every event of one type of a task, in seq order.
fn event_rows(f: &Fixture, task_id: &str, event_type: &str) -> Vec<(Option<String>, Value)> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT run_id, detail FROM events WHERE task_id=?1 AND type=?2 ORDER BY seq")
        .unwrap();
    stmt.query_map([task_id, event_type], |r| {
        let detail: String = r.get(1)?;
        Ok((r.get::<_, Option<String>>(0)?, serde_json::from_str(&detail).unwrap()))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

fn last_event(f: &Fixture, task_id: &str, event_type: &str) -> (Option<String>, Value) {
    event_rows(f, task_id, event_type).pop().unwrap()
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

fn run_count(f: &Fixture, task: &str) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM runs WHERE task_id = ?1", [task], |r| r.get(0))
        .unwrap()
}

fn sha(s: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

fn update_spec_json(f: &Fixture, task: &str, file: &Path) -> Value {
    let mut c = f.cmd();
    c.args(["task", "update-spec", task, "-f"]).arg(file).arg("--json");
    let out = f.run_ok(c, "task update-spec");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn set_status(f: &Fixture, task: &str, status: &str) {
    f.state_conn()
        .execute("UPDATE tasks SET status = ?2 WHERE task_id = ?1", [task, status])
        .unwrap();
}

// --- copied from tests/antigravity.rs -----------------------------------------

/// `Fixture::add_agy_profile`: registers an antigravity profile.
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

// --- copied from tests/spec_review.rs: the MCP server -------------------------

struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start_scenario_task(
        f: &common::Fixture,
        extra: &[&str],
        scenario: &str,
        task: Option<&str>,
    ) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .args(extra)
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("FAKE_SCENARIO", scenario)
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(task) = task {
            c.env("FAKE_TASK_ID", task);
        } else {
            c.env_remove("FAKE_TASK_ID");
        }
        let mut child = c.spawn().unwrap();
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

// --- this file's own helpers --------------------------------------------------

fn standard() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    f.add_codex_profile("codex-a");
    f
}

fn start_cmd(
    f: &Fixture,
    task: &str,
    profile: &str,
    scenario: &str,
    extra: &[&str],
    envs: &[(&str, &str)],
) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task)
        .args(["spec", "review", "start", task, "--profile", profile])
        .args(extra);
    for (k, v) in envs {
        c.env(k, v);
    }
    c
}

/// `spec review start TASK --profile P --json` plus `extra`, exit 0.
fn spec_start(
    f: &Fixture,
    task: &str,
    profile: &str,
    scenario: &str,
    extra: &[&str],
    envs: &[(&str, &str)],
) -> Value {
    let mut c = start_cmd(f, task, profile, scenario, extra, envs);
    c.arg("--json");
    let out = f.run_ok(c, "spec review start");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// `spec review start` that must fail; returns its stderr.
fn spec_start_err(f: &Fixture, task: &str, profile: &str, extra: &[&str], envs: &[(&str, &str)]) -> String {
    let out = start_cmd(f, task, profile, "crash", extra, envs)
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

fn wait_run_human(f: &Fixture, run: &str) -> String {
    let mut c = f.cmd();
    c.args(["wait", "--run", run, "--timeout", "180s"]);
    String::from_utf8(f.run_ok(c, "wait --run")).unwrap()
}

fn status_json(f: &Fixture, task: &str) -> Value {
    let mut c = f.cmd();
    c.args(["status", task, "--json"]);
    let out = f.run_ok(c, "status");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn task_changes_by_run(f: &Fixture, run: &str) -> i64 {
    f.state_conn()
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id=?1 AND type='status_change' AND detail LIKE '%\"entity\":\"task\"%'",
            [run],
            |r| r.get(0),
        )
        .unwrap()
}

fn run_status(f: &Fixture, run: &str) -> String {
    f.state_conn()
        .query_row("SELECT status FROM runs WHERE run_id=?1", [run], |r| r.get(0))
        .unwrap()
}

/// Polls every 250 ms, at most `secs`, until the run's status is `wanted`.
fn wait_run_status(f: &Fixture, run: &str, wanted: &str, secs: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let s = run_status(f, run);
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

/// The detail of the newest `status_change` event written on `run`.
fn last_status_change_of_run(f: &Fixture, run: &str) -> Value {
    let detail: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE run_id=?1 AND type='status_change' ORDER BY seq DESC LIMIT 1",
            [run],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&detail).unwrap()
}

fn spec_review_dir(f: &Fixture, task: &str) -> PathBuf {
    f.home_path()
        .join("repos")
        .join(f.repo_id())
        .join("worktrees")
        .join(format!("{task}-spec-review"))
}

fn s(v: &Value) -> String {
    v.as_str().unwrap().to_string()
}

/// AGY-1: every event of `ty` on `run`, oldest first, through `events --json`.
fn cli_events(f: &Fixture, task: &str, run: &str, ty: &str) -> Vec<Value> {
    let mut c = f.cmd();
    c.args(["events", "--since", "0", "--task", task, "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(c, "events")).unwrap();
    v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["run_id"] == run && e["type"] == ty)
        .map(|e| e["detail"].clone())
        .collect()
}

// --- the tests ----------------------------------------------------------------

#[test]
fn a_codex_spec_reviewer_approve_is_recorded_and_lets_the_task_dispatch() {
    let f = standard();
    let (task, base) = f.create_task(IMPORTANT);
    let head = common::git(&f.repo_path(), &["rev-parse", "HEAD"]);
    let args_log = f.home_path().join("codex-args.jsonl");
    let args_log_s = args_log.display().to_string();
    let r = spec_start(
        &f,
        &task,
        "codex-a",
        "spec_review_approve",
        &["--model", "gpt-5.6-sol", "--effort", "high"],
        &[("FAKE_CODEX_ARGS_LOG", &args_log_s)],
    );
    assert_eq!(r["ok"], true);
    assert_eq!(r["spec_sha256"], sha(IMPORTANT));
    assert_eq!(r["reviewed_commit"], head);
    assert_eq!(r["task_base_commit"], base);
    assert_eq!(r["task_status"], "queued");
    assert_eq!(r["required_before_dispatch"], true);
    assert_eq!(r["worktree_reused"], false);
    assert_eq!(r["worktree_moved"], false);
    assert_eq!(r["profile"], "codex-a");
    assert_eq!(r["model"], "gpt-5.6-sol");
    assert_eq!(r["effort"], "high");
    let wt = PathBuf::from(s(&r["worktree"]));
    assert!(s(&r["worktree"]).ends_with("-spec-review"), "{}", r["worktree"]);
    assert_eq!(common::git(&wt, &["rev-parse", "HEAD"]), head);
    let symbolic = Command::new("git")
        .args(["symbolic-ref", "-q", "HEAD"])
        .current_dir(&wt)
        .output()
        .unwrap();
    assert!(!symbolic.status.success(), "the spec review worktree is detached");
    let run = s(&r["run_id"]);

    let w = wait_run(&f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["role"], "spec_reviewer");
    assert_eq!(w["verdict"], "APPROVE");
    assert_eq!(w["task_status"], "queued");
    assert_eq!(w["timed_out"], false);
    let human = wait_run_human(&f, &run);
    assert!(human.contains("verdict APPROVE"), "{human}");

    let input = std::fs::read(wt.join(".agent").join("spec-review-input").join("spec.json")).unwrap();
    assert_eq!(input, IMPORTANT.as_bytes());
    let report: Value = serde_json::from_str(
        &std::fs::read_to_string(wt.join(".agent").join("spec-review.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(report["verdict"], "APPROVE");

    let log = std::fs::read_to_string(&args_log).unwrap();
    let exec: Vec<String> = log
        .lines()
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .rfind(|v| v["args"][0] == "exec")
        .map(|v| {
            v["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect()
        })
        .unwrap();
    assert!(exec.contains(&"-s".to_string()), "{exec:?}");
    assert!(exec.contains(&"read-only".to_string()), "{exec:?}");
    assert!(exec.contains(&"--output-schema".to_string()), "{exec:?}");
    assert!(exec.contains(&"-o".to_string()), "{exec:?}");
    let after = |flag: &str| exec[exec.iter().position(|a| a == flag).unwrap() + 1].clone();
    assert_eq!(
        Path::new(&after("-o")),
        wt.join(".agent").join("spec-review.json").as_path()
    );
    let run_dir = PathBuf::from(s(&r["run_dir"]));
    assert_eq!(
        Path::new(&after("--output-schema")),
        run_dir.join("spec-review-schema.json").as_path()
    );

    let prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    assert!(prompt.contains("read its body; a name that exists is not enough"), "{prompt}");
    assert!(prompt.contains("even the ones you decide need no change"), "{prompt}");
    assert!(prompt.contains("check that the current code allows it"), "{prompt}");
    assert!(prompt.contains("not against an installed binary"), "{prompt}");
    assert!(prompt.contains(".agent/spec-review-input/spec.json"), "{prompt}");
    assert!(
        prompt.lines().any(|l| l == "Every id in \"criteria\" must be one of: AC1, AC2."),
        "{prompt}"
    );
    assert!(!prompt.contains("rejects a double spend"), "{prompt}");

    assert_eq!(count_events(&f, &task, "spec_review"), 1);
    let (ev_run, d) = last_event(&f, &task, "spec_review");
    assert_eq!(ev_run, Some(run.clone()));
    assert_eq!(d["source"], "reviewer_run");
    assert_eq!(d["reviewer_run_id"], run);
    assert_eq!(d["verdict"], "APPROVE");
    assert_eq!(d["spec_sha256"], sha(IMPORTANT));
    assert_eq!(d["document"]["criteria"].as_array().unwrap().len(), 2);
    assert_eq!(d["findings"], 1);
    assert_eq!(count_events(&f, &task, "spec_review_started"), 1);
    let (_, started) = last_event(&f, &task, "spec_review_started");
    assert_eq!(started["reviewed_commit"], head);
    assert_eq!(started["spec_sha256"], sha(IMPORTANT));
    assert_eq!(task_changes_by_run(&f, &run), 0);
    assert_eq!(f.task_status(&task), "queued");

    let shown = show_json(&f, &task);
    assert_eq!(shown["approved"], true);
    assert_eq!(shown["required_before_dispatch"], true);
    assert_eq!(shown["review_count"], 1);
    assert_eq!(shown["newest"]["source"], "reviewer_run");
    assert_eq!(shown["newest"]["reviewer_run_id"], run);
    assert_eq!(shown["newest"]["current"], true);
    assert_eq!(status_json(&f, &task)["runs"][0]["role"], "spec_reviewer");

    let d = f.dispatch(&task, "claude-b", "crash", &[]);
    assert_eq!(d["spec_review"]["approved"], true);
    assert_eq!(d["spec_review"]["required"], true);
    assert_eq!(d["spec_review"]["skipped"], false);
    assert_eq!(d["attempt"], 1);
    let settled = last_status_change_of_run(&f, &run);
    assert_eq!(settled["spec_review_verdict"], "APPROVE");
    assert_eq!(settled["task_status_written"], Value::Null);
    assert_eq!(settled["review_verdict"], Value::Null);
    f.wait_for(&task, "120s");
}

#[test]
fn a_claude_spec_reviewer_revise_blocks_dispatch_until_the_spec_is_rewritten_and_approved() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let r1 = spec_start(&f, &task, "claude-a", "spec_review_revise", &["--model", "haiku"], &[]);
    let w1 = wait_run(&f, &s(&r1["run_id"]));
    assert_eq!(w1["verdict"], "REVISE", "{}", f.explain_run(&s(&r1["run_id"])));
    let shown = show_json(&f, &task);
    assert_eq!(shown["approved"], false);
    assert_eq!(shown["revise_streak"], 1);
    assert_eq!(shown["newest"]["verdict"], "REVISE");
    let refused = dispatch_err(&f, &task, &[]);
    assert!(refused.contains("has no APPROVE spec review (the newest spec review "), "{refused}");
    assert!(refused.contains(" is REVISE)"), "{refused}");

    let rewritten = write_json(&f, "rewritten.json", REWRITTEN);
    update_spec_json(&f, &task, &rewritten);
    let r2 = spec_start(&f, &task, "claude-a", "spec_review_approve", &["--model", "haiku"], &[]);
    assert_eq!(r2["worktree_reused"], true);
    assert_eq!(r2["worktree_moved"], false);
    assert_eq!(r2["spec_sha256"], sha(REWRITTEN));
    let w2 = wait_run(&f, &s(&r2["run_id"]));
    assert_eq!(w2["verdict"], "APPROVE", "{}", f.explain_run(&s(&r2["run_id"])));
    let shown = show_json(&f, &task);
    assert_eq!(shown["approved"], true);
    assert_eq!(shown["revise_streak"], 0);
    assert_eq!(shown["review_count"], 2);
    f.dispatch(&task, "claude-b", "crash", &[]);
    f.wait_for(&task, "120s");
}

#[test]
fn an_agy_spec_reviewer_answers_through_structured_output() {
    let f = standard();
    f.add_agy_profile("agy-a");
    let (task, _) = f.create_task(IMPORTANT);
    let data = agy_data(&f).display().to_string();
    let r = spec_start(
        &f,
        &task,
        "agy-a",
        "spec_review_approve",
        &["--model", "gemini-3.8-flash-high"],
        &[("FAKE_AGY_DATA_DIR", &data)],
    );
    // AGY-1: `read_file` (agy 1.4.4) beside `view_file`; still no write or shell name
    assert_eq!(r["allowed_tools"], "read_file,view_file,list_dir,find_by_name,grep_search,finish,wait,wait_5_seconds");
    let run = s(&r["run_id"]);
    let w = wait_run(&f, &run);
    assert_eq!(w["verdict"], "APPROVE", "{}", f.explain_run(&run));
    let report_path = PathBuf::from(s(&r["worktree"])).join(".agent").join("spec-review.json");
    assert!(report_path.exists());
    let report: Value = serde_json::from_str(&std::fs::read_to_string(&report_path).unwrap()).unwrap();
    assert_eq!(report["verdict"], "APPROVE");
    assert_eq!(show_json(&f, &task)["approved"], true);
    // AGY-1 (AC5): the fake spec reviewer's write and shell attempts are
    // denied as known names, its `read_file` of the spec is not, and the
    // verdict still comes through structured_output.
    let wt = PathBuf::from(s(&r["worktree"]));
    assert!(!wt.join("agy_reviewer_leak.txt").exists());
    assert!(!wt.join("agy_reviewer_leak2.txt").exists());
    let denied: Vec<Value> = cli_events(&f, &task, &run, "tool_denied");
    let names: Vec<&str> = denied.iter().map(|d| d["tool"].as_str().unwrap()).collect();
    assert_eq!(names, ["write_file", "run_command"], "{denied:?}");
    assert!(denied.iter().all(|d| d["known_to_thoth"] == true), "{denied:?}");
    assert!(denied.iter().all(|d| d["reason"].as_str().unwrap().contains("is not on this run's allow-list")), "{denied:?}");
    let usage = cli_events(&f, &task, &run, "run_usage");
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["tools_denied"], 2, "{}", usage[0]);
    assert_eq!(usage[0]["tools_unknown"], json!([]), "{}", usage[0]);
    assert_eq!(usage[0]["tool_names"], json!(["read_file"]), "{}", usage[0]);
    assert_eq!(w["tools_unknown"], json!([]), "{w}");
    let policy: Value = serde_json::from_str(&std::fs::read_to_string(wt.join(".agent").join("agy-hook.json")).unwrap()).unwrap();
    assert_eq!(policy["role"], "spec_reviewer", "{policy}");
    assert_eq!(policy["allowed_tools"], json!(["read_file", "view_file", "list_dir", "find_by_name", "grep_search", "finish", "wait", "wait_5_seconds"]), "{policy}");

    // No structured_output: nothing to ingest.
    let g = standard();
    g.add_agy_profile("agy-a");
    let (task, _) = g.create_task(IMPORTANT);
    let data = agy_data(&g).display().to_string();
    let r = spec_start(
        &g,
        &task,
        "agy-a",
        "spec_review_no_output",
        &["--model", "gemini-3.8-flash-high"],
        &[("FAKE_AGY_DATA_DIR", &data)],
    );
    let run = s(&r["run_id"]);
    let w = wait_run(&g, &run);
    assert_eq!(w["status"], "completed", "{}", g.explain_run(&run));
    assert_eq!(w["verdict"], Value::Null);
    assert_eq!(count_events(&g, &task, "spec_review_invalid"), 1);
    let (_, d) = last_event(&g, &task, "spec_review_invalid");
    assert_eq!(d["stage"], "read");
    assert_eq!(d["source"], "reviewer_run");
    assert_eq!(d["reviewer_run_id"], run);
    assert_eq!(count_events(&g, &task, "spec_review"), 0);
    assert_eq!(g.task_status(&task), "queued");
}

#[test]
fn a_bad_spec_review_document_writes_one_invalid_event_and_leaves_the_task_alone() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let r = spec_start(&f, &task, "claude-a", "spec_review_bad", &["--model", "haiku"], &[]);
    let run = s(&r["run_id"]);
    let w = wait_run(&f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["verdict"], Value::Null);
    assert_eq!(w["task_status"], "queued");
    assert_eq!(count_events(&f, &task, "spec_review_invalid"), 1);
    let (ev_run, d) = last_event(&f, &task, "spec_review_invalid");
    assert_eq!(d["stage"], "schema");
    assert!(s(&d["reason"]).contains("spec-review.json is not valid JSON"), "{d}");
    assert_eq!(d["source"], "reviewer_run");
    assert_eq!(d["reviewer_run_id"], run);
    assert_eq!(ev_run, Some(run.clone()));
    assert_eq!(count_events(&f, &task, "spec_review"), 0);
    assert_eq!(show_json(&f, &task)["review_count"], 0);
    let refused = dispatch_err(&f, &task, &[]);
    assert!(refused.contains("no spec review is recorded"), "{refused}");
}

#[test]
fn a_spec_changed_under_a_running_spec_reviewer_is_not_recorded() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let gate = f.home_path().join("gate");
    let gate_s = gate.display().to_string();
    let r = spec_start(
        &f,
        &task,
        "claude-a",
        "spec_review_approve",
        &["--model", "haiku"],
        &[("FAKE_REVIEW_GATE", &gate_s)],
    );
    let run = s(&r["run_id"]);
    wait_run_status(&f, &run, "running", 60);
    let rewritten = write_json(&f, "rewritten.json", REWRITTEN);
    update_spec_json(&f, &task, &rewritten);
    std::fs::write(&gate, b"").unwrap();
    let w = wait_run(&f, &run);
    assert_eq!(w["status"], "completed", "{}", f.explain_run(&run));
    assert_eq!(w["verdict"], Value::Null);
    assert_eq!(count_events(&f, &task, "spec_review_invalid"), 1);
    let (_, d) = last_event(&f, &task, "spec_review_invalid");
    assert_eq!(d["stage"], "spec_changed");
    assert!(s(&d["reason"]).contains("the review judged the old spec"), "{d}");
    assert_eq!(count_events(&f, &task, "spec_review"), 0);
    let shown = show_json(&f, &task);
    assert_eq!(shown["spec_sha256"], sha(REWRITTEN));
    assert_eq!(shown["approved"], false);
}

#[test]
fn a_crashed_spec_reviewer_leaves_the_task_queued_and_the_worktree_is_reused() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let r = spec_start(&f, &task, "claude-a", "crash", &["--model", "haiku"], &[]);
    let run = s(&r["run_id"]);
    let w = wait_run(&f, &run);
    assert_eq!(w["status"], "interrupted", "{}", f.explain_run(&run));
    assert_eq!(w["interrupted_reason"], "crash");
    assert_eq!(w["verdict"], Value::Null);
    assert_eq!(w["task_status"], "queued");
    assert_eq!(last_status_change_of_run(&f, &run)["task_status_written"], Value::Null);
    assert_eq!(task_changes_by_run(&f, &run), 0);
    assert_eq!(count_events(&f, &task, "spec_review"), 0);

    let r2 = spec_start(&f, &task, "claude-a", "spec_review_approve", &["--model", "haiku"], &[]);
    assert_eq!(r2["worktree_reused"], true);
    assert_eq!(r2["cleared_stale_report"], false);
    let w2 = wait_run(&f, &s(&r2["run_id"]));
    assert_eq!(w2["verdict"], "APPROVE", "{}", f.explain_run(&s(&r2["run_id"])));
}

#[test]
fn spec_review_start_refuses_and_writes_nothing() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let before = run_count(&f, &task);
    assert_eq!(before, 0);
    let events_before = f.events_of(&task).len();

    // (a) a bad --effort
    let err = spec_start_err(&f, &task, "claude-a", &["--model", "haiku", "--effort", "banana"], &[]);
    assert!(err.contains("banana"), "{err}");

    // (b) the task is pass, cancelled, accepted or running
    for status in ["pass", "cancelled", "accepted", "running"] {
        set_status(&f, &task, status);
        let err = spec_start_err(&f, &task, "claude-a", &["--model", "haiku"], &[]);
        assert!(
            err.contains("spec review start needs a task that is not running, pass, cancelled or accepted"),
            "{status}: {err}"
        );
    }
    set_status(&f, &task, "queued");

    // (c) inside a run
    let out = start_cmd(&f, &task, "claude-a", "crash", &["--model", "haiku"], &[])
        .env("THOTH_RUN_ID", "X")
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("refuses to run with THOTH_RUN_ID set"), "{err}");

    // (d) no such profile
    let err = spec_start_err(&f, &task, "nope-a", &["--model", "haiku"], &[]);
    assert!(err.contains("no such profile: nope-a"), "{err}");

    // (e) without --model nothing is picked (RJ-1): refused, naming --model
    let err = spec_start_err(&f, &task, "codex-a", &[], &[]);
    assert!(err.contains("--model") && err.contains("missing"), "{err}");

    assert_eq!(run_count(&f, &task), before);
    assert_eq!(f.events_of(&task).len(), events_before);
    assert!(!spec_review_dir(&f, &task).exists());

    // (f) a live run of the task
    let gate = f.home_path().join("gate");
    let gate_s = gate.display().to_string();
    let r = spec_start(
        &f,
        &task,
        "claude-a",
        "spec_review_approve",
        &["--model", "haiku"],
        &[("FAKE_REVIEW_GATE", &gate_s)],
    );
    let run = s(&r["run_id"]);
    wait_run_status(&f, &run, "running", 60);
    let err = spec_start_err(&f, &task, "claude-a", &["--model", "haiku"], &[]);
    assert!(err.contains("refusing to start a spec review of task "), "{err}");
    assert!(err.contains(" (spec_reviewer) of task "), "{err}");
    assert!(err.contains(" is still 'running'"), "{err}");
    let refused = dispatch_err(&f, &task, &["--skip-spec-review", "test"]);
    assert!(refused.contains("is still 'running'"), "{refused}");
    assert_eq!(run_count(&f, &task), 1);
    std::fs::write(&gate, b"").unwrap();
    wait_run(&f, &run);
}

#[test]
fn cancel_and_resume_treat_a_spec_reviewer_like_a_reviewer() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let r = spec_start(&f, &task, "claude-a", "slow", &["--model", "haiku"], &[]);
    let run = s(&r["run_id"]);
    wait_run_status(&f, &run, "running", 60);
    let mut c = f.cmd();
    c.args(["cancel", &run, "--json"]);
    f.run_ok(c, "cancel");
    wait_run_status(&f, &run, "cancelled", 60);
    assert_eq!(f.task_status(&task), "queued");
    let out = f
        .cmd()
        .args(["resume", &run])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8(out).unwrap();
    assert!(err.contains("spec reviews are not resumable"), "{err}");
    assert!(err.contains("thoth spec review start "), "{err}");
    assert_eq!(f.task_status(&task), "queued");
}

#[test]
fn the_spec_review_worktree_follows_the_main_worktree_head() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let r1 = spec_start(&f, &task, "claude-a", "spec_review_revise", &["--model", "haiku"], &[]);
    wait_run(&f, &s(&r1["run_id"]));
    let repo = f.repo_path();
    std::fs::write(repo.join("c.txt"), "three\n").unwrap();
    common::git(&repo, &["add", "c.txt"]);
    common::git(&repo, &["commit", "-m", "third"]);
    let head2 = common::git(&repo, &["rev-parse", "HEAD"]);

    let r2 = spec_start(&f, &task, "claude-a", "spec_review_approve", &["--model", "haiku"], &[]);
    assert_eq!(r2["worktree_reused"], true);
    assert_eq!(r2["worktree_moved"], true);
    assert_eq!(r2["reviewed_commit"], head2);
    assert_eq!(r2["worktree"], r1["worktree"]);
    let wt = PathBuf::from(s(&r2["worktree"]));
    assert_eq!(common::git(&wt, &["rev-parse", "HEAD"]), head2);
    let (_, started) = last_event(&f, &task, "spec_review_started");
    assert_eq!(started["worktree_moved"], true);
    assert_eq!(started["reviewed_commit"], head2);
    let w = wait_run(&f, &s(&r2["run_id"]));
    assert_eq!(w["verdict"], "APPROVE", "{}", f.explain_run(&s(&r2["run_id"])));
}

#[test]
fn task_close_removes_the_spec_review_worktree_as_kind_spec_review() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let r = spec_start(&f, &task, "claude-a", "spec_review_approve", &["--model", "haiku"], &[]);
    wait_run(&f, &s(&r["run_id"]));
    let wt = PathBuf::from(s(&r["worktree"]));
    assert!(wt.exists());

    let mut c = f.cmd();
    c.args(["task", "close", &task, "--reason", "not needed", "--json"]);
    let closed: Value =
        serde_json::from_str(String::from_utf8(f.run_ok(c, "task close")).unwrap().trim()).unwrap();
    let removed = closed["worktrees"]["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1, "{closed}");
    assert_eq!(removed[0]["kind"], "spec_review");
    assert_eq!(removed[0]["path"], s(&r["worktree"]));
    assert_eq!(removed[0]["branch"], Value::Null);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while wt.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert!(!wt.exists());
    let (_, d) = last_event(&f, &task, "worktree_removed");
    assert_eq!(d["kind"], "spec_review");
    assert_eq!(d["source"], "task_close");
    let list = common::git(&f.repo_path(), &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(&s(&r["worktree"])), "{list}");
}

#[test]
fn the_runner_sweep_removes_the_spec_review_worktree_when_the_task_is_cancelled() {
    let f = standard();
    let (task, _) = f.create_task(IMPORTANT);
    let r = spec_start(&f, &task, "claude-a", "spec_review_approve", &["--model", "haiku"], &[]);
    let w = wait_run(&f, &s(&r["run_id"]));
    assert_eq!(w["verdict"], "APPROVE", "{}", f.explain_run(&s(&r["run_id"])));
    let wt = PathBuf::from(s(&r["worktree"]));

    let d = f.dispatch(&task, "claude-b", "slow", &[]);
    let implementer = s(&d["run_id"]);
    wait_run_status(&f, &implementer, "running", 60);
    let mut c = f.cmd();
    c.args(["cancel", &implementer, "--json"]);
    f.run_ok(c, "cancel");
    f.wait_for_task_status(&task, &["cancelled"], 60);
    // The runner's sweep happens after the task is settled.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let spec_review_removal = || {
        event_rows(&f, &task, "worktree_removed")
            .into_iter()
            .map(|(_, d)| d)
            .rfind(|d| d["kind"] == "spec_review")
    };
    while spec_review_removal().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
    // The worker worktree (a later run id) is removed after it, so the
    // spec_review removal is the newest one of its kind, not of all.
    let removal = spec_review_removal().expect("no worktree_removed of kind spec_review");
    assert_eq!(removal["source"], "runner");
    assert_eq!(removal["kind"], "spec_review");
    assert!(!wt.exists());
}

#[test]
fn mcp_starts_a_spec_review_and_waits_for_it() {
    let f = common::fixture();
    f.add_profile("claude-a");
    let mut srv = Server::start_scenario_task(&f, &[], "spec_review_approve", None);
    assert_eq!(srv.call(1, "thoth_init", json!({}))["result"]["isError"], false);
    let spec: Value = serde_json::from_str(IMPORTANT).unwrap();
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
    let tool = tools.iter().find(|t| t["name"] == "thoth_spec_review_start").unwrap();
    assert_eq!(tool["annotations"]["readOnlyHint"], false);

    let started = srv.call(
        4,
        "thoth_spec_review_start",
        json!({"task":task,"profile":"claude-a","model":"haiku"}),
    );
    assert_eq!(started["result"]["isError"], false, "{started}");
    let run = s(&started["result"]["structuredContent"]["run_id"]);
    assert!(!run.is_empty());
    // thoth_task_create stores the spec object re-serialized, not IMPORTANT's
    // bytes, so the fingerprint is of the bytes the task holds.
    let stored: String = f
        .state_conn()
        .query_row("SELECT spec FROM tasks WHERE task_id=?1", [&task], |r| r.get(0))
        .unwrap();
    assert_eq!(started["result"]["structuredContent"]["spec_sha256"], sha(&stored));

    let mut waited = Value::Null;
    for i in 0..8 {
        waited = srv.call(5 + i, "thoth_wait", json!({"run":run,"timeout_seconds":25}));
        if waited["result"]["structuredContent"]["timed_out"] == false {
            break;
        }
    }
    assert_eq!(waited["result"]["structuredContent"]["timed_out"], false, "{waited}");
    assert_eq!(waited["result"]["structuredContent"]["verdict"], "APPROVE", "{waited}");
    assert_eq!(waited["result"]["structuredContent"]["role"], "spec_reviewer");
    let shown = srv.call(20, "thoth_spec_review_show", json!({"task":task}));
    assert_eq!(shown["result"]["structuredContent"]["approved"], true, "{shown}");
    assert_eq!(shown["result"]["structuredContent"]["newest"]["source"], "reviewer_run");
}

#[test]
fn help_lists_spec_review_start() {
    let f = common::fixture();
    let help = |args: &[&str]| {
        let out = f.cmd().args(args).assert().success().get_output().stdout.clone();
        String::from_utf8(out).unwrap()
    };
    let spec_review = help(&["spec", "review", "--help"]);
    assert!(spec_review.contains("start"), "{spec_review}");
    let start = help(&["spec", "review", "start", "--help"]);
    assert!(start.contains("--profile"), "{start}");
    assert!(start.contains("--model"), "{start}");
    assert!(start.contains("--effort"), "{start}");
    assert!(start.contains("--reason"), "{start}");
    assert!(start.contains("--timeout"), "{start}");
    assert!(start.contains("did not write the spec"), "{start}");
}
