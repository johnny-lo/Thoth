//! RFIX-1: the small fix a code reviewer may attach to a REJECT (`review
//! start --edit`), and the mechanical bounds Thoth holds it to before it is
//! committed for the confirmer (`review confirm`).
//!
//! The reviewer stays read-only: it lists its edits in `review.json`
//! (`edits`), and Thoth applies them in the fix worktree, measures the result
//! against the candidate with git, and either commits it on the fix branch or
//! refuses it. Thoth never judges what an edit means; that is the
//! confirmer's job. Everything here works on a directory and git, never on
//! the database.

use crate::review_json::Findings;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The most changed lines (added plus deleted, against the candidate) a fix
/// may have. The prompt, the docs and the bounds check all read this.
pub const MAX_CHANGED_LINES: u64 = 40;
/// The most files a fix may touch.
pub const MAX_FILES: usize = 3;

/// The confirmer's input, in the fix worktree.
pub const INPUT_DIR: &str = ".agent/review-fix-input";
pub const INPUT_SPEC: &str = "spec.json";
pub const INPUT_FINDINGS: &str = "findings.json";
pub const INPUT_EDITS: &str = "edits.json";
pub const INPUT_PATCH: &str = "fix.patch";

/// The stages a refused fix names, in the order they are checked.
pub mod stage {
    pub const WORKTREE: &str = "worktree";
    pub const EDITS: &str = "edits";
    pub const APPLY: &str = "apply";
    pub const SIZE: &str = "size";
    pub const BOUNDS: &str = "bounds";
    pub const SPEC_FILE: &str = "spec_file";
    pub const TESTS: &str = "tests";
    pub const PROTECTED: &str = "protected";
    pub const WRITE: &str = "write";
}

/// One edit as the reviewer wrote it: every key there and no other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixEdit {
    pub path: String,
    pub old: String,
    pub new: String,
    pub criterion: String,
    pub reason: String,
}

/// Why a fix was not applied: the stage and a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub stage: &'static str,
    pub reason: String,
}

impl Refusal {
    pub fn new(stage: &'static str, reason: impl Into<String>) -> Self {
        Refusal { stage, reason: reason.into() }
    }
}

/// What an edit is bound to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Binding {
    /// An acceptance criterion the review judged unmet.
    Criterion(String),
    /// `extra_findings[i]`.
    Extra(usize),
}

/// An edit that passed the field and binding rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bound {
    pub edit: FixEdit,
    pub binding: Binding,
    /// The bound finding's `fix_size` and `category`, as stored.
    pub fix_size: Option<String>,
    pub category: Option<String>,
}

/// The `edits` of an edit-mode document: an array, possibly empty. `Err`
/// says why the document is not one (it is then invalid, not refused).
pub fn edit_list(doc: &Value) -> std::result::Result<Vec<Value>, String> {
    match doc.get("edits") {
        None => Err("review.json has no \"edits\": an edit-mode review always carries it, [] when it attaches no fix".into()),
        Some(Value::Null) => Err("review.json \"edits\" is null; it must be an array, [] when no fix is attached".into()),
        Some(Value::Array(a)) => Ok(a.clone()),
        Some(_) => Err("review.json \"edits\" is not an array".into()),
    }
}

/// Windows device names, refused as any segment's stem on every platform.
const DEVICE_NAMES: [&str; 24] = [
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9", "conin$",
    "conout$",
];

/// The first segments a fix may never write under (compared after
/// lower-casing and dropping trailing dots and spaces, which Windows drops
/// too).
const RESERVED_TOP: [&str; 3] = [".agent", ".agents", ".git"];

/// The text rules of an edit's `path`, the same on both platforms: a
/// non-empty relative path with `/` separators only, no empty, `.` or `..`
/// segment, no backslash, colon or NUL, nothing under `.agent/`, `.agents/`
/// or `.git/`, no segment that is a Windows device name (with an extension
/// too) or ends in a dot or a space.
pub fn check_path(path: &str) -> std::result::Result<(), String> {
    if path.is_empty() {
        return Err("the path is empty".into());
    }
    if path.contains('\0') {
        return Err("the path contains NUL".into());
    }
    if path.contains('\\') {
        return Err("the path contains a backslash; use / between segments".into());
    }
    if path.contains(':') {
        return Err("the path contains a colon (a drive or a stream name)".into());
    }
    if path.starts_with('/') {
        return Err("the path is absolute; give it relative to the repository root".into());
    }
    let segments: Vec<&str> = path.split('/').collect();
    for s in &segments {
        if s.is_empty() {
            return Err("the path has an empty segment".into());
        }
        if *s == "." || *s == ".." {
            return Err(format!("the path has a '{s}' segment"));
        }
        if s.ends_with('.') || s.ends_with(' ') {
            return Err(format!("the segment '{s}' ends in a dot or a space"));
        }
        let stem = s.split('.').next().unwrap_or("").trim_end().to_ascii_lowercase();
        if DEVICE_NAMES.contains(&stem.as_str()) {
            return Err(format!("the segment '{s}' is a Windows device name"));
        }
    }
    let top = segments[0].trim_end_matches(['.', ' ']).to_ascii_lowercase();
    if RESERVED_TOP.contains(&top.as_str()) {
        return Err(format!("the path is under {}/, which a fix may not touch", segments[0]));
    }
    Ok(())
}

