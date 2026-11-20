//! SMALL-9a: the quota reserve (`quota-reserve-percent`). Every case runs the
//! CLI with a temporary THOTH_HOME and the fake CLIs; quota readings are
//! seeded only as a codex profile's `sessions/` rollout file (as
//! tests/extra_buckets.rs does), and everything is read back through
//! `--json`, events, evidence and the worktrees folder.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

const CODEX_MODEL: &str = "gpt-5.6-luna";
const FUTURE: u64 = 4_102_444_800; // 2100-01-01T00:00:00Z
const PAST: u64 = 1_000_000_000; // 2001-09-09T01:46:40Z
const OBSERVED: &str = "2026-10-29T00:00:00Z";
const BLOCKED: &str = "codex-q";

/// Two acceptance criteria, for the review flows.
const SPEC: &str = r#"{"goal":"keep the greeting right","acceptance_criteria":["AC1: greets the user","AC2: exits zero"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal"}"#;
/// The spec fake-claude's `spec_review_edit` proposes edits to.
const EDITABLE: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":["no new crate"],"non_goals":["rewriting the transport","changing the log format"],"excluded_alternatives":[],"risk":"important"}"#;

/// `f.cmd()` without any inherited variable whose name holds KEY, TOKEN,
/// SECRET or PASSWORD (names are read, never values).
fn cmd(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_slice(bytes)
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(bytes)))
}

