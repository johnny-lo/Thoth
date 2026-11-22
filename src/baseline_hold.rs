//! SWEEP-1: the temporary checkouts of `thoth verify RUN --at-base` and
//! `thoth suite run` (`THOTH_HOME/repos/REPO/baseline-worktrees/NAME`) carry
//! an owner record, and a sweep removes the ones whose command is gone.
//!
//! The owner record is the file `baseline-holds/NAME.hold` beside that folder,
//! with an exclusive operating-system file lock on it for as long as the
//! command lives. It is made the way the verify hold is (SMALL-12): created as
//! `NAME.pending`, locked, then renamed to `NAME.hold`, so no `.hold` is ever
//! visible unlocked; the checkout is created only after that. The lock is the
//! only signal: the system releases it when the process ends however it ends
//! (a guard skipped by SIGKILL, Ctrl-C or a reboot included), so no pid,
//! heartbeat or age is read. The record lives outside the checkout so it never
//! shows up as an untracked file there.
//!
//! The sweep (`sweep`) runs at the start of `verify --at-base`, at the start
//! of `suite run` and in `worktrees prune`. Per entry of `baseline-worktrees`:
//! a record whose lock someone else holds (or that cannot be opened or asked)
//! keeps the checkout as `in_use`; a record whose lock the sweep can take
//! marks a leftover, removed with its git registration while the sweep holds
//! that lock (`remove_failed` when it stays), unless the checkout is already
//! gone once the lock is taken (its command removed it after the folder was
//! read), which is reported nowhere; no record keeps it untouched, as
//! `no_owner_record` when its name has Thoth's shape and git lists it, and as
//! `not_thoth_checkout` otherwise. Hold files with no checkout are deleted
//! when nobody holds their lock.

use crate::config::Paths;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};

const HOLD_EXT: &str = "hold";
const PENDING_EXT: &str = "pending";

/// Why the sweep kept an entry (`baseline_sweep.kept[].reason`).
pub(crate) const IN_USE: &str = "in_use";
pub(crate) const REMOVE_FAILED: &str = "remove_failed";
pub(crate) const NO_OWNER_RECORD: &str = "no_owner_record";
pub(crate) const NOT_THOTH_CHECKOUT: &str = "not_thoth_checkout";

/// Which command made a checkout (`baseline_sweep.removed[].made_by`).
const MADE_BY_SUITE: &str = "suite_run";
const MADE_BY_AT_BASE: &str = "verify_at_base";

pub(crate) fn checkouts_dir(paths: &Paths, repo_id: &str) -> PathBuf {
    paths.repo_dir(repo_id).join("baseline-worktrees")
}

fn holds_dir(paths: &Paths, repo_id: &str) -> PathBuf {
    paths.repo_dir(repo_id).join("baseline-holds")
}

/// A command's owner record on its checkout `NAME`; see `release`.
pub(crate) struct Hold {
    file: Option<File>,
    path: PathBuf,
}

