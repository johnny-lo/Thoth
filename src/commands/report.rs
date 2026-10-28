//! `thoth report`: one read-only HTML page, in Traditional Chinese, that
//! lets the human owner see the state of every task, review and account
//! without asking the commander (docs/post-phase2/quota-and-report-scope-2026-09-28.md §3).
//!
//! Every database is opened read-only through `usage::read_only`, so the page
//! file is the only thing this command writes (SQLite may still create the
//! -wal/-shm files of a database that already exists).

use crate::commands::models::ModelSnapshot;
use crate::commands::usage::{self, ExtraBucket, Window};
use crate::config::Paths;
use crate::db::global::Profile;
use crate::db::state;
use crate::lang::Lang;
use crate::model::event_type;
use crate::output;
use crate::plan_label;
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::{Duration, OffsetDateTime, UtcOffset};

/// How many closed tasks each repo shows before the "還有 N 件" line.
const CLOSED_TASK_LIMIT: usize = 30;
/// How many REJECT / UNVERIFIED reviews the findings section shows.
const FINDINGS_LIMIT: usize = 30;
/// How many launches the decision table section lists, newest first.
const CHOICE_LIMIT: usize = 30;

pub fn run(
    paths: &Paths,
    out: Option<&Path>,
    open: bool,
    section: Option<&str>,
    json_output: bool,
) -> Result<i32> {
    if let Some(id) = section {
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            bail!("--section must be a section id of lowercase letters, digits and dashes (today: decision-table)")
        }
    }
    if !paths.global_db().is_file() {
        bail!(
            "no thoth state at {} yet; run `thoth init` in a repo first",
            paths.home.display()
        );
    }
    let now = OffsetDateTime::now_utc();
    let snapshot = Snapshot::load(paths, now)?;
    let page = crate::redact::Masker::load(Some(&paths.home)).with_variant(esc).mask_text(&render(&snapshot, now)).0;
    let target = out
        .map(Path::to_path_buf)
        .unwrap_or_else(|| paths.home.join("report.html"));
    // Absolute, so the path printed (and handed to a browser) works from anywhere.
    let target = std::path::absolute(&target).unwrap_or(target);
    write_atomically(&target, &page)?;

    let generated_at = now.format(&Rfc3339)?;
    output::emit(
        json_output,
        &json!({ "path": target.to_string_lossy(), "generated_at": generated_at }),
        &format!("report written to {}", target.display()),
    );
    if open {
        match section {
            Some(id) => {
                let url = format!("{}#{}", file_url(&target), id);
                if let Err(e) = open_url(&url) {
                    masked_eprintln!("warning: could not open {url} in a browser: {e:#}");
                }
            }
            None => {
                if let Err(e) = open_in_browser(&target) {
                    masked_eprintln!(
                        "warning: could not open {} in a browser: {e:#}",
                        target.display()
                    );
                }
            }
        }
    }
    Ok(0)
}

/// Rewrites $THOTH_HOME/report.html with what `thoth report` would
/// write, but only when that file already exists. Called after a
/// state-changing command (or the runner) has committed, so the page the
/// human keeps open does not go stale. Best effort: every error is ignored
/// and nothing is printed, so it cannot change the caller's exit code or
/// output. Never called by `thoth hook`, which opens no database.
pub fn refresh_if_present(paths: &Paths) {
    let target = paths.home.join("report.html");
    if !target.is_file() || !paths.global_db().is_file() {
        return;
    }
    let now = OffsetDateTime::now_utc();
    let Ok(snapshot) = Snapshot::load(paths, now) else {
        return;
    };
    let page = crate::redact::Masker::load(Some(&paths.home)).with_variant(esc).mask_text(&render(&snapshot, now)).0;
    let _ = write_atomically(&target, &page);
}

// ---------------------------------------------------------------- data

struct Snapshot {
    accounts: Vec<Account>,
    models: ModelSnapshot,
    /// Read from the setting itself: `refresh_if_present` loads the models
    /// without the refresh permission, which reports `enabled` as false.
    scores_auto_refresh: bool,
    repos: Vec<RepoTasks>,
    findings: Vec<Finding>,
    counts: Counts,
    /// TIER-1: account id -> bracket text, from this machine's global.db.
    labels: HashMap<String, String>,
}

impl Snapshot {
    /// `claude-a（Max）`, or the bare id for an account this machine has no
    /// tier or label for. Not yet HTML-escaped.
    fn named(&self, id: &str) -> String {
        plan_label::named(id, self.labels.get(id).map(String::as_str), Lang::ZhTw)
    }
}

struct Account {
    profile: Profile,
    windows: Vec<Window>,
    extra_buckets: Vec<ExtraBucket>,
    implementer_runs: i64,
    reviewer_runs: i64,
    busy_seconds: i64,
    last_run: Option<OffsetDateTime>,
    /// FC-1: the quota group a `rate_limited` profile was parked for.
    parked_group: Option<String>,
}

struct RepoTasks {
    root: String,
    open: Vec<TaskRow>,
    closed: Vec<TaskRow>,
    hidden_closed: usize,
    missing_checks: Option<MissingNote>,
}

/// The newest `verify_ran` of a repo, when it found approved checks missing.
struct MissingNote {
    run_id: String,
    ts: String,
    /// (check id, `removed` | `pending`)
    items: Vec<(String, String)>,
}

struct TaskRow {
    id: String,
    title: String,
    status: String,
    risk: String,
    attempts: Option<i64>,
    verdict: Option<String>,
    updated_at: String,
    /// FC-1: the task's newest run (largest run id): run id, profile, model,
    /// interrupted reason, and whether it has a session to resume.
    newest_run: Option<(String, String, String, Option<String>, bool)>,
}

