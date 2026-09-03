//! The real Windows install → run → stop → start → uninstall cycle.
//!
//! This registers a service in the machine's own service control manager, so
//! it is opt-in twice over: set `MCPGW_DAEMON_LIVE=1`, and run it from a
//! terminal opened as administrator. Without either it reports what it
//! skipped and passes, which is what CI gets — a GitHub Windows runner *is*
//! elevated, so nothing but the environment variable stands between this
//! test and a service registered on a build machine.
//!
//! Everything that can be kept off the real machine is: `USERPROFILE` points
//! at a temp directory, so the config, the state and both log files live and
//! die there. The one thing that cannot be isolated is the service name —
//! `mcpgw` is global to the machine, so running this while you have your own
//! mcpgw service installed will remove it.

#![cfg(windows)]

use std::path::Path;
use std::time::{Duration, Instant};

mod util;
use util::{daemon, fixture_config, install_on_a_free_port, stderr, stdout};

const LIVE_ENV: &str = "MCPGW_DAEMON_LIVE";

/// How long the gateway gets to come up under the service manager. Generous:
/// this is a cold process start of a process that starts another process.
const UP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the gateway gets to go away after a stop. Shorter than coming
/// up, because nothing has to start: the service manager only has to notice
/// the request and let the child die.
const DOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Removes the registration whatever happened, so a failed assertion cannot
/// leave a service running on the machine that ran the test.
struct LeaveNothingBehind;

impl Drop for LeaveNothingBehind {
    fn drop(&mut self) {
        let _ = std::process::Command::new("sc")
            .args(["stop", mcpgw_core::daemon::windows::SERVICE_NAME])
            .output();
        let _ = std::process::Command::new("sc")
            .args(["delete", mcpgw_core::daemon::windows::SERVICE_NAME])
            .output();
    }
}

/// Whether the service control manager currently has a registration.
fn registered() -> bool {
    std::process::Command::new("sc")
        .args(["query", mcpgw_core::daemon::windows::SERVICE_NAME])
        .output()
        .unwrap()
        .status
        .success()
}

