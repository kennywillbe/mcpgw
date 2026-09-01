use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mcpgw_core::capture::{CaptureRecord, CaptureWriter, Kind, MAX_BODY_BYTES, TRUNCATION_MARKER};
use mcpgw_core::gateway::{Gateway, resolve, serve_http};
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

    // The gateway identifies itself, not the upstream.
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

    // Every record is stamped with the writer's session, and calls carry
    // both sides of the exchange.
    assert!(records.iter().all(|r| r.session == writer.session()));
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
    use std::fmt::Write as _;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#;
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\nHost: {addr}\r\nContent-Type: application/json\r\n\
         Accept: application/json, text/event-stream\r\nContent-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(origin) = origin {
        let _ = write!(request, "Origin: {origin}\r\n");
    }
    request.push_str("\r\n");
    request.push_str(body);

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await.unwrap();
    String::from_utf8_lossy(&response)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned()
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
