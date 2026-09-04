//! End-to-end coverage for the `mcpgw connect` stdio↔HTTP bridge: a real
//! client spawns the real binary, which pipes to a gateway served in-process.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mcpgw_core::gateway::{Gateway, serve_http};
use mcpgw_core::upstream::UpstreamManager;
use mcpgw_core::{Server, Transport};
use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;
use rmcp::transport::async_rw::AsyncRwTransport;
use tokio::io::{AsyncBufReadExt as _, BufReader};

mod util;
use util::{fixture_binary, fixture_config, free_port, mcpgw};

/// Serves a gateway piping the healthy fixture on an ephemeral port and
/// returns the fixture's own endpoint URL plus the manager (for shutdown).
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
    (format!("http://{addr}/s/fx"), manager)
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

/// A client on the current revision never sends `initialize`: it opens with
/// `server/discover` and carries its protocol version in every request's
/// `_meta` (SEP-2575). The bridge has to serve that client as readily as the
/// handshake one, because it is the lifecycle a current client uses.
#[tokio::test]
async fn a_stdio_client_on_the_current_revision_is_bridged_too() {
    use rmcp::ClientServiceExt as _;

    let (url, manager) = fixture_gateway().await;
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"));
    command.args(["connect", "--url", &url]);
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let client = ()
        .serve_with_lifecycle(
            transport,
            rmcp::ClientLifecycleMode::Discover {
                preferred_versions: vec![rmcp::model::ProtocolVersion::V_2026_07_28],
            },
        )
        .await
        .unwrap();

    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tool_names(&tools), ["echo", "reverse"]);

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

/// A sandbox home with one fixture server per name already configured, which
/// is everything the bridge needs to be able to serve a gateway of its own.
fn home_serving(names: &[&str]) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), fixture_config(names)).unwrap();
    home
}

/// The bridge as a stdio-only client runs it, in `home`'s sandbox, with its
/// stderr collected as it arrives.
///
/// The child is spawned here rather than by rmcp's own child transport
/// because the test has to own it: that transport kills the process when it
/// is dropped, and what half of this file is about is what the bridge does
/// when its stdin closes instead.
async fn spawn_bridge(
    home: &Path,
    args: &[&str],
) -> (Client, tokio::process::Child, Arc<Mutex<String>>) {
    let mut command = tokio::process::Command::from(mcpgw(home));
    let mut child = util::spawn_retrying_while_busy(
        command
            .arg("connect")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    );
    let stdout = child.stdout.take().unwrap();
    let stdin = child.stdin.take().unwrap();

    let errors = Arc::new(Mutex::new(String::new()));
    let mut lines = BufReader::new(child.stderr.take().unwrap()).lines();
    tokio::spawn({
        let errors = Arc::clone(&errors);
        async move {
            while let Ok(Some(line)) = lines.next_line().await {
                let mut errors = errors.lock().unwrap();
                errors.push_str(&line);
                errors.push('\n');
            }
        }
    });

    let client = ().serve(AsyncRwTransport::new_client(stdout, stdin)).await.unwrap();
    (client, child, errors)
}

/// Closes the bridge's stdin the way a client that quits does, and returns
/// the exit status of the process that was behind it.
async fn ends(client: Client, mut child: tokio::process::Child) -> std::process::ExitStatus {
    // Cancelling ends the service task that owns the transport, which drops
    // the child's stdin — the same EOF Claude Desktop leaves behind.
    client.cancel().await.unwrap();
    tokio::time::timeout(Duration::from_secs(60), child.wait())
        .await
        .expect("the bridge did not exit after its stdin closed")
        .unwrap()
}

fn tool_names(tools: &[rmcp::model::Tool]) -> Vec<&str> {
    tools.iter().map(|t| t.name.as_ref()).collect()
}

fn said(errors: &Arc<Mutex<String>>) -> String {
    errors.lock().unwrap().clone()
}

