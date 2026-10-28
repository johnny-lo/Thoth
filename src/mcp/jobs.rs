//! Background jobs for the MCP server: one `thoth verify` child per job, with
//! its record on disk under `<THOTH_HOME>/mcp/jobs/<job_id>/` so a restarted
//! server can still report it.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use ulid::Ulid;

const HINT: &str = "This server did not start this job and it has no result yet. The job may still be running under another server, or that server may be gone. Check thoth_evidence_list for evidence labelled `verify: ` first; start the job again only if there is none.";

/// The jobs this server process started. `Some(child)` while the child has
/// not been seen to exit, `None` once `exit.json` is written. Dropping a
/// `Child` neither kills nor waits for it, so a job outlives the server.
#[derive(Default)]
pub struct Jobs {
    own: HashMap<String, Option<Child>>,
}

impl Jobs {
    /// Whether this server process started the job.
    pub fn owns(&self, job_id: &str) -> bool {
        self.own.contains_key(job_id)
    }
}

fn jobs_dir(home: &Path) -> PathBuf {
    home.join("mcp").join("jobs")
}

pub fn start(
    jobs: &mut Jobs,
    home: &Path,
    argv: &[String],
    repo: &Path,
    run: &str,
) -> Result<Value, String> {
    let program = std::env::current_exe().map_err(|e| e.to_string())?;
    start_with(jobs, home, argv, repo, run, &program)
}

fn start_with(
    jobs: &mut Jobs,
    home: &Path,
    argv: &[String],
    repo: &Path,
    run: &str,
    program: &Path,
) -> Result<Value, String> {
    let job_id = Ulid::generate().to_string();
    let dir = jobs_dir(home).join(&job_id);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let started_at = crate::ids::now_rfc3339();
    // meta.json is on disk before the child exists, so a server that dies
    // right after the spawn still leaves a job a later server can report.
    let mut meta = json!({
        "job_id": job_id,
        "kind": "verify",
        "argv": argv,
        "repo": repo,
        "run": run,
        "started_at": started_at,
        "pid": null,
    });
    if let Err(e) = write_atomic(&dir, "meta.json", meta.to_string().as_bytes()) {
        let _ = fs::remove_dir_all(&dir);
        return Err(e.to_string());
    }
    // The closure owns the Command and both file handles, so they are closed
    // before the directory is removed on failure (Windows refuses otherwise).
    let spawn = || -> io::Result<Child> {
        let out = fs::File::create(dir.join("stdout.json"))?;
        let err = fs::File::create(dir.join("stderr.txt"))?;
        let mut c = Command::new(program);
        // Only thoth_checks_approve's child names the commander's MCP call;
        // the checks verify runs must not inherit that from the server.
        c.args(argv)
            .current_dir(repo)
            .env_remove(super::VIA_MCP_ENV)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        crate::process::quiet(&mut c);
        // Windows hands every inheritable handle to the child, so the
        // client's pipes would stay open until the whole verify tree ends.
        #[cfg(windows)]
        let saved = crate::process::windows::clear_std_handle_inheritance();
        let spawned = c.spawn();
        #[cfg(windows)]
        crate::process::windows::restore_std_handle_inheritance(saved);
        spawned
    };
    let child = match spawn() {
        Ok(c) => c,
        Err(e) => {
            let _ = fs::remove_dir_all(&dir);
            return Err(e.to_string());
        }
    };
    meta["pid"] = json!(child.id());
    jobs.own.insert(job_id.clone(), Some(child));
    // The child is running and owned, so the client must get its job id.
    // A failed rewrite leaves the pid null, which is harmless: `status`
    // never reads the pid.
    let _ = write_atomic(&dir, "meta.json", meta.to_string().as_bytes());
    Ok(json!({
        "ok": true,
        "job_id": job_id,
        "kind": "verify",
        "status": "working",
        "run": run,
        "poll_tool": "thoth_job_status",
        "suggested_poll_seconds": 15,
    }))
}

