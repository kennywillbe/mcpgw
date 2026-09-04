# Servers that need OAuth

Linear, Notion, Sentry, Atlassian, GitHub's remote server: none of them takes a
token you can paste into a config file. They want an OAuth login, in a browser,
against an authorization server they name themselves.

That login has to happen once, on this machine, and mcpgw is the one that has
to do it:

```sh
mcpgw auth login linear
```

## Why the gateway logs in and your client does not

Every client already knows how to do OAuth. Claude Code has `claude mcp login`,
Codex has `codex mcp login`, Cursor has its own callback, opencode starts a
flow the moment it sees a `401`. All of it is driven by the `WWW-Authenticate`
challenge the server sends with that `401`.

Behind the gateway your client never sees the challenge. mcpgw does not relay
it, on purpose: a client that answered it would complete the upstream's flow
and then send the upstream's access token *through the gateway*, and accepting
a token minted for somebody else is exactly the token passthrough the MCP
specification forbids a resource server to do. The gateway is a resource server
to everything that dials it.

So the challenge stops at the gateway, and the login happens at the gateway —
once, for every client at the same time. What your client sees instead is a
named error:

```text
upstream "linear" needs OAuth; run mcpgw auth login linear on this machine
```

## `mcpgw auth login`

```sh
mcpgw auth login linear            # one server
mcpgw auth login                   # every server that is waiting on a login
mcpgw auth login jira --client-id abc123
mcpgw auth login linear --no-browser
```

What happens:

1. mcpgw dials the server without a credential and reads the `WWW-Authenticate`
   challenge it answers with, which names where the protected-resource metadata
   lives. From there it finds the authorization server and its metadata.
2. It picks a client identity — see below.
3. It binds a listener on `127.0.0.1` with a port the OS picks, builds a PKCE
   `S256` authorization request, and opens your browser. The URL is printed as
   well, always, so a login that opened the wrong browser profile is still one
   copy-paste from finishing.
4. You log in. The authorization server redirects back to the loopback
   listener, which accepts exactly one callback and hands the code to the token
   exchange. The `state` parameter has to match the one this login filed, and
   the `iss` in the redirect has to be the issuer discovery named.
5. The tokens land in `~/.local/share/mcpgw/auth/<name>.json`, mode `0600`.

`--no-browser` skips step 3's browser and prints the URL only. The login waits
five minutes by default (`--timeout SECS`) and then gives up.

**The daemon never opens a browser.** Nothing the gateway links to can: opening
one is in the command you run, not in the code the service runs.

### Which identity mcpgw presents

In the order the [2026-07-28 spec][spec] asks for:

1. **A client id you were given.** `--client-id`, or `client_id` in the
   server's `[auth]` table. Atlassian and GitHub accept nothing else. Passing
   `--client-id` writes it into the config, so later logins and every refresh
   present the same one.
2. **A Client ID Metadata Document**, when the authorization server advertises
   `client_id_metadata_document_supported`. The client id *is* an https URL —
   <https://kennywillbe.github.io/mcpgw/client.json> — which the authorization
   server fetches to learn who is asking. One document for every install of
   mcpgw, the way Claude Code ships one; the document is public and static and
   says nothing about your machine.
3. **Dynamic Client Registration**, when the server offers a
   `registration_endpoint`. Deprecated in 2026-07-28 and still what Notion,
   Sentry and Cloudflare do.

The redirect URI in that document is `http://127.0.0.1/callback` with no port.
[RFC 8252 §7.3][rfc8252] obliges an authorization server to accept whichever
ephemeral port a native client's loopback listener got, which is the only way
a CLI can work at all — a fixed port is a login that fails when something else
is already on it. The loopback *literal* rather than `localhost` is from the
same section: `localhost` can resolve onto an interface that is not the
loopback one.

## `mcpgw auth status`

