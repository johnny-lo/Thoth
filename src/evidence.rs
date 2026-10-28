//! Everything between a captured hook line and an `evidence` row.
//!
//! brief §6/§7 and v6 §2.2: evidence records and the evidence index are
//! generated **by the control plane**, never by the claimant. That is the whole
//! point of this module existing separately from `ingest`, which only ever
//! reads what this module wrote.

use crate::config::Paths;
use crate::db::state::{self, NewEvent};
use crate::model::event_type;
use anyhow::{anyhow, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// What `thoth hook` recorded about a captured tool output.
///
/// `source` is `"persisted"` when the hook copied Claude Code's
/// `persistedOutputPath` into the run's inbox, and `"inline"` when the runner
/// has to build the blob from the (possibly truncated) inline `stdout`
/// instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookBlob {
    pub sha256: String,
    pub bytes: i64,
    // Global Constraints inventory: never retired. Read by
    // `a_readable_persisted_output_path_becomes_an_inbox_blob`; Task 3's
    // `record_command` keys off `sha256` alone (whether a file named after it
    // exists under the inbox's blobs dir), not off this field, so it stays
    // unread by production code.
    #[allow(dead_code)]
    pub source: String,
}

/// `E001` → 1. `None` for anything that is not `E` followed by ASCII digits.
// Global Constraints: this needs no marker of its own because `next_code`
// calls it, in its own module, and `next_code` now has a production caller
// (`insert_row`, Task 3) - dead-code analysis propagates through that call.
pub fn parse_code_number(code: &str) -> Option<i64> {
    let rest = code.strip_prefix('E')?;
    if rest.is_empty() || !rest.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    rest.parse().ok()
}

/// The next per-task evidence code (brief §6: "`E001`… 每個 task 一個序列（跨
/// run/attempt 連續）").
///
/// The maximum is computed over parsed numbers rather than with `MAX(code)`,
/// because string ordering puts `E1000` before `E999` and the sequence must
/// keep counting past the zero padding. Callers must run this inside the same
/// transaction as the insert; `idx_evidence_task_code` is the backstop if two
/// writers race anyway.
///
/// Production caller: `insert_row`, below (Task 3). Also read directly by
/// `next_code_starts_at_e001_and_counts_per_task` and its sibling tests.
pub fn next_code(conn: &Connection, task_id: &str) -> Result<String> {
    let mut stmt = conn.prepare("SELECT code FROM evidence WHERE task_id = ?1")?;
    let codes = stmt
        .query_map([task_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    let max = codes
        .iter()
        .filter_map(|c| parse_code_number(c))
        .max()
        .unwrap_or(0);
    Ok(format!("E{:03}", max + 1))
}

/// v6 §10.5's `evidence.collected_by`. Every row Phase 2 writes carries this
/// value: hook capture is out-of-band, the diff and commit come from git, and
/// `verify` re-runs the command itself. v6 §2.2's first companion rule -
/// "Evidence 記錄與 Evidence Index 由 Control Plane 機械生成" - is why no code
/// path writes `worker` here.
pub const COLLECTED_BY_CONTROL_PLANE: &str = "control_plane";

/// brief §6: the index label is the command's first 80 characters.
pub const LABEL_MAX_CHARS: usize = 80;

/// The fixed prefix of a `PostToolUseFailure`'s `error`
/// (02-evidence-capture.md:37: `"Exit code 3\noops-stderr"`).
pub const EXIT_CODE_PREFIX: &str = "Exit code ";

/// A one-line, bounded label for a command.
///
/// Counted in characters and cut on a character boundary: `&s[..80]` panics
/// whenever byte 80 lands inside a multi-byte character, which is the exact bug
/// Phase 1 had to fix in `commands::task::first_line`.
pub fn label_from_command(cmd: &str) -> String {
    let one_line = cmd.replace(['\r', '\n'], " ");
    let trimmed = one_line.trim();
    if trimmed.chars().count() <= LABEL_MAX_CHARS {
        return trimmed.to_string();
    }
    let cut = trimmed
        .char_indices()
        .nth(LABEL_MAX_CHARS)
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    trimmed[..cut].to_string()
}

/// brief §5: a `PostToolUseFailure` has no numeric exit-code field, so the code
/// is the `N` in `"Exit code N\n<output>"`, where the text after the first line
/// is the command's output as Claude Code reported it (not necessarily stderr).
///
/// The spec writes this as `^Exit code (\d+)`. This is a literal expansion of
/// exactly that pattern - anchored at the start, case-sensitive, ASCII digits
/// only - because Phase 1 deliberately carries no `regex` dependency and Phase
/// 2 adds none (Open question 12). If a future Claude Code version changes the
/// wording, change it here; do not reach for the crate.
pub fn parse_exit_code(error_text: &str) -> Option<i64> {
    let rest = error_text.strip_prefix(EXIT_CODE_PREFIX)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse().ok()
}

/// Milliseconds from `from` to `to`, or `None` if either timestamp is
/// unparseable. Used as the fallback when a payload has no `duration_ms`.
pub fn rfc3339_delta_ms(from: &str, to: &str) -> Option<i64> {
    use time::format_description::well_known::Rfc3339;
    use time::OffsetDateTime;
    let a = OffsetDateTime::parse(from, &Rfc3339).ok()?;
    let b = OffsetDateTime::parse(to, &Rfc3339).ok()?;
    Some((b - a).whole_milliseconds() as i64)
}

/// One line of `<run_dir>/inbox/hooks.jsonl`, as `thoth hook` wrote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookLine {
    pub received_at: String,
    pub event: String,
    pub payload: serde_json::Value,
    pub blob: Option<HookBlob>,
    pub truncated_by_hook: bool,
}

pub fn parse_hook_line(line: &str) -> Result<HookLine> {
    let v: serde_json::Value =
        serde_json::from_str(line.trim()).context("inbox line is not valid JSON")?;
    let payload = v
        .get("payload")
        .cloned()
        .ok_or_else(|| anyhow!("inbox line has no payload object"))?;
    let event = v["event"]
        .as_str()
        .or_else(|| payload["hook_event_name"].as_str())
        .unwrap_or("unknown")
        .to_string();
    let blob = v.get("blob").and_then(|b| {
        Some(HookBlob {
            sha256: b["sha256"].as_str()?.to_string(),
            bytes: b["bytes"].as_i64().unwrap_or(0),
            source: b["source"].as_str().unwrap_or("persisted").to_string(),
        })
    });
    Ok(HookLine {
        received_at: v["received_at"].as_str().unwrap_or_default().to_string(),
        event,
        payload,
        blob,
        truncated_by_hook: v["truncated_by_hook"].as_bool().unwrap_or(false),
    })
}

fn str_field(payload: &serde_json::Value, key: &str) -> Option<String> {
    payload[key].as_str().map(|s| s.to_string())
}

/// A `PreToolUse` waiting for its `Post`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingPre {
    tool_use_id: Option<String>,
    cmd: String,
    cwd: String,
    received_at: String,
}

/// One shell command, assembled from a `Pre` plus its `Post`/`PostFailure`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEvidence {
    pub cmd: String,
    pub cwd: String,
    /// Which hook event closed the pair; kept in the detail so an operator can
    /// tell "succeeded" from "failed with an unparseable message" apart.
    pub event: String,
    pub exit_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub session_id: Option<String>,
    pub tool_use_id: Option<String>,
    pub stdout: String,
    pub stderr: String,
    pub error_text: Option<String>,
    /// The inline `stdout` was cut short (Claude Code truncates at 30000
    /// characters, or `thoth hook` hit its own 8 MiB line cap).
    pub truncated_inline: bool,
    pub blob: Option<HookBlob>,
}

/// brief §5: pair `Pre` with `Post` by `tool_use_id`, falling back to FIFO
/// only when the `Post` carries no `tool_use_id` at all.
///
/// The fallback matters because nothing in the payload schema is contractually
/// guaranteed - Phase 0 saw `tool_use_id` on all six lines, but a harness
/// version that drops it must degrade to "the oldest unmatched command" rather
/// than losing the evidence.
///
/// Fix round 1 / M3: a `Post` that *does* carry a `tool_use_id` but matches no
/// pending `Pre` (a dropped or reordered `Pre` line, a bug in a future harness
/// version) must not steal the oldest unrelated `Pre` - that would attribute
/// one command's evidence to a completely different one. FIFO is only ever
/// the fallback for "this harness sends no `tool_use_id` at all", not for
/// "this `tool_use_id` didn't match".
#[derive(Debug, Default)]
pub struct Pairer {
    pending: Vec<PendingPre>,
}

impl Pairer {
    /// How many `Pre` events are still waiting. Read by the pairer tests and by
    /// the runner's final `status_change` detail.
    pub fn pending(&self) -> usize {
        self.pending.len()
    }

