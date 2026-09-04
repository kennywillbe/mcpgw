# Gateway

The gateway is how mcpgw works, not a mode you turn on. `mcpgw sync` points
every client at it: each client talks to mcpgw, and mcpgw talks to the servers.

```text
  Claude Code  ─┐                        ┌─ github    (stdio)
  Cursor       ─┼─→  mcpgw serve  ─────→ ├─ linear    (http)
  VS Code      ─┤    :8137/s/<server>    └─ postgres  (stdio)
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

Every enabled server, each behind its own endpoint on
`http://127.0.0.1:8137`. Ctrl-C shuts it down and reaps the child processes.

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

## Endpoints

Every served server gets its own endpoint, where its tools appear under their
own names. No flag, and no other shape — serving one implies serving all of
them:

```sh
mcpgw serve
# http://127.0.0.1:8137/s/github — github, tool names untouched
# http://127.0.0.1:8137/s/linear — linear, tool names untouched
# http://127.0.0.1:8137/mcp      — the gateway itself; no server, no tools
```

One client, one server, one endpoint. Tool names are never rewritten, so
adding a server cannot rename another server's tool, and a prompt that names
one keeps working.

### Tool allowlists

An endpoint offers every tool its server has, unless the config says
otherwise:

```toml
[servers.github.tools]
allow = ["search_repositories", "get_file_contents"]
deny  = ["delete_*"]
```

Both sides of the pipe are filtered. `tools/list` returns what survives, and
a `tools/call` on anything else is refused before the request reaches the
server — it never spawns a process and never spends a credential:

```text
tool "delete_repository" is not allowed on server "github" (see mcpgw tools github)
```

Refusals are captured under kind `denied`, so `mcpgw watch` shows what a
client tried to reach and did not get, which is the difference between "the
list works" and "the list is why my agent keeps apologising".

```sh
mcpgw tools github                          # every tool, with allowed/denied
mcpgw tools github allow search_repositories
mcpgw tools github deny 'delete_*'
mcpgw tools github clear
```

