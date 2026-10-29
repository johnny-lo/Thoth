use crate::brief::{self, Brief};
use crate::config::Paths;
use crate::db::state;
use crate::ids;
use crate::model::{event_type, TaskStatus};
use crate::output;
use crate::redact::Masker;
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
            // BRIEF-1: the task's last implementer run, by run id, and what
            // its own newest delivery says; another run's result never
            // stands in for it.
            let run_id = state::list_runs_for_task(&conn, &task_id)?
                .into_iter()
                .filter(|r| r.role == crate::model::role::IMPLEMENTER)
                .map(|r| r.run_id)
                .max();
            let delivery = match &run_id {
                Some(r) => Delivery::of_run(&conn, &task_id, r)?,
                None => Delivery::default(),
            };
            let mut v = serde_json::json!({
                "ok": true,
                "task_id": task_id,
                "status": t.status,
                "timed_out": false,
                "run_id": run_id,
            });
            delivery.insert_keys(&mut v);
            let mut human = format!("{task_id} {}", t.status);
            if let Some(b) = &delivery.brief {
                human.push('\n');
                human.push_str(&brief::human_lines(b, &format!("thoth task result {task_id}")));
            }
            output::emit(json, &v, &human);
            return Ok(0);
        }
        if Instant::now() >= deadline {
            let mut v = serde_json::json!({
                "ok": false,
                "task_id": task_id,
                "status": t.status,
                "timed_out": true,
                "run_id": serde_json::Value::Null,
            });
            Delivery::default().insert_keys(&mut v);
            output::emit(
                json,
                &v,
                &format!("timed out waiting for {task_id}; still {}", t.status),
            );
            return Ok(TIMEOUT_EXIT_CODE);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// BRIEF-1: what a wait reports about one run's outcome: the delivery's
/// `result_status` and `claim_count` (an implementer's) and the short
/// conclusion (an implementer's delivery or a reviewer's judgement). Each is
/// null when the run has none.
#[derive(Default)]
struct Delivery {
    result_status: serde_json::Value,
    claim_count: serde_json::Value,
    brief: Option<Brief>,
}

impl Delivery {
    /// An implementer run's newest `result_ingested` event.
    fn of_run(conn: &rusqlite::Connection, task_id: &str, run_id: &str) -> Result<Delivery> {
        let detail = state::events_since(conn, 0, Some(task_id))?
            .into_iter()
            .rev()
            .find(|e| e.run_id.as_deref() == Some(run_id) && e.event_type == event_type::RESULT_INGESTED)
            .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.detail).ok());
        let Some(d) = detail else {
            return Ok(Delivery::default());
        };
        Ok(Delivery {
            result_status: d["result_json"]["status"].clone(),
            claim_count: d["claim_count"].clone(),
            brief: Some(brief::for_result_event(&Masker::current(), &d)),
        })
    }

    /// A reviewer run's own judgement; no `result_status` or `claim_count`.
    fn of_judgement(brief: Option<Brief>) -> Delivery {
        Delivery { brief, ..Delivery::default() }
    }

    fn insert_keys(&self, v: &mut serde_json::Value) {
        if let Some(o) = v.as_object_mut() {
            o.insert("result_status".to_string(), self.result_status.clone());
            o.insert("claim_count".to_string(), self.claim_count.clone());
        }
        brief::insert_keys(v, self.brief.as_ref());
    }
}

/// BRIEF-1: a run's own outcome for `wait --run`, by its role, and the
/// command that shows the full text behind its brief.
fn run_delivery(conn: &rusqlite::Connection, r: &state::Run) -> Result<(Delivery, String)> {
    let masker = Masker::current();
    Ok(match r.role.as_str() {
        crate::model::role::REVIEWER => (
            Delivery::of_judgement(
                state::get_review_for_run(conn, &r.run_id)?
                    .and_then(|rev| brief::for_review_row(&masker, &rev.verdict, &rev.findings)),
            ),
            format!("thoth review show {}", r.task_id),
        ),
        crate::model::role::SPEC_REVIEWER => (
            Delivery::of_judgement(
                crate::commands::spec_review::review_for_run(conn, &r.task_id, &r.run_id)?.and_then(|n| {
                    brief::for_spec_review(&masker, &n.verdict, &n.mode, &n.document, n.edit_count)
                }),
            ),
            format!("thoth spec review show {}", r.task_id),
        ),
        _ => (
            Delivery::of_run(conn, &r.task_id, &r.run_id)?,
            format!("thoth task result {} --run {}", r.task_id, r.run_id),
        ),
    })
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
    let (_repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
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
            let (delivery, full_text) = run_delivery(&conn, &r)?;
            if let Some(b) = &delivery.brief {
                human.push('\n');
                human.push_str(&brief::human_lines(b, &full_text));
            }
            if let Some(line) = tools_unknown_line(&tools_unknown) {
                human.push('\n');
                human.push_str(&line);
            }
            // RFIX-1: whether this run's fix waits for verify and confirm,
            // and what to merge once a PASS lands.
            let pending_fix = crate::commands::review_fix::pending_fix_for_run(&conn, &r)?;
            if pending_fix == serde_json::json!(true) {
                if let Some(line) = fix_hint(&conn, &r)? {
                    human.push('\n');
                    human.push_str(&line);
                }
            }
            if verdict.as_deref() == Some("PASS") && r.role == crate::model::role::REVIEWER {
                if let Some(task) = state::get_task(&conn, &r.task_id)? {
                    let st = crate::commands::review_fix::state(&conn, &task)?;
                    if let (serde_json::Value::String(b), serde_json::Value::String(c)) =
                        crate::commands::review_fix::merge_target(&conn, &root, &task, &st)?
                    {
                        human.push_str(&format!(
                            "\nmerge_branch {b}, merge_commit {c}; read the change before you merge it"
                        ));
                    }
                }
            }
            let mut v = serde_json::json!({
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
                "pending_fix": pending_fix,
            });
            delivery.insert_keys(&mut v);
            output::emit(json, &v, &human);
            return Ok(0);
        }
        if Instant::now() >= deadline {
            // AGY-1: the same additive key on the timeout branch, from the
            // run's newest usage so far (a resume that has not written one
            // yet still shows the previous launch's).
            let tools_unknown = tools_unknown_for_run(&conn, &r.task_id, &run_id);
            let pending_fix = crate::commands::review_fix::pending_fix_for_run(&conn, &r)?;
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
                &{
                    let mut v = serde_json::json!({
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
                        "pending_fix": pending_fix,
                    });
                    // BRIEF-1: null like the verdict: nothing is settled yet.
                    Delivery::default().insert_keys(&mut v);
                    v
                },
                &human,
            );
            return Ok(TIMEOUT_EXIT_CODE);
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// RFIX-1: the next step for a run whose fix is pending.
fn fix_hint(conn: &rusqlite::Connection, r: &state::Run) -> Result<Option<String>> {
    let Some(task) = state::get_task(conn, &r.task_id)? else { return Ok(None) };
    let st = crate::commands::review_fix::state(conn, &task)?;
    let Some((row, _)) = st.applied.as_ref() else { return Ok(None) };
    let edit_run = row.reviewer_run_id.clone().unwrap_or_default();
    Ok(Some(format!(
        "a fix is pending: thoth verify {edit_run}, then thoth review confirm {} --profile P --model M --effort E",
        r.task_id
    )))
}
