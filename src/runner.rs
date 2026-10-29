use crate::adapter::claude_code;
use crate::adapter::{
    codex, looks_like_quota, looks_like_transient, HarnessAdapter, LaunchHooks, LaunchRequest, ResultEvent, RunPlace,
    StreamEvent,
};
use crate::config::Paths;
use crate::db;
use crate::db::global;
use crate::db::state::{self, NewEvent, RunFinish};
use crate::evidence;
use crate::ids;
use crate::ingest::{self, IngestOutcome};
use crate::model::{event_type, InterruptedReason, RunStatus, TaskStatus};
use crate::process::{self, ChildSpec};
use crate::spec::TaskSpec;
use crate::worktree;
use anyhow::{anyhow, bail, Result};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const HEARTBEAT_EVERY: Duration = Duration::from_secs(5);
const POLL_EVERY: Duration = Duration::from_secs(1);
/// How long to keep draining stdout after the child exits before giving up on
/// the reader thread. A grandchild that inherited the child's stdout handle can
/// hold the pipe open past the child's own exit (03-process-and-worktree.md:69:
/// clean teardown of the Bash tool subtree depends on claude cooperating), and a
/// blind `join()` there would wedge the runner forever, leaving the run stuck in
/// `running` with a heartbeat that never advances again.
const STREAM_DRAIN_GRACE: Duration = Duration::from_secs(10);
/// I1 (fix round 1): how long to keep polling a child non-blockingly after
/// `kill_tree` itself reported failure (the pid was still listed after its own
/// ~2 s confirmation loop). A blind `child.wait()` in that situation can wedge
/// the runner forever on a genuinely stuck process; this bounds the wait
/// instead, so the runner always eventually settles the row.
const KILL_WAIT_GRACE: Duration = Duration::from_secs(30);
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
const DB_POLL_FAILURE_LIMIT: u32 = 10;

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
fn needs_login_check(outcome: &Outcome) -> bool {
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
fn apply_login_check(
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

/// The harness a run's stream comes from, and what the runner collects from
/// it besides the `StreamTally`: codex's commands become evidence as they
/// arrive, and its item kinds are counted; anything else the harness adds
/// goes through its launch hooks.
struct StreamState<'a> {
    adapter: Box<dyn HarnessAdapter>,
    /// The adapter's per-launch additions (`HarnessAdapter::launch_hooks`).
    launch: Box<dyn LaunchHooks>,
    codex: bool,
    /// A reviewer run: its commands are never recorded as evidence.
    reviewer: bool,
    /// A worker or source-check reviewer of a research task: codex has the
    /// web search on, so a `web_search` item is expected.
    research: bool,
    worker_pid: u32,
    place: RunPlace<'a>,
    /// Evidence rows recorded straight from the stream (codex) or by the
    /// launch hooks.
    evidence_recorded: u64,
    /// Item kinds outside `codex::EXPECTED_ITEM_TYPES`, each with the ids
    /// seen, so a started-then-completed item counts once.
    unexpected_items: BTreeMap<String, BTreeSet<String>>,
}

impl StreamState<'_> {
    /// Collab/subagent items: codex's own form of a spawned subagent.
    fn subagents(&self) -> i64 {
        self.unexpected_items
            .iter()
            .filter(|(t, _)| t.contains("collab") || t.contains("spawn") || t.contains("subagent"))
            .map(|(_, ids)| ids.len() as i64)
            .sum()
    }

    /// The launch hooks' commands whose outcome is now known (all of them
    /// with `last`), counted into `evidence_recorded`.
    fn settle_commands(&mut self, conn: &Connection, run: &state::Run, session_id: Option<&str>, last: bool) {
        self.evidence_recorded += self.launch.settle_commands(conn, run, &self.place, session_id, last);
    }

    /// The stream has ended: the launch hooks write what they held back.
    fn after_stream(&mut self, conn: &Connection, run: &state::Run) {
        self.launch.after_stream(conn, run, &self.place);
    }

    /// Records one codex command as `command` evidence and republishes the
    /// index, so `.agent/evidence-index.json` is current before the worker
    /// writes result.json. Best-effort like the inbox drain: a failure is
    /// logged, never fatal.
    fn record_command(
        &mut self,
        conn: &Connection,
        run: &state::Run,
        session_id: Option<&str>,
        c: (&Option<String>, &str, &str, Option<i64>, &Option<String>),
    ) {
        let (item_id, command, output, exit_code, status) = c;
        let ctx = evidence::EvidenceContext {
            paths: self.place.paths,
            repo_id: self.place.repo_id,
            task_id: &run.task_id,
            run_id: &run.run_id,
        };
        let row = evidence::StreamCommand {
            cmd: command,
            cwd: &run.worktree,
            exit_code,
            status: status.as_deref(),
            output,
            truncated: codex::output_was_truncated(output),
            session_id,
            item_id: item_id.as_deref(),
            event: "codex_command_execution",
            step_index: None,
            exit_code_source: None,
        };
        match evidence::record_stream_command(conn, &ctx, &row) {
            Ok(_) => {
                self.evidence_recorded += 1;
                if let Err(e) = evidence::refresh_index(conn, &run.task_id, &self.place.worktree) {
                    log_runner_error(
                        &self.place.run_dir,
                        &format!("rewriting the evidence index failed: {e:#}"),
                    );
                }
            }
            Err(e) => log_runner_error(
                &self.place.run_dir,
                &format!("recording command evidence failed: {e:#}"),
            ),
        }
    }
}

/// Codex's top-level `error`: the failure, unless a result line (`turn.failed`,
/// or a `turn.completed` after a recovered error) follows and replaces it. A
/// newer `error` replaces an older one - a transient error followed by a
/// usage-limit error is a quota stop - but never a `turn.*` result, which is
/// told apart by the subtype `error_result` gives it (the event type).
/// The `run_usage` event, plus the `subagents_spawned` warning when any were.
pub(crate) fn append_usage(conn: &Connection, run: &state::Run, usage: serde_json::Value, subagents: Option<i64>) {
    let _ = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::RUN_USAGE.to_string(),
            detail: usage,
        },
    );
    if let Some(n) = subagents.filter(|n| *n > 0) {
        let _ = state::append_event(
            conn,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::SUBAGENTS_SPAWNED.to_string(),
                detail: serde_json::json!({
                    "spawned": n,
                    "note": "the worker launched subagents; judge whether that was acceptable",
                }),
            },
        );
    }
}

fn note_harness_error(tally: &mut StreamTally, r: ResultEvent) {
    let replaceable = match &tally.result {
        None => true,
        Some(prev) => prev.subtype == r.subtype,
    };
    if replaceable {
        tally.result = Some(r);
    }
}

fn handle_line(
    conn: &rusqlite::Connection,
    run: &state::Run,
    stream: &mut StreamState<'_>,
    line: &str,
    tally: &mut StreamTally,
) {
    tally.lines += 1;
    match stream.adapter.parse_stream_line(line) {
        StreamEvent::Init {
            session_id,
            model,
            cwd,
            tools,
        } => {
            // `cwd` only ever appears on this one event (02-evidence-capture.md:121)
            // and `model` is the model the harness actually resolved, which may not
            // be the alias we asked for - both worth having in the audit trail.
            // AGY-1 (AC3): `tools` is the list agy reported on `init`, as
            // written, and `tool_count` its length; both null when the
            // harness reported none (Claude, codex, or an unusable list). A
            // record for comparing agy versions later, not a permission list.
            let mut detail = serde_json::json!({
                "session_id": session_id,
                "model": model,
                "cwd": cwd,
                "tools": tools,
                "tool_count": tools.as_ref().map(Vec::len),
            });
            // The npm `codex` launcher's pid reads `codex` right after spawn
            // and `node` about 0.5 s later; by `thread.started` it has settled,
            // so this is the image `cancel` should expect.
            if stream.codex {
                detail["worker_image"] =
                    serde_json::json!(process::image_name(stream.worker_pid).ok().flatten());
            }
            let _ = state::append_event(
                conn,
                &NewEvent {
                    task_id: Some(run.task_id.clone()),
                    run_id: Some(run.run_id.clone()),
                    event_type: event_type::HARNESS_INIT.to_string(),
                    detail,
                },
            );
            if let Some(id) = session_id.as_deref() {
                stream.launch.on_session(id);
            }
            if session_id.is_some() {
                // Codex names its thread only here, and a run interrupted
                // before `turn.completed` needs it to resume; so does any
                // harness that names its session late.
                if stream.adapter.names_session_late() {
                    let _ = state::set_run_session_and_usage(
                        conn,
                        &run.run_id,
                        session_id.as_deref(),
                        None,
                        None,
                    );
                }
                tally.session_id = session_id;
            }
        }
        StreamEvent::Assistant { session_id } => {
            if tally.session_id.is_none() {
                tally.session_id = session_id;
            }
        }
        StreamEvent::RateLimitWarning {
            rate_limit_type,
            utilization,
            resets_at,
            windows,
        } => {
            tally.saw_rate_limit_warning = true;
            // S3-12: every window, not only the five-hour one; `run_quota`
            // reads this run's five-hour readings back from these events.
            let windows: Vec<serde_json::Value> = windows
                .iter()
                .map(|w| {
                    serde_json::json!({
                        "name": w.name,
                        "utilization": w.utilization,
                        "resets_at": w.resets_at,
                    })
                })
                .collect();
            let _ = state::append_event(
                conn,
                &NewEvent {
                    task_id: Some(run.task_id.clone()),
                    run_id: Some(run.run_id.clone()),
                    event_type: event_type::RATE_LIMIT_WARNING.to_string(),
                    detail: serde_json::json!({
                        "rate_limit_type": rate_limit_type,
                        "utilization": utilization,
                        "resets_at": resets_at,
                        "windows": windows,
                    }),
                },
            );
        }
        StreamEvent::ApiRetry {
            attempt,
            max_retries,
            error,
            error_status,
        } => {
            tally.saw_api_retry = true;
            let _ = state::append_event(
                conn,
                &NewEvent {
                    task_id: Some(run.task_id.clone()),
                    run_id: Some(run.run_id.clone()),
                    event_type: event_type::API_RETRY.to_string(),
                    detail: serde_json::json!({
                        "attempt": attempt,
                        "max_retries": max_retries,
                        "error": error,
                        "error_status": error_status,
                    }),
                },
            );
        }
        StreamEvent::Result(mut r) => {
            // Codex's stream names no model and has no subagent count, so the
            // requested model takes turn.completed's output tokens and the
            // subagent count is the collab items seen.
            if stream.codex {
                r.model_usage = r.output_tokens.map(|t| vec![(run.model.clone(), t)]);
                r.subagents_spawned = Some(stream.subagents());
            }
            stream.launch.on_result(&mut r, &run.model);
            if r.session_id.is_some() {
                tally.session_id = r.session_id.clone();
            }
            let _ = state::set_run_session_and_usage(
                conn,
                &run.run_id,
                tally.session_id.as_deref(),
                None,
                None,
            );
            // The token kinds go in beside it: added up for a per-turn usage
            // (Claude), replaced for a cumulative one (codex, antigravity).
            let _ = state::record_run_tokens(
                conn,
                &run.run_id,
                &state::RunTokens {
                    input: r.input_tokens,
                    cache_read: r.cache_read_tokens,
                    cache_write: r.cache_write_tokens,
                    output: r.output_tokens,
                    reasoning: r.reasoning_tokens,
                    total: r.total_tokens,
                },
                r.usage_cumulative,
            );
            // Which models ran and how many subagents were spawned. Names are
            // informational (Claude Code's own background work adds a little
            // haiku to a normal run); a spawned subagent is the signal. Neither
            // event changes a status.
            let models: serde_json::Map<String, serde_json::Value> = r
                .model_usage
                .iter()
                .flatten()
                .map(|(m, t)| (m.clone(), serde_json::json!(t)))
                .collect();
            let mut usage = serde_json::json!({
                "models": if r.model_usage.is_some() {
                    serde_json::Value::Object(models)
                } else {
                    serde_json::Value::Null
                },
                "subagents_spawned": r.subagents_spawned,
            });
            if stream.codex {
                usage["unexpected_item_types"] =
                    serde_json::json!(stream.unexpected_items.keys().collect::<Vec<_>>());
            }
            stream.launch.extend_usage(&mut usage);
            // Hooks that hold it back write it from `after_stream`, which
            // every path out of `run_worker_inner` after the spawn calls: the
            // DB poll give-up, the kill-wait deadline, a `try_wait` or `wait`
            // error (both return through `?`), and the common exit after the
            // loop that clean exits, crashes, timeouts and cancels all break to.
            if let Some((usage, subagents)) = stream.launch.hold_usage(usage, r.subagents_spawned) {
                append_usage(conn, run, usage, subagents);
            }
            tally.result = Some(r);
        }
        StreamEvent::Unparsable { raw_len } => {
            tally.unparsable_lines += 1;
            tally.unparsable_bytes += raw_len as u64;
        }
        // Codex's top-level `error`: the failure, unless a result line
        // (`turn.failed`, or a `turn.completed` after a recovered error)
        // follows and replaces it.
        StreamEvent::HarnessError(r) => note_harness_error(tally, r),
        // A reviewer's own commands are not evidence about the
        // implementation (brief §1, Open question 4): a codex reviewer's
        // `cat`s are left in stream.jsonl only, as a Claude reviewer's are.
        StreamEvent::CommandCompleted { .. } if stream.reviewer => {}
        StreamEvent::CommandCompleted {
            item_id,
            command,
            output,
            exit_code,
            status,
        } => {
            let session = tally.session_id.clone();
            stream.record_command(
                conn,
                run,
                session.as_deref(),
                (&item_id, &command, &output, exit_code, &status),
            );
        }
        StreamEvent::Item { id, item_type } => {
            let expected = codex::EXPECTED_ITEM_TYPES.contains(&item_type.as_str())
                || (stream.research && codex::RESEARCH_ITEM_TYPES.contains(&item_type.as_str()));
            if !expected {
                let ids = stream.unexpected_items.entry(item_type).or_default();
                let id = id.unwrap_or_else(|| format!("#{}", ids.len()));
                ids.insert(id);
            }
        }
        // Anything else is the launch hooks' to act on (or ignore).
        event => stream.launch.on_event(&event),
    }
}

