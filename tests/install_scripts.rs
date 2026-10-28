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

// INST-2: on Windows the installer reads and writes the Path value of
// THOTH_INSTALL_USER_ENV_KEY (test-only) instead of HKCU\Environment's. Every
// fixture creates a fresh key under HKCU\Software and deletes it when it drops,
// also after a failed assertion. The real HKCU\Environment Path is snapshotted
// when the fixture is made and compared after every subprocess (before the
// subprocess's own result is looked at) and once more when the fixture drops.
#[cfg(windows)]
const REAL_ENV_KEY: &str = r"HKCU\Environment";
#[cfg(windows)]
const TEST_KEY_PREFIX: &str = r"HKCU\Software\thoth-install-test-";

/// Prints `missing`, or `present KIND HEX`: the Path value of
/// HKCU\$env:THOTH_TEST_REG_KEY read without expanding %VAR% entries, one
/// four-digit hex number per UTF-16 code unit, so no character is lost.
#[cfg(windows)]
const READ_PATH_VALUE: &str = r"$ErrorActionPreference = 'Stop'
$k = [Microsoft.Win32.Registry]::CurrentUser.OpenSubKey($env:THOTH_TEST_REG_KEY)
if (-not $k) { throw ('cannot open HKCU\' + $env:THOTH_TEST_REG_KEY) }
try {
    if (@($k.GetValueNames()) -notcontains 'Path') {
        'missing'
    } else {
        $kind = [string]$k.GetValueKind('Path')
        if ($kind -eq 'String') { $kind = 'REG_SZ' } elseif ($kind -eq 'ExpandString') { $kind = 'REG_EXPAND_SZ' }
        $v = $k.GetValue('Path', $null, 'DoNotExpandEnvironmentNames')
        if ($v -is [byte[]]) { $v = [BitConverter]::ToString($v) } elseif ($v -is [string[]]) { $v = $v -join [char]0 }
        'present ' + $kind + ' ' + (-join $(foreach ($c in ([string]$v).ToCharArray()) { ([int]$c).ToString('x4') }))
    }
} finally { $k.Close() }";

/// The Path value of a key under HKCU, compared losslessly: the exit status
/// and raw output bytes of `reg query KEY /v Path`, plus the value as
/// PowerShell reads it. reg.exe maps characters outside the console code page
/// to look-alikes (`ü` prints as `u`), so its bytes alone could miss a change.
#[cfg(windows)]
#[derive(Clone, Debug, PartialEq, Eq)]
struct RegSnapshot {
    reg_query: (Option<i32>, Vec<u8>, Vec<u8>),
    /// `None` when the value is missing, else its kind (`REG_SZ`,
    /// `REG_EXPAND_SZ`, ...) and its UTF-16 code units.
    value: Option<(String, Vec<u16>)>,
}

#[cfg(windows)]
impl RegSnapshot {
    fn take(key: &str) -> Self {
        Self::try_take(key).unwrap_or_else(|e| panic!("{e}"))
    }

    /// Like `take`, without panicking: `Drop` uses it while a failed
    /// assertion unwinds.
    fn try_take(key: &str) -> Result<Self, String> {
        let subkey = key
            .strip_prefix(r"HKCU\")
            .ok_or_else(|| format!("{key} is not under HKCU"))?;
        let reg = Command::new("reg")
            .args(["query", key, "/v", "Path"])
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("reg query {key}: {e}"))?;
        let ps = Command::new("powershell.exe")
            .args(["-NoProfile", "-NonInteractive", "-Command", READ_PATH_VALUE])
            .env("THOTH_TEST_REG_KEY", subkey)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("powershell.exe: {e}"))?;
        let text = String::from_utf8(ps.stdout)
            .map_err(|e| format!("reading {key}'s Path printed non-UTF-8 output: {e}"))?;
        if !ps.status.success() {
            return Err(format!(
                "reading {key}'s Path failed: {text}{}",
                String::from_utf8_lossy(&ps.stderr)
            ));
        }
        let fields: Vec<&str> = text.trim_end_matches(['\r', '\n']).split(' ').collect();
        let value = match fields[..] {
            ["missing"] => None,
            ["present", kind, hex] if hex.len() % 4 == 0 => {
                let units = hex
                    .as_bytes()
                    .chunks(4)
                    .map(|unit| {
                        std::str::from_utf8(unit)
                            .ok()
                            .and_then(|unit| u16::from_str_radix(unit, 16).ok())
                    })
                    .collect::<Option<Vec<u16>>>()
                    .ok_or_else(|| format!("reading {key}'s Path printed {text:?}"))?;
                Some((kind.to_string(), units))
            }
            _ => return Err(format!("reading {key}'s Path printed {text:?}")),
        };
        // reg.exe and PowerShell must agree on whether the value is there.
        if reg.status.success() != value.is_some() {
            return Err(format!(
                "reg query {key} /v Path exited {:?} but PowerShell read {text:?}",
                reg.status.code()
            ));
        }
        Ok(Self {
            reg_query: (reg.status.code(), reg.stdout, reg.stderr),
            value,
        })
    }

    /// The value for a person: unpaired surrogates shown as `\u{XXXX}`.
    fn describe(&self) -> String {
        match &self.value {
            None => "missing".to_string(),
            Some((kind, units)) => {
                let text: String = char::decode_utf16(units.iter().copied())
                    .map(|c| match c {
                        Ok(c) => c.to_string(),
                        Err(e) => format!("\\u{{{:04x}}}", e.unpaired_surrogate()),
                    })
                    .collect();
                format!("{kind} {text}")
            }
        }
    }
}

/// The `value` of a snapshot that holds `raw` with kind `kind`.
#[cfg(windows)]
fn reg_value(kind: &str, raw: &str) -> Option<(String, Vec<u16>)> {
    Some((kind.to_string(), raw.encode_utf16().collect()))
}

#[cfg(windows)]
fn reg(args: &[&str]) -> Output {
    Command::new("reg")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("reg.exe must run")
}

/// The 8.3 short spelling of an existing path, when it differs from `path`.
#[cfg(windows)]
fn short_spelling(path: &str) -> Option<String> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;
    let wide: Vec<u16> = std::ffi::OsStr::new(path)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `wide` is NUL-terminated; the buffer length passed matches `buf`.
    let short = unsafe {
        let needed = GetShortPathNameW(wide.as_ptr(), std::ptr::null_mut(), 0);
        if needed == 0 {
            return None;
        }
        let mut buf = vec![0u16; needed as usize];
        let got = GetShortPathNameW(wide.as_ptr(), buf.as_mut_ptr(), needed);
        if got == 0 || got >= needed {
            return None;
        }
        buf.truncate(got as usize);
        std::ffi::OsString::from_wide(&buf).to_string_lossy().into_owned()
    };
    (!short.eq_ignore_ascii_case(path)).then_some(short)
}

