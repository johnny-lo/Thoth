//! `thoth spec review start|confirm|record|show` — a review of a task's spec
//! before its first dispatch (SR-1, SR-2, SR-3; decisions H9, H10, H11).
//!
//! A spec review is made outside thoth (a commander's subagent, or a human)
//! and recorded here, or `start` has Thoth launch its own read-only spec
//! reviewer run (role `spec_reviewer`) whose report the runner validates the
//! same way. Either writes one `spec_review` event: `APPROVE` or `REVISE`, every
//! finding bound to an acceptance criterion. Each event carries the sha256 of
//! the spec bytes it judged (H10), so `task update-spec` voids an approval by
//! changing the fingerprint. `dispatch` gates the first dispatch of an
//! `important`/`high` task on an APPROVE of the current spec (H11), and stops
//! every dispatch after `REVISE_STREAK_LIMIT` REVISE in a row.
//!
//! SR-3, "the reviewer edits, the author confirms": `start --edit` lets the
//! reviewer propose structured edits (`spec_edits`), which Thoth applies and
//! keeps as the proposed spec in the REVISE event (`mode` `edit`); the
//! reviewer stays read-only. `confirm` launches the author (the same role,
//! the same worktree, `mode` `confirm`) to judge only those edits; its
//! APPROVE replaces the task's spec with the proposed one (as `task
//! update-spec` would, in one transaction with a `spec_updated` event and a
//! `spec_review` APPROVE bound to the new sha256), its REVISE only records.
//! `show --proposed-out` hands the proposed spec to the commander.
//!
//! Everything lives in `events`: the seq is strictly increasing, so "newest"
//! never depends on a timestamp's resolution, and no table or schema change
//! is needed.

use crate::commands::review::{strip_bom, utf16_bom_kind};
use crate::commands::task::UPDATE_SPEC_STATUSES;
use crate::config::Paths;
use crate::db;
use crate::db::global;
use crate::db::state::{self, NewEvent, Run};
use crate::ids;
use crate::ingest::IngestOutcome;
use crate::lang::Lang;
use crate::model::{event_type, role, RunStatus, TaskStatus};
use crate::output;
use crate::plan_label;
use crate::redact::{Counts, Masker};
use crate::review_json;
use crate::spec;
use crate::spec_edits::{self, ReviewMode};
use crate::spec_review_json::{self, SpecReviewDocument, SpecVerdict};
use crate::spec_review_prompt;
use crate::worktree;
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// H11: the risks whose first dispatch needs an APPROVE of the current spec.
pub const GATED_RISKS: [&str; 2] = ["important", "high"];

/// This many REVISE in a row (no APPROVE between) stops dispatch for the
/// human, whatever the task's risk and whether or not it is the first.
pub const REVISE_STREAK_LIMIT: usize = 3;

/// `spec_review.detail.source` of an event `spec review record` wrote.
pub const SOURCE_EXTERNAL: &str = "external";
/// ... of one the runner wrote for a spec reviewer run (review or edit mode).
pub const SOURCE_REVIEWER_RUN: &str = "reviewer_run";
/// SR-3: ... of one the runner wrote for the author's check.
pub const SOURCE_AUTHOR_CHECK: &str = "author_check";
/// SR-3: `spec_review.detail.mode` of a `spec review record` event.
pub const MODE_EXTERNAL: &str = "external";

/// H10: the fingerprint an approval is bound to - the sha256 of the spec
/// bytes exactly as `tasks.spec` stores them, not normalised.
pub fn fingerprint(spec: &str) -> String {
    ids::sha256_hex(spec.as_bytes())
}

/// Whether `current_spec` still has the fingerprint a check was made against.
/// `spec review record` and `dispatch` both ask this inside their write
/// transaction, so an `update-spec` that landed after their read is caught.
pub fn spec_unchanged(expected_sha256: &str, current_spec: &str) -> bool {
    fingerprint(current_spec) == expected_sha256
}

/// The first 12 characters of a fingerprint, for human lines and messages.
pub fn short(sha: &str) -> &str {
    sha.get(..12).unwrap_or(sha)
}

pub fn gated_risk(risk: &str) -> bool {
    GATED_RISKS.contains(&risk)
}

/// Test seam, for tests/spec_review.rs only; debug builds only, so a release
/// build has no such pause. When `var` names a file: create it, then wait
/// (every 50 ms, at most 30 s) until the test deletes it, so the test can
/// change the spec between a command's read and its write transaction.
#[cfg(debug_assertions)]
pub fn test_pause(var: &str) {
    let Some(file) = std::env::var_os(var).map(std::path::PathBuf::from) else {
        return;
    };
    if std::fs::write(&file, b"").is_err() {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while file.exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

/// One `spec_review` event of a task, decoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Newest {
    pub spec_review_id: String,
    pub verdict: String,
    pub spec_sha256: String,
    pub seq: i64,
    pub ts: String,
    pub source: String,
    pub reviewer_run_id: Option<String>,
    pub document: serde_json::Value,
    /// SR-3: `external` | `review` | `edit` | `confirm`; an event written
    /// before SR-3 is `external` or `review` by its `source`.
    pub mode: String,
    pub edit_count: usize,
    pub proposed_sha256: Option<String>,
    /// The proposed spec's bytes; `None` when the event proposes nothing.
    pub proposed_spec: Option<String>,
    /// The completed edit list (`spec_edits::AppliedEdit` objects), `[]`
    /// when none.
    pub edits: serde_json::Value,
    /// The per-criterion summary the event carries (`id`, `verdict`,
    /// `findings` count), `[]` when the event has none. On an author's check
    /// this is the only per-criterion record: the author's document has no
    /// criteria.
    pub criteria: serde_json::Value,
}

/// Where a task's spec review stands right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The fingerprint of the task's current spec.
    pub spec_sha256: String,
    pub review_count: usize,
    pub newest: Option<Newest>,
    /// The newest spec review is an APPROVE of the current spec.
    pub approved: bool,
    /// REVISE in a row, counted back from the newest; an APPROVE ends it.
    /// `update-spec` does not: every REVISE is followed by a spec change.
    pub revise_streak: usize,
    /// The ids of that streak, oldest first.
    pub streak_ids: Vec<String>,
    pub stalled: bool,
    /// SR-3: the newest spec review is an edit-mode REVISE with at least one
    /// edit, of the current spec; `spec review confirm` can run.
    pub pending_edits: bool,
}

fn decode(e: &state::EventRow) -> Option<Newest> {
    let d: serde_json::Value = serde_json::from_str(&e.detail).ok()?;
    let text = |k: &str| d.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let source = text("source").unwrap_or_else(|| SOURCE_EXTERNAL.to_string());
    let mode = text("mode").unwrap_or_else(|| {
        if source == SOURCE_REVIEWER_RUN {
            ReviewMode::Review.as_str().to_string()
        } else {
            MODE_EXTERNAL.to_string()
        }
    });
    Some(Newest {
        spec_review_id: text("spec_review_id")?,
        verdict: text("verdict")?,
        spec_sha256: text("spec_sha256")?,
        seq: e.seq,
        ts: e.ts.clone(),
        source,
        reviewer_run_id: text("reviewer_run_id"),
        document: d.get("document").cloned().unwrap_or(serde_json::Value::Null),
        mode,
        edit_count: d.get("edit_count").and_then(|v| v.as_u64()).unwrap_or(0) as usize,
        proposed_sha256: text("proposed_sha256"),
        proposed_spec: text("proposed_spec"),
        edits: d
            .get("edits")
            .filter(|v| v.is_array())
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
        criteria: d
            .get("criteria")
            .filter(|v| v.is_array())
            .cloned()
            .unwrap_or_else(|| serde_json::json!([])),
    })
}

fn reviews_of(conn: &Connection, task_id: &str) -> Result<Vec<Newest>> {
    Ok(state::events_since(conn, 0, Some(task_id))?
        .iter()
        .filter(|e| e.event_type == event_type::SPEC_REVIEW)
        .filter_map(decode)
        .collect())
}

/// SR-3: one spec review of a task by its id.
pub fn find_review(conn: &Connection, task_id: &str, spec_review_id: &str) -> Result<Option<Newest>> {
    Ok(reviews_of(conn, task_id)?
        .into_iter()
        .find(|n| n.spec_review_id == spec_review_id))
}

/// BRIEF-1: the newest spec review spec reviewer run `run_id` wrote.
pub fn review_for_run(conn: &Connection, task_id: &str, run_id: &str) -> Result<Option<Newest>> {
    Ok(reviews_of(conn, task_id)?
        .into_iter()
        .rev()
        .find(|n| n.reviewer_run_id.as_deref() == Some(run_id)))
}

/// BRIEF-1: the short conclusion of one spec review.
pub fn brief_of(n: &Newest) -> Option<crate::brief::Brief> {
    crate::brief::for_spec_review(&Masker::current(), &n.verdict, &n.mode, &n.document, n.edit_count)
}