struct Finding {
    task_id: String,
    task_title: String,
    verdict: String,
    created_at: String,
    unmet: Vec<(String, String)>,
    extra: Vec<(String, String)>,
}

#[derive(Default)]
struct Counts {
    in_progress: usize,
    waiting: usize,
    attention: usize,
    passed_recently: usize,
}

impl Snapshot {
    fn load(paths: &Paths, now: OffsetDateTime) -> Result<Snapshot> {
        let global = usage::global_read_only(paths)?;
        let profiles = crate::db::global::list_profiles(&global)?;
        let repo_ids = usage::registered_repos(&global)?;
        let labels = plan_label::labels(&global);

        // Only repos whose state.db exists: a registration left behind by a
        // deleted repo is skipped, and nothing is created for it.
        let mut repos = Vec::new();
        {
            let mut stmt = global.prepare(
                "SELECT repo_id, COALESCE(root_path, '') FROM repos ORDER BY root_path, repo_id",
            )?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (repo_id, root) = row?;
                let path = paths.state_db(&repo_id);
                if path.is_file() {
                    repos.push((root, usage::read_only(&path)?));
                }
            }
        }

        let mut accounts = Vec::new();
        for profile in profiles {
            let (windows, extra_buckets) = usage::windows_and_extras(paths, &profile, &repo_ids)?;
            let parked_group = if profile.status == "rate_limited" {
                usage::parked_group(&global, &profile.profile_id)?
            } else {
                None
            };
            let mut account = Account {
                parked_group,
                profile,
                windows,
                extra_buckets,
                implementer_runs: 0,
                reviewer_runs: 0,
                busy_seconds: 0,
                last_run: None,
            };
            for (_, conn) in &repos {
                add_work(conn, &mut account)?;
            }
            accounts.push(account);
        }

        let week_ago = now - Duration::days(7);
        let mut counts = Counts::default();
        let mut repo_tasks = Vec::new();
        let mut findings = Vec::new();
        for (root, conn) in &repos {
            let tasks = load_tasks(conn)?;
            for t in &tasks {
                match t.status.as_str() {
                    "queued" | "running" => counts.in_progress += 1,
                    "in_review" => counts.waiting += 1,
                    "unverified" | "interrupted" | "failed" | "reject" => counts.attention += 1,
                    "pass" if parse_time(&t.updated_at).is_some_and(|u| u >= week_ago) => {
                        counts.passed_recently += 1
                    }
                    _ => {}
                }
            }
            let mut grouped = group_tasks(root.clone(), tasks);
            grouped.missing_checks = load_missing_note(conn)?;
            repo_tasks.push(grouped);
            findings.extend(load_findings(conn)?);
        }
        findings.sort_by_key(|f| std::cmp::Reverse(time_key(&f.created_at)));
        findings.truncate(FINDINGS_LIMIT);
        let models = crate::commands::models::load(paths, false)?;

        Ok(Snapshot {
            accounts,
            models,
            scores_auto_refresh: crate::commands::settings::scores_auto_refresh(&paths.home),
            repos: repo_tasks,
            findings,
            counts,
            labels,
        })
    }
}

fn add_work(conn: &Connection, account: &mut Account) -> Result<()> {
    let id = account.profile.profile_id.as_str();
    let mut stmt = conn
        .prepare("SELECT COALESCE(role, ''), started_at, ended_at FROM runs WHERE account = ?1")?;
    let rows = stmt.query_map([id], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
        ))
    })?;
    for row in rows {
        let (role, started, ended) = row?;
        match role.as_str() {
            "implementer" => account.implementer_runs += 1,
            "reviewer" => account.reviewer_runs += 1,
            _ => {}
        }
        let started = started.as_deref().and_then(parse_time);
        let ended = ended.as_deref().and_then(parse_time);
        if let (Some(s), Some(e)) = (started, ended) {
            account.busy_seconds += (e - s).whole_seconds().max(0);
        }
        if let Some(s) = started {
            if account.last_run.is_none_or(|last| s > last) {
                account.last_run = Some(s);
            }
        }
    }
    Ok(())
}

fn load_tasks(conn: &Connection) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT t.task_id, COALESCE(t.title, ''), COALESCE(t.status, ''), COALESCE(t.risk, ''),
                COALESCE(t.updated_at, ''),
                (SELECT MAX(r.attempt) FROM runs r
                  WHERE r.task_id = t.task_id AND r.role = 'implementer'),
                (SELECT v.verdict FROM reviews v JOIN runs r ON r.run_id = v.target_run_id
                  WHERE r.task_id = t.task_id ORDER BY v.created_at DESC LIMIT 1)
         FROM tasks t",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(TaskRow {
            id: r.get(0)?,
            title: r.get(1)?,
            status: r.get(2)?,
            risk: r.get(3)?,
            updated_at: r.get(4)?,
            attempts: r.get(5)?,
            verdict: r.get(6)?,
            newest_run: None,
        })
    })?;
    let mut tasks = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    let mut newest = conn.prepare(
        "SELECT run_id, COALESCE(account, ''), COALESCE(model, ''), interrupted_reason,
                harness_session_id IS NOT NULL
         FROM runs WHERE task_id = ?1 ORDER BY run_id DESC LIMIT 1",
    )?;
    for t in &mut tasks {
        t.newest_run = newest
            .query_row([&t.id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)))
            .optional()?;
    }
    Ok(tasks)
}

