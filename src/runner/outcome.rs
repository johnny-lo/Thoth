use crate::adapter::{looks_like_quota, looks_like_transient, ResultEvent};
use crate::ingest::IngestOutcome;
use crate::model::{InterruptedReason, RunStatus, TaskStatus};
use anyhow::Result;

/// What the runner learned from the child's stdout.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StreamTally {
    pub result: Option<ResultEvent>,
    pub session_id: Option<String>,
    pub saw_rate_limit_warning: bool,
    pub saw_api_retry: bool,
    pub lines: u64,
    /// Lines the parser could not read as JSON. Counted rather than dropped so a
    /// harness that starts emitting a different shape shows up in the events
    /// instead of silently looking like a clean run.
    pub unparsable_lines: u64,
    pub unparsable_bytes: u64,
}

/// Why the runner killed the child. Our own action, so it outranks whatever the
/// child managed to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillReason {
    Cancelled,
    Timeout,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub run_status: RunStatus,
    pub task_status: TaskStatus,
    pub interrupted_reason: Option<InterruptedReason>,
    pub note: String,
}

/// brief section 2.5, in the order given there, with our own kills checked first.
pub fn classify_outcome(
    tally: &StreamTally,
    exit_code: Option<i32>,
    ingest: &IngestOutcome,
    kill: Option<KillReason>,
) -> Outcome {
    match kill {
        Some(KillReason::Cancelled) => {
            return Outcome {
                run_status: RunStatus::Cancelled,
                task_status: TaskStatus::Cancelled,
                interrupted_reason: Some(InterruptedReason::User),
                note: "cancelled by request".to_string(),
            }
        }
        Some(KillReason::Timeout) => {
            return Outcome {
                run_status: RunStatus::Failed,
                task_status: TaskStatus::Failed,
                interrupted_reason: Some(InterruptedReason::Timeout),
                note: "wall-clock timeout".to_string(),
            }
        }
        None => {}
    }

    let result = match &tally.result {
        // No `result` line at all, whatever the exit code: crash.
        None => {
            return Outcome {
                run_status: RunStatus::Interrupted,
                task_status: TaskStatus::Interrupted,
                interrupted_reason: Some(InterruptedReason::Crash),
                note: format!("child exited {exit_code:?} without a result event"),
            }
        }
        Some(r) => r,
    };

    if !result.is_error && exit_code == Some(0) {
        return match ingest {
            IngestOutcome::Ok { claim_count, brief, .. } => Outcome {
                run_status: RunStatus::Completed,
                task_status: TaskStatus::InReview,
                interrupted_reason: None,
                note: format!("ingested {claim_count} claim(s): {brief}"),
            },
            IngestOutcome::Invalid { reason } => Outcome {
                run_status: RunStatus::Completed,
                task_status: TaskStatus::Unverified,
                interrupted_reason: None,
                note: format!("result.json rejected: {reason}"),
            },
            IngestOutcome::Skipped => Outcome {
                run_status: RunStatus::Completed,
                task_status: TaskStatus::Unverified,
                interrupted_reason: None,
                note: "result.json was never ingested".to_string(),
            },
        };
    }

    if result.is_error {
        // MODELS-1: Claude's structured `api_error` for a model the account's
        // plan cannot run. Checked before the quota pattern because the text
        // carries a 429 that would otherwise park the whole profile. Only
        // this one value counts; `api_error_code: credits_required` alone
        // does not (no recording shows what else carries it).
        if result.api_error.as_deref() == Some(crate::model_list::API_ERROR_MODEL_REQUIRES_CREDITS) {
            return Outcome {
                run_status: RunStatus::Failed,
                task_status: TaskStatus::Failed,
                interrupted_reason: None,
                note: format!(
                    "{MODEL_UNAVAILABLE_PREFIX}{}",
                    result.text.clone().unwrap_or_default().trim()
                ),
            };
        }
        // brief section 2.5: match the error text, and api_error_status folded in
        // so a bare numeric 429 still trips the pattern.
        let haystack = format!(
            "{} {}",
            result.text.clone().unwrap_or_default(),
            result.api_error_status.clone().unwrap_or_default()
        );
        if looks_like_quota(&haystack) || result.quota {
            return Outcome {
                run_status: RunStatus::Interrupted,
                task_status: TaskStatus::Interrupted,
                interrupted_reason: Some(InterruptedReason::Quota),
                note: format!("quota interruption: {}", haystack.trim()),
            };
        }
        // FC-1 (H1): checked after quota, so a text with both `429` and
        // `overloaded` is still quota.
        if looks_like_transient(&haystack) {
            return Outcome {
                run_status: RunStatus::Interrupted,
                task_status: TaskStatus::Interrupted,
                interrupted_reason: Some(InterruptedReason::Transient),
                note: format!("transient interruption: {}", haystack.trim()),
            };
        }
    }

    Outcome {
        run_status: RunStatus::Failed,
        task_status: TaskStatus::Failed,
        interrupted_reason: None,
        note: format!(
            "harness reported failure (exit {exit_code:?}, is_error={}): {}",
            result.is_error,
            result.text.clone().unwrap_or_default()
        ),
    }
}

/// MODELS-1: how the note of a run Claude refused for want of usage credits
/// starts; `record_model_unavailable` recognises the outcome by it.
pub(crate) const MODEL_UNAVAILABLE_PREFIX: &str = "model unavailable on this account: ";

impl Outcome {
    /// MODELS-1: a `failed` run whose model this account cannot use.
    pub(crate) fn is_model_unavailable(&self) -> bool {
        self.run_status == RunStatus::Failed
            && self.interrupted_reason.is_none()
            && self.note.starts_with(MODEL_UNAVAILABLE_PREFIX)
    }
}

/// FC-1 (H2): the outcomes that get the runner's auth check - a plain failure,
/// a crash, or a transient interruption. MODELS-1: not a model the account
/// cannot use; the account is logged in, Claude said so in a structured way.
pub(super) fn needs_login_check(outcome: &Outcome) -> bool {
    match outcome.run_status {
        RunStatus::Failed => outcome.interrupted_reason.is_none() && !outcome.is_model_unavailable(),
        RunStatus::Interrupted => matches!(
            outcome.interrupted_reason,
            Some(InterruptedReason::Crash) | Some(InterruptedReason::Transient)
        ),
        _ => false,
    }
}

/// FC-1 (H2): applies the auth check's answer to `outcome` and returns the
/// `login_check` value for the final `status_change`. Only `signed_out`
/// changes the outcome; an error or an unclear answer leaves it alone.
pub(super) fn apply_login_check(
    outcome: &mut Outcome,
    profile_id: &str,
    check: Result<crate::adapter::AuthStatus>,
) -> String {
    match check {
        Err(e) => {
            let msg = format!("{e:#}");
            format!("error: {}", msg.lines().next().unwrap_or(""))
        }
        Ok(s) if s.signed_out => {
            outcome.run_status = RunStatus::Interrupted;
            outcome.task_status = TaskStatus::Interrupted;
            outcome.interrupted_reason = Some(InterruptedReason::Auth);
            outcome.note = format!("login failure: {profile_id} is not logged in; {}", outcome.note);
            "signed_out".to_string()
        }
        Ok(s) if s.logged_in => "logged_in".to_string(),
        Ok(_) => "unknown".to_string(),
    }
}