pub fn state(conn: &Connection, task: &state::Task) -> Result<State> {
    let events: Vec<state::EventRow> = state::events_since(conn, 0, Some(&task.task_id))?
        .into_iter()
        .filter(|e| e.event_type == event_type::SPEC_REVIEW)
        .collect();
    let review_count = events.len();
    let reviews: Vec<Newest> = events.iter().filter_map(decode).collect();
    let spec_sha256 = fingerprint(&task.spec);
    let newest = reviews.last().cloned();
    let approved = newest
        .as_ref()
        .map(|n| n.verdict == SpecVerdict::Approve.as_str() && n.spec_sha256 == spec_sha256)
        .unwrap_or(false);
    let pending_edits = newest
        .as_ref()
        .map(|n| {
            n.verdict == SpecVerdict::Revise.as_str()
                && n.mode == ReviewMode::Edit.as_str()
                && n.edit_count >= 1
                && n.spec_sha256 == spec_sha256
        })
        .unwrap_or(false);
    let mut streak_ids: Vec<String> = reviews
        .iter()
        .rev()
        .take_while(|n| n.verdict == SpecVerdict::Revise.as_str())
        .map(|n| n.spec_review_id.clone())
        .collect();
    streak_ids.reverse();
    let revise_streak = streak_ids.len();
    Ok(State {
        spec_sha256,
        review_count,
        newest,
        approved,
        revise_streak,
        streak_ids,
        stalled: revise_streak >= REVISE_STREAK_LIMIT,
        pending_edits,
    })
}

/// H11: does the next dispatch of this task need an APPROVE of its current
/// spec? Only for a gated risk, and only before the first dispatch - a task
/// with any implementer run at all (failed and cancelled ones included) has
/// been dispatched already.
pub fn required(conn: &Connection, task: &state::Task) -> Result<bool> {
    if !gated_risk(&task.risk) {
        return Ok(false);
    }
    Ok(!state::list_runs_for_task(conn, &task.task_id)?
        .iter()
        .any(|r| r.role == role::IMPLEMENTER))
}

/// SR-3: the edit-related keys every `spec_review` event carries.
#[derive(Debug, Clone)]
pub struct EditsSummary {
    pub edits: serde_json::Value,
    pub edit_count: usize,
    pub proposed_spec: Option<String>,
    pub proposed_sha256: Option<String>,
    pub bytes_before: usize,
    pub bytes_after: usize,
}

impl EditsSummary {
    /// No edits: the event judges `spec` and proposes nothing.
    pub fn none(spec: &str) -> Self {
        Self {
            edits: serde_json::json!([]),
            edit_count: 0,
            proposed_spec: None,
            proposed_sha256: None,
            bytes_before: spec.len(),
            bytes_after: spec.len(),
        }
    }

    fn from_applied(spec: &str, applied: &spec_edits::Applied) -> Self {
        Self {
            edits: serde_json::to_value(&applied.edits).unwrap_or_else(|_| serde_json::json!([])),
            edit_count: applied.edits.len(),
            proposed_spec: Some(applied.proposed.clone()),
            proposed_sha256: Some(fingerprint(&applied.proposed)),
            bytes_before: spec.len(),
            bytes_after: applied.proposed.len(),
        }
    }

    /// The summary an author's check copies from the edit review it judged.
    fn from_review(n: &Newest, bytes_before: usize) -> Self {
        let bytes_after = n.proposed_spec.as_ref().map(|p| p.len()).unwrap_or(bytes_before);
        Self {
            edits: n.edits.clone(),
            edit_count: n.edit_count,
            proposed_spec: n.proposed_spec.clone(),
            proposed_sha256: n.proposed_sha256.clone(),
            bytes_before,
            bytes_after,
        }
    }
}

/// What one `spec_review` event says, before it is serialized.
struct ReviewEvent<'a> {
    spec_review_id: &'a str,
    verdict: SpecVerdict,
    spec_sha256: &'a str,
    task: &'a state::Task,
    criteria: Vec<serde_json::Value>,
    findings: usize,
    revise_streak: usize,
    source: &'a str,
    reviewer_run_id: Option<&'a str>,
    document: serde_json::Value,
    mode: &'a str,
    edits: &'a EditsSummary,
    /// Confirm-only keys; `None` for every other writer.
    confirm: Option<ConfirmKeys<'a>>,
}

struct ConfirmKeys<'a> {
    confirms_spec_review_id: &'a str,
    spec_sha256_before: &'a str,
    applied: bool,
}

fn review_detail(e: &ReviewEvent<'_>) -> serde_json::Value {
    let mut d = serde_json::json!({
        "spec_review_id": e.spec_review_id,
        "verdict": e.verdict.as_str(),
        "spec_sha256": e.spec_sha256,
        "risk": e.task.risk,
        "task_status": e.task.status,
        "criteria": e.criteria,
        "findings": e.findings,
        "revise_streak": e.revise_streak,
        "stalled": e.revise_streak >= REVISE_STREAK_LIMIT,
        "source": e.source,
        "reviewer_run_id": e.reviewer_run_id,
        "document": e.document,
        "mode": e.mode,
        "edits": e.edits.edits,
        "edit_count": e.edits.edit_count,
        "proposed_spec": e.edits.proposed_spec,
        "proposed_sha256": e.edits.proposed_sha256,
        "bytes_before": e.edits.bytes_before,
        "bytes_after": e.edits.bytes_after,
    });
    if let (Some(c), Some(o)) = (&e.confirm, d.as_object_mut()) {
        o.insert(
            "confirms_spec_review_id".to_string(),
            serde_json::json!(c.confirms_spec_review_id),
        );
        o.insert(
            "spec_sha256_before".to_string(),
            serde_json::json!(c.spec_sha256_before),
        );
        o.insert("applied".to_string(), serde_json::json!(c.applied));
    }
    #[cfg(debug_assertions)]
    test_pre_sr3(&mut d);
    d
}

/// Test seam, for tests/spec_review_edit.rs only; debug builds only, so a
/// release build has no such switch. When `THOTH_TEST_SPEC_REVIEW_PRE_SR3`
/// is set, a `spec_review` event leaves out every key SR-3 added, as a thoth
/// from before SR-3 wrote it, so a test can make one through the CLI and
/// check how it is read.
#[cfg(debug_assertions)]
fn test_pre_sr3(d: &mut serde_json::Value) {
    if std::env::var_os("THOTH_TEST_SPEC_REVIEW_PRE_SR3").is_none() {
        return;
    }
    if let Some(o) = d.as_object_mut() {
        for k in [
            "mode",
            "edits",
            "edit_count",
            "proposed_spec",
            "proposed_sha256",
            "bytes_before",
            "bytes_after",
            "confirms_spec_review_id",
            "spec_sha256_before",
            "applied",
        ] {
            o.remove(k);
        }
    }
}

fn criteria_summary(doc: &SpecReviewDocument) -> Vec<serde_json::Value> {
    doc.criteria
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id, "verdict": c.verdict, "findings": c.findings.len(),
            })
        })
        .collect()
}

pub struct RecordArgs<'a> {
    pub task: &'a str,
    pub file: &'a Path,
    pub json: bool,
}

