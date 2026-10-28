//! `thoth evidence list|show` (v6 §10.7).
//!
//! §2.3 "Evidence On Demand": a reviewer reads the index, not the logs, and
//! pulls the raw bytes only for the codes it actually needs. §10.3 makes this
//! the *only* way a worker or reviewer touches the database, which is also what
//! makes `evidence_opened` a complete record of what the reviewer looked at
//! (§2.3, and the honesty metric in §13).

use crate::config::Paths;
use crate::db::state::{self, NewEvent};
use crate::ids;
use crate::model::event_type;
use crate::output;
use crate::redact::{self, Counts, Masker};
use anyhow::{bail, Result};
use rusqlite::Connection;
use std::path::Path;

/// How many bytes of a blob `evidence show` prints by default: enough for a
/// test summary or a small diff, small enough that a reviewer opening the wrong
/// code has not burned its context. `--bytes N` raises it.
pub const DEFAULT_SHOW_BYTES: u64 = 4000;

/// The `exit_code` an index row shows, pulled back out of the stored detail so
/// there is one source of truth for it.
pub fn detail_exit_code(detail: &str) -> Option<i64> {
    serde_json::from_str::<serde_json::Value>(detail)
        .ok()
        .and_then(|d| d["exit_code"].as_i64())
}

/// The run on whose behalf this invocation is reading, if any.
///
/// The runner puts `THOTH_RUN_ID` in the worker's environment, and a
/// `claude` child inherits it, so a reviewer's `thoth evidence show` is
/// attributable without the reviewer having to know its own run id.
pub fn env_run_id() -> Option<String> {
    std::env::var("THOTH_RUN_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
}

/// Which task a bare code belongs to (Open question 14): `--task`, then the
/// environment's run, then a scan that must find exactly one match.
pub fn resolve_task_for_code(
    conn: &Connection,
    code: &str,
    task: Option<&str>,
) -> Result<String> {
    if let Some(needle) = task {
        return Ok(ids::resolve_prefix(&state::list_task_ids(conn)?, needle)?);
    }
    if let Some(run_id) = env_run_id() {
        if let Some(run) = state::get_run(conn, &run_id)? {
            return Ok(run.task_id);
        }
    }
    let hits = state::find_evidence_by_code(conn, code)?;
    match hits.len() {
        0 => bail!("no evidence with code {code} in this repo"),
        1 => Ok(hits[0].task_id.clone()),
        _ => {
            let tasks: Vec<String> = hits.iter().map(|e| e.task_id.clone()).collect();
            bail!(
                "{code} exists on more than one task ({}); pass --task <id>",
                tasks.join(", ")
            )
        }
    }
}

/// Reads at most `limit` bytes of a row's blob, and re-verifies the whole blob
/// while it is there.
///
/// Returns `(shown_text, total_bytes, verified)`. The verification is separate
/// from v6 §2.5 rule 4's check at ingest time and serves a different purpose: a
/// reviewer deserves to know, at the moment it reads, whether what it is
/// reading still hashes to what the control plane recorded.
pub(crate) fn read_blob(row: &state::Evidence, limit: u64) -> (String, u64, bool) {
    use std::io::Read;
    let Some(path) = row.blob_path.as_deref() else {
        return (String::new(), 0, false);
    };
    let p = Path::new(path);
    let verified = match row.sha256.as_deref() {
        Some(sha) => crate::blobs::verify(p, sha),
        None => false,
    };
    let total = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let Ok(f) = std::fs::File::open(p) else {
        return (String::new(), total, verified);
    };
    let mut buf: Vec<u8> = Vec::new();
    if f.take(limit).read_to_end(&mut buf).is_err() {
        return (String::new(), total, verified);
    }
    // Lossy: a blob is raw command output and need not be valid UTF-8.
    (String::from_utf8_lossy(&buf).into_owned(), total, verified)
}

/// `read_blob` with secrets masked (RD-1). `total` and `verified` describe
/// the stored blob; `masked_len` is the length of the whole masked text.
pub(crate) struct MaskedBlob {
    pub(crate) shown: String,
    pub(crate) total: u64,
    pub(crate) verified: bool,
    pub(crate) masked_len: u64,
    pub(crate) counts: Counts,
}

/// Masks the whole blob first and cuts it afterwards, so a cut never falls
/// inside a secret that would then go unrecognised. With nothing to mask,
/// `shown`, `total` and `verified` are exactly what `read_blob(row, limit)`
/// returns; otherwise `shown` is at most `limit` bytes of the masked text,
/// cut back to a character boundary.
pub(crate) fn read_blob_masked(row: &state::Evidence, limit: u64, masker: &Masker) -> MaskedBlob {
    let (full, total, verified) = read_blob(row, u64::MAX);
    let (masked, counts) = masker.mask_text(&full);
    if counts.is_empty() {
        let (shown, total, verified) = if limit >= total {
            (full, total, verified)
        } else {
            read_blob(row, limit)
        };
        return MaskedBlob { shown, total, verified, masked_len: total, counts };
    }
    let mut cut = usize::try_from(limit).unwrap_or(usize::MAX).min(masked.len());
    while !masked.is_char_boundary(cut) {
        cut -= 1;
    }
    MaskedBlob {
        masked_len: masked.len() as u64,
        shown: masked[..cut].to_string(),
        total,
        verified,
        counts,
    }
}

/// One `evidence list --json` row. TESTS-1 (AC2): `detail` (the stored
/// detail parsed the way `show` parses it, null when it is not JSON) and
/// `blob_path` are additive, so a test can read what it used to read from
/// the table without `show`'s `evidence_opened` event; the earlier keys keep
/// their values, `exit_code` included.
pub fn list_row(e: &state::Evidence) -> serde_json::Value {
    serde_json::json!({
        "code": e.code,
        "kind": e.kind,
        "collected_by": e.collected_by,
        "label": e.label,
        "exit_code": detail_exit_code(&e.detail),
        "bytes": e.bytes,
        "sha256": e.sha256,
        "run_id": e.run_id,
        "created_at": e.created_at,
        "detail": serde_json::from_str::<serde_json::Value>(&e.detail)
            .unwrap_or(serde_json::Value::Null),
        "blob_path": e.blob_path,
    })
}

pub fn list(paths: &Paths, cwd: &Path, needle: &str, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?;
    let rows = state::list_evidence_for_task(&conn, &task_id)?;

    let arr: Vec<serde_json::Value> = rows.iter().map(list_row).collect();
    let human = if rows.is_empty() {
        format!("no evidence recorded for {task_id}")
    } else {
        rows.iter()
            .map(|e| {
                format!(
                    "{:<6} {:<15} {:<14} {:>6}  {}",
                    e.code,
                    e.kind,
                    e.collected_by,
                    detail_exit_code(&e.detail)
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "-".to_string()),
                    e.label
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    output::emit(
        json,
        &serde_json::json!({ "task_id": task_id, "evidence": arr }),
        &human,
    );
    Ok(0)
}

pub fn show(
    paths: &Paths,
    cwd: &Path,
    code: &str,
    task: Option<&str>,
    bytes: Option<u64>,
    json: bool,
) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    // Codes are always `E` plus digits, so accepting `e001` costs nothing.
    let code = code.trim().to_uppercase();
    let task_id = resolve_task_for_code(&conn, &code, task)?;
    let row = state::get_evidence_by_code(&conn, &task_id, &code)?
        .ok_or_else(|| anyhow::anyhow!("task {task_id} has no evidence {code}"))?;

    // RD-1: content, label and detail leave masked; the stored row and blob
    // stay as they are, and sha256, bytes and blob_verified describe them.
    let masker = Masker::load(Some(&paths.home));
    let limit = bytes.unwrap_or(DEFAULT_SHOW_BYTES);
    let blob = read_blob_masked(&row, limit, &masker);
    let (body, total, verified) = (blob.shown, blob.total, blob.verified);
    // With something masked, "more to see" is measured on the masked text.
    let full_len = if blob.counts.is_empty() { total } else { blob.masked_len };
    let truncated = full_len > body.len() as u64;
    let mut counts = blob.counts;
    let (label, label_counts) = masker.mask_text(&row.label);
    counts.merge(&label_counts);
    let (detail, human_detail) = match serde_json::from_str::<serde_json::Value>(&row.detail) {
        Ok(mut detail) => {
            let detail_counts = masker.mask_value(&mut detail);
            counts.merge(&detail_counts);
            // Reprinting sorts the keys and drops the spacing, so the stored
            // text is shown whenever there was nothing to mask in it.
            let human = if detail_counts.is_empty() { row.detail.clone() } else { detail.to_string() };
            (detail, human)
        }
        Err(_) => {
            // Only the human output shows a detail that is not JSON, so only
            // it counts what was masked there.
            let (human, detail_counts) = masker.mask_text(&row.detail);
            if !json {
                counts.merge(&detail_counts);
            }
            (serde_json::Value::Null, human)
        }
    };

    // v6 §2.3/§10.3: every open is recorded. A run id that no longer exists is
    // dropped rather than written, so the event's `run_id` is always either null
    // or a real run - `review show` reads it back to cross-check citations.
    let opened_by =
        env_run_id().filter(|id| state::get_run(&conn, id).ok().flatten().is_some());
    state::append_event(
        &conn,
        &NewEvent {
            task_id: Some(task_id.clone()),
            run_id: opened_by.clone(),
            event_type: event_type::EVIDENCE_OPENED.to_string(),
            detail: serde_json::json!({
                "code": row.code,
                "kind": row.kind,
                "bytes_shown": body.len(),
                "bytes_total": total,
                "blob_verified": verified,
            }),
        },
    )?;
    // A failure to record is reported, not fatal: the content is already
    // masked, and refusing to show it would only hide the evidence.
    if let Err(e) = redact::record(
        &conn,
        &task_id,
        opened_by.as_deref(),
        "evidence_show",
        &counts,
        std::slice::from_ref(&row.code),
    ) {
        masked_eprintln!("warning: could not record secret_redacted: {e:#}");
    }

    let v = serde_json::json!({
        "task_id": task_id,
        "code": row.code,
        "kind": row.kind,
        "collected_by": row.collected_by,
        "label": label,
        "sha256": row.sha256,
        "bytes": row.bytes,
        "blob_path": row.blob_path,
        "blob_verified": verified,
        "run_id": row.run_id,
        "created_at": row.created_at,
        "detail": detail,
        "content": body,
        "content_truncated": truncated,
        "opened_by_run": opened_by,
    });
    let human = format!(
        "{}  {}  collected_by={}\n  label:  {}\n  sha256: {}  ({} bytes, verified={})\n  detail: {}\n--- content ---\n{}{}",
        row.code,
        row.kind,
        row.collected_by,
        label,
        row.sha256.clone().unwrap_or_else(|| "-".to_string()),
        row.bytes.map(|b| b.to_string()).unwrap_or_else(|| "-".to_string()),
        verified,
        human_detail,
        body,
        if truncated {
            format!(
                "\n--- {} more bytes; re-run with --bytes {} ---",
                full_len - body.len() as u64,
                full_len
            )
        } else {
            String::new()
        }
    );
    output::emit(json, &v, &human);
    Ok(0)
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
                label: "cargo test".into(),
                sha256: None,
                bytes: None,
                blob_path: None,
                detail: serde_json::json!({"exit_code": 0}).to_string(),
                created_at: "2026-09-16T00:00:00Z".into(),
            },
        )
        .unwrap();
    }

    fn insert_task(conn: &Connection, task_id: &str) {
        crate::db::state::insert_task(
            conn,
            &crate::db::state::Task {
                task_id: task_id.into(),
                title: "t".into(),
                spec: "{}".into(),
                risk: "normal".into(),
                status: "in_review".into(),
                base_commit: "abc".into(),
                created_at: "n".into(),
                updated_at: "n".into(),
            },
        )
        .unwrap();
    }

    /// TESTS-1 (AC2): a list row carries the parsed detail and the blob path
    /// next to the keys it always had; a detail that is not JSON is null, and
    /// `exit_code` is still read from the stored text.
    #[test]
    fn list_row_adds_detail_and_blob_path_without_touching_the_other_keys() {
        let mut row = crate::db::state::Evidence {
            evidence_id: "EV1".into(),
            code: "E001".into(),
            task_id: "T1".into(),
            run_id: Some("R1".into()),
            kind: "command".into(),
            collected_by: "control_plane".into(),
            label: "cargo test".into(),
            sha256: Some("abc".into()),
            bytes: Some(12),
            blob_path: Some("/blobs/abc".into()),
            detail: r#"{"exit_code":3,"cmd":"cargo test"}"#.into(),
            created_at: "2026-10-22T00:00:00Z".into(),
        };
        let v = list_row(&row);
        assert_eq!(
            v,
            serde_json::json!({
                "code": "E001", "kind": "command", "collected_by": "control_plane",
                "label": "cargo test", "exit_code": 3, "bytes": 12, "sha256": "abc",
                "run_id": "R1", "created_at": "2026-10-22T00:00:00Z",
                "detail": {"exit_code": 3, "cmd": "cargo test"},
                "blob_path": "/blobs/abc",
            })
        );
        row.detail = "not json".into();
        row.blob_path = None;
        let v = list_row(&row);
        assert_eq!(v["detail"], serde_json::Value::Null);
        assert_eq!(v["blob_path"], serde_json::Value::Null);
        assert_eq!(v["exit_code"], serde_json::Value::Null);
        assert_eq!(v["bytes"], 12);
    }

    #[test]
    fn detail_exit_code_reads_the_field_when_there_is_one() {
        assert_eq!(detail_exit_code(r#"{"exit_code":3}"#), Some(3));
        assert_eq!(detail_exit_code(r#"{"exit_code":null}"#), None);
        assert_eq!(detail_exit_code(r#"{"from_commit":"a"}"#), None);
        assert_eq!(detail_exit_code("not json"), None);
    }

    /// Open question 14, resolution order (a) and (c). Order (b) - the
    /// THOTH_RUN_ID path - is covered end to end by the integration test,
    /// because setting a process-wide env var inside a unit test would race
    /// every other test in the binary.
    #[test]
    fn resolve_task_for_code_uses_the_flag_then_falls_back_to_a_scan() {
        let conn = db();
        insert_task(&conn, "01TASKAAAAAAAAAAAAAAAAAAAA");
        insert_task(&conn, "01TASKBBBBBBBBBBBBBBBBBBBB");
        put(&conn, "01TASKAAAAAAAAAAAAAAAAAAAA", "E001");
        put(&conn, "01TASKAAAAAAAAAAAAAAAAAAAA", "E002");
        put(&conn, "01TASKBBBBBBBBBBBBBBBBBBBB", "E001");

        // (a) --task wins, and accepts a prefix
        assert_eq!(
            resolve_task_for_code(&conn, "E001", Some("01TASKB")).unwrap(),
            "01TASKBBBBBBBBBBBBBBBBBBBB"
        );
        // (c) a code on exactly one task needs no flag
        assert_eq!(
            resolve_task_for_code(&conn, "E002", None).unwrap(),
            "01TASKAAAAAAAAAAAAAAAAAAAA"
        );
        // (c) a code on two tasks is an error that names both
        let err = resolve_task_for_code(&conn, "E001", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("01TASKAAAAAAAAAAAAAAAAAAAA"), "got {err}");
        assert!(err.contains("01TASKBBBBBBBBBBBBBBBBBBBB"), "got {err}");
        assert!(err.contains("--task"), "got {err}");
        // and an unknown code says so
        let err = resolve_task_for_code(&conn, "E999", None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("E999"), "got {err}");
    }

    #[test]
    fn read_blob_truncates_verifies_and_tolerates_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let (sha, bytes, path) =
            crate::blobs::put_bytes_into(dir.path(), b"0123456789").unwrap();
        assert_eq!(bytes, 10);
        let mut row = crate::db::state::Evidence {
            evidence_id: "EV1".into(),
            code: "E001".into(),
            task_id: "T1".into(),
            run_id: None,
            kind: "command".into(),
            collected_by: "control_plane".into(),
            label: "x".into(),
            sha256: Some(sha.clone()),
            bytes: Some(10),
            blob_path: Some(path.display().to_string()),
            detail: "{}".into(),
            created_at: "n".into(),
        };

        let (body, total, verified) = read_blob(&row, 4);
        assert_eq!(body, "0123");
        assert_eq!(total, 10);
        assert!(verified);

        let (body, total, verified) = read_blob(&row, 100);
        assert_eq!(body, "0123456789");
        assert_eq!(total, 10);
        assert!(verified);

        // tampered bytes are still shown, but flagged
        std::fs::write(&path, b"xxxxxxxxxx").unwrap();
        let (body, _, verified) = read_blob(&row, 100);
        assert_eq!(body, "xxxxxxxxxx");
        assert!(!verified, "a reviewer must be told the digest no longer matches");

        // a row with no blob at all
        row.blob_path = None;
        assert_eq!(read_blob(&row, 100), (String::new(), 0, false));
    }

    fn blob_row(dir: &Path, bytes: &[u8]) -> crate::db::state::Evidence {
        let (sha, n, path) = crate::blobs::put_bytes_into(dir, bytes).unwrap();
        crate::db::state::Evidence {
            evidence_id: "EV1".into(),
            code: "E001".into(),
            task_id: "T1".into(),
            run_id: None,
            kind: "command".into(),
            collected_by: "control_plane".into(),
            label: "x".into(),
            sha256: Some(sha),
            bytes: Some(n as i64),
            blob_path: Some(path.display().to_string()),
            detail: "{}".into(),
            created_at: "n".into(),
        }
    }

    /// RD-1 (AC2): the blob is masked whole and cut afterwards; with nothing
    /// to mask the result is `read_blob`'s, field for field.
    #[test]
    fn read_blob_masked_masks_before_cutting_and_matches_read_blob_when_clean() {
        let dir = tempfile::tempdir().unwrap();
        let masker = Masker::from_pairs(&[]);
        let payload = "dummyDUMMY0123456789dummyDUMMY012345";
        let token = format!("{}{payload}", ["gh", "p_"].concat());
        let x20 = "x".repeat(20);
        let row = blob_row(dir.path(), format!("{x20} {token}\n").as_bytes());

        let cut = read_blob_masked(&row, 26, &masker);
        assert_eq!(cut.shown, format!("{x20} [reda"));
        assert_eq!(cut.shown.len(), 26);
        for i in 0..=payload.len() - 6 {
            assert!(!cut.shown.contains(&payload[i..i + 6]), "payload piece at {i}");
        }
        assert_eq!(cut.total, row.bytes.unwrap() as u64);
        assert!(cut.verified);
        let whole = read_blob_masked(&row, 1_000_000, &masker);
        assert_eq!(whole.shown, format!("{x20} [redacted:github-token]\n"));
        assert_eq!(whole.masked_len, whole.shown.len() as u64);
        assert_eq!(whole.counts.to_json(), serde_json::json!({"github-token": 1}));

        let ascii = blob_row(dir.path(), b"plain ascii output\n");
        let multibyte = blob_row(dir.path(), "ab\u{4e2d}\u{6587}cd".as_bytes());
        let invalid = blob_row(dir.path(), b"bad \xff\xfe bytes");
        let mut no_blob = ascii.clone();
        no_blob.blob_path = None;
        let mut missing = ascii.clone();
        missing.blob_path = Some(dir.path().join("gone").display().to_string());
        for (name, row, limit) in [
            ("ascii", &ascii, 5),
            ("ascii whole", &ascii, 100),
            ("utf-8 cut", &multibyte, 3),
            ("invalid utf-8", &invalid, 100),
            ("no blob", &no_blob, 100),
            ("missing file", &missing, 100),
        ] {
            let m = read_blob_masked(row, limit, &masker);
            assert_eq!((m.shown, m.total, m.verified), read_blob(row, limit), "{name}");
            assert!(m.counts.is_empty(), "{name}");
            assert_eq!(m.masked_len, m.total, "{name}");
        }
    }
}
