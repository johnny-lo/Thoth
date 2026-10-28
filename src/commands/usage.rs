//! Read-only quota reports and profile recommendations.

use crate::config::Paths;
use crate::db::global::Profile;
use crate::ids;
use crate::lang::Lang;
use crate::output;
use crate::plan_label;
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};

/// A window at or past this is used up; `own_usage` counts the share below it
/// as what is left for runs (the rest is kept for the person).
pub(crate) const EXHAUSTED_PERCENT: f64 = 95.0;
const FRESH_SECS: i64 = 86_400;

#[derive(Clone, Debug)]
pub(crate) struct Window {
    pub(crate) name: String,
    pub(crate) used: f64,
    pub(crate) resets_at: Option<i64>,
    pub(crate) observed_at: String,
    pub(crate) source: &'static str,
    pub(crate) group: Option<String>,
    pub(crate) group_description: Option<String>,
}

impl Window {
    fn json(&self) -> Value {
        json!({
            "name": self.name,
            "used_percent": self.used.clamp(0.0, 100.0),
            "resets_at": self.resets_at.and_then(rfc_from_epoch),
            "observed_at": normalize_rfc(&self.observed_at),
            "source": self.source,
            "reset_since_observed": self.is_reset(),
            "group": self.group,
        })
    }

    /// True once the window's reset time has passed since it was read: the
    /// one reset predicate behind `reset_since_observed`, the 95% exclusion,
    /// the ranking peak and the report page.
    pub(crate) fn is_reset(&self) -> bool {
        self.resets_at
            .is_some_and(|t| t <= OffsetDateTime::now_utc().unix_timestamp())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RankedCandidate {
    pub(crate) profile: Profile,
    live_runs: i64,
    pub(crate) eligible: bool,
    different_provider: bool,
    pub(crate) quota_state: &'static str,
    pub(crate) peak_percent: Option<f64>,
    pub(crate) newest_observed_at: Option<String>,
    pub(crate) recovered: bool,
    pub(crate) quota_group: Option<String>,
    /// FC-1 (H4): the quota group a `rate_limited` profile was parked for
    /// (`parked_group`); `None` for any other status.
    pub(crate) parked_group: Option<String>,
    /// FC-1 (H4): parked for one quota group while the selected group is
    /// another (`is_parked_elsewhere`), so the profile is usable for it.
    pub(crate) parked_elsewhere: bool,
    pub(crate) reasons: Vec<String>,
    /// The profile's current windows, for `own_usage`'s runs-left count
    /// (RJ-2); every window, not only the decision group's.
    pub(crate) windows: Vec<Window>,
}

/// FC-1 (H4): the `quota_group` of the profile's newest `rate_limited`
/// quota event (the group an antigravity profile was parked for), or `None`
/// when there is none or it was not recorded.
pub(crate) fn parked_group(global: &Connection, profile_id: &str) -> Result<Option<String>> {
    let detail: Option<String> = global
        .query_row(
            "SELECT detail FROM quota_events WHERE profile_id=?1 AND kind='rate_limited' ORDER BY seq DESC LIMIT 1",
            [profile_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(detail
        .and_then(|d| serde_json::from_str::<Value>(&d).ok())
        .and_then(|d| d.get("quota_group").and_then(Value::as_str).map(str::to_string)))
}

/// FC-1 (H4): a profile parked for quota group `parked` is still usable when
/// the selected group is known and is another one.
pub(crate) fn is_parked_elsewhere(parked: Option<&str>, selected: Option<&str>) -> bool {
    matches!((parked, selected), (Some(g), Some(h)) if g != h)
}

fn quota_state(windows: &[Window], now: i64) -> (&'static str, Option<f64>, Option<String>) {
    let newest = windows
        .iter()
        .filter_map(|w| OffsetDateTime::parse(&w.observed_at, &Rfc3339).ok())
        .max()
        .map(|t| t.to_offset(UtcOffset::UTC));
    if windows
        .iter()
        .any(|w| w.used >= EXHAUSTED_PERCENT && w.resets_at.is_none_or(|t| t > now))
    {
        return (
            "exhausted",
            None,
            newest.and_then(|t| t.format(&Rfc3339).ok()),
        );
    }
    if windows.is_empty()
        || newest.is_none_or(|t| now.saturating_sub(t.unix_timestamp()) > FRESH_SECS)
    {
        return (
            "unknown",
            None,
            newest.and_then(|t| t.format(&Rfc3339).ok()),
        );
    }
    let peak = windows
        .iter()
        .filter(|w| w.resets_at.is_none_or(|t| t > now))
        .map(|w| w.used)
        .fold(0.0, f64::max);
    (
        "ok",
        Some(peak),
        newest.and_then(|t| t.format(&Rfc3339).ok()),
    )
}

fn decision_windows(windows: &[Window], model: Option<&str>, now: i64) -> (Vec<Window>, Option<String>, Option<String>) {
    let mut names = Vec::new();
    for window in windows {
        if let Some(group) = &window.group {
            if !names.contains(group) { names.push(group.clone()) }
        }
    }
    if names.is_empty() { return (windows.to_vec(), None, None) }
    let peak = |group: &String| windows.iter().filter(|w| w.group.as_ref() == Some(group)
        && w.resets_at.is_none_or(|t| t > now)).map(|w| w.used).fold(0.0, f64::max);
    let mut best: Option<String> = None;
    for group in &names {
        if windows.iter().any(|w| w.group.as_ref() == Some(group)
            && w.used >= EXHAUSTED_PERCENT && w.resets_at.is_none_or(|t| t > now)) { continue }
        if best.as_ref().is_none_or(|old| peak(group) < peak(old)) { best = Some(group.clone()) }
    }
    let best = best.unwrap_or_else(|| names[0].clone());
    let (selected, explanation) = if let Some(model) = model {
        let matches = super::live_quota::matching_groups(windows, model);
        match matches.as_slice() {
            [one] => (one.clone(), format!("quota group {one} for model {model}")),
            [] => (best, format!("model {model} matches no group; using best of {} groups", names.len())),
            _ => (best, format!("model {model} matches more than one group; using best of {} groups", names.len())),
        }
    } else { (best, format!("quota group selected as best of {} groups; use --model to select by model", names.len())) };
    (windows.iter().filter(|w| w.group.as_deref() == Some(&selected)).cloned().collect(), Some(selected), Some(explanation))
}

pub(crate) fn read_only(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening {} read-only", path.display()))
}

pub(crate) fn global_read_only(paths: &Paths) -> Result<Connection> {
    read_only(&paths.global_db())
}

/// A profile's stored tier and its bracket text (TIER-1), for `--json`.
fn plan_fields(
    plans: &BTreeMap<String, crate::db::global::StoredPlan>,
    p: &Profile,
) -> (Option<String>, Option<String>) {
    let plan = plans.get(&p.profile_id).map(|s| s.plan.clone());
    let bracket = plan_label::label(plan.as_deref(), p.account_label.as_deref());
    (plan, bracket)
}

pub(crate) fn registered_repos(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT repo_id FROM repos ORDER BY repo_id")?;
    let rows = stmt
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn normalize_rfc(value: &str) -> String {
    OffsetDateTime::parse(value, &Rfc3339)
        .map(|t| {
            t.to_offset(UtcOffset::UTC)
                .format(&Rfc3339)
                .unwrap_or_else(|_| value.to_string())
        })
        .unwrap_or_else(|_| value.to_string())
}

fn rfc_from_epoch(value: i64) -> Option<String> {
    OffsetDateTime::from_unix_timestamp(value)
        .ok()?
        .to_offset(UtcOffset::UTC)
        .format(&Rfc3339)
        .ok()
}

fn claude_windows(paths: &Paths, profile: &str, repos: &[String]) -> Result<Vec<Window>> {
    let mut newest: std::collections::HashMap<String, Window> = std::collections::HashMap::new();
    for repo in repos {
        let path = paths.state_db(repo);
        if !path.is_file() {
            continue;
        }
        let conn = read_only(&path)?;
        let mut stmt = conn.prepare(
            "SELECT e.ts, e.detail FROM events e JOIN runs r ON r.run_id=e.run_id
             WHERE r.account=?1 AND e.type='rate_limit_warning'",
        )?;
        let rows = stmt.query_map([profile], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (ts, detail) = row?;
            let d: Value = match serde_json::from_str(&detail) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let readings: Vec<(&str, f64, Option<i64>)> =
                if let Some(windows) = d.get("windows").and_then(Value::as_array) {
                    windows
                        .iter()
                        .filter_map(|w| {
                            Some((
                                w.get("name")?.as_str()?,
                                w.get("utilization")?.as_f64()?,
                                w.get("resets_at").and_then(Value::as_i64),
                            ))
                        })
                        .collect()
                } else {
                    match (
                        d.get("rate_limit_type").and_then(Value::as_str),
                        d.get("utilization").and_then(Value::as_f64),
                    ) {
                        (Some(name), Some(util)) => {
                            vec![(name, util, d.get("resets_at").and_then(Value::as_i64))]
                        }
                        _ => Vec::new(),
                    }
                };
            for (name, util, resets_at) in readings {
                let w = Window {
                    name: name.to_string(),
                    used: util * 100.0,
                    resets_at,
                    observed_at: ts.clone(),
                    source: "thoth_events",
                    group: None,
                    group_description: None,
                };
                let replace = newest
                    .get(name)
                    .is_none_or(|old| w.observed_at > old.observed_at);
                if replace {
                    newest.insert(name.to_string(), w);
                }
            }
        }
    }
    let mut out: Vec<_> = newest.into_values().collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Every file under `dir`, recursively, with its modification time. Also
/// used by `run_quota` to find one run's rollout file.
pub(crate) fn rollout_files(dir: &Path, out: &mut Vec<(SystemTime, PathBuf)>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            rollout_files(&path, out)?;
        } else if path.is_file() {
            out.push((
                entry
                    .metadata()?
                    .modified()
                    .unwrap_or(SystemTime::UNIX_EPOCH),
                path,
            ));
        }
    }
    Ok(())
}

/// A codex quota bucket other than the account's general one, such as the
/// per-model reserve codex 0.154 writes (limit_id "base_model_inference",
/// limit_name "gpt-reserve"). Shown by `profile usage` and `report` only;
/// `recommend` and `dispatch` never look at it.
#[derive(Clone, Debug)]
pub(crate) struct ExtraBucket {
    pub(crate) limit_id: String,
    pub(crate) limit_name: Option<String>,
    pub(crate) windows: Vec<Window>,
}

impl ExtraBucket {
    fn json(&self) -> Value {
        json!({
            "limit_id": self.limit_id,
            "limit_name": self.limit_name,
            "windows": self.windows.iter().map(Window::json).collect::<Vec<_>>(),
        })
    }
}

/// A codex window's name from its length in minutes; shared by the rollout
/// reading and the app-server reading.
pub(crate) fn codex_window_name(minutes: i64) -> String {
    match minutes {
        300 => "five_hour".to_string(),
        10080 => "seven_day".to_string(),
        n => format!("{n}m"),
    }
}

/// The `primary` / `secondary` windows of one `rate_limits` object.
pub(crate) fn codex_limit_windows(limits: &Value, observed: &str) -> Vec<Window> {
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        let Some(limit) = limits.get(key).filter(|v| !v.is_null()) else {
            continue;
        };
        let Some(minutes) = limit.get("window_minutes").and_then(Value::as_i64) else {
            continue;
        };
        let name = codex_window_name(minutes);
        let Some(used) = limit.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        windows.push(Window {
            name,
            used,
            resets_at: limit.get("resets_at").and_then(Value::as_i64),
            observed_at: observed.to_string(),
            source: "codex_rollout",
            group: None,
            group_description: None,
        });
    }
    windows
}

/// One rollout line's `(timestamp, payload.rate_limits)` when the line is an
/// `event_msg` whose payload is a `token_count` with a non-null
/// `rate_limits`; `None` for every other line, unparsable ones included.
/// Also used by `run_quota`.
pub(crate) fn token_count_limits(line: &str) -> Option<(String, Value)> {
    let value: Value = serde_json::from_str(line).ok()?;
    if value.get("type").and_then(Value::as_str) != Some("event_msg")
        || value.pointer("/payload/type").and_then(Value::as_str) != Some("token_count")
    {
        return None;
    }
    let timestamp = value.get("timestamp").and_then(Value::as_str)?;
    let limits = value.pointer("/payload/rate_limits")?;
    if limits.is_null() {
        return None;
    }
    Some((timestamp.to_string(), limits.clone()))
}

/// The account's general bucket: `limit_id` is `codex`, or absent (older
/// codex versions wrote none).
pub(crate) fn is_general_bucket(limits: &Value) -> bool {
    matches!(limits.get("limit_id").and_then(Value::as_str), None | Some("codex"))
}

/// The general bucket's windows plus every other bucket, all from the one
/// rollout file that supplied the general bucket, and the timestamp of the
/// line that supplied it (None when no rollout file has one).
fn codex_reading(config_dir: &str) -> Result<(Option<String>, Vec<Window>, Vec<ExtraBucket>)> {
    let sessions = Path::new(config_dir).join("sessions");
    let mut files = Vec::new();
    rollout_files(&sessions, &mut files)?;
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    for (_, path) in files {
        let text = match fs::read_to_string(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut found = None;
        // Last line per other bucket, in file order; keyed so the output is
        // ordered by limit_id.
        let mut others: std::collections::BTreeMap<String, (String, Value)> =
            std::collections::BTreeMap::new();
        for line in text.lines() {
            let Some((timestamp, limits)) = token_count_limits(line) else {
                continue;
            };
            // Only the account's general bucket goes into `windows`. codex
            // 0.154 also writes per-model reserve buckets (limit_id
            // "base_model_inference", limit_name "gpt-reserve") on later lines
            // of the same run; the newest line alone would hide the 5-hour
            // and weekly limits. Those go to the extra buckets instead.
            // Older codex versions wrote no limit_id at all.
            if is_general_bucket(&limits) {
                found = Some((timestamp, limits));
            } else if let Some(id) = limits.get("limit_id").and_then(Value::as_str) {
                others.insert(id.to_string(), (timestamp, limits.clone()));
            }
        }
        if let Some((observed, limits)) = found {
            let windows = codex_limit_windows(&limits, &observed);
            let extras = others
                .into_iter()
                .map(|(limit_id, (observed, limits))| ExtraBucket {
                    limit_id,
                    limit_name: limits
                        .get("limit_name")
                        .and_then(Value::as_str)
                        .map(str::to_string),
                    windows: codex_limit_windows(&limits, &observed),
                })
                .collect();
            return Ok((Some(observed), windows, extras));
        }
    }
    Ok((None, Vec::new(), Vec::new()))
}

/// One profile's quota reading: the windows, the extra buckets, and whether
/// codex's newest live reading says ordinary usage is not allowed.
struct Reading {
    windows: Vec<Window>,
    extras: Vec<ExtraBucket>,
    /// The `ts` of that live reading when it counts (AC5 of S3-11).
    usage_not_allowed_at: Option<String>,
}

fn parse_time(value: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(value, &Rfc3339).ok()
}

/// A codex profile's reading: the rollout reading and the newest valid live
/// (app-server) reading, whichever was observed later; the extra buckets
/// always come from the rollout file. Ordinary usage counts as not allowed
/// only when that newest live reading says so, is at most 24 hours old and is
/// newer than the newest rollout reading.
fn codex_full_reading(paths: &Paths, profile: &Profile) -> Result<Reading> {
    let (rollout_at, rollout_windows, extras) = codex_reading(&profile.config_dir)?;
    let global = global_read_only(paths)?;
    let live = super::live_quota::read_codex_live(&global, &profile.profile_id)?;
    let rollout_time = rollout_at.as_deref().and_then(parse_time);
    let Some(live) = live else {
        return Ok(Reading { windows: rollout_windows, extras, usage_not_allowed_at: None });
    };
    let live_time = parse_time(&live.observed_at);
    let live_is_newer = rollout_at.is_none() || live_time.is_some_and(|l| rollout_time.is_none_or(|r| l > r));
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let not_allowed = live_is_newer
        && live.ordinary_usage_allowed == Some(false)
        && live_time.is_some_and(|t| now.saturating_sub(t.unix_timestamp()) <= FRESH_SECS);
    Ok(Reading {
        windows: if live_is_newer { live.windows } else { rollout_windows },
        extras,
        usage_not_allowed_at: not_allowed.then(|| live.observed_at.clone()),
    })
}

/// A claude-code profile's windows (E7): the run-event readings merged with
/// the newest valid live (`/usage`) reading, name by name. A run-event window
/// is kept only when it was observed strictly later than the live one; a
/// time that does not parse counts for the live reading.
fn claude_full_windows(paths: &Paths, profile: &Profile, repos: &[String]) -> Result<Vec<Window>> {
    let events = claude_windows(paths, &profile.profile_id, repos)?;
    let global = global_read_only(paths)?;
    let Some((_, live)) = super::live_quota::read_claude_live(&global, &profile.profile_id)? else {
        return Ok(events);
    };
    let mut merged: Vec<Window> = live;
    for event in events {
        match merged.iter_mut().find(|w| w.name == event.name) {
            Some(current) => {
                let event_is_newer = matches!((parse_time(&event.observed_at), parse_time(&current.observed_at)),
                    (Some(e), Some(l)) if e > l);
                if event_is_newer {
                    *current = event;
                }
            }
            None => merged.push(event),
        }
    }
    merged.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(merged)
}

fn reading(paths: &Paths, profile: &Profile, repos: &[String]) -> Result<Reading> {
    let plain = |windows| Reading { windows, extras: Vec::new(), usage_not_allowed_at: None };
    match profile.harness.as_str() {
        "claude-code" => Ok(plain(claude_full_windows(paths, profile, repos)?)),
        "codex" => codex_full_reading(paths, profile),
        "antigravity" => {
            let global = global_read_only(paths)?;
            Ok(plain(super::live_quota::read_agy_windows(&global, &profile.profile_id)?))
        }
        _ => Ok(plain(Vec::new())),
    }
}

/// When the profile's current reading was observed (QR-1): the latest of its
/// windows' `observed_at` and its `usage_not_allowed_at`, so codex's rollout
/// and live readings both count. None when there is no reading or it cannot
/// be read; times that do not parse are skipped.
pub(crate) fn newest_observation(paths: &Paths, profile: &Profile, repos: &[String]) -> Option<OffsetDateTime> {
    let reading = reading(paths, profile, repos).ok()?;
    reading
        .windows
        .iter()
        .map(|w| w.observed_at.as_str())
        .chain(reading.usage_not_allowed_at.as_deref())
        .filter_map(parse_time)
        .max()
}

/// The windows plus the extra buckets, for `profile usage` and `report`.
/// A claude-code profile has no extra buckets.
pub(crate) fn windows_and_extras(
    paths: &Paths,
    profile: &Profile,
    repos: &[String],
) -> Result<(Vec<Window>, Vec<ExtraBucket>)> {
    let reading = reading(paths, profile, repos)?;
    Ok((reading.windows, reading.extras))
}

/// The quota state of one profile's decision windows, with codex's "ordinary
/// usage not allowed" making it exhausted. `profile usage` and the ranking
/// both call this, so they agree.
fn profile_quota_state(
    decision: &[Window],
    usage_not_allowed_at: Option<&str>,
    now: i64,
) -> (&'static str, Option<f64>, Option<String>) {
    let (state, peak, newest) = quota_state(decision, now);
    if usage_not_allowed_at.is_some() {
        return ("exhausted", None, newest);
    }
    (state, peak, newest)
}

pub(crate) fn live_runs(paths: &Paths, profile: &str, repos: &[String]) -> Result<i64> {
    let mut total = 0;
    for repo in repos {
        let path = paths.state_db(repo);
        if !path.is_file() {
            continue;
        }
        let conn = read_only(&path)?;
        total += conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE account=?1 AND status IN ('queued','running')",
            [profile],
            |r| r.get::<_, i64>(0),
        )?;
    }
    Ok(total)
}

pub fn profile_usage(paths: &Paths, only: Option<&str>, refresh: bool, json_output: bool) -> Result<i32> {
    let global = global_read_only(paths)?;
    let all = crate::db::global::list_profiles(&global)?;
    let profiles = if let Some(id) = only {
        let p = all
            .into_iter()
            .find(|p| p.profile_id == id)
            .ok_or_else(|| anyhow!("no such profile: {id}"))?;
        vec![p]
    } else {
        all
    };
    let refresh_results = if refresh { Some(super::live_quota::refresh(paths, &profiles)?) } else { None };
    let repos = registered_repos(&global)?;
    // TIER-1: read after the refresh, so a tier it just stored shows up.
    let plans = crate::db::global::profile_plans(&global).unwrap_or_default();
    let mut values = Vec::new();
    for p in &profiles {
        let (plan, plan_label) = plan_fields(&plans, p);
        let Reading { windows: ws, extras, usage_not_allowed_at } = reading(paths, p, &repos)?;
        let now = OffsetDateTime::now_utc().unix_timestamp();
        let (decision, quota_group, _) = decision_windows(&ws, None, now);
        let (state, peak, newest) = profile_quota_state(&decision, usage_not_allowed_at.as_deref(), now);
        // FC-1: read only for a parked profile, as `ranked_candidates` does.
        let parked = if p.status == "rate_limited" { parked_group(&global, &p.profile_id)? } else { None };
        values.push(json!({
            "profile_id": p.profile_id,
            "provider": p.provider,
            "harness": p.harness,
            "status": p.status,
            "live_runs": live_runs(paths, &p.profile_id, &repos)?,
            "quota_state": state,
            "peak_percent": peak,
            "quota_group": quota_group,
            "parked_group": parked,
            "newest_observed_at": newest,
            "windows": ws.iter().map(Window::json).collect::<Vec<_>>(),
            "extra_buckets": extras.iter().map(ExtraBucket::json).collect::<Vec<_>>(),
            "plan": plan,
            "plan_label": plan_label,
        }));
    }
    let window_line = |prefix: &str, w: &Value| {
        let resets_at = w["resets_at"].as_str().unwrap_or("null");
        format!(
            "  {prefix}{}: {}% resets_at={} observed_at={} source={} reset_since_observed={}{}\n",
            w["name"].as_str().unwrap_or(""),
            w["used_percent"],
            resets_at,
            w["observed_at"].as_str().unwrap_or(""),
            w["source"].as_str().unwrap_or(""),
            w["reset_since_observed"],
            w["group"].as_str().map(|g| format!(" group={g}")).unwrap_or_default()
        )
    };
    let human = values
        .iter()
        .map(|v| {
            let text = |key: &str| v[key].as_str().unwrap_or("");
            let mut s = format!(
                "{}  {} / {}  status={}  live_runs={}\n",
                plan_label::named(text("profile_id"), v["plan_label"].as_str(), Lang::En),
                text("provider"),
                text("harness"),
                text("status"),
                v["live_runs"]
            );
            if let Some(windows) = v["windows"].as_array() {
                for w in windows {
                    s.push_str(&window_line("", w));
                }
            }
            if let Some(buckets) = v["extra_buckets"].as_array() {
                for b in buckets {
                    let prefix = format!("{} ", b["limit_id"].as_str().unwrap_or(""));
                    for w in b["windows"].as_array().into_iter().flatten() {
                        s.push_str(&window_line(&prefix, w));
                    }
                }
            }
            s.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut out = json!({"profiles":values});
    if let Some(results) = &refresh_results {
        out["refresh"] = json!(results.iter().map(|r| json!({"profile_id":r.profile_id,"status":r.status,"error":r.error})).collect::<Vec<_>>());
    }
    let mut human = human;
    if let Some(results) = &refresh_results {
        for result in results {
            human.push_str(&format!("\nrefresh {}: {}{}", result.profile_id, result.status,
                result.error.as_ref().map(|e| format!(": {e}")).unwrap_or_default()));
        }
    }
    output::emit(
        json_output,
        &out,
        &if human.is_empty() {
            "no profiles registered".into()
        } else {
            human
        },
    );
    Ok(0)
}

/// The full id of `task` (a prefix), looked up in every registered repo.
fn resolve_task(paths: &Paths, task: &str, repos: &[String]) -> Result<String> {
    let mut ids = Vec::new();
    for repo in repos {
        let path = paths.state_db(repo);
        if !path.is_file() {
            continue;
        }
        let conn = read_only(&path)?;
        let mut stmt = conn.prepare("SELECT task_id FROM tasks")?;
        ids.extend(
            stmt.query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    ids.sort();
    ids.dedup();
    ids::resolve_prefix(&ids, task).map_err(|e| anyhow!("{e}"))
}

fn task_provider(paths: &Paths, task: &str, repos: &[String]) -> Result<String> {
    let id = resolve_task(paths, task, repos)?;
    let mut latest: Option<(String, String)> = None;
    for repo in repos {
        let path = paths.state_db(repo);
        if !path.is_file() {
            continue;
        }
        let conn = read_only(&path)?;
        let row = conn.query_row("SELECT provider, COALESCE(started_at,'') FROM runs WHERE task_id=?1 AND role='implementer' ORDER BY started_at DESC LIMIT 1", [id.as_str()], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
        if latest
            .as_ref()
            .is_none_or(|old| row.as_ref().is_some_and(|r: &(String, String)| r.1 > old.1))
        {
            if let Some(r) = row {
                latest = Some(r);
            }
        }
    }
    Ok(latest.map(|r| r.0).unwrap_or_default())
}

pub(crate) fn ranked_candidates(
    paths: &Paths,
    role: &str,
    task: Option<&str>,
    model: Option<&str>,
) -> Result<Vec<RankedCandidate>> {
    if role != "implementer" && role != "reviewer" {
        bail!("--role must be implementer or reviewer");
    }
    let global = global_read_only(paths)?;
    let profiles = crate::db::global::list_profiles(&global)?;
    let repos = registered_repos(&global)?;
    let task_provider = if role == "reviewer" {
        task.map(|t| task_provider(paths, t, &repos)).transpose()?
    } else {
        None
    };
    let now = OffsetDateTime::now_utc().unix_timestamp();
    let mut candidates = Vec::new();
    for p in profiles {
        let Reading { windows: ws, usage_not_allowed_at, .. } = reading(paths, &p, &repos)?;
        let live = live_runs(paths, &p.profile_id, &repos)?;
        let (decision, quota_group, group_reason) = decision_windows(&ws, model, now);
        let (state, peak, newest) = profile_quota_state(&decision, usage_not_allowed_at.as_deref(), now);
        let mut reasons = Vec::new();
        let parked_at = if p.status == "rate_limited" {
            global.query_row("SELECT ts FROM quota_events WHERE profile_id=?1 AND kind='rate_limited' ORDER BY seq DESC LIMIT 1", [&p.profile_id], |r| r.get::<_, String>(0)).optional()?
                .and_then(|ts| OffsetDateTime::parse(&ts, &Rfc3339).ok()).map(|t| t.unix_timestamp())
        } else {
            None
        };
        let parked_group = if p.status == "rate_limited" {
            parked_group(&global, &p.profile_id)?
        } else {
            None
        };
        let parked_elsewhere = p.status == "rate_limited"
            && is_parked_elsewhere(parked_group.as_deref(), quota_group.as_deref());
        let reset = parked_at.filter(|_| !parked_elsewhere).and_then(|parked| {
            ws.iter()
                .filter(|w| parked_group.is_none() || w.group == parked_group)
                .filter_map(|w| w.resets_at)
                .filter(|reset| *reset > parked && *reset <= now)
                .max()
        });
        let recovered = p.status == "rate_limited" && state != "exhausted" && reset.is_some();
        let mut eligible =
            (p.status == "active" || recovered || parked_elsewhere) && state != "exhausted";
        if parked_elsewhere {
            reasons.push(format!(
                "parked for quota group {}; the selected group {} is unaffected",
                parked_group.as_deref().unwrap_or(""),
                quota_group.as_deref().unwrap_or("")
            ));
        } else if p.status != "active" && !recovered {
            reasons.push(match p.status.as_str() {
                "rate_limited" => format!(
                    "status rate_limited; run `thoth profile check {}`",
                    p.profile_id
                ),
                "logged_out" => format!(
                    "status logged_out; run `thoth profile login {}`",
                    p.profile_id
                ),
                other => format!("status {other}"),
            });
        }
        if let Some(reset) = reset.filter(|_| recovered) {
            reasons.push(format!(
                "window reset at {}; treating parked profile as recovered",
                rfc_from_epoch(reset).unwrap_or_else(|| "unknown".into())
            ));
        }
        if ws.is_empty() && usage_not_allowed_at.is_none() {
            reasons.push(if p.harness == "antigravity" || p.harness == "codex" || p.harness == "claude-code" { "no quota reading yet; run `thoth profile usage --refresh`".into() } else { "no quota reading yet".into() });
        }
        if let Some(at) = &usage_not_allowed_at {
            reasons.push(format!("ordinary usage not allowed (codex app-server, read at {})", normalize_rfc(at)));
        }
        if let Some(reason) = group_reason { reasons.push(reason) }
        for w in &ws {
            let used = w.used.clamp(0.0, 100.0);
            let reset_at = w.resets_at.and_then(rfc_from_epoch);
            let label = w.group.as_ref().map(|g| format!("{} ({g})", w.name)).unwrap_or_else(|| w.name.clone());
            if w.is_reset() {
                reasons.push(format!(
                    "{}: was {}% at {}, reset at {} (counts as 0%)",
                    label,
                    used,
                    normalize_rfc(&w.observed_at),
                    reset_at.as_deref().unwrap_or("unknown")
                ));
            } else {
                reasons.push(format!(
                    "{}: {}% used, resets {}",
                    label,
                    used,
                    reset_at.as_deref().unwrap_or("unknown")
                ));
            }
            if decision.iter().any(|selected| selected.name == w.name && selected.group == w.group)
                && w.used >= EXHAUSTED_PERCENT && !w.is_reset() {
                eligible = false;
                reasons.push(format!("{} is at least 95% and has not reset", w.name));
            }
        }
        reasons.push(format!("{} live run(s)", live));
        let different = task_provider
            .as_ref()
            .is_some_and(|provider| !provider.is_empty() && provider != &p.provider);
        if different {
            reasons.push(format!(
                "different provider from the implementer ({} vs {})",
                p.provider,
                task_provider.as_deref().unwrap_or("unknown")
            ));
        }
        candidates.push(RankedCandidate {
            profile: p,
            live_runs: live,
            eligible,
            different_provider: different,
            quota_state: state,
            peak_percent: peak,
            newest_observed_at: newest,
            recovered,
            quota_group,
            parked_group,
            parked_elsewhere,
            reasons,
            windows: ws,
        });
    }
    candidates.sort_by(|a, b| {
        if a.eligible != b.eligible {
            return b.eligible.cmp(&a.eligible);
        }
        if !a.eligible {
            return a.profile.profile_id.cmp(&b.profile.profile_id);
        }
        if role == "reviewer" && task.is_some() && a.different_provider != b.different_provider {
            return b.different_provider.cmp(&a.different_provider);
        }
        (a.live_runs > 0)
            .cmp(&(b.live_runs > 0))
            .then_with(|| (a.quota_state != "ok").cmp(&(b.quota_state != "ok")))
            .then_with(|| {
                a.peak_percent
                    .unwrap_or(0.0)
                    .partial_cmp(&b.peak_percent.unwrap_or(0.0))
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| a.profile.profile_id.cmp(&b.profile.profile_id))
    });
    Ok(candidates)
}

/// The models a profile can run, for the commander to look up in the tables
/// (RJ-2): what `thoth models` lists (`models::discover`): the fixed aliases
/// for claude-code, the `models_cache.json` slugs for codex and, for
/// antigravity, the list its last login check read (MODELS-1; none stored
/// is `[]`).
fn available_models(profile: &Profile, stored: Option<&crate::db::global::StoredModels>) -> Vec<Value> {
    super::models::discover(profile, stored)
        .models
        .into_iter()
        .map(|(model, display_name)| json!({"model": model, "display_name": display_name}))
        .collect()
}

pub fn recommend(paths: &Paths, role: &str, task: Option<&str>, model: Option<&str>, json_output: bool) -> Result<i32> {
    if role != "implementer" && role != "reviewer" {
        bail!("--role must be implementer or reviewer");
    }
    // RJ-1: only the accounts' facts; which profile, model and effort to use
    // is the commander's choice. The task is resolved here for both roles.
    let repos = registered_repos(&global_read_only(paths)?)?;
    let task_id = match task {
        Some(t) => Some(resolve_task(paths, t, &repos)?),
        None => None,
    };
    // QR-1: due readings are refreshed before ranking; failures only warn.
    let refresh = super::live_quota::refresh_for_ranking(paths);
    for warning in &refresh.warnings {
        masked_eprintln!("warning: {warning}");
    }
    // RJ-2: the day's first call refreshes the external tables when the
    // switch is on (`main` wraps this command in `with_auto_refresh`); a
    // failure warns once and the old caches are sent.
    let score_refresh = super::models::auto_refresh_if_permitted(paths);
    let candidates = ranked_candidates(paths, role, task_id.as_deref(), model)?;
    // TIER-1: the stored tiers, read-only; no tier when they cannot be read.
    // MODELS-1: the stored agy model lists, from the same read-only open (a
    // global.db older than the column reads as none).
    let (plans, stored_lists) = match global_read_only(paths) {
        Ok(g) => (
            crate::db::global::profile_plans(&g).unwrap_or_default(),
            crate::db::global::profile_models(&g).unwrap_or_default(),
        ),
        Err(_) => (BTreeMap::new(), BTreeMap::new()),
    };
    let fields: BTreeMap<&str, (Option<String>, Option<String>)> = candidates
        .iter()
        .map(|c| (c.profile.profile_id.as_str(), plan_fields(&plans, &c.profile)))
        .collect();
    let field = |id: &str| fields.get(id).cloned().unwrap_or_default();
    let profiles: Vec<Profile> = candidates.iter().map(|c| c.profile.clone()).collect();
    let tables = super::tables::build(paths, &profiles, &stored_lists);
    let usage = super::own_usage::build(paths, role, &candidates, &plans)?;
    let task_history = match task_id.as_deref() {
        Some(id) => super::task_history::build(paths, &repos, id)?,
        None => None,
    };
    let out = json!({
        "role": role,
        "task_id": task_id,
        "model": model,
        "candidates": candidates.iter().map(|c| json!({
            "profile_id": c.profile.profile_id, "provider": c.profile.provider, "harness": c.profile.harness,
            "eligible": c.eligible, "reasons": c.reasons, "quota_state": c.quota_state, "peak_percent": c.peak_percent,
            "quota_group": c.quota_group, "parked_group": c.parked_group, "newest_observed_at": c.newest_observed_at,
            "recovered": c.recovered, "plan": field(&c.profile.profile_id).0, "plan_label": field(&c.profile.profile_id).1,
            "available_models": available_models(&c.profile, stored_lists.get(&c.profile.profile_id)),
        })).collect::<Vec<_>>(),
        "live_quota_refresh": refresh.json(),
        "score_refresh": score_refresh.json(),
        "tables": tables,
        "usage": usage.iter().map(super::own_usage::Row::json).collect::<Vec<_>>(),
        "task_history": task_history.as_ref().map(super::task_history::TaskHistory::json),
    });
    let named = |id: &str| plan_label::named(id, field(id).1.as_deref(), Lang::En);
    let mut lines: Vec<String> = Vec::new();
    if let Some(history) = &task_history {
        lines.extend(history.human(role));
    }
    for c in &candidates {
        lines.push(format!(
            "{}  {} / {}  {} {}\n  {}",
            named(&c.profile.profile_id),
            c.profile.provider,
            c.profile.harness,
            if c.eligible { "eligible" } else { "ineligible" },
            c.quota_state,
            c.reasons.join("; ")
        ));
    }
    lines.extend(super::tables::human_lines(&tables));
    lines.extend(usage.iter().map(super::own_usage::Row::human));
    lines.push("the filtered table rows are in --json (tables); nothing above picks for you".to_string());
    output::emit(json_output, &out, &lines.join("\n"));
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parked_elsewhere_needs_both_groups_known_and_different() {
        assert!(is_parked_elsewhere(Some("Gemini Models"), Some("Claude and GPT models")));
        assert!(!is_parked_elsewhere(Some("Gemini Models"), Some("Gemini Models")));
        assert!(!is_parked_elsewhere(None, Some("Claude and GPT models")));
        assert!(!is_parked_elsewhere(Some("Gemini Models"), None));
    }
}
