//! The Codex harness: `codex exec --json` as an implementer worker
//! (docs/post-phase2/codex-worker-scope-2026-09-27.md §2.1-§2.3). The command
//! shapes and every event this parser reads were measured with codex-cli 0.154;
//! the recordings are in `tests/fixtures/codex/`.

use super::{AuthStatus, CommandPlan, HarnessAdapter, LaunchRequest, ResultEvent, StreamEvent};
use anyhow::Result;
use std::path::Path;

pub struct CodexAdapter {
    pub bin: String,
}

impl CodexAdapter {
    pub fn from_env() -> Self {
        CodexAdapter {
            bin: crate::config::codex_bin(),
        }
    }
}

/// `codex debug models` on 2026-09-27. Only used to tell a human what to pass.
pub const CODEX_MODELS: &[&str] = &[
    "gpt-6-astra",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
    "gpt-5.6-luna",
    "gpt-5.5",
];

/// Item types a locked-down codex worker produces. Anything else (an MCP tool
/// call, a web search, a collab/subagent call) means a tool got through that
/// scope §2.2's flags were meant to switch off.
pub const EXPECTED_ITEM_TYPES: &[&str] = &[
    "agent_message",
    "reasoning",
    "command_execution",
    "file_change",
    "error",
    "todo_list",
];

/// Item types a research task's worker or source checker also produces, because
/// `RESEARCH_WEB_SEARCH` switched the tool on (RES-1a).
pub const RESEARCH_ITEM_TYPES: &[&str] = &["web_search"];

/// Whether `model` names a Claude model rather than a codex one: the four
/// Claude Code aliases, or a full `claude-*` id.
pub fn is_claude_model(model: &str) -> bool {
    let m = model.trim().to_lowercase();
    matches!(m.as_str(), "haiku" | "sonnet" | "opus" | "fable") || m.starts_with("claude-")
}

/// Scope §2.2: a codex profile needs a codex model, and the CLI's default
/// `--model` (sonnet) is a Claude alias, so it is refused rather than passed.
/// MODELS-1: `known_models` is what the refusal lists: the account's
/// `models_cache.json` slugs when the caller read them, else `CODEX_MODELS`.
pub fn check_model(profile_id: &str, model: &str, known_models: &str) -> Result<()> {
    if is_claude_model(model) {
        anyhow::bail!(
            "profile {profile_id} runs codex, and '{model}' is a Claude model; pass --model with \
             a codex model: {known_models}"
        );
    }
    Ok(())
}

/// Where the runner puts a codex reviewer's input, relative to the review
/// worktree (scope §2.4): `packet.md` and `evidence/<CODE>.txt`.
pub const REVIEW_INPUT_DIR: &str = ".agent/review-input";

/// The codex reviewer prompt (scope §2.4). Same rules as the Claude
/// reviewer's (`claude_code::build_review_prompt`), but E3a means it cannot
/// run thoth: its evidence comes as files the runner wrote, and its
/// verdict is its final message, which codex writes to `.agent/review.json`.
pub fn build_review_prompt(
    task_id: &str,
    spec: &crate::spec::TaskSpec,
    criterion_ids: &[String],
    diff_code: Option<&str>,
) -> String {
    let mut s = String::new();
    s.push_str("You are an independent reviewer. The current directory is a read-only checkout of the candidate commit, in a read-only sandbox: you cannot change any file, and you do not need to.\n\n");
    s.push_str("## Goal under review\n");
    s.push_str(spec.goal.trim());
    s.push_str("\n\n## Acceptance criteria\n");
    for (id, ac) in criterion_ids.iter().zip(spec.acceptance_criteria.iter()) {
        s.push_str(id);
        s.push_str(" — ");
        s.push_str(ac.trim());
        s.push('\n');
    }

    s.push_str("\n## Your input\n");
    s.push_str(&format!("Read {REVIEW_INPUT_DIR}/packet.md first. It is the review packet: the full spec, the evidence index with every code, the candidate diff and the implementer's claims.\n"));
    s.push_str(&format!("The whole stored output behind each code in the index is in {REVIEW_INPUT_DIR}/evidence/<CODE>.txt (for example {REVIEW_INPUT_DIR}/evidence/E001.txt). Read them with cat, and read the worktree's own files to check the code itself.\n"));
    s.push_str("Do not run thoth: it cannot work in this sandbox, and everything it would show you is already in those files.\n");

    s.push_str("\n## Rules\n");
    match diff_code {
        Some(code) => s.push_str(&format!(
            "1. Read the candidate diff before anything else: {REVIEW_INPUT_DIR}/evidence/{code}.txt. It includes the test files. Changing a test so it passes is the classic escape route, so if the tests moved, say what moved and why it does or does not matter.\n"
        )),
        None => s.push_str(
            "1. No diff evidence was captured for this candidate. Record that as an extra finding and judge what you can from the worktree itself.\n",
        ),
    }
    s.push_str("2. Judge every acceptance criterion exactly once, as met or unmet. Every met criterion must cite at least one code from the packet's evidence index; an unmet one needs a code or a note saying what is missing. Do not invent codes.\n");
    s.push_str("3. Do not trust the implementer's summary or claims. A claim is not evidence.\n");
    s.push_str("4. PASS only if every criterion is met. REJECT when a criterion is unmet. UNVERIFIED when the evidence available cannot settle it.\n");
    s.push_str(&format!("5. {}\n", crate::review_json::INTERFACE_TEST_RULE));
    // TESTS-1 (AC6): the whole suite is the control plane's verify row, not
    // something the implementer proves itself.
    s.push_str(&format!("6. {}\n", crate::review_json::SUITE_EVIDENCE_RULE));

    s.push_str("\n## Output\n");
    s.push_str("Your final message is the review document: a JSON object in the output schema you were given, and nothing else. thoth stores it as .agent/review.json; do not try to write that file yourself. Leave \"note\" empty (\"\") when you have nothing to add. For example:\n");
    s.push_str("{\"verdict\":\"PASS\",\"findings\":{\"criteria\":[{\"id\":\"");
    s.push_str(criterion_ids.first().map(|s| s.as_str()).unwrap_or("AC1"));
    s.push_str("\",\"verdict\":\"met\",\"evidence\":[\"E001\"],\"note\":\"\",\"fix_size\":\"none\",\"category\":\"none\"}],\"extra_findings\":[{\"severity\":\"minor\",\"note\":\"...\",\"evidence\":[\"E001\"],\"fix_size\":\"S\",\"category\":\"docs\"}]}}\n");
    s.push_str(&format!(
        "Every id in \"criteria\" must be one of: {}.\n",
        criterion_ids.join(", ")
    ));
    s.push_str(&crate::review_json::finding_labels_prompt());
    s.push_str("Put \"none\" in both on a met criterion.\n");
    s.push_str(&format!("This review is for task {task_id}.\n"));
    s
}

