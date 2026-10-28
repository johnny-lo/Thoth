//! Every user-owned setting, in one registry: where it is stored, which
//! environment variable overrides it and what it defaults to. The values live
//! in `$THOTH_HOME/config.json`; precedence is environment variable, then
//! config.json, then the built-in default (command-line flags beat all three).
//! The two score API keys live in `$THOTH_HOME/secrets.json` instead (see
//! `secrets`), with the same precedence, and are shown only as set or unset.

use crate::commands::arena::DEFAULT_ARENA_URL;
use crate::commands::models::{DEFAULT_AA_URL, DEFAULT_SCORES_URL};
use crate::commands::secrets;
use crate::config::{self, Paths};
use crate::output;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Once;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Switch,
    Language,
    Text,
    Integer,
    Path,
    Secret,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Kind::Switch => "switch",
            Kind::Language => "language",
            Kind::Text => "text",
            Kind::Integer => "integer",
            Kind::Path => "path",
            Kind::Secret => "secret",
        }
    }
}

pub(crate) struct Setting {
    pub(crate) key: &'static str,
    pub(crate) stored: Option<&'static str>,
    pub(crate) env: Option<&'static str>,
    pub(crate) kind: Kind,
    pub(crate) default: &'static str,
    pub(crate) settable: bool,
    pub(crate) description: &'static str,
}

const fn setting(
    key: &'static str,
    stored: Option<&'static str>,
    env: Option<&'static str>,
    kind: Kind,
    default: &'static str,
    settable: bool,
    description: &'static str,
) -> Setting {
    Setting { key, stored, env, kind, default, settable, description }
}

pub(crate) const REGISTRY: &[Setting] = &[
    setting("live-quota", Some("live_quota"), None, Kind::Switch, "off", true,
        "Query official live quota on --refresh, and for stale readings before recommend ranks."),
    setting("scores-auto-refresh", Some("scores_auto_refresh"), None, Kind::Switch, "off", true,
        "Refresh the external score caches automatically, at most once a day, before the commands that use them."),
    setting("require-reason", Some("require_reason"), None, Kind::Switch, "off", true,
        "Require --reason on every dispatch, review start and spec review start."),
    setting("language", Some("language"), Some("THOTH_LANG"), Kind::Language, "en", true,
        "Language of the human-readable output: en or zh-TW."),
    setting("claude-bin", Some("claude_bin"), Some("THOTH_CLAUDE_BIN"), Kind::Text, "claude", true,
        "The Claude Code executable."),
    setting("codex-bin", Some("codex_bin"), Some("THOTH_CODEX_BIN"), Kind::Text, "codex", true,
        "The Codex executable."),
    setting("agy-bin", Some("agy_bin"), Some("THOTH_AGY_BIN"), Kind::Text, "agy", true,
        "The Antigravity (agy) executable."),
    setting("live-quota-timeout-ms", Some("live_quota_timeout_ms"), Some("THOTH_LIVE_QUOTA_TIMEOUT_MS"), Kind::Integer, "30000", true,
        "How long a live quota query waits for the official CLI, in milliseconds."),
    setting("scores-url", Some("scores_url"), Some("THOTH_SCORES_URL"), Kind::Text, DEFAULT_SCORES_URL, true,
        "Address of the AI Stupid Level model scores."),
    setting("aa-url", Some("aa_url"), Some("THOTH_AA_URL"), Kind::Text, DEFAULT_AA_URL, true,
        "Address of the Artificial Analysis model scores."),
    setting("arena-url", Some("arena_url"), Some("THOTH_ARENA_URL"), Kind::Text, DEFAULT_ARENA_URL, true,
        "Address of the Hugging Face Hub that serves the Agent Arena leaderboard dataset (lmarena-ai/leaderboard-dataset)."),
    setting("machine-name", Some("machine_name"), Some("THOTH_MACHINE_NAME"), Kind::Text, "", true,
        "This computer's name in the shared records folder (the file it writes is NAME.jsonl); the default is the host name."),
    setting("records-dir", Some("records_dir"), Some("THOTH_RECORDS_DIR"), Kind::Text, "unset", true,
        "Absolute path of the shared records folder; every run of this computer is written to NAME.jsonl there and the other computers' files are read from it. unset: runs are kept only in this computer's databases."),
    setting("home", None, Some("THOTH_HOME"), Kind::Path, "~/.thoth", false,
        "The state directory; set by the THOTH_HOME environment variable only."),
    setting("aistupidlevel-api-key", None, Some("AISTUPIDLEVEL_API_KEY"), Kind::Secret, "unset", false,
        "API key for the AI Stupid Level scores; shown only as set or unset; store it with thoth config set-secret."),
    setting("artificialanalysis-api-key", None, Some("ARTIFICIALANALYSIS_API_KEY"), Kind::Secret, "unset", false,
        "API key for the Artificial Analysis scores; shown only as set or unset; store it with thoth config set-secret."),
];

