use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mcpgw_core::capture::{
    Bodies, CapturePolicy, CaptureRecord, CaptureWriter, Kind, MAX_BODY_BYTES, TRUNCATION_MARKER,
};
use mcpgw_core::endpoints::{EndpointTable, Endpoints, endpoint_path};
use mcpgw_core::gateway::{
    Gateway, GatewayAuth, MAX_TOOL_PAGES, NO_TOOLS_HERE, not_allowed, over_budget, serve_http,
    serve_http_with,
};
use mcpgw_core::gateway_token::GatewayToken;
use mcpgw_core::pins::Change;
use mcpgw_core::upstream::{CallError, UpstreamManager, UpstreamStatus};
use mcpgw_core::{Server, Transport};
use rmcp_client_http::ServiceExt as _;
use rmcp_client_http::transport::StreamableHttpClientTransport;

fn stdio_server(mode: &str) -> Server {
    Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Stdio {
            command: env!("CARGO_BIN_EXE_mcpgw-test-server").to_owned(),
            args: vec![mode.to_owned()],
            env: BTreeMap::new(),
        },
    }
}

type Client = rmcp_client_http::service::RunningService<rmcp_client_http::RoleClient, ()>;

fn manager(upstreams: &[(&str, &str)]) -> Arc<UpstreamManager> {
    let servers: BTreeMap<String, Server> = upstreams
        .iter()
        .map(|(name, mode)| ((*name).to_owned(), stdio_server(mode)))
        .collect();
    Arc::new(
        UpstreamManager::new(servers)
            .with_connect_timeout(Duration::from_secs(30))
            .with_backoff_base(Duration::from_millis(20)),
    )
}

/// Serves `gateway` as `name`'s endpoint on an ephemeral port and returns
/// the bound address.
async fn serve(name: &str, gateway: Gateway) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(
        name.to_owned(),
        gateway,
        listener,
        std::future::pending(),
    ));
    addr
}

/// The same, with a client connected to that endpoint.
async fn connect(name: &str, gateway: Gateway) -> Client {
    let addr = serve(name, gateway).await;
    let transport =
        StreamableHttpClientTransport::from_uri(format!("http://{addr}{}", endpoint_path(name)));
    ().serve(transport).await.unwrap()
}

/// Boots a gateway piping one fixture upstream named `fx` and returns a
/// connected MCP client plus the manager for shutdown.
async fn gateway_client(mode: &str) -> (Client, Arc<UpstreamManager>) {
    let manager = manager(&[("fx", mode)]);
    let client = connect("fx", Gateway::new(Arc::clone(&manager), "fx".to_owned())).await;
    (client, manager)
}

fn call(tool: &str, message: &str) -> rmcp_client_http::model::CallToolRequestParams {
    rmcp_client_http::model::CallToolRequestParams::new(tool.to_owned()).with_arguments(
        serde_json::json!({ "message": message })
            .as_object()
            .cloned()
            .unwrap(),
    )
}

#[tokio::test]
async fn pipes_tools_list_from_upstream() {
    let (client, manager) = gateway_client("healthy").await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["echo", "reverse"]);

    // This session was opened before the upstream had ever been reached, so
    // the gateway had nothing to mirror and named itself.
    let info = client.peer_info().unwrap();
    assert_eq!(info.server_info.as_ref().unwrap().name, "mcpgw");
    manager.shutdown().await;
}

#[tokio::test]
async fn pipes_tool_calls_both_ways() {
    let (client, manager) = gateway_client("healthy").await;
    let result = client.call_tool(call("reverse", "mcpgw")).await.unwrap();
    let text = format!("{result:?}");
    assert!(text.contains("wgpcm"), "{text}");
    manager.shutdown().await;
}

#[tokio::test]
async fn dead_upstream_surfaces_as_loud_error() {
    let (client, manager) = gateway_client("exit").await;
    let err = client.list_all_tools().await.unwrap_err();
    let text = err.to_string();
    assert!(
        text.contains("fx"),
        "error should name the upstream: {text}"
    );
    manager.shutdown().await;
}

/// Without a deadline on the request path a client waits out the whole
/// connect ladder (~93s at the defaults) and, for a call that reaches a hung
/// server, forever. It now gets a named error instead.
#[tokio::test]
async fn a_hung_upstream_fails_the_request_on_the_deadline() {
    // The `slow` fixture never answers the handshake, so the request can only
    // end on the deadline.
    let manager = Arc::new(
        UpstreamManager::new(
            [("fx".to_owned(), stdio_server("slow"))]
                .into_iter()
                .collect(),
        )
        .with_connect_timeout(Duration::from_secs(30)),
    );
    let gateway = Gateway::new(Arc::clone(&manager), "fx".to_owned())
        .with_request_timeout(Duration::from_millis(300));
    let client = connect("fx", gateway).await;

    let started = std::time::Instant::now();
    let err = client.call_tool(call("echo", "hi")).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("fx"), "should name the upstream: {text}");
    assert!(text.contains("deadline"), "should say why: {text}");
    // Generous on purpose: the alternative this guards against is waiting out
    // the connect ladder, which is tens of seconds. Anything well inside that
    // proves the deadline fired, without asking a loaded runner to be quick.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "waited {:?}, so nothing bounded the request",
        started.elapsed()
    );

    // tools/list is on the same clock.
    let err = client.list_all_tools().await.unwrap_err();
    assert!(err.to_string().contains("fx"), "{err}");

    client.cancel().await.unwrap();
    manager.shutdown().await;
}

/// Chains a second gateway on top of `gateway`: gw1 is served over http and
/// becomes gw2's upstream, so requests travel client → gw2 → http → gw1 →
/// stdio fixture.
async fn chained_client(upstream: &str) -> (Client, Arc<UpstreamManager>, Arc<UpstreamManager>) {
    let inner = manager(&[("fx", "healthy")]);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let gw1 = Gateway::new(Arc::clone(&inner), "fx".to_owned());
    tokio::spawn(serve_http(
        "fx".to_owned(),
        gw1,
        listener,
        std::future::pending(),
    ));

    let remote = Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            url: format!("http://{addr}{}", endpoint_path("fx")),
            headers_command: Vec::new(),
            headers: BTreeMap::new(),
            auth: None,
        },
    };
    let outer = Arc::new(
        UpstreamManager::new([(upstream.to_owned(), remote)].into_iter().collect())
            .with_connect_timeout(Duration::from_secs(30))
            .with_backoff_base(Duration::from_millis(20)),
    );
    let client = connect(
        upstream,
        Gateway::new(Arc::clone(&outer), upstream.to_owned()),
    )
    .await;
    (client, outer, inner)
}

#[tokio::test]
async fn http_upstream_lists_tools_through_the_chain() {
    let (client, outer, inner) = chained_client("remote").await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["echo", "reverse"]);
    outer.shutdown().await;
    inner.shutdown().await;
}

#[tokio::test]
async fn http_upstream_round_trips_a_tool_call() {
    let (client, outer, inner) = chained_client("remote").await;
    let result = client.call_tool(call("reverse", "mcpgw")).await.unwrap();
    let text = format!("{result:?}");
    assert!(text.contains("wgpcm"), "{text}");
    outer.shutdown().await;
    inner.shutdown().await;
}

/// Every JSONL line written into `dir`, sorted so the parallel tools/list
/// records land in a deterministic order.
fn captured(dir: &std::path::Path) -> Vec<CaptureRecord> {
    let mut records: Vec<CaptureRecord> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .flat_map(|text| {
            text.lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<CaptureRecord>>()
        })
        .collect();
    records.sort_by(|a, b| (a.ts, &a.server).cmp(&(b.ts, &b.server)));
    records
}

#[tokio::test]
async fn capture_records_every_upstream_list_and_call() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let (addr, manager) = serve_both(&[("fx1", "healthy"), ("fx2", "exit")], Some(&writer)).await;
    let one = client_at(addr, &endpoint_path("fx1")).await;
    let two = client_at(addr, &endpoint_path("fx2")).await;

    // Two lists, one of them against an upstream that cannot start, one good
    // call and one call into that dead upstream.
    one.list_all_tools().await.unwrap();
    two.list_all_tools().await.unwrap_err();
    one.call_tool(call("echo", "hi")).await.unwrap();
    two.call_tool(call("echo", "hi")).await.unwrap_err();
    for client in [one, two] {
        client.cancel().await.unwrap();
    }
    manager.shutdown().await;

    let records = captured(writer.dir());
    let shape: Vec<(Kind, &str, Option<&str>, bool)> = records
        .iter()
        .map(|r| (r.kind, r.server.as_str(), r.tool.as_deref(), r.ok))
        .collect();
    assert_eq!(
        shape,
        [
            (Kind::List, "fx1", None, true),
            (Kind::List, "fx2", None, false),
            (Kind::Call, "fx1", Some("echo"), true),
            (Kind::Call, "fx2", Some("echo"), false),
        ],
        "{records:#?}"
    );

    // Both records of one client carry that client's own session id, and
    // neither client is filed under the gateway process's fallback.
    assert_eq!(records[0].session, records[2].session, "{records:#?}");
    assert_eq!(records[1].session, records[3].session, "{records:#?}");
    assert_ne!(records[0].session, records[1].session, "{records:#?}");
    assert_ne!(records[0].session, writer.session());

    // Calls carry both sides of the exchange.
    let good_call = &records[2];
    assert_eq!(good_call.args.as_deref(), Some(r#"{"message":"hi"}"#));
    assert!(good_call.response.as_deref().unwrap().contains("hi"));
    assert!(good_call.error.is_none());

    // The failed upstream names itself in the error of both its records.
    assert!(records[1].error.as_deref().unwrap().contains("fx2"));
    assert!(records[3].error.as_deref().unwrap().contains("fx2"));
}

#[tokio::test]
async fn capture_truncates_oversized_bodies() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let manager = manager(&[("fx", "healthy")]);
    let gateway =
        Gateway::new(Arc::clone(&manager), "fx".to_owned()).with_capture(Arc::clone(&writer));
    let client = connect("fx", gateway).await;

    // Multibyte payload well past the cap, echoed back just as long.
    let message = "é".repeat(MAX_BODY_BYTES);
    client.call_tool(call("echo", &message)).await.unwrap();
    client.cancel().await.unwrap();
    manager.shutdown().await;

    let records = captured(writer.dir());
    assert_eq!(records.len(), 1, "{records:#?}");
    for body in [&records[0].args, &records[0].response] {
        let body = body.as_deref().unwrap();
        assert!(body.ends_with(TRUNCATION_MARKER), "{body}");
        let kept = body.strip_suffix(TRUNCATION_MARKER).unwrap();
        assert!(kept.len() <= MAX_BODY_BYTES);
    }
}

/// A token handed to a tool as an argument, which the fixture echoes back —
/// so one call puts the same credential on both sides of a record.
const FAKE_TOKEN: &str = "ghp_0123456789abcdefghij";

#[tokio::test]
async fn a_credential_in_a_tool_argument_never_reaches_the_file() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let manager = manager(&[("fx", "healthy")]);
    let gateway =
        Gateway::new(Arc::clone(&manager), "fx".to_owned()).with_capture(Arc::clone(&writer));
    let client = connect("fx", gateway).await;

    client.call_tool(call("echo", FAKE_TOKEN)).await.unwrap();
    client.cancel().await.unwrap();
    manager.shutdown().await;

    // The raw bytes, not the parsed record: what is on disk is the claim.
    let file = daily_file(writer.dir());
    assert!(!file.contains(FAKE_TOKEN), "{file}");
    assert!(file.contains("[redacted:ghp_…]"), "{file}");

    let records = captured(writer.dir());
    assert_eq!(records.len(), 1, "{records:#?}");
    assert_eq!(records[0].bodies, Bodies::Redacted);
    // Still a usable record: the tool, the outcome and the argument's shape
    // are all there.
    assert_eq!(records[0].tool.as_deref(), Some("echo"));
    assert!(records[0].args.as_deref().unwrap().contains("message"));
}

/// The escape hatch, and the reason it needs one: `full` is what people
/// debugging a tool call want, and it is not the default.
#[tokio::test]
async fn capture_bodies_full_keeps_the_credential() {
    let state = tempfile::tempdir().unwrap();
    let writer =
        Arc::new(CaptureWriter::under_state_dir(state.path()).with_policy(CapturePolicy::full()));
    let manager = manager(&[("fx", "healthy")]);
    let gateway =
        Gateway::new(Arc::clone(&manager), "fx".to_owned()).with_capture(Arc::clone(&writer));
    let client = connect("fx", gateway).await;

    client.call_tool(call("echo", FAKE_TOKEN)).await.unwrap();
    client.cancel().await.unwrap();
    manager.shutdown().await;

    let file = daily_file(writer.dir());
    assert!(file.contains(FAKE_TOKEN), "{file}");
}

/// Everything in the traffic dir, as bytes. Redaction is a claim about what
/// is written, so the tests that make it read the file rather than a record
/// that has been through serde twice.
fn daily_file(dir: &std::path::Path) -> String {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect()
}

#[tokio::test]
async fn capture_is_off_unless_asked_for() {
    let state = tempfile::tempdir().unwrap();
    let (client, manager) = gateway_client("healthy").await;
    client.list_all_tools().await.unwrap();
    client.cancel().await.unwrap();
    manager.shutdown().await;
    // Nothing constructed a writer, so no traffic dir was ever created.
    assert!(!state.path().join("traffic").exists());
}

/// Sends one raw HTTP request so the test can set an arbitrary `Origin`
/// without adding an HTTP client to the dev-dependencies, and returns the
/// status line of the response.
async fn raw_post(addr: std::net::SocketAddr, origin: Option<&str>) -> String {
    raw_post_to(addr, "/mcp", origin)
        .await
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
}

/// The same, aimed at an arbitrary path and keeping the whole response so a
/// test can read the body as well as the status line.
async fn raw_post_to(addr: std::net::SocketAddr, path: &str, origin: Option<&str>) -> String {
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    raw_post_body(addr, path, origin, &[], body).await
}

