# docs/demo.gif is generated from this file:
#
#   cargo build --release
#   PATH="$PWD/target/release:$PATH" vhs docs/demo.tape
#
# The release binary has to be on PATH before vhs runs — the tape types plain
# `mcpgw`, the same thing a viewer would type. HOME, MCPGW_CONFIG and
# MCPGW_STATE_DIR point at a scratch directory holding two fixture client
# configs, so the recording never touches a real setup and every run starts
# from the same state.
#
# The two servers those clients hold are the workspace's own
# mcpgw-test-server, copied under two names: it handshakes and answers
# `tools/list` in milliseconds with no network, which is what makes the
# recording deterministic. Everything on screen is real output from the real
# binary against them.

Output docs/demo.gif

Set Shell bash
Set FontSize 20
Set Width 1200
Set Height 680
Set Padding 20
Set TypingSpeed 45ms

Env HOME "/tmp/mcpgw-demo/home"
Env MCPGW_CONFIG "/tmp/mcpgw-demo/config.toml"
Env MCPGW_STATE_DIR "/tmp/mcpgw-demo/state"
Env MCPGW_NO_UPDATE_CHECK "1"

Hide
Type "rm -rf /tmp/mcpgw-demo && mkdir -p /tmp/mcpgw-demo/bin /tmp/mcpgw-demo/state /tmp/mcpgw-demo/home/.cursor"
Enter
# The fixture servers, under the names the client configs spell.
Type "cp $(command -v mcpgw-test-server) /tmp/mcpgw-demo/bin/notes-mcp && cp $(command -v mcpgw-test-server) /tmp/mcpgw-demo/bin/todo-mcp"
Enter
Type "export PATH=/tmp/mcpgw-demo/bin:$PATH"
Enter
# Two clients already holding the same two servers — what a machine looks like
# before mcpgw, and what the import step has to fold together.
Type `printf '%s' '{"mcpServers":{"notes":{"command":"notes-mcp"},"todo":{"command":"todo-mcp"}}}' > /tmp/mcpgw-demo/home/.cursor/mcp.json`
Enter
Type `printf '%s' '{"mcpServers":{"notes":{"command":"notes-mcp"},"todo":{"command":"todo-mcp"}}}' > /tmp/mcpgw-demo/home/.claude.json`
Enter
Type "clear"
Enter
Show

Sleep 1s

# --- the whole setup: one command and four answers -------------------------

Type "mcpgw"
Sleep 600ms
Enter
Sleep 3s

# what it found
Type "y"
Sleep 400ms
Enter
Sleep 3.5s

# adopt both clients' servers, one copy of each
Type "y"
Sleep 400ms
Enter
Sleep 3.5s

# no background service for this recording
Type "n"
Sleep 400ms
Enter
Sleep 3.5s

# point both clients at the gateway
Type "y"
Sleep 400ms
Enter
Sleep 6s

Type "clear"
Enter
Sleep 500ms

# --- the gateway they now point at -----------------------------------------

Type "mcpgw serve &"
Sleep 600ms
Enter
Sleep 3s

# Drive real traffic through the gateway (a raw MCP client over the connect
# bridge) so watch has something to show; hidden because a viewer's traffic
# would normally come from Claude/Cursor.
Hide
Type `{ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"demo","version":"0"}}}' '{"jsonrpc":"2.0","method":"notifications/initialized"}' '{"jsonrpc":"2.0","id":2,"method":"tools/list"}' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"notes__echo","arguments":{"message":"hello"}}}' '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"todo__reverse","arguments":{"message":"stnetnoc"}}}'; sleep 5; } | mcpgw connect >/dev/null 2>&1`
Enter
Sleep 8s
Type "clear"
Enter
Show

Type "mcpgw watch"
Sleep 400ms
Enter
Sleep 8s

Ctrl+C
Sleep 1s

Hide
Type "kill %1 2>/dev/null; rm -rf /tmp/mcpgw-demo"
Enter
Show
