//! Test double for `claude -p`. Never used in production: `THOTH_CLAUDE_BIN`
//! points here only from tests. Event shapes copy the real capture recorded
//! in docs/phase0/02-evidence-capture.md §Q3; no invented field names. The
//! in-repo fixture `tests/fixtures/stream-sample.jsonl` that mirrors that
//! capture lands in Task 7, not here.
//!
//! `thoth doctor` switches, unset by default: `FAKE_CLAUDE_VERSION` is what
//! `--version` prints; `FAKE_CLAUDE_PLUGINS` is what `plugin list --json`
//! prints, verbatim (default `[]`).
//!
//! `thoth profile login` switches, unset by default: a bare `claude` (no
//! arguments) is the interactive login and writes `fake-login.ok` in
//! `CLAUDE_CONFIG_DIR`; `FAKE_LOGIN_FAIL=1` makes it leave without writing;
//! `FAKE_LOGIN_SLEEP_MS` sleeps that long first; `FAKE_LOGIN_BY_FILE=1` makes
//! `auth status` report `loggedIn:false` unless that file exists.
//!
//! W3: the prompt is read like the real `claude -p`: the token after `-p`
//! when there is one that does not start with `-` (the old argv form),
//! otherwise stdin to EOF; with neither, the real CLI's error and exit 1.
//! `FAKE_PROMPT_COPY=<path>` writes the prompt received, byte for byte, to
//! that file (overwriting). Unset by default.
//!
//! `thoth setup` switches, unset by default: `FAKE_CLAUDE_PLUGINS_FILE`, when
//! the file exists, is what `plugin list --json` prints (instead of
//! `FAKE_CLAUDE_PLUGINS`), and `plugin install | enable | update` write a
//! one-entry `thoth@thoth` list into it; `FAKE_CLAUDE_PLUGIN_VERSION` is that
//! entry's version (default this crate's); `FAKE_PLUGIN_FAIL=1` makes
//! `plugin marketplace add | install | enable | update` fail with exit 1.
//!
//! FC-1 switches, unset by default: `FAKE_LOGOUT_MARKER=<path>` makes `auth
//! status` print `{"loggedIn":false}` (exit 0) once that file exists, after
//! the `FAKE_LOGIN_BY_FILE` check; `FAKE_AUTH_FAIL_MARKER=<path>` makes it
//! print `fake-claude: auth check unavailable` on stderr, nothing on stdout,
//! and exit 3 once that file exists. Scenarios `auth_expired` and
//! `auth_check_breaks` write those files and fail; `overloaded` fails with a
//! synthetic 529 overloaded error; `oauth_refresh_failed` fails with the
//! recorded OAuth refresh text.
//!
//! FC-2 switches, unset by default: `FAKE_ATTEMPTS_FILE=<path>` makes every
//! stream-json launch (not `auth status`, `--version`, `plugin *` or the
//! interactive login) append `resume <id>` (with `--resume`) or `fresh <id>`
//! (the `--session-id`, else its own uuid) to that file before it picks its
//! scenario, and count the file's lines: that is launch k (from 1).
//! `FAKE_FIRST_SCENARIO=<name>` is the scenario of launches 1 to
//! `FAKE_FIRST_TIMES` (default 1, unparsable is 1); later ones use the usual
//! scenario. Without `FAKE_ATTEMPTS_FILE`, k is always 1, so every launch
//! runs `FAKE_FIRST_SCENARIO`.
//!
//! SR-2 spec reviewer scenarios: `spec_review_approve`, `spec_review_revise`
//! and `spec_review_bad` read `.agent/spec-review-input/spec.json` (missing or
//! not a spec: a stderr line and exit 70), take the ids from the prompt's
//! `Every id in "criteria" must be one of:` line, and write
//! `.agent/spec-review.json` (`bad`: `{not json`); `FAKE_REVIEW_GATE` holds
//! approve and revise before the write, like the review scenarios.
//!
//! SR-3 edit and author scenarios: `spec_review_edit` reads the same
//! `spec.json` and writes a REVISE with every criterion ok and three edits
//! (replace the first acceptance criterion keeping its id, append a
//! constraint, delete the last non_goal; only two when the spec has no
//! non_goals), anchors taken from the file; `spec_review_edit_bad` writes one
//! edit whose `field` is `risk`; `spec_review_edit_big` inserts a 41,000-byte
//! constraint. `spec_confirm_approve` and `spec_confirm_revise` read
//! `.agent/spec-review-input/edits.json` (and need `proposed.json` next to
//! it) and answer every index `ok`, or the first `revise` with a note and the
//! rest `ok`. `FAKE_REVIEW_GATE` holds all five before the write.
//!
//! MODELS-1 scenario `credits_required`: after `init`, one `is_error` result
//! line in the recorded shape of a Pro account asked for Fable
//! (`api_error_status` 429, `api_error_code` `credits_required`, `api_error`
//! `model_requires_usage_credits`, the recorded `result` text), exit 1, no
//! result.json.
//!
//! TIER-1 switch, unset by default: `FAKE_SUBSCRIPTION_TYPE` changes the
//! `subscriptionType` that a logged-in `auth status` prints (default `pro`):
//! `none` leaves the key out, any other value is printed as it is (blanks,
//! slashes and all, so tests can feed an invalid tier).
//!
//! E7 switches, unset by default: a launch whose prompt is exactly `/usage`
//! answers like Claude Code's local `/usage` in stream-json mode, after the
//! args log and before any scenario; it writes no result.json and leaves
//! `FAKE_ATTEMPTS_FILE` alone. `FAKE_CLAUDE_ENV_LOG=<path>` first appends a
//! JSON array naming which of `CLAUDE_CODE_OAUTH_TOKEN`, `ANTHROPIC_API_KEY`
//! and `ANTHROPIC_AUTH_TOKEN` are set (names only). `FAKE_CLAUDE_USAGE_FILE`
//! prints that file verbatim (exit 0; unreadable: a stderr line, exit 70).
//! Otherwise `FAKE_CLAUDE_USAGE`: unset or `ok` prints
//! tests/fixtures/claude-usage-stream.jsonl (exit 0); `exit` prints
//! `fake-claude: usage failed` on stderr (exit 1); `hang` writes
//! `FAKE_CLAUDE_HANG_STARTED`, sleeps `FAKE_CLAUDE_HANG_SECS` seconds (a
//! positive integer; unset or anything else is 6), writes
//! `FAKE_CLAUDE_HANG_MARKER` (exit 0); `signed_out` prints an init line and an `is_error` result line
//! `Not logged in · Please run /login` on stdout (exit 1); anything else is
//! a stderr line and exit 64.
//!
//! RES-1a scenarios, the same in fake-codex and fake-agy: `research_ok` runs
//! like `ok` (same summary) but its claims are one completion and two research
//! claims, `research claim 1` and `research claim 2`; `FAKE_SOURCES` (a JSON
//! array of `{"url", "quote"}`) gives the first its first item and the second
//! the rest, and unset each gets `https://example.invalid/fake` with quote
//! `fake quote`. `research_unsourced` delivers a completed result.json with the
//! completion claim only; `research_no_sources` adds one research claim whose
//! `sources` is `[]`. No URL is ever opened.
//!
//! RD-2 switch, unset by default: `FAKE_ENV_LOG=<path>` makes every worker
//! launch (where `FAKE_PROMPT_COPY` is written; not `--version`, `plugin *`,
//! `auth status` or the interactive login) append one JSON line
//! `{"names":[...]}`: the sorted names of its environment variables, never a
//! value.
//!
//! BRIEF-1 switches, unset by default: in the scenarios that write a
//! result.json with a task_id through `write_result_json` (`ok`, `slow`,
//! `resume`), `FAKE_RESULT_SUMMARY` replaces the summary and
//! `FAKE_RESULT_BRIEF` adds a `brief`; `FAKE_REVIEW_NOTE` replaces the note of
//! `review_reject`'s unmet criterion.

