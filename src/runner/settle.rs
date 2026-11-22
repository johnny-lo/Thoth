use crate::adapter::ResultEvent;
use crate::db;
use crate::db::global;
use crate::db::state::{self, NewEvent, RunFinish};
use crate::ids;
use crate::model::{event_type, InterruptedReason, RunStatus, TaskStatus};
use super::outcome::Outcome;
use anyhow::Result;
use rusqlite::Connection;
use std::io::Write;
use std::path::{Path, PathBuf};

/// brief section 10: a quota interruption writes a `quota_events` row and parks
/// the profile so a human (or `profile check`) has to clear it.
///
/// Fix round 1 (I1): the two global.db writes are wrapped in one transaction -
/// a parked profile can never land without its `quota_events` row, and vice
/// versa - and this function's `Result` is never `?`-propagated by its caller;
/// see `finish_and_settle`.
pub(super) fn record_quota_interruption(
    gconn: &Connection,
    run: &state::Run,
    note: &str,
    quota_group: Option<&str>,
) -> Result<()> {
    let tx = db::write_tx(gconn)?;
    global::insert_quota_event(
        &tx,
        &ids::now_rfc3339(),
        &run.account,
        "rate_limited",
        &serde_json::json!({
            "run_id": run.run_id,
            "task_id": run.task_id,
            "error_text": note,
            "quota_group": quota_group,
        }),
    )?;
    global::set_profile_status(&tx, &run.account, "rate_limited")?;
    tx.commit()?;
    Ok(())
}

/// The tail of `run_worker_inner`: best-effort quota bookkeeping (I1 - a
/// `SQLITE_BUSY` from a concurrent `dispatch`/`profile check` must not be able
/// to turn a correctly detected quota interruption into a plain `failed` row
/// with no `interrupted_reason`, which would strand it off both the
/// `resume`/reroute recovery routes that hang off `interrupted`), then the one
/// state.db settle that always has to happen. Extracted from
/// `run_worker_inner` so a unit test can exercise "quota bookkeeping failed,
/// the run is still settled `interrupted`/`quota`" without spawning a child.
#[allow(clippy::too_many_arguments)]
pub(super) fn finish_and_settle(
    conn: &Connection,
    gconn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    finish: &RunFinish,
    // `None` leaves `tasks.status` untouched - a reviewer run whose process
    // outcome must not move the task (see Task 7's rule: only a verdict may
    // move a task under review). A doc comment is not legal on a parameter, so
    // this is a plain one.
    task_status: Option<TaskStatusWrite>,
    interrupted_detail: Option<serde_json::Value>,
    mut status_change_detail: serde_json::Value,
) -> Result<()> {
    record_quota(conn, gconn, run_dir, run, outcome, &mut status_change_detail);
    record_login_failure(conn, gconn, run_dir, run, outcome, &mut status_change_detail);
    settle_or_log(conn, run_dir, run, outcome, finish, task_status, interrupted_detail, status_change_detail)
}

/// The first half of `finish_and_settle`: for a quota interruption, the
/// `quota_events` row, the parked profile and the `quota_exceeded` event.
/// Never fails; a bookkeeping failure goes into `status_change_detail`.
pub(crate) fn record_quota(
    conn: &Connection,
    gconn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    status_change_detail: &mut serde_json::Value,
) {
    if outcome.interrupted_reason == Some(InterruptedReason::Quota) {
        let quota_group = agy_quota_group(gconn, run);
        if let Err(e) = record_quota_interruption(gconn, run, &outcome.note, quota_group.as_deref()) {
            let msg = format!("{e:#}");
            log_runner_error(run_dir, &format!("record_quota_interruption failed: {msg}"));
            // #46 (final review): without this the profile is never parked
            // and no `quota_events` row exists, so nothing besides
            // `runner-error.log` (which no operator sees for a detached
            // runner) shows the bookkeeping failure. The run still correctly
            // settles `interrupted`/`quota` below; this just makes the
            // failure visible in the same `status_change` event without an
            // extra DB write.
            if let serde_json::Value::Object(map) = status_change_detail {
                map.insert(
                    "quota_bookkeeping_error".to_string(),
                    serde_json::Value::String(msg),
                );
            }
        }
        // Non-fatal like the other auxiliary events `handle_line` records: the
        // classification and the state.db settle below must go through
        // whatever this write does.
        let _ = state::append_event(
            conn,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::QUOTA_EXCEEDED.to_string(),
                detail: serde_json::json!({
                    "profile": run.account,
                    "note": outcome.note,
                    "quota_group": quota_group,
                }),
            },
        );
    }
}

