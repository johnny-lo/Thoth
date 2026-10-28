//! FC-2: the runner retries a transient failure in the same run - after
//! `THOTH_RETRY_WAITS` (30 s and 2 min by default), at most twice, resuming
//! the session when it can and starting over when it cannot. Each test is
//! named after AC7's case it covers. Everything runs on the fakes and a
//! temporary THOTH_HOME.

mod common;

use common::Fixture;
use serde_json::Value;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ---- copied from tests/failure_classes.rs

const MODEL: &str = "gemini-3.8-flash-low";

/// `Fixture::add_agy_profile`: registers an antigravity profile.
trait AgyFixture {
    fn add_agy_profile(&self, id: &str) -> PathBuf;
}

impl AgyFixture for common::Fixture {
    fn add_agy_profile(&self, id: &str) -> PathBuf {
        let cfg = self.home.path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        self.cmd().args(["profile", "add", id, "--provider", "google", "--harness",
            "antigravity", "--config-dir"]).arg(&cfg).assert().success();
        cfg
    }
}

const CONV: &str = "11111111-2222-4333-8444-555555555555";

fn agy_args_log(f: &common::Fixture) -> PathBuf {
    f.home_path().join("fake-agy-args.jsonl")
}

fn agy_env(f: &common::Fixture, cfg: &Path, task_id: &str, scenario: &str) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task_id)
        .env("FAKE_AGY_DATA_DIR", cfg)
        .env("FAKE_AGY_ARGS_LOG", agy_args_log(f));
    c
}

