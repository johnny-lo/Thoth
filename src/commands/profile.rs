use crate::config::Paths;
use crate::db;
use crate::db::global::{self, Profile};
use crate::ids;
use crate::lang::Lang;
use crate::output;
use crate::plan_label;
use anyhow::{bail, Result};
use std::path::Path;

pub struct AddArgs<'a> {
    pub id: &'a str,
    pub provider: &'a str,
    pub harness: &'a str,
    pub config_dir: &'a Path,
    pub label: Option<&'a str>,
    pub json: bool,
}

/// Refuse a harness the runner cannot launch (`adapter::HARNESSES`); a
/// profile for any other harness would silently run claude. `context` names
/// the profile.
pub fn require_supported_harness(context: &str, harness: &str) -> Result<()> {
    let supported: Vec<&str> = crate::adapter::HARNESSES.iter().map(|h| h.name).collect();
    if !supported.contains(&harness) {
        bail!(
            "{context} uses harness '{harness}', but only {} are supported",
            supported.join(", ")
        );
    }
    Ok(())
}

/// Refuse an id no new profile may take (`add` and `login`).
pub fn check_new_profile_id(id: &str) -> Result<()> {
    // Refused in any letter case; `--profile` itself matches only `auto`.
    if id.eq_ignore_ascii_case(crate::commands::auto_pick::AUTO) {
        bail!(
            "'auto' is a reserved profile id: the launch commands refuse `--profile auto`, so \
             no profile can be named auto; choose another id"
        );
    }
    Ok(())
}

/// Write a new profile row and its `note` quota event (`add` and `login`).
pub fn insert_new_profile(conn: &rusqlite::Connection, profile: &Profile) -> Result<()> {
    global::insert_profile(conn, profile)?;
    global::insert_quota_event(
        conn,
        &ids::now_rfc3339(),
        &profile.profile_id,
        "note",
        &serde_json::json!({ "added": true, "config_dir": profile.config_dir }),
    )?;
    Ok(())
}

/// The registered profile whose id equals `id` ignoring ASCII case, if any
/// (an exact match counts).
pub(crate) fn find_profile_ignoring_case<'a>(profiles: &'a [Profile], id: &str) -> Option<&'a Profile> {
    profiles.iter().find(|p| p.profile_id.eq_ignore_ascii_case(id))
}

pub fn add(paths: &Paths, args: AddArgs<'_>) -> Result<i32> {
    check_new_profile_id(args.id)?;
    require_supported_harness("--harness", args.harness)?;
    if !args.config_dir.is_dir() {
        bail!("config dir does not exist: {}", args.config_dir.display());
    }
    let conn = db::open_global(paths)?;
    if global::get_profile(&conn, args.id)?.is_some() {
        bail!("profile '{}' already exists", args.id);
    }
    if let Some(row) = find_profile_ignoring_case(&global::list_profiles(&conn)?, args.id) {
        bail!(
            "profile id '{0}' differs from the registered profile '{1}' only in letter case; \
             two ids that differ only in letter case are too easy to confuse in --profile, in \
             events and in the usage tables, and thoth profile login would map both to one \
             directory on Windows; use '{1}', or pick a clearly different id",
            args.id,
            row.profile_id
        );
    }
    // Same normalisation as `ids::repo_id_for`: canonicalize, then strip the
    // `\\?\` verbatim prefix. This string becomes CLAUDE_CONFIG_DIR for the
    // worker, so it must be the plain path the harness itself reports.
    let canonical = crate::config::canonical_display(args.config_dir)?;
    let profile = Profile {
        profile_id: args.id.to_string(),
        provider: args.provider.to_string(),
        harness: args.harness.to_string(),
        account_label: args.label.map(|s| s.to_string()),
        config_dir: canonical,
        default_models: None,
        status: "active".to_string(),
    };
    insert_new_profile(&conn, &profile)?;
    let v = serde_json::json!({
        "ok": true,
        "profile_id": profile.profile_id,
        "config_dir": profile.config_dir,
        "status": profile.status,
    });
    output::emit(
        args.json,
        &v,
        &format!(
            "added profile {} ({} / {}) -> {}",
            profile.profile_id, profile.provider, profile.harness, profile.config_dir
        ),
    );
    Ok(0)
}