impl Hold {
    /// Lets go of the lock, and deletes the record only when `checkout` is
    /// gone: a checkout the command could not remove keeps its (now
    /// unlocked) record, so the next sweep sees a leftover and tries again.
    pub(crate) fn release(mut self, checkout: &Path) {
        // The handle first: Windows cannot delete a file that is open.
        self.file.take();
        if std::fs::symlink_metadata(checkout).is_err() {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// Lets go of the lock and deletes the record, whatever is at the
    /// checkout's path: for a command that found the path taken and will not
    /// touch it, so its unlocked record must not make that a leftover.
    pub(crate) fn discard(mut self) {
        self.file.take();
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Makes the owner record for the checkout `name`. Best effort: `None` when
/// the file cannot be created, locked or renamed, and the command then makes
/// its checkout without one (every sweep keeps such a checkout as
/// `no_owner_record`; the command's own guard still removes it).
pub(crate) fn acquire(paths: &Paths, repo_id: &str, name: &str) -> Option<Hold> {
    let dir = holds_dir(paths, repo_id);
    std::fs::create_dir_all(&dir).ok()?;
    let pending = dir.join(format!("{name}.{PENDING_EXT}"));
    let path = dir.join(format!("{name}.{HOLD_EXT}"));
    let file = OpenOptions::new().read(true).write(true).create_new(true).open(&pending).ok()?;
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

/// Whether `s` is a ULID as Thoth writes one: 26 characters of uppercase
/// Crockford base32 that parse.
fn is_ulid(s: &str) -> bool {
    s.len() == 26
        && s.bytes().all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
        && ulid::Ulid::from_string(s).is_ok()
}

/// `made_by` and `run_id` for a name of Thoth's shape (`suite-ULID` or
/// `RUN-ULID`); `None` for any other name.
fn made_by(name: &str) -> Option<(&'static str, Option<String>)> {
    if let Some(rest) = name.strip_prefix("suite-") {
        return is_ulid(rest).then_some((MADE_BY_SUITE, None));
    }
    let (run, nonce) = name.split_once('-')?;
    (is_ulid(run) && is_ulid(nonce)).then(|| (MADE_BY_AT_BASE, Some(run.to_string())))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Removed {
    pub path: String,
    pub made_by: &'static str,
    pub run_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Kept {
    pub path: String,
    pub reason: &'static str,
    /// Only for `remove_failed`: the error.
    pub detail: Option<String>,
}

/// What one sweep did, in path order.
#[derive(Debug, Default)]
pub(crate) struct Sweep {
    pub removed: Vec<Removed>,
    pub kept: Vec<Kept>,
    /// The folder could not be read (then both lists are empty).
    pub error: Option<String>,
}

impl Sweep {
    /// The `baseline_sweep` key of `worktrees prune`, `suite run` and
    /// `verify --at-base` (`--json`).
    pub(crate) fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            "removed": self.removed.iter().map(|r| serde_json::json!({
                "path": r.path, "made_by": r.made_by, "run_id": r.run_id,
            })).collect::<Vec<_>>(),
            "kept": self.kept.iter().map(|k| serde_json::json!({
                "path": k.path, "reason": k.reason, "detail": k.detail,
            })).collect::<Vec<_>>(),
            "error": self.error,
        })
    }

    /// The stderr lines of `suite run` and `verify --at-base`: one per
    /// removed leftover and one per failure; nothing for the other reasons.
    pub(crate) fn stderr_lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .removed
            .iter()
            .map(|r| format!("thoth: removed the leftover checkout {} (its command is no longer running)", r.path))
            .collect();
        for k in self.kept.iter().filter(|k| k.reason == REMOVE_FAILED) {
            lines.push(format!(
                "thoth: warning: could not remove the leftover checkout {}: {}",
                k.path,
                k.detail.as_deref().unwrap_or("")
            ));
        }
        if let Some(e) = &self.error {
            lines.push(format!("thoth: warning: could not look for leftover checkouts: {e}"));
        }
        lines
    }

    /// The lines `worktrees prune` adds after its own.
    pub(crate) fn human_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for r in &self.removed {
            let by = match &r.run_id {
                Some(run) => format!("verify --at-base {run}"),
                None => "suite run".to_string(),
            };
            lines.push(format!("  removed  baseline  {}  (left by {by})", r.path));
        }
        for k in &self.kept {
            let tail = match k.reason {
                REMOVE_FAILED => format!(" - {}", one_line(k.detail.as_deref().unwrap_or(""))),
                NO_OWNER_RECORD => format!(
                    " - remove it with git worktree remove --force {} once no thoth older than this version is running",
                    k.path
                ),
                _ => String::new(),
            };
            lines.push(format!("  kept     baseline  {}: {}{tail}", k.path, k.reason));
        }
        if let Some(e) = &self.error {
            lines.push(format!("  warning  baseline  could not look for leftover checkouts: {e}"));
        }
        lines
    }
}

/// `detail` on one line: each line break, with the white space around it,
/// becomes `; ` (blank lines add nothing), so no part of it is lost.
fn one_line(detail: &str) -> String {
    detail.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ")
}

/// Prints `stderr_lines`.
pub(crate) fn warn(s: &Sweep) {
    for line in s.stderr_lines() {
        masked_eprintln!("{line}");
    }
}

/// The hold file's lock, as the sweep found it.
enum Lock {
    /// No `NAME.hold`.
    Missing,
    /// Someone else holds it, or it could not be opened or asked.
    Held,
    /// Nobody held it; the sweep holds it now through this handle.
    Taken(File),
}

fn take_lock(path: &Path) -> Lock {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Lock::Missing,
        Err(_) => return Lock::Held,
        Ok(_) => {}
    }
    let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
        return Lock::Held;
    };
    match file.try_lock() {
        Ok(()) => Lock::Taken(file),
        Err(TryLockError::WouldBlock) | Err(TryLockError::Error(_)) => Lock::Held,
    }
}

