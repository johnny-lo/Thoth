use super::prepare::RunPrep;
use super::settle::log_runner_error;
use crate::db::state;
use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use std::path::Path;

/// What the second section of `run_worker_inner` worked out and wrote for a
/// reviewer: the later sections read the modes and the reviewer's output files.
pub(super) struct ReviewInputs {
    pub(super) spec_review_mode: crate::spec_edits::ReviewMode,
    pub(super) code_review_mode: crate::spec_edits::ReviewMode,
    pub(super) confirm_fix_commit: Option<String>,
    pub(super) review_output: Option<crate::adapter::ReviewOutput>,
}

// The moved statements borrow `&masker`, `&run_dir` and the like as they did when these
// were owned locals; here they are already references, so the `&` is redundant and kept
// so the statements stay word for word the same.
#[allow(clippy::needless_borrow, clippy::needless_borrows_for_generic_args)]
pub(super) fn prepare_review_inputs(
    conn: &Connection,
    repo_root: &Path,
    run: &state::Run,
    prep: &RunPrep,
) -> Result<ReviewInputs> {
    let is_review = prep.is_review;
    let is_spec_review = prep.is_spec_review;
    let research = prep.research;
    let adapter = &prep.adapter;
    let masker = &prep.masker;
    let run_dir = &prep.run_dir;
    // Scope §2.4: a codex reviewer sits in a read-only sandbox where thoth
    // cannot even open the database (E3a), so its input is handed over as
    // files, and its verdict comes back as its schema-constrained final
    // message, which codex itself writes to `.agent/review.json` (E3b). Any
    // reviewer that reads the packet gets the same files; one whose harness
    // cannot write the file has it written below (`write_review_output`).
    // SR-3: how the spec reviewer run was started; `Review` for every other run.
    let mut spec_review_mode = crate::spec_edits::ReviewMode::Review;
    // RFIX-1: how a code reviewer run was started (`review start`, `--edit`
    // or `review confirm`); `Review` for every other run.
    let code_review_mode = if is_review && !is_spec_review {
        crate::commands::review_fix::run_mode(conn, &run.run_id)?
    } else {
        crate::spec_edits::ReviewMode::Review
    };
    let is_confirm = code_review_mode == crate::spec_edits::ReviewMode::Confirm;
    // A confirmer launches only for the fix it was started for, with its
    // input written fresh.
    let confirm_fix_commit = if is_confirm {
        let (counts, fix) = crate::commands::review_fix::prepare_confirm_launch(conn, repo_root, run, &masker)?;
        if let Err(e) = crate::redact::record(conn, &run.task_id, Some(&run.run_id), "review_fix_input", &counts, &[]) {
            log_runner_error(&run_dir, &format!("could not record secret_redacted: {e:#}"));
        }
        Some(fix)
    } else {
        None
    };
    let review_output = if is_spec_review {
        // SR-2: the spec as it was when the review was started, and the
        // spec-review schema; every harness gets them (Claude Code ignores
        // `review_output`, codex gets its read-only sandbox from it).
        let task = state::get_task(conn, &run.task_id)?
            .ok_or_else(|| anyhow!("task {} disappeared", run.task_id))?;
        let started = crate::commands::spec_review::started_detail_for(conn, &run.run_id)?
            .filter(|d| d["spec_sha256"].is_string());
        let Some(started) = started else {
            bail!(
                "run {} has no spec_review_started event; refusing to launch a spec reviewer without the spec it was started against",
                run.run_id
            );
        };
        let sha = started["spec_sha256"].as_str().unwrap_or_default().to_string();
        if !crate::commands::spec_review::spec_unchanged(&sha, &task.spec) {
            bail!(
                "task {}'s spec changed between spec review start and the launch (sha256 {} -> {}); start the spec review again",
                run.task_id,
                crate::commands::spec_review::short(&sha),
                crate::commands::spec_review::short(&crate::commands::spec_review::fingerprint(&task.spec))
            );
        }
        spec_review_mode = crate::commands::spec_review::mode_of(&started);
        let mut input_redactions =
            crate::commands::spec_review::write_input(&task.spec, Path::new(&run.worktree), &masker)?;
        let schema_value = match spec_review_mode {
            crate::spec_edits::ReviewMode::Review => crate::spec_review_json::output_schema(),
            crate::spec_edits::ReviewMode::Edit => crate::spec_edits::edit_output_schema(),
            crate::spec_edits::ReviewMode::Confirm => {
                // SR-3: the author judges the edit review `confirm` named,
                // which has to be the newest word on the spec still.
                let Some(confirms) = started["confirms_spec_review_id"].as_str() else {
                    bail!(
                        "run {} names no confirms_spec_review_id; refusing to launch an author check without the edits it judges",
                        run.run_id
                    );
                };
                let st = crate::commands::spec_review::state(conn, &task)?;
                let edit_review = match st.newest {
                    Some(n) if n.spec_review_id == confirms && st.pending_edits => n,
                    _ => bail!(
                        "task {}'s newest spec review is no longer {confirms} between spec review confirm and the launch; start the author check again",
                        run.task_id
                    ),
                };
                let Some(proposed) = edit_review.proposed_spec.as_deref() else {
                    bail!(
                        "spec review {confirms} carries no proposed spec; refusing to launch an author check"
                    );
                };
                input_redactions.merge(&crate::commands::spec_review::write_confirm_input(
                    Path::new(&run.worktree),
                    proposed,
                    &edit_review.edits,
                    &masker,
                )?);
                crate::spec_edits::confirm_output_schema()
            }
        };
        if let Err(e) = crate::redact::record(
            conn,
            &run.task_id,
            Some(&run.run_id),
            "spec_review_input",
            &input_redactions,
            &[],
        ) {
            log_runner_error(&run_dir, &format!("could not record secret_redacted: {e:#}"));
        }
        let schema = run_dir.join(crate::commands::spec_review::SCHEMA_FILE);
        std::fs::write(&schema, serde_json::to_string_pretty(&schema_value)?)?;
        Some(crate::adapter::ReviewOutput {
            schema,
            output: Path::new(&run.worktree).join(crate::spec_review_prompt::OUTPUT_FILE),
        })
    } else if is_confirm && adapter.reads_review_packet() {
        let schema = run_dir.join("review-schema.json");
        std::fs::write(&schema, serde_json::to_string_pretty(&crate::spec_edits::confirm_output_schema())?)?;
        Some(crate::adapter::ReviewOutput {
            schema,
            output: Path::new(&run.worktree).join(".agent").join("review.json"),
        })
    } else if is_review && adapter.reads_review_packet() {
        // RVI-1: a code reviewer also gets the delivery of the run its
        // `review_started` event names; a source check's input stays as it was.
        let target = if research {
            None
        } else {
            crate::commands::review::target_run_id_for(conn, &run.run_id)?
        };
        let redactions = crate::commands::review_packet::write_review_input(
            conn,
            &run.task_id,
            Path::new(&run.worktree),
            target.as_deref(),
            &masker,
        )?;
        if let Err(e) = crate::redact::record(
            conn,
            &run.task_id,
            Some(&run.run_id),
            "review_input",
            &redactions.counts,
            &redactions.evidence,
        ) {
            log_runner_error(&run_dir, &format!("could not record secret_redacted: {e:#}"));
        }
        let schema = run_dir.join("review-schema.json");
        std::fs::write(
            &schema,
            serde_json::to_string_pretty(&if research {
                crate::review_json::source_check_schema()
            } else if code_review_mode == crate::spec_edits::ReviewMode::Edit {
                crate::review_fix_edits::edit_output_schema()
            } else {
                crate::review_json::output_schema()
            })?,
        )?;
        Some(crate::adapter::ReviewOutput {
            schema,
            output: Path::new(&run.worktree).join(".agent").join("review.json"),
        })
    } else {
        None
    };
    Ok(ReviewInputs { spec_review_mode, code_review_mode, confirm_fix_commit, review_output })
}
