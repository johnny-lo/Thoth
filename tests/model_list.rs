//! MODELS-1: each account's usable models keep themselves up to date. An
//! antigravity profile's list is read from `agy models` on every login check
//! and stored; a launch outside it is refused before anything is written. A
//! claude-code profile learns a model it cannot run from a
//! `credits_required` result and refuses it until the plan changes or
//! `profile forget-model`. Always the fakes and a temporary THOTH_HOME.
//!
//! Only the interface: every run, event, task and profile fact is read from
//! `status --json`, `events --json`, `profile list --json` and `models
//! --json`, and every fixture is built by dispatching the fakes. Nothing here
//! opens state.db or global.db; what the CLI does not show (the detail of a
//! `model_unavailable` quota event, the age note of an old list) is checked
//! by unit tests of the public functions that write and read it.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// `Fixture::add_agy_profile`, as in tests/antigravity.rs.
trait AgyFixture {
    fn add_agy_profile(&self, id: &str) -> PathBuf;
}

impl AgyFixture for Fixture {
    fn add_agy_profile(&self, id: &str) -> PathBuf {
        let cfg = self.home.path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        self.cmd()
            .args(["profile", "add", id, "--provider", "google", "--harness", "antigravity", "--config-dir"])
            .arg(&cfg)
            .assert()
            .success();
        cfg
    }
}

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

/// Exit code, stdout and stderr.
fn run(c: &mut assert_cmd::Command) -> (i32, String, String) {
    let out = c.output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
    )
}

fn parse(text: &str) -> Value {
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"))
}

fn is_rfc3339_utc(s: &str) -> bool {
    OffsetDateTime::parse(s, &Rfc3339).is_ok() && s.ends_with('Z')
}

fn args_log(f: &Fixture) -> PathBuf {
    f.home_path().join("fake-agy-args.jsonl")
}

/// `thoth` with the fake agy wired for a launch: its data dir, args log and
/// the models it knows (`FAKE_AGY_MODELS`).
fn agy_cmd(f: &Fixture, cfg: &Path, task_id: &str, models: &str) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", task_id)
        .env("FAKE_AGY_DATA_DIR", cfg)
        .env("FAKE_AGY_ARGS_LOG", args_log(f))
        .env("FAKE_AGY_MODELS", models);
    c
}

/// `profile check agy-a --json` with the fake knowing `models`.
fn check_agy(f: &Fixture, models: &str) -> Value {
    let mut c = f.cmd();
    c.env("FAKE_AGY_MODELS", models).args(["profile", "check", "agy-a", "--json"]);
    let (code, stdout, stderr) = run(&mut c);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    parse(&stdout)
}

fn json_of(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    parse(&String::from_utf8(f.run_ok(c, &args.join(" "))).unwrap())
}

fn models_json(f: &Fixture) -> Value {
    json_of(f, &["models", "--json"])
}

fn models_text(f: &Fixture) -> String {
    let mut c = f.cmd();
    c.args(["models"]);
    String::from_utf8(f.run_ok(c, "models")).unwrap()
}

fn profile_entry<'a>(v: &'a Value, id: &str) -> &'a Value {
    v["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {v}"))
}

fn model_entry<'a>(profile: &'a Value, model: &str) -> &'a Value {
    profile["models"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["model"] == model)
        .unwrap_or_else(|| panic!("no {model} in {profile}"))
}

/// `status TASK --json`.
fn status_of(f: &Fixture, task_id: &str) -> Value {
    json_of(f, &["status", task_id, "--json"])
}

fn task_status(f: &Fixture, task_id: &str) -> String {
    status_of(f, task_id)["status"].as_str().unwrap().to_string()
}

/// The task's runs from `status --json`.
fn runs_of(f: &Fixture, task_id: &str) -> Vec<Value> {
    status_of(f, task_id)["runs"].as_array().unwrap().clone()
}

/// One run of the task, by id, from `status --json`.
fn run_of(f: &Fixture, task_id: &str, run_id: &str) -> Value {
    runs_of(f, task_id)
        .into_iter()
        .find(|r| r["run_id"] == run_id)
        .unwrap_or_else(|| panic!("no run {run_id} in status of {task_id}"))
}

