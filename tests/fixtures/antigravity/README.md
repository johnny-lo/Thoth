The `e2-*` to `e8-*` files are real agy output recorded with agy 1.2.2 and 1.2.12
on 2026-09-29 (docs/post-phase2/antigravity-worker-scope-2026-09-29.md §1, the
experiments of the same names).
The three `fc1-*` files are each the `result` line of a real failed run from the
Linux acceptance on 2026-09-29 (agy 1.2.12), taken from the run's stream.jsonl in
the thoth state directory, with the personal paths rewritten to /tmp/wt and the
account part of the Google profile picture URL replaced by REDACTED; every other
byte is unchanged. agy exited 3 (`fc1-network-issue`), 1
(`fc1-eligibility-tls-timeout`, whose stream had no init line) and 0
(`fc1-stream-interrupted`). The runner classifies all three as transient
interruptions (FC-1).
