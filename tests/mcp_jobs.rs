mod common;

// S4-3: the MCP server's background `verify` job (thoth_verify_start /
// thoth_job_status) and the three check-catalogue tools. Approval is always
// done here by calling the CLI directly, as the human would: the server has
// no tool for it.

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    /// `None` once a test has closed the server's stdin.
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
    next_id: u64,
}

impl Server {
    fn start(f: &common::Fixture) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp")
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("FAKE_SCENARIO", "ok")
            .env_remove("FAKE_TASK_ID")
            .env_remove("THOTH_RUN_ID")
            .env_remove("THOTH_INBOX_DIR")
            .env_remove("THOTH_EXE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            input,
            output,
            next_id: 1,
        }
    }

    /// One tools/call; returns the `result` member (panics on a protocol error).
    fn call(&mut self, name: &str, arguments: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}});
        let input = self.input.as_mut().unwrap();
        writeln!(input, "{request}").unwrap();
        input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        let v: Value = serde_json::from_str(&s).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert!(v.get("error").is_none(), "{v}");
        v["result"].clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// `checks approve` (the human's step), straight through the CLI.
fn approve(f: &common::Fixture) {
    f.cmd().args(["checks", "approve", "--reason", "test approval", "--json"]).assert().success();
}

/// The script a slow check runs, and the check's shell and cmd.
#[cfg(windows)]
const SLOW: (&str, &str, &str, &str) =
    ("slow.ps1", "Start-Sleep -Seconds 5\r\n", "powershell", r".\slow.ps1");
#[cfg(not(windows))]
const SLOW: (&str, &str, &str, &str) = ("slow.sh", "#!/bin/sh\nsleep 5\n", "sh", "./slow.sh");

