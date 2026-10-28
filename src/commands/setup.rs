//! `thoth setup` (setup scope SETUP-3): doctor's checks in order, asking one
//! question before each change.
//!
//! Every step calls the same item function `thoth doctor` uses, prints it,
//! asks when something can be done here, does it, then calls the item
//! function again and prints its detail as a result line. Nothing already
//! fine is asked about again, so running it twice is safe. It never installs
//! claude, codex or agy and never reads credentials: logins go through
//! `profile login`, login status through `profile::probe_auth_profile`.
//!
//! Only a person at a terminal may run it: it refuses inside a run
//! (`THOTH_RUN_ID`) and without a terminal on stdin and stdout, before
//! anything is opened, created or started. A Y typed here is the human's
//! approval of the checks it lists.

use crate::adapter::{antigravity, HARNESS_CLAUDE_CODE, HARNESS_CODEX};
use crate::checks::{self, Check};
use crate::commands::checks::{add_check, approve_checks, AddArgs};
use crate::commands::doctor::{
    checks_item, cli_item, config_item, first_line, open_read_only, path_fix_lines,
    plugin_item, plugin_marketplace_source, plugin_version_matches, profile_item, profiles_item,
    read_profiles, render_item, repo_item, repo_root_of, run_cli, state_dir_item, Cli, Ctx, Item,
    Profiles, RepoState, Status,
};
use crate::commands::profile_login::{self, LoginArgs, SKIP_TTY_CHECK_VAR};
use crate::config::{self, Paths};
use crate::db::global::Profile;
use crate::lang::{self, Lang};
use crate::output;
use crate::plan_label;
use crate::user_shell::UserShell;
use anyhow::{anyhow, Result};
use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

const PLUGIN_ID: &str = "thoth@thoth";

/// The three CLIs, with the harness each one's profiles use and the name the
/// "add an account" question shows.
const HARNESSES: [(Cli, &str, &str); 3] = [
    (Cli::Claude, HARNESS_CLAUDE_CODE, "Claude"),
    (Cli::Codex, HARNESS_CODEX, "Codex"),
    (Cli::Agy, antigravity::HARNESS_NAME, "agy"),
];

/// One answer: blanks around it and letter case ignored; empty is the
/// default, `y`/`yes` yes, anything else no. `None` (end of input) is no.
pub(crate) fn parse_answer(line: Option<&str>, default_yes: bool) -> bool {
    let Some(line) = line else {
        return false;
    };
    match line.trim().to_ascii_lowercase().as_str() {
        "" => default_yes,
        "y" | "yes" => true,
        _ => false,
    }
}

/// The checks a project of a known kind usually has, Rust, Node, Python in
/// that order.
pub(crate) fn detect_project_checks(root: &Path) -> Vec<(String, String)> {
    let file = |name: &str| root.join(name).is_file();
    let mut out = Vec::new();
    if file("Cargo.toml") {
        out.push(("test".to_string(), "cargo test".to_string()));
        out.push(("clippy".to_string(), "cargo clippy --all-targets -- -D warnings".to_string()));
    }
    if file("package.json") {
        out.push(("npm-test".to_string(), "npm test".to_string()));
    }
    if ["pyproject.toml", "pytest.ini", "setup.py", "setup.cfg"].iter().any(|n| file(n)) {
        out.push(("pytest".to_string(), "pytest".to_string()));
    }
    out
}

/// The screen: the language and shell, the questions asked so far, and the
/// last status of every item.
struct Session {
    c: Ctx,
    eof: bool,
    last: BTreeMap<String, Status>,
    plugin_changed: bool,
}

impl Session {
    fn t(&self, key: &str, args: &[&str]) -> String {
        self.c.t(key, args).local
    }

    fn show(&mut self, item: &Item) {
        for line in render_item(item) {
            println!("{line}");
        }
        self.last.insert(item.id.clone(), item.status);
    }