/// Whether `path` is one of the worktrees `listed` (from `git worktree list`).
fn is_listed(path: &Path, listed: &[PathBuf]) -> bool {
    let canon = std::fs::canonicalize(path).ok();
    listed.iter().any(|l| {
        l == path || (canon.is_some() && std::fs::canonicalize(l).ok() == canon)
    })
}

/// Removes a leftover checkout with its git registration, the way the
/// command's own guard does: directory links inside first (never followed),
/// then `git worktree remove --force`, falling back to deleting the directory
/// and `git worktree prune`. `Err` with the reason when it is still there.
fn remove_checkout(repo_root: &Path, path: &Path) -> Result<(), String> {
    crate::worktree::remove_dir_links(path).map_err(|e| e.to_string())?;
    let git = crate::worktree::remove_worktree_force(repo_root, path).err();
    let mut errors: Vec<String> = Vec::new();
    if std::fs::symlink_metadata(path).is_ok() {
        if let Some(e) = &git {
            errors.push(format!("{e:#}"));
        }
        if let Err(e) = std::fs::remove_dir_all(path) {
            errors.push(format!("deleting the directory failed: {e}"));
        }
    }
    crate::worktree::prune_worktrees(repo_root);
    if std::fs::symlink_metadata(path).is_ok() {
        if errors.is_empty() {
            errors.push("the directory is still there after removing it".to_string());
        }
        return Err(errors.join("; "));
    }
    Ok(())
}

/// The sweep of SWEEP-1 over this repository's `baseline-worktrees` folder
/// and the hold files beside it. A missing folder is an empty result. It
/// deletes only leftovers (their record's lock was free) and stale hold files.
pub(crate) fn sweep(paths: &Paths, repo_id: &str, repo_root: &Path) -> Sweep {
    let mut out = Sweep::default();
    let dir = checkouts_dir(paths, repo_id);
    let holds = holds_dir(paths, repo_id);
    let mut names: Vec<String> = match std::fs::read_dir(&dir) {
        Ok(rd) => {
            let mut v = Vec::new();
            for entry in rd {
                match entry {
                    Ok(e) => v.push(e.file_name().to_string_lossy().into_owned()),
                    Err(e) => {
                        out.error = Some(format!("reading {} failed: {e}", dir.display()));
                        return out;
                    }
                }
            }
            v
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            out.error = Some(format!("reading {} failed: {e}", dir.display()));
            return out;
        }
    };
    names.sort();
    // Asked once, only when an entry without a record needs it.
    let mut listed: Option<Option<Vec<PathBuf>>> = None;
    for name in &names {
        let path = dir.join(name);
        let shown = path.display().to_string();
        let Some((by, run_id)) = made_by(name) else {
            out.kept.push(Kept { path: shown, reason: NOT_THOTH_CHECKOUT, detail: None });
            continue;
        };
        match take_lock(&holds.join(format!("{name}.{HOLD_EXT}"))) {
            Lock::Held => out.kept.push(Kept { path: shown, reason: IN_USE, detail: None }),
            Lock::Missing => {
                // Gone meanwhile (its command just ended): nothing to report.
                let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
                let list = listed.get_or_insert_with(|| crate::worktree::list_worktrees(repo_root).ok());
                let thoth_made = meta.is_dir()
                    && match list {
                        Some(l) => is_listed(&path, l),
                        // git could not list them: a checkout has its `.git` file.
                        None => path.join(".git").is_file(),
                    };
                let reason = if thoth_made { NO_OWNER_RECORD } else { NOT_THOTH_CHECKOUT };
                out.kept.push(Kept { path: shown, reason, detail: None });
            }
            Lock::Taken(file) => {
                test_sweep_pause();
                // Looked at only now that the lock is taken: the command's
                // own cleanup may have removed the checkout after the folder
                // was read and before it let go of its lock. Nothing to report
                // then; its record without a checkout goes as a stale hold.
                if std::fs::symlink_metadata(&path).is_err() {
                    drop(file);
                    let _ = std::fs::remove_file(holds.join(format!("{name}.{HOLD_EXT}")));
                    continue;
                }
                match remove_checkout(repo_root, &path) {
                    Ok(()) => {
                        // Let go of the lock first: Windows cannot delete an open file.
                        drop(file);
                        let _ = std::fs::remove_file(holds.join(format!("{name}.{HOLD_EXT}")));
                        out.removed.push(Removed { path: shown, made_by: by, run_id });
                    }
                    Err(detail) => {
                        drop(file);
                        out.kept.push(Kept { path: shown, reason: REMOVE_FAILED, detail: Some(detail) });
                    }
                }
            }
        }
    }
    sweep_stale_holds(&dir, &holds);
    out
}

