//! RVI-1: the `review_delivery` reviewer scenario shared by fake-codex and
//! fake-agy. It reads what thoth put in `.agent/review-input/` (packet.md,
//! every `evidence/<CODE>.txt` and the delivered `result.json`), judges each
//! acceptance criterion from those inputs and the prompt's instructions, and
//! records what it consumed. Nothing here judges like a model: each criterion
//! carries tags in its text that say what to look for.
//!
//! - `[mapping]`: the delivery must hold an `ac_test_map` object (anywhere in
//!   result.json) naming a test for every criterion id, and each test's name
//!   (after its last `::`) must appear in some evidence file. Absent: unmet.
//!   A name no evidence file holds: unmet, the claim is not trusted.
//! - `[check:TEXT]`: an index row labelled `verify: ` (not `verify@base: `)
//!   for the run the packet's claims come from, whose label contains TEXT,
//!   with `exit 0` and no recorded execution error (neither an
//!   `execution error:` line under it in packet.md nor one at the top of its
//!   evidence file: exit 0 alone is not a pass). An empty output counts only
//!   when the prompt carries the
//!   silent-check rule. `[output:TEXT]` also needs TEXT in that row's file;
//!   `[scope:full-linux-suite]` is unmet when the prompt carries the
//!   linux-container scope rule.
//! - no tag: met, citing the first code in the index.
//!
//! REV-3: each entry of the prompt's `## Required checks not run` section
//! becomes a `check_not_run` extra finding, and any makes the verdict REJECT,
//! as the prompt's rule tells a reviewer.
//!
//! `FAKE_REVIEW_CAPTURE=<path>` writes one JSON object: the prompt, the
//! packet, the evidence files and the result.json text as read, the parsed
//! result, the mapping found and where, and each decision. Unset: nothing.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const SCENARIO: &str = "review_delivery";

/// The phrase that marks the silent-check rule in a prompt.
const SILENT_RULE_MARK: &str = "even when both its output streams are empty";
/// The phrase that marks the linux-container scope rule in a prompt.
const SCOPE_RULE_MARK: &str = "not the whole Linux cargo test suite";
/// The phrase that marks the result-file guidance in a prompt.
const RESULT_RULE_MARK: &str = "not only in the claims";

/// What the fake read.
pub struct Input {
    pub packet: String,
    pub evidence: Vec<(String, String)>,
    pub result_text: Option<String>,
}

fn input_dir() -> PathBuf {
    Path::new(".agent").join("review-input")
}

/// Reads packet.md, every evidence file the packet's index names and
/// result.json through `read` (a harness's own file tool).
pub fn read_with(mut read: impl FnMut(&Path) -> Option<String>) -> Input {
    let dir = input_dir();
    let packet = read(&dir.join("packet.md")).unwrap_or_default();
    let evidence = index_rows(&packet)
        .into_iter()
        .map(|r| {
            let text = read(&dir.join("evidence").join(format!("{}.txt", r.code))).unwrap_or_default();
            (r.code, text)
        })
        .collect();
    let result_text = read(&dir.join("result.json"));
    Input { packet, evidence, result_text }
}

struct Row {
    code: String,
    exit: String,
    run: String,
    label: String,
    no_output: bool,
    /// The `execution error:` line under the entry, when there is one.
    error: Option<String>,
}

/// The line packet.md shows under a check that did not complete cleanly.
const EXECUTION_ERROR_LINE: &str = "execution error: ";
/// How a review-input evidence file of such a check begins.
const EXECUTION_ERROR_FILE: &str = "[thoth: this check recorded an execution error";

/// The evidence index lines: `- CODE | kind | exit N | run R | label`, and
/// from the indented lines under each, whether it says `(no stored output)`
/// and its `execution error:`, if any.
fn index_rows(packet: &str) -> Vec<Row> {
    let lines: Vec<&str> = packet.lines().collect();
    let mut out = Vec::new();
    let mut in_index = false;
    for (i, line) in lines.iter().enumerate() {
        if line.starts_with("## ") {
            in_index = *line == "## Evidence index";
            continue;
        }
        if !in_index {
            continue;
        }
        let Some(rest) = line.strip_prefix("- ") else { continue };
        let parts: Vec<&str> = rest.splitn(5, " | ").collect();
        if parts.len() < 5 || !parts[0].starts_with('E') {
            continue;
        }
        let under: Vec<&str> = lines[i + 1..]
            .iter()
            .take_while(|l| l.starts_with("  "))
            .map(|l| l.trim())
            .collect();
        out.push(Row {
            code: parts[0].to_string(),
            exit: parts[2].trim_start_matches("exit ").to_string(),
            run: parts[3].trim_start_matches("run ").to_string(),
            label: parts[4].to_string(),
            no_output: under.contains(&"(no stored output)"),
            // the packet puts it first, before any output
            error: under.first().and_then(|l| l.strip_prefix(EXECUTION_ERROR_LINE)).map(str::to_string),
        });
    }
    out
}

