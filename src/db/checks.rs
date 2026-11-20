//! Data-layer access to the `checks` and `check_approvals` tables.
//!
//! `crate::checks` re-exports the public functions to preserve existing paths.

use crate::checks::{Approval, Check, Fingerprint};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};

const COLS: &str = "check_id, shell, cmd, cwd, inputs, created_at, approved_at, \
                    approved_fingerprint, approved_detail, approved_commit";

fn row_to_check(r: &rusqlite::Row<'_>) -> rusqlite::Result<Check> {
    let inputs: String = r.get(4)?;
    Ok(Check {
        check_id: r.get(0)?,
        shell: r.get(1)?,
        cmd: r.get(2)?,
        cwd: r.get(3)?,
        inputs: serde_json::from_str(&inputs).unwrap_or_default(),
        created_at: r.get(5)?,
        approved_at: r.get(6)?,
        approved_fingerprint: r.get(7)?,
        approved_detail: r.get(8)?,
        approved_commit: r.get(9)?,
        exe_pin: r.get(10)?,
    })
}

/// The `exe_pin` column, or the literal `'hash'` on a v3 table that has not
/// been migrated yet (`thoth doctor` and `thoth setup` read state.db without
/// migrating it).
fn exe_pin_col(conn: &Connection) -> Result<&'static str> {
    Ok(if crate::db::migrations::column_exists(conn, "checks", "exe_pin")? {
        "exe_pin"
    } else {
        "'hash'"
    })
}

pub fn get_check(conn: &Connection, id: &str) -> Result<Option<Check>> {
    let pin = exe_pin_col(conn)?;
    let sql = format!("SELECT {COLS}, {pin} FROM checks WHERE check_id = ?1");
    Ok(conn.query_row(&sql, [id], row_to_check).optional()?)
}

pub fn list_checks(conn: &Connection) -> Result<Vec<Check>> {
    let pin = exe_pin_col(conn)?;
    let sql = format!("SELECT {COLS}, {pin} FROM checks ORDER BY check_id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], row_to_check)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Whether `check_approvals` exists (state.db v6); `thoth doctor` and `thoth
/// setup` read an older state.db without migrating it.
fn approvals_table(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='check_approvals'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Every approval of `check_id`, newest first. Without `check_approvals`,
/// the `checks` row's own approval (if any) as one `migrated` approval.
pub fn approvals(conn: &Connection, check_id: &str) -> Result<Vec<Approval>> {
    if !approvals_table(conn)? {
        let row = get_check(conn, check_id)?;
        return Ok(row
            .and_then(|c| {
                Some(Approval {
                    approved_at: c.approved_at?,
                    fingerprint: c.approved_fingerprint?,
                    detail: c.approved_detail,
                    approved_commit: c.approved_commit,
                    source: "migrated".to_string(),
                    run_id: None,
                })
            })
            .into_iter()
            .collect());
    }
    let mut stmt = conn.prepare(
        "SELECT approved_at, fingerprint, detail, approved_commit, source, run_id \
         FROM check_approvals WHERE check_id = ?1 ORDER BY seq DESC",
    )?;
    let rows = stmt
        .query_map([check_id], |r| {
            Ok(Approval {
                approved_at: r.get(0)?,
                fingerprint: r.get(1)?,
                detail: r.get(2)?,
                approved_commit: r.get(3)?,
                source: r.get(4)?,
                run_id: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Adds one approval of `check_id` and copies it into the `checks` row's
/// `approved_*` columns, which always hold the newest one.
pub fn record_approval(
    conn: &Connection,
    check_id: &str,
    at: &str,
    fp: &Fingerprint,
    commit: &str,
    source: &str,
    run_id: Option<&str>,
) -> Result<()> {
    let detail = serde_json::to_string(&fp.detail)?;
    conn.execute(
        "INSERT INTO check_approvals \
           (check_id, approved_at, fingerprint, detail, approved_commit, source, run_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![check_id, at, fp.hex, detail, commit, source, run_id],
    )?;
    conn.execute(
        "UPDATE checks SET approved_at = ?2, approved_fingerprint = ?3, approved_detail = ?4, \
         approved_commit = ?5 WHERE check_id = ?1",
        params![check_id, at, fp.hex, detail, commit],
    )?;
    Ok(())
}

/// Forgets every approval of `check_id`.
fn clear_approvals(conn: &Connection, check_id: &str) -> Result<()> {
    if approvals_table(conn)? {
        conn.execute("DELETE FROM check_approvals WHERE check_id = ?1", [check_id])?;
    }
    Ok(())
}

/// Inserts, or replaces the whole row - which also forgets every approval,
/// even when the new definition is identical.
pub fn put_check(conn: &Connection, c: &Check) -> Result<()> {
    clear_approvals(conn, &c.check_id)?;
    conn.execute(
        "INSERT OR REPLACE INTO checks (check_id, shell, cmd, cwd, inputs, created_at, exe_pin) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            c.check_id,
            c.shell,
            c.cmd,
            c.cwd,
            serde_json::to_string(&c.inputs)?,
            c.created_at,
            c.exe_pin
        ],
    )?;
    Ok(())
}

/// Deletes the row and every approval of it.
pub fn delete_check(conn: &Connection, id: &str) -> Result<bool> {
    clear_approvals(conn, id)?;
    Ok(conn.execute("DELETE FROM checks WHERE check_id = ?1", [id])? > 0)
}
