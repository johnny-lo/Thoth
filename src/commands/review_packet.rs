//! `thoth review packet TASK [--out FILE] [--json]`.
//!
//! One read-only document with everything a reviewer needs to judge a task, so
//! the commander no longer stitches it together from `evidence list`,
//! `evidence show` and `events` by hand. It reuses those commands' readers and
//! writes nothing except one `secret_redacted` event when it masked a secret
//! (RD-1): no other row or event, no file other than `--out`. In particular
//! it does not append the `evidence_opened` event `evidence show` does, because
//! a packet is not a reviewer opening a code.
//!
//! Everything in it leaves masked (see `redact`): a command's tail is taken
//! after its whole output was masked, and diffs are masked whole.
//!
//! The worker's own `summary` is left out on purpose: the reviewer judges from
//! evidence, not from the worker's prose. Diffs are shown whole, never cut at
//! `evidence show`'s 4000-byte default (a cut diff hid part of a change from a
//! reviewer during the trials).

use crate::commands::evidence::{detail_exit_code, read_blob, read_blob_masked};
use crate::config::Paths;
use crate::db::state;
use crate::ids;
use crate::model::event_type;
use crate::redact::{self, Counts, Masker};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;

/// How many trailing lines of a command's stored output the index shows.
const TAIL_LINES: usize = 15;

/// The spec keys shown to the reviewer, in reading order.
const SPEC_LIST_KEYS: [(&str, &str); 4] = [
    ("acceptance_criteria", "Acceptance criteria"),
    ("constraints", "Constraints"),
    ("non_goals", "Non-goals"),
    ("excluded_alternatives", "Excluded alternatives"),
];

pub fn run(paths: &Paths, cwd: &Path, needle: &str, out: Option<&Path>, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)
        .map_err(|e| anyhow::anyhow!("task '{needle}': {e}"))?;
    if state::get_task(&conn, &task_id)?.is_none() {
        anyhow::bail!("task '{needle}': no such task {task_id}");
    }
    let masker = Masker::load(Some(&paths.home));
    let (doc, _rows, mut redactions) = packet_doc(&conn, &task_id, &masker)?;

    let mut text = if json {
        serde_json::to_string(&doc)?
    } else {
        let (reasons, counts) = handoff_reasons(&conn, &task_id, &masker)?;
        redactions.counts.merge(&counts);
        render_markdown(&doc, &reasons)
    };
    text.push('\n');

    match out {
        Some(path) => {
            std::fs::write(path, text.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
            masked_eprintln!("wrote {} ({} bytes)", path.display(), text.len());
        }
        None => {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(text.as_bytes())?;
            stdout.flush()?;
        }
    }
    // Reported, not fatal: what was handed over is already masked.
    if let Err(e) = redact::record(
        &conn,
        &task_id,
        None,
        "review_packet",
        &redactions.counts,
        &redactions.evidence,
    ) {
        masked_eprintln!("warning: could not record secret_redacted: {e:#}");
    }
    Ok(0)
}

/// What a packet masked (RD-1): the kinds with their counts, and the codes
/// of the evidence rows whose label, output or diff had a secret in them,
/// in code order.
#[derive(Debug, Default)]
pub(crate) struct Redactions {
    pub(crate) counts: Counts,
    pub(crate) evidence: Vec<String>,
}

impl Redactions {
    /// Adds `counts`, and `code` when they are not empty.
    fn note(&mut self, code: &str, counts: &Counts) {
        if counts.is_empty() {
            return;
        }
        self.counts.merge(counts);
        if !self.evidence.iter().any(|c| c == code) {
            self.evidence.push(code.to_string());
            self.evidence.sort_by_key(|c| code_order(c));
        }
    }
}

/// Evidence codes sort by their number (`E2` before `E10`), then as text.
fn code_order(code: &str) -> (u64, String) {
    let digits: String = code.chars().filter(char::is_ascii_digit).collect();
    (digits.parse().unwrap_or(u64::MAX), code.to_string())
}

