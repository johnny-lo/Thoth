//! SMALL-11 (5): a finished task's worktree is not removed while a `thoth
//! verify` is running its checks in it, and that verify removes it when it
//! ends. A verify that was killed holds nothing.
//!
//! The approved check is a script committed to the fixture repo that tells
//! the test when it started and then waits for a release file, so the order
//! of events is driven by the test instead of raced.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[cfg(windows)]
const SHELL: &str = "cmd";
#[cfg(not(windows))]
const SHELL: &str = "sh";

const REVIEW_PASS: &str = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":["E001"]},{"id":"AC2","verdict":"met","evidence":["E001"]}],"extra_findings":[]}}"#;

fn started(f: &Fixture) -> PathBuf {
    f.home_path().join("verify-started")
}

fn release(f: &Fixture) -> PathBuf {
    f.home_path().join("verify-release")
}

/// Commits `waitcheck`: it creates the started file, waits for the release
/// file (at most a minute), then reads a tracked file.
fn commit_script(f: &Fixture) -> String {
    let repo = f.repo_path();
    let (started, release) = (started(f), release(f));
    let (name, body) = if cfg!(windows) {
        (
            "waitcheck.cmd",
            format!(
                "@echo off\r\ntype nul > \"{}\"\r\nset /a n=0\r\n:wait\r\nif exist \"{}\" goto done\r\nset /a n+=1\r\nif %n% GEQ 60 goto done\r\nping -n 2 127.0.0.1 >nul\r\ngoto wait\r\n:done\r\ntype a.txt\r\n",
                started.display(),
                release.display()
            ),
        )
    } else {
        (
            "waitcheck.sh",
            format!(
                "#!/bin/sh\ntouch '{}'\nn=0\nwhile [ ! -f '{}' ] && [ $n -lt 300 ]; do sleep 0.2; n=$((n+1)); done\ncat a.txt\n",
                started.display(),
                release.display()
            ),
        )
    };
    std::fs::write(repo.join(name), body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(repo.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    common::git(&repo, &["add", name]);
    common::git(&repo, &["commit", "-m", "wait check"]);
    if cfg!(windows) { format!(r".\{name}") } else { format!("./{name}") }
}

struct Setup {
    task_id: String,
    run_id: String,
    worktree: PathBuf,
}

/// init, the committed script as an approved check, and a `hooks`
/// implementer run waiting in review.
fn setup(f: &Fixture) -> Setup {
    let cmd = commit_script(f);
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review", "{}", f.explain_task(&task_id));
    f.cmd()
        .args(["checks", "add", "wait", "--cmd", &cmd, "--shell", SHELL, "--json"])
        .assert()
        .success();
    f.cmd()
        .args(["checks", "approve", "--reason", "test approval", "--json"])
        .assert()
        .success();
    Setup {
        task_id,
        run_id: d["run_id"].as_str().unwrap().to_string(),
        worktree: PathBuf::from(d["worktree"].as_str().unwrap()),
    }
}

/// `thoth verify RUN --json` as a child process of the test, with the same
/// environment `f.cmd()` gives. Killed when dropped, so a failing test leaves
/// nothing running.
struct Verify(Option<Child>);

impl Drop for Verify {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn start_verify(f: &Fixture, run_id: &str) -> Verify {
    let mut c = common::scrubbed(Command::new(env!("CARGO_BIN_EXE_thoth")));
    for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE", "CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        c.env_remove(var);
    }
    c.env_remove("ARTIFICIALANALYSIS_API_KEY")
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env("THOTH_HOME", f.home_path())
        .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
        .current_dir(f.repo_path())
        .args(["verify", run_id, "--json", "--timeout", "120s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = c.spawn().unwrap();
    let verify = Verify(Some(child));
    wait_for_file(&started(f), 60);
    verify
}

fn wait_for_file(path: &Path, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_until_gone(path: &Path, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while path.exists() {
        assert!(Instant::now() < deadline, "{} still exists", path.display());
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Lets the check go on, waits for verify to exit, and returns its exit code
/// and `--json` document.
fn finish_verify(f: &Fixture, mut verify: Verify) -> (i32, Value) {
    std::fs::write(release(f), "go").unwrap();
    let out = verify.0.take().unwrap().wait_with_output().unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("verify printed no JSON ({e}): {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code().unwrap(), doc)
}

fn record_pass(f: &Fixture, task_id: &str) {
    let file = f.home_path().join("review-pass.json");
    std::fs::write(&file, REVIEW_PASS).unwrap();
    let mut c = f.cmd();
    c.args(["review", "record", task_id, "-f"]).arg(&file).arg("--json");
    let out = f.run_ok(c, "review record");
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["task_status"], "pass", "{v}");
}

fn events(f: &Fixture, task_id: &str, ty: &str) -> Vec<Value> {
    f.events_json(task_id)
        .into_iter()
        .filter(|e| e["type"] == ty)
        .map(|e| e["detail"].clone())
        .collect()
}

fn kept_for_verify(f: &Fixture, task_id: &str, worktree: &Path) -> Vec<Value> {
    events(f, task_id, "worktree_kept")
        .into_iter()
        .filter(|d| d["reason"] == "verify_running" && d["path"] == worktree.display().to_string())
        .collect()
}

#[test]
fn a_pass_recorded_while_verify_runs_keeps_the_worktree_and_verify_removes_it_when_it_ends() {
    let f = common::fixture();
    let s = setup(&f);
    let verify = start_verify(&f, &s.run_id);

    record_pass(&f, &s.task_id);
    assert!(s.worktree.is_dir(), "the PASS removed the worktree under the running verify");
    let kept = kept_for_verify(&f, &s.task_id, &s.worktree);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["source"], "review_record");
    // a prune keeps it too, lists it, and writes nothing more (deduplicated)
    let p = f.cmd().args(["worktrees", "prune", "--json"]).output().unwrap();
    let p: Value = serde_json::from_slice(&p.stdout).unwrap();
    let listed: Vec<&Value> = p["kept"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|k| k["reason"] == "verify_running")
        .collect();
    assert_eq!(listed.len(), 1, "{p}");
    assert_eq!(listed[0]["path"], s.worktree.display().to_string().as_str(), "{p}");
    assert!(s.worktree.is_dir());
    assert_eq!(kept_for_verify(&f, &s.task_id, &s.worktree).len(), 1);

    let (code, doc) = finish_verify(&f, verify);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["failures"], 0, "{doc}");
    assert!(!s.worktree.exists(), "verify left the finished task's worktree behind");
    let removed: Vec<Value> = events(&f, &s.task_id, "worktree_removed")
        .into_iter()
        .filter(|d| d["path"] == s.worktree.display().to_string())
        .collect();
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["source"], "verify");
}

#[test]
fn the_runners_sweep_after_a_reviewer_pass_keeps_it_too() {
    let f = common::fixture();
    let s = setup(&f);
    let verify = start_verify(&f, &s.run_id);

    let gate = f.home_path().join("review-gate");
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &s.task_id)
        .env("FAKE_REVIEW_GATE", &gate)
        .args(["review", "start", &s.task_id, "--profile", "claude-a", "--model", "opus", "--effort", "medium", "--json"]);
    let r: Value = serde_json::from_slice(&f.run_ok(c, "review start")).unwrap();
    std::fs::write(&gate, "go").unwrap();
    f.cmd()
        .args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();
    assert_eq!(f.task_status(&s.task_id), "pass");
    // the runner sweeps just after it settles the task
    let deadline = Instant::now() + Duration::from_secs(30);
    while kept_for_verify(&f, &s.task_id, &s.worktree).is_empty() {
        assert!(Instant::now() < deadline, "no worktree_kept event: {:?}", f.events_of(&s.task_id));
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(s.worktree.is_dir());
    assert_eq!(kept_for_verify(&f, &s.task_id, &s.worktree)[0]["source"], "runner");

    let (code, doc) = finish_verify(&f, verify);
    assert_eq!((code, &doc["failures"]), (0, &Value::from(0)), "{doc}");
    wait_until_gone(&s.worktree, 30);
    let removed = events(&f, &s.task_id, "worktree_removed");
    assert!(removed.iter().any(|d| d["source"] == "verify" && d["path"] == s.worktree.display().to_string()), "{removed:?}");
}

#[test]
fn a_verify_that_was_killed_holds_nothing_and_the_next_sweep_removes_the_worktree() {
    let f = common::fixture();
    let s = setup(&f);
    let mut verify = start_verify(&f, &s.run_id);
    record_pass(&f, &s.task_id);
    assert!(s.worktree.is_dir());

    {
        let child = verify.0.as_mut().unwrap();
        child.kill().unwrap();
        child.wait().unwrap();
    }
    // WINFIX-2: the check script the killed verify started is not ended with
    // it and runs on with the worktree as its working directory. Windows does
    // not delete such a folder, and a `git worktree remove --force` that fails
    // on it has already emptied it and dropped it from git's worktree list, so
    // no later sweep could remove it (`git_error`). On Windows the script is
    // let end before the sweep; elsewhere the sweep runs while it still waits.
    #[cfg(windows)]
    {
        std::fs::write(release(&f), "go").unwrap();
        wait_until_no_process_works_in(&s.worktree, 60);
    }
    let path = s.worktree.display().to_string();
    let p = prune(&f);
    assert!(!kept_for(&p, &path, "verify_running"), "the killed verify still holds the worktree: {p}");
    assert!(removed_by(&p, &path), "{p}");
    assert!(!s.worktree.exists());
    // let the orphaned check script end
    std::fs::write(release(&f), "go").unwrap();
}

/// Windows refuses to rename a folder that is a running process's working
/// directory (the same reason it refuses to delete it), so `dir` is renamed and
/// at once renamed back until a rename succeeds: then the check script and its
/// `ping` have ended. At most `secs` seconds.
#[cfg(windows)]
fn wait_until_no_process_works_in(dir: &Path, secs: u64) {
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    let aside = dir.with_file_name(format!("{name}.probe"));
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        match std::fs::rename(dir, &aside) {
            Ok(()) => {
                std::fs::rename(&aside, dir)
                    .unwrap_or_else(|e| panic!("could not rename {} back to {}: {e}", aside.display(), dir.display()));
                return;
            }
            Err(e) => assert!(
                Instant::now() < deadline,
                "a process still works in {} after {secs} s: {e}",
                dir.display()
            ),
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// `thoth worktrees prune --json`'s answer.
fn prune(f: &Fixture) -> Value {
    let p = f.cmd().args(["worktrees", "prune", "--json"]).output().unwrap();
    serde_json::from_slice(&p.stdout)
        .unwrap_or_else(|e| panic!("prune printed no JSON ({e}): {}", String::from_utf8_lossy(&p.stderr)))
}

/// Whether a prune answer lists `path` in `removed`.
fn removed_by(p: &Value, path: &str) -> bool {
    p["removed"].as_array().unwrap().iter().any(|r| r["path"] == path)
}

/// Whether a prune answer lists `path` in `kept` with `reason`.
fn kept_for(p: &Value, path: &str, reason: &str) -> bool {
    p["kept"].as_array().unwrap().iter().any(|k| k["path"] == path && k["reason"] == reason)
}

/// `thoth verify RUN --json` as a child of the test, like `start_verify`, but
/// with extra environment and without waiting for the check's started file.
fn spawn_verify(f: &Fixture, run_id: &str, env: &[(&str, &Path)]) -> Verify {
    let mut c = common::scrubbed(Command::new(env!("CARGO_BIN_EXE_thoth")));
    for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE", "CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        c.env_remove(var);
    }
    c.env_remove("ARTIFICIALANALYSIS_API_KEY")
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env("THOTH_HOME", f.home_path())
        .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
        .current_dir(f.repo_path())
        .args(["verify", run_id, "--json", "--timeout", "120s"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in env {
        c.env(k, v);
    }
    Verify(Some(c.spawn().unwrap()))
}

// SMALL-12 (1): the hold is locked before it has its final name, so a sweep
// that runs while a verify is taking its hold never finds an unlocked one.
#[test]
fn a_sweep_while_a_verify_takes_its_hold_never_removes_that_verifys_worktree() {
    let f = common::fixture();
    let s = setup(&f);

    // A second finished task whose worktree is kept (uncommitted changes), so
    // every later sweep reaches the hold check.
    let (y_task, _) = f.create_task(Fixture::SPEC_TWO_AC);
    let y = f.dispatch(&y_task, "claude-b", "hooks", &[]);
    assert_eq!(f.wait_for(&y_task, "180s").1, "in_review", "{}", f.explain_task(&y_task));
    let y_worktree = PathBuf::from(y["worktree"].as_str().unwrap());
    std::fs::write(y_worktree.join("a.txt"), "changed and left uncommitted\n").unwrap();
    record_pass(&f, &y_task);
    assert!(y_worktree.is_dir(), "Y's worktree with uncommitted changes was removed");

    // The verify pauses right after it made its hold file.
    let pause = f.home_path().join("verify-hold-pause");
    let verify = spawn_verify(&f, &s.run_id, &[("THOTH_TEST_VERIFY_HOLD_PAUSE_FILE", &pause)]);
    wait_for_file(&pause, 60);

    let p = f.cmd().args(["worktrees", "prune", "--json"]).output().unwrap();
    assert_eq!(p.status.code(), Some(0), "{}", String::from_utf8_lossy(&p.stderr));

    std::fs::remove_file(&pause).unwrap();
    wait_for_file(&started(&f), 60);
    record_pass(&f, &s.task_id);

    assert!(s.worktree.is_dir(), "a sweep removed the verify's hold and the PASS removed the worktree under the checks");
    let kept = kept_for_verify(&f, &s.task_id, &s.worktree);
    assert_eq!(kept.len(), 1, "{kept:?}");

    let (code, doc) = finish_verify(&f, verify);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["failures"], 0, "{doc}");
    assert!(!s.worktree.exists(), "verify left the finished task's worktree behind");
    let removed: Vec<Value> = events(&f, &s.task_id, "worktree_removed")
        .into_iter()
        .filter(|d| d["path"] == s.worktree.display().to_string())
        .collect();
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["source"], "verify");
}
