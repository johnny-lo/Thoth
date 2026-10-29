//! Read-only model inventory, with an explicitly refreshed external score cache.

use crate::commands::{arena, usage};
use crate::config::Paths;
use crate::db::global::Profile;
use crate::output;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub(crate) const DEFAULT_SCORES_URL: &str =
    "https://aistupidlevel.info/api/v1/models?period=latest&sortBy=combined";
pub(crate) const SOURCE_URL: &str = "https://aistupidlevel.info";
const KEY_URL: &str = "https://aistupidlevel.info/account/data-keys";
const CACHE_SCHEMA: &str = "aistupidlevel-v1";
const AA_SCHEMA: &str = "artificialanalysis-v2-free";
pub(crate) const AA_SOURCE_URL: &str = "https://artificialanalysis.ai";
pub(crate) const DEFAULT_AA_URL: &str = "https://artificialanalysis.ai/api/v2/language/models/free?page=";
const AA_CACHE_MINUTES: i64 = 360;
const DEFAULT_LICENSE: &str = "Attribution required — cite aistupidlevel.info as the source.";
const AUTO_REFRESH_AFTER_HOURS: i64 = 24;
const STALE_AFTER_HOURS: i64 = 48;
const REFRESH_LOCK_STALE_SECONDS: u64 = 120;
const AUTO_FETCH_MAX_TIME_SECONDS: u64 = 8;
pub(crate) const MANUAL_FETCH_MAX_TIME_SECONDS: u64 = 20;

/// Permission, for this process only, to refresh the score caches
/// automatically when `load_with_refresh` runs. An environment variable would
/// be inherited by the runner and worker children.
static AUTO_REFRESH_ALLOWED: AtomicBool = AtomicBool::new(false);

/// Runs `f` with the automatic refresh permitted for the first score load.
pub(crate) fn with_auto_refresh<T>(f: impl FnOnce() -> T) -> T {
    AUTO_REFRESH_ALLOWED.store(true, Ordering::SeqCst);
    let result = f();
    AUTO_REFRESH_ALLOWED.store(false, Ordering::SeqCst);
    result
}

pub(crate) struct AutoRefresh {
    pub(crate) enabled: bool,
    pub(crate) skipped: Option<&'static str>,
    pub(crate) attempted: Vec<&'static str>,
    pub(crate) refreshed: Vec<&'static str>,
    pub(crate) failed: Vec<(&'static str, String)>,
}

impl AutoRefresh {
    pub(crate) fn json(&self) -> Value {
        json!({
            "enabled": self.enabled,
            "skipped": self.skipped,
            "attempted": self.attempted,
            "refreshed": self.refreshed,
            "failed": self.failed.iter().map(|(s, e)| json!({"source": s, "error": e})).collect::<Vec<_>>(),
        })
    }
}

#[derive(Clone)]
pub(crate) struct ModelInfo {
    pub(crate) model: String,
    pub(crate) display_name: String,
    pub(crate) score_name: Option<String>,
    pub(crate) score: Option<Score>,
    pub(crate) may_have_degraded: bool,
    pub(crate) thoth: Thoth,
    pub(crate) thoth_breakdown: Vec<ThothBreakdown>,
    pub(crate) aa: Option<Vec<AaRow>>,
    /// MODELS-1: a run proved this account cannot use the model, and
    /// nothing has cleared that since (claude-code only).
    pub(crate) unavailable: Option<crate::model_list::Unavailable>,
}

#[derive(Clone)]
pub(crate) struct AaRow {
    pub(crate) name: String,
    pub(crate) effort: Option<String>,
    pub(crate) intelligence_index: Option<f64>,
    pub(crate) coding_index: Option<f64>,
    pub(crate) agentic_index: Option<f64>,
    pub(crate) price_input_per_1m: Option<f64>,
    pub(crate) price_output_per_1m: Option<f64>,
}

#[derive(Clone)]
pub(crate) struct Score {
    pub(crate) current: i64,
    pub(crate) trend: String,
    pub(crate) status: String,
    pub(crate) confidence_lower: f64,
    pub(crate) confidence_upper: f64,
    pub(crate) observed_at: String,
}

#[derive(Clone, Default)]
pub(crate) struct Thoth {
    pub(crate) implementer_runs: i64,
    pub(crate) reviewed: i64,
    pub(crate) pass: i64,
    pub(crate) reject: i64,
}

#[derive(Clone)]
pub(crate) struct ThothBreakdown {
    pub(crate) effort: Option<String>,
    pub(crate) task_type: Option<String>,
    pub(crate) difficulty: Option<String>,
    pub(crate) implementer_runs: i64,
    pub(crate) completed: i64,
    pub(crate) reviewed: i64,
    pub(crate) pass: i64,
    pub(crate) reject: i64,
    pub(crate) input_tokens: i64,
    pub(crate) output_tokens: i64,
    /// The other token kinds (DT-1); a NULL column counts as 0, like
    /// `input_tokens` and `output_tokens`.
    pub(crate) cache_read_tokens: i64,
    pub(crate) cache_write_tokens: i64,
    pub(crate) reasoning_tokens: i64,
    pub(crate) total_tokens: i64,
    /// Five-hour points of the runs whose newest `run_quota` event is
    /// reliable, and how many runs that is (DT-1: unreliable ones are left
    /// out).
    pub(crate) quota_points: Option<f64>,
    pub(crate) quota_runs: i64,
    /// The same for the weekly window (`seven_day.points`).
    pub(crate) quota_points_weekly: Option<f64>,
    pub(crate) quota_runs_weekly: i64,
    /// Runs whose newest `run_quota` event says `reliable: false`, numbers or not.
    pub(crate) quota_runs_unreliable: i64,
    /// The computers whose runs are in this group (DT-3), sorted; this
    /// computer is `records.machine`.
    pub(crate) machines: Vec<String>,
}

pub(crate) struct ProfileModels {
    pub(crate) profile_id: String,
    pub(crate) provider: String,
    pub(crate) harness: String,
    pub(crate) models_source: &'static str,
    pub(crate) note: Option<String>,
    /// MODELS-1: when the list was read (agy: the login check that stored
    /// it; codex: codex's own `fetched_at`, else the cache file's
    /// modification time; claude-code: never).
    pub(crate) models_seen_at: Option<String>,
    pub(crate) models: Vec<ModelInfo>,
}

pub(crate) struct ModelSnapshot {
    pub(crate) cache_path: PathBuf,
    pub(crate) cache_present: bool,
    pub(crate) fetched_at: Option<String>,
    pub(crate) refreshed: bool,
    pub(crate) profiles: Vec<ProfileModels>,
    pub(crate) aa_cache_path: PathBuf,
    pub(crate) aa_cache_present: bool,
    pub(crate) aa_fetched_at: Option<String>,
    pub(crate) aa_refreshed: bool,
    pub(crate) aa_unmatched: Vec<String>,
    pub(crate) arena: ArenaStatus,
    pub(crate) auto_refresh: AutoRefresh,
    pub(crate) records: RecordsSummary,
    pub(crate) decision_table: crate::commands::decision_table::Table,
}

/// The Agent Arena cache as `thoth models` reports it (RJ-2): its state and
/// board names, never its rows.
pub(crate) struct ArenaStatus {
    pub(crate) cache_path: PathBuf,
    pub(crate) cache_present: bool,
    pub(crate) fetched_at: Option<String>,
    pub(crate) refreshed: bool,
    pub(crate) sha: Option<String>,
    pub(crate) boards: Vec<String>,
}

impl ArenaStatus {
    fn json(&self) -> Value {
        let (age_hours, stale) = staleness(self.fetched_at.as_deref().filter(|_| self.cache_present));
        json!({
            "source": {"name": arena::NAME, "url": arena::SOURCE_URL, "license": arena::LICENSE, "license_url": arena::LICENSE_URL},
            "cache": {"path": self.cache_path.to_string_lossy(), "present": self.cache_present, "fetched_at": self.fetched_at, "refreshed": self.refreshed, "age_hours": age_hours, "stale": stale, "sha": self.sha},
            "boards": self.boards,
        })
    }
}

/// One score cache file as read from disk: `present` only when its `schema`
/// is the expected one, and `value` only then.
pub(crate) struct CacheFile {
    pub(crate) path: PathBuf,
    pub(crate) present: bool,
    pub(crate) fetched_at: Option<String>,
    pub(crate) value: Option<Value>,
}

pub(crate) fn read_cache_file(path: PathBuf, schema: &str) -> CacheFile {
    let path = std::path::absolute(&path).unwrap_or(path);
    let value = fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok());
    let present = value
        .as_ref()
        .and_then(|v| v.get("schema"))
        .and_then(Value::as_str)
        == Some(schema);
    let value = value.filter(|_| present);
    let fetched_at = value
        .as_ref()
        .and_then(|v| v.get("fetched_at"))
        .and_then(Value::as_str)
        .map(str::to_string);
    CacheFile { path, present, fetched_at, value }
}

