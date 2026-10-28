//! `thoth profile login`: wait for an interactive CLI that shares this
//! terminal while thoth itself ignores Ctrl+C.
//!
//! claude and agy are left with Ctrl+C, and the terminal sends the same
//! Ctrl+C to thoth too; without this thoth would die and never check the
//! login. Ignoring starts only after the child is spawned: Windows' "ignore
//! Ctrl+C" flag and Linux's `SIG_IGN` are inherited, so setting either before
//! the spawn would take Ctrl+C away from the CLI as well. Failing to set or
//! restore is not fatal; the child is already running, so thoth waits anyway.

use std::io;
use std::process::{Child, ExitStatus};

#[cfg(windows)]
pub fn ignore_ctrl_c_while(child: &mut Child) -> io::Result<ExitStatus> {
    use windows_sys::core::BOOL;
    use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};

    /// Handled (TRUE) for Ctrl+C and Ctrl+Break; anything else, such as
    /// closing the window, falls through to the default and ends thoth.
    unsafe extern "system" fn swallow(ctrl_type: u32) -> BOOL {
        (ctrl_type == CTRL_C_EVENT || ctrl_type == CTRL_BREAK_EVENT) as BOOL
    }

    // SAFETY: `swallow` is a plain function that lives for the whole program
    // and touches no state; it is removed with the same pointer below.
    let added = unsafe { SetConsoleCtrlHandler(Some(swallow), 1) } != 0;
    let status = child.wait();
    if added {
        // SAFETY: as above.
        unsafe { SetConsoleCtrlHandler(Some(swallow), 0) };
    }
    status
}

#[cfg(target_os = "linux")]
pub fn ignore_ctrl_c_while(child: &mut Child) -> io::Result<ExitStatus> {
    // SAFETY: `signal` with SIG_IGN installs no code of ours; the previous
    // disposition it returns is put back unchanged after the wait.
    let previous = unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) };
    let status = child.wait();
    if previous != libc::SIG_ERR {
        // SAFETY: as above.
        unsafe { libc::signal(libc::SIGINT, previous) };
    }
    status
}
