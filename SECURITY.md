# Security

## Reporting a problem

Report security problems privately through GitHub's private vulnerability reporting: the **Security** tab of this repository, then **Report a vulnerability**. Please don't open a public issue for them.

Include what you ran, what happened, and what you expected. A minimal reproduction is the most useful thing you can send.

## What Thoth does with your accounts

- Thoth runs the official, unmodified CLIs (`claude`, `codex`, `agy`) that you installed and logged in to yourself. Each profile is a config directory you created for Thoth (agy uses its own fixed directory).
- Thoth never reads, copies or prints login credentials. It decides whether an account is logged in only from each CLI's own status command.
- Workers run with your user's permissions, and their tool lock is a guard rail, not a sandbox. Run Thoth on repositories and accounts you trust.
- Evidence (command output, diffs) is stored on your machine under `THOTH_HOME`. Review packets send that evidence to whichever reviewer model you choose. Masking secrets in everything that leaves the machine is planned but not built yet. Until then, keep secrets out of the commands that workers run.