fn find(key: &str) -> Option<&'static Setting> {
    REGISTRY.iter().find(|s| s.key == key)
}

/// The environment variables of the secret settings, in registry order:
/// taken out of every worker's environment (RD-2, H15).
pub(crate) fn secret_env_vars() -> Vec<&'static str> {
    REGISTRY.iter().filter(|s| s.kind == Kind::Secret).filter_map(|s| s.env).collect()
}

/// The keys that may appear in config.json.
pub(crate) fn stored_keys() -> Vec<&'static str> {
    REGISTRY.iter().filter_map(|s| s.stored).collect()
}

/// Stored keys of settings that were taken out (RJ-1): an old config.json may
/// still hold them. They have no effect and are not reported as unknown.
pub(crate) const RETIRED_KEYS: &[&str] = &["same_provider_rotation"];

/// `config get`, `set` and `unset` refuse a retired setting by name.
fn refuse_retired(key: &str) -> Result<()> {
    if RETIRED_KEYS.iter().any(|k| k.replace('_', "-") == key) {
        bail!("the setting {key} was removed and has no effect; Thoth no longer picks profiles for you");
    }
    Ok(())
}

pub(crate) struct Loaded {
    pub(crate) exists: bool,
    pub(crate) map: Map<String, Value>,
    pub(crate) problem: Option<String>,
}

/// Reads `home/config.json`; prints nothing.
pub(crate) fn load(home: &Path) -> Loaded {
    let path = home.join("config.json");
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded { exists: false, map: Map::new(), problem: None }
        }
        Err(e) => return Loaded { exists: true, map: Map::new(), problem: Some(e.to_string()) },
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Loaded { exists: true, map, problem: None },
        Ok(_) => Loaded { exists: true, map: Map::new(), problem: Some("not a JSON object".to_string()) },
        Err(e) => Loaded { exists: true, map: Map::new(), problem: Some(e.to_string()) },
    }
}

pub(crate) struct Resolved {
    pub(crate) value: String,
    pub(crate) source: &'static str,
}

/// The value of the setting's environment variable when it counts as set.
fn env_value(s: &Setting) -> Option<String> {
    let raw = std::env::var(s.env?).ok()?;
    match s.kind {
        Kind::Switch => None,
        Kind::Language => {
            let t = raw.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Kind::Text | Kind::Path | Kind::Secret => (!raw.trim().is_empty()).then_some(raw),
        Kind::Integer => raw.trim().parse::<u64>().ok().filter(|n| *n >= 1).map(|n| n.to_string()),
    }
}

fn normalize_language(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "en" => Some("en"),
        "zh-tw" | "zh_tw" => Some("zh-TW"),
        _ => None,
    }
}

/// The stored value when it is present and of the right type. A secret's
/// stored value comes from `secrets` (secrets.json) and reads back as `set`.
fn file_value(s: &Setting, map: &Map<String, Value>, secrets: &Map<String, Value>) -> Option<String> {
    if s.kind == Kind::Secret {
        return secret_file_value(s, secrets).map(|_| "set".to_string());
    }
    let v = map.get(s.stored?)?;
    match s.kind {
        Kind::Switch => v.as_bool().map(|b| if b { "on" } else { "off" }.to_string()),
        Kind::Text => v.as_str().filter(|t| !t.trim().is_empty()).map(str::to_string),
        Kind::Integer => v.as_u64().filter(|n| *n >= 1).map(|n| n.to_string()),
        Kind::Language => v.as_str().and_then(normalize_language).map(str::to_string),
        Kind::Path | Kind::Secret => None,
    }
}

/// The key stored in secrets.json for `s`, trimmed, when it is a non-blank
/// string; the only function that hands the content on.
fn secret_file_value(s: &Setting, secrets: &Map<String, Value>) -> Option<String> {
    secrets
        .get(&secrets::secret_stored_name(s))?
        .as_str()
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
}