/// The file-system half of the path rules: the file and every parent that
/// exists is not a symlink or a junction, and the path resolves inside
/// `root`.
pub fn check_fs_path(root: &Path, path: &str) -> std::result::Result<(), String> {
    let mut at = root.to_path_buf();
    for segment in path.split('/') {
        at.push(segment);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{} is a symlink or a junction", rel(root, &at)));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => return Err(format!("reading {} failed: {e}", rel(root, &at))),
        }
    }
    let real_root = std::fs::canonicalize(root).map_err(|e| format!("resolving the fix worktree failed: {e}"))?;
    // The deepest part that exists, resolved; the rest is plain segments.
    let mut probe = root.join(path);
    while !probe.exists() {
        match probe.parent() {
            Some(p) if p.starts_with(root) => probe = p.to_path_buf(),
            _ => break,
        }
    }
    let real = std::fs::canonicalize(&probe).map_err(|e| format!("resolving {path} failed: {e}"))?;
    if !real.starts_with(&real_root) {
        return Err(format!("{path} resolves outside the fix worktree"));
    }
    Ok(())
}

fn rel(root: &Path, at: &Path) -> String {
    at.strip_prefix(root).unwrap_or(at).display().to_string().replace('\\', "/")
}

/// The field and binding rules for every edit, in order, before anything is
/// applied: the strict shape, `path` (`check_path`, then `check_fs_path`
/// under `root`), a create (`old` empty) with a non-empty `new`, a
/// non-blank `reason`, and a `criterion` that names an acceptance criterion
/// the review judged unmet or `extra_findings[i]` of this review.
pub fn validate(root: &Path, items: &[Value], findings: &Findings) -> std::result::Result<Vec<Bound>, Refusal> {
    let refuse = |i: usize, why: String| Refusal::new(stage::EDITS, format!("edit {i}: {why}"));
    let mut out = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let edit: FixEdit = serde_json::from_value(item.clone()).map_err(|e| {
            refuse(i, format!("not the edit shape (path, old, new, criterion, reason, all strings): {e}"))
        })?;
        check_path(&edit.path).map_err(|e| refuse(i, e))?;
        check_fs_path(root, &edit.path).map_err(|e| refuse(i, e))?;
        if edit.old.is_empty() && edit.new.is_empty() {
            return Err(refuse(i, "old and new are both empty; a new file needs content".into()));
        }
        if edit.reason.trim().is_empty() {
            return Err(refuse(i, "reason is blank".into()));
        }
        let (binding, fix_size, category) = bind(&edit.criterion, findings).map_err(|e| refuse(i, e))?;
        out.push(Bound { edit, binding, fix_size, category });
    }
    Ok(out)
}

/// What `criterion` names: an unmet criterion of `findings`, or
/// `extra_findings[i]`.
fn bind(criterion: &str, findings: &Findings) -> std::result::Result<(Binding, Option<String>, Option<String>), String> {
    let c = criterion.trim();
    if let Some(rest) = c.strip_prefix("extra_findings[").and_then(|r| r.strip_suffix(']')) {
        let i: usize = rest.parse().map_err(|_| format!("criterion '{criterion}' is not extra_findings[N]"))?;
        let Some(e) = findings.extra_findings.get(i) else {
            return Err(format!(
                "criterion '{criterion}' names an extra finding this review does not have ({} extra findings)",
                findings.extra_findings.len()
            ));
        };
        return Ok((Binding::Extra(i), e.fix_size.clone(), e.category.clone()));
    }
    match findings.criteria.iter().find(|f| f.id.trim() == c) {
        Some(f) if f.verdict.trim() == "unmet" => Ok((Binding::Criterion(c.to_string()), f.fix_size.clone(), f.category.clone())),
        Some(_) => Err(format!("criterion '{criterion}' is judged met; an edit fixes an unmet criterion or an extra finding")),
        None => Err(format!("criterion '{criterion}' is not an acceptance criterion of this review")),
    }
}

/// One applied edit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedOp {
    pub index: usize,
    pub path: String,
    /// `create` or `replace`.
    pub op: &'static str,
}

