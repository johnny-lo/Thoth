//! WT-1: Thoth removes the worker, review and spec review worktrees it no
//! longer needs.
//!
//! A task that is `pass`, `cancelled` or `accepted` will never be dispatched,
//! reviewed or resumed again, so its worktree directories (several GB each once cargo has
//! built in them) only take space. Its commits and its evidence (blobs,
//! `runs/`, state.db) are what the commander still needs, and none of them is
//! touched here: a directory goes only through `git worktree remove --force`,
//! only after this module's own checks said it is safe, and only after every
//! directory link inside it was removed without following it. Every other
//! status keeps its worktrees, because a rework or a resume continues in them.
//!
//! The sweeps of `task create` and `worktrees prune` then delete each local
//! branch `thoth/TASK/aN` of those tasks whose work is already in the task's
//! base branch: `git branch -d` when the tip is an ancestor of the base, or
//! `git branch -D` when `git cherry` shows every commit already applied and
//! there is no merge commit. Anything else keeps the branch, with a reason.
//!
//! One function, `sweep`, behind five fixed call sites (the runner when a task
//! settles, `review record` after a PASS, `task close`, `task accept`,
//! `task create`) and the manual `thoth worktrees prune`.

use crate::config::Paths;
use crate::db::state::{self, NewEvent};
use crate::model::{event_type, role, TaskStatus};
use crate::worktree;
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::path::Path;

pub(crate) const SOURCE_RUNNER: &str = "runner";
pub(crate) const SOURCE_REVIEW_RECORD: &str = "review_record";
pub(crate) const SOURCE_TASK_CLOSE: &str = "task_close";
pub(crate) const SOURCE_TASK_ACCEPT: &str = "task_accept";
pub(crate) const SOURCE_TASK_CREATE: &str = "task_create";
pub(crate) const SOURCE_PRUNE: &str = "worktrees_prune";

/// Why a candidate directory was kept (`worktree_kept.detail.reason`).
const OUTSIDE_WORKTREES_DIR: &str = "outside_worktrees_dir";
const UNCOMMITTED_CHANGES: &str = "uncommitted_changes";
const GIT_ERROR: &str = "git_error";
const UNREACHABLE_HEAD: &str = "unreachable_head";
const REMOVE_FAILED: &str = "remove_failed";

/// Why a task branch was kept (`branch_kept.detail.reason`).
const BRANCH_CHECKED_OUT: &str = "branch_checked_out";
const NO_BASE_BRANCH: &str = "no_base_branch";
const BRANCH_UNMERGED: &str = "branch_unmerged";
const BRANCH_DELETE_FAILED: &str = "branch_delete_failed";
/// The branch is another task's base branch (only reachable when that task
/// was created from inside this task's worktree).
const BRANCH_IS_A_BASE: &str = "branch_is_a_base";

/// How a deleted branch's work was found in the base (`merged_by`).
const MERGED_BY_ANCESTOR: &str = "ancestor";
const MERGED_BY_CHERRY: &str = "cherry";

/// How many changed lines a `worktree_kept` detail carries before it says
/// how many more there were.
const MAX_DETAIL_LINES: usize = 20;

/// What one sweep looks at.
pub(crate) enum Scope<'a> {
    /// One task, and only its worker worktrees (implementer runs) and its
    /// spec review worktree (SR-2).
    Task(&'a str),
    /// Every task of the repo, worker and review worktrees alike.
    All,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Removed {
    pub task_id: String,
    /// `runs.worktree` exactly as stored.
    pub path: String,
    /// `worker`, `review` or `spec_review`.
    pub kind: String,
    pub head: String,
    pub branch: Option<String>,
    pub task_status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kept {
    pub task_id: String,
    pub path: String,
    pub kind: String,
    pub reason: String,
    pub detail: String,
    pub task_status: String,
}

/// A local task branch the sweep deleted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemovedBranch {
    pub task_id: String,
    pub branch: String,
    pub tip: String,
    /// `ancestor` or `cherry`.
    pub merged_by: &'static str,
}

/// A local task branch the sweep kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KeptBranch {
    pub task_id: String,
    pub branch: String,
    pub reason: &'static str,
    /// Only for a failure: the git error, or why a check was inconclusive.
    pub detail: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct Sweep {
    pub removed: Vec<Removed>,
    pub kept: Vec<Kept>,
    /// Finished tasks with no live run, i.e. the ones whose worktrees were looked at.
    pub tasks_checked: usize,
    /// Only from `worktrees prune` and `task create`; empty otherwise.
    pub removed_branches: Vec<RemovedBranch>,
    pub kept_branches: Vec<KeptBranch>,
}

/// `review` for a reviewer run's worktree, `spec_review` for a spec reviewer
/// run's (SR-2), `worker` for everything else.
pub(crate) fn kind_of(run_role: &str) -> &'static str {
    if run_role == role::REVIEWER {
        "review"
    } else if run_role == role::SPEC_REVIEWER {
        "spec_review"
    } else {
        "worker"
    }
}

