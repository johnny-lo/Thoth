//! Linux process control: systemd `--user` units and cgroup v2.
//!
//! Design, and the decisions behind it: docs/post-phase2/linux-support-scope-2026-09-26.md §3.
//! In one paragraph: the runner (`spawn_detached`) runs as a transient systemd
//! `--user` service with `Delegate=yes`, its own process in the unit's
//! `runner` subgroup. Each worker (`spawn_child`) joins its own sibling cgroup,
//! `worker-<pid>`, between `fork` and `exec`, so nothing it ever starts is
//! outside it - not even a `setsid`/double-forked grandchild, because a
//! process cannot leave a cgroup by changing session or parent.
//! `KillMode=control-group` + `KillSignal=SIGKILL` on the unit is the Linux
//! counterpart of the Windows Job Object's `KILL_ON_JOB_CLOSE`: when the
//! runner exits or dies, systemd kills everything still in the unit's cgroup,
//! worker trees included. Writing `cgroup.kill` is the counterpart of
//! `TerminateJobObject`, and the deterministic `worker-<pid>` name is the
//! counterpart of the `thoth-job-<pid>` job name, so a different thoth
//! process (`cancel`) can find the same group.
//!
//! No fallback when systemd is not there (decision 2 in the scope document):
//! `spawn_detached`/`ensure_supervised` fail on a missing `systemd-run`, and
//! `spawn_child` refuses to start a worker anywhere but inside a runner unit.

use super::{ChildSpec, ProcessController};
use anyhow::{anyhow, bail, Context, Result};
use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

const CGROUP_FS: &str = "/sys/fs/cgroup";
/// Every runner unit is placed here, so `systemctl --user status
/// thoth.slice` lists every live run at once, and a worker group can be
/// found by name under it (`worker_cgroup_for`).
const SLICE: &str = "thoth.slice";
const UNIT_PREFIX: &str = "thoth-";
/// The `DelegateSubgroup=` the runner's own process lives in. Workers are its
/// siblings, never its children: cgroup v2 does not let a cgroup that has
/// processes of its own also enable controllers for children, and keeping the
/// runner in a leaf keeps that door open.
const RUNNER_SUBGROUP: &str = "runner";
const WORKER_PREFIX: &str = "worker-";

/// The unit properties every runner unit gets, `spawn_detached` and
/// `ensure_supervised` alike.
///
/// - `Type=exec`: `systemd-run` returns only once the unit's main process has
///   `exec`'d, so a start that fails is reported by `systemd-run` itself.
/// - `Delegate=yes` + `DelegateSubgroup=runner`: the unit's cgroup subtree
///   belongs to this user, so the runner may create `worker-<pid>` beside its
///   own `runner` leaf and move a process into it.
/// - `KillMode=control-group` + `KillSignal=SIGKILL`: the moment the main
///   process (the runner) ends, for any reason, everything left in the unit is
///   killed outright - Windows' `KILL_ON_JOB_CLOSE`, which is a hard kill too.
const UNIT_PROPERTIES: &[&str] = &[
    "Type=exec",
    "Delegate=yes",
    "DelegateSubgroup=runner",
    "KillMode=control-group",
    "KillSignal=SIGKILL",
];

/// The variables systemd sets for each unit itself. Forwarding the caller's
/// copies (when the caller is itself running in some unit) would make the new
/// unit claim someone else's identity to journald and to `sd_notify`, or -
/// `--setenv` wins over the manager's own values - point it at someone else's
/// resources. Review finding 5: a desktop session's shell inherits
/// `MEMORY_PRESSURE_WATCH` from gnome-shell's unit, which would have aimed the
/// runner at gnome-shell's `memory.pressure`.
const SYSTEMD_OWN_ENV: &[&str] = &[
    "INVOCATION_ID",
    "JOURNAL_STREAM",
    "SYSTEMD_EXEC_PID",
    "MANAGERPID",
    "MAINPID",
    "NOTIFY_SOCKET",
    "LISTEN_PID",
    "LISTEN_FDS",
    "LISTEN_FDNAMES",
    "WATCHDOG_PID",
    "WATCHDOG_USEC",
    "MEMORY_PRESSURE_WATCH",
    "MEMORY_PRESSURE_WRITE",
    "LOG_NAMESPACE",
    "CREDENTIALS_DIRECTORY",
    "RUNTIME_DIRECTORY",
    "STATE_DIRECTORY",
    "CACHE_DIRECTORY",
    "LOGS_DIRECTORY",
    "CONFIGURATION_DIRECTORY",
    "FDSTORE",
    "SERVICE_RESULT",
];

/// Prefixes of per-unit variable families systemd sets (`TRIGGER_UNIT`,
/// `TRIGGER_PATH`, ...; `MONITOR_SERVICE_RESULT`, `MONITOR_UNIT`, ...).
const SYSTEMD_OWN_ENV_PREFIXES: &[&str] = &["TRIGGER_", "MONITOR_"];

/// `spawn_detached`'s wrapper: writes its own pid to `$0`, then becomes the
/// real program (`exec` keeps the pid), so the pid file names the runner even
/// if it exits a millisecond later. Runs with `--expand-environment=no`: by
/// default systemd would rewrite `$$`/`$0`/`$@` in the command line itself
/// before `sh` ever saw them (measured 2026-09-26, systemd 255).
const PIDFILE_WRAPPER: &str = r#"echo $$ > "$0" && exec "$@""#;

/// How long `spawn_detached` waits for the wrapper's pid file. `systemd-run`
/// has already returned once the wrapper `exec`'d, so this is normally a
/// single poll.
const PIDFILE_WAIT: Duration = Duration::from_secs(5);

pub struct LinuxProcessController;

/// The cgroup v2 path of a process, from the `0::` line of `/proc/<who>/cgroup`
/// (`who` is a pid or `self`). The path is relative to `/sys/fs/cgroup`,
/// starting with `/`. `Ok(None)`: no such process. A process whose cgroup
/// directory has already been removed (the kernel appends ` (deleted)` to the
/// path) reports the path it was in; the directory may be gone, which every
/// reader here already treats as empty.
fn cgroup_path_of(who: &str) -> Result<Option<String>> {
    match std::fs::read_to_string(format!("/proc/{who}/cgroup")) {
        Ok(text) => Ok(parse_cgroup_v2(&text)),
        Err(e) if is_gone(&e) => Ok(None),
        Err(e) => Err(anyhow!(e).context(format!("cannot read /proc/{who}/cgroup"))),
    }
}

fn parse_cgroup_v2(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .map(|p| {
            let p = p.trim_end();
            p.strip_suffix(" (deleted)").unwrap_or(p).to_string()
        })
}

