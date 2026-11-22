//! SMALL-11 (5): a `thoth verify` holds its run's worktree while it runs the
//! checks, so a sweep that finishes the task meanwhile (a PASS, `task close`,
//! `worktrees prune` ...) does not remove the directory under the checks.
//!
//! The hold is a small file under the repository's state directory,
//! `verify-holds/RUN.NONCE.hold`, with an exclusive operating-system file lock
//! on it for as long as the verify lives. A sweep asks the lock, never a pid:
//! the lock is released by the system when the process ends however it ends, so
//! a verify that was killed holds nothing, and a reused pid can keep nothing
//! held. The file's name carries the run id; its content is empty (a lock may
//! stop other handles from reading a file on Windows).
//!
//! SMALL-12: the file is created as `RUN.NONCE.pending`, locked, and only then
//! renamed to `.hold`, so a sweep never sees a `.hold` that is not locked. A
//! sweep ignores `.pending` files (a verify killed between create and rename
//! leaves an empty one; it never counts as a hold).

use crate::config::Paths;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;

fn holds_dir(paths: &Paths, repo_id: &str) -> PathBuf {
    paths.repo_dir(repo_id).join("verify-holds")
}

/// A verify's hold on its run's worktree; released (the lock dropped and the
/// file removed) when this value is dropped.
pub(crate) struct Hold {
    file: Option<File>,
    path: PathBuf,
}

impl Drop for Hold {
    fn drop(&mut self) {
        // The handle first: Windows cannot delete a file that is open.
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Takes the hold for `run_id`. Best effort: `None` when the file cannot be
/// made, locked or renamed, in which case the verify just runs unprotected, as
/// it did.
///
/// SMALL-12: the file is made under a temporary name (`RUN.NONCE.pending`),
/// locked, and only then renamed to `RUN.NONCE.hold`, keeping the handle and
/// its lock; so no file named `.hold` is ever visible without its lock held.
/// A sweep never looks at a `.pending` file.
pub(crate) fn acquire(paths: &Paths, repo_id: &str, run_id: &str) -> Option<Hold> {
    let dir = holds_dir(paths, repo_id);
    std::fs::create_dir_all(&dir).ok()?;
    let stem = format!("{run_id}.{}", crate::ids::new_run_id());
    let pending = dir.join(format!("{stem}.pending"));
    let path = dir.join(format!("{stem}.hold"));
    let file = OpenOptions::new().write(true).create_new(true).open(&pending).ok()?;
    #[cfg(debug_assertions)]
    crate::commands::spec_review::test_pause("THOTH_TEST_VERIFY_HOLD_PAUSE_FILE");
    if file.try_lock().is_err() {
        drop(file);
        let _ = std::fs::remove_file(&pending);
        return None;
    }
    if std::fs::rename(&pending, &path).is_err() {
        drop(file);
        let _ = std::fs::remove_file(&pending);
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some(Hold { file: Some(file), path })
}

/// The run ids of the verifies running now. A hold file nobody holds the lock
/// of belongs to a verify that ended without removing it (it was killed); it
/// is removed and not listed.
pub(crate) fn held_runs(paths: &Paths, repo_id: &str) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(holds_dir(paths, repo_id)) else {
        return Vec::new();
    };
    let mut held = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.ends_with(".hold") {
            continue;
        }
        let Some(run_id) = name.split('.').next().filter(|r| !r.is_empty()) else {
            continue;
        };
        let Ok(file) = File::open(&path) else {
            continue;
        };
        match file.try_lock_shared() {
            Ok(()) => {
                drop(file);
                let _ = std::fs::remove_file(&path);
            }
            // Anything but "nobody holds it" keeps the worktree.
            Err(TryLockError::WouldBlock) | Err(TryLockError::Error(_)) => held.push(run_id.to_string()),
        }
    }
    held
}
