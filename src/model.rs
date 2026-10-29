#[derive(Debug, thiserror::Error)]
#[error("invalid {kind} value: '{value}'")]
pub struct ModelParseError {
    pub kind: &'static str,
    pub value: String,
}

macro_rules! string_enum {
    ($name:ident, $kind:literal, { $($variant:ident => $text:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name { $($variant),+ }

        impl $name {
            #[allow(dead_code)] // some enums only stringify from test code
            pub fn as_str(&self) -> &'static str {
                match self { $($name::$variant => $text),+ }
            }

            #[allow(dead_code)] // RunStatus::parse / InterruptedReason::parse are test-only
            pub fn parse(s: &str) -> Result<Self, ModelParseError> {
                match s {
                    $($text => Ok($name::$variant),)+
                    other => Err(ModelParseError { kind: $kind, value: other.to_string() }),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

string_enum!(TaskStatus, "task status", {
    Draft => "draft",
    Queued => "queued",
    Running => "running",
    InReview => "in_review",
    Pass => "pass",
    Reject => "reject",
    Unverified => "unverified",
    Interrupted => "interrupted",
    Cancelled => "cancelled",
    Failed => "failed",
    Accepted => "accepted",
});

string_enum!(RunStatus, "run status", {
    Queued => "queued",
    Running => "running",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    Interrupted => "interrupted",
});

string_enum!(InterruptedReason, "interrupted reason", {
    Quota => "quota",
    Crash => "crash",
    User => "user",
    Timeout => "timeout",
    Auth => "auth",
    Transient => "transient",
});

string_enum!(Risk, "risk", {
    Low => "low",
    Normal => "normal",
    Important => "important",
    High => "high",
});

impl TaskStatus {
    /// States `thoth wait` returns on (brief section 7).
    pub fn is_wait_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::InReview
                | TaskStatus::Unverified
                | TaskStatus::Failed
                | TaskStatus::Cancelled
                | TaskStatus::Interrupted
        )
    }

    /// States nobody is waiting on any more. `status`'s no-arg filter used to
    /// call this directly (see `is_dead_end` below for why it now doesn't);
    /// kept as the general "is this task over" predicate and exercised by
    /// its own tests.
    #[allow(dead_code)]
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Pass
                | TaskStatus::Reject
                | TaskStatus::Cancelled
                | TaskStatus::Failed
                | TaskStatus::Accepted
        )
    }

    /// True dead ends, a strict subset of `is_terminal`: `thoth status` with
    /// no args hides exactly these (T8/T9 final-review consolidation, #28/#36).
    ///
    /// `Failed` was excluded in Phase 1 because a failed task rejoined
    /// `dispatch_allowed`'s entry states (M1, fix round 1) and is therefore still
    /// actionable. Phase 2 excludes `Reject` for exactly the same reason: it is
    /// now a `dispatch` entry state too - the rework loop's whole point - and a
    /// Supervisor who has just been handed a REJECT would otherwise run
    /// `thoth status` and find the one task that needs them missing from the
    /// list. `Pass`, `Cancelled` and `Accepted` (a document task the
    /// Supervisor closed with `task accept`) are the only decisions nobody
    /// reworks.
    pub fn is_dead_end(&self) -> bool {
        matches!(
            self,
            TaskStatus::Pass | TaskStatus::Cancelled | TaskStatus::Accepted
        )
    }
}

impl RunStatus {
    /// Terminal run states: no runner will touch this run again. T8/T9
    /// final-review consolidation (#28/#36): `runner::is_run_status_terminal`
    /// used to hardcode this same set as its own string match; it now defers
    /// here so the predicate has one definition.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            RunStatus::Completed | RunStatus::Failed | RunStatus::Cancelled | RunStatus::Interrupted
        )
    }
}

/// brief §6: `tasks.attempt_cap`'s default, and the fallback a row written
/// before the v2 migration reads.
// Retired in Task 8: reached through `get_task_attempt_cap`, which
// `commands::dispatch::run` now calls on every dispatch.
pub const DEFAULT_ATTEMPT_CAP: i64 = 2;

