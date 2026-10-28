pub mod global;
pub mod migrations;
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
    let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
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
/// rather than at its first write.
///
/// Whole-branch review Important 1/3/5: the live-run guards in `dispatch`,
/// `resume`, `submit` and `review start` all *read* (is any run of this task
/// still `queued`/`running`?) and then *write* on the strength of that read.
/// `Connection::unchecked_transaction` is `DEFERRED`, so a read taken before
/// the transaction's first write establishes a WAL snapshot that another
/// connection can commit past - and the later write then fails with
/// `SQLITE_BUSY_SNAPSHOT` instead of the guard cleanly refusing. `IMMEDIATE`
/// makes the read authoritative: exactly one writer at a time holds the lock,
/// competing writers wait out the 5 s `busy_timeout` and then see the
/// committed truth. Use this wherever a decision is read inside a transaction
/// and acted on by a later write in the same transaction; a transaction whose
/// *first* statement is its write does not need it.
pub fn write_tx(conn: &Connection) -> Result<rusqlite::Transaction<'_>> {
    Ok(rusqlite::Transaction::new_unchecked(
        conn,
        rusqlite::TransactionBehavior::Immediate,
    )?)
}
