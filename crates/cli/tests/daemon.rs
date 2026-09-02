//! End-to-end coverage for `mcpgw daemon`, driven through the real binary
//! so the output shapes under test are the ones a user sees.

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt as _, BufReader};

mod util;
use util::fixture_binary;

/// The daemon command with a home, config and state dir of its own, so no
/// test can see (or write to) the real ones.
fn daemon(home: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"))
        .arg("daemon")
        .args(args)
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", home.join("config.toml"))
        .env("MCPGW_STATE_DIR", home.join("state"))
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .output()
        .unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Spawns a real foreground gateway on an ephemeral port, returning it with
/// the URL read off its banner — the port is never guessed, so two tests
/// running at once cannot collide.
async fn serve(home: &Path) -> (tokio::process::Child, String) {
    let config_path = home.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "version = 1\n\n[servers.fx1]\ntype = \"stdio\"\ncommand = '{}'\nargs = [\"healthy\"]\n",
            fixture_binary().display()
        ),
    )
    .unwrap();

    let mut child = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"))
        .arg("serve")
        .args(["--port", "0", "--no-capture"])
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", &config_path)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .env_remove("XDG_CONFIG_HOME")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();

    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let banner = lines.next_line().await.unwrap().unwrap();
    let addr = banner
        .split("http://")
        .nth(1)
        .and_then(|rest| rest.split("/mcp").next())
        .unwrap_or_else(|| panic!("no address in banner: {banner}"))
        .to_owned();
    // Kept drained: a gateway whose `println!` hits a closed pipe dies of
    // EPIPE mid-test (see the same guard in tests/serve.rs).
    tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
    (child, format!("http://{addr}/mcp"))
}

#[test]
fn status_reports_a_gateway_that_is_not_there_and_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    // A port that was free a moment ago; nothing here depends on it staying
    // free beyond "not a gateway of ours".
    let port = {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.local_addr().unwrap().port()
    };
    let url = format!("http://127.0.0.1:{port}/mcp");
    let output = daemon(dir.path(), &["status", "--url", &url]);

    let text = stdout(&output);
    assert!(text.contains("gateway   not running"), "{text}");
    assert!(text.contains(&url), "{text}");
    // Every stub says the same thing about a service nobody installed.
    assert!(text.contains("service   not installed"), "{text}");
    assert!(text.contains("daemon.out.log"), "{text}");
    assert!(text.contains("daemon.err.log"), "{text}");
    // No gateway, so the foreground-serve note must not appear.
    assert!(!text.contains("foreground"), "{text}");
    assert_eq!(output.status.code(), Some(1), "{text}");
}

/// The state nearly every user is in this release: a gateway they started in
/// a terminal, and no service anywhere.
#[tokio::test]
async fn status_names_a_running_foreground_gateway_with_no_service_installed() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, url) = serve(dir.path()).await;

    let home = dir.path().to_owned();
    let output = tokio::task::spawn_blocking(move || daemon(&home, &["status", "--url", &url]))
        .await
        .unwrap();

    let text = stdout(&output);
    assert!(text.contains("gateway   running"), "{text}");
    assert!(text.contains("answers (HTTP"), "{text}");
    assert!(text.contains("service   not installed"), "{text}");
    assert!(
        text.contains("no service is installed, but a gateway is already answering"),
        "{text}"
    );
    assert!(text.contains("foreground `mcpgw serve`"), "{text}");
    assert_eq!(output.status.code(), Some(0), "{text}");

    child.kill().await.unwrap();
}

#[test]
fn install_and_start_refuse_a_bind_the_rest_of_the_network_could_reach() {
    let dir = tempfile::tempdir().unwrap();
    for command in ["install", "start"] {
        let output = daemon(dir.path(), &[command, "--bind", "0.0.0.0"]);
        let text = stderr(&output);
        assert!(
            text.contains("refusing to run an unattended gateway"),
            "{text}"
        );
        assert!(text.contains("no authentication"), "{text}");
        assert!(text.contains("127.0.0.1"), "{text}");
        assert!(!output.status.success(), "{command} succeeded: {text}");
    }
}

