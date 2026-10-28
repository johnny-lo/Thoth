# thoth interface contract for the thin adapter

This page lists exactly what the `thoth-dispatch` skill (`plugins/thoth/skills/thoth-dispatch/SKILL.md`) relies on: commands, flags, exit codes, and `--json` keys. **Everything listed here is a promise.** Removing or renaming any of it, or changing its meaning, is a breaking change: update this page and the skill in the same commit. Adding new keys is not a breaking change. Commands, flags and keys that are not listed are not promised. Full behaviour is in `docs/phase1/usage.md` and `docs/phase2/usage.md`.

*(2026-09-30: this tool was renamed from agentctl to thoth.)*

Verified against the code on branch `adapter/thin-adapter` (2026-09-24).

## Conventions

- **Exit codes:** `0` success. `1` any error or refusal; with `--json`, stderr carries `{"ok": false, "error": "..."}`. `3` a `wait` that timed out.
- **Ids:** task and run ids are 26-character ULIDs. Any unique prefix of 6 or more characters is accepted. Evidence codes look like `E001`.
- **Profiles:** a worker account is a registered profile. `thoth profile list` shows them; `thoth profile check ID` confirms the account is logged in; `thoth profile login ID` is how a person logs it in again. The supported harnesses are `claude-code`, `codex` and `antigravity`; any other value is refused without a run row or task change. Codex login comes from `codex login status`; Antigravity login comes from `agy models` exiting 0 without either stream containing `Please sign in`. Both report null email and subscription type. Additive (MODELS-1): the same `agy models` output is the account's model list (every `id<TAB>display name` line; the `Fetching available models...` line and blank lines are skipped, a repeated id keeps its first line, agy's order is kept), and every login check that stores the plan (`profile check`, `profile login`, `dispatch`, `review start`, `spec review start`) also stores that list on the profile (`global.db` schema 4: `profiles.models`, `profiles.models_seen_at`), when the check was logged in and read at least one model; otherwise the stored list is kept. A failed write is one `warning:` line on stderr and changes nothing else. `thoth profile check ID --json` gains `models_count` (the number of models stored by this check; null when it read none or the write failed) and `models_seen_at` (when, RFC 3339 UTC; null with `models_count`), and the human line ends ` models=N` (`models=-` for null). `auto` is a reserved word, not a profile id: `thoth profile add` refuses `auto` in any letter case (`auto`, `Auto`, `AUTO`), exiting `1` saying so. Breaking change (RJ-1): `--profile auto` no longer asks Thoth to pick a profile; `dispatch`, `review start` and `spec review start` refuse it (see `thoth dispatch` below).
  Additive (TIER-1): Thoth stores each profile's subscription tier as `plan` (lower case, 1–32 of `a-z 0-9 . _ -`) with the time it was seen. It takes Claude's `subscriptionType` from the login check that `profile login`, `profile check`, `dispatch`, `review start` and `spec review start` already run, and codex's `planType` from the app-server answer of `profile usage --refresh` (and, additive QR-1, with `live-quota` on and a codex profile due for the automatic refresh, of `recommend`; Breaking change (RJ-1): the launch commands no longer refresh, since `--profile auto` is gone); agy has no source. When a check does not report a valid tier, the stored one is kept. Output a person reads writes an account as `ID (Label)` (`ID（Label）` on the Traditional Chinese pages: the report, and doctor and setup in Chinese); `Label` is the tier with its first letter upper-cased, else the profile's `account_label` (control characters removed, at most 32 characters), else nothing (just `ID`). Account ids inside `--json` never change. `profile list --json` gives every profile three more keys: `plan` (string or null), `plan_label` (the bracket text or null) and `plan_seen_at` (RFC 3339 UTC or null).
