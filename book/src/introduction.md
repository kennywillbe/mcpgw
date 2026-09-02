# Introduction

mcpgw is a single binary that keeps one list of your MCP servers, points every
client at itself, and forwards the calls — so the list lives in one place, and
so does the traffic.

## Two problems, one binary

**The first is bookkeeping.** Claude Desktop keeps its server list in one file.
Claude Code keeps its own. Cursor spells the same entry differently. VS Code
has a fourth shape. One server, four files, four schemas — and every token
rotation means finding all four again.

mcpgw makes `~/.config/mcpgw/config.toml` the canonical list and syncs it
outward. It only ever rewrites entries it wrote itself; anything you added by
hand is left alone and reported back to you as unmanaged. Every client file is
backed up before it's touched, and one command puts the previous version back.
Nothing about that is a one-way door: `mcpgw eject` writes your original
definitions back into every client and leaves you able to uninstall mcpgw
entirely.

**The second is visibility.** Once the servers are wired up, you have no idea
what's happening through them. Which tool did the agent pick? What arguments
went out? Did that call fail, or did the server quietly return nothing?

The entries mcpgw writes point at mcpgw, so every client's calls pass through
one process — `mcpgw serve` in a terminal, or the same gateway supervised by
your machine's service manager via `mcpgw daemon install`. It writes every
request to a daily JSONL file, and `mcpgw watch` follows it live:

```text
$ mcpgw watch
watching /Users/you/.local/share/mcpgw/traffic (Ctrl-C to stop)
  now  ✓  github__create_issue               87ms
  12s  ✓  linear tools/list                   4ms
  30s  ✗  github__search_code               210ms  upstream "github" failed after 3 attempt(s)
```

## Why not the MCP Inspector

The official [MCP Inspector](https://github.com/modelcontextprotocol/inspector)
connects to a server *as its own client*, so everything it shows you is traffic
Inspector generated. That's the right tool for poking at a server you're
writing. It is the wrong tool for the question "what did my agent just do".

mcpgw sits on the path the agent already uses, so the log is the agent's own
calls.

## Where it sits

[agentgateway](https://github.com/agentgateway/agentgateway) targets Kubernetes
and enterprise deployments. [MetaMCP](https://github.com/metatool-ai/metamcp)
gives you aggregation and a web UI, in exchange for a Node and Docker stack you
have to keep alive. mcpgw is a static binary with no runtime dependencies doing
the boring local job: one config, every client, real traffic.

Start with [Installation](./installation.md), then type `mcpgw` — on a terminal
that opens the setup wizard, which walks the [Quickstart](./quickstart.md) for
you one confirmed step at a time. Before you point your whole machine at it,
the [Trust model](./trust-model.md) is what having one process in the middle
does and does not protect.
