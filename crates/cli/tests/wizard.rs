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
    // The config already has a server, so the survey is skipped and the
    // three stubs each name the command that does their job today.
    assert!(stdout.contains("skipping the survey"), "{stdout}");
    assert!(stdout.contains("mcpgw daemon install"), "{stdout}");
    assert!(stdout.contains("mcpgw sync"), "{stdout}");
    // Nothing was written on a --yes run of a wizard whose writing steps
    // are all still stubs.
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
