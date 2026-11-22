//! EVOPEN-1: which evidence a Codex or agy reviewer read, found in its own
//! stream when its review is taken in, so `opened` and `unopened_citation`
//! of `thoth review show` mean the same for all three harnesses. Driven only
//! through the CLI, with fake-claude, fake-codex and fake-agy.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::PathBuf;

const CODEX_MODEL: &str = "gpt-5.6-luna";
const AGY_MODEL: &str = "gemini-3.8-flash-low";
const NEW_WARNING: &str = "(cited but its file in .agent/review-input/evidence/ was never read)";
const STREAM_LINE: &str = "  found in the reviewer's stream:";

#[derive(Clone, Copy, Debug)]
enum Reviewer {
    Codex,
    Agy,
}

impl Reviewer {
    fn harness(self) -> &'static str {
        match self {
            Reviewer::Codex => "codex",
            Reviewer::Agy => "antigravity",
        }
    }

    /// The key that says where in the stream the read was.
    fn place_key(self) -> &'static str {
        match self {
            Reviewer::Codex => "item_id",
            Reviewer::Agy => "step_index",
        }
    }
}

fn run_json(f: &Fixture, args: &[&str]) -> Value {
    let out = f.cmd().args(args).assert().success().get_output().stdout.clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim())
        .unwrap_or_else(|e| panic!("thoth {args:?} printed no JSON ({e})"))
}

fn run_text(f: &Fixture, args: &[&str]) -> String {
    let out = f.cmd().args(args).assert().success().get_output().stdout.clone();
    String::from_utf8(out).unwrap()
}

/// A fixture with claude-b and the reviewer's profile, and a fake-claude
/// `hooks` candidate at `in_review` on a two-criterion spec (codes E001 to
/// E005). Returns (fixture, task_id).
fn candidate(reviewer: Option<Reviewer>) -> (Fixture, String) {
    candidate_with_files(reviewer, &[])
}

/// `candidate`, with `files` (path, content) committed to the fixture
/// repository before the task is created.
fn candidate_with_files(reviewer: Option<Reviewer>, files: &[(&str, &str)]) -> (Fixture, String) {
    let f = common::fixture();
    if !files.is_empty() {
        let repo = f.repo_path();
        for (path, content) in files {
            let file = repo.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, content).unwrap();
            common::git(&repo, &["add", path]);
        }
        common::git(&repo, &["commit", "-m", "files of the same name elsewhere"]);
    }
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    match reviewer {
        Some(Reviewer::Codex) => {
            f.add_profile_with("codex-a", "openai", "codex");
        }
        Some(Reviewer::Agy) => {
            f.add_profile_with("agy-a", "google", "antigravity");
        }
        None => {}
    }
    let task_id = in_review_task(&f);
    (f, task_id)
}

/// One more fake-claude `hooks` task of the fixture, at `in_review`.
fn in_review_task(f: &Fixture) -> String {
    let (task_id, _) = f.create_task(Fixture::SPEC_TWO_AC);
    f.dispatch(&task_id, "claude-b", "hooks", &[]);
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");
    task_id
}

/// `review start` on the reviewer's profile with `scenario` and `envs`, then
/// `wait --run`. Returns the reviewer run id.
fn review(
    f: &Fixture,
    task_id: &str,
    reviewer: Reviewer,
    scenario: &str,
    envs: &[(&str, &str)],
    extra: &[&str],
) -> String {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario);
    match reviewer {
        Reviewer::Codex => {
            c.args(["review", "start", task_id, "--profile", "codex-a", "--model", CODEX_MODEL]);
            c.args(["--effort", "medium"]);
        }
        Reviewer::Agy => {
            c.env("FAKE_AGY_DATA_DIR", f.home_path().join("profiles").join("agy-a"));
            c.args(["review", "start", task_id, "--profile", "agy-a", "--model", AGY_MODEL]);
        }
    }
    c.args(["--json"]).args(extra);
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.assert().success().get_output().stdout.clone();
    let r: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let run_id = r["run_id"].as_str().unwrap().to_string();
    f.cmd().args(["wait", "--run", &run_id, "--timeout", "180s"]).assert().success();
    run_id
}

fn show_json(f: &Fixture, task_id: &str) -> Value {
    run_json(f, &["review", "show", task_id, "--json"])
}

