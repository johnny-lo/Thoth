use crate::adapter::{CommandPlan, HarnessAdapter, LaunchRequest, RunPlace};
use crate::config::Paths;
use crate::db::state::{self, RunFinish};
use crate::ids;
use crate::model::{RunStatus, TaskStatus};
use crate::process::{self, ChildSpec};
use super::outcome::{KillReason, StreamTally};
use super::settle::{log_runner_error, settle_terminal, TaskStatusWrite};
use super::stream::{handle_line, StreamState};
use super::{heartbeat_every, poll_every, InboxWatch, DB_POLL_FAILURE_LIMIT, KILL_WAIT_GRACE, STREAM_DRAIN_GRACE};
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// What every launch of one run shares (FC-2): only the prompt and the
/// session differ between the first launch and a retry.
pub(super) struct LaunchBase<'a> {
    pub(super) model: &'a str,
    pub(super) allowed_tools: &'a str,
    pub(super) config_dir: &'a str,
    pub(super) cwd: PathBuf,
    pub(super) settings_path: Option<PathBuf>,
    pub(super) extra_env: Vec<(String, String)>,
    pub(super) review_output: Option<crate::adapter::ReviewOutput>,
    pub(super) timeout_seconds: i64,
    pub(super) effort: Option<String>,
    pub(super) research: bool,
}

impl LaunchBase<'_> {
    pub(super) fn request<'b>(
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
pub(super) struct Spawned {
    pub(super) child: std::process::Child,
    pub(super) worker_pid: u32,
    pub(super) worker_image: Option<String>,
    pub(super) stdout: std::process::ChildStdout,
}

/// The run's own files one launch writes or reads.
pub(super) struct LaunchFiles<'a> {
    pub(super) stderr_path: &'a Path,
    pub(super) run_dir: &'a Path,
    /// SMALL-16: where, relative to the worktree, a prompt that would push
    /// the command line past the cap is written instead; `None` for a
    /// harness that takes its prompt on stdin.
    pub(super) long_prompt_file: Option<&'a str>,
}

/// SMALL-16: agy takes its prompt on the command line and has no prompt-file
/// or stdin flag a plain prompt could use. When the command line with the
/// full prompt is over thoth's cap, the full prompt is written, byte for
/// byte, to `relative` in the worktree and the `-p` value becomes the short
/// sentence naming it. Under the cap, or without a `-p PROMPT` pair, the plan
/// stays as it is (and any other long argument still fails the cap check).
fn move_long_prompt_to_file(plan: &mut CommandPlan, prompt: &str, relative: &str) -> Result<()> {
    if process::command_line_len(&plan.program, &plan.args) <= process::COMMAND_LINE_CAP {
        return Ok(());
    }
    let Some(at) = plan
        .args
        .iter()
        .position(|a| a == "-p")
        .map(|i| i + 1)
        .filter(|&i| plan.args.get(i).is_some_and(|a| a == prompt))
    else {
        return Ok(());
    };
    let path = plan.cwd.join(relative);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("could not create {}: {e}", dir.display()))?;
    }
    std::fs::write(&path, prompt.as_bytes())
        .map_err(|e| anyhow::anyhow!("could not write the long prompt to {}: {e}", path.display()))?;
    plan.args[at] = crate::adapter::antigravity::prompt_file_instruction(relative);
    Ok(())
}

/// Builds one launch's command, then resolves, checks and spawns it, feeds
/// its stdin and reads the image its pid runs.
pub(super) fn spawn_attempt(
    ctrl: &dyn process::ProcessController,
    adapter: &dyn HarnessAdapter,
    req: &LaunchRequest<'_>,
    files: &LaunchFiles<'_>,
    is_codex: bool,
    masker: &crate::redact::Masker,
) -> Result<Spawned> {
    let (stderr_path, run_dir) = (files.stderr_path, files.run_dir);
    let mut plan = adapter.build_command(req);
    if let Some(relative) = files.long_prompt_file {
        move_long_prompt_to_file(&mut plan, &req.prompt, relative)?;
    }
    // Windows: an npm `.cmd` shim becomes `node.exe <entry.js> ...`; never
    // run through cmd.exe. A failure here settles the run before any spawn.
    let (program, launch_args) = process::resolve_harness_command(&plan.program, &plan.args)?;
    // W3: refuse a command line Windows would reject (os error 206) with a
    // message that says why, before anything is spawned.
    process::check_command_line(&program, &launch_args)?;
    // RD-2 (H15): the worker never gets Thoth's own keys, nor any variable the
    // masker takes for a secret; the runner keeps them to mask with. RD-4:
    // nor a copy of a known value under another name, secrets.json's
    // included, which is why this is the run's masker. GITENV-1: git's
    // GIT_CONFIG_COUNT/KEY_n/VALUE_n group is listed whole or not at all.
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
pub(super) struct AttemptCtx<'a> {
    pub(super) paths: &'a Paths,
    pub(super) conn: &'a Connection,
    pub(super) gconn: &'a Connection,
    pub(super) repo_id: &'a str,
    pub(super) run: &'a state::Run,
    pub(super) harness: &'a str,
    pub(super) run_dir: &'a Path,
    pub(super) stream_path: &'a Path,
    pub(super) is_codex: bool,
    pub(super) is_review: bool,
    /// A worker or source-check reviewer of a research task (`LaunchRequest::research`).
    pub(super) research: bool,
    /// Taken once, after the first spawn: every launch's wall-clock check,
    /// and every retry wait, counts against the run's own `--timeout`.
    pub(super) started: Instant,
    pub(super) ctrl: &'a dyn process::ProcessController,
}

/// How one launch ended.
// One per launch, at most three per run: the size of `Exited` does not matter.
#[allow(clippy::large_enum_variant)]
pub(super) enum AttemptEnd<'a> {
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

/// M6: the DB itself is unreachable or erroring `DB_POLL_FAILURE_LIMIT` times
/// in a row. Records the run's quota and settles the run `failed` (the task
/// too, unless the run is a review). Shared by the launch's 1-second loop and
/// the FC-2 retry wait, so both give up the same way. The caller has already
/// stopped the worker (if one is alive) and returns the early end.
pub(super) fn give_up_after_db_failures(
    cx: &AttemptCtx<'_>,
    session_id: Option<&str>,
    failures: u32,
) -> Result<()> {
    let (conn, run, run_dir, is_review) = (cx.conn, cx.run, cx.run_dir, cx.is_review);
    let ended_at = ids::now_rfc3339();
    let note = format!(
        "runner gave up after {failures} consecutive heartbeat/cancel-check DB failures"
    );
    log_runner_error(run_dir, &note);
    crate::run_quota::record(
        conn,
        cx.gconn,
        cx.paths,
        cx.repo_id,
        run,
        cx.harness,
        session_id,
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
    Ok(())
}

/// Reads one launch's stream until the child has exited and the stream is
/// drained: the 1-second loop (cancel, timeout, heartbeat, the inbox), the
/// two early settles, the job release, the bounded drain, and the launch
/// hooks' last settle.
pub(super) fn watch_attempt<'a>(
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
    // `HEARTBEAT_EVERY`, or the debug-only test switch's beat (TEST-7b).
    let beat = heartbeat_every();
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
        if last_heartbeat.elapsed() >= beat {
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
            give_up_after_db_failures(cx, tally.session_id.as_deref(), db_poll_failures)?;
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
        std::thread::sleep(poll_every());
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
        if last_heartbeat.elapsed() >= beat {
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
