# Running as a daemon

`mcpgw serve` holds a terminal. That is fine while you are trying the gateway
out and wrong the moment you depend on it: the first thing an MCP client does
in the morning is ask for a tool list, and nothing is there to answer.

`mcpgw daemon` is the answer — the gateway supervised by the machine's own
service manager, started at login and restarted when it dies.

Since every client entry mcpgw writes points at the gateway, this is not a
nicety: a gateway nobody started is a client with no servers. The setup wizard
offers to install it for that reason, and `mcpgw daemon install` is the same
step on its own.

The one client that gets by without it is a stdio-only one, because `mcpgw
connect` serves a gateway of its own when it finds nothing listening on
loopback — for that client, for as long as it stays open. That is a fallback
and it costs you what a service buys: the gateway restarts every time the
client does, its stdio servers are started and stopped along with it, and a
second client gets no gateway at all unless its bridge happens to be running.
Where a service *is* installed, `connect` never starts a rival for its port —
it says the service is not running and leaves `mcpgw daemon start` to you.

Every command works on all three platforms, each through that platform's own
supervisor: a launch agent on macOS, a systemd user unit on Linux, a service
on Windows. The three sections below are what is different about each; the
ones after them are the same everywhere.

## macOS: the launch agent

```sh
mcpgw daemon install            # or --port 9000 --bind ::1
```

```text
installed the mcpgw gateway service at ~/Library/LaunchAgents/io.mcpgw.gateway.plist
  macOS will show a "Background Items Added" notification and list mcpgw under
  System Settings › General › Login Items & Extensions — leave it enabled, or the
  gateway will not come back at your next login
  it serves ~/.config/mcpgw/config.toml and runs with the PATH you installed from,
  so re-run `mcpgw daemon install` if either moves
  its output goes to the daemon logs — `mcpgw daemon logs --follow` reads both streams
it will answer on http://127.0.0.1:8137/mcp
```

The notification is the part worth reading twice. macOS announces every new
login item, and the entry it adds is switchable — turn mcpgw off there and
the gateway stops coming back at login, with nothing in mcpgw to say why.

What gets installed is a plain launch agent: a plist in
`~/Library/LaunchAgents`, loaded into your login session with `launchctl
bootstrap`. It is readable, and it is the whole story — `cat` it if you ever
wonder what the daemon is running.

Two things in it are decisions rather than defaults:

- **`KeepAlive` is a dictionary, `SuccessfulExit = false`.** A gateway that
  crashes comes straight back; a gateway you stopped stays stopped. The bare
  `KeepAlive = true` most generators emit cannot tell those apart, and
  restarts the one you just asked it to stop.
