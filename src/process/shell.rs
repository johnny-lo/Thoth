//! `run_shell`: the one place thoth runs an approved check (`verify`). Since
//! P7-1b AC14 it always runs the program the check's fingerprint resolved, by
//! absolute path, whatever the shell.
//!
//! Moved out of `windows.rs` for Linux support
//! (docs/post-phase2/linux-support-scope-2026-09-26.md §3.3): everything here
//! is portable except the shell's name, the Windows `quiet` creation flag and
//! the Unix process group, each gated where it is used. The timeout path goes
//! through `super::controller().kill_tree` on both platforms.

use anyhow::{bail, Result};
use std::path::Path;

/// How an approved check's command line is executed (P7-1b). The names are the
/// `--shell` values `thoth checks add` accepts (`crate::checks::SHELLS`).
// `PowerShell` is the program's own name, not a repeat of the enum's.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// No shell: the line is split by `split_argv` and run directly.
    Argv,
    /// `cmd /C`, Windows only.
    Cmd,
    /// `powershell.exe` (5.1) with `-EncodedCommand`, Windows only.
    PowerShell,
    /// `sh -c`, Linux only.
    Sh,
    /// `bash -c`, Linux only.
    Bash,
}

impl Shell {
    pub fn parse(name: &str) -> Option<Shell> {
        Some(match name {
            "argv" => Shell::Argv,
            "cmd" => Shell::Cmd,
            "powershell" => Shell::PowerShell,
            "sh" => Shell::Sh,
            "bash" => Shell::Bash,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Shell::Argv => "argv",
            Shell::Cmd => "cmd",
            Shell::PowerShell => "powershell",
            Shell::Sh => "sh",
            Shell::Bash => "bash",
        }
    }

    /// Whether this platform can run the shell at all.
    pub fn available(self) -> bool {
        match self {
            Shell::Argv => true,
            Shell::Cmd | Shell::PowerShell => cfg!(windows),
            Shell::Sh | Shell::Bash => cfg!(target_os = "linux"),
        }
    }
}

/// Splits an `argv` check's line: words are separated by whitespace, a pair of
/// double quotes groups words (the quotes themselves are dropped), and there is
/// no backslash escaping. `""` is an empty argument.
pub fn split_argv(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut quoted = false;
    for ch in cmd.chars() {
        if ch == '"' {
            quoted = !quoted;
            in_word = true;
        } else if ch.is_whitespace() && !quoted {
            if in_word {
                out.push(std::mem::take(&mut cur));
                in_word = false;
            }
        } else {
            cur.push(ch);
            in_word = true;
        }
    }
    if in_word {
        out.push(cur);
    }
    out
}

/// Standard base64 (RFC 4648 alphabet, `=` padding).
pub fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The environment variable a `cmd` or `powershell` child reads the program's
/// absolute path from (AC14 (b)): the path never passes through either
/// shell's parser, so a space, `&`, `^`, `'`, `(` or `%` in it is just text.
pub const PROGRAM_ENV: &str = "THOTH_CHECK_PROGRAM";

/// A shell check's arguments as written: the text after its program word,
/// trimmed. AC14 (a) has already limited it to plain characters.
pub fn shell_args_text(cmd_line: &str) -> &str {
    let t = cmd_line.trim();
    match t.find([' ', '\t']) {
        Some(i) => t[i..].trim(),
        None => "",
    }
}

/// The script a `powershell` check runs: the program from `PROGRAM_ENV` with
/// the call operator (which never appends an extension or consults an alias,
/// function or PATH), `args` as written; stop on the first error, and exit
/// with the last native program's exit code when there was one.
pub fn powershell_script(args: &str) -> String {
    let sep = if args.is_empty() { "" } else { " " };
    format!(
        "$ErrorActionPreference='Stop'; & $env:{PROGRAM_ENV}{sep}{args}; \
         if ($null -ne $LASTEXITCODE) {{ exit $LASTEXITCODE }}"
    )
}

/// `-EncodedCommand` takes base64 of the UTF-16LE text, so no quoting of the
/// script is involved at all.
fn powershell_encoded(script: &str) -> String {
    let bytes: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
    base64(&bytes)
}

/// The argv `run_shell` sends for `shell`, the check's `cmd_line` and
/// `program`, the absolute path of the file the fingerprint pinned (AC14 (b)):
/// - `argv`: `program`, then the rest of `split_argv`.
/// - `cmd`: `cmd /D /V:OFF /S /C ""%THOTH_CHECK_PROGRAM%" ARGS"`, sent as one
///   raw string after `cmd`. `/S` strips exactly the outer pair of quotes,
///   leaving the quoted path; without it cmd would strip the path's own
///   quotes and read a space or `&` in it as syntax. `/D` skips AutoRun,
///   `/V:OFF` delayed `!` expansion.
/// - `powershell`: `-EncodedCommand` of `powershell_script(ARGS)`.
/// - `sh`/`bash`: `-c '"$0" "$@"'` with `program` and the whitespace-split
///   arguments as argv: the shell parses nothing that came from the check.
pub fn shell_argv(shell: Shell, program: &Path, cmd_line: &str) -> Vec<String> {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<String>>();
    let program = program.display().to_string();
    let args = shell_args_text(cmd_line);
    match shell {
        Shell::Argv => {
            let mut v = vec![program];
            v.extend(split_argv(cmd_line).into_iter().skip(1));
            v
        }
        Shell::Cmd => {
            let sep = if args.is_empty() { "" } else { " " };
            let mut v = s(&["cmd", "/D", "/V:OFF", "/S", "/C"]);
            v.push(format!("\"\"%{PROGRAM_ENV}%\"{sep}{args}\""));
            v
        }
        Shell::PowerShell => {
            let mut v = s(&[
                "powershell.exe",
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-EncodedCommand",
            ]);
            v.push(powershell_encoded(&powershell_script(args)));
            v
        }
        Shell::Sh | Shell::Bash => {
            let mut v = s(&[shell.name(), "-c", "\"$0\" \"$@\""]);
            v.push(program);
            v.extend(args.split_whitespace().map(String::from));
            v
        }
    }
}

/// Windows: an `argv` check's program must resolve (in P7-1a's order: bare,
/// `.exe`, `.cmd`, `.bat`) to a file ending in `.exe`. AC11 (a): Rust's
/// `Command` tries `<path>.exe` first for any path that does not end in
/// `.exe`, so `tool` or `tool.cmd` would lose to a `tool.exe` or
/// `tool.cmd.exe` added beside it; and a `.cmd`/`.bat` needs `cmd.exe`, which
/// would re-parse its arguments. Such a check is refused with the reason
/// returned here. Always `None` elsewhere.
pub fn argv_refusal(program: &str, cwd: &Path) -> Option<String> {
    if !cfg!(windows) || program.is_empty() {
        return None;
    }
    let hit = crate::checks::resolve_exe(program, cwd, ".")?;
    let name = hit.file_name()?.to_string_lossy().to_lowercase();
    (!name.ends_with(".exe")).then(|| {
        format!(
            "{program} resolves to {}, which is not an .exe: Windows would run '{}.exe' in its \
             place if one appeared; name the .exe itself, or use shell cmd for this check",
            hit.display(),
            hit.display()
        )
    })
}

/// Fix round 2 / N1: how long `run_shell` polls for a killed (or
/// `try_wait`-errored) child to actually be reaped before giving up and
/// returning anyway, rather than blocking in the unbounded `Child::wait()`.
/// Not `pub`: unlike `DRAIN_GRACE_SECS`, nothing outside this function names
/// it.
const REAP_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

/// Runs one command line with `shell` and captures its output, killing the
/// whole child tree if it has not exited by `timeout`.
///
/// The shell's *name* is the main platform-specific thing here, which is why
/// this lives in `process` rather than in `commands::verify`: the Global
/// Constraint is that the core never names a Windows program. P7-1b: the
/// shell is the one the approved check declares (`shell_argv` builds the
/// argv); a shell this platform does not have is an `Err`, and `verify`
/// refuses such a check before it gets here. On Windows the child is
/// spawned through `quiet` like every other console tool, so it does not pop
/// a visible window; on Linux it gets its own process group, so the timeout
/// kill below reaches descendants that are still in that group even once
/// they have been reparented away from the shell.
///
/// stdout/stderr are drained on their own reader threads while this thread
/// polls `try_wait` for exit (fix round 1 / I2): a synchronous `.output()`
/// blocks until the child closes both pipes, which is exactly what a hung
/// verification command never does, so there would be no way to notice the
/// deadline and no way to read whatever the child *did* print before it hung.
///
/// Fix round 2 / N1: **every wait in this function is bounded**, not just the
/// poll loop above. A `Windows` anonymous pipe's read end only sees EOF once
/// *every* duplicate of its write handle is closed, and `cmd.exe` hands its
/// std handles to whatever it spawns - including something a command detaches
/// (`start /b <server>`, a daemonizing test harness). That child can outlive
/// `cmd.exe` itself, so `cmd.exe` exiting (or being killed) is not enough to
/// guarantee the pipe closes: the previous version's `stdout_reader.join()`,
/// the `child.wait()` after a failed `kill_tree`, and `try_wait()?`'s early
/// return were all capable of blocking past the deadline, forever, in that
/// case. No Job Object is used to contain this (deliberately, see below), so
/// this function instead treats "the pipe did not close in time" as data,
/// the same way it already treats a timeout as data:
/// - The reader threads read in bounded chunks into a shared buffer and
///   signal completion over a channel; after the child is no longer being
///   waited on, `drain` gives each at most `DRAIN_GRACE_SECS` to reach EOF,
///   then takes whatever bytes landed and reports the rest as
///   `output_incomplete` instead of joining forever.
/// - After `kill_tree` (however it went), reaping polls `try_wait` for at
///   most `REAP_GRACE` instead of calling the blocking `Child::wait()`.
/// - A `try_wait` error takes the same kill + bounded-reap + bounded-drain
///   path as a timeout before the error is returned, so that path cannot
///   leave the child running or the readers unattended either.
///
/// A non-zero exit, like a timeout or incomplete output, is **not** a
/// `Result::Err` — all three are results `verify` records as data
/// (`ShellOutput::timed_out`/`output_incomplete` distinguish them; `exit_code`
/// is `None` only when the command never produced one, per that field's own
/// doc). `Err` is reserved for thoth itself failing to spawn or supervise
/// the child.
///
/// On expiry the whole tree is killed through `process::controller().kill_tree`,
/// the same path `cancel` already uses (not new FFI): this child was never put
/// in a Job Object / worker cgroup (that only happens for `spawn_child`'s
/// supervised harness children), so `kill_tree`'s group-based half is a no-op
/// here and its live-subtree half (`taskkill /T /F` on Windows; the `/proc`
/// descendant walk plus the process group on Linux) does the work. Fix round
/// 2 / N1.5: putting this child in a Job Object too, so a detached descendant
/// dies with it (closing the pipe immediately instead of needing
/// `output_incomplete` at all), is a real improvement but explicitly deferred:
/// not part of this round, and deliberately not done with a cgroup on Linux
/// either, to keep the two platforms alike.
///
/// AC14 (b): `program` is required for every shell. It is the absolute path
/// of the file the check's fingerprint resolved, and it replaces the first
/// word of `cmd_line`; `shell_argv` shows how each shell receives it. No
/// shell is ever handed the check's program word to look up by itself (AC10
/// (2) for `argv`: `Command::new` would otherwise resolve a relative path
/// against thoth's own working directory on Windows).
pub fn run_shell(
    shell: Shell,
    program: &Path,
    cmd_line: &str,
    cwd: &Path,
    timeout: std::time::Duration,
) -> Result<super::ShellOutput> {
    run_shell_capped(shell, program, cmd_line, cwd, timeout, super::MAX_STREAM_BYTES)
}

/// The child's `PATH` without its empty or relative entries, or `None` when it
/// has none of those. P7-1b / AC10 (3): an entry such as `.` makes every shell
/// search the check's cwd - a directory a worker writes to - for the program,
/// while the fingerprint resolved it elsewhere. AC12 (a): the kept entries are
/// `checks::search_dirs`, the same list the fingerprint searches.
fn path_without_relative_entries(path: Option<&std::ffi::OsStr>) -> Option<std::ffi::OsString> {
    let path = path?;
    if std::env::split_paths(path).all(|p| p.is_absolute()) {
        return None;
    }
    std::env::join_paths(crate::checks::search_dirs(Some(path))).ok()
}

/// `run_shell` with the per-stream byte cap as a parameter.
///
/// Whole-branch review Important 4: production always passes
/// `super::MAX_STREAM_BYTES` (8 MiB). The parameter exists so the cap's
/// behaviour can be tested with a few hundred bytes instead of eight
/// megabytes - a test that had to produce 8 MiB to see the cap fire would be
/// slow enough that nobody would run it.
pub(crate) fn run_shell_capped(
    shell: Shell,
    program: &Path,
    cmd_line: &str,
    cwd: &Path,
    timeout: std::time::Duration,
    max_stream_bytes: usize,
) -> Result<super::ShellOutput> {
    use std::io::Read;
    use std::process::{Command, Stdio};
    use std::sync::{mpsc, Arc, Mutex};

    /// What one reader thread has captured so far.
    #[derive(Default)]
    struct Captured {
        bytes: Vec<u8>,
        /// Set once the cap was reached and bytes started being dropped.
        /// Tracked rather than inferred from `bytes.len() == cap`, so a
        /// stream that produced exactly `cap` bytes and then ended is not
        /// reported as truncated.
        truncated: bool,
    }

    /// A reader thread that copies `pipe` into a shared buffer in bounded
    /// chunks (never one giant blocking `read_to_end`) and signals over `tx`
    /// once it sees EOF or an error. The thread is never explicitly stopped -
    /// there is no portable way to interrupt a blocking `Read::read` on a
    /// pipe - so on the abandon path (see `drain`) it simply keeps running
    /// until whatever is still holding the pipe open closes it, appending
    /// into a buffer nothing reads from any more.
    ///
    /// Whole-branch review Important 4: it appends at most `cap` bytes and
    /// then **keeps reading**. Stopping the read instead would leave the pipe
    /// to fill and the child to block on its next write - turning a noisy
    /// command into a hang, which is exactly the failure this function's
    /// bounded waits exist to avoid. That also bounds the abandoned-reader
    /// case (`output_incomplete`), whose thread would otherwise append
    /// forever.
    fn spawn_reader<R: Read + Send + 'static>(
        mut pipe: R,
        cap: usize,
    ) -> (Arc<Mutex<Captured>>, mpsc::Receiver<()>) {
        let buf: Arc<Mutex<Captured>> = Arc::new(Mutex::new(Captured::default()));
        let buf_writer = Arc::clone(&buf);
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut c = buf_writer.lock().unwrap_or_else(|e| e.into_inner());
                        let room = cap.saturating_sub(c.bytes.len());
                        if room == 0 {
                            c.truncated = true;
                        } else if n > room {
                            c.bytes.extend_from_slice(&chunk[..room]);
                            c.truncated = true;
                        } else {
                            c.bytes.extend_from_slice(&chunk[..n]);
                        }
                    }
                }
            }
            let _ = tx.send(());
        });
        (buf, rx)
    }

    /// Waits up to `grace` for the reader to signal EOF, then takes whatever
    /// is in the buffer either way. Returns `(bytes, incomplete, truncated)`.
    fn drain(
        buf: &Arc<Mutex<Captured>>,
        rx: &mpsc::Receiver<()>,
        grace: std::time::Duration,
    ) -> (Vec<u8>, bool, bool) {
        let reached_eof = rx.recv_timeout(grace).is_ok();
        let captured = std::mem::take(&mut *buf.lock().unwrap_or_else(|e| e.into_inner()));
        (captured.bytes, !reached_eof, captured.truncated)
    }

    if !shell.available() {
        bail!(
            "shell {} is not available on {}",
            shell.name(),
            std::env::consts::OS
        );
    }
    if cmd_line.trim().is_empty() {
        bail!("the command line is empty");
    }
    if !program.is_absolute() {
        bail!(
            "the program to run must be an absolute path, got {}",
            program.display()
        );
    }
    if shell == Shell::Argv {
        if let Some(reason) = argv_refusal(&program.display().to_string(), cwd) {
            bail!("{reason}");
        }
    }
    let argv = shell_argv(shell, program, cmd_line);

    let started = std::time::Instant::now();
    let mut cmd = Command::new(&argv[0]);
    // A `cmd` line goes to cmd verbatim: `args` would quote it with the MSVC
    // convention (`"` -> `\"`), which cmd does not undo.
    #[cfg(windows)]
    if shell == Shell::Cmd {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(argv[1..].join(" "));
    } else {
        cmd.args(&argv[1..]);
    }
    #[cfg(not(windows))]
    cmd.args(&argv[1..]);
    if matches!(shell, Shell::Cmd | Shell::PowerShell) {
        cmd.env(PROGRAM_ENV, program);
    }
    cmd.current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // P7-1b / AC10 (3): nothing may find the program in the check's cwd - a
    // directory the worker writes to - when the fingerprint resolved it on
    // PATH. cmd.exe searches its cwd before PATH unless this variable is set
    // (it and its descendants honour it); and a relative PATH entry would
    // reintroduce the same search in any shell, so those are dropped.
    #[cfg(windows)]
    cmd.env("NoDefaultCurrentDirectoryInExePath", "1");
    if let Some(p) = path_without_relative_entries(std::env::var_os("PATH").as_deref()) {
        cmd.env("PATH", p);
    }
    #[cfg(windows)]
    super::quiet(&mut cmd);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();

    let stdout_pipe = child.stdout.take().expect("stdout was requested as piped");
    let stderr_pipe = child.stderr.take().expect("stderr was requested as piped");
    let (stdout_buf, stdout_rx) = spawn_reader(stdout_pipe, max_stream_bytes);
    let (stderr_buf, stderr_rx) = spawn_reader(stderr_pipe, max_stream_bytes);

    let drain_grace = std::time::Duration::from_secs(super::DRAIN_GRACE_SECS);
    let deadline = started + timeout;
    let mut timed_out = false;
    let mut status = None;
    let mut wait_err: Option<std::io::Error> = None;
    loop {
        match child.try_wait() {
            Ok(Some(s)) => {
                status = Some(s);
                break;
            }
            Ok(None) => {}
            Err(e) => {
                // N1.3: do not return early here - fall through to the same
                // kill + bounded-reap + bounded-drain cleanup a timeout gets,
                // so this path cannot leave the child running or the readers
                // unattended, and only then propagate the error.
                wait_err = Some(e);
                break;
            }
        }
        if std::time::Instant::now() >= deadline {
            timed_out = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    if timed_out || wait_err.is_some() {
        if let Err(e) = super::controller().kill_tree(pid) {
            masked_eprintln!(
                "thoth: warning: kill_tree({pid}) after a verify timeout did not confirm \
                 the tree was gone: {e:#}"
            );
        }

        // N1.2: bounded reap. `kill_tree`'s own confirmation loop already
        // waited for the visible tree to clear, but this process's
        // own `Child` handle still needs reaping - and a descendant that
        // survived (no Job Object here, see the function doc) can keep this
        // handle from ever settling on some platforms, so poll instead of
        // calling the blocking `Child::wait()`.
        let reap_deadline = std::time::Instant::now() + REAP_GRACE;
        loop {
            match child.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {}
            }
            if std::time::Instant::now() >= reap_deadline {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }

    if let Some(e) = wait_err {
        // Best-effort: still bound the drain so the reader threads are not
        // left unaccounted for, even though the bytes are discarded on this
        // path (an `Err` return has no `ShellOutput` to put them in).
        let _ = drain(&stdout_buf, &stdout_rx, drain_grace);
        let _ = drain(&stderr_buf, &stderr_rx, drain_grace);
        return Err(e.into());
    }

    // N1.1: bounded drain. A reader that has not reached EOF by now - a
    // descendant still holding the pipe - is abandoned, not joined; whatever
    // it captured so far is used, and `output_incomplete` says the rest may
    // be missing.
    let (stdout, stdout_incomplete, stdout_truncated) =
        drain(&stdout_buf, &stdout_rx, drain_grace);
    let (stderr, stderr_incomplete, stderr_truncated) =
        drain(&stderr_buf, &stderr_rx, drain_grace);

    let exit_code = if timed_out {
        None
    } else {
        status.and_then(|s| s.code()).map(|c| c as i64)
    };

    Ok(super::ShellOutput {
        exit_code,
        // Lossy: a verification command's output need not be valid UTF-8.
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
        duration_ms: started.elapsed().as_millis() as i64,
        // Recorded, not just used: see the field's doc comment. An operator
        // reading the evidence sees exactly what was sent to the shell.
        argv,
        timed_out,
        output_incomplete: stdout_incomplete || stderr_incomplete,
        output_truncated: stdout_truncated || stderr_truncated,
    })
}

/// The Linux side of the three Windows-only `run_shell` tests in
/// `windows.rs` (timeout kill, stream cap, a detached descendant holding the
/// pipe), in `sh` syntax. Each uses a `sleep` with a duration no other test
/// uses as its tag, so "is it still running" can only match its own process.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// Pids of running `sleep <tag>` processes.
    fn tagged_sleeps(tag: &str) -> Vec<u32> {
        let mut found = Vec::new();
        for e in std::fs::read_dir("/proc").unwrap().flatten() {
            let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                continue;
            };
            let argv: Vec<&[u8]> = cmdline.split(|b| *b == 0).collect();
            if argv.len() >= 2 && argv[0].ends_with(b"sleep") && argv[1] == tag.as_bytes() {
                found.push(pid);
            }
        }
        found
    }

    fn wait_until(secs: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            if cond() {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A `sleep` duration no other test, and no other test *process* running
    /// at once, uses: `first` is the whole-seconds part and the digit after
    /// the dot tells apart the sleeps of one test; the rest of the fraction is
    /// this process's pid, unique among live processes. `sleep` takes
    /// fractional seconds, so the tag is also a valid argument.
    fn tag(first: u32, digit: u32) -> String {
        format!("{first}.{digit}{}", std::process::id())
    }

    /// An executable `sh` script `name` in `dir` (AC14: a shell check runs a
    /// program, so the shell syntax under test lives in a script).
    fn script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    /// Kills any `sleep <tag>` still around if the test panics first.
    struct Guard(String);
    impl Drop for Guard {
        fn drop(&mut self) {
            for pid in tagged_sleeps(&self.0) {
                unsafe {
                    libc::kill(pid as libc::pid_t, libc::SIGKILL);
                }
            }
        }
    }

    /// Fix round 1 / I2 on Linux: a deadline cuts a long command off
    /// promptly, and `kill_tree` takes its child with it - including one it
    /// backgrounded, which is still in `sh`'s process group.
    #[test]
    fn run_shell_kills_the_child_tree_on_timeout_and_reports_it() {
        let (bg, fg) = (tag(61, 5), tag(61, 2));
        let _guard = Guard(fg.clone());
        let _guard2 = Guard(bg.clone());
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "long.sh", &format!("sleep {bg} & sleep {fg}"));
        let started = Instant::now();
        let out = run_shell(Shell::Sh, &program, "./long.sh", dir.path(), Duration::from_secs(2)).unwrap();
        let elapsed = started.elapsed();

        assert!(out.timed_out, "expected the 2s deadline to trip");
        assert_eq!(out.exit_code, None, "no exit status for a killed child");
        assert!(elapsed < Duration::from_secs(6), "run_shell took {elapsed:?}");
        assert!(
            wait_until(2, || tagged_sleeps(&fg).is_empty() && tagged_sleeps(&bg).is_empty()),
            "the timed-out command and its background child must be killed, not orphaned"
        );
        assert_eq!(&out.argv[..2], &["sh".to_string(), "-c".to_string()]);
    }

    #[test]
    fn run_shell_caps_each_stream_and_reports_the_truncation() {
        let dir = tempfile::tempdir().unwrap();
        const CAP: usize = 256;
        let noisy = script(
            dir.path(),
            "noisy.sh",
            "i=0; while [ $i -lt 64 ]; do echo 0123456789012345678901234567890123456789012345678901234567890123; i=$((i+1)); done",
        );
        let out = run_shell_capped(Shell::Sh, &noisy, "./noisy.sh", dir.path(), Duration::from_secs(30), CAP)
            .unwrap();
        assert_eq!(out.stdout.len(), CAP, "exactly the cap is kept, not one byte more");
        assert!(out.output_truncated);
        assert_eq!(out.exit_code, Some(0), "truncation is not a failure");
        assert!(!out.timed_out);
        assert!(!out.output_incomplete, "the reader kept draining to EOF");

        let tiny = script(dir.path(), "tiny.sh", "echo tiny");
        let small =
            run_shell_capped(Shell::Sh, &tiny, "./tiny.sh", dir.path(), Duration::from_secs(30), CAP).unwrap();
        assert!(!small.output_truncated);
        assert!(small.stdout.contains("tiny"));
    }

    /// Fix round 2 / N1 on Linux: `sh` exiting does not close the pipe while
    /// a backgrounded child still holds it; `run_shell` must not wait that
    /// child out, and must say the output may be incomplete.
    #[test]
    fn run_shell_reports_incomplete_output_when_a_background_child_holds_the_pipe() {
        let t = tag(15, 2);
        let _guard = Guard(t.clone());
        let dir = tempfile::tempdir().unwrap();
        let program = script(dir.path(), "detach.sh", &format!("sleep {t} &\nexit 0"));
        let started = Instant::now();
        let out = run_shell(Shell::Sh, &program, "./detach.sh", dir.path(), Duration::from_secs(30)).unwrap();
        let elapsed = started.elapsed();

        assert!(!out.timed_out, "sh itself exits at once");
        assert_eq!(out.exit_code, Some(0));
        assert!(out.output_incomplete, "the background sleep holding the pipe must be reported");
        assert!(
            elapsed < Duration::from_secs(crate::process::DRAIN_GRACE_SECS + 5),
            "run_shell took {elapsed:?}; it must return within the drain grace"
        );
    }
}

/// P7-1b: the pieces `run_shell` builds an argv from, on every platform.
#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn base64_matches_the_known_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
        // the two padding lengths in between
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
    }

    /// AC10 (3): an empty or relative PATH entry would let a shell find the
    /// program in the check's cwd, so the child gets PATH without them.
    #[test]
    fn a_path_with_relative_entries_is_filtered_and_an_absolute_one_is_left_alone() {
        let (a, b) = if cfg!(windows) {
            (r"C:\tools", r"C:\Windows\System32")
        } else {
            ("/usr/local/bin", "/usr/bin")
        };
        let sep = if cfg!(windows) { ";" } else { ":" };
        let clean = format!("{a}{sep}{b}");
        assert_eq!(path_without_relative_entries(Some(std::ffi::OsStr::new(&clean))), None);
        assert_eq!(path_without_relative_entries(None), None);
        for dirty in [
            format!(".{sep}{a}{sep}{b}"),
            format!("{a}{sep}{sep}{b}"),
            format!("{a}{sep}bin{sep}{b}"),
        ] {
            let got = path_without_relative_entries(Some(std::ffi::OsStr::new(&dirty)))
                .unwrap_or_else(|| panic!("{dirty} has a relative entry"));
            let got: Vec<_> = std::env::split_paths(&got).collect();
            assert_eq!(got, [std::path::PathBuf::from(a), std::path::PathBuf::from(b)], "{dirty}");
        }
    }

    #[test]
    fn split_argv_groups_double_quotes_and_drops_them() {
        assert_eq!(split_argv("git --version"), ["git", "--version"]);
        assert_eq!(split_argv("  a   b  "), ["a", "b"]);
        assert_eq!(split_argv(r#"findstr /c:"x y" words.txt"#), ["findstr", "/c:x y", "words.txt"]);
        assert_eq!(split_argv(r#"a "" b"#), ["a", "", "b"]);
        // no backslash escaping: the backslash stays, the quote still toggles
        assert_eq!(split_argv(r#"a\"b c"#), [r"a\b c"]);
        assert!(split_argv("   ").is_empty());
    }

    #[test]
    fn shell_names_round_trip_and_match_the_catalogue() {
        for name in crate::checks::SHELLS {
            assert_eq!(Shell::parse(name).unwrap().name(), name);
        }
        assert!(Shell::parse("pwsh").is_none());
        assert!(Shell::Argv.available());
        assert_eq!(Shell::Cmd.available(), cfg!(windows));
        assert_eq!(Shell::PowerShell.available(), cfg!(windows));
        assert_eq!(Shell::Sh.available(), cfg!(target_os = "linux"));
        assert_eq!(Shell::Bash.available(), cfg!(target_os = "linux"));
    }

    /// An absolute path for argv-building tests; nothing is run.
    fn some_program() -> std::path::PathBuf {
        if cfg!(windows) {
            std::path::PathBuf::from(r"C:\tools dir\check.ps1")
        } else {
            std::path::PathBuf::from("/opt/tools dir/check.sh")
        }
    }

    /// AC14 (b): PowerShell gets the program from `THOTH_CHECK_PROGRAM`
    /// through the call operator, and the arguments as written.
    #[test]
    fn the_powershell_argv_carries_the_utf16le_script_in_base64() {
        let argv = shell_argv(Shell::PowerShell, &some_program(), r".\check.ps1 a b");
        assert_eq!(
            &argv[..6],
            ["powershell.exe", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-EncodedCommand"]
        );
        let script = powershell_script("a b");
        assert_eq!(
            script,
            "$ErrorActionPreference='Stop'; & $env:THOTH_CHECK_PROGRAM a b; \
             if ($null -ne $LASTEXITCODE) { exit $LASTEXITCODE }"
        );
        let utf16: Vec<u8> = script.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(argv[6], base64(&utf16));
        assert_eq!(argv.len(), 7);
        // no arguments: no stray space after the program
        assert_eq!(
            powershell_script(""),
            "$ErrorActionPreference='Stop'; & $env:THOTH_CHECK_PROGRAM; \
             if ($null -ne $LASTEXITCODE) { exit $LASTEXITCODE }"
        );
    }

    /// AC14 (b): cmd reads the program from `THOTH_CHECK_PROGRAM` inside the
    /// `/S` outer quotes; sh and bash get it, and each argument, as argv to a
    /// fixed `"$0" "$@"`; argv sends the program itself.
    #[test]
    fn the_other_shells_get_the_program_by_path_and_the_arguments_as_written() {
        let p = some_program();
        let shown = p.display().to_string();
        assert_eq!(
            shell_argv(Shell::Cmd, &p, "tool --flag=1  x"),
            ["cmd", "/D", "/V:OFF", "/S", "/C", "\"\"%THOTH_CHECK_PROGRAM%\" --flag=1  x\""]
        );
        assert_eq!(
            shell_argv(Shell::Cmd, &p, "tool"),
            ["cmd", "/D", "/V:OFF", "/S", "/C", "\"\"%THOTH_CHECK_PROGRAM%\"\""]
        );
        for (shell, name) in [(Shell::Sh, "sh"), (Shell::Bash, "bash")] {
            assert_eq!(
                shell_argv(shell, &p, "./check.sh a\tb"),
                [name, "-c", "\"$0\" \"$@\"", shown.as_str(), "a", "b"]
            );
        }
        assert_eq!(
            shell_argv(Shell::Argv, &p, r#""./my tool" --version x"#),
            [shown.as_str(), "--version", "x"]
        );
        assert_eq!(shell_args_text("  tool  a b  "), "a b");
        assert_eq!(shell_args_text("tool"), "");
    }

    #[test]
    fn a_shell_this_platform_lacks_is_an_error_not_a_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let missing = if cfg!(windows) { Shell::Bash } else { Shell::PowerShell };
        let e = run_shell(missing, &some_program(), "tool", dir.path(), std::time::Duration::from_secs(5))
            .err()
            .expect("must be an error")
            .to_string();
        assert!(e.contains("not available"), "{e}");
    }

    /// AC14 (b): there is no mode that hands the shell a program to find.
    #[test]
    fn a_program_that_is_not_an_absolute_path_is_an_error_not_a_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let shell = if cfg!(windows) { Shell::Cmd } else { Shell::Sh };
        let e = run_shell(shell, Path::new("tool"), "tool", dir.path(), std::time::Duration::from_secs(5))
            .err()
            .expect("must be an error")
            .to_string();
        assert!(e.contains("absolute path"), "{e}");
    }

    #[test]
    fn an_argv_check_runs_without_a_shell() {
        let dir = tempfile::tempdir().unwrap();
        let git = crate::checks::resolve_exe("git", dir.path(), ".").expect("git on PATH");
        let out = run_shell(Shell::Argv, &git, "git --version", dir.path(), std::time::Duration::from_secs(30))
            .unwrap();
        assert_eq!(out.exit_code, Some(0), "stderr {:?}", out.stderr);
        assert!(out.stdout.contains("git version"), "got {:?}", out.stdout);
        assert_eq!(out.argv, [git.display().to_string(), "--version".to_string()]);
    }

    /// Windows: a program that resolves to anything but an `.exe` is refused
    /// with a pointer to `shell cmd` - a bare name that only resolves to a
    /// `.cmd`, and (AC11 (a)) the `.cmd` or an extensionless file named in full.
    #[cfg(windows)]
    #[test]
    fn a_bare_argv_program_that_is_only_a_batch_file_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("only-batch-zz.cmd"), "@exit /b 0\r\n").unwrap();
        std::fs::write(dir.path().join("no-ext-zz"), "x").unwrap();
        let reason = argv_refusal("./only-batch-zz", dir.path()).expect("must refuse");
        assert!(reason.contains("use shell cmd for this check"), "{reason}");
        for named in ["./only-batch-zz.cmd", "./no-ext-zz"] {
            let reason = argv_refusal(named, dir.path()).expect("must refuse");
            assert!(reason.contains("not an .exe"), "{reason}");
        }
        std::fs::write(dir.path().join("tool-zz.EXE"), "x").unwrap();
        assert!(argv_refusal("./tool-zz.EXE", dir.path()).is_none());
        let e = run_shell(
            Shell::Argv,
            &dir.path().join("only-batch-zz"),
            "./only-batch-zz",
            dir.path(),
            std::time::Duration::from_secs(5),
        )
            .err()
            .expect("must be an error")
            .to_string();
        assert!(e.contains("use shell cmd for this check"), "{e}");
        // an exe found first is fine
        assert!(argv_refusal("git", dir.path()).is_none());
    }
}
