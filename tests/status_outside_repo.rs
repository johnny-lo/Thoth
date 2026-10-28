mod common;

use common::{git, Fixture};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

const NOTE: &str = "not inside a git repository: open tasks of every registered repository are \
                    listed; run dispatch, verify and review inside a repository";

/// A temporary folder git does not see as a repository.
fn outside() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new("git")
        .args(["rev-parse", "--git-common-dir"])
        .current_dir(dir.path())
        .output()
        .expect("git must be on PATH");
    assert!(!out.status.success(), "{} is inside a git repository", dir.path().display());
    dir
}

/// A git repo with two commits, built the way `common::fixture` builds one.
fn other_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    let r = repo.path();
    git(r, &["init", "--initial-branch=main"]);
    git(r, &["config", "user.email", "test@example.com"]);
    git(r, &["config", "user.name", "thoth test"]);
    std::fs::write(r.join("a.txt"), "one\n").unwrap();
    git(r, &["add", "a.txt"]);
    git(r, &["commit", "-m", "first"]);
    std::fs::write(r.join("b.txt"), "two\n").unwrap();
    git(r, &["add", "b.txt"]);
    git(r, &["commit", "-m", "second"]);
    repo
}

fn json_in(f: &Fixture, dir: &Path, args: &[&str]) -> Value {
    let out = f.cmd_in(dir).args(args).assert().success().get_output().stdout.clone();
    serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
}

fn init_in(f: &Fixture, dir: &Path) -> Value {
    json_in(f, dir, &["init", "--json"])
}

fn create_in(f: &Fixture, dir: &Path, title: &str) -> String {
    let spec_file = f.home_path().join("spec.json");
    std::fs::write(&spec_file, Fixture::SPEC).unwrap();
    let spec = spec_file.to_str().unwrap();
    let v = json_in(f, dir, &["task", "create", "-f", spec, "--title", title, "--json"]);
    v["task_id"].as_str().unwrap().to_string()
}

/// Repo A (the fixture) with a1, repo B with b1 and closed b2, repo C
/// registered but with its state.db gone; B and C are kept alive here.
struct World {
    f: Fixture,
    _b: tempfile::TempDir,
    _c: tempfile::TempDir,
    a1: String,
    b1: String,
    root_a: String,
    root_b: String,
}

fn world() -> World {
    let f = common::fixture();
    let root_a = init_in(&f, f.repo.path())["repo_root"].as_str().unwrap().to_string();
    let a1 = create_in(&f, f.repo.path(), "alpha one");

    let b = other_repo();
    let root_b = init_in(&f, b.path())["repo_root"].as_str().unwrap().to_string();
    let b1 = create_in(&f, b.path(), "beta one");
    let b2 = create_in(&f, b.path(), "beta two");
    json_in(&f, b.path(), &["task", "close", &b2, "--reason", "x", "--json"]);

    let c = other_repo();
    let init_c = init_in(&f, c.path());
    create_in(&f, c.path(), "gamma one");
    std::fs::remove_file(init_c["state_db"].as_str().unwrap()).unwrap();

    World { f, _b: b, _c: c, a1, b1, root_a, root_b }
}

const OLD_KEYS: [&str; 7] =
    ["task_id", "title", "status", "risk", "runs", "any_heartbeat_stale", "updated_at"];

#[test]
fn status_outside_a_repository_lists_every_registered_repository() {
    let w = world();
    let dir = outside();
    let v = json_in(&w.f, dir.path(), &["status", "--json"]);
    assert_eq!(v["outside_repo"], true, "{v}");
    assert_eq!(v["note"], NOTE);
    assert_eq!(v["last_verify"], Value::Null);

    let mut expected = vec![(w.root_a.clone(), w.a1.clone()), (w.root_b.clone(), w.b1.clone())];
    expected.sort();
    let tasks = v["tasks"].as_array().unwrap();
    let got: Vec<(String, String)> = tasks
        .iter()
        .map(|t| {
            (t["repo"].as_str().unwrap().to_string(), t["task_id"].as_str().unwrap().to_string())
        })
        .collect();
    assert_eq!(got, expected, "{v}");
    for t in tasks {
        for key in OLD_KEYS {
            assert!(t.get(key).is_some(), "{key} missing in {t}");
        }
    }

    let out = w.f.cmd_in(dir.path()).arg("status").assert().success().get_output().stdout.clone();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], NOTE, "{text}");
    assert!(lines.contains(&w.root_a.as_str()), "{text}");
    assert!(lines.contains(&w.root_b.as_str()), "{text}");
    for id in [&w.a1, &w.b1] {
        assert!(lines.iter().any(|l| l.starts_with(&format!("  {id}"))), "{id} in {text}");
    }
    assert!(!text.contains("beta two"), "{text}");
    assert!(!text.contains("gamma one"), "{text}");
}