#[test]
fn install_and_start_name_the_port_that_is_already_taken() {
    let dir = tempfile::tempdir().unwrap();
    // Held for the whole test so the conflict cannot evaporate under us.
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = held.local_addr().unwrap().port().to_string();

    for command in ["install", "start"] {
        let output = daemon(dir.path(), &[command, "--port", &port]);
        let text = stderr(&output);
        assert!(text.contains("something already listens"), "{text}");
        assert!(text.contains(&format!("127.0.0.1:{port}")), "{text}");
        assert!(text.contains("mcpgw daemon status"), "{text}");
        assert!(!output.status.success(), "{command} succeeded: {text}");
    }
    drop(held);
}

/// The installers land per-OS later; until then every one of them has to say
/// so, and point at the thing that does work today.
#[test]
fn the_service_commands_say_which_release_brings_them_and_what_to_do_meanwhile() {
    let dir = tempfile::tempdir().unwrap();
    // Port 0 rather than a port that looked free a moment ago: nothing can
    // ever be holding it, so the preflight always falls through to the
    // platform and this test can never fail with the port-conflict message.
    for args in [
        vec!["install", "--port", "0"],
        vec!["start", "--port", "0"],
        vec!["stop"],
        vec!["uninstall"],
    ] {
        let output = daemon(dir.path(), &args);
        let text = stderr(&output);
        assert!(text.contains("not in this release yet"), "{args:?}: {text}");
        assert!(text.contains("mcpgw serve"), "{args:?}: {text}");
        assert!(!output.status.success(), "{args:?} succeeded: {text}");
    }
}

#[test]
fn logs_prints_both_streams_and_creates_them_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("state").join("logs");

    // Nothing has run yet: the files do not exist and `logs` has to make
    // them rather than fail, so `--follow` has something to follow.
    let output = daemon(dir.path(), &["logs"]);
    let text = stdout(&output);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(text.contains("daemon.out.log ---"), "{text}");
    assert!(text.contains("daemon.err.log ---"), "{text}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&logs), 0o700, "{:o}", mode(&logs));
        for name in ["daemon.out.log", "daemon.err.log"] {
            let path = logs.join(name);
            assert_eq!(mode(&path), 0o600, "{name} is {:o}", mode(&path));
        }
    }

    // History is tailed, and only the tail asked for.
    std::fs::write(logs.join("daemon.out.log"), "one\ntwo\nthree\n").unwrap();
    std::fs::write(logs.join("daemon.err.log"), "boom\n").unwrap();
    let text = stdout(&daemon(dir.path(), &["logs", "-n", "2"]));
    assert!(!text.contains("one"), "{text}");
    assert!(text.contains("two") && text.contains("three"), "{text}");
    assert!(text.contains("boom"), "{text}");
}

/// `--follow` never returns, so it is driven as a child and read from.
#[tokio::test]
async fn logs_follow_prints_lines_appended_after_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let logs = dir.path().join("state").join("logs");
    // Creates the files, so the follower starts at a known end-of-file.
    daemon(dir.path(), &["logs"]);
    std::fs::write(logs.join("daemon.out.log"), "already there\n").unwrap();

    let mut child = tokio::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"))
        .args(["daemon", "logs", "--follow"])
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", dir.path().join("config.toml"))
        .env("MCPGW_STATE_DIR", dir.path().join("state"))
        .env("HOME", dir.path())
        .env("USERPROFILE", dir.path())
        .env_remove("XDG_DATA_HOME")
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();

    // Appended repeatedly rather than once: the follower may still be
    // printing history when the first append lands, and re-appending is
    // cheaper than guessing how long its startup takes on a loaded runner.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let appender = tokio::spawn({
        let path = logs.join("daemon.err.log");
        async move {
            while std::time::Instant::now() < deadline {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap();
                std::io::Write::write_all(&mut file, b"appended later\n").unwrap();
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
    });

    let found = loop {
        match tokio::time::timeout(std::time::Duration::from_secs(90), lines.next_line()).await {
            Ok(Ok(Some(line))) if line.contains("appended later") => break true,
            Ok(Ok(Some(_))) => {}
            _ => break false,
        }
    };
    appender.abort();
    child.kill().await.unwrap();
    assert!(found, "--follow never printed the appended line");
}
