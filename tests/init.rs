mod common;

use predicates::prelude::*;

#[test]
fn init_creates_the_repo_state_dir_and_registers_the_repo() {
    let f = common::fixture();
    f.cmd()
        .args(["init", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"ok\":true"))
        .stdout(predicate::str::contains(f.repo_id()));

    assert!(f.state_db().is_file(), "state.db not created");
    let repo_dir = f.home_path().join("repos").join(f.repo_id());
    assert!(repo_dir.join("worktrees").is_dir());
    assert!(repo_dir.join("runs").is_dir());
    assert!(repo_dir.join("blobs").is_dir());

    let sconn = f.state_conn();
    let root: String = sconn
        .query_row("SELECT value FROM meta WHERE key='repo_root'", [], |r| r.get(0))
        .unwrap();
    assert!(!root.is_empty());
    let ver: String = sconn
        .query_row("SELECT value FROM meta WHERE key='schema_version'", [], |r| r.get(0))
        .unwrap();
    // Phase 2 (OQ10): `init` now writes `STATE_SCHEMA_VERSION`, which is 6 since `check_approvals` (LESS-1).
    assert_eq!(ver, "6");

    let gconn = f.global_conn();
    let n: i64 = gconn
        .query_row(
            "SELECT COUNT(*) FROM repos WHERE repo_id = ?1",
            [f.repo_id()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn init_is_idempotent() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.cmd().arg("init").assert().success();
    let gconn = f.global_conn();
    let n: i64 = gconn
        .query_row("SELECT COUNT(*) FROM repos", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "init must not duplicate the repos row");
}

#[test]
fn init_outside_a_git_repo_fails() {
    let f = common::fixture();
    let elsewhere = tempfile::tempdir().unwrap();
    let mut c = assert_cmd::Command::new(env!("CARGO_BIN_EXE_thoth"));
    c.env("THOTH_HOME", f.home_path())
        .current_dir(elsewhere.path())
        .arg("init")
        .assert()
        .failure()
        .stderr(predicate::str::contains("not inside a git repository"));
}
