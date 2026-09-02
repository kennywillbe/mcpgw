# Running as a daemon

`mcpgw serve` holds a terminal. That is fine while you are trying the gateway
out and wrong the moment you depend on it: the first thing an MCP client does
in the morning is ask for a tool list, and nothing is there to answer.

`mcpgw daemon` is the answer — the gateway supervised by the machine's own
service manager, started at login and restarted when it dies.

## Status of this feature

The per-OS installers are landing one at a time in this release wave:

| Command                         | State                                      |
| ------------------------------- | ------------------------------------------ |
| `mcpgw daemon status`           | works now                                  |
| `mcpgw daemon logs [--follow]`  | works now                                  |
| `mcpgw daemon install`          | macOS and Windows now; Linux in this wave  |
| `mcpgw daemon uninstall`        | ditto                                      |
| `mcpgw daemon start` / `stop`   | ditto                                      |

Until your platform's installer ships, `install`, `start`, `stop` and
`uninstall` tell you so and point you at `mcpgw serve`. `status` and `logs`
already work everywhere, and `status` reports on a foreground `mcpgw serve`
exactly as it will on a supervised one.

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
  frozen: change it, or move your config, and re-run `install`.

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

## Binding: loopback only

A daemon refuses to install or start on a non-loopback address:

```sh
$ mcpgw daemon install --bind 0.0.0.0
Error: refusing to run an unattended gateway on 0.0.0.0: it has no
authentication, so anyone who can reach that address could call your MCP
servers …
```

`mcpgw serve --bind 0.0.0.0` only warns, and the difference is deliberate.
A warning works when a person is looking at a terminal and can decide. An
unattended service prints its warning into a logfile nobody reads, so the
same address that is a judgement call in the foreground is a machine on your
network answering MCP calls with no authentication, for as long as it stays
up. Loopback is `127.0.0.0/8`, `::1` and `localhost`; put a reverse proxy in
front if the gateway has to be reachable from anywhere else.

Port conflicts are refused up front for the same reason — a service that
cannot bind fails silently in the background:

```text
Error: something already listens on 127.0.0.1:8137 — run `mcpgw daemon
status` to see whether that is an mcpgw gateway you already started
```
