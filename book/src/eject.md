# Backing out

`mcpgw eject` puts every client back the way it was before mcpgw. Your Cursor,
Codex and Claude Desktop entries go back to pointing straight at your servers,
the gateway entry disappears, and the daemon comes out with them.

```sh
mcpgw eject
```

It exists so that adopting mcpgw is never a one-way door. If the gateway isn't
for you, one command undoes it — and you don't need mcpgw installed afterwards
for your clients to keep working.

## What it writes

Your canonical config still holds every server as you originally defined it —
the command, the args, the env, the URL and headers. Syncing never replaced
those; it only changed what your *clients* were pointed at. Eject
writes the originals back under the same names, so a gateway entry is a plain
update over the entry it already occupies, not a remove and an add.

The gateway token goes with them. `sync` wrote it into each entry as an
`Authorization: Bearer` header, and eject writes the original definition back
over that entry — headers and all — so nothing is left holding a credential
for a gateway the client no longer talks to. Nothing hunts for it: it was
never part of your server definition, so restoring the definition removes it.

Everything else in the file is left alone: entries mcpgw never wrote, other
settings, comments, formatting.

Repo-local files come with it. Anything `mcpgw sync --project` wrote is in
mcpgw's record along with its path, so eject restores those files too — no
flag, and including a repo you are not standing in, because a committed entry
pointing at a gateway nobody runs any more is exactly the leftover this
command exists to remove. See
[Project-level client files](./configuration.md#project-level-client-files).

## Show and confirm

Like every command that touches your files, eject prints the whole plan first
and asks once:

```text
mcpgw eject — putting every client back the way it was.

Cursor — 2 entries restored, 1 removed
  ~ github back to your own definition
  ~ linear back to your own definition
  - mcpgw removed (mcpgw put it there)
  ? mine (not mine — left untouched)

Every file is backed up before it is written, and `mcpgw sync --rollback`
undoes this run like any other.
restore these clients? [Y/n]
```

`--yes` skips the question; the plan still prints.

While that question waits, eject holds nothing: a `mcpgw sync` or `mcpgw add`
in another window runs as usual rather than blocking on an unanswered
terminal. The other side of that is that the files can move under the
question, so eject reads them again once you answer and writes what it finds
then. If the plan that comes back is not the one you were shown, eject stops
without writing and asks you to run it again against the current plan.

Eject writes through the same machinery as `mcpgw sync`, so it takes the same
backups — `mcpgw sync --rollback` restores each client from the snapshot taken
just before eject wrote it.

## The daemon

If a gateway service is installed, eject names it and offers to remove it in
the same run:

```text
A gateway service is installed under launchd (~/Library/LaunchAgents/io.mcpgw.gateway.plist).
remove it as well? [Y/n] y
  removed it — your config and captured traffic are untouched
```

Nothing installed, or a platform whose installer hasn't shipped yet, is one
dimmed line and no question.

## What eject does not delete

Your data is yours. Eject rewrites client configs and stops there — the
canonical config, the state directory (gateway token, OAuth logins, backups
and traffic log included) and the binary all stay, and the closing
screen names them so a full uninstall is three deletions you make yourself:

```text
Nothing of yours was deleted. To remove mcpgw entirely, delete these yourself:
  config   ~/.config/mcpgw/config.toml
  state    ~/.local/share/mcpgw   (backups, logs, captured traffic)
  binary   brew uninstall kennywillbe/tap/mcpgw
```

The binary line matches how mcpgw was installed — `cargo uninstall mcpgw` for a
cargo install, the path to delete for a downloaded archive.

Keeping the config is also what makes the decision reversible: run `mcpgw`
again and the wizard puts everything back.

## Edge cases

- **Nothing was ever synced.** Eject says `nothing to eject` and exits 0
  without touching a file.
- **A client config was deleted by hand.** It's reported and skipped; eject
  never recreates a config someone removed.
- **No canonical config.** Eject stops with an error: the original definitions
  live there, and without them there is nothing to restore. `mcpgw import`
  pulls what a client still holds back into the config first, and
  `mcpgw sync --rollback` restores clients from their most recent backup.
- **An HTTP server on Claude Desktop.** Claude Desktop only speaks stdio, so
  that entry couldn't reach an HTTP server before mcpgw either. Eject writes
  back exactly what your config says, unchanged — a faithful restore, not a
  quiet repair.
