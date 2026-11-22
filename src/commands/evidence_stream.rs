//! EVOPEN-1: which evidence a Codex or agy reviewer read, found in its own
//! stream.
//!
//! A Codex or agy reviewer cannot run `thoth evidence show`; it reads the
//! files Thoth put in `.agent/review-input/evidence/` instead. When its
//! review is taken in, the runner reads the run's own stream.jsonl, finds the
//! reads of `.agent/review-input/evidence/<CODE>.txt` in the run's own
//! worktree, and appends one `evidence_opened` event per code (detail
//! `found_in: "reviewer_stream"`), so `review show`'s `opened` and
//! `unopened_citation` mean for these harnesses what they mean for Claude.
//!
//! The matching is deliberately literal: a read is counted only when the
//! command (Codex) or the tool's path parameter (agy) names the file itself,
//! relative to the worktree (`.agent/...` or `./.agent/...`) or as the
//! worktree's absolute path followed by that. Globs, a change of directory
//! and variables are not resolved, so a read may be missed; and since a
//! Codex `command_execution` has no working directory, a relative path after
//! a change of directory (`cd /tmp && cat .agent/review-input/evidence/E001.txt`)
//! may be counted as a read of the run's own `E001.txt`.
//! `packet.md` and `result.json` open nothing.

use crate::db::{self, state::{self, NewEvent}};
use crate::model::event_type;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::Path;

/// The `found_in` value of an `evidence_opened` event written here; an
/// event without `found_in` is an `evidence show` call.
pub const FOUND_IN: &str = "reviewer_stream";

/// The folder each code's file is in, relative to the worktree.
const FOLDER: &str = ".agent/review-input/evidence/";

/// The harnesses whose reviewer reads files rather than running thoth.
pub fn reads_from_files(harness: &str) -> bool {
    harness == crate::adapter::HARNESS_CODEX || harness == crate::adapter::antigravity::HARNESS_NAME
}

/// Where in the stream a read was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// A Codex `command_execution` item's id.
    Item(String),
    /// An agy tool step's `step_index`.
    Step(i64),
}

/// One code's first read in a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Read {
    pub code: String,
    pub place: Place,
}

/// Can a name start right after `c`?
fn starts_after(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | ';' | '&' | '|' | '(' | ',')
}

/// Can a name end right before `c`?
fn ends_before(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\'' | '"' | ';' | '&' | '|' | ')' | ',')
}

/// Does a name start at the end of `before` (the start of the text, or a
/// character a name may follow)?
fn boundary(before: &str) -> bool {
    before.chars().next_back().is_none_or(starts_after)
}

/// `text` ends with `suffix`, ignoring ASCII letter case when `fold`.
fn strip_suffix_folded<'a>(text: &'a str, suffix: &str, fold: bool) -> Option<&'a str> {
    let cut = text.len().checked_sub(suffix.len())?;
    if !text.is_char_boundary(cut) {
        return None;
    }
    let (head, tail) = text.split_at(cut);
    let same = if fold { tail.eq_ignore_ascii_case(suffix) } else { tail == suffix };
    same.then_some(head)
}

/// The codes whose file `text` names, in order of appearance (a code may
/// repeat). `worktrees` are the run's worktree spellings; on `windows` `\`
/// and `/` are alike and the worktree part ignores letter case.
pub fn codes_named(text: &str, worktrees: &[String], windows: bool) -> Vec<String> {
    // `\` and `/` are one byte each, so indices stay aligned.
    let text = if windows { text.replace('\\', "/") } else { text.to_string() };
    let worktrees: Vec<String> = worktrees
        .iter()
        .map(|w| if windows { w.replace('\\', "/") } else { w.clone() })
        .map(|w| w.trim_end_matches('/').to_string())
        .filter(|w| !w.is_empty())
        .collect();
    let mut out = Vec::new();
    for (at, _) in text.match_indices(FOLDER) {
        let rest = &text[at + FOLDER.len()..];
        let len = rest.bytes().take_while(u8::is_ascii_alphanumeric).count();
        if len == 0 {
            continue;
        }
        let Some(tail) = rest[len..].strip_prefix(".txt") else { continue };
        if !tail.chars().next().is_none_or(ends_before) {
            continue;
        }
        let before = &text[..at];
        let relative = boundary(before) || before.strip_suffix("./").is_some_and(boundary);
        let absolute = || {
            worktrees.iter().any(|w| {
                before
                    .strip_suffix('/')
                    .and_then(|b| strip_suffix_folded(b, w, windows))
                    .is_some_and(boundary)
            })
        };
        if relative || absolute() {
            out.push(rest[..len].to_string());
        }
    }
    out
}