/// The Markdown `review packet TASK` prints, byte for byte, plus the
/// evidence rows it lists and what was masked in it.
pub(crate) fn packet_markdown(
    conn: &Connection,
    task_id: &str,
    masker: &Masker,
) -> Result<(String, Vec<state::Evidence>, Redactions)> {
    let (doc, rows, mut redactions) = packet_doc(conn, task_id, masker)?;
    let (reasons, counts) = handoff_reasons(conn, task_id, masker)?;
    redactions.counts.merge(&counts);
    let mut text = render_markdown(&doc, &reasons);
    text.push('\n');
    Ok((text, rows, redactions))
}

/// Scope §2.4: a codex reviewer cannot run thoth (E3a), so its input goes
/// into the review worktree as files. `<worktree>/.agent/review-input/` is
/// emptied and refilled with `packet.md` (the packet's Markdown, masked) and
/// `evidence/<CODE>.txt` for every row the packet lists, holding that row's
/// whole stored blob, lossily decoded and with its secrets masked (RD-1).
/// Nothing else is written. Returns what was masked in all of them, for the
/// caller's `secret_redacted` event: a secret in both packet.md and an
/// evidence file is counted once in each.
pub(crate) fn write_review_input(
    conn: &Connection,
    task_id: &str,
    worktree: &Path,
    masker: &Masker,
) -> Result<Redactions> {
    let dir = worktree.join(crate::adapter::codex::REVIEW_INPUT_DIR);
    // A reused review worktree may hold the previous review's input, and a
    // stale file there would be read as evidence of this one.
    match std::fs::remove_dir_all(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("clearing {}", dir.display())),
    }
    let evidence_dir = dir.join("evidence");
    std::fs::create_dir_all(&evidence_dir)
        .with_context(|| format!("creating {}", evidence_dir.display()))?;
    let (markdown, rows, mut redactions) = packet_markdown(conn, task_id, masker)?;
    std::fs::write(dir.join("packet.md"), markdown.as_bytes())
        .with_context(|| format!("writing {}", dir.join("packet.md").display()))?;
    for row in &rows {
        let blob = read_blob_masked(row, u64::MAX, masker);
        redactions.note(&row.code, &blob.counts);
        let path = evidence_dir.join(format!("{}.txt", row.code));
        std::fs::write(&path, blob.shown.as_bytes())
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(redactions)
}

