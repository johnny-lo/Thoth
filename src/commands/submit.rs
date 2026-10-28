use crate::config::Paths;
use crate::db::state::{self, Run};
use crate::ids;
use crate::ingest::{self, IngestOutcome};
use crate::model::{role, TaskStatus};
use crate::output;
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::path::Path;

/// The first reviewer run of `task_id` that is still `queued`/`running`.
///
/// Only a *reviewer* run blocks a `submit` (whole-branch review Important 1).
/// A live implementer run is already handled by this command's own
/// run/task-status precondition, and an operator hand-fixing a worktree after
/// a crash is exactly who `submit` exists for. A live spec reviewer (SR-2)
/// blocks it the same way.
fn live_reviewer_for_task(conn: &Connection, task_id: &str) -> Result<Option<Run>> {
    Ok(state::list_live_runs_for_task(conn, task_id)?
        .into_iter()
        .find(|r| r.role == role::REVIEWER || r.role == role::SPEC_REVIEWER))
}

/// Manual ingest of a result.json for a run (v6 section 10.7). Same validation
/// and the same UNVERIFIED fallback as the runner's automatic path
/// (`ingest::ingest_path`, which the runner also calls on a clean exit).
///
/// Precondition (Task 11 controller ruling, fix round 1 - widened from the
/// brief's literal "the run is completed or the task is unverified/in_review"
/// to fit the runner's actual terminal states: a crash or a timeout leaves
/// the run `interrupted`/`failed`, not just `completed`, and an operator who
/// hand-fixes the worktree after either still needs `submit` to land the
/// result without a full reroute): **allowed unless the run is
/// `queued`/`running` AND the task is not `unverified`/`in_review`.**
/// Equivalently: refused only while the run is still live (`queued` or
/// `running`) and the task hasn't already been marked `unverified` or
/// `in_review` by an earlier ingest.
pub fn run(paths: &Paths, cwd: &Path, file: &Path, needle: &str, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let run_id = ids::resolve_prefix(&state::list_run_ids(&conn)?, needle)?;
    let run = state::get_run(&conn, &run_id)?
        .ok_or_else(|| anyhow::anyhow!("run {run_id} disappeared"))?;
    let task = state::get_task(&conn, &run.task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {} disappeared", run.task_id))?;

    let run_is_live = matches!(run.status.as_str(), "queued" | "running");
    let task_resubmittable = matches!(task.status.as_str(), "unverified" | "in_review");
    if run_is_live && !task_resubmittable {
        bail!(
            "run {run_id} is '{}' (task {} is '{}'); submit needs a finished run or an \
             unverified/in_review task",
            run.status,
            run.task_id,
            task.status
        );
    }

    // Important 1 (whole-branch review): `submit` writes `tasks.status`, and
    // a reviewer run in flight is about to write it too - with the verdict
    // for the candidate this submit may be replacing. Whichever landed second
    // won, silently. Refused here, cheaply, before anything is ingested (so a
    // refusal leaves no claims behind), and again authoritatively inside the
    // transaction that writes the status below.
    if let Some(live) = live_reviewer_for_task(&conn, &run.task_id)? {
        bail!(
            "{}",
            crate::commands::live_run_refusal(
                &format!("refusing to submit a result for run {run_id}"),
                &run.task_id,
                &live,
            )
        );
    }

    let outcome = ingest::ingest_path(&conn, &run, &task.risk, file);
    let now = ids::now_rfc3339();
    let (task_status, ok, note) = match &outcome {
        IngestOutcome::Ok { summary, claim_count } => (
            TaskStatus::InReview,
            true,
            format!("ingested {claim_count} claim(s): {summary}"),
        ),
        IngestOutcome::Invalid { reason } => (
            TaskStatus::Unverified,
            false,
            format!("result.json rejected: {reason}"),
        ),
        IngestOutcome::Skipped => (
            TaskStatus::Unverified,
            false,
            "nothing was ingested".to_string(),
        ),
    };
    // The authoritative half of the guard above: the write lock is held from
    // `BEGIN`, so a reviewer run that committed since the cheap check is
    // visible here and this refuses instead of racing it.
    let tx = crate::db::write_tx(&conn)?;
    if let Some(live) = live_reviewer_for_task(&tx, &run.task_id)? {
        drop(tx); // explicit: rolls back the status write this refuses to make
        bail!(
            "{}",
            crate::commands::live_run_refusal(
                &format!(
                    "run {run_id}'s result was ingested, but task {}'s status was NOT moved \
                     to '{}'",
                    run.task_id,
                    task_status.as_str()
                ),
                &run.task_id,
                &live,
            )
        );
    }
    state::set_task_status(&tx, &run.task_id, task_status, &now)?;
    tx.commit()?;

    let v = serde_json::json!({
        "ok": ok,
        "run_id": run_id,
        "task_id": run.task_id,
        "task_status": task_status.as_str(),
        "note": note,
    });
    output::emit(json, &v, &format!("{run_id}: {note} -> task {}", task_status.as_str()));
    Ok(if ok { 0 } else { 1 })
}