pub(crate) fn asl_cache(paths: &Paths) -> CacheFile {
    read_cache_file(paths.home.join("model-scores.json"), CACHE_SCHEMA)
}

pub(crate) fn aa_cache(paths: &Paths) -> CacheFile {
    read_cache_file(paths.home.join("model-scores-aa.json"), AA_SCHEMA)
}

/// Which caches a call refreshed itself; the automatic refresh adds its own.
#[derive(Clone, Copy, Default)]
pub(crate) struct Refreshed {
    pub(crate) asl: bool,
    pub(crate) aa: bool,
    pub(crate) arena: bool,
}

/// What the shared records folder contributed (DT-3).
pub(crate) struct RecordsSummary {
    pub(crate) dir: Option<PathBuf>,
    pub(crate) machine: String,
    pub(crate) files: Vec<crate::records::SharedFile>,
    pub(crate) error: Option<String>,
}

impl RecordsSummary {
    fn json(&self) -> Value {
        json!({
            "dir": self.dir.as_ref().map(|d| d.to_string_lossy().to_string()),
            "machine": self.machine,
            "files": self.files.iter().map(|f| json!({
                "name": f.name, "machine": f.machine, "lines": f.lines, "skipped_lines": f.skipped_lines,
            })).collect::<Vec<_>>(),
            "error": self.error,
        })
    }
}

impl AaRow {
    fn json(&self) -> Value {
        json!({"name":self.name,"effort":self.effort,"intelligence_index":self.intelligence_index,"coding_index":self.coding_index,"agentic_index":self.agentic_index,"price_input_per_1m":self.price_input_per_1m,"price_output_per_1m":self.price_output_per_1m})
    }
}

impl Score {
    fn json(&self) -> Value {
        json!({
            "current": self.current,
            "trend": self.trend,
            "status": self.status,
            "confidence_lower": self.confidence_lower,
            "confidence_upper": self.confidence_upper,
            "observed_at": self.observed_at,
        })
    }
}

// The JSON leaves out the pass and reject counts the human output prints: the
// commander judges models by the external tables, not by Thoth's own record.
impl Thoth {
    fn json(&self) -> Value {
        json!({"implementer_runs":self.implementer_runs,"reviewed":self.reviewed})
    }
}

impl ThothBreakdown {
    fn json(&self) -> Value {
        json!({
            "effort": self.effort, "task_type": self.task_type, "difficulty": self.difficulty,
            "implementer_runs": self.implementer_runs, "completed": self.completed,
            "reviewed": self.reviewed,
            "input_tokens": self.input_tokens, "output_tokens": self.output_tokens,
            "quota_points": self.quota_points, "quota_runs": self.quota_runs,
            "cache_read_tokens": self.cache_read_tokens,
            "cache_write_tokens": self.cache_write_tokens,
            "reasoning_tokens": self.reasoning_tokens,
            "total_tokens": self.total_tokens,
            "quota_points_weekly": self.quota_points_weekly,
            "quota_runs_weekly": self.quota_runs_weekly,
            "quota_runs_unreliable": self.quota_runs_unreliable,
            "machines": self.machines,
        })
    }
}

impl ModelInfo {
    fn json(&self) -> Value {
        json!({
            "model":self.model,
            "display_name":self.display_name,
            "score_name":self.score_name,
            "score":self.score.as_ref().map(Score::json),
            "may_have_degraded":self.may_have_degraded,
            "thoth":self.thoth.json(),
            "thoth_breakdown":self.thoth_breakdown.iter().map(ThothBreakdown::json).collect::<Vec<_>>(),
            "aa":self.aa.as_ref().map(|rows| json!({"rows":rows.iter().map(AaRow::json).collect::<Vec<_>>() })),
            "unavailable":self.unavailable.as_ref().map(|u| json!({"seen_at":u.seen_at,"reason":u.reason,"run_id":u.run_id})),
        })
    }
}

impl ModelSnapshot {
    pub(crate) fn json(&self) -> Value {
        let (age_hours, stale) = staleness(self.fetched_at.as_deref().filter(|_| self.cache_present));
        let (aa_age_hours, aa_stale) =
            staleness(self.aa_fetched_at.as_deref().filter(|_| self.aa_cache_present));
        json!({
            "source": {"name":"AI Stupid Level","url":SOURCE_URL},
            "cache": {"path":self.cache_path.to_string_lossy(),"present":self.cache_present,"fetched_at":self.fetched_at,"refreshed":self.refreshed,"age_hours":age_hours,"stale":stale},
            "auto_refresh": self.auto_refresh.json(),
            "records": self.records.json(),
            "decision_table": self.decision_table.json(),
            "external":{
                "artificial_analysis":{"source":{"name":"Artificial Analysis","url":AA_SOURCE_URL},"cache":{"path":self.aa_cache_path.to_string_lossy(),"present":self.aa_cache_present,"fetched_at":self.aa_fetched_at,"refreshed":self.aa_refreshed,"age_hours":aa_age_hours,"stale":aa_stale},"unmatched_profile_models":self.aa_unmatched},
                "agent_arena": self.arena.json(),
            },
            "profiles": self.profiles.iter().map(|p| json!({
                "profile_id":p.profile_id,"provider":p.provider,"harness":p.harness,
                "models_source":p.models_source,"note":p.note,
                "models_seen_at":p.models_seen_at,
                "models":p.models.iter().map(ModelInfo::json).collect::<Vec<_>>()
            })).collect::<Vec<_>>()
        })
    }
}

