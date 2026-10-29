//! `thoth verify <run>` — the control plane's re-run of the repo's approved
//! checks (v6 §2.2 rule 4, brief §10; P7-1b).
//!
//! It runs only checks from the human-approved catalogue (`thoth checks`):
//! the ids the task's own spec lists under `checks`, or every check in the
//! catalogue when it lists none. Each runs with the shell it declares, in the
//! run's own worktree, once its fingerprint, re-computed there, matches one of
//! the check's approvals, or differs from one only in package information
//! (`checks::match_approval`); anything else becomes a refused row. Each is recorded as
//! `collected_by = control_plane` evidence. The `verification_requests` a
//! worker declared in its `result.json` are listed as `worker_declared` for
//! reference and never run - nothing a worker wrote, and nothing on the
//! command line, is executed. It does not touch the task's status - a
//! Supervisor reads the table and decides. `--dry-run` prints what would run
//! without running it.
//!
//! Fix round 1 hardened three things a first pass got wrong, and they still
//! hold: a `cwd` is checked before it is ever joined onto the worktree path
//! (I1), a re-run has a deadline and a kill path (I2), and the run's own
//! ingested record - not the mutable worktree file - is what the reference
//! list is read from (I3).

use crate::checks::{self, cwd_escapes_the_worktree, Check};
use crate::config::Paths;
use crate::db::state::{self, NewEvent, Run};
use crate::evidence;
use crate::ids;
use crate::model::event_type;
use crate::output;
use crate::process;
use crate::result_json::VerificationRequest;
use crate::worktree;
use anyhow::{bail, Result};
use rusqlite::{Connection, OptionalExtension};

use std::path::{Path, PathBuf};

/// The commands a run declared (P7-1b: listed for reference, never run), a
/// human-readable note about where they were read from, and - when both
/// sources exist and disagree, or neither exists - a warning.
///
/// Fix round 1 / I3 (reverses Open question 15's file-first choice): the run's
/// own `result_ingested` event wins whenever one holds a readable
/// `result_json`. `dispatch` reuses one worktree across every attempt of a
/// task, so the worktree's
/// `.agent/result.json` is mutable and can be silently overwritten by a later
/// run sharing that attempt; `parse_result` only validates the task id, which
/// every run of the task shares, so a stale/foreign file would otherwise pass
/// unnoticed. `verify` exists so the control plane can check a worker's
/// submission independently, and its instructions must come from the record
/// that worker cannot rewrite after ingest. The file is read only as a
/// fallback, for a run with no such event - or, per Minor 6, one whose event
/// carries no readable `result_json` (every run a Phase 1 build ingested).
/// That second case warns, because falling back there is a degradation, not
/// the normal absence of a record.
pub fn requests_for_run(
    conn: &Connection,
    run: &Run,
) -> Result<(Vec<VerificationRequest>, String, Option<String>)> {
    let path = Path::new(&run.worktree).join(".agent").join("result.json");

    let detail: Option<String> = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            rusqlite::params![run.run_id, event_type::RESULT_INGESTED],
            |r| r.get(0),
        )
        .optional()?;
    let from_event = detail.as_deref().and_then(|d| {
        let v: serde_json::Value = serde_json::from_str(d).ok()?;
        let doc: crate::result_json::WorkerResult =
            serde_json::from_value(v["result_json"].clone()).ok()?;
        Some(doc.verification_requests)
    });
    // Whole-branch review Minor 6: `from_event` is `None` for two different
    // reasons, and only one of them is "this run has no `result_ingested`
    // event". The other is "the event exists but its `result_json` is absent
    // or unreadable" - which is every run ingested by a Phase 1 build, whose
    // event carried no `result_json` at all - and that case fell through to
    // the mutable worktree file silently, i.e. to exactly the source this
    // function exists to distrust. It still falls through (there is nothing
    // else to fall through to), but the operator is told.
    let event_unreadable = detail.is_some() && from_event.is_none();

    if let Some(requests) = from_event {
        let source = format!("the result_ingested event for run {}", run.run_id);
        // Read the file purely to compare and warn - never to use its
        // contents once an event exists.
        let warning = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| crate::result_json::parse_result(&text, &run.task_id).ok())
            .filter(|doc| doc.verification_requests != requests)
            .map(|_| {
                format!(
                    "{} declares different verification_requests than {source} (the record \
                     actually used); the file was ignored",
                    path.display()
                )
            });
        return Ok((requests, source, warning));
    }

    // No usable result_ingested event for this run: fall back to the file.
    if let Ok(text) = std::fs::read_to_string(&path) {
        if let Ok(doc) = crate::result_json::parse_result(&text, &run.task_id) {
            let warning = event_unreadable.then(|| {
                format!(
                    "run {}'s result_ingested event exists but carries no readable \
                     result_json (a run ingested by a Phase 1 build does not), so the \
                     verification commands came from {} - a file in a worktree that a later \
                     run of the same attempt can overwrite. Re-`submit` a corrected \
                     result.json if you want them on the record instead.",
                    run.run_id,
                    path.display()
                )
            });
            return Ok((
                doc.verification_requests,
                path.display().to_string(),
                warning,
            ));
        }
    }
    // P7-1b: the list is reference only, so its absence is a warning, not a
    // reason to refuse the approved checks.
    Ok((
        Vec::new(),
        "nowhere".to_string(),
        Some(format!(
            "no worker-declared verification_requests for run {}: no result_ingested event \
             holds the document, and {} is missing or invalid either",
            run.run_id,
            path.display()
        )),
    ))
}

/// The temporary checkout `--at-base` runs in. Dropping it removes the
/// worktree, so a failed or timed-out request (or an early `?`) cannot leave
/// one behind.
struct BaseWorktree {
    root: PathBuf,
    path: PathBuf,
}