    /// The item again after a change, as one indented result line.
    fn result(&mut self, item: &Item) {
        println!("    {} {}", item.status.mark(), item.detail.local);
        self.last.insert(item.id.clone(), item.status);
    }

    fn result_line(&self, status: Status, text: &str) {
        println!("    {} {text}", status.mark());
    }

    fn fix_line(&self, text: &str) {
        println!("  → {text}");
    }

    /// Asks one question; after the end of input every later question is
    /// shown and counted as no without reading.
    fn ask(&mut self, question: &str, default_yes: bool) -> bool {
        let tag = self.t(if default_yes { "setup.yn" } else { "setup.ny" }, &[]);
        print!("  → {question} {tag} ");
        let _ = std::io::stdout().flush();
        if self.eof {
            println!();
            return false;
        }
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => {
                self.eof = true;
                println!();
                parse_answer(None, default_yes)
            }
            Ok(_) => parse_answer(Some(&line), default_yes),
        }
    }
}

/// The one way setup refuses: an error on stderr, nothing on stdout, exit 1.
fn refuse(lang: Lang, key: &str, args: &[&str]) -> Result<i32> {
    output::emit_err(false, &lang::fill(lang::text(lang, key), args));
    Ok(1)
}

pub fn run(cwd: &Path, shell_flag: Option<&str>) -> Result<i32> {
    let lang = Lang::from_env();

    // Two gates, before any database, directory or child process.
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        return refuse(lang, "setup.refuse.in_run", &[]);
    }
    if !profile_login::stdio_is_terminal() {
        if std::env::var(SKIP_TTY_CHECK_VAR).as_deref() != Ok("1") {
            let mut again = "thoth setup".to_string();
            if let Some(s) = shell_flag {
                again.push_str(&format!(" --shell {s}"));
            }
            return refuse(lang, "setup.refuse.no_tty", &[&again]);
        }
        masked_eprintln!("{}", lang::text(lang, "setup.note.tty_skipped"));
    }

    let shell = match shell_flag {
        Some(s) => UserShell::parse(s).ok_or_else(|| anyhow!("unknown --shell value: {s}"))?,
        None => UserShell::detect().0,
    };
    let mut s = Session { c: Ctx { lang, shell }, eof: false, last: BTreeMap::new(), plugin_changed: false };

    // The state directory: the old-directory cases stop here, since every
    // other command would refuse.
    let probe = config::probe_state_dir()?;
    let item = state_dir_item(&s.c, &probe);
    s.show(&item);
    if probe.old_dir_only.is_some() || probe.agentctl_var_only.is_some() {
        println!("{}", s.t("setup.stop.state_dir", &[]));
        return Ok(1);
    }
    let paths = Paths::from_root(probe.home);

    let cli_ok = step_clis(&mut s, &paths);
    step_accounts(&mut s, &paths, &cli_ok);
    let (state, repo_root) = step_repo(&mut s, &paths, cwd);
    step_checks(&mut s, &paths, cwd, &state);
    step_live_quota(&mut s, &paths);
    step_plugin(&mut s, repo_root.as_deref());

    println!("──────────");
    let fails = s.last.values().filter(|st| **st == Status::Fail).count();
    if fails == 0 {
        println!("{}", s.t("setup.summary.done", &[]));
    } else {
        println!("{}", s.t("setup.summary.left", &[&fails.to_string()]));
    }
    if s.plugin_changed {
        println!("{}", s.t("setup.summary.restart", &[]));
    }
    Ok(if fails == 0 { 0 } else { 1 })
}

/// The three CLIs, printed only (setup installs none of them); returns the
/// harnesses whose CLI is ok.
fn step_clis(s: &mut Session, paths: &Paths) -> Vec<&'static str> {
    let profiles = read_profiles(paths);
    let mut ok = Vec::new();
    for (cli, harness, _) in HARNESSES {
        let used = match &profiles {
            Profiles::Listed(list) => list.iter().any(|p| p.harness == harness),
            _ => false,
        };
        let item = cli_item(&s.c, cli, used, "setup.fix.rerun");
        if item.status == Status::Ok {
            ok.push(harness);
        }
        s.show(&item);
    }
    ok
}

