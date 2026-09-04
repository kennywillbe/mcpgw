# Trust model

mcpgw now sits between your harnesses and your MCP servers by default: every
client entry it writes points at the gateway, and every call your agent makes
goes through one process on your machine. That is worth being explicit about.
This page is what mcpgw actually does and does not protect, with no marketing
in it.

## The token is the authentication boundary

Every mcpgw install has a gateway token: 32 bytes from the OS random source,
base64url, written once to `~/.local/share/mcpgw/gateway.token` mode `0600`
the first time `mcpgw serve` or `mcpgw daemon install` runs. `mcpgw sync`
writes it into every client entry it manages as an `Authorization: Bearer`
header, and the gateway checks it on every `/s/<server>` request.

```sh
mcpgw token show                 # masked
mcpgw token show --show-secrets  # in full
mcpgw token rotate               # new token, then re-syncs every client
```

Loopback is still the default and still does most of the work. `mcpgw serve`
binds `127.0.0.1:8137` unless you say otherwise, and nothing about the token
changes what a process running as you can do — it can read the token file
just as it can read `~/.cursor/mcp.json`. What the token defends is the
**port**: a bind past loopback, a container sharing the host's network, a
second account on a multi-user box. Before it existed there was nothing to
put in front of those, which is why `mcpgw daemon install --bind 0.0.0.0` was
refused outright.

The reason it exists now is [`mcpgw auth login`](./auth.md). Until the gateway
held OAuth refresh tokens, it held nothing your client configs did not: your
MCP server credentials sit in `~/.cursor/mcp.json`, `~/.claude.json`, the
Claude Desktop config and a dozen files like them, in plaintext, readable by
anything with your uid. A process that wanted your Linear token did not need a
gateway — it needed `cat`. A refresh token minted through a browser flow is
different: it lives in `~/.local/share/mcpgw/auth/` and nowhere else, so
reaching the port became worth strictly more than reading every client config
on the machine.

**What the token does not protect.** A process running as you can read the
token file, so it can present the token. So can anything that can read the
client configs mcpgw wrote it into. The boundary is still your user account;
the token is what extends that boundary across a socket instead of relying on
the socket being unreachable.

### One release of grace

This release still answers a **loopback** request that carries no token, or
the wrong one, and logs one line the first time it does:

```text
warning: clients without a token: run mcpgw sync (this release still answers
them; the next will not)
```

A request from anywhere else without the token gets `401` with
`WWW-Authenticate: Bearer` — bare, with no `realm` and no
`resource_metadata`: this is a static string, not OAuth, and there is no
authorization server for a client to go and discover. `mcpgw doctor` reports
a managed entry with no token as a warning.

To end the grace period now:

```toml
[gateway]
require_token = true
```

Then the token is required on every request, loopback included.

**The one thing that stays open** is a bare `GET /mcp` — the liveness probe
`mcpgw daemon status`, `mcpgw doctor` and the `connect` bridge use to ask
whether anything is listening. It reaches no server and returns nothing of
yours, and a `status` that cannot answer on the machine whose token file is
unreadable is a `status` that is useless exactly when it is needed. Every
other request on `/mcp`, and every request under `/s/`, goes through the
check.

### The two clients that carry no header

Eleven of the thirteen clients hold the token in the entry `sync` writes.
Two do not:

- **Zed.** Its `context_servers` remote entry is documented as a URL and
  nothing else, and nothing has confirmed that a header written into one
  reaches the request. An entry holding a header its client silently drops
  reads as authenticated everywhere and fails at the first call, which is
  worse than one that plainly carries none — so Zed entries stay
  loopback-only, and `mcpgw doctor` does not warn about them, because there is
  no fix to point at.
- **Claude Desktop.** It has no remote entry shape at all, so `sync` writes an
  `mcpgw connect` bridge — which reads the token off the state directory
  itself. That is the better half of the deal: the secret stays in one `0600`
  file instead of being copied into a config the client owns.

A Zed that has to reach a gateway past loopback goes through the bridge too.

## What the flip actually changed

Pointing every client at the gateway does change two things, and neither is
about who can reach what.

**Aggregation.** Before, each harness held its own credentials and spoke to
its own servers. Now one process holds the whole set: every token in your
canonical config is loaded by `mcpgw serve`, and any client that reaches the
gateway can use any of them. The credentials did not become more exposed —
they were already on the disk — but they became reachable through one place
rather than thirteen.

What narrows that is the per-server allowlist. `[servers.NAME.tools]` says
which of a server's tools its endpoint offers at all, and the gateway
enforces it on both sides: a tool outside the list is not in `tools/list`,
and a `tools/call` naming it is refused before the request reaches the
server. It is a reduction in blast radius rather than an authentication
boundary — anything that can reach the gateway can still reach every server
under its own endpoint, and a client that can edit your config can lift the
list — but it is what keeps a project's endpoint from being able to delete a
repository because some other project needed that tool. The lists are
opt-in, and a server without one offers everything it always did:

```sh
mcpgw tools github                             # what this server's endpoint offers
mcpgw tools github deny 'delete_*'             # and what it no longer will
```

