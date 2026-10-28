mod ctrl_c;
#[cfg(target_os = "linux")]
pub mod linux;
mod shell;
#[cfg(windows)]
pub mod windows;

// Linux support (docs/post-phase2/linux-support-scope-2026-09-26.md): the two
// controllers are the whole platform surface. Anything else would compile to
// a binary whose every process-control call panics, which is what the old
// `#[cfg(not(windows))]` stubs did on Linux - refuse to build instead.
#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("thoth supports Windows and Linux only");

use anyhow::Result;
use std::path::Path;
use std::process::Child;

/// Windows `CreateProcess` creation flags. Values from the Win32 headers;
/// verified against Phase 0's process experiments.
///
/// Rider (whole-branch review, Minor 5): `#[cfg(windows)]`-gated rather than
/// left unconditional. Every reader of these constants (`windows.rs`'s
/// `use` at the top of that module, `quiet` below, and
/// `commands::profile::probe_auth`) is already behind its own
/// `#[cfg(windows)]`, so on a Linux build (`linux.rs`'s controller) these
/// constants would otherwise have zero non-test readers and trip
/// `dead_code` under `-D warnings`. Gating the constants themselves - rather than adding a
/// `#[cfg(not(windows))]` reader just to keep them "used" - was the cleaner
/// of the two options the review offered: it says outright that these flags
/// do not exist as a concept outside Windows, instead of manufacturing a
/// reference whose only purpose is to appease the lint.
///
/// `DETACHED_PROCESS` used to live here too, for `spawn_detached`. It is
/// gone on purpose, not an oversight: a `DETACHED_PROCESS` child has no
/// console at all, so any console-subsystem grandchild it later spawns
/// without its own creation flags (production: the runner's `git`/
/// `tasklist`/`taskkill` calls; a test: `spawn_detached`'s own
/// `cmd.exe`/`ping` in the Windows-controller unit tests) gets a brand-new
/// *visible* console from Windows's default child-console rule - and on a
/// machine whose default terminal is Windows Terminal, that surfaces as a
/// popping-up terminal window per invocation, followed by
/// `0x800700e8` once the short-lived child exits. `CREATE_NO_WINDOW` gives
/// the process a *hidden* console instead of none, which descendants
/// inherit, so the whole tree stays windowless. Do not reintroduce
/// `DETACHED_PROCESS` for any child that itself spawns console-subsystem
/// tools.
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;

/// Forces `cmd` to run with a hidden console instead of creating (or, if its
/// parent has none, being handed) a new *visible* one. Every console-
/// subsystem tool thoth shells out to (`git`, `tasklist`, `taskkill`, the
/// `claude` auth probe, ...) needs this, because the process that ends up
/// running it is not always something with its own console already - most
/// notably the detached runner started by `spawn_detached`. See the doc
/// comment on the constants above for the full failure mode this prevents.
/// No-op on non-Windows.
#[cfg(windows)]
pub(crate) fn quiet(cmd: &mut std::process::Command) -> &mut std::process::Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(windows))]
pub(crate) fn quiet(cmd: &mut std::process::Command) -> &mut std::process::Command {
    cmd
}

/// How the runner launches the harness child. stdout is always piped (the runner
/// tees it to stream.jsonl); stderr goes straight to a file so nothing has to
/// drain a second pipe.
pub struct ChildSpec<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a [(String, String)],
    /// Variable names taken out of the inherited environment (RD-2, H15).
    /// Applied after `env`, so `env` cannot add one of them back.
    pub env_remove: &'a [String],
    pub stderr_path: Option<&'a Path>,
    /// `Some`: stdin is piped and the caller feeds it (`feed_stdin`);
    /// `None`: stdin is null.
    pub stdin: Option<&'a str>,
}

/// Fix round 2 / N1: how long `run_shell` waits for a reader thread to reach
/// EOF after the child has exited (or been killed) before giving up on it
/// rather than blocking past the deadline. `pub` so `verify`'s
/// `output_incomplete` error message can name this exact grace instead of a
/// duplicated magic number.
pub const DRAIN_GRACE_SECS: u64 = 2;

