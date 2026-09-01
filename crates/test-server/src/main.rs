//! Scripted stdio MCP server for tests. Deliberately implemented by hand
//! (not rmcp) so the probe client is exercised against an independent
//! implementation of the protocol.
//!
//! Modes (first CLI argument): `healthy` (default), `slow` (never answers),
//! `garbage` (non-JSON output), `exit` (dies immediately), `die-on-tools`
//! (handshakes fine, then dies on the first tools/list — exercises
//! died-after-ready reconnection).

use std::io::{BufRead as _, Write as _};

fn main() {
    let mode = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "healthy".to_owned());
    match mode.as_str() {
        "exit" => std::process::exit(1),
        "garbage" => {
            println!("this is definitely not json-rpc");
            std::io::stdout().flush().unwrap();
            park();
        }
        "slow" => park(),
        mode => serve(mode == "die-on-tools"),
    }
}

fn park() {
    std::thread::sleep(std::time::Duration::from_secs(3600));
}

fn serve(die_on_tools: bool) {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { return };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        // Requests carry an id; notifications don't and get no reply.
        let (Some(method), Some(id)) = (msg["method"].as_str(), msg.get("id").cloned()) else {
            continue;
        };
        match method {
            "initialize" => {
                // Echo the client's protocol version back so any rmcp
                // release negotiates successfully.
                let proto = msg["params"]["protocolVersion"]
                    .as_str()
                    .unwrap_or("2025-06-18");
                respond(
                    &mut stdout,
                    &id,
                    &serde_json::json!({
                        "protocolVersion": proto,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "mcpgw-test-server", "version": "9.9.9" }
                    }),
                );
            }
            "tools/list" if die_on_tools => std::process::exit(0),
            "tools/list" => respond(
                &mut stdout,
                &id,
                &serde_json::json!({
                    "tools": [
                        { "name": "echo", "description": "echoes input",
                          "inputSchema": { "type": "object" } },
                        { "name": "reverse", "description": "reverses input",
                          "inputSchema": { "type": "object" } }
                    ]
                }),
            ),
            "ping" => respond(&mut stdout, &id, &serde_json::json!({})),
            _ => {}
        }
    }
}

fn respond(out: &mut impl std::io::Write, id: &serde_json::Value, result: &serde_json::Value) {
    let msg = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    writeln!(out, "{msg}").unwrap();
    out.flush().unwrap();
}