fn group_tasks(root: String, tasks: Vec<TaskRow>) -> RepoTasks {
    let (mut open, mut closed): (Vec<_>, Vec<_>) = tasks
        .into_iter()
        .partition(|t| !matches!(t.status.as_str(), "pass" | "cancelled" | "accepted"));
    open.sort_by_key(|t| std::cmp::Reverse(time_key(&t.updated_at)));
    closed.sort_by_key(|t| std::cmp::Reverse(time_key(&t.updated_at)));
    let hidden_closed = closed.len().saturating_sub(CLOSED_TASK_LIMIT);
    closed.truncate(CLOSED_TASK_LIMIT);
    RepoTasks {
        root,
        open,
        closed,
        hidden_closed,
        missing_checks: None,
    }
}

fn load_missing_note(conn: &Connection) -> Result<Option<MissingNote>> {
    let Some(e) = state::latest_event_of_type(conn, event_type::VERIFY_RAN, None)? else {
        return Ok(None);
    };
    let d: serde_json::Value = serde_json::from_str(&e.detail).unwrap_or_default();
    let items: Vec<(String, String)> = d["missing_since_last_verify"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|m| {
                    (
                        m["check_id"].as_str().unwrap_or("").to_string(),
                        m["now"].as_str().unwrap_or("removed").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    if items.is_empty() {
        return Ok(None);
    }
    Ok(Some(MissingNote {
        run_id: e.run_id.unwrap_or_default(),
        ts: e.ts,
        items,
    }))
}

fn load_findings(conn: &Connection) -> Result<Vec<Finding>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(r.task_id, ''), COALESCE(t.title, ''), v.verdict,
                COALESCE(v.findings, ''), COALESCE(v.created_at, '')
         FROM reviews v
         JOIN runs r ON r.run_id = v.target_run_id
         LEFT JOIN tasks t ON t.task_id = r.task_id
         WHERE v.verdict IN ('REJECT', 'UNVERIFIED')",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (task_id, task_title, verdict, findings, created_at) = row?;
        let findings: Value = serde_json::from_str(&findings).unwrap_or(Value::Null);
        let text =
            |v: &Value, key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        let unmet = findings
            .get("criteria")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            // `review record` trims verdicts before checking them, so do the same.
            .filter(|c| c.get("verdict").and_then(Value::as_str).map(str::trim) == Some("unmet"))
            .map(|c| (text(c, "id"), text(c, "note")))
            .collect();
        let extra = findings
            .get("extra_findings")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|e| (text(e, "severity"), text(e, "note")))
            .collect();
        out.push(Finding {
            task_id,
            task_title,
            verdict,
            created_at,
            unmet,
            extra,
        });
    }
    Ok(out)
}

fn parse_time(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

/// Sort key for a stored timestamp: parsed, so fractional seconds of
/// different lengths still order correctly; unparseable values sort last.
fn time_key(s: &str) -> Option<OffsetDateTime> {
    parse_time(s)
}

// ---------------------------------------------------------------- labels

fn status_label(status: &str) -> (&str, &'static str) {
    match status {
        "draft" => ("草稿", "muted"),
        "queued" => ("排隊中", "run"),
        "running" => ("進行中", "run"),
        "in_review" => ("等審查", "wait"),
        "pass" => ("通過", "ok"),
        "reject" => ("退件", "bad"),
        "unverified" => ("無法驗證", "bad"),
        "interrupted" => ("中斷", "bad"),
        "cancelled" => ("已取消", "muted"),
        "failed" => ("失敗", "bad"),
        "accepted" => ("已收件", "ok"),
        other => (other, "muted"),
    }
}

fn risk_label(risk: &str) -> &str {
    match risk {
        "low" => "低",
        "normal" => "一般",
        "important" => "重要",
        "high" => "高",
        other => other,
    }
}

fn verdict_label(verdict: Option<&str>) -> &str {
    match verdict {
        None => "—",
        Some("PASS") => "通過",
        Some("REJECT") => "退件",
        Some("UNVERIFIED") => "無法驗證",
        Some(other) => other,
    }
}

/// FC-1: `parked_group` names the antigravity quota group a `rate_limited`
/// profile was parked for.
fn profile_status_label(status: &str, parked_group: Option<&str>) -> (String, &'static str) {
    match (status, parked_group) {
        ("active", _) => ("可用".into(), "ok"),
        ("rate_limited", Some(group)) => (format!("撞到額度（{group}）"), "bad"),
        ("rate_limited", None) => ("撞到額度".into(), "bad"),
        ("logged_out", _) => ("已登出".into(), "bad"),
        (other, _) => (other.into(), "muted"),
    }
}

/// FC-1: the next step for an interrupted or failed task, from its newest
/// run: (run id, profile, interrupted reason, whether it has a session).
fn next_step_text(
    run_id: &str,
    profile: &str,
    reason: Option<&str>,
    has_session: bool,
) -> String {
    let resume = |before: &str, after: &str| {
        if has_session {
            format!("{before}執行 thoth resume {run_id}{after}")
        } else {
            format!("{before}重新派工{after}")
        }
    };
    match reason {
        Some("quota") => resume(&format!("額度用完（{profile}）：等額度重置後"), "，或改派別的帳號"),
        Some("auth") => resume(
            &format!("登入失效（{profile}）：先執行 thoth profile login {profile}，再"),
            "",
        ),
        Some("transient") => resume(
            "暫時性問題（網路或服務忙）：",
            if has_session { " 再試一次" } else { "再試一次" },
        ),
        Some("crash") => resume("worker 當掉：", ""),
        Some("timeout") => "超過時間上限：加大 --timeout 重新派工".to_string(),
        _ => "執行失敗：看 error_text 後重新派工".to_string(),
    }
}

fn window_label(name: &str) -> &str {
    match name {
        "five_hour" => "5 小時",
        "seven_day" => "一週",
        other => other,
    }
}

fn severity_label(severity: &str) -> &str {
    match severity {
        "important" => "重要",
        "minor" => "小",
        other => other,
    }
}

// ---------------------------------------------------------------- formatting

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

/// `YYYY-MM-DD HH:MM +08:00` in the machine's time zone at that instant, or
/// `YYYY-MM-DD HH:MM UTC` when the local offset cannot be determined.
fn show_time(t: OffsetDateTime) -> String {
    let (local, label) = match UtcOffset::local_offset_at(t) {
        Ok(offset) => (t.to_offset(offset), offset_label(offset)),
        Err(_) => (t.to_offset(UtcOffset::UTC), "UTC".to_string()),
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} {label}",
        local.year(),
        u8::from(local.month()),
        local.day(),
        local.hour(),
        local.minute()
    )
}

