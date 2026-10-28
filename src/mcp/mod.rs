mod exe_watch;
mod forward;
mod jobs;
mod tools;

use crate::redact::Masker;
use anyhow::{anyhow, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use ulid::Ulid;

const OLD: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const VERSIONS: &[&str] = &[
    "2026-07-28",
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
pub const FORBIDDEN: &[&[&str]] = &[
    &["checks", "protect"],
    &["checks", "unprotect"],
    &["config", "set"],
    &["config", "unset"],
    &["config", "set-secret"],
    &["config", "unset-secret"],
    &["hook"],
    &["run-worker"],
];
const INSTRUCTIONS: &str = "Before first use, call thoth_guide or load the thoth-dispatch skill. These tools are for the commander workflow. Start with thoth_status. thoth_dispatch, thoth_review_start and thoth_spec_review_start return a run id immediately; use thoth_wait repeatedly, with each wait limited by the server maximum. Verify takes two steps: thoth_verify_start re-runs the repository's approved checks in the background and returns a job id at once, then thoth_job_status reports the job until its status is no longer `working`: completed, failed or unknown. A check added with thoth_checks_add stays pending and is not run until it is approved. The commander approves with thoth_checks_approve, which needs a reason that is recorded for the reviewer; with run it also approves the protected test files that run changed, which thoth_review_start waits for. A person can also approve with `thoth checks approve` in a terminal. See the thoth-dispatch skill and docs/mcp.md for the complete workflow.";

pub struct Options {
    pub default_repo: Option<PathBuf>,
    pub max_wait: u64,
    pub max_result_bytes: usize,
    pub plugin_root: Option<PathBuf>,
}

/// Tools that change state; refused while the installed plugin does not match
/// this executable. (`Tool.read_only` cannot stand in: evidence_show and
/// review_packet are not read-only there but change nothing.)
pub const STATE_CHANGING: &[&str] = &[
    "thoth_init",
    "thoth_task_create",
    "thoth_task_update_spec",
    "thoth_task_close",
    "thoth_task_accept",
    "thoth_dispatch",
    "thoth_review_start",
    "thoth_review_record",
    "thoth_spec_review_record",
    "thoth_spec_review_start",
    "thoth_spec_review_confirm",
    "thoth_resume",
    "thoth_cancel",
    "thoth_verify_start",
    "thoth_checks_add",
    "thoth_checks_approve",
    "thoth_checks_remove",
];

/// Set in the environment of `thoth_checks_approve`'s child only, so the
/// approval it records says it came through MCP (`approved_by`).
const VIA_MCP_ENV: &str = "THOTH_VIA_MCP";

/// The plugin folder to compare: `--plugin-root`, else `CLAUDE_PLUGIN_ROOT`.
fn plugin_root(flag: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(p) = flag {
        if !p.to_string_lossy().contains("${") {
            return Some(p);
        }
        masked_eprintln!(
            "warning: ignoring invalid --plugin-root {}; using CLAUDE_PLUGIN_ROOT if set",
            p.display()
        );
    }
    std::env::var_os("CLAUDE_PLUGIN_ROOT")
        .filter(|v| !v.is_empty() && !v.to_string_lossy().contains("${"))
        .map(PathBuf::from)
}

fn mismatch(o: &Options) -> Option<crate::plugin_check::Mismatch> {
    o.plugin_root
        .as_deref()
        .and_then(crate::plugin_check::compare)
}

fn instructions(o: &Options) -> String {
    match mismatch(o) {
        Some(m) => format!("{INSTRUCTIONS}\n\n{}", m.warning_line()),
        None => INSTRUCTIONS.to_owned(),
    }
}

pub fn run(mut options: Options) -> Result<i32> {
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        return Err(anyhow!(
            "thoth mcp cannot run inside a worker or reviewer (THOTH_RUN_ID is set)"
        ));
    }
    let own_cwd = std::env::current_dir()?;
    if let Some(p) = options.default_repo.as_ref() {
        if p.to_string_lossy().contains("${") || !p.is_dir() {
            masked_eprintln!(
                "warning: ignoring invalid --default-repo {}; using server working directory",
                p.display()
            );
            options.default_repo = None;
        }
    }
    options.plugin_root = plugin_root(options.plugin_root.take());
    if let Some(m) = mismatch(&options) {
        masked_eprintln!("{}", m.warning_line());
    }
    forward::serve(&options, &own_cwd)
}

fn write_response(out: &mut impl Write, v: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *out, &v)?;
    out.write_all(b"\n")?;
    out.flush()
}
fn result(id: Value, v: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"result":v})
}
fn error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut e = json!({"code":code,"message":message});
    if let Some(d) = data {
        e["data"] = d
    }
    json!({"jsonrpc":"2.0","id":id,"error":e})
}
fn generation(params: &Value) -> std::result::Result<bool, Value> {
    let meta = params.get("_meta");
    let version = meta.and_then(|m| m.get("io.modelcontextprotocol/protocolVersion"));
    let Some(v) = version else { return Ok(false) };
    if v != "2026-07-28" {
        return Err(error(
            Value::Null,
            -32022,
            "Unsupported protocol version",
            Some(json!({"supported":VERSIONS,"requested":v})),
        ));
    }
    if !meta
        .and_then(|m| m.get("io.modelcontextprotocol/clientCapabilities"))
        .is_some_and(Value::is_object)
    {
        return Err(error(
            Value::Null,
            -32602,
            "clientCapabilities must be an object",
            None,
        ));
    }
    Ok(true)
}
fn decorate(v: &mut Value, new: bool) {
    if new {
        v["resultType"] = json!("complete");
        v["_meta"] = json!({"io.modelcontextprotocol/serverInfo":server_info()});
    }
}
fn server_info() -> Value {
    json!({"name":"thoth","version":env!("CARGO_PKG_VERSION")})
}

