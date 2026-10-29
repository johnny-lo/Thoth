use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub profile_id: String,
    pub provider: String,
    pub harness: String,
    pub account_label: Option<String>,
    pub config_dir: String,
    pub default_models: Option<String>,
    pub status: String,
}

// `dispatch` reads `ts` and `kind`; `seq`, `profile_id` and `detail` are only read
// by `quota_events_are_sequenced_and_filtered_by_profile`. Kept whole because this
// is the row shape, and Phase 2's scheduler wants all of it.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaEvent {
    pub seq: i64,
    pub ts: String,
    pub profile_id: String,
    pub kind: String,
    pub detail: String,
}

const PROFILE_COLS: &str =
    "profile_id, provider, harness, account_label, config_dir, default_models, status";

fn row_to_profile(r: &rusqlite::Row<'_>) -> rusqlite::Result<Profile> {
    Ok(Profile {
        profile_id: r.get(0)?,
        provider: r.get(1)?,
        harness: r.get(2)?,
        account_label: r.get(3)?,
        config_dir: r.get(4)?,
        default_models: r.get(5)?,
        status: r.get(6)?,
    })
}

pub fn insert_profile(conn: &Connection, p: &Profile) -> Result<()> {
    conn.execute(
        "INSERT INTO profiles (profile_id, provider, harness, account_label, config_dir, default_models, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            p.profile_id,
            p.provider,
            p.harness,
            p.account_label,
            p.config_dir,
            p.default_models,
            p.status
        ],
    )?;
    Ok(())
}

pub fn get_profile(conn: &Connection, id: &str) -> Result<Option<Profile>> {
    let sql = format!("SELECT {PROFILE_COLS} FROM profiles WHERE profile_id = ?1");
    Ok(conn.query_row(&sql, params![id], row_to_profile).optional()?)
}

pub fn list_profiles(conn: &Connection) -> Result<Vec<Profile>> {
    let sql = format!("SELECT {PROFILE_COLS} FROM profiles ORDER BY profile_id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], row_to_profile)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

pub fn set_profile_config_dir(conn: &Connection, id: &str, config_dir: &str) -> Result<()> {
    conn.execute(
        "UPDATE profiles SET config_dir = ?2 WHERE profile_id = ?1",
        params![id, config_dir],
    )?;
    Ok(())
}

/// `profile login --label` on a profile that is already registered.
pub fn set_profile_account_label(conn: &Connection, id: &str, label: &str) -> Result<()> {
    conn.execute(
        "UPDATE profiles SET account_label = ?2 WHERE profile_id = ?1",
        params![id, label],
    )?;
    Ok(())
}

/// `profile check` calls this to flip a profile back to `active` once its
/// quota window has reset (`commands/profile.rs`).
pub fn set_profile_status(conn: &Connection, id: &str, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE profiles SET status = ?2 WHERE profile_id = ?1",
        params![id, status],
    )?;
    Ok(())
}

/// Whether the operator marked this profile as also used by hand
/// (`thoth profile set-shared`). A profile that does not exist is not shared.
pub fn profile_shared(conn: &Connection, id: &str) -> Result<bool> {
    let v: Option<i64> = conn
        .query_row(
            "SELECT shared FROM profiles WHERE profile_id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(v.unwrap_or(0) != 0)
}

pub fn set_profile_shared(conn: &Connection, id: &str, shared: bool) -> Result<()> {
    conn.execute(
        "UPDATE profiles SET shared = ?2 WHERE profile_id = ?1",
        params![id, shared as i64],
    )?;
    Ok(())
}

/// TIER-1: the subscription tier last seen for a profile (already normalised
/// by `plan_label::normalize`) and when. A profile that does not exist is left
/// alone.
pub fn set_profile_plan(conn: &Connection, id: &str, plan: &str, seen_at: &str) -> Result<()> {
    conn.execute(
        "UPDATE profiles SET plan = ?2, plan_seen_at = ?3 WHERE profile_id = ?1",
        params![id, plan, seen_at],
    )?;
    Ok(())
}

/// A stored tier and when it was seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPlan {
    pub plan: String,
    pub seen_at: Option<String>,
}

/// Every profile with a stored tier, keyed by profile id. A global.db that is
/// still at schema 2 (opened read-only, so never migrated) has no `plan`
/// column and reads as no tiers at all.
pub fn profile_plans(conn: &Connection) -> Result<BTreeMap<String, StoredPlan>> {
    let mut out = BTreeMap::new();
    if !crate::db::migrations::column_exists(conn, "profiles", "plan")? {
        return Ok(out);
    }
    let mut stmt = conn.prepare(
        "SELECT profile_id, plan, plan_seen_at FROM profiles WHERE plan IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                StoredPlan { plan: r.get(1)?, seen_at: r.get(2)? },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    out.extend(rows);
    Ok(out)
}

/// MODELS-1: the model list a login check read for a profile, as the JSON
/// text of `[{"model","display_name"}]`, and when. A profile that does not
/// exist is left alone.
pub fn set_profile_models(conn: &Connection, id: &str, models_json: &str, seen_at: &str) -> Result<()> {
    conn.execute(
        "UPDATE profiles SET models = ?2, models_seen_at = ?3 WHERE profile_id = ?1",
        params![id, models_json, seen_at],
    )?;
    Ok(())
}

/// A stored model list and when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredModels {
    pub models: Vec<crate::adapter::ModelEntry>,
    pub seen_at: Option<String>,
}

/// Every profile with a stored model list, keyed by profile id. A global.db
/// still at schema 3 (opened read-only, so never migrated) has no `models`
/// column and reads as no lists at all; a row whose JSON does not parse reads
/// as no list.
pub fn profile_models(conn: &Connection) -> Result<BTreeMap<String, StoredModels>> {
    let mut out = BTreeMap::new();
    if !crate::db::migrations::column_exists(conn, "profiles", "models")? {
        return Ok(out);
    }
    let mut stmt = conn.prepare(
        "SELECT profile_id, models, models_seen_at FROM profiles WHERE models IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (id, json, seen_at) in rows {
        if let Ok(models) = serde_json::from_str::<Vec<crate::adapter::ModelEntry>>(&json) {
            out.insert(id, StoredModels { models, seen_at });
        }
    }
    Ok(out)
}

/// PANEL-3: the login name (the part of the login email before `@`, already
/// cleaned by `login_name::from_email`) a login check last reported for a
/// profile, and when. A profile that does not exist is left alone.
pub fn set_profile_login_name(conn: &Connection, id: &str, name: &str, seen_at: &str) -> Result<()> {
    conn.execute(
        "UPDATE profiles SET login_name = ?2, login_name_seen_at = ?3 WHERE profile_id = ?1",
        params![id, name, seen_at],
    )?;
    Ok(())
}

/// A stored login name and when it was seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredLoginName {
    pub name: String,
    pub seen_at: Option<String>,
}

