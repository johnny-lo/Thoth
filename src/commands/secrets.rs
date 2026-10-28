//! The two score API keys, kept by Thoth itself in `$THOTH_HOME/secrets.json`
//! (config-scope G5). The file is separate from config.json so that "never
//! displayed" rests on one rule: nothing reads this file except the
//! functions here and `settings::secret`, and they hand the content only to
//! `secrets.json` itself and to curl's stdin. A value is never taken from the
//! command line; `thoth config set-secret` reads it from a hidden terminal
//! prompt or from stdin.
//!
//! The registry entry of each key and the precedence rule (environment
//! variable, then this file, then unset) stay in `settings`; this module owns
//! the file and the terminal.

use crate::commands::settings::{Kind, Loaded, Setting, REGISTRY};
use crate::config::Paths;
use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};
use std::fs;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

pub(crate) const FILE_NAME: &str = "secrets.json";

/// The longest value accepted; real keys are far shorter.
const MAX_VALUE_BYTES: usize = 1024;

pub(crate) fn path_in(home: &Path) -> PathBuf {
    home.join(FILE_NAME)
}

/// Reads `home/secrets.json`; prints nothing and does not look at
/// THOTH_RUN_ID (the caller decides whether a run may read it). `problem`
/// never carries any part of the file's content.
pub(crate) fn load_secrets(home: &Path) -> Loaded {
    let path = path_in(home);
    let text = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Loaded { exists: false, map: Map::new(), problem: None }
        }
        Err(e) => return Loaded { exists: true, map: Map::new(), problem: Some(e.to_string()) },
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Loaded { exists: true, map, problem: None },
        Ok(_) => Loaded { exists: true, map: Map::new(), problem: Some("not a JSON object".to_string()) },
        Err(e) => Loaded { exists: true, map: Map::new(), problem: Some(e.to_string()) },
    }
}

/// The key a secret is stored under in secrets.json: its environment
/// variable's name in ASCII lower case.
pub(crate) fn secret_stored_name(s: &Setting) -> String {
    s.env.unwrap_or_default().to_ascii_lowercase()
}

/// Every stored name secrets.json may hold.
pub(crate) fn secret_keys() -> Vec<String> {
    REGISTRY
        .iter()
        .filter(|s| s.kind == Kind::Secret)
        .map(secret_stored_name)
        .collect()
}

