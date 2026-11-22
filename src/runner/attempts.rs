use crate::adapter::claude_code;
use crate::config::Paths;
use crate::db::state::{self, NewEvent};
use crate::evidence;
use crate::ids;
use crate::ingest::IngestOutcome;
use crate::model::{event_type, TaskStatus};
use crate::process;
use super::launch::{give_up_after_db_failures, spawn_attempt, watch_attempt, AttemptCtx, AttemptEnd, LaunchBase, LaunchFiles};
use super::outcome::{classify_outcome, KillReason, StreamTally};
use super::prepare::RunPrep;
use super::prompt::BuiltPrompt;
use super::retry::{retry_launch, retry_waits, wait_fits, wants_retry, RetryLaunch, MAX_RETRIES};
use super::review_inputs::ReviewInputs;
use super::settle::log_runner_error;
use super::stream::StreamState;
use super::{InboxWatch, DB_POLL_FAILURE_LIMIT, HEARTBEAT_EVERY, POLL_EVERY};
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// The cancel read of the FC-2 retry wait (and nowhere else). Debug builds
/// only: when `THOTH_TEST_RETRY_WAIT_CANCEL_READ_FAIL` is set it returns an
/// error instead of reading, so a test can make the wait give up through the
/// CLI. A release build has no such switch and ignores the variable.
fn read_cancel_in_retry_wait(conn: &Connection, run_id: &str) -> Result<bool> {
    #[cfg(debug_assertions)]
    if std::env::var_os("THOTH_TEST_RETRY_WAIT_CANCEL_READ_FAIL").is_some() {
        return Err(anyhow!("cancel read failed (THOTH_TEST_RETRY_WAIT_CANCEL_READ_FAIL)"));
    }
    state::read_cancel_requested(conn, run_id)
}

/// AGY-1 (second part): the level a launch sent and agy's text, when agy
/// refused that `--effort`: the launch carried the flag (an effort that is not
/// the level the model name carries), and the result's error, or with no
/// result line agy's stderr, holds `--effort is not supported for model`.
/// Recognised by agy's own text, never by the model's name.
fn effort_refusal(base: &LaunchBase<'_>, tally: &StreamTally, stderr: &std::path::Path) -> Option<(String, String)> {
    let sent = base.effort.clone()?;
    if crate::adapter::effort::carried_level(crate::adapter::antigravity::HARNESS_NAME, base.model).as_deref()
        == Some(sent.as_str())
    {
        return None;
    }
    let marker = crate::model_list::EFFORT_NOT_SUPPORTED.to_ascii_lowercase();
    let text = match &tally.result {
        // agy's refusal is an ERROR result; a SUCCESS response that only
        // quotes the text is not one.
        Some(result) if !result.is_error => return None,
        Some(result) => result.text.clone().unwrap_or_default(),
        None => {
            let bytes = std::fs::read(stderr).ok()?;
            let text = String::from_utf8_lossy(&bytes).into_owned();
            text.lines()
                .find(|l| l.to_ascii_lowercase().contains(&marker))
                .map(|l| l.trim().to_string())
                .unwrap_or_default()
        }
    };
    text.to_ascii_lowercase().contains(&marker).then(|| (sent, text.trim().to_string()))
}

/// AGY-1 (second part): the run's stored launch settings rewritten, so
/// `status` and a later `thoth resume` see what was really sent. A failure
/// is one line in runner-error.log.
fn store_launch_settings(
    conn: &Connection,
    run: &state::Run,
    run_dir: &std::path::Path,
    settings: &crate::adapter::effort::LaunchSettings,
) {
    let written = crate::adapter::effort::with_launch(&run.spec, settings)
        .and_then(|spec| state::set_run_spec(conn, &run.run_id, &spec));
    if let Err(e) = written {
        log_runner_error(run_dir, &format!("storing the run's launch settings failed: {e:#}"));
    }
}

/// What the fourth section of `run_worker_inner` leaves for the later ones:
/// the last launch's stream and tally, how it ended, and the FC-2 retry
/// bookkeeping.
pub(super) struct Launched<'a> {
    pub(super) stream: StreamState<'a>,
    pub(super) tally: StreamTally,
    pub(super) exit_code: Option<i32>,
    pub(super) kill: Option<KillReason>,
    pub(super) kill_error: Option<String>,
    pub(super) drain_timed_out: bool,
    pub(super) agy_print_timeout: Option<String>,
    pub(super) watch: Option<InboxWatch<'a>>,
    pub(super) retries: usize,
    pub(super) retry_stop: Option<&'static str>,
    pub(super) retry_error: Option<String>,
    pub(super) evidence_from_launches: u64,
}

