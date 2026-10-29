//! P7-2: protected files, end to end with the fake worker.

mod common;

use common::Fixture;
use serde_json::{json, Value};

fn json_of(out: Vec<u8>) -> Value {
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn initialised() -> Fixture {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f
}

fn list(f: &Fixture) -> Value {
    json_of(
        f.cmd()
            .args(["checks", "list", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    )
}

/// The raw `meta` row, or `None` before anything was stored.
fn stored_patterns(f: &Fixture) -> Option<String> {
    use rusqlite::OptionalExtension;
    f.state_conn()
        .query_row("SELECT value FROM meta WHERE key = 'protected_patterns'", [], |r| r.get(0))
        .optional()
        .unwrap()
}

/// `dispatch` of the `hooks` scenario with extra environment for the worker.
fn dispatch_hooks(f: &Fixture, task_id: &str, env: &[(&str, &str)]) -> Value {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "hooks").env("FAKE_TASK_ID", task_id);
    for (k, v) in env {
        c.env(k, v);
    }
    c.args(["dispatch", task_id, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    json_of(f.run_ok(c, "dispatch"))
}

fn prompt_of(f: &Fixture, run_id: &str) -> String {
    let run_dir: String = f
        .state_conn()
        .query_row("SELECT run_dir FROM runs WHERE run_id = ?1", [run_id], |r| r.get(0))
        .unwrap();
    std::fs::read_to_string(std::path::Path::new(&run_dir).join("prompt.txt")).unwrap()
}

/// LEAN-1: a run's prompt.txt, found through `status TASK --json`'s
/// `runs[].run_dir`.
fn prompt_via_status(f: &Fixture, task_id: &str, run_id: &str) -> String {
    let run = f.run_of(task_id, run_id);
    std::fs::read_to_string(std::path::Path::new(run["run_dir"].as_str().unwrap()).join("prompt.txt"))
        .unwrap()
}

/// LEAN-1: a run's events of one type, from `events --task TASK --json`.
fn run_events(f: &Fixture, task_id: &str, run_id: &str, ty: &str) -> Vec<Value> {
    f.events_json(task_id)
        .into_iter()
        .filter(|e| e["run_id"] == run_id && e["type"] == ty)
        .collect()
}

/// The `protected_changes` of a run's diff evidence.
fn diff_protected(f: &Fixture, task_id: &str, run_id: &str) -> Value {
    f.evidence_rows(task_id)
        .into_iter()
        .rfind(|e| e["kind"] == "diff" && e["run_id"] == run_id)
        .expect("a diff evidence row")["detail"]["protected_changes"]
        .clone()
}

/// AC2: protect and unprotect keep a unique, sorted JSON array in `meta`, and
/// `checks list` shows it in both formats.
#[test]
fn protect_and_unprotect_keep_a_sorted_unique_list_that_checks_list_shows() {
    let f = initialised();
    assert_eq!(list(&f)["protected_patterns"], json!([]));

    for p in ["fixtures/", "*.snap", "golden_*", "*.snap"] {
        f.cmd().args(["checks", "protect", p]).assert().success();
    }
    let v = json_of(
        f.cmd()
            .args(["checks", "protect", "Data/Expected.json", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["ok"], true);
    let all = json!(["*.snap", "data/expected.json", "fixtures/", "golden_*"]);
    assert_eq!(v["protected_patterns"], all);
    assert_eq!(list(&f)["protected_patterns"], all);
    assert_eq!(
        stored_patterns(&f).as_deref(),
        Some(r#"["*.snap","data/expected.json","fixtures/","golden_*"]"#)
    );

    let human = String::from_utf8(
        f.cmd().args(["checks", "list"]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    assert!(
        human.contains("protected patterns: *.snap, data/expected.json, fixtures/, golden_*"),
        "{human}"
    );

    let v = json_of(
        f.cmd()
            .args(["checks", "unprotect", "golden_*", "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    assert_eq!(v["protected_patterns"], json!(["*.snap", "data/expected.json", "fixtures/"]));
    assert_eq!(list(&f)["protected_patterns"], json!(["*.snap", "data/expected.json", "fixtures/"]));
    // a pattern that is not in the list is an error
    f.cmd().args(["checks", "unprotect", "golden_*"]).assert().code(1);
}

/// AC2: a pattern outside the three shapes exits 1 and stores nothing.
#[test]
fn protect_refuses_a_pattern_outside_the_three_shapes() {
    let f = initialised();
    for bad in ["a*b", "**/x"] {
        let err = f
            .cmd()
            .args(["checks", "protect", bad])
            .assert()
            .code(1)
            .get_output()
            .stderr
            .clone();
        assert!(String::from_utf8(err).unwrap().contains(bad), "{bad}");
    }
    assert_eq!(stored_patterns(&f), None);
    assert_eq!(list(&f)["protected_patterns"], json!([]));
}

/// AC2: a run may not change the list.
#[test]
fn a_run_environment_cannot_protect_or_unprotect() {
    let f = initialised();
    f.cmd().args(["checks", "protect", "*.snap"]).assert().success();
    for args in [
        vec!["checks", "protect", "fixtures/"],
        vec!["checks", "unprotect", "*.snap"],
    ] {
        let err = f
            .cmd()
            .env("THOTH_RUN_ID", "01SOMERUN")
            .args(&args)
            .assert()
            .code(1)
            .get_output()
            .stderr
            .clone();
        assert!(String::from_utf8(err).unwrap().contains("THOTH_RUN_ID"), "{args:?}");
    }
    assert_eq!(list(&f)["protected_patterns"], json!(["*.snap"]));
}

/// AC6: the plain `hooks` worker only adds `tests/added_test.rs`, so nothing
/// is protected, the result is accepted, and the packet says `(none)`.
#[test]
fn an_added_test_file_is_not_a_protected_change() {
    let f = initialised();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let d = dispatch_hooks(&f, &task_id, &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (_, status) = f.wait_for(&task_id, "180s");
    assert_eq!(status, "in_review");
    assert_eq!(diff_protected(&f, &task_id, &run_id), json!([]));

    let md = String::from_utf8(
        f.cmd().args(["review", "packet", &task_id]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    let at = md.find("## Protected files changed").expect("the section");
    assert!(at < md.find("## Diffs").unwrap(), "before the diffs:\n{md}");
    let section = &md[at..md.find("## Diffs").unwrap()];
    assert_eq!(section.matches("- (none)\n").count(), 2, "no changes, no claims:\n{section}");
}

/// AC2 + AC3: a file matching one of the repo's extra patterns is a
/// protected test file. LEAN-1: modifying it undeclared is listed, not
/// refused (rule 6 is retired).
#[test]
fn modifying_a_file_under_an_extra_pattern_is_listed_without_a_declaration() {
    let f = initialised();
    f.add_profile("claude-b");
    f.cmd().args(["checks", "protect", "a.txt"]).assert().success();
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let d = dispatch_hooks(&f, &task_id, &[("FAKE_WRITE_FILE", "a.txt")]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (_, status) = f.wait_for(&task_id, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task_id));

    assert_eq!(
        diff_protected(&f, &task_id, &run_id),
        json!([{"path": "a.txt", "status": "modified", "kind": "test"}])
    );
    assert!(run_events(&f, &task_id, &run_id, "result_invalid").is_empty());
    assert_eq!(run_events(&f, &task_id, &run_id, "result_ingested").len(), 1);
}

/// AC9, AC5, AC6 end to end. The catalogue holds `cargo test`; the worker adds
/// `cargo.bat` and (optionally, LEAN-1) declares it; the delivery is
/// accepted; the packet and the Claude reviewer's prompt both list the file.
#[test]
fn an_added_file_named_like_a_check_word_is_listed_and_approved() {
    let f = initialised();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    f.cmd()
        .args(["checks", "add", "test", "--cmd", "cargo test", "--json"])
        .assert()
        .success();
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);

    // --- the run: cargo.bat added, with an optional declaration ---------------
    let d = dispatch_hooks(
        &f,
        &task_id,
        &[
            ("FAKE_WRITE_FILE", "cargo.bat"),
            ("FAKE_PROTECTED_CLAIM", "cargo.bat: a wrapper the task asked for"),
        ],
    );
    let first_run = d["run_id"].as_str().unwrap().to_string();
    let (_, status) = f.wait_for(&task_id, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task_id));

    assert_eq!(
        diff_protected(&f, &task_id, &first_run),
        json!([{"path": "cargo.bat", "status": "added", "kind": "check_word"}]),
        "tests/added_test.rs is added too, and is not listed"
    );
    assert!(run_events(&f, &task_id, &first_run, "result_invalid").is_empty());

    // AC5: the prompt told the worker about protected files and the words
    let first_prompt = prompt_via_status(&f, &task_id, &first_run);
    let section = &first_prompt[first_prompt.find("\n## Protected files\n").expect("section")..];
    assert!(section.contains("Check words: cargo, test\n"), "{section}");
    assert!(!first_prompt.contains("was not accepted"));

    // --- AC6: the packet ------------------------------------------------------
    let md = String::from_utf8(
        f.cmd().args(["review", "packet", &task_id]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    let at = md.find("## Protected files changed").expect("the section");
    let diffs_at = md.find("## Diffs").unwrap();
    assert!(at < diffs_at, "before the diffs:\n{md}");
    let section = &md[at..diffs_at];
    assert!(section.contains("cargo.bat (added, check_word)"), "{section}");
    assert!(section.contains("- cargo.bat: a wrapper the task asked for\n"), "{section}");
    assert!(!section.contains("(none)"), "{section}");

    let v = json_of(
        f.cmd()
            .args(["review", "packet", &task_id, "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone(),
    );
    let listed = v["protected_changes"].as_array().unwrap();
    assert_eq!(listed.len(), 1, "one per diff evidence: {listed:?}");
    for (entry, run) in listed.iter().zip([&first_run]) {
        let mut keys: Vec<&str> = entry.as_object().unwrap().keys().map(|s| s.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["code", "kind", "path", "run_id", "status"]);
        assert_eq!(entry["run_id"], run.as_str());
        assert_eq!(entry["path"], "cargo.bat");
        assert_eq!(entry["status"], "added");
        assert_eq!(entry["kind"], "check_word");
        assert!(entry["code"].as_str().unwrap().starts_with('E'));
    }

    // --- AC6: the Claude reviewer's prompt -------------------------------------
    // LESS-5: review start waits until the run's check-word file is approved.
    f.cmd()
        .args(["checks", "approve", "--run", &first_run, "--reason", "test approval", "--json"])
        .assert()
        .success();
    let r = f.review_start(&task_id, "claude-a", "review_pass", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s", "--json"])
        .assert()
        .success();
    let review_prompt = prompt_via_status(&f, &task_id, &reviewer_run);
    let list_at = review_prompt
        .find("Protected files this candidate changed")
        .expect(&review_prompt);
    assert!(review_prompt.contains("- cargo.bat (added, check_word)"), "{review_prompt}");
    assert!(review_prompt.contains("decide whether the change weakened a check"));
    assert!(list_at > review_prompt.find("1. Read the candidate diff").unwrap());
    assert!(list_at < review_prompt.find("2. Judge every acceptance criterion").unwrap());
    // the reviewer's verdict is not automated: this one passes the task
    assert_eq!(f.wait_for_task_status(&task_id, &["pass", "unverified", "rejected"], 60), "pass");
}

/// AC11 (d): the worker prompt lists the inputs of every check, approved or
/// not, the same set rule 6 compares against.
#[test]
fn the_prompt_lists_the_inputs_of_an_unapproved_check() {
    let f = initialised();
    f.add_profile("claude-b");
    std::fs::create_dir_all(f.repo_path().join("data")).unwrap();
    std::fs::write(f.repo_path().join("data/unapproved.json"), "{}").unwrap();
    f.cmd()
        .args(["checks", "add", "data", "--cmd", "cargo test", "--input", "data/unapproved.json", "--json"])
        .assert()
        .success();
    let (task_id, _) = f.create_task(Fixture::SPEC);
    let d = dispatch_hooks(&f, &task_id, &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    f.wait_for(&task_id, "180s");
    let prompt = prompt_of(&f, &run_id);
    assert!(prompt.contains("data/unapproved.json"), "{prompt}");
}

/// SMALL-4: the whole `detail` of a run's diff evidence.
fn diff_detail(f: &Fixture, task_id: &str, run_id: &str) -> Value {
    f.evidence_rows(task_id)
        .into_iter()
        .rfind(|e| e["kind"] == "diff" && e["run_id"] == run_id)
        .expect("a diff evidence row")["detail"]
        .clone()
}

/// SMALL-4: commits everything staged in `dir` with a throwaway identity.
fn commit_in(dir: &std::path::Path, msg: &str) {
    common::git(dir, &["-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-m", msg]);
}

/// SMALL-4 AC5 (t1): attempt 1 adds tests/added_test.rs; the rework rewrites
/// it. Counted from the task's base it is still an added file.
#[test]
fn a_test_file_the_task_added_in_attempt_one_needs_no_claim_in_attempt_two() {
    let f = initialised();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task, base) = f.create_task(Fixture::SPEC_TWO_AC);

    // attempt 1, rejected
    let d1 = dispatch_hooks(&f, &task, &[]);
    let run1 = d1["run_id"].as_str().unwrap().to_string();
    let (_, status) = f.wait_for(&task, "180s");
    assert_eq!(status, "in_review");
    let r = f.review_start(&task, "claude-a", "review_reject", &["--model", "haiku"]);
    let mut wait = f.cmd();
    wait.args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s"]);
    f.run_ok(wait, "wait --run");
    assert_eq!(f.wait_for_task_status(&task, &["pass", "reject", "unverified"], 30), "reject");
    let candidate1: String = f
        .state_conn()
        .query_row("SELECT candidate_commit FROM runs WHERE run_id = ?1", [&run1], |r| r.get(0))
        .unwrap();

    // attempt 2 rewrites the test file attempt 1 added
    let d2 = dispatch_hooks(&f, &task, &[("FAKE_WRITE_FILE", "tests/added_test.rs")]);
    let run2 = d2["run_id"].as_str().unwrap().to_string();
    assert_eq!(d2["attempt"], 2);
    assert_eq!(d2["rework"], true);
    let (_, status) = f.wait_for(&task, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task));
    assert_eq!(diff_protected(&f, &task, &run2), json!([]));
    let detail = diff_detail(&f, &task, &run2);
    assert_eq!(detail["files_changed"], json!(["tests/added_test.rs"]));
    assert_eq!(detail["from_commit"], candidate1.as_str());
    assert_eq!(detail["protected_base"], base.as_str());
    assert_eq!(detail["touches_tests"], true);
    let invalid: i64 = f
        .state_conn()
        .query_row(
            "SELECT count(*) FROM events WHERE run_id = ?1 AND type = 'result_invalid'",
            [&run2],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(invalid, 0);
}

/// SMALL-4 AC5 (t2): the worktree fast-forwards to main, which changed a
/// pre-existing test; that change is not the task's.
#[test]
fn a_change_a_merge_of_main_brought_in_needs_no_claim() {
    let f = initialised();
    f.add_profile("claude-b");
    let repo = f.repo_path();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(repo.join("tests/pre_test.rs"), "#[test]\nfn pre() {}\n").unwrap();
    common::git(&repo, &["add", "tests/pre_test.rs"]);
    commit_in(&repo, "pre-existing test");
    let (task, base) = f.create_task(Fixture::SPEC);

    let d1 = f.dispatch(&task, "claude-b", "crash", &[]);
    let (_, status) = f.wait_for(&task, "180s");
    assert_eq!(status, "interrupted");

    std::fs::write(repo.join("tests/pre_test.rs"), "#[test]\nfn pre() { assert!(true); }\n").unwrap();
    common::git(&repo, &["add", "tests/pre_test.rs"]);
    commit_in(&repo, "main changes the test");
    let tip = common::git(&repo, &["rev-parse", "HEAD"]);
    let wt = std::path::PathBuf::from(d1["worktree"].as_str().unwrap());
    common::git(&wt, &["merge", "main"]);
    assert_eq!(common::git(&wt, &["rev-parse", "HEAD"]), tip);

    let d2 = dispatch_hooks(&f, &task, &[]);
    let run2 = d2["run_id"].as_str().unwrap().to_string();
    assert_eq!(d2["worktree_reused"], true);
    assert_eq!(d2["attempt"], 1);
    let (_, status) = f.wait_for(&task, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task));
    assert_eq!(diff_protected(&f, &task, &run2), json!([]));
    let detail = diff_detail(&f, &task, &run2);
    let mut files: Vec<String> = detail["files_changed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    files.sort();
    assert_eq!(files, ["src/added.rs", "tests/added_test.rs", "tests/pre_test.rs"]);
    assert_eq!(detail["touches_tests"], true);
    assert_eq!(detail["integration_tip"], tip.as_str());
    assert_eq!(detail["protected_base"], base.as_str());

    let md = String::from_utf8(
        f.cmd().args(["review", "packet", &task]).assert().success().get_output().stdout.clone(),
    )
    .unwrap();
    let at = md.find("## Protected files changed").expect("the section");
    let section = &md[at..md.find("## Diffs").unwrap()];
    assert_eq!(section.matches("- (none)\n").count(), 2, "{section}");
}

/// SMALL-4 AC5 (t3): after a merge commit of main, the task edits a
/// pre-existing test itself; that one is still listed, the test main changed
/// is not. LEAN-1: listed, and accepted without a claim.
#[test]
fn a_pre_existing_test_the_task_edits_after_merging_main_is_still_listed() {
    let f = initialised();
    f.add_profile("claude-b");
    let repo = f.repo_path();
    std::fs::create_dir_all(repo.join("tests")).unwrap();
    std::fs::write(repo.join("tests/pre_test.rs"), "#[test]\nfn pre() {}\n").unwrap();
    std::fs::write(repo.join("tests/other_test.rs"), "#[test]\nfn other() {}\n").unwrap();
    common::git(&repo, &["add", "tests/pre_test.rs", "tests/other_test.rs"]);
    commit_in(&repo, "pre-existing tests");
    let (task, _base) = f.create_task(Fixture::SPEC);

    let d1 = f.dispatch(&task, "claude-b", "crash", &[]);
    let (_, status) = f.wait_for(&task, "180s");
    assert_eq!(status, "interrupted");

    std::fs::write(repo.join("tests/other_test.rs"), "#[test]\nfn other() { assert!(true); }\n")
        .unwrap();
    common::git(&repo, &["add", "tests/other_test.rs"]);
    commit_in(&repo, "main changes the other test");
    let wt = std::path::PathBuf::from(d1["worktree"].as_str().unwrap());
    std::fs::create_dir_all(wt.join("src")).unwrap();
    std::fs::write(wt.join("src/own.rs"), "pub fn own() {}\n").unwrap();
    common::git(&wt, &["add", "src/own.rs"]);
    commit_in(&wt, "own work");
    common::git(
        &wt,
        &["-c", "user.email=t@example.com", "-c", "user.name=t", "merge", "--no-edit", "main"],
    );
    common::git(&wt, &["rev-parse", "HEAD^2"]);

    let d2 = dispatch_hooks(&f, &task, &[("FAKE_WRITE_FILE", "tests/pre_test.rs")]);
    let run2 = d2["run_id"].as_str().unwrap().to_string();
    let (_, status) = f.wait_for(&task, "180s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task));
    assert_eq!(
        diff_protected(&f, &task, &run2),
        json!([{"path": "tests/pre_test.rs", "status": "modified", "kind": "test"}])
    );
    assert!(run_events(&f, &task, &run2, "result_invalid").is_empty());
}
