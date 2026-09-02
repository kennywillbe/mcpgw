# mcpgw-core

The engine behind the [mcpgw](https://crates.io/crates/mcpgw) CLI: the config
model, the 13 client adapters that read and write Claude Desktop / Claude Code /
Cursor / VS Code / Zed / Codex and friends, the gateway that multiplexes
upstream MCP servers, and the traffic capture behind `mcpgw watch`.

**If you want the tool, install `mcpgw`, not this crate:**

```sh
curl -fsSL https://github.com/kennywillbe/mcpgw/releases/latest/download/mcpgw-installer.sh | sh
brew install kennywillbe/tap/mcpgw
cargo install mcpgw
```

This library is published for two reasons: a published crate needs published
dependencies, and the client adapters are useful on their own if you're
building something else that has to edit the same config files. The API is not
stable yet and will move with the CLI.

- Repository: <https://github.com/kennywillbe/mcpgw>
- Documentation: <https://kennywillbe.github.io/mcpgw/>

Licensed under MIT or Apache-2.0, at your option.
