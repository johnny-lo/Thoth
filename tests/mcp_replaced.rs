//! LESS-6 (H8 (a)): a running `thoth mcp` whose executable is replaced on
//! disk hands later calls to the new executable, and falls back to its own
//! tools with a warning when it cannot.
//!
//! Every test holds `LOCK` the whole time: on Linux a freshly written
//! executable can fail to start with ETXTBSY while another test thread forks.

mod common;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

static LOCK: Mutex<()> = Mutex::new(());

const THOTH: &str = env!("CARGO_BIN_EXE_thoth");
const FAKE: &str = env!("CARGO_BIN_EXE_fake-mcp");
const RESPONSE: Duration = Duration::from_secs(60);
const NOTE: Duration = Duration::from_secs(5);
const QUIET: Duration = Duration::from_secs(2);
const END: Duration = Duration::from_secs(10);
const NOTICE: &str = "notice: the thoth executable at ";
const WARNING: &str = "warning: the thoth executable this MCP server was started from";
const MAY_HAVE_RUN: &str = "may already have run this call";

fn lock() -> MutexGuard<'static, ()> {
    LOCK.lock().unwrap_or_else(PoisonError::into_inner)
}

// ---- copied from tests/mcp_jobs.rs ----

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

// ---- copied from tests/mcp_plugin.rs ----

const REPO_PLUGIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/plugins/thoth");

fn repo_file(rel: &str) -> String {
    std::fs::read_to_string(Path::new(REPO_PLUGIN).join(rel)).unwrap()
}

fn fake_plugin(f: &common::Fixture, manifest: &str, skill: Option<&str>) -> PathBuf {
    let root = f.home.path().join("plugin-root");
    std::fs::create_dir_all(root.join(".claude-plugin")).unwrap();
    std::fs::write(root.join(".claude-plugin/plugin.json"), manifest).unwrap();
    if let Some(s) = skill {
        std::fs::create_dir_all(root.join("skills/thoth-dispatch")).unwrap();
        std::fs::write(root.join("skills/thoth-dispatch/SKILL.md"), s).unwrap();
    }
    root
}

fn stale_plugin(f: &common::Fixture) -> PathBuf {
    let skill = format!("{}\nextra line\n", repo_file("skills/thoth-dispatch/SKILL.md"));
    fake_plugin(f, &repo_file(".claude-plugin/plugin.json"), Some(&skill))
}

// ---- the executable at the server's path ----

/// The directory the server is started from, with the fake's log and mode
/// files next to the executable.
struct Bin {
    dir: tempfile::TempDir,
    exe: PathBuf,
    renamed: u32,
}

