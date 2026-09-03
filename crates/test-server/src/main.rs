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
//! died-after-ready reconnection), `paged` (serves its tools over two
//! cursored pages — exercises a pipe forwarding pagination rather than
//! collapsing it), `legacy` (answers the way every server predating
//! 2026-07-28 does: no `resultType`, no caching fields), `modern` (the other
//! end of the matrix: 2026-07-28 only, no `initialize` at all, and one tool
//! that needs an MRTR round trip), `pid` (one tool
//! that names this process, slowly — what config reload is checked with,
//! since it has to prove both that an untouched server keeps the *same*
//! child and that a call already in flight still lands on it).
//!
//! `healthy` decorates its answers with the caching fields a 2026-07-28
//! server sends (`ttlMs`, `cacheScope`) and with `_meta`, because a pipe
//! that hands back anything less than what the upstream wrote is what
//! made a strict client reject the gateway's tools/list.

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
        mode => serve(mode),
    }
}

fn park() {
    std::thread::sleep(std::time::Duration::from_secs(3600));
}

fn serve(mode: &str) {
    let die_on_tools = mode == "die-on-tools";
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
        match reply(mode, method, &msg) {
            Some(Ok(result)) => respond(&mut stdout, &id, &result),
            Some(Err(failure)) => fail(&mut stdout, &id, &failure),
            // Methods this fixture does not implement get no answer at all,
            // which is what an unhandled request looks like in the wild.
            None => {}
        }
    }
}

/// JSON-RPC's "method not found". What a server answers for a method the
/// revision it speaks does not have — `initialize`, once sessions went.
const METHOD_NOT_FOUND: i64 = -32601;

/// The spec's resource-not-found code up to 2025-11-25. (2026-07-28 renumbers
/// it to `-32602`, invalid params; the SDK does that translation per peer, so
/// a fixture on the older revision keeps sending the older code.)
const RESOURCE_NOT_FOUND: i64 = -32002;

/// The scripted answer to one request: `None` for a method this fixture does
/// not implement, `Err` for a refusal the gateway is expected to carry
/// through as an error rather than as an empty result.
fn reply(
    mode: &str,
    method: &str,
    msg: &serde_json::Value,
) -> Option<Result<serde_json::Value, Failure>> {
    let params = &msg["params"];
    if mode == "modern" {
        return modern(method, params);
    }
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
        "tools/list" | "tools/call" => tools(mode, method, params),
        "resources/list" | "resources/templates/list" | "resources/read" => {
            return Some(resources(method, params));
        }
        "prompts/list" | "prompts/get" => prompts(method, params),
        "completion/complete" => complete(params),
        _ => return None,
    };
    Some(Ok(result))
}

/// The cursor the `paged` mode hands out for its second page. Deliberately
/// opaque: a pipe may only carry it, never interpret it.
const PAGE_TWO: &str = "fixture-cursor-page-2";

/// One tool entry, carrying a field no MCP model has ever heard of so the
/// suite can pin down how far a pipe's transparency actually reaches.
fn tool(name: &str, description: &str) -> serde_json::Value {
    serde_json::json!({
        "name": name,
        "description": description,
        "inputSchema": { "type": "object" },
        "x-fixture-tool": "alien"
    })
}

/// How long a `pid` call takes to answer. Long enough that a test can swap
/// the config out from under a call in flight and still be inside it.
const PID_CALL: std::time::Duration = std::time::Duration::from_millis(500);

