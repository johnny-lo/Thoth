//! SMALL-10 (B): every implementer worker prompt (Claude Code, codex and
//! Antigravity/agy) tells the worker how to wait for a command that is still
//! running: by its process id or the shell's `wait`, never with `pgrep -f` on
//! the command's own text. Observed through the prompt.txt of a run of each
//! fake worker, found with `status TASK --json`. Fake workers only, in a
//! temporary THOTH_HOME and repo.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::{Path, PathBuf};

const REASON: &str = "wait note fixture: fake worker in a temporary repo";
const SPEC: &str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal"}"#;

/// The sentence every implementer prompt carries, exactly.
const WAIT_SENTENCE: &str = "If you ever have to wait for a command that is still running, wait for it by its process id (`wait PID`, or `kill -0 PID` in a loop) or with the shell's `wait`; never look for it with `pgrep -f` on the command's own text, because that also matches the waiting loop itself and the wait never ends.";

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

/// Runs `args` with `envs`, which must exit 0, and returns its stdout as JSON.
fn ok(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Value {
    let mut c = f.cmd();
    for (k, v) in envs {
        c.env(k, v);
    }
    c.args(args);
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn add_profile(f: &Fixture, id: &str, provider: &str, harness: &str) -> PathBuf {
    let cfg = f.home_path().join("profiles").join(id);
    std::fs::create_dir_all(&cfg).unwrap();
    let mut c = f.cmd();
    c.args(["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir"]).arg(&cfg);
    f.run_ok(c, "profile add");
    cfg
}

/// prompt.txt of a run, found through `status TASK --json`'s `runs[].run_dir`.
fn prompt_of(f: &Fixture, task: &str, run: &str) -> String {
    let status = ok(f, &["status", task, "--json"], &[]);
    let row = status["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run)
        .unwrap_or_else(|| panic!("status {task} lists no run {run}"))
        .clone();
    std::fs::read_to_string(Path::new(&s(&row["run_dir"])).join("prompt.txt")).unwrap()
}

#[test]
fn every_implementer_prompt_says_how_to_wait_for_a_running_command() {
    let f = common::fixture();
    common::git(&f.repo_path(), &["config", "core.autocrlf", "false"]);
    f.cmd().arg("init").assert().success();
    add_profile(&f, "claude-b", "anthropic", "claude-code");
    add_profile(&f, "codex-a", "openai", "codex");
    let agy_cfg = add_profile(&f, "agy-a", "google", "antigravity").display().to_string();
    let spec_file = f.home_path().join("spec.json");
    std::fs::write(&spec_file, SPEC).unwrap();
    for (profile, model, extra) in [
        ("claude-b", "sonnet", vec![]),
        ("codex-a", "gpt-5.6-luna", vec![]),
        ("agy-a", "gemini-3.8-flash-low", vec![("FAKE_AGY_DATA_DIR", agy_cfg.as_str())]),
    ] {
        let created = ok(&f, &["task", "create", "--title", "wait note", "-f", spec_file.to_str().unwrap(), "--json"], &[]);
        let task = s(&created["task_id"]);
        let mut envs = vec![("FAKE_SCENARIO", "ok"), ("FAKE_TASK_ID", task.as_str())];
        envs.extend(extra);
        let d = ok(
            &f,
            &["dispatch", &task, "--profile", profile, "--model", model, "--effort", "medium", "--reason", REASON, "--json"],
            &envs,
        );
        let run = s(&d["run_id"]);
        ok(&f, &["wait", &task, "--timeout", "180s", "--json"], &[]);
        let prompt = prompt_of(&f, &task, &run);
        let at = prompt
            .find(WAIT_SENTENCE)
            .unwrap_or_else(|| panic!("{profile}: the prompt lacks the wait sentence\n{prompt}"));
        assert_eq!(prompt.matches(WAIT_SENTENCE).count(), 1, "{profile}: the sentence must appear once\n{prompt}");
        let protected = prompt.find("\n## Protected files\n").unwrap_or_else(|| panic!("{profile}: no protected section\n{prompt}"));
        assert!(at < protected, "{profile}: the sentence comes after the protected files section\n{prompt}");
        // it sits in the single-run note: after the foreground rule, before the finish rule
        let rule = prompt.find("run every command in the foreground.").unwrap();
        let finish = prompt.find(" Finish everything").unwrap();
        assert!(rule < at && at < finish, "{profile}: the sentence is not between the foreground rule and the finish rule\n{prompt}");
    }
}