// Source-check scenarios: `source_check` checks the prompt's file:// pages;
// `source_check_bad` writes {not json. FAKE_SOURCE_CHECK_VERDICT overrides
// the computed verdict; FAKE_REVIEW_GATE holds output until its file exists.
#[path = "support/source_check.rs"]
mod source_check;
// RFIX-1: `review_fix` hands in `FAKE_FIX_PLAN`'s document (and makes its
// side effects); `fix_confirm_approve`, `fix_confirm_revise` (edit 0 revise)
// and `fix_confirm_bad` (`{not json`) answer the edits in
// `.agent/review-fix-input/edits.json`. All write `.agent/review.json`.
#[path = "support/review_fix.rs"]
mod review_fix;

use std::io::Write;

/// The file the interactive login writes; Thoth itself never knows it.
const LOGIN_MARKER: &str = "fake-login.ok";

fn login_sleep() {
    if let Some(ms) = std::env::var("FAKE_LOGIN_SLEEP_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// The prompt this launch received, the way `claude -p` takes it: the
/// argument after `-p` if it is there and does not start with `-`, otherwise
/// stdin read to EOF. Neither: the real CLI's error, exit 1.
fn prompt_text(args: &[String]) -> String {
    if let Some(p) = arg_value(args, "-p").filter(|p| !p.starts_with('-')) {
        return p;
    }
    let mut buf = String::new();
    let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
    if buf.is_empty() {
        eprintln!("Error: Input must be provided either through stdin or as a prompt argument when using --print");
        std::process::exit(1);
    }
    buf
}

/// RD-2: appends `{"names":[...]}` to `FAKE_ENV_LOG`, the sorted names of
/// this launch's environment variables. Names only, never a value. Unset:
/// writes nothing.
fn log_env_names() {
    let Some(path) = std::env::var_os("FAKE_ENV_LOG") else { return };
    let mut names: Vec<String> =
        std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
    names.sort();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", serde_json::json!({"names": names}));
    }
}

/// FC-1: whether the env var names a file that exists. Unset: false.
fn marker_exists(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|p| std::path::Path::new(&p).is_file())
}

/// FC-1: writes the file the env var names, if it is set.
fn write_marker(var: &str) {
    if let Some(p) = std::env::var_os(var) {
        let _ = std::fs::write(p, "1\n");
    }
}

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

/// The recorded stream-json answer of Claude Code 2.1.296 to `/usage`.
const USAGE_STREAM: &str = include_str!("../../tests/fixtures/claude-usage-stream.jsonl");

/// E7: answers `-p /usage` as the switches say, then exits.
fn usage_answer() -> ! {
    if let Ok(path) = std::env::var("FAKE_CLAUDE_ENV_LOG") {
        let present: Vec<&str> = ["CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"]
            .into_iter()
            .filter(|name| std::env::var_os(name).is_some())
            .collect();
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{}", serde_json::json!(present));
        }
    }
    let mut out = std::io::stdout();
    if let Ok(path) = std::env::var("FAKE_CLAUDE_USAGE_FILE") {
        match std::fs::read(&path) {
            Ok(bytes) => {
                let _ = out.write_all(&bytes);
                let _ = out.flush();
                std::process::exit(0);
            }
            Err(error) => {
                eprintln!("fake-claude: cannot read FAKE_CLAUDE_USAGE_FILE: {error}");
                std::process::exit(70);
            }
        }
    }
    match std::env::var("FAKE_CLAUDE_USAGE").as_deref() {
        Err(_) | Ok("ok") => {
            let _ = out.write_all(USAGE_STREAM.as_bytes());
            let _ = out.flush();
            std::process::exit(0);
        }
        Ok("exit") => {
            eprintln!("fake-claude: usage failed");
            std::process::exit(1);
        }
        Ok("hang") => {
            write_marker("FAKE_CLAUDE_HANG_STARTED");
            // `FAKE_CLAUDE_HANG_SECS`: how long the stand-in hangs. The
            // timeout test sets it well above the timeout so its "ended at
            // once" bound can stay under half of this lifetime.
            let hang_secs = std::env::var("FAKE_CLAUDE_HANG_SECS")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(6);
            std::thread::sleep(std::time::Duration::from_secs(hang_secs));
            write_marker("FAKE_CLAUDE_HANG_MARKER");
            std::process::exit(0);
        }
        Ok("signed_out") => {
            emit(&serde_json::json!({"type": "system", "subtype": "init"}));
            emit(&serde_json::json!({"type": "result", "subtype": "success", "is_error": true, "num_turns": 0,
                "total_cost_usd": 0, "result": "Not logged in · Please run /login"}));
            std::process::exit(1);
        }
        Ok(_) => {
            eprintln!("fake-claude: unknown FAKE_CLAUDE_USAGE");
            std::process::exit(64);
        }
    }
}

fn emit(v: &serde_json::Value) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn init_event(session_id: &str, cwd: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "system",
        "subtype": "init",
        "cwd": cwd,
        "session_id": session_id,
        "tools": ["Bash", "Read", "Write", "Edit", "Glob", "Grep"],
        "model": "fake-model",
        "permissionMode": "default",
        "apiKeySource": "none",
        "claude_code_version": "fake-0.0.0",
        "output_style": "default",
        "uuid": uuid()
    })
}

fn assistant_event(session_id: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "assistant",
        "message": {
            "model": "fake-model",
            "id": format!("msg_{}", uuid().replace('-', "")),
            "type": "message",
            "role": "assistant",
            "content": [{"type": "text", "text": text}],
            "stop_reason": serde_json::Value::Null,
            "usage": {"input_tokens": 10, "output_tokens": 3}
        },
        "parent_tool_use_id": serde_json::Value::Null,
        "session_id": session_id,
        "uuid": uuid(),
        "timestamp": "2026-09-14T00:00:00.000Z"
    })
}

fn result_event(session_id: &str, is_error: bool, text: &str, api_error_status: serde_json::Value)
    -> serde_json::Value
{
    serde_json::json!({
        "type": "result",
        "subtype": if is_error { "error_during_execution" } else { "success" },
        "is_error": is_error,
        "session_id": session_id,
        // The numbers of a real result line (tests/fixtures/stream-sample.jsonl).
        "usage": {"input_tokens": 34, "cache_creation_input_tokens": 1840,
                  "cache_read_input_tokens": 202655, "output_tokens": 610,
                  "output_tokens_details": {"thinking_tokens": 226}},
        // Real shape (claude 2.1.283): keyed by model id. A little haiku next
        // to the requested model is what a normal run looks like.
        "modelUsage": {
            "fake-model": {"inputTokens": 34, "outputTokens": 610, "costUSD": 0.0},
            "fake-haiku": {"inputTokens": 5, "outputTokens": 12, "costUSD": 0.0}
        },
        // Test switch: `FAKE_SUBAGENTS=<n>` reports n spawned subagents.
        "subagent_stats": {"spawned": std::env::var("FAKE_SUBAGENTS")
            .ok()
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(0)},
        "result": text,
        "api_error_status": api_error_status,
        "num_turns": 2,
        "duration_ms": 10,
        "duration_api_ms": 8,
        "total_cost_usd": 0.0,
        "stop_reason": "end_turn",
        "terminal_reason": if is_error { "error" } else { "completed" },
        "permission_denials": [],
        "uuid": uuid()
    })
}