/// The secrets.json keys that count for `s`: loaded only for a secret, and
/// never while THOTH_RUN_ID is set (a worker's thoth sees the environment
/// only, G8). Every other setting gets an empty map without opening the file.
fn secrets_for(s: &Setting, home: Option<&Path>) -> Map<String, Value> {
    match home {
        Some(home) if s.kind == Kind::Secret && std::env::var_os("THOTH_RUN_ID").is_none() => {
            secrets::load_secrets(home).map
        }
        _ => Map::new(),
    }
}

/// The API key `key` names, for the code that spends it (`models --refresh`
/// and the automatic refresh): the environment variable when it is set and
/// not blank, else the value stored in secrets.json, unless THOTH_RUN_ID is
/// set, in which case the file is not opened. `None` for anything that is
/// not a secret.
pub(crate) fn secret(home: &Path, key: &str) -> Option<String> {
    let s = find(key).filter(|s| s.kind == Kind::Secret)?;
    if let Some(v) = env_value(s) {
        return Some(v.trim().to_string());
    }
    secret_file_value(s, &secrets_for(s, Some(home)))
}

fn default_value(s: &Setting) -> String {
    if s.key == "home" {
        return config::probe_state_dir()
            .map(|p| p.home.display().to_string())
            .unwrap_or_else(|_| s.default.to_string());
    }
    if s.key == "machine-name" {
        return default_machine_name();
    }
    s.default.to_string()
}

const UNKNOWN_MACHINE: &str = "unknown-machine";

fn valid_machine_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.starts_with(['.', '-'])
}

/// Makes any string usable as a file stem in the records folder.
fn sanitize_machine_name(raw: &str) -> String {
    let replaced: String = raw
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '-' })
        .collect();
    let trimmed = replaced.trim_start_matches(['.', '-']);
    let cut: String = trimmed.chars().take(64).collect();
    if cut.is_empty() {
        UNKNOWN_MACHINE.to_string()
    } else {
        cut
    }
}

/// The host name, from the environment or /etc/hostname only.
pub(crate) fn default_machine_name() -> String {
    let raw = if cfg!(windows) {
        std::env::var("COMPUTERNAME").ok().filter(|v| !v.trim().is_empty())
    } else {
        std::env::var("HOSTNAME")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| {
                fs::read_to_string("/etc/hostname")
                    .ok()
                    .and_then(|t| t.lines().next().map(|l| l.trim().to_string()))
                    .filter(|l| !l.is_empty())
            })
    };
    sanitize_machine_name(raw.as_deref().unwrap_or(UNKNOWN_MACHINE))
}

/// This computer's name in the shared records folder; never fails.
pub(crate) fn machine_name(home: &Path) -> String {
    match resolve(home, "machine-name") {
        Some(r) => sanitize_machine_name(&r.value),
        None => UNKNOWN_MACHINE.to_string(),
    }
}

/// The shared records folder, when one is set.
pub(crate) fn records_dir(home: &Path) -> Option<PathBuf> {
    warn_once(home);
    let r = resolve(home, "records-dir")?;
    (r.source != "default").then(|| PathBuf::from(r.value))
}

fn resolve_in(s: &Setting, map: &Map<String, Value>, secrets: &Map<String, Value>) -> Resolved {
    if let Some(v) = env_value(s) {
        let value = if s.kind == Kind::Secret { "set".to_string() } else { v };
        return Resolved { value, source: "env" };
    }
    if let Some(value) = file_value(s, map, secrets) {
        return Resolved { value, source: "file" };
    }
    Resolved { value: default_value(s), source: "default" }
}

/// The effective value of `key` and where it came from; `None` for an
/// unknown key.
pub(crate) fn resolve(home: &Path, key: &str) -> Option<Resolved> {
    let s = find(key)?;
    Some(resolve_in(s, &load(home).map, &secrets_for(s, Some(home))))
}

/// For callers without a `Paths`: never fails, prints nothing. Falls back to
/// the environment variable and the default when the state directory cannot
/// be worked out.
pub(crate) fn effective(key: &str) -> String {
    let Some(s) = find(key) else { return String::new() };
    let home = match std::env::var_os("THOTH_HOME") {
        Some(v) => Some(PathBuf::from(v)),
        None => config::probe_state_dir().ok().map(|p| p.home),
    };
    let map = home.as_deref().map(|h| load(h).map).unwrap_or_default();
    resolve_in(s, &map, &secrets_for(s, home.as_deref())).value
}