impl Drop for BaseWorktree {
    fn drop(&mut self) {
        worktree::remove_worktree(&self.root, &self.path);
    }
}

/// The message when there is nothing approved to run (`--json`:
/// `reason: "no_approved_checks"`).
pub const NO_APPROVED_CHECKS: &str = "no approved checks for this repository; add them with \
     thoth checks add ... then approve them with: thoth checks approve --reason \"WHY\"";

/// The ids a task's own spec (`tasks.spec`, not the run's copy) lists under
/// `checks`, in spec order. Read leniently: a spec that predates the key, or
/// one that does not parse, lists none.
fn spec_check_ids(task_spec: &str) -> Vec<String> {
    serde_json::from_str::<serde_json::Value>(task_spec)
        .ok()
        .and_then(|v| {
            v["checks"].as_array().map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect()
            })
        })
        .unwrap_or_default()
}

/// What `refusal_for` found out besides the refusal itself.
#[derive(Default)]
struct Compared {
    /// AC10 (1): `false` unless the fingerprint was compared and matched -
    /// never null, including for a check refused before any comparison.
    fingerprint_ok: bool,
    /// Whether the `input=` lines took part in a comparison that happened.
    inputs_checked: bool,
    /// AC10 (2), AC14 (b): the absolute path of the file the fingerprint
    /// resolved - the one that is executed, whatever the shell.
    program: Option<PathBuf>,
    /// The approval the fingerprint matched, and how (`exact` or
    /// `package_info_only`).
    approval: Option<(checks::Approval, &'static str)>,
    /// The lines package-info auto-accept let through.
    package_lines: Vec<checks::PackageLine>,
}

impl Compared {
    /// `results[].approval` and the evidence detail's `approval`.
    fn approval_json(&self) -> serde_json::Value {
        match &self.approval {
            None => serde_json::Value::Null,
            Some((a, kind)) => serde_json::json!({
                "match": kind, "fingerprint": a.fingerprint, "approved_at": a.approved_at,
                "source": a.source, "run_id": a.run_id,
            }),
        }
    }

    /// `results[].package_info_lines` and the evidence detail's.
    fn package_lines_json(&self) -> serde_json::Value {
        self.package_lines
            .iter()
            .map(|l| serde_json::json!({"path": l.path, "line": l.line, "approved": l.approved, "now": l.now}))
            .collect()
    }
}

/// What `refusal_for` reads besides the check: the run's state.db, and the
/// blob store that holds the approved copies of package files.
struct Store<'a> {
    conn: &'a Connection,
    blobs_dir: &'a Path,
}

/// Why an existing, approved check must not run here, or `None`.
#[allow(clippy::too_many_arguments)]
fn refusal_for(
    store: &Store<'_>,
    c: &Check,
    shell: Option<process::Shell>,
    tree: &Path,
    dir: &Path,
    tree_exists: bool,
    at_base: bool,
    run_id: &str,
    out: &mut Compared,
) -> Result<Option<String>> {
    let id = &c.check_id;
    if c.approved_at.is_none() {
        return Ok(Some(format!(
            "check {id} is not approved; approve it with: thoth checks approve --reason \"WHY\""
        )));
    }
    // Fix round 1 / I1 (kept): the cwd is checked before it is ever joined
    // onto a path that is used.
    if cwd_escapes_the_worktree(&c.cwd) {
        return Ok(Some(format!(
            "cwd '{}' is not allowed: it must stay inside the run's worktree (no absolute path, \
             drive/root component, or '..')",
            c.cwd
        )));
    }
    let Some(shell) = shell.filter(|s| s.available()) else {
        return Ok(Some(format!(
            "check {id} uses shell {}, which is not available on {}",
            c.shell,
            std::env::consts::OS
        )));
    };
    // AC12 (b): the fingerprint pins the first word as the program; a shell
    // that reads quotes in it would run another file. AC14 (a): a shell check
    // is one program and plain arguments, so the shell has nothing left to
    // interpret.
    if let Some(reason) = checks::command_refusal(&c.shell, &c.cmd) {
        return Ok(Some(format!("check {id}: {reason}")));
    }
    let word = checks::program_word(&c.shell, &c.cmd);
    // A baseline dry run has no checkout to fingerprint yet.
    let mut program: Option<PathBuf> = None;
    if tree_exists {
        if !dir.is_dir() {
            return Ok(Some(format!(
                "cwd '{}' does not exist under the worktree (resolved to {})",
                c.cwd,
                dir.display()
            )));
        }
        let now = checks::fingerprint(c, tree)?;
        // AC14 (b): the file this resolution found is the file that runs;
        // nothing below resolves the program again.
        let found = now.program.as_deref();
        // AC14 (d), ahead of AC11: PowerShell hands a batch file to cmd.
        // AC11 (b)(i), (c): an in-tree cmd/powershell program is named in
        // full. AC14 (c), (d): a file the shell would not run as itself.
        if let Some(reason) = checks::powershell_batch_refusal(&c.shell, found)
            .or_else(|| checks::shell_program_refusal(&c.shell, &word, tree, &c.cwd))
            .or_else(|| checks::resolved_program_refusal(&c.shell, &word, found))
        {
            return Ok(Some(format!("check {id}: {reason}")));
        }
        // Any approval of this check may match: exactly, or (with the input
        // files read) differing only in package information.
        let approvals = checks::approvals(store.conn, id)?;
        let matched = checks::match_approval(c, &approvals, &now, tree, store.blobs_dir, !at_base);
        out.inputs_checked = !at_base;
        match matched {
            checks::ApprovalMatch::Exact(a) => out.approval = Some((a, "exact")),
            checks::ApprovalMatch::PackageInfoOnly { approval, lines } => {
                out.approval = Some((approval, "package_info_only"));
                out.package_lines = lines;
            }
            checks::ApprovalMatch::NoMatch { changed, note } => {
                let note = note.map(|n| format!("; {n}")).unwrap_or_default();
                return Ok(Some(format!(
                    "fingerprint mismatch for check {id}: {}{note}; if this change is intended, \
                     approve it with: thoth checks approve --run {run_id} --reason \"WHY\"",
                    changed.join(", ")
                )));
            }
        }
        out.fingerprint_ok = true;
        program = now.program;
    }
    if shell == process::Shell::Argv {
        if let Some(reason) = process::split_argv(&c.cmd)
            .first()
            .and_then(|program| process::argv_refusal(program, dir))
        {
            return Ok(Some(format!("check {id}: {reason}")));
        }
        // AC10 (2): run exactly the file the fingerprint resolved. Handing
        // the word itself to `Command::new` would, on Windows, resolve a
        // relative path against thoth's own working directory instead. AC13:
        // `program_word` is the first element of the same split the run uses,
        // so a quoted path with a space is the program the fingerprint pinned.
        if tree_exists && program.is_none() {
            return Ok(Some(format!(
                "check {id}: program '{word}' does not resolve to an executable file \
                 (relative to the check's cwd for a path, along the absolute PATH \
                 entries otherwise)"
            )));
        }
    }
    out.program = program;
    Ok(None)
}