pub fn run(paths: &Paths, refresh: bool, json_output: bool) -> Result<i32> {
    if !paths.global_db().is_file() {
        bail!(
            "no thoth state at {}; run `thoth init` first",
            paths.global_db().display()
        );
    }
    let asl_attempted =
        refresh && crate::commands::settings::secret(&paths.home, "aistupidlevel-api-key").is_some();
    let aa_attempted =
        refresh && crate::commands::settings::secret(&paths.home, "artificialanalysis-api-key").is_some();
    let refreshed = if refresh {
        refresh_caches(paths)?
    } else {
        Refreshed::default()
    };
    let snapshot = if refresh {
        load_with_refresh(paths, refreshed, true)?
    } else {
        with_auto_refresh(|| load_with_refresh(paths, refreshed, false))?
    };
    let mut plain = String::new();
    if !snapshot.auto_refresh.refreshed.is_empty() {
        plain.push_str(&format!(
            "auto-refresh: {}\n",
            snapshot.auto_refresh.refreshed.join(", ")
        ));
    }
    plain.push_str(&format!("score cache: {}\n", snapshot.cache_path.display()));
    if let Some(dir) = &snapshot.records.dir {
        match &snapshot.records.error {
            Some(error) => plain.push_str(&format!(
                "records: {} cannot be read ({error}); only this computer's runs are counted\n",
                dir.display()
            )),
            None => plain.push_str(&format!(
                "records: {} — {} other file(s), {} line(s)\n",
                dir.display(),
                snapshot.records.files.len(),
                snapshot.records.files.iter().map(|f| f.lines).sum::<usize>()
            )),
        }
    }
    if snapshot.arena.cache_present {
        plain.push_str(&format!(
            "scores: {} {} ({})\n",
            arena::NAME,
            arena::SOURCE_URL,
            arena::LICENSE
        ));
    }
    if snapshot.aa_cache_present {
        plain.push_str("scores: Artificial Analysis https://artificialanalysis.ai\n");
        if aa_attempted && !snapshot.aa_refreshed {
            if let Some(age) = cache_age_minutes(snapshot.aa_fetched_at.as_deref()) {
                plain.push_str(&format!("Artificial Analysis cache fetched {age} minutes ago; cache is still fresh for {AA_CACHE_MINUTES} minutes. Delete the cache file to force a refresh.\n"));
            }
        }
    }
    if snapshot.cache_present {
        plain.push_str("scores: AI Stupid Level https://aistupidlevel.info\n");
        if asl_attempted && !snapshot.refreshed {
            if let Some(age) = cache_age_minutes(snapshot.fetched_at.as_deref()) {
                plain.push_str(&format!("cache fetched {age} minutes ago; AI Stupid Level updates roughly hourly and asks clients to cache. Delete the cache file to force a refresh.\n"));
            }
        }
    } else if snapshot.cache_path.is_file() {
        plain.push_str(&format!(
            "{} is not an AI Stupid Level v1 cache; run `thoth models --refresh`.\n",
            snapshot.cache_path.display()
        ));
    }
    for (name, key_name, present, fetched_at) in [
        (
            "AI Stupid Level",
            "AISTUPIDLEVEL_API_KEY",
            snapshot.cache_present,
            snapshot.fetched_at.as_deref(),
        ),
        (
            "Artificial Analysis",
            "ARTIFICIALANALYSIS_API_KEY",
            snapshot.aa_cache_present,
            snapshot.aa_fetched_at.as_deref(),
        ),
    ] {
        let (age_hours, stale) = staleness(fetched_at.filter(|_| present));
        if !stale {
            continue;
        }
        let fix = format!("run `thoth models --refresh` with {key_name} set in the environment or stored by thoth config set-secret, or `thoth config set scores-auto-refresh on`.");
        match age_hours {
            Some(hours) => plain.push_str(&format!(
                "stale: {name} scores fetched {hours} hours ago; {fix}\n"
            )),
            None => plain.push_str(&format!("stale: no {name} scores cached; {fix}\n")),
        }
    }
    {
        // Agent Arena needs no key, so its fix names none.
        let (age_hours, stale) = staleness(
            snapshot.arena.fetched_at.as_deref().filter(|_| snapshot.arena.cache_present),
        );
        if stale {
            let fix = "run `thoth models --refresh`, or `thoth config set scores-auto-refresh on`.";
            match age_hours {
                Some(hours) => plain.push_str(&format!(
                    "stale: {} scores fetched {hours} hours ago; {fix}\n",
                    arena::NAME
                )),
                None => plain.push_str(&format!("stale: no {} scores cached; {fix}\n", arena::NAME)),
            }
        }
    }
    for p in &snapshot.profiles {
        plain.push_str(&format!(
            "\n{} ({}, {}) [{}]\n",
            p.profile_id, p.provider, p.harness, p.models_source
        ));
        if let Some(seen) = &p.models_seen_at {
            plain.push_str(&format!("  models read at {seen}\n"));
        }
        if let Some(note) = &p.note {
            plain.push_str(&format!("  {note}\n"));
        }
        for m in &p.models {
            let score = m.score.as_ref().map_or_else(
                || "no score".to_string(),
                |s| {
                    format!(
                        "current {} status {} trend {}",
                        s.current, s.status, s.trend
                    )
                },
            );
            plain.push_str(&format!(
                "  {} ({}) — {}; Thoth runs {} reviewed {} PASS {} REJECT {}{}{}\n",
                m.model,
                m.display_name,
                score,
                m.thoth.implementer_runs,
                m.thoth.reviewed,
                m.thoth.pass,
                m.thoth.reject,
                if m.may_have_degraded {
                    " — may have degraded"
                } else {
                    ""
                },
                m.unavailable.as_ref().map_or(String::new(), |u| format!(
                    " — unavailable since {}: {}",
                    u.seen_at, u.reason
                ))
            ));
            if let Some(rows) = &m.aa {
                let values = rows
                    .iter()
                    .map(|r| {
                        format!(
                            "{}: coding {} intelligence {}",
                            r.effort.as_deref().unwrap_or("null"),
                            r.coding_index.map_or("null".into(), |v| v.to_string()),
                            r.intelligence_index
                                .map_or("null".into(), |v| v.to_string())
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                plain.push_str(&format!("    Artificial Analysis {values}\n"));
            }
            for b in &m.thoth_breakdown {
                if b.effort.is_some() || b.task_type.is_some() || b.difficulty.is_some() {
                    plain.push_str(&format!(
                        "    effort {} {}/{} runs {} reviewed {} PASS {} REJECT {}\n",
                        b.effort.as_deref().unwrap_or("null"),
                        b.task_type.as_deref().unwrap_or("null"),
                        b.difficulty.as_deref().unwrap_or("null"),
                        b.implementer_runs,
                        b.reviewed,
                        b.pass,
                        b.reject
                    ));
                }
            }
        }
    }
    output::emit(json_output, &snapshot.json(), plain.trim_end());
    Ok(0)
}

pub(crate) fn load(paths: &Paths, refreshed: bool) -> Result<ModelSnapshot> {
    load_with_refresh(paths, Refreshed { asl: refreshed, ..Refreshed::default() }, false)
}

/// Uses up this process's one permission to refresh the caches
/// automatically (see `with_auto_refresh`), runs the refresh when it was
/// there, and prints its one warning line. Without the permission, nothing
/// is fetched and the result says `off`. `thoth models`, `report` and
/// `recommend` share this.
pub(crate) fn auto_refresh_if_permitted(paths: &Paths) -> AutoRefresh {
    let auto = if AUTO_REFRESH_ALLOWED
        .compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst)
        .is_ok()
    {
        auto_refresh(paths)
    } else {
        AutoRefresh {
            enabled: false,
            skipped: Some("off"),
            attempted: Vec::new(),
            refreshed: Vec::new(),
            failed: Vec::new(),
        }
    };
    if !auto.failed.is_empty() {
        let details = auto
            .failed
            .iter()
            .map(|(source, error)| format!("{source}: {error}"))
            .collect::<Vec<_>>()
            .join("; ");
        masked_eprintln!("warning: automatic score refresh failed: {details}; keeping the cached scores (run `thoth models --refresh` to retry, or `thoth config set scores-auto-refresh off`)");
    }
    auto
}

fn load_with_refresh(paths: &Paths, refreshed: Refreshed, manual: bool) -> Result<ModelSnapshot> {
    let auto = if manual {
        AutoRefresh {
            enabled: crate::commands::settings::scores_auto_refresh(&paths.home),
            skipped: Some("manual"),
            attempted: Vec::new(),
            refreshed: Vec::new(),
            failed: Vec::new(),
        }
    } else {
        auto_refresh_if_permitted(paths)
    };
    let refreshed = Refreshed {
        asl: refreshed.asl || auto.refreshed.contains(&"AI Stupid Level"),
        aa: refreshed.aa || auto.refreshed.contains(&"Artificial Analysis"),
        arena: refreshed.arena || auto.refreshed.contains(&arena::NAME),
    };
    let asl = asl_cache(paths);
    let cache_value = asl.value;
    let aa_file = aa_cache(paths);
    let aa_value = aa_file.value;
    let arena_file = arena::read_cache(paths);
    let arena_status = ArenaStatus {
        boards: arena::board_names(&arena_file),
        sha: arena::cached_sha(&arena_file),
        cache_path: arena_file.path,
        cache_present: arena_file.present,
        fetched_at: arena_file.fetched_at,
        refreshed: refreshed.arena,
    };
    let global = usage::global_read_only(paths)?;
    let profiles = crate::db::global::list_profiles(&global)?;
    let repos = usage::registered_repos(&global)?;
    let shared = crate::records::load_shared(paths);
    // MODELS-1: the stored agy lists and plans, from the same read-only
    // connection (a v3 global.db reads as none).
    let stored_lists = crate::db::global::profile_models(&global)?;
    let plans = crate::db::global::profile_plans(&global)?;
    let mut result = Vec::new();
    for profile in profiles {
        let Discovered { source, note, seen_at, models: discovered } =
            discover(&profile, stored_lists.get(&profile.profile_id));
        let plan = plans.get(&profile.profile_id).map(|p| p.plan.clone());
        let mut models = Vec::new();
        for Listed { model, display_name, score_name, unavailable } in
            listed(paths, &repos, &global, &profile, discovered, plan.as_deref())?
        {
            let score = score_name
                .as_deref()
                .and_then(|name| find_score(cache_value.as_ref(), name));
            let may_have_degraded = score
                .as_ref()
                .is_some_and(|s| s.status != "good" || s.trend == "down");
            let aa = score_name
                .as_deref()
                .and_then(|name| find_aa(aa_value.as_ref(), name));
            let thoth = thoth(paths, &repos, &shared, &profile.profile_id, &model)?;
            let thoth_breakdown =
                thoth_breakdown(paths, &repos, &shared, &profile.profile_id, &model)?;
            models.push(ModelInfo {
                model,
                display_name,
                score_name,
                score,
                may_have_degraded,
                thoth,
                thoth_breakdown,
                aa,
                unavailable,
            });
        }
        result.push(ProfileModels {
            profile_id: profile.profile_id,
            provider: profile.provider,
            harness: profile.harness,
            models_source: source,
            note,
            models_seen_at: seen_at,
            models,
        });
    }
    let aa_unmatched = if aa_file.present {
        result
            .iter()
            .flat_map(|p| {
                p.models
                    .iter()
                    .filter(|m| m.aa.is_none())
                    .map(|m| format!("{}/{}", p.profile_id, m.model))
            })
            .collect()
    } else {
        Vec::new()
    };
    let decision_table =
        crate::commands::decision_table::build(paths, &repos, &shared, OffsetDateTime::now_utc())?;
    Ok(ModelSnapshot {
        cache_path: asl.path,
        cache_present: asl.present,
        fetched_at: asl.fetched_at,
        refreshed: refreshed.asl,
        profiles: result,
        aa_cache_path: aa_file.path,
        aa_cache_present: aa_file.present,
        aa_fetched_at: aa_file.fetched_at,
        aa_refreshed: refreshed.aa,
        aa_unmatched,
        arena: arena_status,
        auto_refresh: auto,
        records: RecordsSummary {
            dir: shared.dir,
            machine: shared.machine,
            files: shared.files,
            error: shared.error,
        },
        decision_table,
    })
}

/// One model of a profile's list with the name the external tables know it
/// by and whether a run proved it unavailable; `thoth models` and
/// `recommend` (CROSS-1) both read it from `listed`.
pub(crate) struct Listed {
    pub(crate) model: String,
    pub(crate) display_name: String,
    pub(crate) score_name: Option<String>,
    pub(crate) unavailable: Option<crate::model_list::Unavailable>,
}

/// `discovered` (from `discover`) with each model's `score_name` and
/// `unavailable`, in the list's order.
pub(crate) fn listed(
    paths: &Paths,
    repos: &[String],
    global: &rusqlite::Connection,
    profile: &Profile,
    discovered: Vec<(String, String)>,
    plan: Option<&str>,
) -> Result<Vec<Listed>> {
    // MODELS-1: a claude-code profile's recorded unavailable models.
    let unavailable_events = if profile.harness == crate::adapter::HARNESS_CLAUDE_CODE {
        crate::db::global::list_quota_events(global, &profile.profile_id)?
    } else {
        Vec::new()
    };
    let mut out = Vec::new();
    for (model, display_name) in discovered {
        let unavailable = crate::model_list::unavailable_now(&unavailable_events, plan, &model);
        // RJ-2: one name for both sources, from what the harness itself
        // reported; no hard-coded alias table any more.
        let score_name = comparison_name(paths, repos, profile, &model)?;
        out.push(Listed { model, display_name, score_name, unavailable });
    }
    Ok(out)
}

/// What `discover` found for one profile.
pub(crate) struct Discovered {
    pub(crate) source: &'static str,
    pub(crate) note: Option<String>,
    /// When the list was read (see `ProfileModels::models_seen_at`).
    pub(crate) seen_at: Option<String>,
    /// `(model, display name)`, in the list's order.
    pub(crate) models: Vec<(String, String)>,
}

/// The models `thoth models` lists for a profile. `stored` is the profile's
/// stored list (MODELS-1), which only an antigravity profile uses: its list
/// is what the last login check read from `agy models`.
pub(crate) fn discover(profile: &Profile, stored: Option<&crate::db::global::StoredModels>) -> Discovered {
    match profile.harness.as_str() {
        "claude-code" => Discovered {
            source: "fixed_list",
            note: Some("fixed list, not confirmed per account".to_string()),
            seen_at: None,
            models: crate::model_list::CLAUDE_MODELS
                .iter()
                .map(|s| ((*s).to_string(), (*s).to_string()))
                .collect(),
        },
        "codex" => {
            let path = Path::new(&profile.config_dir).join("models_cache.json");
            let Some(cache) = crate::model_list::codex_cache(Path::new(&profile.config_dir)) else {
                return Discovered {
                    source: "codex_models_cache",
                    note: Some(format!("{} is missing or unreadable.", path.display())),
                    seen_at: None,
                    models: Vec::new(),
                };
            };
            Discovered {
                source: "codex_models_cache",
                note: None,
                seen_at: cache.seen_at,
                models: cache.models.into_iter().map(|m| (m.model, m.display_name)).collect(),
            }
        }
        crate::adapter::antigravity::HARNESS_NAME => Discovered {
            source: "agy_models",
            note: crate::model_list::agy_note(&profile.profile_id, stored),
            seen_at: stored.filter(|s| !s.models.is_empty()).and_then(|s| s.seen_at.clone()),
            models: stored
                .map(|s| s.models.iter().map(|m| (m.model.clone(), m.display_name.clone())).collect())
                .unwrap_or_default(),
        },
        _ => Discovered { source: "unsupported_harness", note: None, seen_at: None, models: Vec::new() },
    }
}

pub(crate) fn normalized(value: &str) -> String {
    let mut out = String::new();
    let mut separated = false;
    for c in value.to_ascii_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            separated = false;
        } else if !out.is_empty() && !separated {
            out.push('-');
            separated = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// A leaderboard name's base and the effort of its parenthesized variant:
/// `GPT 6.1 Sol (Max)` is `GPT 6.1 Sol` at `max`.
pub(crate) fn aa_name_parts(name: &str) -> (&str, Option<String>) {
    if name.ends_with(')') {
        if let Some(pos) = name.rfind('(') {
            let effort = name[pos + 1..name.len() - 1]
                .to_ascii_lowercase()
                .split(|c: char| !c.is_ascii_alphanumeric())
                .find(|word| {
                    matches!(
                        *word,
                        "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "ultra"
                    )
                })
                .map(str::to_string);
            return (name[..pos].trim_end(), effort);
        }
    }
    (name, None)
}

fn find_aa(cache: Option<&Value>, comparison: &str) -> Option<Vec<AaRow>> {
    let mut rows: Vec<_> = cache?
        .get("data")?
        .as_array()?
        .iter()
        .filter_map(|value| {
            let name = value.get("name")?.as_str()?;
            let (base, effort) = aa_name_parts(name);
            if normalized(base) != normalized(comparison) {
                return None;
            }
            Some(AaRow {
                name: name.to_string(),
                effort,
                intelligence_index: value
                    .pointer("/evaluations/artificial_analysis_intelligence_index")
                    .and_then(Value::as_f64),
                coding_index: value
                    .pointer("/evaluations/artificial_analysis_coding_index")
                    .and_then(Value::as_f64),
                agentic_index: value
                    .pointer("/evaluations/artificial_analysis_agentic_index")
                    .and_then(Value::as_f64),
                price_input_per_1m: value
                    .pointer("/pricing/price_1m_input_tokens")
                    .and_then(Value::as_f64),
                price_output_per_1m: value
                    .pointer("/pricing/price_1m_output_tokens")
                    .and_then(Value::as_f64),
            })
        })
        .collect();
    const ORDER: [&str; 7] = ["minimal", "low", "medium", "high", "xhigh", "max", "ultra"];
    rows.sort_by_key(|row| {
        row.effort.as_ref().map_or(0, |effort| {
            1 + ORDER
                .iter()
                .position(|x| x == effort)
                .unwrap_or(ORDER.len())
        })
    });
    (!rows.is_empty()).then_some(rows)
}

/// The name the external tables know a profile's model by: codex's slug as
/// it is; for Claude Code, the `model` of this profile and alias's newest
/// `harness_init` event with a trailing `-YYYYMMDD` removed, and none until
/// the alias has run once (RJ-2: no hard-coded alias table).
fn comparison_name(
    paths: &Paths,
    repos: &[String],
    profile: &Profile,
    model: &str,
) -> Result<Option<String>> {
    if profile.harness == "codex" {
        return Ok(Some(model.to_string()));
    }
    if profile.harness != "claude-code" {
        return Ok(None);
    }
    let mut newest: Option<(String, String)> = None;
    for repo in repos {
        let path = paths.state_db(repo);
        if !path.is_file() {
            continue;
        }
        let conn = usage::read_only(&path)?;
        let row = conn.query_row(
            "SELECT COALESCE(e.ts,''), e.detail FROM events e JOIN runs r ON r.run_id=e.run_id WHERE r.account=?1 AND r.model=?2 AND e.type='harness_init' ORDER BY e.ts DESC, e.seq DESC LIMIT 1",
            [&profile.profile_id, model], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)));
        match row {
            Ok((ts, detail)) if newest.as_ref().is_none_or(|(best, _)| ts > *best) => {
                newest = Some((ts, detail))
            }
            Ok(_) | Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read harness_init from {}", path.display()))
            }
        }
    }
    Ok(newest
        .and_then(|(_, detail)| serde_json::from_str::<Value>(&detail).ok())
        .and_then(|v| v.get("model").and_then(Value::as_str).map(str::to_string))
        .map(|name| strip_model_date(&name)))
}

