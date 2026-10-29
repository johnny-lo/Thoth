//! RFIX-1, "the reviewer fixes, another model confirms": what the runner does
//! with an edit-mode review's `edits` and with a confirmer's answer, and
//! where a task's fix stands (`review show`, `wait --run`, `review confirm`).
//!
//! An edit-mode REJECT with edits is applied by Thoth in the fix worktree
//! (`worktrees/TASK-aN-fix`), held to `review_fix_edits`' bounds, and committed on
//! `thoth/TASK/aN-fix`. The REJECT is recorded as a review row without a
//! `verdict` event, so the task stays `in_review` until the confirmer
//! answers: one `verdict` per edit-then-confirm chain, never two in one
//! attempt. A refused fix is a plain REJECT.

use crate::commands::review::{self, ReviewIngest};
use crate::config::Paths;
use crate::db;
use crate::db::state::{self, NewEvent, Run};
use crate::evidence;
use crate::review_fix_edits::{self, stage, Bound, Refusal};
use crate::ingest::IngestOutcome;
use crate::model::{event_type, role, TaskStatus};
use crate::redact::{Counts, Masker};
use crate::review_json::{self, ExtraFinding, Findings, Verdict};
use crate::spec_edits::{self, ReviewMode};
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// `verdict.detail.source` and `review show`'s `source` of a confirmer's
/// verdict.
pub const SOURCE_AUTHOR_CHECK: &str = "author_check";
/// `review show`'s `mode` of a `review record`.
pub const MODE_EXTERNAL: &str = "external";

/// `worktrees/TASK-aN-fix`: one per attempt, detached.
pub fn fix_worktree_path(paths: &Paths, repo_id: &str, task_id: &str, attempt: i64) -> PathBuf {
    paths
        .worktrees_dir(repo_id)
        .join(format!("{task_id}-a{attempt}-fix"))
}

/// `thoth/TASK/aN-fix`, the branch a confirmed fix is merged from.
pub fn fix_branch(task_id: &str, attempt: i64) -> String {
    format!("{}-fix", crate::worktree::branch_name(task_id, attempt))
}

/// A reviewer run's newest `review_started` detail.
pub fn started_detail(conn: &Connection, run_id: &str) -> Result<Option<Value>> {
    let detail: Option<String> = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            rusqlite::params![run_id, event_type::REVIEW_STARTED],
            |r| r.get(0),
        )
        .optional()?;
    Ok(detail.and_then(|d| serde_json::from_str(&d).ok()))
}

/// The seq of a reviewer run's newest `review_started` event.
fn started_seq(conn: &Connection, run_id: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT seq FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            rusqlite::params![run_id, event_type::REVIEW_STARTED],
            |r| r.get(0),
        )
        .optional()?)
}

/// The mode a `review_started` detail records; `review` for one written
/// before RFIX-1.
pub fn mode_of(started: Option<&Value>) -> ReviewMode {
    started
        .and_then(|d| d["mode"].as_str())
        .and_then(ReviewMode::parse)
        .unwrap_or(ReviewMode::Review)
}

/// A reviewer run's mode.
pub fn run_mode(conn: &Connection, run_id: &str) -> Result<ReviewMode> {
    Ok(mode_of(started_detail(conn, run_id)?.as_ref()))
}

/// The `mode` `review show` reports for one review row.
fn row_mode(conn: &Connection, r: &state::Review) -> Result<String> {
    Ok(match &r.reviewer_run_id {
        None => MODE_EXTERNAL.to_string(),
        Some(run) => run_mode(conn, run)?.as_str().to_string(),
    })
}

/// The `review_fix_applied` / `review_fix_refused` detail naming `review_id`.
fn fix_event(conn: &Connection, task_id: &str, review_id: &str) -> Result<Option<(String, Value)>> {
    for e in state::events_since(conn, 0, Some(task_id))?.into_iter().rev() {
        if e.event_type != event_type::REVIEW_FIX_APPLIED && e.event_type != event_type::REVIEW_FIX_REFUSED {
            continue;
        }
        let d: Value = serde_json::from_str(&e.detail).unwrap_or(Value::Null);
        if d["review_id"].as_str() == Some(review_id) {
            return Ok(Some((e.event_type, d)));
        }
    }
    Ok(None)
}

/// `fix_verify.ok`: at least one check row, no failure, every row exit 0
/// with no `error` (output collected in full).
pub fn verify_ok(detail: &Value) -> bool {
    let rows = detail["results"].as_array();
    rows.is_some_and(|r| !r.is_empty())
        && detail["failures"].as_u64() == Some(0)
        && rows.into_iter().flatten().all(|r| r["exit_code"].as_i64() == Some(0) && r["error"].is_null())
}

