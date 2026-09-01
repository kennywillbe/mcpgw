use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

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

/// Serves `gateway` on an ephemeral port and connects a client to it.
async fn connect(gateway: Gateway) -> Client {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(gateway, listener, std::future::pending()));

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
