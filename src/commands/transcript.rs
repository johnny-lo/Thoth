//! `thoth transcript RUN [--offset N]` - what a run's worker did, read from
//! its `stream.jsonl` and translated into a fixed-shape list of steps.
//!
//! Read-only: no events, no database writes, nothing in the run directory is
//! touched. The caller pages through the file by byte offset, so a panel that
//! polls does not re-read (or re-translate) what it already has.

use crate::config::Paths;
use crate::db::{global, state};
use crate::ids;
use crate::output;
use crate::redact::Masker;
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const BRIEF_CHARS: usize = 300;

/// Collapses runs of whitespace, trims, and caps at 300 characters.
pub fn brief(s: &str) -> String {
    let collapsed = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.chars().count() > BRIEF_CHARS {
        let cut: String = collapsed.chars().take(BRIEF_CHARS).collect();
        format!("{cut}…")
    } else {
        collapsed
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub at: u64,
    pub kind: &'static str,
    pub nested: bool,
    pub text: String,
    pub tool: Option<String>,
    pub action: Option<&'static str>,
    pub target: Option<String>,
    pub exit_code: Option<i64>,
    pub is_error: Option<bool>,
}

impl Step {
    fn new(at: u64, kind: &'static str, nested: bool, text: &str) -> Step {
        Step {
            at,
            kind,
            nested,
            text: brief(text),
            tool: None,
            action: None,
            target: None,
            exit_code: None,
            is_error: None,
        }
    }

    fn tool(
        at: u64,
        nested: bool,
        tool: &str,
        action: &'static str,
        target: Option<&str>,
    ) -> Step {
        let target = target.map(brief);
        let text = target.clone().unwrap_or_else(|| brief(tool));
        Step {
            at,
            kind: "tool",
            nested,
            text,
            tool: Some(tool.to_string()),
            action: Some(action),
            target,
            exit_code: None,
            is_error: None,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "at": self.at,
            "kind": self.kind,
            "nested": self.nested,
            "text": self.text,
            "tool": self.tool,
            "action": self.action,
            "target": self.target,
            "exit_code": self.exit_code,
            "is_error": self.is_error,
        })
    }
}

fn s<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// Claude Code's tool name to (action, key of the input that names the target).
fn claude_tool(name: &str) -> (&'static str, Option<&'static str>) {
    match name {
        "Read" => ("read", Some("file_path")),
        "Write" => ("write", Some("file_path")),
        "Edit" | "MultiEdit" => ("edit", Some("file_path")),
        "NotebookEdit" => ("edit", Some("notebook_path")),
        "Bash" => ("run", Some("command")),
        "Glob" | "Grep" => ("search", Some("pattern")),
        "Task" => ("other", Some("description")),
        "WebFetch" => ("other", Some("url")),
        "WebSearch" => ("other", Some("query")),
        _ => ("other", None),
    }
}

fn is_codex(v: &Value, ty: &str) -> bool {
    ty.starts_with("thread.")
        || ty.starts_with("turn.")
        || ty.starts_with("item.")
        || (ty == "error" && v.get("message").is_some())
}

/// Translates one parsed line. Each line is judged on its own shape.
pub fn translate(at: u64, v: &Value) -> Vec<Step> {
    if v.get("event").is_some() {
        return Vec::new();
    }
    let Some(ty) = s(v, "type") else {
        return Vec::new();
    };
    if is_codex(v, ty) {
        translate_codex(at, ty, v)
    } else {
        translate_claude(at, ty, v)
    }
}

