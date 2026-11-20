//! WT-1: Thoth removes the worktrees of finished (pass / cancelled) tasks
//! itself, keeps every commit and piece of evidence, and keeps (and reports)
//! any worktree that is not safe to drop. A full sweep also deletes the task
//! branches whose work is already in the task's base branch, and a worktree's
//! directory links are removed without following them before git removes it.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Runs the `hooks` implementer scenario to completion and returns
/// `(task_id, implementer_run_id)` with the task sitting at `in_review`.
/// Copied from `tests/review.rs`'s private `candidate_ready`.
fn candidate_ready(f: &common::Fixture, spec: &str) -> (String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task_id, _) = f.create_task(spec);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");
    (task_id, run_id)
}

/// Copied from `tests/control.rs`.
fn wait_until_running(f: &common::Fixture, run_id: &str) -> i64 {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        {
            let conn = f.state_conn();
            let row: Option<(String, Option<i64>)> = conn
                .query_row(
                    "SELECT status, worker_pid FROM runs WHERE run_id = ?1",
                    [run_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .ok();
            if let Some((status, Some(pid))) = row {
                if status == "running" {
                    return pid;
                }
            }
        }
        assert!(Instant::now() < deadline, "run {run_id} never reached running");
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Polls every 100 ms until `path` no longer exists; panics after `secs`.
fn wait_gone(path: &Path, secs: u64) {
    let deadline = Instant::now() + Duration::from_secs(secs);
    while path.exists() {
        assert!(
            Instant::now() < deadline,
            "{} still exists after {secs}s",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The parsed `detail` of every event of type `ty` for `task_id`, in seq order.
fn events_detail(f: &Fixture, task_id: &str, ty: &str) -> Vec<Value> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT detail FROM events WHERE task_id = ?1 AND type = ?2 ORDER BY seq")
        .unwrap();
    stmt.query_map([task_id, ty], |r| r.get::<_, String>(0))
        .unwrap()
        .map(|d| serde_json::from_str(&d.unwrap()).unwrap())
        .collect()
}

fn worktree_list(f: &Fixture) -> String {
    common::git(&f.repo_path(), &["worktree", "list", "--porcelain"])
}

/// Whether `git worktree list` still has an entry named like `path`'s last
/// component. Compared by name, not by substring: `TASK-a1` is a prefix of
/// `TASK-a1-review`, and git prints forward slashes on Windows.
fn listed(f: &Fixture, path: &Path) -> bool {
    let name = path.file_name().unwrap();
    worktree_list(f)
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .any(|p| Path::new(p).file_name() == Some(name))
}

fn branch_exists(f: &Fixture, name: &str) -> bool {
    std::process::Command::new("git")
        .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{name}")])
        .current_dir(f.repo_path())
        .status()
        .unwrap()
        .success()
}

fn status_json(f: &Fixture, task_id: &str) -> Value {
    let out = f
        .cmd()
        .args(["status", task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

/// `worktrees prune --json`: (exit code, parsed stdout). Exit 1 still prints
/// the document.
fn prune_json(f: &Fixture) -> (i32, Value) {
    let out = f.cmd().args(["worktrees", "prune", "--json"]).output().unwrap();
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "worktrees prune --json printed no JSON ({e}): stdout {} stderr {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.code().unwrap(), v)
}

fn set_run_status(f: &Fixture, run_id: &str, status: &str) {
    f.state_conn()
        .execute("UPDATE runs SET status = ?1 WHERE run_id = ?2", [status, run_id])
        .unwrap();
}

fn set_task_status(f: &Fixture, task_id: &str, status: &str) {
    f.state_conn()
        .execute(
            "UPDATE tasks SET status = ?1 WHERE task_id = ?2",
            [status, task_id],
        )
        .unwrap();
}

fn no_worktree_events(f: &Fixture, task_id: &str) {
    let events = f.events_of(task_id);
    assert!(
        !events.iter().any(|e| e == "worktree_removed" || e == "worktree_kept"),
        "{events:?}"
    );
}

fn path_of(v: &Value) -> PathBuf {
    PathBuf::from(v.as_str().unwrap())
}

/// (a) the runner removes the worker worktree the moment a reviewer PASS
/// settles the task, keeps the branch and the evidence, and leaves the review
/// worktree for the next full sweep - which the next `task create` is.
#[test]
fn a_reviewer_pass_removes_the_worker_worktree_and_keeps_branch_evidence_and_the_review_worktree() {
    let f = common::fixture();
    let (task_id, _implementer_run) = candidate_ready(&f, Fixture::SPEC_TWO_AC);
    let st = status_json(&f, &task_id);
    let run = &st["runs"][0];
    let wt = path_of(&run["worktree"]);
    let run_dir = run["run_dir"].as_str().unwrap().to_string();
    let candidate = run["candidate_commit"].as_str().unwrap().to_string();
    let before = f.evidence_rows(&task_id);
    assert!(!before.is_empty());

    let r = f.review_start(&task_id, "claude-a", "review_pass", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    let review_wt = path_of(&r["worktree"]);
    let out = f
        .cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["task_status"], "pass", "{v}");

    wait_gone(&wt, 30);
    assert!(!listed(&f, &wt), "{}", worktree_list(&f));
    let branch = format!("thoth/{task_id}/a1");
    assert!(branch_exists(&f, &branch));
    assert_eq!(
        common::git(&f.repo_path(), &["rev-parse", &format!("refs/heads/{branch}")]),
        candidate
    );
    let cat = std::process::Command::new("git")
        .args(["cat-file", "-e", &format!("{candidate}^{{commit}}")])
        .current_dir(f.repo_path())
        .status()
        .unwrap();
    assert!(cat.success());
    assert!(review_wt.is_dir(), "the runner must not remove the review worktree");
    assert!(Path::new(&run_dir).is_dir());
    let after = f.evidence_rows(&task_id);
    assert_eq!(after, before);
    for row in &after {
        if let Some(blob) = row["blob_path"].as_str() {
            assert!(Path::new(blob).is_file(), "{blob}");
        }
    }
    f.cmd()
        .args(["evidence", "show", "E001", "--task", &task_id, "--json"])
        .assert()
        .success();

    let removed = events_detail(&f, &task_id, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["path"], wt.display().to_string());
    assert_eq!(removed[0]["kind"], "worker");
    assert_eq!(removed[0]["task_status"], "pass");
    assert_eq!(removed[0]["source"], "runner");
    assert_eq!(removed[0]["branch"], branch.as_str());
    assert_eq!(removed[0]["head"], candidate.as_str());
    assert!(events_detail(&f, &task_id, "worktree_kept").is_empty());

    // the next task's creation sweeps the review worktree
    let (second, _) = f.create_task(Fixture::SPEC);
    assert!(!review_wt.exists(), "{} survived task create", review_wt.display());
    assert!(!listed(&f, &review_wt), "{}", worktree_list(&f));
    let removed = events_detail(&f, &task_id, "worktree_removed");
    assert_eq!(removed.len(), 2, "{removed:?}");
    assert_eq!(removed[1]["kind"], "review");
    assert_eq!(removed[1]["source"], "task_create");
    assert_eq!(removed[1]["branch"], Value::Null);
    assert_eq!(removed[1]["head"], candidate.as_str());
    assert_eq!(f.events_of(&second), vec!["task_created".to_string()]);
}

/// (b) `review record` PASS removes the worker worktree synchronously and
/// keeps its six `--json` keys; the human output names what it removed.
#[test]
fn review_record_pass_removes_the_worker_worktree_without_changing_the_json_keys() {
    let review_json = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":["E001"]},{"id":"AC2","verdict":"met","evidence":["E001"]}],"extra_findings":[]}}"#;
    let ready = |f: &Fixture| -> (String, PathBuf, PathBuf) {
        f.cmd().arg("init").assert().success();
        f.add_profile("claude-b");
        let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
        f.dispatch(&task_id, "claude-b", "hooks", &[]);
        let (_, status) = f.wait_for(&task_id, "180s");
        assert_eq!(status, "in_review");
        let wt = path_of(&status_json(f, &task_id)["runs"][0]["worktree"]);
        let file = f.home_path().join("review-pass.json");
        std::fs::write(&file, review_json).unwrap();
        (task_id, wt, file)
    };

    let f = common::fixture();
    let (task_id, wt, file) = ready(&f);
    let out = f
        .cmd()
        .args(["review", "record", &task_id, "-f"])
        .arg(&file)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(
        keys,
        vec!["review_id", "source", "target_run_id", "task_id", "task_status", "verdict"]
    );
    assert!(!wt.exists(), "{} was not removed", wt.display());
    assert!(branch_exists(&f, &format!("thoth/{task_id}/a1")));
    let removed = events_detail(&f, &task_id, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["source"], "review_record");

    let g = common::fixture();
    let (task_id, wt, file) = ready(&g);
    let out = g
        .cmd()
        .args(["review", "record", &task_id, "-f"])
        .arg(&file)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(
        text.contains(&format!("removed worktree {}", wt.display())),
        "{text}"
    );
}

/// (c) a worktree with a stray file is kept (one event, however many sweeps
/// see it) until the file is gone, and then `worktrees prune` removes it.
#[test]
fn a_dirty_worker_worktree_is_kept_once_and_prune_removes_it_when_clean() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, wt) = f.seed_run("claude-b", Fixture::SPEC);
    std::fs::write(wt.join("leftover.txt"), "left\n").unwrap();
    set_run_status(&f, &run_id, "failed");
    set_task_status(&f, &task_id, "failed");

    let out = f
        .cmd()
        .args(["task", "close", &task_id, "--reason", "gone", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["status"], "cancelled");
    assert_eq!(v["worktrees"]["removed"], serde_json::json!([]));
    let kept = v["worktrees"]["kept"].as_array().unwrap();
    assert_eq!(kept.len(), 1, "{v}");
    assert_eq!(kept[0]["path"], wt.display().to_string());
    assert_eq!(kept[0]["kind"], "worker");
    assert_eq!(kept[0]["reason"], "uncommitted_changes");
    assert!(kept[0]["detail"].as_str().unwrap().contains("leftover.txt"), "{v}");
    assert!(wt.is_dir());
    let events = cli_events(&f, &task_id, "worktree_kept");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["source"], "task_close");

    let (code, p) = prune_json(&f);
    assert_eq!(code, 0, "{p}");
    assert_eq!(p["ok"], true);
    assert_eq!(p["kept"].as_array().unwrap().len(), 1, "{p}");
    assert_eq!(p["removed"], serde_json::json!([]));
    assert_eq!(p["tasks_checked"], 1);
    assert_eq!(cli_events(&f, &task_id, "worktree_kept").len(), 1, "deduplicated");

    let out = f
        .cmd()
        .args(["worktrees", "prune"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("kept     worker  "), "{text}");
    assert!(text.contains("git worktree remove --force "), "{text}");

    std::fs::remove_file(wt.join("leftover.txt")).unwrap();
    let (code, p) = prune_json(&f);
    assert_eq!(code, 0, "{p}");
    let removed = p["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1, "{p}");
    assert_eq!(removed[0]["path"], wt.display().to_string());
    assert_eq!(p["kept"], serde_json::json!([]));
    assert!(!wt.exists());
    // The seeded branch never left the base commit, so once its worktree is
    // gone the same full sweep deletes it as merged (branch cleanup).
    assert!(!branch_exists(&f, &format!("thoth/{task_id}/a1")));
    assert_eq!(p["removed_branches"][0]["merged_by"], "ancestor", "{p}");
    let removed = cli_events(&f, &task_id, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["source"], "worktrees_prune");
}

/// (d) `.agent/` and `.agents/` alone, even when git sees them as untracked,
/// do not keep a worktree.
#[test]
fn agent_dirs_alone_do_not_make_a_worktree_dirty() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, wt) = f.seed_run("claude-b", Fixture::SPEC);
    std::fs::create_dir_all(wt.join(".agent")).unwrap();
    std::fs::write(wt.join(".agent").join("result.json"), "{}").unwrap();
    std::fs::create_dir_all(wt.join(".agents")).unwrap();
    std::fs::write(wt.join(".agents").join("hooks.json"), "{}").unwrap();
    let porcelain = common::git(&wt, &["status", "--porcelain"]);
    assert!(porcelain.contains(".agent/"), "precondition: {porcelain}");
    set_run_status(&f, &run_id, "completed");
    set_task_status(&f, &task_id, "pass");

    let (_, p) = prune_json(&f);
    assert_eq!(p["removed"].as_array().unwrap().len(), 1, "{p}");
    assert_eq!(p["kept"], serde_json::json!([]));
    assert!(!wt.exists());
}

/// (e) every status but pass / cancelled keeps its worktree, and so does a
/// pass task that still has a live run.
#[test]
fn worktrees_of_unfinished_tasks_are_never_removed() {
    for status in [
        "draft",
        "queued",
        "running",
        "in_review",
        "reject",
        "unverified",
        "interrupted",
        "failed",
    ] {
        let f = common::fixture();
        f.cmd().arg("init").assert().success();
        f.add_profile("claude-b");
        let (task_id, run_id, wt) = f.seed_run("claude-b", Fixture::SPEC);
        set_task_status(&f, &task_id, status);
        if ["in_review", "reject", "unverified", "failed"].contains(&status) {
            set_run_status(&f, &run_id, "completed");
        }
        let (_, p) = prune_json(&f);
        assert_eq!(p["removed"], serde_json::json!([]), "{status}: {p}");
        assert_eq!(p["kept"], serde_json::json!([]), "{status}: {p}");
        assert_eq!(p["tasks_checked"], 0, "{status}: {p}");
        assert!(wt.is_dir(), "{status}");
        no_worktree_events(&f, &task_id);
    }

    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _run_id, wt) = f.seed_run("claude-b", Fixture::SPEC);
    set_task_status(&f, &task_id, "pass");
    let (_, p) = prune_json(&f);
    assert_eq!(p["removed"], serde_json::json!([]), "{p}");
    assert_eq!(p["kept"], serde_json::json!([]), "{p}");
    assert_eq!(p["tasks_checked"], 0, "{p}");
    assert!(wt.is_dir());
    no_worktree_events(&f, &task_id);
}

/// (f) `task create` sweeps a cancelled task's worktree and keeps its own
/// `--json` keys.
#[test]
fn a_cancelled_task_is_swept_by_the_next_task_create() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, wt) = f.seed_run("claude-b", Fixture::SPEC);
    set_run_status(&f, &run_id, "cancelled");
    set_task_status(&f, &task_id, "cancelled");

    let spec = f.home_path().join("second-spec.json");
    std::fs::write(&spec, Fixture::SPEC).unwrap();
    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&spec)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["base_branch", "base_commit", "ok", "status", "task_id", "title", "warnings"]);
    assert_eq!(v["warnings"], serde_json::json!([]));
    assert!(!wt.exists(), "{} was not removed", wt.display());
    let removed = events_detail(&f, &task_id, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["source"], "task_create");
    assert_eq!(removed[0]["task_status"], "cancelled");
}

/// (g) a directory deleted by hand is skipped without an event, and git
/// forgets its registration.
#[test]
fn a_worktree_whose_directory_is_already_gone_only_prunes_gits_bookkeeping() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, wt) = f.seed_run("claude-b", Fixture::SPEC);
    set_run_status(&f, &run_id, "completed");
    set_task_status(&f, &task_id, "pass");
    std::fs::remove_dir_all(&wt).unwrap();
    assert!(listed(&f, &wt), "git still knows it: {}", worktree_list(&f));

    let (code, p) = prune_json(&f);
    assert_eq!(code, 0, "{p}");
    assert_eq!(p["removed"], serde_json::json!([]));
    assert_eq!(p["kept"], serde_json::json!([]));
    assert_eq!(p["tasks_checked"], 1);
    assert!(!listed(&f, &wt), "{}", worktree_list(&f));
    no_worktree_events(&f, &task_id);
}