/// Runs `c`; panics with its exit code, stdout and stderr unless it exits 0;
/// its stdout.
fn run_ok(mut c: assert_cmd::Command, what: &str) -> Vec<u8> {
    let out = c.output().expect("spawn thoth");
    if !out.status.success() {
        panic!(
            "thoth {what} exited {:?}\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    out.stdout
}

/// Runs `c`; panics unless it exits 0; its stdout as JSON.
fn ok_json(c: assert_cmd::Command, what: &str) -> Value {
    json_of(&run_ok(c, what))
}

/// `thoth ARGS --json` (the caller gives `--json`), exit 0, as JSON.
fn cli(f: &Fixture, args: &[&str]) -> Value {
    let mut c = cmd(f);
    c.args(args);
    ok_json(c, &args.join(" "))
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

/// `profile add ID --provider P --harness H` for a fresh folder; the folder.
fn add_profile(f: &Fixture, provider: &str, harness: &str, id: &str) -> PathBuf {
    let dir = f.home_path().join("profiles").join(id);
    fs::create_dir_all(&dir).unwrap();
    let shown = dir.display().to_string();
    cli(f, &["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir", &shown, "--json"]);
    dir
}

fn create_task(f: &Fixture, spec: &str) -> (String, String) {
    let file = f.home_path().join("spec.json");
    fs::write(&file, spec).unwrap();
    let shown = file.display().to_string();
    let v = cli(f, &["task", "create", "-f", &shown, "--json"]);
    (s(&v["task_id"]), s(&v["base_commit"]))
}

/// `wait TASK` until a wait-terminal status: (0, status).
fn wait_for(f: &Fixture, task: &str, timeout: &str) -> (i32, String) {
    (0, s(&cli(f, &["wait", task, "--timeout", timeout, "--json"])["status"]))
}

fn events_json(f: &Fixture, task: &str) -> Vec<Value> {
    cli(f, &["events", "--since", "0", "--task", task, "--json"])["events"].as_array().unwrap().clone()
}

fn task_status(f: &Fixture, task: &str) -> String {
    s(&cli(f, &["status", task, "--json"])["status"])
}

fn run_of(f: &Fixture, task: &str, run: &str) -> Value {
    let v = cli(f, &["status", task, "--json"]);
    v["runs"].as_array().unwrap().iter().find(|r| r["run_id"] == run).cloned().unwrap_or_else(|| panic!("no run {run}: {v}"))
}

fn evidence_rows(f: &Fixture, task: &str) -> Vec<Value> {
    cli(f, &["evidence", "list", task, "--json"])["evidence"].as_array().unwrap().clone()
}

/// For a panic message: the task's runs and event types.
fn explain(f: &Fixture, task: &str) -> String {
    let status = cli(f, &["status", task, "--json"]);
    let types: Vec<String> = events_json(f, task).iter().map(|e| s(&e["type"])).collect();
    format!("runs: {}\nevents: {types:?}", status["runs"])
}

fn set_reserve(f: &Fixture, n: &str) {
    let v = ok_json({
        let mut c = cmd(f);
        c.args(["config", "set", "quota-reserve-percent", n, "--json"]);
        c
    }, "config set");
    assert_eq!(v["value"], n, "{v}");
}

/// The codex profile's only rollout file: a general bucket whose five-hour
/// window is `used` percent resetting at `resets_at` (no reset time when
/// `None`), and a weekly window at 1% resetting at `weekly_resets_at`.
fn seed_reading(cfg: &std::path::Path, used: f64, resets_at: Option<u64>, weekly_resets_at: u64) {
    let sessions = cfg.join("sessions");
    fs::create_dir_all(&sessions).unwrap();
    let line = json!({"timestamp": OBSERVED, "type": "event_msg", "payload": {"type": "token_count", "rate_limits": {
        "limit_id": "codex", "limit_name": null,
        "primary": {"used_percent": used, "window_minutes": 300, "resets_at": resets_at},
        "secondary": {"used_percent": 1.0, "window_minutes": 10080, "resets_at": weekly_resets_at}
    }}});
    fs::write(sessions.join("rollout.jsonl"), format!("{line}\n")).unwrap();
}

/// A reading of `used` on the five-hour window resetting in 2100.
fn seed(cfg: &std::path::Path, used: f64) {
    seed_reading(cfg, used, Some(FUTURE), FUTURE);
}

fn init() -> Fixture {
    let f = common::fixture();
    cmd(&f).arg("init").assert().success();
    f
}

/// Everything a refused launch must leave as it was.
#[derive(Debug, PartialEq)]
struct Written {
    task_status: String,
    runs: Vec<(String, String)>,
    events: Vec<(i64, String)>,
    evidence: Value,
    worktrees: Vec<String>,
}

fn worktrees_dir(f: &Fixture) -> PathBuf {
    f.home_path().join("repos").join(f.repo_id()).join("worktrees")
}

fn written(f: &Fixture, task: &str) -> Written {
    let status = ok_json({
        let mut c = cmd(f);
        c.args(["status", task, "--json"]);
        c
    }, "status");
    let runs = status["runs"]
        .as_array()
        .unwrap_or_else(|| panic!("no runs array: {status}"))
        .iter()
        .map(|r| (s(&r["run_id"]), s(&r["status"])))
        .collect();
    let events = events_json(f, task)
        .iter()
        .map(|e| (e["seq"].as_i64().unwrap(), s(&e["type"])))
        .collect();
    let evidence = ok_json({
        let mut c = cmd(f);
        c.args(["evidence", "list", task, "--json"]);
        c
    }, "evidence list")["evidence"]
        .clone();
    let mut worktrees: Vec<String> = match fs::read_dir(worktrees_dir(f)) {
        Ok(dir) => dir.map(|e| e.unwrap().file_name().to_string_lossy().to_string()).collect(),
        Err(_) => Vec::new(),
    };
    worktrees.sort();
    Written { task_status: s(&status["status"]), runs, events, evidence, worktrees }
}

/// The codex launch flags.
fn codex_launch() -> [&'static str; 4] {
    ["--model", CODEX_MODEL, "--effort", "medium"]
}

/// The window a refusal must name.
struct Held<'a> {
    profile: &'a str,
    /// The window name, with the quota group for antigravity.
    window: &'a str,
    used: &'a str,
    observed_at: &'a str,
    /// `resets_at` as the refusal prints it (`unknown` when there is none).
    resets_at: &'a str,
}

/// codex-q's five-hour window at `used`, read at OBSERVED, resetting in 2100.
fn five_hour(used: &str) -> Held<'_> {
    Held { profile: BLOCKED, window: "five_hour", used, observed_at: OBSERVED, resets_at: "2100-01-01T00:00:00Z" }
}

/// Invokes a launch that must be refused by the reserve at `n` for codex-q's
/// five-hour window at `used`; see `assert_held`.
fn assert_refused(f: &Fixture, task: &str, args: &[&str], n: u64, used: &str, role: &str) -> String {
    assert_held(f, task, args, n, &five_hour(used), role)
}

/// Invokes a launch that must be refused by the reserve at `n` for `held`:
/// exit 1, the `{"ok":false,"error":...}` on stderr with every part of the
/// refusal, nothing written, and the account's CLI never started (neither
/// fake-codex nor fake-agy logged a call). Returns the error text.
fn assert_held(f: &Fixture, task: &str, args: &[&str], n: u64, held: &Held<'_>, role: &str) -> String {
    let before = written(f, task);
    let log = f.home_path().join("refused-codex-args.jsonl");
    let agy_log = f.home_path().join("refused-agy-args.jsonl");
    let mut c = cmd(f);
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", task)
        .env("FAKE_CODEX_ARGS_LOG", &log)
        .env("FAKE_AGY_ARGS_LOG", &agy_log)
        .args(args);
    let out = c.output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{args:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let err = json_of(&out.stderr);
    assert_eq!(err["ok"], false, "{err}");
    let text = s(&err["error"]);
    for part in [
        format!("quota-reserve-percent {n}"),
        format!("profile {}", held.profile),
        format!("its {} window is {}% used", held.window, held.used),
        format!("observed at {}", held.observed_at),
        format!("resets at {}", held.resets_at),
        format!("threshold of {}%", 100 - n),
        format!("thoth recommend --role {role} --task {task}"),
        "wait for the window to reset".to_string(),
        "thoth profile usage --refresh".to_string(),
        "thoth config set quota-reserve-percent".to_string(),
    ] {
        assert!(text.contains(&part), "{args:?}: missing {part:?} in {text}");
    }
    assert_eq!(written(f, task), before, "{args:?} wrote something");
    // Before the login check: the account's CLI was never started.
    for log in [log, agy_log] {
        assert!(!log.exists(), "{args:?} started the CLI: {}", fs::read_to_string(&log).unwrap_or_default());
    }
    text
}

/// A codex dispatch that must go ahead; its `--json`, after the run ended.
fn dispatch_ok(f: &Fixture, task: &str) -> Value {
    let mut c = cmd(f);
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", task)
        .args(["dispatch", task, "--profile", BLOCKED, "--json"])
        .args(codex_launch());
    let d = ok_json(c, "dispatch");
    assert_eq!(wait_for(f, task, "180s").1, "in_review", "{}", explain(f, task));
    d
}

fn warnings(v: &Value) -> Vec<String> {
    v["warnings"]
        .as_array()
        .unwrap_or_else(|| panic!("no warnings array: {v}"))
        .iter()
        .map(s)
        .collect()
}

fn reserve_notes(v: &Value) -> Vec<String> {
    warnings(v).into_iter().filter(|w| w.contains("quota-reserve-percent")).collect()
}

/// AC4: the one note of a launch that went ahead without a reading.
fn assert_note(v: &Value, n: u64) {
    let notes = reserve_notes(v);
    assert_eq!(notes.len(), 1, "{v}");
    for part in [format!("quota-reserve-percent {n}"), BLOCKED.to_string(), "not held back".into(), "thoth profile usage --refresh".into()] {
        assert!(notes[0].contains(&part), "missing {part:?} in {}", notes[0]);
    }
}

fn dispatch_args(task: &str) -> Vec<&str> {
    let mut a = vec!["dispatch", task, "--profile", BLOCKED, "--json"];
    a.extend(codex_launch());
    a
}

// (a), (b), (c), (d), (e): dispatch ------------------------------------------------

#[test]
fn a_dispatch_on_an_account_past_the_reserve_is_refused_and_writes_nothing() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, Fixture::SPEC);
    seed(&cfg, 97.0);
    set_reserve(&f, "5");
    let text = assert_refused(&f, &task, &dispatch_args(&task), 5, "97", "implementer");
    assert!(text.contains("resets at 2100-01-01T00:00:00Z"), "{text}");
    assert_eq!(task_status(&f, &task), "queued");
}