/// One raw POST with a caller-supplied body and optional session header,
/// returning headers and body together — the whole point being that nothing
/// on this path parses the answer into a typed model on the way past.
async fn raw_post_body(
    addr: std::net::SocketAddr,
    path: &str,
    origin: Option<&str>,
    extra: &[(&str, &str)],
    body: &str,
) -> String {
    use std::fmt::Write as _;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut request = format!(
        "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(origin) = origin {
        let _ = write!(request, "Origin: {origin}\r\n");
    }
    for (name, value) in extra {
        let _ = write!(request, "{name}: {value}\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8_lossy(&response).into_owned()
}

#[tokio::test]
async fn a_hostile_origin_is_refused_before_it_reaches_the_gateway() {
    let manager = manager(&[("fx", "healthy")]);
    let addr = serve("fx", Gateway::new(Arc::clone(&manager), "fx".to_owned())).await;

    // DNS rebinding: the page's own domain resolves to loopback, so only the
    // Origin header tells the two apart.
    let status = raw_post(addr, Some("https://evil.example")).await;
    assert!(status.contains("403"), "{status}");
    let status = raw_post(addr, Some("http://localhost.evil.example")).await;
    assert!(status.contains("403"), "{status}");

    // A loopback page and a non-browser client (no Origin at all) both pass
    // the guard — whatever the gateway answers, it is not a 403.
    for origin in [Some("http://localhost:8137"), None] {
        let status = raw_post(addr, origin).await;
        assert!(!status.contains("403"), "{origin:?} -> {status}");
    }
    manager.shutdown().await;
}

/// Serves one pipe endpoint per upstream at `/s/<name>`, next to the base
/// endpoint, all over the one shared manager, and returns the address.
async fn serve_both(
    upstreams: &[(&str, &str)],
    capture: Option<&Arc<CaptureWriter>>,
) -> (std::net::SocketAddr, Arc<UpstreamManager>) {
    let manager = manager(upstreams);
    let names: Vec<String> = upstreams.iter().map(|(n, _)| (*n).to_owned()).collect();
    let with_capture = |gateway: Gateway| match capture {
        Some(writer) => gateway.with_capture(Arc::clone(writer)),
        None => gateway,
    };
    let pipes: Vec<(String, Gateway)> = names
        .iter()
        .map(|name| {
            (
                name.clone(),
                with_capture(Gateway::new(Arc::clone(&manager), name.clone())),
            )
        })
        .collect();
    let endpoints = Endpoints::new(EndpointTable::new(pipes));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http_with(
        endpoints,
        GatewayAuth::open(),
        listener,
        std::future::pending(),
    ));
    (addr, manager)
}

/// A client on an arbitrary path of the gateway.
async fn client_at(addr: std::net::SocketAddr, path: &str) -> Client {
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}{path}"));
    ().serve(transport).await.unwrap()
}

fn tool_names(tools: &[rmcp_client_http::model::Tool]) -> Vec<&str> {
    tools.iter().map(|t| t.name.as_ref()).collect()
}

/// The point of the endpoint table: two simultaneous views of the same
/// upstreams, one per server, sharing a single `UpstreamManager`, so no view
/// costs an extra process.
#[tokio::test]
async fn per_server_endpoints_serve_their_own_tools() {
    let (addr, manager) = serve_both(&[("fx1", "healthy"), ("fx2", "healthy")], None).await;

    let fx1 = client_at(addr, &endpoint_path("fx1")).await;
    let fx2 = client_at(addr, &endpoint_path("fx2")).await;

    let (fx1_tools, fx2_tools) = tokio::join!(fx1.list_all_tools(), fx2.list_all_tools());
    // Every endpoint hands out its own server's tools, under their own names.
    assert_eq!(tool_names(&fx1_tools.unwrap()), ["echo", "reverse"]);
    assert_eq!(tool_names(&fx2_tools.unwrap()), ["echo", "reverse"]);

    // And the unprefixed call lands on the server whose endpoint took it.
    let (one, two) = tokio::join!(
        fx1.call_tool(call("echo", "from fx1")),
        fx2.call_tool(call("reverse", "abcd")),
    );
    assert!(format!("{:?}", one.unwrap()).contains("from fx1"));
    assert!(format!("{:?}", two.unwrap()).contains("dcba"));

    for client in [fx1, fx2] {
        client.cancel().await.unwrap();
    }
    manager.shutdown().await;
}

/// What the fixture serves, spelled out here so a test failure says which
/// side drifted.
const RESOURCE_URI: &str = "mem:///greeting.txt";
const RESOURCE_TEXT: &str = "hello from the fixture";

fn read(uri: &str) -> rmcp_client_http::model::ReadResourceRequestParams {
    rmcp_client_http::model::ReadResourceRequestParams::new(uri.to_owned())
}

fn prompt(name: &str, topic: &str) -> rmcp_client_http::model::GetPromptRequestParams {
    rmcp_client_http::model::GetPromptRequestParams::new(name.to_owned()).with_arguments(
        serde_json::json!({ "topic": topic })
            .as_object()
            .cloned()
            .unwrap(),
    )
}

/// A pipe is not a tools-only bridge: everything a client can ask an MCP
/// server, it can ask through `/s/<name>`.
#[tokio::test]
async fn a_pipe_forwards_resources_and_their_contents() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let fx = client_at(addr, &endpoint_path("fx")).await;

    let resources = fx.list_all_resources().await.unwrap();
    let uris: Vec<&str> = resources.iter().map(|r| r.uri.as_str()).collect();
    assert_eq!(uris, [RESOURCE_URI]);

    let templates = fx.list_all_resource_templates().await.unwrap();
    let patterns: Vec<&str> = templates.iter().map(|t| t.uri_template.as_str()).collect();
    assert_eq!(patterns, ["mem:///{name}.txt"]);

    // The contents come back byte for byte, not a summary of them.
    let contents = fx.read_resource(read(RESOURCE_URI)).await.unwrap();
    let text = format!("{contents:?}");
    assert!(text.contains(RESOURCE_TEXT), "{text}");

    // And an upstream refusal stays a refusal instead of an empty read.
    let err = fx.read_resource(read("mem:///nope.txt")).await.unwrap_err();
    assert!(err.to_string().contains("no such resource"), "{err}");

    fx.cancel().await.unwrap();
    manager.shutdown().await;
}

#[tokio::test]
async fn a_pipe_forwards_prompts_and_their_messages() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let fx = client_at(addr, &endpoint_path("fx")).await;

    let prompts = fx.list_all_prompts().await.unwrap();
    let names: Vec<&str> = prompts.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["summarize"]);

    // Arguments travel with the request, so the rendered message is the one
    // the upstream built for them.
    let got = fx
        .get_prompt(prompt("summarize", "gateways"))
        .await
        .unwrap();
    let text = format!("{got:?}");
    assert!(text.contains("summarize gateways"), "{text}");

    fx.cancel().await.unwrap();
    manager.shutdown().await;
}

#[tokio::test]
async fn a_pipe_forwards_argument_completion() {
    use rmcp_client_http::model::{ArgumentInfo, CompleteRequestParams, Reference};

    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let fx = client_at(addr, &endpoint_path("fx")).await;

    let request = CompleteRequestParams::new(
        Reference::for_prompt("summarize"),
        ArgumentInfo::new("topic", "gat"),
    );
    let completion = fx.complete(request).await.unwrap().completion;
    assert_eq!(completion.values, ["gateways", "gators"]);

    fx.cancel().await.unwrap();
    manager.shutdown().await;
}

/// Capabilities are the contract a client reads once, at `initialize`. A pipe
/// that forwards prompts while claiming only tools is a pipe whose prompts
/// nobody ever asks for.
#[tokio::test]
async fn a_pipe_advertises_what_its_upstream_can_do() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;

    // Before first contact there is nothing to report, and `initialize` must
    // not go start the upstream to find out: tools only, honestly.
    let early = client_at(addr, &endpoint_path("fx")).await;
    let capabilities = early.peer_info().unwrap().capabilities.clone();
    assert!(capabilities.tools.is_some());
    assert!(
        capabilities.resources.is_none() && capabilities.prompts.is_none(),
        "pre-contact should not promise what it has not seen: {capabilities:?}"
    );

    // One request reaches the upstream, and every session opened afterwards
    // sees the real set.
    early.list_all_tools().await.unwrap();
    let later = client_at(addr, &endpoint_path("fx")).await;
    let capabilities = later.peer_info().unwrap().capabilities.clone();
    assert!(capabilities.resources.is_some(), "{capabilities:?}");
    assert!(capabilities.prompts.is_some(), "{capabilities:?}");
    assert!(capabilities.completions.is_some(), "{capabilities:?}");
    // Only what the gateway actually implements: nothing here forwards
    // subscriptions or list-changed notifications.
    assert!(
        capabilities.resources.as_ref().unwrap().subscribe != Some(true),
        "{capabilities:?}"
    );

    for client in [early, later] {
        client.cancel().await.unwrap();
    }
    manager.shutdown().await;
}

/// The forwarded families ride the same request deadline as `tools/call`;
/// none of them can leave a client waiting on a server that never answers.
#[tokio::test]
async fn a_hung_upstream_fails_a_forwarded_request_on_the_deadline() {
    let manager = Arc::new(
        UpstreamManager::new(
            [("fx".to_owned(), stdio_server("slow"))]
                .into_iter()
                .collect(),
        )
        .with_connect_timeout(Duration::from_secs(30)),
    );
    let gateway = Gateway::new(Arc::clone(&manager), "fx".to_owned())
        .with_request_timeout(Duration::from_millis(300));
    let client = connect("fx", gateway).await;

    let started = std::time::Instant::now();
    let err = client.list_all_prompts().await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("fx"), "should name the upstream: {text}");
    assert!(text.contains("deadline"), "should say why: {text}");
    // Generous on purpose: the alternative this guards against is waiting out
    // the connect ladder, which is tens of seconds. Anything well inside that
    // proves the deadline fired, without asking a loaded runner to be quick.
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "waited {:?}, so nothing bounded the request",
        started.elapsed()
    );

    client.cancel().await.unwrap();
    manager.shutdown().await;
}

/// Every forwarded family lands in the traffic log under its own kind, so
/// `watch` and `jq` can tell a prompt fetch from a tool call.
#[tokio::test]
async fn forwarded_families_are_captured_under_their_own_kinds() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let (addr, manager) = serve_both(&[("fx", "healthy")], Some(&writer)).await;
    let fx = client_at(addr, &endpoint_path("fx")).await;

    fx.list_all_resources().await.unwrap();
    fx.read_resource(read(RESOURCE_URI)).await.unwrap();
    fx.list_all_prompts().await.unwrap();
    fx.get_prompt(prompt("summarize", "gateways"))
        .await
        .unwrap();
    fx.cancel().await.unwrap();
    manager.shutdown().await;

    let records = captured(writer.dir());
    let shape: Vec<(Kind, Option<&str>, bool)> = records
        .iter()
        .map(|r| (r.kind, r.tool.as_deref(), r.ok))
        .collect();
    assert_eq!(
        shape,
        [
            (Kind::Resources, None, true),
            (Kind::ResourceRead, Some(RESOURCE_URI), true),
            (Kind::Prompts, None, true),
            (Kind::PromptGet, Some("summarize"), true),
        ],
        "{records:#?}"
    );
    // The bodies are there too: a captured read is worth having only if the
    // contents can be read back out of it.
    let read = &records[1];
    assert!(read.response.as_deref().unwrap().contains(RESOURCE_TEXT));
}

#[tokio::test]
async fn an_unknown_endpoint_is_a_404_that_names_the_real_ones() {
    let (addr, manager) = serve_both(&[("fx1", "healthy"), ("fx2", "healthy")], None).await;

    let response = raw_post_to(addr, "/s/nope", None).await;
    assert!(response.contains("404"), "{response}");
    assert!(response.contains("nope"), "{response}");
    assert!(
        response.contains("/s/fx1") && response.contains("/s/fx2"),
        "the 404 should list what is actually served: {response}"
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn per_server_traffic_is_captured_under_the_right_server() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let (addr, manager) =
        serve_both(&[("fx1", "healthy"), ("fx2", "healthy")], Some(&writer)).await;

    let fx2 = client_at(addr, &endpoint_path("fx2")).await;
    fx2.call_tool(call("echo", "hi")).await.unwrap();
    fx2.cancel().await.unwrap();
    manager.shutdown().await;

    let records = captured(writer.dir());
    let shape: Vec<(Kind, &str, Option<&str>, bool)> = records
        .iter()
        .map(|r| (r.kind, r.server.as_str(), r.tool.as_deref(), r.ok))
        .collect();
    // The endpoint, not the tool name, is what attributes the call — the
    // name arriving here is bare.
    assert_eq!(
        shape,
        [(Kind::Call, "fx2", Some("echo"), true)],
        "{records:#?}"
    );
    assert_eq!(records[0].endpoint.as_deref(), Some("s/fx2"));
}

/// The reason N13 exists: a daemon serving several harnesses at once has to
/// be able to say which of them made a call. Two clients on two endpoints
/// must not share a session id, and each record must name the face it
/// arrived on.
#[tokio::test]
async fn concurrent_clients_are_attributed_to_their_own_sessions_and_endpoints() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let (addr, manager) =
        serve_both(&[("fx1", "healthy"), ("fx2", "healthy")], Some(&writer)).await;

    let one = client_at(addr, &endpoint_path("fx1")).await;
    let two = client_at(addr, &endpoint_path("fx2")).await;
    let (first, second) = tokio::join!(
        one.call_tool(call("echo", "from one")),
        two.call_tool(call("echo", "from two")),
    );
    first.unwrap();
    second.unwrap();
    for client in [one, two] {
        client.cancel().await.unwrap();
    }
    manager.shutdown().await;

    let records = captured(writer.dir());
    let by_endpoint: BTreeMap<&str, &CaptureRecord> = records
        .iter()
        .map(|r| (r.endpoint.as_deref().unwrap_or("<none>"), r))
        .collect();
    assert_eq!(
        by_endpoint.keys().copied().collect::<Vec<_>>(),
        ["s/fx1", "s/fx2"],
        "{records:#?}"
    );
    assert_eq!(by_endpoint["s/fx1"].server, "fx1");
    assert_eq!(by_endpoint["s/fx2"].server, "fx2");

    // Two connections, two distinct sessions, neither of them the
    // per-process fallback.
    let sessions: std::collections::BTreeSet<&str> =
        records.iter().map(|r| r.session.as_str()).collect();
    assert_eq!(sessions.len(), 2, "{records:#?}");
    assert!(!sessions.contains(writer.session()), "{records:#?}");
}