const OMITTED_TAIL: &str = " bytes omitted ...";

/// E5: codex keeps the head and tail of a long output and puts
/// `... N bytes omitted ...` in between.
pub fn output_was_truncated(output: &str) -> bool {
    output.match_indices(OMITTED_TAIL).any(|(at, _)| {
        let before = &output[..at];
        let digits = before.len() - before.trim_end_matches(|c: char| c.is_ascii_digit()).len();
        digits > 0 && before[..before.len() - digits].ends_with("... ")
    })
}

/// E6 plus the strings in the codex binary. Not verified against a real
/// usage-limit error: none could be triggered (scope §1 E6, §3).
pub fn looks_like_codex_quota(message: &str) -> bool {
    let compact: String = message.chars().filter(|c| !c.is_whitespace()).collect();
    message.contains("usage_limit_reached")
        || message.contains("rate_limit_reached")
        || message.contains("UsageLimitExceeded")
        || message.contains("hit your usage limit")
        || compact.contains("\"status\":429")
}

fn s(v: &serde_json::Value) -> Option<String> {
    v.as_str().map(|x| x.to_string())
}

/// An E6 error message: a string that is itself JSON with a numeric
/// `status`. The whole string stays the text, so nothing codex said is lost.
fn error_result(message: Option<String>, subtype: &str) -> ResultEvent {
    let text = message.unwrap_or_default();
    let status = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| match &v["status"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(x) => Some(x.clone()),
            other => Some(other.to_string()),
        });
    ResultEvent {
        is_error: true,
        subtype: Some(subtype.to_string()),
        quota: looks_like_codex_quota(&text),
        text: Some(text),
        api_error_status: status,
        ..Default::default()
    }
}

/// RES-1a: a research task's implementer gets network access inside its
/// `workspace-write` sandbox, which has none by default. The key is in
/// codex-cli 0.154's config key table under `sandbox_workspace_write`; a
/// non-boolean value is rejected (`expected a boolean`) and with it the
/// model's permission text reads `Network access is enabled.` (checked
/// offline). A misspelt key is silently ignored, so it is spelt once, here.
pub const RESEARCH_NETWORK_ACCESS: &str = "sandbox_workspace_write.network_access=true";
/// RES-1a: codex's hosted web search tool, live results. codex-cli 0.154
/// accepts `disabled`, `cached`, `indexed` and `live` for `web_search` (any
/// other value is `unknown variant`). `--search` is the same switch, but
/// `codex exec --help` does not list it, so it goes in as config.
pub const RESEARCH_WEB_SEARCH: &str = "web_search=\"live\"";

/// The flags every codex worker gets, fresh or resumed (scope §2.2).
const LOCKDOWN: &[&str] = &[
    "-c",
    "approval_policy=\"never\"",
    "--disable",
    "multi_agent",
    "--disable",
    "apps",
    "--disable",
    "browser_use",
    "--disable",
    "computer_use",
    "--ignore-user-config",
];

impl CodexAdapter {
    fn build_command_for(&self, req: &LaunchRequest<'_>, windows: bool) -> CommandPlan {
        // Every option goes before `resume`: codex-cli 0.154 rejects `-C`,
        // `--add-dir` and `-s` after `exec resume <thread>` as unexpected
        // arguments (measured 2026-09-27).
        let mut args: Vec<String> = vec![
            "exec".to_string(),
            "--json".to_string(),
            "-m".to_string(),
            req.model.to_string(),
        ];
        args.extend(super::effort::launch_args("codex", req.effort.as_deref()));
        if req.resume_session_id.is_some() {
            // E2: `exec resume` refuses `-s`; the sandbox goes in as config.
            args.extend(["-c".to_string(), "sandbox_mode=\"workspace-write\"".to_string()]);
        } else if req.review_output.is_some() {
            // Scope §2.4: the reviewer changes nothing, not even its report,
            // which codex writes to `-o` from outside the sandbox.
            args.extend(["-s".to_string(), "read-only".to_string()]);
        } else {
            args.extend(["-s".to_string(), "workspace-write".to_string()]);
        }
        if req.research {
            if req.review_output.is_none() {
                args.extend(["-c".to_string(), RESEARCH_NETWORK_ACCESS.to_string()]);
            }
            args.extend(["-c".to_string(), RESEARCH_WEB_SEARCH.to_string()]);
        }
        if windows {
            // `--ignore-user-config` leaves codex on Windows with no sandbox
            // implementation, so it rejects every command and write.
            args.extend(["-c".to_string(), "windows.sandbox=\"unelevated\"".to_string()]);
        }
        args.extend(LOCKDOWN.iter().map(|a| a.to_string()));
        if let Some(r) = &req.review_output {
            args.push("--output-schema".to_string());
            args.push(r.schema.display().to_string());
            args.push("-o".to_string());
            args.push(r.output.display().to_string());
        }
        args.push("-C".to_string());
        args.push(req.cwd.display().to_string());
        if let Some(thread) = req.resume_session_id {
            args.extend(["resume".to_string(), thread.to_string()]);
        }
        // W3: `-` makes codex read the prompt from stdin (measured
        // 2026-10-06 on 0.154, fresh and `resume <thread> -`), keeping it off
        // the command line.
        args.push("-".to_string());
        let mut env = vec![("CODEX_HOME".to_string(), req.config_dir.to_string())];
        env.extend(req.extra_env.iter().cloned());
        CommandPlan {
            program: self.bin.clone(),
            args,
            env,
            cwd: req.cwd.clone(),
            stdin: Some(req.prompt.clone()),
        }
    }
}

