use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use mcpgw_core::gateway::{Gateway, serve_http};
use mcpgw_core::probe::{ProbeError, probe_server};
use mcpgw_core::upstream::UpstreamManager;
use mcpgw_core::{Server, Transport};

/// The budget every probe that is *not* testing the timeout gets.
///
/// These tests race a real child process against a tokio timer, so a budget
/// tuned to an idle machine turns a slow spawn on a loaded runner into a
/// `Timeout` where the test expected a handshake outcome. The number is far
/// larger than the work needs because nothing here waits for it: a probe that
/// is going to succeed or fail on the handshake does so immediately, and only
/// a genuinely wedged one spends the budget.
const GENEROUS: u64 = 60_000;

fn stdio_server(command: &str, args: &[&str]) -> Server {
    Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
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
    probe_server("fx", &server, None, Duration::from_millis(timeout_ms)).await
}

#[tokio::test]
async fn healthy_server_reports_identity_and_tools() {
    let success = probe_mode("healthy", GENEROUS).await.unwrap();
    assert_eq!(success.server_name, "mcpgw-test-server");
    assert_eq!(success.server_version, "9.9.9");
    assert_eq!(success.tool_count(), 2);
}

#[tokio::test]
async fn unresponsive_server_times_out() {
    // The one test that wants a short budget, and the only one a slow runner
    // cannot flip: the `slow` fixture never answers, so waiting longer only
    // ever produces the same timeout.
    let err = probe_mode("slow", 300).await.unwrap_err();
    assert!(matches!(err, ProbeError::Timeout { .. }), "got: {err}");
}

#[tokio::test]
async fn garbage_output_fails_the_handshake() {
    // Not GENEROUS: whichever way the transport treats bad frames, either
    // outcome is accepted below, so a budget this test can spend in full is a
    // budget it *will* spend in full. Short enough to keep the suite quick,
    // long enough that a slow spawn still gets to produce a handshake error.
    let err = probe_mode("garbage", 5000).await.unwrap_err();
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
    let err = probe_mode("exit", GENEROUS).await.unwrap_err();
    assert!(matches!(err, ProbeError::Handshake { .. }), "got: {err}");
}

#[tokio::test]
async fn missing_binary_is_a_spawn_error() {
    let server = stdio_server("/nonexistent/mcpgw-no-such-binary", &[]);
    let err = probe_server("fx", &server, None, Duration::from_millis(GENEROUS))
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
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            url: format!("http://{addr}/s/fx"),
            headers_command: Vec::new(),
            headers: BTreeMap::new(),
            auth: None,
        },
    };

    let success = probe_server("fx", &server, None, Duration::from_millis(GENEROUS))
        .await
        .unwrap();
    assert_eq!(success.server_name, "mcpgw");
    assert_eq!(success.tool_count(), 2);
    manager.shutdown().await;
}

#[tokio::test]
async fn unreachable_http_server_fails_the_handshake() {
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            // Port 1 on loopback refuses connections instantly.
            url: "http://127.0.0.1:1/mcp".to_owned(),
            headers_command: Vec::new(),
            headers: BTreeMap::new(),
            auth: None,
        },
    };
    let err = probe_server("fx", &server, None, Duration::from_millis(GENEROUS))
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
            // Long enough that spawning the fixture under parallel load stays
            // inside it; this gateway is scenery, not the thing under test.
            .with_connect_timeout(Duration::from_secs(30))
            .with_backoff_base(Duration::from_millis(20)),
    );
    let gateway = Gateway::new(Arc::clone(&manager), "fx".to_owned());

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

/// A 2026-07-28 server has no `initialize` to answer: the handshake was
/// replaced by `server/discover` and per-request metadata. Reporting such a
/// server as unreachable would be doctor calling a healthy server broken, so
/// the probe tries the newer lifecycle when the older one is refused.
#[tokio::test]
async fn a_server_with_no_initialize_is_probed_over_discover() {
    let success = probe_mode("modern", GENEROUS).await.unwrap();
    assert_eq!(success.server_name, "mcpgw-test-server-modern");
    assert_eq!(success.server_version, "9.9.9");
    assert_eq!(success.tool_count(), 2);
}

/// `doctor --probe` runs the command too, because what it proves is that the
/// server answers the way mcpgw would reach it — and a command that cannot
/// mint a credential is the reason it would not.
#[tokio::test]
async fn a_failing_headers_command_fails_the_probe_by_name() {
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        calls_per_minute: 0,
        tools: None,
        transport: Transport::Http {
            url: "http://127.0.0.1:1/mcp".to_owned(),
            headers_command: vec![
                env!("CARGO_BIN_EXE_mcpgw-test-server").to_owned(),
                "headers-fail".to_owned(),
            ],
            headers: BTreeMap::new(),
            auth: None,
        },
    };
    let err = probe_server("fx", &server, None, Duration::from_millis(GENEROUS))
        .await
        .unwrap_err();
    let ProbeError::HeadersCommand { message } = &err else {
        panic!("expected HeadersCommand, got {err}");
    };
    assert!(message.contains("mcpgw-test-server"), "{message}");
    assert!(message.contains("no vault session"), "{message}");
}