pub fn run(
    paths: &Paths,
    cwd: &Path,
    needle: &str,
    dry_run: bool,
    timeout_secs: u64,
    at_base: bool,
    json: bool,
) -> Result<i32> {
    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let run_id = ids::resolve_prefix(&state::list_run_ids(&conn)?, needle)?;
    let run = state::get_run(&conn, &run_id)?
        .ok_or_else(|| anyhow::anyhow!("run {run_id} disappeared"))?;

    // Fix round 1 / R3, reworked in fix round 2 / R4: a queued/running run's
    // worktree may still be getting edited by the worker itself, and
    // result.json/its ingest may be half-written. Nothing below is executed
    // or recorded for such a run. Reuses `runner::is_run_status_terminal`
    // (the same predicate the runner itself uses to decide whether a status
    // means "nobody will touch this run again") instead of a second,
    // independent `"queued" | "running"` match, so a future non-terminal
    // status cannot silently bypass this guard by not being named here.
    if !crate::runner::is_run_status_terminal(&run.status) {
        bail!(
            "run {run_id} is still {}; verify only re-runs a run's own settled submission, not \
             one that might still change",
            run.status
        );
    }

    // P7-1b: what the worker declared is listed for reference and never run.
    let (declared, _declared_from, warning) = requests_for_run(&conn, &run)?;
    let warnings: Vec<String> = warning.into_iter().collect();
    if !json {
        for w in &warnings {
            masked_eprintln!("thoth: warning: {w}");
        }
    }
    let worker_declared: Vec<serde_json::Value> = declared
        .iter()
        .map(|r| serde_json::json!({"cmd": r.cmd, "cwd": r.cwd}))
        .collect();

    let task = state::get_task(&conn, &run.task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {} disappeared", run.task_id))?;
    let spec_ids = spec_check_ids(&task.spec);
    let (check_ids, source) = if spec_ids.is_empty() {
        let all: Vec<String> = checks::list_checks(&conn)?
            .into_iter()
            .map(|c| c.check_id)
            .collect();
        (all, "approved checks (all)")
    } else {
        (spec_ids, "approved checks (spec)")
    };
    let catalogue: Vec<Option<Check>> = check_ids
        .iter()
        .map(|id| checks::get_check(&conn, id))
        .collect::<Result<_>>()?;

    // The *task's* base_commit, not the run's: a rework run's own base_commit
    // is the previous attempt's candidate, which is not a baseline.
    let at_commit: Option<String> = at_base.then(|| task.base_commit.clone());
    let label_prefix = if at_base {
        evidence::VERIFY_AT_BASE_LABEL_PREFIX
    } else {
        evidence::VERIFY_LABEL_PREFIX
    };

    // CK-1: the approved checks now, against those the previous verify (any
    // task, any run) saw.
    let approved_checks: Vec<String> = checks::list_checks(&conn)?
        .into_iter()
        .filter(|c| c.approved_at.is_some())
        .map(|c| c.check_id)
        .collect();
    let previous = state::latest_event_of_type(&conn, event_type::VERIFY_RAN, None)?;
    let (previous_verify, missing_since_last_verify) =
        compare_with_previous(&conn, &approved_checks, previous.as_ref())?;
    let previous_line = missing_line(&previous_verify, &missing_since_last_verify);

    // An empty catalogue, or a spec none of whose ids is in it: nothing to
    // run, and that is a failure, not a pass.
    if catalogue.iter().all(Option::is_none) {
        let mut doc = serde_json::json!({
            "ok": false, "run_id": run_id, "task_id": run.task_id, "source": source,
            "dry_run": dry_run, "failures": 0, "results": [], "warnings": warnings,
            "worker_declared": worker_declared, "reason": "no_approved_checks",
            "approved_checks": approved_checks, "previous_verify": previous_verify,
            "missing_since_last_verify": missing_since_last_verify,
            "head_commit": null, "head_commit_after": null, "worktree_clean": null,
        });
        if let Some(c) = &at_commit {
            doc["at_commit"] = serde_json::json!(c);
        }
        let mut human = vec![NO_APPROVED_CHECKS.to_string()];
        human.extend(previous_line.clone());
        push_worker_declared(&mut human, &worker_declared);
        output::emit(json, &doc, &human.join("\n"));
        return Ok(1);
    }

    let run_worktree = PathBuf::from(&run.worktree);
    // A baseline run never touches the run's own worktree, so it does not need
    // it to exist.
    if !at_base && !run_worktree.is_dir() {
        bail!(
            "the worktree for run {run_id} is gone ({}), so there is nowhere to re-run",
            run_worktree.display()
        );
    }
    // Where the checks execute: the run's worktree, or a temporary detached
    // checkout of base_commit under THOTH_HOME (removed by the guard on
    // every exit path, including a `?`). A dry run only names the path.
    let mut _base_guard: Option<BaseWorktree> = None;
    let worktree = if at_base {
        let base_dir = paths.repo_dir(&repo_id).join("baseline-worktrees");
        // A nonce keeps the path unique, so a checkout an interrupted run left
        // behind can never be picked up as this run's baseline.
        let path = base_dir.join(format!("{}-{}", run.run_id, ids::new_run_id()));
        if !dry_run {
            // Forget registrations whose directory an interrupted `--at-base`
            // (Ctrl-C / SIGKILL skip the drop guard) left dangling.
            worktree::prune_worktrees(&root);
            let base = at_commit.as_deref().unwrap_or_default();
            let ensured = worktree::ensure_detached_worktree(&root, &path, base)?;
            if ensured.outcome == worktree::EnsureOutcome::Reused {
                bail!("baseline checkout {} already exists; refusing to reuse it", path.display());
            }
            _base_guard = Some(BaseWorktree {
                root: root.clone(),
                path: path.clone(),
            });
        }
        path
    } else {
        run_worktree.clone()
    };

    // RFIX-1: the commit the checks ran on, and whether they left it alone:
    // HEAD before and after, and no project file changed (`.agent/` and
    // `.agents/` are Thoth's; ignored files such as build output do not
    // count, a generated file that is not ignored does). Nothing for a dry run.
    let tree_state = |dir: &Path| -> (Option<String>, bool) {
        let head = worktree::rev_parse_head(dir).ok();
        let clean = crate::review_fix_edits::project_changes(dir, false).map(|c| c.is_empty()).unwrap_or(false);
        (head, clean)
    };
    let (head_before, clean_before) = if dry_run {
        (None, false)
    } else if at_base {
        (at_commit.clone(), tree_state(&worktree).1)
    } else {
        tree_state(&worktree)
    };
    let ctx = evidence::EvidenceContext {
        paths,
        repo_id: &repo_id,
        task_id: &run.task_id,
        run_id: &run.run_id,
    };
    let timeout = std::time::Duration::from_secs(timeout_secs);
    let mut results: Vec<serde_json::Value> = Vec::new();
    // Parallel to `results`: which rows were refused, for the human exit column.
    let mut refused: Vec<bool> = Vec::new();
    let mut failures = 0usize;
    // A baseline dry run has not created its checkout, so there is nothing to
    // look in (or fingerprint) yet.
    let tree_exists = !(dry_run && at_base);
    let blobs_dir = paths.blobs_dir(&repo_id);
    let store = Store { conn: &conn, blobs_dir: &blobs_dir };
    for (i, (id, row)) in check_ids.iter().zip(&catalogue).enumerate() {
        let cmd = row.as_ref().map(|c| c.cmd.clone()).unwrap_or_default();
        let check_cwd = row.as_ref().map(|c| c.cwd.clone()).unwrap_or_else(|| ".".into());
        let shell_name = row.as_ref().map(|c| c.shell.clone());
        let shell = shell_name.as_deref().and_then(process::Shell::parse);
        let exe_pin = row.as_ref().map(|c| c.exe_pin.clone());
        let dir = worktree.join(&check_cwd);

        let mut compared = Compared::default();
        let refusal: Option<String> = match row {
            None => Some(format!("check {id} is not in this repository's check catalogue")),
            Some(c) => refusal_for(
                &store,
                c,
                shell,
                &worktree,
                &dir,
                tree_exists,
                at_base,
                &run.run_id,
                &mut compared,
            )?,
        };
        let approval = compared.approval_json();
        let package_info_lines = compared.package_lines_json();
        // A tree that matches an approval exactly holds the approved bytes:
        // keep a copy of its package files for a later package-info match.
        if !dry_run && !at_base && approval["match"] == "exact" {
            if let Some(c) = row {
                checks::store_package_copies(paths, &repo_id, c, &worktree);
            }
        }

        if dry_run {
            let mut r = serde_json::json!({
                "index": i + 1, "check_id": id, "shell": shell_name, "cmd": cmd,
                "cwd": dir.display().to_string(), "dry_run": true,
                "would_refuse": refusal.is_some(), "fingerprint_ok": compared.fingerprint_ok,
                "exe_pin": exe_pin, "approval": approval,
                "package_info_lines": package_info_lines,
            });
            if let Some(reason) = &refusal {
                r["reason"] = serde_json::json!(reason);
            }
            results.push(r);
            refused.push(refusal.is_some());
            continue;
        }

        let mut extra = serde_json::Map::new();
        extra.insert("check_id".into(), serde_json::json!(id));
        extra.insert("shell".into(), serde_json::json!(shell_name));
        // The matched approval's hash and time; without a match, the newest
        // approval's (the `checks` row holds a copy of it).
        let (fingerprint, approved_at) = match &compared.approval {
            Some((a, _)) => (Some(a.fingerprint.clone()), Some(a.approved_at.clone())),
            None => (
                row.as_ref().and_then(|c| c.approved_fingerprint.clone()),
                row.as_ref().and_then(|c| c.approved_at.clone()),
            ),
        };
        extra.insert("fingerprint".into(), serde_json::json!(fingerprint));
        extra.insert("fingerprint_ok".into(), serde_json::json!(compared.fingerprint_ok));
        extra.insert("inputs_checked".into(), serde_json::json!(compared.inputs_checked));
        extra.insert("approved_at".into(), serde_json::json!(approved_at));
        extra.insert("approval".into(), approval.clone());
        extra.insert("package_info_lines".into(), package_info_lines.clone());
        if approval["match"] == "package_info_only" {
            extra.insert("auto_accepted".into(), serde_json::json!(AUTO_ACCEPTED));
        }
        extra.insert(
            "program".into(),
            serde_json::json!(compared.program.as_ref().map(|p| p.display().to_string())),
        );
        extra.insert("exe_pin".into(), serde_json::json!(exe_pin));
        if shell == Some(process::Shell::PowerShell) {
            extra.insert(
                "script".into(),
                serde_json::json!(process::powershell_script(process::shell_args_text(&cmd))),
            );
        }

        // Every refusal is recorded as data and none aborts the batch, so
        // `refresh_index`/`verify_ran` still run afterward and a later check
        // in the same run still gets its chance.
        if let Some(reason) = &refusal {
            failures += 1;
            let code = evidence::record_control_plane_command(
                &conn,
                &ctx,
                label_prefix,
                &cmd,
                &dir.display().to_string(),
                None,
                None,
                "",
                "",
                &[],
                Some(reason),
                false,
                at_commit.as_deref(),
                &extra,
            )?;
            results.push(serde_json::json!({
                "index": i + 1, "check_id": id, "shell": shell_name, "cmd": cmd,
                "cwd": dir.display().to_string(),
                "exit_code": None::<i64>, "evidence": code, "error": reason,
                "exe_pin": exe_pin, "approval": approval,
                "package_info_lines": package_info_lines,
            }));
            refused.push(true);
            continue;
        }
        refused.push(false);
        let shell = shell.expect("a check with an unknown shell was refused above");

        // Fix round 1 / I1: a spawn failure is also data, not a `?` that
        // tears down the rest of the batch. A check that got this far always
        // has its fingerprint's program (`refusal_for`).
        let program = compared.program.clone().unwrap_or_default();
        let ran = process::run_shell(shell, &program, &cmd, &dir, timeout);
        let out = match ran {
            Ok(out) => out,
            Err(e) => {
                failures += 1;
                let reason = format!("failed to run: {e:#}");
                let argv = process::shell_argv(shell, &program, &cmd);
                let code = evidence::record_control_plane_command(
                    &conn,
                    &ctx,
                    label_prefix,
                    &cmd,
                    &dir.display().to_string(),
                    None,
                    None,
                    "",
                    "",
                    &argv,
                    Some(&reason),
                    false,
                    at_commit.as_deref(),
                    &extra,
                )?;
                results.push(serde_json::json!({
                    "index": i + 1, "check_id": id, "shell": shell_name, "cmd": cmd,
                    "cwd": dir.display().to_string(),
                    "exit_code": None::<i64>, "evidence": code, "error": reason,
                    "exe_pin": exe_pin, "approval": approval,
                    "package_info_lines": package_info_lines,
                }));
                continue;
            }
        };
        // Fix round 2 / N1.4: an incomplete-output check counts as a failure
        // even when the exit code was 0 - a command that leaves a descendant
        // behind holding its pipes did not complete cleanly.
        let is_failure = out.exit_code != Some(0) || out.output_incomplete;
        if is_failure {
            failures += 1;
        }
        // Fix round 1 / I2, extended by fix round 2 / N1.4 and R3: a
        // timed-out or incomplete-output check is `exit_code: null` (or,
        // per `ShellOutput::exit_code`'s doc, occasionally so off Windows for
        // a signal-killed child) and needs its `error` populated the same
        // way a refusal's is. All three reasons can combine (timeout and
        // incomplete output commonly do), so this builds up to a joined
        // message rather than picking one.
        let mut error_parts: Vec<String> = Vec::new();
        if out.timed_out {
            error_parts.push(format!("timed out after {timeout_secs}s"));
        }
        if out.output_incomplete {
            error_parts.push(format!(
                "output may be incomplete: a descendant process still held the output pipe \
                 {}s after the command exited",
                process::DRAIN_GRACE_SECS
            ));
        }
        if error_parts.is_empty() && out.exit_code.is_none() {
            error_parts.push("terminated without an exit code".to_string());
        }
        let error = if error_parts.is_empty() {
            None
        } else {
            Some(error_parts.join("; "))
        };
        let code = evidence::record_control_plane_command(
            &conn,
            &ctx,
            label_prefix,
            &cmd,
            &dir.display().to_string(),
            out.exit_code,
            Some(out.duration_ms),
            &out.stdout,
            &out.stderr,
            &out.argv,
            error.as_deref(),
            out.output_truncated,
            at_commit.as_deref(),
            &extra,
        )?;
        results.push(serde_json::json!({
            "index": i + 1,
            "check_id": id,
            "shell": shell_name,
            "cmd": cmd,
            "cwd": dir.display().to_string(),
            "exit_code": out.exit_code,
            "duration_ms": out.duration_ms,
            "evidence": code,
            "stdout_bytes": out.stdout.len(),
            "stderr_bytes": out.stderr.len(),
            // Whole-branch review Important 4: not a failure and not an
            // `error` - the exit code is still the command's own - but the
            // stored output is a prefix, and a reader comparing
            // `stdout_bytes` against what they expected needs to know.
            "output_truncated": out.output_truncated,
            "error": error,
            "exe_pin": exe_pin,
            "approval": approval,
            "package_info_lines": package_info_lines,
        }));
    }

    let (head_after, clean_after) = if dry_run { (None, false) } else { tree_state(&worktree) };
    let ran = !dry_run;
    let head_keys = serde_json::json!({
        "head_commit": head_before,
        "head_commit_after": head_after,
        "worktree_clean": ran.then(|| clean_before && clean_after && head_before.is_some() && head_before == head_after),
    });
    if !dry_run {
        // The new rows belong in the index the worker and reviewer read.
        if let Err(e) = evidence::refresh_index(&conn, &run.task_id, &run_worktree) {
            masked_eprintln!("thoth: warning: could not republish the evidence index: {e:#}");
        }
        let mut detail = serde_json::json!({
            "source": source,
            "count": results.len(),
            "failures": failures,
            "results": results,
            "warnings": warnings,
            "checks": check_ids,
            "worker_declared": worker_declared,
            "approved_checks": approved_checks,
            "previous_verify": previous_verify.as_object().map(|_| serde_json::json!({
                "run_id": previous_verify["run_id"], "task_id": previous_verify["task_id"],
                "ts": previous_verify["ts"], "seq": previous_verify["seq"],
            })),
            "missing_since_last_verify": missing_since_last_verify,
        });
        if let Some(c) = &at_commit {
            detail["at_commit"] = serde_json::json!(c);
        }
        for (k, v) in head_keys.as_object().into_iter().flatten() {
            detail[k] = v.clone();
        }
        state::append_event(
            &conn,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::VERIFY_RAN.to_string(),
                detail,
            },
        )?;
    }

    let human = {
        let mut lines = vec![format!(
            "{} check(s) from {}{}{}",
            check_ids.len(),
            source,
            match &at_commit {
                Some(c) => format!(" at base commit {c} in {}", worktree.display()),
                None => String::new(),
            },
            if dry_run { " (dry run)" } else { "" }
        )];
        lines.extend(previous_line);
        for w in &warnings {
            lines.push(format!("  warning: {w}"));
        }
        for (r, was_refused) in results.iter().zip(&refused) {
            // Fix round 2 / R2: a `None` exit code can mean several different
            // things (a refusal, a spawn failure, a timeout, or - off Windows
            // - a signal). Refusals are tracked above; each other `error`
            // string's wording is what this file itself chose when building
            // it, so the prefixes below are not a guess at someone else's text.
            let exit_disp = match r["exit_code"].as_i64() {
                Some(c) => c.to_string(),
                None if *was_refused => "REFUSED".to_string(),
                None if dry_run => "-".to_string(),
                None => {
                    let err = r["error"].as_str().unwrap_or("");
                    if err.starts_with("failed to run:") {
                        "ERROR"
                    } else if err.starts_with("timed out") {
                        "TIMEOUT"
                    } else if err.starts_with("output may be incomplete") {
                        "INCOMPLETE"
                    } else if err.starts_with("terminated without") {
                        "SIGNALLED"
                    } else {
                        "?"
                    }
                    .to_string()
                }
            };
            lines.push(format!(
                "  {:>2}  exit={:<8} {:<8} {}: {}{}{}",
                r["index"].as_i64().unwrap_or(0),
                exit_disp,
                r["evidence"].as_str().unwrap_or("-"),
                r["check_id"].as_str().unwrap_or(""),
                r["cmd"].as_str().unwrap_or(""),
                if r["exe_pin"] == serde_json::json!("path") { "  [exe pinned by path only]" } else { "" },
                if r["output_truncated"] == serde_json::json!(true) {
                    "  [output truncated]"
                } else {
                    ""
                }
            ));
            if *was_refused {
                if let Some(reason) = r["error"].as_str().or(r["reason"].as_str()) {
                    lines.push(format!("        {reason}"));
                }
            }
            if r["approval"]["match"] == "package_info_only" {
                lines.push(format!(
                    "        auto-accepted ({AUTO_ACCEPTED}): {}",
                    package_lines_text(&r["package_info_lines"])
                ));
            }
        }
        push_worker_declared(&mut lines, &worker_declared);
        if failures > 0 {
            lines.push(format!(
                "{failures} of {} check(s) exited non-zero or were refused; the task's status \
                 is unchanged - that call is yours",
                results.len()
            ));
        }
        lines.join("\n")
    };
    let mut doc = serde_json::json!({
        "ok": failures == 0,
        "run_id": run_id,
        "task_id": run.task_id,
        "source": source,
        "dry_run": dry_run,
        "failures": failures,
        "results": results,
        "warnings": warnings,
        "worker_declared": worker_declared,
        "approved_checks": approved_checks,
        "previous_verify": previous_verify,
        "missing_since_last_verify": missing_since_last_verify,
    });
    if let Some(c) = &at_commit {
        doc["at_commit"] = serde_json::json!(c);
    }
    for (k, v) in head_keys.as_object().into_iter().flatten() {
        doc[k] = v.clone();
    }
    output::emit(json, &doc, &human);
    // Non-zero when a check failed (or was refused/timed out), so a script
    // notices. The task's status is deliberately untouched (brief §10).
    Ok(if failures > 0 { 1 } else { 0 })
}

