//! `thoth suite run | show` (VSCOPE-1).
//!
//! A task's verify runs only its own test targets (a scoped check) and the
//! cheap checks; the whole suite runs once, on the main line, after one or
//! more tasks are merged and before the main line is pushed. `suite run`
//! runs the approved checks in a temporary detached checkout of a commit -
//! the same kind `verify --at-base` uses, removed afterwards - and records a
//! `suite_ran` event (no task, no run). `suite show` says whether the main
//! worktree's HEAD has a green whole-suite run and lists the merges since
//! the last one.
//!
//! The approval rules are verify's: a check runs only once its fingerprint,
//! computed in the checkout, matches one of its approvals. A `suite_ran` is
//! not a verify of any run, so it never counts for REV-3.

use crate::checks::{self, Check};
use crate::commands::verify::{self, BaseWorktree, Compared, Store};
use crate::config::Paths;
use crate::db::state::{self, NewEvent};
use crate::model::event_type;
use crate::output;
use crate::process;
use crate::worktree;
use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::Path;

pub struct RunArgs<'a> {
    /// The commit to run on; `None` is the main worktree's HEAD.
    pub commit: Option<&'a str>,
    /// `--check`, in the order given; empty runs every unscoped check.
    pub checks: &'a [String],
    /// `--test`: only with exactly one `--check` that has a scope.
    pub tests: &'a [String],
    pub timeout_secs: u64,
    pub json: bool,
}

/// Whether `name` can be a test target name (and so a plain argument word).
fn valid_test_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The combined output stored as a row's blob: stdout, then stderr after a
/// separator line when there is any.
fn combined_output(stdout: &str, stderr: &str) -> Vec<u8> {
    let mut out = stdout.as_bytes().to_vec();
    if !stderr.is_empty() {
        if !out.is_empty() && !out.ends_with(b"\n") {
            out.push(b'\n');
        }
        out.extend_from_slice(b"--- stderr ---\n");
        out.extend_from_slice(stderr.as_bytes());
    }
    out
}