- **`PATH` is captured at install time.** A launch agent otherwise starts with
  `/usr/bin:/bin:/usr/sbin:/sbin`, and almost every stdio MCP server is an
  `npx`, `uvx` or `bunx` living somewhere else — so the gateway would come up
  with every stdio server failing to spawn. The cost is that the `PATH` is
  frozen: change it, or move your config, and re-run `install`. `mcpgw add`
  and `mcpgw doctor` say so when a command stops resolving under it — see
  [After your PATH moves](#after-your-path-moves).

One thing to avoid: a launch agent cannot read through `~/Desktop`,
`~/Documents` or `~/Downloads` unless it has been granted Full Disk Access,
and it has no way to ask for one. A binary that lives in any of those does
not fail to start — it hangs before it runs, so the service reports itself
running, the logs stay empty and nothing ever listens. The same applies to
any stdio server whose command resolves into one of those folders. `install`
warns when it sees either and installs anyway (the grant may already be
there, and nothing in the API says whether it is). A Homebrew or `cargo
install` path is never affected; a `target/release` build inside a Desktop
clone always is.

The rest of the commands do what they say:

```sh
mcpgw daemon stop        # unloads the job; the plist stays, so status says "stopped"
mcpgw daemon start       # loads it again, on the port it was installed with
mcpgw daemon uninstall   # unloads it and deletes the plist
```

`stop` unloads the job rather than signalling it, because a signalled gateway
is a gateway that did not exit successfully — which is exactly what
`KeepAlive` restarts on. `start` runs the plist as it stands, so changing the
port means running `install` again rather than `start --port`.

## Linux: the systemd user unit

```sh
mcpgw daemon install            # or --port 9000 --bind ::1
```

```text
installed the mcpgw gateway service at ~/.config/systemd/user/mcpgw.service
  user lingering is off, so the gateway stops when your last session ends —
  `loginctl enable-linger you` changes that, and mcpgw does not run it for you
  because it outlives every user service you have, not just this one
  it serves ~/.config/mcpgw/config.toml and runs with the PATH you installed from,
  so re-run `mcpgw daemon install` if either moves
  its output goes to the daemon logs — `mcpgw daemon logs --follow` reads both streams
it will answer on http://127.0.0.1:8137/mcp
```

It is a *user* unit, not a system one: the gateway runs your MCP servers with
your environment and reads config out of your home directory, so nothing
about it wants root. Everything is `systemctl --user`, which means
`systemctl --user status mcpgw.service` and `journalctl --user -u mcpgw` work
on it exactly as you would expect. Install writes the unit, reloads the user
manager, enables it and restarts it, so the gateway is up before the command
returns and comes back at your next login — and a reinstall over a running
unit really does hand the port to the binary the new unit names.

The unit is short and worth reading:

```ini
[Unit]
Description=mcpgw MCP gateway
Documentation=https://github.com/kennywillbe/mcpgw

[Service]
Type=simple
Environment=MCPGW_CONFIG=/home/you/.config/mcpgw/config.toml
Environment=MCPGW_STATE_DIR=/home/you/.local/share/mcpgw
Environment=PATH=/home/you/.local/bin:/usr/local/bin:/usr/bin:/bin
ExecStart=/home/you/.local/bin/mcpgw serve --bind 127.0.0.1 --port 8137 --supervised
Restart=on-failure
RestartSec=2
StandardOutput=append:/home/you/.local/share/mcpgw/logs/daemon.out.log
StandardError=append:/home/you/.local/share/mcpgw/logs/daemon.err.log

[Install]
WantedBy=default.target
```

Four of those lines are decisions rather than defaults:

- **`--supervised` on the serve command.** It is what makes an upgrade of
  the binary itself get picked up without a reinstall — see *After an
  upgrade* below. It appears here rather than in anything you type, because
  it only makes sense under something that will start the gateway again.
- **`Type=simple`, and no readiness notification.** sd-notify would mean a
  dependency and a socket protocol to assert something `mcpgw daemon status`
  already checks better — by asking the gateway for an HTTP response on the
  address it was installed for.
- **`Restart=on-failure`, not `always`.** A gateway that crashes comes
  straight back; a gateway you stopped stays stopped.
- **`PATH` is captured at install time.** A user unit otherwise starts with
  the manager's own minimal `PATH`, and almost every stdio MCP server is an
  `npx`, `uvx` or `bunx` living under `~/.local/bin` or a version manager's
  shim directory — so the gateway would come up with every stdio server
  failing to spawn. The cost is that the `PATH` is frozen: change it, or move
  your config, and re-run `install`. `mcpgw add` and `mcpgw doctor` say so
  when a command stops resolving under it — see
  [After your PATH moves](#after-your-path-moves).

`stop` and `start` are `systemctl --user stop` / `start` on that unit, and
`uninstall` disables it, deletes the file and reloads. Uninstalling something
that is not installed succeeds — the end state is what was asked for.

### Logging out stops it, unless you linger

A user manager normally shuts down with your last session, which takes the
gateway with it. `mcpgw daemon install` and `mcpgw daemon status` both report
which side of that you are on:

```text
service   installed under systemd --user, running
          (~/.config/systemd/user/mcpgw.service) — enabled, so it comes back
          at login; user lingering is off, so the gateway stops when your last
          session ends …
```

mcpgw does not run `enable-linger` for you. It is an account-wide switch:
afterwards *every* user service you have keeps running while you are logged
out, which is a decision about the machine and not about this gateway. If
that is what you want — a headless box, or a gateway that answers over an SSH
tunnel with no desktop session — run it once yourself:

```sh
loginctl enable-linger "$USER"
```

If `loginctl` is not there at all, the note says the question could not be
answered rather than guessing at it.

### Distributions without systemd

On a machine with no `systemctl` on `PATH`, `install`, `start`, `stop` and
`uninstall` say so and point at the alternative rather than failing with an
errno:

```text
Error: systemd --user: cannot run `systemctl` (No such file or directory) —
this build installs the gateway as a systemd user unit, and this machine does
not appear to be running systemd. Start it with `mcpgw serve` under whatever
supervisor you do have (an OpenRC, runit or s6 service), and `mcpgw daemon
status` will still report on it
```

## Windows: the service

`mcpgw daemon install` registers a real Windows service called `mcpgw`
("mcpgw gateway" in the Services console), set to start automatically — at
boot, before anyone logs in — and to be restarted by the service manager if
it dies.

```sh
mcpgw daemon install
```

### The administrator prompt

Registering, starting, stopping and removing a service all need
administrator rights. mcpgw tells you why before Windows asks:

```text
Windows needs administrator rights to install a service. It is about to ask
you to approve one elevated `mcpgw daemon install`, which does that and
nothing else. If you say no, nothing changes.
```

Then the UAC dialog appears. Approving it runs that one command elevated and
nothing else; mcpgw waits for it, asks the service manager what actually
happened, and reports that. Declining it is a normal answer, not an error to
decipher:

```text
Windows needs administrator rights to install a service. You said no, so
nothing was installed and nothing was changed. Two ways forward: open a
terminal as administrator and run `mcpgw daemon install` again, or skip the
service and run `mcpgw serve` in a terminal — same gateway, it just stops
when the terminal does.
```

`mcpgw daemon status` and `mcpgw daemon logs` never prompt: reading the
service database needs no rights at all.

### What the service actually runs

A Windows service is not an ordinary program — the service manager expects
the process it starts to report in as a service within thirty seconds, and
`mcpgw serve` is an ordinary program. So the registered command is an
internal one that exists only to be a service: it starts `mcpgw serve` as its
child, redirects that child's output into the two log files `mcpgw daemon
logs` reads, and stops it when Windows asks the service to stop. If the
gateway dies, the service ends with its exit code, which is what makes the
restart actions fire — three restarts within an hour before Windows gives up.

There is no unit file to look at. The registration lives in
`HKLM\SYSTEM\CurrentControlSet\Services\mcpgw`, which is what
`mcpgw daemon status` prints.

### It runs as LocalSystem

This is the one thing worth knowing before it surprises you. A Windows
service runs under a machine account, not yours — running it as you would
mean mcpgw asking for and storing your password, which it will not do. So:

- The gateway is pointed at **your** config file and **your** log directory
  explicitly, at install time. It reads the config you edit, not one under
  `C:\Windows\System32`.
- But the MCP servers it launches run as `SYSTEM` too. A server that needs
  something only your account has — an entry on your `PATH`, a credential in
  your user profile, a tool installed per-user — will not find it there. If a
  server works under `mcpgw serve` and not under the service, this is why.

If that trade is wrong for you, `mcpgw serve` in a terminal is the same
gateway with none of it.

## After an upgrade

An upgrade in place — `brew upgrade mcpgw`, `cargo install mcpgw`, `mcpgw
self-update` — writes a new binary at the same path the service was installed
with, and no service manager notices: launchd, systemd and the Windows
service manager all restart a job that *ends*, not one whose file changed
underneath it. So the gateway watches that file itself. When it is replaced,
the gateway says so and exits with a failing status, and the supervisor
starts the new one a couple of seconds later:

```text
the mcpgw binary at /opt/homebrew/bin/mcpgw changed; restarting so the service runs it (see mcpgw daemon logs)
```

Three things about that are worth knowing:

- **Only the service does it.** A gateway you started in a terminal keeps
  running whatever it was started with, because nothing would bring it back.
- **A service installed by an older mcpgw does not do it yet.** Run `mcpgw
  daemon install` once and it will.
- **It restarts once per binary.** The gateway writes down which one it stood
  aside for, so an upgrade that will not come up cannot be turned into a
  restart loop. Past that, the throttling is the supervisor's own — a binary
  that cannot start at all is a question for `mcpgw daemon logs`.
- **It runs the replacement first.** Publish a new build by renaming it into
  place, the way `brew`, `cargo install` and `self-update` do; an in-place
  overwrite of a running binary (`cp -f` over the path) leaves a file macOS
  refuses to execute, and the gateway says so and keeps serving the build it
  is on rather than restarting into it:

```text
warning: the mcpgw binary at /opt/homebrew/bin/mcpgw changed but does not run (signal: 9 (SIGKILL)); staying on the current build — replace it with a fresh file (rename into place), not an in-place overwrite
```

That check gives the replacement five seconds to answer `mcpgw --version`,
which is a very long time for a line it prints before it reads anything —
but the five seconds are wall clock, so on a machine with far more work in
flight than cores the fork can wait them out before it is scheduled at all,
and a perfectly good upgrade is reported as one that does not run. Set
`MCPGW_VERIFY_TIMEOUT_SECS` in the service's environment to buy more room:

```text
MCPGW_VERIFY_TIMEOUT_SECS=60
```

Whole seconds, read once when the gateway starts; anything else is ignored
and the default stands.

## After mcpgw itself moves

The service definition names the binary it was installed from by absolute
path, so changing how mcpgw is installed — `cargo install` to Homebrew, or
back, or a manual download to either — leaves the service pointing at the old
copy. Run `mcpgw daemon install` again and it is re-registered against the
binary you are running now:

```sh
mcpgw daemon install
```

```text
stopping the running service to reinstall it (was: ~/.cargo/bin/mcpgw)
installed the mcpgw gateway service at ~/Library/LaunchAgents/io.mcpgw.gateway.plist
```

It reinstalls over the running service rather than refusing the port it is
still holding, so there is no `mcpgw daemon stop` to remember first. The
refusal is still there for anything that is *not* the installed service: a
foreground `mcpgw serve` on the same port, or some other program, is left
alone and named instead. The same command is how a moved config file or a
changed `PATH` gets picked up.

You do not have to remember that you moved, either. `mcpgw daemon status`,
`mcpgw doctor` and the status card all compare the binary the service was
installed from with the one you are running, and say so when the two have
come apart — whether the recorded one is gone entirely or is simply a second
copy the service kept running while your upgrades landed elsewhere:

```text
service   installed from ~/.cargo/bin/mcpgw, which is gone — run `mcpgw daemon install` to point it at /opt/homebrew/bin/mcpgw
```

Symlinks are followed before the two are compared, so a Homebrew mcpgw
reached through `/opt/homebrew/bin` is not reported as a different binary
from the one in the Cellar it points at. In `doctor` this is a warning rather
than an error: the gateway may be answering perfectly well on the old binary,
and the only thing actually broken is that upgrading it changes nothing.

The same three also say when the gateway that is *answering* is a different
version than the mcpgw you are typing. A service that watches its binary
closes that gap itself within seconds; one installed before it did leaves
yesterday's gateway on the port until something restarts it:

```text
service   runs mcpgw 0.4.0; you are running 0.4.1 — run `mcpgw daemon install` to restart it on this build
```

That one is only ever said about a gateway that answered the probe: the
running gateway writes down what it is, and a file left behind by a crash is
not a version anybody is serving.

## After your PATH moves

The `PATH` in the plist or the unit is the one the shell you installed from
had, and nothing re-reads it afterwards. That is deliberate — it is what makes
an `npx` or `uvx` server work under a supervisor at all — but it means the
service keeps looking on yesterday's `PATH`. Install a new node with `nvm`,
`asdf`, `volta` or `mise`, or install the daemon from a login shell that had
not sourced the version manager yet, and you get a server that starts when you
run it by hand and dies before the MCP handshake under the daemon:

```text
connection closed: initialize response
```

`mcpgw add` and `mcpgw doctor` now tell that apart from a command that is
simply not installed. Both resolve a bare stdio command twice — once on your
own `PATH`, once on the one the installed service definition records — and say
so when only the first one finds it:

```text
playwright  "npx" resolves in your shell (/Users/you/.nvm/versions/node/v22.23.1/bin/npx) but not
            on the PATH the gateway service was installed with, so the daemon cannot start it —
            re-run `mcpgw daemon install` from this shell to refresh that PATH, or use the
            absolute path /Users/you/.nvm/versions/node/v22.23.1/bin/npx
```

There are three ways out, and which one is right depends on why the two
disagree:

- **`mcpgw daemon install`, from a shell where the command works.** The
  install re-bakes that shell's `PATH`, so this is the fix when your shell's
  `PATH` is the one you want the daemon to have.
- **Spell the command as an absolute path.** No `PATH` decides it, so nothing
  about it can go stale — at the cost of pinning a version manager's directory,
  which moves on the next node upgrade.
- **Give that one server its own `PATH`.** `mcpgw add <name> --force --env
  PATH=... -- <command>` reaches the child whatever the daemon's own
  environment is, which is what to reach for when only one server needs a
  toolchain the rest of the machine does not have.

In `doctor` this is a warning rather than an error: the entry is spelled
correctly and a foreground `mcpgw serve` starts it perfectly — what is broken
is only the daemon's ability to find it. A server carrying its own `PATH` in
`env`, a command already spelled as a path, and a machine with no service
installed are all silent, since none of them has two `PATH`s to disagree.

If a spawn does fail anyway, the gateway's own log says which `PATH` it looked
on rather than only that the file was not found:

```text
spawn failed: "npx" is not on the PATH this gateway searched (/usr/bin:/bin:/usr/sbin:/sbin) —
a service runs with the PATH it was installed with, so re-run `mcpgw daemon install` from a
shell where "npx" resolves, or give the absolute path
```

## Status

```sh
mcpgw daemon status
```

```text
gateway   running — http://127.0.0.1:8137/mcp answers (HTTP 405)
service   not installed under launchd
logs      ~/.local/share/mcpgw/logs/daemon.out.log (not written yet)
          ~/.local/share/mcpgw/logs/daemon.err.log (not written yet)

no service is installed, but a gateway is already answering at
http://127.0.0.1:8137/mcp — that is a foreground `mcpgw serve`, and it stops
when its terminal does
```

Three separate questions, deliberately: something can be listening on the
port without being a gateway, and a gateway can be running without anything
being installed to keep it running. `--url` points the probe somewhere else.

The probe follows the service. `mcpgw daemon install` records the address it
installed with under the state directory (`daemon.json`, `0600`, removed
again by `mcpgw daemon uninstall`), so a service installed with `--port
18137` is probed on 18137 and `mcpgw daemon start` brings it back up there
too — neither needs the flag repeated. With nothing recorded both fall back
to `http://127.0.0.1:8137/mcp`; a service installed by 0.3.0 or earlier is in
that state, and `status` names it rather than reporting a healthy gateway as
down.

It exits `0` when a gateway is answering and `1` when it is not, so it can be
used as a check in a script.

## Logs

A supervised gateway has no terminal, so its output goes to two files under
the state directory:

```sh
mcpgw daemon logs             # the last 50 lines of each
mcpgw daemon logs -n 200
mcpgw daemon logs --follow    # keep printing as it writes
```

Both streams are shown. A gateway that failed to start says why on stderr and
nothing at all on stdout, and picking the wrong file first wastes the minute
you were trying to save.

The log directory is `0700` and the files `0600`, the same discipline the
traffic log gets: everything mcpgw derives from your client configs can carry
the tokens in them.

## Binding: loopback by default

A daemon refuses to install or start on a non-loopback address, unless its
clients authenticate:

```sh
$ mcpgw daemon install --bind 0.0.0.0
hint: a gateway whose clients authenticate may bind anywhere — set
`[gateway] require_token = true` in your config, run `mcpgw sync` so every
client carries this install's token, then install again
Error: refusing to run an unattended gateway on 0.0.0.0: it has no
authentication, so anyone who can reach that address could call your MCP
servers …
```

`mcpgw serve --bind 0.0.0.0` only warns, and the difference is deliberate.
A warning works when a person is looking at a terminal and can decide. An
unattended service prints its warning into a logfile nobody reads, so the
same address that is a judgement call in the foreground is a machine on your
network answering MCP calls, for as long as it stays up. Loopback is
`127.0.0.0/8`, `::1` and `localhost`.

To open the bind:

```toml
# config.toml
[gateway]
require_token = true
```

```sh
mcpgw sync                                   # every client carries the token
mcpgw daemon install --bind 0.0.0.0          # now allowed
```

Both halves are required — a token that exists but is not yet required is not
a boundary, since the one-release grace period still admits an
unauthenticated loopback request — but `daemon install` mints the token if
this install has none, so in practice the switch is the half you have to set.
`daemon start` mints nothing, and refuses the address on a machine whose
token file is gone. `mcpgw doctor` reports a service bound past loopback
without the requirement as an error.

Setting the switch does not sync your clients. Do that first, or every client
on the machine stops being answered the moment the gateway comes up.

A bearer token over plain HTTP is a floor, not a ceiling: on any network you
do not control, put TLS and something that authenticates in front as well. See
[Trust model](./trust-model.md#binding-anywhere-else).

`mcpgw daemon status` keeps working either way — the liveness probe is a bare
`GET /mcp` and is deliberately left open.

Port conflicts are refused up front for the same reason — a service that
cannot bind fails silently in the background:

```text
Error: something already listens on 127.0.0.1:8137 — run `mcpgw daemon
status` to see whether that is an mcpgw gateway you already started
```