/// `thoth spec review record <task> -f <path>`: the same shape as
/// `review record`. Only an invalid *document* writes something (one
/// `spec_review_invalid` event); every precondition refusal writes nothing.
/// The task's status is never changed.
pub fn record(paths: &Paths, cwd: &Path, args: RecordArgs<'_>) -> Result<i32> {
    // Same guard as `review record`: a run must not approve its own spec.
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!(
            "spec review record refuses to run with THOTH_RUN_ID set: a spec review is \
             recorded by the supervising session, not by a run"
        );
    }

    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, args.task)?;

    let raw_bytes = std::fs::read(args.file)
        .with_context(|| format!("reading {}", args.file.display()))?;
    if let Some(kind) = utf16_bom_kind(&raw_bytes) {
        bail!(
            "{} looks like {kind} (Windows PowerShell 5.1's `Out-File`/`>` write this by \
             default); save it as UTF-8 and record again",
            args.file.display()
        );
    }
    let raw = String::from_utf8(raw_bytes)
        .with_context(|| format!("{} is not valid UTF-8", args.file.display()))?;
    let json_text = strip_bom(&raw);

    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;

    let invalid = |stage: &str, reason: String, expected: Option<Vec<String>>| -> Result<i32> {
        let mut detail = serde_json::json!({
            "stage": stage,
            "reason": reason,
            "source": SOURCE_EXTERNAL,
        });
        if let (Some(expected), Some(o)) = (expected, detail.as_object_mut()) {
            o.insert("expected".to_string(), serde_json::json!(expected));
        }
        state::append_event(
            &conn,
            &NewEvent {
                task_id: Some(task_id.clone()),
                run_id: None,
                event_type: event_type::SPEC_REVIEW_INVALID.to_string(),
                detail,
            },
        )?;
        bail!("spec-review.json invalid at stage {stage}: {reason}");
    };
    let doc = match spec_review_json::parse_spec_review(json_text) {
        Ok(d) => d,
        Err(e) => return invalid("schema", e.to_string(), None),
    };
    let parsed = match spec::parse_spec(&task.spec) {
        Ok(p) => p,
        Err(e) => {
            return invalid(
                "spec",
                format!("the task spec does not parse, so its criteria cannot be checked: {e}"),
                None,
            )
        }
    };
    let expected = review_json::ac_ids(&parsed);
    if let Err(e) = spec_review_json::check_coverage(&doc, &expected) {
        return invalid("coverage", e.to_string(), Some(expected));
    }
    // The spec the document was validated against, and so the one it judges.
    let validated_sha256 = fingerprint(&task.spec);

    #[cfg(debug_assertions)]
    test_pause("THOTH_TEST_SPEC_REVIEW_PAUSE_FILE");

    let tx = db::write_tx(&conn)?;
    let current = state::get_task(&tx, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    // An `update-spec` between the read above and this transaction: the
    // review judged the old spec, so it must not be recorded against the new.
    if !spec_unchanged(&validated_sha256, &current.spec) {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "task {task_id}'s spec changed while the spec review was being recorded (sha256 {} \
             -> {}); review the new spec and record again",
            short(&validated_sha256),
            short(&fingerprint(&current.spec))
        );
    }
    let refused = [
        TaskStatus::Running,
        TaskStatus::Pass,
        TaskStatus::Cancelled,
        TaskStatus::Accepted,
    ];
    if refused.iter().any(|s| s.as_str() == current.status) {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "task {task_id} is '{}'; spec review record needs a task that is not running, pass, cancelled or accepted",
            current.status
        );
    }

    let before = state(&tx, &current)?;
    let verdict = SpecVerdict::parse(&doc.verdict)
        .expect("parse_spec_review accepted only APPROVE or REVISE");
    let revise_streak = if verdict == SpecVerdict::Revise {
        before.revise_streak + 1
    } else {
        0
    };
    let stalled = revise_streak >= REVISE_STREAK_LIMIT;
    let spec_review_id = ids::new_review_id();
    let finding_count: usize = doc.criteria.iter().map(|c| c.findings.len()).sum();
    let edits = EditsSummary::none(&current.spec);
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: None,
            event_type: event_type::SPEC_REVIEW.to_string(),
            detail: review_detail(&ReviewEvent {
                spec_review_id: &spec_review_id,
                verdict,
                spec_sha256: &validated_sha256,
                task: &current,
                criteria: criteria_summary(&doc),
                findings: finding_count,
                revise_streak,
                source: SOURCE_EXTERNAL,
                reviewer_run_id: None,
                document: serde_json::to_value(&doc)?,
                mode: MODE_EXTERNAL,
                edits: &edits,
                confirm: None,
            }),
        },
    )?;
    tx.commit()?;

    let required_before_dispatch = required(&conn, &current)?;
    let v = serde_json::json!({
        "task_id": task_id,
        "spec_review_id": spec_review_id,
        "verdict": verdict.as_str(),
        "spec_sha256": validated_sha256,
        "risk": current.risk,
        "task_status": current.status,
        "approved": verdict == SpecVerdict::Approve,
        "required_before_dispatch": required_before_dispatch,
        "revise_streak": revise_streak,
        "stalled": stalled,
        "source": SOURCE_EXTERNAL,
    });
    let stuck = if stalled {
        format!(
            "; stuck: {revise_streak} REVISE in a row, dispatch is stopped until an APPROVE is recorded"
        )
    } else {
        String::new()
    };
    output::emit(
        args.json,
        &v,
        &format!(
            "{task_id}  {}  (spec review {spec_review_id}; spec sha256 {}; {} criteria, {finding_count} findings){stuck}",
            verdict.as_str(),
            short(&validated_sha256),
            doc.criteria.len(),
        ),
    );
    Ok(0)
}

/// The first `n` characters of a text, for a human line.
fn clip(s: &str, n: usize) -> String {
    let one_line: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let cut: String = one_line.chars().take(n).collect();
    if cut.chars().count() < one_line.chars().count() {
        format!("{cut}…")
    } else {
        cut
    }
}

/// `thoth spec review show <task>`: one set of `--json` keys whether or not a
/// spec review exists (L10). SR-3: `--proposed-out FILE` writes the newest
/// review's proposed spec, when it has one for the current spec.
pub fn show(paths: &Paths, cwd: &Path, needle: &str, proposed_out: Option<&Path>, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;
    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    let st = state(&conn, &task)?;
    let required_before_dispatch = required(&conn, &task)?;
    if let Some(file) = proposed_out {
        let takeable = st
            .newest
            .as_ref()
            .filter(|n| n.proposed_spec.is_some() && n.spec_sha256 == st.spec_sha256);
        let Some(n) = takeable else {
            bail!(
                "task {task_id} has no proposed spec to take: the newest spec review {}; run \
                 `thoth spec review start {task_id} --edit ...` for one",
                match &st.newest {
                    None => "does not exist".to_string(),
                    Some(n) if n.proposed_spec.is_none() => format!("{} proposes no edits", n.spec_review_id),
                    Some(n) => format!(
                        "{} judged an older spec (sha256 {}, now {})",
                        n.spec_review_id,
                        short(&n.spec_sha256),
                        short(&st.spec_sha256)
                    ),
                }
            );
        };
        let bytes = n.proposed_spec.as_deref().unwrap_or_default().as_bytes();
        std::fs::write(file, bytes).with_context(|| format!("writing {}", file.display()))?;
        masked_eprintln!("wrote {} ({} bytes)", file.display(), bytes.len());
    }
    let brief = st.newest.as_ref().and_then(brief_of);
    let newest = match &st.newest {
        None => serde_json::Value::Null,
        Some(n) => serde_json::json!({
            "spec_review_id": n.spec_review_id,
            "verdict": n.verdict,
            "spec_sha256": n.spec_sha256,
            "current": n.spec_sha256 == st.spec_sha256,
            "created_at": n.ts,
            "seq": n.seq,
            "source": n.source,
            "reviewer_run_id": n.reviewer_run_id,
            "document": n.document,
            "mode": n.mode,
            "edit_count": n.edit_count,
            "proposed_sha256": n.proposed_sha256,
        }),
    };
    let v = serde_json::json!({
        "task_id": task_id,
        "risk": task.risk,
        "task_status": task.status,
        "spec_sha256": st.spec_sha256,
        "review_count": st.review_count,
        "required_before_dispatch": required_before_dispatch,
        "approved": st.approved,
        "revise_streak": st.revise_streak,
        "stalled": st.stalled,
        "pending_edits": st.pending_edits,
        "newest": newest,
        "brief": brief.as_ref().map(|b| b.text.clone()),
    });

    let Some(n) = &st.newest else {
        output::emit(
            json,
            &v,
            &format!(
                "no spec review recorded for {task_id} (spec sha256 {}); record one with \
                 `thoth spec review record {task_id} -f spec-review.json`",
                short(&st.spec_sha256)
            ),
        );
        return Ok(0);
    };
    let current = if n.spec_sha256 == st.spec_sha256 {
        "current"
    } else {
        "stale: the spec changed since"
    };
    let approved = if st.approved {
        "approved for dispatch"
    } else {
        "not approved"
    };
    let stuck = if st.stalled {
        format!(", stuck: {} REVISE in a row", st.revise_streak)
    } else {
        String::new()
    };
    let mut lines = vec![format!(
        "{task_id}  {}  (spec review {} of {}, spec sha256 {}, {current}, {approved}{stuck})",
        n.verdict,
        n.spec_review_id,
        st.review_count,
        short(&n.spec_sha256),
    )];
    // BRIEF-1: the findings and answers below are the full text.
    if let Some(b) = &brief {
        lines.push(crate::brief::human_lines(b, "the findings below"));
    }
    // One line per criterion. A reviewer's document carries the findings
    // themselves; an author's check (SR-3) carries no criteria, so its lines
    // come from the summary Thoth wrote into the event (every criterion ok on
    // an APPROVE; on a REVISE, revise with one finding per refused edit).
    let is_confirm = n.mode == ReviewMode::Confirm.as_str();
    if is_confirm {
        for c in n.criteria.as_array().into_iter().flatten() {
            lines.push(format!(
                "  {:<6} {:<7} {} finding(s)",
                c["id"].as_str().unwrap_or(""),
                c["verdict"].as_str().unwrap_or(""),
                c["findings"].as_u64().unwrap_or(0)
            ));
        }
    } else if let Ok(doc) = serde_json::from_value::<SpecReviewDocument>(n.document.clone()) {
        for c in &doc.criteria {
            lines.push(format!(
                "  {:<6} {:<7} {} finding(s)",
                c.id,
                c.verdict,
                c.findings.len()
            ));
            for f in &c.findings {
                lines.push(format!("    {:<11} {}  {}", f.check, f.location, f.note));
            }
        }
    }
    // SR-3: one line per proposed edit; on an author's check, the author's
    // answer after it.
    let author: Option<spec_edits::ConfirmDocument> = if is_confirm {
        serde_json::from_value(n.document.clone()).ok()
    } else {
        None
    };
    let masker = Masker::load(Some(&paths.home));
    for e in n.edits.as_array().into_iter().flatten() {
        let index = e["index"].as_u64().unwrap_or(0) as usize;
        // RD-4: the whole text is masked before it is cut
        let after = e["after"]
            .as_str()
            .map(|s| clip(&masker.mask_text(s).0, 60))
            .unwrap_or_else(|| "(deleted)".to_string());
        let mut line = format!(
            "  edit {index}  {}[{} {}]  {}  {}  {}",
            e["field"].as_str().unwrap_or(""),
            e["op"].as_str().unwrap_or(""),
            e["at"].as_u64().unwrap_or(0),
            e["criterion"].as_str().unwrap_or(""),
            e["reason"].as_str().unwrap_or(""),
            after
        );
        if let Some(a) = author
            .as_ref()
            .and_then(|d| d.edits.iter().find(|a| a.index == index))
        {
            line.push_str(&format!("  author: {}", a.verdict));
            if !a.note.trim().is_empty() {
                line.push_str(&format!(" ({})", a.note.trim()));
            }
        }
        lines.push(line);
    }
    if n.mode == ReviewMode::Edit.as_str() && st.pending_edits {
        lines.push(format!(
            "  {} edit(s) await the author check: thoth spec review confirm {task_id} --profile P --model M --effort E",
            n.edit_count
        ));
    }
    output::emit(json, &v, &lines.join("\n"));
    Ok(0)
}

