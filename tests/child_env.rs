//! SMALL-13: no variable of the operator's own environment reaches a thoth
//! child started by a test. `common::OPERATOR_ENV` lists them;
//! `common::scrubbed` and `common::scrubbed_assert` remove them.
//!
//! The outer test starts this very test binary with the variables set (to
//! dummy values, for that child only) and runs the one `#[ignore]`d inner test
//! there. The inner test means something only as that child. Neither test
//! prints an environment value.

mod common;

use serde_json::{json, Value};
use std::io::Write;
use std::process::{Command, Stdio};

const INNER: &str = "inner_children_see_none_of_the_operator_variables";

#[test]
fn operator_variables_in_the_test_environment_reach_no_thoth_child() {
    let tmp = tempfile::tempdir().unwrap();
    // Values built at run time, never shaped like a real key.
    let n = std::process::id();
    let out = Command::new(std::env::current_exe().unwrap())
        .args([INNER, "--exact", "--ignored", "--nocapture"])
        .env("AISTUPIDLEVEL_API_KEY", format!("dummy-{n}"))
        .env("ARTIFICIALANALYSIS_API_KEY", format!("dummy-{}", n + 1))
        .env("CLAUDE_PLUGIN_ROOT", tmp.path().join("no-such-plugin-root"))
        .env("THOTH_RECORDS_DIR", tmp.path().join("no-such-records"))
        .env("THOTH_MACHINE_NAME", "leak-check")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "the inner test failed:\n{stdout}\n{stderr}");
    assert!(stdout.contains("1 passed"), "the inner test did not run once:\n{stdout}");
}

fn setting<'a>(list: &'a Value, key: &str) -> &'a Value {
    list["settings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["key"] == key)
        .unwrap_or_else(|| panic!("config list has no setting {key}"))
}

/// What `config list --json` must show when none of the variables arrived.
/// Only keys and sources are named in a failure, never a value.
fn assert_none_arrived(list: &Value, via: &str) {
    for key in ["aistupidlevel-api-key", "artificialanalysis-api-key"] {
        assert_eq!(setting(list, key)["value"], "unset", "{via}: {key} is not unset");
    }
    for key in ["records-dir", "machine-name"] {
        assert_ne!(setting(list, key)["source"], "env", "{via}: {key} came from the environment");
    }
}

#[test]
#[ignore = "runs only as the child of operator_variables_in_the_test_environment_reach_no_thoth_child"]
fn inner_children_see_none_of_the_operator_variables() {
    // The variables are set here, so the checks below mean something.
    for var in ["AISTUPIDLEVEL_API_KEY", "ARTIFICIALANALYSIS_API_KEY", "CLAUDE_PLUGIN_ROOT", "THOTH_RECORDS_DIR", "THOTH_MACHINE_NAME"] {
        assert!(std::env::var_os(var).is_some(), "{var} is not set in this child: run the outer test");
    }
    let f = common::fixture();

    // (a) config list --json, through Fixture::cmd and through a bare command.
    let out = f.cmd().args(["config", "list", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_none_arrived(&serde_json::from_slice(&out.stdout).unwrap(), "Fixture::cmd");

    let bare = || {
        let mut c = common::scrubbed(Command::new(env!("CARGO_BIN_EXE_thoth")));
        c.current_dir(f.repo.path())
            .env("THOTH_HOME", f.home.path())
            .env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"))
            .env("THOTH_CODEX_BIN", env!("CARGO_BIN_EXE_fake-codex"))
            .env("THOTH_AGY_BIN", env!("CARGO_BIN_EXE_fake-agy"));
        c
    };
    let out = bare().args(["config", "list", "--json"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert_none_arrived(&serde_json::from_slice(&out.stdout).unwrap(), "bare command");

    // (b) thoth mcp, started the bare way: initialize, then one state-changing
    // tool call that a plugin mismatch would refuse.
    let mut child = bare().arg("mcp").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    {
        let mut input = child.stdin.take().unwrap();
        for msg in [
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}),
            json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"thoth_init","arguments":{}}}),
        ] {
            writeln!(input, "{msg}").unwrap();
        }
    }
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let answer = stdout.lines().nth(1).expect("an answer to the tools/call");
    assert!(!answer.contains("plugin_mismatch"), "the call was refused with plugin_mismatch");
    let answer: Value = serde_json::from_str(answer).unwrap();
    assert_ne!(answer["result"]["isError"], true, "thoth_init came back as an error");
    assert!(answer["result"].is_object(), "no result in the answer");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("does not match this thoth executable"), "thoth mcp warned about a plugin mismatch");
}
