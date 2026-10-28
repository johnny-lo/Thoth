#![allow(dead_code)]

use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Run git in `dir` and return trimmed stdout. Panics with git's stderr on failure.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be on PATH");
    assert!(
        out.status.success(),
        "git {:?} failed in {}: {}",
        args,
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// RJ-1: the launch commands no longer pick a model or effort, so the
/// `dispatch` and `review start` helpers add `--model MODEL` and
/// `--effort medium` when `extra` does not give them.
fn launch_defaults(extra: &[&str], model: &'static str) -> Vec<&'static str> {
    let has = |flag: &str| {
        extra.iter().any(|a| *a == flag || a.starts_with(&format!("{flag}=")))
    };
    let mut out = Vec::new();
    if !has("--model") {
        out.extend(["--model", model]);
    }
    if !has("--effort") {
        out.extend(["--effort", "medium"]);
    }
    out
}

pub struct Fixture {
    pub home: TempDir,
    pub repo: TempDir,
}

/// A fresh THOTH_HOME plus a throwaway git repo with two commits.
pub fn fixture() -> Fixture {
    let home = tempfile::tempdir().expect("tempdir");
    let repo = tempfile::tempdir().expect("tempdir");
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
    Fixture { home, repo }
}

impl Fixture {
    pub fn repo_path(&self) -> PathBuf {
        self.repo.path().to_path_buf()
    }

    pub fn home_path(&self) -> PathBuf {
        self.home.path().to_path_buf()
    }

    /// A `thoth` invocation with HOME and cwd pre-wired.
    ///
    /// The variables a thoth run exports to its worker are removed, so a
    /// suite run *inside* a worker (Thoth working on itself) neither trips
    /// `review record`'s run guard nor appends test garbage to that worker's
    /// real inbox. A test that needs one sets it again after this call.
    pub fn cmd(&self) -> assert_cmd::Command {
        let mut c = assert_cmd::Command::new(env!("CARGO_BIN_EXE_thoth"));
        for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE"] {
            c.env_remove(var);
        }
        c.env_remove("CLAUDE_CONFIG_DIR");
        c.env_remove("CODEX_HOME");
        c.env_remove("ARTIFICIALANALYSIS_API_KEY");
        c.env_remove("AISTUPIDLEVEL_API_KEY");
        // RJ-2: Agent Arena needs no key, so point it at a file:// path that
        // does not exist; no test reaches huggingface.co.
        c.env("THOTH_ARENA_URL", offline_arena_url(self.home.path()));
        c.env_remove("THOTH_RECORDS_DIR");
        c.env_remove("THOTH_MACHINE_NAME");
        c.env("THOTH_HOME", self.home.path());
        c.env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"));
        // Never the real codex: every test that launches one gets the fake.
        c.env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"));
        c.env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"));
        c.current_dir(self.repo.path());
        c
    }

    /// Like `cmd`, but run from `dir` instead of the repo root — used to prove
    /// a worker inside its own worktree resolves the same repo (Open question 5).
    pub fn cmd_in(&self, dir: &std::path::Path) -> assert_cmd::Command {
        let mut c = self.cmd();
        c.current_dir(dir);
        c
    }

    /// Same repo_id rule as `ids::repo_id_for`, duplicated because integration
    /// tests are a separate crate and cannot reach into the binary's modules.
    pub fn repo_id(&self) -> String {
        use sha2::{Digest, Sha256};
        let canon = std::fs::canonicalize(self.repo.path()).unwrap();
        let mut s = canon.to_string_lossy().to_string();
        if let Some(stripped) = s.strip_prefix(r"\\?\") {
            s = stripped.to_string();
        }
        let s = s.replace('\\', "/").trim_end_matches('/').to_string();
        #[cfg(windows)]
        let s = s.to_lowercase();
        let hex: String = Sha256::digest(s.as_bytes())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        hex[..12].to_string()
    }

    pub fn state_db(&self) -> PathBuf {
        self.home
            .path()
            .join("repos")
            .join(self.repo_id())
            .join("state.db")
    }

    pub fn state_conn(&self) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(self.state_db())
            .expect("state.db must exist; run init first");
        // Same as src/db/mod.rs::tune: the runner may be writing at the same
        // moment, and an UPDATE from a test should wait it out, not fail.
        conn.busy_timeout(std::time::Duration::from_millis(5000))
            .unwrap();
        conn
    }

    /// TESTS-1: a `thoth ... --json` run for a diagnostic helper. Never
    /// panics and never goes through `run_ok` (whose failure path calls
    /// `explain_recent`, which calls this). `Ok((exit_code, document))` when
    /// the process ran and its stdout parsed as JSON, whatever the exit code;
    /// `Err(text)` names the command and carries stderr otherwise.
    fn cli_json(&self, args: &[&str]) -> Result<(i32, serde_json::Value), String> {
        let mut c = self.cmd();
        c.args(args);
        let out = c
            .output()
            .map_err(|e| format!("could not spawn thoth {args:?}: {e}"))?;
        let code = out.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&out.stdout);
        serde_json::from_str::<serde_json::Value>(stdout.trim())
            .map(|v| (code, v))
            .map_err(|e| {
                format!(
                    "thoth {args:?} exited {code} without a JSON document ({e}); stdout: {}; stderr: {}",
                    stdout.trim(),
                    String::from_utf8_lossy(&out.stderr).trim()
                )
            })
    }

    /// TESTS-1: a `thoth ... --json` run for a reading helper: panics through
    /// `run_ok` (exit code, stderr, newest events) when it fails, and when
    /// stdout is not JSON.
    fn json_ok(&self, args: &[&str], what: &str) -> serde_json::Value {
        let mut c = self.cmd();
        c.args(args);
        let out = self.run_ok(c, what);
        let text = String::from_utf8_lossy(&out);
        serde_json::from_str(text.trim())
            .unwrap_or_else(|e| panic!("thoth {what} printed no JSON document ({e}): {text}"))
    }

    /// Why a review-flow task ended the way it did, as text for a panic
    /// message: the latest `review_invalid` and `result_invalid` event details,
    /// then the newest reviewer run (see `explain_run`). TESTS-1: read from
    /// `events --task` and `status TASK --json`; the newest reviewer is the
    /// one whose first event (its launch) has the highest seq. A document
    /// without its `events` or `runs` array (or a row without a key) is
    /// reported with the command and the response, not read as empty.
    pub fn explain_task(&self, task_id: &str) -> String {
        let events_cmd = format!("events --since 0 --task {task_id} --json");
        let events = match self.cli_json(&["events", "--since", "0", "--task", task_id, "--json"]) {
            Ok((0, v)) => match events_view(&v, &events_cmd) {
                Ok(events) => events,
                Err(e) => return format!("task {task_id}: {e}\n"),
            },
            Ok((code, v)) => return format!("task {task_id}: events --json exited {code}: {v}\n"),
            Err(e) => return format!("task {task_id}: {e}\n"),
        };
        let latest_event = |ty: &str| -> String {
            events
                .iter()
                .rev()
                .find(|e| e.ty == ty)
                .map(|e| e.detail.to_string())
                .unwrap_or_else(|| "(none)".to_string())
        };
        let mut out = format!(
            "review_invalid: {}\nresult_invalid: {}\n",
            latest_event("review_invalid"),
            latest_event("result_invalid")
        );
        let status_cmd = format!("status {task_id} --json");
        let runs = match self.cli_json(&["status", task_id, "--json"]) {
            Ok((0, v)) => match runs_view(&v, &status_cmd) {
                Ok(runs) => runs,
                Err(e) => {
                    out.push_str(&format!("reviewer run: ({e})\n"));
                    return out;
                }
            },
            Ok((code, v)) => {
                out.push_str(&format!("reviewer run: (status --json exited {code}: {v})\n"));
                return out;
            }
            Err(e) => {
                out.push_str(&format!("reviewer run: ({e})\n"));
                return out;
            }
        };
        let reviewers: Vec<&RunView> = runs.iter().filter(|r| r.role == "reviewer").collect();
        match newest_by_launch(&reviewers, &events) {
            None => out.push_str("reviewer run: (none)\n"),
            Some((row, note)) => {
                if let Some(note) = note {
                    out.push_str(&note);
                }
                out.push_str(&describe_run(row));
            }
        }
        out
    }

    /// One run for a panic message: status, interrupted_reason, error_text and
    /// the last 20 lines of its `runner-error.log` and `stderr.log`. TESTS-1:
    /// `wait --run RUN --timeout 0s --json` names the run's task (exit 0 for
    /// a finished run, 3 for a live one; both carry `task_id`), then
    /// `status TASK --json` has the row; a terminal task is found the same way.
    /// A row without a string `run_dir` is an error in the text, not a read
    /// of the current directory.
    pub fn explain_run(&self, run_id: &str) -> String {
        let task_id = match self.cli_json(&["wait", "--run", run_id, "--timeout", "0s", "--json"]) {
            Ok((0 | 3, v)) => match v["task_id"].as_str() {
                Some(t) => t.to_string(),
                None => return format!("run {run_id}: (not found: wait --run gave no task_id: {v})\n"),
            },
            Ok((code, v)) => return format!("run {run_id}: (not found: wait --run exited {code}: {v})\n"),
            Err(e) => return format!("run {run_id}: (not found: {e})\n"),
        };
        let status_cmd = format!("status {task_id} --json");
        let runs = match self.cli_json(&["status", &task_id, "--json"]) {
            Ok((0, v)) => match runs_view(&v, &status_cmd) {
                Ok(runs) => runs,
                Err(e) => return format!("run {run_id}: ({e})\n"),
            },
            Ok((code, v)) => return format!("run {run_id}: (status {task_id} --json exited {code}: {v})\n"),
            Err(e) => return format!("run {run_id}: ({e})\n"),
        };
        let Some(row) = runs.iter().find(|r| r.run_id == run_id) else {
            return format!("run {run_id}: (not found in status {task_id} --json)\n");
        };
        describe_run(row)
    }

    pub fn global_conn(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(self.home.path().join("global.db"))
            .expect("global.db must exist")
    }

    /// Register a profile whose config dir is a real (empty) directory.
    pub fn add_profile(&self, id: &str) -> PathBuf {
        let cfg = self.home.path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        self.cmd()
            .args([
                "profile", "add", id,
                "--provider", "anthropic",
                "--harness", "claude-code",
                "--config-dir",
            ])
            .arg(&cfg)
            .assert()
            .success();
        cfg
    }

    /// Register a codex profile the way docs/phase1/usage.md tells a human to:
    /// the directory exists first, provider openai, harness codex.
    pub fn add_codex_profile(&self, id: &str) -> PathBuf {
        let cfg = self.home.path().join("profiles").join(id);
        std::fs::create_dir_all(&cfg).unwrap();
        self.cmd()
            .args([
                "profile", "add", id,
                "--provider", "openai",
                "--harness", "codex",
                "--config-dir",
            ])
            .arg(&cfg)
            .assert()
            .success();
        cfg
    }

    /// Insert a queued run row directly, the way `dispatch` will in Task 9.
    /// Returns (task_id, run_id, worktree_path).
    pub fn seed_run(&self, profile_id: &str, spec_json: &str) -> (String, String, PathBuf) {
        use rusqlite::params;

        let spec_file = self.home_path().join("seed-spec.json");
        std::fs::write(&spec_file, spec_json).unwrap();
        let out = self
            .cmd()
            .args(["task", "create", "-f"])
            .arg(&spec_file)
            .arg("--json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        let task_id = v["task_id"].as_str().unwrap().to_string();
        let base_commit = v["base_commit"].as_str().unwrap().to_string();

        let repo_id = self.repo_id();
        let wt = self
            .home_path()
            .join("repos")
            .join(&repo_id)
            .join("worktrees")
            .join(format!("{task_id}-a1"));
        let branch = format!("thoth/{task_id}/a1");
        let out = std::process::Command::new("git")
            .arg("worktree")
            .arg("add")
            .arg(&wt)
            .arg("-b")
            .arg(&branch)
            .arg(&base_commit)
            .current_dir(self.repo.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git worktree add: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        let run_id = format!("01SEEDRUN{:016}", std::process::id());
        let run_dir = self
            .home_path()
            .join("repos")
            .join(&repo_id)
            .join("runs")
            .join(&run_id);
        std::fs::create_dir_all(&run_dir).unwrap();
        let cfg = self.home_path().join("profiles").join(profile_id);

        let conn = self.state_conn();
        conn.execute(
            "INSERT INTO runs (run_id, task_id, attempt, role, provider, account, model,
               harness_session_id, config_dir, allowed_tools, worktree, base_commit, spec,
               status, cancel_requested, timeout_seconds, run_dir)
             VALUES (?1,?2,1,'implementer','anthropic',?3,'sonnet',?4,?5,'Bash,Read,Write',
               ?6,?7,?8,'queued',0,1800,?9)",
            params![
                run_id,
                task_id,
                profile_id,
                "11111111-2222-3333-4444-555555555555",
                cfg.display().to_string(),
                wt.display().to_string(),
                base_commit,
                spec_json,
                run_dir.display().to_string(),
            ],
        )
        .unwrap();
        (task_id, run_id, wt)
    }

    pub const SPEC: &'static str = r#"{"goal":"say hello","acceptance_criteria":["AC1: prints hello"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low"}"#;

    /// Two acceptance criteria, for the reviewer tests: a review must name
    /// every criterion, which a one-AC spec cannot exercise.
    pub const SPEC_TWO_AC: &'static str = r#"{"goal":"say hello twice","acceptance_criteria":["AC1: prints hello","AC2: exits zero"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal"}"#;

    /// TESTS-1: every event of a task as `events --since 0 --task TASK --json`
    /// gives it (`seq`, `ts`, `task_id`, `run_id`, `type`, parsed `detail`),
    /// in seq order. For a test that filters by `run_id` or `type` itself.
    pub fn events_json(&self, task_id: &str) -> Vec<serde_json::Value> {
        let v = self.json_ok(&["events", "--since", "0", "--task", task_id, "--json"], "events");
        v["events"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("events --task {task_id} --json has no events array: {v}"))
    }

    /// Every event type recorded for a task, in seq order.
    pub fn events_of(&self, task_id: &str) -> Vec<String> {
        self.events_json(task_id)
            .iter()
            .map(|e| {
                e["type"]
                    .as_str()
                    .unwrap_or_else(|| {
                        panic!("events --since 0 --task {task_id} --json: event without a string type: {e}")
                    })
                    .to_string()
            })
            .collect()
    }

    /// The task's status, from `status TASK --json` (which, unlike the list
    /// without TASK, also shows a `pass`, `cancelled` or `accepted` task).
    pub fn task_status(&self, task_id: &str) -> String {
        let v = self.json_ok(&["status", task_id, "--json"], "status");
        v["status"]
            .as_str()
            .unwrap_or_else(|| panic!("status {task_id} --json has no string status: {v}"))
            .to_string()
    }

    /// TESTS-1: one run of a task as `status TASK --json` lists it in `runs[]`
    /// (status, interrupted_reason, error_text, run_dir, exit_code,
    /// worker_pid, harness_session_id, ...). Panics with the whole status
    /// document when the task has no such run.
    pub fn run_of(&self, task_id: &str, run_id: &str) -> serde_json::Value {
        let v = self.json_ok(&["status", task_id, "--json"], "status");
        let runs = v["runs"]
            .as_array()
            .unwrap_or_else(|| panic!("status {task_id} --json has no runs array:\n{v:#}"));
        runs.iter()
            .find(|r| r["run_id"] == run_id)
            .cloned()
            .unwrap_or_else(|| panic!("task {task_id} has no run {run_id} in status --json:\n{v:#}"))
    }

    /// The task's evidence rows as JSON, in code order, with `detail` already
    /// parsed so a test can index into it. TESTS-1: read from
    /// `evidence list TASK --json` (which writes no event, unlike `show`) and
    /// projected to the nine keys the table used to give: code, kind,
    /// collected_by, label, sha256, bytes, blob_path, detail, run_id.
    pub fn evidence_rows(&self, task_id: &str) -> Vec<serde_json::Value> {
        let v = self.json_ok(&["evidence", "list", task_id, "--json"], "evidence list");
        v["evidence"]
            .as_array()
            .unwrap_or_else(|| panic!("evidence list {task_id} --json has no evidence array: {v}"))
            .iter()
            .map(|e| {
                let mut row = serde_json::Map::new();
                for key in [
                    "code", "kind", "collected_by", "label", "sha256", "bytes", "blob_path",
                    "detail", "run_id",
                ] {
                    // a null stays null; a key the CLI left out is an error,
                    // not a null
                    let value = e.get(key).cloned().unwrap_or_else(|| {
                        panic!("evidence list {task_id} --json: a row has no `{key}` key: {e}")
                    });
                    row.insert(key.to_string(), value);
                }
                serde_json::Value::Object(row)
            })
            .collect()
    }

    /// `task create` and return (task_id, base_commit).
    pub fn create_task(&self, spec_json: &str) -> (String, String) {
        let spec_file = self.home_path().join("spec.json");
        std::fs::write(&spec_file, spec_json).unwrap();
        let out = self
            .cmd()
            .args(["task", "create", "-f"])
            .arg(&spec_file)
            .arg("--json")
            .assert()
            .success()
            .get_output()
            .stdout
            .clone();
        let v: serde_json::Value =
            serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap();
        (
            v["task_id"].as_str().unwrap().to_string(),
            v["base_commit"].as_str().unwrap().to_string(),
        )
    }

    /// `dispatch` with a scenario, returning the parsed JSON result.
    pub fn dispatch(
        &self,
        task_id: &str,
        profile_id: &str,
        scenario: &str,
        extra: &[&str],
    ) -> serde_json::Value {
        let mut c = self.cmd();
        c.env("FAKE_SCENARIO", scenario)
            .env("FAKE_TASK_ID", task_id)
            .args(["dispatch", task_id, "--profile", profile_id, "--json"])
            .args(extra)
            .args(launch_defaults(extra, "sonnet"));
        let out = self.run_ok(c, "dispatch");
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
    }

    /// Runs a command; when it does not exit 0, panics with its exit code,
    /// stdout, stderr, the newest events and the newest run, so a failure
    /// that leaves no output still says where it happened.
    pub fn run_ok(&self, mut c: assert_cmd::Command, what: &str) -> Vec<u8> {
        let out = c.output().expect("spawn thoth");
        if !out.status.success() {
            panic!(
                "thoth {what} exited {:?}
stdout: {}
stderr: {}
{}",
                out.status.code(),
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
                self.explain_recent()
            );
        }
        out.stdout
    }

    /// The last 10 events of the whole repo (seq descending: ts, type,
    /// run_id, detail) and the newest run, for panic messages. TESTS-1: read
    /// from `events --since 0 --json`; the newest run is the one whose first
    /// event (its launch) has the highest seq, found through `explain_run`,
    /// so a run of a finished task is diagnosed too. A run seeded straight
    /// into the table has no event and is not picked. A document without
    /// its `events` array, or an event without a key, is reported with the
    /// command and the response, not read as "no events".
    pub fn explain_recent(&self) -> String {
        let mut out = String::from("--- latest events ---\n");
        let events = match self.cli_json(&["events", "--since", "0", "--json"]) {
            Ok((0, v)) => match events_view(&v, "events --since 0 --json") {
                Ok(events) => events,
                Err(e) => {
                    out.push_str(&format!("({e})\n"));
                    return out;
                }
            },
            Ok((code, v)) => {
                out.push_str(&format!("(events --json exited {code}: {v})\n"));
                return out;
            }
            Err(e) => {
                out.push_str(&format!("({e})\n"));
                return out;
            }
        };
        for e in events.iter().rev().take(10) {
            let detail = if e.detail.is_null() { String::new() } else { e.detail.to_string() };
            out.push_str(&format!(
                "{} {} {} {}\n",
                e.ts,
                e.ty,
                e.run_id.as_deref().unwrap_or(""),
                detail
            ));
        }
        match newest_run_by_launch(&events) {
            Some(run_id) => out.push_str(&self.explain_run(&run_id)),
            None => out.push_str("newest run: (no event names a run)\n"),
        }
        out
    }

    /// `review start` with a scenario, returning the parsed JSON result.
    pub fn review_start(
        &self,
        task_id: &str,
        profile_id: &str,
        scenario: &str,
        extra: &[&str],
    ) -> serde_json::Value {
        let mut c = self.cmd();
        c.env("FAKE_SCENARIO", scenario)
            .env("FAKE_TASK_ID", task_id)
            .args(["review", "start", task_id, "--profile", profile_id, "--json"])
            .args(extra)
            .args(launch_defaults(extra, "opus"));
        let out = self.run_ok(c, "review start");
        serde_json::from_str(String::from_utf8(out).unwrap().trim()).unwrap()
    }

    /// Poll until the task's status is one of `wanted`, and return it.
    ///
    /// Deliberately a *set* to wait for, not "anything other than `in_review`":
    /// waiting for a status to change would return whatever intermediate value
    /// happened to be written first, which is exactly how a bug like a reviewer
    /// run writing `running` hides instead of failing. `thoth wait <task>`
    /// cannot be used here either, because `in_review` is itself wait-terminal
    /// (Open question 6) — `wait --run` is the command-level equivalent, and the
    /// tests use it where a run id is at hand.
    pub fn wait_for_task_status(&self, task_id: &str, wanted: &[&str], secs: u64) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let s = self.task_status(task_id);
            if wanted.contains(&s.as_str()) {
                return s;
            }
            if std::time::Instant::now() >= deadline {
                panic!(
                    "task {task_id} never reached any of {wanted:?} within {secs}s; it is '{s}'
{}",
                    self.explain_task(task_id)
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
    }

    /// `wait` and return (exit_code, final status string). Every current
    /// caller expects `wait` to reach a wait-terminal status (exit 0), never a
    /// timeout (exit 3) or a real error (exit 1); asserting `.success()` here
    /// (M7, fix round 1) means a genuine failure panics with `wait`'s own
    /// stderr attached, instead of this helper panicking on a JSON parse of
    /// whatever partial/absent stdout a failing invocation left behind.
    pub fn wait_for(&self, task_id: &str, timeout: &str) -> (i32, String) {
        let mut c = self.cmd();
        c.args(["wait", task_id, "--timeout", timeout, "--json"]);
        let stdout = self.run_ok(c, "wait");
        let v: serde_json::Value =
            serde_json::from_str(String::from_utf8(stdout).unwrap().trim()).unwrap();
        (0, v["status"].as_str().unwrap_or("?").to_string())
    }
}

/// One event of an `events --json` document, every key the diagnostics use
/// checked by `events_view` (TESTS-1, review round 1).
#[derive(Clone, Debug, PartialEq)]
pub struct EventView {
    pub seq: i64,
    pub ts: String,
    pub run_id: Option<String>,
    pub ty: String,
    /// The parsed `detail`, any JSON value (null when the CLI could not parse
    /// the stored text).
    pub detail: serde_json::Value,
}

/// One row of `status TASK --json`'s `runs[]`, the keys the diagnostics use
/// checked by `runs_view` (TESTS-1, review round 1).
#[derive(Clone, Debug, PartialEq)]
pub struct RunView {
    pub run_id: String,
    pub role: String,
    pub status: String,
    pub interrupted_reason: Option<String>,
    pub error_text: Option<String>,
    pub run_dir: String,
}

/// The message for a key that is missing or has the wrong type: the command
/// whose response it is, where in the document, the key, what was wanted,
/// and the whole response.
fn key_error(command: &str, at: &str, key: &str, want: &str, doc: &serde_json::Value) -> String {
    format!("{command}: {at} has no {want} `{key}`; response: {doc}")
}

/// `obj[key]` as a string, or the error.
fn str_key(
    obj: &serde_json::Value,
    key: &str,
    command: &str,
    at: &str,
    doc: &serde_json::Value,
) -> Result<String, String> {
    obj.get(key)
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| key_error(command, at, key, "string", doc))
}

