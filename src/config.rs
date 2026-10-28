use anyhow::{anyhow, Result};
use std::path::PathBuf;

/// Layout of the Thoth state directory (`THOTH_HOME`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: PathBuf,
}

impl Paths {
    /// Resolve the state directory: `THOTH_HOME`, else `~/.thoth`. Thoth was
    /// renamed from agentctl on 2026-09-30 and never reads or moves the old
    /// `AGENTCTL_HOME` / `~/.agentctl`; it refuses and prints how to move it.
    pub fn from_env() -> Result<Paths> {
        if let Some(v) = std::env::var_os("THOTH_HOME") {
            return Ok(Paths::from_root(PathBuf::from(v)));
        }
        if std::env::var_os("AGENTCTL_HOME").is_some() {
            return Err(anyhow!(
                "AGENTCTL_HOME is set but THOTH_HOME is not: the command was renamed from agentctl to thoth, and the state directory variable is now THOTH_HOME. Set THOTH_HOME to the same directory (or a new one) and unset AGENTCTL_HOME."
            ));
        }
        let probe = probe_state_dir()?;
        let new_home = probe.home;
        if let Some(old_home) = probe.old_dir_only {
            let (old, new) = (old_home.display(), new_home.display());
            let mut lines = vec![
                format!("found the old state directory {old} but not {new}."),
                "Thoth was renamed from agentctl; it does not move the old directory and does not read it."
                    .to_string(),
                "The state database and the git worktrees record absolute paths under the old directory (evidence blob_path, runs.worktree, and each main repo's .git/worktrees/*/gitdir), so moving the directory makes old evidence unreadable unless the old path still resolves.".to_string(),
                "Before any of the three ways below, first finish or cancel any running run with the old `agentctl` (the new `thoth cancel` cannot stop a run started by the old executable).".to_string(),
                "Pick one of three ways:".to_string(),
                "[Way 1, recommended for an existing install] Do not move anything; point THOTH_HOME at the old directory. Every recorded path stays valid.".to_string(),
            ];
            #[cfg(windows)]
            lines.extend([
                format!("setx THOTH_HOME \"{old}\""),
                "Open a new window.".to_string(),
            ]);
            #[cfg(not(windows))]
            lines.extend([
                format!("export THOTH_HOME=\"{old}\""),
                "Put it in your shell profile.".to_string(),
            ]);
            lines.push("[Way 2, move] Move the directory and leave a link at the old path so recorded paths still resolve.".to_string());
            #[cfg(windows)]
            lines.extend([
                format!("Move-Item \"{old}\" \"{new}\""),
                format!("New-Item -ItemType Junction -Path \"{old}\" -Target \"{new}\""),
            ]);
            #[cfg(not(windows))]
            lines.extend([
                format!("mv \"{old}\" \"{new}\""),
                format!("ln -s \"{new}\" \"{old}\""),
            ]);
            lines.extend([
                "Afterwards run `thoth profile list`; if a profile's config_dir is wrong, run `thoth profile set-config-dir ID NEW_DIR`. Without the link, old evidence cannot be read and old worktrees need `git worktree repair` from you.".to_string(),
                format!("[Way 3, start fresh] Create an empty {new} yourself, or set THOTH_HOME to a new folder. Thoth creates and changes nothing here."),
            ]);
            return Err(anyhow!(lines.join("\n")));
        }
        Ok(Paths::from_root(new_home))
    }

    pub fn from_root(home: PathBuf) -> Paths {
        Paths { home }
    }

    pub fn global_db(&self) -> PathBuf {
        self.home.join("global.db")
    }

    pub fn repo_dir(&self, repo_id: &str) -> PathBuf {
        self.home.join("repos").join(repo_id)
    }

    pub fn state_db(&self, repo_id: &str) -> PathBuf {
        self.repo_dir(repo_id).join("state.db")
    }

    pub fn worktrees_dir(&self, repo_id: &str) -> PathBuf {
        self.repo_dir(repo_id).join("worktrees")
    }

    pub fn blobs_dir(&self, repo_id: &str) -> PathBuf {
        self.repo_dir(repo_id).join("blobs")
    }

    pub fn run_dir(&self, repo_id: &str, run_id: &str) -> PathBuf {
        self.repo_dir(repo_id).join("runs").join(run_id)
    }
}

/// What `Paths::from_env` would decide, without refusing: `thoth doctor`
/// reports the old-directory and old-variable cases instead of stopping on
/// them. `home` is where the state directory is (or would be).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateDirProbe {
    pub home: PathBuf,
    /// `"env"` (THOTH_HOME) or `"default"` (`~/.thoth`).
    pub source: &'static str,
    pub exists: bool,
    /// `~/.agentctl` when it exists and `~/.thoth` does not.
    pub old_dir_only: Option<PathBuf>,
    /// The value of AGENTCTL_HOME when it is set and THOTH_HOME is not.
    pub agentctl_var_only: Option<String>,
}

