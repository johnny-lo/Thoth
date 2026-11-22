---
name: thoth-dispatch
description: Use when implementation should be delegated to another subscription account through Thoth, or when a Thoth task is running, awaiting review, interrupted, or unverified.
---

# Dispatching implementation through Thoth

## Overview

You are the commander. You plan and review; a registered worker profile implements through `thoth`, which records evidence of what the worker actually ran. Judge from that evidence, never from the worker's prose, and record your judgment in Thoth. Never implement the delegated task yourself.

Thoth does not choose a profile, model or effort for you. You choose them and say why; Thoth records each launch's choice, your reason and its outcome. `thoth recommend` gives facts only.

Do not guess file formats or output keys. The full contract is `docs/adapter-usage.md` in the Thoth repository; the headings named below are there.

## Setup

- `thoth --version` must work (`thoth` on `PATH`); if not, stop and ask the human to install it.
- The state directory is `THOTH_HOME`, or `~/.thoth` when unset.
- At least one worker profile must be registered and logged in (`thoth profile list`); the human registers and logs in, not you. Names such as `claude-b`, `codex-a` and `agy-a` are examples only.
- Put scratch files (specs, reviews) in your own temporary directory, never in the repository or a worktree. UTF-8 without a BOM.
- First run `thoth status --json` in the repository; if it is not initialized, run `thoth init --json` once and retry. Continue a matching task from its current status instead of creating a duplicate.

## The one command (light path: low or normal risk code tasks)

1. Write the spec (below) and run `thoth pipeline new -f SPEC --title "short title" --reason "..." --json` (why the stored lists fit this task). It creates the task, starts its pipeline and returns at once with `task_id` and `pipeline_id`. It refuses before creating anything on a spec lint error, a docs, research or review task, no list given or stored, a bad entry, or important or high risk; fix it and run it again.
2. Run `thoth pipeline show TASK --wait 9m --json` until `status` is `stopped` (exit 3 means still running; call it again).
3. Act on `stop.reason`, `stop.summary` and `stop.next` (see When the pipeline stops). On `pass`, merge the exact `merge_branch` and `merge_commit`.

For an existing task (`thoth task create -f SPEC --title "..." --json`, or one to continue): `thoth pipeline start TASK --reason "..." --json`, then steps 2 and 3.

Store your default lists once per machine (yours, not a `thoth config` setting):

```
thoth pipeline defaults set --implementer claude-b:sonnet:medium --implementer claude-a:sonnet:high --reviewer codex-a:MODEL:medium --reviewer claude-a:opus:high --reason "why these are the usual choices" --json
```

- `set` replaces only the list whose flag you give, whole, in the given order; `thoth pipeline defaults clear [--only implementer|reviewer]` removes them. Thoth uses a stored list exactly as stored: it never ranks, reorders or shortens it. Put the reviewer order (below) into the reviewer list.
- Check with `thoth pipeline defaults show --json` before relying on them and after profiles or models change: an entry that no longer passes is under `invalid` (`entry_index`, `entry`, `error`) and refuses the start; store a new list or give the flag.
- A flag (`--implementer` or `--reviewer`) overrides that one list for one task. `settings` says where each list came from: `implementers_source` and `reviewers_source` (`flag` or `default`).

## Specify the task

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

- JSON only; `goal` and `acceptance_criteria` are required. The `AC1:` prefix is the criterion id the review uses.
- Every criterion is checkable from a diff or command output. Put no code in the spec.
- A criterion that names a test says which interface the test checks through: a CLI command, its `--json` output, or a public function.
- `risk` is `low`, `normal` (the default), `important` or `high`. `task_type` is `code`, `research`, `review`, `docs` or `mechanical`; `difficulty` is `low`, `normal` or `high`. Set them whenever you know them.
- Lint refuses a spec over 40960 bytes at any risk, or over 20480 bytes at low or normal risk. Run `thoth spec lint SPEC --json` first.

## Choosing profile, model, effort and reviewer

Read `thoth recommend --role implementer|reviewer --task TASK --json` as facts: quota, the external tables, Thoth's usage, `unmatched` models and `task_history` (`candidates[]`, `models[]`, `usage[]`: heading `thoth recommend --role implementer|reviewer [--task TASK] [--tables] [--json]` in `docs/adapter-usage.md`). Judge a model's strength from the external tables, never from its pass or reject record in Thoth.

