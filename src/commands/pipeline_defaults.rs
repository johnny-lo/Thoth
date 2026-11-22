//! LIGHT-1: the commander's stored default implementer and reviewer lists
//! (docs/post-phase2/consolidation-scope-2026-11-13.md, section 3).
//!
//! `thoth pipeline defaults set | show | clear` keeps one implementer list and
//! one reviewer list per machine in the user-visible file
//! `THOTH_HOME/pipeline-defaults.json`, shared by every repository of that
//! THOTH_HOME. `thoth pipeline start` on a low or normal risk code task uses a
//! stored list for a list flag that is left out. A stored list is the
//! commander's choice: it is used as stored, in its stored order with
//! duplicates; Thoth never picks, ranks, reorders or shortens it. It is not a
//! `thoth config` setting (those are the human's).
//!
//! The file is written whole through a temporary file in the same folder and
//! a rename, so a reader never sees half of it. Entries are checked by the
//! same rules as `pipeline start`'s flag entries, when stored and again at
//! every start (a profile may be gone or a model no longer accepted).

use crate::commands::pipeline::{check_entry, parse_entry, Entry};
use crate::config::Paths;
use crate::db;
use crate::output;
use crate::redact::Masker;
use anyhow::{anyhow, bail, Context, Result};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = "pipeline-defaults.json";

/// One stored list: its entries in the commander's order, the reason given
/// when it was stored (masked) and when that was (RFC 3339).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct StoredList {
    pub entries: Vec<Entry>,
    pub reason: String,
    pub set_at: String,
}

/// The whole file. A list that is not stored is null (or left out).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub implementers: Option<StoredList>,
    #[serde(default)]
    pub reviewers: Option<StoredList>,
}

/// THOTH_HOME/pipeline-defaults.json, absolute.
pub fn path(paths: &Paths) -> PathBuf {
    let p = paths.home.join(FILE_NAME);
    std::path::absolute(&p).unwrap_or(p)
}

/// The stored file: `None` when there is none, an error naming the path when
/// it cannot be read or parsed. A stored list with no entry is a file that
/// cannot be parsed, so a hand-edited file never starts a pipeline with an
/// empty list.
pub fn read(paths: &Paths) -> Result<Option<Defaults>> {
    let p = path(paths);
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => bail!("cannot read the pipeline defaults file {}: {e}", p.display()),
    };
    let d: Defaults = serde_json::from_str(&text)
        .map_err(|e| anyhow!("cannot parse the pipeline defaults file {}: {e}", p.display()))?;
    for (name, list) in [("implementers", &d.implementers), ("reviewers", &d.reviewers)] {
        if list.as_ref().is_some_and(|l| l.entries.is_empty()) {
            bail!(
                "cannot parse the pipeline defaults file {}: the stored {name} list has no entry",
                p.display()
            );
        }
    }
    Ok(Some(d))
}

fn write(p: &Path, d: &Defaults) -> Result<()> {
    let dir = p.parent().ok_or_else(|| anyhow!("{} has no parent folder", p.display()))?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(".{FILE_NAME}.{}.{nanos}.tmp", std::process::id()));
    let mut text = serde_json::to_string_pretty(d)?;
    text.push('\n');
    let done = (|| -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, p)
    })();
    if let Err(e) = done {
        let _ = std::fs::remove_file(&tmp);
        bail!("could not write the pipeline defaults file {}: {e}", p.display());
    }
    Ok(())
}

/// A stored list in the shape every `--json` output uses: null, or
/// `{"entries", "reason", "set_at"}`.
pub fn list_json(list: &Option<StoredList>) -> Value {
    match list {
        None => Value::Null,
        Some(l) => json!({"entries": l.entries, "reason": l.reason, "set_at": l.set_at}),
    }
}

fn refuse_in_run(command: &str) -> Result<()> {
    if std::env::var_os("THOTH_RUN_ID").is_some() {
        bail!(
            "pipeline defaults {command} refuses to run with THOTH_RUN_ID set: the default lists are \
             stored by the commander, not by a run"
        );
    }
    Ok(())
}

