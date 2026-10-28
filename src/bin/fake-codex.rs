//! Test double for `codex exec --json`, `codex login status` and
//! `codex app-server --listen stdio://`. Never used in
//! production: `THOTH_CODEX_BIN` points here only from tests. Every event
//! it prints copies a shape recorded from codex-cli 0.154 in
//! `tests/fixtures/codex/` (docs/post-phase2/codex-worker-scope-2026-09-27.md
//! §1); no invented field names. The usage-limit error is the one exception:
//! it could not be triggered for real, so it is the E6 error shape carrying
//! the strings found in the codex binary, and it is marked as such below.
//!
//! Test switches, all unset by default:
//! - `FAKE_SCENARIO`: `ok` | `research_ok` | `research_unsourced` |
//!   `research_no_sources` | `error` | `quota` | `resume` | `crash` | `slow` |
//!   `commits_anyway` | `no_changes` | `review_pass` | `review_reject` |
//!   `review_bad` | `review_uncited` | `auth_expired` (FC-1: writes
//!   `FAKE_LOGOUT_MARKER`'s file, then fails with synthetic text) |
//!   `disconnected` (FC-1: one command, then fails with the synthetic
//!   `stream disconnected before completion` text H5 names) |
//!   `spec_review_approve` | `spec_review_revise` | `spec_review_bad` (SR-2:
//!   need `--output-schema` and `-o`, read
//!   `.agent/spec-review-input/spec.json`, take the ids from the prompt's
//!   `Every id in "criteria" must be one of:` line, and answer with the spec
//!   review document as the final message; `bad`: `{not json`) |
//!   `spec_review_edit` (SR-3: the same, answering a REVISE with every
//!   criterion ok and three edits built from the spec's text: replace the
//!   first acceptance criterion keeping its id, append a constraint, delete
//!   the last non_goal, the last one only when there is a non_goal) |
//!   `spec_confirm_approve` (SR-3: reads `.agent/spec-review-input/edits.json`
//!   and `proposed.json`, answers every index ok as the final message)
//! - `FAKE_CODEX_SCENARIO`: replaces `FAKE_SCENARIO` when set
//! - `FAKE_TASK_ID`: the task id written into result.json
//! - `FAKE_CODEX_ARGS_LOG`: append one JSON line per invocation with argv,
//!   cwd and `CODEX_HOME`, so a test can pin what thoth launched
//! - `FAKE_CODEX_LOGGED_OUT=1`: `login status` answers logged out
//! - `FAKE_LOGOUT_MARKER=<path>` (FC-1): once that file exists, `login
//!   status` prints `Not logged in` on stderr and exits 1
//! - `FAKE_CODEX_EXTRA_ITEMS=1`: the `ok` run also shows a collab (subagent)
//!   item and an MCP tool call, which a locked-down worker should never have
//! - `FAKE_CODEX_WEB_SEARCH=1` (RES-1a): the `ok` and research runs also show
//!   one `web_search` item (synthetic: only `id` and `type`)
//! - RES-1a scenarios `research_ok`, `research_unsourced` and
//!   `research_no_sources`: the `ok` run with its summary, but the claims are
//!   the completion claim plus, for `research_ok`, `research claim 1` (with
//!   the first item of `FAKE_SOURCES`, a JSON array of `{"url", "quote"}`)
//!   and `research claim 2` (the rest; unset, each gets
//!   `https://example.invalid/fake` with quote `fake quote`); for
//!   `research_no_sources`, one research claim with `sources: []`; for
//!   `research_unsourced`, nothing more. No URL is ever opened
//! - `FAKE_CODEX_LAUNCHER=<symlink named node>` (Unix): re-exec through it
//!   first, the way the npm launcher's pid turns from `codex` into `node`
//! - `FAKE_CODEX_ROLLOUT=1`: before the `ok` run ends, write a rollout file
//!   named after the thread under `$CODEX_HOME/sessions/2026/01/01/` with two
//!   `token_count` lines, five-hour `used_percent` 10 then 13, same
//!   `resets_at`; `FAKE_CODEX_ROLLOUT=reset`: the same two lines, but the
//!   second has a later `resets_at` (the window reset mid-run)
//! - `FAKE_ATTEMPTS_FILE=<path>` (FC-2): every `exec` launch appends
//!   `resume <thread>` (`exec ... resume <thread>`) or `fresh <thread>` to
//!   that file before it picks its scenario, and counts the file's lines:
//!   that is launch k (from 1). `login`, `app-server` and `--version` do not
//! - `FAKE_FIRST_SCENARIO=<name>` (FC-2): the scenario of launches 1 to
//!   `FAKE_FIRST_TIMES` (default 1, unparsable is 1); later ones use the
//!   usual scenario. Without `FAKE_ATTEMPTS_FILE`, k is always 1, so every
//!   launch runs `FAKE_FIRST_SCENARIO`
//!
//! `--version` prints `codex-cli 0.0.0-fake` (thoth doctor); no switch.
//!
//! `app-server` (first argument; `thoth profile usage --refresh`) reads JSON
//! lines on stdin and answers from `tests/fixtures/codex/app-server-rate-limits.jsonl`:
//! `initialize` gets the recorded id 0 reply, then the recorded
//! `remoteControl/status/changed` notification; `account/rateLimits/read`
//! gets the recorded rate-limits reply with the request's id; stdin closing
//! exits 0. `FAKE_CODEX_APP_SERVER` changes the rate-limits answer: `ok`
//! (default); `error` (the signed-out error, code -32600); `hang` (no answer,
//! stdin is no longer read: writes `FAKE_CODEX_HANG_STARTED` if set when the
//! request arrives, sleeps `FAKE_CODEX_HANG_SECS` seconds from the request (a
//! positive integer; unset or anything else is 6), writes
//! `FAKE_CODEX_HANG_MARKER` if set, then exits); `denied` (the recorded
//! reply with `ordinaryUsageAllowed` false); `denied_empty` (the same, and
//! `rateLimits.primary`/`secondary` and `rateLimitsByLimitId` null); `exit`
//! (exit 3 right after the initialize reply).
//!
//! TIER-1 switch, unset by default: `FAKE_CODEX_PLAN_TYPE` changes the
//! `planType` of the rate-limits reply (the recorded one says `plus`) on
//! `result.rateLimits` and on every object under `result.rateLimitsByLimitId`
//! (a non-object entry is skipped): `none` removes it, any other value
//! replaces it. `error`, `hang` and `exit` are not affected.
//!
//! `thoth profile login` switches, unset by default: `codex login` is the
//! interactive login and writes `fake-login.ok` in `CODEX_HOME`;
//! `FAKE_LOGIN_FAIL=1` makes it exit 1 without writing; `FAKE_LOGIN_SLEEP_MS`
//! sleeps that long first; `FAKE_LOGIN_BY_FILE=1` makes `login status` also
//! need that file (`FAKE_CODEX_LOGGED_OUT=1` still wins).
//!
//! W3: the prompt is the first positional argument after `exec` (skipping
//! options, their values and `resume <thread>`); when it is `-` or missing,
//! stdin is read to EOF instead, as codex does. `FAKE_PROMPT_COPY=<path>`
//! writes the prompt received, byte for byte, to that file (overwriting).
//! Unset by default.
//!
//! RD-2 switch, unset by default: `FAKE_ENV_LOG=<path>` makes every `exec`
//! launch (where `FAKE_PROMPT_COPY` is written; not `login`, `app-server` or
//! `--version`) append one JSON line `{"names":[...]}`: the sorted names of
//! its environment variables, never a value.
//!
//! `exec ... resume <thread> ...` refuses `-C`, `--add-dir` and `-s` after
//! `resume`, exactly as codex-cli 0.154 does.
//!
//! Reviewer scenarios (`review_pass` | `review_reject` | `review_bad` |
//! `review_uncited`) need `--output-schema <file>` and `-o <file>`, the way
//! thoth launches a codex reviewer (scope §2.4, E3b). The fake reads the
//! input thoth put in `.agent/review-input/` (with a `cat` item on the
//! stream, as the real reviewer would), makes its final message the review
//! document, and writes that message to the `-o` file itself - as the codex
//! CLI does, outside the sandbox. A schema the OpenAI API would refuse for
//! structured output (an object without `additionalProperties: false` or
//! with a property missing from `required`) fails the turn with a 400.

