//! The per-repo catalogue of approved checks (P7-1a).
//!
//! A repo may *propose* what to run (`thoth checks add`), but only the human
//! authorises it (`thoth checks approve`), and the approval lives in
//! `state.db` where a worker cannot reach it. An approval pins a fingerprint of
//! the command line, the shell, the cwd, the executable's bytes and the bytes of
//! its input files. A check keeps every approval it was given; `list`
//! compares them with the tree as it is now, and `verify` (P7-1b) runs only
//! checks one of whose pins still matches, exactly or differing only in
//! package information (`match_approval`).

use crate::blobs;
use crate::config::Paths;
use crate::ids;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf};

pub const SHELLS: [&str; 5] = ["argv", "cmd", "powershell", "sh", "bash"];

/// Files that a toolchain reads implicitly, keyed by the executable's name
/// (file stem, lower-cased, `.exe` dropped). A command such as `cargo test`
/// depends on `Cargo.lock` without ever naming it.
pub const TOOLCHAIN_INPUTS: &[(&[&str], &[&str])] = &[
    (
        &["cargo"],
        &[
            "Cargo.toml",
            "Cargo.lock",
            "build.rs",
            ".cargo/config.toml",
            "rust-toolchain.toml",
            "rust-toolchain",
        ],
    ),
    (
        &["npm", "npx", "pnpm", "yarn", "node"],
        &["package.json", "package-lock.json", "pnpm-lock.yaml", "yarn.lock"],
    ),
    (
        &["python", "python3", "py", "pytest"],
        &[
            "pyproject.toml",
            "setup.cfg",
            "setup.py",
            "pytest.ini",
            "tox.ini",
            "conftest.py",
            "requirements.txt",
        ],
    ),
    (&["go"], &["go.mod", "go.sum"]),
    (
        &["dotnet"],
        &["global.json", "Directory.Build.props", "Directory.Build.targets", "NuGet.config"],
    ),
];

/// `dotnet` additionally depends on any project or solution file in its cwd.
pub const DOTNET_PROJECT_EXTENSIONS: [&str; 3] = ["sln", "csproj", "fsproj"];

/// One row of the `checks` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub check_id: String,
    pub shell: String,
    pub cmd: String,
    pub cwd: String,
    /// Paths relative to the repo root, `/`-separated, sorted, unique.
    pub inputs: Vec<String>,
    pub created_at: String,
    pub approved_at: Option<String>,
    pub approved_fingerprint: Option<String>,
    pub approved_detail: Option<String>,
    pub approved_commit: Option<String>,
    /// How the `exe=` line pins the program: `hash` (its resolved path and
    /// its bytes, the default) or `path` (its resolved absolute path only).
    pub exe_pin: String,
}

pub struct Fingerprint {
    pub hex: String,
    pub lines: Vec<String>,
    pub detail: Value,
    /// The absolute path of the file the `exe=` line pins, or `None` when the
    /// program resolved to no file. AC14 (b): `verify` executes exactly this
    /// file and never resolves the program a second time.
    pub program: Option<PathBuf>,
}

/// `[a-z0-9_-]{1,32}`.
pub fn valid_check_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

/// A declared `cwd` is text from outside the control plane. Purely lexical -
/// no canonicalization, no symlink chasing: the point is to keep the path
/// inside the tree it is about to be joined onto. `Path::join` silently drops
/// the base for an absolute path and never normalizes `..`, so both are
/// rejected explicitly. Shared by `verify` and `checks add`.
pub fn cwd_escapes_the_worktree(cwd: &str) -> bool {
    Path::new(cwd)
        .components()
        .any(|c| !matches!(c, Component::CurDir | Component::Normal(_)))
}

/// A relative path in canonical form (`/` separators, no `.` parts, `.` for
/// the empty path), or `None` when it is absolute, has a drive or `..`.
pub fn normalize_rel(p: &str) -> Option<String> {
    let slashed = p.replace('\\', "/");
    if cwd_escapes_the_worktree(p) || cwd_escapes_the_worktree(&slashed) {
        return None;
    }
    let parts: Vec<&str> = slashed
        .split('/')
        .filter(|s| !s.is_empty() && *s != ".")
        .collect();
    Some(if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    })
}

/// Whitespace-separated words of a command line, surrounding `"` removed.
fn tokens(cmd: &str) -> Vec<String> {
    cmd.split_whitespace()
        .map(|t| t.trim_matches('"').to_string())
        .filter(|t| !t.is_empty())
        .collect()
}

/// A shell check's first word, surrounding `"` removed.
pub fn first_token(cmd: &str) -> String {
    tokens(cmd).into_iter().next().unwrap_or_default()
}

/// The words of a check's command line as `shell` reads them. AC13: `argv`
/// uses the very split its run uses (`process::split_argv`, so a quoted path
/// with a space is one word); a line without quotes splits as before.
fn words_for(shell: &str, cmd: &str) -> Vec<String> {
    if shell == "argv" {
        crate::process::split_argv(cmd).into_iter().filter(|t| !t.is_empty()).collect()
    } else {
        tokens(cmd)
    }
}

/// The word `fingerprint` resolves as the executable: for `argv` the first
/// element of the split the run uses, for a shell its first word.
pub fn program_word(shell: &str, cmd: &str) -> String {
    if shell == "argv" {
        crate::process::split_argv(cmd).into_iter().next().unwrap_or_default()
    } else {
        first_token(cmd)
    }
}

/// The executable's name for the toolchain table: file name, lower-cased, a
/// trailing `.exe` dropped.
fn exe_stem(token: &str) -> String {
    let name = token.rsplit(['/', '\\']).next().unwrap_or(token).to_lowercase();
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}

fn join_rel(dir: &str, file: &str) -> String {
    if dir == "." {
        file.to_string()
    } else {
        format!("{dir}/{file}")
    }
}

/// The inputs `add` can work out by itself, per AC3: words of the command that
/// are files under the cwd, plus the toolchain's implicit files. Only regular
/// files that exist in `root` are returned; sorted, unique, relative to `root`.
pub fn infer_inputs(root: &Path, cwd: &str, shell: &str, cmd: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut push_if_file = |rel: String| {
        if root.join(&rel).is_file() {
            found.push(rel);
        }
    };

    for t in words_for(shell, cmd) {
        // A word that leaves the cwd (`..`, absolute) is not an input of this
        // check's own tree.
        if let Some(n) = normalize_rel(&t) {
            if n != "." {
                push_if_file(join_rel(cwd, &n));
            }
        }
    }

    let stem = exe_stem(&program_word(shell, cmd));
    // The toolchain's files sit in the cwd or, for a workspace, above it.
    let mut dirs: Vec<String> = vec![cwd.to_string()];
    let mut cur = cwd.to_string();
    while let Some((parent, _)) = cur.rsplit_once('/') {
        dirs.push(parent.to_string());
        cur = parent.to_string();
    }
    if cwd != "." {
        dirs.push(".".to_string());
    }
    for (names, files) in TOOLCHAIN_INPUTS {
        if !names.contains(&stem.as_str()) {
            continue;
        }
        for d in &dirs {
            for f in *files {
                push_if_file(join_rel(d, f));
            }
        }
        if stem == "dotnet" {
            if let Ok(rd) = std::fs::read_dir(root.join(cwd)) {
                for e in rd.flatten() {
                    let p = e.path();
                    let ext = p.extension().and_then(|x| x.to_str()).unwrap_or("");
                    if p.is_file() && DOTNET_PROJECT_EXTENSIONS.contains(&ext) {
                        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                            push_if_file(join_rel(cwd, name));
                        }
                    }
                }
            }
        }
    }
    found.sort();
    found.dedup();
    found
}

#[cfg(unix)]
fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata().map(|m| m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_p: &Path) -> bool {
    true
}

/// First usable file among `base` and, on Windows, `base` plus `.exe`,
/// `.cmd`, `.bat`.
fn first_hit(base: &Path) -> Option<PathBuf> {
    let mut candidates = vec![base.to_path_buf()];
    if cfg!(windows) {
        for ext in ["exe", "cmd", "bat"] {
            let mut s = base.as_os_str().to_os_string();
            s.push(format!(".{ext}"));
            candidates.push(PathBuf::from(s));
        }
    }
    candidates.into_iter().find(|c| c.is_file() && is_executable(c))
}

/// The directories a program is searched in: `path`'s entries in order, with
/// the empty and relative ones dropped. AC12 (a): the fingerprint's lookup and
/// the child's `PATH` (`process::shell`) both use this one list, so the file
/// pinned is the file the shell finds - a relative entry such as `.` would be
/// read against thoth's cwd here and against the check's cwd in the child.
pub fn search_dirs(path: Option<&std::ffi::OsStr>) -> Vec<PathBuf> {
    path.map(|p| std::env::split_paths(p).filter(|d| d.is_absolute()).collect())
        .unwrap_or_default()
}

/// Finds the file a command's first word runs. A word with a path separator is
/// relative to `tree_root/cwd`; any other word is searched along `PATH`.
pub fn resolve_exe(token: &str, tree_root: &Path, cwd: &str) -> Option<PathBuf> {
    resolve_exe_on(token, tree_root, cwd, std::env::var_os("PATH").as_deref())
}

/// `resolve_exe` with the `PATH` value as a parameter.
fn resolve_exe_on(
    token: &str,
    tree_root: &Path,
    cwd: &str,
    path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if token.is_empty() {
        return None;
    }
    if token.contains('/') || token.contains('\\') {
        return first_hit(&tree_root.join(cwd).join(token.replace('\\', "/")));
    }
    search_dirs(path)
        .into_iter()
        .find_map(|dir| first_hit(&dir.join(token)))
}

/// cmd.exe's own default when `PATHEXT` is unset.
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC";

/// `PATHEXT` in order, lower-cased, each with its dot.
pub fn pathext() -> Vec<String> {
    let raw = std::env::var("PATHEXT")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PATHEXT.to_string());
    raw.split(';')
        .map(str::trim)
        .filter(|e| e.len() > 1 && e.starts_with('.'))
        .map(str::to_ascii_lowercase)
        .collect()
}

/// The file names, in order, that `cmd` or `powershell` tries for `name`
/// within one directory. Probed on Windows 11 (AC11): a name with an
/// extension is tried as written first; then `name` plus each PATHEXT entry
/// (cmd.exe runs `.com` before `.exe`, `.bat` before `.cmd`); PowerShell tries
/// `name.ps1` ahead of those, and an extensionless `name` last. cmd.exe never
/// runs an extensionless file.
fn shell_candidates(shell: &str, name: &str) -> Vec<String> {
    let has_ext = Path::new(name).extension().is_some();
    let mut out = Vec::new();
    if has_ext {
        out.push(name.to_string());
    }
    if shell == "powershell" {
        out.push(format!("{name}.ps1"));
    }
    out.extend(pathext().into_iter().map(|e| format!("{name}{e}")));
    if shell == "powershell" && !has_ext {
        out.push(name.to_string());
    }
    out
}

/// The file a `cmd` or `powershell` line's first word runs: for a word with a
/// path separator, looked up in that directory (relative to `tree_root/cwd`);
/// otherwise along the absolute `PATH` entries in order, the first directory
/// holding any candidate winning. Neither shell searches the cwd here: thoth
/// sets `NoDefaultCurrentDirectoryInExePath` for cmd.exe, PowerShell never
/// does, and relative `PATH` entries are dropped from the child's `PATH`.
fn resolve_in_shell(shell: &str, token: &str, tree_root: &Path, cwd: &str) -> Option<PathBuf> {
    let (dirs, name) = if token.contains('/') || token.contains('\\') {
        let full = tree_root.join(cwd).join(token.replace('\\', "/"));
        let name = full.file_name()?.to_string_lossy().into_owned();
        (vec![full.parent()?.to_path_buf()], name)
    } else {
        (search_dirs(std::env::var_os("PATH").as_deref()), token.to_string())
    };
    let candidates = shell_candidates(shell, &name);
    dirs.iter()
        .find_map(|d| candidates.iter().map(|c| d.join(c)).find(|p| p.is_file()))
}

