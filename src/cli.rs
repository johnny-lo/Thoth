use clap::{Parser, Subcommand};

/// `dispatch --allowed-tools`'s default: Claude Code's tool names. An adapter
/// with its own tool names may map exactly this string to its own list
/// (`HarnessAdapter::implementer_allowed_tools`).
pub const DEFAULT_ALLOWED_TOOLS: &str = "Bash,Read,Write,Edit,Glob,Grep";

// No Debug here, on `Command` or on `ConfigAction`: the parsed command line
// can hold an API key pasted after (or instead of) KEY in `config set-secret`,
// and no `{:?}` may print it.
#[derive(Parser)]
#[command(
    name = "thoth",
    version,
    about = "Control plane for native coding-agent harnesses"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

// No Debug: see `Cli`.
#[derive(Subcommand)]
pub enum Command {
    /// Read or change user settings.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Write this computer's run records into the shared records folder (records-dir).
    Records {
        #[command(subcommand)]
        action: RecordsAction,
    },
    /// Register this git repository and create its state database.
    Init {
        #[arg(long)]
        json: bool,
    },
    /// Serve the commander workflow as MCP tools over stdio.
    Mcp {
        #[arg(long)]
        default_repo: Option<std::path::PathBuf>,
        #[arg(long, value_parser = parse_mcp_wait, default_value = "30s")]
        max_wait: u64,
        #[arg(long, value_parser = parse_result_bytes, default_value_t = 60_000)]
        max_result_bytes: usize,
        #[arg(long)]
        plugin_root: Option<std::path::PathBuf>,
    },
    /// Print the commander guide (the thoth-dispatch workflow) for this version.
    Guide {
        #[arg(long)]
        json: bool,
    },
    /// Check this machine's setup without changing anything.
    Doctor {
        /// Write fix commands for this shell instead of the detected one.
        #[arg(long, value_parser = ["powershell", "cmd", "bash", "sh"])]
        shell: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Set up this machine and this repository step by step, asking before each change (needs a terminal).
    Setup {
        /// Write commands for this shell instead of the detected one.
        #[arg(long, value_parser = ["powershell", "cmd", "bash", "sh"])]
        shell: Option<String>,
    },
    /// Manage harness account profiles (stored in global.db).
    Profile {
        #[command(subcommand)]
        action: ProfileAction,
    },
    /// List every profile's quota facts (eligible or not, and why) and the
    /// models it can run, the three external tables filtered to the
    /// organizations with an account here (Artificial Analysis, Agent Arena,
    /// AI Stupid Level's recent degradation), Thoth's own usage per account,
    /// model, effort and task type (average tokens and quota points, runs
    /// left), and with --task the task's reject history; it does not choose a
    /// profile, model or effort for you.
    Recommend {
        #[arg(long)]
        role: String,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List models available to every registered profile.
    Models {
        /// Refresh the external score cache using curl.
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
    /// Write a read-only HTML status page (Traditional Chinese) for the human.
    Report {
        /// Write the page to FILE instead of $THOTH_HOME/report.html.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Open the page in the default browser afterwards.
        #[arg(long)]
        open: bool,
        /// With --open: open the page at this section (today only `decision-table`).
        #[arg(long, requires = "open")]
        section: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Count the review findings of the last N days by category, across every
    /// registered repo on this machine, and say which keep repeating.
    Retro {
        /// How many days back to look (1 to 3650).
        #[arg(long, default_value_t = 7, value_parser = clap::value_parser!(u32).range(1..=3650))]
        days: u32,
        #[arg(long)]
        json: bool,
    },
    /// Create and inspect tasks.
    Task {
        #[command(subcommand)]
        action: TaskAction,
    },
    /// Internal: the detached runner for one run. Not part of the public surface.
    #[command(hide = true, name = "run-worker")]
    RunWorker {
        run: String,
        /// Internal: set only by `process::ensure_supervised` on the copy it
        /// re-runs inside a systemd unit (Linux), so that copy refuses rather
        /// than re-running itself again if it still is not in one.
        #[arg(long, hide = true)]
        supervised: bool,
    },
    /// Internal: the Claude Code hook receiver. Reads one event payload on
    /// stdin. Not part of the public surface.
    #[command(hide = true)]
    Hook {
        /// Internal: agy's PreToolUse hook; prints one allow/deny line.
        #[arg(long = "agy-pre")]
        agy_pre: bool,
    },
    /// Start a run for a task on a given profile.
    Dispatch {
        task: String,
        /// A profile id.
        #[arg(long)]
        profile: String,
        /// The model. Required on the first dispatch; a reroute in the same
        /// attempt keeps the previous run's.
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_parser = parse_duration_secs, default_value = "30m")]
        timeout: u64,
        #[arg(long = "allowed-tools", default_value = DEFAULT_ALLOWED_TOOLS)]
        allowed_tools: String,
        /// Permit an attempt up to N for this dispatch only, past the task's
        /// attempt_cap. Writes nothing: the next dispatch is gated again.
        #[arg(long = "allow-attempt")]
        allow_attempt: Option<i64>,
        /// Reasoning effort: low, medium, high, xhigh, max or ultra. Lowered
        /// to what the harness and model support; checked in the command, not
        /// by clap. Required on the first dispatch (unless the model has no
        /// effort setting); a reroute in the same attempt keeps the previous
        /// run's.
        #[arg(long)]
        effort: Option<String>,
        /// Why this profile, model and effort were chosen; recorded in the
        /// launch_choice event; required when require-reason is on.
        #[arg(long)]
        reason: Option<String>,
        /// Dispatch an important/high task without an APPROVE spec review; the reason is recorded in a spec_review_skipped event.
        #[arg(long = "skip-spec-review")]
        skip_spec_review: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show open tasks, or the runs of one task.
    Status {
        task: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print events with seq greater than `--since`.
    Events {
        #[arg(long, default_value_t = 0)]
        since: i64,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Block until a task reaches in_review / unverified / failed / cancelled /
    /// interrupted, or — with --run — until one run reaches a terminal status.
    Wait {
        /// Task id or unique prefix. Mutually exclusive with --run.
        #[arg(required_unless_present = "run", conflicts_with = "run")]
        task: Option<String>,
        /// Run id or unique prefix: block on that run's own terminal status
        /// (completed / failed / cancelled / interrupted) instead of the
        /// task's, and report its verdict if it was a review.
        #[arg(long)]
        run: Option<String>,
        #[arg(long, value_parser = parse_duration_secs, default_value = "30m")]
        timeout: u64,
        #[arg(long)]
        json: bool,
    },
    /// Ask a run to stop, escalating to a kill if its runner is dead.
    Cancel {
        run: String,
        #[arg(long)]
        json: bool,
    },
    /// Restart an interrupted run on the same profile using --resume.
    Resume {
        run: String,
        #[arg(long)]
        json: bool,
    },
    /// Manually ingest a result.json for a run.
    Submit {
        #[arg(short = 'f', long = "file")]
        file: std::path::PathBuf,
        #[arg(long)]
        run: String,
        #[arg(long)]
        json: bool,
    },
    /// Inspect the evidence the control plane captured for a task.
    Evidence {
        #[command(subcommand)]
        action: EvidenceAction,
    },
    /// List what a run's worker did, read from its stream.jsonl; --offset continues from a previous call's next_offset.
    Transcript {
        run: String,
        #[arg(long, default_value_t = 0)]
        offset: u64,
        #[arg(long)]
        json: bool,
    },
    /// Start or inspect an independent review of a task's candidate.
    Review {
        #[command(subcommand)]
        action: ReviewAction,
    },
    /// Lint a spec file, or review a task's spec before it is dispatched.
    Spec {
        #[command(subcommand)]
        action: SpecAction,
    },
    /// Run the repo's approved checks in a run's worktree, as control_plane evidence.
    Verify {
        run: String,
        /// List what would run without running anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Per-check deadline; a check still running at this point is
        /// killed (fix round 1 / I2) and recorded as a failure.
        #[arg(long, value_parser = parse_duration_secs, default_value = "600s")]
        timeout: u64,
        /// Run in a temporary detached worktree at the task's base_commit
        /// instead of the run's worktree, to get a baseline.
        #[arg(long = "at-base")]
        at_base: bool,
        #[arg(long)]
        json: bool,
    },
    /// Record a long-lived decision in the repository.
    Decision {
        #[command(subcommand)]
        action: DecisionAction,
    },
    /// Manage the repo's catalogue of approved checks.
    Checks {
        #[command(subcommand)]
        action: ChecksAction,
    },
    /// Remove the worktrees of finished tasks (pass, cancelled or accepted) that are clean; keep and list the rest.
    Worktrees {
        #[command(subcommand)]
        action: WorktreesAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum WorktreesAction {
    /// Remove every clean worktree of a pass, cancelled or accepted task and list what was kept.
    Prune {
        #[arg(long)]
        json: bool,
    },
}

// No Debug: see `Cli`.
#[derive(Subcommand)]
pub enum ConfigAction {
    /// List all settings.
    List { #[arg(long)] json: bool },
    /// Read one setting.
    Get { key: String, #[arg(long)] json: bool },
    /// Change one setting.
    Set { key: String, value: String, #[arg(long)] json: bool },
    /// Remove one stored setting so its environment variable or default applies.
    Unset { key: String, #[arg(long)] json: bool },
    /// Store an API key read from stdin or a hidden prompt (never from the command line).
    SetSecret {
        key: String,
        /// Anything typed after KEY is collected and refused by `set_secret`,
        /// so clap never echoes a key that was pasted here by mistake.
        #[arg(hide = true, num_args = 0.., allow_hyphen_values = true)]
        value: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Remove a stored API key so its environment variable, if any, applies.
    UnsetSecret { key: String, #[arg(long)] json: bool },
}

#[derive(Subcommand, Debug)]
pub enum RecordsAction {
    /// Regenerate NAME.jsonl in records-dir from every registered repository of this computer.
    Export { #[arg(long)] json: bool },
}

#[derive(Subcommand, Debug)]
pub enum ChecksAction {
    /// Propose a check (or replace one, clearing its approval).
    Add {
        id: String,
        /// The command line to run.
        #[arg(long)]
        cmd: String,
        /// argv, cmd, powershell, sh or bash.
        #[arg(long, default_value = "argv")]
        shell: String,
        /// Working directory, relative to the repo root.
        #[arg(long, default_value = ".")]
        cwd: String,
        /// hash: pin the program's resolved path and its bytes (default); path: pin its resolved path only, for a program outside the repository that updates itself
        #[arg(long = "exe-pin", default_value = "hash")]
        exe_pin: String,
        /// An input file the check depends on (repeatable); more are inferred.
        #[arg(long = "input")]
        input: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Approve every pending or changed check in one go; without a terminal (the commander) --reason is required.
    Approve {
        /// Read the files from this run's worktree instead of the main tree.
        #[arg(long)]
        run: Option<String>,
        /// Why this approval is right; recorded with it for the reviewer.
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show every check with its status: approved, stale or pending.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Delete a check from the catalogue.
    Remove {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Treat more files as protected: `name/`, `*.ext`, `prefix*`, a file name, or a whole path.
    Protect {
        pattern: String,
        #[arg(long)]
        json: bool,
    },
    /// Drop a pattern added with `protect`.
    Unprotect {
        pattern: String,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum DecisionAction {
    /// Append one dated line to decisions.md (or `--file`). Does not commit.
    Add {
        text: String,
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SpecAction {
    /// Start, record or show a spec review.
    Review {
        #[command(subcommand)]
        action: SpecReviewAction,
    },
    /// Check a spec file with the fixed rules task create, task update-spec and spec review start apply first; exits 1 when an error would make them refuse.
    Lint {
        file: std::path::PathBuf,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum SpecReviewAction {
    /// Launch a read-only spec reviewer against the task's current spec, checked out at the HEAD of the worktree the command runs in when that contains the task's base, otherwise at the task's base; the verdict is recorded as a spec_review event. Use a profile of the provider that did not write the spec.
    Start {
        task: String,
        /// A profile id.
        #[arg(long)]
        profile: String,
        /// The model. Required every time: a spec review keeps nothing from
        /// an earlier one.
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_parser = parse_duration_secs, default_value = "30m")]
        timeout: u64,
        /// Reasoning effort: low, medium, high, xhigh, max or ultra (see
        /// dispatch). Required every time; it can be left out only when the
        /// model has no effort setting.
        #[arg(long)]
        effort: Option<String>,
        /// Why this profile, model and effort were chosen; recorded in the
        /// launch_choice event; required when require-reason is on.
        #[arg(long)]
        reason: Option<String>,
        /// Edit mode: the reviewer may also propose edits to the spec, which
        /// thoth applies and keeps as a proposed spec for the author to
        /// confirm with `spec review confirm`.
        #[arg(long)]
        edit: bool,
        #[arg(long)]
        json: bool,
    },
    /// Launch the author's check of the edits the newest edit-mode spec review proposed: ok or revise per edit; an APPROVE replaces the task's spec with the proposed one.
    Confirm {
        task: String,
        /// A profile id.
        #[arg(long)]
        profile: String,
        /// The model. Required every time.
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_parser = parse_duration_secs, default_value = "30m")]
        timeout: u64,
        /// Reasoning effort, as for spec review start.
        #[arg(long)]
        effort: Option<String>,
        /// Why this profile, model and effort were chosen; recorded in the
        /// launch_choice event; required when require-reason is on.
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Record a spec review made outside thoth (APPROVE or REVISE of the task's current spec, each finding bound to an acceptance criterion).
    Record {
        task: String,
        #[arg(short = 'f', long = "file")]
        file: std::path::PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Show the newest spec review and whether the current spec is approved for its first dispatch.
    Show {
        task: String,
        /// Write the newest spec review's proposed spec (its edits applied) to
        /// this file, byte for byte; exits 1 when there is none for the
        /// current spec.
        #[arg(long, value_name = "FILE")]
        proposed_out: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ReviewAction {
    /// Launch a reviewer run against the task's newest completed candidate.
    Start {
        task: String,
        /// A profile id.
        #[arg(long)]
        profile: String,
        /// The model. Required on the first review of an attempt; starting
        /// the review of the same attempt again keeps the previous run's.
        #[arg(long)]
        model: Option<String>,
        #[arg(long, value_parser = parse_duration_secs, default_value = "30m")]
        timeout: u64,
        /// Reasoning effort: low, medium, high, xhigh, max or ultra (see
        /// dispatch). Required on the first review of an attempt (unless the
        /// model has no effort setting); starting it again keeps the previous
        /// run's.
        #[arg(long)]
        effort: Option<String>,
        /// Why this profile, model and effort were chosen; recorded in the
        /// launch_choice event; required when require-reason is on.
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show the newest review's verdict, findings and opened evidence.
    Show {
        task: String,
        #[arg(long)]
        json: bool,
    },
    /// Record an external review (produced outside thoth, e.g. by a
    /// commander session) for a task, settling it the same way a Thoth
    /// reviewer run's verdict would.
    Record {
        task: String,
        #[arg(short = 'f', long = "file")]
        file: std::path::PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Print everything a reviewer needs to judge a task (spec, evidence
    /// index, full diffs, the worker's claims, earlier reviews) in one
    /// read-only document. The worker's own summary is left out on purpose.
    Packet {
        task: String,
        /// Write the document to FILE instead of stdout.
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum ProfileAction {
    /// Register a profile. `--config-dir` must already exist.
    Add {
        /// Profile id, e.g. claude-a / claude-b.
        id: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        harness: String,
        #[arg(long = "config-dir")]
        config_dir: std::path::PathBuf,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// List every registered profile.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Run `claude auth status --json` for this profile and report the identity.
    Check {
        id: String,
        #[arg(long)]
        json: bool,
    },
    /// Log in to a CLI account in this terminal and register it as a profile.
    Login {
        /// Profile id; left out, one is chosen (claude-a, codex-a, agy-a, ...).
        id: Option<String>,
        #[arg(long, value_parser = ["claude-code", "codex", "antigravity"])]
        harness: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Point an existing profile at a different config directory (which must
    /// already exist), e.g. after moving the state directory.
    SetConfigDir {
        id: String,
        dir: std::path::PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Forget that a model was recorded as unavailable on this account (a
    /// `model_unavailable` quota event), so the launch commands accept it
    /// again. The model is matched in any letter case.
    ForgetModel {
        id: String,
        model: String,
        #[arg(long)]
        json: bool,
    },
    /// Mark a profile as also used by a human (`on`) or not (`off`): runs on
    /// a shared profile record their quota points as unreliable. Refused
    /// while THOTH_RUN_ID is set.
    SetShared {
        id: String,
        #[arg(value_parser = ["on", "off"])]
        value: String,
        #[arg(long)]
        json: bool,
    },
    /// Show the last known quota windows for registered profiles.
    Usage {
        id: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum TaskAction {
    /// Create a task from a v6 section 6.3 spec file.
    Create {
        #[arg(short = 'f', long = "file")]
        file: std::path::PathBuf,
        #[arg(long)]
        title: Option<String>,
        /// Where the task starts: any name that resolves to a commit, such as a branch or a sha. Without it, the HEAD of the worktree the command runs in.
        #[arg(long, value_name = "REF")]
        base: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show one task. Accepts a full ULID or a unique prefix of 6+ chars.
    Show {
        task: String,
        #[arg(long)]
        json: bool,
    },
    /// Replace a task's spec. Allowed while the task is draft, queued, reject or unverified.
    #[command(name = "update-spec")]
    UpdateSpec {
        task: String,
        #[arg(short = 'f', long = "file")]
        file: std::path::PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Abandon a task: queued, failed, interrupted, unverified or reject -> cancelled;
    /// in_review too for a docs, research or review task.
    /// Removes the clean worktrees of finished tasks; one with uncommitted changes is kept and reported.
    Close {
        task: String,
        /// Why the task is abandoned; recorded in the task_closed event.
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
    /// Take a docs, research or review task's delivery: in_review -> accepted.
    /// Records no review and no verdict. Removes the clean worktrees of finished tasks.
    Accept {
        task: String,
        /// Why the delivery is taken; recorded in the task_accepted event.
        #[arg(long)]
        reason: String,
        #[arg(long)]
        json: bool,
    },
    /// Print the newest result.json a task's worker delivered: summary and claims. Read-only.
    Result {
        task: String,
        /// Read this run's newest result instead; it must belong to the task.
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Subcommand, Debug)]
pub enum EvidenceAction {
    /// List a task's evidence index.
    List {
        task: String,
        #[arg(long)]
        json: bool,
    },
    /// Print one evidence record and the head of its stored bytes. Records an
    /// `evidence_opened` event.
    Show {
        /// An evidence code, e.g. E001.
        code: String,
        /// Which task the code belongs to. Optional when THOTH_RUN_ID is set
        /// (a worker or reviewer) or when the code is unique across tasks.
        #[arg(long)]
        task: Option<String>,
        /// How many bytes of the stored blob to print.
        #[arg(long)]
        bytes: Option<u64>,
        #[arg(long)]
        json: bool,
    },
}

/// M9 (fix round 1): a `--timeout`/`--wait` value this large would let
/// `Instant::now() + Duration::from_secs(n)` (in `wait::run`) overflow, and
/// would let `runs.timeout_seconds` (stored as an `i64`, computed as
/// `args.timeout_seconds as i64` in `dispatch::run`) wrap negative for any
/// value near `u64::MAX`. Nothing in Phase 1 legitimately waits or times out
/// longer than a week, so anything past that is almost certainly a typo
/// (an extra digit, a wrong unit) rather than an intentional value.
const MAX_DURATION_SECS: u64 = 7 * 24 * 3600;

/// Accept `45`, `5s`, `30m`, `2h`. A bare number means seconds.
pub fn parse_duration_secs(s: &str) -> Result<u64, String> {
    if s.is_empty() {
        return Err("duration is empty".to_string());
    }
    let (digits, mult) = match s.chars().last().unwrap() {
        's' => (&s[..s.len() - 1], 1u64),
        'm' => (&s[..s.len() - 1], 60u64),
        'h' => (&s[..s.len() - 1], 3600u64),
        c if c.is_ascii_digit() => (s, 1u64),
        c => return Err(format!("unknown duration suffix '{c}'; use s, m or h")),
    };
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return Err(format!("'{s}' is not a duration like 45, 5s, 30m or 2h"));
    }
    let n: u64 = digits
        .parse()
        .map_err(|_| format!("'{digits}' does not fit in a u64"))?;
    let secs = n
        .checked_mul(mult)
        .ok_or_else(|| format!("duration '{s}' overflows"))?;
    if secs > MAX_DURATION_SECS {
        return Err(format!(
            "duration '{s}' is {secs}s, which is more than the {MAX_DURATION_SECS}s (7 day) maximum"
        ));
    }
    Ok(secs)
}

fn parse_mcp_wait(s: &str) -> Result<u64, String> {
    let seconds = parse_duration_secs(s)?;
    if (1..=3600).contains(&seconds) {
        Ok(seconds)
    } else {
        Err("must be between 1 second and 3600 seconds".to_string())
    }
}

fn parse_result_bytes(s: &str) -> Result<usize, String> {
    let value = s
        .parse::<usize>()
        .map_err(|_| "must be an integer".to_string())?;
    if value >= 200 {
        Ok(value)
    } else {
        Err("must be at least 200".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::parse_duration_secs;

    #[test]
    fn durations_parse() {
        assert_eq!(parse_duration_secs("45"), Ok(45));
        assert_eq!(parse_duration_secs("5s"), Ok(5));
        assert_eq!(parse_duration_secs("30m"), Ok(1800));
        assert_eq!(parse_duration_secs("2h"), Ok(7200));
    }

    #[test]
    fn durations_reject_garbage() {
        assert!(parse_duration_secs("").is_err());
        assert!(parse_duration_secs("m").is_err());
        assert!(parse_duration_secs("30x").is_err());
        assert!(parse_duration_secs("-5s").is_err());
    }

    #[test]
    fn durations_reject_more_than_seven_days_but_accept_exactly_seven() {
        // 604800s (168h) is the maximum; 168h is exactly it, 169h is past it.
        assert_eq!(parse_duration_secs("168h"), Ok(604_800));
        assert!(parse_duration_secs("169h").is_err());
        assert!(parse_duration_secs("604801s").is_err());
        assert!(parse_duration_secs("604800s").is_ok());
    }
}
