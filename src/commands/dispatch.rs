use crate::commands::spec_review;
use crate::config::Paths;
use crate::db;
use crate::db::global;
use crate::db::state::{self, NewEvent, Run, RunFinish};
use crate::ids;
use crate::lang::Lang;
use crate::model::{event_type, RunStatus, TaskStatus};
use crate::output;
use crate::plan_label;
use crate::process;
use crate::spec_review_json::SpecVerdict;
use crate::worktree::{self, EnsureOutcome};
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};

pub const STUCK_REPEATS: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StuckReason {
    Criterion(String),
    Finding(String),
    Rule(i64),
}

impl StuckReason {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Criterion(_) => "criterion",
            Self::Finding(_) => "finding",
            Self::Rule(_) => "rule",
        }
    }

    pub fn value(&self) -> String {
        match self {
            Self::Criterion(v) | Self::Finding(v) => v.clone(),
            Self::Rule(v) => v.to_string(),
        }
    }

    pub fn message(&self, task_id: &str) -> String {
        match self {
            Self::Criterion(ac) => format!(
                "task {task_id} is stuck: {ac} has been judged unmet in the last 3 REJECT reviews. Automatic rework is stopped. Rewrite the spec with thoth task update-spec {task_id} -f new-spec.json, or close it with thoth task close {task_id} --reason ..."
            ),
            Self::Finding(note) => format!(
                "task {task_id} is stuck: the finding \"{note}\" appeared in the last 3 REJECT reviews. Automatic rework is stopped. Rewrite the spec with thoth task update-spec {task_id} -f new-spec.json, or close it with thoth task close {task_id} --reason ..."
            ),
            Self::Rule(rule) => format!(
                "task {task_id} is stuck: result.json was refused by rule {rule} in the last 3 runs. Automatic rework is stopped. Rewrite the spec with thoth task update-spec {task_id} -f new-spec.json, or close it with thoth task close {task_id} --reason ..."
            ),
        }
    }
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct StuckDispatchError {
    pub message: String,
    pub reason: StuckReason,
}

fn normalize_finding(note: &str) -> String {
    let mut words = Vec::new();
    for word in note.split_whitespace() {
        let bare = word.trim_matches(|c: char| !c.is_alphanumeric());
        if bare.len() > 1
            && bare.chars().next().is_some_and(|c| c == 'e' || c == 'E')
            && bare[1..].chars().all(|c| c.is_ascii_digit())
        {
            continue;
        }
        words.push(word.chars().flat_map(char::to_lowercase).collect::<String>());
    }
    words
        .join(" ")
        .trim_end_matches(|c: char| c.is_whitespace() || matches!(c, '.' | '。' | ';'))
        .to_string()
}

/// Returns the repeated reason only after three consecutive post-spec failures.
/// Event sequence, rather than timestamps or review row ordering, is authoritative.
pub fn stuck_reason(conn: &rusqlite::Connection, task_id: &str) -> Result<Option<StuckReason>> {
    let events = state::events_since(conn, 0, Some(task_id))?;
    let spec_seq = events
        .iter()
        .filter(|e| e.event_type == event_type::SPEC_UPDATED)
        .map(|e| e.seq)
        .max()
        .unwrap_or(0);
    let mut rejects: Vec<(i64, serde_json::Value)> = events
        .iter()
        .filter(|e| e.seq > spec_seq && e.event_type == event_type::VERDICT)
        .filter_map(|e| {
            let detail: serde_json::Value = serde_json::from_str(&e.detail).ok()?;
            (detail["verdict"].as_str() == Some("REJECT")).then_some((e.seq, detail))
        })
        .collect();
    rejects.sort_by_key(|(seq, _)| *seq);
    let result_seq = events
        .iter()
        .filter(|e| e.event_type == event_type::RESULT_INGESTED)
        .map(|e| e.seq)
        .max()
        .unwrap_or(0);
    let mut invalids: Vec<(i64, i64)> = events
        .iter()
        .filter(|e| e.seq > spec_seq && e.seq > result_seq && e.event_type == event_type::RESULT_INVALID)
        .filter_map(|e| {
            let d: serde_json::Value = serde_json::from_str(&e.detail).ok()?;
            let rule = d["rule"].as_i64()?;
            (rule != 0).then_some((e.seq, rule))
        })
        .collect();
    invalids.sort_by_key(|(seq, _)| *seq);
    if invalids.len() >= STUCK_REPEATS {
        let last = &invalids[invalids.len() - STUCK_REPEATS..];
        if last.iter().all(|(_, rule)| *rule == last[0].1) {
            return Ok(Some(StuckReason::Rule(last[0].1)));
        }
    }
    if rejects.len() < STUCK_REPEATS {
        return Ok(None);
    }
    let last = &rejects[rejects.len() - STUCK_REPEATS..];
    let mut criterion_ids = Vec::new();
    for c in last[0].1["criteria"].as_array().into_iter().flatten() {
        if c["verdict"].as_str() == Some("unmet") {
            if let Some(id) = c["id"].as_str() {
                if last.iter().all(|(_, d)| d["criteria"].as_array().is_some_and(|cs| cs.iter().any(|x| x["id"].as_str() == Some(id) && x["verdict"].as_str() == Some("unmet")))) {
                    criterion_ids.push(id.to_string());
                }
            }
        }
    }
    if let Some(id) = criterion_ids.into_iter().next() {
        return Ok(Some(StuckReason::Criterion(id)));
    }
    let reviews = state::list_reviews_for_task(conn, task_id)?;
    for (_, detail) in last {
        let Some(review_id) = detail["review_id"].as_str() else { continue };
        let Some(review) = reviews.iter().find(|r| r.review_id == review_id) else { continue };
        let Ok(findings) = serde_json::from_str::<crate::review_json::Findings>(&review.findings) else { continue };
        for finding in findings.extra_findings {
            let note = normalize_finding(&finding.note);
            if !note.is_empty() && last.iter().all(|(_, d)| {
                d["review_id"].as_str().and_then(|id| reviews.iter().find(|r| r.review_id == id)).and_then(|r| serde_json::from_str::<crate::review_json::Findings>(&r.findings).ok()).is_some_and(|f| f.extra_findings.iter().any(|x| normalize_finding(&x.note) == note))
            }) {
                return Ok(Some(StuckReason::Finding(note)));
            }
        }
    }
    Ok(None)
}

pub struct DispatchArgs<'a> {
    pub task: &'a str,
    /// A profile id (`auto` is refused, RJ-1).
    pub profile: &'a str,
    /// `--model` as typed; `None` keeps the model of the attempt's previous
    /// implementer run, and is refused when there is none (RJ-1).
    pub model: Option<&'a str>,
    pub timeout_seconds: u64,
    pub allowed_tools: &'a str,
    /// One-shot override for `tasks.attempt_cap`: permit this dispatch if the
    /// attempt it computes is at most `N`. Writes nothing, so the next dispatch
    /// is gated again (Open question 11).
    pub allow_attempt: Option<i64>,
    /// `--effort`, as typed; validated by `run` before anything is written.
    pub effort: Option<&'a str>,
    /// `--reason`, as typed (DT-2); trimmed and checked by `run`.
    pub reason: Option<&'a str>,
    /// `--skip-spec-review`, as typed (SR-1); trimmed and checked by `run`.
    pub skip_spec_review: Option<&'a str>,
    pub json: bool,
}

/// brief §7, widened by three rulings and then by Phase 2. Phase 1's Open
/// question 17 admitted `in_review` (reroute) and `unverified` (the result file
/// was invalid); its Task 9 controller ruling admitted `failed` (the runner
/// marks a task failed on several of its own exit paths - see
/// `classify_outcome` and the DB-poll-failure branch in `runner.rs` - and that
/// task must stay re-dispatchable). Phase 2 adds `reject`, which is v6 §6.2's
/// rework loop and the one entry status that **bumps the attempt** rather than
/// reusing it.
///
/// `dispatch` itself never assigns `failed` to the task: on its own launch
/// failure it restores the task to whatever status it found it in and marks
/// only the stillborn run row `failed` (see `run` below).
///
/// Single source of truth for both the fast-path human-readable gate
/// (`dispatch_allowed`) and the conditional `UPDATE ... WHERE status IN (...)`
/// that enforces it inside the transaction (Important 1).
pub const DISPATCH_ENTRY_STATUSES: &[TaskStatus] = &[
    TaskStatus::Queued,
    TaskStatus::Interrupted,
    TaskStatus::InReview,
    TaskStatus::Unverified,
    TaskStatus::Failed,
    TaskStatus::Reject,
];

/// v6 §6.2: at low and normal risk the control plane folds the reviewer's
/// findings into the next attempt's spec by itself; at important and high risk
/// it refuses and sends the Supervisor to `task update-spec`, because a
/// mechanical fold of a subtle finding is exactly how a spec quietly stops
/// meaning what the human meant. A risk the control plane cannot read or does
/// not recognise is not in this list either, so it fails closed onto the
/// Supervisor rather than onto an automatic fold.
///
/// That refusal used to be permanent (trial 20's first important-risk REJECT):
/// `task update-spec` never moves the task off `reject`, so a dispatch that
/// only checks risk and status refuses the same task forever. See
/// `spec_rewritten_since_reject`, which is what lets a dispatch through once
/// the Supervisor has actually rewritten the spec.
pub const AUTO_FOLD_RISKS: [&str; 2] = ["low", "normal"];