/// Real shape, copied from the Phase 0 capture.
fn rate_limit_event(session_id: &str, utilization: f64) -> serde_json::Value {
    serde_json::json!({
        "type": "rate_limit_event",
        "rate_limit_info": {
            "status": "allowed",
            "resetsAt": 1789381800i64,
            "rateLimitType": "five_hour",
            "overageStatus": "rejected",
            "overageDisabledReason": "org_level_disabled",
            "isUsingOverage": false,
            "unifiedWindows": {
                "five_hour": {"utilization": utilization, "resetsAt": 1789381800i64},
                "seven_day": {"utilization": 0.02, "resetsAt": 1789963200i64}
            }
        },
        "uuid": uuid(),
        "session_id": session_id
    })
}

fn write_result_json(task_id: &str, summary: &str, include_task_id: bool) {
    let dir = std::path::Path::new(".agent");
    let _ = std::fs::create_dir_all(dir);
    // `completion` rather than `test_result`: these scenarios fire no hooks, so
    // there is no evidence for them to cite, and v6 §2.5 rule 2 (Task 5) now
    // refuses an uncited `test_result`. The `hooks` scenario is the one that
    // exercises a cited claim.
    let body = if include_task_id {
        let mut body = serde_json::json!({
            "task_id": task_id,
            "status": "completed",
            "summary": summary,
            "claims": [{"kind": "completion", "text": "fake worker finished", "evidence": []}],
            "verification_requests": [{"cmd": "echo verify", "cwd": "."}]
        });
        // BRIEF-1 switches: `FAKE_RESULT_SUMMARY` replaces the summary and
        // `FAKE_RESULT_BRIEF` adds a `brief` with that text.
        if let Ok(text) = std::env::var("FAKE_RESULT_SUMMARY") {
            body["summary"] = serde_json::json!(text);
        }
        if let Ok(text) = std::env::var("FAKE_RESULT_BRIEF") {
            body["brief"] = serde_json::json!(text);
        }
        body
    } else {
        serde_json::json!({
            "status": "completed",
            "summary": summary,
            "claims": [{"kind": "completion", "text": "fake worker finished", "evidence": []}]
        })
    };
    let _ = std::fs::write(dir.join("result.json"), serde_json::to_string_pretty(&body).unwrap());
}

/// A completed result.json with exactly these claims.
fn write_claims_result(task_id: &str, summary: &str, claims: Vec<serde_json::Value>) {
    let dir = std::path::Path::new(".agent");
    let _ = std::fs::create_dir_all(dir);
    let body = serde_json::json!({
        "task_id": task_id,
        "status": "completed",
        "summary": summary,
        "claims": claims,
        "verification_requests": [{"cmd": "echo verify", "cwd": "."}]
    });
    let _ = std::fs::write(dir.join("result.json"), serde_json::to_string_pretty(&body).unwrap());
}

/// RES-1a: the research claims a research scenario adds after its completion
/// claim. `research_ok`: `research claim 1` with `FAKE_SOURCES`' first item and
/// `research claim 2` with the rest (unset: one placeholder source each);
/// `research_no_sources`: one research claim with `sources: []`;
/// `research_unsourced` (and anything else): none. Opens no URL.
fn research_claims(scenario: &str) -> Vec<serde_json::Value> {
    let claim = |n: usize, sources: serde_json::Value| {
        serde_json::json!({"kind": "research", "text": format!("research claim {n}"),
            "evidence": [], "sources": sources})
    };
    match scenario {
        "research_ok" => {
            let given: Vec<serde_json::Value> = std::env::var("FAKE_SOURCES")
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok())
                .unwrap_or_default();
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

/// Pulls the hook command line out of the `--settings` file thoth wrote, so
/// the fake invokes the same receiver a real Claude Code would.
fn hook_command_from_settings(args: &[String]) -> Option<(String, Vec<String>)> {
    let path = arg_value(args, "--settings")?;
    let text = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    let cmd = v["hooks"]["PostToolUse"][0]["hooks"][0]["command"].as_str()?;
    split_hook_command(cmd)
}

/// `"C:\path\thoth.exe" hook` -> ("C:\path\thoth.exe", ["hook"]).
/// Only the leading argument may be quoted, which is the only form
/// `write_hooks_settings` produces.
fn split_hook_command(s: &str) -> Option<(String, Vec<String>)> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix('"') {
        let end = rest.find('"')?;
        Some((
            rest[..end].to_string(),
            rest[end + 1..].split_whitespace().map(String::from).collect(),
        ))
    } else {
        let mut it = s.split_whitespace();
        let program = it.next()?.to_string();
        Some((program, it.map(String::from).collect()))
    }
}

fn call_hook(program: &str, args: &[String], payload: &serde_json::Value) {
    let mut child = match std::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(payload.to_string().as_bytes());
        // `si` drops here, closing the pipe so the hook's read_to_end returns.
    }
    let _ = child.wait();
}

fn hook_pre(session_id: &str, cwd: &str, tuid: &str, cmd: &str) -> serde_json::Value {
    serde_json::json!({
        "session_id": session_id,
        "cwd": cwd,
        "permission_mode": "default",
        "hook_event_name": "PreToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": cmd, "description": "fake command"},
        "tool_use_id": tuid
    })
}

fn hook_post(
    session_id: &str,
    cwd: &str,
    tuid: &str,
    cmd: &str,
    stdout: &str,
    persisted: Option<(&str, u64)>,
) -> serde_json::Value {
    let mut resp = serde_json::json!({
        "stdout": stdout,
        "stderr": "",
        "interrupted": false,
        "isImage": false,
        "noOutputExpected": false
    });
    if let Some((path, size)) = persisted {
        resp["persistedOutputPath"] = serde_json::json!(path);
        resp["persistedOutputSize"] = serde_json::json!(size);
    }
    serde_json::json!({
        "session_id": session_id,
        "cwd": cwd,
        "permission_mode": "default",
        "hook_event_name": "PostToolUse",
        "tool_name": "Bash",
        "tool_input": {"command": cmd, "description": "fake command"},
        "tool_response": resp,
        "tool_use_id": tuid,
        "duration_ms": 83
    })
}

fn hook_fail(
    session_id: &str,
    cwd: &str,
    tuid: &str,
    cmd: &str,
    error: &str,
) -> serde_json::Value {
    serde_json::json!({
        "session_id": session_id,
        "cwd": cwd,
        "permission_mode": "default",
        "hook_event_name": "PostToolUseFailure",
        "tool_name": "Bash",
        "tool_input": {"command": cmd, "description": "fake command"},
        "tool_use_id": tuid,
        "error": error,
        "is_interrupt": false,
        "duration_ms": 77
    })
}

/// The exact bytes Phase 0's `seq 1 20000` produced: 108894.
fn seq_1_to_20000() -> String {
    (1..=20000).map(|n| format!("{n}\n")).collect()
}

/// Writes the persisted tool output where Claude Code would - under this
/// session's config dir - and returns (path, size).
fn write_persisted_output(session_id: &str) -> Option<(String, u64)> {
    let cfg = std::env::var("CLAUDE_CONFIG_DIR").ok()?;
    let dir = std::path::Path::new(&cfg)
        .join("projects")
        .join(session_id)
        .join("tool-results");
    std::fs::create_dir_all(&dir).ok()?;
    let body = seq_1_to_20000();
    let path = dir.join("fake-output.txt");
    std::fs::write(&path, &body).ok()?;
    Some((path.display().to_string(), body.len() as u64))
}

