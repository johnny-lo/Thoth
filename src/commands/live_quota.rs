//! Live quota obtained by invoking an official CLI: agy's `/usage`, the
//! codex app-server's `account/rateLimits/read`, and Claude Code's own
//! `/usage` command (only on a manual `--refresh`). Thoth never reads login
//! files or talks to provider endpoints. The agy signed-out marker is based
//! on `agy models`; real `/usage` signed-out output has not yet been verified.

use crate::{commands::usage::Window, config::Paths, db::global::Profile};
use anyhow::Result;
use rusqlite::Connection;
use serde_json::Value;
use std::{env, io::{BufRead, BufReader, Read, Write}, process::{Child, Command, Stdio}, sync::mpsc, thread, time::{Duration, Instant}};
use time::{format_description::well_known::Rfc3339, OffsetDateTime, UtcOffset};

#[derive(Debug)]
pub(crate) struct RefreshResult {
    pub(crate) profile_id: String,
    pub(crate) status: &'static str,
    pub(crate) error: Option<String>,
}

pub(crate) fn refresh(paths: &Paths, profiles: &[Profile]) -> Result<Vec<RefreshResult>> {
    if env::var_os("THOTH_RUN_ID").is_some() {
        anyhow::bail!("THOTH_RUN_ID is set; quota refresh can only be executed by the user directly");
    }
    let enabled = crate::commands::settings::read_flags(&paths.home);
    if !enabled {
        anyhow::bail!("live quota is off; run `thoth config set live-quota on` yourself (this command can only be executed by the user directly)");
    }
    let conn = crate::db::open_global(paths)?;
    let repos = crate::commands::usage::registered_repos(&conn)?;
    let mut results = profiles.iter().map(|profile| refresh_profile(paths, &conn, profile, &repos, timeout_ms()))
        .collect::<Vec<_>>();
    results.sort_by(|a, b| a.profile_id.cmp(&b.profile_id));
    Ok(results)
}

/// Refreshes one profile's live reading, waiting at most `limit_ms` for the
/// answer. The manual `--refresh` and the automatic refresh both go through
/// here; a harness gains a manual live reading by adding a branch here, and
/// one that should also be refreshed automatically is added to
/// `auto_refresh_after` as well.
fn refresh_profile(paths: &Paths, conn: &Connection, profile: &Profile, repos: &[String], limit_ms: u64) -> RefreshResult {
    match profile.harness.as_str() {
        "antigravity" => refresh_agy(paths, conn, profile, limit_ms),
        "codex" => refresh_codex(paths, conn, profile, repos, limit_ms),
        "claude-code" => refresh_claude(paths, conn, profile, repos, limit_ms),
        _ => RefreshResult { profile_id: profile.profile_id.clone(), status: "unsupported", error: None },
    }
}

/// How old the newest reading may get before the automatic refresh asks again.
const AUTO_REFRESH_AFTER_SECS: i64 = 600;

/// The longest the automatic refresh waits for one profile's answer.
const AUTO_REFRESH_LIMIT_MS: u64 = 10_000;

/// How old a reading of this harness may get before the automatic refresh
/// (QR-1) asks again; None when it is never refreshed automatically.
pub(crate) fn auto_refresh_after(harness: &str) -> Option<Duration> {
    match harness {
        "codex" | "antigravity" => Some(Duration::from_secs(AUTO_REFRESH_AFTER_SECS as u64)),
        // claude-code lands here on purpose: its usage endpoint answers 429
        // when asked too often (the limit is unmeasured), each query starts a
        // whole claude (about 4 s, with the account's hooks and plugins), and
        // a running claude already records readings from rate_limit_event.
        _ => None,
    }
}

/// True when a reading observed at `newest` (unix seconds; None for no
/// reading) is due at `now`: none at all, or strictly older than `after`. A
/// reading from the future (clock skew) is not due.
pub(crate) fn is_due(newest: Option<i64>, now: i64, after: Duration) -> bool {
    match newest {
        None => true,
        Some(at) => now - at > after.as_secs() as i64,
    }
}

/// The wait for one profile during the automatic refresh: the effective
/// live-quota-timeout-ms, capped at 10 seconds.
pub(crate) fn auto_limit_ms(effective_ms: u64) -> u64 {
    effective_ms.min(AUTO_REFRESH_LIMIT_MS)
}

/// The warning for a failed automatic refresh of one profile.
pub(crate) fn refresh_warning(profile_id: &str, error: &str, had_reading: bool) -> String {
    let fallback = if had_reading { "ranking with the previous reading" } else { "ranking without a quota reading" };
    format!("live quota refresh for {profile_id} failed: {error}; {fallback}")
}

/// What the automatic refresh before ranking did (`live_quota_refresh`).
#[derive(Debug)]
pub(crate) struct AutoRefresh {
    pub(crate) enabled: bool,
    pub(crate) skipped: Option<&'static str>,
    pub(crate) results: Vec<RefreshResult>,
    /// One per failed result, in the same order.
    pub(crate) warnings: Vec<String>,
}

impl AutoRefresh {
    pub(crate) fn json(&self) -> Value {
        serde_json::json!({"enabled": self.enabled, "skipped": self.skipped, "results": self.results.iter()
            .map(|r| serde_json::json!({"profile_id": r.profile_id, "status": r.status, "error": r.error}))
            .collect::<Vec<_>>()})
    }
}

/// QR-1: before `recommend` ranks profiles, refreshes every profile whose
/// newest reading is due, one after another.
/// Never fails: a refresh that fails leaves the previous reading in place and
/// adds a warning. Does nothing when the switch is off or inside a worker,
/// and opens no write connection when nothing is due.
pub(crate) fn refresh_for_ranking(paths: &Paths) -> AutoRefresh {
    let enabled = crate::commands::settings::read_flags(&paths.home);
    if !enabled {
        return AutoRefresh { enabled: false, skipped: Some("off"), results: Vec::new(), warnings: Vec::new() };
    }
    if env::var_os("THOTH_RUN_ID").is_some() {
        return AutoRefresh { enabled: true, skipped: Some("worker"), results: Vec::new(), warnings: Vec::new() };
    }
    let mut out = AutoRefresh { enabled: true, skipped: None, results: Vec::new(), warnings: Vec::new() };
    // A listing that fails refreshes nothing: an empty repo list would count
    // codex's live runs as zero and query a profile that is in use.
    let Ok((profiles, repos)) = crate::commands::usage::global_read_only(paths).and_then(|global| {
        Ok((crate::db::global::list_profiles(&global)?, crate::commands::usage::registered_repos(&global)?))
    }) else { return out };
    let now = OffsetDateTime::now_utc().unix_timestamp();
    // (profile, whether it had a reading before the refresh)
    let mut due: Vec<(Profile, bool)> = Vec::new();
    for profile in profiles {
        let Some(after) = auto_refresh_after(&profile.harness) else { continue };
        if profile.status == "logged_out" { continue }
        let newest = crate::commands::usage::newest_observation(paths, &profile, &repos);
        if is_due(newest.map(|t| t.unix_timestamp()), now, after) { due.push((profile, newest.is_some())) }
    }
    due.sort_by(|a, b| a.0.profile_id.cmp(&b.0.profile_id));
    if due.is_empty() { return out }
    let results: Vec<(RefreshResult, bool)> = match crate::db::open_global(paths) {
        Ok(conn) => {
            let limit = auto_limit_ms(timeout_ms());
            due.iter().map(|(p, had)| (refresh_profile(paths, &conn, p, &repos, limit), *had)).collect()
        }
        Err(error) => due.iter().map(|(p, had)| (RefreshResult { profile_id: p.profile_id.clone(), status: "failed",
            error: Some(format!("could not open global.db: {error:#}")) }, *had)).collect(),
    };
    for (result, had) in results {
        if result.status == "failed" {
            out.warnings.push(refresh_warning(&result.profile_id, result.error.as_deref().unwrap_or(""), had));
        }
        out.results.push(result);
    }
    out
}