/// A run's newest `verify_ran`: (seq, detail).
pub fn latest_verify(conn: &Connection, run_id: &str) -> Result<Option<(i64, Value)>> {
    let row: Option<(i64, String)> = conn
        .query_row(
            "SELECT seq, detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            rusqlite::params![run_id, event_type::VERIFY_RAN],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(seq, d)| (seq, serde_json::from_str(&d).unwrap_or(Value::Null))))
}

/// The `fix_verify` object of a confirm run.
pub fn fix_verify_for(seq: i64, detail: &Value) -> Value {
    json!({"seq": seq, "ok": verify_ok(detail), "failures": detail["failures"].as_u64().unwrap_or(0)})
}

/// Where a task's newest review and its fix stand.
pub struct State {
    pub reviews: Vec<state::Review>,
    /// The newest review row's mode, `None` without one.
    pub mode: Option<String>,
    /// The newest review row is an applied fix, the task is `in_review`, and
    /// the newest completed implementer run is still the one it judged.
    pub pending_fix: bool,
    /// The newest applied edit review and its `review_fix_applied` detail,
    /// when it is the newest review row.
    pub applied: Option<(state::Review, Value)>,
    /// `review show`'s `fix`: the newest edit-mode review, or null.
    pub fix: Value,
}

pub fn state(conn: &Connection, task: &state::Task) -> Result<State> {
    let reviews = state::list_reviews_for_task(conn, &task.task_id)?;
    let mut modes = Vec::with_capacity(reviews.len());
    for r in &reviews {
        modes.push(row_mode(conn, r)?);
    }
    let mode = modes.last().cloned();
    let newest = reviews.last();
    let applied = match newest {
        Some(r) if mode.as_deref() == Some(ReviewMode::Edit.as_str()) => match fix_event(conn, &task.task_id, &r.review_id)? {
            Some((ty, d)) if ty == event_type::REVIEW_FIX_APPLIED => Some((r.clone(), d)),
            _ => None,
        },
        _ => None,
    };
    let latest_impl = state::latest_completed_run_for_role(conn, &task.task_id, role::IMPLEMENTER)?;
    let pending_fix = applied.as_ref().is_some_and(|(r, _)| {
        task.status == TaskStatus::InReview.as_str()
            && latest_impl.as_ref().map(|t| t.run_id.as_str()) == Some(r.target_run_id.as_str())
    });
    let edit_row = reviews
        .iter()
        .zip(&modes)
        .rev()
        .find(|(_, m)| m.as_str() == ReviewMode::Edit.as_str())
        .map(|(r, _)| r.clone());
    let fix = match &edit_row {
        None => Value::Null,
        Some(r) => fix_json(conn, &task.task_id, r)?,
    };
    Ok(State { reviews, mode, pending_fix, applied, fix })
}

fn fix_json(conn: &Connection, task_id: &str, r: &state::Review) -> Result<Value> {
    let event = fix_event(conn, task_id, &r.review_id)?;
    let mut v = json!({
        "review_id": r.review_id, "applied": false, "stage": null, "reason": null,
        "fix_commit": null, "fix_branch": null, "files": null, "changed_lines": null,
        "file_count": null, "edits": null, "diff_code": null, "verify": null,
    });
    match event {
        Some((ty, d)) if ty == event_type::REVIEW_FIX_APPLIED => {
            v["applied"] = json!(true);
            for k in ["fix_commit", "fix_branch", "files", "changed_lines", "file_count", "edits", "diff_code"] {
                v[k] = d[k].clone();
            }
            if let Some(run) = &r.reviewer_run_id {
                if let Some((seq, vd)) = latest_verify(conn, run)? {
                    v["verify"] = json!({"seq": seq, "ok": verify_ok(&vd), "head_commit": vd["head_commit"]});
                }
            }
        }
        Some((_, d)) => {
            for k in ["stage", "reason", "files", "changed_lines", "file_count"] {
                if let Some(x) = d.get(k) {
                    v[k] = x.clone();
                }
            }
        }
        None => {}
    }
    Ok(v)
}

/// Why `review confirm` cannot run: there is no pending fix.
pub fn no_pending_fix(task_id: &str, st: &State, conn: &Connection) -> Result<String> {
    let any_edit = !st.fix.is_null();
    Ok(if !any_edit {
        format!(
            "task {task_id} has no edit review; run `thoth review start {task_id} --edit ...` first, then confirm its fix"
        )
    } else if st.applied.is_none() {
        format!(
            "task {task_id}'s newest review is not an edit review with an applied fix (mode {}); run `thoth review start {task_id} --edit ...` again",
            st.mode.as_deref().unwrap_or("none")
        )
    } else {
        let latest = state::latest_completed_run_for_role(conn, task_id, role::IMPLEMENTER)?
            .map(|r| r.run_id)
            .unwrap_or_default();
        format!(
            "task {task_id} has a new implementer run {latest} since the fix was applied; review the new candidate instead"
        )
    })
}

/// The fix's verify, as `review confirm` requires it: the edit run's newest
/// `verify_ran` ran on FIX itself (not `--at-base`), FIX before and after,
/// and the worktree stayed clean. `Ok((seq, detail))` or the refusal.
pub fn usable_verify(conn: &Connection, edit_run: &str, fix_commit: &str) -> Result<std::result::Result<(i64, Value), String>> {
    let hint = format!("run `thoth verify {edit_run}` on the fix first (it runs in the fix worktree)");
    let Some((seq, d)) = latest_verify(conn, edit_run)? else {
        return Ok(Err(format!("the fix has not been verified; {hint}")));
    };
    if d.get("at_commit").is_some_and(|c| !c.is_null()) {
        return Ok(Err(format!("the newest verify of run {edit_run} is a --at-base run, which proves nothing about the fix; {hint}")));
    }
    if d["head_commit"].as_str() != Some(fix_commit) || d["head_commit_after"].as_str() != Some(fix_commit) {
        return Ok(Err(format!(
            "the newest verify of run {edit_run} did not run on the fix commit {fix_commit} (head {} -> {}); {hint}",
            d["head_commit"].as_str().unwrap_or("none"),
            d["head_commit_after"].as_str().unwrap_or("none")
        )));
    }
    if d["worktree_clean"] != json!(true) {
        return Ok(Err(format!(
            "the newest verify of run {edit_run} changed the fix worktree (worktree_clean is not true); put it back and {hint}"
        )));
    }
    Ok(Ok((seq, d)))
}

/// The fix worktree as a confirm needs it: HEAD and the fix branch at FIX,
/// no project file changed (ignored files, such as build output, are fine).
pub fn fix_worktree_ready(root: &Path, wt: &Path, branch: &str, fix_commit: &str) -> std::result::Result<(), String> {
    if !wt.join(".git").exists() {
        return Err(format!("the fix worktree {} is gone", wt.display()));
    }
    match crate::worktree::rev_parse_head(wt) {
        Ok(h) if h == fix_commit => {}
        Ok(h) => return Err(format!("the fix worktree's HEAD is {h}, not the fix commit {fix_commit}")),
        Err(e) => return Err(format!("{e:#}")),
    }
    match review_fix_edits::branch_sha(root, branch) {
        Ok(Some(s)) if s == fix_commit => {}
        Ok(other) => {
            return Err(format!(
                "branch {branch} points at {}, not the fix commit {fix_commit}",
                other.as_deref().unwrap_or("nothing")
            ))
        }
        Err(e) => return Err(format!("{e:#}")),
    }
    match review_fix_edits::project_changes(wt, false) {
        Ok(c) if c.is_empty() => Ok(()),
        Ok(c) => Err(format!("the fix worktree has changes outside .agent/: {}", c.join(", "))),
        Err(e) => Err(format!("{e:#}")),
    }
}

/// The confirmer's four input files, masked.
pub fn write_confirm_input(
    root: &Path,
    task: &state::Task,
    wt: &Path,
    review_row: &state::Review,
    applied: &Value,
    masker: &Masker,
) -> Result<Counts> {
    let dir = wt.join(review_fix_edits::INPUT_DIR);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clearing {}", dir.display())),
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut counts = Counts::default();
    let mut write_value = |name: &str, mut v: Value| -> Result<()> {
        counts.merge(&masker.mask_value(&mut v));
        std::fs::write(dir.join(name), serde_json::to_string_pretty(&v)?)
            .with_context(|| format!("writing {}", dir.join(name).display()))
    };
    let spec: Value = serde_json::from_str(&task.spec).unwrap_or(Value::String(task.spec.clone()));
    write_value(review_fix_edits::INPUT_SPEC, spec)?;
    write_value(
        review_fix_edits::INPUT_FINDINGS,
        serde_json::from_str(&review_row.findings).unwrap_or(Value::Null),
    )?;
    write_value(review_fix_edits::INPUT_EDITS, applied["edits"].clone())?;
    let base = applied["base_commit"].as_str().unwrap_or_default();
    let fix = applied["fix_commit"].as_str().unwrap_or_default();
    let patch = crate::worktree::diff_patch(root, base, fix)?;
    let (masked, c) = masker.mask_text(&String::from_utf8_lossy(&patch));
    counts.merge(&c);
    std::fs::write(dir.join(review_fix_edits::INPUT_PATCH), masked.as_bytes())
        .with_context(|| format!("writing {}", dir.join(review_fix_edits::INPUT_PATCH).display()))?;
    Ok(counts)
}

/// The runner's check right before it launches a confirmer: the fix it was
/// started for is still pending and the fix worktree still at it; then the
/// confirmer's input, masked. Returns the redaction counts and the fix
/// commit.
pub fn prepare_confirm_launch(conn: &Connection, root: &Path, run: &Run, masker: &Masker) -> Result<(Counts, String)> {
    let started = started_detail(conn, &run.run_id)?.unwrap_or(Value::Null);
    let confirms = started["confirms_review_id"].as_str().unwrap_or_default();
    let task = state::get_task(conn, &run.task_id)?.ok_or_else(|| anyhow::anyhow!("task {} disappeared", run.task_id))?;
    let st = state(conn, &task)?;
    let Some((row, applied)) = st.applied.as_ref().filter(|(r, _)| st.pending_fix && r.review_id == confirms) else {
        bail!(
            "task {}'s pending fix is no longer review {confirms} between review confirm and the launch; start the confirm again",
            run.task_id
        );
    };
    let fix_commit = applied["fix_commit"].as_str().unwrap_or_default().to_string();
    let branch = applied["fix_branch"].as_str().unwrap_or_default();
    if let Err(why) = fix_worktree_ready(root, Path::new(&run.worktree), branch, &fix_commit) {
        bail!("refusing to launch the confirmer of task {}: {why}", run.task_id);
    }
    let counts = write_confirm_input(root, &task, Path::new(&run.worktree), row, applied, masker)?;
    Ok((counts, fix_commit))
}

/// Everything the runner's ingest of a fix needs besides the run.
pub struct Ctx<'a> {
    pub paths: &'a Paths,
    pub repo_id: &'a str,
    pub root: &'a Path,
}

