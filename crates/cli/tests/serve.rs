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
/// endpoint — which is the only place its tools are.
#[tokio::test]
async fn a_bare_serve_answers_on_the_per_server_endpoints() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, addr, endpoints) = serve(dir.path(), &[]).await;

    assert!(endpoints.contains("/s/fx1"), "{endpoints}");
    assert!(endpoints.contains("/s/fx2"), "{endpoints}");

    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx1")).await,
        ["echo", "reverse"]
    );
    // The base endpoint answers, and serves nothing: it is the gateway's own
    // address, not a way through it.
    assert!(
        tool_names(&format!("http://{addr}/mcp")).await.is_empty(),
        "the base endpoint served tools"
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

/// The other half of that: a file that lands at the path and does not run is
/// not something to stand aside for.
///
/// Overwriting a running Mach-O in place — a `cp -f` over the path, which no
/// packager does but a developer with a fresh build does — leaves a file
/// macOS kills on sight. A gateway that ended for one would be relaunched
/// into the same refusal by its supervisor for as long as anybody was
/// willing to watch it happen.
#[cfg(unix)]
#[tokio::test]
async fn a_supervised_gateway_stays_on_a_replacement_that_does_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let copy = util::binary_copy(dir.path());
    // Kept because the file at the path is about to stop being a binary, and
    // the second half of the test needs a working one to publish.
    let working = std::fs::read(&copy).unwrap();
    let state = dir.path().join("state");
    write_config(&dir.path().join("config.toml"), &fixture_config(&["fx1"]));
    let (mut child, addr, _, errors) =
        util::serve_binary(&copy, dir.path(), &["--supervised"]).await;
    let port = port_of(&addr);
    wait_for_record(&state, port).await;
    wait_for_stderr(&errors, "watching").await;

    util::publish_binary(&copy, b"not a binary");
    wait_for_stderr(&errors, "changed but does not run").await;

    assert!(
        child.try_wait().unwrap().is_none(),
        "the gateway stood aside for a file that cannot be executed"
    );
    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx1")).await,
        ["echo", "reverse"]
    );

    // Refusing one file is not giving up on the path: the next one is
    // verified afresh, and a working one still ends the gateway.
    let mut upgraded = working;
    upgraded.extend_from_slice(b"an upgrade");
    util::publish_binary(&copy, &upgraded);

    assert_eq!(
        wait_for_exit(&mut child, &errors).await,
        i32::from(mcpgw_core::upgrade::UPGRADE_EXIT)
    );
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
    // A real binary, because the watcher runs its replacement before it
    // stands aside for it. What this path stands for is a machine where the
    // service was installed from somewhere the running image is not.
    std::fs::copy(assert_cmd::cargo::cargo_bin("mcpgw"), &installed).unwrap();
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

/// A credential shaped like a real one, passed where a tool argument would
/// carry it.
const FAKE_TOKEN: &str = "ghp_0123456789abcdefghij";

/// Calls one tool through the binary's own stdio bridge, once the endpoint is
/// known to answer.
async fn call_tool(url: &str, tool: &str, message: &str) {
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"));
    // The bridge must not raise a gateway of its own here: the record this
    // call writes is the thing under test, and a second gateway would write
    // it somewhere else, under a policy nobody set.
    command.args(["connect", "--no-auto-start", "--url", url]);
    let (transport, _stderr) = TokioChildProcess::builder(command)
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let client = ().serve(transport).await.unwrap();
    let request = rmcp::model::CallToolRequestParams::new(tool.to_owned()).with_arguments(
        serde_json::json!({ "message": message })
            .as_object()
            .cloned()
            .unwrap(),
    );
    client.call_tool(request).await.unwrap();
    client.cancel().await.unwrap();
}

/// Everything the gateway captured under `home`, as the bytes on disk.
fn traffic(home: &Path) -> String {
    let dir = home.join("state").join("traffic");
    std::fs::read_dir(&dir)
        .unwrap_or_else(|err| panic!("no traffic under {}: {err}", dir.display()))
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect()
}