fn strip_model_date(name: &str) -> String {
    if name.len() > 9
        && name.as_bytes()[name.len() - 9] == b'-'
        && name[name.len() - 8..].chars().all(|c| c.is_ascii_digit())
    {
        name[..name.len() - 9].to_string()
    } else {
        name.to_string()
    }
}

fn find_score(cache: Option<&Value>, name: &str) -> Option<Score> {
    let rows = cache?.pointer("/models/data")?.as_array()?;
    let data = rows
        .iter()
        .find(|row| row.get("name").and_then(Value::as_str) == Some(name))?;
    Some(Score {
        current: data.get("currentScore")?.as_i64()?,
        trend: data.get("trend")?.as_str()?.to_string(),
        status: data.get("status")?.as_str()?.to_string(),
        confidence_lower: data.get("confidenceLower")?.as_f64()?,
        confidence_upper: data.get("confidenceUpper")?.as_f64()?,
        observed_at: data.get("lastUpdated")?.as_str()?.to_string(),
    })
}

/// The other computers' implementer lines of this profile and model that are
/// not already in this computer's databases, each run once (DT-3).
fn remote_lines<'a>(
    shared: &'a crate::records::Shared,
    local_ids: &HashSet<String>,
    profile: &str,
    model: &str,
) -> Vec<&'a crate::records::Line> {
    let mut seen = HashSet::new();
    shared
        .lines
        .iter()
        .filter(|l| {
            l.role == "implementer"
                && l.profile == profile
                && l.model == model
                && !local_ids.contains(&l.run_id)
        })
        .filter(|l| seen.insert(l.run_id.clone()))
        .collect()
}