/// `obj[key]` as a string or null (`None`), or the error: a key that is not
/// there at all is an error too.
fn opt_str_key(
    obj: &serde_json::Value,
    key: &str,
    command: &str,
    at: &str,
    doc: &serde_json::Value,
) -> Result<Option<String>, String> {
    match obj.get(key) {
        Some(serde_json::Value::Null) => Ok(None),
        Some(serde_json::Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(key_error(command, at, key, "string or null", doc)),
    }
}

/// The `events[]` of an `events --json` document, checked: the array must be
/// there, and every event must carry an integer `seq`, string `ts` and
/// `type`, a `run_id` that is a string or null, and a `detail`. `Err` names
/// `command`, the key and the response; it never reads a missing array as
/// empty. For the explain helpers, which must not panic.
pub fn events_view(doc: &serde_json::Value, command: &str) -> Result<Vec<EventView>, String> {
    let events = doc
        .get("events")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| key_error(command, "the document", "events", "array", doc))?;
    events
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let at = format!("events[{i}]");
            Ok(EventView {
                seq: e
                    .get("seq")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| key_error(command, &at, "seq", "integer", doc))?,
                ts: str_key(e, "ts", command, &at, doc)?,
                run_id: opt_str_key(e, "run_id", command, &at, doc)?,
                ty: str_key(e, "type", command, &at, doc)?,
                detail: e
                    .get("detail")
                    .cloned()
                    .ok_or_else(|| key_error(command, &at, "detail", "value", doc))?,
            })
        })
        .collect()
}