/// MODELS-1: for a run Claude refused because the model needs usage credits
/// this account lacks, the `model_unavailable` quota event (what the launch
/// commands and `thoth models` read) and the run's `model_unavailable`
/// event. The profile is not parked and no `rate_limited` row is written.
/// Like `record_quota`, never fails: a global.db failure goes into
/// `status_change_detail` as `model_bookkeeping_error`.
pub(crate) fn record_model_unavailable(
    conn: &Connection,
    gconn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    result: Option<&ResultEvent>,
    status_change_detail: &mut serde_json::Value,
) {
    if !outcome.is_model_unavailable() {
        return;
    }
    let api_error = result.and_then(|r| r.api_error.clone());
    let api_error_code = result.and_then(|r| r.api_error_code.clone());
    let error_text = result.and_then(|r| r.text.clone());
    let plan = crate::model_list::current_plan(gconn, &run.account);
    if let Err(e) = global::insert_quota_event(
        gconn,
        &ids::now_rfc3339(),
        &run.account,
        crate::model_list::KIND_MODEL_UNAVAILABLE,
        &serde_json::json!({
            "model": run.model,
            "run_id": run.run_id,
            "task_id": run.task_id,
            "api_error": api_error,
            "api_error_code": api_error_code,
            "error_text": error_text,
            "plan": plan,
        }),
    ) {
        let msg = format!("{e:#}");
        log_runner_error(run_dir, &format!("record_model_unavailable failed: {msg}"));
        if let serde_json::Value::Object(map) = status_change_detail {
            map.insert("model_bookkeeping_error".to_string(), serde_json::Value::String(msg));
        }
    }
    let _ = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::MODEL_UNAVAILABLE.to_string(),
            detail: serde_json::json!({
                "profile": run.account,
                "model": run.model,
                "api_error": api_error,
                "note": outcome.note,
            }),
        },
    );
}

/// FC-1 (H4): which of an antigravity profile's quota groups the run's model
/// belongs to, from the profile's latest `live_quota` reading - exactly one
/// matching group, or `None`. `None` for every other harness, for a profile
/// that cannot be read, and when nobody has run `profile usage --refresh`
/// (then the whole profile is parked, as before).
fn agy_quota_group(gconn: &Connection, run: &state::Run) -> Option<String> {
    let profile = global::get_profile(gconn, &run.account).ok().flatten()?;
    if profile.harness != crate::adapter::antigravity::HARNESS_NAME {
        return None;
    }
    let windows = crate::commands::live_quota::read_agy_windows(gconn, &run.account).ok()?;
    let mut groups = crate::commands::live_quota::matching_groups(&windows, &run.model);
    if groups.len() == 1 {
        groups.pop()
    } else {
        None
    }
}

/// FC-1 (H2): the runner's auth check said the profile is not logged in.
/// Like `record_quota`, never fails: a global.db failure goes into
/// `status_change_detail` as `login_bookkeeping_error`.
fn record_login_failure(
    conn: &Connection,
    gconn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    status_change_detail: &mut serde_json::Value,
) {
    if outcome.interrupted_reason != Some(InterruptedReason::Auth) {
        return;
    }
    let write = || -> Result<()> {
        let tx = db::write_tx(gconn)?;
        global::insert_quota_event(
            &tx,
            &ids::now_rfc3339(),
            &run.account,
            "logged_out",
            &serde_json::json!({
                "run_id": run.run_id,
                "task_id": run.task_id,
                "error_text": outcome.note,
            }),
        )?;
        global::set_profile_status(&tx, &run.account, "logged_out")?;
        tx.commit()?;
        Ok(())
    };
    if let Err(e) = write() {
        let msg = format!("{e:#}");
        log_runner_error(run_dir, &format!("record_login_failure failed: {msg}"));
        if let serde_json::Value::Object(map) = status_change_detail {
            map.insert("login_bookkeeping_error".to_string(), serde_json::Value::String(msg));
        }
    }
    let _ = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::LOGIN_FAILED.to_string(),
            detail: serde_json::json!({
                "profile": run.account,
                "note": outcome.note,
            }),
        },
    );
}