/// SR-2: `worktrees/<task_id>-spec-review`, one per task, detached at the
/// HEAD of the worktree `spec review start` runs in when that contains the
/// task's base, otherwise at the task's base.
pub const WORKTREE_SUFFIX: &str = "-spec-review";

/// The output schema handed to codex and agy, in the run dir.
pub const SCHEMA_FILE: &str = "spec-review-schema.json";

pub fn spec_review_worktree_path(paths: &Paths, repo_id: &str, task_id: &str) -> PathBuf {
    paths
        .worktrees_dir(repo_id)
        .join(format!("{task_id}{WORKTREE_SUFFIX}"))
}

/// Writes the spec reviewer's input: `INPUT_DIR/INPUT_FILE`. With no secret
/// in it, that is the stored spec bytes as they are; with one, it is the
/// masked and re-serialized spec (RD-1), and the fingerprint still covers the
/// stored spec. A previous review's input is cleared first.
pub fn write_input(spec: &str, worktree: &Path, masker: &Masker) -> Result<Counts> {
    let dir = worktree.join(spec_review_prompt::INPUT_DIR);
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clearing {}", dir.display())),
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let file = dir.join(spec_review_prompt::INPUT_FILE);
    let (bytes, counts) = masked_spec(spec, masker);
    std::fs::write(&file, bytes.as_bytes()).with_context(|| format!("writing {}", file.display()))?;
    Ok(counts)
}

/// `spec` as a spec reviewer may see it: only string values are masked, so
/// an unterminated key block cannot eat the end of the JSON. Unchanged bytes
/// when nothing was masked; text that is not JSON is masked as text.
fn masked_spec(spec: &str, masker: &Masker) -> (String, Counts) {
    match serde_json::from_str::<serde_json::Value>(spec) {
        Ok(mut v) => {
            let counts = masker.mask_value(&mut v);
            if counts.is_empty() {
                return (spec.to_string(), counts);
            }
            let bytes = match &v {
                // `to_string_pretty` would put the keys in name order.
                serde_json::Value::Object(root) => spec_edits::serialize_spec(root),
                other => serde_json::to_string_pretty(other).unwrap_or_default(),
            };
            (bytes, counts)
        }
        Err(_) => masker.mask_text(spec),
    }
}

/// SR-3: the author's two extra inputs, next to `spec.json` (call after
/// `write_input`): the proposed spec's bytes as they are when there is no
/// secret in them (masked like `write_input`'s otherwise), and the completed
/// edit list, its strings masked.
pub fn write_confirm_input(
    worktree: &Path,
    proposed: &str,
    edits: &serde_json::Value,
    masker: &Masker,
) -> Result<Counts> {
    let dir = worktree.join(spec_review_prompt::INPUT_DIR);
    let proposed_file = dir.join(spec_review_prompt::PROPOSED_FILE);
    let (bytes, mut counts) = masked_spec(proposed, masker);
    std::fs::write(&proposed_file, bytes.as_bytes())
        .with_context(|| format!("writing {}", proposed_file.display()))?;
    let edits_file = dir.join(spec_review_prompt::EDITS_FILE);
    let mut edits = edits.clone();
    counts.merge(&masker.mask_value(&mut edits));
    std::fs::write(&edits_file, serde_json::to_string_pretty(&edits)?)
        .with_context(|| format!("writing {}", edits_file.display()))?;
    Ok(counts)
}

/// The detail of the newest event of `type` written on run `run_id`.
fn run_event_detail(conn: &Connection, run_id: &str, ty: &str) -> Result<Option<serde_json::Value>> {
    let detail: Option<String> = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            rusqlite::params![run_id, ty],
            |r| r.get(0),
        )
        .optional()?;
    Ok(detail.and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok()))
}

/// One key of the newest event of `type` written on run `run_id`.
fn run_event_text(conn: &Connection, run_id: &str, ty: &str, key: &str) -> Result<Option<String>> {
    Ok(run_event_detail(conn, run_id, ty)?.and_then(|v| v[key].as_str().map(|s| s.to_string())))
}

/// A spec reviewer run's `spec_review_started` detail: the sha256 of the
/// spec it was started against, its `mode` (SR-3) and, for an author's
/// check, `confirms_spec_review_id`.
pub fn started_detail_for(conn: &Connection, run_id: &str) -> Result<Option<serde_json::Value>> {
    run_event_detail(conn, run_id, event_type::SPEC_REVIEW_STARTED)
}

/// The mode a `spec_review_started` detail records; `review` for one written
/// before SR-3.
pub fn mode_of(started: &serde_json::Value) -> ReviewMode {
    started["mode"]
        .as_str()
        .and_then(ReviewMode::parse)
        .unwrap_or(ReviewMode::Review)
}

/// The verdict a spec reviewer run reached, from its own `spec_review` event.
pub fn verdict_for_run(conn: &Connection, run_id: &str) -> Result<Option<String>> {
    run_event_text(conn, run_id, event_type::SPEC_REVIEW, "verdict")
}

pub struct StartArgs<'a> {
    pub task: &'a str,
    /// A profile id (`auto` is refused, RJ-1).
    pub profile: &'a str,
    pub model: Option<&'a str>,
    pub timeout_seconds: u64,
    pub effort: Option<&'a str>,
    pub reason: Option<&'a str>,
    /// SR-3: edit mode.
    pub edit: bool,
    pub json: bool,
}

/// `thoth spec review start <task> --profile P`: launch Thoth's own read-only
/// spec reviewer (SR-2). The same order as `review start`; every refusal
/// comes before the run row, so it writes nothing (the worktree may exist).
/// The task's status is never changed, now or when the run ends.
pub fn start(paths: &Paths, cwd: &Path, args: StartArgs<'_>) -> Result<i32> {
    launch(
        paths,
        cwd,
        LaunchArgs {
            task: args.task,
            profile: args.profile,
            model: args.model,
            timeout_seconds: args.timeout_seconds,
            effort: args.effort,
            reason: args.reason,
            json: args.json,
            mode: if args.edit { ReviewMode::Edit } else { ReviewMode::Review },
        },
    )
}

pub struct ConfirmArgs<'a> {
    pub task: &'a str,
    pub profile: &'a str,
    pub model: Option<&'a str>,
    pub timeout_seconds: u64,
    pub effort: Option<&'a str>,
    pub reason: Option<&'a str>,
    pub json: bool,
}

/// SR-3: `thoth spec review confirm <task> --profile P`: launch the author's
/// check of the newest edit review's edits. `start`'s refusals in `start`'s
/// order, plus: the task's status must allow `update-spec` (the APPROVE
/// replaces the spec) and the newest spec review must be an edit-mode REVISE
/// with edits of the current spec.
pub fn confirm(paths: &Paths, cwd: &Path, args: ConfirmArgs<'_>) -> Result<i32> {
    launch(
        paths,
        cwd,
        LaunchArgs {
            task: args.task,
            profile: args.profile,
            model: args.model,
            timeout_seconds: args.timeout_seconds,
            effort: args.effort,
            reason: args.reason,
            json: args.json,
            mode: ReviewMode::Confirm,
        },
    )
}

struct LaunchArgs<'a> {
    task: &'a str,
    profile: &'a str,
    model: Option<&'a str>,
    timeout_seconds: u64,
    effort: Option<&'a str>,
    reason: Option<&'a str>,
    json: bool,
    mode: ReviewMode,
}

/// What the status rule of a launch refuses with.
fn refuse_status(task_id: &str, status: &str, mode: ReviewMode) -> Result<()> {
    if mode == ReviewMode::Confirm {
        if !UPDATE_SPEC_STATUSES.iter().any(|s| s.as_str() == status) {
            bail!(
                "task {task_id} is '{status}'; spec review confirm needs draft, queued, reject or \
                 unverified, because its APPROVE replaces the spec"
            );
        }
        return Ok(());
    }
    let refused = [
        TaskStatus::Running,
        TaskStatus::Pass,
        TaskStatus::Cancelled,
        TaskStatus::Accepted,
    ];
    if refused.iter().any(|s| s.as_str() == status) {
        bail!(
            "task {task_id} is '{status}'; spec review start needs a task that is not running, pass, cancelled or accepted"
        );
    }
    Ok(())
}