/// `profiles_item` without its "log in to one" fix: setup asks instead.
fn profiles_without_fix(s: &Session, profiles: &Profiles) -> Item {
    let mut item = profiles_item(&s.c, profiles);
    if item.status == Status::Fail {
        item.fix = None;
    }
    item
}

/// Runs `profile login` until it logs in, the user stops retrying, or it
/// fails outright; true once logged in.
fn login_loop(s: &mut Session, paths: &Paths, id: Option<&str>, harness: Option<&str>) -> bool {
    loop {
        let args = LoginArgs { id, harness, provider: None, label: None, json: false };
        match profile_login::run(paths, args) {
            Ok(0) => return true,
            Ok(_) => {
                let q = s.t("setup.ask.retry", &[]);
                if !s.ask(&q, true) {
                    return false;
                }
            }
            Err(e) => {
                s.result_line(Status::Fail, &format!("{e:#}"));
                return false;
            }
        }
    }
}

fn step_accounts(s: &mut Session, paths: &Paths, cli_ok: &[&str]) {
    let profiles = read_profiles(paths);
    let item = profiles_without_fix(s, &profiles);
    s.show(&item);
    let list = match &profiles {
        Profiles::Failed(_) => return,
        Profiles::NoDb => Vec::new(),
        Profiles::Listed(list) => list.clone(),
    };

    // Registered profiles: offer a login for an active one that is logged out.
    let labels = plan_label::read_only_labels(paths);
    let label_of = |labels: &std::collections::HashMap<String, String>, id: &str| labels.get(id).cloned();
    for p in &list {
        let item = profile_item(&s.c, p, label_of(&labels, &p.profile_id).as_deref());
        s.show(&item);
        if item.status != Status::Fail || !cli_ok.contains(&p.harness.as_str()) || p.status != "active" {
            continue;
        }
        let q = s.t("setup.ask.login", &[&p.profile_id]);
        if !s.ask(&q, true) {
            continue;
        }
        login_loop(s, paths, Some(&p.profile_id), None);
        // Re-read after the login, which may have stored the tier.
        let fresh = plan_label::read_only_labels(paths);
        let again = profile_item(&s.c, p, label_of(&fresh, &p.profile_id).as_deref());
        s.result(&again);
    }

    // A CLI that works but has no account at all.
    for (_, harness, name) in HARNESSES {
        if !cli_ok.contains(&harness) || list.iter().any(|p| p.harness == harness) {
            continue;
        }
        let q = s.t("setup.ask.add_account", &[name]);
        if !s.ask(&q, true) {
            continue;
        }
        if login_loop(s, paths, None, Some(harness)) {
            if let Profiles::Listed(now) = read_profiles(paths) {
                let fresh = plan_label::read_only_labels(paths);
                let new: Vec<&Profile> = now
                    .iter()
                    .filter(|p| p.harness == harness && !list.iter().any(|o| o.profile_id == p.profile_id))
                    .collect();
                for p in new {
                    let item = profile_item(&s.c, p, label_of(&fresh, &p.profile_id).as_deref());
                    s.result(&item);
                }
            }
        }
    }

    let profiles = read_profiles(paths);
    let item = profiles_without_fix(s, &profiles);
    s.result(&item);
}