#[test]
fn the_threshold_is_inclusive() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, Fixture::SPEC);
    set_reserve(&f, "5");
    seed(&cfg, 95.0);
    assert_refused(&f, &task, &dispatch_args(&task), 5, "95", "implementer");
    seed(&cfg, 94.0);
    let d = dispatch_ok(&f, &task);
    assert!(reserve_notes(&d).is_empty(), "a reading below the reserve has no note: {d}");
}

#[test]
fn a_full_window_whose_reset_has_passed_does_not_hold_back() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, Fixture::SPEC);
    set_reserve(&f, "5");
    // The weekly window still counts, so there is a current reading.
    seed_reading(&cfg, 97.0, Some(PAST), FUTURE);
    let d = dispatch_ok(&f, &task);
    assert!(reserve_notes(&d).is_empty(), "{d}");
}

#[test]
fn reserve_zero_dispatches_at_ninety_nine_percent_without_a_note() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, Fixture::SPEC);
    set_reserve(&f, "0");
    seed(&cfg, 99.0);
    let d = dispatch_ok(&f, &task);
    assert!(reserve_notes(&d).is_empty(), "{d}");
}

#[test]
fn no_reading_dispatches_with_the_note() {
    let f = init();
    add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, Fixture::SPEC);
    set_reserve(&f, "5");
    let d = dispatch_ok(&f, &task);
    assert_note(&d, 5);
    // No new event kind, and the launch_choice detail keeps its keys.
    let events = events_json(&f, &task);
    let types: Vec<String> = events.iter().map(|e| s(&e["type"])).collect();
    assert!(!types.iter().any(|t| t.contains("reserve")), "{types:?}");
    let choice = events
        .into_iter()
        .find(|e| e["type"] == "launch_choice")
        .expect("a launch_choice event")["detail"]
        .clone();
    let mut keys: Vec<&str> = choice.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["actual", "difficulty", "inherited", "previous_run", "reason", "require_reason", "risk", "role", "source", "task_type"],
        "{choice}"
    );
}