/// Commits the 5-second script to the fixture repo, the way tests/decision.rs
/// commits its scripts: the main tree's copy is checked out again so git's
/// line ending conversion applies to it exactly as to a worktree.
fn commit_slow_script(f: &common::Fixture) {
    let repo = f.repo_path();
    let (name, body, _, _) = SLOW;
    std::fs::write(repo.join(name), body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(repo.join(name), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    common::git(&repo, &["add", name]);
    common::git(&repo, &["commit", "-m", "slow check script"]);
    std::fs::remove_file(repo.join(name)).unwrap();
    common::git(&repo, &["checkout", "HEAD", "--", name]);
}

/// init, one profile, a task, one `hooks` run that settled in review (the
/// shape of tests/decision.rs's `settled_run`). Returns (task_id, run_id).
fn settled_run(f: &common::Fixture) -> (String, String) {
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let (task_id, _base) = f.create_task(common::Fixture::SPEC);
    let d = f.dispatch(&task_id, "claude-b", "hooks", &[]);
    let run_id = d["run_id"].as_str().unwrap().to_string();
    let (code, status) = f.wait_for(&task_id, "180s");
    assert_eq!(code, 0);
    assert_eq!(status, "in_review");
    (task_id, run_id)
}

/// Polls thoth_job_status with wait_seconds until the job leaves `working`.
fn poll_until_done(s: &mut Server, job_id: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let r = s.call("thoth_job_status", json!({"job_id":job_id,"wait_seconds":5}));
        assert_eq!(r["isError"], false, "{r}");
        if r["structuredContent"]["status"] != "working" {
            return r;
        }
        assert!(Instant::now() < deadline, "job {job_id} never finished: {r}");
    }
}

/// The evidence rows whose label starts with `verify: `, through MCP.
fn verify_rows(s: &mut Server, task_id: &str) -> Vec<Value> {
    let listed = s.call("thoth_evidence_list", json!({"task":task_id}));
    assert_eq!(listed["isError"], false, "{listed}");
    listed["structuredContent"]["evidence"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["label"].as_str().unwrap().starts_with("verify: "))
        .cloned()
        .collect()
}

fn check_row(listed: &Value, id: &str) -> Option<Value> {
    listed["structuredContent"]["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["check_id"] == id)
        .cloned()
}

fn job_dirs(f: &common::Fixture) -> usize {
    match std::fs::read_dir(f.home.path().join("mcp").join("jobs")) {
        Ok(entries) => entries.count(),
        Err(_) => 0,
    }
}

/// AC9 (a): add → pending → (human approves) → approved → remove → gone.
#[test]
fn checks_catalogue_add_list_approve_by_cli_and_remove() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let mut s = Server::start(&f);

    let added = s.call(
        "thoth_checks_add",
        json!({"id":"git-version","cmd":"git --version","shell":"argv","cwd":"."}),
    );
    assert_eq!(added["isError"], false, "{added}");

    let listed = s.call("thoth_checks_list", json!({}));
    assert_eq!(listed["isError"], false, "{listed}");
    let row = check_row(&listed, "git-version").expect("the added check is listed");
    assert_eq!(row["status"], "pending");

    approve(&f);

    let listed = s.call("thoth_checks_list", json!({}));
    let row = check_row(&listed, "git-version").expect("the check is still listed");
    assert_eq!(row["status"], "approved");

    let removed = s.call("thoth_checks_remove", json!({"id":"git-version"}));
    assert_eq!(removed["isError"], false, "{removed}");

    let listed = s.call("thoth_checks_list", json!({}));
    assert_eq!(listed["isError"], false, "{listed}");
    assert!(check_row(&listed, "git-version").is_none(), "{listed}");
}

/// AC9 (b) and (e): a 5-second approved check runs as a background job, the
/// start call returns at once, the result arrives through thoth_job_status,
/// the evidence is control-plane evidence, and a second server reports the
/// same result as recovered.
#[test]
fn background_verify_returns_at_once_completes_and_survives_a_server_restart() {
    let f = common::fixture();
    commit_slow_script(&f);
    let (task_id, run_id) = settled_run(&f);
    let (_, _, shell, cmd) = SLOW;
    let mut s = Server::start(&f);
    let added = s.call("thoth_checks_add", json!({"id":"slow","cmd":cmd,"shell":shell}));
    assert_eq!(added["isError"], false, "{added}");
    approve(&f);

    assert_eq!(verify_rows(&mut s, &task_id).len(), 0);

    let before = Instant::now();
    let started = s.call("thoth_verify_start", json!({"run":run_id}));
    let elapsed = before.elapsed();
    assert_eq!(started["isError"], false, "{started}");
    let doc = &started["structuredContent"];
    assert_eq!(doc["status"], "working");
    assert!(elapsed < Duration::from_secs(3), "start took {elapsed:?}");
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["kind"], "verify");
    assert_eq!(doc["run"], run_id.as_str());
    assert_eq!(doc["poll_tool"], "thoth_job_status");
    assert_eq!(doc["suggested_poll_seconds"], 15);
    let job_id = doc["job_id"].as_str().unwrap().to_owned();

    let now = s.call("thoth_job_status", json!({"job_id":job_id,"wait_seconds":0}));
    assert_eq!(now["isError"], false, "{now}");
    assert_eq!(now["structuredContent"]["status"], "working");
    assert_eq!(now["structuredContent"]["exit_code"], Value::Null);
    assert_eq!(now["structuredContent"]["result"], Value::Null);
    assert_eq!(now["structuredContent"]["finished_at"], Value::Null);
    assert_eq!(now["structuredContent"]["recovered"], false);

    let done = poll_until_done(&mut s, &job_id);
    let doc = &done["structuredContent"];
    assert_eq!(doc["status"], "completed", "{done}");
    assert_eq!(doc["exit_code"], 0, "{done}");
    let slow = doc["result"]["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["check_id"] == "slow")
        .expect("the slow check is in the results");
    assert_eq!(slow["exit_code"], 0, "{done}");
    assert_eq!(doc["recovered"], false);
    assert_eq!(doc["kind"], "verify");
    assert!(doc["started_at"].is_string() && doc["finished_at"].is_string(), "{done}");

    let rows = verify_rows(&mut s, &task_id);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0]["collected_by"], "control_plane");

    // the job's record on disk
    let dir = f.home.path().join("mcp").join("jobs").join(&job_id);
    let meta: Value = serde_json::from_slice(&std::fs::read(dir.join("meta.json")).unwrap()).unwrap();
    assert_eq!(meta["job_id"], job_id.as_str());
    assert_eq!(meta["kind"], "verify");
    assert_eq!(meta["run"], run_id.as_str());
    assert_eq!(meta["argv"], json!(["verify", run_id, "--json"]));
    assert!(meta["pid"].is_u64() && meta["started_at"].is_string() && meta["repo"].is_string());
    let exit: Value = serde_json::from_slice(&std::fs::read(dir.join("exit.json")).unwrap()).unwrap();
    assert_eq!(exit["exit_code"], 0);

    // (e) a second server on the same THOTH_HOME
    let first_result = doc["result"].clone();
    let mut second = Server::start(&f);
    let again = second.call("thoth_job_status", json!({"job_id":job_id}));
    assert_eq!(again["isError"], false, "{again}");
    assert_eq!(again["structuredContent"]["status"], "completed");
    assert_eq!(again["structuredContent"]["recovered"], true);
    assert_eq!(again["structuredContent"]["result"], first_result);
}