/// SR-3: why `confirm` cannot run when `pending_edits` is false.
fn no_pending_edits(task_id: &str, st: &State) -> String {
    match &st.newest {
        None => format!(
            "task {task_id} has no spec review; run `thoth spec review start {task_id} --edit ...` \
             first, then confirm its edits"
        ),
        Some(n) if n.mode != ReviewMode::Edit.as_str()
            || n.verdict != SpecVerdict::Revise.as_str()
            || n.edit_count == 0 =>
        {
            format!(
                "task {task_id}'s newest spec review {} is not an edit review with edits (mode {}, {}, \
                 {} edits); run `thoth spec review start {task_id} --edit ...` first",
                n.spec_review_id, n.mode, n.verdict, n.edit_count
            )
        }
        Some(n) => format!(
            "task {task_id}'s spec changed after spec review {} (sha256 {} -> {}); the edits were \
             proposed for the old spec, run `thoth spec review start {task_id} --edit ...` again",
            n.spec_review_id,
            short(&n.spec_sha256),
            short(&st.spec_sha256)
        ),
    }
}

fn launch(paths: &Paths, cwd: &Path, args: LaunchArgs<'_>) -> Result<i32> {
    use crate::commands::auto_pick;
    let mode = args.mode;
    let is_confirm = mode == ReviewMode::Confirm;
    let command = if is_confirm { "spec review confirm" } else { "spec review start" };
    let source = if is_confirm {
        auto_pick::SOURCE_SPEC_REVIEW_CONFIRM
    } else {
        auto_pick::SOURCE_SPEC_REVIEW_START
    };
    // Same guard as `record`: a run must not review its own spec.
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!("{command} refuses to run with THOTH_RUN_ID set: a run does not start a review of its own spec");
    }
    // RJ-1: `auto` is refused before anything else.
    auto_pick::refuse_auto(role::REVIEWER, args.task, args.profile, args.model, args.effort)?;
    let effort_given = args
        .effort
        .map(crate::adapter::effort::parse_level)
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?;
    let reason = auto_pick::clean_reason(args.reason)?;
    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, args.task)?;
    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    refuse_status(&task_id, &task.status, mode)?;
    let refusal = if is_confirm {
        format!("refusing to start the author check of task {task_id}")
    } else {
        format!("refusing to start a spec review of task {task_id}")
    };
    // The cheap pre-check; the authoritative one is inside the transaction.
    if let Some(live) = state::list_live_runs_for_task(&conn, &task_id)?.into_iter().next() {
        bail!("{}", crate::commands::live_run_refusal(&refusal, &task_id, &live));
    }
    // SR-3: the author's check needs edits to check, proposed for this spec.
    let edit_review: Option<Newest> = if is_confirm {
        let st = state(&conn, &task)?;
        if !st.pending_edits {
            bail!("{}", no_pending_edits(&task_id, &st));
        }
        st.newest
    } else {
        None
    };
    let proposed_spec: Option<String> = edit_review.as_ref().and_then(|n| n.proposed_spec.clone());
    if is_confirm && proposed_spec.is_none() {
        bail!(
            "task {task_id}'s spec review {} carries no proposed spec; run `thoth spec review start \
             {task_id} --edit ...` again",
            edit_review.as_ref().map(|n| n.spec_review_id.as_str()).unwrap_or("?")
        );
    }
    // The same lint as `task create`, on the spec as stored now (the proposed
    // spec for an author's check): the catalogue or the main tree may have
    // moved since the task was created.
    let lint_target = proposed_spec.as_deref().unwrap_or(&task.spec);
    let lint = crate::commands::spec_lint::for_repo(&conn, &root, lint_target)?;
    if let Some(blocked) = lint.refusal() {
        bail!("{blocked}");
    }

    let gconn = db::open_global(paths)?;
    let profile = match global::get_profile(&gconn, args.profile)? {
        Some(p) => p,
        None => bail!("no such profile: {}", args.profile),
    };
    crate::commands::profile::require_supported_harness(
        &format!("profile {}", profile.profile_id),
        &profile.harness,
    )?;
    let adapter = crate::adapter::for_harness(&profile.harness);
    // MODELS-1: what a model-rule refusal lists (the stored list).
    let known_models = crate::model_list::known_models_text(&gconn, &profile);
    // RJ-1: a spec review continues no attempt, so the model and effort are
    // given every time.
    let choice = auto_pick::choose(
        &gconn,
        &auto_pick::Request {
            task_id: &task_id,
            role: role::REVIEWER,
            model: args.model,
            effort: effort_given,
            previous: None,
            retry: auto_pick::Retry::EveryTime,
        },
        &profile,
        |m| adapter.check_model(&profile.profile_id, m, &known_models),
    )?;
    let require_reason = crate::commands::settings::require_reason(&paths.home);
    auto_pick::require_reason_gate(&task_id, command, reason.as_deref(), require_reason)?;
    // FC-1 (H4): read only to see whether the profile has recovered; an
    // unreadable ranking does not stop the spec review.
    let recovered = crate::commands::usage::ranked_candidates(
        paths,
        role::REVIEWER,
        Some(&task_id),
        Some(&choice.model),
    )
    .unwrap_or_default()
    .iter()
    .find(|c| c.profile.profile_id == profile.profile_id)
    .is_some_and(|c| c.recovered && c.profile.status == "rate_limited");
    let allowed_tools =
        adapter.reviewer_allowed_tools(crate::commands::review::REVIEWER_ALLOWED_TOOLS, false);
    let launch = auto_pick::launch_settings(&choice, &profile);
    let run_spec = crate::adapter::effort::with_launch(&task.spec, &launch)?;
    let (_probed, auth) = crate::commands::profile::probe_auth(paths, &profile.profile_id)?;
    if !auth.logged_in {
        bail!(
            "profile {} failed its auth check; run `thoth profile check {}`; to log in again, run \
             `thoth profile login {}`",
            profile.profile_id,
            profile.profile_id,
            profile.profile_id
        );
    }
    plan_label::remember_or_warn(&gconn, &profile.profile_id, auth.subscription_type.as_deref());
    crate::login_name::remember_or_warn(&gconn, &profile.profile_id, auth.email.as_deref());
    // MODELS-1: store the list just read, then check the model against it
    // (agy) or against the account's recorded unavailable models (Claude),
    // before the worktree and the run row.
    let fresh_models = crate::model_list::remember_or_warn(&gconn, &profile.profile_id, &auth);
    let model_warning =
        crate::model_list::check_launch(&gconn, &profile, &choice.model, fresh_models.as_ref())?;
    let logged_in_again = profile.status == "logged_out";

    // The spec as it is now, and the code the worker will see: it merges the
    // branch it started from first, so the review is against the HEAD of the
    // worktree the command runs in when that HEAD contains the task's base.
    // Otherwise that HEAD is on another line (main's, for a task on a
    // long-lived branch) and the base itself is the closest code.
    let spec_sha256 = fingerprint(&task.spec);
    let caller_head = worktree::rev_parse_head(cwd)?;
    let reviewed_commit = if worktree::is_ancestor(cwd, &task.base_commit, &caller_head)? {
        caller_head
    } else {
        task.base_commit.clone()
    };
    let wt = spec_review_worktree_path(paths, &repo_id, &task_id);
    let ensured = worktree::ensure_detached_worktree(&root, &wt, &reviewed_commit)?;
    let worktree_reused = ensured.outcome == worktree::EnsureOutcome::Reused;
    // A reused worktree stays where the previous review left it.
    let worktree_moved = worktree_reused && worktree::rev_parse_head(&wt)? != reviewed_commit;
    if worktree_moved {
        worktree::checkout_detached(&wt, &reviewed_commit)?;
    }

    // A previous reviewer's report; deleted in the transaction below, under
    // the authoritative live-run check.
    let stale_report = wt.join(spec_review_prompt::OUTPUT_FILE);

    let run_id = ids::new_run_id();
    let run_dir = paths.run_dir(&repo_id, &run_id);
    std::fs::create_dir_all(&run_dir)?;
    let run = Run {
        run_id: run_id.clone(),
        task_id: task_id.clone(),
        // The attempt the next dispatch will use, so a rework's attempt is
        // not bumped by a spec review.
        attempt: crate::commands::dispatch::next_attempt(state::max_attempt(&conn, &task_id)?, false),
        role: role::SPEC_REVIEWER.to_string(),
        provider: profile.provider.clone(),
        account: profile.profile_id.clone(),
        model: choice.model.clone(),
        harness_session_id: if adapter.names_session_late() {
            None
        } else {
            Some(ids::new_session_uuid())
        },
        config_dir: profile.config_dir.clone(),
        allowed_tools: allowed_tools.clone(),
        worktree: wt.display().to_string(),
        base_commit: reviewed_commit.clone(),
        candidate_commit: None,
        spec: run_spec,
        status: RunStatus::Queued.as_str().to_string(),
        interrupted_reason: None,
        exit_code: None,
        input_tokens: None,
        output_tokens: None,
        started_at: None,
        ended_at: None,
        runner_pid: None,
        worker_pid: None,
        heartbeat_at: None,
        cancel_requested: 0,
        timeout_seconds: args.timeout_seconds as i64,
        run_dir: run_dir.display().to_string(),
        error_text: None,
        harness_bin: Some(crate::adapter::harness_bin(&profile.harness)),
    };

    let tx = db::write_tx(&conn)?;
    let current = state::get_task(&tx, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    if !spec_unchanged(&spec_sha256, &current.spec) {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "task {task_id}'s spec changed while the spec review was being started (sha256 {} -> \
             {}); start again",
            short(&spec_sha256),
            short(&fingerprint(&current.spec))
        );
    }
    if let Err(e) = refuse_status(&task_id, &current.status, mode) {
        drop(tx);
        return Err(e);
    }
    if let Some(live) = state::list_live_runs_for_task(&tx, &task_id)?.into_iter().next() {
        drop(tx);
        bail!("{}", crate::commands::live_run_refusal(&refusal, &task_id, &live));
    }
    if let Some(edit) = &edit_review {
        // A `spec review record` between the read and here would make the
        // author check edits that are no longer the newest word.
        let st = state(&tx, &current)?;
        if !st.pending_edits || st.newest.as_ref().map(|n| n.spec_review_id.as_str()) != Some(&edit.spec_review_id) {
            drop(tx);
            bail!(
                "task {task_id}'s newest spec review changed while the author check was being started; \
                 run `thoth spec review show {task_id}` and start again"
            );
        }
    }
    // Fails closed, like `review start`: a stale report the runner could
    // ingest as this reviewer's is a reason not to start.
    let cleared_stale_report = match std::fs::remove_file(&stale_report) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            drop(tx);
            bail!(
                "could not clear the previous spec review report at {}: {e}; refusing to start a \
                 spec review that could ingest it",
                stale_report.display()
            );
        }
    };
    state::insert_run(&tx, &run)?;
    let mut started = serde_json::json!({
        "spec_sha256": spec_sha256,
        "reviewed_commit": reviewed_commit,
        "task_base_commit": current.base_commit,
        "risk": current.risk,
        "task_status": current.status,
        "worktree": wt.display().to_string(),
        "worktree_reused": worktree_reused,
        "worktree_moved": worktree_moved,
        "cleared_stale_report": cleared_stale_report,
        "profile": profile.profile_id,
        "model": choice.model,
        "allowed_tools": allowed_tools,
        "timeout_seconds": args.timeout_seconds,
        "mode": mode.as_str(),
    });
    if let (Some(edit), Some(o)) = (&edit_review, started.as_object_mut()) {
        o.insert(
            "confirms_spec_review_id".to_string(),
            serde_json::json!(edit.spec_review_id),
        );
        o.insert("proposed_sha256".to_string(), serde_json::json!(edit.proposed_sha256));
    }
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: Some(run_id.clone()),
            event_type: event_type::SPEC_REVIEW_STARTED.to_string(),
            detail: started,
        },
    )?;
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: Some(run_id.clone()),
            event_type: event_type::LAUNCH_CHOICE.to_string(),
            detail: auto_pick::LaunchChoice {
                role: role::REVIEWER,
                source,
                task_spec: &task.spec,
                risk: &task.risk,
                profile: &profile.profile_id,
                choice: &choice,
                launch: &launch,
                previous: None,
                reason: reason.as_deref(),
                require_reason,
            }
            .detail(),
        },
    )?;
    tx.commit()?;

    let mut warnings: Vec<String> = Vec::new();
    warnings.extend(model_warning);
    warnings.extend(ensured.warning);
    if let Some(note) = launch.effort_note.clone() {
        warnings.push(note);
    }
    if recovered {
        if let Err(e) = auto_pick::mark_recovered(&gconn, &profile.profile_id, &task_id, &run_id) {
            warnings.push(format!(
                "profile {} was picked because its window reset, but marking it active again \
                 failed: {e}; run `thoth profile check {}`",
                profile.profile_id, profile.profile_id
            ));
        }
    }
    if logged_in_again {
        if let Err(e) = auto_pick::mark_logged_in_again(
            &gconn,
            &profile.profile_id,
            &task_id,
            &run_id,
            source,
        ) {
            warnings.push(format!(
                "profile {} passed its auth check, but marking it active again failed: {e}; run \
                 `thoth profile check {}`",
                profile.profile_id, profile.profile_id
            ));
        }
    }

    let runner_pid = crate::commands::dispatch::spawn_runner(&root, &run_id)?;
    if state::set_runner_pid(&conn, &run_id, runner_pid as i64).is_err() {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Err(e) = state::set_runner_pid(&conn, &run_id, runner_pid as i64) {
            warnings.push(format!(
                "spec reviewer started (pid {runner_pid}) but recording runner_pid failed: {e}; \
                 cancel escalation will need the pid manually"
            ));
        }
    }

    warnings.extend(lint.warning_lines());
    if !args.json {
        for w in &warnings {
            masked_eprintln!("warning: {w}");
        }
    }
    let required_before_dispatch = required(&conn, &current)?;
    let mut v = serde_json::json!({
        "ok": true,
        "run_id": run_id,
        "task_id": task_id,
        "spec_sha256": spec_sha256,
        "reviewed_commit": reviewed_commit,
        "task_base_commit": current.base_commit,
        "risk": current.risk,
        "task_status": current.status,
        "required_before_dispatch": required_before_dispatch,
        "profile": profile.profile_id,
        "model": choice.model,
        "worktree": wt.display().to_string(),
        "worktree_reused": worktree_reused,
        "worktree_moved": worktree_moved,
        "cleared_stale_report": cleared_stale_report,
        "allowed_tools": allowed_tools,
        "runner_pid": runner_pid,
        "run_dir": run_dir.display().to_string(),
        "effort_requested": launch.effort_requested,
        "effort": launch.effort,
        "effort_note": launch.effort_note,
        "warnings": warnings,
        "mode": mode.as_str(),
    });
    if let (Some(edit), Some(o)) = (&edit_review, v.as_object_mut()) {
        o.insert(
            "confirms_spec_review_id".to_string(),
            serde_json::json!(edit.spec_review_id),
        );
        o.insert("proposed_sha256".to_string(), serde_json::json!(edit.proposed_sha256));
    }
    let what = match mode {
        ReviewMode::Review => "spec review",
        ReviewMode::Edit => "spec review (edit mode)",
        ReviewMode::Confirm => "author check",
    };
    output::emit(
        args.json,
        &v,
        &format!(
            "{what} {run_id} started on {} (task {task_id}, spec sha256 {}, commit {}, profile \
             {}, runner pid {runner_pid})\nthe task stays {}; block on it with `thoth wait --run \
             {run_id}`, then `thoth spec review show {task_id}`",
            choice.model,
            short(&spec_sha256),
            reviewed_commit.get(..7).unwrap_or(&reviewed_commit),
            plan_label::named(
                &profile.profile_id,
                plan_label::labels(&gconn).get(&profile.profile_id).map(String::as_str),
                Lang::En,
            ),
            current.status
        ),
    );
    Ok(0)
}

