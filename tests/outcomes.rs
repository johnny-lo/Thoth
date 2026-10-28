mod common;

fn run_status(f: &common::Fixture, run_id: &str) -> (String, Option<String>, Option<i64>, Option<String>) {
    let conn = f.state_conn();
    conn.query_row(
        "SELECT status, interrupted_reason, exit_code, error_text FROM runs WHERE run_id = ?1",
        [run_id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .unwrap()
}

#[test]
fn bad_result_json_lands_the_task_in_unverified_with_the_run_completed() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "bad_result", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();

    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "unverified");

    let (rstatus, reason, exit, _err) = run_status(&f, &run_id);
    assert_eq!(rstatus, "completed", "the harness itself succeeded");
    assert_eq!(reason, None);
    assert_eq!(exit, Some(0));

    let conn = f.state_conn();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM claims WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0, "an invalid result.json must not produce claims");

    let kinds: Vec<String> = conn
        .prepare("SELECT type FROM events WHERE task_id = ?1 ORDER BY seq")
        .unwrap()
        .query_map([&task_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    assert!(kinds.contains(&"result_invalid".to_string()), "{kinds:?}");
    assert!(!kinds.contains(&"result_ingested".to_string()), "{kinds:?}");

    // Open question 17's ruling, on the realistic path: re-dispatch straight out
    // of `unverified`, same attempt, same worktree, and a good result this time.
    let second = f.dispatch(&task_id, "claude-b", "ok", &[]);
    assert_eq!(second["attempt"].as_i64(), Some(1));
    assert_eq!(second["worktree_reused"].as_bool(), Some(true));
    let (code2, status2) = f.wait_for(&task_id, "120s");
    assert_eq!(code2, 0);
    assert_eq!(status2, "in_review");
    let claims: i64 = f
        .state_conn()
        .query_row(
            "SELECT COUNT(*) FROM claims WHERE run_id = ?1",
            [second["run_id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(claims, 1, "the second run's claims did get ingested");
}

/// A dispatch out of `unverified` after a `result_invalid` tells the new worker
/// why: the run's own spec carries the reason, the task's spec does not, and
/// prompt.txt has the section.
#[test]
fn a_reroute_from_unverified_tells_the_worker_why_the_result_was_rejected() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "bad_result", &[]);
    let first_run = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "unverified");

    let conn = f.state_conn();
    let (reason, rule): (String, i64) = {
        let detail: String = conn
            .query_row(
                "SELECT detail FROM events WHERE run_id = ?1 AND type = 'result_invalid'",
                [&first_run],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&detail).unwrap();
        (v["reason"].as_str().unwrap().to_string(), v["rule"].as_i64().unwrap())
    };

    let second = f.dispatch(&task_id, "claude-b", "ok", &[]);
    let second_run = second["run_id"].as_str().unwrap().to_string();
    let (code2, status2) = f.wait_for(&task_id, "120s");
    assert_eq!(code2, 0);
    assert_eq!(status2, "in_review");

    let (run_spec, run_dir): (String, String) = conn
        .query_row(
            "SELECT spec, run_dir FROM runs WHERE run_id = ?1",
            [&second_run],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&run_spec).unwrap();
    assert_eq!(v["reroute"]["reason"], serde_json::json!(reason));
    assert_eq!(v["reroute"]["rule"], serde_json::json!(rule));
    assert_eq!(v["reroute"]["previous_run"], serde_json::json!(first_run));
    assert!(v.get("rework").is_none(), "a reroute is not a rework: {v}");

    let task_spec: String = conn
        .query_row("SELECT spec FROM tasks WHERE task_id = ?1", [&task_id], |r| r.get(0))
        .unwrap();
    assert!(!task_spec.contains("reroute"), "the task's own spec must not change");

    let prompt = std::fs::read_to_string(std::path::Path::new(&run_dir).join("prompt.txt")).unwrap();
    assert!(prompt.contains("was not accepted"), "{prompt}");
    assert!(prompt.contains(&reason), "{prompt}");

    // the first run's prompt (a plain dispatch) has no such section
    let first_dir: String = conn
        .query_row("SELECT run_dir FROM runs WHERE run_id = ?1", [&first_run], |r| r.get(0))
        .unwrap();
    let first_prompt =
        std::fs::read_to_string(std::path::Path::new(&first_dir).join("prompt.txt")).unwrap();
    assert!(!first_prompt.contains("was not accepted"));
}

#[test]
fn a_quota_error_interrupts_the_run_and_parks_the_profile() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "quota", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();

    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "interrupted");

    let (rstatus, reason, exit, err) = run_status(&f, &run_id);
    assert_eq!(rstatus, "interrupted");
    assert_eq!(reason.as_deref(), Some("quota"));
    assert_eq!(exit, Some(1));
    assert!(err.unwrap().to_lowercase().contains("usage limit"));

    // the session id must be captured so resume has something to use
    let conn = f.state_conn();
    let sess: Option<String> = conn
        .query_row(
            "SELECT harness_session_id FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(sess.is_some(), "harness_session_id is the resume handle");

    let kinds: Vec<String> = conn
        .prepare("SELECT type FROM events WHERE task_id = ?1 ORDER BY seq")
        .unwrap()
        .query_map([&task_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    for expected in ["rate_limit_warning", "api_retry", "quota_exceeded", "run_interrupted"] {
        assert!(kinds.contains(&expected.to_string()), "missing {expected} in {kinds:?}");
    }

    let g = f.global_conn();
    let (pstatus, qcount): (String, i64) = (
        g.query_row("SELECT status FROM profiles WHERE profile_id='claude-b'", [], |r| r.get(0))
            .unwrap(),
        g.query_row(
            "SELECT COUNT(*) FROM quota_events WHERE profile_id='claude-b' AND kind='rate_limited'",
            [],
            |r| r.get(0),
        )
        .unwrap(),
    );
    assert_eq!(pstatus, "rate_limited");
    assert_eq!(qcount, 1);
}

#[test]
fn a_crash_with_no_result_event_interrupts_with_reason_crash() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "crash", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();

    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "interrupted");

    let (rstatus, reason, exit, _err) = run_status(&f, &run_id);
    assert_eq!(rstatus, "interrupted");
    assert_eq!(reason.as_deref(), Some("crash"));
    assert_eq!(exit, Some(137));

    // a crash is not a quota event, so the profile stays usable
    let g = f.global_conn();
    let pstatus: String = g
        .query_row("SELECT status FROM profiles WHERE profile_id='claude-b'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(pstatus, "active");

    // M2: a crash IS an interruption (`run_status == Interrupted`), so
    // `settle_terminal` must have written the `run_interrupted` event - and
    // it must not have written any `quota_events` row, since this was never
    // classified as a quota interruption.
    let conn = f.state_conn();
    let kinds: Vec<String> = conn
        .prepare("SELECT type FROM events WHERE task_id = ?1 ORDER BY seq")
        .unwrap()
        .query_map([&task_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    assert!(kinds.contains(&"run_interrupted".to_string()), "{kinds:?}");
    // `profile add` itself writes a `kind='note'` audit row, so filter to the
    // kind a real quota interruption would write (matches the assertion in
    // `a_quota_error_interrupts_the_run_and_parks_the_profile`).
    let qcount: i64 = g
        .query_row(
            "SELECT COUNT(*) FROM quota_events WHERE kind='rate_limited'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(qcount, 0, "a crash must not write a rate_limited quota_events row");
}

#[test]
fn a_wall_clock_timeout_fails_the_run_and_kills_the_child() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "slow", &["--timeout", "5s"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();

    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "failed");

    let (rstatus, reason, _exit, err) = run_status(&f, &run_id);
    assert_eq!(rstatus, "failed");
    assert_eq!(reason.as_deref(), Some("timeout"));
    assert!(err.unwrap().contains("timeout"));

    // the worker pid must be gone
    let conn = f.state_conn();
    let wpid: Option<i64> = conn
        .query_row("SELECT worker_pid FROM runs WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    let wpid = wpid.expect("worker_pid must have been recorded") as u32;
    // Matched by image too (`common::pid_alive`), not the bare pid: the
    // assertion must not trip on an unrelated process that got the number.
    assert!(
        !common::pid_alive(wpid, env!("CARGO_BIN_EXE_fake-claude")),
        "worker {wpid} survived the timeout kill"
    );

    // Controller pointer 3 / design §2: a wall-clock timeout is `failed`, not
    // `interrupted`, so `settle_terminal` must not have written a
    // `run_interrupted` event for it.
    let kinds: Vec<String> = conn
        .prepare("SELECT type FROM events WHERE task_id = ?1 ORDER BY seq")
        .unwrap()
        .query_map([&task_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    assert!(
        !kinds.contains(&"run_interrupted".to_string()),
        "a timeout must not write run_interrupted: {kinds:?}"
    );
}
