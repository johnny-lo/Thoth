mod common;

use predicates::prelude::*;

#[test]
fn dispatch_ok_reaches_in_review_and_the_event_cursor_advances_monotonically() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, base) = f.create_task(common::Fixture::SPEC);

    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(d["attempt"].as_i64(), Some(1));
    assert_eq!(d["worktree_reused"].as_bool(), Some(false));
    assert!(d["runner_pid"].as_u64().unwrap() > 0);

    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0, "wait should succeed, got status {status}");
    assert_eq!(status, "in_review");

    let conn = f.state_conn();
    let (rstatus, cand): (String, Option<String>) = conn
        .query_row(
            "SELECT status, candidate_commit FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rstatus, "completed");
    assert_eq!(cand.as_deref(), Some(base.as_str()), "the fake does not commit, so HEAD is still base");

    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM claims WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);

    // events --since walks forward without repeats
    let out = f
        .cmd()
        .args(["events", "--since", "0", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let seqs: Vec<i64> = v["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["seq"].as_i64().unwrap())
        .collect();
    assert!(!seqs.is_empty());
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "seqs not increasing: {seqs:?}");
    let last = v["last_seq"].as_i64().unwrap();
    assert_eq!(last, *seqs.last().unwrap());

    let out2 = f
        .cmd()
        .args(["events", "--since", &last.to_string(), "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v2: serde_json::Value =
        serde_json::from_str(String::from_utf8(out2).unwrap().trim()).unwrap();
    assert_eq!(v2["events"].as_array().unwrap().len(), 0, "cursor must not replay");
}

#[test]
fn status_lists_the_open_task_then_hides_nothing_while_in_review() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);

    f.cmd()
        .args(["status", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&task_id))
        .stdout(predicate::str::contains("\"status\":\"queued\""));

    f.dispatch(&task_id, "claude-b", "ok", &[]);
    let (code, _) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);

    // in_review is not terminal, so the task is still listed
    f.cmd()
        .args(["status", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"in_review\""));

    // per-task view shows the run with heartbeat info
    f.cmd()
        .args(["status", &task_id, "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"heartbeat_stale\":false"))
        .stdout(predicate::str::contains("\"status\":\"completed\""));
}

#[test]
fn status_still_lists_a_failed_task_since_it_is_re_dispatchable() {
    // M1 (fix round 1): `failed` joined `dispatch_allowed`'s entry states, so
    // `status` with no args must not hide it the way it hides the true dead
    // ends (pass/cancelled) - there is still actionable work here. Task 8 took
    // `reject` out of that set for the same reason.
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE tasks SET status='failed' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();
    }

    f.cmd()
        .args(["status", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&task_id))
        .stdout(predicate::str::contains("\"status\":\"failed\""));

    // #33 (final review): the reverse must also hold - a true dead end
    // (pass/cancelled) must not show up in the no-arg listing.
    let (pass_task_id, _) = f.create_task(common::Fixture::SPEC);
    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE tasks SET status='pass' WHERE task_id = ?1",
            [&pass_task_id],
        )
        .unwrap();
    }

    f.cmd()
        .args(["status", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&task_id))
        .stdout(predicate::str::contains(pass_task_id).not());
}

#[test]
fn wait_times_out_with_exit_code_three() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    // never dispatched, so the task stays queued
    let out = f
        .cmd()
        .args(["wait", &task_id, "--timeout", "2s", "--json"])
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["timed_out"].as_bool(), Some(true));
    assert_eq!(v["status"].as_str(), Some("queued"));
}

#[test]
fn dispatch_refuses_an_unknown_profile_and_a_running_task() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);

    f.cmd()
        .args(["dispatch", &task_id, "--profile", "nope"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no such profile"));

    // force the task into a state dispatch must refuse
    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE tasks SET status='running' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();
    }
    f.cmd()
        .args(["dispatch", &task_id, "--profile", "claude-b"])
        .assert()
        .failure()
        // Task 8 widened the entry set: `reject` is now a dispatch entry state
        // (the rework loop), so the refusal message names it too.
        .stderr(predicate::str::contains(
            "dispatch needs queued, interrupted, in_review, unverified, failed or reject",
        ));
}

