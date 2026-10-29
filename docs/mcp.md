# MCP server

`thoth mcp` exposes the commander workflow to any MCP client over line-delimited JSON-RPC on standard input and output. Start it with:

```text
thoth mcp [--default-repo PATH] [--max-wait DURATION] [--max-result-bytes N] [--plugin-root PATH]
```

`--default-repo` supplies the working repository when a call omits `repo`. `--max-wait` defaults to 30 seconds and may be 1 second through 1 hour. `--max-result-bytes` defaults to 60000 and must be at least 200.

`--plugin-root` is the Claude Code plugin folder. When it is not given, the server reads `CLAUDE_PLUGIN_ROOT`, which Claude Code puts in the environment. With neither, nothing is compared. A value containing `${` is treated as not expanded and ignored.

The initialize handshake supports 2025-11-25, 2025-06-18, 2025-03-26, and 2024-11-05. The metadata-per-request generation supports 2026-07-28; each request must provide its protocol version and client capabilities. In both generations `capabilities` is `{"tools":{"listChanged":true}}`: the server sends `notifications/tools/list_changed` when calls start going to a replaced executable whose tool list differs, or go back to this server's own tools after that executable ended (see "A replaced executable" below).

In that generation, the `tools/list` and `server/discover` results also carry `ttlMs: 0` and `cacheScope: "private"`. The 2026-07-28 schema requires both fields on list results, and Claude Code rejects a tool list that lacks them. The values keep the old "do not cache" behavior: the tool list is tied to the locally installed version and the state belongs to each user. Results for the initialize-handshake generation do not carry these fields.

## Tools

| Tool | CLI command | Parameters |
|---|---|---|
| `thoth_init` | `init --json` | `repo?` |
| `thoth_status` | `status [TASK] --json` | `task?`, `repo?` |
| `thoth_profile_usage` | `profile usage [ID] --json` | `id?` |
| `thoth_models` | `models --json` | none |
| `thoth_recommend` | `recommend --role=... [--task=...] [--tables] --json` (with `live-quota` on it may first query codex and agy and write the readings to `global.db`; with `scores-auto-refresh` on it may first refresh the score caches; see docs/phase2/usage.md). Its document carries, per account, the quota facts and windows, the models it can run now with each model's matching scores in the three external tables and Thoth's own usage with runs left, plus the model names no table matched (CROSS-1); with `tables` true the three full filtered tables; with `task` the task's reject history | `role`, `task?`, `tables?`, `repo?` |
| `thoth_task_create` | `task create -f FILE --title=... [--base=...] --json` | `title`, `spec`, `base?`, `repo?` |
| `thoth_task_update_spec` | `task update-spec TASK -f FILE --json` | `task`, `spec`, `repo?` |
| `thoth_task_close` | `task close TASK --reason=... --json` | `task`, `reason`, `repo?` |
| `thoth_dispatch` | `dispatch TASK --profile=... --json` | `task`, `profile` (a profile id), `model?`, `timeout?`, `effort?`, `reason?`, `allowed_tools?`, `allow_attempt?`, `skip_spec_review?`, `repo?` |
| `thoth_wait` | `wait TASK` or `wait --run=RUN`, with `--timeout=SECONDS --json` | exactly one of `task`/`run`, `timeout_seconds?`, `repo?` |
| `thoth_events` | `events --since=N [--task=...] --json` | `since?`, `task?`, `repo?` |
| `thoth_evidence_list` | `evidence list TASK --json` | `task`, `repo?` |
| `thoth_evidence_show` | `evidence show CODE --task=... [--bytes=N] --json` | `code`, `task`, `bytes?`, `repo?` |
| `thoth_review_packet` | `review packet TASK --out=FILE` | `task`, `repo?` |
| `thoth_review_record` | `review record TASK -f FILE --json` | `task`, `review`, `repo?` |
| `thoth_review_start` | `review start TASK --profile=... --json` | `task`, `profile` (a profile id), `model?`, `timeout?`, `effort?`, `reason?`, `edit?` (boolean; true adds `--edit`), `repo?` |
| `thoth_review_confirm` | `review confirm TASK --profile=... --model=... --json` | `task`, `profile` (a profile id), `model`, `timeout?`, `effort?`, `reason?`, `repo?` |
| `thoth_review_show` | `review show TASK [--full] --json` | `task`, `full?` (boolean; `claims` is null without it), `repo?` |
| `thoth_spec_review_record` | `spec review record TASK -f FILE --json` | `task`, `review`, `repo?` |
| `thoth_spec_review_show` | `spec review show TASK --json` | `task`, `repo?` |
| `thoth_spec_review_start` | `spec review start TASK --profile=... [--edit] --json` | `task`, `profile` (a profile id), `model?`, `timeout?`, `effort?`, `reason?`, `edit?` (boolean), `repo?` |
| `thoth_spec_review_confirm` | `spec review confirm TASK --profile=... --json` | `task`, `profile` (a profile id), `model?`, `timeout?`, `effort?`, `reason?`, `repo?` |
| `thoth_resume` | `resume RUN --json` | `run`, `repo?` |
| `thoth_cancel` | `cancel RUN --json` | `run`, `repo?` |
| `thoth_verify_start` | `verify RUN --json [--at-base] [--dry-run] [--timeout=...]`, started in the background | `run`, `at_base?`, `dry_run?`, `timeout?`, `repo?` |
| `thoth_job_status` | none: reads the job's files | `job_id`, `wait_seconds?` |
| `thoth_checks_list` | `checks list --json` | `repo?` |
| `thoth_checks_add` | `checks add ID --cmd=... [--shell=...] [--cwd=...] [--exe-pin=...] [--input=...]... --json` | `id`, `cmd`, `shell?`, `cwd?`, `exe_pin?`, `inputs?`, `repo?` |
| `thoth_checks_approve` | `checks approve [--run=RUN] --reason=... --json` | `reason`, `run?`, `repo?` |
| `thoth_checks_remove` | `checks remove ID --json` | `id`, `repo?` |
| `thoth_config_list` | `config list --json` | `repo?` |
| `thoth_doctor` | `doctor [--shell=...] --json` | `shell?`, `repo?` |
| `thoth_spec_lint` | `spec lint FILE --json` | exactly one of `spec`/`file` (the absolute path of an existing file), `repo?` |
| `thoth_task_accept` | `task accept TASK --reason=... --json` (only an `in_review` docs, research or review task; no review is recorded) | `task`, `reason`, `repo?` |
| `thoth_task_result` | `task result TASK [--run=RUN] --json` (read-only) | `task`, `run?`, `repo?` |
| `thoth_guide` | `guide --json` | none |