fn thoth(
    paths: &Paths,
    repos: &[String],
    shared: &crate::records::Shared,
    profile: &str,
    model: &str,
) -> Result<Thoth> {
    let mut total = Thoth::default();
    let mut local_ids = HashSet::new();
    for repo in repos {
        let path = paths.state_db(repo);
        if !path.is_file() {
            continue;
        }
        let conn = usage::read_only(&path)?;
        let values = conn.query_row(
            "SELECT COUNT(*),
             SUM(CASE WHEN EXISTS (SELECT 1 FROM reviews v WHERE v.target_run_id=r.run_id) THEN 1 ELSE 0 END),
             (SELECT COUNT(*) FROM reviews v JOIN runs x ON x.run_id=v.target_run_id WHERE x.role='implementer' AND x.account=?1 AND x.model=?2 AND v.verdict='PASS'),
             (SELECT COUNT(*) FROM reviews v JOIN runs x ON x.run_id=v.target_run_id WHERE x.role='implementer' AND x.account=?1 AND x.model=?2 AND v.verdict='REJECT')
             FROM runs r WHERE r.role='implementer' AND r.account=?1 AND r.model=?2",
            [profile, model],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(0), r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)),
        )?;
        total.implementer_runs += values.0;
        total.reviewed += values.1;
        total.pass += values.2;
        total.reject += values.3;
        let mut stmt = conn.prepare(
            "SELECT run_id FROM runs WHERE role='implementer' AND account=?1 AND model=?2",
        )?;
        let ids = stmt.query_map([profile, model], |r| r.get::<_, String>(0))?;
        for id in ids {
            local_ids.insert(id?);
        }
    }
    // A remote line only knows its run's newest verdict.
    for line in remote_lines(shared, &local_ids, profile, model) {
        total.implementer_runs += 1;
        if let Some(verdict) = line.verdict.as_deref() {
            total.reviewed += 1;
            match verdict {
                "PASS" => total.pass += 1,
                "REJECT" => total.reject += 1,
                _ => {}
            }
        }
    }
    Ok(total)
}

fn parse_group_keys(
    run_spec: &str,
    task_spec: &str,
) -> (Option<String>, Option<String>, Option<String>) {
    let run = serde_json::from_str::<Value>(run_spec).ok();
    let task = serde_json::from_str::<Value>(task_spec).ok();
    let effort = run
        .as_ref()
        .and_then(|v| v.pointer("/launch/effort"))
        .and_then(Value::as_str)
        .map(str::to_string);
    let task_type = task
        .as_ref()
        .and_then(|v| v.get("task_type"))
        .and_then(Value::as_str)
        .filter(|v| matches!(*v, "code" | "research" | "review" | "docs" | "mechanical"))
        .map(str::to_string);
    let difficulty = task
        .as_ref()
        .and_then(|v| v.get("difficulty"))
        .and_then(Value::as_str)
        .filter(|v| matches!(*v, "low" | "normal" | "high"))
        .map(str::to_string);
    (effort, task_type, difficulty)
}

fn breakdown_group(
    groups: &mut Vec<ThothBreakdown>,
    effort: &Option<String>,
    task_type: &Option<String>,
    difficulty: &Option<String>,
) -> usize {
    let index = groups
        .iter()
        .position(|g| &g.effort == effort && &g.task_type == task_type && &g.difficulty == difficulty);
    index.unwrap_or_else(|| {
        groups.push(ThothBreakdown {
            effort: effort.clone(),
            task_type: task_type.clone(),
            difficulty: difficulty.clone(),
            implementer_runs: 0,
            completed: 0,
            reviewed: 0,
            pass: 0,
            reject: 0,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            reasoning_tokens: 0,
            total_tokens: 0,
            quota_points: None,
            quota_runs: 0,
            quota_points_weekly: None,
            quota_runs_weekly: 0,
            quota_runs_unreliable: 0,
            machines: Vec::new(),
        });
        groups.len() - 1
    })
}

fn thoth_breakdown(
    paths: &Paths,
    repos: &[String],
    shared: &crate::records::Shared,
    profile: &str,
    model: &str,
) -> Result<Vec<ThothBreakdown>> {
    let mut groups = Vec::new();
    let mut local_ids = HashSet::new();
    for repo in repos {
        let path = paths.state_db(repo);
        if !path.is_file() {
            continue;
        }
        let conn = usage::read_only(&path)?;
        // A repo not yet opened by a version with DT-1 is still v4: its
        // `runs` has no token-kind columns, which then read as NULL.
        let col = |name: &str| {
            if crate::db::migrations::column_exists(&conn, "runs", name).unwrap_or(false) {
                format!("r.{name}")
            } else {
                "NULL".to_string()
            }
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT r.run_id, r.status, r.spec, r.input_tokens, r.output_tokens, t.spec,
                    {}, {}, {}, {}
             FROM runs r LEFT JOIN tasks t ON t.task_id=r.task_id
             WHERE r.role='implementer' AND r.account=?1 AND r.model=?2",
            col("cache_read_tokens"),
            col("cache_write_tokens"),
            col("reasoning_tokens"),
            col("total_tokens"),
        ))?;
        let rows = stmt.query_map([profile, model], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                r.get::<_, Option<i64>>(3)?.unwrap_or(0),
                r.get::<_, Option<i64>>(4)?.unwrap_or(0),
                r.get::<_, Option<String>>(5)?.unwrap_or_default(),
                [
                    r.get::<_, Option<i64>>(6)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(7)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(8)?.unwrap_or(0),
                    r.get::<_, Option<i64>>(9)?.unwrap_or(0),
                ],
            ))
        })?;
        for row in rows {
            let (run_id, status, run_spec, input, output, task_spec, kinds) = row?;
            let (effort, task_type, difficulty) = parse_group_keys(&run_spec, &task_spec);
            let i = breakdown_group(&mut groups, &effort, &task_type, &difficulty);
            local_ids.insert(run_id.clone());
            let g = &mut groups[i];
            if !g.machines.contains(&shared.machine) {
                g.machines.push(shared.machine.clone());
            }
            g.implementer_runs += 1;
            g.completed += i64::from(status == "completed");
            g.input_tokens += input;
            g.output_tokens += output;
            g.cache_read_tokens += kinds[0];
            g.cache_write_tokens += kinds[1];
            g.reasoning_tokens += kinds[2];
            g.total_tokens += kinds[3];
            let latest: Option<String> = conn.query_row(
                "SELECT verdict FROM reviews WHERE target_run_id=?1 ORDER BY created_at DESC, review_id DESC LIMIT 1", [&run_id],
                |r| r.get(0)).ok();
            if latest.is_some() {
                g.reviewed += 1;
            }
            match latest.as_deref() {
                Some("PASS") => g.pass += 1,
                Some("REJECT") => g.reject += 1,
                _ => {}
            }
            let detail: Option<String> = conn.query_row(
                "SELECT detail FROM events WHERE run_id=?1 AND type=?2 ORDER BY seq DESC LIMIT 1",
                [&run_id, crate::model::event_type::RUN_QUOTA], |r| r.get::<_, Option<String>>(0)).ok().flatten();
            let Some(detail) =
                detail.and_then(|d| serde_json::from_str::<Value>(&d).ok())
            else {
                continue;
            };
            // An event without `reliable` (written before DT-1) counts as
            // reliable; only an explicit false leaves a run out.
            if detail.get("reliable").and_then(Value::as_bool) == Some(false) {
                g.quota_runs_unreliable += 1;
                continue;
            }
            if let Some(points) = detail.get("points").and_then(Value::as_f64) {
                g.quota_points = Some(g.quota_points.unwrap_or(0.0) + points);
                g.quota_runs += 1;
            }
            if let Some(points) = detail
                .pointer("/seven_day/points")
                .and_then(Value::as_f64)
            {
                g.quota_points_weekly = Some(g.quota_points_weekly.unwrap_or(0.0) + points);
                g.quota_runs_weekly += 1;
            }
        }
    }
    for line in remote_lines(shared, &local_ids, profile, model) {
        let i = breakdown_group(&mut groups, &line.effort, &line.task_type, &line.difficulty);
        let g = &mut groups[i];
        if !g.machines.contains(&line.machine) {
            g.machines.push(line.machine.clone());
        }
        g.implementer_runs += 1;
        g.completed += i64::from(line.status == "completed");
        g.input_tokens += line.tokens.input.unwrap_or(0);
        g.output_tokens += line.tokens.output.unwrap_or(0);
        g.cache_read_tokens += line.tokens.cache_read.unwrap_or(0);
        g.cache_write_tokens += line.tokens.cache_write.unwrap_or(0);
        g.reasoning_tokens += line.tokens.reasoning.unwrap_or(0);
        g.total_tokens += line.tokens.total.unwrap_or(0);
        if let Some(verdict) = line.verdict.as_deref() {
            g.reviewed += 1;
            match verdict {
                "PASS" => g.pass += 1,
                "REJECT" => g.reject += 1,
                _ => {}
            }
        }
        let Some(quota) = &line.quota else { continue };
        if !quota.reliable {
            g.quota_runs_unreliable += 1;
            continue;
        }
        if let Some(points) = quota.five_hour_points {
            g.quota_points = Some(g.quota_points.unwrap_or(0.0) + points);
            g.quota_runs += 1;
        }
        if let Some(points) = quota.seven_day_points {
            g.quota_points_weekly = Some(g.quota_points_weekly.unwrap_or(0.0) + points);
            g.quota_runs_weekly += 1;
        }
    }
    for g in &mut groups {
        g.machines.sort();
    }
    groups.sort_by(|a, b| {
        fn effort(v: &Option<String>) -> (u8, usize, &str) {
            const ORDER: [&str; 6] = ["low", "medium", "high", "xhigh", "max", "ultra"];
            match v {
                None => (0, 0, ""),
                Some(s) => (
                    1,
                    ORDER.iter().position(|x| x == s).unwrap_or(usize::MAX),
                    s,
                ),
            }
        }
        fn named(v: &Option<String>) -> (u8, &str) {
            (u8::from(v.is_some()), v.as_deref().unwrap_or(""))
        }
        fn difficulty(v: &Option<String>) -> (u8, u8) {
            (
                u8::from(v.is_some()),
                match v.as_deref() {
                    Some("low") => 0,
                    Some("normal") => 1,
                    Some("high") => 2,
                    _ => 0,
                },
            )
        }
        effort(&a.effort)
            .cmp(&effort(&b.effort))
            .then_with(|| named(&a.task_type).cmp(&named(&b.task_type)))
            .then_with(|| difficulty(&a.difficulty).cmp(&difficulty(&b.difficulty)))
    });
    Ok(groups)
}