/// The `runs[]` of a `status TASK --json` document, checked: the array must
/// be there, and every row must carry string `run_id`, `role`, `status` and
/// `run_dir`, and `interrupted_reason` and `error_text` as string or null.
/// `Err` names `command`, the key and the response; a row without `run_dir`
/// is an error, never the current directory. For the explain helpers.
pub fn runs_view(doc: &serde_json::Value, command: &str) -> Result<Vec<RunView>, String> {
    let runs = doc
        .get("runs")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| key_error(command, "the document", "runs", "array", doc))?;
    runs.iter()
        .enumerate()
        .map(|(i, r)| {
            let at = format!("runs[{i}]");
            Ok(RunView {
                run_id: str_key(r, "run_id", command, &at, doc)?,
                role: str_key(r, "role", command, &at, doc)?,
                status: str_key(r, "status", command, &at, doc)?,
                interrupted_reason: opt_str_key(r, "interrupted_reason", command, &at, doc)?,
                error_text: opt_str_key(r, "error_text", command, &at, doc)?,
                run_dir: str_key(r, "run_dir", command, &at, doc)?,
            })
        })
        .collect()
}

/// One `status TASK --json` run row as diagnostic text: status,
/// interrupted_reason, error_text, then the last 20 lines of the run dir's
/// `runner-error.log` and `stderr.log` (`(absent)` for a missing file; an
/// empty `run_dir` is said so, not read as the current directory).
fn describe_run(row: &RunView) -> String {
    let shown = |v: &Option<String>| v.clone().unwrap_or_else(|| "(null)".to_string());
    let mut out = format!(
        "run {}: status={} interrupted_reason={} error_text={}\n",
        row.run_id,
        row.status,
        shown(&row.interrupted_reason),
        shown(&row.error_text)
    );
    if row.run_dir.is_empty() {
        out.push_str("(run_dir is empty: no runner-error.log or stderr.log to show)\n");
        return out;
    }
    let dir = PathBuf::from(&row.run_dir);
    for name in ["runner-error.log", "stderr.log"] {
        out.push_str(&format!("--- {name} (last 20 lines) ---\n"));
        match std::fs::read_to_string(dir.join(name)) {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                let from = lines.len().saturating_sub(20);
                for l in &lines[from..] {
                    out.push_str(l);
                    out.push('\n');
                }
            }
            Err(_) => out.push_str("(absent)\n"),
        }
    }
    out
}