/// The lines of `git status --porcelain` that are not Thoth's own `.agent/` or
/// a harness's `.agents/`, in git's order. Each line is two status characters
/// and a space, then the path (quoted by git when it holds unusual characters).
pub(crate) fn non_agent_changes(porcelain: &str) -> Vec<String> {
    porcelain
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter(|l| {
            let path = l.get(3..).unwrap_or("");
            let path = path.strip_prefix('"').unwrap_or(path);
            !(path.starts_with(".agent/") || path.starts_with(".agents/"))
        })
        .map(|l| l.to_string())
        .collect()
}

/// The changed lines for a `worktree_kept` detail: at most twenty, then one
/// `... and N more` line.
pub(crate) fn kept_detail(lines: &[String]) -> String {
    if lines.len() <= MAX_DETAIL_LINES {
        return lines.join("\n");
    }
    let mut shown: Vec<String> = lines[..MAX_DETAIL_LINES].to_vec();
    shown.push(format!("... and {} more", lines.len() - MAX_DETAIL_LINES));
    shown.join("\n")
}

fn is_finished(status: &str) -> bool {
    matches!(
        TaskStatus::parse(status),
        Ok(TaskStatus::Pass | TaskStatus::Cancelled | TaskStatus::Accepted)
    )
}

/// What happened to one candidate directory.
enum Outcome {
    Removed { head: String, branch: Option<String> },
    Kept { reason: &'static str, detail: String },
    /// Already gone (another sweep removed it first): nothing to report.
    Gone,
}

/// The checks of WT-1 AC2 (d)-(g) for one directory that exists, in order.
/// Only a directory that passes all of them reaches `git worktree remove`.
fn check_and_remove(paths: &Paths, repo_id: &str, repo_root: &Path, raw: &str) -> Outcome {
    let dir = Path::new(raw);
    let inside = match (
        std::fs::canonicalize(dir),
        std::fs::canonicalize(paths.worktrees_dir(repo_id)),
    ) {
        (Ok(d), Ok(root)) => d != root && d.starts_with(&root),
        _ => false,
    };
    if !inside {
        return Outcome::Kept { reason: OUTSIDE_WORKTREES_DIR, detail: raw.to_string() };
    }
    let changes = match worktree::status_porcelain(dir) {
        Ok(p) => non_agent_changes(&p),
        Err(e) => return Outcome::Kept { reason: GIT_ERROR, detail: format!("{e:#}") },
    };
    if !changes.is_empty() {
        return Outcome::Kept { reason: UNCOMMITTED_CHANGES, detail: kept_detail(&changes) };
    }
    let head = match worktree::rev_parse_head(dir) {
        Ok(h) => h,
        Err(e) => return Outcome::Kept { reason: GIT_ERROR, detail: format!("{e:#}") },
    };
    match worktree::branches_containing(repo_root, &head) {
        Ok(b) if b.is_empty() => {
            return Outcome::Kept { reason: UNREACHABLE_HEAD, detail: head };
        }
        Ok(_) => {}
        Err(e) => return Outcome::Kept { reason: GIT_ERROR, detail: format!("{e:#}") },
    }
    let branch = worktree::head_branch(dir).ok().flatten();
    match worktree::remove_worktree_force(repo_root, dir) {
        Ok(()) => Outcome::Removed { head, branch },
        Err(e) => match e.downcast_ref::<worktree::LinkCleanupError>() {
            Some(link) => Outcome::Kept { reason: worktree::LINK_UNREMOVABLE, detail: link.detail() },
            None if !dir.exists() => Outcome::Gone,
            None => Outcome::Kept { reason: REMOVE_FAILED, detail: format!("{e:#}") },
        },
    }
}

/// The task's `base_branch`, from its newest `task_created` event.
fn base_branch_of(conn: &Connection, task_id: &str) -> Result<Option<String>> {
    Ok(state::latest_event_of_type(conn, event_type::TASK_CREATED, Some(task_id))?
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.detail).ok())
        .and_then(|d| d["base_branch"].as_str().map(str::to_string))
        .filter(|b| !b.trim().is_empty()))
}

