//! Test double for an approved check (AUTO-1a's pipeline tests). Never used in
//! production: tests register it with `thoth checks add ID --cmd "PATH MODE ..."`.
//!
//! Modes:
//! - `env`: prints `CARGO_BUILD_JOBS=...` and `RUST_TEST_THREADS=...` (the
//!   value, or `<unset>`), and nothing else; exit 0.
//! - `exit N`: exits N.
//! - `gate NAME [EXIT]`: appends `start NAME PID MS` to `$FAKE_CHECK_DIR/NAME.log`
//!   (MS: milliseconds since the Unix epoch), waits up to 300 s for
//!   `$FAKE_CHECK_DIR/NAME.go`, appends `end NAME PID MS` and exits EXIT
//!   (default 0).
//! - `seq NAME C1 C2 ...`: counts its calls in `$FAKE_CHECK_DIR/NAME.count`
//!   and exits with code k of the list on call k (the last one past its end).
//! - `priority`: prints the process priority class, `below_normal`, `normal`
//!   or `other` (Windows), or `unknown` elsewhere; exit 0.
//!
//! A mode that needs `FAKE_CHECK_DIR` without it set exits 70.

use std::io::Write;

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn dir() -> std::path::PathBuf {
    match std::env::var_os("FAKE_CHECK_DIR") {
        Some(d) => std::path::PathBuf::from(d),
        None => {
            eprintln!("fake-check: FAKE_CHECK_DIR is not set");
            std::process::exit(70);
        }
    }
}

fn append(path: &std::path::Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

#[cfg(windows)]
fn priority() -> &'static str {
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetPriorityClass, BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    };
    // SAFETY: the pseudo-handle of the current process is always valid.
    let class = unsafe { GetPriorityClass(GetCurrentProcess()) };
    if class == BELOW_NORMAL_PRIORITY_CLASS {
        "below_normal"
    } else if class == NORMAL_PRIORITY_CLASS {
        "normal"
    } else {
        "other"
    }
}

#[cfg(not(windows))]
fn priority() -> &'static str {
    "unknown"
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("env") => {
            for name in ["CARGO_BUILD_JOBS", "RUST_TEST_THREADS"] {
                let v = std::env::var(name).unwrap_or_else(|_| "<unset>".to_string());
                println!("{name}={v}");
            }
        }
        Some("exit") => {
            let code = args.get(1).and_then(|c| c.parse().ok()).unwrap_or(1);
            std::process::exit(code);
        }
        Some("gate") => {
            let name = args.get(1).cloned().unwrap_or_else(|| "gate".to_string());
            let code: i32 = args.get(2).and_then(|c| c.parse().ok()).unwrap_or(0);
            let d = dir();
            let log = d.join(format!("{name}.log"));
            let pid = std::process::id();
            append(&log, &format!("start {name} {pid} {}", now_ms()));
            println!("waiting for {name}.go");
            let go = d.join(format!("{name}.go"));
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(300);
            while !go.exists() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            append(&log, &format!("end {name} {pid} {}", now_ms()));
            std::process::exit(code);
        }
        Some("seq") => {
            let name = args.get(1).cloned().unwrap_or_else(|| "seq".to_string());
            let codes: Vec<i32> = args.iter().skip(2).filter_map(|c| c.parse().ok()).collect();
            let counter = dir().join(format!("{name}.count"));
            let n: usize = std::fs::read_to_string(&counter)
                .ok()
                .and_then(|t| t.trim().parse().ok())
                .unwrap_or(0)
                + 1;
            let _ = std::fs::write(&counter, n.to_string());
            let code = codes.get(n - 1).or(codes.last()).copied().unwrap_or(0);
            println!("{name} call {n} exits {code}");
            std::process::exit(code);
        }
        Some("priority") => println!("{}", priority()),
        _ => {
            eprintln!("fake-check: unknown mode {args:?}");
            std::process::exit(64);
        }
    }
}
