use super::{ChildSpec, ProcessController};
use anyhow::Result;
use std::path::Path;
use std::process::Child;

// This whole module is `#[cfg(windows)]` (see `process/mod.rs`); the per-item
// gates below predate that and are kept only because they are harmless. The
// `#[cfg(not(windows))]` stubs that used to live here are gone: Linux has its
// own controller in `linux.rs`, and `run_shell` moved to the shared
// `shell.rs` (docs/post-phase2/linux-support-scope-2026-09-26.md §3.3).
#[cfg(windows)]
use super::{quiet, CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW};
#[cfg(windows)]
use anyhow::Context;
#[cfg(windows)]
use std::collections::HashMap;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
#[cfg(windows)]
use std::sync::{LazyLock, Mutex};
#[cfg(windows)]
use std::time::Duration;
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, GetHandleInformation, GetLastError, SetHandleInformation, ERROR_ALREADY_EXISTS,
    HANDLE, HANDLE_FLAG_INHERIT,
};
#[cfg(windows)]
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, OpenJobObjectW, QueryInformationJobObject,
    SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

// Win32 access rights for job object handles (WinNT.h): `JOB_OBJECT_QUERY` =
// 0x0004, `JOB_OBJECT_TERMINATE` = 0x0008. windows-sys 0.61.2 only exposes
// these from `Win32_System_SystemServices`, a large, otherwise-unrelated
// module; defining them locally keeps the feature list to
// Foundation/Security/JobObjects/Threading. Verified against that crate's
// `Win32/System/SystemServices/mod.rs`.
#[cfg(windows)]
const JOB_OBJECT_QUERY: u32 = 0x0004;
#[cfg(windows)]
const JOB_OBJECT_TERMINATE: u32 = 0x0008;

/// Owns a Job Object HANDLE. `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means the
/// kernel kills every process still in the job the moment its last handle
/// closes, so this handle must outlive anything we want the job to protect.
#[cfg(windows)]
struct JobHandle(HANDLE);

// HANDLE is a raw pointer and so isn't `Send` by default. Moving a Windows
// HANDLE to another thread is safe; the `Mutex` around `JOBS` is what makes
// concurrent access to it safe.
#[cfg(windows)]
unsafe impl Send for JobHandle {}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.0);
        }
    }
}

/// Job Objects created by `spawn_child`, keyed by the child's pid, kept
/// alive here for the life of this process. Without this, the handle
/// `CreateJobObjectW` returns would go out of scope immediately and
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` would tear the job (and its
/// process) down right away.
///
/// This registry is a lifetime-holder and a fast path, not the only way
/// in: `kill_tree`/`is_alive` also reopen jobs by their deterministic name
/// (`thoth-job-<pid>`) for pids this process didn't itself spawn - e.g.
/// a separate `thoth cancel` invocation killing a run a different
/// process started.
#[cfg(windows)]
static JOBS: LazyLock<Mutex<HashMap<u32, JobHandle>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

#[cfg(windows)]
fn job_name(pid: u32) -> String {
    format!("thoth-job-{pid}")
}

#[cfg(windows)]
fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Creates a named, `KILL_ON_JOB_CLOSE` Job Object and assigns the process
/// behind `process_handle` (pid `pid`) to it, so `kill_tree` can take out
/// the whole tree even after `pid` itself has already exited - Windows does
/// not reparent orphaned descendants onto anything under `pid`, so
/// `taskkill /PID <pid> /T /F` alone has no tree left to walk once the root
/// is gone (review finding 1).
///
/// Race: any grandchild the child spawns *before* this call reaches
/// `AssignProcessToJobObject` is not yet a member of the job and would
/// survive a job-based kill. It's a real, small window in production too;
/// `fake-claude`'s `FAKE_SPAWN_CHILD` path sleeps 50ms before spawning its
/// own child so tests aren't flaky against it (see `src/bin/fake-claude.rs`).
///
/// Not fatal if it fails: `spawn_child` logs a warning and returns the
/// child anyway. Losing the job only degrades `kill_tree` for this pid to
/// the taskkill-only, live-subtree-only behavior described on its doc
/// comment; it doesn't affect the process actually running.
///
/// Also refuses to adopt a pre-existing job under this name. Windows
/// recycles pids, so `thoth-job-<pid>` can already exist and still be
/// held by a different, still-alive thoth invocation whose own child
/// once had this pid; `CreateJobObjectW` in that case returns a handle to
/// the *existing* object rather than failing, distinguishable only via
/// `GetLastError() == ERROR_ALREADY_EXISTS` right after the call. Adopting
/// it would let this call's `AssignProcessToJobObject` put an unrelated
/// process into someone else's job (and that job's later `kill_tree` would
/// then kill our process too) - so this closes the handle without
/// assigning and degrades to taskkill-only for this pid instead, same as
/// any other failure here (review finding open-2a).
#[cfg(windows)]
fn create_and_assign_job(pid: u32, process_handle: HANDLE) -> Result<()> {
    let name = wide_null(&job_name(pid));
    let job = unsafe { CreateJobObjectW(std::ptr::null(), name.as_ptr()) };
    let create_error = unsafe { GetLastError() };
    if job.is_null() {
        anyhow::bail!(
            "CreateJobObjectW failed for pid {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
    if create_error == ERROR_ALREADY_EXISTS {
        unsafe {
            CloseHandle(job);
        }
        anyhow::bail!(
            "job object {} already existed (stale name from a recycled pid?); not adopting it",
            job_name(pid)
        );
    }
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let set_ok = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if set_ok == 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        anyhow::bail!("SetInformationJobObject failed for pid {pid}: {err}");
    }
    let assign_ok = unsafe { AssignProcessToJobObject(job, process_handle) };
    if assign_ok == 0 {
        let err = std::io::Error::last_os_error();
        unsafe {
            CloseHandle(job);
        }
        anyhow::bail!("AssignProcessToJobObject failed for pid {pid}: {err}");
    }
    JOBS.lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(pid, JobHandle(job));
    Ok(())
}

/// Terminates the Job Object for `pid`, whether it's one this process
/// created (looked up in `JOBS`) or one a different thoth invocation
/// created (reopened by its deterministic name). This is what lets
/// `kill_tree` reach descendants orphaned by an already-exited root
/// (review finding 1): `TerminateJobObject` kills every process still in
/// the job regardless of the current process tree shape, which
/// `taskkill /T`'s tree walk from `pid` cannot do once `pid` is gone.
/// Does nothing (not an error) if no such job can be found.
///
/// Returns `Some(error text)` if a job *was* found but `TerminateJobObject`
/// itself reported failure; `kill_tree` does not abort on this (it still
/// runs the `taskkill /T` fallback below), but surfaces the text if the
/// final tasklist confirmation also fails (review finding minor-3).
/// Returns `None` on success, or if no job could be found at all.
#[cfg(windows)]
fn terminate_job_for_pid(pid: u32) -> Option<String> {
    {
        let jobs = JOBS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(job) = jobs.get(&pid) {
            let ok = unsafe { TerminateJobObject(job.0, 1) };
            return if ok == 0 {
                Some(format!(
                    "TerminateJobObject({}) failed: {}",
                    job_name(pid),
                    std::io::Error::last_os_error()
                ))
            } else {
                None
            };
        }
    }
    let name = wide_null(&job_name(pid));
    let handle =
        unsafe { OpenJobObjectW(JOB_OBJECT_TERMINATE | JOB_OBJECT_QUERY, 0, name.as_ptr()) };
    if handle.is_null() {
        return None;
    }
    let ok = unsafe { TerminateJobObject(handle, 1) };
    let err = if ok == 0 {
        Some(format!(
            "TerminateJobObject({}) failed: {}",
            job_name(pid),
            std::io::Error::last_os_error()
        ))
    } else {
        None
    };
    unsafe {
        CloseHandle(handle);
    }
    err
}

/// Whether the named job for `pid` still has any active process in it.
/// `is_alive` uses this to report a run's tree as alive even once the root
/// pid itself is gone: `tasklist` on a single pid can't see orphaned
/// descendants, but the job can (review finding 1's liveness half).
#[cfg(windows)]
fn job_has_active_process(pid: u32) -> bool {
    let name = wide_null(&job_name(pid));
    let handle = unsafe { OpenJobObjectW(JOB_OBJECT_QUERY, 0, name.as_ptr()) };
    if handle.is_null() {
        return false;
    }
    let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
    let mut returned: u32 = 0;
    let ok = unsafe {
        QueryInformationJobObject(
            handle,
            JobObjectBasicAccountingInformation,
            &mut info as *mut _ as *mut core::ffi::c_void,
            std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
            &mut returned,
        )
    };
    unsafe {
        CloseHandle(handle);
    }
    ok != 0 && info.ActiveProcesses > 0
}

/// Pure decision logic behind `is_alive`'s tasklist check, split out so its
/// three branches - present, absent-and-succeeded, absent-and-failed - are
/// unit-testable without spawning a real process. `tasklist` exits 0
/// whether or not the pid exists, so "absent" only means "not running" when
/// the command itself actually succeeded; a failing invocation (bad filter
/// syntax, `tasklist.exe` missing, ...) must not be silently read as "the
/// process is dead" (review finding 2).
#[cfg(windows)]
fn liveness_from_tasklist(success: bool, stdout: &str, pid: u32) -> Result<bool> {
    if stdout.contains(&format!("\"{pid}\"")) {
        Ok(true)
    } else if success {
        Ok(false)
    } else {
        anyhow::bail!("tasklist failed for pid {pid}")
    }
}

/// Pure decision logic behind `kill_tree`'s final verdict, split out (like
/// `liveness_from_tasklist`) so it is unit-testable without spawning a
/// process. `still_alive` is the result of the confirmation loop; `taskkill`'s
/// exit code alone never decides success, since it can report 255 for a tree
/// that is already clean.
#[cfg(windows)]
fn kill_tree_outcome(
    taskkill_exit: Option<i32>,
    taskkill_stderr: &str,
    still_alive: bool,
    pid: u32,
    job_err: Option<String>,
) -> Result<()> {
    if !still_alive {
        return Ok(());
    }
    let mut msg = format!("pid {pid} was still listed 2s after kill_tree({pid})");
    if !matches!(taskkill_exit, Some(0)) {
        msg.push_str(&format!(
            "; taskkill /PID {pid} /T /F exited {taskkill_exit:?}: {taskkill_stderr}"
        ));
    }
    if let Some(e) = job_err {
        msg.push_str(&format!("; job termination had also failed: {e}"));
    }
    anyhow::bail!(msg)
}

/// Parses the image name (first CSV column) `tasklist` reports for `pid`.
/// `cancel`'s escalation checks this before falling back to a bare pid kill,
/// since Windows recycles pids and a stale one could belong to an unrelated
/// process by the time `cancel` runs (review finding 4).
#[cfg(windows)]
pub fn image_name(pid: u32) -> Result<Option<String>> {
    use std::process::Command;
    let out = quiet(Command::new("tasklist").args([
        "/FI",
        &format!("PID eq {pid}"),
        "/FO",
        "CSV",
        "/NH",
    ]))
    .output()?;
    if !out.status.success() {
        anyhow::bail!(
            "tasklist /FI \"PID eq {pid}\" /FO CSV /NH failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let quoted_pid = format!("\"{pid}\"");
    for line in stdout.lines() {
        if line.contains(&quoted_pid) {
            if let Some(name) = line.trim_start_matches('"').split("\",\"").next() {
                return Ok(Some(name.to_string()));
            }
        }
    }
    Ok(None)
}

/// Drops this process's tracked Job Object handle for `pid`, if any.
///
/// `is_alive` already does this lazily once a run's job has no active
/// process left in it, so calling this is not required for correctness -
/// but Task 8's runner calls it right after `child.wait()` returns, as
/// belt-and-braces cleanup that doesn't wait for a subsequent `is_alive`
/// poll to notice the run is over.
///
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` means dropping the last handle to
/// the job terminates any process still assigned to it - that is
/// intentional here, not a bug to guard against: it's what cleans up any
/// descendant the harness left running (a stray tool invocation, a leaked
/// subprocess) once the run itself is done. Callers are responsible for
/// only calling this once they actually are done with `pid` - calling it
/// while the run is still in progress would kill it early.
#[cfg(windows)]
pub fn release_job(pid: u32) {
    JOBS.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
}