/// The runner's ingest of an edit-mode review. Validated exactly as a review
/// is; then `edits` must be an array, empty unless the verdict is REJECT. A
/// REJECT with edits gets its fix applied or refused.
pub fn ingest_edit(
    conn: &Connection,
    ctx: &Ctx<'_>,
    run: &Run,
    task: &state::Task,
    target_run_id: &str,
    json_text: &str,
) -> ReviewIngest {
    let (doc, cited) = match review::validate_review_document(conn, &run.task_id, task, target_run_id, json_text) {
        Ok(v) => v,
        Err(e) => return review::review_invalid(conn, run, e.reason, e.detail),
    };
    if target_run_id.is_empty() {
        return review::review_invalid(
            conn,
            run,
            "the review's target run could not be determined".to_string(),
            json!({"stage": "target"}),
        );
    }
    let raw: Value = serde_json::from_str(review::strip_bom(json_text)).unwrap_or(Value::Null);
    let items = match review_fix_edits::edit_list(&raw) {
        Ok(i) => i,
        Err(e) => return review::review_invalid(conn, run, e, json!({"stage": "schema"})),
    };
    let verdict = Verdict::parse(&doc.verdict).unwrap_or(Verdict::Unverified);
    if verdict != Verdict::Reject && !items.is_empty() {
        return review::review_invalid(
            conn,
            run,
            format!("review.json says {} but attaches {} edit(s); a fix is attached only to a REJECT", verdict.as_str(), items.len()),
            json!({"stage": "schema"}),
        );
    }
    if items.is_empty() {
        return write_plain(conn, run, target_run_id, &doc, &cited, verdict, json!({"mode": "edit", "fix": null}), None);
    }

    let started = started_detail(conn, &run.run_id).ok().flatten().unwrap_or(Value::Null);
    let candidate = started["candidate_commit"].as_str().unwrap_or_default().to_string();
    let branch = started["fix_branch"].as_str().map(str::to_string).unwrap_or_else(|| fix_branch(&run.task_id, run.attempt));
    let wt = PathBuf::from(&run.worktree);
    let branch_before = review_fix_edits::branch_sha(ctx.root, &branch).ok().flatten();
    let review_id = crate::ids::new_review_id();

    let attempt = try_fix(conn, ctx, run, task, &wt, &candidate, &branch, &items, &doc.findings);
    let refusal = match attempt {
        Ok(fix) => match write_applied(conn, ctx, run, target_run_id, &doc, &review_id, &candidate, &branch, &wt, branch_before.as_deref(), &fix) {
            Ok(()) => {
                // A review writes no `result_ingested`, so its run note keeps
                // this text.
                let note = format!(
                    "REJECT with a fix of {} edit(s) applied on {branch} ({}); awaiting verify and review confirm",
                    fix.bound.len(),
                    crate::evidence::short_sha(&fix.fix_commit)
                );
                return ReviewIngest {
                    outcome: IngestOutcome::Ok { summary: note.clone(), claim_count: 0, brief: note },
                    verdict: None,
                    unopened_citations: Vec::new(),
                    settled: false,
                    candidate_commit: Some(fix.fix_commit.clone()),
                }
            }
            Err(e) => (Refusal::new(stage::WRITE, format!("{e:#}")), Some(fix.diff.clone())),
        },
        Err(r) => r,
    };
    let (refusal, diff) = refusal;
    // Nothing of the fix stays: the worktree back at the candidate, the
    // branch where it was.
    if let Err(e) = review_fix_edits::reset_worktree(&wt, &candidate) {
        crate::runner::log_runner_error(Path::new(&run.run_dir), &format!("resetting the fix worktree failed: {e:#}"));
    }
    if review_fix_edits::branch_sha(ctx.root, &branch).ok().flatten() != branch_before {
        if let Err(e) = review_fix_edits::set_branch(ctx.root, &branch, branch_before.as_deref()) {
            crate::runner::log_runner_error(Path::new(&run.run_dir), &format!("restoring branch {branch} failed: {e:#}"));
        }
    }
    let mut refused = json!({
        "review_id": review_id,
        "stage": refusal.stage,
        "reason": refusal.reason,
        "edit_count": items.len(),
    });
    if let Some(d) = &diff {
        refused["files"] = d.files_json();
        refused["changed_lines"] = json!(d.changed_lines);
        refused["file_count"] = json!(d.file_count);
    }
    let mut ingested = write_plain(
        conn,
        run,
        target_run_id,
        &doc,
        &cited,
        verdict,
        json!({"mode": "edit", "fix": {"applied": false, "stage": refusal.stage}}),
        Some((review_id, refused)),
    );
    ingested.candidate_commit = Some(candidate);
    ingested
}