/// Says once per process that config.json cannot be used.
pub(crate) fn warn_once(home: &Path) {
    static WARNED: Once = Once::new();
    WARNED.call_once(|| {
        if let Some(problem) = load(home).problem {
            masked_eprintln!(
                "warning: {} cannot be used ({problem}); every setting stored in it is ignored until it is fixed; see thoth config list",
                home.join("config.json").display()
            );
        }
    });
}

fn switch_on(home: &Path, key: &str) -> bool {
    resolve(home, key).is_some_and(|r| r.value == "on")
}

/// Whether dispatch, review start and spec review start refuse a launch that
/// has no `--reason`. Forgiving like `read_flags`: anything unreadable means
/// off.
pub(crate) fn require_reason(home: &Path) -> bool {
    warn_once(home);
    switch_on(home, "require-reason")
}

/// Whether the external score caches may refresh themselves. Forgiving like
/// `read_flags`: anything unreadable means off.
pub(crate) fn scores_auto_refresh(home: &Path) -> bool {
    warn_once(home);
    switch_on(home, "scores-auto-refresh")
}

/// Read the live-quota switch used by read-only commands. Deliberately
/// forgiving: a missing, malformed, or incomplete file means off.
pub(crate) fn read_flags(home: &Path) -> bool {
    warn_once(home);
    switch_on(home, "live-quota")
}

fn legal_keys() -> String {
    REGISTRY.iter().map(|s| s.key).collect::<Vec<_>>().join(", ")
}

fn validate_key(key: &str) -> Result<&'static Setting> {
    find(key).ok_or_else(|| {
        anyhow::anyhow!(
            "invalid setting key '{key}'; legal keys: {}; switches take on or off",
            legal_keys()
        )
    })
}

fn source_label(s: &Setting, source: &str) -> String {
    match source {
        "env" => format!("env {}", s.env.unwrap_or_default()),
        "file" if s.kind == Kind::Secret => secrets::FILE_NAME.to_string(),
        "file" => "config.json".to_string(),
        _ => "default".to_string(),
    }
}

fn stored_string(v: &Value) -> String {
    match v {
        Value::Bool(b) => if *b { "on" } else { "off" }.to_string(),
        Value::String(t) => t.clone(),
        other => other.to_string(),
    }
}

fn count_profiles(paths: &Paths) -> Option<usize> {
    let db = paths.global_db();
    if !db.is_file() {
        return None;
    }
    super::doctor::open_read_only(&db)
        .and_then(|conn| crate::db::global::list_profiles(&conn))
        .ok()
        .map(|l| l.len())
}

/// (total, approved, protected patterns) of the repository `cwd` is in.
fn count_checks(paths: &Paths, cwd: &Path) -> Option<(usize, usize, usize)> {
    let root = crate::worktree::repo_root_from(cwd).ok()?;
    let repo_id = crate::ids::repo_id_for(&root).ok()?;
    let db = paths.state_db(&repo_id);
    if !db.is_file() {
        return None;
    }
    let conn = super::doctor::open_read_only(&db).ok()?;
    let list = crate::checks::list_checks(&conn).ok()?;
    let approved = list.iter().filter(|c| c.approved_at.is_some()).count();
    let patterns = crate::protected::load_patterns(&conn).ok()?.len();
    Some((list.len(), approved, patterns))
}

