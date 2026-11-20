//! AUTO-1a: run a task to the end (docs/post-phase2/thoth-only-outlet-scope-2026-10-16.md, G3).
//!
//! `pipeline start` checks what the commander supplied (ordered implementer
//! and reviewer lists, an attempt ceiling, time limits and one reason),
//! records a pipeline and starts `thoth pipeline-run ID` in the background.
//! That process walks dispatch, wait, the protected-change gate, verify,
//! review start and the verdict by running ordinary thoth commands as
//! children, and settles with a short conclusion: mergeable, out of attempts
//! or in need of the commander's judgement.
//!
//! AUTO-1b continues instead of stopping: it walks the commander's ordered
//! lists forward (never wrapping, never reordering or ranking), skips an
//! entry that `recommend` calls ineligible or whose deciding quota window is
//! at the commander's `--quota-threshold`, reworks a REJECT on the next
//! implementer entry (folding findings into the spec only when the commander
//! opted in with `--fold-on-reject`), hands an interrupted run over, resumes a
//! crashed one once, retries one timeout at twice the limit and rotates
//! reviewers. Facts it cannot read are logged and never hold an entry back;
//! the launch commands' own checks stay authoritative.
//!
//! State: one `pipelines` row per pipeline in the repo's state.db (created on
//! first use, so a repository initialised before this feature works as it
//! is), the `pipeline_*` events, and a user-visible folder
//! `THOTH_HOME/repos/REPO/pipelines/ID/` with `pipeline.log` and each step's
//! output. Verify capacity is shared across repositories through
//! `THOTH_HOME/verify-slots/slot-K.json`.

use crate::config::Paths;
use crate::db::{self, state};
use crate::ids;
use crate::model::{event_type, role};
use crate::output;
use crate::redact::Masker;
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Set on every child command a pipeline runs: the pipeline it belongs to.
/// With THOTH_RUN_ID unset, it lets that child past the ownership gate.
pub const PIPELINE_ID_ENV: &str = "THOTH_PIPELINE_ID";
/// The per-check deadline verify gets when `--verify-timeout` is left out.
pub const DEFAULT_VERIFY_TIMEOUT_SECS: u64 = 120 * 60;
/// How long a pipeline waits for a verify slot when `--verify-wait` is left out.
pub const DEFAULT_VERIFY_WAIT_SECS: u64 = 2 * 3600;
/// The heartbeat interval; status calls a pipeline stale after 15 s of silence.
const HEARTBEAT_EVERY: Duration = Duration::from_secs(2);
/// Each `thoth wait --run` the pipeline runs is this short, so a stop
/// request and a lost runner are noticed between them.
const WAIT_POLL: &str = "2s";
/// Test seams (no effect unless set): fail the background start, or fail
/// the background process when it reaches the named step.
const SPAWN_FAIL_SEAM: &str = "THOTH_TEST_PIPELINE_SPAWN_FAIL";
const FAIL_AT_SEAM: &str = "THOTH_TEST_PIPELINE_FAIL_AT";
/// AUTO-1b test seam: `recommend_exit` / `usage_exit` add an unknown flag to
/// that preflight command (a real non-zero exit), `recommend_text` /
/// `usage_text` drop its `--json` (stdout that is not JSON).
const PREFLIGHT_SEAM: &str = "THOTH_TEST_PIPELINE_PREFLIGHT";

fn preflight_seam(kind: &str, args: &mut Vec<String>) {
    let Ok(seam) = std::env::var(PREFLIGHT_SEAM) else { return };
    if seam == format!("{kind}_exit") {
        args.push("--no-such-flag".into());
    } else if seam == format!("{kind}_text") {
        args.retain(|a| a != "--json");
    }
}

const TABLE: &str = "CREATE TABLE IF NOT EXISTS pipelines (
  pipeline_id       TEXT PRIMARY KEY,
  task_id           TEXT NOT NULL,
  status            TEXT NOT NULL,
  step              TEXT NOT NULL,
  settings          TEXT NOT NULL,
  pipeline_pid      INTEGER,
  heartbeat_at      TEXT,
  started_at        TEXT NOT NULL,
  ended_at          TEXT,
  stop_requested    INTEGER NOT NULL DEFAULT 0,
  stop              TEXT,
  dir               TEXT NOT NULL,
  implementer_index INTEGER NOT NULL DEFAULT 0,
  reviewer_index    INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_pipelines_task ON pipelines(task_id);";

const COLS: &str = "pipeline_id, task_id, status, step, settings, pipeline_pid, heartbeat_at, \
                    started_at, ended_at, stop_requested, stop, dir, implementer_index, reviewer_index";

fn ensure_table(conn: &Connection) -> Result<()> {
    conn.execute_batch(TABLE)?;
    Ok(())
}

/// Whether this state.db has the table; false for a repository that never
/// started a pipeline (also when it is opened read-only).
fn table_exists(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pipelines'",
        [],
        |r| r.get::<_, i64>(0),
    )
    .map(|n| n > 0)
    .unwrap_or(false)
}

/// One implementer or reviewer choice, exactly as the commander supplied it
/// (the effort normalised to its lower-case level).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub profile: String,
    pub model: String,
    pub effort: Option<String>,
}

impl Entry {
    fn text(&self) -> String {
        match &self.effort {
            Some(e) => format!("{}:{}:{e}", self.profile, self.model),
            None => format!("{}:{}", self.profile, self.model),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub implementers: Vec<Entry>,
    pub reviewers: Vec<Entry>,
    pub max_attempts: i64,
    pub timeout_seconds: Option<u64>,
    pub review_timeout_seconds: Option<u64>,
    pub verify_timeout_seconds: u64,
    pub verify_wait_seconds: u64,
    pub reason: String,
    /// AUTO-1b: an entry whose deciding quota window is at least this
    /// percent used is skipped; null when not given.
    #[serde(default)]
    pub quota_threshold: Option<i64>,
    /// AUTO-1b: at important/high risk, append a REJECT's findings unchanged
    /// to the spec's constraints instead of stopping for a rewrite.
    #[serde(default)]
    pub fold_on_reject: bool,
}

#[derive(Debug, Clone)]
struct Row {
    id: String,
    task_id: String,
    status: String,
    step: String,
    settings: Value,
    pid: Option<i64>,
    heartbeat_at: Option<String>,
    started_at: String,
    ended_at: Option<String>,
    stop_requested: bool,
    stop: Option<Value>,
    dir: String,
    implementer_index: i64,
    reviewer_index: i64,
}

fn row_of(r: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let settings: String = r.get(4)?;
    let stop: Option<String> = r.get(10)?;
    Ok(Row {
        id: r.get(0)?,
        task_id: r.get(1)?,
        status: r.get(2)?,
        step: r.get(3)?,
        settings: serde_json::from_str(&settings).unwrap_or(Value::Null),
        pid: r.get(5)?,
        heartbeat_at: r.get(6)?,
        started_at: r.get(7)?,
        ended_at: r.get(8)?,
        stop_requested: r.get::<_, i64>(9)? != 0,
        stop: stop.and_then(|s| serde_json::from_str(&s).ok()),
        dir: r.get(11)?,
        implementer_index: r.get(12)?,
        reviewer_index: r.get(13)?,
    })
}

fn get_row(conn: &Connection, id: &str) -> Result<Option<Row>> {
    Ok(conn
        .query_row(&format!("SELECT {COLS} FROM pipelines WHERE pipeline_id = ?1"), [id], row_of)
        .optional()?)
}

/// The task's newest pipeline (ids are ULIDs, so they sort by start time).
fn latest_for_task(conn: &Connection, task_id: &str) -> Result<Option<Row>> {
    if !table_exists(conn) {
        return Ok(None);
    }
    Ok(conn
        .query_row(
            &format!("SELECT {COLS} FROM pipelines WHERE task_id = ?1 ORDER BY pipeline_id DESC LIMIT 1"),
            [task_id],
            row_of,
        )
        .optional()?)
}

fn running_for_task(conn: &Connection, task_id: &str) -> Result<Vec<Row>> {
    if !table_exists(conn) {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLS} FROM pipelines WHERE task_id = ?1 AND status = 'running' ORDER BY pipeline_id"
    ))?;
    let rows = stmt.query_map([task_id], row_of)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// --- liveness -------------------------------------------------------------------

fn stem(s: &str) -> String {
    let name = Path::new(s.trim())
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_else(|| s.to_string())
        .to_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}

fn thoth_stem() -> String {
    std::env::current_exe()
        .map(|p| stem(&p.to_string_lossy()))
        .unwrap_or_else(|_| "thoth".to_string())
}

/// Whether `pid` is a live process running this thoth executable's image. A
/// pid the OS handed to another program counts as gone; a listing that
/// cannot be read counts as alive, so nothing is taken over on a guess.
fn live_thoth(pid: i64) -> bool {
    let Ok(pid) = u32::try_from(pid) else { return false };
    if pid == 0 {
        return false;
    }
    match crate::process::image_name(pid) {
        Ok(Some(name)) => stem(&name) == thoth_stem(),
        Ok(None) => false,
        Err(_) => true,
    }
}

/// Running, a heartbeat older than 15 s (as `status` judges runs) and no
/// matching live thoth process at its pid.
fn is_stale(row: &Row, now: &str) -> bool {
    row.status == "running"
        && crate::commands::status::heartbeat_is_stale(now, row.heartbeat_at.as_deref(), "running")
        && !row.pid.is_some_and(live_thoth)
}

// --- the stop record -------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Stop {
    reason: String,
    summary: String,
    next: Vec<String>,
    evidence: Value,
}

impl Stop {
    fn new(reason: &str, summary: impl Into<String>) -> Stop {
        Stop { reason: reason.to_string(), summary: summary.into(), next: Vec::new(), evidence: json!({}) }
    }
    fn next(mut self, n: impl Into<String>) -> Stop {
        self.next.push(n.into());
        self
    }
    fn evidence(mut self, v: Value) -> Stop {
        self.evidence = v;
        self
    }
    fn json(&self) -> Value {
        json!({"reason": self.reason, "summary": self.summary, "next": self.next, "evidence": self.evidence})
    }
}

fn append_log(dir: &Path, line: &str, masker: &Masker) {
    let line = masker.mask_text(line).0;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("pipeline.log")) {
        let _ = writeln!(f, "{} {line}", ids::now_rfc3339());
    }
}

fn step_details(conn: &Connection, task_id: &str, pipeline_id: &str) -> Result<Vec<Value>> {
    let mut stmt =
        conn.prepare("SELECT detail FROM events WHERE task_id = ?1 AND type = ?2 ORDER BY seq")?;
    let rows = stmt
        .query_map(params![task_id, event_type::PIPELINE_STEP], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|d| serde_json::from_str::<Value>(&d).ok())
        .filter(|d| d["pipeline_id"] == pipeline_id)
        .collect())
}

/// Settles a running pipeline as `stopped` with `stop`, and writes its one
/// `pipeline_stopped` event. False when it had already settled.
fn settle_in(conn: &Connection, row: &Row, stop: &Stop, masker: &Masker) -> Result<bool> {
    let now = ids::now_rfc3339();
    let mut stop_json = stop.json();
    masker.mask_value(&mut stop_json);
    let n = conn.execute(
        "UPDATE pipelines SET status = 'stopped', ended_at = ?1, stop = ?2 \
         WHERE pipeline_id = ?3 AND status = 'running'",
        params![now, stop_json.to_string(), row.id],
    )?;
    if n == 0 {
        return Ok(false);
    }
    let task_status = state::get_task(conn, &row.task_id)?.map(|t| t.status);
    let steps = step_details(conn, &row.task_id, &row.id)?.len();
    state::append_event(
        conn,
        &state::NewEvent {
            task_id: Some(row.task_id.clone()),
            run_id: None,
            event_type: event_type::PIPELINE_STOPPED.to_string(),
            detail: json!({
                "pipeline_id": row.id,
                "status": "stopped",
                "stop_reason": stop_json["reason"],
                "stop_summary": stop_json["summary"],
                "stop_next": stop_json["next"],
                "stop_evidence": stop_json["evidence"],
                "ended_at": now,
                "task_status": task_status,
                "steps": steps,
                "implementer_index": row.implementer_index,
                "reviewer_index": row.reviewer_index,
                "attempts_used": attempts_used(conn, &row.task_id),
            }),
        },
    )?;
    append_log(
        Path::new(&row.dir),
        &format!("stopped: {} - {}", stop.reason, stop.summary),
        masker,
    );
    Ok(true)
}

fn settle(conn: &Connection, row: &Row, stop: &Stop, masker: &Masker) -> Result<bool> {
    let tx = db::write_tx(conn)?;
    let done = settle_in(&tx, row, stop, masker)?;
    tx.commit()?;
    Ok(done)
}

fn stale_stop(row: &Row) -> Stop {
    Stop::new(
        "stale",
        format!(
            "pipeline {} stopped answering: last heartbeat {} and no thoth process at pid {}; it is \
             recorded as stopped. No worker was cancelled.",
            row.id,
            row.heartbeat_at.as_deref().unwrap_or("never"),
            row.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".into())
        ),
    )
    .next(format!("thoth status {}", row.task_id))
    .next(format!("thoth pipeline start {} ... (attaches to a live run or continues from in_review)", row.task_id))
    .evidence(json!({"pipeline_pid": row.pid, "heartbeat_at": row.heartbeat_at}))
}

// --- start ---------------------------------------------------------------------

pub struct StartArgs<'a> {
    pub task: &'a str,
    pub implementers: &'a [String],
    pub reviewers: &'a [String],
    pub max_attempts: Option<&'a str>,
    pub timeout: Option<&'a str>,
    pub review_timeout: Option<&'a str>,
    pub verify_timeout: Option<&'a str>,
    pub verify_wait: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub quota_threshold: Option<&'a str>,
    pub fold_on_reject: bool,
    pub json: bool,
}

/// The task statuses `pipeline start` continues from (AUTO-1b added the
/// last four).
pub const ENTRY_STATES: [&str; 6] = ["queued", "in_review", "reject", "unverified", "interrupted", "failed"];

/// `--quota-threshold N`: a whole number from 1 to 100.
fn quota_threshold(value: Option<&str>) -> Result<Option<i64>> {
    let Some(text) = value else { return Ok(None) };
    match text.trim().parse::<i64>() {
        Ok(n) if (1..=100).contains(&n) => Ok(Some(n)),
        _ => bail!("invalid --quota-threshold '{text}': it must be a whole number from 1 to 100 (a used percent)"),
    }
}

fn duration(flag: &str, value: Option<&str>) -> Result<Option<u64>> {
    let Some(text) = value else { return Ok(None) };
    match crate::cli::parse_duration_secs(text.trim()) {
        Ok(0) => bail!("invalid --{flag} '{text}': it must be at least 1 second"),
        Ok(n) => Ok(Some(n)),
        Err(e) => bail!("invalid --{flag} '{text}': {e}"),
    }
}