/// Whole-branch review Important 4: how many bytes `run_shell` keeps from one
/// stream before it stops appending.
///
/// `run_shell` bounded time but not size: a worker-declared verification
/// command is allowed to run for up to 600 s by default, and a runaway print
/// loop at 10 MB/s is ~6 GB in `thoth`'s own address space, then the same
/// bytes again in the blob store - which Phase 2 never collects. Past the cap
/// the reader keeps draining the pipe (so the child never blocks on a full
/// one) and simply stops appending; `ShellOutput::output_truncated` says so.
/// Truncation is not a failure: the exit code is still the command's own.
///
/// 8 MiB per stream, the same figure `commands::hook::MAX_LINE_BYTES` uses for
/// a captured line - "more output than any verification command has a reason
/// to produce, and still small enough to hold twice".
pub const MAX_STREAM_BYTES: usize = 8 * 1024 * 1024;

/// What one shell command produced. Only `thoth verify` uses this: v6 §2.2's
/// fourth companion rule allows the control plane to re-run the verification
/// commands a worker declared, and that is the only place thoth runs
/// arbitrary command text.
pub struct ShellOutput {
    /// `None` when `run_shell` killed the command after `timed_out`, and -
    /// off Windows only - when the child was terminated by a signal rather
    /// than exiting normally (the `sh -c` branch: `ExitStatus::code()`
    /// returns `None` for a signalled process independent of this struct's
    /// own `timed_out`). Fix round 2 / R3: the previous wording ("never
    /// `None` for any other reason") was wrong about that second case.
    pub exit_code: Option<i64>,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: i64,
    /// The argv handed to the shell: `cmd`/`/C`/the line (passed verbatim, not
    /// MSVC-quoted, since cmd would not undo that) or `sh`/`-c`/the line.
    /// `verify` exists to re-run the command a worker *declared*, so the
    /// evidence records exactly what was sent.
    pub argv: Vec<String>,
    /// Fix round 1 / I2: whether `run_shell`'s deadline tripped and the whole
    /// child tree was killed rather than the command exiting on its own.
    pub timed_out: bool,
    /// Fix round 2 / N1: whether a reader thread had not reached EOF by
    /// `DRAIN_GRACE_SECS` after the child exited or was killed, meaning
    /// `stdout`/`stderr` may be missing trailing bytes a still-running
    /// descendant (one that inherited the pipe, e.g. `start /b <server>`)
    /// would eventually have written. `verify` treats this as a failure even
    /// when `exit_code` is `Some(0)`: a command that leaves something behind
    /// holding its pipes did not complete cleanly.
    pub output_incomplete: bool,
    /// Whole-branch review Important 4: whether either stream reached
    /// `MAX_STREAM_BYTES` and the rest of it was drained but not kept.
    /// Unlike `output_incomplete` this is **not** a failure - the command ran
    /// to completion and its exit code means what it says - so `verify`
    /// records it in the evidence row and prints it, and leaves `error`
    /// alone.
    pub output_truncated: bool,
}