/// A process that has exited between listing and reading reports `ESRCH`
/// rather than `ENOENT` for some `/proc` files; both mean "gone".
fn is_gone(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(libc::ESRCH)
}

/// If `cgroup` is the `runner` subgroup of a thoth runner unit, the unit's
/// own cgroup directory (absolute).
fn runner_unit_dir(cgroup: &str) -> Option<PathBuf> {
    let (parent, last) = cgroup.rsplit_once('/')?;
    if last != RUNNER_SUBGROUP {
        return None;
    }
    let unit = parent.rsplit('/').next()?;
    if !(unit.starts_with(UNIT_PREFIX) && unit.ends_with(".service")) {
        return None;
    }
    Some(PathBuf::from(format!("{CGROUP_FS}{parent}")))
}

/// The user manager's cgroup directory (`.../user@<uid>.service`): read off
/// `own` (this process's cgroup) when this process is somewhere under it,
/// otherwise systemd's standard layout. `cancel` run from an SSH session is
/// the "otherwise": that lives in `session-N.scope`, outside the manager.
fn user_manager_dir_from(own: Option<&str>, uid: u32) -> PathBuf {
    let marker = format!("user@{uid}.service");
    if let Some(own) = own {
        let mut prefix = String::new();
        for part in own.split('/').filter(|p| !p.is_empty()) {
            prefix.push('/');
            prefix.push_str(part);
            if part == marker {
                return PathBuf::from(format!("{CGROUP_FS}{prefix}"));
            }
        }
    }
    PathBuf::from(format!("{CGROUP_FS}/user.slice/user-{uid}.slice/{marker}"))
}

fn user_manager_dir() -> PathBuf {
    let own = cgroup_path_of("self").ok().flatten();
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    user_manager_dir_from(own.as_deref(), uid)
}

/// The worker group `kill_tree`/`is_alive` act on for `pid`, if `pid` roots
/// one: its own `worker-<pid>` directory.
///
/// A live `pid` is looked up by its own `/proc/<pid>/cgroup` only - if it is
/// alive but not in `worker-<pid>`, it is not a worker root, whatever
/// directories exist. Only once `pid` is gone (the orphaned-descendants case
/// the Job Object name exists for on Windows) are runner units searched for a
/// `worker-<pid>` directory by name.
fn worker_cgroup_for(pid: u32) -> Option<PathBuf> {
    let name = format!("{WORKER_PREFIX}{pid}");
    if let Ok(Some(cg)) = cgroup_path_of(&pid.to_string()) {
        return (cg.rsplit('/').next() == Some(name.as_str()))
            .then(|| PathBuf::from(format!("{CGROUP_FS}{cg}")));
    }
    let slice = user_manager_dir().join(SLICE);
    std::fs::read_dir(slice)
        .ok()?
        .flatten()
        .map(|unit| unit.path().join(&name))
        .find(|candidate| candidate.is_dir())
}

/// If `pid` is the main process of a thoth runner unit, that unit's
/// cgroup directory. "Main" means: in the unit's `runner` subgroup, and
/// started by something outside it (systemd) - the runner's own `git`
/// children share its subgroup but have the runner as their parent.
fn runner_unit_of_main(pid: u32) -> Option<PathBuf> {
    let cg = cgroup_path_of(&pid.to_string()).ok()??;
    let unit = runner_unit_dir(&cg)?;
    let (_, ppid) = proc_stat(pid).ok()??;
    let parent_cg = cgroup_path_of(&ppid.to_string()).ok().flatten();
    (parent_cg.as_deref() != Some(cg.as_str())).then_some(unit)
}

/// Whether anything is left in the cgroup at `dir`. A directory that is gone
/// is empty by definition (systemd removes a stopped unit's cgroup).
fn populated(dir: &Path) -> Result<bool> {
    match std::fs::read_to_string(dir.join("cgroup.events")) {
        Ok(text) => Ok(text.lines().any(|l| l.trim() == "populated 1")),
        Err(e) if is_gone(&e) => Ok(false),
        Err(e) => Err(anyhow!(e).context(format!("cannot read {}/cgroup.events", dir.display()))),
    }
}

/// Kills every process in the cgroup at `dir` and its descendants (kernel
/// 5.14+; this machine's is far newer). Returns as soon as the kill is
/// issued - `populated` says when it has landed.
fn cgroup_kill(dir: &Path) -> std::io::Result<()> {
    std::fs::write(dir.join("cgroup.kill"), "1")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcState {
    Running,
    /// Exited, not yet reaped. Treated as dead everywhere, the way `tasklist`
    /// stops listing an exited process even while handles to it are open.
    Zombie,
}

/// `(state, ppid)` from `/proc/<pid>/stat`. `Ok(None)`: no such process.
fn proc_stat(pid: u32) -> Result<Option<(ProcState, u32)>> {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(text) => parse_stat(&text)
            .map(Some)
            .ok_or_else(|| anyhow!("unparseable /proc/{pid}/stat: {text:?}")),
        Err(e) if is_gone(&e) => Ok(None),
        Err(e) => Err(anyhow!(e).context(format!("cannot read /proc/{pid}/stat"))),
    }
}

/// `pid (comm) S ppid ...`: `comm` may itself contain spaces and `)`, so the
/// fields are read after the *last* `)`.
fn parse_stat(text: &str) -> Option<(ProcState, u32)> {
    let rest = &text[text.rfind(')')? + 1..];
    let mut fields = rest.split_whitespace();
    let state = match fields.next()? {
        "Z" | "X" | "x" => ProcState::Zombie,
        _ => ProcState::Running,
    };
    let ppid = fields.next()?.parse().ok()?;
    Some((state, ppid))
}

fn root_alive(pid: u32) -> Result<bool> {
    Ok(matches!(proc_stat(pid)?, Some((ProcState::Running, _))))
}

/// Every live descendant of `pid`, found by walking `/proc`'s parent links -
/// the Linux spelling of `taskkill /T`'s tree walk, with the same limit: a
/// descendant already reparented away from the tree is not found this way.
fn descendants(pid: u32) -> Vec<u32> {
    let mut links: Vec<(u32, u32)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc") {
        for e in entries.flatten() {
            let Some(p) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if let Ok(Some((_, ppid))) = proc_stat(p) {
                links.push((p, ppid));
            }
        }
    }
    let mut found = vec![pid];
    let mut i = 0;
    while i < found.len() {
        let parent = found[i];
        for &(p, ppid) in &links {
            if ppid == parent && !found.contains(&p) {
                found.push(p);
            }
        }
        i += 1;
    }
    found.remove(0);
    found
}

