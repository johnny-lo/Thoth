//! `thoth review start|show` — the independent reviewer (v6 §4, brief §8).
//!
//! A review is just another run: `role = reviewer`, its own profile and model,
//! its own `run_dir`. What makes it independent is the workspace (a detached,
//! read-only checkout of the candidate commit), the tool list (no `Write`, no
//! `Edit`), and the fact that it reads the implementer's evidence rather than
//! the implementer's summary.

use crate::config::Paths;
use crate::db;
use crate::db::global;
use crate::db::state::{self, NewEvent, Run};
use crate::evidence;
use crate::ids;
use crate::ingest::IngestOutcome;
use crate::lang::Lang;
use crate::model::{event_type, role, RunStatus, TaskStatus};
use crate::output;
use crate::plan_label;
use crate::review_json::{self, Verdict};
use crate::spec;
use crate::worktree;
use anyhow::{bail, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// brief §15: the reviewer may read and run, never edit. `Bash` stays in
/// because the reviewer needs it for `thoth evidence show` and, since there
/// is no `Write`, to create `.agent/review.json` with a redirect (Open question
/// 16). The guarantee that matters - it cannot change the code under review -
/// comes from the detached worktree plus this list together.
///
/// A codex reviewer is recorded with the same list but does not get it: its
/// tools are fixed by the read-only sandbox and the disabled features (scope
/// §2.2, §2.4). An adapter may record its own list instead
/// (`HarnessAdapter::reviewer_allowed_tools`).
pub const REVIEWER_ALLOWED_TOOLS: &str = "Bash,Read,Glob,Grep";

/// `worktrees/<task_id>-a<attempt>-review`.
pub fn review_worktree_path(
    paths: &Paths,
    repo_id: &str,
    task_id: &str,
    attempt: i64,
) -> PathBuf {
    paths
        .worktrees_dir(repo_id)
        .join(format!("{task_id}-a{attempt}-review"))
}

pub struct StartArgs<'a> {
    pub task: &'a str,
    /// A profile id (`auto` is refused, RJ-1).
    pub profile: &'a str,
    /// `--model` as typed; `None` keeps the model of the previous reviewer
    /// run of the attempt under review, and is refused when there is none.
    pub model: Option<&'a str>,
    pub timeout_seconds: u64,
    /// `--effort`, as typed; validated by `start` before anything is written.
    pub effort: Option<&'a str>,
    /// `--reason`, as typed (DT-2); trimmed and checked by `start`.
    pub reason: Option<&'a str>,
    pub json: bool,
}

/// Which implementer run a reviewer run was started against, read back out of
/// its own `review_started` event.
///
/// Recomputing "the newest completed implementer run" in the runner would drift
/// if another implementer run completed while the review was in flight; the
/// event is a fact about this review.
pub fn target_run_id_for(conn: &Connection, reviewer_run_id: &str) -> Result<Option<String>> {
    let detail: Option<String> = conn
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            rusqlite::params![reviewer_run_id, event_type::REVIEW_STARTED],
            |r| r.get(0),
        )
        .optional()?;
    Ok(detail
        .and_then(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
        .and_then(|v| v["target_run_id"].as_str().map(|s| s.to_string())))
}

/// Read the latest accepted delivery for this exact target, including manual
/// resubmissions. Never substitute the task's other attempts.
pub fn target_result(conn: &Connection, target_run_id: &str) -> Result<serde_json::Value> {
    let detail: Option<String> = conn.query_row(
        "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
        rusqlite::params![target_run_id, event_type::RESULT_INGESTED],
        |r| r.get(0),
    ).optional()?;
    Ok(detail.and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .map(|d| d["result_json"].clone()).unwrap_or(serde_json::Value::Null))
}

/// Which evidence codes a reviewer run actually opened, from its
/// `evidence_opened` events (v6 §2.3, and the honesty metric in §13).
pub fn opened_codes(conn: &Connection, reviewer_run_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq",
    )?;
    let details = stmt
        .query_map(
            rusqlite::params![reviewer_run_id, event_type::EVIDENCE_OPENED],
            |r| r.get::<_, String>(0),
        )?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    let mut out: Vec<String> = Vec::new();
    for d in details {
        if let Some(code) = serde_json::from_str::<serde_json::Value>(&d)
            .ok()
            .and_then(|v| v["code"].as_str().map(|s| s.to_string()))
        {
            if !out.contains(&code) {
                out.push(code);
            }
        }
    }
    Ok(out)
}

/// Records the target run's candidate diff the way the runner does: from the
/// run's base, with `protected_changes` attributed to the task's base and the
/// main worktree's HEAD (`evidence::Attribution`), so a rework run lists what
/// the earlier rounds changed too. `root` is the main work tree; it shares
/// the object store with every worktree.
fn record_target_diff(
    conn: &Connection,
    paths: &Paths,
    repo_id: &str,
    task_base: &str,
    target: &Run,
    candidate: &str,
    root: &Path,
) -> Result<String> {
    let tip = worktree::rev_parse_head(root).ok();
    let ctx = evidence::EvidenceContext {
        paths,
        repo_id,
        task_id: &target.task_id,
        // Attributed to the implementer run whose candidate this is, not to the
        // review: the diff is evidence about the implementation.
        run_id: &target.run_id,
    };
    match tip {
        // Nothing to attribute: the run starts at the task's base and there is
        // no tip to compare with, so this is the plain diff.
        None if task_base == target.base_commit => {
            evidence::record_diff(conn, &ctx, root, &target.base_commit, candidate)
        }
        _ => evidence::record_diff_attributed(
            conn,
            &ctx,
            root,
            &target.base_commit,
            candidate,
            Some(&evidence::Attribution { task_base, integration_tip: tip.as_deref() }),
        ),
    }
}

/// Makes the target's protected changes checkable before the gate reads
/// them: `ensure_candidate_evidence`, then, when the newest diff evidence has
/// no readable `protected_changes` (recorded before the key existed, or
/// damaged), one more diff recorded with git. Returns the warnings and the
/// error of that second recording, if it failed. Nothing is refused here.
fn prepare_protected_evidence(
    conn: &Connection,
    paths: &Paths,
    repo_id: &str,
    task_base: &str,
    target: &Run,
    candidate: &str,
    root: &Path,
) -> (Vec<String>, Option<String>) {
    let warnings =
        ensure_candidate_evidence(conn, paths, repo_id, task_base, target, candidate, root);
    let unreadable = matches!(
        crate::protected::latest_diff(conn, target),
        Ok(Some(ref d)) if d.changes.is_none()
    );
    let again = unreadable
        .then(|| record_target_diff(conn, paths, repo_id, task_base, target, candidate, root).err())
        .flatten()
        .map(|e| format!("{e:#}"));
    (warnings, again)
}

