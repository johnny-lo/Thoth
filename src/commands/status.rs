use crate::config::Paths;
use crate::db::state::{self, Run};
use crate::ids;
use crate::lang::Lang;
use crate::model::{event_type, TaskStatus};
use crate::output;
use crate::plan_label;
use anyhow::Result;
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// brief section 2: a runner heartbeats every 5 s, so 15 s of silence means it is
/// probably dead and `cancel` may kill the worker directly.
pub const STALE_AFTER_SECONDS: i64 = 15;

/// Only a `running` run can be stale; anything unparsable is reported as fresh so
/// a clock or format problem never fakes a dead runner.
pub fn heartbeat_is_stale(now_rfc3339: &str, heartbeat: Option<&str>, run_status: &str) -> bool {
    if run_status != "running" {
        return false;
    }
    let Some(hb) = heartbeat else { return false };
    let (Ok(now), Ok(then)) = (
        OffsetDateTime::parse(now_rfc3339, &Rfc3339),
        OffsetDateTime::parse(hb, &Rfc3339),
    ) else {
        return false;
    };
    (now - then).whole_seconds() > STALE_AFTER_SECONDS
}

/// What a run's `run_usage` event recorded: `models` maps model id to output
/// tokens (null when the harness reported none), `subagents_spawned` is the
/// count (null when unknown).
pub struct RunUsage {
    pub models: serde_json::Value,
    pub subagents_spawned: serde_json::Value,
}

/// The newest `run_usage` event of each run of `task_id`, keyed by run id. A
/// run with no such event (never reached a result line) is simply absent.
pub fn run_usage_by_run(
    conn: &rusqlite::Connection,
    task_id: &str,
) -> Result<std::collections::HashMap<String, RunUsage>> {
    let mut map = std::collections::HashMap::new();
    for e in state::events_since(conn, 0, Some(task_id))? {
        if e.event_type != event_type::RUN_USAGE {
            continue;
        }
        let Some(run_id) = e.run_id else { continue };
        let d: serde_json::Value = serde_json::from_str(&e.detail).unwrap_or_default();
        map.insert(
            run_id,
            RunUsage {
                models: d["models"].clone(),
                subagents_spawned: d["subagents_spawned"].clone(),
            },
        );
    }
    Ok(map)
}

/// Whether `run` stopped because its profile's quota was used up.
fn is_quota_stop(run: &Run) -> bool {
    run.status == "interrupted" && run.interrupted_reason.as_deref() == Some("quota")
}

/// RJ-1 (AC4): for a run stopped on quota, the profiles that still have
/// quota (`with_quota`, every eligible profile in ranking order) other than
/// its own; `None` for any other run, or when the quota could not be read.
fn others_with_quota(run: &Run, with_quota: Option<&[String]>) -> Option<Vec<String>> {
    if !is_quota_stop(run) {
        return None;
    }
    with_quota.map(|ids| ids.iter().filter(|id| **id != run.account).cloned().collect())
}