/// A review row and its `verdict`, as `ingest_review` writes them, plus
/// `extra` keys on the verdict and, for a refused fix, the
/// `review_fix_refused` event before them in the same transaction.
#[allow(clippy::too_many_arguments)]
fn write_plain(
    conn: &Connection,
    run: &Run,
    target_run_id: &str,
    doc: &review_json::ReviewDocument,
    cited: &[String],
    verdict: Verdict,
    extra: Value,
    refused: Option<(String, Value)>,
) -> ReviewIngest {
    let opened = review::opened_codes(conn, &run.run_id).unwrap_or_default();
    let unopened: Vec<String> = cited.iter().filter(|c| !opened.contains(c)).cloned().collect();
    let mut row = review::new_review_row(target_run_id.to_string(), Some(run.run_id.clone()), verdict, &doc.findings);
    if let Some((id, _)) = &refused {
        row.review_id = id.clone();
    }
    let mut detail = review::verdict_event_detail(review::VerdictEventShape {
        verdict,
        review_id: &row.review_id,
        target_run_id,
        doc,
        cited,
        opened: Some(&opened),
        unopened: Some(&unopened),
        source: None,
    });
    if let (Some(d), Some(x)) = (detail.as_object_mut(), extra.as_object()) {
        for (k, v) in x {
            d.insert(k.clone(), v.clone());
        }
    }
    let written = (|| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        if let Some((_, r)) = &refused {
            state::append_event(
                &tx,
                &NewEvent {
                    task_id: Some(run.task_id.clone()),
                    run_id: Some(run.run_id.clone()),
                    event_type: event_type::REVIEW_FIX_REFUSED.to_string(),
                    detail: r.clone(),
                },
            )?;
        }
        state::insert_review(&tx, &row)?;
        state::append_event(
            &tx,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::VERDICT.to_string(),
                detail,
            },
        )?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(e) = written {
        return review::review_invalid(conn, run, format!("recording the review failed: {e:#}"), json!({"stage": "write"}));
    }
    let note = format!("{} over {} criteria", verdict.as_str(), doc.findings.criteria.len());
    ReviewIngest {
        outcome: IngestOutcome::Ok { summary: note.clone(), claim_count: 0, brief: note },
        verdict: Some(verdict),
        unopened_citations: unopened,
        settled: false,
        candidate_commit: None,
    }
}

/// A fix that passed every bound, committed but not yet on its branch.
struct Fix {
    bound: Vec<Bound>,
    ops: Vec<review_fix_edits::AppliedOp>,
    diff: review_fix_edits::Diff,
    fix_commit: String,
}

type Refused = (Refusal, Option<review_fix_edits::Diff>);