/// (h) a HEAD no branch reaches would lose its commit with the directory.
#[test]
fn a_worktree_whose_head_no_branch_reaches_is_kept() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, wt) = f.seed_run("claude-b", Fixture::SPEC);
    common::git(&wt, &["checkout", "--detach"]);
    common::git(&wt, &["config", "user.email", "test@example.com"]);
    common::git(&wt, &["config", "user.name", "thoth test"]);
    common::git(&wt, &["commit", "--allow-empty", "-m", "detached"]);
    set_run_status(&f, &run_id, "completed");
    set_task_status(&f, &task_id, "pass");

    let (_, p) = prune_json(&f);
    let kept = p["kept"].as_array().unwrap();
    assert_eq!(kept.len(), 1, "{p}");
    assert_eq!(kept[0]["reason"], "unreachable_head");
    assert_eq!(kept[0]["detail"], common::git(&wt, &["rev-parse", "HEAD"]).as_str());
    assert!(wt.is_dir());
}

/// (i) a run row pointing outside Thoth's worktrees directory is never removed.
#[test]
fn a_worktree_outside_the_worktrees_dir_is_never_removed() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _wt) = f.seed_run("claude-b", Fixture::SPEC);
    let base: String = f
        .state_conn()
        .query_row("SELECT base_commit FROM tasks WHERE task_id = ?1", [&task_id], |r| r.get(0))
        .unwrap();
    let elsewhere = f.home_path().join("elsewhere");
    let out = std::process::Command::new("git")
        .arg("worktree")
        .arg("add")
        .arg(&elsewhere)
        .arg("-b")
        .arg(format!("thoth/{task_id}/outside"))
        .arg(&base)
        .current_dir(f.repo_path())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    f.state_conn()
        .execute(
            "UPDATE runs SET worktree = ?1 WHERE run_id = ?2",
            [elsewhere.display().to_string(), run_id.clone()],
        )
        .unwrap();
    set_run_status(&f, &run_id, "completed");
    set_task_status(&f, &task_id, "pass");

    let (_, p) = prune_json(&f);
    let kept = p["kept"].as_array().unwrap();
    assert_eq!(kept.len(), 1, "{p}");
    assert_eq!(kept[0]["reason"], "outside_worktrees_dir");
    assert_eq!(p["removed"], serde_json::json!([]));
    assert!(elsewhere.is_dir());
}

