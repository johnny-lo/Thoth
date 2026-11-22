use super::*;
use crate::adapter::ResultEvent;
use crate::ingest::IngestOutcome;

/// TEST-2: the test switch only shortens the poll, to 1..=1000 ms; any
/// other value (or none) leaves it at one second.
#[test]
fn poll_interval_is_one_second_unless_the_test_switch_shortens_it() {
    let second = Duration::from_secs(1);
    assert_eq!(parse_poll_every(None), second);
    for raw in ["", "abc", "0", "-1", "1.5", "1001"] {
        assert_eq!(parse_poll_every(Some(raw)), second, "{raw:?}");
    }
    assert_eq!(parse_poll_every(Some("1")), Duration::from_millis(1));
    assert_eq!(parse_poll_every(Some("100")), Duration::from_millis(100));
    assert_eq!(parse_poll_every(Some(" 100 ")), Duration::from_millis(100));
    assert_eq!(parse_poll_every(Some("1000")), Duration::from_millis(1000));
}

/// TEST-7b: the heartbeat switch is on only for a whole number of
/// milliseconds from 1 to 5000; with no value (always so in a release build)
/// it is off and the runner beats every five seconds, as before.
#[test]
fn heartbeat_switch_is_off_unless_a_whole_number_of_milliseconds_up_to_5000() {
    assert_eq!(parse_test_heartbeat_ms(None), None);
    for raw in ["", " ", "abc", "0", "-1", "+5", "1.5", "5001", "99999999999999999999999"] {
        assert_eq!(parse_test_heartbeat_ms(Some(raw)), None, "{raw:?}");
    }
    assert_eq!(parse_test_heartbeat_ms(Some("1")), Some(1));
    assert_eq!(parse_test_heartbeat_ms(Some("500")), Some(500));
    assert_eq!(parse_test_heartbeat_ms(Some(" 500 ")), Some(500));
    assert_eq!(parse_test_heartbeat_ms(Some("5000")), Some(5000));
    assert_eq!(HEARTBEAT_EVERY, Duration::from_secs(5));
}

#[test]
fn is_run_status_terminal_matches_completed_failed_cancelled_interrupted() {
    for s in ["completed", "failed", "cancelled", "interrupted"] {
        assert!(is_run_status_terminal(s), "{s} should be terminal");
    }
    for s in ["queued", "running", "banana"] {
        assert!(!is_run_status_terminal(s), "{s} should not be terminal");
    }
}

fn ok_result() -> StreamTally {
    StreamTally {
        result: Some(ResultEvent {
            session_id: Some("s".into()),
            is_error: false,
            subtype: Some("success".into()),
            text: Some("DONE".into()),
            api_error_status: None,
            input_tokens: Some(1),
            output_tokens: Some(2),
            ..Default::default()
        }),
        session_id: Some("s".into()),
        saw_rate_limit_warning: false,
        saw_api_retry: false,
        lines: 4,
        unparsable_lines: 0,
        unparsable_bytes: 0,
    }
}

#[test]
fn success_with_a_valid_result_json_goes_to_in_review() {
    let o = classify_outcome(
        &ok_result(),
        Some(0),
        &IngestOutcome::Ok { summary: "s".into(), claim_count: 1, brief: "s".into() },
        None,
    );
    // T10 #39: assert the full Outcome, not just a couple of fields.
    assert_eq!(
        o,
        Outcome {
            run_status: RunStatus::Completed,
            task_status: TaskStatus::InReview,
            interrupted_reason: None,
            note: "ingested 1 claim(s): s".to_string(),
        }
    );
}

#[test]
fn success_with_an_invalid_result_json_goes_to_unverified() {
    let o = classify_outcome(
        &ok_result(),
        Some(0),
        &IngestOutcome::Invalid { reason: "missing task_id".into() },
        None,
    );
    // T10 #39: assert the full Outcome, not just a couple of fields.
    assert_eq!(
        o,
        Outcome {
            run_status: RunStatus::Completed,
            task_status: TaskStatus::Unverified,
            interrupted_reason: None,
            note: "result.json rejected: missing task_id".to_string(),
        }
    );
}

fn err_result(text: &str, api_status: Option<&str>) -> StreamTally {
    StreamTally {
        result: Some(ResultEvent {
            session_id: Some("s".into()),
            is_error: true,
            subtype: Some("error_during_execution".into()),
            text: Some(text.into()),
            api_error_status: api_status.map(|x| x.to_string()),
            input_tokens: Some(1),
            output_tokens: Some(0),
            ..Default::default()
        }),
        session_id: Some("s".into()),
        saw_rate_limit_warning: true,
        saw_api_retry: false,
        lines: 3,
        unparsable_lines: 0,
        unparsable_bytes: 0,
    }
}

