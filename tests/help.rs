use assert_cmd::Command;
use predicates::prelude::*;
use std::path::{Path, PathBuf};

/// Every invocation in this suite gets its own throwaway `THOTH_HOME` and
/// the fake harness binary. Whole-branch review Important 2 (Global
/// Constraint: "every test sets THOTH_HOME to a fresh tempdir"): this file
/// used to build its `Command`s with neither env var set, so
/// `run_worker_is_still_invocable_even_though_it_is_hidden` resolved
/// `Paths::from_env()` to the operator's real `%USERPROFILE%\.thoth` and
/// created a `repos/<this-repo-id>/state.db` there before failing at id
/// resolution. The returned `TempDir` must be kept alive for as long as the
/// `Command` runs (its `Drop` removes the directory), so callers bind it
/// alongside the command rather than discarding it.
fn thoth() -> (Command, tempfile::TempDir) {
    let home = tempfile::tempdir().expect("tempdir");
    let mut c = Command::new(env!("CARGO_BIN_EXE_thoth"));
    // Same reason as `common::Fixture::cmd`: a suite run inside a thoth
    // worker must not inherit that worker's run id or inbox.
    for var in ["THOTH_RUN_ID", "THOTH_INBOX_DIR", "THOTH_EXE"] {
        c.env_remove(var);
    }
    c.env("THOTH_HOME", home.path());
    c.env("THOTH_CLAUDE_BIN", env!("CARGO_BIN_EXE_fake-claude"));
    (c, home)
}

/// Mirrors `config::Paths::from_env`'s fallback when `THOTH_HOME` is
/// unset, so the guard test below can locate the one directory none of these
/// tests may ever create.
fn real_thoth_home() -> PathBuf {
    let profile = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .expect("USERPROFILE or HOME must be set in this environment");
    PathBuf::from(profile).join(".thoth")
}

/// Same normalisation as `ids::repo_id_for`, duplicated because integration
/// tests are a separate crate and cannot reach into the binary's modules
/// (see the identical duplication in `tests/common/mod.rs::Fixture::repo_id`).
fn repo_id_for(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    let canon = std::fs::canonicalize(root).unwrap();
    let mut s = canon.to_string_lossy().to_string();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        s = stripped.to_string();
    }
    let s = s.replace('\\', "/").trim_end_matches('/').to_string();
    #[cfg(windows)]
    let s = s.to_lowercase();
    let hex: String = Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex[..12].to_string()
}

/// The directory the real, operator-owned `%USERPROFILE%\.thoth` would
/// gain for *this* repository if some command in this suite ever resolved
/// `THOTH_HOME` to the real default instead of the tempdir `thoth()`
/// hands it.
fn this_repos_real_state_dir() -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    real_thoth_home()
        .join("repos")
        .join(repo_id_for(root))
}

#[test]
fn every_documented_command_exists_and_run_worker_stays_hidden() {
    let (mut c, _home) = thoth();
    let out = c.arg("--help").assert().success().get_output().stdout.clone();
    let help = String::from_utf8(out).unwrap();
    for cmd in [
        // Phase 1
        "init", "profile", "task", "dispatch", "status", "events", "wait", "cancel", "resume",
        "submit",
        // Phase 2
        "evidence", "review", "verify", "decision", "config",
        // P7-1a
        "checks",
    ] {
        assert!(help.contains(cmd), "`{cmd}` missing from --help:\n{help}");
    }
    assert!(
        !help.contains("run-worker"),
        "run-worker must stay hidden:\n{help}"
    );
}

