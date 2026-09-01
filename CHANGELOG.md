# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.1.0] - 2026-09-01

First public release. One binary, no runtime dependencies.

### Management layer

- A canonical server list at `~/.config/mcpgw/config.toml`, hand-editable and
  self-documenting: every generated entry writes its fields out explicitly.
  Reads go through a validating parser, writes go through the TOML syntax tree,
  so comments and ordering survive edits.
- `add`, `remove`, `enable`, `disable` for editing that list. Writes are atomic
  and hold an advisory lock, so a crash mid-write cannot leave a half-file
  behind.
- `import` pulls what's already configured in Claude Desktop, Claude Code,
  Cursor and VS Code into the canonical list, slugifying invalid names and
  reporting every rename.
- `sync` pushes the list back out to those clients. It only touches entries it
  wrote itself — anything you added by hand is left alone and reported as
  unmanaged. Every client file is backed up before it is written, `--dry-run`
  shows the diff first, and `sync --rollback` puts the previous version back.
- `list` renders a table, or JSON with `--json`. So does every other command
  that prints something.

### Gateway

- `serve` runs all your enabled servers behind a single MCP endpoint on
  `127.0.0.1:8137`. Tools are namespaced `server__tool`, so adding a server
  never renames an existing tool.
- One upstream connection per server, multiplexed across clients — no process
  per session. A dead upstream is restarted with backoff and, if it keeps
  failing, reported loudly instead of quietly disappearing from `tools/list`.
- stdio and HTTP upstreams, both behind the same lifecycle.
- `sync --gateway` repoints client configs at the gateway in one command.
- `connect` bridges stdio-only clients (Claude Desktop) to the running gateway,
  and says so plainly when the gateway isn't up.

### Traffic capture, watch, inspect

- While serving, every request is appended to a daily JSONL file under
  `~/.local/share/mcpgw/traffic/`: timestamp, session, server, tool, latency,
  outcome, plus arguments and response truncated to 2 KB. Files are mode 0600.
  `--no-capture` turns it off.
- `watch` follows that log live with one line per call, and filters by
  `--server` or `--tool`. It reads the file, so it works on a gateway that was
  already running and on yesterday's traffic too.
- `inspect <server>` connects to a single server directly and tables its tools
  and resources. No gateway required.

### Diagnostics

- `doctor` checks the canonical config and every detected client: parse errors,
  invalid names, stdio commands that don't resolve on `PATH`, malformed URLs,
  entries a client holds that can't be represented. Errors exit 1, warnings
  don't.
- `doctor --probe` goes further and actually connects: it spawns or dials every
  server, runs the MCP handshake and `tools/list`, and reports name, version
  and tool count. Probes run in parallel with a per-server timeout.

[Unreleased]: https://github.com/kennywillbe/mcpgw/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/kennywillbe/mcpgw/releases/tag/v0.1.0