/// `pid` as a `pid_t` that is safe to hand `kill`: `None` for 0 and 1 (the
/// caller's own group; init) and for anything above i32::MAX, which `as`
/// would turn negative - and a negative pid means a process group, `-1`
/// every process this user owns.
fn signal_target(pid: u32) -> Option<libc::pid_t> {
    libc::pid_t::try_from(pid).ok().filter(|&p| p > 1)
}

/// `SIGKILL` to `pid`, its live descendants, and - when `pid` leads a process
/// group of its own that is not this process's (`run_shell`'s `sh`) - that
/// whole group. Never signals this process itself.
fn kill_live_tree(pid: u32) {
    // Review finding 2: converted with a check, never `as`. A u32 above
    // i32::MAX would come out negative, and `kill(-1, SIGKILL)` signals every
    // process this user owns. No real pid is that large; a corrupt one from
    // the database must still not get that far.
    let Some(root) = signal_target(pid) else {
        return;
    };
    let me = std::process::id();
    // Read before anything is killed: a reaped leader no longer has a pgid.
    // SAFETY: getpgid/getpgrp have no memory-safety preconditions; a stale or
    // foreign pid just yields -1/ESRCH.
    let pgid = unsafe { libc::getpgid(root) };
    let own_pgid = unsafe { libc::getpgrp() };
    let mut targets = descendants(pid);
    targets.push(pid);
    for p in targets {
        let Some(target) = signal_target(p) else {
            continue;
        };
        if p != me {
            // SAFETY: kill with a positive pid only sends a signal.
            unsafe {
                libc::kill(target, libc::SIGKILL);
            }
        }
    }
    if pgid == root && pgid > 1 && pgid != own_pgid {
        // SAFETY: a negative pid signals that process group.
        unsafe {
            libc::kill(-pgid, libc::SIGKILL);
        }
    }
}

/// Where the wrapper's pid file goes: the per-user runtime directory when
/// there is one (`/run/user/<uid>`, not world-writable), else the temp dir.
fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

fn new_unit_name() -> String {
    format!("{UNIT_PREFIX}{}.service", ulid::Ulid::generate())
}

