use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mcpgw_core::gateway::{Gateway, serve_http};
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

/// Boots a gateway piping one fixture upstream on an ephemeral port and
/// returns a connected MCP client plus the manager for shutdown.
async fn gateway_client(
    mode: &str,
) -> (
    rmcp_client_http::service::RunningService<rmcp_client_http::RoleClient, ()>,
    Arc<UpstreamManager>,
) {
    let mut servers = BTreeMap::new();
    servers.insert("fx".to_owned(), stdio_server(mode));
    let manager = Arc::new(
        UpstreamManager::new(servers)
            .with_connect_timeout(Duration::from_secs(5))
            .with_backoff_base(Duration::from_millis(20)),
    );
    let gateway = Gateway::new(Arc::clone(&manager), "fx".to_owned());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(gateway, listener, std::future::pending()));

    let transport = StreamableHttpClientTransport::from_uri(format!("http://{addr}/mcp"));
    let client = ().serve(transport).await.unwrap();
    (client, manager)
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
    let params = rmcp_client_http::model::CallToolRequestParams::new("reverse").with_arguments(
        serde_json::json!({ "message": "mcpgw" })
            .as_object()
            .cloned()
            .unwrap(),
    );
    let result = client.call_tool(params).await.unwrap();
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