#[test]
fn dispatch_moves_the_task_out_of_a_wait_terminal_status_before_it_returns() {
    // Without this, `wait` straight after a reroute returns the stale
    // `interrupted` status: `is_wait_terminal` includes it, and the runner needs
    // hundreds of milliseconds to get as far as writing `running`.
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);

    // put the task in the reroute entry state
    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE tasks SET status='interrupted' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();
    }

    // `slow` keeps the run in flight so the assertion below is deterministic
    let d = f.dispatch(&task_id, "claude-b", "slow", &["--timeout", "10m"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();

    let status: String = {
        let conn = f.state_conn();
        conn.query_row(
            "SELECT status FROM tasks WHERE task_id = ?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(
        status, "running",
        "dispatch must write the transition itself, not leave it to the runner"
    );

    // Do not leave a 60-second worker behind. `thoth cancel` does not exist
    // until Task 11, so set the flag the runner already polls once a second; it
    // settles the run itself within a second or two.
    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE runs SET cancel_requested = 1 WHERE run_id = ?1",
            [&run_id],
        )
        .unwrap();
    }
    assert_eq!(f.wait_for(&task_id, "60s").1, "cancelled");
}

#[test]
fn dispatch_accepts_an_unverified_task_and_reuses_its_worktree() {
    // Open question 17's ruling: a task whose result.json was invalid is
    // re-dispatchable. Same attempt, so the existing worktree is reused - the
    // worker's commits are not thrown away just because it filed a bad report.
    // The `bad_result` scenario that produces `unverified` for real does not exist
    // until Task 10, so the status is set directly here; Task 10's
    // `bad_result_json_lands_the_task_in_unverified_...` covers the realistic path.
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);

    let first = f.dispatch(&task_id, "claude-b", "ok", &[]);
    let first_wt = first["worktree"].as_str().unwrap().to_string();
    assert_eq!(first["worktree_reused"].as_bool(), Some(false));
    assert_eq!(f.wait_for(&task_id, "120s").0, 0);

    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE tasks SET status='unverified' WHERE task_id = ?1",
            [&task_id],
        )
        .unwrap();
    }

    // the same command that handles a reroute handles this
    let second = f.dispatch(&task_id, "claude-b", "ok", &[]);
    assert_eq!(second["attempt"].as_i64(), Some(1), "same attempt");
    assert_eq!(second["worktree"].as_str().unwrap(), first_wt);
    assert_eq!(second["worktree_reused"].as_bool(), Some(true));

    // `unverified` is wait-terminal, so dispatch must have moved the task off it
    // synchronously or the wait below would return immediately with the old status
    let after_dispatch: String = {
        let conn = f.state_conn();
        conn.query_row(
            "SELECT status FROM tasks WHERE task_id = ?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_ne!(after_dispatch, "unverified");
    assert!(
        after_dispatch == "running" || after_dispatch == "in_review",
        "unexpected status after re-dispatch: {after_dispatch}"
    );

    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");

    let conn = f.state_conn();
    let runs: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM runs WHERE task_id = ?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(runs, 2, "a re-dispatch is a new run, not a resume");
}

#[test]
fn dispatch_does_not_clear_a_rate_limited_profile() {
    // `probe_auth` is the read-only half of `profile check`. Being rate limited
    // does not make `auth status` report loggedIn:false, so if dispatch did the
    // bookkeeping every dispatch would silently un-park a parked profile.
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    {
        let g = f.global_conn();
        g.execute(
            "UPDATE profiles SET status='rate_limited' WHERE profile_id='claude-b'",
            [],
        )
        .unwrap();
    }

    f.dispatch(&task_id, "claude-b", "ok", &[]);
    assert_eq!(f.wait_for(&task_id, "120s").0, 0);

    let g = f.global_conn();
    let status: String = g
        .query_row(
            "SELECT status FROM profiles WHERE profile_id='claude-b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "rate_limited", "dispatch must not clear the flag");
    let resets: i64 = g
        .query_row(
            "SELECT COUNT(*) FROM quota_events WHERE kind='window_reset'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(resets, 0, "dispatch must not write a window_reset event");
}

#[test]
fn dispatch_warns_about_a_rate_limited_profile_but_still_runs() {
    // Open question 21's ruling: warn, do not refuse. The operator frequently knows
    // the window reset before the DB does, and Pro accounts get parked often.
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);

    // park claude-b, with a quota event so the warning has a definite date
    {
        let g = f.global_conn();
        g.execute(
            "UPDATE profiles SET status='rate_limited' WHERE profile_id='claude-b'",
            [],
        )
        .unwrap();
        g.execute(
            "INSERT INTO quota_events (ts, profile_id, kind, detail)
             VALUES ('2026-09-22T00:00:00Z', 'claude-b', 'rate_limited', '{}')",
            [],
        )
        .unwrap();
    }

    // human mode: the warning goes to stderr, the run still starts
    f.cmd()
        .env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "warning: profile claude-b is marked rate_limited since 2026-09-22T00:00:00Z",
        ))
        .stderr(predicate::str::contains(
            "Run 'thoth profile check claude-b' to clear.",
        ))
        .stdout(predicate::str::contains("dispatched"));
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    // --json mode: the same text arrives inside the document instead
    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    let warnings = d["warnings"].as_array().expect("warnings array");
    assert_eq!(warnings.len(), 1, "{d}");
    assert!(
        warnings[0].as_str().unwrap().contains("marked rate_limited since"),
        "{d}"
    );
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    // a healthy profile produces no warnings at all
    let clean = f.dispatch(&task_id, "claude-a", "ok", &[]);
    assert_eq!(clean["warnings"].as_array().map(|a| a.len()), Some(0), "{clean}");
    // M6 (fix round 1): drain this last dispatch too, so its runner is not
    // still holding the temp `THOTH_HOME`/worktree open when the test's
    // Fixture (and its TempDirs) drop at the end of the function.
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
}