/// CK-1: the `previous_verify` object (null when there is no earlier verify;
/// its `approved_checks` is null for an event written before this field
/// existed) and the approved checks that earlier verify saw but this one does
/// not: `removed` (not in the catalogue) or `pending` (there, not approved).
fn compare_with_previous(
    conn: &Connection,
    approved_now: &[String],
    previous: Option<&state::EventRow>,
) -> Result<(serde_json::Value, Vec<serde_json::Value>)> {
    let Some(prev) = previous else {
        return Ok((serde_json::Value::Null, Vec::new()));
    };
    let detail: serde_json::Value = serde_json::from_str(&prev.detail).unwrap_or_default();
    let before: Option<Vec<String>> = detail["approved_checks"]
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect());
    let mut missing = Vec::new();
    if let Some(before) = &before {
        let mut gone: Vec<&String> = before.iter().filter(|id| !approved_now.contains(id)).collect();
        gone.sort();
        gone.dedup();
        for id in gone {
            let now = match checks::get_check(conn, id)? {
                None => "removed",
                Some(_) => "pending",
            };
            missing.push(serde_json::json!({"check_id": id, "now": now}));
        }
    }
    let previous_verify = serde_json::json!({
        "run_id": prev.run_id, "task_id": prev.task_id, "ts": prev.ts, "seq": prev.seq,
        "approved_checks": before,
    });
    Ok((previous_verify, missing))
}

