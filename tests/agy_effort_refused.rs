//! AGY-1 (second part): agy refuses `--effort` for some models it lists
//! (`claude-sonnet-4-6` on agy 1.5.14). The runner relaunches such a launch
//! at once without the effort, records the refusal on the profile, and every
//! later launch of that model on that profile sends no effort and needs none;
//! `thoth models` and `thoth recommend` show it, `profile forget-model` clears
//! it. Everything runs on fake-agy and a temporary THOTH_HOME, and is read
//! through the thoth CLI with --json.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::{Path, PathBuf};

const REFUSING: &str = "claude-sonnet-4-6";
const ACCEPTING: &str = "claude-opus-4-6-thinking";
const CARRIED: &str = "gemini-3.8-flash-high";
const CONV: &str = "11111111-2222-4333-8444-555555555555";

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn add_agy_profile(f: &Fixture, id: &str) -> PathBuf {
    let cfg = f.home_path().join("profiles").join(id);
    std::fs::create_dir_all(&cfg).unwrap();
    f.cmd()
        .args(["profile", "add", id, "--provider", "google", "--harness", "antigravity", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();
    cfg
}

fn args_log(f: &Fixture) -> PathBuf {
    f.home_path().join("fake-agy-args.jsonl")
}

fn attempts(f: &Fixture) -> PathBuf {
    f.home_path().join("fake-agy-attempts.txt")
}

/// The text fake-agy (and agy 1.5.14) answers a refused `--effort` with.
fn refusal_text(model: &str, effort: &str) -> String {
    format!(
        "invalid model selection (--model \"{model}\" --effort \"{effort}\"): --effort is not supported for model \"{model}\""
    )
}

/// A thoth command with fake-agy's environment for `profile`'s data dir.
fn agy_cmd(f: &Fixture, cfg: &Path, task: &str) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_TASK_ID", task)
        .env("FAKE_AGY_DATA_DIR", cfg)
        .env("FAKE_AGY_ARGS_LOG", args_log(f))
        .env("FAKE_ATTEMPTS_FILE", attempts(f))
        .env("FAKE_AGY_NO_EFFORT_MODELS", REFUSING)
        .env("FAKE_SCENARIO", "ok");
    c
}

fn dispatch(f: &Fixture, cfg: &Path, task: &str, args: &[&str], env: &[(&str, &str)]) -> Value {
    let mut c = agy_cmd(f, cfg, task);
    for (k, v) in env {
        c.env(k, v);
    }
    c.args(["dispatch", task, "--profile", "agy-a", "--reason", "AGY-1 test", "--json"]).args(args);
    serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap()
}

/// Every stream-json launch fake-agy logged, as its argument list, in order
/// (`agy models` and `-p /usage` are left out).
fn launches(f: &Fixture) -> Vec<Vec<String>> {
    let Ok(text) = std::fs::read_to_string(args_log(f)) else { return Vec::new() };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["args"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect::<Vec<_>>()
        })
        .filter(|args| args.iter().any(|a| a == "stream-json"))
        .collect()
}

fn after(args: &[String], flag: &str) -> Option<String> {
    args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned()
}

