//! PANEL-3: the login name next to an account, so the operator can tell
//! claude-a, b and c apart: `claude-a (alice, Max)`, `claude-a（alice，Max）`
//! on Chinese pages. The name is the part of the login email before `@`; the
//! full email is never stored.
//!
//! Only the five commands that already store a tier (`profile check`,
//! `profile login`, `dispatch`, `review start`, `spec review start`) store a
//! name, and only when their login check said logged in and reported an email.
//! Storing is best effort: a failed write prints one warning on stderr and
//! never changes what the command reports. Display only reads global.db.

use crate::config::Paths;
use crate::db::global;
use crate::lang::Lang;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;

/// The longest login name kept.
const MAX_CHARS: usize = 32;

/// The login name of an email: trimmed, the part before the first `@`,
/// control characters removed, at most 32 characters. No `@`, or nothing
/// before it, gives no name.
pub(crate) fn from_email(raw: &str) -> Option<String> {
    let (local, _) = raw.trim().split_once('@')?;
    let name: String = local.chars().filter(|c| !c.is_control()).take(MAX_CHARS).collect();
    (!name.is_empty()).then_some(name)
}

/// Store the login name of the email a login check reported. Returns whether
/// one was stored; no email, or one without a name, keeps the old one and runs
/// no SQL.
pub(crate) fn remember(conn: &Connection, profile_id: &str, email: Option<&str>) -> Result<bool> {
    let Some(name) = email.and_then(from_email) else {
        return Ok(false);
    };
    global::set_profile_login_name(conn, profile_id, &name, &crate::ids::now_rfc3339())?;
    Ok(true)
}

/// `remember`, with a failure turned into one warning line on stderr. The
/// caller calls it only when the check said logged in.
pub(crate) fn remember_or_warn(conn: &Connection, profile_id: &str, email: Option<&str>) -> bool {
    match remember(conn, profile_id, email) {
        Ok(stored) => stored,
        Err(e) => {
            warn(profile_id, &e);
            false
        }
    }
}

/// The warning for a login name that could not be stored.
pub(crate) fn warn(profile_id: &str, e: &anyhow::Error) {
    masked_eprintln!("warning: could not store the login name of {profile_id}: {e:#}");
}

/// Every stored login name, keyed by profile id. A read error gives an empty
/// map: display never fails a command.
pub(crate) fn names(conn: &Connection) -> HashMap<String, String> {
    global::profile_login_names(conn)
        .map(|m| m.into_iter().map(|(id, s)| (id, s.name)).collect())
        .unwrap_or_default()
}

/// `names` from global.db opened read-only and not migrated. No global.db, or
/// one that cannot be opened, gives an empty map; nothing is created.
pub(crate) fn read_only_names(paths: &Paths) -> HashMap<String, String> {
    let path = paths.global_db();
    if !path.is_file() {
        return HashMap::new();
    }
    match crate::commands::doctor::open_read_only(&path) {
        Ok(conn) => names(&conn),
        Err(_) => HashMap::new(),
    }
}

