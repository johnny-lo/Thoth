use crate::config::Paths;
use crate::db;
use crate::db::global;
use crate::db::state;
use crate::ids;
use crate::output;
use crate::worktree;
use anyhow::Result;
use std::path::{Path, PathBuf};

/// Registers the repository containing `cwd` without printing anything;
/// returns its repo id, root and state.db path. `thoth setup` calls it too.
pub fn init_repo(paths: &Paths, cwd: &Path) -> Result<(String, PathBuf, PathBuf)> {
    let root = worktree::repo_root_from(cwd)?;
    let repo_id = ids::repo_id_for(&root)?;
    let now = ids::now_rfc3339();

    let repo_dir = paths.repo_dir(&repo_id);
    std::fs::create_dir_all(paths.worktrees_dir(&repo_id))?;
    std::fs::create_dir_all(repo_dir.join("runs"))?;
    std::fs::create_dir_all(paths.blobs_dir(&repo_id))?;

    let sconn = db::open_state(paths, &repo_id)?;
    state::set_meta(&sconn, "repo_root", &root.display().to_string())?;
    state::set_meta(
        &sconn,
        "schema_version",
        &db::migrations::STATE_SCHEMA_VERSION.to_string(),
    )?;

    let gconn = db::open_global(paths)?;
    global::upsert_repo(&gconn, &repo_id, &root.display().to_string(), &now)?;

    let state_db = paths.state_db(&repo_id);
    Ok((repo_id, root, state_db))
}

pub fn run(paths: &Paths, cwd: &Path, json: bool) -> Result<i32> {
    let (repo_id, root, state_db) = init_repo(paths, cwd)?;

    let v = serde_json::json!({
        "ok": true,
        "repo_id": repo_id,
        "repo_root": root.display().to_string(),
        "state_db": state_db.display().to_string(),
    });
    output::emit(
        json,
        &v,
        &format!(
            "initialised repo {} ({})\n  state: {}",
            repo_id,
            root.display(),
            state_db.display()
        ),
    );
    Ok(0)
}