```console
$ mcpgw auth status
  linear     valid (47m left)  https://auth.linear.app  client id metadata document
  notion     expired  https://api.notion.com  dynamic client registration — run mcpgw auth login notion
  sentry     no login yet — run mcpgw auth login sentry
  context7   static header
```

A server that authenticates with a `headers` entry of its own reads as
`static header`, and one with a `headers_command` as `headers from command`:
they already carry a credential, so there is nothing to log in to and no
login hint. With no http server to say anything about, the whole listing is
one line — `no server needs a login`.

Three states, and only one of them is a problem:

| State | Meaning |
| --- | --- |
| `valid` | the access token has not run out |
| `expired, renews itself` | it has, and a refresh token is stored — the next call renews it with no browser |
| `expired` | it has, and there is nothing to renew it with |

`--json` gives the same rows as objects, each with a `credential` of
`oauth`, `header`, `command` or `none`, and a logged-in one also with
`expires_at`, `issuer`, `client_id`, `identity` and `scopes`.

## `mcpgw auth logout`

Deletes this machine's copy of the tokens. It does **not** revoke them at the
provider — by the time the file is gone there is nothing left to revoke with,
and not every provider offers a revocation endpoint anyway. If the machine is
at risk, revoke the grant in the provider's own settings as well.

## Refresh, and what the gateway does with a token

The gateway attaches the stored access token to every request to that upstream,
and refreshes it when it is nearly out. A refresh that returns a new refresh
token replaces the old one in the file. Concurrent refreshes are serialised
through a lock beside the token file, so two calls arriving as a token expires
produce one refresh, not two — which matters with a provider that rotates
refresh tokens, where the second rotation would invalidate the first.

A `401` that survives a refresh and a retry puts the server back into
`needs OAuth`. The token file is **not** deleted: `mcpgw auth status` and
`mcpgw doctor --probe` need it to tell "the login expired" apart from "there
was never a login", which are different sentences to whoever reads them.

```console
$ mcpgw doctor --probe
  ⚠ linear: linear needs OAuth — the stored login expired; run mcpgw auth login linear
  ⚠ notion: notion needs OAuth — the gateway cannot complete a client-side login; run mcpgw auth login notion
```

## What is stored

`~/.local/share/mcpgw/auth/<name>.json`, mode `0600` in a `0700` directory:

```json
{
  "version": 1,
  "server": "linear",
  "credentials": {
    "client_id": "https://kennywillbe.github.io/mcpgw/client.json",
    "token_response": {
      "access_token": "…",
      "token_type": "bearer",
      "expires_in": 3600,
      "refresh_token": "…",
      "scope": "read write"
    },
    "granted_scopes": ["read", "write"],
    "token_received_at": 1788000000,
    "issuer": "https://auth.linear.app"
  }
}
```

The issuer is recorded next to the tokens and checked before they are used: a
provider that moves to a different authorization server gets a fresh login
rather than a token minted by the old one. The file is the only place any of
this is written — it is never logged, never captured, and never printed by
`auth status`, which shows the issuer and the client id and nothing else.

## `[servers.NAME.auth]`

Only what you have to choose lives in the config; everything discovery finds
stays with the tokens.

```toml
[servers.jira]
type = "http"
url = "https://mcp.atlassian.com/v1/sse"
auth = { client_id = "abc123", client_secret_env = "JIRA_CLIENT_SECRET", scopes = ["read:jira-work"] }
```

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `client_id` | string | — | a client id the provider issued out of band |
| `client_secret_env` | string | — | the **environment variable** holding its secret, never the secret |
| `scopes` | list of strings | `[]` | empty lets the provider's metadata decide |

`auth` and [`headers_command`](./configuration.md#headers_command) are mutually
exclusive: both fill the `Authorization` header, and a config with both is
refused at parse time rather than silently letting one win.

[spec]: https://modelcontextprotocol.io/specification/2026-07-28/basic/authorization/client-registration
[rfc8252]: https://www.rfc-editor.org/rfc/rfc8252.html#section-7.3