/// Every event of the repo (or of one task) from `events --json`, in seq order.
fn events(f: &Fixture, task_id: Option<&str>) -> Vec<Value> {
    let mut args = vec!["events", "--json"];
    if let Some(t) = task_id {
        args.extend(["--task", t]);
    }
    json_of(f, &args)["events"].as_array().unwrap().clone()
}

fn event_types(f: &Fixture, task_id: &str) -> Vec<String> {
    events(f, Some(task_id)).iter().map(|e| e["type"].as_str().unwrap().to_string()).collect()
}

/// `profile list --json` entry of one profile.
fn profile_row(f: &Fixture, id: &str) -> Value {
    json_of(f, &["profile", "list", "--json"])["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["profile_id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no profile {id}"))
}

/// Every `-p` launch the fake agy logged.
fn stream_launches(f: &Fixture) -> usize {
    std::fs::read_to_string(args_log(f))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["args"][0] == "-p")
        .count()
}

/// A task that is `in_review` with a completed implementer run on claude-b
/// (the fake's `ok` scenario), for `review start`.
fn reviewable_task(f: &Fixture) -> String {
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task_id, "claude-b", "ok", &[]);
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    task_id
}

/// A claude-b dispatch of `model` in `scenario`, with `--effort medium`.
fn claude_dispatch(f: &Fixture, task: &str, model: &str, scenario: &str) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task)
        .args(["dispatch", task, "--profile", "claude-b", "--model", model, "--effort", "medium", "--json"]);
    c
}

/// An agy run of `model` that crashed after its init line (`interrupted`,
/// `crash`), so it can be resumed. `hide_list` makes the dispatch's login
/// check read no list (`FAKE_AGY_HIDE_MODELS`), so nothing is stored by it.
fn crashed_agy_run(f: &Fixture, cfg: &Path, task_id: &str, model: &str, models: &str, hide_list: bool) -> String {
    let mut c = agy_cmd(f, cfg, task_id, models);
    if hide_list {
        c.env("FAKE_AGY_HIDE_MODELS", "1");
    }
    c.env("FAKE_SCENARIO", "crash")
        .args(["dispatch", task_id, "--profile", "agy-a", "--model", model, "--effort", "medium", "--json"]);
    let d = parse(&String::from_utf8(f.run_ok(c, "dispatch")).unwrap());
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(task_id, "120s").1, "interrupted");
    let r = run_of(f, task_id, &run_id);
    assert_eq!((r["status"].as_str(), r["interrupted_reason"].as_str()), (Some("interrupted"), Some("crash")), "{r}");
    run_id
}

// t1 ------------------------------------------------------------------------

