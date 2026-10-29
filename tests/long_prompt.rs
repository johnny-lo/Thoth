//! W3: claude and codex take the prompt on stdin, so a prompt longer than
//! Windows' command-line limit still launches; agy keeps it on argv, and any
//! command line over thoth's cap fails the run with a clear error before
//! anything is spawned. Always the fakes, in a temporary THOTH_HOME.

mod common;

use serde_json::Value;
use std::path::{Path, PathBuf};

/// `Fixture::add_agy_profile`: registers an antigravity profile (copied from
/// tests/antigravity.rs, where the helper lives).
trait AgyFixture {
    fn add_agy_profile(&self, id: &str) -> std::path::PathBuf;
}

impl AgyFixture for common::Fixture {
    fn add_agy_profile(&self, id: &str) -> std::path::PathBuf {
        let cfg = self.home.path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        self.cmd().args(["profile", "add", id, "--provider", "google", "--harness",
            "antigravity", "--config-dir"]).arg(&cfg).assert().success();
        cfg
    }
}

/// `common::Fixture::SPEC`'s shape, with one 40,000-character constraint:
/// past the 32,767-character command line, under spec lint's 40,960-byte limit.
/// LEAN-1: important risk, where a spec over 20,480 bytes is only warned about
/// (at low and normal it is refused); the dispatches skip the spec review that
/// risk asks for, with `SKIP`.
fn long_spec() -> String {
    serde_json::json!({
        "goal": "say hello",
        "acceptance_criteria": ["AC1: prints hello"],
        "constraints": ["x".repeat(40_000)],
        "non_goals": [],
        "excluded_alternatives": [],
        "risk": "important",
    })
    .to_string()
}

/// The `--skip-spec-review` reason of the long-spec dispatches.
const SKIP: &str = "long prompt fixture: the spec review gate is covered by tests/spec_review.rs";

/// The argv of every launch a fake logged, oldest first.
fn launches(log: &Path) -> Vec<Vec<String>> {
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
                .collect()
        })
        .collect()
}

fn prompt_copy(f: &common::Fixture) -> PathBuf {
    f.home_path().join("prompt-copy.txt")
}

/// What the fake received is byte for byte what thoth wrote to prompt.txt,
/// and that is the long prompt.
fn copy_matches(f: &common::Fixture, run_dir: &Path) {
    let sent = std::fs::read(run_dir.join("prompt.txt")).unwrap();
    let got = std::fs::read(prompt_copy(f)).expect("the fake wrote the prompt it received");
    assert!(sent.len() > 40_000, "prompt.txt is {} bytes", sent.len());
    assert!(got == sent, "the fake received {} bytes, prompt.txt has {}", got.len(), sent.len());
}

fn json_out(c: &mut assert_cmd::Command) -> Value {
    let out = c.assert().success().get_output().stdout.clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn error_text(f: &common::Fixture, run_id: &str) -> String {
    f.state_conn()
        .query_row("SELECT error_text FROM runs WHERE run_id=?1", [run_id], |r| {
            r.get::<_, Option<String>>(0)
        })
        .unwrap()
        .unwrap_or_default()
}

fn assert_too_long(f: &common::Fixture, task_id: &str, run_id: &str) {
    assert_eq!(
        f.wait_for_task_status(task_id, &["failed", "in_review", "interrupted"], 60),
        "failed"
    );
    let err = error_text(f, run_id);
    assert!(err.starts_with("command line too long: "), "{err}");
    assert!(err.contains("30000"), "{err}");
}

#[test]
fn claude_prompt_goes_through_stdin() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(&long_spec());
    let log = f.home_path().join("fake-claude-args.jsonl");
    let d = json_out(
        f.cmd()
            .env("FAKE_SCENARIO", "ok")
            .env("FAKE_TASK_ID", &task_id)
            .env("FAKE_CLAUDE_ARGS_LOG", &log)
            .env("FAKE_PROMPT_COPY", prompt_copy(&f))
            .args(["dispatch", &task_id, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium", "--json"])
            .args(["--skip-spec-review", SKIP]),
    );
    assert_eq!(
        f.wait_for_task_status(&task_id, &["in_review", "failed", "interrupted"], 60),
        "in_review"
    );
    copy_matches(&f, &PathBuf::from(d["run_dir"].as_str().unwrap()));
    let workers: Vec<Vec<String>> =
        launches(&log).into_iter().filter(|a| a.first().map(String::as_str) == Some("-p")).collect();
    assert_eq!(workers.len(), 1, "{workers:?}");
    assert_eq!(workers[0][1], "--session-id");
    for a in &workers[0] {
        assert!(a.len() < 1_000, "an argument of {} chars", a.len());
    }
}

#[test]
fn codex_prompt_goes_through_stdin() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_codex_profile("codex-a");
    let (task_id, _) = f.create_task(&long_spec());
    let log = f.home_path().join("fake-codex-args.jsonl");
    let d = json_out(
        f.cmd()
            .env("FAKE_SCENARIO", "ok")
            .env("FAKE_TASK_ID", &task_id)
            .env("FAKE_CODEX_ARGS_LOG", &log)
            .env("FAKE_PROMPT_COPY", prompt_copy(&f))
            .args(["dispatch", &task_id, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--json"])
            .args(["--skip-spec-review", SKIP]),
    );
    assert_eq!(
        f.wait_for_task_status(&task_id, &["in_review", "failed", "interrupted"], 120),
        "in_review"
    );
    copy_matches(&f, &PathBuf::from(d["run_dir"].as_str().unwrap()));
    let execs: Vec<Vec<String>> =
        launches(&log).into_iter().filter(|a| a.first().map(String::as_str) == Some("exec")).collect();
    assert_eq!(execs.len(), 1, "{execs:?}");
    assert_eq!(execs[0].last().unwrap(), "-");
    for a in &execs[0] {
        assert!(a.len() < 1_000, "an argument of {} chars", a.len());
    }
}

#[test]
fn agy_long_prompt_fails_clearly() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(&long_spec());
    let log = f.home_path().join("fake-agy-args.jsonl");
    let d = json_out(
        f.cmd()
            .env("FAKE_SCENARIO", "ok")
            .env("FAKE_TASK_ID", &task_id)
            .env("FAKE_AGY_DATA_DIR", &cfg)
            .env("FAKE_AGY_ARGS_LOG", &log)
            .env("FAKE_PROMPT_COPY", prompt_copy(&f))
            .args(["dispatch", &task_id, "--profile", "agy-a", "--model", "gemini-3.8-flash-low", "--json"])
            .args(["--skip-spec-review", SKIP]),
    );
    assert_too_long(&f, &task_id, d["run_id"].as_str().unwrap());
    // `agy models` (dispatch's login check) is logged too; no `-p` launch is
    let all = launches(&log);
    assert!(!all.iter().any(|a| a.first().map(String::as_str) == Some("-p")), "{all:?}");
    assert!(!prompt_copy(&f).exists());
}

#[test]
fn an_overlong_argv_fails_the_run_before_spawn() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let log = f.home_path().join("fake-claude-args.jsonl");
    let tools = format!("Bash,{}", "R".repeat(30_995));
    let d = json_out(
        f.cmd()
            .env("FAKE_SCENARIO", "ok")
            .env("FAKE_TASK_ID", &task_id)
            .env("FAKE_CLAUDE_ARGS_LOG", &log)
            .args(["dispatch", &task_id, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium"])
            .args(["--allowed-tools", &tools, "--json"]),
    );
    assert_too_long(&f, &task_id, d["run_id"].as_str().unwrap());
    assert!(!log.exists(), "the worker must never have been launched");
}