fn translate_claude(at: u64, ty: &str, v: &Value) -> Vec<Step> {
    let nested = v.get("parent_tool_use_id").is_some_and(|p| !p.is_null());
    let mut out = Vec::new();
    match ty {
        "system" => match s(v, "subtype") {
            Some("init") => out.push(Step::new(at, "start", nested, s(v, "model").unwrap_or(""))),
            Some("api_retry") => {
                let f = |k: &str| match v.get(k) {
                    Some(Value::String(x)) => x.clone(),
                    Some(Value::Null) | None => "?".to_string(),
                    Some(x) => x.to_string(),
                };
                let text = format!(
                    "api retry {}/{}: {}",
                    f("attempt"),
                    f("max_retries"),
                    f("error")
                );
                out.push(Step::new(at, "note", nested, &text));
            }
            Some("task_started") => {
                let d = s(v, "description").unwrap_or("");
                out.push(Step::new(at, "note", nested, &format!("subagent: {d}")));
            }
            _ => {}
        },
        "assistant" => {
            let blocks = v
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for b in &blocks {
                match s(b, "type") {
                    Some("text") => {
                        let t = s(b, "text").unwrap_or("");
                        if !t.trim().is_empty() {
                            out.push(Step::new(at, "message", nested, t));
                        }
                    }
                    Some("tool_use") => {
                        let name = s(b, "name").unwrap_or("");
                        let (action, key) = claude_tool(name);
                        let target = key.and_then(|k| b.get("input").and_then(|i| s(i, k)));
                        out.push(Step::tool(at, nested, name, action, target));
                    }
                    _ => {}
                }
            }
        }
        "user" => {
            let blocks = v
                .pointer("/message/content")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            for b in &blocks {
                if s(b, "type") != Some("tool_result") || b["is_error"] != Value::Bool(true) {
                    continue;
                }
                let text = match b.get("content") {
                    Some(Value::String(t)) => t.clone(),
                    Some(Value::Array(a)) => a
                        .iter()
                        .find(|x| s(x, "type") == Some("text"))
                        .and_then(|x| s(x, "text"))
                        .unwrap_or("tool failed")
                        .to_string(),
                    _ => "tool failed".to_string(),
                };
                let mut st = Step::new(at, "error", nested, &text);
                st.is_error = Some(true);
                out.push(st);
            }
        }
        "result" => {
            let text = s(v, "result").or_else(|| s(v, "subtype")).unwrap_or("");
            let mut st = Step::new(at, "result", nested, text);
            st.is_error = Some(v["is_error"].as_bool().unwrap_or(false));
            out.push(st);
        }
        _ => {}
    }
    out
}

fn translate_codex(at: u64, ty: &str, v: &Value) -> Vec<Step> {
    let mut out = Vec::new();
    match ty {
        "thread.started" => out.push(Step::new(at, "start", false, "")),
        "item.completed" => {
            let item = &v["item"];
            match s(item, "type").unwrap_or("") {
                "agent_message" => {
                    out.push(Step::new(at, "message", false, s(item, "text").unwrap_or("")))
                }
                "command_execution" => {
                    let mut st = Step::tool(
                        at,
                        false,
                        "command_execution",
                        "run",
                        s(item, "command"),
                    );
                    st.exit_code = item["exit_code"].as_i64();
                    st.is_error = Some(s(item, "status") == Some("failed"));
                    out.push(st);
                }
                "file_change" => {
                    let changes = item["changes"].as_array().cloned().unwrap_or_default();
                    for c in &changes {
                        let action = if s(c, "kind") == Some("add") { "write" } else { "edit" };
                        out.push(Step::tool(at, false, "file_change", action, s(c, "path")));
                    }
                }
                "error" => {
                    let mut st = Step::new(at, "error", false, s(item, "message").unwrap_or(""));
                    st.is_error = Some(true);
                    out.push(st);
                }
                "reasoning" | "todo_list" => {}
                other => out.push(Step::tool(at, false, other, "other", None)),
            }
        }
        "error" => {
            let mut st = Step::new(at, "error", false, s(v, "message").unwrap_or(""));
            st.is_error = Some(true);
            out.push(st);
        }
        "turn.completed" => {
            let mut st = Step::new(at, "result", false, "");
            st.is_error = Some(false);
            out.push(st);
        }
        "turn.failed" => {
            let text = v
                .pointer("/error/message")
                .and_then(Value::as_str)
                .unwrap_or("turn failed");
            let mut st = Step::new(at, "result", false, text);
            st.is_error = Some(true);
            out.push(st);
        }
        _ => {}
    }
    out
}