/// v6 §6.2's escape hatch for an important/high rework: the control plane will
/// not fold findings itself, but it will let the dispatch through once the
/// Supervisor's rewrite is *newer* than the REJECT it answers - not merely
/// present somewhere in the task's history. Comparing event `seq` (not
/// timestamps, which are only second-resolution and can tie) is what makes a
/// REJECT recorded after an update-spec refuse again rather than replaying a
/// stale rewrite.
pub fn spec_rewritten_since_reject(conn: &rusqlite::Connection, task_id: &str) -> Result<bool> {
    let events = state::events_since(conn, 0, Some(task_id))?;
    let newest_reject_seq = events
        .iter()
        .rev()
        .find(|e| {
            e.event_type == event_type::VERDICT
                && serde_json::from_str::<serde_json::Value>(&e.detail)
                    .ok()
                    .and_then(|v| v.get("verdict").and_then(|s| s.as_str()).map(str::to_string))
                    .as_deref()
                    == Some(crate::review_json::Verdict::Reject.as_str())
        })
        .map(|e| e.seq);
    let Some(reject_seq) = newest_reject_seq else {
        return Ok(false);
    };
    let newest_spec_updated_seq = events
        .iter()
        .rev()
        .find(|e| e.event_type == event_type::SPEC_UPDATED)
        .map(|e| e.seq);
    Ok(newest_spec_updated_seq.is_some_and(|s| s > reject_seq))
}

pub fn dispatch_allowed(task_status: &str) -> bool {
    DISPATCH_ENTRY_STATUSES.iter().any(|s| s.as_str() == task_status)
}

/// Phase 1 always reused the attempt, because "the reject flow that bumps it is
/// Phase 2". This is that flow. Only a dispatch out of `reject` increments,
/// which is also what gives the rework a fresh worktree while a reroute keeps
/// the existing one (brief §9).
pub fn next_attempt(existing_max: Option<i64>, from_reject: bool) -> i64 {
    let current = existing_max.unwrap_or(1).max(1);
    if from_reject {
        current + 1
    } else {
        current
    }
}

/// brief §9: flattens a review's `findings` into the two lists a rework
/// addendum carries - one line per unmet criterion, one per extra finding.
///
/// Text, not structure, because the consumer is a prompt. Unparseable findings
/// fold to nothing: a rework with no addendum is a worse outcome than no rework,
/// but a panic here would be worse than both.
pub fn fold_findings(findings_json: &str, sources: &[crate::review_json::SourceToCheck]) -> (Vec<String>, Vec<String>) {
    let Ok(f) = serde_json::from_str::<crate::review_json::Findings>(findings_json) else {
        return (Vec::new(), Vec::new());
    };
    let mut unmet: Vec<String> = f
        .criteria
        .iter()
        .filter(|c| c.verdict.trim() == "unmet")
        .map(|c| {
            let note = c.note.clone().unwrap_or_default();
            let body = if note.trim().is_empty() {
                "not met".to_string()
            } else {
                note.trim().to_string()
            };
            let cited = c.evidence.join(",");
            if cited.is_empty() {
                format!("{}: {body}", c.id)
            } else {
                format!("{}: {body} (evidence {cited})", c.id)
            }
        })
        .collect();
    let mut extra: Vec<String> = Vec::new();
    for source in &f.sources {
        let body = match sources.iter().find(|s| s.id == source.id) {
            Some(s) => format!("claim {} ({}): {} — {}: {}", s.claim, s.id, s.claim_text, source.url, source.reason),
            None => format!("{}: {}: {}", source.id, source.url, source.reason),
        };
        match source.verdict.as_str() {
            "not_supported" => unmet.push(body),
            "unreachable" => extra.push(format!("unreachable: {body}; replace the source")),
            _ => {}
        }
    }
    extra.extend(f
        .extra_findings
        .iter()
        .map(|e| {
            let severity = if e.severity.trim().is_empty() {
                "finding".to_string()
            } else {
                e.severity.trim().to_string()
            };
            let cited = e.evidence.join(",");
            if cited.is_empty() {
                format!("{severity}: {}", e.note.trim())
            } else {
                format!("{severity}: {} (evidence {cited})", e.note.trim())
            }
        })
        .collect::<Vec<_>>());
    (unmet, extra)
}

/// brief §9: the run's spec is the task spec plus a `rework` object. The task's
/// own spec is untouched, so a later `task update-spec` diffs against what the
/// Supervisor wrote rather than against a machine-folded copy.
pub fn rework_spec(task_spec: &str, addendum: &crate::spec::ReworkAddendum) -> Result<String> {
    spec_with_addendum(task_spec, "rework", serde_json::to_value(addendum)?)
}

/// The same for a reroute out of `unverified`: the run's spec is the task spec
/// plus a `reroute` object, and the task's own spec is untouched.
pub fn reroute_spec(task_spec: &str, addendum: &crate::spec::RerouteAddendum) -> Result<String> {
    spec_with_addendum(task_spec, "reroute", serde_json::to_value(addendum)?)
}

fn spec_with_addendum(task_spec: &str, key: &str, addendum: serde_json::Value) -> Result<String> {
    let mut v: serde_json::Value = serde_json::from_str(task_spec).map_err(|e| {
        anyhow::anyhow!("the task spec is not valid JSON, so a {key} addendum cannot be added: {e}")
    })?;
    if !v.is_object() {
        bail!("the task spec is not a JSON object, so a {key} addendum cannot be added");
    }
    v[key] = addendum;
    Ok(v.to_string())
}

/// The addenda a handoff carries over from the run it continues.
pub(crate) const CARRIED_ADDENDA: [&str; 2] = ["rework", "reroute"];

/// `run_spec` with the previous run's `rework` and `reroute` objects copied
/// in verbatim, where the previous run's spec has them (S3-10 AC10). A
/// previous spec that is not a JSON object carries nothing.
pub(crate) fn carry_addenda(run_spec: &str, previous_spec: &str) -> Result<String> {
    let previous: serde_json::Value =
        serde_json::from_str(previous_spec).unwrap_or(serde_json::Value::Null);
    let mut out = run_spec.to_string();
    for key in CARRIED_ADDENDA {
        if let Some(addendum) = previous.get(key).filter(|v| !v.is_null()) {
            out = spec_with_addendum(&out, key, addendum.clone())?;
        }
    }
    Ok(out)
}

/// Why the task's newest implementer run was sent to `unverified`, if it was
/// because its result.json was rejected at ingest: that run's newest
/// `result_invalid` event. `None` when the newest implementer run has no such
/// event - an `unverified` made by an invalid review, for one - or when a
/// `result_ingested` event for the run came after it.
pub fn reroute_addendum(
    conn: &rusqlite::Connection,
    task_id: &str,
) -> Result<Option<crate::spec::RerouteAddendum>> {
    let newest = state::list_runs_for_task(conn, task_id)?
        .into_iter()
        .rev()
        .find(|r| r.role == crate::model::role::IMPLEMENTER);
    let Some(run) = newest else {
        return Ok(None);
    };
    let event = state::events_since(conn, 0, Some(task_id))?
        .into_iter()
        .rev()
        .find(|e| {
            (e.event_type == event_type::RESULT_INVALID
                || e.event_type == event_type::RESULT_INGESTED)
                && e.run_id.as_deref() == Some(&run.run_id)
        });
    // A later accepted ingest of the same run (`submit --run`) means the
    // rejection is already fixed, so there is nothing current to quote.
    let Some(event) = event.filter(|e| e.event_type == event_type::RESULT_INVALID) else {
        return Ok(None);
    };
    let Ok(detail) = serde_json::from_str::<serde_json::Value>(&event.detail) else {
        return Ok(None);
    };
    let Some(reason) = detail["reason"].as_str() else {
        return Ok(None);
    };
    Ok(Some(crate::spec::RerouteAddendum {
        previous_run: run.run_id,
        reason: reason.to_string(),
        rule: detail["rule"].as_i64(),
    }))
}

/// S3-9 AC1: the run a dispatch out of `interrupted` or `failed` continues.
/// The newest implementer run of `attempt` that started (by run id, the same
/// order as `list_runs_for_task`), if it ended `interrupted` or `failed`. The
/// profile does not matter: going back to the same profile is a handoff too.
pub fn handoff_source(
    conn: &rusqlite::Connection,
    task_id: &str,
    attempt: i64,
) -> Result<Option<Run>> {
    Ok(state::list_runs_for_task(conn, task_id)?
        .into_iter()
        .rfind(|r| {
            r.attempt == attempt
                && r.role == crate::model::role::IMPLEMENTER
                && r.started_at.is_some()
        })
        .filter(|r| {
            r.status == RunStatus::Interrupted.as_str() || r.status == RunStatus::Failed.as_str()
        }))
}

/// S3-9 AC5: two runs are on different providers only when both the provider
/// label (trimmed, case-insensitive) and the harness differ. A provider label
/// is self-chosen, so the same CLI under another label is not another
/// provider. An unknown `from` provider counts as different.
pub fn is_cross_provider(
    from_provider: Option<&str>,
    from_harness: &str,
    to_provider: &str,
    to_harness: &str,
) -> bool {
    let norm = |p: &str| p.trim().to_lowercase();
    let provider_differs = from_provider.is_none_or(|p| norm(p) != norm(to_provider));
    provider_differs && from_harness != to_harness
}

/// Removes the worktree's handoff file if dispatch is dropped before its
/// transaction commits (S3-9 AC3), so a refused dispatch leaves no file a
/// later worker could read as current.
struct HandoffCleanup<'a> {
    worktree: Option<&'a Path>,
}

