# Trust model

mcpgw now sits between your harnesses and your MCP servers by default: every
client entry it writes points at the gateway, and every call your agent makes
goes through one process on your machine. That is worth being explicit about.
This page is what mcpgw actually does and does not protect, with no marketing
in it.

## Loopback is the authentication boundary

The gateway has no authentication. It listens on `127.0.0.1:8137`, and
anything that can open a socket there can call every server you have
configured.

That sounds worse than it is, and the reason is worth stating plainly: a
process running as you could already do all of it. Your MCP server
credentials sit in `~/.cursor/mcp.json`, `~/.claude.json`, the Claude Desktop
config and a dozen files like them, in plaintext, readable by anything with
your uid. A process that wanted your Linear token did not need a gateway — it
needed `cat`. mcpgw reaching those same servers over loopback is not a new
door into your account; it is the same door, with a socket on it.

So the boundary mcpgw relies on is the user account. Loopback keeps the
gateway inside it. That is the whole of the access control, and everything
below is about the ways that boundary can be widened or the blast radius
behind it can grow.

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

```sh
mcpgw serve --bind 0.0.0.0     # warns loudly, then does it
```

There is no authentication, so this hands your MCP servers — and the
credentials behind them — to anything that can reach that address. The
warning is real.

A gateway under a service manager refuses the same address outright rather
than warning:

```text
$ mcpgw daemon install --bind 0.0.0.0
Error: refusing to run an unattended gateway on 0.0.0.0: it has no
authentication, so anyone who can reach that address could call your MCP
servers …
```

The difference is deliberate. A warning works when a person is reading a
terminal and can decide; an unattended service prints its warning into a log
nobody reads and then keeps answering for weeks. Loopback there is
`127.0.0.0/8`, `::1` and `localhost`. If the gateway genuinely has to be
reachable from another machine, put something that authenticates in front of
it.

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
  your client configs, `managed.json`, the traffic logs, the daemon logs — is
  `0600`. Those backups are copies of files that hold tokens, which is why
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

- Anything running as you can use the gateway. That was already true of your
  server credentials.
- `[servers.NAME.tools]` narrows what any of them can call on a server. It
  shrinks the blast radius; it does not authenticate anybody.
- One process now holds all of them, and one log now records every call.
- The log redacts what looks like a credential; that is a filter, not a
  proof. `--capture-bodies off` and `--no-capture` are the stronger answers.
- Do not `--bind` past loopback without putting authentication in front.
- A short-lived credential belongs in `headers_command`, not in `headers`.
- Nothing here is a substitute for not running MCP servers you do not trust.