/// Why a PASS cannot be reached yet for `target`'s candidate, or `None`: no
/// diff evidence, a diff evidence whose `protected_changes` cannot be read,
/// or a protected test or check-word change nobody approved with `checks
/// approve --run`. `notes` and `again_error` (from
/// `prepare_protected_evidence`) are added to the first two refusals.
fn protected_refusal(
    conn: &Connection,
    task_id: &str,
    target: &Run,
    notes: &[String],
    again_error: Option<&str>,
) -> Result<Option<String>> {
    let run_id = &target.run_id;
    let Some(latest) = crate::protected::latest_diff(conn, target)? else {
        let notes: String = notes.iter().map(|n| format!("; {n}")).collect();
        return Ok(Some(format!(
            "run {run_id} has no diff evidence for its candidate, so its protected file changes \
             cannot be checked{notes}"
        )));
    };
    let Some(changes) = latest.changes else {
        return Ok(Some(format!(
            "run {run_id}'s diff evidence {} has no readable protected_changes, and recording it \
             again failed{}",
            latest.code,
            again_error.map(|e| format!("; {e}")).unwrap_or_default()
        )));
    };
    let left = crate::protected::uncovered(
        &changes,
        &crate::protected::approved_pairs(conn, run_id)?,
    );
    if left.is_empty() {
        return Ok(None);
    }
    let listed: Vec<String> =
        left.iter().map(|c| format!("{} ({}, {})", c.path, c.status, c.kind)).collect();
    Ok(Some(format!(
        "run {run_id} changed protected files that nobody has approved yet: {}; read the change \
         (thoth review packet {task_id}), then approve it with: thoth checks approve --run \
         {run_id} --reason \"WHY\" (MCP: thoth_checks_approve)",
        listed.join(", ")
    )))
}

/// Makes sure the candidate's `diff` and `commit` evidence exist.
///
/// brief §8: the runner adds both when an implementer run ends, but a task that
/// reached `in_review` through `thoth submit` never went through that tail,
/// and a runner that failed to record the diff only logged it - and the diff
/// is the one thing v6 §4 says the reviewer must read, and what the protected
/// file gate reads. Returns warnings rather than failing; the gate
/// (`protected_refusal`) decides what a missing diff means.
fn ensure_candidate_evidence(
    conn: &Connection,
    paths: &Paths,
    repo_id: &str,
    task_base: &str,
    target: &Run,
    candidate: &str,
    root: &Path,
) -> Vec<String> {
    let mut warnings: Vec<String> = Vec::new();
    // Fix round 1 (R1): a failed read here used to fall back to "this task has
    // no evidence yet" silently, which makes the two recorders below write a
    // second copy of rows that already exist. The fallback stays (a duplicate
    // row is worse than a missing one, but neither is worth refusing the review
    // over); what changes is that it now says so.
    let existing = match state::list_evidence_for_task(conn, &target.task_id) {
        Ok(rows) => rows,
        Err(e) => {
            warnings.push(format!(
                "could not read task {}'s existing evidence ({e}); the candidate diff and \
                 commit may be recorded a second time",
                target.task_id
            ));
            Vec::new()
        }
    };
    let matches_field = |row: &state::Evidence, kind: &str, field: &str| -> bool {
        row.kind == kind
            && serde_json::from_str::<serde_json::Value>(&row.detail)
                .ok()
                .and_then(|d| d[field].as_str().map(|s| s == candidate))
                .unwrap_or(false)
    };
    if !existing.iter().any(|e| matches_field(e, "diff", "to_commit")) {
        if let Err(e) = record_target_diff(conn, paths, repo_id, task_base, target, candidate, root)
        {
            warnings.push(format!("could not record the candidate diff: {e:#}"));
        }
    }
    if !existing.iter().any(|e| matches_field(e, "commit", "sha")) {
        let ctx = evidence::EvidenceContext {
            paths,
            repo_id,
            task_id: &target.task_id,
            run_id: &target.run_id,
        };
        if let Err(e) = evidence::record_commit(conn, &ctx, root, candidate) {
            warnings.push(format!("could not record the candidate commit: {e:#}"));
        }
    }
    warnings
}

