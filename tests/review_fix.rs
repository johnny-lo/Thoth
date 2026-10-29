//! RFIX-1: `review start --edit`, the fix Thoth applies and bounds, `verify`
//! on it, and `review confirm`. Everything is read through the CLI (and
//! MCP), the run dirs and git.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const SPEC: &str = r#"{"goal":"keep the greeting right","acceptance_criteria":["AC1: greets the user","AC2: exits zero"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal"}"#;
const DOCS_SPEC: &str = r#"{"goal":"write the guide","acceptance_criteria":["AC1: the guide exists"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal","task_type":"docs"}"#;
const CODEX_MODEL: &str = "gpt-5.6-luna";
const AGY_MODEL: &str = "gemini-3.8-flash-low";

/// `count` numbered lines, `prefix N`.
fn lines(prefix: &str, count: usize) -> String {
    (1..=count).map(|i| format!("{prefix} {i}\n")).collect()
}

/// The files every candidate starts with.
fn base_files() -> Vec<(String, Vec<u8>)> {
    let text = |p: &str, c: String| (p.to_string(), c.into_bytes());
    vec![
        text("src/greet.rs", lines("line", 30)),
        text("src/other.rs", lines("other", 5)),
        text("src/third.rs", lines("third", 5)),
        text("src/fourth.rs", lines("fourth", 5)),
        text("tests/greet_check.rs", "fn helper() {}\nfn checks() { let x = 1; }\n".into()),
        text("tests/git.rs", "fn v() {}\n".into()),
        text("src/guarded.rs", "#[test]\nfn guarded() { assert_eq!(1, 1); }\n#[cfg(test)]\nmod t {}\nplain line\n".into()),
        text("config/data.json", "{\"name\": \"x\"}\n".into()),
        text("config/check_in.txt", "input\n".into()),
        text("plans/spec.json", format!("{SPEC}\n")),
        text("notes/crlf.txt", "alpha\r\nbeta\r\n".into()),
        ("notes/latin1.txt".to_string(), vec![0x63, 0x61, 0x66, 0xE9, 0x0A]),
    ]
}

struct Setup {
    f: Fixture,
    task: String,
    implementer: String,
    candidate: String,
    code: String,
}

trait AgyFixture {
    fn add_agy_profile(&self, id: &str) -> PathBuf;
}

impl AgyFixture for Fixture {
    fn add_agy_profile(&self, id: &str) -> PathBuf {
        let cfg = self.home.path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        self.cmd()
            .args(["profile", "add", id, "--provider", "google", "--harness", "antigravity", "--config-dir"])
            .arg(&cfg)
            .assert()
            .success();
        cfg
    }
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

fn json_out(out: &[u8]) -> Value {
    serde_json::from_str(String::from_utf8_lossy(out).trim())
        .unwrap_or_else(|e| panic!("not JSON ({e}): {}", String::from_utf8_lossy(out)))
}

/// The detached-descendant script `verify` sees as an incomplete output:
/// it exits 0 while a child still holds its output pipe past the grace.
#[cfg(windows)]
const DETACHED: (&str, &str, &str, &str) = ("detached.cmd", "@echo off\r\nstart /b ping -n 5 127.0.0.1\r\n", "cmd", r".\detached.cmd");
#[cfg(not(windows))]
const DETACHED: (&str, &str, &str, &str) = ("detached.sh", "#!/bin/sh\nsleep 4 &\n", "sh", "./detached.sh");

/// Writes `files` into the fixture repo, lets `more` add to it, and commits.
fn commit_files(f: &Fixture, files: &[(String, Vec<u8>)], more: impl Fn(&Path)) {
    let repo = f.repo_path();
    for (p, c) in files {
        let path = repo.join(p);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, c).unwrap();
    }
    more(&repo);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let script = repo.join(DETACHED.0);
        if script.exists() {
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }
    common::git(&repo, &["add", "-A"]);
    common::git(&repo, &["commit", "-m", "candidate files"]);
}

/// A task of `spec` at `in_review` whose candidate holds `base_files()`
/// plus `extra`.
fn setup_with(spec: &str, extra: &[(&str, &str)]) -> Setup {
    let extra: Vec<(String, Vec<u8>)> = extra.iter().map(|(p, c)| (p.to_string(), c.as_bytes().to_vec())).collect();
    setup_files(spec, &extra, |_| {})
}

/// `setup_with`, with `more` run on the repo before the commit.
#[cfg(unix)]
fn setup_custom(spec: &str, more: impl Fn(&Path)) -> Setup {
    setup_files(spec, &[], more)
}

fn setup_files(spec: &str, extra: &[(String, Vec<u8>)], more: impl Fn(&Path)) -> Setup {
    let f = common::fixture();
    let mut files = base_files();
    files.extend(extra.iter().cloned());
    files.push((".gitattributes".into(), b"* -text\n".to_vec()));
    files.push((".gitignore".into(), b"build-out/\n".to_vec()));
    files.push((DETACHED.0.into(), DETACHED.1.as_bytes().to_vec()));
    commit_files(&f, &files, more);
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    f.add_codex_profile("codex-a");
    f.add_agy_profile("agy-a");
    let (task, _) = f.create_task(spec);
    let d = f.dispatch(&task, "claude-b", "ok", &[]);
    let implementer = s(&d["run_id"]);
    let (_, status) = f.wait_for(&task, "120s");
    assert_eq!(status, "in_review", "{}", f.explain_task(&task));
    let candidate = s(&f.run_of(&task, &implementer)["candidate_commit"]);
    let code = s(&f.evidence_rows(&task)[0]["code"]);
    Setup { f, task, implementer, candidate, code }
}

fn setup() -> Setup {
    setup_with(SPEC, &[])
}

/// A review document: AC1 unmet with `size`, AC2 met, the given extras
/// (`(note, size)`), and `edits`.
fn doc(st: &Setup, size: Option<&str>, extras: &[(&str, Option<&str>)], edits: Value) -> Value {
    let mut ac1 = json!({"id": "AC1", "verdict": "unmet", "evidence": [st.code], "note": "the greeting is wrong", "category": "spec_mismatch"});
    if let Some(sz) = size {
        ac1["fix_size"] = json!(sz);
    }
    let extra: Vec<Value> = extras
        .iter()
        .map(|(note, sz)| {
            let mut e = json!({"severity": "minor", "note": note, "evidence": [], "category": "docs"});
            if let Some(sz) = sz {
                e["fix_size"] = json!(sz);
            }
            e
        })
        .collect();
    json!({
        "verdict": "REJECT",
        "findings": {"criteria": [ac1, {"id": "AC2", "verdict": "met", "evidence": [st.code]}], "extra_findings": extra},
        "edits": edits,
    })
}

fn edit(path: &str, old: &str, new: &str, criterion: &str) -> Value {
    json!({"path": path, "old": old, "new": new, "criterion": criterion, "reason": "fixes the finding"})
}

/// A REJECT of AC1 (size S) with these edits, all bound to AC1.
fn fix_of(st: &Setup, edits: Value) -> Value {
    json!({"document": doc(st, Some("S"), &[], edits)})
}