/// The run the packet's claims come from.
fn claims_run(packet: &str) -> Option<String> {
    packet
        .lines()
        .find_map(|l| l.strip_prefix("Claims from run "))
        .map(|r| r.trim_end_matches(':').to_string())
}

/// The criterion ids from the prompt's `Every id in "criteria"` line, and
/// each one's text from its `## Acceptance criteria` section.
fn criteria(prompt: &str) -> Vec<(String, String)> {
    let ids: Vec<String> = prompt
        .lines()
        .find_map(|l| l.strip_prefix("Every id in \"criteria\" must be one of: "))
        .map(|rest| rest.trim_end_matches('.').split(", ").map(String::from).collect())
        .unwrap_or_default();
    ids.into_iter()
        .map(|id| {
            let text = prompt
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{id} — ")))
                .unwrap_or_default()
                .to_string();
            (id, text)
        })
        .collect()
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("[{name}:");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(']')? + start;
    Some(&text[start..end])
}

/// The first `ac_test_map` object anywhere in `v`, with its JSON path.
fn find_mapping(v: &Value, at: &str) -> Option<(String, Value)> {
    match v {
        Value::Object(map) => {
            if let Some(m @ Value::Object(_)) = map.get("ac_test_map") {
                return Some((format!("{at}.ac_test_map"), m.clone()));
            }
            map.iter().find_map(|(k, v)| find_mapping(v, &format!("{at}.{k}")))
        }
        Value::Array(items) => items
            .iter()
            .enumerate()
            .find_map(|(i, v)| find_mapping(v, &format!("{at}[{i}]"))),
        _ => None,
    }
}

struct Decision {
    met: bool,
    evidence: Vec<String>,
    note: String,
}

fn met(evidence: Vec<String>) -> Decision {
    Decision { met: true, evidence, note: String::new() }
}

fn unmet(evidence: Vec<String>, note: String) -> Decision {
    Decision { met: false, evidence, note }
}

/// REV-3: the `CHECK_ID (REASON)` entries of the prompt's `## Required
/// checks not run` section (`- ` lines), in order; none without it.
fn required_not_run(prompt: &str) -> Vec<String> {
    prompt
        .lines()
        .skip_while(|l| *l != "## Required checks not run")
        .skip(1)
        .take_while(|l| !l.starts_with("## "))
        .filter_map(|l| l.strip_prefix("- "))
        .map(str::to_string)
        .collect()
}