pub fn start(paths: &Paths, cwd: &Path, args: StartArgs<'_>) -> Result<i32> {
    use crate::commands::auto_pick;
    // RJ-1: `auto` is refused before anything else.
    auto_pick::refuse_auto(role::REVIEWER, args.task, args.profile, args.model, args.effort)?;
    // S3-7: reject a bad level before a run row, worktree or event exists.
    let effort_given = args
        .effort
        .map(crate::adapter::effort::parse_level)
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?;
    let reason = auto_pick::clean_reason(args.reason)?;
    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, args.task)?;
    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    if task.status != TaskStatus::InReview.as_str() {
        bail!(
            "task {task_id} is '{}'; review start needs in_review",
            task.status
        );
    }

    // Fix round 1 (I3): the H2 rule keeps the task at `in_review` for the whole
    // review, which removes the mutex every other launch path leans on -
    // `dispatch` is refused by the task being `running`, but a review leaves it
    // exactly where a second `review start` would accept it. Two reviewers
    // would then share one worktree and one `.agent/review.json`, each runner
    // ingesting whatever file happened to be on disk: two `reviews` rows, two
    // `verdict` events, and a task status decided by whichever settled last.
    // Refused here rather than fixed by re-keying the worktree, because
    // `review_worktree_path`'s (task_id, attempt) key is what Task 8 consumes.
    //
    // Important 3 (whole-branch review): this check on its own is a
    // check-then-act - the run row it would collide with is not inserted
    // until the transaction near the bottom of this function, and `probe_auth`
    // (which spawns a child) and `ensure_detached_worktree` (a `git worktree
    // add`) both sit in between, seconds of it. It stays as the cheap
    // pre-check, because refusing here is what keeps a refused `review start`
    // from ever reaching those side effects (the one with teeth, deleting the
    // stale report, moved into the transaction instead); the
    // authoritative check is the identical one inside `write_tx` below. It
    // also widened from "a live reviewer run" to "any live run of the task",
    // because an implementer run in flight is just as good a reason not to
    // start reviewing a candidate that is still moving.
    if let Some(live) = state::list_live_runs_for_task(&conn, &task_id)?.into_iter().next() {
        bail!(
            "{}",
            crate::commands::live_run_refusal(
                &format!("refusing to start a review of task {task_id}"),
                &task_id,
                &live,
            )
        );
    }

    let target = state::latest_completed_run_for_role(&conn, &task_id, role::IMPLEMENTER)?
        .ok_or_else(|| {
            anyhow::anyhow!("task {task_id} has no completed implementer run to review")
        })?;
    let research = spec::is_research_task(&task.spec);
    if research && !target_result(&conn, &target.run_id)?["claims"].as_array()
        .is_some_and(|claims| claims.iter().any(|c| c["kind"] == "research"))
    {
        bail!("task {task_id} has no research claim to check; inspect `thoth task result {task_id}`, then use `thoth task close` or `thoth dispatch` as appropriate");
    }
    let Some(candidate) = target.candidate_commit.clone() else {
        bail!(
            "run {} has no candidate_commit, so there is nothing to check out for review",
            target.run_id
        );
    };

    // DT-2: the newest reviewer run of the attempt under review, whose model
    // and effort a left-out `--model` / `--effort` keeps (RJ-1).
    let previous_run = state::list_runs_for_task(&conn, &task_id)?
        .into_iter()
        .rfind(|r| r.attempt == target.attempt && r.role == role::REVIEWER);

    let gconn = db::open_global(paths)?;
    let profile = match global::get_profile(&gconn, args.profile)? {
        Some(p) => p,
        None => bail!("no such profile: {}", args.profile),
    };
    crate::commands::profile::require_supported_harness(
        &format!("profile {}", profile.profile_id),
        &profile.harness,
    )?;
    // Scope §2.4: a codex reviewer takes a codex model, like a codex
    // implementer.
    let adapter = crate::adapter::for_harness(&profile.harness);
    // MODELS-1: what a model-rule refusal lists (the stored list).
    let known_models = crate::model_list::known_models_text(&gconn, &profile);
    // RJ-1: the model and effort the commander gave, or the previous
    // reviewer run's. Every check writes nothing.
    let choice = auto_pick::choose(
        &gconn,
        &auto_pick::Request {
            task_id: &task_id,
            role: role::REVIEWER,
            model: args.model,
            effort: effort_given,
            previous: previous_run.as_ref(),
            retry: auto_pick::Retry::KeepsPrevious,
        },
        &profile,
        |m| adapter.check_model(&profile.profile_id, m, &known_models),
    )?;
    let require_reason = crate::commands::settings::require_reason(&paths.home);
    auto_pick::require_reason_gate(&task_id, "review start", reason.as_deref(), require_reason)?;

    // LESS-5: the protected test files the candidate changed must have been
    // approved (`checks approve --run`) before a review starts. The candidate's
    // evidence is recorded first, so a run whose diff the runner failed to
    // record, or that came in through `thoth submit`, is still judged; when
    // it cannot be, the review is refused. After the launch's own refusals,
    // which write nothing: this one writes nothing but that evidence.
    let (mut warnings, again_error) =
        prepare_protected_evidence(&conn, paths, &repo_id, &task.base_commit, &target, &candidate, &root);
    if let Some(why) = protected_refusal(&conn, &task_id, &target, &warnings, again_error.as_deref())? {
        bail!("refusing to start a review of task {task_id}: {why}");
    }

    // FC-1 (H4): read only to see whether the profile has recovered or is
    // parked for another quota group; an unreadable ranking does not stop
    // the review.
    let ranked = crate::commands::usage::ranked_candidates(
        paths,
        role::REVIEWER,
        Some(&task_id),
        Some(&choice.model),
    )
    .unwrap_or_default();
    let recovered = ranked
        .iter()
        .find(|c| c.profile.profile_id == profile.profile_id)
        .is_some_and(|c| c.recovered && c.profile.status == "rate_limited");
    let allowed_tools = adapter.reviewer_allowed_tools(REVIEWER_ALLOWED_TOOLS, research);
    let launch = auto_pick::launch_settings(&choice, &profile);
    let run_spec = crate::adapter::effort::with_launch(&task.spec, &launch)?;
    // Same pre-flight as `dispatch`: do not burn a worktree on a profile that
    // is not logged in.
    let (_probed, auth) = crate::commands::profile::probe_auth(paths, &profile.profile_id)?;
    if !auth.logged_in {
        bail!(
            "profile {} failed its auth check; run `thoth profile check {}`; to log in again, run \
             `thoth profile login {}`",
            profile.profile_id,
            profile.profile_id,
            profile.profile_id
        );
    }
    plan_label::remember_or_warn(&gconn, &profile.profile_id, auth.subscription_type.as_deref());
    // MODELS-1: store the list just read, then check the model against it
    // (agy) or against the account's recorded unavailable models (Claude),
    // before the worktree and the run row.
    let fresh_models = crate::model_list::remember_or_warn(&gconn, &profile.profile_id, &auth);
    warnings.extend(crate::model_list::check_launch(
        &gconn,
        &profile,
        &choice.model,
        fresh_models.as_ref(),
    )?);
    // FC-1: the auth check just passed, so a `logged_out` profile is marked
    // active again once the run row exists (below).
    let logged_in_again = profile.status == "logged_out";

    let wt = review_worktree_path(paths, &repo_id, &task_id, target.attempt);
    let ensured = worktree::ensure_detached_worktree(&root, &wt, &candidate)?;
    let outcome = ensured.outcome;
    let worktree_warning = ensured.warning;

    // Where a previous reviewer's report would be if it crashed after writing
    // one. Only named here, because the path needs `wt`; it is deleted in the
    // transaction below, under the authoritative live-run check - Important 3
    // (whole-branch review): a `review start` that is going to be refused must
    // not delete a live reviewer's report on its way out.
    let stale_report = wt.join(".agent").join("review.json");

    let run_id = ids::new_run_id();
    let run_dir = paths.run_dir(&repo_id, &run_id);
    std::fs::create_dir_all(&run_dir)?;
    let run = Run {
        run_id: run_id.clone(),
        task_id: task_id.clone(),
        // The review belongs to the attempt it judges, so `reject` → attempt+1
        // still reads the right findings.
        attempt: target.attempt,
        role: role::REVIEWER.to_string(),
        provider: profile.provider.clone(),
        account: profile.profile_id.clone(),
        model: choice.model.clone(),
        // Codex names its own thread, recorded from `thread.started`; so
        // does any harness that names its session late.
        harness_session_id: if adapter.names_session_late() {
            None
        } else {
            Some(ids::new_session_uuid())
        },
        config_dir: profile.config_dir.clone(),
        allowed_tools: allowed_tools.clone(),
        worktree: wt.display().to_string(),
        // The commit under review is this run's own base; a reviewer that
        // produced a different candidate would be a bug, and this makes that
        // visible in `status`.
        base_commit: candidate.clone(),
        candidate_commit: None,
        spec: run_spec,
        status: RunStatus::Queued.as_str().to_string(),
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
        timeout_seconds: args.timeout_seconds as i64,
        run_dir: run_dir.display().to_string(),
        error_text: None,
        harness_bin: Some(crate::adapter::harness_bin(&profile.harness)),
    };

    // One transaction for the run row and the event that names it, the same way
    // `dispatch` does it, so a reader walking `events` can always look the run
    // up. `spawn_runner` stays outside: launching a process is not something a
    // rollback can undo.
    //
    // Important 3 (whole-branch review): `db::write_tx` holds SQLite's write
    // lock from `BEGIN`, so the live-run check below is authoritative rather
    // than a snapshot a concurrent `review start`/`dispatch` can commit past.
    // Two `review start` a second apart used to both clear the pre-check at
    // the top of this function and both insert a run row, sharing one
    // worktree and one `.agent/review.json`.
    let tx = db::write_tx(&conn)?;
    if let Some(live) = state::list_live_runs_for_task(&tx, &task_id)?.into_iter().next() {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "{}",
            crate::commands::live_run_refusal(
                &format!("refusing to start a review of task {task_id}"),
                &task_id,
                &live,
            )
        );
    }
    // Fix round 1 (I3's related check), moved under the guard above: the
    // review worktree is reused across retries of the same (task_id,
    // attempt) and nothing else removes this file, so a report written by a
    // reviewer that then crashed would still be on disk when the next one is
    // spawned - and the runner would ingest it as the new reviewer's verdict,
    // from a process that never wrote a word. Cleared before the run row is
    // committed, so a failure here leaves nothing behind: the `bail!` rolls
    // the transaction back. Fails closed: a stale report that cannot be
    // removed is a reason not to start a review, not a warning to file after
    // starting one.
    let cleared_stale_report = match std::fs::remove_file(&stale_report) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            drop(tx); // explicit: rolls back before anything was inserted
            bail!(
                "could not clear the previous review report at {}: {e}; refusing to start a \
                 review that could ingest it",
                stale_report.display()
            );
        }
    };
    state::insert_run(&tx, &run)?;
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: Some(run_id.clone()),
            event_type: event_type::REVIEW_STARTED.to_string(),
            detail: serde_json::json!({
                "target_run_id": target.run_id,
                "attempt": target.attempt,
                "candidate_commit": candidate,
                "worktree": wt.display().to_string(),
                "worktree_reused": outcome == worktree::EnsureOutcome::Reused,
                "cleared_stale_report": cleared_stale_report,
                "profile": profile.profile_id,
                "model": choice.model,
                "allowed_tools": allowed_tools,
                "timeout_seconds": args.timeout_seconds,
            }),
        },
    )?;
    // RJ-1: what was launched and why.
    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: Some(run_id.clone()),
            event_type: event_type::LAUNCH_CHOICE.to_string(),
            detail: auto_pick::LaunchChoice {
                role: role::REVIEWER,
                source: auto_pick::SOURCE_REVIEW_START,
                task_spec: &task.spec,
                risk: &task.risk,
                profile: &profile.profile_id,
                choice: &choice,
                launch: &launch,
                previous: previous_run.as_ref(),
                reason: reason.as_deref(),
                require_reason,
            }
            .detail(),
        },
    )?;
    tx.commit()?;

    warnings.extend(worktree_warning);
    if let Some(note) = launch.effort_note.clone() {
        warnings.push(note);
    }
    warnings.extend(choice.warning());
    // Same as `dispatch` (AC4): a recovered profile goes back to `active`
    // once its run row exists, before the runner starts.
    if recovered {
        if let Err(e) = auto_pick::mark_recovered(&gconn, &profile.profile_id, &task_id, &run_id) {
            warnings.push(format!(
                "profile {} was picked because its window reset, but marking it active again \
                 failed: {e}; run `thoth profile check {}`",
                profile.profile_id, profile.profile_id
            ));
        }
    }
    // FC-1: a `logged_out` profile whose auth check passed, likewise.
    if logged_in_again {
        if let Err(e) = auto_pick::mark_logged_in_again(
            &gconn,
            &profile.profile_id,
            &task_id,
            &run_id,
            auto_pick::SOURCE_REVIEW_START,
        ) {
            warnings.push(format!(
                "profile {} passed its auth check, but marking it active again failed: {e}; run \
                 `thoth profile check {}`",
                profile.profile_id, profile.profile_id
            ));
        }
    }
    // The reviewer reads the index from its own worktree, so publish it there
    // before the process starts.
    if let Err(e) = evidence::refresh_index(&conn, &task_id, &wt) {
        warnings.push(format!(
            "could not publish the evidence index into the review worktree: {e:#}"
        ));
    }

    let runner_pid = crate::commands::dispatch::spawn_runner(&root, &run_id)?;
    // Same best-effort pid bookkeeping as `dispatch` (its I2 ruling): the
    // reviewer is already live, so a failed write here is a warning, not a
    // failed command.
    if state::set_runner_pid(&conn, &run_id, runner_pid as i64).is_err() {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Err(e) = state::set_runner_pid(&conn, &run_id, runner_pid as i64) {
            warnings.push(format!(
                "reviewer started (pid {runner_pid}) but recording runner_pid failed: {e}; \
                 cancel escalation will need the pid manually"
            ));
        }
    }

    if !args.json {
        for w in &warnings {
            masked_eprintln!("warning: {w}");
        }
    }
    let v = serde_json::json!({
        "ok": true,
        "run_id": run_id,
        "task_id": task_id,
        "target_run_id": target.run_id,
        "attempt": target.attempt,
        "candidate_commit": candidate,
        "profile": profile.profile_id,
        "model": choice.model,
        "worktree": wt.display().to_string(),
        "worktree_reused": outcome == worktree::EnsureOutcome::Reused,
        "cleared_stale_report": cleared_stale_report,
        "allowed_tools": allowed_tools,
        "runner_pid": runner_pid,
        "run_dir": run_dir.display().to_string(),
        "effort_requested": launch.effort_requested,
        "effort": launch.effort,
        "effort_note": launch.effort_note,
        "warnings": warnings,
    });
    output::emit(
        args.json,
        &v,
        &format!(
            "review {run_id} started on {} (task {task_id}, candidate {}, profile {}, runner pid {runner_pid})\n\
             the task stays in_review until the verdict lands; block on it with \
             `thoth wait --run {run_id}`, or watch `thoth events`",
            choice.model,
            crate::evidence::short_sha(&candidate),
            plan_label::named(
                &profile.profile_id,
                plan_label::labels(&gconn).get(&profile.profile_id).map(String::as_str),
                Lang::En,
            )
        ),
    );
    Ok(0)
}

