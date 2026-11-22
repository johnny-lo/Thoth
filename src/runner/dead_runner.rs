//! SMALL-11 (4): a `running` run whose runner process is gone.
//!
//! A runner that dies without settling its run (a killed process, a machine
//! restart) leaves the row `running` for ever; its heartbeat just stops.
//! `thoth status` settles such a run as `interrupted` (`crash`) when it looks;
//! `profile usage`, `recommend` and the live quota refresh only leave it out of
//! their live-run counts and never write.
//!
//! A run counts as dead only when all three hold: it is `running`, its
//! heartbeat is older than `status::STALE_AFTER_SECONDS`, and the pid its
//! runner recorded is not a running thoth process. A run with no recorded
//! `runner_pid` is never judged. The pid alone is not trusted: a pid that now
//! runs another program is as gone as a pid nobody has, but a pid that cannot
//! be asked about counts as alive, since a wrong settle is worse than a late one.

use crate::db::state::{self, Run, RunFinish};
use crate::model::{role, InterruptedReason, RunStatus, TaskStatus};
use anyhow::Result;
use rusqlite::Connection;
use std::path::Path;

/// The file stem of a process image (`thoth`, `thoth.exe` and `Thoth.EXE`
/// all read `thoth`).
fn image_stem(image: &str) -> String {
    let name = Path::new(image)
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| image.to_lowercase());
    name.strip_suffix(".exe").unwrap_or(&name).to_string()
}

/// Whether the process with `pid` is a running thoth process. `true` when the
/// answer is unclear (the process list could not be read). A renamed binary
/// (`thoth.old-DATE`, which a runner started before an upgrade keeps running
/// as) is still a thoth process.
fn is_thoth_process(pid: i64) -> bool {
    let Ok(pid) = u32::try_from(pid) else {
        return false;
    };
    match crate::process::image_name(pid) {
        Ok(None) => false,
        Ok(Some(seen)) => {
            let seen = image_stem(&seen);
            let own = std::env::current_exe()
                .map(|p| image_stem(&p.to_string_lossy()))
                .unwrap_or_else(|_| "thoth".to_string());
            seen == own || seen.starts_with("thoth")
        }
        Err(_) => true,
    }
}

/// Whether this run is one the rule settles: `running`, a stale heartbeat, a
/// recorded runner pid that is no running thoth process.
pub(crate) fn runner_is_gone(
    run_status: &str,
    heartbeat_at: Option<&str>,
    runner_pid: Option<i64>,
    now: &str,
) -> bool {
    let Some(pid) = runner_pid else {
        return false;
    };
    crate::commands::status::heartbeat_is_stale(now, heartbeat_at, run_status) && !is_thoth_process(pid)
}

/// Settles every run of the repository that `runner_is_gone`: the run becomes
/// `interrupted` with reason `crash`, one `status_change` event (with
/// `settled_by: "status"`) and one `run_interrupted` event are written, and an
/// implementer run's task becomes `interrupted` too, as when a runner records a
/// crash itself. A reviewer or spec reviewer run leaves its task alone so the
/// review can be started again. A run another writer settled first is left as
/// it is. Returns the settled run ids.
pub(crate) fn settle_dead_runners(conn: &Connection, now: &str) -> Result<Vec<String>> {
    let mut settled = Vec::new();
    for run in state::list_running_runs_with_runner(conn)? {
        if !runner_is_gone(&run.status, run.heartbeat_at.as_deref(), run.runner_pid, now) {
            continue;
        }
        if settle_one(conn, &run, now)? {
            settled.push(run.run_id.clone());
        }
    }
    Ok(settled)
}

fn settle_one(conn: &Connection, run: &Run, now: &str) -> Result<bool> {
    let pid = run.runner_pid.unwrap_or_default();
    let note = format!(
        "runner process {pid} was gone when thoth status looked: the run's heartbeat stopped at {} \
         and no thoth process has that pid",
        run.heartbeat_at.as_deref().unwrap_or("-")
    );
    let finish = RunFinish {
        status: RunStatus::Interrupted,
        interrupted_reason: Some(InterruptedReason::Crash),
        exit_code: None,
        candidate_commit: None,
        error_text: Some(note.clone()),
        ended_at: now.to_string(),
    };
    let task_status = if run.role == role::REVIEWER || run.role == role::SPEC_REVIEWER {
        None
    } else {
        Some(super::TaskStatusWrite::unconditional(TaskStatus::Interrupted))
    };
    let status_change = serde_json::json!({
        "entity": "run",
        "id": run.run_id,
        "to": "interrupted",
        "interrupted_reason": "crash",
        "note": note,
        "runner_pid": pid,
        "settled_by": "status",
        "task_status_written": task_status.map(|w| w.to.as_str()),
    });
    let interrupted = serde_json::json!({
        "reason": "crash",
        "note": note,
        "harness_session_id": run.harness_session_id,
        "settled_by": "status",
    });
    super::settle_terminal(conn, run, &finish, task_status, Some(interrupted), status_change)
}