#[test]
fn profile_check_stores_the_agy_list_and_models_shows_it() {
    let f = setup();
    f.add_agy_profile("agy-a");
    f.add_profile("claude-a");

    // nothing stored yet
    let v = models_json(&f);
    let agy = profile_entry(&v, "agy-a");
    assert_eq!(agy["models_source"], "agy_models", "{v}");
    assert_eq!(agy["models"], serde_json::json!([]), "{v}");
    assert_eq!(agy["models_seen_at"], Value::Null, "{v}");
    assert_eq!(agy["note"], "no model list stored yet; run `thoth profile check agy-a`", "{v}");
    let text = models_text(&f);
    assert!(text.contains("agy-a (google, antigravity) [agy_models]\n  no model list stored yet; run `thoth profile check agy-a`"), "{text}");

    let checked = check_agy(&f, "a-model,b-model");
    assert_eq!(checked["logged_in"], true, "{checked}");
    assert_eq!(checked["models_count"], 2, "{checked}");
    let seen = checked["models_seen_at"].as_str().expect("models_seen_at");
    assert!(is_rfc3339_utc(seen), "{seen}");

    let v = models_json(&f);
    let agy = profile_entry(&v, "agy-a");
    assert_eq!(agy["models_source"], "agy_models", "{v}");
    assert_eq!(agy["models_seen_at"], seen, "{v}");
    assert_eq!(agy["note"], Value::Null, "{v}");
    let names: Vec<(&str, &str)> = agy["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| (m["model"].as_str().unwrap(), m["display_name"].as_str().unwrap()))
        .collect();
    assert_eq!(names, [("a-model", "Fake display name for a-model"), ("b-model", "Fake display name for b-model")]);
    for m in agy["models"].as_array().unwrap() {
        assert_eq!(m["unavailable"], Value::Null, "{m}");
    }
    let text = models_text(&f);
    assert!(text.contains(&format!("agy-a (google, antigravity) [agy_models]\n  models read at {seen}\n  a-model (Fake display name for a-model)")), "{text}");

    // the other sources keep their keys; claude has no time
    let claude = profile_entry(&v, "claude-a");
    assert_eq!(claude["models_source"], "fixed_list", "{v}");
    assert_eq!(claude["models_seen_at"], Value::Null, "{v}");
    assert_eq!(claude["note"], "fixed list, not confirmed per account", "{v}");
    assert!(!text.contains("claude-a (anthropic, claude-code) [fixed_list]\n  models read at"), "{text}");

    // the human check line, and the keys when the check carries no list
    let mut c = f.cmd();
    c.env("FAKE_AGY_MODELS", "a-model").args(["profile", "check", "agy-a"]);
    let (code, stdout, _) = run(&mut c);
    assert_eq!(code, 0);
    assert!(stdout.trim().ends_with(" models=1"), "{stdout}");
    let mut c = f.cmd();
    c.args(["profile", "check", "claude-a", "--json"]);
    let (code, stdout, _) = run(&mut c);
    assert_eq!(code, 0);
    let v = parse(&stdout);
    assert_eq!(v["models_count"], Value::Null, "{v}");
    assert_eq!(v["models_seen_at"], Value::Null, "{v}");
    let mut c = f.cmd();
    c.args(["profile", "check", "claude-a"]);
    let (_, stdout, _) = run(&mut c);
    assert!(stdout.trim().ends_with(" models=-"), "{stdout}");
    // a check that reads no list keeps the stored one
    let v = check_agy(&f, "");
    assert_eq!(v["models_count"], Value::Null, "{v}");
    let v = models_json(&f);
    assert_eq!(profile_entry(&v, "agy-a")["models"].as_array().unwrap().len(), 1, "{v}");
}

/// `models_seen_at` for a codex profile comes from its cache: `fetched_at`
/// re-formatted in UTC whatever offset codex wrote it in (review 1), else
/// the file's time, else null. (The note for an agy list a day old cannot be
/// produced through the CLI; `discover`'s unit test covers it.)
#[test]
fn models_reads_the_codex_fetch_time_in_utc_or_the_files_time() {
    let f = setup();
    let cfg = f.add_codex_profile("codex-a");
    let cache = cfg.join("models_cache.json");
    let luna = serde_json::json!({"slug": "gpt-5.6-luna", "display_name": "Luna", "visibility": "list", "priority": 1});
    for (written, shown) in [
        ("2026-10-16T07:15:39.244865800Z", "2026-10-16T07:15:39.2448658Z"),
        ("2026-10-16T15:15:39+08:00", "2026-10-16T07:15:39Z"),
        ("2026-10-16T03:15:39-04:00", "2026-10-16T07:15:39Z"),
    ] {
        std::fs::write(&cache, serde_json::json!({"fetched_at": written, "models": [luna]}).to_string()).unwrap();
        let v = models_json(&f);
        let codex = profile_entry(&v, "codex-a");
        assert_eq!(codex["models_source"], "codex_models_cache", "{v}");
        assert_eq!(codex["models_seen_at"], shown, "written as {written}: {v}");
        assert_eq!(codex["models"][0]["model"], "gpt-5.6-luna", "{v}");
        let text = models_text(&f);
        assert!(text.contains(&format!("codex-a (openai, codex) [codex_models_cache]\n  models read at {shown}\n")), "{text}");
    }
    std::fs::write(&cache, serde_json::json!({"models": [luna]}).to_string()).unwrap();
    let v = models_json(&f);
    let seen = profile_entry(&v, "codex-a")["models_seen_at"].as_str().expect("the file's time");
    assert!(is_rfc3339_utc(seen), "{seen}");
    std::fs::remove_file(&cache).unwrap();
    let v = models_json(&f);
    assert_eq!(profile_entry(&v, "codex-a")["models_seen_at"], Value::Null, "{v}");
}

// t2 ------------------------------------------------------------------------

#[test]
fn a_model_outside_the_agy_list_is_refused_before_anything_is_written() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");
    check_agy(&f, "a-model,b-model");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let events_before = events(&f, None);
    let expect_refusal = |stderr: &str| {
        for needle in ["c-model", "a-model", "b-model", "profile check agy-a", "read at 2"] {
            assert!(stderr.contains(needle), "{needle}: {stderr}");
        }
    };

    // dispatch, human and --json
    let mut c = agy_cmd(&f, &cfg, &task_id, "a-model,b-model");
    c.args(["dispatch", &task_id, "--profile", "agy-a", "--model", "c-model", "--effort", "medium"]);
    let (code, stdout, stderr) = run(&mut c);
    assert_eq!(code, 1, "{stdout}\n{stderr}");
    expect_refusal(&stderr);
    let mut c = agy_cmd(&f, &cfg, &task_id, "a-model,b-model");
    c.args(["dispatch", &task_id, "--profile", "agy-a", "--model", "C-Model", "--effort", "medium", "--json"]);
    let (code, stdout, stderr) = run(&mut c);
    assert_eq!(code, 1, "{stdout}\n{stderr}");
    assert!(stdout.trim().is_empty(), "{stdout}");
    let err = parse(&stderr);
    assert_eq!(err["ok"], false, "{err}");
    assert!(err["error"].as_str().unwrap().contains("C-Model"), "{err}");
    assert_eq!(task_status(&f, &task_id), "queued");

    // spec review start
    let mut c = agy_cmd(&f, &cfg, &task_id, "a-model,b-model");
    c.args(["spec", "review", "start", &task_id, "--profile", "agy-a", "--model", "c-model", "--effort", "medium"]);
    let (code, _, stderr) = run(&mut c);
    assert_eq!(code, 1, "{stderr}");
    expect_refusal(&stderr);
    assert_eq!(task_status(&f, &task_id), "queued");
    assert!(runs_of(&f, &task_id).is_empty());
    assert_eq!(events(&f, None), events_before, "no event was written by the three refusals");

    // review start: the candidate's evidence may be recorded first, nothing else
    let reviewed = reviewable_task(&f);
    let runs_before = runs_of(&f, &reviewed);
    let types_before = event_types(&f, &reviewed);
    let mut c = agy_cmd(&f, &cfg, &reviewed, "a-model,b-model");
    c.args(["review", "start", &reviewed, "--profile", "agy-a", "--model", "c-model", "--effort", "medium"]);
    let (code, _, stderr) = run(&mut c);
    assert_eq!(code, 1, "{stderr}");
    expect_refusal(&stderr);
    assert_eq!(task_status(&f, &reviewed), "in_review");
    assert_eq!(runs_of(&f, &reviewed), runs_before, "no reviewer run row");
    let types_after = event_types(&f, &reviewed);
    assert_eq!(types_after[..types_before.len()], types_before[..], "{types_after:?}");
    assert!(types_after[types_before.len()..].iter().all(|t| t == "evidence_added"), "{types_after:?}");

    assert_eq!(stream_launches(&f), 0);
}