/// Applies `edits` to the files under `root`, in order, each one seeing the
/// earlier ones. `old` empty creates `path` (which must not exist) with
/// `new`; otherwise `old` must occur exactly once in the file. CRLF compares
/// as LF, `new` is turned into LF, and a file that had CRLF is written back
/// with CRLF. Only regular UTF-8 text files are touched. When any edit fails,
/// or the edits leave every file as it was, every file is put back and the
/// refusal (stage `apply`) is returned.
pub fn apply(root: &Path, edits: &[FixEdit]) -> std::result::Result<Vec<AppliedOp>, Refusal> {
    let mut originals: Vec<(PathBuf, Option<Vec<u8>>)> = Vec::new();
    let result = apply_inner(root, edits, &mut originals);
    let unchanged = result.is_ok()
        && originals.iter().all(|(p, before)| match before {
            Some(b) => std::fs::read(p).map(|now| now == *b).unwrap_or(false),
            None => false,
        });
    if result.is_err() || unchanged {
        for (p, before) in originals.iter().rev() {
            match before {
                Some(b) => {
                    let _ = std::fs::write(p, b);
                }
                None => {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }
    if unchanged {
        return Err(Refusal::new(stage::APPLY, "the edits leave every file as it was in the candidate"));
    }
    result
}

fn apply_inner(
    root: &Path,
    edits: &[FixEdit],
    originals: &mut Vec<(PathBuf, Option<Vec<u8>>)>,
) -> std::result::Result<Vec<AppliedOp>, Refusal> {
    let refuse = |i: usize, path: &str, why: String| Refusal::new(stage::APPLY, format!("edit {i} ({path}): {why}"));
    let mut out = Vec::new();
    for (i, e) in edits.iter().enumerate() {
        let file = root.join(&e.path);
        let meta = std::fs::symlink_metadata(&file);
        let remember = |originals: &mut Vec<(PathBuf, Option<Vec<u8>>)>, before: Option<Vec<u8>>| {
            if !originals.iter().any(|(p, _)| *p == file) {
                originals.push((file.clone(), before));
            }
        };
        if e.old.is_empty() {
            match meta {
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => return Err(refuse(i, &e.path, "old is empty (create), but the file already exists".into())),
                Err(err) => return Err(refuse(i, &e.path, format!("reading the file failed: {err}"))),
            }
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent).map_err(|err| refuse(i, &e.path, format!("creating its directory failed: {err}")))?;
            }
            remember(originals, None);
            std::fs::write(&file, e.new.replace("\r\n", "\n"))
                .map_err(|err| refuse(i, &e.path, format!("writing the file failed: {err}")))?;
            out.push(AppliedOp { index: i, path: e.path.clone(), op: "create" });
            continue;
        }
        match meta {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(refuse(i, &e.path, "is not a regular file".into())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Err(refuse(i, &e.path, "the file does not exist; old must be empty to create it".into()))
            }
            Err(err) => return Err(refuse(i, &e.path, format!("reading the file failed: {err}"))),
        }
        let bytes = std::fs::read(&file).map_err(|err| refuse(i, &e.path, format!("reading the file failed: {err}")))?;
        let text = String::from_utf8(bytes.clone()).map_err(|_| refuse(i, &e.path, "the file is not UTF-8 text".into()))?;
        let crlf = text.contains("\r\n");
        let lf = text.replace("\r\n", "\n");
        let old = e.old.replace("\r\n", "\n");
        let new = e.new.replace("\r\n", "\n");
        let found = lf.matches(old.as_str()).count();
        if found == 0 {
            return Err(refuse(i, &e.path, "old was not found in the file".into()));
        }
        if found > 1 {
            return Err(refuse(i, &e.path, format!("old occurs {found} times in the file; it must occur exactly once")));
        }
        let mut result = lf.replacen(old.as_str(), &new, 1);
        if crlf {
            result = result.replace('\n', "\r\n");
        }
        remember(originals, Some(bytes));
        std::fs::write(&file, result).map_err(|err| refuse(i, &e.path, format!("writing the file failed: {err}")))?;
        out.push(AppliedOp { index: i, path: e.path.clone(), op: "replace" });
    }
    Ok(out)
}

/// One file of a fix, against the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    pub path: String,
    /// `added`, `modified` or `deleted`.
    pub status: String,
    /// `None` for a binary file (numstat prints `-`).
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

impl FileStat {
    pub fn to_json(&self) -> Value {
        json!({"path": self.path, "status": self.status, "added": self.added, "deleted": self.deleted})
    }
}

/// A fix's size against the candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diff {
    pub files: Vec<FileStat>,
    pub changed_lines: u64,
    pub file_count: usize,
}

impl Diff {
    pub fn files_json(&self) -> Value {
        Value::Array(self.files.iter().map(FileStat::to_json).collect())
    }
}

/// git run in `dir` with the given extra environment; stdout bytes, or an
/// error with git's message.
fn git(dir: &Path, env: &[(&str, &Path)], args: &[&str]) -> Result<Vec<u8>> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    for (k, v) in env {
        cmd.env(k, v);
    }
    crate::process::quiet(&mut cmd);
    let out = cmd.output().with_context(|| format!("running git {args:?}"))?;
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

/// A temporary git index holding the fix worktree's whole result: the
/// candidate's tree plus every file on disk (untracked and ignored ones
/// too), Thoth's `.agent/` and `.agents/` left out. Removed on drop.
pub struct Staged {
    worktree: PathBuf,
    index: PathBuf,
    candidate: String,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.index);
    }
}

impl Staged {
    /// Stages the result of `worktree` against `candidate`, with the index
    /// file at `index` (in the run dir, outside the worktree).
    pub fn new(worktree: &Path, candidate: &str, index: &Path) -> Result<Staged> {
        let _ = std::fs::remove_file(index);
        let staged = Staged {
            worktree: worktree.to_path_buf(),
            index: index.to_path_buf(),
            candidate: candidate.to_string(),
        };
        staged.git(&["read-tree", candidate])?;
        staged.git(&["add", "-A", "-f", "--", ".", ":(exclude).agent", ":(exclude).agents"])?;
        Ok(staged)
    }

    fn git(&self, args: &[&str]) -> Result<Vec<u8>> {
        git(&self.worktree, &[("GIT_INDEX_FILE", &self.index)], args)
    }