fn handle(
    id: Value,
    method: &str,
    params: &Value,
    o: &Options,
    own: &Path,
    jobs: &mut jobs::Jobs,
    replaced: Option<&str>,
) -> Value {
    if method == "initialize" {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or("");
        let v = if OLD.contains(&requested) {
            requested
        } else {
            OLD[0]
        };
        return result(
            id,
            json!({"protocolVersion":v,"capabilities":{"tools":{"listChanged":true}},"serverInfo":server_info(),"instructions":instructions(o)}),
        );
    }
    let new = match generation(params) {
        Ok(v) => v,
        Err(mut e) => {
            e["id"] = id;
            return e;
        }
    };
    // The 2026-07-28 spec requires `ttlMs` and `cacheScope` on list-style results
    // (tools/list, server/discover); Claude Code rejects the list without them.
    // Thoth's tool list is tied to the locally installed version and the state is
    // per user, so 0 + "private" keeps "do not cache" while satisfying the schema.
    let cacheable = |mut v: Value| {
        if new {
            v["ttlMs"] = json!(0);
            v["cacheScope"] = json!("private");
        }
        v
    };
    let mut value = match method {
        "ping" => json!({}),
        "server/discover" => {
            if !new {
                return error(
                    id,
                    -32602,
                    "server/discover requires protocol metadata",
                    None,
                );
            }
            cacheable(
                json!({"supportedVersions":VERSIONS,"capabilities":{"tools":{"listChanged":true}},"instructions":instructions(o)}),
            )
        }
        "tools/list" => cacheable(
            json!({"tools":tools::TOOLS.iter().map(tools::definition).collect::<Vec<_>>() }),
        ),
        "tools/call" => return call(id, params, o, own, new, jobs, replaced),
        _ => return error(id, -32601, "Method not found", None),
    };
    decorate(&mut value, new);
    result(id, value)
}

