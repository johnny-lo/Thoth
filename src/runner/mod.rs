mod attempts;
mod candidate;
mod conclude;
mod dead_runner;
mod inner;
mod launch;
mod outcome;
mod prepare;
mod prompt;
mod retry;
mod review_inputs;
mod settle;
mod stream;
mod verdict;

#[cfg(test)]
mod tests;

use crate::config::Paths;
use crate::db;
use crate::db::state;
use crate::evidence;
use crate::ids;
use crate::model::RunStatus;
use crate::worktree;
use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::Duration;

// AC2 re-exports
// Tests are the only users of this one (src/runner/tests.rs through `use super::*`
// and the antigravity classify test), so it is compiled for the test build only.
#[cfg(test)]
pub use outcome::{classify_outcome, StreamTally};
pub(crate) use settle::{
    is_run_status_terminal, log_runner_error, settle_terminal, TaskStatusWrite,
};
pub(crate) use dead_runner::{runner_is_gone, settle_dead_runners};
pub(crate) use stream::append_usage;

// Re-exports and imports for tests
#[cfg(test)]
pub use outcome::{KillReason, Outcome};
#[cfg(test)]
pub use retry::{MAX_RETRIES, RETRY_WAIT_SECONDS};
#[cfg(test)]
pub(crate) use outcome::MODEL_UNAVAILABLE_PREFIX;
#[cfg(test)]
pub(crate) use retry::{parse_retry_waits, retry_launch, wait_fits, wants_retry, RetryLaunch};
#[cfg(test)]
use crate::adapter::{claude_code, HarnessAdapter, StreamEvent};
#[cfg(test)]
use crate::db::global;
#[cfg(test)]
use crate::db::state::RunFinish;
#[cfg(test)]
use crate::model::{event_type, InterruptedReason, TaskStatus};
#[cfg(test)]
use outcome::needs_login_check;
#[cfg(test)]
use settle::{finish_and_settle, record_model_unavailable, record_quota_interruption, task_risk_for_ingest};
#[cfg(test)]
use stream::note_harness_error;

use inner::run_worker_inner;
use settle::best_effort_settle_on_error;

pub(super) const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
pub(super) const POLL_EVERY: Duration = Duration::from_secs(1);

/// TEST-2: the poll interval a raw value of the test switch asks for. A
/// decimal integer from 1 to 1000 (surrounding blanks ignored) is that many
/// milliseconds; anything else - no value, empty, not a decimal integer, 0,
/// above 1000 - is `POLL_EVERY`. The switch can only shorten the poll.
pub fn parse_poll_every(raw: Option<&str>) -> Duration {
    raw.map(str::trim)
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| (1..=1000).contains(ms))
        .map(Duration::from_millis)
        .unwrap_or(POLL_EVERY)
}

/// TEST-2: the interval of the runner's main loop and of `thoth wait`.
/// Debug builds read the test-only switch `THOTH_TEST_POLL_MS` (set by
/// tests/common/mod.rs so integration tests do not wait a whole second per
/// step); release builds never read it and always poll every `POLL_EVERY`.
/// Deliberately left out of the user documentation.
pub fn poll_every() -> Duration {
    #[cfg(debug_assertions)]
    {
        let raw = std::env::var("THOTH_TEST_POLL_MS").ok();
        parse_poll_every(raw.as_deref())
    }
    #[cfg(not(debug_assertions))]
    {
        parse_poll_every(None)
    }
}

/// TEST-7b: the heartbeat interval a raw value of the test switch asks for,
/// in milliseconds. A decimal integer from 1 to 5000 (surrounding blanks
/// ignored) is that many milliseconds; anything else - no value, empty, not
/// a decimal integer, 0, above 5000 - is `None`: the switch is off.
pub fn parse_test_heartbeat_ms(raw: Option<&str>) -> Option<u64> {
    raw.map(str::trim)
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|ms| (1..=5000).contains(ms))
}