`base` of `thoth_task_create` is where the task starts (a commit or a branch); without it the task starts at the HEAD of the worktree the command runs in: `repo`, else `--default-repo`, else the server's working directory. Pass a linked worktree of a long-lived branch as `repo` to start there.

`thoth_guide` returns the commander guide; setup for other tools is in [integrations.md](integrations.md).

`edit` of `thoth_review_start` (RFIX-1) lets a code reviewer attach a small fix to its REJECT; Thoth applies it within fixed bounds on `thoth/TASK/aN-fix` and `thoth_review_show` reports `pending_fix` true. Verify the fix with `thoth_verify_start` on the edit review's run, then `thoth_review_confirm` starts the confirmer, which only confirms the fix (ok or revise per edit, no new review); `model` is required and the edit review's own model at its own effort is refused. Wait for either run with `thoth_wait` (`run`), whose document carries `pending_fix`. See "Reviewer fixes, author confirms" in `docs/phase2/usage.md`.

`review` of `thoth_spec_review_record` is the spec-review document (see "Spec review before the first dispatch" in `docs/phase2/usage.md`), and `skip_spec_review` of `thoth_dispatch` is `--skip-spec-review`: the reason for dispatching an important or high task without an APPROVE of its current spec, recorded in a `spec_review_skipped` event. `thoth_spec_review_start` starts Thoth's own spec reviewer and returns its run id; wait for it with `thoth_wait` (`run`) and read the verdict with `thoth_spec_review_show`. With `edit` true (SR-3) the reviewer may also propose edits, which Thoth applies into a proposed spec kept in the REVISE event; `thoth_spec_review_show` then reports `pending_edits` true, and `thoth_spec_review_confirm` starts the author's check of those edits (an APPROVE replaces the task's spec; wait for it the same way). `thoth_spec_review_show` stays read-only and has no `proposed_out`: the proposed spec's bytes are taken with `thoth spec review show TASK --proposed-out FILE` in a terminal. See "Reviewer edits, author confirms" in `docs/phase2/usage.md`.

`thoth_doctor` is the read-only health check (`thoth doctor`, see "Health check: thoth doctor" in `docs/phase1/usage.md`). When some item fails, the CLI exits 1, but the tool still returns the full report with `isError: false` and `structuredContent.ok` set to `false`. Known limit: the server resolves the state directory (`THOTH_HOME`) before it runs any tool, so on a machine where only the old `~/.agentctl` exists, `thoth_doctor` gets the existing refusal instead of a report; run `thoth doctor` in a terminal there.

`thoth_spec_lint` is the read-only `thoth spec lint` (see "Spec lint" in `docs/phase2/usage.md`): the fixed rules `thoth_task_create`, `thoth_task_update_spec` and `thoth_spec_review_start` apply first. A `spec` object is written to a temporary file, linted and deleted. When the spec has an error, the CLI exits 1, but the tool still returns the full report with `isError: false` and `structuredContent.ok` set to `false`; giving both `spec` and `file`, neither, or a `file` that is not the absolute path of an existing file is a tool error.

`thoth_config_list` is the read-only `thoth config list`: it returns every setting's effective value and source, the counts kept elsewhere, and for the API keys only `set` or `unset`. Changing a setting or storing an API key is a human's step (`thoth config set`, `thoth config unset`, `thoth config set-secret`, `thoth config unset-secret`); this server has no such tool and never will.

Breaking change (RJ-1): `profile` of `thoth_dispatch`, `thoth_review_start` and `thoth_spec_review_start` is a profile id only; `auto` in `profile`, `model` or `effort` is refused, because Thoth no longer picks the account, model or effort (see "Automatic dispatch" in `docs/phase2/usage.md`). Give `model` and `effort` on the first dispatch or review of an attempt (a reroute in the same attempt keeps the previous run's) and on every spec review start; `effort` can be left out only for a model without an effort setting (haiku) or (AGY-1) an agy model whose name ends in its level (`gemini-3.8-flash-high`: that level is recorded as `effort` and no flag is sent); any other agy model takes `effort` `low`, `medium`, `high`, `xhigh` or `max` and its launch gets `--effort LEVEL`. Leaving a required `effort` out is a tool error (`isError: true`, the CLI's `--effort is missing` error) and starts no run. `thoth_wait` with `run` returns the CLI's `wait --run` document, `tools_unknown` included (see `docs/adapter-usage.md`). `thoth_recommend` lists each profile's quota and the models it can run, each with its matching external scores and Thoth's own usage, the names no table matched, the full filtered tables when `tables` is true and, with `task`, the task's reject history to choose from (RJ-2, CROSS-1; it still picks nothing). `reason` is `--reason`: why this profile, model and effort were chosen, required on every launch while `require-reason` is on (see `require-reason` in `docs/adapter-usage.md`).

Every `repo` must be an absolute path to an existing directory. Otherwise the server uses `--default-repo`, then its own working directory.

## Results and limits

A successful tool result contains text holding the CLI JSON document, the unchanged object as `structuredContent`, and `isError: false`. Failures set `isError: true`; their structured object contains `ok`, `exit_code`, `error`, `error_document`, and `stdout_document`. Thus clients receive both the stable CLI error and useful process diagnostics.

When a successful JSON document exceeds the configured result size, the server writes it under `$THOTH_HOME/mcp/out/` and returns its absolute path and byte count. The caller can read that file or narrow `events` with `since` and `evidence show` with `bytes`.

Tool results, and the large results saved under `mcp/out`, are masked as described under "Secret masking" in [adapter-usage](adapter-usage.md).

`thoth_wait` deliberately caps each wait. Many agent hosts time out a single tool call after 60 to 300 seconds, so dispatch and review start return immediately and callers wait in bounded installments.

## Verify jobs and the check catalogue

`thoth verify` re-runs the approved checks of this repository, not the commands a worker declared, and it can take many minutes. A single tool call cannot wait that long, so verify takes two steps:

1. `thoth_verify_start` starts `thoth verify RUN --json` as a background job and returns at once with `job_id`, `status: "working"`, `poll_tool` and `suggested_poll_seconds`. `at_base` and `dry_run` are JSON booleans; `timeout` is a duration string such as `600s` or `20m`. A parameter that fails validation is a tool error and starts nothing.
2. `thoth_job_status` reports the job. `wait_seconds` (0 through `--max-wait`, default 0) makes the call wait that long for a running job to finish. The result holds `job_id`, `kind`, `status`, `exit_code`, `result`, `stderr_tail` (the last 20 lines, at most 4000 bytes), `started_at`, `finished_at` and `recovered`.

| `status` | Meaning |
|---|---|
| `working` | The job is still running; `exit_code`, `result` and `finished_at` are null. Ask again. |
| `completed` | Verify wrote its JSON document; it is in `result`. A failed or refused check, or a repository with no approved checks (`result.reason` is `no_approved_checks`), gives `exit_code` 1 and `result.ok` false, and is still `completed`. |
| `failed` | Verify exited without a JSON document, for example because the run does not exist; `result` is null and `stderr_tail` says why. |
| `unknown` | This server did not start the job and it has no result yet. The job may still be running under another server, or that server may be gone. `hint` says to look at `thoth_evidence_list` for evidence labelled `verify: ` first, and to start again only if there is none. |

None of the four states is a tool error. `isError` is true only for invalid parameters: a `job_id` that is not 26 digits and upper-case letters or that names no job, a `wait_seconds` outside 0 through `--max-wait`, a parameter of the wrong type, or a missing required parameter.

Each job has a directory, `$THOTH_HOME/mcp/jobs/JOB_ID/`, holding `meta.json`, `stdout.json`, `stderr.txt` and `exit.json`; `thoth_job_status` writes `exit.json` the first time it sees the job finished. Nothing removes these directories; old ones can be deleted by hand.

After a server restart the job is still reported from those files, with `recovered: true`. If `exit.json` is missing but `stdout.json` holds the document, the job is `completed` with a null `exit_code`. A job that is still running when the server exits is neither killed nor waited for: it finishes on its own and records its evidence.

`thoth_checks_list`, `thoth_checks_add`, `thoth_checks_approve` and `thoth_checks_remove` manage the catalogue. A check added through `thoth_checks_add` is pending and is not run until it is approved. The commander approves with `thoth_checks_approve`, whose `reason` is required and is recorded with the approval (`approved_by` `commander_mcp`) for the reviewer to read in the packet; with `run` it reads that settled run's worktree and also approves the protected test files that run changed, which `thoth_review_start` and a PASS `thoth_review_record` wait for. A person can also approve with `thoth checks approve` in a terminal. Adding a check with an id that is already approved makes it pending again. `exe_pin` is `hash` (the default) or `path`; `path` pins only the program's path, for a program outside the repository that updates itself, and is approved like any other check. `thoth_checks_remove` takes effect at once and needs no approval: later verify jobs do not run that check. The next verify result (thoth_job_status's result) and thoth_status list an approved check that was removed or replaced since the previous verify under missing_since_last_verify; report it to the human.

## Safety and maintenance

The server never exposes `checks protect`, `checks unprotect`, `config set`, `config unset`, `config set-secret`, `config unset-secret`, `hook`, or `run-worker`. Check approval goes through `thoth_checks_approve`, always with a reason, or through `thoth checks approve` in a terminal; the review packet shows every approval, who made it and why. It also does not expose account registration or login. Log in with thoth profile login or thoth setup, which only a person at a terminal can run. Runs consume quota from the user's configured subscription accounts; users remain responsible for those accounts and their login state.

The MCP process does not open the database. Each tool starts the same executable as `thoth COMMAND --json`, keeping MCP and CLI on one contract. After calls are handed to a replaced executable, the new `thoth mcp` starts these CLI processes itself. `thoth_job_status` starts nothing: it reads the files in the job's directory. To add a parameter or tool, add a row to the table in `src/mcp/tools.rs`.

## Claude Code plugin

Install the marketplace and plugin with two commands:

```text
claude plugin marketplace add johnny-lo/Thoth
claude plugin install thoth@thoth
```

For a local checkout, add its root directory as the marketplace source, then install the same plugin name:

```text
claude plugin marketplace add /absolute/path/to/thoth
claude plugin install thoth@thoth
```

The `thoth` executable must already be on `PATH`. If it is absent, the skill remains available, but the tool half of the plugin reports an MCP connection failure.

After installation, Claude Code exposes the `/thoth:thoth-dispatch` skill and the `plugin:thoth:thoth` MCP server. Its tools have names such as `mcp__plugin_thoth_thoth__thoth_status`.

### The plugin must match the executable

Claude Code installs a cached copy of the plugin, so it can drift from the `thoth` executable. On every tool call the server compares the installed `skills/thoth-dispatch/SKILL.md` with the one built into the executable (only the line-ending difference between `\r\n` and `\n` is ignored), and the `version` in `.claude-plugin/plugin.json` with the executable's version.

When they differ the server still starts. These 18 tools that change state are refused, start no process and write nothing: `thoth_init`, `thoth_task_create`, `thoth_task_update_spec`, `thoth_task_close`, `thoth_task_accept`, `thoth_dispatch`, `thoth_review_start`, `thoth_review_confirm`, `thoth_review_record`, `thoth_spec_review_record`, `thoth_spec_review_start`, `thoth_spec_review_confirm`, `thoth_resume`, `thoth_cancel`, `thoth_verify_start`, `thoth_checks_add`, `thoth_checks_approve`, `thoth_checks_remove`. The result has `isError` true and an `error` that starts with `refused: the installed Thoth plugin at …`; `error_document.reason` is `plugin_mismatch` and `error_document.differences` lists any of `version` (the two versions differ), `skill` (the skill file differs), `skill_missing` (the skill file cannot be read) and `manifest_missing` (plugin.json cannot be read, is not a JSON object, or has no string `version`).

The other tools work as usual, and the first `content` block of their result is a `warning: …` line. The `instructions` of initialize and server/discover end with the same line.

To fix it, run `claude plugin marketplace update thoth`, then `claude plugin update thoth@thoth` (if the version did not change: `claude plugin uninstall thoth@thoth`, then `claude plugin install thoth@thoth`), then start a new Claude Code session. If the plugin is the newer one, rebuild and reinstall `thoth` instead; `thoth doctor` shows both versions. The check repeats on every call, so a repaired plugin needs no server restart.

A replaced executable is handed the calls. Only the file at the path the server was started from is looked at, and only before a request with an id: its length and modification time each time, and its bytes, compared with the file the server is running, only when those changed. Putting back identical bytes, or an empty path while an update is halfway, changes nothing. `initialize`, and a request whose protocol metadata the server refuses, are answered as before and do not look. When the file holds new bytes, the server starts it as `thoth mcp` with the server's own arguments, working directory and environment (its stderr is the server's), repeats the client's last `initialize` to it when the client sent one (the new executable must answer the same protocol version the client got) followed by `notifications/initialized`, and asks for its tool list, all within 30 seconds. From then on each line the client sends is copied to the new executable unchanged, byte for byte with a `\n` added, `initialize` and notifications included; only lines that are not JSON-RPC requests at all are still answered by the server. Each line the new executable sends is copied back the same way as soon as it arrives, notifications too, in the order it sent them; other lines (not JSON objects, requests to the client, answers to no waiting request) are dropped with a `thoth mcp: dropped a line from the new executable` line on stderr. The first tool result that comes back starts with a `notice: the thoth executable at PATH was replaced; calls now go to the new executable (version V)` block. When the new tool list differs from the server's own, `notifications/tools/list_changed` follows that answer, and Claude Code fetches the new list in the same conversation. `thoth_job_status` for a job this server started with `thoth_verify_start` is still answered by this server, which alone can record how the job ended (`recovered: false`); other job ids go to the new executable. A second replacement is handled by the new executable the same way, so the processes form a chain, each answering for its own jobs.

When the hand-over fails, the server answers with its old tool list, and the first `content` block of each tool result is `warning: the thoth executable this MCP server was started from (PATH) has been replaced; handing calls to the new executable failed (CODE: DETAIL). This server still uses the old tool list. It will try again on the next call. To switch now, run /mcp in Claude Code and reconnect plugin:thoth:thoth, or start a new conversation.`, before the plugin-mismatch line when there is one; for `handshake_failed` and `exited` the retry sentence is `It will try again when that file changes.` CODE is `spawn_failed` (the file could not be started: still being written, not an executable, no execute permission), `handshake_failed` (it started but did not finish the handshake in 30 seconds, answered an error or another protocol version, or ended; it is killed) or `exited` (after the hand-over its output ended or writing to it failed; it is killed, and `notifications/tools/list_changed` is sent, as the client may hold another list). After `spawn_failed` the next call tries again; after the other two, only once the file at the path holds other bytes again. A file at the path that the server cannot read counts as unchanged: nothing is started from it, and no failure is recorded, until it can be read. A request already written to the new executable is never run again by the server, since it may have run it: a tool call gets a tool error and any other method JSON-RPC error -32603, both saying the new thoth executable may already have run this call and to check with `thoth status` or `thoth_status` before retrying. Only these fallbacks need `/mcp`. A server started from a `thoth` that does not have the hand-over only shows a warning and keeps its old tools, so the first time a `thoth` with the hand-over is installed, an open conversation still needs `/mcp` (select `plugin:thoth:thoth`) once, or a new conversation.

For contributors: when `plugins/thoth` changes, bump the version in `Cargo.toml` and `plugins/thoth/.claude-plugin/plugin.json` and append a line to `plugins/thoth-fingerprint.txt`; `cargo test --test plugin_fingerprint` fails otherwise.

### Thoth panel

The plugin also brings a panel to Claude Code. Type `/thoth` to open it. It shows the repository the session is in and has five sections, sorted by who has to act:

- **Needs you**: only what you must do yourself. If the decisions plugin is installed and has cards waiting, one row says how many. Without cards, the section says "none".
- **Commander is on it**: tasks that are in review, unverified, interrupted, failed or rejected. These are for your commander (the Claude session that dispatches work) to handle. Each has two buttons: **Ask the commander** sends your session a short message naming the task and its status, and **Open report** runs `thoth report --open`. **Watch worker** opens a second pane showing what the latest run of that task is doing: the files it read, the commands it ran with their exit codes, the files it wrote, what the worker said, errors and how it ended. It updates with the panel and stops when you close it. After the tasks, each check that waits for approval (never approved, or changed since) gets a row of its own, with no button: the commander approves checks with a reason the reviewer sees, and you can still run `thoth checks approve` in a terminal in this repository. The panel itself never approves. New (PANEL-3): a task with a run queued or running (`live_run` in `thoth status --json`) is never here, whatever its status: a task in review whose reviewer is at work is in progress. Only a task in review, unverified, interrupted, failed or rejected with no live run waits on the commander. A failed, interrupted, unverified or rejected task has two more buttons: **Close task** and **Dispatch again** (see below).
- **In progress**: tasks that are queued or running, and (new, PANEL-3) every task with a live run. A task whose worker has stopped reporting heartbeats is marked "may be stuck". Each has a **Watch worker** button too. It is not available yet for an Antigravity account. New (PANEL-3): while a review runs the line reads `● TITLE under review · ACCOUNT · MODEL` (`審查中`), while a spec review runs `● TITLE spec under review · ACCOUNT · MODEL` (`規格審查中`); "may be stuck" and, outside a repository, the repository's name follow as before. A task with a live run has a **Cancel** button (see below).
- **Account quota**: every registered profile, with the share of each quota window left and when it resets. A profile that has run out is shown in red. When Thoth knows a label for an account (`plan_label` in `thoth profile usage --json`: its tier, or else the profile's `--label`), the account is written `claude-a (Max)`, or `claude-a（Max）` in Traditional Chinese. New (PANEL-3): when `thoth profile list --json` gives the account a `login_name` (the part of its login email before `@`), it comes first: `claude-a (alice, Max)`, `claude-a（alice，Max）`, or `claude-a (alice)` without a label.
- **Decision table**: one line — how many runs the table holds, from how many computers, how many launches there were in the last 7 days and how many of them were started without a reason — with an **Open decision table** button that runs `thoth report --open --section decision-table`. The table itself is on the status page.

Each Claude Code window open on this repository pops up a notification once when a task of this repository goes into review, passes, is rejected, becomes unverified, fails or is interrupted, and when an account runs out of quota. Events that were already there when the panel first loaded under this Claude Code config folder are not announced; a window opened later announces what happened since a window last looked. While something is running, with the commander or waiting on you, a line above the prompt shows how many tasks are running, how many are with the commander (checks waiting for approval count there too), how many things need you and the account with the least quota left, with a button that opens the panel. Whatever other plugins draw in that place stays, under the line. The line is hidden when there is nothing to show. The account in the line above the prompt, the first line of the Watch worker pane, the out-of-quota notification and the under-review lines are written the same way. A task under review counts as running in the line above the prompt, not as with the commander. The message **Ask the commander** sends never carries a login name.

New (PANEL-3): cancel, close and dispatch again, each in two presses. The first press runs no command that changes anything: a sentence and two buttons appear under the task's line, and only the second press acts. **Back** (`返回`) runs nothing.

- **Cancel** (`取消`), on a task in progress with a live run. The sentence depends on the run: for a worker, `Stop the run of TITLE? This is final: the task becomes cancelled and cannot be dispatched again; redoing it needs a new task.`; for a review, `Stop the review of TITLE (ACCOUNT, MODEL)? The task stays in review; the commander can start the review again.`; for a spec review, `Stop the spec review of TITLE? The task stays where it was; the spec review can be started again.` **Yes, cancel** (`確定取消`) runs `thoth cancel RUN --json` for the run that was live at the first press.
- **Close task** (`關閉任務`), on a failed, interrupted, unverified or rejected task waiting on the commander (not on a task in review: the panel cannot tell a document task apart). `Close TITLE? It becomes cancelled and drops out of the list; it cannot be dispatched again.` The second press runs `thoth task close TASK --reason "closed from the Thoth panel" --json`.
- **Dispatch again** (`重派`), on the same tasks. The first press only reads `thoth status TASK --json` and takes the newest implementer run's account, model and effort. Without one, a notification says to ask the commander. When that run stopped because the account's quota is used up or it is logged out, the panel shows that run's `next_step` and only **Back**. Otherwise: `Dispatch TITLE again on ACCOUNT with MODEL, effort EFFORT, the same as its last run? A new run starts and uses that account's quota.` (without the effort part when the run had none). The second press runs `thoth dispatch TASK --profile PROFILE --model MODEL --effort EFFORT --reason "dispatched again from the Thoth panel: same profile, model and effort as run RUN" --json` (no `--effort` when the run had none). The panel never picks another account, model or effort.

The commands run in the task's repository (outside one, the task's own). When one exits 0 a notification says `Cancelled: TITLE`, `Closed: TITLE` or `Dispatched again: TITLE`; otherwise it shows thoth's error (`dispatch` may refuse, for example when the login check fails). Each has 60 seconds; after that a notification says `thoth did not answer in time; the panel will show what happened`. The panel refreshes right after. A sentence still open goes away, running nothing, when the task leaves its section, its live run changes, or the panel reloads; pressing the second button again while the command runs does not run it twice. What becomes of the task is what the command does: a cancelled worker run makes the task cancelled (it leaves the list), a cancelled review leaves it in review, a cancelled spec review leaves it where it was, a closed task is cancelled, and a task dispatched again is running.

The panel refreshes every 10 seconds while any task is in progress (a review counts), every 60 seconds otherwise, and at once when you open it or send a message. It runs `thoth status --json`, `thoth profile usage --json`, `thoth events --since N --json`, `thoth checks list --json` (at most once a minute, but always when you open the panel), `thoth models --json` (at most once a minute, but always when you open the panel) and (new, PANEL-3) `thoth profile list --json` for the login names (at most once a minute, but always when you open the panel; when it fails the accounts are written without names), and, while a run pane is open, `thoth status TASK --json` and `thoth transcript RUN --offset N --json`. Outside a git repository it skips `thoth checks list --json` and `thoth events --since N --json`, and runs the two run pane commands in the watched task's repository. Changed (PANEL-3): the panel never approves checks and never makes a first dispatch; cancel, close and dispatch again (`thoth status TASK --json`, then `thoth cancel`, `thoth task close` or `thoth dispatch`) each need two presses.

When Claude Code is opened in a folder that is not a git repository, the panel still opens. A yellow line under the title says the folder is not a repository, that the tasks of every repository are shown, and that dispatching needs a Claude Code session in a repository. Every task line ends with the name of its repository. Quota, the decision table, the line above the prompt and the buttons work as usual; there are no notifications and no checks waiting for approval, since both belong to one repository.

Three settings are changed in `/config`: `language` (`en`, the default, or `zh-TW`), `thoth_home` (when set, passed to `thoth` as `THOTH_HOME`) and `thoth_bin` (the `thoth` executable to run; empty means the one on `PATH`).

The panel exists only in Claude Code. Other MCP clients keep using the tools and the skill.
