//! End-to-end coverage for `mcpgw serve`: the real binary serves the real
//! routes, reached through the `mcpgw connect` bridge so this test needs no
//! HTTP client of its own.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;

mod util;
use util::fixture_config;

/// How long the gateway has to answer on an endpoint. It covers both the
/// listener becoming ready after the banner and, for the reload test, the
/// two-second config poll noticing an edit.
const READY_DEADLINE: Duration = Duration::from_secs(90);

const POLL: Duration = Duration::from_millis(250);

/// Writes the config the way `mcpgw add` does: a temp file renamed over the
/// target. The rename replaces the inode, which is precisely what the
/// reload's poll has to see through.
fn write_config(path: &Path, text: &str) {
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text).unwrap();
    std::fs::rename(&temp, path).unwrap();
}

/// A gateway on an ephemeral port, returned with its address and the banner
/// line that lists the per-server endpoints.
async fn serve(home: &Path, args: &[&str]) -> (tokio::process::Child, String, String) {
    serve_config(home, &fixture_config(&["fx1", "fx2"]), args).await
}

async fn serve_config(
    home: &Path,
    text: &str,
    args: &[&str],
) -> (tokio::process::Child, String, String) {
    write_config(&home.join("config.toml"), text);
    util::serve(home, args).await
}

/// Bridges to `url` with the binary's own stdio bridge and lists its tools.
///
/// Polled to a deadline rather than attempted once: the banner is printed
/// around the bind, so the first connect can arrive before the listener is
/// accepting — and after a config edit the endpoint only appears once the
/// gateway's poll has got round to the file.
async fn tool_names(url: &str) -> Vec<String> {
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        match try_tool_names(url).await {
            Ok(names) => return names,
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "{url} never answered within {READY_DEADLINE:?}: {err:#}"
                );
                tokio::time::sleep(POLL).await;
            }
        }
    }
}

/// The same, for the paths where not answering yet is an expected state.
async fn try_tool_names(url: &str) -> anyhow::Result<Vec<String>> {
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"));
    command.args(["connect", "--url", url]);
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()?;
    let client = ().serve(transport).await?;
    let tools = client.list_all_tools().await;
    client.cancel().await?;
    Ok(tools?.iter().map(|t| t.name.to_string()).collect())
}

/// The G1 promise: no flag, and every served server already has its own
/// endpoint beside the aggregate.
#[tokio::test]
async fn a_bare_serve_answers_on_the_per_server_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, addr, endpoints) = serve(dir.path(), &[]).await;

    assert!(endpoints.contains("/s/fx1"), "{endpoints}");
    assert!(endpoints.contains("/s/fx2"), "{endpoints}");

    // Unprefixed on the endpoint, namespaced on the aggregate: two faces of
    // the same running gateway.
    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx1")).await,
        ["echo", "reverse"]
    );
    assert_eq!(
        tool_names(&format!("http://{addr}/mcp")).await,
        ["fx1__echo", "fx1__reverse", "fx2__echo", "fx2__reverse"]
    );

    child.kill().await.unwrap();
}

/// The G2 promise, end to end through the real binary: `mcpgw add` while
/// `serve` is running is enough — no restart, no dropped clients.
#[tokio::test]
async fn a_server_added_to_the_config_is_served_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, addr, endpoints) =
        serve_config(dir.path(), &fixture_config(&["fx1"]), &[]).await;
    assert!(!endpoints.contains("/s/fx2"), "{endpoints}");

    write_config(
        &dir.path().join("config.toml"),
        &fixture_config(&["fx1", "fx2"]),
    );

    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx2")).await,
        ["echo", "reverse"]
    );

    // The server that was there all along is still served, by the same
    // gateway process.
    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx1")).await,
        ["echo", "reverse"]
    );
    assert_eq!(
        tool_names(&format!("http://{addr}/mcp")).await,
        ["fx1__echo", "fx1__reverse", "fx2__echo", "fx2__reverse"]
    );

    child.kill().await.unwrap();
}

/// `--per-server` outlives the behaviour it used to gate, so scripts that
/// still pass it get exactly what a bare serve gets.
#[tokio::test]
async fn the_old_per_server_flag_is_still_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, addr, endpoints) = serve(dir.path(), &["--per-server"]).await;

    assert!(endpoints.contains("/s/fx1"), "{endpoints}");
    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx2")).await,
        ["echo", "reverse"]
    );

    child.kill().await.unwrap();
}

/// Port off the banner address, which is where the record's name comes from.
fn port_of(addr: &str) -> u16 {
    addr.rsplit(':')
        .next()
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("no port in {addr}"))
}

/// The record lands around the bind, so the banner is not a guarantee it is
/// already there — polled to the same deadline the endpoints get.
async fn wait_for_record(state: &Path, port: u16) -> mcpgw_core::runtime::GatewayRecord {
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        if let Some(record) = mcpgw_core::runtime::read_record(state, port).unwrap() {
            return record;
        }
        assert!(
            Instant::now() < deadline,
            "no gateway record for port {port} within {READY_DEADLINE:?}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// An upgrade leaves the running service on the old binary, and nothing on
/// the wire says so — the gateway has to state its own version, pid and
/// executable while it is up.
#[tokio::test]
async fn a_running_gateway_records_what_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, addr, _) = serve(dir.path(), &[]).await;
    let port = port_of(&addr);

    let record = wait_for_record(&dir.path().join("state"), port).await;

    assert_eq!(record.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(record.pid, child.id().unwrap());
    assert_eq!(record.port, port);
    assert!(record.exe.exists(), "{}", record.exe.display());

    child.kill().await.unwrap();
}

/// A gateway that shut down cleanly must not leave a record claiming it is
/// still up. (A crash does, which is why readers probe the port as well.)
#[cfg(unix)]
#[tokio::test]
async fn a_clean_shutdown_withdraws_the_record() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (mut child, addr, _) = serve(dir.path(), &[]).await;
    let port = port_of(&addr);
    wait_for_record(&state, port).await;

    // SIGINT rather than the `kill` the other tests use: this is the path
    // Ctrl-C takes, and the only one that gets to clean up after itself.
    let killed = std::process::Command::new("kill")
        .args(["-INT", &child.id().unwrap().to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "{killed}");
    child.wait().await.unwrap();

    assert_eq!(
        mcpgw_core::runtime::read_record(&state, port).unwrap(),
        None
    );
}
