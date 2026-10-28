//! What an antigravity launch adds to the runner (scope §2 G1, G2): its
//! commands become evidence once their exit code is in agy's transcript, its
//! tool steps and step tokens are counted, and its `run_usage` waits until the
//! stream has ended so it can carry the tool lock's denials.

use super::{exit_code_for_step, transcript_path, ExitCodeSource, StepExitCode, DENIALS_FILE};
use crate::adapter::{LaunchHooks, ResultEvent, RunPlace, StreamEvent};
use crate::db::state::{self, NewEvent};
use crate::evidence;
use crate::model::event_type;
use crate::runner::{append_usage, log_runner_error};
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The tools whose steps count as a spawned subagent in an antigravity run.
const AGY_SUBAGENT_TOOLS: &[&str] = &["invoke_subagent", "define_subagent", "browser_subagent"];

/// An antigravity `run_command` step that reached DONE and is waiting for its
/// exit code to appear in agy's transcript.
struct PendingCommand {
    cmd: String,
    output: String,
}

/// One agy launch's state, fed by the runner while the stream is read.
pub struct AgyLaunch {
    /// A reviewer run: its commands are never recorded as evidence.
    reviewer: bool,
    /// The profile's data directory, where the transcript lives.
    config_dir: PathBuf,
    /// The transcript's path, once the conversation id is known (from
    /// `init`, or from `runs.harness_session_id` on a resume).
    transcript: Option<PathBuf>,
    /// DONE `run_command` steps not recorded yet, by step index.
    pending: BTreeMap<i64, PendingCommand>,
    /// Steps already recorded, so a repeated DONE update does not record a
    /// second row.
    recorded_steps: BTreeSet<i64>,
    /// A transcript read failure is logged once per run.
    transcript_error_logged: bool,
    /// The sum of every step's `output_tokens` on this launch.
    step_output_tokens: i64,
    /// The step indices seen for each tool name.
    tool_steps: BTreeMap<String, BTreeSet<i64>>,
    /// The `run_usage` detail and subagent count from the result line, held
    /// until the stream has ended and the denials are counted.
    agy_usage: Option<(serde_json::Value, Option<i64>)>,
}

impl AgyLaunch {
    /// A resumed agy run continues the conversation it already has, so its
    /// transcript is known before `init` repeats the id.
    pub fn new(run: &state::Run, resume_session: Option<&str>) -> Self {
        let config_dir = PathBuf::from(&run.config_dir);
        let transcript = resume_session.map(|id| transcript_path(&config_dir, id));
        Self {
            reviewer: run.role == crate::model::role::REVIEWER || run.role == crate::model::role::SPEC_REVIEWER,
            config_dir,
            transcript,
            pending: BTreeMap::new(),
            recorded_steps: BTreeSet::new(),
            transcript_error_logged: false,
            step_output_tokens: 0,
            tool_steps: BTreeMap::new(),
            agy_usage: None,
        }
    }

    /// Antigravity's subagents: distinct steps of a subagent tool.
    fn agy_subagents(&self) -> i64 {
        let steps: BTreeSet<i64> = AGY_SUBAGENT_TOOLS
            .iter()
            .filter_map(|t| self.tool_steps.get(*t))
            .flatten()
            .copied()
            .collect();
        steps.len() as i64
    }

    /// One antigravity command as `command` evidence; an unknown exit code
    /// also gets its warning event. Returns whether the row was written.
    #[allow(clippy::too_many_arguments)]
    fn record_agy_command(
        &mut self,
        conn: &Connection,
        run: &state::Run,
        place: &RunPlace<'_>,
        session_id: Option<&str>,
        step: i64,
        pending: &PendingCommand,
        exit_code: Option<i64>,
        source: Option<ExitCodeSource>,
    ) -> bool {
        let ctx = evidence::EvidenceContext {
            paths: place.paths,
            repo_id: place.repo_id,
            task_id: &run.task_id,
            run_id: &run.run_id,
        };
        let status = exit_code.map(|c| if c == 0 { "completed" } else { "failed" });
        let row = evidence::StreamCommand {
            cmd: &pending.cmd,
            cwd: &run.worktree,
            exit_code,
            status,
            output: &pending.output,
            truncated: false,
            session_id,
            item_id: None,
            event: "antigravity_command",
            step_index: Some(step),
            exit_code_source: source.map(|s| s.as_str()),
        };
        let code = match evidence::record_stream_command(conn, &ctx, &row) {
            Ok(code) => code,
            Err(e) => {
                log_runner_error(
                    &place.run_dir,
                    &format!("recording command evidence failed: {e:#}"),
                );
                return false;
            }
        };
        if exit_code.is_none() {
            let _ = state::append_event(
                conn,
                &NewEvent {
                    task_id: Some(run.task_id.clone()),
                    run_id: Some(run.run_id.clone()),
                    event_type: event_type::COMMAND_EXIT_CODE_UNKNOWN.to_string(),
                    detail: serde_json::json!({
                        "code": code,
                        "step_index": step,
                        "cmd": pending.cmd,
                        "note": "the command's exit code never appeared in agy's transcript; recorded as null",
                    }),
                },
            );
        }
        true
    }
}