fn offset_label(offset: UtcOffset) -> String {
    let sign = if offset.is_negative() { '-' } else { '+' };
    format!(
        "{sign}{:02}:{:02}",
        offset.whole_hours().unsigned_abs(),
        offset.minutes_past_hour().unsigned_abs()
    )
}

/// A stored timestamp for display; an unparseable value is shown as stored.
fn show_stored(s: &str) -> String {
    match parse_time(s) {
        Some(t) => show_time(t),
        None if s.is_empty() => "不明".to_string(),
        None => s.to_string(),
    }
}

fn show_epoch(secs: i64) -> String {
    OffsetDateTime::from_unix_timestamp(secs)
        .map(show_time)
        .unwrap_or_else(|_| "不明".to_string())
}

fn show_duration(seconds: i64) -> String {
    let minutes = seconds / 60;
    match minutes {
        0 => "不到 1 分鐘".to_string(),
        m if m < 60 => format!("{m} 分鐘"),
        m if m % 60 == 0 => format!("{} 小時", m / 60),
        m => format!("{} 小時 {} 分鐘", m / 60, m % 60),
    }
}

fn percent(used: f64) -> String {
    format!("{:.0}%", used.clamp(0.0, 100.0))
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

// ---------------------------------------------------------------- page

const STYLE: &str = r#"
:root {
  color-scheme: light dark;
  --paper: #f3f5f8;
  --surface: #ffffff;
  --ink: #1b2433;
  --muted: #5d6b80;
  --rule: #d8dee8;
  --track: #e3e8f0;
  --lapis: #2a4f9a;
  --malachite: #2e7d5b;
  --ochre: #9a6515;
  --cinnabar: #b3412e;
}
@media (prefers-color-scheme: dark) {
  :root {
    --paper: #0f1726;
    --surface: #16213a;
    --ink: #e6ecf5;
    --muted: #9aa8bf;
    --rule: #2a3754;
    --track: #24314d;
    --lapis: #8fb0f0;
    --malachite: #6cc59a;
    --ochre: #e3b45a;
    --cinnabar: #f0876f;
  }
}
* { box-sizing: border-box; }
html { -webkit-text-size-adjust: 100%; }
body {
  margin: 0;
  background: var(--paper);
  color: var(--ink);
  font-family: "Noto Sans TC", "PingFang TC", "Microsoft JhengHei", system-ui, sans-serif;
  font-size: 16px;
  line-height: 1.65;
  font-variant-numeric: tabular-nums;
}
main { max-width: 46rem; margin: 0 auto; padding: 2rem 16px 4rem; }
h1 { font-size: 1.1rem; font-weight: 600; color: var(--lapis); margin: 0 0 1.5rem; letter-spacing: .02em; }
h2 { font-size: 1.25rem; font-weight: 700; margin: 3rem 0 1rem; padding-bottom: .4rem; border-bottom: 2px solid var(--ink); }
h3 { font-size: .95rem; font-weight: 600; color: var(--muted); margin: 1.75rem 0 .5rem; overflow-wrap: anywhere; }
p { margin: .25rem 0; }
code { font-family: ui-monospace, "Cascadia Mono", monospace; font-size: .85em; color: var(--muted); }
.lede { font-size: 1.35rem; line-height: 1.9; font-weight: 500; }
.lede .n { font-size: 2.1rem; font-weight: 700; margin-right: .1em; }
.n.run { color: var(--lapis); }
.n.wait { color: var(--ochre); }
.n.bad { color: var(--cinnabar); }
.n.ok { color: var(--malachite); }
.note { color: var(--muted); font-size: .9rem; margin-top: .75rem; }
.note.bad { color: var(--cinnabar); }
.pill { display: inline-block; flex: none; padding: 0 .55em; border-radius: .3rem; font-size: .8rem; font-weight: 600; line-height: 1.7; border: 1.5px solid currentColor; }
.pill.run { color: var(--lapis); }
.pill.wait { color: var(--ochre); }
.pill.bad { color: var(--cinnabar); }
.pill.ok { color: var(--malachite); }
.pill.muted { color: var(--muted); }
.acct { background: var(--surface); border-left: 4px solid var(--lapis); padding: .9rem 1rem; margin: 0 0 1rem; }
.acct-head { display: flex; flex-wrap: wrap; align-items: baseline; gap: .5rem .75rem; }
.acct-name { font-weight: 700; font-size: 1.05rem; }
.acct-prov { color: var(--muted); font-size: .9rem; }
.quota { list-style: none; margin: .6rem 0 .4rem; padding: 0; }
.quota li { display: flex; flex-wrap: wrap; align-items: center; gap: .15rem .75rem; margin: .45rem 0; }
.wname { width: 4.5rem; flex: none; font-size: .9rem; }
.meter { flex: 1 1 8rem; height: .55rem; background: var(--track); border-radius: 1rem; overflow: hidden; }
.fill { display: block; height: 100%; border-radius: 1rem; }
.fill.ok { background: var(--malachite); }
.fill.wait { background: var(--ochre); }
.fill.bad { background: var(--cinnabar); }
.pct { flex: none; width: 3.5rem; text-align: right; font-weight: 700; }
.wdetail { flex-basis: 100%; color: var(--muted); font-size: .85rem; padding-left: 5.25rem; }
.extra { color: var(--muted); font-size: .85rem; }
.work, .empty { color: var(--muted); font-size: .9rem; }
.tasks { list-style: none; margin: 0; padding: 0; }
.task { display: flex; gap: .75rem; align-items: baseline; padding: .6rem 0; border-top: 1px solid var(--rule); }
.task:first-child { border-top: 0; }
.ttitle { font-weight: 500; overflow-wrap: anywhere; }
.meta { display: flex; flex-wrap: wrap; gap: .1rem 1rem; color: var(--muted); font-size: .85rem; }
.next { color: var(--muted); font-size: .85rem; overflow-wrap: anywhere; }
.closed .ttitle { color: var(--muted); font-weight: 400; }
.finding { border-left: 4px solid var(--cinnabar); background: var(--surface); padding: .8rem 1rem; margin: 0 0 1rem; }
.fhead { display: flex; gap: .6rem; align-items: baseline; flex-wrap: wrap; }
.ftitle { font-weight: 600; overflow-wrap: anywhere; }
.finding ul { margin: .5rem 0 0; padding-left: 1.2rem; }
.finding li { margin: .3rem 0; overflow-wrap: anywhere; }
.scroll { overflow-x: auto; }
.dt { width: 100%; border-collapse: collapse; font-size: .85rem; margin: .5rem 0 1rem; }
.dt th, .dt td { text-align: left; vertical-align: top; padding: .35rem .5rem; border-top: 1px solid var(--rule); }
.dt th { color: var(--muted); font-weight: 600; }
.dt td.num { text-align: right; }
.dt .meta { display: block; }
@media (max-width: 30rem) {
  .lede { font-size: 1.15rem; }
  .lede .n { font-size: 1.7rem; }
  .wdetail { padding-left: 0; }
}
"#;

fn render(s: &Snapshot, now: OffsetDateTime) -> String {
    let mut h = String::new();
    h.push_str("<!doctype html>\n<html lang=\"zh-Hant\">\n<head>\n<meta charset=\"utf-8\">\n");
    h.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    h.push_str("<title>Thoth 狀態</title>\n<style>");
    h.push_str(STYLE);
    h.push_str("</style>\n</head>\n<body>\n<main>\n<h1>Thoth 狀態</h1>\n");
    render_overview(&mut h, s, now);
    render_accounts(&mut h, s);
    render_decision_table(&mut h, s);
    render_tasks(&mut h, s);
    render_findings(&mut h, s);
    h.push_str("</main>\n</body>\n</html>\n");
    h
}

fn render_overview(h: &mut String, s: &Snapshot, now: OffsetDateTime) {
    let c = &s.counts;
    h.push_str("<section>\n<h2>總覽</h2>\n");
    let _ = writeln!(
        h,
        "<p class=\"lede\"><b class=\"n run\">{}</b> 件進行中、<b class=\"n wait\">{}</b> 件等審查、\
         <b class=\"n bad\">{}</b> 件需要處理、<b class=\"n ok\">{}</b> 件最近 7 天通過。</p>",
        c.in_progress, c.waiting, c.attention, c.passed_recently
    );
    let _ = writeln!(
        h,
        "<p class=\"note\">產生時間 {}，內容是產生當下的狀態。預設位置的這頁會在任務或工作狀態改變後自動重寫，重新整理瀏覽器就能看到最新的。</p>",
        show_time(now)
    );
    h.push_str("</section>\n");
}

fn render_accounts(h: &mut String, s: &Snapshot) {
    h.push_str("<section>\n<h2>帳號</h2>\n");
    if s.accounts.is_empty() {
        h.push_str("<p class=\"empty\">還沒有登記任何帳號。</p>\n");
    }
    for a in &s.accounts {
        let (status, class) = profile_status_label(&a.profile.status, a.parked_group.as_deref());
        h.push_str("<div class=\"acct\">\n<div class=\"acct-head\">");
        let _ = write!(
            h,
            "<span class=\"acct-name\">{}</span><span class=\"acct-prov\">{}</span><span class=\"pill {class}\">{}</span>",
            esc(&s.named(&a.profile.profile_id)),
            esc(&a.profile.provider),
            esc(&status)
        );
        h.push_str("</div>\n");
        if a.windows.is_empty() {
            h.push_str("<p class=\"empty\">沒有額度資料</p>\n");
        } else {
            h.push_str("<ul class=\"quota\">\n");
            for w in &a.windows {
                render_window(h, w);
            }
            h.push_str("</ul>\n");
        }
        if !a.extra_buckets.is_empty() {
            h.push_str("<ul class=\"quota\">\n");
            for b in &a.extra_buckets {
                render_extra_bucket(h, b);
            }
            h.push_str("</ul>\n");
        }
        let _ = writeln!(h, "<p class=\"work\">{}</p>\n</div>", work_line(a));
    }
    h.push_str("</section>\n");
}

fn task_type_label(task_type: Option<&str>) -> &str {
    match task_type {
        Some("code") => "寫程式",
        Some("research") => "找資料",
        Some("review") => "審查",
        Some("docs") => "文件",
        Some("mechanical") => "機械活",
        _ => "未標",
    }
}

fn role_label(role: &str) -> &str {
    match role {
        "implementer" => "實作者",
        "reviewer" => "審查者",
        other => other,
    }
}

/// What became of a run: its review verdict, else its status.
fn outcome_label(verdict: Option<&str>, status: &str) -> String {
    match verdict {
        Some(_) => verdict_label(verdict).to_string(),
        None if status == "completed" => "做完，還沒審".to_string(),
        None => status_label(status).0.to_string(),
    }
}

/// `1234567` → `1,234,567`.
fn thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 {
        out.insert(0, '-');
    }
    out
}

