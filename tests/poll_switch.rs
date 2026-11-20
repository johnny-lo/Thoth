//! TEST-2: the test-only switch `THOTH_TEST_POLL_MS` (debug builds only)
//! shortens the 1 s poll of the runner and of both `thoth wait` forms; the
//! fixture's `cmd` sets it to 100 ms. Observed only through the CLI and its
//! `--json`: a spec review held by `FAKE_REVIEW_GATE` with a 1 s run timeout
//! beats the timeout only when the runner polls faster than once a second,
//! and staggered waits all answer soon after a cancelled run ends only when
//! they poll faster than once a second. Always the fakes and a temporary
//! THOTH_HOME; the database and run directories are never read.

mod common;

use common::Fixture;
use serde_json::Value;
use std::time::{Duration, Instant, SystemTime};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

fn json_of(f: &Fixture, c: assert_cmd::Command, what: &str) -> Value {
    let out = f.run_ok(c, what);
    let text = String::from_utf8_lossy(&out);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{what} printed no JSON ({e}): {text}"))
}

fn setup() -> (Fixture, String) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let (task, _base) = f.create_task(Fixture::SPEC);
    (f, task)
}

fn rfc3339(s: &str, what: &str) -> OffsetDateTime {
    OffsetDateTime::parse(s, &Rfc3339).unwrap_or_else(|e| panic!("{what} is not RFC 3339 ({e}): {s}"))
}

/// Polls `status TASK --json` until the run has a `started_at`, and returns it.
fn started_at(f: &Fixture, task: &str, run: &str) -> OffsetDateTime {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let r = f.run_of(task, run);
        if let Some(s) = r["started_at"].as_str() {
            return rfc3339(s, "started_at");
        }
        assert!(Instant::now() < deadline, "run {run} never got a started_at: {r}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Starts a gated spec review with a 1 s run timeout (with or without the
/// switch), opens the gate 0.2 s after the run's `started_at`, and returns
/// `wait --run RUN --json`.
fn gated_spec_review(with_switch: bool) -> Value {
    let (f, task) = setup();
    let gate = f.home_path().join("release-the-spec-reviewer");
    let mut c = f.cmd();
    if !with_switch {
        c.env_remove("THOTH_TEST_POLL_MS");
    }
    c.env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_REVIEW_GATE", &gate)
        .args([
            "spec", "review", "start", &task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium",
            "--timeout", "1s", "--json",
        ]);
    let started = json_of(&f, c, "spec review start");
    let run = started["run_id"].as_str().unwrap_or_else(|| panic!("no run_id: {started}")).to_string();

    let open_at = started_at(&f, &task, &run) + Duration::from_millis(200);
    let now = OffsetDateTime::from(SystemTime::now());
    if open_at > now {
        std::thread::sleep((open_at - now).try_into().unwrap());
    }
    std::fs::write(&gate, "go").unwrap();

    let mut c = f.cmd();
    c.args(["wait", "--run", &run, "--timeout", "60s", "--json"]);
    json_of(&f, c, "wait --run")
}

#[test]
fn a_gated_spec_review_beats_a_one_second_timeout_when_the_switch_is_set() {
    let v = gated_spec_review(true);
    assert_eq!(v["status"], "completed", "{v}");
    assert_eq!(v["interrupted_reason"], Value::Null, "{v}");
    assert_eq!(v["verdict"], "APPROVE", "{v}");
    assert_eq!(v["timed_out"], false, "{v}");
}

#[test]
fn without_the_switch_the_runner_still_polls_once_a_second() {
    let v = gated_spec_review(false);
    assert_eq!(v["status"], "failed", "{v}");
    assert_eq!(v["interrupted_reason"], "timeout", "{v}");
    assert_eq!(v["verdict"], Value::Null, "{v}");
    assert_eq!(v["timed_out"], false, "{v}");
}

#[test]
fn both_waits_answer_soon_after_the_run_ends_when_the_switch_is_set() {
    let (f, task) = setup();
    let started = f.dispatch(&task, "claude-a", "slow", &[]);
    let run = started["run_id"].as_str().unwrap_or_else(|| panic!("no run_id: {started}")).to_string();
    f.wait_for_task_status(&task, &["running"], 60);

    // Ten waits, alternating task and run form, started about 0.1 s apart;
    // each thread notes the system time the moment its wait exits.
    let mut waits = Vec::new();
    for i in 0..10 {
        let form = if i % 2 == 0 { "task" } else { "run" };
        let mut c = f.cmd();
        if form == "task" {
            c.args(["wait", &task, "--timeout", "120s", "--json"]);
        } else {
            c.args(["wait", "--run", &run, "--timeout", "120s", "--json"]);
        }
        waits.push((
            form,
            std::thread::spawn(move || {
                let out = c.output().expect("spawn thoth wait");
                (SystemTime::now(), out)
            }),
        ));
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));

    let mut c = f.cmd();
    c.args(["cancel", &run, "--json"]);
    f.run_ok(c, "cancel");

    let mut ends = Vec::new();
    for (form, handle) in waits {
        let (ended, out) = handle.join().expect("wait thread");
        let text = String::from_utf8_lossy(&out.stdout);
        assert_eq!(
            out.status.code(),
            Some(0),
            "{form} wait exited {:?}; stdout: {text}; stderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{form} wait printed no JSON ({e}): {text}"));
        assert_eq!(v["timed_out"], false, "{form} wait: {v}");
        assert_eq!(v["status"], "cancelled", "{form} wait: {v}");
        ends.push((form, ended));
    }

    let r = f.run_of(&task, &run);
    let ended_at = rfc3339(r["ended_at"].as_str().unwrap_or_else(|| panic!("run has no ended_at: {r}")), "ended_at");
    let delays: Vec<(&str, f64)> = ends
        .iter()
        .map(|(form, ended)| (*form, (OffsetDateTime::from(*ended) - ended_at).as_seconds_f64()))
        .collect();
    assert!(
        delays.iter().all(|(_, d)| *d < 0.5),
        "every wait should answer within 0.5 s of the run's ended_at {ended_at}; delays (form, seconds): {delays:?}"
    );
}