impl Bin {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join(if cfg!(windows) { "thoth.exe" } else { "thoth" });
        std::fs::copy(THOTH, &exe).unwrap();
        Self { dir, exe, renamed: 0 }
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("fake-mcp.log")
    }

    fn mode(&self, mode: &str) {
        std::fs::write(self.dir.path().join("fake-mcp.mode"), mode).unwrap();
    }

    /// Renames the file at the path to a name not used before.
    fn move_away(&mut self) {
        self.renamed += 1;
        let old = self.dir.path().join(format!("thoth.old{}", self.renamed));
        std::fs::rename(&self.exe, old).unwrap();
    }

    /// Puts a copy of `src` at the path, one byte longer with `extra`.
    fn put(&self, src: &str, extra: bool) {
        std::fs::copy(src, &self.exe).unwrap();
        if extra {
            let mut f = std::fs::OpenOptions::new().append(true).open(&self.exe).unwrap();
            f.write_all(&[0]).unwrap();
        }
    }

    fn replace(&mut self, src: &str, extra: bool) {
        self.move_away();
        self.put(src, extra);
    }

    /// Leaves the file at the path startable but not readable. False when
    /// this process can read it anyway (root ignores the mode bits).
    #[cfg(windows)]
    fn deny_reading(&self) -> bool {
        let status = Command::new("icacls")
            .arg(&self.exe)
            .args(["/deny", "*S-1-1-0:(RD)"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::read(&self.exe).is_err()
    }

    #[cfg(windows)]
    fn allow_reading(&self) {
        let status = Command::new("icacls")
            .arg(&self.exe)
            .args(["/remove:d", "*S-1-1-0"])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(unix)]
    fn deny_reading(&self) -> bool {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.exe, std::fs::Permissions::from_mode(0o111)).unwrap();
        std::fs::read(&self.exe).is_err()
    }

    #[cfg(unix)]
    fn allow_reading(&self) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&self.exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

// ---- the server ----

/// A running server. Dropping it kills and reaps it, also when a test fails.
struct Server {
    child: Child,
    input: Option<ChildStdin>,
    /// stdout, line by line without the `\n`; `None` at EOF.
    lines: Receiver<Option<Vec<u8>>>,
    stderr: Option<Receiver<String>>,
}

struct Reply {
    v: Value,
    raw: Vec<u8>,
    /// The lines without an id that came before it.
    notes: Vec<Vec<u8>>,
}

impl Reply {
    fn texts(&self) -> Vec<String> {
        self.v["result"]["content"]
            .as_array()
            .unwrap_or_else(|| panic!("no content: {}", self.v))
            .iter()
            .map(|c| c["text"].as_str().unwrap_or("").to_owned())
            .collect()
    }
    fn first(&self) -> String {
        self.texts().first().cloned().unwrap_or_default()
    }
    fn sc(&self) -> &Value {
        &self.v["result"]["structuredContent"]
    }
}

impl Server {
    /// Started, and up: it answered a ping, so it is watching the file at
    /// its path before a test replaces it.
    fn start(f: &common::Fixture, bin: &Bin, extra: &[&str]) -> Self {
        Self::start_with(f, bin, extra, false)
    }

    fn start_with(f: &common::Fixture, bin: &Bin, extra: &[&str], stderr: bool) -> Self {
        Self::start_env(f, bin, extra, stderr, &[])
    }

    /// As `start_with`, with extra environment variables for the server.
    fn start_env(
        f: &common::Fixture,
        bin: &Bin,
        extra: &[&str],
        stderr: bool,
        env: &[(&str, &str)],
    ) -> Self {
        let mut s = Self::start_bare(f, bin, extra, stderr, env);
        s.send(&json!({"jsonrpc":"2.0","id":0,"method":"ping"}));
        let r = s.response(&json!(0));
        assert_eq!(r.v["result"], json!({}), "{}", r.v);
        s
    }

    /// Started only; the test's own first request must be answered before
    /// it replaces the file.
    fn start_bare(
        f: &common::Fixture,
        bin: &Bin,
        extra: &[&str],
        stderr: bool,
        env: &[(&str, &str)],
    ) -> Self {
        let mut c = Command::new(&bin.exe);
        c.arg("mcp")
            .args(extra)
            .envs(env.iter().copied())
            .current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
            .env("FAKE_SCENARIO", "ok")
            .env("FAKE_MCP_LOG", bin.log())
            .env("FAKE_MCP_MODE_FILE", bin.dir.path().join("fake-mcp.mode"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if stderr { Stdio::piped() } else { Stdio::null() });
        for var in [
            "FAKE_TASK_ID",
            "THOTH_RUN_ID",
            "THOTH_INBOX_DIR",
            "THOTH_EXE",
            "CLAUDE_PLUGIN_ROOT",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "ARTIFICIALANALYSIS_API_KEY",
            "AISTUPIDLEVEL_API_KEY",
            "THOTH_RECORDS_DIR",
            "THOTH_MACHINE_NAME",
        ] {
            c.env_remove(var);
        }
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take();
        let out = child.stdout.take().unwrap();
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            let mut r = BufReader::new(out);
            loop {
                let mut buf = Vec::new();
                match r.read_until(b'\n', &mut buf) {
                    Ok(0) | Err(_) => {
                        let _ = tx.send(None);
                        return;
                    }
                    Ok(_) => {
                        if buf.last() == Some(&b'\n') {
                            buf.pop();
                        }
                        let _ = tx.send(Some(buf));
                    }
                }
            }
        });
        let stderr = child.stderr.take().map(|err| {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                for line in BufReader::new(err).lines() {
                    let Ok(line) = line else { return };
                    let _ = tx.send(line);
                }
            });
            rx
        });
        Self { child, input, lines, stderr }
    }

    fn write_raw(&mut self, bytes: &[u8]) {
        let input = self.input.as_mut().unwrap();
        input.write_all(bytes).unwrap();
        input.flush().unwrap();
    }

    fn send(&mut self, v: &Value) {
        self.write_raw(format!("{v}\n").as_bytes());
    }

    /// The next stdout line, `None` at EOF; fails the test at the deadline.
    fn next(&self, deadline: Instant) -> Option<Vec<u8>> {
        match self.lines.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => panic!("timed out waiting for the server's stdout"),
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    fn response_within(&mut self, id: &Value, within: Duration) -> Reply {
        let deadline = Instant::now() + within;
        let mut notes = Vec::new();
        loop {
            let raw = self
                .next(deadline)
                .unwrap_or_else(|| panic!("stdout ended before the answer to {id}"));
            let v: Value = serde_json::from_slice(&raw)
                .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&raw)));
            match v.get("id") {
                None => notes.push(raw),
                Some(got) => {
                    assert_eq!(got, id, "{v}");
                    return Reply { v, raw, notes };
                }
            }
        }
    }

    fn response(&mut self, id: &Value) -> Reply {
        self.response_within(id, RESPONSE)
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Reply {
        self.send(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}));
        self.response(&json!(id))
    }

    fn guide(&mut self, id: u64) -> Reply {
        self.call(id, "thoth_guide", json!({}))
    }

    /// Nothing at all on stdout for 2 seconds.
    fn quiet(&self) {
        match self.lines.recv_timeout(QUIET) {
            Err(RecvTimeoutError::Timeout) => {}
            Ok(Some(l)) => panic!("unexpected line: {}", String::from_utf8_lossy(&l)),
            Ok(None) | Err(RecvTimeoutError::Disconnected) => panic!("stdout ended"),
        }
    }

    /// No notification before `after`, then exactly one list_changed within
    /// 5 seconds, then nothing for 2 seconds.
    fn one_list_changed(&self, after: &Reply) {
        assert!(after.notes.is_empty(), "{:?}", notes(after));
        let line = self
            .next(Instant::now() + NOTE)
            .expect("stdout ended before list_changed");
        let v: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(v, json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}));
        self.quiet();
    }

    fn close_stdin(&mut self) {
        drop(self.input.take());
    }

    fn expect_eof(&self, within: Duration) {
        if let Some(l) = self.next(Instant::now() + within) {
            panic!("a line instead of EOF: {}", String::from_utf8_lossy(&l));
        }
    }

    fn wait_exit(&mut self, within: Duration) -> ExitStatus {
        let deadline = Instant::now() + within;
        loop {
            if let Some(s) = self.child.try_wait().unwrap() {
                return s;
            }
            assert!(Instant::now() < deadline, "the server did not exit within {within:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn notes(r: &Reply) -> Vec<String> {
    r.notes.iter().map(|n| String::from_utf8_lossy(n).into_owned()).collect()
}

/// Neither the hand-over notice nor the fallback warning.
fn assert_plain(r: &Reply) {
    for t in r.texts() {
        assert!(!t.starts_with(NOTICE) && !t.starts_with(WARNING), "{}", r.v);
    }
}

/// Answered by the old server's own tools; returns the warning.
fn assert_local(r: &Reply, code: &str) -> String {
    let texts = r.texts();
    assert!(!texts.iter().any(|t| t.starts_with("fake-mcp ")), "{}", r.v);
    let first = r.first();
    assert!(first.starts_with(WARNING), "{}", r.v);
    assert!(first.contains(&format!("({code}: ")), "{first}");
    assert!(!r.v.to_string().contains(MAY_HAVE_RUN), "{}", r.v);
    first
}

fn assert_notice(r: &Reply, bin: &Bin) {
    let first = r.first();
    assert!(first.starts_with(NOTICE), "{}", r.v);
    assert!(first.contains(&bin.exe.display().to_string()), "{first}");
}

// ---- the fake's log ----

fn events(log: &Path) -> Vec<Value> {
    match std::fs::read_to_string(log) {
        Ok(s) => s.lines().map(|l| serde_json::from_str(l).unwrap()).collect(),
        Err(_) => Vec::new(),
    }
}

fn unhex(s: &str) -> Vec<u8> {
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
}

/// How many times the fake started with (`mcp`) or without `mcp` first.
fn starts(log: &Path, mcp: bool) -> usize {
    events(log)
        .iter()
        .filter_map(|e| e.get("argv"))
        .filter(|a| (a[0] == "mcp") == mcp)
        .count()
}

/// The decoded `recv_hex` or `sent_hex` lines, in order.
fn logged(log: &Path, key: &str) -> Vec<Vec<u8>> {
    events(log).iter().filter_map(|e| e[key].as_str().map(unhex)).collect()
}

fn logged_with_id(log: &Path, key: &str, id: &Value) -> Vec<Vec<u8>> {
    logged(log, key)
        .into_iter()
        .filter(|b| serde_json::from_slice::<Value>(b).ok().is_some_and(|v| v.get("id") == Some(id)))
        .collect()
}

fn one_logged(log: &Path, key: &str, id: &Value) -> Vec<u8> {
    let found = logged_with_id(log, key, id);
    assert_eq!(found.len(), 1, "{key} for {id}");
    found.into_iter().next().unwrap()
}

// ---- tests ----

#[test]
fn mcp_replaced_hands_calls_to_the_new_thoth() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    let before = s.guide(1);
    assert_plain(&before);
    assert_eq!(before.v["result"]["isError"], false, "{}", before.v);

    // An update that renames the running file away and puts the same bytes back
    bin.replace(THOTH, false);
    let r = s.guide(2);
    assert_plain(&r);
    assert!(r.notes.is_empty(), "{:?}", notes(&r));

    bin.replace(THOTH, true);
    let r = s.guide(3);
    assert_notice(&r, &bin);
    assert_eq!(r.texts()[1..], before.texts()[..]);
    assert_eq!(r.sc(), before.sc());
    assert!(r.notes.is_empty(), "{:?}", notes(&r));

    let r = s.guide(4);
    assert_plain(&r);
    assert_eq!(r.texts(), before.texts());
    assert!(r.notes.is_empty(), "{:?}", notes(&r));
    s.quiet();
}

#[test]
fn mcp_replaced_forwards_to_a_server_with_another_tool_list() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start_bare(&f, &bin, &["--max-wait", "7s"], false, &[]);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"first-client","version":"1"}}}));
    let r = s.response(&json!(1));
    assert_eq!(r.v["result"]["capabilities"], json!({"tools":{"listChanged":true}}));
    s.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));

    bin.replace(FAKE, false);
    let second = json!({"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"second-client","version":"2"}});
    s.send(&json!({"jsonrpc":"2.0","id":2,"method":"initialize","params":second}));
    let r = s.response(&json!(2));
    assert_eq!(r.v["result"]["serverInfo"]["name"], "thoth", "{}", r.v);
    assert!(r.notes.is_empty(), "{:?}", notes(&r));

    let r = s.guide(3);
    let texts = r.texts();
    assert!(texts[0].starts_with(NOTICE), "{}", r.v);
    assert!(texts[0].contains("(version 0.0.0-fake)"), "{}", texts[0]);
    assert_eq!(texts[1], "fake-mcp thoth_guide");
    let sc = r.sc();
    assert_eq!(sc["initialize_params"], second);
    assert_eq!(sc["initialized"], true);
    assert_eq!(sc["argv"], json!(["mcp", "--max-wait", "7s"]));
    assert_eq!(
        std::fs::canonicalize(sc["cwd"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(f.repo.path()).unwrap()
    );
    assert_eq!(sc["thoth_home"], json!(f.home.path().to_str().unwrap()));
    s.one_list_changed(&r);

    s.send(&json!({"jsonrpc":"2.0","id":4,"method":"tools/list"}));
    let r = s.response(&json!(4));
    assert!(r.notes.is_empty(), "{:?}", notes(&r));
    let tools = r.v["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 1, "{}", r.v);
    assert_eq!(tools[0]["name"], "fake_only_tool");

    let r = s.call(5, "fake_only_tool", json!({}));
    assert_eq!(r.texts()[0], "fake-mcp fake_only_tool");
    assert_eq!(r.notes.len(), 1, "{:?}", notes(&r));
    let note: Value = serde_json::from_slice(&r.notes[0]).unwrap();
    assert_eq!(note["method"], "notifications/message");
}

#[test]
fn mcp_replaced_forwards_the_metadata_generation() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start_bare(&f, &bin, &[], false, &[]);
    let meta = json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}});
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":meta}}));
    let r = s.response(&json!(1));
    assert_eq!(r.v["result"]["capabilities"], json!({"tools":{"listChanged":true}}), "{}", r.v);

    bin.replace(FAKE, false);
    let future = json!({"io.modelcontextprotocol/protocolVersion":"2030-01-01","io.modelcontextprotocol/clientCapabilities":{}});
    s.send(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":future}}));
    let r = s.response(&json!(2));
    assert_eq!(r.v["error"]["code"], -32022, "{}", r.v);
    assert!(r.notes.is_empty(), "{:?}", notes(&r));
    assert_eq!(starts(&bin.log(), true) + starts(&bin.log(), false), 0);
    s.quiet();

    s.send(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"thoth_guide","arguments":{},"_meta":meta}}));
    let r = s.response(&json!(3));
    assert_notice(&r, &bin);
    assert_eq!(r.sc()["initialize_params"], Value::Null);
    assert_eq!(r.sc()["first_list_meta"], meta);
    s.one_list_changed(&r);
}

