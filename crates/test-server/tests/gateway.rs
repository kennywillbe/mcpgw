use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mcpgw_core::capture::{CaptureRecord, CaptureWriter, Kind, MAX_BODY_BYTES, TRUNCATION_MARKER};
use mcpgw_core::endpoints::{EndpointTable, Endpoints, endpoint_path};
use mcpgw_core::gateway::{Gateway, resolve, serve_http, serve_http_with};
use mcpgw_core::upstream::UpstreamManager;
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
            .with_connect_timeout(Duration::from_secs(5))
            .with_backoff_base(Duration::from_millis(20)),
    )
}

/// Serves `gateway` on an ephemeral port and returns the bound address.
async fn serve(gateway: Gateway) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(gateway, listener, std::future::pending()));
    addr
}

/// Serves `gateway` on an ephemeral port and connects a client to it.
async fn connect(gateway: Gateway) -> Client {
    let addr = serve(gateway).await;
    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
    ().serve(transport).await.unwrap()
}

/// Boots a gateway piping one fixture upstream named `fx` and returns a
/// connected MCP client plus the manager for shutdown.
async fn gateway_client(mode: &str) -> (Client, Arc<UpstreamManager>) {
    let manager = manager(&[("fx", mode)]);
    let client = connect(Gateway::new(Arc::clone(&manager), "fx".to_owned())).await;
    (client, manager)
}

/// Same, but aggregating the given `(name, mode)` upstreams.
async fn aggregate_client(upstreams: &[(&str, &str)]) -> (Client, Arc<UpstreamManager>) {
    let manager = manager(upstreams);
    let names = upstreams.iter().map(|(n, _)| (*n).to_owned()).collect();
    let client = connect(Gateway::aggregate(Arc::clone(&manager), names)).await;
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
    let client = connect(gateway).await;

    let started = std::time::Instant::now();
    let err = client.call_tool(call("echo", "hi")).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("fx"), "should name the upstream: {text}");
    assert!(text.contains("deadline"), "should say why: {text}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "waited {:?}, so nothing bounded the request",
        started.elapsed()
    );

    // tools/list is on the same clock.
    let err = client.list_all_tools().await.unwrap_err();
    assert!(err.to_string().contains("fx"), "{err}");

    client.cancel().await.unwrap();
    manager.shutdown().await;
}

#[tokio::test]
async fn aggregate_merges_prefixed_tools_from_every_upstream() {
    let (client, manager) = aggregate_client(&[("fx1", "healthy"), ("fx2", "healthy")]).await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(
        names,
        ["fx1__echo", "fx1__reverse", "fx2__echo", "fx2__reverse"]
    );
    manager.shutdown().await;
}

#[tokio::test]
async fn aggregate_routes_calls_to_the_named_upstream() {
    let (client, manager) = aggregate_client(&[("fx1", "healthy"), ("fx2", "healthy")]).await;

    let reversed = client.call_tool(call("fx2__reverse", "abc")).await.unwrap();
    let text = format!("{reversed:?}");
    assert!(text.contains("cba"), "{text}");

    let echoed = client.call_tool(call("fx1__echo", "abc")).await.unwrap();
    let text = format!("{echoed:?}");
    assert!(text.contains("abc"), "{text}");
    manager.shutdown().await;
}

#[tokio::test]
async fn unknown_prefix_errors_and_names_the_known_servers() {
    let (client, manager) = aggregate_client(&[("fx1", "healthy"), ("fx2", "healthy")]).await;
    let err = client.call_tool(call("nope__echo", "x")).await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("fx1") && text.contains("fx2"), "{text}");
    manager.shutdown().await;
}

#[tokio::test]
async fn one_broken_upstream_never_hides_the_healthy_ones() {
    let (client, manager) = aggregate_client(&[("fx1", "healthy"), ("fx2", "exit")]).await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["fx1__echo", "fx1__reverse"]);

    // The surviving upstream stays fully usable.
    let echoed = client
        .call_tool(call("fx1__echo", "still here"))
        .await
        .unwrap();
    assert!(format!("{echoed:?}").contains("still here"));
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
    tokio::spawn(serve_http(gw1, listener, std::future::pending()));

    let remote = Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Http {
            url: format!("http://{addr}/mcp"),
            headers: BTreeMap::new(),
        },
    };
    let outer = Arc::new(
        UpstreamManager::new([(upstream.to_owned(), remote)].into_iter().collect())
            .with_connect_timeout(Duration::from_secs(5))
            .with_backoff_base(Duration::from_millis(20)),
    );
    let client = connect(Gateway::aggregate(
        Arc::clone(&outer),
        vec![upstream.to_owned()],
    ))
    .await;
    (client, outer, inner)
}

#[tokio::test]
async fn http_upstream_lists_tools_through_the_chain() {
    let (client, outer, inner) = chained_client("remote").await;
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["remote__echo", "remote__reverse"]);
    outer.shutdown().await;
    inner.shutdown().await;
}