- Do not launch on a profile with any quota window at 95% or more.
- Effort: mechanical or low work gets `low` or `medium`; ordinary work `medium`; `difficulty` `high`, or risk `important` or `high`, at least `high`. Use `--model sonnet` for ordinary work; give `haiku` mechanical work only.
- Every launch, `pipeline new` and `pipeline start` takes `--reason "..."`: why this profile, model and effort. Thoth records it.
- Escalation: list a stronger model or effort in the implementer list where you want one, and repeat an entry to allow it again.

For a code review, a spec review or a source check, at any risk, choose the reviewer in this order:

1. A profile of another provider than the implementer's that has quota and whose tooling can do this job, at that provider's middle model and a medium effort: you switch provider for a different training, not for the most expensive model.
2. When no other provider can be used, do not wait for its quota to reset: stand in with the implementer's provider now, in a new clean run, using a model stronger than the implementer's (judged from the external tables' rows under each model; for example sonnet implemented, opus reviews) or the implementer's model at a higher effort. A stand-in may use an expensive model.

The same model at the same effort never reviews its own work. When one of these is available, start the review now: never wait for some profile's quota to reset. The `--reason` names the step of the order you reached.

When a profile has no quota, start the next entry or profile now; only when no profile has quota anywhere, tell the human.

## Heavy path: important or high risk

`pipeline new` refuses these. Run `thoth task create`, then the spec review gate: `thoth spec review start TASK --edit ...` and `thoth spec review confirm` when it proposes edits, before the first `pipeline start`. An APPROVE spec review is required. Then:

```
thoth pipeline start TASK --implementer P:MODEL:LEVEL --implementer P2:MODEL:LEVEL --reviewer OTHER:MODEL:LEVEL --reviewer STANDIN:MODEL:LEVEL --verify-timeout 120m --reason "..." --json
```

- Give both lists as flags: no stored default fills a list at these risks. Thoth walks both lists forward and picks nothing: a REJECT is reworked on the next implementer entry, a quota or auth stop moves to the next entry, a reviewer entry with the implementer run's model and effort is skipped, and with no entry left it stops (`implementers_exhausted` / `reviewers_exhausted`) instead of waiting for a reset.
- `--max-attempts N` is the attempt ceiling you authorise; never add `--allow-attempt` without that decision.
- A REJECT stops `spec_rewrite_needed` until you rewrite the spec (`thoth task update-spec`), unless you started with `--fold-on-reject`.
- `--verify-timeout` needs room for the whole suite; `--quota-threshold N` skips an entry whose window is N% used or more.
- `thoth pipeline stop TASK` stops before the next launch and cancels no worker. A `stale` pipeline (its process died): run the same `pipeline start` again.

## When the pipeline stops

- `needs_approval`: read `thoth review packet TASK`, approve with `thoth checks approve --run RUN --reason TEXT` yourself, the reason saying what changed, why it is right and which AC answers it. Never approve a deleted or loosened assertion the spec did not ask for: record REJECT with it as a finding. Then run the same `pipeline start` again.
- `needs_fix_confirmation`: the reviewer left a fix. Verify it yourself (`thoth verify EDIT_RUN --json`, approving its protected changes as above) and confirm it with `thoth review confirm` and a model other than the one that wrote the fix, or drop a wrong fix with `thoth review start TASK --no-edit ...`. The pipeline never confirms over a pending fix.
- `spec_rewrite_needed`: rewrite the spec with `thoth task update-spec`, then `pipeline start` again.
- `attempts_exhausted`: decide whether to authorise more attempts, rewrite the spec, or close the task.
- `implementers_exhausted` / `reviewers_exhausted`: start again with entries that have quota now.
- Anything else: read `stop.next`; open `thoth review show`, `thoth evidence list` or `thoth events` only for detail. Never fix even a one-line defect yourself.

## By hand: pointers and the statuses the pipeline does not settle

A docs, research or review task, or a step you want to see, runs by hand. Each pointer is a heading of `docs/adapter-usage.md`:

- Dispatch and `--allowed-tools`: `thoth dispatch TASK --profile ID [--model MODEL] [--effort LEVEL] [--reason TEXT] --json`. **Antigravity worker** (tool lock, evidence, account): the same heading, always with a `--model` that `agy models` lists; the lock is "Worker tool lock, and the usage events" in `docs/phase2/usage.md`.
- Waiting, and the events check when wait keeps exiting 3: `thoth wait TASK --timeout 9m --json`, `thoth events --since N --task TASK --json`. Status keys and `runs[]`: `thoth status TASK --json`.
- Verify, check and protected-change approval, review start, reviewer fixes: `thoth review start TASK --profile ID [--model MODEL] [--effort LEVEL] [--reason TEXT] [--edit | --no-edit] --json` / `thoth verify RUN --json`; `thoth review confirm TASK --profile ID --model MODEL [--effort LEVEL] [--reason TEXT] [--timeout DURATION] --json`; `thoth checks add | approve | list | remove` (additive). The packet: `thoth review packet TASK [--out FILE] [--json]`.
- Spec review and lint: `thoth spec review start TASK --profile ID [--model MODEL] [--effort LEVEL] [--reason TEXT] [--timeout DURATION] [--edit] --json`, `thoth spec review confirm TASK --profile ID --model MODEL [--effort LEVEL] [--reason TEXT] [--timeout DURATION] --json`, `thoth spec lint FILE [--json]`.
- Tasks: `thoth task create -f SPEC.json --title TITLE --json`, `thoth task update-spec TASK -f SPEC.json --json`, `thoth task close TASK --reason TEXT --json`, `thoth resume RUN --json`, `thoth cancel RUN --json`.
- The pipeline's keys, continuation, ownership and verify slots: `thoth pipeline start TASK ...`, "Continuation (AUTO-1b)", `thoth pipeline show TASK [--wait D] [--json]`, "Pipeline ownership (additive refusal, AUTO-1a)", "Verify slots (AUTO-1a)".

`thoth verify RUN --json` re-runs the approved checks and records new evidence codes the reviewer can cite. On a pre-existing or platform failure, do not run the check on the task's `base_commit` by hand: `thoth verify RUN --at-base --json` runs them in a temporary worktree at `base_commit`, labelled `verify@base:`; tell the reviewer to judge by no new failures.

Codex and Antigravity reviewers cannot run thoth: Thoth writes the packet and evidence into `.agent/review-input/`. A codex review listing every cited code as `unopened_citation` is expected.

`interrupted` for quota: move to a profile with quota now; a resumable run continues with `thoth resume RUN --json` (if quota is far off, tell the human and resume in a later turn). `failed`: read `runs[].error_text`; after a timeout `resume` refuses a failed run, so dispatch again. To abandon, `thoth task close TASK --reason "..." --json` is accepted from `failed`, `interrupted`, `unverified` and `reject`; the task leaves `thoth status` but keeps its worktrees.

## Recording a review by hand

Shape rules, `fix_size`, categories and the two classes (blocking and advisory) are under the `docs/adapter-usage.md` headings `thoth review record TASK -f REVIEW.json --json` and `thoth review show TASK [--full] --json`.

```json
{"verdict": "REJECT", "findings": {"criteria": [
  {"id": "AC1", "verdict": "met", "evidence": ["E008"]},
  {"id": "AC2", "verdict": "unmet", "evidence": ["E008"], "note": "asserts 14; expected 21", "fix_size": "S", "category": "spec_mismatch"}
], "extra_findings": []}}
```

Every AC id once; `met` cites evidence; PASS needs every AC met and no blocking finding. Each extra finding is folded into the worker's rework prompt, so keep only actionable ones. Write it to a temporary file and run `thoth review record TASK -f REVIEW --json`.

## After the verdict

- On `pass`, merge and test the merged tree, then from the main working tree run `thoth worktrees prune --json` (its heading in `docs/adapter-usage.md`); report each `kept` path (a `runs[].worktree`) and kept branch, never force-remove it.
- After each merge run `thoth report` (`thoth report --open` opens it). Weekly, run `thoth retro --json` and propose a check for each `suggest_check`.

## Using the MCP tools

`thoth_pipeline_new` (`spec` as an object or `file` as an absolute path, `reason` required; returns at once), `thoth_pipeline_start` (lists as arrays; a list left out uses the stored default at low or normal risk), `thoth_pipeline_show` (`wait_seconds`; a timed-out wait is not an error) and `thoth_pipeline_stop` match the CLI, as do `thoth_task_create`, `thoth_review_record` and `thoth_resume`; they return the CLI's `--json` document. Pass `repo` as the absolute path of the work tree you run in. `thoth_wait` takes `timeout_seconds` (a number); `timed_out: true` means call again.

## Common mistakes

| mistake | instead |
|---|---|
| Markdown spec, or reading `id` | JSON spec; read `task_id` |
| `--wait 30m` or `--timeout 30m` | `9m`, then call again after exit 3 |
| a launch or pipeline without `--reason` | say why this profile, model and effort |
| waiting for a used-up profile's reset | start the next entry or profile now |
| `cancel` to restart a slow run | cancellation is final; let it time out |
| approving with "ok" or "needed" | say what changed, why it is right, which AC |
| fixing a one-line defect yourself | record REJECT and send it back |