/// Polls `daemon status` until the gateway answers, returning its output.
fn wait_until_up(home: &Path, url: &str) -> std::process::Output {
    let deadline = Instant::now() + UP_TIMEOUT;
    loop {
        let output = daemon(home, &["status", "--url", url]);
        if output.status.code() == Some(0) || Instant::now() > deadline {
            return output;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

/// Polls `daemon status` until the gateway stops answering, returning its
/// output — the mirror of [`wait_until_up`], and the condition the
/// assertions after a stop are about. A service manager that mistook the
/// requested stop for a failure keeps the gateway answering, so that case
/// runs out the deadline and fails on the output it returns.
fn wait_until_down(home: &Path, url: &str) -> std::process::Output {
    let deadline = Instant::now() + DOWN_TIMEOUT;
    loop {
        let output = daemon(home, &["status", "--url", url]);
        if output.status.code() != Some(0) || Instant::now() > deadline {
            return output;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

#[test]
fn the_service_installs_runs_stops_and_leaves_nothing_behind() {
    if std::env::var(LIVE_ENV).as_deref() != Ok("1") {
        eprintln!("skipped: set {LIVE_ENV}=1, in an elevated terminal, to register a real service");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let _cleanup = LeaveNothingBehind;
    std::fs::write(home.join("config.toml"), fixture_config(&["fx1"])).unwrap();

    // Install. Elevated already, so no prompt is involved and nothing here
    // waits on a dialog — that branch is covered by the unit tests, which is
    // the only place it can be covered without a person at the keyboard.
    let (port, installed) =
        install_on_a_free_port(|port| daemon(home, &["install", "--port", &port.to_string()]));
    let url = format!("http://127.0.0.1:{port}/mcp");
    let text = stdout(&installed);
    assert!(
        installed.status.success(),
        "install failed: {}",
        stderr(&installed)
    );
    assert!(
        !stderr(&installed).contains("administrator rights"),
        "an elevated install still asked for rights: {}",
        stderr(&installed)
    );
    assert!(
        text.contains(r"HKLM\SYSTEM\CurrentControlSet\Services\mcpgw"),
        "{text}"
    );
    assert!(text.contains("LocalSystem"), "{text}");
    assert!(text.contains(&url), "{text}");
    assert!(registered(), "the service control manager has no mcpgw");

    // The address install was given is recorded under the state dir, by the
    // unelevated half, so `status` can find the service without being told
    // where it is (#104).
    let recorded = home.join("state").join("daemon.json");
    assert!(recorded.exists(), "{}", recorded.display());
    let json = std::fs::read_to_string(&recorded).unwrap();
    assert!(json.contains(&format!("\"port\": {port}")), "{json}");

    // Running: the gateway answers on the port it was installed for, and it
    // read the config the *installing* user has rather than LocalSystem's.
    let up = wait_until_up(home, &url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(text.contains("gateway   running"), "{text}");
    assert!(
        text.contains("installed under the Windows service manager, running"),
        "{text}"
    );
    assert!(!text.contains("foreground"), "{text}");
    // The whole point of redirecting the child: the gateway's own banner
    // names the servers it is serving, and it can only be in this file.
    let out_log = std::fs::read_to_string(home.join("state/logs/daemon.out.log")).unwrap();
    assert!(
        out_log.contains("fx1"),
        "the service captured nothing: {out_log}"
    );

    // Stop: still registered, no longer running, and it stays down — the
    // restart actions must not treat a requested stop as a failure.
    let stopped = daemon(home, &["stop"]);
    assert!(
        stopped.status.success(),
        "stop failed: {}",
        stderr(&stopped)
    );
    let down = wait_until_down(home, &url);
    let text = stdout(&down);
    assert_eq!(down.status.code(), Some(1), "{text}");
    assert!(text.contains("gateway   not running"), "{text}");
    assert!(
        text.contains("installed under the Windows service manager, stopped"),
        "{text}"
    );
    assert!(registered(), "stop removed the registration");

    // Start: back from the registration that was never removed.
    let started = daemon(home, &["start", "--port", &port.to_string()]);
    assert!(
        started.status.success(),
        "start failed: {}",
        stderr(&started)
    );
    let up = wait_until_up(home, &url);
    assert_eq!(up.status.code(), Some(0), "{}", stdout(&up));

    reinstall_over_the_running_service(home, port, &url);

    // Uninstall: out of the database, off the port.
    let removed = daemon(home, &["uninstall"]);
    assert!(
        removed.status.success(),
        "uninstall failed: {}",
        stderr(&removed)
    );
    assert!(!registered(), "the registration survived uninstall");
    // The record describes a service that no longer exists.
    assert!(!recorded.exists(), "{} survived", recorded.display());

    let gone = daemon(home, &["status", "--url", &url]);
    let text = stdout(&gone);
    assert_eq!(gone.status.code(), Some(1), "{text}");
    assert!(text.contains("service   not installed"), "{text}");

    // Uninstalling twice is the same end state, not an error.
    assert!(daemon(home, &["uninstall"]).status.success());
}

/// The #116 step, in a function of its own so the cycle above stays readable:
/// a running service is stopped, re-registered against the binary running now
/// and started again, all by one `daemon install`.
fn reinstall_over_the_running_service(home: &Path, port: u16, url: &str) {
    let again = daemon(home, &["install", "--port", &port.to_string()]);
    let text = stdout(&again);
    let errors = stderr(&again);
    assert!(again.status.success(), "reinstall failed: {errors}");
    assert!(!errors.contains("already listens"), "{errors}");
    assert!(
        text.contains("stopping the running service to reinstall it"),
        "{text}"
    );
    // The note that used to stand in for the stop and the start this command
    // now does itself.
    assert!(!text.contains("it was already running"), "{text}");
    assert!(registered(), "the reinstall left no registration");

    let up = wait_until_up(home, url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(
        text.contains("installed under the Windows service manager, running"),
        "{text}"
    );
}
