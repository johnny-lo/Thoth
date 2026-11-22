//! Data-layer reads and writes of the `pipelines` table (SQL-4), behind
//! `commands::pipeline`.
//!
//! Every function takes the caller's connection (a `Transaction` derefs to
//! it) and never opens a connection or starts, ends or commits a transaction;
//! a database error is returned, never swallowed. The caller keeps the
//! decisions: which handle, which transaction, and what an error means.
//! The table is not a migration: `ensure_table` creates it lazily, and
//! `table_exists` tells a reader whether it was ever created.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

const TABLE: &str = "CREATE TABLE IF NOT EXISTS pipelines (
  pipeline_id       TEXT PRIMARY KEY,
  task_id           TEXT NOT NULL,
  status            TEXT NOT NULL,
  step              TEXT NOT NULL,
  settings          TEXT NOT NULL,
  pipeline_pid      INTEGER,
  heartbeat_at      TEXT,
  started_at        TEXT NOT NULL,
  ended_at          TEXT,
  stop_requested    INTEGER NOT NULL DEFAULT 0,
  stop              TEXT,
  dir               TEXT NOT NULL,
  implementer_index INTEGER NOT NULL DEFAULT 0,
  reviewer_index    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_pipelines_task ON pipelines(task_id);";

const COLS: &str = "pipeline_id, task_id, status, step, settings, pipeline_pid, heartbeat_at, \
                    started_at, ended_at, stop_requested, stop, dir, implementer_index, reviewer_index";

/// One `pipelines` row; `settings` and `stop` are parsed JSON (`settings`
/// reads as `Null` when it does not parse, `stop` as `None`).
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub task_id: String,
    pub status: String,
    pub step: String,
    pub settings: Value,
    pub pid: Option<i64>,
    pub heartbeat_at: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub stop_requested: bool,
    pub stop: Option<Value>,
    pub dir: String,
    pub implementer_index: i64,
    pub reviewer_index: i64,
}

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let settings: String = r.get(4)?;
    let stop: Option<String> = r.get(10)?;
    Ok(Row {
        id: r.get(0)?,
        task_id: r.get(1)?,
        status: r.get(2)?,
        step: r.get(3)?,
        settings: serde_json::from_str(&settings).unwrap_or(Value::Null),
        pid: r.get(5)?,
        heartbeat_at: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
        stop_requested: r.get::<_, i64>(9)? != 0,
        stop: stop.and_then(|s| serde_json::from_str(&s).ok()),
        dir: r.get(11)?,
        implementer_index: r.get(12)?,
        reviewer_index: r.get(13)?,
    })
}

/// Creates the table and its index when missing. Lazy: only a pipeline start
/// calls it, so a repository that never ran one has no `pipelines` table.
pub fn ensure_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(TABLE)?;
    Ok(())
}

/// Whether this state.db has the table; an error is returned, not read as
/// false.
pub fn table_exists(conn: &Connection) -> Result<bool> {
    let n = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pipelines'",
        [],
        |r| r.get::<_, i64>(0),
    )?;
    Ok(n > 0)
}

/// The pipeline `id`, or `None` when there is none.
pub fn get(conn: &Connection, id: &str) -> Result<Option<Row>> {
    Ok(conn
        .query_row(&format!("SELECT {COLS} FROM pipelines WHERE pipeline_id = ?1"), [id], row_of)
        .optional()?)
}

/// The task's newest pipeline (ids are ULIDs, so they sort by start time).
/// The table must exist.
pub fn latest_for_task(conn: &Connection, task_id: &str) -> Result<Option<Row>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM pipelines WHERE task_id = ?1 ORDER BY pipeline_id DESC LIMIT 1"),
            [task_id],
            row_of,
        )
        .optional()?)
}

/// The task's running pipelines, oldest first. The table must exist.
pub fn running_for_task(conn: &Connection, task_id: &str) -> Result<Vec<Row>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM pipelines WHERE task_id = ?1 AND status = 'running' ORDER BY pipeline_id"
    ))?;
    let rows = stmt.query_map([task_id], row_of)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Settles a running pipeline as `stopped` with `stop` (JSON text) at `now`;
/// returns how many rows changed (0 when it had already settled).
pub fn settle_running(conn: &Connection, now: &str, stop: &str, id: &str) -> Result<usize> {
    let n = conn.execute(
        "UPDATE pipelines SET status = 'stopped', ended_at = ?1, stop = ?2 \
         WHERE pipeline_id = ?3 AND status = 'running'",
        params![now, stop, id],
    )?;
    Ok(n)
}

/// Inserts a new pipeline as `running` at step `starting`; `heartbeat_at` and
/// `started_at` are both `now`.
pub fn insert_running(
    conn: &Connection,
    id: &str,
    task_id: &str,
    settings: &str,
    now: &str,
    dir: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pipelines (pipeline_id, task_id, status, step, settings, heartbeat_at, \
         started_at, dir) VALUES (?1, ?2, 'running', 'starting', ?3, ?4, ?4, ?5)",
        params![id, task_id, settings, now, dir],
    )?;
    Ok(())
}

/// Records the pid of the process that runs the pipeline.
pub fn set_pid(conn: &Connection, pid: i64, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pipelines SET pipeline_pid = ?1 WHERE pipeline_id = ?2",
        params![pid, id],
    )?;
    Ok(())
}

/// Asks a running pipeline to stop.
pub fn request_stop(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pipelines SET stop_requested = 1 WHERE pipeline_id = ?1 AND status = 'running'",
        [id],
    )?;
    Ok(())
}

/// Writes the heartbeat of a running pipeline.
pub fn beat(conn: &Connection, now: &str, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pipelines SET heartbeat_at = ?1 WHERE pipeline_id = ?2 AND status = 'running'",
        params![now, id],
    )?;
    Ok(())
}

/// Whether a stop was requested; a missing pipeline is an error.
pub fn stop_requested(conn: &Connection, id: &str) -> Result<bool> {
    let n = conn.query_row(
        "SELECT stop_requested FROM pipelines WHERE pipeline_id = ?1",
        [id],
        |r| r.get::<_, i64>(0),
    )?;
    Ok(n != 0)
}

/// Records the step a running pipeline is on.
pub fn set_step(conn: &Connection, step: &str, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pipelines SET step = ?1 WHERE pipeline_id = ?2 AND status = 'running'",
        params![step, id],
    )?;
    Ok(())
}

/// Records the position in the implementer list.
pub fn set_implementer_index(conn: &Connection, pos: i64, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pipelines SET implementer_index = ?1 WHERE pipeline_id = ?2",
        params![pos, id],
    )?;
    Ok(())
}

/// Records the position in the reviewer list.
pub fn set_reviewer_index(conn: &Connection, pos: i64, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE pipelines SET reviewer_index = ?1 WHERE pipeline_id = ?2",
        params![pos, id],
    )?;
    Ok(())
}