pub fn run(paths: &Paths, cwd: &Path, args: RunArgs<'_>) -> Result<i32> {
    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;

    // `--test` is refused before anything runs or is recorded.
    if !args.tests.is_empty() {
        let scoped = match args.checks {
            [one] => checks::get_check(&conn, one)?.is_some_and(|c| c.scope.is_some()),
            _ => false,
        };
        if !scoped {
            bail!(
                "--test needs exactly one --check, and that check must have a scope \
                 (checks add --scope {})",
                checks::SCOPE_CARGO_TEST_TARGETS
            );
        }
        if let Some(bad) = args.tests.iter().find(|t| !valid_test_name(t)) {
            bail!("--test '{bad}' is not a test target name (letters, digits, _ and - only)");
        }
    }

    let commit = match args.commit {
        Some(rev) => worktree::resolve_commit(&root, rev)?,
        None => worktree::rev_parse_head(&root)?,
    };
    let check_ids: Vec<String> = if args.checks.is_empty() {
        checks::list_checks(&conn)?
            .into_iter()
            .filter(|c| c.scope.is_none())
            .map(|c| c.check_id)
            .collect()
    } else {
        args.checks.to_vec()
    };
    if check_ids.is_empty() {
        bail!(
            "no checks to run: the catalogue has no check without a scope; add one with thoth checks add ... \
             then approve it with: thoth checks approve --reason \"WHY\""
        );
    }
    let catalogue: Vec<Option<Check>> =
        check_ids.iter().map(|id| checks::get_check(&conn, id)).collect::<Result<_>>()?;
    let full = args.checks.is_empty() && args.tests.is_empty();
    let appended: Vec<String> = args.tests.iter().flat_map(|t| ["--test".to_string(), t.clone()]).collect();

    // The temporary checkout, removed by the guard on every exit path.
    let path = paths
        .repo_dir(&repo_id)
        .join("baseline-worktrees")
        .join(format!("suite-{}", crate::ids::new_run_id()));
    worktree::prune_worktrees(&root);
    let ensured = worktree::ensure_detached_worktree(&root, &path, &commit)?;
    if ensured.outcome == worktree::EnsureOutcome::Reused {
        bail!("suite checkout {} already exists; refusing to reuse it", path.display());
    }
    let _guard = BaseWorktree { root: root.clone(), path: path.clone() };

    let timeout = std::time::Duration::from_secs(args.timeout_secs);
    let blobs_dir = paths.blobs_dir(&repo_id);
    let store = Store { conn: &conn, blobs_dir: &blobs_dir };
    let mut results: Vec<Value> = Vec::new();
    let mut failures = 0usize;
    for (i, (id, row)) in check_ids.iter().zip(&catalogue).enumerate() {
        let cmd = row.as_ref().map(|c| c.cmd.clone()).unwrap_or_default();
        let check_cwd = row.as_ref().map(|c| c.cwd.clone()).unwrap_or_else(|| ".".into());
        let shell_name = row.as_ref().map(|c| c.shell.clone());
        let shell = shell_name.as_deref().and_then(process::Shell::parse);
        let exe_pin = row.as_ref().map(|c| c.exe_pin.clone());
        let dir = path.join(&check_cwd);
        let run_cmd = verify::with_appended(&cmd, &appended);

        let mut compared = Compared::default();
        let refusal: Option<String> = match row {
            None => Some(format!("check {id} is not in this repository's check catalogue")),
            Some(c) => verify::refusal_for(&store, c, shell, &path, &dir, true, false, None, &mut compared)?,
        };
        if compared.approval.as_ref().is_some_and(|(_, kind)| *kind == "exact") {
            if let Some(c) = row {
                checks::store_package_copies(paths, &repo_id, c, &path);
            }
        }
        let mut r = json!({
            "index": i + 1, "check_id": id, "shell": shell_name, "cmd": cmd,
            "cwd": dir.display().to_string(), "exit_code": null, "duration_ms": null,
            "evidence": null, "stdout_bytes": null, "stderr_bytes": null,
            "output_truncated": false, "error": null, "exe_pin": exe_pin,
            "fingerprint_ok": compared.fingerprint_ok,
            "approval": compared.approval_json(), "package_info_lines": compared.package_lines_json(),
            "argv": [], "sha256": null, "blob_path": null,
        });
        if !appended.is_empty() {
            r["appended_args"] = json!(appended);
        }
        if let Some(reason) = refusal {
            failures += 1;
            r["error"] = json!(reason);
            results.push(r);
            continue;
        }
        let shell = shell.expect("a check with an unknown shell was refused above");
        let program = compared.program.clone().unwrap_or_default();
        let out = match process::run_shell(shell, &program, &run_cmd, &dir, timeout) {
            Ok(out) => out,
            Err(e) => {
                failures += 1;
                r["argv"] = json!(process::shell_argv(shell, &program, &run_cmd));
                r["error"] = json!(format!("failed to run: {e:#}"));
                results.push(r);
                continue;
            }
        };
        let mut error_parts: Vec<String> = Vec::new();
        if out.timed_out {
            error_parts.push(format!("timed out after {}s", args.timeout_secs));
        }
        if out.output_incomplete {
            error_parts.push(format!(
                "output may be incomplete: a descendant process still held the output pipe {}s after \
                 the command exited",
                process::DRAIN_GRACE_SECS
            ));
        }
        if error_parts.is_empty() && out.exit_code.is_none() {
            error_parts.push("terminated without an exit code".to_string());
        }
        let error = (!error_parts.is_empty()).then(|| error_parts.join("; "));
        if out.exit_code != Some(0) || error.is_some() {
            failures += 1;
        }
        let (sha, _, blob) = crate::blobs::put_bytes(paths, &repo_id, &combined_output(&out.stdout, &out.stderr))?;
        r["argv"] = json!(out.argv);
        r["exit_code"] = json!(out.exit_code);
        r["duration_ms"] = json!(out.duration_ms);
        r["stdout_bytes"] = json!(out.stdout.len());
        r["stderr_bytes"] = json!(out.stderr.len());
        r["output_truncated"] = json!(out.output_truncated);
        r["error"] = json!(error);
        r["sha256"] = json!(sha);
        r["blob_path"] = json!(blob.display().to_string());
        results.push(r);
    }

    let ok = failures == 0;
    let detail = json!({
        "commit": commit,
        "full": full,
        "checks": check_ids,
        "tests": args.tests,
        "failures": failures,
        "results": results,
        "ok": ok,
    });
    state::append_event(
        &conn,
        &NewEvent { task_id: None, run_id: None, event_type: event_type::SUITE_RAN.to_string(), detail: detail.clone() },
    )?;

    let mut human = vec![format!(
        "suite on {commit} ({}): {} check(s)",
        if full { "whole suite" } else { "partial: not a whole-suite run" },
        check_ids.len()
    )];
    for r in &results {
        let exit = match r["exit_code"].as_i64() {
            Some(c) => c.to_string(),
            None => "-".to_string(),
        };
        human.push(format!(
            "  {:>2}  exit={:<4} {}: {}{}",
            r["index"].as_i64().unwrap_or(0),
            exit,
            r["check_id"].as_str().unwrap_or(""),
            r["cmd"].as_str().unwrap_or(""),
            if appended.is_empty() { String::new() } else { format!(" {}", appended.join(" ")) }
        ));
        if let Some(e) = r["error"].as_str() {
            human.push(format!("        {e}"));
        }
        if let Some(b) = r["blob_path"].as_str() {
            human.push(format!("        output: {b}"));
        }
    }
    human.push(if ok {
        "green".to_string()
    } else {
        format!("{failures} of {} check(s) failed or were refused: not green", results.len())
    });
    output::emit(args.json, &detail, &human.join("\n"));
    Ok(if ok { 0 } else { 1 })
}

