//! S3-9: the handoff file a re-dispatch writes when the task's previous run
//! stopped before it finished (quota, timeout, crash).
//!
//! The file holds only what Thoth can work out itself - the run rows, their
//! events and evidence rows, and git's output in the worktree - never the
//! previous worker's stream, stderr, result.json or summary: a worker's own
//! account of its work is not evidence. The new worker's prompt carries only
//! the path (`PATH`), and dispatch records a `handoff_written` event with the
//! file's hash.
//!
//! Neither `gather`/`render_facts` nor `write` looks at the task's status: deciding whether a
//! dispatch is a handoff is the caller's job (`commands::dispatch`, and later
//! the runner's automatic takeover, S3-10).

use crate::db::global;
use crate::db::state::{self, Run};
use crate::redact::{Counts, Masker};
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Where the file sits in the worktree, relative to its root. Also the value
/// of the run spec's `handoff.path` and of the event's `path`.
pub const PATH: &str = ".agent/handoff.md";
/// The run directory's copy.
pub const RUN_DIR_FILE: &str = "handoff.md";

const LOG_LINES: usize = 50;
const STATUS_LINES: usize = 200;
const DIFF_STAT_LINES: usize = 200;
const EVIDENCE_ROWS: usize = 100;

/// The run this handoff is for: where it goes and what it runs with.
pub struct Target<'a> {
    pub profile: &'a str,
    pub harness: &'a str,
    pub model: &'a str,
}

/// The `handoff` object in a run's spec. `TaskSpec` ignores the key; only
/// `parse_handoff` reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HandoffRef {
    pub previous_run: String,
    pub reason: String,
    pub from_profile: String,
    pub from_harness: String,
    pub path: String,
}

/// Reads the `handoff` key back out of a run's spec. Never fails: a missing
/// key, an unparseable spec and a value of the wrong shape are all "none".
pub fn parse_handoff(spec_json: &str) -> Option<HandoffRef> {
    let v: serde_json::Value = serde_json::from_str(spec_json).ok()?;
    serde_json::from_value(v.get("handoff")?.clone()).ok()
}

/// Why the previous run stopped: its `interrupted_reason`, else its status.
pub fn reason(previous: &Run) -> String {
    previous
        .interrupted_reason
        .clone()
        .filter(|r| !r.trim().is_empty())
        .unwrap_or_else(|| previous.status.clone())
}

/// The harness the previous run was launched with: its `run_started` event's
/// `harness`, else its profile's, else `unknown`.
pub fn previous_harness(conn: &Connection, gconn: &Connection, previous: &Run) -> Result<String> {
    let from_event = state::events_since(conn, 0, Some(&previous.task_id))?
        .into_iter()
        .filter(|e| {
            e.event_type == crate::model::event_type::RUN_STARTED
                && e.run_id.as_deref() == Some(previous.run_id.as_str())
        })
        .filter_map(|e| {
            let d: serde_json::Value = serde_json::from_str(&e.detail).ok()?;
            d["harness"].as_str().filter(|h| !h.is_empty()).map(str::to_string)
        })
        .next_back();
    if let Some(h) = from_event {
        return Ok(h);
    }
    Ok(global::get_profile(gconn, &previous.account)?
        .map(|p| p.harness)
        .unwrap_or_else(|| "unknown".to_string()))
}

/// One command evidence row as the handoff lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceLine {
    pub code: String,
    pub exit_code: Option<i64>,
    pub label: String,
}

/// Everything the file says, gathered. `render_facts` turns it into text and
/// has no other input, so the unit tests can hand it any facts they like.
#[derive(Debug, Clone)]
pub struct Facts {
    pub task_id: String,
    pub attempt: i64,
    pub previous_run: String,
    pub previous_profile: String,
    pub previous_harness: String,
    pub previous_model: String,
    pub previous_status: String,
    pub previous_reason: Option<String>,
    pub previous_ended_at: Option<String>,
    pub to_profile: String,
    pub to_harness: String,
    pub to_model: String,
    pub branch: String,
    pub base_commit: String,
    /// Each git output, or git's error.
    pub log: Result<String, String>,
    pub status: Result<String, String>,
    pub diff_stat: Result<String, String>,
    pub evidence: Vec<EvidenceLine>,
}