// t3 ------------------------------------------------------------------------

#[test]
fn the_list_just_read_wins_over_the_stored_one() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");
    check_agy(&f, "a-model,b-model");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let mut c = agy_cmd(&f, &cfg, &task_id, "a-model,b-model,c-model");
    c.args(["dispatch", &task_id, "--profile", "agy-a", "--model", "c-model", "--effort", "medium", "--json"]);
    let v = parse(&String::from_utf8(f.run_ok(c, "dispatch")).unwrap());
    assert_eq!(v["ok"], true, "{v}");
    assert!(v["warnings"].as_array().unwrap().iter().all(|w| !w.as_str().unwrap().contains("not checked")), "{v}");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let models = models_json(&f);
    let names: Vec<&str> = profile_entry(&models, "agy-a")["models"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["model"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["a-model", "b-model", "c-model"]);
    assert_eq!(stream_launches(&f), 1);
}

// t4 ------------------------------------------------------------------------

#[test]
fn without_any_list_the_launch_goes_ahead_with_a_warning() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let mut c = agy_cmd(&f, &cfg, &task_id, "");
    c.args(["dispatch", &task_id, "--profile", "agy-a", "--model", "x-model", "--effort", "medium", "--json"]);
    let v = parse(&String::from_utf8(f.run_ok(c, "dispatch")).unwrap());
    assert_eq!(v["ok"], true, "{v}");
    let warnings = v["warnings"].as_array().unwrap();
    assert!(warnings.iter().any(|w| w == "no model list for agy-a; model x-model was not checked"), "{v}");
    let models = models_json(&f);
    assert_eq!(profile_entry(&models, "agy-a")["models"], serde_json::json!([]), "{models}");
    // the fake knows no model, so the run itself fails
    assert_eq!(f.wait_for(&task_id, "120s").1, "failed");

    // the same warning in the human output
    let (task2, _) = f.create_task(Fixture::SPEC);
    let mut c = agy_cmd(&f, &cfg, &task2, "");
    c.args(["dispatch", &task2, "--profile", "agy-a", "--model", "x-model", "--effort", "medium"]);
    let (code, _, stderr) = run(&mut c);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("warning: no model list for agy-a; model x-model was not checked"), "{stderr}");
    f.wait_for(&task2, "120s");
}

