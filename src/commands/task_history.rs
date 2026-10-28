//! What happened to one task so far (RJ-2): its attempts, how often it was
//! rejected, and its newest implementer and reviewer runs. Facts only; whether
//! to raise the model or effort is the commander's call.

use crate::commands::usage;
use crate::config::Paths;
use crate::db::state;
use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

pub(crate) struct RunFacts {
    pub(crate) run_id: String,
    pub(crate) attempt: i64,
    pub(crate) profile: String,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) effort_requested: Option<String>,
    pub(crate) effort: Option<String>,
    pub(crate) status: String,
    /// An implementer's newest review verdict; the verdict a reviewer gave.
    pub(crate) verdict: Option<String>,
}

impl RunFacts {
    fn json(&self) -> Value {
        json!({
            "run_id": self.run_id,
            "attempt": self.attempt,
            "profile": self.profile,
            "provider": self.provider,
            "model": self.model,
            "effort_requested": self.effort_requested,
            "effort": self.effort,
            "status": self.status,
            "verdict": self.verdict,
        })
    }

    fn human(&self, what: &str) -> String {
        format!(
            "last {what} run {}: {} {} effort {} ({})",
            self.run_id,
            self.profile,
            self.model,
            self.effort.as_deref().unwrap_or("null"),
            self.verdict.as_deref().unwrap_or("no verdict")
        )
    }
}

pub(crate) struct TaskHistory {
    pub(crate) task_id: String,
    pub(crate) attempts: i64,
    pub(crate) rejects: i64,
    pub(crate) last_implementer: Option<RunFacts>,
    pub(crate) last_reviewer: Option<RunFacts>,
}

impl TaskHistory {
    pub(crate) fn json(&self) -> Value {
        json!({
            "attempts": self.attempts,
            "rejects": self.rejects,
            "last_implementer": self.last_implementer.as_ref().map(RunFacts::json),
            "last_reviewer": self.last_reviewer.as_ref().map(RunFacts::json),
        })
    }

    /// The reminder line(s) before the account list; the reviewer line only
    /// for `--role reviewer`. `no runs yet` only when the task has no run of
    /// any role; a task with a spec review but no implementer run yet says so.
    pub(crate) fn human(&self, role: &str) -> Vec<String> {
        let mut lines = Vec::new();
        match &self.last_implementer {
            None if self.attempts == 0 => lines.push(format!("task {}: no runs yet", self.task_id)),
            None => lines.push(format!(
                "task {}: attempt {}, rejected {} time(s); last implementer run: none",
                self.task_id, self.attempts, self.rejects
            )),
            Some(run) => lines.push(format!(
                "task {}: attempt {}, rejected {} time(s); {}",
                self.task_id,
                self.attempts,
                self.rejects,
                run.human("implementer")
            )),
        }
        if role == "reviewer" {
            lines.push(match &self.last_reviewer {
                Some(run) => run.human("reviewer"),
                None => "last reviewer run: none".to_string(),
            });
        }
        lines
    }
}

fn newest_run(conn: &Connection, task_id: &str, role: &str) -> Result<Option<RunFacts>> {
    let verdict_sql = if role == "reviewer" {
        "SELECT verdict FROM reviews WHERE reviewer_run_id=r.run_id ORDER BY created_at DESC, review_id DESC LIMIT 1"
    } else {
        "SELECT verdict FROM reviews WHERE target_run_id=r.run_id ORDER BY created_at DESC, review_id DESC LIMIT 1"
    };
    let sql = format!(
        "SELECT r.run_id, r.attempt, COALESCE(r.account,''), COALESCE(r.provider,''), COALESCE(r.model,''),
                r.spec, COALESCE(r.status,''), ({verdict_sql})
         FROM runs r WHERE r.task_id=?1 AND r.role=?2
         ORDER BY COALESCE(r.started_at,'') DESC, r.rowid DESC LIMIT 1"
    );
    let row = conn
        .query_row(&sql, [task_id, role], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })
        .optional()?;
    Ok(row.map(|(run_id, attempt, profile, provider, model, spec, status, verdict)| {
        let launch = spec.as_deref().and_then(crate::adapter::effort::parse_launch);
        RunFacts {
            run_id,
            attempt,
            profile,
            provider,
            model,
            effort_requested: launch.as_ref().and_then(|l| l.effort_requested.clone()),
            effort: launch.and_then(|l| l.effort),
            status,
            verdict,
        }
    }))
}

/// The history of the task whose full id is `task_id`, looked up in every
/// registered repository; the one that has the task answers.
pub(crate) fn build(paths: &Paths, repos: &[String], task_id: &str) -> Result<Option<TaskHistory>> {
    for repo in repos {
        let path = paths.state_db(repo);
        if !path.is_file() {
            continue;
        }
        let conn = usage::read_only(&path)?;
        let exists: Option<String> = conn
            .query_row("SELECT task_id FROM tasks WHERE task_id=?1", [task_id], |r| r.get(0))
            .optional()
            .with_context(|| format!("looking for task {task_id} in {}", path.display()))?;
        if exists.is_none() {
            continue;
        }
        // The highest attempt of any run of the task, a spec review's included
        // (it carries the attempt the next dispatch will use), 0 without a run.
        let attempts = state::max_attempt(&conn, task_id)?.unwrap_or(0);
        let rejects = state::list_reviews_for_task(&conn, task_id)?
            .iter()
            .filter(|r| r.verdict == "REJECT")
            .count() as i64;
        return Ok(Some(TaskHistory {
            task_id: task_id.to_string(),
            attempts,
            rejects,
            last_implementer: newest_run(&conn, task_id, "implementer")?,
            last_reviewer: newest_run(&conn, task_id, "reviewer")?,
        }));
    }
    Ok(None)
}