    /// Every changed file with its numstat, git's own counting: a replaced
    /// line is one deleted and one added, a path is counted once with its
    /// net change.
    pub fn diff(&self) -> Result<Diff> {
        let numstat = self.git(&["diff", "--cached", "--numstat", "-z", "--no-renames", &self.candidate])?;
        let status = self.git(&["diff", "--cached", "--name-status", "-z", "--no-renames", &self.candidate])?;
        let mut statuses: Vec<(String, String)> = Vec::new();
        let mut parts = status.split(|b| *b == 0);
        while let (Some(letter), Some(path)) = (parts.next(), parts.next()) {
            if path.is_empty() {
                continue;
            }
            let s = match letter.first() {
                Some(b'A') => "added",
                Some(b'D') => "deleted",
                _ => "modified",
            };
            statuses.push((String::from_utf8_lossy(path).to_string(), s.to_string()));
        }
        let mut files = Vec::new();
        for entry in numstat.split(|b| *b == 0).filter(|e| !e.is_empty()) {
            let text = String::from_utf8_lossy(entry);
            let mut cols = text.splitn(3, '\t');
            let (Some(a), Some(d), Some(path)) = (cols.next(), cols.next(), cols.next()) else { continue };
            let status = statuses
                .iter()
                .find(|(p, _)| p == path)
                .map(|(_, s)| s.clone())
                .unwrap_or_else(|| "modified".into());
            files.push(FileStat {
                path: path.to_string(),
                status,
                added: a.parse().ok(),
                deleted: d.parse().ok(),
            });
        }
        let changed_lines = files.iter().map(|f| f.added.unwrap_or(0) + f.deleted.unwrap_or(0)).sum();
        let file_count = files.len();
        Ok(Diff { files, changed_lines, file_count })
    }

    /// The `-U0` diff of one path: its deleted and added lines, without the
    /// leading `-`/`+`.
    pub fn line_changes(&self, path: &str) -> Result<(Vec<String>, Vec<String>)> {
        let out = self.git(&[
            "diff", "--cached", "-U0", "--no-renames", "--no-color", "--no-ext-diff", "--no-textconv",
            &self.candidate, "--", path,
        ])?;
        let text = String::from_utf8_lossy(&out);
        let mut deleted = Vec::new();
        let mut added = Vec::new();
        let mut in_hunk = false;
        for line in text.lines() {
            if line.starts_with("@@") {
                in_hunk = true;
                continue;
            }
            if line.starts_with("diff --git") {
                in_hunk = false;
                continue;
            }
            if !in_hunk {
                continue;
            }
            if let Some(l) = line.strip_prefix('-') {
                deleted.push(l.to_string());
            } else if let Some(l) = line.strip_prefix('+') {
                added.push(l.to_string());
            }
        }
        Ok((deleted, added))
    }

    /// Commits the staged result on top of the candidate (no branch moves,
    /// nothing checked out) and returns the new commit's sha. The identity is
    /// the repository's own, or `thoth` when none is configured, like the
    /// commit Thoth makes for a codex worker.
    pub fn commit(&self, message: &str) -> Result<String> {
        let tree = String::from_utf8_lossy(&self.git(&["write-tree"])?).trim().to_string();
        let mut args: Vec<&str> = Vec::new();
        if !crate::worktree::has_configured_identity(&self.worktree) {
            args.extend(["-c", "user.name=thoth", "-c", "user.email=thoth@localhost"]);
        }
        args.extend(["commit-tree", tree.as_str(), "-p", self.candidate.as_str(), "-m", message]);
        Ok(String::from_utf8_lossy(&git(&self.worktree, &[], &args)?).trim().to_string())
    }
}

/// (b) `bounds`: no binary file, at most `MAX_CHANGED_LINES` changed lines in
/// at most `MAX_FILES` files.
pub fn check_bounds(diff: &Diff) -> std::result::Result<(), Refusal> {
    if let Some(f) = diff.files.iter().find(|f| f.added.is_none() || f.deleted.is_none()) {
        return Err(Refusal::new(stage::BOUNDS, format!("{} is binary to git; a fix touches text files only", f.path)));
    }
    if diff.file_count == 0 {
        return Err(Refusal::new(stage::APPLY, "the fix leaves the candidate unchanged"));
    }
    if diff.changed_lines > MAX_CHANGED_LINES || diff.file_count > MAX_FILES {
        return Err(Refusal::new(
            stage::BOUNDS,
            format!(
                "the fix changes {} line(s) in {} file(s); the limit is {MAX_CHANGED_LINES} lines in {MAX_FILES} files",
                diff.changed_lines, diff.file_count
            ),
        ));
    }
    Ok(())
}

