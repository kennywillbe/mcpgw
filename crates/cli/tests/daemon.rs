//! End-to-end coverage for `mcpgw daemon`, driven through the real binary
//! so the output shapes under test are the ones a user sees.

use std::path::Path;
use std::process::Stdio;

use tokio::io::{AsyncBufReadExt as _, BufReader};

mod util;
use util::{daemon, fixture_config, free_port, stderr, stdout};

/// A gateway serving one healthy fixture server, with the URL of its
/// aggregate endpoint read off the banner.
async fn serve(home: &Path) -> (tokio::process::Child, String) {
    std::fs::write(home.join("config.toml"), fixture_config(&["fx1"])).unwrap();
    let (child, addr, _endpoints) = util::serve(home, &[]).await;
    (child, format!("http://{addr}/mcp"))
}

#[test]
fn status_reports_a_gateway_that_is_not_there_and_exits_nonzero() {
    let dir = tempfile::tempdir().unwrap();
    // Nothing here depends on the port staying free beyond "not a gateway
    // of ours".
    let port = free_port();
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

/// Writes the record `daemon install` leaves behind, without installing a
/// service: a live install would register a launch agent on whatever machine
/// ran the suite, and the behaviour under test is entirely about what
/// `status` and `start` read back out of it.
fn record_installed_spec(home: &Path, bind: &str, port: u16) {
    let state = home.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let path = |name: &str| state.join(name).display().to_string();
    std::fs::write(
        state.join("daemon.json"),
        format!(
            r#"{{"exe":"/usr/local/bin/mcpgw","config_path":{:?},"state_dir":{:?},
                 "bind":"{bind}","port":{port},
                 "logs":{{"stdout":{:?},"stderr":{:?}}}}}"#,
            home.join("config.toml").display().to_string(),
            state.display().to_string(),
            path("logs/daemon.out.log"),
            path("logs/daemon.err.log"),
        ),
    )
    .unwrap();
}

/// The bug in #104: a service installed on `--port 18137` was reported as a
/// gateway that is not there, because `status` always dialled the default.
#[tokio::test]
async fn status_probes_the_address_the_service_was_installed_with() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, url) = serve(dir.path()).await;
    let port: u16 = url
        .rsplit_once(':')
        .and_then(|(_, tail)| tail.split('/').next())
        .unwrap()
        .parse()
        .unwrap();
    record_installed_spec(dir.path(), "127.0.0.1", port);

    let home = dir.path().to_owned();
    // No --url: the whole point is that the flag is not needed.
    let output = tokio::task::spawn_blocking(move || daemon(&home, &["status"]))
        .await
        .unwrap();

    let text = stdout(&output);
    assert!(text.contains("gateway   running"), "{text}");
    assert!(text.contains(&url), "{text}");
    assert!(!text.contains(":8137"), "probed the default anyway: {text}");
    assert_eq!(output.status.code(), Some(0), "{text}");

    child.kill().await.unwrap();
}

/// Every install made before 0.3.1, and every machine with no service at
/// all: nothing recorded, so the default is what gets probed.
#[test]
fn status_falls_back_to_the_default_address_when_nothing_was_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let text = stdout(&daemon(dir.path(), &["status"]));
    assert!(text.contains("http://127.0.0.1:8137/mcp"), "{text}");
}

/// `start` inherits the recorded port too, so a service installed on 18137
/// comes back up on 18137 rather than on whatever the default is. Proved
/// through the port-in-use refusal, which names the port it checked.
#[test]
fn start_uses_the_port_the_service_was_installed_with() {
    let dir = tempfile::tempdir().unwrap();
    // Held for the whole test so the conflict cannot evaporate under us.
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let port = held.local_addr().unwrap().port();
    record_installed_spec(dir.path(), "127.0.0.1", port);

    let output = daemon(dir.path(), &["start"]);
    let text = stderr(&output);
    assert!(text.contains("something already listens"), "{text}");
    assert!(text.contains(&format!("127.0.0.1:{port}")), "{text}");
    assert!(!output.status.success(), "{text}");
    drop(held);
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

    let mut child = tokio::process::Command::from(util::mcpgw(dir.path()))
        .args(["daemon", "logs", "--follow"])
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

/// The other half of #116: reinstalling over our own service is allowed, but
/// a gateway somebody is running in a terminal is not ours to take, and the
/// refusal has to hold even though it answers exactly like a service would.
#[tokio::test]
async fn install_still_refuses_a_port_a_foreground_gateway_answers_on() {
    let dir = tempfile::tempdir().unwrap();
    let (mut child, url) = serve(dir.path()).await;
    let port = url
        .rsplit_once(':')
        .and_then(|(_, tail)| tail.split('/').next())
        .unwrap()
        .to_owned();

    let home = dir.path().to_owned();
    let output = tokio::task::spawn_blocking(move || daemon(&home, &["install", "--port", &port]))
        .await
        .unwrap();

    let text = stderr(&output);
    assert!(text.contains("something already listens"), "{text}");
    assert!(!output.status.success(), "{text}");
    assert!(
        !stdout(&output).contains("stopping the running service"),
        "{}",
        stdout(&output)
    );

    child.kill().await.unwrap();
}
