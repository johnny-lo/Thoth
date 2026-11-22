//! AGY-1 (second part): agy's no-capacity answer (`... UNAVAILABLE (code
//! 503): No capacity available for model M on the server`, exit 3) is a
//! transient failure, waited on and retried like FC-2's others; an unknown
//! model and a login failure are still not retried. Everything runs on
//! fake-agy and a temporary THOTH_HOME, read through the thoth CLI.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::{Path, PathBuf};

const MODEL: &str = "gemini-3.8-flash-low";
const CONV: &str = "11111111-2222-4333-8444-555555555555";
const TRANSIENT_STEP: &str =
    "a temporary problem (network or a busy service), not a quota or login problem; run thoth resume";

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

fn no_capacity(model: &str) -> String {
    format!(
        "Our servers are experiencing high traffic right now, please try again in a minute. \
         (UNAVAILABLE (code 503): No capacity available for model {model} on the server)"
    )
}

/// `dispatch` of `model` on agy-a with fake-agy's switches and `env`.
fn dispatch(f: &Fixture, cfg: &Path, task: &str, model: &str, env: &[(&str, &str)]) -> String {
    let mut c = f.cmd();
    c.env("FAKE_TASK_ID", task)
        .env("FAKE_AGY_DATA_DIR", cfg)
        .env("FAKE_AGY_ARGS_LOG", args_log(f))
        .env("FAKE_ATTEMPTS_FILE", f.home_path().join(format!("attempts-{task}.txt")))
        .env("FAKE_SCENARIO", "ok");
    for (k, v) in env {
        c.env(k, v);
    }
    c.args(["dispatch", task, "--profile", "agy-a", "--model", model, "--effort", "medium", "--json"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    d["run_id"].as_str().unwrap().to_string()
}

fn launches(f: &Fixture) -> Vec<Vec<String>> {
    std::fs::read_to_string(args_log(f))
        .unwrap()
        .lines()
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

fn run_events(f: &Fixture, task: &str, run: &str, ty: &str) -> Vec<Value> {
    f.events_json(task)
        .into_iter()
        .filter(|e| e["run_id"] == run && e["type"] == ty)
        .map(|e| e["detail"].clone())
        .collect()
}

fn final_change(f: &Fixture, task: &str, run: &str) -> Value {
    run_events(f, task, run, "status_change").last().cloned().expect("a status_change")
}

fn profile_status(f: &Fixture, id: &str) -> String {
    let mut c = f.cmd();
    c.args(["profile", "list", "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(c, "profile list")).unwrap();
    v["profiles"].as_array().unwrap().iter().find(|p| p["profile_id"] == id).unwrap()["status"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn a_no_capacity_answer_is_waited_on_and_the_next_try_completes() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let run = dispatch(&f, &cfg, &task, MODEL, &[("FAKE_FIRST_SCENARIO", "no_capacity"), ("THOTH_RETRY_WAITS", "1,2")]);
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    let retries = run_events(&f, &task, &run, "run_retry");
    assert_eq!(retries.len(), 1, "{retries:?}");
    let r = &retries[0];
    assert_eq!(r["attempt"], 1, "{r}");
    assert_eq!(r["error"], no_capacity(MODEL), "{r}");
    assert_eq!(r["exit_code"], 3, "{r}");
    assert_eq!(r["wait_seconds"], 1, "{r}");
    assert_eq!(r["mode"], "resume", "{r}");
    assert_eq!(r["session_id"], CONV, "{r}");
    assert!(r["worker_pid"].as_i64().is_some(), "{r}");
    let change = final_change(&f, &task, &run);
    assert_eq!(change["retries"], 1, "{change}");
    assert!(change["retry_stop"].is_null(), "{change}");
    let l = launches(&f);
    assert_eq!(after(&l[l.len() - 1], "--conversation").as_deref(), Some(CONV), "{l:?}");
    assert!(run_events(&f, &task, &run, "quota_exceeded").is_empty());
    assert_eq!(profile_status(&f, "agy-a"), "active");
    assert_eq!(f.task_status(&task), "in_review");
}

#[test]
fn a_no_capacity_answer_before_init_is_retried_fresh() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let run = dispatch(
        &f,
        &cfg,
        &task,
        MODEL,
        &[("FAKE_FIRST_SCENARIO", "no_capacity_before_init"), ("THOTH_RETRY_WAITS", "1,2")],
    );
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    let retries = run_events(&f, &task, &run, "run_retry");
    assert_eq!(retries.len(), 1, "{retries:?}");
    assert_eq!(retries[0]["mode"], "fresh", "{retries:?}");
    assert_eq!(retries[0]["error"], no_capacity(MODEL), "{retries:?}");
    assert_eq!(retries[0]["exit_code"], 3, "{retries:?}");
    assert!(retries[0]["session_id"].is_null(), "{retries:?}");
    let l = launches(&f);
    assert_eq!(l.len(), 2, "{l:?}");
    assert!(!l[1].iter().any(|a| a == "--conversation"), "{:?}", l[1]);
    assert_eq!(final_change(&f, &task, &run)["retries"], 1);
}

#[test]
fn a_run_that_only_gets_no_capacity_gives_up_after_the_last_try() {
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let run = dispatch(&f, &cfg, &task, MODEL, &[("FAKE_SCENARIO", "no_capacity"), ("THOTH_RETRY_WAITS", "1,2")]);
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");

    let retries = run_events(&f, &task, &run, "run_retry");
    assert_eq!(retries.len(), 2, "{retries:?}");
    assert_eq!(retries[0]["attempt"], 1);
    assert_eq!(retries[1]["attempt"], 2);
    assert_eq!(retries[0]["wait_seconds"], 1);
    assert_eq!(retries[1]["wait_seconds"], 2);
    let s = f.run_of(&task, &run);
    assert_eq!(s["status"], "interrupted", "{s}");
    assert_eq!(s["interrupted_reason"], "transient", "{s}");
    let change = final_change(&f, &task, &run);
    assert_eq!(change["retries"], 2, "{change}");
    assert_eq!(change["retry_stop"], "exhausted", "{change}");
    assert_eq!(change["login_check"], "logged_in", "{change}");
    assert_eq!(s["next_step"], format!("{TRANSIENT_STEP} {run}"), "{s}");
    assert!(run_events(&f, &task, &run, "quota_exceeded").is_empty());
    assert_eq!(profile_status(&f, "agy-a"), "active");
    assert_eq!(launches(&f).len(), 3);
}

#[test]
fn an_unknown_model_and_a_login_failure_still_fail_without_a_retry() {
    // a fresh home, so no stored model list refuses the unknown model first
    let f = setup();
    let cfg = add_agy_profile(&f, "agy-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let run = dispatch(&f, &cfg, &task, "made-up-model", &[("FAKE_AGY_MODELS", ""), ("THOTH_RETRY_WAITS", "1,2")]);
    assert_eq!(f.wait_for(&task, "120s").1, "failed");
    assert!(run_events(&f, &task, &run, "run_retry").is_empty());
    assert_eq!(final_change(&f, &task, &run)["retries"], 0);
    assert_eq!(launches(&f).len(), 1);

    let g = setup();
    let gcfg = add_agy_profile(&g, "agy-a");
    let (task, _) = g.create_task(Fixture::SPEC);
    let marker = g.home_path().join("logged-out.marker");
    let marker = marker.to_str().unwrap();
    let run = dispatch(
        &g,
        &gcfg,
        &task,
        MODEL,
        &[("FAKE_SCENARIO", "auth_expired"), ("FAKE_LOGOUT_MARKER", marker), ("THOTH_RETRY_WAITS", "1,2")],
    );
    assert_eq!(g.wait_for(&task, "120s").1, "interrupted");
    let s = g.run_of(&task, &run);
    assert_eq!(s["interrupted_reason"], "auth", "{s}");
    assert!(run_events(&g, &task, &run, "run_retry").is_empty());
}
