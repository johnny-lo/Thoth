//! SWEEP-1: the temporary checkout of `thoth verify RUN --at-base` carries
//! an owner record, so the checkout of a killed one is removed by the next
//! `verify RUN --at-base` (and by `suite run` and `worktrees prune`), while
//! one still running keeps its own. A plain `verify` and a `--dry-run` do not
//! sweep. The run is a settled `hooks` run from `dispatch` with fake-claude;
//! the slow check is a committed script driven by started, release and done
//! files, the fast one is fake-check.

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

const REASON: &str = "SWEEP-1 fixture: a waiting script and fake-check in a temporary repo";
const FAKE_CHECK: &str = env!("CARGO_BIN_EXE_fake-check");

fn started(f: &Fixture) -> PathBuf {
    f.home_path().join("slow-started")
}

fn release(f: &Fixture) -> PathBuf {
    f.home_path().join("slow-release")
}

fn done(f: &Fixture) -> PathBuf {
    f.home_path().join("slow-done")
}

fn baseline_dir(f: &Fixture) -> PathBuf {
    f.home_path().join("repos").join(f.repo_id()).join("baseline-worktrees")
}

fn holds_dir(f: &Fixture) -> PathBuf {
    f.home_path().join("repos").join(f.repo_id()).join("baseline-holds")
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

fn registered(f: &Fixture, name: &str) -> bool {
    common::git(&f.repo_path(), &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .any(|p| p.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next() == Some(name))
}

fn commit_slow_script(f: &Fixture) -> String {
    let repo = f.repo_path();
    let (started, release, done) = (started(f), release(f), done(f));
    let (name, body) = if cfg!(windows) {
        (
            "slowcheck.cmd",
            format!(
                "@echo off\r\ntype nul > \"{}\"\r\nset /a n=0\r\n:wait\r\nif exist \"{}\" goto done\r\nset /a n+=1\r\nif %n% GEQ 60 goto done\r\nping -n 2 127.0.0.1 >nul\r\ngoto wait\r\n:done\r\ntype nul > \"{}\"\r\ntype a.txt\r\n",
                started.display(),
                release.display(),
                done.display()
            ),
        )
    } else {
        (
            "slowcheck.sh",
            format!(
                "#!/bin/sh\ntouch '{}'\nn=0\nwhile [ ! -f '{}' ] && [ $n -lt 300 ]; do sleep 0.2; n=$((n+1)); done\ntouch '{}'\ncat a.txt\n",
                started.display(),
                release.display(),
                done.display()
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
    common::git(&repo, &["commit", "-m", "slow check"]);
    if cfg!(windows) { format!(r".\{name}") } else { format!("./{name}") }
}

/// The slow script committed, init, a task created after it and a settled
/// `hooks` run, then the slow and fast checks approved. The run's id.
fn setup(f: &Fixture) -> String {
    let slow = commit_slow_script(f);
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review", "{}", f.explain_task(&task_id));
    f.cmd()
        .args(["checks", "add", "slow", "--cmd", &slow, "--shell", SHELL, "--json"])
        .assert()
        .success();
    let fast = format!("\"{FAKE_CHECK}\" args");
    f.cmd()
        .args(["checks", "add", "fast", "--cmd", &fast, "--shell", "argv", "--json"])
        .assert()
        .success();
    f.cmd().args(["checks", "approve", "--reason", REASON, "--json"]).assert().success();
    d["run_id"].as_str().unwrap().to_string()
}

struct Out {
    code: i32,
    doc: Value,
    stderr: String,
}

fn json_run(f: &Fixture, args: &[&str]) -> Out {
    let out = f.cmd().args(args).output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let doc = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("{args:?} printed no JSON ({e}): {stdout}\n{stderr}"));
    Out { code: out.status.code().unwrap_or(-1), doc, stderr }
}

struct Thoth(Option<Child>);

impl Drop for Thoth {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn start_slow(f: &Fixture, args: &[&str]) -> Thoth {
    let _ = std::fs::remove_file(started(f));
    let _ = std::fs::remove_file(release(f));
    let _ = std::fs::remove_file(done(f));
    let mut c = common::scrubbed(Command::new(env!("CARGO_BIN_EXE_thoth")));
    for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE", "CLAUDE_CONFIG_DIR", "CODEX_HOME"] {
        c.env_remove(var);
    }
    c.env_remove("ARTIFICIALANALYSIS_API_KEY")
        .env_remove("AISTUPIDLEVEL_API_KEY")
        .env("THOTH_HOME", f.home_path())
        .env("THOTH_ARENA_URL", common::offline_arena_url(&f.home_path()))
        .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
        .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
        .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
        .current_dir(f.repo_path())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let t = Thoth(Some(c.spawn().unwrap()));
    wait_for_file(&started(f), 60);
    t
}

fn wait_for_file(path: &Path, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn wait_a_while_for(path: &Path, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn kill_and_release(f: &Fixture, mut t: Thoth) {
    let mut child = t.0.take().unwrap();
    child.kill().unwrap();
    let _ = child.wait();
    std::fs::write(release(f), "go").unwrap();
    wait_a_while_for(&done(f), 30);
}

fn finish(f: &Fixture, mut t: Thoth) -> (i32, Value) {
    std::fs::write(release(f), "go").unwrap();
    let out = t.0.take().unwrap().wait_with_output().unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("printed no JSON ({e}): {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code().unwrap_or(-1), doc)
}

fn at_base_args(run_id: &str) -> Vec<&str> {
    vec!["verify", run_id, "--at-base", "--json", "--timeout", "120s"]
}

/// The one entry of the folder whose name starts with the run id.
fn only_run_entry(f: &Fixture, run_id: &str) -> String {
    let prefix = format!("{run_id}-");
    let names: Vec<String> = names_in(&baseline_dir(f)).into_iter().filter(|n| n.starts_with(&prefix)).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    names[0].clone()
}

/// A `verify RUN --at-base` killed in its slow check: the leftover's name.
fn leave_leftover(f: &Fixture, run_id: &str) -> String {
    let t = start_slow(f, &at_base_args(run_id));
    let name = only_run_entry(f, run_id);
    kill_and_release(f, t);
    assert!(baseline_dir(f).join(&name).is_dir(), "the killed --at-base checkout is gone");
    assert!(registered(f, &name), "the killed --at-base checkout is not registered");
    name
}

fn path_of(f: &Fixture, name: &str) -> String {
    baseline_dir(f).join(name).display().to_string()
}

fn list<'a>(doc: &'a Value, key: &str) -> &'a Vec<Value> {
    doc["baseline_sweep"][key].as_array().unwrap_or_else(|| panic!("no baseline_sweep.{key} in {doc}"))
}

/// AC7 (f): the next `verify RUN --at-base` removes the checkout a killed
/// one left, and its own when it ends.
#[test]
fn the_next_at_base_removes_the_checkout_a_killed_at_base_left() {
    let f = common::fixture();
    let run_id = setup(&f);
    let name = leave_leftover(&f, &run_id);
    let path = path_of(&f, &name);

    let v = json_run(&f, &at_base_args(&run_id));
    let removed = list(&v.doc, "removed");
    assert_eq!(removed.len(), 1, "{}", v.doc);
    assert_eq!(removed[0]["path"], path.as_str(), "{}", v.doc);
    assert_eq!(removed[0]["made_by"], "verify_at_base", "{}", v.doc);
    assert_eq!(removed[0]["run_id"], run_id.as_str(), "{}", v.doc);
    assert_eq!(list(&v.doc, "kept").len(), 0, "{}", v.doc);
    assert_eq!(v.doc["baseline_sweep"]["error"], Value::Null, "{}", v.doc);
    assert_eq!(v.code, if v.doc["ok"] == true { 0 } else { 1 }, "{}", v.doc);
    assert!(
        v.stderr.contains(&format!("thoth: removed the leftover checkout {path} (its command is no longer running)")),
        "{}",
        v.stderr
    );
    assert!(!Path::new(&path).exists());
    assert!(!registered(&f, &name));
    assert_eq!(names_in(&baseline_dir(&f)), Vec::<String>::new(), "its own checkout stayed");
    assert_eq!(names_in(&holds_dir(&f)), Vec::<String>::new());
}

/// AC7 (g): a running `verify --at-base`'s checkout is kept as `in_use` by
/// `suite run` and `worktrees prune`, and goes when it ends.
#[test]
fn a_running_at_bases_checkout_is_kept_in_use_until_it_ends() {
    let f = common::fixture();
    let run_id = setup(&f);
    let a = start_slow(&f, &at_base_args(&run_id));
    let name = only_run_entry(&f, &run_id);
    let path = path_of(&f, &name);

    let s = json_run(&f, &["suite", "run", "--check", "fast", "--json"]);
    assert_eq!(s.code, 0, "{}\n{}", s.doc, s.stderr);
    let kept = list(&s.doc, "kept");
    assert_eq!(kept.len(), 1, "{}", s.doc);
    assert_eq!(kept[0]["path"], path.as_str(), "{}", s.doc);
    assert_eq!(kept[0]["reason"], "in_use", "{}", s.doc);
    assert!(!s.stderr.contains(&path), "{}", s.stderr);

    let p = json_run(&f, &["worktrees", "prune", "--json"]);
    assert_eq!((p.code, &p.doc["ok"]), (0, &Value::Bool(true)), "{}", p.doc);
    let kept = list(&p.doc, "kept");
    assert_eq!(kept.len(), 1, "{}", p.doc);
    assert_eq!(kept[0]["path"], path.as_str(), "{}", p.doc);
    assert_eq!(kept[0]["reason"], "in_use", "{}", p.doc);
    assert!(Path::new(&path).is_dir());
    assert!(registered(&f, &name));

    let (code, doc) = finish(&f, a);
    assert_eq!(code, if doc["ok"] == true { 0 } else { 1 }, "{doc}");
    assert!(doc["baseline_sweep"].is_object(), "{doc}");
    assert!(!Path::new(&path).exists(), "verify --at-base left its own checkout");
    assert!(!registered(&f, &name));
    assert_eq!(names_in(&holds_dir(&f)), Vec::<String>::new());
}

/// AC7 (h): a plain `verify` and an `--at-base --dry-run` neither sweep nor
/// carry the key.
#[test]
fn plain_verify_and_a_dry_run_do_not_sweep() {
    let f = common::fixture();
    let run_id = setup(&f);
    let name = leave_leftover(&f, &run_id);

    let plain = json_run(&f, &["verify", &run_id, "--json", "--timeout", "120s"]);
    assert!(plain.doc.get("baseline_sweep").is_none(), "{}", plain.doc);
    assert!(plain.doc["results"].is_array(), "{}", plain.doc);
    assert!(baseline_dir(&f).join(&name).is_dir(), "a plain verify removed the leftover");

    let dry = json_run(&f, &["verify", &run_id, "--at-base", "--dry-run", "--json"]);
    assert_eq!(dry.doc["dry_run"], true, "{}", dry.doc);
    assert!(dry.doc.get("baseline_sweep").is_none(), "{}", dry.doc);
    assert!(baseline_dir(&f).join(&name).is_dir(), "a dry run removed the leftover");
    assert!(registered(&f, &name));
}
