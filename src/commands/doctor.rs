//! `thoth doctor`: a read-only health check (setup scope SETUP-1).
//!
//! It creates nothing and changes nothing: no state directory, no writable
//! SQLite connection (every database is opened `SQLITE_OPEN_READ_ONLY`, so no
//! migration runs), no events, no report refresh. The only children it starts
//! are each CLI's `--version`, the existing login-status commands (through
//! `profile::probe_auth_profile`) and `claude plugin list --json`.
//!
//! Every item carries an English `detail` and `fix` for `--json`; the human
//! output takes the same sentences from `lang`'s table in `THOTH_LANG`'s
//! language, and every shell command in a fix is written for the user's shell
//! (`user_shell`).

use crate::adapter::{antigravity, HARNESS_CLAUDE_CODE, HARNESS_CODEX};
use crate::checks;
use crate::config::{self, Paths, StateDirProbe};
use crate::db::global::{self, Profile};
use crate::lang::{self, Lang};
use crate::plan_label;
use crate::user_shell::UserShell;
use anyhow::{anyhow, Result};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Ok,
    Warn,
    Fail,
    Unknown,
}

impl Status {
    fn name(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
            Status::Unknown => "unknown",
        }
    }

    pub(crate) fn mark(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Warn => "!",
            Status::Fail => "✗",
            Status::Unknown => "?",
        }
    }
}

/// One sentence in English (for JSON) and in the output language.
#[derive(Debug, Clone, Default)]
pub(crate) struct Txt {
    pub(crate) en: String,
    pub(crate) local: String,
}

impl Txt {
    /// Text that is the same in every language: a command, a path.
    pub(crate) fn raw(s: impl Into<String>) -> Txt {
        let s = s.into();
        Txt { en: s.clone(), local: s }
    }

    pub(crate) fn join(parts: &[Txt], sep: &str) -> Txt {
        Txt {
            en: parts.iter().map(|p| p.en.as_str()).collect::<Vec<_>>().join(sep),
            local: parts.iter().map(|p| p.local.as_str()).collect::<Vec<_>>().join(sep),
        }
    }
}

pub(crate) struct Item {
    pub(crate) id: String,
    pub(crate) label: Txt,
    pub(crate) status: Status,
    pub(crate) detail: Txt,
    pub(crate) fix: Option<Txt>,
    pub(crate) data: Value,
}

pub(crate) struct Ctx {
    pub(crate) lang: Lang,
    pub(crate) shell: UserShell,
}

impl Ctx {
    pub(crate) fn t(&self, key: &str, args: &[&str]) -> Txt {
        Txt {
            en: lang::fill(lang::text(Lang::En, key), args),
            local: lang::fill(lang::text(self.lang, key), args),
        }
    }

    /// Several fix lines as one `\n`-separated text.
    pub(crate) fn lines(&self, parts: Vec<Txt>) -> Option<Txt> {
        (!parts.is_empty()).then(|| Txt::join(&parts, "\n"))
    }

    pub(crate) fn item(&self, id: &str, label: Txt, status: Status, detail: Txt, fix: Option<Txt>, data: Value) -> Item {
        Item { id: id.to_string(), label, status, detail, fix, data }
    }
}

pub fn run(cwd: &Path, shell_flag: Option<&str>, json: bool) -> Result<i32> {
    let (shell, shell_source) = match shell_flag {
        Some(s) => (
            UserShell::parse(s).ok_or_else(|| anyhow!("unknown --shell value: {s}"))?,
            "flag",
        ),
        None => UserShell::detect(),
    };
    let c = Ctx { lang: Lang::from_env(), shell };
    let probe = config::probe_state_dir()?;
    let paths = Paths::from_root(probe.home.clone());

    let mut items = vec![state_dir_item(&c, &probe)];
    let profiles = read_profiles(&paths);
    let used = |harness: &str| match &profiles {
        Profiles::Listed(list) => list.iter().any(|p| p.harness == harness),
        _ => false,
    };
    let rerun = "doctor.fix.rerun";
    items.push(cli_item(&c, Cli::Claude, used(HARNESS_CLAUDE_CODE), rerun));
    items.push(cli_item(&c, Cli::Codex, used(HARNESS_CODEX), rerun));
    items.push(cli_item(&c, Cli::Agy, used(antigravity::HARNESS_NAME), rerun));
    items.push(profiles_item(&c, &profiles));
    if let Profiles::Listed(list) = &profiles {
        let labels = plan_label::read_only_labels(&paths);
        let logins = crate::login_name::read_only_names(&paths);
        for p in list {
            let label = labels.get(&p.profile_id).map(String::as_str);
            let login = logins.get(&p.profile_id).map(String::as_str);
            items.push(profile_item(&c, p, label, login));
        }
    }
    let (repo, state) = repo_item(&c, &paths, cwd);
    let repo_root = repo_root_of(&repo);
    items.push(repo);
    items.push(checks_item(&c, &state, "doctor.fix.checks_path"));
    items.push(config_item_with_records(&c, &paths.home));
    items.push(plugin_item(&c, repo_root.as_deref()));

    let fails = items.iter().filter(|i| i.status == Status::Fail).count();
    let doc = json!({
        "ok": fails == 0,
        "version": env!("CARGO_PKG_VERSION"),
        "shell": shell.name(),
        "shell_source": shell_source,
        "items": items.iter().map(|i| json!({
            "id": i.id,
            "status": i.status.name(),
            "detail": i.detail.en,
            "fix": i.fix.as_ref().map(|f| f.en.clone()),
            "data": i.data,
        })).collect::<Vec<_>>(),
    });
    let mut human = Vec::new();
    for i in &items {
        human.extend(render_item(i));
    }
    human.push(if fails == 0 {
        c.t("doctor.summary.ok", &[]).local
    } else {
        c.t("doctor.summary.fail", &[&fails.to_string()]).local
    });
    crate::output::emit(json, &doc, &human.join("\n"));
    Ok(if fails == 0 { 0 } else { 1 })
}

