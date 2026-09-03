# Gateway

The gateway is how mcpgw works, not a mode you turn on. `mcpgw sync` points
every client at it: each client talks to mcpgw, and mcpgw talks to the servers.

```text
  Claude Code  ─┐                        ┌─ github    (stdio)
  Cursor       ─┼─→  mcpgw serve  ─────→ ├─ linear    (http)
  VS Code      ─┤    :8137/mcp           └─ postgres  (stdio)
  Claude Desktop┘    (mcpgw connect)
```

Two things fall out of that shape: one connection per upstream instead of one
per client, and a single place where all the traffic is visible. A third is the
cost — the gateway has to be running for a client to reach anything, which is
what [Running as a daemon](./daemon.md) is for, and one process now holds every
server's credentials, which is what the [Trust model](./trust-model.md) is
about.

Earlier versions could also write each server into each client directly,
gateway or no gateway. That choice is gone: entries written straight at the
servers were a second shape to test, a second thing for `doctor` to reason
about, and the source of a silent breakage on clients that cannot hold an HTTP
entry at all. `mcpgw sync` writes gateway entries, and `mcpgw eject` writes the
originals back if you want out.

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
mcpgw serve --capture-bodies off              # timings only, no bodies at all
```

Captured bodies are redacted by default; see
[Watching traffic](./traffic.md).

### Binding

`--bind` defaults to `127.0.0.1`. Anything else prints a warning, because
**there is no authentication** — whoever can reach the address can call
your MCP servers:

```sh
mcpgw serve --bind 0.0.0.0    # warns loudly; keep it behind something
```

A gateway under a service manager refuses the same address outright — see
[Running as a daemon](./daemon.md#binding-loopback-only). What loopback does
and does not buy you is spelled out in the [Trust model](./trust-model.md).

## Tool names

Tools are exposed as `server__tool` — `github__create_issue`,
`linear__list_issues`. Namespacing every tool up front means adding a server
can never rename an existing one, which would otherwise silently break a prompt
that referenced it.

The exception: `--server` with exactly one name turns the gateway into a plain
pipe to that server, tool names untouched.

## Per-server endpoints

Alongside `/mcp`, every served server gets its own endpoint, where its
tools appear under their own names. No flag — serving one implies serving all
of them:

```sh
mcpgw serve
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

## Protocol revisions

The gateway holds two MCP conversations per request — one with your client, one
with the server — and they do not have to be the same revision. All four
combinations work:

|  | server on 2025-11-25 or older | server on 2026-07-28 |
| --- | --- | --- |
| **client on 2025-11-25 or older** | ✓ | ✓ |
| **client on 2026-07-28** | ✓ | ✓ |

What each side gets:

- **A client on 2026-07-28** talks to the gateway the way that revision
  defines: no `initialize`, no session, the protocol version and its own
  identity in each request's `_meta`, and the `Mcp-Method`/`Mcp-Name` headers
  on every POST. `server/discover` is answered per endpoint — with the
  capabilities and the name and version of the server behind that endpoint,
  not the gateway's. A tool that needs something from the user answers
  `resultType: "input_required"`, and that answer, the requests inside it and
  the opaque `requestState` that correlates the retry all cross the gateway
  untouched: it is your client, not the gateway, that can ask you.
- **A client on 2025-11-25 or older** still handshakes with `initialize` and
  still gets a session, and nothing about it changed. Fields belonging to a
  newer revision are not forced on it.
- **A server on 2026-07-28** has no `initialize` to answer. The gateway tries
  the handshake first — nearly every server in the wild is still on it — and
  falls back to `server/discover` when the server says it has no such method.
  `mcpgw doctor --probe` and `mcpgw inspect` follow the same rule.
- **A server on an older revision** is reached exactly as before.

What the gateway advertises for a server is what that server declared, minus
what a pipe cannot deliver: `listChanged` and `resources.subscribe` (change
notifications stop at the gateway — until it forwards `subscriptions/listen`,
promising them would leave a client waiting forever), `logging`, and the tasks
extension. Everything else is forwarded as-is, including capabilities newer
than this version of mcpgw.

`/mcp` serves tools only, and that is deliberate. Tools can be namespaced
(`github__create_issue`); resource URIs and prompt names cannot. Two servers
can both offer `file:///README.md` — one name, two different documents — and
rewriting the URIs would break every link inside the contents that points at
them. So `/mcp` merges what it can merge honestly, and `/s/<name>` is where the
rest lives — which is why it is `/s/<name>` that clients are pointed at.

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

## Upstream lifecycle

Each server gets one connection, multiplexed across every client session — no
process per session. If an upstream dies it's restarted with backoff. If it
keeps failing, calls to it return a loud error rather than the tool quietly
vanishing from `tools/list`, which is the failure mode that costs you an
afternoon.

