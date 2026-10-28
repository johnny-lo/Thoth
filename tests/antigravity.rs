mod common;

use common::{fixture, git};
use predicates::prelude::*;
use assert_cmd::prelude::*;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::process::Command;
use std::process::Stdio;

const MODEL: &str = "gemini-3.8-flash-low";

/// `Fixture::add_agy_profile`: registers an antigravity profile.
trait AgyFixture {
    fn add_agy_profile(&self, id: &str) -> std::path::PathBuf;
}

impl AgyFixture for common::Fixture {
    fn add_agy_profile(&self, id: &str) -> std::path::PathBuf {
        let cfg = self.home.path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        self.cmd().args(["profile", "add", id, "--provider", "google", "--harness",
            "antigravity", "--config-dir"]).arg(&cfg).assert().success();
        cfg
    }
}

fn run_fake(repo: &std::path::Path, data: &std::path::Path, scenario: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_fake-agy"))
        .args([
            "-p", "do the task", "--output-format", "stream-json", "--model", MODEL,
            "--dangerously-skip-permissions",
        ])
        .env("FAKE_AGY_DATA_DIR", data)
        .env("FAKE_SCENARIO", scenario)
        .current_dir(repo)
        .output()
        .unwrap()
}

#[test]
fn fake_ok_emits_fixture_shaped_stream_transcript_and_commit() {
    let f = fixture();
    let data = tempfile::tempdir().unwrap();
    let out = run_fake(f.repo.path(), data.path(), "ok");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let rows: Vec<Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.first().unwrap()["event"], "init");
    assert_eq!(rows.last().unwrap()["result"]["status"], "SUCCESS");
    assert_eq!(rows.last().unwrap()["result"]["usage"]["output_tokens"], 639);
    assert!(rows.iter().any(|row| {
        row["step_update"]["step_index"] == 9
            && row["step_update"]["text_delta"] == "DONE\n"
    }));
    let id = rows[0]["conversation_id"].as_str().unwrap();
    let transcript = data.path().join("brain").join(id)
        .join(".system_generated").join("logs").join("transcript_full.jsonl");
    let text = std::fs::read_to_string(transcript).unwrap();
    for (step, code) in [(4, 0), (6, 4), (8, 0)] {
        assert!(text.contains(&format!("\"step_index\":{step}")));
        assert!(text.contains(&format!("The command exited with code {code}.")));
    }
    assert_eq!(git(f.repo.path(), &["show", "HEAD:agy_added.txt"]), "agy added");
}