/// The id follows the transport session, not the client: reconnecting is a
/// new session and reads as one, which is what makes "who is calling right
/// now" answerable at all.
#[tokio::test]
async fn reconnecting_starts_a_new_session() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let (addr, manager) = serve_both(&[("fx", "healthy")], Some(&writer)).await;

    for message in ["first", "second"] {
        let client = client_at(addr, &endpoint_path("fx")).await;
        client.call_tool(call("echo", message)).await.unwrap();
        client.cancel().await.unwrap();
    }
    manager.shutdown().await;

    let records = captured(writer.dir());
    assert_eq!(records.len(), 2, "{records:#?}");
    assert_ne!(records[0].session, records[1].session, "{records:#?}");
    assert!(
        records
            .iter()
            .all(|r| r.endpoint.as_deref() == Some("s/fx")),
        "{records:#?}"
    );
}

#[tokio::test]
async fn the_origin_guard_covers_the_per_server_endpoints_too() {
    let (addr, manager) = serve_both(&[("fx1", "healthy")], None).await;

    let response = raw_post_to(addr, "/s/fx1", Some("https://evil.example")).await;
    assert!(response.contains("403"), "{response}");
    // Even an endpoint that does not exist is refused before it is looked up.
    let response = raw_post_to(addr, "/s/nope", Some("https://evil.example")).await;
    assert!(response.contains("403"), "{response}");

    // A non-browser client (no Origin) is untouched by the guard.
    let response = raw_post_to(addr, "/s/fx1", None).await;
    assert!(!response.contains("403"), "{response}");
    manager.shutdown().await;
}

/// A downstream MCP session driven as raw JSON-RPC.
///
/// Every other test here talks through an rmcp client, which decodes what it
/// receives into rmcp's models — exactly the step that was laundering the
/// upstream's answers, and therefore the one thing that must not stand
/// between the assertion and the wire. These tests read the JSON the gateway
/// actually wrote.
struct RawSession {
    addr: std::net::SocketAddr,
    path: String,
    id: u32,
    session: Option<String>,
}

impl RawSession {
    /// Opens a session on `path`: initialize, then the notification that
    /// completes the handshake.
    async fn open(addr: std::net::SocketAddr, path: &str) -> Self {
        Self::open_as(addr, path, "2026-07-28", "raw", "0").await
    }

    /// The same, naming the revision and the client the handshake claims —
    /// which is the only place a client on a revision with a handshake says
    /// who it is.
    async fn open_as(
        addr: std::net::SocketAddr,
        path: &str,
        version: &str,
        client: &str,
        client_version: &str,
    ) -> Self {
        let mut raw = Self {
            addr,
            path: path.to_owned(),
            id: 0,
            session: None,
        };
        let response = raw
            .post(&format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"initialize","params":{{
                    "protocolVersion":"{version}","capabilities":{{}},
                    "clientInfo":{{"name":"{client}","version":"{client_version}"}}}}}}"#
            ))
            .await;
        raw.session = response
            .lines()
            .find_map(|line| {
                let value = line.strip_prefix("mcp-session-id: ")?;
                Some(value.trim().to_owned())
            })
            .or_else(|| panic!("no session id in: {response}"));
        raw.post(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
            .await;
        raw
    }

    async fn post(&self, body: &str) -> String {
        let session: Vec<(&str, &str)> = self
            .session
            .as_deref()
            .map(|id| ("Mcp-Session-Id", id))
            .into_iter()
            .collect();
        raw_post_body(self.addr, &self.path, None, &session, body).await
    }

    /// Sends one request and returns its `result` object verbatim.
    async fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let message = self.attempt(method, params).await;
        assert!(message.get("error").is_none(), "{method} failed: {message}");
        message["result"].clone()
    }

    /// The same, handing back the whole JSON-RPC message so a test can assert
    /// on a failure the gateway is right to produce.
    async fn attempt(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": self.id, "method": method, "params": params
        });
        let response = self.post(&body.to_string()).await;
        // The transport answers over SSE, so the JSON-RPC message is the
        // payload of the one `data:` event that carries a result.
        response
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
            .find(|value| value.get("id").is_some())
            .unwrap_or_else(|| panic!("no JSON-RPC answer in: {response}"))
    }
}

fn names_in(result: &serde_json::Value) -> Vec<&str> {
    result["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect()
}

/// The bug behind issue #62: a pipe used to rebuild the result around the
/// tools it found, so everything else the upstream had written — the
/// SEP-2549 caching fields a strict client validates, the `_meta` the spec
/// reserves for extensions — arrived as `undefined` and the client refused
/// the answer.
#[tokio::test]
async fn a_pipe_hands_back_the_upstreams_own_tools_list_fields() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut fx = RawSession::open(addr, &endpoint_path("fx")).await;

    let result = fx.request("tools/list", serde_json::json!({})).await;

    assert_eq!(names_in(&result), ["echo", "reverse"]);
    assert_eq!(result["ttlMs"], 4242, "{result}");
    assert_eq!(result["cacheScope"], "public", "{result}");
    assert_eq!(
        result["_meta"]["io.mcpgw.test/list"], "verbatim",
        "{result}"
    );
    manager.shutdown().await;
}

/// The same for a tool call: the answer a pipe returns is the upstream's,
/// not one rebuilt out of the parts a pipe happens to care about.
#[tokio::test]
async fn a_pipe_hands_back_the_upstreams_own_call_result_fields() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut fx = RawSession::open(addr, &endpoint_path("fx")).await;

    let result = fx
        .request(
            "tools/call",
            serde_json::json!({ "name": "reverse", "arguments": { "message": "mcpgw" } }),
        )
        .await;

    assert_eq!(result["content"][0]["text"], "wgpcm", "{result}");
    assert_eq!(
        result["_meta"]["io.mcpgw.test/call"], "verbatim",
        "{result}"
    );
    manager.shutdown().await;
}

/// Issue #137: Cursor and Codex both ignore `nextCursor` on `tools/list`, so
/// a client of either sees page one and never learns the rest exists. The
/// pipe walks the pages itself and answers with all of them.
///
/// The merged answer is still page one's — its `ttlMs`, `cacheScope` and
/// `_meta`, which is what issue #62 was about — with only the tools grown.
/// The fixture's second page claims a different policy precisely so a pipe
/// that let the last page win would fail here.
#[tokio::test]
async fn a_pipe_merges_every_page_of_tools_list_into_one_answer() {
    let (addr, manager) = serve_both(&[("fx", "paged")], None).await;
    let mut fx = RawSession::open(addr, &endpoint_path("fx")).await;

    let result = fx.request("tools/list", serde_json::json!({})).await;

    assert_eq!(names_in(&result), ["echo", "reverse"], "{result}");
    // Nothing left to page for, so nothing offering to.
    assert!(result.get("nextCursor").is_none(), "{result}");
    assert_eq!(result["ttlMs"], 4242, "{result}");
    assert_eq!(result["cacheScope"], "public", "{result}");
    assert_eq!(result["_meta"]["io.mcpgw.test/page"], "one", "{result}");
    manager.shutdown().await;
}

/// The same for a client on the newer revision, which is where the two
/// changes could collide: the merge picks the result the bridge then fills
/// in, and page one already had a caching policy of its own to keep.
#[tokio::test]
async fn a_newer_client_gets_the_merged_list_too() {
    let (addr, manager) = serve_both(&[("fx", "paged")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));

    let result = fx.request("tools/list", serde_json::json!({})).await;

    assert_eq!(names_in(&result), ["echo", "reverse"], "{result}");
    assert!(result.get("nextCursor").is_none(), "{result}");
    assert_eq!(result["resultType"], "complete", "{result}");
    assert_eq!(result["ttlMs"], 4242, "{result}");
    assert_eq!(result["cacheScope"], "public", "{result}");
    manager.shutdown().await;
}

/// A client that does paginate asks for page two of a list that is now one
/// page. That request is not an error — the client obeyed the protocol — so
/// it gets the empty list that ends its loop.
#[tokio::test]
async fn a_client_supplied_cursor_gets_an_empty_page_rather_than_an_error() {
    let (addr, manager) = serve_both(&[("fx", "paged")], None).await;
    let mut fx = RawSession::open(addr, &endpoint_path("fx")).await;

    let message = fx
        .attempt(
            "tools/list",
            serde_json::json!({ "cursor": "fixture-cursor-page-2" }),
        )
        .await;

    assert!(message.get("error").is_none(), "{message}");
    let result = &message["result"];
    assert_eq!(names_in(result), Vec::<&str>::new(), "{result}");
    assert!(result.get("nextCursor").is_none(), "{result}");
    manager.shutdown().await;
}

/// A server whose `tools/list` never ends must not be able to hold a client's
/// request open for as long as it keeps issuing cursors. Both shapes stop:
/// the cursor handed out twice, and the cursor that is always new.
#[tokio::test]
async fn a_list_that_never_ends_stops_at_the_guard() {
    let (addr, manager) = serve_both(
        &[("loop", "paged-loop"), ("endless", "paged-endless")],
        None,
    )
    .await;

    // The repeat is caught on the page that returns it, so the walk stops
    // having fetched two pages rather than the ceiling's worth.
    let mut looping = RawSession::open(addr, &endpoint_path("loop")).await;
    let result = looping.request("tools/list", serde_json::json!({})).await;
    assert_eq!(names_in(&result), ["loop", "loop"], "{result}");
    assert!(result.get("nextCursor").is_none(), "{result}");

    // Every cursor new, so only the page ceiling ends this one.
    let mut endless = RawSession::open(addr, &endpoint_path("endless")).await;
    let result = endless.request("tools/list", serde_json::json!({})).await;
    let names = names_in(&result);
    assert_eq!(names.len(), MAX_TOOL_PAGES, "{result}");
    assert_eq!(names.first(), Some(&"page-0"), "{result}");
    assert!(result.get("nextCursor").is_none(), "{result}");
    manager.shutdown().await;
}

/// Resources are not merged. The issue is that clients lose *tools*, and a
/// resource list can run to thousands of entries — so the cursor still
/// crosses the pipe in both directions there.
#[tokio::test]
async fn resources_still_paginate_through_the_pipe() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut fx = RawSession::open(addr, &endpoint_path("fx")).await;

    // The fixture echoes the cursor it was given, which is how a client's own
    // pagination being carried through rather than answered out of a list the
    // pipe collected for itself is visible from here.
    let result = fx
        .request(
            "resources/list",
            serde_json::json!({ "cursor": "some-server-cursor" }),
        )
        .await;
    assert_eq!(
        result["_meta"]["io.mcpgw.test/cursor"], "some-server-cursor",
        "{result}"
    );
    manager.shutdown().await;
}

/// How far transparency reaches, pinned as a fact rather than assumed.
///
/// Both hops go through rmcp's models — the upstream's answer is decoded
/// into `ServerResult` (an untagged enum whose `ListToolsResult` variant
/// matches first and keeps only the fields it declares) before the pipe ever
/// sees it, and rmcp offers no raw relay to sidestep that. So everything the
/// MCP schema defines survives, `_meta` included, which is the extension
/// point the spec actually reserves; a field of a server's own invention
/// does not. If rmcp ever grows a verbatim path this test is the one that
/// notices, by failing.
#[tokio::test]
async fn fields_outside_the_mcp_schema_do_not_survive_the_hop() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut fx = RawSession::open(addr, &endpoint_path("fx")).await;

    let result = fx.request("tools/list", serde_json::json!({})).await;

    assert!(result.get("x-fixture-alien").is_none(), "{result}");
    assert!(
        result["tools"][0].get("x-fixture-tool").is_none(),
        "{result}"
    );
    manager.shutdown().await;
}

/// Issue #63: a per-server pipe is a view onto one server, and telling the
/// user it is "mcpgw" N times over N endpoints tells them nothing. The
/// identity comes from the same post-first-contact snapshot the capabilities
/// do, so `initialize` still never starts an upstream.
#[tokio::test]
async fn a_pipe_names_its_upstream_once_it_has_met_it() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;

    let early = client_at(addr, &endpoint_path("fx")).await;
    let identity = early.peer_info().unwrap().server_info.clone().unwrap();
    assert_eq!(identity.name, "mcpgw", "pre-contact: {identity:?}");

    early.list_all_tools().await.unwrap();
    let later = client_at(addr, &endpoint_path("fx")).await;
    let identity = later.peer_info().unwrap().server_info.clone().unwrap();
    assert_eq!(identity.name, "mcpgw-test-server", "{identity:?}");
    assert_eq!(identity.version, "9.9.9", "{identity:?}");

    // The base endpoint is nobody's proxy: it stays mcpgw.
    let base = client_at(addr, "/mcp").await;
    let identity = base.peer_info().unwrap().server_info.clone().unwrap();
    assert_eq!(identity.name, "mcpgw", "{identity:?}");

    for client in [early, later, base] {
        client.cancel().await.unwrap();
    }
    manager.shutdown().await;
}

/// A downstream client on the 2026-07-28 lifecycle, which has no handshake
/// at all: there is no `initialize`, no session, and every request carries
/// the revision it speaks in its own `_meta` (SEP-2575) alongside the
/// standard MCP headers (SEP-2243). This is how a current client reaches the
/// gateway, and the reason a pipe cannot assume its two sides agree on a
/// revision — the upstream connection behind it still handshakes at
/// 2025-11-25, which is the newest revision that has a handshake.
struct InlineSession {
    addr: std::net::SocketAddr,
    path: String,
    id: u32,
    /// What `_meta` says the client is, or `None` for a client that declines
    /// to say — naming yourself is a SHOULD, so that request is legal.
    client: Option<(String, String)>,
    /// Sent on every request, so a test can add what a real client would put
    /// there — a per-request `Mcp-Param-*`, a credential of its own — without
    /// a second copy of the raw-request machinery.
    extra: Vec<(String, String)>,
}

impl InlineSession {
    const VERSION: &'static str = "2026-07-28";

    fn new(addr: std::net::SocketAddr, path: &str) -> Self {
        Self {
            addr,
            path: path.to_owned(),
            id: 0,
            client: Some(("inline".to_owned(), "1".to_owned())),
            extra: Vec::new(),
        }
    }

    fn naming(mut self, name: &str, version: &str) -> Self {
        self.client = Some((name.to_owned(), version.to_owned()));
        self
    }

    fn anonymous(mut self) -> Self {
        self.client = None;
        self
    }

    fn with_headers(mut self, headers: &[(&str, &str)]) -> Self {
        self.extra = headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        self
    }