#[test]
fn an_error_result_that_mentions_rate_limit_is_a_quota_interruption() {
    let o = classify_outcome(
        &err_result("Request rejected (429): usage limit reached", Some("429")),
        Some(1),
        &IngestOutcome::Skipped,
        None,
    );
    assert_eq!(o.run_status, RunStatus::Interrupted);
    assert_eq!(o.task_status, TaskStatus::Interrupted);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Quota));
}

/// MODELS-1 (AC4, t8): the four result shapes. The recorded
/// credits_required line (claude-b, run 01M51A3MKH77BG4D65NW8XCE9M) is a
/// failed run with the model-unavailable note; the recorded real usage
/// limit (run 01M4T6D9NTDBRJMCH4S4HNEBCV: 429, `usage_limit_reached`, no
/// code) and fake-claude's `Claude AI usage limit reached` stay quota;
/// so does a `credits_required` code whose `api_error` is something else.
#[test]
fn a_model_requires_usage_credits_result_is_a_failed_run_not_a_quota_stop() {
    let with = |text: &str, status: Option<&str>, api_error: Option<&str>, code: Option<&str>| {
        let mut t = err_result(text, status);
        let r = t.result.as_mut().unwrap();
        r.api_error = api_error.map(str::to_string);
        r.api_error_code = code.map(str::to_string);
        t
    };
    let credits_text = "Fable 5.1 requires usage credits. Switch to another model, or manage usage credits at https://claude.ai/settings/usage?from=cc_cli_limit_message, to continue.";
    let o = classify_outcome(
        &with(credits_text, Some("429"), Some("model_requires_usage_credits"), Some("credits_required")),
        Some(1),
        &IngestOutcome::Skipped,
        None,
    );
    assert_eq!(o.run_status, RunStatus::Failed);
    assert_eq!(o.task_status, TaskStatus::Failed);
    assert_eq!(o.interrupted_reason, None);
    assert_eq!(o.note, format!("model unavailable on this account: {credits_text}"));
    assert!(o.is_model_unavailable());
    assert!(!needs_login_check(&o));

    let real_limit = with(
        "You've hit your session limit · resets 3pm (Asia/Taipei)",
        Some("429"),
        Some("usage_limit_reached"),
        None,
    );
    let o = classify_outcome(&real_limit, Some(1), &IngestOutcome::Skipped, None);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Quota));
    assert!(!o.is_model_unavailable());

    let fake_limit = with("Request rejected (429): Claude AI usage limit reached; your five-hour rate limit will reset later.", Some("429"), None, None);
    let o = classify_outcome(&fake_limit, Some(1), &IngestOutcome::Skipped, None);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Quota));

    let code_only = with("usage limit reached", Some("429"), Some("something_else"), Some("credits_required"));
    let o = classify_outcome(&code_only, Some(1), &IngestOutcome::Skipped, None);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Quota));
    assert!(!o.is_model_unavailable());
}

#[test]
fn a_bare_numeric_api_error_status_of_429_still_trips_quota() {
    let o = classify_outcome(
        &err_result("the model refused", Some("429")),
        Some(1),
        &IngestOutcome::Skipped,
        None,
    );
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Quota));
}

/// T10 #40: `classify_outcome` ignores the exit code entirely once
/// `is_error` + quota-shaped text match, so a harness that happens to
/// exit 0 after reporting a quota error must still be classified as an
/// interruption, not a success.
#[test]
fn a_quota_error_that_exits_0_is_still_an_interruption_not_a_success() {
    let o = classify_outcome(
        &err_result("Request rejected (429): usage limit reached", Some("429")),
        Some(0),
        &IngestOutcome::Skipped,
        None,
    );
    assert_eq!(o.run_status, RunStatus::Interrupted);
    assert_eq!(o.task_status, TaskStatus::Interrupted);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Quota));
}

#[test]
fn an_error_result_with_an_ordinary_message_is_a_plain_failure() {
    let o = classify_outcome(
        &err_result("tool call exploded", None),
        Some(1),
        &IngestOutcome::Skipped,
        None,
    );
    assert_eq!(o.run_status, RunStatus::Failed);
    assert_eq!(o.task_status, TaskStatus::Failed);
    assert_eq!(o.interrupted_reason, None);
}

