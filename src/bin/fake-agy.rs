//! Test double for `agy models`, `agy -p /usage --output-format json`, and
//! the stream-json agent mode. The `/usage` signed-out response is an
//! assumption based on `agy models`; it has not been verified against agy.
//!
//! Usage switches, in precedence order: `FAKE_AGY_LOGGED_OUT`,
//! `FAKE_AGY_USAGE=hang` (and `FAKE_AGY_HANG_MARKER`),
//! `FAKE_AGY_USAGE=fail`, `FAKE_AGY_USAGE=fail_long` (exit 3, 200 `a` then
//! more on stderr), `FAKE_AGY_USAGE=noisy` (200 KB of ASCII on stderr, then
//! the built-in JSON, exit 0), and `FAKE_AGY_USAGE_FILE`. Other switches:
//! `FAKE_AGY_DATA_DIR`, `FAKE_AGY_ARGS_LOG`,
//! `FAKE_SCENARIO` (`ok`, `background`, `no_exit_code`, `quota`, `resume`,
//! `subagent`, `unknown_tool`, `crash`, `slow`, and the reviewer scenarios below) and
//! `FAKE_TASK_ID`. `--version` prints `0.0.0-fake` (thoth doctor).
//! Quota is synthetic and
//! untested against the real CLI because scope §1 E9 never triggered it.
//! `FAKE_AGY_RESULT_BEFORE_CRASH` makes `crash` print an ERROR result line
//! before it dies, and `FAKE_AGY_HANG_AFTER_RESULT` makes it then sleep 60 s
//! first, so a run that ends after its result line (a failure, a timeout kill)
//! can be tested.
//! `FAKE_AGY_PRINT_TIMEOUT=1` prints agy's print-timeout line on stderr after init, then an
//! empty SUCCESS result and exit 0, with no result.json and no commit.
//! `FAKE_AGY_PRINT_TIMEOUT_BAD_BYTES=1` does the same, with a stderr line of invalid UTF-8 first.
//!
//! AGY-1: the tool names are agy 1.4.4's. Files are read with `read_file`
//! (parameter `Path`, as the hook payload of 2026-10-21 showed) and written
//! with `write_file` (`Path`, `Content`; read from the binary's tool source
//! names, still to be confirmed on a real run). The `init` line's `tools`
//! is the 60-name list agy 1.4.4 really reports, which still says
//! `view_file` and never `read_file`: the real agy's list is stale, so this
//! fake is deliberately inconsistent with the names its model calls, as the
//! real one is. `FAKE_AGY_INIT_TOOLS` (raw JSON: `[]`, `null`, a mixed
//! list, a non-array) replaces that `tools` value, so a test can see what
//! the runner records for an odd list. `--effort LEVEL` is accepted for `low`, `medium`, `high`,
//! `xhigh` and `max`; any other value prints `invalid --effort "X" (valid:
//! low, medium, high, xhigh, max)` and exits 1, and a `--model` whose name
//! ends in a level together with `--effort` prints `--model X conflicts with
//! --effort=Y` and exits 1 (both strings are agy.exe's; how the real agy
//! reports them is unconfirmed). `FAKE_AGY_EXTRA_TOOLS` (comma-separated
//! names, in order) makes every implementer flow (`ok`, `background`,
//! `no_exit_code`, `subagent`, `resume`, `quota`, `crash`, `research_*`) try each name
//! through the hooks before its first ordinary tool call, with an empty
//! argument object; an allowed one is one DONE step with empty output. The
//! `unknown_tool` scenario is `ok` with `frobnicate_file` as that extra
//! attempt, so the ordinary file write, commit and result still happen.
//!
//! `--json-schema FILE` is accepted as agy's reviewer launch passes it; the
//! file must exist, else the usage line and exit 2. Reviewer scenarios
//! (`review_pass`, `review_reject`, `review_bad`, `review_no_output`,
//! `review_not_object`) first
//! try `write_file` of `agy_reviewer_leak.txt` and `run_command`
//! `echo leak > agy_reviewer_leak2.txt` through the hooks, then `read_file`
//! `.agent/review-input/packet.md` (an ERROR step naming it when missing) and
//! the first `evidence/<CODE>.txt`, then answer with a SUCCESS result whose
//! `response` is the review JSON and whose `structured_output` is the review
//! document (absent for `review_no_output`, the string "PASS" for `review_not_object`). Criterion ids come from the
//! prompt's `Every id in "criteria" must be one of:` line and the cited code
//! from the packet's `- E00x | ` index lines. `review_reject` marks the first
//! criterion unmet with a REJECT; `review_bad` does too but says PASS.
//!
//! SR-2 spec reviewer scenarios (`spec_review_approve`, `spec_review_revise`,
//! `spec_review_no_output`) make the same two leak attempts (`write_file` of
//! `agy_reviewer_leak.txt`, `run_command`), then `read_file`
//! `.agent/spec-review-input/spec.json`
//! (missing or not a spec: an ERROR result naming it and exit 1), take the
//! ids from the same prompt line, and answer with a SUCCESS result whose
//! `structured_output` is the spec review document (absent for
//! `spec_review_no_output`). SR-3: `spec_review_edit` does
//! the same with a REVISE, every criterion ok and three edits built from the
//! spec's text (replace the first acceptance criterion keeping its id, append
//! a constraint, delete the last non_goal when there is one);
//! `spec_confirm_revise` `read_file`s `edits.json` and `proposed.json` in the
//! same folder (no leak attempts) and answers the first edit revise with a
//! note, the rest ok, as `structured_output`.
//!
//! Hooks: before each tool call, if `<cwd>/.agents/hooks.json` exists, every
//! top-level named hook's `PreToolUse` groups whose `matcher` selects the tool
//! have each handler's `command` run through `sh -c` (`cmd /c` on Windows,
//! cwd unchanged) with a payload shaped like
//! `tests/fixtures/antigravity/hooks/pre-*.json` on stdin, stopping at the
//! first handler that does not answer allow. A matcher selects the tool when it
//! is `*`, `""`, `.*`, or a `|`-separated list of exact tool names, optionally
//! wrapped once in `^...$` as a whole. Any other matcher (a real regex such as
//! `run_.*`, `^a$|^b$`, groups) counts as not matching: this subset is
//! intended, because Thoth only ever writes `*` and this binary is std-only,
//! with no regex crate. The tool runs only when every selected handler prints
//! JSON with `decision == "allow"`; otherwise (deny, `{}`, empty or unparsable
//! stdout, non-zero exit) it is skipped whole: no `step_update`, no file
//! written, no command, no transcript line. Without hooks.json nothing changes.
//!
//! MODELS-1: `models` prints `Fetching available models...` and then one
//! `id<TAB>display name` line per model, like agy 1.2.17. `FAKE_AGY_MODELS`
//! (comma-separated ids), when set, replaces the built-in list for both
//! `models` and the `-p` launch's model check; set to the empty string,
//! `models` prints only the first line and every `-p` model is refused.
//! `FAKE_AGY_HIDE_MODELS=1` makes `models` print only the first line while a
//! `-p` launch still accepts the known models: a logged-in check that reads
//! no list, for a run that then needs resuming without one.
//!
//! `thoth profile login` switches, unset by default: a bare `agy` (no
//! arguments) is the interactive login; it creates agy's fixed directory
//! (`USERPROFILE`, else `HOME`, plus `.gemini/antigravity-cli`) and writes
//! `fake-login.ok` in it. `FAKE_LOGIN_FAIL=1` makes it leave without writing;
//! `FAKE_LOGIN_SLEEP_MS` sleeps that long first; `FAKE_LOGIN_BY_FILE=1` makes
//! `models` answer signed out unless that file exists
//! (`FAKE_AGY_LOGGED_OUT=1` still wins).
//!
//! FC-1 switches, unset by default: `FAKE_LOGOUT_MARKER=<path>` makes `models`
//! print `Please sign in to use Antigravity` (exit 0) once that file exists;
//! `FAKE_AUTH_FAIL_MARKER=<path>` makes it print `fake-agy: models
//! unavailable` on stderr and exit 3 once that file exists. FC-1 scenarios:
//! `network`, `eligibility` (no init line) and `stream_interrupted` replay the
//! error text and exit code of the recorded failures in
//! `tests/fixtures/antigravity/fc1-*`; `auth_expired` and `auth_check_breaks`
//! write those files and fail with synthetic text.
//!
//! W3: `FAKE_PROMPT_COPY=<path>` writes the `-p` value of a worker or
//! reviewer (stream-json) launch, byte for byte, to that file (overwriting);
//! the `-p /usage` call does not write it. Unset by default.
//!
//! FC-2 switches, unset by default: `FAKE_ATTEMPTS_FILE=<path>` makes every
//! stream-json launch (not `models`, `-p /usage`, `--version` or the
//! interactive login) append `resume <id>` (with `--conversation <id>`) or
//! `fresh <id>` (the conversation it will name) to that file before it picks
//! its scenario - so before `eligibility` exits - and count the file's lines:
//! that is launch k (from 1). `FAKE_FIRST_SCENARIO=<name>` is the scenario of
//! launches 1 to `FAKE_FIRST_TIMES` (default 1, unparsable is 1); later ones
//! use `FAKE_SCENARIO`. Without `FAKE_ATTEMPTS_FILE`, k is always 1, so every
//! launch runs `FAKE_FIRST_SCENARIO`.
//!
//! RES-1a scenarios, the same in fake-claude and fake-codex: `research_ok`
//! runs like `ok` (same summary) but its claims are one completion and two
//! research claims, `research claim 1` and `research claim 2`;
//! `FAKE_SOURCES` (a JSON array of `{"url", "quote"}`) gives the first its
//! first item and the second the rest, and unset each gets
//! `https://example.invalid/fake` with quote `fake quote`.
//! `research_unsourced` delivers the completion claim only;
//! `research_no_sources` adds one research claim whose `sources` is `[]`. No
//! URL is ever opened.

