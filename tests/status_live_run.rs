//! PANEL-3 AC3: `thoth status --json` (no TASK) gives every task a
//! `live_run`: its newest queued or running run as `{run_id, role, profile,
//! model, status}`, or null. Runs are stopped with the fakes' own switches
//! (`FAKE_REVIEW_GATE`, the `slow` scenario) and awaited with `thoth wait`;
//! the database is never opened. Always the fakes and a temporary THOTH_HOME.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::Path;

/// `f.cmd()` without any inherited variable whose name holds KEY, TOKEN,
/// SECRET or PASSWORD (names only are read), and without the two benchmark
/// keys.
fn cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for name in std::env::vars_os().map(|(name, _)| name) {
        let upper = name.to_string_lossy().to_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c.env_remove("AISTUPIDLEVEL_API_KEY").env_remove("ARTIFICIALANALYSIS_API_KEY");
    c
}

fn json_of(f: &Fixture, c: assert_cmd::Command, what: &str) -> Value {
    let out = f.run_ok(c, what);
    let text = String::from_utf8_lossy(&out);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{what} printed no JSON ({e}): {text}"))
}

fn setup() -> Fixture {
    let f = common::fixture();
    cmd(&f).arg("init").assert().success();
    for id in ["claude-a", "claude-b"] {
        let dir = f.home_path().join("profiles").join(id);
        std::fs::create_dir_all(&dir).unwrap();
        cmd(&f)
            .args(["profile", "add", id, "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
            .arg(&dir)
            .arg("--json")
            .assert()
            .success();
    }
    f
}

fn create_task(f: &Fixture, spec: &str) -> String {
    let file = f.home_path().join("live-run-spec.json");
    std::fs::write(&file, spec).unwrap();
    let mut c = cmd(f);
    c.args(["task", "create", "-f"]).arg(&file).arg("--json");
    json_of(f, c, "task create")["task_id"].as_str().unwrap().to_string()
}

/// The row of `task` in `status --json` run in `dir` (the repository by default).
fn row_in(f: &Fixture, dir: Option<&Path>, task: &str) -> Value {
    let mut c = cmd(f);
    if let Some(dir) = dir {
        c.current_dir(dir);
    }
    c.args(["status", "--json"]);
    let v = json_of(f, c, "status --json");
    v["tasks"]
        .as_array()
        .unwrap_or_else(|| panic!("status --json has no tasks: {v}"))
        .iter()
        .find(|t| t["task_id"] == task)
        .cloned()
        .unwrap_or_else(|| panic!("no task {task} in {v}"))
}

fn wait_run(f: &Fixture, run: &str) {
    let mut c = cmd(f);
    c.args(["wait", "--run", run, "--timeout", "180s", "--json"]);
    f.run_ok(c, "wait --run");
}

/// Dispatches `task` on claude-b and waits until it is in review.
fn dispatched_and_in_review(f: &Fixture, task: &str) {
    let mut c = cmd(f);
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", task)
        .args(["dispatch", task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let run = json_of(f, c, "dispatch")["run_id"].as_str().unwrap().to_string();
    wait_run(f, &run);
    assert_eq!(row_in(f, None, task)["status"], "in_review");
}

/// Keys of `live_run` exactly as AC3 names them.
fn assert_live(live: &Value, run: &str, role: &str, profile: &str, model: &str) {
    let keys: Vec<&str> = live.as_object().unwrap_or_else(|| panic!("not an object: {live}")).keys().map(String::as_str).collect();
    let mut keys = keys;
    keys.sort();
    assert_eq!(keys, ["model", "profile", "role", "run_id", "status"], "{live}");
    assert_eq!(live["run_id"], run, "{live}");
    assert_eq!(live["role"], role, "{live}");
    assert_eq!(live["profile"], profile, "{live}");
    assert_eq!(live["model"], model, "{live}");
    assert!(live["status"] == "queued" || live["status"] == "running", "{live}");
}

#[test]
fn live_run_names_a_gated_review_and_clears_once_it_ends() {
    let f = setup();
    let task = create_task(&f, Fixture::SPEC_TWO_AC);
    assert_eq!(row_in(&f, None, &task)["live_run"], Value::Null);
    dispatched_and_in_review(&f, &task);
    assert_eq!(row_in(&f, None, &task)["live_run"], Value::Null);

    let gate = f.home_path().join("release-the-reviewer");
    let mut c = cmd(&f);
    // A REJECT keeps the task listed afterwards (a PASS would hide it).
    c.env("FAKE_SCENARIO", "review_reject")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_REVIEW_GATE", &gate)
        .args(["review", "start", &task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium", "--json"]);
    let started = json_of(&f, c, "review start");
    let run = started["run_id"].as_str().unwrap().to_string();

    let row = row_in(&f, None, &task);
    assert_eq!(row["status"], "in_review", "{row}");
    assert_live(&row["live_run"], &run, "reviewer", started["profile"].as_str().unwrap(), started["model"].as_str().unwrap());

    // Outside a repository the same task carries the same live run.
    let outside = tempfile::tempdir().unwrap();
    let there = row_in(&f, Some(outside.path()), &task);
    assert_live(&there["live_run"], &run, "reviewer", started["profile"].as_str().unwrap(), started["model"].as_str().unwrap());
    // The runner may move the run from queued to running between the two
    // reads, so the comparison below leaves `status` out (assert_live checked it).
    let without_status = |v: &Value| {
        let mut v = v.clone();
        if let Some(live) = v["live_run"].as_object_mut() {
            live.remove("status");
        }
        v
    };
    let (there, row) = (without_status(&there), without_status(&row));
    assert_eq!(there["live_run"], row["live_run"], "{there}");

    // The human output is unchanged: no live run in it.
    let mut c = cmd(&f);
    c.arg("status");
    let text = String::from_utf8(f.run_ok(c, "status")).unwrap();
    assert!(!text.contains(&run), "{text}");

    std::fs::write(&gate, "go").unwrap();
    wait_run(&f, &run);
    let row = row_in(&f, None, &task);
    assert_eq!(row["status"], "reject", "{row}");
    assert_eq!(row["live_run"], Value::Null, "{row}");
}

/// The CLI creates tasks `queued`, never `draft`; the spec review leaves the
/// status alone, whatever it is (the panel tests cover a draft row).
#[test]
fn live_run_names_a_spec_review_and_the_task_status_stays() {
    let f = setup();
    let task = create_task(&f, Fixture::SPEC);
    let status = row_in(&f, None, &task)["status"].clone();

    let gate = f.home_path().join("release-the-spec-reviewer");
    let mut c = cmd(&f);
    c.env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_REVIEW_GATE", &gate)
        .args(["spec", "review", "start", &task, "--profile", "claude-a", "--model", "haiku", "--effort", "medium", "--json"]);
    let started = json_of(&f, c, "spec review start");
    let run = started["run_id"].as_str().unwrap().to_string();

    let row = row_in(&f, None, &task);
    assert_eq!(row["status"], status, "the spec review leaves the task where it was: {row}");
    assert_live(&row["live_run"], &run, "spec_reviewer", "claude-a", "haiku");

    std::fs::write(&gate, "go").unwrap();
    wait_run(&f, &run);
    let row = row_in(&f, None, &task);
    assert_eq!(row["status"], status, "{row}");
    assert_eq!(row["live_run"], Value::Null, "{row}");
}

#[test]
fn live_run_names_a_running_implementer() {
    let f = setup();
    let task = create_task(&f, Fixture::SPEC);
    let mut c = cmd(&f);
    c.env("FAKE_SCENARIO", "slow")
        .env("FAKE_TASK_ID", &task)
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let started = json_of(&f, c, "dispatch");
    let run = started["run_id"].as_str().unwrap().to_string();

    let row = row_in(&f, None, &task);
    assert_live(&row["live_run"], &run, "implementer", "claude-b", "sonnet");

    let mut c = cmd(&f);
    c.args(["cancel", &run, "--json"]);
    f.run_ok(c, "cancel");
    wait_run(&f, &run);
    // A cancelled task leaves the list; any row still there has no live run.
    let mut c = cmd(&f);
    c.args(["status", "--json"]);
    let v = json_of(&f, c, "status --json");
    for t in v["tasks"].as_array().unwrap() {
        assert_eq!(t["live_run"], Value::Null, "{t}");
    }
}
