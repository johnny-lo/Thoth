use crate::db::state::Run;

/// The message every live-run guard refuses with (whole-branch review
/// Important 1/3/5).
///
/// One wording for all four guards (`dispatch`, `resume`, `submit`,
/// `review start`), because they are one mechanism: something else is still
/// in flight for this task, and the operator's two ways out are the same
/// every time. Names the run, its role and its status, as the ruling
/// requires - a bare "a run is live" leaves the operator hunting for which.
pub fn live_run_refusal(what: &str, task_id: &str, live: &Run) -> String {
    format!(
        "{what}: run {} ({}) of task {task_id} is still '{}'. Wait for it with \
         `thoth wait --run {}`, or stop it with `thoth cancel {}`.",
        live.run_id, live.role, live.status, live.run_id, live.run_id
    )
}

pub mod arena;
pub mod auto_pick;
pub mod cancel;
pub mod checks;
pub mod decision;
pub mod decision_table;
pub mod dispatch;
pub mod doctor;
pub mod evidence;
pub mod events;
pub mod hook;
pub mod init;
pub mod live_quota;
pub mod models;
pub mod own_usage;
pub mod profile;
pub mod profile_login;
pub mod report;
pub mod settings;
pub mod setup;
pub mod resume;
pub mod retro;
pub mod review;
pub mod review_packet;
pub mod secrets;
pub mod spec_lint;
pub mod spec_review;
pub mod status;
pub mod submit;
pub mod tables;
pub mod task;
pub mod task_history;
pub mod transcript;
pub mod verify;
pub mod wait;
pub mod worktrees;
pub mod usage;