// Source-check scenarios: `source_check` checks file:// pages from the prompt;
// `source_check_bad` returns non-object structured_output. Require --json-schema.
// FAKE_SOURCE_CHECK_VERDICT overrides the computed verdict;
// FAKE_REVIEW_GATE holds output until its file exists.
#[path = "support/source_check.rs"]
mod source_check;
// RFIX-1: the same scenarios as fake-claude's, answered as
// `structured_output` under a strict `--json-schema`; `fix_confirm_bad`
// answers an object without `edits`.
#[path = "support/review_fix.rs"]
mod review_fix;

use std::io::Write;
use std::path::Path;

/// The file the interactive login writes; Thoth itself never knows it.
const LOGIN_MARKER: &str = "fake-login.ok";

/// FC-2: appends `resume <id>` or `fresh <id>` to `FAKE_ATTEMPTS_FILE` and
/// returns how many lines it now holds (this launch's number, from 1).
/// Unset: writes nothing, and every launch is the first.
fn record_attempt(resumed: bool, id: &str) -> usize {
    let Ok(path) = std::env::var("FAKE_ATTEMPTS_FILE") else {
        return 1;
    };
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(f, "{} {id}", if resumed { "resume" } else { "fresh" });
    }
    std::fs::read_to_string(&path)
        .map(|t| t.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(1)
        .max(1)
}

/// FC-2: `FAKE_FIRST_SCENARIO` for launches 1 to `FAKE_FIRST_TIMES` (default
/// 1, unparsable is 1); `None` otherwise or when it is unset.
fn first_scenario(launch: usize) -> Option<String> {
    let scenario = std::env::var("FAKE_FIRST_SCENARIO").ok()?;
    let times = std::env::var("FAKE_FIRST_TIMES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(1);
    (launch <= times).then_some(scenario)
}

/// agy's fixed data directory, by the same rule as thoth's.
fn login_dir() -> Option<std::path::PathBuf> {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok()?;
    Some(std::path::PathBuf::from(home).join(".gemini").join("antigravity-cli"))
}

const MODELS: &[&str] = &[
    "gemini-3.8-flash-high", "gemini-3.8-flash-medium", "gemini-3.8-flash-low",
    "gemini-3.7-flash-high", "gemini-3.7-flash-medium", "gemini-3.7-flash-low",
    "gemini-3.6-flash-high", "gemini-3.6-flash-medium", "gemini-3.6-flash-low",
    "gemini-3.1-pro-high", "gemini-3.1-pro-low", "claude-sonnet-4-6",
    "claude-opus-4-6-thinking", "gpt-oss-120b-medium",
];

/// AGY-1: the levels agy's `--effort` takes (`agy --help`).
const EFFORT_LEVELS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// AGY-1: the `init.tools` list agy 1.4.4 really reports: the 57 names of
/// the 1.2.x recording plus `run_workflow`, `list_plugin_accounts` and
/// `search_marketplace`. It still names `view_file` and not `read_file`,
/// which is what the model actually calls; the inconsistency is the real
/// agy's, kept on purpose (see the module doc).
const INIT_TOOLS: &[&str] = &[
    "ask_custom_permission", "ask_permission", "ask_question", "browser_click_element",
    "browser_drag_pixel_to_pixel", "browser_get_dom", "browser_get_network_request",
    "browser_input", "browser_list_network_requests", "browser_mouse_down", "browser_mouse_up",
    "browser_move_mouse", "browser_press_key", "browser_refresh_page", "browser_resize_window",
    "browser_scroll", "browser_scroll_dom", "browser_select_option", "browser_subagent",
    "call_mcp_tool", "capture_browser_console_logs", "capture_browser_screenshot",
    "click_browser_pixel", "command_status", "define_subagent", "delete_knowledge",
    "execute_browser_javascript", "find_by_name", "finish", "generate_image", "grep_search",
    "invoke_subagent", "list_browser_pages", "list_dir", "list_permissions", "list_resources",
    "manage_inbox", "manage_subagents", "manage_task", "multi_replace_file_content",
    "notebook_edit", "notebook_execution", "open_browser_url", "read_browser_page",
    "read_resource", "read_url_content", "replace_file_content", "run_command", "schedule",
    "search_web", "sed_file", "send_command_input", "send_message", "view_file", "wait",
    "wait_5_seconds", "write_to_file",
    "run_workflow", "list_plugin_accounts", "search_marketplace",
];

/// AGY-1: the names `FAKE_AGY_EXTRA_TOOLS` lists, or `frobnicate_file` for
/// the `unknown_tool` scenario; empty otherwise.
fn extra_tools(scenario: &str) -> Vec<String> {
    match std::env::var("FAKE_AGY_EXTRA_TOOLS") {
        Ok(list) => list.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect(),
        Err(_) if scenario == "unknown_tool" => vec!["frobnicate_file".to_string()],
        Err(_) => Vec::new(),
    }
}

/// MODELS-1: the models this fake knows: `FAKE_AGY_MODELS` (comma-separated
/// ids; set to the empty string, none at all) when set, else `MODELS`.
/// `models` prints them and a `-p` launch accepts only them.
fn known_models() -> Vec<String> {
    match std::env::var("FAKE_AGY_MODELS") {
        Ok(list) => list.split(',').map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect(),
        Err(_) => MODELS.iter().map(|m| m.to_string()).collect(),
    }
}

/// The built-in `/usage` answer, shaped like the real agy 1.2.2 sample.
fn usage_json() -> serde_json::Value {
    serde_json::json!({"status":"SUCCESS","num_turns":0,"command":{"name":"usage","data":{"groups":[
        {"name":"Gemini Models","description":"Models within this group: Gemini Flash, Gemini Pro","buckets":[{"id":"gemini-weekly","window":"weekly","remaining_fraction":0.75,"reset_time":"2099-01-01T00:00:00Z"}]},
        {"name":"Claude and GPT models","description":"Models within this group: Claude Opus, Claude Sonnet, GPT-OSS","buckets":[{"id":"3p-weekly","window":"weekly","remaining_fraction":1,"reset_time":"2099-01-01T00:00:00Z"}]}
    ]}}})
}

/// FC-1: whether the env var names a file that exists. Unset: false.
fn marker_exists(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|p| Path::new(&p).is_file())
}