fn has(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// The details of the run's events of `ty`, in order, from `events --json`.
fn run_events(f: &Fixture, task: &str, run: &str, ty: &str) -> Vec<Value> {
    f.events_json(task)
        .into_iter()
        .filter(|e| e["run_id"] == run && e["type"] == ty)
        .map(|e| e["detail"].clone())
        .collect()
}

fn models_json(f: &Fixture) -> Value {
    let mut c = f.cmd();
    c.args(["models", "--json"]);
    serde_json::from_slice(&f.run_ok(c, "models --json")).unwrap()
}

/// `thoth models --json`'s entry for one model of `agy-a`.
fn model_entry(v: &Value, model: &str) -> Value {
    let profile = v["profiles"].as_array().unwrap().iter().find(|p| p["profile_id"] == "agy-a").unwrap();
    profile["models"].as_array().unwrap().iter().find(|m| m["model"] == model).cloned()
        .unwrap_or_else(|| panic!("agy-a has no model {model} in models --json: {profile}"))
}

/// The refusal run every test starts from: a dispatch of the refusing model
/// with `--effort medium`, waited on. Returns (task, run).
fn refuse_once(f: &Fixture, cfg: &Path) -> (String, String) {
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = dispatch(f, cfg, &task, &["--model", REFUSING, "--effort", "medium"], &[]);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    (task, run)
}

#[test]
fn a_refused_effort_is_relaunched_without_it_and_the_run_completes() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = dispatch(&f, &cfg, &task, &["--model", REFUSING, "--effort", "medium"], &[]);
    // printed before agy answered
    assert_eq!(d["effort"], "medium", "{d}");
    assert_eq!(d["effort_requested"], "medium", "{d}");
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    let l = launches(&f);
    assert_eq!(l.len(), 2, "{l:?}");
    assert_eq!(after(&l[0], "--model").as_deref(), Some(REFUSING), "{:?}", l[0]);
    let at = l[0].iter().position(|a| a == "--model").unwrap();
    assert_eq!(&l[0][at + 2..at + 4], ["--effort", "medium"], "{:?}", l[0]);
    assert!(!has(&l[1], "--effort"), "{:?}", l[1]);
    assert!(!has(&l[1], "--conversation"), "{:?}", l[1]);
    assert_eq!(after(&l[1], "--model").as_deref(), Some(REFUSING), "{:?}", l[1]);
    assert_eq!(after(&l[1], "-p"), after(&l[0], "-p"), "the same prompt");

    let refused = run_events(&f, &task, &run, "effort_refused");
    assert_eq!(refused.len(), 1, "{refused:?}");
    let r = &refused[0];
    assert_eq!(r["error"], refusal_text(REFUSING, "medium"), "{r}");
    assert_eq!(r["profile"], "agy-a", "{r}");
    assert_eq!(r["model"], REFUSING, "{r}");
    assert_eq!(r["effort"], "medium", "{r}");
    assert!(r["worker_pid"].as_i64().is_some_and(|p| p > 0), "{r}");
    assert!(r["worker_image"].is_string(), "{r}");

    assert!(run_events(&f, &task, &run, "run_retry").is_empty());
    let changes = run_events(&f, &task, &run, "status_change");
    let last = changes.last().unwrap();
    assert_eq!(last["retries"], 0, "{last}");

    let s = f.run_of(&task, &run);
    assert_eq!(s["status"], "completed", "{s}");
    assert!(s["effort"].is_null(), "{s}");
    assert_eq!(s["effort_requested"], "medium", "{s}");
    assert_eq!(
        s["effort_note"],
        format!("agy refused --effort medium for {REFUSING}; sent without an effort"),
        "{s}"
    );
    assert_eq!(f.task_status(&task), "in_review");
}

