---
name: thoth-dispatch
description: Use when implementation should be delegated to another subscription account through Thoth, or when a Thoth task is running, awaiting review, interrupted, or unverified.
---

# Dispatching implementation through Thoth

## Overview

You are the commander. You plan and review; a registered worker profile implements through `thoth`, which records evidence of what the worker actually ran. Judge from that evidence, not from the worker's prose, and record your judgment in Thoth. Never implement the delegated task yourself.

Thoth does not choose a profile, model or effort for you. You choose them on every launch and say why; Thoth records each launch's choice, your reason and its outcome (a `launch_choice` event, the shared records and the decision table of `thoth models`). `thoth recommend` gives facts only: quota, the external tables, Thoth's own usage and the task's history.

Do not guess file formats or output keys. Every shape needed by this workflow is in this document. The full contract is under `docs/` in the Thoth repository.

## Setup

- `thoth` must be on `PATH`. Check with `thoth --version`. If it is not found, stop and ask the human to install it.
- The state directory is `THOTH_HOME`, or `~/.thoth` when that variable is unset.
- At least one worker profile must be registered and logged in. Use `thoth profile list` to see the actual profiles. Ask the human to register or log in a profile according to the project documentation if none is ready; the commander does not perform login. Names such as `claude-b` and `codex-a` below are examples only, not fixed accounts.
- Put scratch files such as specs and reviews in the commander's own temporary directory, never in the repository or a worktree. Write UTF-8 without a BOM.
- First run `thoth status --json` from the repository. If it says the repository is not initialized, run `thoth init --json` once and retry. Continue an existing matching task from its current status instead of creating a duplicate. Outside a git repository it lists the open tasks of every registered repository instead (`outside_repo` is true and each task carries its `repo`); dispatch, verify and review still have to run inside a repository.

## 1. Specify the task

```json
{
  "goal": "Add pub fn triple(x: i32) -> i32 to src/lib.rs",
  "acceptance_criteria": [
    "AC1: src/lib.rs defines pub fn triple(x: i32) -> i32 returning x * 3",
    "AC2: a test in tests/ asserts triple(7) == 21 and cargo test passes"
  ],
  "constraints": ["Write the failing test first", "No new dependencies"],
  "non_goals": ["Changes outside src/lib.rs and tests/"],
  "excluded_alternatives": [],
  "risk": "normal",
  "task_type": "code",
  "difficulty": "normal"
}
```

- Use JSON only. `goal` and `acceptance_criteria` are required.
- The `AC1:` prefix is the criterion id the review must use.
- Every criterion must be checkable from a diff or command output. Put no code in the spec.
- A criterion that names a test must say which interface the test checks through: a CLI command, its `--json` output, or a public function. A test that reads the database or other internal state directly, only compares a fixed string, or only checks the exit code does not count.
- `risk` is `low`, `normal`, `important`, or `high`.
- `task_type` and `difficulty` are optional labels. Each launch records them in its `launch_choice` event, the shared records and the decision table, so that choices and outcomes can be compared later. `task_type` is `code`, `research`, `review`, `docs`, or `mechanical`; `difficulty` is `low`, `normal`, or `high`. Any other value makes `task create` fail. Set them whenever you know them.

Run `thoth task create -f SPEC --title "short title" --json` and read `task_id` from its output.

The task starts at the HEAD of the work tree you run the command in. Pass `--base REF` (MCP: `base`) to start it at any other commit or branch instead. The output's `base_branch` says where the finished `thoth/TASK/aN` branch merges back. With `--base`, it is REF only when REF is exactly a local branch name, and null for anything else (a sha, a tag, `origin/x`, `HEAD~1`). Without `--base`, it is the branch checked out in that work tree, or null on a detached HEAD. When it is null, the commander decides where to merge.

`task create`, `task update-spec` and `spec review start` first run `thoth spec lint` on the spec (MCP: `thoth_spec_lint`). A lint error refuses the command and writes nothing: a blank or repeated criterion, a check id this repository does not have, a step that writes under `.claude`, a spec over 40960 bytes (split the task), or, at `low` or `normal` risk (a spec without `risk` is `normal`), a spec over 20480 bytes. Lint warnings (line numbers, two-dot diffs, paths or names missing from the main work tree, PowerShell in a bash command, a spec over 20480 bytes at `important` or `high` risk) are added to `warnings`; fix the stale ones before dispatching. Run `thoth spec lint SPEC --json` yourself to see the report before creating the task.

