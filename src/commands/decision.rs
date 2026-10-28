//! `thoth decision add` (v6 §6.4, §10.7).
//!
//! v6's point is that the *mechanism* already exists: every harness loads
//! `AGENTS.md` natively, so long-lived decisions belong in a file the repo
//! already carries. The only gap is the write path - a decision happens in a
//! Human ↔ Supervisor conversation, and without one command to land it, the
//! file drifts out of date. This is that command, and nothing more: it appends
//! one line and records one event. It deliberately does **not** commit
//! (brief §10) - what gets committed and when is the human's call.

use crate::config::Paths;
use crate::db::state::{self, NewEvent};
use crate::ids;
use crate::model::event_type;
use crate::output;
use anyhow::{bail, Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const DEFAULT_DECISIONS_FILE: &str = "decisions.md";

/// Written once, when the file has to be created, so a fresh `decisions.md`
/// explains itself to the next reader.
pub const DECISIONS_HEADING: &str = "# Decisions\n\nLong-lived decisions for this repository, appended by `thoth decision add`.\nEvery harness loads AGENTS.md natively (v6 section 6.4), so AGENTS.md should\nreference this file for these to reach a worker.\n\n";

/// brief §10: `- YYYY-MM-DD: text`, one line.
///
/// Whitespace is collapsed so a pasted multi-line decision stays one bullet -
/// a decision that becomes three unlabelled lines is a decision nobody can
/// cite later.
pub fn decision_line(date: &str, text: &str) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    format!("- {date}: {collapsed}\n")
}

pub fn add(
    paths: &Paths,
    cwd: &Path,
    text: &str,
    file: Option<&Path>,
    json: bool,
) -> Result<i32> {
    if text.trim().is_empty() {
        bail!("a decision needs some text");
    }
    let (_repo_id, root, conn) = crate::commands::task::open_repo(paths, cwd)?;
    // A relative `--file` is relative to the repo root, not to the caller's cwd:
    // the file belongs to the repository, and `repo_root_from` now resolves the
    // main tree even from inside a worktree (Task 6).
    let target: PathBuf = match file {
        Some(p) if p.is_absolute() => p.to_path_buf(),
        Some(p) => root.join(p),
        None => root.join(DEFAULT_DECISIONS_FILE),
    };

    let existed = target.is_file();
    if !existed {
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&target, DECISIONS_HEADING)
            .with_context(|| format!("creating {}", target.display()))?;
    }

    let now = ids::now_rfc3339();
    let date: String = now.chars().take(10).collect();
    let line = decision_line(&date, text);
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&target)
        .with_context(|| format!("opening {}", target.display()))?;
    f.write_all(line.as_bytes())
        .with_context(|| format!("appending to {}", target.display()))?;
    drop(f);

    // v6 §6.4 rides on the harness's own AGENTS.md loading, so a repo without
    // one records the decision but nobody reads it. Worth saying once.
    let mut warnings: Vec<String> = Vec::new();
    let agents = root.join("AGENTS.md");
    if !agents.is_file() {
        warnings.push(format!(
            "{} does not exist, so no harness will load these decisions automatically; \
             create it and have it reference {}",
            agents.display(),
            target.display()
        ));
    }

    state::append_event(
        &conn,
        &NewEvent {
            task_id: None,
            run_id: None,
            event_type: event_type::DECISION_ADDED.to_string(),
            detail: serde_json::json!({
                "file": target.display().to_string(),
                "date": date,
                "text": text,
                "created_file": !existed,
            }),
        },
    )?;

    if !json {
        for w in &warnings {
            masked_eprintln!("warning: {w}");
        }
    }
    let v = serde_json::json!({
        "ok": true,
        "file": target.display().to_string(),
        "line": line.trim_end(),
        "created_file": !existed,
        "committed": false,
        "warnings": warnings,
    });
    output::emit(
        json,
        &v,
        &format!(
            "appended to {}: {}\n(not committed - commit it yourself)",
            target.display(),
            line.trim_end()
        ),
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_line_is_one_dated_bullet_with_whitespace_collapsed() {
        assert_eq!(
            decision_line("2026-09-16", "use SQLite, not Postgres"),
            "- 2026-09-16: use SQLite, not Postgres\n"
        );
        // a multi-line decision must not become multiple bullets
        assert_eq!(
            decision_line("2026-09-16", "first line\nsecond line"),
            "- 2026-09-16: first line second line\n"
        );
        assert_eq!(
            decision_line("2026-09-16", "  padded   text  "),
            "- 2026-09-16: padded text\n"
        );
    }
}
