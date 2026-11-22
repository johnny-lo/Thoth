use crate::config::Paths;
use crate::db::state;
use crate::ingest::{self, IngestOutcome};
use super::attempts::Launched;
use super::candidate::CandidateEvidence;
use super::prepare::RunPrep;
use super::settle::{log_runner_error, task_risk_for_ingest};
use rusqlite::Connection;
use std::path::Path;

/// What the sixth section of `run_worker_inner` ingested from the worktree:
/// the outcome and whatever a reviewer's ingest decided besides.
pub(super) struct IngestResult {
    pub(super) ingest_outcome: IngestOutcome,
    pub(super) review_verdict: Option<crate::review_json::Verdict>,
    pub(super) ingest_candidate: Option<String>,
    pub(super) review_settled: bool,
    pub(super) spec_verdict: Option<crate::spec_review_json::SpecVerdict>,
}

// The moved statements borrow `&masker`, `&run_dir` and the like as they did when these
// were owned locals; here they are already references, so the `&` is redundant and kept
// so the statements stay word for word the same.
#[allow(clippy::needless_borrow, clippy::needless_borrows_for_generic_args, clippy::too_many_arguments)]
pub(super) fn ingest_result(
    paths: &Paths,
    conn: &Connection,
    repo_id: &str,
    repo_root: &Path,
    run: &state::Run,
    prep: &RunPrep,
    code_review_mode: crate::spec_edits::ReviewMode,
    launched: &Launched<'_>,
    candidate: &CandidateEvidence,
) -> IngestResult {
    let is_review = prep.is_review;
    let is_spec_review = prep.is_spec_review;
    let run_dir = &prep.run_dir;
    let stream = &launched.stream;
    let tally = &launched.tally;
    let clean_success = candidate.clean_success;

    let mut review_verdict: Option<crate::review_json::Verdict> = None;
    // RFIX-1: the commit the ingest left the worktree at, and whether it
    // wrote (or must not touch) the task's status itself.
    let mut ingest_candidate: Option<String> = None;
    let mut review_settled = false;
    let mut spec_verdict: Option<crate::spec_review_json::SpecVerdict> = None;
    let ingest_outcome = if !clean_success {
        IngestOutcome::Skipped
    } else if is_spec_review {
        // SR-2: `.agent/spec-review.json`, validated as `spec review record`
        // validates it; a valid one is a `spec_review` event on this run.
        match state::get_task(conn, &run.task_id) {
            Ok(Some(task)) => {
                let path = Path::new(&run.worktree).join(crate::spec_review_prompt::OUTPUT_FILE);
                stream.adapter.write_review_output(&path, tally.result.as_ref(), &run_dir);
                let ingested =
                    crate::commands::spec_review::ingest_run_path(conn, repo_root, run, &task, &path);
                spec_verdict = ingested.verdict;
                ingested.outcome
            }
            _ => IngestOutcome::Invalid {
                reason: format!("task {} disappeared during the spec review", run.task_id),
            },
        }
    } else if is_review {
        // brief §5: a reviewer run ingests `.agent/review.json`, not
        // `result.json`.
        match state::get_task(conn, &run.task_id) {
            Ok(Some(task)) => {
                // The target is the implementer run this review was started
                // against, read from its own `review_started` event. When that
                // read fails or finds nothing, fall back to the task's latest
                // completed implementer run rather than lose the reviewer's
                // verdict: `reviews.target_run_id REFERENCES runs`, so an empty
                // target cannot be inserted. Only when neither lookup yields a
                // run is the target left empty, and `ingest_review` then
                // records `review_invalid` (stage "target") saying the target
                // run could not be determined - never a foreign-key error.
                // `list_reviews_for_task` still falls back from
                // `target_run_id` to `reviewer_run_id` when the former is
                // empty, defensively, in case a row shaped like this was ever
                // written by an outside tool with foreign keys off.
                let from_event = match crate::commands::review::target_run_id_for(conn, &run.run_id)
                {
                    Ok(t) => t.filter(|t| !t.is_empty()),
                    Err(e) => {
                        log_runner_error(
                            &run_dir,
                            &format!("reading this review's target run failed: {e:#}"),
                        );
                        None
                    }
                };
                let target = from_event.unwrap_or_else(|| {
                    match state::latest_completed_run_for_role(conn, &run.task_id, "implementer") {
                        Ok(Some(r)) => {
                            log_runner_error(
                                &run_dir,
                                &format!(
                                    "the review's target run was unreadable; falling back to the \
                                     latest completed implementer run {}",
                                    r.run_id
                                ),
                            );
                            r.run_id
                        }
                        _ => String::new(),
                    }
                });
                let review_path = Path::new(&run.worktree).join(".agent").join("review.json");
                // A reviewer whose harness cannot write files has its verdict
                // written here from the result line, for the unchanged ingest.
                stream.adapter.write_review_output(&review_path, tally.result.as_ref(), &run_dir);
                // EVOPEN-1: a Codex or agy reviewer's reads of its evidence
                // files, found in its own stream, before the review is judged
                // (a refused review gets them too). Not for a source check.
                if !crate::spec::is_research_task(&task.spec) {
                    if let Err(e) = crate::commands::evidence_stream::record(
                        conn,
                        run,
                        &prep.profile.harness,
                        &prep.stream_path,
                    ) {
                        log_runner_error(
                            &run_dir,
                            &format!("recording the evidence the reviewer's stream read failed: {e:#}"),
                        );
                    }
                }
                let fix_ctx = crate::commands::review_fix::Ctx { paths, repo_id, root: repo_root };
                let ingested = match (code_review_mode, std::fs::read_to_string(&review_path)) {
                    (crate::spec_edits::ReviewMode::Edit, Ok(text)) => {
                        crate::commands::review_fix::ingest_edit(conn, &fix_ctx, run, &task, &target, &text)
                    }
                    (crate::spec_edits::ReviewMode::Confirm, Ok(text)) => {
                        crate::commands::review_fix::ingest_confirm(conn, &fix_ctx, run, &text)
                    }
                    (crate::spec_edits::ReviewMode::Confirm, Err(e)) => crate::commands::review_fix::confirm_read_failed(
                        conn,
                        run,
                        format!("could not read review.json at {}: {e}", review_path.display()),
                    ),
                    _ => crate::commands::review::ingest_review_path(conn, run, &task, &target, &review_path),
                };
                if ingested.candidate_commit.is_some() {
                    ingest_candidate = ingested.candidate_commit.clone();
                }
                review_settled = ingested.settled;
                review_verdict = ingested.verdict;
                if !ingested.unopened_citations.is_empty() {
                    log_runner_error(
                        &run_dir,
                        &format!(
                            "the review cites codes it never opened: {}",
                            ingested.unopened_citations.join(", ")
                        ),
                    );
                }
                ingested.outcome
            }
            _ => IngestOutcome::Invalid {
                reason: format!("task {} disappeared during the review", run.task_id),
            },
        }
    } else {
        // `tasks.risk` decides v6 §2.5 rule 5, and it is the task row - not the
        // run's frozen spec copy - that `task update-spec` keeps current.
        // Task 5 fix round 1: unreadable means `high`, never `normal`.
        let task_risk = task_risk_for_ingest(conn, &run.task_id, &run_dir);
        ingest::ingest_path(
            conn,
            run,
            &task_risk,
            &Path::new(&run.worktree).join(".agent").join("result.json"),
        )
    };

    IngestResult { ingest_outcome, review_verdict, ingest_candidate, review_settled, spec_verdict }
}