/// (a) `size`: every bound finding is `S` or `M`, every unmet criterion has
/// an edit, and (GATE-1) so does every extra finding whose severity is not
/// `minor` (trimmed, any case; an empty or unknown one blocks).
pub fn check_size(bound: &[Bound], findings: &Findings) -> std::result::Result<(), Refusal> {
    for (i, b) in bound.iter().enumerate() {
        match b.fix_size.as_deref() {
            Some("S") | Some("M") => {}
            other => {
                return Err(Refusal::new(
                    stage::SIZE,
                    format!(
                        "edit {i} is bound to {} whose fix_size is {}; only S and M findings are fixed here, the rest is redone",
                        b.edit.criterion,
                        other.unwrap_or("not given")
                    ),
                ))
            }
        }
    }
    for c in findings.criteria.iter().filter(|c| c.verdict.trim() == "unmet") {
        if !bound.iter().any(|b| b.binding == Binding::Criterion(c.id.trim().to_string())) {
            return Err(Refusal::new(
                stage::SIZE,
                format!("unmet criterion {} has no edit; a fix covers every unmet criterion or the task is redone", c.id),
            ));
        }
    }
    for (i, e) in findings.extra_findings.iter().enumerate() {
        if e.severity.trim().eq_ignore_ascii_case("minor") || bound.iter().any(|b| b.binding == Binding::Extra(i)) {
            continue;
        }
        return Err(Refusal::new(
            stage::SIZE,
            format!(
                "extra_findings[{i}] (severity {}) has no edit; a fix covers every unmet criterion and every extra finding that is not minor, or the task is redone",
                e.severity.trim()
            ),
        ));
    }
    Ok(())
}

/// Whether `text` parses as a Thoth spec (what `spec lint` reports as
/// `invalid_spec` when it does not).
pub fn is_spec_text(text: &str) -> bool {
    crate::spec::parse_spec(text).is_ok()
}

/// (d) `tests`: an existing test file loses no line; no deleted line holds
/// `assert` (any case), `#[test]` or `#[cfg(test)]`; no added line holds
/// `#[ignore]`. `is_test` is the protected-file rule for test files.
pub fn check_tests(
    staged: &Staged,
    diff: &Diff,
    is_test: impl Fn(&str) -> bool,
) -> Result<std::result::Result<(), Refusal>> {
    for f in &diff.files {
        let (deleted, added) = staged.line_changes(&f.path)?;
        if f.status != "added" && is_test(&f.path) && !deleted.is_empty() {
            return Ok(Err(Refusal::new(
                stage::TESTS,
                format!(
                    "{} is an existing test file and the fix deletes {} line(s) of it; a fix may only add lines to an existing test",
                    f.path,
                    deleted.len()
                ),
            )));
        }
        if let Some(l) = deleted.iter().find(|l| {
            l.to_ascii_lowercase().contains("assert") || l.contains("#[test]") || l.contains("#[cfg(test)]")
        }) {
            return Ok(Err(Refusal::new(
                stage::TESTS,
                format!("the fix deletes a test line from {}: {}", f.path, l.trim()),
            )));
        }
        if let Some(l) = added.iter().find(|l| l.contains("#[ignore]")) {
            return Ok(Err(Refusal::new(
                stage::TESTS,
                format!("the fix adds #[ignore] to {}: {}", f.path, l.trim()),
            )));
        }
    }
    Ok(Ok(()))
}

/// `git status` entries of `dir` outside `.agent/` and `.agents/`, as
/// paths: tracked changes (staged or not) and untracked files, and with
/// `ignored` the ignored files too.
pub fn project_changes(dir: &Path, ignored: bool) -> Result<Vec<String>> {
    let mut args = vec!["status", "--porcelain=v1", "-z", "--untracked-files=all", "--no-renames"];
    if ignored {
        args.push("--ignored=traditional");
    }
    let out = git(dir, &[], &args)?;
    Ok(out
        .split(|b| *b == 0)
        .filter(|e| e.len() > 3)
        .map(|e| String::from_utf8_lossy(&e[3..]).to_string())
        .filter(|p| {
            let p = p.trim_end_matches('/');
            !(p == ".agent" || p == ".agents" || p.starts_with(".agent/") || p.starts_with(".agents/"))
        })
        .collect())
}

/// Puts the fix worktree back at `commit`: HEAD detached there, tracked
/// files as committed, every untracked and ignored file outside `.agent/`
/// and `.agents/` removed.
pub fn reset_worktree(dir: &Path, commit: &str) -> Result<()> {
    git(dir, &[], &["checkout", "--detach", "--force", commit])?;
    git(dir, &[], &["reset", "--hard", commit])?;
    git(dir, &[], &["clean", "-ffdx", "-e", "/.agent/", "-e", "/.agents/"])?;
    Ok(())
}

/// The sha `refs/heads/BRANCH` points at, or `None` when there is no such
/// branch.
pub fn branch_sha(root: &Path, branch: &str) -> Result<Option<String>> {
    let mut cmd = Command::new("git");
    cmd.args(["rev-parse", "--verify", "--quiet", &format!("refs/heads/{branch}")]).current_dir(root);
    crate::process::quiet(&mut cmd);
    let out = cmd.output()?;
    match out.status.code() {
        Some(0) => Ok(Some(String::from_utf8_lossy(&out.stdout).trim().to_string())),
        Some(1) => Ok(None),
        code => bail!("git rev-parse refs/heads/{branch} failed (exit {code:?}): {}", String::from_utf8_lossy(&out.stderr).trim()),
    }
}

