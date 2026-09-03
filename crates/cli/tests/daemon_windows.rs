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
use util::{fixture_config, install_on_a_free_port, stderr, stdout};

const LIVE_ENV: &str = "MCPGW_DAEMON_LIVE";

/// How long the gateway gets to come up under the service manager. Generous:
/// this is a cold process start of a process that starts another process.
const UP_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the gateway gets to go away after a stop. Shorter than coming
/// up, because nothing has to start: the service manager only has to notice
/// the request and let the child die.
const DOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// How long the gateway gets to come back out of a binary that replaced the
/// one it was started from. Much longer than a start: the watcher needs two
/// polls to be sure the new file stopped moving, the outgoing gateway then
/// drains for up to five seconds, and only then does the service manager
/// apply its first restart delay.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(60);

/// Every command in this cycle goes through a copy of mcpgw rather than
/// through the binary cargo built, because the upgrade step replaces the
/// file the service was registered with — and that must not be the file the
/// rest of the run is about to execute.
fn daemon(mcpgw: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    util::run_daemon(util::mcpgw_binary(mcpgw, home), args)
}

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
fn wait_until_up(mcpgw: &Path, home: &Path, url: &str) -> std::process::Output {
    let deadline = Instant::now() + UP_TIMEOUT;
    loop {
        let output = daemon(mcpgw, home, &["status", "--url", url]);
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
fn wait_until_down(mcpgw: &Path, home: &Path, url: &str) -> std::process::Output {
    let deadline = Instant::now() + DOWN_TIMEOUT;
    loop {
        let output = daemon(mcpgw, home, &["status", "--url", url]);
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
    // The service is registered against this copy and not against the binary
    // cargo built, so the upgrade step below has something it may replace.
    let mcpgw = &util::binary_copy(&home.join("bin"));
    let _cleanup = LeaveNothingBehind;
    std::fs::write(home.join("config.toml"), fixture_config(&["fx1"])).unwrap();

    // Install. Elevated already, so no prompt is involved and nothing here
    // waits on a dialog — that branch is covered by the unit tests, which is
    // the only place it can be covered without a person at the keyboard.
    let (port, installed) = install_on_a_free_port(|port| {
        daemon(mcpgw, home, &["install", "--port", &port.to_string()])
    });
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
    let up = wait_until_up(mcpgw, home, &url);
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
    let stopped = daemon(mcpgw, home, &["stop"]);
    assert!(
        stopped.status.success(),
        "stop failed: {}",
        stderr(&stopped)
    );
    let down = wait_until_down(mcpgw, home, &url);
    let text = stdout(&down);
    assert_eq!(down.status.code(), Some(1), "{text}");
    assert!(text.contains("gateway   not running"), "{text}");
    assert!(
        text.contains("installed under the Windows service manager, stopped"),
        "{text}"
    );
    assert!(registered(), "stop removed the registration");

    // Start: back from the registration that was never removed.
    let started = daemon(mcpgw, home, &["start", "--port", &port.to_string()]);
    assert!(
        started.status.success(),
        "start failed: {}",
        stderr(&started)
    );
    let up = wait_until_up(mcpgw, home, &url);
    assert_eq!(up.status.code(), Some(0), "{}", stdout(&up));

    reinstall_over_the_running_service(mcpgw, home, port, &url);

    the_service_runs_the_binary_that_replaced_the_installed_one(mcpgw, home, port, &url);

    // Uninstall: out of the database, off the port.
    let removed = daemon(mcpgw, home, &["uninstall"]);
    assert!(
        removed.status.success(),
        "uninstall failed: {}",
        stderr(&removed)
    );
    assert!(!registered(), "the registration survived uninstall");
    // The record describes a service that no longer exists.
    assert!(!recorded.exists(), "{} survived", recorded.display());

    let gone = daemon(mcpgw, home, &["status", "--url", &url]);
    let text = stdout(&gone);
    assert_eq!(gone.status.code(), Some(1), "{text}");
    assert!(text.contains("service   not installed"), "{text}");

    // Uninstalling twice is the same end state, not an error.
    assert!(daemon(mcpgw, home, &["uninstall"]).status.success());
}

/// The #116 step, in a function of its own so the cycle above stays readable:
/// a running service is stopped, re-registered against the binary running now
/// and started again, all by one `daemon install`.
fn reinstall_over_the_running_service(mcpgw: &Path, home: &Path, port: u16, url: &str) {
    // The notice names the binary being left behind, which is the copy this
    // cycle registered — the one assertion that says the redirection took,
    // rather than the service quietly running what cargo built.
    let recorded = mcpgw_core::daemon::load_spec(&home.join("state"))
        .expect("install has to record the binary it installed")
        .exe;
    assert_eq!(
        std::fs::canonicalize(&recorded).unwrap(),
        std::fs::canonicalize(mcpgw).unwrap()
    );

    let again = daemon(mcpgw, home, &["install", "--port", &port.to_string()]);
    let text = stdout(&again);
    let errors = stderr(&again);
    assert!(again.status.success(), "reinstall failed: {errors}");
    assert!(!errors.contains("already listens"), "{errors}");
    assert!(
        text.contains(&format!(
            "stopping the running service to reinstall it (was: {})",
            recorded.display()
        )),
        "{text}"
    );
    // The note that used to stand in for the stop and the start this command
    // now does itself.
    assert!(!text.contains("it was already running"), "{text}");
    assert!(registered(), "the reinstall left no registration");

    let up = wait_until_up(mcpgw, home, url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(
        text.contains("installed under the Windows service manager, running"),
        "{text}"
    );
}

/// The #130 step: the binary the service was registered with is replaced, and
/// the gateway ends itself with a status the service manager's restart
/// actions fire on — so what comes back is a process running the *new* file,
/// from the same path.
///
/// Both the service process and the gateway it supervises are running images
/// of that file, which Windows will not let anything write into or rename
/// over; [`util::replace_binary`] publishes the way `self_replace` does, by
/// renaming the running image aside and the new one into its place.
fn the_service_runs_the_binary_that_replaced_the_installed_one(
    mcpgw: &Path,
    home: &Path,
    port: u16,
    url: &str,
) {
    let state = home.join("state");
    let before = mcpgw_core::runtime::read_record(&state, port)
        .unwrap()
        .expect("a running gateway has to have published a record");
    assert!(
        before.last_upgrade_restart.is_none(),
        "nothing has been replaced yet: {before:?}"
    );

    util::replace_binary(mcpgw);
    // Checked here rather than left to the timeout: a replacement that will
    // not execute is a service that never comes back, which would otherwise
    // read as a watcher that never noticed.
    let ran = util::output_retrying_while_busy(std::process::Command::new(mcpgw).arg("--version"));
    assert!(
        ran.status.success(),
        "the replaced binary does not run: {}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let after = util::wait_for_an_upgrade_restart(&state, port, before.pid, UPGRADE_TIMEOUT);
    // For the file that is there now, and not for some other change: a
    // restart is only evidence of an upgrade if it is an upgrade into this.
    let restart = after.last_upgrade_restart.unwrap();
    assert_eq!(restart.stamp.len, std::fs::metadata(mcpgw).unwrap().len());
    assert!(registered(), "the restart lost the registration");

    let up = wait_until_up(mcpgw, home, url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(
        text.contains("installed under the Windows service manager, running"),
        "{text}"
    );

    // The gateway said why it was going, on the stderr the service redirects
    // into the log file `daemon logs` prints.
    let logs = daemon(mcpgw, home, &["logs", "--lines", "500"]);
    let text = stdout(&logs);
    assert!(
        text.contains("changed; restarting so the service runs it"),
        "{text}"
    );
}