/// The user's home directory: `USERPROFILE` when set and non-empty, else
/// `HOME` when set and non-empty, else `None`.
pub fn home_dir_from(get: &dyn Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    ["USERPROFILE", "HOME"]
        .iter()
        .find_map(|k| get(k).filter(|v| !v.is_empty()))
        .map(PathBuf::from)
}

/// The rules of `Paths::from_env`, with its two refusals turned into fields.
/// Errs only when THOTH_HOME is unset and neither USERPROFILE nor HOME is set to a non-empty value.
pub fn probe_state_dir() -> Result<StateDirProbe> {
    if let Some(v) = std::env::var_os("THOTH_HOME") {
        let home = PathBuf::from(v);
        return Ok(StateDirProbe {
            exists: home.exists(),
            home,
            source: "env",
            old_dir_only: None,
            agentctl_var_only: None,
        });
    }
    let agentctl_var_only =
        std::env::var_os("AGENTCTL_HOME").map(|v| v.to_string_lossy().to_string());
    let profile = home_dir_from(&|k| std::env::var_os(k))
        .ok_or_else(|| anyhow!("neither THOTH_HOME nor USERPROFILE/HOME is set"))?;
    let new_home = profile.join(".thoth");
    let old_home = profile.join(".agentctl");
    // As in `from_env`, the old variable is decided before the old directory.
    let old_dir_only = (agentctl_var_only.is_none() && !new_home.exists() && old_home.exists())
        .then_some(old_home);
    Ok(StateDirProbe {
        exists: new_home.exists(),
        home: new_home,
        source: "default",
        old_dir_only,
        agentctl_var_only,
    })
}

/// The harness executable to launch. Overridable so tests can point at the fake.
pub fn claude_bin() -> String {
    crate::commands::settings::effective("claude-bin")
}

/// The codex executable, for profiles with harness `codex`. Overridable so
/// tests can point at `fake-codex`.
pub fn codex_bin() -> String {
    crate::commands::settings::effective("codex-bin")
}