/// brief section 10: a quota interruption writes a `quota_events` row and parks
/// the profile so a human (or `profile check`) has to clear it.
///
/// Fix round 1 (I1): the two global.db writes are wrapped in one transaction -
/// a parked profile can never land without its `quota_events` row, and vice
/// versa - and this function's `Result` is never `?`-propagated by its caller;
/// see `finish_and_settle`.
fn record_quota_interruption(
    gconn: &Connection,
    run: &state::Run,
    note: &str,
    quota_group: Option<&str>,
) -> Result<()> {
    let tx = gconn.unchecked_transaction()?;
    global::insert_quota_event(
        &tx,
        &ids::now_rfc3339(),
        &run.account,
        "rate_limited",
        &serde_json::json!({
            "run_id": run.run_id,
            "task_id": run.task_id,
            "error_text": note,
            "quota_group": quota_group,
        }),
    )?;
    global::set_profile_status(&tx, &run.account, "rate_limited")?;
    tx.commit()?;
    Ok(())
}

/// The tail of `run_worker_inner`: best-effort quota bookkeeping (I1 - a
/// `SQLITE_BUSY` from a concurrent `dispatch`/`profile check` must not be able
/// to turn a correctly detected quota interruption into a plain `failed` row
/// with no `interrupted_reason`, which would strand it off both the
/// `resume`/reroute recovery routes that hang off `interrupted`), then the one
/// state.db settle that always has to happen. Extracted from
/// `run_worker_inner` so a unit test can exercise "quota bookkeeping failed,
/// the run is still settled `interrupted`/`quota`" without spawning a child.
#[allow(clippy::too_many_arguments)]
fn finish_and_settle(
    conn: &Connection,
    gconn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    finish: &RunFinish,
    // `None` leaves `tasks.status` untouched - a reviewer run whose process
    // outcome must not move the task (see Task 7's rule: only a verdict may
    // move a task under review). A doc comment is not legal on a parameter, so
    // this is a plain one.
    task_status: Option<TaskStatusWrite>,
    interrupted_detail: Option<serde_json::Value>,
    mut status_change_detail: serde_json::Value,
) -> Result<()> {
    record_quota(conn, gconn, run_dir, run, outcome, &mut status_change_detail);
    record_login_failure(conn, gconn, run_dir, run, outcome, &mut status_change_detail);
    settle_or_log(conn, run_dir, run, outcome, finish, task_status, interrupted_detail, status_change_detail)
}

/// The first half of `finish_and_settle`: for a quota interruption, the
/// `quota_events` row, the parked profile and the `quota_exceeded` event.
/// Never fails; a bookkeeping failure goes into `status_change_detail`.
pub(crate) fn record_quota(
    conn: &Connection,
    gconn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    status_change_detail: &mut serde_json::Value,
) {
    if outcome.interrupted_reason == Some(InterruptedReason::Quota) {
        let quota_group = agy_quota_group(gconn, run);
        if let Err(e) = record_quota_interruption(gconn, run, &outcome.note, quota_group.as_deref()) {
            let msg = format!("{e:#}");
            log_runner_error(run_dir, &format!("record_quota_interruption failed: {msg}"));
            // #46 (final review): without this the profile is never parked
            // and no `quota_events` row exists, so nothing besides
            // `runner-error.log` (which no operator sees for a detached
            // runner) shows the bookkeeping failure. The run still correctly
            // settles `interrupted`/`quota` below; this just makes the
            // failure visible in the same `status_change` event without an
            // extra DB write.
            if let serde_json::Value::Object(map) = status_change_detail {
                map.insert(
                    "quota_bookkeeping_error".to_string(),
                    serde_json::Value::String(msg),
                );
            }
        }
        // Non-fatal like the other auxiliary events `handle_line` records: the
        // classification and the state.db settle below must go through
        // whatever this write does.
        let _ = state::append_event(
            conn,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::QUOTA_EXCEEDED.to_string(),
                detail: serde_json::json!({
                    "profile": run.account,
                    "note": outcome.note,
                    "quota_group": quota_group,
                }),
            },
        );
    }
}

/// MODELS-1: for a run Claude refused because the model needs usage credits
/// this account lacks, the `model_unavailable` quota event (what the launch
/// commands and `thoth models` read) and the run's `model_unavailable`
/// event. The profile is not parked and no `rate_limited` row is written.
/// Like `record_quota`, never fails: a global.db failure goes into
/// `status_change_detail` as `model_bookkeeping_error`.
pub(crate) fn record_model_unavailable(
    conn: &Connection,
    gconn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    result: Option<&ResultEvent>,
    status_change_detail: &mut serde_json::Value,
) {
    if !outcome.is_model_unavailable() {
        return;
    }
    let api_error = result.and_then(|r| r.api_error.clone());
    let api_error_code = result.and_then(|r| r.api_error_code.clone());
    let error_text = result.and_then(|r| r.text.clone());
    let plan = crate::model_list::current_plan(gconn, &run.account);
    if let Err(e) = global::insert_quota_event(
        gconn,
        &ids::now_rfc3339(),
        &run.account,
        crate::model_list::KIND_MODEL_UNAVAILABLE,
        &serde_json::json!({
            "model": run.model,
            "run_id": run.run_id,
            "task_id": run.task_id,
            "api_error": api_error,
            "api_error_code": api_error_code,
            "error_text": error_text,
            "plan": plan,
        }),
    ) {
        let msg = format!("{e:#}");
        log_runner_error(run_dir, &format!("record_model_unavailable failed: {msg}"));
        if let serde_json::Value::Object(map) = status_change_detail {
            map.insert("model_bookkeeping_error".to_string(), serde_json::Value::String(msg));
        }
    }
    let _ = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::MODEL_UNAVAILABLE.to_string(),
            detail: serde_json::json!({
                "profile": run.account,
                "model": run.model,
                "api_error": api_error,
                "note": outcome.note,
            }),
        },
    );
}

/// FC-1 (H4): which of an antigravity profile's quota groups the run's model
/// belongs to, from the profile's latest `live_quota` reading - exactly one
/// matching group, or `None`. `None` for every other harness, for a profile
/// that cannot be read, and when nobody has run `profile usage --refresh`
/// (then the whole profile is parked, as before).
fn agy_quota_group(gconn: &Connection, run: &state::Run) -> Option<String> {
    let profile = global::get_profile(gconn, &run.account).ok().flatten()?;
    if profile.harness != crate::adapter::antigravity::HARNESS_NAME {
        return None;
    }
    let windows = crate::commands::live_quota::read_agy_windows(gconn, &run.account).ok()?;
    let mut groups = crate::commands::live_quota::matching_groups(&windows, &run.model);
    if groups.len() == 1 {
        groups.pop()
    } else {
        None
    }
}

/// FC-1 (H2): the runner's auth check said the profile is not logged in.
/// Like `record_quota`, never fails: a global.db failure goes into
/// `status_change_detail` as `login_bookkeeping_error`.
fn record_login_failure(
    conn: &Connection,
    gconn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    status_change_detail: &mut serde_json::Value,
) {
    if outcome.interrupted_reason != Some(InterruptedReason::Auth) {
        return;
    }
    let write = || -> Result<()> {
        let tx = gconn.unchecked_transaction()?;
        global::insert_quota_event(
            &tx,
            &ids::now_rfc3339(),
            &run.account,
            "logged_out",
            &serde_json::json!({
                "run_id": run.run_id,
                "task_id": run.task_id,
                "error_text": outcome.note,
            }),
        )?;
        global::set_profile_status(&tx, &run.account, "logged_out")?;
        tx.commit()?;
        Ok(())
    };
    if let Err(e) = write() {
        let msg = format!("{e:#}");
        log_runner_error(run_dir, &format!("record_login_failure failed: {msg}"));
        if let serde_json::Value::Object(map) = status_change_detail {
            map.insert("login_bookkeeping_error".to_string(), serde_json::Value::String(msg));
        }
    }
    let _ = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::LOGIN_FAILED.to_string(),
            detail: serde_json::json!({
                "profile": run.account,
                "note": outcome.note,
            }),
        },
    );
}