// t5 ------------------------------------------------------------------------

#[test]
fn resume_checks_the_stored_list() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    // a crashed run of c-model, dispatched while c-model was listed
    let run_id = crashed_agy_run(&f, &cfg, &task_id, "c-model", "a-model,b-model,c-model", false);

    // the list read since no longer has c-model: refused, nothing changes
    check_agy(&f, "a-model,b-model");
    let mut c = f.cmd();
    c.args(["resume", &run_id]);
    let (code, _, stderr) = run(&mut c);
    assert_eq!(code, 1, "{stderr}");
    for needle in ["c-model", "a-model", "b-model", "profile check agy-a"] {
        assert!(stderr.contains(needle), "{needle}: {stderr}");
    }
    assert_eq!(run_of(&f, &task_id, &run_id)["status"], "interrupted");
    assert_eq!(task_status(&f, &task_id), "interrupted");
    assert_eq!(runs_of(&f, &task_id).len(), 1, "no new run row");

    // listed again: the resume goes ahead without the warning
    check_agy(&f, "a-model,b-model,c-model");
    let mut c = agy_cmd(&f, &cfg, &task_id, "a-model,b-model,c-model");
    c.env("FAKE_SCENARIO", "resume").args(["resume", &run_id, "--json"]);
    let v = parse(&String::from_utf8(f.run_ok(c, "resume")).unwrap());
    assert_eq!(v["ok"], true, "{v}");
    assert!(v["warnings"].as_array().unwrap().iter().all(|w| !w.as_str().unwrap().contains("not checked")), "{v}");
    f.wait_for(&task_id, "120s");
}

/// `resume` of an agy run with no list stored warns and goes ahead: the run
/// was dispatched while the login check read no list, so none was stored.
#[test]
fn resume_without_a_stored_list_warns() {
    let f = setup();
    let cfg = f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let run_id = crashed_agy_run(&f, &cfg, &task_id, "gemini-3.8-flash-low", "gemini-3.8-flash-low", true);
    let models = models_json(&f);
    assert_eq!(profile_entry(&models, "agy-a")["models"], serde_json::json!([]), "nothing stored: {models}");

    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "resume")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_AGY_DATA_DIR", &cfg)
        .env("FAKE_AGY_MODELS", "gemini-3.8-flash-low")
        .args(["resume", &run_id, "--json"]);
    let v = parse(&String::from_utf8(f.run_ok(c, "resume")).unwrap());
    assert!(
        v["warnings"].as_array().unwrap().iter().any(|w| w == "no model list for agy-a; model gemini-3.8-flash-low was not checked"),
        "{v}"
    );
    f.wait_for(&task_id, "120s");
}