/// The second half of `finish_and_settle`: the one state.db settle that
/// always has to happen.
#[allow(clippy::too_many_arguments)]
pub(crate) fn settle_or_log(
    conn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    finish: &RunFinish,
    task_status: Option<TaskStatusWrite>,
    interrupted_detail: Option<serde_json::Value>,
    status_change_detail: serde_json::Value,
) -> Result<()> {
    // I2/I3: one transaction for finish_run + status_change + (maybe)
    // run_interrupted + set_task_status, so the runs/tasks rows can never
    // disagree after a partial write.
    if !settle_terminal(conn, run, finish, task_status, interrupted_detail, status_change_detail)? {
        // Important 1: somebody else (cancel's escalation) already settled
        // this run first. Non-fatal - the row already carries a terminal
        // outcome, just not this one - but worth a trace since it means the
        // two writers raced.
        log_runner_error(
            run_dir,
            &format!(
                "run {} was already settled (probably by cancel's escalation) before this outcome could be written: {}",
                run.run_id, outcome.note
            ),
        );
    }
    Ok(())
}

/// Detached runners have no stderr a human will ever see, so any error we
/// cannot otherwise surface also goes to a plain-text log next to the run's
/// other artefacts. Never itself fails loudly: a logging failure must not
/// compound whatever we were already trying to report.
/// `pub(crate)` (fix round 1): `evidence::InboxTailer`/`record_command` also
/// need this, since inbox IO errors and blob-degradation warnings belong in
/// the run's own log, not a separate one.
pub(crate) fn log_runner_error(run_dir: &Path, msg: &str) {
    if std::fs::create_dir_all(run_dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("runner-error.log"))
    {
        let _ = writeln!(f, "[{}] {msg}", ids::now_rfc3339());
    }
}

/// The `tasks.risk` value ingest applies v6 §2.5 rule 5 with.
///
/// `tasks.risk`, not the run's frozen `spec` copy: the task row is what
/// `task update-spec` keeps current and what the Supervisor can see.
///
/// **Fails closed** (Task 5 fix round 1, controller ruling). Rule 5 is the one
/// *safety* rule in ingest, so a risk this function cannot read must not quietly
/// become `normal` and switch the rule off: a transient `SQLITE_BUSY` on a High
/// Risk task would then let an uncorroborated claim through. A missing row or a
/// read error is logged and reported as `high`, which is the strictest reading.
/// `thoth submit` already fails closed by propagating its own `get_task`
/// error (`commands::submit`), so both ingest entry points now agree.
///
/// The cost if the ruling is wrong is bounded and visible: a normal task's
/// result lands `unverified` instead of `in_review` and the operator re-submits.
pub(super) fn task_risk_for_ingest(conn: &Connection, task_id: &str, run_dir: &Path) -> String {
    match state::get_task(conn, task_id) {
        Ok(Some(task)) => task.risk,
        Ok(None) => {
            log_runner_error(
                run_dir,
                &format!(
                    "task {task_id} has no row while resolving the risk for ingest; \
                     applying the strictest risk ('high') to v6 §2.5 rule 5"
                ),
            );
            crate::model::Risk::High.as_str().to_string()
        }
        Err(e) => {
            log_runner_error(
                run_dir,
                &format!(
                    "reading task {task_id} to resolve the risk for ingest failed: {e:#}; \
                     applying the strictest risk ('high') to v6 §2.5 rule 5"
                ),
            );
            crate::model::Risk::High.as_str().to_string()
        }
    }
}

/// The task's title, for the commit message thoth writes on a codex
/// worker's behalf (scope §2.6). Not safety-sensitive like
/// `task_risk_for_ingest` above - a title this cannot read just falls back to
/// the task id, which still names the task unambiguously.
pub(super) fn task_title_for_commit(conn: &Connection, task_id: &str) -> String {
    match state::get_task(conn, task_id) {
        Ok(Some(task)) if !task.title.trim().is_empty() => task.title,
        _ => task_id.to_string(),
    }
}