#[test]
fn fake_background_records_running_task_and_completion() {
    let f = fixture();
    let data = tempfile::tempdir().unwrap();
    let out = run_fake(f.repo.path(), data.path(), "background");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let first: Value = serde_json::from_str(
        String::from_utf8(out.stdout).unwrap().lines().next().unwrap(),
    )
    .unwrap();
    let id = first["conversation_id"].as_str().unwrap();
    let transcript = data
        .path()
        .join("brain")
        .join(id)
        .join(".system_generated")
        .join("logs")
        .join("transcript_full.jsonl");
    let text = std::fs::read_to_string(transcript).unwrap();
    assert!(text.contains(r#""step_index":4"#));
    assert!(text.contains(r#""status":"RUNNING""#));
    assert!(text.contains("task-4"));
    assert!(text.contains("The command exited with code 5."));
}

#[test]
fn fake_auxiliary_scenarios_have_the_promised_shapes() {
    let f = fixture();

    let no_exit_data = tempfile::tempdir().unwrap();
    let no_exit = run_fake(f.repo.path(), no_exit_data.path(), "no_exit_code");
    assert!(no_exit.status.success());
    let no_exit_text = find_transcript(no_exit_data.path());
    assert!(no_exit_text.contains(r#""step_index":4"#));
    assert!(!no_exit_text.contains("The command exited with code 0.\nOutput:\nagy-ok"));

    let quota_data = tempfile::tempdir().unwrap();
    let quota = run_fake(f.repo.path(), quota_data.path(), "quota");
    assert_eq!(quota.status.code(), Some(1));
    assert!(String::from_utf8(quota.stdout).unwrap().contains("RESOURCE_EXHAUSTED"));

    let resume_data = tempfile::tempdir().unwrap();
    let resume_without_id = run_fake(f.repo.path(), resume_data.path(), "resume");
    assert_eq!(resume_without_id.status.code(), Some(1));
    let resume = Command::new(env!("CARGO_BIN_EXE_fake-agy"))
        .args([
            "-p", "resume", "--output-format", "stream-json", "--model", MODEL,
            "--dangerously-skip-permissions", "--conversation", "resume-id",
        ])
        .env("FAKE_AGY_DATA_DIR", resume_data.path())
        .env("FAKE_SCENARIO", "resume")
        .current_dir(f.repo.path())
        .output()
        .unwrap();
    assert!(resume.status.success(), "{}", String::from_utf8_lossy(&resume.stderr));
    assert!(String::from_utf8(resume.stdout).unwrap().contains(r#""conversation_id":"resume-id""#));

    let subagent_data = tempfile::tempdir().unwrap();
    let subagent = run_fake(f.repo.path(), subagent_data.path(), "subagent");
    assert!(subagent.status.success());
    let rows: Vec<Value> = String::from_utf8(subagent.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let states: Vec<&str> = rows
        .iter()
        .filter(|row| row["step_update"]["tool_name"] == "invoke_subagent")
        .filter_map(|row| row["step_update"]["state"].as_str())
        .collect();
    assert_eq!(states, ["ACTIVE", "DONE"]);

    let crash_data = tempfile::tempdir().unwrap();
    let crash = run_fake(f.repo.path(), crash_data.path(), "crash");
    assert_eq!(crash.status.code(), Some(137));
    let crash_rows: Vec<Value> = String::from_utf8(crash.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(crash_rows.len(), 1);
    assert_eq!(crash_rows[0]["event"], "init");
}

#[test]
fn fake_slow_streams_incremental_agent_responses() {
    let f = fixture();
    let data = tempfile::tempdir().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_fake-agy"))
        .args([
            "-p", "slow", "--output-format", "stream-json", "--model", MODEL,
            "--dangerously-skip-permissions",
        ])
        .env("FAKE_AGY_DATA_DIR", data.path())
        .env("FAKE_SCENARIO", "slow")
        .current_dir(f.repo.path())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut init = String::new();
    let mut update = String::new();
    reader.read_line(&mut init).unwrap();
    reader.read_line(&mut update).unwrap();
    child.kill().unwrap();
    child.wait().unwrap();
    assert_eq!(serde_json::from_str::<Value>(&init).unwrap()["event"], "init");
    let update: Value = serde_json::from_str(&update).unwrap();
    assert_eq!(update["event"], "step_update");
    assert_eq!(update["step_update"]["step_type"], "agent_response");
}

fn find_transcript(data: &std::path::Path) -> String {
    let brain = data.join("brain");
    let conversation = std::fs::read_dir(brain).unwrap().next().unwrap().unwrap().path();
    std::fs::read_to_string(
        conversation.join(".system_generated").join("logs").join("transcript_full.jsonl"),
    )
    .unwrap()
}

#[test]
fn fake_validates_arguments_bad_models_and_logged_out_models() {
    let f = fixture();
    let data = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_fake-agy"))
        .args(["-p", "x", "--output-format", "stream-json", "--model", MODEL])
        .env("FAKE_AGY_DATA_DIR", data.path()).current_dir(f.repo.path())
        .assert().failure().code(2).stdout(predicate::str::is_empty());
    Command::new(env!("CARGO_BIN_EXE_fake-agy"))
        .args(["-p", "x", "--output-format", "stream-json", "--model", "no-such-model",
            "--dangerously-skip-permissions"])
        .env("FAKE_AGY_DATA_DIR", data.path()).current_dir(f.repo.path())
        .assert().failure().code(1).stdout(predicate::str::contains("\"status\":\"ERROR\""));
    Command::new(env!("CARGO_BIN_EXE_fake-agy"))
        .arg("models").env("FAKE_AGY_LOGGED_OUT", "1").current_dir(f.repo.path())
        .assert().success().stdout(predicate::str::contains("Please sign in"));
}

#[test]
fn fake_requires_the_json_schema_file_to_exist() {
    let f = fixture();
    let data = tempfile::tempdir().unwrap();
    let launch = |schema: &std::path::Path| {
        let mut c = Command::new(env!("CARGO_BIN_EXE_fake-agy"));
        c.args(["-p", "x", "--output-format", "stream-json", "--model", MODEL,
            "--dangerously-skip-permissions", "--json-schema"])
            .arg(schema)
            .env("FAKE_AGY_DATA_DIR", data.path())
            .env("FAKE_SCENARIO", "review_no_output")
            .current_dir(f.repo.path());
        c
    };
    launch(&data.path().join("missing.json"))
        .assert().failure().code(2).stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("--json-schema"));
    let schema = data.path().join("schema.json");
    std::fs::write(&schema, "{}").unwrap();
    let out = launch(&schema).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("file not found"), "the missing packet is an ERROR step: {stdout}");
    let last: Value = serde_json::from_str(stdout.lines().last().unwrap()).unwrap();
    assert_eq!(last["result"]["status"], "SUCCESS", "{last}");
    assert!(last["result"].get("structured_output").is_none(), "{last}");
}

#[test]
fn antigravity_profile_registers_and_checks_without_harness_env_vars() {
    let f = fixture();
    let cfg = f.home.path().join("agy-data");
    std::fs::create_dir_all(&cfg).unwrap();
    let log = f.home.path().join("agy-args.jsonl");
    f.cmd().args(["profile", "add", "agy-a", "--provider", "google", "--harness",
        "antigravity", "--config-dir"]).arg(&cfg).assert().success();
    f.cmd().args(["profile", "check", "agy-a", "--json"])
        .env("FAKE_AGY_ARGS_LOG", &log).assert().success()
        .stdout(predicate::str::contains("\"logged_in\":true"));
    let row: Value = serde_json::from_str(std::fs::read_to_string(log).unwrap().trim()).unwrap();
    assert_eq!(row["args"], serde_json::json!(["models"]));
    assert!(row["env"]["CLAUDE_CONFIG_DIR"].is_null());
    assert!(row["env"]["CODEX_HOME"].is_null());
    assert_eq!(row["env"]["HOME"], std::env::var("HOME").unwrap());
}

#[test]
fn antigravity_alias_and_codex_models_are_refused_before_the_runner_gate() {
    let f = fixture();
    f.cmd().arg("init").assert().success();
    f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    // MODELS-1: the refusal lists the stored list; with none stored yet it
    // says so and names `profile check`.
    f.cmd()
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", "sonnet"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no model list stored; run `thoth profile check agy-a`"));
    f.cmd().args(["profile", "check", "agy-a"]).assert().success();
    for model in [Some("sonnet"), Some("gpt-5.6-luna"), None] {
        let mut cmd = f.cmd();
        cmd.args(["dispatch", &task_id, "--profile", "agy-a"]);
        match model {
            Some(model) => {
                cmd.args(["--model", model]);
                cmd.assert().failure().stderr(predicate::str::contains("gemini-3.8-flash-low"));
            }
            // RJ-1: no default model any more; a left-out --model is refused
            None => {
                cmd.assert().failure().stderr(predicate::str::contains("--model"));
            }
        }
    }
    assert_eq!(f.task_status(&task_id), "queued");
}

#[test]
fn antigravity_resume_applies_the_model_rule_before_writing() {
    let f = fixture();
    f.cmd().arg("init").assert().success();
    f.add_agy_profile("agy-a");
    // MODELS-1: the refusal lists the stored list, so store one first.
    f.cmd().args(["profile", "check", "agy-a"]).assert().success();
    let (task_id, run_id, _) = f.seed_run("agy-a", common::Fixture::SPEC);
    f.state_conn()
        .execute(
            "UPDATE runs SET status='interrupted', started_at='2026-09-29T00:00:00Z', \
             model='sonnet' WHERE run_id=?1",
            [&run_id],
        )
        .unwrap();
    f.cmd()
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Claude alias"))
        .stderr(predicate::str::contains("gemini-3.8-flash-low"));
    let status: String = f
        .state_conn()
        .query_row("SELECT status FROM runs WHERE run_id=?1", [&run_id], |row| row.get(0))
        .unwrap();
    assert_eq!(status, "interrupted");
    assert_eq!(f.task_status(&task_id), "queued");
}

// The runner (G1b): agy as an implementer, driven end to end through fake-agy.

const CONV: &str = "11111111-2222-4333-8444-555555555555";

fn agy_args_log(f: &common::Fixture) -> std::path::PathBuf {
    f.home_path().join("fake-agy-args.jsonl")
}

/// Every `-p` launch fake-agy logged, oldest first (not `agy models`).
fn stream_invocations(f: &common::Fixture) -> Vec<Value> {
    std::fs::read_to_string(agy_args_log(f))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str::<Value>(l).unwrap())
        .filter(|v| v["args"][0] == "-p")
        .collect()
}

fn agy_setup() -> (common::Fixture, std::path::PathBuf, String) {
    let f = fixture();
    f.cmd().arg("init").assert().success();
    let cfg = f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC);
    (f, cfg, task_id)
}

fn agy_env(
    f: &common::Fixture,
    cfg: &std::path::Path,
    task_id: &str,
    scenario: &str,
) -> assert_cmd::Command {
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", scenario)
        .env("FAKE_TASK_ID", task_id)
        .env("FAKE_AGY_DATA_DIR", cfg)
        .env("FAKE_AGY_ARGS_LOG", agy_args_log(f));
    c
}

fn agy_dispatch(
    f: &common::Fixture,
    cfg: &std::path::Path,
    task_id: &str,
    scenario: &str,
    model: &str,
) -> Value {
    agy_dispatch_with(f, cfg, task_id, scenario, model, &[])
}

fn agy_dispatch_with(
    f: &common::Fixture,
    cfg: &std::path::Path,
    task_id: &str,
    scenario: &str,
    model: &str,
    extra: &[&str],
) -> Value {
    let out = agy_env(f, cfg, task_id, scenario)
        .args(["dispatch", task_id, "--profile", "agy-a", "--model", model, "--json"])
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

/// Every event of `ty` for `run_id` as (seq, detail), oldest first.
fn events(f: &common::Fixture, run_id: &str, ty: &str) -> Vec<(i64, Value)> {
    let conn = f.state_conn();
    let mut stmt = conn
        .prepare("SELECT seq, detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq")
        .unwrap();
    stmt.query_map([run_id, ty], |r| {
        let detail: String = r.get(1)?;
        Ok((r.get::<_, i64>(0)?, serde_json::from_str(&detail).unwrap()))
    })
    .unwrap()
    .collect::<rusqlite::Result<Vec<_>>>()
    .unwrap()
}

fn last_event(f: &common::Fixture, run_id: &str, ty: &str) -> Option<Value> {
    events(f, run_id, ty).pop().map(|(_, d)| d)
}

/// The seq of the `evidence_added` event that minted `code`.
fn added_seq(f: &common::Fixture, code: &str) -> i64 {
    f.state_conn()
        .query_row(
            "SELECT seq FROM events WHERE type = 'evidence_added' AND json_extract(detail, '$.code') = ?1",
            [code],
            |r| r.get(0),
        )
        .unwrap()
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

fn command_rows(f: &common::Fixture, task_id: &str) -> Vec<Value> {
    f.evidence_rows(task_id)
        .into_iter()
        .filter(|r| r["kind"] == "command")
        .collect()
}

fn row_for_step(rows: &[Value], step: i64) -> Value {
    rows.iter()
        .find(|r| r["detail"]["step_index"] == step)
        .unwrap_or_else(|| panic!("no row for step {step}: {rows:#?}"))
        .clone()
}

fn blob(row: &Value) -> Vec<u8> {
    std::fs::read(row["blob_path"].as_str().unwrap()).unwrap()
}

fn index(worktree: &std::path::Path) -> Vec<Value> {
    serde_json::from_str(
        &std::fs::read_to_string(worktree.join(".agent").join("evidence-index.json")).unwrap(),
    )
    .unwrap()
}

// AC1-AC5, AC7's negative half, AC10 --------------------------------------

#[test]
fn a_good_agy_run_reaches_in_review_with_exit_codes_from_the_transcript() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "ok", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let wt = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    let run_dir = std::path::PathBuf::from(d["run_dir"].as_str().unwrap());
    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!((code, status.as_str()), (0, "in_review"));

    // AC2: exactly one stream launch, exactly this command
    let calls = stream_invocations(&f);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    assert_eq!(
        calls[0]["args"],
        serde_json::json!(["-p", prompt, "--output-format", "stream-json", "--model", MODEL,
            "--dangerously-skip-permissions", "--print-timeout", "2100s"])
    );
    assert!(prompt.contains("When done, commit your work on the current branch"), "{prompt}");
    assert!(!prompt.contains("Do not commit"), "{prompt}");
    assert_eq!(
        std::fs::canonicalize(calls[0]["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&wt).unwrap()
    );
    assert!(calls[0]["env"]["CLAUDE_CONFIG_DIR"].is_null(), "{}", calls[0]);
    assert!(calls[0]["env"]["CODEX_HOME"].is_null(), "{}", calls[0]);
    assert_eq!(calls[0]["env"]["HOME"], std::env::var("HOME").unwrap());
    assert!(!run_dir.join("hooks-settings.json").exists());
    assert!(!run_dir.join("inbox").exists());
    let (harness_bin, worktree_col): (String, String) = f
        .state_conn()
        .query_row("SELECT harness_bin, worktree FROM runs WHERE run_id = ?1", [&run_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert!(harness_bin.contains("fake-agy"), "{harness_bin}");

    // AC3: the conversation id is the session, learned from `init`
    assert_eq!(run_row(&f, &run_id).3.as_deref(), Some(CONV));
    let init = last_event(&f, &run_id, "harness_init").unwrap();
    assert_eq!(init["session_id"], CONV, "{init}");
    assert_eq!(init["model"], MODEL, "{init}");
    let started = last_event(&f, &run_id, "run_started").unwrap();
    assert_eq!(started["harness"], "antigravity", "{started}");
    assert!(started["session_id"].is_null(), "{started}");
    assert_eq!(
        common::image_stem(started["worker_image"].as_str().expect("worker_image")),
        common::image_stem(env!("CARGO_BIN_EXE_fake-agy"))
    );

    // AC4: one row per run_command DONE step, codes from the transcript
    let rows = f.evidence_rows(&task_id);
    let commands = command_rows(&f, &task_id);
    assert_eq!(commands.len(), 3, "{rows:#?}");
    let steps: Vec<i64> = commands.iter().map(|c| c["detail"]["step_index"].as_i64().unwrap()).collect();
    assert_eq!(steps, [4, 6, 8]);
    let codes: Vec<i64> = commands.iter().map(|c| c["detail"]["exit_code"].as_i64().unwrap()).collect();
    assert_eq!(codes, [0, 4, 0]);
    for c in &commands {
        let dt = &c["detail"];
        assert_eq!(c["collected_by"], "control_plane", "{c}");
        assert_eq!(c["label"], dt["cmd"], "{c}");
        assert_eq!(dt["cwd"], worktree_col.as_str(), "{c}");
        assert_eq!(dt["event"], "antigravity_command", "{c}");
        assert_eq!(dt["exit_code_source"], "generic", "{c}");
        assert_eq!(dt["session_id"], CONV, "{c}");
        assert_eq!(dt["blob_source"], "stream", "{c}");
        let want = if dt["exit_code"] == 0 { "completed" } else { "failed" };
        assert_eq!(dt["status"], want, "{c}");
    }
    assert_eq!(commands[0]["label"], "echo agy-ok");
    assert_eq!(commands[1]["detail"]["cmd"], "echo agy-fail && exit 4");
    assert_eq!(blob(&commands[0]), b"agy-ok\r\n");
    assert_eq!(blob(&commands[1]), b"agy-fail\r\n");

    // AC5: recorded live, before result.json was written and ingested
    assert!(rows.iter().any(|r| r["kind"] == "diff"), "{rows:#?}");
    assert!(rows.iter().any(|r| r["kind"] == "commit"), "{rows:#?}");
    assert_eq!(index(&wt).len(), rows.len());
    let ingested = events(&f, &run_id, "result_ingested");
    assert_eq!(ingested.len(), 1, "{ingested:?}");
    let (ingested_seq, ingested_detail) = &ingested[0];
    let refs = ingested_detail["evidence_refs"].as_object().unwrap();
    assert!(
        refs.values().any(|codes| codes.as_array().is_some_and(|c| !c.is_empty())),
        "the claim cites a code: {ingested_detail}"
    );
    for c in &commands {
        assert!(added_seq(&f, c["code"].as_str().unwrap()) < *ingested_seq);
    }

    // AC7: nothing was unknown
    assert!(events(&f, &run_id, "command_exit_code_unknown").is_empty());

    // AC10: usage from the step usage, tokens from the result line
    // AGY-1: read through `events --json` (the tool name changed to write_file)
    let usage = cli_events(&f, &task_id, &run_id, "run_usage").pop().expect("run_usage");
    assert_eq!(usage["models"], serde_json::json!({ MODEL: 639 }), "{usage}");
    assert_eq!(usage["subagents_spawned"], 0, "{usage}");
    assert_eq!(usage["tool_names"], serde_json::json!(["run_command", "write_file"]), "{usage}");
    assert!(cli_events(&f, &task_id, &run_id, "subagents_spawned").is_empty());
    let (input, output): (Option<i64>, Option<i64>) = f
        .state_conn()
        .query_row("SELECT input_tokens, output_tokens FROM runs WHERE run_id = ?1", [&run_id], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!((input, output), (Some(100), Some(639)));
    let out = f
        .cmd()
        .args(["status", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let entry = v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run_id.as_str())
        .unwrap()
        .clone();
    assert_eq!(entry["models_used"], serde_json::json!({ MODEL: 639 }), "{entry}");
    assert_eq!(entry["subagents_spawned"], 0, "{entry}");
}

// AC6 ---------------------------------------------------------------------

#[test]
fn a_background_command_gets_its_exit_code_from_the_task_message() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "background", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let row = row_for_step(&command_rows(&f, &task_id), 4);
    assert_eq!(row["detail"]["exit_code"], 5, "{row}");
    assert_eq!(row["detail"]["exit_code_source"], "background_task", "{row}");
    assert_eq!(row["detail"]["status"], "failed", "{row}");
    let (ingested_seq, _) = events(&f, &run_id, "result_ingested").pop().unwrap();
    assert!(added_seq(&f, row["code"].as_str().unwrap()) < ingested_seq);
}

// AC7 ---------------------------------------------------------------------

#[test]
fn a_command_whose_exit_code_never_appears_is_recorded_null_with_a_warning() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "no_exit_code", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let wt = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    assert_eq!(run_row(&f, &run_id).0, "completed");
    let row = row_for_step(&command_rows(&f, &task_id), 4);
    let code = row["code"].as_str().unwrap().to_string();
    assert!(row["detail"]["exit_code"].is_null(), "{row}");
    assert!(row["detail"]["exit_code_source"].is_null(), "{row}");
    assert!(row["detail"]["status"].is_null(), "{row}");
    let entry = index(&wt).into_iter().find(|e| e["code"] == code.as_str()).unwrap();
    assert!(entry["exit_code"].is_null(), "{entry}");

    let unknown = events(&f, &run_id, "command_exit_code_unknown");
    assert_eq!(unknown.len(), 1, "{unknown:?}");
    let (seq, detail) = &unknown[0];
    assert_eq!(detail["code"], code.as_str(), "{detail}");
    assert_eq!(detail["step_index"], 4, "{detail}");
    assert_eq!(detail["cmd"], "echo agy-ok", "{detail}");
    assert!(detail["note"].as_str().is_some_and(|n| !n.is_empty()), "{detail}");
    let rows = f.evidence_rows(&task_id);
    for kind in ["diff", "commit"] {
        let r = rows.iter().find(|r| r["kind"] == kind).unwrap();
        assert!(*seq < added_seq(&f, r["code"].as_str().unwrap()), "{kind}");
    }
    let (ingested_seq, _) = events(&f, &run_id, "result_ingested").pop().unwrap();
    assert!(*seq < ingested_seq);
}

// AC8 ---------------------------------------------------------------------

#[test]
fn an_unknown_agy_model_fails_the_run_with_no_session_to_resume() {
    let (f, cfg, task_id) = agy_setup();
    // MODELS-1: with a model list, dispatch itself would refuse the name; an
    // empty `FAKE_AGY_MODELS` means the login check reads no list, so the
    // launch is let through and agy refuses the model when it runs. AGY-1:
    // a name that carries no level needs --effort like any other model.
    let out = agy_env(&f, &cfg, &task_id, "ok")
        .env("FAKE_AGY_MODELS", "")
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", "no-such-model", "--effort", "medium", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "failed");
    let (status, reason, err, session) = run_row(&f, &run_id);
    assert_eq!(status, "failed");
    assert_eq!(reason, None);
    assert!(err.unwrap().contains("invalid model selection"));
    assert_eq!(session, None);
    // Even with the status forced to interrupted, there is no session.
    f.state_conn()
        .execute("UPDATE runs SET status = 'interrupted' WHERE run_id = ?1", [&run_id])
        .unwrap();
    f.cmd()
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no harness_session_id"));
}

#[test]
fn a_crashed_agy_run_is_interrupted_crash() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "crash", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");
    let (status, reason, _, _) = run_row(&f, &run_id);
    assert_eq!((status.as_str(), reason.as_deref()), ("interrupted", Some("crash")));
}

// agy's own print timeout ----------------------------------------------------

#[test]
fn agys_own_print_timeout_is_a_timeout_failure_and_can_be_redispatched() {
    let (f, cfg, task_id) = agy_setup();
    let out = agy_env(&f, &cfg, &task_id, "ok")
        .env("FAKE_AGY_PRINT_TIMEOUT", "1")
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", MODEL, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first: Value = serde_json::from_slice(&out).unwrap();
    let run_id = first["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "failed");
    let (status, reason, error, _) = run_row(&f, &run_id);
    assert_eq!((status.as_str(), reason.as_deref()), ("failed", Some("timeout")));
    let error = error.unwrap();
    assert!(error.contains("print timeout after 5m0s"), "{error}");
    assert!(error.contains("not thoth's wall-clock limit"), "{error}");
    assert!(events(&f, &run_id, "result_invalid").is_empty());
    let mut status_cmd = f.cmd();
    status_cmd.args(["status", &task_id, "--json"]);
    let shown: Value = serde_json::from_slice(&f.run_ok(status_cmd, "status")).unwrap();
    assert!(shown.to_string().contains("print timeout after 5m0s"), "{shown}");
    assert!(!error.contains("  "), "{error}");
    let shown_run = shown["runs"].as_array().unwrap().iter()
        .find(|r| r["run_id"] == run_id.as_str()).unwrap_or_else(|| panic!("run not in status: {shown}"));
    assert_eq!(shown_run["status"], "failed", "{shown_run}");
    assert_eq!(shown_run["interrupted_reason"], "timeout", "{shown_run}");
    assert!(shown_run["error_text"].as_str().unwrap().contains("print timeout after 5m0s"), "{shown_run}");

    let second = agy_dispatch(&f, &cfg, &task_id, "ok", MODEL);
    assert_eq!(second["attempt"].as_i64(), Some(1));
    assert_eq!(second["worktree_reused"].as_bool(), Some(true));
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
}

#[test]
fn a_print_timeout_line_after_non_utf8_stderr_is_still_a_timeout_failure() {
    let (f, cfg, task_id) = agy_setup();
    let out = agy_env(&f, &cfg, &task_id, "ok")
        .env("FAKE_AGY_PRINT_TIMEOUT_BAD_BYTES", "1")
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", MODEL, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let first: Value = serde_json::from_slice(&out).unwrap();
    let run_id = first["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "failed");
    let (status, reason, error, _) = run_row(&f, &run_id);
    assert_eq!((status.as_str(), reason.as_deref()), ("failed", Some("timeout")));
    let error = error.unwrap();
    assert!(error.contains("print timeout after 5m0s"), "{error}");
}

#[test]
fn every_agy_launch_passes_print_timeout_as_seconds() {
    let (f, cfg, task_id) = agy_setup();
    agy_dispatch(&f, &cfg, &task_id, "ok", MODEL);
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let launches = stream_invocations(&f);
    assert_eq!(launches.len(), 1);
    let args: Vec<String> = launches[0]["args"].as_array().unwrap().iter()
        .map(|a| a.as_str().unwrap().to_string()).collect();
    let at = args.iter().position(|a| a == "--print-timeout").expect("--print-timeout missing");
    let value = &args[at + 1];
    assert!(value.ends_with('s') && value[..value.len() - 1].chars().all(|c| c.is_ascii_digit()), "{value}");
    assert!(value.len() > 1, "{value}");
}

// AC8 quota, AC9 ------------------------------------------------------------

#[test]
fn an_agy_quota_stop_interrupts_and_resume_continues_the_conversation() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "quota", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let run_dir = std::path::PathBuf::from(d["run_dir"].as_str().unwrap());
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");
    let (status, reason, _, session) = run_row(&f, &run_id);
    assert_eq!((status.as_str(), reason.as_deref()), ("interrupted", Some("quota")));
    assert_eq!(session.as_deref(), Some(CONV));
    let profile_status: String = f
        .global_conn()
        .query_row("SELECT status FROM profiles WHERE profile_id = 'agy-a'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(profile_status, "rate_limited");
    assert!(last_event(&f, &run_id, "quota_exceeded").is_some());
    let before = command_rows(&f, &task_id);
    assert_eq!(before.len(), 1, "{before:#?}");
    assert_eq!(before[0]["detail"]["cmd"], "echo before-the-limit");
    assert_eq!(before[0]["detail"]["exit_code"], 0);

    // resume writes the hook files again, from the row's allowed_tools
    let wt = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    std::fs::remove_file(wt.join(".agents").join("hooks.json")).unwrap();
    std::fs::remove_file(wt.join(".agent").join("agy-hook.json")).unwrap();
    f.cmd().args(["profile", "check", "agy-a"]).assert().success();
    agy_env(&f, &cfg, &task_id, "resume")
        .args(["resume", &run_id, "--json"])
        .assert()
        .success();
    let (code, status) = f.wait_for(&task_id, "120s");
    assert_eq!((code, status.as_str()), (0, "in_review"));

    let calls = stream_invocations(&f);
    assert_eq!(calls.len(), 2, "{calls:?}");
    let resume_prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    assert!(resume_prompt.contains("previous session was interrupted"), "{resume_prompt}");
    assert_eq!(
        calls[1]["args"],
        serde_json::json!(["-p", resume_prompt, "--output-format", "stream-json", "--model", MODEL,
            "--dangerously-skip-permissions", "--print-timeout", "2100s", "--conversation", CONV])
    );
    let started = last_event(&f, &run_id, "run_started").unwrap();
    assert_eq!(started["mode"], "resume", "{started}");
    assert!(wt.join(".agents").join("hooks.json").is_file());
    let policy: Value = serde_json::from_str(
        &std::fs::read_to_string(wt.join(".agent").join("agy-hook.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(policy["allowed_tools"], serde_json::json!(AGY_IMPLEMENTER_TOOLS), "{policy}");
    let after = command_rows(&f, &task_id);
    assert_eq!(after.len(), 2, "{after:#?}");
    let resumed = &after[1];
    assert!(resumed["detail"]["step_index"].as_i64().unwrap() >= 8, "{resumed}");
    assert_eq!(resumed["detail"]["session_id"], CONV, "{resumed}");
    assert_eq!(resumed["detail"]["exit_code"], 0, "{resumed}");

    let out = f
        .cmd()
        .args(["evidence", "list", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let listed: Vec<&str> = v["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["code"].as_str().unwrap())
        .collect();
    for row in &after {
        assert!(listed.contains(&row["code"].as_str().unwrap()), "{listed:?}");
    }

    // the resumed launch's usage is its StepUsage sum (328 + 280), not the result line's cumulative 1383
    let usage = last_event(&f, &run_id, "run_usage").expect("run_usage");
    assert_eq!(usage["models"], serde_json::json!({ MODEL: 608 }), "{usage}");
    let output: Option<i64> = f
        .state_conn()
        .query_row("SELECT output_tokens FROM runs WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    assert_eq!(output, Some(1383));
}

#[test]
fn resume_of_an_agy_run_refuses_a_claude_alias() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "quota", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");
    f.state_conn()
        .execute("UPDATE runs SET model = 'sonnet' WHERE run_id = ?1", [&run_id])
        .unwrap();
    f.cmd()
        .args(["resume", &run_id])
        .assert()
        .failure()
        .stderr(predicate::str::contains("gemini-3.8-flash-low"));
    assert_eq!(run_row(&f, &run_id).0, "interrupted");
}

// AC10 --------------------------------------------------------------------

#[test]
fn an_agy_subagent_is_counted_and_warned_without_changing_the_outcome() {
    let (f, cfg, task_id) = agy_setup();
    let tools = format!("{},invoke_subagent", AGY_IMPLEMENTER_TOOLS.join(","));
    let d = agy_dispatch_with(&f, &cfg, &task_id, "subagent", MODEL, &["--allowed-tools", &tools]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(d["allowed_tools"], tools.as_str(), "{d}");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    // AGY-1: read through `events --json` (the tool name changed to write_file)
    let usage = cli_events(&f, &task_id, &run_id, "run_usage").pop().unwrap();
    assert_eq!(usage["subagents_spawned"], 1, "{usage}");
    assert_eq!(usage["tools_denied"], 0, "{usage}");
    assert!(cli_events(&f, &task_id, &run_id, "tool_denied").is_empty());
    assert_eq!(
        usage["tool_names"],
        serde_json::json!(["invoke_subagent", "run_command", "write_file"]),
        "{usage}"
    );
    let warn = last_event(&f, &run_id, "subagents_spawned").expect("a warning event");
    assert_eq!(warn["spawned"], 1, "{warn}");
}

// AC11 --------------------------------------------------------------------

#[test]
fn cancel_stops_a_slow_agy_run() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "slow", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while run_row(&f, &run_id).0 != "running" {
        assert!(std::time::Instant::now() < deadline, "never started");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let worker_pid: i64 = f
        .state_conn()
        .query_row("SELECT worker_pid FROM runs WHERE run_id = ?1", [&run_id], |r| r.get(0))
        .unwrap();
    let out = f
        .cmd()
        .args(["cancel", &run_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(out).unwrap();
    let v: Value = serde_json::from_str(text.trim()).unwrap();
    assert_eq!(v["status"], "cancelled", "{v}");
    assert!(!text.contains("could not confirm"), "{text}");
    assert_eq!(f.wait_for(&task_id, "60s").1, "cancelled");
    assert!(!common::pid_alive(worker_pid as u32, env!("CARGO_BIN_EXE_fake-agy")));
}

// AC6: a transcript that is not where the runner looks never fails the run.

#[test]
fn a_missing_transcript_settles_every_command_null_without_failing_the_run() {
    let (f, _cfg, task_id) = agy_setup();
    let elsewhere = tempfile::tempdir().unwrap();
    let d = agy_dispatch(&f, elsewhere.path(), &task_id, "ok", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let run_dir = std::path::PathBuf::from(d["run_dir"].as_str().unwrap());
    f.wait_for(&task_id, "120s");
    assert_eq!(run_row(&f, &run_id).0, "completed");
    let commands = command_rows(&f, &task_id);
    assert_eq!(commands.len(), 3, "{commands:#?}");
    for c in &commands {
        assert!(c["detail"]["exit_code"].is_null(), "{c}");
    }
    assert_eq!(events(&f, &run_id, "command_exit_code_unknown").len(), 3);
    let log = std::fs::read_to_string(run_dir.join("runner-error.log")).unwrap();
    let lines: Vec<&str> = log.lines().filter(|l| l.contains("transcript")).collect();
    assert_eq!(lines.len(), 1, "{log}");
}

// G2a: the PreToolUse tool lock ---------------------------------------------

/// `AGY_IMPLEMENTER_TOOLS` (AGY-1: agy 1.4.4's `read_file`, `write_file`
/// and `edit_file` beside the 1.4.2 names), which the integration tests
/// cannot import.
const AGY_IMPLEMENTER_TOOLS: &[&str] = &[
    "run_command", "command_status", "send_command_input", "read_file", "view_file", "list_dir",
    "find_by_name", "grep_search", "write_file", "edit_file", "write_to_file",
    "replace_file_content", "multi_replace_file_content", "sed_file", "finish", "wait",
    "wait_5_seconds",
];

/// `thoth hook --agy-pre` with `stdin`, under an empty THOTH_HOME.
/// Returns (exit code, stdout, stderr).
fn agy_pre(home: &std::path::Path, stdin: &str) -> (Option<i32>, String, String) {
    let mut c = assert_cmd::Command::new(env!("CARGO_BIN_EXE_thoth"));
    for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE"] {
        c.env_remove(var);
    }
    let out = c
        .env("THOTH_HOME", home)
        .args(["hook", "--agy-pre"])
        .write_stdin(stdin.to_string())
        .output()
        .unwrap();
    (
        out.status.code(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// A recorded fixture payload with its `workspacePaths` pointed at `ws`.
fn pre_fixture(name: &str, ws: &std::path::Path) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/antigravity/hooks")
        .join(name);
    let mut v: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    v["workspacePaths"] = serde_json::json!([ws.display().to_string()]);
    v
}

fn write_policy(ws: &std::path::Path, run_id: &str, tools: &[&str]) -> std::path::PathBuf {
    write_policy_for_role(ws, run_id, "implementer", tools)
}

fn write_policy_for_role(ws: &std::path::Path, run_id: &str, role: &str, tools: &[&str]) -> std::path::PathBuf {
    let dir = ws.join(".agent");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("agy-hook.json");
    std::fs::write(
        &path,
        serde_json::json!({"run_id": run_id, "role": role, "allowed_tools": tools}).to_string(),
    )
    .unwrap();
    path
}

/// A payload shaped like agy 1.4.4's (`read_file` carries `Path`) for `tool`.
fn payload_for(ws: &std::path::Path, tool: &str) -> Value {
    let args = match tool {
        "read_file" => serde_json::json!({"Path": ws.join("a.txt").display().to_string()}),
        "view_file" => serde_json::json!({"AbsolutePath": ws.join("a.txt").display().to_string()}),
        _ => serde_json::json!({}),
    };
    serde_json::json!({
        "artifactDirectoryPath": "/x", "conversationId": "c-1", "modelName": MODEL,
        "stepIdx": 9, "toolCall": {"name": tool, "args": args},
        "transcriptPath": "/x/t.jsonl", "workspacePaths": [ws.display().to_string()],
    })
}

/// The denial lines `thoth hook --agy-pre` appended under `ws`, oldest first.
fn denial_rows(ws: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(ws.join(".agent").join("agy-hook-denials.jsonl"))
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// The single decision line: exit 0, empty stderr, one JSON line on stdout.
fn decision(home: &std::path::Path, stdin: &str) -> Value {
    let (code, stdout, stderr) = agy_pre(home, stdin);
    assert_eq!(code, Some(0), "{stdout}{stderr}");
    assert_eq!(stderr, "", "{stdout}");
    assert!(stdout.ends_with('\n'), "{stdout:?}");
    assert_eq!(stdout.lines().count(), 1, "{stdout:?}");
    let v: Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_ne!(v, serde_json::json!({}), "{stdout}");
    v
}

fn assert_deny(v: &Value, needles: &[&str]) {
    assert_eq!(v["decision"], "deny", "{v}");
    let reason = v["reason"].as_str().unwrap_or_else(|| panic!("no reason: {v}"));
    for n in needles {
        assert!(reason.contains(n), "{n:?} not in {reason:?}");
    }
    assert_eq!(v.as_object().unwrap().len(), 2, "{v}");
}

fn search_web(ws: &std::path::Path) -> Value {
    serde_json::json!({
        "artifactDirectoryPath": "/x", "conversationId": "c-1", "modelName": MODEL,
        "stepIdx": 9, "toolCall": {"name": "search_web", "args": {"query": "q"}},
        "transcriptPath": "/x/t.jsonl", "workspacePaths": [ws.display().to_string()],
    })
}

#[test]
fn agy_pre_hook_allows_listed_tools_and_denies_the_rest_with_a_reason() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    write_policy(ws.path(), "RUN1", AGY_IMPLEMENTER_TOOLS);
    for name in ["pre-5.json", "pre-6.json", "pre-7.json"] {
        let payload = pre_fixture(name, ws.path()).to_string();
        let (code, stdout, stderr) = agy_pre(home.path(), &payload);
        assert_eq!(code, Some(0), "{name}");
        assert_eq!(stderr, "", "{name}");
        assert_eq!(stdout, "{\"decision\":\"allow\"}\n", "{name}");
    }
    let v = decision(home.path(), &search_web(ws.path()).to_string());
    assert_deny(&v, &["search_web", "allow-list"]);
}

#[test]
fn agy_pre_hook_denies_with_a_reason_when_the_payload_or_policy_is_unusable() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let policy = write_policy(ws.path(), "RUN1", AGY_IMPLEMENTER_TOOLS);

    assert_deny(&decision(home.path(), "not json at all"), &["stdin"]);

    let mut nameless = pre_fixture("pre-6.json", ws.path());
    nameless["toolCall"].as_object_mut().unwrap().remove("name");
    assert_deny(&decision(home.path(), &nameless.to_string()), &["toolCall.name"]);

    for workspaces in [None, Some(serde_json::json!([])), Some(serde_json::json!([""]))] {
        let mut v = pre_fixture("pre-6.json", ws.path());
        match workspaces {
            None => {
                v.as_object_mut().unwrap().remove("workspacePaths");
            }
            Some(w) => v["workspacePaths"] = w,
        }
        assert_deny(&decision(home.path(), &v.to_string()), &["workspacePaths"]);
    }

    let empty = tempfile::tempdir().unwrap();
    let missing = empty.path().join(".agent").join("agy-hook.json");
    let v = decision(home.path(), &pre_fixture("pre-6.json", empty.path()).to_string());
    assert_deny(&v, &[&missing.display().to_string()]);

    for bad in ["not json", r#"{"run_id":"RUN1","role":"implementer"}"#, r#"{"allowed_tools":"view_file"}"#] {
        std::fs::write(&policy, bad).unwrap();
        let v = decision(home.path(), &pre_fixture("pre-6.json", ws.path()).to_string());
        assert_deny(&v, &[&policy.display().to_string()]);
    }
}

#[test]
fn agy_pre_hook_records_denials_and_never_touches_thoth_home() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    write_policy(ws.path(), "RUN1", &["view_file"]);
    let denials = ws.path().join(".agent").join("agy-hook-denials.jsonl");

    assert_eq!(decision(home.path(), &pre_fixture("pre-6.json", ws.path()).to_string())["decision"], "allow");
    assert!(!denials.exists(), "an allow appends nothing");
    let first = decision(home.path(), &pre_fixture("pre-5.json", ws.path()).to_string());
    assert_deny(&first, &["run_command", "allow-list"]);
    let mut stepless = search_web(ws.path());
    stepless.as_object_mut().unwrap().remove("stepIdx");
    stepless.as_object_mut().unwrap().remove("conversationId");
    let second = decision(home.path(), &stepless.to_string());
    assert_deny(&second, &["search_web", "allow-list"]);

    let text = std::fs::read_to_string(&denials).unwrap();
    let rows: Vec<Value> = text.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(rows.len(), 2, "{text}");
    for row in &rows {
        let mut keys: Vec<&str> = row.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        keys.sort();
        assert_eq!(keys, ["conversation_id", "known_to_thoth", "reason", "received_at", "run_id", "step_idx", "tool"], "{row}");
        assert_eq!(row["run_id"], "RUN1", "{row}");
        assert_eq!(row["known_to_thoth"], true, "{row}");
        assert!(row["received_at"].as_str().is_some_and(|t| t.contains('T')), "{row}");
    }
    assert_eq!(rows[0]["tool"], "run_command");
    assert_eq!(rows[0]["reason"], first["reason"]);
    assert_eq!(rows[0]["step_idx"], 2);
    assert_eq!(rows[0]["conversation_id"], "326a9ebd-0874-4826-b999-2309f9376ce7");
    assert_eq!(rows[1]["tool"], "search_web");
    assert!(rows[1]["step_idx"].is_null(), "{}", rows[1]);
    assert!(rows[1]["conversation_id"].is_null(), "{}", rows[1]);

    // No policy and no `.agent/` yet: the directory is created, run_id is null.
    let bare = tempfile::tempdir().unwrap();
    let mut toolless = pre_fixture("pre-6.json", bare.path());
    toolless["toolCall"].as_object_mut().unwrap().remove("name");
    decision(home.path(), &toolless.to_string());
    let text = std::fs::read_to_string(bare.path().join(".agent").join("agy-hook-denials.jsonl")).unwrap();
    let row: Value = serde_json::from_str(text.trim()).unwrap();
    assert!(row["run_id"].is_null(), "{row}");
    assert!(row["tool"].is_null(), "{row}");

    let leaked: Vec<String> = std::fs::read_dir(home.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(leaked.is_empty(), "hook --agy-pre must not create state: found {leaked:?}");
}

/// AGY-1 (AC1): the permission matrix through the hook, for the reviewer
/// and the spec reviewer policy: both file-reading generations are allowed;
/// every write, edit and shell name of the implementer list, and the
/// subagent, task and message tools, are denied with the allow-list reason
/// (they are all names Thoth knows).
#[test]
fn agy_pre_hook_matrix_for_the_two_reviewer_roles() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let mut write_or_shell: Vec<&str> = AGY_IMPLEMENTER_TOOLS
        .iter()
        .copied()
        .filter(|t| !AGY_REVIEWER_TOOLS.contains(t))
        .collect();
    assert_eq!(
        write_or_shell,
        ["run_command", "command_status", "send_command_input", "write_file", "edit_file",
            "write_to_file", "replace_file_content", "multi_replace_file_content", "sed_file"]
    );
    write_or_shell.extend(["invoke_subagent", "manage_task", "send_message"]);
    for role in ["reviewer", "spec_reviewer"] {
        write_policy_for_role(ws.path(), "RUN1", role, AGY_REVIEWER_TOOLS);
        let _ = std::fs::remove_file(ws.path().join(".agent").join("agy-hook-denials.jsonl"));
        for tool in ["read_file", "view_file"] {
            let (code, stdout, stderr) = agy_pre(home.path(), &payload_for(ws.path(), tool).to_string());
            assert_eq!(code, Some(0), "{role}/{tool}");
            assert_eq!(stderr, "", "{role}/{tool}");
            assert_eq!(stdout, "{\"decision\":\"allow\"}\n", "{role}/{tool}");
        }
        assert!(denial_rows(ws.path()).is_empty(), "{role}: an allow appends nothing");
        for tool in write_or_shell.iter().copied() {
            let v = decision(home.path(), &payload_for(ws.path(), tool).to_string());
            assert_deny(&v, &[tool, "is not on this run's allow-list"]);
            assert!(!v["reason"].as_str().unwrap().contains("Thoth knows"), "{role}/{tool}: {v}");
        }
        let rows = denial_rows(ws.path());
        assert_eq!(rows.len(), write_or_shell.len(), "{role}");
        for (row, tool) in rows.iter().zip(write_or_shell.iter().copied()) {
            assert_eq!(row["tool"], tool, "{role}: {row}");
            assert_eq!(row["known_to_thoth"], true, "{role}: {row}");
            assert_eq!(row["run_id"], "RUN1", "{role}: {row}");
        }
    }
    // the implementer policy allows the write and shell names it lists
    write_policy(ws.path(), "RUN1", AGY_IMPLEMENTER_TOOLS);
    for tool in ["read_file", "write_file", "edit_file", "run_command", "view_file", "write_to_file"] {
        let (_, stdout, _) = agy_pre(home.path(), &payload_for(ws.path(), tool).to_string());
        assert_eq!(stdout, "{\"decision\":\"allow\"}\n", "implementer/{tool}");
    }
    for tool in ["invoke_subagent", "search_web"] {
        assert_deny(&decision(home.path(), &payload_for(ws.path(), tool).to_string()), &[tool, "allow-list"]);
    }
}

/// AGY-1 (AC2): through the hook CLI, a known but disallowed name, an
/// unknown name, and a payload or policy the hook cannot use each print one
/// legal decision line and exit 0, and the denial file says which was which.
#[test]
fn agy_pre_hook_tells_an_unknown_name_from_a_disallowed_one() {
    let home = tempfile::tempdir().unwrap();
    let ws = tempfile::tempdir().unwrap();
    let policy = write_policy_for_role(ws.path(), "RUN1", "reviewer", AGY_REVIEWER_TOOLS);

    let known = decision(home.path(), &payload_for(ws.path(), "write_file").to_string());
    assert_deny(&known, &["write_file", "is not on this run's allow-list (read_file, view_file, list_dir"]);
    let unknown = decision(home.path(), &payload_for(ws.path(), "frobnicate_file").to_string());
    assert_deny(&unknown, &["the tool frobnicate_file is not a tool Thoth knows",
        "agy may have renamed its tools, so update Thoth's agy tool table (AGY-1)",
        "this run's allow-list is (read_file, view_file, list_dir, find_by_name, grep_search, finish, wait, wait_5_seconds)"]);
    // a custom implementer list allows an unknown name it lists, without a denial
    let custom = write_policy(ws.path(), "RUN1", &["frobnicate_file", "read_file"]);
    let (_, stdout, _) = agy_pre(home.path(), &payload_for(ws.path(), "frobnicate_file").to_string());
    assert_eq!(stdout, "{\"decision\":\"allow\"}\n");
    assert_eq!(custom, policy);

    // an unparsable payload names no workspace, so nothing is appended for it
    assert_deny(&decision(home.path(), "not json at all"), &["stdin"]);
    let mut nameless = payload_for(ws.path(), "frobnicate_file");
    nameless["toolCall"].as_object_mut().unwrap().remove("name");
    assert_deny(&decision(home.path(), &nameless.to_string()), &["toolCall.name"]);
    std::fs::write(&policy, "not json").unwrap();
    let bad_policy = decision(home.path(), &payload_for(ws.path(), "frobnicate_file").to_string());
    assert_deny(&bad_policy, &["not valid JSON"]);
    assert!(!bad_policy["reason"].as_str().unwrap().contains("Thoth knows"), "{bad_policy}");

    let rows = denial_rows(ws.path());
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!((rows[0]["tool"].as_str(), rows[0]["known_to_thoth"].as_bool()), (Some("write_file"), Some(true)));
    assert_eq!((rows[1]["tool"].as_str(), rows[1]["known_to_thoth"].as_bool()), (Some("frobnicate_file"), Some(false)));
    assert_eq!(rows[1]["reason"], unknown["reason"]);
    assert!(rows[2]["tool"].is_null() && rows[2]["known_to_thoth"] == false, "{}", rows[2]);
    assert_eq!((rows[3]["tool"].as_str(), rows[3]["known_to_thoth"].as_bool()), (Some("frobnicate_file"), Some(false)));
    assert!(rows[3]["reason"].as_str().unwrap().contains("not valid JSON"), "{}", rows[3]);
    assert!(std::fs::read_dir(home.path()).unwrap().next().is_none(), "hook --agy-pre must not create state");
}

fn run_allowed_tools(f: &common::Fixture, task_id: &str, run_id: &str) -> Value {
    let out = f
        .cmd()
        .args(["status", task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    v["runs"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["run_id"] == run_id)
        .unwrap()["allowed_tools"]
        .clone()
}

#[test]
fn an_agy_run_is_launched_with_a_workspace_hook_and_the_default_allow_list() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "ok", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let wt = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    let run_dir = std::path::PathBuf::from(d["run_dir"].as_str().unwrap());
    let joined = AGY_IMPLEMENTER_TOOLS.join(",");
    assert_eq!(d["allowed_tools"], joined.as_str(), "{d}");
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    assert_eq!(run_allowed_tools(&f, &task_id, &run_id), joined.as_str());

    let hooks: Value =
        serde_json::from_str(&std::fs::read_to_string(wt.join(".agents").join("hooks.json")).unwrap()).unwrap();
    let command = hooks["thoth-tool-lock"]["PreToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap_or_else(|| panic!("{hooks}"))
        .to_string();
    // Windows: agy 1.2.14 cannot run a quoted hook command, so no quotes there.
    #[cfg(windows)]
    let exe = command.strip_suffix(" hook --agy-pre").unwrap_or_else(|| panic!("{command}"));
    #[cfg(not(windows))]
    let exe = command
        .strip_prefix('"')
        .and_then(|c| c.strip_suffix("\" hook --agy-pre"))
        .unwrap_or_else(|| panic!("{command}"));
    assert!(!exe.contains('"'), "{command}");
    #[cfg(windows)]
    assert!(!command.contains('"') && !exe.contains(char::is_whitespace), "{command}");
    assert_eq!(
        std::fs::canonicalize(exe).unwrap(),
        std::fs::canonicalize(env!("CARGO_BIN_EXE_thoth")).unwrap()
    );
    assert_eq!(
        hooks,
        serde_json::json!({"thoth-tool-lock": {"PreToolUse": [{"matcher": "*", "hooks": [
            {"type": "command", "command": command, "timeout": 10}]}]}})
    );

    let policy: Value = serde_json::from_str(
        &std::fs::read_to_string(wt.join(".agent").join("agy-hook.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        policy,
        serde_json::json!({"run_id": run_id, "role": "implementer", "allowed_tools": AGY_IMPLEMENTER_TOOLS})
    );
    assert!(!run_dir.join("hooks-settings.json").exists());
    assert!(!run_dir.join("inbox").exists());

    let calls = stream_invocations(&f);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    assert_eq!(
        calls[0]["args"],
        serde_json::json!(["-p", prompt, "--output-format", "stream-json", "--model", MODEL,
            "--dangerously-skip-permissions", "--print-timeout", "2100s"])
    );
    let usage = last_event(&f, &run_id, "run_usage").expect("run_usage");
    assert_eq!(usage["tools_denied"], 0, "{usage}");
}

#[test]
fn a_denied_agy_tool_becomes_a_tool_denied_event_and_a_usage_count() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "subagent", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    // AGY-1: read through `events --json` (the tool name changed to write_file)
    let denied = cli_events_seq(&f, &task_id, &run_id, "tool_denied");
    assert_eq!(denied.len(), 1, "{denied:?}");
    let (denied_seq, detail) = &denied[0];
    assert_eq!(detail["tool"], "invoke_subagent", "{detail}");
    assert!(detail["reason"].as_str().is_some_and(|r| !r.is_empty()), "{detail}");
    assert_eq!(detail["step_index"], 9, "{detail}");
    assert_eq!(detail["conversation_id"], CONV, "{detail}");
    assert!(detail["received_at"].as_str().is_some(), "{detail}");

    let usage = cli_events(&f, &task_id, &run_id, "run_usage").pop().expect("run_usage");
    assert_eq!(usage["tools_denied"], 1, "{usage}");
    assert_eq!(usage["subagents_spawned"], 0, "{usage}");
    assert_eq!(usage["tool_names"], serde_json::json!(["run_command", "write_file"]), "{usage}");
    assert!(cli_events(&f, &task_id, &run_id, "subagents_spawned").is_empty());

    let (terminal_seq, _) =
        cli_events_seq(&f, &task_id, &run_id, "status_change").pop().expect("a terminal status_change");
    assert!(*denied_seq < terminal_seq, "{denied_seq} vs {terminal_seq}");
}

#[test]
fn agy_hook_files_never_reach_the_diff_or_the_commit() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "ok", MODEL);
    let wt = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    assert!(wt.join(".agents").join("hooks.json").is_file());
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
    let rows = f.evidence_rows(&task_id);
    for kind in ["diff", "commit"] {
        let row = rows.iter().find(|r| r["kind"] == kind).unwrap_or_else(|| panic!("no {kind}"));
        let text = String::from_utf8_lossy(&blob(row)).to_string();
        assert!(!text.contains(".agents/"), "{kind}: {text}");
        assert!(!text.contains("agy-hook"), "{kind}: {text}");
    }
}

#[test]
fn a_claude_code_run_gets_no_agents_hooks_file() {
    let f = fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(d["allowed_tools"], "Bash,Read,Write,Edit,Glob,Grep", "{d}");
    let wt = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");
    assert!(!wt.join(".agents").exists());
    assert!(!wt.join(".agent").join("agy-hook.json").exists());
    let usage = last_event(&f, &run_id, "run_usage").expect("run_usage");
    assert!(usage.get("tools_denied").is_none(), "{usage}");
}

// Attempt 2: fake-agy's matcher subset (AC8) -------------------------------

/// The tools fake-agy's `ok` scenario ran when `.agents/hooks.json` holds one
/// group with `matcher` whose handler always denies (empty stdout).
fn tools_run_under_matcher(matcher: &str) -> Vec<String> {
    let f = fixture();
    let data = tempfile::tempdir().unwrap();
    let dir = f.repo.path().join(".agents");
    std::fs::create_dir_all(&dir).unwrap();
    let doc = serde_json::json!({"deny-all": {"PreToolUse": [{"matcher": matcher, "hooks": [
        {"type": "command", "command": "exit 0", "timeout": 10}]}]}});
    std::fs::write(dir.join("hooks.json"), doc.to_string()).unwrap();
    let out = run_fake(f.repo.path(), data.path(), "ok");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let mut names: Vec<String> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter_map(|row| row["step_update"]["tool_name"].as_str().map(String::from))
        .collect();
    names.dedup();
    names
}

#[test]
fn fake_agy_matches_only_the_documented_matcher_subset() {
    for matcher in ["*", "", ".*", "run_command|write_file", "^run_command|write_file$"] {
        assert!(tools_run_under_matcher(matcher).is_empty(), "{matcher} must select every tool the fake runs");
    }
    assert_eq!(tools_run_under_matcher("write_file"), ["run_command"]);
    assert_eq!(tools_run_under_matcher("^run_command$"), ["write_file"]);
    for matcher in ["^run_command$|^write_file$", "run_.*", "^^run_command$", "(run_command)"] {
        assert_eq!(
            tools_run_under_matcher(matcher),
            ["write_file", "run_command"],
            "{matcher} is outside the subset and must not match"
        );
    }
}

// AGY-1: the unknown-name signal, the recorded init list, and `wait --run`
// (AC2, AC3), everything read back through the CLI ------------------------

/// Every event of `ty` on `run_id`, oldest first, through `events --json`,
/// as (seq, detail).
fn cli_events_seq(f: &common::Fixture, task_id: &str, run_id: &str, ty: &str) -> Vec<(i64, Value)> {
    let mut c = f.cmd();
    c.args(["events", "--since", "0", "--task", task_id, "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(c, "events")).unwrap();
    v["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["run_id"] == run_id && e["type"] == ty)
        .map(|e| (e["seq"].as_i64().unwrap(), e["detail"].clone()))
        .collect()
}

/// Every event of `ty` on `run_id`, oldest first, through `events --json`.
fn cli_events(f: &common::Fixture, task_id: &str, run_id: &str, ty: &str) -> Vec<Value> {
    cli_events_seq(f, task_id, run_id, ty).into_iter().map(|(_, d)| d).collect()
}

/// The task's status through `status TASK --json`.
fn cli_task_status(f: &common::Fixture, task_id: &str) -> String {
    let mut c = f.cmd();
    c.args(["status", task_id, "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(c, "status")).unwrap();
    v["status"].as_str().unwrap_or_else(|| panic!("no status in {v}")).to_string()
}

/// `settled` through the CLI: polls `status TASK --json` until the task is
/// `pass`, `reject` or `unverified` (30 s), and returns which.
fn settled_cli(f: &common::Fixture, task_id: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let s = cli_task_status(f, task_id);
        if ["pass", "reject", "unverified"].contains(&s.as_str()) {
            return s;
        }
        assert!(std::time::Instant::now() < deadline, "task {task_id} never settled; it is '{s}'");
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// `wait --run RUN --timeout T --json` as (exit code, document).
fn wait_run_json(f: &common::Fixture, run_id: &str, timeout: &str) -> (i32, Value) {
    let out = f.cmd().args(["wait", "--run", run_id, "--timeout", timeout, "--json"]).output().unwrap();
    let v: Value = serde_json::from_str(String::from_utf8(out.stdout).unwrap().trim())
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&out.stderr)));
    (out.status.code().unwrap(), v)
}

/// `wait --run RUN --timeout T` without `--json`: (exit code, stdout).
fn wait_run_human(f: &common::Fixture, run_id: &str, timeout: &str) -> (i32, String) {
    let out = f.cmd().args(["wait", "--run", run_id, "--timeout", timeout]).output().unwrap();
    (out.status.code().unwrap(), String::from_utf8(out.stdout).unwrap())
}

const UNKNOWN_LINE: &str = "agy called tools Thoth does not know: ";

/// The `status TASK --json` entry of one run.
fn status_run(f: &common::Fixture, task_id: &str, run_id: &str) -> Value {
    let mut c = f.cmd();
    c.args(["status", task_id, "--json"]);
    let v: Value = serde_json::from_slice(&f.run_ok(c, "status")).unwrap();
    v["runs"].as_array().unwrap().iter().find(|r| r["run_id"] == run_id).cloned()
        .unwrap_or_else(|| panic!("run {run_id} not in {v}"))
}

/// AC2: `unknown_tool` is `ok` plus one `frobnicate_file` attempt: denied as
/// a name Thoth does not know, counted, listed in `run_usage.tools_unknown`,
/// shown by `wait --run`, and the run still writes, commits and delivers.
#[test]
fn an_unknown_agy_tool_name_is_denied_as_unknown_and_shown_by_wait() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "unknown_tool", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let wt = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");

    let denied = cli_events(&f, &task_id, &run_id, "tool_denied");
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(denied[0]["tool"], "frobnicate_file", "{}", denied[0]);
    assert_eq!(denied[0]["known_to_thoth"], false, "{}", denied[0]);
    let reason = denied[0]["reason"].as_str().unwrap();
    assert!(reason.contains("is not a tool Thoth knows") && reason.contains("AGY-1"), "{reason}");
    assert_eq!(denied[0]["step_index"], 2, "{}", denied[0]);
    assert_eq!(denied[0]["conversation_id"], CONV, "{}", denied[0]);

    let usage = cli_events(&f, &task_id, &run_id, "run_usage");
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["tools_unknown"], serde_json::json!(["frobnicate_file"]), "{}", usage[0]);
    assert_eq!(usage[0]["tools_denied"], 1, "{}", usage[0]);
    assert_eq!(usage[0]["tool_names"], serde_json::json!(["run_command", "write_file"]), "{}", usage[0]);

    let (code, w) = wait_run_json(&f, &run_id, "30s");
    assert_eq!(code, 0, "{w}");
    assert_eq!(w["status"], "completed", "{w}");
    assert_eq!(w["tools_unknown"], serde_json::json!(["frobnicate_file"]), "{w}");
    assert_eq!(w["timed_out"], false, "{w}");
    let (code, human) = wait_run_human(&f, &run_id, "30s");
    assert_eq!(code, 0);
    let lines: Vec<&str> = human.lines().collect();
    assert_eq!(lines.len(), 2, "{human}");
    assert!(lines[0].starts_with(&format!("{run_id} completed")), "{human}");
    assert_eq!(lines[1], "agy called tools Thoth does not know: frobnicate_file; its tool names may have changed (AGY-1)");

    // the ordinary work still happened
    assert_eq!(git(&wt, &["show", "HEAD:agy_added.txt"]), "agy added");
    assert_eq!(status_run(&f, &task_id, &run_id)["status"], "completed");
}

/// AC2: a known name the lock denies (`invoke_subagent` off the default
/// list) is counted but never listed as unknown, and `wait --run` says so
/// with no warning line.
#[test]
fn a_known_but_denied_agy_tool_is_not_an_unknown_one() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "subagent", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let denied = cli_events(&f, &task_id, &run_id, "tool_denied");
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(denied[0]["tool"], "invoke_subagent");
    assert_eq!(denied[0]["known_to_thoth"], true, "{}", denied[0]);
    assert!(denied[0]["reason"].as_str().unwrap().contains("is not on this run's allow-list"), "{}", denied[0]);
    let usage = cli_events(&f, &task_id, &run_id, "run_usage");
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["tools_denied"], 1, "{}", usage[0]);
    assert_eq!(usage[0]["tools_unknown"], serde_json::json!([]), "{}", usage[0]);
    let (_, w) = wait_run_json(&f, &run_id, "30s");
    assert_eq!(w["tools_unknown"], serde_json::json!([]), "{w}");
    let (_, human) = wait_run_human(&f, &run_id, "30s");
    assert_eq!(human.lines().count(), 1, "{human}");
    assert!(!human.contains(UNKNOWN_LINE), "{human}");
}

/// AC2: three attempts, two distinct names: three events and a count of
/// three, while `tools_unknown` is the distinct names, sorted.
#[test]
fn repeated_unknown_names_count_each_time_but_are_listed_once_sorted() {
    let (f, cfg, task_id) = agy_setup();
    let out = agy_env(&f, &cfg, &task_id, "ok")
        .env("FAKE_AGY_EXTRA_TOOLS", "z_unknown,a_unknown,z_unknown")
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", MODEL, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d: Value = serde_json::from_slice(&out).unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let denied = cli_events(&f, &task_id, &run_id, "tool_denied");
    let names: Vec<&str> = denied.iter().map(|d| d["tool"].as_str().unwrap()).collect();
    assert_eq!(names, ["z_unknown", "a_unknown", "z_unknown"], "{denied:?}");
    assert!(denied.iter().all(|d| d["known_to_thoth"] == false), "{denied:?}");
    let usage = cli_events(&f, &task_id, &run_id, "run_usage");
    assert_eq!(usage.len(), 1);
    assert_eq!(usage[0]["tools_denied"], 3, "{}", usage[0]);
    assert_eq!(usage[0]["tools_unknown"], serde_json::json!(["a_unknown", "z_unknown"]), "{}", usage[0]);
    let (_, w) = wait_run_json(&f, &run_id, "30s");
    assert_eq!(w["tools_unknown"], serde_json::json!(["a_unknown", "z_unknown"]), "{w}");
    let (_, human) = wait_run_human(&f, &run_id, "30s");
    assert!(human.contains("agy called tools Thoth does not know: a_unknown, z_unknown;"), "{human}");
}

/// AC2: a run with no result line gets no `run_usage` (as before), so its
/// `tools_unknown` is `[]`; the denial is still a `tool_denied` event. A
/// Claude run and a slow agy run that has not finished show `[]` too.
#[test]
fn wait_shows_no_unknown_tools_without_a_usage_or_for_other_harnesses() {
    let (f, cfg, task_id) = agy_setup();
    let out = agy_env(&f, &cfg, &task_id, "crash")
        .env("FAKE_AGY_EXTRA_TOOLS", "frobnicate_file")
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", MODEL, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d: Value = serde_json::from_slice(&out).unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");
    let denied = cli_events(&f, &task_id, &run_id, "tool_denied");
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert_eq!(denied[0]["known_to_thoth"], false, "{}", denied[0]);
    assert!(cli_events(&f, &task_id, &run_id, "run_usage").is_empty());
    let (code, w) = wait_run_json(&f, &run_id, "30s");
    assert_eq!((code, w["status"].as_str()), (0, Some("interrupted")), "{w}");
    assert_eq!(w["tools_unknown"], serde_json::json!([]), "{w}");
    let (_, human) = wait_run_human(&f, &run_id, "30s");
    assert!(!human.contains(UNKNOWN_LINE), "{human}");

    // a Claude run: its run_usage has no such key
    f.add_profile("claude-b");
    let (claude_task, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    let c = f.dispatch(&claude_task, "claude-b", "hooks", &[]);
    let claude_run = c["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&claude_task, "180s").1, "in_review");
    let usage = cli_events(&f, &claude_task, &claude_run, "run_usage");
    assert!(!usage.is_empty() && usage.iter().all(|u| u.get("tools_unknown").is_none()), "{usage:?}");
    let (_, w) = wait_run_json(&f, &claude_run, "30s");
    assert_eq!(w["tools_unknown"], serde_json::json!([]), "{w}");
    assert!(!wait_run_human(&f, &claude_run, "30s").1.contains(UNKNOWN_LINE));

    // a slow agy run: init seen, no result yet; the timeout branch carries the key
    let (slow_task, _) = f.create_task(common::Fixture::SPEC);
    let s = agy_dispatch(&f, &cfg, &slow_task, "slow", MODEL);
    let slow_run = s["run_id"].as_str().unwrap().to_string();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while cli_events(&f, &slow_task, &slow_run, "harness_init").is_empty() {
        assert!(std::time::Instant::now() < deadline, "never started");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let (code, w) = wait_run_json(&f, &slow_run, "1s");
    assert_eq!(code, 3, "{w}");
    assert_eq!(w["timed_out"], true, "{w}");
    assert_eq!(w["tools_unknown"], serde_json::json!([]), "{w}");
    let (code, human) = wait_run_human(&f, &slow_run, "1s");
    assert_eq!(code, 3);
    assert!(human.starts_with("timed out waiting for run") && !human.contains(UNKNOWN_LINE), "{human}");
    f.cmd().args(["cancel", &slow_run, "--json"]).assert().success();
    assert_eq!(f.wait_for(&slow_task, "60s").1, "cancelled");
}

/// AC2: only the run's own newest usage counts: a resume without the
/// unknown name replaces the interrupted launch's list, and another run of
/// the same task never mixes in.
#[test]
fn tools_unknown_follows_the_runs_newest_usage_and_never_another_run() {
    let (f, cfg, task_id) = agy_setup();
    let out = agy_env(&f, &cfg, &task_id, "quota")
        .env("FAKE_AGY_EXTRA_TOOLS", "x_unknown")
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", MODEL, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d: Value = serde_json::from_slice(&out).unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");
    let (_, w) = wait_run_json(&f, &run_id, "30s");
    assert_eq!(w["status"], "interrupted", "{w}");
    assert_eq!(w["tools_unknown"], serde_json::json!(["x_unknown"]), "{w}");
    assert_eq!(cli_events(&f, &task_id, &run_id, "run_usage").len(), 1);

    // another run of the same task (a reroute in the same attempt): its own [] only
    f.cmd().args(["profile", "check", "agy-a"]).assert().success();
    let other = agy_dispatch(&f, &cfg, &task_id, "ok", MODEL);
    let other_run = other["run_id"].as_str().unwrap().to_string();
    assert_ne!(other_run, run_id);
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let (_, w) = wait_run_json(&f, &other_run, "30s");
    assert_eq!(w["tools_unknown"], serde_json::json!([]), "{w}");
    assert!(!wait_run_human(&f, &other_run, "30s").1.contains(UNKNOWN_LINE));
    let (_, w) = wait_run_json(&f, &run_id, "30s");
    assert_eq!(w["tools_unknown"], serde_json::json!(["x_unknown"]), "still the first run's own: {w}");

    // a fresh task: interrupted with the unknown name, then resumed without it
    let (task2, _) = f.create_task(common::Fixture::SPEC);
    let out = agy_env(&f, &cfg, &task2, "quota")
        .env("FAKE_AGY_EXTRA_TOOLS", "x_unknown")
        .args(["dispatch", &task2, "--profile", "agy-a", "--model", MODEL, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d: Value = serde_json::from_slice(&out).unwrap();
    let run2 = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task2, "120s").1, "interrupted");
    assert_eq!(wait_run_json(&f, &run2, "30s").1["tools_unknown"], serde_json::json!(["x_unknown"]));
    f.cmd().args(["profile", "check", "agy-a"]).assert().success();
    agy_env(&f, &cfg, &task2, "resume").args(["resume", &run2, "--json"]).assert().success();
    assert_eq!(f.wait_for(&task2, "120s").1, "in_review");
    let usage = cli_events(&f, &task2, &run2, "run_usage");
    assert_eq!(usage.len(), 2, "{usage:?}");
    assert_eq!(usage[0]["tools_unknown"], serde_json::json!(["x_unknown"]), "{}", usage[0]);
    assert_eq!(usage[1]["tools_unknown"], serde_json::json!([]), "{}", usage[1]);
    let (_, w) = wait_run_json(&f, &run2, "30s");
    assert_eq!(w["status"], "completed", "{w}");
    assert_eq!(w["tools_unknown"], serde_json::json!([]), "{w}");
    assert!(!wait_run_human(&f, &run2, "30s").1.contains(UNKNOWN_LINE));
}

/// AC3: `harness_init` records the tool list agy reported, as sent, with
/// its length; a Claude and a codex run record null for both.
#[test]
fn harness_init_records_the_reported_tool_list_only_for_agy() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "ok", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let run_dir = std::path::PathBuf::from(d["run_dir"].as_str().unwrap());
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let init = cli_events(&f, &task_id, &run_id, "harness_init");
    assert_eq!(init.len(), 1, "{init:?}");
    let stream = std::fs::read_to_string(run_dir.join("stream.jsonl")).unwrap();
    let sent: Value = serde_json::from_str(stream.lines().next().unwrap()).unwrap();
    assert_eq!(sent["event"], "init");
    assert_eq!(init[0]["tools"], sent["init"]["tools"], "{}", init[0]);
    let tools = init[0]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 60);
    assert_eq!(init[0]["tool_count"], 60, "{}", init[0]);
    assert!(tools.contains(&Value::from("view_file")) && !tools.contains(&Value::from("read_file")),
        "the recorded list is agy's stale one, not what the model calls: {tools:?}");
    assert_eq!(init[0]["session_id"], CONV);
    assert_eq!(init[0]["model"], MODEL);

    f.add_profile("claude-b");
    let (claude_task, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    let c = f.dispatch(&claude_task, "claude-b", "hooks", &[]);
    let claude_run = c["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&claude_task, "180s").1, "in_review");
    let init = cli_events(&f, &claude_task, &claude_run, "harness_init");
    assert_eq!(init.len(), 1, "{init:?}");
    assert!(init[0]["tools"].is_null() && init[0]["tool_count"].is_null(), "{}", init[0]);
    assert!(init[0].get("tools").is_some() && init[0].get("tool_count").is_some(), "{}", init[0]);

    f.add_codex_profile("codex-a");
    let (codex_task, _) = f.create_task(common::Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &codex_task)
        .args(["dispatch", &codex_task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--json"]);
    let x: Value = serde_json::from_slice(&f.run_ok(c, "dispatch codex")).unwrap();
    let codex_run = x["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&codex_task, "120s").1, "in_review");
    let init = cli_events(&f, &codex_task, &codex_run, "harness_init");
    assert_eq!(init.len(), 1, "{init:?}");
    assert!(init[0]["tools"].is_null() && init[0]["tool_count"].is_null(), "{}", init[0]);
    assert!(init[0].get("tools").is_some() && init[0].get("tool_count").is_some(), "{}", init[0]);
}

/// AC2: `wait --run` reads `tools_unknown` only from a string array on the
/// run's newest `run_usage`. A codex run's usage has no such key; a usage
/// written by a Thoth before AGY-1 has none either, and one holding anything
/// but a string array reads as `[]`. Only an older runner can write those
/// two legacy shapes, so the test appends them to the run's event log the way
/// that runner did (setup only, nothing is read back from the database);
/// every check goes through `wait --run --json` and its human output.
#[test]
fn wait_reads_only_a_string_array_from_the_newest_usage_else_nothing() {
    let (f, cfg, task_id) = agy_setup();
    // a codex run: its run_usage carries no tools_unknown key at all
    f.add_codex_profile("codex-a");
    let (codex_task, _) = f.create_task(common::Fixture::SPEC);
    let mut c = f.cmd();
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", &codex_task)
        .args(["dispatch", &codex_task, "--profile", "codex-a", "--model", "gpt-5.6-luna", "--effort", "medium", "--json"]);
    let x: Value = serde_json::from_slice(&f.run_ok(c, "dispatch codex")).unwrap();
    let codex_run = x["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&codex_task, "120s").1, "in_review");
    let usage = cli_events(&f, &codex_task, &codex_run, "run_usage");
    assert!(!usage.is_empty() && usage.iter().all(|u| u.get("tools_unknown").is_none()), "{usage:?}");
    let (code, w) = wait_run_json(&f, &codex_run, "30s");
    assert_eq!((code, w["status"].as_str()), (0, Some("completed")), "{w}");
    assert_eq!(w["tools_unknown"], serde_json::json!([]), "{w}");
    let (_, human) = wait_run_human(&f, &codex_run, "30s");
    assert_eq!(human.lines().count(), 1, "{human}");
    assert!(!human.contains(UNKNOWN_LINE), "{human}");

    // an agy run that saw an unknown name, then usages in the shapes an
    // older Thoth wrote (or a damaged row), each one newer than the last
    let d = agy_dispatch(&f, &cfg, &task_id, "unknown_tool", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    assert_eq!(wait_run_json(&f, &run_id, "30s").1["tools_unknown"], serde_json::json!(["frobnicate_file"]));
    let append_usage = |detail: &Value| {
        f.state_conn()
            .execute(
                "INSERT INTO events (ts, task_id, run_id, type, detail) \
                 VALUES ('2026-10-22T00:00:00Z', ?1, ?2, 'run_usage', ?3)",
                [task_id.as_str(), run_id.as_str(), detail.to_string().as_str()],
            )
            .unwrap();
    };
    let legacy: [(&str, Value); 6] = [
        ("no key (written before AGY-1)", serde_json::json!({"tools_denied": 1, "tool_names": ["run_command"]})),
        ("null", serde_json::json!({"tools_unknown": null})),
        ("a string", serde_json::json!({"tools_unknown": "frobnicate_file"})),
        ("an object", serde_json::json!({"tools_unknown": {"a": 1}})),
        ("a mixed array", serde_json::json!({"tools_unknown": ["frobnicate_file", 3]})),
        ("an empty array", serde_json::json!({"tools_unknown": []})),
    ];
    for (what, detail) in &legacy {
        append_usage(detail);
        let (code, w) = wait_run_json(&f, &run_id, "30s");
        assert_eq!((code, w["status"].as_str()), (0, Some("completed")), "{what}: {w}");
        assert_eq!(w["tools_unknown"], serde_json::json!([]), "{what}: {w}");
        let (_, human) = wait_run_human(&f, &run_id, "30s");
        assert_eq!(human.lines().count(), 1, "{what}: {human}");
        assert!(!human.contains(UNKNOWN_LINE), "{what}: {human}");
    }
    // a newer string array is read again, and named in order on one line
    append_usage(&serde_json::json!({"tools_unknown": ["a_unknown", "z_unknown"]}));
    let (_, w) = wait_run_json(&f, &run_id, "30s");
    assert_eq!(w["tools_unknown"], serde_json::json!(["a_unknown", "z_unknown"]), "{w}");
    let (_, human) = wait_run_human(&f, &run_id, "30s");
    let lines: Vec<&str> = human.lines().collect();
    assert_eq!(lines.len(), 2, "{human}");
    assert_eq!(lines[1], "agy called tools Thoth does not know: a_unknown, z_unknown; its tool names may have changed (AGY-1)");
    // the codex run was never touched by any of this
    assert_eq!(wait_run_json(&f, &codex_run, "30s").1["tools_unknown"], serde_json::json!([]));
}

/// AC2: a resume that has not written its usage yet (its init seen, no
/// result) still shows the interrupted launch's `tools_unknown` on the
/// timeout branch of `wait --run`, and the run does again on the success
/// branch after that resume is cancelled without a result.
#[test]
fn wait_keeps_the_previous_launches_unknown_tools_while_a_resume_has_no_usage_yet() {
    let (f, cfg, task_id) = agy_setup();
    let out = agy_env(&f, &cfg, &task_id, "quota")
        .env("FAKE_AGY_EXTRA_TOOLS", "x_unknown")
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", MODEL, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d: Value = serde_json::from_slice(&out).unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");
    assert_eq!(wait_run_json(&f, &run_id, "30s").1["tools_unknown"], serde_json::json!(["x_unknown"]));
    assert_eq!(cli_events(&f, &task_id, &run_id, "run_usage").len(), 1);

    // resume with the slow flow: a second init, then no result for a minute
    f.cmd().args(["profile", "check", "agy-a"]).assert().success();
    agy_env(&f, &cfg, &task_id, "slow").args(["resume", &run_id, "--json"]).assert().success();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while cli_events(&f, &task_id, &run_id, "harness_init").len() < 2 {
        assert!(std::time::Instant::now() < deadline, "the resume never started");
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(cli_events(&f, &task_id, &run_id, "run_usage").len(), 1, "no new usage yet");
    let (code, w) = wait_run_json(&f, &run_id, "1s");
    assert_eq!(code, 3, "{w}");
    assert_eq!(w["timed_out"], true, "{w}");
    assert_eq!(w["status"], "running", "{w}");
    assert_eq!(w["tools_unknown"], serde_json::json!(["x_unknown"]), "{w}");
    let (code, human) = wait_run_human(&f, &run_id, "1s");
    assert_eq!(code, 3);
    let lines: Vec<&str> = human.lines().collect();
    assert_eq!(lines.len(), 2, "{human}");
    assert!(lines[0].starts_with(&format!("timed out waiting for run {run_id}; still running")), "{human}");
    assert_eq!(lines[1], "agy called tools Thoth does not know: x_unknown; its tool names may have changed (AGY-1)");

    // cancelled before any result: still no new usage, the list stays
    f.cmd().args(["cancel", &run_id, "--json"]).assert().success();
    assert_eq!(f.wait_for(&task_id, "60s").1, "cancelled");
    assert_eq!(cli_events(&f, &task_id, &run_id, "run_usage").len(), 1);
    let (code, w) = wait_run_json(&f, &run_id, "30s");
    assert_eq!((code, w["status"].as_str()), (0, Some("cancelled")), "{w}");
    assert_eq!(w["tools_unknown"], serde_json::json!(["x_unknown"]), "{w}");
    assert!(wait_run_human(&f, &run_id, "30s").1.contains(UNKNOWN_LINE));
}

/// AC3: through the runner, an `init.tools` of `[]` is recorded as `[]` with
/// `tool_count` 0, while `null`, a list with a non-string element and a
/// non-array are recorded as null for both keys (the parser's None); each
/// on its own run, with the stream showing what the fake really sent.
#[test]
fn harness_init_records_an_empty_list_as_empty_and_an_unusable_one_as_null() {
    let (f, cfg, _) = agy_setup();
    let cases: [(&str, Value, Value); 4] = [
        ("[]", serde_json::json!([]), serde_json::json!(0)),
        ("null", Value::Null, Value::Null),
        (r#"["view_file", 7]"#, Value::Null, Value::Null),
        (r#"{"a": 1}"#, Value::Null, Value::Null),
    ];
    for (sent, tools, count) in &cases {
        let (task_id, _) = f.create_task(common::Fixture::SPEC);
        let out = agy_env(&f, &cfg, &task_id, "ok")
            .env("FAKE_AGY_INIT_TOOLS", sent)
            .args(["dispatch", &task_id, "--profile", "agy-a", "--model", MODEL, "--json"])
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let d: Value = serde_json::from_slice(&out).unwrap();
        let run_id = d["run_id"].as_str().unwrap().to_string();
        let run_dir = std::path::PathBuf::from(d["run_dir"].as_str().unwrap());
        assert_eq!(f.wait_for(&task_id, "120s").1, "in_review", "{sent}");
        let stream = std::fs::read_to_string(run_dir.join("stream.jsonl")).unwrap();
        let first: Value = serde_json::from_str(stream.lines().next().unwrap()).unwrap();
        assert_eq!(first["event"], "init", "{sent}");
        assert_eq!(first["init"]["tools"], serde_json::from_str::<Value>(sent).unwrap(), "{sent}: {first}");
        let init = cli_events(&f, &task_id, &run_id, "harness_init");
        assert_eq!(init.len(), 1, "{sent}: {init:?}");
        assert!(init[0].get("tools").is_some() && init[0].get("tool_count").is_some(), "{sent}: {}", init[0]);
        assert_eq!(init[0]["tools"], *tools, "{sent}: {}", init[0]);
        assert_eq!(init[0]["tool_count"], *count, "{sent}: {}", init[0]);
    }
}

/// AC5: an agy reviewer reads with `read_file` and nothing denies it; the
/// task settles pass.
#[test]
fn an_agy_reviewers_read_file_calls_are_allowed_and_it_settles_pass() {
    let (f, task_id, _) = claude_candidate();
    let r = agy_review(&f, &task_id, "review_pass");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(settled_cli(&f, &task_id), "pass");
    let denied = cli_events(&f, &task_id, &reviewer_run, "tool_denied");
    assert!(!denied.iter().any(|d| d["tool"] == "read_file"), "{denied:?}");
    let names: Vec<&str> = denied.iter().map(|d| d["tool"].as_str().unwrap()).collect();
    assert_eq!(names, ["write_file", "run_command"], "{denied:?}");
    assert!(denied.iter().all(|d| d["known_to_thoth"] == true), "{denied:?}");
    let usage = cli_events(&f, &task_id, &reviewer_run, "run_usage");
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["tool_names"], serde_json::json!(["read_file"]), "{}", usage[0]);
    assert_eq!(usage[0]["tools_unknown"], serde_json::json!([]), "{}", usage[0]);
    assert_eq!(r["allowed_tools"], AGY_REVIEWER_TOOLS.join(",").as_str(), "{r}");
    let (_, w) = wait_run_json(&f, &reviewer_run, "30s");
    assert_eq!(w["verdict"], "PASS", "{w}");
    assert_eq!(w["tools_unknown"], serde_json::json!([]), "{w}");
}

// Attempt 2: the held run_usage on non-happy paths (AC15) ------------------

/// Dispatches `crash` with `FAKE_AGY_RESULT_BEFORE_CRASH` (plus `env`) and
/// returns the run once the task has settled with `task_status`.
fn crash_after_result(env: &[(&str, &str)], extra: &[&str], task_status: &str) -> (common::Fixture, String) {
    let (f, cfg, task_id) = agy_setup();
    let mut c = agy_env(&f, &cfg, &task_id, "crash");
    c.env("FAKE_AGY_RESULT_BEFORE_CRASH", "1");
    for (k, v) in env {
        c.env(k, v);
    }
    let out = c
        .args(["dispatch", &task_id, "--profile", "agy-a", "--model", MODEL, "--json"])
        .args(extra)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let d: Value = serde_json::from_slice(&out).unwrap();
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "120s").1, task_status);
    (f, run_id)
}

fn assert_one_usage_with_tools_denied(f: &common::Fixture, run_id: &str) {
    let usage = events(f, run_id, "run_usage");
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0].1["tools_denied"], 0, "{usage:?}");
}

#[test]
fn an_agy_run_that_dies_after_its_result_line_still_gets_one_run_usage() {
    let (f, run_id) = crash_after_result(&[], &[], "failed");
    assert_eq!(run_row(&f, &run_id).0, "failed");
    assert_one_usage_with_tools_denied(&f, &run_id);
}

#[test]
fn an_agy_run_killed_on_timeout_after_its_result_line_still_gets_one_run_usage() {
    let (f, run_id) = crash_after_result(&[("FAKE_AGY_HANG_AFTER_RESULT", "1")], &["--timeout", "3s"], "failed");
    let (status, reason, _, _) = run_row(&f, &run_id);
    assert_eq!((status.as_str(), reason.as_deref()), ("failed", Some("timeout")));
    assert_one_usage_with_tools_denied(&f, &run_id);
}

// Attempt 2: upgrade safety on resume (AC16) -------------------------------

#[test]
fn resuming_a_pre_g2a_agy_run_maps_the_claude_default_and_excludes_agents() {
    let (f, cfg, task_id) = agy_setup();
    let d = agy_dispatch(&f, &cfg, &task_id, "quota", MODEL);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let wt = std::path::PathBuf::from(d["worktree"].as_str().unwrap());
    assert_eq!(f.wait_for(&task_id, "120s").1, "interrupted");
    assert_eq!(run_row(&f, &run_id).1.as_deref(), Some("quota"));

    // What a pre-G2a dispatch left behind: Claude's default names in the row,
    // no hook files, and an info/exclude that knows only `.agent/`.
    f.state_conn()
        .execute(
            "UPDATE runs SET allowed_tools = 'Bash,Read,Write,Edit,Glob,Grep' WHERE run_id = ?1",
            [&run_id],
        )
        .unwrap();
    std::fs::remove_dir_all(wt.join(".agents")).unwrap();
    std::fs::remove_file(wt.join(".agent").join("agy-hook.json")).unwrap();
    let common = git(&wt, &["rev-parse", "--path-format=absolute", "--git-common-dir"]);
    let exclude = std::path::Path::new(common.trim()).join("info").join("exclude");
    let kept: String = std::fs::read_to_string(&exclude)
        .unwrap()
        .lines()
        .filter(|l| *l != ".agents/")
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&exclude, kept).unwrap();

    f.cmd().args(["profile", "check", "agy-a"]).assert().success();
    agy_env(&f, &cfg, &task_id, "resume")
        .args(["resume", &run_id, "--json"])
        .assert()
        .success();
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let policy: Value = serde_json::from_str(
        &std::fs::read_to_string(wt.join(".agent").join("agy-hook.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(policy["allowed_tools"], serde_json::json!(AGY_IMPLEMENTER_TOOLS), "{policy}");
    assert!(events(&f, &run_id, "tool_denied").is_empty());
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
    let text = std::fs::read_to_string(&exclude).unwrap();
    assert_eq!(text.lines().filter(|l| *l == ".agents/").count(), 1, "{text}");
}

// G2b: agy as the independent reviewer --------------------------------------

/// `AGY_REVIEWER_TOOLS` (AGY-1: `read_file` beside `view_file`).
const AGY_REVIEWER_TOOLS: &[&str] = &[
    "read_file", "view_file", "list_dir", "find_by_name", "grep_search", "finish", "wait",
    "wait_5_seconds",
];

fn agy_data(f: &common::Fixture) -> std::path::PathBuf {
    f.home_path().join("profiles").join("agy-a")
}

/// A claude-code implementer's candidate at `in_review`: the fake-claude
/// `hooks` scenario on a two-criterion spec, plus an agy profile to review it.
/// Returns (fixture, task_id, implementer run_id).
fn claude_candidate() -> (common::Fixture, String, String) {
    let f = fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    f.add_agy_profile("agy-a");
    let (task_id, _) = f.create_task(common::Fixture::SPEC_TWO_AC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task_id, "180s").1, "in_review");
    (f, task_id, run_id)
}

/// `review start --profile agy-a` with a fake-agy reviewer scenario, then
/// `wait --run` on the reviewer. Returns the `review start --json` output.
fn agy_review(f: &common::Fixture, task_id: &str, scenario: &str) -> Value {
    let out = f
        .cmd()
        .env("FAKE_SCENARIO", scenario)
        .env("FAKE_AGY_DATA_DIR", agy_data(f))
        .env("FAKE_AGY_ARGS_LOG", agy_args_log(f))
        .args(["review", "start", task_id, "--profile", "agy-a", "--model", MODEL, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let r: Value = serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
    f.cmd()
        .args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s"])
        .assert()
        .success();
    r
}

fn settled(f: &common::Fixture, task_id: &str) -> String {
    f.wait_for_task_status(task_id, &["pass", "reject", "unverified"], 30)
}

fn count(f: &common::Fixture, sql: &str, param: &str) -> i64 {
    f.state_conn().query_row(sql, [param], |r| r.get(0)).unwrap()
}

/// The part of a review packet that does not move while a review runs.
fn stable_packet_part(packet: &str) -> String {
    let start = packet.find("## Spec").expect("a spec section");
    let end = packet.find("## Reviews recorded so far").expect("a reviews section");
    packet[start..end].to_string()
}

/// The `structured_output` of the result line in a run's stream.jsonl.
fn structured_output(run_dir: &std::path::Path) -> Value {
    let stream = std::fs::read_to_string(run_dir.join("stream.jsonl")).unwrap();
    stream
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "result")
        .expect("a result line")["result"]["structured_output"]
        .clone()
}

/// AC5, AC2, AC3. This is also `a_claude_code_implementer_is_reviewed_by_an_agy_reviewer`
/// (AC7's first half): a claude-code implementer's candidate, an agy reviewer, a PASS.
#[test]
fn an_agy_reviewer_reads_its_input_files_and_its_pass_settles_the_task_pass() {
    let (f, task_id, implementer_run) = claude_candidate();
    let r = agy_review(&f, &task_id, "review_pass");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    let wt = std::path::PathBuf::from(r["worktree"].as_str().unwrap());
    let run_dir = std::path::PathBuf::from(r["run_dir"].as_str().unwrap());
    assert_eq!(r["target_run_id"], serde_json::json!(implementer_run));
    assert_eq!(settled(&f, &task_id), "pass");

    // the launch, exactly
    let calls = stream_invocations(&f);
    assert_eq!(calls.len(), 1, "{calls:?}");
    let prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    let schema = run_dir.join("review-schema.json");
    assert_eq!(
        calls[0]["args"],
        serde_json::json!(["-p", prompt, "--output-format", "stream-json", "--model", MODEL,
            "--dangerously-skip-permissions", "--print-timeout", "2100s", "--json-schema", schema.to_str().unwrap()])
    );
    assert_eq!(
        std::fs::canonicalize(calls[0]["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&wt).unwrap()
    );

    // the prompt
    for needle in [
        ".agent/review-input/packet.md", ".agent/review-input/evidence/", "You have no shell",
        "cannot write files", "Do not run thoth", "Your final message is the review document",
        "do not try to write that file yourself", "Every id in \"criteria\" must be one of: AC1, AC2",
        "1. Read the candidate diff before anything else", "2. Judge every acceptance criterion exactly once",
        "3. Do not trust the implementer's summary or claims", "4. PASS only if every criterion is met",
    ] {
        assert!(prompt.contains(needle), "{needle} missing from {prompt}");
    }
    assert!(!prompt.contains("cat "), "{prompt}");
    assert!(!prompt.contains("sandbox"), "{prompt}");

    // the schema is the codex one
    let written: Value = serde_json::from_str(&std::fs::read_to_string(&schema).unwrap()).unwrap();
    let measured: Value = serde_json::from_str(include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/codex/review-output-schema.json"
    )))
    .unwrap();
    assert_eq!(written, measured);

    // the input files match `review packet` and the stored blobs
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
    assert_eq!(stable_packet_part(&packet), stable_packet_part(&String::from_utf8(printed).unwrap()));
    let rows = f.evidence_rows(&task_id);
    assert!(rows.len() >= 3, "{rows:#?}");
    let mut files: Vec<String> = std::fs::read_dir(input.join("evidence"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    let mut expected: Vec<String> = rows.iter().map(|r| format!("{}.txt", r["code"].as_str().unwrap())).collect();
    expected.sort();
    assert_eq!(files, expected, "one file per listed row, nothing else");
    for row in &rows {
        let code = row["code"].as_str().unwrap();
        assert!(packet.contains(&format!("- {code} | ")), "{code} missing from the packet");
        let stored = row["blob_path"]
            .as_str()
            .map(|p| String::from_utf8_lossy(&std::fs::read(p).unwrap()).into_owned())
            .unwrap_or_default();
        let file = std::fs::read_to_string(input.join("evidence").join(format!("{code}.txt"))).unwrap();
        assert_eq!(file, stored, "{code} must hold the whole stored blob");
    }

    // the verdict: structured_output, written by the runner, ingested as usual
    let review: Value =
        serde_json::from_str(&std::fs::read_to_string(wt.join(".agent").join("review.json")).unwrap()).unwrap();
    assert_eq!(review, structured_output(&run_dir));
    let (verdict, reviewer): (String, Option<String>) = f
        .state_conn()
        .query_row("SELECT verdict, reviewer_run_id FROM reviews", [], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap();
    assert_eq!(verdict, "PASS");
    assert_eq!(reviewer.as_deref(), Some(reviewer_run.as_str()));
    assert_eq!(run_row(&f, &reviewer_run).0, "completed");
    assert_eq!(count(&f, "SELECT COUNT(*) FROM evidence WHERE run_id = ?1", &reviewer_run), 0);
}

#[test]
fn an_agy_reviewer_reject_settles_reject() {
    let (f, task_id, _) = claude_candidate();
    let r = agy_review(&f, &task_id, "review_reject");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(settled(&f, &task_id), "reject");
    let verdict: String = f
        .state_conn()
        .query_row("SELECT verdict FROM reviews WHERE reviewer_run_id = ?1", [&reviewer_run], |r| r.get(0))
        .unwrap();
    assert_eq!(verdict, "REJECT");
    let event = last_event(&f, &reviewer_run, "verdict").expect("a verdict event");
    assert_eq!(event["verdict"], "REJECT", "{event}");
}

/// H7a: an agy reviewer's labels on its unmet criterion reach
/// `review show --json`, and the met one's `"none"`s are read as no label.
#[test]
fn an_agy_reviewers_fix_size_and_category_reach_review_show() {
    let (f, task_id, _) = claude_candidate();
    agy_review(&f, &task_id, "review_reject");
    assert_eq!(settled(&f, &task_id), "reject");
    let out = f
        .cmd()
        .args(["review", "show", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let criteria = v["findings"]["criteria"].as_array().unwrap();
    assert_eq!(criteria.len(), 2, "{v}");
    assert_eq!(criteria[0]["fix_size"], "S", "{v}");
    assert_eq!(criteria[0]["category"], "spec_mismatch", "{v}");
    let met = criteria[1].as_object().unwrap();
    assert!(!met.contains_key("fix_size") && !met.contains_key("category"), "{v}");
}

#[test]
fn a_malformed_agy_review_settles_unverified_with_review_invalid() {
    let (f, task_id, _) = claude_candidate();
    let r = agy_review(&f, &task_id, "review_bad");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(settled(&f, &task_id), "unverified");
    assert_eq!(count(&f, "SELECT COUNT(*) FROM reviews WHERE reviewer_run_id = ?1", &reviewer_run), 0);
    let invalid = last_event(&f, &reviewer_run, "review_invalid").expect("a review_invalid event");
    assert_eq!(invalid["stage"], "schema", "{invalid}");
    assert!(invalid["reason"].as_str().unwrap().contains("says PASS but marks criterion"), "{invalid}");
}

#[test]
fn an_agy_reviewer_without_structured_output_settles_unverified() {
    let (f, task_id, _) = claude_candidate();
    let r = agy_review(&f, &task_id, "review_no_output");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    let wt = std::path::PathBuf::from(r["worktree"].as_str().unwrap());
    assert_eq!(settled(&f, &task_id), "unverified");
    assert!(!wt.join(".agent").join("review.json").exists());
    let invalid = last_event(&f, &reviewer_run, "review_invalid").expect("a review_invalid event");
    assert_eq!(invalid["stage"], "read", "{invalid}");
    assert!(!runner_error_log(&r).contains("structured_output"), "no log line for a missing structured_output");
}

fn runner_error_log(r: &Value) -> String {
    let path = std::path::Path::new(r["run_dir"].as_str().unwrap()).join("runner-error.log");
    std::fs::read_to_string(path).unwrap_or_default()
}

/// AC13: a `structured_output` that is not an object is logged, not written.
#[test]
fn an_agy_reviewer_with_a_non_object_structured_output_settles_unverified_and_logs_it() {
    let (f, task_id, _) = claude_candidate();
    let r = agy_review(&f, &task_id, "review_not_object");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    let wt = std::path::PathBuf::from(r["worktree"].as_str().unwrap());
    assert_eq!(settled(&f, &task_id), "unverified");
    assert!(!wt.join(".agent").join("review.json").exists());
    assert!(last_event(&f, &reviewer_run, "review_invalid").is_some());
    let log = runner_error_log(&r);
    assert!(log.contains(&reviewer_run) && log.contains("structured_output") && log.contains("string"), "{log}");
}

/// AC6: the reviewer's tool lock holds - no write, no command.
#[test]
fn an_agy_reviewer_cannot_write_or_run_commands() {
    let (f, task_id, _) = claude_candidate();
    let r = agy_review(&f, &task_id, "review_pass");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    let wt = std::path::PathBuf::from(r["worktree"].as_str().unwrap());
    // AGY-1: read through `status --json` and `events --json` (the denied
    // write is now called write_file)
    assert_eq!(settled_cli(&f, &task_id), "pass");
    assert!(!wt.join("agy_reviewer_leak.txt").exists());
    assert!(!wt.join("agy_reviewer_leak2.txt").exists());
    assert_eq!(git(&wt, &["status", "--porcelain"]), "");
    let denied: Vec<Value> = cli_events(&f, &task_id, &reviewer_run, "tool_denied");
    let tools: Vec<&str> = denied.iter().map(|d| d["tool"].as_str().unwrap()).collect();
    assert_eq!(tools, ["write_file", "run_command"], "{denied:?}");
    let usage = cli_events(&f, &task_id, &reviewer_run, "run_usage");
    assert_eq!(usage.len(), 1, "{usage:?}");
    assert_eq!(usage[0]["tools_denied"], 2, "{usage:?}");
    let policy: Value =
        serde_json::from_str(&std::fs::read_to_string(wt.join(".agent").join("agy-hook.json")).unwrap()).unwrap();
    assert_eq!(policy["role"], "reviewer", "{policy}");
    assert_eq!(policy["allowed_tools"], serde_json::json!(AGY_REVIEWER_TOOLS), "{policy}");
    assert!(wt.join(".agents").join("hooks.json").is_file());
    assert_eq!(run_allowed_tools(&f, &task_id, &reviewer_run), AGY_REVIEWER_TOOLS.join(",").as_str());
}

/// AC7's second half: an agy implementer's candidate reviewed by a claude-code
/// reviewer, with no agy-specific code on the reviewer side.
#[test]
fn an_agy_implementer_is_reviewed_by_a_claude_code_reviewer() {
    let (f, cfg, task_id) = agy_setup();
    f.add_profile("claude-b");
    agy_dispatch(&f, &cfg, &task_id, "ok", MODEL);
    assert_eq!(f.wait_for(&task_id, "120s").1, "in_review");
    let r = f.review_start(&task_id, "claude-b", "review_pass", &["--model", "haiku"]);
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    let run_dir = std::path::PathBuf::from(r["run_dir"].as_str().unwrap());
    f.cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "180s"])
        .assert()
        .success();
    assert_eq!(settled(&f, &task_id), "pass");
    let verdict = last_event(&f, &reviewer_run, "verdict").expect("a verdict event");
    assert_eq!(verdict["verdict"], "PASS", "{verdict}");
    // A claude-code reviewer gets no packet.md (its path is unchanged); its
    // input is the prompt's index, which `review packet` also lists.
    let prompt = std::fs::read_to_string(run_dir.join("prompt.txt")).unwrap();
    let packet = f
        .cmd()
        .args(["review", "packet", &task_id])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let packet = String::from_utf8(packet).unwrap();
    let commands = command_rows(&f, &task_id);
    assert_eq!(commands.len(), 3, "{commands:#?}");
    for row in &commands {
        let code = row["code"].as_str().unwrap();
        assert!(packet.contains(&format!("- {code} | ")), "{code} missing from the packet");
        assert!(prompt.contains(code), "{code} missing from the reviewer's prompt");
    }
    assert_eq!(stream_invocations(&f).len(), 1, "only the implementer ran agy");
}

/// AC8: `wait --run`, `review show` and `resume` treat an agy reviewer run
/// the way they treat a codex one.
#[test]
fn agy_reviewer_wait_show_and_resume_behave_like_codexs() {
    let (f, task_id, _) = claude_candidate();
    let r = agy_review(&f, &task_id, "review_pass");
    let reviewer_run = r["run_id"].as_str().unwrap().to_string();
    assert_eq!(settled(&f, &task_id), "pass");

    let out = f
        .cmd()
        .args(["wait", "--run", &reviewer_run, "--timeout", "30s", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    assert_eq!(v["role"], "reviewer", "{v}");
    assert_eq!(v["verdict"], "PASS", "{v}");
    assert_eq!(v["task_status"], "pass", "{v}");

    let out = f
        .cmd()
        .args(["review", "show", &task_id, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let v: Value = serde_json::from_slice(&out).unwrap();
    let cited = v["cited"].as_array().unwrap();
    assert!(!cited.is_empty(), "{v}");
    assert_eq!(v["opened"], serde_json::json!([]), "{v}");
    assert_eq!(&v["unopened_citation"], &v["cited"], "{v}");

    f.cmd()
        .args(["resume", &reviewer_run])
        .assert()
        .failure()
        .stderr(predicate::str::contains("reviewer run"));
}

#[test]
fn antigravity_reviewer_refuses_a_claude_alias_and_a_codex_model_without_writing_a_run() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, run_id, _) = f.seed_run("claude-b", common::Fixture::SPEC_TWO_AC);
    let candidate = common::git(f.repo.path(), &["rev-parse", "HEAD"]);
    f.state_conn()
        .execute(
            "UPDATE runs SET status='completed', candidate_commit=?1 WHERE run_id=?2",
            rusqlite::params![candidate, run_id],
        )
        .unwrap();
    f.state_conn()
        .execute("UPDATE tasks SET status='in_review' WHERE task_id=?1", [&task_id])
        .unwrap();
    f.add_agy_profile("agy-a");
    // MODELS-1: the refusal lists the stored list, so store one first.
    f.cmd().args(["profile", "check", "agy-a"]).assert().success();
    for model in ["sonnet", "gpt-5.5"] {
        f.cmd()
            .args(["review", "start", &task_id, "--profile", "agy-a", "--model", model])
            .assert()
            .failure()
            .stderr(predicates::str::contains("gemini-3.8-flash-low"));
    }
    let reviewers: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM runs WHERE role = 'reviewer'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(reviewers, 0);
    let started: i64 = f
        .state_conn()
        .query_row("SELECT COUNT(*) FROM events WHERE type = 'review_started'", [], |row| row.get(0))
        .unwrap();
    assert_eq!(started, 0);
    assert_eq!(f.task_status(&task_id), "in_review");
}