// The moved statements borrow `&masker`, `&run_dir` and the like as they did when these
// were owned locals; here they are already references, so the `&` is redundant and kept
// so the statements stay word for word the same.
#[allow(
    clippy::needless_borrow,
    clippy::needless_borrows_for_generic_args,
    clippy::too_many_arguments
)]
pub(super) fn launch_and_watch<'a>(
    paths: &'a Paths,
    conn: &'a Connection,
    repo_id: &'a str,
    run: &'a state::Run,
    prep: &'a RunPrep,
    inputs: &ReviewInputs,
    built: BuiltPrompt,
) -> Result<Option<Launched<'a>>> {
    let BuiltPrompt { is_resume, session_id, prompt } = built;
    let gconn = &prep.gconn;
    let profile = &prep.profile;
    let adapter = &prep.adapter;
    let masker = &prep.masker;
    let run_dir = &prep.run_dir;
    let stream_path = &prep.stream_path;
    let stderr_path = &prep.stderr_path;
    let inbox_dir = &prep.inbox_dir;
    let is_codex = prep.is_codex;
    let is_review = prep.is_review;
    let research = prep.research;
    let hooked = prep.hooked;
    let settings_path = prep.settings_path.clone();
    let extra_env = prep.extra_env.clone();
    let review_output = inputs.review_output.clone();

    // S3-7: the launch settings dispatch / review start stored in `runs.spec`;
    // a resume re-reads them here, so it keeps the original run's effort.
    let mut launch_settings = crate::adapter::effort::parse_launch(&run.spec);
    // AGY-1 (second part): an agy model whose effort agy already refused on
    // this profile is sent none, whatever the run's stored settings say (a
    // `thoth resume` of a run launched before the refusal was recorded).
    if let (Some(settings), Some(no_effort)) = (
        launch_settings.as_mut(),
        crate::model_list::no_effort_for(gconn, &profile.harness, &run.account, &run.model),
    ) {
        if settings.effort.is_some() {
            settings.effort = None;
            settings.effort_note = Some(no_effort.launch_note(&run.model, &run.account));
            store_launch_settings(conn, run, run_dir, settings);
        }
    }
    let mut base = LaunchBase {
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
    // SMALL-16: agy's prompt goes into a file when it would not fit the
    // command line: the run's working directory for an implementer, the
    // reviewer's input folder for a reviewer or a spec reviewer.
    let long_prompt_file = (profile.harness == crate::adapter::antigravity::HARNESS_NAME)
        .then(|| crate::adapter::antigravity::prompt_file_path(is_review));
    let mut session_id = session_id;
    // The session a `thoth resume`d run continues; FC-2's retry falls back
    // on it when a late-naming harness failed before naming one.
    let known_session: Option<String> = if is_resume { Some(session_id.clone()) } else { None };

    // The returned `stream` borrows through `cx`, which borrows `ctrl`, and it
    // must outlive this function, so the controller is leaked to get a `'static`
    // borrow. The controller is a unit struct with no `Drop`, and `run-worker`
    // runs this once per process, so the leak costs nothing that matters.
    let ctrl: &'static dyn process::ProcessController = Box::leak(process::controller());
    let spawned = spawn_attempt(
        ctrl,
        adapter.as_ref(),
        &base.request(prompt.clone(), &session_id, known_session.as_deref()),
        &LaunchFiles { stderr_path: &stderr_path, run_dir: &run_dir, long_prompt_file },
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
        ctrl,
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
    // AGY-1 (second part): only the run's first launch is looked at for a
    // refused effort; the relaunch carries none.
    let mut effort_checked = false;
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
            return Ok(None);
        };
        evidence_from_launches += evidence_recorded;

        // AGY-1 (second part): agy refused the `--effort` this launch carried
        // (before any model turn). Relaunched at once, without it, in this
        // same run: not a transient retry, so no wait, no `run_retry`, and
        // neither retry is used up.
        if !effort_checked {
            effort_checked = true;
            let refusal = if kill.is_none() && profile.harness == crate::adapter::antigravity::HARNESS_NAME {
                effort_refusal(&base, &tally, &launch_stderr)
            } else {
                None
            };
            if let Some((sent, error)) = refusal {
                base.effort = None;
                let relaunch_stderr = run_dir.join("stderr.no-effort.log");
                match spawn_attempt(
                    ctrl,
                    adapter.as_ref(),
                    &base.request(prompt.clone(), &session_id, known_session.as_deref()),
                    &LaunchFiles { stderr_path: &relaunch_stderr, run_dir: &run_dir, long_prompt_file },
                    is_codex,
                    &masker,
                ) {
                    Ok(spawned) => {
                        launch_stderr = relaunch_stderr;
                        if let Err(e) = state::set_worker_pid(conn, &run.run_id, spawned.worker_pid as i64) {
                            log_runner_error(&run_dir, &format!("set_worker_pid failed: {e:#}"));
                        }
                        if known_session.is_none() && names_session_late {
                            if let Err(e) = state::reset_run_session(conn, &run.run_id, None) {
                                log_runner_error(&run_dir, &format!("reset_run_session failed: {e:#}"));
                            }
                        }
                        let mut settings = launch_settings.clone().unwrap_or_default();
                        settings.effort = None;
                        settings.effort_note =
                            Some(format!("agy refused --effort {sent} for {}; sent without an effort", run.model));
                        store_launch_settings(conn, run, &run_dir, &settings);
                        if let Err(e) = crate::model_list::record_effort_refused(
                            gconn,
                            &run.account,
                            &run.model,
                            &sent,
                            &run.run_id,
                            &run.task_id,
                            &error,
                        ) {
                            log_runner_error(&run_dir, &format!("recording the refused effort failed: {e:#}"));
                        }
                        let _ = state::append_event(
                            conn,
                            &NewEvent {
                                task_id: Some(run.task_id.clone()),
                                run_id: Some(run.run_id.clone()),
                                event_type: event_type::EFFORT_REFUSED.to_string(),
                                detail: serde_json::json!({
                                    "profile": run.account,
                                    "model": run.model,
                                    "effort": sent,
                                    "error": error,
                                    "worker_pid": spawned.worker_pid,
                                    "worker_image": spawned.worker_image,
                                }),
                            },
                        );
                        let _ = state::touch_heartbeat(conn, &run.run_id, &ids::now_rfc3339());
                        drop(stream);
                        end = watch_attempt(&cx, spawned, known_session.as_deref(), &mut watch)?;
                        continue;
                    }
                    Err(e) => {
                        // Settles from the refused launch, as a failed run.
                        log_runner_error(
                            &run_dir,
                            &format!(
                                "relaunching agy without --effort after worker {ended_pid} ({}) was refused did not start: {e:#}",
                                ended_image.as_deref().unwrap_or("unknown image")
                            ),
                        );
                    }
                }
            }
        }

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
        // Consecutive failed cancel reads; `touch_heartbeat` failures stay
        // ignored here. At the limit the runner gives up like the launch's
        // own loop does.
        let mut cancel_read_failures: u32 = 0;
        loop {
            if last_heartbeat.elapsed() >= HEARTBEAT_EVERY {
                let _ = state::touch_heartbeat(conn, &run.run_id, &ids::now_rfc3339());
                last_heartbeat = Instant::now();
            }
            match read_cancel_in_retry_wait(conn, &run.run_id) {
                Ok(true) => {
                    cancelled = true;
                    break;
                }
                Ok(false) => cancel_read_failures = 0,
                Err(e) => {
                    cancel_read_failures += 1;
                    log_runner_error(
                        &run_dir,
                        &format!(
                            "read_cancel_requested failed while waiting to retry ({cancel_read_failures}/{DB_POLL_FAILURE_LIMIT}): {e:#}"
                        ),
                    );
                    if cancel_read_failures >= DB_POLL_FAILURE_LIMIT {
                        give_up_after_db_failures(&cx, tally.session_id.as_deref(), cancel_read_failures)?;
                        // Already settled.
                        return Ok(None);
                    }
                }
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
            ctrl,
            adapter.as_ref(),
            &base.request(launch_prompt, &session_id, resume_session.as_deref()),
            &LaunchFiles { stderr_path: &launch_stderr, run_dir: &run_dir, long_prompt_file },
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

    Ok(Some(Launched {
        stream,
        tally,
        exit_code,
        kill,
        kill_error,
        drain_timed_out,
        agy_print_timeout,
        watch,
        retries,
        retry_stop,
        retry_error,
        evidence_from_launches,
    }))
}