fn code_number(code: &str) -> u64 {
    code.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(u64::MAX)
}

/// The command evidence of every run of `previous`'s attempt up to and
/// including `previous`, by code number.
fn evidence_lines(conn: &Connection, previous: &Run) -> Result<Vec<EvidenceLine>> {
    let runs: Vec<String> = state::list_runs_for_task(conn, &previous.task_id)?
        .into_iter()
        .filter(|r| r.attempt == previous.attempt && r.run_id <= previous.run_id)
        .map(|r| r.run_id)
        .collect();
    let mut lines: Vec<EvidenceLine> = state::list_evidence_for_task(conn, &previous.task_id)?
        .into_iter()
        .filter(|e| e.kind == "command")
        .filter(|e| e.run_id.as_ref().is_some_and(|r| runs.contains(r)))
        .map(|e| EvidenceLine {
            exit_code: serde_json::from_str::<serde_json::Value>(&e.detail)
                .ok()
                .and_then(|d| d["exit_code"].as_i64()),
            code: e.code,
            label: e.label,
        })
        .collect();
    lines.sort_by_key(|l| code_number(&l.code));
    Ok(lines)
}

fn git(result: Result<String>) -> Result<String, String> {
    result.map_err(|e| format!("{e:#}"))
}

/// Reads the facts for a handoff from `previous` to `to`. Git runs in the
/// previous run's worktree; a git failure is recorded in the facts, not
/// returned. Only a database read can fail this.
pub fn gather(
    conn: &Connection,
    gconn: &Connection,
    previous: &Run,
    to: &Target<'_>,
) -> Result<Facts> {
    let worktree = Path::new(&previous.worktree);
    Ok(Facts {
        task_id: previous.task_id.clone(),
        attempt: previous.attempt,
        previous_run: previous.run_id.clone(),
        previous_profile: previous.account.clone(),
        previous_harness: previous_harness(conn, gconn, previous)?,
        previous_model: previous.model.clone(),
        previous_status: previous.status.clone(),
        previous_reason: previous.interrupted_reason.clone(),
        previous_ended_at: previous.ended_at.clone(),
        to_profile: to.profile.to_string(),
        to_harness: to.harness.to_string(),
        to_model: to.model.to_string(),
        branch: crate::worktree::branch_name(&previous.task_id, previous.attempt),
        base_commit: previous.base_commit.clone(),
        log: git(crate::worktree::log_oneline_since(worktree, &previous.base_commit)),
        status: git(crate::worktree::status_short(worktree)),
        diff_stat: git(crate::worktree::diff_stat_head(worktree)),
        evidence: evidence_lines(conn, previous)?,
    })
}

/// CR and LF become spaces, so a label or a commit title cannot start a new
/// line (and with it a new heading) in the file.
fn one_line(text: &str) -> String {
    text.replace(['\r', '\n'], " ")
}

/// The first `n` lines of `text`, then `... (K more lines)` when `K` lines
/// were left out. That extra line is not one of the `n`.
pub fn cap_lines(text: &str, n: usize) -> Vec<String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<String> = lines.iter().take(n).map(|l| l.to_string()).collect();
    if lines.len() > n {
        out.push(format!("... ({} more lines)", lines.len() - n));
    }
    out
}

fn push_git(
    s: &mut String,
    title: &str,
    output: &Result<String, String>,
    n: usize,
    scrub: bool,
    masker: &Masker,
    counts: &mut Counts,
) {
    let mut mask = |text: &str| {
        let (masked, c) = masker.mask_text(text);
        counts.merge(&c);
        masked
    };
    s.push_str(&format!("`{title}`:\n\n"));
    match output {
        Err(e) => s.push_str(&format!("(unavailable: {})\n", one_line(mask(e).trim()))),
        Ok(text) if text.trim().is_empty() => s.push_str("(nothing)\n"),
        Ok(text) => {
            // An indented block: git's own lines cannot be read as headings.
            for line in cap_lines(mask(text).trim_end(), n) {
                let line = if scrub { one_line(&line) } else { line };
                s.push_str("    ");
                s.push_str(&line);
                s.push('\n');
            }
        }
    }
    s.push('\n');
}

