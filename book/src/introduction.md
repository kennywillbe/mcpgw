# Introduction

mcpgw is a single binary that keeps one list of your MCP servers, pushes that
list into every client that wants its own copy, and — when you ask it to —
stands in the middle of the traffic so you can see what your agents are
actually calling.

## Two problems, one binary

**The first is bookkeeping.** Claude Desktop keeps its server list in one file.
Claude Code keeps its own. Cursor spells the same entry differently. VS Code
has a fourth shape. One server, four files, four schemas — and every token
rotation means finding all four again.

mcpgw makes `~/.config/mcpgw/config.toml` the canonical list and syncs it
outward. It only ever rewrites entries it wrote itself; anything you added by
hand is left alone and reported back to you as unmanaged. Every client file is
backed up before it's touched, and one command puts the previous version back.

**The second is visibility.** Once the servers are wired up, you have no idea
what's happening through them. Which tool did the agent pick? What arguments
went out? Did that call fail, or did the server quietly return nothing?

Run `mcpgw serve` and every client talks to one endpoint instead of to each
server directly. The gateway writes every request to a daily JSONL file, and
`mcpgw watch` follows it live:

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

Start with [Installation](./installation.md), then the
[Quickstart](./quickstart.md).