fn tools(mode: &str, method: &str, params: &serde_json::Value) -> serde_json::Value {
    if mode == "pid" {
        if method == "tools/list" {
            return serde_json::json!({
                "tools": [tool("pid", "reports this fixture process's id")]
            });
        }
        std::thread::sleep(PID_CALL);
        return serde_json::json!({
            "content": [{ "type": "text", "text": std::process::id().to_string() }]
        });
    }
    if method == "tools/list" {
        if mode == "paged" {
            return paged_tools(params);
        }
        // What every server written before 2026-07-28 sends: the tools, and
        // nothing the newer revision made mandatory. A pipe with a client on
        // the newer revision has to answer for this server without inventing
        // anything it did not say.
        if mode == "legacy" {
            return serde_json::json!({
                "tools": [
                    tool("echo", "echoes input"),
                    tool("reverse", "reverses input")
                ]
            });
        }
        return serde_json::json!({
            "tools": [
                tool("echo", "echoes input"),
                tool("reverse", "reverses input")
            ],
            // What a 2026-07-28 server says about caching this answer
            // (SEP-2549). A client that asks for that protocol version
            // treats them as required, so a pipe that drops them turns a
            // perfectly good server into an invalid one.
            "ttlMs": 4242,
            "cacheScope": "public",
            "_meta": { "io.mcpgw.test/list": "verbatim" },
            "x-fixture-alien": { "deep": ["a", 1] }
        });
    }
    let message = params["arguments"]["message"].as_str().unwrap_or("");
    let text = match params["name"].as_str().unwrap_or("") {
        "echo" => message.to_owned(),
        "reverse" => message.chars().rev().collect(),
        other => format!("unknown tool {other}"),
    };
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "_meta": { "io.mcpgw.test/call": "verbatim" },
        "x-fixture-alien": "call"
    })
}

/// tools/list over two pages. Only the exact cursor from the first page
/// yields the second, so a pipe that invented its own pagination — or that
/// dropped the cursor the client sent — answers the wrong page here.
fn paged_tools(params: &serde_json::Value) -> serde_json::Value {
    match params["cursor"].as_str() {
        None => serde_json::json!({
            "tools": [tool("echo", "echoes input")],
            "nextCursor": PAGE_TWO
        }),
        Some(PAGE_TWO) => serde_json::json!({
            "tools": [tool("reverse", "reverses input")]
        }),
        // Reported rather than answered: a wrong cursor means the pipe
        // mangled it, and the test should see which one arrived.
        Some(other) => serde_json::json!({
            "tools": [],
            "_meta": { "io.mcpgw.test/unexpectedCursor": other }
        }),
    }
}

/// The opaque state the `ask` tool hands out with its `input_required`
/// answer. The client must echo it back untouched, and the pipe in between
/// must not so much as look at it.
const ASK_STATE: &str = "fixture-request-state-1";

/// A server that speaks 2026-07-28 and nothing else: no `initialize` (the
/// handshake is gone, SEP-2575), `server/discover` instead, every result
/// carrying `resultType` and the cacheable ones `ttlMs`/`cacheScope`
/// (SEP-2322, SEP-2549), and one tool that needs a round trip through the
/// client before it can answer (MRTR, SEP-2322).
///
/// This is the upstream half of the version matrix: a gateway that only knows
/// how to say `initialize` cannot talk to this server at all.
fn modern(method: &str, params: &serde_json::Value) -> Option<Result<serde_json::Value, Failure>> {
    let result = match method {
        // The handshake is not a method in this revision. Answering with an
        // error rather than silence is what lets a client that tried the old
        // lifecycle first learn to use the new one — and is what every
        // 2026-07-28 server does, since the method simply is not there.
        "initialize" => {
            return Some(Err(Failure {
                code: METHOD_NOT_FOUND,
                message: "initialize was removed in 2026-07-28; use server/discover".to_owned(),
            }));
        }
        "server/discover" => serde_json::json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            // Deliberately more than this fixture can back up, because what
            // a pipe in front of it advertises is the interesting part: the
            // notification-shaped promises (`subscribe`, `listChanged`,
            // `logging`) stop at the gateway, the tasks extension is a set of
            // methods it does not forward, and anything else — including an
            // extension nobody has heard of — is the server's to declare.
            "capabilities": {
                "tools": { "listChanged": true },
                "resources": { "subscribe": true, "listChanged": true },
                "prompts": {},
                "completions": {},
                "logging": {},
                "extensions": {
                    "io.modelcontextprotocol/tasks": {},
                    "com.example/thing": { "deep": true }
                }
            },
            "instructions": "the modern fixture",
            "ttlMs": 60000,
            "cacheScope": "public",
            "_meta": {
                "io.modelcontextprotocol/serverInfo": {
                    "name": "mcpgw-test-server-modern", "version": "9.9.9"
                }
            }
        }),
        "tools/list" => serde_json::json!({
            "tools": [
                tool("echo", "echoes input"),
                tool("ask", "asks the client something before it answers")
            ],
            "resultType": "complete",
            "ttlMs": 4242,
            "cacheScope": "public"
        }),
        "tools/call" => modern_call(params),
        "resources/list" | "resources/templates/list" | "resources/read" => {
            return Some(resources(method, params).map(|result| cacheable(result, "complete")));
        }
        "prompts/list" => cacheable(prompts(method, params), "complete"),
        "prompts/get" => complete_result(prompts(method, params)),
        "completion/complete" => complete_result(complete(params)),
        _ => return None,
    };
    Some(Ok(result))
}

