use crate::config::Paths;
use crate::db::state::{self, RunFinish};
use crate::ids;
use crate::model::{RunStatus, TaskStatus};
use super::attempts::Launched;
use super::candidate::CandidateEvidence;
use super::outcome::{apply_login_check, classify_outcome, needs_login_check};
use super::prepare::RunPrep;
use super::settle::{
    finish_and_settle, log_runner_error, record_model_unavailable, TaskStatusWrite,
    VERDICT_ENTRY_STATUSES,
};
use super::verdict::IngestResult;
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

// The moved statements borrow `&masker`, `&run_dir` and the like as they did when these
// were owned locals; here they are already references, so the `&` is redundant and kept
// so the statements stay word for word the same.
#[allow(clippy::needless_borrow, clippy::needless_borrows_for_generic_args, clippy::too_many_arguments)]
pub(super) fn conclude_run(
    paths: &Paths,
    conn: &Connection,
    repo_id: &str,
    repo_root: &Path,
    run: &state::Run,
    prep: &RunPrep,
    launched: Launched<'_>,
    candidate: CandidateEvidence,
    ingested: IngestResult,
) -> Result<()> {
    let gconn = &prep.gconn;
    let profile = &prep.profile;
    let run_dir = &prep.run_dir;
    let is_review = prep.is_review;
    let is_spec_review = prep.is_spec_review;
    let Launched {
        tally,
        exit_code,
        kill,
        kill_error,
        drain_timed_out,
        agy_print_timeout,
        watch,
        evidence_from_launches,
        retries,
        retry_stop,
        retry_error,
        ..
    } = launched;
    let CandidateEvidence { candidate_commit, candidate_commit_error, .. } = candidate;
    let IngestResult { ingest_outcome, review_verdict, ingest_candidate, review_settled, spec_verdict } =
        ingested;

    let mut outcome = classify_outcome(&tally, exit_code, &ingest_outcome, kill);
    if let Some(line) = agy_print_timeout {
        outcome.note = format!(
            "{line} (agy's own print timeout, not thoth's wall-clock limit; agy below 1.2.6 stops after 5 minutes by default)"
        );
    }
    // brief §5: for a reviewer run the *task* status is the verdict. The run
    // status and the interruption classification are untouched - quota, crash
    // and timeout mean the same thing for a reviewer as for a worker.
    if let Some(v) = review_verdict {
        outcome.note = format!("review verdict {}: {}", v.as_str(), outcome.note);
    }
    if let Some(v) = spec_verdict {
        outcome.note = format!("spec review verdict {}: {}", v.as_str(), outcome.note);
    }
    // FC-1 (H2): a run that ended in error, not quota and not our own kill,
    // gets one auth check (the one `profile check` runs; it spends no quota).
    // Only the CLI itself saying "not logged in" reclassifies the run.
    let login_check = if kill.is_none() && needs_login_check(&outcome) {
        Some(apply_login_check(
            &mut outcome,
            &run.account,
            crate::commands::profile::probe_auth_profile(&profile),
        ))
    } else {
        None
    };

    // The one decision that differs by role, in one place. An implementer run
    // writes whatever `classify_outcome` decided, exactly as in Phase 1. A
    // reviewer run writes its verdict or nothing at all - never `running`,
    // never `failed`, never `interrupted` - so a review that did not reach a
    // verdict leaves the task `in_review` and `thoth review start` can be
    // run again.
    //
    // Whole-branch review Important 1: the reviewer's write is *conditional*
    // on the task still being `in_review`. A Supervisor who rerouted the task
    // while the review was in flight (or a second review that landed first)
    // must not have a stale verdict stamped over what the task is doing now;
    // `settle_terminal` records the not-applied verdict instead.
    // SR-2: a spec reviewer never writes the task's status, whatever happened.
    let task_status: Option<TaskStatusWrite> = if is_spec_review || review_settled {
        None
    } else if is_review {
        review_verdict
            .map(|v| TaskStatusWrite::conditional(VERDICT_ENTRY_STATUSES, v.task_status()))
    } else {
        Some(TaskStatusWrite::unconditional(outcome.task_status))
    };
    let candidate_commit = ingest_candidate.or(candidate_commit);
    let ended_at = ids::now_rfc3339();
    let finish = RunFinish {
        status: outcome.run_status,
        interrupted_reason: outcome.interrupted_reason,
        exit_code: exit_code.map(|c| c as i64),
        candidate_commit: candidate_commit.clone(),
        error_text: if outcome.run_status == RunStatus::Completed {
            None
        } else {
            Some(outcome.note.clone())
        },
        ended_at: ended_at.clone(),
    };
    let mut status_change_detail = serde_json::json!({
        "entity": "run",
        "id": run.run_id,
        "to": outcome.run_status.as_str(),
        "interrupted_reason": outcome.interrupted_reason.map(|r| r.as_str()),
        "exit_code": exit_code,
        "candidate_commit": candidate_commit,
        "candidate_commit_error": candidate_commit_error,
        "note": outcome.note,
        "stream_lines": tally.lines,
        "unparsable_lines": tally.unparsable_lines,
        "unparsable_bytes": tally.unparsable_bytes,
        "result_subtype": tally.result.as_ref().and_then(|r| r.subtype.clone()),
        "saw_rate_limit_warning": tally.saw_rate_limit_warning,
        "saw_api_retry": tally.saw_api_retry,
        "stream_drain_timed_out": drain_timed_out,
        "kill_error": kill_error,
        "evidence_recorded": watch.as_ref().map(|w| w.recorded).unwrap_or(0)
            + evidence_from_launches,
        "inbox_bad_lines": watch.as_ref().map(|w| w.bad_lines).unwrap_or(0),
        "inbox_unpaired": watch.as_ref().map(|w| w.pairer.pending()).unwrap_or(0),
        "review_verdict": review_verdict.map(|v| v.as_str()),
        "spec_review_verdict": spec_verdict.map(|v| v.as_str()),
        // Overwritten with `null` by `settle_terminal` if a conditional write
        // turns out not to apply, with `task_status_not_applied` beside it.
        "task_status_written": task_status.map(|w| w.to.as_str()),
        // FC-1: what the runner's auth check said; null when none ran.
        "login_check": login_check,
        // FC-2: how many `run_retry` events this run wrote, why the runner
        // stopped relaunching (null: the last launch was not a transient
        // failure), and, for `launch_error`, what failed.
        "retries": retries,
        "retry_stop": retry_stop,
        "retry_error": retry_error,
    });
    let interrupted_detail = if outcome.run_status == RunStatus::Interrupted {
        Some(serde_json::json!({
            "reason": outcome.interrupted_reason.map(|r| r.as_str()),
            "note": outcome.note,
            "harness_session_id": tally.session_id,
        }))
    } else {
        None
    };
    // S3-12: before the final `status_change`, whatever the outcome.
    crate::run_quota::record(
        conn,
        &gconn,
        paths,
        repo_id,
        run,
        &profile.harness,
        tally.session_id.as_deref(),
        &ended_at,
    );
    // MODELS-1: a model this account cannot use, learned from the result line.
    record_model_unavailable(
        conn,
        &gconn,
        &run_dir,
        run,
        &outcome,
        tally.result.as_ref(),
        &mut status_change_detail,
    );
    finish_and_settle(
        conn,
        &gconn,
        &run_dir,
        run,
        &outcome,
        &finish,
        task_status,
        interrupted_detail,
        status_change_detail,
    )?;
    crate::records::export_quietly(paths, &run_dir);
    // WT-1: a task that just became pass or cancelled no longer needs its
    // worker worktree. Never `?` and never touches the run's outcome; the
    // review worktree is left for the next full sweep.
    if let Ok(Some(task)) = state::get_task(conn, &run.task_id) {
        if task.status == TaskStatus::Pass.as_str() || task.status == TaskStatus::Cancelled.as_str() {
            use crate::commands::worktrees::{sweep, Scope, SOURCE_RUNNER};
            match sweep(paths, repo_id, repo_root, conn, Scope::Task(&run.task_id), SOURCE_RUNNER) {
                Ok(s) => {
                    for k in &s.kept {
                        let first = k.detail.lines().next().unwrap_or("");
                        log_runner_error(
                            &run_dir,
                            &format!("worktree kept: {} ({}): {first}", k.path, k.reason),
                        );
                    }
                }
                Err(e) => log_runner_error(&run_dir, &format!("worktree sweep failed: {e:#}")),
            }
        }
    }
    Ok(())
}