/// Checks one `PROFILE:MODEL[:EFFORT]` entry; reads only.
fn parse_entry(gconn: &Connection, flag: &str, pos: usize, total: usize, raw: &str) -> Result<Entry> {
    use crate::commands::auto_pick::{effort_optional, is_auto, AUTO};
    let fail = |why: String| anyhow!("invalid --{flag} entry {pos} of {total} ('{raw}'): {why}");
    let parts: Vec<&str> = raw.split(':').map(str::trim).collect();
    if !(2..=3).contains(&parts.len()) || parts.iter().any(|p| p.is_empty()) {
        return Err(fail(
            "write PROFILE:MODEL or PROFILE:MODEL:EFFORT, two or three parts, none of them empty".into(),
        ));
    }
    let (profile_id, model, effort) = (parts[0], parts[1], parts.get(2).copied());
    if profile_id == AUTO || is_auto(model) || effort.is_some_and(is_auto) {
        return Err(fail(
            "auto was removed: Thoth does not pick a profile, model or effort for you; give each one yourself".into(),
        ));
    }
    let Some(profile) = db::global::get_profile(gconn, profile_id)? else {
        return Err(fail(format!("no such profile: {profile_id}")));
    };
    crate::commands::profile::require_supported_harness(&format!("profile {profile_id}"), &profile.harness)
        .map_err(|e| fail(format!("{e:#}")))?;
    let adapter = crate::adapter::for_harness(&profile.harness);
    let known = crate::model_list::known_models_text(gconn, &profile);
    adapter
        .check_model(&profile.profile_id, model, &known)
        .map_err(|e| fail(format!("{e:#}")))?;
    let effort = match effort {
        Some(e) => Some(crate::adapter::effort::parse_level(e).map_err(fail)?),
        None if effort_optional(&profile, model) => None,
        None => {
            return Err(fail(format!(
                "model {model} on profile {profile_id} needs an effort (low, medium, high, xhigh, max or \
                 ultra); it can be left out only for a model without an effort setting (haiku) or an agy \
                 model whose name ends in its level"
            )))
        }
    };
    Ok(Entry { profile: profile_id.to_string(), model: model.to_string(), effort })
}

pub fn start(paths: &Paths, cwd: &Path, a: StartArgs<'_>) -> Result<i32> {
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!("pipeline start refuses to run with THOTH_RUN_ID set: a pipeline is started by the commander, not by a run");
    }
    let reason = match a.reason.map(str::trim) {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => bail!(
            "--reason TEXT is required (even when require-reason is off): say why these implementers \
             and reviewers were chosen; it is sent with every launch the pipeline makes"
        ),
    };
    let timeout = duration("timeout", a.timeout)?;
    let review_timeout = duration("review-timeout", a.review_timeout)?;
    let verify_timeout = duration("verify-timeout", a.verify_timeout)?.unwrap_or(DEFAULT_VERIFY_TIMEOUT_SECS);
    let verify_wait = duration("verify-wait", a.verify_wait)?.unwrap_or(DEFAULT_VERIFY_WAIT_SECS);
    let max_attempts: Option<i64> = match a.max_attempts {
        None => None,
        Some(t) => match t.trim().parse::<i64>() {
            Ok(n) if n >= 1 => Some(n),
            _ => bail!("invalid --max-attempts '{t}': it must be a whole number of at least 1"),
        },
    };
    let quota_threshold = quota_threshold(a.quota_threshold)?;
    if a.implementers.is_empty() {
        bail!("the implementer list is empty: give --implementer PROFILE:MODEL[:EFFORT] at least once");
    }
    if a.reviewers.is_empty() {
        bail!("the reviewer list is empty: give --reviewer PROFILE:MODEL[:EFFORT] at least once");
    }

    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, a.task)?;
    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow!("task {task_id} disappeared"))?;
    if crate::spec::is_document_task(&task.spec) {
        bail!(
            "task {task_id} is a document task (task_type docs, research or review); the pipeline runs \
             code tasks only. Take its delivery with `thoth task accept {task_id} --reason TEXT` once it \
             is in_review"
        );
    }
    // A live pipeline is named before the task's own state: while one runs,
    // the task is often `running`. A stale one is settled further down, once
    // every refusal has passed.
    let now = ids::now_rfc3339();
    let mut stale_olds = Vec::new();
    for old in running_for_task(&conn, &task_id)? {
        if is_stale(&old, &now) {
            stale_olds.push(old);
        } else {
            bail!(
                "task {task_id} already has a running pipeline {} (step {}); follow it with `thoth \
                 pipeline show {task_id}`, or stop it with `thoth pipeline stop {task_id}`",
                old.id,
                old.step
            );
        }
    }
    if !ENTRY_STATES.contains(&task.status.as_str()) {
        bail!(
            "task {task_id} is '{}'; pipeline start needs {}",
            task.status,
            "queued, in_review, reject, unverified, interrupted or failed"
        );
    }
    let gconn = db::open_global(paths)?;
    let mut implementers = Vec::new();
    for (i, raw) in a.implementers.iter().enumerate() {
        implementers.push(parse_entry(&gconn, "implementer", i + 1, a.implementers.len(), raw)?);
    }
    let mut reviewers = Vec::new();
    for (i, raw) in a.reviewers.iter().enumerate() {
        reviewers.push(parse_entry(&gconn, "reviewer", i + 1, a.reviewers.len(), raw)?);
    }
    let max_attempts = match max_attempts {
        Some(n) => n,
        None => state::get_task_attempt_cap(&conn, &task_id)?,
    };

    let masker = Masker::load(Some(&paths.home));
    let settings = Settings {
        implementers,
        reviewers,
        max_attempts,
        timeout_seconds: timeout,
        review_timeout_seconds: review_timeout,
        verify_timeout_seconds: verify_timeout,
        verify_wait_seconds: verify_wait,
        reason: masker.mask_text(&reason).0,
        quota_threshold,
        fold_on_reject: a.fold_on_reject,
    };
    let settings_json = serde_json::to_value(&settings)?;
    let require_reason = crate::commands::settings::require_reason(&paths.home);
    let id = ids::new_run_id();
    let dir = paths.repo_dir(&repo_id).join("pipelines").join(&id);
    ensure_table(&conn)?;
    let row = Row {
        id: id.clone(),
        task_id: task_id.clone(),
        status: "running".into(),
        step: "starting".into(),
        settings: settings_json.clone(),
        pid: None,
        heartbeat_at: Some(now.clone()),
        started_at: now.clone(),
        ended_at: None,
        stop_requested: false,
        stop: None,
        dir: dir.display().to_string(),
        implementer_index: 0,
        reviewer_index: 0,
    };
    {
        let tx = db::write_tx(&conn)?;
        for old in running_for_task(&tx, &task_id)? {
            if !stale_olds.iter().any(|s| s.id == old.id) {
                bail!(
                    "task {task_id} already has a running pipeline {} (step {}); follow it with `thoth \
                     pipeline show {task_id}`, or stop it with `thoth pipeline stop {task_id}`",
                    old.id,
                    old.step
                );
            }
        }
        for old in &stale_olds {
            settle_in(&tx, old, &stale_stop(old), &masker)?;
        }
        tx.execute(
            "INSERT INTO pipelines (pipeline_id, task_id, status, step, settings, heartbeat_at, \
             started_at, dir) VALUES (?1, ?2, 'running', 'starting', ?3, ?4, ?4, ?5)",
            params![id, task_id, settings_json.to_string(), now, row.dir],
        )?;
        state::append_event(
            &tx,
            &state::NewEvent {
                task_id: Some(task_id.clone()),
                run_id: None,
                event_type: event_type::PIPELINE_STARTED.to_string(),
                detail: json!({
                    "pipeline_id": id,
                    "settings": settings_json,
                    "entry_status": task.status,
                    "reason": settings.reason,
                    "require_reason": require_reason,
                    "quota_threshold": settings.quota_threshold,
                    "fold_on_reject": settings.fold_on_reject,
                }),
            },
        )?;
        tx.commit()?;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    append_log(
        &dir,
        &format!(
            "started for task {task_id} ({}); implementers {}; reviewers {}; max attempts {max_attempts}; \
             quota threshold {}; fold on reject {}",
            task.status,
            settings.implementers.iter().map(Entry::text).collect::<Vec<_>>().join(", "),
            settings.reviewers.iter().map(Entry::text).collect::<Vec<_>>().join(", "),
            settings.quota_threshold.map(|t| format!("{t}%")).unwrap_or_else(|| "none".into()),
            settings.fold_on_reject
        ),
        &masker,
    );

    let pid = match spawn_background(&root, &id) {
        Ok(pid) => pid,
        Err(e) => {
            let stop = Stop::new(
                "spawn_failed",
                format!("the pipeline's background process could not be started: {e:#}"),
            )
            .next(format!("thoth pipeline start {task_id} --implementer ... --reviewer ... --reason TEXT"))
            .evidence(json!({"dir": row.dir}));
            settle(&conn, &row, &stop, &masker)?;
            bail!(
                "could not start the background process of pipeline {id}: {e:#}; it is recorded as \
                 stopped (spawn_failed)"
            );
        }
    };
    conn.execute(
        "UPDATE pipelines SET pipeline_pid = ?1 WHERE pipeline_id = ?2",
        params![pid as i64, id],
    )?;

    // AUTO-1b: every entry of both lists is used in order, so AUTO-1a's
    // "only the first entry" warning is gone; the key stays for later notes.
    let warnings: Vec<String> = Vec::new();
    let v = json!({
        "ok": true,
        "pipeline_id": id,
        "task_id": task_id,
        "status": "running",
        "step": "starting",
        "settings": settings_json,
        "pipeline_pid": pid,
        "warnings": warnings,
    });
    if !a.json {
        for w in &warnings {
            masked_eprintln!("warning: {w}");
        }
    }
    output::emit(
        a.json,
        &v,
        &format!(
            "pipeline {id} started for task {task_id} (pid {pid}); follow it with `thoth pipeline show {task_id}`"
        ),
    );
    Ok(0)
}

fn spawn_background(root: &Path, id: &str) -> Result<u32> {
    if std::env::var_os(SPAWN_FAIL_SEAM).is_some() {
        bail!("{SPAWN_FAIL_SEAM} is set (test seam)");
    }
    let exe = std::env::current_exe()?;
    crate::process::controller().spawn_detached(
        &exe.to_string_lossy(),
        &["pipeline-run".to_string(), id.to_string()],
        root,
    )
}

// --- show, stop, status ------------------------------------------------------------

fn step_view(d: &Value) -> Value {
    json!({
        "index": d["index"],
        "kind": d["kind"],
        "run_id": d["run_id"],
        "exit_code": d["exit_code"],
        "ok": d["ok"],
        "duration_ms": d["duration_ms"],
        "error": d["error"],
        "waited_for_slot_ms": d["waited_for_slot_ms"],
        "result": d["result"],
        "entry_index": d["entry_index"],
        "skipped": if d["skipped"].is_array() { d["skipped"].clone() } else { json!([]) },
        "attempt": d["attempt"],
        "reason": d["reason"],
        "warning": d["warning"],
    })
}

/// AC9: the highest attempt of any run of the task (0 without a run).
fn attempts_used(conn: &Connection, task_id: &str) -> i64 {
    state::max_attempt(conn, task_id).ok().flatten().unwrap_or(0)
}

fn show_doc(conn: &Connection, row: &Row, now: &str) -> Result<Value> {
    let task_status = state::get_task(conn, &row.task_id)?.map(|t| t.status);
    let steps: Vec<Value> = step_details(conn, &row.task_id, &row.id)?.iter().map(step_view).collect();
    Ok(json!({
        "ok": true,
        "pipeline_id": row.id,
        "task_id": row.task_id,
        "task_status": task_status,
        "status": row.status,
        "step": row.step,
        "stale": is_stale(row, now),
        "stop_requested": row.stop_requested,
        "settings": row.settings,
        "implementer_index": row.implementer_index,
        "reviewer_index": row.reviewer_index,
        "attempts_used": attempts_used(conn, &row.task_id),
        "pipeline_pid": row.pid,
        "heartbeat_at": row.heartbeat_at,
        "started_at": row.started_at,
        "ended_at": row.ended_at,
        "dir": row.dir,
        "steps": steps,
        "stop": row.stop,
    }))
}

fn entries_text(v: &Value) -> String {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|e| {
                    let mut s = format!("{}:{}", e["profile"].as_str().unwrap_or("?"), e["model"].as_str().unwrap_or("?"));
                    if let Some(eff) = e["effort"].as_str() {
                        s.push(':');
                        s.push_str(eff);
                    }
                    s
                })
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

fn show_human(doc: &Value) -> String {
    let task = doc["task_id"].as_str().unwrap_or("?");
    let id = doc["pipeline_id"].as_str().unwrap_or("?");
    let mut lines = Vec::new();
    if doc["status"] == "running" {
        if doc["stale"] == true {
            lines.push(format!(
                "pipeline {id} for task {task} has stopped answering (last heartbeat {}); start it again \
                 with `thoth pipeline start {task} ...`, or settle it with `thoth pipeline stop {task}`",
                doc["heartbeat_at"].as_str().unwrap_or("-")
            ));
        } else {
            lines.push(format!(
                "pipeline {id} for task {task} is running: {}{} (task {})",
                doc["step"].as_str().unwrap_or("?"),
                if doc["stop_requested"] == true { ", stop requested" } else { "" },
                doc["task_status"].as_str().unwrap_or("?")
            ));
        }
    } else {
        let stop = &doc["stop"];
        lines.push(format!(
            "pipeline {id} for task {task} stopped: {} - {}",
            stop["reason"].as_str().unwrap_or("?"),
            stop["summary"].as_str().unwrap_or("")
        ));
        for n in stop["next"].as_array().into_iter().flatten() {
            if let Some(n) = n.as_str() {
                lines.push(format!("  next: {n}"));
            }
        }
    }
    let s = &doc["settings"];
    lines.push(format!(
        "settings: implementers {}; reviewers {}; max attempts {}; verify timeout {}s, verify wait {}s; quota threshold {}; fold on reject {}",
        entries_text(&s["implementers"]),
        entries_text(&s["reviewers"]),
        s["max_attempts"],
        s["verify_timeout_seconds"],
        s["verify_wait_seconds"],
        s["quota_threshold"].as_i64().map(|t| format!("{t}%")).unwrap_or_else(|| "none".into()),
        s["fold_on_reject"].as_bool().unwrap_or(false)
    ));
    lines.push(format!(
        "position: implementer entry {}, reviewer entry {}; attempts used {}",
        doc["implementer_index"].as_i64().unwrap_or(0) + 1,
        doc["reviewer_index"].as_i64().unwrap_or(0) + 1,
        doc["attempts_used"]
    ));
    for st in doc["steps"].as_array().into_iter().flatten() {
        lines.push(format!(
            "  {:>2} {:<13} {:<5} {}{}{}{}",
            st["index"],
            st["kind"].as_str().unwrap_or("?"),
            if st["ok"] == true { "ok" } else { "FAIL" },
            st["entry_index"].as_i64().map(|e| format!("entry {e} ")).unwrap_or_default(),
            st["run_id"].as_str().map(|r| format!("run {r} ")).unwrap_or_default(),
            st["duration_ms"].as_i64().map(|d| format!("({d} ms)")).unwrap_or_default(),
            st["error"].as_str().map(|e| format!(": {e}")).unwrap_or_default()
        ));
        for sk in st["skipped"].as_array().into_iter().flatten() {
            lines.push(format!(
                "       skipped {}: {}",
                sk["profile"].as_str().unwrap_or("?"),
                sk["reason"].as_str().unwrap_or("")
            ));
        }
        if let Some(w) = st["warning"].as_str() {
            lines.push(format!("       warning: {w}"));
        }
    }
    lines.push(format!(
        "details: thoth pipeline show {task} --json; thoth events --task {task} --json; files in {}",
        doc["dir"].as_str().unwrap_or("-")
    ));
    lines.join("\n")
}