#[test]
fn mcp_replaced_keeps_answering_for_its_own_jobs() {
    let _g = lock();
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    let started = s.call(1, "thoth_verify_start", json!({"run":"01DOESNOTEXIST"}));
    assert_eq!(started.v["result"]["isError"], false, "{}", started.v);
    let job_id = started.sc()["job_id"].as_str().unwrap().to_owned();

    bin.replace(FAKE, false);
    let r = s.call(2, "thoth_job_status", json!({"job_id":job_id,"wait_seconds":30}));
    assert_plain(&r);
    assert_eq!(r.sc()["status"], "failed", "{}", r.v);
    assert_eq!(r.sc()["exit_code"], 1);
    assert_eq!(r.sc()["recovered"], false);

    let r = s.call(3, "thoth_job_status", json!({"job_id":"01ARZ3NDEKTSV4RRFFQ69G5FAW"}));
    assert!(r.texts().contains(&"fake-mcp thoth_job_status".to_owned()), "{}", r.v);
    assert_eq!(r.sc()["fake_mcp"], true);
}

#[test]
fn mcp_replaced_falls_back_and_retries_only_after_the_file_changes() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.mode("exit");
    bin.replace(FAKE, false);

    let r = s.guide(1);
    let warning = assert_local(&r, "handshake_failed");
    assert!(warning.contains(&bin.exe.display().to_string()), "{warning}");
    assert!(warning.contains("when that file changes"), "{warning}");
    assert!(warning.contains("/mcp"), "{warning}");

    assert_local(&s.guide(2), "handshake_failed");
    assert_eq!(starts(&bin.log(), true), 1);

    bin.mode("ok");
    assert_local(&s.guide(3), "handshake_failed");
    assert_eq!(starts(&bin.log(), true), 1);

    bin.move_away();
    std::fs::write(&bin.exe, "not an executable\n").unwrap();
    let r = s.guide(4);
    let warning = assert_local(&r, "spawn_failed");
    assert!(warning.contains("on the next call"), "{warning}");

    bin.replace(FAKE, true);
    let r = s.guide(5);
    assert_notice(&r, &bin);
    assert_eq!(starts(&bin.log(), true), 2);
}

