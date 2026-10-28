//! RD-3 (RD-2, H15): a worker never gets Thoth's own key variables, nor any
//! variable the masker takes for a secret; every other variable still
//! reaches it. The fakes log variable names only, never a value.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::Path;

/// The variables every launch here sets: two score keys, a variable named
/// like a secret, and a plain one.
const V: [(&str, &str); 4] = [
    ("AISTUPIDLEVEL_API_KEY", "dummy-rd2-asl"),
    ("ARTIFICIALANALYSIS_API_KEY", "dummy-rd2-aa"),
    ("RD2_TEST_TOKEN", "dummy-rd2-env-1"),
    ("RD2_PLAIN", "hello"),
];

/// Each line of a `FAKE_ENV_LOG` file: the names one launch saw.
fn names(path: &Path) -> Vec<Vec<String>> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect()
        })
        .collect()
}

/// Every launch in the log lost the secret variables and kept the others.
/// A failure names only the variable looked for.
fn assert_stripped(path: &Path) {
    let launches = names(path);
    assert!(!launches.is_empty(), "the worker logged no launch");
    for seen in &launches {
        for gone in ["AISTUPIDLEVEL_API_KEY", "ARTIFICIALANALYSIS_API_KEY", "RD2_TEST_TOKEN"] {
            assert!(!seen.iter().any(|n| n == gone), "{gone} reached the worker");
        }
        for kept in ["RD2_PLAIN", "THOTH_RUN_ID"] {
            assert!(seen.iter().any(|n| n == kept), "{kept} did not reach the worker");
        }
        assert!(seen.iter().any(|n| n.eq_ignore_ascii_case("PATH")), "PATH did not reach the worker");
    }
}

fn with_v(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for (k, v) in V {
        c.env(k, v);
    }
    c
}

fn stdout_json(mut c: assert_cmd::Command, what: &str) -> Value {
    let out = c.output().unwrap();
    assert!(out.status.success(), "{what} exited {:?}: {}", out.status.code(), String::from_utf8_lossy(&out.stderr));
    serde_json::from_slice(&out.stdout).unwrap()
}

/// AC12 worker_env (t1).
#[test]
fn worker_env_claude_implementer_and_reviewer_lose_the_secret_vars() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);

    let log = f.home_path().join("env-impl.jsonl");
    let mut c = with_v(&f);
    c.env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_ENV_LOG", &log)
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    stdout_json(c, "dispatch");
    assert_eq!(f.wait_for(&task, "180s").1, "in_review");
    assert_stripped(&log);

    let log = f.home_path().join("env-review.jsonl");
    let mut c = with_v(&f);
    c.env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_ENV_LOG", &log)
        .args(["review", "start", &task, "--profile", "claude-b", "--model", "haiku", "--json"]);
    let r = stdout_json(c, "review start");
    let run = r["run_id"].as_str().unwrap().to_string();
    f.cmd().args(["wait", "--run", &run, "--timeout", "180s", "--json"]).assert().success();
    assert_stripped(&log);
}

/// AC12 worker_env (t2).
#[test]
fn worker_env_codex_worker_loses_the_secret_vars() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_codex_profile("codex-a");
    let (task, _) = f.create_task(Fixture::SPEC_TWO_AC);

    let log = f.home_path().join("env-codex.jsonl");
    let mut c = with_v(&f);
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_ENV_LOG", &log)
        .args(["dispatch", &task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--json"]);
    stdout_json(c, "dispatch");
    f.wait_for(&task, "180s");
    assert_stripped(&log);
}