/// What a `review.json` ingest produced.
pub struct ReviewIngest {
    pub outcome: IngestOutcome,
    /// The status the task should settle on. `None` only when ingest was never
    /// attempted (the run crashed, was cancelled or timed out), in which case
    /// the runner's ordinary classification applies.
    pub verdict: Option<Verdict>,
    /// Codes the review cites that it never opened (brief §8). A warning only.
    pub unopened_citations: Vec<String>,
}

fn review_invalid(
    conn: &Connection,
    run: &Run,
    reason: String,
    mut detail: serde_json::Value,
) -> ReviewIngest {
    if let Some(o) = detail.as_object_mut() {
        o.insert(
            "reason".to_string(),
            serde_json::Value::String(reason.clone()),
        );
    }
    let _ = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::REVIEW_INVALID.to_string(),
            detail,
        },
    );
    ReviewIngest {
        outcome: IngestOutcome::Invalid { reason },
        // brief §5: a successful reviewer process whose report is missing or
        // invalid leaves the task `unverified`, never `pass`.
        verdict: Some(Verdict::Unverified),
        unopened_citations: Vec::new(),
    }
}

/// What `validate_review_document` found wrong: a human-readable reason and
/// the `review_invalid` / `verdict` event detail explaining which stage failed
/// (`schema` / `spec` / `coverage` / `citation`), *before* `reason` is merged
/// into it - both callers merge it in the same way, so it lives here rather
/// than in the error itself.
struct ReviewValidationError {
    reason: String,
    detail: serde_json::Value,
}