/// AC3's checks in their order, then the commit. The worktree is left with
/// the fix applied on success and in whatever state on a refusal (the caller
/// resets it).
#[allow(clippy::too_many_arguments)]
fn try_fix(
    conn: &Connection,
    ctx: &Ctx<'_>,
    run: &Run,
    task: &state::Task,
    wt: &Path,
    candidate: &str,
    branch: &str,
    items: &[Value],
    findings: &Findings,
) -> std::result::Result<Fix, Refused> {
    let write = |e: anyhow::Error| (Refusal::new(stage::WRITE, format!("{e:#}")), None);
    // (f) the reviewer left the worktree exactly at the candidate.
    let head = crate::worktree::rev_parse_head(wt).map_err(write)?;
    if head != candidate {
        return Err((Refusal::new(stage::WORKTREE, format!("the reviewer moved the fix worktree's HEAD to {head}; it must stay at the candidate {candidate}")), None));
    }
    let changes = review_fix_edits::project_changes(wt, true).map_err(write)?;
    if !changes.is_empty() {
        return Err((
            Refusal::new(stage::WORKTREE, format!("the reviewer changed files in the fix worktree itself: {}", changes.join(", "))),
            None,
        ));
    }
    let bound = review_fix_edits::validate(wt, items, findings).map_err(|r| (r, None))?;
    let edits: Vec<review_fix_edits::FixEdit> = bound.iter().map(|b| b.edit.clone()).collect();
    let ops = review_fix_edits::apply(wt, &edits).map_err(|r| (r, None))?;
    review_fix_edits::check_size(&bound, findings).map_err(|r| (r, None))?;
    let index = Path::new(&run.run_dir).join("fix-index");
    let staged = review_fix_edits::Staged::new(wt, candidate, &index).map_err(write)?;
    let diff = staged.diff().map_err(write)?;
    review_fix_edits::check_bounds(&diff).map_err(|r| (r, Some(diff.clone())))?;
    let refuse = |stage_: &'static str, why: String| (Refusal::new(stage_, why), Some(diff.clone()));
    // (c) no spec file, before or after.
    for f in &diff.files {
        if f.status != "added" {
            let before = staged_candidate_text(ctx.root, candidate, &f.path);
            if before.as_deref().is_some_and(review_fix_edits::is_spec_text) {
                return Err(refuse(stage::SPEC_FILE, format!("{} is a Thoth spec in the candidate; a fix never edits a spec", f.path)));
            }
        }
        if f.status != "deleted" {
            let after = std::fs::read_to_string(wt.join(&f.path)).ok();
            if after.as_deref().is_some_and(review_fix_edits::is_spec_text) {
                return Err(refuse(stage::SPEC_FILE, format!("{} is a Thoth spec after the fix; a fix never writes a spec", f.path)));
            }
        }
    }
    let catalogue = crate::protected::Catalogue::load(conn).map_err(write)?;
    let is_test = |p: &str| crate::protected::classify(p, &catalogue.patterns, &[], &[]) == Some(crate::protected::Kind::Test);
    match review_fix_edits::check_tests(&staged, &diff, is_test) {
        Ok(Ok(())) => {}
        Ok(Err(r)) => return Err((r, Some(diff.clone()))),
        Err(e) => return Err((Refusal::new(stage::WRITE, format!("{e:#}")), Some(diff.clone()))),
    }
    for f in &diff.files {
        let p = crate::protected::normalize(&f.path);
        if catalogue.inputs.iter().any(|i| crate::protected::normalize(i) == p) {
            return Err(refuse(stage::PROTECTED, format!("{} is an input file of a check; a fix never touches what a check reads", f.path)));
        }
        if crate::protected::is_check_word(&f.path, &catalogue.stems) {
            return Err(refuse(stage::PROTECTED, format!("{} is named like a word of a check's command; a fix never touches it", f.path)));
        }
    }
    let message = format!("{} (review fix by thoth run {}, on {branch})", task.title.trim(), run.run_id);
    let fix_commit = staged.commit(&message).map_err(|e| (Refusal::new(stage::WRITE, format!("{e:#}")), Some(diff.clone())))?;
    Ok(Fix { bound, ops, diff, fix_commit })
}

/// A file's text at `commit`, or `None` when it is not there or not text.
fn staged_candidate_text(root: &Path, commit: &str, path: &str) -> Option<String> {
    let mut cmd = std::process::Command::new("git");
    cmd.args(["show", &format!("{commit}:{path}")]).current_dir(root);
    crate::process::quiet(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// Moves the fix branch and the worktree to the fix, records its evidence,
/// the REJECT review row and `review_fix_applied`, all after re-checking in
/// one write transaction that the task is still `in_review`, the review's
/// target is still the newest implementer run, and nothing superseded this
/// review. Any failure puts the branch back (the caller resets the worktree).
#[allow(clippy::too_many_arguments)]
fn write_applied(
    conn: &Connection,
    ctx: &Ctx<'_>,
    run: &Run,
    target_run_id: &str,
    doc: &review_json::ReviewDocument,
    review_id: &str,
    candidate: &str,
    branch: &str,
    wt: &Path,
    branch_before: Option<&str>,
    fix: &Fix,
) -> Result<()> {
    let tx = db::write_tx(conn)?;
    let current = state::get_task(&tx, &run.task_id)?.ok_or_else(|| anyhow::anyhow!("task {} disappeared", run.task_id))?;
    if current.status != TaskStatus::InReview.as_str() {
        bail!("task {} is '{}' now, not in_review; the fix was not applied", run.task_id, current.status);
    }
    let latest = state::latest_completed_run_for_role(&tx, &run.task_id, role::IMPLEMENTER)?;
    if latest.as_ref().map(|r| r.run_id.as_str()) != Some(target_run_id) {
        bail!("a newer implementer run completed while the review ran; the fix was not applied");
    }
    let since = started_seq(&tx, &run.run_id)?.unwrap_or(0);
    let superseded = state::events_since(&tx, since, Some(&run.task_id))?.into_iter().any(|e| {
        e.event_type == event_type::VERDICT
            || e.event_type == event_type::REVIEW_FIX_APPLIED
            || e.event_type == event_type::REVIEW_FIX_REFUSED
    });
    if superseded {
        bail!("another review of task {} was recorded while this one ran; the fix was not applied", run.task_id);
    }
    let restore = |e: anyhow::Error| -> anyhow::Error {
        let _ = review_fix_edits::set_branch(ctx.root, branch, branch_before);
        e
    };
    review_fix_edits::set_branch(ctx.root, branch, Some(&fix.fix_commit)).map_err(restore)?;
    review_fix_edits::reset_worktree(wt, &fix.fix_commit).map_err(restore)?;
    let ectx = evidence::EvidenceContext { paths: ctx.paths, repo_id: ctx.repo_id, task_id: &run.task_id, run_id: &run.run_id };
    let diff_code = evidence::record_review_fix_diff(&tx, &ectx, ctx.root, candidate, &fix.fix_commit).map_err(restore)?;
    let commit_code = evidence::record_review_fix_commit(&tx, &ectx, ctx.root, &fix.fix_commit).map_err(restore)?;
    let row = state::Review {
        review_id: review_id.to_string(),
        ..review::new_review_row(target_run_id.to_string(), Some(run.run_id.clone()), Verdict::Reject, &doc.findings)
    };
    state::insert_review(&tx, &row).map_err(restore)?;
    let edits: Vec<Value> = fix
        .bound
        .iter()
        .zip(&fix.ops)
        .map(|(b, op)| {
            json!({
                "index": op.index, "path": b.edit.path, "op": op.op, "criterion": b.edit.criterion.trim(),
                "fix_size": b.fix_size, "category": b.category, "reason": b.edit.reason,
                "old": b.edit.old, "new": b.edit.new,
            })
        })
        .collect();
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::REVIEW_FIX_APPLIED.to_string(),
            detail: json!({
                "review_id": review_id,
                "target_run_id": target_run_id,
                "base_commit": candidate,
                "fix_commit": fix.fix_commit,
                "fix_branch": branch,
                "fix_worktree": wt.display().to_string(),
                "files": fix.diff.files_json(),
                "changed_lines": fix.diff.changed_lines,
                "file_count": fix.diff.file_count,
                "edits": edits,
                "diff_code": diff_code,
                "commit_code": commit_code,
            }),
        },
    )
    .map_err(restore)?;
    tx.commit().map_err(|e| restore(e.into()))?;
    if let Err(e) = evidence::refresh_index(conn, &run.task_id, wt) {
        crate::runner::log_runner_error(Path::new(&run.run_dir), &format!("rewriting the evidence index failed: {e:#}"));
    }
    Ok(())
}

