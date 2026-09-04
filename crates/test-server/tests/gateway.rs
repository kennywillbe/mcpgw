use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mcpgw_core::capture::{
    Bodies, CapturePolicy, CaptureRecord, CaptureWriter, Kind, MAX_BODY_BYTES, TRUNCATION_MARKER,
};
use mcpgw_core::endpoints::{EndpointTable, Endpoints, endpoint_path};
use mcpgw_core::gateway::{Gateway, NO_TOOLS_HERE, serve_http, serve_http_with};
use mcpgw_core::upstream::{CallError, UpstreamManager, UpstreamStatus};
use mcpgw_core::{Server, Transport};
use rmcp_client_http::ServiceExt as _;
use rmcp_client_http::transport::StreamableHttpClientTransport;

fn stdio_server(mode: &str) -> Server {
    Server {
        enabled: true,
        tags: Vec::new(),
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
        transport: Transport::Http {
            url: format!("http://{addr}{}", endpoint_path("fx")),
            headers_command: Vec::new(),
            headers: BTreeMap::new(),
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
    tokio::spawn(serve_http_with(endpoints, listener, std::future::pending()));
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
        let mut raw = Self {
            addr,
            path: path.to_owned(),
            id: 0,
            session: None,
        };
        let response = raw
            .post(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                    "protocolVersion":"2026-07-28","capabilities":{},
                    "clientInfo":{"name":"raw","version":"0"}}}"#,
            )
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

/// Pagination is the upstream's to run: the pipe carries the cursor out and
/// the next cursor back, and never collapses the pages into one answer of
/// its own. Collapsing is what hid the upstream's own result fields, and it
/// also left a client with no way to ask for page two.
#[tokio::test]
async fn a_pipe_forwards_pagination_cursors_verbatim() {
    let (addr, manager) = serve_both(&[("fx", "paged")], None).await;
    let mut fx = RawSession::open(addr, &endpoint_path("fx")).await;

    let first = fx.request("tools/list", serde_json::json!({})).await;
    assert_eq!(names_in(&first), ["echo"], "{first}");
    let cursor = first["nextCursor"]
        .as_str()
        .unwrap_or_else(|| panic!("no cursor to page with: {first}"))
        .to_owned();

    let second = fx
        .request("tools/list", serde_json::json!({ "cursor": cursor }))
        .await;
    // The fixture answers page two only for its own cursor, and says so when
    // a different one arrives.
    assert_eq!(names_in(&second), ["reverse"], "{second}");
    assert!(second.get("nextCursor").is_none(), "{second}");
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
}

impl InlineSession {
    const VERSION: &'static str = "2026-07-28";

    fn new(addr: std::net::SocketAddr, path: &str) -> Self {
        Self {
            addr,
            path: path.to_owned(),
            id: 0,
        }
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
            "io.modelcontextprotocol/clientInfo": { "name": "inline", "version": "1" },
            "io.modelcontextprotocol/clientCapabilities": {}
        });
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
        transport: Transport::Http {
            url: format!("http://{addr}{}", endpoint_path("fx")),
            headers_command: Vec::new(),
            headers: BTreeMap::new(),
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
    // Dropped: notifications that stop at the gateway, and the methods it
    // does not forward. A client that believed these would wait forever.
    assert!(
        capabilities["resources"].get("subscribe").is_none(),
        "{capabilities}"
    );
    assert!(
        capabilities["resources"].get("listChanged").is_none(),
        "{capabilities}"
    );
    assert!(
        capabilities["tools"].get("listChanged").is_none(),
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
