mod common;

#[test]
#[allow(clippy::type_complexity)] // a one-shot 6-column query_row tuple; a named type would be pure noise here
fn run_worker_on_the_ok_scenario_reaches_completed_and_in_review() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, wt) = f.seed_run("claude-b", common::Fixture::SPEC);

    f.cmd()
        .env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task_id)
        .args(["run-worker", &run_id])
        .assert()
        .success();

    let conn = f.state_conn();
    let (rstatus, exit, cand, sess, itok, otok): (
        String,
        Option<i64>,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<i64>,
    ) = conn
        .query_row(
            "SELECT status, exit_code, candidate_commit, harness_session_id, input_tokens, output_tokens
             FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(rstatus, "completed");
    assert_eq!(exit, Some(0));
    assert!(cand.is_some(), "candidate_commit must be recorded");
    assert_eq!(sess.as_deref(), Some("11111111-2222-3333-4444-555555555555"));
    assert_eq!(itok, Some(34));
    assert_eq!(otok, Some(610));

    let tstatus: String = conn
        .query_row("SELECT status FROM tasks WHERE task_id = ?1", [&task_id], |r| r.get(0))
        .unwrap();
    assert_eq!(tstatus, "in_review");

    let claims: Vec<(String, String)> = conn
        .prepare("SELECT kind, text FROM claims WHERE run_id = ?1")
        .unwrap()
        .query_map([&run_id], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(claims.len(), 1);
    // Task 5: the `ok` fake fires no hooks, so it has no evidence to cite and
    // v6 §2.5 rule 2 would reject an uncited `test_result`; it claims
    // `completion`, which is exempt.
    assert_eq!(claims[0].0, "completion");

    // the event sequence the Supervisor will poll
    let kinds: Vec<String> = conn
        .prepare("SELECT type FROM events WHERE task_id = ?1 ORDER BY seq")
        .unwrap()
        .query_map([&task_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    for expected in [
        "task_created",
        "run_started",
        "rate_limit_warning",
        "result_ingested",
        "status_change",
    ] {
        assert!(kinds.contains(&expected.to_string()), "missing {expected} in {kinds:?}");
    }
    assert_eq!(kinds[0], "task_created");
    assert_eq!(kinds.last().unwrap(), "status_change");

    // the run dir got the three artefacts
    let run_dir = f
        .home_path()
        .join("repos")
        .join(f.repo_id())
        .join("runs")
        .join(&run_id);
    assert!(run_dir.join("stream.jsonl").is_file());
    assert!(run_dir.join("stderr.log").is_file());
    let prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    assert!(prompt.contains("AC1: prints hello"));
    assert!(prompt.contains(&task_id));

    // stream.jsonl is a faithful tee: every line is JSON and includes the result
    let stream = std::fs::read_to_string(run_dir.join("stream.jsonl")).unwrap();
    let mut saw_result = false;
    for line in stream.lines().filter(|l| !l.trim().is_empty()) {
        let v: serde_json::Value = serde_json::from_str(line).expect("tee must stay line-JSON");
        if v["type"] == "result" {
            saw_result = true;
        }
    }
    assert!(saw_result, "stream.jsonl must contain the result line");

    // the worker's result.json is where we expect it, and we did not delete it
    assert!(wt.join(".agent").join("result.json").is_file());
}

/// I2/I3 (fix round 1): a spawn failure (harness binary does not exist) must
/// not leave the row stuck at `queued` - the outer `run_worker` catches the
/// error, settles the run/task `failed`, and logs to `runner-error.log`
/// because a detached runner has no stderr a human will see.
#[test]
fn a_spawn_failure_settles_the_row_failed_and_writes_a_runner_error_log() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _wt) = f.seed_run("claude-b", common::Fixture::SPEC);

    f.cmd()
        .env(
            "THOTH_CLAUDE_BIN",
            "C:/definitely/not/a/real/thoth-test-binary.exe",
        )
        .args(["run-worker", &run_id])
        .assert()
        .failure();

    let conn = f.state_conn();
    let (rstatus, err): (String, Option<String>) = conn
        .query_row(
            "SELECT status, error_text FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rstatus, "failed");
    assert!(
        err.map(|e| !e.is_empty()).unwrap_or(false),
        "error_text must be recorded"
    );

    let tstatus: String = conn
        .query_row("SELECT status FROM tasks WHERE task_id = ?1", [&task_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(tstatus, "failed");

    let run_dir = f
        .home_path()
        .join("repos")
        .join(f.repo_id())
        .join("runs")
        .join(&run_id);
    assert!(
        run_dir.join("runner-error.log").is_file(),
        "runner-error.log must exist"
    );
}

/// M8 (fix round 1): `run-worker` refuses to start against a row that is not
/// `queued`, and makes no writes at all when it refuses.
#[test]
fn run_worker_refuses_a_non_queued_row_without_writing_anything() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _wt) = f.seed_run("claude-b", common::Fixture::SPEC);

    {
        let conn = f.state_conn();
        conn.execute(
            "UPDATE runs SET status = 'running' WHERE run_id = ?1",
            [&run_id],
        )
        .unwrap();
    }

    f.cmd()
        .env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task_id)
        .args(["run-worker", &run_id])
        .assert()
        .failure();

    let conn = f.state_conn();
    let rstatus: String = conn
        .query_row("SELECT status FROM runs WHERE run_id = ?1", [&run_id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(rstatus, "running", "refusal must not touch the row");

    let run_dir = f
        .home_path()
        .join("repos")
        .join(f.repo_id())
        .join("runs")
        .join(&run_id);
    assert!(
        !run_dir.join("stream.jsonl").exists(),
        "refusal must happen before any run artefacts are created"
    );
}

fn run_ok_worker(f: &common::Fixture, extra_env: &[(&str, &str)]) -> (String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _wt) = f.seed_run("claude-b", common::Fixture::SPEC);
    let mut cmd = f.cmd();
    cmd.env("FAKE_SCENARIO", "ok").env("FAKE_TASK_ID", &task_id);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.args(["run-worker", &run_id]).assert().success();
    (task_id, run_id)
}

fn events_of(f: &common::Fixture, run_id: &str, kind: &str) -> Vec<serde_json::Value> {
    f.state_conn()
        .prepare("SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq")
        .unwrap()
        .query_map([run_id, kind], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|d| serde_json::from_str(&d.unwrap()).unwrap())
        .collect()
}

/// Every run records which models it used and how many subagents it spawned,
/// in one `run_usage` event; nothing is flagged when no subagent was spawned.
#[test]
fn a_run_records_one_run_usage_event_and_no_warning_without_subagents() {
    let f = common::fixture();
    let (task_id, run_id) = run_ok_worker(&f, &[]);

    let usage = events_of(&f, &run_id, "run_usage");
    assert_eq!(usage.len(), 1, "exactly one run_usage event: {usage:?}");
    assert_eq!(usage[0]["models"]["fake-model"], 610);
    assert_eq!(usage[0]["models"]["fake-haiku"], 12);
    assert_eq!(usage[0]["subagents_spawned"], 0);
    assert!(events_of(&f, &run_id, "subagents_spawned").is_empty());

    let status: String = f
        .state_conn()
        .query_row("SELECT status FROM tasks WHERE task_id = ?1", [&task_id], |r| r.get(0))
        .unwrap();
    assert_eq!(status, "in_review");
}

/// `subagent_stats.spawned > 0` adds a warning event and changes nothing else:
/// the run and the task settle exactly as they do without it.
#[test]
fn spawned_subagents_add_a_warning_event_and_leave_the_settlement_unchanged() {
    let f = common::fixture();
    let (task_id, run_id) = run_ok_worker(&f, &[("FAKE_SUBAGENTS", "3")]);

    let usage = events_of(&f, &run_id, "run_usage");
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["subagents_spawned"], 3);

    let warn = events_of(&f, &run_id, "subagents_spawned");
    assert_eq!(warn.len(), 1, "one warning: {warn:?}");
    assert_eq!(warn[0]["spawned"], 3);

    let conn = f.state_conn();
    let (rstatus, exit): (String, Option<i64>) = conn
        .query_row("SELECT status, exit_code FROM runs WHERE run_id = ?1", [&run_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((rstatus.as_str(), exit), ("completed", Some(0)));
    let tstatus: String = conn
        .query_row("SELECT status FROM tasks WHERE task_id = ?1", [&task_id], |r| r.get(0))
        .unwrap();
    assert_eq!(tstatus, "in_review");
}

/// AC4: `status TASK --json` adds `models_used` and `subagents_spawned` to each
/// `runs[]` entry, and they are null for a run that has no usage recorded.
#[test]
fn status_json_reports_models_and_subagents_per_run() {
    let f = common::fixture();
    let (task_id, run_id) = run_ok_worker(&f, &[("FAKE_SUBAGENTS", "1")]);
    let out = f
        .cmd()
        .args(["status", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let run = &v["runs"][0];
    assert_eq!(run["run_id"], run_id);
    assert_eq!(run["models_used"]["fake-model"], 610);
    assert_eq!(run["models_used"]["fake-haiku"], 12);
    assert_eq!(run["subagents_spawned"], 1);

    // a run that never reached a result event has neither
    let f2 = common::fixture();
    f2.cmd().arg("init").assert().success();
    f2.add_profile("claude-b");
    let (task2, _run2, _wt) = f2.seed_run("claude-b", common::Fixture::SPEC);
    let out = f2
        .cmd()
        .args(["status", &task2, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert!(v["runs"][0]["models_used"].is_null(), "{v}");
    assert!(v["runs"][0]["subagents_spawned"].is_null(), "{v}");
}