fn run_row(f: &common::Fixture, run_id: &str) -> (String, Option<String>, Option<String>, Option<String>) {
    f.state_conn()
        .query_row(
            "SELECT status, interrupted_reason, error_text, harness_session_id FROM runs WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

fn run_status(f: &common::Fixture, run_id: &str) -> (String, Option<String>, Option<i64>, Option<String>) {
    let conn = f.state_conn();
    conn.query_row(
        "SELECT status, interrupted_reason, exit_code, error_text FROM runs WHERE run_id = ?1",
        [run_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn profile_status(f: &Fixture, id: &str) -> String {
    f.global_conn()
        .query_row("SELECT status FROM profiles WHERE profile_id = ?1", [id], |r| r.get(0))
        .unwrap()
}

/// Every quota event kind of the profile, in seq order.
fn quota_kinds(f: &Fixture, id: &str) -> Vec<String> {
    let conn = f.global_conn();
    let mut stmt = conn
        .prepare("SELECT kind FROM quota_events WHERE profile_id = ?1 ORDER BY seq")
        .unwrap();
    stmt.query_map([id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

/// The detail of the run's newest event of `ty`.
fn event_detail(f: &Fixture, run_id: &str, ty: &str) -> Value {
    let detail: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            [run_id, ty],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&detail).unwrap()
}

/// Every event type of the run, in seq order.
fn events_of_run(f: &Fixture, run_id: &str) -> Vec<String> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT type FROM events WHERE run_id = ?1 ORDER BY seq")
        .unwrap();
    stmt.query_map([run_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap()
}

fn status_json(f: &Fixture, task: &str) -> Value {
    let mut c = f.cmd();
    c.args(["status", task, "--json"]);
    let out = f.run_ok(c, "status --json");
    serde_json::from_slice(&out).unwrap()
}

fn run_of(f: &Fixture, task: &str, run_id: &str) -> Value {
    status_json(f, task)["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run_id)
        .unwrap()
        .clone()
}

fn resume(f: &Fixture, task: &str, run: &str) {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", task)
        .args(["resume", run, "--json"]);
    f.run_ok(c, "resume");
}

fn has(list: &[String], item: &str) -> bool {
    list.iter().any(|x| x == item)
}

// ---- this file's own helpers

/// The details of every `run_retry` event of the run, in seq order.
fn retry_events(f: &Fixture, run_id: &str) -> Vec<Value> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT detail FROM events WHERE run_id = ?1 AND type = 'run_retry' ORDER BY seq")
        .unwrap();
    stmt.query_map([run_id], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|d| serde_json::from_str(&d.unwrap()).unwrap())
        .collect()
}

fn count_of(events: &[String], ty: &str) -> usize {
    events.iter().filter(|e| *e == ty).count()
}

fn run_dir(f: &Fixture, run_id: &str) -> PathBuf {
    let dir: String = f
        .state_conn()
        .query_row("SELECT run_dir FROM runs WHERE run_id = ?1", [run_id], |r| r.get(0))
        .unwrap();
    PathBuf::from(dir)
}

fn worker_pid_of(f: &Fixture, run_id: &str) -> i64 {
    f.state_conn()
        .query_row("SELECT worker_pid FROM runs WHERE run_id = ?1", [run_id], |r| r.get(0))
        .unwrap()
}

fn task_status(f: &Fixture, task: &str) -> String {
    f.task_status(task)
}

/// The file's non-empty lines.
fn lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// The `args` array of one args-log line.
fn args_of(line: &str) -> Vec<String> {
    let v: Value = serde_json::from_str(line).unwrap();
    v["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect()
}

/// The argument right after `flag`.
fn after(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn dispatch_with(
    f: &Fixture,
    task: &str,
    profile: &str,
    extra_args: &[&str],
    env: &[(&str, &OsStr)],
) -> Value {
    let mut c = f.cmd();
    c.env("FAKE_TASK_ID", task);
    for (k, v) in env {
        c.env(k, v);
    }
    c.args(["dispatch", task, "--profile", profile, "--json"]).args(extra_args);
    // RJ-1: the first launch gives --model and --effort
    if !extra_args.contains(&"--model") {
        c.args(["--model", "sonnet"]);
    }
    c.args(["--effort", "medium"]);
    let out = f.run_ok(c, "dispatch");
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn attempts_file(f: &Fixture, task: &str) -> PathBuf {
    f.home_path().join(format!("attempts-{task}.txt"))
}

// ---- AC7

#[test]
fn a_claude_overloaded_error_is_retried_by_resuming_the_session_and_the_run_completes() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let attempts = f.home_path().join("attempts.txt");
    let log = f.home_path().join("claude-args.jsonl");
    let prompt = f.home_path().join("prompt.txt");
    let d = dispatch_with(
        &f,
        &task,
        "claude-b",
        &[],
        &[
            ("FAKE_SCENARIO", OsStr::new("ok")),
            ("FAKE_FIRST_SCENARIO", OsStr::new("overloaded")),
            ("THOTH_RETRY_WAITS", OsStr::new("1,1")),
            ("FAKE_ATTEMPTS_FILE", attempts.as_os_str()),
            ("FAKE_CLAUDE_ARGS_LOG", log.as_os_str()),
            ("FAKE_PROMPT_COPY", prompt.as_os_str()),
        ],
    );
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    assert_eq!(run_status(&f, &run), ("completed".to_string(), None, Some(0), None));
    let change = event_detail(&f, &run, "status_change");
    assert_eq!(change["retries"], 1, "{change}");
    assert!(change["retry_stop"].is_null(), "{change}");
    assert!(change["retry_error"].is_null(), "{change}");
    assert!(change["login_check"].is_null(), "{change}");
    assert!(change["interrupted_reason"].is_null(), "{change}");

    let events = events_of_run(&f, &run);
    assert_eq!(count_of(&events, "run_retry"), 1, "{events:?}");
    assert_eq!(count_of(&events, "harness_init"), 2, "{events:?}");
    assert_eq!(count_of(&events, "run_started"), 1, "{events:?}");
    assert!(!has(&events, "run_interrupted"), "{events:?}");

    let session = run_row(&f, &run).3.expect("a claude run has a session");
    let r = retry_events(&f, &run)[0].clone();
    assert_eq!(r["attempt"], 1, "{r}");
    assert_eq!(r["wait_seconds"], 1, "{r}");
    assert_eq!(r["mode"], "resume", "{r}");
    assert!(r["error"].as_str().unwrap().contains("Overloaded"), "{r}");
    assert_eq!(r["exit_code"], 1, "{r}");
    assert_eq!(r["session_id"], session.as_str(), "{r}");
    assert!(r["worker_pid"].is_i64() || r["worker_pid"].is_u64(), "{r}");
    assert_eq!(Value::from(worker_pid_of(&f, &run)), r["worker_pid"]);
    assert_ne!(r["worker_pid"], event_detail(&f, &run, "run_started")["worker_pid"]);

    let tries = lines(&attempts);
    assert_eq!(tries.len(), 2, "{tries:?}");
    assert!(tries[0].starts_with("fresh "), "{tries:?}");
    assert_eq!(tries[1], format!("resume {session}"));

    let launches = lines(&log);
    assert_eq!(launches.len(), 2, "{launches:?}");
    let first = args_of(&launches[0]);
    assert_eq!(after(&first, "--session-id").as_deref(), Some(session.as_str()), "{first:?}");
    assert!(!has(&first, "--resume"), "{first:?}");
    let second = args_of(&launches[1]);
    assert_eq!(after(&second, "--resume").as_deref(), Some(session.as_str()), "{second:?}");
    assert!(!has(&second, "--session-id"), "{second:?}");

    let sent = std::fs::read_to_string(&prompt).unwrap();
    assert!(sent.starts_with("The previous session was interrupted"), "{sent}");

    assert_eq!(profile_status(&f, "claude-b"), "active");
    let kinds = quota_kinds(&f, "claude-b");
    assert!(!has(&kinds, "rate_limited"), "{kinds:?}");
    assert!(!has(&kinds, "logged_out"), "{kinds:?}");
    assert_eq!(task_status(&f, &task), "in_review");
}

#[test]
fn two_transient_failures_in_a_row_use_both_waits_in_order_and_the_run_still_completes() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let attempts = attempts_file(&f, &task);
    let d = dispatch_with(
        &f,
        &task,
        "claude-b",
        &[],
        &[
            ("FAKE_SCENARIO", OsStr::new("ok")),
            ("FAKE_FIRST_SCENARIO", OsStr::new("overloaded")),
            ("FAKE_FIRST_TIMES", OsStr::new("2")),
            ("THOTH_RETRY_WAITS", OsStr::new("1,2")),
            ("FAKE_ATTEMPTS_FILE", attempts.as_os_str()),
        ],
    );
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    assert_eq!(run_status(&f, &run).0, "completed");
    let change = event_detail(&f, &run, "status_change");
    assert_eq!(change["retries"], 2, "{change}");
    assert!(change["retry_stop"].is_null(), "{change}");
    let rs = retry_events(&f, &run);
    assert_eq!(rs.len(), 2, "{rs:?}");
    assert_eq!(rs[0]["attempt"], 1);
    assert_eq!(rs[0]["wait_seconds"], 1);
    assert_eq!(rs[1]["attempt"], 2);
    assert_eq!(rs[1]["wait_seconds"], 2);
    assert_eq!(rs[0]["mode"], "resume");
    assert_eq!(rs[1]["mode"], "resume");
    let events = events_of_run(&f, &run);
    assert_eq!(count_of(&events, "harness_init"), 3, "{events:?}");

    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT seq, type FROM events WHERE run_id = ?1 ORDER BY seq")
        .unwrap();
    let rows: Vec<(i64, String)> = stmt
        .query_map([&run], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    let first_seq = |ty: &str| rows.iter().find(|(_, t)| t == ty).map(|(s, _)| *s).unwrap();
    assert!(first_seq("run_retry") > first_seq("harness_init"), "{rows:?}");
}

#[test]
fn after_two_retries_the_run_ends_interrupted_transient_and_resume_still_continues_it() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = dispatch_with(
        &f,
        &task,
        "claude-b",
        &[],
        &[
            ("FAKE_SCENARIO", OsStr::new("overloaded")),
            ("THOTH_RETRY_WAITS", OsStr::new("1,1")),
        ],
    );
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");

    let (status, reason, exit, error) = run_status(&f, &run);
    assert_eq!(status, "interrupted");
    assert_eq!(reason.as_deref(), Some("transient"));
    assert_eq!(exit, Some(1));
    let error = error.unwrap();
    assert!(error.starts_with("transient interruption:"), "{error}");
    assert!(error.contains("Overloaded"), "{error}");

    let change = event_detail(&f, &run, "status_change");
    assert_eq!(change["retries"], 2, "{change}");
    assert_eq!(change["retry_stop"], "exhausted", "{change}");
    assert!(change["retry_error"].is_null(), "{change}");
    assert_eq!(change["login_check"], "logged_in", "{change}");
    assert_eq!(change["interrupted_reason"], "transient", "{change}");

    let rs = retry_events(&f, &run);
    assert_eq!(rs.len(), 2, "{rs:?}");
    assert_eq!(rs[0]["attempt"], 1);
    assert_eq!(rs[1]["attempt"], 2);
    assert_eq!(rs[0]["wait_seconds"], 1);
    assert_eq!(rs[1]["wait_seconds"], 1);

    let events = events_of_run(&f, &run);
    assert_eq!(count_of(&events, "harness_init"), 3, "{events:?}");
    assert!(has(&events, "run_interrupted"), "{events:?}");
    assert_eq!(event_detail(&f, &run, "run_interrupted")["reason"], "transient");
    assert!(!has(&events, "quota_exceeded"), "{events:?}");
    assert!(!has(&events, "login_failed"), "{events:?}");

    let d = run_dir(&f, &run);
    assert!(d.join("stderr.log").is_file());
    assert!(d.join("stderr.retry1.log").is_file());
    assert!(d.join("stderr.retry2.log").is_file());
    let stream = lines(&d.join("stream.jsonl"));
    assert!(stream.len() >= 6, "{stream:?}");

    let step = run_of(&f, &task, &run)["next_step"].as_str().unwrap().to_string();
    assert!(step.contains(&format!("thoth resume {run}")), "{step}");
    assert_eq!(task_status(&f, &task), "interrupted");
    assert_eq!(profile_status(&f, "claude-b"), "active");

    resume(&f, &task, &run);
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
}

#[test]
fn a_second_failure_that_is_not_transient_ends_the_run_without_another_retry() {
    let f = setup();
    f.add_profile("claude-a");
    f.add_profile("claude-b");

    // a quota stop after the retry: parked, no login check, no more retries
    let (task1, _) = f.create_task(Fixture::SPEC);
    let attempts1 = attempts_file(&f, &task1);
    let d = dispatch_with(
        &f,
        &task1,
        "claude-b",
        &[],
        &[
            ("FAKE_SCENARIO", OsStr::new("quota")),
            ("FAKE_FIRST_SCENARIO", OsStr::new("overloaded")),
            ("THOTH_RETRY_WAITS", OsStr::new("1,1")),
            ("FAKE_ATTEMPTS_FILE", attempts1.as_os_str()),
        ],
    );
    let run1 = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task1, "120s").1, "interrupted");
    assert_eq!(run_status(&f, &run1).1.as_deref(), Some("quota"));
    let change = event_detail(&f, &run1, "status_change");
    assert_eq!(change["retries"], 1, "{change}");
    assert!(change["retry_stop"].is_null(), "{change}");
    assert!(change["login_check"].is_null(), "{change}");
    let events = events_of_run(&f, &run1);
    assert!(has(&events, "quota_exceeded"), "{events:?}");
    assert_eq!(count_of(&events, "run_retry"), 1, "{events:?}");
    assert_eq!(count_of(&events, "harness_init"), 2, "{events:?}");
    assert_eq!(profile_status(&f, "claude-b"), "rate_limited");

    // a crash after the retry: one login check, no more retries
    let (task2, _) = f.create_task(Fixture::SPEC);
    let attempts2 = attempts_file(&f, &task2);
    let d = dispatch_with(
        &f,
        &task2,
        "claude-a",
        &[],
        &[
            ("FAKE_SCENARIO", OsStr::new("crash")),
            ("FAKE_FIRST_SCENARIO", OsStr::new("overloaded")),
            ("THOTH_RETRY_WAITS", OsStr::new("1,1")),
            ("FAKE_ATTEMPTS_FILE", attempts2.as_os_str()),
        ],
    );
    let run2 = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task2, "120s").1, "interrupted");
    assert_eq!(run_status(&f, &run2).1.as_deref(), Some("crash"));
    let change = event_detail(&f, &run2, "status_change");
    assert_eq!(change["retries"], 1, "{change}");
    assert!(change["retry_stop"].is_null(), "{change}");
    assert_eq!(change["login_check"], "logged_in", "{change}");
    assert_eq!(count_of(&events_of_run(&f, &run2), "run_retry"), 1);
    assert_eq!(profile_status(&f, "claude-a"), "active");
}

#[test]
fn a_retry_is_skipped_when_the_wait_alone_would_reach_the_timeout() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    // No THOTH_RETRY_WAITS: the default 30 s wait does not fit in 25 s.
    let d = dispatch_with(
        &f,
        &task,
        "claude-b",
        &["--timeout", "25s"],
        &[("FAKE_SCENARIO", OsStr::new("overloaded"))],
    );
    let run = d["run_id"].as_str().unwrap().to_string();
    let t0 = Instant::now();
    assert_eq!(f.wait_for(&task, "90s").1, "interrupted");
    assert!(t0.elapsed() < Duration::from_secs(20), "{:?}", t0.elapsed());

    let status = run_status(&f, &run);
    assert_eq!(status.1.as_deref(), Some("transient"));
    assert_eq!(status.2, Some(1));
    let change = event_detail(&f, &run, "status_change");
    assert_eq!(change["retries"], 0, "{change}");
    assert_eq!(change["retry_stop"], "timeout", "{change}");
    assert!(change["retry_error"].is_null(), "{change}");
    assert_eq!(change["login_check"], "logged_in", "{change}");
    let events = events_of_run(&f, &run);
    assert!(!has(&events, "run_retry"), "{events:?}");
    assert_eq!(count_of(&events, "harness_init"), 1, "{events:?}");
    assert_eq!(task_status(&f, &task), "interrupted");
}

#[test]
fn a_cancel_during_the_wait_ends_the_run_cancelled_without_a_relaunch() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = dispatch_with(
        &f,
        &task,
        "claude-b",
        &[],
        &[
            ("FAKE_SCENARIO", OsStr::new("overloaded")),
            ("THOTH_RETRY_WAITS", OsStr::new("60,60")),
        ],
    );
    let run = d["run_id"].as_str().unwrap().to_string();

    // the result line has arrived ...
    let deadline = Instant::now() + Duration::from_secs(30);
    while !has(&events_of_run(&f, &run), "run_usage") {
        assert!(Instant::now() < deadline, "no run_usage within 30 s");
        std::thread::sleep(Duration::from_millis(250));
    }
    // ... and the first launch's drain is over: the runner is waiting.
    std::thread::sleep(Duration::from_secs(2));

    let mut c = f.cmd();
    c.args(["cancel", &run, "--json"]);
    let out: Value = serde_json::from_slice(&f.run_ok(c, "cancel")).unwrap();
    assert_eq!(out["status"], "cancelled", "{out}");
    assert_eq!(out["escalated"], false, "{out}");
    assert_eq!(f.wait_for(&task, "60s").1, "cancelled");

    assert_eq!(
        run_status(&f, &run),
        (
            "cancelled".to_string(),
            Some("user".to_string()),
            Some(1),
            Some("cancelled by request".to_string())
        )
    );
    let change = event_detail(&f, &run, "status_change");
    assert_eq!(change["retries"], 0, "{change}");
    assert_eq!(change["retry_stop"], "cancelled", "{change}");
    assert!(change["kill_error"].is_null(), "{change}");
    assert!(change["login_check"].is_null(), "{change}");
    let events = events_of_run(&f, &run);
    assert!(!has(&events, "run_retry"), "{events:?}");
    assert_eq!(count_of(&events, "harness_init"), 1, "{events:?}");
    assert_eq!(task_status(&f, &task), "cancelled");
}

#[test]
fn a_codex_disconnect_is_retried_by_resuming_the_thread() {
    let f = setup();
    f.add_codex_profile("codex-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let log = f.home_path().join("codex-args.jsonl");
    let attempts = attempts_file(&f, &task);
    let d = dispatch_with(
        &f,
        &task,
        "codex-a",
        &["--model", "gpt-5.6-luna"],
        &[
            ("FAKE_SCENARIO", OsStr::new("ok")),
            ("FAKE_FIRST_SCENARIO", OsStr::new("disconnected")),
            ("THOTH_RETRY_WAITS", OsStr::new("1,1")),
            ("FAKE_CODEX_ARGS_LOG", log.as_os_str()),
            ("FAKE_ATTEMPTS_FILE", attempts.as_os_str()),
        ],
    );
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    assert_eq!(run_status(&f, &run).0, "completed");
    let change = event_detail(&f, &run, "status_change");
    assert_eq!(change["retries"], 1, "{change}");
    assert!(change["retry_stop"].is_null(), "{change}");
    let r = retry_events(&f, &run)[0].clone();
    assert_eq!(r["mode"], "resume", "{r}");
    assert!(r["error"].as_str().unwrap().contains("stream disconnected before completion"), "{r}");
    let thread = run_row(&f, &run).3.expect("codex named its thread");
    assert_eq!(r["session_id"], thread.as_str(), "{r}");

    let execs: Vec<Vec<String>> = lines(&log)
        .iter()
        .map(|l| args_of(l))
        .filter(|a| a.first().map(String::as_str) == Some("exec"))
        .collect();
    assert_eq!(execs.len(), 2, "{execs:?}");
    assert!(!has(&execs[0], "resume"), "{:?}", execs[0]);
    assert_eq!(after(&execs[1], "resume").as_deref(), Some(thread.as_str()), "{:?}", execs[1]);

    let rows = f.evidence_rows(&task);
    let cmd_has = |s: &str| {
        rows.iter()
            .any(|e| e["detail"]["cmd"].as_str().is_some_and(|c| c.contains(s)))
    };
    assert!(cmd_has("before-the-drop"), "{rows:?}");
    assert!(cmd_has("hello-stdout"), "{rows:?}");
    assert_eq!(profile_status(&f, "codex-a"), "active");
}

#[test]
fn an_agy_network_failure_resumes_the_conversation_and_an_eligibility_failure_restarts_fresh() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");

    // the network failure named its conversation: continued
    let (task1, _) = f.create_task(Fixture::SPEC);
    let attempts1 = attempts_file(&f, &task1);
    let mut c = agy_env(&f, &cfg, &task1, "ok");
    c.env("FAKE_FIRST_SCENARIO", "network")
        .env("THOTH_RETRY_WAITS", "1,1")
        .env("FAKE_ATTEMPTS_FILE", &attempts1)
        .args(["dispatch", &task1, "--profile", "agy-a", "--model", MODEL, "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    let run1 = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task1, "120s").1, "in_review");
    assert_eq!(run_status(&f, &run1), ("completed".to_string(), None, Some(0), None));
    assert_eq!(event_detail(&f, &run1, "status_change")["retries"], 1);
    let r = retry_events(&f, &run1)[0].clone();
    assert_eq!(r["mode"], "resume", "{r}");
    assert_eq!(r["session_id"], CONV, "{r}");
    assert!(r["error"].as_str().unwrap().contains("There was a network issue"), "{r}");
    assert_eq!(r["exit_code"], 3, "{r}");
    assert_eq!(run_row(&f, &run1).3.as_deref(), Some(CONV));
    let log = lines(&agy_args_log(&f));
    let (before, last) = (args_of(&log[log.len() - 2]), args_of(&log[log.len() - 1]));
    assert!(!has(&before, "--conversation"), "{before:?}");
    assert_eq!(after(&last, "--conversation").as_deref(), Some(CONV), "{last:?}");

    // the eligibility failure never named one: started over
    let (task2, _) = f.create_task(Fixture::SPEC);
    let attempts2 = attempts_file(&f, &task2);
    let mut c = agy_env(&f, &cfg, &task2, "ok");
    c.env("FAKE_FIRST_SCENARIO", "eligibility")
        .env("THOTH_RETRY_WAITS", "1,1")
        .env("FAKE_ATTEMPTS_FILE", &attempts2)
        .args(["dispatch", &task2, "--profile", "agy-a", "--model", MODEL, "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    let run2 = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task2, "120s").1, "in_review");
    assert_eq!(run_status(&f, &run2).0, "completed");
    assert_eq!(event_detail(&f, &run2, "status_change")["retries"], 1);
    let r = retry_events(&f, &run2)[0].clone();
    assert_eq!(r["mode"], "fresh", "{r}");
    assert!(r["session_id"].is_null(), "{r}");
    assert!(r["error"].as_str().unwrap().contains("Eligibility check failed"), "{r}");
    assert_eq!(r["exit_code"], 1, "{r}");
    assert_eq!(run_row(&f, &run2).3.as_deref(), Some(CONV));
    let log = lines(&agy_args_log(&f));
    let (before, last) = (args_of(&log[log.len() - 2]), args_of(&log[log.len() - 1]));
    assert!(!has(&before, "--conversation"), "{before:?}");
    assert!(!has(&last, "--conversation"), "{last:?}");

    assert_eq!(profile_status(&f, "agy-a"), "active");
    assert!(!has(&quota_kinds(&f, "agy-a"), "rate_limited"));
}

#[test]
fn a_reviewer_run_is_retried_with_a_fresh_session_and_the_verdict_lands() {
    let f = setup();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    f.dispatch(&task, "claude-a", "hooks", &[]);
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    let log = f.home_path().join("review-args.jsonl");
    let attempts = f.home_path().join("attempts-review.txt");
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_FIRST_SCENARIO", "overloaded")
        .env("FAKE_TASK_ID", &task)
        .env("THOTH_RETRY_WAITS", "1,1")
        .env("FAKE_CLAUDE_ARGS_LOG", &log)
        .env("FAKE_ATTEMPTS_FILE", &attempts)
        .args(["review", "start", &task, "--profile", "claude-b", "--model", "opus", "--effort", "medium", "--json"]);
    let started: Value = serde_json::from_slice(&f.run_ok(c, "review start")).unwrap();
    let rev = started["run_id"].as_str().unwrap().to_string();

    let mut c = f.cmd();
    c.args(["wait", "--run", &rev, "--timeout", "120s", "--json"]);
    let waited: Value = serde_json::from_slice(&f.run_ok(c, "wait --run")).unwrap();
    assert_eq!(waited["status"], "completed", "{waited}");
    assert_eq!(waited["verdict"], "PASS", "{waited}");
    assert_eq!(waited["task_status"], "pass", "{waited}");

    let change = event_detail(&f, &rev, "status_change");
    assert_eq!(change["retries"], 1, "{change}");
    assert!(change["retry_stop"].is_null(), "{change}");
    assert_eq!(change["review_verdict"], "PASS", "{change}");
    let r = retry_events(&f, &rev)[0].clone();
    assert_eq!(r["mode"], "fresh", "{r}");
    let s = r["session_id"].as_str().expect("a fresh claude retry names its session").to_string();
    assert_eq!(run_row(&f, &rev).3.as_deref(), Some(s.as_str()));

    let launches = lines(&log);
    assert_eq!(launches.len(), 2, "{launches:?}");
    let first = args_of(&launches[0]);
    let second = args_of(&launches[1]);
    for a in [&first, &second] {
        assert!(has(a, "--session-id"), "{a:?}");
        assert!(!has(a, "--resume"), "{a:?}");
    }
    assert_ne!(after(&first, "--session-id").as_deref(), Some(s.as_str()), "{first:?}");
    assert_eq!(after(&second, "--session-id").as_deref(), Some(s.as_str()), "{second:?}");
    assert_eq!(f.task_status(&task), "pass");
}
