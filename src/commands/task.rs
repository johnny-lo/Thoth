use crate::commands::spec_lint;
use crate::commands::spec_review;
use crate::commands::worktrees;
use crate::config::Paths;
use crate::db;
use crate::db::state::{self, NewEvent, Task};
use crate::ids;
use crate::model::{event_type, TaskStatus};
use crate::output;
use crate::spec;
use crate::worktree;
use anyhow::{bail, Context, Result};
use std::path::Path;

/// What the worktree sweep of `task create` / `task close` / `task accept` did (WT-1).
enum Cleanup {
    /// THOTH_RUN_ID is set: a run never removes other tasks' worktrees, and
    /// nothing about it is printed.
    NotRun,
    /// The shell is inside a worktree, which the sweep could remove under it.
    SkippedInWorktree,
    Ran(worktrees::Sweep),
    Failed(String),
}

const SKIPPED_IN_WORKTREE: &str = "skipped: run from inside a worktree";

/// Sweeps every finished task's worktrees, unless this is a run or the shell
/// stands inside the worktrees directory.
fn sweep_finished(
    paths: &Paths,
    repo_id: &str,
    root: &Path,
    conn: &rusqlite::Connection,
    cwd: &Path,
    source: &str,
) -> Cleanup {
    if worktrees::inside_a_run() {
        return Cleanup::NotRun;
    }
    if worktrees::cwd_inside_worktrees(paths, repo_id, cwd) {
        return Cleanup::SkippedInWorktree;
    }
    match worktrees::sweep(paths, repo_id, root, conn, worktrees::Scope::All, source) {
        Ok(s) => Cleanup::Ran(s),
        Err(e) => Cleanup::Failed(format!("{e:#}")),
    }
}

/// Resolve the repo this invocation is inside, and open its state db.
pub fn open_repo(paths: &Paths, cwd: &Path) -> Result<(String, std::path::PathBuf, rusqlite::Connection)> {
    let root = worktree::repo_root_from(cwd)?;
    let repo_id = ids::repo_id_for(&root)?;
    if !paths.state_db(&repo_id).is_file() {
        bail!(
            "repo {} is not initialised; run `thoth init` in {}",
            repo_id,
            root.display()
        );
    }
    let conn = db::open_state(paths, &repo_id)?;
    Ok((repo_id, root, conn))
}