impl LaunchHooks for AgyLaunch {
    fn on_session(&mut self, session_id: &str) {
        self.transcript = Some(transcript_path(&self.config_dir, session_id));
    }

    /// A tool step: counted by name, and a finished `run_command` waits for
    /// its exit code in the transcript (the stream carries none, scope §1 E2).
    fn on_event(&mut self, event: &StreamEvent) {
        match event {
            StreamEvent::ToolStep {
                step_index,
                tool_name,
                command,
                output,
                done,
            } => {
                let step_index = *step_index;
                let is_command = tool_name == "run_command";
                self.tool_steps.entry(tool_name.clone()).or_default().insert(step_index);
                if *done && is_command && !self.reviewer && !self.recorded_steps.contains(&step_index) {
                    self.pending.entry(step_index).or_insert(PendingCommand {
                        cmd: command.clone().unwrap_or_default(),
                        output: output.clone().unwrap_or_default(),
                    });
                }
            }
            StreamEvent::StepUsage { output_tokens, .. } => {
                self.step_output_tokens += output_tokens;
            }
            _ => {}
        }
    }

    /// agy's result usage spans the whole conversation (a resume included),
    /// so the requested model takes this launch's step tokens; subagents are
    /// the subagent tool steps seen.
    fn on_result(&mut self, result: &mut ResultEvent, requested_model: &str) {
        result.model_usage = Some(vec![(requested_model.to_string(), self.step_output_tokens)]);
        result.subagents_spawned = Some(self.agy_subagents());
    }

    fn extend_usage(&self, usage: &mut serde_json::Value) {
        usage["tool_names"] = serde_json::json!(self.tool_steps.keys().collect::<Vec<_>>());
    }

    /// Held until agy's stream has ended, then written with `tools_denied`
    /// by `after_stream`, which every path out of the runner after the spawn
    /// calls: the DB poll give-up, the kill-wait deadline, a `try_wait` or
    /// `wait` error, and the common exit after the loop that clean exits,
    /// crashes, timeouts and cancels all break to. An agy reviewer is locked
    /// too (scope G2b), so it is held for a reviewer as well.
    fn hold_usage(
        &mut self,
        usage: serde_json::Value,
        subagents: Option<i64>,
    ) -> Option<(serde_json::Value, Option<i64>)> {
        self.agy_usage = Some((usage, subagents));
        None
    }

