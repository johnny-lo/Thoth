//! `--effort` on `dispatch` and `review start` (S3-7): validation, the
//! downgrade rules, what reaches the harness's argv, and where the result is
//! recorded. Always the fakes (`fake-claude`, `fake-codex`, `fake-agy`).

mod common;

use serde_json::Value;
use std::path::{Path, PathBuf};

const CODEX_MODEL: &str = "gpt-5.6-luna";

fn claude_log(f: &common::Fixture) -> PathBuf {
    f.home_path().join("fake-claude-args.jsonl")
}

fn codex_log(f: &common::Fixture) -> PathBuf {
    f.home_path().join("fake-codex-args.jsonl")
}

/// The argv of every launch a fake logged, oldest first. For codex only
/// `exec` calls count (`login status` is logged too).
fn launches(log: &Path, only_exec: bool) -> Vec<Vec<String>> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect::<Vec<String>>()
        })
        .filter(|a| !only_exec || a.first().map(String::as_str) == Some("exec"))
        .collect()
}

fn has_run(args: &[String], seq: &[&str]) -> bool {
    args.windows(seq.len()).any(|w| w.iter().zip(seq).all(|(a, b)| a == b))
}

fn envd(f: &common::Fixture, scenario: &str, task_id: &str) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task_id)
        .env("FAKE_CLAUDE_ARGS_LOG", claude_log(f))
        .env("FAKE_CODEX_ARGS_LOG", codex_log(f));
    c
}

