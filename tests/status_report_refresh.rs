//! REVFIX-1: `thoth status` rewrites an existing report.html when it settles
//! the run of a dead runner, and leaves the page alone (same bytes, same
//! modification time) when it settles nothing. Driven only through the CLI
//! with fake-claude: `report --json` writes the page, `status` settles, and
//! `profile usage --json` (which writes nothing) waits until the dead run is
//! no longer counted live. No heartbeat is aged by hand.

mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

const TITLE: &str = "status settles this title";

fn json(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_slice(&out).unwrap_or_else(|e| panic!("{args:?}: not JSON ({e})"))
}

fn report_path(f: &Fixture) -> PathBuf {
    f.home_path().join("report.html")
}

/// The page's bytes and modification time.
fn page_state(f: &Fixture) -> (Vec<u8>, SystemTime) {
    let p = report_path(f);
    (fs::read(&p).unwrap(), fs::metadata(&p).unwrap().modified().unwrap())
}

fn page(f: &Fixture) -> String {
    fs::read_to_string(report_path(f)).unwrap()
}

/// The markup `thoth report` writes for one task row: the status pill, then
/// the title (as tests/report_refresh.rs matches rows).
fn row(label: &str, class: &str, title: &str) -> String {
    format!("<span class=\"pill {class}\">{label}</span><div><p class=\"ttitle\">{title}</p>")
}

fn create_titled(f: &Fixture, title: &str) -> String {
    let spec_file = f.home_path().join("titled-spec.json");
    fs::write(&spec_file, Fixture::SPEC).unwrap();
    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&spec_file)
        .args(["--title", title, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice::<Value>(&out).unwrap()["task_id"].as_str().unwrap().to_string()
}

fn run_in<'a>(status: &'a Value, run_id: &str) -> &'a Value {
    status["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run_id)
        .unwrap_or_else(|| panic!("no run {run_id} in {status}"))
}

