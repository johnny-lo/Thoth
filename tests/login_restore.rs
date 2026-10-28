//! FC-1: a named `dispatch`, `review start` or `spec review start` whose auth
//! check passes marks a `logged_out` profile active again and records one
//! `login_restored` quota event; when the check fails nothing changes and no
//! run is created. Everything runs on fake-claude and a temporary THOTH_HOME;
//! `FAKE_LOGOUT_MARKER` makes `auth status` answer logged out while its file
//! exists.

mod common;

use common::Fixture;
use serde_json::{json, Value};

// ---- copied from tests/failure_classes.rs

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

/// The detail of the profile's newest quota event of `kind`.
fn newest_quota_detail(f: &Fixture, id: &str, kind: &str) -> Value {
    let detail: String = f
        .global_conn()
        .query_row(
            "SELECT detail FROM quota_events WHERE profile_id = ?1 AND kind = ?2 ORDER BY seq DESC LIMIT 1",
            [id, kind],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&detail).unwrap()
}

// ---- this file's own helpers

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn restored_count(f: &Fixture, id: &str) -> usize {
    quota_kinds(f, id).iter().filter(|k| *k == "login_restored").count()
}

fn run_count(f: &Fixture, task: &str, role: &str) -> i64 {
    f.state_conn()
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE task_id = ?1 AND role = ?2",
            [task, role],
            |r| r.get(0),
        )
        .unwrap()
}

fn mark_logged_out(f: &Fixture, id: &str) {
    f.global_conn()
        .execute("UPDATE profiles SET status='logged_out' WHERE profile_id=?1", [id])
        .unwrap();
}

fn marker(f: &Fixture) -> std::path::PathBuf {
    f.home_path().join("logout.marker")
}

/// A thoth command for `task` with the scenario and the logout marker set.
fn cmd_for(f: &Fixture, scenario: &str, task: &str, args: &[&str]) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task)
        .env("FAKE_LOGOUT_MARKER", marker(f))
        .args(args);
    c
}

/// Runs a command that must be refused by the auth check (exit 1).
fn assert_refused(c: &mut assert_cmd::Command) {
    let out = c.output().expect("spawn thoth");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("failed its auth check"), "stderr: {stderr}");
}

/// Runs a command that must succeed and returns its `run_id`.
fn run_id_of(f: &Fixture, c: assert_cmd::Command, what: &str) -> String {
    let v: Value = serde_json::from_slice(&f.run_ok(c, what)).unwrap();
    v["run_id"].as_str().unwrap().to_string()
}

fn wait_run(f: &Fixture, run: &str) {
    let mut c = f.cmd();
    c.args(["wait", "--run", run, "--timeout", "180s", "--json"]);
    f.run_ok(c, "wait --run");
}

#[test]
fn login_restore_named_dispatch_marks_a_logged_out_profile_active_once() {
    let f = setup();
    f.add_profile("claude-b");
    let (t1, _) = f.create_task(Fixture::SPEC);
    mark_logged_out(&f, "claude-b");
    std::fs::write(marker(&f), "1\n").unwrap();
    let args = ["dispatch", t1.as_str(), "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"];

    // Deliberately wrong: the auth check fails, so nothing is restored.
    assert_refused(&mut cmd_for(&f, "ok", &t1, &args));
    assert_eq!(profile_status(&f, "claude-b"), "logged_out");
    assert_eq!(restored_count(&f, "claude-b"), 0);
    assert_eq!(run_count(&f, &t1, "implementer"), 0);

    std::fs::remove_file(marker(&f)).unwrap();
    let r1 = run_id_of(&f, cmd_for(&f, "ok", &t1, &args), "dispatch");
    assert_eq!(profile_status(&f, "claude-b"), "active");
    assert_eq!(restored_count(&f, "claude-b"), 1);
    assert_eq!(
        newest_quota_detail(&f, "claude-b", "login_restored"),
        json!({"source": "dispatch", "task_id": t1, "run_id": r1})
    );
    assert_eq!(f.wait_for(&t1, "180s").1, "in_review");

    // Control: an active profile is not restored a second time.
    let (t2, _) = f.create_task(Fixture::SPEC);
    let args2 = ["dispatch", t2.as_str(), "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"];
    f.run_ok(cmd_for(&f, "ok", &t2, &args2), "dispatch");
    assert_eq!(restored_count(&f, "claude-b"), 1);
    assert_eq!(f.wait_for(&t2, "180s").1, "in_review");
}

#[test]
fn login_restore_review_start_marks_a_logged_out_reviewer_profile_active() {
    let f = setup();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (t, _) = f.create_task(Fixture::SPEC);
    f.dispatch(&t, "claude-b", "ok", &[]);
    assert_eq!(f.wait_for(&t, "180s").1, "in_review");
    mark_logged_out(&f, "claude-a");
    std::fs::write(marker(&f), "1\n").unwrap();
    let args = ["review", "start", t.as_str(), "--profile", "claude-a", "--model", "haiku", "--json"];

    // Deliberately wrong: the auth check fails, so nothing is restored.
    assert_refused(&mut cmd_for(&f, "review_pass", &t, &args));
    assert_eq!(profile_status(&f, "claude-a"), "logged_out");
    assert_eq!(restored_count(&f, "claude-a"), 0);
    assert_eq!(run_count(&f, &t, "reviewer"), 0);

    std::fs::remove_file(marker(&f)).unwrap();
    let r = run_id_of(&f, cmd_for(&f, "review_pass", &t, &args), "review start");
    assert_eq!(profile_status(&f, "claude-a"), "active");
    assert_eq!(restored_count(&f, "claude-a"), 1);
    assert_eq!(
        newest_quota_detail(&f, "claude-a", "login_restored"),
        json!({"source": "review_start", "task_id": t, "run_id": r})
    );
    wait_run(&f, &r);
}

#[test]
fn login_restore_spec_review_start_marks_a_logged_out_profile_active() {
    let f = setup();
    f.add_profile("claude-a");
    let (t, _) = f.create_task(Fixture::SPEC);
    mark_logged_out(&f, "claude-a");
    std::fs::write(marker(&f), "1\n").unwrap();
    let args = ["spec", "review", "start", t.as_str(), "--profile", "claude-a", "--model", "haiku", "--json"];

    // Deliberately wrong: the auth check fails, so nothing is restored.
    assert_refused(&mut cmd_for(&f, "spec_review_approve", &t, &args));
    assert_eq!(profile_status(&f, "claude-a"), "logged_out");
    assert_eq!(restored_count(&f, "claude-a"), 0);
    assert_eq!(run_count(&f, &t, "spec_reviewer"), 0);

    std::fs::remove_file(marker(&f)).unwrap();
    let s = run_id_of(&f, cmd_for(&f, "spec_review_approve", &t, &args), "spec review start");
    assert_eq!(profile_status(&f, "claude-a"), "active");
    assert_eq!(restored_count(&f, "claude-a"), 1);
    assert_eq!(
        newest_quota_detail(&f, "claude-a", "login_restored"),
        json!({"source": "spec_review_start", "task_id": t, "run_id": s})
    );
    wait_run(&f, &s);
}