/// `runs.role` values Phase 2 uses. v6 §10.5:823 lists the full vocabulary
/// (`implementer|retriever|reviewer|auditor|presenter`); the other three
/// arrive in Phase 3+.
pub mod role {
    /// Fix round 1 / M6: `dispatch` now writes this instead of the literal
    /// `"implementer"` string it used to.
    pub const IMPLEMENTER: &str = "implementer";
    /// Read by `runner::run_worker_inner`'s hook-wiring gate (Task 3):
    /// `is_review = run.role == role::REVIEWER`.
    pub const REVIEWER: &str = "reviewer";
    /// A read-only run that reviews the task's spec before its first dispatch
    /// (SR-2); the runner treats it like `REVIEWER` except that its report is
    /// `.agent/spec-review.json` and it never writes `tasks.status`.
    pub const SPEC_REVIEWER: &str = "spec_reviewer";
}

/// Event `type` values written to `events.type`. Each constant is unread by
/// production code until the task noted below writes its first event of that
/// kind; the attributes come off there.
pub mod event_type {
    pub const TASK_CREATED: &str = "task_created";
    pub const STATUS_CHANGE: &str = "status_change";
    pub const RUN_STARTED: &str = "run_started";
    pub const HARNESS_INIT: &str = "harness_init";
    pub const RUN_INTERRUPTED: &str = "run_interrupted";
    pub const QUOTA_EXCEEDED: &str = "quota_exceeded";
    /// Written by the runner when a run ended in error and the harness's own
    /// auth check said the profile is not logged in (FC-1).
    pub const LOGIN_FAILED: &str = "login_failed";
    /// Written by the runner when a Claude run's result line said the model
    /// needs usage credits this account does not have (MODELS-1): the run is
    /// `failed`, not a quota stop, and the profile is not parked.
    pub const MODEL_UNAVAILABLE: &str = "model_unavailable";
    /// Written by the runner each time it relaunched the harness after a
    /// transient failure, in the same run (FC-2).
    pub const RUN_RETRY: &str = "run_retry";
    pub const RATE_LIMIT_WARNING: &str = "rate_limit_warning";
    pub const API_RETRY: &str = "api_retry";
    /// Written by the runner through `runner::append_usage` each time a launch
    /// gets a result line, so a run retried after a transient failure (FC-2) or
    /// continued by `thoth resume` has one per such launch. Readers that want one
    /// per run take the newest, as `commands::status::run_usage_by_run` does.
    pub const RUN_USAGE: &str = "run_usage";
    /// Written by the runner (`run_quota::record`) before an implementer
    /// run's final `status_change`: roughly how many points of the five-hour
    /// window the run used. Read by `thoth models`.
    pub const RUN_QUOTA: &str = "run_quota";
    /// Written by the runner next to `run_usage` when subagents were spawned.
    pub const SUBAGENTS_SPAWNED: &str = "subagents_spawned";
    /// Written by the runner when an antigravity command's exit code never
    /// appeared in agy's transcript and its row was recorded with null.
    pub const COMMAND_EXIT_CODE_UNKNOWN: &str = "command_exit_code_unknown";
    /// Written by the runner, one per line of an antigravity run's
    /// `.agent/agy-hook-denials.jsonl`, after agy's stream has ended.
    pub const TOOL_DENIED: &str = "tool_denied";
    pub const RESULT_INGESTED: &str = "result_ingested";
    pub const RESULT_INVALID: &str = "result_invalid";
    pub const WORKTREE_CREATED: &str = "worktree_created";
    pub const WORKTREE_REUSED: &str = "worktree_reused";
    pub const WORKTREE_ERROR: &str = "worktree_error";
    /// Written by `commands::worktrees::sweep` when it removed a finished
    /// task's worktree directory (WT-1); the branch is kept.
    pub const WORKTREE_REMOVED: &str = "worktree_removed";
    /// Written by `commands::worktrees::sweep` when it kept a finished task's
    /// worktree directory, with the reason; once per path and reason (WT-1).
    pub const WORKTREE_KEPT: &str = "worktree_kept";
    pub const CANCEL_REQUESTED: &str = "cancel_requested";
    // Phase 2 (v6 §10.5's `type` comment lists evidence_added / evidence_opened
    // / verdict / decision_added by name). None of the remaining seven has a
    // writer yet - each becomes live when the task noted writes its first
    // event of that kind. Not in the brief's inventory table; added here
    // because `cargo clippy --all-targets -D warnings` flags every unwritten
    // constant, same as the existing Phase 1 constants above would if they
    // had no writer.
    /// Written by `evidence::insert_row` (Task 3).
    pub const EVIDENCE_ADDED: &str = "evidence_added";
    /// Written by `commands::evidence::show` (Task 6).
    pub const EVIDENCE_OPENED: &str = "evidence_opened";
    /// Written by `commands::review::ingest_review` (Task 7).
    pub const VERDICT: &str = "verdict";
    /// Written by `commands::review::start` (Task 7).
    pub const REVIEW_STARTED: &str = "review_started";
    /// Written by `commands::review::review_invalid` (Task 7).
    pub const REVIEW_INVALID: &str = "review_invalid";
    /// RFIX-1: an edit review's fix, applied on the fix branch.
    pub const REVIEW_FIX_APPLIED: &str = "review_fix_applied";
    /// RFIX-1: an edit review's fix, refused at a stage.
    pub const REVIEW_FIX_REFUSED: &str = "review_fix_refused";
    /// Written by `commands::decision::add` (Task 9).
    pub const DECISION_ADDED: &str = "decision_added";
    /// Written by `commands::task::update_spec` (Task 8).
    pub const SPEC_UPDATED: &str = "spec_updated";
    /// Written by `commands::verify::run` (Task 9).
    pub const VERIFY_RAN: &str = "verify_ran";
    /// Written by `commands::task::close`.
    pub const TASK_CLOSED: &str = "task_closed";
    /// Written by `commands::task::accept`: a document task's delivery was
    /// taken without a review (DOC-1).
    pub const TASK_ACCEPTED: &str = "task_accepted";
    /// Written by `commands::checks::add`.
    pub const CHECK_ADDED: &str = "check_added";
    /// Written by `commands::checks::approve`.
    pub const CHECK_APPROVED: &str = "check_approved";
    /// Written by `commands::checks::approve` with `--run`: the protected test
    /// and check-word changes of that run it approved.
    pub const PROTECTED_CHANGE_APPROVED: &str = "protected_change_approved";
    /// Written by `commands::checks::remove`.
    pub const CHECK_REMOVED: &str = "check_removed";
    /// Written by dispatch, review start and spec review start: what was
    /// launched, what was kept from the previous run, and why (DT-2, RJ-1).
    pub const LAUNCH_CHOICE: &str = "launch_choice";
    /// Written by `dispatch` when it continues an interrupted or failed run
    /// and wrote `.agent/handoff.md` for the new worker (S3-9).
    pub const HANDOFF_WRITTEN: &str = "handoff_written";
    /// Written by `commands::spec_review::record`: one APPROVE or REVISE of
    /// the task's spec, bound to the spec's sha256 (SR-1). Also written by the
    /// runner for a `spec_reviewer` run (`source` `reviewer_run`).
    pub const SPEC_REVIEW: &str = "spec_review";
    /// Written by `commands::spec_review::record` when the spec-review
    /// document was refused, with the stage and reason (SR-1). Also written by
    /// the runner for a `spec_reviewer` run (`source` `reviewer_run`).
    pub const SPEC_REVIEW_INVALID: &str = "spec_review_invalid";
    /// Written by `commands::spec_review::start`, on the new run, with the
    /// sha256 of the spec it was started against (SR-2).
    pub const SPEC_REVIEW_STARTED: &str = "spec_review_started";
    /// Written by `dispatch` on the new run when `--skip-spec-review` was
    /// given, with the reason (SR-1).
    pub const SPEC_REVIEW_SKIPPED: &str = "spec_review_skipped";
    /// Written through `redact::record` by the exits that hand content over
    /// once (`evidence show`, `review packet`, the runner's review input):
    /// which kinds of secret were masked, how many, in which evidence (RD-1).
    pub const SECRET_REDACTED: &str = "secret_redacted";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_status_round_trips_every_variant() {
        let all = [
            TaskStatus::Draft,
            TaskStatus::Queued,
            TaskStatus::Running,
            TaskStatus::InReview,
            TaskStatus::Pass,
            TaskStatus::Reject,
            TaskStatus::Unverified,
            TaskStatus::Interrupted,
            TaskStatus::Cancelled,
            TaskStatus::Failed,
            TaskStatus::Accepted,
        ];
        for s in all {
            assert_eq!(TaskStatus::parse(s.as_str()).unwrap(), s);
        }
        assert_eq!(TaskStatus::InReview.as_str(), "in_review");
    }