/// DT-7: `thoth_breakdown` of every (profile, model) that has implementer
/// runs, here or in the shared records, whether or not `discover` lists the
/// model or the profile is still registered; sorted by (profile, model).
pub(crate) fn all_breakdowns(
    paths: &Paths,
    repos: &[String],
    shared: &crate::records::Shared,
) -> Result<Vec<(String, String, Vec<ThothBreakdown>)>> {
    let mut pairs = std::collections::BTreeSet::new();
    for repo in repos {
        let path = paths.state_db(repo);
        if !path.is_file() {
            continue;
        }
        let conn = usage::read_only(&path)?;
        let mut stmt = conn.prepare(
            "SELECT DISTINCT account, model FROM runs
             WHERE role='implementer' AND account IS NOT NULL AND model IS NOT NULL AND model <> ''",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            pairs.insert(row?);
        }
    }
    for line in &shared.lines {
        if line.role == "implementer" && !line.model.is_empty() {
            pairs.insert((line.profile.clone(), line.model.clone()));
        }
    }
    let mut out = Vec::new();
    for (profile, model) in pairs {
        let groups = thoth_breakdown(paths, repos, shared, &profile, &model)?;
        out.push((profile, model, groups));
    }
    Ok(out)
}

/// `models --refresh`: the two keyed sources when their key is set (from the
/// environment or secrets.json, CFG-2), and Agent Arena always (RJ-2). A
/// keyed source that fails is an error, as before; an Agent Arena failure
/// only warns and keeps the old cache.
fn refresh_caches(paths: &Paths) -> Result<Refreshed> {
    let asl_key = crate::commands::settings::secret(&paths.home, "aistupidlevel-api-key");
    let aa_key = crate::commands::settings::secret(&paths.home, "artificialanalysis-api-key");
    if asl_key.is_none() && aa_key.is_none() {
        let mut message = format!("note: AI Stupid Level and Artificial Analysis were skipped: AISTUPIDLEVEL_API_KEY or ARTIFICIALANALYSIS_API_KEY is required to refresh them: set the environment variable, or store the key in your own terminal with thoth config set-secret aistupidlevel-api-key (or artificialanalysis-api-key). Free keys take about 30 seconds to create after signing in on the account Data keys page: {KEY_URL}");
        if std::env::var_os("THOTH_RUN_ID").is_some() {
            message.push_str("; secrets.json is not read while THOTH_RUN_ID is set");
        } else if let Some(problem) = crate::commands::secrets::load_secrets(&paths.home).problem {
            message.push_str(&format!(
                "; {} cannot be used ({problem}); see thoth config list",
                crate::commands::secrets::path_in(&paths.home).display()
            ));
        }
        masked_eprintln!("{message}");
    }
    let asl = match &asl_key {
        None => Ok(false),
        Some(key) => refresh_asl_cache(paths, key, MANUAL_FETCH_MAX_TIME_SECONDS),
    };
    let aa = match &aa_key {
        None => Ok(false),
        Some(key) => refresh_aa_cache(paths, key, MANUAL_FETCH_MAX_TIME_SECONDS),
    };
    let arena = match arena::refresh(paths, &arena::Budget::unlimited()) {
        Ok(refreshed) => refreshed,
        Err(error) => {
            masked_eprintln!(
                "warning: {} refresh failed: {}; keeping the cached table",
                arena::NAME,
                one_line(&format!("{error:#}"))
            );
            false
        }
    };
    match (asl, aa) {
        (Ok(a), Ok(b)) => Ok(Refreshed { asl: a, aa: b, arena }),
        (Err(a), Ok(_)) => bail!("AI Stupid Level refresh failed: {a:#}"),
        (Ok(_), Err(b)) => bail!("Artificial Analysis refresh failed: {b:#}"),
        (Err(a), Err(b)) => bail!(
            "AI Stupid Level refresh failed: {a:#}; Artificial Analysis refresh failed: {b:#}"
        ),
    }
}

fn refresh_asl_cache(paths: &Paths, key: &str, max_time: u64) -> Result<bool> {
    let cache_path = paths.home.join("model-scores.json");
    if let Ok(raw) = fs::read_to_string(&cache_path) {
        if let Ok(cache) = serde_json::from_str::<Value>(&raw) {
            if cache.get("schema").and_then(Value::as_str) == Some(CACHE_SCHEMA)
                && cache_age_minutes(cache.get("fetched_at").and_then(Value::as_str))
                    .is_some_and(|age| age < 60)
            {
                return Ok(false);
            }
        }
    }
    let models_url =
        crate::commands::settings::effective("scores-url");
    let nonce = format!(
        "{}.{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let models_tmp = paths.home.join(format!(".model-scores.models.{nonce}.tmp"));
    let cache_tmp = paths.home.join(format!(".model-scores.{nonce}.tmp"));
    let result = (|| -> Result<bool> {
        fetch(
            &models_url,
            &models_tmp,
            &format!("Authorization: Bearer {key}\n"),
            key,
            max_time,
        )?;
        let models: Value = serde_json::from_slice(&fs::read(&models_tmp)?)
            .with_context(|| format!("invalid JSON from {models_url}"))?;
        if models.get("success").and_then(Value::as_bool) != Some(true)
            || !models.get("data").is_some_and(Value::is_array)
        {
            bail!("invalid AI Stupid Level v1 response from {models_url}: expected success=true and data array");
        }
        let fetched_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
        let license = models
            .get("license")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_LICENSE);
        let body = serde_json::to_vec_pretty(&json!({
            "schema":CACHE_SCHEMA,"fetched_at":fetched_at,"url":models_url,
            "source":{"name":"AI Stupid Level","url":SOURCE_URL,"license":license},"models":models
        }))?;
        fs::write(&cache_tmp, body)?;
        replace_cache(&cache_tmp, &cache_path)?;
        Ok(true)
    })();
    for path in [&models_tmp, &cache_tmp] {
        let _ = fs::remove_file(path);
    }
    result
}

fn refresh_aa_cache(paths: &Paths, key: &str, max_time: u64) -> Result<bool> {
    let cache_path = paths.home.join("model-scores-aa.json");
    if let Ok(raw) = fs::read_to_string(&cache_path) {
        if let Ok(cache) = serde_json::from_str::<Value>(&raw) {
            if cache.get("schema").and_then(Value::as_str) == Some(AA_SCHEMA)
                && cache_age_minutes(cache.get("fetched_at").and_then(Value::as_str))
                    .is_some_and(|age| age < AA_CACHE_MINUTES)
            {
                return Ok(false);
            }
        }
    }
    let base = crate::commands::settings::effective("aa-url");
    let nonce = format!(
        "{}.{}",
        std::process::id(),
        OffsetDateTime::now_utc().unix_timestamp_nanos()
    );
    let cache_tmp = paths.home.join(format!(".model-scores-aa.{nonce}.tmp"));
    let result = (|| -> Result<bool> {
        let mut data = Vec::new();
        let mut pages = 0;
        for page in 1..=30 {
            let url = format!("{base}{page}");
            let page_tmp = paths
                .home
                .join(format!(".model-scores-aa.page-{page}.{nonce}.tmp"));
            let fetched = (|| -> Result<Value> {
                fetch(&url, &page_tmp, &format!("x-api-key: {key}\n"), key, max_time)?;
                let value: Value = serde_json::from_slice(&fs::read(&page_tmp)?)
                    .with_context(|| format!("invalid JSON from {url}"))?;
                if !value.is_object() || !value.get("data").is_some_and(Value::is_array) {
                    bail!("invalid Artificial Analysis response from {url}: expected JSON object with data array");
                }
                Ok(value)
            })();
            let _ = fs::remove_file(&page_tmp);
            let value = fetched?;
            data.extend(
                value["data"]
                    .as_array()
                    .expect("validated array")
                    .iter()
                    .cloned(),
            );
            pages = page;
            if value
                .pointer("/pagination/has_more")
                .and_then(Value::as_bool)
                != Some(true)
            {
                break;
            }
            if page == 30 {
                bail!("Artificial Analysis pagination exceeded 30 pages at {url}");
            }
        }
        let fetched_at = OffsetDateTime::now_utc().format(&Rfc3339)?;
        fs::write(
            &cache_tmp,
            serde_json::to_vec_pretty(
                &json!({"schema":AA_SCHEMA,"fetched_at":fetched_at,"source":{"name":"Artificial Analysis","url":AA_SOURCE_URL},"pages":pages,"data":data}),
            )?,
        )?;
        replace_cache(&cache_tmp, &cache_path)?;
        Ok(true)
    })();
    let _ = fs::remove_file(&cache_tmp);
    result
}

