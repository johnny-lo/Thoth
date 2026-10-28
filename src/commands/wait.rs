use crate::config::Paths;
use crate::db::state;
use crate::ids;
use crate::model::TaskStatus;
use crate::output;
use anyhow::Result;
use std::path::Path;
use std::time::{Duration, Instant};

/// Exit code when the wait times out. Distinct from 1 (a real error) so a
/// Supervisor script can tell "still running" from "something broke".
pub const TIMEOUT_EXIT_CODE: i32 = 3;

pub fn run(paths: &Paths, cwd: &Path, task: &str, timeout_seconds: u64, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, task)?;
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);

    loop {
        let t = state::get_task(&conn, &task_id)?
            .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
        let reached = TaskStatus::parse(&t.status)
            .map(|s| s.is_wait_terminal())
            .unwrap_or(false);
        if reached {
            output::emit(
                json,
                &serde_json::json!({
                    "ok": true,
                    "task_id": task_id,
                    "status": t.status,
                    "timed_out": false,
                }),
                &format!("{task_id} {}", t.status),
            );
            return Ok(0);
        }
        if Instant::now() >= deadline {
            output::emit(
                json,
                &serde_json::json!({
                    "ok": false,
                    "task_id": task_id,
                    "status": t.status,
                    "timed_out": true,
                }),
                &format!("timed out waiting for {task_id}; still {}", t.status),
            );
            return Ok(TIMEOUT_EXIT_CODE);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// AGY-1 (AC2): the `tools_unknown` of a run's newest `run_usage` event:
/// the agy tool names its tool lock denied that Thoth does not know. `[]`
/// when the run has no `run_usage` yet (still running, crashed before its
/// result), when the event has no such key (a Claude or codex run, or one
/// written before AGY-1), or when the key is not an array of strings. Only
/// this run's own events count, and only its newest usage: a resume's
/// usage replaces the interrupted launch's.
fn tools_unknown_for_run(conn: &rusqlite::Connection, task_id: &str, run_id: &str) -> Vec<String> {
    let newest = state::events_since(conn, 0, Some(task_id))
        .unwrap_or_default()
        .into_iter()
        .rev()
        .find(|e| e.run_id.as_deref() == Some(run_id) && e.event_type == crate::model::event_type::RUN_USAGE)
        .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.detail).ok());
    tools_unknown_of(newest.as_ref())
}

/// `detail.tools_unknown` of one `run_usage` detail as a list of strings;
/// `[]` for no detail, no key, or anything but an array whose every
/// element is a string.
fn tools_unknown_of(detail: Option<&serde_json::Value>) -> Vec<String> {
    detail
        .and_then(|d| d.get("tools_unknown"))
        .and_then(|v| v.as_array())
        .and_then(|a| a.iter().map(|t| t.as_str().map(String::from)).collect::<Option<Vec<String>>>())
        .unwrap_or_default()
}

/// AGY-1 (AC2): the line a human sees under `wait --run`'s first line when
/// `tools_unknown` is not empty; `None` when it is.
fn tools_unknown_line(tools_unknown: &[String]) -> Option<String> {
    if tools_unknown.is_empty() {
        return None;
    }
    Some(format!(
        "agy called tools Thoth does not know: {}; its tool names may have changed (AGY-1)",
        tools_unknown.join(", ")
    ))
}