// t6, t7 --------------------------------------------------------------------

#[test]
fn a_credits_required_result_fails_the_run_and_the_model_is_refused_until_forgotten() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    let d = parse(&String::from_utf8(f.run_ok(claude_dispatch(&f, &task, "fable", "credits_required"), "dispatch")).unwrap());
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "failed");

    // the run: failed, no interrupted_reason, the model-unavailable note
    let r = run_of(&f, &task, &run_id);
    assert_eq!(r["status"], "failed", "{r}");
    assert_eq!(r["interrupted_reason"], Value::Null, "{r}");
    let error = r["error_text"].as_str().unwrap();
    assert!(error.starts_with("model unavailable on this account: Fable 5.1 requires usage credits."), "{error}");
    assert_eq!(r["profiles_with_quota"], Value::Null, "not a quota stop: {r}");
    // the profile is not parked
    assert_eq!(profile_row(&f, "claude-b")["status"], "active");
    // the run's events: model_unavailable, and none of a quota stop's
    let run_events: Vec<Value> = events(&f, Some(&task)).into_iter().filter(|e| e["run_id"] == run_id.as_str()).collect();
    let types: Vec<&str> = run_events.iter().map(|e| e["type"].as_str().unwrap()).collect();
    assert!(types.contains(&"model_unavailable"), "{types:?}");
    assert!(!types.contains(&"quota_exceeded"), "{types:?}");
    assert!(!types.contains(&"login_failed"), "{types:?}");
    let detail = &run_events.iter().find(|e| e["type"] == "model_unavailable").unwrap()["detail"];
    assert_eq!(detail["profile"], "claude-b", "{detail}");
    assert_eq!(detail["model"], "fable", "{detail}");
    assert_eq!(detail["api_error"], "model_requires_usage_credits", "{detail}");
    assert!(detail["note"].as_str().unwrap().starts_with("model unavailable on this account:"), "{detail}");

    // t7: `thoth models` shows the record (its seen_at, reason and run)
    let v = models_json(&f);
    let fable = model_entry(profile_entry(&v, "claude-b"), "fable");
    let ts = fable["unavailable"]["seen_at"].as_str().expect("seen_at").to_string();
    assert!(is_rfc3339_utc(&ts), "{ts}");
    assert_eq!(fable["unavailable"]["run_id"], run_id.as_str(), "{v}");
    assert!(fable["unavailable"]["reason"].as_str().unwrap().starts_with("Fable 5.1 requires usage credits."), "{v}");
    assert_eq!(model_entry(profile_entry(&v, "claude-b"), "sonnet")["unavailable"], Value::Null, "{v}");
    let text = models_text(&f);
    assert!(text.contains(&format!("  fable (fable) — no score; Thoth runs 1 reviewed 0 PASS 0 REJECT 0 — unavailable since {ts}: Fable 5.1 requires usage credits.")), "{text}");

    // the next dispatch of fable is refused before anything is written
    let runs_before = runs_of(&f, &task);
    let events_before = events(&f, None);
    let (code, stdout, stderr) = run(&mut claude_dispatch(&f, &task, "Fable", "ok"));
    assert_eq!(code, 1, "{stdout}\n{stderr}");
    let err = parse(&stderr);
    assert_eq!(err["ok"], false, "{err}");
    let message = err["error"].as_str().unwrap();
    for needle in ["claude-b", "Fable", ts.as_str(), "Fable 5.1 requires usage credits.", "haiku, sonnet, opus", "thoth profile forget-model claude-b Fable"] {
        assert!(message.contains(needle), "{needle}: {message}");
    }
    assert_eq!(task_status(&f, &task), "failed");
    assert_eq!(runs_of(&f, &task), runs_before);
    assert_eq!(events(&f, None), events_before);

    // another model goes ahead
    let d = parse(&String::from_utf8(f.run_ok(claude_dispatch(&f, &task, "sonnet", "ok"), "dispatch")).unwrap());
    assert_eq!(d["ok"], true, "{d}");
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    // forget-model: fable is accepted again, and `models` shows nothing
    let mut c = f.cmd();
    c.args(["profile", "forget-model", "claude-b", "FABLE", "--json"]);
    let v = parse(&String::from_utf8(f.run_ok(c, "profile forget-model")).unwrap());
    assert_eq!(v, serde_json::json!({"ok": true, "profile_id": "claude-b", "model": "FABLE"}), "{v}");
    assert_eq!(profile_row(&f, "claude-b")["status"], "active");
    let v = models_json(&f);
    assert_eq!(model_entry(profile_entry(&v, "claude-b"), "fable")["unavailable"], Value::Null, "{v}");
    let d = parse(&String::from_utf8(f.run_ok(claude_dispatch(&f, &task, "fable", "ok"), "dispatch")).unwrap());
    assert_eq!(d["ok"], true, "{d}");
    f.wait_for(&task, "120s");

    let mut c = f.cmd();
    c.args(["profile", "forget-model", "nobody", "fable"]);
    let (code, _, stderr) = run(&mut c);
    assert_eq!(code, 1);
    assert!(stderr.contains("no such profile: nobody"), "{stderr}");
}

