# mcpgw

**One binary. All your MCP servers, every client, every call — visible and controlled.**

[![CI](https://github.com/kennywillbe/mcpgw/actions/workflows/ci.yml/badge.svg)](https://github.com/kennywillbe/mcpgw/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/mcpgw.svg)](https://crates.io/crates/mcpgw)

**[Documentation →](https://kennywillbe.github.io/mcpgw/)**

![demo](https://raw.githubusercontent.com/kennywillbe/mcpgw/main/docs/demo.gif)

<!-- The GIF above is generated with vhs from docs/demo.tape: `vhs docs/demo.tape`. -->

## The problem

You add an MCP server to Claude Desktop. Then again to Claude Code, because it
keeps its own list. Then to Cursor, which spells the same entry differently.
Then to VS Code. Four files, four schemas, one server — and the next time you
rotate a token you get to find all four again.

Then the harder half: once they're wired up, you have no idea what's happening.
Which tool did the agent actually call? How long did it take? What arguments
went out? Did that server fail, or did it just quietly return nothing?

mcpgw keeps one list of servers, points every client at itself, and forwards
the calls — so the list is in one place and so is the traffic.

## Quickstart

```sh
# one of:
curl -fsSL https://github.com/kennywillbe/mcpgw/releases/latest/download/mcpgw-installer.sh | sh
brew install kennywillbe/tap/mcpgw
cargo install mcpgw
```

Then type `mcpgw`. That's the whole setup:

```sh
mcpgw
```

On a terminal, a bare `mcpgw` is the wizard. It finds the MCP clients you have,
offers to adopt the servers they already hold, offers to keep the gateway
running in the background, points every client at it, and then checks that the
path actually works — asking before each of those, and writing nothing until
you say yes. `mcpgw init --yes` is the same run with the recommended answer
everywhere, for scripts and agents; it still prints the whole plan.

Once everything is set up, a bare `mcpgw` stops being a wizard and becomes a
status card: how many servers, whether the gateway is answering, which clients
are synced.

Piece by piece, if you'd rather:

```sh
mcpgw import                                              # adopt what your clients already have
mcpgw add github -- npx -y @modelcontextprotocol/server-github
mcpgw add linear --url https://mcp.linear.app/mcp
mcpgw list
mcpgw daemon install                                      # keep the gateway running
mcpgw sync                                                # point every client at it
mcpgw doctor --probe                                      # does any of it actually connect?
```

`sync` only touches entries mcpgw wrote. Anything you added by hand stays where
it is and gets reported as unmanaged, with an `import` suggestion. Every client
file is backed up before it's written; `mcpgw sync --rollback` undoes the last
run.

The list lives in `~/.config/mcpgw/config.toml` and is meant to be edited by
hand. Your comments and ordering survive every write.

Installed from the script or an archive? `mcpgw self-update` replaces the
binary with the latest release (checksum-verified); package-manager installs
are left to `brew upgrade` / `cargo install`.

## How it works

Every client talks to mcpgw, and mcpgw talks to the servers:

```
  Claude Code  ─┐                        ┌─ github    (stdio)
  Cursor       ─┼─→  mcpgw serve  ─────→ ├─ linear    (http)
  VS Code      ─┤    :8137/mcp           └─ postgres  (stdio)
  Claude Desktop┘    (mcpgw connect)
```

```sh
mcpgw serve            # every enabled server, each also on its own endpoint
mcpgw sync             # point every client at it, one entry per server
```

Every server keeps its name and its own entry in the client — only the
transport changes, to that server's own `/s/<name>` endpoint on the gateway —
so tool names stay as they are and anything the client keeps beside the entry
survives the move. `mcpgw sync --aggregate` writes the whole gateway as one
entry instead, where tools are exposed as `server__tool` so adding a server
never renames an existing tool.

Each upstream gets one connection, multiplexed across clients — no process per
session. If an upstream dies it's restarted with backoff, and if it keeps
failing you get a loud error instead of a silently shorter tool list. A running
gateway also follows the config file: `mcpgw add` shows up in every client
within a couple of seconds, with nothing restarted and nothing disconnected.

Clients that only speak stdio (Claude Desktop) get `mcpgw connect`, a stdio↔HTTP
bridge to the running gateway. `sync` picks the right shape per client for
you.

### Keeping it running

`mcpgw serve` holds a terminal, which is fine until you depend on it — the
first thing a client does in the morning is ask for a tool list, and nothing is
there to answer. `mcpgw daemon` hands the gateway to the machine's own service
manager: a launch agent on macOS, a systemd user unit on Linux, a service on
Windows.

```sh
mcpgw daemon install     # starts at login, comes back if it crashes
mcpgw daemon status      # what's running, what's installed, where the logs are
mcpgw daemon logs -f
```

It refuses to install on a non-loopback address, because an unattended
unauthenticated gateway on your network is a different thing from a warning you
read in a terminal.

### Not a one-way door

```sh
mcpgw eject
```

Eject writes your original server definitions back into every client under the
same names, removes the gateway entries, offers to remove the daemon, and
prints what is left for you to delete. Your clients then work with mcpgw
uninstalled — which is the point: nothing here is a decision you can't take
back.

## Watch what's actually happening

While the gateway is serving, it appends every request to a daily JSONL file
and `mcpgw watch` follows it:

```
$ mcpgw watch
watching /Users/you/.local/share/mcpgw/traffic (Ctrl-C to stop)
  now  ✓  [mcp] github__create_issue         87ms
  12s  ✓  [s/linear] linear tools/list        4ms
  30s  ✗  [mcp] github__search_code         210ms  upstream "github" failed after 3 attempt(s)
```

Filter with `--server` / `--tool` / `--endpoint` / `--session`, or take the
lines with `--json` and pipe them into `jq`. Because it's a file, `watch` works
on a gateway that was already running, and on yesterday's traffic.

This is the gap the official MCP Inspector leaves: Inspector connects to a
server *as its own client*, so what you see is the traffic Inspector itself
generates. mcpgw sits on the path your agent already uses, so what you see is
what your agent actually did.

For a single server without any of that, `mcpgw inspect <name>` connects
directly and tables its tools and resources.

## How it compares

[agentgateway](https://github.com/agentgateway/agentgateway) is built for
Kubernetes and enterprise deployments — different scale, different problem.
[MetaMCP](https://github.com/metatool-ai/metamcp) gives you aggregation with a
web UI, at the cost of a Node and Docker stack to keep running. The official
[MCP Inspector](https://github.com/modelcontextprotocol/inspector) is a
debugging client, so it sees its own calls, not your agent's. mcpgw is a single
static binary with no runtime dependencies that does the boring local job:
one config, every client, real traffic.

## Security

Every client entry mcpgw writes points at the gateway, so one process now holds
every server's credentials and one log records every call. The
[trust model](https://kennywillbe.github.io/mcpgw/trust-model.html) is the full
version of what that does and does not protect. The short one:

- The gateway binds to `127.0.0.1` by default and has no authentication:
  anything running as you can call every server you have configured. That was
  already true of the client config files those credentials sit in — loopback
  is the boundary, and it is your user account. `--bind` past loopback opens it
  up and warns loudly; a daemon refuses the same address outright.
- Requests carrying an `Origin` header that isn't a loopback page are refused
  with 403, so a website cannot drive your gateway by rebinding its own domain
  to `127.0.0.1`. MCP clients send no `Origin` and are unaffected.
- Your state directory is mode 0700 and everything mcpgw writes into it —
  backups of client configs, the managed-state file, traffic logs — is 0600.
- `mcpgw list --json` masks `env` and header values; pass `--show-secrets` when
  you actually want them.
- Captured arguments and responses are **truncated at 2 KB, not redacted** — if
  a secret is passed as a tool argument, it lands in that file. Use
  `mcpgw serve --no-capture` if that's not acceptable yet.
- `mcpgw watch --json` masks the captured `args` and `response` values, so
  piping the stream somewhere doesn't spread what's in the file; pass
  `--show-secrets` to see them. The human `watch` view never printed them.
- Redaction, tool allowlists and an OAuth broker are on the roadmap below, not
  shipped.

## Roadmap

Ordered by what feedback asks for first:

- tool allowlist, deny-by-default
- tool-definition drift detection (a server quietly changing what a tool does)
- log redaction
- rate limiting
- OAuth 2.1 with DCR and PKCE — for remote servers, and with it authentication
  on the gateway itself
- a full TUI for `watch`
- `mcpgw connect` starting a managed gateway on its own, when no daemon is
  installed

## Contributing

Conventional commits, green CI, PRs against `main` — details in
[CONTRIBUTING.md](https://github.com/kennywillbe/mcpgw/blob/main/CONTRIBUTING.md).

## License

MIT or Apache-2.0, at your option.
