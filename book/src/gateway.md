# Gateway

Syncing gives every client its own copy of the server list. The gateway does
the opposite: every client talks to mcpgw, and mcpgw talks to the servers.

```text
  Claude Code  ─┐                        ┌─ github    (stdio)
  Cursor       ─┼─→  mcpgw serve  ─────→ ├─ linear    (http)
  VS Code      ─┤    :8137/mcp           └─ postgres  (stdio)
  Claude Desktop┘    (mcpgw connect)
```

Two things fall out of that shape: one connection per upstream instead of one
per client, and a single place where all the traffic is visible.

## Serving

```sh
mcpgw serve
```

Every enabled server, behind `http://127.0.0.1:8137/mcp`. Ctrl-C shuts it down
and reaps the child processes.

```sh
mcpgw serve --port 9000
mcpgw serve --server github --server linear   # a subset (repeatable)
mcpgw serve --no-capture                      # don't write the traffic log
```

### Binding

`--bind` defaults to `127.0.0.1`. Anything else prints a warning, because
**there is no authentication yet** — whoever can reach the address can call
your MCP servers:

```sh
mcpgw serve --bind 0.0.0.0    # warns loudly; keep it behind something
```

## Tool names

Tools are exposed as `server__tool` — `github__create_issue`,
`linear__list_issues`. Namespacing every tool up front means adding a server
can never rename an existing one, which would otherwise silently break a prompt
that referenced it.

The exception: `--server` with exactly one name turns the gateway into a plain
pipe to that server, tool names untouched.

## Upstream lifecycle

Each server gets one connection, multiplexed across every client session — no
process per session. If an upstream dies it's restarted with backoff. If it
keeps failing, calls to it return a loud error rather than the tool quietly
vanishing from `tools/list`, which is the failure mode that costs you an
afternoon.

stdio and HTTP upstreams run through the same lifecycle; from a client's side
they're indistinguishable.

## Pointing clients at it

```sh
mcpgw sync --gateway
```

Replaces the per-server entries with a single `mcpgw` entry per client, and
picks the right shape for each one. HTTP-capable clients get the URL directly.

```sh
mcpgw sync --gateway --gateway-url http://127.0.0.1:9000/mcp
mcpgw sync --gateway --dry-run
mcpgw sync --rollback              # back to whatever was there before
```

## stdio-only clients

Claude Desktop only speaks stdio, so it can't be handed a URL. `mcpgw connect`
is the bridge — a stdio server on one side, an HTTP client to the gateway on
the other:

```sh
mcpgw connect
mcpgw connect --url http://127.0.0.1:9000/mcp
```

`sync --gateway` writes this for you; you rarely type it. If the gateway isn't
running, the client sees a plain message saying so and telling you to start it
— not a transport error.