impl Drop for HandoffCleanup<'_> {
    fn drop(&mut self) {
        if let Some(wt) = self.worktree {
            let _ = crate::handoff::remove(wt);
        }
    }
}

pub fn run(paths: &Paths, cwd: &Path, args: DispatchArgs<'_>) -> Result<i32> {
    use crate::commands::auto_pick;
    // RJ-1: `auto` is refused before anything else.
    auto_pick::refuse_auto(
        crate::model::role::IMPLEMENTER,
        args.task,
        args.profile,
        args.model,
        args.effort,
    )?;
    // S3-7: reject a bad level before a run row, worktree or event exists.
    let effort_given = args
        .effort
        .map(crate::adapter::effort::parse_level)
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?;
    let reason = auto_pick::clean_reason(args.reason)?;
    let skip = match args.skip_spec_review.map(str::trim) {
        None => None,
        Some("") => bail!("--skip-spec-review must not be empty"),
        Some(s) => Some(s.to_string()),
    };
    let (repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, args.task)?;
    let task = state::get_task(&conn, &task_id)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} disappeared"))?;
    if !dispatch_allowed(&task.status) {
        bail!(
            "task {task_id} is '{}'; dispatch needs queued, interrupted, in_review, unverified, \
             failed or reject",
            task.status
        );
    }

    // SR-1 (H11): the spec review gate. Both refusals come before anything
    // is picked, probed or written.
    let sr = spec_review::state(&conn, &task)?;
    let sr_required = spec_review::required(&conn, &task)?;
    // Stuck stops every dispatch, first or not, whatever the risk; the skip
    // flag does not apply.
    if sr.stalled {
        let tail = &sr.streak_ids[sr.streak_ids.len().saturating_sub(3)..];
        bail!(
            "task {task_id} is stuck: its spec was sent back by {} spec reviews in a row ({}). \
             Dispatch is stopped for the human; --skip-spec-review does not apply. Fix the spec \
             with thoth task update-spec {task_id} -f new-spec.json and record an APPROVE with \
             thoth spec review record {task_id} -f spec-review.json, or close it with thoth task \
             close {task_id} --reason ...",
            sr.revise_streak,
            tail.join(", ")
        );
    }
    if sr_required && !sr.approved && skip.is_none() {
        let why = match &sr.newest {
            None => "no spec review is recorded".to_string(),
            Some(n) if n.verdict == SpecVerdict::Revise.as_str() => {
                let mut why = format!("the newest spec review {} is REVISE", n.spec_review_id);
                // SR-3: the reviewer's edits are waiting for the author.
                if sr.pending_edits {
                    why.push_str(&format!(
                        "; its {} edits await the author check: thoth spec review confirm {task_id} \
                         --profile P --model M --effort E",
                        n.edit_count
                    ));
                }
                why
            }
            Some(n) => format!(
                "the newest APPROVE {} is for an older spec (sha256 {})",
                n.spec_review_id,
                spec_review::short(&n.spec_sha256)
            ),
        };
        bail!(
            "task {task_id} is risk '{}' and has not been dispatched yet: its current spec \
             (sha256 {}) has no APPROVE spec review ({why}). Have the other provider review the \
             spec and record the result with thoth spec review record {task_id} -f \
             spec-review.json, or pass --skip-spec-review \"why\" to dispatch without one \
             (recorded as a spec_review_skipped event)",
            task.risk,
            spec_review::short(&sr.spec_sha256)
        );
    }

    // I1 (fix round 1): every decision below - the attempt number, the
    // worktree, the cap, the risk gate, the findings fold - is derived from
    // *this one* status, read before any transaction opened. It is therefore
    // the only status the transaction at the bottom may commit against; see
    // the `transition_task_status` call there. `dispatch_allowed` above stays
    // the fast pre-check over the whole entry set.
    let entry_status = TaskStatus::parse(&task.status)?;
    let from_reject = entry_status == TaskStatus::Reject;
    let attempt = next_attempt(state::max_attempt(&conn, &task_id)?, from_reject);
    // The run a rework folds in.
    let rework_from = if from_reject {
        state::latest_completed_run_for_role(&conn, &task_id, crate::model::role::IMPLEMENTER)?
    } else {
        None
    };
    // DT-2: the newest implementer run of this attempt, whose model and
    // effort a left-out `--model` / `--effort` keeps (RJ-1). A rework is a new
    // attempt, so it has none.
    let previous_run = state::list_runs_for_task(&conn, &task_id)?
        .into_iter()
        .rfind(|r| r.attempt == attempt && r.role == crate::model::role::IMPLEMENTER);

    let gconn = db::open_global(paths)?;
    let profile = match global::get_profile(&gconn, args.profile)? {
        Some(p) => p,
        None => bail!("no such profile: {}", args.profile),
    };
    crate::commands::profile::require_supported_harness(
        &format!("profile {}", profile.profile_id),
        &profile.harness,
    )?;
    let adapter = crate::adapter::for_harness(&profile.harness);
    // MODELS-1: what a model-rule refusal lists (the stored list).
    let known_models = crate::model_list::known_models_text(&gconn, &profile);
    // RJ-1: the model and effort the commander gave, or the previous run's.
    // Every check writes nothing, so a refusal leaves nothing behind.
    let choice = auto_pick::choose(
        &gconn,
        &auto_pick::Request {
            task_id: &task_id,
            role: crate::model::role::IMPLEMENTER,
            model: args.model,
            effort: effort_given,
            previous: previous_run.as_ref(),
            retry: auto_pick::Retry::KeepsPrevious,
        },
        &profile,
        |m| adapter.check_model(&profile.profile_id, m, &known_models),
    )?;
    let require_reason = crate::commands::settings::require_reason(&paths.home);
    auto_pick::require_reason_gate(&task_id, "dispatch", reason.as_deref(), require_reason)?;
    // FC-1 (H4): the ranking is read only to see whether the profile has
    // recovered or is parked for another quota group; an unreadable ranking
    // does not stop the dispatch.
    let ranked = crate::commands::usage::ranked_candidates(
        paths,
        crate::model::role::IMPLEMENTER,
        Some(&task_id),
        Some(&choice.model),
    )
    .unwrap_or_default();
    let own = ranked.iter().find(|c| c.profile.profile_id == profile.profile_id);
    let recovered = own.is_some_and(|c| c.recovered && c.profile.status == "rate_limited");
    let parked_elsewhere = own.is_some_and(|c| c.parked_elsewhere);

    // Pre-flight: refuse to burn a worktree on a profile that is not logged in.
    // `probe_auth` is the silent half of `profile check`, so dispatch's own
    // --json output stays a single JSON document.
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
    crate::login_name::remember_or_warn(&gconn, &profile.profile_id, auth.email.as_deref());
    // MODELS-1: the list the login check just read is stored, then the model
    // is checked against it (agy) or against the models a run proved this
    // account cannot use (Claude), still before anything is written.
    let fresh_models = crate::model_list::remember_or_warn(&gconn, &profile.profile_id, &auth);
    let model_warning =
        crate::model_list::check_launch(&gconn, &profile, &choice.model, fresh_models.as_ref())?;
    // FC-1: the auth check just passed, so a `logged_out` profile is marked
    // active again once the run row exists (below).
    let logged_in_again = profile.status == "logged_out";

    // Open question 21's ruling: a profile marked `rate_limited` is warned about,
    // not refused. The operator often knows the window reset before the DB does,
    // and refusing would force an extra `profile check` round trip. The timestamp
    // comes from the newest `rate_limited` quota event, which is what parked it.
    let mut warnings: Vec<String> = Vec::new();
    warnings.extend(model_warning);
    let launch = auto_pick::launch_settings(&choice, &profile);
    if let Some(note) = launch.effort_note.clone() {
        warnings.push(note);
    }
    warnings.extend(choice.warning());
    // S3-8 AC4: a profile whose window reset since it was parked is marked
    // active once the run row exists (below), so it is not warned about; FC-1:
    // an antigravity profile parked for another quota group than this
    // model's is not warned about either.
    if profile.status == "rate_limited" && !recovered && !parked_elsewhere {
        let since = global::list_quota_events(&gconn, &profile.profile_id)?
            .into_iter()
            .rev()
            .find(|e| e.kind == "rate_limited")
            .map(|e| e.ts)
            .unwrap_or_else(|| "an unrecorded time".to_string());
        warnings.push(format!(
            "profile {id} is marked rate_limited since {since}; run may be interrupted \
             immediately. Run 'thoth profile check {id}' to clear.",
            id = profile.profile_id
        ));
    }
    // M11 (fix round 1): warnings must reach the human even when dispatch goes
    // on to fail later (worktree ensure, spawn_runner) - the earlier code only
    // printed them on the success path, so a `rate_limited` warning computed
    // above could be silently lost if the run never actually started.
    let json_mode = args.json;
    let emit_warnings = |warnings: &[String]| {
        if !json_mode {
            for w in warnings {
                masked_eprintln!("warning: {w}");
            }
        }
    };

    // S3-9 AC1: whether this dispatch continues a run that stopped early.
    let handoff_from = if matches!(entry_status, TaskStatus::Interrupted | TaskStatus::Failed) {
        handoff_source(&conn, &task_id, attempt)?
    } else {
        None
    };

    if matches!(entry_status, TaskStatus::Reject | TaskStatus::Unverified) {
        if let Some(reason) = stuck_reason(&conn, &task_id)? {
            let message = reason.message(&task_id);
            return Err(StuckDispatchError { message, reason }.into());
        }
    }

    // brief §9: the rework path. All three refusals happen before a worktree
    // exists, because each of them means "do not start this run at all".
    let mut rework: Option<crate::spec::ReworkAddendum> = None;
    // AC3: which of the two rework paths actually happened, for `--json`.
    // `None` outside a `reject` dispatch entirely.
    let mut folded_by: Option<&'static str> = None;
    let attempt_cap = state::get_task_attempt_cap(&conn, &task_id)?;
    if from_reject {
        let ceiling = args.allow_attempt.unwrap_or(attempt_cap);
        if attempt > ceiling {
            // M11: a warning computed above (a `rate_limited` profile) must
            // still reach the human on a path that fails afterwards.
            emit_warnings(&warnings);
            bail!(
                "task {task_id} would be attempt {attempt}, past its cap of {attempt_cap}. \
                 v6 §6.2: raise the implementer tier or hand the task back to the Supervisor. \
                 To override this one dispatch, pass --allow-attempt {attempt}"
            );
        }
        // v6 §6.2 plus trial 20's fix: important/high risk never folds
        // mechanically, but a Supervisor rewrite newer than the REJECT it
        // answers is accepted in its place - see `spec_rewritten_since_reject`.
        let spec_rewrite = if AUTO_FOLD_RISKS.contains(&task.risk.as_str()) {
            false
        } else if spec_rewritten_since_reject(&conn, &task_id)? {
            true
        } else {
            emit_warnings(&warnings); // M11, as above
            bail!(
                "task {task_id} is risk '{}', so its findings are not folded in automatically \
                 (v6 §6.2). Rewrite the spec first: \
                 `thoth task update-spec {task_id} -f new-spec.json`",
                task.risk
            );
        };
        folded_by = Some(if spec_rewrite { "spec_rewrite" } else { "control_plane" });
        let (unmet, extra) = if spec_rewrite {
            // Excluded alternative: folding the reviewer's findings on top of
            // the rewritten spec would put back a finding the Supervisor may
            // have dropped on purpose.
            (Vec::new(), Vec::new())
        } else {
            // The newest REJECT verdict is the one being reworked. An older PASS
            // or UNVERIFIED review must not supply the findings.
            let rejected = state::list_reviews_for_task(&conn, &task_id)?
                .into_iter()
                .rev()
                .find(|r| r.verdict == crate::review_json::Verdict::Reject.as_str());
            let (unmet, extra) = match &rejected {
                Some(r) => fold_findings(&r.findings, &crate::review_json::sources_to_check(
                    &crate::commands::review::target_result(&conn, &r.target_run_id)?,
                )),
                None => (Vec::new(), Vec::new()),
            };
            // R3 (fix round 1): a rework with nothing folded in still costs the
            // task an attempt against its cap, and the worker is told to "fix
            // exactly these" with nothing listed. It is not worth refusing -
            // the Supervisor may have rewritten the spec instead - but it must
            // not happen quietly. AC3: a spec-rewrite rework folds zero
            // findings by design, so it does not warn here at all.
            if unmet.is_empty() && extra.is_empty() {
                warnings.push(format!(
                    "task {task_id} is being reworked as attempt {attempt}, but no reviewer \
                     findings could be folded into its spec ({}). The worker will get the \
                     spec and nothing else; check `thoth review show {task_id}`.",
                    match &rejected {
                        Some(_) => "the newest REJECT review's findings were empty or unreadable",
                        None => "no REJECT review is recorded for this task",
                    }
                ));
            }
            (unmet, extra)
        };
        let previous = rework_from.clone();
        rework = Some(crate::spec::ReworkAddendum {
            attempt,
            previous_run: previous
                .as_ref()
                .map(|r| r.run_id.clone())
                .unwrap_or_default(),
            // R2: the attempt the findings and commits really came from, which
            // is not `attempt - 1` when an attempt in between never completed.
            previous_attempt: previous.as_ref().map(|r| r.attempt).unwrap_or(0),
            previous_candidate: previous
                .as_ref()
                .and_then(|r| r.candidate_commit.clone())
                .unwrap_or_default(),
            unmet,
            extra_findings: extra,
        });
    }

    let branch = worktree::branch_name(&task_id, attempt);
    let wt = worktree::worktree_path(paths, &repo_id, &task_id, attempt);
    // brief §9: a rework keeps the work and drops the conversation, so the new
    // attempt's branch starts at the previous candidate commit rather than at
    // the task's original base. Everything else still starts from
    // `task.base_commit` (and, being the same attempt, reuses its worktree).
    let worktree_base = rework
        .as_ref()
        .map(|r| r.previous_candidate.clone())
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| task.base_commit.clone());
    let outcome = match worktree::ensure_worktree(&root, &wt, &branch, &worktree_base) {
        Ok(ensured) => {
            warnings.extend(ensured.warning);
            ensured.outcome
        }
        Err(e) => {
            state::append_event(
                &conn,
                &NewEvent {
                    task_id: Some(task_id.clone()),
                    run_id: None,
                    event_type: event_type::WORKTREE_ERROR.to_string(),
                    detail: serde_json::json!({
                        "branch": branch,
                        "path": wt.display().to_string(),
                        "base_commit": worktree_base,
                        "error": e.to_string(),
                    }),
                },
            )?;
            emit_warnings(&warnings);
            return Err(e);
        }
    };

    let run_id = ids::new_run_id();
    let run_dir = paths.run_dir(&repo_id, &run_id);
    std::fs::create_dir_all(&run_dir)?;

    // S3-9 AC3: a handoff writes `.agent/handoff.md` (and the run directory's
    // copy) now, outside the transaction below, because it runs git several
    // times. Any other dispatch removes a handoff file an earlier one left, so
    // the new worker cannot read a stale one.
    let mut handoff: Option<(crate::handoff::HandoffRef, crate::handoff::Written, Option<String>)> =
        None;
    if let Some(previous) = &handoff_from {
        let written = crate::handoff::gather(
            &conn,
            &gconn,
            previous,
            &crate::handoff::Target {
                profile: &profile.profile_id,
                harness: &profile.harness,
                model: &choice.model,
            },
        )
        .and_then(|facts| {
            // RD-1: the file goes to the new worker, possibly another provider's.
            let masker = crate::redact::Masker::load(Some(&paths.home));
            let (content, counts) = crate::handoff::render_facts(&facts, &masker);
            let mut written = crate::handoff::write(&run_dir, &wt, &content, &masker)?;
            written.redactions.merge(&counts);
            Ok((written, facts.previous_harness.clone()))
        });
        let (written, from_harness) = match written {
            Ok(w) => w,
            Err(e) => {
                emit_warnings(&warnings); // M11
                return Err(e.context(format!(
                    "could not write the handoff file for task {task_id}; nothing was dispatched"
                )));
            }
        };
        let from_provider = Some(previous.provider.clone()).filter(|p| !p.trim().is_empty());
        handoff = Some((
            crate::handoff::HandoffRef {
                previous_run: previous.run_id.clone(),
                reason: crate::handoff::reason(previous),
                from_profile: previous.account.clone(),
                from_harness,
                path: crate::handoff::PATH.to_string(),
            },
            written,
            from_provider,
        ));
    } else if let Err(e) = crate::handoff::remove(&wt) {
        warnings.push(format!("could not remove a stale {}: {e:#}", crate::handoff::PATH));
    }
    // Disarmed once the transaction commits.
    let mut handoff_cleanup = HandoffCleanup {
        worktree: handoff.is_some().then_some(wt.as_path()),
    };

    // brief §9: only the *run's* copy of the spec carries the addendum. The
    // task's own spec stays exactly as the Supervisor wrote it.
    let run_spec = match &rework {
        Some(r) => rework_spec(&task.spec, r)?,
        None if entry_status == TaskStatus::Unverified => match reroute_addendum(&conn, &task_id)? {
            Some(r) => reroute_spec(&task.spec, &r)?,
            None => task.spec.clone(),
        },
        None => task.spec.clone(),
    };
    let run_spec = match &handoff {
        Some((h, _, _)) => {
            // S3-10 AC10: a handoff keeps the previous run's `rework` and
            // `reroute`, so the prompt still lists why the attempt was sent back.
            let previous = handoff_from.as_ref().map(|r| r.spec.as_str()).unwrap_or("{}");
            let run_spec = carry_addenda(&run_spec, previous)?;
            spec_with_addendum(&run_spec, "handoff", serde_json::to_value(h)?)?
        }
        None => run_spec,
    };
    let run_spec = crate::adapter::effort::with_launch(&run_spec, &launch)?;

    let run = Run {
        run_id: run_id.clone(),
        task_id: task_id.clone(),
        attempt,
        role: crate::model::role::IMPLEMENTER.to_string(),
        provider: profile.provider.clone(),
        account: profile.profile_id.clone(),
        model: choice.model.clone(),
        // Codex names its own thread; the runner records it from
        // `thread.started`, so there is nothing to pre-generate. The same
        // goes for any harness that names its session late.
        harness_session_id: if adapter.names_session_late() {
            None
        } else {
            Some(ids::new_session_uuid())
        },
        config_dir: profile.config_dir.clone(),
        // RES-1a: a research task's worker also gets its harness's web tools.
        allowed_tools: adapter
            .implementer_allowed_tools(args.allowed_tools, crate::spec::is_research_task(&task.spec)),
        worktree: wt.display().to_string(),
        base_commit: worktree_base.clone(),
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
        // final-review Important 3: record the harness this run is actually
        // launched with, so `cancel` compares against the run's own binary
        // instead of whatever THOTH_CLAUDE_BIN the cancelling shell has.
        harness_bin: Some(crate::adapter::harness_bin(&profile.harness)),
    };
    // A dispatch entry state may be wait-terminal: `interrupted`, `in_review`,
    // `unverified` and `failed` all satisfy `TaskStatus::is_wait_terminal` -
    // only `queued` does not. If only the runner wrote `running`, a `thoth
    // wait` issued right after a reroute would return the very status the
    // reroute exists to escape, because the runner still has to start a
    // process, open two databases, migrate, create directories and write the
    // prompt first. So the transition happens here, synchronously, before
    // dispatch returns.
    let previous_task_status = task.status.clone();

    // Task 11 controller ruling: `insert_run`, the worktree event and the
    // `running` transition commit as one unit, so a failure partway through
    // (e.g. a `SQLITE_BUSY` on the second or third write) can never leave a
    // run row with no worktree event, or a task stuck `running` with no run
    // row behind it. `spawn_runner` below stays outside this transaction -
    // launching a process is not something a DB rollback can undo.
    //
    // Important 1 (whole-branch review): the gate above (`dispatch_allowed`)
    // read `task.status` before this transaction opened, so a second
    // concurrent `dispatch` for the same task could read the same
    // re-entrant status and pass the same gate. Re-checking the status here,
    // as part of the same conditional `UPDATE` that writes `running`, closes
    // that race: only one of two concurrent callers can win the `WHERE
    // status IN (...)` predicate, and the loser rolls back before it ever
    // inserts a run row or spawns a runner.
    //
    // Important 1/5 (whole-branch review): the status guard alone is not
    // enough, because `in_review` is a dispatch entry state *and* the status a
    // task sits at while a reviewer run is in flight. Rerouting a task under
    // review used to be accepted, leaving an implementer and a reviewer live
    // at once - two workers, one task, and whichever settled last decided the
    // task's status. Written generically (any live run, not just a reviewer
    // one) so it also covers a stale `queued`/`running` row from a runner that
    // never reported. Inside the transaction, immediately before the write it
    // protects; `db::write_tx` takes the write lock at `BEGIN` so the read is
    // authoritative rather than a snapshot another writer can commit past.
    // M2 (fix round 1): appended after `insert_run`, not before, so this event
    // never names a `run_id` that does not exist in `runs` yet - a reader
    // walking `events` right after seeing this one must be able to look the
    // run up.
    let mut events = vec![NewEvent {
        task_id: Some(task_id.clone()),
        run_id: Some(run_id.clone()),
        event_type: match outcome {
            EnsureOutcome::Created => event_type::WORKTREE_CREATED,
            EnsureOutcome::Reused => event_type::WORKTREE_REUSED,
        }
        .to_string(),
        detail: serde_json::json!({
            "branch": branch,
            "path": wt.display().to_string(),
            "attempt": attempt,
            // `worktree_base`, not `task.base_commit`: a rework branches
            // from the previous candidate, and this event is the record of
            // what the worktree was actually created from.
            "base_commit": worktree_base,
        }),
    }];
    // SR-1: a skipped spec review is always on the record, needed or not.
    if let Some(skip_reason) = &skip {
        events.push(NewEvent {
            task_id: Some(task_id.clone()),
            run_id: Some(run_id.clone()),
            event_type: event_type::SPEC_REVIEW_SKIPPED.to_string(),
            detail: serde_json::json!({
                "reason": skip_reason,
                "required": sr_required,
                "risk": task.risk,
                "spec_sha256": sr.spec_sha256,
                "newest_spec_review": sr.newest.as_ref().map(|n| n.spec_review_id.clone()),
                "newest_verdict": sr.newest.as_ref().map(|n| n.verdict.clone()),
                "newest_current": sr.newest.as_ref().is_some_and(|n| n.spec_sha256 == sr.spec_sha256),
            }),
        });
    }
    // S3-9 AC5: committed with the run row, before the status change.
    if let Some((h, written, from_provider)) = &handoff {
        events.push(NewEvent {
            task_id: Some(task_id.clone()),
            run_id: Some(run_id.clone()),
            event_type: event_type::HANDOFF_WRITTEN.to_string(),
            detail: handoff_written_detail(
                h,
                written,
                from_provider.as_deref(),
                handoff_from.as_ref().map(|r| r.model.as_str()),
                &profile,
                &choice.model,
            ),
        });
        events.extend(crate::redact::event(&task_id, Some(&run_id), "handoff", &written.redactions, &[]));
    }
    // RJ-1: what was launched and why, after the other events and before
    // the status change `start_run` adds.
    events.push(NewEvent {
        task_id: Some(task_id.clone()),
        run_id: Some(run_id.clone()),
        event_type: event_type::LAUNCH_CHOICE.to_string(),
        detail: auto_pick::LaunchChoice {
            role: crate::model::role::IMPLEMENTER,
            source: auto_pick::SOURCE_DISPATCH,
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
    });

    let start = RunStart {
        run: &run,
        events,
        recovered,
        logged_in_again,
    };
    #[cfg(debug_assertions)]
    spec_review::test_pause("THOTH_TEST_DISPATCH_PAUSE_FILE");
    // SR-1 (AC10): set when the check below refuses, so the worktree this
    // dispatch created goes with the run that was never written.
    let spec_changed = std::cell::Cell::new(false);
    let started = start_run(
        &conn,
        &gconn,
        &root,
        &start,
        // SR-1 (AC10): the spec review gate judged the spec and risk read
        // before any transaction opened; an `update-spec` since then would
        // launch a spec the gate never saw.
        |tx| {
            let unchanged = state::get_task(tx, &task_id)?.is_none_or(|now| {
                spec_review::spec_unchanged(&sr.spec_sha256, &now.spec) && now.risk == task.risk
            });
            if unchanged {
                return Ok(None);
            }
            spec_changed.set(true);
            Ok(Some(anyhow::anyhow!(
                "task {task_id}'s spec changed while it was being dispatched; dispatch again so \
                 the spec review gate sees the new spec"
            )))
        },
        // I1 (fix round 1): the guard is `entry_status`, not the whole entry
        // set. Phase 1 could pass the set because every entry status was
        // handled identically; Task 8 made the set non-uniform, so a task that
        // moved from one entry status to another under this dispatch
        // (`in_review -> reject` while the worktree was being created is the
        // case that matters) would still have satisfied a set-membership
        // guard, and would have committed a run that skipped the attempt bump,
        // the cap, the risk gate and the findings fold - silently, with no
        // error and no event. Only the status this dispatch actually planned
        // against may commit.
        |tx| {
            if state::transition_task_status(
                tx,
                &task_id,
                &[entry_status],
                TaskStatus::Running,
                &ids::now_rfc3339(),
            )? {
                return Ok(None);
            }
            // Rolls back on drop: the run row and worktree event just inserted
            // never commit, so no run is left behind for a task this dispatch
            // is refusing.
            let now_status = state::get_task(tx, &task_id)?
                .map(|t| t.status)
                .unwrap_or_else(|| "unknown".to_string());
            Ok(Some(anyhow::anyhow!(
                "task {task_id} was '{entry_status}' when this dispatch read it and is \
                 '{now_status}' now; the attempt, worktree and rework it planned all depend on \
                 that status, so nothing was written (state changed underneath). Dispatch again."
            )))
        },
        // Nothing is going to run, so do not leave the task pinned to
        // `running` with no live runner: put it back where it was (the
        // stillborn run is marked failed by `start_run`). M3 (fix round 1):
        // a best-effort compensating write - if it fails, that must not be
        // silent, because a task stuck `running` with no runner is exactly
        // the drift this rollback exists to prevent.
        |_| {
            if let Ok(prev) = TaskStatus::parse(&previous_task_status) {
                if let Err(rollback_err) =
                    state::set_task_status(&conn, &task_id, prev, &ids::now_rfc3339())
                {
                    masked_eprintln!(
                        "thoth: warning: could not roll task {task_id} back to \
                         '{previous_task_status}' after a runner-spawn failure: {rollback_err}"
                    );
                }
            }
        },
    )?;
    let runner_pid = match started {
        StartOutcome::LiveRun(live) => {
            emit_warnings(&warnings);
            bail!(
                "{}",
                crate::commands::live_run_refusal(
                    &format!("refusing to dispatch task {task_id}"),
                    &task_id,
                    &live,
                )
            );
        }
        StartOutcome::Refused(e) => {
            if spec_changed.get() && outcome == EnsureOutcome::Created {
                worktree::remove_worktree(&root, &wt);
            }
            emit_warnings(&warnings);
            return Err(e);
        }
        StartOutcome::SpawnFailed { error, warnings: more, finish_error } => {
            handoff_cleanup.worktree = None;
            warnings.extend(more);
            if let Some(msg) = finish_error {
                masked_eprintln!("thoth: warning: {msg}");
            }
            emit_warnings(&warnings);
            return Err(error);
        }
        StartOutcome::Started { runner_pid, warnings: more } => {
            handoff_cleanup.worktree = None;
            warnings.extend(more);
            runner_pid
        }
    };

    let v = serde_json::json!({
        "ok": true,
        "run_id": run_id,
        "task_id": task_id,
        "attempt": attempt,
        "attempt_cap": attempt_cap,
        "rework": rework.is_some(),
        "base_commit": worktree_base,
        "folded_findings": rework.as_ref().map(|r| r.unmet.len() + r.extra_findings.len()).unwrap_or(0),
        "folded_by": folded_by,
        "spec_review": {
            "required": sr_required,
            "approved": sr.approved,
            "skipped": skip.is_some(),
            "spec_review_id": sr.newest.as_ref().map(|n| n.spec_review_id.clone()),
        },
        "profile": profile.profile_id,
        "model": choice.model,
        "allowed_tools": run.allowed_tools,
        "worktree": wt.display().to_string(),
        "worktree_reused": outcome == EnsureOutcome::Reused,
        "runner_pid": runner_pid,
        "run_dir": run_dir.display().to_string(),
        "timeout_seconds": args.timeout_seconds,
        "effort_requested": launch.effort_requested,
        "effort": launch.effort,
        "effort_note": launch.effort_note,
        // S3-9 AC5: null unless this dispatch continued a stopped run.
        "handoff": handoff.as_ref().map(|(h, _, from_provider)| serde_json::json!({
            "previous_run": h.previous_run,
            "reason": h.reason,
            "cross_provider": is_cross_provider(
                from_provider.as_deref(),
                &h.from_harness,
                &profile.provider,
                &profile.harness,
            ),
            "path": h.path,
        })),
        "warnings": warnings,
    });
    // In human mode the warnings go to stderr so they cannot be mistaken for the
    // run id a script is parsing; in --json mode they travel inside the document.
    emit_warnings(&warnings);
    let labels = plan_label::labels(&gconn);
    let named = plan_label::named(
        &profile.profile_id,
        labels.get(&profile.profile_id).map(String::as_str),
        Lang::En,
    );
    let human = format!(
        "dispatched {run_id} (task {task_id}, attempt {attempt}, profile {named}, runner pid {runner_pid})"
    );
    output::emit(args.json, &v, &human);
    Ok(0)
}