/// A file that can be started but not read counts as unchanged: nothing is
/// started from it, so it cannot fail in a way that is tried on every call.
#[test]
fn mcp_replaced_starts_nothing_it_cannot_read() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.move_away();
    std::fs::write(&bin.exe, "not an executable\n").unwrap();
    assert_local(&s.guide(1), "spawn_failed");

    // spawn_failed tries again on every call, but only once it can stamp the file
    bin.mode("exit");
    bin.replace(FAKE, false);
    if !bin.deny_reading() {
        eprintln!("skipped: this process can read a file without read permission (root)");
        return;
    }
    assert_local(&s.guide(2), "spawn_failed");
    assert_local(&s.guide(3), "spawn_failed");
    assert_eq!(starts(&bin.log(), true), 0);

    bin.allow_reading();
    assert_local(&s.guide(4), "handshake_failed");
    assert_local(&s.guide(5), "handshake_failed");
    assert_eq!(starts(&bin.log(), true), 1);

    // after handshake_failed, new bytes that cannot be read are no change yet
    bin.mode("ok");
    bin.replace(FAKE, true);
    assert!(bin.deny_reading());
    assert_local(&s.guide(6), "handshake_failed");
    assert_eq!(starts(&bin.log(), true), 1);

    bin.allow_reading();
    assert_notice(&s.guide(7), &bin);
    assert_eq!(starts(&bin.log(), true), 2);
}

