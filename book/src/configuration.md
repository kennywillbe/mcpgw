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

`NAME` must match `[a-z0-9-_]`, and may not contain `__` — that sequence is
reserved as the gateway's `server__tool` separator.

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
| `headers` | table of strings | `{}` | sent on every request |

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
`env` and `headers` are sub-tables, so they go last.

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
