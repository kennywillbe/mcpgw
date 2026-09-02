//! The real Linux install → run → stop → start → uninstall cycle.
//!
//! This enables a unit in the systemd user manager of whoever runs it, so it
//! is opt-in: set `MCPGW_DAEMON_LIVE=1`. Without it the test reports what it
//! skipped and passes, which is what CI gets — a runner is a login session
//! too, and leaving units behind in one is how a green build becomes a
//! haunted machine.
//!
//! Less can be isolated here than on macOS. `systemctl --user` reads the unit
//! directory its *manager* was started with, not the one this process points
//! `HOME` at, so the unit really does land in `~/.config/systemd/user` and
//! `mcpgw.service` is global to the session: running this while you have your
//! own mcpgw daemon installed will replace and then remove it. Only the
//! config and the state directory are kept in a temp directory.

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod util;
use util::fixture_binary;

use mcpgw_core::daemon::systemd::UNIT;

const LIVE_ENV: &str = "MCPGW_DAEMON_LIVE";

/// How long the gateway gets to come up under systemd. Generous: the unit is
/// a cold process start, and a flaky "not yet" here would read as a broken
/// installer.
const UP_TIMEOUT: Duration = Duration::from_secs(20);

/// `HOME` is deliberately left alone — see the module docs — so only the
/// config and state directory are redirected into the temp directory.
fn daemon(state: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new(assert_cmd::cargo::cargo_bin("mcpgw"))
        .arg("daemon")
        .args(args)
        .env("MCPGW_NO_UPDATE_CHECK", "1")
        .env("MCPGW_CONFIG", state.join("config.toml"))
        .env("MCPGW_STATE_DIR", state.join("state"))
        .output()
        .unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn systemctl(args: &[&str]) -> std::process::Output {
    std::process::Command::new("systemctl")
        .arg("--user")
        .args(args)
        .output()
        .unwrap()
}

/// Disables the unit and deletes it whatever happened, so a failed assertion
/// cannot leave a supervised gateway running in the session.
struct LeaveNothingBehind;

impl Drop for LeaveNothingBehind {
    fn drop(&mut self) {
        let _ = systemctl(&["disable", "--now", UNIT]);
        if let Some(unit) = unit_path() {
            let _ = std::fs::remove_file(unit);
        }
        let _ = systemctl(&["daemon-reload"]);
    }
}

fn unit_path() -> Option<PathBuf> {
    mcpgw_core::daemon::systemd::unit_path_with(|key| std::env::var_os(key))
}

fn is_active() -> bool {
    String::from_utf8_lossy(&systemctl(&["is-active", UNIT]).stdout).trim() == "active"
}

/// A port that was free a moment ago, and deliberately not 8137: a developer
/// running this very likely has a foreground gateway on the default port.
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

/// Polls `daemon status` until the gateway answers, returning its output.
fn wait_until_up(state: &Path, url: &str) -> std::process::Output {
    let deadline = Instant::now() + UP_TIMEOUT;
    loop {
        let output = daemon(state, &["status", "--url", url]);
        if output.status.code() == Some(0) || Instant::now() > deadline {
            return output;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn the_user_unit_installs_runs_stops_and_leaves_nothing_behind() {
    if std::env::var(LIVE_ENV).as_deref() != Ok("1") {
        eprintln!("skipped: set {LIVE_ENV}=1 to enable a real systemd user unit");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let state = dir.path();
    let _cleanup = LeaveNothingBehind;
    std::fs::write(
        state.join("config.toml"),
        format!(
            "version = 1\n\n[servers.fx1]\ntype = \"stdio\"\ncommand = '{}'\nargs = [\"healthy\"]\n",
            fixture_binary().display()
        ),
    )
    .unwrap();

    let port = free_port();
    let url = format!("http://127.0.0.1:{port}/mcp");
    let unit = unit_path().expect("no HOME to resolve the unit path from");

    // Install: the unit lands where systemctl reads it, and the user is told
    // whether it will survive their logout.
    let installed = daemon(state, &["install", "--port", &port.to_string()]);
    let text = stdout(&installed);
    assert!(
        installed.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(text.contains(&unit.display().to_string()), "{text}");
    assert!(text.contains("lingering"), "{text}");
    assert!(text.contains(&url), "{text}");
    assert!(unit.exists(), "{}", unit.display());
    assert!(is_active(), "systemd did not start the unit");

    // The address install was given is recorded under the state dir, so
    // `status` can find the service without being told where it is (#104).
    let recorded = state.join("state").join("daemon.json");
    assert!(recorded.exists(), "{}", recorded.display());
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&recorded).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }
    let json = std::fs::read_to_string(&recorded).unwrap();
    assert!(json.contains(&format!("\"port\": {port}")), "{json}");

    // Running: the gateway answers on the port it was installed for, and
    // `status` says the service and the gateway are the same thing.
    let up = wait_until_up(state, &url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(text.contains("gateway   running"), "{text}");
    assert!(text.contains("answers (HTTP"), "{text}");

    // The bug this cycle exists to catch: a bare `status`, with no --url,
    // has to probe the port the service was installed with.
    let bare = daemon(state, &["status"]);
    let text = stdout(&bare);
    assert_eq!(bare.status.code(), Some(0), "{text}");
    assert!(text.contains(&url), "{text}");
    assert!(
        text.contains("installed under systemd --user, running"),
        "{text}"
    );
    assert!(!text.contains("foreground"), "{text}");

    // Stop: `Restart=on-failure` restarts a crash, so the proof that stop is
    // not a crash is that the gateway is still down a moment later.
    let stopped = daemon(state, &["stop"]);
    assert!(
        stopped.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(!is_active(), "systemd restarted the gateway after a stop");
    let down = daemon(state, &["status", "--url", &url]);
    let text = stdout(&down);
    assert_eq!(down.status.code(), Some(1), "{text}");
    assert!(text.contains("gateway   not running"), "{text}");
    // Stopped, not uninstalled: the unit is still on disk and enabled, and
    // "it will be back at login" is a different state from gone.
    assert!(unit.exists(), "{}", unit.display());
    assert!(
        text.contains("installed under systemd --user, stopped"),
        "{text}"
    );

    // Start: back from the unit that was never removed.
    let started = daemon(state, &["start", "--port", &port.to_string()]);
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let up = wait_until_up(state, &url);
    assert_eq!(up.status.code(), Some(0), "{}", stdout(&up));

    // Uninstall: out of the manager, off the disk, off the port.
    let removed = daemon(state, &["uninstall"]);
    assert!(
        removed.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!unit.exists(), "{} survived", unit.display());
    assert!(!is_active(), "the unit is still running");
    // The record describes a service that no longer exists.
    assert!(!recorded.exists(), "{} survived", recorded.display());

    let gone = daemon(state, &["status", "--url", &url]);
    let text = stdout(&gone);
    assert_eq!(gone.status.code(), Some(1), "{text}");
    assert!(text.contains("service   not installed"), "{text}");

    // Uninstalling twice is the same end state, not an error.
    assert!(daemon(state, &["uninstall"]).status.success());
}