#[test]
fn a_later_launch_of_a_refused_model_needs_no_effort_and_sends_none() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (_, run1) = refuse_once(&f, &cfg);
    let seen_at = model_entry(&models_json(&f), REFUSING)["no_effort"]["seen_at"].as_str().unwrap().to_string();
    let note = format!("{REFUSING} takes no effort on profile agy-a: agy refused --effort at {seen_at} (run {run1})");

    // no --effort at all: accepted, nothing sent
    let before = launches(&f).len();
    let (task2, _) = f.create_task(Fixture::SPEC);
    let d = dispatch(&f, &cfg, &task2, &["--model", REFUSING], &[]);
    assert!(d["effort_requested"].is_null(), "{d}");
    assert!(d["effort"].is_null(), "{d}");
    assert_eq!(d["effort_note"], note, "{d}");
    assert!(d["warnings"].as_array().unwrap().iter().any(|w| w == &Value::String(note.clone())), "{d}");
    assert_eq!(d["timeout_seconds"], 3600, "{d}");
    let run2 = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task2, "120s").1, "in_review");
    let l = launches(&f);
    assert_eq!(l.len(), before + 1, "{l:?}");
    assert!(!has(&l[before], "--effort"), "{:?}", l[before]);
    assert!(run_events(&f, &task2, &run2, "effort_refused").is_empty());
    let started = run_events(&f, &task2, &run2, "run_started");
    assert!(started[0]["effort"].is_null(), "{started:?}");
    let choice = run_events(&f, &task2, &run2, "launch_choice");
    assert!(choice[0]["actual"]["effort"].is_null(), "{choice:?}");

    // --effort high: recorded as asked, not sent; the no-effort time limit
    let (task3, _) = f.create_task(Fixture::SPEC);
    let d = dispatch(&f, &cfg, &task3, &["--model", REFUSING, "--effort", "high"], &[]);
    assert_eq!(d["effort_requested"], "high", "{d}");
    assert!(d["effort"].is_null(), "{d}");
    assert_eq!(d["effort_note"], note, "{d}");
    assert_eq!(d["timeout_seconds"], 3600, "{d}");
    let run3 = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task3, "120s").1, "in_review");
    let l = launches(&f);
    assert_eq!(l.len(), before + 2, "{l:?}");
    assert!(!has(&l[before + 1], "--effort"), "{:?}", l[before + 1]);
    assert!(run_events(&f, &task3, &run3, "effort_refused").is_empty());
    let s = f.run_of(&task3, &run3);
    assert_eq!(s["effort_requested"], "high", "{s}");
    assert!(s["effort"].is_null(), "{s}");
    assert_eq!(s["effort_note"], note, "{s}");
}

#[test]
fn thoth_models_and_recommend_show_that_the_model_takes_no_effort() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (_, run1) = refuse_once(&f, &cfg);

    let v = models_json(&f);
    let entry = model_entry(&v, REFUSING);
    let ne = &entry["no_effort"];
    assert_eq!(ne["run_id"], run1.as_str(), "{entry}");
    assert_eq!(ne["effort"], "medium", "{entry}");
    assert_eq!(ne["reason"], refusal_text(REFUSING, "medium"), "{entry}");
    let seen_at = ne["seen_at"].as_str().expect("seen_at").to_string();
    assert!(entry["unavailable"].is_null(), "{entry}");
    for other in [ACCEPTING, CARRIED] {
        let e = model_entry(&v, other);
        assert!(e.get("no_effort").is_some_and(Value::is_null), "{e}");
    }

    let mut c = f.cmd();
    c.args(["models"]);
    let human = String::from_utf8(f.run_ok(c, "models")).unwrap();
    let line = human
        .lines()
        .find(|l| l.starts_with(&format!("  {REFUSING} (")))
        .unwrap_or_else(|| panic!("no line for {REFUSING}:\n{human}"));
    assert!(line.ends_with(&format!(" — no effort since {seen_at}")), "{line}");
    let other = human.lines().find(|l| l.starts_with(&format!("  {ACCEPTING} ("))).unwrap();
    assert!(!other.contains("no effort since"), "{other}");

    let mut c = agy_cmd(&f, &cfg, "none");
    c.args(["recommend", "--role", "implementer", "--json"]);
    let r: Value = serde_json::from_slice(&f.run_ok(c, "recommend --json")).unwrap();
    let cand = r["candidates"].as_array().unwrap().iter().find(|c| c["profile_id"] == "agy-a").unwrap();
    let rows = cand["models"].as_array().unwrap();
    let row = rows.iter().find(|m| m["model"] == REFUSING).unwrap_or_else(|| panic!("{cand}"));
    assert_eq!(row["no_effort"], *ne, "{row}");
    let row = rows.iter().find(|m| m["model"] == ACCEPTING).unwrap();
    assert!(row.get("no_effort").is_some_and(Value::is_null), "{row}");

    let mut c = agy_cmd(&f, &cfg, "none");
    c.env_remove("THOTH_LANG").args(["recommend", "--role", "implementer"]);
    let human = String::from_utf8(f.run_ok(c, "recommend")).unwrap();
    assert!(
        human.lines().any(|l| l == format!("  no effort: {REFUSING} since {seen_at}")),
        "{human}"
    );
}