pub trait ProcessController {
    /// Start a process that outlives this one. Returns its native PID.
    fn spawn_detached(&self, program: &str, args: &[String], cwd: &Path) -> Result<u32>;
    /// Start the harness child with stdout piped.
    fn spawn_child(&self, spec: ChildSpec<'_>) -> Result<Child>;
    /// Kill the process and every descendant, then confirm it is gone.
    /// Succeeds if the pid was already gone; errors if it is still listed after
    /// the kill, so a caller can report a kill that did not take.
    fn kill_tree(&self, pid: u32) -> Result<()>;
    /// Whether the pid is currently listed. `kill_tree` uses this to verify
    /// itself; `cancel`'s escalation path relies on that verification.
    fn is_alive(&self, pid: u32) -> Result<bool>;
}

#[cfg(windows)]
pub fn controller() -> Box<dyn ProcessController> {
    Box::new(windows::WindowsProcessController)
}

#[cfg(target_os = "linux")]
pub fn controller() -> Box<dyn ProcessController> {
    Box::new(linux::LinuxProcessController)
}

#[cfg(windows)]
pub use windows::{image_name, release_job};

#[cfg(target_os = "linux")]
pub use linux::{image_name, release_job};

pub use ctrl_c::ignore_ctrl_c_while;

pub use shell::{
    argv_refusal, powershell_script, run_shell, shell_args_text, shell_argv, split_argv, Shell,
};

#[cfg(windows)]
mod resolve;

/// What to launch for a harness executable `program` with `args`.
///
/// Windows: npm installs `codex`/`claude` as a `.cmd` shim and Rust does not
/// look at PATHEXT, so a shim is resolved to `node.exe <entry.js> args...`
/// (`resolve.rs`). A `.cmd`/`.bat` is never run through `cmd.exe`. Elsewhere,
/// and for anything that is already an executable, `program` and `args` come
/// back unchanged. An `Err` names the shim and how to set
/// `THOTH_CODEX_BIN` / `THOTH_CLAUDE_BIN`. Reads files and PATH only.
#[cfg(windows)]
pub fn resolve_harness_command(program: &str, args: &[String]) -> Result<(String, Vec<String>)> {
    resolve::resolve(program, args)
}

#[cfg(not(windows))]
pub fn resolve_harness_command(program: &str, args: &[String]) -> Result<(String, Vec<String>)> {
    Ok((program.to_string(), args.to_vec()))
}

/// Makes sure the calling `run-worker` is running where the platform's
/// process control needs it to be, and re-runs it there if not.
///
/// - `Ok(None)`: carry on in this process.
/// - `Ok(Some(code))`: this process re-ran `run-worker` elsewhere and waited
///   for it; exit with `code` without doing anything else.
///
/// `already_rerun` is `run-worker --supervised`: this process *is* such a
/// re-run. If it is still not where it needs to be, that is an error, never a
/// second re-run - otherwise a host where units do not land where thoth
/// expects would nest units without end instead of refusing.
///
/// On Linux the runner must be inside its own systemd `--user` service
/// (`linux::ensure_supervised`): that is what makes a dead runner take its
/// whole worker tree with it. `dispatch` already starts it there; this covers
/// a `run-worker` started any other way (a test, a hand re-run). Windows has
/// no such requirement - the Job Object is created by the runner itself.
#[cfg(windows)]
pub fn ensure_supervised(_args: &[String], _already_rerun: bool) -> Result<Option<i32>> {
    Ok(None)
}

#[cfg(target_os = "linux")]
pub use linux::ensure_supervised;

/// W3: the longest command line thoth hands to a harness, in UTF-16 units.
/// Windows' `CreateProcess` allows 32,767 (os error 206 past that); the
/// margin leaves room for what a shim resolution or the OS adds.
pub const COMMAND_LINE_CAP: usize = 30_000;

/// The length, in UTF-16 units including the terminating NUL, of the command
/// line Rust's std builds on Windows for `program` and `args`: the program is
/// always quoted; each argument follows one space and is quoted only when it
/// is empty or holds a space or tab; a `"` becomes `\"` with the backslashes
/// before it doubled, and when quoted the trailing backslashes are doubled
/// too. The same rule is used on Linux so the cap means the same thing on
/// both platforms.
pub fn command_line_len(program: &str, args: &[String]) -> usize {
    let mut n = 2 + program.encode_utf16().count() + 1;
    for a in args {
        n += 1 + arg_len(a);
    }
    n
}

fn arg_len(arg: &str) -> usize {
    let quote = arg.is_empty() || arg.contains([' ', '\t']);
    let mut n = if quote { 2 } else { 0 };
    let mut backslashes = 0;
    for c in arg.chars() {
        if c == '\\' {
            backslashes += 1;
        } else {
            if c == '"' {
                // the backslashes before it are doubled, then `\"`
                n += backslashes + 1;
            }
            backslashes = 0;
        }
        n += c.len_utf16();
    }
    if quote {
        n += backslashes;
    }
    n
}

/// `Err` when `command_line_len` is over `COMMAND_LINE_CAP`, naming the
/// length and the start of the longest argument (normally the prompt).
pub fn check_command_line(program: &str, args: &[String]) -> Result<()> {
    let n = command_line_len(program, args);
    if n <= COMMAND_LINE_CAP {
        return Ok(());
    }
    let longest = args.iter().max_by_key(|a| arg_len(a)).map(String::as_str).unwrap_or(program);
    let m = arg_len(longest);
    let head: String = longest.chars().take(40).collect();
    anyhow::bail!(
        "command line too long: {n} UTF-16 units (thoth's cap is {COMMAND_LINE_CAP}; Windows \
         allows 32767); the longest argument is {m} units and starts with {head:?}"
    )
}

/// W3: writes `bytes` to a child's stdin on its own thread, then drops the
/// pipe so the child reads EOF.
///
/// This cannot deadlock the runner: the calling (main) thread never waits on
/// this one - the handle is dropped like the stdout reader's. The writer
/// itself only blocks while the child is not reading its stdin, and claude
/// and codex both read the prompt to EOF before they start work. If the
/// runner kills the child first, the write fails with BrokenPipe (or
/// ERROR_NO_DATA on Windows; Rust's runtime ignores SIGPIPE) and the thread
/// ends. A write failure goes to the run's runner-error.log, never a panic.
pub fn feed_stdin(
    stdin: std::process::ChildStdin,
    bytes: Vec<u8>,
    run_dir: std::path::PathBuf,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("thoth-stdin".to_string()).spawn(move || {
        use std::io::Write;
        let mut stdin = stdin;
        if let Err(e) = stdin.write_all(&bytes).and_then(|()| stdin.flush()) {
            crate::runner::log_runner_error(
                &run_dir,
                &format!("writing the prompt to the worker's stdin failed: {e}"),
            );
        }
        // `stdin` drops here, closing the pipe.
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|a| a.to_string()).collect()
    }

    #[test]
    fn command_line_len_follows_the_std_windows_quoting_rule() {
        assert_eq!(command_line_len("claude", &[]), 9);
        assert_eq!(command_line_len("claude", &s(&["-p"])), 12);
        assert_eq!(command_line_len("c", &s(&["a b"])), 10);
        assert_eq!(command_line_len("c", &s(&[""])), 7);
        assert_eq!(command_line_len("c", &s(&["x\"y"])), 9);
        assert_eq!(command_line_len("c", &s(&["a\\"])), 7);
        assert_eq!(command_line_len("c", &s(&["a \\"])), 11);
        assert_eq!(command_line_len("c", &s(&["中"])), 6);
        assert_eq!(command_line_len("c", &s(&["😀"])), 7);
    }

    #[test]
    fn check_command_line_allows_the_cap_and_refuses_one_past_it() {
        assert!(check_command_line("x", &["a".repeat(29_995)]).is_ok());
        let e = check_command_line("x", &["a".repeat(29_996)]).unwrap_err().to_string();
        assert!(e.starts_with("command line too long: "), "{e}");
        for part in ["30001", "30000", "aaaa"] {
            assert!(e.contains(part), "missing {part:?} in {e}");
        }
    }

    /// The shell these tests' command lines are written for.
    const PLATFORM_SHELL: Shell = if cfg!(windows) { Shell::Cmd } else { Shell::Sh };

    #[test]
    #[cfg(windows)]
    fn creation_flag_values_match_the_windows_headers() {
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
        assert_eq!(CREATE_NEW_PROCESS_GROUP, 0x0000_0200);
        // the combination both the runner and dispatch actually use
        assert_eq!(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP, 0x0800_0200);
    }

    /// A script for `PLATFORM_SHELL` in `dir` (`STEM.cmd` / executable
    /// `STEM.sh`), and the check command line that names it with `args`.
    /// AC14: a shell check runs one program, so shell syntax lives here.
    fn script(dir: &std::path::Path, stem: &str, win: &str, unix: &str, args: &str) -> (std::path::PathBuf, String) {
        let (name, body) = if cfg!(windows) {
            (format!("{stem}.cmd"), format!("@echo off\r\n{win}\r\n"))
        } else {
            (format!("{stem}.sh"), format!("#!/bin/sh\n{unix}\n"))
        };
        let p = dir.join(&name);
        std::fs::write(&p, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        (p, format!("./{name} {args}").trim_end().to_string())
    }

    #[test]
    fn run_shell_captures_stdout_stderr_and_the_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let timeout = std::time::Duration::from_secs(30);
        let (say, line) = script(dir.path(), "say", "echo verify-one", "echo verify-one", "");
        let ok = run_shell(PLATFORM_SHELL, &say, &line, dir.path(), timeout).unwrap();
        assert_eq!(ok.exit_code, Some(0));
        assert!(ok.stdout.contains("verify-one"), "got {:?}", ok.stdout);
        assert!(!ok.timed_out);
        assert!(!ok.output_incomplete, "no descendant to hold the pipe open here");
        // L7: the argv is recorded so what the shell got is visible
        #[cfg(windows)]
        assert_eq!(ok.argv, ["cmd", "/D", "/V:OFF", "/S", "/C", "\"\"%THOTH_CHECK_PROGRAM%\"\""]);
        #[cfg(not(windows))]
        assert_eq!(ok.argv, ["sh".to_string(), "-c".into(), "\"$0\" \"$@\"".into(), say.display().to_string()]);

        // a non-zero exit is data, not an error: `verify` records it
        let (ex, line) = script(dir.path(), "ex", "exit /b 3", "exit 3", "");
        let bad = run_shell(PLATFORM_SHELL, &ex, &line, dir.path(), timeout).unwrap();
        assert_eq!(bad.exit_code, Some(3));
        assert!(!bad.timed_out);

        // cwd is honoured
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let (lister, line) = script(dir.path(), "lister", "dir marker.txt", "ls marker.txt", "");
        let listed = run_shell(PLATFORM_SHELL, &lister, &line, dir.path(), timeout).unwrap();
        assert!(
            listed.stdout.contains("marker.txt"),
            "the command must run in the directory it was given: {:?}",
            listed.stdout
        );
    }

    /// The arguments reach the program exactly as written (AC14 (b): the
    /// shell parses none of them on the way).
    #[test]
    fn run_shell_passes_the_arguments_through_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let (args, line) =
            script(dir.path(), "args", "echo [%1] [%2]", "echo \"[$1] [$2]\"", "alpha --flag:x");
        let out = run_shell(PLATFORM_SHELL, &args, &line, dir.path(), std::time::Duration::from_secs(30)).unwrap();
        assert_eq!(out.exit_code, Some(0), "stdout {:?} stderr {:?}", out.stdout, out.stderr);
        assert!(out.stdout.contains("[alpha] [--flag:x]"), "got {:?}", out.stdout);
        // the argv actually sent: the shell, its fixed switches, then the program
        // by absolute path (Windows: through the environment) and the plain arguments
        #[cfg(windows)]
        assert_eq!(
            out.argv,
            ["cmd", "/D", "/V:OFF", "/S", "/C", "\"\"%THOTH_CHECK_PROGRAM%\" alpha --flag:x\""]
        );
        #[cfg(not(windows))]
        assert_eq!(
            out.argv,
            [
                "sh".to_string(),
                "-c".into(),
                "\"$0\" \"$@\"".into(),
                args.display().to_string(),
                "alpha".into(),
                "--flag:x".into()
            ]
        );
    }
}