pub(crate) fn parse_agy_output(stdout: &str, observed_at: &str) -> std::result::Result<Vec<Window>, String> {
    let root: Value = serde_json::from_str(stdout).map_err(|_| "not JSON".to_string())?;
    if !root.is_object() { return Err("not JSON object".into()) }
    if root.get("status").and_then(Value::as_str) != Some("SUCCESS") {
        return Err(format!("reported status {}", root.get("status").and_then(Value::as_str).unwrap_or("unknown")));
    }
    if root.pointer("/command/name").and_then(Value::as_str) != Some("usage")
        || !root.pointer("/command/data/groups").is_some_and(Value::is_array) {
        return Err("did not answer /usage; agy 1.1.11 or newer is required".into());
    }
    let mut windows = Vec::new();
    for (index, group) in root.pointer("/command/data/groups").unwrap().as_array().unwrap().iter().enumerate() {
        let group_name = group.get("name").and_then(Value::as_str).map(str::to_owned)
            .unwrap_or_else(|| format!("group-{}", index + 1));
        let description = group.get("description").and_then(Value::as_str).unwrap_or("").to_owned();
        for bucket in group.get("buckets").and_then(Value::as_array).into_iter().flatten() {
            let Some(name) = bucket.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()) else { continue };
            let Some(remaining) = bucket.get("remaining_fraction").and_then(Value::as_f64) else { continue };
            let used = (((1.0 - remaining) * 100.0).clamp(0.0, 100.0) * 100.0).round() / 100.0;
            let resets_at = bucket.get("reset_time").and_then(Value::as_str)
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok()).map(|t| t.unix_timestamp());
            windows.push(Window { name: name.into(), used, resets_at, observed_at: observed_at.into(),
                source: "agy_usage", group: Some(group_name.clone()), group_description: Some(description.clone()) });
        }
    }
    if windows.is_empty() { Err("no quota buckets".into()) } else { Ok(windows) }
}

fn refresh_agy(paths: &Paths, conn: &Connection, profile: &Profile, limit_ms: u64) -> RefreshResult {
    let failed = |error| RefreshResult { profile_id: profile.profile_id.clone(), status: "failed", error: Some(error) };
    let args = vec!["-p".into(), "/usage".into(), "--output-format".into(), "json".into()];
    let bin = crate::adapter::antigravity::agy_bin();
    let (program, args) = match crate::process::resolve_harness_command(&bin, &args) {
        Ok(value) => value, Err(error) => return failed(format!("could not run agy from THOTH_AGY_BIN: {error}")),
    };
    let mut command = Command::new(program);
    command.args(args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).current_dir(&paths.home);
    crate::process::quiet(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child, Err(error) => return failed(format!("could not run agy from THOTH_AGY_BIN: {error}")),
    };
    let deadline = Instant::now() + Duration::from_millis(limit_ms);
    // Both pipes are read on their own threads while agy runs: a pipe that is
    // not read fills up (a few KB on Windows) and agy would block on its next
    // write and never exit. The reads share the wait's deadline below.
    let stdout_bytes = drain(child.stdout.take());
    let stderr_bytes = drain(child.stderr.take());
    let status = loop { match child.try_wait() {
        Ok(Some(status)) => break status,
        Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
        Ok(None) => {
            let _ = crate::process::controller().kill_tree(child.id());
            // Also terminate the root directly if platform tree cleanup was
            // degraded (notably a restricted Windows sandbox).
            let _ = child.kill(); let _ = child.wait(); return failed("timed out".into())
        }
        Err(error) => { let _ = crate::process::controller().kill_tree(child.id()); let _ = child.kill(); return failed(format!("could not run agy from THOTH_AGY_BIN: {error}")) }
    }};
    // agy has exited, but a background process it left behind can still hold
    // a pipe open; the reads are bound by the same deadline, so that case also
    // ends as `timed out` instead of hanging. The root is already reaped, so
    // its pid is not killed again here (it may belong to another process now).
    let (Some(stdout), Some(stderr)) = (collected(&stdout_bytes, deadline), collected(&stderr_bytes, deadline)) else {
        return failed("timed out".into())
    };
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    if stdout.contains("Please sign in") || stderr.contains("Please sign in") { return failed("not signed in".into()) }
    if !status.success() {
        return failed(format!("exited with code {}: {}", status.code().unwrap_or(-1), stderr.chars().take(200).collect::<String>()));
    }
    let observed_at = crate::ids::now_rfc3339();
    let windows = match parse_agy_output(&stdout, &observed_at) { Ok(value) => value, Err(error) => return failed(error) };
    let detail = serde_json::json!({"source":"agy_usage", "windows":windows.iter().map(|w| serde_json::json!({
        "name":w.name, "used_percent":w.used,
        "resets_at":w.resets_at.and_then(|v| OffsetDateTime::from_unix_timestamp(v).ok()).and_then(|v| v.to_offset(UtcOffset::UTC).format(&Rfc3339).ok()),
        "group":w.group, "group_description":w.group_description
    })).collect::<Vec<_>>()});
    if let Err(error) = crate::db::global::insert_quota_event(conn, &observed_at, &profile.profile_id, "live_quota", &detail) {
        return failed(format!("failed to save quota event: {error}"));
    }
    RefreshResult { profile_id: profile.profile_id.clone(), status: "refreshed", error: None }
}

/// Reads one child pipe to its end on a separate thread; the bytes arrive on
/// the returned channel once the pipe closes.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Vec<u8>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut bytes = Vec::new();
        if let Some(mut pipe) = pipe { let _ = pipe.read_to_end(&mut bytes); }
        let _ = sender.send(bytes);
    });
    receiver
}

/// The pipe's bytes, or None when the deadline passes before the pipe closes.
fn collected(receiver: &mpsc::Receiver<Vec<u8>>, deadline: Instant) -> Option<Vec<u8>> {
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(bytes) => Some(bytes),
        Err(mpsc::RecvTimeoutError::Disconnected) => Some(Vec::new()),
        Err(mpsc::RecvTimeoutError::Timeout) => None,
    }
}

pub(crate) fn read_agy_windows(conn: &Connection, profile_id: &str) -> Result<Vec<Window>> {
    let mut statement = conn.prepare("SELECT ts, detail FROM quota_events WHERE profile_id=?1 AND kind='live_quota' ORDER BY seq DESC")?;
    let rows = statement.query_map([profile_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (observed_at, text) = row?;
        let Ok(detail) = serde_json::from_str::<Value>(&text) else { continue };
        if detail.get("source").and_then(Value::as_str) != Some("agy_usage") { continue }
        let Some(raw_windows) = detail.get("windows").and_then(Value::as_array) else { continue };
        let mut windows = Vec::new(); let mut valid = true;
        for raw in raw_windows {
            let (Some(name), Some(used), Some(group), Some(description)) = (raw.get("name").and_then(Value::as_str),
                raw.get("used_percent").and_then(Value::as_f64), raw.get("group").and_then(Value::as_str),
                raw.get("group_description").and_then(Value::as_str)) else { valid = false; break };
            let resets_at = match raw.get("resets_at") {
                None | Some(Value::Null) => None,
                Some(Value::String(v)) => match OffsetDateTime::parse(v, &Rfc3339) { Ok(t) => Some(t.unix_timestamp()), Err(_) => { valid=false; break } },
                _ => { valid=false; break }
            };
            windows.push(Window { name:name.into(), used, resets_at, observed_at:observed_at.clone(), source:"agy_usage",
                group:Some(group.into()), group_description:Some(description.into()) });
        }
        if valid && !windows.is_empty() { return Ok(windows) }
    }
    Ok(Vec::new())
}

/// codex's answer to `account/rateLimits/read`, parsed, or a reading stored
/// from one.
#[derive(Debug)]
pub(crate) struct CodexLive {
    pub(crate) windows: Vec<Window>,
    pub(crate) ordinary_usage_allowed: Option<bool>,
    pub(crate) observed_at: String,
}

const CODEX_SOURCE: &str = "codex_app_server";

/// The request id of `account/rateLimits/read`.
const CODEX_RATE_LIMITS_ID: i64 = 1;

/// The line, parsed, when it is the app-server's reply to the rate-limits
/// request: a JSON object whose id is 1 and that carries no `method` (a line
/// with a method is a request or notification from the server). Every other
/// line - not JSON, no id, another id - is None.
pub(crate) fn codex_reply_line(line: &str) -> Option<Value> {
    let value: Value = serde_json::from_str(line.trim()).ok()?;
    let object = value.as_object()?;
    (object.get("id").and_then(Value::as_i64) == Some(CODEX_RATE_LIMITS_ID) && !object.contains_key("method"))
        .then_some(value)
}

/// The general bucket's windows from the rate-limits reply: the `codex`
/// entry of `rateLimitsByLimitId` when there is one, else `rateLimits`.
pub(crate) fn parse_codex_reply(reply: &Value, observed_at: &str) -> std::result::Result<CodexLive, String> {
    if let Some(error) = reply.get("error").filter(|v| !v.is_null()) {
        let message = error.get("message").and_then(Value::as_str).unwrap_or("no message");
        let code = error.get("code").map(Value::to_string).unwrap_or_else(|| "unknown".into());
        return Err(format!("app-server answered with error {code}: {message}"));
    }
    let Some(result) = reply.get("result").filter(|v| v.is_object()) else {
        return Err("app-server answer has no result object".into());
    };
    let limits = match result.get("rateLimitsByLimitId").and_then(Value::as_object).and_then(|m| m.get("codex")) {
        Some(codex) => codex,
        None => result.get("rateLimits").unwrap_or(&Value::Null),
    };
    if !limits.is_object() { return Err("app-server answer has no rateLimits object".into()) }
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        let Some(limit) = limits.get(key).filter(|v| v.is_object()) else { continue };
        let Some(minutes) = limit.get("windowDurationMins").and_then(Value::as_i64) else { continue };
        let Some(used) = limit.get("usedPercent").and_then(Value::as_f64) else { continue };
        windows.push(Window { name: crate::commands::usage::codex_window_name(minutes), used,
            resets_at: limit.get("resetsAt").and_then(Value::as_i64), observed_at: observed_at.into(),
            source: CODEX_SOURCE, group: None, group_description: None });
    }
    let ordinary_usage_allowed = result.get("ordinaryUsageAllowed").and_then(Value::as_bool);
    if windows.is_empty() && ordinary_usage_allowed != Some(false) { return Err("no quota windows".into()) }
    Ok(CodexLive { windows, ordinary_usage_allowed, observed_at: observed_at.into() })
}

