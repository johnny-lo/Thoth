use crate::adapter::{codex, HarnessAdapter, LaunchHooks, ResultEvent, RunPlace, StreamEvent};
use crate::db::state::{self, NewEvent};
use crate::evidence;
use crate::model::event_type;
use crate::process;
use super::outcome::StreamTally;
use super::settle::log_runner_error;
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};

/// The harness a run's stream comes from, and what the runner collects from
/// it besides the `StreamTally`: codex's commands become evidence as they
/// arrive, and its item kinds are counted; anything else the harness adds
/// goes through its launch hooks.
pub(super) struct StreamState<'a> {
    pub(super) adapter: Box<dyn HarnessAdapter>,
    /// The adapter's per-launch additions (`HarnessAdapter::launch_hooks`).
    pub(super) launch: Box<dyn LaunchHooks>,
    pub(super) codex: bool,
    /// A reviewer run: its commands are never recorded as evidence.
    pub(super) reviewer: bool,
    /// A worker or source-check reviewer of a research task: codex has the
    /// web search on, so a `web_search` item is expected.
    pub(super) research: bool,
    pub(super) worker_pid: u32,
    pub(super) place: RunPlace<'a>,
    /// Evidence rows recorded straight from the stream (codex) or by the
    /// launch hooks.
    pub(super) evidence_recorded: u64,
    /// Item kinds outside `codex::EXPECTED_ITEM_TYPES`, each with the ids
    /// seen, so a started-then-completed item counts once.
    pub(super) unexpected_items: BTreeMap<String, BTreeSet<String>>,
}

impl StreamState<'_> {
    /// Collab/subagent items: codex's own form of a spawned subagent.
    pub(super) fn subagents(&self) -> i64 {
        self.unexpected_items
            .iter()
            .filter(|(t, _)| t.contains("collab") || t.contains("spawn") || t.contains("subagent"))
            .map(|(_, ids)| ids.len() as i64)
            .sum()
    }

    /// The launch hooks' commands whose outcome is now known (all of them
    /// with `last`), counted into `evidence_recorded`.
    pub(super) fn settle_commands(&mut self, conn: &Connection, run: &state::Run, session_id: Option<&str>, last: bool) {
        self.evidence_recorded += self.launch.settle_commands(conn, run, &self.place, session_id, last);
    }

    /// The stream has ended: the launch hooks write what they held back.
    pub(super) fn after_stream(&mut self, conn: &Connection, run: &state::Run) {
        self.launch.after_stream(conn, run, &self.place);
    }

    /// Records one codex command as `command` evidence and republishes the
    /// index, so `.agent/evidence-index.json` is current before the worker
    /// writes result.json. Best-effort like the inbox drain: a failure is
    /// logged, never fatal.
    pub(super) fn record_command(
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

pub(super) fn note_harness_error(tally: &mut StreamTally, r: ResultEvent) {
    let replaceable = match &tally.result {
        None => true,
        Some(prev) => prev.subtype == r.subtype,
    };
    if replaceable {
        tally.result = Some(r);
    }
}

pub(super) fn handle_line(
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