/// AC9 (c): nothing was ever added to the catalogue.
#[test]
fn verify_job_with_an_empty_catalogue_completes_with_no_approved_checks() {
    let f = common::fixture();
    let (_task_id, run_id) = settled_run(&f);
    let mut s = Server::start(&f);
    let started = s.call("thoth_verify_start", json!({"run":run_id}));
    assert_eq!(started["isError"], false, "{started}");
    let job_id = started["structuredContent"]["job_id"].as_str().unwrap().to_owned();
    let done = poll_until_done(&mut s, &job_id);
    assert_eq!(done["structuredContent"]["status"], "completed", "{done}");
    assert_eq!(done["structuredContent"]["exit_code"], 1);
    assert_eq!(done["structuredContent"]["result"]["reason"], "no_approved_checks");
    assert_eq!(done["isError"], false);
}

/// AC9 (d): an approved check that always fails.
#[test]
fn verify_job_with_a_failing_check_completes_with_exit_code_1() {
    let f = common::fixture();
    let (_task_id, run_id) = settled_run(&f);
    let mut s = Server::start(&f);
    let added = s.call(
        "thoth_checks_add",
        json!({"id":"fails","cmd":"git no-such-subcommand","shell":"argv"}),
    );
    assert_eq!(added["isError"], false, "{added}");
    approve(&f);
    let started = s.call("thoth_verify_start", json!({"run":run_id}));
    assert_eq!(started["isError"], false, "{started}");
    let job_id = started["structuredContent"]["job_id"].as_str().unwrap().to_owned();
    let done = poll_until_done(&mut s, &job_id);
    assert_eq!(done["structuredContent"]["status"], "completed", "{done}");
    assert_eq!(done["structuredContent"]["exit_code"], 1);
    assert_eq!(done["structuredContent"]["result"]["ok"], false);
    assert_eq!(done["isError"], false);
}

/// AC2 (b): verify fails outright (the run does not exist), so stdout holds
/// no document: the job is `failed`, which is still not a tool error.
#[test]
fn verify_job_for_a_run_that_does_not_exist_is_failed() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let mut s = Server::start(&f);
    let started = s.call("thoth_verify_start", json!({"run":"01DOESNOTEXIST"}));
    assert_eq!(started["isError"], false, "{started}");
    let job_id = started["structuredContent"]["job_id"].as_str().unwrap().to_owned();
    let done = poll_until_done(&mut s, &job_id);
    assert_eq!(done["isError"], false);
    assert_eq!(done["structuredContent"]["status"], "failed", "{done}");
    assert_eq!(done["structuredContent"]["exit_code"], 1);
    assert_eq!(done["structuredContent"]["result"], Value::Null);
    assert_eq!(done["structuredContent"]["recovered"], false);
    assert!(!done["structuredContent"]["stderr_tail"].as_str().unwrap().is_empty(), "{done}");
}

/// AC9 (f): a job directory that only has meta.json, as left by a server that
/// went away before its job was seen to finish.
#[test]
fn job_with_only_meta_json_is_unknown_with_a_hint() {
    let f = common::fixture();
    let job_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let dir = f.home.path().join("mcp").join("jobs").join(job_id);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = json!({"job_id":job_id,"kind":"verify","argv":["verify","R","--json"],"repo":".","run":"R","started_at":"2026-10-01T00:00:00Z","pid":1});
    std::fs::write(dir.join("meta.json"), meta.to_string()).unwrap();
    let mut s = Server::start(&f);
    let r = s.call("thoth_job_status", json!({"job_id":job_id}));
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["structuredContent"]["status"], "unknown");
    assert!(r["structuredContent"].as_object().unwrap().contains_key("hint"), "{r}");
    let hint = r["structuredContent"]["hint"].as_str().unwrap();
    assert!(hint.contains("may still be running"), "{hint}");
    assert!(hint.contains("thoth_evidence_list"), "{hint}");
    assert_eq!(r["structuredContent"]["recovered"], true);
    assert_eq!(r["structuredContent"]["started_at"], "2026-10-01T00:00:00Z");
}

/// AC9 (g) / AC3: the three job ids that are tool errors.
#[test]
fn job_status_refuses_a_missing_or_malformed_job_id() {
    let f = common::fixture();
    let mut s = Server::start(&f);
    let missing = s.call("thoth_job_status", json!({"job_id":"01ARZ3NDEKTSV4RRFFQ69G5FAW"}));
    assert_eq!(missing["isError"], true, "{missing}");
    let traversal = s.call("thoth_job_status", json!({"job_id":"../x"}));
    assert_eq!(traversal["isError"], true, "{traversal}");
    let lower = s.call("thoth_job_status", json!({"job_id":"01arz3ndektsv4rrffq69g5fav"}));
    assert_eq!(lower["isError"], true, "{lower}");
}