pub fn set_config_dir(paths: &Paths, id: &str, dir: &Path, json: bool) -> Result<i32> {
    if !dir.is_dir() {
        bail!("config dir does not exist: {}", dir.display());
    }
    let conn = db::open_global(paths)?;
    if global::get_profile(&conn, id)?.is_none() {
        bail!("no such profile: {id}");
    }
    let canonical = crate::config::canonical_display(dir)?;
    global::set_profile_config_dir(&conn, id, &canonical)?;
    let v = serde_json::json!({
        "ok": true,
        "profile_id": id,
        "config_dir": canonical,
    });
    output::emit(
        json,
        &v,
        &format!("profile {id} now uses config dir {canonical}"),
    );
    Ok(0)
}

/// `profile set-shared ID on|off` (DT-1): the operator's mark that a human
/// also works on this account, so quota points measured on it are not the
/// run's own. Human-only, like `config set`.
pub fn set_shared(paths: &Paths, id: &str, value: &str, json: bool) -> Result<i32> {
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!("profile set-shared is only for the human operator; it is refused while THOTH_RUN_ID is set")
    }
    let shared = match value {
        "on" => true,
        "off" => false,
        _ => bail!("invalid value '{value}'; use on or off"),
    };
    let conn = db::open_global(paths)?;
    if global::get_profile(&conn, id)?.is_none() {
        bail!("no such profile: {id}");
    }
    let previous = global::profile_shared(&conn, id)?;
    global::set_profile_shared(&conn, id, shared)?;
    let v = serde_json::json!({
        "ok": true,
        "profile_id": id,
        "shared": shared,
        "previous": previous,
    });
    output::emit(
        json,
        &v,
        &format!("profile {id}: shared={}", if shared { "on" } else { "off" }),
    );
    Ok(0)
}