    #[test]
    fn wait_terminal_set_matches_the_spec() {
        // brief section 7: wait blocks until in_review / unverified / failed / cancelled / interrupted
        assert!(TaskStatus::InReview.is_wait_terminal());
        assert!(TaskStatus::Unverified.is_wait_terminal());
        assert!(TaskStatus::Failed.is_wait_terminal());
        assert!(TaskStatus::Cancelled.is_wait_terminal());
        assert!(TaskStatus::Interrupted.is_wait_terminal());
        assert!(!TaskStatus::Queued.is_wait_terminal());
        assert!(!TaskStatus::Running.is_wait_terminal());
        assert!(!TaskStatus::Draft.is_wait_terminal());
        // Like pass: the task went through in_review first and wait returned there.
        assert!(!TaskStatus::Accepted.is_wait_terminal());
    }

    #[test]
    fn terminal_set_is_pass_reject_cancelled_failed_accepted() {
        assert!(TaskStatus::Pass.is_terminal());
        assert!(TaskStatus::Reject.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Accepted.is_terminal());
        assert!(!TaskStatus::InReview.is_terminal());
        assert!(!TaskStatus::Unverified.is_terminal());
        assert!(!TaskStatus::Interrupted.is_terminal());
    }

    #[test]
    fn dead_end_set_is_only_pass_cancelled_and_accepted() {
        assert!(TaskStatus::Pass.is_dead_end());
        assert!(TaskStatus::Cancelled.is_dead_end());
        assert!(TaskStatus::Accepted.is_dead_end());
        assert!(
            !TaskStatus::Failed.is_dead_end(),
            "failed is re-dispatchable, not a dead end"
        );
        assert!(
            !TaskStatus::Reject.is_dead_end(),
            "Phase 2: reject is the rework loop's entry state, so `status` must show it"
        );
        assert!(!TaskStatus::InReview.is_dead_end());
        assert!(!TaskStatus::Unverified.is_dead_end());
        assert!(!TaskStatus::Interrupted.is_dead_end());
        assert!(!TaskStatus::Queued.is_dead_end());
        assert!(!TaskStatus::Running.is_dead_end());
        assert!(!TaskStatus::Draft.is_dead_end());
        // `is_terminal` is unchanged: reject is still a terminal *run* outcome,
        // it is just no longer a dead end for the task.
        assert!(TaskStatus::Reject.is_terminal());
    }