/// A one-edit fix of AC1: line 1 of src/greet.rs.
fn small_fix(st: &Setup) -> Value {
    fix_of(st, json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1")]))
}

fn plan_file(st: &Setup, plan: &Value) -> PathBuf {
    let p = st.f.home_path().join(format!("plan-{}.json", now_tag()));
    std::fs::write(&p, plan.to_string()).unwrap();
    p
}

fn now_tag() -> String {
    format!("{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos())
}

fn agy_data(st: &Setup) -> String {
    st.f.home_path().join("profiles").join("agy-a").display().to_string()
}

/// The model and effort flags for a profile.
fn launch(profile: &str) -> Vec<&'static str> {
    match profile {
        "codex-a" => vec!["--model", CODEX_MODEL, "--effort", "medium"],
        "agy-a" => vec!["--model", AGY_MODEL],
        _ => vec!["--model", "opus", "--effort", "medium"],
    }
}

fn review_cmd(st: &Setup, verb: &str, profile: &str, scenario: &str, plan: Option<&Path>, extra: &[&str]) -> assert_cmd::Command {
    let mut c = st.f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", &st.task)
        .env("FAKE_AGY_DATA_DIR", agy_data(st))
        .args(["review", verb, &st.task, "--profile", profile, "--json"]);
    if !extra.contains(&"--model") {
        c.args(launch(profile));
    }
    c.args(extra);
    if let Some(p) = plan {
        c.env("FAKE_FIX_PLAN", p);
    }
    c
}

/// `review start --edit` with `plan`; waits for the run.
fn edit_review(st: &Setup, profile: &str, plan: &Value) -> (String, Value) {
    let p = plan_file(st, plan);
    let c = review_cmd(st, "start", profile, "review_fix", Some(&p), &["--edit"]);
    let started = json_out(&st.f.run_ok(c, "review start --edit"));
    assert_eq!(started["mode"], "edit", "{started}");
    let run = s(&started["run_id"]);
    (run.clone(), wait_run(st, &run))
}

fn wait_run(st: &Setup, run: &str) -> Value {
    let mut c = st.f.cmd();
    c.args(["wait", "--run", run, "--timeout", "120s", "--json"]);
    json_out(&st.f.run_ok(c, "wait --run"))
}

fn show(st: &Setup) -> Value {
    let mut c = st.f.cmd();
    c.args(["review", "show", &st.task, "--json"]);
    json_out(&st.f.run_ok(c, "review show"))
}

fn show_human(st: &Setup) -> String {
    let mut c = st.f.cmd();
    c.args(["review", "show", &st.task]);
    String::from_utf8(st.f.run_ok(c, "review show")).unwrap()
}

fn events(st: &Setup, ty: &str) -> Vec<Value> {
    st.f.events_json(&st.task).into_iter().filter(|e| e["type"] == ty).collect()
}

fn last_event(st: &Setup, ty: &str) -> Value {
    events(st, ty)
        .last()
        .cloned()
        .unwrap_or_else(|| panic!("no {ty} event\n{}", st.f.explain_task(&st.task)))["detail"]
        .clone()
}

fn fix_branch(st: &Setup) -> String {
    format!("thoth/{}/a1-fix", st.task)
}

fn branch_sha(st: &Setup, branch: &str) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")])
        .current_dir(st.f.repo_path())
        .output()
        .unwrap();
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn fix_worktree(st: &Setup) -> PathBuf {
    st.f.home_path().join("repos").join(st.f.repo_id()).join("worktrees").join(format!("{}-a1-fix", st.task))
}

/// Every change in the fix worktree outside `.agent/` and `.agents/`,
/// ignored files included.
fn dirty(wt: &Path) -> Vec<String> {
    common::git(wt, &["status", "--porcelain", "--ignored", "--untracked-files=all"])
        .lines()
        .filter(|l| !l[3..].starts_with(".agent"))
        .map(str::to_string)
        .collect()
}

/// Approved checks: `true` passes (`git --version`), `false` fails.
fn add_checks(st: &Setup, ids: &[(&str, bool)]) {
    for (id, passes) in ids {
        let cmd = if *passes { "git --version" } else { "git no-such-subcommand-rfix" };
        st.f.cmd().args(["checks", "add", id, "--cmd", cmd, "--shell", "argv", "--json"]).assert().success();
    }
    st.f.cmd().args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
}

fn add_check_raw(st: &Setup, id: &str, cmd: &str, shell: &str) {
    st.f.cmd().args(["checks", "add", id, "--cmd", cmd, "--shell", shell, "--json"]).assert().success();
    st.f.cmd().args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
}

fn verify(st: &Setup, run: &str, extra: &[&str]) -> (i32, Value) {
    let out = st.f.cmd().args(["verify", run, "--json"]).args(extra).output().unwrap();
    (out.status.code().unwrap_or(-1), json_out(&out.stdout))
}

fn confirm(st: &Setup, profile: &str, scenario: &str, extra: &[&str]) -> Value {
    let c = review_cmd(st, "confirm", profile, scenario, None, extra);
    json_out(&st.f.run_ok(c, "review confirm"))
}

fn confirm_with(st: &Setup, profile: &str, scenario: &str, envs: &[(&str, &str)]) -> Value {
    let mut c = review_cmd(st, "confirm", profile, scenario, None, &[]);
    for (k, v) in envs {
        c.env(k, v);
    }
    json_out(&st.f.run_ok(c, "review confirm"))
}

/// `review confirm` that must exit 1 and start nothing; its stderr.
fn confirm_err(st: &Setup, profile: &str, extra: &[&str]) -> String {
    let before = run_count(st);
    let out = review_cmd(st, "confirm", profile, "fix_confirm_approve", None, extra).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "stdout {}", String::from_utf8_lossy(&out.stdout));
    assert_eq!(run_count(st), before, "a refused confirm starts no run");
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn run_count(st: &Setup) -> usize {
    let out = st.f.cmd().args(["status", &st.task, "--json"]).output().unwrap();
    json_out(&out.stdout)["runs"].as_array().unwrap().len()
}

/// An applied fix: REJECT recorded, task still `in_review`, pending.
fn assert_applied(st: &Setup, plan: &Value) -> (String, Value) {
    let (run, w) = edit_review(st, "claude-a", plan);
    assert_eq!(w["verdict"], "REJECT", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["task_status"], "in_review", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["pending_fix"], true);
    let applied = last_event(st, "review_fix_applied");
    assert_eq!(st.f.events_json(&st.task).iter().filter(|e| e["type"] == "review_fix_applied" && e["run_id"] == run.as_str()).count(), 1);
    (run, applied)
}

/// A refused fix at `stage`: the REJECT settles the task `reject`, the fix
/// worktree is back at the candidate with nothing else in it, the fix
/// branch is where it was and the implementer's branch is untouched.
fn assert_refused(st: &Setup, plan: &Value, stage: &str) -> Value {
    let branch_before = branch_sha(st, &fix_branch(st));
    let (run, w) = edit_review(st, "claude-a", plan);
    assert_eq!(w["verdict"], "REJECT", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["task_status"], "reject", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["pending_fix"], false);
    let refused = last_event(st, "review_fix_refused");
    assert_eq!(refused["stage"], stage, "{refused}");
    let wt = fix_worktree(st);
    assert_eq!(common::git(&wt, &["rev-parse", "HEAD"]), st.candidate);
    assert!(dirty(&wt).is_empty(), "{:?}", dirty(&wt));
    assert_eq!(branch_sha(st, &fix_branch(st)), branch_before);
    assert_eq!(branch_sha(st, &format!("thoth/{}/a1", st.task)).as_deref(), Some(st.candidate.as_str()));
    assert!(!st.f.events_json(&st.task).iter().any(|e| e["type"] == "review_fix_applied" && e["run_id"] == run.as_str()));
    let verdict = last_event(st, "verdict");
    assert_eq!(verdict["mode"], "edit");
    assert_eq!(verdict["fix"], json!({"applied": false, "stage": stage}));
    assert_eq!(st.f.run_of(&st.task, &run)["candidate_commit"], st.candidate);
    refused
}

macro_rules! refusals {
    ($($name:ident: $stage:literal => |$st:ident| $plan:expr;)*) => {
        $(
            #[test]
            fn $name() {
                let $st = setup();
                let plan = $plan;
                assert_refused(&$st, &plan, $stage);
            }
        )*
    };
}

// AC9 (a): the bounds.
refusals! {
    bounds_refuses_twenty_one_replaced_lines: "bounds" => |st| fix_of(&st, json!([edit("src/greet.rs", &lines("line", 21), &lines("LINE", 21), "AC1")]));
    bounds_refuses_twenty_deleted_and_twenty_one_added: "bounds" => |st| fix_of(&st, json!([edit("src/greet.rs", &lines("line", 20), &lines("LINE", 21), "AC1")]));
    bounds_refuses_a_new_file_of_forty_one_lines: "bounds" => |st| fix_of(&st, json!([edit("src/new.rs", "", &lines("n", 41), "AC1")]));
    bounds_refuses_four_files: "bounds" => |st| fix_of(&st, json!([
        edit("src/other.rs", "other 1\n", "o\n", "AC1"), edit("src/third.rs", "third 1\n", "t\n", "AC1"),
        edit("src/fourth.rs", "fourth 1\n", "f\n", "AC1"), edit("src/greet.rs", "line 1\n", "g\n", "AC1")]));
    bounds_refuses_a_file_with_nul: "bounds" => |st| fix_of(&st, json!([edit("src/blob.dat", "", "a\u{0}b\n", "AC1")]));
}

// AC9 (b): spec files.
refusals! {
    spec_file_refuses_a_spec_in_the_candidate: "spec_file" => |st| fix_of(&st, json!([edit("plans/spec.json", "keep the greeting right", "keep it right", "AC1")]));
    spec_file_refuses_a_file_that_becomes_a_spec: "spec_file" => |st| fix_of(&st, json!([edit("config/data.json", "{\"name\": \"x\"}\n", &format!("{SPEC}\n"), "AC1")]));
    spec_file_refuses_a_new_spec: "spec_file" => |st| fix_of(&st, json!([edit("plans/new.json", "", &format!("{SPEC}\n"), "AC1")]));
}

// AC9 (c): tests.
refusals! {
    tests_refuses_changing_a_line_of_an_existing_test: "tests" => |st| fix_of(&st, json!([edit("tests/greet_check.rs", "let x = 1;", "let x = 2;", "AC1")]));
    tests_refuses_deleting_an_assert_line_from_any_file: "tests" => |st| fix_of(&st, json!([edit("src/guarded.rs", "fn guarded() { assert_eq!(1, 1); }\n", "fn guarded() {}\n", "AC1")]));
    tests_refuses_deleting_a_test_attribute: "tests" => |st| fix_of(&st, json!([edit("src/guarded.rs", "#[test]\nfn guarded", "fn guarded", "AC1")]));
    tests_refuses_deleting_a_cfg_test_attribute: "tests" => |st| fix_of(&st, json!([edit("src/guarded.rs", "#[cfg(test)]\nmod t", "mod t", "AC1")]));
    tests_refuses_adding_ignore: "tests" => |st| fix_of(&st, json!([edit("src/guarded.rs", "plain line\n", "plain line\n#[ignore]\n", "AC1")]));
}

// AC9 (d): size and binding.
refusals! {
    size_refuses_an_l_finding: "size" => |st| json!({"document": doc(&st, Some("L"), &[], json!([edit("src/greet.rs", "line 1\n", "x\n", "AC1")]))});
    size_refuses_an_unsized_finding: "size" => |st| json!({"document": doc(&st, None, &[], json!([edit("src/greet.rs", "line 1\n", "x\n", "AC1")]))});
    size_refuses_a_second_unmet_criterion_without_an_edit: "size" => |st| {
        let mut d = doc(&st, Some("S"), &[], json!([edit("src/greet.rs", "line 1\n", "x\n", "AC1")]));
        d["findings"]["criteria"][1] = json!({"id": "AC2", "verdict": "unmet", "evidence": [st.code], "note": "n", "fix_size": "S", "category": "docs"});
        json!({"document": d})
    };
    edits_refuses_binding_a_met_criterion: "edits" => |st| fix_of(&st, json!([edit("src/greet.rs", "line 1\n", "x\n", "AC2")]));
    edits_refuses_binding_an_unknown_criterion: "edits" => |st| fix_of(&st, json!([edit("src/greet.rs", "line 1\n", "x\n", "AC9")]));
    edits_refuses_binding_an_extra_finding_out_of_range: "edits" => |st| json!({"document": doc(&st, Some("S"), &[("e", Some("S"))], json!([
        edit("src/greet.rs", "line 1\n", "x\n", "AC1"), edit("src/other.rs", "other 1\n", "y\n", "extra_findings[1]")]))});
}

// AC9 (f): the reviewer left the fix worktree changed.
refusals! {
    worktree_refuses_a_tracked_change: "worktree" => |st| { let mut p = small_fix(&st); p["write"] = json!([{"path": "src/other.rs", "content": "changed\n"}]); p };
    worktree_refuses_an_untracked_file: "worktree" => |st| { let mut p = small_fix(&st); p["write"] = json!([{"path": "made_by_reviewer.txt", "content": "x\n"}]); p };
    worktree_refuses_an_ignored_file: "worktree" => |st| { let mut p = small_fix(&st); p["write"] = json!([{"path": "build-out/x.txt", "content": "x\n"}]); p };
    worktree_refuses_a_moved_head: "worktree" => |st| { let mut p = small_fix(&st); p["git"] = json!([["commit", "--allow-empty", "-m", "moved"]]); p };
}

// AC9 (g): AC2's apply and field rules, one case each.
refusals! {
    apply_refuses_old_not_found: "apply" => |st| fix_of(&st, json!([edit("src/greet.rs", "no such text", "x", "AC1")]));
    apply_refuses_old_found_twice: "apply" => |st| fix_of(&st, json!([edit("src/greet.rs", "line 2", "x", "AC1")]));
    apply_refuses_creating_an_existing_file: "apply" => |st| fix_of(&st, json!([edit("src/other.rs", "", "x\n", "AC1")]));
    apply_refuses_replacing_in_a_missing_file: "apply" => |st| fix_of(&st, json!([edit("src/missing.rs", "x", "y", "AC1")]));
    apply_refuses_a_file_that_is_not_utf8: "apply" => |st| fix_of(&st, json!([edit("notes/latin1.txt", "caf", "cafe", "AC1")]));
    apply_refuses_edits_that_change_nothing: "apply" => |st| fix_of(&st, json!([edit("src/greet.rs", "line 1\n", "line one\n", "AC1"), edit("src/greet.rs", "line one\n", "line 1\n", "AC1")]));
    edits_refuses_a_path_that_climbs_out: "edits" => |st| fix_of(&st, json!([edit("../outside.rs", "", "x\n", "AC1")]));
    edits_refuses_a_path_under_dot_git: "edits" => |st| fix_of(&st, json!([edit(".git/config", "", "x\n", "AC1")]));
    edits_refuses_a_device_name: "edits" => |st| fix_of(&st, json!([edit("src/nul.txt", "", "x\n", "AC1")]));
    edits_refuses_an_edit_without_a_reason: "edits" => |st| fix_of(&st, json!([{"path": "src/greet.rs", "old": "line 1\n", "new": "x\n", "criterion": "AC1"}]));
    edits_refuses_a_blank_reason: "edits" => |st| fix_of(&st, json!([{"path": "src/greet.rs", "old": "line 1\n", "new": "x\n", "criterion": "AC1", "reason": "  "}]));
    edits_refuses_an_empty_new_file: "edits" => |st| fix_of(&st, json!([edit("src/empty.rs", "", "", "AC1")]));
}

/// AC9 (g): the second edit failing rolls the first back, and (AC4) a fix
/// branch an earlier applied fix made stays where it was.
#[test]
fn a_failing_second_edit_rolls_back_the_first_and_keeps_the_existing_fix_branch() {
    let st = setup();
    let (_, first) = assert_applied(&st, &small_fix(&st));
    let fix = s(&first["fix_commit"]);
    let refused = assert_refused(
        &st,
        &fix_of(&st, json!([edit("src/other.rs", "other 1\n", "changed\n", "AC1"), edit("src/greet.rs", "nowhere", "x", "AC1")])),
        "apply",
    );
    assert!(s(&refused["reason"]).contains("edit 1"), "{refused}");
    assert_eq!(std::fs::read_to_string(fix_worktree(&st).join("src/other.rs")).unwrap(), lines("other", 5));
    assert_eq!(branch_sha(&st, &fix_branch(&st)).as_deref(), Some(fix.as_str()), "a refused fix leaves the earlier fix ref alone");
}

/// AC9 (g): a path through a symlink (or, on Windows, a junction: see
/// `fix_edits`' unit tests, git cannot commit one) is refused.
#[cfg(unix)]
#[test]
fn edits_refuses_a_path_through_a_symlink() {
    let st = setup_custom(SPEC, |repo| {
        std::os::unix::fs::symlink("other.rs", repo.join("src/linked.rs")).unwrap();
        std::os::unix::fs::symlink("../src", repo.join("config/srclink")).unwrap();
    });
    let refused = assert_refused(&st, &fix_of(&st, json!([edit("src/linked.rs", "other 1\n", "x\n", "AC1")])), "edits");
    assert!(s(&refused["reason"]).contains("symlink"), "{refused}");
    let st = setup_custom(SPEC, |repo| {
        std::os::unix::fs::symlink("../src", repo.join("config/srclink")).unwrap();
    });
    let refused = assert_refused(&st, &fix_of(&st, json!([edit("config/srclink/other.rs", "other 1\n", "x\n", "AC1")])), "edits");
    assert!(s(&refused["reason"]).contains("symlink"), "{refused}");
}

/// AC9 (a) (b) (c) (d) (e): the cases that are inside every bound, each
/// applied in turn (a new edit review supersedes the pending one).
#[test]
fn fixes_inside_every_bound_are_applied() {
    let st = setup();
    let (_, a) = assert_applied(&st, &fix_of(&st, json!([edit("src/greet.rs", &lines("line", 20), &lines("LINE", 20), "AC1")])));
    assert_eq!(a["changed_lines"], 40, "twenty replaced lines count 40");
    let (_, a) = assert_applied(&st, &fix_of(&st, json!([edit("src/new.rs", "", &lines("n", 40), "AC1")])));
    assert_eq!(a["changed_lines"], 40);
    assert_eq!(a["files"][0]["status"], "added");
    assert_eq!(a["edits"][0]["op"], "create");
    let (_, a) = assert_applied(&st, &fix_of(&st, json!([edit("src/greet.rs", "line 1\n", "line A\n", "AC1"), edit("src/greet.rs", "line A\n", "line B\n", "AC1")])));
    assert_eq!(a["changed_lines"], 2, "one path, its net change, counted once");
    assert_eq!(a["file_count"], 1);
    let (_, a) = assert_applied(&st, &fix_of(&st, json!([
        edit("src/other.rs", "other 1\n", "o\n", "AC1"), edit("src/third.rs", "third 1\n", "t\n", "AC1"), edit("src/fourth.rs", "fourth 1\n", "f\n", "AC1")])));
    assert_eq!(a["file_count"], 3);
    let (_, a) = assert_applied(&st, &fix_of(&st, json!([edit("config/data.json", "\"x\"", "\"y\"", "AC1")])));
    assert_eq!(a["files"][0]["path"], "config/data.json");
    let (_, a) = assert_applied(&st, &fix_of(&st, json!([edit("tests/greet_check.rs", "fn helper() {}\n", "fn helper() {}\nfn more() {}\n", "AC1")])));
    assert_eq!(a["files"][0]["deleted"], 0);
    let protected: Vec<Value> = st.f.evidence_rows(&st.task).into_iter().filter(|e| e["kind"] == "diff").collect();
    let fix_diff = protected.last().unwrap();
    assert!(s(&fix_diff["label"]).starts_with("review fix: "), "{fix_diff}");
    assert_eq!(fix_diff["detail"]["protected_changes"], json!([{"path": "tests/greet_check.rs", "status": "modified", "kind": "test"}]));
    let (_, a) = assert_applied(&st, &fix_of(&st, json!([edit("tests/added_check.rs", "", "fn added() {}\n", "AC1")])));
    assert_eq!(a["files"][0]["status"], "added");
    let (_, a) = assert_applied(&st, &json!({"document": doc(&st, Some("M"), &[], json!([edit("src/greet.rs", "line 3\n", "x\n", "AC1")]))}));
    assert_eq!(a["edits"][0]["fix_size"], "M");
    let (_, a) = assert_applied(&st, &json!({"document": doc(&st, Some("s"), &[("an extra", Some("m"))], json!([
        edit("src/greet.rs", "line 4\n", "x\n", "AC1"), edit("src/other.rs", "other 2\n", "y\n", "extra_findings[0]")]))}));
    assert_eq!(a["edits"][0]["fix_size"], "S", "labels are normalised first");
    assert_eq!(a["edits"][1]["fix_size"], "M");
    let (_, a) = assert_applied(&st, &fix_of(&st, json!([edit("notes/crlf.txt", "alpha\nbeta", "alpha\ngamma\nbeta", "AC1")])));
    let fix = s(&a["fix_commit"]);
    let bytes = std::process::Command::new("git").args(["show", &format!("{fix}:notes/crlf.txt")]).current_dir(st.f.repo_path()).output().unwrap().stdout;
    assert_eq!(bytes, b"alpha\r\ngamma\r\nbeta\r\n");
    assert_eq!(st.f.task_status(&st.task), "in_review");
    assert!(events(&st, "verdict").is_empty());
}

/// AC9 (e): check inputs (approved or not) and check-word files are
/// refused, a test that is also a check word too.
#[test]
fn protected_refuses_an_unapproved_check_input() {
    let st = setup();
    st.f.cmd().args(["checks", "add", "pending-one", "--cmd", "git --version", "--shell", "argv", "--input", "config/check_in.txt", "--json"]).assert().success();
    assert_refused(&st, &fix_of(&st, json!([edit("config/check_in.txt", "input\n", "changed\n", "AC1")])), "protected");
}

#[test]
fn protected_refuses_a_check_word_file() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    assert_refused(&st, &fix_of(&st, json!([edit("docs/git.md", "", "v\n", "AC1")])), "protected");
}