/// This repository; offers `thoth init` when it is not registered. Returns
/// what the last `repo` item found and the repository root.
fn step_repo(s: &mut Session, paths: &Paths, cwd: &Path) -> (RepoState, Option<PathBuf>) {
    let (item, state) = repo_item(&s.c, paths, cwd);
    s.show(&item);
    let root = repo_root_of(&item);
    if !matches!(state, RepoState::NotInit) {
        return (state, root);
    }
    let q = s.t("setup.ask.init", &[]);
    if !s.ask(&q, true) {
        return (state, root);
    }
    if let Err(e) = crate::commands::init::init_repo(paths, cwd) {
        s.result_line(Status::Fail, &format!("{e:#}"));
    }
    let (item, state) = repo_item(&s.c, paths, cwd);
    s.result(&item);
    let root = repo_root_of(&item);
    (state, root)
}

/// A check as `fingerprint` sees it, for a command not in the catalogue.
fn probe_check(id: &str, cmd: &str) -> Check {
    Check {
        check_id: id.to_string(),
        shell: "argv".to_string(),
        cmd: cmd.to_string(),
        cwd: ".".to_string(),
        inputs: Vec::new(),
        created_at: String::new(),
        approved_at: None,
        approved_fingerprint: None,
        approved_detail: None,
        approved_commit: None,
        exe_pin: "hash".to_string(),
    }
}

/// True when `checks_actions` is about to propose at least one check: a
/// detected one that is not in the catalogue yet and whose program is on PATH.
fn will_propose_checks(root: &Path, state_db: &Path) -> bool {
    let Ok(catalogue) = open_read_only(state_db).and_then(|conn| checks::list_checks(&conn)) else {
        return false;
    };
    detect_project_checks(root).into_iter().any(|(id, cmd)| {
        !catalogue.iter().any(|c| c.check_id == id || c.cmd == cmd)
            && checks::fingerprint(&probe_check(&id, &cmd), root).is_ok_and(|fp| fp.program.is_some())
    })
}

fn step_checks(s: &mut Session, paths: &Paths, cwd: &Path, state: &RepoState) {
    let mut item = checks_item(&s.c, state, "setup.fix.checks_path");
    let RepoState::Ready { root, state_db } = state else {
        s.show(&item);
        return;
    };
    // With an empty catalogue the only fix is doctor's "add a check" hint;
    // the proposal below says it better, so do not print both.
    if item.data["total"] == serde_json::json!(0) && will_propose_checks(root, state_db) {
        item.fix = None;
    }
    s.show(&item);
    if item.data["missing_program"].as_array().is_some_and(|a| !a.is_empty()) {
        return;
    }
    let acted = match checks_actions(s, paths, cwd, root, state_db) {
        Ok(acted) => acted,
        Err(e) => {
            s.result_line(Status::Fail, &format!("{e:#}"));
            true
        }
    };
    if acted {
        let again = checks_item(&s.c, state, "setup.fix.checks_path");
        s.result(&again);
    }
}

