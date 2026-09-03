//! The real macOS install → run → stop → start → uninstall cycle.
//!
//! This bootstraps a launch agent into the launchd domain of whoever runs it,
//! so it is opt-in: set `MCPGW_DAEMON_LIVE=1`. Without it the test reports
//! what it skipped and passes, which is what CI gets — a runner is a login
//! session too, and leaving agents behind in one is how a green build becomes
//! a haunted machine.
//!
//! Everything else is kept off the real machine: `HOME` points at a temp
//! directory, so the plist is written and removed there rather than in
//! `~/Library/LaunchAgents`. The one thing that cannot be isolated is the
//! label — `io.mcpgw.gateway` is global to the domain, so running this while
//! you have your own mcpgw daemon installed will stop it.

#![cfg(target_os = "macos")]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod util;
use util::{daemon, fixture_config, install_on_a_free_port, stdout};

const LIVE_ENV: &str = "MCPGW_DAEMON_LIVE";

/// How long the gateway gets to come up under launchd. Generous: the agent is
/// a cold process start, and a flaky "not yet" here would read as a broken
/// installer.
const UP_TIMEOUT: Duration = Duration::from_secs(20);

/// Boots the label out of the domain and deletes the plist whatever happened,
/// so a failed assertion cannot leave a supervised gateway running.
struct LeaveNothingBehind {
    home: PathBuf,
}

impl Drop for LeaveNothingBehind {
    fn drop(&mut self) {
        let _ = std::process::Command::new("/bin/launchctl")
            .args(["bootout", &service_target()])
            .output();
        let _ = std::fs::remove_file(plist_path(&self.home));
    }
}

fn uid() -> String {
    let output = std::process::Command::new("/usr/bin/id")
        .arg("-u")
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn service_target() -> String {
    format!("gui/{}/{}", uid(), mcpgw_core::daemon::launchd::LABEL)
}

fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{}.plist", mcpgw_core::daemon::launchd::LABEL))
}

/// Whether launchd currently holds the job.
fn loaded() -> bool {
    std::process::Command::new("/bin/launchctl")
        .args(["print", &service_target()])
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

#[test]
fn the_launch_agent_installs_runs_stops_and_leaves_nothing_behind() {
    if std::env::var(LIVE_ENV).as_deref() != Ok("1") {
        eprintln!("skipped: set {LIVE_ENV}=1 to bootstrap a real launch agent");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let _cleanup = LeaveNothingBehind {
        home: home.to_owned(),
    };
    std::fs::write(home.join("config.toml"), fixture_config(&["fx1"])).unwrap();

    let plist = plist_path(home);

    // Install: the plist lands, launchd takes the job, and the user is told
    // about the notification macOS is about to show them.
    let (port, installed) =
        install_on_a_free_port(|port| daemon(home, &["install", "--port", &port.to_string()]));
    let url = format!("http://127.0.0.1:{port}/mcp");
    let text = stdout(&installed);
    assert!(
        installed.status.success(),
        "install failed: {}",
        String::from_utf8_lossy(&installed.stderr)
    );
    assert!(text.contains(&plist.display().to_string()), "{text}");
    assert!(text.contains("Background Items Added"), "{text}");
    assert!(text.contains(&url), "{text}");
    assert!(plist.exists(), "{}", plist.display());
    assert!(loaded(), "launchd did not take the job");

    // The address install was given is recorded under the state dir, so
    // `status` can find the service without being told where it is (#104).
    let recorded = home.join("state").join("daemon.json");
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
    let up = wait_until_up(home, &url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(text.contains("gateway   running"), "{text}");
    assert!(text.contains("answers (HTTP"), "{text}");

    // The bug this cycle exists to catch: a bare `status`, with no --url,
    // has to probe the port the service was installed with.
    let bare = daemon(home, &["status"]);
    let text = stdout(&bare);
    assert_eq!(bare.status.code(), Some(0), "{text}");
    assert!(text.contains(&url), "{text}");
    assert!(text.contains("installed under launchd, running"), "{text}");
    assert!(!text.contains("foreground"), "{text}");

    // Stop: the decided semantics. `KeepAlive` restarts a crash, so the proof
    // that stop is not a crash is that the gateway is still down a moment
    // later rather than back up.
    let stopped = daemon(home, &["stop"]);
    assert!(
        stopped.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(!loaded(), "launchd restarted the gateway after a stop");
    let down = daemon(home, &["status", "--url", &url]);
    let text = stdout(&down);
    assert_eq!(down.status.code(), Some(1), "{text}");
    assert!(text.contains("gateway   not running"), "{text}");
    // Stopped, not uninstalled: the plist is still on disk and `status` says
    // so, because "it will be back at login" is a different state from gone.
    assert!(plist.exists(), "{}", plist.display());
    assert!(text.contains("installed under launchd, stopped"), "{text}");

    // Start: back from the plist that was never removed.
    let started = daemon(home, &["start", "--port", &port.to_string()]);
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let up = wait_until_up(home, &url);
    assert_eq!(up.status.code(), Some(0), "{}", stdout(&up));

    reinstall_over_the_running_service(home, &plist, port, &url);

    // Uninstall: out of the domain, off the disk, off the port.
    let removed = daemon(home, &["uninstall"]);
    assert!(
        removed.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!plist.exists(), "{} survived", plist.display());
    assert!(!loaded(), "the job is still in the launchd domain");
    // The record describes a service that no longer exists.
    assert!(!recorded.exists(), "{} survived", recorded.display());

    let gone = daemon(home, &["status", "--url", &url]);
    let text = stdout(&gone);
    assert_eq!(gone.status.code(), Some(1), "{text}");
    assert!(text.contains("service   not installed"), "{text}");

    // The real LaunchAgents directory was never a party to any of this.
    if let Some(real_home) = std::env::var_os("HOME") {
        let real = plist_path(Path::new(&real_home));
        assert!(!real.exists(), "{} was written", real.display());
    }

    // Uninstalling twice is the same end state, not an error.
    assert!(daemon(home, &["uninstall"]).status.success());
}

/// The #116 step, in a function of its own so the cycle above stays readable:
/// a service that is running is reinstalled over rather than refused, and the
/// gateway is back afterwards.
fn reinstall_over_the_running_service(home: &Path, plist: &Path, port: u16, url: &str) {
    let again = daemon(home, &["install", "--port", &port.to_string()]);
    let text = stdout(&again);
    let errors = String::from_utf8_lossy(&again.stderr).into_owned();
    assert!(again.status.success(), "reinstall failed: {errors}");
    assert!(!errors.contains("already listens"), "{errors}");
    assert!(
        text.contains("stopping the running service to reinstall it"),
        "{text}"
    );
    assert!(plist.exists(), "{}", plist.display());

    let up = wait_until_up(home, url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(text.contains("installed under launchd, running"), "{text}");
}
