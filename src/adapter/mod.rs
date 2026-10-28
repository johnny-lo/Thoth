pub mod antigravity;
pub mod claude_code;
pub mod codex;
pub mod effort;

use crate::config::Paths;
use crate::db::state;
use anyhow::Result;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// Everything the adapter needs to build one harness invocation.
pub struct LaunchRequest<'a> {
    pub prompt: String,
    pub model: &'a str,
    /// Pre-generated uuid v4 handed to `--session-id` on a fresh run.
    pub session_id: &'a str,
    /// When set, the run is a resume: `--resume <id>` replaces `--session-id`.
    pub resume_session_id: Option<&'a str>,
    pub allowed_tools: &'a str,
    pub config_dir: &'a str,
    pub cwd: PathBuf,
    /// brief §4: the per-run `hooks-settings.json` handed to `--settings`.
    /// `None` for a run that is not hooked (a reviewer run — Open question 4).
    pub settings_path: Option<PathBuf>,
    /// Extra environment for the child, appended after `CLAUDE_CONFIG_DIR`:
    /// `THOTH_RUN_ID`, `THOTH_EXE`, and for hooked runs
    /// `THOTH_INBOX_DIR`.
    pub extra_env: Vec<(String, String)>,
    /// A reviewer run that reads the review packet
    /// (`HarnessAdapter::reads_review_packet`): its verdict is the final
    /// message, shaped by `schema`. Codex writes it to `output` itself (scope
    /// §2.4, E3b); an adapter without an output-file flag leaves `output` to
    /// `HarnessAdapter::write_review_output`. `None` for every other run;
    /// Claude Code ignores it.
    pub review_output: Option<ReviewOutput>,
    /// The run's wall-clock limit in seconds (`runs.timeout_seconds`). Only
    /// agy uses it, to size its own `--print-timeout` above this one.
    pub timeout_seconds: i64,
    /// The effort level actually sent (`effort::resolve`'s `sent`), or `None`
    /// to leave the harness default alone.
    pub effort: Option<String>,
    /// A worker or source-check reviewer of a research task (`spec::is_research_task` on the
    /// run's spec copy); always `false` for a spec reviewer. Only
    /// codex reads it, to enable web search (and the worker sandbox's network): Claude Code and agy get
    /// their web tools through `runs.allowed_tools` instead.
    pub research: bool,
}

/// The comma-separated tool `list` with `extra` appended, skipping any already
/// present (compared after trimming). An empty `list` stays empty: it means
/// "no tool lock" to Claude Code, so it must never become "only these".
pub fn with_tools(list: &str, extra: &[&str]) -> String {
    if list.is_empty() {
        return String::new();
    }
    let mut out = list.to_string();
    for tool in extra {
        if !list.split(',').any(|t| t.trim() == *tool) {
            out.push(',');
            out.push_str(tool);
        }
    }
    out
}