#[test]
fn mcp_replaced_does_not_rerun_a_call_the_new_server_may_have_run() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.mode("crash-on-call");
    bin.replace(FAKE, false);

    let r = s.guide(1);
    assert_eq!(r.v["result"]["isError"], true, "{}", r.v);
    assert!(r.sc()["error"].as_str().unwrap().contains(MAY_HAVE_RUN), "{}", r.v);
    assert!(!r.first().starts_with(WARNING), "{}", r.v);
    let received = logged(&bin.log(), "recv_hex")
        .into_iter()
        .filter(|b| {
            serde_json::from_slice::<Value>(b)
                .ok()
                .is_some_and(|v| v["id"] == 1 && v["method"] == "tools/call")
        })
        .count();
    assert_eq!(received, 1);
    assert_eq!(starts(&bin.log(), false), 0);
    s.one_list_changed(&r);

    assert_local(&s.guide(2), "exited");
}

#[test]
fn mcp_replaced_gives_up_on_a_silent_server() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.mode("silent");
    bin.replace(FAKE, false);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"thoth_guide","arguments":{}}}));
    let r = s.response_within(&json!(1), Duration::from_secs(60));
    assert_local(&r, "handshake_failed");
}

#[test]
fn mcp_replaced_twice_reaches_the_newest_server() {
    let _g = lock();
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.replace(THOTH, true);
    let r = s.guide(1);
    assert_notice(&r, &bin);
    assert!(r.notes.is_empty(), "{:?}", notes(&r));
    s.quiet();
    let started = s.call(2, "thoth_verify_start", json!({"run":"01DOESNOTEXIST"}));
    assert_eq!(started.v["result"]["isError"], false, "{}", started.v);
    let job_id = started.sc()["job_id"].as_str().unwrap().to_owned();

    bin.replace(FAKE, false);
    let r = s.guide(3);
    assert!(r.texts().contains(&"fake-mcp thoth_guide".to_owned()), "{}", r.v);
    s.one_list_changed(&r);

    let r = s.call(4, "thoth_job_status", json!({"job_id":job_id,"wait_seconds":30}));
    assert!(r.notes.is_empty(), "{:?}", notes(&r));
    assert_eq!(r.sc()["status"], "failed", "{}", r.v);
    assert_eq!(r.sc()["recovered"], false);
}

