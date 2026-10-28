use anyhow::Result;
use rusqlite::Connection;

/// `global.db` gained nothing in Phase 2, so it stayed at 1. v2 (DT-1) adds
/// `profiles.shared`: the operator's mark that a profile is also used by hand.
/// v3 (TIER-1) adds `profiles.plan` and `profiles.plan_seen_at`. v4
/// (MODELS-1) adds `profiles.models` (JSON) and `profiles.models_seen_at`.
pub const GLOBAL_SCHEMA_VERSION: i64 = 4;
/// `repos/<id>/state.db` is v2 as of Phase 2: `tasks.attempt_cap`,
/// `runs.harness_bin`, and the evidence/review indexes. v3 adds the approved
/// `checks` catalogue. v4 adds `checks.exe_pin` (`hash` or `path`). v5 adds
/// the four token columns of `runs` (DT-1). v6 adds `check_approvals`: every
/// approval a check was given, not only the latest (LESS-1).
pub const STATE_SCHEMA_VERSION: i64 = 6;

/// v3: the per-repo catalogue of approved checks. Lives in state.db, not in the
/// repo, so a worker cannot edit what the human approved.
pub const STATE_V3_CHECKS: &str = r#"
CREATE TABLE IF NOT EXISTS checks (
  check_id             TEXT PRIMARY KEY,
  shell                TEXT NOT NULL,
  cmd                  TEXT NOT NULL,
  cwd                  TEXT NOT NULL,
  inputs               TEXT NOT NULL,
  created_at           TEXT NOT NULL,
  approved_at          TEXT,
  approved_fingerprint TEXT,
  approved_detail      TEXT,
  approved_commit      TEXT
);
"#;

/// v6: every approval of a check, newest the highest `seq`. `source` is
/// `main`, `run` or `migrated`; `run_id` is set for `run`. The `checks` row's
/// `approved_*` columns stay a copy of the newest one.
pub const STATE_V6_CHECK_APPROVALS: &str = "CREATE TABLE IF NOT EXISTS check_approvals (seq INTEGER PRIMARY KEY AUTOINCREMENT, check_id TEXT NOT NULL, approved_at TEXT NOT NULL, fingerprint TEXT NOT NULL, detail TEXT, approved_commit TEXT, source TEXT NOT NULL, run_id TEXT); CREATE INDEX IF NOT EXISTS idx_check_approvals_check ON check_approvals(check_id);";

/// global.db v1. `JSON`-documented columns are declared TEXT because SQLite
/// would give an unknown type name NUMERIC affinity (see plan Open question 6).
pub const GLOBAL_V1: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);

CREATE TABLE IF NOT EXISTS profiles (
  profile_id     TEXT PRIMARY KEY,
  provider       TEXT,
  harness        TEXT,
  account_label  TEXT,
  config_dir     TEXT,
  default_models TEXT,
  status         TEXT NOT NULL DEFAULT 'active'
);

CREATE TABLE IF NOT EXISTS quota_events (
  seq        INTEGER PRIMARY KEY AUTOINCREMENT,
  ts         TEXT,
  profile_id TEXT REFERENCES profiles,
  kind       TEXT,
  detail     TEXT
);

CREATE TABLE IF NOT EXISTS repos (
  repo_id    TEXT PRIMARY KEY,
  root_path  TEXT,
  created_at TEXT
);
"#;

pub fn current_version(conn: &Connection) -> Result<i64> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Ok(0);
    }
    let v: Option<i64> = conn.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))?;
    Ok(v.unwrap_or(0))
}