/// The second half of `finish_and_settle`: the one state.db settle that
/// always has to happen.
#[allow(clippy::too_many_arguments)]
pub(crate) fn settle_or_log(
    conn: &Connection,
    run_dir: &Path,
    run: &state::Run,
    outcome: &Outcome,
    finish: &RunFinish,
    task_status: Option<TaskStatusWrite>,
    interrupted_detail: Option<serde_json::Value>,
    status_change_detail: serde_json::Value,
) -> Result<()> {
    // I2/I3: one transaction for finish_run + status_change + (maybe)
    // run_interrupted + set_task_status, so the runs/tasks rows can never
    // disagree after a partial write.
    if !settle_terminal(conn, run, finish, task_status, interrupted_detail, status_change_detail)? {
        // Important 1: somebody else (cancel's escalation) already settled
        // this run first. Non-fatal - the row already carries a terminal
        // outcome, just not this one - but worth a trace since it means the
        // two writers raced.
        log_runner_error(
            run_dir,
            &format!(
                "run {} was already settled (probably by cancel's escalation) before this outcome could be written: {}",
                run.run_id, outcome.note
            ),
        );
    }
    Ok(())
}

/// Detached runners have no stderr a human will ever see, so any error we
/// cannot otherwise surface also goes to a plain-text log next to the run's
/// other artefacts. Never itself fails loudly: a logging failure must not
/// compound whatever we were already trying to report.
/// `pub(crate)` (fix round 1): `evidence::InboxTailer`/`record_command` also
/// need this, since inbox IO errors and blob-degradation warnings belong in
/// the run's own log, not a separate one.
pub(crate) fn log_runner_error(run_dir: &Path, msg: &str) {
    if std::fs::create_dir_all(run_dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(run_dir.join("runner-error.log"))
    {
        let _ = writeln!(f, "[{}] {msg}", ids::now_rfc3339());
    }
}

/// The `tasks.risk` value ingest applies v6 §2.5 rule 5 with.
///
/// `tasks.risk`, not the run's frozen `spec` copy: the task row is what
/// `task update-spec` keeps current and what the Supervisor can see.
///
/// **Fails closed** (Task 5 fix round 1, controller ruling). Rule 5 is the one
/// *safety* rule in ingest, so a risk this function cannot read must not quietly
/// become `normal` and switch the rule off: a transient `SQLITE_BUSY` on a High
/// Risk task would then let an uncorroborated claim through. A missing row or a
/// read error is logged and reported as `high`, which is the strictest reading.
/// `thoth submit` already fails closed by propagating its own `get_task`
/// error (`commands::submit`), so both ingest entry points now agree.
///
/// The cost if the ruling is wrong is bounded and visible: a normal task's
/// result lands `unverified` instead of `in_review` and the operator re-submits.
fn task_risk_for_ingest(conn: &Connection, task_id: &str, run_dir: &Path) -> String {
    match state::get_task(conn, task_id) {
        Ok(Some(task)) => task.risk,
        Ok(None) => {
            log_runner_error(
                run_dir,
                &format!(
                    "task {task_id} has no row while resolving the risk for ingest; \
                     applying the strictest risk ('high') to v6 §2.5 rule 5"
                ),
            );
            crate::model::Risk::High.as_str().to_string()
        }
        Err(e) => {
            log_runner_error(
                run_dir,
                &format!(
                    "reading task {task_id} to resolve the risk for ingest failed: {e:#}; \
                     applying the strictest risk ('high') to v6 §2.5 rule 5"
                ),
            );
            crate::model::Risk::High.as_str().to_string()
        }
    }
}

/// The task's title, for the commit message thoth writes on a codex
/// worker's behalf (scope §2.6). Not safety-sensitive like
/// `task_risk_for_ingest` above - a title this cannot read just falls back to
/// the task id, which still names the task unambiguously.
fn task_title_for_commit(conn: &Connection, task_id: &str) -> String {
    match state::get_task(conn, task_id) {
        Ok(Some(task)) if !task.title.trim().is_empty() => task.title,
        _ => task_id.to_string(),
    }
}

/// Run-level terminal statuses (compared as the plain strings the
/// `runs.status` column stores). T8/T9 final-review consolidation (#28/#36):
/// this used to hardcode its own string match; it now defers to
/// `RunStatus::is_terminal` so the predicate has one definition. An
/// unparsable status (should never happen - `runs.status` is only ever
/// written from `RunStatus::as_str`) is conservatively treated as
/// non-terminal, matching the old match's fallback.
///
/// `pub(crate)` (fix round 2 / R4): `commands::verify::run`'s queued/running
/// guard now calls this too, instead of its own hardcoded `"queued" |
/// "running"` match, so there is exactly one definition of "is this run
/// still live" for a future non-terminal status to fall under automatically.
pub(crate) fn is_run_status_terminal(status: &str) -> bool {
    RunStatus::parse(status).map(|s| s.is_terminal()).unwrap_or(false)
}

/// The task statuses a reviewer's verdict is allowed to move the task out of.
///
/// Whole-branch review Important 1: a verdict is an answer to the question
/// "is *this* candidate acceptable", and that question is only open while the
/// task is `in_review`. Once a Supervisor has rerouted the task (it is
/// `running` again, or already `reject`/`pass` from another review), the
/// verdict is about a state of the world that no longer holds and must not be
/// written over whatever the task is doing now.
pub(crate) const VERDICT_ENTRY_STATUSES: &[TaskStatus] = &[TaskStatus::InReview];

/// What a settle should do to `tasks.status`.
///
/// Whole-branch review Important 1: `settle_terminal` used to take a plain
/// `Option<TaskStatus>` and write it with the **unconditional**
/// `state::set_task_status`. That is correct for an implementer run (the task
/// is `running` for that run's whole life, and this settle is what ends it)
/// and wrong for a reviewer run, whose settle can land seconds or minutes
/// after the Supervisor rerouted the task - stamping a stale `pass` over a
/// live implementer run. `only_from` is how the reviewer path says "only
/// while the task is still the thing I judged"; `None` keeps the Phase 1
/// behaviour for every other call site.
#[derive(Debug, Clone, Copy)]
pub(crate) struct TaskStatusWrite {
    pub to: TaskStatus,
    pub only_from: Option<&'static [TaskStatus]>,
}

impl TaskStatusWrite {
    /// Phase 1's behaviour: write `to` whatever the task currently says.
    pub(crate) fn unconditional(to: TaskStatus) -> Self {
        Self { to, only_from: None }
    }

    /// Write `to` only while the task's status is one of `allowed`; otherwise
    /// settle the run, leave the task alone, and record that this happened.
    pub(crate) fn conditional(allowed: &'static [TaskStatus], to: TaskStatus) -> Self {
        Self {
            to,
            only_from: Some(allowed),
        }
    }
}

/// I2/I3 (fix round 1): commits `finish_run`, one `status_change` event, an
/// optional `run_interrupted` event, and the task's terminal status under one
/// transaction, so the `runs` and `tasks` rows can never disagree after a
/// partial write.
///
/// Whole-branch review Important 1: `finish_run` is now conditional on the
/// run still being non-terminal, so this settle as a whole is too - if
/// `finish_run` reports it did not apply (somebody else already settled this
/// run, e.g. the runner's own tail racing `cancel`'s escalation, or vice
/// versa), the transaction is rolled back without touching the
/// `status_change`/`run_interrupted` events or `tasks.status` at all, and
/// this returns `Ok(false)`. Callers must treat `false` as "nothing to do
/// here; the other writer's outcome stands" rather than as an error.
///
/// `pub(crate)` (Task 11): `cancel`'s escalation path settles the run through
/// this exact function rather than re-implementing the same atomic settle.
///
/// Whole-branch review Important 1: `task_status` is now a `TaskStatusWrite`,
/// so a caller can require the task to still be in a given set of statuses.
/// A conditional write that does not apply is **not** an error and does not
/// abort the settle: the run still settles, whatever the run already wrote
/// elsewhere (a `reviews` row, a `verdict` event) still stands, `tasks.status`
/// is left alone, and the fact is recorded twice - as
/// `task_status_not_applied` on this run's own `status_change` event, and as
/// a line in the run's `runner-error.log`.
///
/// The task-status write moved **above** the `status_change` event for that
/// reason: the event has to be able to say whether it applied, and both are
/// in the same transaction, so a reader sees them together either way.
pub(crate) fn settle_terminal(
    conn: &Connection,
    run: &state::Run,
    finish: &RunFinish,
    task_status: Option<TaskStatusWrite>,
    interrupted_detail: Option<serde_json::Value>,
    mut status_change_detail: serde_json::Value,
) -> Result<bool> {
    let tx = conn.unchecked_transaction()?;
    if !state::finish_run(&tx, &run.run_id, finish)? {
        // Rolls back on drop; nothing was written by this call.
        return Ok(false);
    }
    // `None` = settle the run, leave the task where it is. The run's own
    // `status_change` event below still records what happened, so an outcome is
    // never silently swallowed.
    let mut not_applied: Option<(TaskStatus, String)> = None;
    if let Some(write) = task_status {
        match write.only_from {
            None => state::set_task_status(&tx, &run.task_id, write.to, &finish.ended_at)?,
            Some(allowed) => {
                if !state::transition_task_status(
                    &tx,
                    &run.task_id,
                    allowed,
                    write.to,
                    &finish.ended_at,
                )? {
                    let found = state::get_task(&tx, &run.task_id)?
                        .map(|t| t.status)
                        .unwrap_or_else(|| "missing".to_string());
                    not_applied = Some((write.to, found));
                }
            }
        }
    }
    if let Some((to, found)) = &not_applied {
        if let serde_json::Value::Object(map) = &mut status_change_detail {
            // `task_status_written` keeps its meaning - the status this settle
            // actually wrote - so it goes back to `null`, and the companion key
            // says what was intended and what was in the way.
            map.insert("task_status_written".to_string(), serde_json::Value::Null);
            map.insert(
                "task_status_not_applied".to_string(),
                serde_json::json!({ "intended": to.as_str(), "found": found }),
            );
        }
    }
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::STATUS_CHANGE.to_string(),
            detail: status_change_detail,
        },
    )?;
    if let Some(detail) = interrupted_detail {
        state::append_event(
            &tx,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::RUN_INTERRUPTED.to_string(),
                detail,
            },
        )?;
    }
    tx.commit()?;
    if let Some((to, found)) = not_applied {
        log_runner_error(
            Path::new(&run.run_dir),
            &format!(
                "run {}: task {} was NOT moved to '{}' - it is '{}' now, not a status this \
                 settle was allowed to write over. The run's own outcome, and anything it \
                 already recorded (a reviews row, a verdict event), still stand.",
                run.run_id,
                run.task_id,
                to.as_str(),
                found
            ),
        );
    }
    Ok(true)
}