/// `tools/call` on the modern fixture, including the MRTR round trip: `ask`
/// answers `input_required` the first time and completes on the retry that
/// carries the client's `inputResponses` and the echoed `requestState`.
fn modern_call(params: &serde_json::Value) -> serde_json::Value {
    if params["name"].as_str() == Some("ask") {
        let Some(responses) = params.get("inputResponses") else {
            return serde_json::json!({
                "resultType": "input_required",
                "requestState": ASK_STATE,
                "inputRequests": {
                    "city": {
                        "method": "elicitation/create",
                        "params": {
                            "mode": "form",
                            "message": "which city?",
                            "requestedSchema": {
                                "type": "object",
                                "properties": { "city": { "type": "string" } },
                                "required": ["city"]
                            }
                        }
                    }
                }
            });
        };
        // Both halves are checked, because both have to survive the pipe: the
        // client's answer, and the state this server minted for this round.
        let city = responses["city"]["content"]["city"]
            .as_str()
            .unwrap_or("nowhere");
        let state = params["requestState"].as_str().unwrap_or("lost");
        return complete_result(serde_json::json!({
            "content": [{ "type": "text", "text": format!("{city} ({state})") }]
        }));
    }
    let message = params["arguments"]["message"].as_str().unwrap_or("");
    complete_result(serde_json::json!({
        "content": [{ "type": "text", "text": message }]
    }))
}

/// Stamps the `resultType` every 2026-07-28 result carries.
fn complete_result(mut result: serde_json::Value) -> serde_json::Value {
    result["resultType"] = "complete".into();
    result
}

/// The same, plus the caching fields the revision requires on list and read
/// results. `public` and a real window, so a test can tell the upstream's own
/// policy from the "already stale, do not share" one a pipe falls back to.
fn cacheable(mut result: serde_json::Value, result_type: &str) -> serde_json::Value {
    result["resultType"] = result_type.into();
    result["ttlMs"] = 4242.into();
    result["cacheScope"] = "public".into();
    result
}

/// A JSON-RPC error a mode chose to answer with: the code matters, because a
/// client tells "this server has no such method" apart from "this server
/// refused" by reading it.
struct Failure {
    code: i64,
    message: String,
}

fn resources(method: &str, params: &serde_json::Value) -> Result<serde_json::Value, Failure> {
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
                return Err(Failure {
                    code: RESOURCE_NOT_FOUND,
                    message: format!("no such resource {uri}"),
                });
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

/// A JSON-RPC error reply.
fn fail(out: &mut impl std::io::Write, id: &serde_json::Value, failure: &Failure) {
    let msg = serde_json::json!({
        "jsonrpc": "2.0", "id": id,
        "error": { "code": failure.code, "message": failure.message }
    });
    writeln!(out, "{msg}").unwrap();
    out.flush().unwrap();
}