/// `NAME=value` for every variable a runner unit must inherit.
///
/// A systemd service does not inherit its starter's environment at all, and
/// the runner needs nearly all of it: `THOTH_HOME`, `THOTH_CLAUDE_BIN`,
/// `PATH` (so `claude` and `git` resolve exactly as they did for the caller),
/// `HOME`, the tests' `FAKE_*` switches, the session bus address. So
/// everything is forwarded except:
/// - names systemd rejects outright (it then refuses the whole unit, measured
///   2026-09-26 with bash's exported-function `BASH_FUNC_x%%`): anything but
///   `[A-Za-z_][A-Za-z0-9_]*`;
/// - values that are not UTF-8, which systemd also rejects;
/// - systemd's own per-unit variables (`SYSTEMD_OWN_ENV`,
///   `SYSTEMD_OWN_ENV_PREFIXES`).
fn forwardable_env(vars: impl Iterator<Item = (OsString, OsString)>) -> Vec<String> {
    let valid_name = |n: &str| {
        let mut chars = n.chars();
        matches!(chars.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
            && chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
    };
    vars.filter_map(|(k, v)| {
        let k = k.into_string().ok()?;
        let v = v.into_string().ok()?;
        let systemds = SYSTEMD_OWN_ENV.contains(&k.as_str())
            || SYSTEMD_OWN_ENV_PREFIXES.iter().any(|p| k.starts_with(p));
        (valid_name(&k) && !systemds).then(|| format!("{k}={v}"))
    })
    .collect()
}

/// A `systemd-run` invocation that will start `unit` as a runner unit, with
/// the caller's environment and `cwd`; the caller appends mode flags, `--` and
/// the command.
fn systemd_run(unit: &str, cwd: &Path) -> Result<Command> {
    let cwd = std::path::absolute(cwd)
        .with_context(|| format!("cannot make {} absolute", cwd.display()))?;
    let mut cmd = Command::new("systemd-run");
    // `--collect`: a unit whose runner exited non-zero is unloaded anyway,
    // instead of lingering as `failed` until someone runs `reset-failed`.
    cmd.args(["--user", "--quiet", "--collect", "--expand-environment=no"])
        .arg(format!("--unit={unit}"))
        .arg(format!("--slice={SLICE}"));
    let mut wd = OsString::from("--working-directory=");
    wd.push(cwd.as_os_str());
    cmd.arg(wd);
    for p in UNIT_PROPERTIES {
        cmd.arg("-p").arg(p);
    }
    for kv in forwardable_env(std::env::vars_os()) {
        cmd.arg(format!("--setenv={kv}"));
    }
    Ok(cmd)
}

/// The program `spawn_detached` will hand the wrapper, made absolute when it
/// is a path (the unit's working directory is `cwd`, not the caller's), and
/// checked to exist and be executable. The wrapper's `exec` failing would
/// otherwise go unnoticed - `systemd-run` only sees `sh` start - so this
/// stands in for the `spawn()` error Windows reports for a missing program.
fn resolve_program(program: &str) -> Result<OsString> {
    use std::os::unix::fs::PermissionsExt;
    let executable = |p: &Path| {
        p.metadata()
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    };
    if program.contains('/') {
        let abs = std::path::absolute(program)?;
        if executable(&abs) {
            return Ok(abs.into_os_string());
        }
    } else if let Some(path) = std::env::var_os("PATH") {
        if std::env::split_paths(&path).any(|d| executable(&d.join(program))) {
            return Ok(OsString::from(program));
        }
    }
    Err(std::io::Error::from(std::io::ErrorKind::NotFound))
        .with_context(|| format!("cannot start {program}"))
}

/// Waits for the wrapper's pid file. Only a complete line counts: `echo`
/// writes the pid and its newline in one `write`, so a read that lands between
/// the redirect's truncation and that write sees nothing rather than a prefix.
fn wait_for_pidfile(path: &Path, unit: &str) -> Result<u32> {
    let deadline = Instant::now() + PIDFILE_WAIT;
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Some(pid) = text.strip_suffix('\n').and_then(|l| l.trim().parse().ok()) {
                return Ok(pid);
            }
        }
        if Instant::now() >= deadline {
            bail!(
                "{unit} started but never wrote its pid to {} within {PIDFILE_WAIT:?}; see \
                 `journalctl --user -u {unit}`",
                path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Best-effort `systemctl --user stop`, for a unit `spawn_detached` is giving
/// up on. With `KillSignal=SIGKILL` this kills whatever is in it at once.
fn stop_unit(unit: &str) {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", unit])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Makes sure the calling `run-worker` is inside a runner unit; see
/// `process::ensure_supervised` for the contract. Outside one, it re-runs
/// `args` under this same binary in a fresh runner unit, in the foreground
/// (`--wait --pipe`: this process's stdin/stdout/stderr are the unit's, and
/// `systemd-run` exits with the unit's exit status - measured 2026-09-26), and
/// returns that status.
///
/// "Inside" is read off `/proc/self/cgroup`, never off an environment
/// variable: the worker inherits the runner's environment, so a marker
/// variable would also be true for anything the worker ran.
pub fn ensure_supervised(args: &[String], already_rerun: bool) -> Result<Option<i32>> {
    let own = cgroup_path_of("self")?.unwrap_or_default();
    if runner_unit_dir(&own).is_some() {
        return Ok(None);
    }
    if already_rerun {
        bail!(
            "run-worker was re-run inside a systemd --user unit but is still not in a thoth \
             runner unit's `{RUNNER_SUBGROUP}` subgroup (its cgroup is '{own}'). thoth on \
             Linux needs cgroup v2 and a systemd user manager that honours Delegate= and \
             DelegateSubgroup= (systemd 254+); refusing rather than re-running again."
        );
    }
    let exe = std::env::current_exe()?;
    let cwd = std::env::current_dir()?;
    let unit = new_unit_name();
    let status = systemd_run(&unit, &cwd)?
        .args(["--wait", "--pipe", "--"])
        .arg(&exe)
        .args(args)
        .arg("--supervised")
        .status()
        .context("could not run systemd-run (thoth on Linux needs a systemd --user session)")?;
    use std::os::unix::process::ExitStatusExt;
    Ok(Some(
        status
            .code()
            .or_else(|| status.signal().map(|s| 128 + s))
            .unwrap_or(1),
    ))
}

/// Path bytes for `spawn_child`'s `pre_exec`, prepared before `fork` so the
/// child never allocates: after `fork` in a process with other threads, only
/// async-signal-safe calls are allowed until `exec`.
struct WorkerCgroupJoin {
    /// `<unit dir>/worker-`; the child appends its own pid.
    prefix: Vec<u8>,
}

impl WorkerCgroupJoin {
    const BUF: usize = 4096;
    const PROCS: &'static [u8] = b"/cgroup.procs";

    fn new(unit_dir: &Path) -> Result<Self> {
        let mut prefix = unit_dir.as_os_str().as_bytes().to_vec();
        prefix.push(b'/');
        prefix.extend_from_slice(WORKER_PREFIX.as_bytes());
        // pid (at most 10 digits) + "/cgroup.procs" + NUL
        if prefix.contains(&0) || prefix.len() + 10 + Self::PROCS.len() + 1 > Self::BUF {
            bail!("unusable cgroup path for a worker group: {}", unit_dir.display());
        }
        Ok(Self { prefix })
    }

    /// Runs in the child between `fork` and `exec`: creates
    /// `worker-<own pid>` and moves this process into it (writing `0` to
    /// `cgroup.procs` moves the writer). Everything it starts from here on is
    /// born inside, so there is no window like the Windows one between
    /// `spawn()` and `AssignProcessToJobObject`.
    ///
    /// Refuses a `worker-<pid>` that already exists (`EEXIST` fails the
    /// spawn): it can only be a group this unit never released, and joining it
    /// would put this worker in with whatever is left there - the Linux
    /// version of `create_and_assign_job` refusing a stale job name.
    fn join_as_self(&self) -> std::io::Result<()> {
        let mut buf = [0u8; Self::BUF];
        let mut n = self.prefix.len();
        buf[..n].copy_from_slice(&self.prefix);
        // SAFETY: getpid is async-signal-safe and cannot fail.
        n += write_decimal(&mut buf[n..], unsafe { libc::getpid() } as u32);
        buf[n] = 0;
        // SAFETY: `buf` is NUL-terminated at `n`; mkdir only reads it.
        if unsafe { libc::mkdir(buf.as_ptr().cast(), 0o755) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        buf[n..n + Self::PROCS.len()].copy_from_slice(Self::PROCS);
        buf[n + Self::PROCS.len()] = 0;
        // SAFETY: as above; the fd is closed on every path below.
        let fd = unsafe { libc::open(buf.as_ptr().cast(), libc::O_WRONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: writes one byte from a static buffer to an fd we own.
        let written = unsafe { libc::write(fd, b"0".as_ptr().cast(), 1) };
        let result = if written == 1 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        };
        // SAFETY: `fd` came from the successful open above.
        unsafe {
            libc::close(fd);
        }
        result
    }
}

/// Writes `v` in decimal at the start of `out`, returning the length. No
/// allocation: `WorkerCgroupJoin::join_as_self` runs after `fork`.
fn write_decimal(out: &mut [u8], mut v: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut len = 0;
    loop {
        digits[len] = b'0' + (v % 10) as u8;
        len += 1;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    for (i, d) in digits[..len].iter().rev().enumerate() {
        out[i] = *d;
    }
    len
}

/// Best-effort cleanup after a failed `spawn_child`: the child may have
/// created (and joined) its `worker-<pid>` before its `exec` failed, and the
/// pid is not reported back. An empty directory is the only thing removed
/// (`rmdir` refuses a populated cgroup), so this cannot touch a live worker.
fn remove_empty_worker_cgroups(unit_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(unit_dir) else {
        return;
    };
    for e in entries.flatten() {
        let is_worker = e
            .file_name()
            .to_str()
            .is_some_and(|n| n.starts_with(WORKER_PREFIX));
        if is_worker && !populated(&e.path()).unwrap_or(true) {
            let _ = std::fs::remove_dir(e.path());
        }
    }
}

/// `/proc/<pid>/comm` is the executable's file name as `exec` was given it -
/// `claude` for `~/.local/bin/claude`, even though that is a symlink whose
/// target (`/proc/<pid>/exe`) is named after the version. The kernel keeps
/// only 15 bytes of it, so a name that fills all 15 may be cut short; then the
/// `exe` file name is used instead when it starts with those 15 bytes.
/// `Ok(None)` for a pid that is gone or a zombie, as `tasklist` would not list
/// either.
pub fn image_name(pid: u32) -> Result<Option<String>> {
    if !root_alive(pid)? {
        return Ok(None);
    }
    let comm = match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
        Ok(s) => s.trim_end_matches('\n').to_string(),
        Err(e) if is_gone(&e) => return Ok(None),
        Err(e) => return Err(anyhow!(e).context(format!("cannot read /proc/{pid}/comm"))),
    };
    if comm.len() == 15 {
        if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
            if let Some(name) = exe.file_name().and_then(|n| n.to_str()) {
                let name = name.strip_suffix(" (deleted)").unwrap_or(name);
                if name.starts_with(&comm) {
                    return Ok(Some(name.to_string()));
                }
            }
        }
    }
    Ok(Some(comm))
}

/// Kills whatever is left in this runner's `worker-<pid>` group and removes
/// the directory - what dropping the last Job Object handle does on Windows,
/// and the same contract: call it only once the run is over.
///
/// Looks only in this process's own unit, never searches: the runner calls
/// this after reaping the worker, so `pid` is free again and may already
/// belong to another runner's new worker, whose `worker-<pid>` a search could
/// find.
pub fn release_job(pid: u32) {
    let Some(unit_dir) = cgroup_path_of("self")
        .ok()
        .flatten()
        .and_then(|own| runner_unit_dir(&own))
    else {
        return;
    };
    let dir = unit_dir.join(format!("{WORKER_PREFIX}{pid}"));
    if !dir.is_dir() {
        return;
    }
    if let Err(e) = cgroup_kill(&dir) {
        if !is_gone(&e) {
            masked_eprintln!("thoth: warning: cgroup.kill on {} failed: {e}", dir.display());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while populated(&dir).unwrap_or(false) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Err(e) = std::fs::remove_dir(&dir) {
        if !is_gone(&e) {
            masked_eprintln!("thoth: warning: could not remove worker group {}: {e}", dir.display());
        }
    }
}

impl ProcessController for LinuxProcessController {
    /// Starts `program` as the main process of a new runner unit and returns
    /// its pid once it has `exec`'d. The process is systemd's child, not this
    /// one's: nothing this process holds (pipes a test harness is reading,
    /// a terminal) is inherited, which is what `clear_std_handle_inheritance`
    /// has to arrange by hand on Windows.
    fn spawn_detached(&self, program: &str, args: &[String], cwd: &Path) -> Result<u32> {
        let program = resolve_program(program)?;
        let unit = new_unit_name();
        let pidfile = runtime_dir().join(format!("{unit}.pid"));
        let _ = std::fs::remove_file(&pidfile);
        let out = systemd_run(&unit, cwd)?
            // stdout has nowhere useful to go; stderr goes to the journal so a
            // runner panic is still findable (`journalctl --user -u <unit>`).
            // Windows discards both; this is observability, not behaviour.
            .args(["-p", "StandardOutput=null", "-p", "StandardError=journal", "--"])
            .args(["/bin/sh", "-c", PIDFILE_WRAPPER])
            .arg(&pidfile)
            .arg(&program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .output()
            .context("could not run systemd-run (thoth on Linux needs a systemd --user session)")?;
        if !out.status.success() {
            bail!(
                "systemd-run could not start {} as {unit} (exit {:?}): {}",
                program.to_string_lossy(),
                out.status.code(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let pid = wait_for_pidfile(&pidfile, &unit);
        let _ = std::fs::remove_file(&pidfile);
        let pid = match pid {
            Ok(pid) => pid,
            Err(e) => {
                stop_unit(&unit);
                return Err(e);
            }
        };
        // Review finding 1: the whole design rests on the runner sitting in
        // its unit's `runner` subgroup (that is what `ensure_supervised` and
        // `spawn_child` check). A user manager that ignores `Delegate=`/
        // `DelegateSubgroup=` would start it somewhere else; refuse here, so
        // `dispatch` rolls the run back, instead of leaving a runner that
        // cannot start a worker. A runner that has already finished is not an
        // error: it ran. If `/proc/<pid>` is gone entirely (`Ok(None)`) there
        // is nothing to check. One that has exited but is not yet reaped
        // reports its `runner` subgroup with ` (deleted)` appended;
        // `parse_cgroup_v2` drops that note, so it is checked like a live one
        // (a manager that ignores Delegate= is still caught when the runner
        // exits at once).
        if let Ok(Some(cg)) = cgroup_path_of(&pid.to_string()) {
            if runner_unit_dir(&cg).is_none() {
                stop_unit(&unit);
                bail!(
                    "{unit} started pid {pid} in cgroup '{cg}', not in its own \
                     `{RUNNER_SUBGROUP}` subgroup; thoth on Linux needs cgroup v2 and a \
                     systemd user manager that honours Delegate= and DelegateSubgroup= \
                     (systemd 254+)"
                );
            }
        }
        Ok(pid)
    }

    /// Starts the harness child inside its own `worker-<pid>` group; see
    /// `WorkerCgroupJoin::join_as_self`. Only possible from inside a runner
    /// unit, which is where the runner always is (`ensure_supervised`); from
    /// anywhere else this refuses rather than start a worker nothing could
    /// fully kill.
    fn spawn_child(&self, spec: ChildSpec<'_>) -> Result<Child> {
        use std::os::unix::process::CommandExt;
        let own = cgroup_path_of("self")?.unwrap_or_default();
        let unit_dir = runner_unit_dir(&own).ok_or_else(|| {
            anyhow!(
                "a worker can only be started from inside a thoth runner unit (a systemd \
                 --user service thoth starts with Delegate=yes); this process is in cgroup \
                 '{own}'. Start runs with `thoth dispatch`."
            )
        })?;
        let join = WorkerCgroupJoin::new(&unit_dir)?;
        let mut cmd = Command::new(spec.program);
        cmd.args(spec.args)
            .current_dir(spec.cwd)
            .stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped());
        for (k, v) in spec.env {
            cmd.env(k, v);
        }
        for name in spec.env_remove {
            cmd.env_remove(name);
        }
        match spec.stderr_path {
            Some(p) => {
                let f = std::fs::File::create(p)?;
                cmd.stderr(Stdio::from(f));
            }
            None => {
                cmd.stderr(Stdio::null());
            }
        }
        // SAFETY: `join_as_self` makes only async-signal-safe syscalls on
        // memory prepared before the fork, and allocates nothing.
        unsafe {
            cmd.pre_exec(move || join.join_as_self());
        }
        match cmd.spawn() {
            Ok(child) => Ok(child),
            Err(e) => {
                remove_empty_worker_cgroups(&unit_dir);
                Err(e.into())
            }
        }
    }

    /// Kills the tree rooted at `pid` and confirms it is gone.
    ///
    /// - A worker root (`worker-<pid>`, found even after the root itself has
    ///   exited): the whole group, through `cgroup.kill`.
    /// - A runner unit's main process: the whole unit, runner and every
    ///   worker group in it - on Windows `taskkill /T` on the runner reaches
    ///   the worker as its child, and closing the job handle does the rest.
    /// - Then, for any pid, belt and braces: `pid`'s live descendants and its
    ///   own process group (`kill_live_tree`). For a pid in no group at all
    ///   (`run_shell`'s `sh`) this is the whole mechanism, like `taskkill /T`
    ///   alone for a pid with no job.
    ///
    /// Confirmed the Windows way: `is_alive` (plus, for a unit, the unit's
    /// cgroup being empty) must turn false within 2 s.
    fn kill_tree(&self, pid: u32) -> Result<()> {
        let worker = worker_cgroup_for(pid);
        let unit = if worker.is_none() {
            runner_unit_of_main(pid)
        } else {
            None
        };
        let group_err = worker
            .as_ref()
            .or(unit.as_ref())
            .and_then(|dir| cgroup_kill(dir).err().map(|e| (dir.clone(), e)))
            .filter(|(_, e)| !is_gone(e))
            .map(|(dir, e)| format!("cgroup.kill on {} failed: {e}", dir.display()));

        kill_live_tree(pid);

        for _ in 0..20 {
            let unit_busy = match &unit {
                Some(dir) => populated(dir)?,
                None => false,
            };
            if !self.is_alive(pid)? && !unit_busy {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        match group_err {
            Some(e) => bail!("pid {pid} was still alive 2s after kill_tree({pid}); {e}"),
            None => bail!("pid {pid} was still alive 2s after kill_tree({pid})"),
        }
    }

    /// Whether `pid`'s tree is alive: `pid` itself is running (not a zombie),
    /// or it rooted a worker group that still has a process in it - the
    /// orphaned descendant a look at `pid` alone cannot see, like a Job
    /// Object's active-process count.
    fn is_alive(&self, pid: u32) -> Result<bool> {
        if root_alive(pid)? {
            return Ok(true);
        }
        match worker_cgroup_for(pid) {
            Some(dir) => populated(&dir),
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Kills the pids it holds if a test panics before cleaning up itself.
    struct Guard(Vec<u32>);
    impl Drop for Guard {
        fn drop(&mut self) {
            for &p in &self.0 {
                if p > 1 && p != std::process::id() {
                    unsafe {
                        libc::kill(p as libc::pid_t, libc::SIGKILL);
                    }
                }
            }
        }
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

    fn running(pid: u32) -> bool {
        root_alive(pid).unwrap()
    }

    /// Field 6 of `/proc/<pid>/stat`: the session id.
    fn session_of(pid: u32) -> u32 {
        let text = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let rest = &text[text.rfind(')').unwrap() + 1..];
        rest.split_whitespace().nth(3).unwrap().parse().unwrap()
    }

    fn comm_of(pid: u32) -> String {
        std::fs::read_to_string(format!("/proc/{pid}/comm"))
            .unwrap_or_default()
            .trim_end()
            .to_string()
    }

    fn procs_in(dir: &Path) -> Vec<u32> {
        std::fs::read_to_string(dir.join("cgroup.procs"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }

    /// Runs the calling test's body inside a real runner unit. Returns `true`
    /// when this process already is in one (run the body now); otherwise
    /// re-runs just `test` of this same test binary inside a fresh unit,
    /// asserts it passed, and returns `false` (the caller returns at once).
    /// This is how a unit test can call `spawn_child` for real: it only works
    /// from inside a unit, by design.
    fn inside_runner_unit(test: &str) -> bool {
        let own = cgroup_path_of("self").unwrap().unwrap_or_default();
        if runner_unit_dir(&own).is_some() {
            return true;
        }
        let exe = std::env::current_exe().unwrap();
        let unit = new_unit_name();
        let out = systemd_run(&unit, &std::env::current_dir().unwrap())
            .unwrap()
            .args(["--wait", "--pipe", "--"])
            .arg(&exe)
            .args([test, "--exact", "--test-threads=1", "--nocapture"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "{test} failed inside runner unit {unit}: {}\n{stdout}\n{stderr}",
            out.status
        );
        // Review finding 7: a name libtest does not know runs zero tests and
        // still exits 0, which would make this whole test a silent no-op.
        assert!(
            stdout.contains("test result: ok. 1 passed"),
            "{test} did not run exactly one test inside {unit}:\n{stdout}\n{stderr}"
        );
        false
    }

    /// The name libtest knows a test in this module by.
    fn test_path(name: &str) -> String {
        let module = module_path!();
        let module = module.split_once("::").map(|(_, m)| m).unwrap_or(module);
        format!("{module}::{name}")
    }

    /// A worker with a grandchild that has escaped everything but the
    /// cgroup: it is started by a `setsid`'d intermediate `sh` that exits at
    /// once, so it is in another session and process group, and - reparented
    /// to the user manager - no `/proc` walk from the root finds it either
    /// (review finding 3: an earlier version left it a child of the root, so
    /// the walk alone could explain its death). Returns the child, its worker
    /// group, and the escaped grandchild's pid.
    fn spawn_escaping_tree(ws: &Path) -> (Child, PathBuf, u32) {
        let args = vec!["-c".to_string(), "setsid sh -c 'sleep 30 &'; sleep 30".to_string()];
        let child = LinuxProcessController
            .spawn_child(ChildSpec {
                program: "sh",
                args: &args,
                cwd: ws,
                env: &[],
                env_remove: &[],
                stderr_path: None,
                stdin: None,
            })
            .expect("spawn_child");
        let root = child.id();
        let dir = worker_cgroup_for(root).expect("the root must be in its own worker group");
        let mut escaped = None;
        // Only the `sleep` counts: the `setsid`'d intermediate `sh` also has
        // another session, and it exits at once - under load it can pass the
        // `running` check and be reaped before the `descendants` walk looks,
        // so the walk no longer links it to the root and a dying process
        // would be picked (seen once in a full-suite run on 2026-09-27).
        assert!(
            wait_until(5, || {
                escaped = procs_in(&dir).into_iter().find(|&p| {
                    p != root
                        && comm_of(p) == "sleep"
                        && running(p)
                        && session_of(p) != session_of(root)
                        && !descendants(root).contains(&p)
                });
                escaped.is_some()
            }),
            "the setsid grandchild never appeared in {}: {:?}",
            dir.display(),
            procs_in(&dir)
                .into_iter()
                .map(|p| (p, session_of(p)))
                .collect::<Vec<_>>()
        );
        (child, dir, escaped.unwrap())
    }

    #[test]
    fn a_worker_is_in_its_own_group_before_it_execs() {
        if !inside_runner_unit(&test_path("a_worker_is_in_its_own_group_before_it_execs")) {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let args = vec!["/proc/self/cgroup".to_string()];
        let mut child = LinuxProcessController
            .spawn_child(ChildSpec {
                program: "cat",
                args: &args,
                cwd: ws.path(),
                env: &[],
                env_remove: &[],
                stderr_path: None,
                stdin: None,
            })
            .unwrap();
        let pid = child.id();
        let mut out = String::new();
        std::io::Read::read_to_string(child.stdout.as_mut().unwrap(), &mut out).unwrap();
        child.wait().unwrap();
        // `cat` read its own cgroup after exec: it was already inside.
        assert!(
            out.trim_end().ends_with(&format!("/{WORKER_PREFIX}{pid}")),
            "cat {pid} saw itself in {out:?}"
        );
        release_job(pid);
        assert!(worker_cgroup_for(pid).is_none(), "release_job must remove the group");
    }

    #[test]
    fn spawn_child_then_kill_tree_clears_root_and_an_escaped_grandchild() {
        if !inside_runner_unit(&test_path(
            "spawn_child_then_kill_tree_clears_root_and_an_escaped_grandchild",
        )) {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let (mut child, _dir, escaped) = spawn_escaping_tree(ws.path());
        let root = child.id();
        let _guard = Guard(vec![root, escaped]);
        assert!(LinuxProcessController.is_alive(root).unwrap());

        LinuxProcessController.kill_tree(root).unwrap();
        let _ = child.wait();

        assert!(!LinuxProcessController.is_alive(root).unwrap());
        assert!(wait_until(2, || !running(escaped)), "the setsid grandchild survived kill_tree");
        release_job(root);
    }

    /// The Linux side of review finding 1: the root is killed alone, the
    /// escaped grandchild lives on, `is_alive` still reports the tree through
    /// the group, and `kill_tree` still reaches the grandchild through it.
    #[test]
    fn kill_tree_and_is_alive_reach_an_orphan_after_the_root_is_gone() {
        if !inside_runner_unit(&test_path(
            "kill_tree_and_is_alive_reach_an_orphan_after_the_root_is_gone",
        )) {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let (mut child, _dir, escaped) = spawn_escaping_tree(ws.path());
        let root = child.id();
        let _guard = Guard(vec![root, escaped]);

        unsafe {
            libc::kill(root as libc::pid_t, libc::SIGKILL);
        }
        child.wait().unwrap();
        assert!(!running(root));
        assert!(running(escaped), "the grandchild must survive a root-only kill");
        assert!(
            LinuxProcessController.is_alive(root).unwrap(),
            "is_alive must see the group's live orphan even though the root is gone"
        );

        LinuxProcessController.kill_tree(root).unwrap();
        assert!(!running(escaped), "kill_tree must reach the orphan through the group");
        assert!(!LinuxProcessController.is_alive(root).unwrap());
        release_job(root);
    }

    #[test]
    fn release_job_kills_leftovers_and_removes_the_group() {
        if !inside_runner_unit(&test_path("release_job_kills_leftovers_and_removes_the_group")) {
            return;
        }
        let ws = tempfile::tempdir().unwrap();
        let (mut child, dir, escaped) = spawn_escaping_tree(ws.path());
        let root = child.id();
        let _guard = Guard(vec![root, escaped]);
        unsafe {
            libc::kill(root as libc::pid_t, libc::SIGKILL);
        }
        child.wait().unwrap();

        release_job(root);
        assert!(!running(escaped), "release_job must kill what the worker left behind");
        assert!(!dir.exists(), "release_job must remove {}", dir.display());
    }

    #[test]
    fn spawn_child_refuses_to_start_a_worker_outside_a_runner_unit() {
        let own = cgroup_path_of("self").unwrap().unwrap_or_default();
        if runner_unit_dir(&own).is_some() {
            return; // only meaningful from outside one
        }
        let ws = tempfile::tempdir().unwrap();
        let err = LinuxProcessController
            .spawn_child(ChildSpec {
                program: "true",
                args: &[],
                cwd: ws.path(),
                env: &[],
                env_remove: &[],
                stderr_path: None,
                stdin: None,
            })
            .unwrap_err()
            .to_string();
        assert!(err.contains("runner unit"), "{err}");
    }

    /// `spawn_detached` returns the runner's own pid (the wrapper `exec`'d
    /// into it), fast, and `kill_tree` on that pid takes the whole unit -
    /// including a process the "runner" started that has escaped its session,
    /// process group and parentage, which only the unit's `cgroup.kill` can
    /// still reach (review finding 3).
    #[test]
    fn spawn_detached_returns_the_main_pid_and_kill_tree_takes_the_whole_unit() {
        let ws = tempfile::tempdir().unwrap();
        let args = vec!["-c".to_string(), "setsid sh -c 'sleep 30 &'; exec sleep 31".to_string()];
        let started = Instant::now();
        let pid = LinuxProcessController.spawn_detached("sh", &args, ws.path()).unwrap();
        let elapsed = started.elapsed();
        let mut guard = Guard(vec![pid]);
        assert!(elapsed < Duration::from_secs(3), "spawn_detached took {elapsed:?}");

        let cg = cgroup_path_of(&pid.to_string()).unwrap().unwrap();
        let unit = runner_unit_dir(&cg).unwrap_or_else(|| panic!("{pid} is in {cg}"));
        assert!(unit.parent().unwrap().ends_with(SLICE), "{}", unit.display());
        let runner_group = unit.join(RUNNER_SUBGROUP);
        let mut escaped = None;
        assert!(
            wait_until(5, || {
                escaped = procs_in(&runner_group).into_iter().find(|&p| {
                    // `sleep` only, for the reason given in `spawn_escaping_tree`.
                    p != pid
                        && comm_of(p) == "sleep"
                        && running(p)
                        && session_of(p) != session_of(pid)
                        && !descendants(pid).contains(&p)
                });
                escaped.is_some()
            }),
            "the escaped sleep never showed up in {}",
            runner_group.display()
        );
        let all = procs_in(&runner_group);
        guard.0.extend(&all);
        assert!(LinuxProcessController.is_alive(pid).unwrap());
        assert!(wait_until(5, || image_name(pid).unwrap().as_deref() == Some("sleep")));

        LinuxProcessController.kill_tree(pid).unwrap();
        for p in all {
            assert!(!running(p), "{p} survived kill_tree on the unit's main process");
        }
        assert!(!running(escaped.unwrap()), "the escaped sleep survived");
    }

    #[test]
    fn kill_tree_on_a_pid_that_is_already_gone_succeeds() {
        // Above any pid_max this kernel can be configured with (2^22).
        let gone = 0x7FFF_FFF0;
        assert!(!LinuxProcessController.is_alive(gone).unwrap());
        LinuxProcessController.kill_tree(gone).unwrap();
    }

    #[test]
    fn spawn_detached_reports_a_missing_program() {
        let ws = tempfile::tempdir().unwrap();
        let err = LinuxProcessController
            .spawn_detached("/definitely/not/a/real/thoth-test-binary", &[], ws.path())
            .unwrap_err();
        assert!(format!("{err:#}").contains("cannot start"), "{err:#}");
    }

    #[test]
    fn image_name_reads_comm_and_reports_a_missing_pid_as_none() {
        let me = image_name(std::process::id()).unwrap().unwrap();
        let exe = std::env::current_exe().unwrap();
        // The test binary's name is longer than 15 bytes, so this is the
        // `exe` fallback at work.
        assert_eq!(Some(me.as_str()), exe.file_name().unwrap().to_str());
        // Above any pid_max this kernel can be configured with (2^22).
        assert_eq!(image_name(0x7FFF_FFF0).unwrap(), None);
    }

    #[test]
    fn runner_unit_dir_recognises_only_a_runner_units_runner_subgroup() {
        let base = "/user.slice/user-1000.slice/user@1000.service/thoth.slice";
        assert_eq!(
            runner_unit_dir(&format!("{base}/thoth-01ABC.service/runner")),
            Some(PathBuf::from(format!("{CGROUP_FS}{base}/thoth-01ABC.service")))
        );
        // a worker group, the unit itself, a foreign unit, a plain scope
        assert_eq!(runner_unit_dir(&format!("{base}/thoth-01ABC.service/worker-42")), None);
        assert_eq!(runner_unit_dir(&format!("{base}/thoth-01ABC.service")), None);
        assert_eq!(runner_unit_dir(&format!("{base}/other.service/runner")), None);
        assert_eq!(runner_unit_dir("/user.slice/user-1000.slice/session-3.scope"), None);
        assert_eq!(runner_unit_dir(""), None);
    }

    #[test]
    fn parse_cgroup_v2_reads_the_v2_line_and_strips_the_deleted_suffix() {
        let base = "/user.slice/user-1000.slice/user@1000.service/thoth.slice/thoth-01ABC.service/runner";
        assert_eq!(parse_cgroup_v2(&format!("0::{base}\n")), Some(base.to_string()));
        assert_eq!(parse_cgroup_v2(&format!("0::{base} (deleted)\n")), Some(base.to_string()));
        assert_eq!(
            parse_cgroup_v2(&format!("12:memory:/foo\n0::{base} (deleted)\n")),
            Some(base.to_string())
        );
        // the kernel's note always has the space; without it, it is the path
        assert_eq!(parse_cgroup_v2("0::/a/b(deleted)\n"), Some("/a/b(deleted)".to_string()));
        assert_eq!(parse_cgroup_v2("12:memory:/foo\n"), None);
        assert_eq!(parse_cgroup_v2(""), None);
        // the path a runner that exited at once reported in verify
        assert_eq!(
            runner_unit_dir(&parse_cgroup_v2(&format!("0::{base} (deleted)\n")).unwrap()),
            Some(PathBuf::from(format!(
                "{CGROUP_FS}/user.slice/user-1000.slice/user@1000.service/thoth.slice/thoth-01ABC.service"
            )))
        );
    }

    #[test]
    fn parse_stat_reads_state_and_ppid_after_the_last_paren() {
        assert_eq!(
            parse_stat("42 (sleep) S 7 42 42 0 -1"),
            Some((ProcState::Running, 7))
        );
        // `comm` can hold spaces and parentheses of its own
        assert_eq!(
            parse_stat("42 (a) b) Z 9 42 42 0"),
            Some((ProcState::Zombie, 9))
        );
        assert_eq!(parse_stat("42 (x) X 1"), Some((ProcState::Zombie, 1)));
        assert_eq!(parse_stat("garbage"), None);
    }

    #[test]
    fn user_manager_dir_comes_from_the_own_cgroup_or_the_standard_layout() {
        assert_eq!(
            user_manager_dir_from(
                Some("/user.slice/user-1000.slice/user@1000.service/app.slice/x.scope"),
                1000
            ),
            PathBuf::from(format!("{CGROUP_FS}/user.slice/user-1000.slice/user@1000.service"))
        );
        // an SSH session is outside the manager
        assert_eq!(
            user_manager_dir_from(Some("/user.slice/user-1000.slice/session-3.scope"), 1000),
            PathBuf::from(format!("{CGROUP_FS}/user.slice/user-1000.slice/user@1000.service"))
        );
        // a look-alike component is not the manager
        assert_eq!(
            user_manager_dir_from(Some("/x/user@1000.service.d/y"), 1000),
            PathBuf::from(format!("{CGROUP_FS}/user.slice/user-1000.slice/user@1000.service"))
        );
    }

    #[test]
    fn forwardable_env_drops_what_systemd_would_reject_and_its_own_variables() {
        let vars = [
            ("THOTH_HOME", "/tmp/h"),
            ("FAKE_SCENARIO", "ok"),
            ("MULTI", "a\nb"),
            ("_under", "x"),
            ("BASH_FUNC_x%%", "() { :; }"),
            ("9LIVES", "x"),
            ("INVOCATION_ID", "abc"),
            ("MEMORY_PRESSURE_WATCH", "/sys/fs/cgroup/x/memory.pressure"),
            ("TRIGGER_UNIT", "x.timer"),
            ("MONITOR_UNIT", "x.service"),
            ("", "x"),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), OsString::from(v)));
        assert_eq!(
            forwardable_env(vars),
            vec!["THOTH_HOME=/tmp/h", "FAKE_SCENARIO=ok", "MULTI=a\nb", "_under=x"]
        );
        let non_utf8 = OsString::from(std::ffi::OsStr::from_bytes(b"\xff"));
        assert!(forwardable_env([(OsString::from("K"), non_utf8)].into_iter()).is_empty());
    }

    /// Review finding 2, tested on the conversion alone: a regression here
    /// must not be discovered by `kill(-1, SIGKILL)`-ing the test runner.
    #[test]
    fn signal_target_refuses_pids_kill_would_read_as_groups_or_init() {
        assert_eq!(signal_target(0), None);
        assert_eq!(signal_target(1), None);
        assert_eq!(signal_target(u32::MAX), None);
        assert_eq!(signal_target(0x8000_0000), None);
        assert_eq!(signal_target(2), Some(2));
        assert_eq!(signal_target(i32::MAX as u32), Some(i32::MAX));
    }

    #[test]
    fn write_decimal_writes_every_digit_without_allocating() {
        let mut buf = [0u8; 10];
        for (v, s) in [(0u32, "0"), (7, "7"), (42, "42"), (4_194_304, "4194304"), (u32::MAX, "4294967295")] {
            let n = write_decimal(&mut buf, v);
            assert_eq!(&buf[..n], s.as_bytes());
        }
    }
}