/// This process's own stdin/stdout/stderr, as raw HANDLEs. Reading them does not
/// take ownership or close anything; it just looks up what std already has open.
#[cfg(windows)]
fn std_io_handles() -> [HANDLE; 3] {
    use std::os::windows::io::AsRawHandle;
    [
        std::io::stdin().as_raw_handle() as HANDLE,
        std::io::stdout().as_raw_handle() as HANDLE,
        std::io::stderr().as_raw_handle() as HANDLE,
    ]
}

/// Clears `HANDLE_FLAG_INHERIT` on this process's own stdin/stdout/stderr and
/// returns the flags each one had before, in the same order, so the caller can
/// restore them with `restore_std_handle_inheritance`. An entry is `None` when
/// its flags could not even be read (a pseudo-handle, or already closed); that
/// handle is left untouched in both directions.
///
/// Why this exists: `CreateProcess` with `bInheritHandles = TRUE` - which
/// `std::process::Command` on Windows must pass whenever it wires up any of a
/// child's stdin/stdout/stderr, even to NUL - inherits *every* inheritable
/// handle in the calling process, not just the ones named in `STARTUPINFO`. If
/// this process was itself launched with its own stdout/stderr captured through
/// an inheritable pipe (a test harness's `Command::output()`, a shell's
/// `$(...)`, ...), a detached grandchild spawned here would silently inherit
/// that pipe's write end and hold it open for its entire lifetime - so whoever
/// is reading the read end would not see EOF, and would not see this process's
/// output, until the detached process (and anything it in turn spawns) exits
/// too. That defeats the entire point of "detached" and was caught by
/// `dispatch_moves_the_task_out_of_a_wait_terminal_status_before_it_returns`,
/// whose `slow` scenario runs long enough to make a leaked handle's effect
/// (blocking for the scenario's whole duration instead of returning
/// immediately) unmistakable rather than a rare, load-dependent flake.
#[cfg(windows)]
pub(crate) fn clear_std_handle_inheritance() -> [Option<u32>; 3] {
    let mut saved: [Option<u32>; 3] = [None; 3];
    for (i, h) in std_io_handles().into_iter().enumerate() {
        if h.is_null() {
            continue;
        }
        let mut flags: u32 = 0;
        if unsafe { GetHandleInformation(h, &mut flags) } != 0 {
            saved[i] = Some(flags);
            // Best-effort: if this fails, that one handle stays inheritable and
            // may still leak, but every other handle and the spawn itself must
            // still proceed.
            unsafe {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, 0);
            }
        }
    }
    saved
}

/// Restores whatever `clear_std_handle_inheritance` observed, handle-for-handle
/// and in the same order.
#[cfg(windows)]
pub(crate) fn restore_std_handle_inheritance(saved: [Option<u32>; 3]) {
    for (h, flags) in std_io_handles().into_iter().zip(saved) {
        if h.is_null() {
            continue;
        }
        if let Some(flags) = flags {
            unsafe {
                SetHandleInformation(h, HANDLE_FLAG_INHERIT, flags & HANDLE_FLAG_INHERIT);
            }
        }
    }
}

pub struct WindowsProcessController;