#[test]
fn protected_refuses_a_test_that_is_also_a_check_word() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    assert_refused(&st, &fix_of(&st, json!([edit("tests/git.rs", "fn v() {}\n", "fn v() {}\nfn w() {}\n", "AC1")])), "protected");
}

/// A document built from a setup.
type DocCase = dyn Fn(&Setup) -> Value + Sync;

/// AC9 (h): `edits` missing or null, or carried by a PASS or UNVERIFIED, is
/// an invalid edit-mode report.
#[test]
fn an_edit_report_without_edits_or_with_edits_on_a_pass_is_invalid() {
    let cases: Vec<Box<DocCase>> = vec![
        Box::new(|st| { let mut d = doc(st, Some("S"), &[], json!([])); d.as_object_mut().unwrap().remove("edits"); d }),
        Box::new(|st| { let mut d = doc(st, Some("S"), &[], json!([])); d["edits"] = Value::Null; d }),
        Box::new(|st| json!({"verdict": "PASS", "findings": {"criteria": [{"id": "AC1", "verdict": "met", "evidence": [st.code]}, {"id": "AC2", "verdict": "met", "evidence": [st.code]}], "extra_findings": []},
            "edits": [edit("src/greet.rs", "line 1\n", "x\n", "AC1")]})),
        Box::new(|st| json!({"verdict": "UNVERIFIED", "findings": {"criteria": [{"id": "AC1", "verdict": "met", "evidence": [st.code]}, {"id": "AC2", "verdict": "met", "evidence": [st.code]}], "extra_findings": []},
            "edits": [edit("src/greet.rs", "line 1\n", "x\n", "AC1")]})),
    ];
    std::thread::scope(|scope| {
        for case in &cases {
            scope.spawn(move || {
                let st = setup();
                let (_, w) = edit_review(&st, "claude-a", &json!({"document": case(&st)}));
                assert_eq!(w["task_status"], "unverified", "{}", st.f.explain_task(&st.task));
                assert_eq!(last_event(&st, "review_invalid")["stage"], "schema");
                assert!(events(&st, "review_fix_applied").is_empty() && events(&st, "review_fix_refused").is_empty());
            });
        }
    });
}