/// Every profile with a stored login name, keyed by profile id. A global.db
/// still at schema 4 (opened read-only, so never migrated) has no
/// `login_name` column and reads as no names at all.
pub fn profile_login_names(conn: &Connection) -> Result<BTreeMap<String, StoredLoginName>> {
    let mut out = BTreeMap::new();
    if !crate::db::migrations::column_exists(conn, "profiles", "login_name")? {
        return Ok(out);
    }
    let mut stmt = conn.prepare(
        "SELECT profile_id, login_name, login_name_seen_at FROM profiles WHERE login_name IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                StoredLoginName { name: r.get(1)?, seen_at: r.get(2)? },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    out.extend(rows);
    Ok(out)
}

pub fn upsert_repo(conn: &Connection, repo_id: &str, root_path: &str, now: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO repos (repo_id, root_path, created_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(repo_id) DO UPDATE SET root_path = excluded.root_path",
        params![repo_id, root_path, now],
    )?;
    Ok(())
}

pub fn insert_quota_event(
    conn: &Connection,
    ts: &str,
    profile_id: &str,
    kind: &str,
    detail: &serde_json::Value,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO quota_events (ts, profile_id, kind, detail) VALUES (?1, ?2, ?3, ?4)",
        params![ts, profile_id, kind, serde_json::to_string(detail)?],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Quota events for one profile, oldest first. `dispatch` reads this to date the
/// `rate_limited` warning it prints (Open question 21).
pub fn list_quota_events(conn: &Connection, profile_id: &str) -> Result<Vec<QuotaEvent>> {
    let mut stmt = conn.prepare(
        "SELECT seq, ts, profile_id, kind, detail FROM quota_events
         WHERE profile_id = ?1 ORDER BY seq",
    )?;
    let rows = stmt
        .query_map(params![profile_id], |r| {
            Ok(QuotaEvent {
                seq: r.get(0)?,
                ts: r.get(1)?,
                profile_id: r.get(2)?,
                kind: r.get(3)?,
                detail: r.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_global(&conn).unwrap();
        conn
    }

    fn demo(id: &str) -> Profile {
        Profile {
            profile_id: id.to_string(),
            provider: "anthropic".into(),
            harness: "claude-code".into(),
            account_label: Some("label".into()),
            config_dir: "C:/cfg".into(),
            default_models: None,
            status: "active".into(),
        }
    }

    #[test]
    fn global_crud_round_trips() {
        let conn = db();
        assert_eq!(get_profile(&conn, "claude-a").unwrap(), None);
        insert_profile(&conn, &demo("claude-b")).unwrap();
        insert_profile(&conn, &demo("claude-a")).unwrap();
        assert_eq!(get_profile(&conn, "claude-a").unwrap(), Some(demo("claude-a")));

        let all = list_profiles(&conn).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].profile_id, "claude-a", "listing is ordered by id");

        // a duplicate id must be refused by the primary key, not silently merged
        assert!(insert_profile(&conn, &demo("claude-a")).is_err());

        set_profile_status(&conn, "claude-a", "rate_limited").unwrap();
        assert_eq!(
            get_profile(&conn, "claude-a").unwrap().unwrap().status,
            "rate_limited"
        );

        upsert_repo(&conn, "abc123", "C:/repo", "2026-09-14T00:00:00Z").unwrap();
        upsert_repo(&conn, "abc123", "C:/repo-moved", "2026-09-14T01:00:00Z").unwrap();
        let (n, root): (i64, String) = conn
            .query_row("SELECT COUNT(*), MAX(root_path) FROM repos", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(n, 1, "upsert must not duplicate the row");
        assert_eq!(root, "C:/repo-moved");
    }

    #[test]
    fn quota_events_are_sequenced_and_filtered_by_profile() {
        let conn = db();
        insert_profile(&conn, &demo("claude-a")).unwrap();
        insert_profile(&conn, &demo("claude-b")).unwrap();
        let s1 = insert_quota_event(
            &conn,
            "2026-09-14T00:00:00Z",
            "claude-b",
            "rate_limited",
            &serde_json::json!({"run_id": "R1"}),
        )
        .unwrap();
        let s2 = insert_quota_event(
            &conn,
            "2026-09-14T00:01:00Z",
            "claude-a",
            "note",
            &serde_json::json!({"added": true}),
        )
        .unwrap();
        let s3 = insert_quota_event(
            &conn,
            "2026-09-14T00:02:00Z",
            "claude-b",
            "window_reset",
            &serde_json::json!({"source": "profile check"}),
        )
        .unwrap();
        assert!(s1 < s2 && s2 < s3);

        let b = list_quota_events(&conn, "claude-b").unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[0].kind, "rate_limited");
        assert!(b[0].detail.contains("\"run_id\":\"R1\""));
        assert_eq!(b[1].kind, "window_reset");
        assert_eq!(list_quota_events(&conn, "claude-a").unwrap().len(), 1);
        assert!(list_quota_events(&conn, "nobody").unwrap().is_empty());
    }

    #[test]
    fn plan_is_set_read_back_and_null_rows_are_left_out() {
        let conn = db();
        insert_profile(&conn, &demo("claude-a")).unwrap();
        insert_profile(&conn, &demo("claude-b")).unwrap();
        set_profile_plan(&conn, "claude-a", "max", "2026-10-12T00:00:00Z").unwrap();

        let plans = profile_plans(&conn).unwrap();
        assert_eq!(plans.len(), 1);
        let a = &plans["claude-a"];
        assert_eq!(a.plan, "max");
        assert_eq!(a.seen_at.as_deref(), Some("2026-10-12T00:00:00Z"));

        set_profile_plan(&conn, "nobody", "pro", "2026-10-12T00:00:00Z").unwrap();
        assert_eq!(profile_plans(&conn).unwrap(), plans);

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM quota_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn models_are_set_read_back_and_bad_json_reads_as_no_list() {
        let conn = db();
        insert_profile(&conn, &demo("agy-a")).unwrap();
        insert_profile(&conn, &demo("agy-b")).unwrap();
        let json = r#"[{"model":"a-model","display_name":"A"},{"model":"b-model","display_name":"B"}]"#;
        set_profile_models(&conn, "agy-a", json, "2026-10-19T00:00:00Z").unwrap();
        set_profile_models(&conn, "agy-b", "not json", "2026-10-19T00:00:00Z").unwrap();
        set_profile_models(&conn, "nobody", json, "2026-10-19T00:00:00Z").unwrap();
        let got = profile_models(&conn).unwrap();
        assert_eq!(got.len(), 1, "{got:?}");
        let a = &got["agy-a"];
        assert_eq!(a.seen_at.as_deref(), Some("2026-10-19T00:00:00Z"));
        assert_eq!(a.models.iter().map(|m| m.model.as_str()).collect::<Vec<_>>(), ["a-model", "b-model"]);
        assert_eq!(a.models[1].display_name, "B");
    }
}