The lists follow the config file like everything else — an edit is live
within a couple of seconds, on sessions that are already open, and without
the server behind the endpoint being restarted. A server with no table is
unaffected in every way. The rules, the pattern syntax and what
`doctor --probe` makes of an entry that matches nothing are in
[`[servers.NAME.tools]`](./configuration.md#serversnametools); what the lists
are and are not worth is in the [Trust model](./trust-model.md).

### Tool definition drift

A tool's description and its `inputSchema` are prompt material: the model
reads them and does what they say. A server that rewrites either after you
installed it changes what your agent does on a machine where nothing was
reconfigured — benign at install, instructions added later. Nothing else in
the pipe remembers what the server said last time.

So the first time an endpoint lists a server, the gateway hashes each tool it
is going to hand on — `name`, `description`, `inputSchema`, plus
`outputSchema` and `annotations` where the tool has them — and writes the
hashes to `<state>/pins/<name>.json` (mode `0600`, one file per server, not
per client). Every later list is compared against them. A tool whose hash
moved, a tool that has gone and a tool that has appeared are each a drift
event, and each is reported four ways:

```text
⚠  [s/github] github tools/list create_issue   definition changed, 21 → 384 bytes
```

- a traffic record with `"kind": "drift"` — see [the record
  format](./traffic.md#the-record-format);
- that line in `mcpgw watch`, and its own mark and colour in the TUI;
- a `mcpgw doctor` warning naming the server and the tools, which needs no
  `--probe` because the gateway already did the comparison;
- and a line on the gateway's own stderr.

**Nothing is blocked.** The drifted tool stays in `tools/list` and a call to
it still goes through. See [`drift`](./configuration.md#drift) for why, and
for the `drift = "off"` that turns the whole thing off per server.

What the records never carry is the description itself, only how many bytes
it was before and after. The rewritten text is the payload of exactly this
attack, and copying it into the traffic log would put those instructions into
a second file — one that `mcpgw watch`'s detail pane reads back onto a
screen, and that people paste into issues.

Accepting the new definitions is a command:

```sh
mcpgw tools github                # the listing, with a pinned/changed/new column
mcpgw tools github pin --show     # the pinned hashes and the drift since
mcpgw tools github pin            # accept what the server serves now
mcpgw tools github unpin          # forget them; the next list pins afresh
```

A paginated `tools/list` is not compared. This gateway forwards pagination
rather than collapsing it, so one page is a fraction of the list, and every
tool on the other pages would read as removed. Only a request with no cursor
answered with no cursor is the whole list. The comparison is also made after
the allowlist, on exactly the tools the endpoint hands on.

### Call budgets

An agent in a loop can call one tool a few thousand times before anybody
notices. A server can carry a ceiling:

```toml
[servers.linear]
calls_per_minute = 120
```

A token bucket per server, spent by `tools/call` and refilled at the same
rate: 120 calls may go out back to back, and after that one more arrives
every half second. Over the ceiling the call is refused before it reaches the
server, exactly like a denied tool, and the client is told what to do about
it:

```text
server "linear" is over its budget of 120 calls per minute; retry in ~1 s (see mcpgw tools linear)
```

Naming the wait is the point. An agent told only "no" sends the same call
straight back; an agent told how long can stop, which is the difference
between a circuit breaker and a busier loop.

Refusals are captured under kind `throttled` — its own kind, next to
`denied`, because the two say different things about the client that hit
them — so `mcpgw watch` shows a runaway loop while it is running rather than
after the invoice.

```sh
mcpgw tools linear budget 120
mcpgw tools linear budget off
```

The ceiling is read live, like the tool lists: raising it applies to the next
call, on sessions that are already open and without the server behind the
endpoint restarting. The bucket itself survives that reload; only a change to
the server's transport, which replaces the connection anyway, starts a fresh
one. A server with no `calls_per_minute` is unmetered, which is every server
until you say otherwise.

### The base endpoint

`/mcp` is the gateway's own address rather than a way through it. It answers
who it is — `mcpgw`, and its version — and an empty tool list, and a
`tools/call` against it comes back saying to point the client at
`/s/<name>` instead. It is what `mcpgw doctor` and `mcpgw daemon status` ask
whether a gateway is running, and what `--gateway-url` and `--url` name.

Up to 0.4 it served every server's tools at once under `server__tool` names.
It no longer does. An entry still pointing there is migrated to per-server
entries by the next `mcpgw sync`.

An endpoint is a plain pipe, so it forwards everything an MCP server
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
  untouched: it is your client, not the gateway, that can ask you. The
  `Mcp-Param-*` headers a client mirrors onto a `tools/call` (the parameters a
  server marked `x-mcp-header`) are forwarded to an HTTP server on that one
  request and nothing else of the client's is — not its session, not its
  credential — while a stdio server, which has no headers, never sees them.
- **A client on 2025-11-25 or older** still handshakes with `initialize` and
  still gets a session, and nothing about it changed. Fields belonging to a
  newer revision are not forced on it.
- **A server on 2026-07-28** has no `initialize` to answer. The gateway tries
  the handshake first — nearly every server in the wild is still on it — and
  falls back to `server/discover` when the server says it has no such method.
  `mcpgw doctor --probe` and `mcpgw inspect` follow the same rule.
- **A server on an older revision** is reached exactly as before.

What the gateway advertises for a server is what that server declared, minus
what a pipe cannot deliver:

| capability | forwarded? | why |
| --- | --- | --- |
| `tools.listChanged`, `resources.listChanged`, `prompts.listChanged` | yes | the notification crosses the gateway, both ways the two revisions define it |
| `resources.subscribe` | no | per-resource `resources/updated` needs a subscription per URI upstream, which the gateway does not hold |
| `logging` | no | `logging/setLevel` is not forwarded and `notifications/message` does not cross either; deprecated in 2026-07-28 anyway |
| `io.modelcontextprotocol/tasks` | no | advertising it makes the SDK accept `tasks/get` and friends here, which the gateway answers "method not found" |
| everything else | yes | including capabilities newer than this version of mcpgw |

### Change notifications

A server that adds a tool while a client is connected can now say so, and the
client hears it: the gateway listens for `notifications/tools/list_changed`
(and the resources and prompts ones) on every upstream connection and hands
each one to the sessions that were promised it. How it reaches your client
depends on the revision that client is on — a session on 2025-11-25 or older
gets the notification on its own stream, and a client on 2026-07-28 asks for
the same events with `subscriptions/listen` and gets them on that request's
stream. Nothing has to be configured either way; the gateway follows whatever
the client negotiated.

A config reload that changes a server's transport counts as a change and is
announced the same way. The gateway retires the old process and dials the new
one, and whatever that new connection lists is a different list by definition,
so a client sitting on `/s/<name>` re-reads it instead of holding the old one
until it is restarted.

The gateway only ever promises what the server behind it promised. A server
that declares no `listChanged` is fronted by an endpoint that declares none
either, and a client on such an endpoint is never sent one — nor, on
2026-07-28, offered a `subscriptions/listen` stream at all.

`tools/list` is merged. A server is allowed to hand its tools back a page at a
time and expect the client to follow `nextCursor`, and several widely used
clients — Cursor and Codex among them — simply don't, so every tool past the
first page silently disappears with no error to explain it. The gateway walks
the pages itself and answers with all of them in one list, keeping the first
page's caching fields and `_meta`, so clients that ignore `nextCursor` still
see every tool. A client that does paginate and asks for a second page gets an
empty list, which is the end of its loop. `resources/list`,
`resources/templates/list` and `prompts/list` are still paged through
unchanged: it's tools that clients lose, and a resource list can be far too
long to collapse into one reply.

One caveat: an endpoint reports its server's capabilities as of the last time
it reached that server. A client connecting to a freshly started gateway,
before anything has talked to the server yet, is told "tools" — the
conservative answer — because working it out for real would mean starting the
server in the middle of a handshake. Anything that connects after the first
request through the endpoint sees the full set. Change notifications follow
that rule too: a session opened in that window was promised none, so it is
sent none until it reconnects. The name an endpoint reports at
`initialize` follows the same rule: once the gateway has met the server it
answers with that server's own name and version (`Context7 4.0.4`), which is
what a client shows the user; before first contact, and on `/mcp`, it is
`mcpgw`.

The endpoints share one process and one set of upstream connections, so a
client can take one server or several without starting anything twice. A
stdio-only client reaches one the same way:

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
[Servers that need OAuth](./auth.md).

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
command, args, env or URL) restarts that server: editing a
[`[tools]`](./configuration.md#serversnametools) list or a
[`calls_per_minute`](./configuration.md#calls_per_minute) changes what the
next request sees and nothing else.

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
flag went in 0.4 and the endpoint stopped serving those tools in 0.5. A config
that still holds that entry is migrated by the next plain `mcpgw sync`: the
entry was mcpgw's own, so it is removed and the per-server entries arrive in
its place, in one run and without a flag.

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

## The install token

Every request to `/s/<server>` carries this install's gateway token:

```json
{ "type": "http", "url": "http://127.0.0.1:8137/s/github",
  "headers": { "Authorization": "Bearer l8XzX_K9…" } }
```

`sync` writes it; you never have to. It is minted on the first `mcpgw serve`
or `mcpgw daemon install` and kept at `~/.local/share/mcpgw/gateway.token`,
mode `0600`.

```sh
mcpgw token show                 # masked
mcpgw token rotate               # new token, then re-syncs every client
```

This release still answers a loopback client that has not been re-synced, and
logs one line saying so; `[gateway] require_token = true` ends that. A bare
`GET /mcp` — the liveness probe — is always answered. Zed and Claude Desktop
carry no header for their own reasons, both covered in
[Trust model](./trust-model.md#the-two-clients-that-carry-no-header).

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

That gateway is the same one `mcpgw serve` raises — every enabled server, a
face per server, the config watched for edits — and it goes away when the
client quits, taking your stdio servers with it. It is the
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