fn show_human(f: &Fixture, task_id: &str) -> String {
    run_text(f, &["review", "show", task_id])
}

/// The `evidence_opened` events of `run_id`, from `events --task T --json`.
fn opened_events(f: &Fixture, task_id: &str, run_id: &str) -> Vec<Value> {
    f.events_json(task_id)
        .into_iter()
        .filter(|e| e["type"] == "evidence_opened" && e["run_id"] == run_id)
        .collect()
}

/// The kind of evidence `code` of the task, from `evidence list --json`.
fn kind_of(f: &Fixture, task_id: &str, code: &str) -> String {
    f.evidence_rows(task_id)
        .into_iter()
        .find(|r| r["code"] == code)
        .unwrap_or_else(|| panic!("task {task_id} has no evidence {code}"))["kind"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The worktree of the task's first run, from `status T --json`.
fn worktree_of(f: &Fixture, task_id: &str) -> PathBuf {
    let v = run_json(f, &["status", task_id, "--json"]);
    PathBuf::from(v["runs"][0]["worktree"].as_str().unwrap_or_else(|| panic!("{v}")))
}

fn sorted_keys(v: &Value) -> Vec<String> {
    let mut keys: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
    keys.sort();
    keys
}

/// The stream-found event detail's keys for `reviewer`.
fn stream_keys(reviewer: Reviewer) -> Vec<String> {
    let mut keys: Vec<String> = ["code", "kind", "found_in", "harness", "path", reviewer.place_key()]
        .iter()
        .map(|s| s.to_string())
        .collect();
    keys.sort();
    keys
}

/// The run's `runner-error.log`, empty when it has none.
fn runner_log(f: &Fixture, task_id: &str, run_id: &str) -> String {
    let v = run_json(f, &["status", task_id, "--json"]);
    let run = v["runs"].as_array().unwrap().iter().find(|r| r["run_id"] == run_id).cloned().unwrap();
    let dir = PathBuf::from(run["run_dir"].as_str().unwrap());
    std::fs::read_to_string(dir.join("runner-error.log")).unwrap_or_default()
}

// (a) ------------------------------------------------------------------------

fn reads_two_of_three_cited(reviewer: Reviewer) {
    let (f, task_id) = candidate(Some(reviewer));
    let run = review(
        &f,
        &task_id,
        reviewer,
        "review_pass",
        &[("FAKE_REVIEW_READS", "E001,E003"), ("FAKE_REVIEW_CITES", "E001,E002,E003")],
        &[],
    );
    let v = show_json(&f, &task_id);
    assert_eq!(v["reviewer_run_id"], run.as_str(), "{v}");
    assert_eq!(v["cited"], json!(["E001", "E002", "E003"]), "{v}");
    assert_eq!(v["opened"], json!(["E001", "E003"]), "{v}");
    assert_eq!(v["opened_in_stream"], json!(["E001", "E003"]), "{v}");
    assert_eq!(v["unopened_citation"], json!(["E002"]), "{v}");

    let human = show_human(&f, &task_id);
    assert!(human.contains("  opened: E001,E003\n"), "{human}");
    assert!(human.contains(&format!("{STREAM_LINE} E001,E003\n")), "{human}");
    assert!(human.contains(&format!("  WARNING unopened_citation: E002 {NEW_WARNING}")), "{human}");
    assert!(!human.contains("never read through `evidence show`"), "{human}");

    let events = opened_events(&f, &task_id, &run);
    assert_eq!(events.len(), 2, "{events:#?}");
    let places = match reviewer {
        Reviewer::Codex => [json!("item_2"), json!("item_3")],
        Reviewer::Agy => [json!(5), json!(6)],
    };
    for ((event, code), place) in events.iter().zip(["E001", "E003"]).zip(places) {
        let d = &event["detail"];
        assert_eq!(sorted_keys(d), stream_keys(reviewer), "{d}");
        assert_eq!(d["code"], code, "{d}");
        assert_eq!(d["kind"], kind_of(&f, &task_id, code).as_str(), "{d}");
        assert_eq!(d["found_in"], "reviewer_stream", "{d}");
        assert_eq!(d["harness"], reviewer.harness(), "{d}");
        assert_eq!(d["path"], format!(".agent/review-input/evidence/{code}.txt"), "{d}");
        assert_eq!(d[reviewer.place_key()], place, "{d}");
    }

    // The verdict event carries the same lists.
    let verdict = f
        .events_json(&task_id)
        .into_iter()
        .rfind(|e| e["type"] == "verdict" && e["run_id"] == run.as_str())
        .expect("a verdict event of the reviewer run");
    assert_eq!(verdict["detail"]["opened"], json!(["E001", "E003"]), "{verdict}");
    assert_eq!(verdict["detail"]["opened_in_stream"], json!(["E001", "E003"]), "{verdict}");
    assert_eq!(verdict["detail"]["unopened_citation"], json!(["E002"]), "{verdict}");

    // The runner names only the code still unopened.
    let log = runner_log(&f, &task_id, &run);
    assert!(log.contains("the review cites codes it never opened: E002\n"), "{log}");
}

#[test]
fn a_codex_review_that_reads_two_of_three_cited_codes_opens_exactly_those_two() {
    reads_two_of_three_cited(Reviewer::Codex);
}

#[test]
fn an_agy_review_that_reads_two_of_three_cited_codes_opens_exactly_those_two() {
    reads_two_of_three_cited(Reviewer::Agy);
}

// (b) ------------------------------------------------------------------------

fn reads_nothing(reviewer: Reviewer) {
    let (f, task_id) = candidate(Some(reviewer));
    let run = review(
        &f,
        &task_id,
        reviewer,
        "review_pass",
        &[("FAKE_REVIEW_READS", ""), ("FAKE_REVIEW_CITES", "E001,E002")],
        &[],
    );
    let v = show_json(&f, &task_id);
    assert_eq!(v["opened"], json!([]), "{v}");
    assert_eq!(v["opened_in_stream"], json!([]), "{v}");
    assert_eq!(v["unopened_citation"], json!(["E001", "E002"]), "{v}");
    assert_eq!(v["unopened_citation"], v["cited"], "{v}");
    assert!(opened_events(&f, &task_id, &run).is_empty());
    let human = show_human(&f, &task_id);
    assert!(human.contains("  opened: -\n"), "{human}");
    assert!(!human.contains(STREAM_LINE), "{human}");
    assert!(human.contains(&format!("  WARNING unopened_citation: E001,E002 {NEW_WARNING}")), "{human}");
}

#[test]
fn a_codex_review_that_reads_only_the_packet_opens_nothing() {
    reads_nothing(Reviewer::Codex);
}

#[test]
fn an_agy_review_that_reads_only_the_packet_opens_nothing() {
    reads_nothing(Reviewer::Agy);
}

// (c) ------------------------------------------------------------------------

fn reads_outside_its_own_folder(reviewer: Reviewer) {
    // SMALL-18 T5: agy's worktree holds two committed files named E002.txt
    // in other folders, so its reads of them succeed (read_file DONE); a
    // read matched by file name alone would open E002.
    let (f, task_id) = match reviewer {
        Reviewer::Codex => candidate(Some(reviewer)),
        Reviewer::Agy => candidate_with_files(
            Some(reviewer),
            &[("src/E002.txt", "not this run's evidence\n"), ("src/evidence/E002.txt", "not this run's evidence\n")],
        ),
    };
    if matches!(reviewer, Reviewer::Agy) {
        let wt = worktree_of(&f, &task_id);
        for name in ["src/E002.txt", "src/evidence/E002.txt"] {
            assert!(wt.join(name).is_file(), "{name} is not in the reviewed worktree");
        }
    }
    // Another in-review task, whose worktree holds a file at the same
    // relative path, and the fixture's home with one too.
    let other = in_review_task(&f);
    let other_file = worktree_of(&f, &other).join(".agent/review-input/evidence/E002.txt");
    let home_file = f.home_path().join(".agent/review-input/evidence/E002.txt");
    for file in [&other_file, &home_file] {
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, "not this run's evidence\n").unwrap();
    }
    let other_path = other_file.display().to_string();
    let home_path = home_file.display().to_string();
    let extra: Vec<String> = match reviewer {
        Reviewer::Codex => vec![
            format!("/bin/bash -lc 'cat {other_path}'"),
            format!("/bin/bash -lc 'cat {home_path}'"),
            "/bin/bash -lc 'cat src/E002.txt'".to_string(),
            "/bin/bash -lc 'cat src/.agent/review-input/evidence/E002.txt'".to_string(),
            "/bin/bash -lc 'cat .agent/review-input/packet.md'".to_string(),
            "/bin/bash -lc 'cat .agent/review-input/result.json'".to_string(),
        ],
        Reviewer::Agy => vec![
            other_path,
            home_path,
            "src/E002.txt".to_string(),
            "src/evidence/E002.txt".to_string(),
            ".agent/review-input/packet.md".to_string(),
            ".agent/review-input/result.json".to_string(),
        ],
    };
    let extra = serde_json::to_string(&extra).unwrap();
    // agy also reads the file of a code the task does not have.
    let reads = match reviewer {
        Reviewer::Codex => "E001",
        Reviewer::Agy => "E001,E099",
    };
    let run = review(
        &f,
        &task_id,
        reviewer,
        "review_pass",
        &[
            ("FAKE_REVIEW_READS", reads),
            ("FAKE_REVIEW_CITES", "E001,E002"),
            ("FAKE_REVIEW_EXTRA_READS", &extra),
        ],
        &[],
    );
    let v = show_json(&f, &task_id);
    assert_eq!(v["opened"], json!(["E001"]), "{v}");
    assert_eq!(v["opened_in_stream"], json!(["E001"]), "{v}");
    assert_eq!(v["unopened_citation"], json!(["E002"]), "{v}");
    let events = opened_events(&f, &task_id, &run);
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(events[0]["detail"]["code"], "E001");
}

#[test]
fn a_codex_read_outside_its_own_review_input_folder_opens_nothing() {
    reads_outside_its_own_folder(Reviewer::Codex);
}

#[test]
fn an_agy_read_outside_its_own_review_input_folder_opens_nothing() {
    reads_outside_its_own_folder(Reviewer::Agy);
}

// (d) ------------------------------------------------------------------------

#[test]
fn one_codex_command_naming_two_files_opens_both_and_a_second_read_adds_no_event() {
    let reviewer = Reviewer::Codex;
    let (f, task_id) = candidate(Some(reviewer));
    let again = serde_json::to_string(&["/bin/bash -lc 'cat .agent/review-input/evidence/E001.txt'"]).unwrap();
    let run = review(
        &f,
        &task_id,
        reviewer,
        "review_pass",
        &[
            ("FAKE_REVIEW_READS", "E001,E002"),
            ("FAKE_REVIEW_READ_TOGETHER", "1"),
            ("FAKE_REVIEW_CITES", "E001,E002"),
            ("FAKE_REVIEW_EXTRA_READS", &again),
        ],
        &[],
    );
    let v = show_json(&f, &task_id);
    assert_eq!(v["opened"], json!(["E001", "E002"]), "{v}");
    assert_eq!(v["unopened_citation"], json!([]), "{v}");
    let events = opened_events(&f, &task_id, &run);
    assert_eq!(events.len(), 2, "{events:#?}");
    for (event, code) in events.iter().zip(["E001", "E002"]) {
        assert_eq!(event["detail"]["code"], code, "{event}");
        // both from the one command, the first read in the stream
        assert_eq!(event["detail"]["item_id"], "item_2", "{event}");
    }
    let human = show_human(&f, &task_id);
    assert!(!human.contains("WARNING unopened_citation"), "{human}");
}

// (e) ------------------------------------------------------------------------

#[test]
fn a_codex_read_written_with_a_dot_counts_and_listings_globs_and_lookalikes_do_not() {
    let reviewer = Reviewer::Codex;
    let (f, task_id) = candidate(Some(reviewer));
    let extra = serde_json::to_string(&[
        "/bin/bash -lc 'ls .agent/review-input/evidence'",
        "/bin/bash -lc 'cat .agent/review-input/evidence/*.txt'",
        "/bin/bash -lc 'cat .agent/review-input/evidence/E00?.txt'",
        "/bin/bash -lc 'cd .agent/review-input/evidence && cat E002.txt'",
        "/bin/bash -lc 'cat .agent/review-input/evidence/E0021.txt'",
        "/bin/bash -lc 'cat .agent/review-input/evidence/E002.txt.bak'",
        "/bin/bash -lc 'D=.agent/review-input/evidence; cat $D/E002.txt'",
    ])
    .unwrap();
    let run = review(
        &f,
        &task_id,
        reviewer,
        "review_pass",
        &[
            ("FAKE_REVIEW_READS", "E001"),
            ("FAKE_REVIEW_READ_FORM", "dot"),
            ("FAKE_REVIEW_CITES", "E001,E002"),
            ("FAKE_REVIEW_EXTRA_READS", &extra),
        ],
        &[],
    );
    let v = show_json(&f, &task_id);
    assert_eq!(v["opened"], json!(["E001"]), "{v}");
    assert_eq!(v["unopened_citation"], json!(["E002"]), "{v}");
    let events = opened_events(&f, &task_id, &run);
    assert_eq!(events.len(), 1, "{events:#?}");
    // the relative form with `/`, whatever form the stream used
    assert_eq!(events[0]["detail"]["path"], ".agent/review-input/evidence/E001.txt");
}

#[test]
fn a_codex_read_written_as_the_worktrees_absolute_path_counts() {
    let reviewer = Reviewer::Codex;
    let (f, task_id) = candidate(Some(reviewer));
    let run = review(
        &f,
        &task_id,
        reviewer,
        "review_pass",
        &[
            ("FAKE_REVIEW_READS", "E002"),
            ("FAKE_REVIEW_READ_FORM", "absolute"),
            ("FAKE_REVIEW_CITES", "E002"),
        ],
        &[],
    );
    let v = show_json(&f, &task_id);
    assert_eq!(v["opened"], json!(["E002"]), "{v}");
    assert_eq!(v["unopened_citation"], json!([]), "{v}");
    let events = opened_events(&f, &task_id, &run);
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(events[0]["detail"]["path"], ".agent/review-input/evidence/E002.txt");
}

#[test]
fn a_codex_read_written_with_backslashes_counts_only_on_windows() {
    let reviewer = Reviewer::Codex;
    let (f, task_id) = candidate(Some(reviewer));
    let run = review(
        &f,
        &task_id,
        reviewer,
        "review_pass",
        &[
            ("FAKE_REVIEW_READS", "E002"),
            ("FAKE_REVIEW_READ_FORM", "backslash"),
            ("FAKE_REVIEW_CITES", "E002"),
        ],
        &[],
    );
    let v = show_json(&f, &task_id);
    let events = opened_events(&f, &task_id, &run);
    if cfg!(windows) {
        assert_eq!(v["opened"], json!(["E002"]), "{v}");
        assert_eq!(v["unopened_citation"], json!([]), "{v}");
        assert_eq!(events.len(), 1, "{events:#?}");
        assert_eq!(events[0]["detail"]["path"], ".agent/review-input/evidence/E002.txt");
    } else {
        assert_eq!(v["opened"], json!([]), "{v}");
        assert_eq!(v["unopened_citation"], json!(["E002"]), "{v}");
        assert!(events.is_empty(), "{events:#?}");
    }
}

// (f) ------------------------------------------------------------------------

#[test]
fn a_no_edit_codex_review_counts_its_reads_as_an_edit_mode_review_does() {
    let reviewer = Reviewer::Codex;
    let (f, task_id) = candidate(Some(reviewer));
    let run = review(
        &f,
        &task_id,
        reviewer,
        "review_pass",
        &[("FAKE_REVIEW_READS", "E001,E003"), ("FAKE_REVIEW_CITES", "E001,E002,E003")],
        &["--no-edit"],
    );
    let v = show_json(&f, &task_id);
    assert_eq!(v["mode"], "review", "{v}");
    assert_eq!(v["opened"], json!(["E001", "E003"]), "{v}");
    assert_eq!(v["opened_in_stream"], json!(["E001", "E003"]), "{v}");
    assert_eq!(v["unopened_citation"], json!(["E002"]), "{v}");
    assert_eq!(opened_events(&f, &task_id, &run).len(), 2);
}

// (g) ------------------------------------------------------------------------

#[test]
fn a_refused_codex_review_still_gets_its_stream_found_events() {
    let reviewer = Reviewer::Codex;
    let (f, task_id) = candidate(Some(reviewer));
    let run = review(&f, &task_id, reviewer, "review_bad", &[("FAKE_REVIEW_READS", "E001,E002")], &[]);
    assert_eq!(f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30), "unverified");
    let all = f.events_json(&task_id);
    assert!(
        all.iter().any(|e| e["type"] == "review_invalid" && e["run_id"] == run.as_str()),
        "{all:#?}"
    );
    let events = opened_events(&f, &task_id, &run);
    let codes: Vec<&Value> = events.iter().map(|e| &e["detail"]["code"]).collect();
    assert_eq!(codes, [&json!("E001"), &json!("E002")], "{events:#?}");
    assert!(events.iter().all(|e| e["detail"]["found_in"] == "reviewer_stream"), "{events:#?}");
}

