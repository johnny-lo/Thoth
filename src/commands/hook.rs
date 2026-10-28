//! `thoth hook` — the out-of-band evidence receiver (v6 §2.2, brief §3).
//!
//! Claude Code invokes this once per `Bash` `PreToolUse` / `PostToolUse` /
//! `PostToolUseFailure` with the event payload on stdin. Four hard rules, all
//! of them Global Constraints:
//!
//! * **exit 0, always.** A non-zero hook can change what the worker sees; the
//!   control plane's bookkeeping must never be able to do that.
//! * **never print to stdout.** Claude Code parses a hook's stdout as
//!   `hookSpecificOutput`; anything printed here would be interpreted as a
//!   decision about the tool call.
//! * **never open a database.** This runs on the worker's critical path, once
//!   per shell command. The runner is the single writer (v6 §10.3); this
//!   process only appends to a file.
//! * **no network.** Same reason.
//!
//! Target cost is well under 50 ms: one JSON parse, one file append, and - only
//! when the payload carries a `persistedOutputPath` - one streamed file copy.

use crate::evidence::HookBlob;
use crate::ids;
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// brief §3 / Open question 8: an inbox line may not exceed 8 MiB. Only an
/// inline `stdout` can get near that, and when it does the full bytes are in
/// the blob anyway.
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;

pub const HOOKS_FILE: &str = "hooks.jsonl";
pub const ERRORS_FILE: &str = "hook-errors.log";
pub const BLOBS_DIR: &str = "blobs";

