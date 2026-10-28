//! `run_quota`: roughly how many percentage points of its account's
//! five-hour and weekly windows a run used (S3-12, DT-1).
//!
//! The runner writes one event per implementer or reviewer run of a
//! claude-code or codex profile, after the stream has ended and before the
//! run's final `status_change`, whatever the outcome. `thoth models` reads the
//! newest one per run into `quota_points` / `quota_runs` and their weekly
//! counterparts.
//!
//! The number is an estimate: codex reports whole percents, and any other
//! work on the same account during the run counts too. DT-1 marks the event
//! `reliable: false`, with the reasons, when it knows of such work: another
//! Thoth run of the same account overlapped in time, or the operator marked
//! the profile as shared with a human.

use crate::adapter::{HARNESS_CLAUDE_CODE, HARNESS_CODEX};
use crate::commands::usage;
use crate::config::Paths;
use crate::db::global;
use crate::db::state::{self, NewEvent};
use crate::model::event_type;
use anyhow::Result;
use rusqlite::Connection;
use std::collections::BTreeSet;
use std::path::Path;
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

/// The window the event's top-level numbers measure.
pub(crate) const WINDOW: &str = "five_hour";
/// The window of the nested `seven_day` object (DT-1).
pub(crate) const WEEK_WINDOW: &str = "seven_day";

/// Where a run's readings come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    /// The run's own `rate_limit_warning` events (Claude Code's stream).
    ClaudeStream,
    /// The `token_count` lines of the run's codex rollout file.
    CodexRollout,
}

impl Source {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Source::ClaudeStream => "claude_stream",
            Source::CodexRollout => "codex_rollout",
        }
    }
}

/// Which runs get a `run_quota` event: runs of a claude-code or codex
/// profile, whatever their role (a reviewer run since DT-1). Any other
/// harness gets none (antigravity has no such reading).
pub(crate) fn source_for(_run_role: &str, harness: &str) -> Option<Source> {
    match harness {
        HARNESS_CLAUDE_CODE => Some(Source::ClaudeStream),
        HARNESS_CODEX => Some(Source::CodexRollout),
        _ => None,
    }
}

/// One reading of the five-hour window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Reading {
    /// Percent used, 0-100.
    pub(crate) percent: f64,
    /// When the window resets, epoch seconds.
    pub(crate) resets_at: Option<i64>,
}

/// The three numbers of a `run_quota` event: all `Some` or all `None`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct Points {
    pub(crate) start_percent: Option<f64>,
    pub(crate) end_percent: Option<f64>,
    pub(crate) points: Option<f64>,
}

/// Rounded to two decimals, so `0.07 * 100.0` is stored as 7.0 and not
/// 7.000000000000001.
pub(crate) fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// How far apart (seconds) two readings' `resets_at` may be and still count
/// as the same window. Measured 2026-10-01 on six real codex-cli 0.154
/// rollout files: within one five-hour window the 300-minute window's
/// `resets_at` is normally identical on every line, but in the first run of a
/// fresh window it moved by 6 seconds between lines (1790843025, then
/// 1790843031) while `used_percent` went from 0 to 25. A real reset moves it
/// by about five hours (18000 s), far beyond this.
pub(crate) const SAME_WINDOW_DRIFT_SECS: i64 = 600;

/// Whether two readings' reset times name the same window: both absent, or
/// both present and at most `SAME_WINDOW_DRIFT_SECS` apart.
fn same_window(a: Option<i64>, b: Option<i64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.abs_diff(b) <= SAME_WINDOW_DRIFT_SECS as u64,
        _ => false,
    }
}

/// The pure part: from the first and the last reading of a run to its
/// start, end and points (end minus start, in percentage points), each
/// rounded to two decimals.
///
/// A run with one reading passes it as both ends, so the two ends agree and
/// the points are 0. Either end missing, or the two ends in different
/// windows (reset times more than `SAME_WINDOW_DRIFT_SECS` apart, or only
/// one of them known: the window reset mid-run), gives no numbers at all.
pub(crate) fn points(first: Option<Reading>, last: Option<Reading>) -> Points {
    match (first, last) {
        (Some(a), Some(b)) if same_window(a.resets_at, b.resets_at) => {
            let start = round2(a.percent);
            let end = round2(b.percent);
            Points {
                start_percent: Some(start),
                end_percent: Some(end),
                points: Some(round2(end - start)),
            }
        }
        _ => Points::default(),
    }
}

