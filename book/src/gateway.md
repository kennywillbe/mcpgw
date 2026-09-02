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
mcpgw serve --per-server                      # also one endpoint per server
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

## Per-server endpoints

`--per-server` additionally gives every served server its own endpoint, where
its tools appear under their own names:

```sh
mcpgw serve --per-server
# http://127.0.0.1:8137/mcp      — everything, as server__tool
# http://127.0.0.1:8137/s/github — github only, tools unprefixed
# http://127.0.0.1:8137/s/linear — linear only, tools unprefixed
```

A per-server endpoint is a plain pipe, so it forwards everything an MCP server
can offer — tools, resources, resource templates, prompts and argument
completion — with names, URIs and errors untouched.

`/mcp` serves tools only, and that is deliberate. Tools can be namespaced
(`github__create_issue`); resource URIs and prompt names cannot. Two servers
can both offer `file:///README.md` — one name, two different documents — and
rewriting the URIs would break every link inside the contents that points at
them. So the aggregate merges what it can merge honestly, and `/s/<name>` is
where the rest lives.

One caveat: an endpoint reports its server's capabilities as of the last time
it reached that server. A client connecting to a freshly started gateway,
before anything has talked to the server yet, is told "tools" — the
conservative answer — because working it out for real would mean starting the
server in the middle of a handshake. Anything that connects after the first
request through the endpoint sees the full set.

The endpoints share one process and one set of upstream connections, so a
client can take the whole gateway, a single server, or both at once without
starting anything twice. A stdio-only client reaches one the same way:

```sh
mcpgw connect --server github
```

Off unless asked for; `/mcp` alone is what you get otherwise.

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
mcpgw connect --server github    # one server's endpoint, tools unprefixed
```

`sync --gateway` writes this for you; you rarely type it. If the gateway isn't
running, the client sees a plain message saying so and telling you to start it
— not a transport error.