#[test]
fn status_inside_a_repository_only_gains_keys() {
    let w = world();
    let v = json_in(&w.f, w.f.repo.path(), &["status", "--json"]);
    assert_eq!(v["outside_repo"], false, "{v}");
    assert_eq!(v["note"], Value::Null);
    let tasks = v["tasks"].as_array().unwrap();
    assert_eq!(tasks.len(), 1, "{v}");
    assert_eq!(tasks[0]["task_id"], w.a1.as_str());
    assert_eq!(tasks[0]["repo"], w.root_a.as_str());
    assert!(v.get("last_verify").is_some(), "{v}");

    let fresh = other_repo();
    let out = w.f.cmd_in(fresh.path()).args(["status", "--json"]).assert().code(1);
    let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("is not initialised"), "{stderr}");
}

#[test]
fn status_outside_with_nothing_registered_writes_nothing() {
    let f = common::fixture();
    let dir = outside();
    let v = json_in(&f, dir.path(), &["status", "--json"]);
    assert_eq!(v["tasks"], json!([]), "{v}");
    assert_eq!(v["outside_repo"], true);

    let out = f.cmd_in(dir.path()).arg("status").assert().success().get_output().stdout.clone();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines, [NOTE, "no open tasks"], "{text}");

    let left: Vec<_> = std::fs::read_dir(f.home.path()).unwrap().collect();
    assert!(left.is_empty(), "THOTH_HOME gained {left:?}");
}

#[test]
fn commands_that_need_a_repository_still_refuse_outside_one() {
    let w = world();
    let dir = outside();
    let a1 = w.a1.as_str();
    let cases: [&[&str]; 6] = [
        &["status", a1, "--json"],
        &["dispatch", a1, "--profile", "claude-b", "--json"],
        &["verify", "01ABCDEFGHJKMNPQRSTVWXYZ01", "--json"],
        &["review", "show", a1, "--json"],
        &["checks", "list", "--json"],
        &["events", "--since", "0", "--json"],
    ];
    for args in cases {
        let out = w.f.cmd_in(dir.path()).args(args).assert().code(1);
        let stderr = String::from_utf8(out.get_output().stderr.clone()).unwrap();
        assert!(stderr.contains("is not inside a git repository"), "{args:?}: {stderr}");
    }
}

/// `thoth mcp` with the environment `Fixture::cmd` clears, minus the plugin
/// root so the plugin version check does not refuse state-changing tools.
struct Server {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Server {
    fn start(f: &Fixture, default_repo: &Path) -> Self {
        let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
        c.arg("mcp").arg("--default-repo").arg(default_repo).current_dir(default_repo);
        for var in [
            "THOTH_RUN_ID",
            "THOTH_INBOX_DIR",
            "THOTH_EXE",
            "CLAUDE_CONFIG_DIR",
            "CODEX_HOME",
            "ARTIFICIALANALYSIS_API_KEY",
            "AISTUPIDLEVEL_API_KEY",
            "THOTH_RECORDS_DIR",
            "THOTH_MACHINE_NAME",
            "CLAUDE_PLUGIN_ROOT",
            "FAKE_TASK_ID",
        ] {
            c.env_remove(var);
        }
        c.env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output }
    }

    fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        let v = json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}});
        writeln!(self.input, "{v}").unwrap();
        self.input.flush().unwrap();
        let mut s = String::new();
        self.output.read_line(&mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_status_outside_a_repository() {
    let w = world();
    let dir = outside();
    let mut s = Server::start(&w.f, dir.path());

    let status = s.call(1, "thoth_status", json!({}));
    assert_eq!(status["result"]["isError"], false, "{status}");
    let doc = &status["result"]["structuredContent"];
    assert_eq!(doc["outside_repo"], true, "{status}");
    let a1 = doc["tasks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["task_id"] == w.a1.as_str())
        .unwrap_or_else(|| panic!("a1 missing in {status}"));
    assert_eq!(a1["repo"], w.root_a.as_str());

    let dispatched = s.call(2, "thoth_dispatch", json!({"task": w.a1, "profile": "claude-b"}));
    assert_eq!(dispatched["result"]["isError"], true, "{dispatched}");
    let error = dispatched["result"]["structuredContent"]["error"].as_str().unwrap();
    assert!(error.contains("is not inside a git repository"), "{dispatched}");
}
