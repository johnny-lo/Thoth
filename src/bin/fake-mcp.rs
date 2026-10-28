//! Test double for the newer `thoth mcp` a running server hands calls to
//! (tests/mcp_replaced.rs).
//!
//! `FAKE_MCP_LOG`, when set, gets one JSON object per line and event, written
//! unbuffered: `{"argv":[...]}` at start (without argument 0),
//! `{"recv_hex":"..."}` for each line read (the bytes before `\n`, `\r`
//! kept), before it is handled, and `{"sent_hex":"..."}` for each line
//! written (without the `\n`), before it is written.
//!
//! When the first argument is not `mcp` (an old server running its CLI with
//! this file at its path), it says so on stderr and exits 2 without reading.
//! Otherwise `FAKE_MCP_MODE_FILE` names a file holding the mode, read once
//! (unset or missing: `ok`), and stdin is read as line-delimited JSON-RPC
//! until EOF, then exit 0. Every answer is written as
//! `{"result": RESULT, "jsonrpc": "2.0", "id": ID}`, a layout re-serializing
//! always changes. `initialize` echoes the protocol version, `tools/list`
//! lists `fake_only_tool`, `ping` answers `{}`, `tools/call` answers
//! `fake-mcp NAME` and a non-ASCII line with what it saw in
//! `structuredContent` (`fake_only_tool` sends `notifications/message`
//! first); other methods are -32601.
//!
//! Modes: `ok`; `exit` (exit 3 before reading); `silent` (reads and logs,
//! never answers); `crash-on-call` and `crash-on-ping` (exit 3 on the first
//! `tools/call` / `ping`, unanswered); `other-version` (initialize answers
//! `1999-01-01`); `unterminated-on-call` (the first `tools/call` answer has
//! no `\n`, then exit 0); `unterminated-on-list` (the first `tools/list`
//! answer has no `\n`, then stdout is closed; stdin is still read and
//! logged until EOF, and nothing more is answered); `exit-after-call` (exit 0
//! after answering the first `tools/call`); `noise` (`fake-mcp noise` on
//! stderr at start, and three stray lines before each `tools/call` answer).

use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// Closes stdout for good; the process goes on reading stdin.
fn close_stdout() {
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd};
        // SAFETY: nothing writes to stdout after this.
        drop(unsafe { std::fs::File::from_raw_fd(std::io::stdout().as_raw_fd()) });
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::{AsRawHandle, FromRawHandle};
        // SAFETY: nothing writes to stdout after this.
        drop(unsafe { std::fs::File::from_raw_handle(std::io::stdout().as_raw_handle()) });
    }
}

fn log(event: Value) {
    let Some(path) = std::env::var_os("FAKE_MCP_LOG") else {
        return;
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("FAKE_MCP_LOG opens");
    f.write_all(format!("{event}\n").as_bytes()).expect("FAKE_MCP_LOG writes");
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn send(line: &str, end: bool) {
    log(json!({"sent_hex": hex(line.as_bytes())}));
    let mut out = std::io::stdout().lock();
    out.write_all(line.as_bytes()).unwrap();
    if end {
        out.write_all(b"\n").unwrap();
    }
    out.flush().unwrap();
}

fn answer(id: &Value, result: &Value) -> String {
    format!("{{\"result\": {result}, \"jsonrpc\": \"2.0\", \"id\": {id}}}")
}

fn main() {
    let argv: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    log(json!({"argv": argv}));
    if argv.first().map(String::as_str) != Some("mcp") {
        eprintln!("fake-mcp: only `mcp` is served, got {argv:?}");
        std::process::exit(2);
    }
    let mode = std::env::var_os("FAKE_MCP_MODE_FILE")
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|| "ok".into());
    if mode == "exit" {
        std::process::exit(3);
    }
    if mode == "noise" {
        eprintln!("fake-mcp noise");
    }
    let mut initialize_params = Value::Null;
    let mut initialized = false;
    let mut first_list_meta = None;
    let mut calls = 0;
    let mut pings = 0;
    let mut closed = false;
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        if input.read_until(b'\n', &mut buf).unwrap() == 0 {
            std::process::exit(0);
        }
        if buf.last() == Some(&b'\n') {
            buf.pop();
        }
        log(json!({"recv_hex": hex(&buf)}));
        if mode == "silent" || closed {
            continue;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&buf) else {
            continue;
        };
        let method = msg["method"].as_str().unwrap_or("");
        let Some(id) = msg.get("id") else {
            if method == "notifications/initialized" {
                initialized = true;
            }
            continue;
        };
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        match method {
            "initialize" => {
                let version = if mode == "other-version" {
                    json!("1999-01-01")
                } else {
                    params["protocolVersion"].clone()
                };
                initialize_params = params;
                send(
                    &answer(
                        id,
                        &json!({"protocolVersion":version,"capabilities":{"tools":{"listChanged":true}},"serverInfo":{"name":"fake-mcp","version":"0.0.0-fake"}}),
                    ),
                    true,
                );
            }
            "tools/list" => {
                let first = first_list_meta.is_none();
                if first {
                    first_list_meta = Some(params.get("_meta").cloned().unwrap_or(Value::Null));
                }
                let tool = json!({"name":"fake_only_tool","description":"Only the fake has it.","inputSchema":{"type":"object"}});
                let line = answer(id, &json!({"tools":[tool]}));
                if mode == "unterminated-on-list" && first {
                    send(&line, false);
                    close_stdout();
                    closed = true;
                } else {
                    send(&line, true);
                }
            }
            "ping" => {
                pings += 1;
                if mode == "crash-on-ping" && pings == 1 {
                    std::process::exit(3);
                }
                send(&answer(id, &json!({})), true);
            }
            "tools/call" => {
                calls += 1;
                if mode == "crash-on-call" && calls == 1 {
                    std::process::exit(3);
                }
                if mode == "noise" {
                    for stray in [
                        "not json",
                        r#"{"jsonrpc":"2.0","id":"x-1","method":"roots/list"}"#,
                        r#"{"jsonrpc":"2.0","id":"x-2","result":{}}"#,
                    ] {
                        send(stray, true);
                    }
                }
                let name = params["name"].as_str().unwrap_or("");
                if name == "fake_only_tool" {
                    send(
                        r#"{"params": {"tool": "fake_only_tool"}, "method": "notifications/message", "jsonrpc": "2.0"}"#,
                        true,
                    );
                }
                let cwd = std::env::current_dir()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();
                let result = json!({
                    "content": [
                        {"type":"text","text":format!("fake-mcp {name}")},
                        {"type":"text","text":"ünïcödé ✓"},
                    ],
                    "structuredContent": {
                        "fake_mcp": true,
                        "tool": name,
                        "argv": argv,
                        "cwd": cwd,
                        "thoth_home": std::env::var("THOTH_HOME").ok(),
                        "initialize_params": initialize_params,
                        "initialized": initialized,
                        "first_list_meta": first_list_meta.clone().unwrap_or(Value::Null),
                    },
                    "isError": false,
                });
                let first = calls == 1;
                if mode == "unterminated-on-call" && first {
                    send(&answer(id, &result), false);
                    std::process::exit(0);
                }
                send(&answer(id, &result), true);
                if mode == "exit-after-call" && first {
                    std::process::exit(0);
                }
            }
            _ => send(
                &format!(
                    "{{\"error\": {{\"code\": -32601, \"message\": \"Method not found\"}}, \"jsonrpc\": \"2.0\", \"id\": {id}}}"
                ),
                true,
            ),
        }
    }
}