/// Points `refs/heads/BRANCH` at `sha` (creating it), or deletes it with
/// `None`.
pub fn set_branch(root: &Path, branch: &str, sha: Option<&str>) -> Result<()> {
    let name = format!("refs/heads/{branch}");
    match sha {
        Some(s) => git(root, &[], &["update-ref", &name, s])?,
        None => git(root, &[], &["update-ref", "-d", &name])?,
    };
    Ok(())
}

/// The edit section of an edit-mode reviewer's prompt. `claude` is whether
/// the reviewer writes `.agent/review.json` itself.
pub fn edit_prompt_section(claude: bool) -> String {
    let own_file = if claude {
        "; the only file you write is .agent/review.json"
    } else {
        ""
    };
    format!(
        "\n## Attaching a fix (edit mode)\n\
         This review is in edit mode. Besides judging, you may attach a small fix to a REJECT in \"edits\"; thoth applies it to a copy of the candidate, the commander re-runs the checks on it, and another model confirms it. You still do not modify any project file yourself{own_file}.\n\
         \"edits\" is always present at the top level, next to \"verdict\" and \"findings\": [] when you attach nothing, and always [] with PASS or UNVERIFIED.\n\
         Each edit is {{\"path\":\"src/x.rs\",\"old\":\"text to replace\",\"new\":\"replacement\",\"criterion\":\"AC1\",\"reason\":\"why this fixes the finding\"}}:\n\
         - path: relative to the repository root, / between segments; not under .agent/, .agents/ or .git/.\n\
         - old: text that occurs exactly once in the file as the earlier edits left it; \"\" creates a new file whose whole content is new.\n\
         - criterion: the id of a criterion you judged unmet, or extra_findings[i] (i from 0) for one of your extra findings.\n\
         - reason: not blank.\n\
         Edits apply in order and each sees the earlier ones; CRLF and LF compare alike.\n\
         Limits, checked mechanically (one broken limit refuses the whole fix, your REJECT stands, and the task is redone from the candidate):\n\
         - every edit is bound to a finding whose fix_size is S or M, and every unmet criterion and every extra finding whose severity is not minor has at least one edit;\n\
         - at most {MAX_CHANGED_LINES} changed lines (added plus deleted against the candidate, a replaced line counts 2) in at most {MAX_FILES} files, text files only;\n\
         - no file that is a Thoth spec, no check input or check-word file;\n\
         - an existing test file may only gain lines; no deleted line holds assert, #[test] or #[cfg(test)], no added line holds #[ignore].\n\
         When the fix does not fit these limits, or would change what the spec means, leave \"edits\" as [] and only REJECT.\n"
    )
}

/// The JSON Schema of an edit-mode report for codex and agy: the review
/// schema plus `edits`, strict at every level.
pub fn edit_output_schema() -> Value {
    let mut schema = crate::review_json::output_schema();
    schema["required"] = json!(["verdict", "findings", "edits"]);
    schema["properties"]["edits"] = json!({"type": "array", "items": {
        "type": "object",
        "additionalProperties": false,
        "required": ["path", "old", "new", "criterion", "reason"],
        "properties": {
            "path": {"type": "string"},
            "old": {"type": "string"},
            "new": {"type": "string"},
            "criterion": {"type": "string"},
            "reason": {"type": "string"},
        },
    }});
    schema
}

/// How a confirmer hands its document back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    ShellRedirect,
    FinalMessage,
    StructuredOutput,
}