#[test]
fn an_agy_model_that_accepts_an_effort_still_gets_it() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");

    let (task, _) = f.create_task(Fixture::SPEC);
    let d = dispatch(&f, &cfg, &task, &["--model", ACCEPTING, "--effort", "high"], &[]);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    let l = launches(&f);
    assert_eq!(l.len(), 1, "{l:?}");
    let at = l[0].iter().position(|a| a == "--model").unwrap();
    assert_eq!(&l[0][at + 1..at + 4], [ACCEPTING, "--effort", "high"], "{:?}", l[0]);
    assert!(run_events(&f, &task, &run, "effort_refused").is_empty());
    assert_eq!(f.run_of(&task, &run)["effort"], "high");

    // still needs --effort on its first launch
    let (task2, _) = f.create_task(Fixture::SPEC);
    let mut c = agy_cmd(&f, &cfg, &task2);
    c.args(["dispatch", &task2, "--profile", "agy-a", "--model", ACCEPTING, "--json"]);
    let out = c.output().unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--effort"), "{err}");
    assert_eq!(launches(&f).len(), 1);

    // a name that carries its level: no flag, the carried level recorded
    let (task3, _) = f.create_task(Fixture::SPEC);
    let d = dispatch(&f, &cfg, &task3, &["--model", CARRIED], &[]);
    assert_eq!(d["effort"], "high", "{d}");
    assert_eq!(f.wait_for(&task3, "120s").1, "in_review");
    let l = launches(&f);
    assert_eq!(l.len(), 2, "{l:?}");
    assert!(!has(&l[1], "--effort"), "{:?}", l[1]);

    // an unknown model: fails at once, no relaunch, no record (a fresh home,
    // so no stored model list refuses it before the launch)
    let g = setup();
    let gcfg = add_agy_profile(&g, "agy-a");
    let (task4, _) = g.create_task(Fixture::SPEC);
    let d = dispatch(&g, &gcfg, &task4, &["--model", "made-up-model", "--effort", "medium"], &[("FAKE_AGY_MODELS", "")]);
    let run4 = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(g.wait_for(&task4, "120s").1, "failed");
    let l = launches(&g);
    assert_eq!(l.len(), 1, "{l:?}");
    assert!(run_events(&g, &task4, &run4, "effort_refused").is_empty());
    let e = model_entry_any(&g, "made-up-model");
    assert!(e.as_ref().is_none_or(|e| e["no_effort"].is_null()), "{e:?}");
}

/// The agy-a entry for `model` in `models --json`, when it is listed at all.
fn model_entry_any(f: &Fixture, model: &str) -> Option<Value> {
    let v = models_json(f);
    let profile = v["profiles"].as_array()?.iter().find(|p| p["profile_id"] == "agy-a")?.clone();
    profile["models"].as_array()?.iter().find(|m| m["model"] == model).cloned()
}

#[test]
fn the_record_is_per_profile_and_another_agy_profile_still_sends_the_effort() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    refuse_once(&f, &cfg);
    let cfg_b = add_agy_profile(&f, "agy-b");
    let before = launches(&f).len();
    let (task, _) = f.create_task(Fixture::SPEC);
    let mut c = agy_cmd(&f, &cfg_b, &task);
    // agy-b's agy accepts the effort for this model
    c.env_remove("FAKE_AGY_NO_EFFORT_MODELS").args([
        "dispatch", &task, "--profile", "agy-b", "--model", REFUSING, "--effort", "low", "--json",
    ]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch agy-b")).unwrap();
    assert_eq!(d["effort"], "low", "{d}");
    assert!(d["effort_note"].is_null(), "{d}");
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    let l = launches(&f);
    assert_eq!(l.len(), before + 1, "{l:?}");
    assert_eq!(after(&l[before], "--effort").as_deref(), Some("low"), "{:?}", l[before]);
}