/// One score source's fetch date and staleness; no link without a cache.
fn render_score_source(
    h: &mut String,
    id: &str,
    name: &str,
    url: &str,
    present: bool,
    fetched_at: Option<&str>,
) {
    let stale_pill = "<span class=\"pill bad\">過期</span>";
    match fetched_at.filter(|_| present) {
        Some(fetched_at) => {
            let (age, stale) = crate::commands::models::staleness(Some(fetched_at));
            let age = age.map_or_else(|| "不明".to_string(), |a| a.to_string());
            let _ = writeln!(
                h,
                "<p class=\"note\" id=\"{id}\">跑分來源 <a href=\"{url}\">{name}</a>：取得於 {}（{age} 小時前）{}</p>",
                esc(&show_stored(fetched_at)),
                if stale { stale_pill } else { "" }
            );
        }
        None => {
            let _ = writeln!(h, "<p class=\"note\" id=\"{id}\">跑分來源 {name}：沒有跑分資料{stale_pill}</p>");
        }
    }
}

fn render_decision_table(h: &mut String, s: &Snapshot) {
    let m = &s.models;
    h.push_str("<section id=\"decision-table\">\n<h2>派工決策表</h2>\n");
    render_score_source(
        h,
        "scores-asl",
        "AI Stupid Level",
        "https://aistupidlevel.info",
        m.cache_present,
        m.fetched_at.as_deref(),
    );
    render_score_source(
        h,
        "scores-aa",
        "Artificial Analysis",
        "https://artificialanalysis.ai",
        m.aa_cache_present,
        m.aa_fetched_at.as_deref(),
    );
    let _ = writeln!(
        h,
        "<p class=\"note\" id=\"scores-refresh\">跑分自動更新：{}（thoth config set scores-auto-refresh on）</p>",
        if s.scores_auto_refresh { "開" } else { "關" }
    );
    let records = &m.records;
    let machine = esc(&records.machine);
    match (&records.dir, &records.error) {
        (None, _) => {
            let _ = writeln!(h, "<p class=\"note\" id=\"records-note\">紀錄來源：只有這台（{machine}）；沒有設定共用資料夾（thoth config set records-dir 路徑）</p>");
        }
        (Some(dir), Some(e)) => {
            let _ = writeln!(
                h,
                "<p class=\"note bad\" id=\"records-note\">紀錄來源：這台（{machine}）；共用資料夾 {} 讀不到（{}）</p>",
                esc(&dir.to_string_lossy()),
                esc(e)
            );
        }
        (Some(_), None) if records.files.is_empty() => {
            let _ = writeln!(h, "<p class=\"note\" id=\"records-note\">紀錄來源：這台（{machine}）；共用資料夾裡還沒有別台的檔</p>");
        }
        (Some(_), None) => {
            let names: Vec<String> = records.files.iter().map(|f| esc(&f.machine)).collect();
            let _ = writeln!(
                h,
                "<p class=\"note\" id=\"records-note\">紀錄來源：這台（{machine}）加共用資料夾的 {} 台（{}）</p>",
                records.files.len(),
                names.join("、")
            );
        }
    }

    let table = &m.decision_table;
    if table.rows.is_empty() {
        h.push_str("<p class=\"empty\">還沒有派工紀錄。</p>\n");
    } else {
        h.push_str("<div class=\"scroll\"><table class=\"dt\"><thead><tr><th>模型</th><th>帳號</th><th>類型</th><th>effort</th><th>跑過</th><th>審過</th><th>通過率</th><th>平均 token</th><th>平均額度點數（5 小時／一週）</th><th>電腦</th></tr></thead><tbody>\n");
        for r in &table.rows {
            let pass_rate = match r.pass_rate() {
                Some(rate) => format!("{:.0}%（{}/{}）", rate * 100.0, r.pass, r.pass + r.reject),
                None => "—".to_string(),
            };
            let tokens = match r.tokens_avg() {
                Some(v) => thousands(v.round() as i64),
                None => "沒量到".to_string(),
            };
            let points = |avg: Option<f64>, runs: i64| match avg {
                Some(v) => format!("{v:.1}（{runs} 次）"),
                None => "沒量到".to_string(),
            };
            let mut quota = format!(
                "{}／{}",
                points(r.quota_avg(), r.quota_runs),
                points(r.quota_avg_weekly(), r.quota_runs_weekly)
            );
            if r.quota_runs_unreliable > 0 {
                let _ = write!(quota, "；{} 次不準", r.quota_runs_unreliable);
            }
            let _ = writeln!(
                h,
                "<tr class=\"row\"><td>{}</td><td>{}</td><td>{}</td><td>{}</td>\
                 <td class=\"num\">{}</td><td class=\"num\">{}</td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(&r.model),
                esc(&s.named(&r.profile)),
                esc(task_type_label(r.task_type.as_deref())),
                esc(r.effort.as_deref().unwrap_or("沒記")),
                r.implementer_runs,
                r.reviewed,
                esc(&pass_rate),
                esc(&tokens),
                esc(&quota),
                esc(&r.machines.join("、"))
            );
        }
        h.push_str("</tbody></table></div>\n");
    }

    h.push_str("<h3>每次的選擇與結果</h3>\n");
    if table.choices.is_empty() {
        h.push_str("<p class=\"empty\">還沒有派工。</p>\n");
    } else {
        h.push_str("<div class=\"scroll\"><table class=\"dt\"><thead><tr><th>任務</th><th>選擇</th><th>類型・難度</th><th>理由</th><th>結果</th></tr></thead><tbody>\n");
        for c in table.choices.iter().take(CHOICE_LIMIT) {
            let title = c
                .task_title
                .as_deref()
                .filter(|t| !t.is_empty())
                .unwrap_or("（沒有標題）");
            let chosen = format!(
                "{} {} {}",
                s.named(&c.profile),
                c.model,
                c.effort_requested.as_deref().unwrap_or("沒記")
            );
            let kind = format!(
                "{}・{}",
                task_type_label(c.task_type.as_deref()),
                c.difficulty.as_deref().unwrap_or("未標")
            );
            // A spec reviewer's conclusion is not a review verdict, so its
            // run is never "waiting for a review".
            let outcome = if c.role == crate::model::role::SPEC_REVIEWER && c.status == "completed" {
                "做完".to_string()
            } else {
                outcome_label(c.verdict.as_deref(), &c.status)
            };
            let _ = writeln!(
                h,
                "<tr class=\"dev\"><td>{}<span class=\"meta\">{}・{}・{}</span></td><td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>",
                esc(title),
                esc(role_label(&c.role)),
                esc(&c.machine),
                esc(&show_stored(c.started_at.as_deref().unwrap_or(""))),
                esc(&chosen),
                esc(&kind),
                esc(c.reason.as_deref().unwrap_or("沒寫")),
                esc(&outcome)
            );
        }
        h.push_str("</tbody></table></div>\n");
        let hidden = table.choices.len().saturating_sub(CHOICE_LIMIT);
        if hidden > 0 {
            let _ = writeln!(h, "<p class=\"note\">另外還有 {hidden} 次沒有列出。</p>");
        }
    }
    h.push_str("</section>\n");
}

