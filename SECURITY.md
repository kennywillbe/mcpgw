# Security Policy

## Supported versions

Pre-1.0: only the latest published version is supported. There are no other
versions.

## Reporting a vulnerability

Please **do not open a public issue** for security problems.

Use GitHub's [private vulnerability reporting](https://github.com/kennywillbe/mcpgw/security/advisories/new).
Include:

- affected version (`mcpgw --version`) and OS
- what an attacker can achieve, and the smallest reproduction you have
- output of `mcpgw doctor --json`, or logs, with tokens and URLs redacted

You can expect an acknowledgement within 5 working days and a fix or mitigation
plan within 30 days for confirmed issues. We will credit you in the release
notes unless you prefer otherwise.

## Scope

In scope: the gateway accepting traffic it shouldn't (see the Origin check in
the README), secrets leaking into logs, traffic captures or `--json` output
that should have been masked, a client config being written somewhere it
shouldn't, and file permissions on the state directory being wider than
documented.

Out of scope: an MCP server you configured yourself behaving maliciously —
mcpgw connects to the servers you tell it to, and doesn't sandbox them.

## Handling secrets

mcpgw's state directory is mode `0700` and everything it writes into it —
client config backups, the managed-state file, traffic logs — is `0600`.
`mcpgw list --json` and `mcpgw watch --json` mask `env` and header values and
captured request/response bodies by default; `--show-secrets` opts back in.
Captured arguments and responses are truncated at 2 KB, not redacted, so a
secret passed as a tool argument still lands in the traffic log — see the
Security section of the README for the current limits. If you find a code
path that prints or writes a secret it should have masked, treat it as a
vulnerability and report it.