/// The long spelling of an existing path, without the `\\?\` prefix.
#[cfg(windows)]
fn long_spelling(path: &Path) -> String {
    let canonical = fs::canonicalize(path).unwrap().display().to_string();
    canonical
        .strip_prefix(r"\\?\")
        .unwrap_or(&canonical)
        .to_string()
}

struct Fixture {
    root: TempDir,
    #[cfg(windows)]
    env_key: String,
    #[cfg(windows)]
    real_path_before: RegSnapshot,
}

#[cfg(windows)]
impl Drop for Fixture {
    fn drop(&mut self) {
        // This also runs while a failed assertion unwinds: report, never panic then.
        let mut problems = Vec::new();
        if self.env_key.starts_with(TEST_KEY_PREFIX) {
            let deleted = Command::new("reg")
                .args(["delete", &self.env_key, "/f"])
                .stdin(Stdio::null())
                .output()
                .is_ok_and(|out| out.status.success());
            if !deleted {
                problems.push(format!("could not delete the test registry key {}", self.env_key));
            }
        }
        match RegSnapshot::try_take(REAL_ENV_KEY) {
            Ok(now) if now == self.real_path_before => {}
            Ok(now) => problems.push(real_path_changed(&self.real_path_before, &now)),
            Err(e) => problems.push(e),
        }
        if !problems.is_empty() {
            let message = problems.join("\n");
            if std::thread::panicking() {
                eprintln!("{message}");
            } else {
                panic!("{message}");
            }
        }
    }
}

#[cfg(windows)]
fn real_path_changed(before: &RegSnapshot, now: &RegSnapshot) -> String {
    format!(
        "the real user PATH in HKCU\\Environment changed (presence, kind or raw value); \
         restore it by hand to the before value\nbefore: {}\nnow:    {}\n{before:?}\n{now:?}",
        before.describe(),
        now.describe()
    )
}

impl Fixture {
    fn new() -> Self {
        #[cfg(windows)]
        let real_path_before = RegSnapshot::take(REAL_ENV_KEY);
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
        #[cfg(windows)]
        {
            use std::sync::atomic::{AtomicUsize, Ordering};
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let env_key = format!(
                "{TEST_KEY_PREFIX}{}-{}-{nanos}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            // Fresh: never reuse (or later delete) a key this fixture did not make.
            assert!(
                !reg(&["query", &env_key]).status.success(),
                "{env_key} already exists"
            );
            let out = reg(&["add", &env_key, "/f"]);
            // Made before any assertion, so `Drop` deletes the key and checks again.
            let f = Self { root, env_key, real_path_before };
            f.assert_real_user_path_unchanged();
            assert!(
                out.status.success(),
                "reg add {}: {}",
                f.env_key,
                String::from_utf8_lossy(&out.stderr)
            );
            f
        }
        #[cfg(target_os = "linux")]
        Self { root }
    }

    /// Runs `cmd`; on Windows it must carry the isolated key, and the real
    /// user PATH is compared before the caller sees the result.
    fn run(&self, cmd: &mut Command) -> Output {
        self.try_run(cmd).unwrap()
    }

    fn try_run(&self, cmd: &mut Command) -> std::io::Result<Output> {
        #[cfg(windows)]
        assert!(
            cmd.get_envs().any(|(k, v)| k == "THOTH_INSTALL_USER_ENV_KEY"
                && v == Some(std::ffi::OsStr::new(&self.env_key))),
            "every subprocess must run with the isolated THOTH_INSTALL_USER_ENV_KEY"
        );
        let out = cmd.output();
        self.assert_real_user_path_unchanged();
        out
    }

    /// Windows: HKCU\Environment's Path still has the presence, kind and raw
    /// value it had when the fixture was made.
    fn assert_real_user_path_unchanged(&self) {
        #[cfg(windows)]
        {
            let now = RegSnapshot::take(REAL_ENV_KEY);
            assert!(
                now == self.real_path_before,
                "{}",
                real_path_changed(&self.real_path_before, &now)
            );
        }
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
        #[cfg(windows)]
        cmd.env("THOTH_INSTALL_USER_ENV_KEY", &self.env_key);
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
fn success(f: &Fixture, cmd: &mut Command) -> Output {
    let out = f.run(cmd);
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
        let out = success(&f, &mut f.command(mode));
        assert_eq!(hash(&f.target()), hash(&f.asset(false)));
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains(&format!("thoth {}", env!("CARGO_PKG_VERSION"))));
        assert!(!stdout.contains("PATH"), "{stdout}");
        // The fixture's folder is first on the process PATH, so the isolated
        // user Path stays absent.
        #[cfg(windows)]
        assert_eq!(f.user_path().value, None);
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
        success(&f, &mut cmd);
        assert_ne!(hash(&f.asset(false)), hash(&f.asset(true)));
        assert_eq!(hash(&f.target()), hash(&f.asset(true)));
        success(&f, &mut f.command(mode));
        assert_eq!(hash(&f.target()), hash(&f.asset(false)));
        success(&f, &mut cmd);
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
                success(&f, &mut f.command(mode));
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
            let out = f.run(&mut f.command(mode));
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
            assert!(!f.run(&mut f.command(mode)).status.success());
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
        assert!(!f.run(&mut f.command(mode)).status.success());
        for tag in ["../../escape", "v1.2.3\n../../escape", "latest", "v1.2"] {
            let mut cmd = f.command(mode);
            cmd.env("THOTH_INSTALL_VERSION", tag);
            assert!(!f.run(&mut cmd).status.success());
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
        assert_setup_skipped(&f, &success(&f, &mut cmd));
    }
    #[cfg(target_os = "linux")]
    {
        if Command::new("setsid").arg("--version").output().is_ok() {
            let f = Fixture::new();
            let mut cmd = Command::new("setsid");
            cmd.args(["--wait", "sh", "install.sh"]);
            f.isolate(&mut cmd);
            cmd.env_remove("THOTH_INSTALL_NO_SETUP");
            assert_setup_skipped(&f, &success(&f, &mut cmd));
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
        success(&f, &mut f.command(mode));
        let handle = fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_DELETE)
            .open(f.target())
            .unwrap();
        let mut cmd = f.command(mode);
        cmd.env("THOTH_INSTALL_VERSION", "v9.9.9");
        let out = success(&f, &mut cmd);
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
        success(&f, &mut cmd);
        drop(handle);
        success(&f, &mut cmd);
        assert!(!old[0].exists());
        f.assert_no_temps();
    }
}

#[cfg(windows)]
#[test]
fn git_bash_sh_rejects_windows_before_writing_files() {
    let f = Fixture::new();
    let mut probe = Command::new("sh");
    probe.arg("-c").arg(":");
    f.isolate(&mut probe);
    if f.try_run(&mut probe).is_err() {
        eprintln!("sh unavailable; Git Bash unsupported-platform check skipped");
        return;
    }
    fs::remove_dir(f.path("bin")).unwrap();
    let mut cmd = Command::new("sh");
    cmd.arg("install.sh");
    f.isolate(&mut cmd);
    let out = f.run(&mut cmd);
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
        let out = success(&f, &mut cmd);
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
        assert!(String::from_utf8_lossy(&success(&f, &mut cmd).stdout).contains("Parser errors: 0"));
    }
    #[cfg(target_os = "linux")]
    for shell in ["sh", "bash"] {
        let mut cmd = Command::new(shell);
        cmd.args(["-n", "install.sh"]);
        f.isolate(&mut cmd);
        success(&f, &mut cmd);
    }
}

#[cfg(windows)]
impl Fixture {
    /// The isolated key's Path value, as the installer under test sees it.
    fn user_path(&self) -> RegSnapshot {
        RegSnapshot::take(&self.env_key)
    }

    /// Sets the isolated Path value (`None` removes it), checks the real one,
    /// and returns the isolated value read back, which must be exactly `value`.
    fn set_user_path(&self, value: Option<(&str, &str)>) -> RegSnapshot {
        assert!(self.env_key.starts_with(TEST_KEY_PREFIX), "{}", self.env_key);
        let out = match value {
            // Fails harmlessly when the value is already missing.
            None => reg(&["delete", &self.env_key, "/v", "Path", "/f"]),
            Some((kind, raw)) => {
                reg(&["add", &self.env_key, "/v", "Path", "/t", kind, "/d", raw, "/f"])
            }
        };
        self.assert_real_user_path_unchanged();
        let now = self.user_path();
        assert_eq!(
            now.value,
            value.and_then(|(kind, raw)| reg_value(kind, raw)),
            "seeding {} with {value:?} (reg exited {:?}) left {}",
            self.env_key,
            out.status.code(),
            now.describe()
        );
        now
    }

    /// The inherited PATH without any entry that is the fixture's install folder.
    fn path_without_install_dir(&self) -> std::ffi::OsString {
        let bin = fs::canonicalize(self.path("bin")).unwrap();
        let kept: Vec<PathBuf> =
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
                .filter(|p| fs::canonicalize(p).map_or(true, |c| c != bin))
                .collect();
        std::env::join_paths(kept).unwrap()
    }
}

#[cfg(windows)]
#[test]
fn windows_path_update_appends_to_the_raw_user_path_as_expand_sz() {
    for mode in modes() {
        let f = Fixture::new();
        let bin = long_spelling(&f.path("bin"));
        // Characters outside the console code page (cp950 here) and a surrogate
        // pair: a lossy read of the stored value could not match exactly.
        let literal_dir = f.path("literal é ü 測試 😀 folder");
        fs::create_dir(&literal_dir).unwrap();
        fs::create_dir(f.path("bin-other")).unwrap();
        let literal = long_spelling(&literal_dir);
        // Similarly named folders, existing or not, are not the install folder.
        let other = long_spelling(&f.path("bin-other"));
        let mut similar = vec![
            other.clone(),
            format!(r"{}\bi", long_spelling(f.root.path())),
            format!("{bin}x"),
        ];
        similar.extend(short_spelling(&other));
        let similar = similar.join(";");
        let with_vars = format!(r"%LOCALAPPDATA%\x\bin;{literal}");
        let cases = [
            (Some(("REG_EXPAND_SZ", with_vars.clone())), format!("{with_vars};{bin}")),
            (Some(("REG_SZ", with_vars.clone())), format!("{with_vars};{bin}")),
            // Trailing semicolons and empty entries are kept as they are.
            (Some(("REG_EXPAND_SZ", format!("{with_vars};;"))), format!("{with_vars};;;{bin}")),
            (Some(("REG_SZ", format!("{similar};;{with_vars};"))), format!("{similar};;{with_vars};;{bin}")),
            // An empty or absent value becomes the install folder alone.
            (Some(("REG_EXPAND_SZ", String::new())), bin.clone()),
            (Some(("REG_SZ", String::new())), bin.clone()),
            (None, bin.clone()),
        ];
        // The process PATH has the similar folders but not the install folder.
        let mut process_path = std::ffi::OsString::from(format!("{similar};"));
        process_path.push(f.path_without_install_dir());
        for (seed, expected) in cases {
            f.set_user_path(seed.as_ref().map(|(kind, raw)| (*kind, raw.as_str())));
            let mut cmd = f.command(mode);
            cmd.env("PATH", &process_path)
                .env("THOTH_INSTALL_DIR", &bin);
            let out = success(&f, &mut cmd);
            let stdout = String::from_utf8_lossy(&out.stdout);
            assert!(stdout.contains("PATH updated"), "{mode:?} {seed:?}: {stdout}");
            assert!(!stdout.contains("Could not announce"), "{mode:?} {seed:?}: {stdout}");
            let stored = f.user_path();
            assert_eq!(
                stored.value,
                reg_value("REG_EXPAND_SZ", &expected),
                "{mode:?} {seed:?}: stored {}",
                stored.describe()
            );
        }
    }
}

#[cfg(windows)]
#[test]
fn windows_path_holding_the_install_folder_in_another_spelling_is_left_alone() {
    for mode in modes() {
        let f = Fixture::new();
        let root = long_spelling(f.root.path());
        let long = long_spelling(&f.path("bin"));
        // (THOTH_INSTALL_DIR, the PATH entry that names the same folder)
        let mut cases = vec![
            (long.clone(), long.to_uppercase()),
            (long.clone(), long.to_lowercase()),
            (long.clone(), format!(r"{long}\")),
            (long.clone(), format!("{long}/")),
            (long.clone(), format!("\"{long}\"")),
            (long.clone(), r"%THOTH_TEST_INSTALL_ROOT%\bin".to_string()),
        ];
        match short_spelling(&long) {
            Some(short) => {
                cases.push((long.clone(), short.clone()));
                cases.push((short, long.clone()));
            }
            None => eprintln!(
                "{long} has no distinct 8.3 short spelling; the short-name case could not be exercised"
            ),
        }
        // Unrelated entries that do not exist, are relative or are not valid
        // paths come first: they must not break the comparison.
        let unrelated = r#"C:\thoth-no-such-dir\x;%THOTH_NO_SUCH_VAR%\y;;relative\dir;C:\thoth-bad|name;"C:\thoth-no-such-dir\quoted""#;
        for (install_dir, entry) in &cases {
            for in_user_path in [true, false] {
                // The entry is in the PATH under test only.
                let (kind, raw, process_path) = if in_user_path {
                    (
                        "REG_SZ",
                        format!(r"{unrelated};{entry};%LOCALAPPDATA%\x\bin"),
                        f.path_without_install_dir(),
                    )
                } else {
                    let mut process_path =
                        std::ffi::OsString::from(format!("{unrelated};{entry};"));
                    process_path.push(f.path_without_install_dir());
                    (
                        "REG_EXPAND_SZ",
                        format!(r"{unrelated};%LOCALAPPDATA%\x\bin"),
                        process_path,
                    )
                };
                let saved = f.set_user_path(Some((kind, raw.as_str())));
                let mut cmd = f.command(mode);
                cmd.env("PATH", process_path)
                    .env("THOTH_INSTALL_DIR", install_dir)
                    .env("THOTH_TEST_INSTALL_ROOT", &root);
                let out = success(&f, &mut cmd);
                let stdout = String::from_utf8_lossy(&out.stdout);
                let case = format!("{mode:?} dir={install_dir} entry={entry} user={in_user_path}");
                assert!(!stdout.contains("PATH updated"), "{case}: {stdout}");
                // Value, kind and reg query's raw bytes, all unchanged.
                let now = f.user_path();
                assert!(
                    now == saved,
                    "{case}: saved {}, now {}\n{saved:?}\n{now:?}",
                    saved.describe(),
                    now.describe()
                );
                assert_eq!(hash(&f.target()), hash(&f.asset(false)), "{case}");
            }
        }
    }
}
