use super::{
    AuthStatus, CommandPlan, HarnessAdapter, LaunchRequest, ResultEvent, StreamEvent,
};
use crate::spec::TaskSpec;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// brief section 8: the exact text used when resuming an interrupted session.
/// Phase 0 proved an interrupted tool call has no partial result, so the worker
/// is told to re-check state and re-run (01-claude-cli-identity.md:131).
pub const RESUME_PROMPT: &str = "The previous session was interrupted; a tool call may not have completed. Re-check the working tree state and continue the task. Re-run any tool call whose result you did not see.";

pub struct ClaudeCodeAdapter {
    pub bin: String,
}

/// The built-in tools a research task's Claude worker or source checker gets on top of its list
/// (RES-1a). Both are in claude's built-in set that `--tools` whitelists; no
/// MCP server is added (`--strict-mcp-config` stays).
pub const CLAUDE_RESEARCH_TOOLS: &[&str] = &["WebFetch", "WebSearch"];

impl ClaudeCodeAdapter {
    pub fn from_env() -> Self {
        ClaudeCodeAdapter {
            bin: crate::config::claude_bin(),
        }
    }
}

/// brief §4: the worker prompt's evidence contract, verbatim. The wording is
/// fixed because it is the only place a worker learns that citations are
/// mandatory and that inventing codes is not an option.
pub const EVIDENCE_SECTION: &str = "\n## Evidence\nEvery shell command you run is captured automatically. The file .agent/evidence-index.json in this directory lists the captured evidence with codes like E001. When you write .agent/result.json, each claim of kind test_result or doc_reference MUST cite at least one code from that file in its \"evidence\" array. Do not invent codes. If the index is empty when you finish, re-run the relevant command once so it is captured. Run each test, build or check command as its own shell command: do not pipe it into another command (for example | tail or | head) and do not chain other commands after it with ; or &&, because the captured exit code is the last command's.\n";

