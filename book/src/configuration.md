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

**`mcpgw sync` does not write any of them.** It writes the per-user file only,
so an entry in a repo-local file keeps talking to its server directly — the
gateway is not in that path, and neither is `mcpgw disable`.

`mcpgw doctor` reports them, so at least they are not invisible. Run it from
inside the repo and it adds a **project configs** section listing each file,
what it holds, and whether each entry mirrors something your canonical config
already has:

```text
project configs — not managed by sync yet
  /work/api/.mcp.json — Claude Code, 2 servers
      github: mirrors canonical
      scratch: not managed: direct entry stays live after sync
  ⚠ /work/api/.mcp.json holds 1 direct MCP entry mcpgw does not manage — …
```

`--json` carries the same thing as a `projects` array. `mcpgw import` cannot
read these files yet, so adopting one means copying the entry into the
canonical config by hand for now — see [Roadmap](./roadmap.md).

Only the repo root and your working directory are looked at, and never above
the repo root.

## State directory

```text
~/.local/share/mcpgw/
├── managed.json          which server names mcpgw wrote into which client
├── backups/
│   └── cursor/           timestamped copies, newest 5 kept per client
└── traffic/
    └── 2026-09-01.jsonl  daily capture log, mode 0600
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
| `MCPGW_NO_UPDATE_CHECK` | any non-empty value switches the version notice off |
| `XDG_CONFIG_HOME` | base for the config path when `MCPGW_CONFIG` is unset |
| `XDG_DATA_HOME` | base for the state dir when `MCPGW_STATE_DIR` is unset |

The mcpgw-specific variables are ignored when set to the empty string. Setting
the pair of them is the clean way to run mcpgw against a scratch environment:

```sh
MCPGW_CONFIG=/tmp/try/config.toml MCPGW_STATE_DIR=/tmp/try/state mcpgw list
```
