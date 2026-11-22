//! Data-layer reads behind the quota and model reports (SQL-2): `models`,
//! `profile usage` / `recommend`, `report`, the live quota readers and the
//! per-run quota points.
//!
//! Every function takes the caller's connection and only reads; the caller
//! keeps the parsing, ranking and page assembly. Return types are plain
//! values, tuples, `Option`, `Vec` or the structs defined here, so this
//! module never depends on the command layer or the run quota module.

use crate::model::event_type;
use anyhow::Result;
use rusqlite::{Connection, OptionalExtension};

// ------------------------------------------------------------ models

/// `models`' `comparison_name`: the `ts` (empty when NULL) and `detail` of
/// the newest `harness_init` event of a run of `account` with `model`, or
/// `None` when there is none.
pub fn newest_harness_init(
    conn: &Connection,
    account: &str,
    model: &str,
) -> Result<Option<(String, String)>> {
    Ok(conn
        .query_row(
            "SELECT COALESCE(e.ts,''), e.detail FROM events e JOIN runs r ON r.run_id=e.run_id WHERE r.account=?1 AND r.model=?2 AND e.type='harness_init' ORDER BY e.ts DESC, e.seq DESC LIMIT 1",
            [account, model], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .optional()?)
}

/// `models`' `thoth`: of the implementer runs of `profile` with `model`, how
/// many there are, how many have a review, and how many reviews of them say
/// PASS and REJECT.
pub fn implementer_review_counts(
    conn: &Connection,
    profile: &str,
    model: &str,
) -> Result<(i64, i64, i64, i64)> {
    Ok(conn.query_row(
            "SELECT COUNT(*),
             SUM(CASE WHEN EXISTS (SELECT 1 FROM reviews v WHERE v.target_run_id=r.run_id) THEN 1 ELSE 0 END),
             (SELECT COUNT(*) FROM reviews v JOIN runs x ON x.run_id=v.target_run_id WHERE x.role='implementer' AND x.account=?1 AND x.model=?2 AND v.verdict='PASS'),
             (SELECT COUNT(*) FROM reviews v JOIN runs x ON x.run_id=v.target_run_id WHERE x.role='implementer' AND x.account=?1 AND x.model=?2 AND v.verdict='REJECT')
             FROM runs r WHERE r.role='implementer' AND r.account=?1 AND r.model=?2",
            [profile, model],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0), r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
        )?)
}