/// v6 §4/§8, thin-adapter Task 1: parse a `review.json` document, check it
/// covers exactly the task's acceptance criteria, and confirm every cited code
/// exists as evidence of `task_id`. Shared by the reviewer-run path
/// (`ingest_review`, below) and `thoth review record` (an external review
/// from a commander outside thoth), so both are held to identical rules -
/// same stage names, same messages - and the reviewer-run path's existing
/// tests keep exercising this logic unedited.
fn validate_review_document(
    conn: &Connection,
    task_id: &str,
    task: &state::Task,
    target_run_id: &str,
    json_text: &str,
) -> Result<(review_json::ReviewDocument, Vec<String>), ReviewValidationError> {
    let doc = if spec::is_research_task(&task.spec) {
        if target_run_id.is_empty() {
            return Err(ReviewValidationError {
                reason: "the review's target run could not be determined".to_string(),
                detail: serde_json::json!({"stage": "target"}),
            });
        }
        let result = target_result(conn, target_run_id).map_err(|e| ReviewValidationError {
            reason: format!("reading the target result failed: {e}"),
            detail: serde_json::json!({"stage": "target"}),
        })?;
        let expected = review_json::sources_to_check(&result);
        review_json::parse_source_check(json_text, &expected).map_err(|e| {
            let stage = match &e {
                review_json::ReviewError::MissingSource(_)
                | review_json::ReviewError::UnknownSource(_)
                | review_json::ReviewError::DuplicateSource(_) => "coverage",
                review_json::ReviewError::SourceConsistency { .. } => "consistency",
                _ => "schema",
            };
            ReviewValidationError {
                reason: e.to_string(), detail: serde_json::json!({"stage": stage}),
            }
        })?
    } else {
        let doc = review_json::parse_review(json_text).map_err(|e| ReviewValidationError {
            reason: e.to_string(),
            detail: serde_json::json!({"stage": "schema"}),
        })?;
        let parsed_spec = spec::parse_spec(&task.spec).map_err(|e| ReviewValidationError {
            reason: format!("the task spec does not parse, so its criteria cannot be checked: {e}"),
            detail: serde_json::json!({"stage": "spec"}),
        })?;
        let expected = review_json::ac_ids(&parsed_spec);
        review_json::check_coverage(&doc, &expected).map_err(|e| ReviewValidationError {
            reason: e.to_string(),
            detail: serde_json::json!({"stage": "coverage", "expected": expected}),
        })?;
        doc
    };

    // Every cited code must be evidence of this task - the same rule the
    // worker's citations get (v6 §2.5 rule 3). A reviewer citing a code that
    // does not exist is not a verdict either.
    let cited = review_json::cited_codes(&doc);
    for code in &cited {
        match state::get_evidence_by_code(conn, task_id, code) {
            Ok(Some(_)) => {}
            Ok(None) => {
                return Err(ReviewValidationError {
                    reason: format!("the review cites {code}, which is not evidence of task {task_id}"),
                    detail: serde_json::json!({"stage": "citation", "code": code}),
                })
            }
            Err(e) => {
                return Err(ReviewValidationError {
                    reason: format!("reading evidence {code} failed: {e}"),
                    detail: serde_json::json!({"stage": "citation", "code": code}),
                })
            }
        }
    }
    Ok((doc, cited))
}

/// thin-adapter Task 1 fix round 1 (F3): the `reviews` row shape, shared by
/// the reviewer-run path and `record` - only `target_run_id` and
/// `reviewer_run_id` differ between the two.
fn new_review_row(
    target_run_id: String,
    reviewer_run_id: Option<String>,
    verdict: Verdict,
    findings: &review_json::Findings,
) -> state::Review {
    state::Review {
        review_id: ids::new_review_id(),
        target_run_id,
        reviewer_run_id,
        verdict: verdict.as_str().to_string(),
        findings: serde_json::to_string(findings).unwrap_or_else(|_| "{}".to_string()),
        created_at: ids::now_rfc3339(),
    }
}

/// thin-adapter Task 1 fix round 1 (F3): the arguments `verdict_event_detail`
/// needs, bundled into one struct rather than eight positional parameters.
///
/// `opened`/`unopened` are `Some` for a reviewer run (computed from its own
/// `evidence_opened` events) and `None` for an external review (there is no
/// run to have opened anything through `evidence show`) - serialising as
/// `null` either way. `source` is `None` for the reviewer-run path, which
/// must produce exactly the same keys and values it always has (its existing
/// tests guard this byte for byte); passing `Some("external")` is what adds
/// the two keys `record` needs on top - `reviewer_run_id: null` and
/// `source: "external"` - without touching the reviewer-run path's shape at
/// all.
struct VerdictEventShape<'a> {
    verdict: Verdict,
    review_id: &'a str,
    target_run_id: &'a str,
    doc: &'a review_json::ReviewDocument,
    cited: &'a [String],
    opened: Option<&'a [String]>,
    unopened: Option<&'a [String]>,
    source: Option<&'a str>,
}

/// The `verdict` event's `detail`, shared by the reviewer-run path and
/// `record`. See `VerdictEventShape` for what each field means.
fn verdict_event_detail(shape: VerdictEventShape<'_>) -> serde_json::Value {
    let VerdictEventShape {
        verdict,
        review_id,
        target_run_id,
        doc,
        cited,
        opened,
        unopened,
        source,
    } = shape;
    let mut detail = serde_json::json!({
        "verdict": verdict.as_str(),
        "review_id": review_id,
        "target_run_id": target_run_id,
        "criteria": doc.findings.criteria.iter().map(|c| serde_json::json!({
            "id": c.id, "verdict": c.verdict, "evidence": c.evidence, "note": c.note,
        })).collect::<Vec<_>>(),
        "extra_findings": doc.findings.extra_findings.len(),
        "cited": cited,
        "opened": opened,
        "unopened_citation": unopened,
    });
    if !doc.findings.sources.is_empty() {
        detail["sources"] = serde_json::json!(doc.findings.sources);
    }
    if let Some(source) = source {
        if let Some(o) = detail.as_object_mut() {
            o.insert("reviewer_run_id".to_string(), serde_json::Value::Null);
            o.insert("source".to_string(), serde_json::Value::String(source.to_string()));
        }
    }
    detail
}

