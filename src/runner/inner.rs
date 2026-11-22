use crate::config::Paths;
use crate::db::state;
use super::{attempts, candidate, conclude, verdict};
use super::{prepare, prompt, review_inputs};
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

pub(super) fn run_worker_inner(
    paths: &Paths,
    conn: &Connection,
    repo_id: &str,
    repo_root: &Path,
    run: &state::Run,
) -> Result<()> {
    let (prep, addenda) = prepare::prepare_run(paths, conn, run)?;
    let inputs = review_inputs::prepare_review_inputs(conn, repo_root, run, &prep)?;
    let built = prompt::build_run_prompt(conn, run, &prep, addenda, &inputs)?;
    let Some(launched) = attempts::launch_and_watch(paths, conn, repo_id, run, &prep, &inputs, built)? else {
        // already settled by the DB poll give-up or the kill-wait deadline
        return Ok(());
    };
    let candidate = candidate::record_candidate(paths, conn, repo_id, repo_root, run, &prep, &launched);
    let ingested = verdict::ingest_result(
        paths,
        conn,
        repo_id,
        repo_root,
        run,
        &prep,
        inputs.code_review_mode,
        &launched,
        &candidate,
    );
    conclude::conclude_run(paths, conn, repo_id, repo_root, run, &prep, launched, candidate, ingested)
}