/// What a confirmer's document invalid (or out of date) writes: one
/// `review_invalid` (`source` `author_check`); the task stays `in_review`
/// and the fix stays pending, so the confirm can be run again.
fn confirm_invalid(conn: &Connection, run: &Run, stage_: &str, reason: String) -> ReviewIngest {
    let _ = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::REVIEW_INVALID.to_string(),
            detail: json!({"stage": stage_, "reason": reason, "source": SOURCE_AUTHOR_CHECK}),
        },
    );
    ReviewIngest {
        outcome: IngestOutcome::Invalid { reason },
        verdict: None,
        unopened_citations: Vec::new(),
        settled: true,
        candidate_commit: None,
    }
}

/// A confirmer that left no readable `review.json`: invalid, the fix still
/// pending.
pub fn confirm_read_failed(conn: &Connection, run: &Run, reason: String) -> ReviewIngest {
    confirm_invalid(conn, run, "read", reason)
}

/// After a confirmer that changed the fix worktree: puts it back at
/// `fix_commit` while `branch` still points there, so the fix stays
/// confirmable; a moved branch or a missing worktree is left for the
/// commander. Says what it did, for the `review_invalid` reason.
fn restore_fix_worktree(root: &Path, wt: &Path, branch: &str, fix_commit: &str) -> String {
    match review_fix_edits::branch_sha(root, branch) {
        Ok(Some(s)) if s == fix_commit => {
            if !wt.join(".git").exists() {
                return format!("branch {branch} still points at the fix commit, but the fix worktree is gone");
            }
            match review_fix_edits::reset_worktree(wt, fix_commit) {
                Ok(()) => format!("the fix worktree was put back at the fix commit {fix_commit}"),
                Err(e) => format!("putting the fix worktree back at {fix_commit} failed: {e:#}"),
            }
        }
        Ok(_) => format!("the fix worktree was left as it is: branch {branch} no longer points at the fix commit"),
        Err(e) => format!("the fix worktree was left as it is: reading branch {branch} failed: {e:#}"),
    }
}

/// The confirmer's document rules, worded for `review.json`.
fn reword(e: spec_edits::ConfirmError) -> String {
    e.to_string().replace("spec-review.json", "review.json")
}

/// A finding copied from the edit review, without its `fix_size` and
/// `category` (those were counted with the edit review already).
fn strip_extra(e: &ExtraFinding) -> ExtraFinding {
    ExtraFinding { fix_size: None, category: None, ..e.clone() }
}