#[tokio::test]
async fn http_upstream_round_trips_a_tool_call() {
    let (client, outer, inner) = chained_client("remote").await;
    let result = client
        .call_tool(call("remote__reverse", "mcpgw"))
        .await
        .unwrap();
    let text = format!("{result:?}");
    assert!(text.contains("wgpcm"), "{text}");
    outer.shutdown().await;
    inner.shutdown().await;
}

#[test]
fn resolve_prefers_the_longest_known_server_name() {
    let servers = ["a".to_owned(), "a_b".to_owned()];
    // "a_b__t" also parses as server "a" + tool "b__t" only if a separator
    // followed "a" — it does not, so the longer server wins outright.
    assert_eq!(resolve("a_b__t", &servers), Some(("a_b", "t")));

    // `__` inside a tool name stays legal: only the first known-server
    // boundary is consumed.
    let servers = ["a".to_owned()];
    assert_eq!(resolve("a__b__t", &servers), Some(("a", "b__t")));

    assert_eq!(resolve("nope__t", &servers), None);
    // A bare name without the separator belongs to no server.
    assert_eq!(resolve("a", &servers), None);
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
    let manager = manager(&[("fx1", "healthy"), ("fx2", "exit")]);
    let gateway = Gateway::aggregate(
        Arc::clone(&manager),
        vec!["fx1".to_owned(), "fx2".to_owned()],
    )
    .with_capture(Arc::clone(&writer));
    let client = connect(gateway).await;

    // One list (two upstream attempts, one of which cannot start), one good
    // call and one call into the dead upstream.
    client.list_all_tools().await.unwrap();
    client.call_tool(call("fx1__echo", "hi")).await.unwrap();
    client.call_tool(call("fx2__echo", "hi")).await.unwrap_err();
    client.cancel().await.unwrap();
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

    // One client means one downstream session: every record carries the same
    // id, and it is the client's, not the gateway process's fallback.
    let session = records[0].session.clone();
    assert!(records.iter().all(|r| r.session == session), "{records:#?}");
    assert_ne!(session, writer.session());

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
    let client = connect(gateway).await;

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
    let addr = serve(Gateway::new(Arc::clone(&manager), "fx".to_owned())).await;

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

/// Serves `gateway` at `/mcp` and one pipe endpoint per upstream at
/// `/s/<name>`, all over the one shared manager, and returns the address.
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
    let aggregate = with_capture(Gateway::aggregate(Arc::clone(&manager), names));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http_with(
        aggregate,
        Some(endpoints),
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

/// The point of the endpoint table: three simultaneous views of the same
/// upstreams — one aggregate and one per server — sharing a single
/// `UpstreamManager`, so no view costs an extra process.
#[tokio::test]
async fn per_server_endpoints_serve_their_own_tools_next_to_the_aggregate() {
    let (addr, manager) = serve_both(&[("fx1", "healthy"), ("fx2", "healthy")], None).await;

    let aggregate = client_at(addr, "/mcp").await;
    let fx1 = client_at(addr, &endpoint_path("fx1")).await;
    let fx2 = client_at(addr, &endpoint_path("fx2")).await;

    let (agg_tools, fx1_tools, fx2_tools) = tokio::join!(
        aggregate.list_all_tools(),
        fx1.list_all_tools(),
        fx2.list_all_tools(),
    );
    // The aggregate keeps prefixing; each endpoint hands out bare names.
    assert_eq!(
        tool_names(&agg_tools.unwrap()),
        ["fx1__echo", "fx1__reverse", "fx2__echo", "fx2__reverse"]
    );
    assert_eq!(tool_names(&fx1_tools.unwrap()), ["echo", "reverse"]);
    assert_eq!(tool_names(&fx2_tools.unwrap()), ["echo", "reverse"]);

    // And the unprefixed call lands on the server whose endpoint took it.
    let (one, two) = tokio::join!(
        fx1.call_tool(call("echo", "from fx1")),
        fx2.call_tool(call("reverse", "abcd")),
    );
    assert!(format!("{:?}", one.unwrap()).contains("from fx1"));
    assert!(format!("{:?}", two.unwrap()).contains("dcba"));

    for client in [aggregate, fx1, fx2] {
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

/// Pinned, because it is a decision rather than an oversight: resource URIs
/// and prompt names cannot be namespaced the way `server__tool` can, so the
/// aggregate serves none of them and `/s/<name>` is where they live.
#[tokio::test]
async fn the_aggregate_serves_no_resources_or_prompts() {
    let (client, manager) = aggregate_client(&[("fx1", "healthy"), ("fx2", "healthy")]).await;

    assert!(client.list_all_resources().await.unwrap().is_empty());
    assert!(
        client
            .list_all_resource_templates()
            .await
            .unwrap()
            .is_empty()
    );
    assert!(client.list_all_prompts().await.unwrap().is_empty());

    // Reading or getting one is a plain "not here", not a made-up empty body.
    assert!(client.read_resource(read(RESOURCE_URI)).await.is_err());
    assert!(client.get_prompt(prompt("summarize", "x")).await.is_err());

    // And the aggregate advertises exactly what it merges.
    let capabilities = client.peer_info().unwrap().capabilities.clone();
    assert!(capabilities.tools.is_some());
    assert!(capabilities.resources.is_none() && capabilities.prompts.is_none());

    client.cancel().await.unwrap();
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
    let client = connect(gateway).await;

    let started = std::time::Instant::now();
    let err = client.list_all_prompts().await.unwrap_err();
    let text = err.to_string();
    assert!(text.contains("fx"), "should name the upstream: {text}");
    assert!(text.contains("deadline"), "should say why: {text}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
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
    let aggregate = client_at(addr, "/mcp").await;
    let (first, second, third) = tokio::join!(
        one.call_tool(call("echo", "from one")),
        two.call_tool(call("echo", "from two")),
        aggregate.call_tool(call("fx1__echo", "from the aggregate")),
    );
    first.unwrap();
    second.unwrap();
    third.unwrap();
    for client in [one, two, aggregate] {
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
        ["mcp", "s/fx1", "s/fx2"],
        "{records:#?}"
    );
    // The aggregate still resolves the prefix to the real upstream, so the
    // endpoint and the server are independent facts about one call.
    assert_eq!(by_endpoint["mcp"].server, "fx1");
    assert_eq!(by_endpoint["s/fx1"].server, "fx1");
    assert_eq!(by_endpoint["s/fx2"].server, "fx2");

    // Three connections, three distinct sessions, none of them the
    // per-process fallback.
    let sessions: std::collections::BTreeSet<&str> =
        records.iter().map(|r| r.session.as_str()).collect();
    assert_eq!(sessions.len(), 3, "{records:#?}");
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
        self.id += 1;
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": self.id, "method": method, "params": params
        });
        let response = self.post(&body.to_string()).await;
        // The transport answers over SSE, so the JSON-RPC message is the
        // payload of the one `data:` event that carries a result.
        let message = response
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
            .find(|value| value.get("id").is_some())
            .unwrap_or_else(|| panic!("no JSON-RPC answer in: {response}"));
        assert!(message.get("error").is_none(), "{method} failed: {message}");
        message["result"].clone()
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

/// The aggregate is allowed to be lossy, and pinning that keeps the two
/// modes honest: it exists to merge N servers under `server__tool` names, so
/// it collapses pagination and answers with a result of its own making.
#[tokio::test]
async fn the_aggregate_keeps_its_collapsed_prefixed_answer() {
    let (addr, manager) = serve_both(&[("fx", "healthy")], None).await;
    let mut aggregate = RawSession::open(addr, "/mcp").await;

    let result = aggregate.request("tools/list", serde_json::json!({})).await;

    assert_eq!(names_in(&result), ["fx__echo", "fx__reverse"], "{result}");
    assert!(result.get("ttlMs").is_none(), "{result}");
    assert!(result.get("cacheScope").is_none(), "{result}");
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

    // The aggregate is nobody's proxy in particular: it stays mcpgw.
    let aggregate = client_at(addr, "/mcp").await;
    let identity = aggregate.peer_info().unwrap().server_info.clone().unwrap();
    assert_eq!(identity.name, "mcpgw", "{identity:?}");

    for client in [early, later, aggregate] {
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
        let message = response
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter_map(|payload| serde_json::from_str::<serde_json::Value>(payload).ok())
            .find(|value| value.get("id").is_some())
            .or_else(|| {
                let body = response.split("\r\n\r\n").nth(1)?;
                serde_json::from_str::<serde_json::Value>(body).ok()
            })
            .unwrap_or_else(|| panic!("no JSON-RPC answer in: {response}"));
        assert!(message.get("error").is_none(), "{method} failed: {message}");
        message["result"].clone()
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

/// The aggregate answers a newer client too, and it builds its merged result
/// itself — so it needs the same fields, from the same rule.
#[tokio::test]
async fn the_aggregate_answers_a_newer_client_in_its_own_revision() {
    let (addr, manager) = serve_both(&[("fx", "legacy")], None).await;
    let mut aggregate = InlineSession::new(addr, "/mcp");

    let result = aggregate.request("tools/list", serde_json::json!({})).await;

    assert_eq!(names_in(&result), ["fx__echo", "fx__reverse"], "{result}");
    assert_eq!(result["resultType"], "complete", "{result}");
    assert_eq!(result["ttlMs"], 0, "{result}");
    assert_eq!(result["cacheScope"], "private", "{result}");
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
