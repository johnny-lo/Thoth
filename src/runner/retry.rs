use crate::ingest::IngestOutcome;
use crate::model::InterruptedReason;
use super::outcome::{classify_outcome, KillReason, StreamTally};
use std::time::Duration;

/// FC-2 (H5): how many times the runner relaunches the harness after a
/// transient failure, inside the same run.
pub const MAX_RETRIES: usize = 2;
/// FC-2 (H5): the wait before the first and before the second retry.
pub const RETRY_WAIT_SECONDS: [u64; 2] = [30, 120];

/// `A,B` (two non-negative decimal integers, spaces allowed) as `[A, B]`;
/// anything else is `RETRY_WAIT_SECONDS`.
pub(crate) fn parse_retry_waits(raw: Option<&str>) -> [u64; 2] {
    let Some(raw) = raw else {
        return RETRY_WAIT_SECONDS;
    };
    let parts: Vec<&str> = raw.split(',').collect();
    if parts.len() != 2 {
        return RETRY_WAIT_SECONDS;
    }
    match (parts[0].trim().parse::<u64>(), parts[1].trim().parse::<u64>()) {
        (Ok(a), Ok(b)) => [a, b],
        _ => RETRY_WAIT_SECONDS,
    }
}

/// The waits the runner actually uses. `THOTH_RETRY_WAITS` is a test seam
/// only - like the fakes' `FAKE_*` switches, the dispatching shell hands it
/// to the detached runner - so the integration tests need not sit through
/// 30 s and 2 min. It is deliberately left out of the user docs.
pub(super) fn retry_waits() -> [u64; 2] {
    parse_retry_waits(std::env::var("THOTH_RETRY_WAITS").ok().as_deref())
}

/// FC-2: whether this launch's ending is one the runner retries. Only
/// through `classify_outcome`, so our own kill comes first, a crash without a
/// result is not transient, and quota (`result.quota` included) wins over a
/// transient marker in the same text.
pub(crate) fn wants_retry(tally: &StreamTally, exit_code: Option<i32>, kill: Option<KillReason>) -> bool {
    classify_outcome(tally, exit_code, &IngestOutcome::Skipped, kill).interrupted_reason
        == Some(InterruptedReason::Transient)
}

/// FC-2: a retry counts against the run's own `--timeout`, so a wait that
/// would only end at or past the deadline is not waited.
pub(crate) fn wait_fits(elapsed: Duration, wait: Duration, timeout_seconds: i64) -> bool {
    elapsed + wait < Duration::from_secs(timeout_seconds.max(1) as u64)
}

/// FC-2: how a retry launches the harness.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RetryLaunch {
    /// Continue this session / thread / conversation, with `RESUME_PROMPT`.
    Resume(String),
    /// Start over with the first prompt, under a new runner-issued session id
    /// (Claude Code).
    FreshNamedSession,
    /// Start over with the first prompt; the harness names its session in
    /// its stream (codex, antigravity).
    FreshLateSession,
}

/// FC-2: continue the failed launch's session when there is one to continue,
/// start over otherwise. `known` is the session a `thoth resume`d run was
/// started on, `seen` the one the failed launch's stream named.
///
/// A reviewer never continues: codex's `exec resume` always carries
/// `-c sandbox_mode="workspace-write"` (adapter/codex.rs), which would hand a
/// read-only reviewer write access, and `resume` already restarts a review
/// rather than continuing it. A review is cheap and read-only; the same
/// prompt again is the safe retry.
///
/// Unrecorded (FC-2 risk): no harness's real behaviour on continuing right
/// after one of these failures has been captured - Claude's `--resume` of a
/// session that may hold only the user message, codex's `exec resume` after
/// `turn.failed`, agy's `--conversation` after an ERROR result. The fakes'
/// `ok` scenarios simply accept the continuation; that is an assumption.
pub(crate) fn retry_launch(
    is_review: bool,
    names_session_late: bool,
    session_id: &str,
    known: Option<&str>,
    seen: Option<&str>,
) -> RetryLaunch {
    if is_review {
        return if names_session_late {
            RetryLaunch::FreshLateSession
        } else {
            RetryLaunch::FreshNamedSession
        };
    }
    if !names_session_late {
        // Claude Code: the runner issued the session id itself, so it is
        // always there to continue.
        return RetryLaunch::Resume(session_id.to_string());
    }
    // A codex failure before `thread.started`, or an agy one before `init`
    // (the eligibility failure), has no session to continue - unless the
    // run itself was resumed onto a known one.
    match seen.or(known) {
        Some(s) => RetryLaunch::Resume(s.to_string()),
        None => RetryLaunch::FreshLateSession,
    }
}