/// A changed plan clears the record: the dispatch's own login check stores
/// the plan it sees, and the recorded `pro` no longer applies under `max`.
#[test]
fn a_changed_plan_lets_the_model_through_again() {
    let f = setup();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC);
    f.run_ok(claude_dispatch(&f, &task, "fable", "credits_required"), "dispatch");
    assert_eq!(f.wait_for(&task, "120s").1, "failed");
    let (code, _, stderr) = run(&mut claude_dispatch(&f, &task, "fable", "ok"));
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("forget-model"), "{stderr}");

    let mut c = claude_dispatch(&f, &task, "fable", "ok");
    c.env("FAKE_SUBSCRIPTION_TYPE", "max");
    let d = parse(&String::from_utf8(f.run_ok(c, "dispatch")).unwrap());
    assert_eq!(d["ok"], true, "{d}");
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    assert_eq!(profile_row(&f, "claude-b")["plan"], "max");
    let v = models_json(&f);
    assert_eq!(model_entry(profile_entry(&v, "claude-b"), "fable")["unavailable"], Value::Null, "{v}");

    // back on `pro`, the record applies again
    let (code, _, stderr) = run(&mut claude_dispatch(&f, &task, "fable", "ok"));
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("forget-model"), "{stderr}");
    assert_eq!(profile_row(&f, "claude-b")["plan"], "pro");
}

// t9 ------------------------------------------------------------------------

#[test]
fn a_codex_refusal_lists_the_cache_slugs_when_there_is_a_cache() {
    let f = setup();
    let cfg = f.add_codex_profile("codex-a");
    let (task, _) = f.create_task(Fixture::SPEC);
    let dispatch = |f: &Fixture| {
        let mut c = f.cmd();
        c.args(["dispatch", &task, "--profile", "codex-a", "--model", "sonnet", "--effort", "medium"]);
        run(&mut c)
    };
    let (code, _, stderr) = dispatch(&f);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("gpt-6-astra, gpt-5.6-sol"), "{stderr}");

    std::fs::write(cfg.join("models_cache.json"), serde_json::json!({"models": [
        {"slug": "late", "display_name": "Late", "visibility": "list", "priority": 20},
        {"slug": "hidden", "display_name": "Hidden", "visibility": "hide", "priority": 0},
        {"slug": "gpt-5.6-luna", "display_name": "Luna", "visibility": "list", "priority": 10}]}).to_string()).unwrap();
    let (code, _, stderr) = dispatch(&f);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("a codex model: gpt-5.6-luna, late"), "{stderr}");
    assert!(!stderr.contains("hidden"), "{stderr}");
    assert!(!stderr.contains("gpt-6-astra"), "{stderr}");
    assert_eq!(task_status(&f, &task), "queued");
    assert!(runs_of(&f, &task).is_empty());
}
