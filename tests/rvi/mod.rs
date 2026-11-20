//! RVI-1: the codex and antigravity reviewers read the delivered result
//! (`.agent/review-input/result.json`) and are told what a silent check and
//! linux-container prove. Shared by `tests/codex.rs` and
//! `tests/antigravity.rs`, which run every case once per reviewer harness.
//!
//! Only the CLI and the files the fakes consumed are read: no database
//! access. Every thoth process goes through `clean`, which drops the test
//! process's variables whose names hold KEY, TOKEN, SECRET or PASSWORD (names
//! only are inspected). Every fake secret is built by joining strings, and the
//! exact-value one lives in the fixture's own secrets.json.
//!
//! The reviewer is the `review_delivery` fake scenario
//! (`src/bin/support/delivery_review.rs`): it judges each criterion from tags
//! in its text, so these tests check the material and instructions thoth
//! supplies, not a live model's judgment.
#![allow(dead_code)]

use super::common::{self, Fixture};
use serde_json::{json, Value};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug)]
pub enum Reviewer {
    Codex,
    Agy,
}

impl Reviewer {
    fn profile(self) -> &'static str {
        match self {
            Reviewer::Codex => "codex-a",
            Reviewer::Agy => "agy-a",
        }
    }

    fn model(self) -> &'static str {
        match self {
            Reviewer::Codex => "gpt-5.6-luna",
            Reviewer::Agy => "gemini-3.8-flash-medium",
        }
    }

    fn harness(self) -> (&'static str, &'static str) {
        match self {
            Reviewer::Codex => ("openai", "codex"),
            Reviewer::Agy => ("google", "antigravity"),
        }
    }
}

/// `f.cmd()` without any inherited variable whose name holds KEY, TOKEN,
/// SECRET or PASSWORD (only the names are read). git's GIT_CONFIG_COUNT,
/// GIT_CONFIG_KEY_n and GIT_CONFIG_VALUE_n are one group: a KEY among them
/// drops all of them, since a count without its keys makes every git command
/// fail with "missing config key".
pub fn clean(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    let names: Vec<std::ffi::OsString> = std::env::vars_os().map(|(name, _)| name).collect();
    let upper = |name: &std::ffi::OsString| name.to_string_lossy().to_ascii_uppercase();
    let git_group = |u: &str| u == "GIT_CONFIG_COUNT" || u.starts_with("GIT_CONFIG_KEY_") || u.starts_with("GIT_CONFIG_VALUE_");
    let secretish = |u: &str| ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| u.contains(w));
    let drop_git_group = names.iter().any(|n| {
        let u = upper(n);
        git_group(&u) && secretish(&u)
    });
    for name in &names {
        let u = upper(name);
        if secretish(&u) || (drop_git_group && git_group(&u)) {
            c.env_remove(name);
        }
    }
    c
}

