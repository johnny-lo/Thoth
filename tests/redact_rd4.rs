//! RD-4: the five holes of the final review. Diagnostics on stderr, JSON
//! object keys, worker variables that copy a known value under another name,
//! and human lines cut before they were masked. Every fake secret is built by
//! concatenation, so no full key format appears in this file; only variable
//! names are ever read from the test process's environment.

mod common;

use common::Fixture;
use serde_json::{json, Value};
use std::path::Path;

/// A github-format token.
fn token() -> String {
    format!("{}{}", ["gh", "p_"].concat(), "Dummy0123Dummy0123Dummy0123Dummy0123")
}

/// Values in none of the seven formats, at least 40 characters, not starting
/// with x and not all digits; all three differ.
fn v() -> String {
    ["dummy", "Rd4Opaque", "Value0123456789", "abcdefghijKLMNOP"].concat()
}

fn v2() -> String {
    ["dummy", "Rd4Stored", "Value0123456789", "qrstuvwxyzABCDEF"].concat()
}

fn v3() -> String {
    ["dummy", "Rd4Plain", "Value0123456789", "GHIJKLMNOPQRSTUVW"].concat()
}

/// `f.cmd()` without any variable of the test process whose name holds KEY,
/// TOKEN, SECRET or PASSWORD (names only), so the host's values take no part.
fn clean(f: &Fixture) -> assert_cmd::Command {
    let mut c = f.cmd();
    for (name, _) in std::env::vars_os() {
        let upper = name.to_string_lossy().to_ascii_uppercase();
        if ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| upper.contains(w)) {
            c.env_remove(&name);
        }
    }
    c
}

/// `clean` with `RD4_TEST_TOKEN=V`.
fn with_v(f: &Fixture) -> assert_cmd::Command {
    let mut c = clean(f);
    c.env("RD4_TEST_TOKEN", v());
    c
}

fn run(mut c: assert_cmd::Command) -> (Option<i32>, String, String) {
    let out = c.output().unwrap();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// stdout of a command that must exit 0.
fn ok(c: assert_cmd::Command, what: &str) -> String {
    let (code, out, err) = run(c);
    assert_eq!(code, Some(0), "{what}: {err}");
    out
}

fn ok_json(c: assert_cmd::Command, what: &str) -> Value {
    serde_json::from_str(ok(c, what).trim()).unwrap()
}

/// `text` holds no `n`-character piece of `secret` from `from` on.
fn no_piece(text: &str, secret: &str, from: usize, n: usize) {
    assert!(!text.contains(secret), "the whole secret is in the output");
    let tail = &secret[from..];
    for i in 0..=tail.len() - n {
        assert!(!text.contains(&tail[i..i + n]), "{n} characters of a secret, from offset {}, are out", from + i);
    }
}

/// init and a claude profile, every command through `clean`.
fn setup(f: &Fixture) {
    let mut c = clean(f);
    c.arg("init");
    ok(c, "init");
    let cfg = f.home_path().join("profiles").join("claude-b");
    std::fs::create_dir_all(&cfg).unwrap();
    let mut c = clean(f);
    c.args(["profile", "add", "claude-b", "--provider", "anthropic", "--harness", "claude-code", "--config-dir"])
        .arg(&cfg);
    ok(c, "profile add");
}

fn write_spec(f: &Fixture, name: &str, spec: &str) -> std::path::PathBuf {
    let p = f.home_path().join(name);
    std::fs::write(&p, spec).unwrap();
    p
}

/// `task create -f SPEC --json` through `clean`; the task id.
fn create(f: &Fixture, spec: &str) -> String {
    let p = write_spec(f, "spec.json", spec);
    let mut c = clean(f);
    c.args(["task", "create", "-f"]).arg(&p).arg("--json");
    ok_json(c, "task create")["task_id"].as_str().unwrap().to_string()
}

fn task_ids(f: &Fixture) -> Vec<String> {
    let mut c = clean(f);
    c.args(["status", "--json"]);
    let v = ok_json(c, "status");
    v["tasks"].as_array().unwrap().iter().map(|t| t["task_id"].as_str().unwrap().to_string()).collect()
}

fn event_count(f: &Fixture, task: &str) -> usize {
    let mut c = clean(f);
    c.args(["events", "--since", "0", "--task", task, "--json"]);
    ok_json(c, "events")["events"].as_array().unwrap().len()
}

fn wait_run(f: &Fixture, run_id: &str) -> Value {
    let mut c = clean(f);
    c.args(["wait", "--run", run_id, "--timeout", "180s", "--json"]);
    ok_json(c, "wait --run")
}

/// A two-criterion spec with an extra `metadata` object, written as raw text.
fn spec_with_metadata(metadata: &str) -> String {
    format!(
        r#"{{"goal":"say hello twice","acceptance_criteria":["AC1: prints hello","AC2: exits zero"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"normal","metadata":{metadata}}}"#
    )
}

// --- AC1: stderr ------------------------------------------------------------

/// AC1 (a): a token passed as an extra argument is masked in the parse error.
#[test]
fn rd4_a_parse_error_masks_a_token_argument() {
    let f = common::fixture();
    let t = token();
    let mut c = clean(&f);
    c.args(["config", "get", "scores-url", &t, "--json"]);
    let (code, _, err) = run(c);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("[redacted:github-token]"), "{err}");
    no_piece(&err, &t, 4, 16);
}

