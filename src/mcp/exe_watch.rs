//! Notices when the executable `thoth mcp` was started from is replaced on
//! disk, so the server can hand its calls to the new one (src/mcp/forward.rs).
//!
//! Rebuilds often keep the version string, and on Windows a new file under the
//! old name can inherit the old creation time, so neither tells. The length
//! and modification time are cheap to read on every call; only when they move
//! are the bytes compared, against the file this process still holds open.
//! A `Stamp` does the same against the file that was at the path when a
//! hand-over was tried, so a failed one is tried again only for new bytes.
//! A file that cannot be stamped is not tried at all.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The file at the watched path at one moment.
pub struct Stamp {
    len: u64,
    modified: SystemTime,
    hash: String,
}

pub struct ExeWatch {
    path: PathBuf,
    /// The started executable. Opened with std's default sharing, which on
    /// Windows includes FILE_SHARE_DELETE, so it never blocks a rename; it
    /// keeps the old bytes readable after the path gets a new file.
    file: File,
    len: u64,
    modified: SystemTime,
    replaced: bool,
}

impl ExeWatch {
    /// `None` when any step fails; such a server never warns.
    pub fn start(path: PathBuf) -> Option<Self> {
        let file = File::open(&path).ok()?;
        let meta = std::fs::metadata(&path).ok()?;
        let modified = meta.modified().ok()?;
        Some(Self { path, file, len: meta.len(), modified, replaced: false })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether the path now holds other bytes than the started executable.
    /// Once true, stays true. Nothing at the path (an update between renaming
    /// the old file and placing the new one) and any read failure are false.
    pub fn replaced(&mut self) -> bool {
        if self.replaced {
            return true;
        }
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        if meta.len() == self.len && modified == self.modified {
            return false;
        }
        let Ok(now) = std::fs::read(&self.path) else {
            return false;
        };
        let mut started = Vec::new();
        if self.file.seek(SeekFrom::Start(0)).is_err() || self.file.read_to_end(&mut started).is_err() {
            return false;
        }
        if crate::ids::sha256_hex(&now) == crate::ids::sha256_hex(&started) {
            self.len = meta.len();
            self.modified = modified;
            return false;
        }
        self.replaced = true;
        true
    }

    /// The file now at the path; `None` when nothing is there or it cannot
    /// be read.
    pub fn stamp(&self) -> Option<Stamp> {
        let meta = std::fs::metadata(&self.path).ok()?;
        let modified = meta.modified().ok()?;
        let bytes = std::fs::read(&self.path).ok()?;
        Some(Stamp { len: meta.len(), modified, hash: crate::ids::sha256_hex(&bytes) })
    }

    /// Whether the path now holds other bytes than when `stamp` was taken.
    /// The bytes are read only when the length or modification time moved;
    /// the same bytes under a new time update the stamp. Nothing at the path
    /// and any read failure are false.
    pub fn changed_since(&self, stamp: &mut Stamp) -> bool {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return false;
        };
        let Ok(modified) = meta.modified() else {
            return false;
        };
        if meta.len() == stamp.len && modified == stamp.modified {
            return false;
        }
        let Ok(now) = std::fs::read(&self.path) else {
            return false;
        };
        if crate::ids::sha256_hex(&now) == stamp.hash {
            stamp.len = meta.len();
            stamp.modified = modified;
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    /// Writes `bytes` to `path` and gives it a modification time `step`
    /// seconds after a fixed start, so each change is visible to the watch
    /// whatever the file system's time resolution is.
    fn put(path: &std::path::Path, bytes: &[u8], step: u64) {
        std::fs::write(path, bytes).unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + step * 60))
            .unwrap();
    }

    #[test]
    fn exe_watch_flags_new_bytes_only() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("thoth-copy.bin");
        let original = b"original bytes".to_vec();
        put(&exe, &original, 0);
        let mut w = ExeWatch::start(exe.clone()).expect("an existing file is watched");
        assert!(!w.replaced(), "untouched");

        let moved = d.path().join("thoth-copy.old");
        std::fs::rename(&exe, &moved).unwrap();
        assert!(!w.replaced(), "renamed away, nothing at the path yet");

        put(&exe, &original, 1);
        assert!(!w.replaced(), "the same bytes put back");
        assert!(!w.replaced(), "and asked again");

        let other = b"ORIGINAL BYTES".to_vec();
        assert_eq!(other.len(), original.len());
        put(&exe, &other, 2);
        assert!(w.replaced(), "same length, new bytes");

        put(&exe, &original, 3);
        assert!(w.replaced(), "once replaced, always replaced");

        assert!(ExeWatch::start(d.path().join("missing.bin")).is_none());
    }

    #[test]
    fn exe_watch_stamp_changes_for_new_bytes_only() {
        let d = tempfile::tempdir().unwrap();
        let exe = d.path().join("thoth-copy.bin");
        put(&exe, b"first bytes", 0);
        let w = ExeWatch::start(exe.clone()).unwrap();
        put(&exe, b"second bytes", 1);
        let mut s = w.stamp().expect("a readable file is stamped");
        assert!(!w.changed_since(&mut s), "untouched");

        std::fs::rename(&exe, d.path().join("thoth-copy.old")).unwrap();
        assert!(w.stamp().is_none());
        assert!(!w.changed_since(&mut s), "nothing at the path");

        put(&exe, b"second bytes", 2);
        assert!(!w.changed_since(&mut s), "the same bytes, a new time");

        // Unchanged only if the stamp took the new time: otherwise the time
        // moved, the bytes would be read and found new.
        put(&exe, b"SECOND BYTES", 2);
        assert!(!w.changed_since(&mut s), "same length and time: the bytes are not read");

        put(&exe, b"SECOND BYTES", 3);
        assert!(w.changed_since(&mut s), "new bytes under a new time");
        assert!(w.changed_since(&mut s), "and still, until a new stamp");
    }
}