pub fn list(paths: &Paths, cwd: &Path, json_mode: bool) -> Result<i32> {
    let loaded = load(&paths.home);
    let config_path = paths.home.join("config.json");
    let known = stored_keys();
    let mut unknown_keys: Vec<&str> = loaded
        .map
        .keys()
        .map(String::as_str)
        .filter(|k| !known.contains(k) && !RETIRED_KEYS.contains(k))
        .collect();
    unknown_keys.sort_unstable();
    let secrets_report = secrets::Report::inspect(&paths.home);
    let secrets_map = secrets_report.map();

    let mut settings = Vec::new();
    let mut lines = Vec::new();
    for s in REGISTRY {
        let r = resolve_in(s, &loaded.map, &secrets_map);
        lines.push(format!("{}: {} ({})", s.key, r.value, source_label(s, r.source)));
        settings.push(json!({
            "key": s.key,
            "value": r.value,
            "source": r.source,
            "env": s.env,
            "settable": s.settable,
            "kind": s.kind.name(),
            "default": default_value(s),
            "description": s.description,
        }));
    }
    let profiles = count_profiles(paths);
    let checks = count_checks(paths, cwd);
    let doc = json!({
        "settings": settings,
        "config_file": {
            "path": config_path.display().to_string(),
            "exists": loaded.exists,
            "ok": loaded.problem.is_none(),
            "error": loaded.problem,
            "unknown_keys": unknown_keys,
        },
        "secrets_file": secrets_report.json(true),
        "elsewhere": {
            "profiles": {"count": profiles, "command": "thoth profile list"},
            "checks": checks.map(|(total, approved, patterns)| json!({
                "total": total,
                "approved": approved,
                "protected_patterns": patterns,
                "command": "thoth checks list",
            })),
        },
    });
    lines.push(String::new());
    lines.push("elsewhere:".to_string());
    lines.push(format!("profiles: {} registered (thoth profile list)", profiles.unwrap_or(0)));
    lines.push(match checks {
        Some((total, approved, patterns)) => format!(
            "checks: {total} ({approved} approved), protected patterns: {patterns} (thoth checks list)"
        ),
        None => "checks: not an initialized repository (thoth init)".to_string(),
    });
    lines.extend(secrets_report.warnings());
    if let Some(problem) = &loaded.problem {
        lines.push(format!(
            "warning: {} cannot be used ({problem}); the values above are defaults or environment variables",
            config_path.display()
        ));
    }
    if !unknown_keys.is_empty() {
        lines.push(format!(
            "warning: {} has unknown keys: {} (ignored)",
            config_path.display(),
            unknown_keys.join(", ")
        ));
    }
    output::emit(json_mode, &doc, &lines.join("\n"));
    Ok(0)
}

pub fn get(paths: &Paths, key: &str, json_mode: bool) -> Result<i32> {
    refuse_retired(key)?;
    let s = validate_key(key)?;
    let value = resolve_in(s, &load(&paths.home).map, &secrets_for(s, Some(&paths.home))).value;
    output::emit(json_mode, &json!({"key": key, "value": value}), &format!("{key}: {value}"));
    Ok(0)
}

fn notice(key: &str) -> Option<&'static str> {
    Some(match key {
        "live-quota" => "When enabled, Thoth queries the official CLIs (Antigravity's agy, the codex app-server and Claude Code's /usage command) for live quota at two times: when you run `thoth profile usage --refresh`, for every agy, codex and Claude Code profile; and when `thoth recommend` is about to rank profiles, once for each agy and codex profile whose newest reading is older than 10 minutes (or that has none). Claude Code profiles are only asked on --refresh. A codex or Claude Code profile with a run in progress is skipped; a query that fails keeps the previous reading and prints a warning. agy, codex and Claude Code each use their own logged-in account to query their provider; Thoth does not read login credentials. To disable it, run `thoth config set live-quota off`.",
        "scores-auto-refresh" => "When enabled, thoth models, thoth report and thoth recommend refresh the external score caches before reading them, but only when a cache is missing or at least 24 hours old. With AISTUPIDLEVEL_API_KEY set (in the environment or stored with thoth config set-secret) Thoth connects to aistupidlevel.info, and with ARTIFICIALANALYSIS_API_KEY set it connects to artificialanalysis.ai; a source whose key is not set is never contacted. The Agent Arena leaderboard needs no key: Thoth connects to huggingface.co for it (the whole fetch gives up after 20 seconds). If a refresh fails, the old cache is kept and a warning is printed. To disable it, run `thoth config set scores-auto-refresh off`.",
        "require-reason" => "When enabled, every thoth dispatch, thoth review start and thoth spec review start needs a --reason saying why that profile, model and effort were chosen, and is refused without one; the reason is recorded in the launch_choice event. To disable it, run `thoth config set require-reason off`.",
        _ => return None,
    })
}

/// A setting that can be stored; refuses `home` and the API keys.
fn storable(key: &str) -> Result<(&'static Setting, &'static str)> {
    let s = validate_key(key)?;
    match s.stored {
        Some(stored) if s.settable => Ok((s, stored)),
        _ if s.kind == Kind::Path => {
            bail!("home cannot be stored in config.json: set THOTH_HOME instead")
        }
        _ if s.kind == Kind::Secret => bail!(
            "{key} is a secret: in your own terminal run thoth config set-secret {key} (the value is read from stdin or a hidden prompt, never from the command line), or set the environment variable {var}; thoth config unset-secret {key} removes a stored one",
            key = s.key,
            var = s.env.unwrap_or_default()
        ),
        _ => bail!(
            "{} cannot be stored in config.json yet: set the environment variable {}",
            s.key,
            s.env.unwrap_or_default()
        ),
    }
}

