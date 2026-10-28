#![cfg(any(windows, target_os = "linux"))]

use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use tempfile::TempDir;

#[cfg(windows)]
const ASSET: &str = "thoth-x86_64-pc-windows-msvc.exe";
#[cfg(target_os = "linux")]
const ASSET: &str = "thoth-x86_64-unknown-linux-gnu";
#[cfg(windows)]
const INSTALLED: &str = "thoth.exe";
#[cfg(target_os = "linux")]
const INSTALLED: &str = "thoth";

#[derive(Clone, Copy, Debug)]
enum Mode {
    #[cfg(target_os = "linux")]
    Sh,
    #[cfg(windows)]
    File,
    #[cfg(windows)]
    Iex,
}

fn modes() -> Vec<Mode> {
    #[cfg(windows)]
    {
        vec![Mode::File, Mode::Iex]
    }
    #[cfg(target_os = "linux")]
    {
        vec![Mode::Sh]
    }
}

fn hash(path: &Path) -> String {
    Sha256::digest(fs::read(path).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

struct Fixture {
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        // A space in every fixture exercises quoting of local sources and targets.
        let root = tempfile::Builder::new()
            .prefix("install scripts ")
            .tempdir()
            .unwrap();
        for subdir in ["tmp", "user", "bin"] {
            fs::create_dir(root.path().join(subdir)).unwrap();
        }
        for release in ["latest/download", "download/v9.9.9"] {
            let dir = root.path().join("release").join(release);
            fs::create_dir_all(&dir).unwrap();
            let asset = dir.join(ASSET);
            fs::copy(env!("CARGO_BIN_EXE_thoth"), &asset).unwrap();
            if release == "download/v9.9.9" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&asset)
                    .unwrap()
                    .write_all(b"\ninstaller test tagged release\n")
                    .unwrap();
            }
            // Similar names must not match; accept coreutils text and binary markers,
            // as well as uppercase hashes.
            let digest = hash(&asset);
            let entry = if release == "latest/download" {
                format!("{}  {ASSET}", digest.to_uppercase())
            } else {
                format!("{digest} *{ASSET}")
            };
            fs::write(
                dir.join("SHA256SUMS"),
                format!("{}  {ASSET}.other\n{entry}\n", "0".repeat(64)),
            )
            .unwrap();
        }
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }
    fn target(&self) -> PathBuf {
        self.path("bin").join(INSTALLED)
    }
    fn asset(&self, tagged: bool) -> PathBuf {
        self.path(if tagged {
            "release/download/v9.9.9"
        } else {
            "release/latest/download"
        })
        .join(ASSET)
    }

    fn command(&self, mode: Mode) -> Command {
        let mut cmd = match mode {
            #[cfg(target_os = "linux")]
            Mode::Sh => {
                let mut c = Command::new("sh");
                c.arg("install.sh");
                c
            }
            #[cfg(windows)]
            Mode::File => {
                let mut c = Command::new("powershell.exe");
                c.args([
                    "-NoProfile",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-File",
                    "install.ps1",
                ]);
                c
            }
            #[cfg(windows)]
            Mode::Iex => {
                let mut c = Command::new("powershell.exe");
                c.args([
                    "-NoProfile",
                    "-Command",
                    "iex (Get-Content -Raw install.ps1)",
                ]);
                c
            }
        };
        self.isolate(&mut cmd);
        cmd
    }

    fn isolate(&self, cmd: &mut Command) {
        let mut paths = vec![self.path("bin")];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        cmd.current_dir(env!("CARGO_MANIFEST_DIR"))
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("HOME", self.path("user"))
            .env("USERPROFILE", self.path("user"))
            .env("THOTH_HOME", self.path("home"))
            .env("THOTH_INSTALL_DIR", self.path("bin"))
            .env("THOTH_INSTALL_BASE_URL", self.path("release"))
            .env("THOTH_INSTALL_NO_SETUP", "1")
            .env("TMPDIR", self.path("tmp"))
            .env("TEMP", self.path("tmp"))
            .env("TMP", self.path("tmp"))
            .env("LOCALAPPDATA", self.path("user/AppData/Local"))
            .env(
                "PSModuleAnalysisCachePath",
                self.path("tmp/powershell-module-cache"),
            )
            .env_remove("THOTH_LANG")
            .env_remove("THOTH_INSTALL_VERSION")
            .env_remove("THOTH_LOGIN_SKIP_TTY_CHECK")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }

    fn assert_no_temps(&self) {
        for entry in fs::read_dir(self.path("tmp")).unwrap() {
            assert!(!entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("thoth-install-"));
        }
    }

    fn assert_no_state(&self) {
        assert!(!self.path("home").exists());
        for dir in [".thoth", ".claude", ".agentctl"] {
            assert!(!self.path("user").join(dir).exists());
        }
    }
}