    /// Records every pending antigravity command whose exit code agy's
    /// transcript now holds, in step order, re-reading the whole file. With
    /// `last` (the stream has ended) the rest are recorded too, with a null
    /// exit code and a `command_exit_code_unknown` event each. Best-effort:
    /// a missing or unreadable transcript is one line in runner-error.log,
    /// never a failed run (scope §3).
    fn settle_commands(
        &mut self,
        conn: &Connection,
        run: &state::Run,
        place: &RunPlace<'_>,
        session_id: Option<&str>,
        last: bool,
    ) -> u64 {
        if self.pending.is_empty() {
            return 0;
        }
        let text = match &self.transcript {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(text) => Some(text),
                Err(e) => {
                    // Not there yet is normal mid-run; only the final pass, or
                    // a real read error, is worth the log line.
                    if (last || e.kind() != std::io::ErrorKind::NotFound)
                        && !self.transcript_error_logged
                    {
                        self.transcript_error_logged = true;
                        log_runner_error(
                            &place.run_dir,
                            &format!("reading agy's transcript {} failed: {e}", path.display()),
                        );
                    }
                    None
                }
            },
            None => {
                if last && !self.transcript_error_logged {
                    self.transcript_error_logged = true;
                    log_runner_error(
                        &place.run_dir,
                        "agy never named its conversation, so its transcript could not be read",
                    );
                }
                None
            }
        };
        let conversation = session_id.unwrap_or_default();
        let steps: Vec<i64> = self.pending.keys().copied().collect();
        let mut added = 0u64;
        for step in steps {
            let found = text
                .as_deref()
                .map(|t| exit_code_for_step(t, conversation, step));
            let (exit_code, source) = match found {
                Some(StepExitCode::Known { code, source }) => (Some(code), Some(source)),
                _ if last => (None, None),
                _ => continue,
            };
            let Some(pending) = self.pending.remove(&step) else { continue };
            self.recorded_steps.insert(step);
            if self.record_agy_command(conn, run, place, session_id, step, &pending, exit_code, source) {
                added += 1;
            }
        }
        if added > 0 {
            if let Err(e) = evidence::refresh_index(conn, &run.task_id, &place.worktree) {
                log_runner_error(
                    &place.run_dir,
                    &format!("rewriting the evidence index failed: {e:#}"),
                );
            }
        }
        added
    }

    /// Once agy's stream has ended: one `tool_denied` event per line of
    /// `.agent/agy-hook-denials.jsonl` (missing = none), then the held-back
    /// `run_usage` with `tools_denied` set to that count and (AGY-1, AC2)
    /// `tools_unknown`, the distinct names among them Thoth does not know,
    /// sorted (`[]` when none).
    fn after_stream(&mut self, conn: &Connection, run: &state::Run, place: &RunPlace<'_>) {
        let path = place.worktree.join(".agent").join(DENIALS_FILE);
        let text = match std::fs::read(&path) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => {
                log_runner_error(&place.run_dir, &format!("reading {} failed: {e}", path.display()));
                String::new()
            }
        };
        let mut denied: i64 = 0;
        let mut unknown: BTreeSet<String> = BTreeSet::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v = match serde_json::from_str::<serde_json::Value>(line) {
                Ok(v) => v,
                Err(e) => {
                    log_runner_error(&place.run_dir, &format!("unparsable line in {}: {e}", path.display()));
                    continue;
                }
            };
            denied += 1;
            // AGY-1: the hook's own verdict, or (a denial written by an
            // older hook) the same rule applied to the name it recorded. A
            // record without a usable name is never an unknown name.
            let tool = v["tool"].as_str().filter(|t| !t.is_empty());
            let known_to_thoth = match v["known_to_thoth"].as_bool() {
                Some(known) => known,
                None => tool.is_some_and(super::is_known_tool),
            };
            if let Some(tool) = tool.filter(|_| !known_to_thoth) {
                unknown.insert(tool.to_string());
            }
            let _ = state::append_event(
                conn,
                &NewEvent {
                    task_id: Some(run.task_id.clone()),
                    run_id: Some(run.run_id.clone()),
                    event_type: event_type::TOOL_DENIED.to_string(),
                    detail: serde_json::json!({
                        "tool": v["tool"],
                        "known_to_thoth": known_to_thoth,
                        "reason": v["reason"],
                        "step_index": v["step_idx"],
                        "conversation_id": v["conversation_id"],
                        "received_at": v["received_at"],
                    }),
                },
            );
        }
        if let Some((mut usage, subagents)) = self.agy_usage.take() {
            usage["tools_denied"] = serde_json::json!(denied);
            usage["tools_unknown"] = serde_json::json!(unknown);
            append_usage(conn, run, usage, subagents);
        }
    }
}

/// An agy reviewer's verdict: the result's `structured_output` object as
/// pretty JSON at `path`. A missing or null one writes nothing; any other
/// non-object writes nothing and is logged with its JSON type. Best-effort:
/// a failed write is logged and the missing file becomes `review_invalid`.
pub fn write_agy_review(path: &Path, result: Option<&ResultEvent>, run_dir: &Path) {
    let Some(value) = result.and_then(|r| r.structured_output.as_ref()) else {
        return;
    };
    let Some(doc) = value.as_object().map(|_| value) else {
        let kind = match value {
            serde_json::Value::Null => return,
            serde_json::Value::Bool(_) => "boolean",
            serde_json::Value::Number(_) => "number",
            serde_json::Value::String(_) => "string",
            _ => "array",
        };
        let run_id = run_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        log_runner_error(run_dir, &format!("agy reviewer run {run_id}: structured_output is a JSON {kind}, not an object; no review written"));
        return;
    };
    let written = serde_json::to_string_pretty(doc)
        .map_err(anyhow::Error::from)
        .and_then(|text| std::fs::write(path, text).map_err(anyhow::Error::from));
    if let Err(e) = written {
        log_runner_error(run_dir, &format!("writing the agy reviewer's verdict to {} failed: {e:#}", path.display()));
    }
}
