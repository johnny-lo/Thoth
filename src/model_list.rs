//! MODELS-1: the models each account can actually use, kept up to date by
//! the login checks Thoth already runs.
//!
//! - An antigravity profile's list is what `agy models` printed on its last
//!   login check (`profile check`, `profile login`, `dispatch`, `review
//!   start`, `spec review start`), stored on the profile row
//!   (`profiles.models`, `profiles.models_seen_at`). A launch whose model is
//!   not in the list just read (or, failing that, the stored one) is refused
//!   before anything is written; with no list at all it is let through with
//!   a warning.
//! - A claude-code profile's list is the fixed aliases, minus the models a
//!   run proved the account cannot use: the runner records a
//!   `model_unavailable` quota event when Claude answers
//!   `model_requires_usage_credits`, and the launch commands refuse that model
//!   until the account's plan changes or `thoth profile forget-model` writes
//!   a `model_forgotten` event.
//! - A codex profile's list is its own `models_cache.json`; only the text of
//!   a refusal reads it, nothing is refused by it.
//!
//! Like `plan_label`, storing is best effort (a failed write is one
//! `warning:` line on stderr) and reading never migrates a database.

use crate::adapter::{AuthStatus, ModelEntry};
use crate::db::global::{self, Profile, QuotaEvent, StoredModels};
use anyhow::{bail, Result};
use rusqlite::Connection;
use serde_json::Value;
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The Claude Code aliases every claude-code profile is assumed to have.
pub(crate) const CLAUDE_MODELS: &[&str] = &["haiku", "sonnet", "opus", "fable"];

/// `quota_events.kind` of the runner's record that a model needs credits the
/// account does not have, and of `profile forget-model`.
pub(crate) const KIND_MODEL_UNAVAILABLE: &str = "model_unavailable";
pub(crate) const KIND_MODEL_FORGOTTEN: &str = "model_forgotten";

/// Claude's `api_error` for a model the account's plan cannot run.
pub(crate) const API_ERROR_MODEL_REQUIRES_CREDITS: &str = "model_requires_usage_credits";

/// A stored list is called old after this many hours (`thoth models` says so).
const OLD_AFTER_HOURS: i64 = 24;

/// The list a login check just read, with the time it was read. `stored`
/// says whether the write succeeded; the list is used either way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Fresh {
    pub(crate) models: Vec<ModelEntry>,
    pub(crate) seen_at: String,
    pub(crate) stored: bool,
}

/// Store the list a login check reported, when it reported one. Returns
/// what was read (and whether the write succeeded); `None` when the check
/// carried no list, in which case the stored one is kept and no SQL runs.
pub(crate) fn remember(conn: &Connection, profile_id: &str, auth: &AuthStatus) -> Result<Option<Fresh>> {
    let Some(models) = auth.models.as_ref().filter(|m| !m.is_empty()) else {
        return Ok(None);
    };
    let seen_at = crate::ids::now_rfc3339();
    let json = serde_json::to_string(models)?;
    global::set_profile_models(conn, profile_id, &json, &seen_at)?;
    Ok(Some(Fresh { models: models.clone(), seen_at, stored: true }))
}

/// `remember`, with a failure turned into one warning line on stderr; the
/// list just read is still returned so the launch can check against it.
pub(crate) fn remember_or_warn(conn: &Connection, profile_id: &str, auth: &AuthStatus) -> Option<Fresh> {
    match remember(conn, profile_id, auth) {
        Ok(fresh) => fresh,
        Err(e) => {
            masked_eprintln!("warning: could not store the model list of {profile_id}: {e:#}");
            auth.models.as_ref().filter(|m| !m.is_empty()).map(|models| Fresh {
                models: models.clone(),
                seen_at: crate::ids::now_rfc3339(),
                stored: false,
            })
        }
    }
}

/// The stored list of one profile, or `None`. A read error reads as none:
/// nothing here fails a command for a list it cannot read.
pub(crate) fn stored(conn: &Connection, profile_id: &str) -> Option<StoredModels> {
    global::profile_models(conn).ok()?.remove(profile_id)
}

/// `s` re-formatted as RFC 3339 in UTC (`…Z`), or `None` when it is not
/// RFC 3339. The same instant whatever offset it was written in.
fn rfc3339_utc(s: &str) -> Option<String> {
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()?
        .to_offset(time::UtcOffset::UTC)
        .format(&Rfc3339)
        .ok()
}