/// `resolve_exe` as the check's shell sees it. AC11 (e): `argv`, `sh` and
/// `bash` keep P7-1a's lookup (their fingerprints must not change); `cmd` and
/// `powershell` pin the file their shell actually runs.
pub fn resolve_exe_for(shell: &str, token: &str, tree_root: &Path, cwd: &str) -> Option<PathBuf> {
    if token.is_empty() {
        return None;
    }
    match shell {
        "cmd" | "powershell" => resolve_in_shell(shell, token, tree_root, cwd),
        _ => resolve_exe(token, tree_root, cwd),
    }
}

/// AC11 (b)(i), (c): why a `cmd` or `powershell` check whose program is a
/// path must not run, or `None`. Such a program must be named in full: its
/// extension written, one the shell runs itself (PATHEXT, or `.ps1` for
/// PowerShell), and the file present. Without the extension the shell picks
/// by PATHEXT order, so a file added beside the approved one could run in its
/// place; with another extension cmd.exe hands the file to its association.
pub fn shell_program_refusal(shell: &str, token: &str, tree_root: &Path, cwd: &str) -> Option<String> {
    if !(shell == "cmd" || shell == "powershell") || !(token.contains('/') || token.contains('\\')) {
        return None;
    }
    let full = tree_root.join(cwd).join(token.replace('\\', "/"));
    let ext = full
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()));
    let mut runnable = pathext();
    if shell == "powershell" {
        runnable.push(".ps1".into());
    }
    match ext {
        Some(e) if runnable.contains(&e) => {}
        _ => {
            return Some(format!(
                "program '{token}' must be written with its extension ({}), for example \
                 '{token}.exe' or '{token}.cmd': without one {shell} picks a file by PATHEXT \
                 order, so another file could run in place of the approved one",
                runnable.join(", ")
            ))
        }
    }
    (!full.is_file()).then(|| format!("program '{token}' does not exist ({})", full.display()))
}

/// AC12 (b): why a shell check's program word is refused, or `None`. The
/// fingerprint takes the command's first word (up to the first whitespace) as
/// the program; a shell would read quotes, `$` and backquotes in it (and, in
/// `sh`/`bash`, backslashes) and run some other file. `argv` has no shell; its
/// only refusal (AC13) is a double quote that is never closed, since the split
/// would then read the rest of the line as one word.
pub fn program_word_refusal(shell: &str, cmd: &str) -> Option<String> {
    if shell == "argv" {
        return (cmd.matches('"').count() % 2 == 1).then(|| {
            format!(
                "the command '{cmd}' has a double quote that is not closed; close it, or \
                 remove it"
            )
        });
    }
    if !matches!(shell, "cmd" | "powershell" | "sh" | "bash") {
        return None;
    }
    let word = cmd.split_whitespace().next().unwrap_or("");
    let posix = matches!(shell, "sh" | "bash");
    let bad = word
        .chars()
        .find(|c| matches!(c, '"' | '\'' | '`' | '$') || (posix && *c == '\\'))?;
    Some(format!(
        "the program name must be a plain word, but '{word}' contains {bad:?}, which {shell} \
         would interpret before choosing the program; use shell argv for this check, or remove \
         the quotes"
    ))
}

/// The fixed part of every AC14 (a) refusal.
pub const NOT_ONE_PROGRAM: &str = "a shell check must be one program and plain arguments";

/// AC14 (a): the only characters a `cmd`, `powershell`, `sh` or `bash` check
/// may hold. None of them means anything to any of the four shells, so the
/// shell has nothing to interpret: thoth hands it the program as an absolute
/// path and these words as its arguments.
fn plain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, ' ' | '\t' | '.' | '_' | '-' | '/' | '\\' | ':' | '=')
}

/// AC14 (a): why a shell check's command is not one program with plain
/// arguments, or `None` (always `None` for `argv`, which has no shell). The
/// command must not be blank, every character must be `plain_char`, and the
/// program word (up to the first space or tab) must not hold `=` (`FOO=bar
/// tool` sets a variable in sh).
pub fn shell_grammar_refusal(shell: &str, cmd: &str) -> Option<String> {
    if !matches!(shell, "cmd" | "powershell" | "sh" | "bash") {
        return None;
    }
    let what = if cmd.trim().is_empty() {
        "the command is blank".to_string()
    } else if let Some(c) = cmd.chars().find(|c| !plain_char(*c)) {
        format!("{c:?} (U+{:04X}) is not allowed", u32::from(c))
    } else {
        let word = cmd.trim().split([' ', '\t']).next().unwrap_or("");
        if !word.contains('=') {
            return None;
        }
        format!("the program word '{word}' holds '='")
    };
    Some(format!(
        "{NOT_ONE_PROGRAM}: {what} (allowed: ASCII letters and digits, space, tab, and . _ - / \\ : \
         =; no '=' in the program word). To use shell syntax (chaining, redirection, variables, \
         conditions), write it in a script file and add that script as the check. If an \
         argument holds a space or a character outside that list, use shell argv for this check"
    ))
}

/// Everything `checks add` and `verify` refuse in a check's command text
/// alone, in order: AC12 (b)'s program word (and AC13's unclosed quote for
/// `argv`), then AC14 (a)'s grammar.
pub fn command_refusal(shell: &str, cmd: &str) -> Option<String> {
    program_word_refusal(shell, cmd).or_else(|| shell_grammar_refusal(shell, cmd))
}

/// The lower-cased extension of `p` with its dot, or `""`.
fn dot_ext(p: &Path) -> String {
    p.extension()
        .map(|e| format!(".{}", e.to_string_lossy().to_ascii_lowercase()))
        .unwrap_or_default()
}

/// AC14 (d): a `powershell` check whose program resolved to a batch file is
/// refused. PowerShell hands a `.cmd`/`.bat` to cmd.exe, which expands any
/// `%` in its path, so the file that ran might not be the one pinned.
/// Reported ahead of AC11's in-tree naming rule.
pub fn powershell_batch_refusal(shell: &str, program: Option<&Path>) -> Option<String> {
    let p = program.filter(|_| shell == "powershell")?;
    matches!(dot_ext(p).as_str(), ".cmd" | ".bat").then(|| {
        format!(
            "{} is a batch file, which PowerShell hands on to cmd.exe; use shell cmd for this check",
            p.display()
        )
    })
}

/// AC14 (c), (d) and (b)'s PowerShell path rule: why the file a shell check's
/// program resolved to (`program`, from the fingerprint) must not run, or
/// `None`. Applies to `cmd`, `powershell`, `sh` and `bash`.
pub fn resolved_program_refusal(shell: &str, word: &str, program: Option<&Path>) -> Option<String> {
    if !matches!(shell, "cmd" | "powershell" | "sh" | "bash") {
        return None;
    }
    let Some(p) = program else {
        return Some(format!(
            "program not found: '{word}' does not resolve to a file (relative to the check's cwd \
             for a path, along the absolute PATH entries otherwise). A shell's built-in command \
             (cmd's call, cd or exit; sh's exec or exit) is not a program: to use one, write it \
             in a script file and add that script as the check"
        ));
    };
    let runs: &[&str] = match shell {
        "cmd" => &[".exe", ".com", ".cmd", ".bat"],
        "powershell" => &[".exe", ".com", ".ps1"],
        _ => return None,
    };
    let ext = dot_ext(p);
    if !runs.contains(&ext.as_str()) {
        let kind = if ext.is_empty() { "a file without an extension".to_string() } else { format!("a '{ext}' file") };
        return Some(format!(
            "{} is {kind}, which {shell} does not run itself (Windows would open it with its file \
             association, so what started would not be the file exe= pins); {shell} runs {} files",
            p.display(),
            runs.join(", ")
        ));
    }
    if shell == "powershell" {
        if let Some(c) = p.display().to_string().chars().find(|c| matches!(c, '[' | ']' | '`')) {
            return Some(format!(
                "powershell cannot run {}: its path holds {c:?}, which PowerShell's call operator \
                 reads as a wildcard or escape; move the file, or use another shell for this check",
                p.display()
            ));
        }
    }
    None
}

/// `a/./b/../c` -> `a/c`, `/`-separated; no filesystem access.
fn lexical_clean(p: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop();
            }
            other => parts.push(other.as_os_str().to_string_lossy().into_owned()),
        }
    }
    parts.join("/")
}

/// Whether `program` lies inside `tree`. Compared as canonical paths when both
/// resolve (case and separators unified, links followed), otherwise as
/// absolute paths; either way by whole components, so `/tmp/t` does not hold
/// `/tmp/t2/x`.
pub fn inside_tree(program: &Path, tree: &Path) -> bool {
    if let (Ok(p), Ok(t)) = (std::fs::canonicalize(program), std::fs::canonicalize(tree)) {
        return p.starts_with(&t);
    }
    match (std::path::absolute(program), std::path::absolute(tree)) {
        (Ok(p), Ok(t)) => p.starts_with(&t),
        _ => false,
    }
}

fn file_sha(p: &Path) -> Option<String> {
    blobs::sha256_file(p).ok().map(|(h, _)| h)
}