impl HarnessAdapter for CodexAdapter {
    /// `--allowed-tools` and `--settings` do not apply: codex's tools are
    /// fixed by the sandbox and the disabled features, and its commands are
    /// captured from the stream rather than through hooks.
    fn build_command(&self, req: &LaunchRequest<'_>) -> CommandPlan {
        self.build_command_for(req, cfg!(windows))
    }

    /// Stored verbatim even for a research task: codex never reads the list,
    /// and its network is opened by `LaunchRequest::research` instead.
    fn implementer_allowed_tools(&self, given: &str, _research: bool) -> String {
        given.to_string()
    }

    fn auth_command(&self, config_dir: &str, cwd: &Path) -> CommandPlan {
        CommandPlan {
            program: self.bin.clone(),
            args: vec!["login".to_string(), "status".to_string()],
            env: vec![("CODEX_HOME".to_string(), config_dir.to_string())],
            cwd: cwd.to_path_buf(),
            stdin: None,
        }
    }

    fn login_command(&self, config_dir: &str, cwd: &Path) -> CommandPlan {
        CommandPlan {
            program: self.bin.clone(),
            args: vec!["login".to_string()],
            env: vec![("CODEX_HOME".to_string(), config_dir.to_string())],
            cwd: cwd.to_path_buf(),
            stdin: None,
        }
    }

    fn parse_auth_status(&self, stdout: &str) -> Result<AuthStatus> {
        self.parse_auth_output(true, stdout, "")
    }