    pub fn offer(&mut self, line: &HookLine) -> Option<CommandEvidence> {
        match line.event.as_str() {
            "PreToolUse" => {
                self.pending.push(PendingPre {
                    tool_use_id: str_field(&line.payload, "tool_use_id"),
                    cmd: line.payload["tool_input"]["command"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    cwd: str_field(&line.payload, "cwd").unwrap_or_default(),
                    received_at: line.received_at.clone(),
                });
                None
            }
            "PostToolUse" | "PostToolUseFailure" => {
                let tool_use_id = str_field(&line.payload, "tool_use_id");
                let idx = match &tool_use_id {
                    // Has an id: only an exact match may be consumed. No
                    // match at all (not "no pending Pre", specifically "no
                    // pending Pre with this id") - do not fall back to FIFO.
                    Some(id) => self
                        .pending
                        .iter()
                        .position(|p| p.tool_use_id.as_deref() == Some(id.as_str())),
                    // No id in the payload at all: the only signal left is
                    // arrival order.
                    None => (!self.pending.is_empty()).then_some(0),
                };
                let pre = idx.map(|i| self.pending.remove(i));
                Some(build_command_evidence(line, pre.as_ref(), tool_use_id))
            }
            // UserPromptSubmit, SessionStart and friends are not hooked, but a
            // merged user-level settings file may add them (Phase 0 Q2 measured
            // that merge), so unknown events are ignored rather than an error.
            _ => None,
        }
    }
}

/// The marks Claude Code leaves in a tool output it shortened itself, as
/// `(text before N, text after N)`; `None` means the mark has no number.
/// Read from the claude 2.1.283 binary - re-check on a Claude Code upgrade.
const CLAUDE_TRUNCATION_MARKERS: &[(&str, Option<&str>)] = &[
    ("... [", Some(" characters truncated] ...")),
    ("... [", Some(" lines truncated] ...")),
    ("[output truncated]", None),
    ("[output truncated at ", Some(" bytes]")),
    ("... [output truncated - ", Some("KB removed]")),
];

/// True when `output` carries one of Claude Code's own truncation marks.
fn has_claude_truncation_marker(output: &str) -> bool {
    CLAUDE_TRUNCATION_MARKERS.iter().any(|&(before, after)| match after {
        None => output.contains(before),
        Some(after) => output.match_indices(before).any(|(i, _)| {
            let rest = &output[i + before.len()..];
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            digits > 0 && rest[digits..].starts_with(after)
        }),
    })
}

fn build_command_evidence(
    line: &HookLine,
    pre: Option<&PendingPre>,
    tool_use_id: Option<String>,
) -> CommandEvidence {
    let payload = &line.payload;
    let cmd = payload["tool_input"]["command"]
        .as_str()
        .map(|s| s.to_string())
        .or_else(|| pre.map(|p| p.cmd.clone()))
        .unwrap_or_default();
    let cwd = str_field(payload, "cwd")
        .or_else(|| pre.map(|p| p.cwd.clone()))
        .unwrap_or_default();
    let failure = line.event == "PostToolUseFailure";
    let error_text = str_field(payload, "error");
    let exit_code = if failure {
        error_text.as_deref().and_then(parse_exit_code)
    } else {
        // 02-evidence-capture.md:30: a PostToolUse firing at all *is* the
        // success signal. There is no field to read.
        Some(0)
    };
    let duration_ms = payload["duration_ms"]
        .as_i64()
        .or_else(|| pre.and_then(|p| rfc3339_delta_ms(&p.received_at, &line.received_at)));
    let resp = &payload["tool_response"];
    let (stdout, stderr) = if failure {
        // A failure has no `tool_response`. Its `error` is
        // "Exit code N\n<output>", where the text after the first newline is
        // the command's output as Claude Code reported it (compiler stderr and
        // test-harness stdout arrive together). It becomes the primary blob so
        // `evidence show` prints it; an `error` with no newline is kept whole.
        let output = error_text
            .as_deref()
            .map(|e| e.split_once('\n').map_or(e, |(_, t)| t).to_string())
            .unwrap_or_default();
        (output, String::new())
    } else {
        (
            resp["stdout"].as_str().unwrap_or_default().to_string(),
            resp["stderr"].as_str().unwrap_or_default().to_string(),
        )
    };
    let truncated_inline = resp.get("persistedOutputPath").is_some()
        || line.truncated_by_hook
        || has_claude_truncation_marker(&stdout);
    CommandEvidence {
        cmd,
        cwd,
        event: line.event.clone(),
        exit_code,
        duration_ms,
        session_id: str_field(payload, "session_id"),
        tool_use_id,
        stdout,
        stderr,
        error_text,
        truncated_inline,
        blob: line.blob.clone(),
    }
}

/// The file the tailer's byte offset survives a runner restart in.
///
/// Fix round 1 / I1: `resume` reuses the same run row, the same `run_dir`,
/// and therefore the same inbox - but the previous `InboxTailer` (and its
/// in-memory offset) died with the old process. Without a persisted offset a
/// resumed run's tailer starts over at 0 and replays every line the previous
/// attempt already turned into an `evidence` row, duplicating both the rows
/// and their `evidence_added` events.
const OFFSET_FILE: &str = ".offset";

/// Follows `<inbox>/hooks.jsonl` by byte offset, persisting that offset to
/// `<inbox>/.offset` after every successful read so a fresh `InboxTailer`
/// over the same inbox (a resumed run's new runner process) does not replay
/// lines an earlier `InboxTailer` already turned into evidence.
///
/// `thoth hook` appends each record with a single `write_all` that includes
/// the trailing newline, so a partial record is already unlikely; the held-back
/// `partial` buffer covers it anyway, because "unlikely" is not "impossible"
/// when the writer is a separate process.
///
/// The byte persisted is not simply "how far the last raw read reached": it
/// excludes whatever is still sitting in `partial` (bytes read but not yet
/// terminated by a `\n`). That is what makes persistence safe across a
/// restart, which loses the in-memory `partial` buffer - the persisted offset
/// always points *before* any not-yet-complete line, so a fresh tailer
/// re-reads that trailing partial data straight off disk (the file itself is
/// never truncated) instead of silently losing it.
pub struct InboxTailer {
    path: PathBuf,
    offset_path: PathBuf,
    /// Raw bytes fetched from `path` so far; the next read seeks here. Not
    /// what gets persisted - see `safe_offset`.
    offset: u64,
    /// Bytes read but not yet forming a complete (`\n`-terminated) line.
    /// `Vec<u8>`, not `String` (fix round 1 / M2): a read boundary can land
    /// inside a multi-byte UTF-8 character, and lossily decoding *that*
    /// half-character before more bytes arrive would corrupt it permanently.
    /// Only complete lines are ever decoded, and each is decoded on its own.
    partial: Vec<u8>,
}

impl InboxTailer {
    pub fn new(inbox_dir: &Path) -> InboxTailer {
        let path = inbox_dir.join(crate::commands::hook::HOOKS_FILE);
        let offset_path = inbox_dir.join(OFFSET_FILE);
        let mut offset = std::fs::read_to_string(&offset_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        // Fix round 1 / I1: a persisted offset past the hooks file's current
        // length means the file this offset was measured against is not the
        // one on disk now (rotated out from under us, or truncated/corrupt).
        // Trusting it would make `read_new_lines`'s own `len <= self.offset`
        // guard report "nothing new" forever; starting over is the safe
        // choice; a Bash call replayed once is a duplicate evidence row, but
        // it is not a lost run.
        if let Ok(meta) = std::fs::metadata(&path) {
            if offset > meta.len() {
                InboxTailer::log_io_issue(
                    &path,
                    &format!(
                        "persisted offset {offset} exceeds {}'s current size {}; resetting to 0",
                        path.display(),
                        meta.len()
                    ),
                );
                offset = 0;
            }
        }
        InboxTailer { path, offset_path, offset, partial: Vec::new() }
    }

    /// The offset safe to persist: raw bytes read so far, minus whatever is
    /// still an incomplete trailing line in `partial`. See the struct doc.
    fn safe_offset(&self) -> u64 {
        self.offset - self.partial.len() as u64
    }

    /// Best-effort temp+rename, matching `write_index`'s atomicity: a reader
    /// (there is none today, but a future one) must never see a half-written
    /// number. Never fails loudly - a missed persist just means a resumed run
    /// re-reads a few already-seen lines, which `InboxWatch`'s caller-side
    /// idempotency (none yet; tracked as a known gap) does not need for this
    /// to be strictly better than replaying the *entire* inbox, which is the
    /// bug this exists to fix.
    fn persist_offset(&self) {
        let Some(dir) = self.offset_path.parent() else { return };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let temp = dir.join(format!(".offset.{}.tmp", std::process::id()));
        if std::fs::write(&temp, self.safe_offset().to_string()).is_err() {
            let _ = std::fs::remove_file(&temp);
            return;
        }
        let _ = std::fs::rename(&temp, &self.offset_path);
    }

    /// Fix round 1 / M4: inbox IO errors go to `runner-error.log`, not
    /// nowhere. `path` is `<inbox>/hooks.jsonl`, so `path.parent().parent()`
    /// is the run directory (`<run_dir>/inbox/hooks.jsonl`); that layout is
    /// established by `runner::run_worker_inner`, which always builds
    /// `inbox_dir` as `run_dir.join("inbox")`.
    fn log_io_issue(path: &Path, msg: &str) {
        if let Some(run_dir) = path.parent().and_then(|inbox| inbox.parent()) {
            crate::runner::log_runner_error(run_dir, msg);
        }
    }

    /// Whatever has been appended since the last call, split into complete
    /// lines, persisting the new offset before returning. A missing file (no
    /// Bash calls yet) is normal and never logged; any other failure to open,
    /// stat, seek or read the file is logged once per occurrence and treated
    /// the same as "nothing new" - this must never be able to abort the run.
    pub fn read_new_lines(&mut self) -> Vec<String> {
        let f = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
            Err(e) => {
                Self::log_io_issue(&self.path, &format!("opening {}: {e}", self.path.display()));
                return Vec::new();
            }
        };
        let mut f = f;
        let len = match f.metadata().map(|m| m.len()) {
            Ok(len) => len,
            Err(e) => {
                Self::log_io_issue(&self.path, &format!("statting {}: {e}", self.path.display()));
                return Vec::new();
            }
        };
        if len <= self.offset {
            return Vec::new();
        }
        if let Err(e) = f.seek(SeekFrom::Start(self.offset)) {
            Self::log_io_issue(&self.path, &format!("seeking {}: {e}", self.path.display()));
            return Vec::new();
        }
        let mut buf: Vec<u8> = Vec::new();
        let read = match f.read_to_end(&mut buf) {
            Ok(n) => n,
            Err(e) => {
                Self::log_io_issue(&self.path, &format!("reading {}: {e}", self.path.display()));
                return Vec::new();
            }
        };
        self.offset += read as u64;
        self.partial.extend_from_slice(&buf);

        let mut out = Vec::new();
        while let Some(i) = self.partial.iter().position(|&b| b == b'\n') {
            let line_bytes: Vec<u8> = self.partial.drain(..=i).collect();
            // Lossy decoding, like the runner's stdout reader: one bad byte in
            // a command's output must not be able to stop the whole tail.
            // Decoded per complete line (fix round 1 / M2), never across a
            // read boundary, so a multi-byte character split by a read never
            // gets corrupted - it just waits in `partial` for its other half.
            let line = String::from_utf8_lossy(&line_bytes[..line_bytes.len() - 1]).into_owned();
            if !line.trim().is_empty() {
                out.push(line);
            }
        }
        self.persist_offset();
        out
    }
}

/// One row of `.agent/evidence-index.json` (v6 §10.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRow {
    pub code: String,
    pub kind: String,
    pub label: String,
    pub exit_code: Option<i64>,
}

pub fn index_path(worktree: &Path) -> PathBuf {
    worktree.join(".agent").join("evidence-index.json")
}

/// Writes the index through a temp file, then replaces the previous generation
/// with a single `rename`.
///
/// This really is the atomic write brief §2 asks for: `std::fs::rename` replaces
/// an existing destination on Windows as well as on Unix (`MoveFileExW` with
/// `MOVEFILE_REPLACE_EXISTING`), so a worker reading the file concurrently — and
/// the prompt tells it to — sees either the whole old generation or the whole new
/// one. Never `remove_file` the target first: that would open a window in which
/// the file is missing, and the runner rewrites it once a second.
pub fn write_index(worktree: &Path, rows: &[IndexRow]) -> Result<()> {
    let target = index_path(worktree);
    let dir = target
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| worktree.to_path_buf());
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let temp = dir.join(format!(".evidence-index.{}.tmp", std::process::id()));
    std::fs::write(&temp, serde_json::to_string_pretty(rows)?)
        .with_context(|| format!("writing {}", temp.display()))?;
    std::fs::rename(&temp, &target)
        .with_context(|| format!("renaming into {}", target.display()))?;
    Ok(())
}

/// Regenerates the worker-facing index from the database and returns the row
/// count. v6 §10.4: "worker 不自己編證據編號" - this is the only writer.
/// Labels are masked (RD-1): workers and reviewers read this file. No
/// `secret_redacted` event, as the runner rewrites it every second.
pub fn refresh_index(conn: &Connection, task_id: &str, worktree: &Path) -> Result<usize> {
    let masker = crate::redact::Masker::current();
    let rows: Vec<IndexRow> = state::list_evidence_for_task(conn, task_id)?
        .into_iter()
        .map(|e| {
            let exit_code = serde_json::from_str::<serde_json::Value>(&e.detail)
                .ok()
                .and_then(|d| d["exit_code"].as_i64());
            IndexRow {
                code: e.code,
                kind: e.kind,
                label: masker.mask_text(&e.label).0,
                exit_code,
            }
        })
        .collect();
    let n = rows.len();
    write_index(worktree, &rows)?;
    Ok(n)
}

/// Who is recording, and where the blobs go.
pub struct EvidenceContext<'a> {
    pub paths: &'a Paths,
    pub repo_id: &'a str,
    pub task_id: &'a str,
    pub run_id: &'a str,
}

