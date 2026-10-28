//! The shell the human is typing into, so a "how to fix" line can be pasted
//! as is (setup scope U8). Detection reads environment variables only: no
//! parent-process lookup (that would need a new windows-sys feature).
//!
//! Known limit: on Windows a cmd started from PowerShell inherits both
//! PowerShell's module path and cmd's PROMPT, so it is taken for PowerShell;
//! pass `--shell cmd` there.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserShell {
    PowerShell,
    Cmd,
    Bash,
    Sh,
}

/// Module-path segments only an interactive powershell.exe / pwsh.exe adds;
/// the machine-wide value has just the Program Files and system32 ones.
const PS_USER_MODULE_DIRS: [&str; 3] = [
    r"\documents\windowspowershell\modules",
    r"\documents\powershell\modules",
    r"\powershell\7\modules",
];

impl UserShell {
    pub fn parse(s: &str) -> Option<UserShell> {
        match s {
            "powershell" => Some(UserShell::PowerShell),
            "cmd" => Some(UserShell::Cmd),
            "bash" => Some(UserShell::Bash),
            "sh" => Some(UserShell::Sh),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            UserShell::PowerShell => "powershell",
            UserShell::Cmd => "cmd",
            UserShell::Bash => "bash",
            UserShell::Sh => "sh",
        }
    }

    /// The shell this process was started from, and `"env"` or `"fallback"`.
    pub fn detect() -> (UserShell, &'static str) {
        detect_from(&|k| std::env::var(k).ok(), cfg!(windows))
    }

    /// A variable set for good: `setx` on Windows whatever the shell (as
    /// `Paths::from_env`'s refusal says), `export` elsewhere.
    pub fn persist_env(self, var: &str, value: &str) -> String {
        persist_env_on(cfg!(windows), var, value)
    }

    pub fn prepend_path(self, dir: &str) -> String {
        prepend_path_on(cfg!(windows), self, dir)
    }

    pub fn remove_file(self, path: &str) -> String {
        match self {
            UserShell::PowerShell => format!("Remove-Item \"{path}\""),
            UserShell::Cmd => format!("del \"{path}\""),
            UserShell::Bash | UserShell::Sh => format!("rm \"{path}\""),
        }
    }
}

/// `persist_env` for either platform, so both are testable on one.
pub fn persist_env_on(windows: bool, var: &str, value: &str) -> String {
    if windows {
        format!("setx {var} \"{value}\"")
    } else {
        format!("export {var}=\"{value}\"")
    }
}

/// `prepend_path` for either platform. On Windows, Git Bash's PATH holds
/// `/c/a/b`, so bash and sh get a drive path `C:\a\b` written that way;
/// anything else (`DIR`, other shells, elsewhere) is left as is.
pub fn prepend_path_on(windows: bool, shell: UserShell, dir: &str) -> String {
    match shell {
        UserShell::PowerShell => format!("$env:Path = \"{dir};$env:Path\""),
        UserShell::Cmd => format!("set PATH={dir};%PATH%"),
        UserShell::Bash | UserShell::Sh => {
            let dir = if windows { msys_path(dir) } else { dir.to_string() };
            format!("export PATH=\"{dir}:$PATH\"")
        }
    }
}

/// `X:\a\b` (or `X:/a/b`) -> `/x/a/b`; anything else unchanged.
fn msys_path(dir: &str) -> String {
    let b = dir.as_bytes();
    if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/') {
        let drive = (b[0] as char).to_ascii_lowercase();
        format!("/{drive}{}", dir[2..].replace('\\', "/"))
    } else {
        dir.to_string()
    }
}