/// `models`' `thoth`: the run ids of the implementer runs of `profile` with
/// `model`.
pub fn implementer_run_ids(conn: &Connection, profile: &str, model: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
            "SELECT run_id FROM runs WHERE role='implementer' AND account=?1 AND model=?2",
        )?;
    let ids = stmt.query_map([profile, model], |r| r.get::<_, String>(0))?;
    Ok(ids.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One implementer run as `models`' `thoth_breakdown` reads it; NULL reads
/// as empty text or 0.
pub struct BreakdownRun {
    pub run_id: String,
    pub status: String,
    pub run_spec: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub task_spec: String,
    /// cache_read, cache_write, reasoning and total tokens.
    pub kinds: [i64; 4],
}

/// `models`' `thoth_breakdown`: the implementer runs of `profile` with
/// `model`, each with its task's spec.
pub fn implementer_breakdown_runs(
    conn: &Connection,
    profile: &str,
    model: &str,
) -> Result<Vec<BreakdownRun>> {
    // A repo not yet opened by a version with DT-1 is still v4: its
    // `runs` has no token-kind columns, which then read as NULL.
    let col = |name: &str| {
        if crate::db::migrations::column_exists(conn, "runs", name).unwrap_or(false) {
            format!("r.{name}")
        } else {
            "NULL".to_string()
        }
    };
    let mut stmt = conn.prepare(&format!(
            "SELECT r.run_id, r.status, r.spec, r.input_tokens, r.output_tokens, t.spec,
                    {}, {}, {}, {}
             FROM runs r LEFT JOIN tasks t ON t.task_id=r.task_id
             WHERE r.role='implementer' AND r.account=?1 AND r.model=?2",
            col("cache_read_tokens"),
            col("cache_write_tokens"),
            col("reasoning_tokens"),
            col("total_tokens"),
        ))?;
    let rows = stmt.query_map([profile, model], |r| {
            Ok(BreakdownRun {
                run_id: r.get::<_, String>(0)?,
                status: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                run_spec: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                input_tokens: r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                output_tokens: r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                task_spec: r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                kinds: [
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                ],
            })
        })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `models`' `thoth_breakdown`: the verdict of the run's newest review.
/// Any error, a missing row included, reads as `None`.
pub fn latest_verdict(conn: &Connection, run_id: &str) -> Option<String> {
    conn.query_row(
                "SELECT verdict FROM reviews WHERE target_run_id=?1 ORDER BY created_at DESC, review_id DESC LIMIT 1", [&run_id],
                |r| r.get(0)).ok()
}

/// `models`' `thoth_breakdown`: the detail of the run's newest `run_quota`
/// event. Any error, a missing row included, and a NULL detail read as
/// `None`.
pub fn latest_run_quota_detail(conn: &Connection, run_id: &str) -> Option<String> {
    conn.query_row(
                "SELECT detail FROM events WHERE run_id=?1 AND type=?2 ORDER BY seq DESC LIMIT 1",
                [run_id, event_type::RUN_QUOTA], |r| r.get::<_, Option<String>>(0)).ok().flatten()
}

/// `models`' `all_breakdowns`: every (account, model) with an implementer
/// run that names both.
pub fn implementer_pairs(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
            "SELECT DISTINCT account, model FROM runs
             WHERE role='implementer' AND account IS NOT NULL AND model IS NOT NULL AND model <> ''",
        )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ------------------------------------------------------------ usage

/// `usage`'s `parked_group`: the detail of the profile's newest
/// `rate_limited` quota event, or `None` when there is none.
pub fn newest_rate_limited_detail(global: &Connection, profile_id: &str) -> Result<Option<String>> {
    let detail: Option<String> = global
        .query_row(
            "SELECT detail FROM quota_events WHERE profile_id=?1 AND kind='rate_limited' ORDER BY seq DESC LIMIT 1",
            [profile_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(detail)
}

/// `usage`'s `registered_repos`: every repo id in global.db, sorted.
pub fn repo_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT repo_id FROM repos ORDER BY repo_id")?;
    let rows = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `usage`'s `claude_windows`: the (`ts`, `detail`) of every
/// `rate_limit_warning` event of a run of `profile`.
pub fn account_rate_limit_warnings(conn: &Connection, profile: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
            "SELECT e.ts, e.detail FROM events e JOIN runs r ON r.run_id=e.run_id
             WHERE r.account=?1 AND e.type='rate_limit_warning'",
        )?;
    let rows = stmt.query_map([profile], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `usage`'s `live_runs`: how many runs of `profile` are queued or running,
/// leaving out a `running` run whose runner is gone (SMALL-11 (4): stale
/// heartbeat and a recorded runner pid that is no thoth process), which
/// `thoth status` settles as `interrupted`.
pub fn live_run_count(conn: &Connection, profile: &str) -> Result<i64> {
    let mut stmt = conn.prepare(
        "SELECT status, heartbeat_at, runner_pid FROM runs WHERE account=?1 AND status IN ('queued','running')",
    )?;
    let rows = stmt
        .query_map([profile], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?, r.get::<_, Option<i64>>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let now = crate::ids::now_rfc3339();
    Ok(rows
        .iter()
        .filter(|(status, heartbeat, pid)| {
            !crate::runner::runner_is_gone(status, heartbeat.as_deref(), *pid, &now)
        })
        .count() as i64)
}

/// `usage`'s `resolve_task`: every task id.
pub fn task_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT task_id FROM tasks")?;
        ids.extend(
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    Ok(ids)
}

/// `usage`'s `task_provider`: the provider and `started_at` (empty when
/// NULL) of the task's newest implementer run, or `None` when it has none.
pub fn newest_implementer_provider(conn: &Connection, task_id: &str) -> Result<Option<(String, String)>> {
    Ok(conn.query_row("SELECT provider, COALESCE(started_at,'') FROM runs WHERE task_id=?1 AND role='implementer' ORDER BY started_at DESC LIMIT 1", [task_id], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
}

/// `usage`'s `ranked_candidates`: the `ts` of the profile's newest
/// `rate_limited` quota event, or `None` when there is none.
pub fn newest_rate_limited_ts(global: &Connection, profile_id: &str) -> Result<Option<String>> {
    Ok(global.query_row("SELECT ts FROM quota_events WHERE profile_id=?1 AND kind='rate_limited' ORDER BY seq DESC LIMIT 1", [profile_id], |r| r.get::<_, String>(0)).optional()?)
}

// ------------------------------------------------------------ report

/// `report`'s `Snapshot::load`: every (repo id, root path) in global.db, the
/// root path empty when NULL, sorted by root path then repo id.
pub fn repo_roots(global: &Connection) -> Result<Vec<(String, String)>> {
    let mut stmt = global.prepare(
                "SELECT repo_id, COALESCE(root_path, '') FROM repos ORDER BY root_path, repo_id",
            )?;
    let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// A run's role (empty when NULL), `started_at` and `ended_at`.
pub type RunTimes = (String, Option<String>, Option<String>);

/// `report`'s `add_work`: the role, `started_at` and `ended_at` of every run
/// of `id`.
pub fn account_run_times(conn: &Connection, id: &str) -> Result<Vec<RunTimes>> {
    let mut stmt = conn
        .prepare("SELECT COALESCE(role, ''), started_at, ended_at FROM runs WHERE account = ?1")?;
    let rows = stmt.query_map([id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// One task as `report`'s `load_tasks` reads it.
pub struct ReportTask {
    pub id: String,
    pub title: String,
    pub status: String,
    pub risk: String,
    pub updated_at: String,
    /// The largest implementer attempt.
    pub attempts: Option<i64>,
    /// The verdict of the newest review of any of the task's runs.
    pub verdict: Option<String>,
    /// The task's newest run (largest run id): run id, profile, model,
    /// interrupted reason, and whether it has a session to resume.
    pub newest_run: Option<(String, String, String, Option<String>, bool)>,
}

/// `report`'s `load_tasks`: every task, each with its newest run. The
/// newest-run statement is prepared once for the whole list.
pub fn report_tasks(conn: &Connection) -> Result<Vec<ReportTask>> {
    let mut stmt = conn.prepare(
        "SELECT t.task_id, COALESCE(t.title, ''), COALESCE(t.status, ''), COALESCE(t.risk, ''),
                COALESCE(t.updated_at, ''),
                (SELECT MAX(r.attempt) FROM runs r
                  WHERE r.task_id = t.task_id AND r.role = 'implementer'),
                (SELECT v.verdict FROM reviews v JOIN runs r ON r.run_id = v.target_run_id
                  WHERE r.task_id = t.task_id ORDER BY v.created_at DESC LIMIT 1)
         FROM tasks t",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ReportTask {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            risk: r.get(3)?,
            updated_at: r.get(4)?,
            attempts: r.get(5)?,
            verdict: r.get(6)?,
            newest_run: None,
        })
    })?;
    let mut tasks = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut newest = conn.prepare(
        "SELECT run_id, COALESCE(account, ''), COALESCE(model, ''), interrupted_reason,
                harness_session_id IS NOT NULL
         FROM runs WHERE task_id = ?1 ORDER BY run_id DESC LIMIT 1",
    )?;
    for t in &mut tasks {
        t.newest_run = newest
            .query_row([&t.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
    }
    Ok(tasks)
}

/// A review's task id, task title, verdict, findings and `created_at`, each
/// empty when NULL (the verdict excepted).
pub type FindingRow = (String, String, String, String, String);

/// `report`'s `load_findings`: every REJECT or UNVERIFIED review.
pub fn rejected_reviews(conn: &Connection) -> Result<Vec<FindingRow>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(r.task_id, ''), COALESCE(t.title, ''), v.verdict,
                COALESCE(v.findings, ''), COALESCE(v.created_at, '')
         FROM reviews v
         JOIN runs r ON r.run_id = v.target_run_id
         LEFT JOIN tasks t ON t.task_id = r.task_id
         WHERE v.verdict IN ('REJECT', 'UNVERIFIED')",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ------------------------------------------------------------ live quota

/// The live quota readers (`read_agy_windows`, `read_codex_live`,
/// `read_claude_live`): walks the profile's `live_quota` events newest
/// first, handing each (`ts`, `detail`) to `take`, and returns the first
/// value `take` gives back. Rows after that one are not read, so a later
/// row that fails to read is no error; `None` when no row was taken.
pub fn newest_live_quota<T>(
    conn: &Connection,
    profile_id: &str,
    mut take: impl FnMut(String, String) -> Option<T>,
) -> Result<Option<T>> {
    let mut statement = conn.prepare("SELECT ts, detail FROM quota_events WHERE profile_id=?1 AND kind='live_quota' ORDER BY seq DESC")?;
    let rows = statement.query_map([profile_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (observed_at, text) = row?;
        if let Some(value) = take(observed_at, text) {
            return Ok(Some(value));
        }
    }
    Ok(None)
}

// ------------------------------------------------------------ run quota

/// `run_quota`'s `claude_readings`: the detail of each of the run's own
/// `rate_limit_warning` events, in `seq` order.
pub fn run_rate_limit_warning_details(conn: &Connection, run_id: &str) -> Result<Vec<Option<String>>> {
    let mut stmt =
        conn.prepare("SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq")?;
    let details = stmt
        .query_map([run_id, event_type::RATE_LIMIT_WARNING], |r| {
            r.get::<_, Option<String>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(details)
}

/// `run_quota`'s `overlapping_runs`: the run id, `started_at` and `ended_at`
/// of every other run of `account` that has started.
pub fn other_started_runs(
    conn: &Connection,
    account: &str,
    run_id: &str,
) -> Result<Vec<(String, String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, started_at, ended_at FROM runs
         WHERE account = ?1 AND run_id != ?2 AND started_at IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([account, run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ------------------------------------------------------------ task history, retro

/// One run as `task_history` reads it: the verdict is the newest review of
/// the run (of the run as reviewer for role `reviewer`, as target otherwise).
/// `attempt` reads 0 when NULL; profile, provider, model and status are empty
/// when NULL; `spec` and `verdict` stay optional.
pub struct NewestRun {
    pub run_id: String,
    pub attempt: i64,
    pub profile: String,
    pub provider: String,
    pub model: String,
    pub spec: Option<String>,
    pub status: String,
    pub verdict: Option<String>,
}

/// `task_history`'s `newest_run`: the newest run of `role` of the task, or
/// `None` when there is none.
pub fn newest_run_of_role(conn: &Connection, task_id: &str, role: &str) -> Result<Option<NewestRun>> {
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
            Ok(NewestRun {
                run_id: r.get::<_, String>(0)?,
                attempt: r.get::<_, Option<i64>>(1)?.unwrap_or(0),
                profile: r.get::<_, String>(2)?,
                provider: r.get::<_, String>(3)?,
                model: r.get::<_, String>(4)?,
                spec: r.get::<_, Option<String>>(5)?,
                status: r.get::<_, String>(6)?,
                verdict: r.get::<_, Option<String>>(7)?,
            })
        })
        .optional()?;
    Ok(row)
}

/// One review of a target run with its task, as `retro` reads it; the text
/// columns are empty when NULL.
pub struct ReviewWithTask {
    pub review_id: String,
    pub created_at: String,
    pub findings: String,
    pub task_id: String,
    pub title: String,
}

/// `retro`'s `load_reviews`: every review joined to its target run and task.
pub fn review_rows_with_task(conn: &Connection) -> Result<Vec<ReviewWithTask>> {
    let mut stmt = conn.prepare(
        "SELECT v.review_id, COALESCE(v.created_at, ''), COALESCE(v.findings, ''),
                COALESCE(r.task_id, ''), COALESCE(t.title, '')
         FROM reviews v
         JOIN runs r ON r.run_id = v.target_run_id
         LEFT JOIN tasks t ON t.task_id = r.task_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ReviewWithTask {
            review_id: r.get(0)?,
            created_at: r.get(1)?,
            findings: r.get(2)?,
            task_id: r.get(3)?,
            title: r.get(4)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// MEAS-1: one review with its task and the attempt of its target run, as
/// `retro` reads them to tell which records had a REJECT to mark.
pub struct ReviewAttempt {
    pub review_id: String,
    pub task_id: String,
    /// `None` when the target run is not found.
    pub attempt: Option<i64>,
    pub verdict: String,
}

/// MEAS-1: every review in age order (the order `list_reviews_for_task`
/// gives a task's reviews), with its task (through its target run, or its
/// reviewer run when the target is empty) and its target run's attempt.
pub fn review_attempts(conn: &Connection) -> Result<Vec<ReviewAttempt>> {
    let mut stmt = conn.prepare(
        "SELECT v.review_id, COALESCE(ru.task_id, ''), t.attempt, COALESCE(v.verdict, '')
         FROM reviews v
         JOIN runs ru ON ru.run_id = COALESCE(NULLIF(v.target_run_id, ''), v.reviewer_run_id)
         LEFT JOIN runs t ON t.run_id = v.target_run_id
         ORDER BY v.created_at, v.review_id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(ReviewAttempt {
            review_id: r.get(0)?,
            task_id: r.get(1)?,
            attempt: r.get(2)?,
            verdict: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

// ------------------------------------------------------------ records

/// `records`' `latest_event`: the detail of the run's newest event of `kind`.
/// A missing row and a NULL detail read as `None`; a database error is an
/// error (the caller decides to swallow it).
pub fn latest_event(conn: &Connection, run_id: &str, kind: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT detail FROM events WHERE run_id=?1 AND type=?2 ORDER BY seq DESC LIMIT 1",
            [run_id, kind],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// `records`' `latest_verdict`: the verdict of the newest review where the
/// run is the reviewer (`role` is `reviewer`) or the target (any other role).
/// A missing row and a NULL verdict read as `None`; a database error is an
/// error. (Not `latest_verdict`: that name is the `models` read above.)
pub fn latest_verdict_of_role(conn: &Connection, role: &str, run_id: &str) -> Result<Option<String>> {
    let column = if role == "reviewer" { "reviewer_run_id" } else { "target_run_id" };
    Ok(conn
        .query_row(
            &format!(
                "SELECT verdict FROM reviews WHERE {column}=?1 ORDER BY created_at DESC, review_id DESC LIMIT 1"
            ),
            [run_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

/// `records`' `collect_counted`: the `root_path` of a registered repository
/// (global database); `None` for an unknown repository or a NULL path.
pub fn repo_root_path(global: &Connection, repo_id: &str) -> Result<Option<String>> {
    Ok(global
        .query_row("SELECT root_path FROM repos WHERE repo_id=?1", [repo_id], |r| {
            r.get::<_, Option<String>>(0)
        })
        .optional()?
        .flatten())
}

/// One `runs` row left-joined with its task, as `records` reads it: the 23
/// columns in the order of the statement; every one may be NULL. The token
/// kinds a repository database from before DT-1 does not have read as `None`.
pub struct RunWithTask {
    pub run_id: Option<String>,
    pub task_id: Option<String>,
    pub attempt: Option<i64>,
    pub role: Option<String>,
    pub provider: Option<String>,
    pub account: Option<String>,
    pub model: Option<String>,
    pub spec: Option<String>,
    pub status: Option<String>,
    pub interrupted_reason: Option<String>,
    pub exit_code: Option<i64>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub input_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub reasoning_tokens: Option<i64>,
    pub total_tokens: Option<i64>,
    /// `t.task_id`: `None` when the run has no task row.
    pub joined_task_id: Option<String>,
    pub task_title: Option<String>,
    pub task_spec: Option<String>,
    pub task_risk: Option<String>,
}

/// `records`' `collect_counted`: every run of the repository with its task.
/// A token-kind column the database does not have yet reads as NULL (asked
/// once per call). The rows are read before the caller processes the first.
pub fn runs_with_tasks(conn: &Connection) -> Result<Vec<RunWithTask>> {
    let col = |name: &str| {
        if super::migrations::column_exists(conn, "runs", name).unwrap_or(false) {
            format!("r.{name}")
        } else {
            "NULL".to_string()
        }
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT r.run_id, r.task_id, r.attempt, r.role, r.provider, r.account, r.model,
                    r.spec, r.status, r.interrupted_reason, r.exit_code, r.started_at, r.ended_at,
                    r.input_tokens, {}, {}, r.output_tokens, {}, {},
                    t.task_id, t.title, t.spec, t.risk
             FROM runs r LEFT JOIN tasks t ON t.task_id=r.task_id",
        col("cache_read_tokens"),
        col("cache_write_tokens"),
        col("reasoning_tokens"),
        col("total_tokens"),
    ))?;
    let rows = stmt.query_map([], |r| {
        Ok(RunWithTask {
            run_id: r.get(0)?,
            task_id: r.get(1)?,
            attempt: r.get(2)?,
            role: r.get(3)?,
            provider: r.get(4)?,
            account: r.get(5)?,
            model: r.get(6)?,
            spec: r.get(7)?,
            status: r.get(8)?,
            interrupted_reason: r.get(9)?,
            exit_code: r.get(10)?,
            started_at: r.get(11)?,
            ended_at: r.get(12)?,
            input_tokens: r.get(13)?,
            cache_read_tokens: r.get(14)?,
            cache_write_tokens: r.get(15)?,
            output_tokens: r.get(16)?,
            reasoning_tokens: r.get(17)?,
            total_tokens: r.get(18)?,
            joined_task_id: r.get(19)?,
            task_title: r.get(20)?,
            task_spec: r.get(21)?,
            task_risk: r.get(22)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}
