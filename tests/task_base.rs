//! BASE-1: where a task starts. `task create --base REF`, or else the HEAD of
//! the worktree the command runs in - a linked worktree of a long-lived branch
//! included - and the two places that must not assume the main worktree's HEAD
//! contains that start: the diff's integration tip and the spec review checkout.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// The fixture repo plus branch `long`, checked out in a linked worktree `wt`
/// outside the repo directory, one commit ahead of main with `long.txt`.
struct LongBranch {
    f: Fixture,
    wt: PathBuf,
    _holder: TempDir,
}

impl LongBranch {
    fn repo(&self) -> PathBuf {
        self.f.repo_path()
    }
}

/// `prep` runs on main before the worktree is added.
fn long_branch_after(prep: impl FnOnce(&Path)) -> LongBranch {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let repo = f.repo_path();
    prep(&repo);
    let holder = tempfile::tempdir().unwrap();
    let wt = holder.path().join("wt");
    common::git(&repo, &["worktree", "add", "-b", "long", wt.to_str().unwrap()]);
    std::fs::write(wt.join("long.txt"), "only on long\n").unwrap();
    common::git(&wt, &["add", "long.txt"]);
    common::git(&wt, &["commit", "-m", "long only"]);
    LongBranch { f, wt, _holder: holder }
}

fn long_branch() -> LongBranch {
    long_branch_after(|_| {})
}

fn head(dir: &Path) -> String {
    common::git(dir, &["rev-parse", "HEAD"])
}

fn json_of(bytes: &[u8]) -> Value {
    serde_json::from_str(String::from_utf8_lossy(bytes).trim()).unwrap()
}

fn spec_file(f: &Fixture, spec: &str) -> PathBuf {
    let p = f.home_path().join("base-spec.json");
    std::fs::write(&p, spec).unwrap();
    p
}

/// `task create --json` run in `dir` with `extra`; exit 0.
fn create_in(f: &Fixture, dir: &Path, extra: &[&str]) -> Value {
    let mut c = f.cmd_in(dir);
    c.args(["task", "create", "-f"])
        .arg(spec_file(f, Fixture::SPEC))
        .arg("--json")
        .args(extra);
    json_of(&f.run_ok(c, "task create"))
}

/// `task create --json` that must exit 1; returns stderr's `error`.
fn create_err(f: &Fixture, dir: &Path, extra: &[&str]) -> String {
    let out = f
        .cmd_in(dir)
        .args(["task", "create", "-f"])
        .arg(spec_file(f, Fixture::SPEC))
        .arg("--json")
        .args(extra)
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    json_of(&out)["error"].as_str().unwrap().to_string()
}

