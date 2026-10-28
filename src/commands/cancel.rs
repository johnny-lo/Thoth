use crate::config::Paths;
use crate::db::state::{self, NewEvent, RunFinish};
use crate::ids;
use crate::model::{event_type, InterruptedReason, RunStatus, TaskStatus};
use crate::output;
use crate::process;
use crate::runner;
use anyhow::{bail, Result};
use std::path::Path;
use std::time::{Duration, Instant};

/// How long `cancel` waits for a live runner to honour `cancel_requested`
/// (the runner polls once a second) before escalating to a direct kill.
pub const CANCEL_GRACE_SECONDS: u64 = 10;

pub fn cancellable(run_status: &str) -> bool {
    matches!(run_status, "queued" | "running")
}

/// Normalises an image name or configured-binary string for comparison:
/// takes just the file name (drops any directory), lowercases it, then
/// strips a trailing `.exe`/`.cmd`. This has to tolerate both sides of the
/// comparison being spelled very differently for the same real binary:
/// `tasklist` always reports a bare name with its extension (`claude.exe`),
/// while `config::claude_bin()` can be a bare command name with no extension
/// at all (`claude`, e.g. the acceptance procedure's `THOTH_CLAUDE_BIN`)
/// or an absolute path to the test double (`CARGO_BIN_EXE_fake-claude`,
/// ending `...\fake-claude.exe`).
///
/// Fix round 2 (I1): comparing the two sides directly (as the first cut of
/// this code did) is always false for the real binary - `claude` never
/// equals `claude.exe` - which made the "is this really our process"
/// guard inert in production and let escalation report `cancelled` over a
/// possibly still-live worker.
fn image_stem(s: &str) -> String {
    let name = Path::new(s)
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| s.to_string())
        .to_lowercase();
    name.strip_suffix(".exe")
        .or_else(|| name.strip_suffix(".cmd"))
        .unwrap_or(&name)
        .to_string()
}

/// Confirms `pid` is still the process this run actually started - matched
/// by executable stem (see `image_stem`), not raw pid - before killing it.
/// Both the worker and the runner go through this same policy so the two are
/// provably identical (fix round 2, I1: they used to be two separately
/// hand-written copies).
///
/// Three distinct outcomes, not just kill-or-don't, because they mean
/// different things to an operator reading `notes`/`kill_errors`:
/// - `Ok(None)`: `pid` is simply gone - benign, and often *expected* here:
///   the fix round 1 controller ruling is that a dead runner takes its
///   worker down with it via the job, so the counterpart of whichever pid
///   died first is frequently already gone by the time this runs.
/// - `Ok(Some(other))` that does not match `expected_stem`: ambiguous, not
///   benign. It could be Windows having handed the pid to a genuinely
///   unrelated process; but `expected_stem` for the worker comes from the
///   *cancelling* shell's `THOTH_CLAUDE_BIN` (`crate::config::claude_bin()`
///   in `run` below), not from anything recorded on the run row, so a
///   mismatch can equally mean this really is the live worker, just launched
///   under a different `THOTH_CLAUDE_BIN` than the one active in this
///   shell (Important 3, whole-branch review). Reporting that as a `notes`
///   entry and still settling `cancelled` risked declaring a still-running,
///   still-quota-burning worker dead. Treated as a kill failure instead:
///   never killed, and the run settles `failed` (with the mismatch spelled
///   out in `error_text`) rather than a possibly-false `cancelled`.
/// - a match: this really is our process; `kill_tree` it.
/// - `Err`: could not even ask `tasklist`. Also a kill failure, so the run is
///   honestly settled `failed`, not a hopeful `cancelled` over a pid nobody
///   actually confirmed dead.
fn confirm_then_kill(
    ctrl: &dyn process::ProcessController,
    pid: u32,
    expected_stem: &str,
    label: &str,
    killed: &mut Vec<u32>,
    kill_errors: &mut Vec<String>,
    notes: &mut Vec<String>,
) {
    match process::image_name(pid) {
        Ok(None) => {
            notes.push(format!("{label} pid {pid}: pid gone"));
        }
        Ok(Some(seen)) => {
            if image_stem(&seen) == expected_stem {
                match ctrl.kill_tree(pid) {
                    Ok(()) => killed.push(pid),
                    Err(e) => kill_errors.push(format!("pid {pid}: {e}")),
                }
            } else {
                kill_errors.push(format!(
                    "{label} pid {pid}: pid now runs {seen}, expected {expected_stem} - not \
                     killing (the OS may have reused the pid, or this run predates \
                     runs.harness_bin and the expected image came from this shell's \
                     THOTH_CLAUDE_BIN; see docs/phase1/usage.md's Cancelling section)"
                ));
            }
        }
        Err(e) => {
            kill_errors.push(format!(
                "could not confirm {label} pid {pid}'s image before killing it: {e}"
            ));
        }
    }
}

