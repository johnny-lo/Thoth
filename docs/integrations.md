# Using Thoth from other agent tools

Thoth's commander workflow is one guide with three ways to reach it: the Claude Code plugin, the `thoth_guide` MCP tool, and the `thoth guide` command. All three return the same text, and `thoth guide` always matches the installed executable.

## Common prerequisites

- `thoth` is on `PATH` (check with `thoth --version`).
- At least one worker profile is registered and logged in (`thoth profile list`).

## Claude Code

```text
claude plugin marketplace add johnny-lo/Thoth
claude plugin install thoth@thoth
```

See the "Claude Code plugin" section of [mcp.md](mcp.md) for a local checkout and for the names Claude Code gives the skill and tools.
In Claude Code the plugin also adds a read-only panel, opened with `/thoth`: see "Thoth panel" in the same section.

## Codex

Register the MCP server with one command:

```text
codex mcp add thoth -- thoth mcp
```

Or paste this into `~/.codex/config.toml`:

```toml
[mcp_servers.thoth]
command = "thoth"
args = ["mcp"]
tool_timeout_sec = 120
env_vars = ["THOTH_HOME"]
```

`thoth_wait` waits at most 30 seconds per call by default, so it works without raising the tool timeout.

To install the skill, copy the whole folder `plugins/thoth/skills/thoth-dispatch` from a Thoth checkout into Codex's user skill directory. If you do not install the skill, call `thoth_guide` first instead.

Codex has no plugin and does not set `CLAUDE_PLUGIN_ROOT`, so the plugin-versus-executable comparison does not run outside Claude Code.

## Other tools that support MCP

Most MCP clients accept this configuration:

```json
{"mcpServers":{"thoth":{"command":"thoth","args":["mcp"]}}}
```

Call `thoth_guide` once before first use; the server's instructions say so too.

## Tools without MCP (for example Pi)

Put one sentence in the tool's instruction file telling it to run `thoth guide` before dispatching work and to follow the output. For AGENTS.md:

```text
To delegate implementation to another account, first run `thoth guide`
and follow it. Use the `thoth` command for everything; never edit
worker worktrees yourself.
```

## Checked against

| Tool | Version | Checked on | Verified | Not verified |
|---|---|---|---|---|
| Claude Code | 2.1.285 | 2026-09-30 | Command syntax, from `--help` | Everything else (not verified on a real install) |
| Codex CLI | 0.154.0 | 2026-09-30 | Command syntax, from `--help` | Not verified on a real install: whether Codex passes `THOTH_HOME` to the MCP server; whether the user skill directory is `~/.codex/skills` or `~/.agents/skills`; the default tool timeout |
| Pi | not tested | not tested | Nothing | Everything: not tested |
