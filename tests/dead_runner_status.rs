//! SMALL-11 (4): `thoth status` settles a `running` run whose runner process
//! is gone (stale heartbeat, no thoth process at the recorded pid) as
//! `interrupted` with reason `crash`; live-run counts leave such a run out;
//! a run whose runner is alive is never settled, however old its heartbeat.
//! Nothing here ages a heartbeat by hand: the tests wait for real time.
//!
//! TEST-7b: the debug-only switch `THOTH_TEST_HEARTBEAT_MS` (`HEARTBEAT_MS`)
//! is set on the commands that start a runner (dispatch, review start) and
//! on those that judge staleness here (status, profile usage): the runner
//! beats every 0.5 s and a heartbeat is stale after 1.5 s, the same 3x ratio
//! as a release build's 5 s beats and 15 s threshold. Every other command
//! (create, wait, events, the cleanup cancel) stays on `Fixture::cmd` without
//! it, so cancel keeps today's 15 s threshold.

mod common;

use common::Fixture;
use serde_json::Value;
use std::time::{Duration, Instant};

/// The beat the switch asks for; three of them make a heartbeat stale.
const HEARTBEAT_MS: &str = "500";

/// `Fixture::cmd` with the heartbeat switch on.
fn beat_cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("THOTH_TEST_HEARTBEAT_MS", HEARTBEAT_MS);
    c
}

fn json(f: &Fixture, args: &[&str]) -> Value {
    let mut c = beat_cmd(f);
    c.args(args);
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_slice(&out).unwrap_or_else(|e| panic!("{args:?}: not JSON ({e})"))
}

/// The run `run_id` as `status TASK --json` shows it.
fn run_row(f: &Fixture, task_id: &str, run_id: &str) -> Value {
    let v = json(f, &["status", task_id, "--json"]);
    v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run_id)
        .cloned()
        .unwrap_or_else(|| panic!("no run {run_id} in {v}"))
}

/// Polls `status TASK` until the run is `running`, which is when its runner
/// pid and heartbeat are recorded; returns the runner pid.
fn wait_running(f: &Fixture, task_id: &str, run_id: &str) -> u32 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let row = run_row(f, task_id, run_id);
        if row["status"] == "running" {
            if let Some(pid) = row["runner_pid"].as_u64() {
                return pid as u32;
            }
        }
        assert!(Instant::now() < deadline, "run {run_id} never reached running: {row}");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Polls `profile usage --json` until `profile` has no live run; it does once
/// the heartbeat of a run whose runner is gone is 1.5 seconds old (three
/// beats of the heartbeat switch; 15 seconds without it).
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

fn settled_events(f: &Fixture, task_id: &str, run_id: &str) -> Vec<Value> {
    f.events_json(task_id)
        .into_iter()
        .filter(|e| {
            e["type"] == "status_change"
                && e["run_id"] == run_id
                && e["detail"]["settled_by"] == "status"
        })
        .collect()
}