pub fn create(
    paths: &Paths,
    cwd: &Path,
    spec_path: &Path,
    title: Option<&str>,
    base: Option<&str>,
    json: bool,
) -> Result<i32> {
    let (repo_id, root, conn) = open_repo(paths, cwd)?;
    let raw = std::fs::read_to_string(spec_path)
        .with_context(|| format!("reading spec {}", spec_path.display()))?;
    let parsed = spec::parse_spec(&raw)?;
    // RD-2: before spec lint, whose messages quote the spec's sentences.
    let masker = crate::redact::Masker::load(Some(&paths.home));
    if let Some(refusal) = spec::secret_refusal(&raw, &masker) {
        bail!("{refusal}");
    }
    if let Some(title) = title {
        let mut kinds: Vec<String> = Vec::new();
        for span in masker.find(title) {
            if !kinds.contains(&span.kind) {
                kinds.push(span.kind);
            }
        }
        if !kinds.is_empty() {
            bail!(
                "refusing: the title contains what looks like a secret ({}); nothing was stored. Choose another title and run the command again.",
                kinds.join(", ")
            );
        }
    }
    let lint = spec_lint::for_repo(&conn, &root, &raw)?;
    if let Some(refusal) = lint.refusal() {
        bail!("{refusal}");
    }
    let (task_type, difficulty) = spec::labels(&raw);
    let (base_commit, base_branch) = resolve_base(cwd, base)?;

    let now = ids::now_rfc3339();
    let task_id = ids::new_task_id();
    let title = title
        .map(|s| s.to_string())
        .unwrap_or_else(|| first_line(&parsed.goal));
    let task = Task {
        task_id: task_id.clone(),
        title: title.clone(),
        // store the caller's exact bytes so nothing the worker may need is lost
        spec: raw.clone(),
        risk: parsed.risk.clone(),
        status: TaskStatus::Queued.as_str().to_string(),
        base_commit: base_commit.clone(),
        created_at: now.clone(),
        updated_at: now.clone(),
    };
    // #7 (final review): one transaction, so a crash between the two writes
    // can never leave a task row with no task_created event behind it.
    let tx = conn.unchecked_transaction()?;
    state::insert_task(&tx, &task)?;
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: None,
            event_type: event_type::TASK_CREATED.to_string(),
            detail: serde_json::json!({
                "title": title,
                "risk": parsed.risk,
                "base_commit": base_commit,
                "base_branch": base_branch,
                "base_ref": base,
                "acceptance_criteria": parsed.acceptance_criteria.len(),
                "task_type": task_type,
                "difficulty": difficulty,
                "spec_sha256": spec_review::fingerprint(&raw),
            }),
        },
    )?;
    tx.commit()?;

    // WT-1: the next task's start is when the previous ones' leftovers go -
    // review worktrees, and any worktree an earlier sweep had to skip. The
    // events are the record; only a failure is reported, as a warning.
    let sweep_error = match sweep_finished(paths, &repo_id, &root, &conn, cwd, worktrees::SOURCE_TASK_CREATE) {
        Cleanup::Failed(e) => Some(format!("worktree cleanup failed: {e}")),
        _ => None,
    };

    let mut warnings = crate::checks::unapproved_warnings(&conn, &parsed.checks)?;
    // SR-1 (H11): say up front that the first dispatch is gated.
    if spec_review::gated_risk(&parsed.risk) {
        warnings.push(format!(
            "risk {}: a spec review APPROVE of this spec is required before the first dispatch; \
             record one with thoth spec review record {task_id} -f spec-review.json, or dispatch \
             with --skip-spec-review REASON",
            parsed.risk
        ));
    }
    warnings.extend(sweep_error);
    warnings.extend(lint.warning_lines());
    if !json {
        for w in &warnings {
            masked_eprintln!("warning: {w}");
        }
    }
    let v = serde_json::json!({
        "ok": true,
        "task_id": task_id,
        "status": TaskStatus::Queued.as_str(),
        "base_commit": base_commit,
        "base_branch": base_branch,
        "title": title,
        "warnings": warnings,
    });
    output::emit(json, &v, &format!("{task_id}  queued  {title}"));
    Ok(0)
}

/// BASE-1: the task's start and the local branch it was taken from. `base`
/// resolves in `cwd`, so `HEAD` is that worktree's; without it the start is
/// the HEAD of the worktree `cwd` is in, which for a linked worktree is not the
/// main worktree's. The branch is named only when `base` is exactly a local
/// branch (or, without `base`, the branch checked out in `cwd`).
fn resolve_base(cwd: &Path, base: Option<&str>) -> Result<(String, Option<String>)> {
    let Some(rev) = base else {
        return Ok((worktree::rev_parse_head(cwd)?, worktree::head_branch(cwd)?));
    };
    let commit = worktree::resolve_commit(cwd, rev).map_err(|e| anyhow::anyhow!("--base {rev}: {e:#}"))?;
    let name = rev.trim();
    let is_branch = crate::process::quiet(
        std::process::Command::new("git")
            .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}")])
            .current_dir(cwd),
    )
    .status()?
    .success();
    Ok((commit, is_branch.then(|| name.to_string())))
}

/// `base_branch` from the task's newest `task_created` event; null when the
/// event predates it.
fn base_branch_of(conn: &rusqlite::Connection, task_id: &str) -> Result<serde_json::Value> {
    Ok(state::latest_event_of_type(conn, event_type::TASK_CREATED, Some(task_id))?
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.detail).ok())
        .map(|d| d["base_branch"].clone())
        .unwrap_or(serde_json::Value::Null))
}