Every refusal is written to the traffic log under kind `denied`, so
`mcpgw watch` shows what a client tried to reach and did not get. See
[`[servers.NAME.tools]`](./configuration.md#serversnametools).

**Tool definitions.** A server's tool descriptions and input schemas are
prompt material — the model reads them and obeys them — and a server can
rewrite them at any time, on a machine where you reviewed it weeks ago and
changed nothing since. Nothing about that shows up in a version number, and
for a remote URL there is not even one to pin.

So the gateway remembers. The first time an endpoint lists a server it hashes
each tool's `name`, `description` and `inputSchema` (plus `outputSchema` and
`annotations` when present) into `<state>/pins/<name>.json`, and every later
list is compared against it. A description that changed, a tool that vanished
and a tool that appeared are each reported: a `drift` record in the traffic
log, a marked line in `mcpgw watch`, a `mcpgw doctor` warning.

It warns and does not block, deliberately — servers do version their tools,
and a gateway that refuses calls over a description change is one people turn
off. So this is detection, not prevention: it tells you the definitions moved
while you can still act on it, in the same stream as the calls that follow.
It says nothing about whether the *new* definition is malicious, and a server
that was hostile the first time you listed it is pinned as hostile.

Note that the records carry the description's length and never its text. The
rewritten string is the attack, and the traffic log is a file people `cat`
and paste — and that `mcpgw watch` reads back onto a screen next to an agent.

```sh
mcpgw tools github pin --show   # what was pinned, and what has moved since
mcpgw tools github pin          # accept what it serves now
```

