//! The shared records folder (DT-3): one `NAME.jsonl` per computer, one line
//! per run, regenerated whole from every registered repository of this
//! computer. `thoth models` reads the other computers' files from the same
//! folder. Only the fields the decision table needs are written: no spec,
//! prompt, error text, code, conversation or absolute path.

use crate::commands::{settings, usage};
use crate::config::Paths;
use crate::output;
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const LINE_VERSION: u32 = 2;
/// Versions `load_shared` reads: a computer whose thoth is not rebuilt yet
/// still writes v1, whose extra `recommended`, `deviates` and `exempt` are
/// ignored.
const READABLE_VERSIONS: [u32; 2] = [1, 2];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tokens {
    pub input: Option<i64>,
    pub cache_read: Option<i64>,
    pub cache_write: Option<i64>,
    pub output: Option<i64>,
    pub reasoning: Option<i64>,
    pub total: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quota {
    pub five_hour_points: Option<f64>,
    pub seven_day_points: Option<f64>,
    pub reliable: bool,
    #[serde(default)]
    pub unreliable_reasons: Vec<String>,
    pub source: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line {
    pub v: u32,
    pub machine: String,
    pub repo_id: String,
    pub repo: Option<String>,
    pub run_id: String,
    pub task_id: Option<String>,
    pub task_title: Option<String>,
    pub attempt: Option<i64>,
    pub role: String,
    pub profile: String,
    pub provider: String,
    pub harness: Option<String>,
    pub model: String,
    pub effort_requested: Option<String>,
    pub effort: Option<String>,
    pub task_type: Option<String>,
    pub difficulty: Option<String>,
    pub risk: Option<String>,
    pub status: String,
    pub interrupted_reason: Option<String>,
    pub exit_code: Option<i64>,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub verdict: Option<String>,
    pub tokens: Tokens,
    pub quota: Option<Quota>,
    /// Why this profile, model and effort were chosen: the `reason` of the
    /// run's newest `launch_choice`.
    pub reason: Option<String>,
}

/// The columns of a `runs` row this file needs; every one may be NULL.
#[derive(Debug, Clone, Default)]
pub(crate) struct RunRow {
    pub(crate) run_id: Option<String>,
    pub(crate) task_id: Option<String>,
    pub(crate) attempt: Option<i64>,
    pub(crate) role: Option<String>,
    pub(crate) provider: Option<String>,
    pub(crate) account: Option<String>,
    pub(crate) model: Option<String>,
    pub(crate) spec: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) interrupted_reason: Option<String>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) started_at: Option<String>,
    pub(crate) ended_at: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TaskRow {
    pub(crate) title: Option<String>,
    pub(crate) spec: Option<String>,
    pub(crate) risk: Option<String>,
}

/// The last path component of a repository root, tolerating a trailing
/// separator of either kind; never more than that.
fn repo_name(root_path: Option<&str>) -> Option<String> {
    let trimmed = root_path?.trim_end_matches(['/', '\\']);
    if trimmed.is_empty() {
        return None;
    }
    // A Windows path read on Unix has no `/` to split on, so split by hand.
    let last = trimmed.rsplit(['/', '\\']).next()?;
    let name = Path::new(last).file_name()?.to_str()?;
    (!name.is_empty()).then(|| name.to_string())
}

fn str_key(v: Option<&Value>, key: &str) -> Option<String> {
    v?.get(key)?.as_str().map(str::to_string)
}

fn spec_key(spec: Option<&str>, key: &str, allowed: &[&str]) -> Option<String> {
    let value: Value = serde_json::from_str(spec?).ok()?;
    let s = value.get(key)?.as_str()?;
    allowed.contains(&s).then(|| s.to_string())
}

const TASK_TYPES: [&str; 5] = ["code", "research", "review", "docs", "mechanical"];
const DIFFICULTIES: [&str; 3] = ["low", "normal", "high"];

/// Puts the pieces read from the databases together; no I/O.
#[allow(clippy::too_many_arguments)]
pub(crate) fn line_from_parts(
    machine: &str,
    repo_id: &str,
    root_path: Option<&str>,
    run: &RunRow,
    task: Option<&TaskRow>,
    harness: Option<&str>,
    launch_choice: Option<&Value>,
    run_quota: Option<&Value>,
    verdict: Option<&str>,
    tokens: Tokens,
) -> Line {
    let launch = run.spec.as_deref().and_then(crate::adapter::effort::parse_launch);
    let (effort_requested, effort) = match launch {
        Some(l) => (l.effort_requested, l.effort),
        None => (None, None),
    };
    let task_type = str_key(launch_choice, "task_type")
        .or_else(|| spec_key(task.and_then(|t| t.spec.as_deref()), "task_type", &TASK_TYPES));
    let difficulty = str_key(launch_choice, "difficulty")
        .or_else(|| spec_key(task.and_then(|t| t.spec.as_deref()), "difficulty", &DIFFICULTIES));
    let risk = str_key(launch_choice, "risk").or_else(|| task.and_then(|t| t.risk.clone()));
    let quota = run_quota.map(|d| Quota {
        five_hour_points: d.get("points").and_then(Value::as_f64),
        seven_day_points: d.pointer("/seven_day/points").and_then(Value::as_f64),
        reliable: d.get("reliable").and_then(Value::as_bool).unwrap_or(true),
        unreliable_reasons: d
            .get("unreliable_reasons")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
            .unwrap_or_default(),
        source: str_key(Some(d), "source"),
    });
    Line {
        v: LINE_VERSION,
        machine: machine.to_string(),
        repo_id: repo_id.to_string(),
        repo: repo_name(root_path),
        run_id: run.run_id.clone().unwrap_or_default(),
        task_id: run.task_id.clone(),
        task_title: task.and_then(|t| t.title.clone()),
        attempt: run.attempt,
        role: run.role.clone().unwrap_or_default(),
        profile: run.account.clone().unwrap_or_default(),
        provider: run.provider.clone().unwrap_or_default(),
        harness: harness.map(str::to_string),
        model: run.model.clone().unwrap_or_default(),
        effort_requested,
        effort,
        task_type,
        difficulty,
        risk,
        status: run.status.clone().unwrap_or_default(),
        interrupted_reason: run.interrupted_reason.clone(),
        exit_code: run.exit_code,
        started_at: run.started_at.clone(),
        ended_at: run.ended_at.clone(),
        verdict: verdict.map(str::to_string),
        tokens,
        quota,
        reason: str_key(launch_choice, "reason"),
    }
}

fn latest_event(conn: &Connection, run_id: &str, kind: &str) -> Option<Value> {
    let detail: Option<String> = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id=?1 AND type=?2 ORDER BY seq DESC LIMIT 1",
            [run_id, kind],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()
        .ok()
        .flatten()
        .flatten();
    serde_json::from_str(&detail?).ok()
}

fn latest_verdict(conn: &Connection, role: &str, run_id: &str) -> Option<String> {
    let column = if role == "reviewer" { "reviewer_run_id" } else { "target_run_id" };
    conn.query_row(
        &format!(
            "SELECT verdict FROM reviews WHERE {column}=?1 ORDER BY created_at DESC, review_id DESC LIMIT 1"
        ),
        [run_id],
        |r| r.get::<_, Option<String>>(0),
    )
    .optional()
    .ok()
    .flatten()
    .flatten()
}

/// Every run of this computer, from every registered repository, as lines.
pub fn collect(paths: &Paths) -> Result<Vec<Line>> {
    Ok(collect_counted(paths)?.0.into_iter().map(|(line, _)| line).collect())
}

/// A line and the `previous_run` of its run's newest `launch_choice`.
pub(crate) type LineWithPrevious = (Line, Option<String>);

/// `collect`, each line with the `previous_run` of its run's newest
/// `launch_choice` (null when missing or not a string). The decision table
/// shows it for this computer's runs; the line format does not carry it.
pub(crate) fn collect_with_previous_run(paths: &Paths) -> Result<Vec<LineWithPrevious>> {
    Ok(collect_counted(paths)?.0)
}

/// `collect_with_previous_run`, plus how many repositories had a state
/// database to read.
fn collect_counted(paths: &Paths) -> Result<(Vec<LineWithPrevious>, usize)> {
    let machine = settings::machine_name(&paths.home);
    let global = usage::global_read_only(paths)?;
    let repos = usage::registered_repos(&global)?;
    let mut harnesses: HashMap<String, Option<String>> = HashMap::new();
    let mut lines = Vec::new();
    let mut read_repos = 0;
    for repo_id in &repos {
        let state = paths.state_db(repo_id);
        if !state.is_file() {
            continue;
        }
        read_repos += 1;
        let root_path: Option<String> = global
            .query_row("SELECT root_path FROM repos WHERE repo_id=?1", [repo_id], |r| {
                r.get::<_, Option<String>>(0)
            })
            .optional()?
            .flatten();
        let conn = usage::read_only(&state)?;
        // A repository not yet opened by a version with DT-1 has no token-kind
        // columns; they read as NULL. Asked once per connection.
        let col = |name: &str| {
            if crate::db::migrations::column_exists(&conn, "runs", name).unwrap_or(false) {
                format!("r.{name}")
            } else {
                "NULL".to_string()
            }
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT r.run_id, r.task_id, r.attempt, r.role, r.provider, r.account, r.model,
                    r.spec, r.status, r.interrupted_reason, r.exit_code, r.started_at, r.ended_at,
                    r.input_tokens, {}, {}, r.output_tokens, {}, {},
                    t.task_id, t.title, t.spec, t.risk
             FROM runs r LEFT JOIN tasks t ON t.task_id=r.task_id",
            col("cache_read_tokens"),
            col("cache_write_tokens"),
            col("reasoning_tokens"),
            col("total_tokens"),
        ))?;
        let rows = stmt.query_map([], |r| {
            let run = RunRow {
                run_id: r.get(0)?,
                task_id: r.get(1)?,
                attempt: r.get(2)?,
                role: r.get(3)?,
                provider: r.get(4)?,
                account: r.get(5)?,
                model: r.get(6)?,
                spec: r.get(7)?,
                status: r.get(8)?,
                interrupted_reason: r.get(9)?,
                exit_code: r.get(10)?,
                started_at: r.get(11)?,
                ended_at: r.get(12)?,
            };
            let tokens = Tokens {
                input: r.get(13)?,
                cache_read: r.get(14)?,
                cache_write: r.get(15)?,
                output: r.get(16)?,
                reasoning: r.get(17)?,
                total: r.get(18)?,
            };
            let has_task = r.get::<_, Option<String>>(19)?.is_some();
            let task = TaskRow { title: r.get(20)?, spec: r.get(21)?, risk: r.get(22)? };
            Ok((run, tokens, has_task.then_some(task)))
        })?;
        for row in rows {
            let (run, tokens, task) = row?;
            let run_id = run.run_id.clone().unwrap_or_default();
            let role = run.role.clone().unwrap_or_default();
            let account = run.account.clone().unwrap_or_default();
            let harness = match harnesses.get(&account) {
                Some(h) => h.clone(),
                None => {
                    let h = crate::db::global::get_profile(&global, &account)
                        .ok()
                        .flatten()
                        .map(|p| p.harness);
                    harnesses.insert(account.clone(), h.clone());
                    h
                }
            };
            let launch_choice = latest_event(&conn, &run_id, crate::model::event_type::LAUNCH_CHOICE);
            let run_quota = latest_event(&conn, &run_id, crate::model::event_type::RUN_QUOTA);
            let verdict = latest_verdict(&conn, &role, &run_id);
            let previous_run = str_key(launch_choice.as_ref(), "previous_run");
            let line = line_from_parts(
                &machine,
                repo_id,
                root_path.as_deref(),
                &run,
                task.as_ref(),
                harness.as_deref(),
                launch_choice.as_ref(),
                run_quota.as_ref(),
                verdict.as_deref(),
                tokens,
            );
            lines.push((line, previous_run));
        }
    }
    lines.sort_by(|(a, _), (b, _)| a.run_id.cmp(&b.run_id));
    Ok((lines, read_repos))
}