/// The `handoff_written` event's detail.
pub(crate) fn handoff_written_detail(
    h: &crate::handoff::HandoffRef,
    written: &crate::handoff::Written,
    from_provider: Option<&str>,
    from_model: Option<&str>,
    to: &global::Profile,
    to_model: &str,
) -> serde_json::Value {
    serde_json::json!({
        "previous_run": h.previous_run,
        "reason": h.reason,
        "from_profile": h.from_profile,
        "from_provider": from_provider,
        "from_harness": h.from_harness,
        "from_model": from_model,
        "to_profile": to.profile_id,
        "to_provider": to.provider,
        "to_harness": to.harness,
        "to_model": to_model,
        "cross_provider": is_cross_provider(from_provider, &h.from_harness, &to.provider, &to.harness),
        "path": h.path,
        "sha256": written.sha256,
        "bytes": written.bytes,
    })
}

/// A new run for `start_run` to commit and launch.
pub(crate) struct RunStart<'a> {
    pub run: &'a Run,
    /// Appended after the run row, in this order, in the same transaction.
    pub events: Vec<NewEvent>,
    /// The profile's window reset since it was parked (S3-8 AC4, FC-1): it is
    /// marked active once the run row is committed.
    pub recovered: bool,
    /// FC-1: the profile was `logged_out` and its auth check passed: it is
    /// marked active (`login_restored`) once the run row is committed.
    pub logged_in_again: bool,
}