/// The file's text and what was masked in it. RD-1: every value taken from
/// `f` is masked before `one_line` and `cap_lines`, so a secret with a line
/// break in it is still found.
pub fn render_facts(f: &Facts, masker: &Masker) -> (String, Counts) {
    let mut counts = Counts::default();
    let mut mask = |text: &str| {
        let (masked, c) = masker.mask_text(text);
        counts.merge(&c);
        masked
    };
    let dash = |v: &Option<String>| v.clone().filter(|v| !v.is_empty()).unwrap_or_else(|| "-".to_string());
    let previous_profile = mask(&f.previous_profile);
    let previous_harness = mask(&f.previous_harness);
    let previous_model = mask(&f.previous_model);
    let previous_status = mask(&f.previous_status);
    let previous_reason = mask(&dash(&f.previous_reason));
    let previous_ended_at = mask(&dash(&f.previous_ended_at));
    let to_profile = mask(&f.to_profile);
    let to_harness = mask(&f.to_harness);
    let to_model = mask(&f.to_model);
    let branch = mask(&f.branch);
    let base_commit = mask(&f.base_commit);
    let mut s = String::new();
    s.push_str("# Handoff\n\n");
    s.push_str(&format!("- task: {}\n- attempt: {}\n\n", f.task_id, f.attempt));

    s.push_str("## Why\n\n");
    s.push_str("The previous run of this attempt stopped before it finished. Its work is in this worktree.\n\n");
    s.push_str(&format!("- previous run: {}\n", f.previous_run));
    s.push_str(&format!("- previous profile: {}\n", one_line(&previous_profile)));
    s.push_str(&format!("- previous harness: {}\n", one_line(&previous_harness)));
    s.push_str(&format!("- previous model: {}\n", one_line(&previous_model)));
    s.push_str(&format!("- previous status: {}\n", one_line(&previous_status)));
    s.push_str(&format!("- interrupted_reason: {}\n", one_line(&previous_reason)));
    s.push_str(&format!("- ended_at: {}\n", one_line(&previous_ended_at)));
    s.push_str(&format!("- this run's profile: {}\n", one_line(&to_profile)));
    s.push_str(&format!("- this run's harness: {}\n", one_line(&to_harness)));
    s.push_str(&format!("- this run's model: {}\n\n", one_line(&to_model)));

    s.push_str("## Worktree state\n\n");
    s.push_str(&format!("- branch: {branch}\n- base commit: {base_commit}\n\n"));
    let log_title = format!("git log --oneline {base_commit}..HEAD");
    push_git(&mut s, &log_title, &f.log, LOG_LINES, true, masker, &mut counts);
    push_git(&mut s, "git status --short", &f.status, STATUS_LINES, false, masker, &mut counts);
    push_git(&mut s, "git diff --stat HEAD", &f.diff_stat, DIFF_STAT_LINES, false, masker, &mut counts);

    s.push_str("## Evidence so far\n\n");
    if f.evidence.is_empty() {
        s.push_str("(no command evidence was captured in this attempt)\n");
    } else {
        s.push_str("Commands captured in this attempt (code, exit code, command):\n\n");
        let mut rows = f.evidence.clone();
        rows.sort_by_key(|l| code_number(&l.code));
        let skipped = rows.len().saturating_sub(EVIDENCE_ROWS);
        if skipped > 0 {
            s.push_str(&format!("... ({skipped} earlier rows)\n"));
        }
        for row in &rows[skipped..] {
            let exit = row.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "-".to_string());
            let (label, c) = masker.mask_text(&row.label);
            counts.merge(&c);
            s.push_str(&format!("{} exit={} {}\n", row.code, exit, one_line(&label)));
        }
    }
    s.push_str("\nThe full list is in .agent/evidence-index.json.\n\n");

    s.push_str("## What to do\n\n");
    s.push_str("1. Check what is already done against the acceptance criteria.\n");
    s.push_str("2. Finish the rest. Do not start over.\n");
    s.push_str("3. Re-run the checks yourself.\n");
    s.push_str("4. Write .agent/result.json again; do not reuse an old one.\n");
    (s, counts)
}