/// brief §8: validate `.agent/review.json`, write the `reviews` row and the
/// `verdict` event, and report which status the task should settle on.
pub fn ingest_review(
    conn: &Connection,
    run: &Run,
    task: &state::Task,
    target_run_id: &str,
    json_text: &str,
) -> ReviewIngest {
    let (doc, cited) = match validate_review_document(conn, &run.task_id, task, target_run_id, json_text) {
        Ok(v) => v,
        Err(e) => return review_invalid(conn, run, e.reason, e.detail),
    };

    // The runner passes an empty target when neither the review's own
    // `review_started` event nor a completed implementer run names one.
    if target_run_id.is_empty() {
        return review_invalid(
            conn,
            run,
            "the review's target run could not be determined".to_string(),
            serde_json::json!({"stage": "target"}),
        );
    }

    let verdict = Verdict::parse(&doc.verdict).unwrap_or(Verdict::Unverified);
    let opened = opened_codes(conn, &run.run_id).unwrap_or_default();
    let unopened: Vec<String> = cited
        .iter()
        .filter(|c| !opened.contains(c))
        .cloned()
        .collect();

    let review = new_review_row(
        target_run_id.to_string(),
        Some(run.run_id.clone()),
        verdict,
        &doc.findings,
    );

    let tx = match conn.unchecked_transaction() {
        Ok(t) => t,
        Err(e) => {
            return review_invalid(
                conn,
                run,
                format!("starting the review transaction failed: {e}"),
                serde_json::json!({"stage": "write"}),
            )
        }
    };
    if let Err(e) = state::insert_review(&tx, &review) {
        drop(tx);
        return review_invalid(
            conn,
            run,
            format!("writing the review row failed: {e}"),
            serde_json::json!({"stage": "write"}),
        );
    }
    if let Err(e) = state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::VERDICT.to_string(),
            detail: verdict_event_detail(VerdictEventShape {
                verdict,
                review_id: &review.review_id,
                target_run_id,
                doc: &doc,
                cited: &cited,
                opened: Some(&opened),
                unopened: Some(&unopened),
                source: None,
            }),
        },
    ) {
        drop(tx);
        return review_invalid(
            conn,
            run,
            format!("recording the verdict event failed: {e}"),
            serde_json::json!({"stage": "write"}),
        );
    }
    if let Err(e) = tx.commit() {
        return review_invalid(
            conn,
            run,
            format!("committing the review failed: {e}"),
            serde_json::json!({"stage": "write"}),
        );
    }

    ReviewIngest {
        outcome: IngestOutcome::Ok {
            summary: if doc.findings.sources.is_empty() { format!(
                "{} over {} criteria",
                verdict.as_str(),
                doc.findings.criteria.len()
            ) } else { format!("{} over {} sources", verdict.as_str(), doc.findings.sources.len()) },
            // `claims` belong to the implementer; a review writes none.
            claim_count: 0,
        },
        verdict: Some(verdict),
        unopened_citations: unopened,
    }
}

pub fn ingest_review_path(
    conn: &Connection,
    run: &Run,
    task: &state::Task,
    target_run_id: &str,
    path: &Path,
) -> ReviewIngest {
    match std::fs::read_to_string(path) {
        Ok(text) => ingest_review(conn, run, task, target_run_id, &text),
        Err(e) => review_invalid(
            conn,
            run,
            format!("could not read review.json at {}: {e}", path.display()),
            serde_json::json!({"stage": "read"}),
        ),
    }
}

pub struct RecordArgs<'a> {
    pub task: &'a str,
    pub file: &'a Path,
    pub json: bool,
}

/// A single leading UTF-8 BOM, stripped before parsing. Stock Windows
/// PowerShell 5.1's `Set-Content -Encoding utf8` writes one by default
/// (`Out-File`/`>` write UTF-16LE instead - see `utf16_bom_kind`, which
/// refuses those before this function ever runs), and `serde_json` rejects a
/// UTF-8 BOM outright rather than skipping it the way most JSON readers do.
pub(crate) fn strip_bom(s: &str) -> &str {
    s.strip_prefix('\u{feff}').unwrap_or(s)
}

/// thin-adapter Task 1 fix round 1 (F6): stock Windows PowerShell 5.1's
/// `Out-File` and `>` write UTF-16LE, marked with the two-byte BOM `FF FE`
/// (`FE FF` is the same story for UTF-16BE, which nothing on Windows writes
/// by default but is just as easy to name). Neither leading byte is valid at
/// the start of a UTF-8 stream, so `String::from_utf8` on one of these files
/// fails - but with a generic "invalid utf-8" message that does not tell a
/// PowerShell user what actually happened or what to do about it. Checked
/// before `String::from_utf8` so the refusal names the real cause.
pub(crate) fn utf16_bom_kind(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        Some("UTF-16LE")
    } else if bytes.starts_with(&[0xFE, 0xFF]) {
        Some("UTF-16BE")
    } else {
        None
    }
}

