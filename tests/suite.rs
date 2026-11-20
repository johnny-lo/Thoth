//! VSCOPE-1: `thoth suite run` runs the approved checks on a main-line
//! commit in a temporary checkout and records a `suite_ran` event; `thoth
//! suite show` says whether HEAD has a green whole-suite run and which merges
//! came since the last one. Everything goes through the CLI in a temporary
//! THOTH_HOME, on a git repo built here with two merge commits; fake-check's
//! `args` mode prints its arguments and the files of the checkout, so the
//! output blob tells which commit it ran on.

mod common;

use common::Fixture;
use serde_json::{json, Value};

const REASON: &str = "VSCOPE-1 fixture: fake checks in a temporary repo";
const FAKE_CHECK: &str = env!("CARGO_BIN_EXE_fake-check");

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn exec(f: &Fixture, args: &[&str]) -> Out {
    let out = f.cmd().args(args).output().unwrap();
    Out {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    }
}

fn ok(f: &Fixture, args: &[&str]) -> Value {
    let mut c = f.cmd();
    c.args(args);
    let out = f.run_ok(c, &args.join(" "));
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// `suite run ARGS --json`: its exit code and document.
fn suite_run(f: &Fixture, args: &[&str]) -> (i32, Value) {
    let mut all = vec!["suite", "run"];
    all.extend_from_slice(args);
    all.push("--json");
    let out = exec(f, &all);
    let v = serde_json::from_str(out.stdout.trim())
        .unwrap_or_else(|e| panic!("suite run printed no JSON ({e}): {}\n{}", out.stdout, out.stderr));
    (out.code, v)
}

fn show(f: &Fixture) -> Value {
    ok(f, &["suite", "show", "--json"])
}

fn add_check(f: &Fixture, id: &str, mode: &str, scoped: bool) {
    let cmd = format!("\"{FAKE_CHECK}\" {mode}");
    let mut args = vec!["checks", "add", id, "--cmd", &cmd, "--shell", "argv"];
    if scoped {
        args.extend(["--scope", "cargo-test-targets"]);
    }
    args.push("--json");
    ok(f, &args);
}

fn commit_file(repo: &std::path::Path, name: &str) {
    std::fs::write(repo.join(name), format!("{name}\n")).unwrap();
    common::git(repo, &["add", name]);
    common::git(repo, &["commit", "-m", &format!("add {name}")]);
}

/// The fixture's two commits (the last one is returned as `before`), then
/// two `--no-ff` merges of a one-file branch each: m1.txt, then m2.txt.
struct Repo {
    f: Fixture,
    before: String,
    merge1: String,
    merge2: String,
}

fn repo() -> Repo {
    let f = common::fixture();
    let r = f.repo_path();
    let before = common::git(&r, &["rev-parse", "HEAD"]);
    let mut merges = Vec::new();
    for (branch, file, subject) in [("feat1", "m1.txt", "merge: FIRST-1"), ("feat2", "m2.txt", "merge: SECOND-2")] {
        common::git(&r, &["checkout", "-q", "-b", branch]);
        commit_file(&r, file);
        common::git(&r, &["checkout", "-q", "main"]);
        common::git(&r, &["merge", "-q", "--no-ff", branch, "-m", subject]);
        merges.push(common::git(&r, &["rev-parse", "HEAD"]));
    }
    f.cmd().arg("init").assert().success();
    add_check(&f, "plain", "args", false);
    add_check(&f, "scoped", "args", true);
    ok(&f, &["checks", "approve", "--reason", REASON, "--json"]);
    Repo { f, before, merge1: merges[0].clone(), merge2: merges[1].clone() }
}

fn rows(v: &Value) -> &Vec<Value> {
    v["results"].as_array().unwrap_or_else(|| panic!("no results in {v}"))
}

/// The stored output of a row, read from the `blob_path` it reports.
fn output_of(row: &Value) -> String {
    let path = row["blob_path"].as_str().unwrap_or_else(|| panic!("no blob_path in {row}"));
    std::fs::read_to_string(path).unwrap()
}

fn argv_tail(row: &Value, n: usize) -> Vec<String> {
    let argv: Vec<String> = row["argv"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect();
    argv[argv.len() - n..].to_vec()
}

/// AC5, AC6: before any run `show` has nothing; a green whole suite on the
/// commit before the first merge lists both merges since; `--commit` and the
/// default HEAD run on the right commit; a scoped check is not in the default
/// list; a green `--test` run on HEAD does not make HEAD green; a whole-suite
/// run on HEAD does.
#[test]
fn suite_runs_on_the_right_commit_and_show_tracks_the_green_head() {
    let Repo { f, before, merge1, merge2 } = repo();
    let s = show(&f);
    assert_eq!(s["head_commit"], merge2.as_str(), "{s}");
    assert_eq!(s["last"], Value::Null, "{s}");
    assert_eq!(s["last_green_commit"], Value::Null, "{s}");
    assert_eq!(s["head_green"], false, "{s}");
    assert_eq!(s["merges_since_green"], Value::Null, "{s}");

    // green before the first merge
    let (code, v) = suite_run(&f, &["--commit", &before]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["commit"], before.as_str());
    assert_eq!(v["full"], true);
    assert_eq!(v["ok"], true);
    let ids: Vec<&str> = rows(&v).iter().map(|r| r["check_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["plain"], "a scoped check is not in the default list: {v}");
    let out = output_of(&rows(&v)[0]);
    assert!(out.contains("file: b.txt") && !out.contains("file: m1.txt"), "{out}");
    assert!(rows(&v)[0]["sha256"].is_string(), "{v}");

    let s = show(&f);
    assert_eq!(s["last_green_commit"], before.as_str(), "{s}");
    assert_eq!(s["head_green"], false);
    assert_eq!(
        s["merges_since_green"],
        json!([{"commit": merge1, "subject": "merge: FIRST-1"}, {"commit": merge2, "subject": "merge: SECOND-2"}]),
        "{s}"
    );
    assert_eq!(s["last"]["commit"], before.as_str());
    assert!(s["last"]["ts"].is_string(), "{s}");

    // `--commit` on the first merge
    let (code, v) = suite_run(&f, &["--commit", &merge1]);
    assert_eq!(code, 0, "{v}");
    let out = output_of(&rows(&v)[0]);
    assert!(out.contains("file: m1.txt") && !out.contains("file: m2.txt"), "{out}");

    // only a `--test` run on HEAD: green, but not a whole suite
    let (code, v) = suite_run(&f, &["--check", "scoped", "--test", "alpha", "--test", "beta"]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["commit"], merge2.as_str());
    assert_eq!(v["full"], false);
    assert_eq!(argv_tail(&rows(&v)[0], 4), ["--test", "alpha", "--test", "beta"]);
    let out = output_of(&rows(&v)[0]);
    assert!(out.starts_with("--test\nalpha\n--test\nbeta\n"), "{out}");
    let s = show(&f);
    assert_eq!(s["head_green"], false, "{s}");
    assert_eq!(s["last"]["full"], false, "{s}");
    assert_eq!(s["last_green_commit"], merge1.as_str(), "{s}");
    assert_eq!(s["merges_since_green"], json!([{"commit": merge2, "subject": "merge: SECOND-2"}]), "{s}");

    // a whole suite on HEAD (the default commit)
    let (code, v) = suite_run(&f, &[]);
    assert_eq!(code, 0, "{v}");
    assert_eq!(v["commit"], merge2.as_str());
    assert!(output_of(&rows(&v)[0]).contains("file: m2.txt"));
    let s = show(&f);
    assert_eq!(s["head_green"], true, "{s}");
    assert_eq!(s["last_green_commit"], merge2.as_str());
    assert_eq!(s["merges_since_green"], json!([]), "{s}");
    let human = exec(&f, &["suite", "show"]).stdout;
    assert!(human.lines().next().unwrap().contains("is green"), "{human}");
}

/// AC5: `--test` with an unscoped check, with two checks, or with no check
/// exits non-zero, runs nothing and records nothing.
#[test]
fn test_flags_need_exactly_one_scoped_check() {
    let Repo { f, .. } = repo();
    let (code, _) = suite_run(&f, &["--check", "plain"]);
    assert_eq!(code, 0);
    let before = show(&f)["last"].clone();
    for args in [
        vec!["--check", "plain", "--test", "a"],
        vec!["--check", "scoped", "--check", "plain", "--test", "a"],
        vec!["--test", "a"],
        vec!["--check", "scoped", "--test", "not a name"],
    ] {
        let mut all = vec!["suite", "run"];
        all.extend(args.iter().copied());
        all.push("--json");
        let out = exec(&f, &all);
        assert_ne!(out.code, 0, "{args:?} was taken: {}", out.stdout);
        assert!(out.stderr.contains("--test"), "{args:?}: {}", out.stderr);
        assert_eq!(show(&f)["last"], before, "{args:?} recorded a run");
    }
    let human = exec(&f, &["suite", "show"]).stdout;
    assert!(human.lines().next().unwrap().contains("is not green"), "{human}");
}

/// AC5: an unapproved check is refused with its reason and a failing check
/// fails the run; both exit 1 with `ok: false`, and neither makes HEAD green.
#[test]
fn an_unapproved_or_failing_check_makes_the_run_fail() {
    let Repo { f, merge2, .. } = repo();
    add_check(&f, "pending", "exit 0", false);

    let (code, v) = suite_run(&f, &["--check", "pending"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["ok"], false);
    let r = &rows(&v)[0];
    assert!(r["error"].as_str().unwrap_or("").contains("not approved"), "{v}");
    assert_eq!(r["exit_code"], Value::Null);

    // the default list now holds the pending one too
    let (code, v) = suite_run(&f, &[]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(v["full"], true);
    assert_eq!(v["ok"], false);
    let ids: Vec<&str> = rows(&v).iter().map(|r| r["check_id"].as_str().unwrap()).collect();
    assert_eq!(ids, ["pending", "plain"], "{v}");
    let s = show(&f);
    assert_eq!(s["head_commit"], merge2.as_str());
    assert_eq!(s["head_green"], false, "{s}");
    assert_eq!(s["last"]["ok"], false, "{s}");

    ok(&f, &["checks", "remove", "pending", "--json"]);
    add_check(&f, "failing", "exit 3", false);
    ok(&f, &["checks", "approve", "--reason", REASON, "--json"]);
    let (code, v) = suite_run(&f, &["--check", "failing"]);
    assert_eq!(code, 1, "{v}");
    assert_eq!(rows(&v)[0]["exit_code"], 3, "{v}");
    assert_eq!(rows(&v)[0]["error"], Value::Null, "{v}");
    assert_eq!(v["ok"], false);
}
