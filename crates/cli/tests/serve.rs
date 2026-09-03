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

/// Waits for `needle` to show up on the gateway's stderr.
///
/// The line the exe watcher prints when it starts is the only signal that it
/// has taken its baseline stamp — and a binary replaced before that baseline
/// is simply the binary the gateway started with, which is not the situation
/// any of these tests is about.
async fn wait_for_stderr(errors: &std::sync::Mutex<String>, needle: &str) {
    let deadline = Instant::now() + READY_DEADLINE;
    loop {
        let said = errors.lock().unwrap().clone();
        if said.contains(needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the gateway never said {needle:?} within {READY_DEADLINE:?}: {said}"
        );
        tokio::time::sleep(POLL).await;
    }
}

/// Waits for the gateway to end, and says which line it ended on.
async fn wait_for_exit(
    child: &mut tokio::process::Child,
    errors: &std::sync::Mutex<String>,
) -> i32 {
    let ended = tokio::time::timeout(READY_DEADLINE, child.wait()).await;
    let status = ended.unwrap_or_else(|_| {
        panic!(
            "the gateway was still running after {READY_DEADLINE:?}: {}",
            errors.lock().unwrap()
        )
    });
    status.unwrap().code().expect("the gateway was signalled")
}

/// The whole point of the flag: an upgrade lands on disk, and the gateway
/// gets out of the way with a status its supervisor restarts on.
///
/// Unix only, because the running image is what makes it safe to write over
/// the file at all — Windows locks a binary that is executing, which is also
/// why nothing there can produce this situation in the first place.
#[cfg(unix)]
#[tokio::test]
async fn a_supervised_gateway_stands_aside_when_its_binary_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let copy = util::binary_copy(dir.path());
    let state = dir.path().join("state");
    write_config(&dir.path().join("config.toml"), &fixture_config(&["fx1"]));
    let (mut child, addr, _, errors) =
        util::serve_binary(&copy, dir.path(), &["--supervised"]).await;
    let port = port_of(&addr);
    wait_for_record(&state, port).await;
    wait_for_stderr(&errors, "watching").await;

    util::replace_binary(&copy);

    assert_eq!(
        wait_for_exit(&mut child, &errors).await,
        i32::from(mcpgw_core::upgrade::UPGRADE_EXIT)
    );
    let said = errors.lock().unwrap().clone();
    assert!(
        said.contains("changed; restarting so the service runs it"),
        "{said}"
    );

    // Left behind rather than withdrawn: the gateway the supervisor starts
    // next reads it to find out which binary it has already stood aside for,
    // which is the only thing between a bad upgrade and a restart loop.
    let record = mcpgw_core::runtime::read_record(&state, port)
        .unwrap()
        .expect("the record has to survive an upgrade restart");
    let restart = record
        .last_upgrade_restart
        .expect("the restart has to be recorded for the process that replaces this one");
    assert_eq!(restart.stamp.len, std::fs::metadata(&copy).unwrap().len());
}

/// The flag is the whole gate. A gateway somebody is running in a terminal
/// must not disappear because a `cargo build` finished in another one.
#[cfg(unix)]
#[tokio::test]
async fn a_gateway_without_the_flag_serves_straight_through_a_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let copy = util::binary_copy(dir.path());
    write_config(&dir.path().join("config.toml"), &fixture_config(&["fx1"]));
    let (mut child, addr, _, _errors) = util::serve_binary(&copy, dir.path(), &[]).await;

    util::replace_binary(&copy);
    // The one fixed wait in this file, because the assertion is that nothing
    // happens: there is no event to poll for, and a gateway that was going
    // to react to the new binary would have done it within three polls.
    tokio::time::sleep(3 * mcpgw_core::upgrade::POLL_INTERVAL).await;

    assert!(
        child.try_wait().unwrap().is_none(),
        "the gateway exited without being asked to supervise itself"
    );
    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx1")).await,
        ["echo", "reverse"]
    );

    child.kill().await.unwrap();
}

/// Which binary is watched is not "the one this process is running": it is
/// the one the supervisor will relaunch, which the installed spec names. The
/// two differ exactly when it matters — a service installed against
/// `/opt/homebrew/bin/mcpgw` runs a Cellar file no upgrade ever touches.
#[tokio::test]
async fn a_supervised_gateway_watches_the_binary_its_service_was_installed_with() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let installed = dir.path().join("installed-mcpgw");
    // Never executed, only stat-ed: this stands for the path on a machine
    // where the service was installed from somewhere the running image is
    // not.
    std::fs::write(&installed, b"the installed binary").unwrap();
    let spec = mcpgw_core::daemon::DaemonSpec {
        exe: installed.clone(),
        config_path: dir.path().join("config.toml"),
        state_dir: state.clone(),
        bind: "127.0.0.1".to_owned(),
        port: 8137,
        logs: mcpgw_core::daemon::LogPaths::under_state_dir(&state),
    };
    mcpgw_core::daemon::save_spec(&spec).unwrap();
    write_config(&dir.path().join("config.toml"), &fixture_config(&["fx1"]));

    let (mut child, _, _, errors) = util::serve_binary(
        &assert_cmd::cargo::cargo_bin("mcpgw"),
        dir.path(),
        &["--supervised"],
    )
    .await;
    wait_for_stderr(&errors, &installed.display().to_string()).await;

    util::replace_binary(&installed);

    assert_eq!(
        wait_for_exit(&mut child, &errors).await,
        i32::from(mcpgw_core::upgrade::UPGRADE_EXIT)
    );
    let said = errors.lock().unwrap().clone();
    assert!(said.contains(&installed.display().to_string()), "{said}");
}