/// A codex account's `models_cache.json`: the slugs `thoth models` lists
/// (`visibility` `list`, by `priority`) and when codex fetched it (the
/// top-level `fetched_at` when it is RFC 3339, else the file's modification
/// time, else nothing), as RFC 3339 UTC. `None` when the file is missing or
/// unreadable.
pub(crate) fn codex_cache(config_dir: &Path) -> Option<StoredModels> {
    let path = config_dir.join("models_cache.json");
    let text = std::fs::read_to_string(&path).ok()?;
    let parsed: Value = serde_json::from_str(&text).ok()?;
    let entries = parsed.get("models")?.as_array()?;
    let mut selected: Vec<&Value> = entries
        .iter()
        .filter(|v| v.get("visibility").and_then(Value::as_str) == Some("list"))
        .collect();
    selected.sort_by_key(|v| v.get("priority").and_then(Value::as_i64).unwrap_or(i64::MAX));
    let models = selected
        .into_iter()
        .filter_map(|v| {
            Some(ModelEntry {
                model: v.get("slug")?.as_str()?.to_string(),
                display_name: v.get("display_name")?.as_str()?.to_string(),
            })
        })
        .collect();
    let fetched = parsed.get("fetched_at").and_then(Value::as_str).and_then(rfc3339_utc);
    let seen_at = fetched.or_else(|| {
        let modified = std::fs::metadata(&path).ok()?.modified().ok()?;
        OffsetDateTime::from(modified).to_offset(time::UtcOffset::UTC).format(&Rfc3339).ok()
    });
    Some(StoredModels { models, seen_at })
}

/// The slugs of a list, comma-separated, in its order.
pub(crate) fn slugs(models: &[ModelEntry]) -> String {
    models.iter().map(|m| m.model.as_str()).collect::<Vec<_>>().join(", ")
}

/// The sentence a refusal ends with when no list is stored.
pub(crate) fn none_stored_text(profile_id: &str) -> String {
    format!("no model list stored; run `thoth profile check {profile_id}`")
}

/// What a model-rule refusal (`HarnessAdapter::check_model`) lists as the
/// models to pass instead: an antigravity profile's stored slugs (or the
/// sentence saying none is stored); a codex profile's cache slugs, else
/// `CODEX_MODELS`; nothing for claude-code, whose rule never refuses.
pub(crate) fn known_models_text(gconn: &Connection, profile: &Profile) -> String {
    match profile.harness.as_str() {
        crate::adapter::antigravity::HARNESS_NAME => match stored(gconn, &profile.profile_id) {
            Some(s) if !s.models.is_empty() => slugs(&s.models),
            _ => none_stored_text(&profile.profile_id),
        },
        crate::adapter::HARNESS_CODEX => match codex_cache(Path::new(&profile.config_dir)) {
            Some(c) if !c.models.is_empty() => slugs(&c.models),
            _ => crate::adapter::codex::CODEX_MODELS.join(", "),
        },
        _ => String::new(),
    }
}

/// Whether `model` is in `models`, both sides trimmed and compared without
/// regard to ASCII case.
fn lists(models: &[ModelEntry], model: &str) -> bool {
    let wanted = model.trim();
    models.iter().any(|m| m.model.trim().eq_ignore_ascii_case(wanted))
}

/// The launch gate (AC3, AC5): after the login check passed and the list and
/// plan were stored, refuse a model this account cannot use, before any run
/// row, event or worktree. `fresh` is the list the login check just read
/// (none for `resume`, which only has the stored one). Returns the warning
/// to add when the model could not be checked.
pub(crate) fn check_launch(
    gconn: &Connection,
    profile: &Profile,
    model: &str,
    fresh: Option<&Fresh>,
) -> Result<Option<String>> {
    match profile.harness.as_str() {
        crate::adapter::antigravity::HARNESS_NAME => {
            let (models, seen_at) = match fresh {
                Some(f) => (f.models.clone(), f.seen_at.clone()),
                None => match stored(gconn, &profile.profile_id) {
                    Some(s) if !s.models.is_empty() => {
                        (s.models, s.seen_at.unwrap_or_else(|| "an unrecorded time".to_string()))
                    }
                    _ => {
                        return Ok(Some(format!(
                            "no model list for {}; model {model} was not checked",
                            profile.profile_id
                        )))
                    }
                },
            };
            if lists(&models, model) {
                return Ok(None);
            }
            bail!(
                "profile {id} runs antigravity, and '{model}' is not in its model list (read at \
                 {seen_at}): {list}; run `thoth profile check {id}` to read the list again",
                id = profile.profile_id,
                list = slugs(&models)
            )
        }
        crate::adapter::HARNESS_CLAUDE_CODE => {
            let events = global::list_quota_events(gconn, &profile.profile_id)?;
            let plan = current_plan(gconn, &profile.profile_id);
            let Some(u) = unavailable_now(&events, plan.as_deref(), model) else {
                return Ok(None);
            };
            let still: Vec<&str> = CLAUDE_MODELS
                .iter()
                .copied()
                .filter(|m| unavailable_now(&events, plan.as_deref(), m).is_none())
                .collect();
            bail!(
                "profile {id} cannot run model '{model}': a run on this account was refused at \
                 {seen_at} ({reason}); models this profile can still use: {still}; if the account \
                 has changed, run `thoth profile forget-model {id} {model}`",
                id = profile.profile_id,
                seen_at = u.seen_at,
                reason = u.reason,
                still = if still.is_empty() { "none".to_string() } else { still.join(", ") }
            )
        }
        _ => Ok(None),
    }
}