pub fn show(paths: &Paths, cwd: &Path, needle: &str, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = open_repo(paths, cwd)?;
    let ids_list = state::list_task_ids(&conn)?;
    let task_id = ids::resolve_prefix(&ids_list, needle)?;
    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    let spec_value: serde_json::Value =
        serde_json::from_str(&task.spec).unwrap_or(serde_json::Value::Null);
    let v = serde_json::json!({
        "task_id": task.task_id,
        "title": task.title,
        "risk": task.risk,
        "status": task.status,
        "base_commit": task.base_commit,
        "base_branch": base_branch_of(&conn, &task.task_id)?,
        "created_at": task.created_at,
        "updated_at": task.updated_at,
        "spec": spec_value,
    });
    let human = format!(
        "{}\n  title:   {}\n  status:  {}\n  risk:    {}\n  base:    {}\n  created: {}\n  spec:\n{}",
        task.task_id, task.title, task.status, task.risk, task.base_commit, task.created_at, task.spec
    );
    output::emit(json, &v, &human);
    Ok(0)
}

/// brief §9: who may rewrite a spec, and when.
///
/// `draft` never started. `reject` and `unverified` are the two statuses the
/// rework loop hands back to a human. A `running` task has a worker reading the
/// old spec right now, and `pass`/`cancelled`/`accepted` are decisions - editing one
/// would rewrite history rather than plan work. `queued` (SR-1): a spec review
/// that comes back REVISE finds the task still queued, with no worker reading
/// the spec yet.
pub const UPDATE_SPEC_STATUSES: &[TaskStatus] = &[
    TaskStatus::Draft,
    TaskStatus::Queued,
    TaskStatus::Reject,
    TaskStatus::Unverified,
];