pub fn show(paths: &Paths, cwd: &Path, json: bool) -> Result<i32> {
    let (_repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let head = worktree::rev_parse_head(&root)?;
    let mut stmt = conn.prepare("SELECT ts, detail FROM events WHERE type = ?1 ORDER BY seq DESC")?;
    let rows: Vec<(String, Value)> = stmt
        .query_map([event_type::SUITE_RAN], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .map(|(ts, d)| (ts, serde_json::from_str(&d).unwrap_or(Value::Null)))
        .collect();
    let last: Value = rows
        .first()
        .map(|(ts, d)| {
            let mut v = d.clone();
            if v.is_object() {
                v["ts"] = json!(ts);
            }
            v
        })
        .unwrap_or(Value::Null);
    let last_green: Option<String> = rows
        .iter()
        .find(|(_, d)| d["full"] == json!(true) && d["ok"] == json!(true))
        .and_then(|(_, d)| d["commit"].as_str().map(String::from));
    let head_green = last_green.as_deref() == Some(head.as_str());
    let merges: Option<Vec<(String, String)>> = match &last_green {
        Some(g) if matches!(worktree::is_ancestor(&root, g, &head), Ok(true)) => {
            Some(worktree::first_parent_merges(&root, g, &head)?)
        }
        _ => None,
    };
    let doc = json!({
        "head_commit": head,
        "last": last,
        "last_green_commit": last_green,
        "head_green": head_green,
        "merges_since_green": merges.as_ref().map(|m| m.iter()
            .map(|(c, s)| json!({"commit": c, "subject": s})).collect::<Vec<_>>()),
    });
    let mut human = vec![if head_green {
        format!("HEAD {head} is green: the last whole-suite run on it passed")
    } else {
        format!("HEAD {head} is not green: no passing whole-suite run on it")
    }];
    human.push(format!("last green whole suite: {}", last_green.as_deref().unwrap_or("(none)")));
    if !last.is_null() {
        human.push(format!(
            "last suite run: {} on {} ({}, {})",
            last["ts"].as_str().unwrap_or("-"),
            last["commit"].as_str().unwrap_or("-"),
            if last["full"] == json!(true) { "whole suite" } else { "partial" },
            if last["ok"] == json!(true) { "passed" } else { "failed" }
        ));
    }
    match &merges {
        None if last_green.is_some() => {
            human.push("merges since green: unknown (the last green commit is not an ancestor of HEAD)".to_string())
        }
        None => {}
        Some(m) if m.is_empty() => human.push("merges since green: (none)".to_string()),
        Some(m) => {
            human.push("merges since green (oldest first):".to_string());
            for (c, s) in m {
                human.push(format!("  {c} {s}"));
            }
        }
    }
    output::emit(json, &doc, &human.join("\n"));
    Ok(0)
}