/// TEST-7b: the test-only switch `THOTH_TEST_HEARTBEAT_MS`, debug builds
/// only; release builds never read it and always get `None`. When on (N ms),
/// a runner whose environment carries it beats every N ms instead of
/// `HEARTBEAT_EVERY` in its main loop and stream-drain loop, and
/// `status::heartbeat_is_stale` in a process that carries it calls a
/// heartbeat stale after 3 x N ms instead of `STALE_AFTER_SECONDS`.
/// For tests only, and only on their own status, profile usage, dispatch and
/// review start commands. Never set it on a pipeline command: the pipeline
/// keeps its own 2 s beat but judges staleness with the same function, so it
/// would stop a live run as lost after 3 x N ms. Deliberately left out of
/// the user documentation.
pub fn test_heartbeat_ms() -> Option<u64> {
    #[cfg(debug_assertions)]
    {
        let raw = std::env::var("THOTH_TEST_HEARTBEAT_MS").ok();
        parse_test_heartbeat_ms(raw.as_deref())
    }
    #[cfg(not(debug_assertions))]
    {
        parse_test_heartbeat_ms(None)
    }
}

/// TEST-7b: how often the runner's main loop and stream-drain loop beat:
/// `HEARTBEAT_EVERY`, unless the debug-only test switch
/// (`test_heartbeat_ms`) shortens it.
pub(super) fn heartbeat_every() -> Duration {
    test_heartbeat_ms().map(Duration::from_millis).unwrap_or(HEARTBEAT_EVERY)
}
/// How long to keep draining stdout after the child exits before giving up on
/// the reader thread. A grandchild that inherited the child's stdout handle can
/// hold the pipe open past the child's own exit (03-process-and-worktree.md:69:
/// clean teardown of the Bash tool subtree depends on claude cooperating), and a
/// blind `join()` there would wedge the runner forever, leaving the run stuck in
/// `running` with a heartbeat that never advances again.
pub(super) const STREAM_DRAIN_GRACE: Duration = Duration::from_secs(10);
/// I1 (fix round 1): how long to keep polling a child non-blockingly after
/// `kill_tree` itself reported failure (the pid was still listed after its own
/// ~2 s confirmation loop). A blind `child.wait()` in that situation can wedge
/// the runner forever on a genuinely stuck process; this bounds the wait
/// instead, so the runner always eventually settles the row.
pub(super) const KILL_WAIT_GRACE: Duration = Duration::from_secs(30);
/// Untested by an automated test (documented per the fix-round instructions
/// rather than extracted into a pure function): the branch this guards
/// (`kill_wait_deadline` expiring in `run_worker_inner`'s loop) needs a real
/// child that survives `kill_tree`'s own `taskkill /T /F` + Job Object
/// termination - i.e. a process immune to both - which is not something the
/// `fake-claude` harness or a portable integration test can produce on
/// demand. The logic the branch runs on expiry (settle `failed`, log,
/// `release_job`, return) is identical in shape to the already-tested
/// `db_poll_failures >= DB_POLL_FAILURE_LIMIT` branch and to
/// `best_effort_settle_on_error`, both of which *are* covered by
/// `a_spawn_failure_settles_the_row_failed_and_writes_a_runner_error_log`;
/// the only genuinely untested part is the `Instant::now() >= deadline`
/// comparison itself, which is a one-line stdlib comparison.
/// M6 (fix round 1): consecutive `touch_heartbeat`/`read_cancel_requested`
/// failures (shared counter - either kind counts, either kind resets it)
/// before the runner gives up polling the DB, kills the child, and settles the
/// run `failed` rather than looping forever unable to see a cancel or advance
/// a heartbeat.
pub(super) const DB_POLL_FAILURE_LIMIT: u32 = 10;

