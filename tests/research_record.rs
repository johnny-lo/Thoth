//! SMALL-17 (e): a research task's PASS `review record` does its git work
//! (the candidate's diff and commit evidence) before it takes the write lock,
//! and inserts those rows inside its transaction. Through the CLI and the
//! fakes, in a temporary THOTH_HOME.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const SPEC: &str = r#"{"goal":"research retries","task_type":"research","acceptance_criteria":["AC1: report retry behaviour"],"constraints":[],"non_goals":[],"excluded_alternatives":[]}"#;

fn setup() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f
}

fn json_ok(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    serde_json::from_slice(&f.run_ok(c, &args.join(" "))).unwrap()
}

// ---- built as tests/source_check.rs builds them

fn file_url(path: &Path) -> String {
    let s = path.display().to_string().replace('\\', "/");
    let s = s.replace('%', "%25").replace(' ', "%20").replace('#', "%23");
    if s.starts_with('/') {
        format!("file://{s}")
    } else {
        format!("file:///{s}")
    }
}

/// Two source pages under THOTH_HOME and the `sources` that quote them.
fn pages(f: &Fixture) -> Value {
    let paths: Vec<PathBuf> = (1..=2).map(|i| f.home_path().join(format!("page {i}.txt"))).collect();
    for (i, p) in paths.iter().enumerate() {
        std::fs::write(p, format!("prefix\nquote {}\nsuffix", i + 1)).unwrap();
    }
    json!(paths
        .iter()
        .enumerate()
        .map(|(i, p)| json!({"url": file_url(p), "quote": format!("quote {}", i + 1)}))
        .collect::<Vec<_>>())
}

/// The external PASS source check of every source.
fn check_doc(sources: &Value) -> Value {
    json!({"verdict": "PASS", "findings": {"sources": sources.as_array().unwrap().iter().enumerate().map(|(i, s)|
        json!({"id": format!("S{}", i + 1), "url": s["url"], "verdict": "supported", "reason": ""})).collect::<Vec<_>>(),
        "extra_findings": []}})
}

/// A research_ok delivery of fake-claude, the task at in_review; `env` is
/// added to the dispatch. Returns (task, implementer run).
fn delivery(f: &Fixture, sources: &Value, env: &[(&str, &str)]) -> (String, String) {
    let (task, _) = f.create_task(SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "research_ok")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_SOURCES", sources.to_string())
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    for (k, v) in env {
        c.env(k, v);
    }
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch research")).unwrap();
    let run = d["run_id"].as_str().unwrap().to_string();
    let w = json_ok(f, &["wait", "--run", &run, "--timeout", "120s", "--json"]);
    assert_eq!(w["task_status"], "in_review", "{w}");
    (task, run)
}

