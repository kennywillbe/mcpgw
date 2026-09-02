//! End-to-end coverage for `mcpgw serve`: the real binary serves the real
//! routes, reached through the `mcpgw connect` bridge so this test needs no
//! HTTP client of its own.

use std::path::Path;
use std::process::Stdio;

use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;
use tokio::io::{AsyncBufReadExt as _, BufReader};

mod util;
use util::fixture_binary;

fn config() -> String {
    let fixture = fixture_binary();
    format!(
        r#"
version = 1

[servers.fx1]
type = "stdio"
command = '{0}'
args = ["healthy"]

[servers.fx2]
type = "stdio"
command = '{0}'
args = ["healthy"]
"#,
        fixture.display()
    )
}

/// Spawns a gateway on an ephemeral port and returns it with its banner —
/// the banner is where the actual port is announced, so the test reads it
/// rather than guessing a number another test could be holding.
async fn serve(home: &Path, args: &[&str]) -> (tokio::process::Child, String, String) {
    let config_path = home.join("config.toml");
    std::fs::write(&config_path, config()).unwrap();
    let mut child = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"))
        .arg("serve")
        .args(["--port", "0", "--no-capture"])
        .args(args)
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", &config_path)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_CONFIG_HOME")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let listening = lines.next_line().await.unwrap().unwrap();
    let endpoints = lines.next_line().await.unwrap().unwrap();
    let addr = listening
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split("/mcp").next())
        .unwrap_or_else(|| panic!("no address in banner: {listening}"))
        .to_owned();
    (child, addr, endpoints)
}

/// Bridges to `url` with the binary's own stdio bridge and lists its tools.
async fn tool_names(url: &str) -> Vec<String> {
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"));
    command.args(["connect", "--url", url]);
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let client = ().serve(transport).await.unwrap();
    let tools = client.list_all_tools().await.unwrap();
    let names = tools.iter().map(|t| t.name.to_string()).collect();
    client.cancel().await.unwrap();
    names
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
