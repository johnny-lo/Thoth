mod common;

use predicates::prelude::*;
use std::time::{Duration, Instant};

// Normalize image name: trim whitespace, lowercase, then remove one trailing
// .exe (case-insensitive). Used by pid_alive to verify process identity across
// Windows' case-insensitive filesystems and variable .exe capitalization.
fn normalize_image_stem(s: &str) -> String {
    let trimmed = s.trim().to_lowercase();
    if trimmed.ends_with(".exe") {
        trimmed[..trimmed.len() - 4].to_string()
    } else {
        trimmed
    }
}

#[test]
fn normalize_image_stem_handles_mixed_case_exe_extension() {
    // AC1: lowercase, trim whitespace, and strip trailing .exe (case-insensitive)
    assert_eq!(normalize_image_stem("Thoth.Exe"), "thoth");
    assert_eq!(normalize_image_stem("THOTH.EXE"), "thoth");
    assert_eq!(normalize_image_stem("thoth"), "thoth");

    // AC2: do not strip .exe from middle of name
    assert_ne!(normalize_image_stem("thoth-x.exe"), normalize_image_stem("thoth"));
    assert_eq!(normalize_image_stem("thoth-x.exe"), "thoth-x");
}

fn wait_until_running(f: &common::Fixture, run_id: &str) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        {
            let conn = f.state_conn();
            let row: Option<(String, Option<i64>)> = conn
                .query_row(
                    "SELECT status, worker_pid FROM runs WHERE run_id = ?1",
                    [run_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            if let Some((status, Some(pid))) = row {
                if status == "running" {
                    return pid;
                }
            }
        }
        assert!(Instant::now() < deadline, "run {run_id} never reached running");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Whether `pid` is listed **and** is still running `image`.
///
/// Whole-branch review Minor 2: this used to match the pid number alone, and
/// the assertions that use it run tens of seconds after that pid was force
/// killed, in a test that spawns dozens of processes. Windows recycles pids
/// aggressively, so "the pid is listed" was not evidence the killed process
/// was alive - it was evidence *something* holds that number now, and the
/// test failed on an unrelated process getting it. Matching the image name
/// too (stem, case-insensitive, `.exe` dropped) is what `cancel`'s own
/// escalation already does with `process::image_name` for the same reason.
fn pid_alive(pid: u32, image: &str) -> bool {
    // Linux support: the tasklist parsing moved to `common::pid_alive`, which
    // has a `/proc` branch as well; the image-match rule above is unchanged.
    common::pid_alive(pid, image)
}

/// The image `dispatch`/`resume` launch the detached runner as: this test
/// binary's own `thoth`, via `std::env::current_exe`.
const RUNNER_IMAGE: &str = "thoth";
/// The image every worker in these tests is, via `THOTH_CLAUDE_BIN`.
const WORKER_IMAGE: &str = "fake-claude";

/// Sets `runs.heartbeat_at` directly to `seconds_ago` in the past, so a test
/// can put a run past `STALE_AFTER_SECONDS` (15s) without actually waiting
/// that long for a wall clock to catch up.
fn age_heartbeat(f: &common::Fixture, run_id: &str, seconds_ago: i64) {
    let then = (time::OffsetDateTime::now_utc() - time::Duration::seconds(seconds_ago))
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();
    let conn = f.state_conn();
    conn.execute(
        "UPDATE runs SET heartbeat_at = ?1 WHERE run_id = ?2",
        rusqlite::params![then, run_id],
    )
    .unwrap();
}

#[test]
fn cancel_stops_a_slow_run_and_the_child_tree_dies() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "slow", &["--timeout", "10m"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let worker_pid = wait_until_running(&f, &run_id) as u32;
    assert!(pid_alive(worker_pid, WORKER_IMAGE), "worker should be alive before cancel");

    let out = f
        .cmd()
        .args(["cancel", &run_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["status"].as_str(), Some("cancelled"), "{v}");
    // Fix round 2 minor: this is the non-escalated (layer 1) path - the
    // runner itself honoured `cancel_requested` - so `escalated` must be
    // explicitly false, not just absent from a loose substring check.
    assert_eq!(v["escalated"].as_bool(), Some(false), "{v}");

    let (code, status) = f.wait_for(&task_id, "60s");
    assert_eq!(code, 0);
    assert_eq!(status, "cancelled");

    let conn = f.state_conn();
    let (rstatus, reason): (String, Option<String>) = conn
        .query_row(
            "SELECT status, interrupted_reason FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rstatus, "cancelled");
    assert_eq!(reason.as_deref(), Some("user"));

    let mut tries = 0;
    while pid_alive(worker_pid, WORKER_IMAGE) && tries < 60 {
        std::thread::sleep(Duration::from_millis(250));
        tries += 1;
    }
    assert!(!pid_alive(worker_pid, WORKER_IMAGE), "worker {worker_pid} survived cancel");
}

#[test]
fn cancel_escalates_when_the_runner_is_dead_and_still_settles_the_row() {
    // `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is intentional design (Task 6
    // review), not a bug: a runner that dies takes its still-running worker
    // down with it (the worker's job had only one handle - the runner's), so
    // a crashed runner never leaves a worker burning quota unsupervised. This
    // test pins that guarantee first, then exercises `cancel`'s escalation
    // path against the aftermath: `worker_pid` is already gone by the time
    // `cancel` looks, and the image-name check must recognise that rather
    // than trying to kill an unrelated (possibly since-reused) pid. The row
    // still has to settle `cancelled` - that's the guarantee that matters
    // when nothing is left to kill, exactly as when a kill fails, `cancel`
    // must not just return an error and leave the run stuck `running`.
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    // Linux (review finding 4): the worker also starts a grandchild that
    // `setsid`s out of its session and process group (fake-claude's
    // FAKE_SPAWN_CHILD), so this pins that the runner's death takes the
    // *whole* worker tree, not just its root. Windows keeps the root-only
    // form: its job covers the grandchild as well, but that assertion has not
    // been run on a Windows machine yet.
    #[cfg(not(windows))]
    let d = {
        let mut c = f.cmd();
        c.env("FAKE_SCENARIO", "slow")
            .env("FAKE_TASK_ID", &task_id)
            .env("FAKE_SPAWN_CHILD", "1")
            .args(["dispatch", &task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"])
            .args(["--json", "--timeout", "10m"]);
        let out = c.assert().success().get_output().stdout.clone();
        serde_json::from_str::<serde_json::Value>(String::from_utf8(out).unwrap().trim()).unwrap()
    };
    #[cfg(windows)]
    let d = f.dispatch(&task_id, "claude-b", "slow", &["--timeout", "10m"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let worker_pid = wait_until_running(&f, &run_id) as u32;
    #[cfg(not(windows))]
    let grandchild: u32 = {
        let stream = std::path::PathBuf::from(d["run_dir"].as_str().unwrap()).join("stream.jsonl");
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let found = std::fs::read_to_string(&stream)
                .unwrap_or_default()
                .lines()
                .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
                .find(|v| v["subtype"] == "fake_child")
                .and_then(|v| v["pid"].as_u64());
            if let Some(p) = found {
                break p as u32;
            }
            assert!(Instant::now() < deadline, "the worker never reported its fake_child");
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    #[cfg(not(windows))]
    assert!(
        common::pid_alive(grandchild, "sleep"),
        "the escaped grandchild should be running before the runner dies"
    );

    let runner_pid: u32 = {
        let conn = f.state_conn();
        let p: i64 = conn
            .query_row(
                "SELECT runner_pid FROM runs WHERE run_id = ?1",
                [&run_id],
                |r| r.get(0),
            )
            .unwrap();
        p as u32
    };

    // Kill ONLY the runner: no `/T`, no cgroup. This is the scenario
    // `cancel`'s escalation exists for - nobody is left alive to honour
    // `cancel_requested`.
    common::kill_pid_only(runner_pid);

    // Pin the guarantee under test: within 5 s the worker must also be gone,
    // because its job's only handle was the runner's (Windows) / because the
    // runner's systemd unit kills its whole cgroup once its main process is
    // gone (Linux).
    let mut tries = 0;
    while pid_alive(worker_pid, WORKER_IMAGE) && tries < 20 {
        std::thread::sleep(Duration::from_millis(250));
        tries += 1;
    }
    assert!(
        !pid_alive(worker_pid, WORKER_IMAGE),
        "a dead runner must take its still-running worker down with it \
         (JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE)"
    );
    #[cfg(not(windows))]
    {
        let mut tries = 0;
        while common::pid_alive(grandchild, "sleep") && tries < 20 {
            std::thread::sleep(Duration::from_millis(250));
            tries += 1;
        }
        assert!(
            !common::pid_alive(grandchild, "sleep"),
            "a dead runner must take the worker's escaped grandchild down too"
        );
    }

    // Age the heartbeat directly via SQL rather than sleeping in real time
    // for STALE_AFTER_SECONDS (15s): `cancel` still polls for
    // CANCEL_GRACE_SECONDS (10s) before checking staleness, but the
    // staleness check itself no longer has to race a wall clock.
    age_heartbeat(&f, &run_id, 30);

    let out = f
        .cmd()
        .args(["cancel", &run_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value =
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    assert_eq!(v["escalated"].as_bool(), Some(true), "{v}");

    // I3: the worker pid can be recycled by the OS between the moment it
    // died (above) and the moment `cancel`'s image-name check inspects it.
    // `confirm_then_kill` now treats a pid whose image no longer matches the
    // expected harness stem as a `kill_error` ("pid now runs ...  - not
    // killing") rather than silently treating it as "already gone" - that is
    // the whole point of the image-name check, and it makes this test's
    // outcome timing-dependent. Accept either honest outcome: no reuse
    // collision (`cancelled`, no kill errors), or a detected reuse (`failed`,
    // every kill error explicitly about the image mismatch). Either way the
    // escalation must have happened and the worker must be dead - a false
    // "cancelled" over a still-live worker is what the check exists to
    // prevent.
    let kill_errors: Vec<String> = v["kill_errors"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e.as_str().unwrap().to_string())
        .collect();
    let status = v["status"].as_str().unwrap();
    match status {
        "cancelled" => assert!(
            kill_errors.is_empty(),
            "a clean cancellation must not carry kill errors: {v}"
        ),
        "failed" => assert!(
            !kill_errors.is_empty()
                && kill_errors.iter().all(|e| e.contains("not killing")),
            "a failed escalation here must only be due to a pid-reuse image mismatch: {v}"
        ),
        other => panic!("unexpected status {other}: {v}"),
    }
    assert!(!pid_alive(worker_pid, WORKER_IMAGE), "the worker must be gone either way: {v}");

    // the row is settled, not left half-written
    let conn = f.state_conn();
    let (rstatus, err): (String, Option<String>) = conn
        .query_row(
            "SELECT status, error_text FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rstatus, status);
    if status == "cancelled" {
        assert!(err.unwrap().contains("heartbeating"));
    }
    let tstatus: String = conn
        .query_row(
            "SELECT status FROM tasks WHERE task_id = ?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tstatus, status);

    let escalation_detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type='status_change'
               AND detail LIKE '%\"escalated\":true%'",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    // Fix round 2 (I1): `confirm_then_kill`'s benign "pid is simply gone"
    // outcome is worded "pid gone" (as opposed to the image-mismatch "not
    // killing" outcome). "pid gone" is the expected wording *unless* the OS
    // recycled the pid before `cancel` got to look (I3), in which case the
    // "not killing" image-mismatch wording is the honest - and equally
    // acceptable - outcome. See the `status` match above for why both are
    // allowed.
    if status == "cancelled" {
        assert!(
            escalation_detail.contains("pid gone"),
            "the escalation event must say the worker's pid was gone: {escalation_detail}"
        );
    } else {
        assert!(
            escalation_detail.contains("not killing"),
            "a failed escalation here must be explained by the pid-reuse image mismatch: \
             {escalation_detail}"
        );
    }

    // Whole-branch review Minor 2: the duplicate `!pid_alive(worker_pid)`
    // that used to sit here is gone - the identical assertion above, right
    // after the escalation, already proves it and nothing between the two
    // could resurrect the worker.
    assert!(
        !pid_alive(runner_pid, RUNNER_IMAGE),
        "runner must still be gone"
    );
}

#[test]
fn cancel_refuses_a_finished_run() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").0, 0);

    f.cmd()
        .args(["cancel", &run_id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("nothing to cancel"));
}

#[test]
fn quota_interruption_then_resume_reaches_in_review() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "quota", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");

    // the operator decides the window has reset
    f.cmd()
        .args(["profile", "check", "claude-b"])
        .assert()
        .success();

    // resume: the fake only succeeds when it is given --resume
    f.cmd()
        .env("FAKE_SCENARIO", "resume")
        .env("FAKE_TASK_ID", &task_id)
        .args(["resume", &run_id, "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"));

    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");

    let conn = f.state_conn();
    // resume keeps the same run_id and the same attempt
    let (rstatus, attempt): (String, i64) = conn
        .query_row(
            "SELECT status, attempt FROM runs WHERE run_id = ?1",
            [&run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(rstatus, "completed");
    assert_eq!(attempt, 1);
    let total_runs: i64 = conn
        .query_row("SELECT COUNT(*) FROM runs WHERE task_id = ?1", [&task_id], |r| r.get(0))
        .unwrap();
    assert_eq!(total_runs, 1, "resume must not create a second run");

    // the claim says "resumed", proving the fake saw --resume
    let text: String = conn
        .query_row("SELECT text FROM claims WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert!(!text.is_empty());
    let ev: String = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type='result_ingested'",
            [&run_id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(ev.contains("resumed"), "summary should say resumed: {ev}");

    // the prompt sent on resume is the resume prompt, not the original spec
    let prompt = std::fs::read_to_string(
        f.home_path()
            .join("repos")
            .join(f.repo_id())
            .join("runs")
            .join(&run_id)
            .join("prompt.txt"),
    )
    .unwrap();
    assert!(prompt.contains("previous session was interrupted"), "{prompt}");
}

#[test]
fn resume_refuses_a_run_that_is_not_interrupted() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "ok", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").0, 0);

    f.cmd()
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("resume needs an interrupted run"));
}

#[test]
fn reroute_after_a_crash_opens_a_second_run_and_reuses_the_worktree() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);

    let first = f.dispatch(&task_id, "claude-b", "crash", &[]);
    let first_run = first["run_id"].as_str().unwrap().to_string();
    let first_wt = first["worktree"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");

    let second = f.dispatch(&task_id, "claude-a", "ok", &[]);
    let second_run = second["run_id"].as_str().unwrap().to_string();
    assert_ne!(second_run, first_run, "reroute must create a new run");
    assert_eq!(second["attempt"].as_i64(), Some(1), "reroute stays on the same attempt");
    assert_eq!(second["worktree"].as_str().unwrap(), first_wt);
    assert_eq!(second["worktree_reused"].as_bool(), Some(true));

    // dispatch has already moved the task off `interrupted`, so the wait below is
    // not a race between this process and the runner starting up. Reading
    // "interrupted" here would mean the transition was left to the runner.
    let after_dispatch: String = {
        let conn = f.state_conn();
        conn.query_row(
            "SELECT status FROM tasks WHERE task_id = ?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_ne!(
        after_dispatch, "interrupted",
        "dispatch must leave the task in a non-wait-terminal status"
    );
    assert!(
        after_dispatch == "running" || after_dispatch == "in_review",
        "unexpected status after reroute dispatch: {after_dispatch}"
    );

    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");

    let conn = f.state_conn();
    let accounts: Vec<String> = conn
        .prepare("SELECT account FROM runs WHERE task_id = ?1 ORDER BY run_id")
        .unwrap()
        .query_map([&task_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    assert_eq!(accounts.len(), 2);
    assert!(accounts.contains(&"claude-a".to_string()));
    assert!(accounts.contains(&"claude-b".to_string()));

    let kinds: Vec<String> = conn
        .prepare("SELECT type FROM events WHERE task_id = ?1 ORDER BY seq")
        .unwrap()
        .query_map([&task_id], |r| r.get(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<String>>>()
        .unwrap();
    assert!(kinds.contains(&"worktree_created".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"worktree_reused".to_string()), "{kinds:?}");
}

#[test]
fn submit_ingests_a_good_file_and_unverifies_a_bad_one() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "crash", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");

    let bad = f.home_path().join("bad-result.json");
    std::fs::write(&bad, r#"{"status":"completed","summary":"no task id here"}"#).unwrap();
    f.cmd()
        .args(["submit", "-f"])
        .arg(&bad)
        .args(["--run", &run_id, "--json"])
        .assert()
        .code(1)
        .stdout(predicate::str::contains("\"task_status\":\"unverified\""));

    let good = f.home_path().join("good-result.json");
    std::fs::write(
        &good,
        format!(
            r#"{{"task_id":"{task_id}","status":"completed","summary":"fixed by hand",
                 "claims":[{{"kind":"completion","text":"operator verified"}}]}}"#
        ),
    )
    .unwrap();
    f.cmd()
        .args(["submit", "-f"])
        .arg(&good)
        .args(["--run", &run_id, "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"task_status\":\"in_review\""));

    let conn = f.state_conn();
    let claims: i64 = conn
        .query_row("SELECT COUNT(*) FROM claims WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(claims, 1);
}

#[test]
fn submit_refuses_a_run_that_is_still_live() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "slow", &["--timeout", "10m"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    wait_until_running(&f, &run_id);

    let (tstatus_before, claims_before): (String, i64) = {
        let conn = f.state_conn();
        let s: String = conn
            .query_row(
                "SELECT status FROM tasks WHERE task_id = ?1",
                [&task_id],
                |r| r.get(0),
            )
            .unwrap();
        let c: i64 = conn
            .query_row("SELECT COUNT(*) FROM claims WHERE run_id = ?1", [&run_id], |r| r.get(0))
            .unwrap();
        (s, c)
    };
    assert_eq!(tstatus_before, "running", "the task must still be live for this test to mean anything");

    let mid_flight = f.home_path().join("mid-flight-result.json");
    std::fs::write(
        &mid_flight,
        format!(r#"{{"task_id":"{task_id}","status":"completed","summary":"premature","claims":[]}}"#),
    )
    .unwrap();

    f.cmd()
        .args(["submit", "-f"])
        .arg(&mid_flight)
        .args(["--run", &run_id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("submit needs a finished run"));

    let conn = f.state_conn();
    let tstatus_after: String = conn
        .query_row(
            "SELECT status FROM tasks WHERE task_id = ?1",
            [&task_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tstatus_after, tstatus_before, "a refused submit must not change task status");
    let claims_after: i64 = conn
        .query_row("SELECT COUNT(*) FROM claims WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(claims_after, claims_before, "a refused submit must not write any claims");

    // Clean up the still-running slow job so this test does not leak a process.
    f.cmd().args(["cancel", &run_id]).assert().success();
}
