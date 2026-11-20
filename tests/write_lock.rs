//! A command that writes to the database while another writer holds the
//! lock waits for it (the 5 s busy timeout) instead of failing at once with
//! "database is locked". Next to a live run a collision is a matter of
//! timing: a runner commits a row for every stream line and heartbeat, and a
//! command whose transaction reads before it writes meets one of those
//! commits only now and then. The debug-only seam
//! `THOTH_TEST_WRITE_TX_PAUSE_FILE` makes one command hold the write lock for
//! as long as the test wants, so the collision happens every time, on every
//! platform.

mod common;

use common::Fixture;
use serde_json::Value;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// A `thoth` child started in the background, with the environment
/// `Fixture::cmd` sets for a foreground one.
fn spawn(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Child {
    let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
    for var in [
        "THOTH_RUN_ID",
        "THOTH_INBOX_DIR",
        "THOTH_EXE",
        "THOTH_VIA_MCP",
        "THOTH_RECORDS_DIR",
        "THOTH_MACHINE_NAME",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "ARTIFICIALANALYSIS_API_KEY",
        "AISTUPIDLEVEL_API_KEY",
        "THOTH_TEST_WRITE_TX_PAUSE_FILE",
    ] {
        c.env_remove(var);
    }
    c.env("THOTH_HOME", f.home_path())
        .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
        .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
        .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
        .env("THOTH_TEST_POLL_MS", "100")
        .current_dir(f.repo_path())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        c.env(k, v);
    }
    c.spawn().expect("spawn thoth")
}

fn json_of(what: &str, out: &Output) -> Value {
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{what} exited {:?}\nstdout: {stdout}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_str(stdout.trim()).unwrap_or_else(|e| panic!("{what}: stdout is not JSON ({e}): {stdout}"))
}

fn run_json(f: &Fixture, args: &[&str], envs: &[(&str, &str)]) -> Value {
    let mut c = f.cmd();
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.args(args).output().expect("run thoth");
    json_of(&args.join(" "), &out)
}

fn wait_for_file(path: &Path, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !path.exists() {
        assert!(Instant::now() < deadline, "{what}: {} never appeared", path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The check `id` as `checks list --json` reports it.
fn listed_check(f: &Fixture, id: &str) -> Value {
    let l = run_json(f, &["checks", "list", "--json"], &[]);
    l["checks"]
        .as_array()
        .unwrap_or_else(|| panic!("checks list --json has no checks array: {l}"))
        .iter()
        .find(|c| c["check_id"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("checks list --json has no check {id}: {l}"))
}

#[test]
fn checks_add_waits_for_a_writer_while_a_run_is_live() {
    let f = common::fixture();
    let mut init = f.cmd();
    init.arg("init");
    f.run_ok(init, "init");
    f.add_profile("claude-b");
    run_json(&f, &["checks", "add", "first", "--cmd", "git --version", "--json"], &[]);

    // A live run: a spec reviewer held at FAKE_REVIEW_GATE stays running.
    let gate = f.home_path().join("gate");
    let gate_s = gate.display().to_string();
    let (task, _) = f.create_task(Fixture::SPEC);
    let started = run_json(
        &f,
        &["spec", "review", "start", &task, "--profile", "claude-b", "--model", "haiku", "--json"],
        &[
            ("FAKE_SCENARIO", "spec_review_approve"),
            ("FAKE_TASK_ID", &task),
            ("FAKE_REVIEW_GATE", &gate_s),
        ],
    );
    let run_id = started["run_id"].as_str().expect("spec review start --json has a run_id").to_string();
    let run = f.run_of(&task, &run_id);
    assert!(matches!(run["status"].as_str(), Some("queued" | "running")), "{run}");

    // `checks approve` takes the write lock and holds it until `hold` is gone.
    let hold = f.home_path().join("hold");
    let hold_s = hold.display().to_string();
    let holder = spawn(
        &f,
        &["checks", "approve", "--reason", "holding the lock", "--json"],
        &[("THOTH_TEST_WRITE_TX_PAUSE_FILE", &hold_s)],
    );
    wait_for_file(&hold, "checks approve holding the write lock");

    let adder = spawn(&f, &["checks", "add", "second", "--cmd", "git --version", "--json"], &[]);
    // Well inside the 5 s busy timeout: the command is still waiting, not
    // refused.
    std::thread::sleep(Duration::from_millis(700));
    std::fs::remove_file(&hold).unwrap();
    let added = json_of("checks add second", &adder.wait_with_output().unwrap());
    let approved = json_of("checks approve", &holder.wait_with_output().unwrap());

    assert_eq!(added["ok"], true, "{added}");
    assert_eq!(added["check_id"], "second", "{added}");
    assert_eq!(added["status"], "pending", "{added}");
    assert_eq!(approved["ok"], true, "{approved}");
    // The approval committed first, so it covered only the check that
    // existed then; the check added while it held the lock waits for its own.
    assert_eq!(listed_check(&f, "first")["status"], "approved");
    assert_eq!(listed_check(&f, "second")["status"], "pending");

    std::fs::write(&gate, b"").unwrap();
    run_json(&f, &["wait", "--run", &run_id, "--timeout", "180s", "--json"], &[]);
}
