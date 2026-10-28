//! TIER-1: the subscription tier next to each account. The rule for what a
//! tier may look like, how a stored tier is written next to an account id
//! (`claude-a (Max)`, `claude-a（Max）` on Chinese pages), and the one place
//! that stores a tier a CLI reported.
//!
//! Display only reads global.db: no child process, no network, no migration.
//! Storing is best effort: a failed write prints one warning on stderr and
//! never changes what the command reports.

use crate::config::Paths;
use crate::db::global;
use crate::lang::Lang;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;

/// The longest tier or account label written next to an account id.
const MAX_CHARS: usize = 32;

/// A tier as the CLI reported it, trimmed and lower-cased: 1-32 ASCII letters,
/// digits, `-`, `_` or `.`. Anything else counts as not seen.
pub(crate) fn normalize(raw: &str) -> Option<String> {
    let p = raw.trim().to_ascii_lowercase();
    let ok = (1..=MAX_CHARS).contains(&p.len())
        && p.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    ok.then_some(p)
}

/// The display name of a stored tier: its first letter upper-cased.
pub(crate) fn display(plan: &str) -> String {
    let mut chars = plan.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// The text in brackets after an account id: the tier if one is stored, else
/// the operator's `--label` (control characters removed, at most 32
/// characters), else nothing.
pub(crate) fn label(plan: Option<&str>, account_label: Option<&str>) -> Option<String> {
    if let Some(p) = plan.map(str::trim).filter(|p| !p.is_empty()) {
        return Some(display(p));
    }
    let cleaned: String = account_label?.chars().filter(|c| !c.is_control()).collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        return None;
    }
    if cleaned.chars().count() > MAX_CHARS {
        // RD-4: a label is cut for display, so it is masked whole first
        let masked = crate::redact::Masker::current().mask_text(cleaned).0;
        if masked.chars().count() <= MAX_CHARS {
            return Some(masked);
        }
        let head: String = masked.chars().take(MAX_CHARS).collect();
        return Some(format!("{head}…"));
    }
    Some(cleaned.to_string())
}

/// An account id with its bracket text: `claude-a (Max)` in English,
/// `claude-a（Max）` in Chinese, the bare id when there is no text.
pub(crate) fn named(id: &str, label: Option<&str>, lang: Lang) -> String {
    match (label, lang) {
        (None, _) => id.to_string(),
        (Some(l), Lang::En) => format!("{id} ({l})"),
        (Some(l), Lang::ZhTw) => format!("{id}（{l}）"),
    }
}

/// Every profile's bracket text, keyed by profile id; profiles without one are
/// left out. A read error gives an empty map: display never fails a command.
pub(crate) fn labels(conn: &Connection) -> HashMap<String, String> {
    let (Ok(profiles), Ok(plans)) = (global::list_profiles(conn), global::profile_plans(conn)) else {
        return HashMap::new();
    };
    profiles
        .into_iter()
        .filter_map(|p| {
            let plan = plans.get(&p.profile_id).map(|s| s.plan.as_str());
            label(plan, p.account_label.as_deref()).map(|l| (p.profile_id, l))
        })
        .collect()
}

/// `labels` from global.db opened read-only and not migrated. No global.db, or
/// one that cannot be opened, gives an empty map; nothing is created.
pub(crate) fn read_only_labels(paths: &Paths) -> HashMap<String, String> {
    let path = paths.global_db();
    if !path.is_file() {
        return HashMap::new();
    }
    match crate::commands::doctor::open_read_only(&path) {
        Ok(conn) => labels(&conn),
        Err(_) => HashMap::new(),
    }
}

/// Store the tier a CLI reported, if it is a valid one. Returns whether it was
/// stored; a missing or invalid tier keeps the old one and runs no SQL.
pub(crate) fn remember(conn: &Connection, profile_id: &str, raw: Option<&str>) -> Result<bool> {
    let Some(plan) = raw.and_then(normalize) else {
        return Ok(false);
    };
    global::set_profile_plan(conn, profile_id, &plan, &crate::ids::now_rfc3339())?;
    Ok(true)
}

