//! `thoth checks add | approve | list | remove` (P7-1a).
//!
//! The catalogue of approved checks lives in `state.db`; see `crate::checks`
//! for what an approval pins. These commands only manage the catalogue;
//! `verify` runs what it holds (P7-1b).
//!
//! Who approves: the commander (its shell or MCP, with a reason) or a person
//! at a terminal. `approved_by` records which; it is a note for the reviewer,
//! not a security boundary - nothing here is sandboxed.

use crate::checks::{self, Check};
use crate::config::Paths;
use crate::db::state::{self, NewEvent};
use crate::ids;
use crate::model::event_type;
use crate::output;
use crate::protected::Change;
use crate::worktree;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

/// Terminal statuses a run must be in before `approve --run` will read its
/// worktree: nothing may still be writing there.
const SETTLED_RUN_STATUSES: [&str; 4] = ["completed", "failed", "cancelled", "interrupted"];

/// `approved_by`: a person at a terminal (stdin and stdout both are one).
pub(crate) const HUMAN_TERMINAL: &str = "human_terminal";
/// `approved_by`: the commander's shell, no terminal attached.
pub(crate) const COMMANDER_CLI: &str = "commander_cli";
/// `approved_by`: the commander through the MCP tool `thoth_checks_approve`.
pub(crate) const COMMANDER_MCP: &str = "commander_mcp";

/// Who is approving, and the trimmed reason. Only a person at a terminal may
/// leave the reason out; a blank one is refused whoever gives it.
pub(crate) fn approver(
    via_mcp: bool,
    terminal: bool,
    reason: Option<&str>,
) -> Result<(&'static str, Option<String>)> {
    let reason = crate::commands::auto_pick::clean_reason(reason)?;
    let who = if via_mcp {
        COMMANDER_MCP
    } else if terminal {
        HUMAN_TERMINAL
    } else {
        COMMANDER_CLI
    };
    if who != HUMAN_TERMINAL && reason.is_none() {
        bail!(
            "checks approve needs --reason when no terminal is attached (the commander's shell or \
             MCP): say why this approval is right; a person at a terminal may leave it out"
        );
    }
    Ok((who, reason))
}

/// A worker or reviewer has `THOTH_RUN_ID` in its environment; the catalogue is
/// the commander's and the human's, so every writer refuses there (same guard
/// as `review record`). Checked first, before anything is opened or written.
fn refuse_inside_a_run(command: &str) -> Result<()> {
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!(
            "checks {command} refuses to run with THOTH_RUN_ID set: the approved-check \
             catalogue is changed by the supervising session and the human, not by a run"
        );
    }
    Ok(())
}

pub struct AddArgs<'a> {
    pub id: &'a str,
    pub cmd: &'a str,
    pub shell: &'a str,
    pub cwd: &'a str,
    pub inputs: &'a [String],
    /// `hash` (the default) or `path`.
    pub exe_pin: &'a str,
    pub json: bool,
}

pub fn add(paths: &Paths, cwd: &Path, args: AddArgs<'_>) -> Result<i32> {
    let (check, _replaced) = add_check(paths, cwd, &args)?;
    let hints: Vec<&str> = fail_fast_hint(&check.shell, &check.cmd).into_iter().collect();

    let v = serde_json::json!({
        "ok": true,
        "check_id": check.check_id,
        "shell": check.shell,
        "cmd": check.cmd,
        "cwd": check.cwd,
        "inputs": check.inputs,
        "status": "pending",
        "exe_pin": check.exe_pin,
        "hints": hints,
    });
    let mut human = format!(
        "{}  pending  [{}] {}  (cwd {}, {} input file(s){})\nnot approved yet; approve it with: thoth checks approve --reason \"WHY\"",
        check.check_id,
        check.shell,
        check.cmd,
        check.cwd,
        check.inputs.len(),
        if check.exe_pin == "path" { ", program pinned by path only" } else { "" }
    );
    for h in &hints {
        human.push_str(&format!("\nhint: {h}"));
    }
    output::emit(args.json, &v, &human);
    Ok(0)
}