fn rfc_or_null(epoch: Option<i64>) -> Option<String> {
    epoch.and_then(|v| OffsetDateTime::from_unix_timestamp(v).ok()).and_then(|v| v.to_offset(UtcOffset::UTC).format(&Rfc3339).ok())
}

/// The subscription tier in the rate-limits reply (TIER-1): the first
/// `planType` that is a string, from the `codex` bucket then `rateLimits`,
/// normalised; None when it is missing or not a valid tier.
pub(crate) fn codex_plan(reply: &Value) -> Option<String> {
    let result = reply.get("result")?;
    let raw = [result.pointer("/rateLimitsByLimitId/codex/planType"), result.pointer("/rateLimits/planType")]
        .into_iter()
        .flatten()
        .find_map(Value::as_str)?;
    crate::plan_label::normalize(raw)
}

/// The `live_quota` detail stored for a codex reading: only these fields, so
/// nothing else from the reply (account id, plan, credits) is kept. The plan
/// is stored on the profile row instead (TIER-1).
fn codex_detail(reading: &CodexLive) -> Value {
    serde_json::json!({"source": CODEX_SOURCE, "windows": reading.windows.iter().map(|w| serde_json::json!({
        "name": w.name, "used_percent": w.used, "resets_at": rfc_or_null(w.resets_at),
    })).collect::<Vec<_>>(), "ordinary_usage_allowed": reading.ordinary_usage_allowed})
}

/// Asks codex's app-server for the account's rate limits. Skipped while the
/// profile has a queued or running run: the app-server opens its own files
/// in CODEX_HOME and could clash with the worker's.
fn refresh_codex(paths: &Paths, conn: &Connection, profile: &Profile, repos: &[String], limit_ms: u64) -> RefreshResult {
    let failed = |error| RefreshResult { profile_id: profile.profile_id.clone(), status: "failed", error: Some(error) };
    match crate::commands::usage::live_runs(paths, &profile.profile_id, repos) {
        Ok(0) => {}
        Ok(_) => return RefreshResult { profile_id: profile.profile_id.clone(), status: "skipped_live_run", error: None },
        Err(error) => return failed(format!("could not count live runs: {error}")),
    }
    let args = vec!["app-server".into(), "--listen".into(), "stdio://".into()];
    let bin = crate::config::codex_bin();
    let (program, args) = match crate::process::resolve_harness_command(&bin, &args) {
        Ok(value) => value, Err(error) => return failed(format!("could not run codex from THOTH_CODEX_BIN: {error}")),
    };
    let mut command = Command::new(program);
    command.args(args).env("CODEX_HOME", &profile.config_dir).stdin(Stdio::piped()).stdout(Stdio::piped())
        .stderr(Stdio::piped()).current_dir(&paths.home);
    crate::process::quiet(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child, Err(error) => return failed(format!("could not run codex from THOTH_CODEX_BIN: {error}")),
    };
    let deadline = Instant::now() + Duration::from_millis(limit_ms);
    // stderr is read on its own thread so a full pipe never blocks codex;
    // stdout is read line by line on another until the reply shows up.
    let _stderr_bytes = drain(child.stderr.take());
    let replies = reply_reader(child.stdout.take());
    let mut stdin = child.stdin.take();
    let requests = [
        serde_json::json!({"method": "initialize", "id": 0, "params": {"clientInfo": {"name": "thoth", "version": env!("CARGO_PKG_VERSION")}}}),
        serde_json::json!({"method": "initialized"}),
        serde_json::json!({"method": "account/rateLimits/read", "id": CODEX_RATE_LIMITS_ID, "params": {"excludeResetCreditDetails": true}}),
    ];
    let written = stdin.as_mut().map(|pipe| {
        requests.iter().try_for_each(|request| writeln!(pipe, "{request}")).and_then(|_| pipe.flush())
    });
    if let Some(Err(error)) = written {
        end_child(&mut child);
        return failed(format!("could not write to codex app-server: {error}"));
    }
    let reply = match replies.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(reply) => reply,
        Err(mpsc::RecvTimeoutError::Timeout) => { end_child(&mut child); return failed("timed out".into()) }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            drop(stdin);
            let code = wait_briefly(&mut child, Duration::from_secs(5));
            end_child(&mut child);
            return failed(match code {
                Some(code) => format!("codex app-server exited with code {code} before answering"),
                None => "codex app-server closed its output before answering".into(),
            })
        }
    };
    // Closing stdin asks the app-server to exit; it gets 5 seconds.
    drop(stdin);
    wait_briefly(&mut child, Duration::from_secs(5));
    end_child(&mut child);
    let observed_at = crate::ids::now_rfc3339();
    let reading = match parse_codex_reply(&reply, &observed_at) { Ok(value) => value, Err(error) => return failed(error) };
    if let Err(error) = crate::db::global::insert_quota_event(conn, &observed_at, &profile.profile_id, "live_quota", &codex_detail(&reading)) {
        return failed(format!("failed to save quota event: {error}"));
    }
    // TIER-1: the reading is saved, so a failed tier write only warns.
    if let Some(plan) = codex_plan(&reply) {
        crate::plan_label::remember_or_warn(conn, &profile.profile_id, Some(&plan));
    }
    RefreshResult { profile_id: profile.profile_id.clone(), status: "refreshed", error: None }
}

fn timeout_ms() -> u64 {
    crate::commands::settings::effective("live-quota-timeout-ms").parse().unwrap_or(30_000)
}

/// Reads stdout line by line on its own thread and sends the first line that
/// is the rate-limits reply; the channel disconnects when stdout closes first.
fn reply_reader<R: Read + Send + 'static>(pipe: Option<R>) -> mpsc::Receiver<Value> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let Some(pipe) = pipe else { return };
        let mut reader = BufReader::new(pipe);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => if let Some(reply) = codex_reply_line(&String::from_utf8_lossy(&line)) {
                    let _ = sender.send(reply);
                    return;
                },
            }
        }
    });
    receiver
}

/// Waits up to `limit` for the child to exit; its exit code when it did.
fn wait_briefly(child: &mut Child, limit: Duration) -> Option<i32> {
    let deadline = Instant::now() + limit;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.code().unwrap_or(-1)),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => return None,
        }
    }
}

/// Ends a child still running, with its process tree, then reaps it. A child
/// already reaped is left alone: its pid may belong to another process now.
fn end_child(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) { return }
    let _ = crate::process::controller().kill_tree(child.id());
    let _ = child.kill();
    let _ = child.wait();
}