#[test]
fn no_result_event_at_all_is_a_crash_whatever_the_exit_code() {
    for code in [Some(137), Some(0), Some(1), None] {
        let o = classify_outcome(
            &StreamTally::default(),
            code,
            &IngestOutcome::Skipped,
            None,
        );
        assert_eq!(o.run_status, RunStatus::Interrupted, "exit {code:?}");
        assert_eq!(o.task_status, TaskStatus::Interrupted, "exit {code:?}");
        assert_eq!(o.interrupted_reason, Some(InterruptedReason::Crash));
    }
}

#[test]
fn a_clean_result_with_a_nonzero_exit_code_is_a_failure_not_a_success() {
    let o = classify_outcome(
        &ok_result(),
        Some(2),
        &IngestOutcome::Ok { summary: "s".into(), claim_count: 0, brief: "s".into() },
        None,
    );
    assert_eq!(o.run_status, RunStatus::Failed);
    assert_eq!(o.task_status, TaskStatus::Failed);
}

/// Scope §2.3 / AC4: a codex usage-limit error in the E6 shape settles
/// `interrupted`/`quota`, including the marker `looks_like_quota` alone
/// does not know. Built from the E6 shape and the strings in the codex
/// binary - unverified against a real codex usage-limit error (scope §3).
#[test]
fn a_codex_usage_limit_error_is_a_quota_interruption() {
    use crate::adapter::codex::CodexAdapter;
    for inner in [
        serde_json::json!({"type": "error", "status": 429, "error": {
            "type": "usage_limit_reached", "message": "You've hit your usage limit."}}),
        serde_json::json!({"type": "error", "error": {
            "type": "UsageLimitExceeded", "message": "try later"}}),
    ] {
        let line = serde_json::json!({"type": "turn.failed", "error": {"message": inner.to_string()}})
            .to_string();
        let StreamEvent::Result(r) = CodexAdapter { bin: "codex".into() }.parse_stream_line(&line)
        else {
            panic!("turn.failed must parse as the result");
        };
        let tally = StreamTally { result: Some(r), ..Default::default() };
        let o = classify_outcome(&tally, Some(1), &IngestOutcome::Skipped, None);
        assert_eq!(o.run_status, RunStatus::Interrupted, "{inner}");
        assert_eq!(o.interrupted_reason, Some(InterruptedReason::Quota), "{inner}");
    }
}

/// A newer codex `error` replaces an older one, so a transient error
/// followed by a usage-limit error settles `interrupted`/`quota`; a
/// `turn.*` result keeps precedence over any `error`, before or after it.
/// The usage-limit line is built in the E6 shape - unverified against a
/// real codex usage-limit error (scope §3).
#[test]
fn the_newest_codex_error_wins_and_a_turn_result_outranks_errors() {
    use crate::adapter::codex::CodexAdapter;
    let a = CodexAdapter { bin: "codex".into() };
    let error = |inner: serde_json::Value| {
        let line = serde_json::json!({"type": "error", "message": inner.to_string()}).to_string();
        match a.parse_stream_line(&line) {
            StreamEvent::HarnessError(r) => r,
            other => panic!("expected HarnessError, got {other:?}"),
        }
    };
    let transient = error(serde_json::json!({"type": "error", "status": 503, "error": {
        "type": "server_error", "message": "stream disconnected before completion"}}));
    let quota = error(serde_json::json!({"type": "error", "status": 429, "error": {
        "type": "usage_limit_reached", "message": "You've hit your usage limit."}}));
    let failed_line = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/codex/exec-error-bad-model.jsonl"
    ))
    .lines()
    .find(|l| l.contains("turn.failed"))
    .unwrap();
    let StreamEvent::Result(turn_failed) = a.parse_stream_line(failed_line) else {
        panic!("turn.failed must parse as the result");
    };

    let mut tally = StreamTally::default();
    note_harness_error(&mut tally, transient.clone());
    note_harness_error(&mut tally, quota.clone());
    let o = classify_outcome(&tally, Some(1), &IngestOutcome::Skipped, None);
    assert_eq!(o.run_status, RunStatus::Interrupted);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Quota));

    // and the other way round: the later, non-quota error decides; since
    // FC-1 a stream disconnect is a transient interruption, not a failure
    let mut tally = StreamTally::default();
    note_harness_error(&mut tally, quota.clone());
    note_harness_error(&mut tally, transient.clone());
    let o = classify_outcome(&tally, Some(1), &IngestOutcome::Skipped, None);
    assert_eq!(o.run_status, RunStatus::Interrupted);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Transient));
    assert!(o.note.contains("stream disconnected"), "{}", o.note);

    // a turn.* result is not replaced by a later error
    let mut tally = StreamTally {
        result: Some(turn_failed.clone()),
        ..Default::default()
    };
    note_harness_error(&mut tally, quota);
    assert_eq!(tally.result.as_ref(), Some(&turn_failed));
}