/// The secret `key` names, for `set-secret` and `unset-secret`. An unknown
/// key is not repeated: a key pasted where KEY belongs must not be echoed.
/// `set-secret home` keeps `storable`'s message; `unset-secret home` is
/// pointed at `config unset` like any other setting that is not a secret.
fn secret_setting(key: &str, verb: &str) -> Result<&'static Setting> {
    let Some(s) = find(key) else {
        bail!(
            "invalid secret key (the argument is not repeated); set-secret accepts aistupidlevel-api-key or artificialanalysis-api-key; legal keys: {}",
            legal_keys()
        )
    };
    match s.kind {
        Kind::Secret => Ok(s),
        Kind::Path if verb == "set" => bail!("home cannot be stored in config.json: set THOTH_HOME instead"),
        _ if verb == "set" => bail!("{} is not a secret: use thoth config set {} VALUE", s.key, s.key),
        _ => bail!("{} is not a secret: use thoth config unset {}", s.key, s.key),
    }
}

/// `thoth config set-secret KEY`: the value comes from a hidden prompt or
/// stdin, never from `argv`, and goes only to secrets.json.
pub fn set_secret(paths: &Paths, key: &str, argv_values: &[String], json_mode: bool) -> Result<i32> {
    refuse_in_run("set-secret")?;
    let s = secret_setting(key, "set")?;
    if !argv_values.is_empty() {
        bail!("the value must not be given on the command line (it would be visible in process lists and shell history); nothing was stored. Run `thoth config set-secret {key}` again and type it at the hidden prompt, or pipe it on stdin");
    }
    let var = s.env.unwrap_or_default();
    let value = secrets::read_secret_value(key, var)?;
    let stored = secrets::secret_stored_name(s);
    let previous = secrets::write_secret(paths, &stored, Some(&value))?;
    drop(value);
    let shadowed_by = env_value(s).and(s.env);
    let path = secrets::path_in(&paths.home);
    let path = std::path::absolute(&path).unwrap_or(path);
    output::emit(
        json_mode,
        &json!({
            "ok": true,
            "key": key,
            "value": "set",
            "source": "file",
            "previous": if previous { "set" } else { "unset" },
            "shadowed_by": shadowed_by,
            "path": path.display().to_string(),
        }),
        &format!("set-secret {key}: set ({})", secrets::FILE_NAME),
    );
    if !json_mode {
        if let Some(var) = shadowed_by {
            masked_eprintln!("note: {var} is set in this environment and takes precedence here; the stored key applies where {var} is not set");
        }
    }
    Ok(0)
}

/// `thoth config unset-secret KEY`: removes the stored key; the file stays
/// (as `{}` after the last one) and is not created when absent.
pub fn unset_secret(paths: &Paths, key: &str, json_mode: bool) -> Result<i32> {
    refuse_in_run("unset-secret")?;
    let s = secret_setting(key, "unset")?;
    let previous = if secrets::path_in(&paths.home).exists() {
        secrets::write_secret(paths, &secrets::secret_stored_name(s), None)?
    } else {
        false
    };
    let now = resolve(&paths.home, key).unwrap_or(Resolved { value: "unset".to_string(), source: "default" });
    output::emit(
        json_mode,
        &json!({
            "ok": true,
            "key": key,
            "previous": if previous { "set" } else { "unset" },
            "value": now.value,
            "source": now.source,
        }),
        &format!("unset-secret {key}: now {} ({})", now.value, source_label(s, now.source)),
    );
    Ok(0)
}