#[track_caller]
fn success(cmd: &mut Command) -> Output {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?}\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

#[test]
fn clean_install_prints_real_version_without_path_changes() {
    for mode in modes() {
        let f = Fixture::new();
        let out = success(&mut f.command(mode));
        assert_eq!(hash(&f.target()), hash(&f.asset(false)));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(&format!("thoth {}", env!("CARGO_PKG_VERSION"))));
        assert!(!stdout.contains("PATH"), "{stdout}");
        f.assert_no_temps();
        f.assert_no_state();
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(f.target()).unwrap().permissions().mode() & 0o777,
                0o755
            );
        }
    }
}

#[test]
fn explicit_tag_and_repeated_install_upgrade_bytes() {
    for mode in modes() {
        let f = Fixture::new();
        let mut cmd = f.command(mode);
        cmd.env("THOTH_INSTALL_VERSION", "v9.9.9");
        success(&mut cmd);
        assert_ne!(hash(&f.asset(false)), hash(&f.asset(true)));
        assert_eq!(hash(&f.target()), hash(&f.asset(true)));
        success(&mut f.command(mode));
        assert_eq!(hash(&f.target()), hash(&f.asset(false)));
        success(&mut cmd);
        assert_eq!(hash(&f.target()), hash(&f.asset(true)));
        f.assert_no_temps();
        f.assert_no_state();
    }
}

#[test]
fn checksum_failure_cleans_temp_and_preserves_installation() {
    for mode in modes() {
        for existing in [false, true] {
            let f = Fixture::new();
            if existing {
                success(&mut f.command(mode));
            } else {
                fs::remove_dir(f.path("bin")).unwrap();
            }
            let sum = f.path("release/latest/download/SHA256SUMS");
            let digest = hash(&f.asset(false));
            let bad = format!(
                "{}{}",
                if digest.starts_with('0') { "1" } else { "0" },
                &digest[1..]
            );
            fs::write(sum, format!("{bad}  {ASSET}\n")).unwrap();
            let out = f.command(mode).output().unwrap();
            assert!(!out.status.success());
            let message = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
            .to_lowercase();
            assert!(message.contains(&bad));
            assert!(message.contains(&digest));
            if existing {
                assert_eq!(hash(&f.target()), digest);
            } else {
                assert!(!f.path("bin").exists());
            }
            f.assert_no_temps();
            f.assert_no_state();
        }
    }
}

#[test]
fn missing_duplicate_and_malformed_checksum_entries_fail_closed() {
    for mode in modes() {
        let f = Fixture::new();
        fs::remove_dir(f.path("bin")).unwrap();
        let entry = format!("{}  {ASSET}\n", hash(&f.asset(false)));
        for sums in [
            format!("{}  {ASSET}.other\n", "0".repeat(64)),
            entry.repeat(2),
            format!("invalid  {ASSET}\n"),
        ] {
            fs::write(f.path("release/latest/download/SHA256SUMS"), sums).unwrap();
            assert!(!f.command(mode).output().unwrap().status.success());
            assert!(!f.path("bin").exists());
            f.assert_no_temps();
        }
    }
}

#[test]
fn download_failure_and_invalid_tag_leave_install_directory_untouched() {
    for mode in modes() {
        let f = Fixture::new();
        fs::remove_dir(f.path("bin")).unwrap();
        fs::remove_file(f.asset(false)).unwrap();
        assert!(!f.command(mode).output().unwrap().status.success());
        for tag in ["../../escape", "v1.2.3\n../../escape", "latest", "v1.2"] {
            let mut cmd = f.command(mode);
            cmd.env("THOTH_INSTALL_VERSION", tag);
            assert!(!cmd.output().unwrap().status.success());
        }
        assert!(!f.path("bin").exists());
        f.assert_no_temps();
    }
}

fn assert_setup_skipped(f: &Fixture, out: &Output) {
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stdout.contains("thoth setup"));
    assert!(stdout.contains("Keep THOTH_HOME set to"));
    for refusal in [
        "thoth setup needs a real terminal",
        "thoth setup refuses to run with THOTH_RUN_ID set",
    ] {
        assert!(!stdout.contains(refusal), "{stdout}");
        assert!(!stderr.contains(refusal), "{stderr}");
    }
    f.assert_no_state();
    f.assert_no_temps();
}