/// FC-1: writes the file the env var names, if it is set.
fn write_marker(var: &str) {
    if let Some(p) = std::env::var_os(var) {
        let _ = std::fs::write(p, "1\n");
    }
}

/// FC-1: the `error` fields of the recorded agy 1.2.12 failures in
/// tests/fixtures/antigravity/fc1-*.jsonl, verbatim.
const RECORDED_NETWORK_ISSUE: &str =
    "There was a network issue connecting to the server, please try again.";
const RECORDED_ELIGIBILITY: &str = "Eligibility check failed: failed to get profile picture: Get \"https://lh3.googleusercontent.com/a/REDACTED=s96-c\": net/http: TLS handshake timeout";
const RECORDED_STREAM_INTERRUPTED: &str =
    "The stream was interrupted. Please continue the task you were working on.";

fn emit(value: serde_json::Value) {
    println!("{value}");
    let _ = std::io::stdout().flush();
}

fn value(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn log_args(args: &[String]) {
    let Ok(path) = std::env::var("FAKE_AGY_ARGS_LOG") else { return };
    let env = |name: &str| std::env::var(name).ok();
    let row = serde_json::json!({"args": args, "cwd": std::env::current_dir().ok()
        .map(|p| p.display().to_string()), "env": {"HOME": env("HOME"),
        "CLAUDE_CONFIG_DIR": env("CLAUDE_CONFIG_DIR"), "CODEX_HOME": env("CODEX_HOME"),
        "FAKE_AGY_DATA_DIR": env("FAKE_AGY_DATA_DIR")}});
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{row}");
    }
}

fn transcript(data: &Path, id: &str, row: serde_json::Value) {
    let path = data.join("brain").join(id).join(".system_generated").join("logs")
        .join("transcript_full.jsonl");
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{row}");
    }
}

fn generic(data: &Path, id: &str, step: i64, code: i64, output: &str) {
    transcript(data, id, serde_json::json!({"step_index":step,"source":"MODEL",
        "type":"GENERIC","status":"DONE","created_at":"2026-09-29T00:00:00Z",
        "content":format!("Created At: 2026-09-29T00:00:00Z\nCompleted At: 2026-09-29T00:00:00Z\n\nThe command exited with code {code}.\nOutput:\n{output}\n") }));
}

fn step(id: &str, index: i64, step_type: &str, usage: Option<i64>) {
    let mut body = serde_json::json!({"conversation_id":id,"step_index":index,
        "state":"DONE","step_type":step_type});
    if let Some(tokens) = usage {
        body["usage"] = serde_json::json!({"input_tokens":100,"output_tokens":tokens,
            "thinking_tokens":0,"cache_read_tokens":0,"total_tokens":100 + tokens});
    }
    emit(serde_json::json!({"event":"step_update","step_update":body}));
}

fn response_step(id: &str, index: i64, tokens: i64, text_delta: &str) {
    let body = serde_json::json!({
        "conversation_id": id,
        "step_index": index,
        "state": "DONE",
        "step_type": "agent_response",
        "text_delta": text_delta,
        "usage": {
            "input_tokens": 100,
            "output_tokens": tokens,
            "thinking_tokens": 0,
            "cache_read_tokens": 0,
            "total_tokens": 100 + tokens
        }
    });
    emit(serde_json::json!({"event":"step_update","step_update":body}));
}

fn tool(id: &str, index: i64, name: &str, command: Option<&str>, output: Option<&str>) {
    let mut parameters = serde_json::json!({});
    if let Some(command) = command { parameters["CommandLine"] = command.into(); }
    let mut info = serde_json::json!({"name":name,"parameters":parameters});
    if let Some(output) = output { info["output"] = output.into(); }
    emit(serde_json::json!({"event":"step_update","step_update":{"conversation_id":id,
        "step_index":index,"state":if output.is_some(){"DONE"}else{"ACTIVE"},
        "step_type":"tool","tool_name":name,"tool_info":info}}));
}

/// Whether one matcher selects `tool`: `*`, empty, `.*`, or exact names
/// joined by `|`, optionally wrapped once in `^...$`; anything else never
/// matches (see the module doc).
fn matcher_selects(matcher: &str, tool: &str) -> bool {
    if matcher.is_empty() || matcher == "*" || matcher == ".*" {
        return true;
    }
    let names = matcher.strip_prefix('^').and_then(|m| m.strip_suffix('$')).unwrap_or(matcher);
    let exact = |n: &str| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let list: Vec<&str> = names.split('|').collect();
    list.iter().all(|n| exact(n)) && list.contains(&tool)
}

#[cfg(windows)]
fn shell(command: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("cmd");
    c.arg("/c").raw_arg(command);
    c
}

#[cfg(not(windows))]
fn shell(command: &str) -> std::process::Command {
    let mut c = std::process::Command::new("sh");
    c.arg("-c").arg(command);
    c
}

/// One handler's answer: true only for a JSON stdout with `decision == "allow"`.
fn handler_allows(command: &str, payload: &serde_json::Value) -> bool {
    let mut cmd = shell(command);
    cmd.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let Ok(mut child) = cmd.spawn() else { return false };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.to_string().as_bytes());
    }
    let Ok(out) = child.wait_with_output() else { return false };
    if !out.status.success() {
        return false;
    }
    serde_json::from_slice::<serde_json::Value>(&out.stdout)
        .map(|v| v["decision"] == "allow").unwrap_or(false)
}

struct Agy<'a> {
    data: &'a Path,
    id: String,
    model: String,
}