/// Replaces a task's spec, which is how an important/high-risk rework proceeds
/// after `dispatch` refuses to fold findings mechanically.
pub fn update_spec(
    paths: &Paths,
    cwd: &Path,
    needle: &str,
    file: &Path,
    json: bool,
) -> Result<i32> {
    let (_repo_id, root, conn) = open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;
    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    if !UPDATE_SPEC_STATUSES.iter().any(|s| s.as_str() == task.status) {
        bail!(
            "task {task_id} is '{}'; update-spec needs draft, queued, reject or unverified",
            task.status
        );
    }

    let before_state = spec_review::state(&conn, &task)?;
    let raw = std::fs::read_to_string(file)
        .with_context(|| format!("reading spec {}", file.display()))?;
    let parsed = spec::parse_spec(&raw)?;
    if let Some(refusal) = spec::secret_refusal(&raw, &crate::redact::Masker::load(Some(&paths.home))) {
        bail!("{refusal}");
    }
    let lint = spec_lint::for_repo(&conn, &root, &raw)?;
    if let Some(refusal) = lint.refusal() {
        bail!("{refusal}");
    }
    let (task_type, difficulty) = spec::labels(&raw);
    let before = spec::parse_spec(&task.spec).ok();
    let now = ids::now_rfc3339();
    // Stores the caller's exact bytes, the same as `create`, so nothing the
    // worker may need is lost. One transaction for the write and its event,
    // following the convention commit 6153a9b established for `task create`
    // (#7): a crash between the two must not leave a rewritten spec with no
    // `spec_updated` event explaining it.
    //
    // R1 (fix round 1): the gate above read `task.status` before this
    // transaction opened, so a `dispatch` flipping `reject -> running` in
    // between would otherwise let this rewrite the spec under a live worker.
    // The `WHERE ... AND status IN (...)` inside `set_task_spec` makes the
    // check and the write atomic; zero rows means the state moved, and the
    // whole transaction (event included) rolls back.
    let tx = conn.unchecked_transaction()?;
    if !state::set_task_spec(&tx, &task_id, UPDATE_SPEC_STATUSES, &raw, &parsed.risk, &now)? {
        let now_status = state::get_task(&tx, &task_id)?
            .map(|t| t.status)
            .unwrap_or_else(|| "unknown".to_string());
        drop(tx); // explicit: rolls back, so nothing was written
        bail!(
            "task {task_id} is '{now_status}'; update-spec needs draft, queued, reject or \
             unverified (state changed underneath)"
        );
    }
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: None,
            event_type: event_type::SPEC_UPDATED.to_string(),
            detail: serde_json::json!({
                "risk_from": task.risk,
                "risk_to": parsed.risk,
                "goal_changed": before.as_ref().map(|b| b.goal != parsed.goal).unwrap_or(true),
                "acceptance_criteria_from": before.as_ref().map(|b| b.acceptance_criteria.len()),
                "acceptance_criteria_to": parsed.acceptance_criteria.len(),
                "bytes": raw.len(),
                "task_type": task_type,
                "difficulty": difficulty,
                "spec_sha256": spec_review::fingerprint(&raw),
            }),
        },
    )?;
    tx.commit()?;

    let mut warnings = crate::checks::unapproved_warnings(&conn, &parsed.checks)?;
    // SR-1 (H10): an approval is bound to the spec bytes it judged.
    let new_sha = spec_review::fingerprint(&raw);
    if let Some(n) = before_state
        .newest
        .as_ref()
        .filter(|_| before_state.approved && new_sha != before_state.spec_sha256)
    {
        warnings.push(format!(
            "spec review {} (APPROVE) no longer applies: the spec changed (sha256 {} -> {}); \
             review the new spec again before its first dispatch",
            n.spec_review_id,
            spec_review::short(&before_state.spec_sha256),
            spec_review::short(&new_sha)
        ));
    }
    warnings.extend(lint.warning_lines());
    if !json {
        for w in &warnings {
            masked_eprintln!("warning: {w}");
        }
    }
    let v = serde_json::json!({
        "ok": true,
        "task_id": task_id,
        "status": task.status,
        "risk": parsed.risk,
        "acceptance_criteria": parsed.acceptance_criteria.len(),
        "goal": parsed.goal,
        "warnings": warnings,
    });
    output::emit(
        json,
        &v,
        &format!(
            "{task_id} spec updated ({} acceptance criteria, risk {}); \
             dispatch again to rework it",
            parsed.acceptance_criteria.len(),
            parsed.risk
        ),
    );
    Ok(0)
}

/// Who may be closed: the four statuses a task stops at when nothing will
/// move it on by itself, plus `queued` (SR-1: a task whose spec keeps coming
/// back REVISE is stopped before its first dispatch, so it has no run and no
/// worktree; closing it only cancels it). `draft`/`running`/`in_review` still
/// have work ahead of them, and `pass`/`cancelled`/`accepted` are already
/// decided. A document task in `in_review` is closed too, through
/// `DOCUMENT_CLOSE_STATUSES`: it has no review ahead of it.
pub const CLOSE_STATUSES: &[TaskStatus] = &[
    TaskStatus::Queued,
    TaskStatus::Failed,
    TaskStatus::Interrupted,
    TaskStatus::Unverified,
    TaskStatus::Reject,
];

/// `CLOSE_STATUSES` plus `in_review`: the statuses a docs, research or review
/// task is closed from. Never used for a code task, whose `in_review` waits
/// for a review.
const DOCUMENT_CLOSE_STATUSES: &[TaskStatus] = &[
    TaskStatus::Queued,
    TaskStatus::Failed,
    TaskStatus::Interrupted,
    TaskStatus::Unverified,
    TaskStatus::Reject,
    TaskStatus::InReview,
];