fn call(
    id: Value,
    params: &Value,
    o: &Options,
    own: &Path,
    new: bool,
    jobs: &mut jobs::Jobs,
    replaced: Option<&str>,
) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let Some(tool) = tools::TOOLS.iter().find(|t| t.name == name) else {
        return error(id, -32602, &format!("unknown tool {name}"), None);
    };
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let masker = Masker::current();
    let found = mismatch(o);
    let mut v = match found.as_ref() {
        Some(m) if STATE_CHANGING.contains(&tool.name) => {
            tool_error(m.refusal_line(), None, Some(m.to_json()), None)
        }
        Some(m) => {
            let mut v = execute(tool, &args, o, own, jobs, &masker);
            prepend(&mut v, m.warning_line());
            v
        }
        None => execute(tool, &args, o, own, jobs, &masker),
    };
    // Last, so it is the first line: this server's tool list is no longer
    // current, and handing calls to the new executable failed.
    if let Some(line) = replaced {
        prepend(&mut v, line.to_owned());
    }
    mask_result(&mut v, &masker);
    decorate(&mut v, new);
    result(id, v)
}

/// Masks a tool result (RD-1). A text that is the serialized
/// structuredContent becomes the masked document's serialization, so it still
/// parses as JSON; any other text is masked as text.
fn mask_result(v: &mut Value, masker: &Masker) {
    let mut raw = None;
    let mut masked = None;
    if let Some(doc) = v.get_mut("structuredContent") {
        raw = Some(doc.to_string());
        masker.mask_value(doc);
        masked = Some(doc.to_string());
    }
    for item in v.get_mut("content").and_then(Value::as_array_mut).into_iter().flatten() {
        let Some(Value::String(text)) = item.get_mut("text") else { continue };
        *text = match (&raw, &masked) {
            (Some(raw), Some(masked)) if text == raw => masked.clone(),
            _ => masker.mask_text(text).0,
        };
    }
}

fn prepend(v: &mut Value, line: String) {
    if let Some(content) = v.get_mut("content").and_then(Value::as_array_mut) {
        content.insert(0, json!({"type":"text","text":line}));
    }
}

/// The first line of each tool result answered here after a failed hand-over
/// (src/mcp/forward.rs). `next_call` is true for `spawn_failed`.
fn replaced_line(exe: &Path, code: &str, detail: &str, next_call: bool) -> String {
    let retry = if next_call {
        "It will try again on the next call."
    } else {
        "It will try again when that file changes."
    };
    format!(
        "warning: the thoth executable this MCP server was started from ({}) has been replaced; \
         handing calls to the new executable failed ({code}: {detail}). This server still uses \
         the old tool list. {retry} To switch now, run /mcp in Claude Code and reconnect \
         plugin:thoth:thoth, or start a new conversation.",
        exe.display()
    )
}

