//! End-to-end coverage for `mcpgw serve`: the real binary serves the real
//! routes, reached through the `mcpgw connect` bridge so this test needs no
//! HTTP client of its own.
//!
//! Every wait here is on a condition rather than a clock, and the upgrade
//! tests raise `MCPGW_VERIFY_TIMEOUT_SECS` for the gateways they start:
//! this file is the one most sensitive to what else the machine is doing,
//! because a supervised gateway forks the replacement it is checking and
//! waits for it in wall-clock time. If something here starts failing only
//! under a full `cargo test --workspace` on a busy box, that is the shape of
//! it (#218), not the watcher logic.

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
async fn serve(home: &Path, args: &[&str]) -> (util::Spawned, String, String) {
    serve_config(home, &fixture_config(&["fx1", "fx2"]), args).await
}

async fn serve_config(home: &Path, text: &str, args: &[&str]) -> (util::Spawned, String, String) {
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
    let (_gateway, addr, endpoints) = serve(dir.path(), &[]).await;

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
}

/// The G2 promise, end to end through the real binary: `mcpgw add` while
/// `serve` is running is enough — no restart, no dropped clients.
#[tokio::test]
async fn a_server_added_to_the_config_is_served_without_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (_gateway, addr, endpoints) =
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
}

/// `--per-server` outlives the behaviour it used to gate, so scripts that
/// still pass it get exactly what a bare serve gets.
#[tokio::test]
async fn the_old_per_server_flag_is_still_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let (_gateway, addr, endpoints) = serve(dir.path(), &["--per-server"]).await;

    assert!(endpoints.contains("/s/fx1"), "{endpoints}");
    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx2")).await,
        ["echo", "reverse"]
    );
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
    let (child, addr, _) = serve(dir.path(), &[]).await;
    let port = port_of(&addr);

    let record = wait_for_record(&dir.path().join("state"), port).await;

    assert_eq!(record.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(record.pid, child.id().unwrap());
    assert_eq!(record.port, port);
    assert!(record.exe.exists(), "{}", record.exe.display());
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
    assert_eq!(child.wait().await.unwrap().code(), Some(0));

    assert_eq!(
        mcpgw_core::runtime::read_record(&state, port).unwrap(),
        None
    );
}