fn run_json(_f: &Fixture, mut c: assert_cmd::Command, what: &str) -> Value {
    let out = c.output().expect("spawn thoth");
    assert!(
        out.status.success(),
        "thoth {what} exited {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("thoth {what} printed no JSON ({e}): {}", String::from_utf8_lossy(&out.stdout)))
}

fn cli(f: &Fixture, args: &[&str]) -> Value {
    let mut c = clean(f);
    c.args(args);
    run_json(f, c, &args.join(" "))
}

/// The fake check scripts a fixture repo holds, as (stem, Windows body, Linux
/// body).
type Script = (&'static str, String, String);

/// A fresh fixture: `scripts` committed to the repo, init, a claude-code
/// implementer profile and the reviewer's profile.
fn setup(reviewer: Reviewer, scripts: &[Script]) -> Fixture {
    let f = common::fixture();
    if !scripts.is_empty() {
        commit_scripts(&f, scripts);
    }
    let mut c = clean(&f);
    c.arg("init");
    c.assert().success();
    for (id, (provider, harness)) in [("claude-b", ("anthropic", "claude-code")), (reviewer.profile(), reviewer.harness())] {
        let cfg = f.home_path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        let mut c = clean(&f);
        c.args(["profile", "add", id, "--provider", provider, "--harness", harness, "--config-dir"])
            .arg(&cfg)
            .arg("--json");
        run_json(&f, c, "profile add");
    }
    f
}

fn commit_scripts(f: &Fixture, scripts: &[Script]) {
    let repo = f.repo_path();
    let mut names = Vec::new();
    for (stem, win, unix) in scripts {
        let name = if cfg!(windows) { format!("{stem}.cmd") } else { format!("{stem}.sh") };
        let body = if cfg!(windows) { format!("@echo off\r\n{win}\r\n") } else { format!("#!/bin/sh\n{unix}\n") };
        std::fs::write(repo.join(&name), body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(repo.join(&name), std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        names.push(name);
    }
    let mut add = vec!["add"];
    add.extend(names.iter().map(String::as_str));
    common::git(&repo, &add);
    common::git(&repo, &["commit", "-m", "check scripts"]);
    // checked out again, so git's line-ending conversion matches a worktree's
    for n in &names {
        std::fs::remove_file(repo.join(n)).unwrap();
    }
    let mut checkout = vec!["checkout", "HEAD", "--"];
    checkout.extend(names.iter().map(String::as_str));
    common::git(&repo, &checkout);
}

fn script_cmd(stem: &str) -> String {
    if cfg!(windows) { format!(r".\{stem}.cmd") } else { format!("./{stem}.sh") }
}

fn add_check(f: &Fixture, id: &str, stem: &str) {
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    cli(f, &["checks", "add", id, "--cmd", &script_cmd(stem), "--shell", shell, "--json"]);
}

fn approve_checks(f: &Fixture) {
    cli(f, &["checks", "approve", "--reason", "rvi fixture checks", "--json"]);
}

fn create_task(f: &Fixture, spec: &Value) -> String {
    let path = f.home_path().join("rvi-spec.json");
    std::fs::write(&path, spec.to_string()).unwrap();
    let mut c = clean(f);
    c.args(["task", "create", "-f"]).arg(&path).arg("--json");
    run_json(f, c, "task create")["task_id"].as_str().unwrap().to_string()
}

fn spec(acs: &[&str], checks: &[&str]) -> Value {
    let mut s = json!({"goal": "add the function that returns seven",
        "acceptance_criteria": acs, "constraints": [], "non_goals": [], "excluded_alternatives": [],
        "risk": "normal"});
    if !checks.is_empty() {
        s["checks"] = json!(checks);
    }
    s
}

fn wait_run(f: &Fixture, run: &str) -> Value {
    cli(f, &["wait", "--run", run, "--timeout", "180s", "--json"])
}

fn task_status(f: &Fixture, task: &str) -> String {
    cli(f, &["status", task, "--json"])["status"].as_str().unwrap().to_string()
}

fn wait_status(f: &Fixture, task: &str, wanted: &[&str]) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let s = task_status(f, task);
        if wanted.contains(&s.as_str()) {
            return s;
        }
        assert!(std::time::Instant::now() < deadline, "task {task} is {s}, not one of {wanted:?}");
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// A fake-claude `hooks` delivery (dispatch, or the rework after a REJECT)
/// with `patch`'s top-level keys set on its result.json; waits until it
/// settles at in_review. Returns (run_id, implementer worktree).
fn deliver(f: &Fixture, task: &str, patch: &Value, extra: &[&str]) -> (String, PathBuf) {
    let path = f.home_path().join(format!("rvi-patch-{}.json", ids_now()));
    std::fs::write(&path, patch.to_string()).unwrap();
    let mut c = clean(f);
    c.env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", task)
        .env("FAKE_RESULT_PATCH", &path)
        .args(["dispatch", task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"])
        .args(extra);
    let d = run_json(f, c, "dispatch");
    let run = d["run_id"].as_str().unwrap().to_string();
    wait_run(f, &run);
    assert_eq!(wait_status(f, task, &["in_review", "failed"]), "in_review");
    (run, PathBuf::from(d["worktree"].as_str().unwrap()))
}

fn ids_now() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
}

/// One review: `review start` with the `review_delivery` fake, waited for.
pub struct Review {
    pub start: Value,
    pub run: String,
    pub target: String,
    pub worktree: PathBuf,
    pub run_dir: PathBuf,
    /// What the fake consumed (see `delivery_review.rs`).
    pub capture: Value,
}

impl Review {
    fn input(&self) -> PathBuf {
        self.worktree.join(".agent").join("review-input")
    }
}

/// A reviewer run of the `crash` fake scenario: it fails before any verdict,
/// so the task stays in_review and the next review of the same attempt
/// reuses its worktree. Returns that worktree.
fn crashed_review(f: &Fixture, reviewer: Reviewer, task: &str) -> PathBuf {
    let mut c = clean(f);
    c.env("FAKE_SCENARIO", "crash")
        .args(["review", "start", task, "--profile", reviewer.profile(), "--model", reviewer.model(),
            "--effort", "medium", "--reason", "fixture", "--json"]);
    if let Reviewer::Agy = reviewer {
        c.env("FAKE_AGY_DATA_DIR", f.home_path().join("profiles").join("agy-a"));
    }
    let start = run_json(f, c, "review start");
    wait_run(f, start["run_id"].as_str().unwrap());
    assert_eq!(task_status(f, task), "in_review");
    PathBuf::from(start["worktree"].as_str().unwrap())
}

fn review(f: &Fixture, reviewer: Reviewer, task: &str, name: &str) -> Review {
    let capture = f.home_path().join(format!("capture-{name}.json"));
    let mut c = clean(f);
    c.env("FAKE_SCENARIO", "review_delivery")
        .env("FAKE_REVIEW_CAPTURE", &capture)
        .env("FAKE_PROMPT_COPY", f.home_path().join(format!("prompt-{name}.txt")))
        .args(["review", "start", task, "--profile", reviewer.profile(), "--model", reviewer.model(),
            "--effort", "medium", "--reason", "fixture", "--json"]);
    if let Reviewer::Agy = reviewer {
        c.env("FAKE_AGY_DATA_DIR", f.home_path().join("profiles").join("agy-a"));
    }
    let start = run_json(f, c, "review start");
    let run = start["run_id"].as_str().unwrap().to_string();
    wait_run(f, &run);
    wait_status(f, task, &["pass", "reject", "unverified"]);
    let text = std::fs::read_to_string(&capture)
        .unwrap_or_else(|e| panic!("{reviewer:?} wrote no capture for {name}: {e}; status: {:#}", cli(f, &["status", task, "--json"])));
    Review {
        target: start["target_run_id"].as_str().unwrap().to_string(),
        worktree: PathBuf::from(start["worktree"].as_str().unwrap()),
        run_dir: PathBuf::from(start["run_dir"].as_str().unwrap()),
        run,
        start,
        capture: serde_json::from_str(&text).unwrap(),
    }
}

fn events(f: &Fixture, task: &str) -> Vec<Value> {
    cli(f, &["events", "--since", "0", "--task", task, "--json"])["events"].as_array().unwrap().clone()
}

/// The delivery `run` recorded: its newest `result_ingested` event's
/// `detail.result_json`.
fn recorded_delivery(f: &Fixture, task: &str, run: &str) -> Value {
    events(f, task)
        .into_iter()
        .rev()
        .find(|e| e["type"] == "result_ingested" && e["run_id"] == run)
        .unwrap_or_else(|| panic!("run {run} has no result_ingested event"))["detail"]["result_json"]
        .clone()
}

fn review_input_redactions(f: &Fixture, task: &str, run: &str) -> Vec<Value> {
    events(f, task)
        .into_iter()
        .filter(|e| e["type"] == "secret_redacted" && e["run_id"] == run && e["detail"]["exit"] == "review_input")
        .collect()
}

fn evidence_list(f: &Fixture, task: &str) -> Vec<Value> {
    cli(f, &["evidence", "list", task, "--json"])["evidence"].as_array().unwrap().clone()
}

fn codes(f: &Fixture, task: &str) -> Vec<String> {
    evidence_list(f, task).iter().map(|e| e["code"].as_str().unwrap().to_string()).collect()
}

fn criterion<'a>(show: &'a Value, id: &str) -> &'a Value {
    show["findings"]["criteria"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {show:#}"))
}

/// The review document as `review show --json` gives it, after checking it
/// was ingested (no `review_invalid` for the run) and cites only real codes.
fn shown(f: &Fixture, task: &str, r: &Review) -> Value {
    let show = cli(f, &["review", "show", task, "--json"]);
    assert_eq!(show["reviewer_run_id"], json!(r.run), "{show:#}");
    assert_eq!(show["target_run_id"], json!(r.target), "{show:#}");
    let evs = events(f, task);
    assert!(
        !evs.iter().any(|e| e["type"] == "review_invalid" && e["run_id"] == json!(r.run)),
        "review {} was refused: {evs:#?}",
        r.run
    );
    let known = codes(f, task);
    for c in show["findings"]["criteria"].as_array().unwrap() {
        for code in c["evidence"].as_array().unwrap() {
            assert!(known.contains(&code.as_str().unwrap().to_string()), "{code} is not a real code: {show:#}");
        }
    }
    show
}

/// The files of the review input folder, sorted.
fn input_files(r: &Review) -> Vec<String> {
    let mut out: Vec<String> = std::fs::read_dir(r.input())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    out.sort();
    out
}

/// The lines packet.md shows under `code`'s evidence index entry, up to the
/// next entry or section.
fn packet_block(packet: &str, code: &str) -> String {
    let at = packet.find(&format!("\n- {code} | ")).unwrap_or_else(|| panic!("{code} missing from packet.md"));
    packet[at + 1..]
        .lines()
        .skip(1)
        .take_while(|l| !l.starts_with("- ") && !l.starts_with('#') && !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// A verify row's recorded execution error (refused, timed out, incomplete
/// output, no exit code), from `evidence list --json`'s detail.
fn execution_error(row: &Value) -> Option<String> {
    (row["detail"]["event"] == "control_plane_rerun")
        .then(|| row["detail"]["error"].as_str().map(str::to_string))
        .flatten()
        .filter(|e| !e.is_empty())
}

/// packet.md lists every evidence code, and evidence/<CODE>.txt holds each
/// whole stored blob (codes of rows whose blob is not readable are skipped).
/// A verify row with a recorded execution error shows that error under its
/// packet.md entry and in its evidence file, next to the whole blob.
fn check_packet_and_evidence(f: &Fixture, task: &str, r: &Review) {
    let packet = std::fs::read_to_string(r.input().join("packet.md")).unwrap();
    for row in evidence_list(f, task) {
        let code = row["code"].as_str().unwrap();
        assert!(packet.contains(&format!("- {code} | ")), "{code} missing from packet.md");
        let file = std::fs::read_to_string(r.input().join("evidence").join(format!("{code}.txt"))).unwrap();
        let error = execution_error(&row);
        if let Some(e) = &error {
            assert!(packet_block(&packet, code).contains(&format!("execution error: {e}")), "{code}: {packet}");
            assert!(file.contains(e.as_str()), "{code}'s evidence file must show its execution error: {file:?}");
        }
        if let Some(p) = row["blob_path"].as_str() {
            let blob = String::from_utf8_lossy(&std::fs::read(p).unwrap()).into_owned();
            if !blob.is_empty() && !file.contains("[redacted:") {
                if error.is_some() {
                    assert!(file.ends_with(&blob), "{code} must hold the whole stored blob: {file:?}");
                } else {
                    assert_eq!(file, blob, "{code} must hold the whole stored blob");
                }
            }
        }
    }
    // the fake read the packet and the files thoth wrote, nothing else
    assert_eq!(r.capture["packet"].as_str().unwrap(), packet);
}

const RESULT_PATH: &str = ".agent/review-input/result.json";

/// AC2, AC3 and AC7: the guidance in the prompt the fake received, and in
/// the run's prompt.txt.
fn check_prompt(r: &Review, linux_listed: bool) {
    let received = r.capture["prompt"].as_str().unwrap().to_string();
    let stored = std::fs::read_to_string(r.run_dir.join("prompt.txt")).unwrap();
    for p in [&received, &stored] {
        for needle in [
            RESULT_PATH,
            "is the whole result the implementer delivered for the run under review",
            "anywhere in the file, not only in the claims",
            "a field's name or length is not its content",
            "proves only what the worker delivered, not that what it says is true",
            "keep citing an actual code from the packet's evidence index for every met criterion",
            "even when both its output streams are empty",
            "is not by itself missing evidence",
            "a silent clippy or public-tree check",
            "a verify@base: row, a check that only started, a failed, refused, timed-out or incomplete check, an unknown exit status (exit -)",
            "When a criterion asks for particular output, that output has to be there.",
            "A check whose execution error was recorded did not complete cleanly, even when its exit code is 0 or it printed output",
            "Every met criterion must cite at least one code from the packet's evidence index",
            "Do not run thoth",
        ] {
            assert!(p.contains(needle), "missing {needle:?} in:\n{p}");
        }
        assert_eq!(p.contains("linux-container"), linux_listed, "{p}");
        assert_eq!(p.contains("tests/install_scripts.rs"), linux_listed, "{p}");
    }
    assert!(received.contains("Every id in \"criteria\" must be one of:"), "{received}");
}

// --- the deliveries ---------------------------------------------------------

const MAPPING_AC: &str = "AC1: the delivery maps every acceptance criterion to the test that covers it [mapping]";
const PLAIN_AC: &str = "AC2: the added function returns seven";
const TEST: &str = "tests/added_test.rs::added_is_seven";
const GHOST: &str = "tests/ghost.rs::ghost_test_never_written";

/// A mapping for AC1 and AC2, padded with `extra` long entries and a distinct
/// final one.
fn mapping(ac2: &str, extra: usize, last: &str) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("AC1".into(), json!(TEST));
    m.insert("AC2".into(), json!(ac2));
    for i in 0..extra {
        m.insert(format!("note_{i:04}"), json!(format!("padding entry {i} of a long mapping, kept whole by the review input")));
    }
    m.insert("zzzz_final".into(), json!(last));
    Value::Object(m)
}

/// AC1, AC2, AC6, AC7 (and AC5's clean case): three deliveries of one task,
/// each reviewed in the same review worktree. 1: a mapping whose AC2 test no
/// evidence holds (REJECT). 2: no mapping at all, a stale extra file left in
/// review-input before the review (REJECT). 3: a long valid mapping nested
/// outside the claims plus a mapping in the summary, the implementer's
/// worktree result.json overwritten before the review (PASS).
pub fn delivered_result_is_read_from_the_target_run(reviewer: Reviewer) {
    let f = setup(reviewer, &[]);
    let task = create_task(&f, &spec(&[MAPPING_AC, PLAIN_AC], &[]));

    // 1: a misleading mapping
    let misleading = json!({"summary": "first delivery: AC2 is covered by the ghost test",
        "coverage": {"ac_test_map": mapping(GHOST, 0, "FIRST-DELIVERY-END")}});
    let (run1, _) = deliver(&f, &task, &misleading, &[]);
    let r1 = review(&f, reviewer, &task, "r1");
    assert_eq!(r1.target, run1);
    assert_eq!(r1.capture["result"]["coverage"], misleading["coverage"], "{:#}", r1.capture);
    let show = shown(&f, &task, &r1);
    assert_eq!(show["verdict"], "REJECT", "{show:#}");
    let c1 = criterion(&show, "AC1");
    assert_eq!(c1["verdict"], "unmet", "{show:#}");
    assert!(c1["note"].as_str().unwrap().contains(GHOST), "{show:#}");
    assert_eq!(r1.capture["mapping_path"], "$.coverage.ac_test_map");

    // 2: no mapping, while run 1's delivery had one
    let missing = json!({"summary": "second delivery: no mapping this time"});
    let (run2, _) = deliver(&f, &task, &missing, &[]);
    let r2 = review(&f, reviewer, &task, "r2");
    assert_eq!(r2.target, run2);
    assert_eq!(r2.capture["result"]["summary"], missing["summary"]);
    assert!(r2.capture["mapping"].is_null(), "run 1's mapping must not stand in: {:#}", r2.capture);
    assert_eq!(r2.capture["result"], recorded_delivery(&f, &task, &run2));
    let show = shown(&f, &task, &r2);
    assert_eq!(show["verdict"], "REJECT", "{show:#}");
    let c1 = criterion(&show, "AC1");
    assert_eq!(c1["verdict"], "unmet");
    assert!(c1["note"].as_str().unwrap().contains("no AC-to-test mapping"), "{show:#}");

    // 3: a long valid mapping; the implementer's own file is overwritten
    let summary = format!("third delivery. Mapping: AC1 -> {TEST}; AC2 -> {TEST}.");
    let long = mapping(TEST, 400, "THE-DISTINCT-FINAL-ENTRY");
    let good = json!({"summary": summary, "brief": "third delivery, mapped",
        "report": {"coverage": {"ac_test_map": long}, "optional": {"nested": [1, {"deep": "value"}], "empty": null}}});
    let (run3, impl_wt3) = deliver(&f, &task, &good, &["--allow-attempt", "3"]);
    // a first review of this attempt crashes; its worktree, with the input
    // written for it plus a stale extra file, is reused by the next one
    let reused = crashed_review(&f, reviewer, &task);
    let input = reused.join(".agent").join("review-input");
    assert!(input.join("result.json").is_file(), "the crashed review got its input");
    let stale = input.join("stale-extra.txt");
    std::fs::write(&stale, "left over from an earlier review\n").unwrap();
    std::fs::write(input.join("result.json"), r#"{"summary":"a previous review's file"}"#).unwrap();
    // the settled implementer's own file is edited after its delivery
    std::fs::write(impl_wt3.join(".agent").join("result.json"), r#"{"summary":"BOGUS on-disk edit","report":{"coverage":{"ac_test_map":{}}}}"#).unwrap();
    let r3 = review(&f, reviewer, &task, "r3");
    assert_eq!(r3.target, run3);
    assert_eq!(r3.worktree, reused, "the review worktree is reused");
    assert!(!stale.exists(), "the stale file survived the refill");

    // AC1: the file is the whole recorded delivery of the target run
    let file: Value = serde_json::from_str(&std::fs::read_to_string(r3.input().join("result.json")).unwrap()).unwrap();
    let recorded = recorded_delivery(&f, &task, &run3);
    assert_eq!(file, recorded);
    assert_eq!(r3.capture["result"], recorded, "the fake consumed the whole recorded delivery");
    for (k, v) in good.as_object().unwrap() {
        assert_eq!(&recorded[k], v, "{k} as delivered");
    }
    assert_eq!(recorded["task_id"], json!(task));
    assert_eq!(r3.capture["mapping"]["zzzz_final"], "THE-DISTINCT-FINAL-ENTRY");
    assert_eq!(r3.capture["mapping"].as_object().unwrap().len(), 403);
    assert_eq!(r3.capture["mapping_path"], "$.report.coverage.ac_test_map");
    assert!(r3.capture["result_text"].as_str().unwrap().contains("THE-DISTINCT-FINAL-ENTRY"));
    // `task result --run` agrees on summary and claims
    let shown_result = cli(&f, &["task", "result", &task, "--run", &run3, "--json"]);
    assert_eq!(shown_result["summary"], json!(summary));
    assert_eq!(
        shown_result["claims"].as_array().unwrap().len(),
        recorded["claims"].as_array().unwrap().len()
    );
    for (a, b) in shown_result["claims"].as_array().unwrap().iter().zip(recorded["claims"].as_array().unwrap()) {
        assert_eq!(a["kind"], b["kind"]);
        assert_eq!(a["text"], b["text"]);
    }

    // AC2: the valid review cites real codes holding the mapped test
    let show = shown(&f, &task, &r3);
    assert_eq!(show["verdict"], "PASS", "{show:#}");
    let c1 = criterion(&show, "AC1");
    assert_eq!(c1["verdict"], "met");
    let cited: Vec<String> = c1["evidence"].as_array().unwrap().iter().map(|c| c.as_str().unwrap().to_string()).collect();
    assert!(!cited.is_empty());
    for code in &cited {
        let text = r3.capture["evidence"][code].as_str().unwrap();
        assert!(text.contains("added_is_seven"), "{code} does not hold the mapped test");
    }

    // the input folder holds packet.md, evidence/ and result.json only, and
    // no evidence code was invented for the new file
    assert_eq!(input_files(&r3), ["evidence", "packet.md", "result.json"]);
    check_packet_and_evidence(&f, &task, &r3);
    let listed = codes(&f, &task);
    let mut files: Vec<String> = std::fs::read_dir(r3.input().join("evidence"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().trim_end_matches(".txt").to_string())
        .collect();
    files.sort();
    let mut sorted = listed.clone();
    sorted.sort();
    assert_eq!(files, sorted);

    // AC2, AC3, AC7: the instructions, without linux-container
    for r in [&r1, &r2, &r3] {
        check_prompt(r, false);
        // AC5's clean case: nothing masked, no review_input event
        assert!(review_input_redactions(&f, &task, &r.run).is_empty(), "{}", r.run);
        let started = events(&f, &task)
            .into_iter()
            .find(|e| e["type"] == "review_started" && e["run_id"] == json!(r.run))
            .unwrap();
        assert_eq!(started["detail"]["target_run_id"], json!(r.target));
    }
    assert_eq!(r1.start["target_run_id"], json!(run1));
}

// --- secrets ------------------------------------------------------------------

fn token(body: &str) -> String {
    format!("{}{body}", ["gh", "p_"].concat())
}

fn exact_value() -> String {
    ["dummy-rvi-", "exact-value-", "0001"].concat()
}

/// How often each needle occurs in every key and string of `v`.
fn occurrences(v: &Value, needle: &str) -> u64 {
    match v {
        Value::String(s) => s.matches(needle).count() as u64,
        Value::Array(a) => a.iter().map(|x| occurrences(x, needle)).sum(),
        Value::Object(m) => m.iter().map(|(k, x)| k.matches(needle).count() as u64 + occurrences(x, needle)).sum(),
        _ => 0,
    }
}

fn secret_patch(t1: &str, t2: &str, x: &str) -> Value {
    let mut nested = serde_json::Map::new();
    nested.insert("level1".into(), json!({
        "note": format!("quoted \"{t1}\" and a backslash \\ here"),
        "list": ["plain survives", x, {"deeper": x}],
    }));
    nested.insert(format!("key {t1}"), json!("value kept"));
    let mut collide = serde_json::Map::new();
    collide.insert(t1.to_string(), json!("first"));
    collide.insert(t2.to_string(), json!("second"));
    json!({
        "summary": format!("delivered with {t1} in the prose and {x} as well"),
        "brief": "a clean brief",
        "rvi_nested": nested,
        "embedded_json": format!("{{\"inner\":\"{x}\"}}"),
        "collide": collide,
        "unrelated": {"number": 7, "flag": true, "text": "kept as it was"},
    })
}

/// Every review-input file, the capture and prompt.txt hold none of
/// `secrets`.
fn no_secrets(r: &Review, secrets: &[&str]) {
    let mut texts = vec![
        serde_json::to_string(&r.capture).unwrap(),
        std::fs::read_to_string(r.run_dir.join("prompt.txt")).unwrap(),
        std::fs::read_to_string(r.input().join("packet.md")).unwrap(),
        std::fs::read_to_string(r.input().join("result.json")).unwrap(),
    ];
    for e in std::fs::read_dir(r.input().join("evidence")).unwrap() {
        texts.push(std::fs::read_to_string(e.unwrap().path()).unwrap());
    }
    for t in &texts {
        for s in secrets {
            assert!(!t.contains(s), "a fake secret reached the reviewer");
            assert!(!t.contains(&s[4..20]), "part of a fake secret reached the reviewer");
        }
    }
}

/// AC5 (and AC4's unlisted case): deliveries with format and exact-value
/// secrets in nested values, keys, escaped text and colliding keys. 1: only
/// the result carries secrets (REJECT, the delivery has no mapping). 2: a
/// check's output carries one too, through `verify`.
pub fn delivered_result_is_masked(reviewer: Reviewer) {
    let t1 = token("rviDUMMY0123456789rviDUMMY0123456789");
    let t2 = token("rviOTHER0123456789rviOTHER0123456789");
    let t3 = token("rviCHECK0123456789rviCHECK0123456789");
    let x = exact_value();
    let scripts: Vec<Script> = vec![
        ("leaky", format!("echo pushed with {t3}"), format!("echo \"pushed with {t3}\"")),
        ("linux-check", "echo clippy: 0 warnings && echo test result: ok. 9 passed tests/install_scripts.rs".into(),
            "echo 'clippy: 0 warnings'; echo 'test result: ok. 9 passed tests/install_scripts.rs'".into()),
    ];
    let f = setup(reviewer, &scripts);
    std::fs::write(f.home_path().join("secrets.json"), json!({"rvi_fixture_value": x}).to_string()).unwrap();
    add_check(&f, "leaky", "leaky");
    add_check(&f, "linux-container", "linux-check");
    approve_checks(&f);
    let task = create_task(&f, &spec(&[MAPPING_AC, PLAIN_AC], &[]));
    let patch = secret_patch(&t1, &t2, &x);
    let github = occurrences(&patch, &t1) + occurrences(&patch, &t2);
    let exact = occurrences(&patch, &x);
    assert!(github >= 4 && exact >= 3, "{github} {exact}");

    // 1: secrets in the result only
    let (run1, _) = deliver(&f, &task, &patch, &[]);
    let r1 = review(&f, reviewer, &task, "s1");
    assert_eq!(r1.target, run1);
    let ev = review_input_redactions(&f, &task, &r1.run);
    assert_eq!(ev.len(), 1, "{ev:#?}");
    assert_eq!(ev[0]["detail"]["kinds"], json!({"github-token": github, "thoth:secrets.json": exact}), "{ev:#?}");
    assert_eq!(ev[0]["detail"]["evidence"], json!([]));
    check_masked_result(&r1);
    no_secrets(&r1, &[&t1, &t2, &t3, &x]);
    assert_eq!(shown(&f, &task, &r1)["verdict"], "REJECT");

    // 2: and in a check's output
    let (run2, _) = deliver(&f, &task, &patch, &[]);
    let v = cli(&f, &["verify", &run2, "--timeout", "120s", "--json"]);
    assert_eq!(v["failures"], 0, "{v:#}");
    let leaky = v["results"].as_array().unwrap().iter().find(|r| r["check_id"] == "leaky").unwrap()["evidence"]
        .as_str()
        .unwrap()
        .to_string();
    let r2 = review(&f, reviewer, &task, "s2");
    assert_eq!(r2.target, run2);
    let ev = review_input_redactions(&f, &task, &r2.run);
    assert_eq!(ev.len(), 1, "{ev:#?}");
    // the check's token: once in packet.md's output tail, once in its file
    assert_eq!(ev[0]["detail"]["kinds"], json!({"github-token": github + 2, "thoth:secrets.json": exact}), "{ev:#?}");
    assert_eq!(ev[0]["detail"]["evidence"], json!([leaky]));
    let file = std::fs::read_to_string(r2.input().join("evidence").join(format!("{leaky}.txt"))).unwrap();
    assert!(file.contains("pushed with [redacted:github-token]"), "{file}");
    check_masked_result(&r2);
    no_secrets(&r2, &[&t1, &t2, &t3, &x]);
    assert!(events(&f, &task).iter().any(|e| e["type"] == "verify_ran" && e["run_id"] == json!(run2)));

    // AC4's unlisted case: the linux-container row is there, the prompt
    // says nothing about that check
    let packet = r2.capture["packet"].as_str().unwrap();
    assert!(packet.contains("linux-check"), "{packet}");
    let row = evidence_list(&f, &task)
        .into_iter()
        .find(|e| e["detail"]["check_id"] == "linux-container")
        .unwrap();
    let text = r2.capture["evidence"][row["code"].as_str().unwrap()].as_str().unwrap();
    assert!(text.contains("clippy") && text.contains("tests/install_scripts.rs"), "{text}");
    for r in [&r1, &r2] {
        check_prompt(r, false);
    }
}

fn check_masked_result(r: &Review) {
    let text = std::fs::read_to_string(r.input().join("result.json")).unwrap();
    let v: Value = serde_json::from_str(&text).expect("result.json stays valid JSON");
    assert_eq!(r.capture["result"], v, "the fake read the masked file");
    assert_eq!(v["summary"], "delivered with [redacted:github-token] in the prose and [redacted:thoth:secrets.json] as well");
    assert_eq!(v["brief"], "a clean brief");
    assert_eq!(v["rvi_nested"]["level1"]["note"], "quoted \"[redacted:github-token]\" and a backslash \\ here");
    assert_eq!(v["rvi_nested"]["level1"]["list"], json!(["plain survives", "[redacted:thoth:secrets.json]", {"deeper": "[redacted:thoth:secrets.json]"}]));
    assert_eq!(v["rvi_nested"]["key [redacted:github-token]"], "value kept");
    assert_eq!(v["embedded_json"], "{\"inner\":\"[redacted:thoth:secrets.json]\"}");
    assert_eq!(v["collide"], json!({"[redacted:github-token]": "first", "[redacted:github-token]#2": "second"}));
    assert_eq!(v["unrelated"], json!({"number": 7, "flag": true, "text": "kept as it was"}));
}

// --- checks -----------------------------------------------------------------

/// AC3 and AC4's listed case: silent and loud successes, a failure, a
/// timeout, a refusal, an incomplete output (exit 0 with output, while a
/// descendant still held the pipe), a base-only pass and a linux-container
/// row, all made through `checks` and `verify` with fake scripts.
pub fn check_evidence_rules(reviewer: Reviewer) {
    let scripts: Vec<Script> = vec![
        ("clippy-quiet", "exit /b 0".into(), "exit 0".into()),
        ("tree-quiet", "exit /b 0".into(), "exit 0".into()),
        ("loud", "echo loud-banner-ok".into(), "echo loud-banner-ok".into()),
        ("fails", "exit /b 3".into(), "exit 3".into()),
        ("slow", "ping -n 30 127.0.0.1 >nul".into(), "sleep 30".into()),
        ("pending", "exit /b 0".into(), "exit 0".into()),
        // exits 0 at once; the detached child keeps the output pipe open
        // past verify's drain grace
        ("incomplete", "echo partial-output-banner\r\nstart /b ping -n 5 127.0.0.1".into(),
            "echo partial-output-banner\nsleep 4 &".into()),
        ("baseonly", r"if exist src\added.rs (exit /b 5) else (exit /b 0)".into(),
            "if [ -f src/added.rs ]; then exit 5; fi\nexit 0".into()),
        ("linux-check", "echo clippy: 0 warnings && echo test result: ok. 9 passed tests/install_scripts.rs".into(),
            "echo 'clippy: 0 warnings'; echo 'test result: ok. 9 passed tests/install_scripts.rs'".into()),
    ];
    let f = setup(reviewer, &scripts);
    for (id, stem) in [("clippy", "clippy-quiet"), ("public-tree", "tree-quiet"), ("loud", "loud"), ("fails", "fails"),
        ("slow", "slow"), ("incomplete", "incomplete"), ("baseonly", "baseonly"), ("linux-container", "linux-check")] {
        add_check(&f, id, stem);
    }
    approve_checks(&f);
    add_check(&f, "pending", "pending"); // never approved: refused
    let acs = [
        "AC1: the silent clippy check passes [check:clippy-quiet]",
        "AC2: the silent public-tree check passes [check:tree-quiet]",
        "AC3: the loud check prints its banner [check:loud] [output:loud-banner-ok]",
        "AC4: the clippy check prints a banner [check:clippy-quiet] [output:clippy-banner]",
        "AC5: the failing check passes [check:fails]",
        "AC6: the slow check passes [check:slow]",
        "AC7: the pending check passes [check:pending]",
        "AC8: the base-only check passes [check:baseonly]",
        "AC9: Linux clippy and the installation-script tests pass [check:linux-check] [output:install_scripts]",
        "AC10: the whole Linux test suite passes [check:linux-check] [scope:full-linux-suite]",
        "AC11: the incomplete-output check passes [check:incomplete]",
        "AC12: the incomplete-output check prints its banner [check:incomplete] [output:partial-output-banner]",
    ];
    let task = create_task(&f, &spec(&acs, &["clippy", "public-tree", "loud", "fails", "slow", "pending", "incomplete",
        "baseonly", "linux-container"]));
    let (run, _) = deliver(&f, &task, &json!({"summary": "checks fixture"}), &[]);
    let mut c = clean(&f);
    c.args(["verify", &run, "--timeout", "5s", "--json"]);
    let cand = c.output().unwrap();
    assert!(!cand.status.success(), "a failing verify exits non-zero");
    let cand: Value = serde_json::from_slice(&cand.stdout).unwrap();
    let mut c = clean(&f);
    c.args(["verify", &run, "--at-base", "--timeout", "5s", "--json"]);
    let base: Value = serde_json::from_slice(&c.output().unwrap().stdout).unwrap();
    let result_of = |doc: &Value, id: &str| -> Value {
        doc["results"].as_array().unwrap().iter().find(|r| r["check_id"] == id).unwrap().clone()
    };
    for id in ["clippy", "public-tree", "loud", "linux-container"] {
        assert_eq!(result_of(&cand, id)["exit_code"], 0, "{cand:#}");
    }
    assert_eq!(result_of(&cand, "clippy")["stdout_bytes"], 0);
    assert_eq!(result_of(&cand, "clippy")["stderr_bytes"], 0);
    assert_eq!(result_of(&cand, "fails")["exit_code"], 3);
    assert!(result_of(&cand, "slow")["exit_code"].is_null());
    assert!(result_of(&cand, "slow")["error"].as_str().unwrap().contains("timed out"));
    assert!(result_of(&cand, "pending")["exit_code"].is_null());
    assert!(!result_of(&cand, "pending")["error"].as_str().unwrap().is_empty(), "{cand:#}");
    // incomplete: exit code 0 and output, yet an execution error recorded
    let incomplete = result_of(&cand, "incomplete");
    assert_eq!(incomplete["exit_code"], 0, "{cand:#}");
    assert!(incomplete["stdout_bytes"].as_u64().unwrap() > 0, "{cand:#}");
    assert!(incomplete["error"].as_str().unwrap().contains("output may be incomplete"), "{cand:#}");
    assert_eq!(result_of(&cand, "baseonly")["exit_code"], 5);
    assert_eq!(result_of(&base, "baseonly")["exit_code"], 0, "{base:#}");
    let code = |doc: &Value, id: &str| result_of(doc, id)["evidence"].as_str().unwrap().to_string();

    let r = review(&f, reviewer, &task, "c1");
    assert_eq!(r.target, run);
    check_prompt(&r, true);
    for needle in [
        "In this repository that check runs Linux clippy and the installation-script tests (tests/install_scripts.rs), not the whole Linux cargo test suite.",
        "do not judge a criterion unmet merely because its log has no whole-suite test count",
        "do not infer from the check's name that the whole Linux suite passed",
    ] {
        assert!(r.capture["prompt"].as_str().unwrap().contains(needle), "{needle}");
    }
    // the silent rows: empty files, "(no stored output)" in the packet
    let packet = r.capture["packet"].as_str().unwrap();
    for id in ["clippy", "public-tree"] {
        let c = code(&cand, id);
        assert_eq!(r.capture["evidence"][&c], "", "{c}");
        assert_eq!(std::fs::read_to_string(r.input().join("evidence").join(format!("{c}.txt"))).unwrap(), "");
        assert_eq!(packet_block(packet, &c).trim(), "(no stored output)", "{packet}");
    }
    // the incomplete, timed-out and refused rows show their recorded error
    // in packet.md and in their evidence files, the incomplete one next to
    // its exit 0 and its output
    for id in ["incomplete", "slow", "pending"] {
        let c = code(&cand, id);
        let error = result_of(&cand, id)["error"].as_str().unwrap().to_string();
        let block = packet_block(packet, &c);
        assert!(block.contains(&format!("execution error: {error}")), "{id}: {packet}");
        let file = r.capture["evidence"][&c].as_str().unwrap();
        assert!(file.contains(&error), "{id}: {file:?}");
    }
    let c = code(&cand, "incomplete");
    assert!(packet.contains(&format!("- {c} | command | exit 0 | ")), "{packet}");
    assert!(packet_block(packet, &c).contains("partial-output-banner"), "{packet}");
    assert!(r.capture["evidence"][&c].as_str().unwrap().contains("partial-output-banner"));
    check_packet_and_evidence(&f, &task, &r);
    let evs = events(&f, &task);
    assert_eq!(evs.iter().filter(|e| e["type"] == "verify_ran").count(), 2);
    assert!(review_input_redactions(&f, &task, &r.run).is_empty());

    let show = shown(&f, &task, &r);
    assert_eq!(show["verdict"], "REJECT", "{show:#}");
    let expect = |id: &str, verdict: &str, cites: Vec<String>| {
        let c = criterion(&show, id);
        assert_eq!(c["verdict"], verdict, "{id}: {show:#}");
        let got: Vec<String> = c["evidence"].as_array().unwrap().iter().map(|x| x.as_str().unwrap().to_string()).collect();
        for want in &cites {
            assert!(got.contains(want), "{id} should cite {want}: {show:#}");
        }
    };
    expect("AC1", "met", vec![code(&cand, "clippy")]);
    expect("AC2", "met", vec![code(&cand, "public-tree")]);
    expect("AC3", "met", vec![code(&cand, "loud")]);
    expect("AC4", "unmet", vec![code(&cand, "clippy")]);
    expect("AC5", "unmet", vec![code(&cand, "fails")]);
    expect("AC6", "unmet", vec![code(&cand, "slow")]);
    expect("AC7", "unmet", vec![code(&cand, "pending")]);
    // the base row passed, yet is not promoted to the candidate's success
    expect("AC8", "unmet", vec![code(&cand, "baseonly"), code(&base, "baseonly")]);
    expect("AC9", "met", vec![code(&cand, "linux-container")]);
    expect("AC10", "unmet", vec![code(&cand, "linux-container")]);
    assert!(criterion(&show, "AC10")["note"].as_str().unwrap().contains("not the whole Linux suite"));
    // an exit-0 check with output and a recorded execution error is not a
    // pass, not even for a criterion asking for output it did print
    expect("AC11", "unmet", vec![code(&cand, "incomplete")]);
    expect("AC12", "unmet", vec![code(&cand, "incomplete")]);
    for id in ["AC11", "AC12"] {
        let note = criterion(&show, id)["note"].as_str().unwrap();
        assert!(note.contains("output may be incomplete"), "{id}: {show:#}");
    }
}