/// One transaction per row: allocate the code and insert under it, and commit
/// the `evidence_added` event alongside so the log can never name a row that
/// is not there.
///
/// Fix round 1 / M7: the transaction is not what stops two writers from
/// taking the same code - it makes one writer's allocate-then-insert atomic,
/// but two racing transactions can each compute the same `next_code` from the
/// same starting state. What actually prevents the collision is the database
/// schema's `idx_evidence_task_code` UNIQUE index: the second `insert_evidence`
/// fails, `?` turns that into this function's `Err`, and the caller
/// (`InboxWatch::drain`) logs it to `runner-error.log` and drops that one
/// command rather than retrying - a dropped command's evidence is simply a
/// missing citation, which already has a defined consequence downstream
/// (UNVERIFIED). Phase 2 has exactly one writer (the runner) per run, so this
/// is theoretical today; it stays correct if that ever changes.
///
/// Eight parameters, which is one past clippy's default
/// `too-many-arguments-threshold` of 7, so it carries the same allow Phase 1's
/// `runner::finish_and_settle` does. Collapsing the last six into a struct would
/// buy nothing: every caller fills all of them in once and never stores them.
///
/// `pub(crate)` (controller ruling, Task 3): Task 4 consumes this directly to
/// record diff/commit evidence rather than duplicating the code-allocation +
/// insert + event transaction.
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_row(
    conn: &Connection,
    ctx: &EvidenceContext<'_>,
    kind: &str,
    label: &str,
    sha256: Option<String>,
    bytes: Option<i64>,
    blob_path: Option<PathBuf>,
    detail: serde_json::Value,
) -> Result<String> {
    // `write_tx` (BEGIN IMMEDIATE), not a deferred transaction: `next_code` is
    // a read the insert then acts on. Deferred, a commit by another connection
    // between the two - `dispatch` recording `runner_pid` while the runner it
    // just started is already ingesting hooks - fails the insert at once with
    // SQLITE_BUSY instead of waiting out the busy timeout, and the command's
    // evidence is lost. Linux support found it (the runner starts faster there
    // relative to `dispatch` returning; docs/post-phase2/linux-support-scope-
    // 2026-09-26.md §8), but nothing about it is platform-specific.
    // A caller already holding the write lock (research review record) owns
    // the atomic boundary: evidence must roll back with its verdict too.
    let tx = if conn.is_autocommit() { Some(crate::db::write_tx(conn)?) } else { None };
    let writer = tx.as_deref().unwrap_or(conn);
    let code = next_code(writer, ctx.task_id)?;
    let row = state::Evidence {
        evidence_id: crate::ids::new_evidence_id(),
        code: code.clone(),
        task_id: ctx.task_id.to_string(),
        run_id: Some(ctx.run_id.to_string()),
        kind: kind.to_string(),
        collected_by: COLLECTED_BY_CONTROL_PLANE.to_string(),
        label: label.to_string(),
        sha256,
        bytes,
        blob_path: blob_path.map(|p| p.display().to_string()),
        detail: detail.to_string(),
        created_at: crate::ids::now_rfc3339(),
    };
    state::insert_evidence(writer, &row)?;
    state::append_event(
        writer,
        &NewEvent {
            task_id: Some(ctx.task_id.to_string()),
            run_id: Some(ctx.run_id.to_string()),
            event_type: event_type::EVIDENCE_ADDED.to_string(),
            detail: serde_json::json!({
                "code": code,
                "kind": kind,
                "label": label,
                "collected_by": COLLECTED_BY_CONTROL_PLANE,
                "sha256": row.sha256,
                "bytes": row.bytes,
            }),
        },
    )?;
    if let Some(tx) = tx { tx.commit()?; }
    Ok(code)
}

/// brief §10: what marks a `verify` row apart from a captured one.
pub const VERIFY_LABEL_PREFIX: &str = "verify: ";

/// `verify --at-base`: the same re-run, made in a checkout of the task's
/// base_commit, so a reviewer can tell a baseline row from a candidate row.
pub const VERIFY_AT_BASE_LABEL_PREFIX: &str = "verify@base: ";

/// Evidence for a command the control plane ran **itself**, rather than one it
/// observed (v6 §2.2 rule 4: "僅 High Risk 時，Control Plane 可選擇性 re-run").
///
/// Same `kind = "command"` shape as a captured row, so `evidence show`, the
/// index and every citation rule treat it identically; the label prefix is the
/// only thing that distinguishes it, and `detail.event` says so too.
#[allow(clippy::too_many_arguments)]
pub fn record_control_plane_command(
    conn: &Connection,
    ctx: &EvidenceContext<'_>,
    label_prefix: &str,
    cmd: &str,
    cwd: &str,
    exit_code: Option<i64>,
    duration_ms: Option<i64>,
    stdout: &str,
    stderr: &str,
    /* the argv actually executed; see ShellOutput::argv */
    argv: &[String],
    // Fix round 1 / I1, I2: set for a request `verify` refused (an unsafe or
    // missing `cwd`), a spawn failure, or a timeout - the three cases where
    // `exit_code` is `None` because the command never produced one. `None`
    // for every command that actually ran, whatever its exit code.
    error: Option<&str>,
    // Whole-branch review Important 4: `run_shell` stopped appending at
    // `process::MAX_STREAM_BYTES`, so the blob below holds a prefix of what
    // the command printed. Recorded under the same `truncated_inline` key a
    // hook-captured row already uses for "what is stored is cut short", so a
    // reader has one key to check rather than two.
    output_truncated: bool,
    // `verify --at-base`: the commit the command ran against. Absent (no key
    // at all) for a re-run in the run's own worktree.
    at_commit: Option<&str>,
    // P7-1b: keys `verify` adds about the approved check (check_id, shell,
    // fingerprint, ...), merged into `detail` as they are.
    extra: &serde_json::Map<String, serde_json::Value>,
) -> Result<String> {
    let (sha, bytes, path) = crate::blobs::put_bytes(ctx.paths, ctx.repo_id, stdout.as_bytes())?;
    let stderr_sha = if stderr.is_empty() {
        None
    } else {
        Some(crate::blobs::put_bytes(ctx.paths, ctx.repo_id, stderr.as_bytes())?.0)
    };
    let mut detail = serde_json::json!({
        "cmd": cmd,
        "cwd": cwd,
        "exit_code": exit_code,
        "duration_ms": duration_ms,
        "stdout_sha": sha,
        "stderr_sha": stderr_sha,
        "session_id": serde_json::Value::Null,
        "tool_use_id": serde_json::Value::Null,
        "truncated_inline": output_truncated,
        "event": "control_plane_rerun",
        "error": error,
        "blob_source": "control_plane",
        // L7: `cmd` is what the worker declared, `argv` is what actually ran
        // (the shell plus that same line, passed verbatim on both platforms).
        // The whole point of `verify` is re-running the declared command, so
        // both are on the record.
        "argv": argv,
    });
    if let Some(commit) = at_commit {
        detail["at_commit"] = serde_json::json!(commit);
    }
    for (k, v) in extra {
        detail[k] = v.clone();
    }
    let label = format!("{label_prefix}{}", label_from_command(cmd));
    insert_row(
        conn,
        ctx,
        "command",
        &label,
        Some(sha),
        Some(bytes as i64),
        Some(path),
        detail,
    )
}

/// Turns one paired shell command into an `evidence` row plus its blobs.
///
/// The stdout blob prefers the file `thoth hook` already copied out of
/// Claude Code's persisted-output directory, because the inline `stdout` in the
/// payload is truncated at 30000 characters. If that file is somehow gone, the
/// inline text is stored instead and `detail.truncated_inline` says so, which
/// is strictly better than recording nothing. Fix round 1 / M4: that fallback
/// is now also logged to `runner-error.log` - a silent degradation from
/// "complete captured output" to "the truncated preview" is exactly the kind
/// of thing an operator investigating a thin evidence row needs to see.
pub fn record_command(
    conn: &Connection,
    ctx: &EvidenceContext<'_>,
    inbox_dir: &Path,
    ev: &CommandEvidence,
) -> Result<String> {
    let mut blob_source = "inline";
    let (sha, bytes, path) = match &ev.blob {
        Some(b) => {
            let src = inbox_dir
                .join(crate::commands::hook::BLOBS_DIR)
                .join(&b.sha256);
            if src.is_file() {
                blob_source = "persisted";
                crate::blobs::put_file(ctx.paths, ctx.repo_id, &src)?
            } else {
                if let Some(run_dir) = inbox_dir.parent() {
                    crate::runner::log_runner_error(
                        run_dir,
                        &format!(
                            "hook declared a persisted blob {} for `{}` that is missing from \
                             {}; falling back to the (possibly truncated) inline stdout",
                            b.sha256,
                            ev.cmd,
                            src.display()
                        ),
                    );
                }
                crate::blobs::put_bytes(ctx.paths, ctx.repo_id, ev.stdout.as_bytes())?
            }
        }
        None => crate::blobs::put_bytes(ctx.paths, ctx.repo_id, ev.stdout.as_bytes())?,
    };
    let stderr_sha = if ev.stderr.is_empty() {
        None
    } else {
        Some(crate::blobs::put_bytes(ctx.paths, ctx.repo_id, ev.stderr.as_bytes())?.0)
    };

    // v6 §10.5's `detail` comment for kind=command, plus the three fields brief
    // §6 adds (session_id, tool_use_id, truncated_inline). `env_fingerprint`
    // from v6 is deliberately absent: nothing in Phase 2 produces one, and an
    // empty key would be worse than no key.
    let detail = serde_json::json!({
        "cmd": ev.cmd,
        "cwd": ev.cwd,
        "exit_code": ev.exit_code,
        "duration_ms": ev.duration_ms,
        "stdout_sha": sha,
        "stderr_sha": stderr_sha,
        "session_id": ev.session_id,
        "tool_use_id": ev.tool_use_id,
        "truncated_inline": ev.truncated_inline,
        "event": ev.event,
        "error": ev.error_text,
        "blob_source": blob_source,
    });
    insert_row(
        conn,
        ctx,
        "command",
        &label_from_command(&ev.cmd),
        Some(sha),
        Some(bytes as i64),
        Some(path),
        detail,
    )
}

/// One finished command as a harness's own output stream reported it (codex's
/// `command_execution` item), rather than as a hook saw it.
pub struct StreamCommand<'a> {
    pub cmd: &'a str,
    pub cwd: &'a str,
    pub exit_code: Option<i64>,
    pub status: Option<&'a str>,
    /// stdout and stderr together, as the harness folded them.
    pub output: &'a str,
    /// The harness cut the middle out of `output`.
    pub truncated: bool,
    pub session_id: Option<&'a str>,
    pub item_id: Option<&'a str>,
    /// `detail.event`, naming where the row came from.
    pub event: &'a str,
    /// The harness's own step number, when it numbers its steps; `None`
    /// for codex.
    pub step_index: Option<i64>,
    /// Where the exit code was read, when it came from somewhere other than
    /// the stream (a harness transcript: `generic` or `background_task`);
    /// `None` when the stream itself carried it, or when it is unknown.
    pub exit_code_source: Option<&'a str>,
}

/// Turns one streamed command into an `evidence` row: the same `kind =
/// "command"` shape and `detail` keys as a hook-captured row, so the index,
/// `evidence show` and every citation rule treat it identically, plus the
/// harness's own `status`. The exit code is the real one the harness
/// reported; the blob is the output as the stream carried it.
pub fn record_stream_command(
    conn: &Connection,
    ctx: &EvidenceContext<'_>,
    c: &StreamCommand<'_>,
) -> Result<String> {
    let (sha, bytes, path) = crate::blobs::put_bytes(ctx.paths, ctx.repo_id, c.output.as_bytes())?;
    let detail = serde_json::json!({
        "cmd": c.cmd,
        "cwd": c.cwd,
        "exit_code": c.exit_code,
        "duration_ms": serde_json::Value::Null,
        "stdout_sha": sha,
        "stderr_sha": serde_json::Value::Null,
        "session_id": c.session_id,
        "tool_use_id": c.item_id,
        "truncated_inline": c.truncated,
        "event": c.event,
        "status": c.status,
        "error": serde_json::Value::Null,
        "blob_source": "stream",
        "step_index": c.step_index,
        "exit_code_source": c.exit_code_source,
    });
    insert_row(
        conn,
        ctx,
        "command",
        &label_from_command(c.cmd),
        Some(sha),
        Some(bytes as i64),
        Some(path),
        detail,
    )
}

/// Does this diff touch a test file?
///
/// v6 §4 names the escape hatch this exists to expose: "修改測試讓它通過，是最
/// 經典的逃逸路徑". A reviewer must be told, up front, that the test files
/// moved.
///
/// A heuristic, and deliberately a conservative one: it recognises the
/// directory conventions and file-name suffixes of the ecosystems this repo and
/// its likely tasks use, and nothing cleverer. The rules themselves are
/// `protected::classify`'s built-in test-file rules (paths are lowercased and
/// backslashes folded to forward slashes there). A false negative costs a
/// reviewer one extra `evidence show`; a false positive costs nothing but a
/// raised eyebrow.
pub fn touches_tests(files: &[String]) -> bool {
    files.iter().any(|f| {
        crate::protected::classify(f, &[], &[], &[]) == Some(crate::protected::Kind::Test)
    })
}