/// A reminder, never a refusal: `cargo test` without `--no-fail-fast` stops
/// at the first failing test target, so a later target's new tests go unrun.
fn fail_fast_hint(shell: &str, cmd: &str) -> Option<&'static str> {
    let words: Vec<String> = if shell == "argv" {
        crate::process::split_argv(cmd)
    } else {
        cmd.split_whitespace().map(str::to_string).collect()
    };
    let program = words.first()?;
    let name = program.rsplit(['/', '\\']).next().unwrap_or("").to_lowercase();
    if name.strip_suffix(".exe").unwrap_or(&name) != "cargo" {
        return None;
    }
    let sub = words[1..].iter().find(|w| !w.starts_with('-') && !w.starts_with('+'))?;
    if sub != "test" && sub != "t" {
        return None;
    }
    if words.iter().take_while(|w| *w != "--").any(|w| w == "--no-fail-fast") {
        return None;
    }
    Some(
        "cargo test stops at the first test target that fails, so later targets and their new \
         tests never run; consider cargo test --no-fail-fast",
    )
}

/// `checks add` without printing: stores the check as pending and returns it
/// and whether it replaced one. `thoth setup` calls it too.
pub(crate) fn add_check(paths: &Paths, cwd: &Path, args: &AddArgs<'_>) -> Result<(Check, bool)> {
    refuse_inside_a_run("add")?;
    if !checks::valid_check_id(args.id) {
        bail!("check id '{}' must match [a-z0-9_-]{{1,32}}", args.id);
    }
    if !checks::SHELLS.contains(&args.shell) {
        bail!(
            "--shell must be one of {}, got '{}'",
            checks::SHELLS.join(", "),
            args.shell
        );
    }
    if !matches!(args.exe_pin, "hash" | "path") {
        bail!("--exe-pin must be hash or path, got '{}'", args.exe_pin);
    }
    if args.shell == "argv" && args.cmd.trim().is_empty() {
        bail!("--cmd must not be blank");
    }
    // AC12 (b), AC13, AC14 (a): the same refusals verify applies to the text.
    if let Some(reason) = checks::command_refusal(args.shell, args.cmd) {
        bail!("{reason}");
    }
    let Some(check_cwd) = checks::normalize_rel(args.cwd) else {
        bail!(
            "--cwd '{}' must be a relative path inside the repo (no absolute path, drive or ..)",
            args.cwd
        );
    };
    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;

    let mut inputs: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for raw in args.inputs {
        match checks::normalize_rel(raw) {
            Some(n) if n != "." && root.join(&n).is_file() => inputs.push(n),
            _ => missing.push(raw.clone()),
        }
    }
    if !missing.is_empty() {
        bail!(
            "--input must name existing files in the main work tree; not found: {}",
            missing.join(", ")
        );
    }
    inputs.extend(checks::infer_inputs(&root, &check_cwd, args.shell, args.cmd));
    inputs.sort();
    inputs.dedup();

    let replaced = checks::get_check(&conn, args.id)?.is_some();
    let check = Check {
        check_id: args.id.to_string(),
        shell: args.shell.to_string(),
        cmd: args.cmd.to_string(),
        cwd: check_cwd,
        inputs,
        created_at: ids::now_rfc3339(),
        approved_at: None,
        approved_fingerprint: None,
        approved_detail: None,
        approved_commit: None,
        exe_pin: args.exe_pin.to_string(),
    };
    if check.exe_pin == "path" {
        let fp = checks::fingerprint(&check, &root)?;
        if let Some(reason) = path_pin_refusal(paths, &repo_id, &root, &check, &fp)? {
            bail!("{reason}");
        }
    }
    let tx = conn.unchecked_transaction()?;
    checks::put_check(&tx, &check)?;
    state::append_event(
        &tx,
        &NewEvent {
            task_id: None,
            run_id: None,
            event_type: event_type::CHECK_ADDED.to_string(),
            detail: serde_json::json!({
                "check_id": check.check_id,
                "shell": check.shell,
                "cmd": check.cmd,
                "cwd": check.cwd,
                "inputs": check.inputs,
                "replaced": replaced,
                "exe_pin": check.exe_pin,
            }),
        },
    )?;
    tx.commit()?;
    Ok((check, replaced))
}