/// What the runner's ingest of a spec reviewer's report produced.
pub struct SpecReviewIngest {
    pub outcome: IngestOutcome,
    /// `None` unless a valid `spec_review` event was written.
    pub verdict: Option<SpecVerdict>,
}

/// One `spec_review_invalid` event for a spec reviewer run. Never fails: a
/// failed write is logged in the run dir.
fn run_invalid(
    conn: &Connection,
    run: &Run,
    source: &str,
    stage: &str,
    reason: String,
    expected: Option<Vec<String>>,
) -> SpecReviewIngest {
    let mut detail = serde_json::json!({
        "stage": stage,
        "reason": reason,
        "source": source,
        "reviewer_run_id": run.run_id,
    });
    if let (Some(expected), Some(o)) = (expected, detail.as_object_mut()) {
        o.insert("expected".to_string(), serde_json::json!(expected));
    }
    if let Err(e) = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::SPEC_REVIEW_INVALID.to_string(),
            detail,
        },
    ) {
        crate::runner::log_runner_error(
            Path::new(&run.run_dir),
            &format!("recording spec_review_invalid for run {} failed: {e}", run.run_id),
        );
    }
    SpecReviewIngest {
        outcome: IngestOutcome::Invalid { reason },
        verdict: None,
    }
}

/// The runner's ingest of `.agent/spec-review.json` at `path`. `root` is the
/// repository's main work tree, for the lint of a proposed spec.
pub fn ingest_run_path(
    conn: &Connection,
    root: &Path,
    run: &Run,
    task: &state::Task,
    path: &Path,
) -> SpecReviewIngest {
    match std::fs::read_to_string(path) {
        Ok(text) => ingest_run(conn, root, run, task, &text),
        Err(e) => {
            let source = match started_detail_for(conn, &run.run_id) {
                Ok(Some(d)) if mode_of(&d) == ReviewMode::Confirm => SOURCE_AUTHOR_CHECK,
                _ => SOURCE_REVIEWER_RUN,
            };
            run_invalid(
                conn,
                run,
                source,
                "read",
                format!("could not read spec-review.json at {}: {e}", path.display()),
                None,
            )
        }
    }
}