#[cfg(windows)]
impl ProcessController for WindowsProcessController {
    /// Starts a process meant to outlive this one (e.g. the runner the
    /// dispatcher launches and then exits without waiting for) and returns
    /// once it's launched.
    ///
    /// Deliberately *not* put in a Job Object, unlike `spawn_child`. This
    /// asymmetry is intentional, not an oversight: a `KILL_ON_JOB_CLOSE`
    /// job dies (taking every process still in it down too) once its last
    /// handle closes, and the dispatcher's own process - along with
    /// whatever job handles it holds - goes away when the dispatcher
    /// exits. Putting the detached process in a job here would mean it
    /// gets killed by the very act of detaching from it.
    ///
    /// Uses `CREATE_NO_WINDOW`, not `DETACHED_PROCESS`: both make the new
    /// process outlive this call with no console *of its own* attached to
    /// this one, but `DETACHED_PROCESS` gives it no console at all, while
    /// `CREATE_NO_WINDOW` gives it a hidden one. That distinction matters
    /// because this runner process goes on to spawn console-subsystem
    /// children itself (`git`, `tasklist`, `taskkill`, ...) without its own
    /// creation flags - Windows's default rule for a console-subsystem
    /// child is "inherit the parent's console, or create a new *visible*
    /// one if the parent doesn't have one". Under `DETACHED_PROCESS` (no
    /// console to inherit) every one of those children popped a new,
    /// visible console window (`OpenConsole.exe -Embedding` under Windows
    /// Terminal, the default terminal on this machine), which then errored
    /// with `0x800700e8` once the short-lived child exited. Under
    /// `CREATE_NO_WINDOW` there is a console to inherit - it's just hidden -
    /// so the whole tree stays windowless.
    fn spawn_detached(&self, program: &str, args: &[String], cwd: &Path) -> Result<u32> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        // See `clear_std_handle_inheritance`'s doc comment: without this, a
        // caller whose own stdout/stderr are themselves inheritable pipe
        // handles (an assert_cmd test, a shell capturing this process's
        // output, ...) would leak them into the detached process, and whoever
        // is reading this process's output would then block until the
        // detached process exits too - not just until this process does.
        // M4 (fix round 1): this mutates process-global state (this process's
        // own std handle inherit flags) for the duration of one `spawn()`
        // call, with no lock around it. That is safe only because `thoth`
        // is single-threaded at the point this runs (the CLI's `main` calls
        // exactly one command, synchronously) and because the runner (the
        // only other thing that spawns child processes) calls
        // `spawn_detached` only while it settles a run it hands over
        // (S3-10): by then its worker has exited and no other thread of the
        // runner starts a child process. `dispatch` calls it once, near the
        // end of its own `run()`. If a future caller ever spawns two detached
        // processes concurrently from different threads of the same process,
        // this needs a mutex around the clear/spawn/restore sequence.
        let saved = clear_std_handle_inheritance();
        let result = Command::new(program)
            .args(args)
            .current_dir(cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
            .spawn();
        restore_std_handle_inheritance(saved);
        Ok(result?.id())
    }