#[test]
fn mcp_replaced_forwarding_does_not_hold_the_client_pipes() {
    let _g = lock();
    let f = common::fixture();
    commit_slow_script(&f);
    let (_task_id, run_id) = settled_run(&f);
    let (_, _, shell, cmd) = SLOW;
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.replace(THOTH, true);
    let added = s.call(1, "thoth_checks_add", json!({"id":"slow","cmd":cmd,"shell":shell}));
    assert_notice(&added, &bin);
    assert_eq!(added.v["result"]["isError"], false, "{}", added.v);
    approve(&f);
    let started = s.call(2, "thoth_verify_start", json!({"run":run_id}));
    assert_plain(&started);
    assert_eq!(started.sc()["status"], "working", "{}", started.v);
    let job_id = started.sc()["job_id"].as_str().unwrap().to_owned();

    // EOF on stdin: stdout must end and the server exit well before the
    // 5-second check does; nothing it started may hold the client's pipes.
    s.close_stdin();
    s.expect_eof(Duration::from_secs(3));
    let status = s.wait_exit(Duration::from_secs(3));
    assert!(status.success());
    drop(s);

    let mut second = Server::start(&f, &bin, &[]);
    let deadline = Instant::now() + Duration::from_secs(300);
    let mut id = 10;
    loop {
        id += 1;
        let r = second.call(id, "thoth_job_status", json!({"job_id":job_id}));
        assert_eq!(r.v["result"]["isError"], false, "{}", r.v);
        if r.sc()["status"] == "completed" {
            assert_eq!(r.sc()["result"]["ok"], true, "{}", r.v);
            break;
        }
        assert_eq!(r.sc()["status"], "unknown", "{}", r.v);
        assert!(Instant::now() < deadline, "the job never finished: {}", r.v);
        std::thread::sleep(Duration::from_millis(500));
    }
}

#[cfg(unix)]
#[test]
fn mcp_replaced_on_unix_after_unlink_and_chmod() {
    use std::os::unix::fs::PermissionsExt;
    let _g = lock();
    let f = common::fixture();
    let bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    std::fs::remove_file(&bin.exe).unwrap();
    bin.put(THOTH, true);
    assert_notice(&s.guide(1), &bin);
    drop(s);

    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.replace(FAKE, false);
    std::fs::set_permissions(&bin.exe, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_local(&s.guide(1), "spawn_failed");
    std::fs::set_permissions(&bin.exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_notice(&s.guide(2), &bin);
}

#[test]
fn mcp_replaced_reports_a_server_that_exits_during_a_ping() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.mode("crash-on-ping");
    bin.replace(FAKE, false);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}));
    let r = s.response(&json!(1));
    assert!(r.v.get("result").is_none(), "{}", r.v);
    assert_eq!(r.v["error"]["code"], -32603, "{}", r.v);
    assert!(r.v["error"]["message"].as_str().unwrap().contains(MAY_HAVE_RUN), "{}", r.v);
    s.one_list_changed(&r);
    assert_local(&s.guide(2), "exited");
}