/// The runner's ingest of a confirmer's answer: the worktree, the document,
/// then in one transaction the checks that depend on the task's state and
/// the writes (review row, task status from `in_review`, `verdict`).
pub fn ingest_confirm(conn: &Connection, ctx: &Ctx<'_>, run: &Run, json_text: &str) -> ReviewIngest {
    let started = match started_detail(conn, &run.run_id) {
        Ok(Some(d)) => d,
        _ => return confirm_invalid(conn, run, "started", "the run's review_started event could not be read".into()),
    };
    let confirms = started["confirms_review_id"].as_str().unwrap_or_default().to_string();
    let fix_commit = started["fix_commit"].as_str().unwrap_or_default().to_string();
    let branch = started["fix_branch"].as_str().unwrap_or_default().to_string();
    let target_run_id = started["target_run_id"].as_str().unwrap_or_default().to_string();
    let fix_verify = started["fix_verify"].clone();
    let wt = PathBuf::from(&run.worktree);
    if let Err(why) = fix_worktree_ready(ctx.root, &wt, &branch, &fix_commit) {
        let restored = restore_fix_worktree(ctx.root, &wt, &branch, &fix_commit);
        return confirm_invalid(conn, run, stage::WORKTREE, format!("after the confirmer ran: {why}; {restored}"));
    }
    let doc = match spec_edits::parse_confirm_document(review::strip_bom(json_text)) {
        Ok(d) => d,
        Err(e) => return confirm_invalid(conn, run, "schema", reword(e)),
    };
    let applied = match fix_event(conn, &run.task_id, &confirms) {
        Ok(Some((ty, d))) if ty == event_type::REVIEW_FIX_APPLIED => d,
        _ => return confirm_invalid(conn, run, "superseded", format!("the fix of review {confirms} this confirm judges is not on the task")),
    };
    let edits = applied["edits"].as_array().cloned().unwrap_or_default();
    if let Err(e) = spec_edits::check_confirm_coverage(&doc, edits.len()) {
        return confirm_invalid(conn, run, "coverage", reword(e));
    }
    let author = doc;
    let approve = author.verdict.trim() == "APPROVE";
    let verify_ok = fix_verify["ok"] == json!(true);
    let verdict = if approve && verify_ok { Verdict::Pass } else { Verdict::Reject };
    let verify_detail = fix_verify["seq"]
        .as_i64()
        .and_then(|seq| {
            state::events_since(conn, seq - 1, Some(&run.task_id))
                .ok()?
                .into_iter()
                .find(|e| e.seq == seq)
                .and_then(|e| serde_json::from_str::<Value>(&e.detail).ok())
        })
        .unwrap_or(Value::Null);

    type Stop = (&'static str, String);
    let written: Result<std::result::Result<(), Stop>> = (|| {
        let tx = db::write_tx(conn)?;
        let current = state::get_task(&tx, &run.task_id)?.ok_or_else(|| anyhow::anyhow!("task {} disappeared", run.task_id))?;
        if current.status != TaskStatus::InReview.as_str() {
            return Ok(Err(("status", format!("task {} is '{}' now, not in_review", run.task_id, current.status))));
        }
        let reviews = state::list_reviews_for_task(&tx, &run.task_id)?;
        let Some(edit_row) = reviews.last().filter(|r| r.review_id == confirms).cloned() else {
            return Ok(Err((
                "superseded",
                format!(
                    "the newest review is now {}, not the edit review {confirms} this confirm judges",
                    reviews.last().map(|r| r.review_id.as_str()).unwrap_or("none")
                ),
            )));
        };
        let latest = state::latest_completed_run_for_role(&tx, &run.task_id, role::IMPLEMENTER)?;
        if latest.as_ref().map(|r| r.run_id.as_str()) != Some(edit_row.target_run_id.as_str()) || edit_row.target_run_id != target_run_id {
            return Ok(Err(("superseded", "a newer implementer run completed since the fix was applied".to_string())));
        }
        if applied["fix_commit"].as_str() != Some(fix_commit.as_str()) {
            return Ok(Err(("superseded", "the applied fix is not the commit this confirm judged".to_string())));
        }
        // GATE-1: the fix's protected changes, approved on the edit review
        // run, checked again under the write lock before a PASS is written.
        if verdict == Verdict::Pass {
            let edit_run_id = edit_row.reviewer_run_id.clone().unwrap_or_default();
            let why = match state::get_run(&tx, &edit_run_id)? {
                None => Some(format!("the edit review's run {edit_run_id} is gone")),
                Some(edit_run) => review::protected_refusal(&tx, &run.task_id, &edit_run, &[], None)?,
            };
            if let Some(why) = why {
                return Ok(Err((stage::PROTECTED, why)));
            }
        }
        let original: Findings = serde_json::from_str(&edit_row.findings).unwrap_or_default();
        let findings = if verdict == Verdict::Pass {
            pass_findings(&original, &edits, &applied, &verify_detail, &fix_commit, &branch, &run.run_id)
        } else {
            reject_findings(&original, &edits, &author, &verify_detail, verify_ok, &fix_commit, &applied)
        };
        let copied = if verdict == Verdict::Pass {
            findings.extra_findings.len()
        } else {
            original.extra_findings.len()
        };
        let mut stored = serde_json::to_value(&findings)?;
        stored["author_check"] = json!({"confirms_review_id": confirms, "copied_extra_findings": copied});
        let row = state::Review {
            review_id: crate::ids::new_review_id(),
            target_run_id: edit_row.target_run_id.clone(),
            reviewer_run_id: Some(run.run_id.clone()),
            verdict: verdict.as_str().to_string(),
            findings: stored.to_string(),
            created_at: crate::ids::now_rfc3339(),
        };
        state::insert_review(&tx, &row)?;
        let now = crate::ids::now_rfc3339();
        if !state::transition_task_status(&tx, &run.task_id, &[TaskStatus::InReview], verdict.task_status(), &now)? {
            return Ok(Err(("status", format!("task {} moved out of in_review", run.task_id))));
        }
        let doc_view = review_json::ReviewDocument { verdict: verdict.as_str().to_string(), findings: findings.clone() };
        let cited = review_json::cited_codes(&doc_view);
        let opened = review::opened_codes(&tx, &run.run_id).unwrap_or_default();
        let unopened: Vec<String> = cited.iter().filter(|c| !opened.contains(c)).cloned().collect();
        let mut detail = review::verdict_event_detail(review::VerdictEventShape {
            verdict,
            review_id: &row.review_id,
            target_run_id: &row.target_run_id,
            doc: &doc_view,
            cited: &cited,
            opened: Some(&opened),
            unopened: Some(&unopened),
            source: None,
        });
        if let Some(d) = detail.as_object_mut() {
            d.insert("source".into(), json!(SOURCE_AUTHOR_CHECK));
            d.insert("mode".into(), json!(ReviewMode::Confirm.as_str()));
            d.insert("confirms_review_id".into(), json!(confirms));
            d.insert("fix_commit".into(), json!(fix_commit));
            d.insert("fix_verify".into(), fix_verify.clone());
            d.insert("author_verdict".into(), json!(author.verdict.trim()));
            d.insert("document".into(), serde_json::to_value(&author)?);
        }
        state::append_event(
            &tx,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::VERDICT.to_string(),
                detail,
            },
        )?;
        tx.commit()?;
        Ok(Ok(()))
    })();
    match written {
        Err(e) => confirm_invalid(conn, run, stage::WRITE, format!("recording the confirm failed: {e:#}")),
        Ok(Err((s, why))) => confirm_invalid(conn, run, s, why),
        Ok(Ok(())) => {
            let note = format!(
                "confirmer {} over {} edit(s), fix verify {}: {}",
                author.verdict.trim(),
                edits.len(),
                if verify_ok { "green" } else { "not green" },
                verdict.as_str()
            );
            ReviewIngest {
                outcome: IngestOutcome::Ok { summary: note.clone(), claim_count: 0, brief: note },
                verdict: Some(verdict),
                unopened_citations: Vec::new(),
                settled: true,
                candidate_commit: Some(fix_commit),
            }
        }
    }
}

/// The evidence codes of a verify's rows.
fn verify_codes(detail: &Value) -> Vec<String> {
    detail["results"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| r["evidence"].as_str().map(str::to_string))
        .collect()
}

/// Which extra findings an edit list binds.
fn bound_extras(edits: &[Value]) -> Vec<usize> {
    edits
        .iter()
        .filter_map(|e| {
            e["criterion"]
                .as_str()?
                .strip_prefix("extra_findings[")?
                .strip_suffix(']')?
                .parse()
                .ok()
        })
        .collect()
}