fn render_window(h: &mut String, w: &Window) {
    let name = esc(window_label(&w.name));
    let observed = esc(&show_stored(&w.observed_at));
    if w.is_reset() {
        let reset = w
            .resets_at
            .map(show_epoch)
            .unwrap_or_else(|| "不明".to_string());
        let _ = writeln!(
            h,
            "<li><span class=\"wname\">{name}</span><span class=\"meter\"></span><span class=\"pct\">已重置</span>\
             <span class=\"wdetail\">上次讀到 {}（{observed}），已在 {} 重置</span></li>",
            percent(w.used),
            esc(&reset)
        );
    } else {
        let used = w.used.clamp(0.0, 100.0);
        let class = if used >= 95.0 {
            "bad"
        } else if used >= 70.0 {
            "wait"
        } else {
            "ok"
        };
        let reset = match w.resets_at {
            Some(t) => format!("將在 {} 重置，", esc(&show_epoch(t))),
            None => String::new(),
        };
        let _ = writeln!(
            h,
            "<li><span class=\"wname\">{name}</span><span class=\"meter\"><span class=\"fill {class}\" style=\"width:{used:.0}%\"></span></span>\
             <span class=\"pct\">{}</span><span class=\"wdetail\">{reset}資料讀於 {observed}</span></li>",
            percent(used)
        );
    }
}