/// Run-level terminal statuses (compared as the plain strings the
/// `runs.status` column stores). T8/T9 final-review consolidation (#28/#36):
/// this used to hardcode its own string match; it now defers to
/// `RunStatus::is_terminal` so the predicate has one definition. An
/// unparsable status (should never happen - `runs.status` is only ever
/// written from `RunStatus::as_str`) is conservatively treated as
/// non-terminal, matching the old match's fallback.
///
/// `pub(crate)` (fix round 2 / R4): `commands::verify::run`'s queued/running
/// guard now calls this too, instead of its own hardcoded `"queued" |
/// "running"` match, so there is exactly one definition of "is this run
/// still live" for a future non-terminal status to fall under automatically.
pub(crate) fn is_run_status_terminal(status: &str) -> bool {
    RunStatus::parse(status).map(|s| s.is_terminal()).unwrap_or(false)
}

/// The task statuses a reviewer's verdict is allowed to move the task out of.
///
/// Whole-branch review Important 1: a verdict is an answer to the question
/// "is *this* candidate acceptable", and that question is only open while the
/// task is `in_review`. Once a Supervisor has rerouted the task (it is
/// `running` again, or already `reject`/`pass` from another review), the
/// verdict is about a state of the world that no longer holds and must not be
/// written over whatever the task is doing now.
pub(crate) const VERDICT_ENTRY_STATUSES: &[TaskStatus] = &[TaskStatus::InReview];

/// What a settle should do to `tasks.status`.
///
/// Whole-branch review Important 1: `settle_terminal` used to take a plain
/// `Option<TaskStatus>` and write it with the **unconditional**
/// `state::set_task_status`. That is correct for an implementer run (the task
/// is `running` for that run's whole life, and this settle is what ends it)
/// and wrong for a reviewer run, whose settle can land seconds or minutes
/// after the Supervisor rerouted the task - stamping a stale `pass` over a
/// live implementer run. `only_from` is how the reviewer path says "only
/// while the task is still the thing I judged"; `None` keeps the Phase 1
/// behaviour for every other call site.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TaskStatusWrite {
    pub to: TaskStatus,
    pub only_from: Option<&'static [TaskStatus]>,
}

impl TaskStatusWrite {
    /// Phase 1's behaviour: write `to` whatever the task currently says.
    pub(crate) fn unconditional(to: TaskStatus) -> Self {
        Self { to, only_from: None }
    }

    /// Write `to` only while the task's status is one of `allowed`; otherwise
    /// settle the run, leave the task alone, and record that this happened.
    pub(crate) fn conditional(allowed: &'static [TaskStatus], to: TaskStatus) -> Self {
        Self {
            to,
            only_from: Some(allowed),
        }
    }
}