/// Whether only the owner can read `path`: Unix mode bits only. `None` when
/// the file does not exist, and always on Windows (the file inherits the
/// state directory's ACL, which is not inspected).
pub(crate) fn private(path: &Path) -> Option<bool> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).ok().map(|m| m.permissions().mode() & 0o077 == 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// What `config list` and `doctor` say about the file, without touching it
/// beyond reading. While THOTH_RUN_ID is set the file is not opened at all:
/// only its existence and mode are reported.
pub(crate) struct Report {
    pub(crate) path: PathBuf,
    pub(crate) exists: bool,
    pub(crate) read: bool,
    loaded: Option<Loaded>,
    pub(crate) unknown_keys: Option<Vec<String>>,
    pub(crate) private: Option<bool>,
}

impl Report {
    pub(crate) fn inspect(home: &Path) -> Report {
        let path = path_in(home);
        let read = std::env::var_os("THOTH_RUN_ID").is_none();
        let private = private(&path);
        if !read {
            return Report { exists: path.exists(), path, read, loaded: None, unknown_keys: None, private };
        }
        let loaded = load_secrets(home);
        let known = secret_keys();
        let mut unknown: Vec<String> = loaded.map.keys().filter(|k| !known.contains(k)).cloned().collect();
        unknown.sort_unstable();
        Report { exists: loaded.exists, path, read, loaded: Some(loaded), unknown_keys: Some(unknown), private }
    }

    /// The stored keys, empty when the file is unusable or not read.
    pub(crate) fn map(&self) -> Map<String, Value> {
        self.loaded.as_ref().map(|l| l.map.clone()).unwrap_or_default()
    }

    pub(crate) fn ok(&self) -> Option<bool> {
        self.loaded.as_ref().map(|l| l.problem.is_none())
    }

    pub(crate) fn error(&self) -> Option<&str> {
        self.loaded.as_ref().and_then(|l| l.problem.as_deref())
    }

    /// `exists` and `!ok`.
    pub(crate) fn broken(&self) -> bool {
        self.exists && self.ok() == Some(false)
    }

    pub(crate) fn json(&self, with_read: bool) -> Value {
        let mut v = json!({
            "path": self.path.display().to_string(),
            "exists": self.exists,
            "ok": self.ok(),
            "error": self.error(),
            "unknown_keys": self.unknown_keys,
            "private": self.private,
        });
        if with_read {
            v["read"] = json!(self.read);
        }
        v
    }

    /// The warning lines `config list` prints after `elsewhere`; while
    /// THOTH_RUN_ID is set, only the note that the file is not read (its mode
    /// stays in the JSON).
    pub(crate) fn warnings(&self) -> Vec<String> {
        let path = self.path.display();
        if !self.read {
            return vec![format!("note: {path} is not read while THOTH_RUN_ID is set")];
        }
        let mut lines = Vec::new();
        if self.broken() {
            lines.push(format!(
                "warning: {path} cannot be used ({}); the keys stored in it are ignored",
                self.error().unwrap_or_default()
            ));
        }
        if let Some(unknown) = self.unknown_keys.as_ref().filter(|u| !u.is_empty()) {
            lines.push(format!("warning: {path} has unknown keys: {} (ignored)", unknown.join(", ")));
        }
        if self.private == Some(false) {
            lines.push(format!("warning: {path} is readable by other users; run chmod 600 {path}"));
        }
        lines
    }
}

/// Stores (`Some`) or removes (`None`) one key of secrets.json atomically,
/// keeping every other key; returns whether the key was a non-blank string
/// before (the same rule `settings::secret` reads with). The file is created
/// readable by its owner only on Unix; on Windows it inherits the state
/// directory's ACL. No message carries the value.
pub(crate) fn write_secret(paths: &Paths, stored: &str, value: Option<&str>) -> Result<bool> {
    let path = path_in(&paths.home);
    let mut object = match fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
        Err(e) => {
            return Err(e).with_context(|| format!("cannot read secrets file {}", path.display()))
        }
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(object)) => object,
            _ => bail!("secrets file {} is not a valid JSON object", path.display()),
        },
    };
    let previous = object
        .get(stored)
        .and_then(Value::as_str)
        .is_some_and(|t| !t.trim().is_empty());
    match value {
        Some(v) => object.insert(stored.to_string(), Value::String(v.to_string())),
        None => object.remove(stored),
    };
    fs::create_dir_all(&paths.home)
        .with_context(|| format!("cannot create {}", paths.home.display()))?;
    let temp = paths.home.join(format!("{FILE_NAME}.tmp-{}", std::process::id()));
    let written = replace_file(&temp, &path, &object);
    if written.is_err() {
        let _ = fs::remove_file(&temp);
    }
    written?;
    Ok(previous)
}

/// Writes `object` to `temp` (created exclusively, mode 0600 on Unix) and
/// renames it over `path`; afterwards the final file's mode is set to 0600
/// too, which repairs an older file with a wider mode.
fn replace_file(temp: &Path, path: &Path, object: &Map<String, Value>) -> Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(temp)
        .with_context(|| format!("cannot create {}", temp.display()))?;
    file.write_all(&serde_json::to_vec_pretty(&Value::Object(object.clone()))?)
        .with_context(|| format!("cannot write {}", temp.display()))?;
    file.sync_all()
        .with_context(|| format!("cannot write {}", temp.display()))?;
    drop(file);
    fs::rename(temp, path)
        .with_context(|| format!("cannot atomically replace {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("cannot make {} private", path.display()))?;
    }
    Ok(())
}

