# thoth (Phase 1) — how the Supervisor drives it

`thoth` dispatches one task to one native Claude Code worker in an isolated git
worktree and records what happened. There is no daemon: `dispatch` spawns a
detached runner process that lives as long as the worker.

*(2026-09-30: this tool was renamed from agentctl to thoth.)*

All state lives in `THOTH_HOME` (default `%USERPROFILE%\.thoth` on Windows,
`~/.thoth` on Linux; the layout is the same, with `/` separators on Linux):

```
%THOTH_HOME%\
  global.db                       profiles, quota_events, repos
  profiles\<id>\                  a profile's CLAUDE_CONFIG_DIR (thoth profile login creates these)
  repos\<repo_id>\
    state.db                      tasks, runs, claims, events, ...
    worktrees\<task_id>-a<n>\     the worker's workspace
    runs\<run_id>\                stream.jsonl, stderr.log, prompt.txt, runner-error.log
    blobs\                        content-addressed evidence blobs (Phase 2)
```

`repo_id` is `sha256(canonical repo root)[..12]`, with the root lowercased first on
Windows only (Windows paths are case-insensitive; Linux paths are not, so two
roots that differ only in case are two repos there).

### Coming from agentctl

The state directory was `~/.agentctl` (variable `AGENTCTL_HOME`). Thoth never reads
or moves it for you, and it has no silent fallback:

- `THOTH_HOME` set: used as is, nothing else is looked at.
- `THOTH_HOME` unset but `AGENTCTL_HOME` set: every state command exits non-zero and
  says to set `THOTH_HOME` instead.
- Neither variable set, no `~/.thoth`, but a `~/.agentctl`: every state command exits
  non-zero, prints both full paths and the move commands below, and creates or
  changes nothing.
- `~/.thoth` exists: used, even if `~/.agentctl` is still there.
- Neither directory exists: a fresh start; `thoth init` creates `~/.thoth`.

The state database and the git worktrees record **absolute paths** under the old
directory (evidence `blob_path`, `runs.worktree`, and each main repo's
`.git/worktrees/*/gitdir`). Moving the directory therefore breaks old evidence unless
the old path still resolves.

Before any of the three ways below, first finish or cancel any running run with
the old `agentctl` (the new `thoth cancel` cannot stop a run started by the old
executable).

Three ways:

1. **Do not move; point `THOTH_HOME` at the old directory (recommended for an existing
   install).** Every recorded path stays valid.
   - Windows: `setx THOTH_HOME C:\Users\USERNAME\.agentctl`, then open a new window.
   - Linux: `export THOTH_HOME=$HOME/.agentctl`, and put it in your shell profile.
2. **Move.** Move the directory and leave a link at the old path so recorded paths
   still resolve:
   - Windows: `Move-Item $env:USERPROFILE\.agentctl $env:USERPROFILE\.thoth`, then
     `New-Item -ItemType Junction -Path $env:USERPROFILE\.agentctl -Target $env:USERPROFILE\.thoth`
   - Linux: `mv ~/.agentctl ~/.thoth && ln -s ~/.thoth ~/.agentctl`

   Afterwards run `thoth profile list`; repoint a wrong profile with
   `thoth profile set-config-dir` (below). Without the link, old evidence cannot be
   read and old worktrees need `git worktree repair` by hand.
3. **Start fresh.** Create an empty `~/.thoth` yourself, or set `THOTH_HOME` to a new
   folder.

Thoth itself never creates the link, never moves anything and never edits a file on
this path.

## One-time setup

The quickest way is to open a terminal and type `thoth setup`: it goes
through accounts, this repository, checks and the plugin in order, asking
before each change (see "Guided setup: thoth setup" below). The
`thoth profile login …` commands here are what it runs behind the scenes.

Log in to each account with `thoth profile login`. It picks the profile id,
creates the profile's directory, sets the CLI's environment variable, opens
that CLI's own login screen in this terminal, and registers the profile once
the CLI itself says you are logged in:

```
thoth profile login --harness claude-code                          # the first Claude account: claude-a
thoth profile login --harness claude-code --label "pro account"    # the second: claude-b
thoth profile login --harness codex
thoth profile login --harness antigravity
thoth profile login claude-a                                       # log in again when a login expired
thoth profile list
thoth profile check claude-a
```

- The directory is `<THOTH_HOME>/profiles/<id>`, created by thoth. agy keeps
  its own `~/.gemini/antigravity-cli` (on Windows
  `%USERPROFILE%\.gemini\antigravity-cli`), one agy account per machine user.
- thoth sets `CLAUDE_CONFIG_DIR` or `CODEX_HOME` for the CLI; you never type
  an environment variable.
- The login screen opens in the same terminal. thoth reads no login file: it
  asks the CLI's own status command (the one `profile check` uses) whether
  you are logged in.
- Not logged in when the CLI closes: nothing is registered and the command
  exits 1. Run the same command again; the directory is reused.
- A name that differs from a registered profile only in letter case is refused
  (a Windows directory name is not case-sensitive, so they would share one
  directory).
- It needs a real terminal. Claude Code's `!` and pipes are not one, and thoth
  refuses there. In the mintty Git Bash window thoth may not notice, but
  claude's and agy's screens do not work properly there; use Windows Terminal,
  PowerShell, cmd, or Git Bash inside conhost.
- With `--json`, stdout carries only the result object; the CLI's screen still
  appears in the same terminal.