#[test]
fn mcp_replaced_refuses_a_server_with_another_protocol_version() {
    let _g = lock();
    let f = common::fixture();
    let root = stale_plugin(&f);
    let mut bin = Bin::new();
    let mut s =
        Server::start_bare(&f, &bin, &["--plugin-root", root.to_str().unwrap()], false, &[]);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}));
    s.response(&json!(1));
    s.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    bin.mode("other-version");
    bin.replace(FAKE, false);
    let r = s.guide(2);
    assert_local(&r, "handshake_failed");
    assert!(r.texts()[1].starts_with("warning: the installed Thoth plugin at "), "{}", r.v);
    assert!(r.notes.is_empty(), "{:?}", notes(&r));
    s.quiet();
}

#[test]
fn mcp_replaced_forwards_lines_byte_for_byte() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.replace(FAKE, false);
    let r = s.guide(1);
    assert_notice(&r, &bin);
    s.one_list_changed(&r);
    let log = bin.log();

    // (1) a request in an unusual layout, ending in \r\n
    let line = "{ \"params\" :{\"arguments\" : { \"text\" : \"h\u{e9}llo \u{1F30D} \u{2713}\" } , \"name\":\"fake_only_tool\"},\"extra\" : [1, 2.50, null] ,  \"id\" : \"\u{ef}d-1\", \"method\" : \"tools/call\" , \"jsonrpc\":\"2.0\" }";
    s.write_raw(format!("{line}\r\n").as_bytes());
    let id = json!("\u{ef}d-1");
    let r = s.response(&id);
    assert_eq!(one_logged(&log, "recv_hex", &id), line.as_bytes());
    assert_eq!(r.raw, one_logged(&log, "sent_hex", &id));
    assert_eq!(r.notes.len(), 1, "{:?}", notes(&r));
    let message = logged(&log, "sent_hex")
        .into_iter()
        .rfind(|b| String::from_utf8_lossy(b).contains("notifications/message"))
        .unwrap();
    assert_eq!(r.notes[0], message);

    // (2) a notification, then a ping
    let cancelled = "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":\"\u{ef}d-1\",\"reason\":\"t\u{e9}st\"}}";
    s.write_raw(format!("{cancelled}\n").as_bytes());
    s.send(&json!({"jsonrpc":"2.0","id":7,"method":"ping"}));
    let r = s.response(&json!(7));
    assert_eq!(r.v["result"], json!({}));
    let received = logged(&log, "recv_hex");
    let at_cancel = received.iter().position(|b| b == cancelled.as_bytes()).expect("the notification was forwarded");
    let at_ping = received
        .iter()
        .position(|b| serde_json::from_slice::<Value>(b).ok().is_some_and(|v| v["id"] == 7))
        .unwrap();
    assert!(at_cancel < at_ping);

    // (3) lines that are not requests are answered here
    for (raw, code, id) in [
        ("not json", -32700, Value::Null),
        ("[1,2]", -32600, Value::Null),
        (r#"{"jsonrpc":"2.0","id":10,"method":5}"#, -32600, json!(10)),
    ] {
        s.write_raw(format!("{raw}\n").as_bytes());
        let r = s.response(&id);
        assert_eq!(r.v["error"]["code"], code, "{}", r.v);
        assert!(!logged(&log, "recv_hex").iter().any(|b| b == raw.as_bytes()), "{raw}");
    }

    // (4) initialize goes to the new server too
    let init = "{\"params\":{\"clientInfo\":{\"version\":\"1\",\"name\":\"cl\u{ef}ent\"},\"capabilities\":{},\"protocolVersion\":\"2025-06-18\"},\"method\":\"initialize\",\"id\":11,\"jsonrpc\":\"2.0\"}";
    s.write_raw(format!("{init}\n").as_bytes());
    let r = s.response(&json!(11));
    assert_eq!(r.raw, one_logged(&log, "sent_hex", &json!(11)));
    assert_eq!(r.v["result"]["serverInfo"]["name"], "fake-mcp");
    assert_eq!(one_logged(&log, "recv_hex", &json!(11)), init.as_bytes());
    let r = s.call(12, "fake_only_tool", json!({}));
    let sent: Value = serde_json::from_str(init).unwrap();
    assert_eq!(r.sc()["initialize_params"], sent["params"]);

    // (5) a last line without \n, then EOF
    let last = r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"fake_only_tool","arguments":{}}}"#;
    s.write_raw(last.as_bytes());
    s.close_stdin();
    let r = s.response_within(&json!(13), Duration::from_secs(30));
    assert_eq!(r.raw, one_logged(&log, "sent_hex", &json!(13)));
    s.expect_eof(END);
    assert_eq!(s.wait_exit(END).code(), Some(0));
    assert_eq!(one_logged(&log, "recv_hex", &json!(13)), last.as_bytes());
}