fn dispatch_slow(f: &Fixture) -> (String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    // what `Fixture::dispatch` runs, plus the heartbeat switch for the runner
    let mut c = beat_cmd(f);
    c.env("FAKE_SCENARIO", "slow")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--json"])
        .args(["--timeout", "10m", "--model", "sonnet", "--effort", "medium"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    (task_id, d["run_id"].as_str().unwrap().to_string())
}

#[test]
fn an_implementer_run_whose_runner_was_killed_is_settled_as_a_crash_by_status() {
    let f = common::fixture();
    let (task_id, run_id) = dispatch_slow(&f);
    let runner_pid = wait_running(&f, &task_id, &run_id);

    // only the runner: no process tree kill
    common::kill_pid_only(runner_pid);
    wait_live_runs_zero(&f, "claude-b");

    // counting a dead run out does not settle it; status does
    let after = run_row(&f, &task_id, &run_id);
    assert_eq!(after["status"], "interrupted", "{after}");
    assert_eq!(after["interrupted_reason"], "crash", "{after}");
    let error = after["error_text"].as_str().unwrap();
    assert!(error.contains(&runner_pid.to_string()), "{error}");
    assert!(error.contains("gone"), "{error}");
    assert_eq!(f.task_status(&task_id), "interrupted");

    let events = settled_events(&f, &task_id, &run_id);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["detail"]["to"], "interrupted", "{events:?}");
    // `status` with no TASK shows the settled state too
    let list = json(&f, &["status", "--json"]);
    let row = list["tasks"].as_array().unwrap().iter().find(|t| t["task_id"] == task_id.as_str()).unwrap();
    assert_eq!(row["status"], "interrupted", "{list}");
    assert_eq!(row["live_run"], Value::Null, "{list}");
    // and a second look settles nothing more
    assert_eq!(settled_events(&f, &task_id, &run_id).len(), 1);
}

#[test]
fn a_reviewer_run_whose_runner_was_killed_is_settled_and_its_task_stays_in_review() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task_id, "claude-b", "hooks", &[]);
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");

    // a gate file nobody makes: the reviewer waits, its run stays running
    let gate = f.home_path().join("never-made");
    let mut c = beat_cmd(&f);
    c.env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_REVIEW_GATE", &gate)
        .args(["review", "start", &task_id, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--json"]);
    let first: Value = serde_json::from_slice(&f.run_ok(c, "review start")).unwrap();
    let run_id = first["run_id"].as_str().unwrap().to_string();
    let runner_pid = wait_running(&f, &task_id, &run_id);

    common::kill_pid_only(runner_pid);
    wait_live_runs_zero(&f, "claude-a");

    let after = run_row(&f, &task_id, &run_id);
    assert_eq!(after["status"], "interrupted", "{after}");
    assert_eq!(after["interrupted_reason"], "crash", "{after}");
    assert_eq!(f.task_status(&task_id), "in_review");
    assert_eq!(settled_events(&f, &task_id, &run_id).len(), 1);

    // the review can be started again
    std::fs::write(&gate, "go").unwrap();
    let mut c = beat_cmd(&f);
    c.env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_REVIEW_GATE", &gate)
        .args(["review", "start", &task_id, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--json"]);
    let second: Value = serde_json::from_slice(&f.run_ok(c, "second review start")).unwrap();
    f.cmd()
        .args(["wait", "--run", second["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();
}

#[test]
fn a_run_with_a_live_runner_stays_running_however_stale_its_heartbeat() {
    let f = common::fixture();
    let (task_id, run_id) = dispatch_slow(&f);
    let runner_pid = wait_running(&f, &task_id, &run_id);

    // Unix: freeze the runner so its heartbeat really goes stale while the
    // process is alive: 2 s is more than the switch's 1.5 s threshold.
    // Elsewhere the runner just keeps heartbeating.
    #[cfg(unix)]
    {
        let stop = std::process::Command::new("kill").args(["-STOP", &runner_pid.to_string()]).status().unwrap();
        assert!(stop.success());
        std::thread::sleep(Duration::from_secs(2));
    }
    for _ in 0..3 {
        let row = run_row(&f, &task_id, &run_id);
        assert_eq!(row["status"], "running", "{row}");
        std::thread::sleep(Duration::from_millis(200));
    }
    #[cfg(unix)]
    {
        let row = run_row(&f, &task_id, &run_id);
        assert_eq!(row["heartbeat_stale"], true, "{row}");
        let cont = std::process::Command::new("kill").args(["-CONT", &runner_pid.to_string()]).status().unwrap();
        assert!(cont.success());
    }
    assert_eq!(f.task_status(&task_id), "running");
    assert!(settled_events(&f, &task_id, &run_id).is_empty());
    let v = json(&f, &["profile", "usage", "--json"]);
    let live: i64 = v["profiles"]
        .as_array()
        .or(v.as_array())
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == "claude-b")
        .unwrap()["live_runs"]
        .as_i64()
        .unwrap();
    assert_eq!(live, 1, "{v}");

    // leave nothing running (without the switch: cancel keeps today's 15 s
    // threshold, so a beat that is late after SIGCONT has today's room)
    f.cmd().args(["cancel", &run_id, "--json"]).assert().success();
    let _ = runner_pid;
}
