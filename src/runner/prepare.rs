use crate::adapter::{claude_code, HarnessAdapter};
use crate::config::Paths;
use crate::db;
use crate::db::global;
use crate::db::state;
use crate::evidence;
use crate::spec::TaskSpec;
use anyhow::{anyhow, Result};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

/// What the first section of `run_worker_inner` set up, in the order it
/// declared it: everything the later sections read.
pub(super) struct RunPrep {
    pub(super) gconn: Connection,
    pub(super) profile: global::Profile,
    pub(super) is_codex: bool,
    pub(super) adapter: Box<dyn HarnessAdapter>,
    pub(super) spec: TaskSpec,
    pub(super) masker: crate::redact::Masker,
    pub(super) run_dir: PathBuf,
    pub(super) stream_path: PathBuf,
    pub(super) stderr_path: PathBuf,
    pub(super) prompt_path: PathBuf,
    pub(super) is_spec_review: bool,
    pub(super) is_review: bool,
    pub(super) research: bool,
    pub(super) inbox_dir: PathBuf,
    pub(super) thoth_exe: PathBuf,
    pub(super) extra_env: Vec<(String, String)>,
    pub(super) hooked: bool,
    pub(super) settings_path: Option<PathBuf>,
}

/// The addenda the spec stored beside itself in this run's own copy; only the
/// worker prompt uses them.
pub(super) struct PromptAddenda {
    pub(super) rework: Option<crate::spec::ReworkAddendum>,
    pub(super) reroute: Option<crate::spec::RerouteAddendum>,
    pub(super) handoff: Option<crate::handoff::HandoffRef>,
}

pub(super) fn prepare_run(
    paths: &Paths,
    conn: &Connection,
    run: &state::Run,
) -> Result<(RunPrep, PromptAddenda)> {
    let gconn = db::open_global(paths)?;
    // Existence check only: `run.config_dir` (not `profile.config_dir`) is what
    // actually gets used below, precisely so the runner does not have to trust
    // that the profile row still points at this run's session directory.
    //
    // Its harness, though, chooses the adapter.
    let profile = global::get_profile(&gconn, &run.account)?.ok_or_else(|| {
        anyhow!("profile {} is gone; cannot launch run {}", run.account, run.run_id)
    })?;
    let is_codex = profile.harness == crate::adapter::HARNESS_CODEX;
    let adapter = crate::adapter::for_harness(&profile.harness);
    let spec: TaskSpec = crate::spec::parse_spec(&run.spec)?;
    // Task 8: a rework dispatch stored the reviewer's findings alongside the
    // spec in this run's own copy. `None` for every other run.
    let rework = crate::spec::parse_rework(&run.spec);
    let reroute = crate::spec::parse_reroute(&run.spec);
    // S3-9: a dispatch that continued a stopped run names the handoff file.
    let handoff = crate::handoff::parse_handoff(&run.spec);
    // RD-1: everything this run hands its worker or reviewer is masked with
    // the environment this runner was started with, which is the
    // commander's at dispatch time.
    let masker = crate::redact::Masker::load(Some(&paths.home));

    let run_dir = PathBuf::from(&run.run_dir);
    std::fs::create_dir_all(&run_dir)?;
    let stream_path = run_dir.join("stream.jsonl");
    let stderr_path = run_dir.join("stderr.log");
    let prompt_path = run_dir.join("prompt.txt");

    // brief §1 and Open question 4: only a worker run is hooked. Hooking the
    // reviewer would mint new evidence codes for the very task it is citing,
    // and a reviewer's `cat` is not evidence about the implementation.
    // SR-2: a spec reviewer is a reviewer in every branch below, except for
    // its input, its report and the task status it never writes.
    let is_spec_review = run.role == crate::model::role::SPEC_REVIEWER;
    let is_review = run.role == crate::model::role::REVIEWER || is_spec_review;
    // Spec reviewers never need the web, including research specifications.
    let research = !is_spec_review && crate::spec::is_research_task(&run.spec);
    let inbox_dir = run_dir.join("inbox");
    let thoth_exe = std::env::current_exe()?;
    let mut extra_env: Vec<(String, String)> = vec![
        // `evidence show` reads this to attribute an `evidence_opened` event,
        // so every run gets it, reviewer included.
        ("THOTH_RUN_ID".to_string(), run.run_id.clone()),
        // So a worker or reviewer can invoke thoth without a PATH assumption.
        ("THOTH_EXE".to_string(), thoth_exe.display().to_string()),
    ];
    // Only a harness whose commands come through `thoth hook` is hooked:
    // codex's commands, with their real exit codes, arrive on the stream
    // (scope §2.3), so it gets no settings and no inbox.
    let hooked = !is_review && adapter.hooks_through_inbox();
    if !is_review {
        // M8 (fix round 1): the worker prompt's Evidence section names this
        // file unconditionally ("The file .agent/evidence-index.json ...
        // lists the captured evidence"), so it must exist before the child
        // ever reads it - not just after the first row lands. `refresh_index`
        // (from the DB) rather than a hardcoded `&[]`: a *resumed* run's
        // inbox tailer already sits past the offset that produced its
        // earlier rows, so nothing will call `refresh_index` again until (if
        // ever) a genuinely new command is captured - writing an empty array
        // here would silently blank out an already-populated index for the
        // rest of the run.
        evidence::refresh_index(conn, &run.task_id, &PathBuf::from(&run.worktree))?;
    }
    let settings_path = if !hooked {
        None
    } else {
        std::fs::create_dir_all(inbox_dir.join(crate::commands::hook::BLOBS_DIR))?;
        extra_env.push((
            "THOTH_INBOX_DIR".to_string(),
            inbox_dir.display().to_string(),
        ));
        Some(claude_code::write_hooks_settings(&run_dir, &thoth_exe)?)
    };
    // Whatever the harness itself needs in the worktree, written fresh on
    // dispatch and resume, reviewer included.
    adapter.prepare_launch(run, &run_dir, &thoth_exe)?;
    if is_review {
        // The reviewer has no `Write` tool and creates `.agent/review.json`
        // with a shell redirect, which cannot create a missing directory.
        std::fs::create_dir_all(Path::new(&run.worktree).join(".agent"))?;
    }
    Ok((
        RunPrep {
            gconn,
            profile,
            is_codex,
            adapter,
            spec,
            masker,
            run_dir,
            stream_path,
            stderr_path,
            prompt_path,
            is_spec_review,
            is_review,
            research,
            inbox_dir,
            thoth_exe,
            extra_env,
            hooked,
            settings_path,
        },
        PromptAddenda { rework, reroute, handoff },
    ))
}