/// What one pass over the file produced.
struct Pass {
    next_offset: u64,
    lines: u64,
    skipped: u64,
    steps: Vec<Step>,
}

/// Consumes only complete lines (ending in `\n`) of `bytes`, which starts at
/// file position `base`. Each line is masked before it is translated, so the
/// 300-character cut never lands inside a secret that is still whole.
fn consume(base: u64, bytes: &[u8], masker: &Masker) -> Pass {
    let mut pass = Pass {
        next_offset: base,
        lines: 0,
        skipped: 0,
        steps: Vec::new(),
    };
    let mut pos = 0usize;
    while let Some(nl) = bytes[pos..].iter().position(|b| *b == b'\n') {
        let raw = &bytes[pos..pos + nl];
        let at = base + pos as u64;
        let raw = raw.strip_suffix(b"\r").unwrap_or(raw);
        let text = String::from_utf8_lossy(raw);
        pass.lines += 1;
        match serde_json::from_str::<Value>(&text) {
            Ok(mut v) if v.is_object() => {
                masker.mask_value(&mut v);
                pass.steps.extend(translate(at, &v))
            }
            _ => pass.skipped += 1,
        }
        pos += nl + 1;
        pass.next_offset = base + pos as u64;
    }
    pass
}

pub fn run(paths: &Paths, cwd: &Path, needle: &str, offset: u64, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let run_id = ids::resolve_prefix(&state::list_run_ids(&conn)?, needle)?;
    let run = state::get_run(&conn, &run_id)?
        .ok_or_else(|| anyhow::anyhow!("run {run_id} not found"))?;

    let harness: Option<String> = crate::commands::usage::global_read_only(paths)
        .ok()
        .and_then(|g| global::get_profile(&g, &run.account).ok().flatten())
        .map(|p| p.harness);

    let path = Path::new(&run.run_dir).join("stream.jsonl");
    let mut size = 0u64;
    let mut exists = false;
    let mut used_offset = 0u64;
    let mut reset = false;
    let mut pass = Pass {
        next_offset: 0,
        lines: 0,
        skipped: 0,
        steps: Vec::new(),
    };
    if let Ok(meta) = std::fs::metadata(&path) {
        if meta.is_file() {
            exists = true;
            size = meta.len();
            if offset > size {
                reset = true;
            } else {
                used_offset = offset;
            }
            let mut f = std::fs::File::open(&path)?;
            f.seek(SeekFrom::Start(used_offset))?;
            let mut buf = Vec::new();
            f.take(size - used_offset).read_to_end(&mut buf)?;
            pass = consume(used_offset, &buf, &Masker::load(Some(&paths.home)));
        }
    }

    let steps: Vec<Value> = pass.steps.iter().map(Step::to_json).collect();
    let v = json!({
        "run_id": run.run_id,
        "task_id": run.task_id,
        "role": run.role,
        "profile": run.account,
        "model": run.model,
        "status": run.status,
        "harness": harness,
        "path": path.display().to_string(),
        "exists": exists,
        "size": size,
        "offset": used_offset,
        "next_offset": pass.next_offset,
        "reset": reset,
        "lines": pass.lines,
        "skipped": pass.skipped,
        "steps": steps,
    });

    let mut human = format!(
        "{}  {}  {}  {}  {}  harness={}",
        run.run_id,
        run.role,
        run.account,
        run.model,
        run.status,
        harness.as_deref().unwrap_or("-")
    );
    if !exists {
        human.push_str(&format!("\nno stream yet for {}", run.run_id));
    } else {
        human.push_str(&format!(
            "\nstream: {} ({} bytes, read {}..{})",
            path.display(),
            size,
            used_offset,
            pass.next_offset
        ));
        for st in &pass.steps {
            let mut detail = if st.kind == "tool" {
                let mut d = format!(
                    "{} {}",
                    st.action.unwrap_or("other"),
                    st.target.as_deref().or(st.tool.as_deref()).unwrap_or("")
                );
                if let Some(c) = st.exit_code {
                    d.push_str(&format!(" (exit {c})"));
                }
                d
            } else {
                st.text.clone()
            };
            if st.nested {
                detail = format!("  {detail}");
            }
            human.push_str(&format!("\n{:>8}  {:<8} {}", st.at, st.kind, detail));
        }
    }
    output::emit(json, &v, &human);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(line: &str) -> Vec<Step> {
        translate(0, &serde_json::from_str::<Value>(line).unwrap())
    }

    #[test]
    fn brief_collapses_whitespace_and_caps_at_300() {
        assert_eq!(brief("  a\n\n b\tc  "), "a b c");
        let long = "x".repeat(301);
        assert_eq!(brief(&long), format!("{}…", "x".repeat(300)));
        let exact = "x".repeat(300);
        assert_eq!(brief(&exact), exact);
    }

    #[test]
    fn claude_tools_map_to_actions() {
        let cases = [
            ("Read", "read", "file_path"),
            ("Write", "write", "file_path"),
            ("Edit", "edit", "file_path"),
            ("MultiEdit", "edit", "file_path"),
            ("NotebookEdit", "edit", "notebook_path"),
            ("Bash", "run", "command"),
            ("Glob", "search", "pattern"),
            ("Grep", "search", "pattern"),
            ("Task", "other", "description"),
            ("WebFetch", "other", "url"),
            ("WebSearch", "other", "query"),
        ];
        for (name, action, key) in cases {
            let line = json!({"type":"assistant","message":{"content":[
                {"type":"tool_use","name":name,"input":{key:"VAL"}}]}})
            .to_string();
            let steps = one(&line);
            assert_eq!(steps.len(), 1, "{name}");
            assert_eq!(steps[0].action, Some(action), "{name}");
            assert_eq!(steps[0].target.as_deref(), Some("VAL"), "{name}");
            assert_eq!(steps[0].tool.as_deref(), Some(name));
        }
        let line = json!({"type":"assistant","message":{"content":[
            {"type":"tool_use","name":"Mystery","input":{"file_path":"x"}}]}})
        .to_string();
        let steps = one(&line);
        assert_eq!(steps[0].action, Some("other"));
        assert_eq!(steps[0].target, None);
        assert_eq!(steps[0].text, "Mystery");
    }

    #[test]
    fn a_line_is_classified_by_its_own_shape() {
        assert!(one(r#"{"event":"init"}"#).is_empty());
        let m = one(r#"{"type":"item.completed","item":{"type":"agent_message","text":"hi"}}"#);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].kind, "message");
        let n = one(
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"cargo test"}}]},"parent_tool_use_id":"toolu_1"}"#,
        );
        assert_eq!(n.len(), 1);
        assert!(n[0].nested);
        assert_eq!(n[0].action, Some("run"));
        assert!(one(r#"{"hello":1}"#).is_empty());
    }

    #[test]
    fn consume_masks_before_the_300_character_cut() {
        let token = format!("{}{}", ["s", "k-"].concat(), "dummy0123456789dummy99");
        let text = format!("{} {token}", "a".repeat(290));
        let line = json!({"type":"assistant","message":{"content":[{"type":"text","text":text}]}});
        let bytes = format!("{line}\n");
        let pass = consume(0, bytes.as_bytes(), &Masker::from_pairs(&[]));
        assert_eq!(pass.steps.len(), 1);
        assert!(!pass.steps[0].text.contains("sk-"));
        assert!(pass.steps[0].text.contains("[redacted"));
    }
}