/// The newest well-formed codex live reading of a profile; rows that are not
/// JSON or have the wrong shape are skipped.
pub(crate) fn read_codex_live(conn: &Connection, profile_id: &str) -> Result<Option<CodexLive>> {
    let mut statement = conn.prepare("SELECT ts, detail FROM quota_events WHERE profile_id=?1 AND kind='live_quota' ORDER BY seq DESC")?;
    let rows = statement.query_map([profile_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (observed_at, text) = row?;
        let Ok(detail) = serde_json::from_str::<Value>(&text) else { continue };
        if detail.get("source").and_then(Value::as_str) != Some(CODEX_SOURCE) { continue }
        let ordinary_usage_allowed = match detail.get("ordinary_usage_allowed") {
            None | Some(Value::Null) => None,
            Some(Value::Bool(value)) => Some(*value),
            _ => continue,
        };
        let Some(raw_windows) = detail.get("windows").and_then(Value::as_array) else { continue };
        let mut windows = Vec::new(); let mut valid = true;
        for raw in raw_windows {
            let (Some(name), Some(used)) = (raw.get("name").and_then(Value::as_str), raw.get("used_percent").and_then(Value::as_f64))
                else { valid = false; break };
            let resets_at = match raw.get("resets_at") {
                None | Some(Value::Null) => None,
                Some(Value::String(v)) => match OffsetDateTime::parse(v, &Rfc3339) { Ok(t) => Some(t.unix_timestamp()), Err(_) => { valid=false; break } },
                _ => { valid=false; break }
            };
            windows.push(Window { name:name.into(), used, resets_at, observed_at:observed_at.clone(), source:CODEX_SOURCE,
                group:None, group_description:None });
        }
        if valid && (!windows.is_empty() || ordinary_usage_allowed == Some(false)) {
            return Ok(Some(CodexLive { windows, ordinary_usage_allowed, observed_at }))
        }
    }
    Ok(None)
}

const CLAUDE_SOURCE: &str = "claude_usage";

/// Every line of claude's stream-json output that is a JSON object.
fn claude_objects(stdout: &str) -> Vec<Value> {
    stdout.lines().filter_map(|line| serde_json::from_str::<Value>(line.trim()).ok()).filter(Value::is_object).collect()
}

/// The last `result` line among the objects.
fn claude_result_line(objects: &[Value]) -> Option<&Value> {
    objects.iter().rev().find(|v| v.get("type").and_then(Value::as_str) == Some("result"))
}

/// The error a `result` line reports, when its `is_error` is true.
fn claude_reported_error(result: &Value) -> Option<String> {
    if result.get("is_error").and_then(Value::as_bool) != Some(true) { return None }
    let message = match result.get("result").and_then(Value::as_str) {
        Some(text) => text.chars().take(200).collect::<String>(),
        None => "no message".into(),
    };
    Some(format!("claude reported an error: {message}"))
}

/// A row's kind when it is 1-32 ASCII lowercase letters, digits or `_`.
fn claude_kind(row: &Value) -> Option<&str> {
    row.get("kind").and_then(Value::as_str).filter(|kind| (1..=32).contains(&kind.len())
        && kind.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'))
}

/// The window name of a row: the two known kinds map to the run-event
/// names; any other kind is told apart by its scope's display name. The
/// kind alone decides what a row is (claude's own schema says to classify
/// a row on it, never on a label).
fn claude_window_name(kind: &str, row: &Value) -> String {
    match kind {
        "session" => return "five_hour".into(),
        "weekly_all" => return "seven_day".into(),
        _ => {}
    }
    let label = row.pointer("/scope/model/display_name").and_then(Value::as_str)
        .or_else(|| row.pointer("/scope/surface/display_name").and_then(Value::as_str))
        .map(|text| text.chars().filter(|c| !c.is_control()).collect::<String>());
    match label.as_deref().map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{kind}:{}", text.chars().take(32).collect::<String>()),
        None => kind.into(),
    }
}

/// The quota windows in claude's stream-json answer to `/usage`: the
/// `rate_limits.limits` rows of the last `usage_report`. A row that cannot
/// be read is skipped so a new kind does not lose the others.
pub(crate) fn parse_claude_usage(stdout: &str, observed_at: &str) -> std::result::Result<Vec<Window>, String> {
    let objects = claude_objects(stdout);
    let Some(result) = claude_result_line(&objects) else { return Err("claude printed no result line".into()) };
    if let Some(error) = claude_reported_error(result) { return Err(error) }
    if result.get("local_command").and_then(Value::as_str) != Some("usage") {
        return Err("claude did not answer /usage locally; a model turn may have run".into());
    }
    let Some(report) = objects.iter().rev().filter(|v| v.get("type").and_then(Value::as_str) == Some("assistant"))
        .find_map(|v| v.get("usage_report").filter(|r| r.is_object())) else {
        return Err("claude's /usage answer has no usage_report; a newer Claude Code is needed (seen with 2.1.296)".into());
    };
    let Some(rate_limits) = report.get("rate_limits").filter(|v| v.is_object()) else {
        return Err("claude could not read the plan's usage (signed out, no subscription, or a login without the profile scope)".into());
    };
    let Some(rows) = rate_limits.get("limits").and_then(Value::as_array) else {
        return Err("claude reported no usage rows".into());
    };
    let mut windows: Vec<Window> = Vec::new();
    for row in rows {
        if !row.is_object() { continue }
        let Some(kind) = claude_kind(row) else { continue };
        let Some(used) = row.get("percent").and_then(Value::as_f64).filter(|p| (0.0..=100.0).contains(p)) else { continue };
        let name = claude_window_name(kind, row);
        if windows.iter().any(|w| w.name == name) { continue }
        let resets_at = row.get("resets_at").and_then(Value::as_str)
            .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok()).map(|t| t.unix_timestamp());
        windows.push(Window { name, used, resets_at, observed_at: observed_at.into(), source: CLAUDE_SOURCE,
            group: None, group_description: None });
    }
    if windows.is_empty() { Err("no quota windows".into()) } else { Ok(windows) }
}

/// The `live_quota` detail stored for a Claude reading: only these fields, so
/// nothing else from claude's answer (spend, extra usage, severity, session)
/// is kept.
fn claude_detail(windows: &[Window]) -> Value {
    serde_json::json!({"source": CLAUDE_SOURCE, "windows": windows.iter().map(|w| serde_json::json!({
        "name": w.name, "used_percent": w.used, "resets_at": rfc_or_null(w.resets_at),
    })).collect::<Vec<_>>()})
}

/// Asks the official claude CLI for the account's usage with its local
/// `/usage` command; claude signs in and talks to Anthropic itself, and
/// Thoth only reads the structured `usage_report` it prints.
fn refresh_claude(paths: &Paths, conn: &Connection, profile: &Profile, repos: &[String], limit_ms: u64) -> RefreshResult {
    let failed = |error| RefreshResult { profile_id: profile.profile_id.clone(), status: "failed", error: Some(error) };
    // Skipped while a run is queued or running: claude may renew its token in
    // the same config directory, and that run's rate_limit_event already
    // records readings.
    match crate::commands::usage::live_runs(paths, &profile.profile_id, repos) {
        Ok(0) => {}
        Ok(_) => return RefreshResult { profile_id: profile.profile_id.clone(), status: "skipped_live_run", error: None },
        Err(error) => return failed(format!("could not count live runs: {error}")),
    }
    let args: Vec<String> = vec![
        "-p".into(), "/usage".into(), "--output-format".into(), "stream-json".into(), "--verbose".into(),
        // Leaves no conversation record in the config directory.
        "--no-session-persistence".into(),
        // A safeguard: should a version send /usage to the model as a prompt,
        // the cheapest model answers, and the parse rejects that turn.
        "--model".into(), "haiku".into(),
        // Starts no MCP server.
        "--strict-mcp-config".into(),
    ];
    let bin = crate::config::claude_bin();
    let (program, args) = match crate::process::resolve_harness_command(&bin, &args) {
        Ok(value) => value, Err(error) => return failed(format!("could not run claude from THOTH_CLAUDE_BIN: {error}")),
    };
    let mut command = Command::new(program);
    command.args(args).env("CLAUDE_CONFIG_DIR", &profile.config_dir)
        // Any of these in Thoth's environment would make claude use that
        // credential instead of this profile's login; they are only removed,
        // never read.
        .env_remove("CLAUDE_CODE_OAUTH_TOKEN").env_remove("ANTHROPIC_API_KEY").env_remove("ANTHROPIC_AUTH_TOKEN")
        .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).current_dir(&paths.home);
    crate::process::quiet(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child, Err(error) => return failed(format!("could not run claude from THOTH_CLAUDE_BIN: {error}")),
    };
    let deadline = Instant::now() + Duration::from_millis(limit_ms);
    let stdout_bytes = drain(child.stdout.take());
    let stderr_bytes = drain(child.stderr.take());
    let status = loop { match child.try_wait() {
        Ok(Some(status)) => break status,
        Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
        Ok(None) => {
            let _ = crate::process::controller().kill_tree(child.id());
            let _ = child.kill(); let _ = child.wait(); return failed("timed out".into())
        }
        Err(error) => { let _ = crate::process::controller().kill_tree(child.id()); let _ = child.kill(); return failed(format!("could not run claude from THOTH_CLAUDE_BIN: {error}")) }
    }};
    let (Some(stdout), Some(stderr)) = (collected(&stdout_bytes, deadline), collected(&stderr_bytes, deadline)) else {
        return failed("timed out".into())
    };
    let stdout = String::from_utf8_lossy(&stdout);
    let stderr = String::from_utf8_lossy(&stderr);
    if !status.success() {
        // In stream-json mode claude prints its errors (Not logged in, ...) on
        // the result line in stdout and exits 1, often with stderr empty.
        if let Some(error) = claude_result_line(&claude_objects(&stdout)).and_then(claude_reported_error) {
            return failed(error);
        }
        return failed(format!("exited with code {}: {}", status.code().unwrap_or(-1), stderr.chars().take(200).collect::<String>()));
    }
    let observed_at = crate::ids::now_rfc3339();
    let windows = match parse_claude_usage(&stdout, &observed_at) { Ok(value) => value, Err(error) => return failed(error) };
    if let Err(error) = crate::db::global::insert_quota_event(conn, &observed_at, &profile.profile_id, "live_quota", &claude_detail(&windows)) {
        return failed(format!("failed to save quota event: {error}"));
    }
    RefreshResult { profile_id: profile.profile_id.clone(), status: "refreshed", error: None }
}

