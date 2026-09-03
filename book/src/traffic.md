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
  now  ✓  [mcp] github__create_issue         87ms
  12s  ✓  [s/linear] linear tools/list        4ms
  30s  ✗  [mcp] github__search_code         210ms  upstream "github" failed after 3 attempt(s)
```

Age, outcome, the endpoint it arrived on, what was called, how long it took,
and the error if there was one. `watch` replays what's already in today's file
before it starts following, so you see context immediately instead of an empty
screen.

```sh
mcpgw watch --server github        # one upstream
mcpgw watch --tool create_issue    # bare tool name, no server prefix
mcpgw watch --endpoint s/github    # one endpoint (`/s/github` works too)
mcpgw watch --session b1e4c07a     # one downstream client connection
mcpgw watch --json                 # JSONL, for jq
mcpgw watch --json --show-secrets  # …with args/response unmasked
```

```sh
mcpgw watch --json | jq -r 'select(.ok == false) | "\(.server) \(.error)"'
```

## The record format

```json
{
  "ts": 1756742400123,
  "session": "b1e4c07a",
  "endpoint": "mcp",
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
- `session` — which downstream client connection the request came from. Over
  HTTP this is derived from the transport session the client was given at
  `initialize`, so two harnesses talking to one gateway get different ids and
  a client that reconnects gets a new one. It is a fingerprint, not the
  session id itself: the raw id is a credential and does not belong in a log
  file. Where the transport has no session — a stdio client, or an HTTP client
  on MCP 2026-07-28, which
  [removed sessions](https://modelcontextprotocol.io/specification/2026-07-28/)
  — it falls back to an id for the gateway *process*, which cannot tell two
  clients apart. Same field, and the ids never collide; just a coarser answer.
- `endpoint` — which face of the gateway took the request: `mcp` for the
  gateway's own endpoint, `s/<server>` for a per-server one. Absent on stdio
  traffic
  and on lines written before this field existed.
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

  Everything below `call` is written only by a per-server endpoint, which is
  the shape that forwards those families.
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