/// The names one stream line reads, with where: a completed Codex
/// `command_execution` item's `command` (whatever its exit code), or a DONE
/// agy tool step of `read_file` (`Path`) or `view_file` (`AbsolutePath`)
/// that did not end in `ERROR`. Anything else is `None`.
fn read_in_line(harness: &str, line: &str) -> Option<(String, Place)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    if harness == crate::adapter::HARNESS_CODEX {
        let item = &v["item"];
        if v["type"] != "item.completed" || item["type"] != "command_execution" {
            return None;
        }
        let id = item["id"].as_str().unwrap_or_default().to_string();
        return Some((item["command"].as_str()?.to_string(), Place::Item(id)));
    }
    if v["event"] != "step_update" {
        return None;
    }
    let step = &v["step_update"];
    let info = &step["tool_info"];
    if step["state"] != "DONE" || step["step_type"] != "tool" || info["result"] == "ERROR" {
        return None;
    }
    let tool = step["tool_name"].as_str().or_else(|| info["name"].as_str())?;
    let key = match tool {
        "read_file" => "Path",
        "view_file" => "AbsolutePath",
        _ => return None,
    };
    let path = info["parameters"][key].as_str()?.to_string();
    Some((path, Place::Step(step["step_index"].as_i64()?)))
}

/// Every code of `codes` the stream `text` read, once each, in the order of
/// its first read.
pub fn reads_in_stream(
    harness: &str,
    text: &str,
    worktrees: &[String],
    codes: &[String],
    windows: bool,
) -> Vec<Read> {
    let mut out: Vec<Read> = Vec::new();
    for line in text.lines() {
        let Some((named, place)) = read_in_line(harness, line) else { continue };
        for code in codes_named(&named, worktrees, windows) {
            if codes.contains(&code) && !out.iter().any(|r| r.code == code) {
                out.push(Read { code, place: place.clone() });
            }
        }
    }
    out
}

/// The spellings of `worktree` a stream may use: as stored, and its
/// canonical form (without Windows' `\\?\`) when that differs.
fn worktree_spellings(worktree: &str) -> Vec<String> {
    let mut out = vec![worktree.to_string()];
    if let Ok(canonical) = std::fs::canonicalize(worktree) {
        let canonical = canonical.display().to_string();
        let canonical = canonical.strip_prefix(r"\\?\").unwrap_or(&canonical).to_string();
        if !out.contains(&canonical) {
            out.push(canonical);
        }
    }
    out
}

/// The run's `evidence_opened` codes and, of those, the ones found in its
/// stream; each list in event order, without repeats.
pub fn opened_split(conn: &Connection, run_id: &str) -> Result<(Vec<String>, Vec<String>)> {
    let mut opened: Vec<String> = Vec::new();
    let mut in_stream: Vec<String> = Vec::new();
    for d in state::event_details_of_run(conn, run_id, event_type::EVIDENCE_OPENED)? {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&d) else { continue };
        let Some(code) = v["code"].as_str() else { continue };
        if !opened.iter().any(|c| c == code) {
            opened.push(code.to_string());
        }
        if v["found_in"] == FOUND_IN && !in_stream.iter().any(|c| c == code) {
            in_stream.push(code.to_string());
        }
    }
    Ok((opened, in_stream))
}

