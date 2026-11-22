use crate::config::Paths;
use crate::db::state;
use crate::evidence;
use crate::worktree;
use super::attempts::Launched;
use super::prepare::RunPrep;
use super::settle::{log_runner_error, task_title_for_commit};
use rusqlite::Connection;
use std::path::Path;

/// What the fifth section of `run_worker_inner` worked out about the run's
/// candidate commit, for the ingest and the settle tail.
pub(super) struct CandidateEvidence {
    pub(super) clean_success: bool,
    pub(super) candidate_commit: Option<String>,
    pub(super) candidate_commit_error: Option<String>,
}

// The moved statements borrow `&masker`, `&run_dir` and the like as they did when these
// were owned locals; here they are already references, so the `&` is redundant and kept
// so the statements stay word for word the same.
#[allow(clippy::needless_borrow, clippy::needless_borrows_for_generic_args)]
pub(super) fn record_candidate(
    paths: &Paths,
    conn: &Connection,
    repo_id: &str,
    repo_root: &Path,
    run: &state::Run,
    prep: &RunPrep,
    launched: &Launched<'_>,
) -> CandidateEvidence {
    let is_codex = prep.is_codex;
    let is_review = prep.is_review;
    let run_dir = &prep.run_dir;
    let stream = &launched.stream;
    let tally = &launched.tally;
    let exit_code = launched.exit_code;
    let kill = launched.kill;

    let clean_success = kill.is_none()
        && exit_code == Some(0)
        && tally.result.as_ref().map(|r| !r.is_error).unwrap_or(false);

    // M5: a failure here is itself informative (a worktree that vanished, a
    // detached HEAD, ...) and must not be silently dropped via `.ok()`.
    let resolve_candidate = |dir: &Path| match worktree::rev_parse_head(dir) {
        Ok(c) => (Some(c), None),
        Err(e) => (None, Some(e.to_string())),
    };
    let worktree_dir = Path::new(&run.worktree);
    // scope §2.6 (E9): codex's sandbox cannot write a linked worktree's own
    // gitdir, so a codex worker never commits its own changes - thoth
    // commits whatever it left, in the clear, before the candidate commit is
    // read. A failure here settles exactly like a missing candidate always
    // has (never silently): logged, `candidate_commit` left `None`.
    let (candidate_commit, candidate_commit_error) = if is_codex && !is_review && clean_success {
        let title = task_title_for_commit(conn, &run.task_id);
        let message = format!("{title} (thoth run {})", run.run_id);
        match worktree::commit_worker_changes(worktree_dir, &message) {
            Ok(_) => resolve_candidate(worktree_dir),
            Err(e) => {
                log_runner_error(
                    &run_dir,
                    &format!("committing the codex worker's changes on its behalf failed: {e:#}"),
                );
                (None, Some(e.to_string()))
            }
        }
    } else {
        resolve_candidate(worktree_dir)
    };

    // brief §2: the diff and the commit are the control plane's own evidence,
    // added *before* ingest so the index the `result_ingested` event describes
    // is the final one and a reviewer's must-read list is already complete.
    // Only on a clean exit: a crashed or cancelled run's tree is not a
    // candidate, and a diff of it would be evidence of nothing.
    //
    // Best-effort, exactly like the per-tick drain: a git failure here is worth
    // a log line, not a lost run outcome.
    if let Some(cand) = candidate_commit.as_deref().filter(|_| !is_review) {
        if clean_success && !skip_candidate_evidence() {
            let ctx = evidence::EvidenceContext {
                paths,
                repo_id,
                task_id: &run.task_id,
                run_id: &run.run_id,
            };
            let git_dir = Path::new(&run.worktree);
            // SMALL-4: the protected list counts against the task's own base (a rework's
            // run starts from the previous candidate) and drops what equals
            // the main worktree's HEAD (brought in by a merge).
            let task_base: String = match state::get_task(conn, &run.task_id) {
                Ok(Some(t)) => t.base_commit,
                Ok(None) => {
                    log_runner_error(
                        &run_dir,
                        "task row missing; attributing the diff against the run's base_commit",
                    );
                    run.base_commit.clone()
                }
                Err(e) => {
                    log_runner_error(
                        &run_dir,
                        &format!(
                            "reading the task for the diff attribution failed: {e:#}; using the run's base_commit"
                        ),
                    );
                    run.base_commit.clone()
                }
            };
            let tip: Option<String> = match worktree::rev_parse_head(repo_root) {
                Ok(h) => Some(h),
                Err(e) => {
                    log_runner_error(
                        &run_dir,
                        &format!(
                            "reading the main worktree's HEAD for the diff attribution failed: {e:#}; merged-in changes will be counted"
                        ),
                    );
                    None
                }
            };
            let mut added = false;
            match evidence::record_diff_attributed(
                conn,
                &ctx,
                git_dir,
                &run.base_commit,
                cand,
                Some(&evidence::Attribution {
                    task_base: &task_base,
                    integration_tip: tip.as_deref(),
                }),
            ) {
                Ok(_) => added = true,
                Err(e) => log_runner_error(
                    &run_dir,
                    &format!("recording diff evidence failed: {e:#}"),
                ),
            }
            match evidence::record_commit(conn, &ctx, git_dir, cand) {
                Ok(_) => added = true,
                Err(e) => log_runner_error(
                    &run_dir,
                    &format!("recording commit evidence failed: {e:#}"),
                ),
            }
            if added {
                if let Err(e) = evidence::refresh_index(conn, &run.task_id, &stream.place.worktree) {
                    log_runner_error(
                        &run_dir,
                        &format!("rewriting the evidence index failed: {e:#}"),
                    );
                }
            }
        }
    }

    CandidateEvidence { clean_success, candidate_commit, candidate_commit_error }
}

/// Test seam, debug builds only, so a release build has no such switch and
/// ignores the variable. With `THOTH_TEST_SKIP_CANDIDATE_EVIDENCE=1` in the
/// runner's environment (inherited from the dispatching command, as
/// `THOTH_RETRY_WAITS` is), the runner records no diff and no commit evidence
/// for the candidate, so a test can show what `review record` adds itself.
#[cfg(debug_assertions)]
fn skip_candidate_evidence() -> bool {
    std::env::var("THOTH_TEST_SKIP_CANDIDATE_EVIDENCE").as_deref() == Ok("1")
}

#[cfg(not(debug_assertions))]
fn skip_candidate_evidence() -> bool {
    false
}
