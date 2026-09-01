//! End-to-end coverage for the `mcpgw connect` stdio↔HTTP bridge: a real
//! client spawns the real binary, which pipes to a gateway served in-process.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use mcpgw_core::gateway::{Gateway, serve_http};
use mcpgw_core::upstream::UpstreamManager;
use mcpgw_core::{Server, Transport};
use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;

/// The scripted fixture server lives in a sibling package, so `CARGO_BIN_EXE`
/// cannot name it here; it sits next to this test executable's parent
/// (`target/<profile>/`), which holds for every cargo layout the suite runs
/// under and CI always builds the whole workspace.
fn fixture_binary() -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().parent().unwrap();
    let path = dir.join(format!("mcpgw-test-server{}", std::env::consts::EXE_SUFFIX));
    assert!(
        path.exists(),
        "fixture binary missing at {} — build the workspace first",
        path.display()
    );
    path
}

/// Serves a gateway piping the healthy fixture on an ephemeral port and
/// returns its `/mcp` URL plus the manager (for shutdown).
async fn fixture_gateway() -> (String, Arc<UpstreamManager>) {
    let server = Server {
        enabled: true,
        tags: Vec::new(),
        transport: Transport::Stdio {
            command: fixture_binary().to_string_lossy().into_owned(),
            args: vec!["healthy".to_owned()],
            env: BTreeMap::new(),
        },
    };
    let manager = Arc::new(
        UpstreamManager::new(BTreeMap::from([("fx".to_owned(), server)]))
            .with_connect_timeout(Duration::from_secs(5))
            .with_backoff_base(Duration::from_millis(20)),
    );
    let gateway = Gateway::new(Arc::clone(&manager), "fx".to_owned());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(serve_http(gateway, listener, std::future::pending()));
    (format!("http://{addr}/mcp"), manager)
}

type Client = rmcp::service::RunningService<rmcp::RoleClient, ()>;

/// Spawns `mcpgw connect --url <url>` the way a stdio-only client would.
async fn connect_client(url: &str) -> Client {
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"));
    command.args(["connect", "--url", url]);
    // The bridge logs to stderr; drop it so it does not mix into test output.
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    ().serve(transport).await.unwrap()
}

#[tokio::test]
async fn bridges_a_stdio_client_to_the_http_gateway() {
    let (url, manager) = fixture_gateway().await;
    let client = connect_client(&url).await;

    // Pipe mode all the way down: the fixture's tool names arrive unprefixed.
    let tools = client.list_all_tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["echo", "reverse"]);

    let result = client
        .call_tool(
            rmcp::model::CallToolRequestParams::new("echo".to_owned()).with_arguments(
                serde_json::json!({ "message": "over the bridge" })
                    .as_object()
                    .cloned()
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
    let text = serde_json::to_string(&result.content).unwrap();
    assert!(text.contains("over the bridge"), "{text}");

    client.cancel().await.unwrap();
    manager.shutdown().await;
}

#[tokio::test]
async fn a_down_gateway_is_reported_with_the_fix() {
    // Port 1 is never a gateway; the bridge is lazy, so the handshake with
    // the client still succeeds and the failure lands on the first request.
    let client = connect_client("http://127.0.0.1:1/mcp").await;

    let err = client.list_all_tools().await.unwrap_err();
    let message = err.to_string();
    assert!(message.contains("mcpgw serve"), "{message}");
    assert!(message.contains("127.0.0.1:1"), "{message}");

    client.cancel().await.unwrap();
}

#[test]
fn url_defaults_to_the_serve_port() {
    let out = assert_cmd::Command::cargo_bin("mcpgw")
        .unwrap()
        .args(["connect", "--help"])
        .output()
        .unwrap();
    let help = String::from_utf8(out.stdout).unwrap();
    assert!(help.contains("8137"), "{help}");
}