/// FC-1 (H1): what to do about a run that stopped early, by its status and
/// interruption reason, naming its profile, run, task and model. `None` for a
/// run that needs nothing (completed, cancelled, still going, failed/user).
/// `with_quota` is used for a quota stop only (RJ-1): the other profiles that
/// still have quota, or `None` when their quota could not be read.
pub(crate) fn next_step(run: &Run, with_quota: Option<&[String]>) -> Option<String> {
    let profile = &run.account;
    let run_id = &run.run_id;
    let task = &run.task_id;
    let model = &run.model;
    let has_session = run.harness_session_id.is_some();
    let dispatch = format!("thoth dispatch {task} --profile {profile} --model {model}");
    let text = match (run.status.as_str(), run.interrupted_reason.as_deref()) {
        ("interrupted", Some("quota")) => {
            // The commander moves on to a profile with quota now instead of
            // waiting for this one to reset; it writes why in --reason.
            let (again, command) = match run.role.as_str() {
                crate::model::role::REVIEWER => (
                    "start the review again with the next reviewer in your order that still has quota",
                    "thoth review start",
                ),
                crate::model::role::SPEC_REVIEWER => (
                    "start the spec review again with the next reviewer in your order that still has quota",
                    "thoth spec review start",
                ),
                _ => ("dispatch again with a profile that still has quota", "thoth dispatch"),
            };
            let effort = crate::adapter::effort::parse_launch(&run.spec)
                .and_then(|l| l.effort_requested)
                .map(|level| format!(" --effort {level}"))
                .unwrap_or_default();
            let others = match with_quota {
                None => "could not read the other profiles' quota".to_string(),
                Some([]) => "no other profile has quota now".to_string(),
                Some(ids) => format!("profiles with quota now: {}", ids.join(", ")),
            };
            format!(
                "quota used up on {profile}; {again}: {command} {task} --profile OTHER \
                 --model {model}{effort} --reason TEXT ({others}); do not wait for {profile}"
            )
        }
        ("interrupted", Some("auth")) if has_session => format!(
            "{profile} is logged out; run thoth profile login {profile}, then thoth resume {run_id}"
        ),
        ("interrupted", Some("auth")) => format!(
            "{profile} is logged out; run thoth profile login {profile}, then {dispatch}"
        ),
        ("interrupted", Some("transient")) if has_session => format!(
            "a temporary problem (network or a busy service), not a quota or login problem; run \
             thoth resume {run_id}"
        ),
        ("interrupted", Some("transient")) => format!(
            "a temporary problem (network or a busy service), not a quota or login problem; run \
             {dispatch}"
        ),
        ("interrupted", Some("crash")) if has_session => format!(
            "the worker stopped without a result; run thoth resume {run_id} once, and dispatch \
             again if it happens twice"
        ),
        ("interrupted", Some("crash")) => {
            format!("the worker stopped without a result; run {dispatch}")
        }
        ("failed", Some("timeout")) => format!(
            "the run hit its wall-clock timeout; dispatch again with a larger --timeout: \
             {dispatch} --timeout 2h"
        ),
        ("failed", None) => format!(
            "the harness reported a failure; read error_text, then dispatch again: {dispatch}"
        ),
        _ => return None,
    };
    Some(text)
}

fn run_json(
    r: &Run,
    now: &str,
    usage: Option<&RunUsage>,
    with_quota: Option<&[String]>,
) -> serde_json::Value {
    let launch = crate::adapter::effort::parse_launch(&r.spec);
    let others = others_with_quota(r, with_quota);
    serde_json::json!({
        "run_id": r.run_id,
        "task_id": r.task_id,
        "attempt": r.attempt,
        // Whole-branch review Minor 5: without this, the one column that says
        // whether a row is the implementer or the reviewer was missing from
        // the only command an operator uses to look at a task's runs - and
        // Phase 2 is the phase where a task has both.
        "role": r.role,
        "profile": r.account,
        "model": r.model,
        "allowed_tools": r.allowed_tools,
        "status": r.status,
        "interrupted_reason": r.interrupted_reason,
        "exit_code": r.exit_code,
        "worktree": r.worktree,
        "candidate_commit": r.candidate_commit,
        "harness_session_id": r.harness_session_id,
        "runner_pid": r.runner_pid,
        "worker_pid": r.worker_pid,
        "heartbeat_at": r.heartbeat_at,
        "heartbeat_stale": heartbeat_is_stale(now, r.heartbeat_at.as_deref(), &r.status),
        "started_at": r.started_at,
        "ended_at": r.ended_at,
        "cancel_requested": r.cancel_requested != 0,
        "timeout_seconds": r.timeout_seconds,
        "effort_requested": launch.as_ref().and_then(|l| l.effort_requested.clone()),
        "effort": launch.as_ref().and_then(|l| l.effort.clone()),
        "run_dir": r.run_dir,
        "error_text": r.error_text,
        "models_used": usage.map(|u| u.models.clone()).unwrap_or(serde_json::Value::Null),
        "subagents_spawned": usage
            .map(|u| u.subagents_spawned.clone())
            .unwrap_or(serde_json::Value::Null),
        "next_step": next_step(r, others.as_deref()),
        "profiles_with_quota": others,
    })
}

