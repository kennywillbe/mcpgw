//! End-to-end coverage for `mcpgw serve`: the real binary serves the real
//! routes, reached through the `mcpgw connect` bridge so this test needs no
//! HTTP client of its own.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use rmcp::ServiceExt as _;
use rmcp::transport::TokioChildProcess;
use tokio::io::{AsyncBufReadExt as _, BufReader};

mod util;
use util::fixture_binary;

/// How long a banner line may take to arrive. Generous because it covers a
/// cold process start on a runner that is compiling and testing everything
/// else at the same time.
const BANNER_DEADLINE: Duration = Duration::from_secs(60);

/// How long the gateway has to answer on an endpoint. It covers both the
/// listener becoming ready after the banner and, for the reload test, the
/// two-second config poll noticing an edit.
const READY_DEADLINE: Duration = Duration::from_secs(90);

const POLL: Duration = Duration::from_millis(250);

/// A config with one healthy fixture server per name.
fn config(names: &[&str]) -> String {
    use std::fmt::Write as _;

    let fixture = fixture_binary();
    let mut text = "version = 1\n".to_owned();
    for name in names {
        let _ = write!(
            text,
            "\n[servers.{name}]\ntype = \"stdio\"\ncommand = '{}'\nargs = [\"healthy\"]\n",
            fixture.display()
        );
    }
    text
}

/// Writes the config the way `mcpgw add` does: a temp file renamed over the
/// target. The rename replaces the inode, which is precisely what the
/// reload's poll has to see through.
fn write_config(path: &Path, text: &str) {
    let temp = path.with_extension("toml.tmp");
    std::fs::write(&temp, text).unwrap();
    std::fs::rename(&temp, path).unwrap();
}

/// Spawns a gateway on an ephemeral port and returns it with its banner —
/// the banner is where the actual port is announced, so the test reads it
/// rather than guessing a number another test could be holding.
async fn serve(home: &Path, args: &[&str]) -> (tokio::process::Child, String, String) {
    serve_config(home, &config(&["fx1", "fx2"]), args).await
}

async fn serve_config(
    home: &Path,
    text: &str,
    args: &[&str],
) -> (tokio::process::Child, String, String) {
    let config_path = home.join("config.toml");
    write_config(&config_path, text);
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
    let listening = banner_line(&mut lines).await;
    let endpoints = banner_line(&mut lines).await;
    let addr = listening
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split("/mcp").next())
        .unwrap_or_else(|| panic!("no address in banner: {listening}"))
        .to_owned();
    // Kept draining for the life of the gateway. Dropping the read end here
    // would leave the child writing into a closed pipe, and a `println!` that
    // hits EPIPE panics — which killed the gateway mid-test whenever the
    // third banner line happened to land after this function returned.
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    (child, addr, endpoints)
}

/// One line of the startup banner, waited for to a deadline.
///
/// The child has to spawn, bind and flush before its first line lands, and on
/// a loaded runner none of that is instant. A deadline turns "slower than the
/// test expected" into "waited a minute", and only a genuinely wedged gateway
/// still fails — with a message that says so.
async fn banner_line(
    lines: &mut tokio::io::Lines<BufReader<tokio::process::ChildStdout>>,
) -> String {
    tokio::time::timeout(BANNER_DEADLINE, lines.next_line())
        .await
        .expect("the gateway printed no banner line before the deadline")
        .expect("reading the gateway banner")
        .expect("the gateway closed stdout before finishing its banner")
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
    let (mut child, addr, endpoints) = serve_config(dir.path(), &config(&["fx1"]), &[]).await;
    assert!(!endpoints.contains("/s/fx2"), "{endpoints}");

    write_config(&dir.path().join("config.toml"), &config(&["fx1", "fx2"]));

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