## 2. Dispatch and wait

- Run `thoth recommend --role implementer --task TASK --json`. It chooses nothing; read the facts and choose yourself:
  - `candidates[]`: `eligible`, `reasons` (each quota window's used percentage), `quota_state`, `peak_percent`, `plan_label`, `shared` and `windows`.
  - `candidates[].models[]`: the models that account can run now. Each has `agent_arena` and `artificial_analysis` (the rows of those tables whose name matches the model: how strong it is, one row per effort variant) and `ai_stupid_level` (`degraded`: whether it got worse lately); a cell is null when no row matches. Each also has `usage[]`: Thoth's own average tokens (`tokens_avg`), average quota points (`quota_points_avg`, `quota_points_weekly_avg`), `runs_left` and `estimate_reliable` on that account. It has no pass or reject counts.
  - `unmatched`: the models whose name no table matched; look them up yourself with `thoth recommend --tables --json`, which adds the three full filtered `tables`.
  - `task_history`: `attempts`, `rejects`, `last_implementer` and `last_reviewer`.
- Choose like this:
  - Do not launch on a profile with any quota window at 95% or more.
  - Judge how strong a model is from the two external tables' rows, and whether it got worse lately from `degraded`. Use each model's own `usage` only to estimate cost and how many runs are left; never judge a model by its pass or reject record in Thoth.
  - A model Thoth has no runs of is no reason to avoid it: a model the tables rank strong gets ordinary work at once, without a small trial task first.
  - Effort: `mechanical` or `low` work gets `low` or `medium`; ordinary work `medium`; `difficulty` `high`, or risk `important` or `high`, at least `high`. Give `haiku` mechanical work only.
- Every `dispatch`, `review start` and `spec review start` takes `--reason "..."` (the MCP `reason` parameter): why this profile, model and effort. When the human has turned on `thoth config set require-reason on`, a launch without a reason is refused.
- The first launch of an attempt must give `--model` and `--effort`. Only a model without an effort setting (such as `haiku`), or an agy model whose name carries its effort level, may leave out `--effort`; other agy models still need it. A re-dispatch in the same attempt that leaves them out keeps the previous run's of the same role, but a model is never kept across harnesses (pass `--model` when you switch to a profile of another harness), and a kept value is still checked against the model it runs on. `spec review start` keeps nothing: give the model, and the effort with the same exception, every time.
- Dispatch with `thoth dispatch TASK --profile claude-b --model sonnet --effort medium --reason "..." --json`; the output includes `run_id`, `attempt`, and `worktree`. The profile name is only an example. Use `--model sonnet` for ordinary work and `haiku` for trivial work. A run defaults to `30m`; use `--timeout 60m` for larger tasks.
- A codex profile must always receive a codex model that `thoth models` lists. For example: `thoth dispatch TASK --profile codex-a --model MODEL --effort medium --reason "..." --json`. A Claude model or an omitted model is refused. `--allowed-tools` does not apply to codex. Everything after dispatch is the same for all supported profiles.
- **Antigravity worker** (profile `agy-a`, harness `antigravity`, Google): `thoth dispatch TASK --profile agy-a --model MODEL --effort LEVEL --reason "..." --json`, with MODEL a name `agy models` lists (leave out `--effort` only when that name carries its level).
  - **Model.** Always pass an agy `--model`, a name that `agy models` lists. Thoth checks it against the model list it read at this login, or the stored list when it cannot read one; a model on neither is refused before the run starts. Only when there is no list at all does Thoth warn that the model is unverified and let it through. Claude aliases and codex names are refused.
  - **Account.** One agy account per machine user, shared with the human's own agy sessions and their quota. The quota is counted per model group (Gemini models; Claude and GPT models). Thoth reads it only on `thoth profile usage --refresh`, and only after the human has run `thoth config set live-quota on`. The same refresh also asks codex's app-server for each codex profile's live quota (skipped while that profile has a live run).
  - **Tool lock.** `--allowed-tools` left at its default maps to the agy implementer list. It is enforced by a PreToolUse hook (`.agents/hooks.json`), and each blocked call becomes a `tool_denied` event plus `run_usage.tools_denied`. With `run_command` allowed the lock is a guard rail, not a sandbox.
  - **Evidence.** Command exit codes come from agy's own transcript. An unknown code is null plus a `command_exit_code_unknown` event, and `thoth verify` is the backstop.
  - **Reviewer.** `review start TASK --profile agy-a --model MODEL --effort LEVEL --reason "..."` (MODEL from `agy models`) gives a reviewer with no shell and no write tools. Like the codex reviewer, it reads `.agent/review-input/`, and its verdict is agy's `structured_output`.
- Run `thoth wait TASK --timeout 9m --json` and read `status`. Exit code 3 means it is still running; call it again. A tool call must not exceed ten minutes.
- Never dispatch while a run for the task is live.
- Worker tools are restricted by `--allowed-tools` (by default `Bash,Read,Write,Edit,Glob,Grep`, without MCP or subagents). Add a tool only when the task needs it. `review packet` and `status` expose `subagents_spawned`; a value above zero means the worker delegated.

## 3. Route on `status`

| status | what you do |
|---|---|
| `in_review` | Continue to step 4. |
| `unverified` | Run `thoth events --since 0 --task TASK --json` and inspect the `result_invalid` event's `detail.rule` and `detail.reason`. Re-dispatch with the same command; this keeps the attempt and quotes the reason to the worker. If it happens again, add a constraint naming what evidence to cite with `thoth task update-spec TASK -f SPEC --json`, then dispatch. `review record` is valid only from `in_review`. |
| `interrupted` | Run `thoth status TASK --json` and inspect `runs[].interrupted_reason`. For `quota`, read the run's `next_step` and `profiles_with_quota` and move on now to a profile that still has quota: an implementer run is dispatched again (`thoth dispatch TASK --profile OTHER --model MODEL --effort LEVEL --reason "..." --json`, the effort only where the model needs one); a reviewer or spec reviewer run is started again with `review start` or `spec review start`, choosing the reviewer as in step 4, never resumed. Only when no registered profile has quota on any model it can run, tell the human and wait until quota comes back; do not try a launch before that. Then a resumable interrupted implementer run continues with `thoth resume RUN --json` (if that is far off, end your turn after telling the human and resume in a later turn); an implementer run with no session to resume is dispatched again, and a reviewer or spec reviewer run is always started again. For `crash`, run `thoth resume RUN --json` once and report a repeat. Dispatching again from `interrupted`, to the same or another profile, makes Thoth write `.agent/handoff.md` for the new worker and add `handoff` to the dispatch output; the old run can no longer be resumed after that. |
| `failed` | Read `runs[].error_text` and `runner-error.log` under `runs[].run_dir`, then report before retrying. To abandon it, run `thoth task close TASK --reason "..." --json`; it is accepted only from `failed`, `interrupted`, `unverified` and `reject`, and only after all runs stop. It becomes `cancelled` and leaves `thoth status`, but its worktrees stay; later remove them as in step 5. Dispatching again from `failed`, to the same or another profile, makes Thoth write `.agent/handoff.md` for the new worker and add `handoff` to the dispatch output; the old run can no longer be resumed after that. |
| wait keeps exiting 3 | Run `thoth events --since N --task TASK --json`, using the previous `last_seq` (or 0 initially). New events mean progress, so keep waiting. Otherwise allow the run's own timeout; the run and task then become `failed` with `interrupted_reason` `timeout`, and `resume` refuses a failed run. After reporting a timed-out failure, dispatch again to reuse the attempt and worktree; that dispatch, to the same or another profile, makes Thoth write `.agent/handoff.md` and add `handoff` to the dispatch output, and the old run can no longer be resumed. `thoth cancel RUN` is final and requires a new task to retry; to abandon cleanly, use `task close` after the run stops and remove worktrees in step 5. |

## 4. Review

First run `thoth recommend --role reviewer --task TASK --json`. For a code review or a spec review, at any risk, you choose the reviewer in this order:

1. A profile of another provider than the implementer's that has quota and whose harness can do this review. Use that provider's middle model and a medium effort, not its most expensive model or its highest effort: you switch provider for a different training, not for the most expensive model.
2. When no other provider can be used, do not wait for its quota to reset: stand in with the implementer's provider now, using a model stronger than the implementer's (judged from the external tables' rows under each model; for example sonnet implemented, opus reviews) or the implementer's model at a higher effort. A stand-in may use an expensive model.

The same model at the same effort never reviews its own work. When one of these is available, start the review now: never wait for some profile's quota to reset. The `--reason` says which step of the order you reached and why.

A batch of related tasks may also be reviewed by the same provider while it is in progress, and then, once all of it is merged, by another provider in a single review of everything the batch merged. That review reports only problems that make something go wrong: each finding gives the concrete failing case (which input, which state, what wrong result) and its severity; style, naming and wording are not reported.

Build input in the commander's temporary directory with `thoth review packet TASK --out PACKET.md`. It contains the spec and AC ids, evidence excerpts including failed commands, all attempt diffs, current claims, and earlier reviews; it intentionally omits the worker summary. Do not assemble it manually.

Before building the packet, run `thoth verify RUN --json`. It re-runs this repository's **approved checks** in the run's worktree (the ids in the spec's `checks`; without that field, every approved check) and records them as new evidence codes the reviewer can cite. It never runs the commands the worker declared: those come back under `worker_declared`, for reference only.

A check that was not run has `error` set and `exit_code` null. Route on the error:

| error | what you do |
|---|---|
| `check X is not approved` | Decide yourself (below). If the check is right, run `thoth checks approve --reason "..."`, then run `verify` again. |
| `fingerprint mismatch for check X: ...` | The check's program, command or an input file is not what was approved. Read which lines differ. If the spec or the human asked for that change, run the `thoth checks approve --run RUN --reason "..."` command the error prints, then run `verify` again. If not, that change is a finding. |
| `no approved checks for this repository` | Propose checks (below), approve them with a reason, then run `verify` again. |
| `missing_since_last_verify` is not empty | An approved check was removed or replaced since the previous verify, so this verify ran fewer checks. Name each one to the human in your report; only a human can decide that was intended and re-add and approve it. It is not a failure of this run. |

You approve checks and protected test changes yourself, always with `--reason`; the approval, who made it and the reason are shown to the reviewer in the packet's Approvals section.

- Approve when the change is what the spec or the human asked for in the conversation: a new check the spec requires, a changed dependency, a version bump.
- The reason says what changed, why it is right, and which AC or which request it answers.
- Do not approve a deleted or loosened assertion the spec did not ask for: record REJECT with it as a finding. Never approve only so that a check passes.
- Ask the human only when you cannot tell.

While a decision is open, the task stays `in_review` and you record no verdict.

Before a review, in this order:

1. Run `verify` (above).
2. Build a first packet with `thoth review packet TASK --json` and read the `protected_changes` entries of the run under review.
3. No entries, or only `check_input` ones (verify's fingerprint check and its automatic acceptance handle those): nothing to approve.
4. `test` or `check_word` entries: read those diffs. One approved earlier needs nothing more (`checks approve --run` lists it under `protected_unchanged`, and `review start` does not wait for it). A right change: `thoth checks approve --run RUN --reason "..."`. A wrong one: REJECT.
5. After approving, build the final packet for the reviewer, so it has the Approvals section.

`review start` and a PASS `review record` refuse while a `test` or `check_word` change of the run is not approved, and the refusal prints the command to run. The approval belongs to that run: a rework run with the same change needs its own.

To propose a check: `thoth checks add ID --cmd "PROGRAM ARGS" [--shell argv|cmd|powershell|sh|bash] [--exe-pin path] [--input PATH]... --json`, then approve it with `thoth checks approve --reason "..."`. For a program outside the repository that updates itself (Claude Code's `claude`), `--exe-pin path` pins its path and not its bytes; it is approved the same way, with a reason, and a program inside the repository or a worktree, or written as a relative path, is refused. `thoth checks list` shows what exists. With a shell other than `argv`, the command is one program plus plain arguments; put shell syntax in a script file and add the script as the check. To have a narrower command re-run, add it as its own check and list its id in the spec's `checks`.

If the approved checks cannot pass on this machine because of a pre-existing or platform failure, do not run the check on the task's `base_commit` by hand. Use `thoth verify RUN --at-base --json`, which re-runs the approved checks in a temporary worktree at `base_commit` and records evidence labelled `verify@base:`. Give the reviewer those codes (next to any plain `verify` ones) and tell it to judge the criterion by its intent: no new failures compared with the baseline.

When the host supports subagents, use a read-only reviewer in a fresh context (an Explore-type subagent in Claude Code). Give it the packet path and permission to run `thoth evidence show CODE --task TASK --bytes 200000 --json`. Tell it that new or changed tests must check behaviour through the interface (a CLI command, its `--json` output, or a public function), and that a test reading the database or other internal state directly, only comparing a fixed string, only checking the exit code, or repeating the implementation's logic is a finding with category `test_interface`. After rework, tell it to check earlier findings. Do not provide the worker summary. When the host has no subagent facility, always use `thoth review start` with a registered profile chosen in the order above; never ask a profile to review work it dispatched. Pass a model for the reviewer's harness. Codex and Antigravity reviewers cannot run thoth: Thoth writes the packet and every evidence blob into `.agent/review-input/` for them. A codex `thoth review show` listing every cited code as `unopened_citation` is expected, not a warning sign; an Antigravity verdict comes from its `structured_output`.

The review must be exactly this shape:

```json
{
  "verdict": "REJECT",
  "findings": {
    "criteria": [
      {"id": "AC1", "verdict": "met", "evidence": ["E008"]},
      {"id": "AC2", "verdict": "unmet", "evidence": ["E008"], "note": "tests/triple.rs asserts triple(7) == 14; expected 21", "fix_size": "S", "category": "spec_mismatch"}
    ],
    "extra_findings": [
      {"severity": "minor", "note": "what else", "evidence": ["E008"], "fix_size": "S", "category": "docs"}
    ]
  }
}
```

Rules: `verdict` is `PASS`, `REJECT`, or `UNVERIFIED`; every AC id appears exactly once; `met` cites evidence; `unmet` cites evidence or has a note; codes belong to this task; and `PASS` requires every AC to be met. Keep only actionable defects in `extra_findings`: each entry is folded into the worker's rework prompt, so drop "no issue found" remarks and observations the worker cannot act on.

Label every unmet criterion and every extra finding with `fix_size` and `category`; a `met` criterion needs neither. `fix_size` is the smallest fix that would resolve it: `S` (at most 10 changed lines in one file), `M` (at most about 50 changed lines, or more than one file, no design change), `L` (larger, or it needs a design or spec change). `category` is one of `spec_mismatch`, `spec_error`, `test_missing`, `test_interface`, `error_path`, `concurrency`, `platform`, `docs`, `evidence`, `files`, `security`, `other`. Case does not matter. A finding without them is still accepted, but `thoth retro` counts it as `unsized` and `unclassified`; any other value is refused.

Write the review to a temporary file and run `thoth review record TASK -f REVIEW --json`; read `task_status`. Correct any refused shape and record again.

For high risk, review locally but do not record it. Run `thoth review start TASK --profile OTHER_PROFILE --model MODEL --effort LEVEL --reason "..." --json`, then `thoth wait --run RUN --timeout 9m --json`; Thoth's reviewer settles the task. Choose that reviewer in the order at the start of this step. If its verdict disagrees with yours, stop and ask the human.

**Reviewer fixes.** On a code task, `thoth review start TASK --edit --profile OTHER_PROFILE --model MODEL --effort LEVEL --reason "..." --json` lets the reviewer attach a small fix to a REJECT instead of sending the task back for a whole attempt. When `thoth wait --run EDIT_RUN --timeout 9m --json` shows `pending_fix` true, run `thoth verify EDIT_RUN --json` (the checks run in the fix worktree), then read the `protected_changes` entries of EDIT_RUN in `thoth review packet TASK --json`: a fix that adds to a test file or changes an inline test region lists them there, and `review confirm` is refused until you approve a right one with `thoth checks approve --run EDIT_RUN --reason "..."` (an approval of the implementer run does not cover the fix); for a wrong one, do not confirm, settle the task with a plain review instead. Then run `thoth review confirm TASK --profile P2 --model M2 --effort E2 --reason "..." --json` with a model other than the one that wrote the fix (the edit review's model at the edit review's effort is refused). A PASS names `merge_branch` and `merge_commit` in `thoth review show TASK`; read that change, then merge that branch instead of `thoth/TASK/aN`. A fix may change at most 40 lines in 3 files, an existing test file may only gain lines, spec files are never changed, and a fix must cover every unmet criterion and every extra finding that is not minor (otherwise it is refused and the task is redone); the refusal reasons are under `docs/` in the Thoth repository ("Reviewer fixes, author confirms" in `docs/phase2/usage.md`, `review confirm` in `docs/adapter-usage.md`).

## 5. After the verdict

- On `reject`, dispatch again to start attempt N+1 from the candidate; the first launch of the new attempt gives `--model`, `--effort` and `--reason`. At low or normal risk, findings are included automatically. At important or high risk, read `thoth review show TASK`, incorporate agreed findings into a rewritten spec with `thoth task update-spec TASK -f SPEC --json`, then dispatch. A further rejection requires another update. Return to step 2.
- Thoth never steps up the model or effort by itself; you decide, from `thoth recommend --role implementer --task TASK --json`'s `task_history` (`rejects` says how many times the task was rejected, `last_implementer` what the rejected run used). When the worker made the mistake, step up one effort level the model supports; at its highest level, switch to a stronger model. For example, if the attempt ran on `haiku` and the mistake looks like a weak-model error, dispatch the rework with `--model sonnet --effort medium`. When the spec was wrong, fix it with `update-spec` and dispatch again at the same level, saying so in `--reason`. `UNVERIFIED` is not a rejection: do not step up for it.
- On `pass`: merge `thoth/TASK/aN`, run tests on the merged tree, list the task's worktrees from `runs[].worktree` in `thoth status TASK --json` before removing any, inspect each with `git status --short`, remove them, and delete the merged task branches. Use forced worktree removal only when the sole untracked content is `.agent/`.
- After each recorded review and merge, run `thoth report`. It writes `report.html` in the state directory; tell the human its path on the first run; `thoth report --open` opens it in their browser.
- Once a week, run `thoth retro --json`. For every category with `suggest_check` true (the same kind of finding seen twice or more, even in one task's reworks), propose a lint rule or a check to the human.

## Using the MCP tools

When tools whose names begin with `thoth_` are available, they may replace matching CLI commands. For example, `thoth task create` maps to `thoth_task_create`, and `thoth review record` maps to `thoth_review_record`. `thoth resume` maps to `thoth_resume`. They return the same document as the CLI's `--json` output. Pass spec and review JSON directly as objects rather than writing files. Pass `repo` as the absolute path of the work tree you are running in: inside a linked worktree that is the worktree's own path, not the main repository's. A new task starts at the HEAD of that work tree. Without `repo`, the server's default repository is used.

Each `thoth_wait` call has a time limit. The CLI's exit code 3 is represented by `timed_out: true`; call `thoth_wait` again. `thoth_wait` takes `timeout_seconds` (a number), not the CLI's `9m`. `thoth_review_packet` returns a file path. `verify` takes two steps: `thoth_verify_start` starts the approved checks in the background and returns a `job_id` at once, then `thoth_job_status` (with `wait_seconds`) reports the job until its `status` is no longer `working`: `completed` (the verify document is in `result`), `failed` or `unknown`. The check catalogue has `thoth_checks_list`, `thoth_checks_add`, `thoth_checks_approve` and `thoth_checks_remove`; a check added this way is pending. Approve with `thoth_checks_approve`: `reason` is required, and `run` (optional) reads that settled run's worktree and also approves the protected test files that run changed. Commands with no MCP tool are still run through the CLI, for example `checks protect`.

## Common mistakes

| mistake | instead |
|---|---|
| Markdown spec, or reading `id` from task creation | JSON spec; read `task_id` |
| `approve`, `not_met`, or criteria at top level | use the exact review shape above |
| `wait --timeout 30m` | use `9m`, then call again after exit 3 |
| `review record` on `unverified` | re-dispatch; record only from `in_review` |
| `update-spec` before every low-risk rework | findings are folded automatically at low or normal risk |
| expecting Thoth to choose the profile, model or effort, or leaving out a `--model` or `--effort` the first launch needs | choose them yourself from `thoth recommend` and give them on every first launch |
| a launch without `--reason` | every `dispatch`, `review start` and `spec review start` says why this profile, model and effort |
| waiting for a used-up profile's reset when another profile has quota | dispatch again now on a profile with quota; for a review, take the next reviewer in the order of step 4; only when no profile has quota anywhere, tell the human and wait (step 3) |
| `cancel` to restart a slow run | cancellation is final; let it time out, then dispatch again |
| guessing the branch name | use `thoth/TASK/aN` |
| fixing even a one-line defect yourself | record REJECT and send it back |
| approving with a reason like "ok" or "needed" | say what changed, why it is right, and which AC or request it answers |
| approving a loosened test so that a check passes | record REJECT with the loosened assertion as a finding |
| expecting `verify` to run a command the worker declared | `verify` runs approved checks only; add the command as a check and approve it with a reason |