// (h) ------------------------------------------------------------------------

#[test]
fn an_agy_review_reading_with_view_file_counts() {
    let reviewer = Reviewer::Agy;
    let (f, task_id) = candidate(Some(reviewer));
    let run = review(
        &f,
        &task_id,
        reviewer,
        "review_pass",
        &[
            ("FAKE_REVIEW_READS", "E001,E002"),
            ("FAKE_REVIEW_READ_TOOL", "view_file"),
            ("FAKE_REVIEW_CITES", "E001,E002"),
        ],
        &[],
    );
    let v = show_json(&f, &task_id);
    assert_eq!(v["opened"], json!(["E001", "E002"]), "{v}");
    assert_eq!(v["opened_in_stream"], json!(["E001", "E002"]), "{v}");
    assert_eq!(v["unopened_citation"], json!([]), "{v}");
    let events = opened_events(&f, &task_id, &run);
    assert_eq!(events.len(), 2, "{events:#?}");
    assert_eq!(events[0]["detail"]["step_index"], 5, "{events:#?}");
    assert_eq!(events[1]["detail"]["step_index"], 6, "{events:#?}");
}

// (i) ------------------------------------------------------------------------

#[test]
fn a_claude_reviewer_records_what_it_always_did() {
    let (f, task_id) = candidate(None);
    let r = f.review_start(&task_id, "claude-b", "review_pass", &["--model", "haiku"]);
    let run = r["run_id"].as_str().unwrap().to_string();
    f.cmd().args(["wait", "--run", &run, "--timeout", "180s"]).assert().success();
    let v = show_json(&f, &task_id);
    assert_eq!(v["opened"], json!(["E001"]), "{v}");
    assert_eq!(v["opened_in_stream"], json!([]), "{v}");
    let events = opened_events(&f, &task_id, &run);
    assert_eq!(events.len(), 1, "{events:#?}");
    assert_eq!(
        sorted_keys(&events[0]["detail"]),
        ["blob_verified", "bytes_shown", "bytes_total", "code", "kind"],
        "{events:#?}"
    );
    let human = show_human(&f, &task_id);
    assert!(human.contains("  opened: E001\n"), "{human}");
    assert!(!human.contains(STREAM_LINE), "{human}");
}

