//! Scripted stdio MCP server for tests. Deliberately implemented by hand
//! (not rmcp) so the probe client is exercised against an independent
//! implementation of the protocol.
//!
//! Serves two tools, one resource plus a template, one prompt and argument
//! completion — enough for a pipe to be checked on every request family it
//! forwards.
//!
//! Modes (first CLI argument): `healthy` (default), `slow` (never answers),
//! `garbage` (non-JSON output), `exit` (dies immediately), `die-on-tools`
//! (handshakes fine, then dies on the first tools/list — exercises
//! died-after-ready reconnection).

use std::io::{BufRead as _, Write as _};

/// The one resource this fixture serves, and its contents.
const RESOURCE_URI: &str = "mem:///greeting.txt";
const RESOURCE_TEXT: &str = "hello from the fixture";

/// Completion candidates for the `summarize` prompt's `topic` argument.
const TOPICS: [&str; 3] = ["gateways", "gators", "prompts"];

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
        if method == "tools/list" && die_on_tools {
            std::process::exit(0);
        }
        match reply(method, &msg) {
            Some(Ok(result)) => respond(&mut stdout, &id, &result),
            Some(Err(message)) => fail(&mut stdout, &id, &message),
            // Methods this fixture does not implement get no answer at all,
            // which is what an unhandled request looks like in the wild.
            None => {}
        }
    }
}

/// The scripted answer to one request: `None` for a method this fixture does
/// not implement, `Err` for a refusal the gateway is expected to carry
/// through as an error rather than as an empty result.
fn reply(method: &str, msg: &serde_json::Value) -> Option<Result<serde_json::Value, String>> {
    let params = &msg["params"];
    let result = match method {
        "initialize" => {
            // Echo the client's protocol version back so any rmcp release
            // negotiates successfully.
            let proto = params["protocolVersion"].as_str().unwrap_or("2025-06-18");
            serde_json::json!({
                "protocolVersion": proto,
                // Everything this fixture implements, and nothing more:
                // `subscribe` and the `listChanged` flags stay off because
                // nothing here sends notifications.
                "capabilities": {
                    "tools": {},
                    "resources": {},
                    "prompts": {},
                    "completions": {}
                },
                "serverInfo": { "name": "mcpgw-test-server", "version": "9.9.9" }
            })
        }
        "ping" => serde_json::json!({}),
        "tools/list" | "tools/call" => tools(method, params),
        "resources/list" | "resources/templates/list" | "resources/read" => {
            return Some(resources(method, params));
        }
        "prompts/list" | "prompts/get" => prompts(method, params),
        "completion/complete" => complete(params),
        _ => return None,
    };
    Some(Ok(result))
}

fn tools(method: &str, params: &serde_json::Value) -> serde_json::Value {
    if method == "tools/list" {
        return serde_json::json!({
            "tools": [
                { "name": "echo", "description": "echoes input",
                  "inputSchema": { "type": "object" } },
                { "name": "reverse", "description": "reverses input",
                  "inputSchema": { "type": "object" } }
            ]
        });
    }
    let message = params["arguments"]["message"].as_str().unwrap_or("");
    let text = match params["name"].as_str().unwrap_or("") {
        "echo" => message.to_owned(),
        "reverse" => message.chars().rev().collect(),
        other => format!("unknown tool {other}"),
    };
    serde_json::json!({ "content": [{ "type": "text", "text": text }] })
}

fn resources(method: &str, params: &serde_json::Value) -> Result<serde_json::Value, String> {
    match method {
        "resources/list" => Ok(serde_json::json!({
            "resources": [
                { "uri": RESOURCE_URI, "name": "greeting", "mimeType": "text/plain" }
            ]
        })),
        "resources/templates/list" => Ok(serde_json::json!({
            "resourceTemplates": [
                { "uriTemplate": "mem:///{name}.txt", "name": "memo",
                  "mimeType": "text/plain" }
            ]
        })),
        _ => {
            let uri = params["uri"].as_str().unwrap_or("");
            if uri != RESOURCE_URI {
                return Err(format!("no such resource {uri}"));
            }
            Ok(serde_json::json!({
                "contents": [
                    { "uri": uri, "mimeType": "text/plain", "text": RESOURCE_TEXT }
                ]
            }))
        }
    }
}

fn prompts(method: &str, params: &serde_json::Value) -> serde_json::Value {
    if method == "prompts/list" {
        return serde_json::json!({
            "prompts": [
                { "name": "summarize", "description": "summarizes a topic",
                  "arguments": [
                      { "name": "topic", "description": "what to summarize",
                        "required": true }
                  ] }
            ]
        });
    }
    let topic = params["arguments"]["topic"].as_str().unwrap_or("nothing");
    serde_json::json!({
        "description": "summarizes a topic",
        "messages": [
            { "role": "user",
              "content": { "type": "text", "text": format!("summarize {topic}") } }
        ]
    })
}

fn complete(params: &serde_json::Value) -> serde_json::Value {
    let prefix = params["argument"]["value"].as_str().unwrap_or("");
    let values: Vec<&str> = TOPICS
        .iter()
        .copied()
        .filter(|topic| topic.starts_with(prefix))
        .collect();
    serde_json::json!({
        "completion": { "values": values, "total": values.len(), "hasMore": false }
    })
}

fn respond(out: &mut impl std::io::Write, id: &serde_json::Value, result: &serde_json::Value) {
    let msg = serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result });
    writeln!(out, "{msg}").unwrap();
    out.flush().unwrap();
}

/// A JSON-RPC error reply. `-32002` is the spec's "resource not found".
fn fail(out: &mut impl std::io::Write, id: &serde_json::Value, message: &str) {
    let msg = serde_json::json!({
        "jsonrpc": "2.0", "id": id,
        "error": { "code": -32002, "message": message }
    });
    writeln!(out, "{msg}").unwrap();
    out.flush().unwrap();
}