- While the login screen is open, Ctrl+C reaches only that CLI (thoth ignores
  Ctrl+C meanwhile). When the CLI ends, thoth checks the login as usual.

### Manual setup (what login does for you)

```
thoth profile add claude-a --provider anthropic --harness claude-code --config-dir C:\Users\USERNAME\.thoth\profiles\claude-a --label "max account"
thoth profile add claude-b --provider anthropic --harness claude-code --config-dir C:\Users\USERNAME\.thoth\profiles\claude-b --label "pro account"
thoth profile list
thoth profile check claude-a
thoth profile check claude-b
thoth profile add agy-a --provider google --harness antigravity --config-dir ~/.gemini/antigravity-cli
```

**Repointing a profile.** `thoth profile set-config-dir ID DIR [--json]` sets an
existing profile's config dir. `DIR` must already exist (same check and path
normalisation as `profile add`). `--json` prints `profile_id` and the new
`config_dir`. An unknown `ID` or a missing `DIR` exits non-zero and changes nothing.

**`--harness` is `claude-code`, `codex` or `antigravity`.** `profile add` with any other
value exits non-zero ("only claude-code, codex and antigravity are supported") and writes
no profile. `dispatch` applies the same check to the stored profile: if an
older `global.db` already holds a profile with another harness, it exits
non-zero naming that harness, before creating a run or changing the task's
status. `review start` applies the same check to the reviewer's profile and
accepts both harnesses: a `codex` profile can review a Claude worker's run and
vice versa (see `docs/phase2/usage.md`, "Codex reviewers").

**Never point a profile at your own `~/.claude`.** Each profile directory must be
one you created for thoth and logged into separately:

```
New-Item -ItemType Directory -Force C:\Users\USERNAME\.thoth\profiles\claude-a
$env:CLAUDE_CONFIG_DIR = "C:\Users\USERNAME\.thoth\profiles\claude-a"
claude   # log in, then exit
Remove-Item Env:\CLAUDE_CONFIG_DIR
```

On Linux (bash), the same two steps, then `profile add` as above with
`--config-dir ~/.thoth/profiles/claude-a`:

```
mkdir -p ~/.thoth/profiles/claude-a
CLAUDE_CONFIG_DIR=~/.thoth/profiles/claude-a claude   # log in, then /exit
```

**A codex profile** gets its own `CODEX_HOME`, never your own `~/.codex`
(a worker writes into the directory it is given). codex does not create the
directory itself, so make it first, log in once, then register it with
provider `openai` and harness `codex`:

```
mkdir -p ~/.thoth/profiles/codex-a
CODEX_HOME=~/.thoth/profiles/codex-a codex login   # log in with the ChatGPT account
thoth profile add codex-a --provider openai --harness codex --config-dir ~/.thoth/profiles/codex-a
thoth profile check codex-a
```

**Windows: npm-installed `codex` and `claude`.** npm leaves a `codex.cmd`
shell shim, not a `.exe`, and Rust does not consult PATHEXT. So on Windows
thoth resolves `THOTH_CODEX_BIN` / `THOTH_CLAUDE_BIN` (or the defaults `codex`,
`claude`) like this, reading files only:

- a `.exe` path, or a bare name that has a `.exe` on PATH: used as is;
- a bare name with no `.exe` on PATH: the first `name.cmd` / `name.bat` on PATH;
- a `.cmd` / `.bat` (found or given directly): thoth reads it, takes the
  `"%dp0%\node_modules\...\x.js"` entry an npm cmd-shim contains, and runs
  `node.exe <that .js> ARGS` instead, using the `node.exe` beside the shim or
  else the one on PATH. `runs.harness_bin` records that `node.exe`.

thoth never runs a `.cmd` / `.bat` through `cmd.exe` (BatBadBut): `cmd.exe`
expands `%VAR%` and drops newlines in arguments, and the prompt is an argument.
If the shim is not in the npm format, its JS file is missing, or there is no
`node.exe`, the command fails naming the shim; then set `THOTH_CODEX_BIN` or
`THOTH_CLAUDE_BIN` to the real executable.

`profile check` on a codex profile runs `codex login status` (the binary is
`THOTH_CODEX_BIN`, default `codex`) with `CODEX_HOME` set to the profile's
directory: `logged_in` is true when it exits 0 and prints "Logged in". codex
reports neither email nor plan, so `email` and `subscription_type` are null.
Dispatching to a codex profile needs a codex model, e.g. `--model
gpt-5.6-luna`; see docs/phase2/usage.md, "Codex workers".

An Antigravity profile's `config_dir` is agy's own data directory and is used
only to find transcripts. thoth sets no config environment variable and
leaves `HOME` untouched: agy's login is in the system keyring, so one machine
user has one agy account, shared with that user's own sessions and quota.
`profile check` runs `agy models` (`THOTH_AGY_BIN`, default `agy`) and reports
logged in only when it exits 0 without printing `Please sign in`; email and
subscription type are null. `profile usage` can show and explicitly refresh
agy's model-group quota; see Phase 2, "Quota, recommendations and the status
page". Dispatch requires
an agy model; see docs/phase2/usage.md, "Antigravity workers".

**Linux needs a systemd user session** (a normal desktop login has one; check
with `systemctl --user is-system-running`). thoth starts every runner as a
transient systemd `--user` service — see "Cancelling" below for why — and
refuses to start runs where `systemd-run --user` is not available. There is no
fallback without systemd (containers, some WSL setups, minimal servers).

Three reasons, all measured in Phase 0:

