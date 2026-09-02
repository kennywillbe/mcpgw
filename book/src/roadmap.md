# Roadmap

Ordered by what feedback asks for first, not by what's fun to build.

- **tool allowlist, deny-by-default** — decide which tools a client may reach
  through the gateway at all.
- **tool-definition drift detection** — notice when a server quietly changes
  what a tool does under a name your prompts already trust.
- **log redaction** — the missing half of traffic capture; see
  [Watching traffic](./traffic.md).
- **rate limiting**.
- **OAuth 2.1 with DCR and PKCE** — and, with it, authentication on the gateway
  itself, so `--bind` stops being a warning.
- **a full TUI for `watch`**.
- **`mcpgw connect` starting a managed gateway daemon on its own**, so there's
  no separate `serve` to remember.

None of this is in 0.1.0.

## Known limits today

- **The gateway is unauthenticated.** It binds to `127.0.0.1` by default for
  that reason. `--bind` anything else and you are trusting your network.
- **Captured bodies are truncated, not redacted.** A secret passed as a tool
  argument lands in the traffic file. It's mode `0600`, and `--no-capture`
  turns it off.
- **No linux-arm64 prebuilt binary yet.** `cargo install mcpgw` works there.
- **The aggregate endpoint serves tools only.** Resources and prompts reach a
  client through a per-server endpoint (`/s/<name>`), because their
  names cannot be namespaced across servers the way tool names can. See
  [Gateway](./gateway.md).

## Asking for things

Open an issue at
[github.com/kennywillbe/mcpgw/issues](https://github.com/kennywillbe/mcpgw/issues).
"Here's what I was trying to do" is more useful than a feature name — the order
above is going to change based on that.

Patches welcome; see
[CONTRIBUTING.md](https://github.com/kennywillbe/mcpgw/blob/main/CONTRIBUTING.md).