/// The flag is the surface, so the default it advertises is part of the
/// promise: capture redacts unless somebody chose otherwise.
#[test]
fn capture_bodies_defaults_to_redacted_and_names_its_modes() {
    let help = util::stdout(
        &util::mcpgw(Path::new("."))
            .args(["serve", "--help"])
            .output()
            .unwrap(),
    );
    assert!(help.contains("--capture-bodies"), "{help}");
    assert!(help.contains("[default: redacted]"), "{help}");
    assert!(help.contains("off, redacted, full"), "{help}");
}

#[test]
fn an_unknown_capture_bodies_mode_is_refused() {
    let output = util::mcpgw(Path::new("."))
        .args(["serve", "--capture-bodies", "verbatim"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    let said = util::stderr(&output);
    assert!(said.contains("verbatim"), "{said}");
    assert!(said.contains("redacted"), "{said}");
}

/// The default the flag advertises, end to end through the real binary: a
/// token handed to a tool is not in the file the gateway wrote.
#[tokio::test]
async fn a_captured_tool_argument_is_redacted_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, addr, _) = serve_capturing(dir.path(), &[]).await;

    let url = format!("http://{addr}/s/fx1");
    tool_names(&url).await;
    call_tool(&url, "echo", FAKE_TOKEN).await;
    child.kill().await.unwrap();

    let captured = traffic(dir.path());
    assert!(!captured.contains(FAKE_TOKEN), "{captured}");
    assert!(captured.contains("[redacted:ghp_…]"), "{captured}");
    // Still a log worth keeping: the call is named and timed.
    assert!(captured.contains(r#""tool":"echo""#), "{captured}");
}

/// `off` is the mode for people who want the timings and nothing else.
#[tokio::test]
async fn capture_bodies_off_records_metadata_and_no_bodies() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, addr, _) = serve_capturing(dir.path(), &["--capture-bodies", "off"]).await;

    let url = format!("http://{addr}/s/fx1");
    tool_names(&url).await;
    call_tool(&url, "echo", FAKE_TOKEN).await;
    child.kill().await.unwrap();

    let captured = traffic(dir.path());
    assert!(!captured.contains(FAKE_TOKEN), "{captured}");
    assert!(!captured.contains(r#""args""#), "{captured}");
    assert!(!captured.contains(r#""response""#), "{captured}");
    assert!(captured.contains(r#""bodies":"off""#), "{captured}");
    assert!(captured.contains(r#""tool":"echo""#), "{captured}");
}

/// A gateway that writes a traffic log, which every other test in this file
/// deliberately does not.
async fn serve_capturing(home: &Path, args: &[&str]) -> (tokio::process::Child, String, String) {
    write_config(&home.join("config.toml"), &fixture_config(&["fx1"]));
    util::serve_with(home, &[], args).await
}

/// The grace period, end to end: a gateway holds a token, a client arrives
/// without one, and the gateway answers it and says so exactly once. The line
/// is the whole of the warning anybody gets before the next release stops
/// answering, so it is worth a test that watches a real gateway print it.
#[tokio::test]
async fn a_client_without_the_token_is_answered_and_named_once() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    write_config(&home.join("config.toml"), &fixture_config(&["fx1"]));
    let (mut child, addr, _endpoints, errors) =
        util::serve_binary(&assert_cmd::cargo::cargo_bin("mcpgw"), home, &[]).await;

    // The gateway minted its token at startup and holds it in memory. Taking
    // the file away is how a bridge in the same sandbox becomes a client that
    // has not been re-synced.
    let state = home.join("state");
    std::fs::remove_file(mcpgw_core::gateway_token::GatewayToken::path(&state)).unwrap();

    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx1")).await,
        ["echo", "reverse"]
    );
    wait_for_stderr(&errors, "run mcpgw sync").await;

    // Once per process: a second client says nothing further.
    let before = errors.lock().unwrap().matches("run mcpgw sync").count();
    let _ = tool_names(&format!("http://{addr}/s/fx1")).await;
    assert_eq!(
        errors.lock().unwrap().matches("run mcpgw sync").count(),
        before
    );

    child.kill().await.unwrap();
}