/// Abandons a task: `tasks.status: queued | failed | interrupted | unverified
/// | reject -> cancelled` (and `in_review -> cancelled` for a document task),
/// plus one `task_closed` event. Then removes the clean worktrees of every
/// finished task, this one included (WT-1); its branch stays, and a worktree
/// with uncommitted changes is kept and reported.
pub fn close(paths: &Paths, cwd: &Path, needle: &str, reason: &str, json: bool) -> Result<i32> {
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("task close needs a non-empty --reason");
    }
    let (repo_id, root, conn) = open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;

    // The gate, the write and the event share one transaction, the same as
    // `update_spec`: the conditional update makes a status that moved in the
    // meantime a refusal instead of a lost write, and the refusal rolls back.
    let tx = db::write_tx(&conn)?;
    let task = state::get_task(&tx, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    let allowed = if spec::is_document_task(&task.spec) {
        DOCUMENT_CLOSE_STATUSES
    } else {
        CLOSE_STATUSES
    };
    if !allowed.iter().any(|s| s.as_str() == task.status) {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        let in_review_note = if task.status == TaskStatus::InReview.as_str() {
            ". From in_review only a docs, research or review task can be closed; this one \
             waits for a review (thoth review start, thoth review record)"
        } else {
            ""
        };
        bail!(
            "task {task_id} is '{}'; task close needs queued, failed, interrupted, unverified or reject{in_review_note}",
            task.status
        );
    }
    if let Some(live) = state::list_live_runs_for_task(&tx, &task_id)?.into_iter().next() {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "{}",
            crate::commands::live_run_refusal("task close", &task_id, &live)
        );
    }
    let now = ids::now_rfc3339();
    if !state::transition_task_status(&tx, &task_id, allowed, TaskStatus::Cancelled, &now)? {
        drop(tx); // explicit: rolls back, so nothing was written
        bail!(
            "task {task_id} changed status underneath task close; \
             task close needs queued, failed, interrupted, unverified or reject"
        );
    }
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: None,
            event_type: event_type::TASK_CLOSED.to_string(),
            detail: serde_json::json!({
                "from": task.status,
                "reason": reason,
            }),
        },
    )?;
    tx.commit()?;

    let cleanup = sweep_finished(paths, &repo_id, &root, &conn, cwd, worktrees::SOURCE_TASK_CLOSE);
    let mut v = serde_json::json!({
        "ok": true,
        "task_id": task_id,
        "previous_status": task.status,
        "status": TaskStatus::Cancelled.as_str(),
        "reason": reason,
    });
    let mut human = format!("{task_id} closed ({} -> cancelled)", task.status);
    report_cleanup(&cleanup, &mut v, &mut human);
    output::emit(json, &v, &human);
    Ok(0)
}

/// Adds what the sweep of `task close` / `task accept` did to its `--json`
/// document (`worktrees`) and its human line.
fn report_cleanup(cleanup: &Cleanup, v: &mut serde_json::Value, human: &mut String) {
    let failure = match cleanup {
        Cleanup::NotRun => None,
        Cleanup::SkippedInWorktree => Some(SKIPPED_IN_WORKTREE.to_string()),
        Cleanup::Failed(e) => Some(e.clone()),
        Cleanup::Ran(s) => {
            v["worktrees"] = serde_json::json!({
                "removed": s.removed.iter().map(worktrees::removed_json).collect::<Vec<_>>(),
                "kept": s.kept.iter().map(worktrees::kept_json).collect::<Vec<_>>(),
            });
            human.push_str(&format!(
                "; worktrees removed: {}, kept: {}",
                s.removed.len(),
                s.kept.len()
            ));
            for k in &s.kept {
                human.push_str(&format!("\n  kept {}: {}", k.path, k.reason));
                if worktrees::is_uncommitted(k) {
                    human.push_str(&format!(
                        "; run git worktree remove --force {} to drop the changes",
                        k.path
                    ));
                }
            }
            None
        }
    };
    if let Some(error) = failure {
        v["worktrees"] = serde_json::json!({ "removed": [], "kept": [], "error": error });
        human.push_str("; worktrees removed: 0, kept: 0");
        if matches!(cleanup, Cleanup::SkippedInWorktree) {
            human.push_str("\n  worktree cleanup skipped: run from inside a worktree");
        } else {
            human.push_str(&format!("\n  worktree cleanup failed: {error}"));
        }
    }
}