/// `remember`, with a failure turned into one warning line on stderr.
pub(crate) fn remember_or_warn(conn: &Connection, profile_id: &str, raw: Option<&str>) -> bool {
    match remember(conn, profile_id, raw) {
        Ok(stored) => stored,
        Err(e) => {
            masked_eprintln!("warning: could not store the plan of {profile_id}: {e:#}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn profile(id: &str, account_label: Option<&str>) -> global::Profile {
        global::Profile {
            profile_id: id.into(),
            provider: "anthropic".into(),
            harness: "claude-code".into(),
            account_label: account_label.map(str::to_string),
            config_dir: "/c".into(),
            default_models: None,
            status: "active".into(),
        }
    }

    fn row(conn: &Connection, id: &str) -> (Option<String>, Option<String>) {
        conn.query_row(
            "SELECT plan, plan_seen_at FROM profiles WHERE profile_id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    fn quota_rows(conn: &Connection) -> i64 {
        conn.query_row("SELECT COUNT(*) FROM quota_events", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn plan_label_normalize_accepts_one_short_token() {
        assert_eq!(normalize(" Max ").as_deref(), Some("max"));
        assert_eq!(normalize("PRO").as_deref(), Some("pro"));
        assert_eq!(normalize("team_premium").as_deref(), Some("team_premium"));
        assert_eq!(normalize("plus").as_deref(), Some("plus"));
        assert_eq!(normalize(&"a".repeat(32)), Some("a".repeat(32)));
        for bad in [
            String::new(),
            "   ".to_string(),
            "max ultra".to_string(),
            "ultra/max".to_string(),
            "a".repeat(33),
            "專業".to_string(),
            "pro\u{1b}".to_string(),
        ] {
            assert_eq!(normalize(&bad), None, "{bad:?}");
        }
    }

    #[test]
    fn plan_label_display_and_label_precedence() {
        assert_eq!(display("max"), "Max");
        assert_eq!(display("pro"), "Pro");
        assert_eq!(display("plus"), "Plus");
        assert_eq!(display("team_premium"), "Team_premium");
        assert_eq!(display("1x"), "1x");

        assert_eq!(label(Some("pro"), Some("second")).as_deref(), Some("Pro"));
        assert_eq!(label(Some("  "), Some(" work laptop ")).as_deref(), Some("work laptop"));
        assert_eq!(label(None, Some("  ")), None);
        assert_eq!(label(None, None), None);
        assert_eq!(label(None, Some("work\u{7}laptop")).as_deref(), Some("worklaptop"));
        assert_eq!(label(None, Some("\u{7}")), None);
        let forty = "x".repeat(40);
        assert_eq!(label(None, Some(&forty)), Some(format!("{}…", "x".repeat(32))));
        let thirty_two = "x".repeat(32);
        assert_eq!(label(None, Some(&thirty_two)), Some(thirty_two.clone()));
        assert_eq!(label(Some("pro"), Some(&forty)).as_deref(), Some("Pro"));
    }

    #[test]
    fn a_long_label_is_masked_before_it_is_cut() {
        let token = format!("{}{}", ["gh", "p_"].concat(), "dummyDUMMY0123456789dummyDUMMY012345");
        let long = format!("work {token}");
        assert_eq!(label(None, Some(&long)).as_deref(), Some("work [redacted:github-token]"));
        let longer = format!("{} {token}", "x".repeat(20));
        let out = label(None, Some(&longer)).unwrap();
        assert_eq!(out, format!("{} [redacted:g…", "x".repeat(20)));
        assert!(!out.contains(&token[..8]), "{out}");
    }

    #[test]
    fn plan_label_named_uses_half_width_in_english_and_full_width_in_chinese() {
        assert_eq!(named("claude-a", Some("Max"), Lang::En), "claude-a (Max)");
        assert_eq!(named("claude-a", Some("Max"), Lang::ZhTw), "claude-a（Max）");
        assert_eq!(named("agy-a", None, Lang::ZhTw), "agy-a");
    }

    #[test]
    fn plan_label_remember_writes_only_a_valid_plan_and_keeps_the_old_one() {
        let conn = Connection::open_in_memory().unwrap();
        migrations::migrate_global(&conn).unwrap();
        global::insert_profile(&conn, &profile("claude-a", None)).unwrap();

        assert!(remember(&conn, "claude-a", Some(" Max ")).unwrap());
        let (plan, seen) = row(&conn, "claude-a");
        assert_eq!(plan.as_deref(), Some("max"));
        let seen = seen.unwrap();
        time::OffsetDateTime::parse(&seen, &time::format_description::well_known::Rfc3339)
            .unwrap();
        assert!(seen.ends_with('Z'), "{seen}");

        conn.execute(
            "UPDATE profiles SET plan_seen_at = '2000-01-01T00:00:00Z' WHERE profile_id = 'claude-a'",
            [],
        )
        .unwrap();
        assert!(!remember(&conn, "claude-a", None).unwrap());
        assert!(!remember(&conn, "claude-a", Some("")).unwrap());
        assert!(!remember(&conn, "claude-a", Some("max ultra")).unwrap());
        assert_eq!(
            row(&conn, "claude-a"),
            (Some("max".to_string()), Some("2000-01-01T00:00:00Z".to_string()))
        );
        assert_eq!(quota_rows(&conn), 0);
    }

    #[test]
    fn plan_label_labels_read_a_v2_table_as_account_labels_only() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(migrations::GLOBAL_V1).unwrap();
        migrations::add_column_if_missing(&conn, "profiles", "shared", "INTEGER NOT NULL DEFAULT 0")
            .unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (2)", [])
            .unwrap();
        global::insert_profile(&conn, &profile("claude-b", Some("second"))).unwrap();
        global::insert_profile(&conn, &profile("claude-a", None)).unwrap();

        let got = labels(&conn);
        let want: HashMap<String, String> =
            [("claude-b".to_string(), "second".to_string())].into_iter().collect();
        assert_eq!(got, want);
        assert!(!migrations::column_exists(&conn, "profiles", "plan").unwrap());
    }
}