fn git_quiet(args: &[&str]) -> bool {
    std::process::Command::new("git")
        .args(args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `FAKE_EDIT_PLAN`'s JSON, applied in the current directory and staged:
/// `write` is `[{"path", "text"}]` (or `"hex"` for bytes that are not UTF-8),
/// `delete` is `[path]`, and `gitlink` is `[{"path", "sha"}]`: the path's
/// index entry becomes a gitlink to `sha` and its file an empty directory. A plan
/// that cannot be read changes nothing.
fn apply_edit_plan(plan: &std::path::Path) {
    let v: serde_json::Value = std::fs::read_to_string(plan)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or(serde_json::Value::Null);
    let list = |key: &str| v[key].as_array().cloned().unwrap_or_default();
    for w in list("write") {
        let Some(path) = w["path"].as_str() else { continue };
        let bytes: Vec<u8> = match (w["text"].as_str(), w["hex"].as_str()) {
            (Some(text), _) => text.as_bytes().to_vec(),
            (None, Some(hex)) => (0..hex.len() / 2)
                .filter_map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok())
                .collect(),
            (None, None) => continue,
        };
        let p = std::path::Path::new(path);
        if let Some(parent) = p.parent().filter(|p| !p.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(p, bytes);
        git_quiet(&["add", "--", path]);
    }
    for d in list("delete") {
        if let Some(path) = d.as_str() {
            git_quiet(&["rm", "-q", "-f", "--", path]);
        }
    }
    for g in list("gitlink") {
        let (Some(path), Some(sha)) = (g["path"].as_str(), g["sha"].as_str()) else { continue };
        git_quiet(&["rm", "-q", "-f", "--", path]);
        git_quiet(&["update-index", "--add", "--cacheinfo", &format!("160000,{sha},{path}")]);
        // an empty directory is a submodule that is not checked out: the
        // work tree stays clean
        let _ = std::fs::create_dir_all(path);
    }
}

/// Polls the control-plane-generated index until it holds at least `min` rows,
/// then returns their codes. This is the fake's version of the instruction the
/// real worker prompt gives ("If the index is empty when you finish, re-run the
/// relevant command once so it is captured").
fn wait_for_index(min: usize, timeout: std::time::Duration) -> Vec<String> {
    let path = std::path::Path::new(".agent").join("evidence-index.json");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(rows) = serde_json::from_str::<Vec<serde_json::Value>>(&text) {
                let codes: Vec<String> = rows
                    .iter()
                    .filter_map(|r| r["code"].as_str().map(String::from))
                    .collect();
                if codes.len() >= min {
                    return codes;
                }
            }
        }
        if std::time::Instant::now() >= deadline {
            return Vec::new();
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

fn write_result_with_evidence(task_id: &str, summary: &str, codes: &[String]) {
    let dir = std::path::Path::new(".agent");
    let _ = std::fs::create_dir_all(dir);
    let mut claims = vec![
        serde_json::json!({"kind": "test_result", "text": "fake suite green", "evidence": codes}),
        serde_json::json!({"kind": "completion", "text": "all acceptance criteria met", "evidence": []}),
    ];
    // Test switch: `FAKE_PROTECTED_CLAIM=<text>` adds a `protected_change`
    // claim with that text.
    if let Ok(text) = std::env::var("FAKE_PROTECTED_CLAIM") {
        claims.push(serde_json::json!({"kind": "protected_change", "text": text, "evidence": []}));
    }
    let body = serde_json::json!({
        "task_id": task_id,
        "status": "completed",
        "summary": summary,
        "claims": claims,
        "verification_requests": [
            {"cmd": "echo verify-one", "cwd": "."},
            {"cmd": "exit 3", "cwd": "."}
        ]
    });
    let _ = std::fs::write(
        dir.join("result.json"),
        serde_json::to_string_pretty(&body).unwrap(),
    );
}

/// The acceptance-criterion ids the runner listed for the reviewer: only the
/// line starting `Every id in "criteria" must be one of:`. Scanning the whole
/// prompt would also pick up an `AC<digits>` that happens to sit inside the
/// task ULID on the last line or inside an evidence label.
fn criterion_ids_in(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .find(|l| l.starts_with("Every id in \"criteria\" must be one of:"))
        .map(|l| tokens_in(l, "AC"))
        .unwrap_or_default()
}

/// The first name on the prompt's `Every "category" must be one of:` line,
/// which `review_reject` labels its findings with.
fn first_category_in(prompt: &str) -> Option<String> {
    prompt
        .lines()
        .find_map(|l| l.strip_prefix("Every \"category\" must be one of: "))
        .and_then(|rest| rest.split([',', '.']).next())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// `review_reject`'s labels on an unmet criterion or an extra finding.
fn label_finding(finding: &mut serde_json::Value, category: Option<&str>) {
    finding["fix_size"] = serde_json::json!("S");
    if let Some(category) = category {
        finding["category"] = serde_json::json!(category);
    }
}

/// SR-2: where the runner writes the spec reviewer's input.
const SPEC_REVIEW_INPUT: &str = ".agent/spec-review-input/spec.json";

/// Whether `path` holds JSON with an `acceptance_criteria` array.
fn is_spec_file(path: &std::path::Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .map(|v| v["acceptance_criteria"].is_array())
        .unwrap_or(false)
}

/// The spec review document of `spec_review_approve` / `spec_review_revise`.
fn spec_review_doc(ids: &[String], revise: bool) -> serde_json::Value {
    let criteria: Vec<serde_json::Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| match (i, revise) {
            (0, true) => serde_json::json!({"id": id, "verdict": "revise", "findings": [
                {"check": "exists", "location": "src/lib.rs:1", "note": "fake: the function the spec names does not exist"}
            ]}),
            (0, false) => serde_json::json!({"id": id, "verdict": "ok", "findings": [
                {"check": "steps", "location": "spec", "note": "fake: the test steps could name the command to run"}
            ]}),
            _ => serde_json::json!({"id": id, "verdict": "ok", "findings": []}),
        })
        .collect();
    serde_json::json!({
        "verdict": if revise { "REVISE" } else { "APPROVE" },
        "criteria": criteria,
    })
}

/// SR-3: the string entries of a spec's list field, `[]` when absent.
fn spec_list(spec: &serde_json::Value, key: &str) -> Vec<String> {
    spec[key]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// SR-3: the anchor rule a real reviewer follows: the first 10 characters of
/// the trimmed entry, or the whole entry when it is shorter. Thoth counts the
/// anchor after trimming it, so when the tenth character is a space the
/// anchor goes on to the next non-space one.
fn anchor_of(entry: &str) -> String {
    let mut out = String::new();
    for c in entry.trim().chars() {
        out.push(c);
        if out.trim_end().chars().count() >= 10 {
            break;
        }
    }
    out
}

/// SR-3: the edit document of `spec_review_edit` (every criterion ok, three
/// edits built from the spec's own text), `spec_review_edit_bad` (one edit on
/// `risk`) and `spec_review_edit_big` (a 41,000-byte constraint).
fn spec_edit_doc(spec: &serde_json::Value, ids: &[String], scenario: &str) -> serde_json::Value {
    let criterion = ids.first().cloned().unwrap_or_else(|| "AC1".to_string());
    let criteria: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| serde_json::json!({"id": id, "verdict": "ok", "findings": []}))
        .collect();
    let edits: Vec<serde_json::Value> = match scenario {
        "spec_review_edit_bad" => vec![serde_json::json!({
            "field": "risk", "op": "replace", "at": 0, "anchor": "important", "text": "low",
            "criterion": criterion, "reason": "fake: lower the risk"
        })],
        "spec_review_edit_big" => vec![serde_json::json!({
            "field": "constraints", "op": "insert", "at": spec_list(spec, "constraints").len(),
            "anchor": "", "text": "x".repeat(41_000),
            "criterion": criterion, "reason": "fake: a constraint too big for any spec"
        })],
        _ => {
            let acs = spec_list(spec, "acceptance_criteria");
            let first = acs.first().cloned().unwrap_or_default();
            let (head, rest) = first
                .split_once(':')
                .map(|(h, r)| (h.to_string(), r.trim().to_string()))
                .unwrap_or((first.clone(), String::new()));
            let mut edits = vec![
                serde_json::json!({
                    "field": "acceptance_criteria", "op": "replace", "at": 0, "anchor": anchor_of(&first),
                    "text": format!("{head}: {rest} (fake edit: names the function to call)"),
                    "criterion": criterion, "reason": "fake: the criterion did not name what to call"
                }),
                serde_json::json!({
                    "field": "constraints", "op": "insert", "at": spec_list(spec, "constraints").len(),
                    "anchor": "", "text": "fake edit: do not add a crate",
                    "criterion": criterion, "reason": "fake: the spec never said so"
                }),
            ];
            let non_goals = spec_list(spec, "non_goals");
            if let Some(last) = non_goals.last() {
                edits.push(serde_json::json!({
                    "field": "non_goals", "op": "delete", "at": non_goals.len() - 1, "anchor": anchor_of(last),
                    "text": "", "criterion": criterion, "reason": "fake: this non-goal repeats a constraint"
                }));
            }
            edits
        }
    };
    serde_json::json!({"verdict": "REVISE", "criteria": criteria, "edits": edits})
}

/// SR-3: the author's document of `spec_confirm_approve` (every edit ok) and
/// `spec_confirm_revise` (the first edit revise with a note, the rest ok),
/// from the indexes in `edits.json`.
fn spec_confirm_doc(edits: &[serde_json::Value], revise_first: bool) -> serde_json::Value {
    let answers: Vec<serde_json::Value> = edits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let index = e["index"].as_u64().unwrap_or(i as u64);
            if revise_first && i == 0 {
                serde_json::json!({"index": index, "verdict": "revise", "note": "fake author: keep the original wording of this entry"})
            } else {
                serde_json::json!({"index": index, "verdict": "ok", "note": ""})
            }
        })
        .collect();
    serde_json::json!({
        "verdict": if revise_first { "REVISE" } else { "APPROVE" },
        "edits": answers,
    })
}

/// Every `<prefix><digits>` token in `text`, first-seen order. Used to read the
/// acceptance-criterion ids straight out of the prompt the runner built, which
/// is exactly where a real reviewer gets them.
fn tokens_in(text: &str, prefix: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let p: Vec<char> = prefix.chars().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        if i + p.len() <= chars.len() && chars[i..i + p.len()] == p[..] {
            let mut j = i + p.len();
            let mut digits = String::new();
            while j < chars.len() && chars[j].is_ascii_digit() {
                digits.push(chars[j]);
                j += 1;
            }
            if !digits.is_empty() {
                let token = format!("{prefix}{digits}");
                if !out.contains(&token) {
                    out.push(token);
                }
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // `claude --version` (thoth doctor): `FAKE_CLAUDE_VERSION`, default
    // `9.9.9 (fake Claude Code)`. No init event, no args log.
    if args == ["--version"] {
        let v = std::env::var("FAKE_CLAUDE_VERSION")
            .unwrap_or_else(|_| "9.9.9 (fake Claude Code)".to_string());
        println!("{v}");
        std::process::exit(0);
    }
    // `claude plugin list --json` (thoth doctor): `FAKE_CLAUDE_PLUGINS`
    // verbatim, default `[]`. No init event, no args log.
    if args == ["plugin", "list", "--json"] {
        // SETUP-3: `FAKE_CLAUDE_PLUGINS_FILE`, when it names an existing
        // file, is printed verbatim instead.
        if let Some(text) = std::env::var("FAKE_CLAUDE_PLUGINS_FILE")
            .ok()
            .and_then(|p| std::fs::read_to_string(p).ok())
        {
            print!("{text}");
            std::process::exit(0);
        }
        let v = std::env::var("FAKE_CLAUDE_PLUGINS").unwrap_or_else(|_| "[]".to_string());
        println!("{v}");
        std::process::exit(0);
    }

    // `claude auth status --json`
    if args.len() >= 2 && args[0] == "auth" && args[1] == "status" {
        // Test seam (Task 8 fix round 1, I1): `dispatch` reads the task row
        // before it probes auth and opens its transaction well after, so a
        // test that has to change `tasks.status` *between* that read and that
        // transaction can park the probe here. The marker file is how the
        // test knows the read has already happened; the delay is the window
        // it writes in. Neither variable is set by any other test.
        if let Ok(marker) = std::env::var("FAKE_AUTH_MARKER") {
            let _ = std::fs::write(&marker, "probing\n");
        }
        if let Some(ms) = std::env::var("FAKE_AUTH_DELAY_MS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
        {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        let cfg = std::env::var("CLAUDE_CONFIG_DIR").unwrap_or_default();
        if std::env::var("FAKE_LOGIN_BY_FILE").as_deref() == Ok("1")
            && !std::path::Path::new(&cfg).join(LOGIN_MARKER).is_file()
        {
            emit(&serde_json::json!({"loggedIn": false}));
            std::process::exit(0);
        }
        // FC-1 test switches: the run's own scenario writes these files.
        if marker_exists("FAKE_LOGOUT_MARKER") {
            emit(&serde_json::json!({"loggedIn": false}));
            std::process::exit(0);
        }
        if marker_exists("FAKE_AUTH_FAIL_MARKER") {
            eprintln!("fake-claude: auth check unavailable");
            std::process::exit(3);
        }
        let mut status = serde_json::json!({
            "loggedIn": true,
            "authMethod": "claude.ai",
            "apiProvider": "firstParty",
            "email": "fake@example.com",
            "orgName": "fake org",
            "subscriptionType": "pro",
            "configDirectory": cfg
        });
        // TIER-1 switch: `none` drops the key, any other value replaces it.
        match std::env::var("FAKE_SUBSCRIPTION_TYPE") {
            Ok(v) if v == "none" => {
                status.as_object_mut().unwrap().remove("subscriptionType");
            }
            Ok(v) => status["subscriptionType"] = serde_json::json!(v),
            Err(_) => {}
        }
        // PANEL-3 switch: `none` drops the email, any other value replaces it.
        match std::env::var("FAKE_AUTH_EMAIL") {
            Ok(v) if v == "none" => {
                status.as_object_mut().unwrap().remove("email");
            }
            Ok(v) => status["email"] = serde_json::json!(v),
            Err(_) => {}
        }
        emit(&status);
        std::process::exit(0);
    }

    // Test seam (S3-7): append this launch's argv, cwd and config dir as one
    // JSON line. Written after the `auth status` branch, so probes are not
    // logged. Unset in every other test.
    if let Ok(path) = std::env::var("FAKE_CLAUDE_ARGS_LOG") {
        use std::io::Write as _;
        let line = serde_json::json!({
            "args": args,
            "cwd": std::env::current_dir().map(|d| d.display().to_string()).unwrap_or_default(),
            "claude_config_dir": std::env::var("CLAUDE_CONFIG_DIR").ok(),
        });
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{line}");
        }
    }

    // `claude plugin marketplace add X | install | enable | update` (thoth
    // setup), after the args log so each launch is logged. Prints nothing on
    // stdout.
    let plugin_change = args.len() >= 2
        && args[0] == "plugin"
        && (args[1] == "install" || args[1] == "enable" || args[1] == "update"
            || (args.len() == 4 && args[1] == "marketplace" && args[2] == "add"));
    if plugin_change {
        if std::env::var("FAKE_PLUGIN_FAIL").as_deref() == Ok("1") {
            eprintln!("fake-claude: plugin command failed");
            std::process::exit(1);
        }
        if args[1] != "marketplace" {
            if let Ok(path) = std::env::var("FAKE_CLAUDE_PLUGINS_FILE") {
                let version = std::env::var("FAKE_CLAUDE_PLUGIN_VERSION")
                    .unwrap_or_else(|_| env!("CARGO_PKG_VERSION").to_string());
                let list = serde_json::json!([{
                    "id": "thoth@thoth",
                    "version": version,
                    "scope": "user",
                    "enabled": true,
                    "installPath": "fake",
                }]);
                let _ = std::fs::write(path, list.to_string());
            }
        }
        std::process::exit(0);
    }

    // Bare `claude`: the interactive login (`thoth profile login`). Prints
    // nothing on stdout.
    if args.is_empty() {
        let cfg = std::env::var("CLAUDE_CONFIG_DIR").ok().map(std::path::PathBuf::from);
        let Some(cfg) = cfg.filter(|d| d.is_dir()) else {
            eprintln!("fake-claude: CLAUDE_CONFIG_DIR must be an existing directory");
            std::process::exit(64);
        };
        login_sleep();
        if std::env::var("FAKE_LOGIN_FAIL").as_deref() == Ok("1") {
            eprintln!("fake-claude: closed without logging in");
            std::process::exit(0);
        }
        let _ = std::fs::write(cfg.join(LOGIN_MARKER), "1\n");
        eprintln!("fake-claude: logged in");
        std::process::exit(0);
    }

    let prompt = prompt_text(&args);
    if let Ok(path) = std::env::var("FAKE_PROMPT_COPY") {
        let _ = std::fs::write(path, prompt.as_bytes());
    }
    log_env_names();

    // E7: the local `/usage` command. Writes no result.json, does not touch
    // `FAKE_ATTEMPTS_FILE` and runs no scenario.
    if prompt == "/usage" {
        usage_answer();
    }

    let task_id = std::env::var("FAKE_TASK_ID").unwrap_or_else(|_| "UNKNOWN-TASK".to_string());
    let resume = arg_value(&args, "--resume");
    let session_id = resume
        .clone()
        .or_else(|| arg_value(&args, "--session-id"))
        .unwrap_or_else(uuid);

    // FC-2: which launch this is (`FAKE_ATTEMPTS_FILE`), and the scenario
    // for it (`FAKE_FIRST_SCENARIO` for the first `FAKE_FIRST_TIMES`).
    let launch = record_attempt(resume.is_some(), &session_id);
    // `FAKE_CLAUDE_SCENARIO` wins over `FAKE_SCENARIO`, so one test can run
    // claude and codex in different scenarios.
    let scenario = first_scenario(launch).unwrap_or_else(|| {
        std::env::var("FAKE_CLAUDE_SCENARIO")
            .or_else(|_| std::env::var("FAKE_SCENARIO"))
            .unwrap_or_else(|_| "ok".to_string())
    });
    let cwd = std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    emit(&init_event(&session_id, &cwd));

    match scenario.as_str() {
        "ok" => {
            emit(&rate_limit_event(&session_id, 0.07));
            emit(&assistant_event(&session_id, "I will do the thing."));
            write_result_json(&task_id, "fake worker completed the task", true);
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "slow" => {
            if std::env::var("FAKE_SPAWN_CHILD").as_deref() == Ok("1") {
                // Give the parent (thoth's `spawn_child`, or the test
                // harness in `tests/process.rs`) a moment to finish
                // `AssignProcessToJobObject` before this process's own
                // grandchild exists. A grandchild spawned before that
                // assignment lands is not yet a member of the Job Object
                // and would survive a job-based `kill_tree`; this sleep
                // keeps that race from making tests flaky. Production
                // thoth callers race the same window, narrowed to
                // whatever it actually takes `spawn_child` to return.
                std::thread::sleep(std::time::Duration::from_millis(50));
                // Linux: the grandchild `setsid`s out of this process's
                // session and process group on purpose, so only the worker's
                // cgroup can still take it down - the case a process-group
                // kill would miss (docs/post-phase2/linux-support-scope-
                // 2026-09-26.md decision 1). The pid reported is the `sleep`
                // itself: `setsid` execs it without forking, since this child
                // is not a process-group leader.
                #[cfg(windows)]
                let mut grandchild = std::process::Command::new("powershell.exe");
                #[cfg(windows)]
                grandchild.args(["-NoProfile", "-Command", "Start-Sleep -Seconds 300"]);
                #[cfg(not(windows))]
                let mut grandchild = std::process::Command::new("setsid");
                #[cfg(not(windows))]
                grandchild.args(["sleep", "300"]);
                match grandchild
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                {
                    Ok(c) => emit(&serde_json::json!({
                        "type": "system",
                        "subtype": "fake_child",
                        "pid": c.id(),
                        "session_id": session_id,
                        "uuid": uuid()
                    })),
                    Err(e) => emit(&serde_json::json!({
                        "type": "system",
                        "subtype": "fake_child_failed",
                        "error": e.to_string(),
                        "session_id": session_id,
                        "uuid": uuid()
                    })),
                }
            }
            for i in 0..60 {
                emit(&assistant_event(&session_id, &format!("still working {i}")));
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            write_result_json(&task_id, "slow scenario finished", true);
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "research_ok" | "research_unsourced" | "research_no_sources" => {
            emit(&rate_limit_event(&session_id, 0.07));
            emit(&assistant_event(&session_id, "I will do the thing."));
            let mut claims =
                vec![serde_json::json!({"kind": "completion", "text": "fake worker finished", "evidence": []})];
            claims.extend(research_claims(&scenario));
            write_claims_result(&task_id, "fake worker completed the task", claims);
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "bad_result" => {
            emit(&assistant_event(&session_id, "I will do the thing."));
            // same as ok, except result.json has no task_id field
            write_result_json(&task_id, "fake worker completed the task", false);
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        // MODELS-1: the recorded shape of a Pro account asked for Fable
        // (claude-b, run 01M51A3MKH77BG4D65NW8XCE9M): init, then one error
        // result with the structured names, exit 1, no result.json.
        "credits_required" => {
            let mut result = result_event(
                &session_id,
                true,
                "Fable 5.1 requires usage credits. Switch to another model, or manage usage credits at https://claude.ai/settings/usage?from=cc_cli_limit_message, to continue.",
                serde_json::json!(429),
            );
            result["api_error_code"] = serde_json::json!("credits_required");
            result["api_error"] = serde_json::json!("model_requires_usage_credits");
            emit(&result);
            std::process::exit(1);
        }
        "quota" => {
            emit(&rate_limit_event(&session_id, 0.99));
            emit(&serde_json::json!({
                "type": "system",
                "subtype": "api_retry",
                "attempt": 1,
                "max_retries": 10,
                "retry_delay_ms": 2000,
                "error_status": 429,
                "error": "rate_limit",
                "session_id": session_id,
                "uuid": uuid()
            }));
            emit(&result_event(
                &session_id,
                true,
                "Request rejected (429): Claude AI usage limit reached; your five-hour rate limit will reset later.",
                serde_json::json!(429),
            ));
            std::process::exit(1);
        }
        // FC-1: the session was rejected, and from now on `auth status` says
        // logged out. Synthetic text, not a recording.
        "auth_expired" => {
            write_marker("FAKE_LOGOUT_MARKER");
            emit(&result_event(
                &session_id,
                true,
                "fake-claude: the session token was rejected (synthetic text, not a recording)",
                serde_json::Value::Null,
            ));
            std::process::exit(1);
        }
        // FC-1: a plain failure after which `auth status` itself breaks.
        "auth_check_breaks" => {
            write_marker("FAKE_AUTH_FAIL_MARKER");
            emit(&result_event(
                &session_id,
                true,
                "fake-claude: generic failure (synthetic)",
                serde_json::Value::Null,
            ));
            std::process::exit(1);
        }
        // FC-1: a busy service. Synthesised in the shape of brief section
        // 2.5's `overloaded` pattern; not a recording.
        "overloaded" => {
            emit(&result_event(
                &session_id,
                true,
                r#"API Error: 529 {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                serde_json::json!(529),
            ));
            std::process::exit(1);
        }
        // FC-1: the `result` text of the recorded Claude Code 2.1.283 failure
        // in tests/fixtures/stream-oauth-refresh-failed.jsonl, verbatim.
        "oauth_refresh_failed" => {
            emit(&result_event(
                &session_id,
                true,
                "Failed to refresh OAuth token: another Claude Code process is refreshing it or exited mid-refresh. This is usually transient; retry in a minute, and if it persists close other Claude Code processes or sign in again",
                serde_json::Value::Null,
            ));
            std::process::exit(1);
        }
        "crash" => {
            // no result event at all: exactly the shape the runner must call `crash`
            emit(&assistant_event(&session_id, "starting work"));
            std::process::exit(137);
        }
        "resume" => {
            if resume.is_none() {
                emit(&serde_json::json!({
                    "type": "system",
                    "subtype": "fake_resume_required",
                    "session_id": session_id,
                    "uuid": uuid()
                }));
                std::process::exit(2);
            }
            emit(&assistant_event(&session_id, "picking up where I left off"));
            write_result_json(&task_id, "resumed", true);
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "hooks" | "hooks_bad_ref" => {
            let Some((program, hook_args)) = hook_command_from_settings(&args) else {
                emit(&serde_json::json!({
                    "type": "system", "subtype": "fake_no_settings",
                    "session_id": session_id, "uuid": uuid()
                }));
                std::process::exit(65);
            };
            emit(&assistant_event(&session_id, "running three shell commands"));

            // 1: success with small output
            call_hook(&program, &hook_args, &hook_pre(&session_id, &cwd, "toolu_fake_1", "echo hello-stdout"));
            call_hook(
                &program,
                &hook_args,
                &hook_post(&session_id, &cwd, "toolu_fake_1", "echo hello-stdout", "hello-stdout", None),
            );

            // 2: failure, exit code only readable out of the error string
            call_hook(&program, &hook_args, &hook_pre(&session_id, &cwd, "toolu_fake_2", "sh -c 'exit 3'"));
            call_hook(
                &program,
                &hook_args,
                &hook_fail(&session_id, &cwd, "toolu_fake_2", "sh -c 'exit 3'", "Exit code 3\noops-stderr"),
            );

            // Fix round 1 / I1 test support: interrupt right here, as if a
            // quota error hit mid-task after two commands already ran. Lets
            // an integration test exercise "resume after two captured
            // commands" without a third command's evidence in the way.
            if std::env::var("FAKE_HOOKS_THEN_QUOTA").as_deref() == Ok("1") {
                emit(&rate_limit_event(&session_id, 0.99));
                emit(&result_event(
                    &session_id,
                    true,
                    "Request rejected (429): Claude AI usage limit reached; your five-hour rate limit will reset later.",
                    serde_json::json!(429),
                ));
                std::process::exit(1);
            }

            // 3: output too large for the inline payload, so a persisted file
            let persisted = write_persisted_output(&session_id);
            call_hook(&program, &hook_args, &hook_pre(&session_id, &cwd, "toolu_fake_3", "seq 1 20000"));
            call_hook(
                &program,
                &hook_args,
                &hook_post(
                    &session_id,
                    &cwd,
                    "toolu_fake_3",
                    "seq 1 20000",
                    "1\n2\n3\n",
                    persisted.as_ref().map(|(p, n)| (p.as_str(), *n)),
                ),
            );

            // Real work, committed, deliberately touching a test file so the
            // diff evidence has something for `touches_tests` to find.
            let _ = std::fs::create_dir_all("src");
            let _ = std::fs::create_dir_all("tests");
            let _ = std::fs::write(
                std::path::Path::new("src").join("added.rs"),
                "pub fn added() -> u8 { 7 }\n",
            );
            let _ = std::fs::write(
                std::path::Path::new("tests").join("added_test.rs"),
                "#[test]\nfn added_is_seven() { assert_eq!(super_seven(), 7); }\nfn super_seven() -> u8 { 7 }\n",
            );
            // Test switch: `FAKE_COMMIT_NON_UTF8=1` also commits a file whose
            // bytes are not valid UTF-8 (cp950), for the raw-diff-blob test.
            if std::env::var("FAKE_COMMIT_NON_UTF8").as_deref() == Ok("1") {
                let _ = std::fs::write(
                    std::path::Path::new("src").join("legacy.txt"),
                    b"line \xa4\xa4\xa4\xe5 end\n",
                );
            }
            // `add src tests` rather than `add -A`, so `.agent/` (which the
            // control plane writes into) never enters the commit.
            git_quiet(&["add", "src", "tests"]);
            // Test switch: `FAKE_WRITE_FILE=<path>` also writes and commits
            // that file (created, or overwritten when it already exists), for
            // the protected-file tests.
            if let Ok(extra) = std::env::var("FAKE_WRITE_FILE") {
                let path = std::path::Path::new(&extra);
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, "written by the fake worker\n");
                git_quiet(&["add", "--", &extra]);
            }
            // Test switch: `FAKE_EDIT_PLAN=<file>` also applies that plan and
            // commits it (LEAN-1's protected-change fixtures).
            if let Ok(plan) = std::env::var("FAKE_EDIT_PLAN") {
                apply_edit_plan(std::path::Path::new(&plan));
            }
            git_quiet(&[
                "-c", "user.email=fake@example.com",
                "-c", "user.name=fake worker",
                "commit", "-m", "phase2 fake worker commit",
            ]);

            let codes = wait_for_index(3, std::time::Duration::from_secs(30));
            // brief §11: `hooks_bad_ref` cites a code the control plane never
            // allocated, which is exactly v6 §2.5 rule 3.
            let cited: Vec<String> = if scenario == "hooks_bad_ref" {
                vec!["E999".to_string()]
            } else {
                codes.first().cloned().into_iter().collect()
            };
            write_result_with_evidence(
                &task_id,
                "fake worker completed the task with captured evidence",
                &cited,
            );
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "source_check" | "source_check_bad" => {
            let last = if scenario == "source_check_bad" { "{not json".to_string() }
                else { source_check::document(&prompt).to_string() };
            source_check::wait_gate();
            std::fs::create_dir_all(".agent").unwrap();
            std::fs::write(".agent/review.json", last).unwrap();
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "review_pass" | "review_reject" | "review_bad" | "review_uncited" => {
            emit(&assistant_event(&session_id, "reviewing the candidate"));
            let agent_dir = std::path::Path::new(".agent");
            let _ = std::fs::create_dir_all(agent_dir);

            if scenario == "review_bad" {
                let _ = std::fs::write(agent_dir.join("review.json"), "{not json");
                emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
                std::process::exit(0);
            }

            // A real reviewer reads its criteria out of the prompt and its codes
            // out of the index the control plane published into this worktree.
            let mut ids = criterion_ids_in(&prompt);
            if ids.is_empty() {
                ids.push("AC1".to_string());
            }
            let codes = wait_for_index(1, std::time::Duration::from_secs(10));
            // Whole-branch review Important 2: `review_uncited` is the
            // reviewer that never opened anything - every criterion `met`,
            // nothing cited. The control plane must refuse it rather than let
            // it settle the task `pass`.
            let cite: Vec<String> = if scenario == "review_uncited" {
                Vec::new()
            } else {
                codes.first().cloned().into_iter().collect()
            };

            // ...and it opens the evidence it is about to cite, which is what
            // makes `review show`'s unopened_citation check meaningful.
            if let (Ok(exe), Some(code)) = (std::env::var("THOTH_EXE"), cite.first()) {
                let _ = std::process::Command::new(exe)
                    .args(["evidence", "show", code, "--json"])
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }

            let rejecting = scenario == "review_reject";
            let category = first_category_in(&prompt);
            let criteria: Vec<serde_json::Value> = ids
                .iter()
                .enumerate()
                .map(|(i, id)| {
                    let unmet = rejecting && i == 0;
                    let mut c = serde_json::json!({
                        "id": id,
                        "verdict": if unmet { "unmet" } else { "met" },
                        "evidence": cite,
                    });
                    if unmet {
                        // BRIEF-1 switch: `FAKE_REVIEW_NOTE` replaces the note.
                        c["note"] = serde_json::json!(std::env::var("FAKE_REVIEW_NOTE").unwrap_or_else(|_| {
                            "no captured evidence shows this criterion being exercised".to_string()
                        }));
                        label_finding(&mut c, category.as_deref());
                    }
                    c
                })
                .collect();
            // Whole-branch review Important 1 test support: hold the review
            // here until the test says go, so "the task moved while a review
            // was in flight" can be driven deterministically instead of raced.
            // The run stays `running` for as long as the gate file is absent.
            if let Ok(gate) = std::env::var("FAKE_REVIEW_GATE") {
                let gate = std::path::PathBuf::from(gate);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
                while !gate.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }

            let doc = serde_json::json!({
                "verdict": if rejecting { "REJECT" } else { "PASS" },
                "findings": {
                    "criteria": criteria,
                    "extra_findings": if rejecting {
                        let mut extra = serde_json::json!({
                            "severity": "minor",
                            "note": "the new test asserts a constant rather than the behaviour",
                            "evidence": cite
                        });
                        label_finding(&mut extra, category.as_deref());
                        serde_json::json!([extra])
                    } else {
                        serde_json::json!([])
                    }
                }
            });
            let _ = std::fs::write(
                agent_dir.join("review.json"),
                serde_json::to_string_pretty(&doc).unwrap(),
            );
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "review_fix" => {
            emit(&assistant_event(&session_id, "reviewing the candidate and attaching a fix"));
            let plan = review_fix::plan();
            review_fix::side_effects(&plan);
            source_check::wait_gate();
            let _ = std::fs::create_dir_all(".agent");
            let _ = std::fs::write(".agent/review.json", review_fix::document(&plan).to_string());
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "fix_confirm_approve" | "fix_confirm_revise" | "fix_confirm_bad" => {
            emit(&assistant_event(&session_id, "checking the fix"));
            let Some(edits) = review_fix::read_edits() else {
                eprintln!("fake-claude: .agent/review-fix-input is missing or malformed");
                std::process::exit(70);
            };
            review_fix::side_effects(&review_fix::plan());
            source_check::wait_gate();
            let text = if scenario == "fix_confirm_bad" {
                "{not json".to_string()
            } else {
                review_fix::confirm_override()
                    .unwrap_or_else(|| review_fix::confirm_document(&edits, scenario == "fix_confirm_revise"))
                    .to_string()
            };
            let _ = std::fs::create_dir_all(".agent");
            let _ = std::fs::write(".agent/review.json", text);
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "spec_review_approve" | "spec_review_revise" | "spec_review_bad" => {
            emit(&assistant_event(&session_id, "reviewing the spec"));
            // A real spec reviewer reads the spec the runner wrote for it.
            if !is_spec_file(std::path::Path::new(SPEC_REVIEW_INPUT)) {
                eprintln!("fake-claude: .agent/spec-review-input/spec.json is missing or not a spec");
                std::process::exit(70);
            }
            let agent_dir = std::path::Path::new(".agent");
            let _ = std::fs::create_dir_all(agent_dir);
            if scenario == "spec_review_bad" {
                let _ = std::fs::write(agent_dir.join("spec-review.json"), "{not json");
                emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
                std::process::exit(0);
            }
            let mut ids = criterion_ids_in(&prompt);
            if ids.is_empty() {
                ids.push("AC1".to_string());
            }
            let doc = spec_review_doc(&ids, scenario == "spec_review_revise");
            // Same gate as the review scenarios: the run stays `running`
            // until the test creates the file.
            if let Ok(gate) = std::env::var("FAKE_REVIEW_GATE") {
                let gate = std::path::PathBuf::from(gate);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
                while !gate.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            let _ = std::fs::write(
                agent_dir.join("spec-review.json"),
                serde_json::to_string_pretty(&doc).unwrap(),
            );
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "spec_review_edit" | "spec_review_edit_bad" | "spec_review_edit_big" => {
            emit(&assistant_event(&session_id, "reviewing the spec and proposing edits"));
            let spec_text = std::fs::read_to_string(SPEC_REVIEW_INPUT).unwrap_or_default();
            let spec: Option<serde_json::Value> = serde_json::from_str(&spec_text).ok();
            let Some(spec) = spec.filter(|v| v["acceptance_criteria"].is_array()) else {
                eprintln!("fake-claude: .agent/spec-review-input/spec.json is missing or not a spec");
                std::process::exit(70);
            };
            let agent_dir = std::path::Path::new(".agent");
            let _ = std::fs::create_dir_all(agent_dir);
            let mut ids = criterion_ids_in(&prompt);
            if ids.is_empty() {
                ids.push("AC1".to_string());
            }
            let doc = spec_edit_doc(&spec, &ids, &scenario);
            if let Ok(gate) = std::env::var("FAKE_REVIEW_GATE") {
                let gate = std::path::PathBuf::from(gate);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
                while !gate.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            let _ = std::fs::write(
                agent_dir.join("spec-review.json"),
                serde_json::to_string_pretty(&doc).unwrap(),
            );
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        "spec_confirm_approve" | "spec_confirm_revise" => {
            emit(&assistant_event(&session_id, "checking the reviewer's edits"));
            // A real author reads all three files the runner wrote.
            let input = std::path::Path::new(".agent").join("spec-review-input");
            let edits: Option<Vec<serde_json::Value>> = std::fs::read_to_string(input.join("edits.json"))
                .ok()
                .and_then(|t| serde_json::from_str(&t).ok());
            let proposed_ok = is_spec_file(&input.join("proposed.json"));
            let (Some(edits), true) = (edits, proposed_ok) else {
                eprintln!("fake-claude: .agent/spec-review-input/edits.json or proposed.json is missing or malformed");
                std::process::exit(70);
            };
            let agent_dir = std::path::Path::new(".agent");
            let _ = std::fs::create_dir_all(agent_dir);
            let doc = spec_confirm_doc(&edits, scenario == "spec_confirm_revise");
            if let Ok(gate) = std::env::var("FAKE_REVIEW_GATE") {
                let gate = std::path::PathBuf::from(gate);
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
                while !gate.exists() && std::time::Instant::now() < deadline {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            let _ = std::fs::write(
                agent_dir.join("spec-review.json"),
                serde_json::to_string_pretty(&doc).unwrap(),
            );
            emit(&result_event(&session_id, false, "DONE", serde_json::Value::Null));
            std::process::exit(0);
        }
        other => {
            emit(&serde_json::json!({
                "type": "system",
                "subtype": "fake_unknown_scenario",
                "scenario": other,
                "session_id": session_id,
                "uuid": uuid()
            }));
            std::process::exit(64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn criterion_ids_ignore_ac_tokens_outside_the_criteria_line() {
        let prompt = "{\"id\":\"AC9\"}\nEvery id in \"criteria\" must be one of: AC1, AC2. Judge each exactly once.\nThis review is for task 01M3AC7ZZZZZZZZZZZZZZZZZZZ.\n";
        let ids = criterion_ids_in(prompt);
        assert_eq!(ids, vec!["AC1".to_string(), "AC2".to_string()]);
        assert!(!ids.iter().any(|i| i == "AC7"));
    }
}