/// AC9 (h): without `--edit`, edits are ignored: a plain REJECT.
#[test]
fn a_plain_review_ignores_edits_and_rejects() {
    let st = setup();
    let p = plan_file(&st, &small_fix(&st));
    let c = review_cmd(&st, "start", "claude-a", "review_fix", Some(&p), &[]);
    let started = json_out(&st.f.run_ok(c, "review start"));
    assert_eq!(started["mode"], "review");
    assert!(s(&started["worktree"]).ends_with("-a1-review"));
    let w = wait_run(&st, &s(&started["run_id"]));
    assert_eq!(w["task_status"], "reject");
    assert_eq!(w["pending_fix"], false);
    assert!(events(&st, "review_fix_applied").is_empty() && events(&st, "review_fix_refused").is_empty());
    assert_eq!(last_event(&st, "review_started")["mode"], "review");
    assert_eq!(show(&st)["mode"], "review");
}

/// AC1, AC9 (h): `--edit` on a document task is refused before anything is
/// written.
#[test]
fn edit_on_a_document_task_is_refused_and_writes_nothing() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_profile("claude-a");
    let (task, _) = f.create_task(DOCS_SPEC);
    f.dispatch(&task, "claude-b", "ok", &[]);
    let (_, status) = f.wait_for(&task, "120s");
    assert_eq!(status, "in_review");
    let before_events = f.events_json(&task).len();
    let before_runs = f.cmd().args(["status", &task, "--json"]).output().map(|o| json_out(&o.stdout)["runs"].as_array().unwrap().len()).unwrap();
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", "review_fix")
        .args(["review", "start", &task, "--profile", "claude-a", "--edit", "--model", "opus", "--effort", "medium", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("document task"), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(f.events_json(&task).len(), before_events, "no event, evidence_added included");
    let after_runs = f.cmd().args(["status", &task, "--json"]).output().map(|o| json_out(&o.stdout)["runs"].as_array().unwrap().len()).unwrap();
    assert_eq!(after_runs, before_runs);
}

/// Waits until `path` exists (a runner writes it before launching).
fn wait_file(path: &Path) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while !path.exists() {
        assert!(std::time::Instant::now() < deadline, "{} never appeared", path.display());
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

fn cancel(st: &Setup, run: &str) {
    st.f.cmd().args(["cancel", run, "--json"]).assert().success();
    let w = wait_run(st, run);
    assert_eq!(w["status"], "cancelled", "{w}");
}

/// AC1: each harness in both modes: the mode, the worktree, the prompt's
/// edit section, the schema's `edits`, and the same tool list. The runs are
/// held at `FAKE_REVIEW_GATE` and cancelled, which leaves the task
/// `in_review`.
#[test]
fn each_harness_gets_the_edit_section_and_schema_only_in_edit_mode() {
    std::thread::scope(|scope| {
        for profile in ["claude-a", "codex-a", "agy-a"] {
            scope.spawn(move || {
                let st = setup();
                let gate = st.f.home_path().join("never-open");
                let mut tools = Vec::new();
                for edit_mode in [false, true] {
                    let p = plan_file(&st, &small_fix(&st));
                    let flags: &[&str] = if edit_mode { &["--edit"] } else { &[] };
                    let mut c = review_cmd(&st, "start", profile, "review_fix", Some(&p), flags);
                    c.env("FAKE_REVIEW_GATE", &gate);
                    let started = json_out(&st.f.run_ok(c, "review start"));
                    let run = s(&started["run_id"]);
                    assert_eq!(started["mode"], if edit_mode { "edit" } else { "review" });
                    assert_eq!(last_event(&st, "review_started")["mode"], started["mode"]);
                    let row = st.f.run_of(&st.task, &run);
                    assert_eq!(s(&row["worktree"]).ends_with("-a1-fix"), edit_mode, "{row}");
                    let dir = PathBuf::from(s(&started["run_dir"]));
                    wait_file(&dir.join("prompt.txt"));
                    let prompt = std::fs::read_to_string(dir.join("prompt.txt")).unwrap();
                    assert_eq!(prompt.contains("## Attaching a fix (edit mode)"), edit_mode, "{profile}");
                    if edit_mode {
                        assert!(prompt.contains("at most 40 changed lines (added plus deleted"), "{prompt}");
                        assert!(prompt.contains("at most 3 files"), "{prompt}");
                        assert_eq!(prompt.contains("the only file you write is .agent/review.json"), profile == "claude-a");
                    }
                    if profile != "claude-a" {
                        let schema: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("review-schema.json")).unwrap()).unwrap();
                        assert_eq!(schema["properties"].get("edits").is_some(), edit_mode, "{schema}");
                        assert_eq!(schema["required"].as_array().unwrap().contains(&json!("edits")), edit_mode);
                        if edit_mode {
                            let item = &schema["properties"]["edits"]["items"];
                            assert_eq!(item["additionalProperties"], false);
                            assert_eq!(item["required"], json!(["path", "old", "new", "criterion", "reason"]));
                        }
                    }
                    tools.push(row["allowed_tools"].clone());
                    cancel(&st, &run);
                    assert_eq!(st.f.task_status(&st.task), "in_review");
                }
                assert_eq!(tools[0], tools[1], "{profile}: the tools do not change with the mode");
            });
        }
    });
}