/// I2/I3 (fix round 1): last-resort settle for an error that escaped
/// `run_worker_inner` - a spawn failure, a file-open failure, an unexpected DB
/// error mid-loop, or anything else propagated with `?`. Without this the row
/// would stay stuck at `queued`/`running` forever. Never panics or propagates
/// further; this is the end of the line for error handling.
fn best_effort_settle_on_error(conn: &Connection, run: &state::Run, err: &anyhow::Error) {
    let run_dir = PathBuf::from(&run.run_dir);
    let msg = format!("{err:#}");
    log_runner_error(&run_dir, &format!("run_worker failed: {msg}"));

    let current_status = match state::get_run(conn, &run.run_id) {
        Ok(Some(r)) => r.status,
        _ => run.status.clone(),
    };
    if is_run_status_terminal(&current_status) {
        return;
    }

    let ended_at = ids::now_rfc3339();
    let finish = RunFinish {
        status: RunStatus::Failed,
        interrupted_reason: None,
        exit_code: None,
        candidate_commit: None,
        error_text: Some(msg.clone()),
        ended_at: ended_at.clone(),
    };
    let detail = serde_json::json!({
        "entity": "run",
        "id": run.run_id,
        "to": "failed",
        "note": msg,
    });
    // A reviewer run that died before it could report leaves the task
    // `in_review`, so `thoth review start` can be retried; only an
    // implementer run's failure is the task's failure. A spec reviewer never
    // writes the task's status (SR-2).
    let task_status = if run.role == crate::model::role::REVIEWER
        || run.role == crate::model::role::SPEC_REVIEWER
    {
        None
    } else {
        Some(TaskStatusWrite::unconditional(TaskStatus::Failed))
    };
    match settle_terminal(conn, run, &finish, task_status, None, detail) {
        Ok(true) => {}
        Ok(false) => log_runner_error(
            &run_dir,
            &format!(
                "run {} was already settled by someone else before this error-path settle could apply",
                run.run_id
            ),
        ),
        Err(e) => log_runner_error(&run_dir, &format!("settle_terminal failed: {e:#}")),
    }
}

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
struct InboxWatch<'a> {
    paths: &'a Paths,
    repo_id: &'a str,
    inbox_dir: PathBuf,
    run_dir: PathBuf,
    worktree: PathBuf,
    tailer: evidence::InboxTailer,
    pairer: evidence::Pairer,
    bad_lines: u64,
    recorded: u64,
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
    fn drain(&mut self, conn: &Connection, run: &state::Run) {
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
fn retry_waits() -> [u64; 2] {
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

/// What every launch of one run shares (FC-2): only the prompt and the
/// session differ between the first launch and a retry.
struct LaunchBase<'a> {
    model: &'a str,
    allowed_tools: &'a str,
    config_dir: &'a str,
    cwd: PathBuf,
    settings_path: Option<PathBuf>,
    extra_env: Vec<(String, String)>,
    review_output: Option<crate::adapter::ReviewOutput>,
    timeout_seconds: i64,
    effort: Option<String>,
    research: bool,
}

impl LaunchBase<'_> {
    fn request<'b>(
        &'b self,
        prompt: String,
        session_id: &'b str,
        resume_session_id: Option<&'b str>,
    ) -> LaunchRequest<'b> {
        LaunchRequest {
            prompt,
            model: self.model,
            session_id,
            resume_session_id,
            allowed_tools: self.allowed_tools,
            config_dir: self.config_dir,
            cwd: self.cwd.clone(),
            settings_path: self.settings_path.clone(),
            extra_env: self.extra_env.clone(),
            review_output: self.review_output.clone(),
            timeout_seconds: self.timeout_seconds,
            effort: self.effort.clone(),
            research: self.research,
        }
    }
}

/// One launch of the harness, spawned and not yet watched.
struct Spawned {
    child: std::process::Child,
    worker_pid: u32,
    worker_image: Option<String>,
    stdout: std::process::ChildStdout,
}

/// Builds one launch's command, then resolves, checks and spawns it, feeds
/// its stdin and reads the image its pid runs.
fn spawn_attempt(
    ctrl: &dyn process::ProcessController,
    adapter: &dyn HarnessAdapter,
    req: &LaunchRequest<'_>,
    stderr_path: &Path,
    run_dir: &Path,
    is_codex: bool,
    masker: &crate::redact::Masker,
) -> Result<Spawned> {
    let plan = adapter.build_command(req);
    // Windows: an npm `.cmd` shim becomes `node.exe <entry.js> ...`; never
    // run through cmd.exe. A failure here settles the run before any spawn.
    let (program, launch_args) = process::resolve_harness_command(&plan.program, &plan.args)?;
    // W3: refuse a command line Windows would reject (os error 206) with a
    // message that says why, before anything is spawned.
    process::check_command_line(&program, &launch_args)?;
    // RD-2 (H15): the worker never gets Thoth's own keys, nor any variable the
    // masker takes for a secret; the runner keeps them to mask with. RD-4:
    // nor a copy of a known value under another name, secrets.json's
    // included, which is why this is the run's masker.
    let mut env_remove: Vec<String> =
        crate::commands::settings::secret_env_vars().into_iter().map(str::to_string).collect();
    env_remove.extend(masker.secret_env_names());
    env_remove.sort();
    env_remove.dedup();
    let mut child = ctrl.spawn_child(ChildSpec {
        program: &program,
        args: &launch_args,
        cwd: &plan.cwd,
        env: &plan.env,
        env_remove: &env_remove,
        stderr_path: Some(stderr_path),
        stdin: plan.stdin.as_deref(),
    })?;
    let worker_pid = child.id();
    // W3: claude and codex read the prompt from stdin; a separate thread
    // writes it and closes the pipe (see `feed_stdin` on why this never
    // deadlocks).
    if let Some(text) = plan.stdin.as_ref() {
        if let Some(si) = child.stdin.take() {
            if let Err(e) = process::feed_stdin(si, text.clone().into_bytes(), run_dir.to_path_buf()) {
                log_runner_error(run_dir, &format!("could not start the stdin writer: {e}"));
            }
        }
    }
    // The image the worker pid actually runs, for `cancel`'s pid check. Not
    // always the harness binary's stem: the npm `codex` is a Node launcher
    // whose pid reads `codex` now and `node` about 0.5 s later, so a codex
    // run records it on `harness_init` when `thread.started` arrives instead.
    let worker_image = if is_codex {
        None
    } else {
        process::image_name(worker_pid).ok().flatten()
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = ctrl.kill_tree(worker_pid);
        process::release_job(worker_pid);
        bail!("child stdout was not piped");
    };
    Ok(Spawned {
        child,
        worker_pid,
        worker_image,
        stdout,
    })
}

/// What `watch_attempt` needs that is the same for every launch of a run.
struct AttemptCtx<'a> {
    paths: &'a Paths,
    conn: &'a Connection,
    gconn: &'a Connection,
    repo_id: &'a str,
    run: &'a state::Run,
    harness: &'a str,
    run_dir: &'a Path,
    stream_path: &'a Path,
    is_codex: bool,
    is_review: bool,
    /// A worker or source-check reviewer of a research task (`LaunchRequest::research`).
    research: bool,
    /// Taken once, after the first spawn: every launch's wall-clock check,
    /// and every retry wait, counts against the run's own `--timeout`.
    started: Instant,
    ctrl: &'a dyn process::ProcessController,
}

/// How one launch ended.
// One per launch, at most three per run: the size of `Exited` does not matter.
#[allow(clippy::large_enum_variant)]
enum AttemptEnd<'a> {
    /// The DB poll give-up or the kill-wait deadline: the run is already
    /// settled `failed`.
    SettledEarly,
    Exited {
        /// This launch's stream state, for what the settle tail still reads
        /// (`place.worktree`, `adapter.write_review_output`).
        stream: StreamState<'a>,
        tally: StreamTally,
        exit_code: Option<i32>,
        kill: Option<KillReason>,
        kill_error: Option<String>,
        drain_timed_out: bool,
        worker_pid: u32,
        worker_image: Option<String>,
        evidence_recorded: u64,
    },
}