/// The seq of the first event that names `run_id`: the event its launch
/// wrote. `None` for a run no event names (one seeded straight into the
/// table). `events` are in seq order.
fn launch_seq(events: &[EventView], run_id: &str) -> Option<i64> {
    events
        .iter()
        .find(|e| e.run_id.as_deref() == Some(run_id))
        .map(|e| e.seq)
}

/// Of `rows` (status run rows), the one launched last: the highest launch
/// seq. A later status event of an older run does not make it newer. When no
/// row has a launch event the highest run_id is taken, and the note says so.
fn newest_by_launch<'a>(
    rows: &[&'a RunView],
    events: &[EventView],
) -> Option<(&'a RunView, Option<String>)> {
    let launched = rows
        .iter()
        .filter_map(|r| launch_seq(events, &r.run_id).map(|seq| (seq, *r)))
        .max_by_key(|(seq, _)| *seq);
    if let Some((_, row)) = launched {
        return Some((row, None));
    }
    rows.iter().max_by(|a, b| a.run_id.cmp(&b.run_id)).map(|row| {
        (
            *row,
            Some("(no run has a launch event; the highest run_id is shown)\n".to_string()),
        )
    })
}

/// The run launched last in the whole repo: the run_id of the event with
/// the highest seq among each run's first event. `None` when no event names
/// a run.
fn newest_run_by_launch(events: &[EventView]) -> Option<String> {
    let mut seen: Vec<(i64, &str)> = Vec::new();
    for e in events {
        let Some(run_id) = e.run_id.as_deref() else { continue };
        if seen.iter().any(|(_, r)| *r == run_id) {
            continue;
        }
        seen.push((e.seq, run_id));
    }
    seen.into_iter().max_by_key(|(seq, _)| *seq).map(|(_, r)| r.to_string())
}