/// AC4: every window already reset counts as no reading, in --json and on
/// stderr in human mode.
#[test]
fn every_window_reset_dispatches_with_the_note_in_json_and_in_human_mode() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    set_reserve(&f, "5");
    seed_reading(&cfg, 97.0, Some(PAST), PAST);
    let (task, _) = create_task(&f, Fixture::SPEC);
    let d = dispatch_ok(&f, &task);
    assert_note(&d, 5);

    let (human, _) = create_task(&f, Fixture::SPEC);
    let mut c = cmd(&f);
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &human)
        .args(["dispatch", &human, "--profile", BLOCKED])
        .args(codex_launch());
    let out = c.output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let line = stderr
        .lines()
        .find(|l| l.starts_with("warning: quota-reserve-percent 5"))
        .unwrap_or_else(|| panic!("no warning line: {stderr}"));
    assert!(line.contains(BLOCKED) && line.contains("not held back") && line.contains("thoth profile usage --refresh"), "{line}");
    assert_eq!(wait_for(&f, &human, "180s").1, "in_review", "{}", explain(&f, &human));
}

/// A full window with no reset time counts as not reset, as it does for
/// `exhausted`: recommend marks it and the dispatch is refused with `resets
/// at unknown`.
#[test]
fn an_unknown_reset_time_counts_as_not_reset() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, Fixture::SPEC);
    set_reserve(&f, "5");
    seed_reading(&cfg, 97.0, None, FUTURE);
    let c = candidate(&recommend(&f, "implementer"), BLOCKED);
    assert_eq!((c["reserve_blocked"].clone(), c["eligible"].clone()), (json!(true), json!(false)), "{c}");
    let held = Held { resets_at: "unknown", ..five_hour("97") };
    assert_held(&f, &task, &dispatch_args(&task), 5, &held, "implementer");
    assert_eq!(task_status(&f, &task), "queued");
}

