## What and why

<!-- What changes, and what problem it solves. Link the issue if there is one. -->

## How it was verified

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] The three commands above are all green
- [ ] Regression test added that fails without this change

## Checklist

- [ ] Commit message is a conventional commit (`feat:`, `fix:`, `docs:`, …) — release-please reads it
- [ ] Comments explain *why*, not what
- [ ] Docs updated for behaviour or configuration changes
- [ ] No secrets in code, tests, logs or fixtures