/// A `file://` URL under `home` that no test creates, for `THOTH_ARENA_URL`:
/// an Agent Arena fetch against it fails at once instead of reaching the
/// network. Tests that start `thoth mcp` themselves set it too.
pub fn offline_arena_url(home: &Path) -> String {
    let shown = home.join("no-arena-here").to_string_lossy().replace('\\', "/");
    format!("file:///{}", shown.trim_start_matches('/'))
}

/// Length of the `show_time` text at the start of `shown`:
/// `YYYY-MM-DD HH:MM` plus ` UTC` or ` +HH:MM`.
pub fn shown_time_len(shown: &str) -> usize {
    if shown.get(17..20) == Some("UTC") { 20 } else { 23 }
}

/// How the report shows the UTC instant `y-m-d h:min`, in the zone whose
/// label `shown` carries. The report uses the machine's local zone, which a
/// test cannot pin on Windows (`TZ` is Unix-only), so it reads the label back
/// and checks the digits against it. Only rolls over days, not months.
pub fn local_label_time(y: i32, m: u32, d: u32, h: i32, min: i32, shown: &str) -> String {
    let label = &shown[17..shown_time_len(shown)];
    let offset = if label == "UTC" {
        0
    } else {
        let sign = if label.starts_with('-') { -1 } else { 1 };
        sign * (label[1..3].parse::<i32>().unwrap() * 60 + label[4..6].parse::<i32>().unwrap())
    };
    let local = h * 60 + min + offset;
    let day = d as i32 + local.div_euclid(24 * 60);
    let minutes = local.rem_euclid(24 * 60);
    format!("{y:04}-{m:02}-{day:02} {:02}:{:02} {label}", minutes / 60, minutes % 60)
}