// Source-check scenarios: `source_check` checks file:// pages from the prompt;
// `source_check_bad` returns {not json. Both require --output-schema and -o.
// FAKE_SOURCE_CHECK_VERDICT overrides the computed verdict;
// FAKE_REVIEW_GATE holds output until its file exists.
#[path = "support/source_check.rs"]
mod source_check;

use std::io::Write;

/// The file `codex login` writes; Thoth itself never knows it.
const LOGIN_MARKER: &str = "fake-login.ok";

/// FC-2: appends `resume <thread>` or `fresh <thread>` to
/// `FAKE_ATTEMPTS_FILE` and returns how many lines it now holds (this
/// launch's number, from 1). Unset: writes nothing, and every launch is the
/// first.
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

fn emit(v: &serde_json::Value) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// RD-2: appends `{"names":[...]}` to `FAKE_ENV_LOG`, the sorted names of
/// this `exec` launch's environment variables. Names only, never a value.
/// Unset: writes nothing.
fn log_env_names() {
    let Some(path) = std::env::var_os("FAKE_ENV_LOG") else { return };
    let mut names: Vec<String> =
        std::env::vars_os().map(|(k, _)| k.to_string_lossy().into_owned()).collect();
    names.sort();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", serde_json::json!({"names": names}));
    }
}

fn log_args(args: &[String]) {
    let Ok(path) = std::env::var("FAKE_CODEX_ARGS_LOG") else {
        return;
    };
    let line = serde_json::json!({
        "args": args,
        "cwd": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        "codex_home": std::env::var("CODEX_HOME").ok(),
    });
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// `codex exec` options that take a value, so their value is not mistaken for
/// the `resume` subcommand or its thread id.
const VALUE_OPTIONS: &[&str] = &[
    "-m", "--model", "-c", "--config", "-s", "--sandbox", "--disable", "--enable",
    "--add-dir", "-C", "--cd", "-p", "--profile", "-o", "--output-last-message",
    "--output-schema", "-i", "--image",
];

/// Options `codex exec resume` rejects as unexpected arguments (measured on
/// codex-cli 0.154, 2026-09-27): they belong before `resume`.
const REJECTED_AFTER_RESUME: &[&str] = &["-C", "--cd", "--add-dir", "-s", "--sandbox"];

/// The arguments after `exec`: `Ok(Some(thread))` for `exec ... resume
/// <thread> ...`, `Ok(None)` for a fresh run, `Err` for what the real codex
/// refuses.
fn parse_exec(args: &[String]) -> Result<Option<String>, String> {
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if VALUE_OPTIONS.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') {
            i += 1;
            continue;
        }
        if a != "resume" {
            return Ok(None); // the prompt of a fresh run
        }
        let thread = args
            .get(i + 1)
            .cloned()
            .ok_or_else(|| "`exec resume` needs a thread id".to_string())?;
        if let Some(bad) = args[i + 2..]
            .iter()
            .find(|x| REJECTED_AFTER_RESUME.contains(&x.as_str()))
        {
            return Err(format!("unexpected argument '{bad}' found"));
        }
        return Ok(Some(thread));
    }
    Ok(None)
}