/// Source checking deliberately has no diff, evidence index or acceptance
/// criteria rules: the independent judgement is about each cited page.
pub fn source_check_prompt(
    task_id: &str,
    harness: &str,
    sources: &[crate::review_json::SourceToCheck],
) -> String {
    let (open, output) = match harness {
        "codex" => (
            "Use web_search to open each URL. For file:// URLs, use cat to read the local path.",
            "Your final message is the source-check document, constrained by --output-schema.",
        ),
        "antigravity" => (
            "Use read_url_content to open each URL. For file:// URLs, use read_file to read the local path.",
            "Answer with the source-check document in structured_output, constrained by --json-schema.",
        ),
        _ => (
            "Use WebFetch to open each URL. For file:// URLs, use Read or Bash to read the local path.",
            "Use a shell redirect to write the source-check document to .agent/review.json.",
        ),
    };
    format!(
        "You are an independent source checker for task {task_id}. Do not trust the worker's summary.\n\
         Open every source URL yourself and compare its quote with the page and its claim_text. {open}\n\
         supported: the quote appears verbatim or substantially on the page and supports the claim.\n\
         not_supported: the page opens but the quote is absent or does not support (or contradicts) the claim.\n\
         unreachable: the page cannot be opened, times out, or requires login.\n\
         Give a nonblank reason for every not_supported or unreachable judgement.\n\n\
         ## Sources to check\n{}\n\n\
         ## Output\n{output}\n\
         Use this shape (no criteria):\n\
         {{\"verdict\":\"PASS\",\"findings\":{{\"sources\":[{{\"id\":\"S1\",\"url\":\"...\",\"verdict\":\"supported\",\"reason\":\"\"}}],\"extra_findings\":[]}}}}\n\
         Judge each listed id exactly once; do not add ids. Copy its url back unchanged.\n\
         Verdict: any not_supported means REJECT; otherwise any unreachable means UNVERIFIED; otherwise all supported means PASS.\n\
         extra_findings use severity, note, evidence, fix_size and category.\n{}",
        serde_json::to_string(sources).expect("source list serializes"),
        crate::review_json::finding_labels_prompt(),
    )
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewOutput {
    /// `--output-schema`: the review.json schema, in the run dir.
    pub schema: PathBuf,
    /// `-o`: `.agent/review.json` in the review worktree.
    pub output: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandPlan {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// Bytes the runner writes to the child's stdin (then closes it), or
    /// `None` for a null stdin. Claude and codex take the prompt here so it
    /// stays off the command line (W3).
    pub stdin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthStatus {
    pub logged_in: bool,
    /// The CLI itself said this profile is not logged in (FC-1). `false` when
    /// the check could not run or its answer was unclear, so a network
    /// failure is never read as a logout.
    pub signed_out: bool,
    pub email: Option<String>,
    pub subscription_type: Option<String>,
    pub config_directory: Option<String>,
    /// MODELS-1: the models the account can use right now, when the login
    /// check itself lists them (antigravity's `agy models`). `Some` only
    /// when logged in and at least one model was read; `None` for the other
    /// harnesses, whose check says nothing about models.
    pub models: Option<Vec<ModelEntry>>,
}

/// One model an account can use: the id a launch passes and the name the
/// CLI shows for it (MODELS-1). Stored on the profile row as JSON.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ModelEntry {
    pub model: String,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ResultEvent {
    pub session_id: Option<String>,
    pub is_error: bool,
    pub subtype: Option<String>,
    /// The `result` field: the harness's final text, or its error message.
    pub text: Option<String>,
    /// `api_error_status` stringified, because the real field is a number or null.
    pub api_error_status: Option<String>,
    /// Claude Code's structured error names (MODELS-1): the result line's
    /// `api_error` (`usage_limit_reached`, `model_requires_usage_credits`,
    /// ...) and `api_error_code` (`credits_required`, ...). `None` when the
    /// line has none, and always `None` for the other harnesses.
    pub api_error: Option<String>,
    pub api_error_code: Option<String>,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    /// Tokens read from the prompt cache (DT-1). Claude's `input_tokens`
    /// excludes them; codex's includes them.
    pub cache_read_tokens: Option<i64>,
    /// Tokens written to the prompt cache (DT-1); `None` where the harness
    /// reports no such kind (antigravity).
    pub cache_write_tokens: Option<i64>,
    /// Thinking / reasoning tokens (DT-1), a subset of `output_tokens`.
    pub reasoning_tokens: Option<i64>,
    /// Every token of the usage once: `input + output` for codex and
    /// antigravity (their input already holds the cached part), plus the two
    /// cache kinds for Claude. `None` when both `input_tokens` and
    /// `output_tokens` are.
    pub total_tokens: Option<i64>,
    /// The usage covers the whole thread / conversation so far (codex,
    /// antigravity): a later result replaces an earlier one. `false` (Claude):
    /// it covers only the turn that produced this result, so results add up.
    pub usage_cumulative: bool,
    /// `modelUsage` as (model id, output tokens), sorted by model id. `None`
    /// when the result event has no `modelUsage` object.
    pub model_usage: Option<Vec<(String, i64)>>,
    /// `subagent_stats.spawned`; `None` when the event does not carry it.
    pub subagents_spawned: Option<i64>,
    /// The harness's own error named a usage limit in a form
    /// `looks_like_quota` does not cover (codex's `usage_limit_reached`,
    /// `UsageLimitExceeded`). Never set by the Claude Code parser.
    pub quota: bool,
    /// Antigravity's `result.structured_output`: the object a `--json-schema`
    /// run answered with (scope §1 E7). `None` for every other harness.
    pub structured_output: Option<serde_json::Value>,
}

/// One quota window from Claude Code's `rate_limit_event`
/// (`rate_limit_info.unifiedWindows.<name>`).
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitWindow {
    /// The key, as the harness wrote it (`five_hour`, `seven_day`, ...).
    pub name: String,
    pub utilization: f64,
    /// `resetsAt`, epoch seconds.
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    Init {
        session_id: Option<String>,
        model: Option<String>,
        cwd: Option<String>,
        /// AGY-1 (AC3): the tool names the harness reported on its init line,
        /// as written. Antigravity reads `init.tools` (see
        /// `antigravity::init_tools`); Claude Code and codex report none.
        /// Recorded on `harness_init`, never used to decide anything.
        tools: Option<Vec<String>>,
    },
    Assistant {
        session_id: Option<String>,
    },
    RateLimitWarning {
        rate_limit_type: Option<String>,
        utilization: Option<f64>,
        resets_at: Option<i64>,
        /// Every entry of `rate_limit_info.unifiedWindows` that has a
        /// `utilization`, keyed by name as the harness reported it; empty
        /// when the event has no `unifiedWindows`.
        windows: Vec<RateLimitWindow>,
    },
    ApiRetry {
        attempt: Option<i64>,
        max_retries: Option<i64>,
        error: Option<String>,
        error_status: Option<i64>,
    },
    Result(ResultEvent),
    /// A harness error outside the final result (codex's top-level `error`
    /// event). It stands in for the result only when no result line follows.
    HarnessError(ResultEvent),
    /// Codex: one finished shell command, with its real exit code.
    CommandCompleted {
        item_id: Option<String>,
        command: String,
        output: String,
        exit_code: Option<i64>,
        status: Option<String>,
    },
    /// Antigravity: a tool step. The runner task will turn completed commands
    /// into evidence and correlate their exit codes with the transcript.
    ToolStep {
        step_index: i64,
        tool_name: String,
        done: bool,
        command: Option<String>,
        output: Option<String>,
    },
    /// Antigravity: per-step usage, used by the forthcoming runner support.
    StepUsage {
        step_index: i64,
        output_tokens: i64,
    },
    /// Codex: any other `item.*` event, kept so the runner can count item
    /// kinds a locked-down worker should never produce.
    Item {
        id: Option<String>,
        item_type: String,
    },
    Other {
        // Only `unknown_and_broken_lines_do_not_panic` reads this. The runner
        // deliberately ignores non-signal lines (`user`, `thinking_tokens`,
        // `hook_started`, ...) after teeing them to stream.jsonl; the field exists
        // so that test can prove which bucket a line fell into.
        #[allow(dead_code)]
        kind: String,
    },
    Unparsable {
        raw_len: usize,
    },
}

pub trait HarnessAdapter {
    fn build_command(&self, req: &LaunchRequest<'_>) -> CommandPlan;
    fn auth_command(&self, config_dir: &str, cwd: &Path) -> CommandPlan;
    /// `thoth profile login`: the interactive command that lets a person log
    /// in to this harness in their own terminal, for `config_dir`.
    fn login_command(&self, config_dir: &str, cwd: &Path) -> CommandPlan;
    fn parse_auth_status(&self, stdout: &str) -> Result<AuthStatus>;
    /// The whole answer of the auth command. Claude Code reports JSON on
    /// stdout; codex reports through its exit code and a line of text.
    fn parse_auth_output(&self, _success: bool, stdout: &str, _stderr: &str) -> Result<AuthStatus> {
        self.parse_auth_status(stdout)
    }
    fn parse_stream_line(&self, line: &str) -> StreamEvent;

    /// `dispatch`, `review start`, `spec review start` and `resume`: refuse a
    /// model this harness cannot run, before anything is written.
    /// `known_models` is the text the refusal lists as what to pass instead
    /// (`model_list::known_models_text`): the account's stored list, or a
    /// sentence saying none is stored.
    fn check_model(&self, _profile_id: &str, _model: &str, _known_models: &str) -> Result<()> {
        Ok(())
    }
    /// What `runs.allowed_tools` holds for an implementer run dispatched
    /// with `--allowed-tools given`. `research` is whether the task is a
    /// research task (`spec::is_research_task`), the only kind whose worker
    /// gets this harness's web tools.
    fn implementer_allowed_tools(&self, given: &str, research: bool) -> String;
    /// What `runs.allowed_tools` holds for a reviewer run, given the
    /// Claude Code reviewer's list.
    fn reviewer_allowed_tools(&self, default: &str, _research: bool) -> String {
        default.to_string()
    }
    /// Whether a worker run's commands arrive through `thoth hook` and
    /// the run's inbox (Claude Code), rather than on the stream.
    fn hooks_through_inbox(&self) -> bool {
        false
    }
    /// Whether the harness names its own session once the run has started,
    /// so a fresh run has no session id to pre-generate or record at launch.
    fn names_session_late(&self) -> bool {
        false
    }
    /// Whether a reviewer run gets its input as files (`.agent/review-input/`)
    /// and answers with a schema-constrained final message.
    fn reads_review_packet(&self) -> bool {
        false
    }
    /// The reviewer's prompt. The default is Claude Code's, whose reviewer
    /// reads the evidence through `thoth evidence show`. `protected_changes`
    /// is one line per protected file the candidate changed (P7-2).
    #[allow(clippy::too_many_arguments)]
    fn build_review_prompt(
        &self,
        task_id: &str,
        spec: &crate::spec::TaskSpec,
        criterion_ids: &[String],
        thoth_exe: &str,
        diff_code: Option<&str>,
        index_rows: &[crate::evidence::IndexRow],
        protected_changes: &[String],
        sources: Option<&[crate::review_json::SourceToCheck]>,
    ) -> String {
        if let Some(sources) = sources {
            return source_check_prompt(task_id, "claude-code", sources);
        }
        claude_code::build_review_prompt(
            task_id,
            spec,
            criterion_ids,
            thoth_exe,
            diff_code,
            index_rows,
            protected_changes,
        )
    }
    /// A spec reviewer's prompt (SR-2; SR-3: in the run's `mode`). The
    /// default is Claude Code's, which writes `.agent/spec-review.json` with
    /// a shell redirect.
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
            crate::spec_review_prompt::How::ShellRedirect,
            mode,
        )
    }
    /// Runs once before the harness is launched (fresh or resumed), after
    /// the run dir exists. Anything it writes goes into the run's worktree.
    fn prepare_launch(&self, _run: &state::Run, _run_dir: &Path, _thoth_exe: &Path) -> Result<()> {
        Ok(())
    }
    /// A reviewer run that settled cleanly: write its verdict to `path`
    /// (`.agent/review.json`) from the result line, for an adapter whose
    /// harness cannot write the file itself. Best-effort; a missing file
    /// becomes `review_invalid` in the unchanged ingest.
    fn write_review_output(&self, _path: &Path, _result: Option<&ResultEvent>, _run_dir: &Path) {}
    /// The per-launch state the runner feeds while the stream is read.
    /// `resume_session` is the session a resumed run continues.
    fn launch_hooks(&self, _run: &state::Run, _resume_session: Option<&str>) -> Box<dyn LaunchHooks> {
        Box::new(NoLaunchHooks)
    }
}

/// Where one run keeps its files and evidence: what the runner's own
/// recording and a launch's hooks both need besides the DB connection.
pub struct RunPlace<'a> {
    pub paths: &'a Paths,
    pub repo_id: &'a str,
    pub run_dir: PathBuf,
    pub worktree: PathBuf,
}

/// What a harness adds to the runner while one launch's stream is read.
/// Every method defaults to doing nothing.
pub trait LaunchHooks {
    /// The stream named the run's session.
    fn on_session(&mut self, _session_id: &str) {}
    /// A stream event the runner itself does not act on.
    fn on_event(&mut self, _event: &StreamEvent) {}
    /// The result line, before the runner reads its usage.
    fn on_result(&mut self, _result: &mut ResultEvent, _requested_model: &str) {}
    /// Extra keys for the `run_usage` detail.
    fn extend_usage(&self, _usage: &mut serde_json::Value) {}
    /// Offered the `run_usage` detail and subagent count: returns them to be
    /// written now, or `None` after keeping them for `after_stream`.
    fn hold_usage(
        &mut self,
        usage: serde_json::Value,
        subagents: Option<i64>,
    ) -> Option<(serde_json::Value, Option<i64>)> {
        Some((usage, subagents))
    }
    /// Records the commands whose outcome is now known; with `last` (the
    /// stream has ended) every remaining one too. Returns the evidence rows
    /// written.
    fn settle_commands(
        &mut self,
        _conn: &Connection,
        _run: &state::Run,
        _place: &RunPlace<'_>,
        _session_id: Option<&str>,
        _last: bool,
    ) -> u64 {
        0
    }
    /// The stream has ended (or the runner is giving up on it): write
    /// whatever was held back.
    fn after_stream(&mut self, _conn: &Connection, _run: &state::Run, _place: &RunPlace<'_>) {}
}

/// The hooks of a harness that adds nothing.
pub struct NoLaunchHooks;

impl LaunchHooks for NoLaunchHooks {}

/// `profiles.harness` values the runner can launch.
pub const HARNESS_CLAUDE_CODE: &str = "claude-code";
pub const HARNESS_CODEX: &str = "codex";

/// One harness the runner can launch.
pub struct Harness {
    /// The `profiles.harness` value.
    pub name: &'static str,
    pub adapter: fn() -> Box<dyn HarnessAdapter>,
    /// The executable a run launches, as recorded in `runs.harness_bin`.
    pub bin: fn() -> String,
}

/// Every supported harness, in the order messages list them.
pub const HARNESSES: &[Harness] = &[
    Harness {
        name: HARNESS_CLAUDE_CODE,
        adapter: || Box::new(claude_code::ClaudeCodeAdapter::from_env()),
        bin: crate::config::claude_bin,
    },
    Harness {
        name: HARNESS_CODEX,
        adapter: || Box::new(codex::CodexAdapter::from_env()),
        bin: crate::config::codex_bin,
    },
    antigravity::HARNESS,
];

fn find_harness(harness: &str) -> Option<&'static Harness> {
    HARNESSES.iter().find(|h| h.name == harness)
}

/// The adapter for a profile's harness. Anything unlisted is Claude Code:
/// `profile add`, `dispatch` and `review start` refuse other values first.
pub fn for_harness(harness: &str) -> Box<dyn HarnessAdapter> {
    match find_harness(harness) {
        Some(h) => (h.adapter)(),
        None => Box::new(claude_code::ClaudeCodeAdapter::from_env()),
    }
}

/// The executable a run on `harness` launches, as recorded in `runs.harness_bin`.
///
/// The program after Windows shim resolution (e.g. the full path of
/// `node.exe`), since that is what actually starts. A resolution error is not
/// raised here: the runner hits the same error before spawning and settles
/// the run with it, so the raw value is recorded.
pub fn harness_bin(harness: &str) -> String {
    let raw = match find_harness(harness) {
        Some(h) => (h.bin)(),
        None => crate::config::claude_bin(),
    };
    match crate::process::resolve_harness_command(&raw, &[]) {
        Ok((program, _)) => program,
        Err(_) => raw,
    }
}

/// Brief section 2.5 specifies the pattern
/// `(?i)rate.?limit|usage limit|429|quota|overloaded`. Phase 1's dependency list
/// has no regex crate, so this is a literal expansion rather than a translation:
/// the `rate.?limit` branch covers the four separators that actually occur
/// (space, `-`, `_`, `.`) plus the unseparated form, where the regex's `.` would
/// match any single character. Anything exotic like `rate/limit` falls through to
/// the `429` / `quota` branches in practice, and if a real sample ever shows a
/// separator not listed here, add it here rather than reaching for the regex
/// crate.
///
/// (FC-1: `overloaded` moved to `TRANSIENT_MARKERS`. A busy service is not a
/// used-up quota, and parking a profile that still has quota made `recommend`
/// skip it.)
///
/// Note the cost of the spec's `429` branch, kept deliberately: it is a bare
/// substring test, so an error message that merely contains "429" (a line number,
/// part of a hash) classifies the run as a quota interruption and parks an
/// otherwise healthy profile at `rate_limited`. `thoth profile check` is the
/// one-command way back, which is why that command exists.
pub fn looks_like_quota(text: &str) -> bool {
    let t = text.to_lowercase();
    t.contains("ratelimit")
        || t.contains("rate limit")
        || t.contains("rate-limit")
        || t.contains("rate_limit")
        || t.contains("rate.limit")
        || t.contains("usage limit")
        || t.contains("429")
        || t.contains("quota")
}

/// Lower-case pieces of a harness error that mean a temporary problem (the
/// network, a busy service), not a used-up quota or a lost login (FC-1). The
/// one list: FC-2's automatic retry adds to it here.
///
/// - `overloaded`: brief section 2.5's pattern; no recording.
/// - `there was a network issue`, `tls handshake timeout`, `eligibility check
///   failed`, `the stream was interrupted`: recorded agy failures
///   (tests/fixtures/antigravity/fc1-*).
/// - `stream disconnected before completion`: the codex string H5 names; no
///   recording.
/// - `failed to refresh oauth token`: a recorded Claude Code failure
///   (tests/fixtures/stream-oauth-refresh-failed.jsonl), which itself says it
///   is usually transient.
pub const TRANSIENT_MARKERS: &[&str] = &[
    "overloaded",
    "there was a network issue",
    "tls handshake timeout",
    "eligibility check failed",
    "the stream was interrupted",
    "stream disconnected before completion",
    "failed to refresh oauth token",
];

/// Whether a harness error names a temporary problem: `text`, lower-cased,
/// contains one of `TRANSIENT_MARKERS`.
pub fn looks_like_transient(text: &str) -> bool {
    let t = text.to_lowercase();
    TRANSIENT_MARKERS.iter().any(|m| t.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn research_reviewers_get_web_tools_without_write_tools() {
        let default = crate::commands::review::REVIEWER_ALLOWED_TOOLS;
        let claude = claude_code::ClaudeCodeAdapter { bin: "claude".into() };
        let codex = codex::CodexAdapter { bin: "codex".into() };
        let agy = antigravity::AntigravityAdapter { bin: "agy".into() };
        assert_eq!(claude.reviewer_allowed_tools(default, false), default);
        assert_eq!(claude.reviewer_allowed_tools(default, true), format!("{default},WebFetch,WebSearch"));
        assert_eq!(codex.reviewer_allowed_tools(default, true), default);
        assert_eq!(codex.reviewer_allowed_tools(default, false), default);
        let agy_default = antigravity::AGY_REVIEWER_TOOLS.join(",");
        assert_eq!(agy.reviewer_allowed_tools(default, false), agy_default);
        assert_eq!(agy.reviewer_allowed_tools(default, true), format!("{agy_default},search_web,read_url_content"));
    }

    #[test]
    fn source_prompts_share_numbering_and_harness_specific_instructions() {
        let sources = crate::review_json::sources_to_check(&serde_json::json!({"claims":[
            {"kind":"research", "text":"claim\nwith newline", "sources":[{"url":" file:///page ", "quote":"quote\nwith newline"}]}]}));
        for (harness, open, local, output) in [
            ("claude-code", "WebFetch", "Read or Bash", "shell redirect"),
            ("codex", "web_search", "cat", "final message"),
            ("antigravity", "read_url_content", "read_file", "structured_output"),
        ] {
            let prompt = source_check_prompt("T1", harness, &sources);
            assert!(prompt.contains(open) && prompt.contains(local) && prompt.contains(output));
            let line = prompt.lines().skip_while(|l| *l != "## Sources to check").nth(1).unwrap();
            assert_eq!(serde_json::from_str::<serde_json::Value>(line).unwrap(), serde_json::to_value(&sources).unwrap());
            assert!(prompt.contains("independent") && prompt.contains("T1"));
            assert!(!prompt.contains("## Evidence") && !prompt.contains("## Rules"));
        }
    }

    /// RES-1a AC2/AC4: what each harness stores in `runs.allowed_tools` for
    /// an implementer, research task or not.
    #[test]
    fn only_a_research_implementer_gets_its_harness_web_tools() {
        let default = crate::cli::DEFAULT_ALLOWED_TOOLS;
        let claude = claude_code::ClaudeCodeAdapter { bin: "claude".into() };
        let codex = codex::CodexAdapter { bin: "codex".into() };
        let agy = antigravity::AntigravityAdapter { bin: "agy".into() };
        let agy_default = antigravity::AGY_IMPLEMENTER_TOOLS.join(",");

        assert_eq!(claude.implementer_allowed_tools(default, false), default);
        assert_eq!(
            claude.implementer_allowed_tools(default, true),
            format!("{default},WebFetch,WebSearch")
        );
        assert_eq!(claude.implementer_allowed_tools("Bash,WebFetch", true), "Bash,WebFetch,WebSearch");
        assert_eq!(claude.implementer_allowed_tools("", true), "", "empty means no lock");

        assert_eq!(codex.implementer_allowed_tools(default, true), default);
        assert_eq!(codex.implementer_allowed_tools(default, false), default);

        assert_eq!(agy.implementer_allowed_tools(default, false), agy_default);
        assert_eq!(
            agy.implementer_allowed_tools(default, true),
            format!("{agy_default},search_web,read_url_content")
        );
        assert_eq!(agy.implementer_allowed_tools("read_file", true), "read_file,search_web,read_url_content");
        assert_eq!(agy.implementer_allowed_tools("", true), "");
    }

    #[test]
    fn with_tools_appends_only_what_is_missing() {
        assert_eq!(with_tools("A,B", &["C", "D"]), "A,B,C,D");
        assert_eq!(with_tools("A, C ,B", &["C", "D"]), "A, C ,B,D");
        assert_eq!(with_tools("A", &[]), "A");
        assert_eq!(with_tools("", &["C"]), "");
    }

    #[test]
    fn looks_like_quota_covers_every_branch_of_the_spec_pattern() {
        // brief section 2.5: (?i)rate.?limit|usage limit|429|quota|overloaded
        for s in [
            "Rate limit exceeded",
            "rate-limit hit",
            "rate_limit",
            "ratelimit",
            "Claude AI usage limit reached",
            "Request rejected (429)",
            "quota exhausted",
        ] {
            assert!(looks_like_quota(s), "should match: {s}");
        }
        for s in [
            "compile error in main.rs",
            "the tool call failed",
            "",
            "limit the rate of change later",
            // FC-1 (H3): a busy service is transient, not quota.
            "Repeated 529 Overloaded errors",
        ] {
            assert!(!looks_like_quota(s), "should not match: {s}");
        }
    }

    #[test]
    fn looks_like_transient_matches_each_listed_marker_and_nothing_else() {
        // One sentence per marker, in TRANSIENT_MARKERS order. The four agy
        // sentences and the Claude one are recordings (AC7's fixtures);
        // `overloaded` and the codex disconnect are the fakes' synthetic text.
        let sentences = [
            "Repeated 529 Overloaded errors",
            "There was a network issue connecting to the server, please try again.",
            "Eligibility check failed: failed to get profile picture: Get \"https://lh3.googleusercontent.com/a/REDACTED=s96-c\": net/http: TLS handshake timeout",
            "Eligibility check failed: failed to get profile picture: Get \"https://lh3.googleusercontent.com/a/REDACTED=s96-c\": net/http: TLS handshake timeout",
            "The stream was interrupted. Please continue the task you were working on.",
            "stream disconnected before completion: error decoding response body (synthetic; the string H5 names, no recording)",
            "Failed to refresh OAuth token: another Claude Code process is refreshing it",
        ];
        assert_eq!(sentences.len(), TRANSIENT_MARKERS.len());
        for (marker, s) in TRANSIENT_MARKERS.iter().zip(sentences) {
            assert!(s.to_lowercase().contains(marker), "{s} should hold {marker}");
            assert!(looks_like_transient(s), "should match: {s}");
        }
        for s in [
            "compile error in main.rs",
            "",
            "Request rejected (429): Claude AI usage limit reached",
        ] {
            assert!(!looks_like_transient(s), "should not match: {s}");
        }
    }
}