/// One item as the human reads it: `<label> … <mark> <detail>`, then one
/// `  → <fix>` line per fix line.
pub(crate) fn render_item(item: &Item) -> Vec<String> {
    let mut out = vec![format!("{} … {} {}", item.label.local, item.status.mark(), item.detail.local)];
    if let Some(fix) = &item.fix {
        for line in fix.local.lines() {
            out.push(format!("  → {line}"));
        }
    }
    out
}

/// The repository root a `repo` item found (its `data["root"]`); `None`
/// outside a git repository.
pub(crate) fn repo_root_of(repo: &Item) -> Option<PathBuf> {
    repo.data["root"].as_str().map(PathBuf::from)
}

pub(crate) fn state_dir_item(c: &Ctx, p: &StateDirProbe) -> Item {
    let path = p.home.display().to_string();
    let data = json!({
        "path": path,
        "source": p.source,
        "exists": p.exists,
        "old_dir_only": p.old_dir_only.as_ref().map(|o| o.display().to_string()),
        "agentctl_var_only": p.agentctl_var_only,
    });
    let label = c.t("doctor.label.state_dir", &[]);
    let after_persist = if cfg!(windows) { "doctor.fix.new_window" } else { "doctor.fix.shell_profile" };
    let (status, detail, fix) = if let Some(old) = &p.old_dir_only {
        let old = old.display().to_string();
        let cmd = c.shell.persist_env("THOTH_HOME", &old);
        (
            Status::Fail,
            c.t("doctor.state_dir.old_dir_only", &[&old, &path]),
            c.lines(vec![c.t("doctor.fix.persist_old_dir", &[&cmd]), c.t(after_persist, &[])]),
        )
    } else if let Some(var) = &p.agentctl_var_only {
        let cmd = c.shell.persist_env("THOTH_HOME", var);
        (
            Status::Fail,
            c.t("doctor.state_dir.agentctl_var_only", &[var, &path]),
            c.lines(vec![c.t("doctor.fix.persist_var", &[&cmd]), c.t(after_persist, &[])]),
        )
    } else if !p.exists {
        let key = if p.source == "env" { "doctor.state_dir.missing_env" } else { "doctor.state_dir.missing_default" };
        (Status::Warn, c.t(key, &[&path]), None)
    } else if p.source == "env" {
        (Status::Ok, c.t("doctor.state_dir.ok_env", &[&path]), None)
    } else {
        (Status::Ok, c.t("doctor.state_dir.ok_default", &[&path]), None)
    };
    c.item("state_dir", label, status, detail, fix, data)
}

/// One stale check's detail; the hash differs even when no recorded field does
/// (an approval made without `approved_detail`).
fn stale_detail(c: &Ctx, check_id: &str, changed: &[&str]) -> Txt {
    if changed.is_empty() {
        c.t("doctor.checks.stale_unknown", &[check_id])
    } else {
        c.t("doctor.checks.stale", &[check_id, &changed.join(", ")])
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Cli {
    Claude,
    Codex,
    Agy,
}

impl Cli {
    fn raw(self) -> String {
        match self {
            Cli::Claude => config::claude_bin(),
            Cli::Codex => config::codex_bin(),
            Cli::Agy => antigravity::agy_bin(),
        }
    }

    fn var(self) -> &'static str {
        match self {
            Cli::Claude => "THOTH_CLAUDE_BIN",
            Cli::Codex => "THOTH_CODEX_BIN",
            Cli::Agy => "THOTH_AGY_BIN",
        }
    }

    fn setting(self) -> &'static str {
        match self {
            Cli::Claude => "claude-bin",
            Cli::Codex => "codex-bin",
            Cli::Agy => "agy-bin",
        }
    }

    fn id(self) -> &'static str {
        match self {
            Cli::Claude => "cli_claude",
            Cli::Codex => "cli_codex",
            Cli::Agy => "cli_agy",
        }
    }
}

/// What one CLI invocation gave back.
pub(crate) struct Ran {
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) out: std::process::Output,
}

/// Resolves `raw` the way `profile check` and the runner do (an npm `.cmd`
/// shim becomes node.exe plus its script on Windows) and runs it once.
pub(crate) fn run_cli(raw: &str, args: &[&str]) -> std::result::Result<Ran, String> {
    let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (program, launch) =
        crate::process::resolve_harness_command(raw, &args).map_err(|e| format!("{e:#}"))?;
    let mut cmd = std::process::Command::new(&program);
    cmd.args(&launch);
    crate::process::quiet(&mut cmd);
    let out = cmd.output().map_err(|e| e.to_string())?;
    Ok(Ran { program, args: launch, out })
}