/// AC4: an ordinary codex failure (E6's bad model) is a plain failure that
/// carries codex's error text.
#[test]
fn a_codex_turn_failed_that_is_not_quota_is_a_failure_with_the_text() {
    use crate::adapter::codex::CodexAdapter;
    let line = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/codex/exec-error-bad-model.jsonl"
    ))
    .lines()
    .find(|l| l.contains("turn.failed"))
    .unwrap();
    let StreamEvent::Result(r) = CodexAdapter { bin: "codex".into() }.parse_stream_line(line)
    else {
        panic!("turn.failed must parse as the result");
    };
    let tally = StreamTally { result: Some(r), ..Default::default() };
    let o = classify_outcome(&tally, Some(1), &IngestOutcome::Skipped, None);
    assert_eq!(o.run_status, RunStatus::Failed);
    assert_eq!(o.interrupted_reason, None);
    assert!(o.note.contains("model is not supported"), "{}", o.note);
}

/// FC-1 (H3): a busy service is a transient interruption, not quota.
#[test]
fn an_overloaded_error_is_a_transient_interruption_not_quota() {
    let o = classify_outcome(
        &err_result("Repeated 529 Overloaded errors", None),
        Some(1),
        &IngestOutcome::Skipped,
        None,
    );
    assert_eq!(o.run_status, RunStatus::Interrupted);
    assert_eq!(o.task_status, TaskStatus::Interrupted);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Transient));
    assert!(o.note.starts_with("transient interruption:"), "{}", o.note);
}

/// FC-1: quota is checked before the transient markers.
#[test]
fn a_text_with_429_and_overloaded_is_still_quota() {
    let o = classify_outcome(
        &err_result("Request rejected (429): overloaded", None),
        Some(1),
        &IngestOutcome::Skipped,
        None,
    );
    assert_eq!(o.run_status, RunStatus::Interrupted);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Quota));
}

/// FC-1 (H1): the three recorded agy failures (2026-09-29 Linux
/// acceptance, agy 1.2.12; tests/fixtures/antigravity/README.md), with the
/// exit codes agy really gave: 3, 1, and 0.
#[test]
fn the_recorded_agy_network_failures_are_transient_interruptions() {
    use crate::adapter::antigravity::AntigravityAdapter;
    let agy = AntigravityAdapter { bin: "agy".into() };
    for (line, exit, sentence) in [
        (
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/antigravity/fc1-network-issue-result.jsonl"
            )),
            Some(3),
            "There was a network issue",
        ),
        (
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/antigravity/fc1-eligibility-tls-timeout-result.jsonl"
            )),
            Some(1),
            "Eligibility check failed",
        ),
        (
            include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/antigravity/fc1-stream-interrupted-result.jsonl"
            )),
            Some(0),
            "The stream was interrupted",
        ),
    ] {
        let StreamEvent::Result(r) = agy.parse_stream_line(line.trim_end()) else {
            panic!("{sentence}: the fixture must parse as the result");
        };
        let tally = StreamTally { result: Some(r), ..Default::default() };
        let o = classify_outcome(&tally, exit, &IngestOutcome::Skipped, None);
        assert_eq!(o.run_status, RunStatus::Interrupted, "{sentence}");
        assert_eq!(o.interrupted_reason, Some(InterruptedReason::Transient), "{sentence}");
        assert!(o.note.contains(sentence), "{}", o.note);
    }
}

/// FC-1 (H1): a real Claude Code result line (2.1.283, Linux, recorded
/// from a run on the operator's machine in this repo's state directory;
/// tests/fixtures/stream-oauth-refresh-failed.jsonl). Its own text says
/// the failure is usually transient.
#[test]
fn the_recorded_claude_oauth_refresh_failure_is_transient() {
    let line = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stream-oauth-refresh-failed.jsonl"
    ));
    let StreamEvent::Result(r) = claude_code::ClaudeCodeAdapter { bin: "claude".into() }
        .parse_stream_line(line.trim_end())
    else {
        panic!("the fixture must parse as the result");
    };
    let tally = StreamTally { result: Some(r), ..Default::default() };
    let o = classify_outcome(&tally, Some(1), &IngestOutcome::Skipped, None);
    assert_eq!(o.run_status, RunStatus::Interrupted);
    assert_eq!(o.interrupted_reason, Some(InterruptedReason::Transient));
}