#[tokio::test]
async fn a_bridge_with_nothing_to_bridge_to_serves_a_gateway_for_the_session() {
    let home = home_serving(&["fx1"]);
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}/mcp");
    let (client, child, errors) =
        spawn_bridge(home.path(), &["--url", &url, "--server", "fx1"]).await;

    // The bridge answers, which it could only do over a gateway it started.
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tool_names(&tools), ["echo", "reverse"]);
    let said = said(&errors);
    // The URL named is the endpoint the bridge was pointed at, which with
    // `--server` is the server's own face on that gateway.
    assert!(
        said.contains(&format!("no gateway at http://127.0.0.1:{port}/s/fx1")),
        "{said}"
    );
    assert!(said.contains("serving one for this session"), "{said}");
    assert!(said.contains("mcpgw daemon install"), "{said}");

    // Published like any other gateway's, and withdrawn on the way out —
    // which a bridge that was killed rather than closed would not do.
    let state = home.path().join("state");
    assert!(
        mcpgw_core::runtime::read_record(&state, port)
            .unwrap()
            .is_some()
    );

    let status = ends(client, child).await;
    assert!(status.success(), "{status}");
    assert_eq!(
        mcpgw_core::runtime::read_record(&state, port).unwrap(),
        None
    );
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
        "the gateway the bridge started outlived it on port {port}"
    );
}

#[tokio::test]
async fn a_gateway_that_is_already_up_is_bridged_to_and_nothing_is_started() {
    let home = home_serving(&["fx1"]);
    let (mut gateway, addr, _endpoints) = util::serve(home.path(), &[]).await;
    let url = format!("http://{addr}/mcp");

    let (client, child, errors) =
        spawn_bridge(home.path(), &["--url", &url, "--server", "fx1"]).await;
    let tools = client.list_all_tools().await.unwrap();
    assert_eq!(tool_names(&tools), ["echo", "reverse"]);
    let said = said(&errors);
    assert!(!said.contains("serving one for this session"), "{said}");

    ends(client, child).await;
    gateway.kill().await.unwrap();
}

#[tokio::test]
async fn a_service_installed_on_the_port_is_never_raced_by_the_bridge() {
    let home = home_serving(&["fx1"]);
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}/mcp");
    // Installed, and nothing behind it: the state a stopped service leaves.
    util::record_installed_spec(
        home.path(),
        &std::env::current_exe().unwrap(),
        "127.0.0.1",
        port,
    );

    let (client, child, errors) =
        spawn_bridge(home.path(), &["--url", &url, "--server", "fx1"]).await;
    let message = client.list_all_tools().await.unwrap_err().to_string();
    assert!(
        message.contains("the installed service is not running"),
        "{message}"
    );
    assert!(message.contains("mcpgw daemon start"), "{message}");

    let said = said(&errors);
    assert!(!said.contains("serving one for this session"), "{said}");
    assert!(
        said.contains("the installed service is not running"),
        "{said}"
    );
    assert!(
        std::net::TcpListener::bind(("127.0.0.1", port)).is_ok(),
        "the bridge started a gateway on the service's port {port}"
    );

    ends(client, child).await;
}

#[tokio::test]
async fn two_bridges_starting_at_once_end_up_on_one_gateway() {
    let home = home_serving(&["fx1", "fx2"]);
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}/mcp");

    // The shape Claude Desktop produces: two entries, both launched when the
    // app starts, both pointed at their own server's endpoint on one gateway.
    let one_args = ["--url", url.as_str(), "--server", "fx1"];
    let two_args = ["--url", url.as_str(), "--server", "fx2"];
    let (first, second) = tokio::join!(
        spawn_bridge(home.path(), &one_args),
        spawn_bridge(home.path(), &two_args),
    );
    let (one, one_child, one_said) = first;
    let (two, two_child, two_said) = second;

    assert_eq!(
        tool_names(&one.list_all_tools().await.unwrap()),
        ["echo", "reverse"]
    );
    assert_eq!(
        tool_names(&two.list_all_tools().await.unwrap()),
        ["echo", "reverse"]
    );

    // One of them bound the port and the other found it taken and waited.
    let started =
        |errors: &Arc<Mutex<String>>| said(errors).contains("serving one for this session");
    assert!(
        started(&one_said) ^ started(&two_said),
        "one: {}\ntwo: {}",
        said(&one_said),
        said(&two_said)
    );

    ends(one, one_child).await;
    ends(two, two_child).await;
}