pub fn migrate_global(conn: &Connection) -> Result<()> {
    if current_version(conn)? >= GLOBAL_SCHEMA_VERSION {
        return Ok(());
    }
    // v1's tables, then the v2 column, then the v3 and v4 columns; every
    // step tolerates being replayed.
    conn.execute_batch(GLOBAL_V1)?;
    add_column_if_missing(conn, "profiles", "shared", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(conn, "profiles", "plan", "TEXT")?;
    add_column_if_missing(conn, "profiles", "plan_seen_at", "TEXT")?;
    add_column_if_missing(conn, "profiles", "models", "TEXT")?;
    add_column_if_missing(conn, "profiles", "models_seen_at", "TEXT")?;
    conn.execute(
        "INSERT OR IGNORE INTO schema_version (version) VALUES (?1)",
        [GLOBAL_SCHEMA_VERSION],
    )?;
    Ok(())
}

/// repos/<id>/state.db v1 = v6 section 10.5 verbatim in column names and order,
/// plus the Phase 1 additions from brief section 6 and plan Open questions 3 and 4.
pub const STATE_V1: &str = r#"
CREATE TABLE IF NOT EXISTS schema_version (version INTEGER PRIMARY KEY);

CREATE TABLE IF NOT EXISTS meta (
  key   TEXT PRIMARY KEY,
  value TEXT
);

CREATE TABLE IF NOT EXISTS tasks (
  task_id     TEXT PRIMARY KEY,
  title       TEXT,
  spec        TEXT,
  risk        TEXT,
  status      TEXT,
  base_commit TEXT,
  created_at  TEXT,
  updated_at  TEXT
);

CREATE TABLE IF NOT EXISTS runs (
  run_id             TEXT PRIMARY KEY,
  task_id            TEXT REFERENCES tasks,
  attempt            INTEGER,
  role               TEXT,
  provider           TEXT,
  account            TEXT,
  model              TEXT,
  harness_session_id TEXT,
  config_dir         TEXT,
  allowed_tools      TEXT,
  worktree           TEXT,
  base_commit        TEXT,
  candidate_commit   TEXT,
  spec               TEXT,
  status             TEXT,
  interrupted_reason TEXT,
  exit_code          INTEGER,
  input_tokens       INTEGER,
  output_tokens      INTEGER,
  started_at         TEXT,
  ended_at           TEXT,
  runner_pid         INTEGER,
  worker_pid         INTEGER,
  heartbeat_at       TEXT,
  cancel_requested   INTEGER NOT NULL DEFAULT 0,
  timeout_seconds    INTEGER,
  run_dir            TEXT,
  error_text         TEXT
);

CREATE TABLE IF NOT EXISTS claims (
  claim_id TEXT PRIMARY KEY,
  run_id   TEXT REFERENCES runs,
  kind     TEXT,
  text     TEXT
);

CREATE TABLE IF NOT EXISTS evidence (
  evidence_id  TEXT PRIMARY KEY,
  code         TEXT,
  task_id      TEXT,
  run_id       TEXT,
  kind         TEXT,
  collected_by TEXT,
  label        TEXT,
  sha256       TEXT,
  bytes        INTEGER,
  blob_path    TEXT,
  detail       TEXT,
  created_at   TEXT
);

CREATE TABLE IF NOT EXISTS claim_evidence (
  claim_id    TEXT,
  evidence_id TEXT
);

CREATE TABLE IF NOT EXISTS reviews (
  review_id       TEXT PRIMARY KEY,
  target_run_id   TEXT REFERENCES runs,
  reviewer_run_id TEXT REFERENCES runs,
  verdict         TEXT,
  findings        TEXT,
  created_at      TEXT
);

CREATE TABLE IF NOT EXISTS events (
  seq     INTEGER PRIMARY KEY AUTOINCREMENT,
  ts      TEXT,
  task_id TEXT,
  run_id  TEXT,
  type    TEXT,
  detail  TEXT
);

CREATE INDEX IF NOT EXISTS idx_runs_task     ON runs(task_id);
CREATE INDEX IF NOT EXISTS idx_evidence_task ON evidence(task_id);
CREATE INDEX IF NOT EXISTS idx_events_task   ON events(task_id);
"#;

/// brief §6's v2 index set. Every statement is `IF NOT EXISTS`, so this batch
/// is safe to replay.
///
/// `idx_evidence_task_code` is the backstop behind `evidence::next_code`:
/// two writers allocating a code for the same task at the same moment cannot
/// both land, whatever they each computed. Creating it on an existing database
/// would fail if that database already held two rows with the same
/// `(task_id, code)` - Phase 1 never wrote an `evidence` row at all, so there
/// are none, and a future migration that has to cope with real duplicates
/// would need a de-duplication step first.
pub const STATE_V2_INDEXES: &str = r#"
CREATE UNIQUE INDEX IF NOT EXISTS idx_evidence_task_code ON evidence(task_id, code);
CREATE INDEX IF NOT EXISTS idx_claim_evidence_claim      ON claim_evidence(claim_id);
CREATE INDEX IF NOT EXISTS idx_reviews_target            ON reviews(target_run_id);
CREATE INDEX IF NOT EXISTS idx_reviews_reviewer          ON reviews(reviewer_run_id);
"#;

/// Whether `table` already has `column`.
///
/// The table name is interpolated rather than bound: SQLite's pragma
/// table-valued functions do not accept a bound parameter for their argument,
/// and every caller passes a hardcoded name from this file.
pub fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let sql = format!("SELECT name FROM pragma_table_info('{table}')");
    let mut stmt = conn.prepare(&sql)?;
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(names.iter().any(|n| n == column))
}