pub fn show(paths: &Paths, cwd: &Path, needle: &str, wait: Option<&str>, json: bool) -> Result<i32> {
    let wait = duration("wait", wait)?;
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;
    let deadline = wait.map(|w| Instant::now() + Duration::from_secs(w));
    loop {
        let Some(row) = latest_for_task(&conn, &task_id)? else {
            bail!("task {task_id} has no pipeline; start one with `thoth pipeline start {task_id} ...`");
        };
        let running = row.status == "running";
        if running && deadline.is_some_and(|d| Instant::now() < d) {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let doc = show_doc(&conn, &row, &ids::now_rfc3339())?;
        output::emit(json, &doc, &show_human(&doc));
        return Ok(if running && deadline.is_some() { 3 } else { 0 });
    }
}

pub fn stop(paths: &Paths, cwd: &Path, needle: &str, json: bool) -> Result<i32> {
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!("pipeline stop refuses to run with THOTH_RUN_ID set: a pipeline is stopped by the commander, not by a run");
    }
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;
    let Some(row) = running_for_task(&conn, &task_id)?.pop() else {
        bail!("task {task_id} has no running pipeline to stop");
    };
    let masker = Masker::load(Some(&paths.home));
    let now = ids::now_rfc3339();
    if is_stale(&row, &now) {
        settle(&conn, &row, &stale_stop(&row), &masker)?;
        let v = json!({"ok": true, "pipeline_id": row.id, "status": "stopped"});
        output::emit(json, &v, &format!("pipeline {} had stopped answering; it is recorded as stopped (stale)", row.id));
        return Ok(0);
    }
    {
        let tx = db::write_tx(&conn)?;
        tx.execute(
            "UPDATE pipelines SET stop_requested = 1 WHERE pipeline_id = ?1 AND status = 'running'",
            [&row.id],
        )?;
        state::append_event(
            &tx,
            &state::NewEvent {
                task_id: Some(task_id.clone()),
                run_id: None,
                event_type: event_type::PIPELINE_STOP_REQUESTED.to_string(),
                detail: json!({"pipeline_id": row.id}),
            },
        )?;
        tx.commit()?;
    }
    append_log(Path::new(&row.dir), "stop requested", &masker);
    let v = json!({"ok": true, "pipeline_id": row.id, "status": "running"});
    output::emit(
        json,
        &v,
        &format!(
            "pipeline {} will stop before its next launch; no worker is cancelled (thoth cancel RUN does \
             that). Follow it with `thoth pipeline show {task_id} --wait 1m`",
            row.id
        ),
    );
    Ok(0)
}

/// `status TASK --json`'s `pipeline`: null, or the newest pipeline in short.
/// Never an error: a table that does not exist, or cannot be read, is null.
pub(crate) fn status_of_task(conn: &Connection, task_id: &str, now: &str) -> Value {
    match latest_for_task(conn, task_id) {
        Ok(Some(row)) => json!({
            "pipeline_id": row.id,
            "status": row.status,
            "step": row.step,
            "stale": is_stale(&row, now),
            "stop_reason": row.stop.as_ref().map(|s| s["reason"].clone()),
            "started_at": row.started_at,
            "ended_at": row.ended_at,
            "implementer_index": row.implementer_index,
            "reviewer_index": row.reviewer_index,
            "attempts_used": attempts_used(conn, task_id),
        }),
        _ => Value::Null,
    }
}

/// `status --json`'s per-task `pipeline`: null or a shorter summary.
pub(crate) fn status_of_task_brief(conn: &Connection, task_id: &str) -> Value {
    match latest_for_task(conn, task_id) {
        Ok(Some(row)) => json!({
            "pipeline_id": row.id,
            "status": row.status,
            "step": row.step,
            "stop_reason": row.stop.as_ref().map(|s| s["reason"].clone()),
        }),
        _ => Value::Null,
    }
}

// --- the ownership gate --------------------------------------------------------------

pub enum Target<'a> {
    Task(&'a str),
    Run(&'a str),
}

/// Refuses `command` while a live, non-stale pipeline runs the task, unless
/// this process is that pipeline's own child (its THOTH_PIPELINE_ID and no
/// THOTH_RUN_ID, so a worker that inherited the id cannot use it). Anything
/// that cannot be resolved here is left to the command's own checks.
pub fn ownership_gate(paths: &Paths, cwd: &Path, target: Target<'_>, command: &str) -> Result<()> {
    let Ok(Ok(root)) = crate::worktree::find_repo_root(cwd) else { return Ok(()) };
    let Ok(repo_id) = ids::repo_id_for(&root) else { return Ok(()) };
    let path = paths.state_db(&repo_id);
    if !path.is_file() {
        return Ok(());
    }
    let Ok(conn) = crate::commands::usage::read_only(&path) else { return Ok(()) };
    if !table_exists(&conn) {
        return Ok(());
    }
    let task_id = match target {
        Target::Task(needle) => state::list_task_ids(&conn)
            .ok()
            .and_then(|ids| ids::resolve_prefix(&ids, needle).ok()),
        Target::Run(needle) => state::list_run_ids(&conn)
            .ok()
            .and_then(|ids| ids::resolve_prefix(&ids, needle).ok())
            .and_then(|run| state::get_run(&conn, &run).ok().flatten())
            .map(|r| r.task_id),
    };
    let Some(task_id) = task_id else { return Ok(()) };
    let now = ids::now_rfc3339();
    let own = std::env::var(PIPELINE_ID_ENV).ok();
    let in_run = std::env::var_os("THOTH_RUN_ID").is_some();
    for row in running_for_task(&conn, &task_id).unwrap_or_default() {
        if is_stale(&row, &now) {
            continue;
        }
        if !in_run && own.as_deref() == Some(row.id.as_str()) {
            continue;
        }
        bail!(
            "refusing {command}: task {task_id} is being run by pipeline {} (step {}); nothing was \
             changed. Stop it first with `thoth pipeline stop {task_id}`, or follow it with `thoth \
             pipeline show {task_id}`",
            row.id,
            row.step
        );
    }
    Ok(())
}

// --- verify slots --------------------------------------------------------------------

mod slots {
    use super::*;

    pub(super) struct Owner<'a> {
        pub pipeline_id: &'a str,
        pub task_id: &'a str,
        pub run_id: &'a str,
    }

    /// Holds one slot file; dropping it releases the slot.
    pub(super) struct Guard {
        path: PathBuf,
        pipeline_id: String,
        pid: u32,
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            let ours = std::fs::read_to_string(&self.path)
                .ok()
                .and_then(|t| serde_json::from_str::<Value>(&t).ok())
                .is_some_and(|v| v["pipeline_id"] == self.pipeline_id.as_str() && v["pid"] == self.pid);
            if ours {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }

    pub(super) enum Outcome {
        Got(Guard),
        TimedOut(Vec<Value>),
        Stopped,
    }

    pub(super) fn dir(home: &Path) -> PathBuf {
        home.join("verify-slots")
    }

    fn limit(home: &Path) -> u64 {
        crate::commands::settings::verify_slots(home)
    }

    /// Every slot file with its owner, after taking away the recoverable
    /// ones: malformed, or whose pid is gone or now runs another program.
    fn live_holders(dir: &Path) -> Vec<Value> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else { return out };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !(name.starts_with("slot-") && name.ends_with(".json")) {
                continue;
            }
            let path = e.path();
            let text = std::fs::read_to_string(&path).ok();
            let owner = text
                .as_deref()
                .and_then(|t| serde_json::from_str::<Value>(t).ok())
                .filter(|v| v["pid"].as_i64().is_some() && v["pipeline_id"].is_string());
            match owner {
                Some(v) if live_thoth(v["pid"].as_i64().unwrap_or(0)) => {
                    let mut v = v;
                    v["file"] = json!(path.display().to_string());
                    out.push(v);
                }
                // Remove only the owner judged dead: the liveness probe takes a
                // while (tasklist on Windows), and another pipeline that
                // recovered this slot meanwhile has linked its own live owner
                // here, which must stay.
                _ if std::fs::read_to_string(&path).ok() == text => {
                    let _ = std::fs::remove_file(&path);
                }
                _ => {}
            }
        }
        out.sort_by(|a, b| a["file"].as_str().cmp(&b["file"].as_str()));
        out
    }

    /// Waits for a slot until `wait` runs out or `stop()` says so.
    pub(super) fn acquire(home: &Path, owner: &Owner<'_>, wait: Duration, stop: &dyn Fn() -> bool) -> Result<Outcome> {
        let dir = dir(home);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let deadline = Instant::now() + wait;
        let pid = std::process::id();
        loop {
            if stop() {
                return Ok(Outcome::Stopped);
            }
            let limit = limit(home);
            let live = live_holders(&dir);
            if (live.len() as u64) < limit {
                for k in 1..=limit {
                    let path = dir.join(format!("slot-{k}.json"));
                    if path.exists() {
                        continue;
                    }
                    let body = json!({
                        "pid": pid,
                        "pipeline_id": owner.pipeline_id,
                        "task_id": owner.task_id,
                        "run_id": owner.run_id,
                        "acquired_at": ids::now_rfc3339(),
                    });
                    // Written whole to a private file, then linked into place:
                    // the link fails when the slot was taken meanwhile, and a
                    // reader never sees a half-written owner.
                    let tmp = dir.join(format!(".slot-{k}.{pid}.tmp"));
                    std::fs::write(&tmp, body.to_string())?;
                    let linked = std::fs::hard_link(&tmp, &path);
                    let _ = std::fs::remove_file(&tmp);
                    if linked.is_ok() {
                        return Ok(Outcome::Got(Guard {
                            path,
                            pipeline_id: owner.pipeline_id.to_string(),
                            pid,
                        }));
                    }
                }
            }
            if Instant::now() >= deadline {
                return Ok(Outcome::TimedOut(live_holders(&dir)));
            }
            std::thread::sleep(Duration::from_millis(300));
        }
    }
}

// --- the background process -----------------------------------------------------------

struct Cli {
    code: Option<i32>,
    stderr: String,
    json: Option<Value>,
}

impl Cli {
    fn error(&self) -> String {
        error_text(self.code, self.json.is_some(), &self.stderr).unwrap_or_else(|| "no error".into())
    }
}

/// stderr's JSON `error`, else its first non-empty line, else a note on the
/// exit code or the unreadable stdout. `None` for a successful command.
fn error_text(code: Option<i32>, parsed: bool, stderr: &str) -> Option<String> {
    if code == Some(0) && parsed {
        return None;
    }
    for line in stderr.lines().rev() {
        if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
            if let Some(e) = v["error"].as_str() {
                return Some(e.to_string());
            }
        }
    }
    if let Some(line) = stderr.lines().map(str::trim).find(|l| !l.is_empty()) {
        return Some(line.strip_prefix("error: ").unwrap_or(line).to_string());
    }
    Some(match code {
        Some(0) => "stdout was not a JSON document".to_string(),
        Some(c) => format!("exit code {c}"),
        None => "ended without an exit code".to_string(),
    })
}

struct StepOut {
    ok: bool,
    json: Option<Value>,
    error: Option<String>,
    /// stderr's `--json` error document (dispatch's `stuck` lives there).
    err_doc: Option<Value>,
}

struct Bg {
    paths: Paths,
    root: PathBuf,
    conn: Connection,
    id: String,
    task_id: String,
    dir: PathBuf,
    settings: Settings,
    masker: Masker,
    exe: PathBuf,
    step_index: i64,
    current_step: String,
    verify_pid: Arc<AtomicU32>,
    verify_killed: Arc<AtomicBool>,
    /// AUTO-1b: zero-based list positions; they only move forward.
    impl_index: usize,
    rev_index: usize,
    /// The attempt being worked on, as far as the pipeline knows.
    attempt: Option<i64>,
    budgets: HashMap<i64, Budget>,
    impl_history: Vec<Value>,
    rev_history: Vec<Value>,
}

#[cfg(windows)]
fn below_normal(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Threading::{SetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS};
    // SAFETY: the handle belongs to `child`, which is alive for this call.
    unsafe {
        SetPriorityClass(child.as_raw_handle() as _, BELOW_NORMAL_PRIORITY_CLASS);
    }
}

#[cfg(not(windows))]
fn below_normal(_child: &std::process::Child) {}

pub fn run_background(pipeline_id: &str) -> Result<i32> {
    let paths = Paths::from_env()?;
    let cwd = std::env::current_dir()?;
    let (repo_id, root, conn) = crate::commands::task::open_repo(&paths, &cwd)?;
    ensure_table(&conn)?;
    let row = get_row(&conn, pipeline_id)?.ok_or_else(|| anyhow!("no pipeline {pipeline_id}"))?;
    if row.status != "running" {
        return Ok(0);
    }
    conn.execute(
        "UPDATE pipelines SET pipeline_pid = ?1 WHERE pipeline_id = ?2",
        params![std::process::id() as i64, pipeline_id],
    )?;
    let masker = Masker::load(Some(&paths.home));
    let settings: Settings = serde_json::from_value(row.settings.clone())
        .map_err(|e| anyhow!("pipeline {pipeline_id}'s settings cannot be read: {e}"))?;
    let verify_pid = Arc::new(AtomicU32::new(0));
    let verify_killed = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let heartbeat = {
        let (paths, repo_id, id) = (paths.clone(), repo_id.clone(), row.id.clone());
        let (pid, killed, done) = (verify_pid.clone(), verify_killed.clone(), done.clone());
        std::thread::spawn(move || heartbeat_loop(paths, repo_id, id, pid, killed, done))
    };
    let mut bg = Bg {
        paths,
        root,
        conn,
        id: row.id.clone(),
        task_id: row.task_id.clone(),
        dir: PathBuf::from(&row.dir),
        settings,
        masker,
        exe: std::env::current_exe()?,
        step_index: 0,
        current_step: row.step.clone(),
        verify_pid,
        verify_killed,
        impl_index: 0,
        rev_index: 0,
        attempt: None,
        budgets: HashMap::new(),
        impl_history: Vec::new(),
        rev_history: Vec::new(),
    };
    let _ = std::fs::create_dir_all(&bg.dir);
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| bg.drive()));
    let stop = match outcome {
        Ok(Ok(stop)) => stop,
        Ok(Err(e)) => bg.error_stop(&format!("{e:#}")),
        Err(_) => bg.error_stop("the background process panicked"),
    };
    done.store(true, Ordering::SeqCst);
    let _ = heartbeat.join();
    let row = get_row(&bg.conn, &bg.id)?.unwrap_or(row);
    settle(&bg.conn, &row, &stop, &bg.masker)?;
    Ok(0)
}

fn heartbeat_loop(
    paths: Paths,
    repo_id: String,
    id: String,
    verify_pid: Arc<AtomicU32>,
    killed: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
) {
    let Ok(conn) = db::open_state(&paths, &repo_id) else { return };
    let mut last: Option<Instant> = None;
    while !done.load(Ordering::SeqCst) {
        if last.is_none_or(|l| l.elapsed() >= HEARTBEAT_EVERY) {
            let _ = conn.execute(
                "UPDATE pipelines SET heartbeat_at = ?1 WHERE pipeline_id = ?2 AND status = 'running'",
                params![ids::now_rfc3339(), id],
            );
            last = Some(Instant::now());
        }
        let stop = conn
            .query_row("SELECT stop_requested FROM pipelines WHERE pipeline_id = ?1", [&id], |r| r.get::<_, i64>(0))
            .unwrap_or(0)
            != 0;
        let pid = verify_pid.load(Ordering::SeqCst);
        if stop && pid != 0 && !killed.swap(true, Ordering::SeqCst) {
            let _ = crate::process::controller().kill_tree(pid);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// What verify's result says.
enum Verified {
    Green,
    Red(Value),
    Blocked(String),
}

const RETRYABLE_ERRORS: [&str; 3] = ["timed out", "output may be incomplete", "terminated without"];

fn classify(out: &StepOut) -> Verified {
    let Some(doc) = &out.json else {
        return Verified::Blocked(format!(
            "verify gave no readable result: {}",
            out.error.as_deref().unwrap_or("no output")
        ));
    };
    if doc["reason"] == "no_approved_checks" {
        return Verified::Blocked(
            "verify found no approved check to run; add one with `thoth checks add ID --cmd ...` and \
             approve it with `thoth checks approve --reason TEXT`"
                .into(),
        );
    }
    let Some(results) = doc["results"].as_array() else {
        return Verified::Blocked("verify's result has no results list".into());
    };
    if results.is_empty() {
        return Verified::Blocked("verify ran no check".into());
    }
    let mut blocked = Vec::new();
    let mut red = Vec::new();
    for r in results {
        let exit = r["exit_code"].as_i64();
        let err = r["error"].as_str();
        if exit == Some(0) && err.is_none() {
            continue;
        }
        let retryable = err.is_some_and(|e| RETRYABLE_ERRORS.iter().any(|p| e.starts_with(p)));
        let entry = json!({
            "check_id": r["check_id"],
            "evidence": r["evidence"],
            "exit_code": r["exit_code"],
            "error": r["error"],
        });
        if exit.is_none() && !retryable {
            blocked.push(format!(
                "check {}: {}",
                r["check_id"].as_str().unwrap_or("?"),
                err.unwrap_or("refused")
            ));
        } else {
            red.push(entry);
        }
    }
    if !blocked.is_empty() {
        return Verified::Blocked(blocked.join("; "));
    }
    if !red.is_empty() {
        return Verified::Red(Value::Array(red));
    }
    Verified::Green
}

fn find_run<'a>(status: &'a Value, run_id: &str) -> Option<&'a Value> {
    status["runs"].as_array()?.iter().find(|r| r["run_id"] == run_id)
}

fn brief_lead(brief: &Value, rest: String) -> String {
    match brief.as_str().filter(|b| !b.trim().is_empty()) {
        Some(b) => format!("{} | {rest}", b.trim()),
        None => rest,
    }
}

// --- AUTO-1b: continuation helpers ------------------------------------------------------

/// What one recorded child command printed.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    duration_ms: i64,
}