#[test]
fn mcp_replaced_relays_an_unterminated_last_line_from_the_new_server() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.mode("unterminated-on-call");
    bin.replace(FAKE, false);
    s.send(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"thoth_guide","arguments":{}}}));
    let r = s.response_within(&json!(1), Duration::from_secs(30));
    assert_notice(&r, &bin);
    assert!(r.texts().contains(&"fake-mcp thoth_guide".to_owned()), "{}", r.v);
    s.one_list_changed(&r);

    assert_local(&s.guide(2), "exited");
    assert_local(&s.guide(3), "exited");
    assert_eq!(starts(&bin.log(), true), 1);

    bin.mode("ok");
    bin.replace(FAKE, true);
    let r = s.guide(4);
    assert_notice(&r, &bin);
    s.one_list_changed(&r);
    assert_eq!(starts(&bin.log(), true), 2);

    s.close_stdin();
    s.expect_eof(Duration::from_secs(3));
    assert_eq!(s.wait_exit(Duration::from_secs(3)).code(), Some(0));
}

/// The answer to the handshake's `tools/list` is the new executable's last
/// line, without a `\n`: the server knows its output ended before it would
/// write the call that started the hand-over, so it answers that call itself.
///
/// The server is started with the debug-build seam
/// `THOTH_TEST_MCP_EOF_DELAY_MS` (src/mcp/forward.rs): its reader thread
/// holds the end of the new executable's output for 2 seconds before it tells
/// the loop. A reader that reported the answer and the end as two messages
/// would let the loop write the call in that gap, and the call would come
/// back as a "may already have run" error with its line in the fake's log;
/// without the seam that gap is nanoseconds wide and this test cannot see it.
#[test]
fn mcp_replaced_does_not_send_to_a_server_that_closed_after_the_handshake() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let delay = [("THOTH_TEST_MCP_EOF_DELAY_MS", "2000")];
    let mut s = Server::start_env(&f, &bin, &[], false, &delay);
    bin.mode("unterminated-on-list");
    bin.replace(FAKE, false);
    let r = s.guide(1);
    assert_local(&r, "exited");
    let log = bin.log();
    assert!(logged_with_id(&log, "recv_hex", &json!(1)).is_empty());
    let sent = logged(&log, "sent_hex");
    assert_eq!(sent.len(), 1, "{sent:?}");
    let probed: Value = serde_json::from_slice(&sent[0]).unwrap();
    assert!(probed["result"]["tools"].is_array(), "{probed}");
    s.one_list_changed(&r);

    assert_local(&s.guide(2), "exited");
    assert_eq!(starts(&log, true), 1);
}

#[test]
fn mcp_replaced_drops_stray_lines_from_the_new_server() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start_with(&f, &bin, &[], true);
    bin.mode("noise");
    bin.replace(FAKE, false);
    let first = s.guide(1);
    assert_notice(&first, &bin);
    s.one_list_changed(&first);
    let second = s.call(2, "fake_only_tool", json!({}));
    assert_eq!(second.texts()[0], "fake-mcp fake_only_tool");
    for line in [&first, &second].iter().flat_map(|r| r.notes.iter().chain([&r.raw])) {
        let v: Value = serde_json::from_slice(line).unwrap();
        assert!(v.is_object());
        assert!(v.get("id") != Some(&json!("x-1")) && v.get("id") != Some(&json!("x-2")), "{v}");
    }

    let stderr = s.stderr.take().unwrap();
    let deadline = Instant::now() + NOTE;
    let (mut noise, mut dropped) = (false, 0);
    while !(noise && dropped == 6) {
        let line = stderr
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap_or_else(|_| panic!("stderr: noise {noise}, dropped {dropped}"));
        noise |= line == "fake-mcp noise";
        if line.starts_with("thoth mcp: dropped a line from the new executable") {
            dropped += 1;
        }
    }
    let more_until = Instant::now() + QUIET;
    while let Ok(line) = stderr.recv_timeout(more_until.saturating_duration_since(Instant::now())) {
        assert!(!line.starts_with("thoth mcp: dropped a line"), "{line}");
    }
}

#[test]
fn mcp_replaced_notices_a_server_that_exited_while_idle() {
    let _g = lock();
    let f = common::fixture();
    let mut bin = Bin::new();
    let mut s = Server::start(&f, &bin, &[]);
    bin.mode("exit-after-call");
    bin.replace(FAKE, false);
    let r = s.guide(1);
    assert_notice(&r, &bin);
    s.one_list_changed(&r);
    std::thread::sleep(Duration::from_secs(2));
    let r = s.guide(2);
    assert_local(&r, "exited");
    s.one_list_changed(&r);
}
