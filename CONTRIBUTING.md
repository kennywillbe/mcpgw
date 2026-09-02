# Contributing

Issues and patches both welcome. This is a small project; the process is short.

## Commits

Conventional commits, because release-please reads them to decide the next
version and write the changelog:

```text
feat: add --tag filter to list
fix: don't drop env vars when re-importing a stdio server
docs: describe the state directory layout
deps: bump rmcp to 3.1
chore: drop the unused sync fixture
refactor: pull probe timeout handling into one place
test: cover rollback with a missing backup dir
```

`feat`, `fix`, `perf` and `deps` show up in the changelog; the rest are
recorded but hidden.

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
   pushes `v<version>`.
3. The `v*` tag triggers `release.yml`, which builds the four target archives,
   hashes them and attaches everything to a GitHub Release.

The whole workspace ships as one version, and there is one changelog, at the
repo root. The single tracked package sits at the repo root (`"."`), which is
what makes a fix in `crates/core` count: release-please splits commits by the
tracked package's path, and only the root path is exempt from that split and
handed every commit. Tracking `crates/cli` instead meant a core-only fix
produced no release PR at all.

Because the root manifest is a virtual one — `[workspace]` with no `[package]`
— the `rust` release type can't be used there; it insists on rewriting a
`[package] version` at the package path and errors out on a workspace
manifest. So the release type is `simple`, and every version in the tree is
rewritten through the `extra-files` list in `release-please-config.json`: the
`[package] version` of `mcpgw` and `mcpgw-core`, the `mcpgw-core` pins in the
CLI and test-server manifests, and the two matching `Cargo.lock` entries. That
last part matters: CI builds `--locked`, so a bump that moved the manifests and
left the lockfile alone would land a red release PR.

`simple` also wants a `version.txt`, which this repo does not have — the
manifests are the source of truth. A missing file with `createIfMissing` unset
is skipped, so the run logs `file version.txt did not exist` and carries on.
That line is expected; nothing is broken.

The lockfile entries are picked out by a filter that reads a little oddly:

```json
"jsonpath": "$.package[?(@.name==\"mcpgw\"||@.name.value==\"mcpgw\")].version"
```

release-please parses TOML into a tree where every scalar is wrapped as
`{start, end, value}`, so it can splice a single value out of the file without
reformatting the rest — hence matching the name at `@.name.value`. The plain
`@.name` half is there so the filter survives that wrapping going away.

`crates/core/CHANGELOG.md` is now just a pointer to the root one, kept only
because crates.io looks for a file by that name next to the manifest.

Adding a crate to the workspace? Add its `[package] version` to `extra-files`,
and its `Cargo.lock` entry too; if it also depends on `mcpgw-core` with a
`version = "…"` pin, add that pin, or cargo will refuse to resolve the
workspace on the next release. Nothing bumps a version by itself here.

## Dependency updates

Dependabot opens one grouped PR a week per ecosystem — `cargo-deps` for crates,
`actions-deps` for workflow actions — under a `deps:` prefix, which
release-please files under **Dependencies** in the changelog. A `deps:` commit
with nothing else alongside it bumps the patch version.

## Style

- Comments explain *why*, not what. No commented-out code, no changelog
  comments, no bare `TODO` without an issue link.
- Errors: `thiserror` in `mcpgw-core`, `anyhow` in the CLI crate.
- No `unwrap()`/`expect()` outside tests unless the invariant is proven in a
  comment right there.
- Secrets never reach logs, errors, or test fixtures.