/// The AUTO-1b fields a step carries besides AUTO-1a's.
#[derive(Default)]
struct StepMeta {
    /// One-based list position (dispatch, review_start, resume).
    entry_index: Option<usize>,
    skipped: Vec<Value>,
    attempt: Option<i64>,
    /// Only a resume: the commander's reason and the recovery cause.
    reason: Option<String>,
    /// Facts a preflight could not read; execution was not held back.
    warning: Option<String>,
    /// Replaces the kind's default `result`.
    result: Option<Value>,
}

/// Recovery budgets of one attempt: one reroute after an unverified
/// delivery, one crash/transient recovery, one doubled-timeout retry.
#[derive(Default, Clone)]
struct Budget {
    rerouted: bool,
    crash_recovered: bool,
    timeout_retried: bool,
}

/// What sent the pipeline into a list search (a REJECT, a stopped run): an
/// exhaustion stop keeps it next to the entry history, so it still says what
/// the stop it replaces would have said.
#[derive(Default, Clone)]
struct Cause {
    /// The brief the stop leads with.
    brief: Value,
    /// One clause: what happened.
    what: String,
    /// Keys merged into `stop.evidence`.
    evidence: serde_json::Map<String, Value>,
    /// Commands listed before the exhaustion stop's own.
    next: Vec<String>,
}

/// One search over the implementer list.
#[derive(Default)]
struct ImplPlan {
    start: usize,
    context: Option<String>,
    timeout: Option<u64>,
    allow_attempt: Option<i64>,
    cause: Option<Cause>,
}

/// The retries a reviewer entry has used.
#[derive(Default, Clone)]
struct RevRetry {
    pos: Option<usize>,
    crash: bool,
    timeout: bool,
}

/// One search over the reviewer list.
#[derive(Default)]
struct RevPlan {
    start: usize,
    context: Option<String>,
    timeout: Option<u64>,
    retry: RevRetry,
    cause: Option<Cause>,
}

/// Whether a preflight let an entry through, and why not.
struct Usable {
    ok: bool,
    why: Option<String>,
}

fn is_auth_refusal(err: &str) -> bool {
    err.contains("failed its auth check")
}

/// SMALL-9a's launch refusal (`refusing to launch on profile ID:
/// quota-reserve-percent N holds it back ...`).
fn is_reserve_refusal(err: &str) -> bool {
    err.contains("refusing to launch on profile") && err.contains("quota-reserve-percent")
}

/// The last stderr line that is a JSON object (the `--json` error document).
fn stderr_doc(stderr: &str) -> Option<Value> {
    stderr
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
        .find(Value::is_object)
}

/// AC8: the reviewer entry repeats the implementer run's model and effort
/// (its requested or its lowered effort; no effort on both sides matches).
fn same_choice(e: &Entry, run: &Value) -> bool {
    let model = run["model"].as_str().unwrap_or("");
    if !model.trim().eq_ignore_ascii_case(e.model.trim()) {
        return false;
    }
    let norm = |v: &Value| v.as_str().map(|s| s.trim().to_lowercase());
    let mine = e.effort.as_ref().map(|s| s.trim().to_lowercase());
    mine == norm(&run["effort_requested"]) || mine == norm(&run["effort"])
}

fn newest_run<'a>(status: &'a Value, role_name: &str) -> Option<&'a Value> {
    status["runs"]
        .as_array()?
        .iter()
        .filter(|r| r["role"] == role_name)
        .max_by(|a, b| a["run_id"].as_str().cmp(&b["run_id"].as_str()))
}

fn unmet_ids(findings: &Value) -> Vec<String> {
    findings["criteria"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|c| c["verdict"].as_str().map(str::trim) == Some("unmet"))
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect()
}

fn doubled(timeout_seconds: Option<i64>) -> u64 {
    let base = timeout_seconds.filter(|t| *t > 0).unwrap_or(3600) as u64;
    base.saturating_mul(2).min(crate::cli::MAX_DURATION_SECS)
}

impl Bg {
    fn log(&self, line: &str) {
        append_log(&self.dir, line, &self.masker);
    }

    fn error_stop(&self, what: &str) -> Stop {
        self.log(&format!("error: {what}"));
        Stop::new(
            "error",
            format!("the pipeline stopped on an unexpected error at step {}: {what}", self.current_step),
        )
        .next(format!("thoth pipeline show {} --json", self.task_id))
        .next(format!("read {}", self.dir.join("pipeline.log").display()))
        .evidence(json!({"step": self.current_step, "dir": self.dir.display().to_string()}))
    }

    fn set_step(&mut self, step: &str) -> Result<()> {
        self.current_step = step.to_string();
        self.conn.execute(
            "UPDATE pipelines SET step = ?1 WHERE pipeline_id = ?2 AND status = 'running'",
            params![step, self.id],
        )?;
        self.log(&format!("step {step}"));
        if std::env::var(FAIL_AT_SEAM).ok().as_deref() == Some(step) {
            bail!("injected failure at step {step} ({FAIL_AT_SEAM})");
        }
        Ok(())
    }

    fn set_impl_index(&mut self, pos: usize) -> Result<()> {
        if pos >= self.impl_index {
            self.impl_index = pos;
            self.conn.execute(
                "UPDATE pipelines SET implementer_index = ?1 WHERE pipeline_id = ?2",
                params![pos as i64, self.id],
            )?;
        }
        Ok(())
    }

    fn set_rev_index(&mut self, pos: usize) -> Result<()> {
        if pos >= self.rev_index {
            self.rev_index = pos;
            self.conn.execute(
                "UPDATE pipelines SET reviewer_index = ?1 WHERE pipeline_id = ?2",
                params![pos as i64, self.id],
            )?;
        }
        Ok(())
    }

    fn budget(&mut self, attempt: i64) -> &mut Budget {
        self.budgets.entry(attempt).or_default()
    }

    fn attempts_used(&self) -> i64 {
        state::max_attempt(&self.conn, &self.task_id).ok().flatten().unwrap_or(0)
    }

    /// Records what became of one list entry, for the exhaustion stops.
    fn note(&mut self, reviewer: bool, pos: usize, outcome: String, run: Option<&str>) {
        let e = if reviewer { &self.settings.reviewers[pos] } else { &self.settings.implementers[pos] };
        let v = json!({
            "entry_index": pos + 1,
            "profile": e.profile,
            "model": e.model,
            "effort": e.effort,
            "outcome": outcome,
            "run_id": run,
        });
        self.log(&format!(
            "{} entry {} ({}): {}",
            if reviewer { "reviewer" } else { "implementer" },
            pos + 1,
            e.text(),
            v["outcome"].as_str().unwrap_or("")
        ));
        if reviewer {
            self.rev_history.push(v);
        } else {
            self.impl_history.push(v);
        }
    }

    fn stop_requested(&self) -> Result<bool> {
        Ok(self.conn.query_row(
            "SELECT stop_requested FROM pipelines WHERE pipeline_id = ?1",
            [&self.id],
            |r| r.get::<_, i64>(0),
        )? != 0)
    }

    fn stopped(&self, run: Option<&str>) -> Stop {
        let mut summary = format!(
            "stopped on request at step {}; no worker was cancelled",
            self.current_step
        );
        let mut stop = Stop::new("stopped", String::new());
        if let Some(run) = run {
            summary.push_str(&format!("; run {run} keeps going on its own"));
            stop = stop.next(format!("thoth wait --run {run}")).next(format!("thoth cancel {run} (only to end it)"));
        }
        stop.summary = summary;
        stop.next(format!("thoth status {}", self.task_id))
            .evidence(json!({"step": self.current_step, "run_id": run}))
    }