/// A confirmed fix's PASS record: every criterion met; an originally unmet
/// one cites the fix's diff and its verify; only the extra findings no edit
/// was bound to stay.
fn pass_findings(
    original: &Findings,
    edits: &[Value],
    applied: &Value,
    verify: &Value,
    fix_commit: &str,
    branch: &str,
    confirm_run: &str,
) -> Findings {
    let mut evidence: Vec<String> = applied["diff_code"].as_str().map(str::to_string).into_iter().collect();
    evidence.extend(verify_codes(verify));
    let criteria = original
        .criteria
        .iter()
        .map(|c| {
            let mut c = c.clone();
            c.fix_size = None;
            c.category = None;
            if c.verdict.trim() == "unmet" {
                c.verdict = "met".into();
                c.evidence = evidence.clone();
                c.note = Some(format!(
                    "met by the review fix {fix_commit} on {branch}, verified and confirmed by run {confirm_run}"
                ));
            }
            c
        })
        .collect();
    let bound = bound_extras(edits);
    let extra_findings = original
        .extra_findings
        .iter()
        .enumerate()
        .filter(|(i, _)| !bound.contains(i))
        .map(|(_, e)| strip_extra(e))
        .collect();
    Findings { criteria, extra_findings, sources: Vec::new() }
}

/// A refused confirm's REJECT record: the edit review's findings as they
/// were, then one extra finding per revise, one for a red verify, and one
/// saying the fix was not adopted.
fn reject_findings(
    original: &Findings,
    edits: &[Value],
    author: &spec_edits::ConfirmDocument,
    verify: &Value,
    verify_ok: bool,
    fix_commit: &str,
    applied: &Value,
) -> Findings {
    let criteria = original
        .criteria
        .iter()
        .map(|c| review_json::CriterionFinding { fix_size: None, category: None, ..c.clone() })
        .collect();
    let mut extra_findings: Vec<ExtraFinding> = original.extra_findings.iter().map(strip_extra).collect();
    let new = |note: String, evidence: Vec<String>| ExtraFinding {
        severity: "major".into(),
        note,
        evidence,
        fix_size: None,
        category: None,
    };
    for a in author.edits.iter().filter(|a| a.verdict.trim() == "revise") {
        let path = edits
            .iter()
            .find(|e| e["index"].as_u64() == Some(a.index as u64))
            .and_then(|e| e["path"].as_str())
            .unwrap_or("?");
        extra_findings.push(new(format!("author check REVISE on fix edit {} ({path}): {}", a.index, a.note.trim()), Vec::new()));
    }
    if !verify_ok {
        let failing: Vec<String> = verify["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| r["exit_code"].as_i64() != Some(0) || !r["error"].is_null())
            .map(|r| {
                format!(
                    "{} exit {} ({})",
                    r["check_id"].as_str().unwrap_or("?"),
                    r["exit_code"].as_i64().map(|c| c.to_string()).unwrap_or_else(|| "none".into()),
                    r["evidence"].as_str().unwrap_or("-")
                )
            })
            .collect();
        let what = if failing.is_empty() { "no check ran".to_string() } else { failing.join(", ") };
        extra_findings.push(new(format!("verify of the fix {fix_commit} is not green: {what}"), verify_codes(verify)));
    }
    extra_findings.push(new(
        format!(
            "the attached fix {fix_commit} was not adopted; redo the work from the original candidate {}",
            applied["base_commit"].as_str().unwrap_or("?")
        ),
        Vec::new(),
    ));
    Findings { criteria, extra_findings, sources: Vec::new() }
}

/// `review show`'s `merge_branch` and `merge_commit`: only for a passed task
/// whose newest record is a PASS of the newest implementer run, and whose ref
/// still points at the commit that was judged.
pub fn merge_target(
    conn: &Connection,
    root: &Path,
    task: &state::Task,
    st: &State,
) -> Result<(Value, Value)> {
    let none = (Value::Null, Value::Null);
    let Some(latest) = st.reviews.last() else { return Ok(none) };
    if task.status != TaskStatus::Pass.as_str() || latest.verdict != Verdict::Pass.as_str() {
        return Ok(none);
    }
    let Some(target) = state::latest_completed_run_for_role(conn, &task.task_id, role::IMPLEMENTER)? else { return Ok(none) };
    if target.run_id != latest.target_run_id {
        return Ok(none);
    }
    let (branch, commit) = if st.mode.as_deref() == Some(ReviewMode::Confirm.as_str()) {
        let Some(run) = &latest.reviewer_run_id else { return Ok(none) };
        let d = started_detail(conn, run)?.unwrap_or(Value::Null);
        match (d["fix_branch"].as_str(), d["fix_commit"].as_str()) {
            (Some(b), Some(c)) => (b.to_string(), c.to_string()),
            _ => return Ok(none),
        }
    } else {
        match &target.candidate_commit {
            Some(c) => (crate::worktree::branch_name(&task.task_id, target.attempt), c.clone()),
            None => return Ok(none),
        }
    };
    match review_fix_edits::branch_sha(root, &branch)? {
        Some(s) if s == commit => Ok((json!(branch), json!(commit))),
        _ => Ok(none),
    }
}

/// `wait --run`'s `pending_fix` for one run: `null` for a run that is not a
/// code review; otherwise whether the task has a pending fix this run is the
/// edit review of, or a confirm of.
pub fn pending_fix_for_run(conn: &Connection, run: &Run) -> Result<Value> {
    if run.role != role::REVIEWER {
        return Ok(Value::Null);
    }
    let Some(task) = state::get_task(conn, &run.task_id)? else { return Ok(Value::Null) };
    if crate::spec::is_research_task(&task.spec) {
        return Ok(Value::Null);
    }
    let st = state(conn, &task)?;
    let Some((row, _)) = st.applied.as_ref().filter(|_| st.pending_fix) else { return Ok(json!(false)) };
    if row.reviewer_run_id.as_deref() == Some(run.run_id.as_str()) {
        return Ok(json!(true));
    }
    let d = started_detail(conn, &run.run_id)?.unwrap_or(Value::Null);
    Ok(json!(d["confirms_review_id"].as_str() == Some(row.review_id.as_str())))
}
