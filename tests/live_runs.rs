//! One mechanism, five doors: while any run of a task is still `queued` or
//! `running`, nothing else may start a second one or write the task's status
//! out from under it — and a verdict that arrives after the task has moved on
//! is recorded, not applied.
//!
//! Whole-branch review Important 1/3/5. The refusals are driven with run rows
//! written straight into `state.db` rather than with real concurrent runners:
//! the property under test is "a live run row refuses the write", and a race
//! staged with two real processes would test the scheduler instead.

mod common;

use rusqlite::params;

const LIVE_REVIEWER: &str = "01LIVEREVIEWERRUN0000000AA";
const LIVE_IMPLEMENTER: &str = "01LIVEIMPLEMENTERRUN0000BB";

/// A bare run row for `task_id`. Enough columns for the guards, which read
/// `run_id`, `task_id`, `role` and `status`.
fn seed_live_run(f: &common::Fixture, task_id: &str, run_id: &str, role: &str, status: &str) {
    let conn = f.state_conn();
    conn.execute(
        "INSERT INTO runs (run_id, task_id, attempt, role, provider, account, model,
           config_dir, allowed_tools, worktree, base_commit, spec, status,
           cancel_requested, timeout_seconds, run_dir)
         VALUES (?1, ?2, 1, ?3, 'anthropic', 'claude-a', 'haiku',
           '', 'Bash,Read', '', '', '{}', ?4, 0, 1800, '')",
        params![run_id, task_id, role, status],
    )
    .unwrap();
}

fn force_task_status(f: &common::Fixture, task_id: &str, status: &str) {
    let conn = f.state_conn();
    let n = conn
        .execute(
            "UPDATE tasks SET status = ?2 WHERE task_id = ?1",
            params![task_id, status],
        )
        .unwrap();
    assert_eq!(n, 1);
}

fn run_count(f: &common::Fixture) -> i64 {
    f.state_conn()
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap()
}

fn run_status(f: &common::Fixture, run_id: &str) -> String {
    f.state_conn()
        .query_row(
            "SELECT status FROM runs WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )
        .unwrap()
}

/// Every refusal has to name the run that is in the way, its role and its
/// status — "something is running" would leave the operator hunting.
fn assert_names_the_live_run(stderr: &str, run_id: &str, role: &str, status: &str) {
    for needle in [run_id, role, status] {
        assert!(
            stderr.contains(needle),
            "the refusal must name {needle}; got: {stderr}"
        );
    }
}

/// I1: `in_review` is both "waiting for a review" and "a review is running",
/// and it is a `dispatch` entry state. Rerouting a task with a reviewer still
/// in flight used to be accepted, leaving an implementer and a reviewer live
/// at once.
#[test]
fn dispatch_refuses_while_a_reviewer_run_of_the_task_is_live() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    force_task_status(&f, &task_id, "in_review");
    seed_live_run(&f, &task_id, LIVE_REVIEWER, "reviewer", "running");

    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_names_the_live_run(
        &String::from_utf8_lossy(&out.stderr),
        LIVE_REVIEWER,
        "reviewer",
        "running",
    );

    assert_eq!(run_count(&f), 1, "no run row may be inserted for a refused dispatch");
    assert_eq!(
        f.task_status(&task_id),
        "in_review",
        "and the task must not be moved to running"
    );
}

/// I5: every `resume` precondition is about *this* run. R1 interrupted, the
/// operator reroutes (R2 starts in the same worktree, same attempt), then
/// resumes R1 — two workers committing into one worktree.
#[test]
fn resume_refuses_while_another_implementer_run_of_the_task_is_live() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _wt) = f.seed_run("claude-b", common::Fixture::SPEC);
    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE runs SET status = 'interrupted', started_at = '2026-09-20T00:00:00Z'
             WHERE run_id = ?1",
            [&run_id],
        )
        .unwrap();
    }
    seed_live_run(&f, &task_id, LIVE_IMPLEMENTER, "implementer", "queued");

    let out = f
        .cmd()
        .args(["resume", &run_id])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_names_the_live_run(
        &String::from_utf8_lossy(&out.stderr),
        LIVE_IMPLEMENTER,
        "implementer",
        "queued",
    );

    assert_eq!(
        run_status(&f, &run_id),
        "interrupted",
        "prepare_resume must not have put the run back to queued"
    );
    assert_eq!(run_count(&f), 2, "and no new run row appeared");
}

/// The same guard covers a live reviewer: resuming an implementer run under
/// an in-flight review would move the task to `running` while the review is
/// still going to answer for the candidate it was given.
#[test]
fn resume_refuses_while_a_reviewer_run_of_the_task_is_live() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _wt) = f.seed_run("claude-b", common::Fixture::SPEC);
    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE runs SET status = 'interrupted', started_at = '2026-09-20T00:00:00Z'
             WHERE run_id = ?1",
            [&run_id],
        )
        .unwrap();
    }
    force_task_status(&f, &task_id, "in_review");
    seed_live_run(&f, &task_id, LIVE_REVIEWER, "reviewer", "running");

    let out = f
        .cmd()
        .args(["resume", &run_id])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_names_the_live_run(
        &String::from_utf8_lossy(&out.stderr),
        LIVE_REVIEWER,
        "reviewer",
        "running",
    );
    assert_eq!(run_status(&f, &run_id), "interrupted");
    assert_eq!(f.task_status(&task_id), "in_review");
}