/// The prompt of an `exec` launch (`args` after `exec`): the first
/// positional argument - skipping options (except `-` itself), the values of
/// `VALUE_OPTIONS` and `resume <thread>` - or stdin to EOF when that is `-` or
/// there is none.
fn exec_prompt(args: &[String]) -> String {
    let mut positional = None;
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        if VALUE_OPTIONS.contains(&a) {
            i += 2;
            continue;
        }
        if a.starts_with('-') && a != "-" {
            i += 1;
            continue;
        }
        if a == "resume" {
            i += 2;
            continue;
        }
        positional = Some(a.to_string());
        break;
    }
    match positional {
        Some(p) if p != "-" => p,
        _ => {
            let mut buf = String::new();
            let _ = std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf);
            buf
        }
    }
}

fn thread_started(thread_id: &str) -> serde_json::Value {
    serde_json::json!({"type": "thread.started", "thread_id": thread_id})
}

fn turn_started() -> serde_json::Value {
    serde_json::json!({"type": "turn.started"})
}

fn agent_message(id: usize, text: &str) -> serde_json::Value {
    serde_json::json!({"type": "item.completed", "item": {
        "id": format!("item_{id}"), "type": "agent_message", "text": text}})
}

/// `item.started` then `item.completed` for one shell command, exactly as
/// codex prints them: `/bin/bash -lc '<cmd>'`, the output folded into
/// `aggregated_output`, the real exit code, and `failed` for a non-zero exit.
fn command(id: usize, cmd: &str, output: &str, exit_code: i64) {
    let command = format!("/bin/bash -lc '{cmd}'");
    emit(&serde_json::json!({"type": "item.started", "item": {
        "id": format!("item_{id}"), "type": "command_execution", "command": command,
        "aggregated_output": "", "exit_code": null, "status": "in_progress"}}));
    emit(&serde_json::json!({"type": "item.completed", "item": {
        "id": format!("item_{id}"), "type": "command_execution", "command": command,
        "aggregated_output": output, "exit_code": exit_code,
        "status": if exit_code == 0 { "completed" } else { "failed" }}}));
}

fn file_change(id: usize, path: &str) {
    for (ty, status) in [("item.started", "in_progress"), ("item.completed", "completed")] {
        emit(&serde_json::json!({"type": ty, "item": {
            "id": format!("item_{id}"), "type": "file_change",
            "changes": [{"path": path, "kind": "add"}], "status": status}}));
    }
}

fn turn_completed(output_tokens: i64) -> serde_json::Value {
    serde_json::json!({"type": "turn.completed", "usage": {
        "input_tokens": 22534, "cached_input_tokens": 11008, "cache_write_input_tokens": 0,
        "output_tokens": output_tokens, "reasoning_output_tokens": 22}})
}

/// E6: a top-level `error` whose message is itself a JSON string, then
/// `turn.failed` carrying the same message, then exit 1.
fn fail_turn(inner: &serde_json::Value) -> ! {
    let message = inner.to_string();
    emit(&serde_json::json!({"type": "error", "message": message}));
    emit(&serde_json::json!({"type": "turn.failed", "error": {"message": message}}));
    std::process::exit(1);
}