pub fn list(paths: &Paths, json: bool) -> Result<i32> {
    let conn = db::open_global(paths)?;
    let profiles = global::list_profiles(&conn)?;
    let shared: Vec<bool> = profiles
        .iter()
        .map(|p| global::profile_shared(&conn, &p.profile_id))
        .collect::<Result<_>>()?;
    let plans = global::profile_plans(&conn)?;
    let stored = |p: &Profile| plans.get(&p.profile_id);
    let logins = global::profile_login_names(&conn)?;
    let login = |p: &Profile| logins.get(&p.profile_id);
    let bracket = |p: &Profile| {
        plan_label::label(stored(p).map(|s| s.plan.as_str()), p.account_label.as_deref())
    };
    let arr: Vec<serde_json::Value> = profiles
        .iter()
        .zip(&shared)
        .map(|(p, shared)| {
            serde_json::json!({
                "profile_id": p.profile_id,
                "provider": p.provider,
                "harness": p.harness,
                "account_label": p.account_label,
                "config_dir": p.config_dir,
                "status": p.status,
                "shared": shared,
                "plan": stored(p).map(|s| &s.plan),
                "plan_label": bracket(p),
                "plan_seen_at": stored(p).and_then(|s| s.seen_at.as_ref()),
                // PANEL-3: the login name for the panel; never in MCP results.
                "login_name": login(p).map(|s| &s.name),
                "login_name_seen_at": login(p).and_then(|s| s.seen_at.as_ref()),
            })
        })
        .collect();
    // TIER-1: `claude-a (Max)`; the column is as wide as the widest name.
    let names: Vec<String> = profiles
        .iter()
        .map(|p| plan_label::named(&p.profile_id, bracket(p).as_deref(), Lang::En))
        .collect();
    let width = names.iter().map(|n| n.chars().count()).max().unwrap_or(0).max(12);
    let human = if profiles.is_empty() {
        "no profiles registered".to_string()
    } else {
        profiles
            .iter()
            .zip(&shared)
            .zip(&names)
            .map(|((p, shared), name)| {
                format!(
                    "{:<width$} {:<10} {:<12} {:<8} {}{}",
                    name,
                    p.provider,
                    p.harness,
                    p.status,
                    p.config_dir,
                    if *shared { " shared" } else { "" }
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    output::emit(json, &serde_json::json!({ "profiles": arr }), &human);
    Ok(0)
}

/// Runs the harness's auth check for a profile and returns what it said:
/// `claude auth status --json`, or `codex login status` for a codex profile.
///
/// Read-only: it prints nothing and writes nothing. Both properties matter
/// because `dispatch` reuses it as a pre-flight gate — printing would corrupt
/// `dispatch --json`, and writing would let any dispatch silently clear a
/// `rate_limited` flag. Being rate limited does NOT make `auth status` report
/// `loggedIn:false`, so that clearing has to stay an explicit human act. The
/// `rate_limited` -> `active` bookkeeping therefore lives in `check`, per brief
/// section 10 and Open question 8.
pub fn probe_auth(paths: &Paths, id: &str) -> Result<(Profile, crate::adapter::AuthStatus)> {
    let conn = db::open_global(paths)?;
    let profile = match global::get_profile(&conn, id)? {
        Some(p) => p,
        None => bail!("no such profile: {id}"),
    };
    let status = probe_auth_profile(&profile)?;
    Ok((profile, status))
}

/// `probe_auth` for a profile already in hand: no database, no writes, no
/// output. `thoth doctor` calls it for every registered profile.
pub fn probe_auth_profile(profile: &Profile) -> Result<crate::adapter::AuthStatus> {
    let adapter = crate::adapter::for_harness(&profile.harness);
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let plan = adapter.auth_command(&profile.config_dir, &cwd);

    // Windows: resolve an npm `.cmd` shim to node.exe + its JS entry (never
    // run via cmd.exe); an unresolvable shim fails here with the how-to-fix.
    let (program, launch_args) = crate::process::resolve_harness_command(&plan.program, &plan.args)?;
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&launch_args).current_dir(&plan.cwd);
    // Rider (whole-branch review, T7 ledger item): this is a synchronous,
    // portable `output()` call, not process *control* (no kill/liveness),
    // so it stays outside `process::`'s Windows-only surface per the Task 7
    // ruling - but it still spawns a real child, and without this a
    // detached runner's auth probe could flash a console window on
    // Windows. `quiet` is a no-op on non-Windows, so this is safe
    // unconditionally.
    crate::process::quiet(&mut cmd);
    for (k, v) in &plan.env {
        cmd.env(k, v);
    }
    let out = cmd.output().map_err(|e| {
        let hint = if e.kind() == std::io::ErrorKind::NotFound {
            "; set THOTH_CODEX_BIN or THOTH_CLAUDE_BIN to the real executable"
        } else {
            ""
        };
        anyhow::anyhow!("could not run {}: {e}{hint}", plan.program)
    })?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let status = adapter.parse_auth_output(out.status.success(), &stdout, &stderr).map_err(|e| {
        anyhow::anyhow!(
            "{e} (exit {:?}, stderr: {})",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })?;

    Ok(status)
}

/// FC-1: `profiles.status: logged_out -> active` plus a `login_restored`
/// quota event, after the auth check said the profile is logged in again
/// (`profile check`, `profile login`). `source` names the command.
pub(crate) fn restore_login(paths: &Paths, id: &str, source: &str) -> Result<()> {
    let conn = db::open_global(paths)?;
    let tx = conn.unchecked_transaction()?;
    global::set_profile_status(&tx, id, "active")?;
    global::insert_quota_event(
        &tx,
        &ids::now_rfc3339(),
        id,
        "login_restored",
        &serde_json::json!({ "source": source, "logged_in": true }),
    )?;
    tx.commit()?;
    Ok(())
}

pub fn check(paths: &Paths, id: &str, json: bool) -> Result<i32> {
    let (profile, status) = probe_auth(paths, id)?;

    // TIER-1: store the tier the CLI reported; a failed write only warns.
    // MODELS-1: the model list it reported, the same way; `fresh_models` is
    // what was stored this time (None when the check carried no list or the
    // write failed).
    let mut fresh_models = None;
    if status.logged_in {
        match db::open_global(paths) {
            Ok(conn) => {
                plan_label::remember_or_warn(&conn, id, status.subscription_type.as_deref());
                crate::login_name::remember_or_warn(&conn, id, status.email.as_deref());
                fresh_models = crate::model_list::remember_or_warn(&conn, id, &status).filter(|f| f.stored);
            }
            Err(e) => {
                masked_eprintln!("warning: could not store the plan of {id}: {e:#}");
                if status.email.as_deref().and_then(crate::login_name::from_email).is_some() {
                    crate::login_name::warn(id, &e);
                }
            }
        }
    }

    // brief section 10: running `profile check` IS the human's signal that the
    // quota window reset, so this is the only place that clears the flag. It is
    // deliberately not inside `probe_auth`, which `dispatch` calls.
    let mut profile_status = profile.status.clone();
    if status.logged_in && profile.status == "rate_limited" {
        let conn = db::open_global(paths)?;
        global::set_profile_status(&conn, id, "active")?;
        global::insert_quota_event(
            &conn,
            &ids::now_rfc3339(),
            id,
            "window_reset",
            &serde_json::json!({ "source": "profile check", "logged_in": true }),
        )?;
        profile_status = "active".to_string();
    }
    // FC-1: a profile the runner marked `logged_out` is cleared the same way.
    if status.logged_in && profile.status == "logged_out" {
        restore_login(paths, id, "profile check")?;
        profile_status = "active".to_string();
    }

    let v = serde_json::json!({
        "ok": status.logged_in,
        "profile_id": id,
        "logged_in": status.logged_in,
        "email": status.email,
        "subscription_type": status.subscription_type,
        "config_directory": status.config_directory,
        "status": profile_status,
        // MODELS-1: `models_count`, not `models`, so it is never confused
        // with the array of the same name in `thoth models --json`.
        "models_count": fresh_models.as_ref().map(|f| f.models.len()),
        "models_seen_at": fresh_models.as_ref().map(|f| f.seen_at.clone()),
    });
    let human = format!(
        "{id}: logged_in={} email={} subscription={} config_dir={} status={profile_status} models={}",
        status.logged_in,
        status.email.clone().unwrap_or_else(|| "-".to_string()),
        status.subscription_type.clone().unwrap_or_else(|| "-".to_string()),
        status.config_directory.clone().unwrap_or_else(|| "-".to_string()),
        fresh_models.as_ref().map_or("-".to_string(), |f| f.models.len().to_string()),
    );
    output::emit(json, &v, &human);
    Ok(if status.logged_in { 0 } else { 1 })
}

/// `profile forget-model ID MODEL` (MODELS-1): one `model_forgotten` quota
/// event, so the launch commands accept the model again until a run proves
/// it unavailable once more. Nothing else on the profile changes.
pub fn forget_model(paths: &Paths, id: &str, model: &str, json: bool) -> Result<i32> {
    let model = model.trim();
    if model.is_empty() {
        bail!("MODEL must not be empty");
    }
    let conn = db::open_global(paths)?;
    if global::get_profile(&conn, id)?.is_none() {
        bail!("no such profile: {id}");
    }
    crate::model_list::forget(&conn, id, model)?;
    let v = serde_json::json!({
        "ok": true,
        "profile_id": id,
        "model": model,
    });
    output::emit(
        json,
        &v,
        &format!("profile {id}: model {model} is no longer recorded as unavailable"),
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str) -> Profile {
        Profile {
            profile_id: id.to_string(),
            provider: "anthropic".to_string(),
            harness: "claude-code".to_string(),
            account_label: None,
            config_dir: "/x".to_string(),
            default_models: None,
            status: "active".to_string(),
        }
    }

    #[test]
    fn find_profile_ignoring_case_matches_only_the_same_letters() {
        assert!(find_profile_ignoring_case(&[], "claude-a").is_none());
        let rows = vec![row("claude-a")];
        assert!(find_profile_ignoring_case(&rows, "Claude-A").is_some());
        assert!(find_profile_ignoring_case(&rows, "claude-a").is_some());
        assert!(find_profile_ignoring_case(&rows, "claude-b").is_none());
    }
}