/// I1's `submit` half: `submit` writes `tasks.status` whenever the run is not
/// live, and a reviewer run in flight is about to write it too — with the
/// verdict for the candidate this submit may be replacing.
#[test]
fn submit_refuses_while_a_reviewer_run_of_the_task_is_live() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _wt) = f.seed_run("claude-b", common::Fixture::SPEC);
    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE runs SET status = 'completed' WHERE run_id = ?1",
            [&run_id],
        )
        .unwrap();
    }
    force_task_status(&f, &task_id, "in_review");
    seed_live_run(&f, &task_id, LIVE_REVIEWER, "reviewer", "running");

    let result = f.home_path().join("hand-fixed-result.json");
    std::fs::write(
        &result,
        serde_json::json!({
            "task_id": task_id,
            "status": "completed",
            "summary": "hand-fixed by the operator",
            "claims": [{"kind": "completion", "text": "done", "evidence": []}],
            "verification_requests": [],
        })
        .to_string(),
    )
    .unwrap();

    let out = f
        .cmd()
        .args(["submit", "-f"])
        .arg(&result)
        .args(["--run", &run_id])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_names_the_live_run(
        &String::from_utf8_lossy(&out.stderr),
        LIVE_REVIEWER,
        "reviewer",
        "running",
    );

    assert_eq!(f.task_status(&task_id), "in_review", "the status write was refused");
    let claims: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM claims", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        claims, 0,
        "the refusal comes before ingest, so a refused submit writes no claims either"
    );
}

/// I3: `review start`'s in-flight check used to be a check-then-act with a
/// `git worktree add`, an auth probe and the stale-report deletion sitting
/// between the check and the run-row insert. The deletion is the part with
/// teeth: a `review start` that is going to be refused must not delete a live
/// reviewer's report on its way out.
#[test]
fn review_start_refuses_while_a_run_is_live_and_leaves_the_stale_report_alone() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    force_task_status(&f, &task_id, "in_review");
    seed_live_run(&f, &task_id, LIVE_REVIEWER, "reviewer", "running");

    // The report the live reviewer is (notionally) about to have ingested.
    let review_wt = f
        .home_path()
        .join("repos")
        .join(f.repo_id())
        .join("worktrees")
        .join(format!("{task_id}-a1-review"));
    let report = review_wt.join(".agent").join("review.json");
    std::fs::create_dir_all(report.parent().unwrap()).unwrap();
    let body = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":["E001"]}]}}"#;
    std::fs::write(&report, body).unwrap();

    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task_id)
        .args(["review", "start", &task_id, "--profile", "claude-a"])
        .assert()
        .failure()
        .get_output()
        .clone();
    assert_names_the_live_run(
        &String::from_utf8_lossy(&out.stderr),
        LIVE_REVIEWER,
        "reviewer",
        "running",
    );

    assert!(
        report.is_file(),
        "a refused review start must not clear the live reviewer's report"
    );
    assert_eq!(std::fs::read_to_string(&report).unwrap(), body);
    assert_eq!(run_count(&f), 1, "and no second reviewer run row was inserted");
}

/// I1's headline scenario, end to end: a reviewer is in flight, the task moves
/// on underneath it, and the verdict lands afterwards. The verdict is still
/// recorded — a `reviews` row and a `verdict` event — but `tasks.status` is
/// left exactly where the reroute put it, and the run's own `status_change`
/// event says the write did not apply.
#[test]
fn a_verdict_that_lands_after_the_task_moved_on_is_recorded_but_not_applied() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    f.dispatch(&task_id, "claude-b", "hooks", &[]);
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");

    // The reviewer holds at the gate until this test releases it, so "the task
    // moved while the review was in flight" is staged, not raced.
    let gate = f.home_path().join("release-the-reviewer");
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_REVIEW_GATE", &gate)
        .args([
            "review", "start", &task_id, "--profile", "claude-a", "--model", "haiku", "--json",
        ])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();

    // Wait for the reviewer to actually be running before moving the task, so
    // the verdict is genuinely computed against a task that has since moved.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if run_status(&f, &reviewer_run) == "running" {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the reviewer run never reached running"
        );
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    // The reroute, as a conditional write — `dispatch` itself now refuses
    // while the reviewer run is live, which is the other half of this fix.
    {
        let conn = f.state_conn();
        let n = conn
            .execute(
                "UPDATE tasks SET status = 'running' WHERE task_id = ?1 AND status = 'in_review'",
                [&task_id],
            )
            .unwrap();
        assert_eq!(n, 1, "the task must still have been in_review at this point");
    }

    std::fs::write(&gate, "go").unwrap();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();

    assert_eq!(
        f.task_status(&task_id),
        "running",
        "the stale verdict must not have been stamped over the rerouted task"
    );

    let conn = f.state_conn();
    let verdict: String = conn
        .query_row(
            "SELECT verdict FROM reviews WHERE reviewer_run_id = ?1",
            [&reviewer_run],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(verdict, "PASS", "the review itself is still on the record");
    let verdict_events: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND type = 'verdict'",
            [&reviewer_run],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(verdict_events, 1);

    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = 'status_change'
             ORDER BY seq DESC LIMIT 1",
            [&reviewer_run],
            |r| r.get(0),
        )
        .unwrap();
    let detail: serde_json::Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["review_verdict"], "PASS");
    assert_eq!(
        detail["task_status_written"],
        serde_json::Value::Null,
        "nothing was written to tasks.status"
    );
    assert_eq!(detail["task_status_not_applied"]["intended"], "pass");
    assert_eq!(detail["task_status_not_applied"]["found"], "running");

    // ...and the same fact is in the run's own log, for an operator who is
    // reading the run dir rather than the event stream.
    let log = std::fs::read_to_string(
        f.home_path()
            .join("repos")
            .join(f.repo_id())
            .join("runs")
            .join(&reviewer_run)
            .join("runner-error.log"),
    )
    .expect("the not-applied verdict must leave a runner-error.log line");
    assert!(log.contains("was NOT moved to 'pass'"), "got {log}");
    assert!(log.contains(&task_id), "got {log}");
}