fn status_task_ids(f: &Fixture, dir: &Path) -> Vec<String> {
    let mut c = f.cmd_in(dir);
    c.args(["status", "--json"]);
    json_of(&f.run_ok(c, "status"))["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["task_id"].as_str().unwrap().to_string())
        .collect()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap().to_string()
}

fn is_full_sha(v: &str) -> bool {
    v.len() == 40 && v.chars().all(|c| c.is_ascii_hexdigit())
}

/// AC1: `--base` takes anything that names a commit and stores its full sha.
#[test]
fn base_names_a_branch_or_a_short_sha_and_stores_the_full_sha() {
    let lb = long_branch();
    let repo = lb.repo();
    let long = common::git(&repo, &["rev-parse", "long"]);
    let v = create_in(&lb.f, &repo, &["--base", "long"]);
    assert_eq!(v["base_commit"], long);
    assert!(is_full_sha(&s(&v["base_commit"])), "{v}");

    let first = common::git(&repo, &["rev-list", "--max-parents=0", "main"]);
    let v = create_in(&lb.f, &repo, &["--base", &first[..7]]);
    assert_eq!(v["base_commit"], first);
}

/// AC1: a name that is no commit exits 1 and writes nothing.
#[test]
fn a_base_that_names_no_commit_exits_1_and_writes_nothing() {
    let lb = long_branch();
    let repo = lb.repo();
    let before = status_task_ids(&lb.f, &repo).len();
    let tree = common::git(&repo, &["rev-parse", "HEAD^{tree}"]);
    let cases: Vec<(Vec<String>, String)> = vec![
        (vec!["--base".into(), "nosuch".into()], "nosuch".into()),
        (vec!["--base=-x".into()], "-x".into()),
        (vec!["--base".into(), tree.clone()], tree.clone()),
        (vec!["--base".into(), "  ".into()], "  ".into()),
    ];
    for (args, rev) in cases {
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let err = create_err(&lb.f, &repo, &args);
        assert!(err.starts_with("--base "), "{args:?}: {err}");
        assert!(err.contains(&rev), "{args:?}: {err}");
    }
    assert_eq!(status_task_ids(&lb.f, &repo).len(), before);
}

/// AC2 (a)-(d): without `--base`, the start is the HEAD of the worktree the
/// command runs in; every worktree shares one state folder.
#[test]
fn without_base_the_start_is_the_head_of_the_worktree_the_command_runs_in() {
    let lb = long_branch();
    let repo = lb.repo();
    let wt_head = head(&lb.wt);
    assert_ne!(wt_head, head(&repo));

    let a = create_in(&lb.f, &lb.wt, &[]);
    assert_eq!(a["base_commit"], wt_head);

    let sub = lb.wt.join("deep").join("er");
    std::fs::create_dir_all(&sub).unwrap();
    let b = create_in(&lb.f, &sub, &[]);
    assert_eq!(b["base_commit"], wt_head);

    let c = create_in(&lb.f, &repo, &[]);
    assert_eq!(c["base_commit"], head(&repo));

    let from_main = status_task_ids(&lb.f, &repo);
    assert!(from_main.contains(&s(&a["task_id"])), "{from_main:?}");
    assert!(from_main.contains(&s(&b["task_id"])), "{from_main:?}");
    let from_wt = status_task_ids(&lb.f, &lb.wt);
    assert!(from_wt.contains(&s(&c["task_id"])), "{from_wt:?}");
}

/// AC2 (e) and AC4: a detached worktree starts at its HEAD, on no branch.
#[test]
fn a_detached_worktree_starts_at_its_head_with_no_base_branch() {
    let lb = long_branch();
    common::git(&lb.wt, &["checkout", "--detach"]);
    let v = create_in(&lb.f, &lb.wt, &[]);
    assert_eq!(v["base_commit"], head(&lb.wt));
    assert_eq!(v["base_branch"], Value::Null);
}

/// AC4: `base_branch` names a local branch the start was taken from, and the
/// same value comes back from `task show` and `status TASK`.
#[test]
fn base_branch_is_the_local_branch_the_task_started_from() {
    let lb = long_branch();
    let repo = lb.repo();
    let sha = head(&repo);
    let cases: Vec<(&Path, Vec<&str>, Value)> = vec![
        (&lb.wt, vec![], Value::from("long")),
        (&repo, vec!["--base", "long"], Value::from("long")),
        (&repo, vec!["--base", &sha], Value::Null),
        (&repo, vec!["--base", "long~1"], Value::Null),
        (&repo, vec![], Value::from("main")),
    ];
    for (dir, extra, expected) in cases {
        let v = create_in(&lb.f, dir, &extra);
        assert_eq!(v["base_branch"], expected, "{extra:?}: {v}");
        let task = s(&v["task_id"]);

        let mut c = lb.f.cmd();
        c.args(["task", "show", &task, "--json"]);
        assert_eq!(json_of(&lb.f.run_ok(c, "task show"))["base_branch"], expected);
        let mut c = lb.f.cmd();
        c.args(["status", &task, "--json"]);
        assert_eq!(json_of(&lb.f.run_ok(c, "status"))["base_branch"], expected);

        let mut c = lb.f.cmd();
        c.args(["events", "--since", "0", "--task", &task, "--json"]);
        let events = json_of(&lb.f.run_ok(c, "events"));
        let created = events["events"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["type"] == "task_created")
            .unwrap()
            .clone();
        assert_eq!(created["detail"]["base_branch"], expected);
        let base_ref = extra.get(1).map(|r| Value::from(*r)).unwrap_or(Value::Null);
        assert_eq!(created["detail"]["base_ref"], base_ref);
    }
}

/// AC5: dispatch, the worker's branch and `verify --at-base` all use the
/// stored sha, with no change of their own.
#[test]
fn a_task_started_in_a_linked_worktree_is_dispatched_from_its_base() {
    let lb = long_branch();
    let repo = lb.repo();
    let created = create_in(&lb.f, &lb.wt, &[]);
    let task = s(&created["task_id"]);
    let b = s(&created["base_commit"]);
    lb.f.add_profile("claude-b");
    let d = lb.f.dispatch(&task, "claude-b", "ok", &[]);
    lb.f.wait_for(&task, "180s");

    let worker = PathBuf::from(s(&d["worktree"]));
    assert!(worker.join("long.txt").is_file(), "{}", worker.display());
    assert_eq!(
        common::git(&worker, &["symbolic-ref", "--short", "HEAD"]),
        format!("thoth/{task}/a1")
    );
    common::git(&worker, &["merge-base", "--is-ancestor", &b, "HEAD"]);

    let out = lb
        .f
        .cmd_in(&repo)
        .args(["verify", &s(&d["run_id"]), "--at-base", "--dry-run", "--json"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v = json_of(&out);
    assert_eq!(v["at_commit"], b);
    assert_eq!(v["reason"], "no_approved_checks");
}

/// AC6: main's HEAD does not contain the task's base, so it is no integration
/// tip; a test file put back to main's content is still the task's change.
#[test]
fn putting_back_mains_version_of_a_test_on_a_long_branch_is_a_protected_change() {
    let lb = long_branch_after(|repo| {
        std::fs::create_dir_all(repo.join("tests")).unwrap();
        std::fs::write(repo.join("tests").join("x_test.rs"), "written by the fake worker\n").unwrap();
        common::git(repo, &["add", "tests/x_test.rs"]);
        common::git(repo, &["commit", "-m", "main's x_test"]);
    });
    std::fs::write(lb.wt.join("tests").join("x_test.rs"), "changed on long\n").unwrap();
    common::git(&lb.wt, &["add", "tests/x_test.rs"]);
    common::git(&lb.wt, &["commit", "-m", "long changes x_test"]);

    let task = s(&create_in(&lb.f, &lb.wt, &[])["task_id"]);
    lb.f.add_profile("claude-b");
    let mut c = lb.f.cmd();
    c.env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_WRITE_FILE", "tests/x_test.rs")
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let d = json_of(&lb.f.run_ok(c, "dispatch"));
    let run = s(&d["run_id"]);
    let (_, status) = lb.f.wait_for(&task, "180s");
    assert_eq!(status, "unverified", "{}", lb.f.explain_task(&task));

    let mut c = lb.f.cmd();
    c.args(["events", "--since", "0", "--task", &task, "--json"]);
    let events = json_of(&lb.f.run_ok(c, "events"));
    let invalid = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["type"] == "result_invalid")
        .expect("a result_invalid event")
        .clone();
    assert_eq!(invalid["detail"]["rule"], 6, "{invalid}");
    assert_eq!(invalid["detail"]["path"], "tests/x_test.rs", "{invalid}");

    let mut c = lb.f.cmd();
    c.args(["evidence", "list", &task, "--json"]);
    let listed = json_of(&lb.f.run_ok(c, "evidence list"));
    let code = listed["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["kind"] == "diff" && e["run_id"] == run.as_str())
        .expect("a diff evidence row")["code"]
        .as_str()
        .unwrap()
        .to_string();
    let mut c = lb.f.cmd();
    c.args(["evidence", "show", &code, "--task", &task, "--json"]);
    let shown = json_of(&lb.f.run_ok(c, "evidence show"));
    assert_eq!(shown["detail"]["integration_tip"], Value::Null, "{shown}");
}

/// `spec review start TASK --json` in `dir`, then waits for the run.
fn spec_review(f: &Fixture, dir: &Path, task: &str) -> Value {
    let mut c = f.cmd_in(dir);
    c.env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", task)
        .args(["spec", "review", "start", task, "--profile", "claude-b", "--model", "haiku", "--json"]);
    let v = json_of(&f.run_ok(c, "spec review start"));
    let mut c = f.cmd();
    c.args(["wait", "--run", &s(&v["run_id"]), "--timeout", "180s", "--json"]);
    f.run_ok(c, "wait --run");
    v
}

/// AC7: the spec reviewer sees the HEAD of the worktree the command runs in
/// when that contains the task's base, and the base itself otherwise.
#[test]
fn the_spec_review_checks_out_the_callers_head_only_when_it_contains_the_base() {
    let lb = long_branch();
    let repo = lb.repo();
    lb.f.add_profile("claude-b");
    let a = create_in(&lb.f, &lb.wt, &[]);
    let task_a = s(&a["task_id"]);
    let b1 = s(&a["base_commit"]);
    std::fs::write(lb.wt.join("later.txt"), "after the task was created\n").unwrap();
    common::git(&lb.wt, &["add", "later.txt"]);
    common::git(&lb.wt, &["commit", "-m", "later on long"]);
    let b2 = head(&lb.wt);

    let r = spec_review(&lb.f, &lb.wt, &task_a);
    assert_eq!(r["reviewed_commit"], b2);
    assert_eq!(r["task_base_commit"], b1);

    let r = spec_review(&lb.f, &repo, &task_a);
    assert_eq!(r["reviewed_commit"], b1);

    let c = create_in(&lb.f, &repo, &[]);
    let r = spec_review(&lb.f, &repo, &s(&c["task_id"]));
    assert_eq!(r["reviewed_commit"], head(&repo));
}
