# Quickstart

Type `mcpgw`. That is the quickstart.

```sh
mcpgw
```

On a terminal, a bare `mcpgw` with no arguments is the setup wizard, and it
does the whole of this page one confirmed step at a time. It asks before every
change and writes nothing until you say yes.

## What the wizard does

**1 — looks around.** Which of the thirteen supported MCP clients are installed
here, and how many servers each one holds.

```text
Looking around — 2 MCP clients found.
  Cursor          2 servers  ~/.cursor/mcp.json
  Claude Desktop  1 server   ~/Library/Application Support/Claude/claude_desktop_config.json
  11 other supported clients are not installed here
```

**2 — adopts what they already hold.** Every server it found goes into the
canonical config, once, with the duplicates folded together and every rename
printed.

```text
Importing what your clients already have.
  2 servers to bring in, from Cursor.
  The rest come across as they are: github, linear.
```

**3 — offers to keep the gateway running.** A launch agent, a systemd user
unit, or a Windows service, depending on the machine — and on macOS it warns
about the "Background Items Added" notification *before* it appears, rather
than leaving you to wonder what just asked. Declining is a normal answer:
`mcpgw serve` in a terminal is the same gateway.

**4 — points every client at the gateway** and checks the result: is the
gateway answering, does every enabled server answer through its own endpoint,
and does every entry that was just written point at one that does.

```text
Checking that it actually works…
  ✓ gateway answering at http://127.0.0.1:8137/mcp
  ✓ github  http://127.0.0.1:8137/s/github — 41 tools
  ✓ Cursor  2 entries, all pointing at endpoints that answer
```

It ends by telling you to restart your clients, because no harness re-reads its
MCP config while running.

```sh
mcpgw init          # the same thing, spelled out
mcpgw init --yes    # never prompts: the recommended answer at every step
```

`--yes` is for scripts and agents. It still prints the whole plan, and where a
step needs a decision that cannot be made for you it stops and says which
command to run instead.

Off a terminal — in a pipe, a CI job, a `Dockerfile` — a bare `mcpgw` prints
help and exits 2 rather than opening a wizard nobody can answer.

Once everything is set up, a bare `mcpgw` stops being a wizard and becomes a
status card: how many servers, whether the gateway is answering, which clients
are synced.

## Piece by piece

The steps below are what the wizard does, and each remains the way to do that
one thing on its own — after setup, they are how you keep the list current.

## 1. Adopt what you already have

```sh
mcpgw import
```

`import` reads Claude Desktop, Claude Code, Cursor, VS Code, Gemini CLI,
Codex CLI, opencode, Windsurf, Zed, Cline, Amp and Zoo Code, and pulls every
server it finds into the canonical config. Names
that aren't valid mcpgw names get slugified, and every rename is printed. The
same server configured in three clients is imported once.

Two entries come in with a note attached. A stdio server whose command is not
on this machine — an absolute path into an app you have since removed, say —
is imported **disabled**, so it never reaches the gateway or your clients;
`mcpgw toggle <name>` switches it on once the command is back. And a remote
server that turns out to be at the same URL as one you already have, differing
only in what its headers are set to, is almost always the same server with a
second token: `import` says so and asks whether to keep both copies or just
the one you have. Off a terminal, and under `--yes`, it keeps both — the
answer that cannot cost you an account — and prints the reason anyway.

```sh
mcpgw import --dry-run          # look before you leap
mcpgw import --from cursor      # only one client (repeatable)
mcpgw import --yes              # never prompt; keep canonical on conflict
```

A client entry that differs from a canonical entry you already wrote is a
conflict, and `import` asks what to do with it. There are three answers: keep
the canonical entry as it is, keep both — the canonical entry is untouched and
the client's copy comes in as `<name>-2`, so the gateway serves that one too —
or overwrite the canonical entry with the client's copy. Keeping both is the
answer when the two really are different servers that happen to share a name:
keeping only yours leaves that client's entry unmanaged, talking to its server
directly rather than through the gateway.

`--yes` answers "keep the canonical entry" without asking, and so does a run
whose stdin is not a terminal, so scripts and agents can run `import` knowing
it will neither block nor overwrite anything you wrote by hand. The skipped
entries are still listed in the output.

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

## 4. Point every client at the gateway

```sh
mcpgw sync --dry-run     # the diff, no writes
mcpgw sync               # write it
```

Each enabled server keeps its entry and its name in the client; only the
transport changes, to that server's own endpoint on the gateway
(`http://127.0.0.1:8137/s/<name>`). So the client's list looks the same before
and after, tool names are untouched, and anything the client keeps beside the
entry — Cline's off switch, its auto-approved tools — survives the move. Then
run the gateway, with [`mcpgw daemon install`](./daemon.md) or `mcpgw serve`.

`sync` only rewrites entries mcpgw wrote. Anything you added to a client by
hand is left exactly where it is and reported as unmanaged, with an `import`
suggestion attached. Before each write, the client's config file is copied into
your state directory.

```sh
mcpgw sync --client cursor --client vscode    # a subset
mcpgw sync --gateway-url http://127.0.0.1:9000/mcp  # a gateway somewhere else
mcpgw sync --rollback                         # undo the last sync
```

The first time this moves entries that used to point straight at your servers,
`sync` says so once:

```text
  These entries used to point straight at the servers. They now point at mcpgw,
  which forwards to the same servers — same names, same tools.

  One thing changed: if the gateway isn't running, they won't answer.
  `mcpgw daemon status` tells you, `mcpgw daemon install` keeps it running.

  Undo everything this run did: mcpgw sync --rollback
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

[Gateway](./gateway.md) is what your clients are now talking to,
[Running as a daemon](./daemon.md) is how it stays up, and
[Watching traffic](./traffic.md) is what you get for having it in the middle.
If you decide against all of it, [Backing out](./eject.md) is one command.
