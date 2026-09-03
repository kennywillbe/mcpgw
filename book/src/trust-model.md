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

**One log.** Every call now passes a single capture point, and by default it
is written down. That is the feature — it is why `mcpgw watch` can show you
what your agent did — and it is also a file that did not exist before. See
below.

## Captured traffic is truncated, not redacted

The capture log records each request's arguments and the response, **cut at
2 KB and marked `…[truncated]`, with nothing removed**. A token passed as a
tool argument lands in that file in full.

```sh
mcpgw serve --no-capture      # no traffic log at all
```

The file lives under your state directory at mode `0600`, and `mcpgw watch`
masks `args` and `response` in `--json` output unless you pass
`--show-secrets` (the human view never printed them). That bounds how far the
bodies spread; it does not change what is in the file. `--no-capture` is the
switch, and [Watching traffic](./traffic.md) is the longer version of this
paragraph.

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
same-origin and lets it `POST /mcp` with no CORS preflight — a web page
driving your MCP servers.

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

A remote MCP server that requires OAuth is authenticated the way it always
was: with a token you put in the config as a header. mcpgw forwards it. It
does not broker the flow, hold a refresh token, or renew anything on your
behalf.

## The short version

- Anything running as you can use the gateway. That was already true of your
  server credentials.
- One process now holds all of them, and one log now records every call.
- The log is not redacted. `--no-capture` turns it off.
- Do not `--bind` past loopback without putting authentication in front.
- Nothing here is a substitute for not running MCP servers you do not trust.