/// Validates a spec reviewer's document exactly as `record` does, plus the
/// spec it was started against, and writes one `spec_review` event on the
/// run. Never touches `tasks.status`. SR-3: in edit mode the document's
/// edits are applied and the proposed spec linted first; an author's check
/// (`mode` `confirm`) goes to `ingest_confirm`.
pub fn ingest_run(
    conn: &Connection,
    root: &Path,
    run: &Run,
    task: &state::Task,
    json_text: &str,
) -> SpecReviewIngest {
    let started_detail = match started_detail_for(conn, &run.run_id) {
        Ok(Some(d)) if d["spec_sha256"].is_string() => d,
        _ => {
            return run_invalid(
                conn,
                run,
                SOURCE_REVIEWER_RUN,
                "started",
                "the run's spec_review_started event could not be read".to_string(),
                None,
            )
        }
    };
    let mode = mode_of(&started_detail);
    if mode == ReviewMode::Confirm {
        return ingest_confirm(conn, root, run, task, &started_detail, json_text);
    }
    let started = started_detail["spec_sha256"].as_str().unwrap_or_default().to_string();
    let invalid = |stage: &str, reason: String, expected: Option<Vec<String>>| {
        run_invalid(conn, run, SOURCE_REVIEWER_RUN, stage, reason, expected)
    };
    let (doc, edits) = if mode == ReviewMode::Edit {
        match spec_edits::parse_edit_document(strip_bom(json_text)) {
            Ok(d) => (d.review, d.edits),
            Err(e) => return invalid("schema", e.to_string(), None),
        }
    } else {
        match spec_review_json::parse_spec_review(strip_bom(json_text)) {
            Ok(d) => (d, Vec::new()),
            Err(e) => return invalid("schema", e.to_string(), None),
        }
    };
    let parsed = match spec::parse_spec(&task.spec) {
        Ok(p) => p,
        Err(e) => {
            return invalid(
                "spec",
                format!("the task spec does not parse, so its criteria cannot be checked: {e}"),
                None,
            )
        }
    };
    let expected = review_json::ac_ids(&parsed);
    if let Err(e) = spec_review_json::check_coverage(&doc, &expected) {
        return invalid("coverage", e.to_string(), Some(expected));
    }
    if !spec_unchanged(&started, &task.spec) {
        return invalid(
            "spec_changed",
            format!(
                "the task's spec changed while the spec reviewer was running (sha256 {} -> {}); \
                 the review judged the old spec",
                short(&started),
                short(&fingerprint(&task.spec))
            ),
            None,
        );
    }
    // SR-3: the edits, applied; the proposed spec has to pass the lint
    // `update-spec` would run on it.
    let summary = if edits.is_empty() {
        EditsSummary::none(&task.spec)
    } else {
        let applied = match spec_edits::apply_edits(&task.spec, &edits, &expected) {
            Ok(a) => a,
            Err(e) => return invalid("edits", e.to_string(), None),
        };
        match crate::commands::spec_lint::for_repo(conn, root, &applied.proposed) {
            Ok(report) => {
                if let Some(refusal) = report.refusal() {
                    return invalid("lint", format!("the proposed spec is refused: {refusal}"), None);
                }
            }
            Err(e) => return invalid("lint", format!("linting the proposed spec failed: {e:#}"), None),
        }
        EditsSummary::from_applied(&task.spec, &applied)
    };
    let verdict = SpecVerdict::parse(&doc.verdict)
        .expect("the document parser accepted only APPROVE or REVISE");
    let written = (|| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        let before = state(&tx, task)?;
        let revise_streak = if verdict == SpecVerdict::Revise {
            before.revise_streak + 1
        } else {
            0
        };
        let spec_review_id = ids::new_review_id();
        let finding_count: usize = doc.criteria.iter().map(|c| c.findings.len()).sum();
        let mut document = serde_json::to_value(&doc)?;
        if mode == ReviewMode::Edit {
            if let Some(o) = document.as_object_mut() {
                o.insert("edits".to_string(), serde_json::to_value(&edits)?);
            }
        }
        state::append_event(
            &tx,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::SPEC_REVIEW.to_string(),
                detail: review_detail(&ReviewEvent {
                    spec_review_id: &spec_review_id,
                    verdict,
                    spec_sha256: &started,
                    task,
                    criteria: criteria_summary(&doc),
                    findings: finding_count,
                    revise_streak,
                    source: SOURCE_REVIEWER_RUN,
                    reviewer_run_id: Some(&run.run_id),
                    document,
                    mode: mode.as_str(),
                    edits: &summary,
                    confirm: None,
                }),
            },
        )?;
        tx.commit()?;
        Ok(())
    })();
    if let Err(e) = written {
        return invalid("write", format!("recording the spec review failed: {e:#}"), None);
    }
    let edited = if summary.edit_count > 0 {
        format!(" with {} edit(s)", summary.edit_count)
    } else {
        String::new()
    };
    // A review writes no `result_ingested`, so its run note keeps this text.
    let note = format!("{} over {} criteria{edited}", verdict.as_str(), doc.criteria.len());
    SpecReviewIngest {
        outcome: IngestOutcome::Ok {
            summary: note.clone(),
            claim_count: 0,
            brief: note,
        },
        verdict: Some(verdict),
    }
}