/// Normalises an image name for comparison: file name only, lowercased, one
/// trailing `.exe` dropped - the same stem rule `cancel` applies to
/// `process::image_name`.
pub fn image_stem(s: &str) -> String {
    let name = Path::new(s.trim())
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    name.strip_suffix(".exe").map(str::to_string).unwrap_or(name)
}

/// Whether `pid` is listed **and** is still running `image` (compared by
/// `image_stem`), so a pid the OS has since handed to an unrelated process
/// does not count as alive. The portable form of the `tasklist` checks these
/// tests used before Linux support.
///
/// Windows: `tasklist`'s image column. Linux: `/proc/<pid>/comm` (the name
/// `exec` was given, cut to 15 bytes - so the expected name is cut the same
/// way), with a zombie counted as gone, as `tasklist` does not list an
/// exited process either.
pub fn pid_alive(pid: u32, image: &str) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        // `"thoth.exe","1234","Console","1","5,000 K"` - or a "no tasks"
        // notice when the pid is gone.
        let Some(line) = stdout.lines().find(|l| l.contains(&format!("\"{pid}\""))) else {
            return false;
        };
        let listed = line.trim_start_matches('"');
        let listed = listed.split('"').next().unwrap_or("");
        image_stem(listed) == image_stem(image)
    }
    #[cfg(not(windows))]
    {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return false;
        };
        let state = stat
            .rfind(')')
            .and_then(|i| stat[i + 1..].split_whitespace().next())
            .unwrap_or("");
        if matches!(state, "Z" | "X" | "x") {
            return false;
        }
        let Ok(comm) = std::fs::read_to_string(format!("/proc/{pid}/comm")) else {
            return false;
        };
        let want = image_stem(image);
        let want: String = want.chars().take(15).collect();
        comm.trim_end_matches('\n').to_lowercase() == want
    }
}

/// Kills `pid` alone - no tree, no job/cgroup - the way a crash would.
/// Windows: `taskkill /PID <pid> /F`; Linux: `kill -9 <pid>`.
pub fn kill_pid_only(pid: u32) {
    #[cfg(windows)]
    let out = std::process::Command::new("taskkill")
        .args(["/PID", &pid.to_string(), "/F"])
        .output()
        .unwrap();
    #[cfg(not(windows))]
    let out = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "killing pid {pid} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