- A worker started with `CLAUDE_CONFIG_DIR=<your ~/.claude>` **writes into it** —
  session transcripts, and whatever else the harness keeps there. That is exactly
  what this plan's Global Constraints forbid.
- It loads your global `CLAUDE.md`, your hooks and your plugins, so a one-sentence
  task pays 30-50k tokens of cache-creation cost before it starts
  (01-claude-cli-identity.md:42).
- Your personal operating rules leak into the worker's context, which makes the
  worker's behaviour depend on your machine rather than on the task spec.

`profile check` runs `claude auth status --json` with that profile's
`CLAUDE_CONFIG_DIR` and prints `logged_in`, `email`, `subscription_type` and
`config_directory`. It is free — it does not consume a `-p` call. Being rate
limited does not make `claude auth status` report `loggedIn:false`, so the auth
check alone cannot tell that a window reset. Thoth notices it itself: the next
`dispatch` or `review start` on this profile that sees the window has reset since
the profile was parked marks it `active` again (a `window_reset` quota event,
`source: auto`). `profile check` still does the same by hand (`window_reset`,
`source: profile check`), and it also clears a `logged_out` profile back to
`active` once the CLI says it is logged in (a `login_restored` quota event). Its
`--json` output carries `status`, the profile's status after the check. When the
CLI reports a subscription type, the check stores it as the profile's tier, and
from then on the screens a person reads show the account as `claude-a (Max)`.

Then, once per repository, from the repo root:

```
thoth init
```

## The normal loop

```
thoth task create -f spec.json --title "uploader retry"
thoth dispatch 01J... --profile claude-b --model sonnet --effort medium --timeout 30m
thoth status
thoth wait 01J... --timeout 30m
thoth task show 01J...
```

`spec.json` has this shape. `goal` and a non-empty
`acceptance_criteria` are required; `constraints`, `non_goals`,
`excluded_alternatives` default to empty and `risk` defaults to `normal`:

```json
{
  "goal": "Add a retry to the uploader",
  "acceptance_criteria": ["AC1: retries three times", "AC2: logs each attempt"],
  "constraints": ["no new dependencies"],
  "non_goals": ["rewriting the transport"],
  "excluded_alternatives": ["a backoff crate - evaluated, too heavy for one call site"],
  "risk": "normal"
}
```

Every id argument takes a full ULID or a unique prefix of at least 6 characters.

Breaking change (RJ-1): Thoth no longer picks the model or effort. The first
dispatch of a task (and of each rework attempt) gives `--model` and `--effort`
(`--effort` can be left out only for a model without an effort setting, such
as haiku); a re-dispatch in the same attempt keeps the previous run's for
whatever you leave out. `thoth recommend --role implementer --task <task>`
shows each profile's quota to choose from.

### Starting from a long-lived branch

A task starts at the HEAD of the worktree `task create` runs in. To work on a
long-lived branch, run `task create` in a linked worktree that has the branch
checked out (`git worktree add ../feature feature`), or pass the start from
anywhere with `--base`:

```
thoth task create -f spec.json --base feature
```

`--base` takes any name of a commit (a branch, a tag, a sha). The task stores
the commit's sha, so the branch moving later does not move the task, and
`task show --json` gives the branch it started from as `base_branch`. Every
worktree of the repository shares one task list. Thoth never merges: the
finished `thoth/TASK/aN` branch goes back into `base_branch` by hand.

## Polling with the events cursor

`thoth events --since <seq>` returns only rows with `seq` strictly greater
than `<seq>`, plus `last_seq`. Keep `last_seq` and feed it back:

```
thoth events --since 0 --json         # -> {"events":[...],"last_seq":14}
thoth events --since 14 --json        # -> {"events":[],"last_seq":14}
thoth events --since 14 --task 01J... --json
```

Event types you will see in Phase 1: `task_created`, `status_change`,
`worktree_created`, `worktree_reused`, `worktree_error`, `run_started`,
`harness_init`, `rate_limit_warning`, `api_retry`, `result_ingested`,
`result_invalid`, `quota_exceeded`, `run_interrupted`, `cancel_requested`.

`status_change` details carry `entity` (`task` or `run`), `id`, `from` and `to`.

## Blocking instead of polling

`thoth wait <task> --timeout 30m` returns when the task reaches
`in_review`, `unverified`, `failed`, `cancelled` or `interrupted`.

Exit codes: `0` a terminal status was reached (the status is in the output),
`3` the timeout expired (the task is still running), `1` something else failed.
Durations accept `45`, `5s`, `30m`, `2h`; a bare number means seconds. The
maximum accepted duration is 7 days (`168h` / `604800s`); anything past that is
almost certainly a typo and is refused.

As of Phase 2 `thoth wait --run <run_id>` blocks on one run's own terminal
status (`completed` / `failed` / `cancelled` / `interrupted`) instead of the
task's, and reports a reviewer run's verdict. A task id and `--run` are mutually
exclusive. See `docs/phase2/usage.md`.

## Reading the outcome