#[test]
fn redirected_terminal_skips_setup_without_calling_it() {
    for mode in modes() {
        let f = Fixture::new();
        let mut cmd = f.command(mode);
        cmd.env_remove("THOTH_INSTALL_NO_SETUP");
        assert_setup_skipped(&f, &success(&mut cmd));
    }
    #[cfg(target_os = "linux")]
    {
        if Command::new("setsid").arg("--version").output().is_ok() {
            let f = Fixture::new();
            let mut cmd = Command::new("setsid");
            cmd.args(["--wait", "sh", "install.sh"]);
            f.isolate(&mut cmd);
            cmd.env_remove("THOTH_INSTALL_NO_SETUP");
            assert_setup_skipped(&f, &success(&mut cmd));
        } else {
            eprintln!("setsid unavailable; only the piped terminal case was tested");
        }
    }
}

#[cfg(windows)]
#[test]
fn locked_windows_binary_is_renamed_then_cleaned_after_handle_closes() {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_SHARE_DELETE, FILE_SHARE_READ};
    for mode in modes() {
        let f = Fixture::new();
        success(&mut f.command(mode));
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(f.target())
            .unwrap();
        let mut cmd = f.command(mode);
        cmd.env("THOTH_INSTALL_VERSION", "v9.9.9");
        let out = success(&mut cmd);
        assert!(String::from_utf8_lossy(&out.stdout).contains("Old executable renamed"));
        assert_eq!(hash(&f.target()), hash(&f.asset(true)));
        let old: Vec<_> = fs::read_dir(f.path("bin"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("thoth.old-")
            })
            .collect();
        assert_eq!(old.len(), 1);
        assert_eq!(hash(&old[0]), hash(&f.asset(false)));
        // A still-open old file may be undeletable; another upgrade must succeed.
        success(&mut cmd);
        drop(handle);
        success(&mut cmd);
        assert!(!old[0].exists());
        f.assert_no_temps();
    }
}

#[cfg(windows)]
#[test]
fn git_bash_sh_rejects_windows_before_writing_files() {
    if Command::new("sh").arg("-c").arg(":").output().is_err() {
        eprintln!("sh unavailable; Git Bash unsupported-platform check skipped");
        return;
    }
    let f = Fixture::new();
    fs::remove_dir(f.path("bin")).unwrap();
    let mut cmd = Command::new("sh");
    cmd.arg("install.sh");
    f.isolate(&mut cmd);
    let out = cmd.output().unwrap();
    assert!(!out.status.success());
    let message = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(message.contains("cargo install"), "{message}");
    assert!(!f.path("bin").exists());
    f.assert_no_temps();
    f.assert_no_state();
}

#[cfg(windows)]
#[test]
fn iex_keeps_caller_preferences_and_does_not_close_the_shell_on_failure() {
    for fail in [false, true] {
        let f = Fixture::new();
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-Command", "$ErrorActionPreference='Continue'; Set-StrictMode -Off; try { iex (Get-Content -Raw install.ps1) } catch { Write-Output 'Installer failure caught; shell alive' }; if ($ErrorActionPreference -ne 'Continue') { throw 'Error preference leaked' }; $null=$undefinedCallerVariable; Write-Output 'Caller scope intact'"]);
        f.isolate(&mut cmd);
        if fail {
            cmd.env("THOTH_INSTALL_VERSION", "invalid");
        }
        let out = success(&mut cmd);
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("Caller scope intact"));
        assert_eq!(
            stdout.contains("Installer failure caught; shell alive"),
            fail
        );
        assert_eq!(f.target().exists(), !fail);
        f.assert_no_state();
        f.assert_no_temps();
    }
}

#[test]
fn scripts_parse_with_platform_shells() {
    let f = Fixture::new();
    #[cfg(windows)]
    {
        let mut cmd = Command::new("powershell.exe");
        cmd.args(["-NoProfile", "-Command", "$tokens=$null; $errors=$null; $null=[System.Management.Automation.Language.Parser]::ParseFile((Join-Path (Get-Location) 'install.ps1'),[ref]$tokens,[ref]$errors); if ($errors.Count) { throw ($errors | Out-String) }; Write-Output 'Parser errors: 0'"]);
        f.isolate(&mut cmd);
        assert!(String::from_utf8_lossy(&success(&mut cmd).stdout).contains("Parser errors: 0"));
    }
    #[cfg(target_os = "linux")]
    for shell in ["sh", "bash"] {
        let mut cmd = Command::new(shell);
        cmd.args(["-n", "install.sh"]);
        f.isolate(&mut cmd);
        success(&mut cmd);
    }
}