#[test]
fn our_own_kill_reasons_outrank_whatever_the_child_printed() {
    // T10 #41: quota-shaped text, not a plain `ok_result`, so this proves
    // our own kill reasons outrank the classifier's *own* quota
    // detection specifically - not merely outrank a boring success.
    let quota_text = err_result("Request rejected (429): usage limit reached", Some("429"));
    let cancelled = classify_outcome(
        &quota_text,
        Some(1),
        &IngestOutcome::Skipped,
        Some(KillReason::Cancelled),
    );
    assert_eq!(cancelled.run_status, RunStatus::Cancelled);
    assert_eq!(cancelled.task_status, TaskStatus::Cancelled);
    assert_eq!(cancelled.interrupted_reason, Some(InterruptedReason::User));

    let timed_out = classify_outcome(
        &quota_text,
        Some(1),
        &IngestOutcome::Skipped,
        Some(KillReason::Timeout),
    );
    assert_eq!(timed_out.run_status, RunStatus::Failed);
    assert_eq!(timed_out.task_status, TaskStatus::Failed);
    assert_eq!(timed_out.interrupted_reason, Some(InterruptedReason::Timeout));
}

fn demo_run(run_id: &str, task_id: &str, account: &str) -> state::Run {
    state::Run {
        run_id: run_id.into(),
        task_id: task_id.into(),
        attempt: 1,
        role: "implementer".into(),
        provider: "anthropic".into(),
        account: account.into(),
        model: "sonnet".into(),
        harness_session_id: None,
        config_dir: "C:/cfg".into(),
        allowed_tools: "Bash".into(),
        worktree: "C:/wt".into(),
        base_commit: "abc".into(),
        candidate_commit: None,
        spec: "{}".into(),
        status: "running".into(),
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
        timeout_seconds: 1800,
        run_dir: "C:/rd".into(),
        error_text: None,
        harness_bin: None,
    }
}

