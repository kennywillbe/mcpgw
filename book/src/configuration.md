# Configuration reference

One file, TOML, meant to be edited by hand. mcpgw's own write commands go
through a syntax tree rather than a serializer, so your comments and ordering
survive `add`, `remove`, `enable` and `disable`.

## Location

```text
~/.config/mcpgw/config.toml
```

The same path on Linux, macOS and Windows — the dev-CLI convention that git,
gh and ripgrep use, rather than platform-native config directories. Resolution
order:

1. `$MCPGW_CONFIG` — a full path to the file, not a directory. Wins over
   everything.
2. `$XDG_CONFIG_HOME/mcpgw/config.toml`
3. `$HOME/.config/mcpgw/config.toml` (`%USERPROFILE%` on Windows)

A missing file is the normal first-run state, not an error.

## Shape

```toml
# mcpgw canonical config — the single source of truth for your MCP servers.
version = 1

[servers.github]
type = "stdio"
command = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"
```

### `version`

Required, currently `1`. It's read before anything else, so a config from a
future mcpgw fails with "unsupported version" instead of a confusing
field-level parse error.

### `[servers.NAME]`

`NAME` must match `[a-z0-9-_]`, and may not contain `__` — the name is a URL
path segment (`/s/NAME`) and the half before `__` in what `mcpgw watch` prints
for a call, and `__` inside one would make that unreadable.

Common to both transports:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `type` | `"stdio"` \| `"http"` | required | which transport |
| `enabled` | bool | `true` | `false` keeps the entry but skips it everywhere |
| `tags` | list of strings | `[]` | free-form grouping |

`type = "stdio"`:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | string | required | executable, resolved on `PATH` |
| `args` | list of strings | `[]` | passed verbatim |
| `env` | table of strings | `{}` | added to the child's environment |

`type = "http"`:

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `url` | string | required | Streamable HTTP endpoint |
| `headers_command` | list of strings | `[]` | run per connect; its headers win over `headers` |
| `headers` | table of strings | `{}` | sent on every request |
| `auth` | table | — | OAuth client identity; see [Servers that need OAuth](./auth.md) |

#### `[servers.NAME.auth]`

Written by `mcpgw auth login --client-id`, and by hand for the providers that
issue client ids out of band. With no table at all mcpgw picks its own
identity, which is what almost every server wants.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `client_id` | string | — | a client id the provider issued out of band |
| `client_secret_env` | string | — | the environment variable holding its secret, never the secret itself |
| `scopes` | list of strings | `[]` | empty lets the provider's own metadata decide |

```toml
[servers.jira]
type = "http"
url = "https://mcp.atlassian.com/v1/sse"
auth = { client_id = "abc123", scopes = ["read:jira-work"] }
```

`auth` and `headers_command` are mutually exclusive — both fill the
`Authorization` header, so a config with both is refused at parse time rather
than letting one silently win.

#### `[servers.NAME.tools]`

Which of a server's tools clients may reach through its endpoint. Optional,
and absent from every entry until you add it:

```toml
[servers.github.tools]
allow = ["search_repositories", "get_file_contents"]
deny  = ["delete_*"]
```

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `allow` | list of patterns | `[]` | once it has an entry, nothing else is offered |
| `deny` | list of patterns | `[]` | removed from whatever `allow` left |
| `drift` | `"warn"` or `"off"` | `"warn"` | whether changed tool definitions are reported |

A pattern is a literal tool name or a prefix with a trailing `*`
(`delete_*`) — that and nothing more, because a rule language you cannot
predict is the wrong thing to put in front of "which tools can this agent
call".

The rules are read in that order: `allow` first, then `deny` over what is
left, so a broad `allow` can be trimmed without listing every name that
should stay. Deny-by-default starts the moment `allow` has an entry and not
before: **a server with no table, or with two empty lists, offers everything
it always did.**