/// Reads one launch's stream until the child has exited and the stream is
/// drained: the 1-second loop (cancel, timeout, heartbeat, the inbox), the
/// two early settles, the job release, the bounded drain, and the launch
/// hooks' last settle.
fn watch_attempt<'a>(
    cx: &AttemptCtx<'a>,
    spawned: Spawned,
    resume_session: Option<&str>,
    watch: &mut Option<InboxWatch<'_>>,
) -> Result<AttemptEnd<'a>> {
    let (conn, run, run_dir, ctrl) = (cx.conn, cx.run, cx.run_dir, cx.ctrl);
    let is_review = cx.is_review;
    let Spawned {
        mut child,
        worker_pid,
        worker_image,
        stdout,
    } = spawned;

    // One reader thread tees stdout to stream.jsonl and forwards each line.
    let (tx, rx) = mpsc::channel::<String>();
    // Appended: a retry's lines follow the failed launch's.
    let mut stream_file = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cx.stream_path)
    {
        Ok(f) => f,
        Err(e) => {
            let _ = ctrl.kill_tree(worker_pid);
            process::release_job(worker_pid);
            return Err(anyhow::Error::from(e));
        }
    };
    // M2: read raw bytes and lossily decode, rather than `BufRead::lines()`
    // (which requires valid UTF-8 per line and silently ends the whole
    // iterator - and with it this thread - on the first bad byte anywhere in
    // the stream). One mangled byte in a single line must not be able to
    // truncate the rest of the stream and, with it, the `result` line that
    // `classify_outcome` needs to avoid classifying a clean exit as a crash.
    // Deliberately never joined; see STREAM_DRAIN_GRACE and the drain loop below.
    let _reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        let mut buf: Vec<u8> = Vec::new();
        loop {
            buf.clear();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) => break, // EOF
                Ok(_) => {
                    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                        buf.pop();
                    }
                    let line = String::from_utf8_lossy(&buf).into_owned();
                    let _ = writeln!(stream_file, "{line}");
                    let _ = stream_file.flush();
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    // A new adapter per launch: its launch hooks start empty.
    let adapter = crate::adapter::for_harness(cx.harness);
    let launch = adapter.launch_hooks(run, resume_session);
    let mut stream = StreamState {
        adapter,
        launch,
        codex: cx.is_codex,
        reviewer: is_review,
        research: cx.research,
        worker_pid,
        place: RunPlace {
            paths: cx.paths,
            repo_id: cx.repo_id,
            run_dir: run_dir.to_path_buf(),
            worktree: PathBuf::from(&run.worktree),
        },
        evidence_recorded: 0,
        unexpected_items: BTreeMap::new(),
    };

    let mut tally = StreamTally::default();
    let started = cx.started;
    let mut last_heartbeat = Instant::now();
    let mut kill: Option<KillReason> = None;
    let mut kill_error: Option<String> = None;
    // I1: only attempt `kill_tree` once per launch; once it has failed, bound
    // how long we wait for the child instead of blocking on `child.wait()`.
    let mut kill_attempted = false;
    let mut kill_wait_deadline: Option<Instant> = None;
    // M6: consecutive touch_heartbeat/read_cancel_requested failures.
    let mut db_poll_failures: u32 = 0;
    let exit_code: Option<i32>;

    loop {
        while let Ok(line) = rx.try_recv() {
            handle_line(conn, run, &mut stream, &line, &mut tally);
        }
        stream.settle_commands(conn, run, tally.session_id.as_deref(), false);
        if let Some(w) = watch.as_mut() {
            w.drain(conn, run);
        }
        if last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
            match state::touch_heartbeat(conn, &run.run_id, &ids::now_rfc3339()) {
                Ok(()) => db_poll_failures = 0,
                Err(e) => {
                    db_poll_failures += 1;
                    log_runner_error(
                        run_dir,
                        &format!("touch_heartbeat failed ({db_poll_failures}/{DB_POLL_FAILURE_LIMIT}): {e}"),
                    );
                }
            }
            last_heartbeat = Instant::now();
        }
        if kill.is_none() {
            match state::read_cancel_requested(conn, &run.run_id) {
                Ok(true) => {
                    kill = Some(KillReason::Cancelled);
                    db_poll_failures = 0;
                }
                Ok(false) => db_poll_failures = 0,
                Err(e) => {
                    db_poll_failures += 1;
                    log_runner_error(
                        run_dir,
                        &format!(
                            "read_cancel_requested failed ({db_poll_failures}/{DB_POLL_FAILURE_LIMIT}): {e}"
                        ),
                    );
                }
            }
            if kill.is_none() && started.elapsed() >= Duration::from_secs(run.timeout_seconds.max(1) as u64)
            {
                kill = Some(KillReason::Timeout);
            }
        }

        // M6: the DB itself is unreachable/erroring repeatedly - stop trying
        // to observe cancel/heartbeat state and settle rather than spin
        // forever unable to see a cancel request or advance a heartbeat.
        if db_poll_failures >= DB_POLL_FAILURE_LIMIT {
            let _ = ctrl.kill_tree(worker_pid);
            process::release_job(worker_pid);
            stream.settle_commands(conn, run, tally.session_id.as_deref(), true);
            stream.after_stream(conn, run);
            let ended_at = ids::now_rfc3339();
            let note = format!(
                "runner gave up after {db_poll_failures} consecutive heartbeat/cancel-check DB failures"
            );
            log_runner_error(run_dir, &note);
            crate::run_quota::record(
                conn,
                cx.gconn,
                cx.paths,
                cx.repo_id,
                run,
                cx.harness,
                tally.session_id.as_deref(),
                &ended_at,
            );
            if !settle_terminal(
                conn,
                run,
                &RunFinish {
                    status: RunStatus::Failed,
                    interrupted_reason: None,
                    exit_code: None,
                    candidate_commit: None,
                    error_text: Some(note.clone()),
                    ended_at: ended_at.clone(),
                },
                if is_review {
                    None
                } else {
                    Some(TaskStatusWrite::unconditional(TaskStatus::Failed))
                },
                None,
                serde_json::json!({
                    "entity": "run",
                    "id": run.run_id,
                    "to": "failed",
                    "note": note,
                }),
            )? {
                log_runner_error(
                    run_dir,
                    &format!("run {} was already settled by someone else", run.run_id),
                );
            }
            return Ok(AttemptEnd::SettledEarly);
        }

        if kill.is_some() && !kill_attempted {
            kill_attempted = true;
            // Do not `?` this: `kill_tree` confirms the kill with `tasklist`
            // and reports failure, and a failed kill must not abandon the run
            // half-written. Record it and finish the classification below -
            // and, per I1, do not fall through to a blocking `child.wait()`
            // on this failure: bound the wait instead (see below).
            if let Err(e) = ctrl.kill_tree(worker_pid) {
                kill_error = Some(e.to_string());
                kill_wait_deadline = Some(Instant::now() + KILL_WAIT_GRACE);
            }
        }

        let waited = child.try_wait();
        if waited.is_err() {
            stream.after_stream(conn, run);
        }
        match waited? {
            Some(status) => {
                exit_code = status.code();
                break;
            }
            None => {
                if let Some(deadline) = kill_wait_deadline {
                    // I1: `kill_tree` itself reported the pid still alive
                    // after its own ~2 s confirmation loop. A blind
                    // `child.wait()` here could block forever on a
                    // genuinely stuck process; poll non-blockingly instead,
                    // bounded by KILL_WAIT_GRACE.
                    if Instant::now() >= deadline {
                        process::release_job(worker_pid);
                        stream.settle_commands(conn, run, tally.session_id.as_deref(), true);
                        stream.after_stream(conn, run);
                        let ended_at = ids::now_rfc3339();
                        let note = format!(
                            "{}; child did not exit after kill",
                            kill_error.clone().unwrap_or_default()
                        );
                        log_runner_error(run_dir, &note);
                        crate::run_quota::record(
                            conn,
                            cx.gconn,
                            cx.paths,
                            cx.repo_id,
                            run,
                            cx.harness,
                            tally.session_id.as_deref(),
                            &ended_at,
                        );
                        if !settle_terminal(
                            conn,
                            run,
                            &RunFinish {
                                status: RunStatus::Failed,
                                interrupted_reason: None,
                                exit_code: None,
                                candidate_commit: None,
                                error_text: Some(note.clone()),
                                ended_at: ended_at.clone(),
                            },
                            if is_review {
                                None
                            } else {
                                Some(TaskStatusWrite::unconditional(TaskStatus::Failed))
                            },
                            None,
                            serde_json::json!({
                                "entity": "run",
                                "id": run.run_id,
                                "to": "failed",
                                "note": note,
                            }),
                        )? {
                            log_runner_error(
                                run_dir,
                                &format!("run {} was already settled by someone else", run.run_id),
                            );
                        }
                        return Ok(AttemptEnd::SettledEarly);
                    }
                } else if kill.is_some() {
                    // `kill_tree` confirmed the pid is gone, so this returns
                    // promptly - it is not the same blocking risk I1 guards
                    // against above.
                    let waited = child.wait();
                    if waited.is_err() {
                        stream.after_stream(conn, run);
                    }
                    let status = waited?;
                    exit_code = status.code();
                    break;
                }
            }
        }
        std::thread::sleep(POLL_EVERY);
    }

    // Controller pointer 1 (Task 6 review, binding): once `child.wait()` has
    // returned above - whether via a clean exit, a cancel-triggered kill, or a
    // timeout-triggered kill, all three break out of the loop through this one
    // point - drop this run's Job Object handle. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
    // means this silently kills any descendant the harness left running, which is
    // exactly what a finished run should leave behind: nothing.
    process::release_job(worker_pid);

    // Bounded drain instead of `reader.join()`: take whatever the reader still
    // has, stop when the channel closes (reader finished) or when the grace
    // period expires (something is still holding the pipe open), then let the
    // thread go. Dropping `rx` makes the reader's next `send` fail so it unwinds
    // on its own if the pipe ever does reach EOF.
    //
    // M3: the deadline is also checked in the `Ok(line)` arm, not just on a
    // 200ms recv timeout - otherwise a descendant that keeps writing lines
    // faster than every 200ms could hold this drain open indefinitely.
    //
    // Fix round 2 minor (staleness margin): `STALE_AFTER_SECONDS` (15s) is
    // only `HEARTBEAT_EVERY` (5s) plus `STREAM_DRAIN_GRACE` (10s) apart, so a
    // healthy runner that stopped touching the heartbeat the moment the child
    // exited (as this loop used to) could look stale to `cancel` before it
    // ever reaches `finish_and_settle` below - and get killed mid-tail even
    // though nothing is actually wrong. Keep touching the heartbeat on the
    // same cadence during the drain, exactly as the main loop above does.
    let drain_deadline = Instant::now() + STREAM_DRAIN_GRACE;
    let mut drain_timed_out = false;
    let mut last_heartbeat = Instant::now();
    loop {
        if last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
            let _ = state::touch_heartbeat(conn, &run.run_id, &ids::now_rfc3339());
            last_heartbeat = Instant::now();
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                handle_line(conn, run, &mut stream, &line, &mut tally);
                if Instant::now() >= drain_deadline {
                    drain_timed_out = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= drain_deadline {
                    drain_timed_out = true;
                    break;
                }
            }
        }
    }
    drop(rx);

    // Every command the launch hooks still hold is settled now - before the
    // diff and commit evidence and before ingest.
    stream.settle_commands(conn, run, tally.session_id.as_deref(), true);
    stream.after_stream(conn, run);

    // One last pass: the hook may have appended between the child's exit and
    // here, and a worker's final commands are exactly the ones a reviewer wants.
    if let Some(w) = watch.as_mut() {
        w.drain(conn, run);
    }

    // One more heartbeat right before the settle tail (or a retry wait):
    // ingesting a large result.json, classification, and
    // `finish_and_settle`'s transaction can themselves take nontrivial time
    // (and, per the M6/I1 paths above, DB contention is exactly the kind of
    // thing this runner has to tolerate), so `cancel`'s staleness check must
    // not see a heartbeat that is already stale from the *drain*, before the
    // tail below even starts.
    let _ = state::touch_heartbeat(conn, &run.run_id, &ids::now_rfc3339());

    let evidence_recorded = stream.evidence_recorded;
    Ok(AttemptEnd::Exited {
        stream,
        tally,
        exit_code,
        kill,
        kill_error,
        drain_timed_out,
        worker_pid,
        worker_image,
        evidence_recorded,
    })
}

