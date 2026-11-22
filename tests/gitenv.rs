//! GITENV-1: git's configuration-by-environment group (GIT_CONFIG_COUNT=N,
//! then GIT_CONFIG_KEY_n and GIT_CONFIG_VALUE_n for every n below N) is one
//! unit. A worker, reviewer or spec reviewer gets all of it, or none of it
//! when one of its values is a known secret value; a git setting name in
//! GIT_CONFIG_KEY_n is not a secret value.
//!
//! Every thoth process goes through `clean`, which drops the test process's
//! variables whose names hold KEY, TOKEN, SECRET or PASSWORD and the whole
//! inherited git group (names only are read), then sets the five variables
//! Claude Code's Linux shells set. State is read only through thoth's CLI and
//! what the fakes record: `FAKE_ENV_LOG` (names only) and `FAKE_GIT_ENV_LOG`
//! (only the GIT_CONFIG_ variables, with their values). The stored secret is
//! joined from pieces, so no whole secret is in this file.

mod common;

use common::{git, Fixture};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const MODEL: &str = "gemini-3.8-flash-low";

/// The group Claude Code on Linux starts its shells with.
const FIVE: [(&str, &str); 5] = [
    ("GIT_CONFIG_COUNT", "2"),
    ("GIT_CONFIG_KEY_0", "credential.interactive"),
    ("GIT_CONFIG_VALUE_0", "false"),
    ("GIT_CONFIG_KEY_1", "credential.guiPrompt"),
    ("GIT_CONFIG_VALUE_1", "false"),
];

/// A secret-named dummy set next to the group in one run.
const EXTRA: (&str, &str) = ("GITENV_TEST_TOKEN", "dummy-gitenv-extra-1");

/// The value stored with `config set-secret`, joined from pieces.
fn stored_value() -> String {
    ["dummy", "Gitenv", "Stored", "Value", "1"].concat()
}

/// `f.cmd()` without any inherited variable whose name holds KEY, TOKEN,
/// SECRET or PASSWORD, and without any inherited GIT_CONFIG_COUNT,
/// GIT_CONFIG_KEY_n or GIT_CONFIG_VALUE_n: the whole group goes, never a KEY
/// alone (names only are read). The fixture already clears the two benchmark
/// keys.
fn clean(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        let secretish = ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w));
        let git_group = upper == "GIT_CONFIG_COUNT"
            || upper.starts_with("GIT_CONFIG_KEY_")
            || upper.starts_with("GIT_CONFIG_VALUE_");
        if secretish || git_group {
            c.env_remove(&name);
        }
    }
    c
}

/// `clean` with the five variables of the group set.
fn with_five(f: &Fixture) -> assert_cmd::Command {
    let mut c = clean(f);
    for (k, v) in FIVE {
        c.env(k, v);
    }
    c
}