/// The confirmer's prompt: the same for every harness but the delivery line.
pub fn confirm_prompt(task_id: &str, fix_commit: &str, how: How) -> String {
    let deliver = match how {
        How::ShellRedirect => "Write the document to .agent/review.json with a shell redirect (you have no Write tool); write no other file.",
        How::FinalMessage => "Your final message is the document, constrained by --output-schema.",
        How::StructuredOutput => "Answer with the document in structured_output, constrained by --json-schema.",
    };
    format!(
        "You are the confirmer the commander chose for task {task_id}. Another reviewer rejected the candidate and attached a small fix; thoth applied it and this checkout is the candidate with the fix committed ({fix_commit}). You did not write the implementation and have no session of the implementer's: judge only what is in front of you.\n\n\
         ## Input\n\
         Read the files in {INPUT_DIR}/: {INPUT_SPEC} (the task's spec), {INPUT_FINDINGS} (the rejecting review's findings), {INPUT_EDITS} (every edit: index, path, op, criterion, fix_size, category, reason, old, new) and {INPUT_PATCH} (the whole fix as a diff from the candidate).\n\n\
         ## What to judge\n\
         For each edit, only: does it resolve the finding it is bound to; does it change what the spec means; does it bypass or weaken a test; does it break something else. Do not review the rest of the change again, do not raise new findings, and do not modify any file.\n\
         Answer every edit index exactly once: \"ok\", or \"revise\" with a note saying what is wrong. The verdict is APPROVE when every edit is ok and REVISE when at least one is revise.\n\n\
         ## Output\n\
         {deliver}\n\
         Shape: {{\"verdict\":\"APPROVE\",\"edits\":[{{\"index\":0,\"verdict\":\"ok\",\"note\":\"\"}}]}}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_json::{CriterionFinding, ExtraFinding};

    fn findings() -> Findings {
        Findings {
            criteria: vec![
                CriterionFinding {
                    id: "AC1".into(),
                    verdict: "unmet".into(),
                    evidence: vec![],
                    note: Some("n".into()),
                    fix_size: Some("S".into()),
                    category: Some("docs".into()),
                },
                CriterionFinding {
                    id: "AC2".into(),
                    verdict: "met".into(),
                    evidence: vec!["E1".into()],
                    note: None,
                    fix_size: None,
                    category: None,
                },
            ],
            extra_findings: vec![ExtraFinding {
                severity: "minor".into(),
                note: "x".into(),
                evidence: vec![],
                fix_size: Some("M".into()),
                category: None,
            }],
            sources: vec![],
        }
    }

    fn edit(path: &str, old: &str, new: &str) -> FixEdit {
        FixEdit { path: path.into(), old: old.into(), new: new.into(), criterion: "AC1".into(), reason: "r".into() }
    }

    fn item(path: &str, old: &str, new: &str, criterion: &str, reason: &str) -> Value {
        json!({"path": path, "old": old, "new": new, "criterion": criterion, "reason": reason})
    }

    fn stage_of(tmp: &Path, items: Vec<Value>) -> Option<(&'static str, String)> {
        validate(tmp, &items, &findings()).err().map(|r| (r.stage, r.reason))
    }

    #[test]
    fn check_path_refuses_every_unsafe_shape_and_accepts_a_plain_path() {
        assert!(check_path("src/main.rs").is_ok());
        assert!(check_path("docs/a b/c.md").is_ok());
        for bad in [
            "", "a//b", "./a", "a/./b", "a/../b", "..", "a\\b", "c:/x", "a:b", "/etc/x", "a\0b",
            ".agent/x", ".Agents/x", ".git/config", ".GIT./config", ".agent /x", "nul", "NUL.txt",
            "src/con.rs", "src/aux", "com1.log", "lpt9", "a./b", "a /b", "src/x.", "src/x ",
        ] {
            assert!(check_path(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn validate_refuses_each_field_and_binding_problem_at_stage_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cases = vec![
            (json!({"path": "a", "old": "", "new": "x", "criterion": "AC1"}), "shape"),
            (json!({"path": "a", "old": "", "new": "x", "criterion": "AC1", "reason": "r", "more": 1}), "shape"),
            (json!({"path": 1, "old": "", "new": "x", "criterion": "AC1", "reason": "r"}), "shape"),
            (item("../x", "", "x", "AC1", "r"), "segment"),
            (item("a", "", "", "AC1", "r"), "both empty"),
            (item("a", "", "x", "AC1", "   "), "blank"),
            (item("a", "", "x", "AC2", "r"), "met"),
            (item("a", "", "x", "AC9", "r"), "not an acceptance criterion"),
            (item("a", "", "x", "extra_findings[1]", "r"), "does not have"),
            (item("a", "", "x", "extra_findings[x]", "r"), "not extra_findings"),
        ];
        for (it, why) in cases {
            let (stage, reason) = stage_of(root, vec![it.clone()]).unwrap_or_else(|| panic!("{it} must be refused"));
            assert_eq!(stage, stage::EDITS, "{it}");
            assert!(reason.contains(why), "{it}: {reason}");
        }
        let ok = validate(root, &[item("a", "", "x", "AC1", "r"), item("b", "", "y", "extra_findings[0]", "r")], &findings()).unwrap();
        assert_eq!(ok[0].binding, Binding::Criterion("AC1".into()));
        assert_eq!(ok[0].fix_size.as_deref(), Some("S"));
        assert_eq!(ok[1].binding, Binding::Extra(0));
        assert_eq!(ok[1].fix_size.as_deref(), Some("M"));
    }

    #[cfg(unix)]
    #[test]
    fn validate_refuses_a_symlinked_file_or_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("t.txt"), "x\n").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        std::os::unix::fs::symlink(outside.path().join("t.txt"), root.join("f.txt")).unwrap();
        for path in ["link/t.txt", "f.txt"] {
            let (stage, reason) = stage_of(root, vec![item(path, "x", "y", "AC1", "r")]).unwrap();
            assert_eq!(stage, stage::EDITS);
            assert!(reason.contains("symlink"), "{reason}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn validate_refuses_a_junction_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("t.txt"), "x\n").unwrap();
        let mut cmd = std::process::Command::new("cmd");
        cmd.args(["/C", "mklink", "/J"]).arg(root.join("link")).arg(outside.path());
        let status = crate::process::quiet(&mut cmd).output().unwrap();
        assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
        let (stage, reason) = stage_of(root, vec![item("link/t.txt", "x", "y", "AC1", "r")]).unwrap();
        assert_eq!(stage, stage::EDITS);
        assert!(reason.contains("junction"), "{reason}");
    }

    #[test]
    fn apply_refuses_each_problem_at_stage_apply_and_leaves_files_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "one\ntwo\ntwo\n").unwrap();
        std::fs::write(root.join("bin.txt"), [0xff, 0xfe, 0x00, 0x41]).unwrap();
        std::fs::create_dir(root.join("dir")).unwrap();
        let cases = vec![
            (edit("a.txt", "three", "x"), "not found"),
            (edit("a.txt", "two", "x"), "2 times"),
            (edit("a.txt", "", "x"), "already exists"),
            (edit("missing.txt", "one", "x"), "does not exist"),
            (edit("bin.txt", "A", "B"), "not UTF-8"),
            (edit("dir", "x", "y"), "not a regular file"),
            (edit("a.txt", "one", "one"), "as it was"),
        ];
        for (e, why) in cases {
            let r = apply(root, std::slice::from_ref(&e)).unwrap_err();
            assert_eq!(r.stage, stage::APPLY, "{e:?}");
            assert!(r.reason.contains(why), "{e:?}: {}", r.reason);
            assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "one\ntwo\ntwo\n");
        }
    }

    #[test]
    fn apply_runs_in_order_and_a_later_edit_sees_the_earlier_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "alpha\nbeta\n").unwrap();
        let ops = apply(
            root,
            &[edit("a.txt", "alpha", "gamma"), edit("a.txt", "gamma\nbeta", "gamma\ndelta"), edit("new/b.txt", "", "made\n")],
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "gamma\ndelta\n");
        assert_eq!(std::fs::read_to_string(root.join("new/b.txt")).unwrap(), "made\n");
        assert_eq!(ops.iter().map(|o| o.op).collect::<Vec<_>>(), ["replace", "replace", "create"]);
    }

    #[test]
    fn a_failing_second_edit_rolls_the_first_back() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("a.txt"), "alpha\n").unwrap();
        let r = apply(root, &[edit("a.txt", "alpha", "gamma"), edit("c.txt", "", "x"), edit("a.txt", "nope", "x")]).unwrap_err();
        assert_eq!(r.stage, stage::APPLY);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "alpha\n");
        assert!(!root.join("c.txt").exists());
    }

    #[test]
    fn a_crlf_file_matched_with_lf_text_keeps_crlf() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::write(root.join("w.txt"), "one\r\ntwo\r\nthree\r\n").unwrap();
        apply(root, &[edit("w.txt", "one\ntwo", "one\nTWO\nmore")]).unwrap();
        assert_eq!(std::fs::read(root.join("w.txt")).unwrap(), b"one\r\nTWO\r\nmore\r\nthree\r\n");
    }

    #[test]
    fn size_needs_s_or_m_and_every_unmet_criterion_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let f = findings();
        let bound = validate(tmp.path(), &[item("a", "", "x", "AC1", "r")], &f).unwrap();
        assert!(check_size(&bound, &f).is_ok());
        let extra_only = validate(tmp.path(), &[item("a", "", "x", "extra_findings[0]", "r")], &f).unwrap();
        assert_eq!(check_size(&extra_only, &f).unwrap_err().stage, stage::SIZE);
        let mut large = f.clone();
        large.criteria[0].fix_size = Some("L".into());
        let bound = validate(tmp.path(), &[item("a", "", "x", "AC1", "r")], &large).unwrap();
        assert_eq!(check_size(&bound, &large).unwrap_err().stage, stage::SIZE);
        let mut unsized_ = f.clone();
        unsized_.criteria[0].fix_size = None;
        let bound = validate(tmp.path(), &[item("a", "", "x", "AC1", "r")], &unsized_).unwrap();
        assert_eq!(check_size(&bound, &unsized_).unwrap_err().stage, stage::SIZE);
    }

    #[test]
    fn bounds_count_lines_and_files_and_refuse_binary() {
        let file = |path: &str, a: Option<u64>, d: Option<u64>| FileStat { path: path.into(), status: "modified".into(), added: a, deleted: d };
        let diff = |files: Vec<FileStat>| Diff {
            changed_lines: files.iter().map(|f| f.added.unwrap_or(0) + f.deleted.unwrap_or(0)).sum(),
            file_count: files.len(),
            files,
        };
        assert!(check_bounds(&diff(vec![file("a", Some(20), Some(20))])).is_ok());
        assert_eq!(check_bounds(&diff(vec![file("a", Some(21), Some(20))])).unwrap_err().stage, stage::BOUNDS);
        assert!(check_bounds(&diff(vec![file("a", Some(1), None), file("b", Some(1), Some(0))])).unwrap_err().reason.contains("binary"));
        assert!(check_bounds(&diff(vec![file("a", Some(1), Some(0)), file("b", Some(1), Some(0)), file("c", Some(1), Some(0))])).is_ok());
        assert_eq!(
            check_bounds(&diff(vec![file("a", Some(1), Some(0)), file("b", Some(1), Some(0)), file("c", Some(1), Some(0)), file("d", Some(1), Some(0))]))
                .unwrap_err()
                .stage,
            stage::BOUNDS
        );
    }

    #[test]
    fn the_edit_schema_is_strict_at_every_level() {
        fn strict(v: &Value) -> bool {
            if v["type"] == "object" {
                if v["additionalProperties"] != Value::Bool(false) {
                    return false;
                }
                let req: Vec<&str> = v["required"].as_array().unwrap().iter().filter_map(|x| x.as_str()).collect();
                for (k, sub) in v["properties"].as_object().unwrap() {
                    if !req.contains(&k.as_str()) || !strict(sub) {
                        return false;
                    }
                }
            }
            v.get("items").map(strict).unwrap_or(true)
        }
        assert!(strict(&edit_output_schema()));
        assert!(edit_output_schema()["required"].as_array().unwrap().contains(&json!("edits")));
    }
}