fn tool_error(
    message: String,
    exit: Option<i32>,
    errdoc: Option<Value>,
    outdoc: Option<Value>,
) -> Value {
    let d = json!({"ok":false,"exit_code":exit,"error":message,"error_document":errdoc,"stdout_document":outdoc});
    json!({"content":[{"type":"text","text":d.to_string()}],"structuredContent":d,"isError":true})
}
/// A successful document as a tool result; one over `--max-result-bytes` is
/// saved under `mcp/out` and its path returned instead. Both are masked.
fn success(mut doc: Value, paths: &crate::config::Paths, o: &Options, masker: &Masker) -> Value {
    masker.mask_value(&mut doc);
    let encoded = doc.to_string();
    if encoded.len() > o.max_result_bytes {
        let p = paths
            .home
            .join("mcp/out")
            .join(format!("result-{}.json", Ulid::generate()));
        if let Some(parent) = p.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Err(e) = fs::write(&p, encoded.as_bytes()) {
            return tool_error(e.to_string(), None, None, None);
        }
        let d = json!({"ok":true,"too_large":true,"path":p,"bytes":encoded.len(),"hint":"Read the saved file, or narrow events with since / evidence show with bytes."});
        return json!({"content":[{"type":"text","text":d.to_string()}],"structuredContent":d,"isError":false});
    }
    json!({"content":[{"type":"text","text":encoded}],"structuredContent":doc,"isError":false})
}
fn execute(
    t: &tools::Tool,
    args: &Value,
    o: &Options,
    own: &Path,
    jobs: &mut jobs::Jobs,
    masker: &Masker,
) -> Value {
    let Some(a) = args.as_object() else {
        return tool_error("arguments must be an object".into(), None, None, None);
    };
    let accepted = t.params.iter().map(|p| p.name).collect::<Vec<_>>();
    for k in a.keys() {
        if !accepted.contains(&k.as_str()) {
            return tool_error(
                format!(
                    "unknown parameter {k}; accepted parameters: {}",
                    accepted.join(", ")
                ),
                None,
                None,
                None,
            );
        }
    }
    for p in t.params {
        let Some(v) = a.get(p.name) else {
            if p.required {
                return tool_error(format!("missing parameter {}", p.name), None, None, None);
            }
            continue;
        };
        let good = match p.kind {
            tools::Kind::String | tools::Kind::Role => v.is_string(),
            tools::Kind::Integer => v.as_i64().is_some(),
            tools::Kind::Object => v.is_object(),
            tools::Kind::Bool => v.is_boolean(),
            tools::Kind::StrArray => v
                .as_array()
                .is_some_and(|items| items.iter().all(Value::is_string)),
        };
        if !good {
            return tool_error(
                format!("parameter {} has the wrong type", p.name),
                None,
                None,
                None,
            );
        }
        if p.kind == tools::Kind::Role && !matches!(v.as_str(), Some("implementer" | "reviewer")) {
            return tool_error(
                "parameter role must be implementer or reviewer".into(),
                None,
                None,
                None,
            );
        }
    }
    let cwd = match a.get("repo") {
        Some(v) => {
            let p = PathBuf::from(v.as_str().unwrap());
            if !p.is_absolute() || !p.is_dir() {
                return tool_error(
                    "parameter repo must be an absolute existing directory".into(),
                    None,
                    None,
                    None,
                );
            }
            p
        }
        None => o.default_repo.clone().unwrap_or_else(|| own.to_path_buf()),
    };
    let paths = match crate::config::Paths::from_env() {
        Ok(p) => p,
        Err(e) => return tool_error(e.to_string(), None, None, None),
    };
    let mut temp = None;
    let mut packet = None;
    let mut argv = t.command.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let positional = |n: &str| -> std::result::Result<Option<String>, Value> {
        match a.get(n).and_then(Value::as_str) {
            Some(s) if s.starts_with('-') => Err(tool_error(
                format!("parameter {n} must not start with '-'"),
                None,
                None,
                None,
            )),
            Some(s) => Ok(Some(s.into())),
            None => Ok(None),
        }
    };
    macro_rules! pos {
        ($n:literal) => {
            match positional($n) {
                Ok(Some(v)) => argv.push(v),
                Ok(None) => {}
                Err(e) => return e,
            }
        };
    }
    match t.name {
        "thoth_status" => pos!("task"),
        "thoth_profile_usage" => pos!("id"),
        "thoth_recommend" => {
            argv.push(format!("--role={}", a["role"].as_str().unwrap()));
            flag(&mut argv, a, "task", "task")
        }
        "thoth_task_create" => {
            flag(&mut argv, a, "title", "title");
            flag(&mut argv, a, "base", "base");
            temp = match json_file(&paths, "spec", &a["spec"]) {
                Ok(v) => Some(v),
                Err(e) => return tool_error(e.to_string(), None, None, None),
            };
            argv.extend(["-f".into(), temp.as_ref().unwrap().display().to_string()])
        }
        "thoth_task_update_spec" => {
            pos!("task");
            temp = match json_file(&paths, "spec", &a["spec"]) {
                Ok(v) => Some(v),
                Err(e) => return tool_error(e.to_string(), None, None, None),
            };
            argv.extend(["-f".into(), temp.as_ref().unwrap().display().to_string()])
        }
        "thoth_task_close" => {
            pos!("task");
            flag(&mut argv, a, "reason", "reason")
        }
        "thoth_task_accept" => {
            pos!("task");
            flag(&mut argv, a, "reason", "reason")
        }
        "thoth_task_result" => {
            pos!("task");
            flag(&mut argv, a, "run", "run")
        }
        "thoth_dispatch" => {
            pos!("task");
            for (n, f) in [
                ("profile", "profile"),
                ("model", "model"),
                ("timeout", "timeout"),
                ("allowed_tools", "allowed-tools"),
                ("allow_attempt", "allow-attempt"),
                ("effort", "effort"),
                ("reason", "reason"),
                ("skip_spec_review", "skip-spec-review"),
            ] {
                flag(&mut argv, a, n, f)
            }
        }
        "thoth_wait" => {
            let task = a.get("task");
            let run = a.get("run");
            if task.is_some() == run.is_some() {
                return tool_error(
                    "parameters task and run: exactly one is required".into(),
                    None,
                    None,
                    None,
                );
            }
            if task.is_some() {
                pos!("task")
            } else {
                let r = match positional("run") {
                    Ok(Some(v)) => v,
                    Err(e) => return e,
                    _ => unreachable!(),
                };
                argv.push(format!("--run={r}"))
            }
            let timeout = match a.get("timeout_seconds") {
                Some(v) => match v.as_i64() {
                    Some(n) if n >= 1 && (n as u64) <= o.max_wait => n as u64,
                    _ => {
                        return tool_error(
                            format!(
                                "parameter timeout_seconds must be between 1 and {} seconds",
                                o.max_wait
                            ),
                            None,
                            None,
                            None,
                        )
                    }
                },
                None => o.max_wait,
            };
            argv.push(format!("--timeout={timeout}"))
        }
        "thoth_events" => {
            let since = a.get("since").and_then(Value::as_i64).unwrap_or(0);
            if since < 0 {
                return tool_error(
                    "parameter since must be 0 or greater".into(),
                    None,
                    None,
                    None,
                );
            }
            argv.push(format!("--since={since}"));
            flag(&mut argv, a, "task", "task")
        }
        "thoth_evidence_list" => pos!("task"),
        "thoth_evidence_show" => {
            pos!("code");
            flag(&mut argv, a, "task", "task");
            flag(&mut argv, a, "bytes", "bytes")
        }
        "thoth_review_packet" => {
            pos!("task");
            let out = paths
                .home
                .join("mcp/out")
                .join(format!("packet-{}.md", Ulid::generate()));
            if let Some(parent) = out.parent() {
                if let Err(e) = fs::create_dir_all(parent) {
                    return tool_error(e.to_string(), None, None, None);
                }
            }
            argv.push(format!("--out={}", out.display()));
            packet = Some(out)
        }
        "thoth_review_record" => {
            pos!("task");
            temp = match json_file(&paths, "review", &a["review"]) {
                Ok(v) => Some(v),
                Err(e) => return tool_error(e.to_string(), None, None, None),
            };
            argv.extend(["-f".into(), temp.as_ref().unwrap().display().to_string()])
        }
        "thoth_review_start" => {
            pos!("task");
            for (n, f) in [
                ("profile", "profile"),
                ("model", "model"),
                ("timeout", "timeout"),
                ("effort", "effort"),
                ("reason", "reason"),
            ] {
                flag(&mut argv, a, n, f)
            }
        }
        "thoth_review_show" => pos!("task"),
        "thoth_spec_review_record" => {
            pos!("task");
            temp = match json_file(&paths, "spec-review", &a["review"]) {
                Ok(v) => Some(v),
                Err(e) => return tool_error(e.to_string(), None, None, None),
            };
            argv.extend(["-f".into(), temp.as_ref().unwrap().display().to_string()])
        }
        "thoth_spec_review_show" => pos!("task"),
        "thoth_spec_review_start" | "thoth_spec_review_confirm" => {
            pos!("task");
            for (n, f) in [
                ("profile", "profile"),
                ("model", "model"),
                ("timeout", "timeout"),
                ("effort", "effort"),
                ("reason", "reason"),
            ] {
                flag(&mut argv, a, n, f)
            }
            // SR-3: `edit` is a boolean switch, not a valued flag.
            if a.get("edit") == Some(&Value::Bool(true)) {
                argv.push("--edit".into())
            }
        }
        "thoth_resume" | "thoth_cancel" => pos!("run"),
        "thoth_verify_start" => {
            let run = match positional("run") {
                Ok(Some(v)) => v,
                Err(e) => return e,
                _ => unreachable!(),
            };
            argv.extend([run.clone(), "--json".into()]);
            if a.get("at_base") == Some(&Value::Bool(true)) {
                argv.push("--at-base".into())
            }
            if a.get("dry_run") == Some(&Value::Bool(true)) {
                argv.push("--dry-run".into())
            }
            if let Some(timeout) = a.get("timeout").and_then(Value::as_str) {
                if let Err(e) = crate::cli::parse_duration_secs(timeout) {
                    return tool_error(format!("parameter timeout: {e}"), None, None, None);
                }
                argv.push(format!("--timeout={timeout}"))
            }
            if forbidden(&argv) {
                return tool_error("command is forbidden through MCP".into(), None, None, None);
            }
            return match jobs::start(jobs, &paths.home, &argv, &cwd, &run) {
                Ok(doc) => success(doc, &paths, o, masker),
                Err(e) => tool_error(e, None, None, None),
            };
        }
        "thoth_job_status" => {
            let wait = match a.get("wait_seconds") {
                Some(v) => match v.as_i64() {
                    Some(n) if n >= 0 && (n as u64) <= o.max_wait => n as u64,
                    _ => {
                        return tool_error(
                            format!(
                                "parameter wait_seconds must be between 0 and {} seconds",
                                o.max_wait
                            ),
                            None,
                            None,
                            None,
                        )
                    }
                },
                None => 0,
            };
            // Reads the job's own files; no child process and no database.
            let job_id = a["job_id"].as_str().unwrap();
            return match jobs::status(jobs, &paths.home, job_id, wait) {
                Ok(doc) => success(doc, &paths, o, masker),
                Err(e) => tool_error(e, None, None, None),
            };
        }
        "thoth_checks_add" => {
            pos!("id");
            for (n, f) in [("cmd", "cmd"), ("shell", "shell"), ("cwd", "cwd"), ("exe_pin", "exe-pin")] {
                flag(&mut argv, a, n, f)
            }
            for input in a.get("inputs").and_then(Value::as_array).into_iter().flatten() {
                argv.push(format!("--input={}", input.as_str().unwrap()))
            }
        }
        "thoth_checks_approve" => {
            flag(&mut argv, a, "run", "run");
            flag(&mut argv, a, "reason", "reason")
        }
        "thoth_checks_remove" => pos!("id"),
        "thoth_doctor" => flag(&mut argv, a, "shell", "shell"),
        "thoth_spec_lint" => {
            let file = match (a.get("spec"), a.get("file").and_then(Value::as_str)) {
                (Some(spec), None) => match json_file(&paths, "spec", spec) {
                    Ok(p) => {
                        temp = Some(p.clone());
                        p
                    }
                    Err(e) => return tool_error(e.to_string(), None, None, None),
                },
                (None, Some(file)) => {
                    let p = PathBuf::from(file);
                    if !p.is_absolute() || !p.is_file() {
                        return tool_error(
                            "parameter file must be the absolute path of an existing file".into(),
                            None,
                            None,
                            None,
                        );
                    }
                    p
                }
                _ => {
                    return tool_error(
                        "parameters spec and file: exactly one is required".into(),
                        None,
                        None,
                        None,
                    )
                }
            };
            argv.push(file.display().to_string())
        }
        _ => {}
    }
    if t.name != "thoth_review_packet" {
        argv.push("--json".into())
    }
    if forbidden(&argv) {
        if let Some(p) = temp {
            let _ = fs::remove_file(p);
        }
        return tool_error("command is forbidden through MCP".into(), None, None, None);
    }
    let out = run_child(&argv, &cwd, t.name == "thoth_checks_approve");
    if let Some(p) = temp {
        let _ = fs::remove_file(p);
    }
    let out = match out {
        Ok(v) => v,
        Err(e) => return tool_error(e.to_string(), None, None, None),
    };
    // Exit 1 from doctor means "some item failed", and from spec lint "the
    // spec has an error"; either way stdout is still the full report.
    if out.status.success()
        || (t.name == "thoth_wait" && out.status.code() == Some(3))
        || (matches!(t.name, "thoth_doctor" | "thoth_spec_lint") && out.status.code() == Some(1))
    {
        let doc = if let Some(p) = packet {
            match fs::metadata(&p) {
                Ok(m) => json!({"ok":true,"path":p,"bytes":m.len()}),
                Err(e) => return tool_error(e.to_string(), out.status.code(), None, None),
            }
        } else {
            match serde_json::from_slice::<Value>(&out.stdout) {
                Ok(v) if v.is_object() => v,
                _ => return failure(out),
            }
        };
        success(doc, &paths, o, masker)
    } else {
        failure(out)
    }
}
fn flag(argv: &mut Vec<String>, a: &Map<String, Value>, name: &str, flag: &str) {
    if let Some(v) = a.get(name) {
        let s = v
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| v.to_string());
        argv.push(format!("--{flag}={s}"))
    }
}
fn json_file(paths: &crate::config::Paths, label: &str, v: &Value) -> Result<PathBuf> {
    let dir = paths.home.join("mcp/tmp");
    fs::create_dir_all(&dir)?;
    let p = dir.join(format!("{label}-{}.json", Ulid::generate()));
    fs::write(&p, serde_json::to_vec(v)?)?;
    Ok(p)
}
fn forbidden(argv: &[String]) -> bool {
    FORBIDDEN.iter().any(|x| {
        argv.iter()
            .map(String::as_str)
            .take(x.len())
            .eq(x.iter().copied())
    })
}
fn run_child(argv: &[String], cwd: &Path, via_mcp: bool) -> io::Result<std::process::Output> {
    let mut c = Command::new(std::env::current_exe()?);
    c.args(argv)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if via_mcp {
        c.env(VIA_MCP_ENV, "1");
    } else {
        c.env_remove(VIA_MCP_ENV);
    }
    crate::process::quiet(&mut c);
    c.output()
}
fn failure(out: std::process::Output) -> Value {
    let stdout_document = serde_json::from_slice::<Value>(&out.stdout)
        .ok()
        .filter(Value::is_object);
    let last = String::from_utf8_lossy(&out.stderr)
        .lines()
        .rfind(|x| !x.trim().is_empty())
        .unwrap_or("")
        .to_string();
    let error_document = serde_json::from_str::<Value>(&last)
        .ok()
        .filter(Value::is_object);
    let message = error_document
        .as_ref()
        .and_then(|v| v.get("error"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let s = String::from_utf8_lossy(&out.stderr);
            let mut start = s.len().saturating_sub(2000);
            while !s.is_char_boundary(start) {
                start += 1
            }
            s[start..].to_string()
        });
    tool_error(message, out.status.code(), error_document, stdout_document)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn success_masks_inline_and_saved_results() {
        let m = Masker::from_pairs(&[("dummy-mcp-value-1", "env:RD_MCP_X")]);
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths { home: dir.path().to_path_buf() };
        let o = Options { default_repo: None, max_wait: 60, max_result_bytes: 200, plugin_root: None };

        let v = success(json!({"ok":true,"note":"x dummy-mcp-value-1 y"}), &paths, &o, &m);
        assert!(!v["content"][0]["text"].as_str().unwrap().contains("dummy-mcp-value-1"));
        assert!(!v["structuredContent"].to_string().contains("dummy-mcp-value-1"));
        assert!(v["structuredContent"]["note"].as_str().unwrap().contains("[redacted:"));

        let big = format!("{} dummy-mcp-value-1", "a".repeat(400));
        let v = success(json!({"ok":true,"note":big}), &paths, &o, &m);
        assert_eq!(v["structuredContent"]["too_large"], true);
        let saved = fs::read_to_string(v["structuredContent"]["path"].as_str().unwrap()).unwrap();
        assert!(!saved.contains("dummy-mcp-value-1"));
        assert!(saved.contains("[redacted:"));
    }
    #[test]
    fn mask_result_keeps_the_document_text_parseable() {
        let begin = ["-----BEGIN ", "RSA PRIVATE KEY-----"].concat();
        let mut v = tool_error(format!("bad {begin}\nAAAAdummy"), Some(1), None, None);
        prepend(&mut v, "warning: dummy-mcp-value-2 here".to_string());
        mask_result(&mut v, &Masker::from_pairs(&[("dummy-mcp-value-2", "env:RD_MCP_Y")]));
        assert_eq!(v["content"][0]["text"], "warning: [redacted:env:RD_MCP_Y] here");
        let text = v["content"][1]["text"].as_str().unwrap();
        let doc: Value = serde_json::from_str(text).unwrap();
        assert_eq!(doc, v["structuredContent"]);
        assert_eq!(doc["error"], "bad [redacted:private-key]");
        assert_eq!(doc["exit_code"], 1);

        let mut clean = tool_error("plain".into(), None, None, None);
        let before = clean.clone();
        mask_result(&mut clean, &Masker::from_pairs(&[]));
        assert_eq!(clean, before);
    }
    #[test]
    fn instructions_lead_with_the_guide_and_stay_short() {
        assert!(INSTRUCTIONS
            .starts_with("Before first use, call thoth_guide or load the thoth-dispatch skill."));
        assert!(INSTRUCTIONS.chars().count() <= 1500);
        assert!(INSTRUCTIONS.contains("no longer"));
    }
    #[test]
    fn state_changing_tools_are_exactly_the_tools_that_write() {
        for name in STATE_CHANGING {
            let t = tools::TOOLS.iter().find(|t| t.name == *name).unwrap();
            assert!(!t.read_only, "{name}");
        }
        for t in tools::TOOLS.iter().filter(|t| !t.read_only) {
            assert!(
                STATE_CHANGING.contains(&t.name)
                    || matches!(t.name, "thoth_evidence_show" | "thoth_review_packet"),
                "{}",
                t.name
            );
        }
        assert_eq!(STATE_CHANGING.len(), 17);
        let mut names = STATE_CHANGING.to_vec();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), 17);
    }
    #[test]
    fn forbidden_sequences_are_enforced() {
        assert!(forbidden(&["hook".into(), "x".into()]));
        for bad in [
            ["checks", "protect", "*.snap"].as_slice(),
            ["checks", "unprotect", "*.snap"].as_slice(),
        ] {
            let argv = bad.iter().map(|x| x.to_string()).collect::<Vec<_>>();
            assert!(forbidden(&argv), "{argv:?}");
        }
        for t in tools::TOOLS {
            assert!(!forbidden(
                &t.command.iter().map(|x| x.to_string()).collect::<Vec<_>>()
            ))
        }
    }
}