fn entries_text(entries: &[Entry]) -> String {
    entries.iter().map(Entry::text).collect::<Vec<_>>().join(", ")
}

pub fn set(paths: &Paths, implementers: &[String], reviewers: &[String], reason: Option<&str>, json: bool) -> Result<i32> {
    refuse_in_run("set")?;
    if implementers.is_empty() && reviewers.is_empty() {
        bail!(
            "give --implementer PROFILE:MODEL[:EFFORT] or --reviewer PROFILE:MODEL[:EFFORT] at least once: \
             a list whose flag is given replaces that stored list, one left out stays as stored"
        );
    }
    let reason = match reason.map(str::trim) {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => bail!("--reason TEXT is required: say why these lists are your defaults; it is stored with them"),
    };
    let gconn = if paths.global_db().is_file() { Some(db::open_global(paths)?) } else { None };
    let mut checked: Vec<(&str, Vec<Entry>)> = Vec::new();
    for (name, flag, raw) in [("implementers", "--implementer", implementers), ("reviewers", "--reviewer", reviewers)] {
        if raw.is_empty() {
            continue;
        }
        let mut list = Vec::new();
        for (i, r) in raw.iter().enumerate() {
            list.push(parse_entry(gconn.as_ref(), flag, i + 1, raw.len(), r)?);
        }
        checked.push((name, list));
    }
    let p = path(paths);
    let mut d = read(paths)?.unwrap_or_default();
    let masked = Masker::load(Some(&paths.home)).mask_text(&reason).0;
    let now = crate::ids::now_rfc3339();
    let mut replaced = Vec::new();
    for (name, entries) in checked {
        let list = Some(StoredList { entries, reason: masked.clone(), set_at: now.clone() });
        if name == "implementers" {
            d.implementers = list;
        } else {
            d.reviewers = list;
        }
        replaced.push(name);
    }
    write(&p, &d)?;
    let v = json!({
        "ok": true,
        "path": p.display().to_string(),
        "implementers": list_json(&d.implementers),
        "reviewers": list_json(&d.reviewers),
        "replaced": replaced,
    });
    output::emit(
        json,
        &v,
        &format!(
            "stored the default {} in {}; `thoth pipeline start TASK --reason TEXT` uses a stored list for a \
             list flag you leave out on a low or normal risk task\n{}",
            replaced.join(" and "),
            p.display(),
            human_lists(&d, None)
        ),
    );
    Ok(0)
}

/// The profile database for `show`, opened read-only: `Ok(None)` when there
/// is none (every entry then has no such profile), an error text when it
/// cannot be opened.
fn read_only_global(paths: &Paths) -> std::result::Result<Option<Connection>, String> {
    let p = paths.global_db();
    if !p.is_file() {
        return Ok(None);
    }
    crate::commands::usage::global_read_only(paths).map(Some).map_err(|e| format!("{e:#}"))
}

/// Every stored entry of `list` that fails `pipeline start`'s entry check
/// on this machine now.
fn invalid(gconn: &std::result::Result<Option<Connection>, String>, list: &StoredList) -> Vec<Value> {
    let mut out = Vec::new();
    for (i, e) in list.entries.iter().enumerate() {
        let text = e.text();
        let why = match gconn {
            Err(open) => Some(open.clone()),
            Ok(conn) => match check_entry(conn.as_ref(), &text) {
                Ok(Ok(_)) => None,
                Ok(Err(why)) => Some(why),
                Err(err) => Some(format!("{err:#}")),
            },
        };
        if let Some(why) = why {
            out.push(json!({"entry_index": i + 1, "entry": text, "error": why}));
        }
    }
    out
}