/// Canonicalize a path and strip Windows' `\\?\` verbatim prefix, so every path
/// Thoth stores or exports looks like the one the user typed and the one other
/// tools report. Used by `ids::repo_id_for` and by `profile add`.
pub fn canonical_display(p: &std::path::Path) -> Result<String> {
    let canon = std::fs::canonicalize(p)
        .map_err(|e| anyhow!("cannot canonicalize {}: {e}", p.display()))?;
    let s = canon.to_string_lossy().to_string();
    Ok(s.strip_prefix(r"\\?\").unwrap_or(&s).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_dir_from_skips_empty_values() {
        let env = |up: Option<&'static str>, home: Option<&'static str>| {
            move |k: &str| match k {
                "USERPROFILE" => up.map(std::ffi::OsString::from),
                "HOME" => home.map(std::ffi::OsString::from),
                _ => None,
            }
        };
        assert_eq!(home_dir_from(&env(Some("/up"), Some("/home"))), Some(PathBuf::from("/up")));
        assert_eq!(home_dir_from(&env(Some(""), Some("/home"))), Some(PathBuf::from("/home")));
        assert_eq!(home_dir_from(&env(None, Some(""))), None);
        assert_eq!(home_dir_from(&env(None, None)), None);
    }
    #[test]
    fn from_root() {
        let p = Paths::from_root(std::path::PathBuf::from("C:/tmp/ah"));
        assert_eq!(p.global_db(), std::path::PathBuf::from("C:/tmp/ah/global.db"));
        assert_eq!(p.repo_dir("abc123"), std::path::PathBuf::from("C:/tmp/ah/repos/abc123"));
        assert_eq!(p.state_db("abc123"), std::path::PathBuf::from("C:/tmp/ah/repos/abc123/state.db"));
        assert_eq!(p.worktrees_dir("abc123"), std::path::PathBuf::from("C:/tmp/ah/repos/abc123/worktrees"));
        assert_eq!(p.blobs_dir("abc123"), std::path::PathBuf::from("C:/tmp/ah/repos/abc123/blobs"));
        assert_eq!(
            p.run_dir("abc123", "01RUN"),
            std::path::PathBuf::from("C:/tmp/ah/repos/abc123/runs/01RUN")
        );
    }

    #[test]
    fn claude_bin_defaults_to_claude() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        // THOTH_CLAUDE_BIN is not set in a clean test process.
        std::env::remove_var("THOTH_CLAUDE_BIN");
        // An empty state directory, so a developer's config.json cannot answer.
        let empty = tempfile::tempdir().unwrap();
        let prev = std::env::var_os("THOTH_HOME");
        std::env::set_var("THOTH_HOME", empty.path());
        assert_eq!(claude_bin(), "claude");
        match prev {
            Some(v) => std::env::set_var("THOTH_HOME", v),
            None => std::env::remove_var("THOTH_HOME"),
        }
    }

    /// Sets/clears the state-dir variables and both home variables, runs `f`,
    /// then restores them. Callers hold the shared environment lock.
    fn with_env(thoth: Option<&std::path::Path>, agentctl: Option<&str>, home: &std::path::Path, f: impl FnOnce()) {
        let keys = ["THOTH_HOME", "AGENTCTL_HOME", "USERPROFILE", "HOME"];
        let prev: Vec<_> = keys.iter().map(std::env::var_os).collect();
        match thoth {
            Some(v) => std::env::set_var("THOTH_HOME", v),
            None => std::env::remove_var("THOTH_HOME"),
        }
        match agentctl {
            Some(v) => std::env::set_var("AGENTCTL_HOME", v),
            None => std::env::remove_var("AGENTCTL_HOME"),
        }
        std::env::set_var("USERPROFILE", home);
        std::env::set_var("HOME", home);
        f();
        for (k, v) in keys.iter().zip(prev) {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn from_env_reads_thoth_home() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        with_env(Some(dir.path()), None, home.path(), || {
            assert_eq!(Paths::from_env().unwrap().home, dir.path());
        });
    }

    #[test]
    fn from_env_refuses_the_old_variable_alone() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        with_env(None, Some("/somewhere"), home.path(), || {
            let e = Paths::from_env().unwrap_err().to_string();
            assert!(e.contains("THOTH_HOME") && e.contains("AGENTCTL_HOME"), "{e}");
        });
    }

    #[test]
    fn from_env_refuses_an_old_directory_without_a_new_one() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::create_dir(home.path().join(".agentctl")).unwrap();
        with_env(None, None, home.path(), || {
            let e = Paths::from_env().unwrap_err().to_string();
            for want in ["git worktree repair", "three ways", "old `agentctl`"] {
                assert!(e.contains(want), "lacks {want}: {e}");
            }
            #[cfg(windows)]
            for want in ["setx THOTH_HOME", "Move-Item", "New-Item -ItemType Junction"] {
                assert!(e.contains(want), "lacks {want}: {e}");
            }
            #[cfg(not(windows))]
            for want in ["export THOTH_HOME=", "mv ", "ln -s "] {
                assert!(e.contains(want), "lacks {want}: {e}");
            }
        });
        assert!(!home.path().join(".thoth").exists());
    }

    #[test]
    fn from_env_prefers_the_new_directory_and_allows_a_fresh_machine() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        with_env(None, None, home.path(), || {
            assert_eq!(Paths::from_env().unwrap().home, home.path().join(".thoth"));
        });
        std::fs::create_dir(home.path().join(".agentctl")).unwrap();
        std::fs::create_dir(home.path().join(".thoth")).unwrap();
        with_env(None, None, home.path(), || {
            assert_eq!(Paths::from_env().unwrap().home, home.path().join(".thoth"));
        });
    }

    #[test]
    fn probe_state_dir_reports_instead_of_refusing() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let dir = tempfile::tempdir().unwrap();
        with_env(Some(dir.path()), None, home.path(), || {
            let p = probe_state_dir().unwrap();
            assert_eq!((p.home.as_path(), p.source, p.exists), (dir.path(), "env", true));
            assert_eq!((p.old_dir_only, p.agentctl_var_only), (None, None));
        });
        with_env(None, None, home.path(), || {
            let p = probe_state_dir().unwrap();
            assert_eq!((p.home, p.source, p.exists), (home.path().join(".thoth"), "default", false));
            assert_eq!(p.old_dir_only, None);
        });
        with_env(None, Some("/somewhere"), home.path(), || {
            let p = probe_state_dir().unwrap();
            assert_eq!(p.agentctl_var_only.as_deref(), Some("/somewhere"));
            assert_eq!(p.home, home.path().join(".thoth"));
        });
        std::fs::create_dir(home.path().join(".agentctl")).unwrap();
        with_env(None, None, home.path(), || {
            let p = probe_state_dir().unwrap();
            assert_eq!(p.old_dir_only, Some(home.path().join(".agentctl")));
            assert_eq!(p.home, home.path().join(".thoth"));
        });
        assert!(!home.path().join(".thoth").exists());
    }

    #[test]
    fn canonical_display_strips_the_windows_verbatim_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let s = canonical_display(dir.path()).unwrap();
        assert!(!s.starts_with(r"\\?\"), "verbatim prefix leaked: {s}");
        assert!(
            std::path::Path::new(&s).is_dir(),
            "the stripped path must still resolve: {s}"
        );
    }

    #[test]
    fn canonical_display_rejects_a_path_that_does_not_exist() {
        assert!(canonical_display(std::path::Path::new("C:/definitely/not/here")).is_err());
    }
}
