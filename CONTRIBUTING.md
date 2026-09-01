# Contributing

Issues and patches both welcome. This is a small project; the process is short.

## Commits

Conventional commits, because release-please reads them to decide the next
version and write the changelog:

```text
feat: add --tag filter to list
fix: don't drop env vars when re-importing a stdio server
docs: describe the state directory layout
chore: bump rmcp to 3.1
refactor: pull probe timeout handling into one place
test: cover rollback with a missing backup dir
```

Append `!` for a breaking change — `feat!: rename --gateway-url to --url`.
Anything else (`wip:`, no prefix at all) is ignored by release tooling, which
means it silently won't show up in the changelog.

Pre-1.0, `feat` bumps the minor and `fix` bumps the patch.

## Pull requests

- Branch off `main`, PR against `main`.
- CI has to be green — it runs on Linux, macOS and Windows and is the gate.
- Run the same three checks locally first, they're the whole of CI:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Clippy runs with `pedantic` on and warnings denied, so `-D warnings` locally is
not optional if you want a green PR.

- Touching `book/`? `mdbook build book` before pushing.

## Releases

Nobody tags by hand:

1. Merging to `main` updates a standing **"chore: release"** PR with the next
   version and the generated changelog. Edit that PR's `CHANGELOG.md` if the
   generated prose needs help — edits survive.
2. Merging the release PR bumps every crate version, writes `CHANGELOG.md` and
   pushes `v<version>` (the CLI) and `mcpgw-core-v<version>`.
3. The `v*` tag triggers `release.yml`, which builds the four target archives,
   hashes them and attaches everything to a GitHub Release.

Both crates are version-locked, so they always move together.

One-time note for whoever merges the **first** release PR: delete the
`"release-as": "0.1.0"` line from `release-please-config.json` afterwards. It
exists only to pin the initial release at 0.1.0, and left in place it would
propose 0.1.0 forever.