A filtered-out tool is not in `tools/list` and cannot be called: a
`tools/call` naming it comes back as an error, and the traffic log records
the attempt under kind `denied` — see [Gateway](./gateway.md#tool-allowlists).

The lists are editable without opening the file, and `mcpgw tools NAME` shows
them against the tools the server offers right now:

```sh
mcpgw tools github                                # every tool, allowed or denied
mcpgw tools github allow search_repositories      # add to allow
mcpgw tools github deny 'delete_*'                # add to deny
mcpgw tools github clear                          # remove both lists
```

`mcpgw doctor --probe` reports an entry that matches no tool the server
currently offers — a typo, or a tool that has been renamed since.

##### `drift`

The gateway hashes each tool's definition the first time it lists a server
and reports it when it stops matching:

```toml
[servers.github.tools]
drift = "off"    # this server versions its tools constantly; stop telling me
```

`"warn"` — the default, and what every server without the key gets — writes a
`drift` record to the traffic log, a line in `mcpgw watch` and a `mcpgw
doctor` warning, and keeps serving. `"off"` pins nothing and compares
nothing: no pin file is written for that server, and nothing is reported.

There is deliberately **no `"deny"`**. A gateway that refuses calls because a
description changed refuses them the day a server ships a legitimate new
version, and a check that breaks a working setup is a check people turn off —
which is how MCPProxy's quarantine came to over-report. The value here is
that the change is visible at the moment it happens, in the same stream as
the traffic that follows it. Blocking, if it is ever offered, will be a
separate opt-in and not a default.

The pins live in `<state>/pins/<name>.json`, one file per server, mode
`0600`. See [Tool definition drift](./gateway.md#tool-definition-drift).

#### `calls_per_minute`

A ceiling on how fast clients may call a server through its endpoint. Absent
from every entry until you set it, and unlimited while it is:

```toml
[servers.linear]
type = "http"
url = "https://mcp.linear.app/mcp"
calls_per_minute = 120
```

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `calls_per_minute` | integer ≥ 1 | *(absent)* | `tools/call` ceiling for the whole server |

It is a token bucket, not a fixed window: a server at 120 may burst 120 calls
back to back, and then gets one more every half second. That is the shape
that survives normal work — bursty by nature — while still stopping a loop
dead.

Over the ceiling, a `tools/call` comes back as an error naming the limit and
a wait, and the traffic log records it under kind `throttled` — see
[Gateway](./gateway.md#call-budgets).

`0` is a config error rather than a synonym for "unlimited": it reads equally
well as "refuse everything", and the way to say "no budget" is to have no
key. `mcpgw tools NAME budget off` is what removes it:

```sh
mcpgw tools linear budget 120   # 120 calls per minute
mcpgw tools linear budget off   # unmetered again
```

The budget covers the whole server, not one tool and not one client: what it
protects is the thing on the other end, which cannot tell whose loop is
hammering it.

#### `headers_command`

A token you paste into `headers` lasts as long as the token does. Anything an
SSO, an STS, `gcloud auth print-identity-token` or a Vault lease hands you is
short-lived by design, and this is where it goes instead:

```toml
[servers.internal]
type = "http"
url = "https://mcp.corp.example/mcp"
headers_command = ["corp-auth", "print-mcp-headers"]
```

The command must print a JSON object of header names and values on stdout:

```json
{"Authorization": "Bearer eyJ…", "X-Corp-Tenant": "acme"}
```

That is the same contract Claude Code's `headersHelper` and Codex's
`http_headers_helper` use, and `mcpgw import` maps both onto this field, so a
server that authenticated before it moved behind the gateway still does.
`mcpgw eject` writes them back.

Rules, all of them:

- **It is argv, not a shell line.** `["corp-auth", "print-mcp-headers"]`, not
  `corp-auth print-mcp-headers | jq`. Nothing is expanded, globbed or split by
  a shell — the same treatment `command` and `args` get, and for the same
  reason: a path with a space in it should not become two arguments, and a `;`
  in an argument should not become a second command. A bare string is accepted
  and split on whitespace, because that is how both clients above spell theirs;
  anything that needs quoting is written as an array.
- It runs **on every connect**, and once more if the server answers `401` to
  that connect. Nothing is cached: a helper that wants to reuse a token caches
  it itself.
- Its output is **merged over `headers`**, so a name the command prints
  replaces the one written down. Static headers are the fallback.
- A `401` on a live call is treated as an expired credential: the connection is
  dropped and the next call reconnects, rerunning the command. A rotating
  token costs one failed call, not a restart.
- It gets **10 seconds**, and is killed after that.
- It runs with the process environment inherited and the working directory set
  to your home, never the gateway's — which under a service manager is a
  directory you did not choose.
- **Its output is a credential and is treated as one.** It is never logged,
  never captured, and never quoted into an error. `mcpgw list` shows the
  command; `mcpgw doctor` says the headers come *from command*; a failure
  reports the command line and a tail of its **stderr**, which is the
  diagnostic half.

`mcpgw add` takes it as one line:

```sh
mcpgw add internal --url https://mcp.corp.example/mcp \
  --headers-command "corp-auth print-mcp-headers"
```

**Under a daemon, give an absolute path.** launchd, systemd and the Windows
service manager start the gateway with a `PATH` of their own, which usually
does not include `/opt/homebrew/bin` or `~/.local/bin`. `mcpgw doctor`
resolves the command the same way it resolves a stdio `command` and reports
one that is not found — but a `PATH` that differs between your terminal and
your service is exactly the case a passing `doctor` cannot rule out. See
[Running as a daemon](./daemon.md).

### Everything at once

```toml
version = 1

[servers.postgres]
type = "stdio"
command = "mcp-server-postgres"
args = ["--readonly"]
tags = ["work", "db"]

  [servers.postgres.env]
  PGHOST = "localhost"
  PGDATABASE = "app"

[servers.staging]
type = "http"
url = "https://staging.example.com/mcp"
enabled = false
tags = ["work"]

  [servers.staging.headers]
  Authorization = "Bearer sk-…"
```

Values must come before sub-tables within a section — that's TOML, not mcpgw.
`env`, `headers` and `tools` are sub-tables, so they go last.

## `[capture]`

Optional, and absent from a config that never mentions it. One key today:

```toml
version = 1

[capture]
redact = ["ACME-[0-9]{4}"]

[servers.github]
type = "stdio"
command = "npx"
```

`redact` is a list of [`regex`](https://docs.rs/regex) patterns whose matches
are replaced in the traffic log, on top of the built-in credential rules — the
site-specific shapes only you know about. A pattern that does not compile is a
config error naming the pattern, so a rule can never quietly match nothing.
Read once at gateway startup, so an edit needs a restart. See
[Watching traffic](./traffic.md) for what is redacted without it.

## `[gateway]`

Optional, and absent from a config that never mentions it. One key today:

```toml
version = 1

[gateway]
require_token = true

[servers.github]
type = "stdio"
command = "npx"
```

`require_token` ends the one-release grace period on the gateway's install
token. With it off — the default — the gateway checks the token on every
request but still answers a **loopback** client that carries none, logging one
line per process so the state is noticed before the next release stops
answering it. With it on, the token is required everywhere, and a supervised
gateway may then bind past loopback (`mcpgw daemon install --bind`), which is
refused without it.

Read at gateway startup, so an edit needs a restart. The token itself lives in
the state directory, not here — see
[`mcpgw token`](./trust-model.md#the-token-is-the-authentication-boundary).

## Project-level client files

Several clients read a second MCP config from inside the repository, next to
the code and committed with it:

| Client | File | Key |
| --- | --- | --- |
| Claude Code | `.mcp.json` | `mcpServers` |
| Cursor | `.cursor/mcp.json` | `mcpServers` |
| VS Code | `.vscode/mcp.json` | `servers` |
| Gemini CLI | `.gemini/settings.json` | `mcpServers` |
| Codex CLI | `.codex/config.toml` | `[mcp_servers]`, trusted projects only |
| opencode | `opencode.json` / `opencode.jsonc` | `mcp` |
| Amp | `.amp/settings.json` | `amp.mcpServers` |
| Zoo Code | `.roo/mcp.json` | `mcpServers` |

These are the files a team commits and reviews, so mcpgw only touches them
when you ask for it by name:

```sh
mcpgw import --project     # adopt what the repo's files hold
mcpgw sync --project       # point the repo's files at the gateway
```

Both flags are additive: they do everything the plain command does and then
the repo-local files found from your working directory. Without them nothing
in the repo is read or written, which is exactly what earlier releases did.

`import --project` reads those files under the same rules as a per-user one —
slugified names, cross-client dedupe, and keep / overwrite / keep-both for a
conflict, with `--yes` keeping the canonical entry. Each entry's origin names
the file it came from, because "from cursor" would otherwise mean two
different files.

`sync --project` writes the same gateway entries it writes anywhere else, with
the same guarantees: an entry mcpgw never wrote is never touched, the file is
backed up first, `--dry-run` shows the plan and `--rollback` undoes it.

One thing is different, because these files end up in a pull request. They are
edited through the comment-preserving reader even where the client's own
per-user file is strict JSON, so a sync changes the entries it owns and
nothing else — no reordered keys, no reindented file, and the `//` comment a
teammate wrote above a server is still there afterwards. A sync with nothing
to do writes nothing at all, so re-running it leaves the repo with nothing to
commit.

`mcpgw eject` restores these files along with the per-user ones, without a
flag: what mcpgw wrote is in its record, wherever it is.

`mcpgw doctor` run from inside the repo adds a **project configs** section
listing each file, what it holds, and where each entry stands:

```text
project configs
  /work/api/.mcp.json — Claude Code, 3 servers
      github: managed by sync
      linear: mirrors canonical, not managed
      scratch: not managed: direct entry stays live after sync
  ⚠ /work/api/.mcp.json holds 1 direct MCP entry mcpgw does not manage — …
```

`managed by sync` is an entry mcpgw writes and keeps current.
`mirrors canonical, not managed` is right today and nobody's to keep right —
change the canonical entry and this file will not follow. `--json` carries the
same thing as a `projects` array, with a `managed` flag per entry.

Bookkeeping is per file. A repo's `.cursor/mcp.json` and your own
`~/.cursor/mcp.json` are two records in `managed.json`, so managing one never
claims or deletes anything in the other, and two repos on one machine are
independent. A state file written before this existed loads unchanged and
means what it always meant.

Only the repo root and your working directory are looked at, and never above
the repo root.

## State directory

```text
~/.local/share/mcpgw/
├── managed.json          which server names mcpgw wrote into which file
├── gateway.token         this install's gateway token, mode 0600
├── auth/
│   └── linear.json       OAuth tokens for one server, mode 0600
├── backups/
│   ├── cursor/           timestamped copies, newest 5 kept per file
│   └── cursor-1f0c…/     a repo's .cursor/mcp.json, keyed by its path
└── traffic/
    └── 2026-09-01.jsonl  daily capture log, mode 0600, bodies redacted
```

Resolution order:

1. `$MCPGW_STATE_DIR`
2. `$XDG_DATA_HOME/mcpgw`
3. `$HOME/.local/share/mcpgw`

**`managed.json`** is how `sync` knows what it owns. Deleting it is safe: every
client entry then counts as unmanaged, and `sync` stops touching them until you
re-adopt them with `import`.

**`backups/`** is written before every client file is rewritten. The five most
recent per client are kept; `mcpgw sync --rollback` restores the newest.

**`gateway.token`** is what clients present to the gateway, written into their
entries by `sync`. Deleting it costs you nothing permanent: the next `serve`
mints a new one, and `mcpgw sync` writes that one into the clients. `mcpgw
token rotate` does both in one command. See
[Trust model](./trust-model.md#the-token-is-the-authentication-boundary).

**`auth/`** holds one file per logged-in server — see
[Servers that need OAuth](./auth.md). Deleting one is a logout.

**`traffic/`** is the capture log — see [Watching traffic](./traffic.md).

## Environment variables

| Variable | Effect |
| --- | --- |
| `MCPGW_CONFIG` | full path to the canonical config file |
| `MCPGW_STATE_DIR` | overrides the state directory |
| `MCPGW_NO_UPDATE_CHECK` | any non-empty value switches the version check off, in the CLI and in the installed service alike |
| `XDG_CONFIG_HOME` | base for the config path when `MCPGW_CONFIG` is unset |
| `XDG_DATA_HOME` | base for the state dir when `MCPGW_STATE_DIR` is unset |

The mcpgw-specific variables are ignored when set to the empty string. Setting
the pair of them is the clean way to run mcpgw against a scratch environment:

```sh
MCPGW_CONFIG=/tmp/try/config.toml MCPGW_STATE_DIR=/tmp/try/state mcpgw list
```