/// One muted line per window of a non-general bucket (for example codex's
/// gpt-reserve): informational only, `recommend` does not rank on it.
fn render_extra_bucket(h: &mut String, b: &ExtraBucket) {
    let bucket = b.limit_name.as_deref().unwrap_or(&b.limit_id);
    for w in &b.windows {
        let used = if w.is_reset() {
            "已重置".to_string()
        } else {
            percent(w.used)
        };
        let _ = writeln!(
            h,
            "<li class=\"extra\">{} {}：{}</li>",
            esc(bucket),
            esc(window_label(&w.name)),
            esc(&used)
        );
    }
}

fn work_line(a: &Account) -> String {
    if a.implementer_runs == 0 && a.reviewer_runs == 0 {
        return "還沒有工作紀錄".to_string();
    }
    let mut line = format!(
        "實作 {} 次、審查 {} 次，合計 {}",
        a.implementer_runs,
        a.reviewer_runs,
        show_duration(a.busy_seconds)
    );
    if let Some(last) = a.last_run {
        let _ = write!(line, "，最近一次 {}", show_time(last));
    }
    esc(&line)
}

fn render_tasks(h: &mut String, s: &Snapshot) {
    h.push_str("<section>\n<h2>任務</h2>\n");
    if s.repos.is_empty() {
        h.push_str("<p class=\"empty\">還沒有登記任何 repo。</p>\n");
    }
    for repo in &s.repos {
        let _ = writeln!(h, "<h3>{}</h3>", esc(&repo.root));
        if let Some(m) = &repo.missing_checks {
            let items: Vec<String> = m
                .items
                .iter()
                .map(|(id, now)| {
                    let label = if now == "pending" { "等重新核准" } else { "已移除" };
                    format!("{}（{label}）", esc(id))
                })
                .collect();
            let _ = writeln!(
                h,
                "<p class=\"note bad\">上次驗證有、這次沒有的已核准檢查：{}。（驗證 {}，{}）\
                 只有人能核准檢查；不是你移除的，就問指揮官。</p>",
                items.join("、"),
                esc(&short_id(&m.run_id)),
                esc(&show_stored(&m.ts))
            );
        }
        if repo.open.is_empty() && repo.closed.is_empty() {
            h.push_str("<p class=\"empty\">這個 repo 還沒有任務。</p>\n");
            continue;
        }
        h.push_str("<ul class=\"tasks\">\n");
        for t in &repo.open {
            render_task(h, t, false);
        }
        for t in &repo.closed {
            render_task(h, t, true);
        }
        h.push_str("</ul>\n");
        if repo.hidden_closed > 0 {
            let _ = writeln!(
                h,
                "<p class=\"note\">另外還有 {} 件已結束的任務沒有列出。</p>",
                repo.hidden_closed
            );
        }
    }
    h.push_str("</section>\n");
}