/// MODELS-1 (AC4): what the bookkeeping of a model-unavailable run
/// writes, read back through the db modules' public functions: one
/// `model_unavailable` quota event (model, run, task, both structured
/// error names, the text and the plan stored at the time), one
/// `model_unavailable` run event, no `rate_limited` / `quota_exceeded`,
/// and the profile still `active`. Any other outcome writes nothing; a
/// global.db failure lands in `status_change_detail` and the run event
/// is still written.
#[test]
fn record_model_unavailable_writes_the_quota_and_run_events_and_leaves_the_profile_alone() {
    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrations::migrate_state(&conn).unwrap();
    state::insert_task(
        &conn,
        &state::Task {
            task_id: "T1".into(),
            title: "t".into(),
            spec: "{}".into(),
            risk: "normal".into(),
            status: "running".into(),
            base_commit: "abc".into(),
            created_at: "2026-10-19T00:00:00Z".into(),
            updated_at: "2026-10-19T00:00:00Z".into(),
        },
    )
    .unwrap();
    let mut run = demo_run("R1", "T1", "claude-b");
    run.model = "fable".into();
    state::insert_run(&conn, &run).unwrap();
    let gconn = Connection::open_in_memory().unwrap();
    crate::db::migrations::migrate_global(&gconn).unwrap();
    gconn.execute("PRAGMA foreign_keys = ON", []).unwrap();
    global::insert_profile(
        &gconn,
        &global::Profile {
            profile_id: "claude-b".into(),
            provider: "anthropic".into(),
            harness: "claude-code".into(),
            account_label: None,
            config_dir: "C:/cfg".into(),
            default_models: None,
            status: "active".into(),
        },
    )
    .unwrap();
    global::set_profile_plan(&gconn, "claude-b", "pro", "2026-10-19T00:00:00Z").unwrap();
    let run_dir = tempfile::tempdir().unwrap();
    let credits_text = "Fable 5.1 requires usage credits. Switch to another model, or manage usage credits at https://claude.ai/settings/usage?from=cc_cli_limit_message, to continue.";
    let result = ResultEvent {
        session_id: Some("s".into()),
        is_error: true,
        text: Some(credits_text.into()),
        api_error_status: Some("429".into()),
        api_error: Some("model_requires_usage_credits".into()),
        api_error_code: Some("credits_required".into()),
        ..Default::default()
    };
    let unavailable = Outcome {
        run_status: RunStatus::Failed,
        task_status: TaskStatus::Failed,
        interrupted_reason: None,
        note: format!("{MODEL_UNAVAILABLE_PREFIX}{credits_text}"),
    };

    // another failure: nothing is written
    let plain = Outcome { note: "harness reported failure".into(), ..unavailable.clone() };
    let mut detail = serde_json::json!({"to": "failed"});
    record_model_unavailable(&conn, &gconn, run_dir.path(), &run, &plain, Some(&result), &mut detail);
    assert!(global::list_quota_events(&gconn, "claude-b").unwrap().is_empty());
    assert!(state::events_since(&conn, 0, None).unwrap().is_empty());

    record_model_unavailable(&conn, &gconn, run_dir.path(), &run, &unavailable, Some(&result), &mut detail);
    assert_eq!(detail, serde_json::json!({"to": "failed"}), "no bookkeeping error");
    let quota = global::list_quota_events(&gconn, "claude-b").unwrap();
    assert_eq!(quota.len(), 1, "{quota:?}");
    assert_eq!(quota[0].kind, crate::model_list::KIND_MODEL_UNAVAILABLE);
    let d: serde_json::Value = serde_json::from_str(&quota[0].detail).unwrap();
    assert_eq!(
        d,
        serde_json::json!({
            "model": "fable", "run_id": "R1", "task_id": "T1",
            "api_error": "model_requires_usage_credits", "api_error_code": "credits_required",
            "error_text": credits_text, "plan": "pro",
        })
    );
    let events = state::events_since(&conn, 0, None).unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].event_type, event_type::MODEL_UNAVAILABLE);
    assert_eq!(events[0].run_id.as_deref(), Some("R1"));
    assert_eq!(events[0].task_id.as_deref(), Some("T1"));
    let d: serde_json::Value = serde_json::from_str(&events[0].detail).unwrap();
    assert_eq!(
        d,
        serde_json::json!({
            "profile": "claude-b", "model": "fable",
            "api_error": "model_requires_usage_credits",
            "note": format!("{MODEL_UNAVAILABLE_PREFIX}{credits_text}"),
        })
    );
    assert_eq!(global::get_profile(&gconn, "claude-b").unwrap().unwrap().status, "active");
    // the launch gate and `thoth models` now see the model as unavailable
    let plan = crate::model_list::current_plan(&gconn, "claude-b");
    let u = crate::model_list::unavailable_now(&quota, plan.as_deref(), "FABLE").unwrap();
    assert_eq!((u.reason.as_str(), u.run_id.as_deref()), (credits_text, Some("R1")));

    // no profile row: the quota event cannot be written, the run event
    // still is, and the failure is in the status_change detail
    let orphan = demo_run("R2", "T1", "ghost-profile");
    state::insert_run(&conn, &orphan).unwrap();
    let mut detail = serde_json::json!({"to": "failed"});
    record_model_unavailable(&conn, &gconn, run_dir.path(), &orphan, &unavailable, Some(&result), &mut detail);
    assert!(detail["model_bookkeeping_error"].is_string(), "{detail}");
    assert_eq!(state::events_since(&conn, 0, None).unwrap().len(), 2);
    assert!(global::list_quota_events(&gconn, "ghost-profile").unwrap().is_empty());
}

/// I1 (fix round 1): the profile row is gone (or never existed) by the
/// time the quota interruption is recorded - a plausible race with a
/// concurrent `profile remove` - so `insert_quota_event`'s FK onto
/// `profiles` fails. The function must surface that as an `Err`, not
/// panic.
#[test]
fn record_quota_interruption_returns_err_without_panicking_when_the_profile_is_gone() {
    let gconn = Connection::open_in_memory().unwrap();
    crate::db::migrations::migrate_global(&gconn).unwrap();
    gconn.execute("PRAGMA foreign_keys = ON", []).unwrap();
    // Deliberately no `profiles` row for "ghost-profile".
    let run = demo_run("R1", "T1", "ghost-profile");

    let result = record_quota_interruption(&gconn, &run, "quota interruption: 429", None);
    assert!(result.is_err(), "expected an FK error, got Ok");
}

