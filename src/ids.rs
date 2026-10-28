use anyhow::Result;
use sha2::{Digest, Sha256};
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

pub fn new_task_id() -> String {
    ulid::Ulid::generate().to_string()
}

pub fn new_run_id() -> String {
    ulid::Ulid::generate().to_string()
}

pub fn new_claim_id() -> String {
    ulid::Ulid::generate().to_string()
}

// Retired in Task 3: production (and first) caller is `evidence::insert_row`.
pub fn new_evidence_id() -> String {
    ulid::Ulid::generate().to_string()
}

// Retired in Task 7: production caller is `commands::review::ingest_review`,
// which mints the `reviews.review_id` it writes.
pub fn new_review_id() -> String {
    ulid::Ulid::generate().to_string()
}

/// Pre-generated `--session-id` for the harness so resume has a handle.
pub fn new_session_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// `sha256(canonical repo root)[..12]`. The canonical form uses forward
/// slashes, and on Windows it is also lowercased, because Windows paths are
/// case-insensitive: `C:\r` and `c:\r` must map to one repo id. Linux paths
/// are case-sensitive - `/r/A` and `/r/a` are two different repositories - so
/// there the case is kept (Linux support,
/// docs/post-phase2/linux-support-scope-2026-09-26.md §3.3; Windows ids are
/// unchanged). The `\\?\` stripping lives in `config::canonical_display` so
/// `profile add` normalises identically.
pub fn repo_id_for(root: &Path) -> Result<String> {
    let s = crate::config::canonical_display(root)?;
    let s = s.replace('\\', "/").trim_end_matches('/').to_string();
    #[cfg(windows)]
    let s = s.to_lowercase();
    Ok(sha256_hex(s.as_bytes())[..12].to_string())
}

pub fn now_rfc3339() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum IdResolveError {
    #[error("id prefix must be at least 6 characters, got {0}")]
    TooShort(usize),
    #[error("no id matches '{0}'")]
    NotFound(String),
    #[error("ambiguous id prefix, matches: {}", .0.join(", "))]
    Ambiguous(Vec<String>),
}

/// Accept a full ULID or a unique prefix of at least 6 characters.
/// ULIDs are uppercase Crockford base32, so the needle is uppercased first.
pub fn resolve_prefix(candidates: &[String], needle: &str) -> Result<String, IdResolveError> {
    if candidates.iter().any(|c| c == needle) {
        return Ok(needle.to_string());
    }
    if needle.len() < 6 {
        return Err(IdResolveError::TooShort(needle.len()));
    }
    let up = needle.to_uppercase();
    let hits: Vec<String> = candidates
        .iter()
        .filter(|c| c.starts_with(&up))
        .cloned()
        .collect();
    match hits.len() {
        0 => Err(IdResolveError::NotFound(needle.to_string())),
        1 => Ok(hits[0].clone()),
        _ => Err(IdResolveError::Ambiguous(hits)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_ids_are_26_char_ulids_and_unique() {
        let a = new_task_id();
        let b = new_task_id();
        assert_eq!(a.len(), 26, "got {a}");
        assert_ne!(a, b);
        assert!(a.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()));
    }

    #[test]
    fn session_uuids_look_like_uuid_v4() {
        let u = new_session_uuid();
        assert_eq!(u.len(), 36, "got {u}");
        assert_eq!(u.chars().filter(|c| *c == '-').count(), 4);
    }

    #[test]
    fn sha256_hex_matches_the_known_vector() {
        assert_eq!(
            sha256_hex(b"hello"),
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn repo_id_is_12_hex_chars_and_case_insensitive() {
        let dir = tempfile::tempdir().unwrap();
        let a = repo_id_for(dir.path()).unwrap();
        assert_eq!(a.len(), 12);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        let again = repo_id_for(dir.path()).unwrap();
        assert_eq!(a, again, "repo_id must be stable for the same path");
    }

    /// Linux paths are case-sensitive, so two repositories whose roots differ
    /// only in case must not share a state directory.
    #[test]
    #[cfg(not(windows))]
    fn repo_id_keeps_case_where_paths_are_case_sensitive() {
        let dir = tempfile::tempdir().unwrap();
        let upper = dir.path().join("Repo");
        let lower = dir.path().join("repo");
        std::fs::create_dir(&upper).unwrap();
        std::fs::create_dir(&lower).unwrap();
        assert_ne!(repo_id_for(&upper).unwrap(), repo_id_for(&lower).unwrap());
    }

    #[test]
    fn now_rfc3339_ends_in_z() {
        let t = now_rfc3339();
        assert!(t.ends_with('Z'), "got {t}");
        assert!(t.contains('T'));
    }

    #[test]
    fn prefix_resolution_covers_exact_unique_short_missing_and_ambiguous() {
        let ids = vec![
            "01AAAAAAAAAAAAAAAAAAAAAAAA".to_string(),
            "01AAAAAABBBBBBBBBBBBBBBBBB".to_string(),
            "01ZZZZZZZZZZZZZZZZZZZZZZZZ".to_string(),
        ];
        assert_eq!(resolve_prefix(&ids, &ids[0]).unwrap(), ids[0]);
        assert_eq!(resolve_prefix(&ids, "01ZZZZ").unwrap(), ids[2]);
        assert_eq!(resolve_prefix(&ids, "01zzzz").unwrap(), ids[2]);
        assert!(matches!(resolve_prefix(&ids, "01Z"), Err(IdResolveError::TooShort(3))));
        assert!(matches!(resolve_prefix(&ids, "01QQQQQQ"), Err(IdResolveError::NotFound(_))));
        match resolve_prefix(&ids, "01AAAAAA") {
            Err(IdResolveError::Ambiguous(hits)) => assert_eq!(hits.len(), 2),
            other => panic!("expected Ambiguous, got {other:?}"),
        }
    }
}