impl Agy<'_> {
    /// Runs the workspace PreToolUse hooks for one tool call (see the module doc).
    fn allowed(&self, step: i64, name: &str, args: serde_json::Value) -> bool {
        let Ok(text) = std::fs::read_to_string(Path::new(".agents").join("hooks.json")) else { return true };
        let Ok(doc) = serde_json::from_str::<serde_json::Value>(&text) else { return false };
        let cwd = std::env::current_dir().unwrap().display().to_string();
        let artifacts = self.data.join("brain").join(&self.id);
        let payload = serde_json::json!({
            "artifactDirectoryPath": artifacts.display().to_string(),
            "conversationId": self.id,
            "modelName": self.model,
            "stepIdx": step,
            "toolCall": {"name": name, "args": args},
            "transcriptPath": artifacts.join(".system_generated").join("logs")
                .join("transcript_full.jsonl").display().to_string(),
            "workspacePaths": [cwd],
        });
        for hook in doc.as_object().into_iter().flat_map(|o| o.values()) {
            for group in hook["PreToolUse"].as_array().into_iter().flatten() {
                if !matcher_selects(group["matcher"].as_str().unwrap_or(""), name) {
                    continue;
                }
                for handler in group["hooks"].as_array().into_iter().flatten() {
                    let Some(command) = handler["command"].as_str() else { continue };
                    if !handler_allows(command, &payload) {
                        return false;
                    }
                }
            }
        }
        true
    }

    fn run_command_args(command: &str) -> serde_json::Value {
        serde_json::json!({"CommandLine": command,
            "Cwd": std::env::current_dir().unwrap().display().to_string()})
    }

    /// AGY-1: tries every extra tool name at `step` through the hooks; an
    /// allowed one is a DONE step with empty output, a denied one nothing.
    fn try_extra_tools(&self, step: i64, scenario: &str) {
        for name in extra_tools(scenario) {
            if self.allowed(step, &name, serde_json::json!({})) {
                tool(&self.id, step, &name, None, Some(""));
            }
        }
    }

    /// AGY-1: a `write_file` call (`Path`, `Content`) through the hooks;
    /// writes the file and emits the step only when allowed.
    fn write_file(&self, step: i64, path: &Path, content: &str) -> bool {
        let absolute = std::env::current_dir().unwrap().join(path).display().to_string();
        if !self.allowed(step, "write_file", serde_json::json!({"Path": absolute, "Content": content})) {
            return false;
        }
        std::fs::write(path, content).unwrap();
        tool(&self.id, step, "write_file", None, Some(""));
        true
    }

    /// The two leak attempts every reviewer makes (see the module doc).
    fn leak_attempts(&self, first_step: i64) {
        let cwd = std::env::current_dir().unwrap();
        self.write_file(first_step, &cwd.join("agy_reviewer_leak.txt"), "leak\n");
        let command = "echo leak > agy_reviewer_leak2.txt";
        if self.allowed(first_step + 1, "run_command", Agy::run_command_args(command)) {
            let _ = shell(command).status();
            tool(&self.id, first_step + 1, "run_command", Some(command), Some(""));
        }
    }
}

fn commit(_name: &str, message: &str) -> (String, i64) {
    let out = std::process::Command::new("git").args(["-c", "user.name=fake agy", "-c",
        "user.email=fake@example.com", "commit", "-m", message]).output().unwrap();
    (String::from_utf8_lossy(&out.stdout).replace('\n', "\r\n"), out.status.code().unwrap_or(1).into())
}

fn result(id: &str, status: &str, text: &str, tokens: i64) {
    emit(serde_json::json!({"event":"result","result":{"conversation_id":id,"status":status,
        "response":if status == "SUCCESS" { text } else { "" },
        "error":if status == "ERROR" { Some(text) } else { None },"duration_seconds":1,
        "num_turns":1,"usage":{"input_tokens":100,"output_tokens":tokens,
        "thinking_tokens":0,"cache_read_tokens":0,"total_tokens":100 + tokens}}}));
}

/// RES-1a: the research claims a research scenario adds after its completion
/// claim, as in fake-claude. Opens no URL.
fn research_claims(scenario: &str) -> Vec<serde_json::Value> {
    let claim = |n: usize, sources: serde_json::Value| {
        serde_json::json!({"kind": "research", "text": format!("research claim {n}"),
            "evidence": [], "sources": sources})
    };
    match scenario {
        "research_ok" => {
            let given: Vec<serde_json::Value> = std::env::var("FAKE_SOURCES").ok()
                .and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            let (first, rest) = if given.is_empty() {
                let placeholder = serde_json::json!([{"url": "https://example.invalid/fake", "quote": "fake quote"}]);
                (placeholder.clone(), placeholder)
            } else {
                (serde_json::json!([given[0]]), serde_json::json!(given[1..]))
            };
            vec![claim(1, first), claim(2, rest)]
        }
        "research_no_sources" => vec![claim(1, serde_json::json!([]))],
        _ => vec![],
    }
}

fn write_result(expected_rows: usize, scenario: &str) {
    let index = Path::new(".agent").join("evidence-index.json");
    let mut codes = Vec::new();
    if index.exists() {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            if let Ok(text) = std::fs::read_to_string(&index) {
                if let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                    codes = rows.iter().filter_map(|row| row["code"].as_str().map(String::from)).collect();
                    if codes.len() >= expected_rows { break; }
                }
            }
            if std::time::Instant::now() >= deadline { break; }
            std::thread::sleep(std::time::Duration::from_millis(200));
        }
    }
    let _ = std::fs::create_dir_all(".agent");
    let task_id = std::env::var("FAKE_TASK_ID").unwrap_or_else(|_| "fake-task".into());
    let claims = if scenario.starts_with("research_") {
        let mut claims = vec![serde_json::json!({"kind":"completion",
            "text":"fake agy scenario completed","evidence":[]})];
        claims.extend(research_claims(scenario));
        claims
    } else {
        vec![serde_json::json!({"kind":"test_result","text":"fake agy scenario completed",
            "evidence":codes.first().into_iter().collect::<Vec<_>>()})]
    };
    let body = serde_json::json!({"task_id":task_id,"status":"completed",
        "summary":"fake agy completed","claims":claims,
        "verification_requests":[{"cmd":"echo verify","cwd":"."}]});
    let _ = std::fs::write(".agent/result.json", body.to_string());
}

/// A `read_file` step (agy 1.4.4; parameter `Path`): DONE with the file's
/// text, or ERROR naming the missing file. Returns the text when it was read.
fn read_file(agy: &Agy<'_>, index: i64, path: &Path) -> Option<String> {
    let absolute = std::env::current_dir().unwrap().join(path).display().to_string();
    if !agy.allowed(index, "read_file", serde_json::json!({"Path": absolute})) {
        return None;
    }
    let text = std::fs::read_to_string(path).ok();
    let mut info = serde_json::json!({"name": "read_file", "parameters": {"Path": absolute}});
    match &text {
        Some(text) => info["output"] = text.as_str().into(),
        None => {
            info["result"] = "ERROR".into();
            info["error"] = format!("file not found: {absolute}").into();
        }
    }
    emit(serde_json::json!({"event":"step_update","step_update":{"conversation_id":agy.id,
        "step_index":index,"state":"DONE","step_type":"tool","tool_name":"read_file","tool_info":info}}));
    text
}