pub struct Export {
    pub path: PathBuf,
    pub lines: usize,
    pub repos: usize,
}

/// Regenerates `records-dir/NAME.jsonl` from this computer's databases.
pub fn export(paths: &Paths) -> Result<Export> {
    let Some(dir) = settings::records_dir(&paths.home) else {
        bail!("records-dir is not set; run `thoth config set records-dir PATH` with the absolute path of the shared folder")
    };
    if !std::fs::metadata(&dir).is_ok_and(|m| m.is_dir()) {
        bail!(
            "records-dir {} is not a directory; create it (clone the records repository there) or change it with `thoth config set records-dir PATH`",
            dir.display()
        )
    }
    let (lines, repos) = collect_counted(paths)?;
    // The records folder is a shared git repository: masked before it is
    // written (RD-1). A line with nothing masked keeps `Line`'s field order;
    // a masked `Value` would list its keys by name.
    let masker = crate::redact::Masker::load(Some(&paths.home));
    let mut text = String::new();
    for (line, _) in &lines {
        let mut value = serde_json::to_value(line)?;
        if masker.mask_value(&mut value).is_empty() {
            text.push_str(&serde_json::to_string(line)?);
        } else {
            text.push_str(&value.to_string());
        }
        text.push('\n');
    }
    let machine = settings::machine_name(&paths.home);
    let path = dir.join(format!("{machine}.jsonl"));
    let temp = dir.join(format!("{machine}.jsonl.tmp-{}", std::process::id()));
    std::fs::write(&temp, text).with_context(|| format!("cannot write {}", temp.display()))?;
    if let Err(e) = std::fs::rename(&temp, &path) {
        let _ = std::fs::remove_file(&temp);
        return Err(e).with_context(|| format!("cannot atomically replace {}", path.display()));
    }
    Ok(Export { path, lines: lines.len(), repos })
}