#[test]
fn forget_model_clears_the_no_effort_record() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    refuse_once(&f, &cfg);
    assert!(model_entry(&models_json(&f), REFUSING)["no_effort"].is_object());

    let mut c = f.cmd();
    c.args(["profile", "forget-model", "agy-a", REFUSING, "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(c, "forget-model")).unwrap();
    assert_eq!(v, serde_json::json!({"ok": true, "profile_id": "agy-a", "model": REFUSING}));

    assert!(model_entry(&models_json(&f), REFUSING)["no_effort"].is_null());
    let (task, _) = f.create_task(Fixture::SPEC);
    let mut c = agy_cmd(&f, &cfg, &task);
    c.args(["dispatch", &task, "--profile", "agy-a", "--model", REFUSING, "--json"]);
    let out = c.output().unwrap();
    assert!(!out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--effort is missing"), "{err}");
}

#[test]
fn a_failure_after_the_relaunch_is_not_relaunched_for_the_effort_and_resume_sends_none() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    // the refused launch takes no entry: the relaunch is launch 1 and crashes
    let d = dispatch(
        &f,
        &cfg,
        &task,
        &["--model", REFUSING, "--effort", "medium"],
        &[("FAKE_SCENARIO_SEQUENCE", "crash,ok")],
    );
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");
    let s = f.run_of(&task, &run);
    assert_eq!(s["status"], "interrupted", "{s}");
    assert_eq!(s["interrupted_reason"], "crash", "{s}");
    assert_eq!(run_events(&f, &task, &run, "effort_refused").len(), 1);
    assert!(run_events(&f, &task, &run, "run_retry").is_empty());
    assert_eq!(launches(&f).len(), 2);

    let mut c = agy_cmd(&f, &cfg, &task);
    c.env("FAKE_SCENARIO_SEQUENCE", "crash,ok").args(["resume", &run, "--json"]);
    f.run_ok(c, "resume");
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    let l = launches(&f);
    assert_eq!(l.len(), 3, "{l:?}");
    assert_eq!(after(&l[2], "--conversation").as_deref(), Some(CONV), "{:?}", l[2]);
    assert!(!has(&l[2], "--effort"), "{:?}", l[2]);
    assert_eq!(run_events(&f, &task, &run, "effort_refused").len(), 1);
}

// ---- SMALL-18 T3: the other launch commands and the pipeline defaults ----

const REASON: &str = "SMALL-18 test: fake workers in a temporary repo";
/// An `important` spec, as tests/effort.rs's EDITABLE (fake spec reviewers
/// propose edits to it).
const EDITABLE: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":["no new crate"],"non_goals":["rewriting the transport","changing the log format"],"excluded_alternatives":[],"risk":"important"}"#;

/// `thoth ARGS` with fake-agy's environment and `scenario`; its output.
fn run_agy(f: &Fixture, cfg: &Path, task: &str, scenario: &str, args: &[&str]) -> std::process::Output {
    let mut c = agy_cmd(f, cfg, task);
    c.env("FAKE_SCENARIO", scenario).args(args);
    c.output().unwrap()
}

fn json_of(out: &std::process::Output, what: &str) -> Value {
    assert_eq!(
        out.status.code(),
        Some(0),
        "{what}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap_or_else(|e| panic!("{what}: not JSON ({e})"))
}

fn cli_json(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    serde_json::from_slice(&f.run_ok(c, &args.join(" "))).unwrap()
}

fn wait_run(f: &Fixture, run: &str) -> Value {
    cli_json(f, &["wait", "--run", run, "--timeout", "180s", "--json"])
}

fn run_count(f: &Fixture, task: &str) -> usize {
    cli_json(f, &["status", task, "--json"])["runs"].as_array().unwrap().len()
}

/// The tasks the four launch commands start on, each set up to the point
/// where only the launch is left.
struct Ready {
    /// in_review, implemented by claude-b: for `review start`.
    review: String,
    /// in_review with a fix claude-a attached and a green verify of it: for
    /// `review confirm`.
    confirm: String,
    /// a new task: for `spec review start`.
    spec_new: String,
    /// edits claude-b proposed, pending: for `spec review confirm`.
    spec_edits: String,
}

/// A task of SPEC_TWO_AC implemented by claude-b, at in_review.
fn claude_in_review(f: &Fixture) -> String {
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    let d = f.dispatch(&task, "claude-b", "ok", &[]);
    let w = wait_run(f, d["run_id"].as_str().unwrap());
    assert_eq!(w["task_status"], "in_review", "{w}");
    task
}

/// claude-a, claude-b, a green approved check and src/greet.rs, then the
/// four tasks of `Ready`, as tests/review_fix.rs and tests/spec_review_edit.rs
/// set them up.
fn ready(f: &Fixture) -> Ready {
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let repo = f.repo_path();
    std::fs::create_dir_all(repo.join("src")).unwrap();
    let greet: String = (1..=30).map(|i| format!("line {i}\n")).collect();
    std::fs::write(repo.join("src/greet.rs"), greet).unwrap();
    std::fs::write(repo.join(".gitattributes"), "* -text\n").unwrap();
    common::git(&repo, &["add", "-A"]);
    common::git(&repo, &["commit", "-m", "candidate files"]);
    cli_json(f, &["checks", "add", "green", "--cmd", "git --version", "--shell", "argv", "--json"]);
    cli_json(f, &["checks", "approve", "--reason", REASON, "--json"]);

    let review = claude_in_review(f);

    let confirm = claude_in_review(f);
    let evidence = cli_json(f, &["evidence", "list", &confirm, "--json"]);
    let code = evidence["evidence"][0]["code"].as_str().unwrap().to_string();
    let plan = serde_json::json!({"document": {
        "verdict": "REJECT",
        "findings": {"criteria": [
            {"id": "AC1", "verdict": "unmet", "evidence": [code], "note": "the greeting is wrong", "category": "spec_mismatch", "fix_size": "S"},
            {"id": "AC2", "verdict": "met", "evidence": [code]}
        ], "extra_findings": []},
        "edits": [{"path": "src/greet.rs", "old": "line 1\n", "new": "line one\n", "criterion": "AC1", "reason": "fixes the finding"}],
    }});
    let plan_file = f.home_path().join("plan.json");
    std::fs::write(&plan_file, plan.to_string()).unwrap();
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "review_fix").env("FAKE_TASK_ID", &confirm).env("FAKE_FIX_PLAN", &plan_file).args([
        "review", "start", &confirm, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--edit",
        "--reason", REASON, "--json",
    ]);
    let edit: Value = serde_json::from_slice(&f.run_ok(c, "review start --edit")).unwrap();
    let edit = edit["run_id"].as_str().unwrap().to_string();
    let w = wait_run(f, &edit);
    assert_eq!(w["pending_fix"], true, "{w}");
    cli_json(f, &["verify", &edit, "--json"]);

    let (spec_new, _) = f.create_task(EDITABLE);

    let (spec_edits, _) = f.create_task(EDITABLE);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "spec_review_edit").env("FAKE_TASK_ID", &spec_edits).args([
        "spec", "review", "start", &spec_edits, "--profile", "claude-b", "--model", "haiku", "--edit", "--reason",
        REASON, "--json",
    ]);
    let r: Value = serde_json::from_slice(&f.run_ok(c, "spec review start --edit")).unwrap();
    assert_eq!(wait_run(f, r["run_id"].as_str().unwrap())["verdict"], "REVISE");
    assert_eq!(cli_json(f, &["spec", "review", "show", &spec_edits, "--json"])["pending_edits"], true);

    Ready { review, confirm, spec_new, spec_edits }
}