/// RFIX-1: the edit reviewer and the confirmer.
fn fix_review(agy: &Agy<'_>, scenario: &str, args: &[String]) {
    let id = agy.id.as_str();
    let schema = value(args, "--json-schema")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let Some(schema) = schema else {
        result(id, "ERROR", "fake-agy: a reviewer run needs --json-schema", 1);
        std::process::exit(1);
    };
    if let Some(bad) = review_fix::strict_violation(&schema, "schema") {
        result(id, "ERROR", &format!("fake-agy: invalid schema: {bad}"), 1);
        std::process::exit(1);
    }
    step(id, 0, "user_input", None);
    step(id, 1, "agent_response", Some(40));
    let plan = review_fix::plan();
    let doc = if scenario == "review_fix" {
        review_fix::side_effects(&plan);
        review_fix::document(&plan)
    } else {
        let input = Path::new(".agent").join("review-fix-input");
        let edits: Option<Vec<serde_json::Value>> = read_file(agy, 2, &input.join("edits.json"))
            .and_then(|t| serde_json::from_str(&t).ok())
            .filter(|_| review_fix::read_edits().is_some());
        let Some(edits) = edits else {
            result(id, "ERROR", "fake-agy: .agent/review-fix-input is missing or malformed", 1);
            std::process::exit(1);
        };
        review_fix::side_effects(&plan);
        if scenario == "fix_confirm_bad" {
            serde_json::json!({"verdict": "APPROVE"})
        } else {
            review_fix::confirm_override()
                .unwrap_or_else(|| review_fix::confirm_document(&edits, scenario == "fix_confirm_revise"))
        }
    };
    source_check::wait_gate();
    let text = format!("{doc}\n");
    response_step(id, 6, 120, &text);
    let body = serde_json::json!({"conversation_id":id,"status":"SUCCESS","response":text,
        "structured_output":doc,"duration_seconds":1,"num_turns":1,"usage":{"input_tokens":100,
        "output_tokens":160,"thinking_tokens":0,"cache_read_tokens":0,"total_tokens":260}});
    emit(serde_json::json!({"event":"result","result":body}));
}

/// The reviewer scenarios (see the module doc).
fn review(agy: &Agy<'_>, scenario: &str, prompt: &str) {
    let id = agy.id.as_str();
    if scenario.starts_with("source_check") {
        let args: Vec<String> = std::env::args().collect();
        assert!(args.iter().any(|a| a == "--json-schema"), "source check needs --json-schema");
        let doc = if scenario == "source_check_bad" { serde_json::json!("{not json") }
            else { source_check::document(prompt) };
        source_check::wait_gate();
        emit(serde_json::json!({"event":"result", "result": {
            "conversation_id":id, "status":"SUCCESS", "response":doc.to_string(),
            "structured_output":doc, "duration_seconds":1, "num_turns":1,
            "usage":{"input_tokens":100,"output_tokens":160,"total_tokens":260}
        }}));
        return;
    }
    step(id, 0, "user_input", None);
    step(id, 1, "agent_response", Some(40));
    agy.leak_attempts(2);
    let input = Path::new(".agent").join("review-input");
    let packet = read_file(agy, 4, &input.join("packet.md")).unwrap_or_default();
    let codes: Vec<String> = packet
        .lines()
        .filter_map(|l| l.strip_prefix("- ")?.split(" | ").next())
        .filter(|c| c.len() > 1 && c.starts_with('E') && c[1..].chars().all(|ch| ch.is_ascii_digit()))
        .map(String::from)
        .collect();
    if let Some(first) = codes.first() {
        read_file(agy, 5, &input.join("evidence").join(format!("{first}.txt")));
    }
    let mut ids: Vec<String> = prompt
        .lines()
        .find_map(|l| l.strip_prefix("Every id in \"criteria\" must be one of: "))
        .map(|rest| rest.trim_end_matches('.').split(", ").map(String::from).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        ids.push("AC1".into());
    }
    let cite: Vec<String> = codes.first().cloned().into_iter().collect();
    let rejecting = scenario == "review_reject";
    let unmet_first = rejecting || scenario == "review_bad";
    // `review_reject` labels its unmet criterion with the prompt's first
    // category, and puts "none" in both labels on a met one, as the strict schema
    // makes a real agy do.
    let category = prompt
        .lines()
        .find_map(|l| l.strip_prefix("Every \"category\" must be one of: "))
        .and_then(|rest| rest.split([',', '.']).next())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty());
    let criteria: Vec<serde_json::Value> = ids.iter().enumerate().map(|(i, c)| {
        let unmet = unmet_first && i == 0;
        let mut v = serde_json::json!({"id": c, "verdict": if unmet { "unmet" } else { "met" }, "evidence": cite,
            "note": if unmet { "no captured evidence shows this criterion being exercised" } else { "" }});
        if rejecting {
            v["fix_size"] = serde_json::json!(if unmet { "S" } else { "none" });
            match (&category, unmet) {
                (Some(name), true) => v["category"] = serde_json::json!(name),
                (_, false) => v["category"] = serde_json::json!("none"),
                (None, true) => {}
            }
        }
        v
    }).collect();
    let doc = serde_json::json!({"verdict": if rejecting { "REJECT" } else { "PASS" },
        "findings": {"criteria": criteria, "extra_findings": []}});
    let text = format!("{doc}\n");
    response_step(id, 6, 120, &text);
    let mut body = serde_json::json!({"conversation_id":id,"status":"SUCCESS","response":text,
        "duration_seconds":1,"num_turns":1,"usage":{"input_tokens":100,"output_tokens":160,
        "thinking_tokens":0,"cache_read_tokens":0,"total_tokens":260}});
    if scenario == "review_not_object" {
        body["structured_output"] = serde_json::json!("PASS");
    } else if scenario != "review_no_output" {
        body["structured_output"] = doc;
    }
    emit(serde_json::json!({"event":"result","result":body}));
}