/// Why a `--exe-pin path` check must not pin its program by path alone, or
/// `None`. `fp` is the check's fingerprint against the tree being read. The
/// program must resolve, lie outside the repository tree and every worktree
/// (a worker can edit files there), and not be named by a relative path (each
/// tree would find another file under the same `exe=` line). Shared by `add`
/// and `approve`, so both say the same thing.
pub(crate) fn path_pin_refusal(
    paths: &Paths,
    repo_id: &str,
    root: &Path,
    check: &Check,
    fp: &checks::Fingerprint,
) -> Result<Option<String>> {
    let word = checks::program_word(&check.shell, &check.cmd);
    let Some(program) = &fp.program else {
        return Ok(Some(format!(
            "--exe-pin path needs a program that resolves now: '{word}' does not resolve to a \
             file (relative to the check's cwd for a path, along the absolute PATH entries otherwise)"
        )));
    };
    let mut trees: Vec<(PathBuf, &str)> = vec![
        (root.to_path_buf(), "the repository tree"),
        (paths.worktrees_dir(repo_id), "a worktree"),
        (paths.repo_dir(repo_id).join("baseline-worktrees"), "a worktree"),
    ];
    for wt in worktree::list_worktrees(root)? {
        trees.push((wt, "a worktree"));
    }
    for (tree, name) in &trees {
        if checks::inside_tree(program, tree) {
            return Ok(Some(format!(
                "--exe-pin path is refused for {}: it is inside {name} ({}); a worker can edit \
                 files there, so pin it by its bytes (the default)",
                program.display(),
                tree.display()
            )));
        }
    }
    let shown = fp.detail["exe"]["path"].as_str().unwrap_or("");
    if !Path::new(shown).is_absolute() {
        return Ok(Some(format!(
            "--exe-pin path is refused for '{word}': a program named by a relative path is looked \
             up again in each tree the check runs in; use its bare name with its folder on PATH, \
             its absolute path, or pin it by its bytes (the default)"
        )));
    }
    Ok(None)
}

/// The tree an approval reads: the main work tree, or a settled run's worktree.
fn approval_tree(
    conn: &rusqlite::Connection,
    root: &Path,
    run: Option<&str>,
) -> Result<(PathBuf, &'static str, Option<state::Run>)> {
    let Some(needle) = run else {
        return Ok((root.to_path_buf(), "main", None));
    };
    let run_id = ids::resolve_prefix(&state::list_run_ids(conn)?, needle)?;
    let r = state::get_run(conn, &run_id)?
        .ok_or_else(|| anyhow::anyhow!("run {run_id} disappeared"))?;
    if !SETTLED_RUN_STATUSES.contains(&r.status.as_str()) {
        bail!(
            "run {run_id} is '{}'; approve --run needs completed, failed, cancelled or interrupted",
            r.status
        );
    }
    Ok((PathBuf::from(&r.worktree), "run", Some(r)))
}

/// Every run of the repo that has not ended, by run id: recorded with each
/// approval, so the reviewer sees who else could have run the command.
fn live_run_ids(conn: &rusqlite::Connection) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for task_id in state::list_task_ids(conn)? {
        out.extend(state::list_live_runs_for_task(conn, &task_id)?.into_iter().map(|r| r.run_id));
    }
    out.sort();
    Ok(out)
}