    #[test]
    fn run_status_is_terminal_matches_completed_failed_cancelled_interrupted() {
        for s in [RunStatus::Completed, RunStatus::Failed, RunStatus::Cancelled, RunStatus::Interrupted] {
            assert!(s.is_terminal(), "{s:?} should be terminal");
        }
        for s in [RunStatus::Queued, RunStatus::Running] {
            assert!(!s.is_terminal(), "{s:?} should not be terminal");
        }
    }

    #[test]
    fn run_status_and_reasons_round_trip() {
        for s in [
            RunStatus::Queued,
            RunStatus::Running,
            RunStatus::Completed,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::Interrupted,
        ] {
            assert_eq!(RunStatus::parse(s.as_str()).unwrap(), s);
        }
        for r in [
            InterruptedReason::Quota,
            InterruptedReason::Crash,
            InterruptedReason::User,
            InterruptedReason::Timeout,
            InterruptedReason::Auth,
            InterruptedReason::Transient,
        ] {
            assert_eq!(InterruptedReason::parse(r.as_str()).unwrap(), r);
        }
        for r in [Risk::Low, Risk::Normal, Risk::Important, Risk::High] {
            assert_eq!(Risk::parse(r.as_str()).unwrap(), r);
        }
    }

    #[test]
    fn unknown_values_are_rejected_with_the_kind_named() {
        let e = TaskStatus::parse("banana").unwrap_err();
        assert_eq!(e.kind, "task status");
        assert_eq!(e.value, "banana");
        assert!(RunStatus::parse("banana").is_err());
        assert!(InterruptedReason::parse("banana").is_err());
        assert!(Risk::parse("banana").is_err());
    }
}
