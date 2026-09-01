# Quickstart

Five minutes, start to finish. Nothing here touches a client file until you run
`sync`, and `sync` shows you the diff first if you ask.

## 1. Adopt what you already have

```sh
mcpgw import
```

`import` reads Claude Desktop, Claude Code, Cursor, VS Code, Gemini CLI,
Codex CLI and opencode, and pulls every server it finds into the canonical
config. Names
that aren't valid mcpgw names get slugified, and every rename is printed. The
same server configured in three clients is imported once.

```sh
mcpgw import --dry-run          # look before you leap
mcpgw import --from cursor      # only one client (repeatable)
```

Client ids are `claude-desktop`, `claude-code`, `cursor`, `vscode`, `gemini`,
`codex`, `opencode`.

## 2. Add the rest by hand

```sh
mcpgw add github -- npx -y @modelcontextprotocol/server-github
mcpgw add linear --url https://mcp.linear.app/mcp
```

Everything after `--` is the stdio command, verbatim. `--url` makes it an HTTP
server instead. Useful flags:

```sh
mcpgw add db --env PGHOST=localhost --tag work -- my-mcp-server
mcpgw add staging --url https://x/mcp --header "Authorization=Bearer $TOKEN"
mcpgw add scratch --disabled -- some-server     # in the list, not in use
```

## 3. See the list

```sh
mcpgw list
mcpgw list --json
```

Every command that prints something also speaks `--json`.

To take a server out of rotation without losing its config:

```sh
mcpgw disable scratch
mcpgw enable scratch
mcpgw remove scratch
```

## 4. Push it to every client

```sh
mcpgw sync --dry-run     # the diff, no writes
mcpgw sync               # write it
```

`sync` only rewrites entries mcpgw wrote. Anything you added to a client by
hand is left exactly where it is and reported as unmanaged, with an `import`
suggestion attached. Before each write, the client's config file is copied into
your state directory.

```sh
mcpgw sync --client cursor --client vscode    # a subset
mcpgw sync --rollback                         # undo the last sync
```

## 5. Check it actually works

```sh
mcpgw doctor
```

The static pass: does the canonical config parse, are the names valid, do the
stdio commands resolve on `PATH`, are the URLs well-formed, is any client
holding an entry that can't be represented. Errors exit 1; warnings don't, so
this is safe in CI.

```sh
mcpgw doctor --probe
```

The live pass. It spawns or dials every server, runs the MCP handshake and
`tools/list`, and reports name, version and tool count. Probes run in parallel
with a per-server timeout (`--timeout SECS`, default 10).

For one server in detail, without a gateway:

```sh
mcpgw inspect github
```

## Next

Now that the list is real, put a gateway in front of it — see
[Gateway](./gateway.md).