/// The JSON value to store for `raw`, plus the string it reads back as.
fn parse_value(s: &Setting, raw: &str) -> Result<(Value, String)> {
    match s.kind {
        Kind::Switch => match raw {
            "on" => Ok((Value::Bool(true), "on".to_string())),
            "off" => Ok((Value::Bool(false), "off".to_string())),
            _ => bail!("invalid setting value '{raw}'; legal keys: live-quota, scores-auto-refresh, require-reason; values: on, off"),
        },
        Kind::Text => {
            let t = raw.trim();
            if t.is_empty() {
                bail!("invalid setting value for {}: must not be empty", s.key)
            }
            if s.key == "machine-name" && !valid_machine_name(t) {
                bail!("invalid setting value '{raw}' for machine-name: 1 to 64 characters from A-Z, a-z, 0-9, '.', '_' and '-', not starting with '.' or '-' (it names this computer's file in the records folder)")
            }
            if s.key == "records-dir" && !Path::new(t).is_absolute() {
                bail!("invalid setting value '{raw}' for records-dir: must be an absolute path to the shared folder")
            }
            Ok((Value::String(t.to_string()), t.to_string()))
        }
        Kind::Integer => {
            let n = raw
                .parse::<u64>()
                .ok()
                .filter(|n| *n >= 1 && raw.bytes().all(|b| b.is_ascii_digit()));
            match n {
                Some(n) => Ok((json!(n), n.to_string())),
                None => bail!(
                    "invalid setting value '{raw}' for {}: a whole number of milliseconds, at least 1",
                    s.key
                ),
            }
        }
        Kind::Language => match normalize_language(raw) {
            Some(l) => Ok((Value::String(l.to_string()), l.to_string())),
            None => bail!("invalid setting value '{raw}' for language: en or zh-TW"),
        },
        Kind::Path | Kind::Secret => unreachable!("not settable"),
    }
}

fn refuse_in_run(verb: &str) -> Result<()> {
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!("config {verb} is only for the human operator; it is refused while THOTH_RUN_ID is set")
    }
    Ok(())
}

pub fn set(paths: &Paths, key: &str, raw_value: &str, json_mode: bool) -> Result<i32> {
    refuse_in_run("set")?;
    refuse_retired(key)?;
    let (s, storage) = storable(key)?;
    let (value, shown) = parse_value(s, raw_value)?;
    let previous = write_value(paths, storage, Some(value))?;
    let previous = if s.kind == Kind::Switch {
        let was_on = previous.as_ref().and_then(Value::as_bool).unwrap_or(false);
        Some(if was_on { "on" } else { "off" }.to_string())
    } else {
        previous.map(|v| stored_string(&v))
    };
    let shown = if s.kind == Kind::Switch { raw_value.to_string() } else { shown };
    let shadowed_by = env_value(s).and(s.env);
    let notice = if s.kind == Kind::Switch && shown == "on" { notice(key) } else { None };
    output::emit(
        json_mode,
        &json!({"ok": true, "key": key, "value": shown, "previous": previous, "notice": notice, "shadowed_by": shadowed_by}),
        &format!("set {key} {shown}"),
    );
    if !json_mode {
        if let Some(var) = shadowed_by {
            masked_eprintln!("note: {var} is set in this environment and takes precedence here; the stored value applies where {var} is not set");
        }
        if let Some(text) = notice {
            masked_eprintln!("{text}");
        }
    }
    Ok(0)
}

pub fn unset(paths: &Paths, key: &str, json_mode: bool) -> Result<i32> {
    refuse_in_run("unset")?;
    refuse_retired(key)?;
    let (s, storage) = storable(key)?;
    let path = paths.home.join("config.json");
    let previous = if path.exists() {
        write_value(paths, storage, None)?
    } else {
        None
    };
    let previous = previous.map(|v| stored_string(&v));
    let now = resolve(&paths.home, key).unwrap_or(Resolved { value: String::new(), source: "default" });
    output::emit(
        json_mode,
        &json!({"ok": true, "key": key, "previous": previous, "value": now.value, "source": now.source}),
        &format!("unset {key}: now {} ({})", now.value, source_label(s, now.source)),
    );
    Ok(0)
}

/// Writes one switch (`key` is the stored name, e.g. `live_quota`) into
/// config.json atomically, printing nothing; returns its previous value.
/// `thoth setup` calls it too.
pub(crate) fn set_flag(paths: &Paths, key: &str, enabled: bool) -> Result<bool> {
    let previous = write_value(paths, key, Some(Value::Bool(enabled)))?;
    Ok(previous.and_then(|v| v.as_bool()).unwrap_or(false))
}

