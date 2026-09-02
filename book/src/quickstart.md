# Quickstart

Five minutes, start to finish. Nothing here touches a client file until you run
`sync`, and `sync` shows you the diff first if you ask.

## 0. Or let the wizard drive

```sh
mcpgw            # on a terminal, this is `mcpgw init`
mcpgw init       # the same thing, spelled out
```

Run on a terminal with no arguments, mcpgw walks you through the rest of this
page one step at a time: what it found on your machine, what it would adopt,
whether to run the gateway in the background, and which clients to point at it.
It asks before every change and writes nothing until you say yes.

`mcpgw init --yes` never prompts and takes the recommended answer at each step,
for scripts and for agents. It still prints the whole plan, and where a step
needs a decision that cannot be made for you it stops and says which command to
run instead.

Off a terminal — in a pipe, a CI job, a `Dockerfile` — a bare `mcpgw` prints
help and exits 2 rather than opening a wizard nobody can answer.

Once everything is set up, a bare `mcpgw` stops being a wizard and becomes a
status card: how many servers, whether the gateway is answering, which clients
are synced.

The steps below are what the wizard does, and remain the way to do any one of
them on its own.

## 1. Adopt what you already have

```sh
mcpgw import
```

`import` reads Claude Desktop, Claude Code, Cursor, VS Code, Gemini CLI,
Codex CLI, opencode, Windsurf, Zed, Cline, Amp and Zoo Code, and pulls every
server it finds into the canonical config. Names
that aren't valid mcpgw names get slugified, and every rename is printed. The
same server configured in three clients is imported once.

```sh
mcpgw import --dry-run          # look before you leap
mcpgw import --from cursor      # only one client (repeatable)
mcpgw import --yes              # never prompt; keep canonical on conflict
```

A client entry that differs from a canonical entry you already wrote is a
conflict, and `import` asks what to do with it. `--yes` answers "keep the
canonical entry" without asking, so scripts and agents can run `import`
knowing it will neither block nor overwrite anything you wrote by hand. The
skipped entries are still listed in the output.

Client ids are `claude-desktop`, `claude-code`, `cursor`, `vscode`, `gemini`,
`codex`, `opencode`, `windsurf`, `zed`, `cline`, `cline-cli`, `amp`, `zoo` —
`mcpgw sync --help` prints the current list.

Cline is two ids because it is two installs: the VS Code extension and the
standalone CLI read different files, and nothing keeps them in step. A machine
with both gets both, and `import` folds a server it finds on both into one
canonical entry.

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

The live pass, in two sections. *Direct* spawns or dials every server, runs the
MCP handshake and `tools/list`, and reports name, version and tool count.
*Through the gateway* does the same against the endpoints your synced client
entries actually point at — the path a client takes, which a direct probe says
nothing about. Probes run in parallel with a per-server timeout
(`--timeout SECS`, default 10).

The second section appears once `sync` has written entries pointing at a
gateway; see [Gateway](./gateway.md#checking-the-path-clients-take).

For one server in detail, without a gateway:

```sh
mcpgw inspect github
```

## Next

Now that the list is real, put a gateway in front of it — see
[Gateway](./gateway.md).
