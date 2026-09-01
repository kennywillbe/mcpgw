# docs/demo.gif is generated from this file:
#
#   cargo build --release
#   PATH="$PWD/target/release:$PATH" vhs docs/demo.tape
#
# The release binary has to be on PATH before vhs runs — the tape types plain
# `mcpgw`, the same thing a viewer would type. MCPGW_CONFIG and MCPGW_STATE_DIR
# point at a scratch directory so the recording never touches a real setup and
# every run starts from the same empty state.

Output docs/demo.gif

Set Shell bash
Set FontSize 20
Set Width 1200
Set Height 680
Set Padding 20
Set TypingSpeed 45ms

Env MCPGW_CONFIG "/tmp/mcpgw-demo/config.toml"
Env MCPGW_STATE_DIR "/tmp/mcpgw-demo/state"

Hide
Type "rm -rf /tmp/mcpgw-demo && mkdir -p /tmp/mcpgw-demo/state"
Enter
Type "clear"
Enter
Show

Sleep 1s

# --- one list of servers ---------------------------------------------------

Type "mcpgw add github -- npx -y @modelcontextprotocol/server-github"
Sleep 600ms
Enter
Sleep 1.5s

Type "mcpgw add linear --url https://mcp.linear.app/mcp"
Sleep 600ms
Enter
Sleep 1.5s

Type "mcpgw list"
Sleep 400ms
Enter
Sleep 3s

# --- push it into every client ---------------------------------------------

Type "mcpgw sync --dry-run"
Sleep 600ms
Enter
Sleep 4s

Type "clear"
Enter
Sleep 500ms

# --- is any of it actually healthy? ----------------------------------------

Type "mcpgw doctor"
Sleep 400ms
Enter
Sleep 4s

Type "clear"
Enter
Sleep 500ms

# --- the gateway, and the traffic through it -------------------------------

Type "mcpgw serve &"
Sleep 600ms
Enter
Sleep 3s

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