pub fn status(
    jobs: &mut Jobs,
    home: &Path,
    job_id: &str,
    wait_seconds: u64,
) -> Result<Value, String> {
    // The id is checked before it is joined to a path, so nothing outside the
    // jobs directory is ever read.
    if job_id.len() != 26
        || !job_id
            .bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase())
    {
        return Err("parameter job_id must be 26 digits and upper-case letters".into());
    }
    let dir = jobs_dir(home).join(job_id);
    if !dir.is_dir() {
        return Err(format!("no job {job_id}"));
    }
    let meta = read_object(&dir.join("meta.json")).unwrap_or_else(|| json!({}));
    let kind = meta.get("kind").cloned().unwrap_or_else(|| json!("verify"));
    let started_at = meta.get("started_at").cloned().unwrap_or(Value::Null);
    let mut recovered = true;
    if let Some(slot) = jobs.own.get_mut(job_id) {
        recovered = false;
        if let Some(child) = slot {
            let deadline = Instant::now() + Duration::from_secs(wait_seconds);
            let exit_code = loop {
                match child.try_wait() {
                    Ok(Some(s)) => break Some(s.code()),
                    Ok(None) => {}
                    Err(e) => return Err(e.to_string()),
                }
                if Instant::now() >= deadline {
                    break None;
                }
                std::thread::sleep(Duration::from_millis(200));
            };
            let Some(exit_code) = exit_code else {
                return Ok(json!({
                    "ok": true,
                    "job_id": job_id,
                    "kind": kind,
                    "status": "working",
                    "exit_code": null,
                    "result": null,
                    "stderr_tail": stderr_tail(&dir.join("stderr.txt")),
                    "started_at": started_at,
                    "finished_at": null,
                    "recovered": false,
                }));
            };
            let exit = json!({"exit_code": exit_code, "finished_at": crate::ids::now_rfc3339()});
            write_atomic(&dir, "exit.json", exit.to_string().as_bytes())
                .map_err(|e| e.to_string())?;
            *slot = None;
        }
    }
    let exit = read_object(&dir.join("exit.json"));
    let result = read_object(&dir.join("stdout.json"));
    let status = match (&exit, &result) {
        (_, Some(_)) => "completed",
        (Some(_), None) => "failed",
        (None, None) => "unknown",
    };
    let field = |name: &str| {
        exit.as_ref()
            .and_then(|e| e.get(name))
            .cloned()
            .unwrap_or(Value::Null)
    };
    let mut doc = json!({
        "ok": true,
        "job_id": job_id,
        "kind": kind,
        "status": status,
        "exit_code": field("exit_code"),
        "result": result,
        "stderr_tail": stderr_tail(&dir.join("stderr.txt")),
        "started_at": started_at,
        "finished_at": field("finished_at"),
        "recovered": recovered,
    });
    if status == "unknown" {
        doc["hint"] = json!(HINT);
    }
    Ok(doc)
}

/// Writes `dir/name` through a temporary file in the same directory and a
/// rename, so a reader in another server sees the old file or the whole new
/// one, never half of it. The temporary file is removed when either step fails.
fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    let target = dir.join(name);
    let tmp = dir.join(format!("{name}.tmp-{}", std::process::id()));
    let written = fs::write(&tmp, bytes).and_then(|()| fs::rename(&tmp, &target));
    if written.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    written
}

fn read_object(p: &Path) -> Option<Value> {
    serde_json::from_slice::<Value>(&fs::read(p).ok()?)
        .ok()
        .filter(Value::is_object)
}

/// The last 20 lines of the file, at most 4000 bytes.
fn stderr_tail(p: &Path) -> String {
    let bytes = fs::read(p).unwrap_or_default();
    tail(&String::from_utf8_lossy(&bytes))
}

fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let s = lines[lines.len().saturating_sub(20)..].join("\n");
    let mut start = s.len().saturating_sub(4000);
    while !s.is_char_boundary(start) {
        start += 1
    }
    s[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tail_keeps_the_last_twenty_lines_and_at_most_4000_bytes() {
        let text = (1..=30).map(|n| format!("line {n}")).collect::<Vec<_>>().join("\n");
        let t = tail(&text);
        assert_eq!(t.lines().count(), 20);
        assert!(t.starts_with("line 11"));
        assert!(t.ends_with("line 30"));
        // 3-byte characters: 4000 is not a multiple of 3, so the cut lands
        // inside a character and has to move forward to a boundary.
        let long = "中".repeat(5000);
        let t = tail(&long);
        assert!(t.len() <= 4000);
        assert!(!t.is_empty() && t.chars().all(|c| c == '中'));
        assert_eq!(tail(""), "");
    }

    #[test]
    fn a_malformed_job_id_is_refused_before_any_path_is_built() {
        let home = tempfile::tempdir().unwrap();
        let mut jobs = Jobs::default();
        for bad in ["../x", "01m3v52qvxhxm8ysvext3zbs5q", "", "01M3V52QVXHXM8YSVEXT3ZBS5"] {
            assert!(status(&mut jobs, home.path(), bad, 0).is_err(), "{bad}");
        }
        assert!(status(&mut jobs, home.path(), "01M3V52QVXHXM8YSVEXT3ZBS5Q", 0).is_err());
    }

    fn names(dir: &Path) -> std::collections::BTreeSet<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    fn set(items: &[&str]) -> std::collections::BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn jobs_write_atomic_replaces_the_target_and_leaves_no_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let first = br#"{"exit_code":0}"#;
        write_atomic(d.path(), "exit.json", first).unwrap();
        assert_eq!(fs::read(d.path().join("exit.json")).unwrap(), first);
        // An existing target is replaced, on Windows too.
        let second = br#"{"exit_code":1}"#;
        write_atomic(d.path(), "exit.json", second).unwrap();
        assert_eq!(fs::read(d.path().join("exit.json")).unwrap(), second);
        assert_eq!(names(d.path()), set(&["exit.json"]));
    }

    #[test]
    fn jobs_write_atomic_onto_a_directory_fails_and_leaves_no_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("exit.json");
        fs::create_dir(&target).unwrap();
        fs::write(target.join("keep.txt"), "x").unwrap();
        assert!(write_atomic(d.path(), "exit.json", b"{}").is_err());
        assert_eq!(names(d.path()), set(&["exit.json"]));
        assert!(target.is_dir());
        assert_eq!(fs::read_to_string(target.join("keep.txt")).unwrap(), "x");
    }

    #[test]
    fn jobs_start_writes_meta_with_the_pid_and_status_reaps_the_job() {
        let home = tempfile::tempdir().unwrap();
        let repo = tempfile::tempdir().unwrap();
        let mut jobs = Jobs::default();
        let argv = vec!["--version".to_string()];
        let doc = start_with(&mut jobs, home.path(), &argv, repo.path(), "R1", Path::new("git"))
            .unwrap();
        assert_eq!(doc["ok"], json!(true));
        assert_eq!(doc["kind"], json!("verify"));
        assert_eq!(doc["status"], json!("working"));
        assert_eq!(doc["run"], json!("R1"));
        assert_eq!(doc["poll_tool"], json!("thoth_job_status"));
        assert_eq!(doc["suggested_poll_seconds"], json!(15));
        let job_id = doc["job_id"].as_str().unwrap().to_string();
        assert_eq!(job_id.len(), 26);

        let dir = jobs_dir(home.path()).join(&job_id);
        let meta = read_object(&dir.join("meta.json")).unwrap();
        let keys: std::collections::BTreeSet<String> =
            meta.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            set(&["job_id", "kind", "argv", "repo", "run", "started_at", "pid"])
        );
        assert!(meta["pid"].is_u64() && meta["pid"].as_u64().unwrap() > 0);
        assert_eq!(meta["argv"], json!(["--version"]));
        assert_eq!(meta["run"], json!("R1"));
        assert!(meta["started_at"].is_string());
        assert_eq!(names(&dir), set(&["meta.json", "stdout.json", "stderr.txt"]));

        let s = status(&mut jobs, home.path(), &job_id, 30).unwrap();
        assert_eq!(s["exit_code"], json!(0));
        // exit.json is there and stdout.json is not a JSON document.
        assert_eq!(s["status"], json!("failed"));
        assert_eq!(s["started_at"], meta["started_at"]);
        assert_eq!(s["recovered"], json!(false));
        let exit = read_object(&dir.join("exit.json")).unwrap();
        assert_eq!(exit["exit_code"], json!(0));
        assert_eq!(
            names(&dir),
            set(&["meta.json", "stdout.json", "stderr.txt", "exit.json"])
        );
    }

    #[test]
    fn jobs_start_with_a_program_that_cannot_start_leaves_nothing_behind() {
        let home = tempfile::tempdir().unwrap();
        let mut jobs = Jobs::default();
        let argv = vec!["--version".to_string()];
        let program = home.path().join("no-such-program");
        let r = start_with(&mut jobs, home.path(), &argv, home.path(), "R1", &program);
        assert!(r.is_err());
        // The meta.json written before the spawn went with the job directory.
        assert!(names(&jobs_dir(home.path())).is_empty());
        assert!(jobs.own.is_empty());
    }
}