/// Writes the per-run hook configuration and returns its path.
///
/// Shape copied from the Phase 0 capture's `settings_q1.json`
/// (02-evidence-capture.md:238-252): `hooks` → one group per event → matcher
/// `Bash` → one `command` hook. The command is this very executable's absolute
/// path, quoted, so the hook never depends on `PATH` inside the worker's
/// environment. `serde_json` escapes the backslashes in a Windows path when it
/// serializes the string, which is exactly what Claude Code expects to read
/// back.
// Retired in Task 3: production caller is `runner::run_worker_inner`'s hook
// wiring; also read by
// `write_hooks_settings_produces_the_phase0_shape_with_an_absolute_exe`.
pub fn write_hooks_settings(run_dir: &Path, thoth_exe: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(run_dir)
        .with_context(|| format!("creating {}", run_dir.display()))?;
    let command = format!("\"{}\" hook", thoth_exe.display());
    let group = serde_json::json!([{
        "matcher": "Bash",
        "hooks": [{ "type": "command", "command": command }],
    }]);
    let doc = serde_json::json!({
        "hooks": {
            "PreToolUse": group,
            "PostToolUse": group,
            "PostToolUseFailure": group,
        }
    });
    let path = run_dir.join("hooks-settings.json");
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// P7-1b: the worker prompt's section naming the approved checks `verify`
/// will run, placed before the result.json contract.
pub const CHECKS_HEADING: &str = "\n## Checks the control plane will re-run\n";
pub const CHECKS_NOTE: &str = "Run the tests the spec names before you finish. Your verification_requests are kept for reference only; the control plane runs the checks above.\n";

/// TESTS-1 (AC5): right after `CHECKS_NOTE`, in every worker prompt (Claude,
/// agy and codex). A worker's command has a time limit; a whole suite that
/// outruns it leaves an empty or cut-off evidence row, which a reviewer then
/// counts as missing evidence. The whole-suite evidence is the control plane's
/// `verify` of the checks above, so the worker runs what the spec names, one
/// foreground command each. The sentence names no tool: Thoth serves any
/// language's repo.
pub const SUITE_NOTE: &str = "Evidence for the whole test suite comes from the control plane's verify of the checks above, not from you. Run the tests the spec names, each as its own foreground command, and do not start a whole-suite run that cannot finish within one command's time limit; when the spec asks for the whole suite, run it in parts that each finish, and keep each part's output.\n";

/// LEAN-1: right after the result.json shape. The summary is for what Thoth
/// cannot record by itself; files, commands, exit codes and usage it already
/// has.
pub const SUMMARY_NOTE: &str = "In the summary, say what you intended, its limitations and the decisions you made. Do not reproduce file lists, command logs, exit codes or usage: Thoth already records them.\n";

/// P7-2: the worker prompt's last section. LEAN-1: Thoth works the list of
/// protected changes out from the diff itself, so the section only says what
/// is recorded and that the commander approves it; the worker declares
/// nothing. It lists what the worker cannot work out by itself: the input
/// files of every check (approved or not), the check words, and the repo's
/// extra patterns, all from `protected`.
pub fn protected_section(protected: &crate::protected::Catalogue) -> String {
    let list = |items: &[String]| {
        if items.is_empty() {
            "(none)".to_string()
        } else {
            items.join(", ")
        }
    };
    let mut s = String::from("\n## Protected files\n");
    s.push_str("Thoth records protected changes automatically from the diff between the task's base commit and your final commit: every existing test file, and every check input file listed below, that you modify or delete; every existing Rust source file whose inline test region (from its first #[test], #[tokio::test] or #[cfg(test)] module attribute to the end of the file) you change or delete; and every file you add, modify or delete whose name, up to its first dot, is one of the check words listed below. You do not need to declare these files in .agent/result.json, and a delivery is not refused for leaving them out; a claim of kind protected_change is optional. The commander reads the recorded list and approves each test, inline test and check-word change before a review can pass, so change existing tests only when the spec requires it and say why in your summary. Test files this task added itself, and changes that arrive through a git merge of the main branch, are not listed.\n");
    s.push_str("Test files are recognised by path: anything under a tests/, test/ or __tests__/ directory, and names such as test_*, *_test.rs, *.test.ts, *.spec.ts or conftest.py.\n");
    s.push_str(&format!("Check input files: {}\n", list(&protected.inputs)));
    s.push_str(&format!("Check words: {}\n", list(&protected.stems)));
    if !protected.patterns.is_empty() {
        s.push_str(&format!(
            "Also treated as test files in this repo: {}\n",
            protected.patterns.join(", ")
        ));
    }
    s
}

/// RES-1a: the worker prompt's section for a research task, placed before the
/// result.json contract. Ingest rejects a `research` claim without sources
/// (rule 1) and a completed research delivery without one (rule 7).
pub const RESEARCH_SECTION: &str = "\n## Research claims\nThis is a research task. For every factual claim in your report, add one claim of kind research to .agent/result.json. Its \"text\" is the claim itself. Its \"sources\" lists at least one source; each source has a \"url\" you actually opened (http://, https:// or file://) and a \"quote\": a verbatim excerpt of that page that supports the claim, at most 500 characters, copied exactly and not paraphrased. Its \"evidence\" may be an empty array. Leave any claim you have no source for out of the report. A completed delivery with no research claim is not accepted. For example:\n{\"kind\":\"research\",\"text\":\"...\",\"evidence\":[],\"sources\":[{\"url\":\"https://...\",\"quote\":\"...\"}]}\n";

/// brief section 8: the implementer prompt. The result.json shape is spelled out
/// literally so the worker has no room to improvise field names. `research`
/// (a research task, `spec::is_research_task`) adds `RESEARCH_SECTION`.
#[allow(clippy::too_many_arguments)]
pub fn build_prompt(
    task_id: &str,
    spec: &TaskSpec,
    rework: Option<&crate::spec::ReworkAddendum>,
    reroute: Option<&crate::spec::RerouteAddendum>,
    handoff: Option<&crate::handoff::HandoffRef>,
    checks: &[crate::checks::Check],
    protected: &crate::protected::Catalogue,
    research: bool,
) -> String {
    build_prompt_impl(task_id, spec, rework, reroute, handoff, checks, protected, true, research)
}

/// The codex worker prompt (scope §2.6): byte-for-byte `build_prompt` except
/// it tells the worker not to commit. Codex's sandbox cannot write a linked
/// worktree's own gitdir (E9), so unlike Claude Code it never commits its own
/// work; thoth commits on its behalf once the run exits cleanly.
#[allow(clippy::too_many_arguments)]
pub fn build_codex_prompt(
    task_id: &str,
    spec: &TaskSpec,
    rework: Option<&crate::spec::ReworkAddendum>,
    reroute: Option<&crate::spec::RerouteAddendum>,
    handoff: Option<&crate::handoff::HandoffRef>,
    checks: &[crate::checks::Check],
    protected: &crate::protected::Catalogue,
    research: bool,
) -> String {
    build_prompt_impl(task_id, spec, rework, reroute, handoff, checks, protected, false, research)
}

#[allow(clippy::too_many_arguments)]
fn build_prompt_impl(
    task_id: &str,
    spec: &TaskSpec,
    rework: Option<&crate::spec::ReworkAddendum>,
    reroute: Option<&crate::spec::RerouteAddendum>,
    handoff: Option<&crate::handoff::HandoffRef>,
    checks: &[crate::checks::Check],
    protected: &crate::protected::Catalogue,
    worker_commits: bool,
    research: bool,
) -> String {
    let mut s = String::new();
    s.push_str("You are an implementer working in an isolated git worktree. Work only inside the current directory.\n\n");
    s.push_str("## Goal\n");
    s.push_str(spec.goal.trim());
    s.push('\n');
    s.push_str("## Acceptance criteria\n");
    for ac in &spec.acceptance_criteria {
        s.push_str("- ");
        s.push_str(ac.trim());
        s.push('\n');
    }
    s.push_str("## Constraints\n");
    for c in &spec.constraints {
        s.push_str("- ");
        s.push_str(c.trim());
        s.push('\n');
    }
    s.push_str("## Non-goals\n");
    for c in &spec.non_goals {
        s.push_str("- ");
        s.push_str(c.trim());
        s.push('\n');
    }
    s.push_str("## Excluded alternatives\n");
    for c in &spec.excluded_alternatives {
        s.push_str("- ");
        s.push_str(c.trim());
        s.push('\n');
    }
    // brief §9: a rework keeps the previous attempt's commits but not its
    // conversation, so the findings have to arrive in the prompt.
    if let Some(r) = rework {
        s.push_str("\n## Previous attempt was rejected\n");
        s.push_str(&format!("This is attempt {}. ", r.attempt));
        // R2 (fix round 1): name the attempt the findings and the commits
        // actually came from. It is not always `attempt - 1` - a crashed
        // attempt never completes, so `latest_completed_run_for_role` can
        // reach further back - and when there is no completed previous run at
        // all there is no attempt number to name and no commits to build on.
        if r.previous_attempt > 0 {
            s.push_str(&format!(
                "Attempt {} was reviewed and rejected. ",
                r.previous_attempt
            ));
        } else {
            s.push_str("A previous attempt was reviewed and rejected. ");
        }
        if r.previous_candidate.is_empty() {
            s.push_str("Its commits are not in this worktree, so start from the spec above.\n");
        } else {
            s.push_str("Its commits are already in this worktree: build on them rather than starting over.\n");
        }
        if !r.unmet.is_empty() {
            s.push_str("Unmet acceptance criteria:\n");
            for u in &r.unmet {
                s.push_str("- ");
                s.push_str(u.trim());
                s.push('\n');
            }
        }
        if !r.extra_findings.is_empty() {
            s.push_str("Other findings:\n");
            for e in &r.extra_findings {
                s.push_str("- ");
                s.push_str(e.trim());
                s.push('\n');
            }
        }
        s.push_str("For each finding that changes behaviour, first add or change a test that fails on the code as it is now, and run that test on its own so the failing run is captured; then fix the code and run the test again. Findings that change no behaviour (for example documentation) do not need this.\n");
        s.push_str("Fix exactly these. Do not re-litigate the spec; if you believe a finding is wrong, say so in your summary and fix what you can.\n");
    }
    // A reroute reuses the attempt and the worktree, but the conversation is
    // new, so the reason the last result.json was refused has to be said.
    if let Some(r) = reroute {
        s.push_str("\n## Previous run's result.json was not accepted\n");
        s.push_str(&format!(
            "The previous run ({}) finished, but its .agent/result.json was not accepted. Reason:\n",
            r.previous_run
        ));
        s.push_str("> ");
        s.push_str(r.reason.trim());
        s.push('\n');
        s.push_str("That run's work is already in this worktree, so check and fix rather than start over. Write .agent/result.json again with every test_result and doc_reference claim citing a code from .agent/evidence-index.json.\n");
    }
    // S3-9: a run that stopped early (quota, timeout, crash) left its work in
    // this worktree. The prompt names the handoff file; the file itself is not
    // copied in.
    if let Some(h) = handoff {
        s.push_str("\n## Continuing an interrupted run\n");
        s.push_str(&format!(
            "The previous run ({}) stopped before it finished (reason: {}). Its work is already in this worktree. Read `{}` first, then continue from there rather than starting over.\n",
            h.previous_run,
            h.reason.trim(),
            crate::handoff::PATH
        ));
    }
    // P7-1b: `verify` runs the approved checks, not what the worker declares,
    // so the worker is told which ones before it writes its result.
    s.push_str(CHECKS_HEADING);
    if checks.is_empty() {
        s.push_str("(none approved yet)\n");
    }
    for c in checks {
        s.push_str(&format!("{}: {} ({})\n", c.check_id, c.cmd, c.shell));
    }
    s.push_str(CHECKS_NOTE);
    // TESTS-1: the whole suite is verify's job; the worker runs what the spec
    // names, in parts that finish.
    s.push_str(SUITE_NOTE);
    if research {
        s.push_str(RESEARCH_SECTION);
    }
    s.push('\n');
    if worker_commits {
        s.push_str("When done, commit your work on the current branch, then write the file `.agent/result.json` in the current directory with exactly this shape:\n");
    } else {
        s.push_str("When done, write the file `.agent/result.json` in the current directory with exactly this shape:\n");
    }
    s.push_str("{\"task_id\":\"");
    s.push_str(task_id);
    s.push_str("\",\"status\":\"completed\",\"brief\":\"<short conclusion>\",\"summary\":\"<one paragraph>\",\"claims\":[{\"kind\":\"test_result\",\"text\":\"...\",\"evidence\":[]}]}\n");
    s.push_str(BRIEF_NOTE);
    s.push_str(SUMMARY_NOTE);
    s.push_str("Do not modify files outside this directory. Do not push.\n");
    if !worker_commits {
        s.push_str("Do not commit: leave your changes in the worktree. thoth will commit whatever is there once this run finishes cleanly.\n");
    }
    s.push_str(SINGLE_RUN_NOTE);
    s.push_str(EVIDENCE_SECTION);
    s.push_str(&protected_section(protected));
    s
}

/// BRIEF-1: what the worker puts in result.json's `brief`, which the
/// commander reads first.
pub const BRIEF_NOTE: &str = "`brief` is your short conclusion, which the commander reads first: what you delivered (name the paths of any documents you wrote), the outcome of your checks in one sentence, and any decision you need from the commander. Keep it within 1,000 characters: a longer brief is cut at 1,000 characters and marked as cut, and without a brief the start of your summary is shown instead. Put the full explanation in `summary`. Do not list the files you changed or the commands you ran in either: they are already recorded as evidence.\n";

/// The worker prompt's note that the run is one unattended turn. A worker that
/// starts a command in the background and ends its turn is killed with it, and
/// delivers nothing. Reviewer prompts do not carry it.
pub const SINGLE_RUN_NOTE: &str = "This is a single unattended run: when you end your turn the process exits, and anything still running in the background is killed. Do not start background commands; run every command in the foreground. Finish everything - your checks, the commit if you make one, and .agent/result.json - before you end your turn.\n";

/// brief §4: the reviewer prompt.
///
/// Everything v6 §4 says a reviewer must be given, in one place: the spec it is
/// judging against, the *whole* evidence index (§2.3 - it reads the index, not
/// the logs), the code of the candidate diff so the must-read is a concrete
/// command rather than an instruction, and the exact output shape. The thoth
/// path is absolute for the same reason the hook command is: a worker's `PATH`
/// is not ours to assume.
pub fn build_review_prompt(
    task_id: &str,
    spec: &TaskSpec,
    criterion_ids: &[String],
    thoth_exe: &str,
    diff_code: Option<&str>,
    index: &[crate::evidence::IndexRow],
    protected_changes: &[String],
) -> String {
    let mut s = String::new();
    s.push_str("You are an independent reviewer. The current directory is a read-only checkout of the candidate commit. You have no Write or Edit tool, and you must not modify any file here except .agent/review.json.\n\n");
    s.push_str("## Goal under review\n");
    s.push_str(spec.goal.trim());
    s.push_str("\n\n## Acceptance criteria\n");
    for (id, ac) in criterion_ids.iter().zip(spec.acceptance_criteria.iter()) {
        s.push_str(id);
        s.push_str(" — ");
        s.push_str(ac.trim());
        s.push('\n');
    }
    s.push_str("\n## Constraints\n");
    for c in &spec.constraints {
        s.push_str("- ");
        s.push_str(c.trim());
        s.push('\n');
    }
    s.push_str("## Non-goals\n");
    for c in &spec.non_goals {
        s.push_str("- ");
        s.push_str(c.trim());
        s.push('\n');
    }
    s.push_str("## Excluded alternatives\n");
    for c in &spec.excluded_alternatives {
        s.push_str("- ");
        s.push_str(c.trim());
        s.push('\n');
    }

    s.push_str("\n## Evidence index\n");
    if index.is_empty() {
        s.push_str("(the control plane captured no evidence for this task)\n");
    } else {
        for r in index {
            s.push_str(&format!(
                "{}  {:<10}  exit={}  {}\n",
                r.code,
                r.kind,
                r.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                r.label
            ));
        }
    }
    s.push_str("\nThe index is also on disk at .agent/evidence-index.json. To read the bytes behind a code:\n");
    s.push_str(&format!("\"{thoth_exe}\" evidence show E001 --json\n"));

    s.push_str("\n## Rules\n");
    match diff_code {
        Some(code) => s.push_str(&format!(
            "1. Read the candidate diff before anything else: \"{thoth_exe}\" evidence show {code} --bytes 200000. It includes the test files. Changing a test so it passes is the classic escape route, so if the tests moved, say what moved and why it does or does not matter.\n"
        )),
        None => s.push_str(
            "1. No diff evidence was captured for this candidate. Record that as an extra finding and judge what you can from the worktree itself.\n",
        ),
    }
    // P7-2: the control plane's own list of protected files the candidate
    // changed. Whether a change weakened a check is the reviewer's call, so
    // the list comes with the instruction to make it, file by file.
    if !protected_changes.is_empty() {
        s.push_str("   Protected files this candidate changed (existing test files, check input files, existing Rust source files whose inline test region changed, and files named like a word of a check's command), as worked out by the control plane from the diff:\n");
        for line in protected_changes {
            s.push_str(&format!("   - {line}\n"));
        }
        s.push_str("   For each file in this list, decide whether the change weakened a check (an assertion removed or loosened, a test deleted or skipped, a check's input altered, a file that could run in place of a check's program), and say what you decided, file by file, in your findings.\n");
    }
    s.push_str("2. Judge every acceptance criterion separately as met or unmet, and cite at least one evidence code for each judgement. Do not invent codes.\n");
    s.push_str("3. Do not trust the implementer's summary. A claim is not evidence.\n");
    s.push_str("4. Do not modify any file in this directory except .agent/review.json.\n");
    s.push_str("5. PASS only if every criterion is met. REJECT when a criterion is unmet. UNVERIFIED when the evidence available cannot settle it.\n");
    s.push_str(&format!("6. {}\n", crate::review_json::INTERFACE_TEST_RULE));
    // TESTS-1 (AC6): the implementer no longer runs the whole suite; the
    // reviewer reads the control plane's verify rows for it.
    s.push_str(&format!("7. {}\n", crate::review_json::SUITE_EVIDENCE_RULE));

    s.push_str("\n## Output\n");
    s.push_str("You have no Write tool, so create the file with a shell redirect (a Bash heredoc works). Write `.agent/review.json` in the current directory with exactly this shape:\n");
    s.push_str("{\"verdict\":\"PASS\",\"findings\":{\"criteria\":[{\"id\":\"");
    s.push_str(criterion_ids.first().map(|s| s.as_str()).unwrap_or("AC1"));
    s.push_str("\",\"verdict\":\"met\",\"evidence\":[\"E001\"],\"note\":\"optional\"}],\"extra_findings\":[{\"severity\":\"minor\",\"note\":\"...\",\"evidence\":[\"E001\"],\"fix_size\":\"S\",\"category\":\"docs\"}]}}\n");
    s.push_str(&format!(
        "Every id in \"criteria\" must be one of: {}. Judge each exactly once. An unmet criterion needs an evidence code or a note saying what is missing.\n",
        criterion_ids.join(", ")
    ));
    s.push_str(&crate::review_json::finding_labels_prompt());
    s.push_str("Leave both out on a met criterion.\n");
    s.push_str(&format!("This review is for task {task_id}.\n"));
    s
}

fn s(v: &serde_json::Value) -> Option<String> {
    v.as_str().map(|x| x.to_string())
}

/// Every key of `rate_limit_info.unifiedWindows`, named as the harness
/// wrote it; a key without a numeric `utilization` is skipped, and anything
/// that is not an object (absent included) yields no windows.
fn unified_windows(v: &serde_json::Value) -> Vec<super::RateLimitWindow> {
    v.as_object()
        .into_iter()
        .flatten()
        .filter_map(|(name, w)| {
            Some(super::RateLimitWindow {
                name: name.clone(),
                utilization: w["utilization"].as_f64()?,
                resets_at: w["resetsAt"].as_i64(),
            })
        })
        .collect()
}

impl HarnessAdapter for ClaudeCodeAdapter {
    fn build_command(&self, req: &LaunchRequest<'_>) -> CommandPlan {
        // W3: the prompt goes to stdin, not argv - Windows caps a command
        // line at 32,767 UTF-16 units. `claude -p` with no positional prompt
        // reads stdin to EOF (measured 2026-10-06, fresh and `--resume`).
        let mut args: Vec<String> = vec!["-p".to_string()];
        match req.resume_session_id {
            Some(sid) => {
                args.push("--resume".to_string());
                args.push(sid.to_string());
            }
            None => {
                args.push("--session-id".to_string());
                args.push(req.session_id.to_string());
            }
        }
        args.push("--model".to_string());
        args.push(req.model.to_string());
        args.extend(super::effort::launch_args("claude-code", req.effort.as_deref()));
        args.push("--output-format".to_string());
        args.push("stream-json".to_string());
        args.push("--verbose".to_string());
        // #25 (final review): clap's own default for --allowed-tools is
        // non-empty, so only an explicit `--allowed-tools ""` reaches here -
        // and the real harness's behaviour with an empty `--allowedTools ""`
        // is unverified, so skip the flag entirely rather than pass it.
        if !req.allowed_tools.is_empty() {
            args.push("--allowedTools".to_string());
            args.push(req.allowed_tools.to_string());
            // `--allowedTools` only auto-approves; `--tools` is the real
            // whitelist of built-in tools (measured 2026-09-26, claude
            // 2.1.283). Same list, same empty-skip rule.
            args.push("--tools".to_string());
            args.push(req.allowed_tools.to_string());
        }
        // With no `--mcp-config`, this loads no MCP servers at all, so the
        // account's claude.ai connectors are out of the worker's reach.
        args.push("--strict-mcp-config".to_string());
        // Phase 0 02-evidence-capture.md:22 measured that `--settings` really
        // does load `hooks`, and that it *merges* with user-level settings
        // rather than replacing them. Appended last so the flags Phase 1's
        // tests pin keep their positions.
        if let Some(settings) = &req.settings_path {
            args.push("--settings".to_string());
            args.push(settings.display().to_string());
        }
        let mut env = vec![(
            "CLAUDE_CONFIG_DIR".to_string(),
            req.config_dir.to_string(),
        )];
        env.extend(req.extra_env.iter().cloned());
        CommandPlan {
            program: self.bin.clone(),
            args,
            env,
            cwd: req.cwd.clone(),
            stdin: Some(req.prompt.clone()),
        }
    }

    /// `--allowed-tools` verbatim; a research task's worker also gets
    /// `CLAUDE_RESEARCH_TOOLS`, which reach `--allowedTools` and `--tools`
    /// through `build_command` like every other tool.
    fn implementer_allowed_tools(&self, given: &str, research: bool) -> String {
        if research {
            super::with_tools(given, CLAUDE_RESEARCH_TOOLS)
        } else {
            given.to_string()
        }
    }

    fn reviewer_allowed_tools(&self, default: &str, research: bool) -> String {
        if research {
            super::with_tools(default, CLAUDE_RESEARCH_TOOLS)
        } else {
            default.to_string()
        }
    }

    fn auth_command(&self, config_dir: &str, cwd: &Path) -> CommandPlan {
        CommandPlan {
            program: self.bin.clone(),
            args: vec![
                "auth".to_string(),
                "status".to_string(),
                "--json".to_string(),
            ],
            env: vec![(
                "CLAUDE_CONFIG_DIR".to_string(),
                config_dir.to_string(),
            )],
            cwd: cwd.to_path_buf(),
            stdin: None,
        }
    }

    fn login_command(&self, config_dir: &str, cwd: &Path) -> CommandPlan {
        // Bare `claude`: its first screen logs in (or `/login` switches).
        CommandPlan {
            program: self.bin.clone(),
            args: vec![],
            env: vec![("CLAUDE_CONFIG_DIR".to_string(), config_dir.to_string())],
            cwd: cwd.to_path_buf(),
            stdin: None,
        }
    }

    fn parse_auth_status(&self, stdout: &str) -> Result<AuthStatus> {
        let v: serde_json::Value = serde_json::from_str(stdout.trim())
            .context("claude auth status --json did not return JSON")?;
        Ok(AuthStatus {
            logged_in: v["loggedIn"].as_bool().unwrap_or(false),
            // Only an explicit `"loggedIn": false` is the CLI saying so (FC-1).
            signed_out: v["loggedIn"].as_bool() == Some(false),
            email: s(&v["email"]),
            subscription_type: s(&v["subscriptionType"]),
            config_directory: s(&v["configDirectory"]),
            // `auth status` says nothing about models (MODELS-1).
            models: None,
        })
    }

    fn parse_stream_line(&self, line: &str) -> StreamEvent {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return StreamEvent::Unparsable {
                    raw_len: line.len(),
                }
            }
        };
        let ty = v["type"].as_str().unwrap_or("");
        let subtype = v["subtype"].as_str();
        match (ty, subtype) {
            ("system", Some("init")) => StreamEvent::Init {
                session_id: s(&v["session_id"]),
                model: s(&v["model"]),
                cwd: s(&v["cwd"]),
                tools: None,
            },
            ("system", Some("api_retry")) => StreamEvent::ApiRetry {
                attempt: v["attempt"].as_i64(),
                max_retries: v["max_retries"].as_i64(),
                error: s(&v["error"]),
                error_status: v["error_status"].as_i64(),
            },
            ("rate_limit_event", _) => StreamEvent::RateLimitWarning {
                rate_limit_type: s(&v["rate_limit_info"]["rateLimitType"]),
                utilization: v["rate_limit_info"]["unifiedWindows"]["five_hour"]["utilization"]
                    .as_f64(),
                resets_at: v["rate_limit_info"]["resetsAt"].as_i64(),
                windows: unified_windows(&v["rate_limit_info"]["unifiedWindows"]),
            },
            ("assistant", _) => StreamEvent::Assistant {
                session_id: s(&v["session_id"]),
            },
            ("result", _) => {
                let api_error_status = match &v["api_error_status"] {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(x) => Some(x.clone()),
                    other => Some(other.to_string()),
                };
                let usage = &v["usage"];
                let input_tokens = usage["input_tokens"].as_i64();
                let output_tokens = usage["output_tokens"].as_i64();
                let cache_read_tokens = usage["cache_read_input_tokens"].as_i64();
                let cache_write_tokens = usage["cache_creation_input_tokens"].as_i64();
                // Anthropic's `input_tokens` holds neither cache kind and
                // `output_tokens` already holds the thinking tokens.
                let total_tokens = if input_tokens.is_none() && output_tokens.is_none() {
                    None
                } else {
                    Some(
                        input_tokens.unwrap_or(0)
                            + cache_read_tokens.unwrap_or(0)
                            + cache_write_tokens.unwrap_or(0)
                            + output_tokens.unwrap_or(0),
                    )
                };
                StreamEvent::Result(ResultEvent {
                    session_id: s(&v["session_id"]),
                    is_error: v["is_error"].as_bool().unwrap_or(false),
                    subtype: subtype.map(|x| x.to_string()),
                    text: s(&v["result"]),
                    api_error_status,
                    // MODELS-1: the structured error names, when the line
                    // has them (a real credits_required line carries both).
                    api_error: s(&v["api_error"]),
                    api_error_code: s(&v["api_error_code"]),
                    input_tokens,
                    output_tokens,
                    cache_read_tokens,
                    cache_write_tokens,
                    reasoning_tokens: usage["output_tokens_details"]["thinking_tokens"].as_i64(),
                    total_tokens,
                    // A result's usage covers only the turn that produced it.
                    usage_cumulative: false,
                    model_usage: v["modelUsage"].as_object().map(|m| {
                        // serde_json's map is sorted by key already; the
                        // explicit sort keeps that from being an accident.
                        let mut rows: Vec<(String, i64)> = m
                            .iter()
                            .map(|(k, u)| (k.clone(), u["outputTokens"].as_i64().unwrap_or(0)))
                            .collect();
                        rows.sort();
                        rows
                    }),
                    subagents_spawned: v["subagent_stats"]["spawned"].as_i64(),
                    // `quota` stays false: the Claude Code parser never sets it.
                    ..Default::default()
                })
            }
            (other, Some(sub)) => StreamEvent::Other {
                kind: format!("{other}/{sub}"),
            },
            (other, None) => StreamEvent::Other {
                kind: other.to_string(),
            },
        }
    }

    /// brief §4: a worker run's commands come through `thoth hook`.
    fn hooks_through_inbox(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::TaskSpec;

    const SAMPLE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stream-sample.jsonl"
    ));
    /// Constructed from docs/phase0/03-process-and-worktree.md:105-119, NOT from a
    /// real sample: the Phase 0 probes never triggered a real 429.
    const SYNTHETIC: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stream-synthetic.jsonl"
    ));

    fn lines(s: &str) -> Vec<&str> {
        s.lines().filter(|l| !l.trim().is_empty()).collect()
    }

    fn adapter() -> ClaudeCodeAdapter {
        ClaudeCodeAdapter { bin: "claude".to_string() }
    }

    fn demo_spec() -> TaskSpec {
        TaskSpec {
            goal: "Add a retry to the uploader".to_string(),
            acceptance_criteria: vec![
                "AC1: retries three times".to_string(),
                "AC2: logs each attempt".to_string(),
            ],
            constraints: vec!["no new dependencies".to_string()],
            non_goals: vec!["rewriting the transport".to_string()],
            excluded_alternatives: vec!["backoff crate - too heavy".to_string()],
            risk: "normal".to_string(),
            checks: vec![],
        }
    }

    fn result_of(line: &str) -> ResultEvent {
        match adapter().parse_stream_line(line) {
            StreamEvent::Result(r) => r,
            other => panic!("not a result: {other:?}"),
        }
    }

    // DT-1 u1: every token kind of a result line.
    #[test]
    fn a_result_line_carries_every_token_kind_and_a_total_without_double_counting() {
        let r = result_of(lines(SAMPLE)[3]);
        assert_eq!(r.input_tokens, Some(34));
        assert_eq!(r.cache_read_tokens, Some(202655));
        assert_eq!(r.cache_write_tokens, Some(1840));
        assert_eq!(r.output_tokens, Some(610));
        assert_eq!(r.reasoning_tokens, Some(226));
        assert_eq!(r.total_tokens, Some(205139));
        assert!(!r.usage_cumulative, "a Claude result covers only its own turn");

        let r = result_of(lines(SYNTHETIC)[1]);
        assert_eq!(r.input_tokens, Some(12));
        assert_eq!(r.output_tokens, Some(0));
        assert_eq!(r.cache_read_tokens, None);
        assert_eq!(r.cache_write_tokens, None);
        assert_eq!(r.reasoning_tokens, None);
        assert_eq!(r.total_tokens, Some(12));
        assert!(!r.usage_cumulative);

        // no usage at all: no total either
        let r = result_of(r#"{"type":"result","subtype":"success","is_error":false,"result":"x"}"#);
        assert_eq!(r.input_tokens, None);
        assert_eq!(r.total_tokens, None);
    }

    #[test]
    fn the_real_sample_has_the_three_line_kinds_we_rely_on() {
        let ls = lines(SAMPLE);
        assert_eq!(ls.len(), 4, "fixture must have exactly 4 lines");
        let a = adapter();
        assert!(matches!(a.parse_stream_line(ls[0]), StreamEvent::Init { .. }));
        assert!(matches!(a.parse_stream_line(ls[1]), StreamEvent::RateLimitWarning { .. }));
        assert!(matches!(a.parse_stream_line(ls[2]), StreamEvent::Assistant { .. }));
        assert!(matches!(a.parse_stream_line(ls[3]), StreamEvent::Result(_)));
    }

    #[test]
    fn init_yields_the_session_id_model_and_cwd() {
        match adapter().parse_stream_line(lines(SAMPLE)[0]) {
            StreamEvent::Init { session_id, model, cwd, tools } => {
                assert_eq!(session_id.as_deref(), Some("00000000-0000-4000-8000-000000000101"));
                assert_eq!(model.as_deref(), Some("claude-haiku-4-5-20251001"));
                assert!(cwd.unwrap().ends_with("ws"));
                // AGY-1: only agy reports a tool list
                assert_eq!(tools, None);
            }
            other => panic!("expected Init, got {other:?}"),
        }
    }

    #[test]
    fn rate_limit_event_yields_the_five_hour_utilization() {
        match adapter().parse_stream_line(lines(SAMPLE)[1]) {
            StreamEvent::RateLimitWarning { rate_limit_type, utilization, resets_at, windows: _ } => {
                assert_eq!(rate_limit_type.as_deref(), Some("five_hour"));
                assert_eq!(utilization, Some(0.07));
                assert_eq!(resets_at, Some(1789381800));
            }
            other => panic!("expected RateLimitWarning, got {other:?}"),
        }
    }

    /// S3-12 AC1: the real sample's `unifiedWindows` holds exactly two
    /// windows, and both come through with their own reset times.
    #[test]
    fn rate_limit_event_yields_every_unified_window() {
        match adapter().parse_stream_line(lines(SAMPLE)[1]) {
            StreamEvent::RateLimitWarning { windows, .. } => {
                assert_eq!(windows.len(), 2, "{windows:?}");
                let five = windows.iter().find(|w| w.name == "five_hour").expect("five_hour");
                assert_eq!(five.utilization, 0.07);
                assert_eq!(five.resets_at, Some(1789381800));
                let seven = windows.iter().find(|w| w.name == "seven_day").expect("seven_day");
                assert_eq!(seven.utilization, 0.02);
                assert_eq!(seven.resets_at, Some(1789963200));
            }
            other => panic!("expected RateLimitWarning, got {other:?}"),
        }
    }

    /// S3-12 AC1's rules: a key's name is kept as is (not limited to
    /// five_hour and seven_day), a key without `utilization` is skipped, a
    /// missing `resetsAt` is None, and no `unifiedWindows` means no windows.
    #[test]
    fn unified_windows_keep_any_name_skip_a_key_without_utilization_and_may_be_empty() {
        let line = r#"{"type":"rate_limit_event","rate_limit_info":{"rateLimitType":"seven_day_opus","resetsAt":5,"unifiedWindows":{"seven_day_opus":{"utilization":0.5,"resetsAt":5},"no_util":{"resetsAt":6},"no_reset":{"utilization":0.25}}}}"#;
        match adapter().parse_stream_line(line) {
            StreamEvent::RateLimitWarning { mut windows, .. } => {
                windows.sort_by(|a, b| a.name.cmp(&b.name));
                assert_eq!(
                    windows,
                    vec![
                        crate::adapter::RateLimitWindow {
                            name: "no_reset".to_string(),
                            utilization: 0.25,
                            resets_at: None,
                        },
                        crate::adapter::RateLimitWindow {
                            name: "seven_day_opus".to_string(),
                            utilization: 0.5,
                            resets_at: Some(5),
                        },
                    ]
                );
            }
            other => panic!("expected RateLimitWarning, got {other:?}"),
        }
        let bare = r#"{"type":"rate_limit_event","rate_limit_info":{"rateLimitType":"five_hour","resetsAt":5}}"#;
        match adapter().parse_stream_line(bare) {
            StreamEvent::RateLimitWarning { windows, .. } => assert!(windows.is_empty(), "{windows:?}"),
            other => panic!("expected RateLimitWarning, got {other:?}"),
        }
    }

    #[test]
    fn the_success_result_yields_tokens_and_no_error() {
        match adapter().parse_stream_line(lines(SAMPLE)[3]) {
            StreamEvent::Result(r) => {
                assert!(!r.is_error);
                assert_eq!(r.subtype.as_deref(), Some("success"));
                assert_eq!(r.input_tokens, Some(34));
                assert_eq!(r.output_tokens, Some(610));
                assert_eq!(r.api_error_status, None);
                assert_eq!(r.session_id.as_deref(), Some("00000000-0000-4000-8000-000000000101"));
                assert!(r.text.unwrap().starts_with("DONE"));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn api_retry_and_the_error_result_parse_from_the_synthetic_lines() {
        let ls = lines(SYNTHETIC);
        assert_eq!(ls.len(), 2);
        match adapter().parse_stream_line(ls[0]) {
            StreamEvent::ApiRetry { attempt, max_retries, error, error_status } => {
                assert_eq!(attempt, Some(1));
                assert_eq!(max_retries, Some(10));
                assert_eq!(error.as_deref(), Some("rate_limit"));
                assert_eq!(error_status, Some(429));
            }
            other => panic!("expected ApiRetry, got {other:?}"),
        }
        match adapter().parse_stream_line(ls[1]) {
            StreamEvent::Result(r) => {
                assert!(r.is_error);
                assert_eq!(r.api_error_status.as_deref(), Some("429"));
                let text = r.text.unwrap();
                assert!(crate::adapter::looks_like_quota(&text), "text was: {text}");
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn unknown_and_broken_lines_do_not_panic() {
        let a = adapter();
        match a.parse_stream_line(r#"{"type":"user","message":{}}"#) {
            StreamEvent::Other { kind } => assert_eq!(kind, "user"),
            other => panic!("expected Other, got {other:?}"),
        }
        match a.parse_stream_line(r#"{"type":"system","subtype":"thinking_tokens"}"#) {
            StreamEvent::Other { kind } => assert_eq!(kind, "system/thinking_tokens"),
            other => panic!("expected Other, got {other:?}"),
        }
        // "half a line {" is 13 bytes; `raw_len` is `line.len()`, i.e. bytes
        match a.parse_stream_line("half a line {") {
            StreamEvent::Unparsable { raw_len } => assert_eq!(raw_len, 13),
            other => panic!("expected Unparsable, got {other:?}"),
        }
    }

    #[test]
    fn build_command_uses_session_id_on_a_fresh_run() {
        let req = LaunchRequest {
            prompt: "PROMPT-BODY".to_string(),
            model: "sonnet",
            session_id: "11111111-2222-3333-4444-555555555555",
            resume_session_id: None,
            allowed_tools: "Bash,Read,Write,Edit,Glob,Grep",
            config_dir: "C:/cfg/claude-b",
            cwd: std::path::PathBuf::from("C:/wt/task-a1"),
            settings_path: None,
            extra_env: Vec::new(),
            review_output: None,
            timeout_seconds: 1800,
            effort: None,
            research: false,
        };
        let plan = adapter().build_command(&req);
        assert_eq!(plan.program, "claude");
        assert_eq!(
            plan.args,
            vec![
                "-p",
                "--session-id", "11111111-2222-3333-4444-555555555555",
                "--model", "sonnet",
                "--output-format", "stream-json",
                "--verbose",
                "--allowedTools", "Bash,Read,Write,Edit,Glob,Grep",
                "--tools", "Bash,Read,Write,Edit,Glob,Grep",
                "--strict-mcp-config",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<String>>()
        );
        assert_eq!(
            plan.env,
            vec![("CLAUDE_CONFIG_DIR".to_string(), "C:/cfg/claude-b".to_string())]
        );
        assert_eq!(plan.cwd, std::path::PathBuf::from("C:/wt/task-a1"));
        assert_eq!(plan.stdin.as_deref(), Some("PROMPT-BODY"));
        assert!(!plan.args.contains(&"--resume".to_string()));
        assert!(!plan.args.contains(&"--settings".to_string()));
    }

    #[test]
    fn build_command_uses_resume_and_the_resume_prompt_on_a_resume() {
        let req = LaunchRequest {
            prompt: RESUME_PROMPT.to_string(),
            model: "sonnet",
            session_id: "11111111-2222-3333-4444-555555555555",
            resume_session_id: Some("11111111-2222-3333-4444-555555555555"),
            allowed_tools: "Bash",
            config_dir: "C:/cfg/claude-b",
            cwd: std::path::PathBuf::from("C:/wt/task-a1"),
            settings_path: None,
            extra_env: Vec::new(),
            review_output: None,
            timeout_seconds: 1800,
            effort: None,
            research: false,
        };
        let plan = adapter().build_command(&req);
        assert_eq!(plan.args[0], "-p");
        assert_eq!(plan.stdin.as_deref(), Some(RESUME_PROMPT));
        assert!(plan.stdin.as_deref().unwrap().contains("previous session was interrupted"));
        assert_eq!(plan.args[1], "--resume");
        assert_eq!(plan.args[2], "11111111-2222-3333-4444-555555555555");
        assert!(!plan.args.contains(&"--session-id".to_string()));
        // a resumed worker is locked to the same tools as a fresh one
        let at = plan.args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(plan.args[at + 1], "Bash");
        assert!(plan.args.contains(&"--strict-mcp-config".to_string()));
    }

    #[test]
    fn build_command_puts_effort_right_after_the_model_on_a_fresh_run() {
        let req = LaunchRequest {
            prompt: "PROMPT-BODY".to_string(),
            model: "sonnet",
            session_id: "11111111-2222-3333-4444-555555555555",
            resume_session_id: None,
            allowed_tools: "Bash,Read,Write,Edit,Glob,Grep",
            config_dir: "C:/cfg/claude-b",
            cwd: std::path::PathBuf::from("C:/wt/task-a1"),
            settings_path: None,
            extra_env: Vec::new(),
            review_output: None,
            timeout_seconds: 1800,
            effort: Some("high".to_string()),
            research: false,
        };
        let plan = adapter().build_command(&req);
        assert_eq!(
            plan.args,
            vec![
                "-p",
                "--session-id", "11111111-2222-3333-4444-555555555555",
                "--model", "sonnet",
                "--effort", "high",
                "--output-format", "stream-json",
                "--verbose",
                "--allowedTools", "Bash,Read,Write,Edit,Glob,Grep",
                "--tools", "Bash,Read,Write,Edit,Glob,Grep",
                "--strict-mcp-config",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<String>>()
        );
        assert_eq!(plan.stdin.as_deref(), Some("PROMPT-BODY"));
    }

    #[test]
    fn build_command_puts_effort_right_after_the_model_on_a_resume() {
        let req = LaunchRequest {
            prompt: RESUME_PROMPT.to_string(),
            model: "sonnet",
            session_id: "11111111-2222-3333-4444-555555555555",
            resume_session_id: Some("11111111-2222-3333-4444-555555555555"),
            allowed_tools: "Bash",
            config_dir: "C:/cfg/claude-b",
            cwd: std::path::PathBuf::from("C:/wt/task-a1"),
            settings_path: None,
            extra_env: Vec::new(),
            review_output: None,
            timeout_seconds: 1800,
            effort: Some("max".to_string()),
            research: false,
        };
        let plan = adapter().build_command(&req);
        assert_eq!(
            plan.args,
            vec![
                "-p",
                "--resume", "11111111-2222-3333-4444-555555555555",
                "--model", "sonnet",
                "--effort", "max",
                "--output-format", "stream-json",
                "--verbose",
                "--allowedTools", "Bash",
                "--tools", "Bash",
                "--strict-mcp-config",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<String>>()
        );
        assert_eq!(plan.stdin.as_deref(), Some(RESUME_PROMPT));
    }

    #[test]
    fn build_command_omits_the_allowed_tools_flag_when_empty() {
        // #25 (final review): an empty allowed_tools must not send
        // `--allowedTools ""` - the real harness's behaviour there is
        // unverified, so the flag is skipped entirely.
        let req = LaunchRequest {
            prompt: "PROMPT-BODY".to_string(),
            model: "sonnet",
            session_id: "11111111-2222-3333-4444-555555555555",
            resume_session_id: None,
            allowed_tools: "",
            config_dir: "C:/cfg/claude-b",
            cwd: std::path::PathBuf::from("C:/wt/task-a1"),
            settings_path: None,
            extra_env: Vec::new(),
            review_output: None,
            timeout_seconds: 1800,
            effort: None,
            research: false,
        };
        let plan = adapter().build_command(&req);
        assert!(!plan.args.contains(&"--allowedTools".to_string()));
        // `--tools` is skipped exactly like `--allowedTools`; the MCP lock is not
        assert!(!plan.args.contains(&"--tools".to_string()));
        assert!(plan.args.contains(&"--strict-mcp-config".to_string()));
        assert_eq!(plan.stdin.as_deref(), Some("PROMPT-BODY"));
    }

    #[test]
    fn build_command_locks_a_reviewer_to_its_own_tool_list() {
        let req = LaunchRequest {
            prompt: "P".to_string(),
            model: "sonnet",
            session_id: "11111111-2222-3333-4444-555555555555",
            resume_session_id: None,
            allowed_tools: "Bash,Read,Glob,Grep",
            config_dir: "C:/cfg/claude-b",
            cwd: std::path::PathBuf::from("C:/wt/a1"),
            settings_path: None,
            extra_env: Vec::new(),
            review_output: None,
            timeout_seconds: 1800,
            effort: None,
            research: false,
        };
        let plan = adapter().build_command(&req);
        let at = plan.args.iter().position(|a| a == "--tools").unwrap();
        assert_eq!(plan.args[at + 1], "Bash,Read,Glob,Grep");
        let al = plan.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(plan.args[al + 1], plan.args[at + 1]);
        assert_eq!(plan.args.iter().filter(|a| *a == "--strict-mcp-config").count(), 1);
        assert_eq!(plan.stdin.as_deref(), Some("P"));
    }

    #[test]
    fn the_result_event_yields_model_usage_and_subagents_spawned() {
        let line = r#"{"type":"result","subtype":"success","is_error":false,"result":"DONE",
            "modelUsage":{"claude-sonnet-5":{"outputTokens":600,"inputTokens":3},"claude-haiku-4-5-20251001":{"outputTokens":10}},
            "subagent_stats":{"spawned":2}}"#;
        match adapter().parse_stream_line(line) {
            StreamEvent::Result(r) => {
                assert_eq!(
                    r.model_usage,
                    Some(vec![
                        ("claude-haiku-4-5-20251001".to_string(), 10),
                        ("claude-sonnet-5".to_string(), 600),
                    ])
                );
                assert_eq!(r.subagents_spawned, Some(2));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn a_result_event_without_usage_or_subagent_stats_parses_with_none() {
        match adapter().parse_stream_line(lines(SAMPLE)[3]) {
            StreamEvent::Result(r) => {
                assert_eq!(r.subagents_spawned, None);
            }
            other => panic!("expected Result, got {other:?}"),
        }
        match adapter().parse_stream_line(r#"{"type":"result","is_error":false,"modelUsage":{}}"#) {
            StreamEvent::Result(r) => {
                assert_eq!(r.model_usage, Some(Vec::new()));
                assert_eq!(r.subagents_spawned, None);
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn build_command_appends_the_settings_flag_and_the_extra_env() {
        let req = LaunchRequest {
            prompt: "P".to_string(),
            model: "haiku",
            session_id: "11111111-2222-3333-4444-555555555555",
            resume_session_id: None,
            allowed_tools: "Bash",
            config_dir: "C:/cfg/claude-b",
            cwd: std::path::PathBuf::from("C:/wt/a1"),
            settings_path: Some(std::path::PathBuf::from("C:/rd/R1/hooks-settings.json")),
            extra_env: vec![
                ("THOTH_RUN_ID".to_string(), "01RUN".to_string()),
                ("THOTH_INBOX_DIR".to_string(), "C:/rd/R1/inbox".to_string()),
            ],
            review_output: None,
            timeout_seconds: 1800,
            effort: None,
            research: false,
        };
        let plan = adapter().build_command(&req);
        let n = plan.args.len();
        assert_eq!(plan.args[n - 2], "--settings");
        assert_eq!(plan.args[n - 1], "C:/rd/R1/hooks-settings.json");
        assert_eq!(
            plan.env,
            vec![
                ("CLAUDE_CONFIG_DIR".to_string(), "C:/cfg/claude-b".to_string()),
                ("THOTH_RUN_ID".to_string(), "01RUN".to_string()),
                ("THOTH_INBOX_DIR".to_string(), "C:/rd/R1/inbox".to_string()),
            ],
            "CLAUDE_CONFIG_DIR must stay first so it cannot be shadowed"
        );
        assert_eq!(plan.stdin.as_deref(), Some("P"));
    }

    #[test]
    fn write_hooks_settings_produces_the_phase0_shape_with_an_absolute_exe() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("runs").join("R1");
        let exe = std::path::Path::new(r"C:\tools\thoth.exe");
        let path = write_hooks_settings(&run_dir, exe).unwrap();
        assert_eq!(path, run_dir.join("hooks-settings.json"));

        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        for event in ["PreToolUse", "PostToolUse", "PostToolUseFailure"] {
            let group = &v["hooks"][event];
            assert_eq!(group[0]["matcher"], "Bash", "{event} must match Bash only");
            assert_eq!(group[0]["hooks"][0]["type"], "command");
            let cmd = group[0]["hooks"][0]["command"].as_str().unwrap();
            assert_eq!(
                cmd, r#""C:\tools\thoth.exe" hook"#,
                "the hook command must be an absolute, quoted path so it never depends on PATH"
            );
        }
        // nothing else is configured: this file must not carry permissions,
        // models or anything else that would change how the worker behaves
        assert_eq!(v.as_object().unwrap().len(), 1);
        assert_eq!(v["hooks"].as_object().unwrap().len(), 3);
    }

    #[test]
    fn auth_command_sets_the_config_dir_and_asks_for_json() {
        let plan = adapter().auth_command("C:/cfg/claude-b", std::path::Path::new("C:/somewhere"));
        assert_eq!(plan.args, vec!["auth".to_string(), "status".to_string(), "--json".to_string()]);
        assert_eq!(
            plan.env,
            vec![("CLAUDE_CONFIG_DIR".to_string(), "C:/cfg/claude-b".to_string())]
        );
        assert_eq!(plan.stdin, None);
    }

    #[test]
    fn login_command_is_bare_claude_under_the_config_dir() {
        let plan = adapter().login_command("C:/cfg/claude-b", std::path::Path::new("C:/somewhere"));
        assert_eq!(plan.program, "claude");
        assert!(plan.args.is_empty(), "{:?}", plan.args);
        assert_eq!(
            plan.env,
            vec![("CLAUDE_CONFIG_DIR".to_string(), "C:/cfg/claude-b".to_string())]
        );
        assert_eq!(plan.cwd, std::path::PathBuf::from("C:/somewhere"));
        assert_eq!(plan.stdin, None);
    }

    #[test]
    fn auth_status_parses_the_real_eight_field_shape() {
        // 01-claude-cli-identity.md:28-40
        let real = r#"{"loggedIn":true,"authMethod":"claude.ai","apiProvider":"firstParty",
            "email":"u***@example.com","orgName":"u***@example.com's Organization",
            "subscriptionType":"max","configDirectory":"C:\\Users\\user\\.claude"}"#;
        let s = adapter().parse_auth_status(real).unwrap();
        assert!(s.logged_in);
        assert_eq!(s.email.as_deref(), Some("u***@example.com"));
        assert_eq!(s.subscription_type.as_deref(), Some("max"));
        assert_eq!(s.config_directory.as_deref(), Some("C:\\Users\\user\\.claude"));
    }

    #[test]
    fn auth_status_tolerates_a_reduced_shape_and_a_logged_out_answer() {
        let s = adapter()
            .parse_auth_status(r#"{"loggedIn":false}"#)
            .unwrap();
        assert!(!s.logged_in);
        assert_eq!(s.email, None);
        assert!(adapter().parse_auth_status("not json").is_err());
    }

    /// FC-1: tests/fixtures/stream-oauth-refresh-failed.jsonl is the real
    /// `result` line of a Claude Code 2.1.283 run on the Linux machine (taken
    /// from that run's stream.jsonl in the thoth state directory, unchanged).
    /// claude exited 1. The runner classifies it as transient
    /// (`runner::tests::the_recorded_claude_oauth_refresh_failure_is_transient`).
    #[test]
    fn the_recorded_oauth_refresh_failure_parses_as_an_error_result() {
        let line = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/stream-oauth-refresh-failed.jsonl"
        ));
        let StreamEvent::Result(r) = adapter().parse_stream_line(line.trim_end()) else {
            panic!("the fixture must parse as the result");
        };
        assert!(r.is_error);
        assert_eq!(r.api_error_status, None);
        assert!(r.text.unwrap().starts_with("Failed to refresh OAuth token"));
    }

    #[test]
    fn signed_out_is_true_only_when_the_cli_itself_says_so() {
        assert!(adapter().parse_auth_status(r#"{"loggedIn":false}"#).unwrap().signed_out);
        assert!(!adapter().parse_auth_status(r#"{"loggedIn":true}"#).unwrap().signed_out);
    }

    #[test]
    fn the_worker_prompt_contains_every_spec_section_and_the_task_id() {
        let p = build_prompt("01HZTASKID0000000000000000", &demo_spec(), None, None, None, &[], &Default::default(), false);
        assert!(p.starts_with("You are an implementer working in an isolated git worktree."));
        assert!(p.contains("## Goal\nAdd a retry to the uploader"));
        assert!(p.contains("## Acceptance criteria\n- AC1: retries three times\n- AC2: logs each attempt"));
        assert!(p.contains("## Constraints\n- no new dependencies"));
        assert!(p.contains("## Non-goals\n- rewriting the transport"));
        assert!(p.contains("## Excluded alternatives\n- backoff crate - too heavy"));
        assert!(p.contains("write the file `.agent/result.json`"));
        assert!(p.contains("\"task_id\":\"01HZTASKID0000000000000000\""));
        assert!(p.contains("Do not modify files outside this directory. Do not push."));
    }

    #[test]
    fn the_worker_prompt_still_renders_when_optional_lists_are_empty() {
        let spec = TaskSpec {
            goal: "g".to_string(),
            acceptance_criteria: vec!["AC1: x".to_string()],
            constraints: vec![],
            non_goals: vec![],
            excluded_alternatives: vec![],
            risk: "low".to_string(),
            checks: vec![],
        };
        let p = build_prompt("01T", &spec, None, None, None, &[], &Default::default(), false);
        assert!(p.contains("## Constraints\n## Non-goals\n"));
        assert!(p.contains("## Excluded alternatives\n"));
    }

    #[test]
    fn the_review_prompt_carries_the_criteria_the_index_and_the_read_only_rules() {
        let spec = TaskSpec {
            goal: "Add a retry to the uploader".to_string(),
            acceptance_criteria: vec![
                "AC1: retries three times".to_string(),
                "AC2: logs each attempt".to_string(),
            ],
            constraints: vec!["no new dependencies".to_string()],
            non_goals: vec!["rewriting the transport".to_string()],
            excluded_alternatives: vec!["backoff crate - too heavy".to_string()],
            risk: "important".to_string(),
            checks: vec![],
        };
        let index = vec![
            crate::evidence::IndexRow {
                code: "E001".to_string(),
                kind: "command".to_string(),
                label: "cargo test".to_string(),
                exit_code: Some(0),
            },
            crate::evidence::IndexRow {
                code: "E002".to_string(),
                kind: "diff".to_string(),
                label: "git diff aaaaaaaa..bbbbbbbb".to_string(),
                exit_code: None,
            },
        ];
        let ids = vec!["AC1".to_string(), "AC2".to_string()];
        let p = build_review_prompt(
            "01HZTASKID0000000000000000",
            &spec,
            &ids,
            r"C:\tools\thoth.exe",
            Some("E002"),
            &index,
            &[],
        );
        // P7-2: with nothing protected changed, the prompt has no such list
        assert!(!p.contains("Protected files this candidate changed"));

        let listed = build_review_prompt(
            "01HZTASKID0000000000000000",
            &spec,
            &ids,
            r"C:\tools\thoth.exe",
            Some("E002"),
            &index,
            &[
                "tests/x.rs (modified, test) [E002]".to_string(),
                "cargo.bat (added, check_word) [E002]".to_string(),
            ],
        );
        let list_at = listed.find("Protected files this candidate changed").expect("the list");
        assert!(listed.contains("   - tests/x.rs (modified, test) [E002]\n"));
        assert!(listed.contains("   - cargo.bat (added, check_word) [E002]\n"));
        assert!(listed.contains("decide whether the change weakened a check"));
        assert!(listed.contains("file by file, in your findings"));
        // after rule 1, before rule 2
        assert!(list_at > listed.find("1. Read the candidate diff").unwrap());
        assert!(list_at < listed.find("2. Judge every acceptance criterion").unwrap());

        assert!(p.starts_with("You are an independent reviewer."));
        assert!(p.contains("no Write or Edit tool"));
        assert!(p.contains("AC1 — AC1: retries three times"));
        assert!(p.contains("AC2 — AC2: logs each attempt"));
        assert!(p.contains("## Constraints\n- no new dependencies"));
        assert!(p.contains("## Non-goals\n- rewriting the transport"));
        assert!(p.contains("## Excluded alternatives\n- backoff crate - too heavy"));

        // the whole index, with exit codes
        assert!(p.contains("E001"));
        assert!(p.contains("cargo test"));
        assert!(p.contains("E002"));
        assert!(p.contains("git diff aaaaaaaa..bbbbbbbb"));

        // v6 §4's must-read list, naming the diff code explicitly
        assert!(p.contains(r#""C:\tools\thoth.exe" evidence show E002"#));
        assert!(p.contains("It includes the test files"));
        assert!(p.contains("A claim is not evidence."));

        // the output contract
        assert!(p.contains(".agent/review.json"));
        assert!(p.contains("\"verdict\":\"PASS\""));
        assert!(p.contains("\"extra_findings\""));
        assert!(p.contains("must be one of: AC1, AC2"));
        assert!(p.contains("01HZTASKID0000000000000000"));
    }

    #[test]
    fn build_review_prompt_asks_for_fix_size_category_and_interface_tests() {
        let ids = vec!["AC1".to_string(), "AC2".to_string()];
        let p = build_review_prompt("01T", &demo_spec(), &ids, "thoth", Some("E002"), &[], &[]);
        let rules = p.find("\n## Rules\n").unwrap();
        let output = p.find("\n## Output\n").unwrap();
        let rule = p.find(crate::review_json::INTERFACE_TEST_RULE).expect("the interface rule");
        assert!(rules < rule && rule < output, "{p}");
        assert!(p.contains("\n6. New or changed tests must check behaviour through the interface"), "{p}");
        // TESTS-1 (AC6): the suite-evidence rule is rule 7, right after the
        // interface rule, once, and the last rule before ## Output
        const SUITE_RULE: &str = "The implementer is told to run only the tests the spec names; the whole suite is run by the control plane's verify, which appears in the evidence index as rows labelled verify: with collected_by control_plane. Do not judge a criterion unmet or UNVERIFIED because the implementer did not run the whole suite itself, and do not count an implementer's cut-off or empty whole-suite output as missing evidence when a verify row covers the same check. Use only a candidate verify row for the same run and check, with successful completion and usable output; a verify@base: row, a failed, refused, timed-out or incomplete verify does not prove that check passed. When no usable verify row exists for a check, report what is missing in an extra finding with category evidence; do not treat an existing row as success or waive an explicit evidence requirement.";
        let suite = p.find(SUITE_RULE).expect("the suite rule");
        assert!(rule < suite && suite < output, "{p}");
        assert_eq!(p.matches(SUITE_RULE).count(), 1, "{p}");
        assert!(p.contains(&format!(
            "\n6. {}\n7. {SUITE_RULE}\n\n## Output\n",
            crate::review_json::INTERFACE_TEST_RULE
        )), "{p}");
        assert!(!p.contains("\n8. "), "{p}");
        // the id line is untouched; the label lines follow it
        assert!(p.contains("Every id in \"criteria\" must be one of: AC1, AC2. Judge each exactly once."));
        assert!(p.contains("\"severity\":\"minor\",\"note\":\"...\",\"evidence\":[\"E001\"],\"fix_size\":\"S\",\"category\":\"docs\"}"), "{p}");
        let after = &p[output..];
        for line in [
            "Every unmet criterion and every extra finding carries \"fix_size\" and \"category\". \"fix_size\" is the smallest fix that would resolve it: S (at most 10 changed lines in one file), M (at most about 50 changed lines, or more than one file, no design change), L (larger, or it needs a design or spec change).\n",
            "Every \"fix_size\" must be one of: S, M, L.\n",
            "Every \"category\" must be one of: spec_mismatch, spec_error, test_missing, test_interface, error_path, concurrency, platform, docs, evidence, files, security, other.\n",
            "- test_interface: a test bypasses the interface (reads the database or internal state, only compares a fixed string or the exit code, or repeats the implementation's logic)\n",
            "- other: none of the above\nLeave both out on a met criterion.\n",
        ] {
            assert!(after.contains(line), "missing {line:?} in:\n{after}");
        }
        assert!(after.find("Every id in").unwrap() < after.find("Every unmet criterion").unwrap());
    }

    #[test]
    fn the_review_prompt_says_so_when_there_is_no_diff_evidence() {
        let spec = TaskSpec {
            goal: "g".to_string(),
            acceptance_criteria: vec!["AC1: x".to_string()],
            constraints: vec![],
            non_goals: vec![],
            excluded_alternatives: vec![],
            risk: "normal".to_string(),
            checks: vec![],
        };
        let p = build_review_prompt(
            "01T",
            &spec,
            &["AC1".to_string()],
            "thoth",
            None,
            &[],
            &[],
        );
        assert!(p.contains("No diff evidence was captured"));
        assert!(p.contains("the control plane captured no evidence for this task"));
    }

    #[test]
    fn the_worker_prompt_tells_the_worker_to_cite_codes_from_the_index() {
        let p = build_prompt("01HZTASKID0000000000000000", &demo_spec(), None, None, None, &[], &Default::default(), false);
        assert!(p.contains("## Evidence"));
        assert!(p.contains(".agent/evidence-index.json"));
        assert!(p.contains("MUST cite at least one code from that file"));
        assert!(p.contains("Do not invent codes."));
        // the Evidence section comes after the result.json contract, so the
        // last thing the worker reads before writing the file is how to cite
        let evidence_at = p.find("## Evidence").unwrap();
        let result_at = p.find(".agent/result.json").unwrap();
        assert!(result_at < evidence_at, "the result contract comes first");
    }

    #[test]
    fn the_worker_prompt_tells_the_worker_to_run_checks_as_their_own_command() {
        let p = build_prompt("01HZTASKID0000000000000000", &demo_spec(), None, None, None, &[], &Default::default(), false);
        assert!(p.contains("as its own shell command"));
        assert!(p.contains("do not pipe it into another command"));
        assert!(p.contains("| tail or | head"));
        assert!(p.contains("with ; or &&"));
        assert!(p.contains("the captured exit code is the last command's"));
        // the instruction sits inside the Evidence section
        assert!(p.find("as its own shell command").unwrap() > p.find("## Evidence").unwrap());
    }

    fn approved(id: &str, cmd: &str, shell: &str) -> crate::checks::Check {
        crate::checks::Check {
            check_id: id.into(),
            shell: shell.into(),
            cmd: cmd.into(),
            cwd: ".".into(),
            inputs: vec![],
            created_at: "n".into(),
            approved_at: Some("n".into()),
            approved_fingerprint: Some("h".into()),
            approved_detail: None,
            approved_commit: None,
            exe_pin: "hash".into(),
        }
    }

    /// P7-1b / AC7: the approved checks are listed, one `id: cmd (shell)` line
    /// each, in a section that sits before the result.json contract, in both
    /// the Claude and the codex prompt.
    #[test]
    fn the_worker_prompt_lists_the_approved_checks_before_the_result_contract() {
        let checks = [
            approved("test", "cargo test", "argv"),
            approved("lint", "if (Test-Path x) { exit 0 }", "powershell"),
        ];
        for p in [
            build_prompt("01T", &demo_spec(), None, None, None, &checks, &Default::default(), false),
            build_codex_prompt("01T", &demo_spec(), None, None, None, &checks, &Default::default(), false),
        ] {
            let at = p.find("## Checks the control plane will re-run\n").expect("section");
            let section = &p[at..];
            assert!(section.starts_with(
                "## Checks the control plane will re-run\ntest: cargo test (argv)\n\
                 lint: if (Test-Path x) { exit 0 } (powershell)\n\
                 Run the tests the spec names before you finish. Your verification_requests are kept for \
                 reference only; the control plane runs the checks above.\n\
                 Evidence for the whole test suite comes from the control plane's verify of the \
                 checks above, not from you. Run the tests the spec names, each as its own \
                 foreground command, and do not start a whole-suite run that cannot finish within \
                 one command's time limit; when the spec asks for the whole suite, run it in parts \
                 that each finish, and keep each part's output.\n\nWhen done, "
            ), "{section}");
            assert!(!p.contains("(none approved yet)"));
            // before the result.json contract, after the spec sections
            assert!(at < p.find("write the file `.agent/result.json`").unwrap());
            assert!(at > p.find("## Excluded alternatives").unwrap());
        }
    }

    /// TESTS-1 (AC5): the checks note's first sentence and the suite note,
    /// as English, once each, in this order: checks note, suite note, the
    /// result.json contract, `## Protected files`; a research prompt puts the
    /// suite note before `## Research claims`. Claude and agy (`build_prompt`)
    /// and codex (`build_codex_prompt`), with and without approved checks.
    #[test]
    fn the_suite_note_follows_the_checks_note_in_every_worker_prompt() {
        const CHECKS: &str = "Run the tests the spec names before you finish. Your verification_requests are kept for reference only; the control plane runs the checks above.\n";
        const SUITE: &str = "Evidence for the whole test suite comes from the control plane's verify of the checks above, not from you. Run the tests the spec names, each as its own foreground command, and do not start a whole-suite run that cannot finish within one command's time limit; when the spec asks for the whole suite, run it in parts that each finish, and keep each part's output.\n";
        assert_eq!(CHECKS_NOTE, CHECKS);
        assert_eq!(SUITE_NOTE, SUITE);
        let approved_checks = [approved("test", "cargo test", "argv")];
        for research in [false, true] {
            for checks in [&[][..], &approved_checks[..]] {
                let prompts = [
                    ("claude", build_prompt("01T", &demo_spec(), None, None, None, checks, &Default::default(), research)),
                    ("codex", build_codex_prompt("01T", &demo_spec(), None, None, None, checks, &Default::default(), research)),
                ];
                for (who, p) in prompts {
                    let label = format!("{who} research={research} checks={}", checks.len());
                    assert_eq!(p.matches(CHECKS).count(), 1, "{label}: {p}");
                    assert_eq!(p.matches(SUITE).count(), 1, "{label}: {p}");
                    let checks_at = p.find(CHECKS).unwrap();
                    let suite_at = p.find(SUITE).unwrap();
                    let result_at = p.find("write the file `.agent/result.json`").unwrap();
                    let protected_at = p.find("\n## Protected files\n").unwrap();
                    assert!(checks_at < suite_at, "{label}");
                    assert!(suite_at < result_at, "{label}");
                    assert!(result_at < protected_at, "{label}");
                    // the suite note is the very next line after the checks note
                    assert_eq!(checks_at + CHECKS.len(), suite_at, "{label}: {p}");
                    let line_before = if checks.is_empty() {
                        "(none approved yet)\n"
                    } else {
                        "test: cargo test (argv)\n"
                    };
                    assert!(p[..checks_at].ends_with(line_before), "{label}: {p}");
                    match research {
                        true => {
                            let research_at = p.find("\n## Research claims\n").unwrap();
                            assert!(suite_at < research_at && research_at < result_at, "{label}");
                        }
                        false => assert!(!p.contains("## Research claims"), "{label}"),
                    }
                }
            }
        }
    }

    /// RES-1a AC5: a research task's prompt (Claude and agy, and codex) has
    /// the `## Research claims` section right before the result.json shape,
    /// and is otherwise the non-research prompt byte for byte.
    #[test]
    fn a_research_prompt_adds_the_research_claims_section_before_the_shape() {
        let pairs = [
            (
                build_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false),
                build_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), true),
            ),
            (
                build_codex_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false),
                build_codex_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), true),
            ),
        ];
        for (plain, research) in pairs {
            assert!(!plain.contains("## Research claims"), "{plain}");
            let at = research.find("\n## Research claims\n").expect("section");
            assert!(at > research.find(CHECKS_NOTE).unwrap());
            assert!(at < research.find("write the file `.agent/result.json`").unwrap());
            assert_eq!(research.replacen(RESEARCH_SECTION, "", 1), plain);
            for needle in [
                r#"{"kind":"research","text":"...","evidence":[],"sources":[{"url":"https://...","quote":"..."}]}"#,
                "http://, https:// or file://",
                "at most 500 characters",
                "Leave any claim you have no source for out of the report",
            ] {
                assert!(RESEARCH_SECTION.contains(needle), "{needle}");
            }
        }
    }

    /// Every worker prompt (Claude and agy share `build_prompt`; codex has its
    /// own entry point) forbids background commands; the reviewer's does not,
    /// and the note is not the prompt's last section.
    #[test]
    fn worker_prompts_forbid_background_commands_and_reviewer_prompt_does_not() {
        for p in [
            build_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false),
            build_codex_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false),
        ] {
            assert!(p.contains("Do not start background commands"), "{p}");
            assert!(p.contains(SINGLE_RUN_NOTE));
            assert!(p.find(SINGLE_RUN_NOTE).unwrap() < p.find("\n## Protected files\n").unwrap());
        }
        let r = build_review_prompt("01T", &demo_spec(), &["AC1".to_string()], "thoth", None, &[], &[]);
        assert!(!r.contains("Do not start background commands"), "{r}");
    }

    /// P7-2 / AC5, AC9: the `## Protected files` section comes after
    /// `## Evidence`, lists the inputs of the checks that apply to the task
    /// and every check word, and says an added or modified file named like a
    /// check word must be declared too. Same in the Claude and codex prompts.
    #[test]
    fn the_worker_prompt_ends_with_the_protected_files_section() {
        let mut test = approved("test", "cargo test", "argv");
        test.inputs = vec!["Cargo.lock".into(), "Cargo.toml".into()];
        let mut tree = approved("public-tree", "python scripts/test_public_tree.py", "argv");
        tree.inputs = vec!["scripts/test_public_tree.py".into()];
        let checks = [test, tree];
        let protected = crate::protected::Catalogue::from_parts(vec!["*.snap".into()], &checks);
        for p in [
            build_prompt("01T", &demo_spec(), None, None, None, &checks, &protected, false),
            build_codex_prompt("01T", &demo_spec(), None, None, None, &checks, &protected, false),
        ] {
            let at = p.find("\n## Protected files\n").expect("section");
            assert!(at > p.find("## Evidence").unwrap(), "after the Evidence section");
            let section = &p[at..];
            assert!(section.contains(
                "Check input files: Cargo.lock, Cargo.toml, scripts/test_public_tree.py\n"
            ));
            assert!(section.contains("Check words: cargo, python, test, test_public_tree\n"));
            assert!(section.contains("add, modify or delete"));
            assert!(section.contains("Also treated as test files in this repo: *.snap\n"));
        }

        // with no checks at all the section is still there, and says so
        let bare = build_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false);
        let section = &bare[bare.find("\n## Protected files\n").expect("section")..];
        assert!(section.contains("Check input files: (none)\n"));
        assert!(section.contains("Check words: (none)\n"));
        assert!(!section.contains("Also treated as test files"));
    }

    /// P7-2 / AC5: a reroute after a rule 6 rejection quotes rule 6's reason,
    /// so the next worker is told which file to declare.
    #[test]
    fn a_reroute_after_rule_6_quotes_the_missing_claim_reason() {
        const REASON: &str = "claim missing for protected file tests/x.rs (modified): add a claim \
                              of kind protected_change whose text starts with the path and \
                              explains the change";
        let addendum = crate::spec::RerouteAddendum {
            previous_run: "01RUNPREV".to_string(),
            reason: REASON.to_string(),
            rule: Some(6),
        };
        let p = build_prompt("01T", &demo_spec(), None, Some(&addendum), None, &[], &Default::default(), false);
        assert!(p.contains("## Previous run's result.json was not accepted"));
        assert!(p.contains(&format!("> {REASON}\n")), "{p}");
    }

    #[test]
    fn the_worker_prompt_says_so_when_no_check_is_approved() {
        let p = build_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false);
        let at = p.find("## Checks the control plane will re-run\n").expect("section");
        assert!(p[at..].starts_with(
            "## Checks the control plane will re-run\n(none approved yet)\nRun the tests the spec names before you finish."
        ));
        assert!(at < p.find("write the file `.agent/result.json`").unwrap());
    }

    #[test]
    fn the_worker_prompt_carries_the_rework_section_only_on_a_rework() {
        let plain = build_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false);
        assert!(!plain.contains("## Previous attempt was rejected"));

        let addendum = crate::spec::ReworkAddendum {
            attempt: 2,
            previous_run: "01RUNPREV".to_string(),
            previous_attempt: 1,
            previous_candidate: "cafebabe".to_string(),
            unmet: vec!["AC1: the retry never fires (evidence E002)".to_string()],
            extra_findings: vec!["major: the test asserts a constant".to_string()],
        };
        let p = build_prompt("01T", &demo_spec(), Some(&addendum), None, None, &[], &Default::default(), false);
        assert!(p.contains("## Previous attempt was rejected"));
        assert!(p.contains("This is attempt 2"));
        assert!(p.contains("Attempt 1 was reviewed and rejected"));
        assert!(p.contains("- AC1: the retry never fires (evidence E002)"));
        assert!(p.contains("- major: the test asserts a constant"));
        assert!(p.contains("build on them rather than starting over"));
        // the rework section comes before the result.json contract, so the
        // instructions the worker reads last are still about reporting
        assert!(
            p.find("## Previous attempt was rejected").unwrap()
                < p.find(".agent/result.json").unwrap()
        );
        // and the Evidence section is still last
        assert!(p.contains("## Evidence"));
    }

    #[test]
    fn a_rework_asks_for_a_failing_test_before_each_behaviour_fix() {
        const ASK: &str = "first add or change a test that fails on the code as it is now";
        let plain = build_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false);
        assert!(!plain.contains(ASK));

        let addendum = crate::spec::ReworkAddendum {
            attempt: 2,
            previous_run: "01RUNPREV".to_string(),
            previous_attempt: 1,
            previous_candidate: "cafebabe".to_string(),
            unmet: vec!["AC1: the retry never fires".to_string()],
            extra_findings: vec!["major: the test asserts a constant".to_string()],
        };
        let p = build_prompt("01T", &demo_spec(), Some(&addendum), None, None, &[], &Default::default(), false);
        assert!(p.contains(ASK));
        assert!(p.contains("run that test on its own"));
        assert!(p.contains("then fix the code and run the test again"));
        assert!(p.contains("change no behaviour"));
        // after the findings lists, before the result.json contract
        let ask_at = p.find(ASK).unwrap();
        assert!(ask_at > p.find("- major: the test asserts a constant").unwrap());
        assert!(ask_at < p.find(".agent/result.json").unwrap());
    }

    #[test]
    fn the_worker_prompt_carries_the_reroute_section_only_with_a_reason() {
        const HEAD: &str = "## Previous run's result.json was not accepted";
        let plain = build_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false);
        assert!(!plain.contains(HEAD));

        let addendum = crate::spec::RerouteAddendum {
            previous_run: "01RUNPREV".to_string(),
            reason: "claim 2 (test_result) cites no evidence".to_string(),
            rule: Some(2),
        };
        let p = build_prompt("01T", &demo_spec(), None, Some(&addendum), None, &[], &Default::default(), false);
        assert!(p.contains(HEAD));
        assert!(p.contains("claim 2 (test_result) cites no evidence"));
        assert!(p.contains("01RUNPREV"));
        assert!(p.contains("already in this worktree"));
        assert!(p.contains("check and fix rather than start over"));
        assert!(p.contains("every test_result and doc_reference claim"));
        assert!(p.contains(".agent/evidence-index.json"));
        // before the result.json contract, and it is not a rework
        assert!(p.find(HEAD).unwrap() < p.find("When done, commit").unwrap());
        assert!(!p.contains("## Previous attempt was rejected"));
    }

    /// R2 (fix round 1): the prompt must not tell the worker that attempt
    /// `N-1` was reviewed when the findings are older than that, nor that
    /// commits are in the worktree when none are.
    #[test]
    fn the_rework_section_names_the_attempt_the_findings_really_came_from() {
        // attempt 3, but attempt 2 crashed, so the candidate and the findings
        // are attempt 1's
        let after_a_crash = crate::spec::ReworkAddendum {
            attempt: 3,
            previous_run: "01RUNA1".to_string(),
            previous_attempt: 1,
            previous_candidate: "cafebabe".to_string(),
            unmet: vec!["AC1: not met".to_string()],
            extra_findings: vec![],
        };
        let p = build_prompt("01T", &demo_spec(), Some(&after_a_crash), None, None, &[], &Default::default(), false);
        assert!(p.contains("This is attempt 3"));
        assert!(
            p.contains("Attempt 1 was reviewed and rejected"),
            "the findings are attempt 1's, not attempt 2's: {p}"
        );
        assert!(!p.contains("Attempt 2 was reviewed"));
        assert!(p.contains("build on them rather than starting over"));

        // nothing completed before: no attempt to name and no commits to build on
        let nothing_before = crate::spec::ReworkAddendum {
            attempt: 2,
            previous_run: String::new(),
            previous_attempt: 0,
            previous_candidate: String::new(),
            unmet: vec!["AC1: not met".to_string()],
            extra_findings: vec![],
        };
        let p = build_prompt("01T", &demo_spec(), Some(&nothing_before), None, None, &[], &Default::default(), false);
        assert!(p.contains("A previous attempt was reviewed and rejected"));
        assert!(!p.contains("Attempt 0 was reviewed"));
        assert!(!p.contains("build on them rather than starting over"));
        assert!(p.contains("Its commits are not in this worktree"));
    }

    // AC1 (scope §2.6): the codex prompt tells the worker not to commit, and
    // is otherwise byte-for-byte the Claude prompt.

    #[test]
    fn the_claude_prompt_still_tells_the_worker_to_commit_its_work() {
        let p = build_prompt("01HZTASKID0000000000000000", &demo_spec(), None, None, None, &[], &Default::default(), false);
        assert!(p.contains("When done, commit your work on the current branch, then write the file `.agent/result.json`"));
        assert!(!p.contains("Do not commit"));
    }

    #[test]
    fn the_codex_prompt_tells_the_worker_not_to_commit_and_keeps_the_rest() {
        let p = build_codex_prompt("01HZTASKID0000000000000000", &demo_spec(), None, None, None, &[], &Default::default(), false);
        assert!(!p.contains("commit your work"), "{p}");
        assert!(
            p.contains(
                "Do not commit: leave your changes in the worktree. thoth will commit \
                 whatever is there once this run finishes cleanly."
            ),
            "{p}"
        );
        assert!(p.contains("When done, write the file `.agent/result.json`"));
        assert!(p.contains("\"task_id\":\"01HZTASKID0000000000000000\""));
        assert!(p.contains("Do not modify files outside this directory. Do not push."));
        assert!(p.contains(EVIDENCE_SECTION));
    }

    #[test]
    fn the_codex_prompt_differs_from_the_claude_prompt_only_by_the_commit_instruction() {
        let rework = crate::spec::ReworkAddendum {
            attempt: 2,
            previous_run: "01RUNA1".to_string(),
            previous_attempt: 1,
            previous_candidate: "cafebabe".to_string(),
            unmet: vec!["AC1: not met".to_string()],
            extra_findings: vec!["found a leak".to_string()],
        };
        let reroute = crate::spec::RerouteAddendum {
            previous_run: "01RUNB1".to_string(),
            reason: "cited a code it never opened".to_string(),
            rule: None,
        };
        for (rework, reroute) in [
            (None, None),
            (Some(&rework), None),
            (None, Some(&reroute)),
        ] {
            let claude = build_prompt("01T", &demo_spec(), rework, reroute, None, &[], &Default::default(), false);
            let codex = build_codex_prompt("01T", &demo_spec(), rework, reroute, None, &[], &Default::default(), false);
            let claude_no_commit = claude.replacen(
                "When done, commit your work on the current branch, then write the file \
                 `.agent/result.json`",
                "When done, write the file `.agent/result.json`",
                1,
            );
            let (before, after) = claude_no_commit
                .split_once("Do not modify files outside this directory. Do not push.\n")
                .unwrap();
            let expected = format!(
                "{before}Do not modify files outside this directory. Do not push.\nDo not \
                 commit: leave your changes in the worktree. thoth will commit whatever is \
                 there once this run finishes cleanly.\n{after}"
            );
            assert_eq!(codex, expected);
        }
    }

    fn demo_handoff() -> crate::handoff::HandoffRef {
        crate::handoff::HandoffRef {
            previous_run: "01RUNPREV".to_string(),
            reason: "quota".to_string(),
            from_profile: "claude-b".to_string(),
            from_harness: "claude-code".to_string(),
            path: ".agent/handoff.md".to_string(),
        }
    }

    /// S3-9 AC4: the handoff section sits after the excluded alternatives and
    /// before the checks, names the file and the reason, and carries none of
    /// the file's content - in the Claude and the codex prompt alike.
    #[test]
    fn a_handoff_adds_the_continuing_section_before_the_checks_in_both_prompts() {
        let h = demo_handoff();
        let claude = build_prompt("01T", &demo_spec(), None, None, Some(&h), &[], &Default::default(), false);
        let codex = build_codex_prompt("01T", &demo_spec(), None, None, Some(&h), &[], &Default::default(), false);
        for p in [&claude, &codex] {
            let section = p.find("## Continuing an interrupted run\n").expect("section");
            assert!(section > p.find("## Excluded alternatives").unwrap());
            assert!(section < p.find("## Checks the control plane will re-run").unwrap());
            assert!(p.contains(".agent/handoff.md"));
            assert!(p.contains("01RUNPREV"));
            assert!(p.contains("quota"));
            assert!(!p.contains("## Worktree state"));
            assert!(!p.contains("## Evidence so far"));
        }
    }

    /// S3-9 AC4: without a handoff the prompt has no such section.
    #[test]
    fn no_handoff_means_no_continuing_section() {
        let p = build_prompt("01T", &demo_spec(), None, None, None, &[], &Default::default(), false);
        assert!(!p.contains("## Continuing an interrupted run"));
        let with = build_prompt("01T", &demo_spec(), None, None, Some(&demo_handoff()), &[], &Default::default(), false);
        let (head, _) = with.split_once("\n## Continuing an interrupted run\n").unwrap();
        assert!(p.starts_with(head), "only the new section is added");
    }
}
