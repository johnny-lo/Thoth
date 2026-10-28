# Thoth Design Summary

<!-- source: v6.1 §0.1, §16, §18; proposal positioning -->
## Purpose and positioning

Thoth is the account-routing and evidence-preservation layer for native AI coding
agents. Any commander that can invoke commands can call it, while user-facing
front ends are left to other tools.

It is a local control plane for using multiple providers, accounts, and capability
levels without reducing their native agent harnesses to plain model APIs. Its
responsibilities form a narrow sequence:

```text
identity → routing → execution → handoff → evidence → verification
```

The command-line interface is the product boundary. A commander actively creates
tasks, dispatches work, waits for completion, and reads evidence through `thoth`.
Machine-readable output is part of that boundary and must be treated as a public
contract.

Thoth keeps human-readable task, evidence, and review state, but it does not own
the interactive experience. A separate front end may consume its command output.
Likewise, Thoth does not replace a provider's native coding-agent loop. It starts
already authenticated official tools and preserves the isolation of each profile's
configuration, credentials, process, session, and quota.

This positioning addresses two problems. Routing puts work on an appropriate
account and capability level. Evidence preservation makes important results
independently checkable instead of treating an agent's report as fact.

Thoth is therefore not a general multi-model controller, a subscription-usage
optimizer, or another autonomous workflow builder. It is infrastructure for
trustworthy orchestration of heterogeneous AI workers.

<!-- source: v6.1 §18 -->
## The five principles

The design is governed by five sentences:

1. Use the right intelligence for the right task.
2. Keep identities isolated.
3. Pass state instead of conversations.
4. Never trust an agent claim when the underlying evidence can be checked.
5. Never let the claimant build the evidence pipeline.

The final principle is primarily an authority boundary: workers do not write the
control-plane database. Evidence records and the evidence index are generated
mechanically outside the claimant's control.

<!-- source: v6.1 §0.1, §2.2, §4, §7, §9.1 -->
## Roles

### Commander

The commander is the agent or human-facing process that turns an ambiguous request
into a task. It defines the goal, acceptance criteria, constraints, exclusions,
and risk; decides whether to dispatch work; and chooses the required verification
strength. It calls `thoth`, but its planning and interaction model remain its own.

### Worker

The worker implements, fixes, tests, or debugs in an isolated worktree. It receives
task state rather than the commander's conversation. Its shell activity is
captured out of band by the native harness or its structured output stream. The
worker may state claims and cite evidence, but it cannot turn its own prose into
evidence or write the database directly.

### Reviewer

The reviewer compares requirements, implementation, and evidence in fresh context.
It reads the complete diff, including test changes, maps findings to acceptance
criteria, and checks whether sources are correct and current. It returns `PASS`,
`REJECT`, or `UNVERIFIED` with structured findings. A reviewer may use a different
provider profile to reduce correlated failure; that is an option of this role,
not a separate role.

### Runner

The runner is the deterministic execution boundary under the agents. It launches
the selected authenticated profile, manages the isolated worktree and process,
captures command output and artifacts, records status transitions, and can run
verification itself. It preserves command, working directory, exit code, output,
duration, environment fingerprint, timestamp, and content hashes where available.

<!-- source: v6.1 §15; proposal §6 -->
## The two lines

Development follows two demand-driven lines rather than a numbered sequence.

### Evidence line

The evidence line is the current quality-control line. It formalizes evidence
provenance, marks cases where shell pipelines can hide an exit status, records
which evidence a reviewer actually opened, and retains smaller improvements found
through use. Its purpose is not to make every task expensive; verification strength
continues to depend on task risk.

### Routing line

The routing line covers accounts and the worker pool. It adds provider workers
when demand warrants them, produces a self-contained handoff for the next worker,
records available quota information, and recommends an account. It does not make
automatic dispatch decisions merely because quota data exists.

The trigger for prioritizing this line is practical pressure: a constrained account
or a new commander's needs. Routing and evidence remain complementary. One allocates
scarce execution capacity; the other makes the returned result checkable.

<!-- source: v6.1 §1.4, §2.1–§2.5, §10.5 -->
## Evidence

A claim is an assertion made by a worker, such as “the tests passed.” Evidence is
the independently preserved material against which that assertion can be checked:
the command, exit code, raw output, report, commit, diff, environment, timestamp,
or source snapshot. The intended chain is:

```text
claim → evidence → verification → decision
```

Claims and evidence are separate records with an explicit many-to-many link. A
worker's annotation of an artifact remains a claim. It does not gain evidentiary
weight merely because a stronger model repeats it.

The evidence hierarchy prefers executable or raw artifacts, then official sources,
repository sources, external sources, and finally agent interpretation. When a
source document is evidence, its URL, document version, retrieval time, and locator
are required, and the complete retrieved bytes are preserved so that reviewers
inspect the same snapshot.

### Two evidence levels