/// Whether `branch` is exactly `thoth/TASK/aN` for this task (N digits only).
fn is_task_branch(task_id: &str, branch: &str) -> bool {
    branch
        .strip_prefix(&format!("thoth/{task_id}/a"))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// What happened to one task branch.
enum BranchOutcome {
    Deleted { tip: String, merged_by: &'static str },
    Kept { reason: &'static str, detail: Option<String> },
    /// Disappeared between listing and checking: nothing to report.
    Gone,
}

fn unmerged(detail: impl Into<String>) -> BranchOutcome {
    BranchOutcome::Kept { reason: BRANCH_UNMERGED, detail: Some(detail.into()) }
}

/// The checks of AC1 for one task branch, in order, then the deletion.
fn judge_branch(
    repo_root: &Path,
    branch: &str,
    base: Option<&str>,
    checked_out: &std::result::Result<Vec<String>, String>,
    other_bases: &[String],
) -> BranchOutcome {
    if other_bases.iter().any(|b| b == branch) {
        return BranchOutcome::Kept { reason: BRANCH_IS_A_BASE, detail: None };
    }
    match checked_out {
        Ok(list) if list.iter().any(|b| b == branch) => {
            return BranchOutcome::Kept { reason: BRANCH_CHECKED_OUT, detail: None };
        }
        Ok(_) => {}
        Err(e) => {
            return BranchOutcome::Kept {
                reason: BRANCH_CHECKED_OUT,
                detail: Some(format!("could not list the worktrees: {e}")),
            };
        }
    }
    let Some(base) = base else {
        return BranchOutcome::Kept { reason: NO_BASE_BRANCH, detail: None };
    };
    let base_ref = format!("refs/heads/{base}");
    match worktree::ref_commit(repo_root, &base_ref) {
        Ok(Some(_)) => {}
        Ok(None) => return BranchOutcome::Kept { reason: NO_BASE_BRANCH, detail: None },
        Err(e) => {
            return BranchOutcome::Kept { reason: NO_BASE_BRANCH, detail: Some(format!("{e:#}")) };
        }
    }
    let branch_ref = format!("refs/heads/{branch}");
    let tip = match worktree::ref_commit(repo_root, &branch_ref) {
        Ok(Some(t)) => t,
        Ok(None) => return BranchOutcome::Gone,
        Err(e) => return unmerged(format!("{e:#}")),
    };
    let merged_by = match worktree::is_ancestor(repo_root, &tip, &base_ref) {
        Ok(true) => MERGED_BY_ANCESTOR,
        Ok(false) => {
            match worktree::merges_between(repo_root, &base_ref, &branch_ref) {
                Ok(m) if m.is_empty() => {}
                Ok(_) => return BranchOutcome::Kept { reason: BRANCH_UNMERGED, detail: None },
                Err(e) => return unmerged(format!("{e:#}")),
            }
            let out = match worktree::cherry(repo_root, &base_ref, &branch_ref) {
                Ok(o) => o,
                Err(e) => return unmerged(format!("{e:#}")),
            };
            let lines: Vec<&str> = out.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.is_empty() {
                return unmerged(format!(
                    "git cherry {base_ref} {branch_ref} listed no commit although the tip is not \
                     an ancestor of the base"
                ));
            }
            if !lines.iter().all(|l| l.starts_with('-')) {
                return BranchOutcome::Kept { reason: BRANCH_UNMERGED, detail: None };
            }
            MERGED_BY_CHERRY
        }
        Err(e) => return unmerged(format!("{e:#}")),
    };
    let force = merged_by == MERGED_BY_CHERRY;
    match worktree::delete_branch(repo_root, branch, base, force) {
        Ok(()) => BranchOutcome::Deleted { tip, merged_by },
        Err(e) => BranchOutcome::Kept { reason: BRANCH_DELETE_FAILED, detail: Some(format!("{e:#}")) },
    }
}

/// Whether the newest `branch_kept`/`branch_deleted` event for this task's
/// `branch` is already a `branch_kept` with this `reason`.
fn branch_already_kept(conn: &Connection, task_id: &str, branch: &str, reason: &str) -> Result<bool> {
    let events = state::events_since(conn, 0, Some(task_id))?;
    for e in events.iter().rev() {
        if e.event_type != event_type::BRANCH_KEPT && e.event_type != event_type::BRANCH_DELETED {
            continue;
        }
        let detail: serde_json::Value =
            serde_json::from_str(&e.detail).unwrap_or(serde_json::Value::Null);
        if detail["branch"].as_str() != Some(branch) {
            continue;
        }
        return Ok(e.event_type == event_type::BRANCH_KEPT
            && detail["reason"].as_str() == Some(reason));
    }
    Ok(false)
}

/// The branch half of a full sweep: every local `thoth/TASK/aN` of each
/// swept task, after the worktrees were handled and git's registrations of
/// gone directories were pruned.
fn sweep_branches(
    repo_root: &Path,
    conn: &Connection,
    tasks: &[state::Task],
    source: &str,
    out: &mut Sweep,
) -> Result<()> {
    let checked_out = worktree::checked_out_branches(repo_root).map_err(|e| format!("{e:#}"));
    let mut bases: Vec<(String, Option<String>)> = Vec::new();
    for t in state::list_tasks(conn)? {
        let b = base_branch_of(conn, &t.task_id)?;
        bases.push((t.task_id, b));
    }
    for task in tasks {
        let own_base = bases
            .iter()
            .find(|(id, _)| *id == task.task_id)
            .and_then(|(_, b)| b.clone());
        let other_bases: Vec<String> = bases
            .iter()
            .filter(|(id, _)| *id != task.task_id)
            .filter_map(|(_, b)| b.clone())
            .collect();
        // A branch that cannot be listed cannot be named in a report; the
        // next sweep tries again.
        let Ok(branches) = worktree::local_branches_under(repo_root, &format!("thoth/{}/", task.task_id))
        else {
            continue;
        };
        for branch in branches {
            if !is_task_branch(&task.task_id, &branch) || own_base.as_deref() == Some(branch.as_str()) {
                continue;
            }
            match judge_branch(repo_root, &branch, own_base.as_deref(), &checked_out, &other_bases) {
                BranchOutcome::Gone => {}
                BranchOutcome::Deleted { tip, merged_by } => {
                    state::append_event(
                        conn,
                        &NewEvent {
                            task_id: Some(task.task_id.clone()),
                            run_id: None,
                            event_type: event_type::BRANCH_DELETED.to_string(),
                            detail: serde_json::json!({
                                "task_id": task.task_id,
                                "branch": branch,
                                "tip": tip,
                                "merged_by": merged_by,
                                "task_status": task.status,
                                "source": source,
                            }),
                        },
                    )?;
                    out.removed_branches.push(RemovedBranch {
                        task_id: task.task_id.clone(),
                        branch,
                        tip,
                        merged_by,
                    });
                }
                BranchOutcome::Kept { reason, detail } => {
                    if !branch_already_kept(conn, &task.task_id, &branch, reason)? {
                        let mut d = serde_json::json!({
                            "task_id": task.task_id,
                            "branch": branch,
                            "reason": reason,
                            "task_status": task.status,
                            "source": source,
                        });
                        if let Some(detail) = &detail {
                            d["detail"] = serde_json::json!(detail);
                        }
                        state::append_event(
                            conn,
                            &NewEvent {
                                task_id: Some(task.task_id.clone()),
                                run_id: None,
                                event_type: event_type::BRANCH_KEPT.to_string(),
                                detail: d,
                            },
                        )?;
                    }
                    out.kept_branches.push(KeptBranch {
                        task_id: task.task_id.clone(),
                        branch,
                        reason,
                        detail,
                    });
                }
            }
        }
    }
    Ok(())
}

/// Whether the newest `worktree_kept`/`worktree_removed` event for `path` is
/// already a `worktree_kept` with this `reason`: `task create` sweeps on every
/// new task, and a dirty directory must not add an event each time.
fn already_kept(conn: &Connection, task_id: &str, path: &str, reason: &str) -> Result<bool> {
    let events = state::events_since(conn, 0, Some(task_id))?;
    for e in events.iter().rev() {
        if e.event_type != event_type::WORKTREE_KEPT && e.event_type != event_type::WORKTREE_REMOVED {
            continue;
        }
        let detail: serde_json::Value =
            serde_json::from_str(&e.detail).unwrap_or(serde_json::Value::Null);
        if detail["path"].as_str() != Some(path) {
            continue;
        }
        return Ok(e.event_type == event_type::WORKTREE_KEPT
            && detail["reason"].as_str() == Some(reason));
    }
    Ok(false)
}

/// Removes the clean worktrees of `pass`/`cancelled`/`accepted` tasks that have no live
/// run, and reports the ones it kept (WT-1 AC2). Only a state.db read or
/// event write fails the sweep; a git problem with one directory keeps that
/// directory and the sweep goes on.
pub(crate) fn sweep(
    paths: &Paths,
    repo_id: &str,
    repo_root: &Path,
    conn: &Connection,
    scope: Scope<'_>,
    source: &str,
) -> Result<Sweep> {
    let mut out = Sweep::default();
    let (tasks, worker_only) = match scope {
        Scope::All => (
            state::list_tasks(conn)?
                .into_iter()
                .filter(|t| is_finished(&t.status))
                .collect::<Vec<_>>(),
            false,
        ),
        Scope::Task(id) => match state::get_task(conn, id)? {
            Some(t) if is_finished(&t.status) => (vec![t], true),
            _ => return Ok(out),
        },
    };
    let mut swept = Vec::new();
    for task in tasks {
        // Defensive: a finished task has no live run in any supported flow.
        if !state::list_live_runs_for_task(conn, &task.task_id)?.is_empty() {
            continue;
        }
        out.tasks_checked += 1;
        swept.push(task.clone());
        let mut candidates: Vec<(String, &'static str)> = Vec::new();
        for run in state::list_runs_for_task(conn, &task.task_id)? {
            if !candidates.iter().any(|(p, _)| *p == run.worktree) {
                candidates.push((run.worktree.clone(), kind_of(&run.role)));
            }
        }
        for (path, kind) in candidates {
            if worker_only && kind == "review" {
                continue;
            }
            if path.is_empty() || !Path::new(&path).is_dir() {
                continue;
            }
            match check_and_remove(paths, repo_id, repo_root, &path) {
                Outcome::Gone => {}
                Outcome::Removed { head, branch } => {
                    state::append_event(
                        conn,
                        &NewEvent {
                            task_id: Some(task.task_id.clone()),
                            run_id: None,
                            event_type: event_type::WORKTREE_REMOVED.to_string(),
                            detail: serde_json::json!({
                                "task_id": task.task_id,
                                "path": path,
                                "kind": kind,
                                "head": head,
                                "branch": branch,
                                "task_status": task.status,
                                "source": source,
                            }),
                        },
                    )?;
                    out.removed.push(Removed {
                        task_id: task.task_id.clone(),
                        path,
                        kind: kind.to_string(),
                        head,
                        branch,
                        task_status: task.status.clone(),
                    });
                }
                Outcome::Kept { reason, detail } => {
                    if !already_kept(conn, &task.task_id, &path, reason)? {
                        state::append_event(
                            conn,
                            &NewEvent {
                                task_id: Some(task.task_id.clone()),
                                run_id: None,
                                event_type: event_type::WORKTREE_KEPT.to_string(),
                                detail: serde_json::json!({
                                    "task_id": task.task_id,
                                    "path": path,
                                    "kind": kind,
                                    "reason": reason,
                                    "detail": detail,
                                    "task_status": task.status,
                                    "source": source,
                                }),
                            },
                        )?;
                    }
                    out.kept.push(Kept {
                        task_id: task.task_id.clone(),
                        path,
                        kind: kind.to_string(),
                        reason: reason.to_string(),
                        detail,
                        task_status: task.status.clone(),
                    });
                }
            }
        }
    }
    // Lets git forget registrations whose directory is already gone, so a
    // branch whose worktree was deleted by hand no longer counts as checked out.
    worktree::prune_worktrees(repo_root);
    // Branches only in the two sweeps that report or record them as the
    // commander's cleanup step (`worktrees prune`, `task create`); `task
    // close` and `task accept` keep their `worktrees` output as it was, and
    // the next of those two sweeps handles the branch.
    if !worker_only && (source == SOURCE_PRUNE || source == SOURCE_TASK_CREATE) {
        sweep_branches(repo_root, conn, &swept, source, &mut out)?;
    }
    Ok(out)
}

/// One `removed_branches[]` element of `worktrees prune --json`.
pub(crate) fn removed_branch_json(r: &RemovedBranch) -> serde_json::Value {
    serde_json::json!({
        "task_id": r.task_id,
        "branch": r.branch,
        "tip": r.tip,
        "merged_by": r.merged_by,
    })
}

/// One `kept_branches[]` element of `worktrees prune --json`; `detail` only
/// for a failure.
pub(crate) fn kept_branch_json(k: &KeptBranch) -> serde_json::Value {
    let mut v = serde_json::json!({
        "task_id": k.task_id,
        "branch": k.branch,
        "reason": k.reason,
    });
    if let Some(d) = &k.detail {
        v["detail"] = serde_json::json!(d);
    }
    v
}

/// One `removed[]` element of `task close --json` and `worktrees prune --json`.
pub(crate) fn removed_json(r: &Removed) -> serde_json::Value {
    serde_json::json!({
        "task_id": r.task_id,
        "path": r.path,
        "kind": r.kind,
        "head": r.head,
        "branch": r.branch,
        "task_status": r.task_status,
    })
}

/// One `kept[]` element of `task close --json` and `worktrees prune --json`.
pub(crate) fn kept_json(k: &Kept) -> serde_json::Value {
    serde_json::json!({
        "task_id": k.task_id,
        "path": k.path,
        "kind": k.kind,
        "reason": k.reason,
        "detail": k.detail,
        "task_status": k.task_status,
    })
}

/// The manual-removal hint for a directory kept for uncommitted changes.
pub(crate) fn is_uncommitted(k: &Kept) -> bool {
    k.reason == UNCOMMITTED_CHANGES
}

/// Whether `cwd` is inside this repo's worktrees directory. A sweep started
/// from there could remove the shell's own current directory. `false` when
/// the worktrees directory does not exist yet.
pub(crate) fn cwd_inside_worktrees(paths: &Paths, repo_id: &str, cwd: &Path) -> bool {
    match (
        std::fs::canonicalize(paths.worktrees_dir(repo_id)),
        std::fs::canonicalize(cwd),
    ) {
        (Ok(root), Ok(c)) => c.starts_with(&root),
        _ => false,
    }
}

/// Whether this process runs inside a Thoth run (a worker or a reviewer),
/// which must never remove other tasks' worktrees.
pub(crate) fn inside_a_run() -> bool {
    std::env::var_os("THOTH_RUN_ID").is_some()
}

/// `thoth worktrees prune`: sweep every finished task of the repo and list
/// what was removed and what was kept (WT-1 AC4).
pub fn prune(paths: &Paths, cwd: &Path, json: bool) -> Result<i32> {
    if inside_a_run() {
        bail!(
            "worktrees prune refuses to run with THOTH_RUN_ID set: a run must not remove \
             other tasks' worktrees"
        );
    }
    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    if cwd_inside_worktrees(paths, &repo_id, cwd) {
        bail!(
            "run worktrees prune from the main working tree, not from inside a worktree under {}",
            paths.worktrees_dir(&repo_id).display()
        );
    }
    let s = sweep(paths, &repo_id, &root, &conn, Scope::All, SOURCE_PRUNE)?;
    let ok = !s
        .kept
        .iter()
        .any(|k| k.reason == REMOVE_FAILED || k.reason == GIT_ERROR);
    let v = serde_json::json!({
        "ok": ok,
        "removed": s.removed.iter().map(removed_json).collect::<Vec<_>>(),
        "kept": s.kept.iter().map(kept_json).collect::<Vec<_>>(),
        "tasks_checked": s.tasks_checked,
        "removed_branches": s.removed_branches.iter().map(removed_branch_json).collect::<Vec<_>>(),
        "kept_branches": s.kept_branches.iter().map(kept_branch_json).collect::<Vec<_>>(),
    });
    let mut human = format!(
        "removed {} worktree(s), kept {} (checked {} finished task(s)); deleted {} branch(es), kept {}",
        s.removed.len(),
        s.kept.len(),
        s.tasks_checked,
        s.removed_branches.len(),
        s.kept_branches.len()
    );
    for r in &s.removed {
        let branch = match &r.branch {
            Some(b) => format!("on branch {b}"),
            None => format!("detached at {}", r.head.get(..7).unwrap_or(&r.head)),
        };
        human.push_str(&format!(
            "\n  removed  {}  {}  (task {}, {branch})",
            r.kind, r.path, r.task_id
        ));
    }
    for k in &s.kept {
        let tail = if is_uncommitted(k) {
            format!(
                "commit or discard them, or run git worktree remove --force {}",
                k.path
            )
        } else {
            k.detail.lines().next().unwrap_or("").to_string()
        };
        human.push_str(&format!(
            "\n  kept     {}  {}  (task {}): {} - {tail}",
            k.kind, k.path, k.task_id, k.reason
        ));
    }
    for r in &s.removed_branches {
        human.push_str(&format!(
            "\n  deleted  branch  {}  (task {}, was {}, merged by {})",
            r.branch,
            r.task_id,
            r.tip.get(..7).unwrap_or(&r.tip),
            r.merged_by
        ));
    }
    for k in &s.kept_branches {
        let tail = match &k.detail {
            Some(d) => format!(" - {}", d.lines().next().unwrap_or("")),
            None => String::new(),
        };
        human.push_str(&format!(
            "\n  kept     branch  {}  (task {}): {}{tail}",
            k.branch, k.task_id, k.reason
        ));
    }
    crate::output::emit(json, &v, &human);
    Ok(if ok { 0 } else { 1 })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_agent_changes_ignores_only_the_agent_dirs() {
        let input = "?? .agent/\n?? .agents/hooks.json\n M src/a.rs\n?? \"we ird\".txt\n?? .agentx/\nD  gone.rs\n";
        assert_eq!(
            non_agent_changes(input),
            vec![
                " M src/a.rs".to_string(),
                "?? \"we ird\".txt".to_string(),
                "?? .agentx/".to_string(),
                "D  gone.rs".to_string(),
            ]
        );
        assert!(non_agent_changes("").is_empty());
        assert!(non_agent_changes("?? .agent/\n?? .agents/\n").is_empty());
    }

    #[test]
    fn kind_of_role_is_review_only_for_the_reviewer() {
        assert_eq!(kind_of("reviewer"), "review");
        assert_eq!(kind_of("implementer"), "worker");
        assert_eq!(kind_of("anything"), "worker");
        assert_eq!(kind_of("spec_reviewer"), "spec_review");
    }

    #[test]
    fn truncate_detail_keeps_twenty_lines() {
        let lines: Vec<String> = (1..=25).map(|i| format!("?? file{i}.txt")).collect();
        let out = kept_detail(&lines);
        let out_lines: Vec<&str> = out.lines().collect();
        assert_eq!(out_lines.len(), 21, "{out}");
        assert_eq!(out_lines[0], "?? file1.txt");
        assert_eq!(out_lines[19], "?? file20.txt");
        assert_eq!(out_lines[20], "... and 5 more");
        assert_eq!(kept_detail(&lines[..3]), "?? file1.txt\n?? file2.txt\n?? file3.txt");
    }
}
