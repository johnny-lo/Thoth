mod common;

use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

fn guide_cmd(home: &std::path::Path, cwd: &std::path::Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
    c.current_dir(cwd)
        .env("THOTH_HOME", home)
        .env_remove("THOTH_RUN_ID")
        .env_remove("THOTH_INBOX_DIR")
        .env_remove("THOTH_EXE");
    c
}

fn plain_guide() -> String {
    let empty = tempfile::tempdir().unwrap();
    let out = guide_cmd(&empty.path().join("no-such-home"), empty.path())
        .arg("guide")
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn ac3b_guide_works_without_home_or_repo_and_is_clean() {
    let empty = tempfile::tempdir().unwrap();
    let home = empty.path().join("does").join("not").join("exist");
    let out = guide_cmd(&home, empty.path()).arg("guide").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    let first = text.lines().find(|l| !l.trim().is_empty()).unwrap();
    assert!(first.starts_with("# "), "first line: {first}");
    assert!(!text
        .lines()
        .any(|l| l.starts_with("name:") || l.starts_with("description:")));
    assert!(!home.exists(), "guide must not create the state directory");
}

#[test]
fn ac3c_json_guide_equals_plain_guide() {
    let empty = tempfile::tempdir().unwrap();
    let out = guide_cmd(&empty.path().join("h"), empty.path())
        .args(["guide", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let doc: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(doc["ok"], true);
    assert_eq!(doc["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(doc["guide"].as_str().unwrap().trim_end(), plain_guide().trim_end());
}

/// RJ-3: the guide is the plugin's skill as it is now (it has no comment
/// lines), so the commander reads the choose-yourself rules from either.
#[test]
fn the_guide_is_the_current_plugin_skill() {
    let skill = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/plugins/thoth/skills/thoth-dispatch/SKILL.md"
    ))
    .unwrap()
    .replace("\r\n", "\n");
    let body = skill.strip_prefix("---\n").unwrap();
    let body = &body[body.find("\n---\n").unwrap() + "\n---\n".len()..];
    let guide = plain_guide();
    assert_eq!(guide.trim_end().replace("\r\n", "\n"), body.trim());
    assert!(guide.contains("--reason \"...\""));
    assert!(guide.contains("Thoth does not choose a profile, model or effort for you."));
    assert!(guide.contains("never wait for some profile's quota to reset"));
    for gone in ["--profile auto", "recommended_launch", "handover", "gemini-3.1-pro"] {
        assert!(!guide.contains(gone), "{gone}");
    }
}

#[test]
fn ac1_help_lists_guide() {
    let out = Command::new(env!("CARGO_BIN_EXE_thoth"))
        .arg("--help")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|l| l.trim_start().starts_with("guide")));
}

#[test]
fn ac3c_mcp_thoth_guide_matches_the_cli_document() {
    let f = common::fixture();
    let mut child = guide_cmd(f.home.path(), f.repo.path())
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    let mut send = |v: Value| -> Value {
        writeln!(input, "{v}").unwrap();
        input.flush().unwrap();
        let mut s = String::new();
        output.read_line(&mut s).unwrap();
        serde_json::from_str(&s).unwrap()
    };
    let list = send(json!({"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}));
    let tools = list["result"]["tools"].as_array().unwrap();
    let last = tools.last().unwrap();
    assert_eq!(last["name"], "thoth_guide");
    assert_eq!(last["annotations"]["readOnlyHint"], true);
    assert_eq!(last["inputSchema"]["properties"], json!({}));
    let call = send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"thoth_guide","arguments":{}}}));
    assert_eq!(call["result"]["isError"], false);
    let structured = call["result"]["structuredContent"].clone();
    let cli = guide_cmd(f.home.path(), f.repo.path())
        .args(["guide", "--json"])
        .output()
        .unwrap();
    let cli: Value = serde_json::from_slice(&cli.stdout).unwrap();
    assert_eq!(structured, cli);
    assert_eq!(structured["version"], env!("CARGO_PKG_VERSION"));
    let init = send(json!({"jsonrpc":"2.0","id":3,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}));
    assert!(init["result"]["instructions"]
        .as_str()
        .unwrap()
        .starts_with("Before first use, call thoth_guide"));
    let _ = child.kill();
    let _ = child.wait();
}