`harness_reported` evidence comes from a command chosen and executed by the worker,
then observed by a harness hook or structured output stream. It shows what ran and
what was observed, but the worker selected the command, and a shell pipeline may
mask the meaningful exit status.

`control_executed` evidence comes from a verification command executed by Thoth.
Thoth obtains both the exit status and output directly. High-risk key claims may
be required to cite at least one item at this level; verification reruns are not
the default for ordinary work.

The `collected_by` field records the provenance boundary: whether evidence was
recorded by the control plane rather than authored by the worker. It does not, by
itself, say who chose or executed the underlying command. The two levels refine
that distinction so reviewers can judge how much independence a record provides.

Evidence is presented on demand. A reviewer first receives the task, acceptance
criteria, diff, claims, and evidence index, then opens raw material only as needed.
Each opening is recorded, supporting both trust review and context efficiency.

Evidence is kept exactly as it was captured, so its hashes and diffs stay
true. What looks like a secret is masked only at the moment content leaves the
machine: for a reviewer, a commander reading command output, the MCP client,
the report page or the shared records folder.

<!-- source: v6.1 §2.5, §6.1–§6.3, §10.5, §11 -->
## State model

A task represents the durable unit of requested work. Its principal states are:

```text
draft → queued → running → in_review ─┬→ pass
                    │                 ├→ reject → rework
                    │                 └→ unverified
                    ├→ failed
                    └→ cancelled
```

An interrupted execution can be resumed, requeued, or rerouted. After rejection,
the next attempt normally starts with fresh context plus the original specification,
a findings summary, and evidence references. Low- and normal-risk findings can be
folded into a new dispatch mechanically; important and high-risk findings return
to the commander for specification revision.

A run is one execution attempt for a task. Its states are `queued`, `running`,
`completed`, `failed`, `cancelled`, and `interrupted`. An interrupted run records
a reason such as quota, crash, or user action. Its worktree and harness session can
serve as a checkpoint for resume, while rerouting can place later work on another
profile. A rejected task increments the attempt for its next run.

Thoth assigns `UNVERIFIED` without model judgment when any of these six conditions
holds:

1. The result document fails schema validation.
2. A claim has no evidence reference.
3. Referenced evidence is missing.
4. Referenced evidence fails its hash check.
5. A high-risk claim lacks control-plane-level evidence.
6. The candidate diff modified or deleted an existing test file or a check's
   input file (or added a file named like a word of a check's command) and the
   worker's result does not declare that file.

The sixth condition only makes the change visible: Thoth computes the list from
the diff and shows it to the reviewer, who still decides whether a check was
weakened. Tests written inline in a source file are outside its reach.

`UNVERIFIED` is distinct from `REJECT`: it says the support for a conclusion is
insufficient, not that the implementation has been shown to violate the task.

<!-- source: v6.1 §4, §15, §16; proposal §6 -->
## What is deliberately not built

Thoth does not rebuild the planning, implementation, and review workflow. Those
behaviors belong to the commander and native agent harnesses. The removed plan for
a built-in hierarchical workflow was unnecessary after real use showed that the
control plane's value was routing and evidence, not owning agent reasoning.

It does not provide a persistent MCP connection or daemon. That capability is
deferred unless a commander appears that can call Thoth only through MCP. A
command-line contract is sufficient for current commanders and avoids adding a
resident service without a demonstrated need.

It does not build a visual agent office. It also does not build an interactive GUI,
dashboard, or mobile front end. Human-readable output and a read-only static report
are rendered from database facts; external interfaces can consume `thoth` output.

The Retriever role was removed because workers and reviewers already search and
read files through their native harnesses. Its durable requirement survives in the
`source_document` evidence schema and preserved snapshot.

The Presenter role was removed because status and review packets can be rendered
deterministically from the database. Verdicts, evidence identifiers, URLs, hashes,
and numbers must come from templates over stored facts, so another model pass would
add cost and another opportunity for distortion without adding authority.

Thoth also deliberately avoids becoming a hosted service, credential store, API
proxy, general workflow builder, IDE, automatic merge or deployment system, or a
complex scheduler. Workers never write the database, and ordinary verification is
not rerun by default.

<!-- source: v6.1 revision summary, §0.1, §2.4, §4, §15, §16 -->
## Revision note

Version 6.1 was revised on 2026-09-28 after 20 trial tasks. The revision narrowed
the product to account routing plus evidence preservation, confirmed that any
commander can call it while front ends remain external, and replaced the numbered
roadmap with the evidence and routing lines.

It formalized the distinction between worker-selected, harness-reported evidence
and verification executed by Thoth. It also documented the implemented native
capture paths, kept risk-based reruns exceptional, and clarified that command output
is the public integration boundary.

The revision removed the built-in hierarchical workflow, persistent MCP service,
and visual office from the plan; retained only a database-derived static report;
removed Retriever and Presenter as roles while preserving their schema and
deterministic-rendering requirements; and folded cross-provider audit into the
Reviewer role.