/// What `write` wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub sha256: String,
    pub bytes: u64,
    /// What the masking in `write` found; `render_facts`'s own counts are
    /// the caller's to merge in.
    pub redactions: Counts,
}

/// Writes `content`, masked (RD-1), to `<run_dir>/handoff.md` and
/// `<worktree>/.agent/handoff.md` (creating `.agent/` if needed, overwriting
/// an existing file). The hash and size are of the bytes written.
pub fn write(run_dir: &Path, worktree: &Path, content: &str, masker: &Masker) -> Result<Written> {
    let (content, redactions) = masker.mask_text(content);
    let content = content.as_str();
    let run_copy = run_dir.join(RUN_DIR_FILE);
    std::fs::write(&run_copy, content.as_bytes())
        .with_context(|| format!("writing {}", run_copy.display()))?;
    let agent_dir = worktree.join(".agent");
    std::fs::create_dir_all(&agent_dir)
        .with_context(|| format!("creating {}", agent_dir.display()))?;
    let wt_copy = worktree.join(".agent").join("handoff.md");
    std::fs::write(&wt_copy, content.as_bytes())
        .with_context(|| format!("writing {}", wt_copy.display()))?;
    let digest = Sha256::digest(content.as_bytes());
    Ok(Written {
        sha256: digest.iter().map(|b| format!("{b:02x}")).collect(),
        bytes: content.len() as u64,
        redactions,
    })
}

