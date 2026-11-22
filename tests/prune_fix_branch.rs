//! SMALL-11 (1): the sweeps of `worktrees prune` and `task create` delete a
//! merged reviewer fix branch `thoth/TASK/aN-fix` the way they delete
//! `thoth/TASK/aN`, and no other name under `thoth/TASK/`.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::{Path, PathBuf};

const REVIEW_PASS: &str = r#"{"verdict":"PASS","findings":{"criteria":[{"id":"AC1","verdict":"met","evidence":["E001"]},{"id":"AC2","verdict":"met","evidence":["E001"]}],"extra_findings":[]}}"#;

/// `thoth ARGS` that must exit 0, parsed as JSON.
fn json(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_slice(&out).unwrap_or_else(|e| panic!("{args:?}: not JSON ({e})"))
}

struct Passed {
    task_id: String,
    branch: String,
    fix: String,
    tip: String,
}

/// A passed task (fake-claude `hooks` scenario, review record PASS) whose
/// branch `thoth/TASK/a1` holds the candidate commit.
fn passed(f: &Fixture) -> Passed {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let (_, status) = f.wait_for(&task_id, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task_id));
    let st: Value = json(f, &["status", &task_id, "--json"]);
    let wt = PathBuf::from(st["runs"][0]["worktree"].as_str().unwrap());
    let tip = common::git(&wt, &["rev-parse", "HEAD"]);

    let file = f.home_path().join("review-record.json");
    std::fs::write(&file, REVIEW_PASS).unwrap();
    let mut c = f.cmd();
    c.args(["review", "record", &task_id, "-f"]).arg(&file).arg("--json");
    let out = f.run_ok(c, "review record");
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["task_status"], "pass", "{v}");
    Passed {
        branch: format!("thoth/{task_id}/a1"),
        fix: format!("thoth/{task_id}/a1-fix"),
        task_id,
        tip,
    }
}

fn commit_file(dir: &Path, name: &str, contents: &str, message: &str) {
    std::fs::write(dir.join(name), contents).unwrap();
    common::git(dir, &["add", name]);
    common::git(dir, &["commit", "-m", message]);
}

/// `branch` made with git from the a1 tip plus one commit, as the reviewer
/// fix flow leaves it; the main tree is back on main afterwards.
fn make_fix_branch(f: &Fixture, t: &Passed, branch: &str) {
    let repo = f.repo_path();
    common::git(&repo, &["checkout", "-b", branch, &t.tip]);
    commit_file(&repo, "fix.txt", "a small fix\n", "fix");
    common::git(&repo, &["checkout", "main"]);
}

fn merge_no_ff(f: &Fixture, branch: &str) {
    common::git(&f.repo_path(), &["merge", "--no-ff", "-m", "merge", branch]);
}

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

fn prune(f: &Fixture) -> Value {
    json(f, &["worktrees", "prune", "--json"])
}

fn events(f: &Fixture, task_id: &str, ty: &str) -> Vec<Value> {
    f.events_json(task_id)
        .into_iter()
        .filter(|e| e["type"] == ty)
        .map(|e| e["detail"].clone())
        .collect()
}

