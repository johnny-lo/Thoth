//! RJ-1: Thoth only records and dispatches; it no longer picks the profile,
//! model or effort. `auto` is refused, the first launch gives `--model` and
//! `--effort` (a reroute in the same attempt keeps the previous run's), every
//! launch writes a `launch_choice` record, `require-reason` asks for a reason
//! on every launch, `recommend` lists account facts only, a quota stop points
//! to the profiles that still have quota, and the retired
//! `same-provider-rotation` key is ignored. Always the fakes (`fake-claude`,
//! `fake-codex`); nothing here reaches a network or a real harness.
//!
//! Every check reads back through `thoth` itself (`status`, `events`,
//! `profile list`, `profile usage`, `recommend`), never the databases. Only
//! the setup no command can make (a parked profile with a reset window) is
//! written straight into them, with helpers copied from
//! tests/failure_classes.rs, because those files are not modules.

mod common;

use common::Fixture;
use rusqlite::params;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

const CODEX_MODEL: &str = "gpt-5.6-luna";

// --- copied from tests/failure_classes.rs ----------------------------------------

fn at(delta: Duration) -> String {
    (OffsetDateTime::now_utc() + delta).format(&Rfc3339).unwrap()
}

fn epoch(delta: Duration) -> i64 {
    (OffsetDateTime::now_utc() + delta).unix_timestamp()
}

fn add_event(f: &Fixture, run_id: &str, profile: &str, ts: &str, detail: Value) {
    let conn = f.state_conn();
    conn.execute(
        "INSERT INTO events (ts, run_id, type, detail) VALUES (?1, ?2, 'rate_limit_warning', ?3)",
        params![ts, run_id, detail.to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT OR IGNORE INTO runs (run_id, account, status, role) VALUES (?1, ?2, 'completed', 'implementer')",
        params![run_id, profile],
    )
    .unwrap();
}

fn park(f: &Fixture, id: &str, since: &str) {
    let g = f.global_conn();
    g.execute("UPDATE profiles SET status='rate_limited' WHERE profile_id=?1", [id]).unwrap();
    g.execute(
        "INSERT INTO quota_events (ts, profile_id, kind, detail) VALUES (?1, ?2, 'rate_limited', '{}')",
        params![since, id],
    )
    .unwrap();
}

// --- this file's own -----------------------------------------------------------

fn init() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn json_out(c: &mut assert_cmd::Command) -> Value {
    let out = c.assert().success().get_output().stdout.clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// A command with the fake's scenario and task set.
fn envd(f: &Fixture, scenario: &str, task_id: &str) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario).env("FAKE_TASK_ID", task_id);
    c
}