/// `ID (removed)` / `ID (pending re-approval)`, joined with `, `.
pub(crate) fn missing_items(missing: &[serde_json::Value]) -> String {
    missing
        .iter()
        .map(|m| {
            let id = m["check_id"].as_str().unwrap_or("");
            match m["now"].as_str() {
                Some("pending") => format!("{id} (pending re-approval)"),
                _ => format!("{id} (removed)"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// The one human line naming approved checks missing since the previous
/// verify, or `None` when there are none. `previous` carries `run_id` and `ts`.
pub(crate) fn missing_line(
    previous: &serde_json::Value,
    missing: &[serde_json::Value],
) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    Some(format!(
        "  approved checks missing since the previous verify (run {} at {}): {}",
        previous["run_id"].as_str().unwrap_or("-"),
        previous["ts"].as_str().unwrap_or("-"),
        missing_items(missing)
    ))
}

/// Why a check whose fingerprint differs from its approval only in package
/// information ran anyway (`auto_accepted` in the evidence detail).
pub(crate) const AUTO_ACCEPTED: &str = "only package info changed";

/// `PATH:LINE, PATH:LINE` of a `package_info_lines` array (already sorted by
/// path, then line), or `line endings only` when it is empty.
pub(crate) fn package_lines_text(lines: &serde_json::Value) -> String {
    let items: Vec<String> = lines
        .as_array()
        .into_iter()
        .flatten()
        .map(|l| format!("{}:{}", l["path"].as_str().unwrap_or(""), l["line"]))
        .collect();
    if items.is_empty() {
        "line endings only".to_string()
    } else {
        items.join(", ")
    }
}

/// The human output's reference-only list of what the worker declared.
fn push_worker_declared(lines: &mut Vec<String>, declared: &[serde_json::Value]) {
    lines.push("worker declared (reference only):".to_string());
    if declared.is_empty() {
        lines.push("  (none)".to_string());
    }
    for d in declared {
        lines.push(format!(
            "  {}  (cwd {})",
            d["cmd"].as_str().unwrap_or(""),
            d["cwd"].as_str().unwrap_or(".")
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::state::Run;
    use rusqlite::Connection;

    const TASK: &str = "01HZVERIFYTASKVERIFYTASK01";

    fn setup() -> (Connection, Run, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        state::insert_task(
            &conn,
            &state::Task {
                task_id: TASK.into(),
                title: "t".into(),
                spec: "{}".into(),
                risk: "normal".into(),
                status: "in_review".into(),
                base_commit: "abc".into(),
                created_at: "n".into(),
                updated_at: "n".into(),
            },
        )
        .unwrap();
        let run = Run {
            run_id: "01HZVERIFYRUNVERIFYRUN0001".into(),
            task_id: TASK.into(),
            attempt: 1,
            role: "implementer".into(),
            provider: "anthropic".into(),
            account: "claude-b".into(),
            model: "sonnet".into(),
            harness_session_id: None,
            config_dir: "C:/cfg".into(),
            allowed_tools: "Bash".into(),
            worktree: dir.path().display().to_string(),
            base_commit: "abc".into(),
            candidate_commit: Some("def".into()),
            spec: "{}".into(),
            status: "completed".into(),
            interrupted_reason: None,
            exit_code: Some(0),
            input_tokens: None,
            output_tokens: None,
            started_at: None,
            ended_at: None,
            runner_pid: None,
            worker_pid: None,
            heartbeat_at: None,
            cancel_requested: 0,
            timeout_seconds: 1800,
            run_dir: "C:/rd".into(),
            error_text: None,
            harness_bin: None,
        };
        state::insert_run(&conn, &run).unwrap();
        (conn, run, dir)
    }

    fn write_result(dir: &std::path::Path, body: &str) {
        std::fs::create_dir_all(dir.join(".agent")).unwrap();
        std::fs::write(dir.join(".agent").join("result.json"), body).unwrap();
    }

    /// Fix round 1 / I3 (c): no `result_ingested` event exists for this run,
    /// so the worktree's file is the only source and must still work. (This
    /// test previously asserted the file always wins; Open question 15 was
    /// reversed by I3 below, so it now only covers the no-event case.)
    #[test]
    fn requests_fall_back_to_the_worktree_file_when_there_is_no_ingested_event() {
        let (conn, run, dir) = setup();
        write_result(
            dir.path(),
            &format!(
                r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                     "verification_requests":[{{"cmd":"cargo test","cwd":"."}},
                                              {{"cmd":"echo hi"}}]}}"#
            ),
        );
        let (reqs, source, warning) = requests_for_run(&conn, &run).unwrap();
        assert_eq!(reqs.len(), 2);
        assert_eq!(reqs[0].cmd, "cargo test");
        assert_eq!(reqs[0].cwd, ".");
        assert_eq!(reqs[1].cwd, ".", "cwd defaults to the worktree root");
        assert!(source.contains("result.json"), "got {source}");
        assert!(warning.is_none(), "no event to disagree with");
    }

    /// Fix round 1 / I3: reverses Open question 15's file-first choice. The
    /// run-scoped `result_ingested` event wins whenever one exists; the
    /// worktree's copy - gone or invalid - is only a fallback.
    #[test]
    fn requests_prefer_the_result_ingested_event_over_the_worktree_file() {
        let (conn, run, dir) = setup();
        state::append_event(
            &conn,
            &state::NewEvent {
                task_id: Some(TASK.to_string()),
                run_id: Some(run.run_id.clone()),
                event_type: crate::model::event_type::RESULT_INGESTED.to_string(),
                detail: serde_json::json!({
                    "summary": "s",
                    "claim_count": 0,
                    "result_json": {
                        "task_id": TASK,
                        "status": "completed",
                        "summary": "s",
                        "verification_requests": [{"cmd": "cargo test --all", "cwd": "crates/core"}]
                    }
                }),
            },
        )
        .unwrap();

        // no file at all
        let (reqs, source, warning) = requests_for_run(&conn, &run).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].cmd, "cargo test --all");
        assert_eq!(reqs[0].cwd, "crates/core");
        assert!(source.contains("result_ingested"), "got {source}");
        assert!(warning.is_none());

        // an unreadable file falls back the same way, and an unreadable file
        // cannot be compared, so still no warning
        write_result(dir.path(), "{not json");
        let (reqs, source, warning) = requests_for_run(&conn, &run).unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(source.contains("result_ingested"), "got {source}");
        assert!(warning.is_none());
    }

    /// Fix round 1 / I3 (a): event and file agree -> the event is used and no
    /// warning is produced.
    #[test]
    fn requests_agreeing_between_event_and_file_produce_no_warning() {
        let (conn, run, dir) = setup();
        write_result(
            dir.path(),
            &format!(
                r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                     "verification_requests":[{{"cmd":"echo hi","cwd":"."}}]}}"#
            ),
        );
        state::append_event(
            &conn,
            &state::NewEvent {
                task_id: Some(TASK.to_string()),
                run_id: Some(run.run_id.clone()),
                event_type: crate::model::event_type::RESULT_INGESTED.to_string(),
                detail: serde_json::json!({
                    "summary": "s", "claim_count": 0,
                    "result_json": {
                        "task_id": TASK, "status": "completed", "summary": "s",
                        "verification_requests": [{"cmd": "echo hi", "cwd": "."}]
                    }
                }),
            },
        )
        .unwrap();

        let (reqs, source, warning) = requests_for_run(&conn, &run).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].cmd, "echo hi");
        assert!(source.contains("result_ingested"));
        assert!(warning.is_none(), "event and file agree");
    }

    /// Fix round 1 / I3 (b): the file is rewritten after ingest with a
    /// different command (simulating a later run sharing this attempt's
    /// worktree) -> the event's commands are what's returned, and a warning
    /// names both sources.
    #[test]
    fn requests_disagreeing_between_event_and_file_use_the_event_and_warn() {
        let (conn, run, dir) = setup();
        state::append_event(
            &conn,
            &state::NewEvent {
                task_id: Some(TASK.to_string()),
                run_id: Some(run.run_id.clone()),
                event_type: crate::model::event_type::RESULT_INGESTED.to_string(),
                detail: serde_json::json!({
                    "summary": "s", "claim_count": 0,
                    "result_json": {
                        "task_id": TASK, "status": "completed", "summary": "s",
                        "verification_requests": [{"cmd": "the event's command", "cwd": "."}]
                    }
                }),
            },
        )
        .unwrap();
        write_result(
            dir.path(),
            &format!(
                r#"{{"task_id":"{TASK}","status":"completed","summary":"s",
                     "verification_requests":[{{"cmd":"a different, unrelated command","cwd":"."}}]}}"#
            ),
        );

        let (reqs, source, warning) = requests_for_run(&conn, &run).unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].cmd, "the event's command", "the event must win");
        assert!(source.contains("result_ingested"));
        let warning = warning.expect("event and file disagree, so this must warn");
        assert!(warning.contains("result.json"), "got {warning}");
        assert!(warning.contains("result_ingested"), "got {warning}");
    }

    /// P7-1b: the declared list is reference only, so having none is an empty
    /// list plus a warning naming both places, not an error.
    #[test]
    fn no_document_anywhere_is_an_empty_list_and_a_warning_that_names_both_places() {
        let (conn, run, _dir) = setup();
        let (reqs, _source, warning) = requests_for_run(&conn, &run).unwrap();
        assert!(reqs.is_empty());
        let warning = warning.expect("must warn");
        assert!(warning.contains("result.json"), "got {warning}");
        assert!(warning.contains("result_ingested"), "got {warning}");
    }

    #[test]
    fn spec_check_ids_reads_the_task_spec_in_order_and_leniently() {
        assert_eq!(
            spec_check_ids(r#"{"goal":"g","checks":["b","a"]}"#),
            ["b".to_string(), "a".to_string()]
        );
        assert!(spec_check_ids(r#"{"goal":"g"}"#).is_empty());
        assert!(spec_check_ids("{not json").is_empty());
    }

    /// Fix round 1 / I1: a lexical check only, but it must catch every shape
    /// `Path::join` would otherwise mishandle.
    #[test]
    fn cwd_escapes_the_worktree_rejects_absolute_drive_and_parent_but_allows_relative() {
        // A drive letter only means "absolute" on Windows; on Linux `C:` is an
        // ordinary directory name, so `C:/Windows` is a relative path that
        // stays inside the worktree.
        #[cfg(windows)]
        assert!(cwd_escapes_the_worktree("C:/Windows"));
        #[cfg(not(windows))]
        assert!(!cwd_escapes_the_worktree("C:/Windows"));
        assert!(cwd_escapes_the_worktree("/etc"));
        assert!(cwd_escapes_the_worktree(".."));
        assert!(cwd_escapes_the_worktree("a/../../b"));
        assert!(!cwd_escapes_the_worktree("."));
        assert!(!cwd_escapes_the_worktree("crates/core"));
    }
}
