//! Compares an installed Thoth plugin folder with this executable.
//!
//! Reads two small files under the folder; opens no database, reads no
//! settings, uses no network and starts no process.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};

pub struct Mismatch {
    pub plugin_root: PathBuf,
    pub expected_version: String,
    pub installed_version: Option<String>,
    pub differences: Vec<&'static str>,
}

const FIX: &str = "To update the plugin, run: claude plugin marketplace update thoth, then claude plugin update thoth@thoth (if its version did not change: claude plugin uninstall thoth@thoth, then claude plugin install thoth@thoth), then start a new Claude Code session. If the plugin is the newer one, rebuild and reinstall thoth instead; thoth doctor shows both versions.";

fn manifest_path(root: &Path) -> PathBuf {
    root.join(".claude-plugin").join("plugin.json")
}

fn skill_path(root: &Path) -> PathBuf {
    root.join("skills").join("thoth-dispatch").join("SKILL.md")
}

/// `None` when the folder matches this executable.
pub fn compare(root: &Path) -> Option<Mismatch> {
    let expected = env!("CARGO_PKG_VERSION");
    let mut differences = Vec::new();
    let installed_version = fs::read(manifest_path(root))
        .ok()
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v.as_object()?.get("version")?.as_str().map(str::to_owned));
    match installed_version.as_deref() {
        None => differences.push("manifest_missing"),
        Some(v) if v != expected => differences.push("version"),
        Some(_) => {}
    }
    match fs::read(skill_path(root)) {
        Err(_) => differences.push("skill_missing"),
        Ok(bytes) => {
            let normalize = |b: &[u8]| {
                let mut out = Vec::with_capacity(b.len());
                let mut i = 0;
                while i < b.len() {
                    if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                        i += 1;
                        continue;
                    }
                    out.push(b[i]);
                    i += 1;
                }
                out
            };
            if normalize(&bytes) != normalize(crate::guide::source().as_bytes()) {
                differences.push("skill");
            }
        }
    }
    if differences.is_empty() {
        return None;
    }
    Some(Mismatch {
        plugin_root: root.to_path_buf(),
        expected_version: expected.to_owned(),
        installed_version,
        differences,
    })
}

impl Mismatch {
    fn head(&self) -> String {
        let diffs = self
            .differences
            .iter()
            .map(|d| match *d {
                "version" => format!(
                    "plugin version {}, executable version {}",
                    self.installed_version.as_deref().unwrap_or(""),
                    self.expected_version
                ),
                "skill" => format!(
                    "skill file differs from the one built into the executable: {}",
                    skill_path(&self.plugin_root).display()
                ),
                "skill_missing" => {
                    format!("skill file missing: {}", skill_path(&self.plugin_root).display())
                }
                _ => format!(
                    "plugin manifest missing or unreadable: {}",
                    manifest_path(&self.plugin_root).display()
                ),
            })
            .collect::<Vec<_>>()
            .join("; ");
        format!(
            "the installed Thoth plugin at {} does not match this thoth executable ({diffs}).",
            self.plugin_root.display()
        )
    }

    pub fn warning_line(&self) -> String {
        format!(
            "warning: {} Tools that change state are refused until they match; read-only tools still work. {FIX}",
            self.head()
        )
    }

    pub fn refusal_line(&self) -> String {
        format!("refused: {} {FIX}", self.head())
    }

    pub fn to_json(&self) -> Value {
        json!({
            "reason": "plugin_mismatch",
            "plugin_root": self.plugin_root,
            "expected_version": self.expected_version,
            "installed_version": self.installed_version,
            "differences": self.differences,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REPO_PLUGIN: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/plugins/thoth");

    fn write(root: &Path, rel: &str, content: &[u8]) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    #[test]
    fn compare_accepts_the_repo_plugin_and_a_crlf_copy() {
        assert!(compare(Path::new(REPO_PLUGIN)).is_none());
        let dir = tempfile::tempdir().unwrap();
        let repo = Path::new(REPO_PLUGIN);
        let manifest = fs::read(repo.join(".claude-plugin/plugin.json")).unwrap();
        write(dir.path(), ".claude-plugin/plugin.json", &manifest);
        let skill = fs::read_to_string(repo.join("skills/thoth-dispatch/SKILL.md")).unwrap();
        let crlf = skill.replace("\r\n", "\n").replace('\n', "\r\n");
        write(dir.path(), "skills/thoth-dispatch/SKILL.md", crlf.as_bytes());
        assert!(compare(dir.path()).is_none());
    }

    #[test]
    fn compare_lists_differences_in_a_fixed_order() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".claude-plugin/plugin.json",
            br#"{"name":"thoth","version":"0.0.1"}"#,
        );
        let mut skill = crate::guide::source().to_owned();
        skill.push_str("extra\n");
        write(dir.path(), "skills/thoth-dispatch/SKILL.md", skill.as_bytes());
        let m = compare(dir.path()).unwrap();
        assert_eq!(m.differences, ["version", "skill"]);
        assert_eq!(m.installed_version.as_deref(), Some("0.0.1"));
        assert_eq!(m.expected_version, env!("CARGO_PKG_VERSION"));
        let refusal = m.refusal_line();
        assert!(refusal.starts_with("refused: the installed Thoth plugin at "));
        assert!(refusal.contains("plugin version 0.0.1, executable version "));
        assert!(refusal.contains(
            "; skill file differs from the one built into the executable: "
        ));
        assert!(refusal.ends_with(FIX));
        let warning = m.warning_line();
        assert!(warning.starts_with("warning: "));
        assert!(warning.contains("read-only tools still work."));
        let j = m.to_json();
        let mut keys = j.as_object().unwrap().keys().cloned().collect::<Vec<_>>();
        keys.sort();
        assert_eq!(
            keys,
            [
                "differences",
                "expected_version",
                "installed_version",
                "plugin_root",
                "reason"
            ]
        );
        assert_eq!(j["reason"], "plugin_mismatch");

        let empty = tempfile::tempdir().unwrap();
        let m = compare(empty.path()).unwrap();
        assert_eq!(m.differences, ["manifest_missing", "skill_missing"]);
        assert!(m.installed_version.is_none());

        let only = tempfile::tempdir().unwrap();
        let manifest = format!(r#"{{"version":"{}"}}"#, env!("CARGO_PKG_VERSION"));
        write(only.path(), ".claude-plugin/plugin.json", manifest.as_bytes());
        assert_eq!(compare(only.path()).unwrap().differences, ["skill_missing"]);
    }
}
