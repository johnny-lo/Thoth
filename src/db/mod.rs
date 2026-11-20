pub mod checks;
pub mod global;
pub mod migrations;
pub mod reports;
pub mod state;

use anyhow::{Context, Result};
use rusqlite::Connection;
use std::path::Path;
use std::time::Duration;

/// WAL plus a 5 s busy timeout is what lets the detached runner and a CLI
/// invocation write the same database concurrently (brief section 2).
///
/// `foreign_keys = ON` is set explicitly rather than relied on implicitly:
/// this build's bundled SQLite happens to default it on, but that is a
/// vendored build flag, not a documented guarantee, so real enforcement
/// should not ride on it silently.
fn tune(conn: &Connection) -> Result<()> {
    conn.busy_timeout(Duration::from_millis(5000))?;
    let _mode: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
    conn.execute("PRAGMA foreign_keys = ON", [])?;
    Ok(())
}

fn open_tuned(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    let mut conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
    // Every transaction on a thoth connection writes, so every one takes the
    // write lock at `BEGIN`; see `write_tx` for why a deferred one cannot be
    // trusted next to a live runner. Call sites use `write_tx` by name; this
    // default covers a `transaction()` or `unchecked_transaction()` all the
    // same.
    conn.set_transaction_behavior(rusqlite::TransactionBehavior::Immediate);
    tune(&conn)?;
    Ok(conn)
}

pub fn open_global(paths: &crate::config::Paths) -> Result<Connection> {
    let conn = open_tuned(&paths.global_db())?;
    migrations::migrate_global(&conn)?;
    Ok(conn)
}

pub fn open_state(paths: &crate::config::Paths, repo_id: &str) -> Result<Connection> {
    let conn = open_tuned(&paths.state_db(repo_id))?;
    migrations::migrate_state(&conn)?;
    Ok(conn)
}

/// A transaction that takes SQLite's write lock at `BEGIN` (`BEGIN IMMEDIATE`)
/// rather than at its first write. Every transaction thoth opens uses it.
///
/// A `DEFERRED` transaction that reads before its first write holds a read
/// snapshot when that write asks for the lock, and SQLite does not call the
/// busy handler for a connection that already holds a snapshot: if another
/// connection is writing at that moment the write fails at once with
/// `SQLITE_BUSY` ("database is locked"), without waiting out the 5 s
/// `busy_timeout`; if another connection committed since the read, it fails
/// with `SQLITE_BUSY_SNAPSHOT`. A live run's runner commits a row for every
/// stream line and heartbeat, so any CLI command running next to it could
/// hit that window (on Linux it is routine: a fresh runner's first writes
/// land while the next command is already in its transaction).
/// `IMMEDIATE` asks for the lock before anything is read, while the
/// connection holds no snapshot, so it waits for the busy timeout like a
/// lone write does, and what it then reads is the committed truth: exactly
/// one writer at a time, and the live-run guards in `dispatch`, `resume`,
/// `submit` and `review start` decide on what they read. A transaction
/// whose first statement is its write would wait as well, but uses this
/// anyway so that a read added in front of it later cannot bring the
/// failure back.
pub fn write_tx(conn: &Connection) -> Result<rusqlite::Transaction<'_>> {
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)?;
    #[cfg(debug_assertions)]
    hold_write_lock_for_test();
    Ok(tx)
}

/// Test seam, for tests/write_lock.rs only; debug builds only, so a release
/// build has no such pause. When `THOTH_TEST_WRITE_TX_PAUSE_FILE` names a
/// file, the process's first write transaction, once it holds the write
/// lock, creates that file and waits until the test deletes it: the test can
/// then run another command while a writer holds the lock, which otherwise
/// only happens by timing. Only the first, so the rest of the command runs
/// without another pause.
#[cfg(debug_assertions)]
fn hold_write_lock_for_test() {
    static FIRST: std::sync::Once = std::sync::Once::new();
    FIRST.call_once(|| crate::commands::spec_review::test_pause("THOTH_TEST_WRITE_TX_PAUSE_FILE"));
}