/// Checks a typed or piped value and returns it without its one trailing
/// line ending. Every message names the key, never the input.
pub(crate) fn validate_secret_value(key: &str, raw: &str) -> Result<String> {
    let value = raw
        .strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(raw);
    if value.trim().is_empty() {
        bail!("invalid secret value for {key}: nothing was entered");
    }
    if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        bail!("invalid secret value for {key}: the value must be a single token without spaces or control characters");
    }
    if value.len() > MAX_VALUE_BYTES {
        bail!("invalid secret value for {key}: longer than {MAX_VALUE_BYTES} bytes");
    }
    Ok(value.to_string())
}

/// The value from a non-terminal stdin: the whole input, which must be
/// exactly one line.
pub(crate) fn read_secret_from(key: &str, mut input: impl Read) -> Result<String> {
    let mut raw = String::new();
    input
        .read_to_string(&mut raw)
        .with_context(|| format!("cannot read the value for {key} from stdin"))?;
    let body = raw
        .strip_suffix("\r\n")
        .or_else(|| raw.strip_suffix('\n'))
        .unwrap_or(&raw);
    if body.contains('\n') {
        bail!("invalid secret value for {key}: expected exactly one line on stdin");
    }
    validate_secret_value(key, &raw)
}

/// The value for `key` (environment variable `var`): typed at a hidden
/// prompt when stdin is a terminal, else read from stdin.
pub(crate) fn read_secret_value(key: &str, var: &str) -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        return read_secret_from(key, stdin.lock());
    }
    crate::output::eprint_masked(&format!(
        "Enter the value for {key} ({var}); typing is hidden, press Enter when done: "
    ));
    let hidden = match HiddenInput::new() {
        Ok(h) => h,
        Err(reason) => {
            crate::output::eprint_masked("\n");
            bail!("cannot hide typed input on this terminal ({reason}); put the key alone in a file and run thoth config set-secret {key} < FILE, then delete the file");
        }
    };
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    drop(hidden);
    crate::output::eprint_masked("\n");
    read.with_context(|| format!("cannot read the value for {key}"))?;
    validate_secret_value(key, &line)
}

/// Echo switched off on stdin for as long as this value lives; the previous
/// mode comes back on drop, so a failed read or a refused value restores it
/// too. Ctrl+C during the prompt skips the drop (a known limitation).
#[cfg(target_os = "linux")]
struct HiddenInput {
    original: libc::termios,
}

#[cfg(target_os = "linux")]
impl HiddenInput {
    fn new() -> std::io::Result<Self> {
        let mut term = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: fd 0 is this process's stdin, which the caller has checked
        // is a terminal; tcgetattr only writes into the termios we own.
        if unsafe { libc::tcgetattr(0, term.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: tcgetattr returned 0, so it filled the whole struct.
        let original = unsafe { term.assume_init() };
        let mut hidden = original;
        hidden.c_lflag &= !libc::ECHO;
        // SAFETY: `hidden` is a fully initialised termios that outlives the call.
        if unsafe { libc::tcsetattr(0, libc::TCSANOW, &hidden) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { original })
    }
}

#[cfg(target_os = "linux")]
impl Drop for HiddenInput {
    fn drop(&mut self) {
        // SAFETY: `original` is the termios tcgetattr gave us; restoring it
        // is the inverse of `new` and failing to is not fatal.
        unsafe { libc::tcsetattr(0, libc::TCSANOW, &self.original) };
    }
}

#[cfg(windows)]
struct HiddenInput {
    handle: windows_sys::Win32::Foundation::HANDLE,
    original: u32,
}

#[cfg(windows)]
impl HiddenInput {
    fn new() -> std::io::Result<Self> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            GetConsoleMode, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT, STD_INPUT_HANDLE,
        };
        // SAFETY: GetStdHandle takes a constant and returns a handle this
        // process does not own (it must not be closed).
        let handle = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
        if handle == INVALID_HANDLE_VALUE || handle.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        let mut original = 0u32;
        // SAFETY: `original` is a live u32 the call writes into; a non-console
        // stdin (a pipe, or mintty's pty) makes the call fail, which is
        // reported rather than ignored.
        if unsafe { GetConsoleMode(handle, &mut original) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the handle was just validated by GetConsoleMode.
        if unsafe { SetConsoleMode(handle, original & !ENABLE_ECHO_INPUT) } == 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self { handle, original })
    }
}