    /// Sends one request and returns its `result` object verbatim.
    async fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let message = self.attempt(method, params).await;
        assert!(message.get("error").is_none(), "{method} failed: {message}");
        message["result"].clone()
    }

    /// The same, handing back the whole JSON-RPC message so a test can assert
    /// on a failure the gateway is right to produce.
    async fn attempt(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.id += 1;
        let mut params = params;
        params["_meta"] = serde_json::json!({
            "io.modelcontextprotocol/protocolVersion": Self::VERSION,
            "io.modelcontextprotocol/clientCapabilities": {}
        });
        if let Some((name, version)) = &self.client {
            params["_meta"]["io.modelcontextprotocol/clientInfo"] =
                serde_json::json!({ "name": name, "version": version });
        }
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": self.id, "method": method, "params": params
        })
        .to_string();
        // SEP-2243: a request that names a subject repeats it in a header, so
        // an intermediary can route on it without reading the body.
        let subject = match method {
            "tools/call" | "prompts/get" => params["name"].as_str(),
            "resources/read" => params["uri"].as_str(),
            _ => None,
        };
        let mut headers = vec![
            ("MCP-Protocol-Version", Self::VERSION),
            ("Mcp-Method", method),
        ];
        if let Some(subject) = subject {
            headers.push(("Mcp-Name", subject));
        }
        for (name, value) in &self.extra {
            headers.push((name.as_str(), value.as_str()));
        }
        let response = raw_post_body(self.addr, &self.path, None, &headers, &body).await;
        response
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
            .find(|value| value.get("id").is_some())
            .or_else(|| {
                let body = response.split("\r\n\r\n").nth(1)?;
                serde_json::from_str::<serde_json::Value>(body).ok()
            })
            .unwrap_or_else(|| panic!("no JSON-RPC answer in: {response}"))
    }
}

/// The 2026-07-28 revision requires `resultType` on every result and
/// `ttlMs`/`cacheScope` on the cacheable ones. An upstream written before
/// that revision sends none of them, and relaying its answer untouched to a
/// client that negotiated the newer revision produces a reply that client
/// refuses to read — "Connected, tools fetch failed", with the upstream
/// healthy and the request logged as a success.
#[tokio::test]
async fn a_newer_client_gets_the_fields_its_revision_requires() {
    let (addr, manager) = serve_both(&[("fx", "legacy")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));

    let result = fx.request("tools/list", serde_json::json!({})).await;
    assert_eq!(names_in(&result), ["echo", "reverse"], "{result}");
    assert_eq!(result["resultType"], "complete", "{result}");
    // The upstream never said how fresh its answer is, and a pipe does not
    // invent a freshness window on its behalf: 0 is "already stale".
    assert_eq!(result["ttlMs"], 0, "{result}");
    // The answer was fetched with the operator's credentials, so no shared
    // intermediary may serve it to anyone else.
    assert_eq!(result["cacheScope"], "private", "{result}");

    // resultType is required on every result, cacheable or not.
    let call = fx
        .request(
            "tools/call",
            serde_json::json!({ "name": "echo", "arguments": { "message": "hi" } }),
        )
        .await;
    assert_eq!(call["resultType"], "complete", "{call}");
    manager.shutdown().await;
}

/// The bridge fills gaps; it does not overwrite. An upstream that does speak
/// the newer revision has already said what it means, and its own caching
/// policy is what the client must see.
#[tokio::test]
async fn an_upstream_that_speaks_the_newer_revision_keeps_its_own_answer() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));

    let result = fx.request("tools/list", serde_json::json!({})).await;

    assert_eq!(result["ttlMs"], 4242, "{result}");
    assert_eq!(result["cacheScope"], "public", "{result}");
    assert_eq!(result["resultType"], "complete", "{result}");
    manager.shutdown().await;
}

/// The other half of the rule: what a reply must be consistent with is the
/// revision the *downstream* negotiated, not the newest one in existence. A
/// client on the handshake lifecycle negotiates 2025-11-25, where none of
/// these fields exist, so nothing is added for it — and `resultType`, which
/// strict older clients reject, is not sent either.
#[tokio::test]
async fn an_older_client_is_not_handed_fields_from_a_revision_it_did_not_ask_for() {
    let (addr, manager) = serve_both(&[("fx", "legacy")], None).await;
    let mut fx = RawSession::open(addr, &endpoint_path("fx")).await;

    let result = fx.request("tools/list", serde_json::json!({})).await;

    assert_eq!(names_in(&result), ["echo", "reverse"], "{result}");
    assert!(result.get("resultType").is_none(), "{result}");
    assert!(result.get("ttlMs").is_none(), "{result}");
    assert!(result.get("cacheScope").is_none(), "{result}");
    manager.shutdown().await;
}

/// Every forwarded family, not just tools: the revision's requirement is on
/// results, and a pipe answers for all of them.
#[tokio::test]
async fn the_other_forwarded_families_are_bridged_as_well() {
    let (addr, manager) = serve_both(&[("fx", "legacy")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));

    // Cacheable: prompts/list, resources/list and resources/read.
    for method in ["prompts/list", "resources/list", "resources/templates/list"] {
        let result = fx.request(method, serde_json::json!({})).await;
        assert_eq!(result["resultType"], "complete", "{method}: {result}");
        assert_eq!(result["ttlMs"], 0, "{method}: {result}");
        assert_eq!(result["cacheScope"], "private", "{method}: {result}");
    }
    let read = fx
        .request(
            "resources/read",
            serde_json::json!({ "uri": "mem:///greeting.txt" }),
        )
        .await;
    assert_eq!(read["resultType"], "complete", "{read}");
    assert_eq!(read["ttlMs"], 0, "{read}");

    // Not cacheable, but still a result: prompts/get and completion.
    let prompt = fx
        .request(
            "prompts/get",
            serde_json::json!({ "name": "summarize", "arguments": { "topic": "gateways" } }),
        )
        .await;
    assert_eq!(prompt["resultType"], "complete", "{prompt}");
    let completion = fx
        .request(
            "completion/complete",
            serde_json::json!({
                "ref": { "type": "ref/prompt", "name": "summarize" },
                "argument": { "name": "topic", "value": "gat" }
            }),
        )
        .await;
    assert_eq!(completion["resultType"], "complete", "{completion}");
    manager.shutdown().await;
}

/// One in-process http MCP server: a gateway fronting a `healthy` fixture,
/// which from the outside is what any remote upstream looks like.
struct Remote {
    addr: std::net::SocketAddr,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
    inner: Arc<UpstreamManager>,
}

impl Remote {
    /// Port 0 takes any free port; a fixed one is how a restart lands where
    /// an upstream is already pointing.
    async fn start(port: u16) -> Self {
        let deadline = Instant::now() + Duration::from_secs(30);
        // A predecessor releases its listener when its task actually winds
        // down, which `abort` only schedules — so retrying the bind is the
        // synchronisation here, rather than a sleep guessed at how long that
        // takes on a loaded machine.
        let listener = loop {
            match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
                Ok(listener) => break listener,
                Err(err) => {
                    assert!(Instant::now() < deadline, "port {port} never freed: {err}");
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            }
        };
        let addr = listener.local_addr().unwrap();
        // `legacy` rather than `healthy`: the healthy fixture marks its
        // tools/list cacheable for four seconds and the pipe forwards that
        // faithfully, so a client would answer the second list out of its own
        // cache — which says nothing about the connection these tests are
        // entirely about.
        let inner = manager(&[("fx", "legacy")]);
        let gateway = Gateway::new(Arc::clone(&inner), "fx".to_owned());
        let task = tokio::spawn(serve_http(
            "fx".to_owned(),
            gateway,
            listener,
            std::future::pending(),
        ));
        Self { addr, task, inner }
    }

    /// Ends the server the way an outage does: the port stops answering, with
    /// no goodbye to the client holding a session on it.
    async fn stop(self) {
        self.task.abort();
        self.inner.shutdown().await;
    }
}

/// A manager with one http upstream named `remote`, pointed at `addr`.
fn remote_manager(addr: std::net::SocketAddr) -> Arc<UpstreamManager> {
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            url: format!("http://{addr}{}", endpoint_path("fx")),
            headers_command: Vec::new(),
            headers: BTreeMap::new(),
            auth: None,
        },
    };
    Arc::new(
        UpstreamManager::new([("remote".to_owned(), server)].into_iter().collect())
            .with_connect_timeout(Duration::from_secs(30))
            .with_backoff_base(Duration::from_millis(20)),
    )
}

async fn list_through(
    manager: &UpstreamManager,
) -> Result<Vec<rmcp_client_http::model::Tool>, CallError> {
    manager
        .call(
            "remote",
            |service| async move { service.list_all_tools().await },
        )
        .await
}

/// The connection an http upstream holds outlives the server behind it: rmcp
/// hands the failed POST to the request that made it and keeps its worker
/// running, so nothing in the transport's own liveness ever says the remote
/// is gone. The slot used to stay `Ready` for the life of the process,
/// failing every request on a connection that could not come back.
#[tokio::test]
async fn a_vanished_http_upstream_stops_reporting_ready() {
    let remote = Remote::start(0).await;
    let outer = remote_manager(remote.addr);
    assert!(!list_through(&outer).await.unwrap().is_empty());
    assert_eq!(outer.status("remote").await, Some(UpstreamStatus::Ready));

    remote.stop().await;

    let err = list_through(&outer).await.unwrap_err();
    let status = outer.status("remote").await;
    let Some(UpstreamStatus::Failed(reason)) = status else {
        panic!("expected Failed after {err}, got {status:?}");
    };
    assert!(
        reason.contains("transport"),
        "the reason should name the transport failure: {reason}"
    );
    outer.shutdown().await;
}

/// And the demotion is what buys the recovery: the next demand finds a
/// `Failed` slot, runs the ladder against the restarted remote and gets a
/// live connection instead of a second failure.
#[tokio::test]
async fn a_restarted_http_upstream_is_reconnected_on_the_next_demand() {
    let remote = Remote::start(0).await;
    let addr = remote.addr;
    let outer = remote_manager(addr);
    list_through(&outer).await.unwrap();
    remote.stop().await;
    assert!(list_through(&outer).await.is_err());
    // The demotion is what makes the next demand a demand at all: a slot
    // still reading `Ready` would hand the same dead connection back.
    assert!(matches!(
        outer.status("remote").await,
        Some(UpstreamStatus::Failed(_))
    ));

    let remote = Remote::start(addr.port()).await;
    let tools = list_through(&outer).await.unwrap();
    assert_eq!(tool_names(&tools), ["echo", "reverse"]);
    assert_eq!(outer.status("remote").await, Some(UpstreamStatus::Ready));
    outer.shutdown().await;
    remote.stop().await;
}

/// The other half of the same rule: a server that answers "no" is a server
/// that is answering. Tearing the connection down over an unknown tool name
/// would mean a client typo costing every other session a reconnect.
#[tokio::test]
async fn a_json_rpc_error_from_a_live_http_upstream_keeps_the_slot_ready() {
    let remote = Remote::start(0).await;
    let outer = remote_manager(remote.addr);
    list_through(&outer).await.unwrap();

    // A resource that is not there: the fixture answers it with a JSON-RPC
    // error, which is a server saying no rather than a server going away.
    let err = outer
        .call("remote", |service| async move {
            service.read_resource(read("mem:///nope.txt")).await
        })
        .await
        .unwrap_err();
    assert!(
        matches!(err, CallError::Service(_)),
        "the server answered, so this is not an upstream failure: {err}"
    );
    assert_eq!(outer.status("remote").await, Some(UpstreamStatus::Ready));

    // Still usable, which is the whole point.
    list_through(&outer).await.unwrap();
    outer.shutdown().await;
    remote.stop().await;
}

/// A remote that restarts on its own port between two requests is recovered
/// by the transport itself: the stale `Mcp-Session-Id` earns a 404 and rmcp
/// repeats the handshake underneath the call. That is the fast path in front
/// of the connect ladder, and it must keep working — see the explicit
/// `reinit_on_expired_session` in `http_config`.
#[tokio::test]
async fn a_session_expired_by_a_restart_is_reinitialized_under_the_call() {
    let remote = Remote::start(0).await;
    let addr = remote.addr;
    let outer = remote_manager(addr);
    list_through(&outer).await.unwrap();

    // No request goes through the outage, so the slot never learns about it.
    remote.stop().await;
    let remote = Remote::start(addr.port()).await;
    assert_eq!(outer.status("remote").await, Some(UpstreamStatus::Ready));

    let tools = list_through(&outer).await.unwrap();
    assert_eq!(tool_names(&tools), ["echo", "reverse"]);
    outer.shutdown().await;
    remote.stop().await;
}

/// A bare http server that answers every request `401` — an OAuth-protected
/// remote, from the point of view of a gateway with no token for it.
fn unauthorized_server() -> std::net::SocketAddr {
    let app = axum::Router::new().fallback(|| async {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            [(
                axum::http::header::WWW_AUTHENTICATE,
                "Bearer resource_metadata=\"https://auth.example.com/.well-known/\
                 oauth-protected-resource\"",
            )],
        )
    });
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// What a client at `/s/<name>` is told when the server behind it wants a
/// login. Not the upstream's `WWW-Authenticate` — relaying that would have
/// the client authenticate to the upstream and hand the gateway its token —
/// but the one thing that can be done about it on this machine.
#[tokio::test]
async fn an_oauth_upstream_names_the_login_rather_than_relaying_the_challenge() {
    let manager = remote_manager(unauthorized_server());
    let client = connect(
        "remote",
        Gateway::new(Arc::clone(&manager), "remote".to_owned()),
    )
    .await;

    let text = client.list_all_tools().await.unwrap_err().to_string();
    assert!(
        text.contains(
            "upstream \"remote\" needs OAuth; run mcpgw auth login remote on this machine"
        ),
        "should name the login: {text}"
    );
    assert!(
        !text.to_ascii_lowercase().contains("www-authenticate")
            && !text.contains("resource_metadata"),
        "the upstream's challenge must not be relayed: {text}"
    );
    assert!(matches!(
        manager.status("remote").await,
        Some(UpstreamStatus::AuthRequired { .. })
    ));

    client.cancel().await.unwrap();
    manager.shutdown().await;
}