/// SR-3: the runner's ingest of an author's check. The checks that depend on
/// the task's state are made again inside the write transaction (an
/// `IMMEDIATE` one, so no `update-spec` or `record` lands between them and
/// the write). APPROVE replaces the spec and writes `spec_updated` plus the
/// APPROVE; REVISE writes the REVISE only. `tasks.status` is never written.
fn ingest_confirm(
    conn: &Connection,
    root: &Path,
    run: &Run,
    task: &state::Task,
    started: &serde_json::Value,
    json_text: &str,
) -> SpecReviewIngest {
    let invalid = |stage: &str, reason: String| run_invalid(conn, run, SOURCE_AUTHOR_CHECK, stage, reason, None);
    let Some(confirms) = started["confirms_spec_review_id"].as_str().map(str::to_string) else {
        return invalid(
            "started",
            "the run's spec_review_started event names no confirms_spec_review_id".to_string(),
        );
    };
    let started_sha = started["spec_sha256"].as_str().unwrap_or_default().to_string();
    let doc = match spec_edits::parse_confirm_document(strip_bom(json_text)) {
        Ok(d) => d,
        Err(e) => return invalid("schema", e.to_string()),
    };
    let edit_review = match find_review(conn, &task.task_id, &confirms) {
        Ok(Some(n)) => n,
        Ok(None) => {
            return invalid(
                "superseded",
                format!("the edit review {confirms} this author check judges is not on the task"),
            )
        }
        Err(e) => return invalid("superseded", format!("reading the edit review {confirms} failed: {e:#}")),
    };
    if let Err(e) = spec_edits::check_confirm_coverage(&doc, edit_review.edit_count) {
        return invalid("coverage", e.to_string());
    }
    let Some(proposed) = edit_review.proposed_spec.clone() else {
        return invalid(
            "superseded",
            format!("the edit review {confirms} carries no proposed spec"),
        );
    };
    let verdict = SpecVerdict::parse(&doc.verdict).expect("parse_confirm_document accepted only APPROVE or REVISE");
    let proposed_sha256 = fingerprint(&proposed);
    // APPROVE: what `update-spec` checks before it writes.
    let proposed_parsed = if verdict == SpecVerdict::Approve {
        match crate::commands::spec_lint::for_repo(conn, root, &proposed) {
            Ok(report) => {
                if let Some(refusal) = report.refusal() {
                    return invalid("lint", format!("the proposed spec is refused: {refusal}"));
                }
            }
            Err(e) => return invalid("lint", format!("linting the proposed spec failed: {e:#}")),
        }
        match spec::parse_spec(&proposed) {
            Ok(p) => Some(p),
            Err(e) => return invalid("lint", format!("the proposed spec does not parse: {e}")),
        }
    } else {
        None
    };
    let ac_ids: Vec<String> = match spec::parse_spec(&task.spec) {
        Ok(p) => review_json::ac_ids(&p),
        Err(e) => {
            return invalid(
                "spec",
                format!("the task spec does not parse, so its criteria cannot be listed: {e}"),
            )
        }
    };
    // The criteria summary: on REVISE the criterion a refused edit serves is
    // `revise`, with one finding per refused edit; on APPROVE all `ok`.
    let refused: Vec<usize> = doc
        .edits
        .iter()
        .filter(|e| e.verdict.trim() == "revise")
        .map(|e| e.index)
        .collect();
    let criterion_of = |index: usize| -> Option<String> {
        edit_review
            .edits
            .as_array()
            .into_iter()
            .flatten()
            .find(|e| e["index"].as_u64() == Some(index as u64))
            .and_then(|e| e["criterion"].as_str().map(str::to_string))
    };
    let criteria: Vec<serde_json::Value> = ac_ids
        .iter()
        .map(|id| {
            let findings = refused
                .iter()
                .filter(|i| criterion_of(**i).as_deref() == Some(id.as_str()))
                .count();
            serde_json::json!({
                "id": id,
                "verdict": if findings > 0 { "revise" } else { "ok" },
                "findings": findings,
            })
        })
        .collect();
    let findings = refused.len();
    let document = match serde_json::to_value(&doc) {
        Ok(v) => v,
        Err(e) => return invalid("write", format!("serializing the document failed: {e}")),
    };

    // Inside the transaction: the task as it is now, the two checks again,
    // then the writes. `Err(Ok(..))` is a refusal with its stage.
    type Refusal = (&'static str, String);
    let written: Result<Result<(), Refusal>> = (|| {
        let tx = db::write_tx(conn)?;
        let current = state::get_task(&tx, &task.task_id)?
            .ok_or_else(|| anyhow::anyhow!("task {} disappeared", task.task_id))?;
        if !spec_unchanged(&started_sha, &current.spec) {
            return Ok(Err((
                "spec_changed",
                format!(
                    "the task's spec changed while the author check was running (sha256 {} -> {}); \
                     the edits were proposed for the old spec",
                    short(&started_sha),
                    short(&fingerprint(&current.spec))
                ),
            )));
        }
        let before = state(&tx, &current)?;
        let newest_id = before.newest.as_ref().map(|n| n.spec_review_id.clone()).unwrap_or_default();
        if newest_id != confirms {
            return Ok(Err((
                "superseded",
                format!(
                    "the newest spec review is now {newest_id}, not the edit review {confirms} this \
                     author check judges"
                ),
            )));
        }
        let summary = EditsSummary::from_review(&edit_review, current.spec.len());
        let spec_review_id = ids::new_review_id();
        let now = ids::now_rfc3339();
        let (spec_sha256, revise_streak, applied) = match (verdict, &proposed_parsed) {
            (SpecVerdict::Approve, Some(parsed)) => {
                if !state::set_task_spec(
                    &tx,
                    &current.task_id,
                    UPDATE_SPEC_STATUSES,
                    &proposed,
                    &parsed.risk,
                    &now,
                )? {
                    return Ok(Err((
                        "status",
                        format!(
                            "task {} is '{}'; the author's APPROVE replaces the spec, which needs draft, \
                             queued, reject or unverified",
                            current.task_id, current.status
                        ),
                    )));
                }
                let (task_type, difficulty) = spec::labels(&proposed);
                let old = spec::parse_spec(&current.spec).ok();
                state::append_event(
                    &tx,
                    &NewEvent {
                        task_id: Some(current.task_id.clone()),
                        run_id: Some(run.run_id.clone()),
                        event_type: event_type::SPEC_UPDATED.to_string(),
                        detail: serde_json::json!({
                            "risk_from": current.risk,
                            "risk_to": parsed.risk,
                            "goal_changed": old.as_ref().map(|b| b.goal != parsed.goal).unwrap_or(true),
                            "acceptance_criteria_from": old.as_ref().map(|b| b.acceptance_criteria.len()),
                            "acceptance_criteria_to": parsed.acceptance_criteria.len(),
                            "bytes": proposed.len(),
                            "task_type": task_type,
                            "difficulty": difficulty,
                            "spec_sha256": proposed_sha256,
                            "source": SOURCE_AUTHOR_CHECK,
                            "spec_review_id": spec_review_id,
                            "confirms_spec_review_id": confirms,
                        }),
                    },
                )?;
                (proposed_sha256.clone(), 0, true)
            }
            _ => (started_sha.clone(), before.revise_streak + 1, false),
        };
        let mut after = current.clone();
        if applied {
            // The event records the spec as it is after the write.
            after.spec = proposed.clone();
        }
        state::append_event(
            &tx,
            &NewEvent {
                task_id: Some(current.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::SPEC_REVIEW.to_string(),
                detail: review_detail(&ReviewEvent {
                    spec_review_id: &spec_review_id,
                    verdict,
                    spec_sha256: &spec_sha256,
                    task: &after,
                    criteria: criteria.clone(),
                    findings,
                    revise_streak,
                    source: SOURCE_AUTHOR_CHECK,
                    reviewer_run_id: Some(&run.run_id),
                    document: document.clone(),
                    mode: ReviewMode::Confirm.as_str(),
                    edits: &summary,
                    confirm: Some(ConfirmKeys {
                        confirms_spec_review_id: &confirms,
                        spec_sha256_before: &started_sha,
                        applied,
                    }),
                }),
            },
        )?;
        tx.commit()?;
        Ok(Ok(()))
    })();
    match written {
        Err(e) => invalid("write", format!("recording the author check failed: {e:#}")),
        Ok(Err((stage, reason))) => invalid(stage, reason),
        Ok(Ok(())) => {
            let note = format!(
                "author {} over {} edit(s){}",
                verdict.as_str(),
                edit_review.edit_count,
                if verdict == SpecVerdict::Approve { ", spec replaced" } else { "" }
            );
            SpecReviewIngest {
                outcome: IngestOutcome::Ok { summary: note.clone(), claim_count: 0, brief: note },
                verdict: Some(verdict),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The `mode`, `edit_count` and `proposed_sha256` that `show --json` reads
    // out of an event, and the folded, clipped `after` of its human edit
    // lines, are checked through the CLI in tests/spec_review_edit.rs
    // (`a_review_mode_run_and_a_recorded_review_show_their_mode_without_edits`,
    // `show_folds_and_clips_a_long_edit_text_on_its_human_line`; an event
    // written before SR-3, without `mode`:
    // `an_event_without_a_mode_reads_its_mode_from_its_source`).

    #[test]
    fn spec_unchanged_compares_the_fingerprint_of_the_stored_bytes() {
        let spec = r#"{"goal":"guard the ledger","risk":"important"}"#;
        assert!(spec_unchanged(&fingerprint(spec), spec));
        assert!(!spec_unchanged(&fingerprint(spec), &format!("{spec} ")));
        assert!(spec_unchanged(&fingerprint(""), ""));
    }

    #[test]
    fn write_inputs_mask_strings_without_breaking_json() {
        let tmp = tempfile::tempdir().unwrap();
        let wt = tmp.path();
        let dir = wt.join(spec_review_prompt::INPUT_DIR);
        let value = "dummy\"pass\\word-1";
        let escaped = serde_json::to_string(value).unwrap();
        let escaped = &escaped[1..escaped.len() - 1];
        let masker = Masker::from_pairs(&[(value, "env:RD_UNIT_PASSWORD")]);
        let key_header = ["-----BEGIN ", "RSA PRIVATE KEY-----"].concat();
        let spec = serde_json::to_string_pretty(&serde_json::json!({
            "goal": format!("ship it with {value} set"),
            "acceptance_criteria": ["AC1: works"],
            "constraints": [format!("see {key_header}\nAAAAdummy"), "plain constraint"],
            "risk": "high",
        }))
        .unwrap();
        let absent = |text: &str| {
            assert!(!text.contains(value), "{text}");
            assert!(!text.contains(escaped), "{text}");
        };

        let counts = write_input(&spec, wt, &masker).unwrap();
        let text = std::fs::read_to_string(dir.join(spec_review_prompt::INPUT_FILE)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(v["goal"].as_str().unwrap().contains("[redacted:env:RD_UNIT_PASSWORD]"), "{text}");
        assert!(v["constraints"][0].as_str().unwrap().contains("[redacted:private-key]"), "{text}");
        assert_eq!(v["constraints"][1], "plain constraint");
        let goal_at = text.find("\n  \"goal\":").expect("goal key");
        let ac_at = text.find("\n  \"acceptance_criteria\":").expect("acceptance_criteria key");
        assert!(text.starts_with("{\n  \"goal\":"), "{text}");
        assert!(goal_at < ac_at, "{text}");
        assert!(!text[goal_at + 1..ac_at].contains("\n  \""), "nothing between goal and acceptance_criteria: {text}");
        absent(&text);
        assert_eq!(
            counts.to_json(),
            serde_json::json!({"env:RD_UNIT_PASSWORD": 1, "private-key": 1})
        );

        let edits = serde_json::json!([{"field": "goal", "op": "replace", "text": format!("use {value}")}]);
        let counts = write_confirm_input(wt, &spec, &edits, &masker).unwrap();
        for file in [spec_review_prompt::PROPOSED_FILE, spec_review_prompt::EDITS_FILE] {
            let text = std::fs::read_to_string(dir.join(file)).unwrap();
            serde_json::from_str::<serde_json::Value>(&text).unwrap();
            absent(&text);
            assert!(text.contains("[redacted:env:RD_UNIT_PASSWORD]"), "{file}: {text}");
        }
        assert_eq!(
            counts.to_json(),
            serde_json::json!({"env:RD_UNIT_PASSWORD": 2, "private-key": 1})
        );

        let clean = "{ \"risk\": \"low\",\n\n   \"goal\":  \"g\" , \"acceptance_criteria\": [\"AC1: x\"] }";
        let clean_edits = serde_json::json!([{"field": "goal", "op": "replace", "text": "plain"}]);
        assert!(write_input(clean, wt, &masker).unwrap().is_empty());
        assert_eq!(std::fs::read(dir.join(spec_review_prompt::INPUT_FILE)).unwrap(), clean.as_bytes());
        assert!(write_confirm_input(wt, clean, &clean_edits, &masker).unwrap().is_empty());
        assert_eq!(std::fs::read(dir.join(spec_review_prompt::PROPOSED_FILE)).unwrap(), clean.as_bytes());
        assert_eq!(
            std::fs::read(dir.join(spec_review_prompt::EDITS_FILE)).unwrap(),
            serde_json::to_string_pretty(&clean_edits).unwrap().as_bytes()
        );

        let not_json = format!("goal: {value}\n{{ not json");
        write_input(&not_json, wt, &masker).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(spec_review_prompt::INPUT_FILE)).unwrap(),
            masker.mask_text(&not_json).0
        );
    }
}