/// What `status` with no TASK says first, outside a git repository.
pub const OUTSIDE_REPO_NOTE: &str = "not inside a git repository: open tasks of every registered \
                                     repository are listed; run dispatch, verify and review inside a repository";

/// The open tasks of one repo (pass and cancelled hidden), each as its
/// `--json` row and its human line; `repo` is the root every row carries.
fn open_task_rows(
    conn: &rusqlite::Connection,
    now: &str,
    repo: &str,
) -> Result<Vec<(serde_json::Value, String)>> {
    let mut out = Vec::new();
    for t in state::list_tasks(conn)? {
        // `failed` stays listed: it can be dispatched again. `is_dead_end`
        // is exactly the states nobody can act on (pass, cancelled, accepted); `reject`
        // is the rework loop's entry state and stays listed too.
        let hide = match TaskStatus::parse(&t.status) {
            Ok(s) => s.is_dead_end(),
            Err(_) => false,
        };
        if hide {
            continue;
        }
        let runs = state::list_runs_for_task(conn, &t.task_id)?;
        let stale = runs
            .iter()
            .any(|r| heartbeat_is_stale(now, r.heartbeat_at.as_deref(), &r.status));
        let row = serde_json::json!({
            "task_id": t.task_id,
            "title": t.title,
            "status": t.status,
            "risk": t.risk,
            "runs": runs.len(),
            "any_heartbeat_stale": stale,
            "updated_at": t.updated_at,
            "repo": repo,
        });
        let line = format!(
            "{}  {:<11} runs={} {}{}",
            t.task_id,
            t.status,
            runs.len(),
            if stale { "STALE " } else { "" },
            t.title
        );
        out.push((row, line));
    }
    Ok(out)
}

/// `status` with no TASK outside a git repository: the open tasks of every
/// registered repo whose state.db exists, read-only, ordered by root. No
/// global.db means nothing is registered, and no file is created.
fn run_outside(paths: &Paths, json: bool) -> Result<i32> {
    let now = ids::now_rfc3339();
    let mut repos = Vec::new();
    if paths.global_db().is_file() {
        let global = crate::commands::usage::global_read_only(paths)?;
        let mut stmt = global.prepare(
            "SELECT repo_id, COALESCE(root_path, '') FROM repos ORDER BY root_path, repo_id",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (repo_id, root) = row?;
            let path = paths.state_db(&repo_id);
            if path.is_file() {
                repos.push((root, crate::commands::usage::read_only(&path)?));
            }
        }
    }
    let mut rows = Vec::new();
    let mut lines = vec![OUTSIDE_REPO_NOTE.to_string()];
    for (root, conn) in &repos {
        let tasks = open_task_rows(conn, &now, root)?;
        if tasks.is_empty() {
            continue;
        }
        lines.push(root.clone());
        for (row, line) in tasks {
            rows.push(row);
            lines.push(format!("  {line}"));
        }
    }
    if rows.is_empty() {
        lines.push("no open tasks".to_string());
    }
    output::emit(
        json,
        &serde_json::json!({
            "tasks": rows,
            "last_verify": null,
            "outside_repo": true,
            "note": OUTSIDE_REPO_NOTE,
        }),
        &lines.join("\n"),
    );
    Ok(0)
}