/// AC9 (h) / AC1: a bad parameter is refused before anything is created.
#[test]
fn verify_start_validates_its_parameters_before_creating_a_job() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let mut s = Server::start(&f);
    let before = job_dirs(&f);
    let bad_timeout = s.call("thoth_verify_start", json!({"run":"01RUN","timeout":"5x"}));
    assert_eq!(bad_timeout["isError"], true, "{bad_timeout}");
    assert_eq!(job_dirs(&f), before);
    let bad_bool = s.call("thoth_verify_start", json!({"run":"01RUN","at_base":"yes"}));
    assert_eq!(bad_bool["isError"], true, "{bad_bool}");
    let bad_dry = s.call("thoth_verify_start", json!({"run":"01RUN","dry_run":1}));
    assert_eq!(bad_dry["isError"], true, "{bad_dry}");
    let dash = s.call("thoth_verify_start", json!({"run":"--help"}));
    assert_eq!(dash["isError"], true, "{dash}");
    assert_eq!(job_dirs(&f), before);
}

/// AC12 (b): wait_seconds is range-checked against a job that really exists.
#[test]
fn job_status_refuses_a_wait_seconds_outside_the_server_maximum() {
    let f = common::fixture();
    let job_id = "01ARZ3NDEKTSV4RRFFQ69G5FAV";
    let dir = f.home.path().join("mcp").join("jobs").join(job_id);
    std::fs::create_dir_all(&dir).unwrap();
    let meta = json!({"job_id":job_id,"kind":"verify","argv":["verify","R","--json"],"repo":".","run":"R","started_at":"2026-10-01T00:00:00Z","pid":1});
    std::fs::write(dir.join("meta.json"), meta.to_string()).unwrap();
    let mut s = Server::start(&f);
    let max_wait: i64 = 30;
    for bad in [max_wait + 1, -1] {
        let r = s.call("thoth_job_status", json!({"job_id":job_id,"wait_seconds":bad}));
        assert_eq!(r["isError"], true, "{r}");
        assert!(r.to_string().contains("wait_seconds"), "{r}");
    }
    let ok = s.call("thoth_job_status", json!({"job_id":job_id,"wait_seconds":0}));
    assert_eq!(ok["isError"], false, "{ok}");
}

/// AC12 (i): at_base becomes `--at-base` in the job's argv.
#[test]
fn verify_start_passes_at_base_to_the_verify_child() {
    let f = common::fixture();
    let (_task_id, run_id) = settled_run(&f);
    let mut s = Server::start(&f);
    let started = s.call("thoth_verify_start", json!({"run":run_id,"at_base":true,"dry_run":true}));
    assert_eq!(started["isError"], false, "{started}");
    let job_id = started["structuredContent"]["job_id"].as_str().unwrap().to_owned();
    let meta: Value = serde_json::from_slice(
        &std::fs::read(f.home.path().join("mcp").join("jobs").join(&job_id).join("meta.json")).unwrap(),
    )
    .unwrap();
    assert!(meta["argv"].as_array().unwrap().contains(&json!("--at-base")), "{meta}");
}

/// AC12 (i): cwd and shell reach the catalogue.
#[test]
fn checks_add_passes_cwd_and_shell() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    std::fs::create_dir(f.repo.path().join("sub")).unwrap();
    let shell = if cfg!(windows) { "cmd" } else { "sh" };
    let mut s = Server::start(&f);
    let added = s.call(
        "thoth_checks_add",
        json!({"id":"in-sub","cmd":"git --version","shell":shell,"cwd":"sub"}),
    );
    assert_eq!(added["isError"], false, "{added}");
    let listed = s.call("thoth_checks_list", json!({}));
    let row = check_row(&listed, "in-sub").expect("the added check is listed");
    assert_eq!(row["cwd"], "sub");
    assert_eq!(row["shell"], shell);
}

