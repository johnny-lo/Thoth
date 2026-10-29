//! `thoth profile login` (setup scope SETUP-2): open a CLI's own login
//! screen in this terminal, with the profile's directory and environment
//! already set, then register the profile once the CLI itself says it is
//! logged in.
//!
//! Thoth never reads, lists or copies anything in the profile directory or
//! the CLIs' own directories: whether the login worked comes only from
//! `profile::probe_auth_profile` (the CLI's own status command). Nothing is
//! written for a profile that is not logged in, so running it again is safe.
//! Only a person at a terminal may run it: it refuses inside a run
//! (`THOTH_RUN_ID`) and without a terminal on stdin and stdout.

use crate::adapter::{antigravity, HARNESS_CODEX};
use crate::commands::profile;
use crate::config::Paths;
use crate::db;
use crate::db::global::{self, Profile};
use crate::lang::{self, Lang};
use crate::output;
use crate::plan_label;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

/// Test-only switch: `1` skips the terminal check (the tests' stdio is
/// piped). It never skips the `THOTH_RUN_ID` refusal. `thoth setup` reads the
/// same variable the same way.
pub(crate) const SKIP_TTY_CHECK_VAR: &str = "THOTH_LOGIN_SKIP_TTY_CHECK";

pub struct LoginArgs<'a> {
    pub id: Option<&'a str>,
    pub harness: Option<&'a str>,
    pub provider: Option<&'a str>,
    pub label: Option<&'a str>,
    pub json: bool,
}

/// The first of `<prefix>-a` ... `<prefix>-z` that `taken` says is free.
pub fn next_free_id(prefix: &str, taken: &dyn Fn(&str) -> bool) -> Option<String> {
    ('a'..='z').map(|c| format!("{prefix}-{c}")).find(|id| !taken(id))
}

/// An id `login` may create: 1-64 ASCII letters, digits, `-`, `_` or `.`,
/// not starting with `.`.
pub fn valid_login_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && !id.starts_with('.')
        && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn id_prefix(harness: &str) -> &'static str {
    match harness {
        HARNESS_CODEX => "codex",
        h if h == antigravity::HARNESS_NAME => "agy",
        _ => "claude",
    }
}

/// The harness a new id names by its prefix: `claude-` is claude-code,
/// `codex-` codex, `agy-` antigravity, with at least one character after the
/// prefix. The prefixes are lower case and matched as written.
fn infer_harness(id: &str) -> Option<&'static str> {
    [
        ("claude-", crate::adapter::HARNESS_CLAUDE_CODE),
        ("codex-", HARNESS_CODEX),
        ("agy-", antigravity::HARNESS_NAME),
    ]
    .into_iter()
    .find(|(prefix, _)| id.strip_prefix(prefix).is_some_and(|rest| !rest.is_empty()))
    .map(|(_, harness)| harness)
}

fn default_provider(harness: &str) -> &'static str {
    match harness {
        HARNESS_CODEX => "openai",
        h if h == antigravity::HARNESS_NAME => "google",
        _ => "anthropic",
    }
}

/// A message from the table: English for `--json`, else `THOTH_LANG`'s.
fn msg(json: bool, key: &str, args: &[&str]) -> String {
    let lang = if json { Lang::En } else { Lang::from_env() };
    lang::fill(lang::text(lang, key), args)
}

/// The one way this command refuses: an error on stderr, nothing on stdout, exit 1.
fn refuse(json: bool, key: &str, args: &[&str]) -> Result<i32> {
    output::emit_err(json, &msg(json, key, args));
    Ok(1)
}

pub(crate) fn stdio_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// What `login` is about to log in to.
struct Target {
    id: String,
    harness: String,
    provider: String,
    /// The registered row, for a profile that already exists.
    existing: Option<Profile>,
}

