use crate::ids;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

// Read side of `meta`. `protected::load_patterns` reads the repo's extra
// protected patterns through it.
pub fn get_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
            r.get(0)
        })
        .optional()?)
}

#[derive(Debug, Clone)]
pub struct NewEvent {
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    pub event_type: String,
    pub detail: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    pub seq: i64,
    pub ts: String,
    pub task_id: Option<String>,
    pub run_id: Option<String>,
    pub event_type: String,
    pub detail: String,
}

/// Appends one row and returns its `seq`, which is the Supervisor's cursor.
pub fn append_event(conn: &Connection, e: &NewEvent) -> Result<i64> {
    conn.execute(
        "INSERT INTO events (ts, task_id, run_id, type, detail) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            ids::now_rfc3339(),
            e.task_id,
            e.run_id,
            e.event_type,
            serde_json::to_string(&e.detail)?
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn row_to_event(r: &rusqlite::Row<'_>) -> rusqlite::Result<EventRow> {
    Ok(EventRow {
        seq: r.get(0)?,
        ts: r.get(1)?,
        task_id: r.get(2)?,
        run_id: r.get(3)?,
        event_type: r.get(4)?,
        detail: r.get(5)?,
    })
}

/// The newest event of `event_type` (highest `seq`), optionally of one task.
pub fn latest_event_of_type(
    conn: &Connection,
    event_type: &str,
    task_id: Option<&str>,
) -> Result<Option<EventRow>> {
    use rusqlite::OptionalExtension;
    let row = conn
        .query_row(
            "SELECT seq, ts, task_id, run_id, type, detail FROM events
             WHERE type = ?1 AND (?2 IS NULL OR task_id = ?2) ORDER BY seq DESC LIMIT 1",
            params![event_type, task_id],
            row_to_event,
        )
        .optional()?;
    Ok(row)
}

/// Strictly `seq > since`, ascending, so a caller can poll with the last seq it saw.
pub fn events_since(conn: &Connection, since: i64, task_id: Option<&str>) -> Result<Vec<EventRow>> {
    let rows = match task_id {
        Some(t) => {
            let mut stmt = conn.prepare(
                "SELECT seq, ts, task_id, run_id, type, detail FROM events
                 WHERE seq > ?1 AND task_id = ?2 ORDER BY seq",
            )?;
            let rows = stmt
                .query_map(params![since, t], row_to_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
        None => {
            let mut stmt = conn.prepare(
                "SELECT seq, ts, task_id, run_id, type, detail FROM events
                 WHERE seq > ?1 ORDER BY seq",
            )?;
            let rows = stmt
                .query_map(params![since], row_to_event)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        }
    };
    Ok(rows)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub task_id: String,
    pub title: String,
    pub spec: String,
    pub risk: String,
    pub status: String,
    pub base_commit: String,
    pub created_at: String,
    pub updated_at: String,
}

const TASK_COLS: &str = "task_id, title, spec, risk, status, base_commit, created_at, updated_at";

fn row_to_task(r: &rusqlite::Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        task_id: r.get(0)?,
        title: r.get(1)?,
        spec: r.get(2)?,
        risk: r.get(3)?,
        status: r.get(4)?,
        base_commit: r.get(5)?,
        created_at: r.get(6)?,
        updated_at: r.get(7)?,
    })
}

pub fn insert_task(conn: &Connection, t: &Task) -> Result<()> {
    let sql = format!("INSERT INTO tasks ({TASK_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)");
    conn.execute(
        &sql,
        params![
            t.task_id, t.title, t.spec, t.risk, t.status, t.base_commit, t.created_at, t.updated_at
        ],
    )?;
    Ok(())
}

pub fn get_task(conn: &Connection, id: &str) -> Result<Option<Task>> {
    let sql = format!("SELECT {TASK_COLS} FROM tasks WHERE task_id = ?1");
    Ok(conn.query_row(&sql, params![id], row_to_task).optional()?)
}

pub fn list_task_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT task_id FROM tasks ORDER BY task_id")?;
    let ids = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(ids)
}

pub fn list_tasks(conn: &Connection) -> Result<Vec<Task>> {
    let sql = format!("SELECT {TASK_COLS} FROM tasks ORDER BY task_id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], row_to_task)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Writes the new status and appends a `status_change` event in the same call so
/// the two can never drift.
pub fn set_task_status(
    conn: &Connection,
    task_id: &str,
    status: crate::model::TaskStatus,
    now: &str,
) -> Result<()> {
    let before: Option<String> = conn
        .query_row(
            "SELECT status FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )
        .optional()?;
    conn.execute(
        "UPDATE tasks SET status = ?2, updated_at = ?3 WHERE task_id = ?1",
        params![task_id, status.as_str(), now],
    )?;
    if before.as_deref() != Some(status.as_str()) {
        append_event(
            conn,
            &NewEvent {
                task_id: Some(task_id.to_string()),
                run_id: None,
                event_type: crate::model::event_type::STATUS_CHANGE.to_string(),
                detail: serde_json::json!({
                    "entity": "task",
                    "id": task_id,
                    "from": before,
                    "to": status.as_str(),
                }),
            },
        )?;
    }
    Ok(())
}

/// Conditional version of `set_task_status`: the write only applies while the
/// task's current status is one of `allowed`. Whole-branch review Important 1
/// found that every state gate used to read the status, decide, and only
/// then write unconditionally inside a transaction opened after the read, so
/// two concurrent callers could both pass the same gate and both commit.
/// Folding the check into the `UPDATE`'s `WHERE` clause makes the decision
/// and the write atomic with respect to any other writer using this same
/// helper (or SQLite's own transaction serialisation), because rusqlite's
/// row count is read from the same statement that performed the write.
///
/// Returns `true` if the row was updated (and, if the status actually
/// changed, a `status_change` event was appended); `false` if the task's
/// status was not in `allowed` by the time this ran - the caller must treat
/// that as "the state changed underneath me" and not proceed with whatever it
/// was about to do next.
pub fn transition_task_status(
    conn: &Connection,
    task_id: &str,
    allowed: &[crate::model::TaskStatus],
    to: crate::model::TaskStatus,
    now: &str,
) -> Result<bool> {
    if allowed.is_empty() {
        return Ok(false);
    }
    let before: Option<String> = conn
        .query_row(
            "SELECT status FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| r.get(0),
        )
        .optional()?;

    let to_str = to.as_str();
    let allowed_strs: Vec<&str> = allowed.iter().map(|s| s.as_str()).collect();
    let placeholders: Vec<String> = (0..allowed_strs.len())
        .map(|i| format!("?{}", i + 4))
        .collect();
    let sql = format!(
        "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE task_id = ?3 AND status IN ({})",
        placeholders.join(",")
    );
    let mut bind_params: Vec<&dyn rusqlite::ToSql> = vec![&to_str, &now, &task_id];
    for s in &allowed_strs {
        bind_params.push(s);
    }
    let rows = conn.execute(&sql, bind_params.as_slice())?;
    let applied = rows == 1;
    if applied && before.as_deref() != Some(to_str) {
        append_event(
            conn,
            &NewEvent {
                task_id: Some(task_id.to_string()),
                run_id: None,
                event_type: crate::model::event_type::STATUS_CHANGE.to_string(),
                detail: serde_json::json!({
                    "entity": "task",
                    "id": task_id,
                    "from": before,
                    "to": to_str,
                }),
            },
        )?;
    }
    Ok(applied)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub run_id: String,
    pub task_id: String,
    pub attempt: i64,
    pub role: String,
    pub provider: String,
    /// The profile id. v6 section 10.5 names this column `account`.
    pub account: String,
    pub model: String,
    pub harness_session_id: Option<String>,
    /// Resume is bound to the config dir (01-claude-cli-identity.md:130), so the
    /// run records the one it was launched with.
    pub config_dir: String,
    pub allowed_tools: String,
    pub worktree: String,
    pub base_commit: String,
    pub candidate_commit: Option<String>,
    pub spec: String,
    pub status: String,
    pub interrupted_reason: Option<String>,
    pub exit_code: Option<i64>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub runner_pid: Option<i64>,
    pub worker_pid: Option<i64>,
    pub heartbeat_at: Option<String>,
    pub cancel_requested: i64,
    pub timeout_seconds: i64,
    pub run_dir: String,
    pub error_text: Option<String>,
    /// The harness executable this run was launched with, recorded at dispatch
    /// or resume time. Phase 1's final review (Important 3,
    /// docs/phase1/process/final-review.md:481-482) found that `cancel` had to
    /// guess the worker's image name from the *cancelling* shell's
    /// `THOTH_CLAUDE_BIN`, which silently mismatched whenever the two
    /// shells disagreed. `NULL` on every row Phase 1 wrote, so readers must
    /// keep the old fallback.
    pub harness_bin: Option<String>,
}

const RUN_COLS: &str = "run_id, task_id, attempt, role, provider, account, model, \
     harness_session_id, config_dir, allowed_tools, worktree, base_commit, candidate_commit, \
     spec, status, interrupted_reason, exit_code, input_tokens, output_tokens, started_at, \
     ended_at, runner_pid, worker_pid, heartbeat_at, cancel_requested, timeout_seconds, \
     run_dir, error_text, harness_bin";

fn row_to_run(r: &rusqlite::Row<'_>) -> rusqlite::Result<Run> {
    Ok(Run {
        run_id: r.get(0)?,
        task_id: r.get(1)?,
        attempt: r.get(2)?,
        role: r.get(3)?,
        provider: r.get(4)?,
        account: r.get(5)?,
        model: r.get(6)?,
        harness_session_id: r.get(7)?,
        config_dir: r.get(8)?,
        allowed_tools: r.get(9)?,
        worktree: r.get(10)?,
        base_commit: r.get(11)?,
        candidate_commit: r.get(12)?,
        spec: r.get(13)?,
        status: r.get(14)?,
        interrupted_reason: r.get(15)?,
        exit_code: r.get(16)?,
        input_tokens: r.get(17)?,
        output_tokens: r.get(18)?,
        started_at: r.get(19)?,
        ended_at: r.get(20)?,
        runner_pid: r.get(21)?,
        worker_pid: r.get(22)?,
        heartbeat_at: r.get(23)?,
        cancel_requested: r.get(24)?,
        timeout_seconds: r.get(25)?,
        run_dir: r.get(26)?,
        error_text: r.get(27)?,
        harness_bin: r.get(28)?,
    })
}

pub fn insert_run(conn: &Connection, r: &Run) -> Result<()> {
    let sql = format!(
        "INSERT INTO runs ({RUN_COLS}) VALUES \
         (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29)"
    );
    conn.execute(
        &sql,
        params![
            r.run_id, r.task_id, r.attempt, r.role, r.provider, r.account, r.model,
            r.harness_session_id, r.config_dir, r.allowed_tools, r.worktree, r.base_commit,
            r.candidate_commit, r.spec, r.status, r.interrupted_reason, r.exit_code,
            r.input_tokens, r.output_tokens, r.started_at, r.ended_at, r.runner_pid,
            r.worker_pid, r.heartbeat_at, r.cancel_requested, r.timeout_seconds, r.run_dir,
            r.error_text, r.harness_bin
        ],
    )?;
    Ok(())
}

pub fn get_run(conn: &Connection, run_id: &str) -> Result<Option<Run>> {
    let sql = format!("SELECT {RUN_COLS} FROM runs WHERE run_id = ?1");
    Ok(conn.query_row(&sql, params![run_id], row_to_run).optional()?)
}

pub fn list_run_ids(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT run_id FROM runs ORDER BY run_id")?;
    let ids = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(ids)
}

pub fn list_runs_for_task(conn: &Connection, task_id: &str) -> Result<Vec<Run>> {
    let sql = format!("SELECT {RUN_COLS} FROM runs WHERE task_id = ?1 ORDER BY run_id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![task_id], row_to_run)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Every run of `task_id` that is still live - one nobody has settled, i.e.
/// `runs.status` is `queued` or `running`.
///
/// Whole-branch review Important 1/3/5: four commands (`dispatch`, `resume`,
/// `submit`, `review start`) have to refuse while another run of the same
/// task is still in flight, and each used to answer "is one live?" its own
/// way (or not at all). The set is defined once, in `RunStatus::is_terminal`,
/// and reached here through `runner::is_run_status_terminal` - the same
/// predicate the runner and `verify` already use - so a future non-terminal
/// run status is live for every one of those guards automatically instead of
/// slipping through a hardcoded `"queued" | "running"` match.
///
/// Callers must call this **inside** the write transaction that performs the
/// write it protects (see `db::write_tx`), immediately before that write;
/// read outside one, the answer is stale the moment it is returned.
pub fn list_live_runs_for_task(conn: &Connection, task_id: &str) -> Result<Vec<Run>> {
    Ok(list_runs_for_task(conn, task_id)?
        .into_iter()
        .filter(|r| !crate::runner::is_run_status_terminal(&r.status))
        .collect())
}

pub fn max_attempt(conn: &Connection, task_id: &str) -> Result<Option<i64>> {
    Ok(conn.query_row(
        "SELECT MAX(attempt) FROM runs WHERE task_id = ?1",
        params![task_id],
        |r| r.get::<_, Option<i64>>(0),
    )?)
}

/// `started_at` is only written once. A non-null `started_at` is what tells the
/// runner "this run has already had a session, so resume it" (plan Task 8).
/// M8: gated on `status = 'queued'` so two runners racing to claim the same
/// run (a hand double-launch of `run-worker`, or a resume raced by two
/// processes) cannot both think they won. Returns `false` - and writes
/// nothing - when the row was not `queued` at the moment of the update; the
/// caller (already holding a live spawned child in the M8 claim path) is
/// responsible for treating that as "someone else got there first" rather
/// than proceeding to run a worker nobody will supervise as the row of
/// record.
pub fn mark_run_started(conn: &Connection, run_id: &str, worker_pid: i64, now: &str) -> Result<bool> {
    let rows = conn.execute(
        "UPDATE runs SET status = 'running', worker_pid = ?2, heartbeat_at = ?3,
         started_at = COALESCE(started_at, ?3) WHERE run_id = ?1 AND status = 'queued'",
        params![run_id, worker_pid, now],
    )?;
    Ok(rows == 1)
}

/// FC-2: the runner relaunched the harness in the same run (a retry after a
/// transient failure); `cancel` kills the worker this names.
pub fn set_worker_pid(conn: &Connection, run_id: &str, worker_pid: i64) -> Result<()> {
    conn.execute(
        "UPDATE runs SET worker_pid = ?2 WHERE run_id = ?1",
        params![run_id, worker_pid],
    )?;
    Ok(())
}

/// FC-2: for the runner's fresh retry only. Overwrites `harness_session_id`
/// unconditionally - `None` clears it - unlike `set_run_session_and_usage`,
/// whose COALESCE keeps the old id when given `None`. A fresh retry has to
/// drop the failed launch's session so the new launch's own id (or none) is
/// what `resume` later continues.
pub fn reset_run_session(conn: &Connection, run_id: &str, session_id: Option<&str>) -> Result<()> {
    conn.execute(
        "UPDATE runs SET harness_session_id = ?2 WHERE run_id = ?1",
        params![run_id, session_id],
    )?;
    Ok(())
}

pub fn set_runner_pid(conn: &Connection, run_id: &str, pid: i64) -> Result<()> {
    conn.execute(
        "UPDATE runs SET runner_pid = ?2 WHERE run_id = ?1",
        params![run_id, pid],
    )?;
    Ok(())
}

pub fn touch_heartbeat(conn: &Connection, run_id: &str, now: &str) -> Result<()> {
    conn.execute(
        "UPDATE runs SET heartbeat_at = ?2 WHERE run_id = ?1",
        params![run_id, now],
    )?;
    Ok(())
}

pub fn read_cancel_requested(conn: &Connection, run_id: &str) -> Result<bool> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT cancel_requested FROM runs WHERE run_id = ?1",
            params![run_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.unwrap_or(0) != 0)
}

pub fn request_cancel(conn: &Connection, run_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE runs SET cancel_requested = 1 WHERE run_id = ?1",
        params![run_id],
    )?;
    Ok(())
}

pub fn set_run_session_and_usage(
    conn: &Connection,
    run_id: &str,
    session_id: Option<&str>,
    input: Option<i64>,
    output: Option<i64>,
) -> Result<()> {
    conn.execute(
        "UPDATE runs SET
           harness_session_id = COALESCE(?2, harness_session_id),
           input_tokens       = COALESCE(?3, input_tokens),
           output_tokens      = COALESCE(?4, output_tokens)
         WHERE run_id = ?1",
        params![run_id, session_id, input, output],
    )?;
    Ok(())
}

/// The token kinds a run reported (DT-1). Every field is `None` until the
/// harness reported that kind; `total` never counts a token twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RunTokens {
    pub input: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub output: Option<i64>,
    pub reasoning: Option<i64>,
    pub total: Option<i64>,
}

/// Folds one result's usage into the run's token columns.
///
/// `cumulative` false (Claude): the result covers only the turn that
/// produced it, so each kind it reports is added to what the run already
/// has. `cumulative` true (codex, antigravity): the result covers the whole
/// thread or conversation so far, so each kind it reports replaces the old
/// value. Either way a kind the result does not report leaves the column
/// alone.
pub fn record_run_tokens(
    conn: &Connection,
    run_id: &str,
    t: &RunTokens,
    cumulative: bool,
) -> Result<()> {
    const COLUMNS: [&str; 6] = [
        "input_tokens",
        "cache_read_tokens",
        "cache_write_tokens",
        "output_tokens",
        "reasoning_tokens",
        "total_tokens",
    ];
    let set = COLUMNS
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let n = i + 2;
            if cumulative {
                format!("{c} = COALESCE(?{n}, {c})")
            } else {
                format!(
                    "{c} = CASE WHEN ?{n} IS NULL THEN {c} ELSE COALESCE({c}, 0) + ?{n} END"
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute(
        &format!("UPDATE runs SET {set} WHERE run_id = ?1"),
        params![
            run_id,
            t.input,
            t.cache_read,
            t.cache_write,
            t.output,
            t.reasoning,
            t.total
        ],
    )?;
    Ok(())
}

/// The run's token columns; `None` when there is no such run.
// Read by `run_tokens_add_for_a_per_turn_usage_and_replace_for_a_cumulative_one`
// (u5) and the DT-1 integration tests through SQL; no production caller yet.
#[allow(dead_code)]
pub fn run_tokens(conn: &Connection, run_id: &str) -> Result<Option<RunTokens>> {
    Ok(conn
        .query_row(
            "SELECT input_tokens, cache_read_tokens, cache_write_tokens, output_tokens,
                    reasoning_tokens, total_tokens FROM runs WHERE run_id = ?1",
            params![run_id],
            |r| {
                Ok(RunTokens {
                    input: r.get(0)?,
                    cache_read: r.get(1)?,
                    cache_write: r.get(2)?,
                    output: r.get(3)?,
                    reasoning: r.get(4)?,
                    total: r.get(5)?,
                })
            },
        )
        .optional()?)
}

#[derive(Debug, Clone)]
pub struct RunFinish {
    pub status: crate::model::RunStatus,
    pub interrupted_reason: Option<crate::model::InterruptedReason>,
    pub exit_code: Option<i64>,
    pub candidate_commit: Option<String>,
    pub error_text: Option<String>,
    pub ended_at: String,
}

/// Settles a run to a terminal state, but only while it is still non-terminal
/// (`queued`/`running`). Whole-branch review Important 1: the runner's own
/// settle tail and `cancel`'s escalation settle can race - a stale heartbeat
/// can make `cancel` decide to escalate while the runner is, at that very
/// moment, legitimately finishing the same run - and an unconditional write
/// here let the loser silently overwrite the winner's outcome. Returns
/// whether the write applied; `false` means somebody else already settled
/// this run first, and the caller must not treat its own finish as having
/// taken effect (in particular, must not also overwrite `tasks.status`).
pub fn finish_run(conn: &Connection, run_id: &str, f: &RunFinish) -> Result<bool> {
    let rows = conn.execute(
        "UPDATE runs SET status = ?2, interrupted_reason = ?3, exit_code = ?4,
         candidate_commit = ?5, error_text = ?6, ended_at = ?7
         WHERE run_id = ?1 AND status IN ('queued', 'running')",
        params![
            run_id,
            f.status.as_str(),
            f.interrupted_reason.map(|r| r.as_str()),
            f.exit_code,
            f.candidate_commit,
            f.error_text,
            f.ended_at
        ],
    )?;
    Ok(rows == 1)
}

/// Put an interrupted run back in the queue for `thoth resume`, keeping
/// `started_at` and `harness_session_id` so the runner picks the resume path.
pub fn prepare_resume(conn: &Connection, run_id: &str) -> Result<()> {
    conn.execute(
        "UPDATE runs SET status = 'queued', interrupted_reason = NULL, ended_at = NULL,
         error_text = NULL, exit_code = NULL, cancel_requested = 0 WHERE run_id = ?1",
        params![run_id],
    )?;
    Ok(())
}

/// Read side of `claims`. `ingest::ingest_with_evidence`'s re-ingest merge (#32/M4,
/// final review) is the first production consumer; also read directly by
/// `claims_insert_and_list_per_run` and by the integration tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub claim_id: String,
    pub run_id: String,
    pub kind: String,
    pub text: String,
}

pub fn insert_claim(conn: &Connection, claim_id: &str, run_id: &str, kind: &str, text: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO claims (claim_id, run_id, kind, text) VALUES (?1, ?2, ?3, ?4)",
        params![claim_id, run_id, kind, text],
    )?;
    Ok(())
}

/// Deletes exactly one claim by id. `ingest::ingest_with_evidence`'s re-ingest merge
/// (#32/M4, final review) uses this for a claim whose (kind, text) no longer
/// appears in the freshly ingested set, instead of `delete_claims_for_run`'s
/// clear-everything approach, so a claim that is still present keeps its
/// `claim_id` across repeated ingests.
pub fn delete_claim(conn: &Connection, claim_id: &str) -> Result<()> {
    conn.execute("DELETE FROM claims WHERE claim_id = ?1", params![claim_id])?;
    Ok(())
}

/// Clears every claim recorded for `run_id`. No longer called by
/// `ingest::ingest_with_evidence` (#32/M4 replaced the old delete-all-then-reinsert
/// re-ingest with a (kind, text) merge that keeps stable claim_ids via
/// `delete_claim`); kept as a general "wipe a run's claims" primitive.
#[allow(dead_code)]
pub fn delete_claims_for_run(conn: &Connection, run_id: &str) -> Result<()> {
    conn.execute("DELETE FROM claims WHERE run_id = ?1", params![run_id])?;
    Ok(())
}

pub fn list_claims_for_run(conn: &Connection, run_id: &str) -> Result<Vec<Claim>> {
    let mut stmt = conn.prepare(
        "SELECT claim_id, run_id, kind, text FROM claims WHERE run_id = ?1 ORDER BY claim_id",
    )?;
    let rows = stmt
        .query_map(params![run_id], |r| {
            Ok(Claim {
                claim_id: r.get(0)?,
                run_id: r.get(1)?,
                kind: r.get(2)?,
                text: r.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn set_run_harness_bin(conn: &Connection, run_id: &str, bin: &str) -> Result<()> {
    conn.execute(
        "UPDATE runs SET harness_bin = ?2 WHERE run_id = ?1",
        params![run_id, bin],
    )?;
    Ok(())
}

/// Newest `completed` run of one role for a task. `review start` uses it to
/// find the implementer run it is reviewing, so the ordering matters: the
/// highest attempt wins, and within one attempt the newest run id (ULIDs sort
/// by creation time).
// Retired in Task 7: production caller is `commands::review::start`; also read
// by `latest_completed_run_for_role_picks_the_newest_attempt`.
pub fn latest_completed_run_for_role(
    conn: &Connection,
    task_id: &str,
    role: &str,
) -> Result<Option<Run>> {
    let sql = format!(
        "SELECT {RUN_COLS} FROM runs WHERE task_id = ?1 AND role = ?2 AND status = 'completed'
         ORDER BY attempt DESC, run_id DESC LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, params![task_id, role], row_to_run)
        .optional()?)
}

/// `task update-spec` rewrites both the document and the risk it declares, so
/// the two can never disagree.
///
/// Conditional on the task's status, for the same reason
/// `transition_task_status` is (whole-branch review Important 1, and R1 of
/// Task 8's fix round 1): the caller reads the status, decides it may edit,
/// and only then opens the transaction. A `dispatch` flipping `reject ->
/// running` in that gap would otherwise let the spec be rewritten under a live
/// worker - and `review start` reads `tasks.spec` for its criteria, so the next
/// review would judge new criteria against work done for the old ones.
///
/// Returns `true` if the row was written, `false` if the task's status was not
/// in `allowed` by the time this ran; the caller must treat `false` as "the
/// state changed underneath me".
// Retired in Task 8: production caller is `commands::task::update_spec`; also
// read by `harness_bin_and_attempt_cap_and_spec_writes`.
pub fn set_task_spec(
    conn: &Connection,
    task_id: &str,
    allowed: &[crate::model::TaskStatus],
    spec: &str,
    risk: &str,
    now: &str,
) -> Result<bool> {
    if allowed.is_empty() {
        return Ok(false);
    }
    let allowed_strs: Vec<&str> = allowed.iter().map(|s| s.as_str()).collect();
    let placeholders: Vec<String> = (0..allowed_strs.len())
        .map(|i| format!("?{}", i + 5))
        .collect();
    let sql = format!(
        "UPDATE tasks SET spec = ?2, risk = ?3, updated_at = ?4
         WHERE task_id = ?1 AND status IN ({})",
        placeholders.join(",")
    );
    let mut bind: Vec<&dyn rusqlite::ToSql> = vec![&task_id, &spec, &risk, &now];
    for s in &allowed_strs {
        bind.push(s);
    }
    Ok(conn.execute(&sql, bind.as_slice())? == 1)
}

/// The task's rework attempt cap. A task row written before the v2 migration
/// reads the column default; a task id that does not exist at all falls back
/// to the same default rather than erroring, because the caller is about to
/// fail on the missing task for a better reason.
// Retired in Task 8: production caller is `commands::dispatch::run`'s rework
// attempt-cap check; also read by `harness_bin_and_attempt_cap_and_spec_writes`.
pub fn get_task_attempt_cap(conn: &Connection, task_id: &str) -> Result<i64> {
    let v: Option<Option<i64>> = conn
        .query_row(
            "SELECT attempt_cap FROM tasks WHERE task_id = ?1",
            params![task_id],
            |r| r.get::<_, Option<i64>>(0),
        )
        .optional()?;
    Ok(v.flatten().unwrap_or(crate::model::DEFAULT_ATTEMPT_CAP))
}

/// v6 §10.5's `evidence` row. `code` is the per-task display number
/// (`E001`...), `collected_by` is the trust marker §2.4 turns into policy, and
/// `blob_path` plus `sha256` are what §2.5 rule 4 re-checks at ingest time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    pub evidence_id: String,
    pub code: String,
    pub task_id: String,
    pub run_id: Option<String>,
    pub kind: String,
    pub collected_by: String,
    pub label: String,
    pub sha256: Option<String>,
    pub bytes: Option<i64>,
    pub blob_path: Option<String>,
    pub detail: String,
    pub created_at: String,
}

const EVIDENCE_COLS: &str = "evidence_id, code, task_id, run_id, kind, collected_by, label, \
     sha256, bytes, blob_path, detail, created_at";

fn row_to_evidence(r: &rusqlite::Row<'_>) -> rusqlite::Result<Evidence> {
    Ok(Evidence {
        evidence_id: r.get(0)?,
        code: r.get(1)?,
        task_id: r.get(2)?,
        run_id: r.get(3)?,
        kind: r.get(4)?,
        collected_by: r.get(5)?,
        label: r.get(6)?,
        sha256: r.get(7)?,
        bytes: r.get(8)?,
        blob_path: r.get(9)?,
        detail: r.get(10)?,
        created_at: r.get(11)?,
    })
}

// Retired in Task 3: production caller is `evidence::insert_row`; also read
// by `evidence_rows_round_trip_and_list_in_code_order` and other Task 1
// evidence tests.
pub fn insert_evidence(conn: &Connection, e: &Evidence) -> Result<()> {
    let sql = format!(
        "INSERT INTO evidence ({EVIDENCE_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)"
    );
    conn.execute(
        &sql,
        params![
            e.evidence_id, e.code, e.task_id, e.run_id, e.kind, e.collected_by, e.label,
            e.sha256, e.bytes, e.blob_path, e.detail, e.created_at
        ],
    )?;
    Ok(())
}

/// Ordered by code *number*, not by string: `ORDER BY code` alone would put
/// `E1000` before `E999`, and the evidence index the worker reads must be in
/// the order the codes were allocated.
// Retired in Task 3: production caller is `evidence::refresh_index`; also
// read by `evidence_rows_round_trip_and_list_in_code_order`. (Task 6's
// `evidence` listing becomes a second caller.)
pub fn list_evidence_for_task(conn: &Connection, task_id: &str) -> Result<Vec<Evidence>> {
    let sql = format!(
        "SELECT {EVIDENCE_COLS} FROM evidence WHERE task_id = ?1 ORDER BY LENGTH(code), code"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![task_id], row_to_evidence)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// Retired in Task 5: the first production caller is `ingest_with_evidence`'s
// citation resolution (rule 3), ahead of `evidence show --task` in Task 6;
// also read by `evidence_rows_round_trip_and_list_in_code_order`.
pub fn get_evidence_by_code(
    conn: &Connection,
    task_id: &str,
    code: &str,
) -> Result<Option<Evidence>> {
    let sql = format!("SELECT {EVIDENCE_COLS} FROM evidence WHERE task_id = ?1 AND code = ?2");
    Ok(conn
        .query_row(&sql, params![task_id, code], row_to_evidence)
        .optional()?)
}

// Codes are per task, so a bare code can match more than one. Read by
// `find_evidence_by_code_spans_tasks`; production caller is
// `commands::evidence::resolve_task_for_code`'s scan fallback (Task 6, Open
// question 14).
pub fn find_evidence_by_code(conn: &Connection, code: &str) -> Result<Vec<Evidence>> {
    let sql =
        format!("SELECT {EVIDENCE_COLS} FROM evidence WHERE code = ?1 ORDER BY task_id, created_at");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![code], row_to_evidence)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// Retired in Task 5: production caller is `ingest_with_evidence`'s linking
// pass; also read by `claim_evidence_links_round_trip_and_clear_per_run`.
pub fn insert_claim_evidence(conn: &Connection, claim_id: &str, evidence_id: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO claim_evidence (claim_id, evidence_id) VALUES (?1, ?2)",
        params![claim_id, evidence_id],
    )?;
    Ok(())
}

/// Drops every link belonging to a run's claims. `ingest_with_evidence` runs
/// this before it inserts or deletes any claim, so a re-ingest can never leave
/// a `claim_evidence` row pointing at a claim that no longer exists (Open
/// question 2) - the lookup joins through `claims.run_id`, so a link whose
/// claim was deleted first would be unreachable.
// Retired in Task 5: production caller is `ingest_with_evidence`'s re-ingest
// merge; also read by `claim_evidence_links_round_trip_and_clear_per_run`.
pub fn delete_claim_evidence_for_run(conn: &Connection, run_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM claim_evidence
         WHERE claim_id IN (SELECT claim_id FROM claims WHERE run_id = ?1)",
        params![run_id],
    )?;
    Ok(())
}

// Retired in Task 7: production caller is `commands::review::show`'s claim
// table; also read by `claim_evidence_links_round_trip_and_clear_per_run` and
// by Task 5's `a_valid_result_writes_claims_and_a_result_ingested_event` /
// `m4_...`.
pub fn list_evidence_for_claim(conn: &Connection, claim_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn
        .prepare("SELECT evidence_id FROM claim_evidence WHERE claim_id = ?1 ORDER BY evidence_id")?;
    let rows = stmt
        .query_map(params![claim_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// v6 §10.5's `reviews` row: one per review, pointing at the implementer run
/// it judged. `findings` is the JSON document from §10.5's comment block.
///
/// `reviewer_run_id` is `NULL` for an external review recorded by
/// `thoth review record` (thin-adapter Task 1) — the commander that judged
/// it is a Claude Code session outside thoth, and there is no `runs` row to
/// point at. A run row means a thoth-launched process; a placeholder one
/// would be a lie in the `runs` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Review {
    pub review_id: String,
    pub target_run_id: String,
    pub reviewer_run_id: Option<String>,
    pub verdict: String,
    pub findings: String,
    pub created_at: String,
}

const REVIEW_COLS: &str =
    "review_id, target_run_id, reviewer_run_id, verdict, findings, created_at";

fn row_to_review(r: &rusqlite::Row<'_>) -> rusqlite::Result<Review> {
    Ok(Review {
        review_id: r.get(0)?,
        target_run_id: r.get(1)?,
        reviewer_run_id: r.get(2)?,
        verdict: r.get(3)?,
        findings: r.get(4)?,
        created_at: r.get(5)?,
    })
}

// Retired in Task 7: production caller is `commands::review::ingest_review`;
// also read by `reviews_round_trip_per_run_and_per_task`.
pub fn insert_review(conn: &Connection, r: &Review) -> Result<()> {
    let sql =
        format!("INSERT INTO reviews ({REVIEW_COLS}) VALUES (?1,?2,?3,?4,?5,?6)");
    conn.execute(
        &sql,
        params![
            r.review_id, r.target_run_id, r.reviewer_run_id, r.verdict, r.findings, r.created_at
        ],
    )?;
    Ok(())
}

// Retired in Task 7: production caller is `commands::wait::for_run`'s verdict
// lookup; also read by `reviews_round_trip_per_run_and_per_task`.
pub fn get_review_for_run(conn: &Connection, reviewer_run_id: &str) -> Result<Option<Review>> {
    let sql = format!(
        "SELECT {REVIEW_COLS} FROM reviews WHERE reviewer_run_id = ?1
         ORDER BY created_at DESC, review_id DESC LIMIT 1"
    );
    Ok(conn
        .query_row(&sql, params![reviewer_run_id], row_to_review)
        .optional()?)
}

/// Every review recorded for a task, oldest first. Joined through
/// `target_run_id` rather than `reviewer_run_id`: every review (Thoth's own or
/// an external one recorded by `review record`) points at the implementer run
/// it judged, and that run always belongs to the task. Joining through
/// `reviewer_run_id` instead would silently drop an external review, whose
/// `reviewer_run_id` is NULL (thin-adapter Task 1) — breaking `review show`,
/// `status`'s verdict line, and `dispatch`'s REJECT fold, all three of which
/// call this.
///
/// thin-adapter Task 1 fix round 1 (F2), corrected fix round 2 (R4):
/// `COALESCE(NULLIF(r.target_run_id, ''), r.reviewer_run_id)` also falls back
/// to the reviewer run when `target_run_id` is an empty string, rather than
/// only ever reading `target_run_id` verbatim. `runner.rs`'s degraded path (a
/// `review_started` event that could not be read back) names exactly this
/// shape; see the comment there for why, given `reviews.target_run_id
/// REFERENCES runs` and this build's on-by-default foreign key enforcement,
/// that path in fact never gets as far as *persisting* a row with an empty
/// `target_run_id` through thoth's own code. The FK has been in place
/// since Phase 1's `STATE_V1`, so no thoth-written database ever lacked
/// it - the fallback is defensive against a row an *outside* tool wrote with
/// foreign keys off (e.g. the `sqlite3` CLI, where they default off), not
/// against an older schema.
// Retired in Task 7: production callers are `commands::review::show` and
// `status <task>`'s latest-verdict line; also read by
// `reviews_round_trip_per_run_and_per_task`.
pub fn list_reviews_for_task(conn: &Connection, task_id: &str) -> Result<Vec<Review>> {
    let sql = "SELECT r.review_id, r.target_run_id, r.reviewer_run_id, r.verdict, r.findings,
                r.created_at
         FROM reviews r
         JOIN runs ru ON ru.run_id = COALESCE(NULLIF(r.target_run_id, ''), r.reviewer_run_id)
         WHERE ru.task_id = ?1 ORDER BY r.created_at, r.review_id";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params![task_id], row_to_review)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        conn
    }

    fn demo_run(run_id: &str, task_id: &str) -> Run {
        Run {
            run_id: run_id.into(),
            task_id: task_id.into(),
            attempt: 1,
            role: "implementer".into(),
            provider: "anthropic".into(),
            account: "claude-b".into(),
            model: "sonnet".into(),
            harness_session_id: Some("11111111-2222-3333-4444-555555555555".into()),
            config_dir: "C:/cfg/claude-b".into(),
            allowed_tools: "Bash,Read".into(),
            worktree: "C:/wt/a1".into(),
            base_commit: "abc123".into(),
            candidate_commit: None,
            spec: "{}".into(),
            status: "queued".into(),
            interrupted_reason: None,
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
            run_dir: "C:/ah/runs/R1".into(),
            error_text: None,
            harness_bin: None,
        }
    }

    /// `runs.task_id` is a foreign key onto `tasks`, and this build's bundled
    /// SQLite enforces foreign keys by default (no `PRAGMA foreign_keys = ON`
    /// needed), so every `demo_run` needs a real parent row first.
    fn insert_demo_task(conn: &Connection, task_id: &str) {
        insert_task(
            conn,
            &Task {
                task_id: task_id.into(),
                title: "t".into(),
                spec: "{}".into(),
                risk: "normal".into(),
                status: "queued".into(),
                base_commit: "abc123".into(),
                created_at: "2026-09-14T00:00:00Z".into(),
                updated_at: "2026-09-14T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    // DT-1 u5: results add up (Claude) or replace (codex, agy).
    #[test]
    fn run_tokens_add_for_a_per_turn_usage_and_replace_for_a_cumulative_one() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        insert_run(&conn, &demo_run("R2", "T1")).unwrap();
        insert_run(&conn, &demo_run("R3", "T1")).unwrap();
        assert_eq!(run_tokens(&conn, "R1").unwrap(), Some(RunTokens::default()));

        let turn = RunTokens {
            input: Some(34),
            output: Some(610),
            cache_read: Some(202655),
            cache_write: Some(1840),
            reasoning: Some(226),
            total: Some(205139),
        };
        record_run_tokens(&conn, "R1", &turn, false).unwrap();
        record_run_tokens(&conn, "R1", &turn, false).unwrap();
        let both = RunTokens {
            input: Some(68),
            output: Some(1220),
            cache_read: Some(405310),
            cache_write: Some(3680),
            reasoning: Some(452),
            total: Some(410278),
        };
        assert_eq!(run_tokens(&conn, "R1").unwrap(), Some(both));
        // a result that reports nothing changes nothing
        record_run_tokens(&conn, "R1", &RunTokens::default(), false).unwrap();
        assert_eq!(run_tokens(&conn, "R1").unwrap(), Some(both));

        // replacing: the later total wins, kinds it leaves out stay as they were
        let first = RunTokens { output: Some(157), ..Default::default() };
        record_run_tokens(&conn, "R2", &first, true).unwrap();
        let second = RunTokens { output: Some(213), input: Some(22534), ..Default::default() };
        record_run_tokens(&conn, "R2", &second, true).unwrap();
        let expected = RunTokens { output: Some(213), input: Some(22534), ..Default::default() };
        assert_eq!(run_tokens(&conn, "R2").unwrap(), Some(expected));
        record_run_tokens(&conn, "R2", &RunTokens::default(), true).unwrap();
        assert_eq!(run_tokens(&conn, "R2").unwrap(), Some(expected));

        assert_eq!(run_tokens(&conn, "NONE").unwrap(), None);
    }

    #[test]
    fn run_rows_round_trip_through_every_lifecycle_write() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        let r = get_run(&conn, "R1").unwrap().unwrap();
        assert_eq!(r.status, "queued");
        assert_eq!(r.timeout_seconds, 1800);
        assert_eq!(r.cancel_requested, 0);
        assert!(r.started_at.is_none(), "a queued run has not started");

        set_runner_pid(&conn, "R1", 4242).unwrap();
        assert!(mark_run_started(&conn, "R1", 777, "2026-09-14T00:00:00Z").unwrap());
        let r = get_run(&conn, "R1").unwrap().unwrap();
        assert_eq!(r.status, "running");
        assert_eq!(r.runner_pid, Some(4242));
        assert_eq!(r.worker_pid, Some(777));
        assert_eq!(r.started_at.as_deref(), Some("2026-09-14T00:00:00Z"));
        assert_eq!(r.heartbeat_at.as_deref(), Some("2026-09-14T00:00:00Z"));

        // a second start (resume) must not overwrite the original started_at.
        // `mark_run_started` only claims a `queued` row, so put the row back
        // there first, exactly as `prepare_resume` does for a real resume.
        prepare_resume(&conn, "R1").unwrap();
        assert!(mark_run_started(&conn, "R1", 888, "2026-09-14T01:00:00Z").unwrap());
        let r = get_run(&conn, "R1").unwrap().unwrap();
        assert_eq!(r.started_at.as_deref(), Some("2026-09-14T00:00:00Z"));
        assert_eq!(r.worker_pid, Some(888));

        touch_heartbeat(&conn, "R1", "2026-09-14T02:00:00Z").unwrap();
        assert_eq!(
            get_run(&conn, "R1").unwrap().unwrap().heartbeat_at.as_deref(),
            Some("2026-09-14T02:00:00Z")
        );

        assert!(!read_cancel_requested(&conn, "R1").unwrap());
        request_cancel(&conn, "R1").unwrap();
        assert!(read_cancel_requested(&conn, "R1").unwrap());

        set_run_session_and_usage(&conn, "R1", Some("sess-9"), Some(34), Some(610)).unwrap();
        let r = get_run(&conn, "R1").unwrap().unwrap();
        assert_eq!(r.harness_session_id.as_deref(), Some("sess-9"));
        assert_eq!(r.input_tokens, Some(34));
        assert_eq!(r.output_tokens, Some(610));

        finish_run(
            &conn,
            "R1",
            &RunFinish {
                status: crate::model::RunStatus::Completed,
                interrupted_reason: None,
                exit_code: Some(0),
                candidate_commit: Some("def456".into()),
                error_text: None,
                ended_at: "2026-09-14T03:00:00Z".into(),
            },
        )
        .unwrap();
        let r = get_run(&conn, "R1").unwrap().unwrap();
        assert_eq!(r.status, "completed");
        assert_eq!(r.exit_code, Some(0));
        assert_eq!(r.candidate_commit.as_deref(), Some("def456"));
        assert_eq!(r.ended_at.as_deref(), Some("2026-09-14T03:00:00Z"));
    }

    // FC-2 AC3.
    #[test]
    fn set_worker_pid_and_reset_run_session_overwrite_what_mark_run_started_and_coalesce_leave() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &Run { harness_session_id: None, ..demo_run("R1", "T1") }).unwrap();
        assert!(mark_run_started(&conn, "R1", 777, "2026-09-14T00:00:00Z").unwrap());
        assert_eq!(get_run(&conn, "R1").unwrap().unwrap().worker_pid, Some(777));

        set_worker_pid(&conn, "R1", 999).unwrap();
        assert_eq!(get_run(&conn, "R1").unwrap().unwrap().worker_pid, Some(999));

        let session = |conn: &Connection| get_run(conn, "R1").unwrap().unwrap().harness_session_id;
        set_run_session_and_usage(&conn, "R1", Some("a"), None, None).unwrap();
        assert_eq!(session(&conn).as_deref(), Some("a"));
        // COALESCE(?2, harness_session_id): `None` keeps the old id, so this
        // function can never clear it. (A `Some` replaces it.)
        set_run_session_and_usage(&conn, "R1", None, None, None).unwrap();
        assert_eq!(session(&conn).as_deref(), Some("a"));
        reset_run_session(&conn, "R1", None).unwrap();
        assert_eq!(session(&conn), None);
        set_run_session_and_usage(&conn, "R1", Some("c"), None, None).unwrap();
        assert_eq!(session(&conn).as_deref(), Some("c"));
        reset_run_session(&conn, "R1", Some("d")).unwrap();
        assert_eq!(session(&conn).as_deref(), Some("d"));
    }

    #[test]
    fn mark_run_started_refuses_a_run_that_is_not_queued() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        assert!(mark_run_started(&conn, "R1", 111, "2026-09-14T00:00:00Z").unwrap());

        // The row is now `running`, not `queued` - a second claim (the M8
        // race this gate exists for: two runners racing the same run) must
        // be refused and must not touch the row at all.
        assert!(!mark_run_started(&conn, "R1", 222, "2026-09-14T00:05:00Z").unwrap());
        let r = get_run(&conn, "R1").unwrap().unwrap();
        assert_eq!(
            r.worker_pid,
            Some(111),
            "a refused claim must not overwrite the winner's worker_pid"
        );
        assert_eq!(r.heartbeat_at.as_deref(), Some("2026-09-14T00:00:00Z"));
    }

    #[test]
    fn attempt_and_listing_helpers_work() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_demo_task(&conn, "T2");
        assert_eq!(max_attempt(&conn, "T1").unwrap(), None);
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        let mut second = demo_run("R2", "T1");
        second.attempt = 2;
        insert_run(&conn, &second).unwrap();
        insert_run(&conn, &demo_run("R3", "T2")).unwrap();
        assert_eq!(max_attempt(&conn, "T1").unwrap(), Some(2));
        assert_eq!(list_run_ids(&conn).unwrap().len(), 3);
        assert_eq!(list_runs_for_task(&conn, "T1").unwrap().len(), 2);
    }

    #[test]
    fn claims_insert_and_list_per_run() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        insert_claim(&conn, "C1", "R1", "test_result", "suite green").unwrap();
        insert_claim(&conn, "C2", "R1", "completion", "all criteria met").unwrap();
        let claims = list_claims_for_run(&conn, "R1").unwrap();
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].kind, "test_result");
        assert_eq!(claims[1].text, "all criteria met");
    }

    #[test]
    fn prepare_resume_clears_the_interruption_but_keeps_started_at() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        mark_run_started(&conn, "R1", 1, "2026-09-14T00:00:00Z").unwrap();
        finish_run(
            &conn,
            "R1",
            &RunFinish {
                status: crate::model::RunStatus::Interrupted,
                interrupted_reason: Some(crate::model::InterruptedReason::Quota),
                exit_code: Some(1),
                candidate_commit: Some("c1".into()),
                error_text: Some("rate limit".into()),
                ended_at: "2026-09-14T01:00:00Z".into(),
            },
        )
        .unwrap();
        request_cancel(&conn, "R1").unwrap();

        prepare_resume(&conn, "R1").unwrap();
        let r = get_run(&conn, "R1").unwrap().unwrap();
        assert_eq!(r.status, "queued");
        assert_eq!(r.interrupted_reason, None);
        assert_eq!(r.ended_at, None);
        assert_eq!(r.error_text, None);
        assert_eq!(r.cancel_requested, 0);
        assert_eq!(
            r.started_at.as_deref(),
            Some("2026-09-14T00:00:00Z"),
            "started_at is the flag that says 'use --resume'"
        );
    }

    #[test]
    fn meta_round_trips_and_overwrites() {
        let conn = db();
        assert_eq!(get_meta(&conn, "repo_root").unwrap(), None);
        set_meta(&conn, "repo_root", "C:/a").unwrap();
        assert_eq!(get_meta(&conn, "repo_root").unwrap().as_deref(), Some("C:/a"));
        set_meta(&conn, "repo_root", "C:/b").unwrap();
        assert_eq!(get_meta(&conn, "repo_root").unwrap().as_deref(), Some("C:/b"));
    }

    #[test]
    fn events_get_monotonic_seqs_and_filter_by_task() {
        let conn = db();
        let s1 = append_event(
            &conn,
            &NewEvent {
                task_id: Some("T1".into()),
                run_id: None,
                event_type: "task_created".into(),
                detail: serde_json::json!({"title": "one"}),
            },
        )
        .unwrap();
        let s2 = append_event(
            &conn,
            &NewEvent {
                task_id: Some("T2".into()),
                run_id: None,
                event_type: "task_created".into(),
                detail: serde_json::json!({"title": "two"}),
            },
        )
        .unwrap();
        let s3 = append_event(
            &conn,
            &NewEvent {
                task_id: Some("T1".into()),
                run_id: Some("R1".into()),
                event_type: "run_started".into(),
                detail: serde_json::json!({"worker_pid": 42}),
            },
        )
        .unwrap();
        assert!(s1 < s2 && s2 < s3, "{s1} {s2} {s3}");

        let all = events_since(&conn, 0, None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[0].seq, s1);
        assert!(all[0].ts.ends_with('Z'));

        let after_first = events_since(&conn, s1, None).unwrap();
        assert_eq!(after_first.len(), 2);

        let only_t1 = events_since(&conn, 0, Some("T1")).unwrap();
        assert_eq!(only_t1.len(), 2);
        assert_eq!(only_t1[1].event_type, "run_started");
        assert_eq!(only_t1[1].run_id.as_deref(), Some("R1"));
        assert!(only_t1[1].detail.contains("\"worker_pid\":42"));
    }

    #[test]
    fn set_task_status_records_one_status_change_and_skips_no_ops() {
        let conn = db();
        insert_task(
            &conn,
            &Task {
                task_id: "T1".into(),
                title: "t".into(),
                spec: "{}".into(),
                risk: "normal".into(),
                status: "queued".into(),
                base_commit: "abc".into(),
                created_at: "now".into(),
                updated_at: "now".into(),
            },
        )
        .unwrap();

        set_task_status(&conn, "T1", crate::model::TaskStatus::Running, "t1").unwrap();
        set_task_status(&conn, "T1", crate::model::TaskStatus::Running, "t2").unwrap();
        let evs = events_since(&conn, 0, Some("T1")).unwrap();
        assert_eq!(evs.len(), 1, "a repeat status must not emit a second event");
        assert!(evs[0].detail.contains("\"from\":\"queued\""));
        assert!(evs[0].detail.contains("\"to\":\"running\""));
        assert_eq!(
            get_task(&conn, "T1").unwrap().unwrap().status,
            "running"
        );
        assert_eq!(list_task_ids(&conn).unwrap(), vec!["T1".to_string()]);
    }

    /// Whole-branch review Important 1: `transition_task_status` is the
    /// conditional replacement for the check-then-act pattern in `dispatch`.
    /// The allowed set must be honoured (a status inside it applies) and a
    /// status outside it must report zero rows affected rather than writing
    /// anyway.
    #[test]
    fn transition_task_status_honours_the_allowed_set() {
        use crate::model::TaskStatus;

        let conn = db();
        insert_task(
            &conn,
            &Task {
                task_id: "T1".into(),
                title: "t".into(),
                spec: "{}".into(),
                risk: "normal".into(),
                status: "queued".into(),
                base_commit: "abc".into(),
                created_at: "now".into(),
                updated_at: "now".into(),
            },
        )
        .unwrap();

        // "queued" is in the allowed set: applies, and records the event.
        let applied = transition_task_status(
            &conn,
            "T1",
            &[TaskStatus::Queued, TaskStatus::Interrupted],
            TaskStatus::Running,
            "t1",
        )
        .unwrap();
        assert!(applied, "queued should be a permitted starting status");
        assert_eq!(get_task(&conn, "T1").unwrap().unwrap().status, "running");
        let evs = events_since(&conn, 0, Some("T1")).unwrap();
        assert_eq!(evs.len(), 1);

        // The task is now "running", which is not in the allowed set below:
        // this simulates a second concurrent caller that read a stale
        // "queued"/"interrupted" status and lost the race.
        let applied_again = transition_task_status(
            &conn,
            "T1",
            &[TaskStatus::Queued, TaskStatus::Interrupted],
            TaskStatus::Running,
            "t2",
        )
        .unwrap();
        assert!(!applied_again, "running is outside the allowed set; must be refused");
        // No second event, and the status/updated_at from the first call stand.
        let evs = events_since(&conn, 0, Some("T1")).unwrap();
        assert_eq!(evs.len(), 1, "a refused transition must not write anything");
        assert_eq!(get_task(&conn, "T1").unwrap().unwrap().status, "running");

        // An empty allowed set can never apply.
        let applied_empty =
            transition_task_status(&conn, "T1", &[], TaskStatus::Cancelled, "t3").unwrap();
        assert!(!applied_empty);
    }

    /// Whole-branch review Important 1: settling the same run twice must be
    /// safe. The first settle stands; the second is a no-op that reports
    /// `false` rather than overwriting the first outcome (the race this
    /// guards is `cancel`'s escalation settling over a run the runner just
    /// finished, or vice versa).
    #[test]
    fn finish_run_twice_is_a_no_op_the_second_time_and_the_first_outcome_stands() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();

        let first = RunFinish {
            status: crate::model::RunStatus::Completed,
            interrupted_reason: None,
            exit_code: Some(0),
            candidate_commit: Some("first-commit".into()),
            error_text: None,
            ended_at: "2026-09-15T00:00:00Z".into(),
        };
        assert!(finish_run(&conn, "R1", &first).unwrap(), "first settle must apply");

        let second = RunFinish {
            status: crate::model::RunStatus::Cancelled,
            interrupted_reason: Some(crate::model::InterruptedReason::User),
            exit_code: None,
            candidate_commit: Some("second-commit".into()),
            error_text: Some("cancelled after the fact".into()),
            ended_at: "2026-09-15T00:05:00Z".into(),
        };
        assert!(
            !finish_run(&conn, "R1", &second).unwrap(),
            "second settle must be a no-op: the run is already terminal"
        );

        let r = get_run(&conn, "R1").unwrap().unwrap();
        assert_eq!(r.status, "completed", "the first outcome must stand");
        assert_eq!(r.candidate_commit.as_deref(), Some("first-commit"));
        assert_eq!(r.ended_at.as_deref(), Some("2026-09-15T00:00:00Z"));
        assert_eq!(r.error_text, None);
    }

    fn demo_evidence(code: &str, task_id: &str, collected_by: &str) -> Evidence {
        Evidence {
            evidence_id: format!("EV_{code}"),
            code: code.into(),
            task_id: task_id.into(),
            run_id: Some("R1".into()),
            kind: "command".into(),
            collected_by: collected_by.into(),
            label: format!("cargo test {code}"),
            sha256: Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into()),
            bytes: Some(5),
            blob_path: Some("C:/ah/repos/r1/blobs/2cf24dba".into()),
            detail: serde_json::json!({"cmd": "cargo test", "exit_code": 0}).to_string(),
            created_at: "2026-09-16T00:00:00Z".into(),
        }
    }

    #[test]
    fn evidence_rows_round_trip_and_list_in_code_order() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();

        // inserted out of order, and with a code past the zero padding, so the
        // ordering assertion below is about numbers rather than about strings
        for code in ["E010", "E002", "E001", "E1000"] {
            insert_evidence(&conn, &demo_evidence(code, "T1", "control_plane")).unwrap();
        }
        let rows = list_evidence_for_task(&conn, "T1").unwrap();
        let codes: Vec<&str> = rows.iter().map(|e| e.code.as_str()).collect();
        assert_eq!(codes, vec!["E001", "E002", "E010", "E1000"]);
        assert_eq!(rows[0].kind, "command");
        assert_eq!(rows[0].collected_by, "control_plane");
        assert_eq!(rows[0].bytes, Some(5));

        let one = get_evidence_by_code(&conn, "T1", "E002").unwrap().unwrap();
        assert_eq!(one.evidence_id, "EV_E002");
        assert!(get_evidence_by_code(&conn, "T1", "E999").unwrap().is_none());
        assert!(get_evidence_by_code(&conn, "T2", "E002").unwrap().is_none());
    }

    #[test]
    fn find_evidence_by_code_spans_tasks() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_demo_task(&conn, "T2");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        insert_evidence(&conn, &demo_evidence("E001", "T1", "control_plane")).unwrap();
        let mut second = demo_evidence("E001", "T2", "worker");
        second.evidence_id = "EV_OTHER".into();
        second.run_id = None;
        insert_evidence(&conn, &second).unwrap();

        let hits = find_evidence_by_code(&conn, "E001").unwrap();
        assert_eq!(hits.len(), 2);
        let mut tasks: Vec<&str> = hits.iter().map(|e| e.task_id.as_str()).collect();
        tasks.sort();
        assert_eq!(tasks, vec!["T1", "T2"]);
        assert!(find_evidence_by_code(&conn, "E777").unwrap().is_empty());
    }

    #[test]
    fn claim_evidence_links_round_trip_and_clear_per_run() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        insert_run(&conn, &demo_run("R2", "T1")).unwrap();
        insert_claim(&conn, "C1", "R1", "test_result", "green").unwrap();
        insert_claim(&conn, "C2", "R2", "test_result", "also green").unwrap();
        insert_evidence(&conn, &demo_evidence("E001", "T1", "control_plane")).unwrap();
        insert_evidence(&conn, &demo_evidence("E002", "T1", "control_plane")).unwrap();

        insert_claim_evidence(&conn, "C1", "EV_E001").unwrap();
        insert_claim_evidence(&conn, "C1", "EV_E002").unwrap();
        insert_claim_evidence(&conn, "C2", "EV_E001").unwrap();
        let mut linked = list_evidence_for_claim(&conn, "C1").unwrap();
        linked.sort();
        assert_eq!(linked, vec!["EV_E001".to_string(), "EV_E002".to_string()]);

        // Clearing by run must take only that run's links with it (Open
        // question 2: a re-ingest deletes the run's claims, so its links have
        // to go first or they would point at rows that no longer exist).
        delete_claim_evidence_for_run(&conn, "R1").unwrap();
        assert!(list_evidence_for_claim(&conn, "C1").unwrap().is_empty());
        assert_eq!(list_evidence_for_claim(&conn, "C2").unwrap().len(), 1);
    }

    #[test]
    fn reviews_round_trip_per_run_and_per_task() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_demo_task(&conn, "T2");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        let mut reviewer = demo_run("R2", "T1");
        reviewer.role = "reviewer".into();
        insert_run(&conn, &reviewer).unwrap();
        insert_run(&conn, &demo_run("R3", "T2")).unwrap();

        insert_review(
            &conn,
            &Review {
                review_id: "RV1".into(),
                target_run_id: "R1".into(),
                reviewer_run_id: Some("R2".into()),
                verdict: "REJECT".into(),
                findings: serde_json::json!({"criteria": [{"id": "AC1", "verdict": "unmet"}]})
                    .to_string(),
                created_at: "2026-09-16T00:00:00Z".into(),
            },
        )
        .unwrap();

        let got = get_review_for_run(&conn, "R2").unwrap().unwrap();
        assert_eq!(got.verdict, "REJECT");
        assert_eq!(got.target_run_id, "R1");
        assert!(got.findings.contains("\"AC1\""));
        assert!(get_review_for_run(&conn, "R1").unwrap().is_none());

        let for_task = list_reviews_for_task(&conn, "T1").unwrap();
        assert_eq!(for_task.len(), 1);
        assert!(list_reviews_for_task(&conn, "T2").unwrap().is_empty());
    }

    /// Thin-adapter Task 1: `list_reviews_for_task` joins through
    /// `target_run_id`, not `reviewer_run_id`, precisely so a review with a
    /// NULL `reviewer_run_id` (an external review recorded by `thoth
    /// review record`) is not silently dropped. Both a Thoth reviewer-run
    /// review and an external one on the same task must show up, oldest
    /// first, interleaved by creation order rather than separated by kind.
    #[test]
    fn list_reviews_for_task_includes_external_reviews_alongside_reviewer_run_ones() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();
        let mut reviewer = demo_run("R2", "T1");
        reviewer.role = "reviewer".into();
        insert_run(&conn, &reviewer).unwrap();

        insert_review(
            &conn,
            &Review {
                review_id: "RV1".into(),
                target_run_id: "R1".into(),
                reviewer_run_id: Some("R2".into()),
                verdict: "REJECT".into(),
                findings: "{}".into(),
                created_at: "2026-09-24T00:00:00Z".into(),
            },
        )
        .unwrap();
        // The external review: no reviewer run, so `reviewer_run_id` is NULL.
        insert_review(
            &conn,
            &Review {
                review_id: "RV2".into(),
                target_run_id: "R1".into(),
                reviewer_run_id: None,
                verdict: "PASS".into(),
                findings: "{}".into(),
                created_at: "2026-09-24T01:00:00Z".into(),
            },
        )
        .unwrap();

        let for_task = list_reviews_for_task(&conn, "T1").unwrap();
        assert_eq!(for_task.len(), 2, "both reviews must be found: {for_task:?}");
        assert_eq!(for_task[0].review_id, "RV1", "oldest first");
        assert_eq!(for_task[0].reviewer_run_id.as_deref(), Some("R2"));
        assert_eq!(for_task[1].review_id, "RV2");
        assert_eq!(
            for_task[1].reviewer_run_id, None,
            "the external review's reviewer_run_id must round-trip as NULL"
        );
        assert_eq!(for_task[1].verdict, "PASS");
    }

    /// Fix round 1 (F2): a degraded reviewer-run review whose `target_run_id`
    /// is an empty string (`runner.rs`'s fallback when `target_run_id_for`
    /// could not read the `review_started` event back) must still be found,
    /// through its `reviewer_run_id`.
    ///
    /// `reviews.target_run_id REFERENCES runs`, and this build's SQLite
    /// enforces foreign keys by default (see `db::mod`'s `tune`), so an
    /// empty-string `target_run_id` cannot actually be *inserted* through
    /// `insert_review` today - I verified this empirically (it raises
    /// `FOREIGN KEY constraint failed`). Through thoth's own code the
    /// degraded path therefore fails closed into `review_invalid` (stage
    /// "write") rather than landing a row shaped like this one - see
    /// `runner.rs`'s comment on that branch. This test disables the
    /// constraint for its own connection to construct the row directly
    /// anyway: the join fix is correct and worth having regardless, as
    /// defense against a row written by an *outside* tool with foreign keys
    /// off (e.g. the `sqlite3` CLI, where they default off, not thoth
    /// itself - the FK has been in `STATE_V1` since Phase 1), and the brief
    /// asks for it named explicitly.
    #[test]
    fn list_reviews_for_task_finds_a_degraded_review_with_an_empty_target_run_id() {
        let conn = db();
        conn.execute("PRAGMA foreign_keys = OFF", []).unwrap();
        insert_demo_task(&conn, "T1");
        let mut reviewer = demo_run("R2", "T1");
        reviewer.role = "reviewer".into();
        insert_run(&conn, &reviewer).unwrap();

        insert_review(
            &conn,
            &Review {
                review_id: "RV1".into(),
                target_run_id: "".into(),
                reviewer_run_id: Some("R2".into()),
                verdict: "PASS".into(),
                findings: "{}".into(),
                created_at: "2026-09-24T02:00:00Z".into(),
            },
        )
        .unwrap();

        let for_task = list_reviews_for_task(&conn, "T1").unwrap();
        assert_eq!(
            for_task.len(),
            1,
            "the degraded-path review must not vanish: {for_task:?}"
        );
        assert_eq!(for_task[0].review_id, "RV1");
        assert_eq!(for_task[0].target_run_id, "");
        assert_eq!(for_task[0].reviewer_run_id.as_deref(), Some("R2"));
    }

    #[test]
    fn latest_completed_run_for_role_picks_the_newest_attempt() {
        let conn = db();
        insert_demo_task(&conn, "T1");

        let mut a1 = demo_run("R1", "T1");
        a1.status = "completed".into();
        a1.candidate_commit = Some("cand-a1".into());
        insert_run(&conn, &a1).unwrap();

        // a running attempt 2 must not win over the completed attempt 1
        let mut a2 = demo_run("R2", "T1");
        a2.attempt = 2;
        a2.status = "running".into();
        insert_run(&conn, &a2).unwrap();

        // a completed reviewer run on the same task is a different role
        let mut rev = demo_run("R3", "T1");
        rev.role = "reviewer".into();
        rev.status = "completed".into();
        insert_run(&conn, &rev).unwrap();

        let got = latest_completed_run_for_role(&conn, "T1", "implementer")
            .unwrap()
            .unwrap();
        assert_eq!(got.run_id, "R1");
        assert_eq!(got.candidate_commit.as_deref(), Some("cand-a1"));
        assert_eq!(
            latest_completed_run_for_role(&conn, "T1", "reviewer").unwrap().unwrap().run_id,
            "R3"
        );
        assert!(latest_completed_run_for_role(&conn, "T1", "auditor").unwrap().is_none());

        // and once attempt 2 completes, it wins
        let mut a2_done = demo_run("R4", "T1");
        a2_done.attempt = 2;
        a2_done.status = "completed".into();
        insert_run(&conn, &a2_done).unwrap();
        assert_eq!(
            latest_completed_run_for_role(&conn, "T1", "implementer").unwrap().unwrap().run_id,
            "R4"
        );
    }

    #[test]
    fn harness_bin_and_attempt_cap_and_spec_writes() {
        let conn = db();
        insert_demo_task(&conn, "T1");
        insert_run(&conn, &demo_run("R1", "T1")).unwrap();

        // Phase 1 rows have no harness_bin, which is exactly what `cancel` has
        // to cope with (it falls back to the shell's THOTH_CLAUDE_BIN).
        assert_eq!(get_run(&conn, "R1").unwrap().unwrap().harness_bin, None);
        set_run_harness_bin(&conn, "R1", "C:/tools/claude.exe").unwrap();
        assert_eq!(
            get_run(&conn, "R1").unwrap().unwrap().harness_bin.as_deref(),
            Some("C:/tools/claude.exe")
        );

        assert_eq!(get_task_attempt_cap(&conn, "T1").unwrap(), 2);
        conn.execute("UPDATE tasks SET attempt_cap = 5 WHERE task_id = 'T1'", []).unwrap();
        assert_eq!(get_task_attempt_cap(&conn, "T1").unwrap(), 5);
        // an unknown task falls back to the default rather than erroring
        assert_eq!(get_task_attempt_cap(&conn, "NOPE").unwrap(), 2);

        // R1 (fix round 1): the write is conditional on the status, so the
        // demo task's `queued` has to be in the allowed set for it to apply.
        assert!(set_task_spec(
            &conn,
            "T1",
            &[crate::model::TaskStatus::Queued],
            r#"{"goal":"new goal","acceptance_criteria":["AC1: x"],"risk":"high"}"#,
            "high",
            "2026-09-16T01:00:00Z",
        )
        .unwrap());
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert!(t.spec.contains("new goal"));
        assert_eq!(t.risk, "high");
        assert_eq!(t.updated_at, "2026-09-16T01:00:00Z");

        // ...and a status outside the allowed set writes nothing at all
        assert!(!set_task_spec(
            &conn,
            "T1",
            &[crate::model::TaskStatus::Draft],
            r#"{"goal":"must not land","acceptance_criteria":["AC1: x"],"risk":"low"}"#,
            "low",
            "2026-09-16T02:00:00Z",
        )
        .unwrap());
        let t = get_task(&conn, "T1").unwrap().unwrap();
        assert!(t.spec.contains("new goal"), "the refused write must not apply");
        assert_eq!(t.risk, "high");
        assert_eq!(t.updated_at, "2026-09-16T01:00:00Z");
    }
}