/// AC9 (i) / AC5: `inputs` is a string array, one `--input` each.
#[test]
fn checks_add_passes_each_input_and_refuses_a_non_array() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let mut s = Server::start(&f);
    let added = s.call(
        "thoth_checks_add",
        json!({"id":"with-inputs","cmd":"git --version","inputs":["a.txt","b.txt"]}),
    );
    assert_eq!(added["isError"], false, "{added}");
    let listed = s.call("thoth_checks_list", json!({}));
    let row = check_row(&listed, "with-inputs").expect("the added check is listed");
    assert_eq!(row["inputs"], json!(["a.txt", "b.txt"]));

    let as_string = s.call(
        "thoth_checks_add",
        json!({"id":"bad-inputs","cmd":"git --version","inputs":"a.txt"}),
    );
    assert_eq!(as_string["isError"], true, "{as_string}");
    let with_number = s.call(
        "thoth_checks_add",
        json!({"id":"bad-inputs","cmd":"git --version","inputs":["a.txt",1]}),
    );
    assert_eq!(with_number["isError"], true, "{with_number}");
    let listed = s.call("thoth_checks_list", json!({}));
    assert!(check_row(&listed, "bad-inputs").is_none(), "{listed}");
}

/// AC1: at_base, dry_run and timeout become flags of the verify child; false
/// adds nothing. A dry run records no evidence.
#[test]
fn verify_start_passes_dry_run_and_timeout_to_the_verify_child() {
    let f = common::fixture();
    let (task_id, run_id) = settled_run(&f);
    let mut s = Server::start(&f);
    let added = s.call("thoth_checks_add", json!({"id":"git-version","cmd":"git --version"}));
    assert_eq!(added["isError"], false, "{added}");
    approve(&f);
    let started = s.call(
        "thoth_verify_start",
        json!({"run":run_id,"dry_run":true,"at_base":false,"timeout":"90s"}),
    );
    assert_eq!(started["isError"], false, "{started}");
    let job_id = started["structuredContent"]["job_id"].as_str().unwrap().to_owned();
    let done = poll_until_done(&mut s, &job_id);
    assert_eq!(done["structuredContent"]["status"], "completed", "{done}");
    assert_eq!(done["structuredContent"]["exit_code"], 0);
    assert_eq!(done["structuredContent"]["result"]["dry_run"], true);
    let meta: Value = serde_json::from_slice(
        &std::fs::read(f.home.path().join("mcp").join("jobs").join(&job_id).join("meta.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(meta["argv"], json!(["verify", run_id, "--json", "--dry-run", "--timeout=90s"]));
    assert_eq!(verify_rows(&mut s, &task_id).len(), 0);
}

/// AC4: at stdin EOF the server exits without killing or waiting for a job
/// that is still running; the job finishes on its own and leaves its evidence.
#[test]
fn a_running_job_outlives_the_server_that_started_it() {
    let f = common::fixture();
    commit_slow_script(&f);
    let (task_id, run_id) = settled_run(&f);
    let (_, _, shell, cmd) = SLOW;
    let mut s = Server::start(&f);
    let added = s.call("thoth_checks_add", json!({"id":"slow","cmd":cmd,"shell":shell}));
    assert_eq!(added["isError"], false, "{added}");
    approve(&f);
    let started = s.call("thoth_verify_start", json!({"run":run_id}));
    assert_eq!(started["structuredContent"]["status"], "working", "{started}");
    let job_id = started["structuredContent"]["job_id"].as_str().unwrap().to_owned();

    // EOF on stdin: the server must be gone well before the 5-second check ends
    let closed = Instant::now();
    drop(s.input.take());
    // The client's stdout pipe must reach EOF at once: the job child must not
    // have inherited the server's std handles (Windows hands them down).
    let mut rest = Vec::new();
    std::io::Read::read_to_end(&mut s.output, &mut rest).unwrap();
    assert!(closed.elapsed() < Duration::from_secs(3), "stdout EOF took {:?}", closed.elapsed());
    let status = s.child.wait().unwrap();
    assert!(status.success());
    assert!(closed.elapsed() < Duration::from_secs(3), "server took {:?} to exit", closed.elapsed());
    drop(s);

    // the job was not killed: a second server sees it finish
    let mut second = Server::start(&f);
    let deadline = Instant::now() + Duration::from_secs(300);
    let done = loop {
        let r = second.call("thoth_job_status", json!({"job_id":job_id}));
        assert_eq!(r["isError"], false, "{r}");
        if r["structuredContent"]["status"] == "completed" {
            break r;
        }
        assert_eq!(r["structuredContent"]["status"], "unknown", "{r}");
        assert!(Instant::now() < deadline, "the job never finished: {r}");
        std::thread::sleep(Duration::from_millis(500));
    };
    let doc = &done["structuredContent"];
    assert_eq!(doc["recovered"], true);
    assert_eq!(doc["exit_code"], Value::Null);
    assert_eq!(doc["result"]["ok"], true, "{done}");
    let rows = verify_rows(&mut second, &task_id);
    assert_eq!(rows.len(), 1, "{rows:#?}");
    assert_eq!(rows[0]["collected_by"], "control_plane");
}