/// `server/discover` is a MUST on a 2026-07-28 server, and a gateway is one:
/// a client that has never spoken to this endpoint asks it what it can do,
/// and the answer has to describe the server behind the pipe.
#[tokio::test]
async fn a_pipe_answers_discover_for_the_server_behind_it() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));

    // Capabilities and identity come from the last successful connect, so
    // this is what makes there be one — the same rule `initialize` follows.
    fx.request("tools/list", serde_json::json!({})).await;
    let result = fx.request("server/discover", serde_json::json!({})).await;

    assert_eq!(result["resultType"], "complete", "{result}");
    let versions: Vec<&str> = result["supportedVersions"]
        .as_array()
        .unwrap_or_else(|| panic!("no supportedVersions: {result}"))
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect();
    assert!(versions.contains(&"2026-07-28"), "{result}");
    // The pipe forwards for clients on the older revision too, and says so.
    assert!(versions.contains(&"2025-11-25"), "{result}");
    assert!(result["capabilities"]["tools"].is_object(), "{result}");
    assert!(result["capabilities"]["prompts"].is_object(), "{result}");
    // The server behind the endpoint, not "mcpgw": one gateway serving N
    // servers under N endpoints all called mcpgw tells a user nothing.
    let identity = &result["_meta"]["io.modelcontextprotocol/serverInfo"];
    assert_eq!(identity["name"], "mcpgw-test-server", "{result}");
    // Discovery is a cacheable result like any other, and the gateway's own
    // answer changes with the upstream: nothing here may be cached.
    assert_eq!(result["ttlMs"], 0, "{result}");
    assert_eq!(result["cacheScope"], "private", "{result}");
    manager.shutdown().await;
}

/// The base endpoint answers it as itself. It fronts no server, so the
/// identity is the gateway's own and the tool list is empty — which is a
/// different thing from an endpoint that is not there, and the difference is
/// what a client dialing the base is entitled to learn.
#[tokio::test]
async fn the_base_endpoint_discovers_as_mcpgw_and_serves_no_tools() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut base = InlineSession::new(addr, "/mcp");

    let discovered = base.request("server/discover", serde_json::json!({})).await;
    assert_eq!(
        discovered["_meta"]["io.modelcontextprotocol/serverInfo"]["name"], "mcpgw",
        "{discovered}"
    );
    assert!(
        discovered["capabilities"]["tools"].is_object(),
        "{discovered}"
    );
    // Nothing is forwarded from here, so nothing else may be advertised.
    assert!(
        discovered["capabilities"].get("resources").is_none(),
        "{discovered}"
    );
    assert!(
        discovered["capabilities"].get("prompts").is_none(),
        "{discovered}"
    );

    // An empty list, still shaped the way the revision requires — an empty
    // answer a strict client rejects is no better than no answer.
    let listed = base.request("tools/list", serde_json::json!({})).await;
    assert_eq!(names_in(&listed), Vec::<&str>::new(), "{listed}");
    assert_eq!(listed["resultType"], "complete", "{listed}");
    assert_eq!(listed["ttlMs"], 0, "{listed}");
    assert_eq!(listed["cacheScope"], "private", "{listed}");

    manager.shutdown().await;
}

/// A call that arrives here comes from a client config pointing one path too
/// high — the shape 0.3 and 0.4 wrote. The answer says where to point it.
#[tokio::test]
async fn the_base_endpoint_refuses_a_tool_call_and_says_where_to_go() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut base = InlineSession::new(addr, "/mcp");

    let message = base
        .attempt(
            "tools/call",
            serde_json::json!({ "name": "echo", "arguments": { "message": "hi" } }),
        )
        .await;

    // -32601: JSON-RPC's own method-not-found, which means the same thing to
    // a client of either revision.
    assert_eq!(message["error"]["code"], -32601, "{message}");
    assert_eq!(message["error"]["message"], NO_TOOLS_HERE, "{message}");
    manager.shutdown().await;
}

/// `doctor` and `daemon status` ask the port whether a gateway is there with
/// a plain GET and take any HTTP answer for a yes. The base endpoint is what
/// they land on, and it has to keep answering one.
#[tokio::test]
async fn the_base_endpoint_answers_the_daemon_probe() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;

    let reach =
        mcpgw_core::daemon::probe_gateway(&format!("http://{addr}/mcp"), Duration::from_secs(5))
            .await;

    assert!(reach.is_up(), "{reach:?}");
    manager.shutdown().await;
}

/// Capabilities are forwarded allow-new-by-default: everything the upstream
/// declared reaches the client except the short list the pipe would be lying
/// about. The old rule was an allow-list of the families this gateway had
/// heard of, which silently dropped every capability the spec added after it
/// was written.
#[tokio::test]
async fn every_upstream_capability_is_forwarded_except_what_the_pipe_cannot_honour() {
    let (addr, manager) = serve_both(&[("fx", "modern")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));
    fx.request("tools/list", serde_json::json!({})).await;

    let capabilities =
        fx.request("server/discover", serde_json::json!({})).await["capabilities"].clone();

    // Forwarded, including one this gateway has never heard of.
    assert!(capabilities["tools"].is_object(), "{capabilities}");
    assert!(capabilities["resources"].is_object(), "{capabilities}");
    assert!(capabilities["completions"].is_object(), "{capabilities}");
    assert_eq!(
        capabilities["extensions"]["com.example/thing"]["deep"], true,
        "{capabilities}"
    );
    // `listChanged` is forwarded now that the pipe carries the notification
    // (issue #140), on both the families this fixture declares it for.
    assert_eq!(capabilities["tools"]["listChanged"], true, "{capabilities}");
    assert_eq!(
        capabilities["resources"]["listChanged"], true,
        "{capabilities}"
    );
    // Dropped: the promises that still stop at the gateway. Per-resource
    // updates need a subscription the pipe does not hold, and a client that
    // believed `subscribe` would wait forever.
    assert!(
        capabilities["resources"].get("subscribe").is_none(),
        "{capabilities}"
    );
    assert!(capabilities.get("logging").is_none(), "{capabilities}");
    assert!(
        capabilities["extensions"]
            .get("io.modelcontextprotocol/tasks")
            .is_none(),
        "{capabilities}"
    );
    manager.shutdown().await;
}

/// The other half of the version matrix: an upstream that speaks only
/// 2026-07-28 has no `initialize` to answer, so a gateway that knows one
/// handshake cannot reach it at all. Every forwarded family has to work
/// through it, for a client on the current revision.
#[tokio::test]
async fn a_modern_client_reaches_a_modern_upstream() {
    let (addr, manager) = serve_both(&[("fx", "modern")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));

    let tools = fx.request("tools/list", serde_json::json!({})).await;
    assert_eq!(names_in(&tools), ["echo", "ask"], "{tools}");
    // The upstream's own caching policy, not the pipe's fallback.
    assert_eq!(tools["ttlMs"], 4242, "{tools}");
    assert_eq!(tools["cacheScope"], "public", "{tools}");

    let call = fx
        .request(
            "tools/call",
            serde_json::json!({ "name": "echo", "arguments": { "message": "hi" } }),
        )
        .await;
    assert_eq!(call["content"][0]["text"], "hi", "{call}");
    assert_eq!(call["resultType"], "complete", "{call}");

    let resources = fx.request("resources/list", serde_json::json!({})).await;
    assert_eq!(resources["resources"][0]["name"], "greeting", "{resources}");
    let prompts = fx.request("prompts/list", serde_json::json!({})).await;
    assert_eq!(prompts["prompts"][0]["name"], "summarize", "{prompts}");
    manager.shutdown().await;
}

/// And the reverse: a client still on the handshake lifecycle, in front of an
/// upstream that has none. The pipe holds one conversation on each side and
/// neither client nor server has to know about the other's revision.
#[tokio::test]
async fn an_older_client_reaches_a_modern_upstream() {
    let (addr, manager) = serve_both(&[("fx", "modern")], None).await;
    let mut fx = RawSession::open(addr, &endpoint_path("fx")).await;

    let tools = fx.request("tools/list", serde_json::json!({})).await;
    assert_eq!(names_in(&tools), ["echo", "ask"], "{tools}");
    // 2025-11-25 has no `resultType`, so the SDK strips what the upstream
    // sent rather than handing a client a field its revision rejects.
    assert!(tools.get("resultType").is_none(), "{tools}");

    let call = fx
        .request(
            "tools/call",
            serde_json::json!({ "name": "echo", "arguments": { "message": "hi" } }),
        )
        .await;
    assert_eq!(call["content"][0]["text"], "hi", "{call}");

    let prompts = fx.request("prompts/list", serde_json::json!({})).await;
    assert_eq!(prompts["prompts"][0]["name"], "summarize", "{prompts}");

    // The one cell of the matrix that cannot work, and must fail loudly
    // rather than quietly: MRTR arrived with 2026-07-28, so an
    // `input_required` answer has no shape this client could read. The pipe
    // does not invent one, and the client is told the request failed instead
    // of being handed a result it would misread.
    let refused = fx
        .attempt("tools/call", serde_json::json!({ "name": "ask" }))
        .await;
    assert!(refused.get("error").is_some(), "{refused}");
    manager.shutdown().await;
}

/// MRTR (SEP-2322): a tool that needs input answers `input_required`, and the
/// client retries the same call with what it collected. The pipe is not
/// allowed to satisfy that round itself — it has no user to ask — so both the
/// request for input and the state that correlates the retry have to cross it
/// untouched, in both directions.
#[tokio::test]
async fn an_input_required_round_trip_crosses_the_pipe_untouched() {
    let (addr, manager) = serve_both(&[("fx", "modern")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));

    let first = fx
        .request("tools/call", serde_json::json!({ "name": "ask" }))
        .await;
    assert_eq!(first["resultType"], "input_required", "{first}");
    assert_eq!(
        first["inputRequests"]["city"]["method"], "elicitation/create",
        "{first}"
    );
    assert_eq!(
        first["inputRequests"]["city"]["params"]["message"], "which city?",
        "{first}"
    );
    let state = first["requestState"]
        .as_str()
        .unwrap_or_else(|| panic!("no requestState to echo: {first}"))
        .to_owned();

    let second = fx
        .request(
            "tools/call",
            serde_json::json!({
                "name": "ask",
                "requestState": state,
                "inputResponses": {
                    "city": { "action": "accept", "content": { "city": "berlin" } }
                }
            }),
        )
        .await;
    assert_eq!(second["resultType"], "complete", "{second}");
    // The upstream renders both halves back: the client's answer, and its own
    // state as it came home.
    assert_eq!(
        second["content"][0]["text"], "berlin (fixture-request-state-1)",
        "{second}"
    );
    manager.shutdown().await;
}

/// A 2026-07-28 request allocates no session. The revision removed them
/// (SEP-2567), and the session manager the endpoints still hold — which a
/// client on the older revision needs — must not mint one here.
#[tokio::test]
async fn a_modern_request_is_answered_without_a_session() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/list",
        "params": { "_meta": {
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": "inline", "version": "1" },
            "io.modelcontextprotocol/clientCapabilities": {}
        }}
    })
    .to_string();
    let response = raw_post_body(
        addr,
        &endpoint_path("fx"),
        None,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/list"),
        ],
        &body,
    )
    .await;

    assert!(response.contains("echo"), "{response}");
    assert!(
        !response.to_lowercase().contains("mcp-session-id"),
        "a sessionless revision was handed a session: {response}"
    );
    manager.shutdown().await;
}

/// SEP-2243 headers are the point of an intermediary: `Mcp-Method` (and
/// `Mcp-Name` where the method names a subject) let one route without reading
/// the body, and a mismatch between header and body is a lie the server has
/// to refuse. The SDK enforces it in front of this gateway; the check is here
/// because it is part of what mcpgw promises a client, not part of rmcp's
/// internals.
#[tokio::test]
async fn the_standard_headers_are_required_and_checked_against_the_body() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let meta = serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
        "io.modelcontextprotocol/clientInfo": { "name": "inline", "version": "1" },
        "io.modelcontextprotocol/clientCapabilities": {}
    });
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": "echo", "arguments": { "message": "hi" }, "_meta": meta }
    })
    .to_string();

    // The subject is named in the body but not in the headers.
    let missing = raw_post_body(
        addr,
        &endpoint_path("fx"),
        None,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
        ],
        &body,
    )
    .await;
    assert!(missing.contains("Mcp-Name"), "{missing}");

    // The headers name a different tool than the body does.
    let mismatched = raw_post_body(
        addr,
        &endpoint_path("fx"),
        None,
        &[
            ("MCP-Protocol-Version", "2026-07-28"),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "reverse"),
        ],
        &body,
    )
    .await;
    assert!(mismatched.contains("does not match"), "{mismatched}");
    manager.shutdown().await;
}

/// Issue #138: 2026-07-28 has no sessions, so `session` collapses to one
/// value and the log can no longer answer "which harness made this call".
/// The answer the revision does offer is the client identity every request
/// carries in its own `_meta` (SEP-2575), and the record keeps it.
#[tokio::test]
async fn a_stateless_request_is_attributed_to_the_client_that_named_itself() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let (addr, manager) = serve_both(&[("fx", "healthy")], Some(&writer)).await;

    let mut fx = InlineSession::new(addr, &endpoint_path("fx")).naming("claude-code", "2.1.3");
    fx.request(
        "tools/call",
        serde_json::json!({ "name": "echo", "arguments": { "message": "hi" } }),
    )
    .await;
    manager.shutdown().await;

    let records = captured(writer.dir());
    assert_eq!(records.len(), 1, "{records:#?}");
    assert_eq!(
        records[0].client.as_deref(),
        Some("claude-code/2.1.3"),
        "{records:#?}"
    );
}

/// The other half of the same question, for a client on a revision that still
/// handshakes: `clientInfo` arrives once, at `initialize`, and every later
/// request has to be attributed from the peer rather than from its own body.
#[tokio::test]
async fn a_handshaking_client_is_attributed_from_its_initialize() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let (addr, manager) = serve_both(&[("fx", "healthy")], Some(&writer)).await;

    let mut fx =
        RawSession::open_as(addr, &endpoint_path("fx"), "2025-11-25", "cursor", "0.48").await;
    fx.request(
        "tools/call",
        serde_json::json!({ "name": "echo", "arguments": { "message": "hi" } }),
    )
    .await;
    manager.shutdown().await;

    let records = captured(writer.dir());
    assert_eq!(records.len(), 1, "{records:#?}");
    assert_eq!(
        records[0].client.as_deref(),
        Some("cursor/0.48"),
        "{records:#?}"
    );
    // The session id is still what says *which connection*, and it is still
    // not the raw one: attribution added a field, it replaced nothing.
    assert_ne!(records[0].session, writer.session(), "{records:#?}");
}

/// Naming yourself is a SHOULD. A client that does not is left unattributed
/// rather than filed under a guess, and its line is a line like any other.
#[tokio::test]
async fn a_client_that_names_nobody_gets_no_client_field() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let (addr, manager) = serve_both(&[("fx", "healthy")], Some(&writer)).await;

    let mut fx = InlineSession::new(addr, &endpoint_path("fx")).anonymous();
    fx.request("tools/list", serde_json::json!({})).await;
    manager.shutdown().await;

    let line = daily_file(writer.dir());
    assert!(!line.contains("\"client\""), "{line}");
    // …and the line is still a record, read by the same parser as any other.
    let records = captured(writer.dir());
    assert_eq!(records.len(), 1, "{records:#?}");
    assert_eq!(records[0].client, None, "{records:#?}");
    assert_eq!(records[0].kind, Kind::List, "{records:#?}");
}