/// An antigravity account is held back only by the quota group its model
/// uses, and the refusal names the window with its group. The reading comes
/// from fake-agy's `/usage` answer through `profile usage --refresh`.
#[test]
fn an_antigravity_account_is_held_back_by_the_group_of_its_model() {
    const AGY: &str = "agy-q";
    const GEMINI_MODEL: &str = "gemini-3.8-flash-low";
    const OTHER_MODEL: &str = "claude-sonnet-4-6";
    let f = init();
    add_profile(&f, "google", "antigravity", AGY);
    let (task, _) = create_task(&f, Fixture::SPEC);
    let answer = json!({"status": "SUCCESS", "num_turns": 0, "command": {"name": "usage", "data": {"groups": [
        {"name": "Gemini Models", "description": "Models within this group: Gemini Flash, Gemini Pro", "buckets": [
            {"id": "gemini-weekly", "window": "weekly", "remaining_fraction": 0.03, "reset_time": "2099-01-01T00:00:00Z"}]},
        {"name": "Claude and GPT models", "description": "Models within this group: Claude Opus, Claude Sonnet, GPT-OSS", "buckets": [
            {"id": "3p-weekly", "window": "weekly", "remaining_fraction": 0.9, "reset_time": "2099-01-01T00:00:00Z"}]}
    ]}}});
    let file = f.home_path().join("agy-usage.json");
    fs::write(&file, answer.to_string()).unwrap();
    cli(&f, &["config", "set", "live-quota", "on", "--json"]);
    let mut c = cmd(&f);
    c.env("FAKE_AGY_USAGE_FILE", &file).args(["profile", "usage", "--refresh", "--json"]);
    let usage = ok_json(c, "profile usage --refresh");
    // Off again, so nothing below reads agy's built-in answer instead.
    cli(&f, &["config", "set", "live-quota", "off", "--json"]);
    let profile = usage["profiles"].as_array().unwrap().iter().find(|p| p["profile_id"] == AGY).cloned().unwrap();
    let gemini = profile["windows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|w| w["name"] == "gemini-weekly")
        .cloned()
        .unwrap_or_else(|| panic!("no gemini-weekly reading: {usage}"));
    assert_eq!(gemini["used_percent"], 97.0, "{gemini}");
    let observed_at = s(&gemini["observed_at"]);
    set_reserve(&f, "5");

    let mut c = cmd(&f);
    c.args(["recommend", "--role", "implementer", "--model", GEMINI_MODEL, "--json"]);
    let blocked = candidate(&ok_json(c, "recommend --model"), AGY);
    assert_eq!((blocked["reserve_blocked"].clone(), blocked["eligible"].clone()), (json!(true), json!(false)), "{blocked}");
    let reason = blocked["reasons"].as_array().unwrap().iter().map(s).find(|r| r.contains("quota-reserve-percent 5"));
    let reason = reason.unwrap_or_else(|| panic!("no reserve reason: {blocked}"));
    for part in ["gemini-weekly (Gemini Models)", "97%", &observed_at, "95%"] {
        assert!(reason.contains(part), "missing {part:?} in {reason}");
    }
    // The other group decides for a model in it; Gemini's 97% does not count.
    let mut c = cmd(&f);
    c.args(["recommend", "--role", "implementer", "--model", OTHER_MODEL, "--json"]);
    let open = candidate(&ok_json(c, "recommend --model"), AGY);
    assert_eq!(open["reserve_blocked"], false, "{open}");

    let held = Held {
        profile: AGY,
        window: "gemini-weekly (Gemini Models)",
        used: "97",
        observed_at: &observed_at,
        resets_at: "2099-01-01T00:00:00Z",
    };
    let args = ["dispatch", &task, "--profile", AGY, "--model", GEMINI_MODEL, "--json"];
    assert_held(&f, &task, &args, 5, &held, "implementer");
}

// (f): the other launches ----------------------------------------------------------

/// `count` numbered lines, `prefix N`.
fn lines(prefix: &str, count: usize) -> String {
    (1..=count).map(|i| format!("{prefix} {i}\n")).collect()
}

/// A task of SPEC at `in_review`, implemented by claude-a, with codex-q
/// registered; returns (fixture, codex-q's folder, task).
fn in_review() -> (Fixture, PathBuf, String) {
    let f = common::fixture();
    let repo = f.repo_path();
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("src/greet.rs"), lines("line", 30)).unwrap();
    fs::write(repo.join(".gitattributes"), "* -text\n").unwrap();
    common::git(&repo, &["add", "-A"]);
    common::git(&repo, &["commit", "-m", "candidate files"]);
    cmd(&f).arg("init").assert().success();
    add_profile(&f, "anthropic", "claude-code", "claude-a");
    add_profile(&f, "anthropic", "claude-code", "claude-b");
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, SPEC);
    let mut c = cmd(&f);
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task)
        .args(["dispatch", &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium", "--json"]);
    run_ok(c, "dispatch");
    assert_eq!(wait_for(&f, &task, "180s").1, "in_review", "{}", explain(&f, &task));
    (f, cfg, task)
}

#[test]
fn review_start_with_and_without_edit_is_refused_before_the_evidence() {
    let (f, cfg, task) = in_review();
    seed(&cfg, 97.0);
    set_reserve(&f, "5");
    let mut args = vec!["review", "start", &task, "--profile", BLOCKED, "--json"];
    args.extend(codex_launch());
    assert_refused(&f, &task, &args, 5, "97", "reviewer");
    args.push("--edit");
    assert_refused(&f, &task, &args, 5, "97", "reviewer");
    assert_eq!(task_status(&f, &task), "in_review");
}

#[test]
fn review_confirm_of_a_pending_fix_is_refused() {
    let (f, cfg, task) = in_review();
    // RFIX-1, as tests/review_fix.rs does it: an applied fix of AC1, verified
    // by an approved green check.
    cmd(&f).args(["checks", "add", "green", "--cmd", "git --version", "--shell", "argv", "--json"]).assert().success();
    cmd(&f).args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
    let code = s(&evidence_rows(&f, &task)[0]["code"]);
    let plan = json!({"document": {
        "verdict": "REJECT",
        "findings": {"criteria": [
            {"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "the greeting is wrong", "category": "spec_mismatch", "fix_size": "S"},
            {"id": "AC2", "verdict": "met", "evidence": [code]}
        ], "extra_findings": []},
        "edits": [{"path": "src/greet.rs", "old": "line 1\n", "new": "line one\n", "criterion": "AC1", "reason": "fixes the finding"}],
    }});
    let plan_file = f.home_path().join("plan.json");
    fs::write(&plan_file, plan.to_string()).unwrap();
    let mut c = cmd(&f);
    c.env("FAKE_SCENARIO", "review_fix")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_FIX_PLAN", &plan_file)
        .args(["review", "start", &task, "--profile", "claude-b", "--model", "opus", "--effort", "medium", "--edit", "--json"]);
    let started = ok_json(c, "review start --edit");
    let edit_run = s(&started["run_id"]);
    let mut c = cmd(&f);
    c.args(["wait", "--run", &edit_run, "--timeout", "180s", "--json"]);
    let w = ok_json(c, "wait --run");
    assert_eq!(w["pending_fix"], true, "{w}\n{}", explain(&f, &task));
    let out = cmd(&f).args(["verify", &edit_run, "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stdout));

    seed(&cfg, 97.0);
    set_reserve(&f, "5");
    let mut args = vec!["review", "confirm", &task, "--profile", BLOCKED, "--json"];
    args.extend(codex_launch());
    assert_refused(&f, &task, &args, 5, "97", "reviewer");
    assert_eq!(task_status(&f, &task), "in_review");
}

#[test]
fn spec_review_start_with_and_without_edit_is_refused() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, SPEC);
    seed(&cfg, 97.0);
    set_reserve(&f, "5");
    let mut args = vec!["spec", "review", "start", &task, "--profile", BLOCKED, "--json"];
    args.extend(codex_launch());
    assert_refused(&f, &task, &args, 5, "97", "reviewer");
    args.push("--edit");
    assert_refused(&f, &task, &args, 5, "97", "reviewer");
}

#[test]
fn spec_review_confirm_of_pending_edits_is_refused() {
    let f = init();
    add_profile(&f, "anthropic", "claude-code", "claude-a");
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, EDITABLE);
    // SR-3, as tests/spec_review_edit.rs does it: claude-a's edit review.
    let mut c = cmd(&f);
    c.env("FAKE_SCENARIO", "spec_review_edit")
        .env("FAKE_TASK_ID", &task)
        .args(["spec", "review", "start", &task, "--profile", "claude-a", "--model", "haiku", "--edit", "--json"]);
    let r = ok_json(c, "spec review start --edit");
    let mut c = cmd(&f);
    c.args(["wait", "--run", &s(&r["run_id"]), "--timeout", "180s", "--json"]);
    assert_eq!(ok_json(c, "wait --run")["verdict"], "REVISE");
    let mut c = cmd(&f);
    c.args(["spec", "review", "show", &task, "--json"]);
    assert_eq!(ok_json(c, "spec review show")["pending_edits"], true);

    seed(&cfg, 97.0);
    set_reserve(&f, "5");
    let mut args = vec!["spec", "review", "confirm", &task, "--profile", BLOCKED, "--json"];
    args.extend(codex_launch());
    assert_refused(&f, &task, &args, 5, "97", "reviewer");
}

/// The profiles `status TASK --json` lists as still having quota, for the
/// run that stopped on quota.
fn profiles_with_quota(f: &Fixture, task: &str, run: &str) -> Vec<String> {
    run_of(f, task, run)["profiles_with_quota"]
        .as_array()
        .unwrap_or_else(|| panic!("no profiles_with_quota: {}", run_of(f, task, run)))
        .iter()
        .map(s)
        .collect()
}

#[test]
fn resume_is_refused_past_the_reserve_and_goes_ahead_with_the_note_after_the_reset() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, Fixture::SPEC);
    let mut c = cmd(&f);
    c.env("FAKE_SCENARIO", "quota")
        .env("FAKE_TASK_ID", &task)
        .args(["dispatch", &task, "--profile", BLOCKED, "--json"])
        .args(codex_launch());
    let run = s(&ok_json(c, "dispatch")["run_id"]);
    assert_eq!(wait_for(&f, &task, "180s").1, "interrupted", "{}", explain(&f, &task));
    assert!(run_of(&f, &task, &run)["harness_session_id"].is_string());
    cmd(&f).args(["profile", "check", BLOCKED]).assert().success();

    // AC2: status follows `eligible` (it lists the accounts other than the
    // run's own).
    let other = add_profile(&f, "openai", "codex", "codex-other");
    seed(&other, 92.0);
    set_reserve(&f, "0");
    assert_eq!(profiles_with_quota(&f, &task, &run), ["codex-other"]);
    set_reserve(&f, "10");
    assert_eq!(profiles_with_quota(&f, &task, &run), Vec::<String>::new());

    seed(&cfg, 97.0);
    set_reserve(&f, "5");
    assert_refused(&f, &task, &["resume", &run, "--json"], 5, "97", "implementer");
    assert_eq!(task_status(&f, &task), "interrupted");

    seed_reading(&cfg, 97.0, Some(PAST), PAST);
    let mut c = cmd(&f);
    c.env("FAKE_SCENARIO", "resume").env("FAKE_TASK_ID", &task).args(["resume", &run, "--json"]);
    let v = ok_json(c, "resume");
    assert_note(&v, 5);
    assert_eq!(wait_for(&f, &task, "180s").1, "in_review", "{}", explain(&f, &task));
}