| task status | what it means | what to do |
|---|---|---|
| `in_review` | the run finished and `.agent/result.json` validated; claims are in the DB | review the candidate commit on branch `thoth/<task>/a<n>` |
| `unverified` | the harness succeeded but the result file was missing or invalid | read the `result_invalid` event's `reason`. If the work is sound and only the report was malformed, fix the file and `thoth submit`. If you want the worker to have another go, `thoth dispatch <task> --profile <id>` again: it reuses the same worktree and the commits already there |
| `interrupted` | the run stopped early; the worktree and `harness_session_id` are intact. `runs.interrupted_reason` says why: `quota` (the account's quota is used up), `crash` (the worker ended without a result), `auth` (the profile is logged out; see Login failures below), `transient` (a network problem or a busy service; the runner already retried it twice in the same run (see Temporary problems below)) | `quota` (Breaking change, RJ-3): `thoth dispatch <task> --profile <other> --reason <why>` now with a profile that still has quota (`thoth status <task>` lists them; the re-dispatch keeps the run's model and effort when `<other>` has the same harness); do not wait for the used-up profile or resume the run on it. A reviewer or spec reviewer run is started again with `review start` / `spec review start` on the next reviewer that has quota. Only when no profile has quota anywhere does the commander wait for quota to come back. `auth`: `thoth profile login <profile>`, then `thoth resume <run>`. `transient`: `thoth resume <run>` to try again. `crash`: `thoth resume <run>` (same profile) or `thoth dispatch <task> --profile <other>` |
| `failed` | the harness reported a real failure, the wall-clock timeout fired, a cancel could not kill every process, or the runner gave up after repeated database errors | read `runs.error_text` and `runs\<run_id>\stderr.log` (and `stderr.retry1.log` / `stderr.retry2.log` when the runner relaunched the harness). `failed` is still re-dispatchable — `thoth dispatch <task> --profile <id>` starts a new run on the same attempt, reusing the worktree, exactly like the `interrupted`/`unverified` re-entry paths |
| `cancelled` | you cancelled it and every process died | nothing |

`thoth status <task> --json` gives each run a `next_step`: the command to run
next for an interrupted or failed run, naming its profile, run and task (null
when there is nothing to do). The human output adds ` reason=<reason>` to a run
that has one and prints the newest run's next step once, as `  next: ...`.
Breaking change (RJ-3): for a quota stop it says `quota used up on <profile>;
dispatch again with a profile that still has quota: thoth dispatch <task>
--profile OTHER --model <model> --effort <level> --reason TEXT (profiles with
quota now: …); do not wait for <profile>` (a reviewer or spec reviewer run:
`start the review again with the next reviewer in your order that still has
quota` / `start the spec review again with …`); it no longer suggests waiting
for a reset or `thoth resume`, and the run's `profiles_with_quota` in
`--json` lists those profiles (null for any other run, or when their quota
could not be read).

`thoth status <task>` lists the task's runs with `heartbeat_stale`. A
`running` run whose heartbeat is more than 15 seconds old has probably lost its
runner; `thoth cancel` will then kill the worker tree directly.
`thoth status` with no argument lists every task that is not `pass` or
`cancelled` — `failed` and, as of Phase 2, `reject` are both included even
though nothing is waiting on either any more, because both are still
actionable (re-dispatchable; a `reject` dispatch is Phase 2's rework loop, see
`docs/phase2/usage.md`).
Outside a git repository, `thoth status` with no argument does not fail: its
first line says so, and it lists the open tasks of every repository registered
with `thoth init`, under a line naming each repository's root (`--json` adds
`outside_repo: true` and each task's `repo`). Every other command that needs a
repository still refuses there.

`dispatch` accepts a task in `queued`, `interrupted`, `in_review`, `unverified`,
`failed` or `reject`, and moves it to `running` before it returns. It refuses
`draft`, `running`, `pass` and `cancelled`. A dispatch out of `reject` is a
rework: it increments `attempt` — see `docs/phase2/usage.md`.

## Cancelling

```
thoth cancel <run>
```

Two layers. First it sets `cancel_requested`, which a live runner notices within
a second. If the run is still not finished after 10 seconds and its heartbeat is
stale (or it never started), `cancel` kills the worker tree and the runner
directly and settles the row itself.

Before killing a bare pid, `cancel` confirms it is still the process thoth
actually started, by comparing the executable name the OS reports (`tasklist`
on Windows, `/proc/<pid>/comm` on Linux) against the harness/thoth binary it
expects (case- and extension-insensitive, e.g. `claude` and `claude.exe` are the
same binary). Both OSes recycle process ids, so a stale pid could otherwise
belong to something unrelated by the time `cancel` runs. Three outcomes for each pid it checks: the pid is simply gone (benign and
often expected — see the Job Objects note below), the pid now belongs to a
different image (never killed, and treated as a kill failure — see the
`THOTH_CLAUDE_BIN` warning right below), or it matches and is killed. If
every pid the escalation looked at ends up dead or already gone, the run
settles `cancelled`; if a pid could not be confirmed dead, the run settles
`failed` instead, with the kill error in `runs.error_text` — something may still
be writing in that worktree, so it is not honest to call it a clean
cancellation.

**The expected worker image is the one the runner saw.** The runner records
the image name the OS reports for the worker pid as `worker_image`, and
`cancel` compares against that when it is there. For a Claude run it is read
right after the worker starts and sits on `run_started`. It matters for codex:
the npm `codex` is a Node launcher whose pid reads `codex` right after spawn
and `node` about half a second later, so a codex run reads it when
`thread.started` arrives and records it on that `harness_init` event
(`run_started` has null). `cancel` takes a `harness_init` value newer than the
run's latest `run_started`, else `run_started`'s. A run without a recorded
`worker_image` (any run started before this change, or a codex run stopped
before `thread.started`) falls back to `runs.harness_bin`, as described next.