    /// Starts a child whose life this process supervises for its own
    /// duration - the harness worker, piped so its output can be streamed -
    /// unlike `spawn_detached`, whose whole point is to outlive the caller.
    /// Putting it in a Job Object here is safe for exactly that reason: the
    /// caller sticking around (or not) is irrelevant to what `kill_tree`/
    /// `is_alive` need the job for, which is surviving `pid` itself having
    /// already exited (review finding 1), not surviving the caller's exit.
    fn spawn_child(&self, spec: ChildSpec<'_>) -> Result<Child> {
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        let mut cmd = Command::new(spec.program);
        cmd.args(spec.args)
            .current_dir(spec.cwd)
            .stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP);
        for (k, v) in spec.env {
            cmd.env(k, v);
        }
        // std compares Windows variable names without regard to case.
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
        let child = cmd.spawn()?;
        let pid = child.id();
        let process_handle = child.as_raw_handle() as HANDLE;
        if let Err(e) = create_and_assign_job(pid, process_handle) {
            // Degraded, not fatal: see `create_and_assign_job`'s doc comment.
            masked_eprintln!(
                "thoth: warning: no job object for pid {pid}, kill_tree degraded to \
                 taskkill-only for it: {e:#}"
            );
        }
        Ok(child)
    }

    /// Kills the process tree rooted at `pid` and confirms it's gone.
    ///
    /// Full-tree kill is guaranteed for processes started via
    /// `spawn_child`: they're in a `KILL_ON_JOB_CLOSE` Job Object named
    /// `thoth-job-<pid>`, so `TerminateJobObject` reaches every
    /// descendant even if `pid` itself has already exited by the time this
    /// runs - Windows does not reparent orphans onto anything under `pid`
    /// for `taskkill /T` to walk (review finding 1). For a bare pid with no
    /// such job (nothing thoth spawned it through, or job creation
    /// failed), only the live subtree `tasklist`/`taskkill` can still see
    /// under `pid` is killed.
    ///
    /// Succeeds if the pid's tree is gone, whatever `taskkill` exited with:
    /// under load `taskkill` can exit 255 because part of the tree (a console
    /// host, say) ended on its own between its enumeration and its kill, and
    /// the tree is nonetheless clean. Only the `is_alive` confirmation decides
    /// success; errors if the tree is still listed 2s after the kill. The
    /// message carries `taskkill`'s exit code and stderr only when it did not
    /// exit 0, plus any job termination failure. A `taskkill` that cannot be
    /// started at all is an immediate error.
    ///
    /// For a pid with no job, the final confirmation only sees the root
    /// process and its job: a descendant that has already detached from the
    /// root is invisible to it.
    fn kill_tree(&self, pid: u32) -> Result<()> {
        // (a)/(b): terminate the job for `pid` first, if one can be found,
        // so descendants of an already-exited root are reachable at all.
        // A failure here doesn't abort the kill - the taskkill fallback
        // below still runs - but is remembered in case the final
        // confirmation fails too (review finding minor-3).
        let job_err = terminate_job_for_pid(pid);

        // (c) belt-and-braces: also kill whatever live subtree still hangs
        // off `pid` directly. This is the only mechanism for a pid with no
        // job, and catches anything the spawn-time job-assignment race
        // (see `create_and_assign_job`) left out of the job.
        use std::process::Command;
        let out = quiet(Command::new("taskkill").args(["/PID", &pid.to_string(), "/T", "/F"]))
            .output()?;
        let exit_code = out.status.code();
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();

        // (e) confirm via `is_alive`, which also checks the job's active
        // process count (and lazily releases our registry entry once the
        // job has nothing left in it - review finding open-2b), so this
        // loop covers job-based kills too, not just the taskkill-visible
        // subtree. It runs whatever taskkill's exit code was.
        let mut still_alive = true;
        for _ in 0..20 {
            if !self.is_alive(pid)? {
                still_alive = false;
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        kill_tree_outcome(exit_code, &stderr, still_alive, pid, job_err)
    }

    /// Whether `pid`'s tree is alive: true if `pid` itself is listed by
    /// `tasklist`, or if its Job Object still has an active process even
    /// though `pid` has already exited - an orphaned descendant a bare
    /// `tasklist /FI "PID eq <pid>"` can't see (review finding 1's liveness
    /// half). `kill_tree` uses this to verify itself; `cancel`'s escalation
    /// path relies on that verification.
    ///
    /// Also does lazy cleanup of `JOBS`: once `pid` is gone from `tasklist`
    /// and its job (if any) has no active process left, this drops our
    /// registry entry for it. Without this, a finished run's entry would
    /// sit in `JOBS` for the rest of this process's life, and if Windows
    /// later recycles `pid` for an unrelated process, `create_and_assign_job`
    /// would find a name collision it has to refuse rather than the pid
    /// simply being free again (review finding open-2b).
    fn is_alive(&self, pid: u32) -> Result<bool> {
        use std::process::Command;
        let out = quiet(Command::new("tasklist").args([
            "/FI",
            &format!("PID eq {pid}"),
            "/FO",
            "CSV",
            "/NH",
        ]))
        .output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let root_alive = liveness_from_tasklist(out.status.success(), &stdout, pid)
            .with_context(|| {
                format!(
                    "tasklist /FI \"PID eq {pid}\" /FO CSV /NH stderr: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                )
            })?;
        if root_alive {
            return Ok(true);
        }
        let job_alive = job_has_active_process(pid);
        if !job_alive {
            JOBS.lock().unwrap_or_else(|e| e.into_inner()).remove(&pid);
        }
        Ok(job_alive)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::process::shell::{run_shell, run_shell_capped, Shell};
    use std::process::Command;
    use std::time::Instant;

    /// Creation time (FILETIME ticks) of the process behind an open
    /// `handle`, `None` if `GetProcessTimes` fails.
    fn creation_ticks(handle: HANDLE) -> Option<u64> {
        use windows_sys::Win32::Foundation::FILETIME;
        use windows_sys::Win32::System::Threading::GetProcessTimes;
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
        let ok =
            unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) };
        (ok != 0)
            .then(|| (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime))
    }

    /// Creation time (FILETIME ticks) of the process currently holding `pid`,
    /// `None` if there is none. Together with the pid it identifies one
    /// process, since Windows recycles pids quickly.
    fn creation_time(pid: u32) -> Option<u64> {
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if handle.is_null() {
            return None;
        }
        let ticks = creation_ticks(handle);
        unsafe {
            CloseHandle(handle);
        }
        ticks
    }

    /// Whether the process that was created at `created` (FILETIME ticks)
    /// still holds `pid` and has not exited. This is how every test here
    /// asks "is the process *I* started still there": a bare pid lookup
    /// (`tasklist /FI "PID eq"`) says yes for whatever process Windows has
    /// since handed the pid to, and another copy of this suite running at
    /// once makes that a real possibility.
    ///
    /// Both halves are needed: a process that has exited still answers
    /// `GetProcessTimes` with its creation time for as long as any handle to
    /// it is open (the `Child` a test still holds, for instance), so the
    /// creation time alone does not say it is running; and the pid alone
    /// does not say it is the same process.
    fn alive_as(pid: u32, created: u64) -> bool {
        use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
        use windows_sys::Win32::System::Threading::{
            OpenProcess, WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION,
            PROCESS_SYNCHRONIZE,
        };
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                0,
                pid,
            )
        };
        if handle.is_null() {
            return false;
        }
        let same_process = creation_ticks(handle) == Some(created);
        let still_running = unsafe { WaitForSingleObject(handle, 0) } == WAIT_TIMEOUT;
        unsafe {
            CloseHandle(handle);
        }
        same_process && still_running
    }

    /// Drop guard so a panic between spawning a tree and killing it can't
    /// leak a real process (review finding 6). It remembers which process
    /// held `pid` when it was armed and only kills that one: by the time it
    /// drops, the tree has usually been killed already and Windows may have
    /// handed the pid to a process another test (or another copy of the
    /// suite) has just started, which a bare `taskkill /PID` would kill.
    struct Guard {
        pid: u32,
        created: Option<u64>,
    }
    impl Guard {
        fn new(pid: u32) -> Guard {
            Guard {
                pid,
                created: creation_time(pid),
            }
        }
        /// A guard for the process already identified as (`pid`, `created`),
        /// such as a grandchild or tagged ping found by `wait_for_grandchild`
        /// or `wait_for_tagged_ping_pid`, so it is not looked up a second time.
        fn armed(pid: u32, created: u64) -> Guard {
            Guard {
                pid,
                created: Some(created),
            }
        }
        /// A guard armed for an earlier incarnation of `pid`.
        fn stale(pid: u32) -> Guard {
            Guard {
                pid,
                created: Some(1),
            }
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            if self.created.is_none() || creation_time(self.pid) != self.created {
                return;
            }
            let _ = Command::new("taskkill")
                .args(["/PID", &self.pid.to_string(), "/T", "/F"])
                .output();
        }
    }

    #[test]
    fn a_guard_armed_for_an_earlier_incarnation_of_a_pid_does_not_kill_the_new_holder() {
        let mut victim = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn victim");
        let pid = victim.id();
        drop(Guard::stale(pid));
        std::thread::sleep(Duration::from_millis(500));
        let survived = victim.try_wait().expect("try_wait").is_none();
        let _ = victim.kill();
        let _ = victim.wait();
        assert!(
            survived,
            "a stale guard killed the current holder of pid {pid}"
        );
    }

    #[test]
    fn alive_as_tells_a_finished_or_different_process_apart() {
        let mut child = Command::new("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping");
        let pid = child.id();
        let _guard = Guard::new(pid);
        let created = creation_time(pid).expect("creation time of the ping just spawned");

        assert!(alive_as(pid, created), "the running ping is alive as itself");
        assert!(
            !alive_as(pid, created + 1),
            "a different creation time is a different process, even with the same pid"
        );

        child.kill().expect("kill ping");
        child.wait().expect("wait ping");
        // `child` is still held here, so its handle is open and the pid
        // cannot have been recycled: the only reason for `false` is that
        // the process has exited.
        assert!(
            !alive_as(pid, created),
            "a finished process must not count as alive just because a handle to it is still open"
        );
        drop(child);
    }

    fn wait_until(deadline_secs: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(deadline_secs);
        loop {
            if cond() {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    // `CARGO_BIN_EXE_*` is only available to integration tests (`tests/*.rs`),
    // not unit tests compiled into the crate itself, so these spawn
    // `cmd.exe /c ping ... & powershell ... Start-Sleep` directly rather
    // than `fake-claude`.
    //
    // The `ping -n 2 127.0.0.1 >nul &` prefix delays the grandchild's own
    // spawn by roughly a second. `spawn_child`'s `AssignProcessToJobObject`
    // call happens in this process immediately after `Command::spawn()`
    // returns - effectively instantly - so by the time `ping` finishes and
    // `powershell` starts, the job assignment has provably already
    // happened. Without this delay these tests could pass by winning a race
    // rather than by the job actually working (review finding minor-6).
    fn spawn_sleep_tree(ctl: &WindowsProcessController, ws: &Path) -> Child {
        let args = vec![
            "/c".to_string(),
            "ping".to_string(),
            "-n".to_string(),
            "2".to_string(),
            "127.0.0.1".to_string(),
            ">nul".to_string(),
            "&".to_string(),
            "powershell".to_string(),
            "-NoProfile".to_string(),
            "-Command".to_string(),
            // Long enough that, with up to 20 s spent finding it under load
            // (`wait_for_grandchild`), at least 40 s remain for the
            // assertions that follow.
            "Start-Sleep -Seconds 60".to_string(),
        ];
        let spec = ChildSpec {
            program: "cmd.exe",
            args: &args,
            cwd: ws,
            env: &[],
            env_remove: &[],
            stderr_path: None,
            stdin: None,
        };
        ctl.spawn_child(spec).expect("spawn_child")
    }

    /// Which of the `powershell.exe` processes whose parent pid is `root`
    /// is really `root`'s grandchild (pure; `wait_for_grandchild`). Windows
    /// never clears an orphan's `ParentProcessId`, so if `root`'s pid was
    /// earlier held by another, already-exited process - another copy of
    /// this suite deliberately leaves such orphans behind for a while - its
    /// orphaned powershell still names `root`'s pid as its parent. That one
    /// was created *before* `root` (`root_created`, FILETIME ticks) and is
    /// skipped; a candidate with no creation time has already exited and is
    /// skipped too. Of several acceptable ones the earliest created wins.
    fn pick_grandchild(candidates: &[(u32, Option<u64>)], root_created: u64) -> Option<u32> {
        candidates
            .iter()
            .filter_map(|(pid, created)| created.map(|c| (c, *pid)))
            .filter(|(created, _)| *created >= root_created)
            .min()
            .map(|(_, pid)| pid)
    }

    #[test]
    fn pick_grandchild_skips_a_process_older_than_its_parent() {
        let root_created = 1_000;
        assert_eq!(
            pick_grandchild(&[(10, Some(999))], root_created),
            None,
            "created before the root: an orphan of an earlier holder of the root's pid"
        );
        assert_eq!(pick_grandchild(&[(10, Some(1_000))], root_created), Some(10));
        assert_eq!(pick_grandchild(&[(10, Some(1_001))], root_created), Some(10));
        assert_eq!(
            pick_grandchild(&[(10, None)], root_created),
            None,
            "no creation time: already exited"
        );
        assert_eq!(
            pick_grandchild(&[(10, Some(1_005)), (11, Some(1_002))], root_created),
            Some(11),
            "two acceptable candidates: the earlier one"
        );
        assert_eq!(
            pick_grandchild(&[(10, Some(999)), (11, None)], root_created),
            None,
            "nothing acceptable"
        );
        assert_eq!(pick_grandchild(&[], root_created), None);
    }

    /// Polls for the `powershell.exe` grandchild specifically (not the
    /// transient `ping.exe` that runs first under `spawn_sleep_tree`'s
    /// delay), so the pid returned is the long-lived `Start-Sleep` process
    /// the tests actually want to exercise the kill against. Returns the
    /// pid with the creation time that identifies it (`alive_as`,
    /// `Guard::armed`). Only a powershell created no earlier than `root`
    /// counts (`pick_grandchild`).
    fn wait_for_grandchild(root: u32) -> (u32, u64) {
        let root_created =
            creation_time(root).unwrap_or_else(|| panic!("no creation time for root pid {root}"));
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let out = Command::new("powershell")
                .args([
                    "-NoProfile",
                    "-Command",
                    &format!(
                        "(Get-CimInstance Win32_Process -Filter \"ParentProcessId={root} and Name='powershell.exe'\").ProcessId"
                    ),
                ])
                .output()
                .expect("powershell query for grandchild pid");
            let text = String::from_utf8_lossy(&out.stdout);
            let candidates: Vec<(u32, Option<u64>)> = text
                .lines()
                .filter_map(|l| l.trim().parse::<u32>().ok())
                .map(|pid| (pid, creation_time(pid)))
                .collect();
            if let Some(pid) = pick_grandchild(&candidates, root_created) {
                let created = candidates
                    .iter()
                    .find(|(p, _)| *p == pid)
                    .and_then(|(_, c)| *c)
                    .expect("pick_grandchild only returns a candidate with a creation time");
                return (pid, created);
            }
            if Instant::now() > deadline {
                panic!("grandchild of pid {root} never appeared (candidates seen last: {candidates:?})");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn spawn_child_then_kill_tree_clears_root_and_grandchild() {
        let ctl = WindowsProcessController;
        let ws = tempfile::tempdir().unwrap();
        let mut child = spawn_sleep_tree(&ctl, ws.path());
        let root = child.id();
        let _root_guard = Guard::new(root);
        let (grandchild, grandchild_created) = wait_for_grandchild(root);
        let _grandchild_guard = Guard::armed(grandchild, grandchild_created);

        assert!(ctl.is_alive(root).unwrap(), "root should be alive");
        assert!(
            alive_as(grandchild, grandchild_created),
            "grandchild should be alive"
        );

        ctl.kill_tree(root).unwrap();
        let _ = child.wait();

        assert!(
            !ctl.is_alive(root).unwrap(),
            "root should be dead after kill_tree"
        );
        assert!(
            wait_until(10, || !alive_as(grandchild, grandchild_created)),
            "grandchild survived kill_tree"
        );
    }

    #[test]
    fn kill_tree_clears_orphans_when_root_already_exited() {
        // Direct test of review finding 1: kill the root alone (no `/T`) so
        // the grandchild is orphaned, then confirm `kill_tree` still reaches
        // it via the job even though `taskkill /PID <root> /T /F` has no
        // tree left under `root` to walk.
        let ctl = WindowsProcessController;
        let ws = tempfile::tempdir().unwrap();
        let mut child = spawn_sleep_tree(&ctl, ws.path());
        let root = child.id();
        let root_created = creation_time(root).expect("creation time of the root just spawned");
        let _root_guard = Guard::new(root);
        let (grandchild, grandchild_created) = wait_for_grandchild(root);
        let _grandchild_guard = Guard::armed(grandchild, grandchild_created);

        let out = Command::new("taskkill")
            .args(["/PID", &root.to_string(), "/F"])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            wait_until(10, || !alive_as(root, root_created)),
            "root did not die"
        );
        let _ = child.wait();
        assert!(
            alive_as(grandchild, grandchild_created),
            "grandchild should survive a root-only kill"
        );

        ctl.kill_tree(root).unwrap();

        assert!(
            wait_until(10, || !alive_as(grandchild, grandchild_created)),
            "kill_tree must clear the orphaned grandchild via the job"
        );
    }

    #[test]
    fn is_alive_reports_tree_alive_while_orphan_runs() {
        let ctl = WindowsProcessController;
        let ws = tempfile::tempdir().unwrap();
        let mut child = spawn_sleep_tree(&ctl, ws.path());
        let root = child.id();
        let root_created = creation_time(root).expect("creation time of the root just spawned");
        let _root_guard = Guard::new(root);
        let (grandchild, grandchild_created) = wait_for_grandchild(root);
        let _grandchild_guard = Guard::armed(grandchild, grandchild_created);

        let out = Command::new("taskkill")
            .args(["/PID", &root.to_string(), "/F"])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            wait_until(10, || !alive_as(root, root_created)),
            "root did not die"
        );
        let _ = child.wait();

        assert!(
            ctl.is_alive(root).unwrap(),
            "is_alive must see the job's live orphan even though the root pid is gone"
        );

        ctl.kill_tree(root).unwrap();
        assert!(!ctl.is_alive(root).unwrap());
    }

    #[test]
    fn liveness_from_tasklist_reports_present_absent_and_failure() {
        let present = "\"powershell.exe\",\"1234\",\"Console\",\"1\",\"10,000 K\"\r\n";
        assert!(liveness_from_tasklist(true, present, 1234).unwrap());

        let absent_ok = "INFO: No tasks are running which match the specified criteria.\r\n";
        assert!(!liveness_from_tasklist(true, absent_ok, 1234).unwrap());

        let absent_failed = "ERROR: The search filter cannot be recognized.\r\n";
        assert!(liveness_from_tasklist(false, absent_failed, 1234).is_err());
    }

    #[test]
    fn kill_tree_exit_0_and_tree_gone_is_ok() {
        assert!(kill_tree_outcome(Some(0), "", false, 7, None).is_ok());
    }

    #[test]
    fn kill_tree_exit_255_and_tree_gone_is_ok() {
        assert!(kill_tree_outcome(Some(255), "ERROR: gone", false, 7, None).is_ok());
    }

    #[test]
    fn kill_tree_exit_255_and_tree_still_alive_reports_code_and_stderr() {
        let err = kill_tree_outcome(Some(255), "ERROR: access denied", true, 7, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("255"), "{err}");
        assert!(err.contains("ERROR: access denied"), "{err}");
    }

    #[test]
    fn kill_tree_exit_0_and_tree_still_alive_is_the_plain_error() {
        let err = kill_tree_outcome(Some(0), "", true, 7, None)
            .unwrap_err()
            .to_string();
        assert_eq!(err, "pid 7 was still listed 2s after kill_tree(7)");
    }

    #[test]
    fn kill_tree_still_alive_error_also_carries_the_job_failure() {
        let err = kill_tree_outcome(Some(0), "", true, 7, Some("job boom".to_string()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("job boom"), "{err}");
    }

    /// A short-lived but not instantaneous real process (~1s), so there's
    /// ample time for the synchronous `CreateJobObjectW`/
    /// `SetInformationJobObject`/`AssignProcessToJobObject` sequence in
    /// `create_and_assign_job` to run against its still-live handle before
    /// it exits on its own.
    fn spawn_disposable_process() -> Child {
        Command::new("cmd.exe")
            .args(["/c", "ping", "-n", "2", "127.0.0.1", ">nul"])
            .spawn()
            .expect("spawn disposable process")
    }

    #[test]
    fn create_and_assign_job_does_not_adopt_a_stale_job_with_the_same_name() {
        // Simulates Windows recycling a pid: a job named
        // `thoth-job-<pid>` already exists (because a still-alive
        // thoth invocation's earlier child once had this same pid
        // number) by the time a *new* process reuses it.
        // `create_and_assign_job` must refuse to adopt that job rather than
        // silently assigning an unrelated process into it (review finding
        // open-2a). `fake_pid` is only a HashMap key / job-name suffix here,
        // not the real pid of anything this test spawns.
        // The job name is a machine-wide kernel object name, so a constant
        // here collides with the same test in another test process running
        // at once. Derived from this process's own pid: unique among live
        // processes, and above any real pid so it never meets a real job.
        let fake_pid = 0x7000_0000u32 + std::process::id();

        let mut first = spawn_disposable_process();
        let _first_guard = Guard::new(first.id());
        let first_handle = first.as_raw_handle() as HANDLE;
        create_and_assign_job(fake_pid, first_handle)
            .expect("first create_and_assign_job under a fresh name should succeed");

        let mut second = spawn_disposable_process();
        let _second_guard = Guard::new(second.id());
        let second_handle = second.as_raw_handle() as HANDLE;
        let result = create_and_assign_job(fake_pid, second_handle);
        // Rider (whole-branch review, ledger-parked T6): assert the specific
        // wording from the `ERROR_ALREADY_EXISTS` branch, not just `is_err()`
        // - otherwise this test would also pass if `AssignProcessToJobObject`
        // failed for some unrelated reason.
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("already existed"),
            "must not silently adopt a pre-existing job under the same name: {err}"
        );

        release_job(fake_pid);
        let _ = first.wait();
        let _ = second.wait();
    }

    #[test]
    fn spawn_detached_returns_quickly_while_child_keeps_running() {
        // Direct test of the `spawn_detached`/`spawn_child` asymmetry
        // (review finding minor-5): a detached process has no job, starts
        // fast, and is still running right after `spawn_detached` returns.
        let ctl = WindowsProcessController;
        let ws = tempfile::tempdir().unwrap();
        let args = vec![
            "/c".to_string(),
            "ping".to_string(),
            "-n".to_string(),
            // Long enough that a machine busy with other test runs cannot
            // let the child exit on its own before `kill_tree` below; the
            // guard and `kill_tree` end it.
            "30".to_string(),
            "127.0.0.1".to_string(),
            ">nul".to_string(),
        ];
        let start = Instant::now();
        let pid = ctl.spawn_detached("cmd.exe", &args, ws.path()).unwrap();
        let elapsed = start.elapsed();
        let _guard = Guard::new(pid);

        // The bound is relative to the child's own ~29 s lifetime, not to
        // how fast an idle machine spawns: under the load of a second copy
        // of the suite a spawn takes a few seconds, and anything well under
        // half the lifetime still proves `spawn_detached` did not wait for
        // the child.
        assert!(
            elapsed < Duration::from_secs(10),
            "spawn_detached took {elapsed:?} to return; the bound is 10s, far below the ~29s the \
             detached ping lives, so anything past it means spawn_detached waited for the child"
        );
        assert!(
            ctl.is_alive(pid).unwrap(),
            "detached child should still be running right after spawn_detached returns"
        );

        ctl.kill_tree(pid).unwrap();
    }

    /// Direct test of `terminate_job_for_pid`'s cross-process fallback (Task
    /// 11 fix round 1, corrected in fix round 2 - I2): a job this process's
    /// own `JOBS` registry no longer tracks, but which still exists in the
    /// kernel for some other reason - in production, that "other reason" is
    /// `cancel` running in a fresh process against a run whose runner (the
    /// process that actually called `spawn_child`) has since exited; here,
    /// it's this test holding its own extra handle, opened by the job's
    /// deterministic name, standing in for that.
    ///
    /// Fix round 2 (I2): the first cut of this test called `kill_tree(root)`
    /// while `root` was still alive, so `kill_tree`'s own `taskkill /PID
    /// root /T /F` fallback - a plain process-tree walk from a *live* root -
    /// was already sufficient to explain the grandchild's death; the test
    /// could pass even if `terminate_job_for_pid`'s `OpenJobObjectW`+
    /// `TerminateJobObject` path were deleted entirely. This version kills
    /// `root` alone first (no `/T`), confirms the grandchild is *still*
    /// alive with `root` gone - at that point nothing but the job (kept open
    /// by `extra_handle`) still connects the two, so no tree walk from `root`
    /// can reach the grandchild any more - and only then calls `kill_tree`,
    /// so the grandchild's death afterwards can only be attributed to the
    /// `OpenJobObjectW`-by-name path.
    #[test]
    fn kill_tree_falls_back_to_open_job_object_by_name_when_the_registry_entry_is_gone() {
        let ctl = WindowsProcessController;
        let ws = tempfile::tempdir().unwrap();
        let mut child = spawn_sleep_tree(&ctl, ws.path());
        let root = child.id();
        let root_created = creation_time(root).expect("creation time of the root just spawned");
        let _root_guard = Guard::new(root);
        let (grandchild, grandchild_created) = wait_for_grandchild(root);
        let _grandchild_guard = Guard::armed(grandchild, grandchild_created);

        // Open our own handle to the job by name before dropping the
        // registry's handle below - this is what keeps the job (and the
        // grandchild) alive once (a) this process no longer tracks it
        // itself and (b) `root` itself is about to be killed directly.
        let name = wide_null(&job_name(root));
        let extra_handle =
            unsafe { OpenJobObjectW(JOB_OBJECT_TERMINATE | JOB_OBJECT_QUERY, 0, name.as_ptr()) };
        assert!(
            !extra_handle.is_null(),
            "must be able to open the job just created for `root`"
        );

        // Drop this process's own tracked handle/registry entry for `root`.
        // Without `extra_handle` above, `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
        // would tear the job - and both `root` and `grandchild` with it -
        // down right here. With it, the job (and the grandchild) survive,
        // but `JOBS` no longer has an entry for `root`, so `kill_tree` below
        // cannot use the fast in-registry path and must go through
        // `terminate_job_for_pid`'s `OpenJobObjectW`-by-name fallback
        // instead.
        release_job(root);

        // Kill ONLY `root` (no `/T`), orphaning the grandchild: a `taskkill
        // /PID root /T /F` walk from `root` has nothing left to find it
        // through from here on - only the job, kept alive by
        // `extra_handle`, still connects the two.
        let out = Command::new("taskkill")
            .args(["/PID", &root.to_string(), "/F"])
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(0),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            wait_until(10, || !alive_as(root, root_created)),
            "root did not die"
        );
        let _ = child.wait();
        assert!(
            alive_as(grandchild, grandchild_created),
            "the extra job handle must keep the grandchild alive after root alone dies"
        );

        ctl.kill_tree(root).unwrap();

        assert!(
            wait_until(10, || !alive_as(grandchild, grandchild_created)),
            "kill_tree must reach the grandchild via OpenJobObjectW by name - root is already \
             gone, so a taskkill /T tree walk cannot be what killed it"
        );

        unsafe {
            CloseHandle(extra_handle);
        }
    }

    /// The pid of a process matching a uniquely-tagged `ping` command line,
    /// if one is currently running. Each caller uses its own `-n <count>` as
    /// the tag, distinct from every other ping-spawning test in this file, so
    /// a hit can only be that caller's own process - unlike a bare pid, which
    /// Windows can recycle. Reused both to find a pid for a `Guard` around a
    /// backgrounded `run_shell` call (fix round 2 / R1) and to confirm a
    /// process is really gone afterward.
    ///
    /// The tag is the `-n <count>` plus a loopback target derived from this
    /// test process's own pid (`own_loopback`), so another test process
    /// running the same tests at once starts a ping with a different command
    /// line. (Ancestry cannot be used: the detached `start /b` ping is an
    /// orphan by the time it is looked for.)
    fn tagged_ping_pid(count: u32) -> Option<u32> {
        let ip = own_loopback().replace('.', "\\.");
        let out = Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "(Get-CimInstance Win32_Process -Filter \"Name='PING.EXE'\" | \
                     Where-Object {{ $_.CommandLine -match '-n {count} {ip}(\\s|\"|$)' }}).ProcessId"
                ),
            ])
            .output()
            .expect("powershell query for the tagged ping");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .lines()
            .next()
            .and_then(|l| l.trim().parse::<u32>().ok())
    }

    /// A 127.x.y.z address unique to this test process (its pid in the low
    /// 24 bits); every 127/8 address answers ping on Windows.
    fn own_loopback() -> String {
        let p = std::process::id();
        format!("127.{}.{}.{}", (p >> 16) & 255, (p >> 8) & 255, p & 255)
    }

    /// The tagged ping's pid and the creation time that identifies it
    /// (`alive_as`, `Guard::armed`). The 20 s deadline is at least as long
    /// as any `run_shell` deadline a caller races against: each scan is a
    /// `powershell Get-CimInstance` pass that costs 0.5-1.5 s, more under
    /// load.
    fn wait_for_tagged_ping_pid(count: u32) -> (u32, u64) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(pid) = tagged_ping_pid(count) {
                let created = creation_time(pid).unwrap_or_else(|| {
                    panic!("no creation time for the tagged ping '-n {count}' (pid {pid})")
                });
                return (pid, created);
            }
            if Instant::now() > deadline {
                panic!("tagged ping '-n {count}' never appeared");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// A batch file `name` in `dir` holding `body` (AC14: a cmd check runs one
    /// program, so the cmd syntax under test lives in a script).
    fn batch(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("@echo off\r\n{body}\r\n")).unwrap();
        p
    }

    /// A deadline must cut a much longer-running `ping` off promptly and
    /// leave nothing behind; a re-run whose deadline did not work would hang
    /// `thoth verify` forever and leak the child.
    ///
    /// The numbers are relative to the ping's own ~59 s lifetime (`-n 60`,
    /// a count no other tagged ping in this file uses): a 15 s deadline, a
    /// window for finding the tagged ping at least that long
    /// (`wait_for_tagged_ping_pid`; each of its `Get-CimInstance` passes
    /// costs 0.5-1.5 s, more under load, and the ping must be found before
    /// `run_shell` kills it), and a return bound of the deadline plus 10 s -
    /// under half the ping's lifetime, so a `run_shell` that waited for the
    /// ping to finish on its own would still fail.
    ///
    /// `run_shell` blocks synchronously, so the only way to get a pid for a
    /// `Guard` *before* any assertion below can panic is to run it on a
    /// background thread and poll for the tagged process concurrently from
    /// here. `-n 60` (about a minute) is also small enough that even an
    /// unguarded leak would self-heal, but the guard removes the need to
    /// rely on that.
    #[test]
    fn run_shell_kills_the_child_tree_on_timeout_and_reports_it() {
        const DEADLINE_SECS: u64 = 15;
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let program = batch(dir.path(), "long.cmd", &format!("ping -n 60 {}", own_loopback()));
        let started = Instant::now();

        let handle = std::thread::spawn(move || {
            run_shell(
                Shell::Cmd,
                &program,
                r".\long.cmd",
                &dir_path,
                Duration::from_secs(DEADLINE_SECS),
            )
        });
        let (pid, created) = wait_for_tagged_ping_pid(60);
        let _guard = Guard::armed(pid, created);

        let out = handle.join().unwrap().unwrap();
        let elapsed = started.elapsed();

        assert!(out.timed_out, "expected the {DEADLINE_SECS}s deadline to trip");
        assert_eq!(out.exit_code, None, "no exit status for a killed child");
        assert!(
            elapsed < Duration::from_secs(DEADLINE_SECS + 10),
            "run_shell took {elapsed:?} to return after a {DEADLINE_SECS}s deadline; the bound is \
             the deadline plus 10s, well under the ~59s the ping would live on its own"
        );
        assert!(
            wait_until(10, || !alive_as(pid, created)),
            "the timed-out ping child must be killed, not orphaned"
        );
    }

    /// Whole-branch review Important 4: `run_shell` bounded time but not
    /// size, so a runaway print loop inside a worker-declared verification
    /// command could put gigabytes into `thoth`'s address space and then
    /// into the blob store, which Phase 2 never collects. The cap is passed
    /// as a parameter here so a few hundred bytes stand in for the real
    /// 8 MiB - what is under test is the behaviour at the cap, not the
    /// number.
    #[test]
    fn run_shell_caps_each_stream_and_reports_the_truncation() {
        let dir = tempfile::tempdir().unwrap();
        const CAP: usize = 256;

        // ~4 KiB of stdout, well past a 256-byte cap, from a command that
        // still exits 0 of its own accord.
        let noisy = batch(
            dir.path(),
            "noisy.cmd",
            "for /L %%i in (1,1,64) do echo 0123456789012345678901234567890123456789012345678901234567890123",
        );
        let started = Instant::now();
        let out = run_shell_capped(Shell::Cmd, &noisy, r".\noisy.cmd", dir.path(), Duration::from_secs(30), CAP)
            .unwrap();

        assert_eq!(
            out.stdout.len(),
            CAP,
            "exactly the cap is kept, not one byte more"
        );
        assert!(
            out.output_truncated,
            "and the caller is told bytes were dropped"
        );
        assert_eq!(
            out.exit_code,
            Some(0),
            "truncation is not a failure: the exit code is still the command's own"
        );
        assert!(!out.timed_out);
        assert!(
            !out.output_incomplete,
            "the reader kept draining to EOF, it just stopped appending"
        );
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "capping must not make the call wait out the drain grace: {:?}",
            started.elapsed()
        );

        // A command whose output fits is not reported as truncated, even when
        // it lands exactly on the boundary of what was kept.
        let tiny = batch(dir.path(), "tiny.cmd", "echo tiny");
        let small =
            run_shell_capped(Shell::Cmd, &tiny, r".\tiny.cmd", dir.path(), Duration::from_secs(30), CAP).unwrap();
        assert!(!small.output_truncated);
        assert!(small.stdout.contains("tiny"));
    }

    /// Fix round 2 / N1: `cmd.exe` exiting is not enough to guarantee its
    /// pipes close. `start /b` detaches `ping` from `cmd.exe`'s own lifetime,
    /// but the detached child can still hold a duplicate of the very pipe
    /// handle `cmd.exe` itself inherited from `run_shell` (the same
    /// inheritance hazard this file already documents for its own detached
    /// spawns - see `clear_std_handle_inheritance`'s doc comment). `cmd.exe`
    /// returns almost immediately; the still-running `ping` must not make
    /// `run_shell` wait for it - `output_incomplete` reports it instead.
    ///
    /// The ping is `-n 75` (~74 s; a count no other tagged ping in this
    /// file uses) and the return bound is the drain grace plus 20 s, under
    /// half the ping's lifetime, so a `run_shell` that waited for the
    /// detached ping would still fail, while the few seconds the load of a
    /// second copy of the suite adds pass. The ping is asserted alive the
    /// moment `run_shell` returns - that is what shows `run_shell` did *not*
    /// contain it - and the test then ends it itself, by identity.
    #[test]
    fn run_shell_reports_incomplete_output_when_a_detached_descendant_holds_the_pipe() {
        let dir = tempfile::tempdir().unwrap();
        let dir_path = dir.path().to_path_buf();
        let program = batch(dir.path(), "detach.cmd", &format!("start /b ping -n 75 {}", own_loopback()));
        let started = Instant::now();

        let handle = std::thread::spawn(move || {
            run_shell(
                Shell::Cmd,
                &program,
                r".\detach.cmd",
                &dir_path,
                // Generous: only `cmd.exe` itself needs to finish inside
                // this, and it must do so almost immediately once `start`
                // hands `ping` off and returns.
                Duration::from_secs(30),
            )
        });
        let (pid, created) = wait_for_tagged_ping_pid(75);
        let guard = Guard::armed(pid, created);

        let out = handle.join().unwrap().unwrap();
        let elapsed = started.elapsed();

        assert!(
            !out.timed_out,
            "cmd.exe itself must exit well inside the 30s timeout; only the detached ping lingers"
        );
        assert!(
            out.output_incomplete,
            "the detached ping still holding the pipe must be reported, not silently dropped"
        );
        assert!(
            elapsed < Duration::from_secs(crate::process::DRAIN_GRACE_SECS + 20),
            "run_shell took {elapsed:?}; it must return within the drain grace plus 20s, not wait \
             out ping's own ~74s lifetime"
        );

        assert!(
            alive_as(pid, created),
            "the detached ping must still be running when run_shell returns: run_shell does not \
             contain it (no Job Object containment this round, by design - see run_shell's doc \
             comment)"
        );
        // The guard kills only the process it was armed for (it compares the
        // creation time first), so this ends our ping and nobody else's.
        drop(guard);
        assert!(
            wait_until(10, || !alive_as(pid, created)),
            "the detached ping did not end after the test killed it by identity"
        );
    }

    /// What one copy of the unit-test binary reported in one stress round
    /// (`stress_copies_of_the_unit_suite_run_at_once`).
    struct StressCopy {
        round: usize,
        copy: usize,
        /// `None` when the copy ran past the round's deadline and was ended.
        exit_code: Option<i32>,
        timed_out: bool,
        /// Every `test NAME ... FAILED` line's NAME, in order of appearance.
        failed: Vec<String>,
        /// Each `panicked at` line together with the line after it (the
        /// panic message), joined with a newline.
        panics: Vec<String>,
    }

    /// Pulls the failed test names and the panic lines out of one copy's
    /// combined stdout and stderr (pure; `stress_copies_of_the_unit_suite_run_at_once`).
    fn stress_report(text: &str) -> (Vec<String>, Vec<String>) {
        let mut failed = Vec::new();
        let mut panics = Vec::new();
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let line = line.trim_end();
            if let Some(rest) = line.strip_prefix("test ") {
                if let Some(name) = rest.strip_suffix(" ... FAILED") {
                    failed.push(name.to_string());
                }
            }
            if line.contains("panicked at") {
                let next = lines.get(i + 1).map(|l| l.trim_end()).unwrap_or("");
                panics.push(format!("{line}\n{next}"));
            }
        }
        (failed, panics)
    }

    /// Waits for the copies of one stress round together, then hands each
    /// copy's outcome - its number, exit code (`None` when it was ended)
    /// and whether it was still running at `deadline` and had to be ended
    /// (`kill` + `wait`) - to `handle`, in the order the copies were given.
    /// Every copy is polled on every pass and every straggler is ended on
    /// the first pass after the deadline; `handle` runs only once all of
    /// them have ended, so however long it takes, no copy is left running
    /// unwatched meanwhile.
    fn wait_round(
        running: Vec<(usize, Child)>,
        deadline: Instant,
        mut handle: impl FnMut(usize, Option<i32>, bool),
    ) {
        let mut pending = running;
        let mut outcomes: Vec<(usize, Option<i32>, bool)> = Vec::new();
        while !pending.is_empty() {
            let past_deadline = Instant::now() > deadline;
            let mut still_running = Vec::new();
            for (copy, mut child) in pending {
                match child.try_wait().expect("try_wait") {
                    Some(status) => outcomes.push((copy, status.code(), false)),
                    None if past_deadline => {
                        let _ = child.kill();
                        let _ = child.wait();
                        outcomes.push((copy, None, true));
                    }
                    None => still_running.push((copy, child)),
                }
            }
            pending = still_running;
            if !pending.is_empty() {
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        outcomes.sort_by_key(|(copy, _, _)| *copy);
        for (copy, exit_code, timed_out) in outcomes {
            handle(copy, exit_code, timed_out);
        }
    }

    /// Handling one copy's outcome takes time (its output is a whole
    /// suite's log), so `wait_round` must have ended every copy that was
    /// still running at the deadline before it hands any outcome over:
    /// a copy left running meanwhile could exit on its own past the
    /// deadline and then be taken for one that finished in time.
    #[test]
    fn wait_round_ends_every_copy_still_running_at_the_deadline_before_handling_any_output() {
        let ping = |count: &str| {
            Command::new("ping")
                .args(["-n", count, "127.0.0.1"])
                .stdout(std::process::Stdio::null())
                .spawn()
                .expect("spawn ping")
        };
        let quick = ping("1");
        let lingering = ping("4");
        let lingering_pid = lingering.id();
        let lingering_created =
            creation_time(lingering_pid).expect("creation time of the ping just spawned");
        let _guard = Guard::armed(lingering_pid, lingering_created);
        let deadline = Instant::now() + Duration::from_secs(1);

        let mut outcomes = Vec::new();
        wait_round(vec![(1, quick), (2, lingering)], deadline, |copy, exit_code, timed_out| {
            if copy == 1 {
                // Handling the quick copy outlives the lingering copy's
                // own ~3 s: if that copy is still running now, it will
                // exit on its own, past the deadline, before anyone looks.
                std::thread::sleep(Duration::from_secs(5));
            }
            outcomes.push((copy, exit_code, timed_out));
        });

        assert_eq!(outcomes.len(), 2, "{outcomes:?}");
        assert_eq!(
            (outcomes[0].0, outcomes[0].2),
            (1, false),
            "the quick copy finished inside the deadline: {outcomes:?}"
        );
        assert_eq!(
            (outcomes[1].0, outcomes[1].1, outcomes[1].2),
            (2, None, true),
            "the copy still running at the deadline must have been ended there and reported as \
             timed out, not taken for one that finished on its own while the quick copy's \
             outcome was handled: {outcomes:?}"
        );
        assert!(
            !alive_as(lingering_pid, lingering_created),
            "a copy reported as timed out must really have been ended"
        );
    }

    /// The false reds this module is prone to come from a second copy of
    /// the whole suite running on the same machine. This reproduces that
    /// load with copies of this very binary - `std::env::current_exe()` with no arguments runs
    /// every unit test except `#[ignore]`d ones such as this, so it never
    /// recurses - and asserts that no copy, in any round, fails a
    /// `process::windows::tests::` test. Other tests' failures are printed
    /// but not asserted on; a copy that is ended at the round's deadline, or
    /// exits non-zero without naming a failed test (a crash), counts as a
    /// failure too.
    ///
    /// `THOTH_STRESS_COPIES` (default 2) copies per round, `THOTH_STRESS_ROUNDS`
    /// (default 3) rounds. Each copy's stdout and stderr go to files, not
    /// pipes: the suite leaves a detached process behind on purpose
    /// (`run_shell_reports_incomplete_output_when_a_detached_descendant_holds_the_pipe`)
    /// and a pipe would not close until it exited.
    ///
    /// `cargo test --bin thoth stress_copies -- --ignored --nocapture`, or
    /// `THOTH_STRESS_COPIES=3 THOTH_STRESS_ROUNDS=3 cargo test --bin thoth stress_copies -- --ignored --nocapture`.
    #[test]
    #[ignore = "runs copies of the whole unit-test binary at once; run with -- --ignored"]
    fn stress_copies_of_the_unit_suite_run_at_once() {
        use std::process::Stdio;
        const ROUND_DEADLINE_SECS: u64 = 300;
        let positive = |var: &str, default: usize| -> usize {
            std::env::var(var)
                .ok()
                .and_then(|v| v.trim().parse::<usize>().ok())
                .filter(|n| *n > 0)
                .unwrap_or(default)
        };
        let copies = positive("THOTH_STRESS_COPIES", 2);
        let rounds = positive("THOTH_STRESS_ROUNDS", 3);
        let exe = std::env::current_exe().expect("current_exe");
        let dir = tempfile::tempdir().unwrap();
        println!(
            "stress: {copies} copies x {rounds} rounds of {} (output under {})",
            exe.display(),
            dir.path().display()
        );

        let mut reports: Vec<StressCopy> = Vec::new();
        for round in 1..=rounds {
            let round_started = Instant::now();
            let deadline = round_started + Duration::from_secs(ROUND_DEADLINE_SECS);
            let log = |copy: usize, stream: &str| {
                dir.path().join(format!("round{round}-copy{copy}.{stream}"))
            };
            let mut running = Vec::new();
            for copy in 1..=copies {
                let child = Command::new(&exe)
                    .stdin(Stdio::null())
                    .stdout(Stdio::from(std::fs::File::create(log(copy, "stdout")).unwrap()))
                    .stderr(Stdio::from(std::fs::File::create(log(copy, "stderr")).unwrap()))
                    .spawn()
                    .unwrap_or_else(|e| panic!("round {round} copy {copy}: spawn {}: {e}", exe.display()));
                running.push((copy, child));
            }
            wait_round(running, deadline, |copy, exit_code, timed_out| {
                let mut text = std::fs::read_to_string(log(copy, "stdout")).unwrap_or_default();
                text.push('\n');
                text.push_str(&std::fs::read_to_string(log(copy, "stderr")).unwrap_or_default());
                let (failed, panics) = stress_report(&text);
                reports.push(StressCopy {
                    round,
                    copy,
                    exit_code,
                    timed_out,
                    failed,
                    panics,
                });
            });
            println!(
                "stress: round {round} finished in {:?}",
                round_started.elapsed()
            );
        }

        let mut problems: Vec<String> = Vec::new();
        for r in &reports {
            let where_ = format!("round {} copy {}", r.round, r.copy);
            if r.timed_out {
                problems.push(format!(
                    "{where_}: timed out after {ROUND_DEADLINE_SECS}s and was ended"
                ));
            } else if r.exit_code != Some(0) && r.failed.is_empty() {
                problems.push(format!(
                    "{where_}: exited {:?} without naming a failed test (crashed?)",
                    r.exit_code
                ));
            }
            for name in &r.failed {
                let panic = r
                    .panics
                    .iter()
                    .find(|p| p.contains(&format!("'{name}'")))
                    .cloned()
                    .unwrap_or_else(|| "(no panic lines found)".to_string());
                let line = format!("{where_}: {name} FAILED\n{panic}");
                println!("{line}");
                if name.starts_with("process::windows::tests::") {
                    problems.push(line);
                }
            }
            println!(
                "stress: {where_}: exit {:?}, {} failed test(s)",
                r.exit_code,
                r.failed.len()
            );
        }
        assert!(
            problems.is_empty(),
            "process::windows tests failed (or a copy did not finish) while {copies} copies of the \
             unit suite ran at once:\n{}",
            problems.join("\n")
        );
    }
}