/// The first eight characters of a sha, for labels only. Never used to look a
/// commit up.
pub fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// brief §2: the candidate diff, recorded by the control plane once the child
/// has exited cleanly. The blob is the full patch, so a reviewer running
/// `thoth evidence show` on this code is handed the diff itself.
///
/// `git_dir` may be the run's worktree: a linked worktree shares the main
/// tree's object store, so both commits resolve there.
pub fn record_diff(
    conn: &Connection,
    ctx: &EvidenceContext<'_>,
    git_dir: &Path,
    from: &str,
    to: &str,
) -> Result<String> {
    record_diff_attributed(conn, ctx, git_dir, from, to, None)
}

/// SMALL-4: what `protected_changes` is counted against. `task_base` is the
/// task's original base commit (a rework's run starts from the previous
/// candidate); `integration_tip` is the main worktree's HEAD, and a file whose
/// content at the candidate equals its content there is not counted. The tip
/// is used only when it contains `task_base`: a task started on a long-lived
/// branch the main worktree has not merged would otherwise lose a change that
/// happens to put back main's version of a protected file.
pub struct Attribution<'a> {
    pub task_base: &'a str,
    pub integration_tip: Option<&'a str>,
}

/// `record_diff`, with `protected_changes` attributed to the task (see
/// `Attribution`). Everything else in the detail, the blob and the label is
/// still `from..to`. Without an attribution this is `record_diff`.
pub fn record_diff_attributed(
    conn: &Connection,
    ctx: &EvidenceContext<'_>,
    git_dir: &Path,
    from: &str,
    to: &str,
    attribution: Option<&Attribution<'_>>,
) -> Result<String> {
    let stat = crate::worktree::diff_stat(git_dir, from, to)?;
    let files = crate::worktree::diff_name_only(git_dir, from, to)?;
    let patch = crate::worktree::diff_patch(git_dir, from, to)?;
    // P7-2: the existing test files and check inputs this diff modified or
    // deleted (and the added files named like a check word), worked out here
    // by the control plane so neither the worker nor the reviewer has to be
    // trusted to notice. Ingest rule 6 and the review packet read this key.
    let catalogue = crate::protected::Catalogue::load(conn)?;
    let base = attribution.map(|a| a.task_base).unwrap_or(from);
    let own = crate::worktree::diff_name_status(git_dir, base, to)?;
    // The tip filter is used only when the candidate is not already in the
    // tip and the tip contains the task's base; any git failure here turns it
    // off (list more, never fewer).
    let mut tip_used: Option<&str> = attribution
        .and_then(|a| a.integration_tip.map(|tip| (tip, a.task_base)))
        .filter(|(tip, task_base)| {
            *tip != to
                && matches!(crate::worktree::is_ancestor(git_dir, to, tip), Ok(false))
                && matches!(crate::worktree::is_ancestor(git_dir, task_base, tip), Ok(true))
        })
        .map(|(tip, _)| tip);
    // `--no-renames`: with rename detection a moved test's old path would not
    // be in the set, and rule 6 would not see the deletion.
    let keep: Option<Vec<String>> = match tip_used
        .map(|tip| crate::worktree::diff_name_status(git_dir, tip, to))
    {
        Some(Ok(v)) => Some(v.into_iter().map(|(_, p)| p).collect()),
        Some(Err(_)) => {
            tip_used = None;
            None
        }
        None => None,
    };
    let protected_changes: Vec<serde_json::Value> = crate::protected::changes(
        &catalogue,
        &crate::protected::attributed(&own, keep.as_deref()),
    )
    .iter()
    .map(|c| c.to_json())
    .collect();
    let (sha, bytes, path) = crate::blobs::put_bytes(ctx.paths, ctx.repo_id, &patch)?;
    let detail = serde_json::json!({
        "from_commit": from,
        "to_commit": to,
        "files_changed": files,
        "touches_tests": touches_tests(&files),
        "protected_changes": protected_changes,
        "stat": stat,
        "protected_base": base,
        "integration_tip": tip_used,
    });
    let label = format!("git diff {}..{}", short_sha(from), short_sha(to));
    insert_row(
        conn,
        ctx,
        "diff",
        &label,
        Some(sha),
        Some(bytes as i64),
        Some(path),
        detail,
    )
}

