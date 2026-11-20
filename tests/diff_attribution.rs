//! SMALL-9c: a diff evidence row and the review packet show only the
//! worker's own changes when the worker merged (or fast-forwarded to) the
//! main line during its run. Everything is observed through the CLI:
//! `evidence list --json`, `evidence show --json` and `review packet`, with
//! fake-claude workers and git on temporary repositories.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// A in the spec: a file both the main line and the worker edit, in
/// separate places.
const SHARED: &str = "src/shared.rs";
/// B: a file only the worker edits.
const WORKER_ONLY: &str = "src/worker_only.rs";
/// T: a test file only the main line adds.
const MAIN_TEST: &str = "tests/main_line_test.rs";
/// What the main line writes on line 2 of A, and the worker on line 18.
const MAIN_LINE: &str = "main line two";
const WORKER_LINE: &str = "worker line eighteen";
/// A path holding a space and `[`, which a glob would read as a class.
const SPECIAL: &str = "src/a [b].rs";
/// The worker path that `SPECIAL` would match if it were read as a glob.
const GLOB_MATCH: &str = "src/a b.rs";

/// A `thoth` invocation from the fixture, minus every inherited variable
/// whose name holds KEY, TOKEN, SECRET or PASSWORD (names only are read).
fn thoth(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c
}