/// SR-3: the string entries of a spec's list field, `[]` when absent (copied
/// from fake-claude).
fn spec_list(spec: &serde_json::Value, key: &str) -> Vec<String> {
    spec[key].as_array().map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// SR-3: the first 10 characters of the trimmed entry, or the whole entry.
fn anchor_of(entry: &str) -> String {
    // Counted after trimming, like Thoth does: when the tenth character is
    // a space the anchor goes on to the next non-space one.
    let mut out = String::new();
    for c in entry.trim().chars() {
        out.push(c);
        if out.trim_end().chars().count() >= 10 {
            break;
        }
    }
    out
}

/// SR-3: the three edits of `spec_review_edit`, built from the spec's text
/// (copied from fake-claude).
fn spec_edits(spec: &serde_json::Value, criterion: &str) -> Vec<serde_json::Value> {
    let acs = spec_list(spec, "acceptance_criteria");
    let first = acs.first().cloned().unwrap_or_default();
    let (head, rest) = first.split_once(':').map(|(h, r)| (h.to_string(), r.trim().to_string()))
        .unwrap_or((first.clone(), String::new()));
    let mut edits = vec![
        serde_json::json!({"field": "acceptance_criteria", "op": "replace", "at": 0, "anchor": anchor_of(&first),
            "text": format!("{head}: {rest} (fake edit: names the function to call)"),
            "criterion": criterion, "reason": "fake: the criterion did not name what to call"}),
        serde_json::json!({"field": "constraints", "op": "insert", "at": spec_list(spec, "constraints").len(),
            "anchor": "", "text": "fake edit: do not add a crate",
            "criterion": criterion, "reason": "fake: the spec never said so"}),
    ];
    let non_goals = spec_list(spec, "non_goals");
    if let Some(last) = non_goals.last() {
        edits.push(serde_json::json!({"field": "non_goals", "op": "delete", "at": non_goals.len() - 1,
            "anchor": anchor_of(last), "text": "", "criterion": criterion, "reason": "fake: this non-goal repeats a constraint"}));
    }
    edits
}

/// SR-3: the author's check (`spec_confirm_revise`): the first edit revise
/// with a note, the rest ok, as `structured_output`.
fn spec_confirm(agy: &Agy<'_>) {
    let id = agy.id.as_str();
    step(id, 0, "user_input", None);
    step(id, 1, "agent_response", Some(40));
    let input = Path::new(".agent").join("spec-review-input");
    let edits: Option<Vec<serde_json::Value>> = read_file(agy,2, &input.join("edits.json"))
        .and_then(|t| serde_json::from_str(&t).ok());
    let proposed = read_file(agy,3, &input.join("proposed.json")).filter(|t| {
        serde_json::from_str::<serde_json::Value>(t).map(|v| v["acceptance_criteria"].is_array()).unwrap_or(false)
    });
    let (Some(edits), Some(_)) = (edits, proposed) else {
        result(id, "ERROR", "fake-agy: .agent/spec-review-input/edits.json or proposed.json is missing or malformed", 1);
        std::process::exit(1);
    };
    let answers: Vec<serde_json::Value> = edits.iter().enumerate().map(|(i, e)| {
        let index = e["index"].as_u64().unwrap_or(i as u64);
        if i == 0 {
            serde_json::json!({"index": index, "verdict": "revise", "note": "fake author: keep the original wording of this entry"})
        } else {
            serde_json::json!({"index": index, "verdict": "ok", "note": ""})
        }
    }).collect();
    let doc = serde_json::json!({"verdict": "REVISE", "edits": answers});
    let text = format!("{doc}\n");
    response_step(id, 4, 120, &text);
    let mut body = serde_json::json!({"conversation_id":id,"status":"SUCCESS","response":text,
        "duration_seconds":1,"num_turns":1,"usage":{"input_tokens":100,"output_tokens":160,
        "thinking_tokens":0,"cache_read_tokens":0,"total_tokens":260}});
    body["structured_output"] = doc;
    emit(serde_json::json!({"event":"result","result":body}));
}

/// The SR-2 spec reviewer scenarios (see the module doc); SR-3's
/// `spec_review_edit` too.
fn spec_review(agy: &Agy<'_>, scenario: &str, prompt: &str) {
    let id = agy.id.as_str();
    step(id, 0, "user_input", None);
    step(id, 1, "agent_response", Some(40));
    // AGY-1: the same leak attempts as a reviewer, then the spec.
    agy.leak_attempts(2);
    let input = Path::new(".agent").join("spec-review-input").join("spec.json");
    let spec = read_file(agy, 4, &input).filter(|t| {
        serde_json::from_str::<serde_json::Value>(t)
            .map(|v| v["acceptance_criteria"].is_array())
            .unwrap_or(false)
    });
    let Some(spec) = spec else {
        result(id, "ERROR", "fake-agy: .agent/spec-review-input/spec.json is missing or not a spec", 1);
        std::process::exit(1);
    };
    let mut ids: Vec<String> = prompt
        .lines()
        .find_map(|l| l.strip_prefix("Every id in \"criteria\" must be one of: "))
        .map(|rest| rest.trim_end_matches('.').split(", ").map(String::from).collect())
        .unwrap_or_default();
    if ids.is_empty() {
        ids.push("AC1".into());
    }
    let revise = scenario == "spec_review_revise";
    let edit = scenario == "spec_review_edit";
    let criteria: Vec<serde_json::Value> = ids.iter().enumerate().map(|(i, c)| match (i, revise) {
        (0, true) => serde_json::json!({"id": c, "verdict": "revise", "findings": [
            {"check": "exists", "location": "src/lib.rs:1", "note": "fake: the function the spec names does not exist"}]}),
        (0, false) if !edit => serde_json::json!({"id": c, "verdict": "ok", "findings": [
            {"check": "steps", "location": "spec", "note": "fake: the test steps could name the command to run"}]}),
        _ => serde_json::json!({"id": c, "verdict": "ok", "findings": []}),
    }).collect();
    let doc = if edit {
        let parsed: serde_json::Value = serde_json::from_str(&spec).unwrap_or_default();
        serde_json::json!({"verdict": "REVISE", "criteria": criteria, "edits": spec_edits(&parsed, &ids[0])})
    } else {
        serde_json::json!({"verdict": if revise { "REVISE" } else { "APPROVE" }, "criteria": criteria})
    };
    let text = format!("{doc}\n");
    response_step(id, 5, 120, &text);
    let mut body = serde_json::json!({"conversation_id":id,"status":"SUCCESS","response":text,
        "duration_seconds":1,"num_turns":1,"usage":{"input_tokens":100,"output_tokens":160,
        "thinking_tokens":0,"cache_read_tokens":0,"total_tokens":260}});
    if scenario != "spec_review_no_output" {
        body["structured_output"] = doc;
    }
    emit(serde_json::json!({"event":"result","result":body}));
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    log_args(&args);
    if args == ["-p", "/usage", "--output-format", "json"] {
        if std::env::var("FAKE_AGY_LOGGED_OUT").ok().as_deref() == Some("1") {
            println!("Please sign in to use Antigravity");
        } else if std::env::var("FAKE_AGY_USAGE").ok().as_deref() == Some("hang") {
            std::thread::sleep(std::time::Duration::from_secs(6));
            if let Some(path) = std::env::var_os("FAKE_AGY_HANG_MARKER") { let _ = std::fs::write(path, "finished\n"); }
        } else if std::env::var("FAKE_AGY_USAGE").ok().as_deref() == Some("fail") {
            eprintln!("boom"); std::process::exit(3);
        } else if std::env::var("FAKE_AGY_USAGE").ok().as_deref() == Some("fail_long") {
            // 200 `a`, then text that only a longer cut would keep.
            eprintln!("{}BEYOND_200_CHARS", "a".repeat(200)); std::process::exit(3);
        } else if std::env::var("FAKE_AGY_USAGE").ok().as_deref() == Some("noisy") {
            // 200 KB on stderr first: far more than a pipe buffer holds, so a
            // caller that does not read stderr while waiting blocks this write.
            let line = format!("{}\n", "x".repeat(1023));
            let mut stderr = std::io::stderr().lock();
            for _ in 0..200 { let _ = stderr.write_all(line.as_bytes()); }
            let _ = stderr.flush();
            println!("{}", usage_json());
        } else if let Some(path) = std::env::var_os("FAKE_AGY_USAGE_FILE") {
            match std::fs::read_to_string(path) { Ok(text) => print!("{text}"), Err(error) => { eprintln!("{error}"); std::process::exit(2) } }
        } else {
            println!("{}", usage_json());
        }
        return;
    }
    // Bare `agy`: the interactive login (`thoth profile login`). Like the
    // real agy it manages its own directory. Prints nothing on stdout.
    if args.is_empty() {
        let Some(dir) = login_dir() else {
            eprintln!("fake-agy: neither USERPROFILE nor HOME is set");
            std::process::exit(64);
        };
        if let Some(ms) = std::env::var("FAKE_LOGIN_SLEEP_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        if std::env::var("FAKE_LOGIN_FAIL").as_deref() == Ok("1") {
            eprintln!("fake-agy: closed without logging in");
            std::process::exit(0);
        }
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join(LOGIN_MARKER), "1\n");
        eprintln!("fake-agy: logged in");
        std::process::exit(0);
    }
    if args == ["models"] {
        // FC-1: files the `auth_expired` / `auth_check_breaks` runs write.
        if marker_exists("FAKE_LOGOUT_MARKER") {
            println!("Please sign in to use Antigravity");
            return;
        }
        if marker_exists("FAKE_AUTH_FAIL_MARKER") {
            eprintln!("fake-agy: models unavailable");
            std::process::exit(3);
        }
        let by_file_out = std::env::var("FAKE_LOGIN_BY_FILE").as_deref() == Ok("1")
            && !login_dir().is_some_and(|d| d.join(LOGIN_MARKER).is_file());
        if std::env::var("FAKE_AGY_LOGGED_OUT").ok().as_deref() == Some("1") || by_file_out {
            println!("Please sign in to use Antigravity");
        } else {
            // The real agy 1.2.17 prints this line first, then one
            // `id<TAB>display name` line per model.
            println!("Fetching available models...");
            if std::env::var("FAKE_AGY_HIDE_MODELS").as_deref() != Ok("1") {
                for model in known_models() {
                    println!("{model}\tFake display name for {model}");
                }
            }
        }
        return;
    }
    // `agy --version` (thoth doctor)
    if args == ["--version"] {
        println!("0.0.0-fake");
        return;
    }
    let valid = value(&args, "-p").is_some()
        && value(&args, "--output-format").as_deref() == Some("stream-json")
        && value(&args, "--model").is_some()
        && args.iter().any(|a| a == "--dangerously-skip-permissions")
        && (!args.iter().any(|a| a == "--effort") || value(&args, "--effort").is_some());
    let schema_ok = match args.iter().position(|a| a == "--json-schema") {
        None => true,
        Some(at) => args.get(at + 1).is_some_and(|p| Path::new(p).is_file()),
    };
    if !valid || !schema_ok {
        eprintln!("usage: agy -p PROMPT --output-format stream-json --model MODEL [--effort LEVEL] --dangerously-skip-permissions [--json-schema FILE]");
        std::process::exit(2);
    }
    if let Ok(path) = std::env::var("FAKE_PROMPT_COPY") {
        let _ = std::fs::write(path, value(&args, "-p").unwrap_or_default().as_bytes());
    }
    let model = value(&args, "--model").unwrap();
    let model_name = model.clone();
    // AGY-1: agy's own `--effort` checks, with agy.exe's strings (how the
    // real agy reports them is unconfirmed): an unknown level, and a level
    // beside a model name that already carries one.
    if let Some(effort) = value(&args, "--effort") {
        if !EFFORT_LEVELS.contains(&effort.as_str()) {
            let error = format!("invalid --effort \"{effort}\" (valid: {})", EFFORT_LEVELS.join(", "));
            result("", "ERROR", &error, 0);
            eprintln!("{error}");
            std::process::exit(1);
        }
        if EFFORT_LEVELS.iter().any(|level| model.ends_with(&format!("-{level}"))) {
            let error = format!("--model {model} conflicts with --effort={effort}");
            result("", "ERROR", &error, 0);
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
    if !known_models().contains(&model) {
        let error = format!("invalid model selection (--model \"{model}\" --effort \"{}\"): model {model} is not recognized as a known model or custom model in settings",
            value(&args, "--effort").unwrap_or_default());
        result("", "ERROR", &error, 0);
        eprintln!("{error}");
        std::process::exit(1);
    }
    let Ok(data) = std::env::var("FAKE_AGY_DATA_DIR") else {
        eprintln!("FAKE_AGY_DATA_DIR must be set");
        std::process::exit(64);
    };
    let data = Path::new(&data);
    let conversation = value(&args, "--conversation")
        .unwrap_or_else(|| "11111111-2222-4333-8444-555555555555".into());
    // FC-2: which launch this is (`FAKE_ATTEMPTS_FILE`), and the scenario
    // for it (`FAKE_FIRST_SCENARIO` for the first `FAKE_FIRST_TIMES`).
    let launch = record_attempt(value(&args, "--conversation").is_some(), &conversation);
    let scenario = first_scenario(launch)
        .unwrap_or_else(|| std::env::var("FAKE_SCENARIO").unwrap_or_else(|_| "ok".into()));
    let agy = Agy { data, id: conversation.clone(), model: model_name };
    if scenario == "resume" && value(&args, "--conversation").is_none() {
        result("", "ERROR", "resume requires --conversation", 0);
        std::process::exit(1);
    }
    // FC-1: like the recording, no init line: the only stream line is the
    // result, with an empty conversation id; the error also goes to stderr.
    if scenario == "eligibility" {
        result("", "ERROR", RECORDED_ELIGIBILITY, 0);
        eprintln!("error: {RECORDED_ELIGIBILITY}");
        std::process::exit(1);
    }
    // AGY-1 (AC3): `FAKE_AGY_INIT_TOOLS` is raw JSON for the `tools` value
    // (`[]`, `null`, a list with a non-string, a non-array), so a test can
    // drive what the runner records for an odd list; unset, the real list.
    let init_tools: serde_json::Value = match std::env::var("FAKE_AGY_INIT_TOOLS") {
        Ok(text) => serde_json::from_str(&text).expect("FAKE_AGY_INIT_TOOLS must be JSON"),
        Err(_) => serde_json::json!(INIT_TOOLS),
    };
    emit(serde_json::json!({"event":"init","conversation_id":conversation,
        "init":{"model":model,"cwd":std::env::current_dir().unwrap(),"tools":init_tools,
        "permission_mode":"always-proceed"}}));
    if std::env::var_os("FAKE_AGY_PRINT_TIMEOUT").is_some() || std::env::var_os("FAKE_AGY_PRINT_TIMEOUT_BAD_BYTES").is_some() {
        if std::env::var_os("FAKE_AGY_PRINT_TIMEOUT_BAD_BYTES").is_some() {
            use std::io::Write;
            let _ = std::io::stderr().write_all(&[0xA5, 0xFF, 0x0A]);
        }
        eprintln!("[agy] print timeout after 5m0s with turn in progress; returning partial output");
        result(&conversation, "SUCCESS", "", 0);
        return;
    }
    if scenario == "crash" {
        // AGY-1: the extra attempts happen before the crash too, so a run
        // with no result line can still show a denied unknown name.
        agy.try_extra_tools(1, &scenario);
        if std::env::var_os("FAKE_AGY_RESULT_BEFORE_CRASH").is_some() {
            result(&conversation, "ERROR", "the agent process died", 7);
            if std::env::var_os("FAKE_AGY_HANG_AFTER_RESULT").is_some() {
                std::thread::sleep(std::time::Duration::from_secs(60));
            }
        }
        std::process::exit(137);
    }
    if scenario == "slow" {
        for n in 0..60 { step(&conversation, n, "agent_response", Some(1)); std::thread::sleep(std::time::Duration::from_secs(1)); }
        result(&conversation, "SUCCESS", "DONE\n", 60);
        return;
    }
    if scenario == "quota" {
        agy.try_extra_tools(1, &scenario);
        if agy.allowed(1, "run_command", Agy::run_command_args("echo before-the-limit")) {
            tool(&conversation, 1, "run_command", Some("echo before-the-limit"), Some("before-the-limit\r\n"));
            generic(data, &conversation, 1, 0, "before-the-limit\r\n");
        }
        result(&conversation, "ERROR", "RESOURCE_EXHAUSTED: quota exceeded, try again later", 1);
        std::process::exit(1);
    }
    // FC-1: the recorded network failure; agy exited 3.
    if scenario == "network" {
        result(&conversation, "ERROR", RECORDED_NETWORK_ISSUE, 1);
        std::process::exit(3);
    }
    // FC-1: the recorded interrupted stream, which carries a partial
    // response; agy exited 0.
    if scenario == "stream_interrupted" {
        emit(serde_json::json!({"event":"result","result":{"conversation_id":conversation,
            "status":"ERROR","response":"partial answer","error":RECORDED_STREAM_INTERRUPTED,
            "duration_seconds":1,"num_turns":1,"usage":{"input_tokens":100,"output_tokens":1,
            "thinking_tokens":0,"cache_read_tokens":0,"total_tokens":101}}}));
        return;
    }
    // FC-1: signed out mid-run; from now on `models` says so. Synthetic text.
    if scenario == "auth_expired" {
        write_marker("FAKE_LOGOUT_MARKER");
        result(&conversation, "ERROR", "fake-agy: signed out (synthetic text, not a recording)", 1);
        std::process::exit(1);
    }
    // FC-1: a plain failure after which `models` itself breaks.
    if scenario == "auth_check_breaks" {
        write_marker("FAKE_AUTH_FAIL_MARKER");
        result(&conversation, "ERROR", "fake-agy: generic failure (synthetic)", 1);
        std::process::exit(1);
    }
    if scenario == "spec_confirm_revise" {
        spec_confirm(&agy);
        return;
    }
    if scenario == "review_fix" || scenario.starts_with("fix_confirm_") {
        fix_review(&agy, &scenario, &args);
        return;
    }
    if scenario.starts_with("spec_review_") {
        spec_review(&agy, &scenario, &value(&args, "-p").unwrap());
        return;
    }
    if scenario.starts_with("review_") || scenario.starts_with("source_check") {
        review(&agy, &scenario, &value(&args, "-p").unwrap());
        return;
    }
    let resume = scenario == "resume";
    let offset = if resume { 8 } else { 0 };
    step(&conversation, offset, "user_input", None);
    step(&conversation, offset + 1, "agent_response", Some(if resume { 328 } else { 292 }));
    let filename = if resume { "agy_resumed.txt" } else { "agy_added.txt" };
    let content = if resume { "agy resumed\n" } else { "agy added\n" };
    // AGY-1: the extra attempts come before the first ordinary tool call;
    // the ordinary write still happens whatever the hooks said to them.
    agy.try_extra_tools(offset + 2, &scenario);
    agy.write_file(offset + 2, Path::new(filename), content);
    let mut commands = 0;
    if resume {
        let cmd = "git add agy_resumed.txt && git commit -m 'add agy_resumed'";
        if agy.allowed(offset + 3, "run_command", Agy::run_command_args(cmd)) {
            let _ = std::process::Command::new("git").args(["add", filename]).status();
            let (output, code) = commit(filename, "add agy_resumed");
            tool(&conversation, offset + 3, "run_command", Some(cmd), Some(&output));
            generic(data, &conversation, offset + 3, code, &output);
            commands += 1;
        }
        write_result(commands, &scenario);
        step(&conversation, offset + 4, "agent_response", Some(280));
        result(&conversation, "SUCCESS", "DONE\n", 1383);
        return;
    }
    step(&conversation, 3, "agent_response", Some(180));
    let command = if scenario == "background" { "sleep 1; echo long-done; exit 5" } else { "echo agy-ok" };
    let output = if scenario == "background" { "long-done\r\n" } else { "agy-ok\r\n" };
    if agy.allowed(4, "run_command", Agy::run_command_args(command)) {
        tool(&conversation, 4, "run_command", Some(command), Some(output));
        commands += 1;
        if scenario == "background" {
            transcript(data, &conversation, serde_json::json!({"step_index":4,"source":"MODEL",
                "type":"GENERIC","status":"RUNNING","created_at":"2026-09-29T00:00:00Z",
                "content":format!("Tool is running as a background task with task id: {conversation}/task-4")}));
            std::thread::sleep(std::time::Duration::from_secs(2));
            transcript(data, &conversation, serde_json::json!({"step_index":5,"source":"SYSTEM",
                "type":"SYSTEM_MESSAGE","status":"DONE","created_at":"2026-09-29T00:00:02Z",
                "content":format!("Task id \"{conversation}/task-4\" finished with result:\n\nThe command exited with code 5.\nOutput:\nlong-done\r\n")}));
        } else if scenario == "no_exit_code" {
            transcript(data, &conversation, serde_json::json!({"step_index":4,"source":"MODEL",
                "type":"GENERIC","status":"DONE","created_at":"2026-09-29T00:00:00Z","content":"Created At\nOutput:\nagy-ok"}));
        } else { generic(data, &conversation, 4, 0, output); }
    }
    step(&conversation, 5, "agent_response", Some(149));
    if agy.allowed(6, "run_command", Agy::run_command_args("echo agy-fail && exit 4")) {
        tool(&conversation, 6, "run_command", Some("echo agy-fail && exit 4"), Some("agy-fail\r\n"));
        generic(data, &conversation, 6, 4, "agy-fail\r\n");
        commands += 1;
    }
    step(&conversation, 7, "agent_response", Some(15));
    let commit_cmd = "git add agy_added.txt && git commit -m 'add agy_added'";
    if agy.allowed(8, "run_command", Agy::run_command_args(commit_cmd)) {
        let _ = std::process::Command::new("git").args(["add", filename]).status();
        let (output, code) = commit(filename, "add agy_added");
        tool(&conversation, 8, "run_command", Some(commit_cmd), Some(&output));
        generic(data, &conversation, 8, code, &output);
        commands += 1;
    }
    write_result(commands, &scenario);
    if scenario == "subagent" && agy.allowed(9, "invoke_subagent", serde_json::json!({})) {
        tool(&conversation, 9, "invoke_subagent", None, None);
        tool(&conversation, 9, "invoke_subagent", None, Some(""));
    }
    response_step(&conversation, 9, 3, "DONE\n");
    result(&conversation, "SUCCESS", "DONE\n", 639);
}
