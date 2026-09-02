//! End-to-end coverage for the first-run wizard: what a bare `mcpgw` does,
//! what `mcpgw init --yes` does, and what a machine that is already set up
//! is told instead.

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

mod util;
use util::fixture_binary;

/// How long a wizard run is given before the test calls it hung. Generous:
/// the only thing this bounds is a genuine deadlock on stdin, and a loaded
/// CI runner is slow, not stuck.
const DEADLINE: Duration = Duration::from_secs(90);

/// A config with one healthy fixture server.
fn config() -> String {
    format!(
        "version = 1\n\n[servers.fx1]\ntype = \"stdio\"\ncommand = '{}'\nargs = [\"healthy\"]\n",
        fixture_binary().display()
    )
}

/// A `mcpgw` invocation pointed at `home` and nothing of the real machine:
/// its own config, its own state directory, and no XDG override leaking in
/// from the environment the test itself was started in.
fn command(home: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"));
    command
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", home.join("config.toml"))
        .env("MCPGW_STATE_DIR", home.join("state"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env("APPDATA", home.join("AppData"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME");
    command
}

/// Waits for `child` to exit, polling to a deadline rather than blocking:
/// the point of these tests is that the wizard never waits for stdin, and a
/// blocking wait would express that as a hung suite instead of a failure.
async fn finish(mut child: tokio::process::Child, what: &str) -> std::process::Output {
    let deadline = Instant::now() + DEADLINE;
    loop {
        if let Some(_status) = child.try_wait().unwrap() {
            return child.wait_with_output().await.unwrap();
        }
        if Instant::now() >= deadline {
            child.kill().await.unwrap();
            panic!("{what} never exited — it is waiting for something");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Off a terminal there is nobody to answer the wizard's questions, so a
/// bare `mcpgw` is the same missing-subcommand failure it has always been.
#[test]
fn a_bare_run_off_a_terminal_prints_help_and_exits_2() {
    let dir = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"))
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", dir.path().join("config.toml"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
        .wait_with_output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("Usage: mcpgw"), "{stderr}");
    assert!(stderr.contains("Commands:"), "{stderr}");
    // The wizard's own opening must not appear on a path that cannot ask.
    assert!(!stderr.contains("let's get your MCP servers"), "{stderr}");
}

/// `--yes` walks the whole wizard with its stdin held open and never
/// written to: if any step reached for a line, this test would hang.
#[tokio::test]
async fn init_yes_walks_every_step_without_reading_stdin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();

    // A port held open by a socket that will never answer HTTP, so the
    // daemon step reports "not running" for a reason the test owns. Pointing
    // at the default 127.0.0.1:8137 instead would read whatever gateway the
    // developer or the runner happens to have up.
    let blocked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", blocked.local_addr().unwrap());

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes`").await;

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("let's get your MCP servers running through one gateway"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Nothing is written until you say yes"),
        "{stdout}"
    );
    // The config already has a server, so the survey is skipped.
    assert!(stdout.contains("skipping the survey"), "{stdout}");
    // The daemon step announces itself and then declines to fight for a port
    // somebody else holds — or, on a platform whose installer has not shipped,
    // says so. Both end at the same offer, which is the assertion that holds
    // on every runner in the matrix.
    assert!(stdout.contains("keep the gateway running"), "{stdout}");
    assert!(stdout.contains("mcpgw serve"), "{stdout}");
    assert!(!stdout.contains("installed at"), "{stdout}");
    // No client is installed under this sandbox home, so the sync step has
    // nowhere to push and writes nothing — but it still closes the wizard.
    assert!(stdout.contains("no MCP client here"), "{stdout}");
    assert!(stdout.contains("Restart your clients"), "{stdout}");
    assert!(!dir.path().join("state").join("managed.json").exists());
}

/// Every step done — servers configured, a gateway answering, a client
/// already synced — and the wizard has nothing to walk anyone through.
#[tokio::test]
async fn an_already_finished_machine_gets_the_status_card() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();
    std::fs::create_dir_all(dir.path().join("state")).unwrap();
    std::fs::write(
        dir.path().join("state").join("managed.json"),
        r#"{"clients":{"cursor":["fx1"]}}"#,
    )
    .unwrap();

    // Port 0 and the banner rather than a number of our own: a fixed port
    // is a race against every other test in the suite (#54, #83).
    let mut gateway = command(dir.path())
        .args(["serve", "--port", "0", "--no-capture"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let url = gateway_url(&mut gateway).await;

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes` against a live gateway").await;
    gateway.kill().await.unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("everything is set up"), "{stdout}");
    assert!(!stdout.contains("let's get your MCP servers"), "{stdout}");
    assert!(stdout.contains("1 configured, 1 enabled"), "{stdout}");
    // One per-server endpoint plus the aggregate.
    assert!(stdout.contains("2 endpoints"), "{stdout}");
    assert!(stdout.contains("Cursor"), "{stdout}");
    for suggestion in ["mcpgw list", "mcpgw watch", "mcpgw doctor --probe"] {
        assert!(stdout.contains(suggestion), "{stdout}");
    }
}

/// A gateway that is already answering is one the wizard has nothing to add
/// to: the daemon step does not run, and `--yes` does not turn "already
/// working" into a login item nobody asked for.
#[tokio::test]
async fn a_running_gateway_is_left_alone_and_no_service_is_installed() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();

    // No `managed.json`, so the last step still has something to say and the
    // wizard walks its steps rather than printing the status card.
    let mut gateway = command(dir.path())
        .args(["serve", "--port", "0", "--no-capture"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let url = gateway_url(&mut gateway).await;

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes` beside a running gateway").await;
    gateway.kill().await.unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("the gateway is already answering"),
        "{stdout}"
    );
    // Not even the offer: the step is skipped outright.
    assert!(!stdout.contains("keep the gateway running"), "{stdout}");
    assert_no_service(dir.path());
}

/// "No" to the login service is an answer, not an error: the step prints the
/// alternative and the wizard carries on.
#[tokio::test]
async fn declining_the_service_prints_the_alternative_and_is_not_a_failure() {
    use tokio::io::AsyncWriteExt as _;

    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();

    // An address the OS handed out and nothing holds, so the step gets as far
    // as its offer on a platform whose installer has shipped. Asked for rather
    // than picked: a fixed port is a race against the rest of the suite
    // (#54, #83).
    let free = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let url = format!("http://{free}/mcp");

    let mut child = command(dir.path())
        .args(["init", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // More noes than there are questions, deliberately: an exhausted stdin
    // takes the recommended answer, and the recommended answer here installs
    // a real login service on the machine running the tests.
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all("n\n".repeat(8).as_bytes()).await.unwrap();
    drop(stdin);
    let output = finish(child, "`mcpgw init` answered with no").await;

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("keep the gateway running"), "{stdout}");
    // Whichever way the step ended — declined, or a platform that cannot
    // install one — the user leaves knowing how to run the gateway anyway.
    assert!(stdout.contains("mcpgw serve"), "{stdout}");
    assert!(!stdout.contains("installed at"), "{stdout}");
    assert_no_service(dir.path());
}

/// No test may leave a real login service behind, so every run that could
/// have installed one checks the sandbox home it would have gone into.
/// Installing for real is the platform milestone's own env-gated test.
fn assert_no_service(home: &Path) {
    for candidate in [
        home.join("Library").join("LaunchAgents"),
        home.join(".config").join("systemd"),
    ] {
        assert!(!candidate.exists(), "{} was written", candidate.display());
    }
}

/// Reads the served address out of the gateway's own banner and keeps
/// draining its stdout, so a later banner line cannot hit a closed pipe.
async fn gateway_url(child: &mut tokio::process::Child) -> String {
    use tokio::io::{AsyncBufReadExt as _, BufReader};

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let banner = lines.next_line().await.unwrap().unwrap();
    let url = banner
        .split_whitespace()
        .find(|word| word.starts_with("http://"))
        .unwrap_or_else(|| panic!("no address in banner: {banner}"))
        .to_owned();
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    url
}

/// A Cursor config with an entry mcpgw did not write. It has to survive the
/// wizard untouched and be named as somebody else's on the way past.
const CURSOR_WITH_A_HAND_MADE_ENTRY: &str = r#"{
  "mcpServers": {
    "notes": { "command": "notes-mcp" }
  }
}"#;

/// Mirrors `ClientKind::config_path` for Claude Desktop under the sandbox
/// environment [`command`] builds.
fn claude_desktop_config(home: &Path) -> std::path::PathBuf {
    let app_data = if cfg!(target_os = "macos") {
        home.join("Library/Application Support")
    } else if cfg!(windows) {
        home.join("AppData")
    } else {
        home.join(".config")
    };
    app_data.join("Claude/claude_desktop_config.json")
}

/// Two clients on the machine: Cursor, which holds http entries and already
/// has a hand-made one, and Claude Desktop, which cannot and gets the stdio
/// bridge. Claude Desktop is installed but unconfigured, so the wizard has to
/// create its file as well as write into one.
fn install_two_clients(home: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let cursor = home.join(".cursor/mcp.json");
    std::fs::create_dir_all(cursor.parent().unwrap()).unwrap();
    std::fs::write(&cursor, CURSOR_WITH_A_HAND_MADE_ENTRY).unwrap();

    let claude = claude_desktop_config(home);
    std::fs::create_dir_all(claude.parent().unwrap()).unwrap();
    (cursor, claude)
}

fn json_at(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

/// The whole point of the step, end to end: every client ends up pointing at
/// the gateway by the server's own name, and the wizard proves it by dialing
/// the endpoint the clients were just told to use.
#[tokio::test]
async fn the_sync_step_points_every_client_at_a_live_gateway_and_checks_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();
    let (cursor, claude) = install_two_clients(dir.path());

    let mut gateway = command(dir.path())
        .args(["serve", "--port", "0", "--no-capture"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let url = gateway_url(&mut gateway).await;

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes` with two clients installed").await;
    gateway.kill().await.unwrap();

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();

    // The plan, per client, by name — and the entry that is not mcpgw's.
    assert!(
        stdout.contains("Pointing your clients at the gateway"),
        "{stdout}"
    );
    assert!(stdout.contains("+ fx1"), "{stdout}");
    assert!(stdout.contains("Claude Desktop"), "{stdout}");
    assert!(
        stdout.contains("notes (not mine — left untouched)"),
        "{stdout}"
    );

    // The reassurance, then exactly one question for the whole set.
    assert!(
        stdout.contains("Each server keeps its name and its entry"),
        "{stdout}"
    );
    assert!(stdout.contains("Tool names don't change"), "{stdout}");
    assert!(stdout.contains("mcpgw sync --rollback"), "{stdout}");
    assert_eq!(stdout.matches("[Y/n] y").count(), 1, "{stdout}");

    let endpoint = url.replace("/mcp", "/s/fx1");
    let entries = json_at(&cursor)["mcpServers"].clone();
    assert_eq!(entries["fx1"]["url"], endpoint);
    assert_eq!(entries["fx1"]["type"], "http");
    // The hand-made entry is exactly as it was left.
    assert_eq!(entries["notes"]["command"], "notes-mcp");

    // Claude Desktop holds no http entry, so it gets the bridge — the
    // gateway's own URL plus the server name, not a path shape.
    let bridged = json_at(&claude)["mcpServers"]["fx1"].clone();
    assert!(
        bridged["command"].as_str().unwrap().contains("mcpgw"),
        "{bridged}"
    );
    assert_eq!(
        bridged["args"],
        serde_json::json!(["connect", "--server", "fx1", "--url", url])
    );

    // mcpgw's own record of what it wrote, which is what `sync` and `doctor`
    // read to tell its entries from the user's.
    let state = json_at(&dir.path().join("state/managed.json"));
    assert_eq!(state["clients"]["cursor"], serde_json::json!(["fx1"]));
    assert_eq!(
        state["clients"]["claude-desktop"],
        serde_json::json!(["fx1"])
    );

    // And the half that decides whether any of it worked: the gateway
    // answering, the server's endpoint answering through it, and both
    // clients landing on it.
    assert!(
        stdout.contains("Checking that it actually works"),
        "{stdout}"
    );
    assert!(stdout.contains("gateway answering at"), "{stdout}");
    assert!(stdout.contains(&format!("{endpoint} — ")), "{stdout}");
    assert!(stdout.contains("tools"), "{stdout}");
    assert_eq!(
        stdout
            .matches("pointing at an endpoint that answers")
            .count(),
        2,
        "{stdout}"
    );

    // The line whose absence turns every first run into a bug report.
    assert!(
        stdout.contains("Done. Restart your clients to pick up the new config."),
        "{stdout}"
    );
    for suggestion in [
        "mcpgw watch",
        "mcpgw add",
        "mcpgw doctor --probe",
        "mcpgw eject",
    ] {
        assert!(stdout.contains(suggestion), "{stdout}");
    }
}

/// The daemon step was skipped, so there is nothing to check against. The
/// config the wizard wrote is still correct, and saying so — with the two
/// commands that finish the job — beats failing a run that did its work.
#[tokio::test]
async fn a_gateway_that_is_down_is_reported_honestly_and_does_not_fail_the_run() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();
    let (cursor, _claude) = install_two_clients(dir.path());

    // A port held open by a socket that never answers HTTP, so "down" is a
    // state this test owns rather than whatever is running on the machine.
    let blocked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", blocked.local_addr().unwrap());

    let child = command(dir.path())
        .args(["init", "--yes", "--gateway-url", &url])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let output = finish(child, "`mcpgw init --yes` against a gateway that is down").await;

    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();

    // Written anyway: the entries are right, they simply have nothing to
    // reach yet.
    assert_eq!(
        json_at(&cursor)["mcpServers"]["fx1"]["url"],
        url.replace("/mcp", "/s/fx1")
    );

    assert!(
        stdout.contains("Checking that it actually works"),
        "{stdout}"
    );
    assert!(stdout.contains("nothing is answering at"), "{stdout}");
    assert!(stdout.contains("mcpgw daemon install"), "{stdout}");
    assert!(stdout.contains("mcpgw serve"), "{stdout}");
    // No endpoint was dialed, so nothing may claim one answered.
    assert!(!stdout.contains("tools"), "{stdout}");
    assert!(stdout.contains("Restart your clients"), "{stdout}");
}

/// Second time round there is nothing left to push, and the wizard says so in
/// one dim line rather than walking the step again.
#[tokio::test]
async fn a_second_run_has_nothing_left_to_push() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("config.toml"), config()).unwrap();
    install_two_clients(dir.path());

    let mut gateway = command(dir.path())
        .args(["serve", "--port", "0", "--no-capture"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let url = gateway_url(&mut gateway).await;

    for run in 1..=2 {
        let child = command(dir.path())
            .args(["init", "--yes", "--gateway-url", &url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let output = finish(child, "`mcpgw init --yes`").await;
        assert_eq!(output.status.code(), Some(0));
        let stdout = String::from_utf8(output.stdout).unwrap();

        if run == 1 {
            assert!(stdout.contains("+ fx1"), "{stdout}");
        } else {
            // The hand-made Cursor entry keeps the import step pending, so
            // the wizard still walks — and the sync step is the one with
            // nothing to say.
            assert!(stdout.contains("nothing to push"), "{stdout}");
            assert!(!stdout.contains("Point them at the gateway?"), "{stdout}");
        }
    }
    gateway.kill().await.unwrap();
}