/// Open question 6's ruling: block on one **run**'s terminal status.
///
/// `TaskStatus::is_wait_terminal` deliberately excludes `pass` and `reject`, so
/// the task-level `wait` returns `in_review` the moment `review start` does and
/// can never carry a verdict. A reviewer *run*, though, does reach a terminal
/// status, so this is the handle: `thoth review start --json` hands back the
/// reviewer `run_id` and the Supervisor chains `thoth wait --run <id>`.
///
/// Uses `RunStatus::is_terminal` (commit 6153a9b consolidated that predicate;
/// `runner::is_run_status_terminal` already defers to it) rather than spelling
/// the set out a third time.
///
/// SR-2: for a spec reviewer run, `verdict` is its spec review verdict
/// (`APPROVE` or `REVISE`), read from the `spec_review` event it wrote.
pub fn for_run(
    paths: &Paths,
    cwd: &Path,
    needle: &str,
    timeout_seconds: u64,
    json: bool,
) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let run_id = ids::resolve_prefix(&state::list_run_ids(&conn)?, needle)?;
    let deadline = Instant::now() + Duration::from_secs(timeout_seconds);

    loop {
        let r = state::get_run(&conn, &run_id)?
            .ok_or_else(|| anyhow::anyhow!("run {run_id} disappeared"))?;
        let reached = crate::model::RunStatus::parse(&r.status)
            .map(|s| s.is_terminal())
            .unwrap_or(false);
        if reached {
            // For a reviewer run the answer the caller wants is the verdict,
            // not the process outcome - a reviewer that REJECTs still exits 0.
            let mut verdict = state::get_review_for_run(&conn, &run_id)?.map(|rev| rev.verdict);
            if verdict.is_none() && r.role == crate::model::role::SPEC_REVIEWER {
                verdict = crate::commands::spec_review::verdict_for_run(&conn, &run_id)?;
            }
            let task_status = state::get_task(&conn, &r.task_id)?.map(|t| t.status);
            let tools_unknown = tools_unknown_for_run(&conn, &r.task_id, &run_id);
            let mut human = format!(
                "{run_id} {}{}{}",
                r.status,
                verdict
                    .as_ref()
                    .map(|v| format!("  verdict {v}"))
                    .unwrap_or_default(),
                task_status
                    .as_ref()
                    .map(|t| format!("  (task {t})"))
                    .unwrap_or_default()
            );
            if let Some(line) = tools_unknown_line(&tools_unknown) {
                human.push('\n');
                human.push_str(&line);
            }
            output::emit(
                json,
                &serde_json::json!({
                    "ok": true,
                    "run_id": run_id,
                    "task_id": r.task_id,
                    "role": r.role,
                    "status": r.status,
                    "interrupted_reason": r.interrupted_reason,
                    "exit_code": r.exit_code,
                    "verdict": verdict,
                    "task_status": task_status,
                    "timed_out": false,
                    "tools_unknown": tools_unknown,
                }),
                &human,
            );
            return Ok(0);
        }
        if Instant::now() >= deadline {
            // AGY-1: the same additive key on the timeout branch, from the
            // run's newest usage so far (a resume that has not written one
            // yet still shows the previous launch's).
            let tools_unknown = tools_unknown_for_run(&conn, &r.task_id, &run_id);
            let mut human = format!("timed out waiting for run {run_id}; still {}", r.status);
            if let Some(line) = tools_unknown_line(&tools_unknown) {
                human.push('\n');
                human.push_str(&line);
            }
            output::emit(
                json,
                // Whole-branch review Minor 1: the same key set as the
                // terminal branch above, which is what `docs/phase2/usage.md`
                // promises unconditionally. A script that reads
                // `v["task_status"]` should not have to know which branch
                // produced the document. The three extra keys are `null`
                // rather than the run's current values on purpose: this run
                // has not finished, so it has no `interrupted_reason` or
                // `exit_code` yet, and reporting a `task_status` here would
                // read like a settled answer.
                &serde_json::json!({
                    "ok": false,
                    "run_id": run_id,
                    "task_id": r.task_id,
                    "role": r.role,
                    "status": r.status,
                    "interrupted_reason": serde_json::Value::Null,
                    "exit_code": serde_json::Value::Null,
                    "verdict": serde_json::Value::Null,
                    "task_status": serde_json::Value::Null,
                    "timed_out": true,
                    "tools_unknown": tools_unknown,
                }),
                &human,
            );
            return Ok(TIMEOUT_EXIT_CODE);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}