    /// `codex login status` prints "Logged in using ChatGPT" and exits 0 when
    /// logged in, and prints neither email nor plan.
    fn parse_auth_output(&self, success: bool, stdout: &str, stderr: &str) -> Result<AuthStatus> {
        Ok(AuthStatus {
            logged_in: success && (stdout.contains("Logged in") || stderr.contains("Logged in")),
            // FC-1: fake-codex prints this when logged out. The real codex's
            // logged-out text has not been verified; if it reads differently,
            // the only cost is that the run stays `failed` - never a false
            // logout.
            signed_out: stdout.contains("Not logged in") || stderr.contains("Not logged in"),
            ..Default::default()
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
        match ty {
            "thread.started" => StreamEvent::Init {
                session_id: s(&v["thread_id"]),
                model: None,
                cwd: None,
                tools: None,
            },
            "turn.completed" => {
                let usage = &v["usage"];
                let input_tokens = usage["input_tokens"].as_i64();
                let output_tokens = usage["output_tokens"].as_i64();
                // OpenAI's `input_tokens` already holds the cached part and
                // `output_tokens` the reasoning part. `cache_write` stays out
                // of the total: the samples are all 0, so they cannot show
                // whether it is inside `input_tokens`; leaving it out never
                // counts a token twice.
                let total_tokens = if input_tokens.is_none() && output_tokens.is_none() {
                    None
                } else {
                    Some(input_tokens.unwrap_or(0) + output_tokens.unwrap_or(0))
                };
                StreamEvent::Result(ResultEvent {
                    is_error: false,
                    subtype: Some(ty.to_string()),
                    input_tokens,
                    output_tokens,
                    cache_read_tokens: usage["cached_input_tokens"].as_i64(),
                    cache_write_tokens: usage["cache_write_input_tokens"].as_i64(),
                    reasoning_tokens: usage["reasoning_output_tokens"].as_i64(),
                    total_tokens,
                    // The usage is the whole thread's so far, across
                    // `exec resume` too.
                    usage_cumulative: true,
                    ..Default::default()
                })
            }
            "turn.failed" => StreamEvent::Result(error_result(s(&v["error"]["message"]), ty)),
            "error" => StreamEvent::HarnessError(error_result(s(&v["message"]), ty)),
            "item.started" | "item.updated" | "item.completed" => {
                let item = &v["item"];
                let item_type = item["type"].as_str().unwrap_or("").to_string();
                if ty == "item.completed" && item_type == "command_execution" {
                    StreamEvent::CommandCompleted {
                        item_id: s(&item["id"]),
                        command: s(&item["command"]).unwrap_or_default(),
                        output: s(&item["aggregated_output"]).unwrap_or_default(),
                        exit_code: item["exit_code"].as_i64(),
                        status: s(&item["status"]),
                    }
                } else {
                    StreamEvent::Item {
                        id: s(&item["id"]),
                        item_type,
                    }
                }
            }
            other => StreamEvent::Other {
                kind: other.to_string(),
            },
        }
    }

    fn check_model(&self, profile_id: &str, model: &str, known_models: &str) -> Result<()> {
        check_model(profile_id, model, known_models)
    }

    /// Codex names its thread only in `thread.started`.
    fn names_session_late(&self) -> bool {
        true
    }

    /// Scope §2.4: the reviewer's sandbox cannot open the database (E3a).
    fn reads_review_packet(&self) -> bool {
        true
    }

    /// The index is in the packet file it is told to read.
    fn build_review_prompt(
        &self,
        task_id: &str,
        spec: &crate::spec::TaskSpec,
        criterion_ids: &[String],
        _thoth_exe: &str,
        diff_code: Option<&str>,
        _index_rows: &[crate::evidence::IndexRow],
        // In the packet file too (`## Protected files changed`).
        _protected_changes: &[String],
        sources: Option<&[crate::review_json::SourceToCheck]>,
    ) -> String {
        if let Some(sources) = sources {
            return super::source_check_prompt(task_id, "codex", sources);
        }
        build_review_prompt(task_id, spec, criterion_ids, diff_code)
    }

    /// SR-2: a read-only sandbox; the final message is the document.
    fn build_spec_review_prompt(
        &self,
        task_id: &str,
        spec: &crate::spec::TaskSpec,
        criterion_ids: &[String],
        mode: crate::spec_edits::ReviewMode,
    ) -> String {
        crate::spec_review_prompt::build(
            task_id,
            spec,
            criterion_ids,
            crate::spec_review_prompt::How::FinalMessageWithShell,
            mode,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::ReviewOutput;
    use std::path::PathBuf;

    macro_rules! fixture {
        ($name:literal) => {
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/codex/", $name))
        };
    }
    const OK_AND_FAILING: &str = fixture!("exec-ok-and-failing-command.jsonl");
    const BAD_MODEL: &str = fixture!("exec-error-bad-model.jsonl");
    const RESUME: &str = fixture!("exec-resume.jsonl");
    const COMMIT: &str = fixture!("exec-commit-in-worktree.jsonl");
    const REVIEW: &str = fixture!("exec-review-output-schema.jsonl");
    const TRUNCATED: &str = fixture!("exec-truncated-output.jsonl");

    fn adapter() -> CodexAdapter {
        CodexAdapter { bin: "codex".to_string() }
    }

    fn parse_all(s: &str) -> Vec<StreamEvent> {
        s.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| adapter().parse_stream_line(l))
            .collect()
    }

    fn result_on_line(text: &str, n: usize, ty: &str) -> ResultEvent {
        let line = text.lines().nth(n - 1).unwrap();
        assert!(line.contains(ty), "line {n} is not a {ty}: {line}");
        match adapter().parse_stream_line(line) {
            StreamEvent::Result(r) => r,
            other => panic!("not a result: {other:?}"),
        }
    }

    // DT-1 u2: turn.completed carries the whole thread's usage.
    #[test]
    fn turn_completed_carries_every_token_kind_and_a_cumulative_usage() {
        let r = result_on_line(COMMIT, 7, "turn.completed");
        assert_eq!(r.input_tokens, Some(22534));
        assert_eq!(r.cache_read_tokens, Some(11008));
        assert_eq!(r.cache_write_tokens, Some(0));
        assert_eq!(r.output_tokens, Some(157));
        assert_eq!(r.reasoning_tokens, Some(22));
        assert_eq!(r.total_tokens, Some(22691));
        assert!(r.usage_cumulative);

        let r = result_on_line(REVIEW, 7, "turn.completed");
        assert_eq!(r.input_tokens, Some(21581));
        assert_eq!(r.cache_read_tokens, Some(9984));
        assert_eq!(r.cache_write_tokens, Some(0));
        assert_eq!(r.output_tokens, Some(326));
        assert_eq!(r.reasoning_tokens, Some(107));
        assert_eq!(r.total_tokens, Some(21907));
        assert!(r.usage_cumulative);

        // turn.failed carries no usage
        let r = result_on_line(BAD_MODEL, 5, "turn.failed");
        assert_eq!(r.input_tokens, None);
        assert_eq!(r.output_tokens, None);
        assert_eq!(r.cache_read_tokens, None);
        assert_eq!(r.cache_write_tokens, None);
        assert_eq!(r.reasoning_tokens, None);
        assert_eq!(r.total_tokens, None);
    }

    fn strings(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    fn request(resume: Option<&'static str>) -> LaunchRequest<'static> {
        LaunchRequest {
            prompt: "PROMPT-BODY".to_string(),
            model: "gpt-5.6-luna",
            session_id: "ignored-for-codex",
            resume_session_id: resume,
            allowed_tools: "Bash,Read,Write,Edit,Glob,Grep",
            config_dir: "/home/u/.thoth/profiles/codex-a",
            cwd: PathBuf::from("/wt/task-a1"),
            settings_path: None,
            extra_env: vec![("THOTH_RUN_ID".to_string(), "01RUN".to_string())],
            review_output: None,
            timeout_seconds: 1800,
            effort: None,
            research: false,
        }
    }

    /// RES-1a AC3: a research implementer's four tokens sit right after the
    /// sandbox pair (fresh `-s`, resume `-c sandbox_mode`) and before the
    /// Windows pair. Literal strings, not the constants: a misspelt key would
    /// be silently ignored by codex, so the test pins the spelling.
    #[test]
    fn build_command_opens_the_network_for_a_research_implementer() {
        for resume in [None, Some("01a0e237-7be7-77b2-88de-ea7ef9beec3c")] {
            for windows in [false, true] {
                let mut req = request(resume);
                req.research = true;
                let plan = adapter().build_command_for(&req, windows);
                let mut expected = vec!["exec", "--json", "-m", "gpt-5.6-luna"];
                if resume.is_some() {
                    expected.extend(["-c", "sandbox_mode=\"workspace-write\""]);
                } else {
                    expected.extend(["-s", "workspace-write"]);
                }
                expected.extend([
                    "-c", "sandbox_workspace_write.network_access=true",
                    "-c", "web_search=\"live\"",
                ]);
                if windows {
                    expected.extend(["-c", "windows.sandbox=\"unelevated\""]);
                }
                expected.extend([
                    "-c", "approval_policy=\"never\"",
                    "--disable", "multi_agent",
                    "--disable", "apps",
                    "--disable", "browser_use",
                    "--disable", "computer_use",
                    "--ignore-user-config",
                    "-C", "/wt/task-a1",
                ]);
                if let Some(thread) = resume {
                    expected.extend(["resume", thread]);
                }
                expected.push("-");
                assert_eq!(
                    plan.args,
                    strings(&expected),
                    "resume={resume:?} windows={windows}"
                );
            }
        }
    }

    #[test]
    fn build_command_source_checker_adds_only_live_search_after_read_only_sandbox() {
        for windows in [false, true] {
            let mut req = request(None);
            req.review_output = Some(crate::adapter::ReviewOutput {
                schema: "/run/review-schema.json".into(),
                output: "/wt/.agent/review.json".into(),
            });
            let plain = adapter().build_command_for(&req, windows);
            req.research = true;
            let research = adapter().build_command_for(&req, windows);
            let mut expected = plain.args;
            let sandbox = expected.windows(2).position(|w| w == ["-s", "read-only"]).unwrap();
            expected.splice(sandbox+2..sandbox+2, ["-c".into(), "web_search=\"live\"".into()]);
            assert_eq!(research.args, expected);
            assert!(!research.args.iter().any(|a| a.contains("network_access")));
        }
    }

    #[test]
    fn build_command_is_the_scope_command_on_a_fresh_run() {
        let plan = adapter().build_command_for(&request(None), false);
        assert_eq!(plan.program, "codex");
        assert_eq!(
            plan.args,
            strings(&[
                "exec", "--json",
                "-m", "gpt-5.6-luna",
                "-s", "workspace-write",
                "-c", "approval_policy=\"never\"",
                "--disable", "multi_agent",
                "--disable", "apps",
                "--disable", "browser_use",
                "--disable", "computer_use",
                "--ignore-user-config",
                "-C", "/wt/task-a1",
                "-",
            ])
        );
        assert_eq!(
            plan.env,
            vec![
                ("CODEX_HOME".to_string(), "/home/u/.thoth/profiles/codex-a".to_string()),
                ("THOTH_RUN_ID".to_string(), "01RUN".to_string()),
            ],
            "CODEX_HOME first so it cannot be shadowed"
        );
        assert_eq!(plan.cwd, PathBuf::from("/wt/task-a1"));
        assert_eq!(plan.stdin.as_deref(), Some("PROMPT-BODY"));
    }

    /// Measured on codex-cli 0.154 (2026-09-27): every option goes before the
    /// `resume` subcommand; `exec resume <id> -C ...` and `... -s ...` are
    /// rejected as unexpected arguments.
    #[test]
    fn build_command_resumes_the_thread_with_every_option_before_resume() {
        let mut req = request(Some("01a0e237-7be7-77b2-88de-ea7ef9beec3c"));
        req.prompt = crate::adapter::claude_code::RESUME_PROMPT.to_string();
        let plan = adapter().build_command_for(&req, false);
        assert_eq!(
            plan.args,
            strings(&[
                "exec", "--json",
                "-m", "gpt-5.6-luna",
                "-c", "sandbox_mode=\"workspace-write\"",
                "-c", "approval_policy=\"never\"",
                "--disable", "multi_agent",
                "--disable", "apps",
                "--disable", "browser_use",
                "--disable", "computer_use",
                "--ignore-user-config",
                "-C", "/wt/task-a1",
                "resume", "01a0e237-7be7-77b2-88de-ea7ef9beec3c",
                "-",
            ])
        );
        assert_eq!(plan.env[0].0, "CODEX_HOME");
        assert_eq!(plan.stdin.as_deref(), Some(crate::adapter::claude_code::RESUME_PROMPT));
    }

    /// Scope §2.4 / E3b: a reviewer is read-only, and its verdict is the
    /// schema-constrained final message codex itself writes to `-o`.
    #[test]
    fn build_command_for_a_reviewer_is_read_only_with_the_schema_and_output_file() {
        let mut req = request(None);
        req.cwd = PathBuf::from("/wt/task-a1-review");
        req.review_output = Some(ReviewOutput {
            schema: PathBuf::from("/rd/review-schema.json"),
            output: PathBuf::from("/wt/task-a1-review/.agent/review.json"),
        });
        let plan = adapter().build_command_for(&req, false);
        assert_eq!(
            plan.args,
            strings(&[
                "exec", "--json",
                "-m", "gpt-5.6-luna",
                "-s", "read-only",
                "-c", "approval_policy=\"never\"",
                "--disable", "multi_agent",
                "--disable", "apps",
                "--disable", "browser_use",
                "--disable", "computer_use",
                "--ignore-user-config",
                "--output-schema", "/rd/review-schema.json",
                "-o", "/wt/task-a1-review/.agent/review.json",
                "-C", "/wt/task-a1-review",
                "-",
            ])
        );
        assert_eq!(plan.env[0], ("CODEX_HOME".to_string(), "/home/u/.thoth/profiles/codex-a".to_string()));
        assert_eq!(plan.stdin.as_deref(), Some("PROMPT-BODY"));
    }

    fn effort_request(resume: Option<&'static str>) -> LaunchRequest<'static> {
        let mut req = request(resume);
        req.effort = Some("high".to_string());
        req
    }

    /// The `-c model_reasoning_effort` pair follows `-m MODEL` directly, so it
    /// is also before `resume`; the windows flag decides only the sandbox pair.
    #[test]
    fn effort_pair_follows_the_model_on_a_fresh_run_with_and_without_the_windows_flag() {
        for windows in [false, true] {
            let plan = adapter().build_command_for(&effort_request(None), windows);
            let mut expected = vec![
                "exec", "--json",
                "-m", "gpt-5.6-luna",
                "-c", "model_reasoning_effort=\"high\"",
                "-s", "workspace-write",
            ];
            if windows {
                expected.extend(["-c", "windows.sandbox=\"unelevated\""]);
            }
            expected.extend([
                "-c", "approval_policy=\"never\"",
                "--disable", "multi_agent",
                "--disable", "apps",
                "--disable", "browser_use",
                "--disable", "computer_use",
                "--ignore-user-config",
                "-C", "/wt/task-a1",
                "-",
            ]);
            assert_eq!(plan.args, strings(&expected), "windows={windows}");
            assert_eq!(plan.stdin.as_deref(), Some("PROMPT-BODY"), "windows={windows}");
        }
    }

    #[test]
    fn effort_pair_follows_the_model_on_a_resume_with_and_without_the_windows_flag() {
        for windows in [false, true] {
            let mut req = effort_request(Some("01a0e237-7be7-77b2-88de-ea7ef9beec3c"));
            req.prompt = crate::adapter::claude_code::RESUME_PROMPT.to_string();
            let plan = adapter().build_command_for(&req, windows);
            let mut expected = vec![
                "exec", "--json",
                "-m", "gpt-5.6-luna",
                "-c", "model_reasoning_effort=\"high\"",
                "-c", "sandbox_mode=\"workspace-write\"",
            ];
            if windows {
                expected.extend(["-c", "windows.sandbox=\"unelevated\""]);
            }
            expected.extend([
                "-c", "approval_policy=\"never\"",
                "--disable", "multi_agent",
                "--disable", "apps",
                "--disable", "browser_use",
                "--disable", "computer_use",
                "--ignore-user-config",
                "-C", "/wt/task-a1",
                "resume", "01a0e237-7be7-77b2-88de-ea7ef9beec3c",
                "-",
            ]);
            assert_eq!(plan.args, strings(&expected), "windows={windows}");
            assert_eq!(plan.stdin.as_deref(), Some(crate::adapter::claude_code::RESUME_PROMPT), "windows={windows}");
        }
    }

    #[test]
    fn effort_pair_follows_the_model_for_a_reviewer_with_and_without_the_windows_flag() {
        for windows in [false, true] {
            let mut req = effort_request(None);
            req.cwd = PathBuf::from("/wt/task-a1-review");
            req.review_output = Some(ReviewOutput {
                schema: PathBuf::from("/rd/review-schema.json"),
                output: PathBuf::from("/wt/task-a1-review/.agent/review.json"),
            });
            let plan = adapter().build_command_for(&req, windows);
            let mut expected = vec![
                "exec", "--json",
                "-m", "gpt-5.6-luna",
                "-c", "model_reasoning_effort=\"high\"",
                "-s", "read-only",
            ];
            if windows {
                expected.extend(["-c", "windows.sandbox=\"unelevated\""]);
            }
            expected.extend([
                "-c", "approval_policy=\"never\"",
                "--disable", "multi_agent",
                "--disable", "apps",
                "--disable", "browser_use",
                "--disable", "computer_use",
                "--ignore-user-config",
                "--output-schema", "/rd/review-schema.json",
                "-o", "/wt/task-a1-review/.agent/review.json",
                "-C", "/wt/task-a1-review",
                "-",
            ]);
            assert_eq!(plan.args, strings(&expected), "windows={windows}");
            assert_eq!(plan.stdin.as_deref(), Some("PROMPT-BODY"), "windows={windows}");
        }
    }

    #[test]
    fn the_reviewer_prompt_points_at_the_input_files_and_forbids_thoth() {
        let spec = crate::spec::TaskSpec {
            goal: "say hello".to_string(),
            acceptance_criteria: vec!["AC1: prints hello".to_string(), "AC2: exits zero".to_string()],
            constraints: vec!["no new deps".to_string()],
            non_goals: vec![],
            excluded_alternatives: vec![],
            risk: "normal".to_string(),
            checks: vec![],
        };
        let ids = vec!["AC1".to_string(), "AC2".to_string()];
        let p = build_review_prompt("01TASK", &spec, &ids, Some("E004"));
        for line in [
            "Read .agent/review-input/packet.md first.",
            ".agent/review-input/evidence/<CODE>.txt",
            "Do not run thoth",
            "Read the candidate diff before anything else: .agent/review-input/evidence/E004.txt",
            "Judge every acceptance criterion exactly once",
            "Every met criterion must cite at least one code from the packet's evidence index",
            "Your final message is the review document",
            "Every id in \"criteria\" must be one of: AC1, AC2.",
            "AC1 — AC1: prints hello",
            "This review is for task 01TASK.",
        ] {
            assert!(p.contains(line), "missing {line:?} in:\n{p}");
        }
        assert!(!p.contains("evidence show"), "{p}");
        let no_diff = build_review_prompt("01TASK", &spec, &ids, None);
        assert!(no_diff.contains("No diff evidence was captured"), "{no_diff}");
    }

    #[test]
    fn build_review_prompt_asks_for_fix_size_category_and_interface_tests() {
        let spec: crate::spec::TaskSpec = crate::spec::parse_spec(
            r#"{"goal":"g","acceptance_criteria":["AC1: a","AC2: b"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low"}"#,
        ).unwrap();
        let ids = vec!["AC1".to_string(), "AC2".to_string()];
        let p = build_review_prompt("01TASK", &spec, &ids, Some("E004"));
        let rules = p.find("\n## Rules\n").unwrap();
        let output = p.find("\n## Output\n").unwrap();
        let rule = p.find(crate::review_json::INTERFACE_TEST_RULE).expect("the interface rule");
        assert!(rules < rule && rule < output, "{p}");
        assert!(p.contains("\n5. New or changed tests must check behaviour through the interface"), "{p}");
        // TESTS-1 (AC6): the suite-evidence rule is rule 6, right after the
        // interface rule, once, and the last rule before ## Output
        const SUITE_RULE: &str = "The implementer is told to run only the tests the spec names; the whole suite is run by the control plane's verify, which appears in the evidence index as rows labelled verify: with collected_by control_plane. Do not judge a criterion unmet or UNVERIFIED because the implementer did not run the whole suite itself, and do not count an implementer's cut-off or empty whole-suite output as missing evidence when a verify row covers the same check. Use only a candidate verify row for the same run and check, with successful completion and usable output; a verify@base: row, a failed, refused, timed-out or incomplete verify does not prove that check passed. When no usable verify row exists for a check, report what is missing in an extra finding with category evidence; do not treat an existing row as success or waive an explicit evidence requirement.";
        let suite = p.find(SUITE_RULE).expect("the suite rule");
        assert!(rule < suite && suite < output, "{p}");
        assert_eq!(p.matches(SUITE_RULE).count(), 1, "{p}");
        assert!(p.contains(&format!(
            "\n5. {}\n6. {SUITE_RULE}\n\n## Output\n",
            crate::review_json::INTERFACE_TEST_RULE
        )), "{p}");
        assert!(!p.contains("\n7. "), "{p}");
        assert!(p.contains("\"verdict\":\"met\",\"evidence\":[\"E001\"],\"note\":\"\",\"fix_size\":\"none\",\"category\":\"none\"}"), "{p}");
        assert!(p.contains("\"note\":\"...\",\"evidence\":[\"E001\"],\"fix_size\":\"S\",\"category\":\"docs\"}"), "{p}");
        let after = &p[output..];
        for line in [
            "Every id in \"criteria\" must be one of: AC1, AC2.\nEvery unmet criterion and every extra finding carries \"fix_size\" and \"category\". \"fix_size\" is the smallest fix that would resolve it: S (at most 10 changed lines in one file), M (at most about 50 changed lines, or more than one file, no design change), L (larger, or it needs a design or spec change).\n",
            "Every \"fix_size\" must be one of: S, M, L.\n",
            "Every \"category\" must be one of: spec_mismatch, spec_error, test_missing, test_interface, error_path, concurrency, platform, docs, evidence, files, security, other.\n",
            "- spec_mismatch: the change does not do what a criterion says, or does what a constraint or non-goal rules out\n",
            "- other: none of the above\nPut \"none\" in both on a met criterion.\n",
        ] {
            assert!(after.contains(line), "missing {line:?} in:\n{after}");
        }
        assert!(!p.contains("evidence show"), "{p}");
    }

    /// `--ignore-user-config` leaves codex on Windows without a sandbox
    /// implementation (measured 2026-09-29), so all three launch shapes carry
    /// `-c windows.sandbox="unelevated"`, before `-C` and before `resume`.
    #[test]
    fn windows_launches_carry_the_sandbox_pair_before_dash_c_and_resume() {
        let mut reviewer = request(None);
        reviewer.review_output = Some(ReviewOutput {
            schema: PathBuf::from("/rd/review-schema.json"),
            output: PathBuf::from("/wt/task-a1/.agent/review.json"),
        });
        let shapes = [request(None), request(Some("01a0e237-7be7-77b2-88de-ea7ef9beec3c")), reviewer];
        for req in &shapes {
            let on = adapter().build_command_for(req, true).args;
            let off = adapter().build_command_for(req, false).args;
            assert_eq!(adapter().build_command_for(req, true).stdin.as_deref(), Some(req.prompt.as_str()));
            let pair = strings(&["-c", "windows.sandbox=\"unelevated\""]);
            let at = on
                .windows(2)
                .position(|w| *w == pair[..])
                .unwrap_or_else(|| panic!("no pair: {on:?}"));
            assert!(at < on.iter().position(|a| a == "-C").unwrap(), "{on:?}");
            if let Some(r) = on.iter().position(|a| a == "resume") {
                assert!(at < r, "{on:?}");
            }
            let mut without = on.clone();
            without.drain(at..at + 2);
            assert_eq!(without, off, "only the pair differs");
            assert!(!off.iter().any(|a| a.contains("windows.sandbox")), "{off:?}");
        }
    }

    #[test]
    fn build_command_ignores_allowed_tools_and_settings() {
        let mut req = request(None);
        req.allowed_tools = "Bash";
        req.settings_path = Some(PathBuf::from("/rd/hooks-settings.json"));
        let plan = adapter().build_command(&req);
        assert_eq!(plan.args, adapter().build_command(&request(None)).args);
        assert_eq!(plan.stdin.as_deref(), Some("PROMPT-BODY"));
    }

    #[test]
    fn auth_command_is_login_status_under_codex_home() {
        let plan = adapter().auth_command("/cfg/codex-a", Path::new("/somewhere"));
        assert_eq!(plan.program, "codex");
        assert_eq!(plan.args, strings(&["login", "status"]));
        assert_eq!(plan.env, vec![("CODEX_HOME".to_string(), "/cfg/codex-a".to_string())]);
        assert_eq!(plan.cwd, PathBuf::from("/somewhere"));
        assert_eq!(plan.stdin, None);
    }

    #[test]
    fn login_command_is_codex_login_under_codex_home() {
        let plan = adapter().login_command("/cfg/codex-a", Path::new("/somewhere"));
        assert_eq!(plan.program, "codex");
        assert_eq!(plan.args, strings(&["login"]));
        assert_eq!(plan.env, vec![("CODEX_HOME".to_string(), "/cfg/codex-a".to_string())]);
        assert_eq!(plan.cwd, PathBuf::from("/somewhere"));
        assert_eq!(plan.stdin, None);
    }

    #[test]
    fn logged_in_needs_exit_0_and_the_logged_in_line_on_either_stream() {
        let a = adapter();
        let s = a.parse_auth_output(true, "", "Logged in using ChatGPT\n").unwrap();
        assert!(s.logged_in);
        assert_eq!(s.email, None);
        assert_eq!(s.subscription_type, None);
        assert!(a.parse_auth_output(true, "Logged in using ChatGPT\n", "").unwrap().logged_in);
        assert!(!a.parse_auth_output(false, "", "Logged in using ChatGPT\n").unwrap().logged_in);
        assert!(!a.parse_auth_output(true, "", "Not logged in\n").unwrap().logged_in);
        assert!(!a.parse_auth_output(false, "", "Not logged in\n").unwrap().logged_in);
    }

    #[test]
    fn signed_out_is_true_only_when_the_cli_itself_says_so() {
        let a = adapter();
        assert!(a.parse_auth_output(false, "", "Not logged in").unwrap().signed_out);
        assert!(!a.parse_auth_output(false, "", "boom").unwrap().signed_out);
        assert!(!a.parse_auth_output(true, "", "Logged in using ChatGPT").unwrap().signed_out);
    }

    #[test]
    fn claude_model_names_are_refused_with_the_codex_models_listed() {
        let known = CODEX_MODELS.join(", ");
        for m in ["haiku", "sonnet", "opus", "fable", "Sonnet", "claude-sonnet-5", "claude-haiku-4-5-20251001"] {
            assert!(is_claude_model(m), "{m}");
            let e = check_model("codex-a", m, &known).unwrap_err().to_string();
            assert!(e.contains("Claude model"), "{e}");
            assert!(e.contains("codex-a"), "{e}");
            for c in CODEX_MODELS {
                assert!(e.contains(c), "{e}");
            }
        }
        for m in CODEX_MODELS.iter().copied().chain(["gpt-9", "o5-mini"]) {
            assert!(!is_claude_model(m), "{m}");
            check_model("codex-a", m, &known).unwrap();
        }
    }

    #[test]
    fn the_ok_fixture_yields_the_thread_two_commands_and_the_usage() {
        let evs = parse_all(OK_AND_FAILING);
        match &evs[0] {
            StreamEvent::Init { session_id, model, cwd, tools } => {
                assert_eq!(session_id.as_deref(), Some("01a0e237-7be7-77b2-88de-ea7ef9beec3c"));
                assert_eq!((model, cwd), (&None, &None));
                // AGY-1: only agy reports a tool list
                assert_eq!(tools, &None);
            }
            other => panic!("expected Init, got {other:?}"),
        }
        assert!(matches!(&evs[1], StreamEvent::Other { kind } if kind == "turn.started"));
        let commands: Vec<&StreamEvent> = evs
            .iter()
            .filter(|e| matches!(e, StreamEvent::CommandCompleted { .. }))
            .collect();
        assert_eq!(commands.len(), 2);
        match commands[0] {
            StreamEvent::CommandCompleted { item_id, command, output, exit_code, status } => {
                assert_eq!(item_id.as_deref(), Some("item_1"));
                assert_eq!(command, "/bin/bash -lc 'echo hello-stdout'");
                assert_eq!(output, "hello-stdout\n");
                assert_eq!(*exit_code, Some(0));
                assert_eq!(status.as_deref(), Some("completed"));
            }
            _ => unreachable!(),
        }
        match commands[1] {
            StreamEvent::CommandCompleted { command, output, exit_code, status, .. } => {
                assert_eq!(command, "/bin/bash -lc 'sh -c \"echo oops >&2; exit 3\"'");
                assert_eq!(output, "oops\n");
                assert_eq!(*exit_code, Some(3));
                assert_eq!(status.as_deref(), Some("failed"));
            }
            _ => unreachable!(),
        }
        // item.started of a command, file_change and agent_message are items
        let items: Vec<&str> = evs
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Item { item_type, .. } => Some(item_type.as_str()),
                _ => None,
            })
            .collect();
        assert!(items.contains(&"agent_message"));
        assert!(items.contains(&"file_change"));
        assert!(items.contains(&"command_execution"));
        match evs.last().unwrap() {
            StreamEvent::Result(r) => {
                assert!(!r.is_error);
                assert!(!r.quota);
                assert_eq!(r.subtype.as_deref(), Some("turn.completed"));
                assert_eq!(r.input_tokens, Some(53850));
                assert_eq!(r.output_tokens, Some(146));
                assert_eq!(r.session_id, None);
                assert_eq!(r.model_usage, None, "the stream names no model");
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn the_bad_model_fixture_yields_an_error_then_a_failed_turn_that_is_not_quota() {
        let evs = parse_all(BAD_MODEL);
        assert!(matches!(&evs[0], StreamEvent::Init { .. }));
        // the warning item is an item of type error, not a harness failure
        assert!(matches!(&evs[1], StreamEvent::Item { item_type, .. } if item_type == "error"));
        match &evs[3] {
            StreamEvent::HarnessError(r) => {
                assert!(r.is_error);
                assert!(!r.quota);
                assert_eq!(r.api_error_status.as_deref(), Some("400"));
                assert!(r.text.as_deref().unwrap().contains("model is not supported"));
            }
            other => panic!("expected HarnessError, got {other:?}"),
        }
        match &evs[4] {
            StreamEvent::Result(r) => {
                assert!(r.is_error);
                assert!(!r.quota);
                assert_eq!(r.subtype.as_deref(), Some("turn.failed"));
                assert_eq!(r.api_error_status.as_deref(), Some("400"));
                assert!(r.text.as_deref().unwrap().contains("model is not supported"));
            }
            other => panic!("expected Result, got {other:?}"),
        }
    }

    #[test]
    fn the_resume_commit_and_review_fixtures_parse() {
        let evs = parse_all(RESUME);
        assert!(matches!(&evs[0], StreamEvent::Init { session_id: Some(s), .. } if s == "01a0e237-7be7-77b2-88de-ea7ef9beec3c"));
        assert!(matches!(evs.last().unwrap(), StreamEvent::Result(r) if r.output_tokens == Some(213)));

        let evs = parse_all(COMMIT);
        let cmd = evs.iter().find_map(|e| match e {
            StreamEvent::CommandCompleted { command, exit_code, .. } => Some((command.clone(), *exit_code)),
            _ => None,
        });
        assert_eq!(
            cmd,
            Some(("/bin/bash -lc 'git add c.txt && git commit -m add-c'".to_string(), Some(0)))
        );
        assert!(matches!(evs.last().unwrap(), StreamEvent::Result(r) if r.output_tokens == Some(157)));

        let evs = parse_all(REVIEW);
        assert_eq!(
            evs.iter().filter(|e| matches!(e, StreamEvent::CommandCompleted { .. })).count(),
            1
        );
        assert!(matches!(evs.last().unwrap(), StreamEvent::Result(r) if r.output_tokens == Some(326)));
    }

    #[test]
    fn the_truncated_fixture_carries_the_omitted_bytes_marker() {
        let evs = parse_all(TRUNCATED);
        let output = evs
            .iter()
            .find_map(|e| match e {
                StreamEvent::CommandCompleted { output, .. } => Some(output.clone()),
                _ => None,
            })
            .unwrap();
        assert!(output_was_truncated(&output));
        assert!(!output_was_truncated("hello-stdout\n"));
        assert!(!output_was_truncated("... some bytes omitted ...\n"));
        assert!(!output_was_truncated("1 bytes omitted"));
        assert!(output_was_truncated("head\n... 7 bytes omitted ...\ntail"));
    }

    /// Built in the E6 shape with the usage-limit strings found in the codex
    /// binary. Unverified against a real codex usage-limit error (scope §3).
    #[test]
    fn a_usage_limit_error_in_the_e6_shape_is_quota() {
        let inner = serde_json::json!({"type": "error", "status": 429, "error": {
            "type": "usage_limit_reached",
            "message": "You've hit your usage limit. Try again later."}})
        .to_string();
        let line = serde_json::json!({"type": "turn.failed", "error": {"message": inner}}).to_string();
        match adapter().parse_stream_line(&line) {
            StreamEvent::Result(r) => {
                assert!(r.is_error);
                assert!(r.quota);
                assert_eq!(r.api_error_status.as_deref(), Some("429"));
            }
            other => panic!("expected Result, got {other:?}"),
        }
        for m in [
            "usage_limit_reached",
            "rate_limit_reached",
            "UsageLimitExceeded",
            "You've hit your usage limit.",
            r#"{"status":429}"#,
        ] {
            assert!(looks_like_codex_quota(m), "{m}");
        }
        assert!(!looks_like_codex_quota(
            r#"{"type":"error","status":400,"error":{"type":"invalid_request_error","message":"The 'x' model is not supported"}}"#
        ));
    }

    #[test]
    fn broken_and_unknown_lines_do_not_panic() {
        assert!(matches!(adapter().parse_stream_line("half {"), StreamEvent::Unparsable { raw_len: 6 }));
        assert!(matches!(
            adapter().parse_stream_line(r#"{"type":"something.new"}"#),
            StreamEvent::Other { kind } if kind == "something.new"
        ));
        assert!(matches!(
            adapter().parse_stream_line(r#"{"type":"item.completed","item":{"id":"i9","type":"mcp_tool_call"}}"#),
            StreamEvent::Item { id: Some(i), item_type } if i == "i9" && item_type == "mcp_tool_call"
        ));
    }
}
