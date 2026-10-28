//! WT-1: Thoth removes the worktrees of finished (pass / cancelled) tasks
//! itself, keeps every branch, commit and piece of evidence, and keeps (and
//! reports) any worktree that is not safe to drop.

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
    let events = events_detail(&f, &task_id, "worktree_kept");
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0]["source"], "task_close");

    let (code, p) = prune_json(&f);
    assert_eq!(code, 0, "{p}");
    assert_eq!(p["ok"], true);
    assert_eq!(p["kept"].as_array().unwrap().len(), 1, "{p}");
    assert_eq!(p["removed"], serde_json::json!([]));
    assert_eq!(p["tasks_checked"], 1);
    assert_eq!(events_detail(&f, &task_id, "worktree_kept").len(), 1, "deduplicated");

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
    assert!(branch_exists(&f, &format!("thoth/{task_id}/a1")));
    let removed = events_detail(&f, &task_id, "worktree_removed");
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