/// The image the runner recorded for this run's current worker, if any: a
/// codex run's `harness_init` (written when `thread.started` arrives, after
/// the npm launcher has turned into `node`) newer than the latest
/// `run_started`, else that `run_started`'s own `worker_image` (Claude's, read
/// right after spawn; null for codex).
fn recorded_worker_image(conn: &rusqlite::Connection, task_id: &str, run_id: &str) -> Option<String> {
    let image = |detail: &str| {
        serde_json::from_str::<serde_json::Value>(detail)
            .ok()
            .and_then(|d| d["worker_image"].as_str().map(str::to_string))
    };
    for e in state::events_since(conn, 0, Some(task_id)).ok()?.into_iter().rev() {
        if e.run_id.as_deref() != Some(run_id) {
            continue;
        }
        if e.event_type == event_type::HARNESS_INIT {
            if let Some(i) = image(&e.detail) {
                return Some(i);
            }
        } else if e.event_type == event_type::RUN_STARTED {
            return image(&e.detail);
        }
    }
    None
}

/// The stem `confirm_then_kill` expects the worker pid to run.
fn expected_worker_stem(worker_image: Option<&str>, harness_bin: &str) -> String {
    match worker_image.filter(|i| !i.trim().is_empty()) {
        Some(image) => image_stem(image),
        None => image_stem(harness_bin),
    }
}

