//! Codex as a second implementer harness (docs/post-phase2/codex-worker-scope-
//! 2026-09-27.md §2.1-§2.3), driven end to end through `fake-codex`. Never the
//! real codex: `common::Fixture::cmd` points `THOTH_CODEX_BIN` at the fake.

mod common;

use predicates::prelude::*;
use std::path::PathBuf;

const MODEL: &str = "gpt-5.6-luna";

fn args_log(f: &common::Fixture) -> PathBuf {
    f.home_path().join("fake-codex-args.jsonl")
}

/// Every invocation fake-codex logged, oldest first.
fn logged_invocations(f: &common::Fixture) -> Vec<serde_json::Value> {
    std::fs::read_to_string(args_log(f))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn exec_invocations(f: &common::Fixture) -> Vec<Vec<String>> {
    logged_invocations(f)
        .into_iter()
        .map(|v| {
            v["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_string())
                .collect::<Vec<String>>()
        })
        .filter(|a| a.first().map(String::as_str) == Some("exec"))
        .collect()
}

fn dispatch_codex(
    f: &common::Fixture,
    task_id: &str,
    scenario: &str,
    envs: &[(&str, &str)],
) -> serde_json::Value {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task_id)
        .env("FAKE_CODEX_ARGS_LOG", args_log(f))
        .args(["dispatch", task_id, "--profile", "codex-a", "--model", MODEL, "--effort", "medium", "--json"]);
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.assert().success().get_output().stdout.clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// The newest event of `ty` for `run_id`, parsed.
fn event_detail(f: &common::Fixture, run_id: &str, ty: &str) -> Option<serde_json::Value> {
    f.state_conn()
        .query_row(
            "SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq DESC LIMIT 1",
            [run_id, ty],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .map(|d| serde_json::from_str(&d).unwrap())
}

fn run_row(f: &common::Fixture, run_id: &str) -> (String, Option<String>, Option<String>, Option<String>) {
    f.state_conn()
        .query_row(
            "SELECT status, interrupted_reason, error_text, harness_session_id FROM runs WHERE run_id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
}

fn setup() -> (common::Fixture, PathBuf, String) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_codex_profile("codex-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    (f, cfg, task_id)
}

// AC1 ---------------------------------------------------------------------

#[test]
fn profile_add_accepts_codex_and_check_reads_login_status() {
    let f = common::fixture();
    let cfg = f.add_codex_profile("codex-a");

    let out = f
        .cmd()
        .args(["profile", "list", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["profiles"][0]["harness"], "codex", "{v}");
    assert_eq!(v["profiles"][0]["provider"], "openai", "{v}");

    // fake-codex answers "Logged in" only when CODEX_HOME names a directory
    let out = f
        .cmd()
        .env("FAKE_CODEX_ARGS_LOG", args_log(&f))
        .args(["profile", "check", "codex-a", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["logged_in"], true, "{v}");
    assert_eq!(v["email"], serde_json::Value::Null, "{v}");
    assert_eq!(v["subscription_type"], serde_json::Value::Null, "{v}");

    let calls = logged_invocations(&f);
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert_eq!(calls[0]["args"], serde_json::json!(["login", "status"]));
    // Both sides canonicalized: the stored config dir is the plain path (see
    // tests/profile.rs), while canonicalize returns `\\?\C:\...` on Windows.
    let home = std::fs::canonicalize(&cfg).unwrap();
    assert_eq!(
        calls[0]["codex_home"].as_str().map(|p| std::fs::canonicalize(p).unwrap()),
        Some(home),
        "CODEX_HOME must be the profile's config dir"
    );

    // logged out: exit 1 from codex means logged_in false, and check says so
    let out = f
        .cmd()
        .env("FAKE_CODEX_LOGGED_OUT", "1")
        .args(["profile", "check", "codex-a", "--json"])
        .assert()
        .code(1)
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["logged_in"], false, "{v}");
}

// AC2 ---------------------------------------------------------------------

#[test]
fn dispatch_to_codex_refuses_a_claude_model_name() {
    let (f, _cfg, task_id) = setup();
    for model in [None, Some("haiku"), Some("opus"), Some("fable"), Some("claude-sonnet-5")] {
        let mut c = f.cmd();
        c.args(["dispatch", &task_id, "--profile", "codex-a"]);
        match model {
            Some(m) => {
                c.args(["--model", m]);
                c.assert()
                    .failure()
                    .stderr(predicate::str::contains("Claude model"))
                    .stderr(predicate::str::contains("gpt-5.6-luna"));
            }
            // RJ-1: no default model any more; a left-out --model is refused
            None => {
                c.assert().failure().stderr(predicate::str::contains("--model"));
            }
        }
    }
    let runs: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
        .unwrap();
    assert_eq!(runs, 0);
    assert_eq!(f.task_status(&task_id), "queued");
}

// AC2-AC5, AC6's recording half: one good run, end to end ------------------

#[test]
fn a_good_codex_run_reaches_in_review_with_stream_evidence() {
    let (f, cfg, task_id) = setup();
    let d = dispatch_codex(&f, &task_id, "ok", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let wt = PathBuf::from(d["worktree"].as_str().unwrap());
    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!((code, status.as_str()), (0, "in_review"));

    // AC2: exactly the scope §2.2 command, with CODEX_HOME set. No `--add-dir`
    // (C1b, E9): codex's sandbox cannot write a linked worktree's own gitdir
    // regardless, so thoth commits on the worker's behalf instead.
    let calls = logged_invocations(&f);
    let exec = calls
        .iter()
        .find(|c| c["args"][0] == "exec")
        .expect("fake-codex was launched");
    let args: Vec<String> = exec["args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap().to_string())
        .collect();
    let win_pair: &[&str] = if cfg!(windows) { &["-c", "windows.sandbox=\"unelevated\""] } else { &[] };
    let expected_head: Vec<String> = [
        &["exec", "--json", "-m", MODEL, "-c", "model_reasoning_effort=\"medium\"", "-s", "workspace-write"][..],
        win_pair,
        &[
        "-c", "approval_policy=\"never\"",
        "--disable", "multi_agent", "--disable", "apps",
        "--disable", "browser_use", "--disable", "computer_use",
        "--ignore-user-config", "-C",
        ],
    ]
    .concat()
    .iter()
    .map(|s| s.to_string())
    .collect();
    assert_eq!(args[..expected_head.len()], expected_head[..], "{args:?}");
    assert_eq!(d["effort_requested"], "medium", "{d}");
    assert!(!args.contains(&"--add-dir".to_string()), "{args:?}");
    assert_eq!(PathBuf::from(&args[expected_head.len()]), wt);
    assert_eq!(args.len(), expected_head.len() + 2, "the prompt is the last argument");
    let prompt = std::fs::read_to_string(PathBuf::from(d["run_dir"].as_str().unwrap()).join("prompt.txt")).unwrap();
    assert_eq!(args.last().unwrap(), "-");
    assert!(prompt.contains("## Acceptance criteria"));
    // AC1: the codex prompt tells the worker not to commit
    assert!(!prompt.contains("commit your work"), "{prompt}");
    assert!(prompt.contains("Do not commit: leave your changes in the worktree"), "{prompt}");
    assert_eq!(
        exec["codex_home"].as_str().map(|p| std::fs::canonicalize(p).unwrap()),
        Some(std::fs::canonicalize(&cfg).unwrap())
    );

    // no hooks for codex: no settings file, no inbox
    let run_dir = PathBuf::from(d["run_dir"].as_str().unwrap());
    assert!(!run_dir.join("hooks-settings.json").exists());
    assert!(!run_dir.join("inbox").exists());

    // AC3: the thread id is the session id
    let stream = std::fs::read_to_string(run_dir.join("stream.jsonl")).unwrap();
    let first: serde_json::Value = serde_json::from_str(stream.lines().next().unwrap()).unwrap();
    let thread_id = first["thread_id"].as_str().unwrap().to_string();
    let (_, _, _, session) = run_row(&f, &run_id);
    assert_eq!(session.as_deref(), Some(thread_id.as_str()));

    // AC3: one command row per command_execution, with codex's real exit code
    let rows = f.evidence_rows(&task_id);
    let commands: Vec<&serde_json::Value> = rows.iter().filter(|r| r["kind"] == "command").collect();
    assert_eq!(commands.len(), 3, "{rows:#?}");
    for c in &commands {
        assert_eq!(c["detail"]["event"], "codex_command_execution", "{c}");
        assert_eq!(c["run_id"].as_str(), Some(run_id.as_str()));
        assert_eq!(c["label"], c["detail"]["cmd"], "label is the command: {c}");
    }
    assert_eq!(commands[0]["label"], "/bin/bash -lc 'echo hello-stdout'");
    assert_eq!(commands[0]["detail"]["exit_code"], 0);
    assert_eq!(commands[0]["detail"]["status"], "completed");
    assert_eq!(commands[0]["detail"]["truncated_inline"], false);
    assert_eq!(commands[1]["detail"]["exit_code"], 3);
    assert_eq!(commands[1]["detail"]["status"], "failed");
    assert_eq!(commands[2]["detail"]["truncated_inline"], true, "the omitted-bytes marker");
    // the blob is aggregated_output
    let blob = std::fs::read(commands[1]["blob_path"].as_str().unwrap()).unwrap();
    assert_eq!(blob, b"oops\n");

    // AC4: diff and commit evidence, and the cited result.json went through ingest
    assert!(rows.iter().any(|r| r["kind"] == "diff"), "{rows:#?}");
    assert!(rows.iter().any(|r| r["kind"] == "commit"), "{rows:#?}");
    assert!(event_detail(&f, &run_id, "result_ingested").is_some());

    // AC2/AC3 (C1b): thoth committed the file the worker left uncommitted,
    // naming the task's title and the run id, and that commit is the candidate
    let candidate: String = f
        .state_conn()
        .query_row("SELECT candidate_commit FROM runs WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert!(wt.join("codex_added.txt").is_file());
    let files = common::git(&wt, &["show", "--stat", &candidate]);
    assert!(files.contains("codex_added.txt"), "{files}");
    let subject = common::git(&wt, &["show", "-s", "--format=%s", &candidate]);
    assert!(subject.contains("say hello"), "{subject}");
    assert!(subject.contains(&run_id), "{subject}");
    let status = common::git(&wt, &["status", "--porcelain"]);
    assert_eq!(status, "", "the worktree must be clean after thoth's commit");
    let index: Vec<serde_json::Value> = serde_json::from_str(
        &std::fs::read_to_string(wt.join(".agent").join("evidence-index.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(index.len(), rows.len());

    // AC5: run_usage from turn.completed, keyed by the requested model
    let usage = event_detail(&f, &run_id, "run_usage").expect("run_usage");
    assert_eq!(usage["models"], serde_json::json!({ MODEL: 157 }), "{usage}");
    assert_eq!(usage["subagents_spawned"], 0, "{usage}");
    assert_eq!(usage["unexpected_item_types"], serde_json::json!([]), "{usage}");
    assert!(event_detail(&f, &run_id, "subagents_spawned").is_none());

    // AC6: the worker's real image is on record for cancel, read when
    // thread.started arrives (the npm launcher has settled by then), not at
    // spawn
    let started = event_detail(&f, &run_id, "run_started").unwrap();
    assert_eq!(started["worker_image"], serde_json::Value::Null, "{started}");
    let init = event_detail(&f, &run_id, "harness_init").unwrap();
    assert_eq!(
        common::image_stem(init["worker_image"].as_str().expect("worker_image recorded")),
        common::image_stem(env!("CARGO_BIN_EXE_fake-codex")),
        "{init}"
    );
    let harness_bin: Option<String> = f
        .state_conn()
        .query_row("SELECT harness_bin FROM runs WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert!(harness_bin.unwrap().contains("fake-codex"));
}

// C1b (scope §2.6): thoth commits on the codex worker's behalf ----------

#[test]
fn a_codex_run_with_no_changes_commits_nothing_and_the_candidate_is_the_base() {
    let (f, _cfg, task_id) = setup();
    let base_commit = common::git(&f.repo_path(), &["rev-parse", "HEAD"]);
    let d = dispatch_codex(&f, &task_id, "no_changes", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let wt = PathBuf::from(d["worktree"].as_str().unwrap());
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    let candidate: String = f
        .state_conn()
        .query_row("SELECT candidate_commit FROM runs WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(candidate, base_commit, "no change means the candidate is the base");
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "");
    let rows = f.evidence_rows(&task_id);
    assert!(rows.iter().any(|r| r["kind"] == "diff"), "{rows:#?}");
    assert!(rows.iter().any(|r| r["kind"] == "commit"), "{rows:#?}");
}

#[test]
fn a_codex_worker_that_commits_anyway_leaves_thoth_nothing_left_to_commit() {
    let (f, _cfg, task_id) = setup();
    let d = dispatch_codex(&f, &task_id, "commits_anyway", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let wt = PathBuf::from(d["worktree"].as_str().unwrap());
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    let candidate: String = f
        .state_conn()
        .query_row("SELECT candidate_commit FROM runs WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    // the worker's own commit is the candidate: thoth added nothing on top
    let worker_subject = common::git(&wt, &["show", "-s", "--format=%s", &candidate]);
    assert_eq!(worker_subject, "fake codex commit");
    assert!(!worker_subject.contains(&task_id));
    assert_eq!(common::git(&wt, &["status", "--porcelain"]), "");
    let rows = f.evidence_rows(&task_id);
    assert!(rows.iter().any(|r| r["kind"] == "commit"), "{rows:#?}");
}

// AC4 ---------------------------------------------------------------------

#[test]
fn a_failed_codex_turn_settles_failed_with_the_error_text() {
    let (f, _cfg, task_id) = setup();
    let d = dispatch_codex(&f, &task_id, "error", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (_, status) = f.wait_for(&task_id, "120s");
    assert_eq!(status, "failed");
    let (rstatus, reason, err, _) = run_row(&f, &run_id);
    assert_eq!(rstatus, "failed");
    assert_eq!(reason, None);
    let err = err.unwrap();
    assert!(err.contains("model is not supported"), "{err}");
}

#[test]
fn a_codex_usage_limit_interrupts_and_resume_relaunches_with_exec_resume() {
    let (f, _cfg, task_id) = setup();
    let d = dispatch_codex(&f, &task_id, "quota", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");
    let (rstatus, reason, _, session) = run_row(&f, &run_id);
    assert_eq!(rstatus, "interrupted");
    assert_eq!(reason.as_deref(), Some("quota"));
    let thread_id = session.expect("the thread id is the resume handle");

    // the evidence captured before the limit is kept
    assert_eq!(f.evidence_rows(&task_id).len(), 1);

    f.cmd().args(["profile", "check", "codex-a"]).assert().success();
    f.cmd()
        .env("FAKE_SCENARIO", "resume")
        .env("FAKE_TASK_ID", &task_id)
        .env("FAKE_CODEX_ARGS_LOG", args_log(&f))
        .args(["resume", &run_id, "--json"])
        .assert()
        .success();
    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!((code, status.as_str()), (0, "in_review"));

    let execs = exec_invocations(&f);
    assert_eq!(execs.len(), 2, "{execs:?}");
    let resumed = &execs[1];
    assert_eq!(resumed[0], "exec");
    // every option before `resume <thread> <prompt>`, as the real codex needs
    let n = resumed.len();
    assert_eq!(resumed[n - 3..n - 1], ["resume".to_string(), thread_id.clone()], "{resumed:?}");
    let at = |opt: &str| resumed.iter().position(|a| a == opt).unwrap();
    assert!(at("-C") < n - 3, "{resumed:?}");
    assert!(!resumed.contains(&"--add-dir".to_string()), "{resumed:?}");
    assert!(resumed.contains(&"sandbox_mode=\"workspace-write\"".to_string()), "{resumed:?}");
    assert!(!resumed.contains(&"-s".to_string()), "resume does not take -s: {resumed:?}");
    assert!(resumed.contains(&"--ignore-user-config".to_string()));
    assert_eq!(resumed.last().unwrap(), "-");
    let resumed_prompt = std::fs::read_to_string(PathBuf::from(d["run_dir"].as_str().unwrap()).join("prompt.txt")).unwrap();
    assert!(resumed_prompt.contains("previous session was interrupted"), "{resumed_prompt}");
    let (_, _, _, session) = run_row(&f, &run_id);
    assert_eq!(session.as_deref(), Some(thread_id.as_str()), "resume keeps the thread");
}

#[test]
fn resume_of_a_codex_run_refuses_a_claude_model() {
    let (f, _cfg, task_id) = setup();
    let d = dispatch_codex(&f, &task_id, "quota", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");
    f.state_conn()
        .execute("UPDATE runs SET model = 'sonnet' WHERE run_id = ?1", [&run_id])
        .unwrap();
    f.cmd()
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Claude model"));
    assert_eq!(run_row(&f, &run_id).0, "interrupted");
}

// AC5 ---------------------------------------------------------------------

#[test]
fn unexpected_codex_items_are_counted_and_warned_without_changing_the_outcome() {
    let (f, _cfg, task_id) = setup();
    let d = dispatch_codex(&f, &task_id, "ok", &[("FAKE_CODEX_EXTRA_ITEMS", "1")]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let usage = event_detail(&f, &run_id, "run_usage").unwrap();
    assert_eq!(usage["subagents_spawned"], 1, "{usage}");
    assert_eq!(
        usage["unexpected_item_types"],
        serde_json::json!(["collab_tool_call", "mcp_tool_call"]),
        "{usage}"
    );
    let warn = event_detail(&f, &run_id, "subagents_spawned").expect("a warning event");
    assert_eq!(warn["spawned"], 1, "{warn}");
}

// AC6 ---------------------------------------------------------------------

#[test]
fn cancel_stops_a_slow_codex_run() {
    let (f, _cfg, task_id) = setup();
    let d = dispatch_codex(&f, &task_id, "slow", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while run_row(&f, &run_id).0 != "running" {
        assert!(std::time::Instant::now() < deadline, "never started");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let out = f
        .cmd()
        .args(["cancel", &run_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["status"], "cancelled", "{v}");
    assert_eq!(f.wait_for(&task_id, "60s").1, "cancelled");
}

/// The npm `codex` is `#!/usr/bin/env node`: its pid's comm reads `codex`
/// right after spawn and `node` once the launcher has exec'd. fake-codex
/// imitates that by re-exec'ing itself through a symlink named `node`, so the
/// image recorded for cancel must be the settled one.
#[cfg(target_os = "linux")]
#[test]
fn the_worker_image_is_read_after_the_codex_launcher_settles() {
    let (f, _cfg, task_id) = setup();
    let node = f.home_path().join("launcher").join("node");
    std::fs::create_dir_all(node.parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_fake-codex"), &node).unwrap();
    let d = dispatch_codex(
        &f,
        &task_id,
        "ok",
        &[("FAKE_CODEX_LAUNCHER", node.to_str().unwrap())],
    );
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let init = event_detail(&f, &run_id, "harness_init").unwrap();
    assert_eq!(init["worker_image"], "node", "{init}");
    let started = event_detail(&f, &run_id, "run_started").unwrap();
    assert_eq!(started["worker_image"], serde_json::Value::Null, "{started}");
}

// The codex reviewer (scope §2.4) ------------------------------------------

/// A claude-code implementer's candidate at `in_review`: the fake-claude
/// `hooks` scenario on a two-criterion spec, plus a codex profile to review it.
/// Returns (fixture, codex config dir, task_id, implementer run_id).
fn claude_candidate() -> (common::Fixture, PathBuf, String, String) {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let cfg = f.add_codex_profile("codex-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");
    (f, cfg, task_id, run_id)
}

/// `review start --profile codex-a` with a fake-codex reviewer scenario, then
/// `wait --run` on the reviewer. Returns the `review start --json` output.
fn codex_review(f: &common::Fixture, task_id: &str, scenario: &str) -> serde_json::Value {
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", scenario)
        .env("FAKE_CODEX_ARGS_LOG", args_log(f))
        .args(["review", "start", task_id, "--profile", "codex-a", "--model", MODEL, "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: serde_json::Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    f.cmd()
        .args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();
    r
}

fn count(f: &common::Fixture, sql: &str, param: &str) -> i64 {
    f.state_conn().query_row(sql, [param], |r| r.get(0)).unwrap()
}

/// The part of a review packet that does not move while a review runs: from
/// the spec to the worker's claims (the status, the runs and the reviews
/// recorded so far change when the verdict lands).
fn stable_packet_part(packet: &str) -> String {
    let start = packet.find("## Spec").expect("a spec section");
    let end = packet.find("## Reviews recorded so far").expect("a reviews section");
    packet[start..end].to_string()
}

/// AC1-AC3 and AC4's first half: a claude-code implementer, a codex reviewer,
/// a PASS.
#[test]
fn a_codex_reviewer_reads_its_input_files_and_its_pass_settles_the_task_pass() {
    let (f, cfg, task_id, implementer_run) = claude_candidate();
    let r = codex_review(&f, &task_id, "review_pass");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    let wt = PathBuf::from(r["worktree"].as_str().unwrap());
    let run_dir = PathBuf::from(r["run_dir"].as_str().unwrap());
    assert_eq!(r["target_run_id"], serde_json::json!(implementer_run));

    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "pass"
    );

    // AC2: the launch, exactly
    let execs = exec_invocations(&f);
    assert_eq!(execs.len(), 1, "{execs:?}");
    let launched = &execs[0];
    let (prompt, flags) = launched.split_last().unwrap();
    let schema = run_dir.join("review-schema.json");
    let out = wt.join(".agent").join("review.json");
    let win_pair: Vec<&str> = if cfg!(windows) { vec!["-c", "windows.sandbox=\"unelevated\""] } else { vec![] };
    let mut expected: Vec<&str> = vec![
        "exec", "--json", "-m", MODEL, "-c", "model_reasoning_effort=\"medium\"", "-s", "read-only",
    ];
    expected.extend(win_pair);
    expected.extend([
            "-c", "approval_policy=\"never\"",
            "--disable", "multi_agent",
            "--disable", "apps",
            "--disable", "browser_use",
            "--disable", "computer_use",
            "--ignore-user-config",
            "--output-schema", schema.to_str().unwrap(),
            "-o", out.to_str().unwrap(),
            "-C", wt.to_str().unwrap(),
    ]);
    assert_eq!(flags.to_vec(), expected);
    assert_eq!(r["effort_requested"], "medium", "{r}");
    assert_eq!(prompt, "-");
    let prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    assert!(prompt.contains(".agent/review-input/packet.md"), "{prompt}");
    assert!(prompt.contains("Do not run thoth"), "{prompt}");
    let call = logged_invocations(&f).pop().unwrap();
    assert_eq!(
        call["codex_home"].as_str().map(|p| std::fs::canonicalize(p).unwrap()),
        Some(std::fs::canonicalize(&cfg).unwrap()),
        "CODEX_HOME is the codex profile's config dir"
    );

    // AC1: the schema in the run dir is the strict one measured in E3b
    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&schema).unwrap()).unwrap();
    let measured: serde_json::Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/codex/review-output-schema.json"
    )))
    .unwrap();
    assert_eq!(written, measured);

    // AC1: packet.md is `review packet`'s Markdown ...
    let input = wt.join(".agent").join("review-input");
    let packet = std::fs::read_to_string(input.join("packet.md")).unwrap();
    assert!(packet.starts_with(&format!("# Review packet: {task_id}\n")), "{packet}");
    let printed = f
        .cmd()
        .args(["review", "packet", &task_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    assert_eq!(
        stable_packet_part(&packet),
        stable_packet_part(&String::from_utf8(printed).unwrap())
    );
    // ... and evidence/<CODE>.txt is the full blob of every row it lists
    let rows = f.evidence_rows(&task_id);
    assert!(rows.len() >= 3, "{rows:#?}");
    let mut files: Vec<String> = std::fs::read_dir(input.join("evidence"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    let mut expected: Vec<String> = rows
        .iter()
        .map(|r| format!("{}.txt", r["code"].as_str().unwrap()))
        .collect();
    expected.sort();
    assert_eq!(files, expected, "one file per listed row, nothing else");
    for row in &rows {
        let code = row["code"].as_str().unwrap();
        assert!(packet.contains(&format!("- {code} | ")), "{code} missing from the packet");
        let blob = row["blob_path"]
            .as_str()
            .map(|p| String::from_utf8_lossy(&std::fs::read(p).unwrap()).into_owned())
            .unwrap_or_default();
        let file = std::fs::read_to_string(input.join("evidence").join(format!("{code}.txt"))).unwrap();
        assert_eq!(file, blob, "{code} must hold the whole stored blob");
    }

    // AC3: ingested with the ordinary rules; the strict schema's empty notes are fine
    let (verdict, reviewer, findings): (String, Option<String>, String) = f
        .state_conn()
        .query_row(
            "SELECT verdict, reviewer_run_id, findings FROM reviews",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(verdict, "PASS");
    assert_eq!(reviewer.as_deref(), Some(reviewer_run.as_str()));
    let findings: serde_json::Value = serde_json::from_str(&findings).unwrap();
    assert_eq!(findings["criteria"].as_array().unwrap().len(), 2, "{findings}");
    assert_eq!(findings["criteria"][0]["note"], "", "{findings}");
    // the reviewer's `cat`s on the stream are not evidence
    assert_eq!(
        count(&f, "SELECT COUNT(*) FROM evidence WHERE run_id = ?1", &reviewer_run),
        0
    );
    assert_eq!(run_row(&f, &reviewer_run).0, "completed");
}

#[test]
fn a_codex_reviewer_reject_settles_reject_and_its_findings_fold_into_the_rework() {
    let (f, _cfg, task_id, _) = claude_candidate();
    let r = codex_review(&f, &task_id, "review_reject");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "reject"
    );
    let verdict = event_detail(&f, &reviewer_run, "verdict").unwrap();
    assert_eq!(verdict["verdict"], "REJECT", "{verdict}");
    assert_eq!(verdict["extra_findings"], 1, "{verdict}");

    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    assert_eq!(d["rework"].as_bool(), Some(true), "{d}");
    let spec: String = f
        .state_conn()
        .query_row(
            "SELECT spec FROM runs WHERE run_id = ?1",
            [d["run_id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    let spec: serde_json::Value = serde_json::from_str(&spec).unwrap();
    let unmet = spec["rework"]["unmet"].as_array().unwrap();
    assert_eq!(unmet.len(), 1, "{spec}");
    assert!(
        unmet[0].as_str().unwrap().starts_with("AC1: no captured evidence"),
        "{spec}"
    );
    assert_eq!(spec["rework"]["extra_findings"].as_array().unwrap().len(), 1, "{spec}");
    f.wait_for(&task_id, "180s");
}

/// H7a: the strict schema makes a codex reviewer send `fix_size` and
/// `category` on every judgement; the labels of the unmet one reach
/// `review show --json`, and the met one's `"none"`s are read as no label.
#[test]
fn a_codex_reviewers_fix_size_and_category_reach_review_show() {
    let (f, _cfg, task_id, _) = claude_candidate();
    codex_review(&f, &task_id, "review_reject");
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "reject"
    );
    let out = f
        .cmd()
        .args(["review", "show", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let criteria = v["findings"]["criteria"].as_array().unwrap();
    assert_eq!(criteria.len(), 2, "{v}");
    assert_eq!(criteria[0]["fix_size"], "S", "{v}");
    assert_eq!(criteria[0]["category"], "spec_mismatch", "{v}");
    let met = criteria[1].as_object().unwrap();
    assert!(!met.contains_key("fix_size") && !met.contains_key("category"), "{v}");
    assert_eq!(v["findings"]["extra_findings"][0]["category"], "spec_mismatch", "{v}");
}

fn assert_codex_review_is_invalid(scenario: &str, reason: &str) {
    let (f, _cfg, task_id, _) = claude_candidate();
    let r = codex_review(&f, &task_id, scenario);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "unverified"
    );
    assert_eq!(count(&f, "SELECT COUNT(*) FROM reviews WHERE reviewer_run_id = ?1", &reviewer_run), 0);
    let invalid = event_detail(&f, &reviewer_run, "review_invalid").expect("a review_invalid event");
    assert_eq!(invalid["stage"], "schema", "{invalid}");
    assert!(invalid["reason"].as_str().unwrap().contains(reason), "{invalid}");
}

#[test]
fn a_malformed_codex_review_settles_unverified_with_review_invalid() {
    assert_codex_review_is_invalid("review_bad", "not valid JSON");
}

#[test]
fn an_uncited_codex_pass_settles_unverified_with_review_invalid() {
    assert_codex_review_is_invalid("review_uncited", "is met but cites no evidence");
}

#[test]
fn review_start_on_codex_refuses_a_claude_model_name() {
    let (f, _cfg, task_id, _) = claude_candidate();
    for model in [None, Some("haiku"), Some("claude-sonnet-5")] {
        let mut c = f.cmd();
        c.args(["review", "start", &task_id, "--profile", "codex-a"]);
        match model {
            Some(m) => {
                c.args(["--model", m]);
                c.assert()
                    .failure()
                    .stderr(predicate::str::contains("Claude model"))
                    .stderr(predicate::str::contains("gpt-5.6-luna"));
            }
            // RJ-1: no default model any more; a left-out --model is refused
            None => {
                c.assert().failure().stderr(predicate::str::contains("--model"));
            }
        }
    }
    assert_eq!(count(&f, "SELECT COUNT(*) FROM runs WHERE role = ?1", "reviewer"), 0);
    assert_eq!(f.task_status(&task_id), "in_review");
}

/// AC4's second half: a codex implementer's candidate reviewed by a
/// claude-code reviewer, with no codex-specific code on the reviewer side.
#[test]
fn a_codex_implementer_is_reviewed_by_a_claude_code_reviewer() {
    let (f, _cfg, task_id) = setup();
    f.add_profile("claude-b");
    dispatch_codex(&f, &task_id, "ok", &[]);
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let r = f.review_start(&task_id, "claude-b", "review_pass", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();
    assert_eq!(
        f.wait_for_task_status(&task_id, &["pass", "reject", "unverified"], 30),
        "pass"
    );
    let verdict = event_detail(&f, &reviewer_run, "verdict").unwrap();
    assert_eq!(verdict["verdict"], "PASS", "{verdict}");
    assert_eq!(exec_invocations(&f).len(), 1, "only the implementer ran codex");
}