/// FC-1: `fail_turn`'s shape with a plain-text message.
fn fail_turn_text(message: &str) -> ! {
    emit(&serde_json::json!({"type": "error", "message": message}));
    emit(&serde_json::json!({"type": "turn.failed", "error": {"message": message}}));
    std::process::exit(1);
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

/// Polls the control-plane-generated index until it holds at least `min` rows,
/// then returns their codes - what the worker prompt tells a real worker to
/// read before it writes result.json.
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

fn write_result(task_id: &str, summary: &str, codes: &[String]) {
    let dir = std::path::Path::new(".agent");
    let _ = std::fs::create_dir_all(dir);
    let body = serde_json::json!({
        "task_id": task_id,
        "status": "completed",
        "summary": summary,
        "claims": [
            {"kind": "test_result", "text": "fake suite green", "evidence": codes},
            {"kind": "completion", "text": "all acceptance criteria met", "evidence": []}
        ],
        "verification_requests": [{"cmd": "echo verify", "cwd": "."}]
    });
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
/// claim, as in fake-claude. Opens no URL.
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

/// Writes one file and reports the `file_change` item codex would print.
/// The real codex cannot commit inside a linked worktree (scope §2.6, E9), so
/// the normal scenarios below leave the file uncommitted; thoth commits on
/// its behalf.
fn add_file(id: usize, path: &str, body: &str) {
    let _ = std::fs::write(path, body);
    file_change(id, path);
}

/// `FAKE_CODEX_ROLLOUT`: the rollout file codex keeps for a thread, reduced
/// to its `token_count` lines (shape as in codex-cli 0.154: the general
/// bucket's `limit_id` "codex", a 300-minute `primary` and a weekly
/// `secondary`). Unset or any other value: nothing is written.
fn write_rollout(thread_id: &str) {
    const RESETS_AT: i64 = 1_767_243_600; // 2026-01-01T05:00:00Z
    let second_resets_at = match std::env::var("FAKE_CODEX_ROLLOUT").as_deref() {
        Ok("1") => RESETS_AT,
        Ok("reset") => RESETS_AT + 18_000,
        _ => return,
    };
    let Ok(home) = std::env::var("CODEX_HOME") else {
        return;
    };
    let dir = std::path::Path::new(&home)
        .join("sessions")
        .join("2026")
        .join("01")
        .join("01");
    let _ = std::fs::create_dir_all(&dir);
    let line = |ts: &str, used: f64, resets_at: i64| {
        serde_json::json!({"timestamp": ts, "type": "event_msg", "payload": {
            "type": "token_count",
            "info": null,
            "rate_limits": {
                "limit_id": "codex", "limit_name": null,
                "primary": {"used_percent": used, "window_minutes": 300, "resets_at": resets_at},
                "secondary": {"used_percent": 2.0, "window_minutes": 10080, "resets_at": RESETS_AT + 500_000}
            }}})
        .to_string()
    };
    let text = format!(
        "{}\n{}\n",
        line("2026-01-01T00:00:01.000Z", 10.0, RESETS_AT),
        line("2026-01-01T00:00:02.000Z", 13.0, second_resets_at)
    );
    let _ = std::fs::write(
        dir.join(format!("rollout-2026-01-01T00-00-00-{thread_id}.jsonl")),
        text,
    );
}

/// `FAKE_SCENARIO=commits_anyway`: a worker that bypasses the sandbox and
/// commits its own change, exercising the case where thoth's own commit
/// step finds nothing left to commit (AC3's third case).
fn commit_file(id: usize, name: &str, body: &str) {
    let _ = std::fs::write(name, body);
    file_change(id, name);
    git_quiet(&["add", name]);
    git_quiet(&[
        "-c", "user.email=fake@example.com",
        "-c", "user.name=fake codex",
        "commit", "-m", "fake codex commit",
    ]);
    command(id + 1, &format!("git add {name} && git commit -m fake-codex-commit"),
        "[wt 960ce36] fake-codex-commit\n 1 file changed, 1 insertion(+)\n", 0);
}

/// `FAKE_CODEX_LAUNCHER=<path>`: like the npm `codex` (`#!/usr/bin/env
/// node`), whose pid first shows the image `codex` and then `node`, re-exec
/// this binary through `<path>` (a symlink named `node`), keeping the pid.
#[cfg(unix)]
fn relaunch_like_the_npm_launcher(args: &[String]) {
    use std::os::unix::process::CommandExt;
    let Ok(launcher) = std::env::var("FAKE_CODEX_LAUNCHER") else {
        return;
    };
    if std::env::var_os("FAKE_CODEX_LAUNCHED").is_some() {
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let err = std::process::Command::new(launcher)
        .args(args)
        .env("FAKE_CODEX_LAUNCHED", "1")
        .exec();
    eprintln!("fake-codex: relaunch failed: {err}");
    std::process::exit(70);
}

#[cfg(not(unix))]
fn relaunch_like_the_npm_launcher(_args: &[String]) {}

/// What the API's strict structured output demands of every object in the
/// schema: `additionalProperties: false`, and every property listed in
/// `required`. Returns the first violation found.
fn strict_schema_violation(v: &serde_json::Value, at: &str) -> Option<String> {
    if v["type"] == "object" {
        if v["additionalProperties"] != serde_json::Value::Bool(false) {
            return Some(format!("{at}: additionalProperties must be false"));
        }
        let required: Vec<&str> = v["required"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
            .unwrap_or_default();
        for (key, sub) in v["properties"].as_object().into_iter().flatten() {
            if !required.contains(&key.as_str()) {
                return Some(format!("{at}: '{key}' is missing from required"));
            }
            if let Some(bad) = strict_schema_violation(sub, &format!("{at}.{key}")) {
                return Some(bad);
            }
        }
    }
    if let Some(items) = v.get("items") {
        return strict_schema_violation(items, &format!("{at}[]"));
    }
    None
}

/// The acceptance criterion ids from the packet's spec section: the text
/// before the first `:` of each `- ` line under "Acceptance criteria:".
fn criterion_ids(packet: &str) -> Vec<String> {
    packet
        .lines()
        .skip_while(|l| l.trim() != "Acceptance criteria:")
        .skip(1)
        .take_while(|l| l.starts_with("- "))
        .filter_map(|l| l[2..].split(':').next().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty() && s != "(none)")
        .collect()
}

/// The reviewer half: `--output-schema` + `-o`, read the input files, answer
/// with the review document as the final message.
fn review(scenario: &str, args: &[String], prompt: &str, thread_id: &str) -> ! {
    let (Some(schema_path), Some(out_path)) = (arg_value(args, "--output-schema"), arg_value(args, "-o")) else {
        emit(&turn_started());
        fail_turn(&serde_json::json!({"type": "error", "status": 400, "error": {
            "type": "invalid_request_error",
            "message": "fake-codex: a reviewer run needs --output-schema and -o"}}));
    };
    let schema = std::fs::read_to_string(&schema_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let Some(schema) = schema else {
        eprintln!("Error: failed to read output schema file {schema_path}");
        std::process::exit(1);
    };
    emit(&turn_started());
    if let Some(bad) = strict_schema_violation(&schema, "schema") {
        fail_turn(&serde_json::json!({"type": "error", "status": 400, "error": {
            "type": "invalid_request_error",
            "message": format!("Invalid schema for response_format 'codex_output_schema': {bad}")}}));
    }

    if scenario.starts_with("source_check") {
        let last = if scenario == "source_check_bad" { "{not json".to_string() }
            else { source_check::document(prompt).to_string() };
        source_check::wait_gate();
        if std::env::var("FAKE_CODEX_WEB_SEARCH").as_deref() == Ok("1") {
            emit(&serde_json::json!({"type":"item.completed", "item":{
                "id":"source-search", "type":"web_search", "action":{"type":"open_page", "url":"file://offline"}}}));
        }
        emit(&agent_message(8, &last));
        std::fs::write(&out_path, &last).unwrap();
        write_rollout(thread_id);
        emit(&turn_completed(326));
        std::process::exit(0);
    }

    let input = std::path::Path::new(".agent").join("review-input");
    let Ok(packet) = std::fs::read_to_string(input.join("packet.md")) else {
        fail_turn(&serde_json::json!({"type": "error", "status": 500, "error": {
            "type": "fake_codex", "message": "fake-codex: .agent/review-input/packet.md is missing"}}));
    };
    command(1, "cat .agent/review-input/packet.md", &packet, 0);
    let mut codes: Vec<String> = std::fs::read_dir(input.join("evidence"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            e.file_name()
                .to_string_lossy()
                .strip_suffix(".txt")
                .map(str::to_string)
        })
        .collect();
    codes.sort();
    if let Some(first) = codes.first() {
        let body = std::fs::read_to_string(input.join("evidence").join(format!("{first}.txt")))
            .unwrap_or_default();
        command(2, &format!("cat .agent/review-input/evidence/{first}.txt"), &body, 0);
    }

    let mut ids = criterion_ids(&packet);
    if ids.is_empty() {
        ids.push("AC1".to_string());
    }
    let cite: Vec<String> = if scenario == "review_uncited" {
        Vec::new()
    } else {
        codes.first().cloned().into_iter().collect()
    };
    let rejecting = scenario == "review_reject";
    let category = first_category_in(prompt);
    // The strict schema makes `note` required, so a met criterion carries an
    // empty one - exactly what the real codex produces.
    let criteria: Vec<serde_json::Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| {
            let unmet = rejecting && i == 0;
            let mut c = serde_json::json!({
                "id": id,
                "verdict": if unmet { "unmet" } else { "met" },
                "evidence": cite,
                "note": if unmet { "no captured evidence shows this criterion being exercised" } else { "" },
            });
            // ...and so do `fix_size` and `category`, on a met one too.
            if rejecting {
                if unmet {
                    label_finding(&mut c, category.as_deref());
                } else {
                    c["fix_size"] = serde_json::json!("none");
                    c["category"] = serde_json::json!("none");
                }
            }
            c
        })
        .collect();
    let doc = serde_json::json!({
        "verdict": if rejecting { "REJECT" } else { "PASS" },
        "findings": {
            "criteria": criteria,
            "extra_findings": if rejecting {
                let mut extra = serde_json::json!({"severity": "minor",
                    "note": "the new test asserts a constant rather than the behaviour",
                    "evidence": cite});
                label_finding(&mut extra, category.as_deref());
                serde_json::json!([extra])
            } else {
                serde_json::json!([])
            },
        },
    });
    let last = if scenario == "review_bad" {
        "{not json".to_string()
    } else {
        doc.to_string()
    };
    emit(&agent_message(8, &last));
    // The codex CLI itself writes the final message to -o, outside the sandbox.
    if let Err(e) = std::fs::write(&out_path, &last) {
        eprintln!("fake-codex: writing {out_path} failed: {e}");
        std::process::exit(1);
    }
    // Like the implementer path: a rollout file for `FAKE_CODEX_ROLLOUT`, so
    // a reviewer run's `run_quota` has readings (DT-1).
    write_rollout(thread_id);
    emit(&turn_completed(326));
    std::process::exit(0);
}

/// The acceptance-criterion ids the runner listed for the spec reviewer: only
/// the line starting `Every id in "criteria" must be one of:` (copied from
/// fake-claude).
fn criterion_ids_in(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .find(|l| l.starts_with("Every id in \"criteria\" must be one of:"))
        .map(|l| tokens_in(l, "AC"))
        .unwrap_or_default()
}

/// The first name on the prompt's `Every "category" must be one of:` line
/// (copied from fake-claude).
fn first_category_in(prompt: &str) -> Option<String> {
    prompt
        .lines()
        .find_map(|l| l.strip_prefix("Every \"category\" must be one of: "))
        .and_then(|rest| rest.split([',', '.']).next())
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// `review_reject`'s labels on an unmet criterion or an extra finding
/// (copied from fake-claude).
fn label_finding(finding: &mut serde_json::Value, category: Option<&str>) {
    finding["fix_size"] = serde_json::json!("S");
    if let Some(category) = category {
        finding["category"] = serde_json::json!(category);
    }
}

/// Every `<prefix><digits>` token in `text`, first-seen order (copied from
/// fake-claude).
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

/// SR-3: the string entries of a spec's list field, `[]` when absent (copied
/// from fake-claude).
fn spec_list(spec: &serde_json::Value, key: &str) -> Vec<String> {
    spec[key]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
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

/// SR-3: the edit document of `spec_review_edit`: `criteria` as given, three
/// edits built from the spec's own text (copied from fake-claude).
fn spec_edit_doc(spec: &serde_json::Value, ids: &[String], criteria: Vec<serde_json::Value>) -> serde_json::Value {
    let criterion = ids.first().cloned().unwrap_or_else(|| "AC1".to_string());
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
    serde_json::json!({"verdict": "REVISE", "criteria": criteria, "edits": edits})
}

/// SR-2, the spec reviewer half: `--output-schema` + `-o`, read the spec the
/// runner wrote, answer with the spec review document as the final message.
/// SR-3: `spec_review_edit` and `spec_confirm_approve` too.
fn spec_review(scenario: &str, args: &[String], prompt: &str, thread_id: &str) -> ! {
    let (Some(schema_path), Some(out_path)) = (arg_value(args, "--output-schema"), arg_value(args, "-o")) else {
        emit(&turn_started());
        fail_turn(&serde_json::json!({"type": "error", "status": 400, "error": {
            "type": "invalid_request_error",
            "message": "fake-codex: a spec reviewer run needs --output-schema and -o"}}));
    };
    let schema = std::fs::read_to_string(&schema_path)
        .ok()
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let Some(schema) = schema else {
        eprintln!("Error: failed to read output schema file {schema_path}");
        std::process::exit(1);
    };
    emit(&turn_started());
    if let Some(bad) = strict_schema_violation(&schema, "schema") {
        fail_turn(&serde_json::json!({"type": "error", "status": 400, "error": {
            "type": "invalid_request_error",
            "message": format!("Invalid schema for response_format 'codex_output_schema': {bad}")}}));
    }
    let input_dir = std::path::Path::new(".agent").join("spec-review-input");
    // SR-3: the author's check reads the edits and the proposed spec, and
    // answers every index ok.
    if scenario == "spec_confirm_approve" {
        let edits_text = std::fs::read_to_string(input_dir.join("edits.json")).unwrap_or_default();
        let edits: Option<Vec<serde_json::Value>> = serde_json::from_str(&edits_text).ok();
        let proposed = std::fs::read_to_string(input_dir.join("proposed.json")).ok().filter(|t| {
            serde_json::from_str::<serde_json::Value>(t)
                .map(|v| v["acceptance_criteria"].is_array())
                .unwrap_or(false)
        });
        let (Some(edits), Some(proposed)) = (edits, proposed) else {
            fail_turn(&serde_json::json!({"type": "error", "status": 500, "error": {
                "type": "fake_codex",
                "message": "fake-codex: .agent/spec-review-input/edits.json or proposed.json is missing or malformed"}}));
        };
        command(1, "cat .agent/spec-review-input/edits.json", &edits_text, 0);
        command(2, "cat .agent/spec-review-input/proposed.json", &proposed, 0);
        let answers: Vec<serde_json::Value> = edits
            .iter()
            .enumerate()
            .map(|(i, e)| serde_json::json!({"index": e["index"].as_u64().unwrap_or(i as u64), "verdict": "ok", "note": ""}))
            .collect();
        let last = serde_json::json!({"verdict": "APPROVE", "edits": answers}).to_string();
        emit(&agent_message(8, &last));
        if let Err(e) = std::fs::write(&out_path, &last) {
            eprintln!("fake-codex: writing {out_path} failed: {e}");
            std::process::exit(1);
        }
        write_rollout(thread_id);
        emit(&turn_completed(240));
        std::process::exit(0);
    }
    let input = input_dir.join("spec.json");
    let text = std::fs::read_to_string(&input).ok().filter(|t| {
        serde_json::from_str::<serde_json::Value>(t)
            .map(|v| v["acceptance_criteria"].is_array())
            .unwrap_or(false)
    });
    let Some(text) = text else {
        fail_turn(&serde_json::json!({"type": "error", "status": 500, "error": {
            "type": "fake_codex",
            "message": "fake-codex: .agent/spec-review-input/spec.json is missing or not a spec"}}));
    };
    command(1, "cat .agent/spec-review-input/spec.json", &text, 0);
    let mut ids = criterion_ids_in(prompt);
    if ids.is_empty() {
        ids.push("AC1".to_string());
    }
    let revise = scenario == "spec_review_revise";
    let criteria: Vec<serde_json::Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| match (i, revise) {
            (0, true) => serde_json::json!({"id": id, "verdict": "revise", "findings": [
                {"check": "exists", "location": "src/lib.rs:1", "note": "fake: the function the spec names does not exist"}
            ]}),
            (0, false) if scenario != "spec_review_edit" => serde_json::json!({"id": id, "verdict": "ok", "findings": [
                {"check": "steps", "location": "spec", "note": "fake: the test steps could name the command to run"}
            ]}),
            _ => serde_json::json!({"id": id, "verdict": "ok", "findings": []}),
        })
        .collect();
    let doc = if scenario == "spec_review_edit" {
        let spec: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
        spec_edit_doc(&spec, &ids, criteria)
    } else {
        serde_json::json!({
            "verdict": if revise { "REVISE" } else { "APPROVE" },
            "criteria": criteria,
        })
    };
    let last = if scenario == "spec_review_bad" {
        "{not json".to_string()
    } else {
        doc.to_string()
    };
    emit(&agent_message(8, &last));
    // The codex CLI itself writes the final message to -o, outside the sandbox.
    if let Err(e) = std::fs::write(&out_path, &last) {
        eprintln!("fake-codex: writing {out_path} failed: {e}");
        std::process::exit(1);
    }
    write_rollout(thread_id);
    emit(&turn_completed(326));
    std::process::exit(0);
}

/// The app-server exchange recorded from codex-cli 0.154: the initialize
/// reply, one notification, the rate-limits reply.
const APP_SERVER_FIXTURE: &str = include_str!("../../tests/fixtures/codex/app-server-rate-limits.jsonl");

/// `app-server`: answers JSON lines on stdin the way codex's app-server does
/// over stdio, following `FAKE_CODEX_APP_SERVER` (see the file header).
fn app_server() -> ! {
    use std::io::BufRead;
    let recorded: Vec<serde_json::Value> = APP_SERVER_FIXTURE
        .lines()
        .map(|l| serde_json::from_str(l).expect("fixture line is JSON"))
        .collect();
    let mode = std::env::var("FAKE_CODEX_APP_SERVER").unwrap_or_else(|_| "ok".to_string());
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match request["method"].as_str() {
            Some("initialize") => {
                emit(&recorded[0]);
                if mode == "exit" {
                    std::process::exit(3);
                }
                emit(&recorded[1]);
            }
            Some("account/rateLimits/read") => {
                let mut reply = recorded[2].clone();
                reply["id"] = request["id"].clone();
                match mode.as_str() {
                    "error" => {
                        emit(&serde_json::json!({"id": request["id"], "error": {"code": -32600,
                            "message": "codex account authentication required to read rate limits"}}));
                        continue;
                    }
                    "hang" => {
                        if let Some(path) = std::env::var_os("FAKE_CODEX_HANG_STARTED") {
                            let _ = std::fs::write(path, "started\n");
                        }
                        // `FAKE_CODEX_HANG_SECS`: how long the stand-in hangs.
                        // The timeout test sets it well above the timeout so
                        // its "ended at once" bound can stay under half of
                        // this lifetime.
                        let hang_secs = std::env::var("FAKE_CODEX_HANG_SECS")
                            .ok()
                            .and_then(|v| v.trim().parse::<u64>().ok())
                            .filter(|n| *n > 0)
                            .unwrap_or(6);
                        std::thread::sleep(std::time::Duration::from_secs(hang_secs));
                        if let Some(path) = std::env::var_os("FAKE_CODEX_HANG_MARKER") {
                            let _ = std::fs::write(path, "finished\n");
                        }
                        std::process::exit(0);
                    }
                    "denied" => reply["result"]["ordinaryUsageAllowed"] = false.into(),
                    "denied_empty" => {
                        reply["result"]["ordinaryUsageAllowed"] = false.into();
                        reply["result"]["rateLimits"]["primary"] = serde_json::Value::Null;
                        reply["result"]["rateLimits"]["secondary"] = serde_json::Value::Null;
                        reply["result"]["rateLimitsByLimitId"] = serde_json::Value::Null;
                    }
                    _ => {}
                }
                // TIER-1 switch: `none` drops `planType`, any other value
                // replaces it, on `rateLimits` and every bucket object.
                if let Ok(plan) = std::env::var("FAKE_CODEX_PLAN_TYPE") {
                    let set = |v: &mut serde_json::Value| {
                        let Some(obj) = v.as_object_mut() else { return };
                        if plan == "none" {
                            obj.remove("planType");
                        } else {
                            obj.insert("planType".into(), serde_json::json!(plan));
                        }
                    };
                    if let Some(result) = reply.get_mut("result") {
                        if let Some(limits) = result.get_mut("rateLimits") {
                            set(limits);
                        }
                        if let Some(map) =
                            result.get_mut("rateLimitsByLimitId").and_then(|v| v.as_object_mut())
                        {
                            map.values_mut().for_each(set);
                        }
                    }
                }
                emit(&reply);
            }
            _ => {}
        }
    }
    std::process::exit(0);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    relaunch_like_the_npm_launcher(&args);
    log_args(&args);

    // `codex login` (`thoth profile login`): the real one does not create
    // CODEX_HOME, so this refuses a missing one. Prints nothing on stdout.
    if args == ["login"] {
        let home = std::env::var("CODEX_HOME").ok().map(std::path::PathBuf::from);
        let Some(home) = home.filter(|h| h.is_dir()) else {
            eprintln!("fake-codex: CODEX_HOME must be an existing directory");
            std::process::exit(64);
        };
        if let Some(ms) = std::env::var("FAKE_LOGIN_SLEEP_MS").ok().and_then(|v| v.parse::<u64>().ok()) {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
        if std::env::var("FAKE_LOGIN_FAIL").as_deref() == Ok("1") {
            eprintln!("fake-codex: login aborted");
            std::process::exit(1);
        }
        let _ = std::fs::write(home.join(LOGIN_MARKER), "1\n");
        eprintln!("fake-codex: logged in");
        std::process::exit(0);
    }

    // `codex login status`: exit 0 and "Logged in using ChatGPT" on stderr
    // when logged in; nothing about email or plan. CODEX_HOME must name a
    // real directory, which is how a test proves thoth set it.
    if args.len() >= 2 && args[0] == "login" && args[1] == "status" {
        let home_ok = std::env::var("CODEX_HOME")
            .map(|h| std::path::Path::new(&h).is_dir())
            .unwrap_or(false);
        let marker_ok = std::env::var("FAKE_LOGIN_BY_FILE").as_deref() != Ok("1")
            || std::env::var("CODEX_HOME")
                .map(|h| std::path::Path::new(&h).join(LOGIN_MARKER).is_file())
                .unwrap_or(false);
        // FC-1: `FAKE_LOGOUT_MARKER` names a file the `auth_expired` run writes.
        let logout_marker = std::env::var_os("FAKE_LOGOUT_MARKER")
            .is_some_and(|p| std::path::Path::new(&p).is_file());
        if home_ok
            && marker_ok
            && !logout_marker
            && std::env::var("FAKE_CODEX_LOGGED_OUT").as_deref() != Ok("1")
        {
            eprintln!("Logged in using ChatGPT");
            std::process::exit(0);
        }
        eprintln!("Not logged in");
        std::process::exit(1);
    }

    // `codex app-server --listen stdio://` (thoth profile usage --refresh)
    if args.first().map(String::as_str) == Some("app-server") {
        app_server();
    }

    // `codex --version` (thoth doctor)
    if args == ["--version"] {
        println!("codex-cli 0.0.0-fake");
        std::process::exit(0);
    }

    if args.first().map(String::as_str) != Some("exec") {
        eprintln!("fake-codex: unsupported invocation {args:?}");
        std::process::exit(64);
    }
    let resume_thread = match parse_exec(&args[1..]) {
        Ok(r) => r,
        Err(msg) => {
            // clap's wording and exit code, as the real codex answers
            eprintln!("error: {msg}");
            std::process::exit(2);
        }
    };
    let prompt = exec_prompt(&args[1..]);
    if let Ok(path) = std::env::var("FAKE_PROMPT_COPY") {
        let _ = std::fs::write(path, prompt.as_bytes());
    }
    log_env_names();
    if let Some(dir) = arg_value(&args, "-C") {
        let _ = std::env::set_current_dir(dir);
    }
    let thread_id = resume_thread
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // FC-2: which launch this is (`FAKE_ATTEMPTS_FILE`), and the scenario
    // for it (`FAKE_FIRST_SCENARIO` for the first `FAKE_FIRST_TIMES`).
    let launch = record_attempt(resume_thread.is_some(), &thread_id);
    // `FAKE_CODEX_SCENARIO` wins over `FAKE_SCENARIO`, so one test can run
    // codex and claude in different scenarios.
    let scenario = first_scenario(launch).unwrap_or_else(|| {
        std::env::var("FAKE_CODEX_SCENARIO")
            .or_else(|_| std::env::var("FAKE_SCENARIO"))
            .unwrap_or_else(|_| "ok".to_string())
    });
    let task_id = std::env::var("FAKE_TASK_ID").unwrap_or_else(|_| "UNKNOWN-TASK".to_string());

    emit(&thread_started(&thread_id));

    if std::env::var("CODEX_HOME").is_err() {
        emit(&turn_started());
        fail_turn(&serde_json::json!({"type": "error", "status": 401,
            "error": {"type": "auth_error", "message": "fake-codex: CODEX_HOME is not set"}}));
    }

    match scenario.as_str() {
        "ok" | "research_ok" | "research_unsourced" | "research_no_sources" => {
            emit(&turn_started());
            emit(&agent_message(0, "I'll run the checks, leave my change uncommitted, then stop.\n"));
            command(1, "echo hello-stdout", "hello-stdout\n", 0);
            command(2, "sh -c \"echo oops >&2; exit 3\"", "oops\n", 3);
            // E5: codex keeps the head and tail of a long output and says how
            // much it dropped in between.
            command(3, "seq 1 200000", "1\n2\n3\n... 240319 bytes omitted ...\n199999\n200000\n", 0);
            if std::env::var("FAKE_CODEX_EXTRA_ITEMS").as_deref() == Ok("1") {
                for (id, ty) in [(4, "collab_tool_call"), (5, "mcp_tool_call")] {
                    for (ev, status) in [("item.started", "in_progress"), ("item.completed", "completed")] {
                        emit(&serde_json::json!({"type": ev, "item": {
                            "id": format!("item_{id}"), "type": ty, "status": status}}));
                    }
                }
            }
            // NOT a recording: no web search was captured from codex. Only
            // `id` and `type`, the two keys thoth reads from an item.
            if std::env::var("FAKE_CODEX_WEB_SEARCH").as_deref() == Ok("1") {
                for ev in ["item.started", "item.completed"] {
                    emit(&serde_json::json!({"type": ev, "item": {"id": "item_7", "type": "web_search"}}));
                }
            }
            add_file(6, "codex_added.txt", "added by fake codex\n");
            let codes = wait_for_index(3, std::time::Duration::from_secs(30));
            if scenario == "ok" {
                write_result(&task_id, "fake codex completed the task", &codes[..1.min(codes.len())]);
            } else {
                let mut claims = vec![serde_json::json!(
                    {"kind": "completion", "text": "all acceptance criteria met", "evidence": []})];
                claims.extend(research_claims(&scenario));
                write_claims_result(&task_id, "fake codex completed the task", claims);
            }
            write_rollout(&thread_id);
            emit(&agent_message(9, "Done."));
            emit(&turn_completed(157));
            std::process::exit(0);
        }
        "error" => {
            // exec-error-bad-model.jsonl, verbatim apart from the model name
            emit(&serde_json::json!({"type": "item.completed", "item": {"id": "item_0",
                "type": "error", "message": "Model metadata for `no-such-model-xyz` not found. Defaulting to fallback metadata; this can degrade performance and cause issues."}}));
            emit(&turn_started());
            fail_turn(&serde_json::json!({"type": "error", "status": 400, "error": {
                "type": "invalid_request_error",
                "message": "The 'no-such-model-xyz' model is not supported when using Codex with a ChatGPT account."}}));
        }
        "quota" => {
            // NOT a recording: codex's usage limit could not be triggered
            // (scope §1 E6). The E6 error shape, carrying the strings found in
            // the codex binary.
            emit(&turn_started());
            command(1, "echo before-the-limit", "before-the-limit\n", 0);
            fail_turn(&serde_json::json!({"type": "error", "status": 429, "error": {
                "type": "usage_limit_reached",
                "message": "You've hit your usage limit. Try again later."}}));
        }
        "resume" => {
            if resume_thread.is_none() {
                emit(&turn_started());
                fail_turn(&serde_json::json!({"type": "error", "status": 429, "error": {
                    "type": "usage_limit_reached",
                    "message": "You've hit your usage limit. Try again later."}}));
            }
            emit(&turn_started());
            emit(&agent_message(0, "Picking up where I left off.\n"));
            add_file(1, "codex_resumed.txt", "again\n");
            let codes = wait_for_index(1, std::time::Duration::from_secs(30));
            write_result(&task_id, "fake codex resumed and finished", &codes[..1.min(codes.len())]);
            emit(&turn_completed(213));
            std::process::exit(0);
        }
        "no_changes" => {
            // AC2's no-op case: the worker touches nothing, so thoth's
            // post-run commit has nothing to add and the candidate is the base.
            emit(&turn_started());
            emit(&agent_message(0, "Nothing needs to change here.\n"));
            command(1, "echo nothing-to-do", "nothing-to-do\n", 0);
            let codes = wait_for_index(1, std::time::Duration::from_secs(30));
            write_result(&task_id, "fake codex found nothing to change", &codes[..1.min(codes.len())]);
            emit(&turn_completed(42));
            std::process::exit(0);
        }
        "commits_anyway" => {
            emit(&turn_started());
            emit(&agent_message(0, "Committing this myself.\n"));
            commit_file(1, "worker_committed.txt", "committed by the worker itself\n");
            let codes = wait_for_index(1, std::time::Duration::from_secs(30));
            write_result(&task_id, "fake codex committed its own change", &codes[..1.min(codes.len())]);
            emit(&turn_completed(99));
            std::process::exit(0);
        }
        "review_pass" | "review_reject" | "review_bad" | "review_uncited" | "source_check" | "source_check_bad" => {
            review(&scenario, &args, &prompt, &thread_id)
        }
        "spec_review_approve" | "spec_review_revise" | "spec_review_bad" | "spec_review_edit"
        | "spec_confirm_approve" => spec_review(&scenario, &args, &prompt, &thread_id),
        // FC-1: the session was revoked, and from now on `login status` says
        // logged out. Synthetic text, not a recording.
        "auth_expired" => {
            emit(&turn_started());
            if let Some(p) = std::env::var_os("FAKE_LOGOUT_MARKER") {
                let _ = std::fs::write(p, "1\n");
            }
            fail_turn_text("fake-codex: session revoked (synthetic text, not a recording)");
        }
        // FC-1: the stream dropped mid-turn. The string H5 names; codex's real
        // error could not be recorded.
        "disconnected" => {
            emit(&turn_started());
            command(1, "echo before-the-drop", "before-the-drop\n", 0);
            fail_turn_text(
                "stream disconnected before completion: error decoding response body (synthetic; the string H5 names, no recording)",
            );
        }
        "crash" => {
            emit(&turn_started());
            std::process::exit(137);
        }
        "slow" => {
            emit(&turn_started());
            for i in 0..60 {
                emit(&agent_message(i, &format!("still working {i}")));
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            emit(&turn_completed(1));
            std::process::exit(0);
        }
        other => {
            eprintln!("fake-codex: unknown scenario {other}");
            std::process::exit(64);
        }
    }
}