/// Appends one `evidence_opened` event per code of the run's task that the
/// reviewer run's stream (`stream_path`) read and the run has not opened
/// yet. Returns how many were appended; nothing for a harness that runs
/// thoth itself, or a stream that cannot be read.
pub fn record(conn: &Connection, run: &state::Run, harness: &str, stream_path: &Path) -> Result<usize> {
    if !reads_from_files(harness) {
        return Ok(0);
    }
    let Ok(text) = std::fs::read_to_string(stream_path) else { return Ok(0) };
    let kinds: HashMap<String, String> = state::list_evidence_for_task(conn, &run.task_id)?
        .into_iter()
        .map(|e| (e.code, e.kind))
        .collect();
    let codes: Vec<String> = kinds.keys().cloned().collect();
    let (already, _) = opened_split(conn, &run.run_id)?;
    let reads: Vec<Read> =
        reads_in_stream(harness, &text, &worktree_spellings(&run.worktree), &codes, cfg!(windows))
            .into_iter()
            .filter(|r| !already.contains(&r.code))
            .collect();
    if reads.is_empty() {
        return Ok(0);
    }
    let tx = db::write_tx(conn)?;
    for read in &reads {
        let mut detail = serde_json::json!({
            "code": read.code,
            "kind": kinds.get(&read.code),
            "found_in": FOUND_IN,
            "harness": harness,
            "path": format!("{FOLDER}{}.txt", read.code),
        });
        match &read.place {
            Place::Item(id) => detail["item_id"] = serde_json::json!(id),
            Place::Step(index) => detail["step_index"] = serde_json::json!(index),
        }
        state::append_event(
            &tx,
            &NewEvent {
                task_id: Some(run.task_id.clone()),
                run_id: Some(run.run_id.clone()),
                event_type: event_type::EVIDENCE_OPENED.to_string(),
                detail,
            },
        )?;
    }
    tx.commit()?;
    Ok(reads.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const WT: &str = "/home/u/.thoth/worktrees/T-a1-review";

    fn named(text: &str, windows: bool) -> Vec<String> {
        codes_named(text, &[WT.to_string()], windows)
    }

    fn codex_line(id: &str, command: &str, kind: &str) -> String {
        serde_json::json!({"type": kind, "item": {"id": id, "type": "command_execution",
            "command": command, "aggregated_output": "", "exit_code": 1, "status": "failed"}})
        .to_string()
    }

    fn agy_line(index: i64, tool: &str, key: &str, path: &str, state: &str, error: bool) -> String {
        let mut info = serde_json::json!({"name": tool, "parameters": {key: path}});
        if error {
            info["result"] = "ERROR".into();
        }
        serde_json::json!({"event": "step_update", "step_update": {"conversation_id": "c",
            "step_index": index, "state": state, "step_type": "tool", "tool_name": tool,
            "tool_info": info}})
        .to_string()
    }

    fn codes() -> Vec<String> {
        ["E001", "E002", "E003"].iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_relative_dot_or_absolute_name_counts_inside_quotes_and_the_shell_wrapper() {
        assert_eq!(named("/bin/bash -lc 'cat .agent/review-input/evidence/E001.txt'", false), ["E001"]);
        assert_eq!(named("cat ./.agent/review-input/evidence/E001.txt", false), ["E001"]);
        assert_eq!(named(&format!("cat \"{WT}/.agent/review-input/evidence/E002.txt\""), false), ["E002"]);
        assert_eq!(named(".agent/review-input/evidence/E003.txt", false), ["E003"]);
        assert_eq!(named(&format!("{WT}/.agent/review-input/evidence/E003.txt"), false), ["E003"]);
    }

    #[test]
    fn several_names_in_one_command_each_count() {
        assert_eq!(
            named("cat .agent/review-input/evidence/E001.txt .agent/review-input/evidence/E002.txt", false),
            ["E001", "E002"]
        );
        assert_eq!(
            named("cat .agent/review-input/evidence/E002.txt&&cat .agent/review-input/evidence/E001.txt", false),
            ["E002", "E001"]
        );
        assert_eq!(
            named(r"Get-Content .agent\review-input\evidence\E001.txt, .agent\review-input\evidence\E003.txt", true),
            ["E001", "E003"]
        );
        assert_eq!(named("(cat .agent/review-input/evidence/E001.txt)|wc", false), ["E001"]);
    }

    #[test]
    fn a_name_must_end_at_txt_and_start_at_a_boundary() {
        for text in [
            "cat .agent/review-input/evidence/E001.txt.bak",
            "cat .agent/review-input/evidence/E001.txtx",
            "cat .agent/review-input/evidence/*.txt",
            "cat .agent/review-input/evidence/E00?.txt",
            "cat .agent/review-input/evidence/",
            "ls .agent/review-input/evidence",
            "cd .agent/review-input/evidence && cat E001.txt",
            "cat src/.agent/review-input/evidence/E001.txt",
            "cat $D/.agent/review-input/evidence/E001.txt",
            "cat /tmp/other/.agent/review-input/evidence/E001.txt",
            "cat x./.agent/review-input/evidence/E001.txt",
            "cat .agent/review-input/packet.md .agent/review-input/result.json",
        ] {
            assert!(named(text, false).is_empty(), "{text}");
        }
        // `E0011` is a name, but not E001's
        assert_eq!(named("cat .agent/review-input/evidence/E0011.txt", false), ["E0011"]);
    }

    #[test]
    fn backslashes_and_letter_case_of_the_worktree_count_only_on_windows() {
        let win_wt = r"C:\Users\u\.thoth\worktrees\T-a1-review".to_string();
        let back = r"cat .agent\review-input\evidence\E001.txt";
        assert_eq!(codes_named(back, &[], true), ["E001"]);
        assert!(codes_named(back, &[], false).is_empty());
        let upper = r"type C:\USERS\U\.thoth\worktrees\T-A1-REVIEW\.agent\review-input\evidence\E002.txt";
        assert_eq!(codes_named(upper, std::slice::from_ref(&win_wt), true), ["E002"]);
        let mixed = "cat C:/Users/u/.thoth/worktrees/T-a1-review/.agent/review-input/evidence/E002.txt";
        assert_eq!(codes_named(mixed, std::slice::from_ref(&win_wt), true), ["E002"]);
        // elsewhere the worktree part keeps its case
        let other_case = WT.to_uppercase();
        let text = format!("cat {other_case}/.agent/review-input/evidence/E001.txt");
        assert!(named(&text, false).is_empty());
        // and the relative part keeps its case on Windows too
        assert!(codes_named(r"cat .AGENT\review-input\evidence\E001.txt", &[], true).is_empty());
    }

    #[test]
    fn a_codex_stream_counts_completed_items_only_once_per_code_in_first_read_order() {
        let stream = [
            codex_line("item_1", "/bin/bash -lc 'cat .agent/review-input/evidence/E003.txt'", "item.started"),
            codex_line("item_2", "/bin/bash -lc 'grep x .agent/review-input/evidence/E002.txt'", "item.completed"),
            codex_line("item_3", "/bin/bash -lc 'cat .agent/review-input/evidence/E002.txt .agent/review-input/evidence/E001.txt'", "item.completed"),
            codex_line("item_4", "/bin/bash -lc 'cat .agent/review-input/evidence/E009.txt'", "item.completed"),
            r#"{"type":"item.completed","item":{"id":"item_5","type":"agent_message","text":".agent/review-input/evidence/E003.txt"}}"#.to_string(),
            "not json .agent/review-input/evidence/E003.txt".to_string(),
        ]
        .join("\n");
        let reads = reads_in_stream("codex", &stream, &[WT.to_string()], &codes(), false);
        assert_eq!(
            reads,
            [
                Read { code: "E002".into(), place: Place::Item("item_2".into()) },
                Read { code: "E001".into(), place: Place::Item("item_3".into()) },
            ]
        );
    }

    #[test]
    fn an_agy_stream_counts_done_read_file_and_view_file_steps_without_an_error() {
        let p = |code: &str| format!("{WT}/.agent/review-input/evidence/{code}.txt");
        let stream = [
            agy_line(4, "read_file", "Path", &p("E003"), "ACTIVE", false),
            agy_line(5, "read_file", "Path", &p("E003"), "DONE", true),
            agy_line(6, "list_dir", "Path", &p("E003"), "DONE", false),
            agy_line(7, "read_file", "AbsolutePath", &p("E003"), "DONE", false),
            agy_line(8, "view_file", "AbsolutePath", &p("E002"), "DONE", false),
            agy_line(9, "read_file", "Path", &p("E001"), "DONE", false),
            agy_line(10, "read_file", "Path", &p("E002"), "DONE", false),
        ]
        .join("\n");
        let reads = reads_in_stream("antigravity", &stream, &[WT.to_string()], &codes(), false);
        assert_eq!(
            reads,
            [
                Read { code: "E002".into(), place: Place::Step(8) },
                Read { code: "E001".into(), place: Place::Step(9) },
            ]
        );
        // a Claude stream is never read for this
        assert!(!reads_from_files("claude-code"));
        assert!(reads_from_files("codex") && reads_from_files("antigravity"));
    }
}