fn render_task(h: &mut String, t: &TaskRow, closed: bool) {
    let (status, class) = status_label(&t.status);
    let title = if t.title.is_empty() {
        "（沒有標題）"
    } else {
        t.title.as_str()
    };
    let attempts = match t.attempts {
        Some(n) => format!("實作 {n} 次"),
        None => "還沒派工".to_string(),
    };
    // FC-1: what to do next, for a task that stopped early.
    let next = match &t.newest_run {
        Some((run_id, profile, _model, reason, has_session))
            if t.status == "interrupted" || t.status == "failed" =>
        {
            format!(
                "<p class=\"next\">{}</p>",
                esc(&next_step_text(run_id, profile, reason.as_deref(), *has_session))
            )
        }
        _ => String::new(),
    };
    let _ = writeln!(
        h,
        "<li class=\"task{}\"><span class=\"pill {class}\">{}</span><div>\
         <p class=\"ttitle\">{}</p><p class=\"meta\"><code>{}</code><span>風險：{}</span><span>{attempts}</span>\
         <span>審查：{}</span><span>更新於 {}</span></p>{next}</div></li>",
        if closed { " closed" } else { "" },
        esc(status),
        esc(title),
        esc(&short_id(&t.id)),
        esc(risk_label(&t.risk)),
        esc(verdict_label(t.verdict.as_deref())),
        esc(&show_stored(&t.updated_at))
    );
}

fn render_findings(h: &mut String, s: &Snapshot) {
    h.push_str("<section>\n<h2>審查抓到的問題</h2>\n");
    if s.findings.is_empty() {
        h.push_str("<p class=\"empty\">目前沒有。</p>\n");
    }
    for f in &s.findings {
        let title = if f.task_title.is_empty() {
            "（沒有標題）"
        } else {
            f.task_title.as_str()
        };
        let _ = writeln!(
            h,
            "<article class=\"finding\"><div class=\"fhead\"><span class=\"pill bad\">{}</span>\
             <span class=\"ftitle\">{}</span><code>{}</code></div><p class=\"note\">{}</p>",
            esc(verdict_label(Some(&f.verdict))),
            esc(title),
            esc(&short_id(&f.task_id)),
            esc(&show_stored(&f.created_at))
        );
        if !f.unmet.is_empty() || !f.extra.is_empty() {
            h.push_str("<ul>\n");
            for (id, note) in &f.unmet {
                let note = if note.is_empty() {
                    "（沒有說明）"
                } else {
                    note.as_str()
                };
                let _ = writeln!(h, "<li><b>{}</b> 沒達成：{}</li>", esc(id), esc(note));
            }
            for (severity, note) in &f.extra {
                let _ = writeln!(
                    h,
                    "<li><b>{}</b> {}</li>",
                    esc(severity_label(severity)),
                    esc(note)
                );
            }
            h.push_str("</ul>\n");
        }
        h.push_str("</article>\n");
    }
    h.push_str("</section>\n");
}

// ---------------------------------------------------------------- output

/// Write to a temporary file beside the target, then rename it over the
/// target, so a reader never sees a half-written page.
fn write_atomically(target: &Path, page: &str) -> Result<()> {
    let dir: PathBuf = match target.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .with_context(|| format!("{} has no file name", target.display()))?
        .to_string_lossy()
        .to_string();
    let tmp = dir.join(format!(".{name}.tmp-{}", std::process::id()));
    fs::write(&tmp, page).with_context(|| format!("writing {}", tmp.display()))?;
    if let Err(e) = fs::rename(&tmp, target) {
        let _ = fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("replacing {}", target.display()));
    }
    Ok(())
}

fn open_in_browser(path: &Path) -> Result<()> {
    use std::process::{Command, Stdio};

    // Explorer opens a file with its default handler without going through
    // cmd.exe, whose parser would split a path at `&`, `|` or `^` and expand
    // `%VAR%`; it is a GUI program, so no console window appears.
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("explorer.exe");
        c.arg(path);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(path);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(path);
        c
    };

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the system's default browser")?;
    Ok(())
}

/// A `file://` URL for `path`: backslashes become slashes, a drive path gets
/// a leading slash, and every byte outside `A–Z a–z 0–9 - . _ ~ / :` is
/// percent-encoded.
fn file_url(path: &Path) -> String {
    let mut text = path.to_string_lossy().replace('\\', "/");
    if !text.starts_with('/') {
        text.insert(0, '/');
    }
    let mut url = String::from("file://");
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b':') {
            url.push(b as char);
        } else {
            let _ = write!(url, "%{b:02X}");
        }
    }
    url
}

/// Opens a URL (with its `#fragment`) in the default browser, without a
/// shell; like `open_in_browser`, it does not wait.
fn open_url(url: &str) -> Result<()> {
    use std::process::{Command, Stdio};

    // explorer.exe drops the `#fragment` of a file URL; the URL protocol
    // handler keeps it.
    #[cfg(windows)]
    let mut cmd = {
        let mut c = Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler").arg(url);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };

    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("starting the system's default browser")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_url_escapes_and_keeps_the_slashes() {
        assert_eq!(
            file_url(Path::new("/home/j/.thoth/report.html")),
            "file:///home/j/.thoth/report.html"
        );
        assert_eq!(
            file_url(Path::new("C:\\Users\\王 五\\report.html")),
            "file:///C:/Users/%E7%8E%8B%20%E4%BA%94/report.html"
        );
        assert_eq!(file_url(Path::new("/a b/c.html")), "file:///a%20b/c.html");
    }
}