/// Stores (`Some`) or removes (`None`) one key of config.json atomically,
/// keeping every other key; returns what was stored before.
fn write_value(paths: &Paths, storage: &str, value: Option<Value>) -> Result<Option<Value>> {
    let path = paths.home.join("config.json");
    let mut object = if !path.exists() {
        Map::new()
    } else {
        let text = fs::read_to_string(&path)
            .with_context(|| format!("cannot read settings file {}", path.display()))?;
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(object)) => object,
            _ => bail!("settings file {} is not a valid JSON object", path.display()),
        }
    };
    let previous = match value {
        Some(v) => object.insert(storage.to_string(), v),
        None => object.remove(storage),
    };
    fs::create_dir_all(&paths.home)?;
    let temp = paths.home.join(format!("config.json.tmp-{}", std::process::id()));
    fs::write(&temp, serde_json::to_vec_pretty(&Value::Object(object))?)?;
    fs::rename(&temp, &path)
        .with_context(|| format!("cannot atomically replace {}", path.display()))?;
    Ok(previous)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn restore(vars: &[(&str, Option<std::ffi::OsString>)]) {
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn secret_env_vars_are_the_two_score_keys() {
        assert_eq!(secret_env_vars(), ["AISTUPIDLEVEL_API_KEY", "ARTIFICIALANALYSIS_API_KEY"]);
    }

    #[test]
    fn effective_prefers_env_then_file_then_default() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let names = ["THOTH_HOME", "THOTH_CLAUDE_BIN", "THOTH_LIVE_QUOTA_TIMEOUT_MS", "THOTH_LANG"];
        let saved: Vec<_> = names.iter().map(|k| (*k, std::env::var_os(k))).collect();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("THOTH_HOME", dir.path());
        std::env::remove_var("THOTH_CLAUDE_BIN");
        std::env::remove_var("THOTH_LIVE_QUOTA_TIMEOUT_MS");
        std::env::remove_var("THOTH_LANG");

        assert_eq!(effective("claude-bin"), "claude");
        assert_eq!(effective("live-quota-timeout-ms"), "30000");

        fs::write(
            dir.path().join("config.json"),
            r#"{"claude_bin":"/f","live_quota_timeout_ms":5,"language":"zh-TW","scores_url":7}"#,
        )
        .unwrap();
        assert_eq!(effective("claude-bin"), "/f");
        assert_eq!(effective("live-quota-timeout-ms"), "5");
        assert_eq!(effective("language"), "zh-TW");
        assert_eq!(effective("scores-url"), DEFAULT_SCORES_URL);

        std::env::set_var("THOTH_CLAUDE_BIN", "/e");
        std::env::set_var("THOTH_LIVE_QUOTA_TIMEOUT_MS", "0");
        assert_eq!(effective("claude-bin"), "/e");
        assert_eq!(effective("live-quota-timeout-ms"), "5");

        restore(&saved);
    }

    #[test]
    fn machine_names_are_sanitised_and_validated() {
        assert_eq!(sanitize_machine_name("My PC (home)"), "My-PC--home-");
        assert_eq!(sanitize_machine_name("-.x"), "x");
        assert_eq!(sanitize_machine_name(""), "unknown-machine");
        assert_eq!(sanitize_machine_name("---"), "unknown-machine");
        assert_eq!(sanitize_machine_name(&"a".repeat(65)), "a".repeat(64));
        assert!(valid_machine_name("host-1.local"));
        assert!(!valid_machine_name("a b"));
        assert!(!valid_machine_name(".a"));
        assert!(!valid_machine_name(""));
        assert!(!valid_machine_name(&"a".repeat(65)));
    }

    #[test]
    fn records_settings_prefer_env_then_file_then_default() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let names = ["THOTH_HOME", "THOTH_MACHINE_NAME", "THOTH_RECORDS_DIR"];
        let saved: Vec<_> = names.iter().map(|k| (*k, std::env::var_os(k))).collect();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("THOTH_HOME", dir.path());
        std::env::remove_var("THOTH_MACHINE_NAME");
        std::env::remove_var("THOTH_RECORDS_DIR");

        assert_eq!(records_dir(dir.path()), None);
        let default_name = machine_name(dir.path());
        assert!(!default_name.is_empty());
        assert!(valid_machine_name(&default_name));

        fs::write(
            dir.path().join("config.json"),
            r#"{"machine_name":"alpha","records_dir":"/srv/records"}"#,
        )
        .unwrap();
        assert_eq!(machine_name(dir.path()), "alpha");
        assert_eq!(records_dir(dir.path()), Some(PathBuf::from("/srv/records")));

        std::env::set_var("THOTH_MACHINE_NAME", "env box");
        assert_eq!(machine_name(dir.path()), "env-box");
        std::env::set_var("THOTH_RECORDS_DIR", "/env/records");
        assert_eq!(records_dir(dir.path()), Some(PathBuf::from("/env/records")));

        restore(&saved);
    }
}