/// The whole runner: one child, one 1-second loop, one fixed classification.
/// Loads the run row and enforces M8's precondition, then hands off to
/// `run_worker_inner`; any error that escapes it is settled best-effort here
/// (I2/I3) before being returned unchanged.
pub fn run_worker(needle: &str) -> Result<i32> {
    let paths = Paths::from_env()?;
    let cwd = std::env::current_dir()?;
    let root = worktree::repo_root_from(&cwd)?;
    let repo_id = ids::repo_id_for(&root)?;
    let conn = db::open_state(&paths, &repo_id)?;

    let run_id = ids::resolve_prefix(&state::list_run_ids(&conn)?, needle)?;
    let run = state::get_run(&conn, &run_id)?.ok_or_else(|| anyhow!("run {run_id} not found"))?;

    // M8: a runner only ever starts a `queued` row (a fresh dispatch, or one
    // Task 11's `resume` put back to `queued`). Refusing here, before any
    // write, means a double-launch (e.g. an operator re-running `run-worker`
    // by hand against an already-running or already-terminal run) cannot
    // stomp on a run that is mid-flight or already settled.
    if run.status != RunStatus::Queued.as_str() {
        bail!(
            "run {run_id} is not queued (status = '{}'); refusing to start",
            run.status
        );
    }

    // The report page is rewritten only once the run has been settled and
    // committed, on either path, so it never shows a run mid-settle.
    match run_worker_inner(&paths, &conn, &repo_id, &root, &run) {
        Ok(()) => {
            crate::commands::report::refresh_if_present(&paths);
            Ok(0)
        }
        Err(e) => {
            best_effort_settle_on_error(&conn, &run, &e);
            crate::commands::report::refresh_if_present(&paths);
            Err(e)
        }
    }
}

/// Everything the 1-second loop needs to keep the inbox drained, gathered so
/// the loop body stays one call.
pub(super) struct InboxWatch<'a> {
    pub(super) paths: &'a Paths,
    pub(super) repo_id: &'a str,
    pub(super) inbox_dir: PathBuf,
    pub(super) run_dir: PathBuf,
    pub(super) worktree: PathBuf,
    pub(super) tailer: evidence::InboxTailer,
    pub(super) pairer: evidence::Pairer,
    pub(super) bad_lines: u64,
    pub(super) recorded: u64,
}

impl InboxWatch<'_> {
    /// Reads whatever `thoth hook` appended since the last call, turns each
    /// completed Pre/Post pair into one `evidence` row, and regenerates the
    /// worker-facing index if anything landed.
    ///
    /// brief §5: never propagates. A malformed line, a failed blob write or a
    /// busy database is counted and logged to `runner-error.log`; the run's own
    /// outcome does not depend on the evidence pipeline succeeding, because a
    /// missing citation already has a defined consequence (UNVERIFIED) and a
    /// crashed runner does not.
    pub(super) fn drain(&mut self, conn: &Connection, run: &state::Run) {
        let lines = self.tailer.read_new_lines();
        if lines.is_empty() {
            return;
        }
        let ctx = evidence::EvidenceContext {
            paths: self.paths,
            repo_id: self.repo_id,
            task_id: &run.task_id,
            run_id: &run.run_id,
        };
        let mut added = 0usize;
        for line in lines {
            let parsed = match evidence::parse_hook_line(&line) {
                Ok(p) => p,
                Err(e) => {
                    self.bad_lines += 1;
                    log_runner_error(
                        &self.run_dir,
                        &format!(
                            "skipping a malformed inbox line ({e:#}); {} skipped so far",
                            self.bad_lines
                        ),
                    );
                    continue;
                }
            };
            if let Some(cmd_ev) = self.pairer.offer(&parsed) {
                match evidence::record_command(conn, &ctx, &self.inbox_dir, &cmd_ev) {
                    Ok(_code) => {
                        added += 1;
                        self.recorded += 1;
                    }
                    Err(e) => log_runner_error(
                        &self.run_dir,
                        &format!("recording command evidence failed: {e:#}"),
                    ),
                }
            }
        }
        if added > 0 {
            if let Err(e) = evidence::refresh_index(conn, &run.task_id, &self.worktree) {
                log_runner_error(
                    &self.run_dir,
                    &format!("rewriting the evidence index failed: {e:#}"),
                );
            }
        }
    }
}