stdio and HTTP upstreams run through the same lifecycle; from a client's side
they're indistinguishable. With one exception: a remote server that answers
`401` is not retried at all. Its endpoint reports `needs OAuth`, `mcpgw doctor
--probe` says the same thing as a warning rather than a failure, and calls
through it fail naming the login rather than a server that is down — see
[Remote servers and OAuth](./trust-model.md#remote-servers-and-oauth).

## Config reload

A running gateway follows the config file. `mcpgw add`, `remove`, `enable` and
`disable` take effect within a couple of seconds — no restart, and nothing
disconnected:

```sh
mcpgw add github -- npx -y @modelcontextprotocol/server-github
# a moment later, without touching the gateway:
curl http://127.0.0.1:8137/s/github
```

An added server gets its endpoint and joins `/mcp`; a removed or disabled one
loses both and its process is stopped. A server the edit didn't mention is left
completely alone — same connection, same child process — so adding one server
never interrupts the others. Only a change to a server's own transport (its
command, args, env or URL) restarts that server.

Nothing is torn down under a request in flight: a `tools/call` that was already
running when the config changed still gets its answer from the process it
started on.

A server with a
[`headers_command`](./configuration.md#headers_command) is the one case where a
`401` is not the end: its credential is minted per connect, so the command is
rerun once and the connect retried before the server is reported as needing a
login. A `401` on a live call drops that connection instead of latching it, and
the next call reconnects with whatever the command prints then — which is how a
token that expires every hour survives a gateway that has been up for a day.

On Unix, `kill -HUP` the gateway to reload immediately rather than waiting for
the next check. A config file that doesn't parse changes nothing — the gateway
says so and keeps serving what it already had, so a typo can't take your servers
down.

## Pointing clients at it

```sh
mcpgw sync
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
`type: "remote"` for opencode, and so on. Any running `mcpgw serve` answers on
those endpoints.

```sh
mcpgw sync --gateway-url http://127.0.0.1:9000/mcp
mcpgw sync --dry-run
mcpgw sync --rollback              # back to whatever was there before
```

There is no other shape. `sync` has no modes and nothing to choose between:
every enabled server gets its own entry pointing at its own endpoint, in every
client, and that is all mcpgw does to a client config.

```sh
mcpgw sync --project               # the repo's committed files as well
```

`--project` adds the repo-local configs found from your working directory —
`.mcp.json`, `.cursor/mcp.json` and the rest — to the same run, written the
same way and with a diff kept small enough to review. See
[Project-level client files](./configuration.md#project-level-client-files).

Up to 0.3.x there was a second one — `sync --aggregate`, a single `mcpgw` entry
per client pointing at `/mcp`, with every tool namespaced `server__tool`. The
flag is gone. A config that still holds that entry is migrated by the next
plain `mcpgw sync`: the entry was mcpgw's own, so it is removed and the
per-server entries arrive in its place, in one run and without a flag.

### Checking the path clients take

```sh
mcpgw doctor --probe
```

A server that answers when mcpgw spawns it directly tells you nothing about
whether a client can reach it through the gateway, so `--probe` reports the two
separately:

```text
probes — direct to each server
  ✓ github (canonical): github-mcp-server 0.19.1, 41 tools

probes — through the gateway at http://127.0.0.1:8137/mcp
  ✓ http://127.0.0.1:8137/s/github ← Cursor "github", Zed "github": github-mcp-server 0.19.1, 41 tools
```

The second section takes every entry mcpgw wrote into a client, keeps the ones
aimed at the gateway, and runs a real `initialize` and `tools/list` against
that endpoint — the same request the client makes. Entries you wrote by hand
are left alone, and entries pointing somewhere else are not the gateway's
business.

Two failures it exists to name. A gateway that isn't running is one error, not
one per client:

```text
  ✗ not reachable — start it with `mcpgw serve` (3 endpoint(s) not checked)
```

And an entry left over from a server that has since been renamed or disabled,
which is the failure that otherwise shows up as a client silently missing its
tools:

```text
  ✗ Cursor "ghost" points at http://127.0.0.1:8137/s/ghost, which the running
    gateway does not serve — no server endpoint named "ghost" — known
    endpoints: /s/github, /s/linear
```

`--gateway-url` points the check at a gateway on another port, matching
`sync --gateway-url`.

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
gateway is and the server's endpoint is resolved on it — which is what `sync`
writes for a stdio-only client.

`sync` writes this for you; you rarely type it.

If nothing is listening when the bridge starts, and the address is loopback,
`connect` serves a gateway itself for as long as the client keeps the bridge
open, and says so on stderr — which is where the client's MCP log is:

```text
mcpgw connect: no gateway at http://127.0.0.1:8137/s/github; serving one for this session (install a service with `mcpgw daemon install` to keep it running)
```

That gateway is the same one `mcpgw serve` raises — every enabled server, the
aggregate on `/mcp`, a face per server, the config watched for edits — and it
goes away when the client quits, taking your stdio servers with it. It is the
fallback, not the arrangement: `mcpgw daemon install` gives you one gateway
that every client shares and that survives the client restarting.

Two clients launching their bridges at the same moment is fine. Only one can
bind the port; the other notices, waits for it, and bridges to it.

Two cases where the bridge starts nothing. A service is installed on that port
and is not running — the bridge says `the installed service is not running` and
points at `mcpgw daemon start`, because a gateway the supervisor does not know
about would only mask that. And a port held by something that is not a gateway,
which fails the way it always did. Either way the client sees a plain message
saying what to start — not a transport error.
