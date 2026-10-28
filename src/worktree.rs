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

/// `git worktree remove --force PATH`, and nothing else: no fallback to
/// deleting the directory. The worktree sweep has already decided the
/// directory is safe to drop; when git still refuses (a file locked on
/// Windows), the caller keeps the directory and reports git's message.
/// The branch is left alone - `git worktree remove` never deletes one.
pub fn remove_worktree_force(repo_root: &Path, path: &Path) -> Result<()> {
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
fn has_configured_identity(dir: &Path) -> bool {
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
/// falls back to deleting the directory and pruning git's bookkeeping.
pub fn remove_worktree(repo_root: &Path, path: &Path) {
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
    bytes
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| String::from_utf8_lossy(p).to_string())
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