/// The profile's stored plan right now (`None` when none, or unreadable).
pub(crate) fn current_plan(gconn: &Connection, profile_id: &str) -> Option<String> {
    global::profile_plans(gconn).ok()?.remove(profile_id).map(|p| p.plan)
}

/// What `thoth models` shows for a model a run proved unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unavailable {
    pub(crate) seen_at: String,
    pub(crate) reason: String,
    pub(crate) run_id: Option<String>,
}

fn detail_of(event: &QuotaEvent) -> Value {
    serde_json::from_str(&event.detail).unwrap_or(Value::Null)
}

fn detail_model_is(detail: &Value, model: &str) -> bool {
    detail["model"]
        .as_str()
        .is_some_and(|m| m.trim().eq_ignore_ascii_case(model.trim()))
}

/// Whether `model` counts as unavailable on this account now: the newest
/// `model_unavailable` / `model_forgotten` event naming it (any letter case)
/// is a `model_unavailable` whose recorded `plan` equals `current_plan`
/// (null equals null). A later `model_forgotten`, or a plan that changed
/// since, clears it. `events` are oldest first, as `list_quota_events` gives
/// them.
pub(crate) fn unavailable_now(events: &[QuotaEvent], current_plan: Option<&str>, model: &str) -> Option<Unavailable> {
    let newest = events
        .iter()
        .rev()
        .map(|e| (e, detail_of(e)))
        .find(|(e, d)| {
            (e.kind == KIND_MODEL_UNAVAILABLE || e.kind == KIND_MODEL_FORGOTTEN) && detail_model_is(d, model)
        })?;
    let (event, detail) = newest;
    if event.kind != KIND_MODEL_UNAVAILABLE {
        return None;
    }
    let recorded_plan = detail["plan"].as_str();
    if recorded_plan.map(str::trim) != current_plan.map(str::trim) {
        return None;
    }
    Some(Unavailable {
        seen_at: event.ts.clone(),
        reason: detail["error_text"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("the harness refused the model")
            .to_string(),
        run_id: detail["run_id"].as_str().map(str::to_string),
    })
}

/// `thoth profile forget-model`: one `model_forgotten` quota event, nothing
/// else on the profile.
pub(crate) fn forget(conn: &Connection, profile_id: &str, model: &str) -> Result<i64> {
    global::insert_quota_event(
        conn,
        &crate::ids::now_rfc3339(),
        profile_id,
        KIND_MODEL_FORGOTTEN,
        &serde_json::json!({ "model": model, "source": "profile forget-model" }),
    )
}

/// Whole hours between `seen_at` and now, or `None` when it does not parse.
pub(crate) fn hours_since(seen_at: &str) -> Option<i64> {
    let seen = OffsetDateTime::parse(seen_at, &Rfc3339).ok()?;
    Some((OffsetDateTime::now_utc() - seen).whole_hours())
}

/// `thoth models`' note for an antigravity profile: none stored, or stored
/// more than a day ago.
pub(crate) fn agy_note(profile_id: &str, stored: Option<&StoredModels>) -> Option<String> {
    match stored {
        None => Some(format!("no model list stored yet; run `thoth profile check {profile_id}`")),
        Some(s) if s.models.is_empty() => {
            Some(format!("no model list stored yet; run `thoth profile check {profile_id}`"))
        }
        Some(s) => {
            let hours = s.seen_at.as_deref().and_then(hours_since)?;
            (hours >= OLD_AFTER_HOURS).then(|| {
                format!(
                    "model list read {hours} hours ago; run `thoth profile check {profile_id}` to \
                     read it again"
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::migrations;

    fn profile(id: &str, harness: &str) -> Profile {
        Profile {
            profile_id: id.into(),
            provider: "x".into(),
            harness: harness.into(),
            account_label: None,
            config_dir: "/c".into(),
            default_models: None,
            status: "active".into(),
        }
    }

    fn entry(model: &str) -> ModelEntry {
        ModelEntry { model: model.into(), display_name: model.to_uppercase() }
    }

    fn auth_with(models: &[&str]) -> AuthStatus {
        AuthStatus {
            logged_in: true,
            models: Some(models.iter().map(|m| entry(m)).collect()),
            ..Default::default()
        }
    }

    fn quota(kind: &str, ts: &str, detail: Value) -> QuotaEvent {
        QuotaEvent { seq: 0, ts: ts.into(), profile_id: "claude-b".into(), kind: kind.into(), detail: detail.to_string() }
    }

    /// AC6 (t8): a v3 global.db (no `models` column), as a read-only open
    /// would see it, reads as no list and nothing fails.
    #[test]
    fn model_list_reads_a_v3_table_as_no_list() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(migrations::GLOBAL_V1).unwrap();
        migrations::add_column_if_missing(&conn, "profiles", "shared", "INTEGER NOT NULL DEFAULT 0").unwrap();
        migrations::add_column_if_missing(&conn, "profiles", "plan", "TEXT").unwrap();
        migrations::add_column_if_missing(&conn, "profiles", "plan_seen_at", "TEXT").unwrap();
        conn.execute("INSERT OR IGNORE INTO schema_version (version) VALUES (3)", []).unwrap();
        global::insert_profile(&conn, &profile("agy-a", "antigravity")).unwrap();

        assert_eq!(stored(&conn, "agy-a"), None);
        assert!(global::profile_models(&conn).unwrap().is_empty());
        assert_eq!(known_models_text(&conn, &profile("agy-a", "antigravity")), none_stored_text("agy-a"));
        let warning = check_launch(&conn, &profile("agy-a", "antigravity"), "x-model", None).unwrap();
        assert_eq!(warning.as_deref(), Some("no model list for agy-a; model x-model was not checked"));
        assert!(!migrations::column_exists(&conn, "profiles", "models").unwrap());
        assert_eq!(migrations::current_version(&conn).unwrap(), 3);
    }

    #[test]
    fn remember_stores_a_list_and_keeps_the_old_one_without_a_new_list() {
        let conn = Connection::open_in_memory().unwrap();
        migrations::migrate_global(&conn).unwrap();
        global::insert_profile(&conn, &profile("agy-a", "antigravity")).unwrap();
        let fresh = remember(&conn, "agy-a", &auth_with(&["a-model", "b-model"])).unwrap().unwrap();
        assert!(fresh.stored);
        assert!(fresh.seen_at.ends_with('Z'));
        let s = stored(&conn, "agy-a").unwrap();
        assert_eq!(slugs(&s.models), "a-model, b-model");
        assert_eq!(s.seen_at.as_deref(), Some(fresh.seen_at.as_str()));
        assert_eq!(s.models[0].display_name, "A-MODEL");

        assert_eq!(remember(&conn, "agy-a", &AuthStatus { logged_in: true, ..Default::default() }).unwrap(), None);
        assert_eq!(remember(&conn, "agy-a", &auth_with(&[])).unwrap(), None);
        assert_eq!(stored(&conn, "agy-a").unwrap(), s);
        assert_eq!(known_models_text(&conn, &profile("agy-a", "antigravity")), "a-model, b-model");
    }

    #[test]
    fn check_launch_prefers_the_fresh_list_and_compares_trimmed_without_case() {
        let conn = Connection::open_in_memory().unwrap();
        migrations::migrate_global(&conn).unwrap();
        global::insert_profile(&conn, &profile("agy-a", "antigravity")).unwrap();
        let p = profile("agy-a", "antigravity");
        remember(&conn, "agy-a", &auth_with(&["a-model"])).unwrap();
        // stored only
        assert_eq!(check_launch(&conn, &p, " A-MODEL ", None).unwrap(), None);
        let e = check_launch(&conn, &p, "c-model", None).unwrap_err().to_string();
        assert!(e.contains("'c-model' is not in its model list (read at 2"), "{e}");
        assert!(e.ends_with("a-model; run `thoth profile check agy-a` to read the list again"), "{e}");
        // the fresh list wins over the stored one
        let fresh = Fresh { models: vec![entry("c-model")], seen_at: "2026-10-19T01:02:03Z".into(), stored: false };
        assert_eq!(check_launch(&conn, &p, "c-model", Some(&fresh)).unwrap(), None);
        let e = check_launch(&conn, &p, "a-model", Some(&fresh)).unwrap_err().to_string();
        assert!(e.contains("read at 2026-10-19T01:02:03Z"), "{e}");
        // other harnesses: nothing to check
        assert_eq!(check_launch(&conn, &profile("codex-a", "codex"), "haiku", None).unwrap(), None);
    }

    #[test]
    fn unavailable_now_follows_the_newest_event_and_the_plan() {
        let unavailable = |ts: &str, model: &str, plan: Option<&str>| {
            quota(KIND_MODEL_UNAVAILABLE, ts, serde_json::json!({
                "model": model, "run_id": "R1", "error_text": "Fable 5.1 requires usage credits.", "plan": plan
            }))
        };
        let forgotten = |ts: &str, model: &str| {
            quota(KIND_MODEL_FORGOTTEN, ts, serde_json::json!({"model": model, "source": "profile forget-model"}))
        };
        let events = vec![unavailable("2026-10-18T00:00:00Z", "fable", Some("pro"))];
        let u = unavailable_now(&events, Some("pro"), "FABLE").unwrap();
        assert_eq!(u.seen_at, "2026-10-18T00:00:00Z");
        assert_eq!(u.reason, "Fable 5.1 requires usage credits.");
        assert_eq!(u.run_id.as_deref(), Some("R1"));
        assert_eq!(unavailable_now(&events, Some("max"), "fable"), None);
        assert_eq!(unavailable_now(&events, None, "fable"), None);
        assert_eq!(unavailable_now(&events, Some("pro"), "opus"), None);

        let events = vec![
            unavailable("2026-10-18T00:00:00Z", "fable", None),
            forgotten("2026-10-18T01:00:00Z", "Fable"),
        ];
        assert_eq!(unavailable_now(&events, None, "fable"), None);
        let events = vec![
            forgotten("2026-10-18T00:00:00Z", "fable"),
            unavailable("2026-10-18T01:00:00Z", "fable", None),
        ];
        assert!(unavailable_now(&events, None, "fable").is_some());
        assert_eq!(unavailable_now(&[quota("rate_limited", "t", serde_json::json!({"model": "fable"}))], None, "fable"), None);
    }

    #[test]
    fn claude_launch_is_refused_only_while_the_record_holds() {
        let conn = Connection::open_in_memory().unwrap();
        migrations::migrate_global(&conn).unwrap();
        global::insert_profile(&conn, &profile("claude-b", "claude-code")).unwrap();
        global::set_profile_plan(&conn, "claude-b", "pro", "2026-10-19T00:00:00Z").unwrap();
        let p = profile("claude-b", "claude-code");
        assert_eq!(check_launch(&conn, &p, "fable", None).unwrap(), None);
        global::insert_quota_event(&conn, "2026-10-19T00:00:01Z", "claude-b", KIND_MODEL_UNAVAILABLE,
            &serde_json::json!({"model": "fable", "run_id": "R1", "error_text": "needs credits", "plan": "pro"})).unwrap();
        let e = check_launch(&conn, &p, "Fable", None).unwrap_err().to_string();
        assert!(e.contains("claude-b cannot run model 'Fable'"), "{e}");
        assert!(e.contains("2026-10-19T00:00:01Z (needs credits)"), "{e}");
        assert!(e.contains("still use: haiku, sonnet, opus;"), "{e}");
        assert!(e.contains("thoth profile forget-model claude-b Fable"), "{e}");
        assert_eq!(check_launch(&conn, &p, "sonnet", None).unwrap(), None);
        // the plan changed: the record no longer applies
        global::set_profile_plan(&conn, "claude-b", "max", "2026-10-19T00:00:02Z").unwrap();
        assert_eq!(check_launch(&conn, &p, "fable", None).unwrap(), None);
        global::set_profile_plan(&conn, "claude-b", "pro", "2026-10-19T00:00:03Z").unwrap();
        assert!(check_launch(&conn, &p, "fable", None).is_err());
        forget(&conn, "claude-b", "FABLE").unwrap();
        assert_eq!(check_launch(&conn, &p, "fable", None).unwrap(), None);
        // what `profile forget-model` wrote: one event, the model as given
        let events = global::list_quota_events(&conn, "claude-b").unwrap();
        let forgotten: Vec<&QuotaEvent> = events.iter().filter(|e| e.kind == KIND_MODEL_FORGOTTEN).collect();
        assert_eq!(forgotten.len(), 1, "{events:?}");
        assert_eq!(
            serde_json::from_str::<Value>(&forgotten[0].detail).unwrap(),
            serde_json::json!({"model": "FABLE", "source": "profile forget-model"})
        );
        assert_eq!(global::get_profile(&conn, "claude-b").unwrap().unwrap().status, "active");
    }

    #[test]
    fn codex_cache_lists_visible_slugs_and_reads_fetched_at_or_the_mtime() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(codex_cache(dir.path()), None);
        std::fs::write(dir.path().join("models_cache.json"), serde_json::json!({
            "fetched_at": "2026-10-16T07:15:39.244865800Z",
            "models": [
                {"slug": "late", "display_name": "Late", "visibility": "list", "priority": 20},
                {"slug": "hidden", "display_name": "Hidden", "visibility": "hide", "priority": 0},
                {"slug": "gpt-5.6-luna", "display_name": "Luna", "visibility": "list", "priority": 10}
            ]
        }).to_string()).unwrap();
        let c = codex_cache(dir.path()).unwrap();
        assert_eq!(slugs(&c.models), "gpt-5.6-luna, late");
        // the real file's shape: already UTC; re-formatted, so the trailing
        // zeros of the fraction go
        assert_eq!(c.seen_at.as_deref(), Some("2026-10-16T07:15:39.2448658Z"));
        std::fs::write(dir.path().join("models_cache.json"), serde_json::json!({
            "fetched_at": "yesterday", "models": []
        }).to_string()).unwrap();
        let c = codex_cache(dir.path()).unwrap();
        assert!(c.models.is_empty());
        let seen = c.seen_at.expect("the file's modification time");
        assert!(OffsetDateTime::parse(&seen, &Rfc3339).is_ok(), "{seen}");
        assert!(seen.ends_with('Z'), "{seen}");
        std::fs::write(dir.path().join("models_cache.json"), "not json").unwrap();
        assert_eq!(codex_cache(dir.path()), None);
    }

    /// AC2 (review 1): a `fetched_at` with a non-zero offset is the same
    /// instant in UTC, not the offset form codex wrote.
    #[test]
    fn codex_cache_reports_fetched_at_in_utc_whatever_offset_it_was_written_in() {
        let dir = tempfile::tempdir().unwrap();
        for (written, utc) in [
            ("2026-10-16T15:15:39+08:00", "2026-10-16T07:15:39Z"),
            ("2026-10-16T03:15:39.5-04:00", "2026-10-16T07:15:39.5Z"),
            ("2026-10-16T07:15:39+00:00", "2026-10-16T07:15:39Z"),
            ("2026-10-16T07:15:39Z", "2026-10-16T07:15:39Z"),
        ] {
            std::fs::write(
                dir.path().join("models_cache.json"),
                serde_json::json!({"fetched_at": written, "models": []}).to_string(),
            )
            .unwrap();
            let c = codex_cache(dir.path()).unwrap();
            assert_eq!(c.seen_at.as_deref(), Some(utc), "written as {written}");
        }
    }

    #[test]
    fn agy_note_says_none_stored_or_how_old() {
        assert_eq!(agy_note("agy-a", None).as_deref(), Some("no model list stored yet; run `thoth profile check agy-a`"));
        let empty = StoredModels { models: vec![], seen_at: Some("2026-10-19T00:00:00Z".into()) };
        assert!(agy_note("agy-a", Some(&empty)).unwrap().starts_with("no model list stored yet"));
        let fresh = StoredModels { models: vec![entry("a")], seen_at: Some(crate::ids::now_rfc3339()) };
        assert_eq!(agy_note("agy-a", Some(&fresh)), None);
        let old = StoredModels { models: vec![entry("a")], seen_at: Some("2020-01-01T00:00:00Z".into()) };
        let note = agy_note("agy-a", Some(&old)).unwrap();
        assert!(note.starts_with("model list read ") && note.contains(" hours ago; run `thoth profile check agy-a`"), "{note}");
        let unparsable = StoredModels { models: vec![entry("a")], seen_at: Some("whenever".into()) };
        assert_eq!(agy_note("agy-a", Some(&unparsable)), None);
    }
}