/// An account id with its login name and bracket text: `claude-a (alice,
/// Max)` in English, `claude-a（alice，Max）` in Chinese; without a login name
/// it is `plan_label::named`.
pub(crate) fn named(id: &str, login: Option<&str>, label: Option<&str>, lang: Lang) -> String {
    let Some(login) = login else {
        return crate::plan_label::named(id, label, lang);
    };
    match (label, lang) {
        (None, Lang::En) => format!("{id} ({login})"),
        (None, Lang::ZhTw) => format!("{id}（{login}）"),
        (Some(l), Lang::En) => format!("{id} ({login}, {l})"),
        (Some(l), Lang::ZhTw) => format!("{id}（{login}，{l}）"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    #[test]
    fn login_name_from_email_keeps_the_part_before_the_first_at() {
        assert_eq!(from_email(" alice@example.org ").as_deref(), Some("alice"));
        assert_eq!(from_email("a@b@c").as_deref(), Some("a"));
        assert_eq!(from_email("noatsign"), None);
        assert_eq!(from_email("@example.org"), None);
        assert_eq!(from_email(""), None);
        assert_eq!(from_email("\u{7}@example.org"), None);
        assert_eq!(from_email("al\u{1b}ice@example.org").as_deref(), Some("alice"));
        let long = format!("{}@example.org", "x".repeat(40));
        assert_eq!(from_email(&long), Some("x".repeat(32)));
    }

    #[test]
    fn login_name_named_writes_the_name_before_the_label() {
        assert_eq!(named("claude-a", Some("alice"), Some("Max"), Lang::En), "claude-a (alice, Max)");
        assert_eq!(named("claude-a", Some("alice"), Some("Max"), Lang::ZhTw), "claude-a（alice，Max）");
        assert_eq!(named("claude-a", Some("alice"), None, Lang::En), "claude-a (alice)");
        assert_eq!(named("claude-a", Some("alice"), None, Lang::ZhTw), "claude-a（alice）");
        assert_eq!(named("claude-a", None, Some("Max"), Lang::En), "claude-a (Max)");
        assert_eq!(named("codex-a", None, None, Lang::ZhTw), "codex-a");
    }

    fn profile(id: &str) -> global::Profile {
        global::Profile {
            profile_id: id.into(),
            provider: "anthropic".into(),
            harness: "claude-code".into(),
            account_label: None,
            config_dir: "/c".into(),
            default_models: None,
            status: "active".into(),
        }
    }

    #[test]
    fn login_name_a_failed_write_only_warns() {
        let conn = Connection::open_in_memory().unwrap();
        migrations::migrate_global(&conn).unwrap();
        global::insert_profile(&conn, &profile("claude-a")).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER block_login BEFORE UPDATE OF login_name ON profiles \
             BEGIN SELECT RAISE(ABORT, 'login blocked'); END",
        )
        .unwrap();
        assert!(remember(&conn, "claude-a", Some("alice@example.org")).is_err());
        assert!(!remember_or_warn(&conn, "claude-a", Some("alice@example.org")));
        assert!(names(&conn).is_empty());
    }

    #[test]
    fn login_name_a_schema_4_table_reads_as_no_names() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(migrations::GLOBAL_V1).unwrap();
        for (col, ty) in [
            ("shared", "INTEGER NOT NULL DEFAULT 0"),
            ("plan", "TEXT"),
            ("plan_seen_at", "TEXT"),
            ("models", "TEXT"),
            ("models_seen_at", "TEXT"),
        ] {
            migrations::add_column_if_missing(&conn, "profiles", col, ty).unwrap();
        }
        conn.execute("INSERT INTO schema_version (version) VALUES (4)", []).unwrap();
        global::insert_profile(&conn, &profile("claude-a")).unwrap();
        assert!(names(&conn).is_empty());
        assert!(global::profile_login_names(&conn).unwrap().is_empty());
        assert!(!migrations::column_exists(&conn, "profiles", "login_name").unwrap());
    }

    #[test]
    fn login_name_remember_writes_only_a_name_and_keeps_the_old_one() {
        let conn = Connection::open_in_memory().unwrap();
        migrations::migrate_global(&conn).unwrap();
        global::insert_profile(&conn, &profile("claude-a")).unwrap();
        assert!(remember(&conn, "claude-a", Some("alice@example.org")).unwrap());
        assert!(!remember(&conn, "claude-a", None).unwrap());
        assert!(!remember(&conn, "claude-a", Some("noatsign")).unwrap());
        let got = global::profile_login_names(&conn).unwrap();
        assert_eq!(got["claude-a"].name, "alice");
        assert!(got["claude-a"].seen_at.as_deref().is_some_and(|s| s.ends_with('Z')));
        assert_eq!(names(&conn).get("claude-a").map(String::as_str), Some("alice"));
    }
}