pub fn approve(
    paths: &Paths,
    cwd: &Path,
    run: Option<&str>,
    reason: Option<&str>,
    json: bool,
) -> Result<i32> {
    refuse_inside_a_run("approve")?;
    let (approved_by, reason) = approver(
        std::env::var("THOTH_VIA_MCP").as_deref() == Ok("1"),
        crate::commands::profile_login::stdio_is_terminal(),
        reason,
    )?;
    let outcome = approve_checks(paths, cwd, run, None, approved_by, reason.as_deref())?;
    for w in &outcome.warnings {
        masked_eprintln!("warning: {w}");
    }
    let mut approved = Vec::new();
    let mut human = String::new();
    for (c, fp) in &outcome.approved {
        human.push_str(&format!(
            "approved {}\n  shell: {}\n  cmd:   {}\n  cwd:   {}\n  exe:   {}{}\n",
            c.check_id,
            c.shell,
            c.cmd,
            c.cwd,
            fp.detail["exe"]["path"].as_str().unwrap_or(""),
            if c.exe_pin == "path" { "  (pinned by path only)" } else { "" }
        ));
        for i in fp.detail["inputs"].as_array().into_iter().flatten() {
            let sha = i["sha256"].as_str().map(|h| &h[..12.min(h.len())]).unwrap_or("missing");
            human.push_str(&format!(
                "  input: {}  {}\n",
                i["path"].as_str().unwrap_or(""),
                sha
            ));
        }
        approved.push(serde_json::json!({
            "check_id": c.check_id,
            "fingerprint": fp.hex,
            "source": outcome.source,
        }));
    }
    for (id, why) in &outcome.refused {
        human.push_str(&format!("not approved {id}: {why}\n"));
    }
    if let Some(run_id) = outcome.protected_run.as_deref() {
        for c in &outcome.protected_approved {
            human.push_str(&format!(
                "approved protected change of run {run_id}: {} ({}, {})\n",
                c.path, c.status, c.kind
            ));
        }
    }
    if !approved.is_empty() || !outcome.protected_approved.is_empty() {
        human.push_str(&format!(
            "approved by {approved_by}; reason: {}\n",
            reason.as_deref().unwrap_or("-")
        ));
    }
    if approved.is_empty() && outcome.refused.is_empty() && outcome.protected_approved.is_empty() {
        human = "nothing to approve".to_string();
    }
    let protected_approved: Vec<serde_json::Value> =
        outcome.protected_approved.iter().map(Change::to_json).collect();
    let mut v = serde_json::json!({
        "ok": true,
        "approved": approved,
        "unchanged": outcome.unchanged,
        "approved_by": approved_by,
        "reason": reason,
        "protected_approved": protected_approved,
        "protected_unchanged": outcome.protected_unchanged,
    });
    if !outcome.refused.is_empty() {
        v["refused"] = outcome
            .refused
            .iter()
            .map(|(id, reason)| serde_json::json!({"check_id": id, "reason": reason}))
            .collect();
    }
    output::emit(json, &v, human.trim_end());
    Ok(0)
}

/// What `approve_checks` did.
pub(crate) struct ApproveOutcome {
    /// Each check approved now, with the fingerprint it was pinned to.
    pub approved: Vec<(Check, checks::Fingerprint)>,
    /// Checks already approved with the current fingerprint.
    pub unchanged: Vec<String>,
    /// With `only`: listed checks whose fingerprint changed since they were
    /// listed, or that are gone; none of them was approved.
    pub changed_since_listed: Vec<String>,
    /// `--exe-pin path` checks not approved, with the reason
    /// (`path_pin_refusal`).
    pub refused: Vec<(String, String)>,
    /// `"main"` or `"run"`.
    pub source: &'static str,
    /// With `--run`: that run's id.
    pub protected_run: Option<String>,
    /// With `--run`: the run's protected test and check-word changes approved
    /// now (`protected::uncovered`).
    pub protected_approved: Vec<Change>,
    /// With `--run`: paths of those changes an earlier approval already covers.
    pub protected_unchanged: Vec<String>,
    /// Said on stderr by `approve`, with or without `--json`.
    pub warnings: Vec<String>,
}