/// I1 (fix round 1): the caller path (`finish_and_settle`, the tail of
/// `run_worker_inner`) must tolerate `record_quota_interruption` failing -
/// the run still has to land `interrupted`/`quota` via `settle_terminal`,
/// not `failed` with no reason.
#[test]
fn finish_and_settle_still_settles_the_run_when_quota_bookkeeping_fails() {
    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrations::migrate_state(&conn).unwrap();
    state::insert_task(
        &conn,
        &state::Task {
            task_id: "T1".into(),
            title: "t".into(),
            spec: "{}".into(),
            risk: "normal".into(),
            status: "running".into(),
            base_commit: "abc".into(),
            created_at: "2026-09-15T00:00:00Z".into(),
            updated_at: "2026-09-15T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run = demo_run("R1", "T1", "ghost-profile");
    state::insert_run(&conn, &run).unwrap();

    // A global.db with no `profiles` row at all: `record_quota_interruption`
    // must fail here too.
    let gconn = Connection::open_in_memory().unwrap();
    crate::db::migrations::migrate_global(&gconn).unwrap();
    gconn.execute("PRAGMA foreign_keys = ON", []).unwrap();

    let outcome = Outcome {
        run_status: RunStatus::Interrupted,
        task_status: TaskStatus::Interrupted,
        interrupted_reason: Some(InterruptedReason::Quota),
        note: "quota interruption: 429".into(),
    };
    let finish = RunFinish {
        status: outcome.run_status,
        interrupted_reason: outcome.interrupted_reason,
        exit_code: Some(1),
        candidate_commit: None,
        error_text: Some(outcome.note.clone()),
        ended_at: "2026-09-15T00:01:00Z".into(),
    };
    let run_dir = tempfile::tempdir().unwrap();

    finish_and_settle(
        &conn,
        &gconn,
        run_dir.path(),
        &run,
        &outcome,
        &finish,
        Some(TaskStatusWrite::unconditional(TaskStatus::Interrupted)),
        Some(serde_json::json!({"reason": "quota"})),
        serde_json::json!({"to": "interrupted"}),
    )
    .expect("settle_terminal must still run even though quota bookkeeping failed");

    let rstatus: String = conn
        .query_row("SELECT status FROM runs WHERE run_id = ?1", ["R1"], |r| r.get(0))
        .unwrap();
    assert_eq!(rstatus, "interrupted", "the run must still be settled, not left stuck");

    let tstatus: String = conn
        .query_row("SELECT status FROM tasks WHERE task_id = ?1", ["T1"], |r| r.get(0))
        .unwrap();
    assert_eq!(tstatus, "interrupted");

    // #46: the bookkeeping failure must be visible in the status_change
    // event detail, not only in runner-error.log.
    let detail: String = conn
        .query_row(
            "SELECT detail FROM events WHERE type = 'status_change' AND run_id = ?1",
            ["R1"],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        detail.contains("quota_bookkeeping_error"),
        "detail was: {detail}"
    );
}

/// H2: a reviewer run's process outcome must not move `tasks.status`. This is
/// the `None` path through `settle_terminal` - the run settles, the task does
/// not budge - and it is what lets `thoth review start` be retried after a
/// reviewer crash.
#[test]
fn settle_terminal_with_no_task_status_settles_the_run_and_leaves_the_task_alone() {
    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrations::migrate_state(&conn).unwrap();
    state::insert_task(
        &conn,
        &state::Task {
            task_id: "T1".into(),
            title: "t".into(),
            spec: "{}".into(),
            risk: "normal".into(),
            status: "in_review".into(),
            base_commit: "abc".into(),
            created_at: "2026-09-16T00:00:00Z".into(),
            updated_at: "2026-09-16T00:00:00Z".into(),
        },
    )
    .unwrap();
    let mut run = demo_run("R1", "T1", "claude-a");
    run.role = crate::model::role::REVIEWER.to_string();
    state::insert_run(&conn, &run).unwrap();

    let finish = RunFinish {
        status: RunStatus::Interrupted,
        interrupted_reason: Some(InterruptedReason::Crash),
        exit_code: Some(137),
        candidate_commit: None,
        error_text: Some("child exited without a result event".into()),
        ended_at: "2026-09-16T00:01:00Z".into(),
    };
    assert!(settle_terminal(
        &conn,
        &run,
        &finish,
        None,
        None,
        serde_json::json!({"entity": "run", "id": "R1", "to": "interrupted"}),
    )
    .unwrap());

    let rstatus: String = conn
        .query_row("SELECT status FROM runs WHERE run_id = 'R1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rstatus, "interrupted", "the run must still be settled");
    let tstatus: String = conn
        .query_row("SELECT status FROM tasks WHERE task_id = 'T1'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        tstatus, "in_review",
        "a crashed review must leave the task exactly where it was"
    );
    // and no task-level status_change event was invented
    let task_changes: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM events WHERE type = 'status_change'
             AND detail LIKE '%\"entity\":\"task\"%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(task_changes, 0);
}

/// Task 5 fix round 1: rule 5 is ingest's only safety rule, so a risk the
/// runner cannot read must fail **closed**. The happy path returns the task
/// row's own risk; a task id with no row returns `high` and leaves a line
/// in `runner-error.log` naming the task, so the degradation is visible
/// rather than silent.
#[test]
fn task_risk_for_ingest_uses_the_task_row_and_fails_closed_when_it_cannot() {
    let conn = Connection::open_in_memory().unwrap();
    crate::db::migrations::migrate_state(&conn).unwrap();
    state::insert_task(
        &conn,
        &state::Task {
            task_id: "T1".into(),
            title: "t".into(),
            spec: "{}".into(),
            risk: "low".into(),
            status: "running".into(),
            base_commit: "abc".into(),
            created_at: "2026-09-15T00:00:00Z".into(),
            updated_at: "2026-09-15T00:00:00Z".into(),
        },
    )
    .unwrap();
    let run_dir = tempfile::tempdir().unwrap();

    // the row is there: its own risk, and nothing logged
    assert_eq!(task_risk_for_ingest(&conn, "T1", run_dir.path()), "low");
    assert!(
        !run_dir.path().join("runner-error.log").exists(),
        "a resolvable risk must not log a degradation"
    );

    // no row for this id on an otherwise healthy state.db
    assert_eq!(
        task_risk_for_ingest(&conn, "T-MISSING", run_dir.path()),
        crate::model::Risk::High.as_str(),
        "an unreadable risk must apply the strictest rule, not the laxest"
    );
    let log = std::fs::read_to_string(run_dir.path().join("runner-error.log"))
        .expect("the fail-closed path must leave a runner-error.log line");
    assert!(log.contains("T-MISSING"), "log was: {log}");
    assert!(log.contains("high"), "log was: {log}");
}

// FC-2 AC1 (u1).
#[test]
fn retry_waits_default_to_30_and_120_seconds_unless_the_test_seam_overrides() {
    for raw in [None, Some(""), Some("abc"), Some("1"), Some("1,2,3"), Some("-1,2")] {
        assert_eq!(parse_retry_waits(raw), [30, 120], "{raw:?}");
    }
    assert_eq!(parse_retry_waits(Some("1,2")), [1, 2]);
    assert_eq!(parse_retry_waits(Some(" 0 , 0 ")), [0, 0]);
    assert_eq!(RETRY_WAIT_SECONDS, [30, 120]);
    assert_eq!(MAX_RETRIES, 2);
}

// FC-2 AC1 (u2).
#[test]
fn wants_retry_only_for_a_transient_result_without_our_own_kill() {
    let overloaded = err_result("Repeated 529 Overloaded errors", Some("529"));
    assert!(wants_retry(&overloaded, Some(1), None));
    assert!(!wants_retry(&overloaded, Some(1), Some(KillReason::Timeout)));
    assert!(!wants_retry(&overloaded, Some(1), Some(KillReason::Cancelled)));
    assert!(!wants_retry(
        &err_result("Request rejected (429): overloaded", Some("429")),
        Some(1),
        None
    ));
    assert!(!wants_retry(&err_result("tool call exploded", None), Some(1), None));
    assert!(!wants_retry(&StreamTally::default(), Some(1), None));
    assert!(!wants_retry(&ok_result(), Some(0), None));
}

// FC-2 AC1 (u3).
#[test]
fn wait_fits_requires_the_whole_wait_to_end_before_the_deadline() {
    let s = Duration::from_secs;
    assert!(wait_fits(s(10), s(30), 60));
    assert!(wait_fits(s(29), s(30), 60));
    assert!(!wait_fits(s(30), s(30), 60));
    assert!(!wait_fits(s(31), s(30), 60));
    assert!(!wait_fits(s(0), s(30), 30));
    assert!(wait_fits(s(0), s(0), 1));
}

// FC-2 AC1 (u4): is_review, names_session_late, session_id, known, seen.
#[test]
fn retry_launch_resumes_a_known_session_and_restarts_otherwise() {
    let resume = |s: &str| RetryLaunch::Resume(s.to_string());
    assert_eq!(retry_launch(false, false, "S", None, None), resume("S"));
    assert_eq!(retry_launch(false, false, "S", None, Some("S")), resume("S"));
    assert_eq!(retry_launch(false, true, "S", None, Some("T")), resume("T"));
    assert_eq!(retry_launch(false, true, "S", None, None), RetryLaunch::FreshLateSession);
    assert_eq!(retry_launch(true, false, "S", None, Some("S")), RetryLaunch::FreshNamedSession);
    assert_eq!(retry_launch(true, true, "S", None, Some("T")), RetryLaunch::FreshLateSession);
    // a resumed run that failed again before `init`
    assert_eq!(retry_launch(false, true, "S", Some("S"), None), resume("S"));
    assert_eq!(retry_launch(false, true, "S", Some("S"), Some("T")), resume("T"));
}
