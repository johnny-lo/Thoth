//! SMALL-9a: the quota reserve the user sets (`quota-reserve-percent`). With
//! N above 0, an account whose deciding quota window has used at least 100
//! minus N percent and has not reset is marked by `recommend`
//! (`reserve_blocked`) and refused by every launch command. Both read the
//! stored readings through `usage::ranked_candidates` and decide with
//! `blocking_window`, so they agree; nothing here refreshes quota. Thoth
//! still picks no account (RJ-1): the human sets the number, and only a
//! missing reading lets a launch through, with a note.

use crate::commands::usage::{self, RankedCandidate, Window};
use anyhow::{bail, Result};

/// The deciding window that holds an account back.
#[derive(Clone, Debug)]
pub(crate) struct Block {
    pub(crate) percent: u64,
    /// The window name, with the quota group for antigravity.
    pub(crate) label: String,
    pub(crate) used: f64,
    pub(crate) observed_at: String,
    pub(crate) resets_at: Option<String>,
}

impl Block {
    pub(crate) fn threshold(&self) -> u64 {
        100 - self.percent
    }

    /// The candidate's `reasons` entry.
    pub(crate) fn reason(&self) -> String {
        format!(
            "held back by quota-reserve-percent {}: {} is {}% used (observed at {}), at least {}% and not reset",
            self.percent,
            self.label,
            self.used,
            self.observed_at,
            self.threshold()
        )
    }

    /// A launch's refusal.
    fn refusal(&self, profile_id: &str, role: &str, task_id: &str) -> String {
        let resets = self.resets_at.as_deref().unwrap_or("unknown");
        format!(
            "refusing to launch on profile {profile_id}: quota-reserve-percent {} holds it back, because its {} window is {}% used (observed at {}, resets at {resets}), at or above the threshold of {}%; nothing was written. Use another account (see `thoth recommend --role {role} --task {task_id}`), wait for the window to reset (at {resets}), run `thoth profile usage --refresh` when the reading may be old, or ask the human to run `thoth config set quota-reserve-percent` with a lower number (0 turns it off)",
            self.percent,
            self.label,
            self.used,
            self.observed_at,
            self.threshold()
        )
    }
}

/// The fullest of the deciding windows `decision` that is at or above
/// 100 minus `percent` and has not reset (an unknown reset time counts as not
/// reset, as for `exhausted`), whatever the reading's age; `None` when
/// `percent` is 0 or no window is that full.
pub(crate) fn blocking_window(decision: &[Window], percent: u64) -> Option<Block> {
    if percent == 0 {
        return None;
    }
    let threshold = 100u64.saturating_sub(percent) as f64;
    let w = decision
        .iter()
        .filter(|w| w.used >= threshold && !w.is_reset())
        .fold(None::<&Window>, |best, w| match best {
            Some(b) if b.used >= w.used => Some(b),
            _ => Some(w),
        })?;
    Some(Block {
        percent,
        label: w.group.as_ref().map(|g| format!("{} ({g})", w.name)).unwrap_or_else(|| w.name.clone()),
        used: w.used.clamp(0.0, 100.0),
        observed_at: usage::normalize_rfc(&w.observed_at),
        resets_at: w.resets_at.and_then(usage::rfc_from_epoch),
    })
}

/// The reserve check of one launch on `profile_id`, given the ranking the
/// launch already read for its task and model: `Err` is the refusal, which
/// the caller returns before it writes anything; `Ok(Some(note))` lets the
/// launch go ahead with a warning, because the deciding windows are empty
/// (no reading, or every window already reset) or the facts could not be
/// read; `Ok(None)` when the reserve is off or the account is below it.
pub(crate) fn launch_gate(
    percent: u64,
    profile_id: &str,
    role: &str,
    task_id: &str,
    ranked: Result<&[RankedCandidate], &anyhow::Error>,
) -> Result<Option<String>> {
    if percent == 0 {
        return Ok(None);
    }
    let candidates = match ranked {
        Ok(c) => c,
        Err(e) => {
            return Ok(Some(not_held_back(
                percent,
                profile_id,
                &format!("its quota facts could not be read ({e:#})"),
            )))
        }
    };
    let Some(own) = candidates.iter().find(|c| c.profile.profile_id == profile_id) else {
        return Ok(Some(not_held_back(percent, profile_id, "it has no quota reading")));
    };
    if let Some(block) = blocking_window(&own.decision, percent) {
        bail!("{}", block.refusal(profile_id, role, task_id));
    }
    if own.decision.iter().all(Window::is_reset) {
        return Ok(Some(not_held_back(
            percent,
            profile_id,
            "it has no current quota reading (none yet, or every window has reset since it was read)",
        )));
    }
    Ok(None)
}

fn not_held_back(percent: u64, profile_id: &str, why: &str) -> String {
    format!(
        "quota-reserve-percent {percent}: profile {profile_id} was not held back because {why}; run `thoth profile usage --refresh` to read its quota"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Paths;

    // The other reserve scenarios are CLI tests in tests/quota_reserve.rs;
    // only an unreadable ranking cannot be built there without opening the
    // database.
    #[test]
    fn unreadable_quota_facts_let_the_launch_go_ahead_with_the_note() {
        // A state directory with no global database: the ranking's read of
        // the quota facts fails for real.
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(dir.path().join("missing-home"));
        let ranked = crate::commands::usage::ranked_candidates(&paths, "implementer", Some("T1"), Some("m"));
        assert!(ranked.is_err());
        let note = launch_gate(5, "codex-q", "implementer", "T1", ranked.as_deref())
            .expect("not refused")
            .expect("a note");
        for part in ["quota-reserve-percent 5", "codex-q", "not held back", "thoth profile usage --refresh"] {
            assert!(note.contains(part), "{part}: {note}");
        }
        assert!(launch_gate(0, "codex-q", "implementer", "T1", ranked.as_deref()).unwrap().is_none());
    }
}