/// thin-adapter Task 1: `thoth review record <task> -f <path>` lets a
/// commander running its own reviewer outside thoth (v6's Single Writer
/// rule does not apply to it - it is not a Thoth run at all) hand Thoth the
/// same `review.json` a Thoth reviewer run would produce, so the task settles
/// exactly as it would after `review start` - same validation
/// (`validate_review_document`, shared with `ingest_review`), same verdict ->
/// task-status mapping, same REJECT fold in `dispatch`.
///
/// Unlike the reviewer-run path, an invalid document here **refuses** rather
/// than settling the task `unverified`: the commander is present and can fix
/// the file and record again, so leaving the task exactly where it was (still
/// `in_review`) is more useful than spending an attempt on a report nobody
/// wrote as thoth.
///
/// Fix round 1 (F1): only an invalid *document* writes a `review_invalid`
/// event (stage + reason, `source: "external"`). Every precondition refusal
/// below - `THOTH_RUN_ID` set, the task not `in_review`, a live run, no
/// completed implementer run, that run having no `candidate_commit` - names
/// what it found and writes nothing at all; there is no report to blame for
/// those.
pub fn record(paths: &Paths, cwd: &Path, args: RecordArgs<'_>) -> Result<i32> {
    // Fix round 1 (F7): a guard against *accidental* use, not a security
    // boundary - v6 §16 has no sandbox, and a process can simply unset the
    // variable. `THOTH_RUN_ID` is only ever in the environment of a
    // process the runner itself launched (a worker or reviewer's `claude`,
    // runner.rs sets it before spawning), so seeing it here means this
    // command was invoked from inside one of those, most likely by the
    // worker/reviewer running `thoth review record` on its own task via
    // its `Bash` tool - which is exactly the "a live worker cannot approve
    // itself" property the other guarded commands already hold. Checked
    // before the file is even read.
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!(
            "review record refuses to run with THOTH_RUN_ID set: an external review is \
             recorded by the supervising session, not by a run"
        );
    }

    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, args.task)?;

    // 1. Read the file (stripping one leading UTF-8 BOM). A UTF-16 file (F6)
    // or an unreadable file is a plain error exit; nothing about the task or
    // its reviews is touched.
    let raw_bytes = std::fs::read(args.file)
        .with_context(|| format!("reading {}", args.file.display()))?;
    if let Some(kind) = utf16_bom_kind(&raw_bytes) {
        bail!(
            "{} looks like {kind} (Windows PowerShell 5.1's `Out-File`/`>` write this by \
             default); save it as UTF-8 and record again",
            args.file.display()
        );
    }
    let raw = String::from_utf8(raw_bytes)
        .with_context(|| format!("{} is not valid UTF-8", args.file.display()))?;
    let json_text = strip_bom(&raw);

    // 2. Resolve the task.
    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;

    // 3. Validate with the same rules a reviewer run's report is held to.
    // Invalid -> refuse: one `review_invalid` event (source: external), no
    // `reviews` row, `tasks.status` untouched.
    let validation_target = if spec::is_research_task(&task.spec) {
        Some(state::latest_completed_run_for_role(&conn, &task_id, role::IMPLEMENTER)?
            .ok_or_else(|| anyhow::anyhow!("task {task_id} has no completed implementer run to review"))?.run_id)
    } else { None };
    let (doc, cited) = match validate_review_document(&conn, &task_id, &task, validation_target.as_deref().unwrap_or(""), json_text) {
        Ok(v) => v,
        Err(e) => {
            let stage = e
                .detail
                .get("stage")
                .and_then(|s| s.as_str())
                .unwrap_or("unknown")
                .to_string();
            let mut detail = e.detail;
            if let Some(o) = detail.as_object_mut() {
                o.insert("reason".to_string(), serde_json::Value::String(e.reason.clone()));
                o.insert(
                    "source".to_string(),
                    serde_json::Value::String("external".to_string()),
                );
            }
            state::append_event(
                &conn,
                &NewEvent {
                    task_id: Some(task_id.clone()),
                    run_id: None,
                    event_type: event_type::REVIEW_INVALID.to_string(),
                    detail,
                },
            )?;
            bail!("review.json invalid at stage {stage}: {}", e.reason);
        }
    };
    let verdict = Verdict::parse(&doc.verdict).unwrap_or(Verdict::Unverified);

    // LESS-5: a PASS waits for the protected file approvals, like `review
    // start`; the candidate's evidence is made checkable first. Research
    // preparation waits for the transaction's target check below so a changed
    // target leaves no writes. REJECT and UNVERIFIED need no approval.
    let mut again: Option<(String, String)> = None;
    let mut notes: Vec<String> = Vec::new();
    if verdict == Verdict::Pass && validation_target.is_none() {
        if let Some(t) = state::latest_completed_run_for_role(&conn, &task_id, role::IMPLEMENTER)? {
            if let Some(candidate) = t.candidate_commit.clone() {
                let (w, e) = prepare_protected_evidence(
                    &conn,
                    paths,
                    &repo_id,
                    &task.base_commit,
                    &t,
                    &candidate,
                    &root,
                );
                for line in &w {
                    masked_eprintln!("warning: {line}");
                }
                notes = w;
                again = e.map(|e| (t.run_id.clone(), e));
            }
        }
    }

    // 4-7: everything else, inside one write transaction. `db::write_tx`
    // takes SQLite's write lock at `BEGIN`, so the preconditions re-checked
    // here are authoritative rather than a snapshot another writer (a Thoth
    // reviewer run landing, `dispatch` rerouting the task) could commit past.
    let tx = db::write_tx(&conn)?;

    let current = state::get_task(&tx, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    if current.status != TaskStatus::InReview.as_str() {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "task {task_id} is '{}'; review record needs in_review",
            current.status
        );
    }
    // Covers a Thoth reviewer run in flight and an implementer still running -
    // the same guard `review start` / `dispatch` / `resume` use.
    if let Some(live) = state::list_live_runs_for_task(&tx, &task_id)?.into_iter().next() {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "{}",
            crate::commands::live_run_refusal(
                &format!("refusing to record a review of task {task_id}"),
                &task_id,
                &live,
            )
        );
    }
    // Same choice `review start` makes: the newest completed implementer run.
    let target = match state::latest_completed_run_for_role(&tx, &task_id, role::IMPLEMENTER)? {
        Some(t) => t,
        None => {
            drop(tx); // explicit: rolls back, though nothing has been written yet
            bail!("task {task_id} has no completed implementer run to review");
        }
    };
    if validation_target.as_ref().is_some_and(|id| *id != target.run_id) {
        bail!("the research review target changed during validation; nothing was written. Record again.");
    }
    // Fix round 1 (F5): the same refusal `review start` makes
    // (`review.rs`'s `start`, above) before it ever checks anything out -
    // there is nothing for a review to have judged. A precondition refusal,
    // so nothing is written.
    if target.candidate_commit.is_none() {
        drop(tx); // explicit: rolls back, though nothing has been written yet
        bail!(
            "run {} has no candidate_commit, so there is nothing to record a review against",
            target.run_id
        );
    }
    // Hold the write lock before any evidence preparation for source checks.
    // A target that moved since validation must leave no writes behind.
    if verdict == Verdict::Pass && validation_target.is_some() {
        let (w, e) = prepare_protected_evidence(
            &tx, paths, &repo_id, &task.base_commit, &target,
            target.candidate_commit.as_deref().expect("checked candidate"), &root,
        );
        for line in &w { masked_eprintln!("warning: {line}"); }
        notes = w;
        again = e.map(|e| (target.run_id.clone(), e));
    }
    if verdict == Verdict::Pass {
        let again_error = again
            .as_ref()
            .filter(|(run_id, _)| *run_id == target.run_id)
            .map(|(_, e)| e.as_str());
        if let Some(why) = protected_refusal(&tx, &task_id, &target, &notes, again_error)? {
            drop(tx); // explicit: rolls back, though nothing has been written yet
            bail!("refusing to record a PASS for task {task_id}: {why}");
        }
    }

    let review = new_review_row(target.run_id.clone(), None, verdict, &doc.findings);
    state::insert_review(&tx, &review)?;

    let now = ids::now_rfc3339();
    if !state::transition_task_status(
        &tx,
        &task_id,
        &[TaskStatus::InReview],
        verdict.task_status(),
        &now,
    )? {
        // An external record must never leave a review row that did not
        // settle the task: unlike the runner (which cannot roll back a
        // finished process), this command can simply fail.
        drop(tx); // explicit: rolls back the reviews row inserted above too
        bail!(
            "task {task_id} moved out of in_review while this review was being recorded; \
             nothing was written. Record again."
        );
    }

    state::append_event(
        &tx,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: None,
            event_type: event_type::VERDICT.to_string(),
            detail: verdict_event_detail(VerdictEventShape {
                verdict,
                review_id: &review.review_id,
                target_run_id: &target.run_id,
                doc: &doc,
                cited: &cited,
                opened: None,
                unopened: None,
                source: Some("external"),
            }),
        },
    )?;
    tx.commit()?;

    // DT-3: the verdict is part of the shared records; a failure here only warns.
    if crate::commands::settings::records_dir(&paths.home).is_some() {
        if let Err(e) = crate::records::export(paths) {
            masked_eprintln!("warning: records export failed: {e:#}");
        }
    }

    // WT-1: a PASS finishes the task, so its clean worker worktree goes now.
    // Reported in the human lines only; the `--json` keys stay as they were.
    let mut worktree_lines = String::new();
    if verdict.task_status() == TaskStatus::Pass
        && !crate::commands::worktrees::cwd_inside_worktrees(paths, &repo_id, cwd)
    {
        use crate::commands::worktrees::{is_uncommitted, sweep, Scope, SOURCE_REVIEW_RECORD};
        match sweep(paths, &repo_id, &root, &conn, Scope::Task(&task_id), SOURCE_REVIEW_RECORD) {
            Ok(s) => {
                for r in &s.removed {
                    worktree_lines.push_str(&format!("\n  removed worktree {}", r.path));
                }
                for k in &s.kept {
                    worktree_lines.push_str(&format!("\n  kept worktree {}: {}", k.path, k.reason));
                    if is_uncommitted(k) {
                        worktree_lines.push_str(&format!(
                            "; run git worktree remove --force {} to drop the changes",
                            k.path
                        ));
                    }
                }
            }
            Err(e) => masked_eprintln!("warning: worktree cleanup failed: {e:#}"),
        }
    }

    let v = serde_json::json!({
        "task_id": task_id,
        "review_id": review.review_id,
        "verdict": verdict.as_str(),
        "task_status": verdict.task_status().as_str(),
        "target_run_id": target.run_id,
        "source": "external",
    });
    output::emit(
        args.json,
        &v,
        &format!(
            "{task_id}  {}  (external review; task now {}){worktree_lines}",
            verdict.as_str(),
            verdict.task_status().as_str()
        ),
    );
    Ok(0)
}

