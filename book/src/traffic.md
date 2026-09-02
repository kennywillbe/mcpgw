# Watching traffic

While the gateway is serving, it appends one JSON object per upstream request
to a daily file. Everything in this chapter reads that file, which is why it
works on a gateway that was already running before you started looking — and on
yesterday's traffic.

## Where it lives

```text
~/.local/share/mcpgw/traffic/2026-09-01.jsonl
```

One file per day, mode `0600`. `mcpgw serve --no-capture` turns it off.

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
  "response": "{\"content\":[…]}"
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
  aggregate, `s/<server>` for a per-server endpoint. Absent on stdio traffic
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

Since it's plain JSONL, `tail -f`, `jq` and `grep` all work on it directly.

## Truncation, not redaction

Captured arguments and responses are **cut at 2 KB and marked
`…[truncated]`. They are not redacted.** If a secret is passed as a tool
argument, it lands in that file.

The mitigations today are that the file is `0600` under your own state
directory, that `mcpgw serve --no-capture` disables capture entirely, and that
`watch` does not put those bodies back on your terminal: the one-line view
never showed them, and `--json` replaces each with `"***"` unless you ask for
`--show-secrets`. That bounds the spread, not the file itself. Redaction at
capture time is on the [roadmap](./roadmap.md); until it ships, this is the
honest description.

## One server, no gateway

If you just want to know what a single server offers, `inspect` connects
directly and tables its tools and resources:

```sh
mcpgw inspect github
mcpgw inspect github --json --timeout 30
```

No gateway has to be running — it uses the same connect path as
`doctor --probe`.