pub fn run(paths: &Paths, args: LoginArgs<'_>) -> Result<i32> {
    let json = args.json;

    // Two gates, before any database or directory is touched.
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        return refuse(json, "login.refuse.in_run", &[]);
    }
    if std::env::var(SKIP_TTY_CHECK_VAR).as_deref() != Ok("1") && !stdio_is_terminal() {
        let mut again = "thoth profile login".to_string();
        if let Some(id) = args.id {
            again.push_str(&format!(" {id}"));
        }
        if let Some(h) = args.harness {
            again.push_str(&format!(" --harness {h}"));
        }
        return refuse(json, "login.refuse.no_tty", &[&again]);
    }

    let conn = db::open_global(paths)?;
    let target = match decide_target(&conn, &args)? {
        Ok(t) => t,
        Err(code) => return Ok(code),
    };
    let new = target.existing.is_none();
    let is_agy = target.harness == antigravity::HARNESS_NAME;

    // The directory the CLI keeps this account in, and the string the probe
    // (and, for claude and codex, the registration) uses.
    let (dir, probe_dir) = match &target.existing {
        Some(row) => {
            let dir = PathBuf::from(&row.config_dir);
            if !is_agy {
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
            (dir, row.config_dir.clone())
        }
        None if is_agy => {
            // agy's own data directory: agy creates it, not thoth.
            let dir = antigravity::fixed_config_dir()?;
            let shown = dir.display().to_string();
            (dir, shown)
        }
        None => {
            let dir = paths.home.join("profiles").join(&target.id);
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("cannot create {}", dir.display()))?;
            let canonical = crate::config::canonical_display(&dir)?;
            (dir, canonical)
        }
    };

    let open = match target.harness.as_str() {
        HARNESS_CODEX => msg(false, "login.open.codex", &[&target.id, &probe_dir]),
        h if h == antigravity::HARNESS_NAME => msg(false, "login.open.agy", &[&target.id]),
        _ => msg(false, "login.open.claude", &[&target.id, &probe_dir]),
    };
    // stdout carries only the result object under --json.
    if json {
        masked_eprintln!("{open}");
    } else {
        println!("{open}");
    }

    let cli_exit = launch(&target.harness, &probe_dir)?;

    let mut profile = target.existing.clone().unwrap_or_else(|| Profile {
        profile_id: target.id.clone(),
        provider: target.provider.clone(),
        harness: target.harness.clone(),
        account_label: None,
        config_dir: probe_dir.clone(),
        default_models: None,
        status: "active".to_string(),
    });
    let status = profile::probe_auth_profile(&profile)
        .map_err(|e| anyhow!("cannot check the login of {}: {e:#}", target.id))?;

    if !status.logged_in {
        let again = if new {
            let mut s = format!("thoth profile login {} --harness {}", target.id, target.harness);
            if let Some(p) = args.provider {
                s.push_str(&format!(" --provider {p}"));
            }
            s
        } else {
            format!("thoth profile login {}", target.id)
        };
        let key = if new { "login.not_logged_in.new" } else { "login.not_logged_in.again" };
        let v = serde_json::json!({
            "ok": false,
            "profile_id": profile.profile_id,
            "harness": profile.harness,
            "provider": profile.provider,
            "config_dir": profile.config_dir,
            "new": new,
            "logged_in": false,
            "registered": false,
            "email": null,
            "subscription_type": null,
            "account_label": null,
            "cli_exit": cli_exit,
        });
        output::emit(json, &v, &msg(false, key, &[&target.id, &again]));
        return Ok(1);
    }

    if new {
        if is_agy {
            // `profile add` requires the directory to exist; an empty one is
            // harmless to agy, and the adapter only looks in it for transcripts.
            if !dir.is_dir() {
                std::fs::create_dir_all(&dir)
                    .with_context(|| format!("cannot create {}", dir.display()))?;
            }
            profile.config_dir = crate::config::canonical_display(&dir)?;
        }
        profile.account_label = args
            .label
            .map(str::to_string)
            .or_else(|| status.subscription_type.clone());
        profile::insert_new_profile(&conn, &profile)?;
    } else if let Some(label) = args.label {
        global::set_profile_account_label(&conn, &profile.profile_id, label)?;
        profile.account_label = Some(label.to_string());
    }
    // TIER-1: the tier the CLI just reported; a failed write only warns.
    plan_label::remember_or_warn(&conn, &profile.profile_id, status.subscription_type.as_deref());
    // PANEL-3: the login name of the email it reported, the same way.
    crate::login_name::remember_or_warn(&conn, &profile.profile_id, status.email.as_deref());
    // MODELS-1: the model list the CLI just reported, the same way.
    crate::model_list::remember_or_warn(&conn, &profile.profile_id, &status);
    // FC-1: the runner marked this profile `logged_out`; the CLI now says it
    // is logged in, so it is active again.
    if target.existing.as_ref().is_some_and(|row| row.status == "logged_out") {
        profile::restore_login(paths, &profile.profile_id, "profile login")?;
    }

    let about = status
        .subscription_type
        .clone()
        .or_else(|| status.email.clone())
        .unwrap_or_else(|| "-".to_string());
    let human = if new {
        msg(false, "login.ok.new", &[&profile.profile_id, &about, &profile.config_dir])
    } else {
        msg(false, "login.ok.again", &[&profile.profile_id, &about, &profile.config_dir])
    };
    let v = serde_json::json!({
        "ok": true,
        "profile_id": profile.profile_id,
        "harness": profile.harness,
        "provider": profile.provider,
        "config_dir": profile.config_dir,
        "new": new,
        "logged_in": true,
        "registered": true,
        "email": status.email,
        "subscription_type": status.subscription_type,
        "account_label": profile.account_label,
        "cli_exit": cli_exit,
    });
    output::emit(json, &v, &human);
    Ok(0)
}