/// Proposing detected checks, then approving pending or stale ones. True
/// when something was done (or tried), so the result line is worth printing.
fn checks_actions(s: &mut Session, paths: &Paths, cwd: &Path, root: &Path, state_db: &Path) -> Result<bool> {
    let mut acted = false;
    let catalogue = open_read_only(state_db).and_then(|conn| checks::list_checks(&conn))?;

    // Detected checks not in the catalogue whose program is on PATH.
    let mut proposals = Vec::new();
    for (id, cmd) in detect_project_checks(root) {
        if catalogue.iter().any(|c| c.check_id == id || c.cmd == cmd) {
            continue;
        }
        let fp = checks::fingerprint(&probe_check(&id, &cmd), root)?;
        if fp.program.is_none() {
            let program = checks::program_word("argv", &cmd);
            println!("    {}", s.t("setup.checks.skip_missing", &[&id, &program]));
            continue;
        }
        proposals.push((id, cmd));
    }
    if !proposals.is_empty() {
        for (id, cmd) in &proposals {
            println!("    {id}: {cmd}");
        }
        let q = s.t("setup.ask.checks_add", &[&proposals.len().to_string()]);
        if s.ask(&q, true) {
            acted = true;
            for (id, cmd) in &proposals {
                let args = AddArgs { id, cmd, shell: "argv", cwd: ".", inputs: &[], exe_pin: "hash", json: false };
                add_check(paths, cwd, &args)?;
            }
        }
    }

    // Pending or stale checks: list them, then ask for the approval.
    let conn = open_read_only(state_db)?;
    let catalogue = checks::list_checks(&conn)?;
    // Where `Paths::blobs_dir` keeps the approved copies of package files.
    let blobs_dir = state_db.parent().map(|d| d.join("blobs")).unwrap_or_default();
    let mut waiting = Vec::new();
    for c in catalogue {
        let fp = checks::fingerprint(&c, root)?;
        let all = checks::approvals(&conn, &c.check_id)?;
        // The same judgement as `checks list`, `verify` and `doctor`.
        let (status, changed) = match all.first() {
            Some(newest) if c.approved_at.is_some() => {
                match checks::match_approval(&c, &all, &fp, root, &blobs_dir, true) {
                    checks::ApprovalMatch::NoMatch { .. } => ("stale", checks::changed_lines(&c, newest, &fp)),
                    _ => continue,
                }
            }
            _ => ("pending", Vec::new()),
        };
        waiting.push((c, fp, status, changed));
    }
    drop(conn);
    if waiting.is_empty() {
        return Ok(acted);
    }
    let missing: Vec<&(Check, checks::Fingerprint, &str, Vec<String>)> =
        waiting.iter().filter(|(_, fp, _, _)| fp.program.is_none()).collect();
    if !missing.is_empty() {
        let mut programs = Vec::new();
        for (c, _, _, _) in &missing {
            let program = checks::program_word(&c.shell, &c.cmd);
            println!("    {}", s.t("setup.checks.skip_missing", &[&c.check_id, &program]));
            programs.push(program);
        }
        for line in path_fix_lines(&s.c, "setup.fix.checks_path", &programs) {
            s.fix_line(&line.local);
        }
        return Ok(acted);
    }
    for (c, _, status, changed) in &waiting {
        let pin_note = if c.exe_pin == "path" { "  (exe pinned by path only)" } else { "" };
        println!("    {}  {status}  [{}] {}{pin_note}", c.check_id, c.shell, c.cmd);
        if *status == "stale" && !changed.is_empty() {
            println!("      changed: {}", changed.join(", "));
        }
    }
    let q = s.t("setup.ask.checks_approve", &[&waiting.len().to_string()]);
    if !s.ask(&q, true) {
        return Ok(acted);
    }
    let listed: Vec<(String, String)> =
        waiting.iter().map(|(c, fp, _, _)| (c.check_id.clone(), fp.hex.clone())).collect();
    // THOTH_LOGIN_SKIP_TTY_CHECK lets setup run without a terminal; then the
    // approval is not a person's at a terminal, and says so.
    let (approved_by, reason) = if profile_login::stdio_is_terminal() {
        (crate::commands::checks::HUMAN_TERMINAL, None)
    } else {
        (
            crate::commands::checks::COMMANDER_CLI,
            Some("approved through thoth setup without a terminal"),
        )
    };
    let outcome = approve_checks(paths, cwd, None, Some(&listed), approved_by, reason)?;
    for id in &outcome.changed_since_listed {
        s.result_line(Status::Fail, &s.t("setup.checks.changed_since_listed", &[id]));
    }
    for (id, reason) in &outcome.refused {
        s.result_line(Status::Fail, &s.t("setup.checks.path_pin_refused", &[id, reason]));
    }
    Ok(true)
}

