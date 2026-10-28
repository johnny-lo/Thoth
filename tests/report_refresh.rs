mod common;

use common::Fixture;
use serde_json::Value;
use std::fs;
use std::path::PathBuf;

fn json(out: &[u8]) -> Value {
    serde_json::from_slice(out).expect("valid json")
}

fn report_path(f: &Fixture) -> PathBuf {
    f.home_path().join("report.html")
}

/// `task create --title TITLE --json`, returning the task id.
fn create_titled(f: &Fixture, title: &str) -> String {
    let spec_file = f.home_path().join("refresh-spec.json");
    fs::write(&spec_file, Fixture::SPEC).unwrap();
    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&spec_file)
        .args(["--title", title, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    json(&out)["task_id"].as_str().unwrap().to_string()
}

/// The markup `thoth report` writes for one task row: the status pill,
/// then the title.
fn row(label: &str, class: &str, title: &str) -> String {
    format!("<span class=\"pill {class}\">{label}</span><div><p class=\"ttitle\">{title}</p>")
}

#[test]
fn no_report_file_means_none_is_created() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    assert!(!report_path(&f).exists());
    create_titled(&f, "no page please");
    assert!(!report_path(&f).exists(), "task create created report.html");
}

#[test]
fn an_existing_report_is_rewritten_by_task_create_and_task_close() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.cmd().args(["report", "--json"]).assert().success();
    assert!(report_path(&f).is_file());

    let task_id = create_titled(&f, "auto refresh title");
    let page = fs::read_to_string(report_path(&f)).unwrap();
    assert!(page.contains("auto refresh title"), "{page}");

    f.state_conn()
        .execute("UPDATE tasks SET status = 'failed' WHERE task_id = ?1", [&task_id])
        .unwrap();
    f.cmd()
        .args(["task", "close", &task_id, "--reason", "gone", "--json"])
        .assert()
        .success();
    let page = fs::read_to_string(report_path(&f)).unwrap();
    assert!(page.contains(&row("已取消", "muted", "auto refresh title")), "{page}");
}

#[test]
fn the_runner_rewrites_the_report_after_it_settles_a_run() {
    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.add_profile("claude-b");
    let task_id = create_titled(&f, "runner refresh title");
    f.cmd().args(["report", "--json"]).assert().success();

    f.dispatch(&task_id, "claude-b", "ok", &[]);
    let (_, status) = f.wait_for(&task_id, "60s");
    assert_eq!(status, "in_review");

    let want = row("等審查", "wait", "runner refresh title");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let page = fs::read_to_string(report_path(&f)).unwrap_or_default();
        if page.contains(&want) {
            break;
        }
        if std::time::Instant::now() >= deadline {
            panic!("report.html never showed {want}\n{page}");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

#[cfg(unix)]
#[test]
fn a_failed_rebuild_changes_nothing_about_the_command() {
    use std::os::unix::fs::PermissionsExt;

    let f = common::fixture();
    f.cmd().arg("init").assert().success();
    f.cmd().args(["report", "--json"]).assert().success();
    let before = fs::read(report_path(&f)).unwrap();

    // A second registered repo whose state.db cannot be opened: the report
    // rebuild fails on it, while the fixture repo's own writes are unaffected.
    let other = tempfile::tempdir().unwrap();
    common::git(other.path(), &["init", "--initial-branch=main"]);
    common::git(other.path(), &["config", "user.email", "test@example.com"]);
    common::git(other.path(), &["config", "user.name", "thoth test"]);
    fs::write(other.path().join("o.txt"), "o\n").unwrap();
    common::git(other.path(), &["add", "o.txt"]);
    common::git(other.path(), &["commit", "-m", "first"]);
    f.cmd_in(other.path()).arg("init").assert().success();
    let other_db = {
        let g = f.global_conn();
        let mut s = g.prepare("SELECT repo_id FROM repos").unwrap();
        let ids = s
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        ids.into_iter()
            .map(|id| f.home_path().join("repos").join(id).join("state.db"))
            .find(|p| *p != f.state_db())
            .unwrap()
    };
    fs::set_permissions(&other_db, fs::Permissions::from_mode(0o000)).unwrap();

    let spec_file = f.home_path().join("refresh-spec.json");
    fs::write(&spec_file, Fixture::SPEC).unwrap();
    let out = f
        .cmd()
        .args(["task", "create", "-f"])
        .arg(&spec_file)
        .args(["--title", "rebuild fails", "--json"])
        .output()
        .unwrap();
    // The same rebuild run by hand fails, so the check below is not vacuous.
    let manual = f
        .cmd()
        .args(["report", "--out"])
        .arg(f.home_path().join("manual.html"))
        .output()
        .unwrap();

    fs::set_permissions(&other_db, fs::Permissions::from_mode(0o644)).unwrap();

    assert_eq!(manual.status.code(), Some(1), "report should fail on the unreadable state.db");

    assert_eq!(out.status.code(), Some(0), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(out.stderr.is_empty(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let v = json(&out.stdout);
    assert_eq!(v["ok"], true);
    assert_eq!(fs::read(report_path(&f)).unwrap(), before, "report.html changed");
}