fn json_out(c: &mut assert_cmd::Command) -> Value {
    let out = c.assert().success().get_output().stdout.clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn dispatch_json(
    f: &common::Fixture,
    task_id: &str,
    profile: &str,
    model: &str,
    scenario: &str,
    extra: &[&str],
) -> Value {
    json_out(
        envd(f, scenario, task_id)
            .args(["dispatch", task_id, "--profile", profile, "--model", model, "--json"])
            .args(extra),
    )
}

fn review_start_json(
    f: &common::Fixture,
    task_id: &str,
    profile: &str,
    model: &str,
    extra: &[&str],
) -> Value {
    json_out(
        envd(f, "review_pass", task_id)
            .args(["review", "start", task_id, "--profile", profile, "--model", model, "--json"])
            .args(extra),
    )
}

fn warnings(v: &Value) -> Vec<String> {
    v["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w.as_str().unwrap().to_string())
        .collect()
}

/// A codex model cache naming `CODEX_MODEL` with low through xhigh.
fn write_codex_cache(cfg: &Path) {
    let levels: Vec<Value> = ["low", "medium", "high", "xhigh"]
        .iter()
        .map(|e| serde_json::json!({"effort": e}))
        .collect();
    std::fs::write(
        cfg.join("models_cache.json"),
        serde_json::json!({"models": [{"slug": CODEX_MODEL, "supported_reasoning_levels": levels}]})
            .to_string(),
    )
    .unwrap();
}

fn count(f: &common::Fixture, table: &str) -> i64 {
    f.state_conn()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn run_spec(f: &common::Fixture, run_id: &str) -> Value {
    let text: String = f
        .state_conn()
        .query_row("SELECT spec FROM runs WHERE run_id = ?1", [run_id], |r| r.get(0))
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn event_detail(f: &common::Fixture, run_id: &str, ty: &str) -> Value {
    let d: String = f
        .state_conn()
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            [run_id, ty],
            |r| r.get(0),
        )
        .unwrap();
    serde_json::from_str(&d).unwrap()
}

fn claude_setup() -> (common::Fixture, String) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    (f, task_id)
}

fn codex_setup() -> (common::Fixture, PathBuf, String) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-a");
    write_codex_cache(&cfg);
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    (f, cfg, task_id)
}

/// Implement with the `hooks` scenario so the task sits at `in_review`.
fn candidate_ready(f: &common::Fixture) -> String {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    dispatch_json(f, &task_id, "claude-b", "sonnet", "hooks", &["--effort", "medium"]);
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");
    task_id
}

fn wait_run(f: &common::Fixture, run_id: &str) {
    let mut c = f.cmd();
    c.args(["wait", "--run", run_id, "--timeout", "180s"]);
    f.run_ok(c, "wait --run");
}

// AC1 ---------------------------------------------------------------------

#[test]
fn dispatch_rejects_an_unknown_level_with_exit_1_and_writes_nothing() {
    let (f, task_id) = claude_setup();
    let runs = count(&f, "runs");
    let events = count(&f, "events");
    let out = envd(&f, "ok", &task_id)
        .args(["dispatch", &task_id, "--profile", "claude-a", "--model", "sonnet"])
        .args(["--effort", "extreme"])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(stderr.contains("ultra"), "{stderr}");
    assert_eq!(count(&f, "runs"), runs);
    assert_eq!(count(&f, "events"), events);
    assert_eq!(f.task_status(&task_id), "queued");
}

#[test]
fn review_start_rejects_an_unknown_level_with_exit_1_and_writes_nothing() {
    let f = common::fixture();
    let task_id = candidate_ready(&f);
    let runs = count(&f, "runs");
    let events = count(&f, "events");
    let out = envd(&f, "review_pass", &task_id)
        .args(["review", "start", &task_id, "--profile", "claude-a", "--model", "sonnet"])
        .args(["--effort", "extreme"])
        .assert()
        .failure()
        .code(1)
        .get_output()
        .clone();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(stderr.contains("ultra"), "{stderr}");
    assert_eq!(count(&f, "runs"), runs);
    assert_eq!(count(&f, "events"), events);
}

// AC2, AC3, AC4 (claude) ----------------------------------------------------

#[test]
fn claude_gets_effort_right_after_the_model_and_it_is_recorded_everywhere() {
    let (f, task_id) = claude_setup();
    let d = dispatch_json(&f, &task_id, "claude-a", "sonnet", "ok", &["--effort", "high"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(d["effort_requested"], "high", "{d}");
    assert_eq!(d["effort"], "high", "{d}");
    assert_eq!(d["effort_note"], Value::Null, "{d}");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    let all = launches(&claude_log(&f), false);
    let worker: Vec<&Vec<String>> = all.iter().filter(|a| a.first().map(String::as_str) == Some("-p")).collect();
    assert_eq!(worker.len(), 1, "{all:?}");
    assert!(has_run(worker[0], &["--model", "sonnet", "--effort", "high"]), "{:?}", worker[0]);

    // runs.spec carries `launch`; the task's own spec does not
    let spec = run_spec(&f, &run_id);
    assert_eq!(spec["launch"]["effort_requested"], "high", "{spec}");
    assert_eq!(spec["launch"]["effort"], "high", "{spec}");
    let task_spec: String = f
        .state_conn()
        .query_row("SELECT spec FROM tasks WHERE task_id = ?1", [&task_id], |r| r.get(0))
        .unwrap();
    assert!(!task_spec.contains("launch"), "{task_spec}");

    // status, the run_started event and the review packet
    let status = json_out(f.cmd().args(["status", &task_id, "--json"]));
    let run = &status["runs"][0];
    assert_eq!(run["effort_requested"], "high", "{run}");
    assert_eq!(run["effort"], "high", "{run}");
    let started = event_detail(&f, &run_id, "run_started");
    assert_eq!(started["effort_requested"], "high", "{started}");
    assert_eq!(started["effort"], "high", "{started}");
    let packet = json_out(f.cmd().args(["review", "packet", &task_id, "--json"]));
    assert_eq!(packet["runs"][0]["effort"], "high", "{packet}");
    let md = f.cmd().args(["review", "packet", &task_id]).assert().success().get_output().stdout.clone();
    let md = String::from_utf8(md).unwrap();
    assert!(md.contains(" | effort: high\n"), "{md}");
}

#[test]
fn haiku_gets_no_effort_flag_and_a_warning() {
    let (f, task_id) = claude_setup();
    let d = dispatch_json(&f, &task_id, "claude-a", "haiku", "ok", &["--effort", "high"]);
    assert_eq!(d["effort"], Value::Null, "{d}");
    assert_eq!(d["effort_requested"], "high", "{d}");
    assert!(warnings(&d).iter().any(|w| w.contains("does not support effort")), "{d}");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let all = launches(&claude_log(&f), false);
    assert!(!all.is_empty(), "no launch was logged");
    for args in &all {
        assert!(!args.iter().any(|a| a == "--effort"), "{args:?}");
    }
}

// AC3, AC4 (codex) ----------------------------------------------------------

#[test]
fn codex_lowers_to_the_cache_and_the_run_records_both_levels() {
    let (f, _cfg, task_id) = codex_setup();
    let d = dispatch_json(&f, &task_id, "codex-a", CODEX_MODEL, "ok", &["--effort", "max"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(d["effort_requested"], "max", "{d}");
    assert_eq!(d["effort"], "xhigh", "{d}");
    assert!(warnings(&d).iter().any(|w| w.contains("is not supported")), "{d}");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    let execs = launches(&codex_log(&f), true);
    assert_eq!(execs.len(), 1, "{execs:?}");
    assert!(execs[0].iter().any(|a| a == "model_reasoning_effort=\"xhigh\""), "{:?}", execs[0]);

    // (e) status and the run_started event
    let status = json_out(f.cmd().args(["status", &task_id, "--json"]));
    let run = &status["runs"][0];
    assert_eq!(run["effort_requested"], "max", "{run}");
    assert_eq!(run["effort"], "xhigh", "{run}");
    let started = event_detail(&f, &run_id, "run_started");
    assert_eq!(started["effort_requested"], "max", "{started}");
    assert_eq!(started["effort"], "xhigh", "{started}");
}

// AC1 (T7), RJ-1: a left-out `--effort` is refused on the first launch and
// kept from the previous run on a reroute in the same attempt ---------------

/// `ARGS...` that must exit 1 naming `--effort`, writing no run or event.
fn refused_without_effort(f: &common::Fixture, task_id: &str, args: &[&str]) {
    let runs = count(f, "runs");
    let events = count(f, "events");
    let out = envd(f, "ok", task_id).args(args).assert().failure().code(1).get_output().clone();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(stderr.contains("--effort is missing"), "{args:?}: {stderr}");
    assert_eq!(count(f, "runs"), runs, "{args:?}");
    assert_eq!(count(f, "events"), events, "{args:?}");
}

#[test]
fn without_effort_the_rule_level_is_picked_and_recorded_everywhere() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    f.add_profile("claude-b");
    let cfg = f.add_codex_profile("codex-a");
    write_codex_cache(&cfg);
    let (claude_task, _) = f.create_task(common::Fixture::SPEC);
    let (codex_task, _) = f.create_task(common::Fixture::SPEC);

    // the first dispatch without --effort is refused
    refused_without_effort(&f, &claude_task, &["dispatch", &claude_task, "--profile", "claude-a", "--model", "sonnet"]);
    refused_without_effort(&f, &codex_task, &["dispatch", &codex_task, "--profile", "codex-a", "--model", CODEX_MODEL]);
    assert_eq!(f.task_status(&claude_task), "queued");
    assert_eq!(f.task_status(&codex_task), "queued");

    // a first run with --effort medium stops on quota; the reroute in the
    // same attempt leaves --effort out and keeps medium
    let c1 = dispatch_json(&f, &claude_task, "claude-a", "sonnet", "quota", &["--effort", "medium"]);
    assert_eq!(f.wait_for(&claude_task, "120s").1, "interrupted");
    let c = dispatch_json(&f, &claude_task, "claude-b", "sonnet", "ok", &[]);
    assert_eq!(f.wait_for(&claude_task, "120s").1, "in_review");
    let x1 = dispatch_json(&f, &codex_task, "codex-a", CODEX_MODEL, "quota", &["--effort", "medium"]);
    assert_eq!(f.wait_for(&codex_task, "120s").1, "interrupted");
    f.cmd().args(["profile", "check", "codex-a"]).assert().success();
    let x = dispatch_json(&f, &codex_task, "codex-a", CODEX_MODEL, "ok", &[]);
    assert_eq!(f.wait_for(&codex_task, "120s").1, "in_review");

    for (d, first) in [(&c, &c1), (&x, &x1)] {
        assert_eq!(d["effort_requested"], "medium", "{d}");
        assert_eq!(d["effort"], "medium", "{d}");
        assert_eq!(d["effort_note"], Value::Null, "{d}");
        assert!(d.get("auto").is_none(), "{d}");
        let kept = format!("effort medium kept from run {}", first["run_id"].as_str().unwrap());
        assert!(warnings(d).iter().any(|w| w.contains(&kept)), "{d}");
        let spec = run_spec(&f, d["run_id"].as_str().unwrap());
        assert_eq!(spec["launch"]["effort_requested"], "medium", "{spec}");
        assert_eq!(spec["launch"]["effort"], "medium", "{spec}");
        assert!(spec["launch"].get("effort_auto").is_none(), "{spec}");
    }
    let claude_launches: Vec<Vec<String>> = launches(&claude_log(&f), false)
        .into_iter()
        .filter(|a| a.first().map(String::as_str) == Some("-p"))
        .collect();
    assert!(!claude_launches.is_empty(), "no launch was logged");
    for args in &claude_launches {
        assert!(has_run(args, &["--model", "sonnet", "--effort", "medium"]), "{args:?}");
    }
    let execs = launches(&codex_log(&f), true);
    assert_eq!(execs.len(), 2, "{execs:?}");
    for argv in &execs {
        assert!(argv.iter().any(|a| a == "model_reasoning_effort=\"medium\""), "{argv:?}");
    }

    // status: the kept run's two keys are medium
    let claude_run_id = c["run_id"].as_str().unwrap().to_string();
    let status = json_out(f.cmd().args(["status", &claude_task, "--json"]));
    let implementer = status["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == claude_run_id.as_str())
        .unwrap_or_else(|| panic!("no implementer run: {status}"));
    assert_eq!(implementer["effort_requested"], "medium", "{implementer}");
    assert_eq!(implementer["effort"], "medium", "{implementer}");

    // the run_started event
    let started = event_detail(&f, &claude_run_id, "run_started");
    assert_eq!(started["effort_requested"], "medium", "{started}");
    assert_eq!(started["effort"], "medium", "{started}");

    // the first review start without --effort is refused too
    refused_without_effort(&f, &claude_task, &["review", "start", &claude_task, "--profile", "claude-a", "--model", "sonnet"]);
    let review = review_start_json(&f, &claude_task, "claude-a", "sonnet", &["--effort", "medium"]);
    assert_eq!(review["effort_requested"], "medium", "{review}");
    assert_eq!(review["effort"], "medium", "{review}");
    assert_eq!(review["effort_note"], Value::Null, "{review}");
    wait_run(&f, review["run_id"].as_str().unwrap());

    // the review packet: three runs (the stopped one, the reroute and the
    // reviewer), all medium
    let packet = json_out(f.cmd().args(["review", "packet", &claude_task, "--json"]));
    let packet_runs = packet["runs"].as_array().unwrap();
    assert_eq!(packet_runs.len(), 3, "{packet}");
    for run in packet_runs {
        assert_eq!(run["effort"], "medium", "{run}");
    }
    let md = f.cmd().args(["review", "packet", &claude_task]).assert().success().get_output().stdout.clone();
    let md = String::from_utf8(md).unwrap();
    for run in packet_runs {
        let prefix = format!("- {} ", run["run_id"].as_str().unwrap());
        let lines: Vec<&str> = md.lines().filter(|l| l.starts_with(&prefix)).collect();
        assert_eq!(lines.len(), 1, "{prefix}: {md}");
        assert!(lines[0].contains(" | effort: medium"), "{}", lines[0]);
    }
    assert!(!md.contains(" | effort: high"), "{md}");
}

// A `launch` key in the task's own spec is never sent -----------------------

fn launch_in_task_spec_is_dropped(launch: &str) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let mut spec: Value = serde_json::from_str(common::Fixture::SPEC).unwrap();
    spec["launch"] = serde_json::from_str(launch).unwrap();
    let (task_id, _) = f.create_task(&spec.to_string());
    let d = dispatch_json(&f, &task_id, "claude-a", "sonnet", "ok", &["--effort", "medium"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let spec = run_spec(&f, &run_id);
    assert_eq!(spec["launch"]["effort_requested"], "medium", "{spec}");
    assert_eq!(spec["launch"]["effort"], "medium", "{spec}");
    let all: Vec<Vec<String>> = launches(&claude_log(&f), false)
        .into_iter()
        .filter(|a| a.first().map(String::as_str) == Some("-p"))
        .collect();
    assert!(!all.is_empty(), "no launch was logged");
    for args in &all {
        assert!(has_run(args, &["--effort", "medium"]), "{args:?}");
        assert!(!args.iter().any(|a| a == "ultra"), "{args:?}");
    }
    let status = json_out(f.cmd().args(["status", &task_id, "--json"]));
    let run = &status["runs"][0];
    assert_eq!(run["effort"], "medium", "{run}");
    assert_eq!(run["effort_requested"], "medium", "{run}");
}

#[test]
fn a_launch_key_in_the_task_spec_is_dropped_and_never_sent() {
    launch_in_task_spec_is_dropped(r#"{"effort":"ultra","effort_requested":"ultra","effort_note":null}"#);
}

#[test]
fn a_valid_launch_key_in_the_task_spec_is_dropped_too() {
    launch_in_task_spec_is_dropped(r#"{"effort":"high"}"#);
}

// AC2: rework and launch live side by side ---------------------------------

#[test]
fn a_rework_dispatch_with_effort_keeps_both_rework_and_launch_in_the_run_spec() {
    let f = common::fixture();
    let task_id = candidate_ready(&f);
    let rev = json_out(
        envd(&f, "review_reject", &task_id)
            .args(["review", "start", &task_id, "--profile", "claude-a", "--model", "haiku", "--json"]),
    );
    wait_run(&f, rev["run_id"].as_str().unwrap());
    assert_eq!(f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30), "reject");

    let d = dispatch_json(&f, &task_id, "claude-b", "sonnet", "hooks", &["--effort", "high"]);
    let spec = run_spec(&f, d["run_id"].as_str().unwrap());
    assert!(spec.get("rework").is_some(), "{spec}");
    assert!(spec.get("launch").is_some(), "{spec}");
    assert_eq!(spec["launch"]["effort"], "high", "{spec}");
}

// AC5: resume keeps the original run's effort -------------------------------

#[test]
fn resume_reuses_the_effort_the_run_was_dispatched_with() {
    let (f, _cfg, task_id) = codex_setup();
    let d = dispatch_json(&f, &task_id, "codex-a", CODEX_MODEL, "quota", &["--effort", "high"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");

    f.cmd().args(["profile", "check", "codex-a"]).assert().success();
    envd(&f, "resume", &task_id)
        .args(["resume", &run_id, "--json"])
        .assert()
        .success();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    let execs = launches(&codex_log(&f), true);
    assert_eq!(execs.len(), 2, "{execs:?}");
    for argv in &execs {
        assert!(argv.iter().any(|a| a == "model_reasoning_effort=\"high\""), "{argv:?}");
    }
}

// AC6 (f): review start ------------------------------------------------------

#[test]
fn review_start_passes_effort_to_the_reviewer() {
    let f = common::fixture();
    let task_id = candidate_ready(&f);
    let r = review_start_json(&f, &task_id, "claude-a", "sonnet", &["--effort", "high"]);
    assert_eq!(r["effort"], "high", "{r}");
    assert_eq!(r["effort_requested"], "high", "{r}");
    let run_id = r["run_id"].as_str().unwrap().to_string();
    wait_run(&f, &run_id);

    let all = launches(&claude_log(&f), false);
    let reviewer = all.last().unwrap();
    assert!(has_run(reviewer, &["--effort", "high"]), "{reviewer:?}");
    assert!(has_run(reviewer, &["--model", "sonnet", "--effort", "high"]), "{reviewer:?}");
    let spec = run_spec(&f, &run_id);
    assert_eq!(spec["launch"]["effort"], "high", "{spec}");
    let started = event_detail(&f, &run_id, "run_started");
    assert_eq!(started["effort"], "high", "{started}");
}

// AGY-1 (AC4): agy's `--effort`. Everything below reads state through the
// CLI (`status`, `events`, `wait --run`) and fake-agy's argv log. ------------

/// An agy model name that carries its level, and one that does not.
const AGY_HIGH: &str = "gemini-3.8-flash-high";
const AGY_BARE: &str = "claude-sonnet-4-6";

fn agy_log(f: &common::Fixture) -> PathBuf {
    f.home_path().join("fake-agy-args.jsonl")
}

/// A fixture with an agy profile (its fake data dir) and one task.
fn agy_setup() -> (common::Fixture, PathBuf, String) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.home.path().join("profiles").join("agy-a");
    std::fs::create_dir_all(&cfg).unwrap();
    f.cmd()
        .args(["profile", "add", "agy-a", "--provider", "google", "--harness", "antigravity", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    (f, cfg, task_id)
}

fn agy_cmd(f: &common::Fixture, cfg: &Path, scenario: &str, task_id: &str) -> assert_cmd::Command {
    let mut c = envd(f, scenario, task_id);
    c.env("FAKE_AGY_DATA_DIR", cfg).env("FAKE_AGY_ARGS_LOG", agy_log(f));
    c
}

/// Every `-p` launch fake-agy logged, oldest first.
fn agy_launches(f: &common::Fixture) -> Vec<Vec<String>> {
    launches(&agy_log(f), false)
        .into_iter()
        .filter(|a| a.first().map(String::as_str) == Some("-p"))
        .collect()
}

fn no_effort_flag(args: &[String]) -> bool {
    !args.iter().any(|a| a == "--effort")
}

/// The `status TASK --json` entries.
fn status_runs(f: &common::Fixture, task_id: &str) -> Vec<Value> {
    let v = json_out(f.cmd().args(["status", task_id, "--json"]));
    v["runs"].as_array().unwrap().clone()
}

/// The task's status through `status TASK --json`.
fn task_status_cli(f: &common::Fixture, task_id: &str) -> String {
    let v = json_out(f.cmd().args(["status", task_id, "--json"]));
    v["status"].as_str().unwrap_or_else(|| panic!("no status in {v}")).to_string()
}

/// The newest `run_started` detail of `run_id`, through `events --json`.
fn run_started(f: &common::Fixture, task_id: &str, run_id: &str) -> Value {
    let v = json_out(f.cmd().args(["events", "--since", "0", "--task", task_id, "--json"]));
    v["events"]
        .as_array()
        .unwrap()
        .iter()
        .rev()
        .find(|e| e["run_id"] == run_id && e["type"] == "run_started")
        .map(|e| e["detail"].clone())
        .unwrap_or_else(|| panic!("no run_started for {run_id}: {v}"))
}

/// A launch that must exit 1 naming `--effort`, adding no run to `status`.
fn refused_for_effort(f: &common::Fixture, cfg: &Path, task_id: &str, args: &[&str]) {
    let before = status_runs(f, task_id).len();
    let out = agy_cmd(f, cfg, "ok", task_id).args(args).assert().failure().code(1).get_output().clone();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(stderr.contains("--effort is missing"), "{args:?}: {stderr}");
    assert!(!stderr.contains("has no effort setting"), "{stderr}");
    assert_eq!(status_runs(f, task_id).len(), before, "{args:?}");
}

/// (a) a carried level given as is: recorded, no note, no flag; (b) a higher
/// request lowered to the carried level with a note; (i) `ultra` on a bare
/// name lowered to `max`.
#[test]
fn agy_effort_follows_the_model_name_when_it_carries_a_level() {
    let (f, cfg, task_id) = agy_setup();
    let d = json_out(agy_cmd(&f, &cfg, "ok", &task_id)
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", AGY_HIGH, "--effort", "high", "--json"]));
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(d["effort"], "high", "{d}");
    assert_eq!(d["effort_requested"], "high", "{d}");
    assert_eq!(d["effort_note"], Value::Null, "{d}");
    assert!(!warnings(&d).iter().any(|w| w.contains("effort")), "{d}");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let all = agy_launches(&f);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(no_effort_flag(&all[0]), "{:?}", all[0]);
    assert!(has_run(&all[0], &["--model", AGY_HIGH, "--dangerously-skip-permissions"]), "{:?}", all[0]);
    let run = &status_runs(&f, &task_id)[0];
    assert_eq!((run["effort"].as_str(), run["effort_requested"].as_str()), (Some("high"), Some("high")), "{run}");
    let started = run_started(&f, &task_id, &run_id);
    assert_eq!((started["effort"].as_str(), started["effort_requested"].as_str()), (Some("high"), Some("high")), "{started}");

    // (b) a request above the carried level is lowered to it, with a note
    let (g, cfg2, task2) = agy_setup();
    let d = json_out(agy_cmd(&g, &cfg2, "ok", &task2)
        .args(["dispatch", &task2, "--profile", "agy-a", "--model", AGY_HIGH, "--effort", "max", "--json"]));
    assert_eq!(d["effort"], "high", "{d}");
    assert_eq!(d["effort_requested"], "max", "{d}");
    let note = d["effort_note"].as_str().unwrap_or_default();
    assert!(note.contains("not supported by gemini-3.8-flash-high"), "{d}");
    assert!(warnings(&d).iter().any(|w| w == note), "{d}");
    assert!(!warnings(&d).iter().any(|w| w.contains("has no effort setting")), "{d}");
    assert_eq!(g.wait_for(&task2, "120s").1, "in_review");
    let all = agy_launches(&g);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(no_effort_flag(&all[0]), "{:?}", all[0]);

    // (i) `ultra` on a bare name is lowered to agy's highest, `max`
    let (h, cfg3, task3) = agy_setup();
    let d = json_out(agy_cmd(&h, &cfg3, "ok", &task3)
        .args(["dispatch", &task3, "--profile", "agy-a", "--model", AGY_BARE, "--effort", "ultra", "--json"]));
    assert_eq!(d["effort"], "max", "{d}");
    assert_eq!(d["effort_requested"], "ultra", "{d}");
    assert!(d["effort_note"].as_str().unwrap_or_default().contains("ultra is not supported by claude-sonnet-4-6"), "{d}");
    assert_eq!(h.wait_for(&task3, "120s").1, "in_review");
    let all = agy_launches(&h);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(has_run(&all[0], &["--model", AGY_BARE, "--effort", "max", "--dangerously-skip-permissions"]), "{:?}", all[0]);
}

/// (c) a carried level with `--effort` left out is recorded with its note
/// and `effort_requested` null; (d) a bare name sends the flag right after
/// the model; (e) a bare name without `--effort` is refused and writes no run.
#[test]
fn agy_effort_is_optional_only_for_a_model_name_that_carries_it() {
    let (f, cfg, task_id) = agy_setup();
    let d = json_out(agy_cmd(&f, &cfg, "ok", &task_id)
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", AGY_HIGH, "--json"]));
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(d["effort"], "high", "{d}");
    assert_eq!(d["effort_requested"], Value::Null, "{d}");
    assert_eq!(d["effort_note"], "effort high is carried by the model variant gemini-3.8-flash-high", "{d}");
    assert!(warnings(&d).iter().any(|w| w.contains("carried by the model variant")), "{d}");
    assert!(!warnings(&d).iter().any(|w| w.contains("has no effort setting")), "{d}");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    assert!(no_effort_flag(&agy_launches(&f)[0]));
    let run = &status_runs(&f, &task_id)[0];
    assert_eq!(run["effort"], "high", "{run}");
    assert_eq!(run["effort_requested"], Value::Null, "{run}");
    let started = run_started(&f, &task_id, &run_id);
    assert_eq!(started["effort"], "high", "{started}");
    assert_eq!(started["effort_requested"], Value::Null, "{started}");

    // (e) a bare name needs --effort on the first launch; nothing is written
    let (g, cfg2, task2) = agy_setup();
    refused_for_effort(&g, &cfg2, &task2, &["dispatch", &task2, "--profile", "agy-a", "--model", AGY_BARE]);
    assert!(status_runs(&g, &task2).is_empty());
    assert_eq!(task_status_cli(&g, &task2), "queued");
    // (d) with it, the flag follows the model
    let d = json_out(agy_cmd(&g, &cfg2, "ok", &task2)
        .args(["dispatch", &task2, "--profile", "agy-a", "--model", AGY_BARE, "--effort", "medium", "--json"]));
    assert_eq!(d["effort"], "medium", "{d}");
    assert_eq!(d["effort_requested"], "medium", "{d}");
    assert_eq!(d["effort_note"], Value::Null, "{d}");
    assert_eq!(g.wait_for(&task2, "120s").1, "in_review");
    let all = agy_launches(&g);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(has_run(&all[0], &["--model", AGY_BARE, "--effort", "medium", "--dangerously-skip-permissions"]), "{:?}", all[0]);
    let run = &status_runs(&g, &task2)[0];
    assert_eq!((run["effort"].as_str(), run["effort_requested"].as_str()), (Some("medium"), Some("medium")), "{run}");
}

/// (f) `review start` on a carried-level name: the reviewer run records the
/// level and its argv has no flag; (i) a left-out value in the same attempt
/// keeps the requested effort for a bare name.
#[test]
fn agy_review_start_and_a_same_attempt_reroute_follow_the_same_effort_rules() {
    let f = common::fixture();
    let task_id = candidate_ready(&f);
    let cfg = f.home.path().join("profiles").join("agy-a");
    std::fs::create_dir_all(&cfg).unwrap();
    f.cmd()
        .args(["profile", "add", "agy-a", "--provider", "google", "--harness", "antigravity", "--config-dir"])
        .arg(&cfg)
        .assert()
        .success();
    let r = json_out(agy_cmd(&f, &cfg, "review_pass", &task_id)
        .args(["review", "start", &task_id, "--profile", "agy-a", "--model", AGY_HIGH, "--effort", "high", "--json"]));
    let reviewer = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(r["effort"], "high", "{r}");
    assert_eq!(r["effort_note"], Value::Null, "{r}");
    wait_run(&f, &reviewer);
    let started = run_started(&f, &task_id, &reviewer);
    assert_eq!(started["effort"], "high", "{started}");
    assert_eq!(started["effort_requested"], "high", "{started}");
    let all = agy_launches(&f);
    assert_eq!(all.len(), 1, "{all:?}");
    assert!(no_effort_flag(&all[0]), "{:?}", all[0]);
    assert!(has_run(&all[0], &["--model", AGY_HIGH, "--dangerously-skip-permissions"]), "{:?}", all[0]);

    // a bare name stopped by quota, then rerouted in the same attempt
    // without --effort: the requested level is kept and sent again
    let (g, cfg2, task2) = agy_setup();
    let first = json_out(agy_cmd(&g, &cfg2, "quota", &task2)
        .args(["dispatch", &task2, "--profile", "agy-a", "--model", AGY_BARE, "--effort", "medium", "--json"]));
    assert_eq!(g.wait_for(&task2, "120s").1, "interrupted");
    g.cmd().args(["profile", "check", "agy-a"]).assert().success();
    let again = json_out(agy_cmd(&g, &cfg2, "ok", &task2)
        .args(["dispatch", &task2, "--profile", "agy-a", "--json"]));
    assert_eq!(again["effort"], "medium", "{again}");
    assert_eq!(again["effort_requested"], "medium", "{again}");
    let kept = format!("effort medium kept from run {}", first["run_id"].as_str().unwrap());
    assert!(warnings(&again).iter().any(|w| w.contains(&kept)), "{again}");
    assert_eq!(g.wait_for(&task2, "120s").1, "in_review");
    let all = agy_launches(&g);
    assert_eq!(all.len(), 2, "{all:?}");
    for args in &all {
        assert!(has_run(args, &["--model", AGY_BARE, "--effort", "medium"]), "{args:?}");
    }
}

/// (g) `spec review start`: a carried-level name without `--effort`, a bare
/// name with it, and a bare name without it (refused, no run).
#[test]
fn agy_spec_review_start_follows_the_effort_rules_every_time() {
    let (f, cfg, task_id) = agy_setup();
    let carried = json_out(agy_cmd(&f, &cfg, "spec_review_approve", &task_id)
        .args(["spec", "review", "start", &task_id, "--profile", "agy-a", "--model", AGY_HIGH, "--json"]));
    let run1 = carried["run_id"].as_str().unwrap().to_string();
    assert_eq!(carried["effort"], "high", "{carried}");
    assert_eq!(carried["effort_requested"], Value::Null, "{carried}");
    assert!(carried["effort_note"].as_str().unwrap().contains("carried by the model variant gemini-3.8-flash-high"), "{carried}");
    let w = json_out(f.cmd().args(["wait", "--run", &run1, "--timeout", "180s", "--json"]));
    assert_eq!(w["verdict"], "APPROVE", "{w}");
    let started = run_started(&f, &task_id, &run1);
    assert_eq!(started["effort"], "high", "{started}");
    assert_eq!(started["effort_requested"], Value::Null, "{started}");

    let before = status_runs(&f, &task_id).len();
    refused_for_effort(&f, &cfg, &task_id, &["spec", "review", "start", &task_id, "--profile", "agy-a", "--model", AGY_BARE]);
    assert_eq!(status_runs(&f, &task_id).len(), before);

    let bare = json_out(agy_cmd(&f, &cfg, "spec_review_approve", &task_id)
        .args(["spec", "review", "start", &task_id, "--profile", "agy-a", "--model", AGY_BARE, "--effort", "low", "--json"]));
    let run2 = bare["run_id"].as_str().unwrap().to_string();
    assert_eq!(bare["effort"], "low", "{bare}");
    assert_eq!(bare["effort_requested"], "low", "{bare}");
    assert_eq!(bare["effort_note"], Value::Null, "{bare}");
    let w = json_out(f.cmd().args(["wait", "--run", &run2, "--timeout", "180s", "--json"]));
    assert_eq!(w["verdict"], "APPROVE", "{w}");
    let started = run_started(&f, &task_id, &run2);
    assert_eq!((started["effort"].as_str(), started["effort_requested"].as_str()), (Some("low"), Some("low")), "{started}");

    let all = agy_launches(&f);
    assert_eq!(all.len(), 2, "{all:?}");
    assert!(no_effort_flag(&all[0]), "{:?}", all[0]);
    assert!(has_run(&all[0], &["--model", AGY_HIGH, "--dangerously-skip-permissions"]), "{:?}", all[0]);
    assert!(has_run(&all[1], &["--model", AGY_BARE, "--effort", "low", "--dangerously-skip-permissions"]), "{:?}", all[1]);
    assert!(all.iter().all(|a| a.iter().any(|x| x == "--json-schema")), "{all:?}");
}

/// (h) a resume reuses the run's launch for both kinds of name: no flag for
/// a carried level, the same flag for a bare name.
#[test]
fn agy_resume_reuses_the_runs_effort_for_both_kinds_of_model_name() {
    let none: &[&str] = &[];
    let medium: &[&str] = &["--effort", "medium"];
    for (model, effort_args, expect_flag) in [(AGY_HIGH, none, false), (AGY_BARE, medium, true)] {
        let (f, cfg, task_id) = agy_setup();
        let d = json_out(agy_cmd(&f, &cfg, "quota", &task_id)
            .args(["dispatch", &task_id, "--profile", "agy-a", "--model", model, "--json"])
            .args(effort_args));
        let run_id = d["run_id"].as_str().unwrap().to_string();
        assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted", "{model}");
        f.cmd().args(["profile", "check", "agy-a"]).assert().success();
        agy_cmd(&f, &cfg, "resume", &task_id).args(["resume", &run_id, "--json"]).assert().success();
        assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{model}");
        let all = agy_launches(&f);
        assert_eq!(all.len(), 2, "{model}: {all:?}");
        for args in &all {
            assert!(has_run(args, &["--model", model]), "{model}: {args:?}");
            if expect_flag {
                assert!(has_run(args, &["--model", model, "--effort", "medium"]), "{model}: {args:?}");
            } else {
                assert!(no_effort_flag(args), "{model}: {args:?}");
            }
        }
        assert!(all[1].iter().any(|a| a == "--conversation"), "{model}: {:?}", all[1]);
        let run = &status_runs(&f, &task_id)[0];
        let expected = if expect_flag { "medium" } else { "high" };
        assert_eq!(run["effort"], expected, "{model}: {run}");
    }
}

// SMALL-9b: a launch without `--timeout` gets the default for the effort
// sent; a given `--timeout` wins; a timed-out run's next step follows the
// run. Read through the CLI only: `--json`, `status`, `events`, the help and
// the fakes' argument logs. ---------------------------------------------------

mod timeout_by_effort {
    use super::common::{self, Fixture};
    use serde_json::{json, Value};
    use std::fs;
    use std::path::PathBuf;

    const REASON: &str = "SMALL-9b test: the fake worker stands in for a real one";
    /// Two acceptance criteria, for the review flows.
    const SPEC: &str = r#"{"goal":"keep the greeting right","acceptance_criteria":["AC1: greets the user","AC2: exits zero"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal"}"#;
    /// The spec fake-claude's `spec_review_edit` proposes edits to.
    const EDITABLE: &str = r#"{"goal":"guard the ledger","acceptance_criteria":["AC1: rejects a double spend","AC2: logs the rejection"],"constraints":["no new crate"],"non_goals":["rewriting the transport","changing the log format"],"excluded_alternatives":[],"risk":"important"}"#;
    const HOUR: u64 = 3600;

    /// `f.cmd()` without any inherited variable whose name holds KEY, TOKEN,
    /// SECRET or PASSWORD (names are read, never values), nor the two
    /// benchmark keys.
    fn cmd(f: &Fixture) -> assert_cmd::Command {
        let mut c = f.cmd();
        for (name, _) in std::env::vars_os() {
            let upper = name.to_string_lossy().to_uppercase();
            if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
                c.env_remove(&name);
            }
        }
        c.env_remove("AISTUPIDLEVEL_API_KEY").env_remove("ARTIFICIALANALYSIS_API_KEY");
        c
    }

    /// Runs `c`; panics with its exit code, stdout and stderr unless it
    /// exits 0; its stdout.
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

    fn ok_json(c: assert_cmd::Command, what: &str) -> Value {
        let out = run_ok(c, what);
        serde_json::from_slice(&out)
            .unwrap_or_else(|e| panic!("{what}: not JSON ({e}): {}", String::from_utf8_lossy(&out)))
    }

    /// `thoth ARGS` (the caller gives `--json`), exit 0, as JSON.
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

    fn create_task(f: &Fixture, spec: &str) -> String {
        let file = f.home_path().join("spec.json");
        fs::write(&file, spec).unwrap();
        let shown = file.display().to_string();
        s(&cli(f, &["task", "create", "-f", &shown, "--json"])["task_id"])
    }

    /// A fresh THOTH_HOME and repository, initialised, with claude-a and
    /// claude-b registered.
    fn init() -> Fixture {
        let f = common::fixture();
        run_ok({
            let mut c = cmd(&f);
            c.arg("init");
            c
        }, "init");
        add_profile(&f, "anthropic", "claude-code", "claude-a");
        add_profile(&f, "anthropic", "claude-code", "claude-b");
        f
    }

    /// `wait --run RUN` until the run is terminal; its `--json`.
    fn wait_run(f: &Fixture, run: &str) -> Value {
        cli(f, &["wait", "--run", run, "--timeout", "180s", "--json"])
    }

    /// The run's entry in `status TASK --json`, found by its id.
    fn run_of(f: &Fixture, task: &str, run: &str) -> Value {
        let v = cli(f, &["status", task, "--json"]);
        v["runs"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["run_id"] == run)
            .cloned()
            .unwrap_or_else(|| panic!("no run {run}: {v}"))
    }

    /// The `ty` event of `run`, through `events --task TASK --json`.
    fn event_of(f: &Fixture, task: &str, run: &str, ty: &str) -> Value {
        let v = cli(f, &["events", "--since", "0", "--task", task, "--json"]);
        v["events"]
            .as_array()
            .unwrap()
            .iter()
            .rev()
            .find(|e| e["run_id"] == run && e["type"] == ty)
            .cloned()
            .unwrap_or_else(|| panic!("no {ty} for {run}: {v}"))
    }

    /// A launch command with the fake's scenario and the task it works on.
    fn launch(f: &Fixture, task: &str, scenario: &str, args: &[&str]) -> assert_cmd::Command {
        let mut c = cmd(f);
        c.env("FAKE_SCENARIO", scenario).env("FAKE_TASK_ID", task).args(args);
        c
    }

    /// `dispatch TASK --profile claude-a --reason R --json` plus `extra`.
    fn dispatch(f: &Fixture, task: &str, scenario: &str, extra: &[&str]) -> Value {
        let mut c = launch(f, task, scenario, &["dispatch", task, "--profile", "claude-a", "--reason", REASON, "--json"]);
        c.args(extra);
        ok_json(c, "dispatch")
    }

    // AC1: dispatch ------------------------------------------------------------

    #[test]
    fn a_dispatch_without_timeout_gets_the_default_for_the_effort_sent() {
        for (model, effort, want) in [
            ("sonnet", Some("low"), HOUR),
            ("sonnet", Some("high"), 3 * HOUR),
            ("sonnet", Some("max"), 4 * HOUR),
            ("haiku", None, HOUR),
        ] {
            let f = init();
            let task = create_task(&f, Fixture::SPEC);
            let mut extra = vec!["--model", model];
            if let Some(e) = effort {
                extra.extend(["--effort", e]);
            }
            let d = dispatch(&f, &task, "ok", &extra);
            let run = s(&d["run_id"]);
            assert_eq!(d["effort"], json!(effort), "{d}");
            assert_eq!(d["timeout_seconds"], want, "{model} {effort:?}: {d}");
            wait_run(&f, &run);
            let r = run_of(&f, &task, &run);
            assert_eq!(r["effort"], json!(effort), "{r}");
            assert_eq!(r["timeout_seconds"], want, "{model} {effort:?}: {r}");
        }
    }

    #[test]
    fn a_given_timeout_is_used_as_given() {
        let f = init();
        let task = create_task(&f, Fixture::SPEC);
        let d = dispatch(&f, &task, "ok", &["--model", "sonnet", "--effort", "high", "--timeout", "45m"]);
        let run = s(&d["run_id"]);
        assert_eq!(d["timeout_seconds"], 2700, "{d}");
        wait_run(&f, &run);
        assert_eq!(run_of(&f, &task, &run)["timeout_seconds"], 2700);
    }

    /// A reroute in the same attempt keeps the previous run's effort, and the
    /// default follows that effort; the previous run's own `--timeout` is not
    /// kept.
    #[test]
    fn a_reroute_that_keeps_high_gets_the_high_default_not_the_previous_timeout() {
        let f = init();
        let task = create_task(&f, Fixture::SPEC);
        let first = dispatch(&f, &task, "quota", &["--model", "sonnet", "--effort", "high", "--timeout", "20m"]);
        assert_eq!(first["timeout_seconds"], 1200, "{first}");
        let first_run = s(&first["run_id"]);
        assert_eq!(wait_run(&f, &first_run)["status"], "interrupted");

        // neither --model nor --effort nor --timeout: the reroute keeps sonnet
        // and high from the stopped run
        let c = launch(&f, &task, "ok", &["dispatch", &task, "--profile", "claude-b", "--reason", REASON, "--json"]);
        let d = ok_json(c, "reroute dispatch");
        let run = s(&d["run_id"]);
        assert_eq!(d["attempt"], first["attempt"], "{d}");
        assert_eq!(d["effort"], "high", "{d}");
        assert_eq!(d["timeout_seconds"], 3 * HOUR, "{d}");
        wait_run(&f, &run);
        let r = run_of(&f, &task, &run);
        assert_eq!(r["timeout_seconds"], 3 * HOUR, "{r}");
        assert_eq!(run_of(&f, &task, &first_run)["timeout_seconds"], 1200);
    }

    /// An agy model name that carries `high` is that effort: its default, and
    /// agy's `--print-timeout` 300 seconds past it.
    #[test]
    fn an_agy_dispatch_passes_print_timeout_from_the_default() {
        let f = common::fixture();
        run_ok({
            let mut c = cmd(&f);
            c.arg("init");
            c
        }, "init");
        let data = add_profile(&f, "google", "antigravity", "agy-a");
        let log = f.home_path().join("fake-agy-args.jsonl");
        let task = create_task(&f, Fixture::SPEC);
        let mut c = launch(&f, &task, "ok", &["dispatch", &task, "--profile", "agy-a", "--model", "gemini-3.8-flash-high", "--reason", REASON, "--json"]);
        c.env("FAKE_AGY_DATA_DIR", &data).env("FAKE_AGY_ARGS_LOG", &log);
        let d = ok_json(c, "agy dispatch");
        let run = s(&d["run_id"]);
        assert_eq!(d["effort"], "high", "{d}");
        assert_eq!(d["timeout_seconds"], 3 * HOUR, "{d}");
        wait_run(&f, &run);
        let launches: Vec<Vec<String>> = fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["args"][0] == "-p")
            .map(|v| v["args"].as_array().unwrap().iter().map(s).collect())
            .collect();
        assert_eq!(launches.len(), 1, "{launches:?}");
        let want = format!("{}s", 3 * HOUR + 300);
        let at = launches[0].iter().position(|a| a == "--print-timeout").expect("--print-timeout");
        assert_eq!(launches[0][at + 1], want, "{:?}", launches[0]);
    }

    /// A fresh THOTH_HOME with codex-a, whose model cache offers `CODEX_MODEL`
    /// at `levels` only, and a task.
    fn codex_init(levels: &[&str]) -> (Fixture, String) {
        let f = common::fixture();
        run_ok({
            let mut c = cmd(&f);
            c.arg("init");
            c
        }, "init");
        let cfg = add_profile(&f, "openai", "codex", "codex-a");
        let offered: Vec<Value> = levels.iter().map(|e| json!({"effort": e})).collect();
        let cache = json!({"models": [{"slug": super::CODEX_MODEL, "supported_reasoning_levels": offered}]});
        fs::write(cfg.join("models_cache.json"), cache.to_string()).unwrap();
        let task = create_task(&f, Fixture::SPEC);
        (f, task)
    }

    /// The `model_reasoning_effort` of every `exec` fake-codex logged.
    fn codex_efforts(log: &std::path::Path) -> Vec<String> {
        fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .filter(|v| v["args"][0] == "exec")
            .flat_map(|v| {
                v["args"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(s)
                    .filter_map(|a| a.strip_prefix("model_reasoning_effort=").map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// codex lowers an effort its model does not offer; the default follows
    /// the level it lowered to, not the one asked for, in `--json` and in the
    /// human first line alike.
    #[test]
    fn a_codex_dispatch_gets_the_default_for_the_lowered_effort() {
        // --json: max is not offered, high is sent; max would have been 4h
        let (f, task) = codex_init(&["low", "medium", "high"]);
        let log = f.home_path().join("fake-codex-args.jsonl");
        let mut c = launch(&f, &task, "ok", &["dispatch", &task, "--profile", "codex-a", "--model", super::CODEX_MODEL, "--effort", "max", "--reason", REASON, "--json"]);
        c.env("FAKE_CODEX_ARGS_LOG", &log);
        let d = ok_json(c, "codex dispatch --json");
        let run = s(&d["run_id"]);
        assert_eq!(d["effort_requested"], "max", "{d}");
        assert_eq!(d["effort"], "high", "{d}");
        assert_eq!(d["timeout_seconds"], 3 * HOUR, "{d}");
        wait_run(&f, &run);
        let r = run_of(&f, &task, &run);
        assert_eq!(r["effort"], "high", "{r}");
        assert_eq!(r["timeout_seconds"], 3 * HOUR, "{r}");
        assert_eq!(codex_efforts(&log), ["\"high\""]);

        // human: high is not offered, medium is sent; high would have been 3h
        let (f, task) = codex_init(&["low", "medium"]);
        let log = f.home_path().join("fake-codex-args.jsonl");
        let mut c = launch(&f, &task, "ok", &["dispatch", &task, "--profile", "codex-a", "--model", super::CODEX_MODEL, "--effort", "high", "--reason", REASON]);
        c.env("FAKE_CODEX_ARGS_LOG", &log);
        let out = c.output().expect("spawn thoth");
        let stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(out.status.success(), "stdout: {stdout}\nstderr: {stderr}");
        assert!(stderr.contains("high is not supported by gpt-5.6-luna; using medium"), "{stderr}");
        let first = stdout.lines().next().unwrap_or_default().to_string();
        assert!(first.ends_with(", timeout 3600s)"), "{stdout}");
        let run = first
            .strip_prefix("dispatched ")
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_else(|| panic!("no run id in {first}"))
            .to_string();
        wait_run(&f, &run);
        let r = run_of(&f, &task, &run);
        assert_eq!(r["effort_requested"], "high", "{r}");
        assert_eq!(r["effort"], "medium", "{r}");
        assert_eq!(r["timeout_seconds"], HOUR, "{r}");
        assert_eq!(codex_efforts(&log), ["\"medium\""]);
    }

    // AC2: the human first line ------------------------------------------------

    #[test]
    fn the_human_first_line_ends_with_the_timeout() {
        let f = init();
        let task = create_task(&f, Fixture::SPEC);
        let out = run_ok(
            launch(&f, &task, "ok", &["dispatch", &task, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium", "--reason", REASON]),
            "dispatch",
        );
        let out = String::from_utf8(out).unwrap();
        let first = out.lines().next().unwrap_or_default().to_string();
        assert!(first.ends_with(", timeout 3600s)"), "{out}");
        assert!(first.contains(", runner pid "), "{out}");
        let run = first
            .strip_prefix("dispatched ")
            .and_then(|rest| rest.split(' ').next())
            .unwrap_or_else(|| panic!("no run id in {first}"));
        wait_run(&f, run);
    }

    // AC1: help ----------------------------------------------------------------

    #[test]
    fn the_launch_commands_help_says_the_default_follows_the_effort() {
        let f = common::fixture();
        let help = |args: &[&str]| {
            let mut c = cmd(&f);
            c.args(args).arg("--help");
            String::from_utf8(run_ok(c, &args.join(" "))).unwrap()
        };
        for args in [
            &["dispatch"][..],
            &["review", "start"],
            &["review", "confirm"],
            &["spec", "review", "start"],
            &["spec", "review", "confirm"],
        ] {
            let text = help(args);
            let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
            assert!(!text.contains("[default: 30m]"), "{args:?}: {text}");
            assert!(flat.contains("the default follows the effort sent"), "{args:?}: {text}");
        }
        assert!(help(&["wait"]).contains("[default: 30m]"));
        assert!(help(&["verify"]).contains("[default: 600s]"));
    }

    // AC1: review start, review confirm ----------------------------------------

    /// `count` numbered lines, `prefix N`.
    fn lines(prefix: &str, count: usize) -> String {
        (1..=count).map(|i| format!("{prefix} {i}\n")).collect()
    }

    /// A task of SPEC at `in_review`, implemented by claude-a.
    fn in_review() -> (Fixture, String) {
        let f = common::fixture();
        let repo = f.repo_path();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::write(repo.join("src/greet.rs"), lines("line", 30)).unwrap();
        fs::write(repo.join(".gitattributes"), "* -text\n").unwrap();
        common::git(&repo, &["add", "-A"]);
        common::git(&repo, &["commit", "-m", "candidate files"]);
        run_ok({
            let mut c = cmd(&f);
            c.arg("init");
            c
        }, "init");
        add_profile(&f, "anthropic", "claude-code", "claude-a");
        add_profile(&f, "anthropic", "claude-code", "claude-b");
        let task = create_task(&f, SPEC);
        let d = dispatch(&f, &task, "ok", &["--model", "sonnet", "--effort", "medium"]);
        wait_run(&f, &s(&d["run_id"]));
        assert_eq!(cli(&f, &["status", &task, "--json"])["status"], "in_review");
        (f, task)
    }

    /// The run's `timeout_seconds` in `status` and in its `started` event.
    fn assert_timeout(f: &Fixture, task: &str, run: &str, started: &str, want: u64) {
        assert_eq!(run_of(f, task, run)["timeout_seconds"], want, "{run}");
        let e = event_of(f, task, run, started);
        assert_eq!(e["detail"]["timeout_seconds"], want, "{e}");
    }

    #[test]
    fn review_start_defaults_by_effort_and_takes_a_given_timeout() {
        for (extra, want) in [(&["--effort", "high"][..], 3 * HOUR), (&["--effort", "medium", "--timeout", "20m"], 1200)] {
            let (f, task) = in_review();
            let mut c = launch(&f, &task, "review_pass", &["review", "start", &task, "--profile", "claude-b", "--model", "sonnet", "--reason", REASON, "--json"]);
            c.args(extra);
            let r = ok_json(c, "review start");
            let run = s(&r["run_id"]);
            wait_run(&f, &run);
            assert_timeout(&f, &task, &run, "review_started", want);
        }
    }

    /// RFIX-1, as tests/review_fix.rs does it: an applied fix of AC1 by an
    /// edit review, verified by an approved green check.
    fn pending_fix() -> (Fixture, String) {
        let (f, task) = in_review();
        cli(&f, &["checks", "add", "green", "--cmd", "git --version", "--shell", "argv", "--json"]);
        cli(&f, &["checks", "approve", "--reason", "test approval", "--json"]);
        let evidence = cli(&f, &["evidence", "list", &task, "--json"]);
        let code = s(&evidence["evidence"][0]["code"]);
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
        let mut c = launch(&f, &task, "review_fix", &["review", "start", &task, "--profile", "claude-b", "--model", "opus", "--effort", "medium", "--edit", "--reason", REASON, "--json"]);
        c.env("FAKE_FIX_PLAN", &plan_file);
        let edit = s(&ok_json(c, "review start --edit")["run_id"]);
        let w = wait_run(&f, &edit);
        assert_eq!(w["pending_fix"], true, "{w}");
        let mut c = cmd(&f);
        c.args(["verify", &edit, "--json"]);
        run_ok(c, "verify");
        (f, task)
    }

    #[test]
    fn review_confirm_defaults_by_effort_and_takes_a_given_timeout() {
        for (extra, want) in [(&["--effort", "high"][..], 3 * HOUR), (&["--effort", "high", "--timeout", "20m"], 1200)] {
            let (f, task) = pending_fix();
            let mut c = launch(&f, &task, "fix_confirm_approve", &["review", "confirm", &task, "--profile", "claude-a", "--model", "sonnet", "--reason", REASON, "--json"]);
            c.args(extra);
            let r = ok_json(c, "review confirm");
            let run = s(&r["run_id"]);
            wait_run(&f, &run);
            assert_timeout(&f, &task, &run, "review_started", want);
        }
    }

    // AC1: spec review start, spec review confirm ------------------------------

    #[test]
    fn spec_review_start_defaults_by_effort_and_takes_a_given_timeout() {
        for (spec, scenario, extra, want) in [
            (SPEC, "spec_review_approve", &["--effort", "high"][..], 3 * HOUR),
            (EDITABLE, "spec_review_edit", &["--effort", "medium", "--timeout", "20m", "--edit"], 1200),
        ] {
            let f = init();
            let task = create_task(&f, spec);
            let mut c = launch(&f, &task, scenario, &["spec", "review", "start", &task, "--profile", "claude-a", "--model", "sonnet", "--reason", REASON, "--json"]);
            c.args(extra);
            let r = ok_json(c, "spec review start");
            let run = s(&r["run_id"]);
            wait_run(&f, &run);
            assert_timeout(&f, &task, &run, "spec_review_started", want);
        }
    }

    /// SR-3, as tests/spec_review_edit.rs does it: claude-b's edit review of
    /// EDITABLE, its edits pending. haiku has no effort setting, so that
    /// review gets the one-hour default.
    fn pending_edits() -> (Fixture, String) {
        let f = init();
        let task = create_task(&f, EDITABLE);
        let c = launch(&f, &task, "spec_review_edit", &["spec", "review", "start", &task, "--profile", "claude-b", "--model", "haiku", "--edit", "--reason", REASON, "--json"]);
        let run = s(&ok_json(c, "spec review start --edit")["run_id"]);
        assert_eq!(wait_run(&f, &run)["verdict"], "REVISE");
        assert_timeout(&f, &task, &run, "spec_review_started", HOUR);
        assert_eq!(cli(&f, &["spec", "review", "show", &task, "--json"])["pending_edits"], true);
        (f, task)
    }

    #[test]
    fn spec_review_confirm_defaults_by_effort_and_takes_a_given_timeout() {
        for (extra, want) in [(&["--effort", "high"][..], 3 * HOUR), (&["--effort", "high", "--timeout", "20m"], 1200)] {
            let (f, task) = pending_edits();
            let mut c = launch(&f, &task, "spec_confirm_approve", &["spec", "review", "confirm", &task, "--profile", "claude-a", "--model", "sonnet", "--reason", REASON, "--json"]);
            c.args(extra);
            let r = ok_json(c, "spec review confirm");
            let run = s(&r["run_id"]);
            wait_run(&f, &run);
            assert_timeout(&f, &task, &run, "spec_review_started", want);
        }
    }

    // AC3: the timeout next step -----------------------------------------------

    #[test]
    fn a_timed_out_runs_next_step_doubles_the_larger_of_its_timeout_and_its_default() {
        for (extra, model, want) in [
            (&[][..], "haiku", "2h"),
            (&["--effort", "high"], "sonnet", "6h"),
        ] {
            let f = init();
            let task = create_task(&f, Fixture::SPEC);
            let mut args = vec!["--model", model, "--timeout", "5s"];
            args.extend(extra);
            let d = dispatch(&f, &task, "slow", &args);
            let run = s(&d["run_id"]);
            let w = wait_run(&f, &run);
            assert_eq!(w["status"], "failed", "{w}");
            let r = run_of(&f, &task, &run);
            assert_eq!(r["interrupted_reason"], "timeout", "{r}");
            assert_eq!(
                r["next_step"],
                format!(
                    "the run hit its wall-clock timeout; dispatch again with a larger --timeout: \
                     thoth dispatch {task} --profile claude-a --model {model} --timeout {want}"
                ),
                "{r}"
            );
        }
    }
}
