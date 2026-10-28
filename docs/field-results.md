# Field results: 59 real tasks

Before Thoth was released, it was used for 59 real development tasks, from
2026-09-24 to 2026-10-06. This page gives the tally, what the evidence and reviews
caught, what it cost, and what the numbers do **not** show.

## How the trial was run

- **Commander:** the author's own Claude Code session. It wrote each spec (some
  through a planning subagent), dispatched it through Thoth, judged the review from
  evidence and merged the result.
- **Workers:** Claude Code, mostly on a second subscription account and on `sonnet`,
  with `opus` for work on processes, concurrency or large specs. In later tasks,
  Codex as well. Antigravity (`agy`) support was built during the trial
  (tasks 25–29); agy workers were tried twice later (tasks 53 and 54), and both runs
  stopped early, so that work was finished without agy.
- **Reviewers:** independent reviewers that judged from the evidence: Thoth
  reviewer runs on Claude Code or Codex, often from a different vendor than the
  worker, or the commander's own subagents.
- **Repositories:** Thoth itself (55 tasks) and one other project of the author's (4 tasks).
- **Records:** one record per task, filled in by the commander from evidence codes
  and commits. Only the human time was supplied by the person. A script with fixed
  text rules turns the records into the table below, so no language model is
  involved in the tally. The per-task records (in Traditional Chinese) and that
  script stay in the author's private repository and are not published.

## The numbers

| | |
|---|---|
| Tasks | 59 |
| Final status | 58 pass, 1 failed |
| Tasks where the reviewer caught at least one real defect | 45 of 59 |
| Real defects caught by reviewers | 46 important, 128 minor, 4 unlabelled |
| Times the recorded evidence contradicted the agent | 6, in 6 tasks |
| Tasks rejected and redone at least once | 31 |
| Attempts / runs in total | 110 / 138 |
| `unverified` outcomes | 6, of which real problems: 3 |
| Runs on a second account or another vendor | 110 |
| Human time, all tasks together | 13 minutes |
| "Did Thoth help on this task?" | yes 51, somewhat 8 |

What the rows mean:

- **Attempt:** one try at the task. A `reject` starts a new attempt from the rejected commit.
- **Run:** one worker or reviewer process. An attempt can have several.
- **Real defect:** a problem a reviewer found. A defect is counted once, on the
  task where it was first caught. The first 11 tasks did not grade severity yet,
  which is where the 4 unlabelled ones come from.
- **Evidence contradicted the agent:** the recorded commands showed something
  different from what the worker's claims or report said.
- **Second account or another vendor:** runs that did not use the commander's own account.

## What the checks caught

- **A skipped red step.** The spec required a failing test first. The evidence
  showed the failing run never happened. Nothing in the worker's report said so.
- **User settings silently overwritten.** The first version of a change overwrote
  the user's own git exclude rules. Two dispatches running at once, or a process
  killed halfway, would have deleted them. An `opus` reviewer rejected it.
- **Claim and code disagreeing.** The claim and the docs said the baseline was the
  start of the task. The code used the start of the run. On a rework the two
  differ, and the first round of tests happened not to cover that case.
- **Bugs that only a real Codex triggers.** While Codex support was being added,
  the argument order of the resume command and the moment the Codex process name
  was read were both wrong. Only a real Codex run would show either, and both were
  caught before merge.
- **Bugs in Thoth itself.** The same task also found that an `important`-risk task,
  once rejected, could never be dispatched again. It was fixed along the way.

## A weak spot the trial exposed

Workers kept piping their test runs into `| tail` or `| grep` (for example in
tasks 1, 16, 19 and 20). The recorded exit code then belongs to `tail` or `grep`,
not to the test runner. The evidence still holds the output, so a reviewer has to
read it, or have Thoth re-run the checks itself with `thoth verify` (used in tasks
11, 19 and 20, among others). Thoth's worker prompt has asked workers to run test
commands on their own since task 6, and they still did it afterwards, so read the
output.

## The one failure

Task 22 (the HTML status page) was sent to a Codex worker twice. The first attempt
did not compile: Codex's offline sandbox could not fetch a new dependency.
`thoth verify` showed the failure at once, before anyone read the code. The second
attempt ran out of time after the Codex service dropped the connection, and the
stream log said exactly that. The commander then built the feature directly,
outside Thoth, and the task stayed `failed` in Thoth. Thoth did not get the work
done here. It did make it clear, quickly and without reading code, why the
dispatched attempts had not.

## What it cost

- **Human time:** 13 minutes across all 59 tasks (task 55 also records one design
  question the person answered, without a time). There was no comparison group,
  and delegating to subagents directly also takes almost no human time, so this is
  "no worse", not "better".
- **Wall-clock time:** in the first 20 tasks, each took roughly 5 to 15 extra
  minutes of waiting and review. High-risk tasks with long rework loops took far
  longer: task 55 went through 11 attempts.
- **Transient failures:** a login token refresh that failed on the first run after
  it expired (re-dispatching fixed it), and Codex service stalls.
- **Top-tier model usage:** implementation moved off the commander's account, so
  the most expensive model was spent on specs, judgement and review. The
  commander's own usage was not logged, so this is not quantified.

## What the numbers do not show

- **There was no control group.** No task was also done without Thoth. "45 of 59
  tasks had a defect caught" measures what Thoth caught, not the difference from
  working without it.
- **Most tasks were on Thoth itself** (55 of 59). Building a tool with itself
  brings in extra problems, and some of the defects caught were Thoth's own.
- **The commander filled in the records.** It is the same AI that ran the tasks.
  Only human time came from the person. Every entry points back to evidence codes
  and commits, but the records are not independent.
- **Defects are not weighted.** "Minor" ranges from a wrong comment to a
  mislabelled line number.
- **The quota view was barely exercised.** Worker accounts did run out mid-task
  (task 55 used up the Claude worker account's five-hour window twice). Once
  (task 46) a rework moved to the other vendor's account and finished in 6 minutes
  instead of waiting 74 minutes for the reset; once (task 51) a routing choice made
  a task wait almost three hours. That is too few cases to judge the quota report
  or the account recommendation.
- **One small model sample.** `haiku` was used on only two tasks (1 and 13). On
  task 1 it made the kind of mistakes a weaker model makes, and the task passed
  after moving to `sonnet`.

## In one sentence

Across these 59 tasks, reviewing from evidence kept catching problems the worker's
own report did not show, at the cost of waiting for reviews and reworks and
almost no human time.