/// AC6 (i): a codex edit review, an agy confirmer's REVISE: the task is
/// rejected with the note, one REJECT verdict, and the next attempt starts
/// from the implementer's candidate, not the fix.
#[test]
fn codex_fix_revised_by_agy_rejects_and_the_next_attempt_skips_the_fix() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let (run, w) = edit_review(&st, "codex-a", &small_fix(&st));
    assert_eq!(w["pending_fix"], true, "{}", st.f.explain_task(&st.task));
    let fix = s(&last_event(&st, "review_fix_applied")["fix_commit"]);
    assert_eq!(verify(&st, &run, &[]).0, 0);
    let c = confirm(&st, "agy-a", "fix_confirm_revise", &[]);
    let w = wait_run(&st, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "REJECT", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["task_status"], "reject");
    assert_eq!(w["pending_fix"], false);
    let shown = show(&st);
    let notes: Vec<String> = shown["findings"]["extra_findings"].as_array().unwrap().iter().map(|e| s(&e["note"])).collect();
    assert!(notes.iter().any(|n| n.starts_with("author check REVISE on fix edit 0 (src/greet.rs): fake confirmer")), "{notes:?}");
    assert!(notes.iter().any(|n| n.contains("was not adopted")), "{notes:?}");
    assert_eq!(shown["findings"]["criteria"][0]["verdict"], "unmet");
    assert_eq!(shown["merge_branch"], Value::Null);
    let rejects: Vec<Value> = events(&st, "verdict").into_iter().filter(|e| e["detail"]["verdict"] == "REJECT").collect();
    assert_eq!(rejects.len(), 1, "one REJECT for the edit-then-confirm chain");
    assert_eq!(rejects[0]["detail"]["source"], "author_check");
    assert_eq!(rejects[0]["detail"]["mode"], "confirm");
    let d = st.f.dispatch(&st.task, "claude-b", "ok", &[]);
    let (_, status) = st.f.wait_for(&st.task, "120s");
    assert_eq!(status, "in_review");
    assert_eq!(d["attempt"], 2);
    let a2 = format!("thoth/{}/a2", st.task);
    let ancestor = std::process::Command::new("git").args(["merge-base", "--is-ancestor", &fix, &a2]).current_dir(st.f.repo_path()).status().unwrap();
    assert!(!ancestor.success(), "attempt 2 must not contain the fix");
}

/// AC6 (i): an agy edit review, a Claude confirmer's APPROVE: PASS.
#[test]
fn agy_fix_confirmed_by_claude_passes() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let (run, w) = edit_review(&st, "agy-a", &small_fix(&st));
    assert_eq!(w["pending_fix"], true, "{}", st.f.explain_task(&st.task));
    assert_eq!(verify(&st, &run, &[]).0, 0);
    let c = confirm(&st, "claude-a", "fix_confirm_approve", &["--model", "haiku"]);
    assert_eq!(c["same_provider_as_implementer"], true, "claude-b implemented, claude-a confirms");
    let w = wait_run(&st, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "PASS", "{}", st.f.explain_task(&st.task));
    let shown = show(&st);
    assert_eq!(shown["findings"]["criteria"][0]["verdict"], "met");
    assert!(shown["findings"]["criteria"][0]["note"].as_str().unwrap().contains(&s(&last_event(&st, "review_fix_applied")["fix_commit"])));
}

/// AC5 (i): each family's malformed confirm lands at `schema` (not a
/// missing file); the fix stays pending and a second confirm succeeds.
#[test]
fn a_malformed_confirm_is_invalid_at_schema_and_can_be_run_again() {
    std::thread::scope(|scope| {
        for profile in ["claude-a", "codex-a", "agy-a"] {
            scope.spawn(move || {
                let st = setup();
                add_checks(&st, &[("green", true)]);
                let (run, _) = edit_review(&st, if profile == "claude-a" { "codex-a" } else { "claude-a" }, &small_fix(&st));
                assert_eq!(verify(&st, &run, &[]).0, 0);
                let extra: &[&str] = if profile == "claude-a" { &["--model", "haiku"] } else { &[] };
                let c = confirm(&st, profile, "fix_confirm_bad", extra);
                let w = wait_run(&st, &s(&c["run_id"]));
                assert_eq!(w["status"], "completed", "{}", st.f.explain_run(&s(&c["run_id"])));
                let invalid = last_event(&st, "review_invalid");
                assert_eq!(invalid["stage"], "schema", "{profile}: {invalid}");
                assert_eq!(invalid["source"], "author_check");
                assert!(s(&invalid["reason"]).contains("review.json"), "{invalid}");
                assert_eq!(st.f.task_status(&st.task), "in_review");
                assert_eq!(show(&st)["pending_fix"], true);
                let c = confirm(&st, profile, "fix_confirm_approve", extra);
                assert_eq!(wait_run(&st, &s(&c["run_id"]))["verdict"], "PASS", "{}", st.f.explain_task(&st.task));
            });
        }
    });
}