/// `checks approve` without printing. `only` limits it to the listed
/// `(check_id, fingerprint hex)` pairs, approving each only while its hex is
/// still the listed one (`thoth setup` approves exactly what it showed);
/// checks outside the list are left alone.
///
/// Everything is read and written in one write transaction (BEGIN IMMEDIATE):
/// `dispatch` and `review start` insert their run row in theirs before they
/// start a worker, so no run can start, and reuse the worktree read here,
/// half way through. With `run`, a live run of that run's task refuses the
/// whole approval: a run of the same attempt reuses that worktree.
pub(crate) fn approve_checks(
    paths: &Paths,
    cwd: &Path,
    run: Option<&str>,
    only: Option<&[(String, String)]>,
    approved_by: &str,
    reason: Option<&str>,
) -> Result<ApproveOutcome> {
    refuse_inside_a_run("approve")?;
    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let tx = crate::db::write_tx(&conn)?;
    let (tree, source, run_row) = approval_tree(&tx, &root, run)?;
    let run_id: Option<String> = run_row.as_ref().map(|r| r.run_id.clone());
    // Asked before the worktree is read; the write lock held since BEGIN keeps
    // the answer current through the writes below.
    if let Some(r) = &run_row {
        if let Some(live) = state::list_live_runs_for_task(&tx, &r.task_id)?.into_iter().next() {
            bail!(
                "checks approve --run {} refuses while task {} has a live run ({} is {}): a run \
                 of the same attempt reuses that worktree, so its files may still be changing; \
                 wait for it to end, then approve again",
                r.run_id,
                r.task_id,
                live.run_id,
                live.status
            );
        }
    }

    let mut to_approve: Vec<(Check, checks::Fingerprint)> = Vec::new();
    let mut unchanged: Vec<String> = Vec::new();
    let mut changed_since_listed: Vec<String> = Vec::new();
    let mut refused: Vec<(String, String)> = Vec::new();
    // Package copies are blobs on disk, written once the transaction is over.
    let mut copies: Vec<Check> = Vec::new();
    let commit = worktree::rev_parse_head(&tree)?;
    let all = checks::list_checks(&tx)?;
    if let Some(list) = only {
        for (id, _) in list {
            if !all.iter().any(|c| c.check_id == *id) {
                changed_since_listed.push(id.clone());
            }
        }
    }
    for c in all {
        let listed = match only {
            None => None,
            Some(list) => match list.iter().find(|(id, _)| *id == c.check_id) {
                Some((_, hex)) => Some(hex.as_str()),
                None => continue,
            },
        };
        let fp = checks::fingerprint(&c, &tree)?;
        // Any approval with exactly this fingerprint is enough; a match only
        // through package-info auto-accept still gets an approval of its own.
        if checks::approvals(&tx, &c.check_id)?.iter().any(|a| a.fingerprint == fp.hex) {
            unchanged.push(c.check_id.clone());
            copies.push(c);
            continue;
        }
        if listed.is_some_and(|hex| hex != fp.hex) {
            changed_since_listed.push(c.check_id.clone());
            continue;
        }
        // Checked again at approval: the program may now resolve somewhere
        // a worker can edit (this process's PATH, the tree read now).
        if c.exe_pin == "path" {
            if let Some(reason) = path_pin_refusal(paths, &repo_id, &root, &c, &fp)? {
                refused.push((c.check_id.clone(), reason));
                continue;
            }
        }
        to_approve.push((c, fp));
    }

    // With `run`: the run's protected test and check-word changes no earlier
    // approval covers, read from its newest diff evidence. A list that cannot
    // be read is "not known", never "none": nothing is approved for it.
    let mut protected_approved: Vec<Change> = Vec::new();
    let mut protected_unchanged: Vec<String> = Vec::new();
    let mut protected_diff_code: Option<String> = None;
    let mut warnings: Vec<String> = Vec::new();
    if let Some(r) = &run_row {
        if let Some(latest) = crate::protected::latest_diff(&tx, r)? {
            match &latest.changes {
                None => warnings.push(format!(
                    "run {}'s diff evidence {} has no readable protected_changes; run review \
                     start, then approve again",
                    r.run_id, latest.code
                )),
                Some(changes) => {
                    let earlier = crate::protected::approved_pairs(&tx, &r.run_id)?;
                    protected_approved = crate::protected::uncovered(changes, &earlier);
                    protected_unchanged = crate::protected::uncovered(changes, &[])
                        .into_iter()
                        .filter(|c| !protected_approved.contains(c))
                        .map(|c| c.path)
                        .collect();
                    protected_diff_code = Some(latest.code.clone());
                }
            }
        }
    }

    // Read right before the writes, inside the same transaction.
    let live_runs = live_run_ids(&tx)?;
    for (c, fp) in &to_approve {
        let now = ids::now_rfc3339();
        checks::record_approval(&tx, &c.check_id, &now, fp, &commit, source, run_id.as_deref())?;
        state::append_event(
            &tx,
            &NewEvent {
                task_id: None,
                run_id: run_id.clone(),
                event_type: event_type::CHECK_APPROVED.to_string(),
                detail: serde_json::json!({
                    "check_id": c.check_id,
                    "fingerprint": fp.hex,
                    "detail": fp.detail,
                    "source": source,
                    "run_id": run_id,
                    "approved_by": approved_by,
                    "reason": reason,
                    "live_runs": live_runs,
                }),
            },
        )?;
    }
    if let Some(r) = run_row.as_ref().filter(|_| !protected_approved.is_empty()) {
        state::append_event(
            &tx,
            &NewEvent {
                task_id: Some(r.task_id.clone()),
                run_id: Some(r.run_id.clone()),
                event_type: event_type::PROTECTED_CHANGE_APPROVED.to_string(),
                detail: serde_json::json!({
                    "run_id": r.run_id,
                    "diff_code": protected_diff_code,
                    "changes": protected_approved.iter().map(Change::to_json).collect::<Vec<_>>(),
                    "approved_by": approved_by,
                    "reason": reason,
                    "live_runs": live_runs,
                }),
            },
        )?;
    }
    tx.commit()?;

    for c in copies.iter().chain(to_approve.iter().map(|(c, _)| c)) {
        checks::store_package_copies(paths, &repo_id, c, &tree);
    }
    Ok(ApproveOutcome {
        approved: to_approve,
        unchanged,
        changed_since_listed,
        refused,
        source,
        protected_run: run_id,
        protected_approved,
        protected_unchanged,
        warnings,
    })
}