/// Every Phase 2 surface an operator can type must be in the Phase 2 guide, and
/// the one they must not type has to be marked internal.
#[test]
fn the_phase2_usage_doc_documents_every_new_command() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/phase2/usage.md"
    ))
    .expect("docs/phase2/usage.md must exist");
    for cmd in [
        "thoth evidence list",
        "thoth evidence show",
        "thoth review start",
        "thoth review show",
        "thoth task update-spec",
        "thoth decision add",
        "thoth verify",
        "thoth wait --run",
        "--allow-attempt",
        "thoth hook",
    ] {
        assert!(doc.contains(cmd), "docs/phase2/usage.md does not document `{cmd}`");
    }
    // v6 §2.5's five automatic conditions each have to be written down, because
    // an operator meeting `unverified` needs to know which one fired.
    for rule in ["rule 1", "rule 2", "rule 3", "rule 4", "rule 5"] {
        assert!(doc.contains(rule), "docs/phase2/usage.md does not explain `{rule}`");
    }
    // Open question 21's ruling: the trust marker's limit is documented, not
    // implied. An operator must not read `control_plane` as tamper-proof.
    assert!(
        doc.contains("## Known limitations"),
        "docs/phase2/usage.md must carry the Known limitations section"
    );
    assert!(
        doc.contains("protects against mistakes"),
        "the sandbox caveat on collected_by=control_plane must be spelled out"
    );
}

/// The Phase 1 guide has to stay honest about the two behaviours Phase 2
/// changed, or an operator will read a promise that is no longer true.
#[test]
fn the_phase1_usage_doc_was_updated_for_the_phase2_behaviour_changes() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/phase1/usage.md"
    ))
    .unwrap();
    assert!(
        doc.contains("reject"),
        "dispatch now accepts a rejected task; the Phase 1 entry-status list must say so"
    );
    assert!(
        doc.contains("runs.harness_bin"),
        "the cancel/THOTH_CLAUDE_BIN caveat is resolved and must name the column"
    );
    assert!(
        doc.contains("docs/phase2/usage.md"),
        "the Phase 1 guide must point at the Phase 2 one"
    );
}

#[test]
fn run_worker_is_still_invocable_even_though_it_is_hidden() {
    // it must fail for lack of a repo, not for being an unknown subcommand
    let (mut c, _home) = thoth();
    c.arg("run-worker")
        .arg("01NOSUCHRUN")
        .assert()
        .failure()
        .stderr(predicate::str::contains("unrecognized subcommand").not());
}

/// Linux, review finding 1: `run-worker --supervised` is the copy
/// `process::ensure_supervised` re-ran inside a systemd unit. Found outside a
/// runner unit anyway, it must refuse - not re-run itself again, which on a
/// host whose units do not land where thoth expects would nest units
/// without end. This test process is not in a runner unit, so it stands in
/// for that host.
#[test]
#[cfg(target_os = "linux")]
fn run_worker_supervised_outside_a_runner_unit_refuses_instead_of_rerunning() {
    let (mut c, _home) = thoth();
    c.args(["run-worker", "01NOSUCHRUN", "--supervised"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("refusing rather than re-running again"));
}

#[test]
fn the_usage_doc_mentions_every_command_in_the_help_output() {
    let doc = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/docs/phase1/usage.md"
    ))
    .expect("docs/phase1/usage.md must exist");
    for cmd in [
        "thoth init",
        "thoth profile add",
        "thoth profile list",
        "thoth profile check",
        "thoth task create",
        "thoth task show",
        "thoth dispatch",
        "thoth status",
        "thoth events",
        "thoth wait",
        "thoth cancel",
        "thoth resume",
        "thoth submit",
    ] {
        assert!(doc.contains(cmd), "usage.md does not document `{cmd}`");
    }
}

#[test]
fn no_doc_points_a_profile_at_the_operators_own_claude_config_dir() {
    // Global Constraint: never touch the operator's own ~/.claude. A worker writes into
    // whatever CLAUDE_CONFIG_DIR it is handed, so a `--config-dir` pointing there
    // would violate it on the first real dispatch.
    for rel in [
        "/docs/phase1/usage.md",
        "/docs/phase2/usage.md",
        "/README.md",
    ] {
        let path = format!("{}{}", env!("CARGO_MANIFEST_DIR"), rel);
        let doc = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
        for line in doc.lines() {
            if line.contains("--config-dir") {
                // `\.claude` is the Windows spelling, `/.claude` the Linux
                // one (`~/.claude`); the docs show both platforms' commands.
                let personal = line.contains(r"\.claude") || line.contains("/.claude");
                assert!(
                    !personal || line.contains(".thoth") || line.contains(".agentctl"),
                    "{rel} points --config-dir at a personal config dir: {line}"
                );
            }
        }
    }
}