/// AC5 (i): coverage and consistency of the confirmer's document.
#[test]
fn confirm_documents_are_held_to_coverage_and_consistency() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let (run, _) = edit_review(&st, "claude-a", &fix_of(&st, json!([edit("src/greet.rs", "line 1\n", "a\n", "AC1"), edit("src/other.rs", "other 1\n", "b\n", "AC1")])));
    assert_eq!(verify(&st, &run, &[]).0, 0);
    let ok = |i: u64| json!({"index": i, "verdict": "ok", "note": ""});
    let cases = [
        (json!({"verdict": "APPROVE", "edits": [ok(0)]}), "coverage"),
        (json!({"verdict": "APPROVE", "edits": [ok(0), ok(1), ok(1)]}), "coverage"),
        (json!({"verdict": "APPROVE", "edits": [ok(0), ok(1), ok(2)]}), "coverage"),
        (json!({"verdict": "APPROVE", "edits": [ok(0), {"index": 1, "verdict": "revise", "note": "n"}]}), "schema"),
        (json!({"verdict": "REVISE", "edits": [ok(0), ok(1)]}), "schema"),
        (json!({"verdict": "REVISE", "edits": [ok(0), {"index": 1, "verdict": "revise", "note": " "}]}), "schema"),
        (json!({"verdict": "REVISE", "edits": [ok(0), {"index": 1, "verdict": "revise"}]}), "schema"),
    ];
    for (doc, stage) in cases {
        let c = confirm_with(&st, "codex-a", "fix_confirm_approve", &[("FAKE_CONFIRM_DOC", &doc.to_string())]);
        let w = wait_run(&st, &s(&c["run_id"]));
        assert_eq!(w["status"], "completed");
        let invalid = last_event(&st, "review_invalid");
        assert_eq!(invalid["stage"], stage, "{doc}: {invalid}");
        assert_eq!(st.f.task_status(&st.task), "in_review");
    }
    let c = confirm(&st, "codex-a", "fix_confirm_approve", &[]);
    assert_eq!(wait_run(&st, &s(&c["run_id"]))["verdict"], "PASS");
}

/// AC6 (f): a confirmer that writes a file cannot pass the task.
#[test]
fn a_confirmer_that_changes_files_is_invalid_at_worktree() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
    assert_eq!(verify(&st, &run, &[]).0, 0);
    let plan = plan_file(&st, &json!({"write": [{"path": "src/confirmer.rs", "content": "x\n"}]}));
    let c = confirm_with(&st, "codex-a", "fix_confirm_approve", &[("FAKE_FIX_PLAN", plan.to_str().unwrap())]);
    let w = wait_run(&st, &s(&c["run_id"]));
    assert_eq!(w["verdict"], Value::Null);
    assert_eq!(last_event(&st, "review_invalid")["stage"], "worktree");
    assert_eq!(st.f.task_status(&st.task), "in_review");
    assert_eq!(show(&st)["pending_fix"], true);
}

/// AC7: a confirm ruled invalid because the confirmer changed files can be
/// run again: the fix worktree is put back at the fix commit, so the next
/// confirmer, one that writes nothing, gets a verdict.
#[test]
fn after_a_confirmer_that_changed_files_another_confirm_gets_a_verdict() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
    assert_eq!(verify(&st, &run, &[]).0, 0);
    let fix = s(&last_event(&st, "review_fix_applied")["fix_commit"]);
    let plan = plan_file(&st, &json!({"write": [{"path": "src/greet.rs", "content": "rewritten\n"}, {"path": "src/confirmer.rs", "content": "x\n"}]}));
    let c = confirm_with(&st, "codex-a", "fix_confirm_approve", &[("FAKE_FIX_PLAN", plan.to_str().unwrap())]);
    let w = wait_run(&st, &s(&c["run_id"]));
    assert_eq!(w["verdict"], Value::Null);
    let invalid = last_event(&st, "review_invalid");
    assert_eq!(invalid["stage"], "worktree", "{invalid}");
    assert_eq!(st.f.task_status(&st.task), "in_review");
    assert_eq!(show(&st)["pending_fix"], true);
    let wt = fix_worktree(&st);
    assert_eq!(common::git(&wt, &["rev-parse", "HEAD"]), fix);
    assert!(dirty(&wt).is_empty(), "{:?}", dirty(&wt));
    assert_eq!(branch_sha(&st, &fix_branch(&st)).as_deref(), Some(fix.as_str()));

    let c = confirm(&st, "claude-a", "fix_confirm_approve", &["--model", "haiku"]);
    let w = wait_run(&st, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "PASS", "{}", st.f.explain_task(&st.task));
    assert_eq!(st.f.task_status(&st.task), "pass");
    assert_eq!(show(&st)["merge_commit"], fix.as_str());
}

/// AC5 (i), AC6: a red verify, or one whose output was not collected in
/// full, lets the confirm run, and its APPROVE still rejects.
#[test]
fn an_approve_over_a_red_verify_rejects_with_the_check_named() {
    let st = setup();
    add_checks(&st, &[("green", true), ("redcheck", false)]);
    let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
    assert_eq!(verify(&st, &run, &[]).0, 1);
    let c = confirm(&st, "codex-a", "fix_confirm_approve", &[]);
    assert_eq!(c["fix_verify"]["ok"], false);
    let w = wait_run(&st, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "REJECT", "{}", st.f.explain_task(&st.task));
    let notes: Vec<String> = show(&st)["findings"]["extra_findings"].as_array().unwrap().iter().map(|e| s(&e["note"])).collect();
    assert!(notes.iter().any(|n| n.contains("redcheck exit")), "{notes:?}");
    assert_eq!(last_event(&st, "verdict")["author_verdict"], "APPROVE");
}

#[test]
fn an_exit_zero_verify_with_an_incomplete_output_is_not_green() {
    let st = setup();
    add_check_raw(&st, "detached", DETACHED.3, DETACHED.2);
    let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
    let (_, v) = verify(&st, &run, &[]);
    assert_eq!(v["results"][0]["exit_code"], 0, "{v}");
    assert!(!v["results"][0]["error"].is_null(), "{v}");
    assert_eq!(v["worktree_clean"], true, "{v}");
    let c = confirm(&st, "codex-a", "fix_confirm_approve", &[]);
    assert_eq!(c["fix_verify"]["ok"], false);
    assert_eq!(wait_run(&st, &s(&c["run_id"]))["verdict"], "REJECT");
}

/// AC5: the confirm refusals that write nothing, and the same-provider key.
#[test]
fn confirm_is_refused_without_a_usable_verify_or_with_the_fixers_model() {
    let st = setup();
    let none = confirm_err(&st, "codex-a", &[]);
    assert!(none.contains("has no edit review"), "{none}");
    add_checks(&st, &[("green", true)]);
    let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
    let fix = s(&last_event(&st, "review_fix_applied")["fix_commit"]);
    let err = confirm_err(&st, "codex-a", &[]);
    assert!(err.contains(&format!("thoth verify {run}")), "{err}");
    assert_eq!(verify(&st, &run, &["--dry-run"]).1["head_commit"], Value::Null);
    let err = confirm_err(&st, "codex-a", &[]);
    assert!(err.contains("thoth verify"), "a dry run records nothing: {err}");
    let (_, base) = verify(&st, &run, &["--at-base"]);
    assert_eq!(base["head_commit"], base["at_commit"]);
    let err = confirm_err(&st, "codex-a", &[]);
    assert!(err.contains("--at-base"), "{err}");
    let (_, v) = verify(&st, &run, &[]);
    assert_eq!(v["head_commit"], fix);
    let err = confirm_err(&st, "claude-b", &["--model", "opus", "--effort", "medium"]);
    assert!(err.contains("edit review's own"), "another profile of the same model and effort: {err}");
    let err = confirm_err(&st, "claude-a", &["--model", "opus"]);
    assert!(err.contains("--effort"), "{err}");
    let c = confirm(&st, "claude-b", "fix_confirm_approve", &["--model", "opus", "--effort", "high"]);
    assert_eq!(c["same_provider_as_implementer"], true);
    assert_eq!(last_event(&st, "launch_choice")["source"], "review_confirm");
    assert_eq!(wait_run(&st, &s(&c["run_id"]))["verdict"], "PASS");
}