/// The newest well-formed Claude live reading of a profile with its `ts`;
/// a row with any window of the wrong shape is skipped as a whole.
pub(crate) fn read_claude_live(conn: &Connection, profile_id: &str) -> Result<Option<(String, Vec<Window>)>> {
    let mut statement = conn.prepare("SELECT ts, detail FROM quota_events WHERE profile_id=?1 AND kind='live_quota' ORDER BY seq DESC")?;
    let rows = statement.query_map([profile_id], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
    for row in rows {
        let (observed_at, text) = row?;
        let Ok(detail) = serde_json::from_str::<Value>(&text) else { continue };
        if !detail.is_object() || detail.get("source").and_then(Value::as_str) != Some(CLAUDE_SOURCE) { continue }
        let Some(raw_windows) = detail.get("windows").and_then(Value::as_array) else { continue };
        let mut windows = Vec::new(); let mut valid = true;
        for raw in raw_windows {
            let (Some(name), Some(used)) = (raw.get("name").and_then(Value::as_str), raw.get("used_percent").and_then(Value::as_f64))
                else { valid = false; break };
            let resets_at = match raw.get("resets_at") {
                None | Some(Value::Null) => None,
                Some(Value::String(v)) => match OffsetDateTime::parse(v, &Rfc3339) { Ok(t) => Some(t.unix_timestamp()), Err(_) => { valid=false; break } },
                _ => { valid=false; break }
            };
            windows.push(Window { name:name.into(), used, resets_at, observed_at:observed_at.clone(), source:CLAUDE_SOURCE,
                group:None, group_description:None });
        }
        if valid && !windows.is_empty() { return Ok(Some((observed_at, windows))) }
    }
    Ok(None)
}

pub(crate) fn matching_groups(windows: &[Window], model: &str) -> Vec<String> {
    let model_words = model.to_lowercase().split('-').filter(|s| !s.is_empty()).map(str::to_owned).collect::<Vec<_>>();
    let mut matches = Vec::new();
    for window in windows {
        let Some(group) = &window.group else { continue }; if matches.contains(group) { continue }
        let Some((_, families)) = window.group_description.as_deref().unwrap_or("").split_once(':') else { continue };
        if families.split(',').any(|family| family.to_lowercase().split([' ', '-']).filter(|s| !s.is_empty())
            .all(|word| model_words.iter().any(|candidate| candidate == word))) { matches.push(group.clone()) }
    }
    matches
}

#[cfg(test)]
mod tests {
    //! Named after the acceptance criterion and case each one covers
    //! (`ac3_c_...` is AC3 case (c)); `..._rule_...` covers a rule of that
    //! criterion that has no lettered case.
    use super::*;
    use serde_json::json;

    const OBSERVED: &str = "2026-10-01T00:00:00Z";
    const GEMINI_DESCRIPTION: &str = "Models within this group: Gemini Flash, Gemini Pro";
    const OTHER_DESCRIPTION: &str = "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS";

    fn parse(value: Value) -> std::result::Result<Vec<Window>, String> {
        parse_agy_output(&value.to_string(), OBSERVED)
    }

    /// A successful `/usage` answer with these groups.
    fn answer(groups: Value) -> Value {
        json!({"status": "SUCCESS", "num_turns": 0, "command": {"name": "usage", "data": {"groups": groups}}})
    }

    fn epoch(text: &str) -> i64 {
        OffsetDateTime::parse(text, &Rfc3339).unwrap().unix_timestamp()
    }

    fn used(windows: &[Window]) -> Vec<(&str, f64)> {
        windows.iter().map(|w| (w.name.as_str(), w.used)).collect()
    }

    // ---- AC3: parsing agy's answer ----

    #[test]
    fn ac3_a_two_groups_with_one_bucket_each() {
        let windows = parse(answer(json!([
            {"name": "Gemini Models", "description": GEMINI_DESCRIPTION, "buckets": [{"id": "gemini-weekly",
                "window": "weekly", "remaining_fraction": 0.9912887811660767, "reset_time": "2026-10-08T03:04:05Z"}]},
            {"name": "Claude and GPT models", "description": OTHER_DESCRIPTION, "buckets": [{"id": "3p-weekly",
                "window": "weekly", "remaining_fraction": 1, "reset_time": "2026-10-09T00:00:00Z"}]}
        ])))
        .unwrap();
        assert_eq!(used(&windows), [("gemini-weekly", 0.87), ("3p-weekly", 0.0)]);
        assert_eq!(windows[0].group.as_deref(), Some("Gemini Models"));
        assert_eq!(windows[1].group.as_deref(), Some("Claude and GPT models"));
        assert_eq!(windows[0].resets_at, Some(epoch("2026-10-08T03:04:05Z")));
        assert_eq!(windows[1].resets_at, Some(epoch("2026-10-09T00:00:00Z")));
        assert_eq!(windows[0].group_description.as_deref(), Some(GEMINI_DESCRIPTION));
        assert_eq!(windows[1].group_description.as_deref(), Some(OTHER_DESCRIPTION));
        assert!(windows.iter().all(|w| w.source == "agy_usage" && w.observed_at == OBSERVED));
    }

    #[test]
    fn ac3_b_one_group_with_a_5h_and_a_weekly_bucket() {
        let windows = parse(answer(json!([{"name": "X", "description": "", "buckets": [
            {"id": "x-5h", "window": "5h", "remaining_fraction": 0.25},
            {"id": "x-weekly", "window": "weekly", "remaining_fraction": 0.5}
        ]}])))
        .unwrap();
        assert_eq!(used(&windows), [("x-5h", 75.0), ("x-weekly", 50.0)]);
        assert_eq!(windows[0].group, windows[1].group);
        assert_eq!(windows[0].group.as_deref(), Some("X"));
    }

    #[test]
    fn ac3_c_bucket_without_remaining_fraction_is_skipped() {
        let windows = parse(answer(json!([{"name": "X", "buckets": [
            {"id": "no-fraction", "window": "weekly"},
            {"id": "ok", "window": "weekly", "remaining_fraction": 0.4}
        ]}])))
        .unwrap();
        assert_eq!(used(&windows), [("ok", 60.0)]);
    }

    #[test]
    fn ac3_d_remaining_fraction_is_clamped() {
        let windows = parse(answer(json!([{"name": "X", "buckets": [
            {"id": "above-one", "remaining_fraction": 1.7},
            {"id": "below-zero", "remaining_fraction": -0.2}
        ]}])))
        .unwrap();
        assert_eq!(used(&windows), [("above-one", 0.0), ("below-zero", 100.0)]);
    }

    #[test]
    fn ac3_e_unparsable_reset_time_keeps_the_window() {
        let windows = parse(answer(json!([{"name": "X", "buckets": [
            {"id": "x-weekly", "remaining_fraction": 0.5, "reset_time": "not a time"}
        ]}])))
        .unwrap();
        assert_eq!(used(&windows), [("x-weekly", 50.0)]);
        assert_eq!(windows[0].resets_at, None);
    }

    #[test]
    fn ac3_f_group_without_name_is_numbered_from_one() {
        let bucket = |id: &str| json!([{"id": id, "remaining_fraction": 1}]);
        let windows = parse(answer(json!([
            {"buckets": bucket("a")},
            {"name": "Named", "buckets": bucket("b")},
            {"name": 7, "buckets": bucket("c")}
        ])))
        .unwrap();
        let groups: Vec<_> = windows.iter().map(|w| w.group.as_deref().unwrap()).collect();
        assert_eq!(groups, ["group-1", "Named", "group-3"]);
    }

    #[test]
    fn ac3_g5_output_that_is_not_json_is_an_error() {
        assert!(parse_agy_output("not json", OBSERVED).unwrap_err().contains("not JSON"));
    }

    #[test]
    fn ac3_g6_status_other_than_success_is_an_error() {
        let error = parse(json!({"status": "ERROR"})).unwrap_err();
        assert!(error.contains("reported status") && error.contains("ERROR"), "{error}");
    }

    #[test]
    fn ac3_g7_answer_that_is_not_usage_is_an_error() {
        let error = parse(json!({"status": "SUCCESS", "num_turns": 1})).unwrap_err();
        assert!(error.contains("did not answer /usage") && error.contains("1.1.11"), "{error}");
        let not_array = json!({"status": "SUCCESS", "command": {"name": "usage", "data": {"groups": {}}}});
        assert!(parse(not_array).unwrap_err().contains("did not answer /usage"));
    }

    #[test]
    fn ac3_g8_no_usable_bucket_is_an_error() {
        assert!(parse(answer(json!([]))).unwrap_err().contains("no quota buckets"));
    }

    #[test]
    fn ac3_rule_json_that_is_not_an_object_is_not_json() {
        assert!(parse_agy_output("[1, 2]", OBSERVED).unwrap_err().contains("not JSON"));
    }

    #[test]
    fn ac3_rule_bucket_id_must_be_a_non_empty_string() {
        let windows = parse(answer(json!([{"name": "X", "buckets": [
            {"remaining_fraction": 0.5},
            {"id": "", "remaining_fraction": 0.5},
            {"id": 3, "remaining_fraction": 0.5},
            {"id": "ok", "remaining_fraction": 0.5}
        ]}])))
        .unwrap();
        assert_eq!(used(&windows), [("ok", 50.0)]);
    }

    #[test]
    fn ac3_rule_remaining_fraction_must_be_a_number() {
        let windows = parse(answer(json!([{"name": "X", "buckets": [
            {"id": "text", "remaining_fraction": "0.5"},
            {"id": "ok", "remaining_fraction": 0.5}
        ]}])))
        .unwrap();
        assert_eq!(used(&windows), [("ok", 50.0)]);
    }

    #[test]
    fn ac3_rule_missing_description_is_empty_and_missing_reset_time_is_null() {
        let windows = parse(answer(json!([{"name": "X", "buckets": [{"id": "x", "remaining_fraction": 0.5}]}]))).unwrap();
        assert_eq!(windows[0].group_description.as_deref(), Some(""));
        assert_eq!(windows[0].resets_at, None);
    }

    #[test]
    fn ac3_rule_reset_time_with_an_offset_is_the_same_instant() {
        let windows = parse(answer(json!([{"name": "X", "buckets": [
            {"id": "x", "remaining_fraction": 0.5, "reset_time": "2026-10-08T08:00:00+08:00"}
        ]}])))
        .unwrap();
        assert_eq!(windows[0].resets_at, Some(epoch("2026-10-08T00:00:00Z")));
    }

    #[test]
    fn ac3_rule_used_percent_is_rounded_to_two_decimals() {
        let windows = parse(answer(json!([{"name": "X", "buckets": [{"id": "x", "remaining_fraction": 0.123456}]}]))).unwrap();
        assert_eq!(windows[0].used, 87.65);
    }

    // ---- AC6: which group a model belongs to ----

    /// The real sample's two groups, one bucket each.
    fn two_groups() -> Vec<Window> {
        parse(answer(json!([
            {"name": "Gemini Models", "description": GEMINI_DESCRIPTION, "buckets": [{"id": "a", "remaining_fraction": 1}]},
            {"name": "Claude and GPT models", "description": OTHER_DESCRIPTION, "buckets": [{"id": "b", "remaining_fraction": 1}]}
        ])))
        .unwrap()
    }

    #[test]
    fn ac6_unit_gemini_flash_medium_is_in_the_first_group() {
        assert_eq!(matching_groups(&two_groups(), "gemini-3.8-flash-medium"), ["Gemini Models"]);
    }

    #[test]
    fn ac6_unit_gemini_pro_high_is_in_the_first_group() {
        assert_eq!(matching_groups(&two_groups(), "gemini-3.1-pro-high"), ["Gemini Models"]);
    }

    #[test]
    fn ac6_unit_claude_sonnet_is_in_the_second_group() {
        assert_eq!(matching_groups(&two_groups(), "claude-sonnet-4-6"), ["Claude and GPT models"]);
    }

    #[test]
    fn ac6_unit_claude_opus_thinking_is_in_the_second_group() {
        assert_eq!(matching_groups(&two_groups(), "claude-opus-4-6-thinking"), ["Claude and GPT models"]);
    }

    #[test]
    fn ac6_unit_gpt_oss_is_in_the_second_group() {
        assert_eq!(matching_groups(&two_groups(), "gpt-oss-120b-medium"), ["Claude and GPT models"]);
    }

    #[test]
    fn ac6_unit_unknown_model_matches_no_group() {
        assert!(matching_groups(&two_groups(), "no-such-model").is_empty());
    }

    #[test]
    fn ac6_unit_description_without_a_colon_matches_no_model() {
        let windows = parse(answer(json!([
            {"name": "Gemini Models", "description": "Gemini family", "buckets": [{"id": "a", "remaining_fraction": 1}]}
        ])))
        .unwrap();
        assert!(matching_groups(&windows, "gemini-3.8-flash-medium").is_empty());
    }

    #[test]
    fn ac6_unit_same_description_twice_matches_more_than_one_group() {
        let description = "Models within this group: Gemini Flash";
        let windows = parse(answer(json!([
            {"name": "First", "description": description, "buckets": [{"id": "a", "remaining_fraction": 1}]},
            {"name": "Second", "description": description, "buckets": [{"id": "b", "remaining_fraction": 1}]}
        ])))
        .unwrap();
        assert_eq!(matching_groups(&windows, "gemini-3.8-flash-low"), ["First", "Second"]);
    }

    #[test]
    fn ac6_rule_matching_ignores_case() {
        assert_eq!(matching_groups(&two_groups(), "Claude-Sonnet-4-6"), ["Claude and GPT models"]);
    }

    // ---- AC9 (2): pipe reads share the wait's deadline ----

    #[test]
    fn ac9_2_drain_returns_the_whole_pipe() {
        let receiver = drain(Some(std::io::Cursor::new(vec![b'x'; 300_000])));
        let bytes = collected(&receiver, Instant::now() + Duration::from_secs(10)).unwrap();
        assert_eq!(bytes.len(), 300_000);
    }

    #[test]
    fn ac9_2_pipe_still_open_at_the_deadline_gives_up() {
        // A sender that never sends stands in for a pipe a background process
        // keeps open after agy exits.
        let (_sender, receiver) = mpsc::channel::<Vec<u8>>();
        let started = Instant::now();
        assert!(collected(&receiver, started + Duration::from_millis(100)).is_none());
        assert!(started.elapsed() >= Duration::from_millis(100));
    }

    // ---- S3-11: the codex app-server reading (AC3 and AC6 of that spec) ----

    /// The real app-server exchange recorded from codex-cli 0.154.
    const CODEX_FIXTURE: &str = include_str!("../../tests/fixtures/codex/app-server-rate-limits.jsonl");

    fn codex_fixture_line(index: usize) -> Value {
        serde_json::from_str(CODEX_FIXTURE.lines().nth(index).unwrap()).unwrap()
    }

    fn codex_parse(reply: Value) -> std::result::Result<CodexLive, String> {
        parse_codex_reply(&reply, OBSERVED)
    }

    fn codex_window(used: i64, minutes: i64, resets_at: i64) -> Value {
        json!({"usedPercent": used, "windowDurationMins": minutes, "resetsAt": resets_at})
    }

    #[test]
    fn codex_ac3_a_the_real_reply_has_five_hour_and_seven_day() {
        let reading = codex_parse(codex_fixture_line(2)).unwrap();
        assert_eq!(reading.windows.len(), 2);
        assert_eq!(reading.windows[0].name, "five_hour");
        assert_eq!(reading.windows[0].used, 0.0);
        assert_eq!(reading.windows[0].resets_at, Some(1790863904));
        assert_eq!(reading.windows[1].name, "seven_day");
        assert_eq!(reading.windows[1].used, 86.0);
        assert_eq!(reading.windows[1].resets_at, Some(1791057728));
        assert_eq!(reading.ordinary_usage_allowed, Some(true));
        assert!(reading.windows.iter().all(|w| w.source == "codex_app_server" && w.observed_at == OBSERVED && w.group.is_none()));
    }

    #[test]
    fn codex_ac3_b_without_rate_limits_by_limit_id_rate_limits_is_used() {
        let reading = codex_parse(json!({"id": 1, "result": {"ordinaryUsageAllowed": true,
            "rateLimits": {"primary": codex_window(12, 300, 100), "secondary": null}}})).unwrap();
        assert_eq!(used(&reading.windows), [("five_hour", 12.0)]);
        let reading = codex_parse(json!({"id": 1, "result": {"ordinaryUsageAllowed": true, "rateLimitsByLimitId": null,
            "rateLimits": {"primary": codex_window(34, 300, 100), "secondary": null}}})).unwrap();
        assert_eq!(used(&reading.windows), [("five_hour", 34.0)]);
    }

    #[test]
    fn codex_ac3_b_rule_the_codex_bucket_wins_over_rate_limits() {
        let reading = codex_parse(json!({"id": 1, "result": {
            "rateLimits": {"primary": codex_window(1, 300, 100)},
            "rateLimitsByLimitId": {"codex": {"primary": codex_window(55, 300, 100)}}}})).unwrap();
        assert_eq!(used(&reading.windows), [("five_hour", 55.0)]);
    }

    #[test]
    fn codex_ac3_c_a_weekly_primary_is_named_seven_day() {
        let reading = codex_parse(json!({"id": 1, "result": {"rateLimitsByLimitId": {"codex": {
            "primary": codex_window(20, 10080, 100), "secondary": codex_window(5, 60, 100)}}}})).unwrap();
        assert_eq!(used(&reading.windows), [("seven_day", 20.0), ("60m", 5.0)]);
    }

    #[test]
    fn codex_ac3_rule_a_window_without_duration_is_skipped_and_null_reset_is_kept() {
        let reading = codex_parse(json!({"id": 1, "result": {"rateLimits": {
            "primary": {"usedPercent": 9, "windowDurationMins": null, "resetsAt": 1},
            "secondary": {"usedPercent": 7, "windowDurationMins": 10080, "resetsAt": null}}}})).unwrap();
        assert_eq!(used(&reading.windows), [("seven_day", 7.0)]);
        assert_eq!(reading.windows[0].resets_at, None);
        assert_eq!(reading.ordinary_usage_allowed, None);
    }

    #[test]
    fn codex_ac3_d1_signed_out_error_is_a_failure_with_its_message() {
        let error = codex_parse(json!({"id":1,"error":{"code":-32600,"message":"codex account authentication required to read rate limits"}})).unwrap_err();
        assert!(error.contains("codex account authentication required to read rate limits"), "{error}");
    }

    #[test]
    fn codex_ac3_d2_expired_token_error_is_a_failure_with_its_message() {
        let error = codex_parse(json!({"id":1,"error":{"code":-32603,"message":"401 Unauthorized"}})).unwrap_err();
        assert!(error.contains("401 Unauthorized"), "{error}");
    }

    #[test]
    fn codex_ac3_e_the_reply_line_is_found_after_skipping_the_others() {
        let lines: Vec<&str> = CODEX_FIXTURE.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(codex_reply_line(lines[0]).is_none());
        assert!(codex_reply_line(lines[1]).is_none());
        let found = lines.iter().find_map(|line| codex_reply_line(line)).unwrap();
        assert_eq!(found, codex_fixture_line(2));
        assert!(codex_reply_line(r#"{"id":1,"method":"x/y","params":{}}"#).is_none());
    }

    #[test]
    fn codex_ac3_e_rule_lines_that_are_not_json_or_have_another_id_are_skipped() {
        assert!(codex_reply_line("not json").is_none());
        assert!(codex_reply_line("[1]").is_none());
        assert!(codex_reply_line(r#"{"id":2,"result":{}}"#).is_none());
        assert!(codex_reply_line(r#"{"id":"1","result":{}}"#).is_none());
        assert!(codex_reply_line("{\"id\":1,\"result\":{}}\r\n").is_some());
    }

    #[test]
    fn codex_ac3_f_no_windows_is_success_only_when_usage_is_not_allowed() {
        let empty = |allowed: Value| json!({"id": 1, "result": {"ordinaryUsageAllowed": allowed,
            "rateLimits": {"primary": null, "secondary": null}}});
        let reading = codex_parse(empty(json!(false))).unwrap();
        assert!(reading.windows.is_empty());
        assert_eq!(reading.ordinary_usage_allowed, Some(false));
        assert!(codex_parse(empty(json!(true))).unwrap_err().contains("no quota windows"));
        assert!(codex_parse(empty(Value::Null)).unwrap_err().contains("no quota windows"));
        let missing = json!({"id": 1, "result": {"rateLimits": {"primary": null, "secondary": null}}});
        assert!(codex_parse(missing).unwrap_err().contains("no quota windows"));
    }

    #[test]
    fn codex_ac3_rule_a_reply_without_rate_limits_is_a_failure() {
        assert!(codex_parse(json!({"id": 1, "result": {}})).is_err());
        assert!(codex_parse(json!({"id": 1})).is_err());
    }

    #[test]
    fn codex_ac4_detail_keeps_only_the_listed_fields() {
        let reading = codex_parse(codex_fixture_line(2)).unwrap();
        let detail = codex_detail(&reading);
        assert_eq!(detail, json!({"source": "codex_app_server", "windows": [
            {"name": "five_hour", "used_percent": 0.0, "resets_at": "2026-10-01T14:11:44Z"},
            {"name": "seven_day", "used_percent": 86.0, "resets_at": "2026-10-03T20:02:08Z"},
        ], "ordinary_usage_allowed": true}));
        assert!(!detail.to_string().contains("accountId") && !detail.to_string().contains("plus"));
    }

    #[test]
    fn codex_plan_reads_the_codex_bucket_then_rate_limits() {
        assert_eq!(codex_plan(&codex_fixture_line(2)).as_deref(), Some("plus"));
        assert_eq!(codex_plan(&json!({"id": 1, "result": {"rateLimits": {"planType": " Pro "}}})).as_deref(), Some("pro"));
        assert_eq!(codex_plan(&json!({"id": 1, "result": {
            "rateLimitsByLimitId": {"codex": {"planType": "team"}}, "rateLimits": {"planType": "plus"}}})).as_deref(), Some("team"));
        assert_eq!(codex_plan(&json!({"id": 1, "result": {
            "rateLimitsByLimitId": {"codex": {"planType": "pro plan"}}, "rateLimits": {"planType": "plus"}}})), None);
        assert_eq!(codex_plan(&json!({"id": 1, "result": {"rateLimits": {"planType": 5}}})), None);
        assert_eq!(codex_plan(&json!({"id": 1, "result": {"rateLimits": {"primary": null}}})), None);
        assert_eq!(codex_plan(&json!({"id": 1, "error": {"code": -32600, "message": "x"}})), None);
    }

    #[test]
    fn codex_ac6_b_setup_question_names_agy_and_codex() {
        use crate::lang::Lang;
        let en = crate::lang::text(Lang::En, "setup.ask.live_quota");
        assert!(en.starts_with("Turn it on?"), "{en}");
        assert!(en.contains("agy") && en.contains("codex"), "{en}");
        assert!(!en.contains("only CLI supported"), "{en}");
        let zh = crate::lang::text(Lang::ZhTw, "setup.ask.live_quota");
        assert!(zh.contains("agy") && zh.contains("codex"), "{zh}");
        assert!(!zh.contains("目前只支援"), "{zh}");
    }

    // ---- QR-1: the automatic refresh before ranking ----

    #[test]
    fn qr1_a_a_reading_is_due_when_missing_or_strictly_older_than_the_threshold() {
        let after = Duration::from_secs(600);
        let now = 1_800_000_000;
        assert!(is_due(None, now, after));
        assert!(!is_due(Some(now - 600), now, after));
        assert!(is_due(Some(now - 601), now, after));
        assert!(!is_due(Some(now + 300), now, after));
    }

    #[test]
    fn qr1_b_the_wait_is_capped_at_ten_seconds() {
        assert_eq!(auto_limit_ms(60_000), 10_000);
        assert_eq!(auto_limit_ms(1_000), 1_000);
        assert_eq!(auto_limit_ms(10_000), 10_000);
    }

    #[test]
    fn qr1_c_only_codex_and_agy_are_refreshed_automatically() {
        assert_eq!(auto_refresh_after("codex"), Some(Duration::from_secs(600)));
        assert_eq!(auto_refresh_after("antigravity"), Some(Duration::from_secs(600)));
        assert_eq!(auto_refresh_after("claude-code"), None);
        assert_eq!(auto_refresh_after("something-else"), None);
    }

    #[test]
    fn qr1_d_the_warning_says_which_reading_is_used() {
        assert_eq!(refresh_warning("codex-a", "timed out", true),
            "live quota refresh for codex-a failed: timed out; ranking with the previous reading");
        assert_eq!(refresh_warning("codex-a", "timed out", false),
            "live quota refresh for codex-a failed: timed out; ranking without a quota reading");
    }

    #[test]
    fn qr1_e_switch_off_does_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(dir.path().to_path_buf());
        let refresh = refresh_for_ranking(&paths);
        assert_eq!(refresh.json(), json!({"enabled": false, "skipped": "off", "results": []}));
        assert!(!paths.global_db().exists());
    }

    #[test]
    fn qr1_f_setup_question_names_the_automatic_refresh() {
        use crate::lang::Lang;
        let en = crate::lang::text(Lang::En, "setup.ask.live_quota");
        assert!(en.starts_with("Turn it on?"), "{en}");
        assert!(en.contains("recommend") && en.contains("10 minutes"), "{en}");
        assert!(!en.contains("--profile auto"), "{en}");
        let zh = crate::lang::text(Lang::ZhTw, "setup.ask.live_quota");
        assert!(zh.contains("recommend") && zh.contains("10 分鐘"), "{zh}");
        assert!(!zh.contains("--profile auto"), "{zh}");
    }

    // ---- E7: Claude Code's /usage reading ----

    /// The real stream-json answer recorded from Claude Code 2.1.296.
    const CLAUDE_FIXTURE: &str = include_str!("../../tests/fixtures/claude-usage-stream.jsonl");

    /// Each value on its own line.
    fn claude_lines(values: &[Value]) -> String {
        values.iter().map(|v| format!("{v}\n")).collect()
    }

    /// A successful local `/usage` answer whose usage_report has these rows.
    fn claude_answer(limits: Value) -> String {
        claude_lines(&[
            json!({"type": "system", "subtype": "init"}),
            json!({"type": "assistant", "usage_report": {"rate_limits": {"limits": limits}}}),
            json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 0, "total_cost_usd": 0,
                "local_command": "usage", "result": "Current session: 1% used"}),
        ])
    }

    fn claude_ok_result() -> Value {
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 0, "total_cost_usd": 0,
            "local_command": "usage", "result": "ok"})
    }

    fn claude_model_turn() -> Value {
        json!({"type": "result", "subtype": "success", "is_error": false, "num_turns": 1, "total_cost_usd": 0.01, "result": "hi"})
    }

    fn claude_error(text: &str) -> String {
        parse_claude_usage(text, OBSERVED).unwrap_err()
    }

    fn assert_fixture_windows(windows: &[Window]) {
        assert_eq!(used(windows), [("five_hour", 1.0), ("seven_day", 23.0)]);
        assert_eq!(windows[0].resets_at, Some(epoch("2026-10-12T08:20:00Z")));
        assert_eq!(windows[1].resets_at, Some(epoch("2026-10-16T08:00:00Z")));
        assert!(windows.iter().all(|w| w.source == "claude_usage" && w.observed_at == OBSERVED && w.group.is_none()));
    }

    #[test]
    fn claude_usage_real_answer_reads_five_hour_and_seven_day() {
        assert_fixture_windows(&parse_claude_usage(CLAUDE_FIXTURE, OBSERVED).unwrap());
    }

    #[test]
    fn claude_usage_lines_that_are_not_json_objects_are_skipped() {
        let text = format!("not json\n[1,2]\n\n{CLAUDE_FIXTURE}");
        assert_fixture_windows(&parse_claude_usage(&text, OBSERVED).unwrap());
    }

    #[test]
    fn claude_usage_checks_fail_in_order() {
        let init = json!({"type": "system", "subtype": "init"});
        let report = |rate_limits: Value| json!({"type": "assistant", "usage_report": {"rate_limits": rate_limits}});
        // (a)
        assert!(claude_error("").contains("claude printed no result line"));
        assert!(claude_error(&claude_lines(&[init])).contains("claude printed no result line"));
        // (b) before (c)
        let signed_out = json!({"type": "result", "is_error": true, "result": "Not logged in · Please run /login"});
        assert_eq!(claude_error(&claude_lines(&[signed_out])), "claude reported an error: Not logged in · Please run /login");
        let long = json!({"type": "result", "is_error": true, "result": "x".repeat(300)});
        assert_eq!(claude_error(&claude_lines(&[long])), format!("claude reported an error: {}", "x".repeat(200)));
        let not_text = json!({"type": "result", "is_error": true, "result": 5});
        assert_eq!(claude_error(&claude_lines(&[not_text])), "claude reported an error: no message");
        // (c) before (d)
        let error = claude_error(&claude_lines(&[claude_model_turn()]));
        assert!(error.contains("did not answer /usage locally") && !error.contains("no usage_report"), "{error}");
        let cost = json!({"type": "result", "is_error": false, "local_command": "cost"});
        assert!(claude_error(&claude_lines(&[cost])).contains("did not answer /usage locally"));
        let good_report = report(json!({"limits": [{"kind": "session", "percent": 1}]}));
        assert!(claude_error(&claude_lines(&[good_report, claude_model_turn()])).contains("did not answer /usage locally"));
        // (d)
        let no_report = json!({"type": "assistant", "message": {}});
        assert!(claude_error(&claude_lines(&[no_report, claude_ok_result()])).contains("no usage_report"));
        let text_report = json!({"type": "assistant", "usage_report": "x"});
        assert!(claude_error(&claude_lines(&[text_report, claude_ok_result()])).contains("no usage_report"));
        // (e)
        assert!(claude_error(&claude_lines(&[report(Value::Null), claude_ok_result()])).contains("could not read the plan's usage"));
        let missing = json!({"type": "assistant", "usage_report": {}});
        assert!(claude_error(&claude_lines(&[missing, claude_ok_result()])).contains("could not read the plan's usage"));
        // (f)
        assert!(claude_error(&claude_lines(&[report(json!({"limits": null})), claude_ok_result()])).contains("no usage rows"));
        assert!(claude_error(&claude_lines(&[report(json!({"limits": {}})), claude_ok_result()])).contains("no usage rows"));
        // (g)
        assert!(claude_error(&claude_answer(json!([]))).contains("no quota windows"));
    }

    #[test]
    fn claude_usage_the_last_result_and_usage_report_lines_count() {
        let signed_out = json!({"type": "result", "is_error": true, "result": "Not logged in"});
        let report = |percent: i64| json!({"type": "assistant", "usage_report": {"rate_limits": {"limits": [{"kind": "session", "percent": percent}]}}});
        let windows = parse_claude_usage(&claude_lines(&[report(5), signed_out, claude_ok_result()]), OBSERVED).unwrap();
        assert_eq!(used(&windows), [("five_hour", 5.0)]);
        let windows = parse_claude_usage(&claude_lines(&[report(10), report(20), claude_ok_result()]), OBSERVED).unwrap();
        assert_eq!(used(&windows), [("five_hour", 20.0)]);
    }

    #[test]
    fn claude_usage_row_rules() {
        let windows = parse_claude_usage(&claude_answer(json!([
            {"kind": "session", "percent": 12.5, "resets_at": "2099-01-01T08:00:00.123456+08:00"},
            {"kind": "weekly_all", "percent": 0, "resets_at": null},
            {"kind": "weekly_scoped", "percent": 100, "scope": {"model": {"display_name": " Son\u{7}net "}}, "resets_at": "not a time"},
            {"kind": "weekly_scoped", "percent": 5, "scope": {"model": null, "surface": {"display_name": "Cowork"}}, "resets_at": 5},
            {"kind": "weekly_scoped", "percent": 7, "scope": {"model": {"display_name": "\u{7}\u{7}"}}},
            {"kind": "session", "percent": 99},
            {"kind": "new_meter", "percent": 50, "scope": {"model": {"display_name": "a".repeat(40)}}},
            {"kind": "Weekly All", "percent": 1},
            {"kind": "", "percent": 1},
            {"kind": "a".repeat(33), "percent": 1},
            {"kind": 5, "percent": 1},
            {"percent": 1},
            {"kind": "x1", "percent": "35"},
            {"kind": "x2", "percent": null},
            {"kind": "x3", "percent": 100.5},
            {"kind": "x4", "percent": -0.1},
            {"kind": "x5"},
            {"kind": "x6", "percent": 1, "scope": {"model": {"display_name": 9}}},
            3
        ])), OBSERVED).unwrap();
        let new_meter = format!("new_meter:{}", "a".repeat(32));
        assert_eq!(used(&windows), [("five_hour", 12.5), ("seven_day", 0.0), ("weekly_scoped:Sonnet", 100.0),
            ("weekly_scoped:Cowork", 5.0), ("weekly_scoped", 7.0), (new_meter.as_str(), 50.0), ("x6", 1.0)]);
        assert_eq!(windows[0].resets_at, Some(epoch("2099-01-01T00:00:00Z")));
        assert_eq!(windows[1].resets_at, None);
        assert_eq!(windows[2].resets_at, None);
        assert_eq!(windows[3].resets_at, None);
    }

    #[test]
    fn claude_usage_detail_keeps_only_the_listed_fields() {
        let detail = claude_detail(&parse_claude_usage(CLAUDE_FIXTURE, OBSERVED).unwrap());
        assert_eq!(detail, json!({"source": "claude_usage", "windows": [
            {"name": "five_hour", "used_percent": 1.0, "resets_at": "2026-10-12T08:20:00Z"},
            {"name": "seven_day", "used_percent": 23.0, "resets_at": "2026-10-16T08:00:00Z"},
        ]}));
        let text = detail.to_string();
        for word in ["severity", "is_active", "extra_usage", "subscription"] { assert!(!text.contains(word), "{text}") }
    }

    #[test]
    fn claude_usage_setup_question_names_claude() {
        use crate::lang::Lang;
        let en = crate::lang::text(Lang::En, "setup.ask.live_quota");
        assert!(en.starts_with("Turn it on?"), "{en}");
        assert!(en.contains("claude") && en.contains("agy") && en.contains("codex"), "{en}");
        assert!(!en.contains("only when you explicitly"), "{en}");
        let zh = crate::lang::text(Lang::ZhTw, "setup.ask.live_quota");
        assert!(zh.contains("claude") && zh.contains("agy") && zh.contains("codex"), "{zh}");
    }
}