/// (j) the command is listed, and refuses inside a run and inside a worktree.
#[test]
fn prune_refuses_inside_a_run_and_inside_a_worktree_and_shows_in_help() {
    let f = common::fixture();
    let out = f.cmd().arg("--help").assert().success().get_output().stdout.clone();
    assert!(String::from_utf8_lossy(&out).contains("worktrees"));
    let out = f
        .cmd()
        .args(["worktrees", "prune", "--help"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("--json"));

    f.cmd().arg("init").assert().success();
    let out = f
        .cmd()
        .env("THOTH_RUN_ID", "01RUN")
        .args(["worktrees", "prune"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("THOTH_RUN_ID"));

    f.add_profile("claude-b");
    let (_task_id, _run_id, wt) = f.seed_run("claude-b", Fixture::SPEC);
    let out = f
        .cmd_in(&wt)
        .args(["worktrees", "prune"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    assert!(String::from_utf8_lossy(&out).contains("main working tree"));
    assert!(wt.is_dir());
}

/// (k) a cancelled implementer run's runner removes its clean worktree.
#[test]
fn a_cancelled_implementer_run_is_swept_by_the_runner() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "slow", &["--timeout", "10m"]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    wait_until_running(&f, &run_id);
    let wt = path_of(&status_json(&f, &task_id)["runs"][0]["worktree"]);
    let out = f
        .cmd()
        .args(["cancel", &run_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["status"], "cancelled", "{v}");
    let (_, status) = f.wait_for(&task_id, "60s");
    assert_eq!(status, "cancelled");

    if cfg!(windows) {
        // A file the dying worker still holds makes git refuse; that is the
        // one acceptable way for the directory to stay.
        let deadline = Instant::now() + Duration::from_secs(60);
        while wt.exists() && events_detail(&f, &task_id, "worktree_kept").is_empty() {
            assert!(Instant::now() < deadline, "{} still exists", wt.display());
            std::thread::sleep(Duration::from_millis(100));
        }
        if wt.exists() {
            let kept = events_detail(&f, &task_id, "worktree_kept");
            assert_eq!(kept[0]["reason"], "remove_failed", "{kept:?}");
            assert!(branch_exists(&f, &format!("thoth/{task_id}/a1")));
            return;
        }
    } else {
        wait_gone(&wt, 60);
    }
    // The runner removes the directory first and appends `worktree_removed`
    // after it, so the event can land a moment after the directory is gone.
    // Wait for it through the public interface before reading the details.
    let gone_at = Instant::now();
    let deadline = gone_at + Duration::from_secs(30);
    loop {
        let out = f
            .cmd()
            .args(["events", "--task", &task_id, "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: Value = serde_json::from_slice(&out).unwrap();
        let removed_seen = v["events"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "worktree_removed");
        if removed_seen {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{} is gone but no worktree_removed event was written in {:?}",
            wt.display(),
            gone_at.elapsed()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let removed = events_detail(&f, &task_id, "worktree_removed");
    assert_eq!(removed.len(), 1, "{removed:?}");
    assert_eq!(removed[0]["source"], "runner");
    assert_eq!(removed[0]["task_status"], "cancelled");
    assert!(branch_exists(&f, &format!("thoth/{task_id}/a1")));
}

/// (l) a run of a finished task is not resumed: its worktree may be gone.
#[test]
fn resume_refuses_a_run_of_a_cancelled_or_passed_task() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _wt) = f.seed_run("claude-b", Fixture::SPEC);
    // `started_at` too: resume refuses a run that never started before it
    // ever looks at the task.
    f.state_conn()
        .execute(
            "UPDATE runs SET status = 'interrupted', interrupted_reason = 'crash',
               started_at = '2026-10-01T00:00:00Z' WHERE run_id = ?1",
            [&run_id],
        )
        .unwrap();
    set_task_status(&f, &task_id, "interrupted");
    f.cmd()
        .args(["task", "close", &task_id, "--reason", "gone", "--json"])
        .assert()
        .success();
    let running_before = running_changes(&f, &task_id);

    let out = f
        .cmd()
        .args(["resume", &run_id, "--json"])
        .assert()
        .code(1)
        .get_output()
        .stderr
        .clone();
    let err = String::from_utf8_lossy(&out).to_string();
    assert!(err.contains("resume needs a task that is not pass, cancelled or accepted"), "{err}");
    assert_eq!(f.task_status(&task_id), "cancelled");
    let run_status: String = f
        .state_conn()
        .query_row("SELECT status FROM runs WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(run_status, "interrupted");
    assert_eq!(running_changes(&f, &task_id), running_before);
}

/// `status_change` events that moved the task to running.
fn running_changes(f: &Fixture, task_id: &str) -> usize {
    events_detail(f, task_id, "status_change")
        .iter()
        .filter(|d| d["entity"] == "task" && d["to"] == "running")
        .count()
}

// ---------------------------------------------------------------------------
// Branch cleanup: merged task branches are deleted by `worktrees prune` and
// `task create`; everything else is kept with a reason.
// ---------------------------------------------------------------------------

const REVIEW_PASS: &str = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":["E001"]},{"id":"AC2","verdict":"met","evidence":["E001"]}],"extra_findings":[]}}"#;
const REVIEW_REJECT: &str = r#"{"verdict":"REJECT","findings":{"criteria":[{"id":"AC1","verdict":"unmet","note":"not yet"},{"id":"AC2","verdict":"met","evidence":["E001"]}],"extra_findings":[]}}"#;

/// One implemented task, as the CLI and git report it.
struct Implemented {
    task_id: String,
    branch: String,
    tip: String,
    wt: PathBuf,
}

fn setup(f: &Fixture) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
}

/// `task create` from the main tree as it stands, `dispatch` with the
/// committing `hooks` scenario, and wait for `in_review`. The branch and tip
/// are read from the worktree `status --json` names.
fn implemented(f: &Fixture) -> Implemented {
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let (_, status) = f.wait_for(&task_id, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task_id));
    let st = status_json(f, &task_id);
    let wt = path_of(&st["runs"][0]["worktree"]);
    let branch = common::git(&wt, &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(branch, format!("thoth/{task_id}/a1"));
    let tip = common::git(&wt, &["rev-parse", "HEAD"]);
    assert_eq!(st["runs"][0]["candidate_commit"], tip.as_str(), "{st}");
    Implemented { task_id, branch, tip, wt }
}

/// `review record TASK -f FILE --json` with `json`; returns the task status.
fn record(f: &Fixture, task_id: &str, json: &str) -> String {
    let file = f.home_path().join("review-record.json");
    std::fs::write(&file, json).unwrap();
    let mut c = f.cmd();
    c.args(["review", "record", task_id, "-f"]).arg(&file).arg("--json");
    let out = f.run_ok(c, "review record");
    let v: Value = serde_json::from_slice(&out).unwrap();
    v["task_status"].as_str().unwrap().to_string()
}

fn passed(f: &Fixture) -> Implemented {
    let t = implemented(f);
    assert_eq!(record(f, &t.task_id, REVIEW_PASS), "pass");
    t
}

/// Every local branch, as `git branch --list --format=%(refname:short)`
/// prints them, sorted.
fn branches(f: &Fixture) -> Vec<String> {
    let mut v: Vec<String> = common::git(
        &f.repo_path(),
        &["branch", "--list", "--format=%(refname:short)"],
    )
    .lines()
    .map(|l| l.trim().to_string())
    .filter(|l| !l.is_empty())
    .collect();
    v.sort();
    v
}

fn names(list: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = list.iter().map(|s| s.to_string()).collect();
    v.sort();
    v
}

/// The `detail` of every event of `ty` for the task, read through
/// `thoth events --task TASK --json`.
fn cli_events(f: &Fixture, task_id: &str, ty: &str) -> Vec<Value> {
    f.events_json(task_id)
        .into_iter()
        .filter(|e| e["type"] == ty)
        .map(|e| e["detail"].clone())
        .collect()
}

/// `worktrees prune --json` that must exit 0 with `ok` true.
fn prune_ok(f: &Fixture) -> Value {
    let (code, p) = prune_json(f);
    assert_eq!(code, 0, "{p}");
    assert_eq!(p["ok"], true, "{p}");
    p
}

fn merge_no_ff(f: &Fixture, branch: &str) {
    common::git(&f.repo_path(), &["merge", "--no-ff", "-m", "merge the task", branch]);
}

fn commit_file(dir: &Path, name: &str, contents: &str, message: &str) -> String {
    std::fs::write(dir.join(name), contents).unwrap();
    common::git(dir, &["add", name]);
    common::git(dir, &["commit", "-m", message]);
    common::git(dir, &["rev-parse", "HEAD"])
}

fn is_ancestor(f: &Fixture, a: &str, b: &str) -> bool {
    std::process::Command::new("git")
        .args(["merge-base", "--is-ancestor", a, b])
        .current_dir(f.repo_path())
        .status()
        .unwrap()
        .success()
}

/// A passed task merged with `--no-ff` is deleted with `git branch -d`, even
/// while the main tree has another branch checked out; a second prune finds
/// nothing and writes nothing.
#[test]
fn a_passed_task_merged_with_no_ff_loses_its_branch_and_a_second_prune_is_quiet() {
    let f = common::fixture();
    setup(&f);
    let t = passed(&f);
    assert!(!t.wt.exists(), "review record PASS removes the worker worktree");
    merge_no_ff(&f, &t.branch);
    // The main tree moves to a branch that does not contain the task, so
    // only a check against the base (not HEAD) can call it merged.
    common::git(&f.repo_path(), &["checkout", "-b", "side", "HEAD~1"]);
    assert!(!is_ancestor(&f, &t.tip, "HEAD"));

    let p = prune_ok(&f);
    assert_eq!(
        p["removed_branches"],
        serde_json::json!([{
            "task_id": t.task_id, "branch": t.branch, "tip": t.tip, "merged_by": "ancestor"
        }]),
        "{p}"
    );
    assert_eq!(p["kept_branches"], serde_json::json!([]), "{p}");
    assert_eq!(branches(&f), names(&["main", "side"]));
    let deleted = cli_events(&f, &t.task_id, "branch_deleted");
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(deleted[0]["task_id"], t.task_id.as_str());
    assert_eq!(deleted[0]["branch"], t.branch.as_str());
    assert_eq!(deleted[0]["tip"], t.tip.as_str());
    assert_eq!(deleted[0]["merged_by"], "ancestor");
    assert_eq!(deleted[0]["source"], "worktrees_prune");

    let before = f.events_of(&t.task_id).len();
    let again = prune_ok(&f);
    assert_eq!(again["removed_branches"], serde_json::json!([]), "{again}");
    assert_eq!(again["kept_branches"], serde_json::json!([]), "{again}");
    assert_eq!(f.events_of(&t.task_id).len(), before);
    // the existing keys keep their meaning
    for key in ["ok", "removed", "kept", "tasks_checked"] {
        assert!(again.get(key).is_some(), "{key}: {again}");
    }
}

/// Cherry-picked onto an independently advanced base: a different commit,
/// the tip is no ancestor, and `git cherry` marks every commit as applied.
#[test]
fn a_passed_task_cherry_picked_onto_an_advanced_base_loses_its_branch() {
    let f = common::fixture();
    setup(&f);
    let t = passed(&f);
    commit_file(&f.repo_path(), "advance.txt", "base moved on\n", "advance the base");
    common::git(&f.repo_path(), &["cherry-pick", &t.tip]);
    let picked = common::git(&f.repo_path(), &["rev-parse", "HEAD"]);
    assert_ne!(picked, t.tip);
    assert!(!is_ancestor(&f, &t.tip, "main"));

    let p = prune_ok(&f);
    assert_eq!(
        p["removed_branches"],
        serde_json::json!([{
            "task_id": t.task_id, "branch": t.branch, "tip": t.tip, "merged_by": "cherry"
        }]),
        "{p}"
    );
    assert_eq!(branches(&f), names(&["main"]));
    let deleted = cli_events(&f, &t.task_id, "branch_deleted");
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(deleted[0]["merged_by"], "cherry");
}

/// A commit the base does not have keeps the branch (once, however many
/// prunes see it); merging it afterwards lets the next prune delete it.
#[test]
fn an_extra_commit_absent_from_the_base_keeps_the_branch_until_it_is_merged() {
    let f = common::fixture();
    setup(&f);
    let t = passed(&f);
    common::git(&f.repo_path(), &["cherry-pick", &t.tip]);
    common::git(&f.repo_path(), &["checkout", &t.branch]);
    commit_file(&f.repo_path(), "extra.txt", "only on the branch\n", "extra work");
    common::git(&f.repo_path(), &["checkout", "main"]);

    let p = prune_ok(&f);
    let entry = serde_json::json!({
        "task_id": t.task_id, "branch": t.branch, "reason": "branch_unmerged"
    });
    assert_eq!(p["kept_branches"], serde_json::json!([entry.clone()]), "{p}");
    assert_eq!(p["removed_branches"], serde_json::json!([]), "{p}");
    assert_eq!(branches(&f), names(&["main", &t.branch]));
    let kept = cli_events(&f, &t.task_id, "branch_kept");
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["reason"], "branch_unmerged");
    assert_eq!(kept[0]["branch"], t.branch.as_str());
    assert!(kept[0].get("detail").is_none(), "{kept:?}");

    let before = f.events_of(&t.task_id).len();
    let again = prune_ok(&f);
    assert_eq!(again["kept_branches"], serde_json::json!([entry]), "{again}");
    assert_eq!(f.events_of(&t.task_id).len(), before, "deduplicated");

    merge_no_ff(&f, &t.branch);
    let p = prune_ok(&f);
    assert_eq!(p["removed_branches"][0]["merged_by"], "ancestor", "{p}");
    assert_eq!(p["kept_branches"], serde_json::json!([]), "{p}");
    assert_eq!(branches(&f), names(&["main"]));
    assert_eq!(cli_events(&f, &t.task_id, "branch_deleted").len(), 1);
}

/// A cancelled task's unmerged work stays; when its base branch goes away
/// the reason changes, and that is a new event.
#[test]
fn a_cancelled_unmerged_task_keeps_its_branch_and_a_new_reason_is_a_new_event() {
    let f = common::fixture();
    setup(&f);
    let t = implemented(&f);
    assert_eq!(record(&f, &t.task_id, REVIEW_REJECT), "reject");
    f.cmd()
        .args(["task", "close", &t.task_id, "--reason", "abandoned", "--json"])
        .assert()
        .success();
    assert_eq!(f.task_status(&t.task_id), "cancelled");
    assert!(!t.wt.exists(), "task close removes the clean worktree");

    let p = prune_ok(&f);
    assert_eq!(
        p["kept_branches"],
        serde_json::json!([{
            "task_id": t.task_id, "branch": t.branch, "reason": "branch_unmerged"
        }]),
        "{p}"
    );
    assert_eq!(branches(&f), names(&["main", &t.branch]));
    assert_eq!(cli_events(&f, &t.task_id, "branch_kept").len(), 1);

    common::git(&f.repo_path(), &["branch", "-m", "main", "trunk"]);
    let p = prune_ok(&f);
    assert_eq!(p["kept_branches"][0]["reason"], "no_base_branch", "{p}");
    let kept = cli_events(&f, &t.task_id, "branch_kept");
    assert_eq!(kept.len(), 2, "{kept:?}");
    assert_eq!(kept[1]["reason"], "no_base_branch");
    assert_eq!(branches(&f), names(&["trunk", &t.branch]));
}

/// A task created on a detached HEAD has no base branch, so even merged
/// work keeps its branch.
#[test]
fn a_task_with_a_detached_base_keeps_its_branch() {
    let f = common::fixture();
    setup(&f);
    common::git(&f.repo_path(), &["checkout", "--detach"]);
    let t = passed(&f);
    let show = f
        .cmd()
        .args(["task", "show", &t.task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let show: Value = serde_json::from_slice(&show).unwrap();
    assert_eq!(show["base_branch"], Value::Null, "{show}");
    common::git(&f.repo_path(), &["checkout", "main"]);
    merge_no_ff(&f, &t.branch);

    let p = prune_ok(&f);
    assert_eq!(
        p["kept_branches"],
        serde_json::json!([{
            "task_id": t.task_id, "branch": t.branch, "reason": "no_base_branch"
        }]),
        "{p}"
    );
    assert_eq!(branches(&f), names(&["main", &t.branch]));
}

/// A dirty worktree that is kept still has the branch checked out, so the
/// branch stays even though it is merged; once the worktree is clean the
/// next prune removes both.
#[test]
fn a_dirty_retained_worktree_keeps_its_merged_branch_checked_out() {
    let f = common::fixture();
    setup(&f);
    let t = implemented(&f);
    std::fs::write(t.wt.join("sentinel.txt"), "hold\n").unwrap();
    assert_eq!(record(&f, &t.task_id, REVIEW_PASS), "pass");
    assert!(t.wt.is_dir());
    merge_no_ff(&f, &t.branch);

    let p = prune_ok(&f);
    let kept = p["kept"].as_array().unwrap();
    assert_eq!(kept.len(), 1, "{p}");
    assert_eq!(kept[0]["reason"], "uncommitted_changes");
    assert_eq!(
        p["kept_branches"],
        serde_json::json!([{
            "task_id": t.task_id, "branch": t.branch, "reason": "branch_checked_out"
        }]),
        "{p}"
    );
    assert_eq!(branches(&f), names(&["main", &t.branch]));
    let before = f.events_of(&t.task_id).len();
    prune_ok(&f);
    assert_eq!(f.events_of(&t.task_id).len(), before, "deduplicated");

    std::fs::remove_file(t.wt.join("sentinel.txt")).unwrap();
    let p = prune_ok(&f);
    assert_eq!(p["removed"].as_array().unwrap().len(), 1, "{p}");
    assert_eq!(p["removed_branches"][0]["branch"], t.branch.as_str(), "{p}");
    assert_eq!(branches(&f), names(&["main"]));
    assert!(!t.wt.exists());
}

/// A merge commit on the branch whose resolution adds a change the base
/// does not have: the tip is no ancestor, and the merge keeps the branch
/// although every ordinary commit was cherry-picked.
#[test]
fn a_merge_with_its_own_resolution_change_keeps_the_branch() {
    let f = common::fixture();
    setup(&f);
    let t = passed(&f);
    let repo = f.repo_path();
    commit_file(&repo, "a.txt", "one\nchanged on main\n", "main moves on");
    common::git(&repo, &["checkout", &t.branch]);
    common::git(&repo, &["merge", "--no-ff", "--no-commit", "main"]);
    std::fs::write(repo.join("resolution.txt"), "only in the merge\n").unwrap();
    common::git(&repo, &["add", "resolution.txt"]);
    common::git(&repo, &["commit", "-m", "merge main, with a resolution change"]);
    let merge_tip = common::git(&repo, &["rev-parse", "HEAD"]);
    common::git(&repo, &["checkout", "main"]);
    common::git(&repo, &["cherry-pick", &t.tip]);
    assert!(!is_ancestor(&f, &merge_tip, "main"));
    let absent = std::process::Command::new("git")
        .args(["cat-file", "-e", "main:resolution.txt"])
        .current_dir(&repo)
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!absent.success(), "precondition: the base lacks the merge's change");

    let p = prune_ok(&f);
    assert_eq!(
        p["kept_branches"],
        serde_json::json!([{
            "task_id": t.task_id, "branch": t.branch, "reason": "branch_unmerged"
        }]),
        "{p}"
    );
    assert_eq!(branches(&f), names(&["main", &t.branch]));
}

/// Two attempts whose worktrees are both gone: merging the second makes
/// both branches ancestors of the base, and one prune deletes both.
#[test]
fn every_attempt_branch_of_a_task_is_considered_after_its_worktrees_are_gone() {
    let f = common::fixture();
    setup(&f);
    let t = implemented(&f);
    assert_eq!(record(&f, &t.task_id, REVIEW_REJECT), "reject");
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &t.task_id)
        .env("FAKE_WRITE_FILE", "second.txt")
        .args(["dispatch", &t.task_id, "--profile", "claude-b", "--json"])
        .args(["--model", "sonnet", "--effort", "medium"]);
    let d: Value = serde_json::from_slice(&f.run_ok(c, "dispatch")).unwrap();
    assert_eq!(d["attempt"], 2, "{d}");
    let (_, status) = f.wait_for(&t.task_id, "180s");
    assert_eq!(status, "in_review");
    let wt2 = path_of(&d["worktree"]);
    let branch2 = common::git(&wt2, &["symbolic-ref", "--short", "HEAD"]);
    assert_eq!(branch2, format!("thoth/{}/a2", t.task_id));
    let tip2 = common::git(&wt2, &["rev-parse", "HEAD"]);
    assert_ne!(tip2, t.tip);
    assert_eq!(record(&f, &t.task_id, REVIEW_PASS), "pass");
    assert!(!t.wt.exists() && !wt2.exists(), "both worktrees are gone");
    merge_no_ff(&f, &branch2);

    let p = prune_ok(&f);
    let mut removed: Vec<(String, String, String)> = p["removed_branches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            assert_eq!(e["task_id"], t.task_id.as_str());
            (
                e["branch"].as_str().unwrap().to_string(),
                e["tip"].as_str().unwrap().to_string(),
                e["merged_by"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    removed.sort();
    assert_eq!(
        removed,
        vec![
            (t.branch.clone(), t.tip.clone(), "ancestor".to_string()),
            (branch2.clone(), tip2.clone(), "ancestor".to_string()),
        ],
        "{p}"
    );
    assert_eq!(branches(&f), names(&["main"]));
    assert_eq!(cli_events(&f, &t.task_id, "branch_deleted").len(), 2);
}

/// The next `task create` runs the same cleanup: its `--json` keys stay the
/// same, and the deletion is in the finished task's events.
#[test]
fn the_next_task_create_deletes_a_merged_branch_and_keeps_its_json_keys() {
    let f = common::fixture();
    setup(&f);
    let t = passed(&f);
    merge_no_ff(&f, &t.branch);
    let spec = f.home_path().join("next-spec.json");
    std::fs::write(&spec, Fixture::SPEC).unwrap();
    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&spec)
        .arg("--json")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(|s| s.as_str()).collect();
    keys.sort();
    assert_eq!(keys, vec!["base_branch", "base_commit", "ok", "status", "task_id", "title", "warnings"]);
    assert_eq!(v["warnings"], serde_json::json!([]));
    assert_eq!(branches(&f), names(&["main"]));
    let deleted = cli_events(&f, &t.task_id, "branch_deleted");
    assert_eq!(deleted.len(), 1, "{deleted:?}");
    assert_eq!(deleted[0]["branch"], t.branch.as_str());
    assert_eq!(deleted[0]["tip"], t.tip.as_str());
    assert_eq!(deleted[0]["merged_by"], "ancestor");
    assert_eq!(deleted[0]["source"], "task_create");
}

/// A branch git refuses to delete (a held ref lock) is kept with the reason
/// and git's message, without changing `ok` or the exit code; once the lock
/// is gone the next prune deletes it.
#[test]
fn a_branch_git_cannot_delete_is_kept_with_the_error_and_ok_stays_true() {
    let f = common::fixture();
    setup(&f);
    let t = passed(&f);
    merge_no_ff(&f, &t.branch);
    let lock = f
        .repo_path()
        .join(".git")
        .join("refs")
        .join("heads")
        .join("thoth")
        .join(&t.task_id)
        .join("a1.lock");
    assert!(lock.parent().unwrap().is_dir(), "the ref is a loose file");
    std::fs::write(&lock, "").unwrap();

    let p = prune_ok(&f);
    let kept = p["kept_branches"].as_array().unwrap();
    assert_eq!(kept.len(), 1, "{p}");
    assert_eq!(kept[0]["task_id"], t.task_id.as_str());
    assert_eq!(kept[0]["branch"], t.branch.as_str());
    assert_eq!(kept[0]["reason"], "branch_delete_failed");
    let detail = kept[0]["detail"].as_str().unwrap();
    assert!(detail.contains("git branch -d") && detail.contains("lock"), "{detail}");
    assert_eq!(p["removed_branches"], serde_json::json!([]));
    assert_eq!(branches(&f), names(&["main", &t.branch]));
    let events = cli_events(&f, &t.task_id, "branch_kept");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["reason"], "branch_delete_failed");
    assert!(events[0]["detail"].as_str().unwrap().contains("lock"), "{events:?}");

    let again = prune_ok(&f);
    assert_eq!(again["kept_branches"][0]["reason"], "branch_delete_failed");
    assert_eq!(cli_events(&f, &t.task_id, "branch_kept").len(), 1, "deduplicated");

    std::fs::remove_file(&lock).unwrap();
    let p = prune_ok(&f);
    assert_eq!(p["removed_branches"][0]["branch"], t.branch.as_str(), "{p}");
    assert_eq!(branches(&f), names(&["main"]));
}

/// A directory link at `link` to `target`: a junction on Windows (no
/// privilege needed), a symlink elsewhere.
fn dir_link(target: &Path, link: &Path) {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("cmd")
            .arg("/C")
            .arg("mklink")
            .arg("/J")
            .arg(link)
            .arg(target)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "mklink /J: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
}

/// The 2026-10-14 regression: an ignored `node_modules` link inside a
/// finished task's worktree. `worktrees prune` removes the worktree and the
/// directory the link pointed to is untouched.
#[test]
fn prune_removes_a_worktree_holding_a_node_modules_link_without_touching_its_target() {
    let f = common::fixture();
    setup(&f);
    let external = tempfile::tempdir().unwrap();
    let marker = external.path().join("marker.txt");
    std::fs::write(&marker, "main tree's node_modules\n").unwrap();
    std::fs::create_dir_all(external.path().join("pkg")).unwrap();
    std::fs::write(external.path().join("pkg").join("index.js"), "module.exports = 1;\n").unwrap();

    let t = implemented(&f);
    std::fs::write(t.wt.join("sentinel.txt"), "hold\n").unwrap();
    assert_eq!(record(&f, &t.task_id, REVIEW_PASS), "pass");
    assert!(t.wt.is_dir(), "the sentinel holds the worktree");
    let kept = cli_events(&f, &t.task_id, "worktree_kept");
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["reason"], "uncommitted_changes");

    std::fs::remove_file(t.wt.join("sentinel.txt")).unwrap();
    let exclude = f.repo_path().join(".git").join("info").join("exclude");
    let mut text = std::fs::read_to_string(&exclude).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str("node_modules\n");
    std::fs::write(&exclude, text).unwrap();
    dir_link(external.path(), &t.wt.join("node_modules"));
    let porcelain = common::git(&t.wt, &["status", "--porcelain"]);
    let non_agent: Vec<&str> = porcelain
        .lines()
        .filter(|l| {
            let p = l.get(3..).unwrap_or("");
            !(p.starts_with(".agent/") || p.starts_with(".agents/"))
        })
        .collect();
    assert!(non_agent.is_empty(), "git must see no non-agent change: {porcelain}");

    let p = prune_ok(&f);
    let removed = p["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 1, "{p}");
    assert_eq!(removed[0]["path"], t.wt.display().to_string());
    assert_eq!(p["kept"], serde_json::json!([]), "{p}");
    assert!(!t.wt.exists(), "{} is still there", t.wt.display());
    assert_eq!(std::fs::read_to_string(&marker).unwrap(), "main tree's node_modules\n");
    assert_eq!(
        std::fs::read_to_string(external.path().join("pkg").join("index.js")).unwrap(),
        "module.exports = 1;\n"
    );
}