    fn command(&self, args: &[String]) -> std::process::Command {
        let mut c = std::process::Command::new(&self.exe);
        c.args(args)
            .current_dir(&self.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env(PIPELINE_ID_ENV, &self.id)
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_VIA_MCP");
        crate::process::quiet(&mut c);
        c
    }

    /// A read-only observation (status, wait, review show, events): not a step.
    fn cli(&self, args: &[&str]) -> Result<Cli> {
        let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        let out = self
            .command(&args)
            .output()
            .with_context(|| format!("running thoth {}", args.join(" ")))?;
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        Ok(Cli {
            code: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            json: serde_json::from_str::<Value>(stdout.trim()).ok().filter(Value::is_object),
        })
    }

    fn status(&self) -> Result<Value> {
        let c = self.cli(&["status", &self.task_id, "--json"])?;
        c.json.clone().filter(|_| c.code == Some(0)).ok_or_else(|| anyhow!("thoth status {} --json failed: {}", self.task_id, c.error()))
    }

    fn review_show(&self) -> Result<Value> {
        let c = self.cli(&["review", "show", &self.task_id, "--json"])?;
        c.json.clone().filter(|_| c.code == Some(0)).ok_or_else(|| anyhow!("thoth review show {} --json failed: {}", self.task_id, c.error()))
    }

    /// `pipeline ID step N: REASON` for a first choice without context (as
    /// AUTO-1a), otherwise `pipeline ID step N: CONTEXT; ROLE entry K of L:
    /// REASON`.
    fn reason_for(&self, index: i64, context: Option<&str>, role_name: &str, pos: usize, total: usize) -> String {
        let mut parts: Vec<String> = Vec::new();
        if let Some(c) = context {
            parts.push(c.to_string());
        }
        if pos != 0 || context.is_some() {
            parts.push(format!("{role_name} entry {} of {total}", pos + 1));
        }
        if parts.is_empty() {
            format!("pipeline {} step {index}: {}", self.id, self.settings.reason)
        } else {
            format!("pipeline {} step {index}: {}: {}", self.id, parts.join("; "), self.settings.reason)
        }
    }

    fn run_verify(&self, args: &[String], index: i64) -> Result<(Option<i32>, String, String)> {
        let raw = self.dir.join(format!("step-{index:03}.stderr.raw"));
        let env = vec![(PIPELINE_ID_ENV.to_string(), self.id.clone())];
        let remove = vec!["THOTH_RUN_ID".to_string(), "THOTH_VIA_MCP".to_string()];
        let exe = self.exe.to_string_lossy().to_string();
        let mut child = crate::process::controller().spawn_child(crate::process::ChildSpec {
            program: &exe,
            args,
            cwd: &self.root,
            env: &env,
            env_remove: &remove,
            stderr_path: Some(&raw),
            stdin: None,
        })?;
        let pid = child.id();
        below_normal(&child);
        self.verify_killed.store(false, Ordering::SeqCst);
        self.verify_pid.store(pid, Ordering::SeqCst);
        let mut bytes = Vec::new();
        if let Some(mut out) = child.stdout.take() {
            let _ = out.read_to_end(&mut bytes);
        }
        let status = child.wait();
        self.verify_pid.store(0, Ordering::SeqCst);
        crate::process::release_job(pid);
        let stderr = std::fs::read(&raw).map(|b| String::from_utf8_lossy(&b).to_string()).unwrap_or_default();
        let _ = std::fs::remove_file(&raw);
        Ok((status?.code(), String::from_utf8_lossy(&bytes).to_string(), stderr))
    }

    fn begin_step(&mut self) -> i64 {
        self.step_index += 1;
        self.step_index
    }

    fn run_step(&self, kind: &str, args: &[String], index: i64) -> Ran {
        let started = Instant::now();
        let ran: Result<(Option<i32>, String, String)> = if kind == "verify" {
            self.run_verify(args, index)
        } else {
            self.command(args)
                .output()
                .map(|o| {
                    (
                        o.status.code(),
                        String::from_utf8_lossy(&o.stdout).to_string(),
                        String::from_utf8_lossy(&o.stderr).to_string(),
                    )
                })
                .map_err(anyhow::Error::from)
        };
        let duration_ms = started.elapsed().as_millis() as i64;
        let (code, stdout, stderr) = match ran {
            Ok(r) => r,
            Err(e) => (None, String::new(), format!("could not run thoth {}: {e:#}", args.first().map(String::as_str).unwrap_or(""))),
        };
        Ran { code, stdout, stderr, duration_ms }
    }

    /// Keeps a step's output in the pipeline folder and summarises it in a
    /// `pipeline_step` event.
    fn finish_step(
        &mut self,
        index: i64,
        kind: &str,
        args: &[String],
        ran: &Ran,
        waited_ms: Option<i64>,
        meta: StepMeta,
    ) -> Result<StepOut> {
        let _ = std::fs::write(self.dir.join(format!("step-{index:03}.stdout.json")), self.masker.mask_text(&ran.stdout).0);
        let _ = std::fs::write(self.dir.join(format!("step-{index:03}.stderr.txt")), self.masker.mask_text(&ran.stderr).0);
        let parsed = serde_json::from_str::<Value>(ran.stdout.trim()).ok().filter(Value::is_object);
        let ok = ran.code == Some(0) && parsed.is_some();
        let error = error_text(ran.code, parsed.is_some(), &ran.stderr);
        let d = parsed.as_ref();
        let run_id: Value = match kind {
            "verify" | "resume" => json!(args.get(1)),
            _ => d.map(|v| v["run_id"].clone()).filter(Value::is_string).unwrap_or(Value::Null),
        };
        let result = match meta.result {
            Some(r) => Some(r),
            None => d.map(|v| match kind {
                "dispatch" => json!({"attempt": v["attempt"], "rework": v["rework"], "folded_findings": v["folded_findings"], "handoff": v["handoff"]}),
                "review_start" => json!({"target_run_id": v["target_run_id"], "attempt": v["attempt"], "mode": v["mode"]}),
                "resume" => json!({"run_id": v["run_id"], "warnings": v["warnings"]}),
                _ => json!({"ok": v["ok"], "failures": v["failures"], "missing_since_last_verify": v["missing_since_last_verify"]}),
            }),
        };
        let attempt = meta
            .attempt
            .or_else(|| match kind {
                "dispatch" | "review_start" => d.and_then(|v| v["attempt"].as_i64()),
                _ => None,
            })
            .or(self.attempt);
        let mut detail = json!({
            "pipeline_id": self.id,
            "index": index,
            "kind": kind,
            "argv": args,
            "exit_code": ran.code,
            "duration_ms": ran.duration_ms,
            "ok": ok,
            "run_id": run_id,
            "error": error,
            "stdout_bytes": ran.stdout.len(),
            "waited_for_slot_ms": waited_ms,
            "result": result,
            "entry_index": meta.entry_index,
            "skipped": meta.skipped,
            "attempt": attempt,
            "reason": meta.reason,
            "warning": meta.warning,
            "implementer_index": self.impl_index,
            "reviewer_index": self.rev_index,
            "attempts_used": self.attempts_used(),
        });
        self.masker.mask_value(&mut detail);
        state::append_event(
            &self.conn,
            &state::NewEvent {
                task_id: Some(self.task_id.clone()),
                run_id: run_id.as_str().map(str::to_string),
                event_type: event_type::PIPELINE_STEP.to_string(),
                detail,
            },
        )?;
        self.log(&format!(
            "step {index} {kind}: {}{}{}{}",
            if ok { "ok" } else { "failed" },
            run_id.as_str().map(|r| format!(" run {r}")).unwrap_or_default(),
            error.as_deref().map(|e| format!(" - {e}")).unwrap_or_default(),
            meta.warning.as_deref().map(|w| format!(" (warning: {w})")).unwrap_or_default()
        ));
        Ok(StepOut { ok, json: parsed, error, err_doc: stderr_doc(&ran.stderr) })
    }

    fn step(&mut self, kind: &str, args: Vec<String>, waited_ms: Option<i64>) -> Result<StepOut> {
        let index = self.begin_step();
        let ran = self.run_step(kind, &args, index);
        self.finish_step(index, kind, &args, &ran, waited_ms, StepMeta::default())
    }

    // --- AC2/AC3: preflight ----------------------------------------------------------------

    /// Reads `recommend` (and, with a quota threshold, `profile usage`) for
    /// one entry and says whether it may be launched. Facts that cannot be
    /// read never hold an entry back; the launch's own checks still apply.
    fn preflight(&mut self, role_name: &str, e: &Entry, pos: usize) -> Result<Usable> {
        let args: Vec<String> = [
            "recommend", "--role", role_name, "--task", &self.task_id, "--model", &e.model, "--json",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut args = args;
        preflight_seam("recommend", &mut args);
        let index = self.begin_step();
        let ran = self.run_step("recommend", &args, index);
        let parsed = serde_json::from_str::<Value>(ran.stdout.trim()).ok().filter(Value::is_object);
        let readable = ran.code == Some(0) && parsed.is_some();
        let mut warnings: Vec<String> = Vec::new();
        let candidate = parsed
            .as_ref()
            .filter(|_| readable)
            .and_then(|d| d["candidates"].as_array())
            .and_then(|c| c.iter().find(|c| c["profile_id"] == e.profile.as_str()))
            .cloned();
        let field = |k: &str| candidate.as_ref().map(|c| c[k].clone()).unwrap_or(Value::Null);
        let result = json!({
            "role": role_name,
            "entry": pos + 1,
            "profile_id": e.profile,
            "eligible": field("eligible"),
            "reasons": field("reasons"),
            "quota_state": field("quota_state"),
            "quota_group": field("quota_group"),
            "peak_percent": field("peak_percent"),
            "reserve_blocked": field("reserve_blocked"),
            "quota_reserve_percent": parsed.as_ref().filter(|_| readable).map(|d| d["quota_reserve_percent"].clone()).unwrap_or(Value::Null),
        });
        if !readable {
            warnings.push(format!(
                "recommend could not be read ({}), so the eligibility, quota and reserve of profile {} are unknown; execution was not held back",
                error_text(ran.code, parsed.is_some(), &ran.stderr).unwrap_or_else(|| "no output".into()),
                e.profile
            ));
        } else if candidate.is_none() {
            warnings.push(format!(
                "recommend lists no candidate for profile {}, so its eligibility, quota and reserve are unknown; execution was not held back",
                e.profile
            ));
        } else if !result["eligible"].is_boolean() {
            warnings.push(format!(
                "recommend's candidate {} has no readable eligible field; execution was not held back",
                e.profile
            ));
        }
        // A readable candidate that recommend does not disqualify, but whose
        // quota it does not know, is still a fact worth saying: the quota is
        // unknown, not zero, and that alone holds nothing back.
        if candidate.is_some() && result["eligible"] != false {
            match result["quota_state"].as_str() {
                Some("unknown") => warnings.push(format!(
                    "recommend reports quota_state unknown for profile {}, so its quota is unknown, not zero; execution was not held back for that",
                    e.profile
                )),
                Some(_) => {}
                None => warnings.push(format!(
                    "recommend's candidate {} has no readable quota_state, so its quota is unknown, not zero; execution was not held back for that",
                    e.profile
                )),
            }
        }
        let reasons: Vec<String> = result["reasons"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(str::to_string))
            .collect();
        let mut why: Option<String> = None;
        if result["eligible"] == false {
            why = Some(format!(
                "not eligible in recommend: {}",
                if reasons.is_empty() { "no reason given".to_string() } else { reasons.join("; ") }
            ));
        }
        let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
        // The candidate's quota group as recommend gave it: `Ok(None)` is an
        // explicit null (an ungrouped account: every window decides), `Err`
        // says why it could not be read (then no window decides).
        let group: std::result::Result<Option<String>, &str> = if !readable {
            Err("recommend could not be read")
        } else {
            match candidate.as_ref().map(|c| c.get("quota_group")) {
                None => Err("recommend lists no candidate for it"),
                Some(Some(Value::Null)) => Ok(None),
                Some(Some(Value::String(g))) => Ok(Some(g.clone())),
                Some(_) => Err("its candidate's quota_group field is missing or not a string"),
            }
        };
        let copied = (result["eligible"].clone(), result["reasons"].clone());
        self.finish_step(
            index,
            "recommend",
            &args,
            &ran,
            None,
            StepMeta { result: Some(result), warning, ..Default::default() },
        )?;

        if let Some(threshold) = self.settings.quota_threshold {
            let mut args: Vec<String> =
                ["profile", "usage", &e.profile, "--json"].iter().map(|s| s.to_string()).collect();
            preflight_seam("usage", &mut args);
            let index = self.begin_step();
            let ran = self.run_step("profile_usage", &args, index);
            let parsed = serde_json::from_str::<Value>(ran.stdout.trim()).ok().filter(Value::is_object);
            let readable = ran.code == Some(0) && parsed.is_some();
            let mut warnings: Vec<String> = Vec::new();
            let profile = parsed
                .as_ref()
                .filter(|_| readable)
                .and_then(|d| d["profiles"].as_array())
                .and_then(|p| p.iter().find(|p| p["profile_id"] == e.profile.as_str()))
                .cloned();
            let mut deciding: Option<(String, f64, Value)> = None;
            if !readable {
                warnings.push(format!(
                    "profile usage could not be read ({}), so the quota of profile {} is unknown; execution was not held back",
                    error_text(ran.code, parsed.is_some(), &ran.stderr).unwrap_or_else(|| "no output".into()),
                    e.profile
                ));
            } else if let (Some(_), Err(why_unknown)) = (&profile, &group) {
                // Which windows apply depends on the group; an unknown group
                // is not an ungrouped account, so no window decides.
                warnings.push(format!(
                    "the quota_group of profile {} for model {} is unknown ({why_unknown}), so which of its windows decides is unknown and quota-threshold {threshold} held nothing back; execution was not held back",
                    e.profile, e.model
                ));
            } else if let (Some(p), Ok(group)) = (&profile, &group) {
                match p["windows"].as_array() {
                    None => warnings.push(format!(
                        "profile usage has no readable windows for profile {}; its quota is unknown; execution was not held back",
                        e.profile
                    )),
                    Some(windows) => {
                        let mut malformed = 0usize;
                        for w in windows {
                            if w["reset_since_observed"] == true {
                                continue;
                            }
                            if let Some(g) = group {
                                if w["group"].as_str() != Some(g.as_str()) {
                                    continue;
                                }
                            }
                            match (w["name"].as_str(), w["used_percent"].as_f64()) {
                                (Some(name), Some(used)) => {
                                    if deciding.as_ref().is_none_or(|d| used > d.1) {
                                        deciding = Some((name.to_string(), used, w["group"].clone()));
                                    }
                                }
                                _ => malformed += 1,
                            }
                        }
                        if malformed > 0 {
                            warnings.push(format!(
                                "{malformed} window(s) of profile {} have no readable name or used_percent and were left out",
                                e.profile
                            ));
                        }
                        if deciding.is_none() {
                            warnings.push(format!(
                                "profile usage has no current (not reset) quota window for profile {}{}; its quota is unknown, not zero; execution was not held back",
                                e.profile,
                                group.as_deref().map(|g| format!(" in group {g}")).unwrap_or_default()
                            ));
                        }
                    }
                }
            } else {
                warnings.push(format!(
                    "profile usage lists no profile {}; its quota is unknown; execution was not held back",
                    e.profile
                ));
            }
            let held_back = deciding.as_ref().map(|d| d.1 >= threshold as f64);
            let pfield = |k: &str| profile.as_ref().map(|p| p[k].clone()).unwrap_or(Value::Null);
            let result = json!({
                "role": role_name,
                "entry": pos + 1,
                "profile_id": e.profile,
                "eligible": copied.0,
                "reasons": copied.1,
                "quota_state": pfield("quota_state"),
                "quota_group": group.as_ref().ok().cloned().flatten(),
                "quota_group_known": group.is_ok(),
                "peak_percent": pfield("peak_percent"),
                "threshold": threshold,
                "window": deciding.as_ref().map(|d| json!({"name": d.0, "used_percent": d.1, "group": d.2})),
                "held_back": held_back,
            });
            if let (Some(true), Some((name, used, g))) = (held_back, &deciding) {
                let text = format!(
                    "quota-threshold {threshold}: window {name}{} is {}% used, at or above {threshold}%",
                    g.as_str().map(|g| format!(" ({g})")).unwrap_or_default(),
                    fmt_percent(*used)
                );
                why = Some(match why {
                    Some(w) => format!("{w}; {text}"),
                    None => text,
                });
            }
            let warning = (!warnings.is_empty()).then(|| warnings.join("; "));
            self.finish_step(
                index,
                "profile_usage",
                &args,
                &ran,
                None,
                StepMeta { result: Some(result), warning, ..Default::default() },
            )?;
        }
        Ok(Usable { ok: why.is_none(), why })
    }

    // --- stops ---------------------------------------------------------------------------

    fn attempts_exhausted(&self, attempt: i64, review: Option<&Value>) -> Stop {
        let mut summary = format!(
            "task {} is at attempt {attempt}, above this pipeline's max-attempts {}; nothing more is launched",
            self.task_id, self.settings.max_attempts
        );
        let mut evidence = json!({"attempt": attempt, "max_attempts": self.settings.max_attempts});
        if let Some(rs) = review {
            let unmet = unmet_ids(&rs["findings"]);
            summary = brief_lead(
                &rs["brief"],
                format!(
                    "REJECT {} would need attempt {attempt}, above this pipeline's max-attempts {}; unmet: {}; nothing more is launched",
                    rs["review_id"].as_str().unwrap_or("?"),
                    self.settings.max_attempts,
                    if unmet.is_empty() { "none listed".to_string() } else { unmet.join(", ") }
                ),
            );
            evidence["review_id"] = rs["review_id"].clone();
            evidence["unmet"] = json!(unmet);
            evidence["brief"] = rs["brief"].clone();
        }
        let mut stop = Stop::new("attempts_exhausted", summary).next(format!("thoth status {}", self.task_id));
        if review.is_some() {
            stop = stop.next(format!("thoth review show {} --json", self.task_id));
        }
        stop.evidence(evidence)
    }

    /// A run that stopped (interrupted or failed): what AUTO-1a's
    /// `run_interrupted` / `run_failed` stop kept about it.
    fn run_cause(&self, run: &str, r: &Value, brief: &Value) -> Cause {
        let failed = r["status"] == "failed";
        let mut evidence = serde_json::Map::new();
        evidence.insert("run_id".into(), json!(run));
        for key in ["interrupted_reason", "error_text", "next_step"] {
            evidence.insert(key.into(), r[key].clone());
        }
        Cause {
            brief: brief.clone(),
            what: format!(
                "run {run} {} ({}){}",
                if failed { "failed" } else { "was interrupted" },
                r["interrupted_reason"].as_str().unwrap_or("no reason recorded"),
                r["error_text"].as_str().map(|e| format!(": {e}")).unwrap_or_default()
            ),
            evidence,
            next: r["next_step"].as_str().map(|n| vec![n.to_string()]).unwrap_or_default(),
        }
    }

    /// A REJECT: what AUTO-1a's `reject` stop kept, plus the unmet criteria.
    fn reject_cause(&self, rs: &Value, attempt: i64) -> Cause {
        let unmet = unmet_ids(&rs["findings"]);
        let mut evidence = serde_json::Map::new();
        for key in ["reviewer_run_id", "review_id", "verdict", "brief"] {
            evidence.insert(key.into(), rs[key].clone());
        }
        evidence.insert("unmet".into(), json!(unmet));
        Cause {
            brief: rs["brief"].clone(),
            what: format!(
                "REJECT {} (attempt {attempt}; unmet: {})",
                rs["review_id"].as_str().unwrap_or("?"),
                if unmet.is_empty() { "none listed".to_string() } else { unmet.join(", ") }
            ),
            evidence,
            next: vec![format!("thoth review show {} --json", self.task_id)],
        }
    }

    /// An implementer delivery left `unverified`.
    fn unverified_cause(&self, run: &str, brief: &Value) -> Cause {
        let mut evidence = serde_json::Map::new();
        evidence.insert("run_id".into(), json!(run));
        Cause {
            brief: brief.clone(),
            what: format!("the delivery of run {run} was unverified"),
            evidence,
            next: vec![format!("thoth task result {} --run {run}", self.task_id)],
        }
    }

    /// A reviewer run that ended without a verdict: what AUTO-1a's
    /// `review_interrupted` stop kept about it.
    fn reviewer_cause(&self, run: &str, r: &Value) -> Cause {
        let mut evidence = serde_json::Map::new();
        evidence.insert("reviewer_run_id".into(), json!(run));
        for key in ["status", "interrupted_reason", "error_text"] {
            evidence.insert(key.into(), r[key].clone());
        }
        Cause {
            brief: Value::Null,
            what: format!(
                "reviewer run {run} {} ({}){}",
                if r["status"] == "failed" { "failed" } else { "was interrupted" },
                r["interrupted_reason"].as_str().unwrap_or("no reason recorded"),
                r["error_text"].as_str().map(|e| format!(": {e}")).unwrap_or_default()
            ),
            evidence,
            next: Vec::new(),
        }
    }

    fn exhausted(&self, reviewer: bool, start: usize, why: &str, cause: Option<&Cause>) -> Stop {
        let (reason, label, total, hist) = if reviewer {
            ("reviewers_exhausted", "reviewer", self.settings.reviewers.len(), &self.rev_history)
        } else {
            ("implementers_exhausted", "implementer", self.settings.implementers.len(), &self.impl_history)
        };
        let listed: Vec<String> = hist
            .iter()
            .map(|h| {
                format!(
                    "entry {} {}:{}{}: {}",
                    h["entry_index"],
                    h["profile"].as_str().unwrap_or("?"),
                    h["model"].as_str().unwrap_or("?"),
                    h["effort"].as_str().map(|e| format!(":{e}")).unwrap_or_default(),
                    h["outcome"].as_str().unwrap_or("")
                )
            })
            .collect();
        let searched = if start >= total {
            format!("the list has no entry after entry {start} of {total}")
        } else {
            format!("searched entries {} to {total} of {total}", start + 1)
        };
        let rest = format!(
            "{}no usable {label} entry is left ({why}): {searched}; nothing was waited for. {}",
            cause.map(|c| format!("{}; ", c.what)).unwrap_or_default(),
            if listed.is_empty() { "No entry was tried.".to_string() } else { listed.join("; ") }
        );
        let mut stop = Stop::new(reason, brief_lead(cause.map(|c| &c.brief).unwrap_or(&Value::Null), rest));
        for n in cause.map(|c| c.next.as_slice()).unwrap_or_default() {
            stop = stop.next(n.clone());
        }
        let mut evidence = cause.map(|c| c.evidence.clone()).unwrap_or_default();
        evidence.insert("start_entry".into(), json!(start + 1));
        evidence.insert("entries".into(), json!(hist));
        stop.next(format!("thoth pipeline show {} --json", self.task_id))
            .next(format!("thoth recommend --role {label} --task {} --json", self.task_id))
            .next(format!("thoth pipeline start {} ... (with more or other --{label} entries)", self.task_id))
            .evidence(Value::Object(evidence))
    }

    fn run_interrupted_stop(&self, run: &str, r: &Value, brief: &Value) -> Stop {
        let mut stop = Stop::new(
            "run_interrupted",
            brief_lead(
                brief,
                format!(
                    "run {run} was interrupted ({}){}",
                    r["interrupted_reason"].as_str().unwrap_or("no reason recorded"),
                    r["error_text"].as_str().map(|e| format!(": {e}")).unwrap_or_default()
                ),
            ),
        );
        if let Some(n) = r["next_step"].as_str() {
            stop = stop.next(n);
        }
        stop.next(format!("thoth status {}", self.task_id)).evidence(json!({
            "run_id": run,
            "interrupted_reason": r["interrupted_reason"],
            "error_text": r["error_text"],
            "next_step": r["next_step"],
        }))
    }

    fn run_failed_stop(&self, run: &str, r: &Value, brief: &Value, extra: &str) -> Stop {
        let mut stop = Stop::new(
            "run_failed",
            brief_lead(
                brief,
                format!(
                    "run {run} failed{}{extra}",
                    r["error_text"].as_str().map(|e| format!(": {e}")).unwrap_or_default()
                ),
            ),
        );
        if let Some(n) = r["next_step"].as_str() {
            stop = stop.next(n);
        }
        stop.next(format!("thoth status {}", self.task_id)).evidence(json!({
            "run_id": run,
            "interrupted_reason": r["interrupted_reason"],
            "error_text": r["error_text"],
            "next_step": r["next_step"],
        }))
    }

    fn spec_rewrite_needed(&self, rs: &Value, attempt: i64, why: &str, error: Option<&str>) -> Stop {
        let review_id = rs["review_id"].as_str().unwrap_or("?").to_string();
        Stop::new(
            "spec_rewrite_needed",
            brief_lead(
                &rs["brief"],
                format!("REJECT {review_id} (attempt {attempt}) needs the spec rewritten before rework: {why}"),
            ),
        )
        .next(format!("thoth review show {} --json", self.task_id))
        .next(format!("thoth task update-spec {} -f FILE --json", self.task_id))
        .next(format!("thoth pipeline start {} ... (from reject, once the spec is rewritten)", self.task_id))
        .evidence(json!({
            "review_id": review_id,
            "attempt": attempt,
            "unmet": unmet_ids(&rs["findings"]),
            "error": error,
        }))
    }

    // --- the walk --------------------------------------------------------------------------

    fn drive(&mut self) -> Result<Stop> {
        let st = self.status()?;
        let task_status = st["status"].as_str().unwrap_or("").to_string();
        self.attempt = st["attempt"].as_i64();
        if task_status == "cancelled" {
            return Ok(self.task_cancelled(None));
        }
        let rs = self.review_show()?;
        if rs["pending_fix"] == true {
            return Ok(self.needs_fix(&rs));
        }
        if let Some(a) = st["attempt"].as_i64().filter(|a| *a > self.settings.max_attempts) {
            return Ok(self.attempts_exhausted(a, None));
        }
        let live = st["runs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["status"] == "queued" || r["status"] == "running")
            .max_by(|a, b| a["run_id"].as_str().cmp(&b["run_id"].as_str()))
            .cloned();
        if let Some(live) = live {
            let run = live["run_id"].as_str().unwrap_or("").to_string();
            self.log(&format!("attaching to live {} run {run}", live["role"].as_str().unwrap_or("?")));
            if live["role"] == role::REVIEWER {
                let target = newest_run(&st, role::IMPLEMENTER)
                    .and_then(|r| r["run_id"].as_str().map(str::to_string))
                    .unwrap_or_default();
                return self.wait_review(&run, &target, 0, RevRetry { pos: Some(0), ..Default::default() });
            }
            if live["role"] == role::IMPLEMENTER {
                return self.wait_implementation(&run);
            }
        }
        let newest_impl = newest_run(&st, role::IMPLEMENTER).cloned().unwrap_or(Value::Null);
        let newest_id = newest_impl["run_id"].as_str().unwrap_or("").to_string();
        let newest_attempt = newest_impl["attempt"].as_i64().unwrap_or(1);
        match task_status.as_str() {
            "queued" => self.implement(ImplPlan::default()),
            "in_review" => {
                let candidate = st["runs"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|r| r["role"] == role::IMPLEMENTER && r["status"] == "completed")
                    .max_by(|a, b| a["run_id"].as_str().cmp(&b["run_id"].as_str()))
                    .and_then(|r| r["run_id"].as_str().map(str::to_string));
                match candidate {
                    Some(run) => {
                        self.log(&format!("continuing from in_review with candidate run {run}"));
                        self.protected(&run)
                    }
                    None => Ok(Stop::new(
                        "error",
                        format!("task {} is in_review but has no completed implementer run", self.task_id),
                    )
                    .next(format!("thoth status {}", self.task_id))),
                }
            }
            "reject" => {
                if rs["verdict"] != "REJECT" {
                    return Ok(Stop::new(
                        "error",
                        format!(
                            "task {} is reject but its newest review is {}, not a REJECT",
                            self.task_id,
                            rs["verdict"].as_str().unwrap_or("missing")
                        ),
                    )
                    .next(format!("thoth review show {} --json", self.task_id)));
                }
                self.log("continuing from reject: rework from the first implementer entry");
                self.after_reject(&rs, 0)
            }
            "unverified" => {
                if self.unverified_by_reviewer(&newest_id)? {
                    return Ok(Stop::new(
                        "review_unverified",
                        brief_lead(
                            &rs["brief"],
                            format!(
                                "task {} was left unverified by a review of run {newest_id}, not by its delivery; an UNVERIFIED review is yours to judge, so nothing was launched",
                                self.task_id
                            ),
                        ),
                    )
                    .next(format!("thoth review show {} --json", self.task_id))
                    .next(format!("thoth events --task {} --json", self.task_id))
                    .evidence(json!({"run_id": newest_id, "review_id": rs["review_id"]})));
                }
                self.budget(newest_attempt).rerouted = true;
                self.implement(ImplPlan {
                    context: Some(format!("reroute after the unverified delivery of run {newest_id}")),
                    cause: Some(self.unverified_cause(&newest_id, &Value::Null)),
                    ..Default::default()
                })
            }
            "interrupted" => {
                let why = newest_impl["interrupted_reason"].as_str().unwrap_or("").to_string();
                if !matches!(why.as_str(), "quota" | "auth" | "crash" | "transient") {
                    return Ok(self.run_interrupted_stop(&newest_id, &newest_impl, &Value::Null));
                }
                if matches!(why.as_str(), "crash" | "transient") {
                    self.budget(newest_attempt).crash_recovered = true;
                }
                self.implement(ImplPlan {
                    context: Some(format!(
                        "dispatch after run {newest_id} on {} was interrupted ({why})",
                        newest_impl["profile"].as_str().unwrap_or("?")
                    )),
                    cause: Some(self.run_cause(&newest_id, &newest_impl, &Value::Null)),
                    ..Default::default()
                })
            }
            "failed" => {
                if newest_impl["interrupted_reason"] != "timeout" {
                    return Ok(self.run_failed_stop(&newest_id, &newest_impl, &Value::Null, ""));
                }
                self.budget(newest_attempt).timeout_retried = true;
                let t = doubled(newest_impl["timeout_seconds"].as_i64());
                self.implement(ImplPlan {
                    context: Some(format!(
                        "retry after run {newest_id} timed out at {}s; timeout doubled to {t}s",
                        newest_impl["timeout_seconds"]
                    )),
                    timeout: Some(t),
                    cause: Some(self.run_cause(&newest_id, &newest_impl, &Value::Null)),
                    ..Default::default()
                })
            }
            other => Ok(Stop::new(
                "error",
                format!("task {} is '{other}', which this pipeline does not continue from", self.task_id),
            )
            .next(format!("thoth status {}", self.task_id))),
        }
    }

    /// AC1: an `unverified` the newest implementer run's review left (an
    /// UNVERIFIED verdict or a `review_invalid` after its `result_ingested`).
    fn unverified_by_reviewer(&self, newest_impl: &str) -> Result<bool> {
        if newest_impl.is_empty() {
            return Ok(false);
        }
        let c = self.cli(&["events", "--since", "0", "--task", &self.task_id, "--json"])?;
        let doc = c
            .json
            .clone()
            .filter(|_| c.code == Some(0))
            .ok_or_else(|| anyhow!("thoth events --task {} --json failed: {}", self.task_id, c.error()))?;
        let events = doc["events"].as_array().cloned().unwrap_or_default();
        let ingested = events
            .iter()
            .filter(|e| e["type"] == event_type::RESULT_INGESTED && e["run_id"] == newest_impl)
            .filter_map(|e| e["seq"].as_i64())
            .max();
        let Some(seq) = ingested else { return Ok(false) };
        Ok(events.iter().any(|e| {
            e["seq"].as_i64().is_some_and(|s| s > seq)
                && ((e["type"] == event_type::VERDICT && e["detail"]["verdict"] == "UNVERIFIED")
                    || e["type"] == event_type::REVIEW_INVALID)
        }))
    }

    /// AC6/AC7: searches the implementer list forward from `plan.start` and
    /// dispatches the first usable entry; never wraps.
    fn implement(&mut self, plan: ImplPlan) -> Result<Stop> {
        let total = self.settings.implementers.len();
        let mut skipped: Vec<Value> = Vec::new();
        for pos in plan.start..total {
            if self.stop_requested()? {
                return Ok(self.stopped(None));
            }
            let e = self.settings.implementers[pos].clone();
            self.set_step("dispatch")?;
            let u = self.preflight(role::IMPLEMENTER, &e, pos)?;
            if !u.ok {
                let why = u.why.unwrap_or_default();
                skipped.push(json!({"profile": e.profile, "reason": why}));
                self.set_impl_index(pos)?;
                self.note(false, pos, format!("skipped: {why}"), None);
                continue;
            }
            let index = self.begin_step();
            let mut args: Vec<String> = vec![
                "dispatch".into(),
                self.task_id.clone(),
                "--profile".into(),
                e.profile.clone(),
                "--model".into(),
                e.model.clone(),
            ];
            if let Some(effort) = &e.effort {
                args.extend(["--effort".into(), effort.clone()]);
            }
            args.extend([
                "--reason".into(),
                self.reason_for(index, plan.context.as_deref(), "implementer", pos, total),
            ]);
            if let Some(t) = plan.timeout.or(self.settings.timeout_seconds) {
                args.extend(["--timeout".into(), format!("{t}s")]);
            }
            if let Some(a) = plan.allow_attempt {
                args.extend(["--allow-attempt".into(), a.to_string()]);
            }
            args.push("--json".into());
            self.set_impl_index(pos)?;
            let ran = self.run_step("dispatch", &args, index);
            let meta = StepMeta { entry_index: Some(pos + 1), skipped: skipped.clone(), ..Default::default() };
            let out = self.finish_step(index, "dispatch", &args, &ran, None, meta)?;
            if !out.ok {
                let err = out.error.clone().unwrap_or_else(|| "dispatch failed".into());
                if let Some(stuck) = out.err_doc.as_ref().map(|d| d["stuck"].clone()).filter(|s| !s.is_null()) {
                    self.note(false, pos, format!("refused (stuck): {err}"), None);
                    return Ok(Stop::new("stuck", format!("dispatch of {} refused automatic rework: {err}", e.text()))
                        .next(format!("thoth review show {} --json", self.task_id))
                        .next(format!("thoth task update-spec {} -f FILE --json", self.task_id))
                        .next(format!("thoth task close {} --reason TEXT", self.task_id))
                        .evidence(json!({"step": index, "stuck": stuck, "error": err})));
                }
                if is_auth_refusal(&err) || is_reserve_refusal(&err) {
                    skipped.push(json!({"profile": e.profile, "reason": format!("dispatch refused: {err}")}));
                    self.note(false, pos, format!("dispatch refused: {err}"), None);
                    continue;
                }
                self.note(false, pos, format!("dispatch refused: {err}"), None);
                return Ok(Stop::new("dispatch_refused", format!("dispatch of {} was refused: {err}", e.text()))
                    .next(format!("thoth status {}", self.task_id))
                    .evidence(json!({"step": index, "error": err})));
            }
            let run = out.json.as_ref().and_then(|v| v["run_id"].as_str()).unwrap_or("").to_string();
            if let Some(a) = out.json.as_ref().and_then(|v| v["attempt"].as_i64()) {
                self.attempt = Some(a);
            }
            self.note(false, pos, format!("dispatched run {run} (attempt {})", self.attempt.unwrap_or(0)), Some(&run));
            if self.stop_requested()? {
                return Ok(self.stopped(Some(&run)));
            }
            return self.wait_implementation(&run);
        }
        Ok(self.exhausted(false, plan.start, "every remaining entry was skipped, refused or already used", plan.cause.as_ref()))
    }

    /// Waits for `run` to end; `Err(stop)` for a stop request or a lost runner.
    fn wait_run(&mut self, run: &str) -> Result<std::result::Result<Value, Stop>> {
        loop {
            if self.stop_requested()? {
                return Ok(Err(self.stopped(Some(run))));
            }
            let w = self.cli(&["wait", "--run", run, "--timeout", WAIT_POLL, "--json"])?;
            match w.code {
                Some(0) => return Ok(Ok(w.json.unwrap_or(Value::Null))),
                Some(3) => {}
                _ => bail!("thoth wait --run {run} failed: {}", w.error()),
            }
            let st = self.status()?;
            if let Some(r) = find_run(&st, run).filter(|r| r["heartbeat_stale"] == true) {
                let hb = r["heartbeat_at"].as_str().unwrap_or("-").to_string();
                return Ok(Err(Stop::new(
                    "runner_lost",
                    format!(
                        "run {run} is still 'running' but its runner stopped answering (last heartbeat {hb}); \
                         nothing will settle it on its own"
                    ),
                )
                .next(format!("thoth cancel {run}"))
                .next(format!("thoth status {}", self.task_id))
                .evidence(json!({"run_id": run, "heartbeat_at": hb}))));
            }
        }
    }

    fn task_cancelled(&self, run: Option<&str>) -> Stop {
        Stop::new("task_cancelled", format!("task {} was cancelled; nothing more is launched", self.task_id))
            .next(format!("thoth status {}", self.task_id))
            .evidence(json!({"run_id": run}))
    }

    fn wait_implementation(&mut self, run: &str) -> Result<Stop> {
        self.set_step("wait_run")?;
        let waited = match self.wait_run(run)? {
            Ok(v) => v,
            Err(stop) => return Ok(stop),
        };
        let st = self.status()?;
        let task_status = st["status"].as_str().unwrap_or("").to_string();
        let r = find_run(&st, run).cloned().unwrap_or(Value::Null);
        let run_status = r["status"].as_str().unwrap_or("").to_string();
        let brief = waited["brief"].clone();
        let attempt = r["attempt"].as_i64().or(self.attempt).unwrap_or(1);
        let profile = r["profile"].as_str().unwrap_or("?").to_string();
        if task_status == "cancelled" || run_status == "cancelled" {
            return Ok(self.task_cancelled(Some(run)));
        }
        let pos = self.impl_index;
        match run_status.as_str() {
            "interrupted" => {
                let why = r["interrupted_reason"].as_str().unwrap_or("").to_string();
                match why.as_str() {
                    "quota" | "auth" => {
                        if self.impl_index < self.settings.implementers.len() {
                            self.note(false, pos, format!("run {run} stopped ({why}); handed over"), Some(run));
                        }
                        return self.implement(ImplPlan {
                            start: self.impl_index + 1,
                            context: Some(format!("handoff after run {run} on {profile} stopped ({why})")),
                            cause: Some(self.run_cause(run, &r, &brief)),
                            ..Default::default()
                        });
                    }
                    "crash" | "transient" => return self.recover_crash(run, &r, &why, attempt, &brief),
                    _ => return Ok(self.run_interrupted_stop(run, &r, &brief)),
                }
            }
            "failed" => {
                if r["interrupted_reason"] == "timeout" && !self.budget(attempt).timeout_retried {
                    self.budget(attempt).timeout_retried = true;
                    let t = doubled(r["timeout_seconds"].as_i64());
                    self.note(false, pos, format!("run {run} timed out; one retry at {t}s"), Some(run));
                    return self.implement(ImplPlan {
                        start: self.impl_index,
                        context: Some(format!(
                            "retry after run {run} on {profile} timed out at {}s; timeout doubled to {t}s",
                            r["timeout_seconds"]
                        )),
                        timeout: Some(t),
                        cause: Some(self.run_cause(run, &r, &brief)),
                        ..Default::default()
                    });
                }
                let extra = if r["interrupted_reason"] == "timeout" {
                    format!(" (its attempt {attempt} already had its one doubled-timeout retry)")
                } else {
                    String::new()
                };
                return Ok(self.run_failed_stop(run, &r, &brief, &extra));
            }
            _ => {}
        }
        match task_status.as_str() {
            "in_review" => self.protected(run),
            "unverified" if !self.budget(attempt).rerouted => {
                self.budget(attempt).rerouted = true;
                self.log(&format!("run {run}'s delivery is unverified; dispatching once more in the same attempt"));
                self.implement(ImplPlan {
                    start: self.impl_index,
                    context: Some(format!("reroute after the unverified delivery of run {run}")),
                    cause: Some(self.unverified_cause(run, &brief)),
                    ..Default::default()
                })
            }
            "unverified" => {
                let why = state::latest_event_of_type(&self.conn, event_type::RESULT_INVALID, Some(&self.task_id))?
                    .and_then(|e| serde_json::from_str::<Value>(&e.detail).ok())
                    .map(|d| d["reason"].clone())
                    .unwrap_or(Value::Null);
                Ok(Stop::new(
                    "unverified_twice",
                    brief_lead(
                        &brief,
                        format!(
                            "the delivery of run {run} is unverified again after one more dispatch in the same \
                             attempt{}",
                            why.as_str().map(|w| format!(": {w}")).unwrap_or_default()
                        ),
                    ),
                )
                .next(format!("thoth task result {} --run {run}", self.task_id))
                .next(format!("thoth events --task {} --json", self.task_id))
                .evidence(json!({"run_id": run, "result_invalid_reason": why})))
            }
            "failed" => Ok(Stop::new("run_failed", brief_lead(&brief, format!("run {run} ended and task {} failed", self.task_id)))
                .next(format!("thoth status {}", self.task_id))
                .evidence(json!({
                    "run_id": run,
                    "interrupted_reason": r["interrupted_reason"],
                    "error_text": r["error_text"],
                    "next_step": r["next_step"],
                }))),
            other => Ok(Stop::new(
                "error",
                format!("run {run} ended '{run_status}' and left task {} '{other}', which this pipeline does not handle", self.task_id),
            )
            .next(format!("thoth status {}", self.task_id))
            .evidence(json!({"run_id": run}))),
        }
    }

    /// AC7: the first crash/transient recovery of an attempt resumes the run
    /// once when its entry is still usable; otherwise, and the second time,
    /// the next entry is dispatched.
    fn recover_crash(&mut self, run: &str, r: &Value, why: &str, attempt: i64, brief: &Value) -> Result<Stop> {
        let profile = r["profile"].as_str().unwrap_or("?").to_string();
        let pos = self.impl_index;
        let total = self.settings.implementers.len();
        let cause = self.run_cause(run, r, brief);
        if self.budget(attempt).crash_recovered {
            self.note(false, pos.min(total - 1), format!("run {run} interrupted ({why}) again; moving on"), Some(run));
            return self.implement(ImplPlan {
                start: pos + 1,
                context: Some(format!(
                    "after run {run} on {profile} was interrupted ({why}) again in attempt {attempt}; its one resume was used"
                )),
                cause: Some(cause),
                ..Default::default()
            });
        }
        self.budget(attempt).crash_recovered = true;
        let ours = pos < total && {
            let e = &self.settings.implementers[pos];
            e.profile == profile && r["model"].as_str().is_some_and(|m| m.eq_ignore_ascii_case(&e.model))
        };
        if !ours {
            return self.implement(ImplPlan {
                start: pos,
                context: Some(format!("after run {run} on {profile} was interrupted ({why}); it was not launched from this entry, so it is not resumed")),
                cause: Some(cause),
                ..Default::default()
            });
        }
        let e = self.settings.implementers[pos].clone();
        self.set_step("resume")?;
        let u = self.preflight(role::IMPLEMENTER, &e, pos)?;
        let why_not = if u.ok {
            let index = self.begin_step();
            let args: Vec<String> = vec!["resume".into(), run.to_string(), "--json".into()];
            let reason = format!(
                "pipeline {} step {index}: resume of run {run} on {profile} after it was interrupted ({why}): {}",
                self.id, self.settings.reason
            );
            let ran = self.run_step("resume", &args, index);
            let meta = StepMeta {
                entry_index: Some(pos + 1),
                attempt: Some(attempt),
                reason: Some(reason),
                ..Default::default()
            };
            let out = self.finish_step(index, "resume", &args, &ran, None, meta)?;
            if out.ok {
                self.note(false, pos, format!("run {run} interrupted ({why}); resumed"), Some(run));
                if self.stop_requested()? {
                    return Ok(self.stopped(Some(run)));
                }
                return self.wait_implementation(run);
            }
            format!("resume was refused: {}", out.error.unwrap_or_else(|| "resume failed".into()))
        } else {
            format!("its entry is not usable: {}", u.why.unwrap_or_default())
        };
        self.note(false, pos, format!("run {run} interrupted ({why}); {why_not}"), Some(run));
        self.implement(ImplPlan {
            start: pos + 1,
            context: Some(format!("after run {run} on {profile} was interrupted ({why}); {why_not}")),
            cause: Some(cause),
            ..Default::default()
        })
    }

    // --- AC4/AC5: a REJECT -------------------------------------------------------------------

    fn after_reject(&mut self, rs: &Value, start: usize) -> Result<Stop> {
        if rs["pending_fix"] == true {
            return Ok(self.needs_fix(rs));
        }
        let review_id = rs["review_id"].as_str().unwrap_or("?").to_string();
        let reviews = state::list_reviews_for_task(&self.conn, &self.task_id)?;
        let rejected_attempt = reviews
            .iter()
            .rev()
            .find(|r| r.review_id == review_id)
            .and_then(|r| state::get_run(&self.conn, &r.target_run_id).ok().flatten())
            .map(|r| r.attempt)
            .unwrap_or_else(|| self.attempt.unwrap_or(1));
        let proposed = self.attempts_used().max(1) + 1;
        if proposed > self.settings.max_attempts {
            return Ok(self.attempts_exhausted(proposed, Some(rs)));
        }
        let task = state::get_task(&self.conn, &self.task_id)?.ok_or_else(|| anyhow!("task {} disappeared", self.task_id))?;
        let auto_fold = crate::commands::dispatch::AUTO_FOLD_RISKS.contains(&task.risk.as_str());
        if !auto_fold {
            if self.settings.fold_on_reject {
                if let Some(stop) = self.repeated_reject(&reviews) {
                    return Ok(stop);
                }
            }
            if !crate::commands::dispatch::spec_rewritten_since_reject(&self.conn, &self.task_id)? {
                if !self.settings.fold_on_reject {
                    return Ok(self.spec_rewrite_needed(
                        rs,
                        rejected_attempt,
                        &format!(
                            "the task is risk '{}', no spec change after the REJECT exists and --fold-on-reject is off; rewrite it with `thoth task update-spec {} -f FILE --json` (or an author's APPROVE through `thoth spec review confirm`), then start the pipeline again",
                            task.risk, self.task_id
                        ),
                        None,
                    ));
                }
                if let Some(stop) = self.fold(rs, &review_id, rejected_attempt, &task.spec)? {
                    return Ok(stop);
                }
            }
        }
        let cap = state::get_task_attempt_cap(&self.conn, &self.task_id)?;
        let allow = (proposed > cap && proposed <= self.settings.max_attempts).then_some(proposed);
        if start > 0 && start - 1 < self.settings.implementers.len() {
            self.note(false, start - 1, format!("rejected by review {review_id}"), None);
        }
        self.implement(ImplPlan {
            start,
            context: Some(format!(
                "rework of REJECT {review_id} (attempt {rejected_attempt} rejected, now attempt {proposed})"
            )),
            timeout: None,
            allow_attempt: allow,
            cause: Some(self.reject_cause(rs, rejected_attempt)),
        })
    }

    /// AC4: three latest consecutive REJECT reviews sharing an unmet AC.
    fn repeated_reject(&self, reviews: &[state::Review]) -> Option<Stop> {
        let streak: Vec<&state::Review> = reviews
            .iter()
            .rev()
            .take_while(|r| r.verdict == crate::review_json::Verdict::Reject.as_str())
            .take(3)
            .collect();
        if streak.len() < 3 {
            return None;
        }
        let unmet = |r: &state::Review| unmet_ids(&serde_json::from_str::<Value>(&r.findings).unwrap_or(Value::Null));
        let sets: Vec<Vec<String>> = streak.iter().map(|r| unmet(r)).collect();
        let shared = sets[0].iter().find(|ac| sets[1..].iter().all(|s| s.contains(ac)))?.clone();
        let ids: Vec<String> = streak.iter().rev().map(|r| r.review_id.clone()).collect();
        Some(
            Stop::new(
                "repeated_reject",
                format!(
                    "{shared} was unmet in the last three REJECT reviews in a row ({}); folding the findings again would repeat them, so nothing more is launched",
                    ids.join(", ")
                ),
            )
            .next(format!("thoth review show {} --json", self.task_id))
            .next(format!("thoth task update-spec {} -f FILE --json", self.task_id))
            .next(format!("thoth task close {} --reason TEXT", self.task_id))
            .evidence(json!({"criterion": shared, "review_ids": ids})),
        )
    }

    /// AC5: appends the REJECT's findings, unchanged, to the spec's
    /// constraints after a marker and replaces the spec with `task
    /// update-spec`. `Some(stop)` when it could not.
    fn fold(&mut self, rs: &Value, review_id: &str, attempt: i64, spec_text: &str) -> Result<Option<Stop>> {
        self.set_step("update_spec")?;
        let folded = crate::commands::dispatch::fold_findings(&rs["findings"].to_string(), &[]);
        let lines: Vec<String> = folded
            .unmet
            .iter()
            .chain(folded.extra.iter())
            .chain(folded.advisory.iter())
            .cloned()
            .collect();
        if lines.is_empty() {
            return Ok(Some(self.spec_rewrite_needed(
                rs,
                attempt,
                "--fold-on-reject is on, but the REJECT has no findings to fold; rewrite the spec yourself",
                None,
            )));
        }
        let mut spec: Value = match serde_json::from_str(spec_text) {
            Ok(v @ Value::Object(_)) => v,
            _ => {
                return Ok(Some(self.spec_rewrite_needed(rs, attempt, "the stored spec could not be read as a JSON object", None)))
            }
        };
        let marker = format!("REJECT {review_id} (attempt {attempt}), folded as-is by pipeline {}:", self.id);
        if !spec["constraints"].is_array() {
            spec["constraints"] = json!([]);
        }
        let constraints = spec["constraints"].as_array_mut().expect("an array");
        if constraints.iter().any(|c| c.as_str().is_some_and(|s| s.starts_with(&format!("REJECT {review_id} (")))) {
            return Ok(Some(self.spec_rewrite_needed(
                rs,
                attempt,
                "the findings of this REJECT are already folded into the spec; it is not appended twice",
                None,
            )));
        }
        constraints.push(json!(marker));
        for l in &lines {
            constraints.push(json!(l));
        }
        let text = serde_json::to_string_pretty(&spec)?;
        let index = self.begin_step();
        let file = self.dir.join(format!("step-{index:03}.spec.json"));
        let masked = self.masker.mask_text(&text).0;
        std::fs::write(&file, &masked).with_context(|| format!("writing {}", file.display()))?;
        // The file keeps only masked text; when masking changed it, the raw
        // spec goes to `task update-spec` through a short-lived file so its
        // own secret refusal decides, and the file is removed at once.
        let raw = (masked != text).then(|| self.dir.join(format!(".step-{index:03}.spec.raw.tmp")));
        if let Some(raw) = &raw {
            std::fs::write(raw, &text).with_context(|| format!("writing {}", raw.display()))?;
        }
        let pass = raw.clone().unwrap_or_else(|| file.clone());
        let args: Vec<String> = vec![
            "task".into(),
            "update-spec".into(),
            self.task_id.clone(),
            "-f".into(),
            pass.display().to_string(),
            "--json".into(),
        ];
        let ran = self.run_step("update_spec", &args, index);
        if let Some(raw) = &raw {
            let _ = std::fs::remove_file(raw);
        }
        let ok = ran.code == Some(0);
        let sha = if ok {
            state::latest_event_of_type(&self.conn, event_type::SPEC_UPDATED, Some(&self.task_id))?
                .and_then(|e| serde_json::from_str::<Value>(&e.detail).ok())
                .map(|d| d["spec_sha256"].clone())
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let result = json!({
            "spec_sha256": sha,
            "review_id": review_id,
            "folded_lines": lines.len(),
            "marker": marker,
            "file": file.display().to_string(),
        });
        let out = self.finish_step(index, "update_spec", &args, &ran, None, StepMeta { result: Some(result), ..Default::default() })?;
        if !out.ok {
            let err = out.error.unwrap_or_else(|| "task update-spec failed".into());
            return Ok(Some(self.spec_rewrite_needed(
                rs,
                attempt,
                &format!("folding the findings was refused by task update-spec, so the spec is unchanged: {err}"),
                Some(&err),
            )));
        }
        Ok(None)
    }

    fn protected(&mut self, run_id: &str) -> Result<Stop> {
        if self.stop_requested()? {
            return Ok(self.stopped(None));
        }
        self.set_step("protected")?;
        let run = state::get_run(&self.conn, run_id)?.ok_or_else(|| anyhow!("run {run_id} disappeared"))?;
        if run.attempt > self.settings.max_attempts {
            return Ok(self.attempts_exhausted(run.attempt, None));
        }
        let task = state::get_task(&self.conn, &self.task_id)?.ok_or_else(|| anyhow!("task {} disappeared", self.task_id))?;
        let Some(candidate) = run.candidate_commit.clone() else {
            return Ok(Stop::new("protected_unreadable", format!("run {run_id} has no candidate commit, so its protected changes cannot be read"))
                .next(format!("thoth status {}", self.task_id))
                .evidence(json!({"run_id": run_id})));
        };
        let repo_id = ids::repo_id_for(&self.root)?;
        let (warnings, again) = crate::commands::review::prepare_protected_evidence(
            &self.conn,
            &self.paths,
            &repo_id,
            &task.base_commit,
            &run,
            &candidate,
            &self.root,
        );
        let unreadable = |why: String| {
            Stop::new("protected_unreadable", why)
                .next(format!("thoth evidence list {}", self.task_id))
                .next(format!("thoth review packet {}", self.task_id))
                .evidence(json!({"run_id": run_id, "warnings": warnings, "error": again}))
        };
        let latest = match crate::protected::latest_diff(&self.conn, &run)? {
            None => {
                return Ok(unreadable(format!(
                    "run {run_id} has no diff evidence for its candidate and recording it failed{}; its \
                     protected changes are not known, so nothing is treated as unchanged",
                    warnings.iter().map(|w| format!(": {w}")).collect::<String>()
                )))
            }
            Some(d) => d,
        };
        let Some(changes) = latest.changes else {
            return Ok(unreadable(format!(
                "run {run_id}'s diff evidence {} has no readable protected_changes and recording it again \
                 failed{}",
                latest.code,
                again.as_deref().map(|e| format!(": {e}")).unwrap_or_default()
            )));
        };
        let left = crate::protected::uncovered(&changes, &crate::protected::approved_pairs(&self.conn, run_id)?);
        if !left.is_empty() {
            let listed: Vec<String> = left.iter().map(|c| format!("{} ({}, {})", c.path, c.status, c.kind)).collect();
            return Ok(Stop::new(
                "needs_approval",
                format!(
                    "run {run_id} changed protected files nobody has approved yet: {}; read the change, then \
                     approve it yourself and start the pipeline again (it never approves for you)",
                    listed.join(", ")
                ),
            )
            .next(format!("thoth review packet {}", self.task_id))
            .next(format!("thoth checks approve --run {run_id} --reason TEXT"))
            .next(format!("thoth pipeline start {} ... (continues this candidate)", self.task_id))
            .evidence(json!({
                "run_id": run_id,
                "diff": latest.code,
                "changes": left.iter().map(|c| c.to_json()).collect::<Vec<_>>(),
            })));
        }
        self.verify(run_id)
    }

    fn verify(&mut self, run_id: &str) -> Result<Stop> {
        let mut attempts: Vec<Value> = Vec::new();
        loop {
            if self.stop_requested()? {
                return Ok(self.stopped(None));
            }
            self.set_step("verify_wait")?;
            let waiting = Instant::now();
            let owner = slots::Owner { pipeline_id: &self.id, task_id: &self.task_id, run_id };
            let stop_check = || self.stop_requested().unwrap_or(false);
            let got = slots::acquire(
                &self.paths.home,
                &owner,
                Duration::from_secs(self.settings.verify_wait_seconds),
                &stop_check,
            )?;
            let guard = match got {
                slots::Outcome::Got(g) => g,
                slots::Outcome::Stopped => return Ok(self.stopped(None)),
                slots::Outcome::TimedOut(holders) => {
                    let listed: Vec<String> = holders
                        .iter()
                        .map(|h| {
                            format!(
                                "{} (pipeline {}, task {}, run {}, pid {}, since {})",
                                h["file"].as_str().unwrap_or("?"),
                                h["pipeline_id"].as_str().unwrap_or("?"),
                                h["task_id"].as_str().unwrap_or("?"),
                                h["run_id"].as_str().unwrap_or("?"),
                                h["pid"],
                                h["acquired_at"].as_str().unwrap_or("?")
                            )
                        })
                        .collect();
                    return Ok(Stop::new(
                        "verify_wait_timeout",
                        format!(
                            "no verify slot came free within {}s; held by: {}",
                            self.settings.verify_wait_seconds,
                            if listed.is_empty() { "nobody now".to_string() } else { listed.join("; ") }
                        ),
                    )
                    .next("thoth config get verify-slots")
                    .next(format!("thoth pipeline start {} ... (continues this candidate)", self.task_id))
                    .evidence(json!({"holders": holders, "dir": slots::dir(&self.paths.home).display().to_string()})));
                }
            };
            let waited_ms = (waiting.elapsed().as_millis() as i64).max(1);
            self.set_step("verify")?;
            let args = vec![
                "verify".to_string(),
                run_id.to_string(),
                "--timeout".to_string(),
                format!("{}s", self.settings.verify_timeout_seconds),
                "--json".to_string(),
            ];
            let out = self.step("verify", args, Some(waited_ms))?;
            drop(guard);
            if self.verify_killed.load(Ordering::SeqCst) || self.stop_requested()? {
                return Ok(self.stopped(None));
            }
            let codes: Vec<Value> = out
                .json
                .as_ref()
                .and_then(|d| d["results"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .map(|r| r["evidence"].clone())
                .collect();
            match classify(&out) {
                Verified::Green => break,
                Verified::Blocked(why) => {
                    return Ok(Stop::new("verify_blocked", format!("verify of run {run_id} could not judge the candidate: {why}"))
                        .next("thoth checks list")
                        .next("thoth checks approve --reason TEXT (or --run RUN for files the run changed)")
                        .next(format!("thoth verify {run_id} --dry-run"))
                        .evidence(json!({"run_id": run_id, "step": self.step_index, "evidence": codes, "why": why})));
                }
                Verified::Red(failed) => {
                    attempts.push(json!({"step": self.step_index, "evidence": codes, "failed": failed}));
                    if attempts.len() < 2 {
                        self.log("verify was red; releasing the slot and trying once more");
                        continue;
                    }
                    let failed_ids: Vec<String> = attempts
                        .iter()
                        .flat_map(|a| a["failed"].as_array().cloned().unwrap_or_default())
                        .map(|f| {
                            format!(
                                "{}{}",
                                f["check_id"].as_str().unwrap_or("?"),
                                f["error"].as_str().map(|e| format!(" ({e})")).unwrap_or_else(|| format!(" (exit {})", f["exit_code"]))
                            )
                        })
                        .collect();
                    return Ok(Stop::new(
                        "verify_failed",
                        format!("verify of run {run_id} was red twice: {}", failed_ids.join(", ")),
                    )
                    .next(format!("thoth evidence list {}", self.task_id))
                    .next(format!("thoth verify {run_id} --json"))
                    .evidence(json!({"run_id": run_id, "attempts": attempts})));
                }
            }
        }
        self.review(run_id)
    }

    // --- AC8: the review ------------------------------------------------------------------

    fn review(&mut self, target: &str) -> Result<Stop> {
        let start = self.rev_index;
        self.review_from(target, RevPlan { start, ..Default::default() })
    }

    fn review_from(&mut self, target: &str, plan: RevPlan) -> Result<Stop> {
        if self.stop_requested()? {
            return Ok(self.stopped(None));
        }
        if let Some(run) = state::get_run(&self.conn, target)? {
            if run.attempt > self.settings.max_attempts {
                return Ok(self.attempts_exhausted(run.attempt, None));
            }
        }
        let st = self.status()?;
        let candidate = find_run(&st, target).cloned().unwrap_or(Value::Null);
        let total = self.settings.reviewers.len();
        let mut skipped: Vec<Value> = Vec::new();
        for pos in plan.start..total {
            if self.stop_requested()? {
                return Ok(self.stopped(None));
            }
            let e = self.settings.reviewers[pos].clone();
            self.set_step("review_start")?;
            if same_choice(&e, &candidate) {
                let why = format!(
                    "same model and effort as implementer run {target} ({}, effort {} requested, {} sent)",
                    candidate["model"].as_str().unwrap_or("?"),
                    candidate["effort_requested"].as_str().unwrap_or("none"),
                    candidate["effort"].as_str().unwrap_or("none")
                );
                skipped.push(json!({"profile": e.profile, "reason": why}));
                self.set_rev_index(pos)?;
                self.note(true, pos, format!("skipped: {why}"), None);
                continue;
            }
            let u = self.preflight(role::REVIEWER, &e, pos)?;
            if !u.ok {
                let why = u.why.unwrap_or_default();
                skipped.push(json!({"profile": e.profile, "reason": why}));
                self.set_rev_index(pos)?;
                self.note(true, pos, format!("skipped: {why}"), None);
                continue;
            }
            let index = self.begin_step();
            let mut args: Vec<String> = vec![
                "review".into(),
                "start".into(),
                self.task_id.clone(),
                "--profile".into(),
                e.profile.clone(),
                "--model".into(),
                e.model.clone(),
            ];
            if let Some(effort) = &e.effort {
                args.extend(["--effort".into(), effort.clone()]);
            }
            args.extend([
                "--reason".into(),
                self.reason_for(index, plan.context.as_deref(), "reviewer", pos, total),
            ]);
            let retried = plan.retry.pos == Some(pos);
            let timeout = if retried { plan.timeout } else { None }.or(self.settings.review_timeout_seconds);
            if let Some(t) = timeout {
                args.extend(["--timeout".into(), format!("{t}s")]);
            }
            args.push("--json".into());
            self.set_rev_index(pos)?;
            let ran = self.run_step("review_start", &args, index);
            let meta = StepMeta { entry_index: Some(pos + 1), skipped: skipped.clone(), ..Default::default() };
            let out = self.finish_step(index, "review_start", &args, &ran, None, meta)?;
            if !out.ok {
                let err = out.error.clone().unwrap_or_else(|| "review start failed".into());
                self.note(true, pos, format!("review start refused: {err}"), None);
                if is_auth_refusal(&err) || is_reserve_refusal(&err) {
                    skipped.push(json!({"profile": e.profile, "reason": format!("review start refused: {err}")}));
                    continue;
                }
                return Ok(Stop::new("review_start_refused", format!("review start with {} was refused: {err}", e.text()))
                    .next(format!("thoth status {}", self.task_id))
                    .evidence(json!({"step": index, "error": err})));
            }
            let run = out.json.as_ref().and_then(|v| v["run_id"].as_str()).unwrap_or("").to_string();
            self.note(true, pos, format!("started review run {run}"), Some(&run));
            if self.stop_requested()? {
                return Ok(self.stopped(Some(&run)));
            }
            let retry = if retried { plan.retry.clone() } else { RevRetry { pos: Some(pos), ..Default::default() } };
            return self.wait_review(&run, target, pos, retry);
        }
        Ok(self.exhausted(
            true,
            plan.start,
            "every remaining entry was skipped, refused or already used; no reset is waited for",
            plan.cause.as_ref(),
        ))
    }

    fn needs_fix(&self, rs: &Value) -> Stop {
        let edit_run = rs["reviewer_run_id"].as_str().unwrap_or("EDIT_RUN").to_string();
        Stop::new(
            "needs_fix_confirmation",
            brief_lead(
                &rs["brief"],
                format!(
                    "review {} (edit review run {edit_run}) left a pending fix{}; it is kept for your \
                     judgement: verify it, then confirm it with another model or effort",
                    rs["review_id"].as_str().unwrap_or("?"),
                    rs["fix"]["fix_branch"].as_str().map(|b| format!(" on {b}")).unwrap_or_default()
                ),
            ),
        )
        .next(format!("thoth verify {edit_run}"))
        .next(format!(
            "thoth review confirm {} --profile P --model M --effort E --reason TEXT",
            self.task_id
        ))
        .evidence(json!({"review_id": rs["review_id"], "reviewer_run_id": rs["reviewer_run_id"], "fix": rs["fix"]}))
    }

    fn wait_review(&mut self, run: &str, target: &str, pos: usize, retry: RevRetry) -> Result<Stop> {
        self.set_step("wait_review")?;
        if let Err(stop) = self.wait_run(run)? {
            return Ok(stop);
        }
        let rs = self.review_show()?;
        let st = self.status()?;
        if rs["pending_fix"] == true {
            return Ok(self.needs_fix(&rs));
        }
        let task_status = st["status"].as_str().unwrap_or("").to_string();
        if task_status == "cancelled" {
            return Ok(self.task_cancelled(Some(run)));
        }
        let verdict = if rs["reviewer_run_id"] == run { rs["verdict"].as_str() } else { None };
        let evidence = json!({"reviewer_run_id": run, "review_id": rs["review_id"], "verdict": verdict});
        let show_cmd = format!("thoth review show {} --json", self.task_id);
        let r = find_run(&st, run).cloned().unwrap_or(Value::Null);
        let profile = r["profile"].as_str().unwrap_or("?").to_string();
        let total = self.settings.reviewers.len();
        Ok(match (verdict, task_status.as_str()) {
            (Some("PASS"), "pass") => match (rs["merge_branch"].as_str(), rs["merge_commit"].as_str()) {
                (Some(b), Some(c)) => Stop::new(
                    "pass",
                    brief_lead(&rs["brief"], format!("PASS: merge branch {b} at commit {c}; read the change before you merge it")),
                )
                .next(show_cmd)
                .evidence(json!({"reviewer_run_id": run, "review_id": rs["review_id"], "merge_branch": b, "merge_commit": c})),
                _ => Stop::new(
                    "merge_target_unavailable",
                    brief_lead(&rs["brief"], "PASS, but review show names no merge branch and commit; nothing is guessed".to_string()),
                )
                .next(show_cmd)
                .evidence(evidence),
            },
            (Some("REJECT"), _) => {
                let next = self.impl_index + 1;
                return self.after_reject(&rs, next);
            }
            (_, "unverified") => Stop::new(
                "review_unverified",
                brief_lead(&rs["brief"], format!("the review by run {run} left task {} unverified", self.task_id)),
            )
            .next(show_cmd)
            .next(format!("thoth events --task {} --json", self.task_id))
            .evidence(evidence),
            (None, _) => {
                let why = r["interrupted_reason"].as_str().unwrap_or("").to_string();
                let cause = self.reviewer_cause(run, &r);
                match (r["status"].as_str().unwrap_or(""), why.as_str()) {
                    ("interrupted", "quota" | "auth") => {
                        self.note(true, pos.min(total - 1), format!("review run {run} stopped ({why}); moving on"), Some(run));
                        return self.review_from(
                            target,
                            RevPlan {
                                start: pos + 1,
                                context: Some(format!("review after reviewer run {run} on {profile} stopped ({why})")),
                                cause: Some(cause),
                                ..Default::default()
                            },
                        );
                    }
                    ("interrupted", "crash" | "transient") => {
                        if retry.crash {
                            self.note(true, pos.min(total - 1), format!("review run {run} interrupted ({why}) again; moving on"), Some(run));
                            return self.review_from(
                                target,
                                RevPlan {
                                    start: pos + 1,
                                    context: Some(format!("review after reviewer run {run} on {profile} was interrupted ({why}) again")),
                                    cause: Some(cause),
                                    ..Default::default()
                                },
                            );
                        }
                        let mut retry = retry;
                        retry.crash = true;
                        retry.pos = Some(pos);
                        self.note(true, pos.min(total - 1), format!("review run {run} interrupted ({why}); one retry"), Some(run));
                        return self.review_from(
                            target,
                            RevPlan {
                                start: pos,
                                context: Some(format!("retry of the review after reviewer run {run} on {profile} was interrupted ({why})")),
                                timeout: None,
                                retry,
                                cause: Some(cause),
                            },
                        );
                    }
                    ("failed", "timeout") => {
                        if retry.timeout {
                            self.note(true, pos.min(total - 1), format!("review run {run} timed out again; moving on"), Some(run));
                            return self.review_from(
                                target,
                                RevPlan {
                                    start: pos + 1,
                                    context: Some(format!("review after reviewer run {run} on {profile} timed out again")),
                                    cause: Some(cause),
                                    ..Default::default()
                                },
                            );
                        }
                        let mut retry = retry;
                        retry.timeout = true;
                        retry.pos = Some(pos);
                        let t = doubled(r["timeout_seconds"].as_i64());
                        self.note(true, pos.min(total - 1), format!("review run {run} timed out; one retry at {t}s"), Some(run));
                        return self.review_from(
                            target,
                            RevPlan {
                                start: pos,
                                context: Some(format!(
                                    "retry of the review after reviewer run {run} on {profile} timed out at {}s; timeout doubled to {t}s",
                                    r["timeout_seconds"]
                                )),
                                timeout: Some(t),
                                retry,
                                cause: Some(cause),
                            },
                        );
                    }
                    _ => Stop::new(
                        "review_interrupted",
                        format!("reviewer run {run} ended without a verdict; task {} is still {task_status}", self.task_id),
                    )
                    .next(format!("thoth status {}", self.task_id))
                    .next(format!("thoth pipeline start {} ... (starts the review again)", self.task_id))
                    .evidence(json!({
                        "reviewer_run_id": run,
                        "review_id": rs["review_id"],
                        "verdict": verdict,
                        "status": r["status"],
                        "interrupted_reason": r["interrupted_reason"],
                        "error_text": r["error_text"],
                    })),
                }
            }
            (Some(v), other) => Stop::new(
                "error",
                format!("review verdict {v} left task {} '{other}', which this pipeline does not handle", self.task_id),
            )
            .next(show_cmd)
            .evidence(evidence),
        })
    }
}

/// Whole numbers without a decimal point.
fn fmt_percent(p: f64) -> String {
    if p.fract() == 0.0 {
        format!("{}", p as i64)
    } else {
        format!("{p}")
    }
}