/// What `start_run` did.
pub(crate) enum StartOutcome {
    /// Committed, and the runner is running. `warnings` are things that went
    /// wrong after the commit and did not stop the run.
    Started { runner_pid: u32, warnings: Vec<String> },
    /// A live run of the task exists; no new run was inserted.
    LiveRun(Box<Run>),
    /// A check of the caller's refused inside the transaction, which rolled
    /// back. The error is the caller's.
    Refused(anyhow::Error),
    /// Committed, but the runner could not be started; the run row is now
    /// `failed` (unless another process had settled it already, which is in
    /// `warnings`). `finish_error` is set when marking it failed itself
    /// failed.
    SpawnFailed {
        error: anyhow::Error,
        warnings: Vec<String>,
        finish_error: Option<String>,
    },
}

/// The part of starting a run after every check outside the transaction: in
/// one `db::write_tx` transaction, the live-run check, the caller's
/// `before_insert` check, the run row, `start.events` and the caller's
/// `after_events` step; after the commit, `mark_recovered`, the runner and
/// its pid.
///
/// Task 11 controller ruling: the run row and its events commit as one unit,
/// so a failure partway through can never leave a run row with no events
/// behind it. Important 1 (whole-branch review): the live-run check is inside
/// the transaction, immediately before the write it protects; `write_tx`
/// takes the write lock at `BEGIN`, so the read is authoritative. Launching a
/// process is not something a rollback can undo, so the runner starts after
/// the commit; when it cannot, `on_spawn_failure` runs (the caller's own
/// compensation) and then the stillborn run is marked `failed`.
///
/// `before_insert` / `after_events` return `Some(error)` to refuse; a
/// refusal, like a live run, rolls the transaction back.
pub(crate) fn start_run(
    conn: &rusqlite::Connection,
    gconn: &rusqlite::Connection,
    repo_root: &Path,
    start: &RunStart<'_>,
    before_insert: impl FnOnce(&rusqlite::Connection) -> Result<Option<anyhow::Error>>,
    after_events: impl FnOnce(&rusqlite::Connection) -> Result<Option<anyhow::Error>>,
    on_spawn_failure: impl FnOnce(&anyhow::Error),
) -> Result<StartOutcome> {
    let run = start.run;
    let tx = db::write_tx(conn)?;
    if let Some(live) = state::list_live_runs_for_task(&tx, &run.task_id)?.into_iter().next() {
        return Ok(StartOutcome::LiveRun(Box::new(live)));
    }
    if let Some(refusal) = before_insert(&tx)? {
        return Ok(StartOutcome::Refused(refusal));
    }
    state::insert_run(&tx, run)?;
    for event in &start.events {
        state::append_event(&tx, event)?;
    }
    if let Some(refusal) = after_events(&tx)? {
        drop(tx); // rolls back the run row and the events above
        return Ok(StartOutcome::Refused(refusal));
    }
    tx.commit()?;

    let mut warnings = Vec::new();
    // S3-8 AC4: only now - every check passed and the run row exists - does a
    // recovered profile go back to `active`. Before the runner starts, so a
    // quota stop the run itself records cannot be overwritten by this. The
    // run is already committed, so a failure here is a warning, not a failed
    // start.
    if start.recovered {
        if let Err(e) = crate::commands::auto_pick::mark_recovered(
            gconn,
            &run.account,
            &run.task_id,
            &run.run_id,
        ) {
            warnings.push(format!(
                "profile {} was picked because its window reset, but marking it active again \
                 failed: {e}; run `thoth profile check {}`",
                run.account, run.account
            ));
        }
    }
    // FC-1: a `logged_out` profile whose auth check passed, at the same point
    // and with the same failure handling.
    if start.logged_in_again {
        if let Err(e) = crate::commands::auto_pick::mark_logged_in_again(
            gconn,
            &run.account,
            &run.task_id,
            &run.run_id,
            crate::commands::auto_pick::SOURCE_DISPATCH,
        ) {
            warnings.push(format!(
                "profile {} passed its auth check, but marking it active again failed: {e}; run \
                 `thoth profile check {}`",
                run.account, run.account
            ));
        }
    }

    let runner_pid = match spawn_runner(repo_root, &run.run_id) {
        Ok(pid) => pid,
        Err(e) => {
            on_spawn_failure(&e);
            // M3 (fix round 1): a best-effort compensating write; a failure is
            // reported, never silent, because a run left `queued` forever is
            // exactly the drift this exists to prevent.
            let mut finish_error = None;
            match state::finish_run(
                conn,
                &run.run_id,
                &RunFinish {
                    status: RunStatus::Failed,
                    interrupted_reason: None,
                    exit_code: None,
                    candidate_commit: None,
                    error_text: Some(format!("could not spawn the runner: {e}")),
                    ended_at: ids::now_rfc3339(),
                },
            ) {
                Ok(true) => {}
                Ok(false) => {
                    warnings.push(format!(
                        "run {} was already settled by another process; \
                         this runner-spawn-failure rollback did not overwrite it",
                        run.run_id
                    ));
                }
                Err(finish_err) => {
                    finish_error = Some(format!(
                        "could not mark run {} failed after a runner-spawn failure: {finish_err}",
                        run.run_id
                    ));
                }
            }
            return Ok(StartOutcome::SpawnFailed { error: e, warnings, finish_error });
        }
    };

    // I2 (fix round 1, controller ruling): the worker is already live at this
    // point, so a failure to record its pid must not be reported as a failed
    // start - that would make an operator think no run started at all, and a
    // second `dispatch` would then race a worker that is, in fact, running.
    // One retry after a short pause covers a transient lock/busy error; if it
    // still fails, the start still succeeds, but warns that `cancel`'s
    // pid-based escalation will not have a pid to use until someone repairs
    // the row.
    if state::set_runner_pid(conn, &run.run_id, runner_pid as i64).is_err() {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if let Err(e) = state::set_runner_pid(conn, &run.run_id, runner_pid as i64) {
            warnings.push(format!(
                "runner started (pid {runner_pid}) but recording runner_pid failed: {e}; \
                 cancel escalation will need the pid manually"
            ));
        }
    }
    Ok(StartOutcome::Started { runner_pid, warnings })
}