/// The packet as JSON (what `--json` prints), the evidence rows it lists, and
/// what was masked in it (the handoff reasons only the Markdown shows aside).
fn packet_doc(
    conn: &Connection,
    task_id: &str,
    masker: &Masker,
) -> Result<(Value, Vec<state::Evidence>, Redactions)> {
    let task = state::get_task(conn, task_id)?
        .ok_or_else(|| anyhow::anyhow!("no such task {task_id}"))?;

    let spec = spec_view(&task.spec);

    let rows = state::list_evidence_for_task(conn, task_id)?;
    let mut redactions = Redactions::default();
    let mut evidence = Vec::new();
    let mut diffs = Vec::new();
    let mut protected_changes = Vec::new();
    for row in &rows {
        let (label, mut row_counts) = masker.mask_text(&row.label);
        let output_tail = if row.kind == "command" {
            let (body, _total, _verified) = read_blob(row, u64::MAX);
            // A failed command has an empty blob (PostToolUseFailure carries
            // no tool_response); its output is in `detail.error`.
            let body = if body.is_empty() {
                serde_json::from_str::<Value>(&row.detail)
                    .ok()
                    .and_then(|d| d["error"].as_str().map(str::to_string))
                    .unwrap_or_default()
            } else {
                body
            };
            let (tail, counts) = command_tail(&body, masker);
            row_counts.merge(&counts);
            Value::String(tail)
        } else {
            Value::Null
        };
        if row.kind == "diff" {
            // P7-2: what the control plane found changed among the protected
            // files, per diff evidence.
            let detail: Value = serde_json::from_str(&row.detail).unwrap_or(Value::Null);
            for c in crate::protected::changes_in_detail(&detail) {
                protected_changes.push(json!({
                    "run_id": row.run_id,
                    "code": row.code,
                    "path": c.path,
                    "status": c.status,
                    "kind": c.kind,
                }));
            }
            let (body, _total, verified) = read_blob(row, u64::MAX);
            let (body, counts) = masker.mask_text(&body);
            row_counts.merge(&counts);
            diffs.push(json!({
                "code": row.code,
                "run_id": row.run_id,
                "blob_verified": verified,
                "content": body,
            }));
        }
        redactions.note(&row.code, &row_counts);
        evidence.push(json!({
            "code": row.code,
            "kind": row.kind,
            "exit_code": detail_exit_code(&row.detail),
            "run_id": row.run_id,
            "label": label,
            "output_tail": output_tail,
        }));
    }

    // The newest `result_ingested` event carries the raw result.json; only its
    // `claims` are read. `summary` is deliberately never touched.
    let ingested = state::events_since(conn, 0, Some(task_id))?
        .into_iter()
        .rev()
        .find(|e| e.event_type == event_type::RESULT_INGESTED);
    let (claims, claims_run_id) = match ingested {
        Some(e) => {
            let detail: Value = serde_json::from_str(&e.detail).unwrap_or(Value::Null);
            let claims = detail["result_json"]["claims"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            (claims, e.run_id.map(Value::String).unwrap_or(Value::Null))
        }
        None => (Vec::new(), Value::Null),
    };

    let reviews: Vec<Value> = state::list_reviews_for_task(conn, task_id)?
        .into_iter()
        .map(|r| {
            json!({
                "verdict": r.verdict,
                "external": r.reviewer_run_id.is_none(),
                "findings": serde_json::from_str::<Value>(&r.findings).unwrap_or(Value::Null),
            })
        })
        .collect();

    let usage = crate::commands::status::run_usage_by_run(conn, task_id)?;
    let runs: Vec<Value> = state::list_runs_for_task(conn, task_id)?
        .into_iter()
        .map(|r| {
            let u = usage.get(&r.run_id);
            let effort = crate::adapter::effort::parse_launch(&r.spec).and_then(|l| l.effort);
            json!({
                "run_id": r.run_id,
                "role": r.role,
                "attempt": r.attempt,
                "effort": effort,
                // S3-9: the run this one continued, from its spec's handoff.
                "continued_from": crate::handoff::parse_handoff(&r.spec).map(|h| h.previous_run),
                "models_used": u.map(|u| u.models.clone()).unwrap_or(Value::Null),
                "subagents_spawned": u.map(|u| u.subagents_spawned.clone()).unwrap_or(Value::Null),
            })
        })
        .collect();

    let mut doc = json!({
        "task_id": task_id,
        "title": task.title,
        "status": task.status,
        "risk": task.risk,
        "runs": runs,
        "spec": spec,
        "evidence": evidence,
        "protected_changes": protected_changes,
        "diffs": diffs,
        "claims": claims,
        "claims_run_id": claims_run_id,
        "approvals": approvals(conn, task_id)?,
        "reviews": reviews,
    });
    // The rest (spec, claims, approvals, reviews, ...) is masked here; what
    // the loop above masked is markers already and is not counted again.
    let counts = masker.mask_value(&mut doc);
    redactions.counts.merge(&counts);
    Ok((doc, rows, redactions))
}

/// LESS-5: the approvals the reviewer should know about, oldest first: every
/// check approval since the task was created (a check approval names no task,
/// and any of them changes what verify ran), and every approval of this
/// task's protected changes.
fn approvals(conn: &Connection, task_id: &str) -> Result<Vec<Value>> {
    let events = state::events_since(conn, 0, None)?;
    let created = events
        .iter()
        .find(|e| e.event_type == event_type::TASK_CREATED && e.task_id.as_deref() == Some(task_id))
        .map(|e| e.seq)
        .unwrap_or(0);
    let mut out = Vec::new();
    for e in events {
        let check = e.event_type == event_type::CHECK_APPROVED && e.seq > created;
        let protected = e.event_type == event_type::PROTECTED_CHANGE_APPROVED
            && e.task_id.as_deref() == Some(task_id);
        if !check && !protected {
            continue;
        }
        let d: Value = serde_json::from_str(&e.detail).unwrap_or(Value::Null);
        let or_null = |key: &str| d.get(key).cloned().unwrap_or(Value::Null);
        out.push(json!({
            "seq": e.seq,
            "ts": e.ts,
            "type": e.event_type,
            "approved_by": or_null("approved_by"),
            "reason": or_null("reason"),
            "run_id": e.run_id,
            "check_id": if check { or_null("check_id") } else { Value::Null },
            "source": if check { or_null("source") } else { Value::Null },
            "changes": if check { json!([]) } else { d.get("changes").cloned().unwrap_or_else(|| json!([])) },
            "live_runs": d.get("live_runs").cloned().unwrap_or_else(|| json!([])),
        }));
    }
    Ok(out)
}

/// S3-9: why each run that continued a stopped run did so, by run id, masked,
/// with what was masked. Only the Markdown shows it; `--json` carries
/// `continued_from` alone.
fn handoff_reasons(
    conn: &Connection,
    task_id: &str,
    masker: &Masker,
) -> Result<(std::collections::HashMap<String, String>, Counts)> {
    let mut counts = Counts::default();
    let reasons = state::list_runs_for_task(conn, task_id)?
        .into_iter()
        .filter_map(|r| crate::handoff::parse_handoff(&r.spec).map(|h| (r.run_id, h.reason)))
        .map(|(run_id, reason)| {
            let (reason, c) = masker.mask_text(&reason);
            counts.merge(&c);
            (run_id, reason)
        })
        .collect();
    Ok((reasons, counts))
}

/// The five spec fields the reviewer is judging against, verbatim. A spec that
/// is not the expected shape yields empty fields rather than an error: the
/// packet is for reading, and `task create` already validated the spec.
fn spec_view(spec_json: &str) -> Value {
    let raw: Value = serde_json::from_str(spec_json).unwrap_or(Value::Null);
    let mut view = serde_json::Map::new();
    view.insert(
        "goal".to_string(),
        raw.get("goal").cloned().unwrap_or(Value::String(String::new())),
    );
    for (key, _) in SPEC_LIST_KEYS {
        view.insert(
            key.to_string(),
            raw.get(key).cloned().unwrap_or_else(|| json!([])),
        );
    }
    Value::Object(view)
}

/// The last `TAIL_LINES` lines of a command's output, masked. The whole body
/// is masked first: a tail cut through a private key block would leave its
/// last lines unrecognisable.
fn command_tail(body: &str, masker: &Masker) -> (String, Counts) {
    let (masked, counts) = masker.mask_text(body);
    (tail_lines(&masked, TAIL_LINES), counts)
}

/// The last `n` lines of `text`, joined with `\n`.
fn tail_lines(text: &str, n: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// A code fence that cannot be closed early by `content`: one backtick longer
/// than the longest run inside it, and at least three.
fn fence_for(content: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in content.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

fn fenced(info: &str, content: &str) -> String {
    let fence = fence_for(content);
    let sep = if content.ends_with('\n') { "" } else { "\n" };
    format!("{fence}{info}\n{content}{sep}{fence}\n")
}

fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

fn string_list(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|x| s(x).to_string()).collect())
        .unwrap_or_default()
}

fn render_markdown(doc: &Value, handoff_reasons: &std::collections::HashMap<String, String>) -> String {
    let mut m = String::new();

    // (1) identity
    m.push_str(&format!("# Review packet: {}\n\n", s(&doc["task_id"])));
    m.push_str(&format!(
        "- title: {}\n- status: {}\n- risk: {}\n\n",
        s(&doc["title"]),
        s(&doc["status"]),
        s(&doc["risk"])
    ));

    // runs: which models ran and whether any subagent was spawned
    m.push_str("## Runs\n\n");
    let runs = doc["runs"].as_array().cloned().unwrap_or_default();
    if runs.is_empty() {
        m.push_str("No runs recorded for this task.\n\n");
    }
    for r in &runs {
        let models = match r["models_used"].as_object() {
            Some(o) if !o.is_empty() => o
                .iter()
                .map(|(k, v)| format!("{k} {}", v.as_i64().unwrap_or(0)))
                .collect::<Vec<_>>()
                .join(", "),
            _ => "unknown".to_string(),
        };
        let subagents = r["subagents_spawned"]
            .as_i64()
            .map(|n| n.to_string())
            .unwrap_or_else(|| "unknown".to_string());
        m.push_str(&format!(
            "- {} | {} a{} | models (output tokens): {} | subagents: {} | effort: {}",
            s(&r["run_id"]),
            s(&r["role"]),
            r["attempt"].as_i64().unwrap_or(0),
            models,
            subagents,
            r["effort"].as_str().unwrap_or("-")
        ));
        // S3-9: a run that continued a stopped one says which, and why.
        if let Some(previous) = r["continued_from"].as_str() {
            let reason = handoff_reasons.get(s(&r["run_id"])).map(String::as_str).unwrap_or("-");
            m.push_str(&format!(
                " | continued from {previous} ({})",
                reason.replace(['\r', '\n'], " ")
            ));
        }
        m.push('\n');
        if let Some(n) = r["subagents_spawned"].as_i64().filter(|n| *n > 0) {
            m.push_str(&format!(
                "  WARNING: run {} spawned {n} subagent(s); the worker was not supposed to delegate.\n",
                s(&r["run_id"])
            ));
        }
    }
    if !runs.is_empty() {
        m.push('\n');
    }

    // (2) spec
    m.push_str("## Spec\n\n");
    m.push_str(&format!("Goal: {}\n\n", s(&doc["spec"]["goal"])));
    for (key, heading) in SPEC_LIST_KEYS {
        m.push_str(&format!("{heading}:\n"));
        let items = string_list(&doc["spec"][key]);
        if items.is_empty() {
            m.push_str("- (none)\n");
        }
        for item in items {
            m.push_str(&format!("- {item}\n"));
        }
        m.push('\n');
    }

    // (3) evidence index
    m.push_str("## Evidence index\n\n");
    let evidence = doc["evidence"].as_array().cloned().unwrap_or_default();
    if evidence.is_empty() {
        m.push_str("No evidence recorded for this task.\n\n");
    }
    for e in &evidence {
        m.push_str(&format!(
            "- {} | {} | exit {} | run {} | {}\n",
            s(&e["code"]),
            s(&e["kind"]),
            e["exit_code"]
                .as_i64()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".to_string()),
            e["run_id"].as_str().unwrap_or("-"),
            s(&e["label"]),
        ));
        if let Some(tail) = e["output_tail"].as_str() {
            if tail.is_empty() {
                m.push_str("  (no stored output)\n");
            } else {
                for line in fenced("", tail).lines() {
                    m.push_str(&format!("  {line}\n"));
                }
            }
        }
    }
    if !evidence.is_empty() {
        m.push('\n');
    }

    // P7-2: protected files the candidate changed, ahead of the diffs so the
    // reviewer reads them knowing where to look hardest
    m.push_str("## Protected files changed\n\n");
    m.push_str(
        "Existing test files and check input files that a diff modified or deleted, existing Rust source files whose inline test region was changed or deleted (inline_test), and added or modified files named like a word of a check's command. Worked out by the control plane from each diff; the worker does not have to declare them, and the commander approves them with thoth checks approve --run. Whether a change weakened a check is for you to judge, file by file.\n\n",
    );
    let protected = doc["protected_changes"].as_array().cloned().unwrap_or_default();
    if protected.is_empty() {
        m.push_str("- (none)\n");
    }
    for p in &protected {
        m.push_str(&format!(
            "- {} ({}, {}) | {} | run {}\n",
            s(&p["path"]),
            s(&p["status"]),
            s(&p["kind"]),
            s(&p["code"]),
            p["run_id"].as_str().unwrap_or("-"),
        ));
    }
    m.push_str("\nWhat the worker declared (its optional protected_change claims):\n");
    let declared: Vec<&Value> = doc["claims"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter(|c| s(&c["kind"]) == crate::protected::CLAIM_KIND)
                .collect()
        })
        .unwrap_or_default();
    if declared.is_empty() {
        m.push_str("- (none)\n");
    }
    for c in declared {
        m.push_str(&format!("- {}\n", s(&c["text"])));
    }
    m.push('\n');

    // (4) diffs
    m.push_str("## Diffs\n\n");
    let diffs = doc["diffs"].as_array().cloned().unwrap_or_default();
    if diffs.is_empty() {
        m.push_str("No diff evidence recorded for this task.\n\n");
    }
    for d in &diffs {
        m.push_str(&format!(
            "### {} (run {}, blob_verified: {})\n\n",
            s(&d["code"]),
            d["run_id"].as_str().unwrap_or("-"),
            d["blob_verified"].as_bool().unwrap_or(false),
        ));
        m.push_str(&fenced("diff", s(&d["content"])));
        m.push('\n');
    }

    // (5) claims
    m.push_str("## Worker claims\n\n");
    m.push_str(
        "The worker's own summary is left out of this packet on purpose: judge from the evidence, not from its prose.\n\n",
    );
    let claims = doc["claims"].as_array().cloned().unwrap_or_default();
    match doc["claims_run_id"].as_str() {
        None => m.push_str("No result ingested for this task, so there are no claims.\n\n"),
        Some(run_id) => {
            m.push_str(&format!("Claims from run {run_id}:\n"));
            if claims.is_empty() {
                m.push_str("- (none)\n");
            }
            for c in &claims {
                let cited = string_list(&c["evidence"]);
                m.push_str(&format!(
                    "- [{}] {} (evidence: {})\n",
                    s(&c["kind"]),
                    s(&c["text"]),
                    if cited.is_empty() { "-".to_string() } else { cited.join(", ") }
                ));
                // RES-1a: a research claim's sources, one line each.
                for src in c["sources"].as_array().into_iter().flatten() {
                    m.push_str(&format!("  - {} — \"{}\"\n", s(&src["url"]), s(&src["quote"])));
                }
            }
            m.push('\n');
        }
    }

    // LESS-5: who approved checks and protected changes, and why
    m.push_str("## Approvals during this task\n\n");
    m.push_str(
        "Check approvals since this task was created, and approvals of this task's protected file changes. By: human_terminal is a person at a terminal; commander_cli and commander_mcp are the commander (its shell, or the MCP tool). \"while run X was running\" names runs that had not ended when the approval was made: any of them could have run the command itself.\n\n",
    );
    let approvals = doc["approvals"].as_array().cloned().unwrap_or_default();
    if approvals.is_empty() {
        m.push_str("- (none)\n");
    }
    for a in &approvals {
        let what = if s(&a["type"]) == event_type::PROTECTED_CHANGE_APPROVED {
            let changes: Vec<String> = a["changes"]
                .as_array()
                .map(|cs| {
                    cs.iter()
                        .map(|c| format!("{} ({}, {})", s(&c["path"]), s(&c["status"]), s(&c["kind"])))
                        .collect()
                })
                .unwrap_or_default();
            format!(
                "protected changes of run {}: {}",
                a["run_id"].as_str().unwrap_or("-"),
                changes.join(", ")
            )
        } else {
            let run = a["run_id"].as_str().map(|r| format!(", run {r}")).unwrap_or_default();
            format!("check {} ({}{run})", s(&a["check_id"]), s(&a["source"]))
        };
        let live = string_list(&a["live_runs"]);
        let while_note = match live.len() {
            0 => String::new(),
            1 => format!(" (while run {} was running)", live[0]),
            _ => format!(" (while runs {} were running)", live.join(", ")),
        };
        m.push_str(&format!(
            "- {} | {what} | by {} | reason: {}{while_note}\n",
            s(&a["ts"]),
            a["approved_by"].as_str().unwrap_or("unknown"),
            a["reason"].as_str().map(|r| r.replace(['\r', '\n'], " ")).unwrap_or_else(|| "-".to_string()),
        ));
    }
    m.push('\n');

    // (6) reviews
    m.push_str("## Reviews recorded so far\n\n");
    let reviews = doc["reviews"].as_array().cloned().unwrap_or_default();
    if reviews.is_empty() {
        m.push_str("No review recorded for this task.\n");
    }
    for (i, r) in reviews.iter().enumerate() {
        m.push_str(&format!(
            "### Review {}: {} ({})\n\n",
            i + 1,
            s(&r["verdict"]),
            if r["external"].as_bool().unwrap_or(false) {
                "external, no reviewer run"
            } else {
                "reviewer run"
            }
        ));
        let findings = serde_json::to_string_pretty(&r["findings"]).unwrap_or_default();
        m.push_str(&fenced("json", &findings));
        m.push('\n');
    }
    while m.ends_with("\n\n") {
        m.pop();
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_lines_keeps_the_last_n() {
        let text: String = (1..=20).map(|i| format!("l{i}\n")).collect();
        assert_eq!(tail_lines(&text, 3), "l18\nl19\nl20");
        assert_eq!(tail_lines("a\nb", 15), "a\nb");
        assert_eq!(tail_lines("", 15), "");
    }

    #[test]
    fn fence_outgrows_any_backtick_run_in_the_content() {
        assert_eq!(fence_for("plain"), "```");
        assert_eq!(fence_for("has ``` inside"), "````");
        assert_eq!(fence_for("`````x"), "``````");
    }

    #[test]
    fn spec_view_defaults_missing_fields() {
        let v = spec_view(r#"{"goal":"g","acceptance_criteria":["AC1: x"]}"#);
        assert_eq!(v["goal"], "g");
        assert_eq!(v["constraints"], json!([]));
        assert_eq!(v["acceptance_criteria"][0], "AC1: x");
        assert_eq!(spec_view("not json")["goal"], "");
    }

    /// RD-1 (AC3): a private key across lines 10 to 20 of a 30-line output
    /// becomes one marker line before the last 15 lines are taken.
    #[test]
    fn the_tail_is_taken_after_masking() {
        let mut lines: Vec<String> = (1..=30).map(|i| format!("line-{i:02}")).collect();
        let mut block = vec![["-----BEGIN ", "RSA PRIVATE KEY-----"].concat()];
        block.extend((1..=9).map(|i| format!("QUFBQUFBQUFBQUFBQUFBQUFBdummy{i}")));
        block.push(["-----END ", "RSA PRIVATE KEY-----"].concat());
        lines.splice(9..20, block);
        let text = lines.join("\n");

        let (tail, counts) = command_tail(&text, &Masker::from_pairs(&[]));
        let mut expected: Vec<String> = (6..=9).map(|i| format!("line-{i:02}")).collect();
        expected.push("[redacted:private-key]".to_string());
        expected.extend((21..=30).map(|i| format!("line-{i:02}")));
        assert_eq!(tail, expected.join("\n"));
        assert_eq!(counts.to_json(), json!({"private-key": 1}));
    }

    #[test]
    fn redactions_list_each_code_once_in_number_order() {
        let mut r = Redactions::default();
        let (_, one) = Masker::from_pairs(&[("dummy-value-1", "test")]).mask_text("dummy-value-1");
        r.note("E10", &one);
        r.note("E2", &one);
        r.note("E10", &one);
        r.note("E3", &Counts::default());
        assert_eq!(r.evidence, ["E2", "E10"]);
        assert_eq!(r.counts.to_json(), json!({"test": 3}));
    }
}