/// AC1 (b): a known value passed as an argument is masked by its name.
#[test]
fn rd4_a_parse_error_masks_a_known_value_argument() {
    let f = common::fixture();
    let mut c = with_v(&f);
    c.args(["config", "get", "scores-url", &v(), "--json"]);
    let (code, _, err) = run(c);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("[redacted:env:RD4_TEST_TOKEN]"), "{err}");
    assert!(!err.contains(&v()[..8]), "{err}");
}

/// AC1 (c): with nothing to mask the parse error is as it was.
#[test]
fn rd4_a_parse_error_without_a_secret_is_unchanged() {
    let f = common::fixture();
    let mut c = clean(&f);
    c.args(["config", "get", "scores-url", "plainextra", "--json"]);
    let (code, _, err) = run(c);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("plainextra"), "{err}");
    assert!(!err.contains("[redacted:"), "{err}");
}

// --- AC2: object keys ---------------------------------------------------------

/// AC2 (i): a key spelled with a `\u` escape is still refused, by create and
/// by update-spec, and nothing is stored.
#[test]
fn rd4_a_secret_in_an_object_key_is_refused() {
    let f = common::fixture();
    setup(&f);
    let t = token();
    let escaped = format!("\\u{:04x}{}", u32::from('g'), &t[1..]);
    let bad = spec_with_metadata(&format!(r#"{{"{escaped}":true}}"#));
    assert!(!bad.contains(&t), "the raw spec carries the token unescaped");
    let check = |err: &str| {
        let doc: Value = serde_json::from_str(err.trim()).unwrap_or_else(|_| panic!("not JSON: {err}"));
        let msg = doc["error"].as_str().unwrap();
        assert!(msg.starts_with("refusing: the spec contains what looks like a secret"), "{msg}");
        assert!(msg.contains("github-token in metadata.[redacted:github-token]"), "{msg}");
        no_piece(err, &t, 4, 16);
    };

    let before = task_ids(&f);
    let p = write_spec(&f, "bad.json", &bad);
    let mut c = clean(&f);
    c.args(["task", "create", "-f"]).arg(&p).args(["--title", "T", "--json"]);
    let (code, _, err) = run(c);
    assert_eq!(code, Some(1), "{err}");
    check(&err);
    assert_eq!(task_ids(&f), before, "a task was created");

    let task = create(&f, Fixture::SPEC_TWO_AC);
    let show = |f: &Fixture| {
        let mut c = clean(f);
        c.args(["task", "show", &task, "--json"]);
        ok_json(c, "task show")["spec"].clone()
    };
    let spec_before = show(&f);
    let events_before = event_count(&f, &task);
    let mut c = clean(&f);
    c.args(["task", "update-spec", &task, "-f"]).arg(&p).arg("--json");
    let (code, _, err) = run(c);
    assert_eq!(code, Some(1), "{err}");
    check(&err);
    assert_eq!(show(&f), spec_before, "the spec changed");
    assert_eq!(event_count(&f, &task), events_before, "an event was written");
}

/// AC2 (ii): a key stored before its value became known leaves masked.
#[test]
fn rd4_a_known_value_in_a_stored_key_is_masked_on_output() {
    let f = common::fixture();
    setup(&f);
    let spec = spec_with_metadata(&format!(r#"{{"{}":true,"owner":"plain","list":[1,2]}}"#, v()));
    let task = create(&f, &spec);

    let mut c = with_v(&f);
    c.args(["task", "show", &task, "--json"]);
    let out = ok(c, "task show");
    assert!(!out.contains(&v()), "V is in the output");
    let masked: Value = serde_json::from_str(out.trim()).unwrap();

    let mut c = clean(&f);
    c.args(["task", "show", &task, "--json"]);
    let plain_out = ok(c, "task show");
    assert!(plain_out.contains(&v()), "V is not there without the variable");
    let plain: Value = serde_json::from_str(plain_out.trim()).unwrap();

    assert_eq!(
        masked["spec"]["metadata"],
        json!({"[redacted:env:RD4_TEST_TOKEN]": true, "owner": "plain", "list": [1, 2]})
    );
    let mut renamed = plain.clone();
    let meta = renamed["spec"]["metadata"].as_object_mut().unwrap();
    let value = meta.remove(&v()).unwrap();
    meta.insert("[redacted:env:RD4_TEST_TOKEN]".to_string(), value);
    assert_eq!(masked, renamed, "something besides the key changed");
}

// --- AC3: worker environment ------------------------------------------------------

/// Each line of a `FAKE_ENV_LOG` file: the names one launch saw.
fn names(path: &Path) -> Vec<Vec<String>> {
    let launches: Vec<Vec<String>> = std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let v: Value = serde_json::from_str(l).unwrap();
            v["names"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_string()).collect()
        })
        .collect();
    assert!(!launches.is_empty(), "the worker logged no launch");
    launches
}

/// AC3 (a)-(c): copies of a known value are taken away whatever their name,
/// a variable that only contains one (PATH) and an unknown value stay.
#[test]
fn rd4_worker_loses_aliases_of_known_values() {
    let f = common::fixture();
    setup(&f);
    let mut c = clean(&f);
    c.args(["config", "set-secret", "artificialanalysis-api-key"]).write_stdin(format!("{}\n", v2()));
    ok(c, "config set-secret");
    let task = create(&f, Fixture::SPEC_TWO_AC);

    let sep = if cfg!(windows) { ";" } else { ":" };
    let missing = f.home_path().join(format!("no-such-{}", v()));
    let path = format!("{}{sep}{}", std::env::var("PATH").unwrap(), missing.display());
    let log = f.home_path().join("env-rd4.jsonl");
    let mut c = with_v(&f);
    c.env("RD4_ALIAS", v())
        .env("RD4_ALIAS2", v2())
        .env("RD4_PLAIN3", v3())
        .env("PATH", &path)
        .env("FAKE_SCENARIO", "hooks")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_ENV_LOG", &log)
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let d = ok_json(c, "dispatch");
    wait_run(&f, d["run_id"].as_str().unwrap());
    for seen in names(&log) {
        for gone in ["RD4_TEST_TOKEN", "RD4_ALIAS", "RD4_ALIAS2"] {
            assert!(!seen.iter().any(|n| n == gone), "{gone} reached the worker");
        }
        assert!(seen.iter().any(|n| n == "RD4_PLAIN3"), "RD4_PLAIN3 did not reach the worker");
        assert!(seen.iter().any(|n| n.eq_ignore_ascii_case("PATH")), "PATH did not reach the worker");
    }
}

// --- AC4: masked before cut -------------------------------------------------------

/// AC4 (a): a source quote is masked whole before `task result` cuts it.
#[test]
fn rd4_task_result_masks_a_quote_before_cutting_it() {
    let f = common::fixture();
    setup(&f);
    let spec = r#"{"goal":"find out how the uploader retries","acceptance_criteria":["AC1: the report is written"],"constraints":[],"non_goals":[],"excluded_alternatives":[],"risk":"low","checks":[],"task_type":"research"}"#;
    let task = create(&f, spec);
    let quote = format!("{} {}", "x".repeat(99), v());
    // research_ok gives the rest to its second research claim, which must
    // have a source too, so a plain second one is added
    let sources = json!([
        {"url": "https://example.invalid/rd4", "quote": quote},
        {"url": "https://example.invalid/plain", "quote": "plain quote"},
    ]);
    let mut c = clean(&f);
    c.env("FAKE_SCENARIO", "research_ok")
        .env("FAKE_TASK_ID", &task)
        .env("FAKE_SOURCES", sources.to_string())
        .args(["dispatch", &task, "--profile", "claude-b", "--model", "sonnet", "--effort", "medium", "--json"]);
    let d = ok_json(c, "dispatch");
    wait_run(&f, d["run_id"].as_str().unwrap());

    let mut c = with_v(&f);
    c.args(["task", "result", &task]);
    let out = ok(c, "task result");
    assert!(out.contains("[redacted:"), "{out}");
    assert!(!out.contains(&v()[..8]), "{out}");

    let mut c = clean(&f);
    c.args(["task", "result", &task]);
    let out = ok(c, "task result");
    assert!(out.contains(&v()[..8]), "{out}");
    assert!(!out.contains("[redacted:"), "{out}");
}

/// AC4 (b): an edit's `after` is masked whole before `spec review show`
/// cuts it.
#[test]
fn rd4_spec_review_show_masks_an_edit_before_cutting_it() {
    let f = common::fixture();
    setup(&f);
    let first = format!("AC1: {} {}", "x".repeat(40), v());
    let spec = json!({"goal": "say hello twice", "acceptance_criteria": [first, "AC2: exits zero"],
        "constraints": [], "non_goals": [], "excluded_alternatives": [], "risk": "normal"});
    let task = create(&f, &spec.to_string());
    let mut c = clean(&f);
    c.env("FAKE_SCENARIO", "spec_review_edit")
        .env("FAKE_TASK_ID", &task)
        .args(["spec", "review", "start", &task, "--profile", "claude-b", "--model", "haiku", "--effort", "medium", "--edit", "--json"]);
    let r = ok_json(c, "spec review start");
    wait_run(&f, r["run_id"].as_str().unwrap());

    let edit0 = |c: assert_cmd::Command| {
        let out = ok(c, "spec review show");
        out.lines().find(|l| l.trim_start().starts_with("edit 0 ")).unwrap_or_else(|| panic!("no edit 0: {out}")).to_string()
    };
    let mut c = with_v(&f);
    c.args(["spec", "review", "show", &task]);
    let line = edit0(c);
    assert!(line.contains("[redacted:"), "{line}");
    assert!(!line.contains(&v()[..8]), "{line}");

    let mut c = clean(&f);
    c.args(["spec", "review", "show", &task]);
    let line = edit0(c);
    assert!(line.contains(&v()[..8]), "{line}");
}