/// AC5: a verify that moved HEAD or changed a file is not on the fix.
#[test]
fn confirm_is_refused_when_the_verify_changed_the_fix_worktree() {
    for (cmd, key) in [("git config --file made_by_check.cfg a.b c", "worktree_clean"), ("git -c user.name=v -c user.email=v@example.com commit --allow-empty -m moved", "head_commit_after")] {
        let st = setup();
        add_checks(&st, &[("green", true)]);
        let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
        let fix = s(&last_event(&st, "review_fix_applied")["fix_commit"]);
        st.f.cmd().args(["checks", "add", "changer", "--cmd", cmd, "--shell", "argv", "--json"]).assert().success();
        st.f.cmd().args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
        let (_, v) = verify(&st, &run, &[]);
        assert_eq!(v["worktree_clean"], false, "{v}");
        if key == "head_commit_after" {
            assert_ne!(v["head_commit_after"], fix);
        }
        let err = confirm_err(&st, "codex-a", &[]);
        assert!(err.contains("thoth verify"), "{err}");
    }
}

/// AC5, AC7: a reroute's new implementer run ends the pending fix.
#[test]
fn a_reroute_after_the_fix_ends_it() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
    assert_eq!(verify(&st, &run, &[]).0, 0);
    st.f.dispatch(&st.task, "claude-b", "ok", &[]);
    let (_, status) = st.f.wait_for(&st.task, "120s");
    assert_eq!(status, "in_review");
    assert_eq!(show(&st)["pending_fix"], false);
    let err = confirm_err(&st, "codex-a", &[]);
    assert!(err.contains("new implementer run"), "{err}");
}