fn run_worker_inner(
    paths: &Paths,
    conn: &Connection,
    repo_id: &str,
    repo_root: &Path,
    run: &state::Run,
) -> Result<()> {
    let gconn = db::open_global(paths)?;
    // Existence check only: `run.config_dir` (not `profile.config_dir`) is what
    // actually gets used below, precisely so the runner does not have to trust
    // that the profile row still points at this run's session directory.
    //
    // Its harness, though, chooses the adapter.
    let profile = global::get_profile(&gconn, &run.account)?.ok_or_else(|| {
        anyhow!("profile {} is gone; cannot launch run {}", run.account, run.run_id)
    })?;
    let is_codex = profile.harness == crate::adapter::HARNESS_CODEX;
    let adapter = crate::adapter::for_harness(&profile.harness);
    let spec: TaskSpec = crate::spec::parse_spec(&run.spec)?;
    // Task 8: a rework dispatch stored the reviewer's findings alongside the
    // spec in this run's own copy. `None` for every other run.
    let rework = crate::spec::parse_rework(&run.spec);
    let reroute = crate::spec::parse_reroute(&run.spec);
    // S3-9: a dispatch that continued a stopped run names the handoff file.
    let handoff = crate::handoff::parse_handoff(&run.spec);
    // RD-1: everything this run hands its worker or reviewer is masked with
    // the environment this runner was started with, which is the
    // commander's at dispatch time.
    let masker = crate::redact::Masker::load(Some(&paths.home));

    let run_dir = PathBuf::from(&run.run_dir);
    std::fs::create_dir_all(&run_dir)?;
    let stream_path = run_dir.join("stream.jsonl");
    let stderr_path = run_dir.join("stderr.log");
    let prompt_path = run_dir.join("prompt.txt");

    // brief §1 and Open question 4: only a worker run is hooked. Hooking the
    // reviewer would mint new evidence codes for the very task it is citing,
    // and a reviewer's `cat` is not evidence about the implementation.
    // SR-2: a spec reviewer is a reviewer in every branch below, except for
    // its input, its report and the task status it never writes.
    let is_spec_review = run.role == crate::model::role::SPEC_REVIEWER;
    let is_review = run.role == crate::model::role::REVIEWER || is_spec_review;
    // Spec reviewers never need the web, including research specifications.
    let research = !is_spec_review && crate::spec::is_research_task(&run.spec);
    let inbox_dir = run_dir.join("inbox");
    let thoth_exe = std::env::current_exe()?;
    let mut extra_env: Vec<(String, String)> = vec![
        // `evidence show` reads this to attribute an `evidence_opened` event,
        // so every run gets it, reviewer included.
        ("THOTH_RUN_ID".to_string(), run.run_id.clone()),
        // So a worker or reviewer can invoke thoth without a PATH assumption.
        ("THOTH_EXE".to_string(), thoth_exe.display().to_string()),
    ];
    // Only a harness whose commands come through `thoth hook` is hooked:
    // codex's commands, with their real exit codes, arrive on the stream
    // (scope §2.3), so it gets no settings and no inbox.
    let hooked = !is_review && adapter.hooks_through_inbox();
    if !is_review {
        // M8 (fix round 1): the worker prompt's Evidence section names this
        // file unconditionally ("The file .agent/evidence-index.json ...
        // lists the captured evidence"), so it must exist before the child
        // ever reads it - not just after the first row lands. `refresh_index`
        // (from the DB) rather than a hardcoded `&[]`: a *resumed* run's
        // inbox tailer already sits past the offset that produced its
        // earlier rows, so nothing will call `refresh_index` again until (if
        // ever) a genuinely new command is captured - writing an empty array
        // here would silently blank out an already-populated index for the
        // rest of the run.
        evidence::refresh_index(conn, &run.task_id, &PathBuf::from(&run.worktree))?;
    }
    let settings_path = if !hooked {
        None
    } else {
        std::fs::create_dir_all(inbox_dir.join(crate::commands::hook::BLOBS_DIR))?;
        extra_env.push((
            "THOTH_INBOX_DIR".to_string(),
            inbox_dir.display().to_string(),
        ));
        Some(claude_code::write_hooks_settings(&run_dir, &thoth_exe)?)
    };
    // Whatever the harness itself needs in the worktree, written fresh on
    // dispatch and resume, reviewer included.
    adapter.prepare_launch(run, &run_dir, &thoth_exe)?;
    if is_review {
        // The reviewer has no `Write` tool and creates `.agent/review.json`
        // with a shell redirect, which cannot create a missing directory.
        std::fs::create_dir_all(Path::new(&run.worktree).join(".agent"))?;
    }
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
        let redactions = crate::commands::review_packet::write_review_input(
            conn,
            &run.task_id,
            Path::new(&run.worktree),
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

    // A run that already has a started_at has had a session, so this is a resume.
    let is_resume = run.started_at.is_some() && run.harness_session_id.is_some();
    let session_id = run
        .harness_session_id
        .clone()
        .unwrap_or_else(ids::new_session_uuid);
    // RD-1: each value the prompt embeds is masked on its own first, so an
    // unterminated key block eats at most the rest of its own value and the
    // prompt's later sections stay. Criterion ids, the checks and the
    // research decision still come from the spec as stored.
    let mut prompt_redactions = crate::redact::Counts::default();
    let mask_into = |counts: &mut crate::redact::Counts, text: &str| -> String {
        let (masked, c) = masker.mask_text(text);
        counts.merge(&c);
        masked
    };
    let masked_spec = |counts: &mut crate::redact::Counts| -> TaskSpec {
        let mut list = |items: &[String]| -> Vec<String> {
            items.iter().map(|s| mask_into(counts, s)).collect()
        };
        let acceptance_criteria = list(&spec.acceptance_criteria);
        let constraints = list(&spec.constraints);
        let non_goals = list(&spec.non_goals);
        let excluded_alternatives = list(&spec.excluded_alternatives);
        TaskSpec {
            goal: mask_into(counts, &spec.goal),
            acceptance_criteria,
            constraints,
            non_goals,
            excluded_alternatives,
            ..spec.clone()
        }
    };
    let resume_prompt = !is_review && is_resume;
    let prompt = if is_spec_review {
        adapter.build_spec_review_prompt(
            &run.task_id,
            &masked_spec(&mut prompt_redactions),
            &crate::review_json::ac_ids(&spec),
            spec_review_mode,
        )
    } else if let Some(fix) = &confirm_fix_commit {
        // RFIX-1: the confirmer's prompt is the same for every harness but
        // the line saying how the answer comes back.
        let how = if is_codex {
            crate::review_fix_edits::How::FinalMessage
        } else if profile.harness == crate::adapter::antigravity::HARNESS_NAME {
            crate::review_fix_edits::How::StructuredOutput
        } else {
            crate::review_fix_edits::How::ShellRedirect
        };
        crate::review_fix_edits::confirm_prompt(&run.task_id, fix, how)
    } else if is_review {
        // Fix round 1 (R1): the empty fallback tells the reviewer "the control
        // plane captured no evidence for this task", which is a sentence it may
        // reasonably PASS or UNVERIFY on - so a read failure here must not be
        // indistinguishable from a task that genuinely has no evidence. The
        // fallback stays (a reviewer with a thin prompt still beats a runner
        // that refuses to start); the silence does not.
        let rows = state::list_evidence_for_task(conn, &run.task_id).unwrap_or_else(|e| {
            log_runner_error(
                &run_dir,
                &format!(
                    "reading task {}'s evidence for the review prompt failed: {e:#}; the \
                     reviewer is being told no evidence was captured",
                    run.task_id
                ),
            );
            Vec::new()
        });
        // P7-2: every diff evidence's protected changes, one line each, as
        // the review packet lists them. LESS-5: a line whose (run, path,
        // status) a `protected_change_approved` event of that run covers says
        // who approved it and why.
        let approvals: Vec<(String, String, String, String)> =
            state::events_since(conn, 0, Some(&run.task_id))
                .unwrap_or_default()
                .into_iter()
                .filter(|ev| ev.event_type == event_type::PROTECTED_CHANGE_APPROVED)
                .flat_map(|ev| {
                    let d = serde_json::from_str::<serde_json::Value>(&ev.detail)
                        .unwrap_or(serde_json::Value::Null);
                    let note = format!(
                        "{}: {}",
                        d["approved_by"].as_str().unwrap_or("unknown"),
                        d["reason"].as_str().unwrap_or("-").replace(['\r', '\n'], " ")
                    );
                    let run_id = ev.run_id.unwrap_or_default();
                    d["changes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|c| {
                            (
                                run_id.clone(),
                                c["path"].as_str().unwrap_or("").to_string(),
                                c["status"].as_str().unwrap_or("").to_string(),
                                note.clone(),
                            )
                        })
                        .collect::<Vec<_>>()
                })
                .collect();
        let protected_changes: Vec<String> = rows
            .iter()
            .filter(|e| e.kind == "diff")
            .flat_map(|e| {
                let detail = serde_json::from_str::<serde_json::Value>(&e.detail)
                    .unwrap_or(serde_json::Value::Null);
                let diff_run = e.run_id.clone().unwrap_or_default();
                crate::protected::changes_in_detail(&detail)
                    .into_iter()
                    .map(|c| {
                        let approved = approvals
                            .iter()
                            .find(|(r, p, s, _)| *r == diff_run && *p == c.path && *s == c.status)
                            .map(|(_, _, _, note)| format!("; approved by {note}"))
                            .unwrap_or_default();
                        format!("{} ({}, {}) [{}]{approved}", c.path, c.status, c.kind, e.code)
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        let index_rows: Vec<evidence::IndexRow> = rows
            .into_iter()
            .map(|e| {
                let exit_code = serde_json::from_str::<serde_json::Value>(&e.detail)
                    .ok()
                    .and_then(|d| d["exit_code"].as_i64());
                evidence::IndexRow {
                    code: e.code,
                    kind: e.kind,
                    label: e.label,
                    exit_code,
                }
            })
            .collect();
        let diff_code = index_rows
            .iter()
            .find(|r| r.kind == "diff")
            .map(|r| r.code.clone());
        let criterion_ids = crate::review_json::ac_ids(&spec);
        let sources = if research {
            let target = crate::commands::review::target_run_id_for(conn, &run.run_id)?
                .ok_or_else(|| anyhow!("source check has no target run"))?;
            Some(crate::review_json::sources_to_check(
                &crate::commands::review::target_result(conn, &target)?,
            ))
        } else { None };
        let counts = &mut prompt_redactions;
        let index_rows: Vec<evidence::IndexRow> = index_rows
            .into_iter()
            .map(|r| evidence::IndexRow { label: mask_into(counts, &r.label), ..r })
            .collect();
        let protected_changes: Vec<String> =
            protected_changes.iter().map(|line| mask_into(counts, line)).collect();
        let sources = sources.map(|list| {
            list.into_iter()
                .map(|s| crate::review_json::SourceToCheck {
                    claim_text: mask_into(counts, &s.claim_text),
                    url: mask_into(counts, &s.url),
                    quote: mask_into(counts, &s.quote),
                    ..s
                })
                .collect::<Vec<_>>()
        });
        let mut prompt = adapter.build_review_prompt(
            &run.task_id,
            &masked_spec(counts),
            &criterion_ids,
            &thoth_exe.display().to_string(),
            diff_code.as_deref(),
            &index_rows,
            &protected_changes,
            sources.as_deref(),
        );
        // RFIX-1: an edit-mode reviewer is also told how to attach a fix.
        if code_review_mode == crate::spec_edits::ReviewMode::Edit {
            prompt.push_str(&crate::review_fix_edits::edit_prompt_section(
                profile.harness == crate::adapter::HARNESS_CLAUDE_CODE,
            ));
        }
        prompt
    } else if resume_prompt {
        claude_code::RESUME_PROMPT.to_string()
    } else {
        let counts = &mut prompt_redactions;
        let masked = masked_spec(counts);
        let rework = rework.map(|r| crate::spec::ReworkAddendum {
            unmet: r.unmet.iter().map(|u| mask_into(counts, u)).collect(),
            extra_findings: r.extra_findings.iter().map(|e| mask_into(counts, e)).collect(),
            ..r
        });
        let reroute = reroute.map(|r| crate::spec::RerouteAddendum { reason: mask_into(counts, &r.reason), ..r });
        let handoff = handoff.map(|h| crate::handoff::HandoffRef { reason: mask_into(counts, &h.reason), ..h });
        let checks = crate::checks::applicable_approved(conn, &spec.checks)?;
        let protected = crate::protected::Catalogue::load(conn)?;
        if is_codex {
            // scope §2.6: codex's sandbox cannot write a linked worktree's own
            // gitdir, so unlike Claude Code its worker must not try to commit;
            // thoth commits on its behalf below, once the run exits cleanly.
            claude_code::build_codex_prompt(
                &run.task_id,
                &masked,
                rework.as_ref(),
                reroute.as_ref(),
                handoff.as_ref(),
                &checks,
                &protected,
                research,
            )
        } else {
            claude_code::build_prompt(
                &run.task_id,
                &masked,
                rework.as_ref(),
                reroute.as_ref(),
                handoff.as_ref(),
                &checks,
                &protected,
                research,
            )
        }
    };
    // The whole prompt once more, for what was not masked value by value
    // (the checks' commands, the protected files); markers are not counted
    // again. `RESUME_PROMPT` is fixed text and is sent as it is.
    let prompt = if resume_prompt {
        prompt
    } else {
        let (masked, c) = masker.mask_text(&prompt);
        prompt_redactions.merge(&c);
        masked
    };
    std::fs::write(&prompt_path, &prompt)?;
    if let Err(e) = crate::redact::record(conn, &run.task_id, Some(&run.run_id), "prompt", &prompt_redactions, &[]) {
        log_runner_error(&run_dir, &format!("could not record secret_redacted: {e:#}"));
    }

    // S3-7: the launch settings dispatch / review start stored in `runs.spec`;
    // a resume re-reads them here, so it keeps the original run's effort.
    let launch_settings = crate::adapter::effort::parse_launch(&run.spec);
    let base = LaunchBase {
        model: &run.model,
        allowed_tools: &run.allowed_tools,
        // `run.config_dir`, not `profile.config_dir`: the column exists precisely
        // so the runner does not have to trust that the profile row still points
        // at the directory this run's session belongs to (Open question 3).
        // The profile lookup above stays as an existence check.
        config_dir: &run.config_dir,
        cwd: PathBuf::from(&run.worktree),
        settings_path,
        extra_env,
        review_output,
        timeout_seconds: run.timeout_seconds,
        effort: launch_settings.as_ref().and_then(|l| l.effort.clone()),
        research,
    };
    let mut session_id = session_id;
    // The session a `thoth resume`d run continues; FC-2's retry falls back
    // on it when a late-naming harness failed before naming one.
    let known_session: Option<String> = if is_resume { Some(session_id.clone()) } else { None };

    let ctrl = process::controller();
    let spawned = spawn_attempt(
        ctrl.as_ref(),
        adapter.as_ref(),
        &base.request(prompt.clone(), &session_id, known_session.as_deref()),
        &stderr_path,
        &run_dir,
        is_codex,
        &masker,
    )?;
    let worker_pid = spawned.worker_pid;
    let worker_image = spawned.worker_image.clone();

    // I2/I3: if any of the setup below fails after the child is already
    // running, kill it and release its Job Object before propagating the
    // error - otherwise the outer catch would settle the row `failed` while
    // leaving the harness process itself orphaned and unsupervised.
    let kill_and_bail = |e: anyhow::Error| -> anyhow::Error {
        let _ = ctrl.kill_tree(worker_pid);
        process::release_job(worker_pid);
        e
    };

    let now = ids::now_rfc3339();
    // M8 (closing the remaining check-then-act gate): the child is already
    // spawned by this point, so `mark_run_started` is where this runner
    // actually claims the row. `false` means it lost the race - another
    // runner's own `mark_run_started` landed first (a hand double-launch of
    // `run-worker`, most plausibly) - and this runner must not leave its
    // own, now-unclaimed child running unsupervised: kill it and its job the
    // same way `kill_and_bail` would, then bail before any further DB write
    // that would otherwise make it look like this runner is the one of
    // record for the run.
    if !state::mark_run_started(conn, &run.run_id, worker_pid as i64, &now).map_err(kill_and_bail)? {
        return Err(kill_and_bail(anyhow!(
            "run {} was claimed by another runner before this one could start (row is no \
             longer 'queued'); this runner's own worker has been killed",
            run.run_id
        )));
    }
    // A fresh codex run has no session id until `thread.started` names its
    // thread (nor does any harness that names its session late);
    // `handle_line` records it then.
    let names_session_late = adapter.names_session_late();
    let recorded_session = if names_session_late && !is_resume {
        None
    } else {
        Some(session_id.as_str())
    };
    if !is_resume && !names_session_late {
        state::set_run_session_and_usage(conn, &run.run_id, Some(&session_id), None, None)
            .map_err(kill_and_bail)?;
    }
    // brief §8: a review leaves the task `in_review` for its whole duration.
    // Only a verdict may move it, and only out of the settle tail below - so a
    // reviewer run makes no `tasks.status` write here at all. Without this
    // guard the task would flip `in_review -> running`, which every status
    // poll would see and which a crashed reviewer would leave behind
    // permanently (`running` is in neither DISPATCH_ENTRY_STATUSES nor
    // UPDATE_SPEC_STATUSES, so nothing could move it again).
    if !is_review {
        state::set_task_status(conn, &run.task_id, TaskStatus::Running, &now)
            .map_err(kill_and_bail)?;
    }
    state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::RUN_STARTED.to_string(),
            detail: serde_json::json!({
                "mode": if is_resume { "resume" } else { "fresh" },
                "worker_pid": worker_pid,
                "model": run.model,
                "profile": run.account,
                "session_id": recorded_session,
                "worktree": run.worktree,
                "timeout_seconds": run.timeout_seconds,
                "harness": profile.harness,
                "worker_image": worker_image,
                "effort_requested": launch_settings.as_ref().and_then(|l| l.effort_requested.clone()),
                "effort": launch_settings.as_ref().and_then(|l| l.effort.clone()),
            }),
        },
    )
    .map_err(kill_and_bail)?;

    // One inbox watch for the whole run: a retry's hooks append to the same
    // inbox, and the tailer's offset carries over.
    let mut watch = if !hooked {
        None
    } else {
        Some(InboxWatch {
            paths,
            repo_id,
            inbox_dir: inbox_dir.clone(),
            run_dir: run_dir.clone(),
            worktree: PathBuf::from(&run.worktree),
            tailer: evidence::InboxTailer::new(&inbox_dir),
            pairer: evidence::Pairer::default(),
            bad_lines: 0,
            recorded: 0,
        })
    };

    let cx = AttemptCtx {
        paths,
        conn,
        gconn: &gconn,
        repo_id,
        run,
        harness: &profile.harness,
        run_dir: &run_dir,
        stream_path: &stream_path,
        is_codex,
        is_review,
        research,
        started: Instant::now(),
        ctrl: ctrl.as_ref(),
    };
    let mut end = watch_attempt(&cx, spawned, known_session.as_deref(), &mut watch)?;

    // FC-2 (H5): a launch that ended in a transient failure is relaunched in
    // this same run - after 30 s, then after 2 min, at most twice - before
    // the run settles. Waits and retries count against the run's `--timeout`.
    let mut retries: usize = 0;
    // Why the runner stopped relaunching; `None` when the last launch was not
    // a transient failure.
    let mut retry_stop: Option<&'static str> = None;
    let mut retry_error: Option<String> = None;
    let mut evidence_from_launches: u64 = 0;
    let mut launch_stderr = stderr_path.clone();
    let (stream, tally, exit_code, kill, kill_error, drain_timed_out, agy_print_timeout) = loop {
        let AttemptEnd::Exited {
            stream,
            tally,
            exit_code,
            mut kill,
            kill_error,
            drain_timed_out,
            worker_pid: ended_pid,
            worker_image: ended_image,
            evidence_recorded,
        } = end
        else {
            // Already settled (the DB poll give-up or the kill-wait deadline).
            return Ok(());
        };
        evidence_from_launches += evidence_recorded;

        // agy's own `--print-timeout` ran out: it exits 0 with a SUCCESS result
        // and partial output. Settle it as thoth's own timeout, keeping agy's
        // line. Read from this launch's own stderr file.
        let mut agy_print_timeout: Option<String> = None;
        if kill.is_none() && profile.harness == crate::adapter::antigravity::HARNESS_NAME {
            agy_print_timeout = std::fs::read(&launch_stderr)
                .ok()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .and_then(|t| t.lines().find(|l| l.contains("print timeout after")).map(|l| l.trim().to_string()));
            if agy_print_timeout.is_some() {
                kill = Some(KillReason::Timeout);
            }
        }

        if !wants_retry(&tally, exit_code, kill) {
            break (stream, tally, exit_code, kill, kill_error, drain_timed_out, agy_print_timeout);
        }
        if retries == MAX_RETRIES {
            retry_stop = Some("exhausted");
            break (stream, tally, exit_code, kill, kill_error, drain_timed_out, agy_print_timeout);
        }
        let wait_seconds = retry_waits()[retries];
        let wait = Duration::from_secs(wait_seconds);
        if !wait_fits(cx.started.elapsed(), wait, run.timeout_seconds) {
            retry_stop = Some("timeout");
            break (stream, tally, exit_code, kill, kill_error, drain_timed_out, agy_print_timeout);
        }

        // The wait: the run stays `running`, the heartbeat keeps moving (so
        // `cancel`'s staleness check never mistakes this runner for a dead
        // one), and a cancel ends it at once. No event is written here.
        let wait_started = Instant::now();
        let mut last_heartbeat = Instant::now();
        let mut cancelled = false;
        loop {
            if last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
                let _ = state::touch_heartbeat(conn, &run.run_id, &ids::now_rfc3339());
                last_heartbeat = Instant::now();
            }
            match state::read_cancel_requested(conn, &run.run_id) {
                Ok(true) => {
                    cancelled = true;
                    break;
                }
                Ok(false) => {}
                Err(e) => log_runner_error(
                    &run_dir,
                    &format!("read_cancel_requested failed while waiting to retry: {e:#}"),
                ),
            }
            let waited = wait_started.elapsed();
            if waited >= wait {
                break;
            }
            std::thread::sleep(POLL_EVERY.min(wait - waited));
        }
        if cancelled {
            // Settles `cancelled`/`user` below, with the failed launch's
            // exit code; there is no worker left to kill.
            kill = Some(KillReason::Cancelled);
            retry_stop = Some("cancelled");
            break (stream, tally, exit_code, kill, kill_error, drain_timed_out, agy_print_timeout);
        }

        let error = classify_outcome(&tally, exit_code, &IngestOutcome::Skipped, kill)
            .note
            .strip_prefix("transient interruption: ")
            .map(str::to_string)
            .unwrap_or_default();
        let how = retry_launch(
            is_review,
            names_session_late,
            &session_id,
            known_session.as_deref(),
            tally.session_id.as_deref(),
        );
        // A fresh start over a late-naming harness clears the failed launch's
        // session so the new `init` / `thread.started` fills it through
        // `handle_line`; a fresh Claude reviewer gets a new session id (claude
        // refuses an existing `--session-id`).
        //
        // Accepted (FC-2 non-goal): codex and agy report their usage for the
        // whole thread / conversation, so after a fresh start the run's token
        // columns hold the new conversation's numbers and miss the failed
        // one's. Claude's per-turn usage adds up and is unaffected.
        let (resume_session, launch_prompt, mode, event_session) = match how {
            RetryLaunch::Resume(s) => (
                Some(s.clone()),
                claude_code::RESUME_PROMPT.to_string(),
                "resume",
                Some(s),
            ),
            RetryLaunch::FreshNamedSession => {
                session_id = ids::new_session_uuid();
                if let Err(e) = state::reset_run_session(conn, &run.run_id, Some(&session_id)) {
                    log_runner_error(&run_dir, &format!("reset_run_session failed: {e:#}"));
                }
                (None, prompt.clone(), "fresh", Some(session_id.clone()))
            }
            RetryLaunch::FreshLateSession => {
                if let Err(e) = state::reset_run_session(conn, &run.run_id, None) {
                    log_runner_error(&run_dir, &format!("reset_run_session failed: {e:#}"));
                }
                (None, prompt.clone(), "fresh", None)
            }
        };
        let attempt = retries + 1;
        launch_stderr = run_dir.join(format!("stderr.retry{attempt}.log"));
        // Untested by an automated test (FC-2 non-goal): it takes a harness
        // executable that vanishes mid-run, which the fakes cannot do. On
        // failure the run settles from the failed launch, through the same
        // path `retry_stop = "timeout"` takes.
        let spawned = match spawn_attempt(
            ctrl.as_ref(),
            adapter.as_ref(),
            &base.request(launch_prompt, &session_id, resume_session.as_deref()),
            &launch_stderr,
            &run_dir,
            is_codex,
            &masker,
        ) {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("{e:#}");
                log_runner_error(
                    &run_dir,
                    &format!(
                        "relaunching the harness after worker {ended_pid} ({}) failed transiently did not start: {msg}",
                        ended_image.as_deref().unwrap_or("unknown image")
                    ),
                );
                retry_stop = Some("launch_error");
                retry_error = Some(msg.lines().next().unwrap_or("").to_string());
                break (stream, tally, exit_code, kill, kill_error, drain_timed_out, agy_print_timeout);
            }
        };
        // `cancel`'s second layer kills `runs.worker_pid`.
        if let Err(e) = state::set_worker_pid(conn, &run.run_id, spawned.worker_pid as i64) {
            log_runner_error(&run_dir, &format!("set_worker_pid failed: {e:#}"));
        }
        retries = attempt;
        let _ = state::append_event(
            conn,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::RUN_RETRY.to_string(),
                detail: serde_json::json!({
                    "attempt": attempt,
                    "error": error,
                    "exit_code": exit_code,
                    "wait_seconds": wait_seconds,
                    "mode": mode,
                    "session_id": event_session,
                    "worker_pid": spawned.worker_pid,
                    "worker_image": spawned.worker_image,
                }),
            },
        );
        let _ = state::touch_heartbeat(conn, &run.run_id, &ids::now_rfc3339());
        drop(stream);
        end = watch_attempt(&cx, spawned, resume_session.as_deref(), &mut watch)?;
    };

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
        if clean_success {
            let ctx = evidence::EvidenceContext {
                paths,
                repo_id,
                task_id: &run.task_id,
                run_id: &run.run_id,
            };
            let git_dir = Path::new(&run.worktree);
            // SMALL-4: rule 6 counts against the task's own base (a rework's
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

    let mut outcome = classify_outcome(&tally, exit_code, &ingest_outcome, kill);
    if let Some(line) = agy_print_timeout {
        outcome.note = format!(
            "{line} (agy's own print timeout, not thoth's wall-clock limit; agy below 1.2.6 stops after 5 minutes by default)"
        );
    }
    // brief §5: for a reviewer run the *task* status is the verdict. The run
    // status and the interruption classification are untouched - quota, crash
    // and timeout mean the same thing for a reviewer as for a worker.
    if let Some(v) = review_verdict {
        outcome.note = format!("review verdict {}: {}", v.as_str(), outcome.note);
    }
    if let Some(v) = spec_verdict {
        outcome.note = format!("spec review verdict {}: {}", v.as_str(), outcome.note);
    }
    // FC-1 (H2): a run that ended in error, not quota and not our own kill,
    // gets one auth check (the one `profile check` runs; it spends no quota).
    // Only the CLI itself saying "not logged in" reclassifies the run.
    let login_check = if kill.is_none() && needs_login_check(&outcome) {
        Some(apply_login_check(
            &mut outcome,
            &run.account,
            crate::commands::profile::probe_auth_profile(&profile),
        ))
    } else {
        None
    };

    // The one decision that differs by role, in one place. An implementer run
    // writes whatever `classify_outcome` decided, exactly as in Phase 1. A
    // reviewer run writes its verdict or nothing at all - never `running`,
    // never `failed`, never `interrupted` - so a review that did not reach a
    // verdict leaves the task `in_review` and `thoth review start` can be
    // run again.
    //
    // Whole-branch review Important 1: the reviewer's write is *conditional*
    // on the task still being `in_review`. A Supervisor who rerouted the task
    // while the review was in flight (or a second review that landed first)
    // must not have a stale verdict stamped over what the task is doing now;
    // `settle_terminal` records the not-applied verdict instead.
    // SR-2: a spec reviewer never writes the task's status, whatever happened.
    let task_status: Option<TaskStatusWrite> = if is_spec_review || review_settled {
        None
    } else if is_review {
        review_verdict
            .map(|v| TaskStatusWrite::conditional(VERDICT_ENTRY_STATUSES, v.task_status()))
    } else {
        Some(TaskStatusWrite::unconditional(outcome.task_status))
    };
    let candidate_commit = ingest_candidate.or(candidate_commit);
    let ended_at = ids::now_rfc3339();
    let finish = RunFinish {
        status: outcome.run_status,
        interrupted_reason: outcome.interrupted_reason,
        exit_code: exit_code.map(|c| c as i64),
        candidate_commit: candidate_commit.clone(),
        error_text: if outcome.run_status == RunStatus::Completed {
            None
        } else {
            Some(outcome.note.clone())
        },
        ended_at: ended_at.clone(),
    };
    let mut status_change_detail = serde_json::json!({
        "entity": "run",
        "id": run.run_id,
        "to": outcome.run_status.as_str(),
        "interrupted_reason": outcome.interrupted_reason.map(|r| r.as_str()),
        "exit_code": exit_code,
        "candidate_commit": candidate_commit,
        "candidate_commit_error": candidate_commit_error,
        "note": outcome.note,
        "stream_lines": tally.lines,
        "unparsable_lines": tally.unparsable_lines,
        "unparsable_bytes": tally.unparsable_bytes,
        "result_subtype": tally.result.as_ref().and_then(|r| r.subtype.clone()),
        "saw_rate_limit_warning": tally.saw_rate_limit_warning,
        "saw_api_retry": tally.saw_api_retry,
        "stream_drain_timed_out": drain_timed_out,
        "kill_error": kill_error,
        "evidence_recorded": watch.as_ref().map(|w| w.recorded).unwrap_or(0)
            + evidence_from_launches,
        "inbox_bad_lines": watch.as_ref().map(|w| w.bad_lines).unwrap_or(0),
        "inbox_unpaired": watch.as_ref().map(|w| w.pairer.pending()).unwrap_or(0),
        "review_verdict": review_verdict.map(|v| v.as_str()),
        "spec_review_verdict": spec_verdict.map(|v| v.as_str()),
        // Overwritten with `null` by `settle_terminal` if a conditional write
        // turns out not to apply, with `task_status_not_applied` beside it.
        "task_status_written": task_status.map(|w| w.to.as_str()),
        // FC-1: what the runner's auth check said; null when none ran.
        "login_check": login_check,
        // FC-2: how many `run_retry` events this run wrote, why the runner
        // stopped relaunching (null: the last launch was not a transient
        // failure), and, for `launch_error`, what failed.
        "retries": retries,
        "retry_stop": retry_stop,
        "retry_error": retry_error,
    });
    let interrupted_detail = if outcome.run_status == RunStatus::Interrupted {
        Some(serde_json::json!({
            "reason": outcome.interrupted_reason.map(|r| r.as_str()),
            "note": outcome.note,
            "harness_session_id": tally.session_id,
        }))
    } else {
        None
    };
    // S3-12: before the final `status_change`, whatever the outcome.
    crate::run_quota::record(
        conn,
        &gconn,
        paths,
        repo_id,
        run,
        &profile.harness,
        tally.session_id.as_deref(),
        &ended_at,
    );
    // MODELS-1: a model this account cannot use, learned from the result line.
    record_model_unavailable(
        conn,
        &gconn,
        &run_dir,
        run,
        &outcome,
        tally.result.as_ref(),
        &mut status_change_detail,
    );
    finish_and_settle(
        conn,
        &gconn,
        &run_dir,
        run,
        &outcome,
        &finish,
        task_status,
        interrupted_detail,
        status_change_detail,
    )?;
    crate::records::export_quietly(paths, &run_dir);
    // WT-1: a task that just became pass or cancelled no longer needs its
    // worker worktree. Never `?` and never touches the run's outcome; the
    // review worktree is left for the next full sweep.
    if let Ok(Some(task)) = state::get_task(conn, &run.task_id) {
        if task.status == TaskStatus::Pass.as_str() || task.status == TaskStatus::Cancelled.as_str() {
            use crate::commands::worktrees::{sweep, Scope, SOURCE_RUNNER};
            match sweep(paths, repo_id, repo_root, conn, Scope::Task(&run.task_id), SOURCE_RUNNER) {
                Ok(s) => {
                    for k in &s.kept {
                        let first = k.detail.lines().next().unwrap_or("");
                        log_runner_error(
                            &run_dir,
                            &format!("worktree kept: {} ({}): {first}", k.path, k.reason),
                        );
                    }
                }
                Err(e) => log_runner_error(&run_dir, &format!("worktree sweep failed: {e:#}")),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ResultEvent;
    use crate::ingest::IngestOutcome;

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
}