/// For the runner: writes nothing when records-dir is unset, logs a failure
/// to `runner-error.log`, and never fails.
pub fn export_quietly(paths: &Paths, run_dir: &Path) {
    if settings::records_dir(&paths.home).is_none() {
        return;
    }
    if let Err(e) = export(paths) {
        crate::runner::log_runner_error(run_dir, &format!("records export failed: {e:#}"));
    }
}

/// `thoth records export [--json]`.
pub fn export_command(paths: &Paths, json_mode: bool) -> Result<i32> {
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!("records export is only for the human operator or the commander; it is refused while THOTH_RUN_ID is set")
    }
    let done = export(paths)?;
    let machine = settings::machine_name(&paths.home);
    let path = done.path.display().to_string();
    output::emit(
        json_mode,
        &json!({"ok": true, "path": path, "machine": machine, "lines": done.lines, "repos": done.repos}),
        &format!("wrote {} run(s) from {} repositor(y/ies) to {path}", done.lines, done.repos),
    );
    Ok(0)
}

pub struct SharedFile {
    pub name: String,
    pub machine: String,
    pub lines: usize,
    pub skipped_lines: usize,
}

pub struct Shared {
    pub dir: Option<PathBuf>,
    pub machine: String,
    pub files: Vec<SharedFile>,
    pub lines: Vec<Line>,
    pub error: Option<String>,
}