/// The review document, and the capture of what was consumed.
pub fn judge(prompt: &str, input: &Input) -> (Value, Value) {
    let rows = index_rows(&input.packet);
    let candidate = claims_run(&input.packet);
    let result: Option<Value> = input.result_text.as_deref().and_then(|t| serde_json::from_str(t).ok());
    let mapping = result.as_ref().and_then(|r| find_mapping(r, "$"));
    let ids: Vec<String> = criteria(prompt).into_iter().map(|(id, _)| id).collect();
    let file_of = |code: &str| {
        input.evidence.iter().find(|(c, _)| c == code).map(|(_, t)| t.as_str()).unwrap_or("")
    };
    let mut decisions = Vec::new();
    let mut criteria_out = Vec::new();
    for (id, text) in criteria(prompt) {
        let d = if text.contains("[mapping]") {
            match (&result, &mapping) {
                (None, _) => unmet(vec![], "result.json is missing or not JSON".into()),
                (Some(_), None) => unmet(vec![], "result.json carries no ac_test_map: the delivery has no AC-to-test mapping".into()),
                (Some(_), Some(_)) if !prompt.contains(RESULT_RULE_MARK) => {
                    unmet(vec![], "the prompt gave no guidance on reading result.json".into())
                }
                (Some(_), Some((_, m))) => {
                    let mut cited = Vec::new();
                    let mut missing = Vec::new();
                    for want in &ids {
                        let test = m[want].as_str().unwrap_or_default();
                        let name = test.rsplit("::").next().unwrap_or_default();
                        match input.evidence.iter().find(|(_, t)| !name.is_empty() && t.contains(name)) {
                            Some((code, _)) => {
                                if !cited.contains(code) {
                                    cited.push(code.clone());
                                }
                            }
                            None => missing.push(format!("{want} -> {test:?}")),
                        }
                    }
                    if missing.is_empty() {
                        met(cited)
                    } else {
                        unmet(cited, format!("the mapping names tests no evidence file holds: {}", missing.join(", ")))
                    }
                }
            }
        } else if let Some(want) = tag(&text, "check") {
            let candidates: Vec<&Row> = rows
                .iter()
                .filter(|r| r.label.starts_with("verify: ") && r.label.contains(want))
                .filter(|r| candidate.as_deref() == Some(r.run.as_str()))
                .collect();
            let seen: Vec<String> = rows
                .iter()
                .filter(|r| r.label.contains(want))
                .map(|r| r.code.clone())
                .collect();
            // exit 0 alone is not a pass: a recorded execution error, in the
            // packet or at the top of the row's evidence file, means the
            // check did not complete cleanly
            let error_of = |r: &Row| -> Option<String> {
                r.error.clone().or_else(|| {
                    let file = file_of(&r.code);
                    file.starts_with(EXECUTION_ERROR_FILE).then(|| file.lines().next().unwrap_or_default().to_string())
                })
            };
            let errored: Vec<String> = candidates
                .iter()
                .filter(|r| r.exit == "0")
                .filter_map(|r| error_of(r).map(|e| format!("{} exited 0 but recorded an execution error: {e}", r.code)))
                .collect();
            match candidates.iter().find(|r| r.exit == "0" && error_of(r).is_none()) {
                None if !errored.is_empty() => unmet(seen, errored.join("; ")),
                None => unmet(seen, format!("no candidate verify row for {want:?} completed with exit 0")),
                Some(r) if r.no_output && !prompt.contains(SILENT_RULE_MARK) => {
                    unmet(vec![r.code.clone()], format!("{} has no stored output", r.code))
                }
                Some(r) => match tag(&text, "output") {
                    Some(out) if !file_of(&r.code).contains(out) => {
                        unmet(vec![r.code.clone()], format!("{} does not show the required output {out:?}", r.code))
                    }
                    _ if tag(&text, "scope") == Some("full-linux-suite") && prompt.contains(SCOPE_RULE_MARK) => unmet(
                        vec![r.code.clone()],
                        format!("{} is linux-container, which covers Linux clippy and tests/install_scripts.rs, not the whole Linux suite", r.code),
                    ),
                    _ => met(vec![r.code.clone()]),
                },
            }
        } else {
            met(rows.first().map(|r| r.code.clone()).into_iter().collect())
        };
        let mut c = json!({"id": id, "verdict": if d.met { "met" } else { "unmet" },
            "evidence": d.evidence, "note": d.note});
        if d.met {
            c["fix_size"] = json!("none");
            c["category"] = json!("none");
        } else {
            c["fix_size"] = json!("S");
            c["category"] = json!("evidence");
        }
        decisions.push(json!({"id": id, "met": d.met, "evidence": c["evidence"], "note": c["note"]}));
        criteria_out.push(c);
    }
    // REV-3: one check_not_run extra finding per check the prompt's
    // `## Required checks not run` section lists, which makes it a REJECT.
    let not_run = required_not_run(prompt);
    let extras: Vec<Value> = not_run
        .iter()
        .map(|line| json!({"severity": "major", "note": format!("required check did not run: {line}"),
            "evidence": [], "fix_size": "S", "category": "check_not_run"}))
        .collect();
    let all_met = decisions.iter().all(|d| d["met"] == true) && extras.is_empty();
    let doc = json!({"verdict": if all_met { "PASS" } else { "REJECT" },
        "findings": {"criteria": criteria_out, "extra_findings": extras}});
    let capture = json!({
        "prompt": prompt,
        "packet": input.packet,
        "evidence": input.evidence.iter().map(|(c, t)| (c.clone(), Value::String(t.clone()))).collect::<serde_json::Map<_, _>>(),
        "result_text": input.result_text,
        "result": result,
        "mapping": mapping.as_ref().map(|(_, m)| m.clone()),
        "mapping_path": mapping.as_ref().map(|(p, _)| p.clone()),
        "candidate_run": candidate,
        "decisions": decisions,
    });
    (doc, capture)
}

/// Writes the capture to `FAKE_REVIEW_CAPTURE`, when set.
pub fn record(capture: &Value) {
    if let Some(path) = std::env::var_os("FAKE_REVIEW_CAPTURE") {
        let _ = std::fs::write(path, serde_json::to_string_pretty(capture).unwrap_or_default());
    }
}