pub fn run(paths: &Paths, cwd: &Path, needle: &str, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let run_id = ids::resolve_prefix(&state::list_run_ids(&conn)?, needle)?;
    let run = state::get_run(&conn, &run_id)?
        .ok_or_else(|| anyhow::anyhow!("run {run_id} disappeared"))?;
    if !cancellable(&run.status) {
        bail!("run {run_id} is already '{}'; nothing to cancel", run.status);
    }

    // Layer 1: ask the runner to stop. Its own poll loop checks
    // `cancel_requested` once a second (`runner::POLL_EVERY`).
    state::request_cancel(&conn, &run_id)?;
    state::append_event(
        &conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run_id.clone()),
            event_type: event_type::CANCEL_REQUESTED.to_string(),
            detail: serde_json::json!({
                "runner_pid": run.runner_pid,
                "worker_pid": run.worker_pid,
            }),
        },
    )?;

    let deadline = Instant::now() + Duration::from_secs(CANCEL_GRACE_SECONDS);
    let mut latest = run.clone();
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(500));
        latest = state::get_run(&conn, &run_id)?
            .ok_or_else(|| anyhow::anyhow!("run {run_id} disappeared"))?;
        if !cancellable(&latest.status) {
            let v = serde_json::json!({
                "ok": true,
                "run_id": run_id,
                "status": latest.status,
                "escalated": false,
            });
            output::emit(
                json,
                &v,
                &format!("run {run_id} is now {} (runner honoured the request)", latest.status),
            );
            return Ok(0);
        }
    }

    // Layer 2: the runner is not answering. If its heartbeat is stale (or the
    // run never even got past `queued` in the grace window - a runner that
    // died before it could start anything) it is dead, so `cancel` kills the
    // worker/runner itself and settles the row.
    let now = ids::now_rfc3339();
    let stale = crate::commands::status::heartbeat_is_stale(
        &now,
        latest.heartbeat_at.as_deref(),
        &latest.status,
    ) || latest.status == "queued";
    if !stale {
        let v = serde_json::json!({
            "ok": true,
            "run_id": run_id,
            "status": latest.status,
            "escalated": false,
            "note": "cancel_requested is set; the runner is still heartbeating",
        });
        output::emit(
            json,
            &v,
            &format!("cancel requested for {run_id}; runner still alive, status {}", latest.status),
        );
        return Ok(0);
    }

    let ctrl = process::controller();
    // The image stem of the currently configured harness binary (e.g.
    // `claude` for both `claude.exe` and a bare `claude`) and of this very
    // thoth build, so a bare-pid kill of either `worker_pid` or
    // `runner_pid` never lands on an unrelated process Windows has since
    // reused that pid for (review finding 4 / controller pointer 2).
    //
    // Fix round 1 controller ruling: `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is
    // intentional design, not a bug - a runner that dies takes its still-live
    // worker down with it (its job's only handle was the runner's), so by the
    // time this code runs, one of the two pids is very often already gone.
    // That is the expected, safe outcome, not a failure to report: this
    // escalation exists for the cases it *doesn't* cover (the runner hung
    // rather than died, or the worker somehow survived), so both pids go
    // through the identical `confirm_then_kill` policy before either is
    // killed.
    // final-review Important 3 (docs/phase1/process/final-review.md:481-482):
    // the expected worker image now comes from the run row, which records what
    // `dispatch`/`resume` actually launched. Only a run written before the v2
    // migration has no value there, and for those the Phase 1 behaviour - this
    // shell's `THOTH_CLAUDE_BIN` - is still the best guess available.
    //
    // The codex worker is the npm Node launcher, whose image is `node`, not
    // the stem of `codex`; so the image the runner saw right after spawning
    // the worker comes first, and `harness_bin` is the fallback for runs that
    // did not record one.
    let harness_bin = latest
        .harness_bin
        .clone()
        .unwrap_or_else(crate::config::claude_bin);
    let harness_stem = expected_worker_stem(
        recorded_worker_image(&conn, &latest.task_id, &run_id).as_deref(),
        &harness_bin,
    );
    let thoth_stem = std::env::current_exe()
        .map(|p| image_stem(&p.to_string_lossy()))
        .unwrap_or_else(|_| "thoth".to_string());

    let mut killed: Vec<u32> = Vec::new();
    let mut kill_errors: Vec<String> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    if let Some(worker_pid) = latest.worker_pid {
        confirm_then_kill(
            ctrl.as_ref(),
            worker_pid as u32,
            &harness_stem,
            "worker",
            &mut killed,
            &mut kill_errors,
            &mut notes,
        );
    }

    if let Some(runner_pid) = latest.runner_pid {
        confirm_then_kill(
            ctrl.as_ref(),
            runner_pid as u32,
            &thoth_stem,
            "runner",
            &mut killed,
            &mut kill_errors,
            &mut notes,
        );
    }

    // A pid we could not confirm dead means we cannot honestly call this
    // `cancelled`: something may still be writing in the worktree. Record it
    // as `failed` with the kill error, so `status` and `events` show the
    // truth instead of a clean cancellation that did not happen. Only a pid
    // that is confirmed simply gone (`notes` above) is benign; a pid whose
    // image did not match what we expected is now also a kill error
    // (Important 3), because that mismatch may just mean this shell's
    // `THOTH_CLAUDE_BIN` differs from the one active at dispatch time,
    // not that the process is actually dead or unrelated.
    let all_dead = kill_errors.is_empty();
    let ended_at = ids::now_rfc3339();
    let (run_status, task_status) = if all_dead {
        (RunStatus::Cancelled, TaskStatus::Cancelled)
    } else {
        (RunStatus::Failed, TaskStatus::Failed)
    };
    let mut error_text = if all_dead {
        "cancelled by thoth after the runner stopped heartbeating".to_string()
    } else {
        format!("cancel could not kill every process: {}", kill_errors.join("; "))
    };
    if !notes.is_empty() {
        error_text = format!("{error_text}; {}", notes.join("; "));
    }

    let finish = RunFinish {
        status: run_status,
        // #55 (final review): only a genuine `cancelled` outcome is really
        // "interrupted by the user" - when a kill did not fully take and the
        // run lands `failed` instead, nothing reads `interrupted_reason`
        // there, and leaving it `user` on a `failed` row is misleading in
        // `status --json`.
        interrupted_reason: if all_dead { Some(InterruptedReason::User) } else { None },
        exit_code: None,
        candidate_commit: crate::worktree::rev_parse_head(Path::new(&latest.worktree)).ok(),
        error_text: Some(error_text.clone()),
        ended_at: ended_at.clone(),
    };
    let status_change_detail = serde_json::json!({
        "entity": "run",
        "id": run_id,
        "to": run_status.as_str(),
        "escalated": true,
        "killed_pids": killed,
        "kill_errors": kill_errors,
        "notes": notes,
    });
    // Reuse the runner's own atomic settle (Task 8/11 controller ruling)
    // instead of re-implementing finish_run + status_change + set_task_status
    // separately: the runs and tasks rows must never be able to disagree
    // after a partial write here either.
    //
    // Important 1 (whole-branch review): `settle_terminal` is now conditional
    // on the run still being non-terminal. `false` here means the runner's
    // own settle tail landed first, between this function's last read of
    // `latest` and this write - the run already carries a real, presumably
    // correct outcome, and this escalation must not overwrite it with its
    // own guess.
    //
    // Phase 2: cancelling a *review* says nothing about the task under review.
    // Leave it `in_review` so the review can be started again; only an
    // implementer run's cancellation is the task's cancellation. Cancelling a
    // spec review (SR-2) is not cancelling the task either.
    let task_status_to_write = if latest.role == crate::model::role::REVIEWER
        || latest.role == crate::model::role::SPEC_REVIEWER
    {
        None
    } else {
        // Unconditional, exactly as before: this run *is* the task's current
        // work, and cancelling it is the task's cancellation.
        Some(runner::TaskStatusWrite::unconditional(task_status))
    };
    let applied = runner::settle_terminal(
        &conn,
        &latest,
        &finish,
        task_status_to_write,
        None,
        status_change_detail,
    )?;
    if !applied {
        let now_status = state::get_run(&conn, &run_id)?
            .map(|r| r.status)
            .unwrap_or_else(|| "unknown".to_string());
        // Important (final review): this branch used to report `ok: true`
        // unconditionally and drop `kill_errors`/`killed_pids` on the floor.
        // The run row landed on its own outcome first, but the pids this
        // invocation tried to confirm dead are a separate fact - if one of
        // them could not be confirmed, the operator still needs to know a
        // process may be alive and unsupervised, even though there is
        // nothing left here to write to the row.
        let confirmed = kill_errors.is_empty();
        let v = serde_json::json!({
            "ok": confirmed,
            "run_id": run_id,
            "status": now_status,
            "escalated": true,
            "killed_pids": killed,
            "kill_errors": kill_errors,
            "note": "the run settled on its own just before this escalation's write landed; \
                     nothing was overwritten to the row",
        });
        output::emit(
            json,
            &v,
            &format!(
                "run {run_id} already reached {now_status} before cancel's escalation applied; \
                 left as is"
            ),
        );
        if !confirmed {
            masked_eprintln!(
                "thoth: warning: could not confirm every process dead while cancelling \
                 {run_id} (the row already settled on its own): {}",
                kill_errors.join("; ")
            );
            return Ok(1);
        }
        return Ok(0);
    }

    let v = serde_json::json!({
        "ok": all_dead,
        "run_id": run_id,
        "status": run_status.as_str(),
        "escalated": true,
        "killed_pids": killed,
        "kill_errors": kill_errors,
    });
    output::emit(
        json,
        &v,
        &if all_dead {
            format!("run {run_id} cancelled; killed pids {killed:?}")
        } else {
            format!("run {run_id} marked failed: {error_text}")
        },
    );
    // Non-zero when a process survived: the operator has to go look.
    Ok(if all_dead { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_queued_or_running_runs_can_be_cancelled() {
        assert!(cancellable("queued"));
        assert!(cancellable("running"));
        for done in ["completed", "failed", "cancelled", "interrupted"] {
            assert!(!cancellable(done), "{done} is already over");
        }
        assert_eq!(CANCEL_GRACE_SECONDS, 10);
    }

    /// AC6: the npm `codex` is a Node launcher, so the worker pid's image is
    /// `node`, not the harness binary's stem. The image the runner recorded
    /// at spawn wins; a run without one falls back to `harness_bin`'s stem.
    #[test]
    fn the_expected_worker_image_prefers_the_recorded_one() {
        assert_eq!(expected_worker_stem(Some("node"), "/usr/bin/codex"), "node");
        assert_eq!(expected_worker_stem(Some("fake-codex.exe"), "codex"), "fake-codex");
        assert_eq!(expected_worker_stem(None, "/usr/bin/codex"), "codex");
        assert_eq!(expected_worker_stem(Some(""), "claude"), "claude", "empty is not a record");
    }

    #[test]
    fn worker_image_is_read_from_the_newest_run_started_event_of_the_run() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        let ev = |run: &str, ty: &str, detail: serde_json::Value| {
            state::append_event(
                &conn,
                &NewEvent {
                    task_id: Some("T1".into()),
                    run_id: Some(run.into()),
                    event_type: ty.into(),
                    detail,
                },
            )
            .unwrap();
        };
        assert_eq!(recorded_worker_image(&conn, "T1", "R1"), None);
        // a Phase 1/2 run_started has no worker_image
        ev("R1", event_type::RUN_STARTED, serde_json::json!({"worker_pid": 1}));
        assert_eq!(recorded_worker_image(&conn, "T1", "R1"), None);
        ev("R1", event_type::RUN_STARTED, serde_json::json!({"worker_image": "node"}));
        ev("R2", event_type::RUN_STARTED, serde_json::json!({"worker_image": "claude"}));
        ev("R1", event_type::STATUS_CHANGE, serde_json::json!({"worker_image": "bogus"}));
        assert_eq!(recorded_worker_image(&conn, "T1", "R1").as_deref(), Some("node"));
    }

    /// A codex run records no image at spawn (the npm launcher's pid still
    /// reads `codex` then) and records the settled one on its `harness_init`
    /// event when `thread.started` arrives; that one wins.
    #[test]
    fn a_codex_worker_image_recorded_at_thread_started_wins() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        let ev = |ty: &str, detail: serde_json::Value| {
            state::append_event(
                &conn,
                &NewEvent {
                    task_id: Some("T1".into()),
                    run_id: Some("R1".into()),
                    event_type: ty.into(),
                    detail,
                },
            )
            .unwrap();
        };
        ev(event_type::RUN_STARTED, serde_json::json!({"worker_image": null}));
        assert_eq!(recorded_worker_image(&conn, "T1", "R1"), None, "falls back to harness_bin");
        ev(event_type::HARNESS_INIT, serde_json::json!({"session_id": "t", "worker_image": "node"}));
        assert_eq!(recorded_worker_image(&conn, "T1", "R1").as_deref(), Some("node"));
        // a Claude run's harness_init has no worker_image; run_started's stays
        ev(event_type::RUN_STARTED, serde_json::json!({"worker_image": "claude"}));
        ev(event_type::HARNESS_INIT, serde_json::json!({"session_id": "s"}));
        assert_eq!(recorded_worker_image(&conn, "T1", "R1").as_deref(), Some("claude"));
        // a resumed codex launch: an older launch's image is not this pid's
        ev(event_type::RUN_STARTED, serde_json::json!({"worker_image": null}));
        assert_eq!(recorded_worker_image(&conn, "T1", "R1"), None);
    }

    /// Fix round 2 (I1): the guard comparing `tasklist`'s image name against
    /// the configured binary was always false in production because it
    /// compared raw strings (`claude` vs `claude.exe`) instead of stems.
    #[test]
    fn image_stem_normalises_extension_case_and_directory() {
        assert_eq!(image_stem("claude.exe"), "claude");
        assert_eq!(image_stem("claude"), "claude");
        assert_eq!(image_stem("claude"), image_stem("claude.exe"));
        assert_eq!(image_stem("Claude.EXE"), "claude");
        // A backslash is a directory separator only on Windows; on Linux it is
        // an ordinary file-name character, and `THOTH_CLAUDE_BIN` is a
        // forward-slash path there.
        #[cfg(windows)]
        {
            assert_eq!(image_stem(r"C:\x\claude.exe"), "claude");
            assert_eq!(image_stem(r"C:\x\claude.exe"), image_stem("claude"));
        }
        #[cfg(not(windows))]
        {
            assert_eq!(image_stem("/home/u/.local/bin/claude"), "claude");
            assert_eq!(image_stem("/x/target/debug/fake-claude"), "fake-claude");
        }
        assert_ne!(image_stem("fake-claude.exe"), image_stem("claude.exe"), "must not conflate different binaries");
    }
}