pub fn list(paths: &Paths, cwd: &Path, json: bool) -> Result<i32> {
    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let blobs_dir = paths.blobs_dir(&repo_id);
    let mut rows = Vec::new();
    let mut human = Vec::new();
    for c in checks::list_checks(&conn)? {
        let now = checks::fingerprint(&c, &root)?;
        let all = checks::approvals(&conn, &c.check_id)?;
        let mut matched: Option<(checks::Approval, &str)> = None;
        let mut package_lines: Vec<checks::PackageLine> = Vec::new();
        let (status, changed) = match all.first() {
            None => ("pending", Vec::new()),
            Some(_) if c.approved_at.is_none() => ("pending", Vec::new()),
            Some(newest) => match checks::match_approval(&c, &all, &now, &root, &blobs_dir, true) {
                checks::ApprovalMatch::Exact(a) => {
                    matched = Some((a, "exact"));
                    ("approved", Vec::new())
                }
                checks::ApprovalMatch::PackageInfoOnly { approval, lines } => {
                    matched = Some((approval, "package_info_only"));
                    package_lines = lines;
                    ("approved", Vec::new())
                }
                checks::ApprovalMatch::NoMatch { .. } => {
                    ("stale", checks::changed_lines(&c, newest, &now))
                }
            },
        };
        let other_approvals = all.len().saturating_sub(1);
        let mut line = format!(
            "{}  {}  [{}] {}  cwd {}  inputs [{}]  approved {}",
            c.check_id,
            status,
            c.shell,
            c.cmd,
            c.cwd,
            c.inputs.join(", "),
            c.approved_at.as_deref().unwrap_or("-")
        );
        if c.exe_pin == "path" {
            line.push_str("  exe pinned by path only");
        }
        if other_approvals > 0 {
            line.push_str(&format!("  (+{other_approvals} earlier approvals)"));
        }
        let package_info_lines: serde_json::Value = package_lines
            .iter()
            .map(|l| serde_json::json!({"path": l.path, "line": l.line, "approved": l.approved, "now": l.now}))
            .collect();
        if matched.as_ref().is_some_and(|(_, kind)| *kind == "package_info_only") {
            line.push_str(&format!(
                "\n  auto-accepted ({}): {}",
                crate::commands::verify::AUTO_ACCEPTED,
                crate::commands::verify::package_lines_text(&package_info_lines)
            ));
        }
        if !changed.is_empty() {
            line.push_str(&format!("\n  changed: {}", changed.join(", ")));
        }
        human.push(line);
        rows.push(serde_json::json!({
            "check_id": c.check_id,
            "status": status,
            "shell": c.shell,
            "cmd": c.cmd,
            "cwd": c.cwd,
            "inputs": c.inputs,
            "approved_at": c.approved_at,
            "approved_fingerprint": c.approved_fingerprint,
            "changed": changed,
            "exe_pin": c.exe_pin,
            "other_approvals": other_approvals,
            "match": matched.as_ref().map(|(_, kind)| *kind),
            "matched_approval": matched.as_ref().map(|(a, _)| serde_json::json!({
                "fingerprint": a.fingerprint, "approved_at": a.approved_at,
                "source": a.source, "run_id": a.run_id,
            })),
            "package_info_lines": package_info_lines,
        }));
    }
    let patterns = crate::protected::load_patterns(&conn)?;
    let mut text = if human.is_empty() {
        "no checks".to_string()
    } else {
        human.join("\n")
    };
    text.push_str(&format!(
        "\nprotected patterns: {}",
        if patterns.is_empty() { "(none)".to_string() } else { patterns.join(", ") }
    ));
    output::emit(
        json,
        &serde_json::json!({"checks": rows, "protected_patterns": patterns}),
        &text,
    );
    Ok(0)
}

