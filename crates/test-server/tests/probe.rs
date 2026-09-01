use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mcpgw_core::gateway::{Gateway, serve_http};
use mcpgw_core::probe::{ProbeError, probe_server};
use mcpgw_core::upstream::UpstreamManager;
use mcpgw_core::{Server, Transport};

fn stdio_server(command: &str, args: &[&str]) -> Server {
    Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Stdio {
            command: command.to_owned(),
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            env: BTreeMap::new(),
        },
    }
}

async fn probe_mode(
    mode: &str,
    timeout_ms: u64,
) -> Result<mcpgw_core::probe::ProbeSuccess, ProbeError> {
    // Same-package binaries get this env var from cargo at test build time.
    let server = stdio_server(env!("CARGO_BIN_EXE_mcpgw-test-server"), &[mode]);
    probe_server(&server, Duration::from_millis(timeout_ms)).await
}

#[tokio::test]
async fn healthy_server_reports_identity_and_tools() {
    let success = probe_mode("healthy", 5000).await.unwrap();
    assert_eq!(success.server_name, "mcpgw-test-server");
    assert_eq!(success.server_version, "9.9.9");
    assert_eq!(success.tool_count, 2);
}

#[tokio::test]
async fn unresponsive_server_times_out() {
    let err = probe_mode("slow", 300).await.unwrap_err();
    assert!(matches!(err, ProbeError::Timeout { .. }), "got: {err}");
}

#[tokio::test]
async fn garbage_output_fails_the_handshake() {
    let err = probe_mode("garbage", 3000).await.unwrap_err();
    // Non-JSON output either breaks the handshake outright or starves it
    // into the timeout, depending on how the transport treats bad frames.
    assert!(
        matches!(
            err,
            ProbeError::Handshake { .. } | ProbeError::Timeout { .. }
        ),
        "got: {err}"
    );
}

#[tokio::test]
async fn immediate_exit_fails_the_handshake() {
    let err = probe_mode("exit", 3000).await.unwrap_err();
    assert!(matches!(err, ProbeError::Handshake { .. }), "got: {err}");
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error() {
    let server = stdio_server("/nonexistent/mcpgw-no-such-binary", &[]);
    let err = probe_server(&server, Duration::from_millis(1000))
        .await
        .unwrap_err();
    assert!(
        matches!(err, ProbeError::Spawn { .. } | ProbeError::Handshake { .. }),
        "got: {err}"
    );
}

#[tokio::test]
async fn http_server_reports_identity_and_tools() {
    // A gateway piping the fixture is the http MCP server under probe.
    let (addr, manager) = fixture_gateway().await;
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Http {
            url: format!("http://{addr}/mcp"),
            headers: BTreeMap::new(),
        },
    };

    let success = probe_server(&server, Duration::from_secs(10))
        .await
        .unwrap();
    assert_eq!(success.server_name, "mcpgw");
    assert_eq!(success.tool_count, 2);
    manager.shutdown().await;
}

#[tokio::test]
async fn unreachable_http_server_fails_the_handshake() {
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Http {
            // Port 1 on loopback refuses connections instantly.
            url: "http://127.0.0.1:1/mcp".to_owned(),
            headers: BTreeMap::new(),
        },
    };
    let err = probe_server(&server, Duration::from_secs(5))
        .await
        .unwrap_err();
    assert!(matches!(err, ProbeError::Handshake { .. }), "got: {err}");
}

/// Boots a gateway that pipes the healthy fixture over Streamable HTTP and
/// returns its address plus the manager, so the probe has a real remote
/// MCP server to talk to.
async fn fixture_gateway() -> (std::net::SocketAddr, Arc<UpstreamManager>) {
    let servers = [(
        "fx".to_owned(),
        stdio_server(env!("CARGO_BIN_EXE_mcpgw-test-server"), &["healthy"]),
    )]
    .into_iter()
    .collect();
    let manager = Arc::new(
        UpstreamManager::new(servers)
            .with_connect_timeout(Duration::from_secs(5))
            .with_backoff_base(Duration::from_millis(20)),
    );
    let gateway = Gateway::new(Arc::clone(&manager), "fx".to_owned());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(gateway, listener, std::future::pending()));
    (addr, manager)
}