/// The (kind, run_id) of every diff and commit row of the task.
fn candidate_rows(f: &Fixture, task: &str) -> Vec<(String, String)> {
    json_ok(f, &["evidence", "list", task, "--json"])["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["kind"] == "diff" || e["kind"] == "commit")
        .map(|e| (e["kind"].as_str().unwrap().to_string(), e["run_id"].as_str().unwrap_or("").to_string()))
        .collect()
}

/// A `thoth` process to spawn, with the environment `Fixture::cmd` gives.
fn spawned_thoth(f: &Fixture) -> std::process::Command {
    f.std_cmd_of(env!("CARGO_BIN_EXE_thoth"))
}

/// SMALL-18 T2: a folder holding an executable `git` that appends its
/// arguments as one line to `log`, then runs the real git; and the PATH with
/// that folder first.
#[cfg(unix)]
fn logging_git(f: &Fixture, log: &Path) -> std::ffi::OsString {
    use std::os::unix::fs::PermissionsExt;
    let path = std::env::var_os("PATH").expect("PATH is set");
    let real = std::env::split_paths(&path)
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("git must be on PATH");
    let bin = f.home_path().join("logging-git-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("git");
    std::fs::write(
        &script,
        format!("#!/bin/sh\necho \"$*\" >> '{}'\nexec '{}' \"$@\"\n", log.display(), real.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut dirs = vec![bin];
    dirs.extend(std::env::split_paths(&path));
    std::env::join_paths(dirs).unwrap()
}

fn wait_for_file(path: &Path, secs: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    path.exists()
}

#[test]
fn a_research_pass_record_does_its_git_work_before_the_write_lock() {
    let f = setup();
    let sources = pages(&f);
    // SMALL-18 T2: no candidate evidence yet, so the record must do git work
    let (task, run) = delivery(&f, &sources, &[("THOTH_TEST_SKIP_CANDIDATE_EVIDENCE", "1")]);
    assert_eq!(candidate_rows(&f, &task), Vec::<(String, String)>::new());
    let file = f.home_path().join("check.json");
    std::fs::write(&file, check_doc(&sources).to_string()).unwrap();
    let pause = f.home_path().join("record-pause");
    let reached = PathBuf::from(format!("{}.reached", pause.display()));
    let go = PathBuf::from(format!("{}.go", pause.display()));
    let git_log = f.home_path().join("git-args.log");

    let mut record = spawned_thoth(&f);
    #[cfg(unix)]
    record.env("PATH", logging_git(&f, &git_log));
    let mut record = record
        .env("THOTH_TEST_RECORD_PAUSE", &pause)
        .args(["review", "record", &task, "-f"])
        .arg(&file)
        .arg("--json")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    while !reached.exists() && Instant::now() < deadline && record.try_wait().unwrap().is_none() {
        std::thread::sleep(Duration::from_millis(100));
    }
    if !wait_for_file(&reached, 1) {
        let _ = record.kill();
        let out = record.wait_with_output().unwrap();
        panic!(
            "review record never paused before the write lock; it exited {:?}\nstdout: {}\nstderr: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    // the record's git work (the candidate's diff) is done by the pause
    let git_done = std::fs::read_to_string(&git_log).unwrap_or_default();
    if cfg!(unix) && !git_done.lines().any(|l| l.split_whitespace().any(|w| w == "diff")) {
        std::fs::write(&go, "").unwrap();
        let _ = record.wait();
        panic!("no git diff ran before the write lock; git ran:\n{git_done}");
    }

    // while it is paused, another command takes the write lock at once
    let other = f.home_path().join("other-spec.json");
    std::fs::write(&other, common::Fixture::SPEC).unwrap();
    let started = Instant::now();
    let created = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&other)
        .arg("--json")
        .timeout(Duration::from_secs(10))
        .output()
        .unwrap();
    let took = started.elapsed();
    std::fs::write(&go, "").unwrap();
    assert!(
        created.status.success(),
        "task create during the pause failed after {took:?}: {}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(took < Duration::from_secs(10), "task create took {took:?}");

    let out = record.wait_with_output().unwrap();
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(f.task_status(&task), "pass");
    let mut rows = candidate_rows(&f, &task);
    rows.sort();
    assert_eq!(rows, vec![("commit".to_string(), run.clone()), ("diff".to_string(), run)]);
}

#[test]
fn a_research_pass_record_inserts_missing_candidate_evidence_in_its_transaction() {
    let f = setup();
    let sources = pages(&f);
    let (task, run) = delivery(&f, &sources, &[("THOTH_TEST_SKIP_CANDIDATE_EVIDENCE", "1")]);
    assert_eq!(candidate_rows(&f, &task), Vec::<(String, String)>::new());

    let file = f.home_path().join("check.json");
    std::fs::write(&file, check_doc(&sources).to_string()).unwrap();
    let mut c = f.cmd();
    c.args(["review", "record", &task, "-f"]).arg(&file).arg("--json");
    let r: Value = serde_json::from_slice(&f.run_ok(c, "review record")).unwrap();
    assert_eq!(r["task_status"], "pass", "{r}");
    assert_eq!(f.task_status(&task), "pass");
    let mut rows = candidate_rows(&f, &task);
    rows.sort();
    assert_eq!(rows, vec![("commit".to_string(), run.clone()), ("diff".to_string(), run)]);
}