`calls_per_minute` bounds the same endpoint in the other direction: how
many calls, rather than which ones. It is a circuit breaker for a runaway
agent — the thing that turns a broken loop into a hot laptop, a metered bill
or an account-level rate limit that takes your other tooling down with it —
and explicitly **not** a security boundary. It stops nothing an attacker
would do; it caps what a mistake costs while you are not at the desk.
Refusals are logged under kind `throttled`. See
[`calls_per_minute`](./configuration.md#calls_per_minute).

Per-client scoping (`[clients.KIND]`) narrows the same thing one step
further — which client is offered which servers and tools — and it is worth
being explicit about what it is: **a scope is per client file, not per
caller.** `sync` writes a scoped client's entries pointing at
`/s/<server>?client=<kind>`, and the gateway applies that client's rules to
requests that arrive with the tag. Nothing stops anything else on the machine
from dialing the same endpoint without the tag, or with somebody else's. The
tag says which client config a request came from; it does not prove it. What
scoping buys is a smaller context and a smaller blast radius per harness, not
a boundary — the boundary is still that anything running as you can reach the
gateway.

**One log.** Every call now passes a single capture point, and by default it
is written down. That is the feature — it is why `mcpgw watch` can show you
what your agent did — and it is also a file that did not exist before. See
below.

## Captured traffic is redacted, not private

The capture log records each request's arguments, the response and the error
text, and **redacts before it truncates**. Keys named like credentials,
`Bearer`/`Basic` values, known issuer prefixes (`ghp_`, `sk-`, `AKIA…`, JWTs),
credential-looking URL query values and high-entropy tokens are replaced on
the way to the disk; only what is left is then cut at 2 KB.

```sh
mcpgw serve                        # --capture-bodies redacted (default)
mcpgw serve --capture-bodies off   # metadata only — no bodies at all
mcpgw serve --capture-bodies full  # verbatim
mcpgw serve --no-capture           # no traffic log at all
```

That is a filter over shapes, not a proof. A secret with no marker and low
entropy — a short passphrase, a PIN, a sentence that happens to be the
password — looks like ordinary text and stays in the file. The rest of the
answer is the same as it always was: the file is mode `0600` under your state
directory, `mcpgw watch` does not put bodies back on your terminal, and `off`
and `--no-capture` are there for anyone who would rather not have the file.

[Watching traffic](./traffic.md) lists every rule, and how to add your own
under `[capture] redact`.

## Binding anywhere else

Loopback is still the default, and a bind past it is still something you have
to ask for twice.

```sh
mcpgw serve --bind 0.0.0.0     # warns loudly, then does it
```

A foreground `serve` warns and proceeds, as it always has. Without
`require_token` the warning is the old one — the grace period means an
unauthenticated request still gets through, and on `0.0.0.0` that is your MCP
servers handed to anything that can reach the address. With `require_token`
the warning says what it now is: reachable by anyone who can route to it *and*
holds this install's token.

A gateway under a service manager is stricter, because a warning it prints
goes into a log nobody reads and it then keeps answering for weeks. It
refuses the address unless the clients actually authenticate:

```text
$ mcpgw daemon install --bind 0.0.0.0
hint: a gateway whose clients authenticate may bind anywhere — set
`[gateway] require_token = true` in your config, run `mcpgw sync` so every
client carries this install's token, then install again
Error: refusing to run an unattended gateway on 0.0.0.0: it has no
authentication …
```

Both halves are required, and neither is enough alone. A token with the grace
period still running is not a boundary — an unauthenticated loopback request
still passes — and `require_token` on a machine with no token file is a rule
with nothing to enforce. Loopback there is `127.0.0.0/8`, `::1` and
`localhost`.

`mcpgw doctor` reports a gateway bound past loopback that does not require its
token as an **error**, whether the address came from `daemon install` or from
a foreground `serve`. If the gateway is exposed to a network you do not
control, a bearer token over plain HTTP is the floor, not the ceiling: put TLS
and something that authenticates in front of it.

## The `Origin` check

Binding to loopback is not protection against a browser. Under DNS rebinding
a hostile page's own domain resolves to `127.0.0.1`, which makes its requests
same-origin and lets it `POST /s/<server>` with no CORS preflight — a web
page driving your MCP servers.

So the gateway rejects any request whose `Origin` header is not a loopback
page (`http(s)://localhost`, `127.0.0.1` or `[::1]`, with any port) with
`403`. The `null` origin a `file://` page sends is rejected too. Real MCP
clients send no `Origin` at all and are unaffected.

## What is on disk, and who can read it

- The state directory is `0700`; everything mcpgw writes into it — backups of
  your client configs, `managed.json`, the traffic logs, the daemon logs, the
  gateway token — is `0600`. Those backups are copies of files that hold tokens, which is why
  they get the same treatment as the traffic log.
- The canonical config holds your `env` values and headers in plaintext, the
  same way every client config already does. `mcpgw list --json` masks them;
  `--show-secrets` prints them.
- `mcpgw eject` puts the original definitions back into every client, so the
  state above is not a lock-in — see [Backing out](./eject.md).

## Remote servers and OAuth

A remote MCP server that requires OAuth is logged into once, on this machine,
with `mcpgw auth login <name>` — see [Servers that need OAuth](./auth.md) for
the whole of it. mcpgw runs the flow, holds the refresh token and renews the
access token on your behalf. A server whose token is a fixed string still goes
in `headers` and is forwarded as it always was.

What that means for what mcpgw holds:

- The refresh tokens are the reason the gateway authenticates its clients at
  all — see [above](#the-token-is-the-authentication-boundary).
- The tokens live in `~/.local/share/mcpgw/auth/<name>.json`, mode `0600`
  inside the `0700` state directory, one file per server. They are never
  logged, never captured — the traffic log has always redacted
  `Authorization` — and never printed, by `auth status` or by anything else.
- The refresh token is the long-lived half and is stored beside the access
  token, because renewing without a browser is the whole point. `mcpgw auth
  logout <name>` deletes both; it cannot revoke the grant at the provider,
  so revoke it there too if the machine is at risk.
- The issuer that minted a token is recorded with it and checked before it is
  presented, so a provider that moves to another authorization server gets a
  fresh login instead of a token offered to a stranger.
- mcpgw identifies itself with a public, static [Client ID Metadata
  Document](https://kennywillbe.github.io/mcpgw/client.json) — one document for
  every install. The authorization server fetches it; it says who mcpgw is and
  nothing about your machine.
- **Your client's own token is never passed through.** The gateway presents the
  token it holds for the upstream, and only that one.

A server that answers `401` and has no stored login is a server mcpgw stops at,
and it says so. The endpoint reports `needs OAuth`, `mcpgw doctor --probe`
reports a warning rather than a failure, and a call through the endpoint comes
back as `upstream "linear" needs OAuth; run mcpgw auth login linear on this
machine`.

The `WWW-Authenticate` challenge the server sent is not relayed to your
client. Passing it on invites the client to run the flow and send the
resulting token back through the gateway, which is the token passthrough the
MCP spec forbids a resource server to accept — and the gateway is a resource
server to everything that dials it. The `401` is not retried either: it will
still be a `401` on the third attempt, so it is reported at once instead of
after a backoff ladder.

A credential that expires can be minted per connect instead of written down —
see [`headers_command`](./configuration.md#headers_command). What that command
prints is treated as a credential end to end: it goes to the transport and
nowhere else, and no log line, capture record or error message carries it. The
command line itself is not a secret and is shown; so is a tail of its stderr,
because a helper that fails has to be fixable. mcpgw runs it with no shell.

## The short version

- Anything running as you can use the gateway: it can read the token file. That
  was already true of your server credentials.
- The gateway token is what carries that boundary across a socket. Loopback is
  still the default; the token is what makes anything else defensible.
- `[servers.NAME.tools]` narrows what any of them can call on a server, and
  `[clients.KIND]` narrows what one client is offered. Both shrink the blast
  radius; neither authenticates anybody, and the `?client=` tag is a label a
  client file carries, not a credential.
- Tool definitions are pinned on first sight and a change is reported, not
  refused. It makes a rug pull visible; it does not stop one.
- `calls_per_minute` caps how fast they can call it. It bounds a runaway
  loop; it is not a security boundary either.
- One process now holds all of them, and one log now records every call.
- The log redacts what looks like a credential; that is a filter, not a
  proof. `--capture-bodies off` and `--no-capture` are the stronger answers.
- Do not `--bind` past loopback without `[gateway] require_token = true`, and
  not over an untrusted network without TLS in front.
- A short-lived credential belongs in `headers_command`, not in `headers`.
- Nothing here is a substitute for not running MCP servers you do not trust.