pub fn run(paths: &Paths, cwd: &Path, task: Option<&str>, json: bool) -> Result<i32> {
    if task.is_none() && crate::worktree::find_repo_root(cwd)?.is_err() {
        return run_outside(paths, json);
    }
    let (_repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let now = ids::now_rfc3339();

    match task {
        None => {
            let (rows, lines): (Vec<_>, Vec<_>) =
                open_task_rows(&conn, &now, &root.display().to_string())?.into_iter().unzip();
            let mut human = if lines.is_empty() {
                "no open tasks".to_string()
            } else {
                lines.join("\n")
            };
            let last = state::latest_event_of_type(&conn, event_type::VERIFY_RAN, None)?;
            let last_verify = last.as_ref().map(|e| {
                let d: serde_json::Value = serde_json::from_str(&e.detail).unwrap_or_default();
                let missing = d["missing_since_last_verify"].clone();
                let missing = if missing.is_array() { missing } else { serde_json::json!([]) };
                serde_json::json!({
                    "task_id": e.task_id, "run_id": e.run_id, "ts": e.ts, "seq": e.seq,
                    "missing_since_last_verify": missing,
                })
            });
            if let Some(lv) = &last_verify {
                let missing = lv["missing_since_last_verify"].as_array().cloned().unwrap_or_default();
                if !missing.is_empty() {
                    human.push_str(&format!(
                        "\nverify of run {} (task {}) found approved checks missing since the \
                         previous verify: {}",
                        lv["run_id"].as_str().unwrap_or("-"),
                        lv["task_id"].as_str().unwrap_or("-"),
                        crate::commands::verify::missing_items(&missing)
                    ));
                }
            }
            output::emit(
                json,
                &serde_json::json!({
                    "tasks": rows,
                    "last_verify": last_verify,
                    "outside_repo": false,
                    "note": null,
                }),
                &human,
            );
        }
        Some(needle) => {
            let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;
            let t = state::get_task(&conn, &task_id)?
                .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
            let runs = state::list_runs_for_task(&conn, &task_id)?;
            let reviews = state::list_reviews_for_task(&conn, &task_id)?;
            let latest_verdict = reviews.last().map(|r| r.verdict.clone());
            let stuck = crate::commands::dispatch::stuck_reason(&conn, &task_id)?;
            // Task 8: how many attempts this task has spent and how many the
            // rework loop will still allow, so a Supervisor looking at a
            // `reject` can see whether `dispatch` will take it. R4 (fix round
            // 1): a task with no runs has spent no attempts - `None`, not 0,
            // which is a number no attempt ever has.
            let max_attempt: Option<i64> = runs.iter().map(|r| r.attempt).max();
            let attempt_cap = state::get_task_attempt_cap(&conn, &task_id)?;
            let usage = run_usage_by_run(&conn, &task_id)?;
            // RJ-1 (AC4): the profiles that still have quota, read only when
            // a run stopped on quota; read-only, no refresh. `None` when the
            // quota cannot be read.
            let with_quota: Option<Vec<String>> = if runs.iter().any(is_quota_stop) {
                crate::commands::usage::ranked_candidates(
                    paths,
                    crate::model::role::IMPLEMENTER,
                    None,
                    None,
                )
                .ok()
                .map(|ranked| {
                    ranked
                        .into_iter()
                        .filter(|c| c.eligible)
                        .map(|c| c.profile.profile_id)
                        .collect()
                })
            } else {
                None
            };
            let run_rows: Vec<serde_json::Value> = runs
                .iter()
                .map(|r| run_json(r, &now, usage.get(&r.run_id), with_quota.as_deref()))
                .collect();
            let mut lines = vec![format!(
                "{}{}  {}  {}{}{}",
                t.task_id,
                stuck.as_ref().map(|s| format!(" [stuck: {}]", s.value())).unwrap_or_default(),
                t.status,
                t.title,
                max_attempt
                    .map(|a| format!("  [attempt {a}/{attempt_cap}]"))
                    .unwrap_or_default(),
                latest_verdict
                    .as_ref()
                    .map(|v| format!("  [latest review: {v}]"))
                    .unwrap_or_default()
            )];
            // TIER-1: `claude-a (Max)` in the account column, read-only and
            // only for the human output; the column is as wide as the widest.
            let labels = if json { Default::default() } else { plan_label::read_only_labels(paths) };
            let accounts: Vec<String> = runs
                .iter()
                .map(|r| plan_label::named(&r.account, labels.get(&r.account).map(String::as_str), Lang::En))
                .collect();
            let width = accounts.iter().map(|a| a.chars().count()).max().unwrap_or(0).max(10);
            for (r, account) in runs.iter().zip(&accounts) {
                lines.push(format!(
                    "  {}  a{}  {:<11} {:<11} {:<width$} {:<8} hb={}{}{}",
                    r.run_id,
                    r.attempt,
                    r.role,
                    r.status,
                    account,
                    r.model,
                    r.heartbeat_at.clone().unwrap_or_else(|| "-".to_string()),
                    if heartbeat_is_stale(&now, r.heartbeat_at.as_deref(), &r.status) {
                        "  STALE"
                    } else {
                        ""
                    },
                    r.interrupted_reason
                        .as_ref()
                        .map(|reason| format!(" reason={reason}"))
                        .unwrap_or_default()
                ));
            }
            // FC-1: the next step for the newest run only, once.
            if let Some(text) = runs.iter().max_by(|a, b| a.run_id.cmp(&b.run_id)).and_then(|r| {
                next_step(r, others_with_quota(r, with_quota.as_deref()).as_deref())
            }) {
                lines.push(format!("  next: {text}"));
            }
            let verify = state::latest_event_of_type(&conn, event_type::VERIFY_RAN, Some(&task_id))?
                .map(|e| {
                    let d: serde_json::Value =
                        serde_json::from_str(&e.detail).unwrap_or_default();
                    let missing = if d["missing_since_last_verify"].is_array() {
                        d["missing_since_last_verify"].clone()
                    } else {
                        serde_json::json!([])
                    };
                    let count = d["count"].as_i64().unwrap_or(0);
                    let failures = d["failures"].as_i64().unwrap_or(0);
                    serde_json::json!({
                        "run_id": e.run_id, "ts": e.ts, "seq": e.seq,
                        "ok": failures == 0, "count": count, "failures": failures,
                        "source": d["source"],
                        "at_commit": d["at_commit"],
                        "approved_checks": d["approved_checks"],
                        "previous_verify": d["previous_verify"],
                        "missing_since_last_verify": missing,
                    })
                });
            if let Some(v) = &verify {
                lines.push(format!(
                    "  verify: {} at {}  {} check(s), {} failed or refused",
                    v["run_id"].as_str().unwrap_or("-"),
                    v["ts"].as_str().unwrap_or("-"),
                    v["count"],
                    v["failures"]
                ));
                let missing = v["missing_since_last_verify"].as_array().cloned().unwrap_or_default();
                if let Some(l) =
                    crate::commands::verify::missing_line(&v["previous_verify"], &missing)
                {
                    lines.push(l);
                }
            }
            // BASE-1: the branch the task started from lives only in its
            // task_created event; null for a task created before it.
            let base_branch = state::latest_event_of_type(&conn, event_type::TASK_CREATED, Some(&task_id))?
                .and_then(|e| serde_json::from_str::<serde_json::Value>(&e.detail).ok())
                .map(|d| d["base_branch"].clone())
                .unwrap_or(serde_json::Value::Null);
            output::emit(
                json,
                &serde_json::json!({
                    "verify": verify,
                    "task_id": t.task_id,
                    "title": t.title,
                    "status": t.status,
                    "risk": t.risk,
                    "base_commit": t.base_commit,
                    "base_branch": base_branch,
                    "runs": run_rows,
                    "latest_verdict": latest_verdict,
                    "review_count": reviews.len(),
                    "attempt": max_attempt,
                    "attempt_cap": attempt_cap,
                    "stuck": stuck.as_ref().map(|s| serde_json::json!({"kind": s.kind(), "value": s.value(), "count": 3})),
                }),
                &lines.join("\n"),
            );
        }
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_run_with_an_old_heartbeat_is_stale() {
        let now = "2026-09-14T10:00:30Z";
        assert!(heartbeat_is_stale(now, Some("2026-09-14T10:00:00Z"), "running"));
        assert!(!heartbeat_is_stale(now, Some("2026-09-14T10:00:20Z"), "running"));
        // exactly at the threshold is not yet stale
        assert!(!heartbeat_is_stale(now, Some("2026-09-14T10:00:15Z"), "running"));
    }

    #[test]
    fn a_finished_or_never_started_run_is_never_stale() {
        let now = "2026-09-14T10:00:30Z";
        assert!(!heartbeat_is_stale(now, Some("2026-09-14T09:00:00Z"), "completed"));
        assert!(!heartbeat_is_stale(now, None, "queued"));
        assert!(!heartbeat_is_stale(now, Some("nonsense"), "running"));
        assert_eq!(STALE_AFTER_SECONDS, 15);
    }

    fn run(status: &str, reason: Option<&str>, session: bool) -> Run {
        Run {
            run_id: "RUN1".into(),
            task_id: "TASK1".into(),
            attempt: 1,
            role: "implementer".into(),
            provider: "anthropic".into(),
            account: "claude-b".into(),
            model: "sonnet".into(),
            harness_session_id: session.then(|| "S1".to_string()),
            config_dir: "/cfg".into(),
            allowed_tools: "Bash".into(),
            worktree: "/wt".into(),
            base_commit: "abc".into(),
            candidate_commit: None,
            spec: "{}".into(),
            status: status.into(),
            interrupted_reason: reason.map(str::to_string),
            exit_code: None,
            input_tokens: None,
            output_tokens: None,
            started_at: None,
            ended_at: None,
            runner_pid: None,
            worker_pid: None,
            heartbeat_at: None,
            cancel_requested: 0,
            timeout_seconds: 1800,
            run_dir: "/rd".into(),
            error_text: None,
            harness_bin: None,
        }
    }

    #[test]
    fn next_step_names_the_profile_run_and_task_for_each_reason() {
        let cases = [
            (
                "interrupted",
                Some("quota"),
                true,
                "quota used up on claude-b; dispatch again with a profile that still has quota: \
                 thoth dispatch TASK1 --profile OTHER --model sonnet --reason TEXT (profiles with \
                 quota now: claude-a, codex-a); do not wait for claude-b",
            ),
            (
                "interrupted",
                Some("auth"),
                true,
                "claude-b is logged out; run thoth profile login claude-b, then thoth resume RUN1",
            ),
            (
                "interrupted",
                Some("auth"),
                false,
                "claude-b is logged out; run thoth profile login claude-b, then thoth dispatch \
                 TASK1 --profile claude-b --model sonnet",
            ),
            (
                "interrupted",
                Some("transient"),
                true,
                "a temporary problem (network or a busy service), not a quota or login problem; \
                 run thoth resume RUN1",
            ),
            (
                "interrupted",
                Some("transient"),
                false,
                "a temporary problem (network or a busy service), not a quota or login problem; \
                 run thoth dispatch TASK1 --profile claude-b --model sonnet",
            ),
            (
                "interrupted",
                Some("crash"),
                true,
                "the worker stopped without a result; run thoth resume RUN1 once, and dispatch \
                 again if it happens twice",
            ),
            (
                "interrupted",
                Some("crash"),
                false,
                "the worker stopped without a result; run thoth dispatch TASK1 --profile claude-b \
                 --model sonnet",
            ),
            (
                "failed",
                Some("timeout"),
                true,
                "the run hit its wall-clock timeout; dispatch again with a larger --timeout: thoth \
                 dispatch TASK1 --profile claude-b --model sonnet --timeout 2h",
            ),
            (
                "failed",
                None,
                true,
                "the harness reported a failure; read error_text, then dispatch again: thoth \
                 dispatch TASK1 --profile claude-b --model sonnet",
            ),
        ];
        let with_quota = ["claude-a".to_string(), "codex-a".to_string()];
        for (status, reason, session, expected) in cases {
            let others = (reason == Some("quota")).then_some(&with_quota[..]);
            assert_eq!(
                next_step(&run(status, reason, session), others).as_deref(),
                Some(expected),
                "{status} {reason:?} session={session}"
            );
        }
        assert_eq!(next_step(&run("completed", None, true), None), None);
        assert_eq!(next_step(&run("cancelled", Some("user"), true), None), None);
    }

    #[test]
    fn a_quota_stop_names_the_role_command_effort_and_the_other_profiles() {
        // a spec reviewer whose quota could not be read: the AC4 (2) example
        let mut r = run("interrupted", Some("quota"), true);
        r.role = "spec_reviewer".into();
        r.account = "claude-a".into();
        r.model = "opus".into();
        r.task_id = "T1".into();
        r.spec = r#"{"launch":{"effort_requested":"high","effort":"high","effort_note":null}}"#.into();
        assert_eq!(
            next_step(&r, None).as_deref(),
            Some(
                "quota used up on claude-a; start the spec review again with the next reviewer in \
                 your order that still has quota: thoth spec review start T1 --profile OTHER \
                 --model opus --effort high --reason TEXT (could not read the other profiles' \
                 quota); do not wait for claude-a"
            )
        );
        // every role, with and without a session, every state of the list,
        // with and without an effort: never wait, never resume
        let effort_spec = r#"{"launch":{"effort_requested":"high","effort":"high","effort_note":null}}"#;
        let roles = [
            ("implementer", "dispatch again with a profile that still has quota: thoth dispatch T1"),
            ("reviewer", "start the review again with the next reviewer in your order that still has quota: thoth review start T1"),
            ("spec_reviewer", "start the spec review again with the next reviewer in your order that still has quota: thoth spec review start T1"),
        ];
        let only_b = ["claude-b".to_string()];
        let others: [(Option<&[String]>, &str); 3] = [
            (Some(&[]), "(no other profile has quota now)"),
            (None, "(could not read the other profiles' quota)"),
            (Some(&only_b), "(profiles with quota now: claude-b)"),
        ];
        for (role, command) in roles {
            for session in [true, false] {
                for (with_quota, list) in others {
                    for spec in [effort_spec, "{}"] {
                        r.role = role.into();
                        r.harness_session_id = session.then(|| "S1".to_string());
                        r.spec = spec.into();
                        let text = next_step(&r, with_quota).unwrap();
                        let effort = if spec == "{}" { "" } else { " --effort high" };
                        let flags = format!("--profile OTHER --model opus{effort} --reason TEXT {list}");
                        assert!(text.contains(command), "{text}");
                        assert!(text.contains(&flags), "{text}");
                        assert!(text.ends_with("; do not wait for claude-a"), "{text}");
                        assert!(!text.contains("thoth resume"), "{text}");
                        assert!(!text.contains("window to reset"), "{text}");
                        assert_eq!(text.contains("--effort"), spec != "{}", "{text}");
                    }
                }
            }
        }
        // the json list leaves out the run's own profile, and is only for a quota stop
        r.role = "implementer".into();
        let all = ["claude-a".to_string(), "claude-b".to_string()];
        assert_eq!(others_with_quota(&r, Some(&all)), Some(vec!["claude-b".to_string()]));
        assert_eq!(others_with_quota(&r, None), None);
        assert_eq!(others_with_quota(&run("interrupted", Some("auth"), true), Some(&all)), None);
    }
}