#[cfg(windows)]
impl Drop for HiddenInput {
    fn drop(&mut self) {
        use windows_sys::Win32::System::Console::SetConsoleMode;
        // SAFETY: `handle` and `original` come from a successful `new`;
        // putting the mode back is the inverse of it and failing is not fatal.
        unsafe { SetConsoleMode(self.handle, self.original) };
    }
}

#[cfg(not(any(target_os = "linux", windows)))]
struct HiddenInput;

#[cfg(not(any(target_os = "linux", windows)))]
impl HiddenInput {
    fn new() -> std::io::Result<Self> {
        Err(std::io::Error::other("hidden input is not supported on this platform"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::settings::secret;

    fn restore(vars: &[(&str, Option<std::ffi::OsString>)]) {
        for (k, v) in vars {
            match v {
                Some(v) => std::env::set_var(k, v),
                None => std::env::remove_var(k),
            }
        }
    }

    #[test]
    fn secret_prefers_env_then_file_and_ignores_the_file_in_a_run() {
        let _guard = crate::ENV_LOCK.lock().unwrap();
        // The host may carry real keys in its environment: clear both before
        // anything reads them, put them back at the end, and assert without
        // printing what `secret` returned.
        let names = ["AISTUPIDLEVEL_API_KEY", "ARTIFICIALANALYSIS_API_KEY", "THOTH_RUN_ID"];
        let saved: Vec<_> = names.iter().map(|k| (*k, std::env::var_os(k))).collect();
        for name in names {
            std::env::remove_var(name);
        }
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let asl = || secret(home, "aistupidlevel-api-key");

        assert!(asl().is_none(), "no file and no variable: unset");
        fs::write(
            path_in(home),
            r#"{"aistupidlevel_api_key":" dummy-file-key ","artificialanalysis_api_key":5}"#,
        )
        .unwrap();
        assert!(asl().as_deref() == Some("dummy-file-key"), "the stored key, trimmed");
        assert!(secret(home, "artificialanalysis-api-key").is_none(), "a stored number is not a key");

        std::env::set_var("AISTUPIDLEVEL_API_KEY", "dummy-env-key");
        assert!(asl().as_deref() == Some("dummy-env-key"), "the variable wins");
        std::env::set_var("AISTUPIDLEVEL_API_KEY", "   ");
        assert!(asl().as_deref() == Some("dummy-file-key"), "a blank variable counts as unset");

        std::env::remove_var("AISTUPIDLEVEL_API_KEY");
        std::env::set_var("THOTH_RUN_ID", "01RUN");
        assert!(asl().is_none(), "a run does not read secrets.json");
        assert!(secret(home, "claude-bin").is_none(), "claude-bin is not a secret");

        restore(&saved);
    }

    #[cfg(unix)]
    #[test]
    fn write_secret_creates_a_0600_file_and_keeps_other_keys() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::from_root(dir.path().to_path_buf());
        let path = path_in(dir.path());
        let mode = || fs::metadata(&path).unwrap().permissions().mode() & 0o777;

        assert!(!write_secret(&paths, "aistupidlevel_api_key", Some("dummy-a")).unwrap());
        assert_eq!(mode(), 0o600);
        assert!(write_secret(&paths, "aistupidlevel_api_key", Some("dummy-a2")).unwrap());

        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!write_secret(&paths, "artificialanalysis_api_key", Some("dummy-b")).unwrap());
        assert_eq!(mode(), 0o600);
        let stored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored["aistupidlevel_api_key"], "dummy-a2");
        assert_eq!(stored["artificialanalysis_api_key"], "dummy-b");

        assert!(write_secret(&paths, "aistupidlevel_api_key", None).unwrap());
        let stored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(stored.as_object().unwrap().len(), 1);
        assert_eq!(stored["artificialanalysis_api_key"], "dummy-b");
        let leftovers: Vec<String> = fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
            .filter(|n| n.contains(".tmp-"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn validate_secret_value_accepts_one_token_and_strips_one_line_ending() {
        for raw in ["dummy-key\n", "dummy-key\r\n", "dummy-key"] {
            assert_eq!(validate_secret_value("k", raw).unwrap(), "dummy-key");
        }
        for raw in ["", "\n", "   \n"] {
            let msg = validate_secret_value("k", raw).unwrap_err().to_string();
            assert!(msg.contains("nothing was entered"), "{raw:?}: {msg}");
        }
        for raw in ["dummy key\n", "dummy\tkey"] {
            let msg = validate_secret_value("k", raw).unwrap_err().to_string();
            assert!(msg.contains("single token"), "{raw:?}: {msg}");
            assert!(!msg.contains("dummy"), "{msg}");
        }
        let long = format!("{}\n", "a".repeat(1025));
        let msg = validate_secret_value("k", &long).unwrap_err().to_string();
        assert!(msg.contains("1024 bytes"), "{msg}");
        assert!(!msg.contains("aaaa"), "{msg}");
    }

    #[test]
    fn stdin_text_must_be_exactly_one_line() {
        assert_eq!(read_secret_from("k", &b"dummy-1\n"[..]).unwrap(), "dummy-1");
        let msg = read_secret_from("k", &b"dummy-1\ndummy-2\n"[..]).unwrap_err().to_string();
        assert!(msg.contains("exactly one line"), "{msg}");
        assert!(!msg.contains("dummy"), "{msg}");
        let msg = read_secret_from("k", &b"dummy-1\n\n"[..]).unwrap_err().to_string();
        assert!(msg.contains("exactly one line"), "{msg}");
        assert!(!msg.contains("dummy"), "{msg}");
    }

    #[test]
    fn set_secret_collects_any_argv_values_instead_of_letting_clap_print_them() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from([
            "thoth", "config", "set-secret", "aistupidlevel-api-key", "-dummy", "dummy2",
        ])
        .unwrap();
        match cli.command {
            crate::cli::Command::Config { action: crate::cli::ConfigAction::SetSecret { key, value, json } } => {
                assert_eq!(key, "aistupidlevel-api-key");
                assert_eq!(value, ["-dummy", "dummy2"]);
                assert!(!json);
            }
            _ => panic!("parsed as something else"),
        }
        let cli = crate::cli::Cli::try_parse_from(["thoth", "config", "set-secret", "aistupidlevel-api-key", "--json"])
            .unwrap();
        match cli.command {
            crate::cli::Command::Config { action: crate::cli::ConfigAction::SetSecret { value, json, .. } } => {
                assert!(value.is_empty());
                assert!(json);
            }
            _ => panic!("parsed as something else"),
        }
    }

    #[test]
    fn the_parsed_command_line_cannot_be_printed_with_debug() {
        // `config set-secret` keeps what follows KEY (and KEY itself, where a
        // key may be pasted by mistake) in the parsed command line, so `Cli`,
        // `Command` and `ConfigAction` have no Debug that could print it.
        // Method lookup takes `Probe<T>`'s `is_debug` when T is Debug and
        // falls back to the one on `&Probe<T>` otherwise.
        use std::marker::PhantomData;
        struct Probe<T>(PhantomData<T>);
        trait IsDebug {
            fn is_debug(&self) -> bool {
                true
            }
        }
        impl<T: std::fmt::Debug> IsDebug for Probe<T> {}
        trait NotDebug {
            fn is_debug(&self) -> bool {
                false
            }
        }
        impl<T> NotDebug for &Probe<T> {}

        let records = &Probe::<crate::cli::RecordsAction>(PhantomData);
        assert!(records.is_debug(), "the probe must recognise a Debug type");
        let cli = &Probe::<crate::cli::Cli>(PhantomData);
        assert!(!cli.is_debug(), "Cli implements Debug");
        let command = &Probe::<crate::cli::Command>(PhantomData);
        assert!(!command.is_debug(), "Command implements Debug");
        let action = &Probe::<crate::cli::ConfigAction>(PhantomData);
        assert!(!action.is_debug(), "ConfigAction implements Debug");
    }
}