/// A keyed request: the header goes in through stdin and redirects are not
/// followed, so the key never reaches another host.
fn fetch(url: &str, target: &Path, header: &str, key: &str, max_time: u64) -> Result<()> {
    fetch_with(url, target, Some(header), key, max_time)
}

/// A public request (Agent Arena): no header, and redirects are followed
/// because Hugging Face answers `resolve` with a 302 to its CDN.
pub(crate) fn fetch_public(url: &str, target: &Path, max_time: u64) -> Result<()> {
    fetch_with(url, target, None, "", max_time)
}

fn fetch_with(url: &str, target: &Path, header: Option<&str>, key: &str, max_time: u64) -> Result<()> {
    let mut child = Command::new("curl")
        .args(curl_args(url, target, max_time, header.is_none()))
        .stdin(if header.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow!("fetching {url} with curl: {e}"))?;
    let write_result = match header {
        Some(header) => child
            .stdin
            .take()
            .expect("curl stdin was configured as piped")
            .write_all(header.as_bytes()),
        None => Ok(()),
    };
    let output = child
        .wait_with_output()
        .map_err(|e| anyhow!("fetching {url} with curl: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = if key.is_empty() { stderr.into_owned() } else { stderr.replace(key, "[redacted]") };
        bail!("{}", curl_failure_message(url, stderr.trim()));
    }
    write_result.with_context(|| format!("sending authorization header to curl for {url}"))?;
    Ok(())
}

/// `follow` (Agent Arena only) adds `-L`; a keyed request never follows a
/// redirect, and reads its header from stdin (`-H @-`).
fn curl_args(url: &str, target: &Path, max_time: u64, follow: bool) -> Vec<std::ffi::OsString> {
    let mut args: Vec<std::ffi::OsString> = vec![
        "-sS".into(),
        "--fail".into(),
        "--max-time".into(),
        max_time.to_string().into(),
    ];
    if follow {
        args.push("-L".into());
    } else {
        args.push("-H".into());
        args.push("@-".into());
    }
    args.push("-o".into());
    args.push(target.as_os_str().to_owned());
    args.push(url.into());
    args
}

#[cfg(not(windows))]
pub(crate) fn replace_cache(source: &Path, target: &Path) -> Result<()> {
    fs::rename(source, target).with_context(|| format!("replacing {}", target.display()))
}

#[cfg(windows)]
pub(crate) fn replace_cache(source: &Path, target: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(existing: *const u16, replacement: *const u16, flags: u32) -> i32;
    }
    let target_display = target.display().to_string();
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both pointers refer to live, NUL-terminated UTF-16 buffers for
    // the duration of this synchronous call.
    if unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    } == 0
    {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("replacing {target_display}"));
    }
    Ok(())
}

fn cache_age_minutes(fetched_at: Option<&str>) -> Option<i64> {
    let fetched = OffsetDateTime::parse(fetched_at?, &Rfc3339).ok()?;
    Some((OffsetDateTime::now_utc() - fetched).whole_minutes().max(0))
}

/// (age in whole hours, stale). Stale means no usable cache or at least
/// `STALE_AFTER_HOURS` old.
pub(crate) fn staleness(fetched_at: Option<&str>) -> (Option<i64>, bool) {
    match cache_age_minutes(fetched_at) {
        Some(minutes) => {
            let hours = minutes / 60;
            (Some(hours), hours >= STALE_AFTER_HOURS)
        }
        None => (None, true),
    }
}

fn due_for_auto_refresh(fetched_at: Option<&str>) -> bool {
    cache_age_minutes(fetched_at).is_none_or(|age| age >= AUTO_REFRESH_AFTER_HOURS * 60)
}

/// Whether the cache file at `path` is missing, foreign, or old enough to
/// refresh automatically.
fn source_due(path: &Path, schema: &str) -> bool {
    let value = fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .filter(|v| v.get("schema").and_then(Value::as_str) == Some(schema));
    due_for_auto_refresh(
        value
            .as_ref()
            .and_then(|v| v.get("fetched_at"))
            .and_then(Value::as_str),
    )
}

/// A lock is stale when its modification time is more than
/// `REFRESH_LOCK_STALE_SECONDS` behind `now`. The content is never consulted:
/// between `create_new` and the write the file is briefly empty, and an empty
/// lock still belongs to a live process.
fn lock_is_stale(modified: SystemTime, now: SystemTime) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| age > Duration::from_secs(REFRESH_LOCK_STALE_SECONDS))
}

enum LockOutcome {
    Held,
    Busy,
    Failed(std::io::Error),
}

/// Creates the lock file exclusively and writes the time for a human reader.
/// Only the file this call created is removed if the write fails.
fn create_lock_file(lock: &Path) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new().write(true).create_new(true).open(lock)?;
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_default();
    if let Err(error) = file.write_all(now.as_bytes()) {
        drop(file);
        let _ = fs::remove_file(lock);
        return Err(error);
    }
    Ok(())
}

/// Best-effort lock: whoever creates the file exclusively holds it. A lock
/// older than `REFRESH_LOCK_STALE_SECONDS` is removed and taken once; if two
/// processes reclaim the same stale lock at once, both may fetch.
fn acquire_lock(lock: &Path, create: &dyn Fn(&Path) -> std::io::Result<()>) -> LockOutcome {
    match create(lock) {
        Ok(()) => return LockOutcome::Held,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return LockOutcome::Failed(error),
    }
    let modified = match fs::metadata(lock).and_then(|m| m.modified()) {
        Ok(modified) => modified,
        Err(_) => return LockOutcome::Busy,
    };
    if !lock_is_stale(modified, SystemTime::now()) {
        return LockOutcome::Busy;
    }
    let _ = fs::remove_file(lock);
    match create(lock) {
        Ok(()) => LockOutcome::Held,
        Err(_) => LockOutcome::Busy,
    }
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Refreshes the external score caches that are missing or a day old, when
/// the user switched `scores-auto-refresh` on and the source's key is set.
/// Never fails: problems come back in `failed`.
fn auto_refresh(paths: &Paths) -> AutoRefresh {
    let mut result = AutoRefresh {
        enabled: false,
        skipped: None,
        attempted: Vec::new(),
        refreshed: Vec::new(),
        failed: Vec::new(),
    };
    if !crate::commands::settings::scores_auto_refresh(&paths.home) {
        result.skipped = Some("off");
        return result;
    }
    result.enabled = true;
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        result.skipped = Some("worker");
        return result;
    }
    let key_of = |key: &str| crate::commands::settings::secret(&paths.home, key).unwrap_or_default();
    let asl_key = key_of("aistupidlevel-api-key");
    let aa_key = key_of("artificialanalysis-api-key");
    let asl_due = !asl_key.is_empty()
        && source_due(&paths.home.join("model-scores.json"), CACHE_SCHEMA);
    let aa_due = !aa_key.is_empty()
        && source_due(&paths.home.join("model-scores-aa.json"), AA_SCHEMA);
    // Agent Arena needs no key (RJ-2).
    let arena_due = source_due(&arena::cache_path(paths), arena::SCHEMA);
    if !asl_due && !aa_due && !arena_due {
        return result;
    }
    let lock = paths.home.join(".model-scores.refresh.lock");
    match acquire_lock(&lock, &create_lock_file) {
        LockOutcome::Held => {}
        LockOutcome::Busy => {
            result.skipped = Some("busy");
            return result;
        }
        LockOutcome::Failed(error) => {
            for (name, due) in [
                ("AI Stupid Level", asl_due),
                ("Artificial Analysis", aa_due),
                (arena::NAME, arena_due),
            ] {
                if due {
                    result.failed.push((name, one_line(&error.to_string())));
                }
            }
            return result;
        }
    }
    let mut outcomes = Vec::new();
    if asl_due {
        outcomes.push((
            "AI Stupid Level",
            refresh_asl_cache(paths, &asl_key, AUTO_FETCH_MAX_TIME_SECONDS),
        ));
    }
    if aa_due {
        outcomes.push((
            "Artificial Analysis",
            refresh_aa_cache(paths, &aa_key, AUTO_FETCH_MAX_TIME_SECONDS),
        ));
    }
    if arena_due {
        // The index and every board share one budget; `Ok(false)` (same sha,
        // only `fetched_at` moved) counts as attempted, not refreshed.
        let budget = arena::Budget::seconds(arena::AUTO_BUDGET_SECONDS);
        outcomes.push((arena::NAME, arena::refresh(paths, &budget)));
    }
    let _ = fs::remove_file(&lock);
    for (name, outcome) in outcomes {
        result.attempted.push(name);
        match outcome {
            Ok(true) => result.refreshed.push(name),
            Ok(false) => {}
            Err(error) => result.failed.push((name, one_line(&format!("{error:#}")))),
        }
    }
    result
}

