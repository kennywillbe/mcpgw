# Roadmap

Ordered by what feedback asks for first, not by what's fun to build.

- **tool allowlist, deny-by-default** — decide which tools a client may reach
  through the gateway at all.
- **tool-definition drift detection** — notice when a server quietly changes
  what a tool does under a name your prompts already trust.
- **log redaction** — the missing half of traffic capture; see
  [Watching traffic](./traffic.md).
- **rate limiting**.
- **OAuth 2.1 with DCR and PKCE** — brokering the flow for remote servers that
  require it, instead of a token you paste into a header, and with it
  authentication on the gateway itself so `--bind` stops being a warning.
- **a full TUI for `watch`**.
- **`mcpgw connect` starting a gateway on its own** when no daemon is
  installed, so a client that dials a gateway nobody started still works.

None of it is shipped.

## Already shipped

Things this page used to promise, so the line is clear:

- **The setup wizard** — a bare `mcpgw` on a terminal; see
  [Quickstart](./quickstart.md).
- **One behaviour, no modes.** `mcpgw sync` writes one entry per server, each
  pointing at that server's own endpoint on mcpgw. There is nothing to choose:
  the direct mode and the single-entry `--aggregate` mode are both gone, and a
  client that still holds what either of them wrote is migrated by the next
  plain `mcpgw sync`.
- **Per-server endpoints** (`/s/<name>`), on by default.
- **Running as a service** on macOS, Linux and Windows — see
  [Running as a daemon](./daemon.md).
- **`mcpgw eject`** — every client back the way it was.
- **Config hot reload** — a running gateway follows the config file.

## Known limits today

- **The gateway is unauthenticated.** It binds to `127.0.0.1` by default for
  that reason, and a daemon refuses anything else. `--bind` past loopback in
  the foreground and you are trusting your network — the
  [Trust model](./trust-model.md) is the long version.
- **Captured bodies are truncated, not redacted.** A secret passed as a tool
  argument lands in the traffic file. It's mode `0600`, and `--no-capture`
  turns it off.
- **A remote server's OAuth is your problem.** mcpgw forwards the header you
  configure; it does not run the flow and does not refresh anything.
- **The gateway's own `/mcp` endpoint serves tools only.** Resources and
  prompts reach a client through a per-server endpoint (`/s/<name>`), because
  their names cannot be namespaced across servers the way tool names can. See
  [Gateway](./gateway.md).

## Asking for things

Open an issue at
[github.com/kennywillbe/mcpgw/issues](https://github.com/kennywillbe/mcpgw/issues).
"Here's what I was trying to do" is more useful than a feature name — the order
above is going to change based on that.

Patches welcome; see
[CONTRIBUTING.md](https://github.com/kennywillbe/mcpgw/blob/main/CONTRIBUTING.md).