/// The reason the field exists: two harnesses through one gateway, on the
/// revision that gives them no sessions to be told apart by.
#[tokio::test]
async fn two_stateless_clients_are_attributed_to_themselves() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let (addr, manager) = serve_both(&[("fx", "healthy")], Some(&writer)).await;

    let mut one = InlineSession::new(addr, &endpoint_path("fx")).naming("claude-code", "2.1.3");
    let mut two = InlineSession::new(addr, &endpoint_path("fx")).naming("cursor", "0.48");
    for message in ["from one", "from two"] {
        let session = if message == "from one" {
            &mut one
        } else {
            &mut two
        };
        session
            .request(
                "tools/call",
                serde_json::json!({ "name": "echo", "arguments": { "message": message } }),
            )
            .await;
    }
    manager.shutdown().await;

    let records = captured(writer.dir());
    let attributed: BTreeMap<&str, &str> = records
        .iter()
        .map(|r| {
            (
                r.client.as_deref().unwrap_or("<none>"),
                r.args.as_deref().unwrap_or("<none>"),
            )
        })
        .collect();
    assert_eq!(attributed.len(), 2, "{records:#?}");
    assert!(
        attributed["claude-code/2.1.3"].contains("from one"),
        "{records:#?}"
    );
    assert!(
        attributed["cursor/0.48"].contains("from two"),
        "{records:#?}"
    );
}

// ---------------------------------------------------------------------------
// Issue #140: an upstream that changes its lists can reach a connected client
// ---------------------------------------------------------------------------

/// A downstream client that records the list-changed notifications it is
/// sent. The default handler (`()`) decodes them and drops them, which is
/// indistinguishable from never having been told.
#[derive(Clone)]
struct Listening(tokio::sync::mpsc::UnboundedSender<&'static str>);

impl rmcp_client_http::ClientHandler for Listening {
    async fn on_tool_list_changed(
        &self,
        _context: rmcp_client_http::service::NotificationContext<rmcp_client_http::RoleClient>,
    ) {
        let _ = self.0.send("tools");
    }

    async fn on_resource_list_changed(
        &self,
        _context: rmcp_client_http::service::NotificationContext<rmcp_client_http::RoleClient>,
    ) {
        let _ = self.0.send("resources");
    }

    async fn on_prompt_list_changed(
        &self,
        _context: rmcp_client_http::service::NotificationContext<rmcp_client_http::RoleClient>,
    ) {
        let _ = self.0.send("prompts");
    }
}

type Listener = rmcp_client_http::service::RunningService<rmcp_client_http::RoleClient, Listening>;

/// A 2025-11-25 session that keeps what it was told, plus the queue it keeps
/// it in.
async fn listening_at(
    addr: std::net::SocketAddr,
    path: &str,
) -> (Listener, tokio::sync::mpsc::UnboundedReceiver<&'static str>) {
    let (heard, queue) = tokio::sync::mpsc::unbounded_channel();
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}{path}"));
    (Listening(heard).serve(transport).await.unwrap(), queue)
}

/// An endpoint advertises what it last heard from the server behind it, so a
/// session opened before the gateway has ever reached it is promised nothing.
/// One request through the endpoint settles that; every test below wants a
/// session that was promised something.
async fn warmed(addr: std::net::SocketAddr, name: &str) {
    let warm = client_at(addr, &endpoint_path(name)).await;
    warm.list_all_tools().await.unwrap();
    warm.cancel().await.unwrap();
}

/// The bug in issue #140: an upstream announcing a new tool had nowhere to
/// announce it to, so a client kept calling the list it read at connect time
/// until someone restarted it.
#[tokio::test]
async fn a_session_hears_when_the_upstream_changes_its_tool_list() {
    let (addr, manager) = serve_both(&[("fx", "bump")], None).await;
    warmed(addr, "fx").await;

    let (client, mut heard) = listening_at(addr, &endpoint_path("fx")).await;
    // The promise first: a client only listens for what it was offered.
    let capabilities = client.peer_info().unwrap().capabilities.clone();
    assert_eq!(
        capabilities.tools.as_ref().unwrap().list_changed,
        Some(true),
        "{capabilities:?}"
    );

    client.call_tool(call("bump", "")).await.unwrap();

    let what = tokio::time::timeout(Duration::from_secs(10), heard.recv())
        .await
        .expect("no list-changed reached the client")
        .unwrap();
    assert_eq!(what, "tools");
    // And the news is true: the list really did change behind it.
    let tools = client.list_all_tools().await.unwrap();
    assert!(tool_names(&tools).contains(&"bumped"), "{tools:?}");

    client.cancel().await.unwrap();
    manager.shutdown().await;
}

/// Only the family the upstream declared: the `bump` fixture announces tools
/// and nothing else, and a pipe that turned one promise into three would have
/// clients re-reading lists that never move.
#[tokio::test]
async fn only_the_families_the_upstream_announces_are_advertised() {
    let (addr, manager) = serve_both(&[("fx", "bump")], None).await;
    warmed(addr, "fx").await;

    let (client, _heard) = listening_at(addr, &endpoint_path("fx")).await;
    let capabilities = client.peer_info().unwrap().capabilities.clone();

    assert_eq!(
        capabilities.tools.as_ref().unwrap().list_changed,
        Some(true),
        "{capabilities:?}"
    );
    assert_eq!(
        capabilities.resources.as_ref().unwrap().list_changed,
        None,
        "{capabilities:?}"
    );
    assert_eq!(
        capabilities.prompts.as_ref().unwrap().list_changed,
        None,
        "{capabilities:?}"
    );
    client.cancel().await.unwrap();
    manager.shutdown().await;
}

/// The other half: a server that announces nothing leaves the session exactly
/// as it was. Nothing is promised, nothing is pushed, and the ordinary
/// request families still work.
#[tokio::test]
async fn a_session_on_a_silent_server_is_promised_nothing_and_still_works() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    warmed(addr, "fx").await;

    let (client, mut heard) = listening_at(addr, &endpoint_path("fx")).await;
    let capabilities = client.peer_info().unwrap().capabilities.clone();
    assert_eq!(
        capabilities.tools.as_ref().unwrap().list_changed,
        None,
        "{capabilities:?}"
    );

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tool_names(&tools), ["echo", "reverse"]);
    let result = client.call_tool(call("reverse", "mcpgw")).await.unwrap();
    assert!(format!("{result:?}").contains("wgpcm"));
    assert!(heard.try_recv().is_err(), "nothing should have been pushed");

    client.cancel().await.unwrap();
    manager.shutdown().await;
}

/// One `subscriptions/listen` stream, read as it arrives.
///
/// Every other raw helper here reads the response to the end, which a
/// subscription never reaches: staying open is the whole point of it.
struct ListenStream {
    stream: tokio::net::TcpStream,
    /// What has arrived and not yet been split into lines.
    buffer: String,
}

impl ListenStream {
    /// Opens a stream asking for `notifications`, in the 2026-07-28 shape:
    /// no session, the revision and the client in `_meta`, the method in a
    /// header (SEP-2243).
    async fn open(
        addr: std::net::SocketAddr,
        path: &str,
        notifications: serde_json::Value,
    ) -> Self {
        use tokio::io::AsyncWriteExt as _;

        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "subscriptions/listen",
            "params": {
                "notifications": notifications,
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": InlineSession::VERSION,
                    "io.modelcontextprotocol/clientInfo": { "name": "listen", "version": "1" },
                    "io.modelcontextprotocol/clientCapabilities": {}
                }
            }
        })
        .to_string();
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
             Accept: application/json, text/event-stream\r\n\
             MCP-Protocol-Version: {}\r\nMcp-Method: subscriptions/listen\r\n\
             Content-Length: {}\r\n\r\n{body}",
            InlineSession::VERSION,
            body.len()
        );
        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        Self {
            stream,
            buffer: String::new(),
        }
    }

    /// The next JSON-RPC message on the stream, waiting for it to arrive.
    async fn next(&mut self) -> serde_json::Value {
        use tokio::io::AsyncReadExt as _;

        loop {
            while let Some(end) = self.buffer.find('\n') {
                let line: String = self.buffer.drain(..=end).collect();
                if let Some(payload) = line.trim_end().strip_prefix("data: ")
                    && let Ok(value) = serde_json::from_str::<serde_json::Value>(payload)
                {
                    return value;
                }
            }
            let mut chunk = [0_u8; 4096];
            let read = tokio::time::timeout(Duration::from_secs(10), self.stream.read(&mut chunk))
                .await
                .expect("nothing arrived on the subscription")
                .unwrap();
            assert!(read > 0, "the subscription stream closed");
            self.buffer
                .push_str(&String::from_utf8_lossy(&chunk[..read]));
        }
    }
}

/// 2026-07-28 has no session to hang a notification on, so the same events
/// travel over `subscriptions/listen` instead (SEP-2568). Same upstream, same
/// pipe, different downstream shape — which is the point of driving it off
/// the revision the client is on rather than one hardcoded path.
#[tokio::test]
async fn a_2026_client_gets_list_changed_over_subscriptions_listen() {
    let (addr, manager) = serve_both(&[("fx", "bump")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));
    fx.request("tools/list", serde_json::json!({})).await;

    let mut listen = ListenStream::open(
        addr,
        &endpoint_path("fx"),
        // More than the server behind the endpoint announces, so the
        // acknowledgment has something to narrow.
        serde_json::json!({ "toolsListChanged": true, "promptsListChanged": true }),
    )
    .await;

    let ack = listen.next().await;
    assert_eq!(
        ack["method"], "notifications/subscriptions/acknowledged",
        "{ack}"
    );
    assert_eq!(
        ack["params"]["notifications"]["toolsListChanged"], true,
        "{ack}"
    );
    assert!(
        ack["params"]["notifications"]
            .get("promptsListChanged")
            .is_none(),
        "{ack}"
    );

    fx.request(
        "tools/call",
        serde_json::json!({ "name": "bump", "arguments": {} }),
    )
    .await;

    let event = listen.next().await;
    assert_eq!(
        event["method"], "notifications/tools/list_changed",
        "{event}"
    );

    let tools = fx.request("tools/list", serde_json::json!({})).await;
    assert!(names_in(&tools).contains(&"bumped"), "{tools}");
    manager.shutdown().await;
}

/// A pipe in front of a server that announces nothing has no subscription to
/// offer, and says so rather than handing back a stream that would never
/// carry anything.
#[tokio::test]
async fn subscriptions_listen_is_refused_when_the_server_announces_nothing() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut fx = InlineSession::new(addr, &endpoint_path("fx"));
    fx.request("tools/list", serde_json::json!({})).await;

    let message = fx
        .attempt(
            "subscriptions/listen",
            serde_json::json!({ "notifications": { "toolsListChanged": true } }),
        )
        .await;

    assert_eq!(message["error"]["code"], -32601, "{message}");
    manager.shutdown().await;
}

/// A fixture server carrying a `[tools]` table.
fn filtered_server(mode: &str, allow: &[&str], deny: &[&str]) -> Server {
    let owned = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
    Server {
        calls_per_minute: 0,
        tools: Some(mcpgw_core::ToolRules {
            allow: owned(allow),
            deny: owned(deny),
            ..mcpgw_core::ToolRules::default()
        }),
        ..stdio_server(mode)
    }
}

/// The tools a client sees through a pipe over a server with these lists.
async fn visible_tools(allow: &[&str], deny: &[&str]) -> Vec<String> {
    let manager = Arc::new(
        UpstreamManager::new(
            [("fx".to_owned(), filtered_server("healthy", allow, deny))]
                .into_iter()
                .collect(),
        )
        .with_connect_timeout(Duration::from_secs(30)),
    );
    let client = connect("fx", Gateway::new(Arc::clone(&manager), "fx".to_owned())).await;
    let tools = client.list_all_tools().await.unwrap();
    let names = tools.iter().map(|t| t.name.to_string()).collect();
    client.cancel().await.unwrap();
    manager.shutdown().await;
    names
}

#[tokio::test]
async fn an_allow_list_hides_everything_it_does_not_name() {
    assert_eq!(visible_tools(&["echo"], &[]).await, ["echo"]);
}

#[tokio::test]
async fn a_deny_list_hides_only_what_it_names() {
    assert_eq!(visible_tools(&[], &["echo"]).await, ["reverse"]);
}

#[tokio::test]
async fn allow_is_applied_first_and_deny_over_what_is_left() {
    assert_eq!(
        visible_tools(&["echo", "reverse"], &["reverse"]).await,
        ["echo"]
    );
}

#[tokio::test]
async fn a_trailing_star_matches_a_prefix() {
    assert_eq!(visible_tools(&["rev*"], &[]).await, ["reverse"]);
    assert_eq!(visible_tools(&[], &["ech*"]).await, ["reverse"]);
}

/// The upgrade promise: a server with no table lists exactly what it listed
/// before there was such a thing as a table.
#[tokio::test]
async fn no_table_leaves_the_list_alone() {
    let (client, manager) = gateway_client("healthy").await;
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tool_names(&tools), ["echo", "reverse"]);
    manager.shutdown().await;
}