// (g): recommend --------------------------------------------------------------------

fn recommend(f: &Fixture, role: &str) -> Value {
    let mut c = cmd(f);
    c.args(["recommend", "--role", role, "--json"]);
    ok_json(c, "recommend")
}

fn candidate(v: &Value, id: &str) -> Value {
    v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["profile_id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no candidate {id}: {v}"))
}

#[test]
fn recommend_marks_a_blocked_account_and_says_why() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    add_profile(&f, "anthropic", "claude-code", "claude-a");
    seed(&cfg, 92.0);
    set_reserve(&f, "10");
    for role in ["implementer", "reviewer"] {
        let v = recommend(&f, role);
        assert_eq!(v["quota_reserve_percent"], 10, "{v}");
        let c = candidate(&v, BLOCKED);
        assert_eq!(c["reserve_blocked"], true, "{c}");
        assert_eq!(c["eligible"], false, "{c}");
        let reason = c["reasons"]
            .as_array()
            .unwrap()
            .iter()
            .map(s)
            .find(|r| r.contains("quota-reserve-percent 10"))
            .unwrap_or_else(|| panic!("no reserve reason: {c}"));
        for part in ["five_hour", "92%", OBSERVED, "90%"] {
            assert!(reason.contains(part), "missing {part:?} in {reason}");
        }
        // No reading is not a block.
        assert_eq!(candidate(&v, "claude-a")["reserve_blocked"], false, "{v}");
    }
    let human = String::from_utf8(run_ok({
        let mut c = cmd(&f);
        c.args(["recommend", "--role", "implementer"]);
        c
    }, "recommend")).unwrap();
    let line = human
        .lines()
        .find(|l| l.trim_start().starts_with("reasons:") && l.contains("quota-reserve-percent 10"))
        .unwrap_or_else(|| panic!("no reasons line with the reserve: {human}"));
    assert!(line.contains("five_hour"), "{line}");

    seed(&cfg, 89.0);
    let c = candidate(&recommend(&f, "implementer"), BLOCKED);
    assert_eq!(c["reserve_blocked"], false, "{c}");
    assert_eq!(c["eligible"], true, "{c}");

    seed(&cfg, 92.0);
    set_reserve(&f, "0");
    let v = recommend(&f, "implementer");
    assert_eq!(v["quota_reserve_percent"], 0, "{v}");
    for c in v["candidates"].as_array().unwrap() {
        assert_eq!(c["reserve_blocked"], false, "{c}");
        assert!(!c["reasons"].to_string().contains("quota-reserve-percent"), "{c}");
    }
    assert_eq!(candidate(&v, BLOCKED)["eligible"], true, "{v}");
}