#[test]
fn events_can_be_filtered_to_one_task() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let (t1, _) = f.create_task(common::Fixture::SPEC);
    let (t2, _) = f.create_task(common::Fixture::SPEC);

    let out = f
        .cmd()
        .args(["events", "--since", "0", "--task", &t1, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let arr = v["events"].as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["task_id"].as_str(), Some(t1.as_str()));
    assert_ne!(arr[0]["task_id"].as_str(), Some(t2.as_str()));
}

/// final-review Important 3: the run row must remember the harness binary it
/// was launched with, so `cancel` no longer depends on the cancelling shell's
/// environment. The fixture always dispatches with the fake harness, so that
/// is what must land in the column.
#[test]
fn dispatch_records_the_harness_binary_on_the_run_row() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");

    let bin: Option<String> = f
        .state_conn()
        .query_row(
            "SELECT harness_bin FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    let bin = bin.expect("harness_bin must be recorded at dispatch time");
    assert!(
        bin.to_lowercase().contains("fake-claude"),
        "expected the fake harness path, got {bin}"
    );
}

#[test]
fn dispatch_refuses_a_profile_whose_stored_harness_is_not_supported() {
    // Every registered harness is supported; this value is not.
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("other-a");
    f.global_conn()
        .execute("UPDATE profiles SET harness='gemini' WHERE profile_id='other-a'", [])
        .unwrap();
    let (task_id, _) = f.create_task(common::Fixture::SPEC);

    f.cmd()
        .args(["dispatch", &task_id, "--profile", "other-a"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("gemini"))
        .stderr(predicate::str::contains("only claude-code, codex"))
        .stderr(predicate::str::contains("are supported"));

    let runs: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(runs, 0);
    assert_eq!(f.task_status(&task_id), "queued");
}