/// AC9 (j), AC6: a confirm held at the gate blocks a new edit review
/// without its input being cleared; cancelled, the fix is still pending and
/// a second confirm passes.
#[test]
fn a_live_confirm_blocks_a_new_edit_review_and_a_cancelled_one_can_be_redone() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
    assert_eq!(verify(&st, &run, &[]).0, 0);
    let gate = st.f.home_path().join("gate");
    let c = confirm_with(&st, "codex-a", "fix_confirm_approve", &[("FAKE_REVIEW_GATE", gate.to_str().unwrap())]);
    let confirm_run = s(&c["run_id"]);
    let input = fix_worktree(&st).join(".agent/review-fix-input/edits.json");
    wait_file(&input);
    let p = plan_file(&st, &small_fix(&st));
    let out = review_cmd(&st, "start", "claude-a", "review_fix", Some(&p), &["--edit"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(input.exists(), "a refused start must not clear the live confirm's input");
    let out = st.f.cmd().args(["dispatch", &st.task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "dispatch is blocked by the live confirm too");
    let w = st.f.cmd().args(["wait", "--run", &confirm_run, "--timeout", "0s", "--json"]).output().unwrap();
    assert_eq!(w.status.code(), Some(3));
    assert_eq!(json_out(&w.stdout)["pending_fix"], true);
    cancel(&st, &confirm_run);
    assert_eq!(st.f.task_status(&st.task), "in_review");
    assert_eq!(show(&st)["pending_fix"], true);
    let c = confirm(&st, "codex-a", "fix_confirm_approve", &[]);
    assert_eq!(wait_run(&st, &s(&c["run_id"]))["verdict"], "PASS");
}

/// AC7: what `review show` and `wait` report around a fix, and that only a
/// PASS still pointing at its commit names what to merge.
#[test]
fn show_and_wait_report_the_fix_and_only_a_standing_pass_names_a_merge() {
    let st = setup();
    let empty = show(&st);
    assert_eq!(empty["pending_fix"], false);
    for k in ["mode", "fix", "merge_branch", "merge_commit"] {
        assert_eq!(empty[k], Value::Null, "{k}");
    }
    let w = st.f.cmd().args(["wait", "--run", &st.implementer, "--timeout", "5s", "--json"]).output().unwrap();
    assert_eq!(json_out(&w.stdout)["pending_fix"], Value::Null, "an implementer run is not a code review");
    add_checks(&st, &[("green", true)]);
    let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
    let human = show_human(&st);
    assert!(human.contains("fix    applied"), "{human}");
    assert!(human.contains("edit 0  src/greet.rs  AC1  S  fixes the finding"), "{human}");
    assert!(human.contains(&format!("next: thoth verify {run}, then thoth review confirm {}", st.task)), "{human}");
    let out = st.f.cmd().args(["task", "close", &st.task, "--reason", "not now"]).output().unwrap();
    assert_eq!(out.status.code(), Some(1), "task close refuses an in_review code task");
    assert_eq!(verify(&st, &run, &[]).0, 0);
    assert_eq!(show(&st)["fix"]["verify"]["ok"], true);
    let c = confirm(&st, "codex-a", "fix_confirm_approve", &[]);
    let w = wait_run(&st, &s(&c["run_id"]));
    assert_eq!(w["verdict"], "PASS");
    let fix = s(&last_event(&st, "review_fix_applied")["fix_commit"]);
    assert_eq!(show(&st)["merge_commit"], fix);
    let human = String::from_utf8(st.f.run_ok({ let mut c = st.f.cmd(); c.args(["wait", "--run", &s(&w["run_id"])]); c }, "wait")).unwrap();
    assert!(human.contains(&format!("merge_branch {}", fix_branch(&st))), "{human}");
    // prune clears the clean fix worktree; the branch and commit stay
    st.f.cmd().args(["worktrees", "prune", "--json"]).assert().success();
    assert!(!fix_worktree(&st).exists(), "prune removes the clean fix worktree of a passed task");
    assert_eq!(branch_sha(&st, &fix_branch(&st)).as_deref(), Some(fix.as_str()));
    // a ref moved elsewhere is not reported as the thing to merge
    common::git(&st.f.repo_path(), &["branch", "-f", &fix_branch(&st), &st.candidate]);
    assert_eq!(show(&st)["merge_branch"], Value::Null);
    assert_eq!(show(&st)["merge_commit"], Value::Null);
}

/// AC7: a `review record` after a fix supersedes it.
#[test]
fn a_recorded_review_supersedes_the_pending_fix() {
    let st = setup();
    let _ = edit_review(&st, "claude-a", &small_fix(&st));
    let pass = json!({"verdict": "PASS", "findings": {"criteria": [
        {"id": "AC1", "verdict": "met", "evidence": [st.code]}, {"id": "AC2", "verdict": "met", "evidence": [st.code]}], "extra_findings": []}});
    let file = st.f.home_path().join("external.json");
    std::fs::write(&file, pass.to_string()).unwrap();
    st.f.cmd().args(["review", "record", &st.task, "-f"]).arg(&file).assert().success();
    let shown = show(&st);
    assert_eq!(shown["mode"], "external");
    assert_eq!(shown["pending_fix"], false);
    assert_eq!(shown["fix"]["applied"], true, "the newest edit review is still reported");
    assert_eq!(shown["merge_branch"], format!("thoth/{}/a1", st.task));
    assert_eq!(shown["merge_commit"], st.candidate);
    assert!(confirm_err(&st, "codex-a", &[]).contains("needs in_review"), "the recorded PASS settled the task");
}

/// AC6: `retro` counts an edit-then-confirm chain once: the edit review's
/// findings and only what the confirm added.
#[test]
fn retro_counts_a_confirmed_and_a_revised_fix_once_each() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let (run, _) = edit_review(&st, "claude-a", &small_fix(&st));
    assert_eq!(verify(&st, &run, &[]).0, 0);
    let c = confirm(&st, "codex-a", "fix_confirm_approve", &[]);
    assert_eq!(wait_run(&st, &s(&c["run_id"]))["verdict"], "PASS");

    let (task2, _) = st.f.create_task(SPEC);
    st.f.dispatch(&task2, "claude-b", "ok", &[]);
    assert_eq!(st.f.wait_for(&task2, "120s").1, "in_review");
    let st2 = Setup { f: st.f, task: task2, implementer: String::new(), candidate: st.candidate.clone(), code: st.code.clone() };
    let (run2, _) = edit_review(&st2, "claude-a", &small_fix(&st2));
    assert_eq!(verify(&st2, &run2, &[]).0, 0);
    let c = confirm(&st2, "codex-a", "fix_confirm_revise", &[]);
    assert_eq!(wait_run(&st2, &s(&c["run_id"]))["verdict"], "REJECT");

    let out = st2.f.cmd().args(["retro", "--json"]).output().unwrap();
    let r = json_out(&out.stdout);
    assert_eq!(r["reviews"], 2, "{r}");
    let count = |cat: &str| r["categories"].as_array().unwrap().iter().find(|g| g["category"] == cat).map(|g| g["count"].as_u64().unwrap()).unwrap_or(0);
    assert_eq!(count("spec_mismatch"), 2, "the two edit reviews' AC1, once each: {r}");
    assert_eq!(count("unclassified"), 2, "the REVISE note and the not-adopted line: {r}");
    assert_eq!(r["findings"], 4, "{r}");
}

struct Mcp {
    child: std::process::Child,
    input: std::process::ChildStdin,
    output: BufReader<std::process::ChildStdout>,
    id: u64,
}

impl Mcp {
    fn start(st: &Setup, scenario: &str, plan: &Path) -> Self {
        let mut c = std::process::Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(st.f.repo.path())
            .env("THOTH_HOME", st.f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("FAKE_SCENARIO", scenario)
            .env("FAKE_TASK_ID", &st.task)
            .env("FAKE_FIX_PLAN", plan)
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .env_remove("AISTUPIDLEVEL_API_KEY")
            .env_remove("ARTIFICIALANALYSIS_API_KEY")
            .env("THOTH_ARENA_URL", common::offline_arena_url(st.f.home.path()))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null());
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        let mut m = Mcp { child, input, output, id: 0 };
        m.send("initialize", json!({"protocolVersion": "2025-06-18"}));
        m
    }

    fn send(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(self.input, "{}", json!({"jsonrpc": "2.0", "id": self.id, "method": method, "params": params})).unwrap();
        self.input.flush().unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn call(&mut self, name: &str, args: Value) -> Value {
        self.send("tools/call", json!({"name": name, "arguments": args}))["result"].clone()
    }

    fn wait(&mut self, run: &str) -> Value {
        loop {
            let r = self.call("thoth_wait", json!({"run": run, "timeout_seconds": 20}));
            if r["structuredContent"]["timed_out"] != true {
                return r["structuredContent"].clone();
            }
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// AC9 (k), AC8: the whole chain through MCP, with the CLI's verify.
#[test]
fn mcp_starts_an_edit_review_and_a_confirm() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let plan = plan_file(&st, &small_fix(&st));
    let mut m = Mcp::start(&st, "review_fix", &plan);
    let r = m.call("thoth_review_start", json!({"task": st.task, "profile": "claude-a", "model": "opus", "effort": "medium", "edit": true}));
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["mode"], "edit");
    let run = s(&r["structuredContent"]["run_id"]);
    assert_eq!(m.wait(&run)["pending_fix"], true);
    let shown = m.call("thoth_review_show", json!({"task": st.task}));
    assert_eq!(shown["structuredContent"]["pending_fix"], true);
    assert_eq!(verify(&st, &run, &[]).0, 0);
    let missing = m.call("thoth_review_confirm", json!({"task": st.task, "profile": "codex-a", "effort": "medium"}));
    assert_eq!(missing["isError"], true, "model is required: {missing}");
    drop(m);
    let mut m = Mcp::start(&st, "fix_confirm_approve", &plan);
    let c = m.call("thoth_review_confirm", json!({"task": st.task, "profile": "codex-a", "model": CODEX_MODEL, "effort": "medium", "reason": "the other provider"}));
    assert_eq!(c["isError"], false, "{c}");
    assert_eq!(c["structuredContent"]["mode"], "confirm");
    let w = m.wait(&s(&c["structuredContent"]["run_id"]));
    assert_eq!(w["verdict"], "PASS", "{}", st.f.explain_task(&st.task));
    assert_eq!(st.f.task_status(&st.task), "pass");
}

/// AC8: `edit` false is a plain review.
#[test]
fn mcp_review_start_without_edit_attaches_no_fix() {
    let st = setup();
    let plan = plan_file(&st, &small_fix(&st));
    let mut m = Mcp::start(&st, "review_fix", &plan);
    let r = m.call("thoth_review_start", json!({"task": st.task, "profile": "claude-a", "model": "opus", "effort": "medium", "edit": false}));
    assert_eq!(r["structuredContent"]["mode"], "review", "{r}");
    let w = m.wait(&s(&r["structuredContent"]["run_id"]));
    assert_eq!(w["task_status"], "reject");
    assert!(events(&st, "review_fix_applied").is_empty());
}

/// AC1, AC4, AC5, AC6, AC7 (i): a Claude edit review whose fix is applied,
/// a verify on the fix, a codex confirmer's APPROVE: the task passes on the
/// fix branch.
#[test]
fn claude_fix_verified_and_confirmed_by_codex_passes_on_the_fix_branch() {
    let st = setup();
    add_checks(&st, &[("green", true)]);
    let (run, w) = edit_review(&st, "claude-a", &small_fix(&st));
    assert_eq!(w["status"], "completed", "{}", st.f.explain_run(&run));
    assert_eq!(w["verdict"], "REJECT");
    assert_eq!(w["task_status"], "in_review", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["pending_fix"], true);

    let applied = last_event(&st, "review_fix_applied");
    let fix = s(&applied["fix_commit"]);
    assert_eq!(branch_sha(&st, &fix_branch(&st)).as_deref(), Some(fix.as_str()));
    assert_eq!(common::git(&st.f.repo_path(), &["rev-parse", &format!("{fix}^")]), st.candidate);
    assert_eq!(applied["changed_lines"], 2);
    assert_eq!(applied["file_count"], 1);
    assert_eq!(applied["edits"][0]["op"], "replace");
    assert_eq!(applied["edits"][0]["fix_size"], "S");
    let numstat = common::git(&st.f.repo_path(), &["diff", "--numstat", &st.candidate, &fix]);
    let sum: u64 = numstat.lines().map(|l| l.split('\t').take(2).map(|n| n.parse::<u64>().unwrap()).sum::<u64>()).sum();
    assert_eq!(sum, 2);
    let wt = fix_worktree(&st);
    assert_eq!(std::fs::read_to_string(wt.join("src/greet.rs")).unwrap().lines().next(), Some("line one"));
    let reviewer = st.f.run_of(&st.task, &run);
    assert!(s(&reviewer["worktree"]).ends_with("-a1-fix"), "{reviewer}");
    assert_eq!(reviewer["candidate_commit"], fix);
    assert_eq!(st.f.run_of(&st.task, &st.implementer)["candidate_commit"], st.candidate);
    assert!(events(&st, "verdict").is_empty(), "an applied fix writes no verdict yet");
    let labels: Vec<String> = st.f.evidence_rows(&st.task).iter().filter(|e| e["run_id"] == run).map(|e| s(&e["label"])).collect();
    assert!(labels.iter().any(|l| l.starts_with("review fix: git diff")), "{labels:?}");
    assert!(labels.iter().any(|l| l.starts_with("review fix: commit")), "{labels:?}");

    let shown = show(&st);
    assert_eq!(shown["mode"], "edit");
    assert_eq!(shown["pending_fix"], true);
    assert_eq!(shown["fix"]["applied"], true);
    assert_eq!(shown["fix"]["fix_commit"], fix);
    assert_eq!(shown["merge_branch"], Value::Null);

    let (code, v) = verify(&st, &run, &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["head_commit"], fix);
    assert_eq!(v["head_commit_after"], fix);
    assert_eq!(v["worktree_clean"], true);

    let c = confirm(&st, "codex-a", "fix_confirm_approve", &["--reason", "another provider, fresh quota"]);
    assert_eq!(c["mode"], "confirm");
    assert_eq!(c["fix_verify"]["ok"], true);
    assert_eq!(c["same_provider_as_implementer"], false);
    assert_eq!(c["implementer_provider"], "anthropic");
    let confirm_run = s(&c["run_id"]);
    let started = last_event(&st, "review_started");
    assert_eq!(started["mode"], "confirm");
    assert_eq!(started["fix_commit"], fix);
    let w = wait_run(&st, &confirm_run);
    let edits_json: Value = serde_json::from_str(&std::fs::read_to_string(wt.join(".agent/review-fix-input/edits.json")).unwrap()).unwrap();
    assert_eq!(edits_json, applied["edits"]);
    assert_eq!(w["verdict"], "PASS", "{}", st.f.explain_task(&st.task));
    assert_eq!(w["task_status"], "pass");
    assert_eq!(st.f.task_status(&st.task), "pass");
    assert_eq!(st.f.run_of(&st.task, &confirm_run)["candidate_commit"], fix);
    let shown = show(&st);
    assert_eq!(shown["mode"], "confirm");
    assert_eq!(shown["source"], "author_check");
    assert_eq!(shown["merge_branch"], fix_branch(&st));
    assert_eq!(shown["merge_commit"], fix);
    assert_eq!(events(&st, "verdict").len(), 1, "one verdict per edit-then-confirm chain");
    let verdict = last_event(&st, "verdict");
    assert_eq!(verdict["source"], "author_check");
    assert_eq!(verdict["author_verdict"], "APPROVE");
}