/// Launch `thoth run-worker <run_id>` detached, with cwd = repo root so the
/// runner can rediscover the repo the same way every other command does.
pub fn spawn_runner(repo_root: &Path, run_id: &str) -> Result<u32> {
    let exe: PathBuf = std::env::current_exe()?;
    let ctrl = process::controller();
    ctrl.spawn_detached(
        &exe.display().to_string(),
        &["run-worker".to_string(), run_id.to_string()],
        repo_root,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_finding_strips_every_trailing_terminator_but_not_inner_ones() {
        assert_eq!(normalize_finding("Missing check.;。"), "missing check");
        assert_eq!(normalize_finding("Fix a.b now.;。"), "fix a.b now");
        assert_eq!(normalize_finding("x ."), "x");
        assert_eq!(normalize_finding("x. ."), "x");
        assert_eq!(normalize_finding("x ; 。 ."), "x");
    }

    /// Phase 1 test, widened: Phase 2 adds `reject`, which is the whole point
    /// of v6 §6.2's rework loop.
    #[test]
    fn dispatch_is_allowed_from_the_six_re_entry_statuses() {
        // brief §7 gives queued / interrupted / in_review (reroute); Phase 1's
        // Open question 17 added unverified; the Task 9 controller ruling added
        // failed; Phase 2 adds reject.
        for ok in [
            "queued",
            "interrupted",
            "in_review",
            "unverified",
            "failed",
            "reject",
        ] {
            assert!(dispatch_allowed(ok), "{ok} should be dispatchable");
        }
        // draft has no spec finalised yet; running already has a worker; pass
        // cancelled and accepted are decisions nobody is reworking
        for no in ["draft", "running", "pass", "cancelled", "accepted"] {
            assert!(!dispatch_allowed(no), "{no} should be refused");
        }
    }

    /// Phase 1 test, extended (Open question 19): a reroute still reuses the
    /// attempt, and only a dispatch out of `reject` increments it.
    #[test]
    fn attempt_numbering_reuses_the_attempt_except_out_of_reject() {
        assert_eq!(next_attempt(None, false), 1);
        assert_eq!(next_attempt(Some(1), false), 1);
        assert_eq!(next_attempt(Some(3), false), 3);

        assert_eq!(
            next_attempt(None, true),
            2,
            "a reject with no runs is still a rework"
        );
        assert_eq!(next_attempt(Some(1), true), 2);
        assert_eq!(next_attempt(Some(3), true), 4);
    }

    #[test]
    fn only_low_and_normal_risk_fold_their_findings_automatically() {
        // v6 §6.2: low/normal fold mechanically, important/high go back to the
        // Supervisor to rewrite the spec.
        assert_eq!(AUTO_FOLD_RISKS, ["low", "normal"]);
        assert!(AUTO_FOLD_RISKS.contains(&"low"));
        assert!(AUTO_FOLD_RISKS.contains(&"normal"));
        assert!(!AUTO_FOLD_RISKS.contains(&"important"));
        assert!(!AUTO_FOLD_RISKS.contains(&"high"));
    }

    #[test]
    fn fold_findings_flattens_the_unmet_criteria_and_the_extras() {
        let findings = serde_json::json!({
            "criteria": [
                {"id": "AC1", "verdict": "unmet", "evidence": ["E002"],
                 "note": "the retry never fires"},
                {"id": "AC2", "verdict": "met", "evidence": ["E001"]},
                {"id": "AC3", "verdict": "unmet", "evidence": []}
            ],
            "extra_findings": [
                {"severity": "major", "note": "the test asserts a constant", "evidence": ["E004"]},
                {"severity": "", "note": "a stray TODO", "evidence": []}
            ]
        })
        .to_string();

        let (unmet, extra) = fold_findings(&findings, &[]);
        assert_eq!(unmet.len(), 2, "only the unmet criteria travel: {unmet:?}");
        assert_eq!(unmet[0], "AC1: the retry never fires (evidence E002)");
        assert_eq!(
            unmet[1], "AC3: not met",
            "an unmet criterion with no note still has to name itself"
        );
        assert_eq!(extra.len(), 2);
        assert_eq!(extra[0], "major: the test asserts a constant (evidence E004)");
        assert_eq!(extra[1], "finding: a stray TODO");

        // junk folds to nothing rather than panicking
        assert_eq!(fold_findings("not json", &[]), (Vec::new(), Vec::new()));
        assert_eq!(fold_findings("{}", &[]), (Vec::new(), Vec::new()));
    }

    #[test]
    fn fold_source_findings_maps_claims_and_keeps_unreachable_before_document_extras() {
        let result = serde_json::json!({"claims":[{"kind":"completion"},
            {"kind":"research", "text":"retry", "sources":[
                {"url":"file:///good", "quote":"A"},
                {"url":"file:///bad", "quote":"B"},
                {"url":"file:///dead", "quote":"C"}]}]});
        let sources = crate::review_json::sources_to_check(&result);
        let findings = serde_json::json!({"sources":[
            {"id":"S1", "url":"file:///good", "verdict":"supported", "reason":""},
            {"id":"S2", "url":"file:///bad", "verdict":"not_supported", "reason":"quote absent"},
            {"id":"S3", "url":"file:///dead", "verdict":"unreachable", "reason":"not found"}],
            "extra_findings":[{"severity":"minor", "note":"report typo"}]}).to_string();
        let (unmet, extra) = fold_findings(&findings, &sources);
        assert_eq!(unmet, vec!["claim 2 (S2): retry — file:///bad: quote absent"]);
        assert_eq!(extra, vec!["unreachable: claim 2 (S3): retry — file:///dead: not found; replace the source", "minor: report typo"]);
        let (unmet, extra) = fold_findings(&findings, &[]);
        assert_eq!(unmet, vec!["S2: file:///bad: quote absent"]);
        assert_eq!(extra[0], "unreachable: S3: file:///dead: not found; replace the source");
    }

    #[test]
    fn rework_spec_adds_the_addendum_and_leaves_the_rest_alone() {
        let task_spec =
            r#"{"goal":"g","acceptance_criteria":["AC1: x"],"risk":"normal","custom":42}"#;
        let addendum = crate::spec::ReworkAddendum {
            attempt: 2,
            previous_run: "01RUNPREV".to_string(),
            previous_attempt: 1,
            previous_candidate: "cafebabe".to_string(),
            unmet: vec!["AC1: not met".to_string()],
            extra_findings: vec![],
        };
        let out = rework_spec(task_spec, &addendum).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["goal"], "g");
        assert_eq!(v["custom"], 42, "unknown keys must survive verbatim");
        assert_eq!(v["rework"]["attempt"], 2);
        assert_eq!(v["rework"]["previous_run"], "01RUNPREV");
        assert_eq!(v["rework"]["previous_candidate"], "cafebabe");
        assert_eq!(v["rework"]["unmet"], serde_json::json!(["AC1: not met"]));

        // the result is still a valid task spec, and the addendum reads back
        assert!(crate::spec::parse_spec(&out).is_ok());
        assert_eq!(crate::spec::parse_rework(&out).unwrap().attempt, 2);

        // a non-object spec is refused rather than silently replaced
        assert!(rework_spec("[1,2,3]", &addendum).is_err());
        assert!(rework_spec("not json", &addendum).is_err());
    }

    #[test]
    fn reroute_spec_adds_the_reason_verbatim_and_leaves_the_rest_alone() {
        let task_spec = r#"{"goal":"g","acceptance_criteria":["AC1: x"],"risk":"low"}"#;
        let addendum = crate::spec::RerouteAddendum {
            previous_run: "01RUNPREV".to_string(),
            reason: "claim 2 (test_result) cites no evidence".to_string(),
            rule: Some(2),
        };
        let out = reroute_spec(task_spec, &addendum).unwrap();
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["goal"], "g");
        assert_eq!(v["reroute"]["reason"], "claim 2 (test_result) cites no evidence");
        assert_eq!(v["reroute"]["rule"], 2);
        assert_eq!(v["reroute"]["previous_run"], "01RUNPREV");
        assert!(v.get("rework").is_none());
        assert!(crate::spec::parse_spec(&out).is_ok());
        assert_eq!(crate::spec::parse_reroute(&out).unwrap(), addendum);
        assert!(reroute_spec("[1,2,3]", &addendum).is_err());
    }

    fn reroute_db_with_run() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        state::insert_task(
            &conn,
            &state::Task {
                task_id: "T1".into(),
                title: "t".into(),
                spec: r#"{"goal":"g","acceptance_criteria":["AC1: x"],"risk":"low"}"#.into(),
                risk: "low".into(),
                status: "unverified".into(),
                base_commit: "abc".into(),
                created_at: "2026-09-27T00:00:00Z".into(),
                updated_at: "2026-09-27T00:00:00Z".into(),
            },
        )
        .unwrap();
        state::insert_run(
            &conn,
            &Run {
                run_id: "R1".into(),
                task_id: "T1".into(),
                attempt: 1,
                role: crate::model::role::IMPLEMENTER.into(),
                provider: "anthropic".into(),
                account: "claude-b".into(),
                model: "sonnet".into(),
                harness_session_id: None,
                config_dir: "/cfg".into(),
                allowed_tools: "Bash".into(),
                worktree: "/wt".into(),
                base_commit: "abc".into(),
                candidate_commit: None,
                spec: "{}".into(),
                status: "completed".into(),
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
                run_dir: "/rd".into(),
                error_text: None,
                harness_bin: None,
            },
        )
        .unwrap();
        conn
    }

    fn reroute_event(conn: &rusqlite::Connection, kind: &str, detail: serde_json::Value) {
        state::append_event(
            conn,
            &NewEvent {
                task_id: Some("T1".into()),
                run_id: Some("R1".into()),
                event_type: kind.into(),
                detail,
            },
        )
        .unwrap();
    }

    fn rejection(conn: &rusqlite::Connection) {
        reroute_event(
            conn,
            event_type::RESULT_INVALID,
            serde_json::json!({"reason": "claim 2 cites no evidence", "rule": 2}),
        );
    }

    fn acceptance(conn: &rusqlite::Connection) {
        reroute_event(conn, event_type::RESULT_INGESTED, serde_json::json!({}));
    }

    #[test]
    fn reroute_quotes_a_rejection_that_no_later_ingest_superseded() {
        let conn = reroute_db_with_run();
        acceptance(&conn);
        rejection(&conn);
        let addendum = reroute_addendum(&conn, "T1").unwrap().expect("reason is current");
        assert_eq!(addendum.reason, "claim 2 cites no evidence");
        let spec = reroute_spec(r#"{"goal":"g"}"#, &addendum).unwrap();
        assert!(spec.contains("claim 2 cites no evidence"));
    }

    #[test]
    fn reroute_does_not_quote_a_rejection_a_later_ingest_superseded() {
        let conn = reroute_db_with_run();
        rejection(&conn);
        acceptance(&conn);
        assert_eq!(reroute_addendum(&conn, "T1").unwrap(), None);
    }

    fn reject_verdict_event(conn: &rusqlite::Connection, task_id: &str) {
        state::append_event(
            conn,
            &NewEvent {
                task_id: Some(task_id.into()),
                run_id: Some("R1".into()),
                event_type: event_type::VERDICT.to_string(),
                detail: serde_json::json!({"verdict": "REJECT"}),
            },
        )
        .unwrap();
    }

    fn spec_updated_event(conn: &rusqlite::Connection, task_id: &str) {
        state::append_event(
            conn,
            &NewEvent {
                task_id: Some(task_id.into()),
                run_id: None,
                event_type: event_type::SPEC_UPDATED.to_string(),
                detail: serde_json::json!({}),
            },
        )
        .unwrap();
    }

    /// trial 20: an important/high rework may proceed once the Supervisor's
    /// `spec_updated` postdates the REJECT `verdict` it answers - not merely
    /// exists somewhere in the task's history.
    #[test]
    fn spec_rewritten_since_reject_needs_the_update_to_come_after_the_reject() {
        let conn = reroute_db_with_run();
        assert!(
            !spec_rewritten_since_reject(&conn, "T1").unwrap(),
            "no REJECT at all yet"
        );

        reject_verdict_event(&conn, "T1");
        assert!(
            !spec_rewritten_since_reject(&conn, "T1").unwrap(),
            "a REJECT with no update-spec since it must still refuse"
        );

        spec_updated_event(&conn, "T1");
        assert!(
            spec_rewritten_since_reject(&conn, "T1").unwrap(),
            "the update-spec came after the REJECT"
        );

        // a second REJECT after that update-spec must refuse again
        reject_verdict_event(&conn, "T1");
        assert!(
            !spec_rewritten_since_reject(&conn, "T1").unwrap(),
            "the newest REJECT postdates the only update-spec so far"
        );

        spec_updated_event(&conn, "T1");
        assert!(
            spec_rewritten_since_reject(&conn, "T1").unwrap(),
            "a fresh update-spec answers the second REJECT too"
        );
    }

    #[test]
    fn stuck_finding_normalizes_case_whitespace_and_evidence_codes() {
        assert_eq!(
            normalize_finding("The retry never fires (evidence E004)."),
            normalize_finding("the retry never fires  (evidence e011)")
        );
        assert_ne!(
            normalize_finding("the retry never fires"),
            normalize_finding("the retry fires twice")
        );
    }

    fn repeated_event_db() -> rusqlite::Connection {
        reroute_db_with_run()
    }

    fn add_reject(conn: &rusqlite::Connection, n: usize, note: &str) {
        add_reject_with_criterion(conn, n, note, "AC1");
    }

    fn add_reject_with_criterion(
        conn: &rusqlite::Connection,
        n: usize,
        note: &str,
        criterion: &str,
    ) {
        let review_id = format!("V{n}");
        state::insert_review(conn, &state::Review {
            review_id: review_id.clone(), target_run_id: "R1".into(), reviewer_run_id: None,
            verdict: "REJECT".into(),
            findings: serde_json::json!({"criteria": [{"id":criterion,"verdict":"unmet"}], "extra_findings":[{"note":note,"severity":"important"}]}).to_string(),
            created_at: format!("2026-09-27T00:00:0{n}Z"),
        }).unwrap();
        reroute_event(conn, event_type::VERDICT, serde_json::json!({"verdict":"REJECT","review_id":review_id,"criteria":[{"id":criterion,"verdict":"unmet"}]}));
    }

    #[test]
    fn stuck_reason_requires_three_and_returns_criterion() {
        let conn = repeated_event_db();
        add_reject(&conn, 1, "same");
        add_reject(&conn, 2, "same");
        assert_eq!(stuck_reason(&conn, "T1").unwrap(), None);
        add_reject(&conn, 3, "same");
        assert_eq!(stuck_reason(&conn, "T1").unwrap(), Some(StuckReason::Criterion("AC1".into())));
    }

    #[test]
    fn stuck_reason_rule_zero_does_not_count() {
        let conn = repeated_event_db();
        for _ in 0..3 {
            reroute_event(&conn, event_type::RESULT_INVALID, serde_json::json!({"rule":0}));
        }
        assert_eq!(stuck_reason(&conn, "T1").unwrap(), None);
    }

    #[test]
    fn stuck_reason_two_same_criteria_and_one_different_is_not_stuck() {
        let conn = repeated_event_db();
        add_reject_with_criterion(&conn, 1, "same", "AC1");
        add_reject_with_criterion(&conn, 2, "same", "AC1");
        add_reject_with_criterion(&conn, 3, "different", "AC2");
        assert_eq!(stuck_reason(&conn, "T1").unwrap(), None);
    }

    #[test]
    fn stuck_reason_finding_ignores_evidence_case_and_whitespace() {
        let conn = repeated_event_db();
        add_reject_with_criterion(&conn, 1, "The retry never fires (evidence E004).", "AC1");
        add_reject_with_criterion(&conn, 2, "the retry never fires  (evidence e011)", "AC2");
        add_reject_with_criterion(&conn, 3, " THE   RETRY NEVER FIRES (evidence e099); ", "AC3");
        assert_eq!(
            stuck_reason(&conn, "T1").unwrap(),
            Some(StuckReason::Finding("the retry never fires (evidence".into()))
        );
    }

    #[test]
    fn stuck_reason_spec_updated_resets_the_reject_count() {
        let conn = repeated_event_db();
        add_reject(&conn, 1, "same");
        add_reject(&conn, 2, "same");
        spec_updated_event(&conn, "T1");
        add_reject(&conn, 3, "same");
        add_reject(&conn, 4, "same");
        assert_eq!(stuck_reason(&conn, "T1").unwrap(), None);
        add_reject(&conn, 5, "same");
        assert_eq!(stuck_reason(&conn, "T1").unwrap(), Some(StuckReason::Criterion("AC1".into())));
    }

    /// S3-9 AC5: another provider needs both the label and the harness to
    /// differ; an unknown `from` provider counts as different.
    #[test]
    fn cross_provider_needs_both_the_provider_and_the_harness_to_differ() {
        assert!(is_cross_provider(Some("anthropic"), "claude-code", "openai", "codex"));
        assert!(!is_cross_provider(Some("anthropic"), "claude-code", "anthropic", "claude-code"));
        assert!(!is_cross_provider(Some("anthropic"), "claude-code", "other", "claude-code"));
        assert!(!is_cross_provider(Some(" Anthropic "), "claude-code", "anthropic", "codex"));
        assert!(is_cross_provider(None, "claude-code", "openai", "codex"));
        assert!(!is_cross_provider(None, "codex", "openai", "codex"));
    }

    /// S3-10 AC10: a handoff copies the previous run's `rework` and
    /// `reroute` verbatim, and nothing when it has none.
    #[test]
    fn carry_addenda_copies_rework_and_reroute_only_when_present() {
        let previous = r#"{"goal":"g","rework":{"attempt":2,"unmet":["AC1: x"]},"reroute":{"reason":"r"},"launch":{}}"#;
        let out: serde_json::Value =
            serde_json::from_str(&carry_addenda(r#"{"goal":"g"}"#, previous).unwrap()).unwrap();
        assert_eq!(out["rework"], serde_json::json!({"attempt":2,"unmet":["AC1: x"]}));
        assert_eq!(out["reroute"], serde_json::json!({"reason":"r"}));
        assert!(out.get("launch").is_none());
        assert_eq!(carry_addenda(r#"{"goal":"g"}"#, r#"{"goal":"g"}"#).unwrap(), r#"{"goal":"g"}"#);
        assert_eq!(carry_addenda(r#"{"goal":"g"}"#, "not json").unwrap(), r#"{"goal":"g"}"#);
    }

    #[test]
    fn stuck_reason_repeated_nonzero_rule_returns_rule() {
        let conn = repeated_event_db();
        for _ in 0..3 {
            reroute_event(&conn, event_type::RESULT_INVALID, serde_json::json!({"rule":2}));
        }
        assert_eq!(stuck_reason(&conn, "T1").unwrap(), Some(StuckReason::Rule(2)));
    }
}