fn branch_names(list: &Value) -> Vec<String> {
    let mut v: Vec<String> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["branch"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

#[test]
fn a_merged_fix_branch_is_deleted_with_its_task_branch() {
    let f = common::fixture();
    let t = passed(&f);
    make_fix_branch(&f, &t, &t.fix);
    merge_no_ff(&f, &t.fix);

    let p = prune(&f);
    let mut want = vec![t.branch.clone(), t.fix.clone()];
    want.sort();
    assert_eq!(branch_names(&p["removed_branches"]), want, "{p}");
    for b in p["removed_branches"].as_array().unwrap() {
        assert_eq!(b["task_id"], t.task_id.as_str(), "{p}");
        assert_eq!(b["merged_by"], "ancestor", "{p}");
        assert!(b["tip"].as_str().is_some_and(|s| s.len() >= 7), "{p}");
    }
    assert_eq!(p["kept_branches"], serde_json::json!([]), "{p}");
    let deleted = events(&f, &t.task_id, "branch_deleted");
    assert_eq!(deleted.len(), 2, "{deleted:?}");
    assert!(deleted.iter().all(|d| d["source"] == "worktrees_prune"), "{deleted:?}");
    assert_eq!(branches(&f), vec!["main".to_string()]);
}

#[test]
fn an_unmerged_fix_branch_is_kept_once_and_deleted_when_merged() {
    let f = common::fixture();
    let t = passed(&f);
    make_fix_branch(&f, &t, &t.fix);
    // a1 is in main (its commits are applied), a1-fix has a commit main lacks
    merge_no_ff(&f, &t.branch);

    let p = prune(&f);
    assert_eq!(branch_names(&p["removed_branches"]), vec![t.branch.clone()], "{p}");
    assert_eq!(
        p["kept_branches"],
        serde_json::json!([{
            "task_id": t.task_id, "branch": t.fix, "reason": "branch_unmerged"
        }]),
        "{p}"
    );
    assert!(branches(&f).contains(&t.fix));
    let kept = events(&f, &t.task_id, "branch_kept");
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(kept[0]["branch"], t.fix.as_str());
    assert_eq!(kept[0]["reason"], "branch_unmerged");

    // deduplicated as for aN
    let again = prune(&f);
    assert_eq!(branch_names(&again["kept_branches"]), vec![t.fix.clone()], "{again}");
    assert_eq!(events(&f, &t.task_id, "branch_kept").len(), 1);

    merge_no_ff(&f, &t.fix);
    let p = prune(&f);
    assert_eq!(branch_names(&p["removed_branches"]), vec![t.fix.clone()], "{p}");
    assert_eq!(p["removed_branches"][0]["merged_by"], "ancestor", "{p}");
    assert_eq!(p["kept_branches"], serde_json::json!([]), "{p}");
    assert_eq!(branches(&f), vec!["main".to_string()]);
}

#[test]
fn other_names_under_the_task_prefix_are_neither_deleted_nor_listed() {
    let f = common::fixture();
    let t = passed(&f);
    let other_a = format!("thoth/{}/a1-fixed", t.task_id);
    let other_b = format!("thoth/{}/a1-fix2", t.task_id);
    // both are merged into main, so only their names keep them
    common::git(&f.repo_path(), &["branch", &other_a, &t.tip]);
    common::git(&f.repo_path(), &["branch", &other_b, &t.tip]);
    merge_no_ff(&f, &t.branch);

    let p = prune(&f);
    assert_eq!(branch_names(&p["removed_branches"]), vec![t.branch.clone()], "{p}");
    assert_eq!(p["kept_branches"], serde_json::json!([]), "{p}");
    let left = branches(&f);
    assert!(left.contains(&other_a) && left.contains(&other_b), "{left:?}");
    let all = f.events_json(&t.task_id);
    assert!(
        !all.iter().any(|e| {
            let d = e["detail"].to_string();
            (e["type"] == "branch_kept" || e["type"] == "branch_deleted")
                && (d.contains("a1-fixed") || d.contains("a1-fix2"))
        }),
        "{all:?}"
    );
}

#[test]
fn task_create_deletes_a_merged_fix_branch() {
    let f = common::fixture();
    let t = passed(&f);
    make_fix_branch(&f, &t, &t.fix);
    merge_no_ff(&f, &t.fix);

    f.create_task(Fixture::SPEC);
    assert_eq!(branches(&f), vec!["main".to_string()]);
    let deleted = events(&f, &t.task_id, "branch_deleted");
    assert_eq!(deleted.len(), 2, "{deleted:?}");
    let fix = deleted
        .iter()
        .find(|d| d["branch"] == t.fix.as_str())
        .unwrap_or_else(|| panic!("no branch_deleted for the fix branch: {deleted:?}"));
    assert_eq!(fix["source"], "task_create");
    assert_eq!(fix["merged_by"], "ancestor");
}