fn curl_failure_message(url: &str, stderr: &str) -> String {
    let mut message = format!("fetching {url} with curl failed: {stderr}");
    if stderr.contains("401") {
        message.push_str(&format!(
            "; key is missing or invalid; create one at {KEY_URL}"
        ));
    }
    if stderr.contains("429") {
        message.push_str("; the free plan limit (10 requests per day, 1 per minute) is exhausted; keeping the existing cache");
    }
    message
}

#[cfg(test)]
mod tests {
    use super::{
        aa_name_parts, acquire_lock, LockOutcome, curl_args, curl_failure_message, due_for_auto_refresh, lock_is_stale,
        normalized, staleness, strip_model_date,
    };
    use std::path::Path;
    use std::time::{Duration as StdDuration, SystemTime};
    use time::format_description::well_known::Rfc3339;
    use time::{Duration, OffsetDateTime};

    fn ago(hours: i64) -> String {
        (OffsetDateTime::now_utc() - Duration::hours(hours))
            .format(&Rfc3339)
            .unwrap()
    }

    #[test]
    fn due_for_auto_refresh_needs_a_missing_or_day_old_cache() {
        assert!(due_for_auto_refresh(None));
        assert!(due_for_auto_refresh(Some("garbage")));
        assert!(!due_for_auto_refresh(Some(&ago(23))));
        assert!(due_for_auto_refresh(Some(&ago(25))));
    }

    #[test]
    fn lock_is_stale_after_two_minutes_of_modification_time() {
        let now = SystemTime::now();
        let before = |seconds: u64| now - StdDuration::from_secs(seconds);
        assert!(!lock_is_stale(now, now));
        assert!(!lock_is_stale(before(119), now));
        assert!(lock_is_stale(before(121), now));
        assert!(lock_is_stale(before(600), now));
        assert!(!lock_is_stale(now + StdDuration::from_secs(5), now));
    }

    #[test]
    fn a_failed_retry_after_reclaiming_a_stale_lock_is_busy_not_failed() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("lock");
        std::fs::write(&lock, "").unwrap();
        let file = std::fs::File::options().write(true).open(&lock).unwrap();
        file.set_modified(SystemTime::now() - StdDuration::from_secs(600))
            .unwrap();
        drop(file);
        let calls = std::cell::Cell::new(0);
        let create = |_: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                Err(std::io::ErrorKind::AlreadyExists.into())
            } else {
                Err(std::io::ErrorKind::PermissionDenied.into())
            }
        };
        assert!(matches!(acquire_lock(&lock, &create), LockOutcome::Busy));
        assert_eq!(calls.get(), 2);
        assert!(!lock.exists(), "the stale lock was removed before the retry");
    }

    #[test]
    fn staleness_is_forty_eight_hours() {
        assert_eq!(staleness(None), (None, true));
        assert_eq!(staleness(Some("garbage")), (None, true));
        assert_eq!(staleness(Some(&ago(47))), (Some(47), false));
        assert_eq!(staleness(Some(&ago(48))), (Some(48), true));
        assert_eq!(staleness(Some(&ago(49))), (Some(49), true));
        assert_eq!(staleness(Some(&ago(-1))), (Some(0), false));
    }

    #[test]
    fn curl_max_time_is_a_parameter() {
        let key = "asl_live_test_key";
        for seconds in [8u64, 20] {
            let args: Vec<String> = curl_args("https://example.invalid/m", Path::new("s.tmp"), seconds, false)
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            let at = args.iter().position(|a| a == "--max-time").unwrap();
            assert_eq!(args[at + 1], seconds.to_string());
            assert!(args.iter().all(|a| !a.contains(key)));
        }
    }

    // RJ-2: which fetch follows redirects (Agent Arena's public requests
    // only; a keyed request never, so its header cannot reach another host)
    // is checked through `thoth models --refresh` with a curl stand-in on
    // PATH, in tests/arena.rs (review of attempt 2), not on `curl_args`.

    #[test]
    fn curl_failure_explains_401_and_429() {
        let unauthorized = curl_failure_message("https://example", "curl: (22) 401");
        assert!(unauthorized.contains("key is missing or invalid"));
        assert!(unauthorized.contains("https://aistupidlevel.info/account/data-keys"));
        let limited = curl_failure_message("https://example", "curl: (22) 429");
        assert!(limited.contains("10 requests per day, 1 per minute"));
        assert!(limited.contains("keeping the existing cache"));
    }

    #[test]
    fn curl_argv_does_not_contain_api_key() {
        let key = "asl_live_test_key";
        let args = curl_args("https://example.invalid/models", Path::new("scores.tmp"), 8, false);
        assert!(args.iter().all(|arg| !arg.to_string_lossy().contains(key)));
    }

    /// MODELS-1 (AC2): what `discover` reports for an antigravity profile from
    /// its stored list: no list (or an empty one) is the "none stored" note
    /// and no time; a list read today is the list, its time and no note; a
    /// list a day old or more keeps its time and gets the "N hours ago"
    /// note. (`thoth models` cannot be handed an old list through the CLI,
    /// so the note is checked here.)
    #[test]
    fn discover_reports_the_stored_agy_list_its_time_and_the_age_note() {
        use crate::adapter::ModelEntry;
        use crate::db::global::{Profile, StoredModels};
        let profile = Profile {
            profile_id: "agy-a".into(),
            provider: "google".into(),
            harness: "antigravity".into(),
            account_label: None,
            config_dir: "C:/cfg".into(),
            default_models: None,
            status: "active".into(),
        };
        let entry = |m: &str| ModelEntry { model: m.into(), display_name: format!("Name of {m}") };
        let none_note = "no model list stored yet; run `thoth profile check agy-a`";

        let d = super::discover(&profile, None);
        assert_eq!((d.source, d.note.as_deref(), d.seen_at.as_deref()), ("agy_models", Some(none_note), None));
        assert!(d.models.is_empty());
        let empty = StoredModels { models: vec![], seen_at: Some("2026-10-19T00:00:00Z".into()) };
        let d = super::discover(&profile, Some(&empty));
        assert_eq!((d.note.as_deref(), d.seen_at.as_deref()), (Some(none_note), None));

        let now = ago(0);
        let fresh = StoredModels { models: vec![entry("a-model"), entry("b-model")], seen_at: Some(now.clone()) };
        let d = super::discover(&profile, Some(&fresh));
        assert_eq!((d.source, d.note, d.seen_at.as_deref()), ("agy_models", None, Some(now.as_str())));
        assert_eq!(d.models, [("a-model".to_string(), "Name of a-model".to_string()), ("b-model".to_string(), "Name of b-model".to_string())]);

        let old_time = ago(30);
        let old = StoredModels { models: vec![entry("a-model")], seen_at: Some(old_time.clone()) };
        let d = super::discover(&profile, Some(&old));
        assert_eq!(d.seen_at.as_deref(), Some(old_time.as_str()));
        assert_eq!(d.note.as_deref(), Some("model list read 30 hours ago; run `thoth profile check agy-a` to read it again"));
        assert_eq!(d.models.len(), 1);
    }

    #[test]
    fn artificial_analysis_names_and_efforts_are_normalized() {
        assert_eq!(
            aa_name_parts("Claude Fable 5.1 (Adaptive Reasoning, Max Effort, Default Fallback)"),
            ("Claude Fable 5.1", Some("max".into()))
        );
        assert_eq!(
            aa_name_parts("GPT-5.6 Luna (low)"),
            ("GPT-5.6 Luna", Some("low".into()))
        );
        assert_eq!(
            aa_name_parts("GPT-5.6 Luna (xhigh)").1.as_deref(),
            Some("xhigh")
        );
        assert_eq!(aa_name_parts("Claude Opus 5.5"), ("Claude Opus 5.5", None));
        assert_eq!(normalized("GPT-5.6 Luna"), normalized("gpt-5.6-luna"));
        assert_eq!(
            normalized(&strip_model_date("claude-haiku-4-5-20251001")),
            normalized("Claude Haiku 4.5")
        );
    }
}