- **Encoding:** spec and review files are UTF-8. A UTF-8 BOM is tolerated by `review record` only. Write files without a BOM.
- **MCP entry:** `thoth mcp` returns the same `--json` documents described on this page as tools; see [the MCP guide](mcp.md). `thoth guide` prints the commander guide; the `--json` keys are ok, version, guide.
- **Secret masking (RD-1):** content is masked at the moment it leaves Thoth, never where it is stored. Two kinds of secret are recognised. (1) Seven key formats, by their prefix and length: `anthropic-key` (`sk-ant-`), `openai-key` (`sk-`), `google-api-key` (`AIza`), `github-token` (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`), `gitlab-token` (`glpat-`), `aws-access-key-id` (`AKIA`, `ASIA`) and `private-key` (a block from the line that starts with `-----BEGIN`, names a `PRIVATE KEY` and ends in five dashes, through its `-----END` line, or to the end of the text when there is none; extra dashes before the BEGIN line, as a diff that deletes a key file has, are allowed). A `-` or `+` right at the start of a line (a diff's deleted or added line) does not hide a key format after it. (2) Exact values: the value of every environment variable of the thoth process whose name contains `KEY`, `TOKEN`, `SECRET` or `PASSWORD` (kind `env:NAME`), and the keys stored in `$THOTH_HOME/secrets.json` (kind `thoth:aistupidlevel-api-key`, `thoth:artificialanalysis-api-key`, or `thoth:secrets.json` for any other key), each also in its JSON-escaped form; a value shorter than 8 characters (after trimming) or made only of digits is not matched. Each secret is replaced by `[redacted:KIND]`; KIND never carries any part of the value. The exits that mask:
  - `evidence show` (content, label, detail);
  - `review packet` (stdout, `--out` and `--json`);
  - the `.agent/review-input/` files a codex or antigravity reviewer reads (`packet.md`, `evidence/<CODE>.txt`);
  - `.agent/evidence-index.json` (each `label`);
  - the prompt the runner hands every worker and reviewer (the run directory's `prompt.txt` is the same text);
  - a spec reviewer's `.agent/spec-review-input/spec.json`, and the author check's `proposed.json` and `edits.json`;
  - `.agent/handoff.md` (and the run directory's copy);
  - (RD-3) the document every command prints on stdout, JSON and human alike, and the error message on stderr (`{"ok": false, "error": …}` and `error: …`);
  - (RD-3) `thoth transcript` (each line masked before a step's text is cut to 300 characters);
  - (RD-3) MCP tool results, and the large results saved under `$THOTH_HOME/mcp/out`;
  - (RD-3) `report.html` (and `--out`);
  - (RD-3) the file `thoth records export` writes;
  - (RD-4) every other line thoth itself writes to stderr: a command-line parse error (with the arguments it quotes), `warning:` lines and notes such as `wrote FILE (N bytes)`, and what `thoth mcp` writes there; they keep the markers only and write no `secret_redacted`.

  (RD-4) Keys count too: every JSON exit masks each object's keys as well as its string values; a masked key keeps its value, and when two keys of one object mask to the same text the second and later ones, in the order of their original names, get `#2`, `#3`, … (the first number that names no other key). An exit that writes `secret_redacted` counts a masked key in `kinds`. (RD-4) A human line that cuts text when it is printed (`task result`'s source quotes, `spec review show`'s `after` preview) masks the whole text before cutting it, as `thoth transcript` does; the cut may fall inside a marker, never inside a secret.

  The stored blobs, database rows and logs are unchanged, and with nothing to mask every output is byte for byte what it was. Only exits that hand content over once write a `secret_redacted` event (see `thoth events`); the evidence index keeps the markers only, and so do the RD-3 exits (stdout, stderr, transcript, MCP results, `report.html`, `records export`), which never write `secret_redacted`. A worker, reviewer or spec reviewer starts without the two settings of kind secret (`AISTUPIDLEVEL_API_KEY`, `ARTIFICIALANALYSIS_API_KEY`) and without any other variable the masking takes for a secret; (RD-4) nor any variable, whatever its name, whose whole value is a known exact value (an environment value taken by its name, or a value in `secrets.json`); a variable that only contains one, such as PATH, stays; every other variable reaches it as before.

  Known limits: a report field holding an unterminated private-key header masks the rest of the page into one marker (over-masking, no leak); inside a run the masker sees only the run's stripped environment, so a value that exists only in the commander's environment is caught there by the formats alone, and `thoth config set-secret` is the way to have it masked everywhere.

## Commands

### `thoth profile usage [ID] [--json]`

Additive optional flag: `--refresh` (below).

Read-only report of the last quota observations already on disk. Without `ID`,
all registered profiles are listed in `profile_id` order; an unknown ID is an
error. It reads Claude `rate_limit_warning` events from registered repositories
and Codex rollout JSONL below each profile's `sessions/` directory. For Codex,
only the account's general bucket counts: `rate_limits.limit_id` `codex`, or no
`limit_id` at all; per-model reserve buckets (for example `base_model_inference`)
are skipped. Without `--refresh` it does not refresh usage or open credentials.
With `--refresh`, the user-owned `live-quota` switch and explicit flag are both
required. A disabled switch exits 1; individual update failures still exit 0
and appear in top-level `refresh` entries `{profile_id,status,error}`. Status is
`refreshed`, `unsupported`, `skipped_live_run`, or `failed`. Antigravity's
official agy supplies successful readings with source `agy_usage`; the
official codex CLI's app-server supplies codex readings with source
`codex_app_server`; the official claude CLI's local `/usage` command (`claude
-p /usage`, stream-json output) supplies claude-code readings with source
`claude_usage`. `skipped_live_run` (codex and claude-code, `error` null) means
the profile had a queued or running run, so codex or claude was not started.

With `--json`, the shape is `{"profiles":[...]}`. Each profile object has:

| field | meaning |
|---|---|
| `profile_id` | registered profile identifier |
| `provider` | provider recorded in `global.db` |
| `harness` | `claude-code`, `codex` or `antigravity` |
| `status` | profile status from `global.db` |
| `live_runs` | runs for this account with status `queued` or `running`, across registered repositories |
| `windows` | last known quota windows, empty when no reading exists |
| `extra_buckets` | additive: a codex profile's other quota buckets (for example `base_model_inference` / `gpt-reserve`), each `{limit_id, limit_name (string or null), windows}` with `windows` in the same shape as above, ordered by `limit_id`, read from the same rollout file as `windows`; always `[]` for a claude-code profile. Informational: `recommend` ignores it |
| `quota_state` | additive: `ok`, `unknown` or `exhausted`; same definition as the `quota_state` of a `recommend` candidate (see `thoth recommend` below) |
| `peak_percent` | additive: number only when `quota_state` is `ok`, otherwise null; same definition as the `peak_percent` of a `recommend` candidate |
| `newest_observed_at` | additive: RFC 3339 UTC time of the newest reading, or null when there is none; same definition as the `newest_observed_at` of a `recommend` candidate |
| `quota_group` | additive: selected agy model group, or null for ungrouped profiles |
| `parked_group` | additive (FC-1): for a `rate_limited` profile, the agy quota group its newest `rate_limited` quota event recorded (`detail.quota_group`); null otherwise, and null when no group was recorded (no `live_quota` reading at the time) |
| `plan` | additive (TIER-1): the stored subscription tier, or null; with `--refresh` it is read after the refresh, so a tier the refresh just stored is already here |
| `plan_label` | additive (TIER-1): the text Thoth writes in brackets after the account id (see Conventions), or null |

Each `windows` entry has `name`, `used_percent` (0–100), `resets_at` (RFC
3339 UTC or `null`), `observed_at` (RFC 3339 UTC), `source` (`thoth_events`,
`codex_rollout`, `codex_app_server`, `agy_usage`, or `claude_usage`), `group` (agy group or null), and
`reset_since_observed` (whether the known reset time is
not later than now). A claude-code profile's windows take, name by name, the
newer of the run-event reading (`thoth_events`) and the live reading
(`claude_usage`). Human output shows the same facts one profile per block.

### `thoth recommend --role implementer|reviewer [--task TASK] [--json]`

*(2026-09-30: ranking changed to account for quota observation freshness.)*

Additive optional flag: `--model MODEL`, the model about to be dispatched; it
selects which agy model group decides each grouped profile's quota (below).

Read-only ranking advice; it never dispatches a run. The exception (additive,
QR-1): with the `live-quota` switch on, it first refreshes the agy and codex
profiles (not `logged_out` ones) whose newest reading is more than 10 minutes
old or missing, one after another, by starting agy's `/usage` or codex's
app-server as `profile usage --refresh` does, waiting at most the smaller of
`THOTH_LIVE_QUOTA_TIMEOUT_MS` and 10 seconds each; then it opens `global.db`
for writing and stores the readings (and a codex `planType`). Claude Code
profiles are not refreshed automatically, only by `profile usage --refresh`. Nothing is
refreshed with the switch off, with `THOTH_RUN_ID` set, or when nothing is due.
A refresh never changes the exit code: each failure prints `warning: live
quota refresh for ID failed: ERROR; ranking with the previous reading` (or
`; ranking without a quota reading` when the profile had none) on stderr, with
or without `--json`, and the ranking uses the stored reading. `--json` gains
`live_quota_refresh`: `{"enabled": bool, "skipped": null|"off"|"worker",
"results": [{"profile_id", "status", "error"}]}`, `status` being `refreshed`,
`failed` or `skipped_live_run` (a codex profile with a live run) and `error` a
string or null; only due profiles are listed. For either role, `TASK`
accepts a full task ID or unique prefix of at least six characters, including
tasks with no run, looked up across every registered repository; one that
cannot be resolved exits `1`.

Breaking change (RJ-1): `recommend` lists each profile's quota facts and does
not choose a profile, model or effort for you; the commander does, and gives
them to `dispatch`, `review start` and `spec review start`.

Breaking change (RJ-2): besides the account facts, `recommend` sends the three
external tables filtered by fact only, Thoth's own usage per account, model,
effort and task type, and, with `--task`, the task's reject history (all
below). It reads the three score caches once, and with `scores-auto-refresh`
on the first call of the day refreshes a missing or 24-hour-old cache first,
exactly as `thoth models` and `thoth report` do (see `thoth models`); a
failed refresh prints one `warning: automatic score refresh failed: …` line
and the old cache is used, the exit code unchanged. `dispatch`, `review start`
and `spec review start` still read no cache and never refresh.

A profile is eligible when it is active (or a parked `rate_limited` profile has
a known reset after its latest rate-limit event) and is not exhausted. `exhausted` means any window is at least 95%
and has not reset, regardless of observation age. Otherwise quota is `unknown`
when there is no reading or the newest reading is over 24 hours old, and `ok`
when it is fresh. Eligible profiles are ranked by reviewer cross-provider
preference (when applicable), no live runs, `ok` before `unknown`, lower peak
usage, then profile ID. Ineligible profiles follow in profile ID order.
Additive (FC-1): a `logged_out` profile is never eligible, with the reason
`status logged_out; run `thoth profile login ID``. A `rate_limited` antigravity
profile whose `parked_group` is known and differs from the selected
`quota_group` is "parked elsewhere": it is eligible by the ordinary rules (not
exhausted), `recovered` is false, and its reasons say `parked for quota
group G; the selected group H is unaffected` instead of the `thoth profile
check` sentence. For any
other parked profile with a known `parked_group`, only that group's windows
decide whether it has reset since it was parked.

Breaking change (RJ-1): a used-up or parked profile no longer holds back the
other profiles with the same harness or provider (the `same_provider_rotation`
setting is gone); each profile is judged on its own facts only, and the
reasons never say `same-provider rotation is off`.

With `--json`, the shape is:

```json
{"role":"implementer","task_id":null,"model":null,"candidates":[...],"live_quota_refresh":{...},"score_refresh":{...},"tables":{...},"usage":[...],"task_history":null}
```

Breaking change (RJ-2): the top-level fields are exactly `role`, `task_id`
(the resolved full id, null without `--task`), `model` (the supplied value,
otherwise null), `candidates` (all registered profiles in rank order),
`live_quota_refresh`, `score_refresh`, `tables`, `usage` and `task_history`.
Breaking change (RJ-1): `recommended`, `recommended_launch` and `selection` are
gone; the first eligible candidate is what `recommended` used to name. Each candidate has `profile_id`,
`provider`, `harness`, `eligible`, and `reasons` (facts explaining status,
quota, live runs, and ranking), plus `quota_state` (`ok`, `unknown`, or
`exhausted`), `peak_percent` (number only for `ok`, otherwise null),
`newest_observed_at` (RFC 3339 UTC or null), `quota_group` (selected agy group,
otherwise null), `parked_group` (additive, FC-1: same definition as in `thoth
profile usage`), `recovered`, (additive, TIER-1) `plan` and
`plan_label` (same definitions as in `thoth profile usage`), and (RJ-2)
`available_models`: `[{model, display_name}]`, the models the account can
run, for the commander to look up in the tables, as `thoth models` lists
them: claude-code's fixed aliases; a codex account's `models_cache.json`
slugs (`[]` without the file); an antigravity account's stored list, what
its last login check read from `agy models` (MODELS-1), with the display
names agy printed (`[]` before any check). Grouped profiles use the selected
group for state, peak and eligibility. `--model` matches agy's descriptions;
zero or multiple matches fall back to the lowest-peak non-exhausted group. A
rate-limited reason tells the operator to run `thoth profile check ID` unless
the known window reset makes it recoverable for this recommendation. The
command never changes the stored profile status. Breaking change (RJ-1): a
candidate no longer has `models` or `held`, and the plain output no longer has
the `launch:` and `selection:` lines (the decision table is still in `thoth
models`). This remains read-only and does not dispatch anything.

`score_refresh` (RJ-2) has the shape of `thoth models`' `auto_refresh`:
`enabled`, `skipped` (null, or `off`, `worker`, `busy`), `attempted`,
`refreshed` (source names) and `failed` (`{source, error}`).

`tables` (RJ-2) has exactly the keys `artificial_analysis`, `agent_arena` and
`ai_stupid_level`. Thoth filters by fact only: it keeps the rows of the
organizations with an account registered here (ORGS: every registered
profile's provider, any status, plus for an antigravity profile the
organizations its model names start with: `claude-` anthropic, `gpt-`
openai, `gemini-` google; both sides compared after the same normalization
`thoth models` applies to names) and the columns listed below. It never maps
a row to an account (no `profile_id` appears in `tables`), never ranks and
never recommends. Each table has `source` (`name`, `url`), `license` (`text`,
`url`: Agent Arena `CC BY 4.0` and its link; Artificial Analysis the terms:
cite the source, the data stays on this computer and must not be forwarded;
AI Stupid Level the license text its cache carries, null without a cache),
`filtered` (one fixed sentence saying what was kept and that the full table is
at the source; the Agent Arena sentence also notes the modification as CC BY
asks, and that the subset boards are not comparable with each other or with
the main board), `present`, `fetched_at`, `age_hours`, `stale` (as `thoth
models`' `cache`) and `rows` (`[]` without a cache). Rows:

- `agent_arena`: the main board (`agent`) rows whose `organization` is in
  ORGS, in the source's `rank` order; each `model_name`, `organization`,
  `effort` (the effort word of a trailing parenthesis, as Artificial Analysis
  names are read; null without one), `score`, `score_ci_lower`,
  `score_ci_upper`, `rank`, `observation_count` and `signals` (one entry per
  `agent_*` subset board that has the identical `model_name`: `{score,
  rank}`; subsets without it are left out). `license`, `category` and
  `session_count` are not sent; a model that appears only in a subset is not
  sent. The table also has `sha` (the dataset version) and `publish_date`
  (the main board's `leaderboard_publish_date`).
- `artificial_analysis`: the `data[]` rows whose `model_creator.slug` or
  `model_creator.name`, normalized, equals an ORG or starts with `ORG-`; each
  `name`, `effort`, `intelligence_index`, `coding_index` and `agentic_index`.
  No prices or other fields.
- `ai_stupid_level`: the `models.data[]` rows whose `provider` is in ORGS;
  each only `name`, `degraded` (`status` not `good` or `trend` `down`, the
  `may_have_degraded` rule of `thoth models`) and `observed_at`
  (`lastUpdated`). No scores.

`usage` (RJ-2) is Thoth's own usage: one row per `role`, `profile`, `model`,
`effort` (the `launch.effort` sent) and `task_type` (difficulty merged), over
this computer's runs plus the shared records folder's lines whose run is not
here (each run once, as `thoth models`' decision table merges them); only
accounts registered here; `--role implementer` counts implementer runs,
`--role reviewer` reviewer and spec reviewer runs. `role` is the run's own
role (`implementer`, `reviewer` or `spec_reviewer`), so reviewer and spec
reviewer runs of the same account, model, effort and task type are two rows;
sorted like the decision table rows, then by `role`. Each row has exactly
those five keys plus `plan`, `plan_label` (as a candidate's), `shared` (the
account's `profile set-shared` mark),
`machines` (sorted), `runs`, `completed`, `tokens_avg` (each run's
max(`total`, `input` + `output`), averaged; null when all are 0),
`quota_runs` (the runs behind `quota_points_avg`), `quota_points_avg`,
`quota_points_weekly_avg`, `quota_runs_unreliable`, `estimate_reliable`,
`runs_left` and `runs_left_window`. Nothing about verdicts: no `pass`,
`reject`, `pass_rate` or any field computed from a review (the records keep
usage; model quality is the external tables' business). The quota numbers
use this computer's runs only (an account of the same name on another
computer may be another subscription; `runs`, `completed`, `tokens_avg` and
`machines` do count the shared lines): with reliable `run_quota` events the
averages are theirs and `estimate_reliable` is true unless the account is
`shared` (a reliable event without numbers still counts as one: the averages
are then null and `quota_runs` 0, never the unreliable events' numbers); only
with no reliable event at all are the unreliable events that have numbers
averaged, with `estimate_reliable` false (so a shared account still gets a
number, marked as not its own); with neither both averages are null.
`runs_left`: for
`five_hour` and `seven_day`, the account's current window of that name
(absent: nothing; already reset: 0% used), remaining = max(0, 95 − used), and
when that window's average is above 0, floor(remaining ÷ average); the
smaller of the two is `runs_left` and its window name `runs_left_window`;
both null when neither can be computed; an account whose `quota_state` is
`exhausted` has `runs_left` 0; an antigravity account always null (agy
records no `run_quota`).

`task_history` (RJ-2) is null without `--task`; otherwise `attempts` (the
highest `attempt` of any run of the task, a spec review's included since it
carries the attempt the next dispatch will use; 0 without a run), `rejects` (every REJECT
review of the task, external ones included), `last_implementer` and
`last_reviewer` (the newest implementer and reviewer run: `run_id`,
`attempt`, `profile`, `provider`, `model`, `effort_requested`, `effort`,
`status`, `verdict` — an implementer's newest review verdict, the verdict a
reviewer gave; null without such a run). Facts only: nothing says whether to
change the model or effort.

Human output (RJ-2): with `--task`, one line before the account list, `task
T: attempt N, rejected K time(s); last implementer run R: PROFILE MODEL effort
E (VERDICT)` (`task T: no runs yet` when the task has no run of any role;
`… last implementer run: none` when its only runs are spec reviews), and for
`--role reviewer` a `last reviewer run …` line; after the account list one
line per table (`NAME: N row(s) (filtered), fetched AT` or `NAME: not
cached`), one `usage: …` line per usage row ending in `runs left N (WINDOW)`
(`runs left 0 (window unknown)` for an exhausted account whose runs give no
average) or `runs left unknown`, with `(estimate unreliable)` when the
estimate is not the account's own, and a last line saying the table rows are
in `--json`.

### `thoth config`

Precedence for every setting: environment variable, then `$THOTH_HOME/config.json` (or `$THOTH_HOME/secrets.json` for the two API keys), then the built-in default (command-line flags beat all three). When config.json is broken, the other commands run with the defaults and print one line `warning: … cannot be used` on stderr; `config list` and `config get` print nothing on stderr and `config list` and `doctor` report the problem in their output.

`thoth config list [--json]` returns four top-level keys.
- `settings`: 16 entries in a fixed order (additive, RJ-2: `arena-url` after `aa-url`; 15 before), each with `key`, `value`, `source` (`env`, `file` or `default`), `env` (the variable name or `null`), `settable`, `kind` (`switch`, `language`, `text`, `integer`, `path` or `secret`), `default` (a string) and `description`. The keys, with their environment variable and default: `live-quota`, `scores-auto-refresh`, `require-reason` (switches, no variable, `off`; Breaking change (RJ-1): `same-provider-rotation` is gone, see below); `language` (`THOTH_LANG`, `en`); `claude-bin` (`THOTH_CLAUDE_BIN`, `claude`); `codex-bin` (`THOTH_CODEX_BIN`, `codex`); `agy-bin` (`THOTH_AGY_BIN`, `agy`); `live-quota-timeout-ms` (`THOTH_LIVE_QUOTA_TIMEOUT_MS`, `30000`); `scores-url` (`THOTH_SCORES_URL`, `https://aistupidlevel.info/api/v1/models?period=latest&sortBy=combined`), `aa-url` (`THOTH_AA_URL`, `https://artificialanalysis.ai/api/v2/language/models/free?page=`) and `arena-url` (`THOTH_ARENA_URL`, `https://huggingface.co`: the Hugging Face Hub that serves the Agent Arena dataset; a trailing `/` is ignored; the index is `BASE/api/datasets/lmarena-ai/leaderboard-dataset` and each board `BASE/datasets/lmarena-ai/leaderboard-dataset/resolve/SHA/BOARD/latest-00000-of-00001.parquet`); `machine-name` (`THOTH_MACHINE_NAME`, the host name) and `records-dir` (`THOTH_RECORDS_DIR`, `unset`); `home` (`THOTH_HOME`, `~/.thoth`; read-only); `aistupidlevel-api-key` (`AISTUPIDLEVEL_API_KEY`) and `artificialanalysis-api-key` (`ARTIFICIALANALYSIS_API_KEY`), whose `value` is only `set` or `unset`, never the key; stored with `set-secret`, source `file` means secrets.json.
- `config_file`: `path`, `exists`, `ok`, `error` (`null` when ok) and `unknown_keys` (sorted). Breaking change (RJ-1): a `same_provider_rotation` left in an old config.json has no effect and is not listed in `unknown_keys` (`doctor`'s config item stays ok and leaves it out of `stored_keys`; its `data` no longer has `same_provider_rotation`); `config get`, `set` and `unset` of `same-provider-rotation` exit `1` saying the setting was removed and has no effect, and `config set` of another key keeps it in the file.
- `secrets_file` (CFG-2): `path` (`$THOTH_HOME/secrets.json`), `exists`, `ok`, `error` (`null` when ok), `unknown_keys` (the keys in the file other than `aistupidlevel_api_key` and `artificialanalysis_api_key`, sorted), `private` (Linux: whether only the owner can read it; `null` on Windows and when the file does not exist) and `read` (the opposite of "THOTH_RUN_ID is set"). While THOTH_RUN_ID is set the file is not opened: `ok`, `error` and `unknown_keys` are `null`, `path`, `exists` and `private` are still filled, and the two keys resolve from the environment only.
- `elsewhere`: `profiles` (`count`, `null` when global.db does not exist or cannot be read; `command`) and `checks` (`null` when the working directory is not an initialized repository or its database cannot be read; otherwise `total`, `approved`, `protected_patterns`, `command`). Read-only: nothing is created or migrated.

`thoth config get KEY [--json]` returns `key` and `value` for any of the 16 keys.

`thoth config set KEY VALUE [--json]` returns `ok`, `key`, `value`, `previous`, `notice` and `shadowed_by` (the environment variable that overrides the stored value in this environment, or `null`). `previous` is `on`/`off` for a switch and the previously stored value as a string (or `null`) for the others; `notice` is `null` except when one of the three switches is turned on. Breaking change (RJ-1): `require-reason` now asks for `--reason` on every `dispatch`, `review start` and `spec review start`, not only on a launch that differs from a recommendation. Values: switches `on` or `off`; text (`claude-bin`, `codex-bin`, `agy-bin`, `scores-url`, `aa-url`, `arena-url`) a non-empty string; `machine-name` 1 to 64 characters from `A-Z a-z 0-9 . _ -`, not starting with `.` or `-` (else `invalid setting value '…' for machine-name`); `records-dir` an absolute path (else `invalid setting value '…' for records-dir: must be an absolute path`); `live-quota-timeout-ms` a whole number of at least 1; `language` `en` or `zh-TW` (any letter case, `zh_tw` too). `home` cannot be stored; the two API keys cannot be stored with `config set` (use `config set-secret`). The commander (agent) must not execute `config set`, `config unset`, `config set-secret` or `config unset-secret`, and must never ask the human to paste a key into the conversation; it may only ask a human to run them in a terminal.

`thoth config unset KEY [--json]` removes the stored value so the environment variable or default applies. It returns `ok`, `key`, `previous` (the stored value as a string, or `null`), `value` and `source` after the removal. It does not create config.json.

`thoth config set-secret KEY [--json]` (CFG-2) stores one of the two API keys in `$THOTH_HOME/secrets.json` (created readable by the owner only on Linux). KEY is `aistupidlevel-api-key` or `artificialanalysis-api-key`. The value is never taken from the command line: when stdin is a terminal it is typed at a hidden prompt (nothing is echoed), otherwise exactly one line is read from stdin (`thoth config set-secret KEY < FILE`). The value must be one line, not empty, a single token without whitespace or control characters, at most 1024 bytes; one trailing `\n` or `\r\n` is dropped. It returns `ok`, `key`, `value` (always `set`), `source` (always `file`), `previous` (`set` or `unset`: whether the file already held this key), `shadowed_by` (the environment variable when it is set and not blank in this environment, else `null`) and `path` (the absolute path of secrets.json). Refused, exit `1`, nothing written: a value given after KEY on the command line (it is not echoed back); an unknown KEY (not echoed back either); a KEY that is not a secret; THOTH_RUN_ID set; a secrets.json that is not a JSON object; a terminal whose echo cannot be switched off (use `< FILE`). No output ever contains the value.

`thoth config unset-secret KEY [--json]` removes the stored key so the environment variable, if any, applies. It returns `ok`, `key`, `previous` (`set` or `unset`), `value` and `source` after the removal. It does not create secrets.json and keeps the file (as `{}`) after the last key; it is refused while THOTH_RUN_ID is set.

While THOTH_RUN_ID is set, secrets.json is not used: a worker's thoth sees only the environment (the file is read only to mask its values in output).

### `thoth doctor [--shell powershell|cmd|bash|sh] [--json]`

New (SETUP-1). A read-only health check: it creates no directory, writes no database and changes no file.

Exit code: `0` when no item is `fail` (`warn` and `unknown` do not count); `1` when at least one item is `fail`; `1` with `{"ok":false,"error":...}` on stderr (with `--json`) when doctor cannot run at all, for example when `THOTH_HOME`, `USERPROFILE` and `HOME` are all unset. `--shell` accepts only the four values above; anything else is a usage error (exit `2`).

`--json` prints one object with `ok` (true when no item is `fail`), `version` (this thoth's version), `shell` (`powershell`, `cmd`, `bash` or `sh`: the shell the fix commands are written for), `shell_source` (`flag`, `env` or `fallback`) and `items`.

Each item has `id`, `status` (`ok`, `warn`, `fail` or `unknown`), `detail` (a string), `fix` (a string or `null`; never `null` for `fail`; several lines are separated by `\n`) and `data` (an object or `null`, per item). Items always come in this order: `state_dir`, `cli_claude`, `cli_codex`, `cli_agy`, `profiles`, one `profile:<profile_id>` per registered profile (sorted by id), `repo`, `checks`, `config`, `plugin`.

The `config` item's `data` also has `stored_keys` (the known keys present in config.json, sorted); a `cli_*` item's `data.source` is `env`, `file` or `path`. Additive (DT-3): and `machine_name` and `records_dir` (`{value, source, exists}`); the item is `warn` with a fix when records-dir is set and is not a directory; the config item's data also has `secrets_file` (CFG-2: the same object as `config list`'s without `read`) and `secrets` (set/unset with source per key), and the item is `warn` with a fix when secrets.json is unusable, has unknown keys or (Linux) is readable by other users.

`detail` and `fix` are always English. `THOTH_LANG=zh-TW` changes only the human output (without `--json`); the JSON document is the same byte for byte.

### `thoth setup [--shell powershell|cmd|bash|sh]`

New (SETUP-3). **For a person at a terminal only.** It asks questions on stdin and changes things when the answer is yes, so a commander or the plugin must not run it; there is no `--json` (`thoth setup --json` is a usage error, exit `2`) and no MCP tool. Without a terminal on stdin and stdout, or with `THOTH_RUN_ID` set, it exits `1` at once, touching nothing, with `error: …` on stderr and nothing on stdout.

Exit code: `0` when, after all its steps, no item is `fail` (the same items and statuses as `thoth doctor`); `1` when at least one still is, or when it refused; `2` usage error.

Answering no to "Enable it now?" ends the plugin step for this run: it does not go on to ask about an update. What a commander should do instead: run `thoth doctor --json` and, when something needs a person, ask them to open a terminal and type `thoth setup`.

### `thoth profile login [ID] [--harness claude-code|codex|antigravity] [--provider P] [--label L] [--json]`

New (SETUP-2). **For a person at a terminal only.** A commander or the plugin must not run it; there is no MCP tool for it. It opens the CLI's own login screen in the same terminal, then registers the profile if the CLI's status command says it is logged in. Exit `1` has two kinds. **Refused** (no terminal on stdin and stdout, `THOTH_RUN_ID` set, an argument rule broken, or an id that differs from a registered profile only in letter case): the login screen was not opened, stdout is empty, and stderr has one line (`{"ok":false,"error":...}` with `--json`, otherwise `error: …`). **Not logged in** (the login screen was opened and the CLI did not end logged in): with `--json`, stdout is the object below (`ok`, `logged_in` and `registered` all false) and thoth's one-line note about the login screen is on stderr; without `--json`, that note and the one human sentence (run the same command again) are both on stdout. The CLI's own screen appears in the terminal in both cases.

Exit code: `0` logged in and registered; `1` refused or not logged in (as above); `2` usage error (for example an unknown `--harness`).

With `--json`, stdout carries one object (the CLI's own screen still appears in the terminal, and thoth's one-line note about it goes to stderr):

- `ok`: true when logged in and registered.
- `profile_id`, `harness`, `provider`, `config_dir`: the profile logged in to; `config_dir` is the directory registered (or, when not logged in, the one that would have been).
- `new`: true when the profile was not registered before this run.
- `logged_in`, `registered`: both true on success (`registered` is also true for a profile that was already registered); both false when not logged in, and then nothing was written.
- `email`, `subscription_type`: what the CLI's status command reported, or null; null when not logged in.
- `account_label`: the profile's label after this run (`--label`, else for a new profile the subscription type), or null; null when not logged in.
- `cli_exit`: the CLI's exit code, or null when it had none (killed by a signal). It does not decide the result.

The JSON keys and values do not depend on `THOTH_LANG`.

### `thoth profile set-shared ID on|off [--json]`

New (DT-1). **For a person at a terminal only.** A commander or the plugin must not run it; there is no MCP tool for it. It marks a profile as one a human also uses by hand (`on`) or not (`off`); `run_quota` events of runs on a marked profile say `reliable: false` with the reason `shared`. The mark is kept in `global.db`.

Exit code: `0` written; `1` refused (`THOTH_RUN_ID` is set) or no such profile, stdout empty and one line on stderr; `2` usage error (a value other than `on` or `off`).

With `--json`, stdout carries one object: `ok` (true), `profile_id`, `shared` (the new value) and `previous` (the value before). `thoth profile list --json` gains `shared` (boolean) on every entry of `profiles`.

### `thoth profile forget-model ID MODEL [--json]`

New (MODELS-1). CLI only, no MCP tool (like `profile check` and `profile login`). Writes one `quota_events` row of kind `model_forgotten` (detail: `model` as given, trimmed; `source` `profile forget-model`) and changes nothing else on the profile, so the launch commands accept MODEL on a claude-code profile again after a run recorded it as unavailable (see "Model rule for a `claude-code` profile" under `thoth dispatch`). MODEL is matched in any letter case. Exit `1` for an unknown profile or a blank MODEL, nothing written. With `--json`, stdout carries `ok` (true), `profile_id` and `model`.

### `thoth report [--out FILE] [--open] [--section ID] [--json]`

Writes one self-contained, read-only HTML status page for the human owner, in
Traditional Chinese, to `FILE` or by default `$THOTH_HOME/report.html`. The
page is replaced atomically. Databases are opened read-only; the page is the
only file written. Works from any directory. It exits non-zero when there is no
`global.db` yet. With `--open`, it also opens the page in the default browser;
if that fails, it prints a warning to stderr and still exits 0.

`--section ID` requires `--open` (exit 2 without it) and opens the page at
that section, as a `file://` URL with `#ID`. `ID` is lowercase letters, digits
and dashes; today the one section is `decision-table`. Any other shape exits 1
with a message naming `--section`, before the page is written.

Relied on: exit code; with `--json`, `path` (the file written) and
`generated_at` (RFC 3339 UTC). The page's content and markup are for people,
not a contract.

Once `$THOTH_HOME/report.html` exists, it is also rewritten automatically
after every state-changing command (`task create`, `task update-spec`,
`task close`, `dispatch`, `resume`, `cancel`, `submit`, `review start`,
`review record`) and by the runner after it settles a run. That rewrite is
best effort, never creates the file and never changes the command's exit code
or output, so re-running `thoth report` is only needed to create the page
or to write it with `--out`.

Additive (RD-3): the page, both the one `thoth report` writes and the
automatic rewrite, is masked (see Secret masking above), each exact value
also in its HTML-escaped form; no `secret_redacted` is written.

### `thoth retro [--days N] [--json]`

New (H10), compatible. Groups the review findings of the last `N` days (default 7, 1 to 3650; anything else exits 2) by category, across every repo registered on this machine (see docs/phase2/usage.md, "Repeated findings: `thoth retro`"). Works from any directory, reads the databases read-only, writes nothing; with no `global.db` it reports nothing and exits 0.

Relied on, with `--json`:
- `days`; `since` (RFC 3339 UTC, `days` × 24 hours before now); `reviews` (reviews created since then); `findings` (unmet criteria plus extra findings in them); `sizes` (`S`, `M`, `L`, `unsized`, integers);
- `categories`: one entry per category found, most findings first, ties by name; `[]` when there is none (with no finding, `findings` and every `sizes` value are 0, `reviews` still counts the reviews in the window, and every key is present). Each entry: `category` (a name from the review's category list, or `unclassified`), `count`, `task_count`, `tasks` (`[{repo, task_id, title}]`, `repo` being the repo's root path, in the order each task first appears), `sizes` (same four keys), `suggest_check` (true when the category has two or more findings in the window, counting findings and not tasks, so a repeat in one task's rework counts, and is neither `other` nor `unclassified`), and `findings` (`[{repo, task_id, review_id, created_at, source, id, severity, fix_size, note}]`: `source` is `criterion` or `extra`; `id` is the AC id, `null` for an extra finding; `severity` is the extra finding's, `null` for a criterion; `fix_size` is `null` when absent; `note` is `""` when absent).

### `thoth init --json`
Registers the current git repo with thoth, once per repo. Every other command fails with an error containing `is not initialised` until this has run. Relied on: exit code.

### `thoth status --json`
Lists every task that is not `pass`, `cancelled` or `accepted` (`accepted` is additive, DOC-1). Relied on: `tasks[].task_id`, `tasks[].title`, `tasks[].status`. Additive (CK-1): a `last_verify` key: `null` when the repository has no `verify_ran` event, else `{"task_id","run_id","ts","seq","missing_since_last_verify"}` of the newest `verify_ran` of any task (`missing_since_last_verify` as under `verify`; `[]` for an event written before CK-1). The `tasks` array is unchanged. Additive (SMALL-8): an `outside_repo` key, a `note` key and `tasks[].repo`. Inside a repository `outside_repo` is `false`, `note` is `null` and every task's `repo` is the repository's root; outside a git repository the command exits 0 instead of failing, `outside_repo` is `true`, `note` says that every registered repository is listed and that dispatch, verify and review need a repository, `last_verify` is `null`, and `tasks` holds the open tasks of every repository registered by `thoth init` whose state still exists, ordered by root, each with `repo` set to that root (the same string as `thoth init --json`'s `repo_root`).

### `thoth status TASK --json`

Additive (CK-1): a `verify` key: `null` when the task has no `verify_ran` event, else `{"run_id","ts","seq","ok" (failures == 0),"count","failures","source","at_commit" (null when absent),"approved_checks","previous_verify","missing_since_last_verify"}` read from the task's newest `verify_ran` event (`approved_checks` and `previous_verify` are null and `missing_since_last_verify` is `[]` for an event written before CK-1).

Additive (BASE-1): `base_branch`, the local branch the task started from, as under `task create` (read from its newest `task_created` event; null when that event has none). `status --json` without TASK and the human output do not change.

The task document additionally includes `stuck` (null or an object with
`kind`, `value`, and `count: 3`). Human task output starts with
`[stuck: VALUE]` when present; the no-argument task list is unchanged.
Relied on: `status`, `attempt`, `latest_verdict`, and each `runs[]` entry's `run_id`, `role` (`implementer` | `reviewer`), `status`, `interrupted_reason` (`quota` | `crash` | `user` | `timeout` | `auth` | `transient` | null; `auth` and `transient` are additive, FC-1), `worktree` (once the task is `pass` or `cancelled`, this directory has usually been removed, WT-1), `candidate_commit`, `run_dir` and `error_text`. Additive (FC-1): each `runs[]` entry also has `next_step` (string or null): what to run next, naming the run's profile, run id, task id and model. It is set for an `interrupted` run with reason `quota` (Breaking change (RJ-3): `quota used up on PROFILE; dispatch again with a profile that still has quota: thoth dispatch TASK --profile OTHER --model MODEL --effort LEVEL --reason TEXT (profiles with quota now: A, B); do not wait for PROFILE`; a reviewer run says `start the review again with the next reviewer in your order that still has quota` with `thoth review start`, a spec reviewer run `start the spec review again with the next reviewer in your order that still has quota` with `thoth spec review start`; ` --effort LEVEL` is the run's requested level and is left out when it has none; `--reason TEXT` is for the commander's reason for the new choice; the brackets say `no other profile has quota now` when the list is empty, or `could not read the other profiles' quota`; the text is the same with or without a session and no longer tells the commander to wait for the profile's reset or to `thoth resume` the run), `auth` (`thoth profile login PROFILE`, then `thoth resume RUN`), `transient` (`thoth resume RUN`) or `crash` (`thoth resume RUN` once), and for a `failed` run with reason `timeout` (dispatch again with a larger `--timeout`) or no reason (read `error_text`, then dispatch again); a run with no `harness_session_id` gets `thoth dispatch TASK --profile PROFILE --model MODEL` in place of `thoth resume RUN` (for `auth`, `transient` and `crash`). Null for every other run (completed, cancelled, queued, running, failed with reason `user`). The human output adds ` reason=REASON` to each run line that has one and, after the run lines, `  next: TEXT` for the newest run when it has a next step. Additive: each `runs[]` entry also has `models_used` (object model id → output tokens, null when unknown) and `subagents_spawned` (integer, null when unknown). Additive (S3-7): each `runs[]` entry also has `effort_requested` and `effort` (strings read from the run's `launch` settings, null when `--effort` was not given). Breaking change (RJ-1): the runner no longer hands a run stopped by quota over to another profile; the run ends `interrupted` with reason `quota` and the commander dispatches again. Each `runs[]` entry has `profiles_with_quota`: for a run stopped by quota, the ids of the other profiles that have quota now (eligible in `recommend --role implementer` without a task or model, in its order, without the run's own profile; possibly `[]`), read only when the task has such a run, read-only and without a refresh; null for every other run, and null when the quota could not be read. One attempt can still have several implementer runs on different profiles when the commander rerouted it; the newest one is the live or final one. Model names are informational; a `subagents_spawned` above 0 means the worker delegated, which the commander should look at. Additive (SR-2): `runs[].role` may be `spec_reviewer`.

### `thoth task create -f SPEC.json --title TITLE --json`
Relied on: `task_id`. The spec is a JSON object:

| key | required | meaning |
|---|---|---|
| `goal` | yes | one non-blank sentence |
| `acceptance_criteria` | yes | non-empty array of strings; write each as `"AC1: ..."`. The prefix before the colon is the criterion id a review must use; without a prefix the id is `AC1`, `AC2`, … by position |
| `constraints`, `non_goals`, `excluded_alternatives` | no | arrays of strings |
| `risk` | no | `low` \| `normal` (default) \| `important` \| `high` |
| `checks` | no | array of check ids (`[a-z0-9_-]{1,32}`, no duplicates, none blank) from the approved-check catalogue (`thoth checks`); a bad list exits `1`. When the catalogue has any check, an id it lacks is a spec lint error (`unknown_check`, below) and exits `1`. An id that is in the catalogue but not approved yet, or any id while the catalogue is empty, is only a warning (see below). Additive |
| `task_type` | no | `code` \| `research` \| `review` \| `docs` \| `mechanical`; null is the same as leaving it out. Any other value (another string, a number) exits `1` from `task create` and `task update-spec`, the error listing the five values. Breaking change (RJ-1): nothing picks by it any more; the `launch_choice` event records it as written, null when missing. Additive (S3-8) |
| `difficulty` | no | `low` \| `normal` \| `high`; null is the same as leaving it out. Any other value exits `1` the same way, listing the three values. Breaking change (RJ-1): missing no longer means `normal`; the `launch_choice` event records null. Additive (S3-8) |

Additive: `task create --json` and `task update-spec --json` also print `warnings`, an array of strings (empty when there is nothing to say), one `check X is not approved yet; approve it with: thoth checks approve --reason "WHY"` (**Breaking change, LESS-5**: it said `ask the human to run: thoth checks approve`) per listed check that is in the repository's catalogue but unapproved, or missing while the catalogue is still empty (a listed id missing from a non-empty catalogue is the `unknown_check` lint error below). Without `--json` the same text goes to stderr as `warning: …`. Additive (S3-8): the `task_created` and `spec_updated` events' `detail` has `task_type` and `difficulty` (the spec's values, null when absent).

Breaking change (LESS-3): once the spec parses, `task create` runs `thoth spec lint` on it (below) before writing anything. With any lint error it exits `1` and writes nothing (no task, no event); the error (the `error` of `--json`'s stderr document) starts with `spec lint: N blocking problem(s): ` followed by each error as `RULE (LOCATION): MESSAGE`, joined by `; `. Lint warnings do not stop it: `warnings` gains one `spec lint: RULE (LOCATION): MESSAGE` line per warning, after every other line.

Additive refusal (RD-2): before spec lint, when any line of the spec or any of its string values, or `--title`, holds what looks like a secret (see Secret masking above), `task create` exits `1` and writes nothing (no task, no event). The error starts with `refusing: the spec contains what looks like a secret` or `refusing: the title contains what looks like a secret` and lists the kinds, the field paths (for example `github-token in acceptance_criteria[1]`) and the line numbers, never the value. (RD-4) Any object key, at any depth, is checked too, as parsed, so a `\u` escape in the file does not hide it; in the field path that key is shown as its marker.

Additive (WT-1): after the task is created, `task create` removes the clean worktrees of every `pass` / `cancelled` / `accepted` task (the same sweep as `worktrees prune`, below; skipped when `THOTH_RUN_ID` is set or the current directory is inside the worktrees directory). The `--json` keys do not change; the `worktree_removed` / `worktree_kept` events are the record, and only a failure of the sweep shows, as one `worktree cleanup failed: …` entry in `warnings[]`.

Additive (SR-1): for an `important` / `high` task, `warnings` gains one line, after the check warnings and before a worktree cleanup failure: `risk RISK: a spec review APPROVE of this spec is required before the first dispatch; record one with thoth spec review record TASK -f spec-review.json, or dispatch with --skip-spec-review REASON`. The `task_created` event's `detail` carries `spec_sha256`, the sha256 of the spec bytes stored.

Additive (BASE-1): `--base REF` sets where the task starts: any name that resolves to a commit (a branch, a tag, a full or short sha, `HEAD~1`), resolved in the current directory, so `HEAD` means the HEAD of the worktree the command runs in. `base_commit` stores the full sha resolved at creation; a branch that moves later does not move the task. A REF that resolves to no commit (an unknown name, a tree or blob, blank, or one starting with `-`) exits `1` and writes nothing (no task, no event, no worktree cleanup); the error starts with `--base ` and contains REF as given. Without `--base` the start is the HEAD of the worktree the command runs in (a linked worktree's own HEAD, not the main worktree's); from the main worktree this is the same as before. The repository and its state folder are still those of the main worktree, from any worktree. `task create --json` gains `base_branch`: with `--base`, REF (trimmed) when it is exactly a local branch name (`refs/heads/REF` exists), else null (a sha, a tag, `origin/x`, `long~1`); without it, the branch checked out in the current worktree, null when its HEAD is detached. The `task_created` event's `detail` gains `base_branch` and `base_ref` (REF as given, null without `--base`). `task show --json` also gains `base_branch`, read from the task's newest `task_created` event (null for a task created before BASE-1).

### `thoth dispatch TASK --profile ID [--model MODEL] [--effort LEVEL] [--reason TEXT] --json`
Starts an implementer run; it does not wait for it. Relied on: `run_id`, `attempt`, `rework`, `folded_findings`, `folded_by`, `worktree`, `warnings`. **Model rule for a `codex` profile:** `--model` must be a codex model (e.g. `gpt-5.6-luna`); a Claude name (`haiku`, `sonnet`, `opus`, `fable`, or `claude-*`) exits non-zero with the codex models listed. Breaking change (RJ-1): there is no default model and no rework step-up any more, so `--model` is always given on the first dispatch of an attempt (see "Model and effort" below). `--allowed-tools` does not apply to codex. The same rule applies to `resume` of a codex run. It is accepted from these task statuses:

**`--effort LEVEL` (additive, S3-7).** `dispatch` and `review start` accept `--effort low|medium|high|xhigh|max|ultra` (case-insensitive). Any other value exits `1` before a run row, worktree or event exists, and the error lists the six levels. Thoth lowers the request to what the harness and model support (Claude Code: Haiku has no effort; Sonnet, Opus and Fable take low to max, `claude-opus-4-6` and `claude-sonnet-4-6` skip xhigh; codex: the levels in the account's `models_cache.json`; an unlisted Claude model or a missing codex cache is passed as given with a warning; antigravity: see the Breaking change (AGY-1) below), picking the highest supported level below the request, or the lowest supported one when none is below. The result is stored in the run's spec under `launch` (the task's own spec is not changed), and a `resume` reuses it. Breaking change (RJ-1): `--effort` left out is no longer worked out by a rule; see "Model and effort" below. Every new run's spec has `launch`, with exactly `effort_requested`, `effort` and `effort_note`. Additive keys of `--json`: `effort_requested` (the level asked for, as normalised), `effort` (the level actually sent, null when none was) and `effort_note` (a note about the level, also in `warnings`). The note is not null in these cases: the requested level is not in the supported list and another level is sent (the highest supported level below the request, else the lowest supported one); the model has no effort setting (`NoEffort`, nothing is sent); the harness has no effort setting (`NotApplicable`, nothing is sent); or Thoth cannot tell what the model supports (for example a codex account without `models_cache.json`, or a Claude model name not in Thoth's table), in which case the level is sent as requested and the note says it could not be confirmed. The note is null only when the requested level is confirmed supported and sent as given. A `launch` key in the task's own spec is not used: every run's spec gets its own `launch` from Thoth. Both `dispatch` and `review start` have them.

**Breaking change (AGY-1): `--effort` on an `antigravity` profile.** It used to be `NotApplicable` (nothing sent, `effort` always null, the flag only warned `has no effort setting`). Now agy's own `--effort LEVEL` is sent, with these rules. An agy model whose name ends in `-low`, `-medium`, `-high`, `-xhigh` or `-max` (trimmed, any letter case: `gemini-3.8-flash-high`, `gpt-oss-120b-medium`) supports exactly that level: `--effort` may be left out, and then `effort` is that level, `effort_requested` null and `effort_note` `effort LEVEL is carried by the model variant MODEL`; a `--effort` equal to it is sent as given with a null note; any other `--effort` is lowered to it with the usual `… is not supported by MODEL; using LEVEL` note. No `--effort` flag ever reaches agy for such a name (agy refuses the pair with `--model X conflicts with --effort=Y`); the name is passed as given. Any other agy model (`claude-sonnet-4-6`, `claude-opus-4-6-thinking`) supports `low`, `medium`, `high`, `xhigh` and `max` (`ultra` is lowered to `max` with a note), needs `--effort` on the first launch like a Claude or codex model (left out: exit `1`, nothing written, the error naming `--effort`), and its launch gets `--effort LEVEL` right after `--model MODEL`, before `--dangerously-skip-permissions`, on a fresh run, a resume, a reviewer and a spec reviewer. The `effort` key of `dispatch --json`, `review start --json`, `spec review start --json`, `status TASK --json` and `run_started` is therefore no longer null for an agy run. Whether agy accepts the flag for a name without a level is still to be confirmed on a real run; a confirmed refusal will make that model `NoEffort` in a later change.

**Model and effort (Breaking change, RJ-1).** Thoth no longer picks the profile, model or effort; the commander gives them, and `thoth recommend --role ROLE --task TASK` shows each profile's quota to choose from.
- `auto` is refused: `--profile auto`, `--model auto` or `--effort auto` (the last two in any letter case, with surrounding spaces) exits `1` before anything else is checked or written, saying the value was removed, that Thoth no longer picks for you, that you give the value yourself, and naming `thoth recommend --role ROLE --task TASK`. `--json` loses the keys `auto`, `auto_reasons` and `live_quota_refresh`, no `auto_pick` event is written, and the launch commands no longer refresh live quota (only `recommend` does).
- The first dispatch of an attempt needs `--model` and `--effort`. A reroute in the same attempt (the newest implementer run of this attempt exists; a rework after a REJECT is a new attempt, so it has none) keeps the previous run's model and its requested effort for whatever is left out, and `warnings` gains one line saying what was kept from which run. A model kept for a profile whose harness differs from the previous run's profile (or whose profile is no longer registered) is refused, naming `--model`. `--effort` can also be left out when the model has no effort setting (haiku; nothing is sent then) or (Breaking change, AGY-1) when it is an agy model whose name ends in its level (`gemini-3.8-flash-high`; that level is recorded as `effort` and no flag is sent, see "`--effort` on an `antigravity` profile" above; any other agy model needs it). Otherwise a missing flag exits `1` before anything is written, naming the missing flag(s) and saying Thoth no longer picks them.
- The order of the refusals: `auto`; `--effort`'s level and a blank `--reason`; the task and its status (and the spec review gate, below); the profile and its harness; the model (given, or kept, with the harness check) and its model rule; a missing `--model` / `--effort`; `require-reason`; then the login check, the model list check (MODELS-1, right after the login check stored the plan and the list: the agy list rule and the Claude unavailable-model rule below), the stuck check, the attempt cap and the spec rewrite, as before.
- There is no default model, no rework step-up and no `treated as` warning any more; `--effort LEVEL`'s checks and lowering and the codex and agy model rules are unchanged (MODELS-1 changes only what the agy and codex refusals list, and adds the list check after the login check).

**`--reason TEXT` and `require-reason` (additive, DT-2).** `dispatch`, `review start` and `spec review start` take `--reason TEXT` (MCP: `reason`): Breaking change (RJ-1): why this profile, model and effort were chosen (it used to be why a launch differed from Thoth's recommendation; there is no recommendation any more). It is trimmed; an empty or blank one exits `1` with `--reason must not be empty` before anything is written. With the switch `require-reason` on (`thoth config set require-reason on`, off by default; a human sets it), every launch without `--reason`, a reroute in the same attempt included, exits `1` (with `--json`, `{"ok":false,"error":...}` on stderr) before any run row, event, status change or worktree is written; the error names `--reason`, says every launch needs one saying why this profile, model and effort were chosen (MCP: the reason parameter), and that a human can run `thoth config set require-reason off`. There are no exemptions any more. With the switch off the reason can be left out. Every launch writes a `launch_choice` event (see `events` below), and `reason` is null when none was given. `dispatch --json` and `review start --json` have no new keys.
- A profile that is `rate_limited` but whose window reset since it was parked (`recovered` in `recommend`) is set back to `active` with a `window_reset` quota event whose `detail.source` is `auto` (Breaking change (RJ-1): `detail.profile_auto` is gone), only after every check passed and the run row exists; the "marked rate_limited" warning is not given for it. FC-1: this holds for every launch (`review start` and `spec review start` likewise). A `logged_out` profile whose login check passes is set back to `active` the same way, with a `login_restored` quota event (`detail.source` `dispatch` or `review_start`, `task_id`, `run_id`). A `rate_limited` antigravity profile parked for another quota group than the model's (`parked_group` in `recommend`) gets no warning and keeps its status. When the login check fails, the error is `profile ID failed its auth check; run `thoth profile check ID`; to log in again, run `thoth profile login ID``.

Breaking change (RJ-1): a run is never moved to another profile by Thoth. The runner's handover is gone: a run that stops on its quota ends `interrupted` with reason `quota`, and `status TASK` points to the profiles that still have quota (`next_step`, `profiles_with_quota`); the commander dispatches again.

**Timeouts for an `antigravity` profile:** thoth passes agy `--print-timeout` set to the run's timeout plus 300 s; if agy's own limit still expires the run is `failed` with `interrupted_reason` `timeout` (see docs/phase2/usage.md, "Antigravity workers").

**Model rule for an `antigravity` profile** (Breaking change, MODELS-1: Thoth no
longer carries a written-down agy model list; the account's own list is the
rule). `dispatch`, `review start`, `spec review start` and `resume` still
refuse the four Claude aliases, an empty name and every known codex model at
the model-rule step, but the refusal now lists the profile's stored list
(comma-separated slugs, in agy's order), or ends `no model list stored; run
`thoth profile check ID`` when none is stored yet. Then, after the login check
has passed and stored the list it just read (and before the stuck check, the
attempt cap, the worktree, the run row and every event; `review start` and
`spec review start`: before `ensure_detached_worktree` and the run row), the
model is compared against that list: the one the login check just read, or,
when the check read no `id<TAB>name` line, the stored one. Both sides are
trimmed and compared without regard to letter case; the string sent to agy is
the one given. A model outside the list exits `1` with
`profile ID runs antigravity, and 'MODEL' is not in its model list (read at
TIME): SLUG, SLUG, …; run `thoth profile check ID` to read the list again`
(`--json`: `{"ok":false,"error":...}` on stderr); no run row, event or
worktree is written, the task status does not change and agy is not started.
With no list at all (nothing just read, nothing stored) the launch goes ahead
and `warnings` gains `no model list for ID; model MODEL was not checked`
(human output: the same line as `warning:` on stderr). `resume` has no login
check, so it uses the stored list only, with the same refusal and warning
(the warning is in `resume --json`'s `warnings`). Full agy ids such as
`claude-sonnet-4-6` and `gpt-oss-120b-medium` are accepted when the account's
list has them. There is no timer: every launch reads the list again, and
`thoth profile check ID` reads it on demand.

**Model rule for a `claude-code` profile** (additive, MODELS-1). The fixed
aliases stay; what a run proves the account cannot use is learned from the
run. When a Claude run's result line says `api_error`
`model_requires_usage_credits` (the shape of a Pro account asked for Fable:
`is_error` true, `api_error_status` 429, `api_error_code`
`credits_required`, `result` `Fable 5.1 requires usage credits. …`), the run
is `failed` (not `interrupted` / `quota`), `interrupted_reason` null,
`error_text` starting `model unavailable on this account: ` followed by the
text; the task follows the runner's role rules (an implementer run writes
`failed`, a reviewer or spec reviewer run leaves the task alone); the profile
stays `active`, no `rate_limited` quota event and no `quota_exceeded` event is
written, and no login check runs. Instead the runner writes a `quota_events`
row of kind `model_unavailable` (detail: `model`, `run_id`, `task_id`,
`api_error`, `api_error_code`, `error_text`, `plan` (the profile's stored
plan at that moment, or null)) and the run gets a `model_unavailable` event
(see `events`). Only that one `api_error` value counts: a result whose
`api_error_code` is `credits_required` but whose `api_error` is something
else, and every real usage limit (`usage_limit_reached`, `Claude AI usage
limit reached`, a 429 without these names), are still quota stops. From then
on the four launch commands (`dispatch`, `review start`, `spec review start`
at the model list step above; `resume` at its model-rule step) refuse that
model on that profile (matched without regard to letter case) while the
newest `model_unavailable` / `model_forgotten` event naming it is a
`model_unavailable` whose recorded `plan` equals the profile's stored plan
(null equals null): exit `1`, writing nothing, with `profile ID cannot run
model 'MODEL': a run on this account was refused at TIME (TEXT); models this
profile can still use: …; if the account has changed, run `thoth profile
forget-model ID MODEL``. A plan that changed since (the launch's own login
check stores the plan it sees, so a dispatch after an upgrade clears it by
itself) or a later `thoth profile forget-model ID MODEL` lets the model
through again. **`--allowed-tools`
for an `antigravity` profile** is agy's tool allow-list: left at its default
(`Bash,Read,Write,Edit,Glob,Grep`) it is stored as agy's implementer list
(Breaking change, AGY-1: `run_command,command_status,send_command_input,read_file,view_file,list_dir,find_by_name,grep_search,write_file,edit_file,write_to_file,replace_file_content,multi_replace_file_content,sed_file,finish,wait,wait_5_seconds`;
agy 1.4.4's `read_file`, `write_file` and `edit_file` beside the 1.4.2 names, both generations matched exactly; it used to be the 14 old names);
any other value is stored verbatim, so pass the whole list plus the extra tool
to allow one more. `resume` reuses the stored value. Additive: `dispatch --json`
and each `runs[]` entry of `status TASK --json` carry `allowed_tools`, the
stored string (Claude and codex runs store `--allowed-tools` exactly as given).
Additive (RES-1a): only for a task whose spec `task_type` is `research`, the
worker gets its harness's web tools. A `claude-code` run stores the given value
followed by `,WebFetch,WebSearch` (a tool already in it is not added twice), so
both reach `--allowedTools` and `--tools`; an `antigravity` run stores the list
above followed by `,search_web,read_url_content` the same way; a `codex` run
stores `--allowed-tools` as given and its launch gets `-c
sandbox_workspace_write.network_access=true -c web_search="live"` right after the
sandbox pair. An empty `--allowed-tools ""` stays empty. Every other
`task_type` (`docs` and `review` included, or none) is unchanged, and reviewer
runs get web tools only for source checks of research tasks (RES-1b, see `review start`).
- `queued`, `interrupted`, `unverified`, `failed`, and `in_review` (a reroute that keeps the attempt);
- `reject`: attempt + 1, a new worktree from the previous candidate, and the latest REJECT's unmet criteria and extra findings folded into the spec (`folded_by: "control_plane"`). At `important` / `high` risk a `reject` is refused until `task update-spec` rewrites the spec **after** that REJECT; once it has, dispatch proceeds the same way but folds nothing (`folded_by: "spec_rewrite"`, `folded_findings: 0`, no warning) because the rewritten spec is the answer. A later REJECT on the same task is refused again until the next `task update-spec`. `folded_by` is `null` outside a `reject` dispatch. Breaking change (RJ-1): the rework no longer steps up one level from the previous run; it is a new attempt, so it needs `--model` and `--effort` like a first dispatch. A dispatch out of `unverified` is the same attempt and keeps the previous run's model and effort for what is left out.

  Additive (RES-1b): research source checks fold each `not_supported` source
  into `unmet`: `claim N (S1): CLAIM_TEXT — URL: REASON`. Each `unreachable`
  source goes into `extra_findings` before the document's extras:
  `unreachable: claim N (S1): CLAIM_TEXT — URL: REASON; replace the source`.
  N is its position among all target result claims (from 1). If the source
  cannot be mapped back to that review's target result, the body is
  `S1: URL: REASON` instead. Supported sources are omitted; both lists count
  in `folded_findings`.

`draft`, `running`, `pass`, `cancelled` and `accepted` (DOC-1, additive) are refused with exit `1`.

After three identical post-spec REJECT reasons, dispatch refuses automatic
rework before creating a run or worktree. The error names the criterion,
normalized finding, or non-zero `result.json` rule and points to `task
update-spec` or `task close`; `--allow-attempt` cannot bypass it. With
`--json`, the additive error document has `stuck` with `kind` and `value`.

**Spec review gate (additive, SR-1).** An `important` / `high` task that has no implementer run yet (of any status) is dispatched only when its newest spec review (`thoth spec review record`, below) is an `APPROVE` of its current spec (the sha256 of the stored spec bytes). Otherwise `dispatch` exits `1` before anything is picked, probed or written (no run, no event, no worktree, the status unchanged) with `task TASK is risk 'RISK' and has not been dispatched yet: its current spec (sha256 SHORT) has no APPROVE spec review (WHY). …`, WHY being `no spec review is recorded`, `the newest spec review ID is REVISE` or `the newest APPROVE ID is for an older spec (sha256 SHORT)`. Additive (SR-3): when WHY is `the newest spec review ID is REVISE` and `spec review show`'s `pending_edits` is true, WHY continues, inside the parentheses, with `; its N edits await the author check: thoth spec review confirm TASK --profile P --model M --effort E`; nothing else in the message changes. `--skip-spec-review REASON` (MCP: `skip_spec_review`) dispatches anyway; a blank reason exits `1` with `--skip-spec-review must not be empty`. Whenever the flag is given and the dispatch goes ahead (needed or not) a `spec_review_skipped` event is written on the new run. After 3 `REVISE` in a row with no `APPROVE` between them, every dispatch of the task (any risk, first or not) exits `1` with `task TASK is stuck: its spec was sent back by N spec reviews in a row (IDS). …`; `--skip-spec-review` does not apply, and only an `APPROVE` of the current spec or `task close` ends it. When the task's spec or risk changed after the gate read it (an `update-spec` while the dispatch was in flight), `dispatch` exits `1` with `task TASK's spec changed while it was being dispatched; dispatch again so the spec review gate sees the new spec`, writing no run and no event, leaving the status unchanged and removing the worktree it had created. Additive key of `--json`, after `folded_by`: `spec_review`, `{required, approved, skipped, spec_review_id}` (`required`: the gate applied to this dispatch; `approved`: the newest spec review is an APPROVE of the current spec; `skipped`: `--skip-spec-review` was given; `spec_review_id`: the newest spec review's id, or null). No warning is added, whether skipped or not.

**Long prompts (additive, W3, 2026-10-06).** Claude and codex get the prompt on stdin, not on the command line (`prompt.txt` in the run dir is still the full text sent); agy still takes it on the command line. When a harness's final command line is over 30,000 UTF-16 units, the runner starts nothing: `dispatch` itself still exits 0, and the run then settles `failed` (task `failed`) with an `error_text` that starts with `command line too long:` and gives the length and the cap. `status TASK --json` shows it as that run's `error_text`. For a long agy prompt, dispatch to a claude or codex profile instead.

**Handoff (additive, S3-9).** A dispatch out of `interrupted` or `failed` whose same attempt's newest started implementer run ended `interrupted` or `failed` (quota, timeout, crash) is a handoff, whichever profile it goes to (the same one included). Before the run row is written it writes `.agent/handoff.md` in the worktree, and `handoff.md` in the new run's `run_dir`: Markdown built only from Thoth's database and git (why the previous run stopped, the branch, `git log`/`git status`/`git diff --stat`, the attempt's command evidence, what to do next); it never includes the previous worker's conversation, stream, stderr, summary or result.json. The new worker's prompt names the path and does not include the content. If either copy cannot be written, dispatch exits `1` and writes no run and no event; if the dispatch is refused after the write, the worktree copy is removed. Any other dispatch removes a stale `.agent/handoff.md` left in the worktree (a warning if it cannot). Additive key of `--json`: `handoff`, `null` when the dispatch was not a handoff, else an object with `previous_run`, `reason` (the previous run's `interrupted_reason`, or its status when it has none), `cross_provider` (true only when both the provider label, trimmed and case-insensitive, and the harness differ; an unknown previous provider counts as different) and `path` (always `.agent/handoff.md`). The run's spec (never the task's) gets a `handoff` object with `previous_run`, `reason`, `from_profile`, `from_harness` and `path`, next to any `launch`. Additive (S3-10): it also keeps the previous run's `rework` and `reroute` objects (copied as they are, where present), so the prompt still lists why the attempt was sent back; `--json`'s `rework`, `folded_findings` and `folded_by` are unchanged (`false`, `0`, `null`).

### `thoth wait TASK --timeout 9m --json`
Returns when the task reaches `in_review`, `unverified`, `failed`, `cancelled` or `interrupted`. Relied on: `status`, `timed_out`. On timeout it exits `3`; calling it again is safe. Breaking change (RJ-1): there is no handover on quota any more, so a run stopped by quota leaves the task `interrupted` and `wait TASK` returns. Additive (SR-2): `thoth wait --run RUN --json` on a `spec_reviewer` run fills `verdict` with the spec review verdict (`APPROVE` | `REVISE`), null when the run reached none. Additive (AGY-1): `thoth wait --run RUN --json` carries `tools_unknown` on both its branches (exit `0` and the exit `3` timeout, whose other extra keys stay null): the `tools_unknown` array of that run's newest `run_usage` event (an antigravity run's denied agy tool names Thoth does not know, sorted, distinct), `[]` when the run has no `run_usage` yet (still running, crashed before its result), when its newest usage has no such key (a Claude or codex run, or one written before AGY-1) or the key is not an array of strings; the run's own events only, never another run of the task, and a resume's usage replaces the interrupted launch's. The exit codes, `verdict` and the other keys keep their meaning. Human output adds one line after the first, `agy called tools Thoth does not know: NAME, NAME; its tool names may have changed (AGY-1)`, only when the array is not empty. The MCP tool `thoth_wait` (`run`) returns the same document.

### `thoth evidence list TASK --json`
Relied on: each `evidence[]` entry's `code`, `kind` (`command` | `diff` | `commit`), `collected_by`, `label` and `exit_code` (null for `diff` / `commit`). An antigravity command's `exit_code` is read from agy's transcript and is null when it never appeared there (a `command_exit_code_unknown` event says so). Additive, in each stream-recorded command row's stored detail: `step_index` (agy's step number) and `exit_code_source` (`generic` | `background_task` | null); both are null on codex rows.

Additive (TESTS-1): each `evidence[]` entry also carries `detail` (the row's stored detail parsed as JSON, any JSON value, with the same parsing as `evidence show`; null when the stored text is not JSON) and `blob_path` (string, the stored blob's absolute path, or null). The existing keys, their values and types, the row order and the human output do not change; `list` still writes no event (only `evidence show` records `evidence_opened`). The MCP tool `thoth_evidence_list` returns the same document.

### `thoth evidence show CODE --task TASK [--bytes N] --json`
Relied on: `content`, `content_truncated`, `blob_verified`. `content` holds at most N bytes of the stored blob; N defaults to 4000, so pass `--bytes` for a diff and check `content_truncated`. It records an `evidence_opened` event.

Additive (RD-1): `content`, `label` and `detail` are masked (see Secret masking under Conventions). The whole blob is masked before the first N bytes are taken, so with something masked `content` is at most N bytes of the masked text, `content_truncated` says whether the masked text is longer than that, and the human output's `--- N more bytes; re-run with --bytes M ---` counts the masked text. `bytes`, `sha256` and `blob_verified` still describe the stored blob. When something was masked it also records one `secret_redacted` event (exit `evidence_show`, `evidence` `[CODE]`, the same `run_id` as the `evidence_opened` event); if that write fails, the output is unchanged, the exit code is `0` and stderr gets one `warning: could not record secret_redacted: REASON` line.

### `thoth transcript RUN [--offset N] --json`
Read-only: lists what a run's worker did, from the run directory's `stream.jsonl`. It reads from byte `--offset` (default 0) and consumes only complete lines, those ending in a newline; a partial last line is left for the next call. `next_offset` is where the next call should start (pass it as `--offset`), so a caller that polls gets only the new steps. `lines` counts the consumed lines and `skipped` the ones that were not JSON objects. `reset` is true when `--offset` was past the end of the file (it was recreated or shortened): the read then started at 0. When the file does not exist yet, the exit code is 0 and `exists` is false, `size`, `offset` and `next_offset` are 0 and `steps` is empty. It writes no event and changes nothing in the run directory.

`steps[]` entries always carry nine keys: `at` (the line's first byte in the file), `kind` (`start` | `message` | `tool` | `error` | `note` | `result`), `nested` (true for a subagent's step), `text`, `tool`, `action` (`read` | `write` | `edit` | `run` | `search` | `other`, null unless `kind` is `tool`), `target` (file path, command, pattern or URL), `exit_code` and `is_error` (null when the stream does not say). `text` and `target` are whitespace-collapsed and cut at 300 characters. Claude Code and codex streams are translated; an antigravity run's lines count in `lines` but produce no steps in this version. Relied on: `exists`, `next_offset`, `reset`, `harness`, and each `steps[]` entry's `kind`, `action`, `target`, `exit_code`, `is_error`, `nested`, `text`.

### `thoth events --since N --task TASK --json`

Additive (RES-1b): a source-check `verdict` detail has `criteria: []` and
`sources: [{"id","url","verdict","reason"}]`; other verdict events have no
`sources` key. `review_invalid` stages include `consistency` when the declared
verdict differs from the source verdicts, `coverage` for missing, unknown or
duplicate source ids, and `schema` for other invalid source-check content.
An implementer or source-check reviewer of a research task does not count
`web_search` in `run_usage.detail.unexpected_item_types`.
Relied on: `last_seq` (pass it as the next call's `--since` to get only newer events), each `events[]` entry's `seq`, `type` and `detail`, plus:
- the latest `result_ingested` event's `detail.result_json.claims[]` (`kind`, `text`, `evidence`), which are the implementer's claims;
- `detail.result_json.summary`, the implementer's own prose;
- a `result_invalid` event's `detail.rule` and `detail.reason`;
- additive (P7-2): a claim's `kind` may also be `protected_change`. Its `text` starts with the path of one protected file the worker changed, then `:` or whitespace and the reason; it needs no `evidence`. A `result_invalid` event may have `detail.rule` `6`, with `detail.path`, `detail.status` (`modified` | `deleted` | `type_changed` | `added`) and `detail.kind` (`test` | `check_input` | `check_word`): the run's diff changed that protected file and no `protected_change` claim declares it. `detail.reason` is `claim missing for protected file PATH (STATUS): add a claim of kind protected_change whose text starts with the path and explains the change`; a dispatch out of `unverified` quotes it to the next worker like any other reason. A `diff` evidence row's `detail` has `protected_changes`: `[{"path","status","kind"}]`, sorted by path, `[]` when none. An added file is listed only as `added` / `check_word`: its name, up to the first `.`, equals a word of some check's command (for example `cargo.bat` when a check is `cargo test`). Since SMALL-4 `protected_changes` is attributed to the task: it is `git diff --name-status --no-renames` from the task's base commit (not the run's) to the candidate, keeping only the paths whose content at the candidate also differs from the main worktree's HEAD (the integration tip); the tip is not used when it cannot be read, equals the candidate or already contains it, or (additive, BASE-1) does not contain the task's base commit (a task started on a branch the main worktree's HEAD has not merged), and (LESS-5) the diff `review start` and a PASS `review record` record for a run that has none (after `thoth submit`, or when the runner failed to record it) is attributed the same way. The same `detail` has two more keys: `protected_base` (string, the base the list was counted from) and `integration_tip` (string, or null when the tip was not used); the other keys keep their meaning;
- additive (RES-1a): a claim's `kind` may also be `research`, a factual claim of a research task with `sources`: `[{"url","quote"}]`, at least one, each `url` starting with `http://`, `https://` or `file://` (any case) with no whitespace, each `quote` not blank. No other kind may carry a non-empty `sources`; the key may be absent. A `result_invalid` event with `detail.rule` `1` now also covers a research claim with no sources, a bad `url`, a blank `quote`, and `sources` on another kind (`detail.reason` names the claim number and the field). New `detail.rule` `7`: a research task's `completed` result.json has no `research` claim (`blocked` is not checked); `detail.reason` is `a completed delivery of a research task needs at least one claim of kind research with its sources`. Rule 5 (high risk) does not apply to a `research` claim;
- a `rate_limit_warning` event's `detail.resets_at` (epoch seconds, as the harness reported it), and a `quota_exceeded` event's `detail.note` (the harness's own text, which usually names the reset).
- additive: a `run_started` event's `detail.harness` (`claude-code` | `codex` | `antigravity`) and `detail.worker_image` (for a Claude or antigravity run, the image name the OS reported for the worker pid right after it started, normally `agy` for antigravity, or null; always null for a codex run), and a codex run's `harness_init` event's `detail.worker_image` (the image read when `thread.started` arrived, after the npm launcher settled; normally `node`, or null). `cancel` checks the newest of these for the current launch before it kills the worker. A `run_usage` event of a codex run also has `detail.unexpected_item_types` (array of item types a locked-down codex worker should not produce; additive, RES-1a: an implementer run of a research task does not count `web_search`); one of an antigravity run has `detail.tool_names` (sorted distinct agy tool names the worker used). Additive (AGY-1): every `harness_init` event has `detail.tools` and `detail.tool_count`: for an antigravity run the tool list agy's `init` line reported, as written (order and repeats kept; an empty array is `[]` and `0`), and its length; both null when the line had no usable list (missing, null, not an array, or an element that is not a string) and always null for a Claude or codex run. A record only: agy 1.4.4 still reports the pre-rename names there while its model calls `read_file`, so it is never compared with the allow-lists.
- additive (S3-7): a `run_started` event's `detail.effort_requested` and `detail.effort` (the level asked for and the level actually sent; both null when the run has no `launch` settings). It is written for implementer and reviewer runs alike.
- additive (S3-8): an `auto_pick` event, one per run whose profile, model or effort `dispatch` / `review start` / `spec review start` (SR-2, `detail.role` `reviewer`) picked (`auto`), with the run's `task_id` and `run_id` and `detail.role` (`implementer` | `reviewer`), `detail.profile`, `detail.model`, `detail.effort_requested`, `detail.effort`, `detail.profile_auto`, `detail.model_auto`, `detail.effort_auto`, `detail.reasons` (the same strings as `auto_reasons`) and `detail.skipped`: `[{"profile","reason"}]`, one per registered profile `--profile auto` did not pick, with the reason described under `dispatch` (`[]` when the profile was named). None is written only when `--effort LEVEL` was given with a named profile (and a named or left-out model); the runner's handover always writes one. Breaking change (RJ-1): no longer written (nothing is picked); older events stay and `events` still lists them.
- additive (new event, S3-9): a `handoff_written` event, written by `dispatch` in the same transaction as the run row and before its `status_change`, with the new run's `task_id` and `run_id` and `detail.previous_run`, `detail.reason`, `detail.from_profile`, `detail.from_provider` (null when unknown), `detail.from_harness`, `detail.from_model`, `detail.to_profile`, `detail.to_provider`, `detail.to_harness`, `detail.to_model`, `detail.cross_provider` (boolean, as under `dispatch`), `detail.path`, `detail.sha256` and `detail.bytes` (of the file written). Breaking change (RJ-1): the runner's handover is gone, so only `dispatch` writes it.
- additive (new event, DT-2): a `launch_choice` event, exactly one per `dispatch`, `review start` and `spec review start` (SR-2), with the new run's `task_id` and `run_id`, written in the same transaction as the run row, after `handoff_written` and before the `status_change` (`review start`: after `review_started`; `spec review start`: after `spec_review_started`). Breaking change (RJ-1): `detail` has exactly these keys, all always present: `role` (`implementer` | `reviewer`; `reviewer` for `spec review start` too), `source` (`dispatch` | `review_start` | `spec_review_start`), `task_type` and `difficulty` (the task spec's labels as written, null when absent, no defaults), `risk` (the task's), `actual` (`{profile, model, effort_requested, effort}`, `effort` being the level sent), `inherited` (`{model, effort}` booleans: whether the value was kept from the previous run of the same attempt), `previous_run` (that run's id, or null; always null for `spec review start`), `reason` (the trimmed `--reason`, or null) and `require_reason` (the switch at that moment). `labels_defaulted`, `recommended`, `ranking`, `recommendation_error`, `differs`, `deviates`, `exempt` and `continuation` are gone from new events; older events keep them. A refused launch writes none.
- additive (new keys, CK-1): a `verify_ran` event's `detail.approved_checks`, `detail.previous_verify` (`{"run_id","task_id","ts","seq"}` or null) and `detail.missing_since_last_verify`; a `check_removed` event's `detail.was_approved` (boolean) and `detail.approved_at` (string or null).
- additive (new event, S3-10): a `handover` event, written by the runner in the same transaction as the new run's row when it continued a `--profile auto` implementer run stopped by quota on another profile, with the new run's `run_id` and `detail.attempt`, `detail.from_run`, `detail.to_run`, `detail.from_profile`, `detail.to_profile`, `detail.cross_provider` (as under `dispatch`), `detail.model`, `detail.effort_requested`, `detail.effort`, `detail.reasons` and `detail.skipped`: `[{"profile","reason"}]`, the profiles passed over (`reason` `no model`, `allowed tools` or `auth`), `[]` when none. Breaking change (RJ-1): no longer written, nor are `handover_skipped` and `handover_failed` (below); older events stay.
- additive (new event, S3-10): a `handover_skipped` event, with the stopped run's `run_id`, when such a run was not continued: `detail.attempt`, `detail.reason` (`cancel requested`, `limit`, `no eligible profile`, `live run`, or the passed-over profiles as `PROFILE: REASON` joined with `; `) and `detail.skipped` (as in `handover`, `[]` when none). By event `seq`, refusals during the initial judgment precede the stopped run's final `status_change`. A `live run` or a late `cancel requested` detected in the insertion transaction follows that `status_change`; the refusal event and any task status change commit in that same transaction, without inserting a new run.
- additive (new event, S3-10): a `handover_failed` event, when a handover started but could not finish (the task is then `interrupted`), with the new run's `run_id` (the stopped run's when no new run row was written) and `detail.attempt`, `detail.from_run`, `detail.to_run` (null when no new run row was written) and `detail.error`.
- additive: a `command_exit_code_unknown` event (antigravity only) with `detail.code` (the evidence code recorded with a null exit code), `detail.step_index`, `detail.cmd` and `detail.note`.
- additive: a `tool_denied` event (antigravity only), one per tool call the tool lock denied, with `detail.tool`, `detail.reason`, `detail.step_index`, `detail.conversation_id` and `detail.received_at`; and an antigravity run's `run_usage` event has `detail.tools_denied` (the number of those events, 0 when none; absent for Claude and codex runs).
- additive (AGY-1): a `tool_denied` event's `detail.known_to_thoth` (boolean): whether `detail.tool` is a name in Thoth's table of agy tool names (`AGY_KNOWN_TOOLS`: the recorded `init.tools`, the three names agy added later, and agy 1.4.4's `read_file`, `write_file`, `edit_file`), matched exactly; false when the denial has no usable name (a payload without `toolCall.name`). The hook's reason differs by it: a known name off the list says `… is not on this run's allow-list (…)`, an unknown one says `Thoth tool lock: the tool NAME is not a tool Thoth knows; agy may have renamed its tools, so update Thoth's agy tool table (AGY-1); this run's allow-list is (…)`; a payload or policy the hook cannot use keeps its own reason. Both are denials, exit 0, one line. The same boolean is in each line of `.agent/agy-hook-denials.jsonl`. An antigravity run's `run_usage` event also has `detail.tools_unknown` (array of strings: the distinct non-empty denied names with `known_to_thoth` false in this launch, sorted; `[]` when none; absent for Claude and codex runs, like `tools_denied`); `tools_denied` still counts every denial. The table only recognises names; it never produces or widens an allow-list.
- additive (new key, S3-12): a `rate_limit_warning` event's `detail.windows`, an array with one `{name, utilization, resets_at}` per entry of the harness's `rate_limit_info.unifiedWindows` (names as the harness wrote them, e.g. `five_hour`, `seven_day`; an entry without `utilization` is left out; `[]` when the harness sent none). `rate_limit_type`, `utilization` and `resets_at` keep their meaning and values.
- additive (new event, S3-12): a `run_quota` event, written by the runner for an implementer run of a claude-code or codex profile after its stream ends and before its final `status_change`, whatever the outcome (never for a reviewer or antigravity run; a resumed run writes another, so take each run's newest). `detail.window` is `five_hour`; `detail.start_percent` and `detail.end_percent` are the first and last reading of that window during the run, in percent; `detail.points` is end minus start, in percentage points; the three are numbers rounded to two decimals, or all null when either end has no reading or the window reset mid-run, i.e. the two ends' reset times differ (reset times up to 600 seconds apart count as the same window, because codex shifts the reset time by a few seconds at the start of a window; only one end having a reset time counts as different). One reading gives `points` 0. `detail.source` is `claude_stream` (the run's own `rate_limit_warning` events) or `codex_rollout` (the run's codex rollout file). It is a rough estimate: codex's `used_percent` is a whole number, and other work on the same account during the run counts too.
- additive (DT-1): a reviewer run of a claude-code or codex profile writes a `run_quota` event too (the S3-12 sentence "never for a reviewer" no longer holds; an antigravity run still writes none), by the same rule and at the same point. Every `run_quota` event gains three keys; the five above keep their names, values and meaning. `detail.seven_day` is an object `{start_percent, end_percent, points}`, the same numbers for the weekly window (`seven_day`; judged on its own, so a mid-run reset of the five-hour window does not null them). `detail.reliable` is a boolean: false when the numbers may include work that was not this run's. `detail.unreliable_reasons` is an array of strings, `[]` when `reliable` is true, in this order: `shared` (the run's profile is marked with `thoth profile set-shared`), `shared-check-failed`, `overlap:<run_id>` (one per other Thoth run of the same account, in any registered repo, any role and any status, whose time interval overlaps this run's; sorted by run id), `overlap-check-failed:<repo_id>` (that repo's state database exists but could not be looked at, opened or queried, or, for the run's own repo, the run's own row could not be re-read for its start time; a repo whose state database does not exist is skipped). A reader that wants an average should leave `reliable: false` events out. An event written before DT-1 has none of the three keys and counts as reliable.
- additive (new event, FC-1): a `login_failed` event, written by the runner when a run ended in error (not a quota stop, not cancelled or timed out) and the profile's own auth check said it is not logged in; the run is then `interrupted` with `interrupted_reason` `auth` and the profile `logged_out`. It has the run's `task_id` and `run_id` and `detail.profile` and `detail.note` (the run's error text, starting `login failure: PROFILE is not logged in; `). A `run_interrupted` event is written too, as for any interrupted run.
- additive (new key, FC-1): a `quota_exceeded` event's `detail.quota_group`: the antigravity quota group the run's model belongs to, from the profile's newest `live_quota` reading (exactly one matching group), else null; always null for claude-code and codex.
- additive (new event, MODELS-1): a `model_unavailable` event, written by the runner when a Claude run's result line said `api_error` `model_requires_usage_credits` (see "Model rule for a `claude-code` profile" under `dispatch`); the run is `failed` with no `interrupted_reason`, not a quota stop, and no `quota_exceeded` or `login_failed` event accompanies it. It has the run's `task_id` and `run_id` and `detail.profile`, `detail.model`, `detail.api_error` and `detail.note` (the run's error text, starting `model unavailable on this account: `). The matching `quota_events` row (kind `model_unavailable`) and `profile forget-model`'s row (kind `model_forgotten`) are in `global.db`, which no command lists; `thoth models` shows their effect as `unavailable`. A failure to write the row is `model_bookkeeping_error` in the run's final `status_change` detail and does not change the outcome.
- additive (new event, FC-2): a `run_retry` event, written by the runner each time it relaunched the harness in the same run after a transient failure (`interrupted_reason` `transient`), after the relaunched worker started and before its stream is read; at most two per run. It has the run's `task_id` and `run_id` and `detail.attempt` (1 or 2), `detail.error` (the failure's text, without the `transient interruption: ` prefix), `detail.exit_code` (the failed launch's, or null), `detail.wait_seconds` (how long the runner waited first), `detail.mode` (`resume` or `fresh`), `detail.session_id` (the session, thread or conversation continued; for a fresh Claude Code relaunch the new session id; else null), `detail.worker_pid` and `detail.worker_image` (the new launch's; `worker_image` null for codex, as in `run_started`). Every key is always present.
- additive (new event, WT-1): a `worktree_removed` event, with the task's `task_id` and no `run_id`, each time Thoth removed a finished task's worktree directory (the branch stays): `detail.task_id`, `detail.path` (`runs.worktree` as stored), `detail.kind` (`worker` | `review` | `spec_review`, the last additive, SR-2), `detail.head` (the commit the directory had checked out), `detail.branch` (the branch it had checked out, null for a detached review or spec review worktree), `detail.task_status` (`pass` | `cancelled`) and `detail.source` (`runner` | `review_record` | `task_close` | `task_create` | `worktrees_prune`).
- additive (new event, WT-1): a `worktree_kept` event, with the task's `task_id` and no `run_id`, when a finished task's worktree directory was kept: `detail.task_id`, `detail.path`, `detail.kind` (as above, `spec_review` included), `detail.reason` (`outside_worktrees_dir` | `uncommitted_changes` | `unreachable_head` | `git_error` | `remove_failed`), `detail.detail` (for `uncommitted_changes` the `git status --porcelain` lines outside `.agent/` and `.agents/`, at most 20 plus `... and N more`; for `unreachable_head` the commit; for `outside_worktrees_dir` the path; otherwise the error), `detail.task_status` and `detail.source` (as above). It is written once per path and reason: while the newest `worktree_kept` / `worktree_removed` event for that path is a `worktree_kept` with the same reason, later sweeps write none.
- additive (new event, SR-1): a `spec_review` event, with the task's `task_id`, one per `spec review record` (no `run_id`, `source` `external`, `reviewer_run_id` null) or per spec reviewer run that reached a valid verdict (SR-2: `run_id` and `detail.reviewer_run_id` the run's id, `source` `reviewer_run`): `detail.spec_review_id`, `detail.verdict` (`APPROVE` | `REVISE`), `detail.spec_sha256` (of the spec judged), `detail.risk`, `detail.task_status` (when recorded), `detail.criteria` (`[{"id","verdict","findings"}]`, `findings` being a count), `detail.findings` (the total count), `detail.revise_streak`, `detail.stalled`, `detail.source` (`external` | `reviewer_run`), `detail.reviewer_run_id` and `detail.document` (the validated document). Every key is always present. Additive (SR-3): `source` may also be `author_check` (the author's check, `run_id` and `reviewer_run_id` the confirm run's). Every writer now also sets `detail.mode` (`external` | `review` | `edit` | `confirm`), `detail.edits` (the edits Thoth applied, `[{"index","field","op","at","criterion","anchor","before","after","reason"}]`, `before` null for an insert and `after` null for a delete; `[]` without edits), `detail.edit_count`, `detail.proposed_spec` (the proposed spec's whole text; null without edits), `detail.proposed_sha256` (null without edits), `detail.bytes_before` and `detail.bytes_after` (the stored spec's size and the proposed spec's; equal without edits); `detail.document` stays the document the reviewer or author handed in (in edit mode it includes `edits` as written). An author's check (`mode` `confirm`) adds `detail.confirms_spec_review_id`, `detail.spec_sha256_before` and `detail.applied` (true on its APPROVE, which replaced the spec: `spec_sha256` is then the new spec's), and copies `edits`, `edit_count`, `proposed_spec`, `proposed_sha256`, `bytes_before` and `bytes_after` from the edit review it judged.
- additive (new event, SR-1): a `spec_review_invalid` event, with the task's `task_id` and no `run_id`, when `spec review record` refused the document: `detail.stage` (`schema` | `spec` | `coverage`), `detail.reason`, `detail.source` (`external`), and `detail.expected` (the criterion ids) at the coverage stage. Additive (SR-2): the runner writes one for a spec reviewer run whose report it refused, with the run's `run_id`, `detail.source` `reviewer_run`, `detail.reviewer_run_id` (the run's id) and `detail.stage` one of `read` | `schema` | `spec` | `coverage` | `started` | `spec_changed` | `write`. Additive (SR-3): an edit-mode run may also be refused at stage `edits` (an edit breaks a rule; the reason names the edit's index, field and why; an edit whose text carries a mask marker the stored spec does not have is refused at this stage too) or `lint` (the proposed spec fails `spec lint`); an author's check (`detail.source` `author_check`) at `read` | `started` | `schema` | `coverage` | `superseded` | `lint` | `spec` | `spec_changed` | `status` | `write`.
- additive (new event, SR-2): a `spec_review_started` event, with the task's `task_id` and the spec reviewer run's `run_id`, one per `spec review start`, in the same transaction as the run row: `detail.spec_sha256`, `detail.reviewed_commit`, `detail.task_base_commit`, `detail.risk`, `detail.task_status`, `detail.worktree`, `detail.worktree_reused`, `detail.worktree_moved`, `detail.cleared_stale_report`, `detail.profile`, `detail.model`, `detail.allowed_tools`, `detail.timeout_seconds`. Every key is always present. Additive (SR-3): `detail.mode` (`review` | `edit` | `confirm`; an event written before SR-3 has none and reads as `review`); `spec review confirm` writes one too, with `detail.confirms_spec_review_id` and `detail.proposed_sha256` besides.
- additive (SR-3): a `spec_updated` event is also written by the runner when an author's check APPROVEs (see `spec review confirm`), with the confirm run's `run_id` and, besides `task update-spec`'s keys, `detail.source` `author_check`, `detail.spec_review_id` and `detail.confirms_spec_review_id`. A `launch_choice` event's `detail.source` may also be `spec_review_confirm`.
- additive (new event, SR-1): a `spec_review_skipped` event, on the new run, when `dispatch --skip-spec-review` went ahead: `detail.reason`, `detail.required`, `detail.risk`, `detail.spec_sha256`, `detail.newest_spec_review` (id or null), `detail.newest_verdict` (`APPROVE` | `REVISE` | null) and `detail.newest_current` (the newest spec review judged the current spec; false when there is none). It is written after `worktree_created` / `worktree_reused` (Breaking change (RJ-1): there is no `auto_pick` after it any more).
- additive (new key, SR-1): the `task_created` and `spec_updated` events' `detail.spec_sha256`, the sha256 of the spec bytes stored.
- additive (new keys, FC-2): a run's final `status_change` (the one the runner writes when it settles the run) has `detail.retries` (0 to 2: how many `run_retry` events the run wrote), `detail.retry_stop` (null when the last launch was not a transient failure; `exhausted` when it still was after two retries; `timeout` when the wait would have reached the run's `--timeout`; `cancelled` when a cancel arrived during the wait; `launch_error` when the relaunch could not be started) and `detail.retry_error` (the reason for `launch_error`, else null).
- additive (new key, SR-2): a run's final `status_change` has `detail.spec_review_verdict`: the spec review verdict (`APPROVE` | `REVISE`) of a spec reviewer run that reached a valid one, null for every other run. A spec reviewer run's `detail.task_status_written` is always null.
- additive (new keys, LESS-5): a `check_approved` event (no `task_id`; read it with `--since` and no `--task`) has `detail.approved_by` (`human_terminal` | `commander_cli` | `commander_mcp`), `detail.reason` (string or null) and `detail.live_runs` (the ids of every run of the repo that had not ended when it was written, sorted). The other keys keep their meaning. An event written before LESS-5 has none of the three.
- additive (new event, RD-1): a `secret_redacted` event, written when an exit that hands content over once masked a secret (see Secret masking under Conventions), with the task's `task_id`. `detail` has exactly three keys: `exit` (where the content went: `evidence_show`, written by `evidence show` with the reader's run as `run_id` or null; `review_packet`, written by `review packet` with no `run_id`; `review_input`, written by the runner with the review run's `run_id` when it filled a codex or antigravity reviewer's `.agent/review-input/`; `prompt`, written by the runner with the run's `run_id` when it masked the prompt of a worker or reviewer, its `kinds` counting every value masked while the prompt was built, which may include fields the harness's prompt does not carry; `spec_review_input`, written by the runner with the spec reviewer's `run_id` when it masked `spec.json`, `proposed.json` or `edits.json`; `handoff`, written by `dispatch` with the new run's `run_id` right after `handoff_written` in the same transaction when it masked `.agent/handoff.md`), `kinds` (an object of kind to count, keys in name order) and `evidence` (the evidence codes involved, `[]` when none). It never carries the masked content, its length or its place. A research task's source URL that holds a secret is masked in the reviewer's prompt like any other value: the reviewer copies back the masked URL, that source check is `review_invalid`, and the worker has to use another source.
- additive (new event, LESS-5): a `protected_change_approved` event, written by `checks approve --run RUN` with RUN's `task_id` and `run_id` when it approved protected changes of RUN: `detail.run_id`, `detail.diff_code` (the diff evidence the changes were read from), `detail.changes` (`[{"path","status","kind"}]`, the `test` and `check_word` changes approved now), `detail.approved_by`, `detail.reason` and `detail.live_runs` (as in `check_approved`).

A run's `runner-error.log`, when present, is in its `run_dir`.

### `thoth records export [--json]`
New (DT-3). Writes `records-dir/NAME.jsonl` (NAME is the `machine-name` setting): one line per run of every repository registered on this computer, regenerated whole (see docs/phase2/usage.md, "Shared records folder (DT-3)"). Relied on: `ok`, `path`, `machine`, `lines`, `repos`. Exit `1`, nothing written, when `records-dir` is not set (stderr starts `records-dir is not set`), when it is not a directory (`records-dir PATH is not a directory`) and when `THOTH_RUN_ID` is set (`records export is only for the human operator or the commander`). It writes only `records-dir/NAME.jsonl`. The runner writes the same file after each run ends and `review record` after it settles the task; a failure there never changes the run or the review (the runner logs it in `runner-error.log`, `review record` prints a `warning:` line on stderr and its stdout is unchanged). Additive (RD-3): every line is masked before it is written (see Secret masking above); a line with nothing to mask is byte for byte what it was, and no `secret_redacted` is written.

Breaking change (RJ-3): the line format is version 2 (`v` is 2). It no longer has `recommended`, `deviates` or `exempt`, and adds no key; `reason` is why the profile, model and effort were chosen (the `reason` of the run's newest `launch_choice`, null without one). Reading the folder (`thoth models`, `recommend`, the report page) accepts lines of version 1 and 2, ignoring a v1 line's three old keys; any other `v` counts in `skipped_lines`.

### `thoth review record TASK -f REVIEW.json --json`

Additive (RES-1b): for research tasks the document checks sources instead of criteria:

```json
{"verdict":"PASS","findings":{"sources":[{"id":"S1","url":"https://example.org/page","verdict":"supported","reason":""}],"extra_findings":[]}}
```

Ids come from the latest completed implementer run's latest result, research
claims first in their result order then each claim's source order (`S1`, `S2`, …).
Both the expected list and reported sources must be nonempty. Every id must
appear exactly once, with no unknown ids; URLs must match after trimming both
ends. Source verdicts are `supported`, `not_supported`, `unreachable`; the last
two need nonblank reasons. Nonempty criteria are refused. Extra finding labels
use the usual normalization and only their evidence codes are checked.
Any not_supported means REJECT, else any unreachable means UNVERIFIED, else
PASS; a different declared verdict is refused at `consistency`. Other shape
errors use `schema`, id coverage errors `coverage`. The transaction refuses
without writes if its target differs from the target used for validation.

For a research PASS, any candidate evidence preparation happens inside that
transaction after its target check; ordinary review preparation remains
before the transaction as described below.

Settles an `in_review` task with a review made outside thoth. Relied on: `verdict`, `task_status` (`pass` | `reject` | `unverified`), `review_id`. The review file:

```json
{
  "verdict": "PASS | REJECT | UNVERIFIED",
  "findings": {
    "criteria": [
      {"id": "AC1", "verdict": "met", "evidence": ["E008"]},
      {"id": "AC2", "verdict": "unmet", "evidence": ["E005"], "note": "why", "fix_size": "S", "category": "spec_mismatch"}
    ],
    "extra_findings": [
      {"severity": "important", "note": "what", "evidence": ["E008"], "fix_size": "M", "category": "error_path"}
    ]
  }
}
```

Rules:
- every AC id appears exactly once;
- `met` cites at least one code;
- `unmet` cites a code or has a `note`;
- every cited code is evidence of this task;
- `PASS` with any `unmet` is invalid;
- `fix_size`, when present, is `S`, `M` or `L`, and `category`, when present, is one of `spec_mismatch`, `spec_error`, `test_missing`, `test_interface`, `error_path`, `concurrency`, `platform`, `docs`, `evidence`, `files`, `security`, `other`; both are case-insensitive, a blank value or `none` counts as absent, and any other value is invalid at the `schema` stage.

Additive (H7a): `fix_size` and `category` on an unmet criterion or an extra finding are optional, compatible additions: a file without them is accepted and stored as before. When present they are stored normalised (`S`/`M`/`L`, lower-case category) and `review show --json` returns them in `findings`. `thoth retro` counts them.

An invalid file is refused with exit `1`, prints the failing stage and reason, writes one `review_invalid` event, and leaves the task in `in_review`. The command also refuses, writing nothing, when:
- the task is not `in_review`;
- a run is live;
- there is no completed implementer run with a candidate commit;
- it is invoked from inside a thoth run (`THOTH_RUN_ID` set);
- additive (LESS-5), for a `PASS` only: that run changed a protected `test` or `check_word` file that no `checks approve --run RUN` approved for that run (`refusing to record a PASS for task TASK: run RUN changed protected files that nobody has approved yet: PATH (STATUS, KIND), …; read the change (thoth review packet TASK), then approve it with: thoth checks approve --run RUN --reason "WHY" (MCP: thoth_checks_approve)`); or the run has no diff evidence for its candidate and none could be recorded (`… run RUN has no diff evidence for its candidate, so its protected file changes cannot be checked`); or its newest diff evidence has no readable `protected_changes` and recording it again failed (`… run RUN's diff evidence CODE has no readable protected_changes, and recording it again failed; ERROR`). Before the write transaction a `PASS` records the candidate's diff and commit evidence when the run has none, and the diff again when the newest one is unreadable; a failure there is a `warning: ` line on stderr. `REJECT` and `UNVERIFIED` are not affected.

Additive (WT-1): after a `PASS` it removes the task's clean worker worktrees (not when run from inside the worktrees directory). The `--json` keys do not change; the human output gets one more line per directory, `  removed worktree PATH` or `  kept worktree PATH: REASON`, and a failure of the cleanup is one `warning: worktree cleanup failed: …` line on stderr.

### `thoth review show TASK --json`

Read-only: returns the newest review and `review_count` (zero with null review
fields when none exists). Additive (RES-1b): source checks have
`findings.sources: [{"id","url","verdict","reason"}]`, omitted in ordinary
reviews. Each `claims[]` item also has `sources: [{"url","quote"}]`, or `[]`.
Sources are matched in occurrence order by (kind, text) against that review's
target run's latest result; an unmatched claim gets `[]`. Human output replaces
the criteria table with `  S1  supported  URL  REASON` lines for source checks.

### `thoth spec review record TASK -f SPEC-REVIEW.json --json`
New (SR-1). Records a review of the task's current spec made outside thoth (a subagent or a human), before its first dispatch. Relied on: `verdict`, `approved`, `required_before_dispatch`, `revise_streak`, `stalled`, `spec_review_id`. The `--json` keys are exactly `task_id`, `spec_review_id`, `verdict`, `spec_sha256`, `risk`, `task_status`, `approved`, `required_before_dispatch`, `revise_streak`, `stalled`, `source` (`external`). The file:

```json
{
  "verdict": "APPROVE | REVISE",
  "criteria": [
    {"id": "AC1", "verdict": "revise", "findings": [
      {"check": "exists", "location": "src/ledger.rs:40", "note": "what to change"}
    ]},
    {"id": "AC2", "verdict": "ok", "findings": []}
  ]
}
```

`check` is one of `verifiable` (every acceptance criterion can be verified), `exists` (the files, functions, commands and tests the spec names exist in the current code), `consistent` (the criteria do not contradict each other) and `steps` ('how to test' is written as step-by-step actions); `location` is `file:line`, or `spec` for the spec text itself.

Rules:
- every AC id appears exactly once;
- `revise` needs at least one finding;
- every finding has a known check, a location and a note;
- `APPROVE` with any `revise` criterion is invalid;
- `REVISE` with no `revise` criterion is invalid.

An invalid file is refused with exit `1`, prints `spec-review.json invalid at stage STAGE: REASON`, writes one `spec_review_invalid` event and nothing else. The command also refuses, writing nothing, when:
- it is invoked from inside a thoth run (`THOTH_RUN_ID` set);
- the task is `running`, `pass`, `cancelled` or `accepted` (`accepted` is additive, DOC-1);
- the task's spec changed between the read the file was validated against and the write (`task TASK's spec changed while the spec review was being recorded (sha256 OLD -> NEW); review the new spec and record again`).

A valid file writes one `spec_review` event; the task's status does not change. The approval is bound to the spec's sha256: after `task update-spec` it no longer applies. Additive (SR-3): the event's `detail.mode` is `external`, its `edits` `[]`, `edit_count` 0, `proposed_spec` and `proposed_sha256` null, `bytes_before` and `bytes_after` the spec's size; the file never carries `edits` (an `edits` key is ignored). The `--json` keys do not change.

### `thoth spec review show TASK [--proposed-out FILE] --json`
New (SR-1). Read-only. Relied on: `approved` (the newest spec review is an APPROVE of the current spec), `required_before_dispatch` (the task is `important` / `high` and has no implementer run yet), `stalled` (3 REVISE in a row; dispatch is stopped), `newest` (null, or an object with `spec_review_id`, `verdict`, `spec_sha256`, `current`, `created_at`, `seq`, `source`, `reviewer_run_id`, `document`), `spec_sha256` (the current spec's). The keys are the same whether or not a spec review exists: `task_id`, `risk`, `task_status`, `spec_sha256`, `review_count`, `required_before_dispatch`, `approved`, `revise_streak`, `stalled`, `newest`, and (additive, SR-3) `pending_edits`.

Additive (SR-3): `pending_edits` (boolean) is true when the newest spec review is a `mode` `edit` REVISE with `edit_count` at least 1 whose `spec_sha256` is the current spec's: `spec review confirm` can run. `newest` gains `mode` (`external` | `review` | `edit` | `confirm`; an event written before SR-3 reads as `external` or `review` by its `source`), `edit_count` (0 for such an event) and `proposed_sha256` (null for such an event). `source` may now also be `author_check`. The human output lists, after the criteria, one line per proposed edit (`edit INDEX  FIELD[OP AT]  CRITERION  REASON  AFTER` with the first 60 characters of `after`, `(deleted)` for a delete), and on an author's check the author's `verdict` and note after each; the criteria lines of an author's check come from the event's `criteria` summary (id, verdict, number of findings; the author's document has no criteria), so they are there on an APPROVE and a REVISE alike. `--proposed-out FILE` (CLI only; the MCP tool has no such parameter and stays read-only) also writes the newest spec review's `proposed_spec` to FILE, byte for byte, with `wrote FILE (N bytes)` on stderr, when that review has one (`proposed_spec` not null) and its `spec_sha256` is the current spec's; this is wider than `pending_edits`, so after the author's REVISE the commander can still take the proposed spec and edit it by hand. Otherwise it exits `1`, writes no file and says the task `has no proposed spec to take` and why (no spec review; the newest proposes no edits; the newest judged an older spec).

### `thoth spec review start TASK --profile ID [--model MODEL] [--effort LEVEL] [--reason TEXT] [--timeout 30m] [--edit] --json`
New (SR-2). Launches Thoth's own read-only spec reviewer, a run with `role` `spec_reviewer`, against the task's current spec, and returns at once. Relied on: exit code, `run_id`, `spec_sha256`, `reviewed_commit`, `required_before_dispatch`. The `--json` keys are exactly `ok`, `run_id`, `task_id`, `spec_sha256`, `reviewed_commit`, `task_base_commit`, `risk`, `task_status`, `required_before_dispatch`, `profile`, `model`, `worktree`, `worktree_reused`, `worktree_moved`, `cleared_stale_report`, `allowed_tools`, `runner_pid`, `run_dir`, `effort_requested`, `effort`, `effort_note`, `warnings` (Breaking change (RJ-1): `auto`, `auto_reasons` and `live_quota_refresh` are gone). These refusals exit `1` and write no run row and no event (the worktree may have been created): `THOTH_RUN_ID` set; `auto` in `--profile`, `--model` or `--effort` (RJ-1, right after the `THOTH_RUN_ID` check); a missing `--model`, or a missing `--effort` for a model that has an effort setting (RJ-1: a spec review continues no attempt, so they are given every time; Breaking change (AGY-1): an agy model whose name ends in its level may leave `--effort` out, and then `effort` is that level, `effort_requested` null and `effort_note` says it is carried by the model variant; any other agy model needs `--effort` and its launch gets `--effort LEVEL`, see `dispatch`); no `--reason` while `require-reason` is on; the task `running`, `pass`, `cancelled` or `accepted` (DOC-1); a live run of the task; the spec changed between the read and the write; a bad `--effort`; an unknown profile or a model its harness refuses; a profile that is not logged in. What the run does: in `worktrees/TASK_ID-spec-review`, a detached checkout (BASE-1) of the HEAD of the worktree the command runs in when that HEAD contains the task's base commit, otherwise of the task's base commit (`reviewed_commit`; moved there on every start, reused across starts; a git failure deciding this exits `1` before the worktree and the run row), it reads `.agent/spec-review-input/spec.json` (the stored spec bytes when the spec holds no secret; when it holds one, spec.json is the masked and re-serialized spec, and `spec_sha256` is still the stored spec's) and answers with the spec-review document of `spec review record` as `.agent/spec-review.json` (codex: read-only sandbox, `--output-schema`, `-o`; antigravity: `--json-schema`, `structured_output`; Claude Code: a shell redirect). Breaking change (RJ-1): the profile, model and effort are the ones given; Thoth picks none of them. Use a profile of the provider that did not write the spec. What it records: `spec_review_started` and `launch_choice` with the run row; at the end one `spec_review` event (`source` `reviewer_run`) for a valid report or one `spec_review_invalid` event for a refused one. The task's status never changes, whatever the run's outcome; `resume` refuses the run, `cancel` leaves the task where it was. `thoth wait --run RUN --json` returns the verdict in `verdict`. Breaking change (LESS-3): after the live-run check it lints the stored spec as `task create` does; a lint error exits `1` with the same `spec lint: …` error, before any worktree, run row or event, and each lint warning is one more `spec lint: …` line at the end of `warnings`.

Additive (SR-3), edit mode: `--edit` (MCP: `edit`, boolean) lets the reviewer propose edits to the spec besides judging it; without the flag nothing changes. The refusals, their order, the worktree rule, the live-run and sha checks and `launch_choice` are those of `start`. `--json` gains `mode` in both cases (`edit` | `review`), and `spec_review_started.detail.mode` says the same; the runner picks the prompt and the output schema from it. The prompt adds a section "You may edit the spec" with the edit rules below; the schema handed to codex and agy (`spec-review-schema.json` in the run dir) gains `edits` (strict: every property required, no extra keys; a property that does not apply holds `""` or `0`). The reviewer is still read-only: Claude's allowed tools, codex's read-only sandbox and agy's tool list are unchanged; the edits are applied by Thoth, never by the run.

The edit document is the spec review document plus `edits` (array, always present, may be empty; a document without it, or with `null`, is stage `schema`). Each edit has exactly the seven properties below, as the schema says: one left out is not defaulted and one more is refused, both stage `schema` (`edits array is malformed: …`). Each edit: `field` (`goal` | `acceptance_criteria` | `constraints` | `non_goals` | `excluded_alternatives`; anything else is refused), `op` (`replace` | `insert` | `delete`; `goal` and `acceptance_criteria` allow only `replace`, and after the edits `review_json::ac_ids` of the proposed spec must equal the task's), `at` (from 0; `replace` / `delete` below the list's length at that point, `insert` up to and including it (append); ignored for `goal`), `anchor` (`replace` / `delete`: the first 10 characters of the entry at `at`, trimmed, or the whole entry when it is shorter; the 10 are counted after the anchor itself is trimmed, so when the entry's tenth character is a space the anchor goes on to the next non-space one; compared with `starts_with` against that one entry only (the goal for `goal`), never searched; not checked for `insert`), `text` (`replace` / `insert`: the whole new entry, not blank; not checked for `delete`), `criterion` (one of the task's criterion ids) and `reason` (not blank). A list field the stored spec lacks is an empty list. The edits apply in order, each `at` counting the earlier ones. An APPROVE must have no edits; a REVISE may have none or many, and with edits every criterion may be `ok` (`says REVISE but marks no criterion revise` is raised only when `edits` is empty too); the other criteria rules are unchanged. A review-mode run and `spec review record` parse the document as before and never read `edits`.

The runner's ingest, after the stages `started`, `schema`, `spec`, `coverage` and `spec_changed`: an edit that breaks a rule is `spec_review_invalid` with `detail.stage` `edits` and a reason that names the edit's index, its field and why (`edit 0 (risk): field must be one of …`); the proposed spec is linted with `spec_lint` (the rules of `task create`), an error (`too_large` over 40960 bytes included) is stage `lint`, warnings do not block; neither writes a `spec_review` event. A valid edit document writes one `spec_review` REVISE (`spec_sha256` the reviewed spec's, `source` `reviewer_run`, the streak as before) whose `detail` carries the keys under `events` below (`mode` `edit`, the completed `edits`, `edit_count`, `proposed_spec`, `proposed_sha256`, `bytes_before`, `bytes_after`). The proposed spec is serialized from the stored spec read as JSON (unknown keys keep their values): two-space indent, LF, one trailing newline, keys in the order `goal`, `acceptance_criteria`, `constraints`, `non_goals`, `excluded_alternatives`, `task_type`, `difficulty`, `checks`, `risk`, then every other key alphabetically.

### `thoth spec review confirm TASK --profile ID --model MODEL [--effort LEVEL] [--reason TEXT] [--timeout 30m] --json`
New (SR-3). Launches the author's check of the newest edit review's edits: a run with `role` `spec_reviewer` (so `wait --run`, `cancel`, `status` and the worktree kinds treat it as `start`'s run), `mode` `confirm`, in the same `worktrees/TASK_ID-spec-review` (the same `reviewed_commit` rule). Relied on: exit code, `run_id`, `mode`, `confirms_spec_review_id`, `proposed_sha256`. The profile, model and effort are given (RJ-1; use the profile and model that wrote the spec); `--timeout` is `start`'s. The `--json` keys are `start`'s (`mode` `confirm`) plus `confirms_spec_review_id` (the edit review's id) and `proposed_sha256` (its proposed spec's). MCP: `thoth_spec_review_confirm` (`task`, `profile`, `model?`, `timeout?`, `effort?`, `reason?`, `repo?`).

It refuses with exit `1`, no run row and no event: everything `start` refuses, in `start`'s order, except that the task's status must be one `update-spec` allows (`draft`, `queued`, `reject`, `unverified`: `task TASK is 'STATUS'; spec review confirm needs draft, queued, reject or unverified, because its APPROVE replaces the spec`); and, after the live-run check, when `pending_edits` is false: `task TASK has no spec review; …` (none recorded), `task TASK's newest spec review ID is not an edit review with edits (mode M, VERDICT, N edits); …` (a REVISE of another mode, an APPROVE, or no edits), or `task TASK's spec changed after spec review ID (sha256 OLD -> NEW); the edits were proposed for the old spec, run `thoth spec review start TASK --edit ...` again` (`update-spec` ran after the review). The lint runs on the proposed spec, not the stored one. In the write transaction it also checks that the newest spec review is still that edit review.

What it records: `spec_review_started` (`detail.mode` `confirm`, plus `confirms_spec_review_id` and `proposed_sha256`) and `launch_choice` (`detail.source` `spec_review_confirm`). Before the launch the runner checks the spec's sha256 and that the newest spec review is still `confirms_spec_review_id` (otherwise it refuses to launch, as for a spec changed between `start` and the launch), then writes `.agent/spec-review-input/spec.json` (the current spec), `proposed.json` (the proposed spec's bytes) and `edits.json` (the completed edit list of the event); when they hold a secret, `spec.json`, `proposed.json` and `edits.json` are all masked (see Secret masking under Conventions). The prompt tells the author to judge only the edits (against the code the edit names in the checkout, against the rest of the spec, and whether the original intent is kept), not to review the whole spec again and not to add findings; it is read-only; the spec and the edits are only in the files. The author's document, delivered like `start`'s (same schema file, strict, `note` required and `""` allowed on `ok`): `{"verdict":"APPROVE"|"REVISE","edits":[{"index":0,"verdict":"ok"|"revise","note":"…"}]}`. The runner's ingest (`spec_review_invalid` with `detail.source` `author_check`): a wrong shape (the document is exactly `verdict` and `edits`, each answer exactly `index`, `verdict` and `note`; an answer without `index` or `note` is not defaulted, a key more is refused: `does not have the author's shape …`), a `revise` without a note, APPROVE with a `revise` or REVISE without one are stage `schema`; an index missing, unknown or repeated is stage `coverage`; then, inside an `IMMEDIATE` transaction that re-reads the task and its events, the spec's sha256 still the one at start (else stage `spec_changed`) and the newest spec review still `confirms_spec_review_id` (else stage `superseded`). APPROVE: the proposed spec is linted again first (error: stage `lint`); in the transaction `set_task_spec` replaces the spec (allowed from `update-spec`'s statuses; 0 rows is stage `status`, the transaction rolls back and only the invalid event remains), one `spec_updated` is written (the keys of `task update-spec`, plus `source` `author_check`, `spec_review_id` (the new APPROVE's) and `confirms_spec_review_id`; its `run_id` is the confirm run), and one `spec_review` APPROVE (`spec_sha256` the new spec's, `source` `author_check`, `mode` `confirm`, `revise_streak` 0, `stalled` false, `criteria` every criterion `ok` with 0 findings, `document` the author's, plus `confirms_spec_review_id`, `spec_sha256_before`, `applied` true, and the edit review's `edits`, `edit_count`, `proposed_spec`, `proposed_sha256`, `bytes_before`, `bytes_after`). REVISE: one `spec_review` REVISE only (`spec_sha256` the old spec's, `source` `author_check`, `mode` `confirm`, `applied` false, the streak +1; `criteria`: the criterion a refused edit serves is `revise` with one finding per refused edit, the others `ok`; the same extra keys), and the spec stays. `tasks.status` is never written. `thoth wait --run RUN --json` returns either verdict in `verdict`. The streak counts an edit review's REVISE and the author's REVISE alike; any APPROVE ends it; the limit of 3 and `--skip-spec-review` are unchanged.

### `thoth spec lint FILE [--json]`
New (LESS-3), additive. Read-only: checks a spec file with the fixed rules `task create`, `task update-spec` and `spec review start` apply first; it writes nothing and works in an initialized repository. `--json` → `{"ok","bytes","errors":[{"rule","location","message"}],"warnings":[same shape]}`; `ok` is true when `errors` is empty, `bytes` is the file's size, and `location` is `goal`, `acceptance_criteria[i]`, `constraints[i]` or `checks[i]` (from 0), or null for the size rules and `invalid_spec`. Exit `1` when there is an error, `0` otherwise; stdout is the whole report either way. Without `--json` each finding is one line, `error: RULE (LOCATION): MESSAGE` or `warning: …` (no parentheses without a location), then `N error(s), M warning(s)`; with nothing found the only line is `no problems found`. A file that cannot be read fails as in `task create`. Only `goal`, `acceptance_criteria` and `constraints` are scanned (`non_goals` and `excluded_alternatives` often name what is not done). Errors:
- `too_large`: the file is over 40960 bytes; split the task.
- `invalid_spec`: the spec does not parse; the message is the parse error, and only the size rules run besides.
- `ac_blank`: an acceptance criterion is blank once trimmed.
- `ac_duplicate`: an acceptance criterion repeats an earlier one once trimmed and with runs of spaces collapsed; the message names the earlier one.
- `unknown_check`: a `checks` id is not in the repository's catalogue (approved or not). Not reported while the catalogue is empty.
- `claude_write`: a sentence of a field names a `.claude` folder (`~/.claude/x`, `.claude/skills/y`; not `.claude-plugin`, `.claude.json` or `CLAUDE.md`) and, outside backticks, has a write word (such as 改, 新增, 刪, edit, update, delete) and no negation (such as 不, 沒, not, never, without). One per field.

Warnings:
- `large`: the file is over 20480 bytes (and at most 40960).
- `missing_path`: a repository path (letters, digits and `_.-/`, with a `/`, whose first part exists) is not in the main work tree; a path ending in `/` must be a folder. Not reported for a path with `new`, `create` or `新` in the 12 characters before it anywhere in the spec, nor for `/x`, `./x`, `../x`, `$X/…`, `~/…` or URLs.
- `missing_name`: an existing file followed by `的` (or `裡的`, `裡`, `中的`) and backticked names separated by `、` `,` `，` `與` `和`, or by `:name`, where the file does not contain that name as a whole word.
- `line_number`: a file name with a code or text extension followed by `:` and a number (`a.rs:12`), a backticked name followed by `:` and a number, or `第 12 行`. One per field, with the first example and the count.
- `two_dot_diff`: within 100 characters after `diff` on the same line, a two-dot range `A..B` (`git diff master..HEAD`). One per field.
- `powershell_in_bash`: a backticked command with `$env:`, `$null`, `-ErrorAction` or a `Verb-Noun` cmdlet, in a field that never says PowerShell or pwsh. One per field.

### `thoth review packet TASK [--out FILE] [--json]`
Read-only: prints everything a reviewer needs in one document, so the commander does not assemble it from `evidence list`, `evidence show` and `events`. It writes no row and no event, except one secret_redacted event when it masked something (it does **not** record `evidence_opened`), and works at any task status. Additive (RD-1): every output form is masked (see Secret masking under Conventions); the `secret_redacted` event has exit `review_packet`, no `run_id`, and `evidence` the codes of the rows whose label, output or diff had a secret, in code order. If that write fails, the output is unchanged, the exit code is `0` and stderr gets one `warning: could not record secret_redacted: REASON` line. The worker's own `summary` is left out on purpose. Relied on: exit code (`1` and an error naming the id for an unknown task) and, with `--json`, these top-level keys:
- `task_id`, `title`, `status`, `risk`;
- `runs[]` (additive): `run_id`, `role`, `attempt`, `models_used`, `subagents_spawned`, as in `status`; additive (S3-7) `effort`, the level actually sent (null when none). Each Markdown run line has ` | effort: LEVEL`, or ` | effort: -` when there is none, and ends there unless the run continued a stopped run (S3-9 handoff), in which case ` | continued from PREVIOUS_RUN (REASON)` follows it. Additive (S3-9): `continued_from`, the id of the run it continued, or null. The Markdown packet lists them under `## Runs` and adds a line starting `WARNING:` for any run that spawned subagents;
- `spec`: `goal`, `acceptance_criteria`, `constraints`, `non_goals`, `excluded_alternatives`, verbatim;
- `evidence[]`: `code`, `kind`, `exit_code`, `run_id`, `label`, `output_tail` (the last 15 lines of the stored output for `command` rows, null otherwise; RD-1: the whole output is masked first and the last 15 lines are taken from the masked text);
- `protected_changes[]` (additive, P7-2): `run_id`, `code`, `path`, `status`, `kind` — one entry per protected file each `diff` row changed (`code` and `run_id` are that diff's), `[]` when none. `status` is `modified` | `deleted` | `type_changed` | `added`; `kind` is `test` | `check_input` | `check_word`. The Markdown packet lists them under `## Protected files changed`, placed before `## Diffs`, as `path (status, kind)`, followed by the text of each `protected_change` claim; an empty list reads `(none)`;
- `diffs[]`: `code`, `run_id`, `blob_verified`, `content` (the whole diff, never truncated);
- `claims[]` (`kind`, `text`, `evidence`) from the newest `result_ingested` event, and `claims_run_id` (null when there is none). Each entry is the claim as the worker wrote it, so a `research` claim (RES-1a) carries its `sources` (`[{"url","quote"}]`) and a claim written without the key has none; the Markdown packet adds one indented `  - URL — "QUOTE"` line per source under the claim's line;
- `approvals[]` (additive, LESS-5), by event `seq`, oldest first: every `check_approved` event after the task's `task_created` event (from the main tree or any run) and every `protected_change_approved` event of this task. Each has `seq`, `ts`, `type` (`check_approved` | `protected_change_approved`), `approved_by` and `reason` (null for an event written before LESS-5), `run_id`, `check_id` and `source` (null for a protected one), `changes` (`[{"path","status","kind"}]`; `[]` for a check one) and `live_runs` (`[]` for an older event). The Markdown packet lists them under `## Approvals during this task`, after `## Worker claims` and before `## Reviews recorded so far`; an empty list reads `- (none)`;
- `reviews[]`, oldest first: `verdict`, `external` (true when there was no reviewer run), `findings`.

Without `--json` the same content is Markdown, in that order. With `--out FILE` the exact bytes that would have gone to stdout are written to FILE (UTF-8, no BOM), stdout stays empty, and stderr gets `wrote FILE (N bytes)`. There is no `summary` key anywhere.

### `thoth resume RUN --json`
Continues an `interrupted` implementer run in its own session. The runner has already retried a transient failure twice inside the run before it is `interrupted` / `transient`; `resume` tries again by hand, and the resumed run again retries a transient failure up to twice. Relied on: exit code and `run_id`. An antigravity run is resumed with `--conversation <harness_session_id>`, the conversation id recorded when agy's `init` arrived; a run that failed before `init` (an unknown model) has none and is refused. Additive (AGY-1): the resumed launch reuses the run's stored `launch` settings, so an agy run gets the same `--effort LEVEL` again (or, for a model name that carries its level, none again); nothing is chosen anew. Additive (MODELS-1): at its model-rule step it also applies the agy list rule (the stored list only) and the Claude unavailable-model rule of `dispatch`, refusing before anything is written; with no stored agy list `warnings` gains the `was not checked` line. Additive refusal (S3-9): once a later implementer run of the same attempt has started (a handoff, see `dispatch`), resuming the earlier run exits `1` with a message naming the later run and containing `continued`; dispatch the task again instead. Breaking change (RJ-1): the runner no longer hands a run over on quota, so the only later run is one the commander dispatched. A run of a `pass`, `cancelled` or `accepted` task is refused with exit `1` (`resume needs a task that is not pass, cancelled or accepted`; `accepted` is additive, DOC-1).

### `thoth cancel RUN --json`
Stops a `queued` / `running` run. Relied on: exit code. When it has to kill the worker itself, it first checks the pid's image against the run's recorded `worker_image`, falling back to the stem of `runs.harness_bin` for runs without one. A cancel that arrives while the runner is waiting to retry a transient failure ends the run `cancelled` at once; there is no worker to kill and nothing is relaunched. **Final for the task:** a cancelled run leaves the task `cancelled`, and a `cancelled` task can never be dispatched again, so a retry needs a new `task create`. In the rare case where the runner had already died and the worker had to be killed, the task becomes `failed` instead, which `dispatch` does accept. A run that simply takes too long stops itself at its `dispatch --timeout` (default `30m`): the run and the task both become `failed`, with `interrupted_reason` `timeout`. `resume` does not accept a `failed` run; re-dispatch with `dispatch`, which writes a handoff file (see `dispatch`). A run stopped by quota is already `interrupted`, so cancelling it exits `1`; Breaking change (RJ-1): no handover run follows it any more.

### `thoth task update-spec TASK -f SPEC.json --json`
Allowed from `draft`, `queued`, `reject` and `unverified` (`queued` is additive, SR-1: a spec review that comes back REVISE finds the task still queued). Relied on: exit code. Note: the `status` key in its output is the status **before** the update. Additive (SR-1): the `spec_updated` event's `detail` carries `spec_sha256` (the new spec's), and when the newest spec review was an APPROVE of the old spec, `warnings` gains `spec review ID (APPROVE) no longer applies: the spec changed (sha256 OLD -> NEW); review the new spec again before its first dispatch`. Breaking change (LESS-3): the new spec is linted as in `task create`; a lint error exits `1` with the same `spec lint: …` error and leaves the spec and the events unchanged, and each lint warning is one more `spec lint: …` line at the end of `warnings`. Additive (SR-3): an author's APPROVE (`spec review confirm`) replaces the spec the same way, under the same statuses, and writes a `spec_updated` event too; so it counts as a spec change after a REJECT, and a rejected `important` / `high` task whose spec was replaced that way is dispatched again without the "spec unchanged since the REJECT" refusal. An `accepted` task (DOC-1) is refused like `pass` and `cancelled`.

Additive refusal (RD-2): as in `task create`, before spec lint a spec in which any line or any string value holds what looks like a secret exits `1` with the error starting `refusing: the spec contains what looks like a secret` (kinds, field paths and line numbers, never the value); the spec and the events are unchanged. (RD-4) As there, an object key that holds a secret is refused too, whether or not it is written with `\u` escapes.

### `thoth task close TASK --reason TEXT --json`
Abandons a task: `queued`, `failed`, `interrupted`, `unverified` or `reject` → `cancelled` (`queued` is additive, SR-1: a task stopped by three spec review REVISE in a row is still `queued`), with one `task_closed` event (`detail.from`, `detail.reason`). Relied on: exit code, `status` (`cancelled`), `previous_status`. It exits `1` and writes nothing from any other status, while a run of the task is `queued` / `running`, or with an empty `--reason`. Changed (WT-1): it then removes the clean worktrees of this and every other `pass` / `cancelled` task (the same sweep as `worktrees prune`, below); the branches stay. Additive key of `--json`: `worktrees`, `{"removed":[{"task_id","path","kind","head","branch","task_status"}],"kept":[{"task_id","path","kind","reason","detail","task_status"}]}`; when the sweep failed, or was skipped because the current directory is inside the worktrees directory, it is `{"removed":[],"kept":[],"error":"…"}` and the command still exits `0` with `ok` true (the task is closed). The key is absent when `THOTH_RUN_ID` is set, where no sweep runs.

Additive (DOC-1): a docs, research or review task (spec `task_type` `docs`, `research` or `review`) is also closed from `in_review` (`detail.from` `in_review`), for a delivery that is not used; to take a delivery, use `task accept`. An `in_review` task of any other `task_type` (or none) is still refused, and the error adds that from `in_review` only a docs, research or review task can be closed (it contains `docs, research or review`). `accepted` is refused like `pass` and `cancelled`.

### `thoth task accept TASK --reason TEXT --json`
New (DOC-1). Takes the delivery of a document task: `in_review` → `accepted`, a final status (no `dispatch`, `task update-spec`, `task close`, `review start`, `review record`, `spec review start`, `spec review record` or `resume` afterwards). A document task is one whose spec `task_type` is `docs`, `research` or `review`; whether the spec lists `checks` does not matter. Nothing is written to the reviews table and no `verdict` event is written, so `thoth models` counts the run as completed but not reviewed (no `pass`, no `reject`) and its records line has `verdict` null. It writes one `task_accepted` event (`detail`: `from`, `reason`, `task_type`, and `run_id`, `result_status`, `candidate_commit` of the task's newest `result_ingested` event, each null when there is none) and the task's `status_change`. Relied on: exit code, `status` (`accepted`), `previous_status` (`in_review`), `run_id`. The `--json` keys are exactly `ok`, `task_id`, `previous_status`, `status`, `reason`, `run_id`, `result_status` (`completed` | `blocked` | null), `candidate_commit`, `worktrees`. Then it removes the clean worktrees of every finished task as `task close` does; `worktrees` has the same shape (it is always present: the command refuses when `THOTH_RUN_ID` is set). It exits `1` and writes nothing when `THOTH_RUN_ID` is set (checked first; the error names `THOTH_RUN_ID`), with an empty or blank `--reason`, when the task is not `in_review` (the error names the status), when it is a task of another `task_type` (the error says it is a code task and names `review start` and `review record`), or while a run of the task is `queued` / `running` (the live-run error of `dispatch`, starting `task accept: `). Human output: `TASK accepted (in_review -> accepted)`, then the worktree text of `task close`. MCP: `thoth_task_accept` (`task`, `reason`).

### `thoth task result TASK [--run RUN] --json`
New (DOC-1). Read-only (no event): the result.json the task's worker delivered, from the task's newest `result_ingested` event, or with `--run` that run's newest one (the run must belong to TASK). It reads the event, so it works at any status, after `task accept` or `task close` too. Relied on: exit code, `summary`, `claims`, `run_id`. The `--json` keys are exactly `ok`, `task_id`, `task_status`, `title`, `run_id`, `attempt`, `role`, `profile`, `model`, `candidate_commit`, `worktree`, `ingested_at` (the event's time), `result_status` (`completed` | `blocked`), `summary` (the worker's text in full, not cut), `claim_count`, `claims` (`[{"kind","text","evidence","sources"}]`, `evidence` the claim's codes as the worker wrote them, `[]` when none; `sources` additive, RES-1a: the claim's `[{"url","quote"}]` as written, `[]` when none). It exits `1` when there is no such event (the error names the task's status, says there is no ingested result.json and points to `thoth events --task TASK --json` for `result_invalid`), and when `--run` names a run of another task. Human output: the task id, status and title; the run id, attempt, role, profile, model and candidate commit; `summary:` and the text indented two spaces; `claims (N):` and one `- KIND: TEXT [CODES]` line per claim, each source of it on its own line below, indented four spaces: `URL  "QUOTE"`, the quote cut at 120 characters plus `...` (RES-1a). MCP: `thoth_task_result` (`task`, `run`), read-only.

### `thoth review start TASK --profile ID [--model MODEL] [--effort LEVEL] [--reason TEXT] --json` / `thoth verify RUN --json`

Additive (RES-1b): a research task's reviewer checks the sources in the target
run's latest accepted result. After the in_review and live-run checks, before
any evidence preparation or other write, no research claim is refused with
exit 1 and guidance to `task result`, `task close` and `dispatch`. Claude
`allowed_tools` is `Bash,Read,Glob,Grep,WebFetch,WebSearch`; agy is its reviewer
list below plus `search_web,read_url_content`; codex's stored string is unchanged
and its launch adds only `-c web_search="live"` after `-s read-only` (before
Windows sandbox config), never `network_access`. Codex and agy receive the
strict source-check schema. Research `web_search` items are expected.
`spec review start` always keeps web tools off, including research tasks.
The commander chooses the profile, model and effort: another provider first,
then a stronger same-provider model or higher effort only when quota is
unavailable, explaining the step in `--reason`. Reasons use the existing
`require-reason` switch; there is no extra same-provider rule for research.
The launch and verification contracts:
- `review start` accepts `--effort` with the same levels, downgrade rules and stored `launch` settings as `dispatch` (see above); `review start --json` carries the additive keys `effort_requested`, `effort` and `effort_note`, and the reviewer's `run_started` event has `detail.effort_requested` and `detail.effort`.
- Breaking change (RJ-1): `--model` and `--effort` work as for `dispatch` (see "Model and effort" above): `auto` is refused first; the first review of an attempt needs both (`--effort` can be left out only for a model without an effort setting, or (Breaking change, AGY-1) an agy model whose name ends in its level, which is then recorded as `effort` with no flag sent; any other agy model needs it and gets `--effort LEVEL` on its launch); starting the review of the same attempt again keeps the previous reviewer run's model and requested effort for what is left out, with the same harness check. There is no `opus` default any more. `--reason TEXT` works as for `dispatch` (see `--reason TEXT` and `require-reason` above), with `source` `review_start`. The order: the task's status, the live-run check, the run under review and its candidate, the previous reviewer run, the profile and its harness, the model and its model rule, a missing flag, `require-reason`, then the protected-change check, the login check and (MODELS-1) the model list check (the agy list and the Claude unavailable-model rule of `dispatch`, right after the login check stored the plan and the list, before the review worktree and the run row; a refusal leaves only the protected-change check's evidence behind, and with no agy list at all `warnings` gains the `was not checked` line). `spec review start` has the same step at the same place (after its login check, before its worktree and run row).
- Breaking change (RJ-1): `review start --json` no longer has `auto`, `auto_reasons` or `live_quota_refresh`, and no `auto_pick` event is written. A recovered `rate_limited` profile is set back to `active` as for `dispatch`.
- **Same-provider reviewer.** Breaking change (RJ-1): Thoth no longer raises a same-provider reviewer's model one tier above the implementer's, and `require-reason` has no `stronger` exemption. Breaking change (RJ-3): the commander chooses the reviewer, for code reviews and spec reviews at every risk, in this order: a profile of another provider that has quota; otherwise a stronger model of the same provider than the implementer's; otherwise the implementer's model at a higher effort (never the same model at the same effort), starting it at once instead of waiting for a profile to reset, with `--reason` saying which step it took. Thoth enforces none of this.
- additive (LESS-5): `review start` also refuses, with exit `1`, when the run under review changed a protected `test` or `check_word` file that no `checks approve --run RUN` approved for that run, with the same three messages as `review record` (above) starting `refusing to start a review of task TASK: `. It first records the candidate's diff and commit evidence when the run has none (their `evidence_added` events are the only thing a refusal leaves behind: no run row, no other event, no worktree), and the diff again when the newest one has no readable `protected_changes`. The refusal comes after the launch's own refusals (RJ-1: `auto`, a missing flag, `require-reason`), which write nothing. The warnings of that recording are in `warnings` of a review that starts.
- `review start` runs Thoth's own reviewer in addition to the commander's review. Its verdict settles the task (`in_review → pass | reject | unverified`), after which `review record` is refused because the task is no longer `in_review`. On a high-risk task, the commander therefore reviews without recording, then runs `review start` + `wait --run RUN --timeout 9m --json` (relied on: `verdict`, `task_status`).
- **A `codex` profile as the reviewer** (e.g. `--profile codex-a --model gpt-5.6-luna`, a cross-provider reviewer): the same model rule as `dispatch` — a Claude name exits non-zero with the codex models listed, and no run row is written (Breaking change (RJ-1): no `--model` on the first review is refused as missing; there is no `opus` default). Nothing else in the contract changes: the same `--json` keys (`allowed_tools` is recorded but does not apply to codex), the same `wait --run`, the same verdict rules and task statuses. The codex reviewer cannot run thoth; it reads `.agent/review-input/packet.md` and `.agent/review-input/evidence/<CODE>.txt` in the review worktree, and its schema-constrained final message becomes `.agent/review.json`. Its `review show` lists every cited code under `unopened_citation`, because it opens nothing through `evidence show`.
- **An `antigravity` profile as the reviewer** (e.g. `--profile agy-a --model gemini-3.8-flash-low`): the same model rule as `dispatch` — a Claude alias, an empty name or a codex model exits non-zero with the profile's stored list (or the `no model list stored` sentence) listed, and (MODELS-1) a model outside the list the login check just read (or the stored one) exits `1` after the login check with the same message as `dispatch`; in both cases no run row, worktree or event is written. Nothing else in the contract changes: the same `--json` keys, the same `wait --run`, the same verdict rules and task statuses. Its `allowed_tools` (in `review start --json` and `status TASK --json`) is `read_file,view_file,list_dir,find_by_name,grep_search,finish,wait,wait_5_seconds` (Breaking change, AGY-1: agy 1.4.4's `read_file` beside `view_file`; it used to start at `view_file`; a spec reviewer's `allowed_tools` is the same string, and a research source checker's adds `,search_web,read_url_content`): no shell and no write tool, so it cannot run thoth, tests or git. It reads `.agent/review-input/packet.md` and `.agent/review-input/evidence/<CODE>.txt` in the review worktree, is launched with `--json-schema` (and, AGY-1, `--effort LEVEL` after the model when the model name does not carry its level), and the result line's `structured_output` becomes `.agent/review.json` (written by the runner; a result without it is `review_invalid` stage `read`, task `unverified`). Its `review show` lists every cited code under `unopened_citation`. Every tool call its lock denied is a `tool_denied` event on the reviewer run, counted in that run's `run_usage` `tools_denied` and, when the name is one Thoth does not know, listed in its `tools_unknown` (AGY-1).
- **Breaking change (2026-09-30, P7-1b): `verify` no longer runs the commands the worker declared.** It runs the repo's approved checks (`thoth checks`): the ids the task's own spec lists under `checks`, in spec order, or every check in the catalogue, in `check_id` order, when the spec lists none. Each runs with its own shell (`argv` anywhere; `cmd`, `powershell` on Windows; `sh`, `bash` on Linux) after its fingerprint is recomputed on the tree it runs in and found equal to any one of the check's approvals, or different from one only in package information (LESS-1, below). `fingerprint_ok` is `true` in both cases; for `package_info_only` it means "differs from that approval only in the allowed package-information lines", and the evidence detail's `fingerprint` and `approved_at` are those of the approval that matched. A check that is not in the catalogue, not approved, whose `cwd` is unsafe or missing, whose shell this platform lacks, or whose fingerprint differs is a refused row (`exit_code: null`, `error` set, counted in `failures`); the error for a changed fingerprint names the changed lines and ends `approve it with: thoth checks approve --run RUN --reason "WHY"` with the real run id (**Breaking change, LESS-5**: it ended `ask the human to run: thoth checks approve --run RUN`); the error for a check that is not approved is `check ID is not approved; approve it with: thoth checks approve --reason "WHY"` (**Breaking change, LESS-5**), and `no_approved_checks`' message ends `then approve them with: thoth checks approve --reason "WHY"`. The worker's `verification_requests` are only listed, as `worker_declared`.
- `verify RUN [--dry-run] --json` → `{"ok","run_id","task_id","source","dry_run","failures","results","warnings","worker_declared"}`. `source` is `approved checks (spec)` or `approved checks (all)`. `worker_declared` is `[{"cmd","cwd"}]` (`[]` plus a warning when the run has no result document). Each `results[]` entry carries `index`, `check_id`, `shell`, `cmd`, `cwd`, and either `exit_code`, `evidence`, `error` (plus `duration_ms`, `stdout_bytes`, `stderr_bytes`, `output_truncated` when it ran) or, with `--dry-run`, `dry_run`, `would_refuse`, `reason` (only when refused) and `fingerprint_ok`. The evidence row's `detail` adds `check_id`, `shell`, `fingerprint`, `fingerprint_ok`, `inputs_checked`, `approved_at`, `program` (and `script` for `powershell`); `fingerprint_ok` and `inputs_checked` (and the dry run's `fingerprint_ok`) are always booleans, `false` when no comparison happened. `program` is the absolute path of the file the fingerprint resolved and that was run, for every shell (`null` on a refused row); for an `argv` check it is also `detail.argv[0]`. The `verify_ran` event's detail adds `checks` (the ids), `worker_declared`, and (CK-1) `approved_checks`, `previous_verify` and `missing_since_last_verify`. Additive (`--exe-pin`): every `results[]` entry and the evidence row's `detail` also carry `exe_pin` (`hash` or `path`, the check's value; `null` for an id not in the catalogue).
- additive (CK-1): `verify RUN … --json` also carries `approved_checks` (array of ids: the checks in the catalogue that are approved now, in id order), `previous_verify` (`null`, or `{"run_id","task_id","ts","seq","approved_checks"}` of the newest earlier `verify_ran` event of this repository, whatever the task or run; its `approved_checks` is `null` for an event written before CK-1) and `missing_since_last_verify` (`[{"check_id","now"}]` in id order: the ids approved at the previous verify and not approved now; `now` is `removed` when the id is no longer in the catalogue and `pending` when it is there but not approved, for example after `checks add` replaced it). The comparison is of approved sets, not of which checks ran. `[]` when there is no previous verify or it predates CK-1. `ok`, `failures` and the exit code are not affected, and all four paths carry the three keys: a normal run, `--dry-run`, `--at-base`, and the early end with `reason` `no_approved_checks`. Human output prints one line `  approved checks missing since the previous verify (run RUN at TS): ID (removed), ID (pending re-approval)` after the first line when the array is not empty. The `verify_ran` event's detail carries the same three keys (`previous_verify` without `approved_checks`).
- additive (LESS-1): a check may hold several approvals (each `checks approve` that is not already an exact match adds one), and `verify` runs it when any of them matches. When none matches exactly, it still runs when its fingerprint differs from one approval only in package information: lines of Cargo.toml's `[package]` table that assign one of `version`, `description`, `license`, `license-file`, `authors`, `repository`, `homepage`, `documentation`, `readme`, `keywords`, `categories` a one-line string or array of strings, and the `version = "X"` line of a Cargo.lock package without a `source` line; CRLF versus LF is not a difference. Anything else (dependencies, features, `build`, `links`, `edition`, `[profile]`, `[patch]`, `.cargo/config`, toolchain files, any other input, the program, the command) is refused as before. The comparison needs the approved bytes of the file, which Thoth stores in the repo's blob store when `checks approve` approves or confirms a check, and when a `verify` matches exactly. Every `results[]` entry (run, refused, failed to start, and `--dry-run`) carries `approval` (`null`, or `{"match":"exact"|"package_info_only","fingerprint","approved_at","source","run_id"}` of the approval that matched) and `package_info_lines` (`[{"path","line","approved","now"}]`, `[]` unless `package_info_only`); the evidence detail carries the same two keys, plus `auto_accepted: "only package info changed"` for `package_info_only`. Human output prints `        auto-accepted (only package info changed): PATH:LINE, …` under such a row (`line endings only` when no line differs but its line endings). A refusal whose changed lines are all Cargo.toml/Cargo.lock inputs whose approved copy is not stored adds `; package-info auto-accept needs the approved copy of PATH, which is not stored (approvals made before this version keep none); approve once more` before `; if this change is intended, …`. `--at-base` matches any approval by its own hex and never auto-accepts. The top-level keys and the exit code are unchanged.
- **Breaking change (2026-10-01, P7-1b AC14): a `cmd`, `powershell`, `sh` or `bash` check must be one program and plain arguments.** Its command may hold only ASCII letters and digits, space, tab and `. _ - / \ : =` (no `=` in the program word); `checks add` refuses anything else (exit `1`, nothing written, error containing `a shell check must be one program and plain arguments`) and `verify` refuses such a row. The program is resolved once, by the fingerprint, and handed to the shell as an absolute path (`detail.program`); a program that resolves to no file (a shell built-in such as `call` or `exit`) is refused with `program not found`, and on Windows one its shell would not run itself (`cmd`: not `.exe`/`.com`/`.cmd`/`.bat`; `powershell`: not `.exe`/`.com`/`.ps1`, a batch file → `use shell cmd for this check`, or a path holding `[`, `]` or a backquote) is refused. Shell syntax belongs in a script approved as the check. `detail.argv` for these shells is now the new invocation (`cmd /D /V:OFF /S /C ""%THOTH_CHECK_PROGRAM%" ARGS"`; `sh -c '"$0" "$@"' PATH ARGS...`).
- WT-1: once a task is `pass` (or `cancelled`) its worker worktree is usually removed, so `verify RUN` then exits `1` with `the worktree for run RUN is gone`; `verify RUN --at-base` is not affected (it checks out its own temporary worktree).
- With nothing to run (empty catalogue, or none of the spec's ids in it) `verify` exits `1`, runs and records nothing, and `--json` is the same key set with `ok: false`, `failures: 0`, empty `results` and `reason: "no_approved_checks"`.
- `verify RUN --at-base --json` (additive) runs the same checks in a temporary detached worktree at the task's `base_commit` and records them as evidence labelled `verify@base: …`. There the base checkout's inputs are not read: the hex compared with the approved fingerprint is built from the current `shell=`, `cmd=`, `cwd=` lines, the `exe=` line resolved in the base checkout and the approval's own `input=` lines; the evidence detail has `inputs_checked: false`. Its `--json` output is the plain `verify` shape plus `at_commit` (the base sha); `--at-base --dry-run` executes and records nothing. Plain `verify` output has no `at_commit` key.

Relied on: exit code and `run_id`.

### `thoth checks add | approve | list | remove` (additive)
The repo's catalogue of approved checks, kept in `state.db`. The commander proposes checks and approves them, with a reason (LESS-5); a person can also approve in a terminal. `verify` runs only these (see above). `add`, `approve` and `remove` exit `1` and write nothing when `THOTH_RUN_ID` is set (inside a worker or reviewer).
- `thoth checks add ID --cmd COMMAND [--shell argv|cmd|powershell|sh|bash] [--cwd PATH] [--exe-pin hash|path] [--input PATH]... --json` → `{"ok":true,"check_id","shell","cmd","cwd","inputs","status":"pending","exe_pin"}`. `inputs` are repo-relative `/` paths: the `--input` files plus the ones Thoth infers (files named in the command, and the toolchain's own such as `Cargo.lock`). An existing ID is replaced and loses every approval, even when the new definition is identical (LESS-1). `--exe-pin path` (pin the program's resolved path, not its bytes) is refused when the program lies in the repository tree, lies in a worktree, is written as a relative path, or does not resolve; that, and a value other than `hash` or `path`, exits `1` and writes nothing.
- `thoth checks approve [--run RUN] --json` → `{"ok":true,"approved":[{"check_id","fingerprint","source"}],"unchanged":["id"]}`; `source` is `main` or `run`. A check whose current fingerprint equals one of its approvals is listed in `unchanged`; every other check gets one more approval (LESS-1: earlier approvals are kept, not replaced; a check that matches only through package-info auto-accept is approved too). For each approved or unchanged check, its Cargo.toml / Cargo.lock inputs are stored in the blob store for package-info auto-accept. With nothing to do the human output is `nothing to approve` and `approved` is empty. Additive: only when an `--exe-pin path` check is refused this time does the output also have `"refused":[{"check_id","reason"}]`; a refused check is not approved, the others are, and the exit code is still `0`.
- LESS-5: `thoth checks approve [--run RUN] [--reason TEXT] --json`. **Breaking change:** with no terminal attached (stdin and stdout not both a terminal: the commander's shell, `!` in Claude Code, or the MCP tool) `--reason` is required; without it, or with a blank one, it exits `1` and writes nothing (`checks approve needs --reason when no terminal is attached …`, `--reason must not be empty`). A person at a terminal may leave it out. Additive keys: `approved_by` (`human_terminal` | `commander_cli` | `commander_mcp`), `reason` (string or null), `protected_approved` (`[{"path","status","kind"}]`: with `--run`, the run's `test` and `check_word` changes from its newest diff evidence that no earlier approval of that run covers, approved now and recorded in one `protected_change_approved` event) and `protected_unchanged` (paths of those changes an earlier approval already covers); both are `[]` without `--run`, and when the run has no diff evidence or its `protected_changes` cannot be read (then stderr has `warning: run RUN's diff evidence CODE has no readable protected_changes; run review start, then approve again`, with or without `--json`). Human output adds `approved by WHO; reason: TEXT` and one `approved protected change of run RUN: PATH (STATUS, KIND)` per protected change approved. Additive refusal: with `--run`, while RUN's task has a run that has not ended, it exits `1` and writes nothing (`checks approve --run RUN refuses while task TASK has a live run (LIVE_RUN is STATUS): …`); a live run of another task, and `approve` without `--run`, are not refused. The whole approval is one write transaction.
- `thoth checks list --json` → `{"checks":[{"check_id","status","shell","cmd","cwd","inputs","approved_at","approved_fingerprint","changed","exe_pin"}]}` sorted by `check_id`. `status` is `approved` | `stale` | `pending`; `changed` lists the fingerprint lines that differ for a `stale` check (e.g. `input=Cargo.toml`), else `[]`.
- additive (LESS-1): `status` is `approved` when the main tree matches any approval, exactly or through package-info auto-accept (as `verify`), `stale` when it matches none (`changed` is against the newest approval), `pending` without approvals; there are still only these three values. `approved_at` and `approved_fingerprint` are the newest approval's. Each entry also has `other_approvals` (the number of approvals besides the newest, `0` without any), `match` (`exact`, `package_info_only` or `null`), `matched_approval` (`null` or `{"fingerprint","approved_at","source","run_id"}`; `source` is `main`, `run` or `migrated` for an approval made before state.db v6) and `package_info_lines` (as `verify`). The human line ends `  (+N earlier approvals)` when `other_approvals` is above 0, and a `package_info_only` check has a next line `  auto-accepted (only package info changed): PATH:LINE, …`. `list` writes nothing.
- `thoth checks remove ID --json` → `{"ok":true,"check_id","was_approved"}`; an unknown ID exits `1`. Additive (CK-1): `was_approved` is true when the removed check was approved. Removing a check forgets every approval of it (LESS-1).
- additive (LESS-3): `thoth checks add --json` also has `hints`, an array of strings, `[]` when there is nothing to say. A `cargo test` command (`cargo` or `cargo.exe`, by any path, whose first word not starting with `-` or `+` is `test` or `t`) without `--no-fail-fast` before a lone `--` gets `cargo test stops at the first test target that fails, so later targets and their new tests never run; consider cargo test --no-fail-fast`. The human output has the same text as a last line starting `hint: `. A hint never refuses: the check is still stored as `pending`.
- additive (P7-2): `thoth checks list --json` also has `protected_patterns` (array of strings, sorted), the repo's extra protected-file patterns.
- additive (P7-2): `thoth checks protect PATTERN --json` → `{"ok":true,"pattern","added","protected_patterns"}` and `thoth checks unprotect PATTERN --json` → `{"ok":true,"pattern","protected_patterns"}`. `pattern` is the stored form (lower-cased, `/` separators); `added` is false when it was already there. A pattern is `name/` (a path segment), a file name with at most one `*` at its start or end (`*.snap`, `golden_*`), or a whole path; any other shape (`a*b`, `**/x`) exits `1`, as does `unprotect` of a pattern not in the list. Both exit `1` and write nothing when `THOTH_RUN_ID` is set. A changed file matching a pattern is reported as kind `test`.

### `thoth worktrees prune --json`
New (WT-1). Removes the clean worktrees of every `pass` / `cancelled` / `accepted` task of the repo (`accepted` is additive, DOC-1), worker and review alike, and lists what it removed and kept. A task with a `queued` / `running` run is skipped. A worktree is removed only with `git worktree remove --force`, and only when it lies under the repo's worktrees directory, has no uncommitted change outside `.agent/` and `.agents/`, and its `HEAD` is contained in some local branch; branches, commits and evidence are never touched. Every call ends with `git worktree prune`. It exits `1` and does nothing when `THOTH_RUN_ID` is set (stderr names `THOTH_RUN_ID`) or when run from inside the worktrees directory (stderr: `run worktrees prune from the main working tree, …`). Relied on: `ok`, `removed`, `kept`, `tasks_checked`; `removed[]` and `kept[]` entries have the keys listed under `task close`. `tasks_checked` counts the finished tasks with no live run. `ok` is false, and the exit code `1`, when an entry of `kept` has reason `remove_failed` or `git_error`; otherwise `ok` is true and the exit code `0`, whatever else was kept. Each removal and each newly kept directory is an event (`worktree_removed` / `worktree_kept`, see `events`).

## Git side

The candidate lives on branch `thoth/TASK_ID/aN` (N = attempt) in the task's worktree (`status TASK --json` → `runs[].worktree`). Attempt 1 starts from the task's `base_commit`; a rework attempt starts from the previous attempt's candidate. thoth never merges; it removes the clean worktrees of `pass`/`cancelled`/`accepted` tasks itself (see `thoth worktrees prune`), and the branch stays for the commander to merge. Additive (BASE-1): the commander merges it into the task's `base_branch` (`task show --json`); when that is null, the commander decides where it goes.
### thoth models [--refresh] [--json]

Lists each registered profile and its usable models. JSON output has top-level
`source` (`name`, `url`), `cache` (`path`, `present`, `fetched_at`, `refreshed`), and `profiles`; each
profile provides `profile_id`, `provider`, `harness`, `models_source`, `note`,
and `models`. Model entries provide `model`, `display_name`, `score_name`,
`score`, `may_have_degraded`, `thoth`, and the new `thoth_breakdown` array.

Additive (MODELS-1). `models_source` has a third value, `agy_models`: an
antigravity profile's `models` is the list its last login check stored
(`model` the slug, `display_name` agy's name for it, in agy's order), where
it used to be `unsupported_harness` with no models. Every profile gains
`models_seen_at` (RFC 3339 UTC or null): for agy the time the stored list was
read; for codex the account's `models_cache.json` top-level `fetched_at` when
it parses as RFC 3339, else the file's modification time, null when neither
can be read; always null for claude-code. Whatever offset codex wrote
`fetched_at` in, the value is the same instant re-formatted in UTC (`…Z`;
`2026-10-16T15:15:39+08:00` reads `2026-10-16T07:15:39Z`). An agy profile with no stored list
has `models` `[]` and `note` `no model list stored yet; run `thoth profile
check ID``; one whose list is 24 hours old or more has `note` `model list
read N hours ago; run `thoth profile check ID` to read it again`; otherwise
`note` is null. Every model entry gains `unavailable`: null, or `{"seen_at",
"reason", "run_id"}` when a run proved this account cannot use the model and
nothing has cleared that since (claude-code only; see "Model rule for a
`claude-code` profile" under `dispatch`; `reason` is the harness's text,
`run_id` that run). The human output prints `  models read at TIME` under a
profile's title line when `models_seen_at` is not null (the note, when there
is one, on the next line), and ends a model's line with ` — unavailable since
TIME: REASON`. `fixed_list` and `codex_models_cache` keep their keys and
order; the command still starts no child process and reads no network (apart
from `--refresh` and `scores-auto-refresh`), and reads `global.db` read-only
(a schema 3 database shows no agy list). The MCP tool `thoth_models` is this
output; `thoth_dispatch`, `thoth_review_start` and `thoth_spec_review_start`
return the refusals above as their `isError` document, with no new parameter.

Each
breakdown entry has the new keys `effort`, `task_type`, `difficulty`,
`implementer_runs`, `completed`, `reviewed`, `pass`, `reject`, `input_tokens`,
`output_tokens`, `quota_points`, and `quota_runs`. `effort` comes from
`runs.spec`'s `launch.effort`; `task_type` and `difficulty` come from the
associated task's `tasks.spec`. `quota_points` uses numeric `detail.points`
from the latest `run_quota` event for each run; the runner writes that event as
each implementer run settles (see `thoth events`), and a run whose `points` is
null is not counted in `quota_points` or `quota_runs`. Like the event, the sum is
a rough estimate. Only `--refresh` touches the network, unless the
`scores-auto-refresh` setting is on; without either, the command reads the
local score cache only.

Breaking change (RJ-3): the JSON (and so MCP `thoth_models`) carries no pass
or reject counts and no verdicts anywhere: a model's `thoth` has exactly
`implementer_runs` and `reviewed`, and a `thoth_breakdown` entry no longer has
`pass` or `reject` (its other keys are unchanged). The commander judges models
by the external tables, not by Thoth's own record. The human output still
prints `PASS N REJECT M` on each model line and each labelled breakdown line,
and the report page still shows the pass rate and each run's outcome.

Additive (DT-6): `cache` and `external.artificial_analysis.cache` each gain
`age_hours` (whole hours since `fetched_at`; null with no cache) and `stale`
(true with no cache or at 48 hours or more). The top level gains
`auto_refresh`: `enabled`, `skipped` (null, or `off`, `worker`, `busy`,
`manual`), `attempted` and `refreshed` (arrays of source names), and `failed`
(array of `{source, error}`). With `scores-auto-refresh` on, `thoth models`,
`thoth report` and (RJ-2) `thoth recommend` refresh a missing or 24-hour-old
cache first (the two keyed sources only when their key is set; Agent Arena
needs none). `dispatch`, `review start` and `spec review start` read no score
cache, so they never trigger it. A failed refresh prints one `warning:` line
on stderr and the old cache is used.

Additive (RJ-2): Agent Arena is a third source, the Hugging Face dataset
`lmarena-ai/leaderboard-dataset` (CC BY 4.0), fetched without a key through
the same curl child process (following redirects, since the Hub answers a
parquet `resolve` with a 302 to its CDN; the two keyed sources still never
follow one). `models --refresh` always fetches it; the automatic refresh does
when its cache is missing, foreign or 24 hours old, the whole fetch (index
plus every board) within 20 seconds. The index gives the dataset `sha` and
its files; the boards are `agent` plus every `agent_*` folder that has a
`latest-00000-of-00001.parquet`, sorted by name, all taken at that one
`sha`. When the `sha` equals the cached one nothing is downloaded and only
`fetched_at` moves (counted as attempted, not refreshed). Otherwise every
board is downloaded and read (the four columns `model_name`, `organization`,
`score`, `rank` must be present; the others are null when missing; extra
columns are ignored), and the cache `$THOTH_HOME/model-scores-arena.json`
(`schema` `agent-arena-v1`: `fetched_at`, `sha`, `source` with `name`,
`url`, `license`, `license_url`, and `boards`: `BOARD → {publish_date,
rows}` with every row, `model_name`, `organization`, `score`,
`score_ci_lower`, `score_ci_upper`, `observation_count`, `rank`) is replaced
whole only when every board succeeded; any failure stops the fetch and leaves
the old cache untouched. An Agent Arena failure never changes the exit code:
`--refresh` prints `warning: Agent Arena refresh failed: …`, the automatic
refresh its usual warning; a keyed source's failure still exits `1` from
`--refresh`. Breaking change (RJ-2): `models --refresh` with neither key set
no longer exits `1`: it fetches Agent Arena only and says on stderr that AI
Stupid Level and Artificial Analysis were skipped (a `note:` naming both
environment variables and `thoth config set-secret`, with the sentence about
free keys; CFG-2: "set" means in the environment or in secrets.json, and the
note adds that secrets.json is not read while THOTH_RUN_ID is set, or, when
secrets.json cannot be used, its path, the problem and `see thoth config
list`). The JSON gains `external.agent_arena`: `source` (`name`, `url`,
`license`, `license_url`), `cache` (`path`, `present`, `fetched_at`,
`refreshed`, `age_hours`, `stale`, `sha`) and `boards` (the board names);
no rows (`recommend` sends them filtered). The plain output gains `scores:
Agent Arena URL (CC BY 4.0)` with a cache, else the usual `stale:` line
without a key name. The cache never enters the records folder, an event or
evidence.

Breaking change (RJ-2): `score_name` of a claude-code model is no longer a
fixed alias table. It is the `model` the alias's newest `harness_init` event
reported for that profile (a trailing `-YYYYMMDD` removed), the same name the
Artificial Analysis match uses; an alias that never ran on that profile has
`score_name`, `score` and `aa` null (and is listed in
`unmatched_profile_models`). Codex still uses its slug.

Additive (DT-1): each `thoth_breakdown` entry gains `cache_read_tokens`,
`cache_write_tokens`, `reasoning_tokens` and `total_tokens` (summed like
`input_tokens`: runs with a null column count as 0; `total_tokens` counts no
token twice), `quota_points_weekly` (sum of `detail.seven_day.points` over the
reliable runs that have one; null when none), `quota_runs_weekly` (how many)
and `quota_runs_unreliable` (runs whose latest `run_quota` event says
`reliable: false`, numbers or not). `quota_points` and `quota_runs` now count
only reliable runs: an event without `reliable`, or with `reliable: true`.
A reviewer run now writes a `run_quota` event too (see `thoth events`), and
`thoth_breakdown` still counts implementer runs only. A repo whose state
database a DT-1 build has not opened yet has no token columns and reads as 0.

Additive (DT-3): with `records-dir` set, the other computers' `NAME.jsonl`
files in that folder are merged into `thoth` and `thoth_breakdown`. The top
level gains `records`: `dir` (string or null), `machine` (this computer's
name), `files` (array of `{name, machine, lines, skipped_lines}`, one per other
file, in name order) and `error` (null, or the reason the folder cannot be
read; the totals are then local). Each `thoth_breakdown` entry gains `machines`
(string array, sorted: the computers whose runs are in the group; this
computer's name for local runs). A line is counted only when it is an
implementer line, its `profile` and `model` equal the profile id and model
being totalled, and its `run_id` is not in this computer's databases (the
computer's own file is never read); a line that does not parse or has `v` other
than 1 or 2 counts in `skipped_lines` (Breaking change, RJ-3: v2 is the
current line format, v1 is still read). Remote `thoth.reviewed` (and the pass
and reject counts of the human output) use each run's latest verdict only, and
a remote group is keyed by the line's
`effort` (not `effort_requested`), `task_type` and `difficulty`. The plain
output gains one line after `score cache:` when `records-dir` is set:
`records: DIR — N other file(s), M line(s)`, or `records: DIR cannot be read
(ERROR); only this computer's runs are counted`. With `records-dir` unset the
plain output is unchanged.

Additive (DT-7): the top level gains `decision_table`, the dispatch decision
table the report page and the panel show, with three keys. `rows`: one entry
per model, profile, task type and effort (difficulty merged), taken from
`thoth_breakdown` of every (profile, model) with implementer runs here or in
the shared records, whether or not `profiles` lists the model, sorted by
`model`, then `profile`, then `task_type` (`code`, `research`, `review`,
`docs`, `mechanical`, null last), then `effort` (`low` to `ultra`, others
after, null last). Each row has `model`, `profile` (the account),
`task_type`, `effort` (null when the run recorded none), `machines` (a
sorted string array), `implementer_runs`, `reviewed`, `tokens` (the
sum over the merged groups of max(`total_tokens`,
`input_tokens` + `output_tokens`)), `tokens_avg` (`tokens` / `implementer_runs`;
null when `tokens` is 0), `quota_points`, `quota_runs`, `quota_points_avg`,
`quota_points_weekly`, `quota_runs_weekly`, `quota_points_weekly_avg` (the
averages count reliable runs only; null when there are none) and
`quota_runs_unreliable`. Breaking change (RJ-3): a row no longer has `pass`,
`reject` or `pass_rate` (the report page still shows the pass rate to the
human), and `deviations` is replaced by `choices`: every run, every role
(implementer, reviewer, spec reviewer), from this computer's databases plus
the shared folder (a `run_id` present here is taken from here), newest
`started_at` first, a run without a start time last, the same start time by
`run_id` descending. Each has exactly `run_id`, `machine`, `task_id`,
`task_title`, `attempt`, `role`, `started_at`, `profile`, `model`,
`effort_requested`, `effort`, `task_type`, `difficulty`, `risk`,
`previous_run` (the `previous_run` of this computer's run's newest
`launch_choice`; null when missing, and always null for another computer's
run, whose line does not carry it), `reason` (why this profile, model and
effort; null when none was given) and `status`; no verdict. `summary`: `runs`
(sum of `implementer_runs`), `machines` (distinct computers over the rows),
`rows`, `choices` (how many), `choices_recent` (those started in the last
`recent_days` days), `choices_recent_without_reason` (of those, the ones whose
`reason` is null) and `recent_days` (7); `deviations` and `deviations_recent`
are gone. There is no pass or reject total. Relied on: `summary.runs`,
`summary.machines`, `summary.choices_recent`,
`summary.choices_recent_without_reason`, `summary.recent_days` (the panel's
line). Changed (DT-7b, 2026-10-12): until DT-7b a row merged every
profile of a model and carried a `profiles` array; it is replaced by
`profile` — quota points are a share of that account's window and the
windows differ between plans, so an average across accounts misled. Nothing
outside Thoth read `rows`.

Artificial Analysis is a separate optional source. Set
`ARTIFICIALANALYSIS_API_KEY` to refresh its free endpoint,
`GET https://artificialanalysis.ai/api/v2/language/models/free?page=N`;
`THOTH_AA_URL` replaces the complete prefix through `page=`. Its free tier is
limited to 100 requests per 24 hours and Thoth reuses the local
`model-scores-aa.json` cache for 360 minutes. Data stays local and must not be
forwarded. Names are lowercased and runs of non-ASCII-alphanumeric characters
become one `-`; parenthesized variants supply the first recognized effort.
Codex uses its slug, while Claude uses the latest alias run's
`harness_init.detail.model` after removing a trailing `-YYYYMMDD` (RJ-2: no
fallback; an alias without a run matches nothing). Models add nullable `aa`, whose `rows`
contain name, effort, intelligence/coding/agentic indexes, and input/output
prices. Top-level `external.artificial_analysis` adds `source`, `cache`, and
`unmatched_profile_models`. Every terminal, JSON, and HTML presentation labels
and links [Artificial Analysis](https://artificialanalysis.ai) as the source.

Breaking change (2026-09-29): scores now use the AI Stupid Level v1 API.
`score` contains `current`, `trend`, `status`, `confidence_lower`,
`confidence_upper`, and `observed_at`; the former baseline, delta, regime, and
drift fields are removed. `may_have_degraded` is true when status is not `good`
or trend is `down`.
