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
completion — with names, URIs and errors untouched. Answers are handed back as
the server wrote them: caching metadata, `_meta`, and pagination cursors all
survive the hop, and a client pages through a long `tools/list` against the
server's own cursors instead of being handed one list the gateway assembled.

The one thing the gateway does adjust is the protocol revision, because the two
sides of it need not agree. A current client speaks MCP 2026-07-28, where every
result carries `resultType` and lists carry `ttlMs` and `cacheScope`; the server
behind the gateway may predate all three. The gateway fills in what the client's
revision requires and never overwrites what the server actually said, so each
client gets a reply that is valid for the revision it negotiated. A server that
gave no freshness hint is reported as `ttlMs: 0`, `cacheScope: private` — "ask
again, and do not share it": the gateway will not invent a caching policy on a
server's behalf, and the answer was fetched with your credentials.

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
request through the endpoint sees the full set. The name an endpoint reports at
`initialize` follows the same rule: once the gateway has met the server it
answers with that server's own name and version (`Context7 4.0.4`), which is
what a client shows the user; before first contact, and on `/mcp`, it is
`mcpgw`.

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

Every enabled server keeps its entry and its name; only the transport changes,
to that server's own `/s/<name>` endpoint. So the client's list of servers
looks the same before and after, tools stay unprefixed, and anything the client
keeps beside the entry — Cline's off switch, its auto-approved tools — survives
the move, because it is still the same entry.

```text
"github": { "type": "http", "url": "http://127.0.0.1:8137/s/github" }
```

The shape is per client: `httpUrl` for Gemini, `serverUrl` for Windsurf,
`type: "remote"` for opencode, and so on. Run it against a gateway started with
`--per-server`.

```sh
mcpgw sync --gateway --gateway-url http://127.0.0.1:9000/mcp
mcpgw sync --gateway --dry-run
mcpgw sync --rollback              # back to whatever was there before
```

### One entry for the whole gateway

```sh
mcpgw sync --gateway --aggregate
```

Replaces the per-server entries with a single `mcpgw` entry pointing at `/mcp`,
where every tool is namespaced `server__tool`. One entry per client instead of
a dozen, at the price of prefixed tool names and no resources or prompts (see
above). Switching between the two modes is a normal sync either way: the
entries the other mode wrote are mcpgw's, so they are replaced, not left
behind.

## stdio-only clients

Claude Desktop only speaks stdio, so it can't be handed a URL. `mcpgw connect`
is the bridge — a stdio server on one side, an HTTP client to the gateway on
the other:

```sh
mcpgw connect
mcpgw connect --url http://127.0.0.1:9000/mcp
mcpgw connect --server github    # one server's endpoint, tools unprefixed
mcpgw connect --server github --url http://127.0.0.1:9000/mcp
```

`--url` alone is the gateway, verbatim. With `--server` it says where the
gateway is and the server's endpoint is resolved on it — which is what
`sync --gateway` writes for a stdio-only client.

`sync --gateway` writes this for you; you rarely type it. If the gateway isn't
running, the client sees a plain message saying so and telling you to start it
— not a transport error.