/// I2/I3 (fix round 1): commits `finish_run`, one `status_change` event, an
/// optional `run_interrupted` event, and the task's terminal status under one
/// transaction, so the `runs` and `tasks` rows can never disagree after a
/// partial write.
///
/// Whole-branch review Important 1: `finish_run` is now conditional on the
/// run still being non-terminal, so this settle as a whole is too - if
/// `finish_run` reports it did not apply (somebody else already settled this
/// run, e.g. the runner's own tail racing `cancel`'s escalation, or vice
/// versa), the transaction is rolled back without touching the
/// `status_change`/`run_interrupted` events or `tasks.status` at all, and
/// this returns `Ok(false)`. Callers must treat `false` as "nothing to do
/// here; the other writer's outcome stands" rather than as an error.
///
/// `pub(crate)` (Task 11): `cancel`'s escalation path settles the run through
/// this exact function rather than re-implementing the same atomic settle.
///
/// Whole-branch review Important 1: `task_status` is now a `TaskStatusWrite`,
/// so a caller can require the task to still be in a given set of statuses.
/// A conditional write that does not apply is **not** an error and does not
/// abort the settle: the run still settles, whatever the run already wrote
/// elsewhere (a `reviews` row, a `verdict` event) still stands, `tasks.status`
/// is left alone, and the fact is recorded twice - as
/// `task_status_not_applied` on this run's own `status_change` event, and as
/// a line in the run's `runner-error.log`.
///
/// The task-status write moved **above** the `status_change` event for that
/// reason: the event has to be able to say whether it applied, and both are
/// in the same transaction, so a reader sees them together either way.
pub(crate) fn settle_terminal(
    conn: &Connection,
    run: &state::Run,
    finish: &RunFinish,
    task_status: Option<TaskStatusWrite>,
    interrupted_detail: Option<serde_json::Value>,
    mut status_change_detail: serde_json::Value,
) -> Result<bool> {
    let tx = db::write_tx(conn)?;
    if !state::finish_run(&tx, &run.run_id, finish)? {
        // Rolls back on drop; nothing was written by this call.
        return Ok(false);
    }
    // `None` = settle the run, leave the task where it is. The run's own
    // `status_change` event below still records what happened, so an outcome is
    // never silently swallowed.
    let mut not_applied: Option<(TaskStatus, String)> = None;
    if let Some(write) = task_status {
        match write.only_from {
            None => state::set_task_status(&tx, &run.task_id, write.to, &finish.ended_at)?,
            Some(allowed) => {
                if !state::transition_task_status(
                    &tx,
                    &run.task_id,
                    allowed,
                    write.to,
                    &finish.ended_at,
                )? {
                    let found = state::get_task(&tx, &run.task_id)?
                        .map(|t| t.status)
                        .unwrap_or_else(|| "missing".to_string());
                    not_applied = Some((write.to, found));
                }
            }
        }
    }
    if let Some((to, found)) = &not_applied {
        if let serde_json::Value::Object(map) = &mut status_change_detail {
            // `task_status_written` keeps its meaning - the status this settle
            // actually wrote - so it goes back to `null`, and the companion key
            // says what was intended and what was in the way.
            map.insert("task_status_written".to_string(), serde_json::Value::Null);
            map.insert(
                "task_status_not_applied".to_string(),
                serde_json::json!({ "intended": to.as_str(), "found": found }),
            );
        }
    }
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::STATUS_CHANGE.to_string(),
            detail: status_change_detail,
        },
    )?;
    if let Some(detail) = interrupted_detail {
        state::append_event(
            &tx,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::RUN_INTERRUPTED.to_string(),
                detail,
            },
        )?;
    }
    tx.commit()?;
    if let Some((to, found)) = not_applied {
        log_runner_error(
            Path::new(&run.run_dir),
            &format!(
                "run {}: task {} was NOT moved to '{}' - it is '{}' now, not a status this \
                 settle was allowed to write over. The run's own outcome, and anything it \
                 already recorded (a reviews row, a verdict event), still stand.",
                run.run_id,
                run.task_id,
                to.as_str(),
                found
            ),
        );
    }
    Ok(true)
}

/// I2/I3 (fix round 1): last-resort settle for an error that escaped
/// `run_worker_inner` - a spawn failure, a file-open failure, an unexpected DB
/// error mid-loop, or anything else propagated with `?`. Without this the row
/// would stay stuck at `queued`/`running` forever. Never panics or propagates
/// further; this is the end of the line for error handling.
pub(super) fn best_effort_settle_on_error(conn: &Connection, run: &state::Run, err: &anyhow::Error) {
    let run_dir = PathBuf::from(&run.run_dir);
    let msg = format!("{err:#}");
    log_runner_error(&run_dir, &format!("run_worker failed: {msg}"));

    let current_status = match state::get_run(conn, &run.run_id) {
        Ok(Some(r)) => r.status,
        _ => run.status.clone(),
    };
    if is_run_status_terminal(&current_status) {
        return;
    }

    let ended_at = ids::now_rfc3339();
    let finish = RunFinish {
        status: RunStatus::Failed,
        interrupted_reason: None,
        exit_code: None,
        candidate_commit: None,
        error_text: Some(msg.clone()),
        ended_at: ended_at.clone(),
    };
    let detail = serde_json::json!({
        "entity": "run",
        "id": run.run_id,
        "to": "failed",
        "note": msg,
    });
    // A reviewer run that died before it could report leaves the task
    // `in_review`, so `thoth review start` can be retried; only an
    // implementer run's failure is the task's failure. A spec reviewer never
    // writes the task's status (SR-2).
    let task_status = if run.role == crate::model::role::REVIEWER
        || run.role == crate::model::role::SPEC_REVIEWER
    {
        None
    } else {
        Some(TaskStatusWrite::unconditional(TaskStatus::Failed))
    };
    match settle_terminal(conn, run, &finish, task_status, None, detail) {
        Ok(true) => {}
        Ok(false) => log_runner_error(
            &run_dir,
            &format!(
                "run {} was already settled by someone else before this error-path settle could apply",
                run.run_id
            ),
        ),
        Err(e) => log_runner_error(&run_dir, &format!("settle_terminal failed: {e:#}")),
    }
}