/// The other computers' lines from the records folder; never fails.
pub fn load_shared(paths: &Paths) -> Shared {
    let machine = settings::machine_name(&paths.home);
    let mut shared = Shared {
        dir: settings::records_dir(&paths.home),
        machine: machine.clone(),
        files: Vec::new(),
        lines: Vec::new(),
        error: None,
    };
    let Some(dir) = shared.dir.clone() else { return shared };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            shared.error = Some(e.to_string());
            return shared;
        }
    };
    let own = format!("{machine}.jsonl");
    let mut names: Vec<String> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.ends_with(".jsonl") && *n != own)
        .collect();
    names.sort();
    for name in names {
        let Ok(bytes) = std::fs::read(dir.join(&name)) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        // The names were filtered on this suffix, so exactly one is removed.
        let mut file = SharedFile {
            machine: name.strip_suffix(".jsonl").unwrap_or(&name).to_string(),
            name,
            lines: 0,
            skipped_lines: 0,
        };
        // A blank line is not a line of the format: it is skipped and counted.
        for raw in text.lines() {
            match serde_json::from_str::<Line>(raw) {
                Ok(line) if READABLE_VERSIONS.contains(&line.v) => {
                    file.lines += 1;
                    shared.lines.push(line);
                }
                _ => file.skipped_lines += 1,
            }
        }
        shared.files.push(file);
    }
    shared
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_run() -> RunRow {
        RunRow {
            run_id: Some("R1".into()),
            task_id: Some("T1".into()),
            attempt: Some(2),
            role: Some("implementer".into()),
            provider: Some("anthropic".into()),
            account: Some("claude-b".into()),
            model: Some("sonnet".into()),
            spec: Some(r#"{"launch":{"effort_requested":"high","effort":"high"}}"#.into()),
            status: Some("completed".into()),
            exit_code: Some(0),
            ..RunRow::default()
        }
    }

    #[test]
    fn a_line_is_built_from_the_parts_and_leaves_out_everything_private() {
        let task = TaskRow {
            title: Some("t".into()),
            spec: Some(r#"{"task_type":"docs","difficulty":"low"}"#.into()),
            risk: Some("normal".into()),
        };
        let choice: Value = serde_json::from_str(
            r#"{"task_type":"code","difficulty":null,"risk":"low","recommended":{"profile":"codex-a","model":"gpt-5.6-luna","effort_requested":"medium","effort":"medium"},"deviates":true,"exempt":null,"reason":"codex is busy"}"#,
        )
        .unwrap();
        let quota: Value = serde_json::from_str(
            r#"{"points":1.5,"seven_day":{"points":0.5},"reliable":false,"unreliable_reasons":["shared"],"source":"claude_stream"}"#,
        )
        .unwrap();
        let tokens = Tokens { input: Some(34), ..Tokens::default() };
        let line = line_from_parts(
            "m",
            "abc",
            Some("/home/u/Thoth"),
            &sample_run(),
            Some(&task),
            Some("claude-code"),
            Some(&choice),
            Some(&quota),
            Some("PASS"),
            tokens,
        );
        let v = serde_json::to_value(&line).unwrap();
        assert_eq!(v["v"], 2);
        assert_eq!(v["repo"], "Thoth");
        assert_eq!(v["task_title"], "t");
        assert_eq!(v["effort_requested"], "high");
        assert_eq!(v["effort"], "high");
        assert_eq!(v["task_type"], "code");
        assert_eq!(v["difficulty"], "low");
        assert_eq!(v["risk"], "low");
        assert_eq!(v["verdict"], "PASS");
        assert_eq!(v["tokens"]["input"], 34);
        assert!(v["tokens"]["total"].is_null());
        assert_eq!(v["quota"]["five_hour_points"], 1.5);
        assert_eq!(v["quota"]["seven_day_points"], 0.5);
        assert_eq!(v["quota"]["reliable"], false);
        assert_eq!(v["quota"]["unreliable_reasons"], json!(["shared"]));
        assert_eq!(v["quota"]["source"], "claude_stream");
        assert_eq!(v["reason"], "codex is busy");
        let object = v.as_object().unwrap();
        for key in [
            "spec", "error_text", "worktree", "run_dir", "config_dir", "harness_session_id",
            "recommended", "deviates", "exempt", "previous_run", "require_reason",
        ] {
            assert!(!object.contains_key(key), "{key} must not be written");
        }
    }

    #[test]
    fn a_bare_run_gives_nulls_and_round_trips() {
        let run = RunRow { spec: Some("{}".into()), ..sample_run() };
        let line = line_from_parts("m", "abc", None, &run, None, None, None, None, None, Tokens::default());
        let v = serde_json::to_value(&line).unwrap();
        for key in [
            "effort_requested", "effort", "task_title", "task_type", "difficulty", "risk", "repo",
            "harness", "quota", "reason", "verdict",
        ] {
            assert!(v[key].is_null(), "{key} should be null");
        }
        let back: Line = serde_json::from_str(&serde_json::to_string(&line).unwrap()).unwrap();
        assert_eq!(back, line);
    }

    #[test]
    fn a_reason_that_is_missing_or_not_a_string_is_written_as_null() {
        let line_with = |choice: &str| {
            let choice: Value = serde_json::from_str(choice).unwrap();
            line_from_parts(
                "m",
                "abc",
                None,
                &sample_run(),
                None,
                None,
                Some(&choice),
                None,
                None,
                Tokens::default(),
            )
        };
        for malformed in [r#"{}"#, r#"{"reason":null}"#, r#"{"reason":3}"#, r#"{"reason":{"text":"x"}}"#] {
            let line = line_with(malformed);
            assert_eq!(line.reason, None, "{malformed}");
            assert!(serde_json::to_value(&line).unwrap()["reason"].is_null(), "{malformed}");
        }
        let line = line_with(r#"{"reason":"tests are many","previous_run":"R0","require_reason":true}"#);
        assert_eq!(line.reason.as_deref(), Some("tests are many"));
    }

    #[test]
    fn a_v1_line_with_the_old_keys_still_parses() {
        let run = RunRow { spec: Some("{}".into()), ..sample_run() };
        let line = line_from_parts("m", "abc", None, &run, None, None, None, None, None, Tokens::default());
        let mut v = serde_json::to_value(&line).unwrap();
        v["v"] = json!(1);
        v["recommended"] = json!({"profile":"p","model":"m","effort_requested":"low"});
        v["deviates"] = json!(true);
        v["exempt"] = json!("continuation");
        let back: Line = serde_json::from_value(v).unwrap();
        assert_eq!(back.v, 1);
        assert!(READABLE_VERSIONS.contains(&back.v));
        assert_eq!(back.run_id, "R1");
    }

    #[test]
    fn the_repo_name_tolerates_a_trailing_separator() {
        assert_eq!(repo_name(Some("C:\\Users\\j\\Thoth-dev\\")).as_deref(), Some("Thoth-dev"));
        assert_eq!(repo_name(Some("/srv/x/Thoth-dev/")).as_deref(), Some("Thoth-dev"));
        assert_eq!(repo_name(Some("")), None);
        assert_eq!(repo_name(None), None);
    }
}