/// Test seam, debug builds only, so a release build has no such switch and
/// ignores the variable. With `THOTH_TEST_SWEEP_PAUSE=P`, the sweep writes the
/// file `P.reached` right after it takes the free lock of an entry's hold and
/// before it looks at that entry's checkout, then waits (at most 30 s) until
/// the file `P.go` exists, so a test can remove the checkout in between.
#[cfg(debug_assertions)]
fn test_sweep_pause() {
    let Some(p) = std::env::var_os("THOTH_TEST_SWEEP_PAUSE") else { return };
    let p = PathBuf::from(p);
    let with = |ext: &str| {
        let mut s = p.clone().into_os_string();
        s.push(ext);
        PathBuf::from(s)
    };
    let _ = std::fs::write(with(".reached"), b"");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !with(".go").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(not(debug_assertions))]
fn test_sweep_pause() {}

/// Deletes the hold files with no checkout (`NAME.hold` with no `NAME`) and
/// every `.pending` file whose lock nobody holds. Locked ones stay. Silent.
fn sweep_stale_holds(dir: &Path, holds: &Path) {
    let Ok(rd) = std::fs::read_dir(holds) else { return };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        let stale_candidate = if let Some(stem) = name.strip_suffix(&format!(".{HOLD_EXT}")) {
            std::fs::symlink_metadata(dir.join(stem)).is_err()
        } else {
            name.ends_with(&format!(".{PENDING_EXT}"))
        };
        if !stale_candidate || !entry.file_type().is_ok_and(|t| t.is_file()) {
            continue;
        }
        if let Lock::Taken(file) = take_lock(&path) {
            drop(file);
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn made_by_reads_only_thoths_two_name_shapes() {
        let u = "01M823VE9DYK1HR930H9JRSSFK";
        let r = "01M823XFDTJFD8CFC0PN3A876X";
        assert_eq!(made_by(&format!("suite-{u}")), Some((MADE_BY_SUITE, None)));
        assert_eq!(made_by(&format!("{r}-{u}")), Some((MADE_BY_AT_BASE, Some(r.to_string()))));
        for bad in [
            "notes".to_string(),
            "stray.txt".to_string(),
            format!("suite-{}", u.to_lowercase()),
            format!("suite-{u}x"),
            format!("{r}-{u}-{u}"),
            format!("{r}_{u}"),
            format!("suite-{u}.hold"),
            "suite-".to_string(),
        ] {
            assert_eq!(made_by(&bad), None, "{bad}");
        }
    }
}