/// brief §2: the candidate commit itself. The blob is `git show --stat`, which
/// is what an operator wants when they ask "what is this commit".
pub fn record_commit(
    conn: &Connection,
    ctx: &EvidenceContext<'_>,
    git_dir: &Path,
    sha: &str,
) -> Result<String> {
    let (subject, author_date) = crate::worktree::commit_meta(git_dir, sha)?;
    let stat = crate::worktree::show_stat(git_dir, sha)?;
    let (blob_sha, bytes, path) = crate::blobs::put_bytes(ctx.paths, ctx.repo_id, &stat)?;
    // design-brief.md:83: the commit detail shape is exactly {sha, subject,
    // author_date} - no `stat`. The blob (just above) already holds the full
    // `git show --stat` output, so a `stat` key here would only duplicate it
    // and bloat the row and the index (fix round 1, Task 4 review).
    let detail = serde_json::json!({
        "sha": sha,
        "subject": subject,
        "author_date": author_date,
    });
    let label = format!(
        "commit {} {}",
        short_sha(sha),
        label_from_command(&subject)
    );
    insert_row(
        conn,
        ctx,
        "commit",
        &label,
        Some(blob_sha),
        Some(bytes as i64),
        Some(path),
        detail,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        conn
    }

    fn put(conn: &Connection, task_id: &str, code: &str) {
        crate::db::state::insert_evidence(
            conn,
            &crate::db::state::Evidence {
                evidence_id: format!("{task_id}-{code}"),
                code: code.into(),
                task_id: task_id.into(),
                run_id: None,
                kind: "command".into(),
                collected_by: "control_plane".into(),
                label: "x".into(),
                sha256: None,
                bytes: None,
                blob_path: None,
                detail: "{}".into(),
                created_at: "2026-09-16T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    #[test]
    fn parse_code_number_reads_the_padding_and_rejects_junk() {
        assert_eq!(parse_code_number("E001"), Some(1));
        assert_eq!(parse_code_number("E012"), Some(12));
        assert_eq!(parse_code_number("E999"), Some(999));
        assert_eq!(parse_code_number("E1000"), Some(1000));
        for junk in ["", "E", "001", "X001", "E00a", "e001", "E-1"] {
            assert_eq!(parse_code_number(junk), None, "should reject {junk}");
        }
    }

    #[test]
    fn next_code_starts_at_e001_and_counts_per_task() {
        let conn = db();
        assert_eq!(next_code(&conn, "T1").unwrap(), "E001");

        put(&conn, "T1", "E001");
        assert_eq!(next_code(&conn, "T1").unwrap(), "E002");
        // a different task has its own sequence (v6 §10.5: "per-task 顯示編號")
        assert_eq!(next_code(&conn, "T2").unwrap(), "E001");

        put(&conn, "T2", "E001");
        put(&conn, "T1", "E002");
        assert_eq!(next_code(&conn, "T1").unwrap(), "E003");
        assert_eq!(next_code(&conn, "T2").unwrap(), "E002");
    }

    #[test]
    fn next_code_counts_past_the_zero_padding() {
        let conn = db();
        put(&conn, "T1", "E999");
        assert_eq!(
            next_code(&conn, "T1").unwrap(),
            "E1000",
            "the padding is a minimum width, so the sequence must not wrap or restart"
        );
        put(&conn, "T1", "E1000");
        assert_eq!(next_code(&conn, "T1").unwrap(), "E1001");
    }

    #[test]
    fn next_code_ignores_a_code_it_cannot_parse() {
        let conn = db();
        put(&conn, "T1", "E004");
        put(&conn, "T1", "not-a-code");
        assert_eq!(next_code(&conn, "T1").unwrap(), "E005");
    }

    fn pre(tuid: &str, cmd: &str, at: &str) -> HookLine {
        HookLine {
            received_at: at.into(),
            event: "PreToolUse".into(),
            payload: serde_json::json!({
                "session_id": "s-1",
                "cwd": "C:/wt/a1",
                "hook_event_name": "PreToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": cmd},
                "tool_use_id": tuid
            }),
            blob: None,
            truncated_by_hook: false,
        }
    }

    fn post(tuid: &str, cmd: &str, stdout: &str, at: &str) -> HookLine {
        HookLine {
            received_at: at.into(),
            event: "PostToolUse".into(),
            payload: serde_json::json!({
                "session_id": "s-1",
                "cwd": "C:/wt/a1",
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_input": {"command": cmd},
                "tool_response": {"stdout": stdout, "stderr": ""},
                "tool_use_id": tuid
            }),
            blob: None,
            truncated_by_hook: false,
        }
    }

    #[test]
    fn parse_exit_code_reads_the_phase0_error_string_and_nothing_else() {
        // the real payload string, docs/phase0/02-evidence-capture.md:37
        assert_eq!(parse_exit_code("Exit code 3\noops-stderr"), Some(3));
        assert_eq!(parse_exit_code("Exit code 0\n"), Some(0));
        assert_eq!(parse_exit_code("Exit code 137"), Some(137));
        for no in [
            "",
            "Exit code ",
            "Exit code x",
            "exit code 3",                    // the spec regex has no (?i)
            "Command failed: Exit code 3",     // the pattern is anchored (^)
            "Exitcode 3",
        ] {
            assert_eq!(parse_exit_code(no), None, "should not parse: {no:?}");
        }
    }

    #[test]
    fn label_from_command_cuts_at_eighty_characters_on_a_character_boundary() {
        assert_eq!(label_from_command("seq 1 20000"), "seq 1 20000");
        assert_eq!(label_from_command("  cargo test  "), "cargo test");
        assert_eq!(label_from_command("a\nb\nc"), "a b c");

        let long = "x".repeat(200);
        assert_eq!(label_from_command(&long).chars().count(), LABEL_MAX_CHARS);

        // 100 x "中" is 100 characters / 300 bytes: byte 80 lands inside a
        // character, which is what makes a byte slice panic here.
        let wide = "中".repeat(100);
        let cut = label_from_command(&wide);
        assert_eq!(cut.chars().count(), LABEL_MAX_CHARS);
        assert!(wide.starts_with(&cut));
    }

    #[test]
    fn rfc3339_delta_ms_measures_the_gap_and_tolerates_junk() {
        assert_eq!(
            rfc3339_delta_ms("2026-09-16T00:00:00Z", "2026-09-16T00:00:01.500Z"),
            Some(1500)
        );
        assert_eq!(
            rfc3339_delta_ms("2026-09-16T00:00:00Z", "2026-09-16T00:00:00Z"),
            Some(0)
        );
        assert_eq!(rfc3339_delta_ms("nonsense", "2026-09-16T00:00:00Z"), None);
        assert_eq!(rfc3339_delta_ms("2026-09-16T00:00:00Z", "nonsense"), None);
    }

    #[test]
    fn the_pairer_matches_on_tool_use_id_even_when_the_posts_arrive_out_of_order() {
        let mut p = Pairer::default();
        assert!(p.offer(&pre("t1", "first", "2026-09-16T00:00:00Z")).is_none());
        assert!(p.offer(&pre("t2", "second", "2026-09-16T00:00:01Z")).is_none());
        assert_eq!(p.pending(), 2);

        // the second command finishes first
        let ev = p
            .offer(&post("t2", "second", "two", "2026-09-16T00:00:02Z"))
            .expect("a Post must yield evidence");
        assert_eq!(ev.cmd, "second");
        assert_eq!(ev.tool_use_id.as_deref(), Some("t2"));
        assert_eq!(ev.exit_code, Some(0), "a PostToolUse implies success");
        assert_eq!(ev.stdout, "two");
        assert_eq!(ev.cwd, "C:/wt/a1");
        assert_eq!(ev.session_id.as_deref(), Some("s-1"));
        assert_eq!(p.pending(), 1, "only the matched Pre is consumed");

        let ev = p
            .offer(&post("t1", "first", "one", "2026-09-16T00:00:03Z"))
            .unwrap();
        assert_eq!(ev.cmd, "first");
        assert_eq!(p.pending(), 0);
    }

    #[test]
    fn the_pairer_falls_back_to_fifo_when_there_is_no_tool_use_id() {
        let mut p = Pairer::default();
        let mut bare_pre = pre("ignored", "only command", "2026-09-16T00:00:00Z");
        bare_pre.payload["tool_use_id"] = serde_json::Value::Null;
        assert!(p.offer(&bare_pre).is_none());

        let mut bare_post = post("ignored", "only command", "out", "2026-09-16T00:00:00.400Z");
        bare_post.payload["tool_use_id"] = serde_json::Value::Null;
        let ev = p.offer(&bare_post).unwrap();
        assert_eq!(ev.cmd, "only command");
        assert_eq!(ev.tool_use_id, None);
        // no duration_ms in the payload, so it comes from the received_at gap
        assert_eq!(ev.duration_ms, Some(400));
        assert_eq!(p.pending(), 0);
    }

    #[test]
    fn a_failure_event_parses_its_exit_code_and_keeps_the_error_text_as_output() {
        let mut p = Pairer::default();
        p.offer(&pre("t1", "sh -c 'exit 3'", "2026-09-16T00:00:00Z"));
        let line = HookLine {
            received_at: "2026-09-16T00:00:00.077Z".into(),
            event: "PostToolUseFailure".into(),
            payload: serde_json::json!({
                "session_id": "s-1",
                "cwd": "C:/wt/a1",
                "hook_event_name": "PostToolUseFailure",
                "tool_name": "Bash",
                "tool_input": {"command": "sh -c 'exit 3'"},
                "tool_use_id": "t1",
                "error": "Exit code 3\noops-stderr",
                "is_interrupt": false,
                "duration_ms": 77
            }),
            blob: None,
            truncated_by_hook: false,
        };
        let ev = p.offer(&line).unwrap();
        assert_eq!(ev.event, "PostToolUseFailure");
        assert_eq!(ev.exit_code, Some(3));
        assert_eq!(ev.duration_ms, Some(77), "the payload's own value wins");
        assert_eq!(
            ev.stdout, "oops-stderr",
            "a failure has no tool_response; its output is the text after the exit-code line"
        );
        assert_eq!(ev.stderr, "");
        assert_eq!(ev.error_text.as_deref(), Some("Exit code 3\noops-stderr"));
        assert!(!ev.truncated_inline);
    }

    #[test]
    fn an_unparseable_failure_message_leaves_the_exit_code_null_but_keeps_the_text() {
        let mut p = Pairer::default();
        p.offer(&pre("t1", "weird", "2026-09-16T00:00:00Z"));
        let mut line = HookLine {
            received_at: "2026-09-16T00:00:01Z".into(),
            event: "PostToolUseFailure".into(),
            payload: serde_json::json!({
                "hook_event_name": "PostToolUseFailure",
                "tool_input": {"command": "weird"},
                "tool_use_id": "t1",
                "error": "the tool was interrupted"
            }),
            blob: None,
            truncated_by_hook: false,
        };
        line.payload["is_interrupt"] = serde_json::Value::Bool(true);
        let ev = p.offer(&line).unwrap();
        assert_eq!(ev.exit_code, None);
        assert_eq!(ev.error_text.as_deref(), Some("the tool was interrupted"));
    }

    #[test]
    fn a_failed_command_stores_its_output_in_the_primary_blob() {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let inbox = home.path().join("inbox");
        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };
        let mut p = Pairer::default();
        p.offer(&pre("t1", "cargo test", "2026-09-16T00:00:00Z"));
        let line = HookLine {
            received_at: "2026-09-16T00:00:01Z".into(),
            event: "PostToolUseFailure".into(),
            payload: serde_json::json!({
                "hook_event_name": "PostToolUseFailure",
                "tool_input": {"command": "cargo test"},
                "tool_use_id": "t1",
                "error": "Exit code 101\nline one\nline two",
                "is_interrupt": false
            }),
            blob: None,
            truncated_by_hook: false,
        };
        let ev = p.offer(&line).unwrap();
        record_command(&conn, &ctx, &inbox, &ev).unwrap();

        let row = crate::db::state::get_evidence_by_code(&conn, "T1", "E001")
            .unwrap()
            .unwrap();
        let blob = std::path::PathBuf::from(row.blob_path.clone().unwrap());
        assert_eq!(std::fs::read_to_string(&blob).unwrap(), "line one\nline two");
        let detail: serde_json::Value = serde_json::from_str(&row.detail).unwrap();
        assert_eq!(detail["exit_code"], 101);
        assert_eq!(detail["stdout_sha"], row.sha256.clone().unwrap());
        assert_eq!(detail["error"], "Exit code 101\nline one\nline two");

        // an error with no newline is stored whole
        let mut p = Pairer::default();
        p.offer(&pre("t2", "weird", "2026-09-16T00:00:00Z"));
        let mut line = line;
        line.payload["tool_use_id"] = serde_json::json!("t2");
        line.payload["tool_input"] = serde_json::json!({"command": "weird"});
        line.payload["error"] = serde_json::json!("the tool was interrupted");
        let ev = p.offer(&line).unwrap();
        assert_eq!(ev.stdout, "the tool was interrupted");
    }

    fn failure_line(error: &str) -> HookLine {
        HookLine {
            received_at: "2026-09-16T00:00:01Z".into(),
            event: "PostToolUseFailure".into(),
            payload: serde_json::json!({
                "hook_event_name": "PostToolUseFailure",
                "tool_input": {"command": "cargo test"},
                "tool_use_id": "t1",
                "error": error,
                "is_interrupt": false
            }),
            blob: None,
            truncated_by_hook: false,
        }
    }

    #[test]
    fn a_characters_marker_in_a_failure_output_marks_it_truncated() {
        let mut p = Pairer::default();
        p.offer(&pre("t1", "cargo test", "2026-09-16T00:00:00Z"));
        let ev = p
            .offer(&failure_line(
                "Exit code 101\nhead\n\n... [4321 characters truncated] ...\n\ntail",
            ))
            .unwrap();
        assert!(ev.truncated_inline);
    }

    #[test]
    fn a_lines_marker_in_a_success_output_marks_it_truncated() {
        let mut p = Pairer::default();
        p.offer(&pre("t1", "seq 1 9999", "2026-09-16T00:00:00Z"));
        let mut line = post(
            "t1",
            "seq 1 9999",
            "1\n2\n... [900 lines truncated] ...\n9999\n",
            "2026-09-16T00:00:01Z",
        );
        line.event = "PostToolUse".into();
        assert!(p.offer(&line).unwrap().truncated_inline);
    }

    #[test]
    fn an_output_truncated_marker_marks_it_truncated() {
        let mut p = Pairer::default();
        p.offer(&pre("t1", "cargo test", "2026-09-16T00:00:00Z"));
        let ev = p
            .offer(&failure_line("Exit code 1\nabc\n[output truncated at 30000 bytes]"))
            .unwrap();
        assert!(ev.truncated_inline);
    }

    #[test]
    fn a_marker_in_the_exit_code_line_or_absent_leaves_it_false() {
        let mut p = Pairer::default();
        p.offer(&pre("t1", "cargo test", "2026-09-16T00:00:00Z"));
        let ev = p.offer(&failure_line("Exit code 1\nplain output, 12 lines")).unwrap();
        assert!(!ev.truncated_inline);
        assert!(!has_claude_truncation_marker("nothing cut here"));
    }

    #[test]
    fn a_persisted_output_marks_the_evidence_as_inline_truncated() {
        let mut p = Pairer::default();
        p.offer(&pre("t1", "seq 1 20000", "2026-09-16T00:00:00Z"));
        let mut line = post("t1", "seq 1 20000", "1\n2\n3\n", "2026-09-16T00:00:00.083Z");
        line.event = "PostToolUse".into();
        line.payload["tool_response"]["persistedOutputPath"] =
            serde_json::json!("C:/cfg/tool-results/x.txt");
        line.payload["tool_response"]["persistedOutputSize"] = serde_json::json!(108_894);
        line.blob = Some(HookBlob {
            sha256: "deadbeef".into(),
            bytes: 108_894,
            source: "persisted".into(),
        });
        let ev = p.offer(&line).unwrap();
        assert!(ev.truncated_inline, "the inline stdout was cut at 30000 characters");
        assert_eq!(ev.blob.as_ref().unwrap().bytes, 108_894);
    }

    #[test]
    fn parse_hook_line_reads_what_the_hook_wrote_and_rejects_junk() {
        let line = crate::commands::hook::record_line(
            "2026-09-16T00:00:00Z",
            "PostToolUse",
            serde_json::json!({
                "hook_event_name": "PostToolUse",
                "tool_response": {"stdout": "ok", "stderr": ""}
            }),
            Some(&HookBlob { sha256: "abc".into(), bytes: 2, source: "persisted".into() }),
        );
        let parsed = parse_hook_line(&line).unwrap();
        assert_eq!(parsed.event, "PostToolUse");
        assert_eq!(parsed.received_at, "2026-09-16T00:00:00Z");
        assert_eq!(parsed.payload["tool_response"]["stdout"], "ok");
        assert_eq!(parsed.blob.unwrap().sha256, "abc");
        assert!(!parsed.truncated_by_hook);

        assert!(parse_hook_line("half a line {").is_err());
        assert!(
            parse_hook_line(r#"{"received_at":"x","event":"PostToolUse"}"#).is_err(),
            "a line with no payload is unusable"
        );
    }

    #[test]
    fn the_tailer_returns_only_new_complete_lines() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let path = inbox.join(crate::commands::hook::HOOKS_FILE);

        let mut t = InboxTailer::new(&inbox);
        assert!(t.read_new_lines().is_empty(), "no file yet");

        std::fs::write(&path, "one\ntwo\n").unwrap();
        assert_eq!(t.read_new_lines(), vec!["one".to_string(), "two".to_string()]);
        assert!(t.read_new_lines().is_empty(), "already consumed");

        // an append is picked up, and a line the writer has not finished is held
        // back until its newline arrives
        std::fs::write(&path, "one\ntwo\nthree\nfou").unwrap();
        assert_eq!(t.read_new_lines(), vec!["three".to_string()]);
        std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();
        assert_eq!(t.read_new_lines(), vec!["four".to_string()]);
    }

    #[test]
    fn recording_a_command_allocates_a_code_stores_blobs_and_logs_an_event() {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let inbox = home.path().join("inbox");
        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };

        let ev = CommandEvidence {
            cmd: "cargo test".into(),
            cwd: "C:/wt/a1".into(),
            event: "PostToolUseFailure".into(),
            exit_code: Some(101),
            duration_ms: Some(4200),
            session_id: Some("s-1".into()),
            tool_use_id: Some("t1".into()),
            stdout: "running 3 tests".into(),
            stderr: "test foo ... FAILED".into(),
            error_text: Some("Exit code 101\ntest foo ... FAILED".into()),
            truncated_inline: false,
            blob: None,
        };
        let code = record_command(&conn, &ctx, &inbox, &ev).unwrap();
        assert_eq!(code, "E001");

        let row = crate::db::state::get_evidence_by_code(&conn, "T1", "E001")
            .unwrap()
            .unwrap();
        assert_eq!(row.kind, "command");
        assert_eq!(row.collected_by, COLLECTED_BY_CONTROL_PLANE);
        assert_eq!(row.label, "cargo test");
        assert_eq!(row.run_id.as_deref(), Some("R1"));
        assert_eq!(row.bytes, Some(15), "\"running 3 tests\" is 15 bytes");
        let blob = std::path::PathBuf::from(row.blob_path.clone().unwrap());
        assert!(blob.is_file());
        assert!(crate::blobs::verify(&blob, row.sha256.as_deref().unwrap()));
        assert_eq!(std::fs::read_to_string(&blob).unwrap(), "running 3 tests");

        let detail: serde_json::Value = serde_json::from_str(&row.detail).unwrap();
        assert_eq!(detail["cmd"], "cargo test");
        assert_eq!(detail["cwd"], "C:/wt/a1");
        assert_eq!(detail["exit_code"], 101);
        assert_eq!(detail["duration_ms"], 4200);
        assert_eq!(detail["session_id"], "s-1");
        assert_eq!(detail["tool_use_id"], "t1");
        assert_eq!(detail["truncated_inline"], false);
        assert_eq!(detail["blob_source"], "inline");
        assert_eq!(detail["stdout_sha"], row.sha256.clone().unwrap());
        // a non-empty stderr gets its own blob, and its digest is recorded
        let stderr_sha = detail["stderr_sha"].as_str().unwrap();
        assert!(crate::blobs::get_path(&paths, "repo01", stderr_sha).is_some());

        // and the event log names the row
        let evs = crate::db::state::events_since(&conn, 0, Some("T1")).unwrap();
        let added: Vec<&crate::db::state::EventRow> = evs
            .iter()
            .filter(|e| e.event_type == "evidence_added")
            .collect();
        assert_eq!(added.len(), 1);
        assert!(added[0].detail.contains("\"code\":\"E001\""));

        // a second command takes the next code
        assert_eq!(record_command(&conn, &ctx, &inbox, &ev).unwrap(), "E002");
    }

    #[test]
    fn a_hook_stored_blob_is_promoted_into_the_repo_store_rather_than_rebuilt() {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let inbox = home.path().join("inbox");
        let body: String = (1..=20000).map(|n| format!("{n}\n")).collect();
        let (sha, bytes, _) =
            crate::blobs::put_bytes_into(&inbox.join(crate::commands::hook::BLOBS_DIR), body.as_bytes())
                .unwrap();
        assert_eq!(bytes, 108_894);

        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };
        let ev = CommandEvidence {
            cmd: "seq 1 20000".into(),
            cwd: "C:/wt/a1".into(),
            event: "PostToolUse".into(),
            exit_code: Some(0),
            duration_ms: Some(83),
            session_id: None,
            tool_use_id: None,
            // the inline stdout is the truncated version; the blob is complete
            stdout: "1\n2\n3\n".into(),
            stderr: String::new(),
            error_text: None,
            truncated_inline: true,
            blob: Some(HookBlob { sha256: sha.clone(), bytes: 108_894, source: "persisted".into() }),
        };
        record_command(&conn, &ctx, &inbox, &ev).unwrap();

        let row = crate::db::state::get_evidence_by_code(&conn, "T1", "E001")
            .unwrap()
            .unwrap();
        assert_eq!(row.sha256.as_deref(), Some(sha.as_str()));
        assert_eq!(
            row.bytes,
            Some(108_894),
            "the complete bytes, not the 6-byte inline preview"
        );
        let detail: serde_json::Value = serde_json::from_str(&row.detail).unwrap();
        assert_eq!(detail["blob_source"], "persisted");
        assert_eq!(detail["truncated_inline"], true);
        assert_eq!(detail["stderr_sha"], serde_json::Value::Null);
        assert!(std::path::Path::new(row.blob_path.as_deref().unwrap()).is_file());
    }

    #[test]
    fn touches_tests_recognises_the_usual_test_locations() {
        for yes in [
            "tests/thing_test.rs",
            "tests/common/mod.rs",
            "crates/core/tests/api.rs",
            "test/helper.js",
            "src/__tests__/button.tsx",
            "src/test_parser.py",
            "pkg/handler_test.go",
            "web/button.test.ts",
            "web/button.spec.js",
            "conftest.py",
            "backend/conftest.py",
            r"tests\windows_style.rs",
        ] {
            assert!(
                touches_tests(&[yes.to_string()]),
                "should count as a test file: {yes}"
            );
        }
        for no in [
            "src/main.rs",
            "README.md",
            "src/latest.rs",
            "docs/contest.md",
            "src/protest/mod.rs",
        ] {
            assert!(
                !touches_tests(&[no.to_string()]),
                "should not count as a test file: {no}"
            );
        }
        // one hit anywhere in the list is enough, and an empty diff is not one
        assert!(touches_tests(&["src/main.rs".into(), "tests/x.rs".into()]));
        assert!(!touches_tests(&[]));
    }

    #[test]
    fn short_sha_takes_eight_characters_and_tolerates_a_short_input() {
        assert_eq!(short_sha("a3b24ca44a5486feeef7b12776732f0f4011c71e"), "a3b24ca4");
        assert_eq!(short_sha("abc"), "abc");
        assert_eq!(short_sha(""), "");
    }

    /// A throwaway repo whose second commit touches a test file. Duplicated
    /// from `src/worktree.rs`'s test module on purpose: the two modules are
    /// tested independently and a shared helper would couple them.
    fn repo_with_two_commits() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(r)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "thoth test"]);
        std::fs::write(r.join("a.txt"), "one\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-m", "first"]);
        let base = crate::worktree::rev_parse_head(r).unwrap();
        std::fs::create_dir_all(r.join("tests")).unwrap();
        std::fs::write(r.join("tests").join("added_test.rs"), "#[test]\nfn t() {}\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "second: add a test"]);
        let head = crate::worktree::rev_parse_head(r).unwrap();
        (dir, base, head)
    }

    #[test]
    fn record_diff_stores_the_patch_and_flags_the_test_file() {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let (repo, base, head) = repo_with_two_commits();
        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };

        let code = record_diff(&conn, &ctx, repo.path(), &base, &head).unwrap();
        assert_eq!(code, "E001");
        let row = crate::db::state::get_evidence_by_code(&conn, "T1", "E001")
            .unwrap()
            .unwrap();
        assert_eq!(row.kind, "diff");
        assert_eq!(row.collected_by, COLLECTED_BY_CONTROL_PLANE);
        assert_eq!(
            row.label,
            format!("git diff {}..{}", short_sha(&base), short_sha(&head))
        );

        let detail: serde_json::Value = serde_json::from_str(&row.detail).unwrap();
        assert_eq!(detail["from_commit"], base);
        assert_eq!(detail["to_commit"], head);
        assert_eq!(
            detail["files_changed"],
            serde_json::json!(["tests/added_test.rs"])
        );
        assert_eq!(
            detail["touches_tests"], true,
            "v6 §4: a reviewer has to be told when the test files moved"
        );
        assert!(detail["stat"].as_str().unwrap().contains("1 file changed"));
        assert_eq!(
            detail["protected_changes"],
            serde_json::json!([]),
            "P7-2: an added test file is not a protected change"
        );

        // the blob is the patch itself, so `evidence show` hands over a diff
        let blob = std::path::PathBuf::from(row.blob_path.clone().unwrap());
        let patch = std::fs::read_to_string(&blob).unwrap();
        assert!(patch.contains("+fn t() {}"), "got {patch}");
        assert!(crate::blobs::verify(&blob, row.sha256.as_deref().unwrap()));
        assert_eq!(row.bytes, Some(patch.len() as i64));
    }

    /// P7-2 / AC3: one existing test file modified, one test file added, one
    /// file that is an input of a check deleted - exactly two entries, and the
    /// added file is not one of them.
    #[test]
    fn record_diff_lists_the_modified_test_file_and_the_deleted_check_input_only() {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(r)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "thoth test"]);
        std::fs::create_dir_all(r.join("tests")).unwrap();
        std::fs::create_dir_all(r.join("data")).unwrap();
        std::fs::write(r.join("tests").join("existing_test.rs"), "#[test]\nfn t() { assert!(false); }\n").unwrap();
        std::fs::write(r.join("data").join("golden.json"), "{}\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "first"]);
        let base = crate::worktree::rev_parse_head(r).unwrap();

        crate::checks::put_check(
            &conn,
            &crate::checks::Check {
                check_id: "golden".into(),
                shell: "argv".into(),
                cmd: "nosuchexe-zz --compare".into(),
                cwd: ".".into(),
                inputs: vec!["data/golden.json".into()],
                created_at: "n".into(),
                approved_at: None,
                approved_fingerprint: None,
                approved_detail: None,
                approved_commit: None,
                exe_pin: "hash".into(),
            },
        )
        .unwrap();

        // modify the existing test, add a new test, delete the check's input
        std::fs::write(r.join("tests").join("existing_test.rs"), "#[test]\nfn t() {}\n").unwrap();
        std::fs::write(r.join("tests").join("new_test.rs"), "#[test]\nfn n() {}\n").unwrap();
        std::fs::remove_file(r.join("data").join("golden.json")).unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "second"]);
        let head = crate::worktree::rev_parse_head(r).unwrap();

        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };
        let code = record_diff(&conn, &ctx, r, &base, &head).unwrap();
        let row = crate::db::state::get_evidence_by_code(&conn, "T1", &code)
            .unwrap()
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&row.detail).unwrap();
        let listed = detail["protected_changes"].as_array().unwrap();
        assert_eq!(listed.len(), 2, "exactly two entries: {listed:?}");
        assert_eq!(
            detail["protected_changes"],
            serde_json::json!([
                {"path": "data/golden.json", "status": "deleted", "kind": "check_input"},
                {"path": "tests/existing_test.rs", "status": "modified", "kind": "test"},
            ]),
            "sorted by path"
        );
        assert!(
            !listed.iter().any(|c| c["path"] == "tests/new_test.rs"),
            "an added test file is not a protected change"
        );
        // the keys that were already there keep their meaning
        assert_eq!(
            detail["files_changed"],
            serde_json::json!(["data/golden.json", "tests/existing_test.rs", "tests/new_test.rs"])
        );
        assert_eq!(detail["touches_tests"], true);
    }

    /// P7-2 / AC12 (c): a test file whose name holds a double quote is still
    /// listed - git's quoted form of the path must not reach the classifier.
    /// Not on Windows: a file name cannot hold `"` there.
    #[cfg(not(windows))]
    #[test]
    fn record_diff_lists_a_modified_test_file_whose_name_has_a_double_quote() {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(r)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "thoth test"]);
        std::fs::create_dir_all(r.join("tests")).unwrap();
        std::fs::write(r.join("tests").join("a\"b.rs"), "one\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "first"]);
        let base = crate::worktree::rev_parse_head(r).unwrap();
        std::fs::write(r.join("tests").join("a\"b.rs"), "two\n").unwrap();
        run(&["add", "-A"]);
        run(&["commit", "-m", "second"]);
        let head = crate::worktree::rev_parse_head(r).unwrap();

        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };
        let code = record_diff(&conn, &ctx, r, &base, &head).unwrap();
        let row = crate::db::state::get_evidence_by_code(&conn, "T1", &code)
            .unwrap()
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&row.detail).unwrap();
        let listed = detail["protected_changes"].as_array().unwrap();
        assert_eq!(listed.len(), 1, "got {listed:?}");
        assert_eq!(listed[0]["path"], "tests/a\"b.rs");
        assert_eq!(listed[0]["status"], "modified");
        assert_eq!(listed[0]["kind"], "test");
        assert_eq!(detail["touches_tests"], true);
    }

    #[test]
    fn record_diff_and_commit_blobs_are_git_raw_bytes_even_when_not_utf8() {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let (repo, base, _head) = repo_with_two_commits();
        let r = repo.path();
        // cp950 bytes for two Chinese characters: not valid UTF-8, no NUL
        let legacy: &[u8] = b"line \xa4\xa4\xa4\xe5 end\n";
        std::fs::write(r.join("legacy.txt"), legacy).unwrap();
        for args in [
            vec!["add", "legacy.txt"],
            vec!["commit", "-m", "add legacy file"],
        ] {
            let out = std::process::Command::new("git").args(&args).current_dir(r).output().unwrap();
            assert!(out.status.success());
        }
        let head = crate::worktree::rev_parse_head(r).unwrap();
        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };

        let range = format!("{base}..{head}");
        let raw_diff = std::process::Command::new("git")
            .args(["diff", "--patch", "--stat", &range])
            .current_dir(r)
            .output()
            .unwrap()
            .stdout;
        let raw_show = std::process::Command::new("git")
            .args(["show", "--stat", "--format=%H%n%s%n%aI", &head])
            .current_dir(r)
            .output()
            .unwrap()
            .stdout;
        assert!(raw_diff.windows(legacy.len()).any(|w| w == legacy), "git itself emits the raw bytes");

        let d = record_diff(&conn, &ctx, r, &base, &head).unwrap();
        let c = record_commit(&conn, &ctx, r, &head).unwrap();
        for (code, raw) in [(d, raw_diff), (c, raw_show)] {
            let row = crate::db::state::get_evidence_by_code(&conn, "T1", &code)
                .unwrap()
                .unwrap();
            let blob = std::path::PathBuf::from(row.blob_path.clone().unwrap());
            let stored = std::fs::read(&blob).unwrap();
            if row.kind == "diff" {
                assert_eq!(stored, raw, "diff blob must be git's exact stdout");
                assert!(!stored.windows(3).any(|w| w == [0xEF, 0xBF, 0xBD]), "no U+FFFD");
            } else {
                assert_eq!(stored, raw, "commit blob must be git's exact stdout");
            }
            assert_eq!(row.bytes, Some(raw.len() as i64));
            assert!(crate::blobs::verify(&blob, row.sha256.as_deref().unwrap()));
        }
    }

    #[test]
    fn record_commit_stores_the_subject_date_and_stat() {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let (repo, _base, head) = repo_with_two_commits();
        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };

        let code = record_commit(&conn, &ctx, repo.path(), &head).unwrap();
        assert_eq!(code, "E001");
        let row = crate::db::state::get_evidence_by_code(&conn, "T1", "E001")
            .unwrap()
            .unwrap();
        assert_eq!(row.kind, "commit");
        assert!(row.label.starts_with(&format!("commit {}", short_sha(&head))));
        assert!(row.label.contains("second: add a test"));

        let detail: serde_json::Value = serde_json::from_str(&row.detail).unwrap();
        assert_eq!(detail["sha"], head);
        assert_eq!(detail["subject"], "second: add a test");
        assert!(detail["author_date"].as_str().unwrap().contains('T'));
        // Fix round 1 / Task 4 review: design-brief.md:83 defines the commit
        // detail shape as exactly {sha, subject, author_date} - no `stat`, which
        // would only duplicate the blob below.
        let keys: std::collections::BTreeSet<&str> =
            detail.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        assert_eq!(
            keys,
            ["sha", "subject", "author_date"].into_iter().collect(),
            "commit detail must carry exactly these three keys, got {detail}"
        );

        // the stat lives in the blob, not the detail
        let blob = std::path::PathBuf::from(row.blob_path.clone().unwrap());
        let stat = std::fs::read_to_string(&blob).unwrap();
        assert!(stat.contains("1 file changed"), "got {stat}");
    }

    /// RD-1 (AC4): a label leaves masked; the code does not change.
    #[test]
    fn refresh_index_masks_labels() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let var = "RD_UNIT_INDEX_TOKEN";
        let value = "dummy-index-secret-1";
        let token = format!("{}{}", ["gh", "p_"].concat(), "dummyDUMMY0123456789dummyDUMMY012345");
        std::env::set_var(var, value);
        let conn = db();
        let wt = tempfile::tempdir().unwrap();
        crate::db::state::insert_evidence(
            &conn,
            &crate::db::state::Evidence {
                evidence_id: "EV1".into(),
                code: "E001".into(),
                task_id: "T1".into(),
                run_id: Some("R1".into()),
                kind: "command".into(),
                collected_by: COLLECTED_BY_CONTROL_PLANE.into(),
                label: format!("curl -u {value} -H {token}"),
                sha256: None,
                bytes: None,
                blob_path: None,
                detail: serde_json::json!({"exit_code": 0}).to_string(),
                created_at: "2026-10-27T00:00:00Z".into(),
            },
        )
        .unwrap();
        let written = refresh_index(&conn, "T1", wt.path());
        std::env::remove_var(var);
        assert_eq!(written.unwrap(), 1);
        let text = std::fs::read_to_string(index_path(wt.path())).unwrap();
        assert!(!text.contains(value), "{text}");
        assert!(!text.contains(&token), "{text}");
        assert!(text.contains("[redacted:github-token]"), "{text}");
        assert!(text.contains("[redacted:env:"), "{text}");
        let rows: Vec<IndexRow> = serde_json::from_str(&text).unwrap();
        assert_eq!(rows[0].code, "E001");
    }

    #[test]
    fn refresh_index_writes_the_v6_index_shape_atomically() {
        let conn = db();
        let wt = tempfile::tempdir().unwrap();
        crate::db::state::insert_evidence(
            &conn,
            &crate::db::state::Evidence {
                evidence_id: "EV1".into(),
                code: "E001".into(),
                task_id: "T1".into(),
                run_id: Some("R1".into()),
                kind: "command".into(),
                collected_by: COLLECTED_BY_CONTROL_PLANE.into(),
                label: "cargo test".into(),
                sha256: None,
                bytes: None,
                blob_path: None,
                detail: serde_json::json!({"cmd": "cargo test", "exit_code": 0}).to_string(),
                created_at: "2026-09-16T00:00:00Z".into(),
            },
        )
        .unwrap();
        crate::db::state::insert_evidence(
            &conn,
            &crate::db::state::Evidence {
                evidence_id: "EV2".into(),
                code: "E002".into(),
                task_id: "T1".into(),
                run_id: Some("R1".into()),
                kind: "diff".into(),
                collected_by: COLLECTED_BY_CONTROL_PLANE.into(),
                label: "git diff aaaaaaaa..bbbbbbbb".into(),
                sha256: None,
                bytes: None,
                blob_path: None,
                detail: serde_json::json!({"from_commit": "a", "to_commit": "b"}).to_string(),
                created_at: "2026-09-16T00:00:01Z".into(),
            },
        )
        .unwrap();

        assert_eq!(refresh_index(&conn, "T1", wt.path()).unwrap(), 2);
        let path = index_path(wt.path());
        assert_eq!(path, wt.path().join(".agent").join("evidence-index.json"));
        let rows: Vec<IndexRow> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].code, "E001");
        assert_eq!(rows[0].kind, "command");
        assert_eq!(rows[0].label, "cargo test");
        assert_eq!(rows[0].exit_code, Some(0));
        assert_eq!(rows[1].code, "E002");
        assert_eq!(rows[1].exit_code, None, "a diff has no exit code");

        // rewriting replaces rather than appends, and leaves no temp behind
        assert_eq!(refresh_index(&conn, "T1", wt.path()).unwrap(), 2);
        let rows: Vec<IndexRow> =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(rows.len(), 2);
        let leftovers: Vec<String> = std::fs::read_dir(wt.path().join(".agent"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    // --- Fix round 1 -----------------------------------------------------

    /// I1: a fresh `InboxTailer` over an inbox an earlier one already drained
    /// (the exact shape of a resumed run's new runner process) must not
    /// replay lines the earlier one turned into evidence.
    #[test]
    fn the_tailer_persists_its_offset_so_a_fresh_instance_does_not_replay_old_lines() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let path = inbox.join(crate::commands::hook::HOOKS_FILE);
        std::fs::write(&path, "one\ntwo\nthree\nfour\n").unwrap();

        let mut t = InboxTailer::new(&inbox);
        assert_eq!(
            t.read_new_lines(),
            vec!["one", "two", "three", "four"]
                .into_iter()
                .map(String::from)
                .collect::<Vec<_>>()
        );
        assert!(inbox.join(".offset").is_file(), "the offset must be persisted");

        // simulate a runner restart: a brand new tailer over the same inbox,
        // with no memory of what the first one already returned.
        let mut t2 = InboxTailer::new(&inbox);
        assert!(
            t2.read_new_lines().is_empty(),
            "the persisted offset must skip already-drained lines"
        );

        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\nsix\n").unwrap();
        assert_eq!(
            t2.read_new_lines(),
            vec!["five".to_string(), "six".to_string()]
        );
    }

    /// A persisted offset must survive a restart even when the last read left
    /// an unterminated trailing line in memory - that memory (`partial`) does
    /// not survive the restart, but the bytes on disk do.
    #[test]
    fn a_restart_after_a_partial_trailing_line_still_recovers_it() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let path = inbox.join(crate::commands::hook::HOOKS_FILE);
        std::fs::write(&path, "one\ntwo\nthr").unwrap();

        let mut t = InboxTailer::new(&inbox);
        assert_eq!(t.read_new_lines(), vec!["one".to_string(), "two".to_string()]);

        // "restart": a fresh tailer, `partial` gone, offset re-read from disk
        let mut t2 = InboxTailer::new(&inbox);
        assert!(t2.read_new_lines().is_empty(), "the trailing partial line is not complete yet");

        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert_eq!(
            t2.read_new_lines(),
            vec!["three".to_string()],
            "the partial bytes are still on disk and must be recovered whole, not truncated"
        );
    }

    /// I1: a persisted offset that no longer fits the file on disk (rotated
    /// out from under the tailer, or corrupted) must reset to 0 rather than
    /// silently reporting "nothing new" forever.
    #[test]
    fn a_persisted_offset_past_the_current_file_length_resets_to_zero_and_logs() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("runs").join("R1");
        let inbox = run_dir.join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let path = inbox.join(crate::commands::hook::HOOKS_FILE);
        std::fs::write(&path, "one\ntwo\n").unwrap();

        let mut t = InboxTailer::new(&inbox);
        t.read_new_lines();
        assert!(inbox.join(".offset").is_file());

        // the file on disk is now shorter than the persisted offset
        std::fs::write(&path, "x\n").unwrap();
        let mut t2 = InboxTailer::new(&inbox);
        assert_eq!(t2.read_new_lines(), vec!["x".to_string()], "must start over from 0, not skip x");

        let log = std::fs::read_to_string(run_dir.join("runner-error.log")).unwrap();
        assert!(log.contains("resetting to 0"), "got: {log}");
    }

    /// M3: a `Post` naming a `tool_use_id` that has no matching pending `Pre`
    /// must not steal an unrelated `Pre` off the front of the queue.
    #[test]
    fn an_unmatched_tool_use_id_does_not_steal_the_oldest_pending_pre() {
        let mut p = Pairer::default();
        p.offer(&pre("t1", "first", "2026-09-16T00:00:00Z"));
        assert_eq!(p.pending(), 1);

        let ev = p
            .offer(&post("t-unknown", "second", "out", "2026-09-16T00:00:01Z"))
            .unwrap();
        assert_eq!(ev.cmd, "second", "must not borrow t1's cmd");
        assert_eq!(ev.tool_use_id.as_deref(), Some("t-unknown"));
        assert_eq!(
            ev.duration_ms, None,
            "no matching Pre to diff against, and the payload has no duration_ms of its own"
        );
        assert_eq!(p.pending(), 1, "t1's Pre must remain pending, not be stolen");

        // and t1's own Post still resolves correctly afterward
        let ev2 = p.offer(&post("t1", "first", "one", "2026-09-16T00:00:02Z")).unwrap();
        assert_eq!(ev2.cmd, "first");
        assert_eq!(p.pending(), 0);
    }

    /// M4: a hook line that declared a persisted blob whose file is missing
    /// from the inbox's blobs directory must degrade to the inline stdout
    /// (already covered indirectly, but this proves it end to end from
    /// `record_command`) and log the degradation.
    #[test]
    fn a_declared_blob_missing_from_disk_falls_back_to_inline_and_logs_the_degradation() {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let run_dir = home.path().join("runs").join("R1");
        let inbox = run_dir.join("inbox");
        std::fs::create_dir_all(&inbox).unwrap();
        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };
        let ev = CommandEvidence {
            cmd: "seq 1 20000".into(),
            cwd: "C:/wt/a1".into(),
            event: "PostToolUse".into(),
            exit_code: Some(0),
            duration_ms: Some(83),
            session_id: None,
            tool_use_id: None,
            stdout: "1\n2\n3\n".into(),
            stderr: String::new(),
            error_text: None,
            truncated_inline: true,
            blob: Some(HookBlob {
                sha256: "deadbeef-not-on-disk".into(),
                bytes: 108_894,
                source: "persisted".into(),
            }),
        };
        record_command(&conn, &ctx, &inbox, &ev).unwrap();

        let row = crate::db::state::get_evidence_by_code(&conn, "T1", "E001")
            .unwrap()
            .unwrap();
        let detail: serde_json::Value = serde_json::from_str(&row.detail).unwrap();
        assert_eq!(detail["blob_source"], "inline", "the declared blob is missing, so this falls back");
        assert_eq!(row.bytes, Some(6), "the 6-byte inline preview, not the declared 108894");

        let log = std::fs::read_to_string(run_dir.join("runner-error.log")).unwrap();
        assert!(log.contains("deadbeef-not-on-disk"), "got: {log}");
    }

    /// A throwaway repo for the SMALL-4 attribution tests, on branch `main`.
    struct AttrRepo {
        dir: tempfile::TempDir,
    }

    impl AttrRepo {
        fn new() -> Self {
            let repo = AttrRepo { dir: tempfile::tempdir().unwrap() };
            repo.git(&["init", "--initial-branch=main"]);
            repo.git(&["config", "user.email", "test@example.com"]);
            repo.git(&["config", "user.name", "thoth test"]);
            repo
        }

        fn path(&self) -> &Path {
            self.dir.path()
        }

        /// Runs git, asserts it succeeded, returns its trimmed stdout.
        fn git(&self, args: &[&str]) -> String {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(self.path())
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }

        fn write(&self, rel: &str, content: &str) {
            let p = self.path().join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }

        /// Commits everything and returns the new HEAD.
        fn commit(&self, msg: &str) -> String {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-m", msg]);
            self.git(&["rev-parse", "HEAD"])
        }
    }

    /// Records the diff on a fresh state db and returns its detail.
    fn attributed_detail(
        repo: &AttrRepo,
        from: &str,
        to: &str,
        attribution: Option<&Attribution<'_>>,
    ) -> serde_json::Value {
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };
        let code = record_diff_attributed(&conn, &ctx, repo.path(), from, to, attribution).unwrap();
        let row = crate::db::state::get_evidence_by_code(&conn, "T1", &code)
            .unwrap()
            .unwrap();
        serde_json::from_str(&row.detail).unwrap()
    }

    /// SMALL-4 AC2 (u2).
    #[test]
    fn an_attributed_diff_does_not_count_a_test_file_the_task_added_in_an_earlier_attempt() {
        let repo = AttrRepo::new();
        repo.write("tests/old_test.rs", "#[test]\nfn old() {}\n");
        repo.write("src/lib.rs", "pub fn f() {}\n");
        let c1 = repo.commit("base");
        repo.write("tests/new_test.rs", "#[test]\nfn new() {}\n");
        let c2 = repo.commit("attempt 1");
        repo.write("tests/new_test.rs", "#[test]\nfn new() { assert!(true); }\n");
        repo.write("tests/old_test.rs", "#[test]\nfn old() { assert!(true); }\n");
        let c3 = repo.commit("attempt 2");

        let detail = attributed_detail(
            &repo,
            &c2,
            &c3,
            Some(&Attribution { task_base: &c1, integration_tip: None }),
        );
        assert_eq!(
            detail["protected_changes"],
            serde_json::json!([{"path": "tests/old_test.rs", "status": "modified", "kind": "test"}])
        );
        assert_eq!(
            detail["files_changed"],
            serde_json::json!(["tests/new_test.rs", "tests/old_test.rs"])
        );
        assert_eq!(detail["from_commit"], c2);
        assert_eq!(detail["protected_base"], c1);
        assert!(detail["integration_tip"].is_null());

        // without an attribution: today's behaviour
        let conn = db();
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let ctx = EvidenceContext {
            paths: &paths,
            repo_id: "repo01",
            task_id: "T1",
            run_id: "R1",
        };
        let code = record_diff(&conn, &ctx, repo.path(), &c2, &c3).unwrap();
        let row = crate::db::state::get_evidence_by_code(&conn, "T1", &code)
            .unwrap()
            .unwrap();
        let plain: serde_json::Value = serde_json::from_str(&row.detail).unwrap();
        let listed = plain["protected_changes"].as_array().unwrap();
        assert_eq!(listed.len(), 2, "got {listed:?}");
        assert!(listed.contains(
            &serde_json::json!({"path": "tests/new_test.rs", "status": "modified", "kind": "test"})
        ));
        assert_eq!(plain["protected_base"], c2);
    }

    /// SMALL-4 AC2 (u3).
    #[test]
    fn an_attributed_diff_does_not_count_what_a_fast_forward_merge_of_main_brought_in() {
        let repo = AttrRepo::new();
        repo.write("tests/pre_test.rs", "#[test]\nfn pre() {}\n");
        let c1 = repo.commit("base");
        repo.git(&["branch", "task", &c1]);
        repo.write("tests/pre_test.rs", "#[test]\nfn pre() { assert!(true); }\n");
        let c2 = repo.commit("main changes the test");
        repo.git(&["checkout", "task"]);
        repo.git(&["merge", "--no-edit", "main"]);
        assert_eq!(repo.git(&["rev-parse", "HEAD"]), c2, "a fast-forward");
        repo.write("src/own.rs", "pub fn own() {}\n");
        let c3 = repo.commit("own work");

        let detail = attributed_detail(
            &repo,
            &c1,
            &c3,
            Some(&Attribution { task_base: &c1, integration_tip: Some(&c2) }),
        );
        assert_eq!(detail["protected_changes"], serde_json::json!([]));
        assert_eq!(
            detail["files_changed"],
            serde_json::json!(["src/own.rs", "tests/pre_test.rs"])
        );
        assert_eq!(detail["touches_tests"], true);
        assert_eq!(detail["integration_tip"], c2);
    }

    /// SMALL-4 AC2 (u4) and (u7): the task's branch merges main (a merge
    /// commit) and then edits an existing test. Returns the repo, the base,
    /// main's tip and the candidate.
    fn merged_then_edited_repo() -> (AttrRepo, String, String, String) {
        let repo = AttrRepo::new();
        repo.write("tests/pre_test.rs", "#[test]\nfn pre() {}\n");
        repo.write("tests/other_test.rs", "#[test]\nfn other() {}\n");
        let c1 = repo.commit("base");
        repo.git(&["branch", "task", &c1]);
        repo.write("tests/other_test.rs", "#[test]\nfn other() { assert!(true); }\n");
        let c2 = repo.commit("main changes the other test");
        repo.git(&["checkout", "task"]);
        repo.write("src/own.rs", "pub fn own() {}\n");
        repo.commit("own work");
        repo.git(&["merge", "--no-edit", "main"]);
        repo.git(&["rev-parse", "HEAD^2"]);
        repo.write("tests/pre_test.rs", "#[test]\nfn pre() { assert_eq!(1, 1); }\n");
        let c4 = repo.commit("edit the pre-existing test");
        (repo, c1, c2, c4)
    }

    /// SMALL-4 AC2 (u4).
    #[test]
    fn a_test_the_task_edits_itself_after_merging_main_is_still_counted() {
        let (repo, c1, c2, c4) = merged_then_edited_repo();
        let detail = attributed_detail(
            &repo,
            &c1,
            &c4,
            Some(&Attribution { task_base: &c1, integration_tip: Some(&c2) }),
        );
        assert_eq!(
            detail["protected_changes"],
            serde_json::json!([{"path": "tests/pre_test.rs", "status": "modified", "kind": "test"}])
        );
    }

    /// SMALL-4 AC2 (u5).
    #[test]
    fn the_tip_filter_is_skipped_when_the_candidate_is_already_in_the_tip() {
        let repo = AttrRepo::new();
        repo.write("tests/pre_test.rs", "#[test]\nfn pre() {}\n");
        let c1 = repo.commit("base");
        repo.write("tests/pre_test.rs", "#[test]\nfn pre() { assert!(true); }\n");
        let c2 = repo.commit("main changes the test");
        let expected =
            serde_json::json!([{"path": "tests/pre_test.rs", "status": "modified", "kind": "test"}]);

        let detail = attributed_detail(
            &repo,
            &c1,
            &c2,
            Some(&Attribution { task_base: &c1, integration_tip: Some(&c2) }),
        );
        assert_eq!(detail["protected_changes"], expected);
        assert!(detail["integration_tip"].is_null());

        repo.git(&["commit", "--allow-empty", "-m", "empty"]);
        let c3 = repo.git(&["rev-parse", "HEAD"]);
        let detail = attributed_detail(
            &repo,
            &c1,
            &c2,
            Some(&Attribution { task_base: &c1, integration_tip: Some(&c3) }),
        );
        assert_eq!(detail["protected_changes"], expected);
        assert!(detail["integration_tip"].is_null());
    }

    /// SMALL-4 AC2 (u6).
    #[test]
    fn a_renamed_pre_existing_test_is_still_listed_as_deleted() {
        let repo = AttrRepo::new();
        repo.write("tests/old_test.rs", "#[test]\nfn old() {}\n// three lines\n");
        let c1 = repo.commit("base");
        repo.git(&["mv", "tests/old_test.rs", "tests/moved_test.rs"]);
        let c2 = repo.commit("move the test");
        let detail = attributed_detail(
            &repo,
            &c1,
            &c2,
            Some(&Attribution { task_base: &c1, integration_tip: Some(&c1) }),
        );
        assert_eq!(
            detail["protected_changes"],
            serde_json::json!([{"path": "tests/old_test.rs", "status": "deleted", "kind": "test"}])
        );
    }

    /// BASE-1 AC6: a task started on a long-lived branch, with the main
    /// worktree's HEAD still on main. Reverting the branch's test to main's
    /// version, and deleting a test only the branch has, both stay listed.
    #[test]
    fn an_attributed_diff_ignores_a_tip_that_does_not_contain_the_task_base() {
        let repo = AttrRepo::new();
        repo.write("tests/x_test.rs", "#[test]\nfn x() {}\n");
        let c1 = repo.commit("main");
        repo.git(&["checkout", "-b", "long", &c1]);
        repo.write("tests/x_test.rs", "#[test]\nfn x() { assert!(true); }\n");
        repo.write("tests/long_test.rs", "#[test]\nfn long() {}\n");
        let c2 = repo.commit("long-lived branch");
        repo.write("tests/x_test.rs", "#[test]\nfn x() {}\n");
        repo.git(&["rm", "-q", "tests/long_test.rs"]);
        let c3 = repo.commit("the task's work");

        let detail = attributed_detail(
            &repo,
            &c2,
            &c3,
            Some(&Attribution { task_base: &c2, integration_tip: Some(&c1) }),
        );
        assert_eq!(
            detail["protected_changes"],
            serde_json::json!([
                {"path": "tests/long_test.rs", "status": "deleted", "kind": "test"},
                {"path": "tests/x_test.rs", "status": "modified", "kind": "test"},
            ])
        );
        assert!(detail["integration_tip"].is_null());
    }

    /// SMALL-4 AC2 (u7).
    #[test]
    fn a_bad_tip_disables_the_filter_instead_of_losing_the_diff() {
        let (repo, c1, _c2, c4) = merged_then_edited_repo();
        let detail = attributed_detail(
            &repo,
            &c1,
            &c4,
            Some(&Attribution {
                task_base: &c1,
                integration_tip: Some("0000000000000000000000000000000000000000"),
            }),
        );
        assert!(detail["integration_tip"].is_null());
        assert_eq!(
            detail["protected_changes"],
            serde_json::json!([
                {"path": "tests/other_test.rs", "status": "modified", "kind": "test"},
                {"path": "tests/pre_test.rs", "status": "modified", "kind": "test"},
            ])
        );
    }
}