pub(crate) fn first_line(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// `rerun_key` is the last fix line's message (doctor's or setup's).
pub(crate) fn cli_item(c: &Ctx, cli: Cli, used: bool, rerun_key: &str) -> Item {
    let raw = cli.raw();
    let source = if std::env::var_os(cli.var()).is_some() {
        "env"
    } else {
        let home = config::probe_state_dir().ok().map(|p| p.home);
        match home.and_then(|h| crate::commands::settings::resolve(&h, cli.setting())) {
            Some(r) if r.source == "file" => "file",
            _ => "path",
        }
    };
    let label = c.t(&format!("doctor.label.{}", cli.id()), &[]);
    let mut data = json!({"raw": raw, "source": source, "program": null, "args": null, "version": null});
    let problem = match run_cli(&raw, &["--version"]) {
        Err(e) => Some(c.t("doctor.cli.not_run", &[&raw, &e])),
        Ok(ran) => {
            let mut version = first_line(&ran.out.stdout);
            if version.is_empty() {
                version = first_line(&ran.out.stderr);
            }
            data["program"] = json!(ran.program);
            data["args"] = json!(ran.args);
            data["version"] = json!(version);
            if ran.out.status.success() {
                return c.item(cli.id(), label, Status::Ok, c.t("doctor.cli.ok", &[&version]), None, data);
            }
            let code = ran.out.status.code().map_or("none".to_string(), |n| n.to_string());
            Some(c.t("doctor.cli.exit", &[&raw, &code, &first_line(&ran.out.stderr)]))
        }
    };
    let mut detail = problem.unwrap_or_default();
    if !used {
        detail = Txt::join(&[detail, c.t("doctor.cli.unused", &[])], " ");
    }
    let mut fix = match cli {
        Cli::Claude => vec![c.t("doctor.fix.install_claude", &["npm install -g @anthropic-ai/claude-code"])],
        Cli::Codex => vec![c.t("doctor.fix.install_codex", &["npm install -g @openai/codex"])],
        Cli::Agy => vec![c.t("doctor.fix.install_agy", &["https://antigravity.google/"])],
    };
    if matches!(cli, Cli::Agy) && cfg!(not(windows)) {
        fix.push(c.t(
            "doctor.fix.install_agy_linux",
            &["curl -fsSL https://antigravity.google/cli/install.sh | sh"],
        ));
    }
    fix.push(c.t(rerun_key, &[]));
    let status = if used { Status::Fail } else { Status::Warn };
    c.item(cli.id(), label, status, detail, c.lines(fix), data)
}

pub(crate) enum Profiles {
    NoDb,
    Failed(String),
    Listed(Vec<Profile>),
}

/// A read-only connection: never creates the file, never migrates it.
pub(crate) fn open_read_only(path: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(conn)
}

pub(crate) fn read_profiles(paths: &Paths) -> Profiles {
    let db = paths.global_db();
    if !db.is_file() {
        return Profiles::NoDb;
    }
    match open_read_only(&db).and_then(|conn| global::list_profiles(&conn)) {
        Ok(list) => Profiles::Listed(list),
        Err(e) => Profiles::Failed(format!("{e:#}")),
    }
}

pub(crate) fn profiles_item(c: &Ctx, profiles: &Profiles) -> Item {
    let label = c.t("doctor.label.profiles", &[]);
    let add_fix = || {
        c.lines(vec![
            c.t(
                "doctor.fix.profile_add",
                &["thoth profile login --harness claude-code|codex|antigravity"],
            ),
            c.t("doctor.fix.profile_docs", &[]),
        ])
    };
    match profiles {
        Profiles::NoDb => c.item(
            "profiles",
            label,
            Status::Fail,
            c.t("doctor.profiles.no_db", &[]),
            add_fix(),
            json!({"count": 0}),
        ),
        Profiles::Failed(e) => c.item(
            "profiles",
            label,
            Status::Unknown,
            c.t("doctor.profiles.error", &[e]),
            None,
            Value::Null,
        ),
        Profiles::Listed(list) if list.is_empty() => c.item(
            "profiles",
            label,
            Status::Fail,
            c.t("doctor.profiles.none", &[]),
            add_fix(),
            json!({"count": 0}),
        ),
        Profiles::Listed(list) => c.item(
            "profiles",
            label,
            Status::Ok,
            c.t("doctor.profiles.ok", &[&list.len().to_string()]),
            None,
            json!({"count": list.len()}),
        ),
    }
}

/// `label` is the stored bracket text (TIER-1) and the heading also names the
/// login (PANEL-3): `Profile claude-a (alice, Max)` / `帳號 claude-a（alice，Max）`.
/// The login name comes from the email this check just saw, else
/// `stored_login`. The detail is what this check just saw.
pub(crate) fn profile_item(c: &Ctx, p: &Profile, label: Option<&str>, stored_login: Option<&str>) -> Item {
    let id = format!("profile:{}", p.profile_id);
    let key = "doctor.label.profile";
    let heading = |login: Option<&str>| Txt {
        en: lang::fill(
            lang::text(Lang::En, key),
            &[&crate::login_name::named(&p.profile_id, login, label, Lang::En)],
        ),
        local: lang::fill(
            lang::text(c.lang, key),
            &[&crate::login_name::named(&p.profile_id, login, label, c.lang)],
        ),
    };
    let mut data = json!({
        "harness": p.harness,
        "provider": p.provider,
        "status": p.status,
        "logged_in": null,
        "email": null,
        "subscription_type": null,
    });
    let auth = match crate::commands::profile::probe_auth_profile(p) {
        Ok(a) => a,
        Err(e) => {
            let detail = c.t("doctor.profile.error", &[&format!("{e:#}")]);
            return c.item(&id, heading(stored_login), Status::Unknown, detail, None, data);
        }
    };
    let fresh_login = auth.email.as_deref().and_then(crate::login_name::from_email);
    let label = heading(fresh_login.as_deref().or(stored_login));
    data["logged_in"] = json!(auth.logged_in);
    data["email"] = json!(auth.email);
    data["subscription_type"] = json!(auth.subscription_type);
    if auth.logged_in {
        let about: Vec<&str> = [auth.subscription_type.as_deref(), auth.email.as_deref()]
            .into_iter()
            .flatten()
            .collect();
        let detail = if about.is_empty() {
            c.t("doctor.profile.ok", &[])
        } else {
            c.t("doctor.profile.ok_with", &[&about.join(", ")])
        };
        return c.item(&id, label, Status::Ok, detail, None, data);
    }
    let fix = c.lines(vec![c.t("doctor.fix.login", &[&format!("thoth profile login {}", p.profile_id)])]);
    c.item(&id, label, Status::Fail, c.t("doctor.profile.logged_out", &[]), fix, data)
}

/// What `repo` found, for `checks`.
pub(crate) enum RepoState {
    NotGit,
    Unknown,
    NotInit,
    Ready { root: PathBuf, state_db: PathBuf },
}

pub(crate) fn repo_item(c: &Ctx, paths: &Paths, cwd: &Path) -> (Item, RepoState) {
    let label = c.t("doctor.label.repo", &[]);
    let root = match crate::worktree::repo_root_from(cwd) {
        Ok(r) => r,
        Err(_) => {
            let detail = c.t("doctor.repo.not_git", &[&cwd.display().to_string()]);
            return (c.item("repo", label, Status::Unknown, detail, None, Value::Null), RepoState::NotGit);
        }
    };
    let repo_id = match crate::ids::repo_id_for(&root) {
        Ok(id) => id,
        Err(e) => {
            let detail = c.t("doctor.repo.error", &[&format!("{e:#}")]);
            return (c.item("repo", label, Status::Unknown, detail, None, Value::Null), RepoState::Unknown);
        }
    };
    let state_db = paths.state_db(&repo_id);
    let root_s = root.display().to_string();
    let data = json!({"root": root_s, "repo_id": repo_id, "state_db": state_db.display().to_string()});
    if !state_db.is_file() {
        let fix = c.lines(vec![c.t("doctor.fix.init", &["thoth init"])]);
        let detail = c.t("doctor.repo.not_init", &[&root_s]);
        return (c.item("repo", label, Status::Fail, detail, fix, data), RepoState::NotInit);
    }
    let detail = c.t("doctor.repo.ok", &[&root_s]);
    (c.item("repo", label, Status::Ok, detail, None, data), RepoState::Ready { root, state_db })
}

/// `path_key` is the message for a missing program (doctor's or setup's).
pub(crate) fn checks_item(c: &Ctx, state: &RepoState, path_key: &str) -> Item {
    let label = c.t("doctor.label.checks", &[]);
    let unknown = |detail: Txt| c.item("checks", label.clone(), Status::Unknown, detail, None, Value::Null);
    let (root, state_db) = match state {
        RepoState::NotGit => return unknown(c.t("doctor.checks.no_repo", &[])),
        RepoState::Unknown => return unknown(c.t("doctor.checks.no_repo_id", &[])),
        RepoState::NotInit => return unknown(c.t("doctor.checks.not_init", &[])),
        RepoState::Ready { root, state_db } => (root, state_db),
    };
    let (conn, list) = match open_read_only(state_db).and_then(|conn| {
        let list = checks::list_checks(&conn)?;
        Ok((conn, list))
    }) {
        Ok(l) => l,
        Err(e) => return unknown(c.t("doctor.checks.error", &[&format!("{e:#}")])),
    };
    // Where `Paths::blobs_dir` keeps the approved copies of package files.
    let blobs_dir = state_db.parent().map(|d| d.join("blobs")).unwrap_or_default();
    let total = list.len();
    let approved = list.iter().filter(|x| x.approved_at.is_some()).count();
    let pending = total - approved;
    let (mut missing, mut stale, mut unreadable) = (Vec::new(), Vec::new(), Vec::new());
    for check in list.iter().filter(|x| x.approved_at.is_some()) {
        let read = checks::fingerprint(check, root)
            .and_then(|now| Ok((checks::approvals(&conn, &check.check_id)?, now)));
        match read {
            Err(e) => unreadable.push(json!({"check_id": check.check_id, "error": format!("{e:#}")})),
            // A program missing from PATH outranks a stale pin: an approval
            // made while the program was missing still matches, and `checks
            // list` calls it approved, yet `verify` could not run it.
            Ok((_, now)) if now.program.is_none() => missing.push(json!({
                "check_id": check.check_id,
                "program": checks::program_word(&check.shell, &check.cmd),
            })),
            // The same judgement as `checks list` and `verify`.
            Ok((all, now)) => {
                if let checks::ApprovalMatch::NoMatch { changed, .. } =
                    checks::match_approval(check, &all, &now, root, &blobs_dir, true)
                {
                    let changed = match all.first() {
                        Some(newest) => checks::changed_lines(check, newest, &now),
                        None => changed,
                    };
                    stale.push(json!({"check_id": check.check_id, "changed": changed}));
                }
            }
        }
    }
    let s = |v: &Value, k: &str| v[k].as_str().unwrap_or("").to_string();
    let mut detail = vec![c.t(
        "doctor.checks.counts",
        &[&total.to_string(), &approved.to_string(), &pending.to_string()],
    )];
    // `list_checks` is ordered by check_id.
    let path_only: Vec<&str> =
        list.iter().filter(|x| x.exe_pin == "path").map(|x| x.check_id.as_str()).collect();
    for id in &path_only {
        detail.push(c.t("doctor.checks.path_only", &[id]));
    }
    for m in &missing {
        detail.push(c.t("doctor.checks.missing", &[&s(m, "check_id"), &s(m, "program")]));
    }
    for st in &stale {
        let changed: Vec<&str> = st["changed"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        detail.push(stale_detail(c, &s(st, "check_id"), &changed));
    }
    for u in &unreadable {
        detail.push(c.t("doctor.checks.unreadable", &[&s(u, "check_id"), &s(u, "error")]));
    }
    let mut fix = Vec::new();
    if !missing.is_empty() {
        let programs: Vec<String> = missing.iter().map(|m| s(m, "program")).collect();
        fix.extend(path_fix_lines(c, path_key, &programs));
    }
    if !stale.is_empty() {
        fix.push(c.t("doctor.fix.checks_stale", &["thoth checks approve"]));
    }
    if pending > 0 {
        fix.push(c.t("doctor.fix.checks_pending", &["thoth checks approve"]));
    }
    if total == 0 {
        fix.push(c.t("doctor.fix.checks_none", &["thoth checks add test --cmd \"cargo test\""]));
    }
    let status = if !missing.is_empty() {
        Status::Fail
    } else if !stale.is_empty() || !unreadable.is_empty() || pending > 0 || total == 0 {
        Status::Warn
    } else {
        Status::Ok
    };
    let data = json!({
        "total": total,
        "approved": approved,
        "pending": pending,
        "missing_program": missing,
        "stale": stale,
        "unreadable": unreadable,
        "path_only": path_only,
    });
    c.item("checks", label, status, Txt::join(&detail, "; "), c.lines(fix), data)
}

/// The "put it on PATH" fix lines for `programs` (duplicates dropped): one
/// line per well-known folder that holds some of them, then one line with the
/// placeholder `DIR` for the rest.
pub(crate) fn path_fix_lines(c: &Ctx, key: &str, programs: &[String]) -> Vec<Txt> {
    let home = user_home();
    let mut found: Vec<(PathBuf, Vec<String>)> = Vec::new();
    let mut rest: Vec<String> = Vec::new();
    for p in programs {
        if found.iter().any(|(_, ps)| ps.contains(p)) || rest.contains(p) {
            continue;
        }
        match well_known_dir(p, home.as_deref(), &|f: &Path| f.is_file()) {
            Some(dir) => match found.iter_mut().find(|(d, _)| *d == dir) {
                Some((_, ps)) => ps.push(p.clone()),
                None => found.push((dir, vec![p.clone()])),
            },
            None => rest.push(p.clone()),
        }
    }
    let mut lines: Vec<Txt> = found
        .iter()
        .map(|(dir, ps)| c.t(key, &[&ps.join(", "), &c.shell.prepend_path(&dir.display().to_string())]))
        .collect();
    if !rest.is_empty() {
        lines.push(c.t(key, &[&rest.join(", "), &c.shell.prepend_path("DIR")]));
    }
    lines
}

/// USERPROFILE, else HOME (an empty value counts as unset).
fn user_home() -> Option<PathBuf> {
    config::home_dir_from(&|k| std::env::var_os(k))
}

/// `claude plugin marketplace add <source>`; a source with whitespace in it
/// is double-quoted (PowerShell, cmd, bash and sh all take that).
pub(crate) fn marketplace_add_command(source: &str) -> String {
    if source.chars().any(char::is_whitespace) {
        format!("claude plugin marketplace add \"{source}\"")
    } else {
        format!("claude plugin marketplace add {source}")
    }
}

/// The first of `<home>/.cargo/bin` and `<home>/.local/bin` that holds
/// `program` (or `program.exe`); `None` for a program word with a path in it.
pub(crate) fn well_known_dir(program: &str, home: Option<&Path>, is_file: &dyn Fn(&Path) -> bool) -> Option<PathBuf> {
    if program.contains('/') || program.contains('\\') {
        return None;
    }
    let home = home?;
    [home.join(".cargo").join("bin"), home.join(".local").join("bin")]
        .into_iter()
        .find(|dir| is_file(&dir.join(program)) || is_file(&dir.join(format!("{program}.exe"))))
}

/// Reads config.json itself rather than through `settings::read_flags`,
/// which treats a broken file as "all off" without saying so.
pub(crate) fn config_item(c: &Ctx, path: &Path) -> Item {
    let label = c.t("doctor.label.config", &[]);
    let p = path.display().to_string();
    if !path.exists() {
        let data = json!({"path": p, "exists": false, "unknown_keys": []});
        return c.item("config", label, Status::Ok, c.t("doctor.config.none", &[&p]), None, data);
    }
    let parsed: std::result::Result<serde_json::Map<String, Value>, Txt> = std::fs::read_to_string(path)
        .map_err(|e| Txt::raw(e.to_string()))
        .and_then(|text| serde_json::from_str::<Value>(&text).map_err(|e| Txt::raw(e.to_string())))
        .and_then(|v| match v {
            Value::Object(m) => Ok(m),
            _ => Err(c.t("doctor.config.not_object", &[])),
        });
    let map = match parsed {
        Ok(m) => m,
        Err(why) => {
            let detail = Txt {
                en: lang::fill(lang::text(Lang::En, "doctor.config.broken"), &[&p, &why.en]),
                local: lang::fill(lang::text(c.lang, "doctor.config.broken"), &[&p, &why.local]),
            };
            let fix = c.lines(vec![c.t("doctor.fix.config_broken", &[&c.shell.remove_file(&p)])]);
            let data = json!({"path": p, "exists": true, "unknown_keys": []});
            return c.item("config", label, Status::Warn, detail, fix, data);
        }
    };
    let known = crate::commands::settings::stored_keys();
    let retired = crate::commands::settings::RETIRED_KEYS;
    let unknown_keys: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| !known.contains(k) && !retired.contains(k))
        .collect();
    let mut stored_keys: Vec<&str> = map
        .keys()
        .map(String::as_str)
        .filter(|k| known.contains(k))
        .collect();
    stored_keys.sort_unstable();
    let flag = |k: &str| map.get(k).and_then(Value::as_bool).unwrap_or(false);
    let data = json!({
        "path": p,
        "exists": true,
        "unknown_keys": unknown_keys,
        "stored_keys": stored_keys,
        "live_quota": flag("live_quota"),
    });
    if !unknown_keys.is_empty() {
        let keys = unknown_keys.join(", ");
        let detail = c.t("doctor.config.unknown_keys", &[&p, &keys]);
        let fix = c.lines(vec![c.t("doctor.fix.config_unknown", &[&p, &keys])]);
        return c.item("config", label, Status::Warn, detail, fix, data);
    }
    let detail = c.t("doctor.config.ok", &[&p, &stored_keys.len().to_string()]);
    c.item("config", label, Status::Ok, detail, None, data)
}

/// `config_item` plus what DT-3 adds: this computer's name and the shared
/// records folder, which is a warning when it is set and is not a directory.
pub(crate) fn config_item_with_records(c: &Ctx, home: &Path) -> Item {
    let machine = crate::commands::settings::machine_name(home);
    let records = crate::commands::settings::resolve(home, "records-dir");
    let mut item = config_item(c, &home.join("config.json"));
    let (records_dir, set_path) = match records {
        Some(r) if r.source != "default" => {
            let exists = Path::new(&r.value).is_dir();
            (json!({"value": r.value, "source": r.source, "exists": exists}), Some((r.value, exists)))
        }
        _ => (json!({"value": null, "source": "default", "exists": null}), None),
    };
    if let Some(data) = item.data.as_object_mut() {
        data.insert("machine_name".to_string(), json!(machine));
        data.insert("records_dir".to_string(), records_dir);
    }
    if let (Status::Ok, Some((path, false))) = (&item.status, set_path) {
        let detail = Txt {
            en: lang::fill(lang::text(Lang::En, "doctor.config.records_dir_missing"), &[&item.detail.en, &path]),
            local: lang::fill(
                lang::text(c.lang, "doctor.config.records_dir_missing"),
                &[&item.detail.local, &path],
            ),
        };
        item.status = Status::Warn;
        item.detail = detail;
        item.fix = Some(c.t("doctor.fix.records_dir", &["thoth config set records-dir PATH"]));
    }
    add_secrets(c, home, &mut item);
    item
}

/// Secrets.json (the two score API keys) in the `config` item: `data` gets
/// `secrets_file` and `secrets` (set or unset with source per key, never
/// the content), and a broken, odd or world-readable file turns the item into
/// a warning. Placed after the records-dir warning, which only fires while
/// the item is still ok. While THOTH_RUN_ID is set the file is not opened
/// and changes nothing.
fn add_secrets(c: &Ctx, home: &Path, item: &mut Item) {
    let report = crate::commands::secrets::Report::inspect(home);
    let mut secrets = serde_json::Map::new();
    for key in ["aistupidlevel-api-key", "artificialanalysis-api-key"] {
        let (value, source) = match crate::commands::settings::resolve(home, key) {
            Some(r) => (r.value, r.source),
            None => ("unset".to_string(), "default"),
        };
        secrets.insert(key.to_string(), json!({"value": value, "source": source}));
    }
    if let Some(data) = item.data.as_object_mut() {
        data.insert("secrets_file".to_string(), report.json(false));
        data.insert("secrets".to_string(), Value::Object(secrets));
    }
    if !report.read {
        return;
    }
    let path = report.path.display().to_string();
    let warn = |item: &mut Item, key: &str, args_en: &[&str], args_local: &[&str], fix: Txt| {
        item.status = Status::Warn;
        item.detail = Txt {
            en: lang::fill(lang::text(Lang::En, key), args_en),
            local: lang::fill(lang::text(c.lang, key), args_local),
        };
        item.fix = c.lines(item.fix.take().into_iter().chain([fix]).collect());
    };
    if report.broken() {
        let error = report.error().unwrap_or_default().to_string();
        let fix = c.t("doctor.fix.config_broken", &[&c.shell.remove_file(&path)]);
        let (en, local) = (item.detail.en.clone(), item.detail.local.clone());
        warn(item, "doctor.config.secrets_broken", &[&en, &path, &error], &[&local, &path, &error], fix);
    } else if let Some(unknown) = report.unknown_keys.as_ref().filter(|u| !u.is_empty()) {
        let keys = unknown.join(", ");
        let fix = c.t("doctor.fix.config_unknown", &[&path, &keys]);
        let (en, local) = (item.detail.en.clone(), item.detail.local.clone());
        warn(item, "doctor.config.secrets_unknown", &[&en, &path, &keys], &[&local, &path, &keys], fix);
    }
    if report.private == Some(false) {
        let fix = c.t("doctor.fix.secrets_private", &[&format!("chmod 600 {path}")]);
        let (en, local) = (item.detail.en.clone(), item.detail.local.clone());
        warn(item, "doctor.config.secrets_exposed", &[&en, &path], &[&local, &path], fix);
    }
}

/// A marketplace install reports `<version>-<hash>` (e.g.
/// `6.4.2-8ca22dba9a94`), so a hash suffix still counts as the same version.
pub fn plugin_version_matches(installed: &str, expected: &str) -> bool {
    installed == expected
        || installed
            .strip_prefix(expected)
            .is_some_and(|rest| rest.starts_with('-'))
}

/// Where `claude plugin marketplace add` installs from when this is not
/// Thoth's own repository.
pub(crate) const PLUGIN_MARKETPLACE_SOURCE: &str = "johnny-lo/Thoth";

/// `repo_root` itself when it is Thoth's own repository (its marketplace file
/// names `thoth` and the plugin's manifest is there), else the GitHub source.
pub(crate) fn plugin_marketplace_source(repo_root: Option<&Path>) -> String {
    let local = repo_root.filter(|root| {
        let named_thoth = std::fs::read_to_string(root.join(".claude-plugin").join("marketplace.json"))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .is_some_and(|v| v.is_object() && v["name"].as_str() == Some("thoth"));
        let manifest = root.join("plugins").join("thoth").join(".claude-plugin").join("plugin.json");
        named_thoth && manifest.is_file()
    });
    match local {
        Some(root) => root.display().to_string(),
        None => PLUGIN_MARKETPLACE_SOURCE.to_string(),
    }
}

pub(crate) fn plugin_item(c: &Ctx, repo_root: Option<&Path>) -> Item {
    let label = c.t("doctor.label.plugin", &[]);
    let expected = env!("CARGO_PKG_VERSION");
    let unknown = |detail: Txt| c.item("plugin", label.clone(), Status::Unknown, detail, None, Value::Null);
    let ran = match run_cli(&config::claude_bin(), &["plugin", "list", "--json"]) {
        Ok(r) => r,
        Err(_) => return unknown(c.t("doctor.plugin.no_claude", &[])),
    };
    let stderr = first_line(&ran.out.stderr);
    if !ran.out.status.success() {
        let code = ran.out.status.code().map_or("none".to_string(), |n| n.to_string());
        return unknown(c.t("doctor.plugin.failed", &[&code, &stderr]));
    }
    let list = match serde_json::from_slice::<Value>(&ran.out.stdout) {
        Ok(Value::Array(a)) => a,
        _ => return unknown(c.t("doctor.plugin.bad_output", &[&stderr])),
    };
    let ours: Vec<&Value> = list
        .iter()
        .filter(|e| e["id"].as_str().is_some_and(|id| id.starts_with("thoth@")))
        .collect();
    let Some(first) = ours.first() else {
        let data = json!({
            "found": false, "id": null, "enabled": null, "installed_version": null,
            "expected_version": expected, "install_path": null,
        });
        let fix = c.lines(vec![
            c.t(
                "doctor.fix.plugin_install",
                &[&marketplace_add_command(&plugin_marketplace_source(repo_root))],
            ),
            Txt::raw("claude plugin install thoth@thoth"),
        ]);
        return c.item("plugin", label, Status::Fail, c.t("doctor.plugin.missing", &[]), fix, data);
    };
    let entry = ours
        .iter()
        .find(|e| e["enabled"].as_bool() == Some(true))
        .unwrap_or(first);
    let id = entry["id"].as_str().unwrap_or("").to_string();
    let enabled = entry["enabled"].as_bool() == Some(true);
    let installed = entry["version"].as_str().unwrap_or("").to_string();
    let data = json!({
        "found": true,
        "id": id,
        "enabled": enabled,
        "installed_version": entry["version"],
        "expected_version": expected,
        "install_path": entry["installPath"],
    });
    if !enabled {
        let fix = c.lines(vec![c.t("doctor.fix.plugin_enable", &[&format!("claude plugin enable {id}")])]);
        return c.item("plugin", label, Status::Fail, c.t("doctor.plugin.disabled", &[&id]), fix, data);
    }
    if !plugin_version_matches(&installed, expected) {
        let fix = c.lines(vec![c.t("doctor.fix.plugin_update", &[&format!("claude plugin update {id}")])]);
        let detail = c.t("doctor.plugin.version", &[&id, &installed, expected]);
        return c.item("plugin", label, Status::Fail, detail, fix, data);
    }
    c.item("plugin", label, Status::Ok, c.t("doctor.plugin.ok", &[&id, &installed]), None, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marketplace_add_command_quotes_only_sources_with_whitespace() {
        assert_eq!(marketplace_add_command("johnny-lo/Thoth"), "claude plugin marketplace add johnny-lo/Thoth");
        assert_eq!(
            marketplace_add_command(r"C:\Users\Some One\Thoth"),
            r#"claude plugin marketplace add "C:\Users\Some One\Thoth""#
        );
        assert_eq!(marketplace_add_command("/home/u/my thoth"), "claude plugin marketplace add \"/home/u/my thoth\"");
        assert_eq!(marketplace_add_command("/home/u/thoth"), "claude plugin marketplace add /home/u/thoth");
    }

    #[test]
    fn doctor_plugin_version_matches() {
        assert!(plugin_version_matches("0.1.0", "0.1.0"));
        assert!(plugin_version_matches("0.1.0-8ca22dba9a94", "0.1.0"));
        assert!(!plugin_version_matches("0.1.01", "0.1.0"));
        assert!(!plugin_version_matches("0.1", "0.1.0"));
        assert!(!plugin_version_matches("0.2.0", "0.1.0"));
    }

    #[test]
    fn doctor_stale_detail_with_and_without_changed_fields() {
        let c = Ctx { lang: Lang::En, shell: UserShell::Bash };
        assert_eq!(stale_detail(&c, "t", &[]).en, "t: fingerprint differs");
        assert_eq!(stale_detail(&c, "t", &["a=1", "b=2"]).en, "t: changed a=1, b=2");
    }

    #[test]
    fn doctor_statuses_and_marks() {
        let all = [Status::Ok, Status::Warn, Status::Fail, Status::Unknown];
        let names: Vec<_> = all.iter().map(|s| s.name()).collect();
        let marks: Vec<_> = all.iter().map(|s| s.mark()).collect();
        assert_eq!(names, ["ok", "warn", "fail", "unknown"]);
        assert_eq!(marks, ["✓", "!", "✗", "?"]);
    }

    #[test]
    fn doctor_well_known_dir() {
        let home = Path::new("/h");
        let cargo_bin = home.join(".cargo").join("bin");
        let local_bin = home.join(".local").join("bin");
        let plain = cargo_bin.join("cargo");
        let exe = cargo_bin.join("cargo.exe");
        assert_eq!(well_known_dir("cargo", Some(home), &|p: &Path| p == plain), Some(cargo_bin.clone()));
        assert_eq!(well_known_dir("cargo", Some(home), &|p: &Path| p == exe), Some(cargo_bin.clone()));
        let in_local = local_bin.join("uv");
        assert_eq!(well_known_dir("uv", Some(home), &|p: &Path| p == in_local), Some(local_bin));
        // Both hold it: .cargo/bin comes first.
        assert_eq!(well_known_dir("cargo", Some(home), &|_| true), Some(cargo_bin));
        assert_eq!(well_known_dir("cargo", Some(home), &|_| false), None);
        assert_eq!(well_known_dir("cargo", None, &|_| true), None);
        assert_eq!(well_known_dir("bin/cargo", Some(home), &|_| true), None);
        assert_eq!(well_known_dir(r"bin\cargo", Some(home), &|_| true), None);
    }

    #[test]
    fn doctor_plugin_marketplace_source() {
        let dir = std::env::temp_dir().join(format!("thoth-doctor-pms-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        std::fs::create_dir_all(dir.join("plugins").join("thoth").join(".claude-plugin")).unwrap();
        std::fs::write(dir.join("plugins/thoth/.claude-plugin/plugin.json"), r#"{"name":"thoth"}"#).unwrap();
        let market = dir.join(".claude-plugin").join("marketplace.json");
        std::fs::write(&market, r#"{"name":"thoth","plugins":[]}"#).unwrap();
        assert_eq!(plugin_marketplace_source(Some(&dir)), dir.display().to_string());
        std::fs::write(&market, r#"{"name":"other","plugins":[]}"#).unwrap();
        assert_eq!(plugin_marketplace_source(Some(&dir)), "johnny-lo/Thoth");
        std::fs::write(&market, r#"{"name":"thoth","plugins":[]}"#).unwrap();
        std::fs::remove_file(dir.join("plugins/thoth/.claude-plugin/plugin.json")).unwrap();
        assert_eq!(plugin_marketplace_source(Some(&dir)), "johnny-lo/Thoth");
        assert_eq!(plugin_marketplace_source(None), "johnny-lo/Thoth");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn doctor_render_item_lines() {
        let c = Ctx { lang: Lang::En, shell: UserShell::Bash };
        let fix = c.lines(vec![Txt::raw("a"), Txt::raw("b")]);
        let item = c.item("x", Txt::raw("L"), Status::Warn, Txt::raw("d"), fix, Value::Null);
        assert_eq!(render_item(&item), ["L … ! d", "  → a", "  → b"]);
    }

    #[test]
    fn doctor_fix_text_follows_the_language_but_json_stays_english() {
        let c = Ctx { lang: Lang::ZhTw, shell: UserShell::Cmd };
        let t = c.t("doctor.fix.config_broken", &[&c.shell.remove_file("x")]);
        assert_eq!(t.en, "Fix or delete this file: del \"x\"");
        assert_eq!(t.local, "修好或刪掉這個檔：del \"x\"");
    }
}
