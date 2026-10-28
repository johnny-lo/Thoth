use crate::config::Paths;
use crate::db::state;
use crate::ids;
use crate::output;
use anyhow::Result;
use std::path::Path;

pub fn run(paths: &Paths, cwd: &Path, since: i64, task: Option<&str>, json: bool) -> Result<i32> {
    let (_repo_id, _root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    let task_id = match task {
        Some(needle) => Some(ids::resolve_prefix(&state::list_task_ids(&conn)?, needle)?),
        None => None,
    };
    let rows = state::events_since(&conn, since, task_id.as_deref())?;
    let last_seq = rows.last().map(|r| r.seq).unwrap_or(since);
    let arr: Vec<serde_json::Value> = rows
        .iter()
        .map(|e| {
            serde_json::json!({
                "seq": e.seq,
                "ts": e.ts,
                "task_id": e.task_id,
                "run_id": e.run_id,
                "type": e.event_type,
                "detail": serde_json::from_str::<serde_json::Value>(&e.detail)
                    .unwrap_or(serde_json::Value::Null),
            })
        })
        .collect();
    let human = if rows.is_empty() {
        format!("no events after seq {since}")
    } else {
        rows.iter()
            .map(|e| {
                format!(
                    "{:>6}  {}  {:<20} {}  {}",
                    e.seq,
                    e.ts,
                    e.event_type,
                    e.task_id.clone().unwrap_or_else(|| "-".to_string()),
                    e.detail
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    output::emit(
        json,
        &serde_json::json!({ "events": arr, "last_seq": last_seq }),
        &human,
    );
    Ok(0)
}