/// The four launches of agy-a's refusing model without --effort: (task,
/// fake-agy scenario, arguments).
fn no_effort_launches(r: &Ready) -> Vec<(String, &'static str, Vec<String>)> {
    let tail = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    vec![
        (r.review.clone(), "review_pass", tail(&["review", "start", &r.review, "--profile", "agy-a", "--model", REFUSING, "--json"])),
        (r.confirm.clone(), "fix_confirm_revise", tail(&["review", "confirm", &r.confirm, "--profile", "agy-a", "--model", REFUSING, "--json"])),
        (r.spec_new.clone(), "spec_review_approve", tail(&["spec", "review", "start", &r.spec_new, "--profile", "agy-a", "--model", REFUSING, "--json"])),
        (r.spec_edits.clone(), "spec_confirm_revise", tail(&["spec", "review", "confirm", &r.spec_edits, "--profile", "agy-a", "--model", REFUSING, "--json"])),
    ]
}

const DEFAULTS_SET: [&str; 9] = [
    "pipeline",
    "defaults",
    "set",
    "--implementer",
    "agy-a:claude-sonnet-4-6",
    "--reviewer",
    "agy-a:claude-sonnet-4-6",
    "--reason",
    "r",
];

#[test]
fn every_launch_command_and_the_pipeline_defaults_take_a_remembered_model_without_an_effort() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (_, run1) = refuse_once(&f, &cfg);
    let seen_at = model_entry(&models_json(&f), REFUSING)["no_effort"]["seen_at"].as_str().unwrap().to_string();
    let note = format!("{REFUSING} takes no effort on profile agy-a: agy refused --effort at {seen_at} (run {run1})");
    let r = ready(&f);

    for (task, scenario, args) in no_effort_launches(&r) {
        let what = args[..3].join(" ");
        let before = launches(&f).len();
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let v = json_of(&run_agy(&f, &cfg, &task, scenario, &argv), &what);
        assert!(v["effort"].is_null(), "{what}: {v}");
        assert_eq!(v["effort_note"], note, "{what}: {v}");
        wait_run(&f, v["run_id"].as_str().unwrap());
        let l = launches(&f);
        assert_eq!(l.len(), before + 1, "{what}: {l:?}");
        assert_eq!(after(&l[before], "--model").as_deref(), Some(REFUSING), "{what}: {:?}", l[before]);
        assert!(!has(&l[before], "--effort"), "{what}: {:?}", l[before]);
    }

    let mut c = f.cmd();
    c.args(DEFAULTS_SET).arg("--json");
    let set: Value = serde_json::from_slice(&f.run_ok(c, "pipeline defaults set")).unwrap();
    assert_eq!(set["ok"], true, "{set}");
    let shown = cli_json(&f, &["pipeline", "defaults", "show", "--json"]);
    for list in ["implementers", "reviewers"] {
        let entry = &shown[list]["entries"][0];
        assert_eq!(entry["profile"], "agy-a", "{shown}");
        assert_eq!(entry["model"], REFUSING, "{shown}");
        assert!(entry["effort"].is_null(), "{shown}");
    }
}