// (j) ------------------------------------------------------------------------

#[test]
fn an_external_review_has_no_opened_in_stream() {
    let (f, task_id) = candidate(None);
    let file = f.home_path().join("review.json");
    std::fs::write(
        &file,
        r#"{"verdict":"PASS","findings":{"criteria":[
            {"id":"AC1","verdict":"met","evidence":["E001"]},
            {"id":"AC2","verdict":"met","evidence":["E001"]}
        ],"extra_findings":[]}}"#,
    )
    .unwrap();
    let file = file.display().to_string();
    run_json(&f, &["review", "record", &task_id, "-f", &file, "--json"]);
    let v = show_json(&f, &task_id);
    assert_eq!(v["source"], "external", "{v}");
    assert_eq!(v["opened"], Value::Null, "{v}");
    assert_eq!(v["unopened_citation"], Value::Null, "{v}");
    assert_eq!(v["opened_in_stream"], Value::Null, "{v}");
    let human = show_human(&f, &task_id);
    assert!(human.contains("  opened: n/a (external review)"), "{human}");
    assert!(!human.contains(STREAM_LINE), "{human}");
}

#[test]
fn a_task_with_no_review_shows_an_empty_opened_in_stream() {
    let (f, task_id) = candidate(None);
    let v = show_json(&f, &task_id);
    assert_eq!(v["review_count"], 0, "{v}");
    assert_eq!(v["opened_in_stream"], json!([]), "{v}");
}