/// Takes a document task's delivery: `in_review -> accepted`, plus one
/// `task_accepted` event naming the run whose result.json was taken. Nothing
/// goes into the reviews table and no `verdict` event is written, so the
/// pass and reject counts of `thoth models` and records never see it (DOC-1).
/// Then sweeps finished tasks' worktrees, the same as `close`.
pub fn accept(paths: &Paths, cwd: &Path, needle: &str, reason: &str, json: bool) -> Result<i32> {
    // Same guard as `review record`: a worker must not take its own delivery.
    // Checked before the database is opened.
    if worktrees::inside_a_run() {
        bail!(
            "task accept refuses to run with THOTH_RUN_ID set: a delivery is accepted by the \
             supervising session, not by a run"
        );
    }
    let reason = reason.trim();
    if reason.is_empty() {
        bail!("task accept needs a non-empty --reason");
    }
    let (repo_id, root, conn) = open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;

    // One transaction for the gate, the write and the event, as in `close`.
    let tx = db::write_tx(&conn)?;
    let task = state::get_task(&tx, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    if task.status != TaskStatus::InReview.as_str() {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "task {task_id} is '{}'; task accept needs an in_review docs, research or review task",
            task.status
        );
    }
    let task_type = spec::labels(&task.spec).0;
    if !spec::is_document_task(&task.spec) {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "task {task_id} is a code task (task_type {}); task accept takes only docs, research \
             or review tasks. Review it with thoth review start and record the verdict with \
             thoth review record",
            task_type.as_deref().unwrap_or("not set")
        );
    }
    if let Some(live) = state::list_live_runs_for_task(&tx, &task_id)?.into_iter().next() {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "{}",
            crate::commands::live_run_refusal("task accept", &task_id, &live)
        );
    }
    let ingested = state::latest_event_of_type(&tx, event_type::RESULT_INGESTED, Some(&task_id))?;
    let run_id = ingested.as_ref().and_then(|e| e.run_id.clone());
    let result_status = ingested
        .as_ref()
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.detail).ok())
        .map(|d| d["result_json"]["status"].clone())
        .filter(|s| !s.is_null());
    let candidate_commit = match &run_id {
        Some(r) => state::get_run(&tx, r)?.and_then(|r| r.candidate_commit),
        None => None,
    };
    let now = ids::now_rfc3339();
    if !state::transition_task_status(&tx, &task_id, &[TaskStatus::InReview], TaskStatus::Accepted, &now)? {
        drop(tx); // explicit: rolls back, so nothing was written
        bail!(
            "task {task_id} changed status underneath task accept; \
             task accept needs an in_review docs, research or review task"
        );
    }
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: None,
            event_type: event_type::TASK_ACCEPTED.to_string(),
            detail: serde_json::json!({
                "from": task.status,
                "reason": reason,
                "task_type": task_type,
                "run_id": run_id,
                "result_status": result_status,
                "candidate_commit": candidate_commit,
            }),
        },
    )?;
    tx.commit()?;

    let cleanup = sweep_finished(paths, &repo_id, &root, &conn, cwd, worktrees::SOURCE_TASK_ACCEPT);
    let mut v = serde_json::json!({
        "ok": true,
        "task_id": task_id,
        "previous_status": task.status,
        "status": TaskStatus::Accepted.as_str(),
        "reason": reason,
        "run_id": run_id,
        "result_status": result_status,
        "candidate_commit": candidate_commit,
    });
    let mut human = format!("{task_id} accepted ({} -> accepted)", task.status);
    report_cleanup(&cleanup, &mut v, &mut human);
    output::emit(json, &v, &human);
    Ok(0)
}