pub fn show(paths: &Paths, cwd: &Path, needle: &str, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;
    let reviews = state::list_reviews_for_task(&conn, &task_id)?;
    // L10: one schema, whether or not a review exists. A script reading
    // `review show --json` should test `review_count` (or `verdict == null`),
    // never have to cope with a different set of keys.
    let Some(latest) = reviews.last().cloned() else {
        output::emit(
            json,
            &serde_json::json!({
                "task_id": task_id,
                "review_count": 0,
                "review_id": serde_json::Value::Null,
                "verdict": serde_json::Value::Null,
                "target_run_id": serde_json::Value::Null,
                "reviewer_run_id": serde_json::Value::Null,
                "created_at": serde_json::Value::Null,
                "findings": serde_json::Value::Null,
                "cited": [],
                "opened": [],
                "unopened_citation": [],
                "claims": [],
                "source": serde_json::Value::Null,
            }),
            &format!("no review recorded for {task_id}; run `thoth review start {task_id} --profile <id>`"),
        );
        return Ok(0);
    };

    let findings_value: serde_json::Value =
        serde_json::from_str(&latest.findings).unwrap_or(serde_json::Value::Null);
    let doc = review_json::ReviewDocument {
        verdict: latest.verdict.clone(),
        findings: serde_json::from_value(findings_value.clone()).unwrap_or_default(),
    };
    let cited = review_json::cited_codes(&doc);
    // thin-adapter Task 1: an external review (`thoth review record`) has
    // no reviewer run to have opened anything through `evidence show` - there
    // is nothing to attribute a run-less `evidence_opened` event to, so
    // "opened" and "unopened_citation" are reported as not applicable rather
    // than as an empty (and misleadingly clean-looking) list.
    let source = if latest.reviewer_run_id.is_some() {
        "reviewer_run"
    } else {
        "external"
    };
    let (opened, unopened): (Option<Vec<String>>, Option<Vec<String>>) =
        match &latest.reviewer_run_id {
            Some(reviewer_run_id) => {
                let opened = opened_codes(&conn, reviewer_run_id)?;
                // brief §8: a citation the reviewer never opened is reported,
                // not punished. It may have read the same bytes out of the
                // diff it was handed, or out of the index label - but it is
                // exactly the signal §13's honesty metric wants.
                let unopened: Vec<String> = cited
                    .iter()
                    .filter(|c| !opened.contains(c))
                    .cloned()
                    .collect();
                (Some(opened), Some(unopened))
            }
            None => (None, None),
        };

    // The claims the review was judging, with the codes each one cited. This is
    // the "Requirement vs Implementation vs Evidence" triangle from v6 §4, in
    // one table.
    let code_of: std::collections::HashMap<String, String> =
        state::list_evidence_for_task(&conn, &task_id)?
            .into_iter()
            .map(|e| (e.evidence_id, e.code))
            .collect();
    let result = target_result(&conn, &latest.target_run_id)?;
    let mut result_claims = result["claims"].as_array().cloned().unwrap_or_default();
    let claims: Vec<serde_json::Value> = state::list_claims_for_run(&conn, &latest.target_run_id)?
        .into_iter()
        .map(|c| {
            let codes: Vec<String> = state::list_evidence_for_claim(&conn, &c.claim_id)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|eid| code_of.get(&eid).cloned())
                .collect();
            let sources = result_claims.iter().position(|r| r["kind"] == c.kind && r["text"] == c.text)
                .map(|i| result_claims.remove(i)["sources"].as_array().cloned().unwrap_or_default())
                .unwrap_or_default();
            serde_json::json!({"kind": c.kind, "text": c.text, "evidence": codes, "sources": sources})
        })
        .collect();

    let v = serde_json::json!({
        "task_id": task_id,
        "review_count": reviews.len(),
        "review_id": latest.review_id,
        "verdict": latest.verdict,
        "target_run_id": latest.target_run_id,
        "reviewer_run_id": latest.reviewer_run_id,
        "created_at": latest.created_at,
        "findings": findings_value,
        "cited": cited,
        "opened": opened,
        "unopened_citation": unopened,
        "claims": claims,
        "source": source,
    });

    let reviewer_label = match &latest.reviewer_run_id {
        Some(r) => format!("reviewer run {r}"),
        None => "external review".to_string(),
    };
    let mut lines = vec![format!(
        "{task_id}  {}  (review {} of {}, {reviewer_label})",
        latest.verdict,
        reviews.len(),
        reviews.len(),
    )];
    for c in &doc.findings.criteria {
        lines.push(format!(
            "  {:<6} {:<6} {:<20} {}",
            c.id,
            c.verdict,
            c.evidence.join(","),
            c.note.clone().unwrap_or_default()
        ));
    }
    for s in &doc.findings.sources {
        lines.push(format!("  {}  {}  {}  {}", s.id, s.verdict, s.url, s.reason));
    }
    for e in &doc.findings.extra_findings {
        lines.push(format!("  extra  {:<6} {} [{}]", e.severity, e.note, e.evidence.join(",")));
    }
    match &opened {
        Some(opened) => lines.push(format!(
            "  opened: {}",
            if opened.is_empty() { "-".to_string() } else { opened.join(",") }
        )),
        None => lines.push("  opened: n/a (external review)".to_string()),
    }
    if let Some(unopened) = &unopened {
        if !unopened.is_empty() {
            lines.push(format!(
                "  WARNING unopened_citation: {} (cited but never read through `evidence show`)",
                unopened.join(",")
            ));
        }
    }
    for c in &claims {
        lines.push(format!(
            "  claim  {:<14} {:<22} {}",
            c["kind"].as_str().unwrap_or(""),
            c["evidence"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join(","))
                .unwrap_or_default(),
            c["text"].as_str().unwrap_or("")
        ));
    }
    output::emit(json, &v, &lines.join("\n"));
    Ok(0)
}