/// Polls `status TASK --json` until the run is `running` with a recorded
/// runner pid (a live runner: these calls settle nothing); returns the pid.
fn wait_running(f: &Fixture, task_id: &str, run_id: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let v = json(f, &["status", task_id, "--json"]);
        let r = run_in(&v, run_id);
        if r["status"] == "running" {
            if let Some(pid) = r["runner_pid"].as_u64() {
                return pid as u32;
            }
        }
        assert!(Instant::now() < deadline, "run {run_id} never reached running: {r}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Polls `profile usage --json` until `profile` has no live run; it does once
/// the heartbeat of a run whose runner is gone is 15 seconds old.
fn wait_live_runs_zero(f: &Fixture, profile: &str) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let v = json(f, &["profile", "usage", "--json"]);
        let live = v["profiles"]
            .as_array()
            .or(v.as_array())
            .and_then(|a| a.iter().find(|p| p["profile_id"] == profile))
            .map(|p| p["live_runs"].as_i64().unwrap())
            .unwrap_or_else(|| panic!("no profile {profile} in {v}"));
        if live == 0 {
            return;
        }
        assert!(Instant::now() < deadline, "live_runs of {profile} is still {live}");
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A titled task with a `slow` run whose runner has been killed and whose
/// heartbeat is now stale; nothing has settled it yet. With `report` the page
/// is written while the run is live (showing it 進行中) before the kill.
/// Returns (task id, run id).
fn dead_run(f: &Fixture, report: bool) -> (String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let task_id = create_titled(f, TITLE);
    let d = f.dispatch(&task_id, "claude-b", "slow", &["--timeout", "10m"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let runner_pid = wait_running(f, &task_id, &run_id);
    if report {
        f.cmd().args(["report", "--json"]).assert().success();
        assert!(page(f).contains(&row("進行中", "run", TITLE)), "{}", page(f));
    }
    let before = report.then(|| page_state(f).0);
    common::kill_pid_only(runner_pid);
    wait_live_runs_zero(f, "claude-b");
    if let Some(before) = before {
        assert!(page_state(f).0 == before, "something rewrote report.html before status looked");
    }
    (task_id, run_id)
}

/// The page shows the task interrupted, with the resume step for `run_id`,
/// and no longer running.
fn assert_page_shows_the_crash(f: &Fixture, run_id: &str) {
    let p = page(f);
    assert!(p.contains(&row("中斷", "bad", TITLE)), "no 中斷 row for {TITLE}:\n{p}");
    assert!(p.contains(&format!("worker 當掉：執行 thoth resume {run_id}")), "no resume step for {run_id}:\n{p}");
    assert!(!p.contains(&row("進行中", "run", TITLE)), "the 進行中 row is still there:\n{p}");
}

#[test]
fn status_task_json_that_settles_a_run_rewrites_the_page_and_a_second_status_does_not() {
    let f = common::fixture();
    let (task_id, run_id) = dead_run(&f, true);

    let v = json(&f, &["status", &task_id, "--json"]);
    let r = run_in(&v, &run_id);
    assert_eq!(r["status"], "interrupted", "{r}");
    assert_eq!(r["interrupted_reason"], "crash", "{r}");
    assert_eq!(v["status"], "interrupted", "{v}");
    assert_page_shows_the_crash(&f, &run_id);

    // the run is settled now: a second look settles nothing and leaves the page
    let (bytes, mtime) = page_state(&f);
    std::thread::sleep(Duration::from_millis(1100));
    let again = json(&f, &["status", &task_id, "--json"]);
    assert_eq!(run_in(&again, &run_id)["status"], "interrupted", "{again}");
    let (bytes2, mtime2) = page_state(&f);
    assert!(bytes2 == bytes, "a status that settled nothing changed report.html");
    assert_eq!(mtime2, mtime, "a status that settled nothing touched report.html");
}

#[test]
fn plain_status_with_human_output_that_settles_a_run_rewrites_the_page() {
    let f = common::fixture();
    let (task_id, run_id) = dead_run(&f, true);

    let out = f.cmd().arg("status").assert().success().get_output().stdout.clone();
    let out = String::from_utf8_lossy(&out);
    let line = out.lines().find(|l| l.contains(&task_id)).unwrap_or_else(|| panic!("no {task_id} in {out}"));
    assert!(line.contains("interrupted"), "{out}");
    assert_page_shows_the_crash(&f, &run_id);

    let v = json(&f, &["status", &task_id, "--json"]);
    assert_eq!(run_in(&v, &run_id)["interrupted_reason"], "crash", "{v}");
}

#[test]
fn a_status_that_settles_a_run_creates_no_page() {
    let f = common::fixture();
    let (task_id, _run_id) = dead_run(&f, false);
    assert!(!report_path(&f).exists());

    let out = f.cmd().arg("status").assert().success().get_output().stdout.clone();
    let out = String::from_utf8_lossy(&out);
    let line = out.lines().find(|l| l.contains(&task_id)).unwrap_or_else(|| panic!("no {task_id} in {out}"));
    assert!(line.contains("interrupted"), "{out}");
    assert!(!report_path(&f).exists(), "status created report.html");
}

#[test]
fn a_status_that_settles_nothing_leaves_the_page_alone() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let task_id = create_titled(&f, "queued and untouched");
    f.cmd().args(["report", "--json"]).assert().success();
    assert!(page(&f).contains("queued and untouched"));

    let outside = tempfile::tempdir().unwrap();
    let calls: [(&str, Vec<&str>); 4] = [
        ("status", vec!["status"]),
        ("status --json", vec!["status", "--json"]),
        ("status TASK --json", vec!["status", task_id.as_str(), "--json"]),
        ("status outside a repository", vec!["status"]),
    ];
    for (i, (what, args)) in calls.iter().enumerate() {
        let (bytes, mtime) = page_state(&f);
        std::thread::sleep(Duration::from_millis(1100));
        let mut c = if i == 3 { f.cmd_in(outside.path()) } else { f.cmd() };
        c.args(args);
        f.run_ok(c, what);
        let (bytes2, mtime2) = page_state(&f);
        assert!(bytes2 == bytes, "{what} changed report.html");
        assert_eq!(mtime2, mtime, "{what} touched report.html");
    }
}

/// SMALL-18 T1: settling the run of a reviewer whose runner is gone rewrites
/// the page, and the task keeps its 等審查 row (it stays in_review).
#[test]
fn status_that_settles_a_reviewer_run_rewrites_the_page_and_keeps_the_task_waiting_for_review() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let task_id = create_titled(&f, TITLE);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let w = json(&f, &["wait", "--run", d["run_id"].as_str().unwrap(), "--timeout", "180s", "--json"]);
    assert_eq!(w["task_status"], "in_review", "{w}");

    let r = f.review_start(&task_id, "claude-a", "slow", &["--timeout", "10m"]);
    let run_id = r["run_id"].as_str().unwrap().to_string();
    let runner_pid = wait_running(&f, &task_id, &run_id);
    f.cmd().args(["report", "--json"]).assert().success();
    let waiting = row("等審查", "wait", TITLE);
    assert!(page(&f).contains(&waiting), "{}", page(&f));
    let (before, mtime) = page_state(&f);
    common::kill_pid_only(runner_pid);
    wait_live_runs_zero(&f, "claude-a");
    assert!(page_state(&f).0 == before, "something rewrote report.html before status looked");

    std::thread::sleep(Duration::from_millis(1100));
    let v = json(&f, &["status", &task_id, "--json"]);
    assert_eq!(run_in(&v, &run_id)["status"], "interrupted", "{v}");
    assert_eq!(v["status"], "in_review", "{v}");
    let (_, mtime2) = page_state(&f);
    assert_ne!(mtime2, mtime, "settling the reviewer run did not rewrite report.html");
    let p = page(&f);
    assert!(p.contains(&waiting), "no 等審查 row for {TITLE}:\n{p}");
    assert!(!p.contains(&row("中斷", "bad", TITLE)), "a 中斷 row for {TITLE}:\n{p}");
}
