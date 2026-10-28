//! Test double for the `curl` Thoth's score fetches start (RJ-2, tests/arena.rs):
//! copied onto a PATH of its own as `curl`, it records every argument list
//! `thoth models --refresh` hands it and serves `file://` URLs the way curl
//! does, so the real fetch code runs end to end against the tests' file trees
//! while the test reads what curl was asked to do. Nothing here reaches a
//! network: any other scheme fails like an unresolved host.
//!
//! `FAKE_CURL_LOG=<path>`, when set, gets one JSON line per start,
//! `{"args": [...], "stdin_bytes": N}`: `args` is argv without the program
//! name; `stdin_bytes` is how many bytes were read from stdin, which is read
//! to its end when `-H @-` is among the arguments (the header of a keyed
//! request), as curl does. The header's content is never written anywhere.
//!
//! Serving: the last argument is the URL. A `file://` URL's file is copied to
//! the `-o` target (printed to stdout without one) and the exit code is 0; a
//! file that cannot be read exits 37 (curl's "couldn't read a file:// file");
//! a URL of any other scheme exits 6 ("could not resolve host"). Every other
//! option (`-sS`, `--fail`, `-L`, `--max-time N`) is accepted and ignored.

use std::io::{Read, Write};
use std::path::PathBuf;

/// The path a `file://` URL names, once the scheme is gone: `/C:/x` is the
/// Windows path `C:/x`, `/home/x` is itself.
fn file_path(rest: &str) -> PathBuf {
    let bytes = rest.as_bytes();
    if bytes.len() >= 3 && bytes[0] == b'/' && bytes[1].is_ascii_alphabetic() && bytes[2] == b':' {
        PathBuf::from(&rest[1..])
    } else {
        PathBuf::from(rest)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut stdin_bytes = 0usize;
    if args.iter().any(|a| a == "@-") {
        let mut header = Vec::new();
        let _ = std::io::stdin().read_to_end(&mut header);
        stdin_bytes = header.len();
    }
    if let Ok(log) = std::env::var("FAKE_CURL_LOG") {
        let line = serde_json::json!({"args": args, "stdin_bytes": stdin_bytes});
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
            let _ = writeln!(file, "{line}");
        }
    }
    let target: Option<PathBuf> = args
        .iter()
        .position(|a| a == "-o")
        .and_then(|at| args.get(at + 1))
        .map(PathBuf::from);
    let Some(url) = args.last() else {
        eprintln!("curl: no URL specified");
        std::process::exit(2);
    };
    let Some(rest) = url.strip_prefix("file://") else {
        eprintln!("curl: (6) Could not resolve host: {url} (fake-curl serves file:// only)");
        std::process::exit(6);
    };
    let path = file_path(rest);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!("curl: (37) Couldn't open file {}: {e}", path.display());
            std::process::exit(37);
        }
    };
    match target {
        Some(target) => {
            if let Err(e) = std::fs::write(&target, &bytes) {
                eprintln!("curl: (23) Failure writing output to destination {}: {e}", target.display());
                std::process::exit(23);
            }
        }
        None => {
            let _ = std::io::stdout().write_all(&bytes);
        }
    }
}