fn run_json(mut c: assert_cmd::Command, what: &str) -> Value {
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

/// A fixture after `thoth init` (run with the five set), with an agy
/// implementer profile and a claude reviewer profile. Returns the agy
/// profile's config dir, the fake's data dir.
fn setup() -> (Fixture, PathBuf) {
    let f = common::fixture();
    let mut c = with_five(&f);
    c.arg("init");
    run_json_or_ok(c, "init");
    let cfg = f.add_profile_with("agy-a", "google", "antigravity");
    f.add_profile("claude-b");
    (f, cfg)
}

/// `init` prints no JSON; only its success matters.
fn run_json_or_ok(mut c: assert_cmd::Command, what: &str) {
    let out = c.output().expect("spawn thoth");
    assert!(
        out.status.success(),
        "thoth {what} exited {:?}\nstdout: {}\nstderr: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// `task create --file SPEC --json` run with the five set: the whole output.
fn create(f: &Fixture, spec: &Value) -> std::process::Output {
    let path = f.home_path().join(format!("spec-{}.json", spec_counter()));
    std::fs::write(&path, spec.to_string()).unwrap();
    let mut c = with_five(f);
    c.args(["task", "create", "--file"]).arg(&path).arg("--json");
    c.output().unwrap()
}

fn spec_counter() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    N.fetch_add(1, Ordering::SeqCst)
}

fn spec(goal: &str) -> Value {
    json!({"goal": goal, "acceptance_criteria": ["AC1: prints hello"], "constraints": [],
        "non_goals": [], "excluded_alternatives": [], "risk": "low"})
}

/// `task create` of a plain spec; returns the task id.
fn plain_task(f: &Fixture) -> String {
    let out = create(f, &spec("say hello"));
    assert!(out.status.success(), "task create: {}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    v["task_id"].as_str().unwrap().to_string()
}

/// `config set-secret aistupidlevel-api-key`, the value fed on stdin.
fn store_secret(f: &Fixture) {
    let mut c = clean(f);
    c.args(["config", "set-secret", "aistupidlevel-api-key"]).write_stdin(format!("{}\n", stored_value()));
    run_json_or_ok(c, "config set-secret");
}

/// Every line a fake appended to `path`, parsed.
fn log_lines(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

/// Each launch's GIT_CONFIG_COUNT, GIT_CONFIG_KEY_n and GIT_CONFIG_VALUE_n
/// from a `FAKE_GIT_ENV_LOG` file, with their values.
fn git_groups(path: &Path) -> Vec<Value> {
    log_lines(path)
        .into_iter()
        .map(|l| {
            let all = l["git_config"].as_object().expect("git_config").clone();
            let group: serde_json::Map<String, Value> = all
                .into_iter()
                .filter(|(k, _)| k == "GIT_CONFIG_COUNT" || k.starts_with("GIT_CONFIG_KEY_") || k.starts_with("GIT_CONFIG_VALUE_"))
                .collect();
            Value::Object(group)
        })
        .collect()
}

/// Each launch's variable names from a `FAKE_ENV_LOG` file.
fn env_names(path: &Path) -> Vec<Vec<String>> {
    log_lines(path)
        .into_iter()
        .map(|l| l["names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect())
        .collect()
}

fn five_json() -> Value {
    Value::Object(FIVE.iter().map(|(k, v)| (k.to_string(), json!(v))).collect())
}

/// `dispatch TASK --profile agy-a` of fake-agy's `ok`, from `c`.
fn agy_dispatch(mut c: assert_cmd::Command, cfg: &Path, task: &str, logs: (&Path, &Path)) -> Value {
    c.env("FAKE_SCENARIO", "ok")
        .env("FAKE_TASK_ID", task)
        .env("FAKE_AGY_DATA_DIR", cfg)
        .env("FAKE_ENV_LOG", logs.0)
        .env("FAKE_GIT_ENV_LOG", logs.1)
        .args(["dispatch", task, "--profile", "agy-a", "--model", MODEL, "--effort", "medium", "--json"]);
    run_json(c, "dispatch")
}

/// AC6, AC4: an agy implementer and a claude reviewer started from thoth
/// commands given the five variables get all five with their values; git
/// works in the implementer (its commit is recorded and its worktree is
/// clean); a secret-named variable set beside the group still stays out.
#[test]
fn the_git_config_group_reaches_an_implementer_and_a_reviewer_whole() {
    let (f, cfg) = setup();
    let task = plain_task(&f);

    let names_log = f.home_path().join("impl-names.jsonl");
    let git_log = f.home_path().join("impl-git.jsonl");
    let mut c = with_five(&f);
    c.env(EXTRA.0, EXTRA.1);
    let d = agy_dispatch(c, &cfg, &task, (&names_log, &git_log));
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    let groups = git_groups(&git_log);
    assert!(!groups.is_empty(), "the implementer logged no launch");
    for g in &groups {
        assert_eq!(*g, five_json(), "the implementer's git group");
    }
    let launches = env_names(&names_log);
    assert!(!launches.is_empty(), "the implementer logged no names");
    for seen in &launches {
        assert!(!seen.iter().any(|n| n == EXTRA.0), "{} reached the worker", EXTRA.0);
        for (k, _) in FIVE {
            assert!(seen.iter().any(|n| n == k), "{k} did not reach the worker");
        }
    }

    // git worked inside the worker: nothing left uncommitted, and the commit
    // evidence holds the file the fake added
    let wt = PathBuf::from(d["worktree"].as_str().unwrap());
    assert_eq!(git(&wt, &["status", "--porcelain"]), "", "the worktree is not clean");
    let commits: Vec<Value> = f.evidence_rows(&task).into_iter().filter(|r| r["kind"] == "commit").collect();
    assert!(!commits.is_empty(), "no commit evidence");
    let shown: Vec<String> = commits
        .iter()
        .map(|r| {
            let mut c = with_five(&f);
            c.args(["evidence", "show", r["code"].as_str().unwrap(), "--task", &task]);
            let out = c.output().unwrap();
            assert!(out.status.success(), "evidence show: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).into_owned()
        })
        .collect();
    assert!(shown.iter().any(|s| s.contains("agy_added.txt")), "the commit evidence: {shown:?}");

    // the reviewer
    let git_log = f.home_path().join("review-git.jsonl");
    let mut c = with_five(&f);
    c.env("FAKE_SCENARIO", "review_pass")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_GIT_ENV_LOG", &git_log)
        .args(["review", "start", &task, "--profile", "claude-b", "--model", "haiku", "--effort", "medium", "--json"]);
    let r = run_json(c, "review start");
    let mut c = clean(&f);
    c.args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s", "--json"]);
    run_json(c, "wait --run");
    let groups = git_groups(&git_log);
    assert!(!groups.is_empty(), "the reviewer logged no launch");
    for g in &groups {
        assert_eq!(*g, five_json(), "the reviewer's git group");
    }
}

/// AC7, AC2: when GIT_CONFIG_VALUE_0 equals a value stored with `config
/// set-secret`, the worker gets none of the five, and the run still settles.
#[test]
fn a_group_holding_a_stored_secret_reaches_the_worker_not_at_all() {
    let (f, cfg) = setup();
    store_secret(&f);
    let task = plain_task(&f);

    let names_log = f.home_path().join("names.jsonl");
    let git_log = f.home_path().join("git.jsonl");
    let mut c = with_five(&f);
    c.env("GIT_CONFIG_VALUE_0", stored_value());
    agy_dispatch(c, &cfg, &task, (&names_log, &git_log));
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");

    let launches = env_names(&names_log);
    assert!(!launches.is_empty(), "the worker logged no names");
    for seen in &launches {
        for (k, _) in FIVE {
            assert!(!seen.iter().any(|n| n == k), "{k} reached the worker");
        }
    }
    for g in git_groups(&git_log) {
        assert_eq!(g, json!({}), "a group member reached the worker");
    }
}

/// AC8, AC3: with the five set, a spec mentioning credential.interactive is
/// created and shown unmasked; a spec holding the stored secret is still
/// refused under the secret's own kind.
#[test]
fn a_git_setting_name_is_not_a_secret_value() {
    let (f, _) = setup();
    store_secret(&f);

    let out = create(&f, &spec("keep credential.interactive off for unattended workers"));
    assert!(out.status.success(), "task create: {}", String::from_utf8_lossy(&out.stderr));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true, "{v}");
    let task = v["task_id"].as_str().unwrap().to_string();

    let mut c = with_five(&f);
    c.args(["task", "show", &task, "--json"]);
    let shown = run_json(c, "task show");
    let text = shown.to_string();
    assert!(text.contains("keep credential.interactive off"), "{text}");
    assert!(!text.contains("[redacted:env:GIT_CONFIG_KEY_0]"), "{text}");

    let out = create(&f, &spec(&format!("deploy with {} now", stored_value())));
    assert!(!out.status.success(), "a spec holding a stored secret was accepted");
    let said = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(said.contains("thoth:aistupidlevel-api-key"), "{said}");
    assert!(!said.contains("GIT_CONFIG_KEY_0"), "{said}");
    assert!(!said.contains(&stored_value()), "the refusal printed the secret");
}

// --- TESTGAP-1 (GITENV-1 gaps) ---------------------------------------------------------------

/// A count git reads but Thoth does not loses its keys today. git reads
/// GIT_CONFIG_COUNT with strtoul, so ` 2` and `+2` both mean 2 to it; Thoth's
/// rule is digits only (docs/adapter-usage.md: a decimal number N), so such a
/// count is not a group and its KEY variables are removed like any variable
/// named KEY. This test asserts today's behaviour of that documented
/// limitation, not a wish: when Thoth learns to read such counts, the
/// commander updates it to expect the whole group.
#[test]
fn a_count_git_reads_but_thoth_does_not_loses_its_keys_today() {
    let (f, cfg) = setup();
    for count in [" 2", "+2"] {
        let task = plain_task(&f);
        let names_log = f.home_path().join(format!("names-{}.jsonl", count.trim().replace('+', "plus")));
        let git_log = f.home_path().join(format!("git-{}.jsonl", count.trim().replace('+', "plus")));
        let mut c = with_five(&f);
        c.env("GIT_CONFIG_COUNT", count);
        agy_dispatch(c, &cfg, &task, (&names_log, &git_log));
        f.wait_for(&task, "120s");

        let groups = git_groups(&git_log);
        assert!(!groups.is_empty(), "[{count:?}] the worker logged no launch");
        for g in &groups {
            assert_eq!(
                *g,
                json!({"GIT_CONFIG_COUNT": count, "GIT_CONFIG_VALUE_0": "false", "GIT_CONFIG_VALUE_1": "false"}),
                "[{count:?}] the worker's git group"
            );
        }
        let launches = env_names(&names_log);
        assert!(!launches.is_empty(), "[{count:?}] the worker logged no names");
        for seen in &launches {
            for key in ["GIT_CONFIG_KEY_0", "GIT_CONFIG_KEY_1"] {
                assert!(!seen.iter().any(|n| n == key), "[{count:?}] {key} reached the worker");
            }
        }
    }
}

/// A KEY outside any group never reaches the worker, and a stray KEY beside a
/// whole group does not come with it. No group is inherited (`clean`).
#[test]
fn a_git_config_key_outside_a_group_does_not_reach_the_worker() {
    let (f, cfg) = setup();

    // (a) GIT_CONFIG_KEY_0 alone, set to a setting name
    let task = plain_task(&f);
    let names_log = f.home_path().join("stray-a-names.jsonl");
    let git_log = f.home_path().join("stray-a-git.jsonl");
    let mut c = clean(&f);
    c.env("GIT_CONFIG_KEY_0", "credential.interactive");
    agy_dispatch(c, &cfg, &task, (&names_log, &git_log));
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    let launches = env_names(&names_log);
    assert!(!launches.is_empty(), "(a) the worker logged no names");
    for seen in &launches {
        assert!(!seen.iter().any(|n| n == "GIT_CONFIG_KEY_0"), "(a) GIT_CONFIG_KEY_0 reached the worker");
    }
    let groups = git_groups(&git_log);
    assert!(!groups.is_empty(), "(a) the worker logged no launch");
    for g in groups {
        assert_eq!(g, json!({}), "(a) a group member reached the worker");
    }

    // (b) a group of one, and a stray KEY_1 beside it
    let task = plain_task(&f);
    let names_log = f.home_path().join("stray-b-names.jsonl");
    let git_log = f.home_path().join("stray-b-git.jsonl");
    let mut c = clean(&f);
    c.env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "credential.interactive")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "credential.guiPrompt");
    agy_dispatch(c, &cfg, &task, (&names_log, &git_log));
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    let launches = env_names(&names_log);
    assert!(!launches.is_empty(), "(b) the worker logged no names");
    for seen in &launches {
        for k in ["GIT_CONFIG_COUNT", "GIT_CONFIG_KEY_0", "GIT_CONFIG_VALUE_0"] {
            assert!(seen.iter().any(|n| n == k), "(b) {k} did not reach the worker");
        }
        assert!(!seen.iter().any(|n| n == "GIT_CONFIG_KEY_1"), "(b) the stray GIT_CONFIG_KEY_1 reached the worker");
    }
    let groups = git_groups(&git_log);
    assert!(!groups.is_empty(), "(b) the worker logged no launch");
    for g in groups {
        assert_eq!(
            g,
            json!({"GIT_CONFIG_COUNT": "1", "GIT_CONFIG_KEY_0": "credential.interactive", "GIT_CONFIG_VALUE_0": "false"}),
            "(b) the worker's git group"
        );
    }
}

/// A spec reviewer started from thoth commands given the five variables gets
/// all five with their values.
#[test]
fn the_git_config_group_reaches_a_spec_reviewer_whole() {
    let (f, _cfg) = setup();
    let task = plain_task(&f);
    let git_log = f.home_path().join("spec-review-git.jsonl");
    let mut c = with_five(&f);
    c.env("FAKE_SCENARIO", "spec_review_approve")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_GIT_ENV_LOG", &git_log)
        .args([
            "spec", "review", "start", &task, "--profile", "claude-b", "--model", "haiku", "--effort", "medium",
            "--reason", "gitenv fixture: fake spec reviewer in a temporary repo", "--json",
        ]);
    let r = run_json(c, "spec review start");
    let mut c = clean(&f);
    c.args(["wait", "--run", r["run_id"].as_str().unwrap(), "--timeout", "180s", "--json"]);
    run_json(c, "wait --run");
    let groups = git_groups(&git_log);
    assert!(!groups.is_empty(), "the spec reviewer logged no launch");
    for g in &groups {
        assert_eq!(*g, five_json(), "the spec reviewer's git group");
    }
}

/// A run interrupted by quota and resumed from thoth commands given the five
/// variables gets all five with their values in the resumed launch.
#[test]
fn the_git_config_group_reaches_a_resumed_run_whole() {
    let (f, _cfg) = setup();
    let task = plain_task(&f);
    let mut c = with_five(&f);
    c.env("FAKE_SCENARIO", "quota")
        .env("FAKE_TASK_ID", &task)
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let d = run_json(c, "dispatch");
    let run = d["run_id"].as_str().unwrap().to_string();
    assert_eq!(f.wait_for(&task, "120s").1, "interrupted");

    let mut c = clean(&f);
    c.args(["profile", "check", "claude-b"]);
    run_json_or_ok(c, "profile check");

    let git_log = f.home_path().join("resume-git.jsonl");
    let mut c = with_five(&f);
    c.env("FAKE_SCENARIO", "resume")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_GIT_ENV_LOG", &git_log)
        .args(["resume", &run, "--json"]);
    run_json(c, "resume");
    assert_eq!(f.wait_for(&task, "120s").1, "in_review");
    let groups = git_groups(&git_log);
    assert!(!groups.is_empty(), "the resumed run logged no launch");
    for g in &groups {
        assert_eq!(*g, five_json(), "the resumed run's git group");
    }
}

/// A run started by a pipeline from a `pipeline start` given the five
/// variables gets all five with their values (the pipeline's child commands
/// inherit them).
#[test]
fn the_git_config_group_reaches_a_pipeline_started_run_whole() {
    let (f, _cfg) = setup();
    f.add_codex_profile("codex-a");
    let gates = f.home_path().join("check-gates");
    std::fs::create_dir_all(&gates).unwrap();
    let check = format!("\"{}\" exit 0", env!("CARGO_BIN_EXE_fake-check"));
    let mut c = clean(&f);
    c.args(["checks", "add", "green", "--cmd", &check, "--json"]);
    run_json(c, "checks add");
    let mut c = clean(&f);
    c.args(["checks", "approve", "--reason", "gitenv fixture checks", "--json"]);
    run_json(c, "checks approve");
    let task = plain_task(&f);

    let git_log = f.home_path().join("pipeline-git.jsonl");
    let mut c = with_five(&f);
    c.env("FAKE_TASK_ID", &task)
        .env("FAKE_CLAUDE_SCENARIO", "hooks")
        .env("FAKE_CODEX_SCENARIO", "review_pass")
        .env("FAKE_GIT_ENV_LOG", &git_log)
        .env("FAKE_CHECK_DIR", &gates)
        .args([
            "pipeline", "start", &task, "--implementer", "claude-b:sonnet:medium", "--reviewer", "codex-a:gpt-5.6-luna:medium",
            "--reason", "gitenv fixture: fake workers in a temporary repo", "--json",
        ]);
    run_json(c, "pipeline start");

    let mut c = clean(&f);
    c.env("FAKE_CHECK_DIR", &gates).args(["pipeline", "show", &task, "--wait", "240s", "--json"]);
    let shown = run_json(c, "pipeline show");
    assert_eq!(shown["status"], "stopped", "{shown:#}");
    let groups = git_groups(&git_log);
    assert!(!groups.is_empty(), "the pipeline's implementer logged no launch");
    for g in &groups {
        assert_eq!(*g, five_json(), "the pipeline-started run's git group");
    }
}
