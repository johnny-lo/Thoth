use crate::config::Paths;
use crate::db;
use crate::db::global;
use crate::db::state::{self, NewEvent};
use crate::ids;
use crate::model::{event_type, role, TaskStatus};
use crate::output;
use anyhow::{bail, Result};
use std::path::Path;

pub fn run(paths: &Paths, cwd: &Path, needle: &str, json: bool) -> Result<i32> {
    let (_repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let run_id = ids::resolve_prefix(&state::list_run_ids(&conn)?, needle)?;
    let run = state::get_run(&conn, &run_id)?
        .ok_or_else(|| anyhow::anyhow!("run {run_id} disappeared"))?;

    // Task 7 fix round 1 (I1): a reviewer run is not resumable, and this check
    // comes before every other precondition because an interrupted reviewer run
    // with a live session satisfies all of them. Resuming one would write
    // `tasks.status = running` below, moving the task out of `in_review` for
    // the review's whole duration - and, if the resumed review then reached no
    // verdict, leaving it `running` for good, a status `review start` and
    // `dispatch` both refuse. A review is read-only and cheap to restart, so
    // the supported recovery is a fresh `review start`, which already reuses
    // the review worktree (see `a_crashed_reviewer_leaves_the_task_in_review...`).
    // Refuses the same way every other precondition here does: `bail!`, so
    // nothing is written and `main` renders it in the caller's chosen shape.
    if run.role == role::REVIEWER {
        bail!(
            "run {run_id} is a reviewer run for task {}, and reviews are not resumable \
             (resuming would move the task out of in_review); start a fresh one with \
             `thoth review start {}`",
            run.task_id,
            run.task_id
        );
    }
    // SR-2: likewise a spec reviewer; `spec review start` reuses its worktree.
    if run.role == role::SPEC_REVIEWER {
        bail!(
            "run {run_id} is a spec reviewer run for task {}, and spec reviews are not resumable; \
             start a fresh one with `thoth spec review start {} --profile ...`",
            run.task_id,
            run.task_id
        );
    }
    if run.status != "interrupted" {
        bail!("run {run_id} is '{}'; resume needs an interrupted run", run.status);
    }
    let Some(session_id) = run.harness_session_id.clone() else {
        bail!("run {run_id} has no harness_session_id, so there is nothing to resume");
    };
    if run.started_at.is_none() {
        bail!("run {run_id} never started, so there is no session to resume");
    }
    // Resume is bound to the config dir that created the session
    // (01-claude-cli-identity.md:130), so the profile must still agree.
    let gconn = db::open_global(paths)?;
    let profile = match global::get_profile(&gconn, &run.account)? {
        None => bail!("profile {} is gone; reroute instead of resuming", run.account),
        Some(p) if p.config_dir != run.config_dir => bail!(
            "profile {} now points at {} but run {run_id} used {}; resume would fail with \
             'No conversation found'. Reroute with `thoth dispatch` instead.",
            run.account,
            p.config_dir,
            run.config_dir
        ),
        Some(p) => p,
    };
    // A resumed run keeps its model, so the same rule `dispatch` applies.
    // MODELS-1: then the stored model list (agy) or the account's recorded
    // unavailable models (Claude); `resume` runs no login check, so it has
    // only the stored list.
    let known_models = crate::model_list::known_models_text(&gconn, &profile);
    crate::adapter::for_harness(&profile.harness).check_model(&profile.profile_id, &run.model, &known_models)?;
    let model_warning = crate::model_list::check_launch(&gconn, &profile, &run.model, None)?;

    // Sets `runs.status` back to `queued` while keeping `started_at` and
    // `harness_session_id` intact - that pair of untouched columns is exactly
    // what tells the runner (`is_resume` in `runner::run_worker_inner`) to
    // build a resume command (`--resume <session_id>`, or `codex exec resume
    // <thread_id>`) instead of a fresh one.
    //
    // Important 5 (whole-branch review): every precondition above is about
    // *this* run, and none of them notices that another run of the same task
    // is already live. The case that bites: R1 is interrupted, the operator
    // reroutes with `dispatch` (R2 starts in the same worktree, same attempt),
    // and then runs `thoth resume R1` - two `claude` workers committing
    // into one worktree, both writing `.agent/result.json`. A live reviewer
    // run is covered by the same check. Checked inside the write transaction
    // that performs `prepare_resume`, immediately before it, so a `dispatch`
    // racing this one either loses the write lock and then sees the resumed
    // row, or commits first and is seen here.
    let tx = db::write_tx(&conn)?;
    // WT-1: a pass, cancelled or accepted task is finished, and its worktree
    // may already have been removed, so none of its runs is resumed.
    if let Some(task) = state::get_task(&tx, &run.task_id)? {
        if matches!(
            TaskStatus::parse(&task.status),
            Ok(TaskStatus::Pass | TaskStatus::Cancelled | TaskStatus::Accepted)
        ) {
            drop(tx); // explicit: rolls back, though nothing has been written yet
            bail!(
                "task {} is '{}'; resume needs a task that is not pass, cancelled or accepted",
                run.task_id,
                task.status
            );
        }
    }
    if let Some(live) = state::list_live_runs_for_task(&tx, &run.task_id)?
        .into_iter()
        .find(|r| r.run_id != run_id)
    {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "{}",
            crate::commands::live_run_refusal(
                &format!("refusing to resume run {run_id}"),
                &run.task_id,
                &live,
            )
        );
    }
    // S3-9 AC7: once a later run of the same attempt has started on this
    // worktree (a handoff), resuming the old conversation would carry on over
    // the newer run's changes and mix the two.
    if let Some(newer) = state::list_runs_for_task(&tx, &run.task_id)?
        .into_iter()
        .find(|r| {
            r.attempt == run.attempt
                && r.role == role::IMPLEMENTER
                && r.started_at.is_some()
                && r.run_id > run_id
        })
    {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "refusing to resume run {run_id}: its work was continued by run {} in the same \
             worktree, so resuming would mix the two. Dispatch task {} again instead.",
            newer.run_id,
            run.task_id
        );
    }
    state::prepare_resume(&tx, &run_id)?;
    tx.commit()?;
    // The detached runner inherits this process's environment, so the binary
    // this shell would launch is the binary the resumed run will use.
    let harness_bin = crate::adapter::harness_bin(&profile.harness);
    if let Err(e) = state::set_run_harness_bin(&conn, &run_id, &harness_bin) {
        masked_eprintln!(
            "thoth: warning: could not record the harness binary for {run_id}: {e}; \
             cancel will fall back to this shell's THOTH_CLAUDE_BIN"
        );
    }
    let runner_pid = crate::commands::dispatch::spawn_runner(&root, &run_id)?;

    // I3 (fix round 2, mirrors dispatch.rs's I2 controller ruling): the
    // runner is already live at this point, so a failure in any of the
    // bookkeeping below must not be reported as a failed resume - that would
    // make an operator think resume never took, and a retry would then race
    // a runner that is, in fact, already running. Every write from here on
    // is best-effort: collect failures into `warnings` and still return
    // success.
    let mut warnings: Vec<String> = Vec::new();
    warnings.extend(model_warning);

    if state::set_runner_pid(&conn, &run_id, runner_pid as i64).is_err() {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Err(e) = state::set_runner_pid(&conn, &run_id, runner_pid as i64) {
            warnings.push(format!(
                "runner started (pid {runner_pid}) but recording runner_pid failed: {e}; \
                 cancel escalation will need the pid manually"
            ));
        }
    }

    let now = ids::now_rfc3339();
    if let Err(e) = state::set_task_status(&conn, &run.task_id, TaskStatus::Running, &now) {
        warnings.push(format!(
            "runner started (pid {runner_pid}) but updating task {} to running failed: {e}; \
             `status` may show a stale value until the run finishes",
            run.task_id
        ));
    }
    if let Err(e) = state::append_event(
        &conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run_id.clone()),
            event_type: event_type::STATUS_CHANGE.to_string(),
            detail: serde_json::json!({
                "entity": "run",
                "id": run_id,
                "to": "queued",
                "mode": "resume",
                "harness_session_id": session_id,
                "runner_pid": runner_pid,
            }),
        },
    ) {
        warnings.push(format!(
            "runner started (pid {runner_pid}) but recording the resume event failed: {e}"
        ));
    }

    // Warnings travel on stderr in human mode (so they cannot be mistaken
    // for the run id a script parses) and inside the document in --json
    // mode, same convention as `dispatch`.
    if !json {
        for w in &warnings {
            masked_eprintln!("warning: {w}");
        }
    }
    let v = serde_json::json!({
        "ok": true,
        "run_id": run_id,
        "task_id": run.task_id,
        "harness_session_id": session_id,
        "runner_pid": runner_pid,
        "warnings": warnings,
    });
    output::emit(
        json,
        &v,
        &format!("resuming {run_id} with session {session_id} (runner pid {runner_pid})"),
    );
    Ok(0)
}