/// Whole-branch review Important 2 / Global Constraint guard: this whole
/// suite must never create state under the operator's real
/// `%USERPROFILE%\.thoth`. Every `thoth()` command above is wired to a
/// tempdir `THOTH_HOME`; this proves that wiring actually holds by
/// checking the one directory a leak would create, both before (catching
/// residue from a previous bad run) and after running a representative
/// command through the exact same helper every other test in this file uses.
///
/// Thin adapter: Thoth is now also run on itself (the trial records in
/// `docs/trial/`), so the operator may legitimately have state for this very
/// repo. The guard therefore compares a snapshot of that directory (every
/// file's size and modification time, or its absence) from before and after
/// the command, instead of demanding that it does not exist. A leak that
/// creates it, or merely opens its `state.db` (SQLite touches the `-wal`/`-shm`
/// files), still fails. Running this suite while a real Thoth run on this repo
/// is live can change the snapshot too; don't.
#[test]
fn this_suite_never_creates_a_state_dir_in_the_operators_real_thoth_home() {
    let dir = this_repos_real_state_dir();
    let before = snapshot_files(&dir);

    let (mut c, _home) = thoth();
    c.arg("run-worker").arg("01NOSUCHRUN").assert().failure();

    assert_eq!(
        before,
        snapshot_files(&dir),
        "{} was created or modified by this suite - THOTH_HOME leaked to the \
         operator's real home instead of the tempdir `thoth()` set",
        dir.display()
    );
}

/// Every file under `dir` with its size and modification time, sorted; `None`
/// when `dir` does not exist.
fn snapshot_files(dir: &Path) -> Option<Vec<(PathBuf, u64, std::time::SystemTime)>> {
    if !dir.exists() {
        return None;
    }
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(d) = pending.pop() {
        for entry in std::fs::read_dir(&d).expect("read_dir") {
            let entry = entry.expect("dir entry");
            let meta = entry.metadata().expect("metadata");
            if meta.is_dir() {
                pending.push(entry.path());
            } else {
                files.push((entry.path(), meta.len(), meta.modified().expect("mtime")));
            }
        }
    }
    files.sort();
    Some(files)
}

/// Global Constraint: `thoth hook` never prints to stdout and always exits
/// 0, even when it has nothing to work with. Both halves matter — Claude Code
/// parses a hook's stdout as a decision about the tool call, and a non-zero
/// hook can change what the worker sees.
#[test]
fn the_hook_subcommand_is_hidden_silent_and_always_succeeds() {
    let (mut c, _home) = thoth();
    let out = c.arg("--help").assert().success().get_output().stdout.clone();
    let help = String::from_utf8(out).unwrap();
    // Match the shape of a subcommand listing, not the substring: `--help` also
    // contains every subcommand's one-line description, and a future description
    // that merely mentions a hook must not turn this red (L8).
    assert!(
        !help.lines().any(|l| l.trim_start().starts_with("hook")),
        "hook must stay hidden:\n{help}"
    );

    // no THOTH_INBOX_DIR, no THOTH_RUN_ID, garbage on stdin
    let (mut c, _home) = thoth();
    c.arg("hook")
        .write_stdin("not json at all")
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    // a real payload, but still no inbox: still silent, still 0
    let (mut c, home) = thoth();
    c.arg("hook")
        .write_stdin(r#"{"hook_event_name":"PostToolUse","tool_name":"Bash"}"#)
        .assert()
        .success()
        .stdout(predicate::str::is_empty());

    // Global Constraint, pinned rather than left to code review: `hook` must not
    // open a database. `thoth()` points THOTH_HOME at a fresh empty
    // tempdir, and every other command creates `global.db` there on its first
    // run — so if anything appears under it, the hook opened something it must
    // not have.
    let leaked: Vec<String> = std::fs::read_dir(home.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().to_string())
        .collect();
    assert!(
        leaked.is_empty(),
        "hook must not open a database or create state: found {leaked:?}"
    );
}