/// The same shutdown, reached the way a supervisor reaches it. `launchctl
/// bootout`, `systemctl stop` and a bare `kill` all send SIGTERM, so an
/// ordinary stop of the service must not be the path a crash takes.
#[cfg(unix)]
#[tokio::test]
async fn a_supervisor_stop_shuts_down_as_cleanly_as_ctrl_c() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    let (mut child, addr, _) = serve(dir.path(), &[]).await;
    let port = port_of(&addr);
    wait_for_record(&state, port).await;

    let killed = std::process::Command::new("kill")
        .args(["-TERM", &child.id().unwrap().to_string()])
        .status()
        .unwrap();
    assert!(killed.success(), "{killed}");
    let status = tokio::time::timeout(READY_DEADLINE, child.wait())
        .await
        .expect("the gateway never ended after SIGTERM")
        .unwrap();
    assert_eq!(status.code(), Some(0), "{status}");

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
async fn wait_for_exit(child: &mut util::Spawned, errors: &std::sync::Mutex<String>) -> i32 {
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

    // The one upgrade test that publishes a real mcpgw: what the gateway
    // stands aside for here is the file a `brew upgrade` leaves behind, and
    // the record below is asserted against its length. The rest of them are
    // about which line the watcher prints and use a stub.
    util::replace_binary(&copy);

    // Waited for before the exit, so a gateway that is merely slow says so
    // in the message rather than looking like one that hung.
    wait_for_stderr(&errors, "changed; restarting so the service runs it").await;
    assert_eq!(
        wait_for_exit(&mut child, &errors).await,
        i32::from(mcpgw_core::upgrade::UPGRADE_EXIT)
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
    util::publish_stub(&copy, "the upgrade after the broken one");

    wait_for_stderr(&errors, "changed; restarting so the service runs it").await;
    assert_eq!(
        wait_for_exit(&mut child, &errors).await,
        i32::from(mcpgw_core::upgrade::UPGRADE_EXIT)
    );
}

/// How long the pre-flight check waits is the machine's business, not a
/// constant compiled into the binary: on a loaded box the fork alone can eat
/// the five seconds the default allows, and a good upgrade is then reported
/// as one that does not run.
///
/// Driven the short way round, because a deadline is far easier to prove by
/// making it too small than by making it large enough: one second, against a
/// replacement that takes three to answer.
#[cfg(unix)]
#[tokio::test]
async fn the_verify_timeout_is_what_the_environment_says_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let copy = util::binary_copy(dir.path());
    write_config(&dir.path().join("config.toml"), &fixture_config(&["fx1"]));
    let (mut child, addr, _, errors) = util::serve_binary_with_env(
        &copy,
        dir.path(),
        &["--supervised"],
        &[(mcpgw_core::upgrade::VERIFY_TIMEOUT_ENV, "1")],
    )
    .await;
    wait_for_stderr(&errors, "watching").await;

    // Answers exactly what the watcher asks for, only later than it is
    // willing to wait.
    util::publish_binary(&copy, b"#!/bin/sh\nsleep 3\necho \"mcpgw 0.0.0-stub\"\n");

    wait_for_stderr(&errors, "did not answer --version within 1s").await;
    assert!(
        child.try_wait().unwrap().is_none(),
        "the gateway stood aside for a replacement it never got an answer out of"
    );
    assert_eq!(
        tool_names(&format!("http://{addr}/s/fx1")).await,
        ["echo", "reverse"]
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
}

/// Which binary is watched is not "the one this process is running": it is
/// the one the supervisor will relaunch, which the installed spec names. The
/// two differ exactly when it matters — a service installed against
/// `/opt/homebrew/bin/mcpgw` runs a Cellar file no upgrade ever touches.
#[tokio::test]
async fn a_supervised_gateway_watches_the_binary_its_service_was_installed_with() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().join("state");
    // A file that runs, because the watcher runs the replacement before it
    // stands aside for it — but never a gateway: what this path stands for
    // is a machine where the service was installed from somewhere the
    // running image is not, and nothing here ever starts it.
    let installed = util::stub_binary(&dir.path().join("installed-mcpgw"), "as installed");
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

    util::publish_stub(&installed, "the upgrade the service will run");

    wait_for_stderr(&errors, "changed; restarting so the service runs it").await;
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
    let (child, addr, _) = serve_capturing(dir.path(), &[]).await;

    let url = format!("http://{addr}/s/fx1");
    tool_names(&url).await;
    call_tool(&url, "echo", FAKE_TOKEN).await;
    child.stop().await;

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
    let (child, addr, _) = serve_capturing(dir.path(), &["--capture-bodies", "off"]).await;

    let url = format!("http://{addr}/s/fx1");
    tool_names(&url).await;
    call_tool(&url, "echo", FAKE_TOKEN).await;
    child.stop().await;

    let captured = traffic(dir.path());
    assert!(!captured.contains(FAKE_TOKEN), "{captured}");
    assert!(!captured.contains(r#""args""#), "{captured}");
    assert!(!captured.contains(r#""response""#), "{captured}");
    assert!(captured.contains(r#""bodies":"off""#), "{captured}");
    assert!(captured.contains(r#""tool":"echo""#), "{captured}");
}

/// Since #221 an accepted record is a queued record: the client has its
/// answer while the line is still in memory. What makes that safe is the
/// flush on the way out, so a gateway asked to stop has to leave the record
/// behind on disk.
///
/// The writer is told to hold everything until that flush, which is the
/// state this is about — otherwise the test only asserts that a background
/// thread usually wins a race.
#[cfg(unix)]
#[tokio::test]
async fn a_record_accepted_just_before_a_graceful_stop_is_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    write_config(&home.join("config.toml"), &fixture_config(&["fx1"]));
    let (child, addr, _, _errors) = util::serve_binary_capturing(
        &assert_cmd::cargo::cargo_bin("mcpgw"),
        home,
        &[],
        &[(mcpgw_core::capture::HOLD_UNTIL_FLUSH_ENV, "1")],
    )
    .await;
    let state = home.join("state");
    let port = port_of(&addr);
    wait_for_record(&state, port).await;

    call_tool(&format!("http://{addr}/s/fx1"), "echo", "the last call").await;
    child.stop().await;

    // The withdrawn record is what says the stop went through the shutdown
    // path at all: a hard kill leaves it, and skips the flush with it.
    assert_eq!(
        mcpgw_core::runtime::read_record(&state, port).unwrap(),
        None
    );
    let captured = traffic(home);
    assert!(captured.contains(r#""tool":"echo""#), "{captured}");
}

/// The same guarantee on the exit nobody sends a signal for: a supervised
/// gateway standing aside for a replaced binary ends itself, and the traffic
/// it has already answered for has to survive that too.
#[cfg(unix)]
#[tokio::test]
async fn a_record_accepted_just_before_a_stand_aside_is_on_disk() {
    let dir = tempfile::tempdir().unwrap();
    let copy = util::binary_copy(dir.path());
    let state = dir.path().join("state");
    write_config(&dir.path().join("config.toml"), &fixture_config(&["fx1"]));
    let (mut child, addr, _, errors) = util::serve_binary_capturing(
        &copy,
        dir.path(),
        &["--supervised"],
        &[(mcpgw_core::capture::HOLD_UNTIL_FLUSH_ENV, "1")],
    )
    .await;
    let port = port_of(&addr);
    wait_for_record(&state, port).await;
    wait_for_stderr(&errors, "watching").await;

    call_tool(&format!("http://{addr}/s/fx1"), "echo", "the last call").await;
    util::publish_stub(&copy, "the upgrade this gateway stands aside for");

    wait_for_stderr(&errors, "changed; restarting so the service runs it").await;
    assert_eq!(
        wait_for_exit(&mut child, &errors).await,
        i32::from(mcpgw_core::upgrade::UPGRADE_EXIT)
    );

    let captured = traffic(dir.path());
    assert!(captured.contains(r#""tool":"echo""#), "{captured}");
}

/// A gateway that writes a traffic log, which every other test in this file
/// deliberately does not.
async fn serve_capturing(home: &Path, args: &[&str]) -> (util::Spawned, String, String) {
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
    let (_gateway, addr, _endpoints, errors) =
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
}

/// The harness's own promise: a gateway spawned by a test that then fails is
/// killed anyway.
///
/// The whole point of the guard — the pattern it replaced killed the child on
/// the last line of the test body, which a panic never reaches, so one failed
/// assertion left a gateway holding a port (and its stdio fixture servers
/// under it) for the rest of the run. The panic is raised inside a task so it
/// is a real unwind rather than a scope ending, and the port coming back is
/// what proves the process is gone rather than merely unreachable.
#[cfg(unix)]
#[tokio::test]
async fn a_test_that_panics_does_not_leave_its_gateway_running() {
    let dir = tempfile::tempdir().unwrap();
    write_config(&dir.path().join("config.toml"), &fixture_config(&["fx1"]));
    let home = dir.path().to_owned();

    let (sender, receiver) = tokio::sync::oneshot::channel();
    let failed = tokio::spawn(async move {
        let (_gateway, addr, _endpoints) = util::serve(&home, &[]).await;
        sender.send(port_of(&addr)).unwrap();
        panic!("the failed assertion this test is standing in for");
    });

    let port = receiver.await.unwrap();
    assert!(failed.await.is_err(), "the task was supposed to panic");

    let deadline = std::time::Instant::now() + READY_DEADLINE;
    while std::net::TcpListener::bind(("127.0.0.1", port)).is_err() {
        assert!(
            std::time::Instant::now() < deadline,
            "the gateway on port {port} outlived the test that panicked"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}