fn step_live_quota(s: &mut Session, paths: &Paths) {
    let item = config_item(&s.c, &paths.home.join("config.json"));
    s.show(&item);
    if item.status == Status::Warn {
        return;
    }
    let live_quota = crate::commands::settings::read_flags(&paths.home);
    if live_quota {
        s.result_line(Status::Ok, &s.t("setup.live_quota.on", &[]));
        return;
    }
    println!("    {}", s.t("setup.live_quota.off", &[]));
    let q = s.t("setup.ask.live_quota", &[]);
    if !s.ask(&q, false) {
        return;
    }
    match crate::commands::settings::set_flag(paths, "live_quota", true) {
        Ok(_) => s.result_line(Status::Ok, &s.t("setup.live_quota.on", &[])),
        Err(e) => {
            s.result_line(Status::Fail, &format!("{e:#}"));
            s.last.insert("config".to_string(), Status::Fail);
        }
    }
}

/// Runs one `claude plugin …` command; `Err` carries the first line of its
/// error output.
fn claude_plugin(args: &[&str]) -> std::result::Result<(), String> {
    match run_cli(&config::claude_bin(), args) {
        Err(e) => Err(e),
        Ok(ran) if ran.out.status.success() => Ok(()),
        Ok(ran) => {
            let mut why = first_line(&ran.out.stderr);
            if why.is_empty() {
                let code = ran.out.status.code().map_or("none".to_string(), |n| n.to_string());
                why = format!("exit {code}");
            }
            Err(why)
        }
    }
}

/// A failed plugin command: a ✗ result line, then the commands to run by hand.
fn plugin_failed(s: &mut Session, failed: &str, why: &str, by_hand: &[String]) {
    s.result_line(Status::Fail, &s.t("setup.plugin.failed", &[failed, why]));
    for cmd in by_hand {
        s.fix_line(cmd);
    }
}

fn step_plugin(s: &mut Session, repo_root: Option<&Path>) {
    let item = plugin_item(&s.c, repo_root);
    s.show(&item);
    if item.status == Status::Unknown {
        return;
    }
    let expected = env!("CARGO_PKG_VERSION");

    if item.data["found"] == serde_json::json!(false) {
        let q = s.t("setup.ask.plugin_install", &[]);
        if !s.ask(&q, true) {
            return;
        }
        let source = plugin_marketplace_source(repo_root);
        let add = super::doctor::marketplace_add_command(&source);
        let install = format!("claude plugin install {PLUGIN_ID}");
        // The marketplace may already be there; only the install decides.
        let _ = claude_plugin(&["plugin", "marketplace", "add", &source]);
        match claude_plugin(&["plugin", "install", PLUGIN_ID]) {
            Ok(()) => s.plugin_changed = true,
            Err(why) => plugin_failed(s, &install, &why, &[add, install.clone()]),
        }
        let again = plugin_item(&s.c, repo_root);
        s.result(&again);
        return;
    }

    let id = item.data["id"].as_str().unwrap_or(PLUGIN_ID).to_string();
    let mut current = item;
    if current.data["enabled"] == serde_json::json!(false) {
        let q = s.t("setup.ask.plugin_enable", &[]);
        if s.ask(&q, true) {
            let enable = format!("claude plugin enable {id}");
            if let Err(why) = claude_plugin(&["plugin", "enable", &id]) {
                plugin_failed(s, &enable, &why, std::slice::from_ref(&enable));
            }
            current = plugin_item(&s.c, repo_root);
            s.result(&current);
        } else {
            return;
        }
    }
    let installed = |it: &Item| it.data["installed_version"].as_str().unwrap_or("").to_string();
    if current.data["found"] != serde_json::json!(true) || plugin_version_matches(&installed(&current), expected) {
        return;
    }
    let q = s.t("setup.ask.plugin_update", &[]);
    if !s.ask(&q, true) {
        return;
    }
    let update = format!("claude plugin update {id}");
    match claude_plugin(&["plugin", "update", &id]) {
        Ok(()) => s.plugin_changed = true,
        Err(why) => plugin_failed(s, &update, &why, std::slice::from_ref(&update)),
    }
    let again = plugin_item(&s.c, repo_root);
    let now = installed(&again);
    if again.data["found"] == serde_json::json!(true) && !plugin_version_matches(&now, expected) {
        s.result_line(Status::Fail, &s.t("setup.plugin.version_left", &[&id, &now, expected]));
        s.last.insert(again.id.clone(), Status::Fail);
    } else {
        s.result(&again);
    }
}