#[tokio::test]
async fn calling_a_filtered_out_tool_is_refused_and_captured_as_denied() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let manager = Arc::new(
        UpstreamManager::new(
            [("fx".to_owned(), filtered_server("healthy", &["echo"], &[]))]
                .into_iter()
                .collect(),
        )
        .with_connect_timeout(Duration::from_secs(30)),
    );
    let gateway =
        Gateway::new(Arc::clone(&manager), "fx".to_owned()).with_capture(Arc::clone(&writer));
    let client = connect("fx", gateway).await;

    let err = client
        .call_tool(call("reverse", "mcpgw"))
        .await
        .unwrap_err();
    let text = err.to_string();
    // Compared against the one function that writes it, so the wording here
    // cannot drift away from the wording a user reads.
    assert!(text.contains(&not_allowed("reverse", "fx")), "{text}");
    assert!(text.contains("mcpgw tools fx"), "{text}");
    // The tool that survived the list still works, so the refusal is the
    // filter and not a broken endpoint.
    client.call_tool(call("echo", "hi")).await.unwrap();
    client.cancel().await.unwrap();
    manager.shutdown().await;

    let records = captured(writer.dir());
    let shape: Vec<(Kind, Option<&str>, bool)> = records
        .iter()
        .map(|r| (r.kind, r.tool.as_deref(), r.ok))
        .collect();
    assert_eq!(
        shape,
        [
            (Kind::Denied, Some("reverse"), false),
            (Kind::Call, Some("echo"), true)
        ],
        "{records:#?}"
    );
    // The line carries the reason, so `watch` shows why the call never left.
    assert!(
        records[0].error.as_deref().unwrap().contains("not allowed"),
        "{records:#?}"
    );
    // On disk under its own name, not only in this build's enum.
    let line = std::fs::read_to_string(
        std::fs::read_dir(writer.dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    assert!(line.contains(r#""kind":"denied""#), "{line}");
}

/// A denied call must not be the thing that starts a server: the refusal is
/// about an endpoint that does not offer that tool, and spawning the process
/// first would make the filter a delay rather than a boundary.
#[tokio::test]
async fn a_denied_call_never_reaches_the_upstream() {
    let manager = Arc::new(
        UpstreamManager::new(
            // `exit` dies the moment it is spawned, so anything that reached
            // it would fail with the upstream's own error instead.
            [("fx".to_owned(), filtered_server("exit", &["echo"], &[]))]
                .into_iter()
                .collect(),
        )
        .with_connect_timeout(Duration::from_secs(30)),
    );
    let client = connect("fx", Gateway::new(Arc::clone(&manager), "fx".to_owned())).await;
    let err = client.call_tool(call("reverse", "x")).await.unwrap_err();
    assert!(err.to_string().contains("is not allowed"), "{err}");
    assert_eq!(manager.status("fx").await, Some(UpstreamStatus::Idle));
    manager.shutdown().await;
}

/// A gateway over the `drift` fixture, with its own state directory: pins in
/// `<state>/pins`, traffic in `<state>/traffic`.
async fn drifting_gateway(
    state: &std::path::Path,
    drift: mcpgw_core::Drift,
) -> (Client, Arc<UpstreamManager>, Arc<CaptureWriter>) {
    let server = Server {
        tools: Some(mcpgw_core::ToolRules {
            drift,
            ..mcpgw_core::ToolRules::default()
        }),
        ..stdio_server("drift")
    };
    let manager = Arc::new(
        UpstreamManager::new([("fx".to_owned(), server)].into_iter().collect())
            .with_connect_timeout(Duration::from_secs(30)),
    );
    let writer = Arc::new(CaptureWriter::under_state_dir(state));
    let gateway = Gateway::new(Arc::clone(&manager), "fx".to_owned())
        .with_capture(Arc::clone(&writer))
        .with_pins(Arc::new(mcpgw_core::pins::PinStore::under_state_dir(state)));
    let client = connect("fx", gateway).await;
    (client, manager, writer)
}

/// The rug pull, end to end: a list that pins, a `bump` that rewrites the
/// server's tools, and a second list that has to say so — and keep serving.
#[tokio::test]
async fn a_server_that_rewrites_its_tools_is_reported_and_still_served() {
    let state = tempfile::tempdir().unwrap();
    let (client, manager, writer) = drifting_gateway(state.path(), mcpgw_core::Drift::Warn).await;
    let store = mcpgw_core::pins::PinStore::under_state_dir(state.path());

    // First sight: pinned, nothing reported.
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tool_names(&tools), ["echo", "reverse", "bump"]);
    let pinned = store.read("fx").unwrap().unwrap();
    assert_eq!(pinned.tools.len(), 3);
    assert!(pinned.drift.is_empty());
    assert!(captured(writer.dir()).iter().all(|r| r.kind != Kind::Drift));

    client.call_tool(call("bump", "")).await.unwrap();

    // Second sight: one tool rewritten, one gone, one new.
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tool_names(&tools), ["echo", "exfiltrate", "bump"]);
    // A drifted tool is still callable: warn, never block.
    let result = client.call_tool(call("echo", "still works")).await.unwrap();
    assert!(format!("{result:?}").contains("still works"));
    client.cancel().await.unwrap();
    manager.shutdown().await;

    let drift: Vec<CaptureRecord> = captured(writer.dir())
        .into_iter()
        .filter(|record| record.kind == Kind::Drift)
        .collect();
    let shape: Vec<(Option<&str>, Option<Change>)> = drift
        .iter()
        .map(|record| (record.tool.as_deref(), record.change))
        .collect();
    assert_eq!(
        shape,
        [
            (Some("echo"), Some(Change::Changed)),
            (Some("exfiltrate"), Some(Change::Added)),
            (Some("reverse"), Some(Change::Removed)),
        ],
        "{drift:#?}"
    );
    // Lengths either side of the change, and no description anywhere: the
    // rewritten text is the payload of this attack and must not be copied
    // into the log a reader (or a model) reads back.
    assert_eq!(drift[0].desc_len_before, Some(12));
    assert!(drift[0].desc_len_after.is_some_and(|len| len > 12));
    assert_eq!(drift[1].desc_len_before, None);
    assert_eq!(drift[2].desc_len_after, None);
    let text = std::fs::read_to_string(
        std::fs::read_dir(writer.dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    assert!(text.contains(r#""kind":"drift""#), "{text}");
    assert!(text.contains(r#""change":"changed""#), "{text}");
    assert!(!text.contains("id_rsa"), "{text}");

    // And the same disagreement is on the pin file for `doctor` to read.
    let after = store.read("fx").unwrap().unwrap();
    assert_eq!(after.drift.len(), 3);
    assert_eq!(
        after.tools, pinned.tools,
        "the pins are not silently updated"
    );
}

/// `drift = "off"` writes nothing and reports nothing, on a server doing
/// exactly what the other test catches.
#[tokio::test]
async fn drift_off_records_nothing_and_pins_nothing() {
    let state = tempfile::tempdir().unwrap();
    let (client, manager, writer) = drifting_gateway(state.path(), mcpgw_core::Drift::Off).await;
    client.list_all_tools().await.unwrap();
    client.call_tool(call("bump", "")).await.unwrap();
    client.list_all_tools().await.unwrap();
    client.cancel().await.unwrap();
    manager.shutdown().await;

    let store = mcpgw_core::pins::PinStore::under_state_dir(state.path());
    assert_eq!(store.read("fx").unwrap(), None);
    assert!(
        !store.dir().exists(),
        "no pin file, and no directory either"
    );
    assert!(captured(writer.dir()).iter().all(|r| r.kind != Kind::Drift));
}

/// Accepting the current definitions ends the reporting: the next list
/// agrees with the pins and writes no record.
#[tokio::test]
async fn a_re_pin_ends_the_drift() {
    let state = tempfile::tempdir().unwrap();
    let (client, manager, writer) = drifting_gateway(state.path(), mcpgw_core::Drift::Warn).await;
    let store = mcpgw_core::pins::PinStore::under_state_dir(state.path());

    client.list_all_tools().await.unwrap();
    client.call_tool(call("bump", "")).await.unwrap();
    let moved = client.list_all_tools().await.unwrap();
    assert_eq!(
        captured(writer.dir())
            .iter()
            .filter(|r| r.kind == Kind::Drift)
            .count(),
        3
    );

    // What `mcpgw tools fx pin` does, over the definitions the server is
    // serving now.
    let accepted: Vec<mcpgw_core::pins::ToolFingerprint> = moved
        .iter()
        .map(mcpgw_core::pins::ToolFingerprint::of)
        .collect();
    store.pin("fx", &accepted).unwrap();

    client.list_all_tools().await.unwrap();
    client.cancel().await.unwrap();
    manager.shutdown().await;
    assert_eq!(
        captured(writer.dir())
            .iter()
            .filter(|r| r.kind == Kind::Drift)
            .count(),
        3,
        "the accepted definitions must not report again"
    );
    assert!(store.read("fx").unwrap().unwrap().drift.is_empty());
}

// ---------------------------------------------------------------------------
// SEP-2243 `Mcp-Param-*` forwarding
// ---------------------------------------------------------------------------

use rmcp_client_http::handler::server::ServerHandler;
use rmcp_client_http::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ErrorData, ListToolsResult,
    MetaObject, PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp_client_http::service::{RequestContext, RoleServer};

/// An HTTP upstream that answers `tools/call` with the headers its own POST
/// arrived with, in the result's `_meta`. Every question these tests ask —
/// did this header cross, did that one stop — is then asked of the answer the
/// client gets back, rather than of a side channel.
#[derive(Clone)]
struct HeaderEcho;

impl ServerHandler for HeaderEcho {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::default();
        info.capabilities = ServerCapabilities::builder().enable_tools().build();
        info
    }

    fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<ListToolsResult, ErrorData>> {
        // Deliberately unannotated: a property carrying `x-mcp-header` would
        // have rmcp's own client build the header out of the arguments, and
        // what is under test is the header a gateway forwards *without*
        // recognising it.
        let schema = serde_json::json!({ "type": "object", "properties": {} })
            .as_object()
            .cloned()
            .unwrap();
        std::future::ready(Ok(ListToolsResult::with_all_items(vec![Tool::new(
            "echo",
            "reports the headers its request arrived with",
            Arc::new(schema),
        )])))
    }

    fn call_tool(
        &self,
        _request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> impl Future<Output = Result<CallToolResponse, ErrorData>> {
        let mut headers = serde_json::Map::new();
        let mut params = serde_json::Map::new();
        if let Some(parts) = context.extensions.get::<axum::http::request::Parts>() {
            for (name, value) in &parts.headers {
                let value = serde_json::Value::from(value.to_str().unwrap_or_default());
                if name.as_str().starts_with("mcp-param-") {
                    params.insert(name.to_string(), value.clone());
                }
                headers.insert(name.to_string(), value);
            }
        }
        let mut result =
            CallToolResult::success(vec![rmcp_client_http::model::ContentBlock::text("ok")]);
        result.meta = Some(MetaObject(
            serde_json::json!({ "params": params, "headers": headers })
                .as_object()
                .cloned()
                .unwrap(),
        ));
        std::future::ready(Ok(CallToolResponse::Complete(result)))
    }
}

/// A [`HeaderEcho`] on an ephemeral port, with the task serving it so the
/// test can stop it again.
async fn header_echo() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    use rmcp_client_http::transport::streamable_http_server::session::local::LocalSessionManager;
    use rmcp_client_http::transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
    };

    let service = StreamableHttpService::new(
        || Ok(HeaderEcho),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    );
    let app = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, task)
}

/// A gateway endpoint piping to a [`HeaderEcho`] upstream, plus a raw
/// 2026-07-28 client aimed at it.
async fn echo_pipe(
    headers: &[(&str, &str)],
) -> (
    InlineSession,
    Arc<UpstreamManager>,
    tokio::task::JoinHandle<()>,
) {
    let (upstream, task) = header_echo().await;
    let servers: BTreeMap<String, Server> = [(
        "remote".to_owned(),
        Server {
            enabled: true,
            tags: Vec::new(),
            calls_per_minute: 0,
            transport: Transport::Http {
                url: format!("http://{upstream}/mcp"),
                headers_command: Vec::new(),
                headers: BTreeMap::new(),
                auth: None,
            },
            tools: None,
        },
    )]
    .into_iter()
    .collect();
    let manager = Arc::new(
        UpstreamManager::new(servers)
            .with_connect_timeout(Duration::from_secs(30))
            .with_backoff_base(Duration::from_millis(20)),
    );
    let addr = serve(
        "remote",
        Gateway::new(Arc::clone(&manager), "remote".to_owned()),
    )
    .await;
    let session = InlineSession::new(addr, &endpoint_path("remote")).with_headers(headers);
    (session, manager, task)
}

/// The `_meta` a [`HeaderEcho`] put on the answer, as `(params, headers)`.
fn echoed(result: &serde_json::Value) -> (&serde_json::Value, &serde_json::Value) {
    (&result["_meta"]["params"], &result["_meta"]["headers"])
}

async fn echo_call(session: &mut InlineSession) -> serde_json::Value {
    session
        .request(
            "tools/call",
            serde_json::json!({ "name": "echo", "arguments": {} }),
        )
        .await
}

/// The point of the issue. A client's per-request `Mcp-Param-*` is not the
/// gateway's to eat: 2026-07-28 makes forwarding it a MUST for an
/// intermediary that does not recognise it.
#[tokio::test]
async fn a_param_header_reaches_an_http_upstream() {
    let (mut session, manager, upstream) = echo_pipe(&[("Mcp-Param-Region", "eu")]).await;

    let result = echo_call(&mut session).await;
    let (params, _) = echoed(&result);
    assert_eq!(params["mcp-param-region"], "eu", "{result}");

    manager.shutdown().await;
    upstream.abort();
}

/// Forwarding is per request, not a property of the connection: the next call
/// through the same upstream carries nothing.
#[tokio::test]
async fn a_request_without_one_forwards_nothing() {
    let (mut session, manager, upstream) = echo_pipe(&[("Mcp-Param-Region", "eu")]).await;
    let _ = echo_call(&mut session).await;

    let mut plain = InlineSession::new(session.addr, &session.path);
    let result = echo_call(&mut plain).await;
    let (params, _) = echoed(&result);
    assert_eq!(
        params.as_object().map(serde_json::Map::len),
        Some(0),
        "{result}"
    );

    manager.shutdown().await;
    upstream.abort();
}

/// Everything else the client sent belongs to the hop between the client and
/// the gateway, and stops here — its credential above all.
#[tokio::test]
async fn the_clients_own_credentials_stop_at_the_gateway() {
    let (mut session, manager, upstream) = echo_pipe(&[
        ("Authorization", "Bearer downstream-secret"),
        ("Mcp-Session-Id", "downstream-session"),
        ("Mcp-Param-Region", "eu"),
    ])
    .await;

    let result = echo_call(&mut session).await;
    let (params, headers) = echoed(&result);
    assert_eq!(params["mcp-param-region"], "eu", "{result}");
    assert!(headers.get("authorization").is_none(), "{result}");
    // The upstream connection has a session of its own — rmcp handshakes with
    // it at 2025-11-25 — and the assertion is that it is that one, never the
    // client's.
    assert_ne!(headers["mcp-session-id"], "downstream-session", "{result}");

    manager.shutdown().await;
    upstream.abort();
}

/// A stdio upstream has nowhere to put a header, so the request goes through
/// unchanged rather than failing.
#[tokio::test]
async fn a_stdio_upstream_ignores_them() {
    let manager = manager(&[("fx", "healthy")]);
    let addr = serve("fx", Gateway::new(Arc::clone(&manager), "fx".to_owned())).await;
    let mut session =
        InlineSession::new(addr, &endpoint_path("fx")).with_headers(&[("Mcp-Param-Region", "eu")]);

    let result = session
        .request(
            "tools/call",
            serde_json::json!({ "name": "echo", "arguments": { "message": "hi" } }),
        )
        .await;
    assert!(format!("{result}").contains("hi"), "{result}");
    manager.shutdown().await;
}

/// A fixture server carrying a `calls_per_minute` ceiling.
fn metered_server(mode: &str, calls_per_minute: u32) -> Server {
    Server {
        calls_per_minute,
        ..stdio_server(mode)
    }
}

#[tokio::test]
async fn a_call_over_the_budget_is_refused_and_captured_as_throttled() {
    let state = tempfile::tempdir().unwrap();
    let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
    let manager = Arc::new(
        UpstreamManager::new(
            [("fx".to_owned(), metered_server("healthy", 2))]
                .into_iter()
                .collect(),
        )
        .with_connect_timeout(Duration::from_secs(30)),
    );
    let gateway =
        Gateway::new(Arc::clone(&manager), "fx".to_owned()).with_capture(Arc::clone(&writer));
    let client = connect("fx", gateway).await;

    // Listing is not metered, and doing it first pays the child's start-up
    // cost outside the window — so the three calls below land milliseconds
    // apart and the wait the third is told to take is a whole 30 seconds
    // rather than 30 seconds minus a spawn.
    assert_eq!(
        tool_names(&client.list_all_tools().await.unwrap()),
        ["echo", "reverse"]
    );
    for n in 0..2 {
        client
            .call_tool(call("echo", "hi"))
            .await
            .unwrap_or_else(|err| panic!("call {n} was refused: {err}"));
    }
    let err = client.call_tool(call("echo", "hi")).await.unwrap_err();
    let text = err.to_string();
    // Compared against the one function that writes it, so the wording here
    // cannot drift away from the wording a user reads.
    assert!(
        text.contains(&over_budget("fx", 2, Duration::from_secs(30))),
        "{text}"
    );

    client.cancel().await.unwrap();
    manager.shutdown().await;

    let records = captured(writer.dir());
    let shape: Vec<(Kind, Option<&str>, bool)> = records
        .iter()
        .map(|r| (r.kind, r.tool.as_deref(), r.ok))
        .collect();
    assert_eq!(
        shape,
        [
            (Kind::List, None, true),
            (Kind::Call, Some("echo"), true),
            (Kind::Call, Some("echo"), true),
            (Kind::Throttled, Some("echo"), false),
        ],
        "{records:#?}"
    );
    // On disk under its own name, not only in this build's enum.
    let line = std::fs::read_to_string(
        std::fs::read_dir(writer.dir())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path(),
    )
    .unwrap();
    assert!(line.contains(r#""kind":"throttled""#), "{line}");
}

/// The upgrade promise: a server with no `calls_per_minute` is metered by
/// nothing, however hard a client leans on it.
#[tokio::test]
async fn no_budget_leaves_a_burst_alone() {
    let (client, manager) = gateway_client("healthy").await;
    for n in 0..50 {
        client
            .call_tool(call("echo", "hi"))
            .await
            .unwrap_or_else(|err| panic!("call {n} of an unmetered server was refused: {err}"));
    }
    client.cancel().await.unwrap();
    manager.shutdown().await;
}

/// A throttled call must not be the thing that starts a server, for the same
/// reason a denied one must not: the budget is spent before the upstream is
/// acquired, so being over it cannot be a way to spawn a process.
#[tokio::test]
async fn a_throttled_call_never_reaches_the_upstream() {
    let manager = Arc::new(
        UpstreamManager::new(
            // A budget of 1 with nothing spent yet still lets the first call
            // through, so this asks the second one.
            [("fx".to_owned(), metered_server("exit", 1))]
                .into_iter()
                .collect(),
        )
        .with_connect_timeout(Duration::from_secs(30))
        .with_backoff_base(Duration::from_millis(20)),
    );
    let client = connect("fx", Gateway::new(Arc::clone(&manager), "fx".to_owned())).await;
    // The first call spends the budget and fails on the upstream itself,
    // which is the fixture dying on spawn.
    let first = client.call_tool(call("echo", "x")).await.unwrap_err();
    assert!(!first.to_string().contains("over its budget"), "{first}");
    let second = client.call_tool(call("echo", "x")).await.unwrap_err();
    assert!(second.to_string().contains("over its budget"), "{second}");
    manager.shutdown().await;
}

/// Serves one pipe endpoint holding `token`, with the grace period on or off,
/// and returns the address.
async fn serve_authenticated(
    token: &str,
    require: bool,
) -> (std::net::SocketAddr, Arc<UpstreamManager>) {
    let manager = manager(&[("fx", "healthy")]);
    let endpoints = Endpoints::new(EndpointTable::new([(
        "fx".to_owned(),
        Gateway::new(Arc::clone(&manager), "fx".to_owned()),
    )]));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http_with(
        endpoints,
        GatewayAuth::new(GatewayToken::from_secret(token), require),
        listener,
        std::future::pending(),
    ));
    (addr, manager)
}

/// One raw `GET` of `path`, which is what `daemon status` and `doctor` do to
/// ask whether anything is listening.
async fn raw_get(addr: std::net::SocketAddr, path: &str) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: text/event-stream\r\n\
         Connection: close\r\n\r\n"
    );
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = vec![0u8; 256];
    let read = stream.read(&mut response).await.unwrap();
    String::from_utf8_lossy(&response[..read]).into_owned()
}

#[tokio::test]
async fn the_right_token_reaches_the_endpoint() {
    let (addr, manager) = serve_authenticated("t0ken", true).await;
    let response = raw_post_body(
        addr,
        &endpoint_path("fx"),
        None,
        &[("Authorization", "Bearer t0ken")],
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
    )
    .await;
    assert!(response.contains("200"), "{response}");
    assert!(!response.contains("401"), "{response}");
    manager.shutdown().await;
}

#[tokio::test]
async fn during_the_grace_period_a_loopback_client_without_the_token_still_passes() {
    let (addr, manager) = serve_authenticated("t0ken", false).await;
    // Wrong token and no token alike: this release answers a loopback client
    // whatever it presents, and says so once on stderr.
    for header in [&[][..], &[("Authorization", "Bearer wrong")][..]] {
        let response = raw_post_body(
            addr,
            &endpoint_path("fx"),
            None,
            header,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#,
        )
        .await;
        assert!(!response.contains("401"), "{header:?} -> {response}");
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn require_token_refuses_the_wrong_token_with_a_bearer_challenge() {
    let (addr, manager) = serve_authenticated("t0ken", true).await;
    for header in [&[][..], &[("Authorization", "Bearer wrong")][..]] {
        let response = raw_post_body(
            addr,
            &endpoint_path("fx"),
            None,
            header,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
        )
        .await;
        assert!(response.contains("401"), "{header:?} -> {response}");
        // Bare `Bearer`, with no `realm` and no `resource_metadata`: this is
        // a static token and there is no authorization server for a client
        // to go and discover.
        assert!(
            response
                .lines()
                .any(|line| line.eq_ignore_ascii_case("www-authenticate: Bearer")),
            "{response}"
        );
        assert!(response.contains("mcpgw sync"), "{response}");
    }
    manager.shutdown().await;
}

#[tokio::test]
async fn the_liveness_probe_stays_open_and_the_endpoints_do_not() {
    let (addr, manager) = serve_authenticated("t0ken", true).await;
    // What `daemon status` asks. It reaches no server and carries none of the
    // user's data, and a status that cannot answer is worth more than the
    // nothing it would protect.
    let probe = raw_get(addr, "/mcp").await;
    assert!(!probe.contains("401"), "{probe}");

    // Everything else on the same gateway is closed, `/mcp` included the
    // moment it stops being the bare probe.
    let endpoint = raw_get(addr, &endpoint_path("fx")).await;
    assert!(endpoint.contains("401"), "{endpoint}");
    let post = raw_post_to(addr, "/mcp", None).await;
    assert!(post.contains("401"), "{post}");
    manager.shutdown().await;
}

/// A bind past loopback is only allowed under `require_token`, which is the
/// same rule the daemon preflight enforces — so what a remote client meets
/// there is what a loopback client meets here with the grace period spent.
/// Simulated rather than bound: a second loopback address needs an interface
/// alias on macOS, and the rule under test is the one the flag decides.
#[tokio::test]
async fn a_gateway_that_requires_the_token_refuses_every_client_without_it() {
    let (addr, manager) = serve_authenticated("t0ken", true).await;
    let refused = raw_post_body(
        addr,
        &endpoint_path("fx"),
        None,
        &[],
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    )
    .await;
    assert!(refused.contains("401"), "{refused}");
    manager.shutdown().await;
}

/// The gateway half of per-client scoping: one server, one endpoint, and two
/// clients that see different tools because the URL they dialled says which
/// of them is asking.
mod client_scoping {
    use super::*;
    use mcpgw_core::config::{ClientScope, ClientScopes};
    use mcpgw_core::gateway::{not_allowed_for, not_offered};

    fn scope(servers: &[&str], deny: &[&str]) -> ClientScope {
        let owned = |names: &[&str]| names.iter().map(|name| (*name).to_owned()).collect();
        ClientScope {
            servers: owned(servers),
            max_tools: None,
            tools: (!deny.is_empty()).then(|| mcpgw_core::ToolRules {
                allow: Vec::new(),
                deny: owned(deny),
                ..mcpgw_core::ToolRules::default()
            }),
        }
    }

    /// Serves `fx` with `cursor` scoped as given, and hands back the address
    /// so a test can dial the endpoint both ways.
    async fn serve_scoped(
        server: Server,
        cursor: ClientScope,
        capture: Option<&Arc<CaptureWriter>>,
    ) -> (std::net::SocketAddr, Arc<UpstreamManager>) {
        let manager = Arc::new(
            UpstreamManager::new([("fx".to_owned(), server)].into_iter().collect())
                .with_connect_timeout(Duration::from_secs(30)),
        );
        let scopes = ClientScopes::new([("cursor".to_owned(), cursor)].into_iter().collect());
        let mut gateway =
            Gateway::new(Arc::clone(&manager), "fx".to_owned()).with_client_scopes(scopes);
        if let Some(writer) = capture {
            gateway = gateway.with_capture(Arc::clone(writer));
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve_http(
            "fx".to_owned(),
            gateway,
            listener,
            std::future::pending(),
        ));
        (addr, manager)
    }

    fn tagged(name: &str, client: &str) -> String {
        format!("{}?client={client}", endpoint_path(name))
    }

    #[tokio::test]
    async fn one_endpoint_answers_two_clients_with_different_lists() {
        let (addr, manager) =
            serve_scoped(stdio_server("healthy"), scope(&["fx"], &["reverse"]), None).await;

        let plain = client_at(addr, &endpoint_path("fx")).await;
        assert_eq!(
            tool_names(&plain.list_all_tools().await.unwrap()),
            ["echo", "reverse"]
        );
        plain.cancel().await.unwrap();

        let cursor = client_at(addr, &tagged("fx", "cursor")).await;
        assert_eq!(
            tool_names(&cursor.list_all_tools().await.unwrap()),
            ["echo"]
        );
        // What the scope leaves alone still works through the same endpoint.
        cursor.call_tool(call("echo", "hi")).await.unwrap();

        let err = cursor
            .call_tool(call("reverse", "mcpgw"))
            .await
            .unwrap_err()
            .to_string();
        // Compared against the function that writes it, so the sentence a
        // user reads cannot drift away from the one asserted here.
        assert!(
            err.contains(&not_allowed_for("reverse", "fx", "cursor")),
            "{err}"
        );
        assert!(err.contains("mcpgw clients cursor"), "{err}");
        cursor.cancel().await.unwrap();
        manager.shutdown().await;
    }

    /// A whole server a client is not given: the endpoint is still there —
    /// scoping is not authentication — and it offers that client nothing.
    #[tokio::test]
    async fn a_server_outside_the_scope_offers_the_client_nothing() {
        let (addr, manager) =
            serve_scoped(stdio_server("healthy"), scope(&["other"], &[]), None).await;
        let cursor = client_at(addr, &tagged("fx", "cursor")).await;
        assert!(cursor.list_all_tools().await.unwrap().is_empty());
        let err = cursor
            .call_tool(call("echo", "hi"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(&not_offered("fx", "cursor")), "{err}");
        cursor.cancel().await.unwrap();
        manager.shutdown().await;
    }

    /// An id nothing answers to is not a scope of its own: the request is
    /// served as though it carried no tag at all.
    #[tokio::test]
    async fn an_unknown_tag_is_served_the_unscoped_endpoint() {
        let (addr, manager) =
            serve_scoped(stdio_server("healthy"), scope(&["fx"], &["reverse"]), None).await;
        let client = client_at(addr, &tagged("fx", "nonesuch")).await;
        assert_eq!(
            tool_names(&client.list_all_tools().await.unwrap()),
            ["echo", "reverse"]
        );
        client.cancel().await.unwrap();
        manager.shutdown().await;
    }

    /// The capture column: a client that names itself keeps its own name,
    /// and one that declines is filed under the kind its endpoint was tagged
    /// with rather than under nothing at all.
    #[tokio::test]
    async fn the_tag_names_a_client_that_named_itself_to_nobody() {
        let state = tempfile::tempdir().unwrap();
        let writer = Arc::new(CaptureWriter::under_state_dir(state.path()));
        let (addr, manager) =
            serve_scoped(stdio_server("healthy"), scope(&["fx"], &[]), Some(&writer)).await;

        let mut anonymous = InlineSession::new(addr, &tagged("fx", "cursor")).anonymous();
        anonymous
            .request(
                "tools/call",
                serde_json::json!({ "name": "echo", "arguments": { "message": "quiet" } }),
            )
            .await;
        let mut named = InlineSession::new(addr, &tagged("fx", "cursor")).naming("cursor", "0.48");
        named
            .request(
                "tools/call",
                serde_json::json!({ "name": "echo", "arguments": { "message": "loud" } }),
            )
            .await;
        manager.shutdown().await;

        let clients: Vec<Option<String>> = captured(writer.dir())
            .iter()
            .map(|record| record.client.clone())
            .collect();
        assert!(clients.contains(&Some("cursor".to_owned())), "{clients:?}");
        // The client's own name and version win over the tag: both are
        // self-reported, and one of them says more.
        assert!(
            clients.contains(&Some("cursor/0.48".to_owned())),
            "{clients:?}"
        );
    }
}