#[test]
fn without_the_record_every_launch_command_and_the_pipeline_defaults_need_an_effort() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let r = ready(&f);

    for (task, scenario, args) in no_effort_launches(&r) {
        let what = args[..3].join(" ");
        let before = run_count(&f, &task);
        let launched = launches(&f).len();
        let argv: Vec<&str> = args.iter().map(String::as_str).collect();
        let out = run_agy(&f, &cfg, &task, scenario, &argv);
        let err = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{what}: {err}");
        assert!(err.contains("--effort is missing"), "{what}: {err}");
        assert_eq!(run_count(&f, &task), before, "{what} started a run");
        assert_eq!(launches(&f).len(), launched, "{what} launched agy");
    }

    let out = f.cmd().args(DEFAULTS_SET).arg("--json").output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("needs an effort"), "{text}");
    let shown = cli_json(&f, &["pipeline", "defaults", "show", "--json"]);
    assert!(shown["implementers"].is_null() && shown["reviewers"].is_null(), "{shown}");
}

#[test]
fn a_refusal_that_cannot_be_recorded_is_logged_and_the_run_still_completes() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = dispatch(&f, &cfg, &task, &["--model", REFUSING, "--effort", "medium"], &[("THOTH_TEST_FAIL_EFFORT_RECORD", "1")]);
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    assert_eq!(run_events(&f, &task, &run, "effort_refused").len(), 1);

    let s = f.run_of(&task, &run);
    assert_eq!(s["status"], "completed", "{s}");
    let run_dir = PathBuf::from(s["run_dir"].as_str().unwrap_or_else(|| panic!("no run_dir: {s}")));
    let log = std::fs::read_to_string(run_dir.join("runner-error.log")).unwrap_or_default();
    assert!(log.lines().any(|l| l.contains("recording the refused effort failed: ")), "{log}");
    assert!(model_entry(&models_json(&f), REFUSING)["no_effort"].is_null());
}