/// `checks protect PATTERN`: adds one extra protected pattern (P7-2). The list
/// lives in `state.db`'s `meta` table, where a worker cannot edit it.
pub fn protect(paths: &Paths, cwd: &Path, pattern: &str, json: bool) -> Result<i32> {
    refuse_inside_a_run("protect")?;
    let stored = match crate::protected::parse_pattern(pattern) {
        Ok(p) => p,
        Err(why) => bail!("{why}"),
    };
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let mut patterns = crate::protected::load_patterns(&conn)?;
    let added = !patterns.contains(&stored);
    patterns.push(stored.clone());
    let patterns = crate::protected::store_patterns(&conn, patterns)?;
    let human = format!(
        "{} {stored}\nprotected patterns: {}",
        if added { "protected" } else { "already protected" },
        patterns.join(", ")
    );
    output::emit(
        json,
        &serde_json::json!({"ok": true, "pattern": stored, "added": added, "protected_patterns": patterns}),
        &human,
    );
    Ok(0)
}

/// `checks unprotect PATTERN`: drops a pattern `protect` added.
pub fn unprotect(paths: &Paths, cwd: &Path, pattern: &str, json: bool) -> Result<i32> {
    refuse_inside_a_run("unprotect")?;
    let stored = crate::protected::normalize(pattern.trim());
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let mut patterns = crate::protected::load_patterns(&conn)?;
    if !patterns.contains(&stored) {
        bail!("no protected pattern '{pattern}'");
    }
    patterns.retain(|p| *p != stored);
    let patterns = crate::protected::store_patterns(&conn, patterns)?;
    let human = format!(
        "unprotected {stored}\nprotected patterns: {}",
        if patterns.is_empty() { "(none)".to_string() } else { patterns.join(", ") }
    );
    output::emit(
        json,
        &serde_json::json!({"ok": true, "pattern": stored, "protected_patterns": patterns}),
        &human,
    );
    Ok(0)
}

pub fn remove(paths: &Paths, cwd: &Path, id: &str, json: bool) -> Result<i32> {
    refuse_inside_a_run("remove")?;
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let tx = conn.unchecked_transaction()?;
    let Some(before) = checks::get_check(&tx, id)? else {
        bail!("no check '{id}'");
    };
    let was_approved = before.approved_at.is_some();
    if !checks::delete_check(&tx, id)? {
        bail!("no check '{id}'");
    }
    state::append_event(
        &tx,
        &NewEvent {
            task_id: None,
            run_id: None,
            event_type: event_type::CHECK_REMOVED.to_string(),
            detail: serde_json::json!({
                "check_id": id,
                "was_approved": was_approved,
                "approved_at": before.approved_at,
            }),
        },
    )?;
    tx.commit()?;
    let human = if was_approved {
        format!(
            "removed {id} (it was approved; the next verify lists it under \
             missing_since_last_verify)"
        )
    } else {
        format!("removed {id}")
    };
    output::emit(
        json,
        &serde_json::json!({"ok": true, "check_id": id, "was_approved": was_approved}),
        &human,
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approver_names_who_and_requires_a_reason_without_a_terminal() {
        assert_eq!(approver(false, true, None).unwrap(), (HUMAN_TERMINAL, None));
        assert_eq!(
            approver(false, true, Some("  x \n")).unwrap(),
            (HUMAN_TERMINAL, Some("x".to_string()))
        );
        let err = approver(false, false, None).unwrap_err().to_string();
        assert!(err.contains("--reason"), "{err}");
        assert_eq!(
            approver(false, false, Some("y")).unwrap(),
            (COMMANDER_CLI, Some("y".to_string()))
        );
        let err = approver(true, true, None).unwrap_err().to_string();
        assert!(err.contains("--reason"), "{err}");
        assert_eq!(
            approver(true, true, Some("z")).unwrap(),
            (COMMANDER_MCP, Some("z".to_string()))
        );
        let err = approver(false, false, Some("   ")).unwrap_err().to_string();
        assert!(err.contains("must not be empty"), "{err}");
    }
}