/// The hook's only diagnostic channel: stdout belongs to Claude Code and a
/// detached runner has no stderr a human will ever read, so anything worth
/// saying goes into a file beside the inbox. Never fails loudly — a logging
/// failure must not compound whatever it was reporting.
pub fn log_hook_error(inbox_dir: &Path, msg: &str) {
    if std::fs::create_dir_all(inbox_dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(inbox_dir.join(ERRORS_FILE))
    {
        let _ = writeln!(f, "[{}] {msg}", ids::now_rfc3339());
    }
}

/// Serializes one inbox record: `{received_at, event, payload}` plus `blob`
/// when a persisted output was copied, plus `truncated_by_hook` when the 8 MiB
/// cap fired.
pub fn record_line(
    received_at: &str,
    event: &str,
    payload: serde_json::Value,
    blob: Option<&HookBlob>,
) -> String {
    let build = |p: serde_json::Value, truncated: bool| -> serde_json::Value {
        let mut v = serde_json::json!({
            "received_at": received_at,
            "event": event,
            "payload": p,
        });
        if let Some(b) = blob {
            v["blob"] = serde_json::json!({
                "sha256": b.sha256,
                "bytes": b.bytes,
                "source": b.source,
            });
        }
        if truncated {
            v["truncated_by_hook"] = serde_json::Value::Bool(true);
        }
        v
    };

    let text = build(payload.clone(), false).to_string();
    if text.len() <= MAX_LINE_BYTES {
        return text;
    }
    let mut trimmed = payload;
    if let Some(resp) = trimmed.get_mut("tool_response") {
        if resp.get("stdout").is_some() {
            resp["stdout"] = serde_json::Value::String(String::new());
        }
    }
    build(trimmed, true).to_string()
}

/// The whole receiver, minus the environment and stdin so a test can drive it.
pub fn process_payload(raw: &str, inbox_dir: &Path, received_at: &str) -> Result<()> {
    std::fs::create_dir_all(inbox_dir)
        .with_context(|| format!("creating {}", inbox_dir.display()))?;
    let payload: serde_json::Value =
        serde_json::from_str(raw.trim()).context("hook stdin was not valid JSON")?;
    let event = payload["hook_event_name"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();

    // brief §3 and Phase 0 02-evidence-capture.md:53: the persisted output file
    // belongs to Claude Code's session lifecycle, so it has to be copied *now*.
    // Reading it a second later is not guaranteed to work.
    let mut blob: Option<HookBlob> = None;
    if let Some(p) = payload["tool_response"]["persistedOutputPath"].as_str() {
        let src = PathBuf::from(p);
        if src.is_file() {
            match crate::blobs::put_file_into(&inbox_dir.join(BLOBS_DIR), &src) {
                Ok((sha256, bytes, _)) => {
                    blob = Some(HookBlob {
                        sha256,
                        bytes: bytes as i64,
                        source: "persisted".to_string(),
                    })
                }
                Err(e) => log_hook_error(inbox_dir, &format!("copying {p} failed: {e:#}")),
            }
        } else {
            log_hook_error(
                inbox_dir,
                &format!(
                    "persistedOutputPath {p} is not a readable file; the runner will fall back \
                     to the inline stdout"
                ),
            );
        }
    }

    let line = record_line(received_at, &event, payload, blob.as_ref());
    let target = inbox_dir.join(HOOKS_FILE);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)
        .with_context(|| format!("opening {}", target.display()))?;
    // One `write_all` for the record *and* its newline. The runner's tailer
    // only consumes up to the last '\n' it can see, so a concurrent read can
    // never observe half a record.
    let mut bytes = line.into_bytes();
    bytes.push(b'\n');
    f.write_all(&bytes)
        .with_context(|| format!("appending to {}", target.display()))?;
    Ok(())
}

/// The subcommand. Returns 0 unconditionally; see the module doc.
pub fn run() -> i32 {
    // Read bytes, not a `String`: a payload carries arbitrary command output,
    // and `read_to_string` would reject the whole thing over one bad byte (the
    // same lesson as the runner's stdout reader).
    let mut buf: Vec<u8> = Vec::new();
    if std::io::stdin().read_to_end(&mut buf).is_err() {
        return 0;
    }
    let raw = String::from_utf8_lossy(&buf).into_owned();

    let inbox = match std::env::var("THOTH_INBOX_DIR") {
        Ok(v) if !v.trim().is_empty() => PathBuf::from(v),
        // Without an inbox there is nowhere to write and nowhere to complain.
        _ => return 0,
    };
    if std::env::var("THOTH_RUN_ID")
        .map(|v| v.trim().is_empty())
        .unwrap_or(true)
    {
        log_hook_error(
            &inbox,
            "THOTH_RUN_ID is unset or empty; this payload was not recorded",
        );
        return 0;
    }
    if let Err(e) = process_payload(&raw, &inbox, &ids::now_rfc3339()) {
        log_hook_error(&inbox, &format!("{e:#}"));
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/hooks-sample.jsonl"
    ));

    fn lines(s: &str) -> Vec<&str> {
        s.lines().filter(|l| !l.trim().is_empty()).collect()
    }

    fn recorded(inbox: &Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(inbox.join(HOOKS_FILE)).unwrap();
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("every inbox line must be JSON"))
            .collect()
    }

    #[test]
    fn the_fixture_has_the_three_real_event_kinds_we_hook() {
        let ls = lines(SAMPLE);
        assert_eq!(ls.len(), 6, "three Bash calls, one Pre and one Post each");
        let events: Vec<String> = ls
            .iter()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["hook_event_name"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(
            events,
            vec![
                "PreToolUse",
                "PostToolUse",
                "PreToolUse",
                "PostToolUseFailure",
                "PreToolUse",
                "PostToolUse",
            ]
        );
    }

    #[test]
    fn every_fixture_line_becomes_one_inbox_line() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        for (i, l) in lines(SAMPLE).iter().enumerate() {
            process_payload(l, &inbox, &format!("2026-09-16T00:00:0{i}Z")).unwrap();
        }
        let rows = recorded(&inbox);
        assert_eq!(rows.len(), 6);
        assert_eq!(rows[0]["event"], "PreToolUse");
        assert_eq!(rows[0]["received_at"], "2026-09-16T00:00:00Z");
        assert_eq!(rows[0]["payload"]["tool_input"]["command"], "echo hello-stdout");
        assert_eq!(rows[1]["payload"]["tool_response"]["stdout"], "hello-stdout");
        assert_eq!(rows[1]["payload"]["duration_ms"], 758);
        assert_eq!(rows[3]["event"], "PostToolUseFailure");
        assert_eq!(rows[3]["payload"]["error"], "Exit code 3\noops-stderr");

        // The payload is passed through byte-for-byte; the hook adds fields
        // beside it and never edits it (unless the 8 MiB cap fires).
        let original: serde_json::Value = serde_json::from_str(lines(SAMPLE)[3]).unwrap();
        assert_eq!(rows[3]["payload"], original);

        // Line 6 names a `persistedOutputPath` that no longer exists (Claude
        // Code owns that file's lifetime and the Phase 0 session is long
        // gone), so no blob is created and the fallback is recorded instead of
        // the hook failing.
        assert!(rows[5].get("blob").is_none());
        assert!(inbox.join(ERRORS_FILE).is_file());
        let err = std::fs::read_to_string(inbox.join(ERRORS_FILE)).unwrap();
        assert!(err.contains("persistedOutputPath"), "got: {err}");
    }

    #[test]
    fn a_readable_persisted_output_path_becomes_an_inbox_blob() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        let persisted = dir.path().join("tool-results").join("abc123.txt");
        std::fs::create_dir_all(persisted.parent().unwrap()).unwrap();
        let body: String = (1..=20000).map(|n| format!("{n}\n")).collect();
        assert_eq!(body.len(), 108_894);
        std::fs::write(&persisted, &body).unwrap();

        let payload = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "session_id": "s-1",
            "cwd": "C:/wt",
            "tool_use_id": "toolu_x",
            "tool_input": {"command": "seq 1 20000"},
            "tool_response": {
                "stdout": "1\n2\n3\n",
                "stderr": "",
                "persistedOutputPath": persisted.display().to_string(),
                "persistedOutputSize": 108_894,
            },
            "duration_ms": 83,
        });
        process_payload(&payload.to_string(), &inbox, "2026-09-16T00:00:00Z").unwrap();

        let rows = recorded(&inbox);
        assert_eq!(rows.len(), 1);
        let blob = &rows[0]["blob"];
        assert_eq!(blob["bytes"], 108_894);
        assert_eq!(blob["source"], "persisted");
        let sha = blob["sha256"].as_str().unwrap();
        assert_eq!(sha.len(), 64);
        let stored = inbox.join(BLOBS_DIR).join(sha);
        assert!(stored.is_file(), "the blob must be copied at hook time");
        assert_eq!(std::fs::read(&stored).unwrap().len(), 108_894);
        assert!(crate::blobs::verify(&stored, sha));
    }

    #[test]
    fn a_line_over_the_cap_drops_the_inline_stdout_and_says_so() {
        let huge = "x".repeat(MAX_LINE_BYTES + 1024);
        let payload = serde_json::json!({
            "hook_event_name": "PostToolUse",
            "tool_response": {"stdout": huge, "stderr": ""},
        });
        let line = record_line("2026-09-16T00:00:00Z", "PostToolUse", payload, None);
        assert!(
            line.len() <= MAX_LINE_BYTES,
            "capped line is still {} bytes",
            line.len()
        );
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["truncated_by_hook"], true);
        assert_eq!(v["payload"]["tool_response"]["stdout"], "");
        assert_eq!(v["payload"]["tool_response"]["stderr"], "");

        // a small line keeps everything and carries no flag
        let small = serde_json::json!({"hook_event_name": "PostToolUse", "tool_response": {"stdout": "ok"}});
        let line = record_line("2026-09-16T00:00:00Z", "PostToolUse", small, None);
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("truncated_by_hook").is_none());
        assert_eq!(v["payload"]["tool_response"]["stdout"], "ok");
    }

    #[test]
    fn a_blob_is_reported_on_the_line_when_one_was_stored() {
        let blob = crate::evidence::HookBlob {
            sha256: "abc".into(),
            bytes: 7,
            source: "persisted".into(),
        };
        let line = record_line(
            "2026-09-16T00:00:00Z",
            "PostToolUse",
            serde_json::json!({"hook_event_name": "PostToolUse"}),
            Some(&blob),
        );
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["blob"]["sha256"], "abc");
        assert_eq!(v["blob"]["bytes"], 7);
        assert_eq!(v["blob"]["source"], "persisted");
    }

    #[test]
    fn an_unwritable_inbox_dir_fails_process_payload_and_never_panics_when_logging() {
        // A plain file sitting where the inbox directory would go: `create_dir_all`
        // cannot turn a file into a directory, so this stands in for a permission
        // failure without needing platform-specific ACL manipulation.
        let dir = tempfile::tempdir().unwrap();
        let blocked = dir.path().join("blocked-inbox");
        std::fs::write(&blocked, b"not a directory").unwrap();

        let err = process_payload(
            r#"{"hook_event_name":"PostToolUse"}"#,
            &blocked,
            "2026-09-16T00:00:00Z",
        )
        .expect_err("an inbox dir that cannot be created must be an error the caller logs");
        assert!(
            format!("{err:#}").contains("blocked-inbox"),
            "got: {err:#}"
        );

        // Global Constraint: logging the failure must itself never panic, even
        // though it will fail to create the very same directory.
        log_hook_error(&blocked, "should be swallowed, not panic");
        assert!(
            !blocked.join(ERRORS_FILE).exists(),
            "no errors log can exist under a path that is a file, not a directory"
        );
    }

    #[test]
    fn garbage_on_stdin_is_logged_and_never_lands_in_the_inbox() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        let err = process_payload("half a line {", &inbox, "2026-09-16T00:00:00Z")
            .expect_err("unparsable stdin must be an error the caller logs");
        assert!(format!("{err:#}").contains("JSON"), "got: {err:#}");
        assert!(
            !inbox.join(HOOKS_FILE).exists(),
            "a rejected payload must not append a line"
        );

        // and the caller's own logging path works on the same directory
        log_hook_error(&inbox, "something went wrong");
        let text = std::fs::read_to_string(inbox.join(ERRORS_FILE)).unwrap();
        assert!(text.contains("something went wrong"));
    }
}