/// The profile to log in to, by the rules in order; `Err(code)` once a
/// refusal has been printed.
fn decide_target(conn: &rusqlite::Connection, args: &LoginArgs<'_>) -> Result<std::result::Result<Target, i32>> {
    let json = args.json;
    let existing_target = |row: Profile| -> Result<std::result::Result<Target, i32>> {
        if let Some(h) = args.harness {
            if h != row.harness {
                return refuse(json, "login.refuse.harness_mismatch", &[&row.profile_id, &row.harness]).map(Err);
            }
        }
        if let Some(p) = args.provider {
            if p != row.provider {
                return refuse(json, "login.refuse.provider_mismatch", &[&row.profile_id, &row.provider])
                    .map(Err);
            }
        }
        Ok(Ok(Target {
            id: row.profile_id.clone(),
            harness: row.harness.clone(),
            provider: row.provider.clone(),
            existing: Some(row),
        }))
    };
    let first_agy = || -> Result<Option<Profile>> {
        Ok(global::list_profiles(conn)?
            .into_iter()
            .filter(|p| p.harness == antigravity::HARNESS_NAME)
            .min_by(|a, b| a.profile_id.cmp(&b.profile_id)))
    };

    let (id, harness) = match args.id {
        Some(id) => {
            if let Some(row) = global::get_profile(conn, id)? {
                return existing_target(row);
            }
            if let Some(row) = profile::find_profile_ignoring_case(&global::list_profiles(conn)?, id) {
                return refuse(json, "login.refuse.id_case", &[id, &row.profile_id]).map(Err);
            }
            profile::check_new_profile_id(id)?;
            if !valid_login_id(id) {
                return refuse(json, "login.refuse.bad_id", &[id]).map(Err);
            }
            // PANEL-3: a new `claude-x`, `codex-x` or `agy-x` names its CLI.
            let Some(harness) = args.harness.or_else(|| infer_harness(id)) else {
                return refuse(json, "login.refuse.need_harness", &[id]).map(Err);
            };
            if harness == antigravity::HARNESS_NAME {
                if let Some(agy) = first_agy()? {
                    let again = format!("thoth profile login {}", agy.profile_id);
                    return refuse(json, "login.refuse.agy_exists", &[&agy.profile_id, &again]).map(Err);
                }
            }
            (id.to_string(), harness)
        }
        None => {
            let Some(harness) = args.harness else {
                return refuse(json, "login.refuse.need_harness_noid", &[]).map(Err);
            };
            if harness == antigravity::HARNESS_NAME {
                if let Some(agy) = first_agy()? {
                    return existing_target(agy);
                }
            }
            let prefix = id_prefix(harness);
            let registered = global::list_profiles(conn)?;
            let taken = |candidate: &str| profile::find_profile_ignoring_case(&registered, candidate).is_some();
            match next_free_id(prefix, &taken) {
                Some(id) => (id, harness),
                None => return refuse(json, "login.refuse.no_free_id", &[prefix]).map(Err),
            }
        }
    };
    let provider = args.provider.unwrap_or_else(|| default_provider(harness)).to_string();
    Ok(Ok(Target { id, harness: harness.to_string(), provider, existing: None }))
}

/// Runs the harness's login command in this terminal (inherited stdio) and
/// waits for it, ignoring Ctrl+C meanwhile. Returns its exit code, which
/// does not decide anything: only the probe afterwards does.
fn launch(harness: &str, config_dir: &str) -> Result<Option<i32>> {
    let adapter = crate::adapter::for_harness(harness);
    let cwd = std::env::current_dir()?;
    let plan = adapter.login_command(config_dir, &cwd);
    let (program, launch_args) = crate::process::resolve_harness_command(&plan.program, &plan.args)?;
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&launch_args).current_dir(&plan.cwd);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("could not run {program}: {e}; install it first (thoth doctor shows how)"))?;
    let status = crate::process::ignore_ctrl_c_while(&mut child);
    Ok(status.ok().and_then(|s| s.code()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_login_next_free_id() {
        assert_eq!(next_free_id("claude", &|_| false).as_deref(), Some("claude-a"));
        let ab = |id: &str| id == "claude-a" || id == "claude-b";
        assert_eq!(next_free_id("claude", &ab).as_deref(), Some("claude-c"));
        assert_eq!(next_free_id("claude", &|_| true), None);
    }

    #[test]
    fn profile_login_valid_login_id() {
        for ok in ["claude-a", "my_acct.2"] {
            assert!(valid_login_id(ok), "{ok}");
        }
        let long = "a".repeat(65);
        for bad in ["", ".x", "a/b", "a\\b", "a b", long.as_str(), "中文"] {
            assert!(!valid_login_id(bad), "{bad}");
        }
        assert!(valid_login_id(&"a".repeat(64)));
    }

    #[test]
    fn profile_login_prefixes_and_providers() {
        assert_eq!(id_prefix("claude-code"), "claude");
        assert_eq!(id_prefix("codex"), "codex");
        assert_eq!(id_prefix("antigravity"), "agy");
        assert_eq!(default_provider("claude-code"), "anthropic");
        assert_eq!(default_provider("codex"), "openai");
        assert_eq!(default_provider("antigravity"), "google");
    }
}