/// The detection rules; the first that matches wins.
///
/// Windows: `MSYSTEM` (non-empty) or `SHLVL` -> bash (Git Bash, which is also
/// what Claude Code's `!` runs); a per-user PowerShell module directory in
/// `PSModulePath`, or `PSExecutionPolicyPreference` -> powershell; a
/// non-empty `PROMPT` -> cmd; otherwise powershell as a fallback.
/// Elsewhere: `SHELL` whose file name is `bash` -> bash, any other -> sh,
/// unset -> sh as a fallback.
pub fn detect_from(
    get: &dyn Fn(&str) -> Option<String>,
    windows: bool,
) -> (UserShell, &'static str) {
    let non_empty = |k: &str| get(k).is_some_and(|v| !v.is_empty());
    if windows {
        if non_empty("MSYSTEM") || get("SHLVL").is_some() {
            return (UserShell::Bash, "env");
        }
        let user_modules = get("PSModulePath").is_some_and(|v| {
            v.split(';').any(|seg| {
                let seg = seg.to_lowercase();
                PS_USER_MODULE_DIRS.iter().any(|d| seg.contains(d))
            })
        });
        if user_modules || get("PSExecutionPolicyPreference").is_some() {
            return (UserShell::PowerShell, "env");
        }
        if non_empty("PROMPT") {
            return (UserShell::Cmd, "env");
        }
        return (UserShell::PowerShell, "fallback");
    }
    match get("SHELL") {
        Some(s) if !s.is_empty() => {
            let name = s.rsplit('/').next().unwrap_or("");
            if name == "bash" {
                (UserShell::Bash, "env")
            } else {
                (UserShell::Sh, "env")
            }
        }
        _ => (UserShell::Sh, "fallback"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)], windows: bool) -> (UserShell, &'static str) {
        let map: std::collections::HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        detect_from(&|k| map.get(k).cloned(), windows)
    }

    const MACHINE_PS: &str =
        r"C:\Program Files\WindowsPowerShell\Modules;C:\WINDOWS\system32\WindowsPowerShell\v1.0\Modules";

    #[test]
    fn user_shell_windows_rules() {
        assert_eq!(env(&[("MSYSTEM", "MINGW64")], true), (UserShell::Bash, "env"));
        assert_eq!(env(&[("SHLVL", "1")], true), (UserShell::Bash, "env"));
        let user_ps = format!(r"C:\Users\x\Documents\WindowsPowerShell\Modules;{MACHINE_PS}");
        assert_eq!(env(&[("PSModulePath", &user_ps)], true), (UserShell::PowerShell, "env"));
        let pwsh = format!(r"C:\Program Files\PowerShell\7\Modules;{MACHINE_PS}");
        assert_eq!(env(&[("PSModulePath", &pwsh)], true), (UserShell::PowerShell, "env"));
        assert_eq!(
            env(&[("PSModulePath", MACHINE_PS), ("PSExecutionPolicyPreference", "Bypass")], true),
            (UserShell::PowerShell, "env")
        );
        assert_eq!(
            env(&[("PSModulePath", MACHINE_PS), ("PROMPT", "$P$G")], true),
            (UserShell::Cmd, "env")
        );
        assert_eq!(
            env(&[("MSYSTEM", "MINGW64"), ("PROMPT", "$P$G")], true),
            (UserShell::Bash, "env")
        );
        assert_eq!(env(&[("MSYSTEM", ""), ("PROMPT", "")], true), (UserShell::PowerShell, "fallback"));
        assert_eq!(env(&[], true), (UserShell::PowerShell, "fallback"));
    }

    #[test]
    fn user_shell_linux_rules() {
        assert_eq!(env(&[("SHELL", "/bin/bash")], false), (UserShell::Bash, "env"));
        assert_eq!(env(&[("SHELL", "/bin/zsh")], false), (UserShell::Sh, "env"));
        assert_eq!(env(&[], false), (UserShell::Sh, "fallback"));
        // Windows-only markers mean nothing here.
        assert_eq!(env(&[("MSYSTEM", "MINGW64")], false), (UserShell::Sh, "fallback"));
    }

    #[test]
    fn user_shell_parse_and_name_round_trip() {
        for s in ["powershell", "cmd", "bash", "sh"] {
            assert_eq!(UserShell::parse(s).unwrap().name(), s);
        }
        assert_eq!(UserShell::parse("pwsh"), None);
    }

    #[test]
    fn user_shell_format_functions_verbatim() {
        use UserShell::*;
        assert_eq!(persist_env_on(true, "V", "x"), r#"setx V "x""#);
        assert_eq!(persist_env_on(false, "V", "x"), r#"export V="x""#);
        for s in [PowerShell, Cmd, Bash, Sh] {
            assert_eq!(s.persist_env("V", "x"), persist_env_on(cfg!(windows), "V", "x"));
        }

        assert_eq!(PowerShell.prepend_path("DIR"), r#"$env:Path = "DIR;$env:Path""#);
        assert_eq!(Cmd.prepend_path("DIR"), "set PATH=DIR;%PATH%");
        assert_eq!(Bash.prepend_path("DIR"), r#"export PATH="DIR:$PATH""#);
        assert_eq!(Sh.prepend_path("DIR"), r#"export PATH="DIR:$PATH""#);

        for w in [true, false] {
            assert_eq!(prepend_path_on(w, Bash, "DIR"), r#"export PATH="DIR:$PATH""#);
            assert_eq!(prepend_path_on(w, Sh, "DIR"), r#"export PATH="DIR:$PATH""#);
        }

        assert_eq!(PowerShell.remove_file("p"), r#"Remove-Item "p""#);
        assert_eq!(Cmd.remove_file("p"), r#"del "p""#);
        assert_eq!(Bash.remove_file("p"), r#"rm "p""#);
        assert_eq!(Sh.remove_file("p"), r#"rm "p""#);
    }

    #[test]
    fn user_shell_prepend_path_drive_paths_for_bash_on_windows() {
        use UserShell::*;
        let d = r"C:\Users\u\.cargo\bin";
        assert_eq!(prepend_path_on(true, Bash, d), r#"export PATH="/c/Users/u/.cargo/bin:$PATH""#);
        assert_eq!(prepend_path_on(true, Sh, d), r#"export PATH="/c/Users/u/.cargo/bin:$PATH""#);
        assert_eq!(prepend_path_on(true, Bash, "DIR"), r#"export PATH="DIR:$PATH""#);
        assert_eq!(prepend_path_on(true, PowerShell, d), format!(r#"$env:Path = "{d};$env:Path""#));
        assert_eq!(prepend_path_on(true, Cmd, d), format!("set PATH={d};%PATH%"));
        assert_eq!(prepend_path_on(false, Bash, d), format!(r#"export PATH="{d}:$PATH""#));
        assert_eq!(prepend_path_on(false, Bash, "/home/u/.cargo/bin"), r#"export PATH="/home/u/.cargo/bin:$PATH""#);
    }
}
