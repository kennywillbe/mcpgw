# mcpgw

**One binary. All your MCP servers, every client, every call — visible and controlled.**

[![CI](https://github.com/kennywillbe/mcpgw/actions/workflows/ci.yml/badge.svg)](https://github.com/kennywillbe/mcpgw/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/mcpgw.svg)](https://crates.io/crates/mcpgw)

**[Documentation →](https://kennywillbe.github.io/mcpgw/)**

![demo](docs/demo.gif)

<!-- The GIF above is generated with vhs from docs/demo.tape: `vhs docs/demo.tape`. -->

## The problem

You add an MCP server to Claude Desktop. Then again to Claude Code, because it
keeps its own list. Then to Cursor, which spells the same entry differently.
Then to VS Code. Four files, four schemas, one server — and the next time you
rotate a token you get to find all four again.

Then the harder half: once they're wired up, you have no idea what's happening.
Which tool did the agent actually call? How long did it take? What arguments
went out? Did that server fail, or did it just quietly return nothing?

mcpgw keeps one list of servers, pushes it into every client, and — if you want
it to — sits in the middle of the traffic so you can watch it.

## Quickstart

```sh
# one of:
curl -fsSL https://github.com/kennywillbe/mcpgw/releases/latest/download/mcpgw-installer.sh | sh
brew install kennywillbe/tap/mcpgw
cargo install mcpgw
```

```sh
mcpgw import                                              # adopt what your clients already have
mcpgw add github -- npx -y @modelcontextprotocol/server-github
mcpgw add linear --url https://mcp.linear.app/mcp
mcpgw list
mcpgw sync --dry-run                                      # see the diff first
mcpgw sync                                                # write it to every client
mcpgw doctor --probe                                      # does any of it actually connect?
```

`sync` only touches entries mcpgw wrote. Anything you added by hand stays where
it is and gets reported as unmanaged, with an `import` suggestion. Every client
file is backed up before it's written; `mcpgw sync --rollback` undoes the last
run.

The list lives in `~/.config/mcpgw/config.toml` and is meant to be edited by
hand. Your comments and ordering survive every write.

## Gateway

Instead of every client talking to every server, they can all talk to mcpgw:

```
  Claude Code  ─┐                        ┌─ github    (stdio)
  Cursor       ─┼─→  mcpgw serve  ─────→ ├─ linear    (http)
  VS Code      ─┤    :8137/mcp           └─ postgres  (stdio)
  Claude Desktop┘    (mcpgw connect)
```

```sh
mcpgw serve                # all enabled servers behind one endpoint
mcpgw sync --gateway       # point every client at it
```

Tools are exposed as `server__tool`, so adding a server never renames an
existing tool. Each upstream gets one connection, multiplexed across clients —
no process per session. If an upstream dies it's restarted with backoff, and if
it keeps failing you get a loud error instead of a silently shorter tool list.

Clients that only speak stdio (Claude Desktop) get `mcpgw connect`, a stdio↔HTTP
bridge to the running gateway. `sync --gateway` picks the right shape per
client for you.

## Watch what's actually happening

While the gateway is serving, it appends every request to a daily JSONL file
and `mcpgw watch` follows it:

```
$ mcpgw watch
watching /Users/you/.local/share/mcpgw/traffic (Ctrl-C to stop)
  now  ✓  github__create_issue               87ms
  12s  ✓  linear tools/list                   4ms
  30s  ✗  github__search_code               210ms  upstream "github" failed after 3 attempt(s)
```

Filter with `--server` / `--tool`, or take the raw lines with `--json` and pipe
them into `jq`. Because it's a file, `watch` works on a gateway that was already
running, and on yesterday's traffic.

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

Honest limits, since this handles your tool traffic:

- The gateway binds to `127.0.0.1` by default. `--bind` opens it up and warns
  you loudly; there is no authentication yet.
- Requests carrying an `Origin` header that isn't a loopback page are refused
  with 403, so a website cannot drive your gateway by rebinding its own domain
  to `127.0.0.1`. MCP clients send no `Origin` and are unaffected.
- Your state directory is mode 0700 and everything mcpgw writes into it —
  backups of client configs, the managed-state file, traffic logs — is 0600.
- `mcpgw list --json` masks `env` and header values; pass `--show-secrets` when
  you actually want them.
- Captured arguments and responses are **truncated at 2 KB, not redacted** — if
  a secret is passed as a tool argument, it lands in that file, and
  `mcpgw watch --json` prints those lines back verbatim. Use
  `mcpgw serve --no-capture` if that's not acceptable yet.
- Redaction, tool allowlists and OAuth are on the roadmap below, not in 0.1.0.

## Roadmap

Post-launch, ordered by what feedback says first:

- tool allowlist, deny-by-default
- tool-definition drift detection (a server quietly changing what a tool does)
- log redaction
- rate limiting
- OAuth 2.1 with DCR and PKCE
- a full TUI for `watch`
- `mcpgw connect` starting a managed gateway daemon on its own

## Contributing

Conventional commits, green CI, PRs against `main` — details in
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

MIT or Apache-2.0, at your option.
