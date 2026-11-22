//! SWEEP-1: the temporary checkout a `thoth suite run` makes under
//! `THOTH_HOME/repos/REPO/baseline-worktrees` carries an owner record (a
//! locked file in `baseline-holds`), so a run that was killed leaves a
//! checkout the next `suite run` or `thoth worktrees prune` removes, and a
//! run still going keeps its own. Anything in that folder Thoth did not make
//! is never touched.
//!
//! The slow check is a script committed to the fixture repo that touches a
//! started file, waits for a release file, then touches a done file, so the
//! order of events is driven by the test instead of raced. The fast check is
//! fake-check, run with `--check fast`. Everything goes through the CLI.

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
/// A fixed ULID for a hand-made checkout that has Thoth's name shape.
const FIXED_ULID: &str = "01M823VE9DYK1HR930H9JRSSFK";

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

/// The names in a folder, sorted; none when it does not exist.
fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

/// Whether `git worktree list --porcelain` names a worktree whose last path
/// component is `name` (the names carry a fresh ULID, so this is exact).
fn registered(f: &Fixture, name: &str) -> bool {
    common::git(&f.repo_path(), &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .any(|p| p.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next() == Some(name))
}

/// Commits `slowcheck`: touches the started file, waits for the release file
/// (at most a minute), touches the done file, then reads a tracked file.
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

/// init, the slow script as check `slow` and fake-check as check `fast`,
/// both approved.
fn setup() -> Fixture {
    let f = common::fixture();
    let slow = commit_slow_script(&f);
    f.cmd().arg("init").assert().success();
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
    f
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

fn fast_suite(f: &Fixture) -> Out {
    json_run(f, &["suite", "run", "--check", "fast", "--json"])
}

fn prune(f: &Fixture) -> Out {
    json_run(f, &["worktrees", "prune", "--json"])
}

/// A thoth child process with the environment `f.cmd()` gives; killed when
/// dropped, so a failing test leaves nothing running.
struct Thoth(Option<Child>);

impl Drop for Thoth {
    fn drop(&mut self) {
        if let Some(c) = self.0.as_mut() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

/// Starts `thoth ARGS` and waits until the slow check has started.
fn start_slow(f: &Fixture, args: &[&str]) -> Thoth {
    let _ = std::fs::remove_file(started(f));
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

/// Waits for `path` for at most `secs`, without failing when it never comes.
fn wait_a_while_for(path: &Path, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while !path.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Kills the child process alone (its check may live on), then lets the
/// check go on and waits for it to finish or 30 s.
fn kill_and_release(f: &Fixture, mut t: Thoth) {
    let mut child = t.0.take().unwrap();
    child.kill().unwrap();
    let _ = child.wait();
    std::fs::write(release(f), "go").unwrap();
    wait_a_while_for(&done(f), 30);
}

/// Releases the check and waits for the child; its exit code and document.
fn finish(f: &Fixture, mut t: Thoth) -> (i32, Value) {
    std::fs::write(release(f), "go").unwrap();
    let out = t.0.take().unwrap().wait_with_output().unwrap();
    let doc: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("printed no JSON ({e}): {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code().unwrap_or(-1), doc)
}

/// The one `suite-...` entry of the folder.
fn only_suite_entry(f: &Fixture) -> String {
    let names: Vec<String> = names_in(&baseline_dir(f)).into_iter().filter(|n| n.starts_with("suite-")).collect();
    assert_eq!(names.len(), 1, "{names:?}");
    names[0].clone()
}

/// A `suite run --check slow` killed in its check: the leftover's name.
fn leave_leftover(f: &Fixture) -> String {
    let t = start_slow(f, &["suite", "run", "--check", "slow", "--json"]);
    let name = only_suite_entry(f);
    kill_and_release(f, t);
    assert!(baseline_dir(f).join(&name).is_dir(), "the killed suite run's checkout is gone");
    assert!(registered(f, &name), "the killed suite run's checkout is not registered");
    name
}

fn path_of(f: &Fixture, name: &str) -> String {
    baseline_dir(f).join(name).display().to_string()
}

fn sweep_of(doc: &Value) -> &Value {
    let s = &doc["baseline_sweep"];
    assert!(s.is_object(), "no baseline_sweep in {doc}");
    s
}

fn list<'a>(doc: &'a Value, key: &str) -> &'a Vec<Value> {
    sweep_of(doc)[key].as_array().unwrap_or_else(|| panic!("no baseline_sweep.{key} in {doc}"))
}

fn removed_line(path: &str) -> String {
    format!("thoth: removed the leftover checkout {path} (its command is no longer running)")
}

/// AC6 (a): a killed `suite run` leaves its checkout; the next `suite run`
/// removes it with its registration, says so on stderr, and removes its own.
#[test]
fn the_next_suite_run_removes_the_checkout_a_killed_suite_run_left() {
    let f = setup();
    let name = leave_leftover(&f);
    let path = path_of(&f, &name);

    let b = fast_suite(&f);
    assert_eq!(b.code, 0, "{}\n{}", b.doc, b.stderr);
    assert_eq!(b.doc["ok"], true, "{}", b.doc);
    let removed = list(&b.doc, "removed");
    assert_eq!(removed.len(), 1, "{}", b.doc);
    assert_eq!(removed[0]["path"], path.as_str(), "{}", b.doc);
    assert_eq!(removed[0]["made_by"], "suite_run", "{}", b.doc);
    assert_eq!(removed[0]["run_id"], Value::Null, "{}", b.doc);
    assert_eq!(list(&b.doc, "kept").len(), 0, "{}", b.doc);
    assert_eq!(sweep_of(&b.doc)["error"], Value::Null, "{}", b.doc);
    assert!(b.stderr.contains(&removed_line(&path)), "{}", b.stderr);

    assert!(!Path::new(&path).exists(), "the leftover is still there");
    assert!(!registered(&f, &name), "the leftover is still registered");
    // its own checkout and every hold file are gone too
    assert_eq!(names_in(&baseline_dir(&f)), Vec::<String>::new());
    assert_eq!(names_in(&holds_dir(&f)), Vec::<String>::new());
}

/// AC6 (b): `worktrees prune` removes the same kind of leftover, and a
/// second prune lists nothing.
#[test]
fn worktrees_prune_removes_a_killed_suite_runs_checkout() {
    let f = setup();
    let name = leave_leftover(&f);
    let path = path_of(&f, &name);

    let p = prune(&f);
    assert_eq!(p.code, 0, "{}\n{}", p.doc, p.stderr);
    assert_eq!(p.doc["ok"], true, "{}", p.doc);
    let removed = list(&p.doc, "removed");
    assert_eq!(removed.len(), 1, "{}", p.doc);
    assert_eq!(removed[0]["path"], path.as_str(), "{}", p.doc);
    assert_eq!(removed[0]["made_by"], "suite_run", "{}", p.doc);
    assert!(!Path::new(&path).exists());
    assert!(!registered(&f, &name));
    assert_eq!(names_in(&holds_dir(&f)), Vec::<String>::new());

    let again = prune(&f);
    assert_eq!(again.code, 0, "{}", again.doc);
    assert_eq!(list(&again.doc, "removed").len(), 0, "{}", again.doc);
    assert_eq!(list(&again.doc, "kept").len(), 0, "{}", again.doc);
}

/// AC6 (c): a checkout whose `suite run` is still running is kept as
/// `in_use` by another `suite run` and by `worktrees prune`, and its own run
/// removes it with its hold when it ends.
#[test]
fn a_running_suite_runs_checkout_is_kept_in_use_until_it_ends() {
    let f = setup();
    let a = start_slow(&f, &["suite", "run", "--check", "slow", "--json"]);
    let name = only_suite_entry(&f);
    let path = path_of(&f, &name);
    assert_eq!(names_in(&holds_dir(&f)), vec![format!("{name}.hold")]);

    let b = fast_suite(&f);
    assert_eq!(b.code, 0, "{}\n{}", b.doc, b.stderr);
    let kept = list(&b.doc, "kept");
    assert_eq!(kept.len(), 1, "{}", b.doc);
    assert_eq!(kept[0]["path"], path.as_str(), "{}", b.doc);
    assert_eq!(kept[0]["reason"], "in_use", "{}", b.doc);
    assert_eq!(kept[0]["detail"], Value::Null, "{}", b.doc);
    assert_eq!(list(&b.doc, "removed").len(), 0, "{}", b.doc);
    assert!(!b.stderr.contains(&path), "{}", b.stderr);

    let p = prune(&f);
    assert_eq!((p.code, &p.doc["ok"]), (0, &Value::Bool(true)), "{}", p.doc);
    let kept = list(&p.doc, "kept");
    assert_eq!(kept.len(), 1, "{}", p.doc);
    assert_eq!(kept[0]["path"], path.as_str(), "{}", p.doc);
    assert_eq!(kept[0]["reason"], "in_use", "{}", p.doc);
    assert!(Path::new(&path).is_dir(), "a sweep removed a checkout in use");
    assert!(registered(&f, &name));

    let (code, doc) = finish(&f, a);
    assert_eq!(code, 0, "{doc}");
    assert_eq!(doc["ok"], true, "{doc}");
    assert!(!Path::new(&path).exists(), "suite run left its own checkout");
    assert!(!registered(&f, &name));
    assert_eq!(names_in(&holds_dir(&f)), Vec::<String>::new());
}

/// AC6 (d): a directory and a file Thoth did not make, and a Thoth-shaped
/// checkout with no owner record, are kept and never touched.
#[test]
fn entries_thoth_did_not_make_and_checkouts_without_a_record_are_left_alone() {
    let f = setup();
    let dir = baseline_dir(&f);
    std::fs::create_dir_all(dir.join("notes")).unwrap();
    std::fs::write(dir.join("notes").join("keep.txt"), "mine\n").unwrap();
    std::fs::write(dir.join("stray.txt"), "stray\n").unwrap();
    let old = format!("suite-{FIXED_ULID}");
    let old_path = dir.join(&old);
    common::git(&f.repo_path(), &["worktree", "add", "--detach", &old_path.display().to_string(), "HEAD"]);
    let files_before = names_in(&old_path);

    let expect = |doc: &Value| {
        let kept = list(doc, "kept");
        let got: Vec<(String, String, Value)> = kept
            .iter()
            .map(|k| (k["path"].as_str().unwrap().to_string(), k["reason"].as_str().unwrap().to_string(), k["detail"].clone()))
            .collect();
        assert_eq!(
            got,
            vec![
                (path_of(&f, "notes"), "not_thoth_checkout".to_string(), Value::Null),
                (path_of(&f, "stray.txt"), "not_thoth_checkout".to_string(), Value::Null),
                (path_of(&f, &old), "no_owner_record".to_string(), Value::Null),
            ],
            "{doc}"
        );
        assert_eq!(list(doc, "removed").len(), 0, "{doc}");
    };

    let b = fast_suite(&f);
    assert_eq!(b.code, 0, "{}\n{}", b.doc, b.stderr);
    assert_eq!(b.doc["ok"], true, "{}", b.doc);
    expect(&b.doc);
    assert!(!b.stderr.contains("leftover"), "{}", b.stderr);

    let p = prune(&f);
    assert_eq!((p.code, &p.doc["ok"]), (0, &Value::Bool(true)), "{}", p.doc);
    expect(&p.doc);

    assert_eq!(std::fs::read_to_string(dir.join("notes").join("keep.txt")).unwrap(), "mine\n");
    assert_eq!(std::fs::read_to_string(dir.join("stray.txt")).unwrap(), "stray\n");
    assert_eq!(names_in(&old_path), files_before);
    assert!(registered(&f, &old), "the hand-made checkout lost its registration");
    assert_eq!(names_in(&dir), vec!["notes".to_string(), "stray.txt".to_string(), old.clone()]);

    // the human prune names the manual removal for the record-less checkout
    let human = f.cmd().args(["worktrees", "prune"]).output().unwrap();
    let text = String::from_utf8_lossy(&human.stdout).to_string();
    assert!(
        text.contains(&format!(
            "  kept     baseline  {}: no_owner_record - remove it with git worktree remove --force {} once no thoth older than this version is running",
            path_of(&f, &old),
            path_of(&f, &old)
        )),
        "{text}"
    );
    assert!(text.contains(&format!("  kept     baseline  {}: not_thoth_checkout", path_of(&f, "notes"))), "{text}");
}

/// AC6 (e): a leftover that cannot be removed is kept as `remove_failed`
/// with the error, warned about, never fails the command, and goes once it
/// can be removed.
#[cfg(unix)]
#[test]
fn a_leftover_that_cannot_be_removed_is_reported_and_removed_later() {
    use std::os::unix::fs::PermissionsExt;
    let f = setup();
    let name = leave_leftover(&f);
    let path = path_of(&f, &name);
    let ro = Path::new(&path).join("locked");
    std::fs::create_dir(&ro).unwrap();
    std::fs::write(ro.join("inside.txt"), "x\n").unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    // Someone who can write into a read-only directory (root) can also
    // delete from it: nothing to show then.
    if std::fs::write(ro.join("probe.txt"), "p").is_ok() {
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: a read-only directory does not stop this user");
        return;
    }

    let check = |doc: &Value| {
        let kept = list(doc, "kept");
        assert_eq!(kept.len(), 1, "{doc}");
        assert_eq!(kept[0]["path"], path.as_str(), "{doc}");
        assert_eq!(kept[0]["reason"], "remove_failed", "{doc}");
        assert!(kept[0]["detail"].as_str().is_some_and(|d| !d.trim().is_empty()), "{doc}");
        assert_eq!(list(doc, "removed").len(), 0, "{doc}");
    };
    let b = fast_suite(&f);
    assert_eq!(b.code, 0, "{}\n{}", b.doc, b.stderr);
    assert_eq!(b.doc["ok"], true, "{}", b.doc);
    check(&b.doc);
    assert!(
        b.stderr.contains(&format!("thoth: warning: could not remove the leftover checkout {path}: ")),
        "{}",
        b.stderr
    );

    let p = prune(&f);
    assert_eq!((p.code, &p.doc["ok"]), (0, &Value::Bool(true)), "{}", p.doc);
    check(&p.doc);
    assert!(Path::new(&path).is_dir());

    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    let p = prune(&f);
    assert_eq!((p.code, &p.doc["ok"]), (0, &Value::Bool(true)), "{}", p.doc);
    let removed = list(&p.doc, "removed");
    assert_eq!(removed.len(), 1, "{}", p.doc);
    assert_eq!(removed[0]["path"], path.as_str(), "{}", p.doc);
    assert!(!Path::new(&path).exists());
    assert!(!registered(&f, &name));
    assert_eq!(names_in(&holds_dir(&f)), Vec::<String>::new());
}

/// `P.reached` and `P.go` of a test pause `P`.
fn pause_files(p: &Path) -> (PathBuf, PathBuf) {
    let with = |ext: &str| {
        let mut s = p.as_os_str().to_os_string();
        s.push(ext);
        PathBuf::from(s)
    };
    (with(".reached"), with(".go"))
}

/// SMALL-18 C1: a sweep that takes the free lock of a leftover's hold while
/// the checkout's own command is removing it (here `git worktree remove`
/// during the sweep's test pause, which sits right after the lock is taken)
/// reports nothing for it and deletes the hold without a checkout.
#[test]
fn a_checkout_removed_after_the_sweep_takes_its_lock_is_not_reported() {
    let f = setup();
    let name = leave_leftover(&f);
    let path = path_of(&f, &name);
    let pause = f.home_path().join("sweep-pause");
    let (reached, go) = pause_files(&pause);

    let mut c = f.std_cmd_of(env!("CARGO_BIN_EXE_thoth"));
    c.env("THOTH_TEST_SWEEP_PAUSE", &pause)
        .args(["worktrees", "prune", "--json"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut t = Thoth(Some(c.spawn().unwrap()));
    wait_for_file(&reached, 60);
    common::git(&f.repo_path(), &["worktree", "remove", "--force", &path]);
    assert!(!Path::new(&path).exists(), "git did not remove the checkout");
    std::fs::write(&go, "go").unwrap();
    let out = t.0.take().unwrap().wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).to_string();
    let doc: Value = serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("printed no JSON ({e}): {stderr}"));
    assert_eq!(out.status.code(), Some(0), "{doc}\n{stderr}");
    assert_eq!(list(&doc, "removed"), &Vec::<Value>::new(), "{doc}");
    assert_eq!(list(&doc, "kept"), &Vec::<Value>::new(), "{doc}");
    assert!(!stderr.contains(&path), "{stderr}");
    assert_eq!(names_in(&holds_dir(&f)), Vec::<String>::new());
}

/// The absolute path of the real `git` on PATH.
#[cfg(unix)]
fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").expect("PATH is set");
    std::env::split_paths(&path)
        .map(|d| d.join("git"))
        .find(|p| p.is_file())
        .expect("git must be on PATH")
}

/// SMALL-18 C2: the human line of a `remove_failed` baseline checkout carries
/// the whole detail on one line, each line break as `; `.
#[cfg(unix)]
#[test]
fn the_human_line_of_a_checkout_that_cannot_be_removed_carries_the_whole_detail() {
    use std::os::unix::fs::PermissionsExt;
    let f = setup();
    let name = leave_leftover(&f);
    let path = path_of(&f, &name);
    let ro = Path::new(&path).join("locked");
    std::fs::create_dir(&ro).unwrap();
    std::fs::write(ro.join("inside.txt"), "x\n").unwrap();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o555)).unwrap();
    if std::fs::write(ro.join("probe.txt"), "p").is_ok() {
        std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: a read-only directory does not stop this user");
        return;
    }

    // A `git` first on PATH whose `worktree remove` fails with two lines.
    let bin = f.home_path().join("fake-git-bin");
    std::fs::create_dir_all(&bin).unwrap();
    let script = bin.join("git");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nif [ \"$1\" = worktree ] && [ \"$2\" = remove ]; then\n  echo 'warning: first line' >&2\n  echo 'error: second line' >&2\n  exit 1\nfi\nexec '{}' \"$@\"\n",
            real_git().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![bin.clone()];
    paths.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let new_path = std::env::join_paths(paths).unwrap();

    let out = f.cmd().env("PATH", &new_path).args(["worktrees", "prune"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    std::fs::set_permissions(&ro, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(out.status.code(), Some(0), "{text}\n{}", String::from_utf8_lossy(&out.stderr));
    let line = text
        .lines()
        .find(|l| l.starts_with(&format!("  kept     baseline  {path}: remove_failed - ")))
        .unwrap_or_else(|| panic!("no remove_failed line: {text}"));
    assert!(line.contains("warning: first line; error: second line"), "{line}");
    assert!(line.contains("deleting the directory failed: "), "{line}");
}

/// SMALL-18 T4 (a): a hold with no checkout and an unlocked `.pending` file
/// are deleted by prune.
#[test]
fn prune_deletes_a_hold_without_a_checkout_and_an_unlocked_pending_file() {
    let f = setup();
    let holds = holds_dir(&f);
    std::fs::create_dir_all(&holds).unwrap();
    std::fs::write(holds.join(format!("suite-{FIXED_ULID}.hold")), "").unwrap();
    std::fs::write(holds.join("x.pending"), "").unwrap();
    assert_eq!(names_in(&holds).len(), 2);

    let p = prune(&f);
    assert_eq!((p.code, &p.doc["ok"]), (0, &Value::Bool(true)), "{}\n{}", p.doc, p.stderr);
    assert_eq!(list(&p.doc, "removed").len(), 0, "{}", p.doc);
    assert_eq!(list(&p.doc, "kept").len(), 0, "{}", p.doc);
    assert_eq!(names_in(&holds), Vec::<String>::new());
}

/// SMALL-18 T4 (b): a `baseline-worktrees` that cannot be read as a folder is
/// an error of the sweep, shown in --json and the human output, and never
/// fails prune.
#[test]
fn an_unreadable_baseline_folder_is_a_sweep_error_and_prune_still_succeeds() {
    let f = setup();
    let dir = baseline_dir(&f);
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, "not a folder\n").unwrap();
    let prefix = format!("reading {} failed: ", dir.display());

    let p = prune(&f);
    assert_eq!((p.code, &p.doc["ok"]), (0, &Value::Bool(true)), "{}\n{}", p.doc, p.stderr);
    let error = sweep_of(&p.doc)["error"].as_str().unwrap_or_else(|| panic!("no error: {}", p.doc));
    assert!(error.starts_with(&prefix), "{error}");
    assert_eq!(list(&p.doc, "removed").len(), 0, "{}", p.doc);
    assert_eq!(list(&p.doc, "kept").len(), 0, "{}", p.doc);

    let human = f.cmd().args(["worktrees", "prune"]).output().unwrap();
    assert_eq!(human.status.code(), Some(0));
    let text = String::from_utf8_lossy(&human.stdout).to_string();
    assert!(
        text.contains(&format!("  warning  baseline  could not look for leftover checkouts: {prefix}")),
        "{text}"
    );
}

/// SMALL-18 T4 (c): the human prune names a removed leftover and who left it.
#[test]
fn the_human_prune_names_a_removed_leftover_of_a_suite_run() {
    let f = setup();
    let name = leave_leftover(&f);
    let path = path_of(&f, &name);

    let human = f.cmd().args(["worktrees", "prune"]).output().unwrap();
    let text = String::from_utf8_lossy(&human.stdout).to_string();
    assert_eq!(human.status.code(), Some(0), "{text}");
    assert!(text.contains(&format!("  removed  baseline  {path}  (left by suite run)")), "{text}");
    assert!(!Path::new(&path).exists());
}
