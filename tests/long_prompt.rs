//! W3: claude and codex take the prompt on stdin, so a prompt longer than
//! Windows' command-line limit still launches; agy keeps it on argv unless
//! it would push the command line past thoth's cap (SMALL-16: then the prompt
//! goes into a file and the command line names it), and any other command
//! line over the cap fails the run with a clear error before anything is
//! spawned. Always the fakes, in a temporary THOTH_HOME.

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

/// The same, with the 40,000 characters in the one criterion instead: the
/// reviewer's prompt holds the goal and the criteria, not the constraints.
fn long_criterion_spec() -> String {
    serde_json::json!({
        "goal": "say hello",
        "acceptance_criteria": [format!("AC1: prints hello {}", "x".repeat(40_000))],
        "constraints": [],
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

/// SMALL-16: the `-p` value agy got instead of the long prompt.
const FILE_PROMPT_HEAD: &str = "Your full instructions are in the file ";
const FILE_PROMPT_TAIL: &str = ". Read that whole file with your file tool before anything else, then do what it says. It is your complete task.";

/// What the fake agy got as its `-p` value is the one short sentence naming
/// `relative`, and the file there holds the bytes of the run's prompt.txt.
fn file_prompt_matches(f: &common::Fixture, run_dir: &Path, worktree: &Path, relative: &str) {
    let got = std::fs::read_to_string(prompt_copy(f)).expect("the fake wrote the -p value it received");
    assert!(got.len() < 1_000, "the -p value is {} characters", got.len());
    assert_eq!(got, format!("{FILE_PROMPT_HEAD}{relative}{FILE_PROMPT_TAIL}"));
    let sent = std::fs::read(run_dir.join("prompt.txt")).unwrap();
    assert!(sent.len() > 40_000, "prompt.txt is {} bytes", sent.len());
    let file = std::fs::read(worktree.join(relative)).expect("the prompt file in the worktree");
    assert!(file == sent, "the file has {} bytes, prompt.txt {}", file.len(), sent.len());
}

#[test]
fn agy_long_implementer_prompt_goes_through_a_file() {
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
    let run_id = d["run_id"].as_str().unwrap();
    assert_eq!(
        f.wait_for_task_status(&task_id, &["failed", "in_review", "interrupted"], 60),
        "in_review"
    );
    file_prompt_matches(
        &f,
        &PathBuf::from(d["run_dir"].as_str().unwrap()),
        &PathBuf::from(d["worktree"].as_str().unwrap()),
        ".agent/prompt.md",
    );
    // the file is Thoth's own: it is not in the delivered commit
    let candidate = f.run_of(&task_id, run_id)["candidate_commit"].as_str().unwrap().to_string();
    let names = common::git(&f.repo_path(), &["show", "--name-only", "--format=", &candidate]);
    assert!(!names.trim().is_empty(), "the worker committed something");
    for line in names.lines() {
        assert!(!line.starts_with(".agent/"), "{line} is in the candidate commit");
    }
    // one `-p` launch, and it carried the short sentence
    let workers: Vec<Vec<String>> =
        launches(&log).into_iter().filter(|a| a.first().map(String::as_str) == Some("-p")).collect();
    assert_eq!(workers.len(), 1, "{workers:?}");
    for a in &workers[0] {
        assert!(a.len() < 1_000, "an argument of {} chars", a.len());
    }
}

#[test]
fn agy_long_review_prompt_goes_through_a_file() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-a");
    let cfg = f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(&long_criterion_spec());
    json_out(
        f.cmd()
            .env("FAKE_SCENARIO", "ok")
            .env("FAKE_TASK_ID", &task_id)
            .args(["dispatch", &task_id, "--profile", "claude-a", "--model", "sonnet", "--effort", "medium", "--json"])
            .args(["--skip-spec-review", SKIP]),
    );
    assert_eq!(
        f.wait_for_task_status(&task_id, &["in_review", "failed", "interrupted"], 60),
        "in_review"
    );
    let r = json_out(
        f.cmd()
            .env("FAKE_SCENARIO", "review_pass")
            .env("FAKE_AGY_DATA_DIR", &cfg)
            .env("FAKE_PROMPT_COPY", prompt_copy(&f))
            .args(["review", "start", &task_id, "--profile", "agy-a", "--model", "gemini-3.8-flash-low", "--json"]),
    );
    let run_id = r["run_id"].as_str().unwrap();
    f.cmd().args(["wait", "--run", run_id, "--timeout", "120s"]).assert().success();
    assert_eq!(f.run_of(&task_id, run_id)["status"], "completed");
    file_prompt_matches(
        &f,
        &PathBuf::from(r["run_dir"].as_str().unwrap()),
        &PathBuf::from(r["worktree"].as_str().unwrap()),
        ".agent/review-input/prompt.md",
    );
    // the reviewer found the criterion ids in the file and passed the task
    assert_eq!(f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30), "pass");
}

#[test]
fn a_short_agy_prompt_stays_on_the_command_line_and_writes_no_file() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    let d = json_out(
        f.cmd()
            .env("FAKE_SCENARIO", "ok")
            .env("FAKE_TASK_ID", &task_id)
            .env("FAKE_AGY_DATA_DIR", &cfg)
            .env("FAKE_PROMPT_COPY", prompt_copy(&f))
            .args(["dispatch", &task_id, "--profile", "agy-a", "--model", "gemini-3.8-flash-low", "--json"]),
    );
    assert_eq!(f.wait_for_task_status(&task_id, &["failed", "in_review", "interrupted"], 60), "in_review");
    let run_dir = PathBuf::from(d["run_dir"].as_str().unwrap());
    let got = std::fs::read(prompt_copy(&f)).unwrap();
    assert!(got == std::fs::read(run_dir.join("prompt.txt")).unwrap(), "the -p value is the whole prompt");
    assert!(!PathBuf::from(d["worktree"].as_str().unwrap()).join(".agent").join("prompt.md").exists());
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

/// `long_spec`'s shape with the 40,000 characters in the goal: a spec
/// reviewer's prompt carries the goal and the criterion ids, and reads the
/// constraints from the spec file, so only a long goal makes it long.
fn long_goal_spec() -> String {
    serde_json::json!({
        "goal": format!("say hello {}", "x".repeat(40_000)),
        "acceptance_criteria": ["AC1: prints hello"],
        "constraints": [],
        "non_goals": [],
        "excluded_alternatives": [],
        "risk": "important",
    })
    .to_string()
}

/// SMALL-17 (f): an agy spec reviewer's long prompt goes through the same
/// file as a code reviewer's, `.agent/review-input/prompt.md`, and the fake
/// spec reviewer follows it there (the criterion ids are in the file only).
#[test]
fn agy_long_spec_review_prompt_goes_through_a_file() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(&long_goal_spec());
    let r = json_out(
        f.cmd()
            .env("FAKE_SCENARIO", "spec_review_approve")
            .env("FAKE_TASK_ID", &task_id)
            .env("FAKE_AGY_DATA_DIR", &cfg)
            .env("FAKE_PROMPT_COPY", prompt_copy(&f))
            .args(["spec", "review", "start", &task_id, "--profile", "agy-a", "--model", "gemini-3.8-flash-high", "--json"]),
    );
    let run_id = r["run_id"].as_str().unwrap();
    f.cmd().args(["wait", "--run", run_id, "--timeout", "120s"]).assert().success();
    let show = json_out(f.cmd().args(["spec", "review", "show", &task_id, "--json"]));
    assert_eq!(show["approved"], true, "{show}");
    file_prompt_matches(
        &f,
        &PathBuf::from(r["run_dir"].as_str().unwrap()),
        &PathBuf::from(r["worktree"].as_str().unwrap()),
        ".agent/review-input/prompt.md",
    );
}
