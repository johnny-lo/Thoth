//! Content-addressed blob store: `~/.thoth/repos/<repo_id>/blobs/<sha256>`.
//!
//! brief §6. Two properties matter and both fall out of content addressing:
//! the same bytes are stored once however many runs produce them, and a blob
//! can be re-verified later without trusting the database (v6 §2.5 rule 4).
//! Phase 2 never deletes a blob.

use crate::config::Paths;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};

/// Streaming chunk size. A persisted tool output has no upper bound (Phase 0
/// saw 108894 bytes from one `seq`), so nothing here reads a whole file into
/// memory just to hash it.
// Retired in Task 2: `sha256_file` now has a production caller
// (`commands::hook::process_payload` via `put_file_into`), so this and every
// item below it up to `put_file_into` are reachable from `main` and no longer
// need a scaffolding marker. Test coverage unchanged: directly
// `put_file_streams_and_agrees_with_put_bytes`, indirectly (through
// `put_file_into`/`verify`) `put_bytes_is_content_addressed_and_dedupes`,
// `verify_accepts_the_stored_bytes_and_rejects_tampering_or_absence`, and
// `get_path_finds_a_stored_blob_and_misses_an_unknown_one`.
const CHUNK_BYTES: usize = 64 * 1024;

/// Streams `src` and returns `(sha256_hex, bytes)`.
// Retired in Task 2 together with `put_file_into` (see the module-level note
// above `CHUNK_BYTES`); read by `put_file_streams_and_agrees_with_put_bytes`.
pub fn sha256_file(src: &Path) -> Result<(String, u64)> {
    let mut f =
        std::fs::File::open(src).with_context(|| format!("opening {}", src.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK_BYTES];
    let mut total: u64 = 0;
    loop {
        let n = f
            .read(&mut buf)
            .with_context(|| format!("reading {}", src.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    let digest = hasher.finalize();
    Ok((digest.iter().map(|b| format!("{b:02x}")).collect(), total))
}

// Retired in Task 2 together with `put_file_into` (see the module-level note
// above `CHUNK_BYTES`): a private helper of `put_bytes_into`/`put_file_into`,
// exercised whenever a test stores content this store has not seen before -
// `put_bytes_is_content_addressed_and_dedupes`,
// `put_file_streams_and_agrees_with_put_bytes`,
// `verify_accepts_the_stored_bytes_and_rejects_tampering_or_absence`, and
// `get_path_finds_a_stored_blob_and_misses_an_unknown_one`.
fn temp_name(dir: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(".tmp-{}-{}", std::process::id(), stamp))
}

/// temp + rename, so a tailing reader never sees a half-written blob.
///
/// A destination that already exists is left exactly as it is: with content
/// addressing, "already there" means "same bytes", so there is nothing to
/// replace - and skipping the rename also avoids touching a file another process
/// may have open for reading.
///
/// `std::fs::rename` **replaces** an existing destination on Windows as well as
/// on Unix (it goes through `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`), so
/// a blob that appeared between the check above and the call is simply
/// overwritten with byte-identical content. No race handling is needed, and none
/// should be added: an earlier draft of this function assumed Windows `rename`
/// refuses an existing target, which is wrong.
// Retired in Task 2 together with `put_file_into` (see the module-level note
// above `CHUNK_BYTES`): a private helper of `put_bytes_into`/`put_file_into`,
// exercised whenever a test stores content this store has not seen before -
// `put_bytes_is_content_addressed_and_dedupes`,
// `put_file_streams_and_agrees_with_put_bytes`,
// `verify_accepts_the_stored_bytes_and_rejects_tampering_or_absence`, and
// `get_path_finds_a_stored_blob_and_misses_an_unknown_one`.
fn commit_temp(temp: &Path, dest: &Path) -> Result<()> {
    if dest.exists() {
        let _ = std::fs::remove_file(temp);
        return Ok(());
    }
    if let Err(e) = std::fs::rename(temp, dest) {
        let _ = std::fs::remove_file(temp);
        return Err(e).with_context(|| format!("renaming into {}", dest.display()));
    }
    Ok(())
}

/// Stores `bytes` in `dir`. Used with the run's inbox directory by
/// `thoth hook` (which must not know the repo layout) and with the repo's
/// blob directory by the runner.
///
/// Retired in Task 3: production caller is `put_bytes` (via
/// `evidence::record_command`); also read directly by
/// `put_bytes_is_content_addressed_and_dedupes` and by the evidence tests that
/// pre-seed an inbox blob.
pub fn put_bytes_into(dir: &Path, bytes: &[u8]) -> Result<(String, u64, PathBuf)> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let sha = crate::ids::sha256_hex(bytes);
    let dest = dir.join(&sha);
    if !dest.exists() {
        let temp = temp_name(dir);
        std::fs::write(&temp, bytes)
            .with_context(|| format!("writing {}", temp.display()))?;
        commit_temp(&temp, &dest)?;
    }
    Ok((sha, bytes.len() as u64, dest))
}

/// Copies `src` into `dir` under its own digest, streaming twice (once to hash,
/// once to copy) rather than buffering the file.
// Retired in Task 2: production caller is `commands::hook::process_payload`
// (see the module-level note above `CHUNK_BYTES`); read by
// `put_file_streams_and_agrees_with_put_bytes`.
pub fn put_file_into(dir: &Path, src: &Path) -> Result<(String, u64, PathBuf)> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let (sha, bytes) = sha256_file(src)?;
    let dest = dir.join(&sha);
    if !dest.exists() {
        let temp = temp_name(dir);
        std::fs::copy(src, &temp)
            .with_context(|| format!("copying {} to {}", src.display(), temp.display()))?;
        commit_temp(&temp, &dest)?;
    }
    Ok((sha, bytes, dest))
}

/// Retired in Task 3: production caller is `evidence::record_command`
/// (the inline-stdout and stderr blobs); also read by
/// `get_path_finds_a_stored_blob_and_misses_an_unknown_one`.
pub fn put_bytes(paths: &Paths, repo_id: &str, bytes: &[u8]) -> Result<(String, u64, PathBuf)> {
    put_bytes_into(&paths.blobs_dir(repo_id), bytes)
}

/// Retired in Task 3: production caller is `evidence::record_command`
/// (promoting a hook-persisted blob into the repo store); also read by
/// `get_path_finds_a_stored_blob_and_misses_an_unknown_one`.
pub fn put_file(paths: &Paths, repo_id: &str, src: &Path) -> Result<(String, u64, PathBuf)> {
    put_file_into(&paths.blobs_dir(repo_id), src)
}

// Scaffolding: read by `get_path_finds_a_stored_blob_and_misses_an_unknown_one`
// and by Task 3's evidence-recording test, which uses it to prove the stderr
// blob landed. Never retired: `evidence show` reads `evidence.blob_path`
// instead, so this never gains a production caller (see Global Constraints
// inventory).
#[allow(dead_code)]
pub fn get_path(paths: &Paths, repo_id: &str, sha256: &str) -> Option<PathBuf> {
    let p = paths.blobs_dir(repo_id).join(sha256);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// v6 §2.5 rule 4: a citation whose blob is missing, or whose bytes no longer
/// hash to the recorded digest, makes the whole result UNVERIFIED.
///
/// Deliberately returns `bool` rather than `Result`: an unreadable blob *is* a
/// failed verification, and ingest must not abort on it.
// Retired in Task 5: the first production caller is `ingest_with_evidence`'s
// rule 4 check, ahead of the `verify` command's own re-check in Task 9. Test
// coverage unchanged:
// `verify_accepts_the_stored_bytes_and_rejects_tampering_or_absence`,
// `get_path_finds_a_stored_blob_and_misses_an_unknown_one`, and Task 5's
// `rule4_a_blob_that_no_longer_matches_its_digest_is_unverified`.
pub fn verify(path: &Path, expected_sha256: &str) -> bool {
    match sha256_file(path) {
        Ok((actual, _)) => actual == expected_sha256,
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact string the fake harness persists, and the exact size Phase 0
    /// recorded for `seq 1 20000` (`persistedOutputSize: 108894`).
    fn seq_1_to_20000() -> String {
        (1..=20000).map(|n| format!("{n}\n")).collect()
    }

    #[test]
    fn put_bytes_is_content_addressed_and_dedupes() {
        let dir = tempfile::tempdir().unwrap();
        let (sha, bytes, path) = put_bytes_into(dir.path(), b"hello").unwrap();
        // the known sha256 vector Phase 1 already pins in ids::sha256_hex
        assert_eq!(
            sha,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(bytes, 5);
        assert_eq!(path, dir.path().join(&sha));
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");

        // storing the same bytes again is a no-op that reports the same place
        let before = std::fs::metadata(&path).unwrap().len();
        let (sha2, bytes2, path2) = put_bytes_into(dir.path(), b"hello").unwrap();
        assert_eq!((sha2, bytes2, path2), (sha, bytes, path.clone()));
        assert_eq!(std::fs::metadata(&path).unwrap().len(), before);

        // and no temp files are left behind
        let leftovers: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.starts_with(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "temp files left: {leftovers:?}");
    }

    #[test]
    fn put_file_streams_and_agrees_with_put_bytes() {
        let src_dir = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let body = seq_1_to_20000();
        assert_eq!(body.len(), 108_894, "the Phase 0 persistedOutputSize");
        let src = src_dir.path().join("tool-output.txt");
        std::fs::write(&src, &body).unwrap();

        let (sha_file, bytes_file, path) = put_file_into(store.path(), &src).unwrap();
        let (sha_mem, bytes_mem, _) = put_bytes_into(store.path(), body.as_bytes()).unwrap();
        assert_eq!(sha_file, sha_mem, "streaming and in-memory digests must agree");
        assert_eq!(bytes_file, 108_894);
        assert_eq!(bytes_mem, 108_894);
        assert_eq!(std::fs::read(&path).unwrap().len(), 108_894);

        let (sha_direct, bytes_direct) = sha256_file(&src).unwrap();
        assert_eq!(sha_direct, sha_file);
        assert_eq!(bytes_direct, 108_894);
    }

    #[test]
    fn verify_accepts_the_stored_bytes_and_rejects_tampering_or_absence() {
        let dir = tempfile::tempdir().unwrap();
        let (sha, _, path) = put_bytes_into(dir.path(), b"evidence bytes").unwrap();
        assert!(verify(&path, &sha));

        // v6 §2.5 rule 4: bytes that no longer hash to the recorded digest
        std::fs::write(&path, b"tampered bytes").unwrap();
        assert!(!verify(&path, &sha));

        // and a blob that is simply gone
        std::fs::remove_file(&path).unwrap();
        assert!(!verify(&path, &sha));
        assert!(!verify(&dir.path().join("never-existed"), &sha));
    }

    #[test]
    fn get_path_finds_a_stored_blob_and_misses_an_unknown_one() {
        let home = tempfile::tempdir().unwrap();
        let paths = crate::config::Paths::from_root(home.path().to_path_buf());
        let (sha, _, path) = put_bytes(&paths, "repo01", b"stored").unwrap();
        assert_eq!(path, paths.blobs_dir("repo01").join(&sha));
        assert_eq!(get_path(&paths, "repo01", &sha), Some(path));
        assert_eq!(get_path(&paths, "repo01", "deadbeef"), None);
        assert_eq!(get_path(&paths, "other-repo", &sha), None);

        let src = home.path().join("src.txt");
        std::fs::write(&src, b"from a file").unwrap();
        let (fsha, fbytes, fpath) = put_file(&paths, "repo01", &src).unwrap();
        assert_eq!(fbytes, 11);
        assert!(fpath.is_file());
        assert!(verify(&fpath, &fsha));
    }
}