/// `ALTER TABLE ... ADD COLUMN` is not itself re-entrant (SQLite raises
/// "duplicate column name"), so every added column goes through this guard.
/// Returns whether the column was actually added.
pub fn add_column_if_missing(
    conn: &Connection,
    table: &str,
    column: &str,
    decl: &str,
) -> Result<bool> {
    if column_exists(conn, table, column)? {
        return Ok(false);
    }
    conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"))?;
    Ok(true)
}

/// brief §6's v1 → v2 delta, written so it can be replayed on a v1 database, a
/// half-migrated one, or a v2 one and always leave the same shape.
///
/// `attempt_cap`'s default is a literal, which is what lets SQLite apply it to
/// rows that already exist rather than leaving them NULL.
pub fn migrate_state_v1_to_v2(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "tasks", "attempt_cap", "INTEGER DEFAULT 2")?;
    add_column_if_missing(conn, "runs", "harness_bin", "TEXT")?;
    conn.execute_batch(STATE_V2_INDEXES)?;
    Ok(())
}

/// v2 → v3: only adds a table, so replaying it is always safe.
pub fn migrate_state_v2_to_v3(conn: &Connection) -> Result<()> {
    conn.execute_batch(STATE_V3_CHECKS)?;
    Ok(())
}

/// v3 → v4: how a check pins its program. Existing rows take the default,
/// `hash` (the program's path and bytes), so their approvals stay valid.
pub fn migrate_state_v3_to_v4(conn: &Connection) -> Result<()> {
    add_column_if_missing(conn, "checks", "exe_pin", "TEXT NOT NULL DEFAULT 'hash'")?;
    Ok(())
}

/// v4 → v5: the token kinds a run reported, besides `input_tokens` and
/// `output_tokens`. NULL means the harness reported nothing of that kind.
pub fn migrate_state_v4_to_v5(conn: &Connection) -> Result<()> {
    for column in ["cache_read_tokens", "cache_write_tokens", "reasoning_tokens", "total_tokens"] {
        add_column_if_missing(conn, "runs", column, "INTEGER")?;
    }
    Ok(())
}

/// v5 → v6: adds `check_approvals` and copies each check's current approval
/// into it as `migrated`. A check that already has a row there is skipped, so
/// replaying the step inserts nothing twice.
pub fn migrate_state_v5_to_v6(conn: &Connection) -> Result<()> {
    conn.execute_batch(STATE_V6_CHECK_APPROVALS)?;
    conn.execute(
        "INSERT INTO check_approvals \
           (check_id, approved_at, fingerprint, detail, approved_commit, source, run_id) \
         SELECT check_id, approved_at, approved_fingerprint, approved_detail, approved_commit, \
                'migrated', NULL \
           FROM checks c \
          WHERE approved_at IS NOT NULL AND approved_fingerprint IS NOT NULL \
            AND NOT EXISTS (SELECT 1 FROM check_approvals a WHERE a.check_id = c.check_id) \
          ORDER BY check_id",
        [],
    )?;
    Ok(())
}