/// Prints the newest result.json a worker of this task delivered (or of
/// `--run`), read from its `result_ingested` event. Read-only: it writes no
/// event, and it reads the same after the task is closed or accepted.
pub fn result(paths: &Paths, cwd: &Path, needle: &str, run: Option<&str>, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;
    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    let wanted_run = match run {
        Some(r) => {
            let run_id = ids::resolve_prefix(&state::list_run_ids(&conn)?, r)?;
            let row = state::get_run(&conn, &run_id)?
                .ok_or_else(|| anyhow::anyhow!("run {run_id} disappeared"))?;
            if row.task_id != task_id {
                bail!("run {run_id} belongs to task {}, not to task {task_id}", row.task_id);
            }
            Some(run_id)
        }
        None => None,
    };
    let ingested = state::events_since(&conn, 0, Some(&task_id))?
        .into_iter()
        .rev()
        .filter(|e| e.event_type == event_type::RESULT_INGESTED)
        .find(|e| wanted_run.is_none() || e.run_id == wanted_run);
    let Some(event) = ingested else {
        let whose = match &wanted_run {
            Some(r) => format!("run {r} of task {task_id}"),
            None => format!("task {task_id}"),
        };
        bail!(
            "task {task_id} is '{}'; {whose} has no ingested result.json. A refused one is a \
             result_invalid event: see thoth events --task {task_id} --json",
            task.status
        );
    };
    let detail: serde_json::Value =
        serde_json::from_str(&event.detail).unwrap_or(serde_json::Value::Null);
    let doc = &detail["result_json"];
    let summary = detail["summary"]
        .as_str()
        .or_else(|| doc["summary"].as_str())
        .unwrap_or("")
        .to_string();
    let claims: Vec<serde_json::Value> = doc["claims"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|c| {
                    serde_json::json!({
                        "kind": c["kind"],
                        "text": c["text"],
                        "evidence": c["evidence"].as_array().cloned().unwrap_or_default(),
                        "sources": c["sources"].as_array().cloned().unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let claim_count = detail["claim_count"].as_u64().unwrap_or(claims.len() as u64);
    let run_row = match &event.run_id {
        Some(r) => state::get_run(&conn, r)?,
        None => None,
    };
    let masker = crate::redact::Masker::load(Some(&paths.home));
    let brief =crate::brief::for_result_event(&masker, &detail);
    let mut v = serde_json::json!({
        "ok": true,
        "task_id": task_id,
        "task_status": task.status,
        "title": task.title,
        "run_id": event.run_id,
        "attempt": run_row.as_ref().map(|r| r.attempt),
        "role": run_row.as_ref().map(|r| r.role.clone()),
        "profile": run_row.as_ref().map(|r| r.account.clone()),
        "model": run_row.as_ref().map(|r| r.model.clone()),
        "candidate_commit": run_row.as_ref().and_then(|r| r.candidate_commit.clone()),
        "worktree": run_row.as_ref().map(|r| r.worktree.clone()),
        "ingested_at": event.ts,
        "result_status": doc["status"],
        "summary": summary,
        "claim_count": claim_count,
        "claims": claims,
    });
    crate::brief::insert_keys(&mut v, Some(&brief));

    let text = |v: &serde_json::Value| v.as_str().unwrap_or("-").to_string();
    let mut human = format!("{task_id}  {}  {}", task.status, task.title);
    human.push_str(&format!(
        "\nrun {}  attempt {}  {}  {}  {}  {}",
        event.run_id.as_deref().unwrap_or("-"),
        run_row.as_ref().map(|r| r.attempt.to_string()).unwrap_or_else(|| "-".into()),
        text(&v["role"]),
        text(&v["profile"]),
        text(&v["model"]),
        text(&v["candidate_commit"]),
    ));
    human.push('\n');
    human.push_str(&crate::brief::human_lines(
        &brief,
        &match &event.run_id {
            Some(r) => format!("thoth task result {task_id} --run {r}"),
            None => format!("thoth task result {task_id}"),
        },
    ));
    human.push_str("\nsummary:");
    for line in summary.lines() {
        human.push_str(&format!("\n  {line}"));
    }
    human.push_str(&format!("\nclaims ({}):", claims.len()));
    for c in &claims {
        let codes: Vec<String> = c["evidence"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|e| e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string()))
            .collect();
        human.push_str(&format!(
            "\n- {}: {} [{}]",
            text(&c["kind"]),
            text(&c["text"]),
            codes.join(", ")
        ));
        // RES-1a: a research claim's sources, one indented line each. RD-4:
        // the whole quote is masked before it is cut.
        for s in c["sources"].as_array().into_iter().flatten() {
            let quote = masker.mask_text(s["quote"].as_str().unwrap_or("")).0;
            human.push_str(&format!(
                "\n    {}  \"{}\"",
                text(&s["url"]),
                clip(&quote, SOURCE_QUOTE_CHARS)
            ));
        }
    }
    output::emit(json, &v, &human);
    Ok(0)
}

/// How much of a source's quote `task result` prints on its line.
const SOURCE_QUOTE_CHARS: usize = 120;

/// `s` cut to `max` characters plus `...` when longer, on a character boundary.
fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((cut, _)) => format!("{}...", &s[..cut]),
        None => s.to_string(),
    }
}

fn first_line(goal: &str) -> String {
    let line = goal.lines().next().unwrap_or("").trim();
    if line.chars().count() <= 72 {
        return line.to_string();
    }
    // Count characters, and cut on a character boundary via char_indices.
    // `&line[..69]` is a BYTE slice and panics whenever byte 69 lands inside a
    // multi-byte character, which a mixed-script goal hits easily.
    let cut = line
        .char_indices()
        .nth(69)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    format!("{}...", &line[..cut])
}

#[cfg(test)]
mod tests {
    use super::{clip, first_line};

    #[test]
    fn clip_keeps_short_text_and_cuts_long_text_on_a_character() {
        assert_eq!(clip("short", 120), "short");
        assert_eq!(clip(&"a".repeat(120), 120), "a".repeat(120));
        assert_eq!(clip(&"a".repeat(121), 120), format!("{}...", "a".repeat(120)));
        assert_eq!(clip(&"中".repeat(130), 120), format!("{}...", "中".repeat(120)));
    }

    #[test]
    fn first_line_takes_the_first_line_and_trims_it() {
        assert_eq!(first_line("  hello  \nworld"), "hello");
        assert_eq!(first_line(""), "");
        assert_eq!(first_line("short goal"), "short goal");
    }

    #[test]
    fn first_line_never_splits_a_multibyte_character() {
        // 37 x "中a" = 74 characters / 148 bytes. Byte 69 is the second byte of a
        // 3-byte character, so the old byte-slicing version panicked here.
        let goal = "中a".repeat(37);
        let out = first_line(&goal);
        assert!(out.ends_with("..."), "got {out}");
        assert_eq!(out.chars().count(), 72, "69 characters plus the ellipsis");
        assert!(goal.starts_with(out.trim_end_matches('.')));
    }

    #[test]
    fn update_spec_is_allowed_only_from_draft_queued_reject_or_unverified() {
        use crate::model::TaskStatus;
        let allowed: Vec<&str> = super::UPDATE_SPEC_STATUSES
            .iter()
            .map(|s| s.as_str())
            .collect();
        assert_eq!(allowed, vec!["draft", "queued", "reject", "unverified"]);
        // a running task has a worker reading the old spec; a decided one is done
        for no in [
            TaskStatus::Running,
            TaskStatus::InReview,
            TaskStatus::Pass,
            TaskStatus::Cancelled,
            TaskStatus::Accepted,
        ] {
            assert!(
                !super::UPDATE_SPEC_STATUSES.contains(&no),
                "{} must not be editable",
                no.as_str()
            );
        }
    }
}