// (i): the setting ------------------------------------------------------------------

fn config(f: &Fixture, args: &[&str]) -> std::process::Output {
    cmd(f).arg("config").args(args).output().unwrap()
}

fn listed(f: &Fixture) -> Value {
    let out = config(f, &["list", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    let v = json_of(&out.stdout);
    let settings = v["settings"].as_array().unwrap();
    let at = settings
        .iter()
        .position(|s| s["key"] == "quota-reserve-percent")
        .unwrap_or_else(|| panic!("no quota-reserve-percent: {v}"));
    assert_eq!(settings[at - 1]["key"], "live-quota-timeout-ms", "{v}");
    settings[at].clone()
}

fn get(f: &Fixture) -> Value {
    let out = config(f, &["get", "quota-reserve-percent", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    json_of(&out.stdout)
}

#[test]
fn the_setting_is_listed_validated_and_a_bad_file_reads_as_off() {
    let f = common::fixture();
    let s0 = listed(&f);
    assert_eq!(s0["kind"], "integer");
    assert!(s0["env"].is_null());
    assert_eq!(s0["default"], "0");
    assert_eq!(s0["settable"], true);
    assert_eq!(s0["value"], "0");
    assert_eq!(s0["source"], "default");
    let description = s(&s0["description"]);
    assert!(description.contains("100 minus this number") && description.contains("0 turns it off"), "{description}");

    let out = config(&f, &["set", "quota-reserve-percent", "5", "--json"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(json_of(&out.stdout)["value"], "5");
    assert_eq!(get(&f), json!({"key": "quota-reserve-percent", "value": "5"}));
    assert_eq!(listed(&f)["source"], "file");

    let config_file = f.home_path().join("config.json");
    let before = fs::read_to_string(&config_file).unwrap();
    let bad: [&[&str]; 5] = [
        &["set", "quota-reserve-percent", "100", "--json"],
        &["set", "quota-reserve-percent", "--json", "--", "-1"],
        &["set", "quota-reserve-percent", "abc", "--json"],
        &["set", "quota-reserve-percent", "5.5", "--json"],
        &["set", "quota-reserve-percent", "", "--json"],
    ];
    for args in bad {
        let out = config(&f, args);
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let e = s(&json_of(&out.stderr)["error"]);
        assert!(e.contains("quota-reserve-percent") && e.contains("0 to 99"), "{args:?}: {e}");
        assert_eq!(fs::read_to_string(&config_file).unwrap(), before, "{args:?}");
    }

    let out = config(&f, &["set", "quota-reserve-percent", "0", "--json"]);
    assert_eq!(json_of(&out.stdout)["value"], "0");
    assert_eq!(get(&f)["value"], "0");
    assert_eq!(listed(&f)["source"], "file");

    let out = config(&f, &["unset", "quota-reserve-percent", "--json"]);
    let u = json_of(&out.stdout);
    assert_eq!((u["value"].clone(), u["source"].clone()), (json!("0"), json!("default")), "{u}");

    for stored in [json!(150), json!("abc"), json!(-3), json!(5.5)] {
        fs::write(&config_file, json!({"quota_reserve_percent": stored}).to_string()).unwrap();
        assert_eq!(get(&f)["value"], "0", "{stored}");
        assert_eq!(listed(&f)["source"], "default", "{stored}");
    }

    // live-quota-timeout-ms keeps its own rule.
    let out = config(&f, &["set", "live-quota-timeout-ms", "0", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(s(&json_of(&out.stderr)["error"]).contains("at least 1"));
    let out = config(&f, &["set", "live-quota-timeout-ms", "1", "--json"]);
    assert_eq!(out.status.code(), Some(0));
}

/// A setting stored as a number outside 0 to 99 never refuses a launch.
#[test]
fn a_bad_stored_reserve_never_refuses() {
    let f = init();
    let cfg = add_profile(&f, "openai", "codex", BLOCKED);
    let (task, _) = create_task(&f, Fixture::SPEC);
    seed(&cfg, 99.0);
    fs::write(f.home_path().join("config.json"), json!({"quota_reserve_percent": 150}).to_string()).unwrap();
    let d = dispatch_ok(&f, &task);
    assert!(reserve_notes(&d).is_empty(), "{d}");
}