/// Claude: the entry named `window` (`five_hour` or `seven_day`) of
/// `windows` in each of this run's own `rate_limit_warning` events, in `seq`
/// order (a resumed run's earlier launch included), utilization as a percent.
/// An event without that entry is skipped.
pub(crate) fn claude_readings(
    conn: &Connection,
    run_id: &str,
    window: &str,
) -> Result<Vec<Reading>> {
    let mut stmt =
        conn.prepare("SELECT detail FROM events WHERE run_id = ?1 AND type = ?2 ORDER BY seq")?;
    let details = stmt
        .query_map([run_id, event_type::RATE_LIMIT_WARNING], |r| {
            r.get::<_, Option<String>>(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(details
        .into_iter()
        .flatten()
        .filter_map(|d| serde_json::from_str::<serde_json::Value>(&d).ok())
        .filter_map(|d| {
            let w = d
                .get("windows")?
                .as_array()?
                .iter()
                .find(|w| w.get("name").and_then(serde_json::Value::as_str) == Some(window))?;
            Some(Reading {
                percent: w.get("utilization")?.as_f64()? * 100.0,
                resets_at: w.get("resets_at").and_then(serde_json::Value::as_i64),
            })
        })
        .collect())
}

/// Codex: the rollout file under `<config_dir>/sessions` (searched
/// recursively) whose name contains `thread_id` - the newest one if several
/// do - and in it, every `token_count` line of the general bucket that has a
/// window named `window` in `primary` or `secondary` (300 minutes is
/// `five_hour`, 10080 is `seven_day`), in file order. Only that file is read;
/// nothing else in the profile directory is opened.
pub(crate) fn codex_readings(
    config_dir: &str,
    thread_id: &str,
    window: &str,
) -> Result<Vec<Reading>> {
    let mut files = Vec::new();
    usage::rollout_files(&Path::new(config_dir).join("sessions"), &mut files)?;
    files.retain(|(_, path)| {
        path.file_name()
            .is_some_and(|n| n.to_string_lossy().contains(thread_id))
    });
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    let Some((_, path)) = files.into_iter().next() else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&path)?;
    Ok(text
        .lines()
        .filter_map(usage::token_count_limits)
        .filter(|(_, limits)| usage::is_general_bucket(limits))
        .filter_map(|(observed, limits)| {
            // `codex_limit_windows` names a 300-minute window `five_hour` and
            // a 10080-minute one `seven_day`.
            usage::codex_limit_windows(&limits, &observed)
                .into_iter()
                .find(|w| w.name == window)
                .map(|w| Reading {
                    percent: w.used,
                    resets_at: w.resets_at,
                })
        })
        .collect())
}

/// The `run_quota` event's detail: the five original keys as S3-12 wrote
/// them, then `seven_day`, `reliable` and `unreliable_reasons` (DT-1).
fn detail(
    five_hour: Points,
    seven_day: Points,
    source: Source,
    unreliable_reasons: &[String],
) -> serde_json::Value {
    serde_json::json!({
        "window": WINDOW,
        "start_percent": five_hour.start_percent,
        "end_percent": five_hour.end_percent,
        "points": five_hour.points,
        "source": source.as_str(),
        "seven_day": {
            "start_percent": seven_day.start_percent,
            "end_percent": seven_day.end_percent,
            "points": seven_day.points,
        },
        "reliable": unreliable_reasons.is_empty(),
        "unreliable_reasons": unreliable_reasons,
    })
}

fn parse_time(s: &str) -> Option<OffsetDateTime> {
    OffsetDateTime::parse(s, &Rfc3339).ok()
}

/// The other Thoth runs of `account` in one repo's `runs` table that overlap
/// `[start, end]`, as run ids. A run that never started cannot overlap; one
/// without a parsable end is still running as far as anyone knows.
fn overlapping_runs(
    conn: &Connection,
    account: &str,
    run_id: &str,
    start: OffsetDateTime,
    end: OffsetDateTime,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT run_id, started_at, ended_at FROM runs
         WHERE account = ?1 AND run_id != ?2 AND started_at IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([account, run_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|(_, started, ended)| {
            let Some(other_start) = parse_time(started) else {
                return false;
            };
            let other_end = ended.as_deref().and_then(parse_time);
            other_start <= end && other_end.is_none_or(|e| e >= start)
        })
        .map(|(id, _, _)| id)
        .collect())
}

/// Why this run's numbers cannot be trusted as its own, in this order:
/// `shared` (or `shared-check-failed`), then `overlap:<run_id>` for every
/// other Thoth run of the same account - any repo, any role, any status - that
/// overlapped it in time, then `overlap-check-failed:<repo_id>` for every repo
/// that could not be read (this repo too when the run's own row could not be
/// re-read for its start time). Failing to check counts against the run: better
/// fewer points than points that belong to somebody else.
fn unreliable_reasons(
    conn: &Connection,
    gconn: &Connection,
    paths: &Paths,
    repo_id: &str,
    run: &state::Run,
    ended_at: &str,
) -> Vec<String> {
    let run_dir = Path::new(&run.run_dir);
    let mut reasons = Vec::new();
    match global::profile_shared(gconn, &run.account) {
        Ok(true) => reasons.push("shared".to_string()),
        Ok(false) => {}
        Err(e) => {
            crate::runner::log_runner_error(
                run_dir,
                &format!("reading the profile's shared mark failed: {e:#}"),
            );
            reasons.push("shared-check-failed".to_string());
        }
    }

    // The run's own interval; a missing start collapses it to the end.
    let Some(end) = parse_time(ended_at) else {
        reasons.push(format!("overlap-check-failed:{repo_id}"));
        return reasons;
    };
    let mut failed: BTreeSet<String> = BTreeSet::new();
    // A row that reads fine but has no start (or an unparsable one) is the
    // single point `end`; a row that cannot be read at all is a check that
    // could not be done, and says so.
    let start = match state::get_run(conn, &run.run_id) {
        Ok(row) => row
            .and_then(|r| r.started_at)
            .and_then(|s| parse_time(&s))
            .unwrap_or(end),
        Err(e) => {
            crate::runner::log_runner_error(
                run_dir,
                &format!("re-reading run {} for its start time failed: {e:#}", run.run_id),
            );
            failed.insert(repo_id.to_string());
            end
        }
    };

    // This repo is always checked, registered or not.
    let mut repos: BTreeSet<String> = BTreeSet::new();
    repos.insert(repo_id.to_string());
    match usage::registered_repos(gconn) {
        Ok(all) => repos.extend(all),
        Err(e) => {
            crate::runner::log_runner_error(
                run_dir,
                &format!("listing the registered repos failed: {e:#}"),
            );
            failed.insert(repo_id.to_string());
        }
    }
    let mut overlapping: BTreeSet<String> = BTreeSet::new();
    for repo in &repos {
        let found = if repo == repo_id {
            overlapping_runs(conn, &run.account, &run.run_id, start, end)
        } else {
            let db = paths.state_db(repo);
            // Only "does not exist" is a repo without runs; any other failure
            // to look at the file (permissions, a bad path) is a check that
            // could not be done.
            match std::fs::metadata(&db) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => Err(anyhow::Error::new(e)
                    .context(format!("looking at {}", db.display()))),
                // Read-only: another repo's state.db is never upgraded from here.
                Ok(_) => usage::read_only(&db)
                    .and_then(|c| overlapping_runs(&c, &run.account, &run.run_id, start, end)),
            }
        };
        match found {
            Ok(ids) => overlapping.extend(ids),
            Err(e) => {
                crate::runner::log_runner_error(
                    run_dir,
                    &format!("checking repo {repo} for overlapping runs failed: {e:#}"),
                );
                failed.insert(repo.clone());
            }
        }
    }
    reasons.extend(overlapping.into_iter().map(|id| format!("overlap:{id}")));
    reasons.extend(failed.into_iter().map(|r| format!("overlap-check-failed:{r}")));
    reasons
}

/// Writes this run's `run_quota` event when it should have one. Best effort:
/// a failed read leaves the numbers null, a failed write is dropped, and both
/// are logged to the run's `runner-error.log`; no status is touched.
///
/// `observed_session` is the session id the stream reported (codex's thread
/// id from `thread.started`); without one, the run row's stored
/// `harness_session_id` is used. `ended_at` is when the runner decided the
/// run is over; it bounds the interval the overlap check uses.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    conn: &Connection,
    gconn: &Connection,
    paths: &Paths,
    repo_id: &str,
    run: &state::Run,
    harness: &str,
    observed_session: Option<&str>,
    ended_at: &str,
) {
    let Some(source) = source_for(&run.role, harness) else {
        return;
    };
    let run_dir = Path::new(&run.run_dir);
    // The codex thread: the row as it is now, not `run`, because a fresh
    // run's pre-launch row has no session id yet.
    let thread = match source {
        Source::ClaudeStream => None,
        Source::CodexRollout => observed_session.map(str::to_string).or_else(|| {
            state::get_run(conn, &run.run_id)
                .ok()
                .flatten()
                .and_then(|r| r.harness_session_id)
        }),
    };
    let points_of = |window: &str| -> Points {
        let readings = match source {
            Source::ClaudeStream => claude_readings(conn, &run.run_id, window),
            Source::CodexRollout => match &thread {
                Some(t) => codex_readings(&run.config_dir, t, window),
                None => Ok(Vec::new()),
            },
        };
        let readings = readings.unwrap_or_else(|e| {
            crate::runner::log_runner_error(
                run_dir,
                &format!("reading the run's {window} quota readings failed: {e:#}"),
            );
            Vec::new()
        });
        points(readings.first().copied(), readings.last().copied())
    };
    let five_hour = points_of(WINDOW);
    let seven_day = points_of(WEEK_WINDOW);
    let reasons = unreliable_reasons(conn, gconn, paths, repo_id, run, ended_at);
    if let Err(e) = state::append_event(
        conn,
        &NewEvent {
            task_id: Some(run.task_id.clone()),
            run_id: Some(run.run_id.clone()),
            event_type: event_type::RUN_QUOTA.to_string(),
            detail: detail(five_hour, seven_day, source, &reasons),
        },
    ) {
        crate::runner::log_runner_error(run_dir, &format!("writing the run_quota event failed: {e:#}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RESET: i64 = 1_767_243_600;

    fn at(percent: f64, resets_at: i64) -> Reading {
        Reading {
            percent,
            resets_at: Some(resets_at),
        }
    }

    // AC4: the four cases of the pure function ------------------------------

    #[test]
    fn the_same_reset_time_from_10_to_13_is_3_points() {
        let p = points(Some(at(10.0, RESET)), Some(at(13.0, RESET)));
        assert_eq!(p.points, Some(3.0));
        assert_eq!(p.start_percent, Some(10.0));
        assert_eq!(p.end_percent, Some(13.0));
    }

    #[test]
    fn different_reset_times_give_no_numbers() {
        let p = points(Some(at(10.0, RESET)), Some(at(13.0, RESET + 18_000)));
        assert_eq!(p.start_percent, None);
        assert_eq!(p.end_percent, None);
        assert_eq!(p.points, None);
    }

    #[test]
    fn a_single_reading_is_both_ends_and_0_points() {
        let readings = [at(42.0, RESET)];
        let p = points(readings.first().copied(), readings.last().copied());
        assert_eq!(p.points, Some(0.0));
        assert_eq!(p.start_percent, Some(42.0));
        assert_eq!(p.end_percent, Some(42.0));
    }

    #[test]
    fn no_reading_gives_no_numbers() {
        let readings: [Reading; 0] = [];
        let p = points(readings.first().copied(), readings.last().copied());
        assert_eq!(p.start_percent, None);
        assert_eq!(p.end_percent, None);
        assert_eq!(p.points, None);
        // either end alone is just as unknown
        assert_eq!(points(Some(at(10.0, RESET)), None), Points::default());
        assert_eq!(points(None, Some(at(10.0, RESET))), Points::default());
    }

    #[test]
    fn a_6_second_reset_drift_at_the_start_of_a_window_is_the_same_window() {
        let p = points(Some(at(0.0, 1790843025)), Some(at(25.0, 1790843031)));
        assert_eq!(p.points, Some(25.0));
        assert_eq!(p.start_percent, Some(0.0));
        assert_eq!(p.end_percent, Some(25.0));
    }

    #[test]
    fn reset_times_exactly_600_seconds_apart_are_the_same_window() {
        let p = points(Some(at(10.0, RESET)), Some(at(13.0, RESET + 600)));
        assert_eq!(p.points, Some(3.0));
        assert_eq!(p.start_percent, Some(10.0));
        assert_eq!(p.end_percent, Some(13.0));
        // the order of the two does not matter
        let p = points(Some(at(10.0, RESET + 600)), Some(at(13.0, RESET)));
        assert_eq!(p.points, Some(3.0));
    }

    #[test]
    fn reset_times_601_seconds_apart_give_no_numbers() {
        let p = points(Some(at(10.0, RESET)), Some(at(13.0, RESET + 601)));
        assert_eq!(p.start_percent, None);
        assert_eq!(p.end_percent, None);
        assert_eq!(p.points, None);
    }

    #[test]
    fn a_reset_time_on_only_one_end_gives_no_numbers() {
        let without = Reading {
            percent: 13.0,
            resets_at: None,
        };
        let p = points(Some(at(10.0, RESET)), Some(without));
        assert_eq!(p.start_percent, None);
        assert_eq!(p.end_percent, None);
        assert_eq!(p.points, None);
        let p = points(Some(without), Some(at(10.0, RESET)));
        assert_eq!(p.start_percent, None);
        assert_eq!(p.end_percent, None);
        assert_eq!(p.points, None);
    }

    #[test]
    fn utilization_0_07_times_100_rounds_to_7() {
        let raw = 0.07 * 100.0;
        assert_ne!(raw, 7.0, "the float problem this rounding exists for");
        assert_eq!(round2(raw), 7.0);
        let p = points(Some(at(raw, RESET)), Some(at(raw, RESET)));
        assert_eq!(p.start_percent, Some(7.0));
        assert_eq!(p.end_percent, Some(7.0));
        assert_eq!(p.points, Some(0.0));
        // and a difference keeps two decimals
        assert_eq!(points(Some(at(7.01, RESET)), Some(at(7.12, RESET))).points, Some(0.11));
    }

    // AC2: which runs get one ------------------------------------------------

    #[test]
    fn claude_code_and_codex_runs_of_either_role_have_a_source() {
        assert_eq!(source_for("implementer", "claude-code"), Some(Source::ClaudeStream));
        assert_eq!(source_for("implementer", "codex"), Some(Source::CodexRollout));
        assert_eq!(source_for("reviewer", "claude-code"), Some(Source::ClaudeStream));
        assert_eq!(source_for("reviewer", "codex"), Some(Source::CodexRollout));
        assert_eq!(source_for("implementer", "antigravity"), None);
        assert_eq!(source_for("reviewer", "antigravity"), None);
        assert_eq!(Source::ClaudeStream.as_str(), "claude_stream");
        assert_eq!(Source::CodexRollout.as_str(), "codex_rollout");
    }

    #[test]
    fn the_detail_has_the_five_keys() {
        let d = detail(
            points(Some(at(10.0, RESET)), Some(at(13.0, RESET))),
            Points::default(),
            Source::CodexRollout,
            &[],
        );
        assert_eq!(
            d,
            serde_json::json!({"window": "five_hour", "start_percent": 10.0, "end_percent": 13.0,
                "points": 3.0, "source": "codex_rollout",
                "seven_day": {"start_percent": null, "end_percent": null, "points": null},
                "reliable": true, "unreliable_reasons": []})
        );
        let d = detail(
            Points::default(),
            points(Some(at(2.0, RESET)), Some(at(2.5, RESET))),
            Source::ClaudeStream,
            &["shared".to_string()],
        );
        assert_eq!(
            d,
            serde_json::json!({"window": "five_hour", "start_percent": null, "end_percent": null,
                "points": null, "source": "claude_stream",
                "seven_day": {"start_percent": 2.0, "end_percent": 2.5, "points": 0.5},
                "reliable": false, "unreliable_reasons": ["shared"]})
        );
    }

    // AC2: the Claude source ---------------------------------------------------

    #[test]
    fn claude_readings_are_this_runs_five_hour_windows_in_seq_order() {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        let add = |run: &str, ty: &str, detail: serde_json::Value| {
            conn.execute(
                "INSERT INTO events (ts, task_id, run_id, type, detail) VALUES ('t', 'T', ?1, ?2, ?3)",
                rusqlite::params![run, ty, detail.to_string()],
            )
            .unwrap();
        };
        let warning = |five: f64, seven: f64| {
            serde_json::json!({"rate_limit_type": "five_hour", "utilization": five, "resets_at": RESET,
                "windows": [{"name": "seven_day", "utilization": seven, "resets_at": RESET + 1},
                            {"name": "five_hour", "utilization": five, "resets_at": RESET}]})
        };
        add("R1", "rate_limit_warning", warning(0.10, 0.5));
        add("R2", "rate_limit_warning", warning(0.90, 0.5)); // another run
        add("R1", "run_usage", serde_json::json!({"windows": [{"name": "five_hour", "utilization": 0.5}]}));
        // no five_hour entry: skipped
        add("R1", "rate_limit_warning", serde_json::json!({"windows": [{"name": "seven_day", "utilization": 0.6}]}));
        add("R1", "rate_limit_warning", warning(0.13, 0.5));
        let r = claude_readings(&conn, "R1", "five_hour").unwrap();
        assert_eq!(r.len(), 2, "{r:?}");
        assert_eq!(round2(r[0].percent), 10.0);
        assert_eq!(round2(r[1].percent), 13.0);
        assert_eq!(r[0].resets_at, Some(RESET));
        let p = points(r.first().copied(), r.last().copied());
        assert_eq!(p.points, Some(3.0));
        assert!(claude_readings(&conn, "NONE", "five_hour").unwrap().is_empty());

        // DT-1 u6: the same events, read as the weekly window - the event
        // with only a seven_day entry counts this time.
        let w = claude_readings(&conn, "R1", "seven_day").unwrap();
        assert_eq!(w.len(), 3, "{w:?}");
        assert_eq!(round2(w[0].percent), 50.0);
        assert_eq!(round2(w[1].percent), 60.0);
        assert_eq!(round2(w[2].percent), 50.0);
        assert_eq!(points(w.first().copied(), w.last().copied()).points, Some(0.0));
    }

    // AC2: the codex source ----------------------------------------------------

    fn token_count(limits: serde_json::Value) -> String {
        serde_json::json!({"timestamp": "2026-01-01T00:00:00.000Z", "type": "event_msg",
            "payload": {"type": "token_count", "rate_limits": limits}})
        .to_string()
    }

    #[test]
    fn codex_readings_take_the_general_buckets_300_minute_window_from_the_threads_file() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = dir.path().to_str().unwrap();
        let nested = dir.path().join("sessions").join("2026").join("01").join("01");
        std::fs::create_dir_all(&nested).unwrap();
        let thread = "0199aaaa-bbbb-7ccc-8ddd-eeeeffff0000";
        let lines = [
            r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"x"}}"#.to_string(),
            "not json".to_string(),
            token_count(serde_json::json!({"limit_id": "codex",
                "primary": {"used_percent": 10.0, "window_minutes": 300, "resets_at": RESET},
                "secondary": {"used_percent": 4.0, "window_minutes": 10080, "resets_at": RESET + 1}})),
            // a reserve bucket's 300-minute window is not the account's
            token_count(serde_json::json!({"limit_id": "base_model_inference",
                "primary": {"used_percent": 77.0, "window_minutes": 300, "resets_at": RESET}})),
            // older codex: no limit_id; the 300-minute window in `secondary`
            token_count(serde_json::json!({
                "primary": {"used_percent": 5.0, "window_minutes": 10080, "resets_at": RESET + 1},
                "secondary": {"used_percent": 13.0, "window_minutes": 300, "resets_at": RESET}})),
            token_count(serde_json::Value::Null),
        ];
        std::fs::write(
            nested.join(format!("rollout-2026-01-01T00-00-00-{thread}.jsonl")),
            lines.join("\n"),
        )
        .unwrap();
        // another thread's file is never read
        std::fs::write(
            nested.join("rollout-2026-01-01T00-00-00-other.jsonl"),
            token_count(serde_json::json!({"limit_id": "codex",
                "primary": {"used_percent": 99.0, "window_minutes": 300, "resets_at": RESET}})),
        )
        .unwrap();

        let r = codex_readings(cfg, thread, "five_hour").unwrap();
        assert_eq!(r, vec![at(10.0, RESET), at(13.0, RESET)]);
        assert_eq!(points(r.first().copied(), r.last().copied()).points, Some(3.0));

        // DT-1 u6: the weekly window of the same file; the first line has it
        // in `secondary`, the third in `primary`, the reserve bucket's line
        // is still skipped.
        let w = codex_readings(cfg, thread, "seven_day").unwrap();
        assert_eq!(w, vec![at(4.0, RESET + 1), at(5.0, RESET + 1)]);
        assert_eq!(points(w.first().copied(), w.last().copied()).points, Some(1.0));

        assert!(codex_readings(cfg, "no-such-thread", "five_hour").unwrap().is_empty());
        let empty = tempfile::tempdir().unwrap();
        assert!(codex_readings(empty.path().to_str().unwrap(), thread, "five_hour").unwrap().is_empty());
    }

    // DT-1 AC6: the overlap check, its interval and its error paths ---------

    const T0: &str = "2000-01-01T00:00:00Z";

    fn a_run(run_id: &str, run_dir: &Path, started_at: Option<&str>) -> state::Run {
        state::Run {
            run_id: run_id.into(),
            task_id: "T1".into(),
            attempt: 1,
            role: "implementer".into(),
            provider: "anthropic".into(),
            account: "claude-b".into(),
            model: "sonnet".into(),
            harness_session_id: None,
            config_dir: String::new(),
            allowed_tools: String::new(),
            worktree: String::new(),
            base_commit: "abc".into(),
            candidate_commit: None,
            spec: "{}".into(),
            status: "completed".into(),
            interrupted_reason: None,
            exit_code: None,
            input_tokens: None,
            output_tokens: None,
            started_at: started_at.map(str::to_string),
            ended_at: None,
            runner_pid: None,
            worker_pid: None,
            heartbeat_at: None,
            cancel_requested: 0,
            timeout_seconds: 0,
            run_dir: run_dir.to_string_lossy().into_owned(),
            error_text: None,
            harness_bin: None,
        }
    }

    /// A migrated in-memory state.db holding task T1.
    fn state_with_task() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_state(&conn).unwrap();
        state::insert_task(
            &conn,
            &state::Task {
                task_id: "T1".into(),
                title: "t".into(),
                spec: "{}".into(),
                risk: "normal".into(),
                status: "completed".into(),
                base_commit: "abc".into(),
                created_at: T0.into(),
                updated_at: T0.into(),
            },
        )
        .unwrap();
        conn
    }

    fn global_with_repo(repo_id: &str) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        crate::db::migrations::migrate_global(&conn).unwrap();
        global::upsert_repo(&conn, repo_id, "/somewhere", T0).unwrap();
        conn
    }

    #[test]
    fn the_runs_own_interval_runs_from_its_started_at_to_the_end() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(dir.path().to_path_buf());
        let conn = state_with_task();
        let mine = a_run("R1", &dir.path().join("run"), Some(T0));
        state::insert_run(&conn, &mine).unwrap();
        let mut mid = a_run("MID", &dir.path().join("mid"), Some("2000-01-01T12:00:00Z"));
        mid.ended_at = Some("2000-01-01T13:00:00Z".into());
        state::insert_run(&conn, &mid).unwrap();
        let mut late = a_run("LATE", &dir.path().join("late"), Some("2000-01-04T00:00:00Z"));
        late.ended_at = Some("2000-01-05T00:00:00Z".into());
        state::insert_run(&conn, &late).unwrap();
        let never = a_run("NEVER", &dir.path().join("never"), None);
        state::insert_run(&conn, &never).unwrap();
        let gconn = global_with_repo("REPO");
        let reasons = unreliable_reasons(&conn, &gconn, &paths, "REPO", &mine, "2000-01-03T00:00:00Z");
        assert_eq!(reasons, vec!["overlap:MID".to_string()]);
    }

    #[test]
    fn a_failing_reread_of_the_run_row_is_a_failed_check_not_a_single_point() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("run");
        let paths = Paths::from_root(dir.path().to_path_buf());
        // A `runs` table with the columns the overlap query reads but not the
        // ones `state::get_run` selects: the re-read fails, the query works.
        // OLD ended before the run's end but overlaps its real start, and the
        // run's end alone (the single point the old code fell back to) does
        // not touch it.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (run_id TEXT, account TEXT, started_at TEXT, ended_at TEXT);
             INSERT INTO runs VALUES ('R1', 'claude-b', '2000-01-01T00:00:00Z', '2000-01-03T00:00:00Z');
             INSERT INTO runs VALUES ('OLD', 'claude-b', '2000-01-01T00:00:00Z', '2000-01-02T00:00:00Z');",
        )
        .unwrap();
        assert!(state::get_run(&conn, "R1").is_err());
        let mine = a_run("R1", &run_dir, Some(T0));
        let gconn = global_with_repo("REPO");
        let reasons = unreliable_reasons(&conn, &gconn, &paths, "REPO", &mine, "2000-01-03T00:00:00Z");
        assert_eq!(reasons, vec!["overlap-check-failed:REPO".to_string()]);
        let log = std::fs::read_to_string(run_dir.join("runner-error.log")).unwrap();
        assert!(log.contains("re-reading run R1"), "{log}");
    }

    #[test]
    fn another_repos_state_db_that_cannot_be_opened_fails_the_check() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("run");
        let paths = Paths::from_root(dir.path().to_path_buf());
        let gconn = global_with_repo("OTHER");
        // a directory where the state.db file should be
        std::fs::create_dir_all(paths.state_db("OTHER")).unwrap();
        let conn = state_with_task();
        let mine = a_run("R1", &run_dir, Some(T0));
        state::insert_run(&conn, &mine).unwrap();
        let reasons = unreliable_reasons(&conn, &gconn, &paths, "REPO", &mine, "2000-01-03T00:00:00Z");
        assert_eq!(reasons, vec!["overlap-check-failed:OTHER".to_string()]);
        assert!(run_dir.join("runner-error.log").exists());
    }

    #[test]
    fn a_registered_repo_whose_state_db_does_not_exist_is_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("run");
        let paths = Paths::from_root(dir.path().to_path_buf());
        let gconn = global_with_repo("GONE");
        let conn = state_with_task();
        let mine = a_run("R1", &run_dir, Some(T0));
        state::insert_run(&conn, &mine).unwrap();
        let reasons = unreliable_reasons(&conn, &gconn, &paths, "REPO", &mine, "2000-01-03T00:00:00Z");
        assert!(reasons.is_empty(), "{reasons:?}");
    }

    /// The state.db is there, but something other than "not found" stops
    /// `metadata` from seeing it (here: the repo directory cannot be searched).
    /// That is a check that could not be done, not a repo without runs.
    #[cfg(unix)]
    #[test]
    fn another_repos_state_db_that_cannot_be_looked_at_fails_the_check() {
        use std::os::unix::fs::PermissionsExt;
        struct Restore(std::path::PathBuf);
        impl Drop for Restore {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let run_dir = dir.path().join("run");
        let paths = Paths::from_root(dir.path().to_path_buf());
        let gconn = global_with_repo("OTHER");
        std::fs::create_dir_all(paths.repo_dir("OTHER")).unwrap();
        {
            let other = Connection::open(paths.state_db("OTHER")).unwrap();
            crate::db::migrations::migrate_state(&other).unwrap();
        }
        let _restore = Restore(paths.repo_dir("OTHER"));
        std::fs::set_permissions(paths.repo_dir("OTHER"), std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::metadata(paths.state_db("OTHER")).is_ok() {
            // permissions are not enforced for this user (root): nothing to test
            return;
        }
        let conn = state_with_task();
        let mine = a_run("R1", &run_dir, Some(T0));
        state::insert_run(&conn, &mine).unwrap();
        let reasons = unreliable_reasons(&conn, &gconn, &paths, "REPO", &mine, "2000-01-03T00:00:00Z");
        assert_eq!(reasons, vec!["overlap-check-failed:OTHER".to_string()]);
    }
}