/// Removes `<worktree>/.agent/handoff.md`. A missing file is not an error.
pub fn remove(worktree: &Path) -> Result<()> {
    let path = worktree.join(".agent").join("handoff.md");
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            task_id: "T1".into(),
            attempt: 1,
            previous_run: "R1".into(),
            previous_profile: "claude-b".into(),
            previous_harness: "claude-code".into(),
            previous_model: "sonnet".into(),
            previous_status: "interrupted".into(),
            previous_reason: Some("quota".into()),
            previous_ended_at: Some("2026-10-01T00:00:00Z".into()),
            to_profile: "codex-a".into(),
            to_harness: "codex".into(),
            to_model: "gpt-5.6-luna".into(),
            branch: "thoth/T1/a1".into(),
            base_commit: "abc".into(),
            log: Ok("1234567 first\n".into()),
            status: Ok(String::new()),
            diff_stat: Ok(String::new()),
            evidence: Vec::new(),
        }
    }

    fn numbered(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    /// The lines of `text` after `heading` up to the next blank-line-separated
    /// heading, i.e. the section body.
    fn section<'a>(text: &'a str, heading: &str, next: &str) -> &'a str {
        let start = text.find(heading).expect("heading") + heading.len();
        let end = text[start..].find(next).map(|i| start + i).unwrap_or(text.len());
        &text[start..end]
    }

    #[test]
    fn exactly_fifty_lines_get_no_omission_line() {
        let got = cap_lines(&numbered(50), 50);
        assert_eq!(got.len(), 50);
        assert_eq!(got[49], "line 50");
        assert!(!got.iter().any(|l| l.starts_with("...")));
    }

    #[test]
    fn fifty_three_lines_keep_fifty_and_say_three_more() {
        let got = cap_lines(&numbered(53), 50);
        assert_eq!(got.len(), 51);
        assert_eq!(got[49], "line 50");
        assert_eq!(got[50], "... (3 more lines)");
    }

    #[test]
    fn a_failed_git_command_is_written_as_unavailable() {
        let mut f = facts();
        f.status = Err("git [\"status\"] failed\nfatal: not a git repository".into());
        let text = render_facts(&f, &Masker::from_pairs(&[])).0;
        let body = section(&text, "`git status --short`:\n\n", "\n\n");
        assert!(body.starts_with("(unavailable: "), "got {body:?}");
        assert!(!body.contains('\n'), "the error stays on one line: {body:?}");
    }

    #[test]
    fn a_hundred_and_three_rows_list_the_newest_hundred() {
        let mut f = facts();
        // Shuffled on purpose: the file orders them by number itself.
        f.evidence = (1..=103)
            .rev()
            .map(|i| EvidenceLine { code: format!("E{i:03}"), exit_code: Some(0), label: format!("cmd {i}") })
            .collect();
        let text = render_facts(&f, &Masker::from_pairs(&[])).0;
        let body = section(&text, "## Evidence so far\n", "## What to do");
        let rows: Vec<&str> = body.lines().filter(|l| l.starts_with('E') || l.starts_with("...")).collect();
        assert_eq!(rows[0], "... (3 earlier rows)");
        assert_eq!(rows.len(), 101);
        assert!(rows[1].starts_with("E004 "), "got {}", rows[1]);
        assert!(rows[100].starts_with("E103 "), "got {}", rows[100]);
        for gone in ["E001 ", "E002 ", "E003 "] {
            assert!(!body.contains(gone), "{gone} should be left out");
        }
        assert!(body.contains(".agent/evidence-index.json"));
    }

    #[test]
    fn line_breaks_in_the_why_fields_become_spaces() {
        let mut f = facts();
        f.to_model = "x\n## Injected".into();
        f.previous_profile = "p\r\n# H".into();
        let text = render_facts(&f, &Masker::from_pairs(&[])).0;
        assert!(text.contains("- this run's model: x ## Injected\n"), "got {text:?}");
        assert!(text.contains("- previous profile: p  # H\n"), "got {text:?}");
        let why = section(&text, "## Why", "## Worktree state");
        assert!(!why.lines().any(|l| l.starts_with('#')), "got {why:?}");
    }

    #[test]
    fn a_null_exit_code_is_written_as_a_dash() {
        let mut f = facts();
        f.evidence = vec![
            EvidenceLine { code: "E001".into(), exit_code: None, label: "agy thing".into() },
            EvidenceLine { code: "E002".into(), exit_code: Some(2), label: "false".into() },
        ];
        let text = render_facts(&f, &Masker::from_pairs(&[])).0;
        assert!(text.contains("E001 exit=- agy thing\n"), "{text}");
        assert!(text.contains("E002 exit=2 false\n"), "{text}");
    }

    #[test]
    fn line_breaks_in_a_label_become_spaces_and_cannot_open_a_heading() {
        let mut f = facts();
        f.evidence = vec![
            EvidenceLine { code: "E001".into(), exit_code: Some(0), label: "echo a\n## Injected".into() },
            EvidenceLine { code: "E002".into(), exit_code: Some(0), label: "echo b\r\n## Injected too".into() },
        ];
        let text = render_facts(&f, &Masker::from_pairs(&[])).0;
        assert!(text.contains("E001 exit=0 echo a ## Injected\n"), "{text}");
        assert!(text.contains("E002 exit=0 echo b  ## Injected too\n"), "{text}");
        let body = section(&text, "## Evidence so far\n", "## What to do");
        assert!(
            !body.lines().any(|l| l.starts_with('#')),
            "no heading between Evidence so far and What to do: {body}"
        );
    }

    #[test]
    fn line_breaks_in_a_commit_title_become_spaces() {
        let mut f = facts();
        f.log = Ok("1234567 subject\rwith a CR\n".into());
        let text = render_facts(&f, &Masker::from_pairs(&[])).0;
        assert!(text.contains("    1234567 subject with a CR\n"), "{text}");
    }

    #[test]
    fn the_sections_come_in_order() {
        let text = render_facts(&facts(), &Masker::from_pairs(&[])).0;
        let at = |h: &str| text.find(h).unwrap_or_else(|| panic!("missing {h}"));
        assert!(text.starts_with("# Handoff\n"));
        assert!(at("## Why") < at("## Worktree state"));
        assert!(at("## Worktree state") < at("## Evidence so far"));
        assert!(at("## Evidence so far") < at("## What to do"));
        assert!(text.contains("- interrupted_reason: quota"));
        assert!(text.contains("`git log --oneline abc..HEAD`"));
    }

    #[test]
    fn parse_handoff_reads_none_from_a_missing_wrong_or_partial_key() {
        assert!(parse_handoff(r#"{"goal":"g"}"#).is_none());
        assert!(parse_handoff(r#"{"handoff":"R1"}"#).is_none());
        assert!(parse_handoff(
            r#"{"handoff":{"reason":"quota","from_profile":"p","from_harness":"h","path":".agent/handoff.md"}}"#
        )
        .is_none());
        assert!(parse_handoff("{not json").is_none());
        let full = r#"{"handoff":{"previous_run":"R1","reason":"quota","from_profile":"p","from_harness":"h","path":".agent/handoff.md"}}"#;
        assert_eq!(parse_handoff(full).unwrap().previous_run, "R1");
    }

    #[test]
    fn write_puts_the_same_bytes_in_both_places_and_hashes_them() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        let w = write(&run_dir, &wt, "hello\n", &Masker::from_pairs(&[])).unwrap();
        assert_eq!(std::fs::read(run_dir.join("handoff.md")).unwrap(), b"hello\n");
        assert_eq!(std::fs::read(wt.join(".agent/handoff.md")).unwrap(), b"hello\n");
        assert_eq!(w.bytes, 6);
        assert_eq!(w.sha256, "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03");
        remove(&wt).unwrap();
        assert!(!wt.join(".agent/handoff.md").exists());
        remove(&wt).unwrap();
    }

    #[test]
    fn write_masks_the_content_and_hashes_what_it_wrote() {
        let tmp = tempfile::tempdir().unwrap();
        let run_dir = tmp.path().join("run");
        let wt = tmp.path().join("wt");
        std::fs::create_dir_all(&run_dir).unwrap();
        std::fs::create_dir_all(&wt).unwrap();
        let masker = Masker::from_pairs(&[("dummy-handoff-value-1", "env:RD_UNIT_HANDOFF")]);
        let w = write(&run_dir, &wt, "# Handoff\nlabel dummy-handoff-value-1 end\n", &masker).unwrap();
        let run_copy = std::fs::read(run_dir.join("handoff.md")).unwrap();
        let wt_copy = std::fs::read(wt.join(".agent/handoff.md")).unwrap();
        assert_eq!(run_copy, wt_copy);
        let text = String::from_utf8(run_copy.clone()).unwrap();
        assert!(!text.contains("dummy-handoff-value-1"), "{text}");
        assert!(text.contains("label [redacted:env:RD_UNIT_HANDOFF] end"), "{text}");
        let digest = Sha256::digest(&run_copy);
        let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
        assert_eq!(w.sha256, hex);
        assert_eq!(w.bytes, run_copy.len() as u64);
        assert_eq!(w.redactions.to_json(), serde_json::json!({"env:RD_UNIT_HANDOFF": 1}));
    }

    #[test]
    fn render_facts_masks_a_secret_with_a_line_break() {
        let secret = "dummy-line\nbreak-1";
        let masker = Masker::from_pairs(&[(secret, "env:RD_UNIT_BREAK")]);
        let mut f = facts();
        f.previous_reason = Some(secret.to_string());
        f.evidence = vec![EvidenceLine {
            code: "E001".into(),
            exit_code: Some(0),
            label: format!("echo {secret}"),
        }];
        let (text, counts) = render_facts(&f, &masker);
        assert!(text.contains("[redacted:env:RD_UNIT_BREAK]"), "{text}");
        assert!(!text.contains("dummy-line"), "{text}");
        assert!(!text.contains("break-1"), "{text}");
        let lines: Vec<&str> = text.lines().collect();
        let at = lines
            .iter()
            .position(|l| l.starts_with("- interrupted_reason: "))
            .expect("interrupted_reason line");
        assert!(lines[at + 1].starts_with("- ended_at: "), "{text}");
        assert_eq!(counts.to_json(), serde_json::json!({"env:RD_UNIT_BREAK": 2}));
    }
}