/// The fixed-order text that is hashed. Built from the `detail` document so
/// that the lines of an old approval can be rebuilt from what was stored.
fn lines_from(shell: &str, cmd: &str, cwd: &str, detail: &Value) -> Vec<String> {
    let mut lines = vec![format!("shell={shell}"), format!("cmd={cmd}"), format!("cwd={cwd}")];
    let exe_path = detail["exe"]["path"].as_str().unwrap_or("");
    // A check added with `--exe-pin path` pins the path alone; the marker is
    // part of the line, so switching it needs a new approval.
    let exe_sha = if detail["exe"]["pin"].as_str() == Some("path") {
        "path-only"
    } else {
        detail["exe"]["sha256"].as_str().unwrap_or("none")
    };
    lines.push(format!("exe={exe_path}|{exe_sha}"));
    let mut inputs: Vec<(String, String)> = detail["inputs"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| {
                    (
                        i["path"].as_str().unwrap_or("").to_string(),
                        i["sha256"].as_str().unwrap_or("missing").to_string(),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    inputs.sort();
    for (p, h) in inputs {
        lines.push(format!("input={p}|{h}"));
    }
    lines
}

/// What the check would pin right now, computed against `tree_root` (the main
/// tree, or a run's worktree). A missing input or executable is recorded as
/// such rather than failing, so its later appearance shows up as a change.
pub fn fingerprint(check: &Check, tree_root: &Path) -> Result<Fingerprint> {
    let token = program_word(&check.shell, &check.cmd);
    let resolved = resolve_exe_for(&check.shell, &token, tree_root, &check.cwd);
    let exe = match &resolved {
        Some(p) => {
            // In-tree executables are named relative to the tree, so the same
            // file fingerprints alike in the main tree and in a worktree.
            let shown = match p.strip_prefix(tree_root) {
                Ok(rel) => lexical_clean(rel),
                Err(_) => p.display().to_string(),
            };
            if check.exe_pin == "path" {
                json!({"path": shown, "sha256": Value::Null, "pin": "path"})
            } else {
                json!({"path": shown, "sha256": file_sha(p)})
            }
        }
        None => json!({"path": token, "sha256": Value::Null}),
    };
    // A hit through a relative PATH entry would be relative to thoth's own
    // cwd; `search_dirs` drops those, so every hit is absolute. `absolute`
    // only tidies it lexically (`wt/./x/tool.cmd` -> `wt\x\tool.cmd`).
    let program = resolved
        .filter(|p| p.is_absolute())
        .map(|p| std::path::absolute(&p).unwrap_or(p));
    let mut paths = check.inputs.clone();
    paths.sort();
    let inputs: Vec<Value> = paths
        .iter()
        .map(|p| json!({"path": p, "sha256": file_sha(&tree_root.join(p))}))
        .collect();
    let detail = json!({"exe": exe, "inputs": inputs});
    let lines = lines_from(&check.shell, &check.cmd, &check.cwd, &detail);
    let mut text = lines.join("\n");
    text.push('\n');
    Ok(Fingerprint {
        hex: ids::sha256_hex(text.as_bytes()),
        lines,
        detail,
        program,
    })
}

/// An approval's stored detail document, or `Null` when it has none.
fn approval_detail(approval: &Approval) -> Value {
    approval
        .detail
        .as_deref()
        .and_then(|d| serde_json::from_str(d).ok())
        .unwrap_or(Value::Null)
}

/// The keys (text before the first `|`) of the lines that differ between the
/// approval's lines and the current ones, e.g. `input=Cargo.toml`.
pub fn changed_lines(check: &Check, approval: &Approval, now: &Fingerprint) -> Vec<String> {
    let detail = approval_detail(approval);
    let old = lines_from(&check.shell, &check.cmd, &check.cwd, &detail);
    let key = |l: &String| l.split('|').next().unwrap_or("").to_string();
    let mut out: Vec<String> = old
        .iter()
        .filter(|l| !now.lines.contains(l))
        .chain(now.lines.iter().filter(|l| !old.contains(l)))
        .map(key)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// `None` when `now` still matches `approval`, otherwise the keys of the lines
/// that differ (as `changed_lines`). `inputs_checked == false` is the `verify
/// --at-base` rule: a baseline checkout predates the candidate's edits by
/// design, so its input files are never read: the approval's own `input=`
/// lines stand in for them (see `at_base_hex`).
pub fn fingerprint_mismatch(
    check: &Check,
    approval: &Approval,
    now: &Fingerprint,
    inputs_checked: bool,
) -> Option<Vec<String>> {
    if inputs_checked {
        if approval.fingerprint == now.hex {
            return None;
        }
        let changed = changed_lines(check, approval, now);
        // The hex differs but no line does (an approval without its detail):
        // still a mismatch, named as a whole.
        return Some(if changed.is_empty() {
            vec!["fingerprint".to_string()]
        } else {
            changed
        });
    }
    // AC10 (4): the approval's own hex is what is compared, not just lines
    // rebuilt from the row. The text hashed is the row's shell/cmd/cwd, the
    // exe as resolved now, and the input lines the approval recorded - so a
    // changed shell, cmd, cwd or exe, or a stored fingerprint that no longer
    // matches its own detail, all refuse.
    if approval.fingerprint == at_base_hex(check, approval, now) {
        return None;
    }
    let changed: Vec<String> = changed_lines(check, approval, now)
        .into_iter()
        .filter(|k| !k.starts_with("input="))
        .collect();
    Some(if changed.is_empty() {
        vec!["fingerprint".to_string()]
    } else {
        changed
    })
}

/// The hex `verify --at-base` compares with an approval's fingerprint: the
/// check's current `shell=`/`cmd=`/`cwd=` lines, the `exe=` line from `now`,
/// and the `input=` lines from the approval's stored detail (a baseline
/// checkout predates the candidate's input edits by design). Hashed exactly as
/// `fingerprint` hashes.
fn at_base_hex(check: &Check, approval: &Approval, now: &Fingerprint) -> String {
    let approved = approval_detail(approval);
    let detail = json!({"exe": now.detail["exe"].clone(), "inputs": approved["inputs"].clone()});
    hash_lines(&lines_from(&check.shell, &check.cmd, &check.cwd, &detail))
}

/// The fingerprint hex of a set of lines: joined with `\n`, one more `\n`.
fn hash_lines(lines: &[String]) -> String {
    let mut text = lines.join("\n");
    text.push('\n');
    ids::sha256_hex(text.as_bytes())
}

/// One human approval of a check (a row of `check_approvals`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Approval {
    pub approved_at: String,
    pub fingerprint: String,
    pub detail: Option<String>,
    pub approved_commit: Option<String>,
    /// `main`, `run` or `migrated`.
    pub source: String,
    pub run_id: Option<String>,
}

/// One line package-info auto-accept let through: `line` counts from 1, the
/// texts are the line without its trailing `\r`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageLine {
    pub path: String,
    pub line: usize,
    pub approved: String,
    pub now: String,
}

/// The approval a fingerprint was matched to, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalMatch {
    Exact(Approval),
    PackageInfoOnly { approval: Approval, lines: Vec<PackageLine> },
    /// `changed` names the lines that differ from the newest approval;
    /// `note` explains why package-info auto-accept could not be tried.
    NoMatch { changed: Vec<String>, note: Option<String> },
}

/// The `[package]` keys whose single-line value may change without a new
/// approval: none of them changes what is compiled or run.
const PACKAGE_INFO_KEYS: [&str; 11] = [
    "version",
    "description",
    "license",
    "license-file",
    "authors",
    "repository",
    "homepage",
    "documentation",
    "readme",
    "keywords",
    "categories",
];

/// Whether `path`'s last `/`-separated part is `Cargo.toml` or `Cargo.lock`.
pub fn is_package_file(path: &str) -> bool {
    matches!(path.rsplit('/').next(), Some("Cargo.toml" | "Cargo.lock"))
}

const BLANKS: [char; 2] = [' ', '\t'];

/// `"..."` with no `"` or `\` inside.
fn plain_string(s: &str) -> bool {
    s.len() >= 2 && s.starts_with('"') && s.ends_with('"') && !s[1..s.len() - 1].contains(['"', '\\'])
}

/// A plain string, or a one-line array of plain strings (a trailing comma
/// allowed). A string holding a comma is not split correctly and so refused.
fn plain_value(v: &str) -> bool {
    if plain_string(v) {
        return true;
    }
    let Some(inner) = v.strip_prefix('[').and_then(|r| r.strip_suffix(']')) else {
        return false;
    };
    let inner = inner.trim_matches(BLANKS);
    if inner.is_empty() {
        return true;
    }
    let inner = inner.strip_suffix(',').map(|r| r.trim_end_matches(BLANKS)).unwrap_or(inner);
    inner.split(',').all(|item| plain_string(item.trim_matches(BLANKS)))
}

/// The key of a `KEY = VALUE` line whose key is one of `PACKAGE_INFO_KEYS`
/// and whose value is `plain_value` with nothing after it, or `None`.
fn package_info_key(line: &str) -> Option<&str> {
    let s = line.trim_start_matches(BLANKS);
    let (key, value) = s.split_once('=')?;
    let key = key.trim_end_matches(BLANKS);
    (PACKAGE_INFO_KEYS.contains(&key) && plain_value(value.trim_matches(BLANKS))).then_some(key)
}

/// Whether line `i` lies in the `[package]` table: the nearest line above it
/// that starts a table is exactly `[package]`. A table header itself is not
/// inside any table.
fn in_package_table(lines: &[&str], i: usize) -> bool {
    if lines[i].trim().starts_with('[') {
        return false;
    }
    lines[..i]
        .iter()
        .rev()
        .map(|l| l.trim())
        .find(|l| l.starts_with('['))
        == Some("[package]")
}

/// The lines of `text` split at `\n`, each without one trailing `\r`.
fn split_lines(text: &str) -> Vec<&str> {
    text.split('\n').map(|l| l.strip_suffix('\r').unwrap_or(l)).collect()
}

/// `version = "X"` exactly, as Cargo.lock writes it.
fn lock_version_line(line: &str) -> bool {
    line.strip_prefix("version = ").is_some_and(plain_string)
}

/// Whether line `i` lies in a Cargo.lock `[[package]]` block without a
/// `source = ` line (a package of this workspace, not a registry or git one).
fn in_local_lock_package(lines: &[&str], i: usize) -> bool {
    let Some(start) = (0..i).rev().find(|&j| lines[j].starts_with('[')) else {
        return false;
    };
    if lines[start] != "[[package]]" {
        return false;
    }
    let end = (start + 1..lines.len()).find(|&j| lines[j].starts_with('[')).unwrap_or(lines.len());
    !lines[start..end].iter().any(|l| l.starts_with("source = "))
}

/// The lines in which `now` differs from `approved` when every difference is
/// package information only, or `None`. For `Cargo.toml`: a single-line
/// `KEY = VALUE` of one of `PACKAGE_INFO_KEYS` in `[package]`, the same key
/// on both sides. For `Cargo.lock`: the `version = "X"` line of a package
/// without `source`. Lines are compared without their trailing `\r`, so a
/// file that only changed its line endings is `Some` of an empty list.
/// Anything this cannot read for certain (not UTF-8, a multi-line string,
/// another line count, any other path) is `None`.
pub fn package_info_only(path: &str, approved: &[u8], now: &[u8]) -> Option<Vec<PackageLine>> {
    let toml = match path.rsplit('/').next() {
        Some("Cargo.toml") => true,
        Some("Cargo.lock") => false,
        _ => return None,
    };
    let (a, b) = (std::str::from_utf8(approved).ok()?, std::str::from_utf8(now).ok()?);
    if [a, b].iter().any(|t| t.contains("\"\"\"") || t.contains("'''")) {
        return None;
    }
    let (la, lb) = (split_lines(a), split_lines(b));
    if la.len() != lb.len() {
        return None;
    }
    let mut out = Vec::new();
    for i in 0..la.len() {
        if la[i] == lb[i] {
            continue;
        }
        let allowed = if toml {
            in_package_table(&la, i)
                && in_package_table(&lb, i)
                && package_info_key(la[i]).is_some()
                && package_info_key(la[i]) == package_info_key(lb[i])
        } else {
            lock_version_line(la[i])
                && lock_version_line(lb[i])
                && in_local_lock_package(&la, i)
                && in_local_lock_package(&lb, i)
        };
        if !allowed {
            return None;
        }
        out.push(PackageLine {
            path: path.to_string(),
            line: i + 1,
            approved: la[i].to_string(),
            now: lb[i].to_string(),
        });
    }
    Some(out)
}

/// The `(path, sha256)` pairs of a detail document's inputs; a missing
/// file's sha256 is `None`.
fn input_hashes(detail: &Value) -> Vec<(String, Option<String>)> {
    let mut v: Vec<(String, Option<String>)> = detail["inputs"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|i| {
                    (
                        i["path"].as_str().unwrap_or("").to_string(),
                        i["sha256"].as_str().map(String::from),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// The stored copy of the file an approval hashed to `sha`, read only after
/// its bytes are confirmed to hash to `sha`.
fn approved_copy(blobs_dir: &Path, sha: &str) -> Option<Vec<u8>> {
    let p = blobs_dir.join(sha);
    if !p.is_file() || !blobs::verify(&p, sha) {
        return None;
    }
    let bytes = std::fs::read(&p).ok()?;
    (ids::sha256_hex(&bytes) == sha).then_some(bytes)
}

/// The package lines by which `now` (read in `tree_root`) differs from
/// `approval`, when that is all that differs, or `None`.
fn package_info_match(
    check: &Check,
    approval: &Approval,
    now: &Fingerprint,
    tree_root: &Path,
    blobs_dir: &Path,
) -> Option<Vec<PackageLine>> {
    approval.detail.as_ref()?;
    let detail = approval_detail(approval);
    let old = lines_from(&check.shell, &check.cmd, &check.cwd, &detail);
    // The approval's own hash must still fit the check as defined now.
    if hash_lines(&old) != approval.fingerprint {
        return None;
    }
    let exe = |lines: &[String]| lines.iter().find(|l| l.starts_with("exe=")).cloned();
    if exe(&old).is_none() || exe(&old) != exe(&now.lines) {
        return None;
    }
    let (was, is) = (input_hashes(&detail), input_hashes(&now.detail));
    if was.len() != is.len() || was.iter().zip(&is).any(|(a, b)| a.0 != b.0) {
        return None;
    }
    let mut lines = Vec::new();
    for ((path, old_sha), (_, new_sha)) in was.iter().zip(&is) {
        if old_sha == new_sha {
            continue;
        }
        let (Some(old_sha), Some(new_sha)) = (old_sha, new_sha) else {
            return None;
        };
        if !is_package_file(path) {
            return None;
        }
        let approved = approved_copy(blobs_dir, old_sha)?;
        let current = std::fs::read(tree_root.join(path)).ok()?;
        if ids::sha256_hex(&current) != *new_sha {
            return None;
        }
        lines.extend(package_info_only(path, &approved, &current)?);
    }
    lines.sort_by(|a, b| (&a.path, a.line).cmp(&(&b.path, b.line)));
    Some(lines)
}

/// The approval `now` matches, newest first. Approvals older than the check's
/// own `created_at` belong to a definition it replaced and never count.
/// `inputs_checked`: an exact match, or else one that differs only in
/// package information (`package_info_only`, the approved copy taken from
/// `blobs_dir` and verified). Without it (`verify --at-base`), only the
/// approval's own hex as `at_base_hex` rebuilds it. Reads files, writes none.
pub fn match_approval(
    check: &Check,
    approvals: &[Approval],
    now: &Fingerprint,
    tree_root: &Path,
    blobs_dir: &Path,
    inputs_checked: bool,
) -> ApprovalMatch {
    let valid: Vec<&Approval> = approvals
        .iter()
        .filter(|a| a.approved_at.as_str() >= check.created_at.as_str())
        .collect();
    let Some(newest) = valid.first() else {
        return ApprovalMatch::NoMatch { changed: vec!["fingerprint".to_string()], note: None };
    };
    if !inputs_checked {
        if let Some(a) = valid.iter().find(|a| a.fingerprint == at_base_hex(check, a, now)) {
            return ApprovalMatch::Exact((*a).clone());
        }
        let changed = fingerprint_mismatch(check, newest, now, false)
            .unwrap_or_else(|| vec!["fingerprint".to_string()]);
        return ApprovalMatch::NoMatch { changed, note: None };
    }
    if let Some(a) = valid.iter().find(|a| a.fingerprint == now.hex) {
        return ApprovalMatch::Exact((*a).clone());
    }
    for a in &valid {
        if let Some(lines) = package_info_match(check, a, now, tree_root, blobs_dir) {
            return ApprovalMatch::PackageInfoOnly { approval: (*a).clone(), lines };
        }
    }
    let mut changed = changed_lines(check, newest, now);
    if changed.is_empty() {
        changed.push("fingerprint".to_string());
    }
    ApprovalMatch::NoMatch { note: missing_copy_note(newest, &changed, blobs_dir), changed }
}

/// When every changed line is a Cargo.toml/Cargo.lock input and the newest
/// approval's copy of one of them is not stored (or no longer verifies), the
/// note saying package-info auto-accept could not be tried.
fn missing_copy_note(newest: &Approval, changed: &[String], blobs_dir: &Path) -> Option<String> {
    let paths: Vec<&str> = changed.iter().filter_map(|k| k.strip_prefix("input=")).collect();
    if paths.len() != changed.len() || !paths.iter().all(|p| is_package_file(p)) {
        return None;
    }
    let hashes = input_hashes(&approval_detail(newest));
    let path = paths.into_iter().find(|p| {
        hashes
            .iter()
            .find(|(q, _)| q == p)
            .and_then(|(_, sha)| sha.as_deref())
            .is_some_and(|sha| approved_copy(blobs_dir, sha).is_none())
    })?;
    Some(format!(
        "package-info auto-accept needs the approved copy of {path}, which is not stored \
         (approvals made before this version keep none); approve once more"
    ))
}

/// Keeps a copy of each Cargo.toml / Cargo.lock input as `tree` holds it, so
/// a later package-info-only change can be compared with it. Called only when
/// `tree` matches an approval exactly or is being approved. A failure only
/// warns: without the copy, auto-accept is not tried.
pub fn store_package_copies(paths: &Paths, repo_id: &str, check: &Check, tree: &Path) {
    for p in check.inputs.iter().filter(|p| is_package_file(p)) {
        let file = tree.join(p);
        if !file.is_file() {
            continue;
        }
        if let Err(e) = blobs::put_file(paths, repo_id, &file) {
            masked_eprintln!("thoth: warning: could not keep the approved copy of {p}: {e:#}");
        }
    }
}

/// The approved checks a task's prompt lists: the spec's ids in spec order
/// when it names any (unknown or unapproved ids skipped), otherwise every
/// approved check in `check_id` order.
pub fn applicable_approved(conn: &Connection, spec_ids: &[String]) -> Result<Vec<Check>> {
    let rows = if spec_ids.is_empty() {
        list_checks(conn)?
    } else {
        let mut v = Vec::new();
        for id in spec_ids {
            if let Some(c) = get_check(conn, id)? {
                v.push(c);
            }
        }
        v
    };
    Ok(rows.into_iter().filter(|c| c.approved_at.is_some()).collect())
}

// ---- table access ----------------------------------------------------------

const COLS: &str = "check_id, shell, cmd, cwd, inputs, created_at, approved_at, \
                    approved_fingerprint, approved_detail, approved_commit";

fn row_to_check(r: &rusqlite::Row<'_>) -> rusqlite::Result<Check> {
    let inputs: String = r.get(4)?;
    Ok(Check {
        check_id: r.get(0)?,
        shell: r.get(1)?,
        cmd: r.get(2)?,
        cwd: r.get(3)?,
        inputs: serde_json::from_str(&inputs).unwrap_or_default(),
        created_at: r.get(5)?,
        approved_at: r.get(6)?,
        approved_fingerprint: r.get(7)?,
        approved_detail: r.get(8)?,
        approved_commit: r.get(9)?,
        exe_pin: r.get(10)?,
    })
}

/// The `exe_pin` column, or the literal `'hash'` on a v3 table that has not
/// been migrated yet (`thoth doctor` and `thoth setup` read state.db without
/// migrating it).
fn exe_pin_col(conn: &Connection) -> Result<&'static str> {
    Ok(if crate::db::migrations::column_exists(conn, "checks", "exe_pin")? {
        "exe_pin"
    } else {
        "'hash'"
    })
}

pub fn get_check(conn: &Connection, id: &str) -> Result<Option<Check>> {
    let pin = exe_pin_col(conn)?;
    let sql = format!("SELECT {COLS}, {pin} FROM checks WHERE check_id = ?1");
    Ok(conn.query_row(&sql, [id], row_to_check).optional()?)
}

pub fn list_checks(conn: &Connection) -> Result<Vec<Check>> {
    let pin = exe_pin_col(conn)?;
    let sql = format!("SELECT {COLS}, {pin} FROM checks ORDER BY check_id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], row_to_check)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Whether `check_approvals` exists (state.db v6); `thoth doctor` and `thoth
/// setup` read an older state.db without migrating it.
fn approvals_table(conn: &Connection) -> Result<bool> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='check_approvals'",
        [],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

/// Every approval of `check_id`, newest first. Without `check_approvals`,
/// the `checks` row's own approval (if any) as one `migrated` approval.
pub fn approvals(conn: &Connection, check_id: &str) -> Result<Vec<Approval>> {
    if !approvals_table(conn)? {
        let row = get_check(conn, check_id)?;
        return Ok(row
            .and_then(|c| {
                Some(Approval {
                    approved_at: c.approved_at?,
                    fingerprint: c.approved_fingerprint?,
                    detail: c.approved_detail,
                    approved_commit: c.approved_commit,
                    source: "migrated".to_string(),
                    run_id: None,
                })
            })
            .into_iter()
            .collect());
    }
    let mut stmt = conn.prepare(
        "SELECT approved_at, fingerprint, detail, approved_commit, source, run_id \
         FROM check_approvals WHERE check_id = ?1 ORDER BY seq DESC",
    )?;
    let rows = stmt
        .query_map([check_id], |r| {
            Ok(Approval {
                approved_at: r.get(0)?,
                fingerprint: r.get(1)?,
                detail: r.get(2)?,
                approved_commit: r.get(3)?,
                source: r.get(4)?,
                run_id: r.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Adds one approval of `check_id` and copies it into the `checks` row's
/// `approved_*` columns, which always hold the newest one.
pub fn record_approval(
    conn: &Connection,
    check_id: &str,
    at: &str,
    fp: &Fingerprint,
    commit: &str,
    source: &str,
    run_id: Option<&str>,
) -> Result<()> {
    let detail = serde_json::to_string(&fp.detail)?;
    conn.execute(
        "INSERT INTO check_approvals \
           (check_id, approved_at, fingerprint, detail, approved_commit, source, run_id) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![check_id, at, fp.hex, detail, commit, source, run_id],
    )?;
    conn.execute(
        "UPDATE checks SET approved_at = ?2, approved_fingerprint = ?3, approved_detail = ?4, \
         approved_commit = ?5 WHERE check_id = ?1",
        params![check_id, at, fp.hex, detail, commit],
    )?;
    Ok(())
}

/// Forgets every approval of `check_id`.
fn clear_approvals(conn: &Connection, check_id: &str) -> Result<()> {
    if approvals_table(conn)? {
        conn.execute("DELETE FROM check_approvals WHERE check_id = ?1", [check_id])?;
    }
    Ok(())
}

/// Inserts, or replaces the whole row - which also forgets every approval,
/// even when the new definition is identical.
pub fn put_check(conn: &Connection, c: &Check) -> Result<()> {
    clear_approvals(conn, &c.check_id)?;
    conn.execute(
        "INSERT OR REPLACE INTO checks (check_id, shell, cmd, cwd, inputs, created_at, exe_pin) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            c.check_id,
            c.shell,
            c.cmd,
            c.cwd,
            serde_json::to_string(&c.inputs)?,
            c.created_at,
            c.exe_pin
        ],
    )?;
    Ok(())
}

/// Deletes the row and every approval of it.
pub fn delete_check(conn: &Connection, id: &str) -> Result<bool> {
    clear_approvals(conn, id)?;
    Ok(conn.execute("DELETE FROM checks WHERE check_id = ?1", [id])? > 0)
}

/// One warning per id a spec lists that the catalogue does not hold or that
/// nobody approved yet. Never an error: the human can approve after the task
/// is created.
pub fn unapproved_warnings(conn: &Connection, ids: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for id in ids {
        let approved = get_check(conn, id)?.map(|c| c.approved_at.is_some()).unwrap_or(false);
        if !approved {
            out.push(format!(
                "check {id} is not approved yet; approve it with: thoth checks approve --reason \"WHY\""
            ));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    fn check(cmd: &str, inputs: &[&str]) -> Check {
        Check {
            check_id: "t".into(),
            shell: "argv".into(),
            cmd: cmd.into(),
            cwd: ".".into(),
            inputs: inputs.iter().map(|s| s.to_string()).collect(),
            created_at: "n".into(),
            approved_at: None,
            approved_fingerprint: None,
            approved_detail: None,
            approved_commit: None,
            exe_pin: "hash".into(),
        }
    }

    /// An approval of `fp`, as `checks approve` in the main tree records it.
    fn approval_of(fp: &Fingerprint) -> Approval {
        Approval {
            approved_at: "n".into(),
            fingerprint: fp.hex.clone(),
            detail: Some(fp.detail.to_string()),
            approved_commit: None,
            source: "main".into(),
            run_id: None,
        }
    }

    #[test]
    fn check_ids_follow_the_documented_alphabet_and_length() {
        for ok in ["test", "clippy-all", "a_b", "0", &"a".repeat(32)] {
            assert!(valid_check_id(ok), "{ok}");
        }
        for bad in ["Test", "a b", "", "a.b", &"a".repeat(33), "é"] {
            assert!(!valid_check_id(bad), "{bad}");
        }
    }

    #[test]
    fn normalize_rel_keeps_relative_paths_and_refuses_escapes() {
        assert_eq!(normalize_rel("./a\\b//c").as_deref(), Some("a/b/c"));
        assert_eq!(normalize_rel(".").as_deref(), Some("."));
        assert_eq!(normalize_rel("a/../b"), None);
        assert_eq!(normalize_rel("/etc"), None);
    }

    #[test]
    fn the_toolchain_table_lists_every_documented_file() {
        let find = |name: &str| {
            TOOLCHAIN_INPUTS
                .iter()
                .find(|(names, _)| names.contains(&name))
                .map(|(_, files)| files.to_vec())
                .unwrap()
        };
        assert_eq!(
            find("cargo"),
            ["Cargo.toml", "Cargo.lock", "build.rs", ".cargo/config.toml", "rust-toolchain.toml", "rust-toolchain"]
        );
        for n in ["npm", "npx", "pnpm", "yarn", "node"] {
            assert_eq!(find(n), ["package.json", "package-lock.json", "pnpm-lock.yaml", "yarn.lock"]);
        }
        for n in ["python", "python3", "py", "pytest"] {
            assert_eq!(
                find(n),
                ["pyproject.toml", "setup.cfg", "setup.py", "pytest.ini", "tox.ini", "conftest.py", "requirements.txt"]
            );
        }
        assert_eq!(find("go"), ["go.mod", "go.sum"]);
        assert_eq!(
            find("dotnet"),
            ["global.json", "Directory.Build.props", "Directory.Build.targets", "NuGet.config"]
        );
        assert_eq!(DOTNET_PROJECT_EXTENSIONS, ["sln", "csproj", "fsproj"]);
    }

    #[test]
    fn every_table_row_is_inferred_for_its_executables() {
        for (names, files) in TOOLCHAIN_INPUTS {
            let dir = tempfile::tempdir().unwrap();
            for f in *files {
                write(dir.path(), f, "x");
            }
            let mut want: Vec<String> = files.iter().map(|s| s.to_string()).collect();
            want.sort();
            for n in *names {
                // upper case and `.exe` must not matter
                let cmd = format!("{}.EXE arg", n.to_uppercase());
                assert_eq!(infer_inputs(dir.path(), ".", "argv", &cmd), want, "{n}");
            }
        }
    }

    #[test]
    fn dotnet_adds_the_project_files_in_its_cwd() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.sln", "x");
        write(dir.path(), "b.csproj", "x");
        write(dir.path(), "c.fsproj", "x");
        write(dir.path(), "d.txt", "x");
        assert_eq!(
            infer_inputs(dir.path(), ".", "argv", "dotnet test"),
            ["a.sln", "b.csproj", "c.fsproj"]
        );
    }

    #[test]
    fn inference_takes_cargo_files_that_exist_and_not_the_ones_that_do_not() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "x");
        write(dir.path(), "Cargo.lock", "x");
        assert_eq!(infer_inputs(dir.path(), ".", "argv", "cargo test"), ["Cargo.lock", "Cargo.toml"]);
    }

    #[test]
    fn a_word_naming_a_file_under_the_cwd_is_an_input() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "scripts/test_public_tree.py", "x");
        write(dir.path(), "crates/core/t.sh", "x");
        assert_eq!(
            infer_inputs(dir.path(), ".", "argv", "python scripts/test_public_tree.py --flag"),
            ["scripts/test_public_tree.py"]
        );
        assert_eq!(
            infer_inputs(dir.path(), ".", "argv", "sh \"crates/core/t.sh\" nope.txt"),
            ["crates/core/t.sh"]
        );
        // relative to the cwd, reported relative to the repo root
        assert_eq!(infer_inputs(dir.path(), "crates/core", "argv", "sh t.sh"), ["crates/core/t.sh"]);
        // a directory is not a regular file
        assert!(infer_inputs(dir.path(), ".", "argv", "ls scripts").is_empty());
    }

    #[test]
    fn a_workspace_root_lockfile_is_found_from_a_nested_cwd() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.lock", "x");
        write(dir.path(), "crates/core/Cargo.toml", "x");
        assert_eq!(
            infer_inputs(dir.path(), "crates/core", "argv", "cargo build"),
            ["Cargo.lock", "crates/core/Cargo.toml"]
        );
    }

    #[test]
    fn the_fingerprint_is_stable_and_follows_the_documented_line_layout() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "in.txt", "hello");
        let c = check("nosuchexe-zz --x", &["in.txt"]);
        let a = fingerprint(&c, dir.path()).unwrap();
        let b = fingerprint(&c, dir.path()).unwrap();
        assert_eq!(a.hex, b.hex);
        assert_eq!(a.lines[0], "shell=argv");
        assert_eq!(a.lines[1], "cmd=nosuchexe-zz --x");
        assert_eq!(a.lines[2], "cwd=.");
        assert_eq!(a.lines[3], "exe=nosuchexe-zz|none");
        assert_eq!(
            a.lines[4],
            format!("input=in.txt|{}", ids::sha256_hex(b"hello"))
        );
        let mut text = a.lines.join("\n");
        text.push('\n');
        assert_eq!(a.hex, ids::sha256_hex(text.as_bytes()));
        assert_eq!(a.detail["exe"]["sha256"], Value::Null);
        assert_eq!(a.detail["inputs"][0]["path"], "in.txt");
    }

    #[test]
    fn one_changed_byte_or_another_shell_changes_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "in.txt", "hello");
        let c = check("nosuchexe-zz", &["in.txt"]);
        let before = fingerprint(&c, dir.path()).unwrap();
        write(dir.path(), "in.txt", "hellp");
        let after = fingerprint(&c, dir.path()).unwrap();
        assert_ne!(before.hex, after.hex);
        assert_eq!(changed_lines(&c, &approval_of(&before), &after), ["input=in.txt"]);

        let mut other = c.clone();
        other.shell = "sh".into();
        assert_ne!(after.hex, fingerprint(&other, dir.path()).unwrap().hex);
    }

    #[test]
    fn a_missing_input_is_fingerprinted_as_missing_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = check("nosuchexe-zz", &["gone.txt"]);
        let f = fingerprint(&c, dir.path()).unwrap();
        assert_eq!(f.lines[4], "input=gone.txt|missing");
        assert_eq!(f.detail["inputs"][0]["sha256"], Value::Null);
    }

    #[test]
    fn a_word_with_a_separator_resolves_relative_to_the_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let name = if cfg!(windows) { "tool.cmd" } else { "tool.sh" };
        write(dir.path(), &format!("sub/bin/{name}"), "#!/bin/sh\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join(format!("sub/bin/{name}"));
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut c = check(&format!("./bin/{name}"), &[]);
        c.cwd = "sub".into();
        let f = fingerprint(&c, dir.path()).unwrap();
        assert_eq!(f.detail["exe"]["path"], format!("sub/bin/{name}"));
        assert_eq!(f.detail["exe"]["sha256"], ids::sha256_hex(b"#!/bin/sh\n"));
        // the same file under another tree root fingerprints alike
        let other = tempfile::tempdir().unwrap();
        write(other.path(), &format!("sub/bin/{name}"), "#!/bin/sh\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = other.path().join(format!("sub/bin/{name}"));
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(f.hex, fingerprint(&c, other.path()).unwrap().hex);
    }

    #[test]
    fn an_in_tree_executable_and_its_input_fingerprint_alike_in_two_roots() {
        let name = if cfg!(windows) { "tool.cmd" } else { "tool.sh" };
        let make = || {
            let d = tempfile::tempdir().unwrap();
            write(d.path(), &format!("bin/{name}"), "run
");
            write(d.path(), "data/in.txt", "same");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let p = d.path().join(format!("bin/{name}"));
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            d
        };
        let (a, b) = (make(), make());
        let c = check(&format!("./bin/{name}"), &["data/in.txt"]);
        let fa = fingerprint(&c, a.path()).unwrap();
        let fb = fingerprint(&c, b.path()).unwrap();
        assert_eq!(fa.hex, fb.hex);
        assert_eq!(fa.lines[3], fb.lines[3]);
        assert!(fa.lines[3].starts_with(&format!("exe=bin/{name}|")));
    }

    /// AC11 (e): the hex of an `argv`, `sh` or `bash` check must never change
    /// (existing approvals stay valid). Fixed inputs, known hexes, computed
    /// independently of this code (sha256 of the documented line layout).
    #[test]
    fn argv_sh_and_bash_fingerprints_keep_their_known_hex() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bin/tool.exe", "run\n");
        write(dir.path(), "data/in.txt", "same");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join("bin/tool.exe");
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        for (shell, hex) in [
            ("argv", "ec2d4e7cfbc467e2977d2761312fec3c9af6c4a1144196d112e326477b95e6b8"),
            ("sh", "89e2209a3edafdb8d686c3976a9ddd09515b73c2966548363a3c2dea7a8d002f"),
            ("bash", "be7e28529ee24bdf9b728d1a35406d4459e692b1be5d711fc3a77ff6278d57ec"),
        ] {
            let mut c = check("./bin/tool.exe --v", &["data/in.txt"]);
            c.shell = shell.into();
            assert_eq!(fingerprint(&c, dir.path()).unwrap().hex, hex, "{shell}");
        }
    }

    /// AC11 (b), (c): the in-directory order cmd.exe and PowerShell were
    /// probed to use, and the in-tree naming rule.
    #[test]
    fn cmd_and_powershell_resolve_a_program_in_their_own_order() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["tool", "tool.cmd", "tool.bat"] {
            write(dir.path(), f, "x");
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join("tool");
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let hit = |shell: &str, token: &str| {
            resolve_exe_for(shell, token, dir.path(), ".")
                .map(|p| p.file_name().unwrap().to_string_lossy().to_lowercase())
        };
        // cmd: never the extensionless file; .bat before .cmd
        assert_eq!(hit("cmd", "./tool").as_deref(), Some("tool.bat"));
        write(dir.path(), "tool.com", "x");
        assert_eq!(hit("cmd", "./tool").as_deref(), Some("tool.com"));
        // written with its extension: exactly that file
        assert_eq!(hit("cmd", "./tool.cmd").as_deref(), Some("tool.cmd"));
        // powershell: .ps1 first, the extensionless file last
        write(dir.path(), "tool.ps1", "x");
        assert_eq!(hit("powershell", "./tool").as_deref(), Some("tool.ps1"));
        // argv keeps P7-1a's lookup
        assert_eq!(hit("argv", "./tool").as_deref(), Some("tool"));

        assert!(shell_program_refusal("cmd", "./tool", dir.path(), ".").unwrap().contains("extension"));
        assert!(shell_program_refusal("cmd", "./tool.ps1", dir.path(), ".").is_some());
        assert!(shell_program_refusal("powershell", "./tool.ps1", dir.path(), ".").is_none());
        assert!(shell_program_refusal("cmd", "./tool.cmd", dir.path(), ".").is_none());
        assert!(shell_program_refusal("cmd", "./gone.exe", dir.path(), ".").unwrap().contains("does not exist"));
        // a bare name, or another shell, is not this rule's business
        assert!(shell_program_refusal("cmd", "hostname", dir.path(), ".").is_none());
        assert!(shell_program_refusal("argv", "./tool", dir.path(), ".").is_none());
    }

    /// AC12 (a): empty and relative PATH entries are dropped, the absolute
    /// ones kept in order.
    #[test]
    fn search_dirs_keeps_only_the_absolute_entries_in_order() {
        let (a, b, c) = if cfg!(windows) {
            (r"C:\tools", r"C:\Windows\System32", r"D:\bin")
        } else {
            ("/usr/local/bin", "/usr/bin", "/opt/bin")
        };
        let sep = if cfg!(windows) { ";" } else { ":" };
        let mixed = format!(".{sep}{a}{sep}{sep}rel/dir{sep}{b}{sep}..{sep}{c}{sep}bin");
        assert_eq!(
            search_dirs(Some(std::ffi::OsStr::new(&mixed))),
            [PathBuf::from(a), PathBuf::from(b), PathBuf::from(c)]
        );
        assert!(search_dirs(None).is_empty());
    }

    /// AC12 (a): a same-named file reached through a relative PATH entry is
    /// not what the fingerprint pins; the later absolute directory's file is,
    /// because that is the one the child (whose PATH drops relative entries)
    /// runs.
    #[test]
    fn resolve_exe_skips_a_relative_path_entry_for_the_absolute_one() {
        // A relative entry is read against the process's cwd; make one there.
        let rel = format!("target/ac12-rel-{}", std::process::id());
        let name = "ac12tool.exe";
        let rel_file = Path::new(&rel).join(name);
        std::fs::create_dir_all(&rel).unwrap();
        std::fs::write(&rel_file, "decoy").unwrap();
        let abs = tempfile::tempdir().unwrap();
        write(abs.path(), name, "pinned");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for p in [rel_file.clone(), abs.path().join(name)] {
                std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        let path = std::env::join_paths([PathBuf::from(&rel), abs.path().to_path_buf()]).unwrap();
        let hit = resolve_exe_on(name, abs.path(), ".", Some(&path));
        let _ = std::fs::remove_dir_all(&rel);
        assert_eq!(hit, Some(abs.path().join(name)));
    }

    /// AC12 (b): each character a shell would interpret in the program word
    /// is refused; plain words (with a Windows backslash for cmd) are not.
    #[test]
    fn a_shell_program_word_must_be_plain() {
        for (shell, cmd) in [
            ("cmd", r#""my tool" arg"#),
            ("powershell", "'my tool' arg"),
            ("sh", "`tool` arg"),
            ("bash", "$TOOL arg"),
            ("sh", r"my\ tool arg"),
            ("bash", r"\tool"),
            ("sh", r#"./my"tool" arg"#),
            ("cmd", "tool$ arg"),
        ] {
            let err = program_word_refusal(shell, cmd).unwrap_or_else(|| panic!("{shell}: {cmd}"));
            assert!(err.contains("the program name must be a plain word"), "{err}");
            assert!(err.contains("argv"), "{err}");
        }
        for (shell, cmd) in [
            ("cmd", "cargo test"),
            ("sh", "cargo test"),
            ("cmd", r".\tool.exe arg"),
            ("powershell", r".\tool.exe arg"),
            ("sh", "./tool arg"),
            ("bash", "./tool 'quoted arg' \"x\" $HOME"),
            // argv has no shell to misread the word
            ("argv", r#""my tool" arg"#),
        ] {
            assert_eq!(program_word_refusal(shell, cmd), None, "{shell}: {cmd}");
        }
    }

    /// AC14 (g)(1): the shell-check grammar, as one table with a shell column.
    /// Every negative is run for all four shells; quotes sit outside the first
    /// word, where AC12 (b) would refuse them first.
    #[test]
    fn a_shell_check_is_one_program_and_plain_arguments() {
        let accepted: &[(&str, &str)] = &[
            ("cmd", "cargo test"),
            ("powershell", "cargo test"),
            ("sh", "cargo test"),
            ("bash", "cargo test"),
            ("cmd", "cargo clippy --all-targets -- -D warnings"),
            ("cmd", r".\tool.cmd --flag=1"),
            ("cmd", r"C:\tools\x.exe /c"),
            ("powershell", r".\check.ps1 a b"),
            ("sh", "./check.sh a b"),
            ("bash", "./check.sh a b"),
            // argv has no shell; the grammar is not its business
            ("argv", r#""./my tool.exe" a|b"#),
        ];
        for (shell, cmd) in accepted {
            assert_eq!(command_refusal(shell, cmd), None, "{shell}: {cmd}");
        }

        let mut refused: Vec<String> = [
            '&', '|', ';', '<', '>', '^', '%', '!', '$', '`', '(', ')', '[', ']', '{', '}', '*',
            '?', '"', '\'', '~', '#', '@', ',', '+', '\n', 'é',
        ]
        .iter()
        .map(|c| format!("tool a{c}b"))
        .collect();
        refused.extend([
            r#"tool "a b""#.to_string(),
            "tool 'a b'".to_string(),
            "FOO=bar tool".to_string(),
            "   ".to_string(),
        ]);
        for shell in ["cmd", "powershell", "sh", "bash"] {
            for cmd in &refused {
                let err = command_refusal(shell, cmd).unwrap_or_else(|| panic!("{shell}: {cmd:?}"));
                assert!(err.contains(NOT_ONE_PROGRAM), "{shell}: {cmd:?}: {err}");
                assert!(err.contains("script"), "{err}");
                assert!(err.contains("argv"), "{err}");
            }
        }
        // the refusal names the first character it does not allow
        let err = shell_grammar_refusal("cmd", r".\to^ol.cmd a&b").unwrap();
        assert!(err.contains("'^'"), "{err}");
        // AC12 (b) still answers first for the program word
        let err = command_refusal("cmd", r#""my tool" a"#).unwrap();
        assert!(err.contains("the program name must be a plain word"), "{err}");
    }

    /// AC14 (c), (d): what the resolved file must be for each shell.
    #[test]
    fn a_resolved_program_must_exist_and_be_a_file_its_shell_runs_itself() {
        let p = |s: &str| PathBuf::from(s);
        let not_found = resolved_program_refusal("sh", "exit", None).unwrap();
        assert!(not_found.contains("program not found"), "{not_found}");
        assert!(resolved_program_refusal("cmd", "call", None).unwrap().contains("program not found"));
        for (shell, file, ok) in [
            ("cmd", r"C:\t\x.exe", true),
            ("cmd", r"C:\t\x.COM", true),
            ("cmd", r"C:\t\x.cmd", true),
            ("cmd", r"C:\t\x.bat", true),
            ("cmd", r"C:\t\x.vbs", false),
            ("cmd", r"C:\t\x.ps1", false),
            ("cmd", r"C:\t\x", false),
            ("powershell", r"C:\t\x.exe", true),
            ("powershell", r"C:\t\x.com", true),
            ("powershell", r"C:\t\x.PS1", true),
            ("powershell", r"C:\t\x", false),
            ("powershell", r"C:\t\x.js", false),
            ("powershell", r"C:\t[1]\x.ps1", false),
            ("powershell", r"C:\t`1\x.ps1", false),
            ("sh", "/usr/bin/x", true),
            ("bash", "/opt/t[1]/x", true),
            ("argv", r"C:\t\x", true),
        ] {
            assert_eq!(resolved_program_refusal(shell, "x", Some(&p(file))).is_none(), ok, "{shell}: {file}");
        }
        for file in [r"C:\t\x.cmd", r"C:\t\x.BAT"] {
            let err = powershell_batch_refusal("powershell", Some(&p(file))).unwrap();
            assert!(err.contains("use shell cmd for this check"), "{err}");
            assert!(powershell_batch_refusal("cmd", Some(&p(file))).is_none());
        }
        assert!(powershell_batch_refusal("powershell", Some(&p(r"C:\t\x.ps1"))).is_none());
    }

    /// AC13 (a): an argv check's program is the first element of `split_argv`,
    /// so a quoted path with a space is one program for the fingerprint too.
    #[test]
    fn an_argv_program_in_quotes_is_the_whole_quoted_path() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "my tool.exe", "run\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join("my tool.exe");
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let cmd = r#""./my tool.exe" --version"#;
        assert_eq!(program_word("argv", cmd), "./my tool.exe");
        let fp = fingerprint(&check(cmd, &[]), dir.path()).unwrap();
        assert_eq!(fp.detail["exe"]["path"], "my tool.exe", "{:#}", fp.detail);
        assert!(fp.detail["exe"]["sha256"].is_string(), "{:#}", fp.detail);
    }

    /// AC13 (a): a double quote that is never closed is refused for argv.
    #[test]
    fn an_argv_check_with_an_unclosed_quote_is_refused() {
        let err = program_word_refusal("argv", r#""my tool.exe"#).expect("an unclosed quote");
        assert!(err.contains("not closed"), "{err}");
        assert!(program_word_refusal("argv", r#"tool "a b"#).is_some());
        assert_eq!(program_word_refusal("argv", r#""my tool.exe" "a b""#), None);
    }

    /// AC13: the program written with or without quotes pins the same `exe=`
    /// line; a quoted input path with a space is inferred as one file.
    #[test]
    fn an_argv_program_pins_the_same_exe_line_quoted_or_not() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bin/tool.exe", "run\n");
        write(dir.path(), "data dir/in.txt", "x");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join("bin/tool.exe");
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let exe_line = |cmd: &str| {
            let fp = fingerprint(&check(cmd, &[]), dir.path()).unwrap();
            fp.lines.into_iter().find(|l| l.starts_with("exe=")).unwrap()
        };
        let plain = exe_line("./bin/tool.exe --version");
        assert!(plain.starts_with("exe=bin/tool.exe|"), "{plain}");
        assert_eq!(exe_line(r#""./bin/tool.exe" --version"#), plain);
        assert_eq!(
            infer_inputs(dir.path(), ".", "argv", r#"./bin/tool.exe "data dir/in.txt""#),
            ["bin/tool.exe", "data dir/in.txt"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_path_word_without_the_execute_bit_is_not_an_executable() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "run.sh", "x");
        assert!(resolve_exe("./run.sh", dir.path(), ".").is_none());
    }

    /// P7-1b / AC3: a full run compares the whole hex; a baseline run ignores
    /// `input=` lines but still sees an `exe=` change.
    #[test]
    fn fingerprint_mismatch_checks_inputs_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "in.txt", "hello");
        let c = check("nosuchexe-zz", &["in.txt"]);
        let at_approval = fingerprint(&c, dir.path()).unwrap();
        let a = approval_of(&at_approval);
        assert!(fingerprint_mismatch(&c, &a, &at_approval, true).is_none());

        write(dir.path(), "in.txt", "hellp");
        let edited = fingerprint(&c, dir.path()).unwrap();
        assert_eq!(fingerprint_mismatch(&c, &a, &edited, true), Some(vec!["input=in.txt".to_string()]));
        assert!(fingerprint_mismatch(&c, &a, &edited, false).is_none(), "inputs are not checked at base");

        // an exe that now resolves in the tree is a mismatch either way
        let name = if cfg!(windows) { "tool.cmd" } else { "tool.sh" };
        let d = check(&format!("./bin/{name}"), &[]);
        let before = fingerprint(&d, dir.path()).unwrap();
        let b = approval_of(&before);
        write(dir.path(), &format!("bin/{name}"), "x");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join(format!("bin/{name}"));
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let after = fingerprint(&d, dir.path()).unwrap();
        let at_base = fingerprint_mismatch(&d, &b, &after, false).expect("exe changed");
        assert_eq!(at_base, [format!("exe=./bin/{name}"), format!("exe=bin/{name}")]);
        assert!(fingerprint_mismatch(&d, &b, &after, true).is_some());
    }

    /// AC10 (4): at base the approval's own hex is compared - a stored
    /// fingerprint that no longer matches (one character changed) refuses,
    /// and so does a row whose cmd changed after approval.
    #[test]
    fn at_base_compares_the_approved_hex_not_only_the_rebuilt_lines() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "in.txt", "hello");
        let c = check("nosuchexe-zz", &["in.txt"]);
        let fp = fingerprint(&c, dir.path()).unwrap();
        let a = approval_of(&fp);
        assert!(fingerprint_mismatch(&c, &a, &fp, false).is_none());

        let mut tampered = a.clone();
        let flipped = if fp.hex.starts_with('0') { "1" } else { "0" };
        tampered.fingerprint = format!("{flipped}{}", &fp.hex[1..]);
        assert_eq!(
            fingerprint_mismatch(&c, &tampered, &fp, false),
            Some(vec!["fingerprint".to_string()])
        );

        let mut edited = c.clone();
        edited.cmd = "nosuchexe-zz --other".into();
        let now = fingerprint(&edited, dir.path()).unwrap();
        assert!(fingerprint_mismatch(&edited, &a, &now, false).is_some());
    }

    #[test]
    fn applicable_approved_follows_the_spec_or_takes_every_approved_check() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        let fp = Fingerprint { hex: "h".into(), lines: vec![], detail: json!({}), program: None };
        for id in ["a", "b", "c"] {
            let mut c = check("x", &[]);
            c.check_id = id.into();
            put_check(&conn, &c).unwrap();
        }
        record_approval(&conn, "a", "now", &fp, "abc", "main", None).unwrap();
        record_approval(&conn, "c", "now", &fp, "abc", "main", None).unwrap();
        let ids = |v: Vec<Check>| v.into_iter().map(|c| c.check_id).collect::<Vec<_>>();
        assert_eq!(ids(applicable_approved(&conn, &[]).unwrap()), ["a", "c"]);
        let spec: Vec<String> = ["c", "b", "zz", "a"].iter().map(|s| s.to_string()).collect();
        assert_eq!(ids(applicable_approved(&conn, &spec).unwrap()), ["c", "a"]);
    }

    #[test]
    fn the_catalogue_replaces_a_row_and_clears_its_approval() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        let c = check("x", &[]);
        put_check(&conn, &c).unwrap();
        let fp = Fingerprint { hex: "h".into(), lines: vec![], detail: json!({}), program: None };
        record_approval(&conn, "t", "now", &fp, "abc", "main", None).unwrap();
        assert_eq!(get_check(&conn, "t").unwrap().unwrap().approved_fingerprint.as_deref(), Some("h"));
        put_check(&conn, &c).unwrap();
        let back = get_check(&conn, "t").unwrap().unwrap();
        assert!(back.approved_at.is_none() && back.approved_commit.is_none());
        assert!(delete_check(&conn, "t").unwrap());
        assert!(!delete_check(&conn, "t").unwrap());
    }

    /// `thoth doctor` and `thoth setup` read state.db without migrating it: a
    /// v3 `checks` table (no `exe_pin` column) still reads, as `hash`.
    #[test]
    fn a_v3_checks_table_without_exe_pin_reads_as_hash() {
        use crate::db::migrations;
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(migrations::STATE_V1).unwrap();
        migrations::migrate_state_v1_to_v2(&conn).unwrap();
        migrations::migrate_state_v2_to_v3(&conn).unwrap();
        conn.execute(
            "INSERT INTO checks (check_id, shell, cmd, cwd, inputs, created_at) \
             VALUES ('t','argv','x','.','[]','n')",
            [],
        )
        .unwrap();
        assert!(!migrations::column_exists(&conn, "checks", "exe_pin").unwrap());
        let all = list_checks(&conn).unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].exe_pin, "hash");
        assert_eq!(get_check(&conn, "t").unwrap().unwrap().exe_pin, "hash");

        migrations::migrate_state_v3_to_v4(&conn).unwrap();
        assert_eq!(list_checks(&conn).unwrap()[0].exe_pin, "hash");
        assert_eq!(get_check(&conn, "t").unwrap().unwrap().exe_pin, "hash");
    }

    #[test]
    fn a_path_pinned_check_ignores_the_programs_bytes_but_not_its_path() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "bin/tool.exe", "run\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let p = dir.path().join("bin/tool.exe");
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let by_hash = check("./bin/tool.exe --v", &[]);
        let mut by_path = by_hash.clone();
        by_path.exe_pin = "path".into();

        let p1 = fingerprint(&by_path, dir.path()).unwrap();
        assert_eq!(p1.lines[3], "exe=bin/tool.exe|path-only");
        assert_eq!(p1.detail["exe"]["sha256"], Value::Null);
        assert_eq!(p1.detail["exe"]["pin"], "path");
        let h1 = fingerprint(&by_hash, dir.path()).unwrap();

        write(dir.path(), "bin/tool.exe", "changed\n");
        let p2 = fingerprint(&by_path, dir.path()).unwrap();
        assert_eq!(p1.hex, p2.hex);
        let h2 = fingerprint(&by_hash, dir.path()).unwrap();
        assert_ne!(h1.hex, h2.hex);

        // the switch itself is part of the line
        assert_eq!(changed_lines(&by_path, &approval_of(&h1), &p2), ["exe=bin/tool.exe"]);
    }

    #[test]
    fn an_old_approval_detail_without_pin_rebuilds_the_same_lines() {
        let detail: Value = serde_json::from_str(
            r#"{"exe":{"path":"bin/tool.exe","sha256":"abc"},"inputs":[{"path":"in.txt","sha256":"def"}]}"#,
        )
        .unwrap();
        assert_eq!(
            lines_from("argv", "./bin/tool.exe", ".", &detail),
            ["shell=argv", "cmd=./bin/tool.exe", "cwd=.", "exe=bin/tool.exe|abc", "input=in.txt|def"]
        );
        let mut pinned = detail.clone();
        pinned["exe"]["sha256"] = Value::Null;
        pinned["exe"]["pin"] = json!("path");
        assert_eq!(
            lines_from("argv", "./bin/tool.exe", ".", &pinned)[3],
            "exe=bin/tool.exe|path-only"
        );
    }

    #[test]
    fn inside_tree_compares_whole_components_and_canonical_paths() {
        let base = tempfile::tempdir().unwrap();
        let t = base.path().join("t");
        write(&t, "bin/x", "x");
        let x = t.join("bin").join("x");
        assert!(inside_tree(&x, &t));
        // a tree that does not exist is compared lexically
        assert!(!inside_tree(&x, &t.join("other")));
        // the prefix trap: `t2` is not inside `t`
        let t2 = base.path().join(format!("{}2", t.file_name().unwrap().to_string_lossy()));
        std::fs::create_dir_all(&t2).unwrap();
        assert!(!inside_tree(&x, &t2));
        #[cfg(windows)]
        {
            let upper = PathBuf::from(t.display().to_string().to_uppercase());
            assert!(inside_tree(&x, &upper));
            let slashed = PathBuf::from(t.display().to_string().replace('\\', "/"));
            assert!(inside_tree(&x, &slashed));
        }
    }

    fn fp_named(hex: &str) -> Fingerprint {
        Fingerprint { hex: hex.into(), lines: vec![], detail: json!({}), program: None }
    }

    #[test]
    fn approvals_are_newest_first_and_cleared_by_put_and_delete() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        let c = check("x", &[]);
        put_check(&conn, &c).unwrap();
        record_approval(&conn, "t", "2026-01-01T00:00:00Z", &fp_named("h1"), "c1", "main", None).unwrap();
        record_approval(&conn, "t", "2026-01-02T00:00:00Z", &fp_named("h2"), "c2", "run", Some("R1")).unwrap();
        let all = approvals(&conn, "t").unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            all[0],
            Approval {
                approved_at: "2026-01-02T00:00:00Z".into(),
                fingerprint: "h2".into(),
                detail: Some("{}".into()),
                approved_commit: Some("c2".into()),
                source: "run".into(),
                run_id: Some("R1".into()),
            }
        );
        assert_eq!(all[1].fingerprint, "h1");
        assert_eq!(all[1].source, "main");
        assert_eq!(all[1].run_id, None);
        let row = get_check(&conn, "t").unwrap().unwrap();
        assert_eq!(row.approved_fingerprint.as_deref(), Some("h2"));
        assert_eq!(row.approved_at.as_deref(), Some("2026-01-02T00:00:00Z"));

        // the very same definition added again forgets every approval
        put_check(&conn, &c).unwrap();
        assert!(approvals(&conn, "t").unwrap().is_empty());
        assert_eq!(get_check(&conn, "t").unwrap().unwrap().approved_at, None);

        record_approval(&conn, "t", "2026-01-03T00:00:00Z", &fp_named("h3"), "c3", "main", None).unwrap();
        assert_eq!(approvals(&conn, "t").unwrap().len(), 1);
        assert!(delete_check(&conn, "t").unwrap());
        assert!(approvals(&conn, "t").unwrap().is_empty());
    }

    #[test]
    fn approvals_fall_back_to_the_checks_row_without_the_table() {
        use crate::db::migrations;
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(migrations::STATE_V1).unwrap();
        migrations::migrate_state_v1_to_v2(&conn).unwrap();
        migrations::migrate_state_v2_to_v3(&conn).unwrap();
        migrations::migrate_state_v3_to_v4(&conn).unwrap();
        migrations::migrate_state_v4_to_v5(&conn).unwrap();
        conn.execute(
            "INSERT INTO checks (check_id, shell, cmd, cwd, inputs, created_at, approved_at, \
             approved_fingerprint, approved_detail, approved_commit) \
             VALUES ('t','argv','x','.','[]','n','2026-01-01T00:00:00Z','fp','{}','abc')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO checks (check_id, shell, cmd, cwd, inputs, created_at) \
             VALUES ('p','argv','x','.','[]','n')",
            [],
        )
        .unwrap();
        let all = approvals(&conn, "t").unwrap();
        assert_eq!(
            all,
            [Approval {
                approved_at: "2026-01-01T00:00:00Z".into(),
                fingerprint: "fp".into(),
                detail: Some("{}".into()),
                approved_commit: Some("abc".into()),
                source: "migrated".into(),
                run_id: None,
            }]
        );
        assert!(approvals(&conn, "p").unwrap().is_empty());
        assert!(approvals(&conn, "nope").unwrap().is_empty());
    }

    const TOML: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\
        rust-version = \"1.80\"\nbuild = \"build.rs\"\nlinks = \"foo\"\ndescription = \"demo\"\n\
        keywords = [\"a\", \"b\"]\n\n[dependencies]\nserde = \"1.0.1\"\n\n[dependencies.extra]\n\
        version = \"2.0.0\"\n\n[workspace.package]\nversion = \"9.0.0\"\n";

    const LOCK: &str = "# This file is automatically @generated by Cargo.\nversion = 4\n\n\
        [[package]]\nname = \"demo\"\nversion = \"0.1.0\"\ndependencies = [\n \"serde\",\n]\n\n\
        [[package]]\nname = \"serde\"\nversion = \"1.0.1\"\n\
        source = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"abc\"\n";

    /// `text` with the first `from` replaced by `to`; `from` must be there.
    fn swap(text: &str, from: &str, to: &str) -> String {
        assert!(text.contains(from), "{from:?}");
        text.replacen(from, to, 1)
    }

    /// The 1-based number of the first line of `text` that is exactly `line`.
    fn line_no(text: &str, line: &str) -> usize {
        text.split('\n').position(|l| l == line).unwrap() + 1
    }

    fn crlf(text: &str) -> String {
        text.replace('\n', "\r\n")
    }

    #[test]
    fn package_info_only_accepts_package_lines_and_local_lock_versions() {
        let pl = |path: &str, line: usize, approved: &str, now: &str| PackageLine {
            path: path.into(),
            line,
            approved: approved.into(),
            now: now.into(),
        };
        // (a) version and description of [package]
        let now = swap(&swap(TOML, "version = \"0.1.0\"", "version = \"0.1.1\""), "description = \"demo\"", "description = \"demo two\"");
        assert_eq!(
            package_info_only("Cargo.toml", TOML.as_bytes(), now.as_bytes()),
            Some(vec![
                pl("Cargo.toml", line_no(TOML, "version = \"0.1.0\""), "version = \"0.1.0\"", "version = \"0.1.1\""),
                pl("Cargo.toml", line_no(TOML, "description = \"demo\""), "description = \"demo\"", "description = \"demo two\""),
            ])
        );
        // a nested path and blanks around `=` are fine too
        let now = swap(TOML, "version = \"0.1.0\"", "\tversion\t=  \"0.2.0\" ");
        assert_eq!(package_info_only("crates/a/Cargo.toml", TOML.as_bytes(), now.as_bytes()).unwrap().len(), 1);
        // (b) keywords' array
        let now = swap(TOML, "keywords = [\"a\", \"b\"]", "keywords = [ \"a\", \"c\", ]");
        assert_eq!(
            package_info_only("Cargo.toml", TOML.as_bytes(), now.as_bytes()),
            Some(vec![pl(
                "Cargo.toml",
                line_no(TOML, "keywords = [\"a\", \"b\"]"),
                "keywords = [\"a\", \"b\"]",
                "keywords = [ \"a\", \"c\", ]"
            )])
        );
        // (c) the local package's version in Cargo.lock, CRLF before and LF now
        let approved = crlf(LOCK);
        let now = swap(LOCK, "name = \"demo\"\nversion = \"0.1.0\"", "name = \"demo\"\nversion = \"0.1.1\"");
        assert_eq!(
            package_info_only("Cargo.lock", approved.as_bytes(), now.as_bytes()),
            Some(vec![pl("Cargo.lock", line_no(LOCK, "version = \"0.1.0\""), "version = \"0.1.0\"", "version = \"0.1.1\"")])
        );
        // (d) line endings alone
        assert_eq!(package_info_only("Cargo.toml", crlf(TOML).as_bytes(), TOML.as_bytes()), Some(vec![]));
        assert_eq!(package_info_only("Cargo.lock", LOCK.as_bytes(), crlf(LOCK).as_bytes()), Some(vec![]));
    }

    #[test]
    fn package_info_only_refuses_everything_else() {
        let toml_cases: Vec<(&str, String)> = vec![
            ("a dependency's version", swap(TOML, "serde = \"1.0.1\"", "serde = \"1.0.2\"")),
            ("version in [dependencies.x]", swap(TOML, "version = \"2.0.0\"", "version = \"2.0.1\"")),
            ("version in [workspace.package]", swap(TOML, "version = \"9.0.0\"", "version = \"9.0.1\"")),
            ("edition", swap(TOML, "edition = \"2021\"", "edition = \"2024\"")),
            ("rust-version", swap(TOML, "rust-version = \"1.80\"", "rust-version = \"1.81\"")),
            ("name", swap(TOML, "name = \"demo\"", "name = \"demo2\"")),
            ("build", swap(TOML, "build = \"build.rs\"", "build = \"other.rs\"")),
            ("links", swap(TOML, "links = \"foo\"", "links = \"bar\"")),
            ("one more line", swap(TOML, "version = \"0.1.0\"\n", "version = \"0.1.0\"\nhomepage = \"h\"\n")),
            ("single quotes", swap(TOML, "version = \"0.1.0\"", "version = '0.1.1'")),
            ("a comment after", swap(TOML, "version = \"0.1.0\"", "version = \"0.1.1\" # x")),
            ("a dotted key", swap(TOML, "version = \"0.1.0\"", "version.workspace = true")),
            ("another key", swap(TOML, "version = \"0.1.0\"", "license = \"0.1.0\"")),
            ("a backslash", swap(TOML, "version = \"0.1.0\"", "version = \"0.1\\u0031\"")),
            ("an inline table", swap(TOML, "keywords = [\"a\", \"b\"]", "keywords = { a = \"b\" }")),
            ("the [package] header", swap(TOML, "[package]", "[package] # x")),
        ];
        for (what, now) in &toml_cases {
            assert_eq!(package_info_only("Cargo.toml", TOML.as_bytes(), now.as_bytes()), None, "{what}");
        }
        // a multi-line string anywhere in either file, even on a line that did not change
        for marker in ["\"\"\"", "'''"] {
            let approved = format!("{TOML}# {marker}\n");
            let now = swap(&approved, "version = \"0.1.0\"", "version = \"0.1.1\"");
            assert_eq!(package_info_only("Cargo.toml", approved.as_bytes(), now.as_bytes()), None, "{marker}");
        }
        let now = swap(TOML, "description = \"demo\"", "description = \"\"\"demo\"\"\"");
        assert_eq!(package_info_only("Cargo.toml", TOML.as_bytes(), now.as_bytes()), None);

        let lock_cases: Vec<(&str, String)> = vec![
            ("a registry package's version", swap(LOCK, "version = \"1.0.1\"", "version = \"1.0.2\"")),
            ("the file's own version", swap(LOCK, "version = 4", "version = 5")),
            ("a dependencies line", swap(LOCK, " \"serde\",", " \"serde2\",")),
            ("an indented version", swap(LOCK, "name = \"demo\"\nversion = \"0.1.0\"", "name = \"demo\"\n version = \"0.1.1\"")),
        ];
        for (what, now) in &lock_cases {
            assert_eq!(package_info_only("Cargo.lock", LOCK.as_bytes(), now.as_bytes()), None, "{what}");
        }

        let version_only = swap(TOML, "version = \"0.1.0\"", "version = \"0.1.1\"");
        for path in ["Cargo.toml.bak", "x/Cargo.lockx", "cargo.toml", "package.json"] {
            assert_eq!(package_info_only(path, TOML.as_bytes(), version_only.as_bytes()), None, "{path}");
        }
        let mut bad = TOML.as_bytes().to_vec();
        bad.push(0xff);
        assert_eq!(package_info_only("Cargo.toml", &bad, TOML.as_bytes()), None);
        assert_eq!(package_info_only("Cargo.toml", TOML.as_bytes(), &bad), None);
    }

    fn approved_at(fp: &Fingerprint, at: &str) -> Approval {
        Approval { approved_at: at.into(), ..approval_of(fp) }
    }

    #[test]
    fn match_approval_ignores_approvals_of_an_earlier_check_definition() {
        let tree = tempfile::tempdir().unwrap();
        let blobs_dir = tempfile::tempdir().unwrap();
        write(tree.path(), "Cargo.toml", TOML);
        blobs::put_bytes_into(blobs_dir.path(), TOML.as_bytes()).unwrap();
        let bumped = swap(TOML, "version = \"0.1.0\"", "version = \"0.1.1\"");
        let mut c = check("nosuchexe-zz --new", &["Cargo.toml"]);
        c.created_at = "2026-01-02T00:00:00Z".into();

        // (a) approved after the check was added: package info only
        let a = approved_at(&fingerprint(&c, tree.path()).unwrap(), "2026-01-03T00:00:00Z");
        write(tree.path(), "Cargo.toml", &bumped);
        let now = fingerprint(&c, tree.path()).unwrap();
        match match_approval(&c, std::slice::from_ref(&a), &now, tree.path(), blobs_dir.path(), true) {
            ApprovalMatch::PackageInfoOnly { approval, lines } => {
                assert_eq!(approval, a);
                assert_eq!(lines.len(), 1);
            }
            other => panic!("{other:?}"),
        }

        // (b) an approval of the replaced definition (another cmd, same exe)
        // that check_approvals still held
        write(tree.path(), "Cargo.toml", TOML);
        let old = check("nosuchexe-zz --old", &["Cargo.toml"]);
        let a_old = approved_at(&fingerprint(&old, tree.path()).unwrap(), "2026-01-03T00:00:00Z");
        write(tree.path(), "Cargo.toml", &bumped);
        assert!(matches!(
            match_approval(&c, &[a_old], &now, tree.path(), blobs_dir.path(), true),
            ApprovalMatch::NoMatch { .. }
        ));

        // (c) the same definition and the same tree, approved before the
        // check was (re)added: not even an exact match counts
        write(tree.path(), "Cargo.toml", TOML);
        let same = fingerprint(&c, tree.path()).unwrap();
        let early = approved_at(&same, "2026-01-01T00:00:00Z");
        assert_eq!(early.fingerprint, same.hex);
        assert!(matches!(
            match_approval(&c, std::slice::from_ref(&early), &same, tree.path(), blobs_dir.path(), true),
            ApprovalMatch::NoMatch { .. }
        ));
        assert!(matches!(
            match_approval(&c, &[early], &same, tree.path(), blobs_dir.path(), false),
            ApprovalMatch::NoMatch { .. }
        ));
    }

    #[test]
    fn match_approval_exact_then_package_info_then_names_the_newest_diff() {
        let tree = tempfile::tempdir().unwrap();
        let blobs_dir = tempfile::tempdir().unwrap();
        let mut c = check("nosuchexe-zz", &["Cargo.toml", "a.txt"]);
        c.created_at = "2026-01-01T00:00:00Z".into();
        write(tree.path(), "Cargo.toml", TOML);
        write(tree.path(), "a.txt", "x");
        let x = approved_at(&fingerprint(&c, tree.path()).unwrap(), "2026-01-02T00:00:00Z");
        let (copy_sha, _, copy) = blobs::put_bytes_into(blobs_dir.path(), TOML.as_bytes()).unwrap();
        write(tree.path(), "a.txt", "y");
        let y = approved_at(&fingerprint(&c, tree.path()).unwrap(), "2026-01-03T00:00:00Z");
        assert_ne!(x.fingerprint, y.fingerprint);
        let m = |tree: &Path, list: &[Approval], inputs_checked: bool| {
            let now = fingerprint(&c, tree).unwrap();
            match_approval(&c, list, &now, tree, blobs_dir.path(), inputs_checked)
        };

        // (a) the tree is back at X: the older approval, exactly
        write(tree.path(), "a.txt", "x");
        assert_eq!(m(tree.path(), &[y.clone(), x.clone()], true), ApprovalMatch::Exact(x.clone()));

        // (b) X with [package] version bumped: X, package info only
        write(tree.path(), "Cargo.toml", &swap(TOML, "version = \"0.1.0\"", "version = \"0.1.1\""));
        assert_eq!(
            m(tree.path(), &[y.clone(), x.clone()], true),
            ApprovalMatch::PackageInfoOnly {
                approval: x.clone(),
                lines: vec![PackageLine {
                    path: "Cargo.toml".into(),
                    line: line_no(TOML, "version = \"0.1.0\""),
                    approved: "version = \"0.1.0\"".into(),
                    now: "version = \"0.1.1\"".into(),
                }],
            }
        );

        // (c) X's copy is gone: no match, and the note says why
        assert_eq!(x.detail.as_deref().map(|d| d.contains(&copy_sha)), Some(true));
        std::fs::remove_file(&copy).unwrap();
        match m(tree.path(), std::slice::from_ref(&x), true) {
            ApprovalMatch::NoMatch { changed, note } => {
                assert_eq!(changed, ["input=Cargo.toml"]);
                let note = note.expect("a note");
                assert!(
                    note.starts_with("package-info auto-accept needs the approved copy of Cargo.toml"),
                    "{note}"
                );
            }
            other => panic!("{other:?}"),
        }

        // (d) another input changed as well: no note
        write(tree.path(), "a.txt", "z");
        match m(tree.path(), std::slice::from_ref(&x), true) {
            ApprovalMatch::NoMatch { changed, note } => {
                assert_eq!(changed, ["input=Cargo.toml", "input=a.txt"]);
                assert_eq!(note, None);
            }
            other => panic!("{other:?}"),
        }

        // (e) at base the inputs are not read; X's own hex still matches
        let zeroed = Approval { fingerprint: "0".repeat(64), ..y.clone() };
        assert_eq!(m(tree.path(), &[zeroed, x.clone()], false), ApprovalMatch::Exact(x.clone()));

        // no approvals at all
        assert_eq!(
            m(tree.path(), &[], true),
            ApprovalMatch::NoMatch { changed: vec!["fingerprint".into()], note: None }
        );
    }
}
