# Watching traffic

While the gateway is serving, it appends one JSON object per upstream request
to a daily file. Everything in this chapter reads that file, which is why it
works on a gateway that was already running before you started looking — and on
yesterday's traffic.

## Where it lives

```text
~/.local/share/mcpgw/traffic/2026-09-01.jsonl
```

One file per day, mode `0600`. `mcpgw serve --no-capture` turns it off, and
`--capture-bodies` decides how much of each request goes in — see
[What is redacted](#what-is-redacted) below.

## Following it live

```sh
mcpgw watch
```

```text
watching /Users/you/.local/share/mcpgw/traffic (Ctrl-C to stop)
  now  ✓  [s/github] github__create_issue      87ms  claude-code/2.1.3
  12s  ✓  [s/linear] linear tools/list          4ms  cursor/0.48
  30s  ✗  [s/github] github__search_code       210ms  claude-code/2.1.3  upstream "github" failed after 3 attempt(s)
```

Age, outcome, the endpoint it arrived on, what was called, how long it took,
which client made the call, and the error if there was one. The client is
whatever it named itself as; a call from a client that named itself nowhere
just doesn't have that part. `watch` replays what's already in today's file
before it starts following, so you see context immediately instead of an empty
screen.

```sh
mcpgw watch --server github        # one upstream
mcpgw watch --tool create_issue    # bare tool name, no server prefix
mcpgw watch --endpoint s/github    # one endpoint (`/s/github` works too)
mcpgw watch --session b1e4c07a     # one downstream client connection
mcpgw watch --client cursor        # one client, by substring of its name
mcpgw watch --json                 # JSONL, for jq
mcpgw watch --json --show-secrets  # …with args/response unmasked
```

```sh
mcpgw watch --json | jq -r 'select(.ok == false) | "\(.server) \(.error)"'
```

## The terminal UI

```sh
mcpgw watch --tui
```

Same records, three panes instead of a stream. It exists because the questions
that matter with four clients and ten servers are comparative — which server is
slow, which tool fails and how often, what that one client was doing right
before it hung — and a line stream can only be scrolled, not compared.

- **Top** — a live table, one row per server and tool: calls, errors, p50 and
  p95 latency, and how long ago it was last seen. Over a rolling window of the
  last 1000 records, so the numbers are the shape of the traffic now rather
  than an all-day average that flattens the spike you opened this to find.
- **Middle** — the call log, newest at the bottom. Age, outcome, server,
  target, method, client and duration. It follows the tail until you scroll up,
  and follows again when you scroll back to the last row.
- **Bottom** — the detail pane for the selected call, under the same redaction
  rules as everything else here: a line captured `full` shows `***` for its
  args and response unless you started with `--show-secrets`, and a line the
  gateway already redacted is shown as it was written.

| key | what |
| --- | --- |
| `q`, `Esc`, `Ctrl-C` | quit |
| `↑`/`↓`, `k`/`j` | select a call |
| `Enter` | show or hide the detail pane |
| `f` | filter by server → tool → status → client, one prompt at a time |
| `/` | free text filter over server, tool, method, endpoint, session, client and error |
| `p` | pause and resume — lines that arrive while paused are held, not dropped |
| `c` | clear |
| `s` | sort the table by calls, errors or p95 |
| `?` | the key list, over the panes |

At a prompt, an empty answer takes that filter off again, and `Esc` cancels.
The `--server`, `--tool`, `--endpoint`, `--session` and `--client` flags work
the same as they do for the stream and set where the TUI starts:

```sh
mcpgw watch --tui --server github
```

The line stream stays the default. `mcpgw watch` with no flags prints lines
exactly as it always did, and `--tui` needs a real terminal — off one it says
so and points at the stream rather than writing escape sequences into a pipe.

## The record format

```json
{
  "ts": 1756742400123,
  "session": "b1e4c07a",
  "client": "claude-code/2.1.3",
  "endpoint": "s/github",
  "server": "github",
  "tool": "create_issue",
  "kind": "call",
  "duration_ms": 87,
  "ok": true,
  "args": "{\"title\":\"…\"}",
  "response": "{\"content\":[…]}",
  "bodies": "redacted"
}
```

- `ts` — when the request *finished*, epoch milliseconds. It started
  `duration_ms` earlier.
- `session` — which downstream *connection* the request came from, and what
  it means depends on the revision the client speaks:

  | the client speaks | `session` is |
  | --- | --- |
  | a revision with sessions (2025-11-25 and earlier) | the transport session it was given at `initialize` — two harnesses get different ids, and reconnecting gets a new one |
  | 2026-07-28, which [removed sessions](https://modelcontextprotocol.io/specification/2026-07-28/) | its own name and version, so two windows of one editor share a row where two harnesses do not |
  | neither, having named itself nowhere | the gateway *process*, which cannot tell any two clients apart |

  It is a fingerprint in every case, not the value itself: a session id is a
  credential and does not belong in a log file. The ids never collide, so the
  field is filterable whatever produced it; a coarser one is just a coarser
  answer.
- `client` — which client *software* made the call: `<name>/<version>` as the
  client gives it, or a bare name for a client that names no version. This is
  the field that answers "which harness was this", and the one to reach for
  since 2026-07-28: a client on that revision repeats its identity on every
  request, where `session` has nothing left to distinguish it by. A client on
  an older revision gives the same identity once, at `initialize`, and it is
  recorded here just the same.

  Naming yourself is a SHOULD in the protocol, not a MUST. A client that
  declines has no `client` on its lines at all — absent, never guessed — and
  `--client` does not match it. Lines written before mcpgw recorded this are
  absent for the same reason and read the same way.
- `endpoint` — which face of the gateway took the request. It is
  `s/<server>`: a server is reached through its own endpoint and nowhere
  else. Absent on stdio traffic and on lines written before this field
  existed. `mcp` appears on lines a 0.4 gateway wrote, when the base endpoint
  still served every server's tools at once, and on nothing written since.
- `kind` — which request family the record describes:

  | `kind` | method |
  | --- | --- |
  | `list` | `tools/list` |
  | `call` | `tools/call` |
  | `resources` | `resources/list` |
  | `resource_templates` | `resources/templates/list` |
  | `resource_read` | `resources/read` |
  | `prompts` | `prompts/list` |
  | `prompt_get` | `prompts/get` |
  | `complete` | `completion/complete` |
  | `denied` | `tools/call`, refused by the server's `[tools]` table |
  | `drift` | `tools/list`, whose definitions no longer match the pins |

  Everything below `call` was added when endpoints grew past tools; older
  lines carry only `list` and `call`.
- `change` / `desc_len_before` / `desc_len_after` — on a `drift` line only.
  `change` is `changed`, `added` or `removed`, `tool` names the tool it
  happened to, and the two lengths are the size in bytes of the description
  either side of it:

  ```json
  {"ts":1756742400123,"session":"b1e4c07a","endpoint":"s/github",
   "server":"github","tool":"create_issue","kind":"drift","duration_ms":0,
   "ok":true,"change":"changed","desc_len_before":21,"desc_len_after":384}
  ```

  Lengths and never the text. A rewritten description is the payload of a
  tool-poisoning attack, and copying it here would put those instructions in
  front of the next reader — human or model — of the traffic log. `ok` is
  `true` because nothing failed: the list succeeded, and the drift is a fact
  about the answer rather than a request that went wrong. One record per
  tool that moved, written once per change rather than once per list, so a
  client polling `tools/list` does not fill the file with the same
  unaccepted change. See [Tool definition
  drift](./gateway.md#tool-definition-drift).
- `tool` — what the request named: the tool, the prompt, the resource URI or
  the argument being completed. Absent on the list kinds, which name nothing.
- `ok` / `error` — `error` carries the full text; `watch`'s one-line view
  truncates it, `--json` doesn't.
- `args` / `response` — see below.
- `bodies` — how much of `args`, `response` and `error` this line was allowed
  to keep: `off`, `redacted` or `full`. Absent means `full`, which is what
  every line written before redaction existed is.

Since it's plain JSONL, `tail -f`, `jq` and `grep` all work on it directly.

## What is redacted

Bodies are **redacted first and truncated second**, at capture time, before
anything reaches the disk. That order is the point: a secret sitting past the
2 KB cap would otherwise be cut in half and stored anyway.

```sh
mcpgw serve                              # --capture-bodies redacted (default)
mcpgw serve --capture-bodies off         # metadata only: no args, response or error text
mcpgw serve --capture-bodies full        # everything, verbatim
mcpgw serve --no-capture                 # no traffic log at all
```

Under `redacted`, four classes of rule run over `args`, `response` and the
`error` text:

| Rule | What goes | Example |
| --- | --- | --- |
| **Key names** | the whole value under a JSON key that matches, case-insensitively, `authorization`, `cookie`, `set-cookie`, or anything containing `token`, `secret`, `password`, `passwd`, `credential` or `api-key`/`api_key`/`apikey` | `{"api_key": "[redacted]"}` |
| **Auth schemes** | the credential after `Bearer` or `Basic`, anywhere in a string; the scheme stays | `Authorization: Bearer [redacted]` |
| **Issuer prefixes** | `sk-`, `ghp_`, `gho_`, `xoxb-`/`xoxa-`/`xoxp-`, `AKIA…`, and JWTs (`eyJ….eyJ….`) | `[redacted:ghp_…]` |
| **Query values** | the value of a URL parameter whose *name* looks like any of the key names above; the parameter name stays | `?access_token=[redacted]` |

Plus one heuristic, for the credentials that carry no marker at all: a run of
32 or more `A–Z a–z 0–9 _ -` characters is replaced when **all four** of these
hold — it mixes at least two of lowercase, uppercase and digits; it has no run
of eight lowercase letters (which is what an English word looks like); under
15% of it is `-` or `_` (which is what a URL slug looks like); and its Shannon
entropy is at least **3.3 bits per character**. Random base64 and hex score
3.4–4.8; prose and identifiers fall below one of the first three tests before
entropy is even measured.

Redacted values keep four leading characters — `[redacted:ghp_…]` — so you can
still tell *which* credential was there without the log holding any of it. The
bias is deliberately towards redacting: a false positive costs a debugging
clue, a false negative costs a credential. A UUID or a git sha in a tool
argument will sometimes be caught; `--capture-bodies full` is the way out.

### Adding your own patterns

Site-specific shapes — an internal ticket id, a customer number — go in the
config:

```toml
version = 1

[capture]
redact = ["ACME-[0-9]{4}", "cus_[A-Za-z0-9]+"]
```

They're [Rust `regex`](https://docs.rs/regex) patterns, added to the built-in
rules rather than replacing them, and every match becomes `[redacted]`. A
pattern the engine cannot compile is a config error naming the pattern, not a
rule that quietly matches nothing. The table is read once at startup, so an
edit needs a gateway restart — unlike adding a server, which hot-reloads.

### What redaction is not

It is a filter over shapes, not a guarantee. A secret with no marker, no
prefix and low entropy — a short passphrase, a four-digit PIN — looks like
ordinary text and stays. The file is still mode `0600` under your own state
directory, `--capture-bodies off` still exists for people who only want the
timings, and `--no-capture` still turns the whole thing off. The
[Trust model](./trust-model.md) puts this beside everything else worth knowing
before every call goes through one process.

`mcpgw watch --json` masks `args` and `response` with `"***"` for lines
captured as `full`, and prints redacted lines as they were written — there is
nothing left in them to mask, and masking would hide the `[redacted:ghp_…]`
hints redaction deliberately left legible. `--show-secrets` opts out of the
masking and says once, on stderr, when it meets a line that had nothing to
reveal.

## One server, no gateway

If you just want to know what a single server offers, `inspect` connects
directly and tables its tools and resources:

```sh
mcpgw inspect github
mcpgw inspect github --json --timeout 30
```

No gateway has to be running — it uses the same connect path as
`doctor --probe`.