/// `ARGS... --json`, which must exit 0.
fn launch_ok(f: &Fixture, task_id: &str, scenario: &str, args: &[&str]) -> Value {
    let mut c = envd(f, scenario, task_id);
    c.args(args).arg("--json");
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// `ARGS...` that must exit 1; returns stderr.
fn launch_refused(f: &Fixture, task_id: &str, args: &[&str]) -> String {
    let out = envd(f, "ok", task_id).args(args).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert_eq!(
        out.status.code(),
        Some(1),
        "{args:?}\nstdout: {}\nstderr: {stderr}",
        String::from_utf8_lossy(&out.stdout)
    );
    stderr
}

fn status_json(f: &Fixture, task_id: &str) -> Value {
    json_out(f.cmd().args(["status", task_id, "--json"]))
}

/// The task's status, from `status --json`.
fn task_status(f: &Fixture, task_id: &str) -> String {
    let s = status_json(f, task_id);
    s["status"].as_str().unwrap_or_else(|| panic!("no status: {s}")).to_string()
}

/// Polls `status --json` until the task is in one of `wanted`, for at most
/// `secs` seconds.
fn wait_status(f: &Fixture, task_id: &str, wanted: &[&str], secs: u64) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    loop {
        let s = task_status(f, task_id);
        if wanted.contains(&s.as_str()) {
            return s;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "task {task_id} never reached any of {wanted:?} within {secs}s; it is '{s}'"
        );
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// The task's events, from `events --task TASK --json`.
fn events(f: &Fixture, task_id: &str) -> Vec<Value> {
    let v = json_out(f.cmd().args(["events", "--task", task_id, "--json"]));
    v["events"].as_array().unwrap_or_else(|| panic!("no events: {v}")).clone()
}

fn event_types(f: &Fixture, task_id: &str) -> Vec<String> {
    events(f, task_id)
        .iter()
        .map(|e| e["type"].as_str().unwrap().to_string())
        .collect()
}

/// The profile's status, from `profile list --json`.
fn profile_status(f: &Fixture, id: &str) -> String {
    let v = json_out(f.cmd().args(["profile", "list", "--json"]));
    v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == id)
        .unwrap_or_else(|| panic!("no profile {id}: {v}"))["status"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The profile's `recommend --role ROLE --task TASK --json` candidate.
fn candidate(f: &Fixture, role: &str, task_id: &str, id: &str) -> Value {
    let v = json_out(f.cmd().args(["recommend", "--role", role, "--task", task_id, "--json"]));
    v["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["profile_id"] == id)
        .unwrap_or_else(|| panic!("no candidate {id}: {v}"))
        .clone()
}

/// What a refusal must leave as it was: the task's runs (from `status
/// --json`) and the newest event seq (from `events --json`).
fn written(f: &Fixture, task_id: &str) -> (Value, Value) {
    let runs = status_json(f, task_id)["runs"].clone();
    let seq = json_out(f.cmd().args(["events", "--json"]))["last_seq"].clone();
    (runs, seq)
}

fn wait_run(f: &Fixture, run_id: &str) {
    let mut c = f.cmd();
    c.args(["wait", "--run", run_id, "--timeout", "180s"]);
    f.run_ok(c, "wait --run");
}

fn run_id_of(d: &Value) -> String {
    d["run_id"].as_str().unwrap().to_string()
}

fn warnings(v: &Value) -> Vec<String> {
    v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect()
}

/// The detail of the run's one `launch_choice` event, from `events --json`.
fn choice(f: &Fixture, task_id: &str, run_id: &str) -> Value {
    let rows: Vec<Value> = events(f, task_id)
        .into_iter()
        .filter(|e| e["run_id"] == run_id && e["type"] == "launch_choice")
        .collect();
    assert_eq!(rows.len(), 1, "{rows:?}\n{}", f.explain_run(run_id));
    rows[0]["detail"].clone()
}

/// The status `--json` row of one run.
fn status_run(f: &Fixture, task_id: &str, run_id: &str) -> Value {
    let s = status_json(f, task_id);
    s["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run_id)
        .unwrap_or_else(|| panic!("no run {run_id}: {s}"))
        .clone()
}

fn require_reason(f: &Fixture, value: &str) {
    f.cmd()
        .args(["config", "set", "require-reason", value, "--json"])
        .assert()
        .success();
}

/// A codex `models_cache.json` that lists `CODEX_MODEL` with its levels.
fn write_codex_cache(cfg: &Path) {
    fs::write(
        cfg.join("models_cache.json"),
        json!({"models":[{"slug":CODEX_MODEL,"display_name":CODEX_MODEL,"visibility":"list","priority":1,"supported_reasoning_levels":[{"effort":"low"},{"effort":"medium"},{"effort":"high"}]}]})
            .to_string(),
    )
    .unwrap();
}

const DISPATCH: &str = "dispatch";

// (a) -------------------------------------------------------------------------

#[test]
fn auto_values_are_refused_and_write_nothing() {
    let f = init();
    f.add_profile("claude-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let before = written(&f, &task);
    let cases: [&[&str]; 6] = [
        &[DISPATCH, &task, "--profile", "auto", "--model", "sonnet", "--effort", "medium"],
        &[DISPATCH, &task, "--profile", "claude-a", "--model", "auto", "--effort", "medium"],
        &[DISPATCH, &task, "--profile", "claude-a", "--model", " Auto ", "--effort", "medium"],
        &[DISPATCH, &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "AUTO"],
        // the task is queued: the auto refusal comes before review start's
        // status check
        &["review", "start", &task, "--profile", "auto", "--model", "opus", "--effort", "high"],
        &["spec", "review", "start", &task, "--profile", "auto", "--model", "opus", "--effort", "high"],
    ];
    for args in cases {
        let stderr = launch_refused(&f, &task, args);
        assert!(stderr.contains("thoth recommend"), "{args:?}: {stderr}");
        assert!(stderr.contains("no longer picks"), "{args:?}: {stderr}");
        assert!(!stderr.contains("invalid effort level"), "{args:?}: {stderr}");
        assert!(!stderr.contains("needs in_review"), "{args:?}: {stderr}");
    }
    assert_eq!(written(&f, &task), before);
    assert_eq!(task_status(&f, &task), "queued");
}

// (b) -------------------------------------------------------------------------

#[test]
fn a_first_launch_needs_model_and_effort() {
    let f = init();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);

    let refusals = |prefix: &[&str], again: &str| {
        let before = written(&f, &task);
        for (extra, missing) in [
            (&["--effort", "medium"][..], "--model is missing"),
            (&["--model", "sonnet"][..], "--effort is missing"),
            (&[][..], "--model and --effort are missing"),
        ] {
            let mut args = prefix.to_vec();
            args.extend(extra);
            let stderr = launch_refused(&f, &task, &args);
            assert!(stderr.contains(missing), "{args:?}: {stderr}");
            assert!(stderr.contains("no longer picks"), "{args:?}: {stderr}");
            assert!(stderr.contains("thoth recommend"), "{args:?}: {stderr}");
            assert!(stderr.contains(again), "{args:?}: {stderr}");
        }
        assert_eq!(written(&f, &task), before, "{prefix:?}");
    };
    let same_attempt = "a reroute in the same attempt keeps the previous run's";

    // the first dispatch
    refusals(&[DISPATCH, &task, "--profile", "claude-a"], same_attempt);
    let d = launch_ok(&f, &task, "hooks", &[DISPATCH, &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium"]);
    assert_eq!(f.wait_for(&task, "180s").1, "in_review", "{}", f.explain_run(&run_id_of(&d)));

    // the first review of the attempt, and a spec review (every time)
    refusals(&["review", "start", &task, "--profile", "claude-b"], same_attempt);
    refusals(&["spec", "review", "start", &task, "--profile", "claude-b"], "spec review start needs them every time");

    // a rework after a REJECT is a new attempt: nothing to keep
    let r = launch_ok(&f, &task, "review_reject", &["review", "start", &task, "--profile", "claude-b", "--model", "haiku"]);
    wait_run(&f, &run_id_of(&r));
    assert_eq!(wait_status(&f, &task, &["pass", "reject", "unverified"], 30), "reject");
    refusals(&[DISPATCH, &task, "--profile", "claude-a"], same_attempt);

    // a model without an effort setting needs no --effort
    let d = launch_ok(&f, &task, "ok", &[DISPATCH, &task, "--profile", "claude-a", "--model", "haiku"]);
    let run = run_id_of(&d);
    assert_eq!(d["attempt"], 2, "{d}");
    assert!(d["effort_requested"].is_null(), "{d}");
    f.wait_for(&task, "180s");
    assert!(status_run(&f, &task, &run)["effort_requested"].is_null());
    assert!(choice(&f, &task, &run)["actual"]["effort_requested"].is_null());
}

// (c) -------------------------------------------------------------------------

#[test]
fn a_reroute_in_the_same_attempt_keeps_model_and_effort() {
    let f = init();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    f.add_profile("claude-c");
    let (task, _) = f.create_task(Fixture::SPEC);

    // dispatch: the first run stops on quota
    let first = launch_ok(&f, &task, "quota", &[DISPATCH, &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "high"]);
    let first = run_id_of(&first);
    assert_eq!(f.wait_for(&task, "180s").1, "interrupted");
    let d = launch_ok(&f, &task, "hooks", &[DISPATCH, &task, "--profile", "claude-b"]);
    let second = run_id_of(&d);
    assert_eq!(d["model"], "sonnet", "{d}");
    assert_eq!(d["effort_requested"], "high", "{d}");
    assert!(
        warnings(&d).iter().any(|w| w.contains(&format!("kept from run {first}"))),
        "{d}"
    );
    assert_eq!(f.wait_for(&task, "180s").1, "in_review", "{}", f.explain_run(&second));
    let row = status_run(&f, &task, &second);
    assert_eq!(row["model"], "sonnet", "{row}");
    assert_eq!(row["effort_requested"], "high", "{row}");
    let c = choice(&f, &task, &second);
    assert_eq!(c["inherited"], json!({"model": true, "effort": true}), "{c}");
    assert_eq!(c["previous_run"], first.as_str(), "{c}");

    // review start: the review of the same attempt, started again
    let r1 = launch_ok(&f, &task, "quota", &["review", "start", &task, "--profile", "claude-c", "--model", "opus", "--effort", "high"]);
    let r1 = run_id_of(&r1);
    wait_run(&f, &r1);
    assert_eq!(task_status(&f, &task), "in_review");
    let r2 = launch_ok(&f, &task, "review_pass", &["review", "start", &task, "--profile", "claude-b"]);
    assert_eq!(r2["model"], "opus", "{r2}");
    assert_eq!(r2["effort_requested"], "high", "{r2}");
    assert!(warnings(&r2).iter().any(|w| w.contains(&format!("kept from run {r1}"))), "{r2}");
    let r2 = run_id_of(&r2);
    wait_run(&f, &r2);
    let c = choice(&f, &task, &r2);
    assert_eq!(c["inherited"], json!({"model": true, "effort": true}), "{c}");
    assert_eq!(c["previous_run"], r1.as_str(), "{c}");

    // a model kept across harnesses is refused: codex -> claude
    let cfg = f.add_codex_profile("codex-a");
    write_codex_cache(&cfg);
    let (task2, _) = f.create_task(Fixture::SPEC);
    let x = launch_ok(&f, &task2, "quota", &[DISPATCH, &task2, "--profile", "codex-a", "--model", CODEX_MODEL, "--effort", "medium"]);
    assert_eq!(f.wait_for(&task2, "180s").1, "interrupted", "{}", f.explain_run(&run_id_of(&x)));
    let before = written(&f, &task2);
    let stderr = launch_refused(&f, &task2, &[DISPATCH, &task2, "--profile", "claude-a"]);
    assert!(stderr.contains("--model"), "{stderr}");
    assert!(stderr.contains("another harness"), "{stderr}");
    assert_eq!(written(&f, &task2), before);
}

// (d) -------------------------------------------------------------------------

#[test]
fn the_launch_choice_event_is_a_record() {
    let f = init();
    f.add_profile("claude-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = launch_ok(&f, &task, "ok", &[DISPATCH, &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium"]);
    let run = run_id_of(&d);
    f.wait_for(&task, "180s");
    let c = choice(&f, &task, &run);
    let mut keys: Vec<&str> = c.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "actual", "difficulty", "inherited", "previous_run", "reason", "require_reason", "risk",
            "role", "source", "task_type"
        ],
        "{c}"
    );
    assert_eq!(c["role"], "implementer", "{c}");
    assert_eq!(c["source"], "dispatch", "{c}");
    assert!(c["task_type"].is_null(), "{c}");
    assert!(c["difficulty"].is_null(), "{c}");
    assert_eq!(c["risk"], "low", "{c}");
    assert_eq!(
        c["actual"],
        json!({"profile": "claude-a", "model": "sonnet", "effort_requested": "medium", "effort": "medium"}),
        "{c}"
    );
    assert_eq!(c["inherited"], json!({"model": false, "effort": false}), "{c}");
    assert!(c["previous_run"].is_null(), "{c}");
    assert!(c["reason"].is_null(), "{c}");
    assert_eq!(c["require_reason"], false, "{c}");
    let types = event_types(&f, &task);
    assert!(types.iter().any(|t| t == "launch_choice"), "{types:?}");
    assert!(!types.iter().any(|t| t == "auto_pick"), "{types:?}");
    // the dispatch --json has no automatic-pick keys
    for key in ["auto", "auto_reasons", "live_quota_refresh"] {
        assert!(d.get(key).is_none(), "{key}: {d}");
    }

    // a labelled task: the labels as written
    let spec = json!({"goal":"g","acceptance_criteria":["AC1: x"],"risk":"normal","task_type":"docs","difficulty":"high"});
    let (labelled, _) = f.create_task(&spec.to_string());
    let d = launch_ok(&f, &labelled, "ok", &[DISPATCH, &labelled, "--profile", "claude-a", "--model", "sonnet", "--effort", "low"]);
    let run = run_id_of(&d);
    f.wait_for(&labelled, "180s");
    let c = choice(&f, &labelled, &run);
    assert_eq!(c["task_type"], "docs", "{c}");
    assert_eq!(c["difficulty"], "high", "{c}");
    assert_eq!(c["risk"], "normal", "{c}");
}

// (e) -------------------------------------------------------------------------

#[test]
fn require_reason_is_needed_on_every_launch() {
    let f = init();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    require_reason(&f, "on");

    let refused = |args: &[&str]| {
        let before = written(&f, &task);
        let stderr = launch_refused(&f, &task, args);
        assert!(stderr.contains("--reason"), "{args:?}: {stderr}");
        assert!(stderr.contains("require-reason off"), "{args:?}: {stderr}");
        assert_eq!(written(&f, &task), before, "{args:?}");
    };

    // dispatch, the first launch
    refused(&[DISPATCH, &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium"]);
    let d = launch_ok(&f, &task, "quota", &[DISPATCH, &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium", "--reason", "first try"]);
    assert_eq!(f.wait_for(&task, "180s").1, "interrupted");
    assert_eq!(choice(&f, &task, &run_id_of(&d))["reason"], "first try");
    // a reroute in the same attempt needs one too
    refused(&[DISPATCH, &task, "--profile", "claude-b"]);
    let d = launch_ok(&f, &task, "hooks", &[DISPATCH, &task, "--profile", "claude-b", "--reason", "  claude-a is out of quota  "]);
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");
    let c = choice(&f, &task, &run_id_of(&d));
    assert_eq!(c["reason"], "claude-a is out of quota", "{c}");
    assert_eq!(c["require_reason"], true, "{c}");

    // review start
    refused(&["review", "start", &task, "--profile", "claude-a", "--model", "opus", "--effort", "high"]);
    let r = launch_ok(&f, &task, "review_pass", &["review", "start", &task, "--profile", "claude-a", "--model", "opus", "--effort", "high", "--reason", "other account"]);
    let r = run_id_of(&r);
    wait_run(&f, &r);
    assert_eq!(choice(&f, &task, &r)["reason"], "other account");

    // spec review start
    let (task2, _) = f.create_task(Fixture::SPEC);
    let before = written(&f, &task2);
    let stderr = launch_refused(&f, &task2, &["spec", "review", "start", &task2, "--profile", "claude-a", "--model", "opus", "--effort", "high"]);
    assert!(stderr.contains("--reason"), "{stderr}");
    assert_eq!(written(&f, &task2), before);
    let s = launch_ok(&f, &task2, "spec_review_approve", &["spec", "review", "start", &task2, "--profile", "claude-a", "--model", "opus", "--effort", "high", "--reason", "spec check"]);
    let s = run_id_of(&s);
    wait_run(&f, &s);
    let c = choice(&f, &task2, &s);
    assert_eq!(c["reason"], "spec check", "{c}");
    assert_eq!(c["source"], "spec_review_start", "{c}");

    // switched off: no reason needed, and none recorded
    require_reason(&f, "off");
    let (task3, _) = f.create_task(Fixture::SPEC);
    let d = launch_ok(&f, &task3, "ok", &[DISPATCH, &task3, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium"]);
    f.wait_for(&task3, "180s");
    let c = choice(&f, &task3, &run_id_of(&d));
    assert!(c["reason"].is_null(), "{c}");
    assert_eq!(c["require_reason"], false, "{c}");
}

// (f) -------------------------------------------------------------------------

#[test]
fn recommend_lists_account_facts_only() {
    let f = init();
    f.add_profile("claude-a");
    f.add_codex_profile("codex-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    for role in ["implementer", "reviewer"] {
        for given in [None, Some(&task[..12]), Some(task.as_str())] {
            let mut c = f.cmd();
            c.args(["recommend", "--role", role, "--json"]);
            if let Some(t) = given {
                c.args(["--task", t]);
            }
            let v = json_out(&mut c);
            let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
            keys.sort_unstable();
            // RJ-2: the filtered tables, the score refresh and (with --task)
            // the task's history join the account facts. CROSS-1: Thoth's own
            // usage moved under each candidate's models, `unmatched` is new
            // and `tables` is null without --tables.
            assert_eq!(
                keys,
                ["candidates", "live_quota_refresh", "model", "role", "score_refresh", "tables", "task_history", "task_id", "unmatched"],
                "{v}"
            );
            assert!(v["tables"].is_null(), "{v}");
            match given {
                Some(_) => {
                    assert_eq!(v["task_id"], task.as_str(), "{v}");
                    assert!(v["task_history"].is_object(), "{v}");
                }
                None => {
                    assert!(v["task_id"].is_null(), "{v}");
                    assert!(v["task_history"].is_null(), "{v}");
                }
            }
            let candidates = v["candidates"].as_array().unwrap();
            assert_eq!(candidates.len(), 2, "{v}");
            for c in candidates {
                // CROSS-1: `models` is back, as the account's models and
                // their matching scores, not RJ-1's picked models
                for m in c["models"].as_array().unwrap() {
                    let mut keys: Vec<&str> = m.as_object().unwrap().keys().map(String::as_str).collect();
                    keys.sort_unstable();
                    assert_eq!(keys, ["agent_arena", "ai_stupid_level", "artificial_analysis", "model", "names", "usage"], "{c}");
                }
                assert!(c.get("held").is_none(), "{c}");
                assert!(c.get("eligible").is_some(), "{c}");
                assert!(c.get("available_models").is_none(), "{c}");
            }

            let mut c = f.cmd();
            c.args(["recommend", "--role", role]);
            if let Some(t) = given {
                c.args(["--task", t]);
            }
            let out = c.assert().success().get_output().stdout.clone();
            let human = String::from_utf8(out).unwrap();
            assert!(!human.contains("launch:"), "{human}");
            assert!(!human.contains("selection:"), "{human}");
            assert!(human.contains("claude-a"), "{human}");
        }
        // an unknown task is an error for both roles
        f.cmd()
            .args(["recommend", "--role", role, "--task", "NOSUCHTASK", "--json"])
            .assert()
            .code(1);
    }
}

// (g) -------------------------------------------------------------------------

#[test]
fn a_quota_stop_points_to_other_profiles() {
    let f = init();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = launch_ok(&f, &task, "quota", &[DISPATCH, &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium"]);
    let run = run_id_of(&d);
    assert_eq!(f.wait_for(&task, "180s").1, "interrupted");
    // no handover: the task stays interrupted with its one run
    let s = status_json(&f, &task);
    assert_eq!(s["status"], "interrupted", "{s}");
    assert_eq!(s["runs"].as_array().unwrap().len(), 1, "{s}");
    let types = event_types(&f, &task);
    assert!(types.iter().any(|t| t == "launch_choice"), "{types:?}");
    for ty in ["handover", "handover_skipped", "handover_failed"] {
        assert!(!types.iter().any(|t| t == ty), "{ty}: {types:?}");
    }
    let row = &s["runs"][0];
    assert_eq!(row["interrupted_reason"], "quota", "{row}");
    // claude-b is not held back for being the same provider
    assert_eq!(row["profiles_with_quota"], json!(["claude-b"]), "{row}");
    let next = row["next_step"].as_str().unwrap();
    assert!(next.starts_with("quota used up on claude-a"), "{next}");
    assert!(next.contains("profiles with quota now: claude-b"), "{next}");
    // RJ-3: move on to a profile with quota now; never wait for a reset or
    // resume on the used-up profile
    assert!(next.contains(&format!("thoth dispatch {task} --profile OTHER --model sonnet --effort medium --reason TEXT")), "{next}");
    assert!(next.contains("do not wait for claude-a"), "{next}");
    assert!(!next.contains("thoth resume"), "{next}");
    assert!(!next.contains("window to reset"), "{next}");
    let out = f.cmd().args(["status", &task]).assert().success().get_output().stdout.clone();
    let human = String::from_utf8(out).unwrap();
    let line = human
        .lines()
        .find(|l| l.trim_start().starts_with("next:"))
        .unwrap_or_else(|| panic!("no next: line\n{human}"));
    assert!(line.contains("quota used up on claude-a"), "{line}");
    assert!(line.contains("claude-b"), "{line}");
    assert!(line.contains("thoth dispatch"), "{line}");
    assert!(line.contains("do not wait"), "{line}");
    assert!(line.contains("--reason TEXT"), "{line}");
    assert!(!line.contains("thoth resume"), "{line}");
    assert!(!line.contains(&format!("resume {run}")), "{line}");
    assert!(!line.contains("window to reset"), "{line}");
    assert!(line.ends_with(next), "{line}\n{next}");

    // the reminder followed: claude-b takes it over
    let d = launch_ok(&f, &task, "ok", &[DISPATCH, &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"]);
    let second = run_id_of(&d);
    f.wait_for(&task, "180s");
    let row = status_run(&f, &task, &second);
    assert!(row["profiles_with_quota"].is_null(), "{row}");
}

// (h) -------------------------------------------------------------------------

#[test]
fn the_retired_rotation_key_is_ignored() {
    let f = common::fixture();
    let path = f.home_path().join("config.json");
    fs::write(&path, r#"{"same_provider_rotation":true}"#).unwrap();

    let v = json_out(f.cmd().args(["config", "list", "--json"]));
    assert_eq!(v["config_file"]["unknown_keys"], json!([]), "{v}");
    assert!(!v["settings"].as_array().unwrap().iter().any(|s| s["key"] == "same-provider-rotation"), "{v}");
    let out = f.cmd().args(["config", "list"]).assert().success().get_output().clone();
    let human = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!human.contains("unknown keys"), "{human}");

    let out = f.cmd().args(["doctor", "--json"]).output().unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    let config = doc["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == "config")
        .unwrap_or_else(|| panic!("no config item: {doc}"));
    assert_eq!(config["status"], "ok", "{config}");
    assert_eq!(config["data"]["unknown_keys"], json!([]), "{config}");

    for args in [
        &["config", "get", "same-provider-rotation"][..],
        &["config", "set", "same-provider-rotation", "on"][..],
        &["config", "unset", "same-provider-rotation"][..],
    ] {
        let out = f.cmd().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(1), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("removed") && stderr.contains("no effect"), "{args:?}: {stderr}");
    }

    f.cmd().args(["config", "set", "live-quota", "on", "--json"]).assert().success();
    let stored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(stored["same_provider_rotation"], true, "{stored}");
    assert_eq!(stored["live_quota"], true, "{stored}");
}

// (i) -------------------------------------------------------------------------

#[test]
fn spec_review_start_restores_a_reset_profile() {
    let f = init();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    park(&f, "claude-b", &at(Duration::hours(-2)));
    add_event(&f, "R", "claude-b", &at(Duration::hours(-1)), json!({
        "rate_limit_type": "five_hour", "utilization": 0.99,
        "resets_at": epoch(Duration::minutes(-30))}));
    // parked, and the reviewer ranking (AC1's ROLE) sees its window reset
    assert_eq!(profile_status(&f, "claude-b"), "rate_limited");
    let before = candidate(&f, "reviewer", &task, "claude-b");
    assert_eq!(before["recovered"], true, "{before}");

    let s = launch_ok(&f, &task, "spec_review_approve", &["spec", "review", "start", &task, "--profile", "claude-b", "--model", "opus", "--effort", "high"]);
    assert!(!warnings(&s).iter().any(|w| w.contains("rate_limited")), "{s}");
    // marked active again: `profile list`, `profile usage` and the ranking
    // agree. The `window_reset` quota event behind it (its detail without
    // `profile_auto`) is shown by no command; auto_pick.rs's
    // `mark_recovered_writes_one_window_reset_without_profile_auto` pins it.
    assert_eq!(profile_status(&f, "claude-b"), "active");
    let usage = json_out(f.cmd().args(["profile", "usage", "claude-b", "--json"]));
    assert_eq!(usage["profiles"][0]["status"], "active", "{usage}");
    let after = candidate(&f, "reviewer", &task, "claude-b");
    assert_eq!(after["recovered"], false, "{after}");
    assert_eq!(after["eligible"], true, "{after}");
    let reasons = after["reasons"].to_string();
    assert!(!reasons.contains("rate_limited"), "{after}");
    assert!(!reasons.contains("treating parked profile as recovered"), "{after}");
    wait_run(&f, &run_id_of(&s));
}

// (j) the switch itself ---------------------------------------------------------------

#[test]
fn j_the_switch_is_listed_read_and_set_like_the_others() {
    let f = common::fixture();
    let list = json_out(f.cmd().args(["config", "list", "--json"]));
    let settings = list["settings"].as_array().unwrap();
    assert_eq!(settings.len(), 16, "{list}");
    assert_eq!(settings[2]["key"], "require-reason", "{list}");
    assert_eq!(settings[2]["value"], "off", "{list}");
    assert_eq!(settings[2]["default"], "off", "{list}");
    let get = json_out(f.cmd().args(["config", "get", "require-reason", "--json"]));
    assert_eq!(get, serde_json::json!({"key": "require-reason", "value": "off"}));

    let set = json_out(f.cmd().args(["config", "set", "require-reason", "on", "--json"]));
    assert_eq!(set["ok"], true, "{set}");
    assert_eq!(set["previous"], "off", "{set}");
    let notice = set["notice"].as_str().unwrap();
    assert!(notice.contains("--reason"), "{notice}");
    assert!(notice.contains("require-reason off"), "{notice}");
    let get = json_out(f.cmd().args(["config", "get", "require-reason", "--json"]));
    assert_eq!(get["value"], "on", "{get}");

    let out = f.cmd().args(["config", "set", "require-reason", "yes"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("require-reason"));

    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01RUN")
        .args(["config", "set", "require-reason", "off"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("THOTH_RUN_ID"));
}
