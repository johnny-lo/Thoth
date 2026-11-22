use super::prepare::{PromptAddenda, RunPrep};
use super::review_inputs::ReviewInputs;
use super::settle::log_runner_error;
use crate::adapter::claude_code;
use crate::db::state;
use crate::evidence;
use crate::ids;
use crate::model::event_type;
use crate::spec::TaskSpec;
use anyhow::{anyhow, Result};
use rusqlite::Connection;

/// What the third section of `run_worker_inner` settled about the session and
/// the prompt it wrote to `prompt.txt`.
pub(super) struct BuiltPrompt {
    pub(super) is_resume: bool,
    pub(super) session_id: String,
    pub(super) prompt: String,
}

// The moved statements borrow `&masker`, `&run_dir` and the like as they did when these
// were owned locals; here they are already references, so the `&` is redundant and kept
// so the statements stay word for word the same.
#[allow(clippy::needless_borrow, clippy::needless_borrows_for_generic_args)]
pub(super) fn build_run_prompt(
    conn: &Connection,
    run: &state::Run,
    prep: &RunPrep,
    addenda: PromptAddenda,
    inputs: &ReviewInputs,
) -> Result<BuiltPrompt> {
    let PromptAddenda { rework, reroute, handoff } = addenda;
    let adapter = &prep.adapter;
    let spec = &prep.spec;
    let masker = &prep.masker;
    let profile = &prep.profile;
    let is_codex = prep.is_codex;
    let is_review = prep.is_review;
    let is_spec_review = prep.is_spec_review;
    let research = prep.research;
    let thoth_exe = &prep.thoth_exe;
    let run_dir = &prep.run_dir;
    let prompt_path = &prep.prompt_path;
    let spec_review_mode = inputs.spec_review_mode;
    let code_review_mode = inputs.code_review_mode;
    let confirm_fix_commit = &inputs.confirm_fix_commit;
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
        // REV-3 (AC2): a code review is told which required checks did not
        // run on the run under review, as Thoth works them out now from the
        // task's current spec and that run's verify rows.
        if !research {
            let not_run = (|| -> Result<Vec<crate::required_checks::NotRun>> {
                let task = state::get_task(conn, &run.task_id)?
                    .ok_or_else(|| anyhow!("task {} disappeared", run.task_id))?;
                let target = crate::commands::review::target_run_id_for(conn, &run.run_id)?;
                crate::required_checks::for_run(conn, &run.task_id, &task.spec, target.as_deref())
            })();
            let required = match not_run {
                Ok(list) => crate::review_json::required_checks_prompt_section(
                    &crate::required_checks::lines(&list),
                ),
                Err(e) => {
                    log_runner_error(
                        &run_dir,
                        &format!("working out the required checks not run for the review prompt failed: {e:#}"),
                    );
                    "\n## Required checks not run\nThoth could not work out this list; judge from the evidence index which checks the spec's checks field lists did not run. Thoth still checks your report against the list when it receives it.\n".to_string()
                }
            };
            // MEAS-1 (AC3): the previous REJECT's findings to mark, by the
            // rule Thoth checks the report with when it arrives.
            let prior = (|| -> Result<crate::prior_rejection::Target> {
                let target = crate::commands::review::target_run_id_for(conn, &run.run_id)?
                    .ok_or_else(|| anyhow!("the review has no target run"))?;
                crate::prior_rejection::target_for(conn, &run.task_id, &target)
            })();
            let prior = match prior {
                Ok(t) => Some(t),
                Err(e) => {
                    log_runner_error(
                        &run_dir,
                        &format!("working out the previous rejection for the review prompt failed: {e:#}"),
                    );
                    None
                }
            };
            // REV-3, MEAS-1 and, in edit mode, RFIX-1's section on how to
            // attach a fix.
            prompt.push_str(&crate::adapter::code_review_sections(
                &profile.harness,
                &required,
                prior.as_ref(),
                code_review_mode,
                &mut |text| mask_into(counts, text),
            ));
        } else if code_review_mode == crate::spec_edits::ReviewMode::Edit {
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
            advisory: r.advisory.iter().map(|a| mask_into(counts, a)).collect(),
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
    Ok(BuiltPrompt { is_resume, session_id, prompt })
}