/// Runs `c`, requires exit 0 and returns its stdout.
fn ok(mut c: assert_cmd::Command, what: &str) -> String {
    let out = c.output().expect("spawn thoth");
    assert!(
        out.status.success(),
        "thoth {what} exited {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn ok_json(c: assert_cmd::Command, what: &str) -> Value {
    let text = ok(c, what);
    serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{what}: {e}: {text}"))
}

/// git in `dir`, stdout as raw bytes; panics with git's stderr.
fn git_bytes(dir: &Path, args: &[&str]) -> Vec<u8> {
    let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

fn git(dir: &Path, args: &[&str]) -> String {
    common::git(dir, args)
}

fn write(dir: &Path, rel: &str, text: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// Stages everything in `dir`, commits with a throwaway identity and returns
/// the new HEAD.
fn commit_all(dir: &Path, msg: &str) -> String {
    git(dir, &["add", "-A", "--", "."]);
    git(dir, &["-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

fn merge_main(wt: &Path) {
    git(wt, &["-c", "user.email=t@example.com", "-c", "user.name=t", "merge", "--no-edit", "-q", "main"]);
}

/// Twenty numbered lines, so edits at line 2 and line 18 merge cleanly.
fn shared_text(main_edit: bool, worker_edit: bool) -> String {
    (1..=20)
        .map(|i| match i {
            2 if main_edit => format!("{MAIN_LINE}\n"),
            18 if worker_edit => format!("{WORKER_LINE}\n"),
            _ => format!("line {i:02}\n"),
        })
        .collect()
}

/// A fixture repo with A, B and a file to restore committed on `main`,
/// initialised, with profile claude-b.
fn setup() -> Fixture {
    let f = common::fixture();
    let repo = f.repo_path();
    write(&repo, SHARED, &shared_text(false, false));
    write(&repo, WORKER_ONLY, "pub fn worker() -> u8 { 1 }\n");
    write(&repo, "src/restored.rs", "pub fn restored() -> u8 { 1 }\n");
    commit_all(&repo, "fixture files");
    let mut c = thoth(&f);
    c.arg("init");
    ok(c, "init");
    let cfg = f.home_path().join("profiles").join("claude-b");
    std::fs::create_dir_all(&cfg).unwrap();
    let mut c = thoth(&f);
    c.args(["profile", "add", "claude-b", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&cfg)
        .arg("--json");
    ok_json(c, "profile add");
    f
}

/// `task create [--base REF] -f SPEC --json` with the normal-risk two-AC
/// spec; returns (task_id, base_commit).
fn create_task(f: &Fixture, base: Option<&str>) -> (String, String) {
    let spec = f.home_path().join("spec.json");
    std::fs::write(&spec, Fixture::SPEC_TWO_AC).unwrap();
    let mut c = thoth(f);
    c.args(["task", "create"]);
    if let Some(b) = base {
        c.args(["--base", b]);
    }
    c.arg("-f").arg(&spec).arg("--json");
    let v = ok_json(c, "task create");
    (v["task_id"].as_str().unwrap().to_string(), v["base_commit"].as_str().unwrap().to_string())
}

/// `dispatch TASK` with a fake-claude scenario and extra environment.
fn dispatch(f: &Fixture, task: &str, scenario: &str, env: &[(&str, &str)]) -> Value {
    let mut c = thoth(f);
    c.env("FAKE_SCENARIO", scenario).env("FAKE_TASK_ID", task);
    for (k, v) in env {
        c.env(k, v);
    }
    c.args(["dispatch", task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    ok_json(c, "dispatch")
}

/// `resume RUN --json` with the fake's `resume` scenario.
fn resume(f: &Fixture, task: &str, run: &str) {
    let mut c = thoth(f);
    c.env("FAKE_SCENARIO", "resume").env("FAKE_TASK_ID", task).args(["resume", run, "--json"]);
    ok_json(c, "resume");
}

/// `wait TASK --json` until a wait-terminal status, which it returns.
fn wait(f: &Fixture, task: &str) -> String {
    let mut c = thoth(f);
    c.args(["wait", task, "--timeout", "180s", "--json"]);
    ok_json(c, "wait")["status"].as_str().unwrap().to_string()
}

fn s(v: &Value) -> String {
    v.as_str().unwrap_or_else(|| panic!("not a string: {v}")).to_string()
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array().unwrap_or_else(|| panic!("not an array: {v}")).iter().map(s).collect()
}

/// The newest diff evidence row of `run`, from `evidence list --json`.
fn newest_diff(f: &Fixture, task: &str, run: &str) -> Value {
    let mut c = thoth(f);
    c.args(["evidence", "list", task, "--json"]);
    let v = ok_json(c, "evidence list");
    v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .rfind(|e| e["kind"] == "diff" && e["run_id"] == run)
        .cloned()
        .unwrap_or_else(|| panic!("no diff row for run {run}: {v:#}"))
}

/// `evidence show CODE --task TASK --bytes N --json`, whole.
fn show(f: &Fixture, task: &str, code: &str) -> Value {
    let mut c = thoth(f);
    c.args(["evidence", "show", code, "--task", task, "--bytes", "100000000", "--json"]);
    let v = ok_json(c, "evidence show");
    assert_eq!(v["content_truncated"], false, "{v:#}");
    v
}

/// Records a valid REJECT covering both criteria of the fixture spec.
fn record_reject(f: &Fixture, task: &str) {
    let review = r#"{"verdict":"REJECT","findings":{"criteria":[
        {"id":"AC1","verdict":"unmet","note":"needs another pass"},
        {"id":"AC2","verdict":"met","evidence":["E001"]}
    ],"extra_findings":[]}}"#;
    let file = f.home_path().join("review-reject.json");
    std::fs::write(&file, review).unwrap();
    let mut c = thoth(f);
    c.args(["review", "record", task, "-f"]).arg(&file).arg("--json");
    let v = ok_json(c, "review record");
    assert_eq!(v["task_status"], "reject", "{v}");
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Case (a): the worker's interrupted worktree edits A (line 18) and B,
/// then merges a main line that edited A (line 2) and added T; the same
/// attempt then completes. Returns (fixture, task, run, main-line commit).
fn merged_first_round() -> (Fixture, String, String, String) {
    let f = setup();
    let repo = f.repo_path();
    let (task, _base) = create_task(&f, None);
    let d1 = dispatch(&f, &task, "crash", &[]);
    assert_eq!(wait(&f, &task), "interrupted");
    let wt = PathBuf::from(s(&d1["worktree"]));

    write(&repo, SHARED, &shared_text(true, false));
    write(&repo, MAIN_TEST, "#[test]\nfn main_line() {}\n");
    let main_commit = commit_all(&repo, "main line: A line 2, add T");

    write(&wt, SHARED, &shared_text(false, true));
    write(&wt, WORKER_ONLY, "pub fn worker() -> u8 { 2 }\n");
    commit_all(&wt, "worker: A line 18 and B");
    merge_main(&wt);

    let d2 = dispatch(&f, &task, "hooks", &[]);
    assert_eq!(d2["worktree_reused"], true, "{d2}");
    assert_eq!(d2["attempt"], 1, "{d2}");
    let run = s(&d2["run_id"]);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    (f, task, run, main_commit)
}

/// AC5 (a): after a first-round merge of the main line, the diff starts at
/// the merge base, leaves out T and keeps only the worker's hunks of A.
#[test]
fn a_first_round_merge_of_main_starts_the_diff_at_the_merge_base() {
    let (f, task, run, main_commit) = merged_first_round();
    let row = newest_diff(&f, &task, &run);
    let d = &row["detail"];
    let cand = s(&d["to_commit"]);
    assert_eq!(d["diff_from_rule"], "merge_base", "{d:#}");
    assert_eq!(d["from_commit"], main_commit.as_str(), "{d:#}");
    assert_ne!(d["run_base_commit"], d["from_commit"], "{d:#}");
    assert_eq!(d["integrated_files"], json!([MAIN_TEST]), "{d:#}");
    // the hooks worker adds src/added.rs and tests/added_test.rs itself
    assert_eq!(
        sorted(strings(&d["files_changed"])),
        ["src/added.rs", SHARED, WORKER_ONLY, "tests/added_test.rs"],
        "{d:#}"
    );
    assert_eq!(d["touches_tests"], true);
    assert!(!s(&d["stat"]).contains(MAIN_TEST), "{d:#}");
    assert_eq!(
        s(&row["label"]),
        format!("git diff {}..{}", &main_commit[..8], &cand[..8])
    );
    let shown = show(&f, &task, &s(&row["code"]));
    let patch = s(&shown["content"]);
    assert!(patch.contains(&format!("+{WORKER_LINE}")), "{patch}");
    assert!(!patch.contains(MAIN_LINE), "{patch}");
    assert!(!patch.contains(MAIN_TEST), "{patch}");
    assert!(patch.contains(WORKER_ONLY), "{patch}");
}

/// AC5 (f): both packet formats for case (a).
#[test]
fn the_packet_explains_where_a_merged_diff_starts_and_what_it_leaves_out() {
    let (f, task, run, main_commit) = merged_first_round();
    let row = newest_diff(&f, &task, &run);
    let cand = s(&row["detail"]["to_commit"]);

    let mut c = thoth(&f);
    c.args(["review", "packet", &task, "--json"]);
    let v = ok_json(c, "review packet --json");
    let entry = v["diffs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == row["code"])
        .cloned()
        .unwrap_or_else(|| panic!("no packet diff {}: {v:#}", row["code"]));
    assert_eq!(entry["from_commit"], main_commit.as_str());
    assert_eq!(entry["to_commit"], cand.as_str());
    assert_eq!(entry["diff_from_rule"], "merge_base");
    assert_eq!(entry["integrated_files"], json!([MAIN_TEST]));
    let content = s(&entry["content"]);
    assert!(content.contains(&format!("+{WORKER_LINE}")), "{content}");
    assert!(!content.contains(MAIN_LINE) && !content.contains(MAIN_TEST), "{content}");

    let mut c = thoth(&f);
    c.args(["review", "packet", &task]);
    let md = ok(c, "review packet");
    let heading = format!("### {} (run {run}", s(&row["code"]));
    let at = md.find(&heading).unwrap_or_else(|| panic!("no heading {heading}:\n{md}"));
    let after = &md[at..];
    let line = after.lines().nth(2).unwrap();
    assert_eq!(
        line,
        format!(
            "From {} to {} (start: merge base with the main worktree); 1 file(s) brought in by merging the main line are left out: {MAIN_TEST}",
            &main_commit[..8],
            &cand[..8]
        ),
        "{after}"
    );
    let diffs_section = &md[md.find("## Diffs").unwrap()..md.find("## Worker claims").unwrap()];
    assert!(diffs_section.contains(&format!("+{WORKER_LINE}")), "{diffs_section}");
    assert!(!diffs_section.contains(MAIN_LINE), "{diffs_section}");
    assert!(!diffs_section.contains(&format!("+++ b/{MAIN_TEST}")), "{diffs_section}");
}

/// AC5 (b): the main line moved but the worker never merged it: the diff
/// is today's, byte for byte, including a file that is not UTF-8.
#[test]
fn without_a_merge_the_diff_is_gits_own_from_the_run_base() {
    let f = setup();
    let repo = f.repo_path();
    let (task, base) = create_task(&f, None);
    let d1 = dispatch(&f, &task, "crash", &[]);
    assert_eq!(wait(&f, &task), "interrupted");
    let wt = PathBuf::from(s(&d1["worktree"]));

    write(&repo, SHARED, &shared_text(true, false));
    write(&repo, MAIN_TEST, "#[test]\nfn main_line() {}\n");
    commit_all(&repo, "main line moves");
    write(&wt, SHARED, &shared_text(false, true));
    write(&wt, WORKER_ONLY, "pub fn worker() -> u8 { 3 }\n");
    commit_all(&wt, "worker edits, no merge");

    let d2 = dispatch(&f, &task, "hooks", &[("FAKE_COMMIT_NON_UTF8", "1")]);
    let run = s(&d2["run_id"]);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));

    let row = newest_diff(&f, &task, &run);
    let d = &row["detail"];
    let cand = s(&d["to_commit"]);
    assert_eq!(d["diff_from_rule"], "run_base", "{d:#}");
    assert_eq!(d["from_commit"], base.as_str());
    assert_eq!(d["run_base_commit"], base.as_str());
    assert_eq!(d["integrated_files"], json!([]));
    assert!(!d["integration_tip"].is_null(), "the tip was usable: {d:#}");

    let range = format!("{base}..{cand}");
    let names: Vec<String> = git_bytes(&repo, &["diff", "--name-only", "-z", &range])
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).to_string())
        .collect();
    assert_eq!(strings(&d["files_changed"]), names);
    assert!(names.iter().any(|n| n == "src/legacy.txt"), "{names:?}");
    let stat = String::from_utf8(git_bytes(&repo, &["diff", "--stat", &range])).unwrap();
    assert_eq!(s(&d["stat"]), stat.trim_end());

    let reference = git_bytes(&repo, &["diff", "--patch", "--stat", &range]);
    assert!(reference.windows(4).any(|w| w == b"\xa4\xa4\xa4\xe5"), "the reference holds the non-UTF-8 bytes");
    assert_eq!(row["bytes"], json!(reference.len()));
    use sha2::{Digest, Sha256};
    let sha: String = Sha256::digest(&reference).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(row["sha256"], sha.as_str());
    let shown = show(&f, &task, &s(&row["code"]));
    assert_eq!(shown["blob_verified"], true, "{shown:#}");
    let patch = s(&shown["content"]);
    assert!(patch.contains(&format!("+{WORKER_LINE}")), "{patch}");
    assert!(!patch.contains(MAIN_LINE), "{patch}");
}

/// How `path` can appear in a stat or a patch: as written, and as git's
/// C-quoted form (core.quotepath, on by default, writes each byte from 0x80
/// up as a backslash and three octal digits).
fn printed_forms(path: &str) -> Vec<String> {
    let quoted: String = path
        .bytes()
        .map(|b| if b >= 0x80 { format!("\\{b:03o}") } else { (b as char).to_string() })
        .collect();
    if quoted == path {
        vec![path.to_string()]
    } else {
        vec![path.to_string(), quoted]
    }
}

/// Cases (c) and (g): attempt 1 completes and is rejected, the main line
/// moves, the rework (from the previous candidate) crashes, merges the main
/// line and commits its own edits, and is resumed to completion. The main
/// line also adds each of `extra_main`, and the rework each of
/// `extra_worker`.
fn rework_that_merges_main(extra_main: &[&str], extra_worker: &[&str]) {
    let f = setup();
    let repo = f.repo_path();
    let (task, _base) = create_task(&f, None);
    let d1 = dispatch(&f, &task, "hooks", &[]);
    let run1 = s(&d1["run_id"]);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let previous = s(&newest_diff(&f, &task, &run1)["detail"]["to_commit"]);
    record_reject(&f, &task);

    write(&repo, "a.txt", "one\nmain line\n");
    write(&repo, "tests/main_rework_test.rs", "#[test]\nfn main_rework() {}\n");
    let mut main_only = vec!["a.txt".to_string(), "tests/main_rework_test.rs".to_string()];
    for p in extra_main {
        write(&repo, p, "pub fn from_the_main_line() {}\n");
        main_only.push(p.to_string());
    }
    commit_all(&repo, "main line moves on");

    let d2 = dispatch(&f, &task, "crash", &[]);
    assert_eq!(d2["base_commit"], previous.as_str(), "{d2}");
    let run2 = s(&d2["run_id"]);
    assert_eq!(wait(&f, &task), "interrupted");
    let wt = PathBuf::from(s(&d2["worktree"]));
    merge_main(&wt);
    write(&wt, WORKER_ONLY, "pub fn worker() -> u8 { 4 }\n");
    let mut own = vec![WORKER_ONLY.to_string()];
    for p in extra_worker {
        write(&wt, p, "pub fn from_the_rework() {}\n");
        own.push(p.to_string());
    }
    commit_all(&wt, "rework edits");

    resume(&f, &task, &run2);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));

    let row = newest_diff(&f, &task, &run2);
    let d = &row["detail"];
    assert_eq!(d["diff_from_rule"], "run_base", "{d:#}");
    assert_eq!(d["run_base_commit"], previous.as_str(), "{d:#}");
    assert_eq!(d["from_commit"], previous.as_str(), "{d:#}");
    assert_eq!(strings(&d["integrated_files"]), sorted(main_only.clone()), "{d:#}");
    let files = strings(&d["files_changed"]);
    let stat = s(&d["stat"]);
    let shown = show(&f, &task, &s(&row["code"]));
    let patch = s(&shown["content"]);
    for p in &main_only {
        assert!(!files.contains(p), "{p} in files_changed: {d:#}");
        for form in printed_forms(p) {
            assert!(!stat.contains(form.as_str()), "{form} in stat: {stat}");
            assert!(!patch.contains(&format!("b/{form}")), "{form} in the patch: {patch}");
        }
    }
    assert!(!patch.contains("main line"), "{patch}");
    assert!(!patch.contains("from_the_main_line"), "{patch}");
    assert_eq!(sorted(files), sorted(own.clone()), "{d:#}");
    for p in &own {
        assert!(stat.contains(p.as_str()), "{p} not in stat: {stat}");
        assert!(patch.contains(&format!("b/{p}")), "{p} not in the patch: {patch}");
    }
}

/// AC5 (c): a rework round that merges the main line keeps the run's base
/// as its start and leaves out the paths only the main line changed.
#[test]
fn a_rework_that_merges_main_leaves_out_the_main_lines_own_paths() {
    rework_that_merges_main(&[], &[]);
}

/// AC5 (g): an integrated path with a space and `[` is matched literally,
/// and the worker path a glob reading would match stays in the diff.
#[test]
fn an_integrated_path_with_glob_characters_is_matched_literally() {
    rework_that_merges_main(&[SPECIAL], &[GLOB_MATCH]);
}

/// AC2: a main-line-only path that holds U+FFFD is valid UTF-8, so it is
/// left out like any other; only a path git prints in bytes that are not
/// UTF-8 stays in the diff.
#[test]
fn an_integrated_path_holding_the_replacement_character_is_left_out() {
    rework_that_merges_main(&["src/\u{FFFD}.rs"], &[]);
}

/// AC5 (d): a task on a branch the main worktree's HEAD has not merged has
/// no integration tip, so nothing is attributed.
#[test]
fn a_task_on_an_unmerged_branch_has_no_tip_and_nothing_left_out() {
    let f = setup();
    let repo = f.repo_path();
    git(&repo, &["checkout", "-q", "-b", "side"]);
    write(&repo, "src/side.rs", "pub fn side() {}\n");
    let side = commit_all(&repo, "side branch");
    git(&repo, &["checkout", "-q", "main"]);
    let (task, base) = create_task(&f, Some("side"));
    assert_eq!(base, side);
    let d = dispatch(&f, &task, "hooks", &[]);
    let run = s(&d["run_id"]);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let row = newest_diff(&f, &task, &run);
    let d = &row["detail"];
    assert!(d["integration_tip"].is_null(), "{d:#}");
    assert_eq!(d["diff_from_rule"], "run_base");
    assert_eq!(d["from_commit"], base.as_str());
    assert_eq!(d["run_base_commit"], base.as_str());
    assert_eq!(d["integrated_files"], json!([]));
    assert_eq!(sorted(strings(&d["files_changed"])), ["src/added.rs", "tests/added_test.rs"]);
    let shown = assert_blob_is_gits_diff(&f, &task, &row, &base);
    let patch = s(&shown["content"]);
    assert!(patch.contains("b/src/added.rs") && patch.contains("b/tests/added_test.rs"), "{patch}");
}

/// Requires the diff row's blob to be, byte for byte, what `git diff
/// --patch --stat FROM..CANDIDATE` prints in the fixture repo (its length
/// and SHA-256 against the row's `bytes` and `sha256`), and `evidence show`
/// to verify it; returns the `evidence show --json` response.
fn assert_blob_is_gits_diff(f: &Fixture, task: &str, row: &Value, from: &str) -> Value {
    use sha2::{Digest, Sha256};
    let range = format!("{from}..{}", s(&row["detail"]["to_commit"]));
    let reference = git_bytes(&f.repo_path(), &["diff", "--patch", "--stat", &range]);
    assert_eq!(row["bytes"], json!(reference.len()), "{row:#}");
    let sha: String = Sha256::digest(&reference).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(row["sha256"], sha.as_str(), "{row:#}");
    let shown = show(f, task, &s(&row["code"]));
    assert_eq!(shown["blob_verified"], true, "{shown:#}");
    shown
}

/// Stages nothing new: commits what is in the index (so an index-only
/// mode, symlink or gitlink entry is kept) and returns the new HEAD.
fn commit_staged(dir: &Path, msg: &str) -> String {
    git(dir, &["-c", "user.email=t@example.com", "-c", "user.name=t", "commit", "-q", "-m", msg]);
    git(dir, &["rev-parse", "HEAD"])
}

/// Writes `bytes` as a blob and returns its id.
fn hash_object(dir: &Path, bytes: &[u8]) -> String {
    use std::io::Write as _;
    let mut child = std::process::Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .current_dir(dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// AC2: equal means the same tree entry. The main line changes the content
/// of two files, deletes one and adds a gitlink; the worker fast-forwards to
/// it and then changes only the mode of one file and the type of another
/// (a symlink). Those two stay in the diff; the deletion and the gitlink,
/// as the main line made them, are left out.
#[test]
fn only_a_path_whose_tree_entry_equals_the_tips_is_left_out() {
    let f = setup();
    let repo = f.repo_path();
    write(&repo, "src/moded.rs", "one\n");
    write(&repo, "src/linked.rs", "one\n");
    write(&repo, "src/gone.rs", "gone\n");
    commit_all(&repo, "files the main line will change");
    let (task, base) = create_task(&f, None);
    let d1 = dispatch(&f, &task, "crash", &[]);
    let run = s(&d1["run_id"]);
    assert_eq!(wait(&f, &task), "interrupted");
    let wt = PathBuf::from(s(&d1["worktree"]));

    write(&repo, "src/moded.rs", "two\n");
    write(&repo, "src/linked.rs", "two\n");
    git(&repo, &["rm", "-q", "src/gone.rs"]);
    git(&repo, &["add", "-A", "--", "."]);
    git(&repo, &["update-index", "--add", "--cacheinfo", &format!("160000,{base},vendor/sub")]);
    let tip = commit_staged(&repo, "main line: content, a deletion and a gitlink");
    std::fs::create_dir_all(repo.join("vendor").join("sub")).unwrap();

    merge_main(&wt);
    assert_eq!(git(&wt, &["rev-parse", "HEAD"]), tip, "a fast-forward");
    git(&wt, &["update-index", "--chmod=+x", "src/moded.rs"]);
    let target = hash_object(&wt, b"moded.rs");
    git(&wt, &["update-index", "--cacheinfo", &format!("120000,{target},src/linked.rs")]);
    let cand = commit_staged(&wt, "worker: a mode and a type");

    // `resume` commits nothing, so the index-only entries stay as committed
    resume(&f, &task, &run);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let row = newest_diff(&f, &task, &run);
    let d = &row["detail"];
    assert_eq!(d["to_commit"], cand.as_str(), "{d:#}");
    assert_eq!(d["diff_from_rule"], "merge_base", "{d:#}");
    assert_eq!(d["from_commit"], tip.as_str(), "{d:#}");
    assert_eq!(d["run_base_commit"], base.as_str(), "{d:#}");
    assert_eq!(d["integrated_files"], json!(["src/gone.rs", "vendor/sub"]), "{d:#}");
    assert_eq!(d["files_changed"], json!(["src/linked.rs", "src/moded.rs"]), "{d:#}");
    let stat = s(&d["stat"]);
    assert!(!stat.contains("vendor/sub") && !stat.contains("gone.rs"), "{stat}");
    let patch = s(&show(&f, &task, &s(&row["code"]))["content"]);
    assert!(patch.contains("new mode 100755"), "{patch}");
    assert!(patch.contains("new file mode 120000"), "{patch}");
    assert!(!patch.contains("vendor/sub"), "{patch}");
    assert!(!patch.contains("gone.rs"), "{patch}");
}

/// AC1: when a git step of the attributed view fails, the row is today's
/// whole diff from the run's base. Here the root tree of the merge base is
/// removed from the object store after the worker merged it; only the
/// attributed diff (merge base to candidate) reads that tree.
#[test]
fn a_failing_attribution_step_falls_back_to_the_whole_diff_from_the_run_base() {
    let f = setup();
    let repo = f.repo_path();
    let (task, base) = create_task(&f, None);
    let d1 = dispatch(&f, &task, "crash", &[]);
    let run = s(&d1["run_id"]);
    assert_eq!(wait(&f, &task), "interrupted");
    let wt = PathBuf::from(s(&d1["worktree"]));

    write(&repo, MAIN_TEST, "#[test]\nfn main_line() {}\n");
    let m1 = commit_all(&repo, "main line 1: add T");
    write(&repo, "src/main_two.rs", "pub fn two() {}\n");
    let m2 = commit_all(&repo, "main line 2");

    write(&wt, WORKER_ONLY, "pub fn worker() -> u8 { 5 }\n");
    commit_all(&wt, "worker: B");
    git(&wt, &["-c", "user.email=t@example.com", "-c", "user.name=t", "merge", "--no-edit", "-q", &m1]);
    let cand = git(&wt, &["rev-parse", "HEAD"]);
    // with every object present the diff would start at m1 (AC1): the merge
    // base with the tip is m1, which is not the run's base and contains it
    assert_eq!(git(&repo, &["merge-base", &cand, &m2]), m1);
    assert_ne!(m1, base);
    git(&repo, &["merge-base", "--is-ancestor", &base, &m1]);

    let tree = git(&repo, &["rev-parse", &format!("{m1}^{{tree}}")]);
    // the task's worktree is linked to the fixture repo and shares its objects
    let loose = repo.join(".git").join("objects").join(&tree[..2]).join(&tree[2..]);
    assert!(loose.is_file(), "the merge base's tree is a loose object: {}", loose.display());
    let mut perms = std::fs::metadata(&loose).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    perms.set_readonly(false);
    std::fs::set_permissions(&loose, perms).unwrap();
    std::fs::remove_file(&loose).unwrap();

    resume(&f, &task, &run);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let row = newest_diff(&f, &task, &run);
    let d = &row["detail"];
    assert_eq!(d["to_commit"], cand.as_str(), "{d:#}");
    assert_eq!(d["diff_from_rule"], "run_base", "{d:#}");
    assert_eq!(d["from_commit"], base.as_str(), "{d:#}");
    assert_eq!(d["run_base_commit"], base.as_str(), "{d:#}");
    assert_eq!(d["integrated_files"], json!([]), "{d:#}");
    assert_eq!(d["files_changed"], json!([WORKER_ONLY, MAIN_TEST]), "{d:#}");
    assert_eq!(d["touches_tests"], true, "{d:#}");
    assert_eq!(d["integration_tip"], m2.as_str(), "the protected metadata is unchanged: {d:#}");
    assert_eq!(s(&row["label"]), format!("git diff {}..{}", &base[..8], &cand[..8]));
    let shown = assert_blob_is_gits_diff(&f, &task, &row, &base);
    assert!(s(&shown["content"]).contains(&format!("b/{MAIN_TEST}")), "{shown:#}");
}

/// The `diffs[]` entry of `review packet TASK --json` for evidence `code`.
fn packet_diff(f: &Fixture, task: &str, code: &str) -> Value {
    let mut c = thoth(f);
    c.args(["review", "packet", task, "--json"]);
    let v = ok_json(c, "review packet --json");
    v["diffs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["code"] == code)
        .cloned()
        .unwrap_or_else(|| panic!("no packet diff {code}: {v:#}"))
}

/// AC7: the task's base holds `before` (the files given, all committed on
/// the main line); the main line then removes `main_removes` and writes
/// `main_writes`; the crashed worker commits `worker_writes` on top of a
/// merge of that main line, and the same run is resumed to completion.
/// Returns (fixture, task, run, tip, candidate).
fn kind_change(
    before: &[(&str, &str)],
    main_removes: &str,
    main_writes: &[(&str, &str)],
    worker_writes: &[(&str, &str)],
) -> (Fixture, String, String, String, String) {
    let f = setup();
    let repo = f.repo_path();
    for (p, text) in before {
        write(&repo, p, text);
    }
    commit_all(&repo, "the task's base");
    let (task, _base) = create_task(&f, None);
    let d1 = dispatch(&f, &task, "crash", &[]);
    let run = s(&d1["run_id"]);
    assert_eq!(wait(&f, &task), "interrupted");
    let wt = PathBuf::from(s(&d1["worktree"]));

    git(&repo, &["rm", "-r", "-q", "--", main_removes]);
    for (p, text) in main_writes {
        write(&repo, p, text);
    }
    let tip = commit_all(&repo, "main line: a file becomes a directory or back");

    write(&wt, WORKER_ONLY, "pub fn worker() -> u8 { 6 }\n");
    commit_all(&wt, "worker: B");
    merge_main(&wt);
    for (p, text) in worker_writes {
        write(&wt, p, text);
    }
    let cand = commit_all(&wt, "worker: its own change below the main line's");

    resume(&f, &task, &run);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    (f, task, run, tip, cand)
}

/// AC7: the main line turns file assets/item into a directory holding
/// main.txt, and the worker merges it and adds own.txt there. The directory
/// differs between the tip and the candidate, so only main.txt is left out
/// and own.txt shows in the files, the stat and the patch.
#[test]
fn a_file_the_main_line_turned_into_a_directory_keeps_the_workers_file_in_it() {
    let (f, task, run, tip, cand) = kind_change(
        &[("assets/item", "the old item\n")],
        "assets/item",
        &[("assets/item/main.txt", "from the main line\n")],
        &[("assets/item/own.txt", "from the worker\n")],
    );
    let row = newest_diff(&f, &task, &run);
    let d = &row["detail"];
    assert_eq!(d["to_commit"], cand.as_str(), "{d:#}");
    assert_eq!(d["diff_from_rule"], "merge_base", "{d:#}");
    assert_eq!(d["from_commit"], tip.as_str(), "{d:#}");
    assert_eq!(d["files_changed"], json!(["assets/item/own.txt", WORKER_ONLY]), "{d:#}");
    let stat = s(&d["stat"]);
    assert!(stat.contains("assets/item/own.txt"), "{stat}");
    assert!(!stat.contains("main.txt"), "{stat}");

    let entry = packet_diff(&f, &task, &s(&row["code"]));
    assert_eq!(entry["integrated_files"], json!(["assets/item/main.txt"]), "{entry:#}");
    let content = s(&entry["content"]);
    assert!(content.contains("+++ b/assets/item/own.txt"), "{content}");
    assert!(!content.contains("main.txt") && !content.contains("from the main line"), "{content}");

    let patch = s(&show(&f, &task, &s(&row["code"]))["content"]);
    assert!(patch.contains("+++ b/assets/item/own.txt"), "{patch}");
    assert!(patch.contains("+from the worker"), "{patch}");
    assert!(!patch.contains("main.txt"), "{patch}");
}

/// AC7, the reverse: the main line turns directory assets/item into a file
/// and the worker merges it and changes that file. The file the main line
/// removed is left out; the worker's change to the new file shows.
#[test]
fn a_directory_the_main_line_turned_into_a_file_keeps_the_workers_change_to_it() {
    let (f, task, run, tip, cand) = kind_change(
        &[("assets/item/old.txt", "the old file\n")],
        "assets/item",
        &[("assets/item", "the main line's item\n")],
        &[("assets/item", "the worker's item\n")],
    );
    let row = newest_diff(&f, &task, &run);
    let d = &row["detail"];
    assert_eq!(d["to_commit"], cand.as_str(), "{d:#}");
    assert_eq!(d["diff_from_rule"], "merge_base", "{d:#}");
    assert_eq!(d["from_commit"], tip.as_str(), "{d:#}");
    assert_eq!(d["files_changed"], json!(["assets/item", WORKER_ONLY]), "{d:#}");
    let stat = s(&d["stat"]);
    assert!(stat.contains("assets/item "), "{stat}");
    assert!(!stat.contains("old.txt"), "{stat}");

    let entry = packet_diff(&f, &task, &s(&row["code"]));
    assert_eq!(entry["integrated_files"], json!(["assets/item/old.txt"]), "{entry:#}");
    let content = s(&entry["content"]);
    assert!(content.contains("+the worker's item"), "{content}");
    assert!(!content.contains("old.txt"), "{content}");

    let patch = s(&show(&f, &task, &s(&row["code"]))["content"]);
    assert!(patch.contains("-the main line's item"), "{patch}");
    assert!(patch.contains("+the worker's item"), "{patch}");
    assert!(!patch.contains("old.txt"), "{patch}");
}

/// AC2 and "list more, never fewer": a path the worker's diff cannot leave
/// out without also hiding a worker change below it. The first attempt adds
/// assets/kind/own.txt; the main line adds a file named assets/kind; the
/// rework deletes its directory and merges the main line, so assets/kind
/// equals the tip's while the deletion of own.txt is the worker's own. A
/// path filter would hide both, so the row is today's diff from the run's
/// base.
#[test]
fn a_left_out_path_that_would_hide_a_worker_change_below_it_falls_back_to_the_whole_diff() {
    let f = setup();
    let repo = f.repo_path();
    let (task, _base) = create_task(&f, None);
    let d1 = dispatch(&f, &task, "hooks", &[("FAKE_WRITE_FILE", "assets/kind/own.txt")]);
    let run1 = s(&d1["run_id"]);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let previous = s(&newest_diff(&f, &task, &run1)["detail"]["to_commit"]);
    record_reject(&f, &task);

    write(&repo, "assets/kind", "a file from the main line\n");
    write(&repo, "docs/main_only.md", "main line only\n");
    commit_all(&repo, "main line: a file where the worker had a directory");

    let d2 = dispatch(&f, &task, "crash", &[]);
    assert_eq!(d2["base_commit"], previous.as_str(), "{d2}");
    let run2 = s(&d2["run_id"]);
    assert_eq!(wait(&f, &task), "interrupted");
    let wt = PathBuf::from(s(&d2["worktree"]));
    git(&wt, &["rm", "-r", "-q", "--", "assets/kind"]);
    commit_all(&wt, "rework: drop the directory");
    merge_main(&wt);

    resume(&f, &task, &run2);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let row = newest_diff(&f, &task, &run2);
    let d = &row["detail"];
    assert_eq!(d["run_base_commit"], previous.as_str(), "{d:#}");
    assert_eq!(d["diff_from_rule"], "run_base", "{d:#}");
    assert_eq!(d["from_commit"], previous.as_str(), "{d:#}");
    assert_eq!(d["integrated_files"], json!([]), "{d:#}");
    let files = strings(&d["files_changed"]);
    assert!(files.contains(&"assets/kind/own.txt".to_string()), "{d:#}");
    assert!(files.contains(&"assets/kind".to_string()), "{d:#}");
    let entry = packet_diff(&f, &task, &s(&row["code"]));
    assert_eq!(entry["integrated_files"], json!([]), "{entry:#}");
    let shown = assert_blob_is_gits_diff(&f, &task, &row, &previous);
    let patch = s(&shown["content"]);
    assert!(patch.contains("--- a/assets/kind/own.txt"), "{patch}");
    assert!(patch.contains("-written by the fake worker"), "{patch}");
}

/// AC5 (e): a rework that puts a file back to its task-base content, with
/// an unrelated main-line commit and no merge, keeps that file in the diff.
#[test]
fn a_file_the_rework_restores_to_its_base_content_stays_in_the_diff() {
    let f = setup();
    let repo = f.repo_path();
    let (task, _base) = create_task(&f, None);
    let d1 = dispatch(&f, &task, "hooks", &[("FAKE_WRITE_FILE", "src/restored.rs")]);
    let run1 = s(&d1["run_id"]);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let first = newest_diff(&f, &task, &run1);
    assert!(strings(&first["detail"]["files_changed"]).contains(&"src/restored.rs".to_string()));
    let previous = s(&first["detail"]["to_commit"]);
    record_reject(&f, &task);

    write(&repo, "docs/unrelated.md", "unrelated\n");
    commit_all(&repo, "main line: unrelated");

    let d2 = dispatch(&f, &task, "crash", &[]);
    assert_eq!(d2["base_commit"], previous.as_str(), "{d2}");
    let run2 = s(&d2["run_id"]);
    assert_eq!(wait(&f, &task), "interrupted");
    let wt = PathBuf::from(s(&d2["worktree"]));
    write(&wt, "src/restored.rs", "pub fn restored() -> u8 { 1 }\n");
    commit_all(&wt, "put the file back");

    resume(&f, &task, &run2);
    assert_eq!(wait(&f, &task), "in_review", "{}", f.explain_task(&task));
    let row = newest_diff(&f, &task, &run2);
    let d = &row["detail"];
    assert_eq!(d["run_base_commit"], previous.as_str(), "{d:#}");
    assert_eq!(d["diff_from_rule"], "run_base", "{d:#}");
    assert_eq!(d["integrated_files"], json!([]), "{d:#}");
    assert!(strings(&d["files_changed"]).contains(&"src/restored.rs".to_string()), "{d:#}");
    let patch = s(&show(&f, &task, &s(&row["code"]))["content"]);
    assert!(patch.contains("+pub fn restored() -> u8 { 1 }"), "{patch}");
}