pub fn migrate_state(conn: &Connection) -> Result<()> {
    let from = current_version(conn)?;
    if from >= STATE_SCHEMA_VERSION {
        return Ok(());
    }
    // A brand-new database needs the v1 tables first; a Phase 1 database
    // already has them and only needs the delta.
    if from < 1 {
        conn.execute_batch(STATE_V1)?;
    }
    migrate_state_v1_to_v2(conn)?;
    migrate_state_v2_to_v3(conn)?;
    migrate_state_v3_to_v4(conn)?;
    migrate_state_v4_to_v5(conn)?;
    migrate_state_v5_to_v6(conn)?;
    // `current_version` is MAX(version), so leaving the old `1` row in place
    // is harmless and keeps the row history.
    conn.execute(
        "INSERT OR IGNORE INTO schema_version (version) VALUES (?1)",
        [STATE_SCHEMA_VERSION],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap()
    }

    fn index_count(conn: &Connection, name: &str) -> i64 {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name = ?1",
            [name],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn global_migration_creates_the_tables_and_is_reentrant() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_global(&conn).unwrap();
        let names = table_names(&conn);
        for t in ["profiles", "quota_events", "repos", "schema_version"] {
            assert!(names.contains(&t.to_string()), "missing {t} in {names:?}");
        }
        assert_eq!(current_version(&conn).unwrap(), GLOBAL_SCHEMA_VERSION);

        // running it twice must not fail and must not bump the version
        migrate_global(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), GLOBAL_SCHEMA_VERSION);
    }

    // DT-1 u7: a v1 global.db gains `profiles.shared`, off for every profile.
    #[test]
    fn a_v1_global_db_gains_the_shared_column_off_for_existing_profiles() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(GLOBAL_V1).unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (1)", [])
            .unwrap();
        let profile = crate::db::global::Profile {
            profile_id: "claude-b".into(),
            provider: "anthropic".into(),
            harness: "claude-code".into(),
            account_label: Some("b".into()),
            config_dir: "/c".into(),
            default_models: None,
            status: "active".into(),
        };
        crate::db::global::insert_profile(&conn, &profile).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 1);
        assert!(!column_exists(&conn, "profiles", "shared").unwrap());

        migrate_global(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), GLOBAL_SCHEMA_VERSION);
        assert!(column_exists(&conn, "profiles", "shared").unwrap());
        assert!(!crate::db::global::profile_shared(&conn, "claude-b").unwrap());
        assert_eq!(
            crate::db::global::get_profile(&conn, "claude-b").unwrap().unwrap(),
            profile
        );

        migrate_global(&conn).unwrap();
        migrate_global(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), GLOBAL_SCHEMA_VERSION);
    }

    // TIER-1: a v2 global.db gains `profiles.plan` and `profiles.plan_seen_at`,
    // NULL for every existing profile.
    #[test]
    fn a_v2_global_db_gains_the_plan_columns_as_null_and_the_step_is_reentrant() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(GLOBAL_V1).unwrap();
        add_column_if_missing(&conn, "profiles", "shared", "INTEGER NOT NULL DEFAULT 0").unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (2)", [])
            .unwrap();
        let profile = crate::db::global::Profile {
            profile_id: "claude-b".into(),
            provider: "anthropic".into(),
            harness: "claude-code".into(),
            account_label: Some("b".into()),
            config_dir: "/c".into(),
            default_models: None,
            status: "active".into(),
        };
        crate::db::global::insert_profile(&conn, &profile).unwrap();
        crate::db::global::set_profile_shared(&conn, "claude-b", true).unwrap();
        assert!(!column_exists(&conn, "profiles", "plan").unwrap());
        assert!(!column_exists(&conn, "profiles", "plan_seen_at").unwrap());

        migrate_global(&conn).unwrap();
        // MODELS-1: the migration now ends at v4.
        assert_eq!(current_version(&conn).unwrap(), 4);
        assert!(column_exists(&conn, "profiles", "plan").unwrap());
        assert!(column_exists(&conn, "profiles", "plan_seen_at").unwrap());
        let (plan, seen): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT plan, plan_seen_at FROM profiles WHERE profile_id = 'claude-b'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(plan, None);
        assert_eq!(seen, None);
        assert_eq!(
            crate::db::global::get_profile(&conn, "claude-b").unwrap().unwrap(),
            profile
        );
        assert!(crate::db::global::profile_shared(&conn, "claude-b").unwrap());

        migrate_global(&conn).unwrap();
        migrate_global(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 4);
    }

    // MODELS-1: a v3 global.db gains `profiles.models` and
    // `profiles.models_seen_at`, NULL for every existing profile; `plan` and
    // `shared` are untouched, and the step can be replayed.
    #[test]
    fn a_v3_global_db_gains_the_model_columns_as_null_and_the_step_is_reentrant() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(GLOBAL_V1).unwrap();
        add_column_if_missing(&conn, "profiles", "shared", "INTEGER NOT NULL DEFAULT 0").unwrap();
        add_column_if_missing(&conn, "profiles", "plan", "TEXT").unwrap();
        add_column_if_missing(&conn, "profiles", "plan_seen_at", "TEXT").unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (3)", [])
            .unwrap();
        let profile = crate::db::global::Profile {
            profile_id: "agy-a".into(),
            provider: "google".into(),
            harness: "antigravity".into(),
            account_label: None,
            config_dir: "/a".into(),
            default_models: None,
            status: "active".into(),
        };
        crate::db::global::insert_profile(&conn, &profile).unwrap();
        crate::db::global::set_profile_shared(&conn, "agy-a", true).unwrap();
        crate::db::global::set_profile_plan(&conn, "agy-a", "ultra", "2026-10-01T00:00:00Z").unwrap();
        assert_eq!(current_version(&conn).unwrap(), 3);
        assert!(!column_exists(&conn, "profiles", "models").unwrap());
        assert!(!column_exists(&conn, "profiles", "models_seen_at").unwrap());

        migrate_global(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 4);
        assert!(column_exists(&conn, "profiles", "models").unwrap());
        assert!(column_exists(&conn, "profiles", "models_seen_at").unwrap());
        let (models, seen, plan, shared): (Option<String>, Option<String>, Option<String>, i64) = conn
            .query_row(
                "SELECT models, models_seen_at, plan, shared FROM profiles WHERE profile_id = 'agy-a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(models, None);
        assert_eq!(seen, None);
        assert_eq!(plan.as_deref(), Some("ultra"));
        assert_eq!(shared, 1);
        assert_eq!(
            crate::db::global::get_profile(&conn, "agy-a").unwrap().unwrap(),
            profile
        );
        assert!(crate::db::global::profile_models(&conn).unwrap().is_empty());

        migrate_global(&conn).unwrap();
        migrate_global(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 4);
    }

    #[test]
    fn state_migration_creates_every_v6_table_plus_the_phase1_and_phase2_columns() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_state(&conn).unwrap();
        let names = table_names(&conn);
        for t in [
            "checks",
            "claim_evidence",
            "claims",
            "events",
            "evidence",
            "meta",
            "reviews",
            "runs",
            "schema_version",
            "tasks",
        ] {
            assert!(names.contains(&t.to_string()), "missing {t} in {names:?}");
        }

        let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('runs')").unwrap();
        let cols: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<String>>>()
            .unwrap();
        for c in [
            "run_id", "task_id", "attempt", "role", "provider", "account", "model",
            "harness_session_id", "config_dir", "allowed_tools", "worktree", "base_commit",
            "candidate_commit", "spec", "status", "interrupted_reason", "exit_code",
            "input_tokens", "output_tokens", "started_at", "ended_at", "runner_pid",
            "worker_pid", "heartbeat_at", "cancel_requested", "timeout_seconds", "run_dir",
            "error_text",
            // Phase 2 (final-review Important 3 follow-up)
            "harness_bin",
        ] {
            assert!(cols.contains(&c.to_string()), "runs is missing {c}: {cols:?}");
        }

        // re-entrant, and a fresh database lands directly on v6
        migrate_state(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), STATE_SCHEMA_VERSION);
        assert_eq!(STATE_SCHEMA_VERSION, 6);
    }

    #[test]
    fn a_fresh_state_db_has_the_v2_columns_and_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_state(&conn).unwrap();
        assert!(column_exists(&conn, "tasks", "attempt_cap").unwrap());
        assert!(column_exists(&conn, "runs", "harness_bin").unwrap());
        assert!(!column_exists(&conn, "tasks", "no_such_column").unwrap());
        for idx in [
            "idx_evidence_task_code",
            "idx_claim_evidence_claim",
            "idx_reviews_target",
            "idx_reviews_reviewer",
        ] {
            assert_eq!(index_count(&conn, idx), 1, "missing index {idx}");
        }
    }

    /// brief §13: the migration must run on a database Phase 1 built. This
    /// builds exactly that - `STATE_V1` plus `schema_version = 1` - with one
    /// pre-existing task row, so the `ADD COLUMN ... DEFAULT 2` backfill is
    /// observable rather than assumed.
    #[test]
    fn a_v1_state_db_migrates_to_v2_and_the_delta_is_reentrant() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(STATE_V1).unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO tasks (task_id, title, spec, risk, status, base_commit, created_at, updated_at)
             VALUES ('T1','t','{}','normal','in_review','abc','n','n')",
            [],
        )
        .unwrap();
        assert_eq!(current_version(&conn).unwrap(), 1);
        assert!(!column_exists(&conn, "tasks", "attempt_cap").unwrap());
        assert!(!column_exists(&conn, "runs", "harness_bin").unwrap());

        migrate_state(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), STATE_SCHEMA_VERSION);
        assert!(column_exists(&conn, "tasks", "attempt_cap").unwrap());
        assert!(column_exists(&conn, "runs", "harness_bin").unwrap());

        let cap: i64 = conn
            .query_row("SELECT attempt_cap FROM tasks WHERE task_id = 'T1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cap, 2, "an existing row must inherit the column default");
        let title: String = conn
            .query_row("SELECT title FROM tasks WHERE task_id = 'T1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "t", "the migration must not disturb existing data");

        // Re-entrancy, both ways round: through `migrate_state` (which
        // short-circuits on the version) and through the delta itself (which
        // is the part that has to tolerate being replayed).
        migrate_state(&conn).unwrap();
        migrate_state_v1_to_v2(&conn).unwrap();
        migrate_state_v1_to_v2(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), STATE_SCHEMA_VERSION);
        assert_eq!(index_count(&conn, "idx_evidence_task_code"), 1);
    }

    /// A v2 database (Phase 2 shape) with a task in it: the v3 step adds the
    /// `checks` table, leaves the task alone, and can be replayed.
    #[test]
    fn a_v2_state_db_migrates_to_v3_and_the_delta_is_reentrant() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(STATE_V1).unwrap();
        migrate_state_v1_to_v2(&conn).unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (2)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO tasks (task_id, title, spec, risk, status, base_commit, created_at, updated_at)
             VALUES ('T1','t','{}','normal','in_review','abc','n','n')",
            [],
        )
        .unwrap();
        assert_eq!(current_version(&conn).unwrap(), 2);
        assert!(!table_names(&conn).contains(&"checks".to_string()));

        migrate_state(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), STATE_SCHEMA_VERSION);
        assert!(table_names(&conn).contains(&"checks".to_string()));
        for c in [
            "check_id", "shell", "cmd", "cwd", "inputs", "created_at", "approved_at",
            "approved_fingerprint", "approved_detail", "approved_commit", "exe_pin",
        ] {
            assert!(column_exists(&conn, "checks", c).unwrap(), "checks lacks {c}");
        }
        let title: String = conn
            .query_row("SELECT title FROM tasks WHERE task_id = 'T1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(title, "t", "the migration must not disturb existing data");

        migrate_state(&conn).unwrap();
        migrate_state_v2_to_v3(&conn).unwrap();
        migrate_state_v2_to_v3(&conn).unwrap();
        migrate_state_v3_to_v4(&conn).unwrap();
        migrate_state_v3_to_v4(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), STATE_SCHEMA_VERSION);
    }

    /// A v3 database with a check in it: the v4 step adds `exe_pin`, and the
    /// existing row reads as `hash`.
    #[test]
    fn a_v3_state_db_gains_exe_pin_with_hash_for_existing_rows() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(STATE_V1).unwrap();
        migrate_state_v1_to_v2(&conn).unwrap();
        migrate_state_v2_to_v3(&conn).unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (3)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO checks (check_id, shell, cmd, cwd, inputs, created_at) \
             VALUES ('t','argv','x','.','[]','n')",
            [],
        )
        .unwrap();
        assert_eq!(current_version(&conn).unwrap(), 3);

        migrate_state(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 6);
        let pin: String = conn
            .query_row("SELECT exe_pin FROM checks WHERE check_id='t'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(pin, "hash");
        assert_eq!(crate::checks::get_check(&conn, "t").unwrap().unwrap().exe_pin, "hash");
    }

    // DT-1 u4: a v4 database with a run in it gains four NULL token columns.
    #[test]
    fn a_v4_state_db_gains_the_token_columns_as_null_and_the_step_is_reentrant() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(STATE_V1).unwrap();
        migrate_state_v1_to_v2(&conn).unwrap();
        migrate_state_v2_to_v3(&conn).unwrap();
        migrate_state_v3_to_v4(&conn).unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (4)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO tasks (task_id, title, spec, risk, status, base_commit, created_at, updated_at)
             VALUES ('T1','t','{}','normal','in_review','abc','n','n')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (run_id, task_id, attempt, role, provider, account, model, config_dir,
                allowed_tools, worktree, base_commit, spec, status, input_tokens, cancel_requested,
                timeout_seconds, run_dir)
             VALUES ('R1','T1',1,'implementer','anthropic','claude-b','sonnet','','','','abc','{}',
                'completed',34,0,0,'')",
            [],
        )
        .unwrap();
        assert_eq!(current_version(&conn).unwrap(), 4);
        let cols = ["cache_read_tokens", "cache_write_tokens", "reasoning_tokens", "total_tokens"];
        for c in cols {
            assert!(!column_exists(&conn, "runs", c).unwrap(), "{c} before");
        }

        migrate_state(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 6);
        for c in cols {
            assert!(column_exists(&conn, "runs", c).unwrap(), "{c} after");
            let v: Option<i64> = conn
                .query_row(&format!("SELECT {c} FROM runs WHERE run_id = 'R1'"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, None, "{c}");
        }
        let input: i64 = conn
            .query_row("SELECT input_tokens FROM runs WHERE run_id = 'R1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(input, 34);

        migrate_state(&conn).unwrap();
        migrate_state(&conn).unwrap();
        migrate_state_v4_to_v5(&conn).unwrap();
        migrate_state_v4_to_v5(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 6);
    }

    // LESS-1: a v5 database's current approval becomes one `migrated` row of
    // `check_approvals`; a pending check gets none; the step is re-entrant.
    #[test]
    fn a_v5_state_db_gains_check_approvals_with_its_current_approval() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(STATE_V1).unwrap();
        migrate_state_v1_to_v2(&conn).unwrap();
        migrate_state_v2_to_v3(&conn).unwrap();
        migrate_state_v3_to_v4(&conn).unwrap();
        migrate_state_v4_to_v5(&conn).unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (5)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO checks (check_id, shell, cmd, cwd, inputs, created_at, approved_at, \
             approved_fingerprint, approved_detail, approved_commit) \
             VALUES ('ok','argv','x','.','[]','n','2026-10-01T00:00:00Z','fp1','{}','abc')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO checks (check_id, shell, cmd, cwd, inputs, created_at) \
             VALUES ('pending','argv','x','.','[]','n')",
            [],
        )
        .unwrap();
        assert_eq!(current_version(&conn).unwrap(), 5);

        migrate_state(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 6);
        let ok = crate::checks::approvals(&conn, "ok").unwrap();
        assert_eq!(ok.len(), 1);
        assert_eq!(ok[0].source, "migrated");
        assert_eq!(ok[0].fingerprint, "fp1");
        assert_eq!(ok[0].approved_at, "2026-10-01T00:00:00Z");
        assert_eq!(ok[0].detail.as_deref(), Some("{}"));
        assert_eq!(ok[0].approved_commit.as_deref(), Some("abc"));
        assert_eq!(ok[0].run_id, None);
        assert!(crate::checks::approvals(&conn, "pending").unwrap().is_empty());

        migrate_state(&conn).unwrap();
        migrate_state(&conn).unwrap();
        migrate_state_v5_to_v6(&conn).unwrap();
        migrate_state_v5_to_v6(&conn).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 6);
        assert_eq!(crate::checks::approvals(&conn, "ok").unwrap().len(), 1);
        assert!(crate::checks::approvals(&conn, "pending").unwrap().is_empty());
    }

    #[test]
    fn the_v2_unique_index_rejects_a_duplicate_code_within_one_task() {
        let conn = Connection::open_in_memory().unwrap();
        migrate_state(&conn).unwrap();
        conn.execute(
            "INSERT INTO evidence (evidence_id, code, task_id) VALUES ('EV_A','E001','T1')",
            [],
        )
        .unwrap();
        // same code, different task: fine
        conn.execute(
            "INSERT INTO evidence (evidence_id, code, task_id) VALUES ('EV_B','E001','T2')",
            [],
        )
        .unwrap();
        // same code, same task: refused by idx_evidence_task_code
        assert!(
            conn.execute(
                "INSERT INTO evidence (evidence_id, code, task_id) VALUES ('EV_C','E001','T1')",
                [],
            )
            .is_err(),
            "UNIQUE(task_id, code) must reject a duplicate code inside one task"
        );
    }
}