**Otherwise the expected worker image comes from the run row, not from the
cancelling shell.** `dispatch` and `resume` record the harness binary they actually
launched into `runs.harness_bin`, and `cancel` compares against that value —
so cancelling from a different shell than the one that dispatched the run is
safe as long as both shells agree on where `claude` actually lives. Only a run
created before the schema v2 migration has no value there (`harness_bin` is
`NULL` on every row Phase 1 wrote); for those, `cancel` falls back to the
*cancelling* shell's `THOTH_CLAUDE_BIN`, which is the Phase 1 behaviour and
carries the same caveat Phase 1 had — run `cancel` with the same
`THOTH_CLAUDE_BIN` the run was dispatched with. Either way, if the image
the OS reports does not match what was expected, `cancel` cannot tell
"unrelated process that reused this pid" apart from "this really is the
still-live worker, just under a different image" — so it treats that mismatch
as a kill failure: the run settles `failed` with the mismatch spelled out in
`runs.error_text`, and `cancel` exits 1, rather than reporting a possibly-false
`cancelled` over a worker that is still running and burning quota.

**On Windows, a worker runs inside a Job Object created with
`JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.** If the runner process dies (crashes, is
killed directly, loses its own process), the kernel tears down every process
still in that job — the worker and anything it spawned — the moment the
runner's handle to the job closes. This is by design: a dead runner can never
leave a worker burning quota unsupervised. In practice this means the worker
pid is very often already gone by the time `cancel`'s escalation runs against a
dead runner; that shows up as a "pid gone" note, not a failure. The run still
shows `running` with a stale heartbeat until `cancel` settles it.

**On Linux, the same guarantee comes from systemd and cgroups.** Each runner is
the main process of its own transient `systemd --user` service
(`thoth-<ULID>.service`, in `thoth.slice`), started with
`KillMode=control-group` and `KillSignal=SIGKILL`: the moment the runner exits
or dies, systemd kills everything left in the service. The worker lives in its
own cgroup inside that service (`.../thoth-<ULID>.service/worker-<pid>`), which
it joins before it starts, so everything it spawns is in it too — including a
grandchild that `setsid`s or double-forks away from its parent, which a
process-group kill would miss. `cancel` and the runner kill a worker through
that cgroup (`cgroup.kill`). To look at live runs: `systemctl --user status
thoth.slice`; a runner's own stderr (a panic, say) is in `journalctl --user
-u thoth-<ULID>.service`. `thoth run-worker` started by hand re-runs
itself inside such a service, in the foreground.

## Quota interruptions

A quota interruption sets `runs.status=interrupted`,
`runs.interrupted_reason=quota`, `tasks.status=interrupted`, writes a
`quota_events` row with `kind=rate_limited`, and parks the profile at
`status=rate_limited`.

Being parked does **not** block anything. `dispatch` to a parked profile runs as
normal and prints one warning — to stderr in human mode, in the `--json`
document's `warnings` array otherwise (the array is always present, and empty
when there is nothing to say):

```
warning: profile claude-b is marked rate_limited since 2026-09-14T09:00:00Z; run may be interrupted immediately. Run 'thoth profile check claude-b' to clear.
```

The warning is not given when the window has reset since (the profile is marked
active instead) or, for an antigravity profile, when the model belongs to another
quota group.

**A model the account cannot use is not a quota stop** (MODELS-1). A Claude result
line whose `api_error` is `model_requires_usage_credits` (a Pro account asked for
Fable; its text carries a 429 and `requires usage credits`) settles the run
`failed` with no `interrupted_reason` and `error_text` starting `model unavailable
on this account: `; the profile stays active, no `rate_limited` row and no
`quota_exceeded` event are written, and the runner writes a `model_unavailable`
quota event and run event instead. The launch commands then refuse that model on
that profile until the plan changes or `thoth profile forget-model <profile>
<model>` (see docs/phase2/usage.md, "Model inventory and scores").

**Login failures.** When a run ends in an error that is not a quota stop (a plain
failure, a crash or a temporary problem), the runner runs the profile's own auth
check once - the one `profile check` runs; it spends no quota. Only when the CLI
itself says it is not logged in is the run reclassified: `runs.status=interrupted`,
`runs.interrupted_reason=auth`, `tasks.status=interrupted`, the error text starts
with `login failure: <profile> is not logged in; `, a `quota_events` row with
`kind=logged_out` is written and the profile is set to `status=logged_out`, and the
run gets a `login_failed` event. A check that cannot run or gives an unclear answer
changes nothing. The final `status_change` of every run says what the check found
in `login_check`: `logged_in`, `signed_out`, `unknown`, `error: ...`, or null when
no check ran. `recommend` skips a `logged_out` profile and `dispatch` refuses it
while its auth check fails, saying to run `thoth profile login <profile>`. Three
things clear it back to `active` with a `login_restored` quota event:
`thoth profile login <profile>`, `thoth profile check <profile>`, or a named
`dispatch` / `review start` whose auth check passes. Then `thoth resume <run>`
continues the same session.

**Temporary problems.** An error whose text contains one of the transient markers
(`overloaded`, `There was a network issue`, `TLS handshake timeout`,
`Eligibility check failed`, `The stream was interrupted`,
`stream disconnected before completion`, `Failed to refresh OAuth token`, in any
letter case) is `runs.status=interrupted`, `runs.interrupted_reason=transient`; the
quota match is checked first, so a text that also names a quota is still a quota
stop. The profile is not touched and no quota event is written.

The runner retries these itself, in the same run and the same worktree: it waits
30 seconds and relaunches the harness; if that launch fails the same way, it waits
2 minutes and relaunches once more - two retries at most. A retry continues the
session when there is one to continue (claude `--resume <session>`, codex
`exec resume <thread>`, agy `--conversation <id>`, with the resume prompt). A codex
or agy failure that came before the stream named a session, and any reviewer run,
starts over with the same prompt instead. Each retry writes one `run_retry` event
(`attempt`, `error`, `wait_seconds`, `mode`, `session_id`, `worker_pid`). While the
runner waits the run stays `running`, its heartbeat keeps moving, and
`thoth cancel <run>` works as usual: the run ends `cancelled` and nothing is
relaunched. The waits and the retries count against the run's own `--timeout`; a
wait that would only end at or past it is not taken. Only when the last allowed
retry fails the same way is the run `interrupted` / `transient`; then
`thoth resume <run>` tries again by hand.

The run's final `status_change` says how this went: `retries` (how many `run_retry`
events it wrote, 0 to 2), `retry_stop` and `retry_error`. `retry_stop` is one of:

- null: the last launch was not a temporary problem (it succeeded, failed some
  other way, or there never was a retry);
- `exhausted`: the last launch was still a temporary problem after both retries;
- `timeout`: the next wait would have reached `--timeout`, so the runner did not retry;
- `cancelled`: a cancel arrived while the runner was waiting to retry;
- `launch_error`: the retry could not be started; `retry_error` says why (null otherwise).

An error that is not in the list above is never retried. When a retry ends in a
quota stop, a logout, a crash or a plain failure instead, that ending is handled by
its own rules above, as if it had been the first launch.

The timestamp comes from the newest `rate_limited` quota event recorded for
that profile. This is deliberate: you often know the window reset before the
database does, and the flag is cleared when something looks again (`profile
check`, or the next dispatch or `review start` that finds the window reset), not
by a timer. If you were right, the run proceeds; if you were wrong, it comes
straight back as another quota interruption.

Two ways out of the interruption itself:

```
thoth profile check claude-b        # after the window resets: profile back to active
thoth resume <run>                  # same profile, --resume <session>, same run_id
```

or reroute to a different account:

```
thoth dispatch <task> --profile claude-a --model sonnet --effort medium
```

Breaking change (RJ-1): Thoth no longer moves a run stopped by quota to another
account by itself. `thoth status <task>` says `quota used up on <profile>;
dispatch again with a profile that still has quota: …` and lists the profiles
that have quota now; a reroute in the same attempt keeps the stopped run's
model and effort for whatever you leave out (give `--model` when the new
profile has another harness).

Reroute creates a **new run on the same attempt** and **reuses the existing
worktree**, because git refuses a second worktree on the same branch. Resume
keeps the same `run_id`. Neither one resets the worker's commits.

Both paths move `tasks.status` to `running` **before the command returns**, so an
`thoth wait` issued straight afterwards blocks instead of returning the stale
`interrupted` status it was called to escape. You do not need to sleep between
`dispatch`/`resume` and `wait`.

Resume is bound to the config dir that created the session: switching a
profile's `--config-dir` after the fact makes `--resume` fail with
"No conversation found". `thoth resume` refuses up front in that case and
tells you to reroute.

An interrupted tool call has no partial result. The resume prompt tells the
worker to re-check the tree and re-run anything whose result it never saw; do not
expect the interrupted command's output to come back.

## Manual ingest

```
thoth submit -f C:\path\to\result.json --run <run_id>
```

Same validation as the automatic path. Exit 0 and `task_status=in_review` on
success; exit 1 and `task_status=unverified` when the document is rejected.

`submit` is refused only while the run is still live (`queued` or `running`)
**and** the task has not already been marked `unverified` or `in_review` by an
earlier ingest — so it works after a `completed` run, after an `interrupted` or
`failed` run (e.g. you hand-fixed the worktree and want to file the result
without a full reroute), and it can be re-run to replace an earlier submission
(claims are replaced, not doubled).

## Common errors

| message | cause |
|---|---|
| `repo <id> is not initialised; run thoth init in <path>` | you are in a repo that has no `state.db` yet |
| `... is not inside a git repository` | cwd is not under a git working tree; `thoth status` without a TASK lists every registered repository's tasks there instead |
| `id prefix must be at least 6 characters` | shorten-prefix guard; pass more characters |
| `ambiguous id prefix, matches: ...` | two ids share that prefix |
| `task <id> is '...'; dispatch needs queued, interrupted, in_review, unverified, failed or reject` | something is already running for this task, or it has reached a decided state (`pass`, `cancelled`, `draft`) |
| `profile <id> failed its auth check` | `claude auth status --json` said `loggedIn:false` for that config dir |
| `git worktree add failed ... already used by worktree at ...` | a stale worktree holds the branch; the `worktree_error` event has git's exact message |
| `cancel could not kill every process: pid N: ...` | the process was still listed after the kill attempt. The run is settled as `failed` rather than `cancelled`, deliberately — something may still be writing in the worktree. Find the pid with `tasklist /FI "PID eq N"` (Linux: `ps -p N -o pid,comm`, and `cat /proc/N/cgroup` for its group) and deal with it before reusing that worktree |
| `run <id> is '...'; resume needs an interrupted run` | `resume` only works on a run whose status is `interrupted` |
| `submit needs a finished run or an unverified/in_review task` | the run is still `queued`/`running` and the task has not already been marked `unverified`/`in_review` |

## Guided setup: thoth setup

`thoth setup` runs the same checks as `thoth doctor`, in the same order, and
for each one that is not right asks one question before doing anything:

1. **State directory**: shown only. If only the old `~/.agentctl` exists, or
   only `AGENTCTL_HOME` is set, setup prints the fix and stops (exit `1`);
   settle it and run `thoth setup` again.
2. **The three CLIs**: shown only, with the install command or page when one
   is missing. Setup never installs claude, codex or agy.
3. **Accounts**: a registered, active profile that is logged out gets "Log in
   to … now? [Y/n]"; a CLI that works but has no account at all gets "Add an
   account for … now? [Y/n]". Both open the CLI's own login screen through
   `thoth profile login`; a failed login offers "Try again? [Y/n]". A second
   account of the same CLI is never offered; the closing line says how to add
   one.
4. **This repository**: not registered → "Register this repository with thoth
   now? [Y/n]" (`thoth init`).
5. **Checks**: for a Rust (`Cargo.toml`), Node (`package.json`) or Python
   (`pyproject.toml`, `pytest.ini`, `setup.py`, `setup.cfg`) project it lists
   the usual checks that are not in the catalogue yet and asks "Add these …
   check(s)? [Y/n]" (they are added as pending). Then it lists every pending
   or stale check and asks "Approve these … check(s) now? [Y/n]": answering Y
   there is your approval. A check whose program is not on `PATH` is never
   proposed or approved; setup prints how to put its folder on `PATH`
   instead. When the catalogue is empty and setup has something to propose,
   it does not also print doctor's "Add a check, for example" hint.
6. **Live quota**: off → "Turn it on? [y/N]" (default no).
7. **Claude Code plugin**: not installed → "Install the Thoth plugin into
   Claude Code now? [Y/n]" (`claude plugin marketplace add …`, then `claude
   plugin install thoth@thoth`); installed but disabled → "Enable it now?";
   another version → "Update it now (claude plugin update)?". Answering no to
   "Enable it now?" ends this step: the update is not asked about this time.

Pressing Enter takes the default shown in capitals; `n`, anything else, or the
end of input skips that step. Running it again is safe: what is already fine is
only shown with ✓ and not asked about. The only things it writes are what those
answers ask for: `thoth init`, the profile registration `thoth profile login`
does, `thoth checks add` and `approve`, the `live_quota` switch in
`config.json`, and `claude plugin marketplace add`, `install`, `enable` and
`update`. It does not read credentials.

It needs a real terminal: Claude Code's `!`, a pipe, or an MCP tool is not
one, and setup refuses there (exit `1`, nothing changed); it also refuses
inside a run (`THOTH_RUN_ID`). Exit code: `0` when, at the end, no item fails;
`1` otherwise. `--shell powershell|cmd|bash|sh` chooses the shell the printed
commands are written for, as for doctor; `THOTH_LANG=zh-TW` prints everything
in Traditional Chinese. There is no `--json`.

## Health check: thoth doctor

`thoth doctor` checks the setup of this machine and this repository and says, for each item that is not right, how to fix it. It only reads: it does not create the state directory, does not open a database for writing, does not run `thoth init`, and changes no file. The only programs it starts are each CLI's `--version`, the login checks `thoth profile check` already uses (`claude auth status --json`, `codex login status`, `agy models`) and `claude plugin list --json`.

What it checks, in order:

- **Thoth state directory** (`state_dir`): where `THOTH_HOME` (or `~/.thoth`) points, and whether it exists. A missing directory is only a warning; the first command that needs it creates it. If only the old `~/.agentctl` exists, or only `AGENTCTL_HOME` is set, doctor reports it as a failure and prints the `setx` / `export` line, instead of refusing to run as every other command does; the other ways to settle it are under "Coming from agentctl" above.
- **The three CLIs** (`cli_claude`, `cli_codex`, `cli_agy`): each one's `--version`, through the same Windows npm-shim resolution as `profile check`. A CLI that does not run fails when a registered profile uses it, and only warns otherwise. The fix is the install command (or the agy install page); doctor never installs anything.
- **Profiles** (`profiles`, then one `profile:<id>` per profile): that `global.db` has at least one profile, and that each profile is logged in. A logged-out profile gets `thoth profile login <id>`; with no profile at all, the fix is `thoth profile login --harness claude-code|codex|antigravity`. Each profile's heading shows its stored tier (`Profile claude-a (Max)`, in Traditional Chinese `帳號 claude-a（Max）`); the detail is still what this check just saw.
- **This repository** (`repo`): that the current directory is in a git repository and that `thoth init` has been run for it.
- **Checks** (`checks`): the approved checks of this repository. A check whose program is not on `PATH` right now fails, even when `thoth checks list` shows it as `approved` (an approval made while the program was missing still matches): `verify` could not run it. The fix names the program and shows how to put its folder on `PATH`; when the missing program is in `~/.cargo/bin` or `~/.local/bin`, the fix names that folder instead of a placeholder. A check whose pin changed since approval (`thoth checks list` calls it `stale`) is a warning, listed with what changed; approve it again in a terminal with `thoth checks approve` after looking at the change. Pending checks and an empty catalogue are warnings too. A check added with `--exe-pin path` is listed as `program pinned by path only`; that is information, not a warning.
- **Config file** (`config`): that `config.json`, when present, is a JSON object with only the known keys. A broken file is a warning: every setting stored in it is ignored (environment variables and defaults still apply). An unknown key is a warning and blocks nothing. `secrets.json` (the two API keys) is checked the same way, and on Linux it must be readable by you only.
- **Claude Code plugin** (`plugin`): that `claude plugin list --json` shows an enabled `thoth@...` plugin of the same version as this thoth (a marketplace install's `-<hash>` suffix is fine). When claude itself cannot run, this item is `unknown`. Inside Thoth's own repository (the root has `.claude-plugin/marketplace.json`) the install command points at that local folder; anywhere else it is `claude plugin marketplace add johnny-lo/Thoth`. When the local folder's path contains whitespace, the printed command has the path in double quotes.

Exit code: `0` when nothing fails (warnings are fine), `1` when at least one item fails. `--json` prints the machine form described in `docs/adapter-usage.md`.

Every command in a fix is written for your shell. Doctor detects it from the environment: on Windows Git Bash (`MSYSTEM` or `SHLVL`, which is also what Claude Code's `!` runs), then PowerShell (a per-user module folder in `PSModulePath`), then cmd (`PROMPT`), else PowerShell; on Linux, `bash` when `$SHELL` is bash, else `sh`. `--shell powershell|cmd|bash|sh` overrides it. Known limit: a cmd window opened from PowerShell looks like PowerShell to doctor; use `--shell cmd` there.

`THOTH_LANG=zh-TW` prints the human output in Traditional Chinese; anything else (or unset) is English. It does not change `--json`.

## Troubleshooting

**`program not found` (Windows).** `thoth profile check` says `could not run
codex: program not found` when the CLI is npm-installed and only a `codex.cmd`
shim exists. Current thoth resolves the npm shim itself (see "Windows:
npm-installed `codex` and `claude`" above). If it still fails, the CLI is not on
PATH or is not an npm shim: set `THOTH_CODEX_BIN` or `THOTH_CLAUDE_BIN` to the
real executable.

**An `interrupted`/`crash` that was really a slow pipe.** The runner gives the
harness's stdout 10 seconds to drain after the child exits, then gives up so that a
grandchild holding the pipe open cannot wedge the run. If the `result` line had not
arrived by then, the run has no `result` event and is classified `crash`. The
signature is: `runs.exit_code` is 0 but the task is `interrupted` with
`interrupted_reason=crash`, and the last line of `runs\<run_id>\stream.jsonl` is not
a `result`. Confirm with the run's final `status_change` event, which carries
`stream_drain_timed_out: true`. This is not a real crash — resume or re-dispatch
rather than going hunting for a bug in the worker.

**A worktree directory you deleted by hand.** `thoth` removes a task's clean
worker worktree itself once the task is `pass` (a reviewer's PASS, or
`review record`) or `cancelled` (`cancel`, `task close`); the review worktree
goes at the next `task create`, `task close` or `thoth worktrees prune`.
Worktrees of tasks in any other status are never touched (see "Worktree
cleanup" in `docs/phase2/usage.md`). If you delete one manually, run
`git worktree prune` in the repo before dispatching or resuming into the same
task/attempt again, or git's own bookkeeping will still think the worktree
exists - or run `thoth worktrees prune`, which runs `git worktree prune` every
time.

**A run stuck at `running` with a stale heartbeat.** `thoth status <task>`
shows `heartbeat_stale: true` once a `running` run's heartbeat is more than 15
seconds old — its runner has probably died or lost the ability to write to the
database. `thoth cancel <run>` escalates past the (now unreachable) runner
and settles the row directly.

**`runner-error.log`.** A detached runner has no stderr a human will ever see,
so anything it cannot otherwise report (a DB write that failed, a kill that did
not take, a setup failure before the row could be updated) is appended to
`runs\<run_id>\runner-error.log` as a last resort. Check it whenever a run's
outcome looks inconsistent with `stream.jsonl`.

**Dispatching to a `rate_limited` profile.** This is allowed (see "Quota
interruptions" above) but likely to be interrupted immediately if the window
has not actually reset yet — that is expected, not a bug in the warning.

**`cancel` reports `failed` with "not killing" in `error_text` even though the
worker looks dead to you.** Check whether the shell running `cancel` has the
same `THOTH_CLAUDE_BIN` as the shell that ran `dispatch` for that run (see
"Cancelling" above). A different value there makes `cancel` unable to confirm
the pid's image matches the harness it expects, and it now refuses to call
that safe rather than risk a false `cancelled` over a still-live worker. That
first `cancel` already settled the run `failed`, though, so re-running
`cancel` will not help — it only accepts a run that is still `queued` or
`running`. Instead, kill the worker by hand: get its pid from
`thoth status <task>` (`worker_pid`), run `taskkill /PID N /T /F`, confirm
it is gone with `tasklist /FI "PID eq N"` (Linux: `cat /proc/N/cgroup` gives its
`.../worker-N` group; `echo 1 > /sys/fs/cgroup<that path>/cgroup.kill`, then
`ps -p N` to confirm), then re-dispatch — a `failed` task
is re-dispatchable. Use the same `THOTH_CLAUDE_BIN` as the original
`dispatch` next time to avoid landing here again.

## Phase 1 limits

Worktrees are never removed, and neither are blobs or evidence rows (no `gc`
yet). Phase 1 had no reviewer and no evidence capture; Phase 2 adds both — see
`docs/phase2/usage.md`. There is still no sandbox, no automatic merge and no
daemon. `attempt` increments only on a dispatch out of `reject` (Phase 2's
rework loop). On neither Windows nor Linux is the worker sandboxed: it can reach the whole
filesystem, including `.git/hooks` shared across worktrees. Only dispatch work
you would run yourself.