fn human_lists(d: &Defaults, invalid: Option<(&[Value], &[Value])>) -> String {
    let mut lines = Vec::new();
    for (k, (name, list)) in [("implementers", &d.implementers), ("reviewers", &d.reviewers)].into_iter().enumerate() {
        match list {
            None => lines.push(format!("{name}: none stored")),
            Some(l) => {
                lines.push(format!("{name}: {}", entries_text(&l.entries)));
                lines.push(format!("  reason: {}; set at {}", l.reason, l.set_at));
                let bad = invalid.map(|(a, b)| if k == 0 { a } else { b }).unwrap_or(&[]);
                for b in bad {
                    lines.push(format!(
                        "  INVALID entry {} ({}): {}",
                        b["entry_index"],
                        b["entry"].as_str().unwrap_or("?"),
                        b["error"].as_str().unwrap_or("")
                    ));
                }
            }
        }
    }
    lines.join("\n")
}

pub fn show(paths: &Paths, json: bool) -> Result<i32> {
    let p = path(paths);
    let Some(d) = read(paths)? else {
        let v = json!({"ok": true, "path": p.display().to_string(), "exists": false, "implementers": null, "reviewers": null});
        output::emit(
            json,
            &v,
            &format!(
                "no default lists are stored ({} does not exist); store them with `thoth pipeline defaults set \
                 --implementer PROFILE:MODEL[:EFFORT] --reviewer PROFILE:MODEL[:EFFORT] --reason TEXT`",
                p.display()
            ),
        );
        return Ok(0);
    };
    let gconn = read_only_global(paths);
    let with_invalid = |list: &Option<StoredList>| -> (Value, Vec<Value>) {
        match list {
            None => (Value::Null, Vec::new()),
            Some(l) => {
                let bad = invalid(&gconn, l);
                let mut v = list_json(list);
                v["invalid"] = Value::Array(bad.clone());
                (v, bad)
            }
        }
    };
    let (iv, ibad) = with_invalid(&d.implementers);
    let (rv, rbad) = with_invalid(&d.reviewers);
    let v = json!({"ok": true, "path": p.display().to_string(), "exists": true, "implementers": iv, "reviewers": rv});
    output::emit(
        json,
        &v,
        &format!("pipeline defaults in {}\n{}", p.display(), human_lists(&d, Some((&ibad, &rbad)))),
    );
    Ok(0)
}

pub fn clear(paths: &Paths, only: Option<&str>, json: bool) -> Result<i32> {
    refuse_in_run("clear")?;
    let p = path(paths);
    let names: Vec<&str> = match only {
        None => vec!["implementers", "reviewers"],
        Some("implementer") => vec!["implementers"],
        Some("reviewer") => vec!["reviewers"],
        Some(other) => bail!("invalid --only '{other}': write implementer or reviewer"),
    };
    let (previous, after, cleared, warning) = match read(paths) {
        Ok(None) => (Defaults::default(), Defaults::default(), Vec::new(), None),
        Ok(Some(before)) => {
            let mut after = before.clone();
            let mut cleared = Vec::new();
            for name in &names {
                let slot = if *name == "implementers" { &mut after.implementers } else { &mut after.reviewers };
                if slot.take().is_some() {
                    cleared.push(*name);
                }
            }
            if !cleared.is_empty() {
                write(&p, &after)?;
            }
            (before, after, cleared, None)
        }
        Err(e) if only.is_none() => {
            let after = Defaults::default();
            write(&p, &after)?;
            let warning = format!("{e:#}; it was replaced with a file that holds no list");
            (Defaults::default(), after, Vec::new(), Some(warning))
        }
        Err(e) => return Err(e),
    };
    let v = json!({
        "ok": true,
        "path": p.display().to_string(),
        "cleared": cleared,
        "previous": {"implementers": list_json(&previous.implementers), "reviewers": list_json(&previous.reviewers)},
        "implementers": list_json(&after.implementers),
        "reviewers": list_json(&after.reviewers),
        "warning": warning,
    });
    let mut human = Vec::new();
    if let Some(w) = &warning {
        human.push(format!("warning: {w}"));
    }
    human.push(if cleared.is_empty() {
        format!("nothing to clear in {}", p.display())
    } else {
        format!("cleared the default {} in {}", cleared.join(" and "), p.display())
    });
    human.push(human_lists(&after, None));
    output::emit(json, &v, &human.join("\n"));
    Ok(0)
}
