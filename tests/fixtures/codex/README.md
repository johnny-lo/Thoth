Real `codex exec --json` output recorded with codex-cli 0.154 on 2026-09-27
(docs/post-phase2/codex-worker-scope-2026-09-27.md §1), paths rewritten to
/tmp/repo and /tmp/wt. `exec-truncated-output.jsonl` is synthetic, built on the
real middle-truncation marker (`... N bytes omitted ...`, E5) because the real
recording is 1 MiB. `review-output-schema.json` is the schema thoth hands over
with `--output-schema` / `--json-schema`; E3b measured the version without
`fix_size` and `category`.
`app-server-rate-limits.jsonl` is one real `codex app-server --listen stdio://` exchange
(initialize, then the rate-limits request) recorded with codex-cli 0.154 on 2026-10-01,
sanitised; fake-codex replays it in its `app-server` mode (S3-11).