/// Every message key setup uses, for the table test.
#[cfg(test)]
const KEYS: [&str; 28] = [
    "setup.refuse.in_run",
    "setup.refuse.no_tty",
    "setup.note.tty_skipped",
    "setup.stop.state_dir",
    "setup.fix.rerun",
    "setup.ask.login",
    "setup.ask.retry",
    "setup.ask.add_account",
    "setup.ask.init",
    "setup.checks.skip_missing",
    "setup.ask.checks_add",
    "setup.ask.checks_approve",
    "setup.live_quota.on",
    "setup.live_quota.off",
    "setup.ask.live_quota",
    "setup.ask.plugin_install",
    "setup.ask.plugin_enable",
    "setup.ask.plugin_update",
    "setup.plugin.failed",
    "setup.summary.done",
    "setup.summary.left",
    "setup.summary.restart",
    "setup.fix.checks_path",
    "setup.checks.changed_since_listed",
    "setup.checks.path_pin_refused",
    "setup.plugin.version_left",
    "setup.yn",
    "setup.ny",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setup_parse_answer() {
        for default_yes in [true, false] {
            assert_eq!(parse_answer(Some(""), default_yes), default_yes);
            assert_eq!(parse_answer(Some("\n"), default_yes), default_yes);
            assert!(parse_answer(Some(" Y "), default_yes));
            assert!(parse_answer(Some("yes"), default_yes));
            assert!(parse_answer(Some("YES\r\n"), default_yes));
            assert!(!parse_answer(Some("n"), default_yes));
            assert!(!parse_answer(Some("No"), default_yes));
            assert!(!parse_answer(Some("maybe"), default_yes));
            assert!(!parse_answer(None, default_yes));
        }
    }

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("thoth-setup-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ids(v: Vec<(String, String)>) -> Vec<String> {
        v.into_iter().map(|(id, cmd)| format!("{id}={cmd}")).collect()
    }

    #[test]
    fn setup_detect_project_checks() {
        let d = tmp("detect");
        assert!(detect_project_checks(&d).is_empty());

        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        assert_eq!(
            ids(detect_project_checks(&d)),
            ["test=cargo test", "clippy=cargo clippy --all-targets -- -D warnings"]
        );
        std::fs::write(d.join("package.json"), "{}").unwrap();
        assert_eq!(
            ids(detect_project_checks(&d)),
            ["test=cargo test", "clippy=cargo clippy --all-targets -- -D warnings", "npm-test=npm test"]
        );
        std::fs::remove_file(d.join("Cargo.toml")).unwrap();
        assert_eq!(ids(detect_project_checks(&d)), ["npm-test=npm test"]);
        std::fs::remove_file(d.join("package.json")).unwrap();

        for py in ["pyproject.toml", "pytest.ini", "setup.py", "setup.cfg"] {
            std::fs::write(d.join(py), "").unwrap();
            assert_eq!(ids(detect_project_checks(&d)), ["pytest=pytest"], "{py}");
            std::fs::remove_file(d.join(py)).unwrap();
        }
        // A directory with the name is not a project file.
        std::fs::create_dir(d.join("Cargo.toml")).unwrap();
        assert!(detect_project_checks(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn setup_message_keys_exist_in_both_languages() {
        for key in KEYS {
            assert_ne!(lang::text(Lang::En, key), key, "{key}");
            assert_ne!(lang::text(Lang::ZhTw, key), key, "{key}");
        }
        assert!(lang::text(Lang::ZhTw, "setup.refuse.no_tty").contains("請在終端機打 {0}"));
    }
}
