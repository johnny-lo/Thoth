use crate::config::Paths;
use anyhow::{bail, Result};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The **main** working tree's root for the repository containing `dir`.
///
/// `git rev-parse --show-toplevel` is wrong here: inside a linked worktree it
/// answers with that worktree's own root, and `ids::repo_id_for` hashes whatever
/// this returns - so a worker or reviewer running `thoth` from its worktree
/// would compute a different `repo_id` and be told the repo is not initialised
/// (Open question 5). `--git-common-dir` is the main tree's `.git` from
/// anywhere in the repository, main tree included, so its parent is the root.
/// Measured on git 2.53 today: in `.../p2probe/wt1`, `--show-toplevel` gives
/// `.../p2probe/wt1` while `--path-format=absolute --git-common-dir` gives
/// `.../p2probe/repo/.git`.
pub fn repo_root_from(dir: &Path) -> Result<PathBuf> {
    match find_repo_root(dir)? {
        Ok(root) => Ok(root),
        Err(stderr) => bail!("{} is not inside a git repository: {}", dir.display(), stderr),
    }
}

/// `repo_root_from` with "not inside a repository" told apart from a failure:
/// `Ok(Err(stderr))` when git exits non-zero (decided by the exit code, since
/// git's message may be translated), `Err` when git cannot start or answers
/// with nothing.
pub fn find_repo_root(dir: &Path) -> Result<std::result::Result<PathBuf, String>> {
    let out = crate::process::quiet(
        Command::new("git")
            .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
            .current_dir(dir),
    )
    .output()?;
    if !out.status.success() {
        return Ok(Err(String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    let common = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if common.is_empty() {
        bail!(
            "git rev-parse --git-common-dir returned nothing for {}",
            dir.display()
        );
    }
    let root = Path::new(&common)
        .parent()
        .ok_or_else(|| anyhow::anyhow!("git reported a common dir with no parent: {common}"))?;
    // git prints forward slashes even on Windows. Canonicalize to the native
    // form (and strip the `\\?\` verbatim prefix) so every path thoth stores
    // or prints uses the native separator, and so this stays consistent with
    // `ids::repo_id_for`'s own normalisation of the same root.
    let native = crate::config::canonical_display(root)?;
    Ok(Ok(PathBuf::from(native)))
}

/// `git rev-parse HEAD` inside `dir`. Used for a task's base_commit and for a
/// run's candidate_commit.
pub fn rev_parse_head(dir: &Path) -> Result<String> {
    let out = crate::process::quiet(
        Command::new("git").args(["rev-parse", "HEAD"]).current_dir(dir),
    )
    .output()?;
    if !out.status.success() {
        bail!(
            "git rev-parse HEAD failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The full sha of the commit `rev` names, resolved in `dir`: `HEAD` and
/// `HEAD~1` are that worktree's, branch and tag names are shared by every
/// worktree. An annotated tag peels to its commit; a tree, a blob or an unknown
/// name is an error. A name starting with `-` never reaches git, which would
/// read it as an option.
pub fn resolve_commit(dir: &Path, rev: &str) -> Result<String> {
    let rev = rev.trim();
    if rev.is_empty() {
        bail!("an empty name is not a commit");
    }
    if rev.starts_with('-') {
        bail!("a name starting with '-' is not a commit");
    }
    let out = crate::process::quiet(
        Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &format!("{rev}^{{commit}}")])
            .current_dir(dir),
    )
    .output()?;
    if !out.status.success() {
        bail!("no commit named {rev} in {}", dir.display());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Whether `dir` has anything to commit: any tracked or untracked change.
/// `.agent/` is already excluded through `info/exclude` (`ensure_worktree`),
/// so it never counts.
fn has_uncommitted_changes(dir: &Path) -> Result<bool> {
    Ok(!status_porcelain(dir)?.trim().is_empty())
}

/// `git status --porcelain` in `dir`, verbatim. The worktree sweep filters
/// Thoth's own `.agent/` and `.agents/` out of it itself
/// (`commands::worktrees::non_agent_changes`), because `info/exclude` may not
/// have been written.
pub fn status_porcelain(dir: &Path) -> Result<String> {
    git_capture(dir, &["status", "--porcelain"])
}

/// The branch `dir` has checked out, or `None` when its HEAD is detached (a
/// review worktree). `git symbolic-ref -q` exits 1 for a detached HEAD and
/// something else for a real failure, such as `dir` not being a repository.
pub fn head_branch(dir: &Path) -> Result<Option<String>> {
    let out = crate::process::quiet(
        Command::new("git")
            .args(["symbolic-ref", "-q", "--short", "HEAD"])
            .current_dir(dir),
    )
    .output()?;
    match out.status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string())),
        Some(1) => Ok(None),
        code => bail!(
            "git symbolic-ref HEAD failed in {} (exit {code:?}): {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
/// `git merge-base --is-ancestor` exits 0 for yes, 1 for no, and something
/// else for a real failure, such as an unknown commit.
pub fn is_ancestor(dir: &Path, ancestor: &str, descendant: &str) -> Result<bool> {
    let out = crate::process::quiet(
        Command::new("git")
            .args(["merge-base", "--is-ancestor", ancestor, descendant])
            .current_dir(dir),
    )
    .output()?;
    match out.status.code() {
        Some(0) => Ok(true),
        Some(1) => Ok(false),
        code => bail!(
            "git merge-base --is-ancestor failed in {} (exit {code:?}): {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// The local branches (`refs/heads/`) whose history contains `sha`. A commit
/// that only a detached HEAD reaches - in any worktree, the main one included -
/// gives an empty list. `git branch --contains` is not used: in a main tree
/// with a detached HEAD it adds a localised "HEAD detached" line.
pub fn branches_containing(repo_root: &Path, sha: &str) -> Result<Vec<String>> {
    Ok(git_capture(
        repo_root,
        &[
            "for-each-ref",
            "--contains",
            sha,
            "--format=%(refname:short)",
            "refs/heads/",
        ],
    )?
    .lines()
    .map(|l| l.trim().to_string())
    .filter(|l| !l.is_empty())
    .collect())
}

/// Why a worktree was kept when its directory links could not be removed
/// safely (`worktree_kept.detail.reason`).
pub const LINK_UNREMOVABLE: &str = "link_unremovable";

/// A worktree's directory links could not be removed safely, so nothing else
/// was done to it: no `git worktree remove`, no recursive delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkCleanupError {
    /// The entry that could not be read, inspected or removed.
    pub path: PathBuf,
    pub error: String,
}

impl LinkCleanupError {
    fn new(path: &Path, what: &str, e: impl std::fmt::Display) -> Self {
        LinkCleanupError { path: path.to_path_buf(), error: format!("{what}: {e}") }
    }

    /// The path and the error, for a `kept` entry's `detail`.
    pub fn detail(&self) -> String {
        format!("{}: {}", self.path.display(), self.error)
    }
}

impl std::fmt::Display for LinkCleanupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{LINK_UNREMOVABLE}: the worktree's directory links could not be removed safely, \
             so it was left in place: {}",
            self.detail()
        )
    }
}

impl std::error::Error for LinkCleanupError {}

/// Whether the entry `ft` describes (read without following it) is a link to
/// a directory. Windows: a directory symlink or a junction (both are
/// name-surrogate reparse points, which std reports as symlinks). Elsewhere:
/// a symlink whose target is a directory or does not exist (missing, or a
/// path through a file); a dangling one is removed too, since unlinking never
/// touches a target. When the target cannot be read for any other reason
/// (permission denied, a link loop), nobody can tell what it is, so that is
/// an error naming the link, and the caller leaves the worktree alone.
fn is_dir_link(
    path: &Path,
    ft: &std::fs::FileType,
) -> std::result::Result<bool, LinkCleanupError> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTypeExt;
        let _ = path;
        Ok(ft.is_symlink_dir())
    }
    #[cfg(not(windows))]
    {
        use std::io::ErrorKind;
        if !ft.is_symlink() {
            return Ok(false);
        }
        match std::fs::metadata(path) {
            Ok(m) => Ok(m.is_dir()),
            Err(e) if matches!(e.kind(), ErrorKind::NotFound | ErrorKind::NotADirectory) => Ok(true),
            Err(e) => Err(LinkCleanupError::new(path, "reading the link's target failed", e)),
        }
    }
}

/// Removes one directory link as a directory entry, never its target.
fn remove_dir_link(path: &Path) -> std::io::Result<()> {
    // Windows: RemoveDirectoryW on a junction or directory symlink drops the
    // reparse point itself. Unix: unlink never follows a symlink.
    #[cfg(windows)]
    {
        std::fs::remove_dir(path)
    }
    #[cfg(not(windows))]
    {
        std::fs::remove_file(path)
    }
}

/// Walks `root` without descending into links and skipping every `.git`
/// entry, and removes each directory symlink or junction it finds as a link
/// entry, never touching what it points to. Run before `git worktree remove
/// --force`, which on Windows has followed a `node_modules` junction and
/// emptied its target (2026-10-14). Any read, metadata or removal failure -
/// including a link whose target cannot be read - stops here with the entry
/// and the error, leaving that link in place; the caller must then leave the
/// worktree alone. A `root` that does not exist is not an error (git reports
/// it); a `root` that is itself a link is.
pub fn remove_dir_links(root: &Path) -> std::result::Result<usize, LinkCleanupError> {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(LinkCleanupError::new(root, "reading metadata failed", e)),
    };
    if meta.file_type().is_symlink() {
        return Err(LinkCleanupError::new(
            root,
            "refusing",
            "the worktree path is itself a link",
        ));
    }
    if !meta.is_dir() {
        return Ok(0);
    }
    let mut removed = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| LinkCleanupError::new(&dir, "reading the directory failed", e))?;
        for entry in entries {
            let entry =
                entry.map_err(|e| LinkCleanupError::new(&dir, "reading the directory failed", e))?;
            if entry.file_name() == ".git" {
                continue;
            }
            let path = entry.path();
            let ft = std::fs::symlink_metadata(&path)
                .map_err(|e| LinkCleanupError::new(&path, "reading metadata failed", e))?
                .file_type();
            if is_dir_link(&path, &ft)? {
                remove_dir_link(&path)
                    .map_err(|e| LinkCleanupError::new(&path, "removing the link failed", e))?;
                removed += 1;
            } else if ft.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(removed)
}

/// `git worktree remove --force PATH`, and nothing else: no fallback to
/// deleting the directory. The worktree sweep has already decided the
/// directory is safe to drop; when git still refuses (a file locked on
/// Windows), the caller keeps the directory and reports git's message.
/// The branch is left alone - `git worktree remove` never deletes one.
/// Every directory link inside is removed first (`remove_dir_links`); when
/// that fails the error is a `LinkCleanupError` and git is never run.
pub fn remove_worktree_force(repo_root: &Path, path: &Path) -> Result<()> {
    remove_dir_links(path)?;
    let mut cmd = Command::new("git");
    cmd.arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(path)
        .current_dir(repo_root);
    crate::process::quiet(&mut cmd);
    let out = cmd.output()?;
    if !out.status.success() {
        // Same joining as `ensure_worktree`: a separator only when both
        // streams have something to say.
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let combined = match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => String::new(),
            (true, false) => stderr,
            (false, true) => stdout,
            (false, false) => format!("{stdout}\n{stderr}"),
        };
        bail!("git worktree remove failed (exit {:?}): {combined}", out.status.code());
    }
    Ok(())
}

/// Whether `git config` resolves both `user.name` and `user.email` for `dir`
/// (repo-local, or the user's own global/system config) - if either is
/// missing, `git commit` would refuse rather than guess.
pub fn has_configured_identity(dir: &Path) -> bool {
    let configured = |key: &str| {
        crate::process::quiet(Command::new("git").args(["config", key]).current_dir(dir))
            .output()
            .map(|o| o.status.success() && !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false)
    };
    configured("user.name") && configured("user.email")
}

/// Commits whatever a codex worker left in `dir` on Thoth's behalf: codex's
/// sandbox cannot write a linked worktree's own gitdir, so it cannot commit
/// itself (docs/post-phase2/codex-worker-scope-2026-09-27.md §2.6, E9). Adds
/// everything (`.agent/` is already excluded) and commits with `message`,
/// under the repository's own configured identity, or a fixed fallback when
/// none is configured anywhere. `Ok(None)` when there was nothing to commit -
/// the worker made no change, or already committed everything itself.
pub fn commit_worker_changes(dir: &Path, message: &str) -> Result<Option<String>> {
    if !has_uncommitted_changes(dir)? {
        return Ok(None);
    }
    let mut add = Command::new("git");
    add.args(["add", "-A"]).current_dir(dir);
    crate::process::quiet(&mut add);
    let out = add.output()?;
    if !out.status.success() {
        bail!(
            "git add -A failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let mut commit = Command::new("git");
    if !has_configured_identity(dir) {
        commit.args(["-c", "user.name=thoth", "-c", "user.email=thoth@localhost"]);
    }
    commit.args(["commit", "-m", message]).current_dir(dir);
    crate::process::quiet(&mut commit);
    let out = commit.output()?;
    if !out.status.success() {
        bail!(
            "git commit failed in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(Some(rev_parse_head(dir)?))
}

/// One branch per (task, attempt). Phase 0 proved a shared branch name makes the
/// second `worktree add` fatal (03-process-and-worktree.md:235-242).
pub fn branch_name(task_id: &str, attempt: i64) -> String {
    format!("thoth/{task_id}/a{attempt}")
}

pub fn worktree_path(paths: &Paths, repo_id: &str, task_id: &str, attempt: i64) -> PathBuf {
    paths
        .worktrees_dir(repo_id)
        .join(format!("{task_id}-a{attempt}"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnsureOutcome {
    Created,
    Reused,
}

/// Make the repository ignore `.agent/` (Thoth's per-worktree files) through
/// `<git common dir>/info/exclude`, which every linked worktree shares. The
/// tracked `.gitignore` is never touched. The line is appended only when absent.
fn ensure_agent_dir_excluded(repo_root: &Path) -> Result<()> {
    let common = git_capture(
        repo_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )?;
    let exclude = Path::new(common.trim()).join("info").join("exclude");
    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Bytes, not a String: git accepts any bytes in a pattern, and a non-UTF-8
    // line elsewhere in the file must not stop `.agent/` from being added.
    let existing = match std::fs::read(&exclude) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    // Git keeps leading spaces as part of a pattern, so only the line ending and
    // trailing whitespace are ignored here. `.agents/` holds a harness's own
    // workspace files (its hooks) and is excluded the same way.
    let missing: Vec<&str> = [".agent/", ".agents/"]
        .into_iter()
        .filter(|p| {
            !existing
                .split(|b| *b == b'\n')
                .any(|l| l.trim_ascii_end() == p.as_bytes())
        })
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    // Append-only: a concurrent writer or a kill mid-write can at worst leave a
    // duplicate line, never truncate the user's own patterns.
    let mut addition = String::new();
    if !existing.is_empty() && !existing.ends_with(b"\n") {
        addition.push('\n');
    }
    for pattern in missing {
        addition.push_str(pattern);
        addition.push('\n');
    }
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(&exclude)?;
    file.write_all(addition.as_bytes())?;
    Ok(())
}

/// A failure to write the exclude file must never fail a dispatch or review:
/// the worktree works either way, `.agent/` just shows up as untracked. The
/// caller gets the message back so it can put it in its `warnings[]`.
pub fn exclude_agent_dir_or_warn(repo_root: &Path) -> Option<String> {
    ensure_agent_dir_excluded(repo_root).err().map(|e| {
        format!("could not add .agent/ and .agents/ to the repository's info/exclude: {e}")
    })
}

/// What `ensure_worktree` / `ensure_detached_worktree` did, plus a non-fatal
/// warning for the caller's `warnings[]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ensured {
    pub outcome: EnsureOutcome,
    pub warning: Option<String>,
}

/// Create the worktree, or report that it already exists. Reuse is the normal
/// path for reroute and resume (brief section 9), and it never rewinds the
/// worker's commits.
pub fn ensure_worktree(
    repo_root: &Path,
    path: &Path,
    branch: &str,
    base_commit: &str,
) -> Result<Ensured> {
    let warning = exclude_agent_dir_or_warn(repo_root);
    if path.join(".git").exists() {
        return Ok(Ensured { outcome: EnsureOutcome::Reused, warning });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // If `branch` already exists (e.g. still checked out by another worktree
    // for this task/attempt), `-b` would fail with git's generic "branch
    // already exists" instead of the specific, more useful "already used by
    // worktree at ..." (Phase 0 03-process-and-worktree.md:235-242). Checking
    // out an existing branch (no `-b`) surfaces that specific message.
    let branch_exists = crate::process::quiet(
        Command::new("git")
            .args(["show-ref", "--verify", "--quiet", &format!("refs/heads/{branch}")])
            .current_dir(repo_root),
    )
    .status()?
    .success();
    let mut cmd = Command::new("git");
    cmd.arg("worktree").arg("add").arg(path);
    if branch_exists {
        cmd.arg(branch);
    } else {
        cmd.arg("-b").arg(branch).arg(base_commit);
    }
    cmd.current_dir(repo_root);
    crate::process::quiet(&mut cmd);
    let out = cmd.output()?;
    if !out.status.success() {
        // git's own message goes through verbatim so it can land in events.
        // #12 (final review): join with a separator only when both streams
        // have something to say - concatenating them bare could otherwise
        // run the last word of stdout into the first word of stderr.
        let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        let combined = match (stdout.is_empty(), stderr.is_empty()) {
            (true, true) => String::new(),
            (true, false) => stderr,
            (false, true) => stdout,
            (false, false) => format!("{stdout}\n{stderr}"),
        };
        bail!("git worktree add failed (exit {:?}): {combined}", out.status.code());
    }
    Ok(Ensured { outcome: EnsureOutcome::Created, warning })
}

/// A read-only checkout of one commit, for the reviewer (brief §1).
///
/// `--detach` rather than a branch: HEAD is not a ref anybody else owns, so
/// two reviews of the same candidate can coexist and a reviewer cannot move the
/// implementer's branch even if it tried. Reuse is detected the same way
/// `ensure_worktree` does it, so a re-run of `review start` is cheap.
pub fn ensure_detached_worktree(
    repo_root: &Path,
    path: &Path,
    commit: &str,
) -> Result<Ensured> {
    let warning = exclude_agent_dir_or_warn(repo_root);
    if path.join(".git").exists() {
        return Ok(Ensured { outcome: EnsureOutcome::Reused, warning });
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut cmd = Command::new("git");
    cmd.arg("worktree")
        .arg("add")
        .arg("--detach")
        .arg(path)
        .arg(commit)
        .current_dir(repo_root);
    // Like every other git invocation in this module (hotfix c397777): a bare
    // `std::process::Command` would pop a console window on Windows.
    crate::process::quiet(&mut cmd);
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "git worktree add --detach failed (exit {:?}): {}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(Ensured { outcome: EnsureOutcome::Created, warning })
}

/// Moves an existing detached worktree to `commit` (SR-2: the spec-review
/// worktree follows the commit `spec review start` picks for the task).
/// `ensure_detached_worktree`
/// reuses a worktree without looking at its commit, so the caller moves it.
pub fn checkout_detached(dir: &Path, commit: &str) -> Result<()> {
    let mut cmd = Command::new("git");
    cmd.arg("checkout").arg("--detach").arg(commit).current_dir(dir);
    // Like every other git invocation in this module (hotfix c397777).
    crate::process::quiet(&mut cmd);
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "git checkout --detach failed (exit {:?}): {}{}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).trim(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Remove a linked worktree that `ensure_detached_worktree` made for a
/// throwaway job (the `verify --at-base` checkout). Best effort by design: it
/// runs from a drop guard, where there is nobody to return an error to, so it
/// falls back to deleting the directory and pruning git's bookkeeping. The
/// directory links inside are removed first; when that fails nothing else
/// happens - neither git nor the recursive delete runs - and the error says
/// which entry and why, for the caller to report.
pub fn remove_worktree(
    repo_root: &Path,
    path: &Path,
) -> std::result::Result<(), LinkCleanupError> {
    remove_dir_links(path)?;
    let mut cmd = Command::new("git");
    cmd.arg("worktree")
        .arg("remove")
        .arg("--force")
        .arg(path)
        .current_dir(repo_root);
    let removed = crate::process::quiet(&mut cmd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !removed {
        let _ = std::fs::remove_dir_all(path);
        let mut prune = Command::new("git");
        prune.args(["worktree", "prune"]).current_dir(repo_root);
        let _ = crate::process::quiet(&mut prune).output();
    }
    Ok(())
}

/// The local branches under `refs/heads/PREFIX`, as names without
/// `refs/heads/` (full ref names are read, so a tag of the same name cannot
/// change how a branch is spelled).
pub fn local_branches_under(repo_root: &Path, prefix: &str) -> Result<Vec<String>> {
    let pattern = format!("refs/heads/{prefix}");
    Ok(git_capture(repo_root, &["for-each-ref", "--format=%(refname)", &pattern])?
        .lines()
        .filter_map(|l| l.trim().strip_prefix("refs/heads/").map(str::to_string))
        .filter(|l| !l.is_empty())
        .collect())
}

/// The branches some worktree (the main one included) has checked out, as
/// names without `refs/heads/`.
pub fn checked_out_branches(repo_root: &Path) -> Result<Vec<String>> {
    Ok(git_capture(repo_root, &["worktree", "list", "--porcelain"])?
        .lines()
        .filter_map(|l| l.trim_end().strip_prefix("branch refs/heads/").map(str::to_string))
        .collect())
}

/// The commit a full ref name points to: `Ok(None)` when there is no such ref
/// (`rev-parse --verify --quiet` exits 1), `Err` for a real failure.
pub fn ref_commit(repo_root: &Path, refname: &str) -> Result<Option<String>> {
    let out = crate::process::quiet(
        Command::new("git")
            .args(["rev-parse", "--verify", "--quiet", &format!("{refname}^{{commit}}")])
            .current_dir(repo_root),
    )
    .output()?;
    match out.status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string())),
        Some(1) => Ok(None),
        code => bail!(
            "git rev-parse --verify {refname} failed in {} (exit {code:?}): {}",
            repo_root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// The merge commits in `base..branch` (both full ref names).
pub fn merges_between(repo_root: &Path, base: &str, branch: &str) -> Result<Vec<String>> {
    let range = format!("{base}..{branch}");
    Ok(git_capture(repo_root, &["rev-list", "--merges", &range])?
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// VSCOPE-1: the merge commits on the first-parent line of `from..to`,
/// oldest first, each with its subject.
pub fn first_parent_merges(repo_root: &Path, from: &str, to: &str) -> Result<Vec<(String, String)>> {
    let range = format!("{from}..{to}");
    Ok(git_capture(
        repo_root,
        &["log", "--first-parent", "--merges", "--reverse", "--format=%H%x09%s", &range],
    )?
    .lines()
    .filter_map(|l| {
        let (sha, subject) = l.split_once('\t')?;
        Some((sha.trim().to_string(), subject.trim_end().to_string()))
    })
    .collect())
}

/// `git cherry BASE BRANCH` verbatim: one line per commit of `BASE..BRANCH`,
/// `-` when an equivalent change is already in `BASE`, `+` when not.
pub fn cherry(repo_root: &Path, base: &str, branch: &str) -> Result<String> {
    git_capture(repo_root, &["cherry", base, branch])
}

/// Deletes the local branch `branch`. Without `force`, `git branch -d`, with
/// `BASE` standing in as the branch's upstream for this one command: git then
/// checks the branch is merged into `refs/heads/BASE` rather than into
/// whatever the main tree has checked out. With `force`, `git branch -D`.
/// git's own refusal (a checked-out branch, a lock) comes back as the error.
pub fn delete_branch(repo_root: &Path, branch: &str, base: &str, force: bool) -> Result<()> {
    let mut cmd = Command::new("git");
    if !force {
        cmd.arg("-c")
            .arg(format!("branch.{branch}.remote=."))
            .arg("-c")
            .arg(format!("branch.{branch}.merge=refs/heads/{base}"));
    }
    cmd.arg("branch")
        .arg(if force { "-D" } else { "-d" })
        .arg("--")
        .arg(branch)
        .current_dir(repo_root);
    crate::process::quiet(&mut cmd);
    let out = cmd.output()?;
    if !out.status.success() {
        bail!(
            "git branch {} {branch} failed (exit {:?}): {}",
            if force { "-D" } else { "-d" },
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// `git worktree prune`: drop registrations whose directory is gone. Best effort.
pub fn prune_worktrees(repo_root: &Path) {
    let mut prune = Command::new("git");
    prune.args(["worktree", "prune"]).current_dir(repo_root);
    let _ = crate::process::quiet(&mut prune).output();
}

/// Runs git in `dir` and returns its stdout, or an error carrying git's own
/// message. Every read-only helper below goes through this so the failure shape
/// is identical and lands intact in an event.
///
/// Goes through `crate::process::quiet` like every other git invocation in this
/// module (hotfix c397777): a bare `std::process::Command` would pop a console
/// window on Windows.
fn git_capture(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8_lossy(&git_capture_bytes(dir, args)?).to_string())
}

/// Like `git_capture`, but hands back git's stdout byte for byte. For the
/// evidence blobs, which must be exactly what git produced.
fn git_capture_bytes(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let out = crate::process::quiet(Command::new("git").args(args).current_dir(dir)).output()?;
    if !out.status.success() {
        bail!(
            "git {:?} failed in {} (exit {:?}): {}",
            args,
            dir.display(),
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(out.stdout)
}

/// `git diff --stat <from>..<to>`. Run inside the worktree: a linked worktree
/// shares the main tree's object store, so both commits are reachable there.
pub fn diff_stat(dir: &Path, from: &str, to: &str) -> Result<String> {
    let range = format!("{from}..{to}");
    Ok(git_capture(dir, &["diff", "--stat", &range])?
        .trim_end()
        .to_string())
}

/// The repo-relative paths the range touches. git prints forward slashes on
/// Windows too, so these strings are directly comparable across platforms -
/// which is what `touches_tests` relies on.
pub fn diff_name_only(dir: &Path, from: &str, to: &str) -> Result<Vec<String>> {
    let range = format!("{from}..{to}");
    Ok(parse_name_only_z(&git_capture_bytes(
        dir,
        &["diff", "--name-only", "-z", &range],
    )?))
}

/// Parses `git diff --name-only -z`: each path followed by a NUL. Paths are
/// passed on as git wrote them - no trim, no unquoting.
fn parse_name_only_z(bytes: &[u8]) -> Vec<String> {
    split_z(bytes)
        .into_iter()
        .map(|p| String::from_utf8_lossy(&p).to_string())
        .collect()
}

/// The NUL-terminated paths of a `-z` listing, as raw bytes.
fn split_z(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(<[u8]>::to_vec)
        .collect()
}

/// `git diff --name-status --no-renames <from>..<to>` as `(status letter,
/// path)` pairs, in git's order. Without rename detection a moved file is a
/// `D` of the old path and an `A` of the new one, so a test file cannot leave
/// the protected set by being renamed. `-z` makes git print paths raw: with
/// line output git quotes and escapes a path holding `"`, a backslash or a
/// control character, and the quoted form would not classify as a test file.
pub fn diff_name_status(dir: &Path, from: &str, to: &str) -> Result<Vec<(char, String)>> {
    let range = format!("{from}..{to}");
    Ok(parse_name_status_z(&git_capture_bytes(
        dir,
        &["diff", "--name-status", "--no-renames", "-z", &range],
    )?))
}

/// LEAN-1: the bytes of `path` at `commit` (`git cat-file blob`), as git
/// stores them. An error when the path is not a blob there (missing, a
/// gitlink, an object that cannot be read).
pub fn blob_at(dir: &Path, commit: &str, path: &str) -> Result<Vec<u8>> {
    git_capture_bytes(dir, &["cat-file", "blob", &format!("{commit}:{path}")])
}

/// Parses `git diff --name-status --no-renames -z`: status, NUL, path, NUL,
/// repeated. Paths are passed on as git wrote them - no trim, no unquoting.
fn parse_name_status_z(bytes: &[u8]) -> Vec<(char, String)> {
    let mut parts = bytes.split(|b| *b == 0);
    let mut out = Vec::new();
    while let (Some(status), Some(path)) = (parts.next(), parts.next()) {
        let Some(letter) = status.first() else { continue };
        if path.is_empty() {
            continue;
        }
        out.push((*letter as char, String::from_utf8_lossy(path).to_string()));
    }
    out
}

/// The full patch, stat block included. This is the blob behind the `diff`
/// evidence, and therefore literally what `thoth evidence show` hands a
/// reviewer (v6 §4: "必讀清單 1. Diff —— 明確包含測試檔案的 diff").
/// Returned as git's raw bytes: a patch touching a non-UTF-8 file must not be
/// rewritten with U+FFFD before it is stored.
pub fn diff_patch(dir: &Path, from: &str, to: &str) -> Result<Vec<u8>> {
    let range = format!("{from}..{to}");
    git_capture_bytes(dir, &["diff", "--patch", "--stat", &range])
}

/// SMALL-9c: the best common ancestor of `a` and `b` (`git merge-base`), or
/// `None` when they have none. git exits 1 with nothing printed when there is
/// no merge base; any other failure is an error.
pub fn merge_base(dir: &Path, a: &str, b: &str) -> Result<Option<String>> {
    let out = crate::process::quiet(
        Command::new("git").args(["merge-base", a, b]).current_dir(dir),
    )
    .output()?;
    match out.status.code() {
        Some(0) => {
            let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if sha.is_empty() {
                bail!("git merge-base printed nothing in {}", dir.display());
            }
            Ok(Some(sha))
        }
        Some(1) if out.stdout.is_empty() && out.stderr.is_empty() => Ok(None),
        code => bail!(
            "git merge-base failed in {} (exit {code:?}): {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// SMALL-9c: every file-level path the range changes, as `git diff` lists
/// it: a blob, symlink or gitlink entry that differs (object, type or mode;
/// an added or deleted path counts). A directory is never listed itself,
/// only what changed below it; a path that is a file on one side and a
/// directory on the other is listed for its file side. Without rename
/// detection, so a moved file is both of its paths, and with submodule
/// entries always compared by commit whatever `diff.ignoreSubmodules` says.
/// Paths are git's raw bytes, not decoded: a path that is not UTF-8 and one
/// that holds U+FFFD must not compare equal.
pub fn changed_paths(dir: &Path, from: &str, to: &str) -> Result<Vec<Vec<u8>>> {
    let range = format!("{from}..{to}");
    Ok(split_z(&git_capture_bytes(
        dir,
        &[
            "diff",
            "--name-only",
            "--no-renames",
            "--ignore-submodules=none",
            "-z",
            &range,
        ],
    )?))
}

/// SMALL-9c: every path whose tree entry differs between `from` and `to`,
/// directories included: like `changed_paths`, plus each directory whose
/// tree object differs (`git diff-tree -r -t`). A path absent from this list
/// has the same tree entry (object, type and mode) in both commits, or is
/// absent from both; a directory is judged by its own tree object, never by
/// the paths that changed below it. Raw bytes, as in `changed_paths`.
pub fn changed_entries(dir: &Path, from: &str, to: &str) -> Result<Vec<Vec<u8>>> {
    Ok(split_z(&git_capture_bytes(
        dir,
        &[
            "diff-tree",
            "-r",
            "-t",
            "--no-renames",
            "--ignore-submodules=none",
            "--name-only",
            "-z",
            from,
            to,
        ],
    )?))
}

/// SMALL-9c: the pathspec that keeps the whole tree except `exclude`, each
/// path matched literally (no glob, no magic) and from the top of the tree
/// whatever directory git runs in. Empty when nothing is excluded, so the
/// command is exactly the unfiltered one. Like every git pathspec, an
/// excluded path also excludes everything below it: a caller that must not
/// hide such a path checks for one first.
fn excluding_pathspec(exclude: &[String]) -> Vec<String> {
    if exclude.is_empty() {
        return Vec::new();
    }
    let mut args = vec!["--".to_string(), ":/".to_string()];
    args.extend(exclude.iter().map(|p| format!(":(top,exclude,literal){p}")));
    args
}

/// Runs `git <base args> [-- :/ :(top,exclude,literal)PATH...]`.
fn git_capture_bytes_excluding(dir: &Path, base: &[&str], exclude: &[String]) -> Result<Vec<u8>> {
    let spec = excluding_pathspec(exclude);
    let mut args: Vec<&str> = base.to_vec();
    args.extend(spec.iter().map(String::as_str));
    git_capture_bytes(dir, &args)
}

/// SMALL-9c: `diff_stat` without the `exclude` paths.
pub fn diff_stat_excluding(dir: &Path, from: &str, to: &str, exclude: &[String]) -> Result<String> {
    let range = format!("{from}..{to}");
    let bytes = git_capture_bytes_excluding(dir, &["diff", "--stat", &range], exclude)?;
    Ok(String::from_utf8_lossy(&bytes).trim_end().to_string())
}

/// SMALL-9c: `diff_name_only` without the `exclude` paths.
pub fn diff_name_only_excluding(
    dir: &Path,
    from: &str,
    to: &str,
    exclude: &[String],
) -> Result<Vec<String>> {
    let range = format!("{from}..{to}");
    Ok(parse_name_only_z(&git_capture_bytes_excluding(
        dir,
        &["diff", "--name-only", "-z", &range],
        exclude,
    )?))
}

/// SMALL-9c: `diff_patch` without the `exclude` paths; with nothing excluded
/// it is `diff_patch` byte for byte.
pub fn diff_patch_excluding(dir: &Path, from: &str, to: &str, exclude: &[String]) -> Result<Vec<u8>> {
    let range = format!("{from}..{to}");
    git_capture_bytes_excluding(dir, &["diff", "--patch", "--stat", &range], exclude)
}

/// `git show --stat` for one commit: three `--format` lines (full sha, subject,
/// author date) then git's stat block. Measured on git 2.53: adding `-s`
/// suppresses the stat, so `-s` belongs only to `commit_meta`.
/// Raw bytes, like `diff_patch`: this is the commit blob.
pub fn show_stat(dir: &Path, sha: &str) -> Result<Vec<u8>> {
    git_capture_bytes(dir, &["show", "--stat", "--format=%H%n%s%n%aI", sha])
}

/// `(subject, author_date)` for one commit, the two fields v6 §10.5's
/// `detail` comment asks for on a `commit` evidence row.
pub fn commit_meta(dir: &Path, sha: &str) -> Result<(String, String)> {
    let text = git_capture(dir, &["show", "-s", "--format=%s%n%aI", sha])?;
    let mut lines = text.lines();
    let subject = lines.next().unwrap_or("").trim().to_string();
    let date = lines.next().unwrap_or("").trim().to_string();
    Ok((subject, date))
}

/// `git log --oneline <base>..HEAD`: the commits a run made on top of its base.
/// For the handoff file (S3-9).
pub fn log_oneline_since(dir: &Path, base: &str) -> Result<String> {
    let range = format!("{base}..HEAD");
    git_capture(dir, &["log", "--oneline", &range])
}

/// `git status --short`: what is changed but not committed. For the handoff
/// file (S3-9). `.agent/` is excluded, so Thoth's own files never show.
pub fn status_short(dir: &Path) -> Result<String> {
    git_capture(dir, &["status", "--short"])
}

/// `git diff --stat HEAD`: the size of the uncommitted change. For the handoff
/// file (S3-9).
pub fn diff_stat_head(dir: &Path) -> Result<String> {
    git_capture(dir, &["diff", "--stat", "HEAD"])
}

/// Paths from `git worktree list --porcelain`, main tree first.
// Called by `checks add --exe-pin path` (and its approval) to refuse a program inside any worktree.
pub fn list_worktrees(repo_root: &Path) -> Result<Vec<PathBuf>> {
    let out = crate::process::quiet(
        Command::new("git")
            .args(["worktree", "list", "--porcelain"])
            .current_dir(repo_root),
    )
    .output()?;
    if !out.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    Ok(text
        .lines()
        .filter_map(|l| l.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P7-2 / AC12 (b): the `-z` name-status parser keeps every path verbatim.
    #[test]
    fn name_status_z_parser_keeps_quotes_spaces_and_unicode_verbatim() {
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(b"M\0tests/a\"b.rs\0");
        input.extend_from_slice(b"D\0 data/lead.json \0");
        input.extend_from_slice("A\0中文/測試.rs\0".as_bytes());
        input.extend_from_slice(b"T\0bad\xFF.rs\0");
        let got = parse_name_status_z(&input);
        assert_eq!(got.len(), 4, "got {got:?}");
        assert_eq!(got[0], ('M', "tests/a\"b.rs".to_string()));
        assert_eq!(got[1], ('D', " data/lead.json ".to_string()));
        assert_eq!(got[2], ('A', "中文/測試.rs".to_string()));
        assert_eq!(got[3].0, 'T');
    }

    /// P7-2 / AC12 (b): the `-z` name-only parser keeps every path verbatim.
    #[test]
    fn name_only_z_parser_keeps_quotes_spaces_and_unicode_verbatim() {
        let mut input: Vec<u8> = Vec::new();
        input.extend_from_slice(b"tests/a\"b.rs\0");
        input.extend_from_slice(b" data/lead.json \0");
        input.extend_from_slice("中文/測試.rs\0".as_bytes());
        let got = parse_name_only_z(&input);
        assert_eq!(got.len(), 3, "got {got:?}");
        assert_eq!(got[0], "tests/a\"b.rs");
        assert_eq!(got[1], " data/lead.json ");
        assert_eq!(got[2], "中文/測試.rs");
    }

    // Most tests only care about the outcome; the warning has its own test.
    fn ensure_worktree(
        repo_root: &Path,
        path: &Path,
        branch: &str,
        base_commit: &str,
    ) -> Result<EnsureOutcome> {
        super::ensure_worktree(repo_root, path, branch, base_commit).map(|e| e.outcome)
    }

    fn ensure_detached_worktree(
        repo_root: &Path,
        path: &Path,
        commit: &str,
    ) -> Result<EnsureOutcome> {
        super::ensure_detached_worktree(repo_root, path, commit).map(|e| e.outcome)
    }

    /// A throwaway repo with two commits. Duplicated from tests/common/mod.rs on
    /// purpose: unit tests and integration tests are separate crates.
    fn tmp_repo() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let run = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(r).output().unwrap();
            assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "thoth test"]);
        std::fs::write(r.join("a.txt"), "one\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-m", "first"]);
        std::fs::write(r.join("b.txt"), "two\n").unwrap();
        run(&["add", "b.txt"]);
        run(&["commit", "-m", "second"]);
        let head = rev_parse_head(r).unwrap();
        (dir, head)
    }

    #[test]
    fn branch_and_path_naming_carry_the_attempt_number() {
        assert_eq!(
            branch_name("01HZTASK", 1),
            "thoth/01HZTASK/a1"
        );
        assert_eq!(branch_name("01HZTASK", 2), "thoth/01HZTASK/a2");

        let paths = crate::config::Paths::from_root(std::path::PathBuf::from("C:/ah"));
        assert_eq!(
            worktree_path(&paths, "repo01", "01HZTASK", 1),
            std::path::PathBuf::from("C:/ah/repos/repo01/worktrees/01HZTASK-a1")
        );
    }

    #[test]
    fn ensure_worktree_creates_then_reuses() {
        let (repo, head) = tmp_repo();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt = wt_parent.path().join("01HZTASK-a1");
        let branch = branch_name("01HZTASK", 1);

        let first = ensure_worktree(repo.path(), &wt, &branch, &head).unwrap();
        assert_eq!(first, EnsureOutcome::Created);
        assert!(wt.join("a.txt").is_file());
        assert!(wt.join("b.txt").is_file());
        assert_eq!(rev_parse_head(&wt).unwrap(), head);

        // a second call on the same path must not try `worktree add` again
        let second = ensure_worktree(repo.path(), &wt, &branch, &head).unwrap();
        assert_eq!(second, EnsureOutcome::Reused);
    }

    fn status_porcelain(dir: &Path) -> String {
        git_capture(dir, &["status", "--porcelain"]).unwrap()
    }

    #[test]
    fn worktrees_ignore_the_agent_dir_through_info_exclude() {
        let (repo, head) = tmp_repo();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("01HZTASK-a1");
        ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
        std::fs::create_dir_all(wt.join(".agent")).unwrap();
        std::fs::write(wt.join(".agent").join("result.json"), "{}").unwrap();
        std::fs::create_dir_all(wt.join(".agents")).unwrap();
        std::fs::write(wt.join(".agents").join("hooks.json"), "{}").unwrap();
        assert_eq!(status_porcelain(&wt), "", "the worktree must look clean");

        let detached = holder.path().join("review-a1");
        ensure_detached_worktree(repo.path(), &detached, &head).unwrap();
        std::fs::create_dir_all(detached.join(".agent")).unwrap();
        std::fs::write(detached.join(".agent").join("review.json"), "{}").unwrap();
        std::fs::create_dir_all(detached.join(".agents")).unwrap();
        std::fs::write(detached.join(".agents").join("hooks.json"), "{}").unwrap();
        assert_eq!(status_porcelain(&detached), "");

        let exclude = std::fs::read_to_string(repo.path().join(".git/info/exclude")).unwrap();
        assert_eq!(exclude.lines().filter(|l| *l == ".agent/").count(), 1, "{exclude}");
        assert_eq!(exclude.lines().filter(|l| *l == ".agents/").count(), 1, "{exclude}");
        assert!(!repo.path().join(".gitignore").exists());
    }

    #[test]
    fn an_unwritable_exclude_file_does_not_fail_worktree_creation() {
        let (repo, head) = tmp_repo();
        let exclude = repo.path().join(".git/info/exclude");
        // Windows: a directory where the file should be makes every write
        // fail, and git only warns about it.
        #[cfg(windows)]
        {
            let _ = std::fs::remove_file(&exclude);
            std::fs::create_dir_all(&exclude).unwrap();
        }
        // Linux: git itself refuses a directory there (`fatal: cannot use
        // .git/info/exclude as an exclude file`), so a read-only file stands
        // in - git can still read it, only the append fails. Root ignores the
        // permission bits, so there is nothing to test as root.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if unsafe { libc::geteuid() } == 0 {
                return;
            }
            std::fs::write(&exclude, "mine/\n").unwrap();
            std::fs::set_permissions(&exclude, std::fs::Permissions::from_mode(0o444)).unwrap();
        }
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("01HZTASK-a1");
        let ensured =
            super::ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
        assert_eq!(ensured.outcome, EnsureOutcome::Created);
        let warning = ensured.warning.expect("the failed exclude write must be reported");
        assert!(warning.contains("info/exclude"), "{warning}");

        let detached = holder.path().join("review-a1");
        let ensured = super::ensure_detached_worktree(repo.path(), &detached, &head).unwrap();
        assert_eq!(ensured.outcome, EnsureOutcome::Created);
        assert!(ensured.warning.is_some());
    }

    #[test]
    fn exclude_is_append_only_and_matches_the_pattern_exactly() {
        let (repo, head) = tmp_repo();
        let exclude = repo.path().join(".git/info/exclude");
        // no trailing newline, and an indented look-alike that git would not
        // treat as `.agent/`
        std::fs::write(&exclude, "mine/\n  .agent/").unwrap();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("01HZTASK-a1");
        let ensured =
            super::ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
        assert_eq!(ensured.warning, None);
        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            "mine/\n  .agent/\n.agent/\n.agents/\n"
        );
    }

    #[test]
    fn a_non_utf8_exclude_file_still_gets_the_pattern_once_and_no_warning() {
        let (repo, _head) = tmp_repo();
        let exclude = repo.path().join(".git/info/exclude");
        let mut bytes = b"caf\xe9/\n".to_vec();
        bytes.extend_from_slice(b"mine/");
        std::fs::write(&exclude, &bytes).unwrap();

        assert_eq!(exclude_agent_dir_or_warn(repo.path()), None);
        assert_eq!(exclude_agent_dir_or_warn(repo.path()), None);

        let mut expected = bytes.clone();
        expected.extend_from_slice(b"\n.agent/\n.agents/\n");
        assert_eq!(std::fs::read(&exclude).unwrap(), expected);
    }

    #[test]
    fn ensure_worktree_reuses_across_a_new_commit_in_the_worktree() {
        let (repo, head) = tmp_repo();
        let wt_parent = tempfile::tempdir().unwrap();
        let wt = wt_parent.path().join("01HZTASK-a1");
        let branch = branch_name("01HZTASK", 1);
        ensure_worktree(repo.path(), &wt, &branch, &head).unwrap();

        std::fs::write(wt.join("c.txt"), "three\n").unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(&wt).output().unwrap();
            assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        };
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "thoth test"]);
        run(&["add", "c.txt"]);
        run(&["commit", "-m", "worker commit"]);
        let candidate = rev_parse_head(&wt).unwrap();
        assert_ne!(candidate, head, "the worker's commit must advance HEAD");

        // reroute / resume path: reuse, and do not reset the worker's commit
        assert_eq!(
            ensure_worktree(repo.path(), &wt, &branch, &head).unwrap(),
            EnsureOutcome::Reused
        );
        assert_eq!(rev_parse_head(&wt).unwrap(), candidate);
    }

    #[test]
    fn a_second_worktree_on_the_same_branch_is_a_hard_error() {
        // Phase 0 03-process-and-worktree.md:235-242 - git exits 128 with "already used by worktree"
        let (repo, head) = tmp_repo();
        let p1 = tempfile::tempdir().unwrap();
        let p2 = tempfile::tempdir().unwrap();
        let branch = branch_name("01HZTASK", 1);
        ensure_worktree(repo.path(), &p1.path().join("a1"), &branch, &head).unwrap();
        let err = ensure_worktree(repo.path(), &p2.path().join("dup"), &branch, &head)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("already used by worktree"),
            "expected git's fatal message, got: {err}"
        );
    }

    #[test]
    fn list_worktrees_includes_the_main_tree_and_the_new_one() {
        let (repo, head) = tmp_repo();
        let p = tempfile::tempdir().unwrap();
        let wt = p.path().join("a1");
        ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
        let list = list_worktrees(repo.path()).unwrap();
        assert!(list.len() >= 2, "got {list:?}");
        let canon_wt = std::fs::canonicalize(&wt).unwrap();
        assert!(
            list.iter().any(|p| std::fs::canonicalize(p).ok() == Some(canon_wt.clone())),
            "new worktree missing from {list:?}"
        );
    }

    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        Command::new("git").args(args).current_dir(dir).status().unwrap().success()
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
        assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// WT-1 AC1 (5): the sweep's remove drops the directory and git's
    /// registration, and nothing else - the branch and its commit stay.
    #[test]
    fn remove_worktree_force_drops_the_directory_but_keeps_the_branch() {
        let (repo, head) = tmp_repo();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("01HZTASK-a1");
        ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
        std::fs::write(wt.join("c.txt"), "three\n").unwrap();
        git_out(&wt, &["config", "user.email", "test@example.com"]);
        git_out(&wt, &["config", "user.name", "thoth test"]);
        git_out(&wt, &["add", "c.txt"]);
        git_out(&wt, &["commit", "-m", "worker commit"]);
        let candidate = rev_parse_head(&wt).unwrap();
        let canon_wt = std::fs::canonicalize(&wt).unwrap();

        remove_worktree_force(repo.path(), &wt).unwrap();

        assert!(!wt.exists(), "{} is still there", wt.display());
        let list = list_worktrees(repo.path()).unwrap();
        assert!(
            !list.iter().any(|p| std::fs::canonicalize(p).ok() == Some(canon_wt.clone())
                || p.file_name() == wt.file_name()),
            "removed worktree still listed in {list:?}"
        );
        assert!(git_ok(
            repo.path(),
            &["show-ref", "--verify", "--quiet", "refs/heads/thoth/01HZTASK/a1"]
        ));
        assert_eq!(
            git_out(repo.path(), &["rev-parse", "refs/heads/thoth/01HZTASK/a1"]),
            candidate
        );
        assert!(git_ok(repo.path(), &["cat-file", "-e", &format!("{candidate}^{{commit}}")]));

        let err = remove_worktree_force(repo.path(), &holder.path().join("not-there"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("git worktree remove failed"), "{err}");
    }

    /// A directory link at `link` to `target`: a junction on Windows (no
    /// privilege needed), a symlink elsewhere.
    fn dir_link(target: &Path, link: &Path) {
        #[cfg(windows)]
        {
            let out = Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(link)
                .arg(target)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "mklink /J: {}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).unwrap();
        assert!(std::fs::symlink_metadata(link).unwrap().file_type().is_symlink());
    }

    /// An outside directory holding a marker file and a subdirectory.
    fn outside_target() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("inner")).unwrap();
        std::fs::write(dir.path().join("inner").join("deep.txt"), "deep\n").unwrap();
        let marker = dir.path().join("marker.txt");
        std::fs::write(&marker, "precious contents\n").unwrap();
        (dir, marker)
    }

    fn assert_target_intact(dir: &Path, marker: &Path) {
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "precious contents\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("inner").join("deep.txt")).unwrap(),
            "deep\n"
        );
    }

    /// A worktree with a link at its top (`node_modules`) and one two
    /// directories down, each to its own outside directory.
    fn worktree_with_nested_links() -> (
        tempfile::TempDir,
        tempfile::TempDir,
        PathBuf,
        [(tempfile::TempDir, PathBuf); 2],
    ) {
        let (repo, head) = tmp_repo();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("01HZTASK-a1");
        ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
        let top = outside_target();
        let nested = outside_target();
        dir_link(top.0.path(), &wt.join("node_modules"));
        std::fs::create_dir_all(wt.join("pkg").join("deep")).unwrap();
        dir_link(nested.0.path(), &wt.join("pkg").join("deep").join("vendor"));
        (repo, holder, wt, [top, nested])
    }

    /// Links removal: both public removal functions drop nested directory
    /// links as links, then the worktree, and leave every target as it was.
    #[test]
    fn both_removal_functions_remove_nested_dir_links_without_touching_their_targets() {
        let (repo, _holder, wt, targets) = worktree_with_nested_links();
        remove_worktree_force(repo.path(), &wt).unwrap();
        assert!(!wt.exists(), "{} is still there", wt.display());
        for (dir, marker) in &targets {
            assert_target_intact(dir.path(), marker);
        }

        let (repo, _holder, wt, targets) = worktree_with_nested_links();
        remove_worktree(repo.path(), &wt).unwrap();
        assert!(!wt.exists(), "{} is still there", wt.display());
        for (dir, marker) in &targets {
            assert_target_intact(dir.path(), marker);
        }
    }

    /// Makes removing `link` fail until the guard is dropped. Windows: a
    /// handle on the link itself that does not share delete access. Unix: a
    /// read-only parent directory (`None` as root, which ignores it).
    #[cfg(windows)]
    fn block_link_removal(link: &Path) -> Option<std::fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_SHARE_READ: u32 = 0x1;
        const FILE_SHARE_WRITE: u32 = 0x2;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        Some(
            std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                .open(link)
                .unwrap(),
        )
    }

    /// Gives a directory whose permissions a test took away its `0o755` back
    /// when dropped.
    #[cfg(unix)]
    struct RestoreMode(PathBuf);

    #[cfg(unix)]
    impl Drop for RestoreMode {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        }
    }

    #[cfg(unix)]
    fn block_link_removal(link: &Path) -> Option<RestoreMode> {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return None;
        }
        let parent = link.parent().unwrap().to_path_buf();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o555)).unwrap();
        Some(RestoreMode(parent))
    }

    /// Links removal: when a link cannot be removed, neither function runs
    /// git or a recursive delete - the worktree, its files and its
    /// registration stay, and so does the target - and the error names the
    /// link. Once the link can go, the removal goes through.
    #[test]
    fn a_link_that_cannot_be_removed_leaves_the_worktree_and_its_target() {
        let (repo, _holder, wt, targets) = worktree_with_nested_links();
        let link = wt.join("pkg").join("deep").join("vendor");
        let Some(guard) = block_link_removal(&link) else {
            return;
        };
        let canon_wt = std::fs::canonicalize(&wt).unwrap();
        let still_registered = || {
            list_worktrees(repo.path())
                .unwrap()
                .iter()
                .any(|p| std::fs::canonicalize(p).ok() == Some(canon_wt.clone()))
        };

        let err = remove_worktree_force(repo.path(), &wt).unwrap_err();
        let link_err = err
            .downcast_ref::<LinkCleanupError>()
            .unwrap_or_else(|| panic!("not a LinkCleanupError: {err:#}"));
        assert_eq!(link_err.path, link);
        assert!(link_err.detail().contains("vendor"), "{}", link_err.detail());
        assert!(err.to_string().contains(LINK_UNREMOVABLE), "{err}");
        assert!(wt.join("a.txt").is_file(), "git or a recursive delete ran");
        assert!(wt.join(".git").exists());
        assert!(still_registered());

        let err = remove_worktree(repo.path(), &wt).unwrap_err();
        assert_eq!(err.path, link);
        assert!(wt.join("a.txt").is_file(), "the recursive fallback ran");
        assert!(wt.join("b.txt").is_file());
        assert!(still_registered());
        for (dir, marker) in &targets {
            assert_target_intact(dir.path(), marker);
        }

        drop(guard);
        remove_worktree_force(repo.path(), &wt).unwrap();
        assert!(!wt.exists());
        assert!(!still_registered());
        for (dir, marker) in &targets {
            assert_target_intact(dir.path(), marker);
        }
    }

    /// A worktree holding `pkg/vendor`, a symlink to `inner` in an outside
    /// directory that has lost its search permission, so reading what the
    /// link points to fails with "permission denied" while the link itself
    /// could be unlinked.
    #[cfg(unix)]
    struct UnreadableLinkTarget {
        repo: tempfile::TempDir,
        _holder: tempfile::TempDir,
        wt: PathBuf,
        canon_wt: PathBuf,
        link: PathBuf,
        // Declared before `target`, so the permission comes back before the
        // outside directory is deleted.
        guard: Option<RestoreMode>,
        target: (tempfile::TempDir, PathBuf),
    }

    #[cfg(unix)]
    impl UnreadableLinkTarget {
        /// `None` as root, where a directory without search permission can
        /// still be read: the test has nothing to check there.
        fn new(test: &str) -> Option<Self> {
            use std::os::unix::fs::PermissionsExt;
            if unsafe { libc::geteuid() } == 0 {
                masked_eprintln!("{test}: skipped, running as root (permission checks do not apply)");
                return None;
            }
            let (repo, head) = tmp_repo();
            let holder = tempfile::tempdir().unwrap();
            let wt = holder.path().join("01HZTASK-a1");
            ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
            let canon_wt = std::fs::canonicalize(&wt).unwrap();
            let target = outside_target();
            std::fs::create_dir_all(wt.join("pkg")).unwrap();
            let link = wt.join("pkg").join("vendor");
            dir_link(&target.0.path().join("inner"), &link);
            std::fs::set_permissions(target.0.path(), std::fs::Permissions::from_mode(0o600))
                .unwrap();
            let guard = Some(RestoreMode(target.0.path().to_path_buf()));
            let e = std::fs::metadata(&link).unwrap_err();
            assert_eq!(e.kind(), std::io::ErrorKind::PermissionDenied, "precondition: {e}");
            Some(UnreadableLinkTarget { repo, _holder: holder, wt, canon_wt, link, guard, target })
        }

        fn registered(&self) -> bool {
            list_worktrees(self.repo.path())
                .unwrap()
                .iter()
                .any(|p| std::fs::canonicalize(p).ok() == Some(self.canon_wt.clone()))
        }

        /// Cleanup stopped at the link: the error names it and the failure,
        /// the link is still there, and neither git nor a recursive delete
        /// touched the worktree or its registration.
        fn assert_stopped_at_the_link(&self, err: &LinkCleanupError) {
            assert_eq!(err.path, self.link);
            let detail = err.detail();
            assert!(detail.contains(&self.link.display().to_string()), "{detail}");
            assert!(detail.contains("Permission denied"), "{detail}");
            assert!(
                std::fs::symlink_metadata(&self.link).unwrap().file_type().is_symlink(),
                "the link was removed"
            );
            assert!(self.wt.join("a.txt").is_file(), "git or a recursive delete ran");
            assert!(self.wt.join("b.txt").is_file(), "git or a recursive delete ran");
            assert!(self.wt.join(".git").exists());
            assert!(self.registered());
        }

        fn give_the_permission_back(&mut self) {
            drop(self.guard.take());
        }

        fn assert_removed_with_the_target_intact(&self) {
            assert!(!self.wt.exists(), "{} is still there", self.wt.display());
            assert!(!self.registered());
            assert_target_intact(self.target.0.path(), &self.target.1);
        }
    }

    /// Unix: a link whose target cannot be read for any reason but its
    /// absence might be a directory link, so `remove_worktree_force` stops
    /// before git and reports the link and the error; with the permission
    /// back the removal goes through.
    #[cfg(unix)]
    #[test]
    fn remove_worktree_force_stops_before_git_when_a_link_target_cannot_be_read() {
        let test = "remove_worktree_force_stops_before_git_when_a_link_target_cannot_be_read";
        let Some(mut s) = UnreadableLinkTarget::new(test) else {
            return;
        };
        let err = remove_worktree_force(s.repo.path(), &s.wt).unwrap_err();
        let link_err = err
            .downcast_ref::<LinkCleanupError>()
            .unwrap_or_else(|| panic!("not a LinkCleanupError: {err:#}"));
        s.assert_stopped_at_the_link(link_err);
        assert!(err.to_string().contains(LINK_UNREMOVABLE), "{err}");

        s.give_the_permission_back();
        remove_worktree_force(s.repo.path(), &s.wt).unwrap();
        s.assert_removed_with_the_target_intact();
    }

    /// Unix: the same for `remove_worktree`, which also must not reach its
    /// recursive-delete fallback.
    #[cfg(unix)]
    #[test]
    fn remove_worktree_stops_before_git_and_its_fallback_when_a_link_target_cannot_be_read() {
        let test = "remove_worktree_stops_before_git_and_its_fallback_when_a_link_target_cannot_be_read";
        let Some(mut s) = UnreadableLinkTarget::new(test) else {
            return;
        };
        let err = remove_worktree(s.repo.path(), &s.wt).unwrap_err();
        s.assert_stopped_at_the_link(&err);

        s.give_the_permission_back();
        remove_worktree(s.repo.path(), &s.wt).unwrap();
        s.assert_removed_with_the_target_intact();
    }

    /// Unix: a dangling symlink - its target missing, or a path through a
    /// file - is still removed as a link entry by both functions, before git.
    #[cfg(unix)]
    #[test]
    fn both_removal_functions_remove_dangling_links_as_link_entries() {
        for force in [true, false] {
            let (repo, head) = tmp_repo();
            let holder = tempfile::tempdir().unwrap();
            let wt = holder.path().join("01HZTASK-a1");
            ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
            let file = holder.path().join("a-file");
            std::fs::write(&file, "not a directory\n").unwrap();
            let missing = wt.join("node_modules");
            std::os::unix::fs::symlink(holder.path().join("missing"), &missing).unwrap();
            let through_a_file = wt.join("vendor");
            std::os::unix::fs::symlink(file.join("sub"), &through_a_file).unwrap();
            for link in [&missing, &through_a_file] {
                assert!(std::fs::metadata(link).is_err(), "precondition: {} dangles", link.display());
            }

            if force {
                remove_worktree_force(repo.path(), &wt).unwrap();
            } else {
                remove_worktree(repo.path(), &wt).unwrap();
            }
            assert!(!wt.exists(), "{} is still there", wt.display());
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "not a directory\n");
        }
        let (repo, head) = tmp_repo();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("01HZTASK-a1");
        ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
        std::os::unix::fs::symlink(holder.path().join("missing"), wt.join("node_modules"))
            .unwrap();
        assert_eq!(remove_dir_links(&wt), Ok(1), "removed as a link entry");
        assert!(std::fs::symlink_metadata(wt.join("node_modules")).is_err());
    }

    /// WT-1 AC1 (5): a worker worktree's HEAD is on its branch; a detached
    /// commit - in a review worktree or in the main tree - is on no branch.
    #[test]
    fn head_branch_and_branches_containing_tell_a_branch_tip_from_a_detached_commit() {
        let (repo, head) = tmp_repo();
        let holder = tempfile::tempdir().unwrap();
        let wt = holder.path().join("01HZTASK-a1");
        ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();
        assert_eq!(head_branch(&wt).unwrap(), Some("thoth/01HZTASK/a1".to_string()));
        let wt_head = rev_parse_head(&wt).unwrap();
        let containing = branches_containing(repo.path(), &wt_head).unwrap();
        assert!(containing.contains(&"thoth/01HZTASK/a1".to_string()), "{containing:?}");

        let review = holder.path().join("01HZTASK-a1-review");
        ensure_detached_worktree(repo.path(), &review, &wt_head).unwrap();
        assert_eq!(head_branch(&review).unwrap(), None);
        let containing = branches_containing(repo.path(), &wt_head).unwrap();
        assert!(!containing.is_empty());
        assert!(containing.contains(&"thoth/01HZTASK/a1".to_string()), "{containing:?}");

        git_out(&review, &["config", "user.email", "test@example.com"]);
        git_out(&review, &["config", "user.name", "thoth test"]);
        git_out(&review, &["commit", "--allow-empty", "-m", "detached"]);
        let detached = rev_parse_head(&review).unwrap();
        assert_eq!(branches_containing(repo.path(), &detached).unwrap(), Vec::<String>::new());

        git_out(repo.path(), &["checkout", "--detach"]);
        git_out(repo.path(), &["commit", "--allow-empty", "-m", "main-detached"]);
        let main_detached = rev_parse_head(repo.path()).unwrap();
        assert_eq!(
            branches_containing(repo.path(), &main_detached).unwrap(),
            Vec::<String>::new()
        );
    }

    /// BASE-1 AC1: what `task create --base` accepts and refuses.
    #[test]
    fn resolve_commit_gives_the_full_sha_of_a_commit_and_refuses_anything_else() {
        let (repo, head) = tmp_repo();
        let r = repo.path();
        let first = git_out(r, &["rev-parse", "HEAD~1"]);
        git_out(r, &["branch", "long", &first]);
        git_out(r, &["tag", "-a", "v1", "-m", "annotated", &first]);

        assert_eq!(resolve_commit(r, "HEAD").unwrap(), head);
        assert_eq!(resolve_commit(r, "  main\n").unwrap(), head);
        assert_eq!(resolve_commit(r, "long").unwrap(), first);
        assert_eq!(resolve_commit(r, &first[..7]).unwrap(), first);
        assert_eq!(resolve_commit(r, "v1").unwrap(), first, "an annotated tag peels to its commit");
        assert_eq!(head.len(), 40);

        let tree = git_out(r, &["rev-parse", "HEAD^{tree}"]);
        let blob = git_out(r, &["rev-parse", "HEAD:a.txt"]);
        for bad in ["nosuch", "-x", "--all", "", "  ", tree.as_str(), blob.as_str()] {
            assert!(resolve_commit(r, bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn repo_root_from_a_non_repo_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        assert!(repo_root_from(dir.path()).is_err());
    }

    /// A repo whose second commit touches a test file, which is what the
    /// `touches_tests` heuristic exists to notice.
    fn tmp_repo_with_a_test_file() -> (tempfile::TempDir, String, String) {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let run = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(r).output().unwrap();
            assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "thoth test"]);
        std::fs::write(r.join("a.txt"), "one\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-m", "first"]);
        let base = rev_parse_head(r).unwrap();

        std::fs::create_dir_all(r.join("tests")).unwrap();
        std::fs::write(r.join("tests").join("thing_test.rs"), "#[test]\nfn t() {}\n").unwrap();
        std::fs::write(r.join("a.txt"), "one\ntwo\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "second: add a test"]);
        let head = rev_parse_head(r).unwrap();
        (dir, base, head)
    }

    #[test]
    fn diff_helpers_report_the_stat_the_files_and_the_patch() {
        let (repo, base, head) = tmp_repo_with_a_test_file();
        let stat = diff_stat(repo.path(), &base, &head).unwrap();
        assert!(stat.contains("a.txt"), "got {stat}");
        assert!(stat.contains("thing_test.rs"), "got {stat}");
        assert!(stat.contains("2 files changed"), "got {stat}");

        let mut files = diff_name_only(repo.path(), &base, &head).unwrap();
        files.sort();
        assert_eq!(
            files,
            vec!["a.txt".to_string(), "tests/thing_test.rs".to_string()],
            "git prints repo-relative paths with forward slashes"
        );

        let patch = String::from_utf8(diff_patch(repo.path(), &base, &head).unwrap()).unwrap();
        assert!(patch.contains("+two"), "the patch body must be there: {patch}");
        assert!(patch.contains("+fn t() {}"), "including the test file: {patch}");
        assert!(patch.contains("2 files changed"), "and the stat block");
    }

    /// SMALL-9c: excluded paths are matched literally and from the top of
    /// the tree. The paths are written straight into the index, since `*`
    /// and `:` cannot be file names on Windows.
    #[test]
    fn excluding_helpers_match_each_path_literally() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let git = |args: &[&str], input: Option<&[u8]>| -> String {
            let mut child = Command::new("git")
                .args(["-c", "core.protectNTFS=false"])
                .args(args)
                .current_dir(r)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut stdin = child.stdin.take().unwrap();
            if let Some(input) = input {
                stdin.write_all(input).unwrap();
            }
            drop(stdin);
            let out = child.wait_with_output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "--initial-branch=main"], None);
        git(&["config", "user.email", "test@example.com"], None);
        git(&["config", "user.name", "thoth test"], None);
        git(&["commit", "--allow-empty", "-q", "-m", "base"], None);
        let base = git(&["rev-parse", "HEAD"], None);
        let blob = git(&["hash-object", "-w", "--stdin"], Some(b"x\n"));
        let paths = ["s*.rs", "sa.rs", ":c.rs", "src/a [b].rs", "src/a b.rs", "src/ab.rs"];
        for p in paths {
            git(&["update-index", "--add", "--cacheinfo", &format!("100644,{blob},{p}")], None);
        }
        git(&["commit", "-q", "-m", "paths"], None);
        let head = git(&["rev-parse", "HEAD"], None);

        let exclude: Vec<String> = ["s*.rs", ":c.rs", "src/a [b].rs"].iter().map(|s| s.to_string()).collect();
        let mut files = diff_name_only_excluding(r, &base, &head, &exclude).unwrap();
        files.sort();
        assert_eq!(files, ["sa.rs", "src/a b.rs", "src/ab.rs"]);
        let stat = diff_stat_excluding(r, &base, &head, &exclude).unwrap();
        assert!(stat.contains("3 files changed"), "{stat}");
        let patch = String::from_utf8(diff_patch_excluding(r, &base, &head, &exclude).unwrap()).unwrap();
        assert!(!patch.contains("s*.rs") && !patch.contains(":c.rs") && !patch.contains("[b]"), "{patch}");
        assert!(patch.contains("src/a b.rs"), "{patch}");
        // from a subdirectory the paths are still read from the top
        std::fs::create_dir_all(r.join("src")).unwrap();
        let mut in_sub = diff_name_only_excluding(&r.join("src"), &base, &head, &exclude).unwrap();
        in_sub.sort();
        assert_eq!(in_sub, files);
        // nothing excluded is the unfiltered command, byte for byte
        assert_eq!(diff_patch_excluding(r, &base, &head, &[]).unwrap(), diff_patch(r, &base, &head).unwrap());
        // everything excluded leaves nothing
        let all: Vec<String> = paths.iter().map(|s| s.to_string()).collect();
        assert!(diff_name_only_excluding(r, &base, &head, &all).unwrap().is_empty());
        assert!(diff_stat_excluding(r, &base, &head, &all).unwrap().is_empty());
        assert!(diff_patch_excluding(r, &base, &head, &all).unwrap().is_empty());
        // changed_paths sees every path; merge_base finds the base
        assert_eq!(changed_paths(r, &base, &head).unwrap().len(), paths.len());
        assert_eq!(merge_base(r, &base, &head).unwrap().as_deref(), Some(base.as_str()));
    }

    #[test]
    fn commit_helpers_report_the_subject_the_date_and_the_stat() {
        let (repo, _base, head) = tmp_repo_with_a_test_file();
        let (subject, date) = commit_meta(repo.path(), &head).unwrap();
        assert_eq!(subject, "second: add a test");
        assert!(date.starts_with("20"), "an RFC3339 author date, got {date}");
        assert!(date.contains('T'), "got {date}");

        let stat = String::from_utf8(show_stat(repo.path(), &head).unwrap()).unwrap();
        assert!(stat.starts_with(&head), "the first --format line is the full sha: {stat}");
        assert!(stat.contains("second: add a test"));
        assert!(stat.contains("2 files changed"), "`-s` would suppress this: {stat}");
    }

    #[test]
    fn a_bad_revision_is_an_error_not_a_panic() {
        let (repo, base, _head) = tmp_repo_with_a_test_file();
        assert!(diff_stat(repo.path(), &base, "not-a-commit").is_err());
        assert!(commit_meta(repo.path(), "not-a-commit").is_err());
    }

    /// Open question 5: a worker (and, in Phase 2, a reviewer running
    /// `thoth evidence show`) executes from inside a linked worktree.
    /// `git rev-parse --show-toplevel` answers with that worktree's own root
    /// there, which would hash to a different `repo_id` and make every command
    /// report "repo ... is not initialised".
    #[test]
    fn repo_root_from_inside_a_linked_worktree_resolves_the_main_root() {
        let (repo, head) = tmp_repo();
        let p = tempfile::tempdir().unwrap();
        let wt = p.path().join("a1");
        ensure_worktree(repo.path(), &wt, &branch_name("01HZTASK", 1), &head).unwrap();

        let from_main = repo_root_from(repo.path()).unwrap();
        let from_worktree = repo_root_from(&wt).unwrap();
        assert_eq!(
            from_worktree, from_main,
            "a worker inside its worktree must resolve the same repo as its Supervisor"
        );
        assert_eq!(
            std::fs::canonicalize(&from_worktree).unwrap(),
            std::fs::canonicalize(repo.path()).unwrap()
        );

        // and a subdirectory of the worktree resolves the same way
        std::fs::create_dir_all(wt.join("deep").join("nested")).unwrap();
        assert_eq!(
            repo_root_from(&wt.join("deep").join("nested")).unwrap(),
            from_main
        );
    }

    /// brief §1/§15: the reviewer works in a detached checkout of the candidate
    /// commit, so nothing it could do moves a ref the implementer owns, and it
    /// can never see the implementer's uncommitted files.
    #[test]
    fn ensure_detached_worktree_checks_out_the_commit_with_no_branch() {
        let (repo, head) = tmp_repo();
        let p = tempfile::tempdir().unwrap();
        let wt = p.path().join("01HZTASK-a1-review");

        assert_eq!(
            ensure_detached_worktree(repo.path(), &wt, &head).unwrap(),
            EnsureOutcome::Created
        );
        assert!(wt.join("a.txt").is_file());
        assert_eq!(rev_parse_head(&wt).unwrap(), head);

        // HEAD must not be a branch
        let sym = Command::new("git")
            .args(["symbolic-ref", "-q", "HEAD"])
            .current_dir(&wt)
            .status()
            .unwrap();
        assert!(!sym.success(), "HEAD must be detached, not a branch");

        // a second call reuses rather than failing
        assert_eq!(
            ensure_detached_worktree(repo.path(), &wt, &head).unwrap(),
            EnsureOutcome::Reused
        );

        // two detached worktrees on the same commit are fine, unlike two
        // worktrees on one branch
        let other = p.path().join("second-review");
        assert_eq!(
            ensure_detached_worktree(repo.path(), &other, &head).unwrap(),
            EnsureOutcome::Created
        );

        // and an unknown commit is a hard error carrying git's message
        let bad = p.path().join("nope");
        assert!(ensure_detached_worktree(repo.path(), &bad, "not-a-commit").is_err());
    }

    #[test]
    fn repo_root_from_returns_a_native_path_with_no_verbatim_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(r)
                .output()
                .expect("git must be on PATH");
            assert!(out.status.success(), "git {args:?} failed: {:?}", out);
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "worktree test"]);
        std::fs::write(r.join("a.txt"), "one\n").unwrap();
        run(&["add", "a.txt"]);
        run(&["commit", "-m", "first"]);

        let root = repo_root_from(r).unwrap();
        let s = root.to_string_lossy();
        assert!(!s.starts_with(r"\\?\"), "verbatim prefix leaked: {s}");
        #[cfg(windows)]
        assert!(!s.contains('/'), "path is not native-separator form: {s}");
        assert!(root.is_dir(), "resolved root must still exist: {s}");
    }

    // scope §2.6 (E9): codex's sandbox cannot write a linked worktree's own
    // gitdir, so a codex worker cannot commit; thoth commits on its behalf.
    // `ENV_LOCK` guards the tests below that temporarily point `HOME` at an
    // empty directory to simulate a machine with no git identity configured -
    // every other test in this module sets an explicit repo-local identity,
    // which always outranks a missing global one, so this is safe to run
    // alongside them.
    fn without_git_identity<T>(f: impl FnOnce() -> T) -> T {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("HOME");
        let empty_home = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", empty_home.path());
        let result = f();
        match prev {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }
        result
    }

    #[test]
    fn commit_worker_changes_commits_new_files_under_the_repos_own_identity() {
        let (repo, head) = tmp_repo();
        std::fs::write(repo.path().join("c.txt"), "three\n").unwrap();

        let candidate = commit_worker_changes(repo.path(), "task title (thoth run 01RUN)")
            .unwrap()
            .expect("there were changes to commit");
        assert_ne!(candidate, head);
        assert_eq!(rev_parse_head(repo.path()).unwrap(), candidate);
        let (subject, _date) = commit_meta(repo.path(), &candidate).unwrap();
        assert_eq!(subject, "task title (thoth run 01RUN)");
        let author = git_capture(repo.path(), &["show", "-s", "--format=%an <%ae>", &candidate]).unwrap();
        assert_eq!(author.trim(), "thoth test <test@example.com>");
    }

    #[test]
    fn commit_worker_changes_does_nothing_when_the_worktree_is_clean() {
        let (repo, head) = tmp_repo();
        let result = commit_worker_changes(repo.path(), "should not be used").unwrap();
        assert_eq!(result, None);
        assert_eq!(rev_parse_head(repo.path()).unwrap(), head);
    }

    #[test]
    fn commit_worker_changes_falls_back_to_a_fixed_identity_when_none_is_configured() {
        without_git_identity(|| {
            let dir = tempfile::tempdir().unwrap();
            let r = dir.path();
            let run = |args: &[&str]| {
                let out = Command::new("git").args(args).current_dir(r).output().unwrap();
                assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
            };
            run(&["init", "--initial-branch=main"]);
            // the seed commit needs an explicit identity since none is
            // configured anywhere in this isolated HOME
            run(&[
                "-c", "user.name=seed", "-c", "user.email=seed@example.com",
                "commit", "--allow-empty", "-m", "seed",
            ]);
            std::fs::write(r.join("a.txt"), "one\n").unwrap();

            let candidate = commit_worker_changes(r, "no identity here").unwrap().unwrap();
            let author = git_capture(r, &["show", "-s", "--format=%an <%ae>", &candidate]).unwrap();
            assert_eq!(author.trim(), "thoth <thoth@localhost>");
        });
    }

    #[test]
    fn commit_worker_changes_commits_only_whats_left_when_the_worker_already_committed() {
        let (repo, head) = tmp_repo();
        std::fs::write(repo.path().join("worker.txt"), "worker\n").unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(repo.path()).output().unwrap();
            assert!(out.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&out.stderr));
        };
        run(&["add", "worker.txt"]);
        run(&["commit", "-m", "worker's own commit"]);
        let worker_commit = rev_parse_head(repo.path()).unwrap();
        assert_ne!(worker_commit, head);

        // nothing left uncommitted: thoth's call is a no-op
        let result = commit_worker_changes(repo.path(), "unused").unwrap();
        assert_eq!(result, None);
        assert_eq!(rev_parse_head(repo.path()).unwrap(), worker_commit);

        // the worker left something extra on top of its own commit
        std::fs::write(repo.path().join("leftover.txt"), "leftover\n").unwrap();
        let candidate = commit_worker_changes(repo.path(), "thoth finishing up")
            .unwrap()
            .expect("the leftover file needed a commit");
        assert_ne!(candidate, worker_commit);
        let files = diff_name_only(repo.path(), &worker_commit, &candidate).unwrap();
        assert_eq!(files, vec!["leftover.txt".to_string()]);
    }
}
