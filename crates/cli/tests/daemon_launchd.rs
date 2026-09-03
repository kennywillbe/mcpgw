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
use util::{fixture_binary, fixture_config, install_on_a_free_port, stdout};

const LIVE_ENV: &str = "MCPGW_DAEMON_LIVE";

/// How long the gateway gets to come up under launchd. Generous: the agent is
/// a cold process start, and a flaky "not yet" here would read as a broken
/// installer.
const UP_TIMEOUT: Duration = Duration::from_secs(20);

/// How long the gateway gets to come back out of a binary that replaced the
/// one it was started from. Much longer than a start: the watcher needs two
/// polls to be sure the new file stopped moving, the outgoing gateway then
/// drains for up to five seconds, and only after that does launchd get to
/// run the replacement.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(60);

/// Every command in this cycle goes through a copy of mcpgw rather than
/// through the binary cargo built, because the upgrade step replaces the
/// file the service was installed from — and that must not be the file the
/// rest of the run is about to execute.
fn daemon(mcpgw: &Path, home: &Path, args: &[&str]) -> std::process::Output {
    util::run_daemon(util::mcpgw_binary(mcpgw, home), args)
}

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

/// The plist of the machine's own mcpgw service, if this session has a `HOME`
/// at all.
fn real_plist() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|home| plist_path(Path::new(&home)))
}

/// Refuses the cycle when anything it would run sits in a folder macOS keeps
/// behind TCC.
///
/// A launch agent has no TCC grant and no window to ask for one, so a gateway
/// launched from `~/Desktop` hangs in dyld before `main` (#105). What the
/// cycle then reports is the first "gateway running" check timing out after
/// twenty seconds with "nothing is listening" — true, and about the wrong
/// thing. This runs before anything is installed so the failure a developer
/// reads is the remedy instead.
///
/// The mcpgw checked is the copy the service will be installed from and not
/// the one cargo built, because the copy is what launchd executes: a target
/// directory under `~/Desktop` is fine now, and a `TMPDIR` under one is not.
fn refuse_to_run_from_a_tcc_protected_dir(mcpgw: &Path) {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    // Both, because the gateway the agent starts is the mcpgw binary and the
    // servers it then spawns are the fixture; either one under TCC hangs.
    for exe in [mcpgw.to_owned(), fixture_binary()] {
        if let Some(dir) = mcpgw_core::daemon::tcc_protected_dir(&exe, &home) {
            panic!(
                "{} is under ~/{dir}, which macOS keeps behind TCC: a launch \
                 agent gets no grant there, so the gateway would hang in dyld \
                 before main and this cycle would fail twenty seconds later \
                 claiming nothing is listening. Put it outside that folder and \
                 rerun — for the fixture that means building elsewhere, e.g. \
                 CARGO_TARGET_DIR=~/.cache/mcpgw-target {}=1 cargo test -p \
                 mcpgw --test daemon_launchd, and for the copy it means a \
                 TMPDIR that is not under it either",
                exe.display(),
                LIVE_ENV,
            );
        }
    }
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

#[test]
fn the_launch_agent_installs_runs_stops_and_leaves_nothing_behind() {
    if std::env::var(LIVE_ENV).as_deref() != Ok("1") {
        eprintln!("skipped: set {LIVE_ENV}=1 to bootstrap a real launch agent");
        return;
    }

    // Snapshotted before anything is installed: the developer running this may
    // have mcpgw installed as a real service, and the end of the cycle has to
    // be able to tell "left alone" from "absent".
    let real_before = real_plist().map(|path| std::fs::read(&path).ok());

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    // The service is installed from this copy and not from the binary cargo
    // built, so the upgrade step below has something it may replace.
    let mcpgw = &util::binary_copy(&home.join("bin"));
    refuse_to_run_from_a_tcc_protected_dir(mcpgw);
    let _cleanup = LeaveNothingBehind {
        home: home.to_owned(),
    };
    std::fs::write(home.join("config.toml"), fixture_config(&["fx1"])).unwrap();

    let plist = plist_path(home);

    // Install: the plist lands, launchd takes the job, and the user is told
    // about the notification macOS is about to show them.
    let (port, installed) = install_on_a_free_port(|port| {
        daemon(mcpgw, home, &["install", "--port", &port.to_string()])
    });
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
    let up = wait_until_up(mcpgw, home, &url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(text.contains("gateway   running"), "{text}");
    assert!(text.contains("answers (HTTP"), "{text}");

    // The bug this cycle exists to catch: a bare `status`, with no --url,
    // has to probe the port the service was installed with.
    let bare = daemon(mcpgw, home, &["status"]);
    let text = stdout(&bare);
    assert_eq!(bare.status.code(), Some(0), "{text}");
    assert!(text.contains(&url), "{text}");
    assert!(text.contains("installed under launchd, running"), "{text}");
    assert!(!text.contains("foreground"), "{text}");

    // Stop: the decided semantics. `KeepAlive` restarts a crash, so the proof
    // that stop is not a crash is that the gateway is still down a moment
    // later rather than back up.
    let stopped = daemon(mcpgw, home, &["stop"]);
    assert!(
        stopped.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stopped.stderr)
    );
    std::thread::sleep(Duration::from_secs(3));
    assert!(!loaded(), "launchd restarted the gateway after a stop");
    let down = daemon(mcpgw, home, &["status", "--url", &url]);
    let text = stdout(&down);
    assert_eq!(down.status.code(), Some(1), "{text}");
    assert!(text.contains("gateway   not running"), "{text}");
    // Stopped, not uninstalled: the plist is still on disk and `status` says
    // so, because "it will be back at login" is a different state from gone.
    assert!(plist.exists(), "{}", plist.display());
    assert!(text.contains("installed under launchd, stopped"), "{text}");

    // Start: back from the plist that was never removed.
    let started = daemon(mcpgw, home, &["start", "--port", &port.to_string()]);
    assert!(
        started.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let up = wait_until_up(mcpgw, home, &url);
    assert_eq!(up.status.code(), Some(0), "{}", stdout(&up));

    reinstall_over_the_running_service(mcpgw, home, &plist, port, &url);

    the_service_runs_the_binary_that_replaced_the_installed_one(mcpgw, home, port, &url);

    // Uninstall: out of the domain, off the disk, off the port.
    let removed = daemon(mcpgw, home, &["uninstall"]);
    assert!(
        removed.status.success(),
        "uninstall failed: {}",
        String::from_utf8_lossy(&removed.stderr)
    );
    assert!(!plist.exists(), "{} survived", plist.display());
    assert!(!loaded(), "the job is still in the launchd domain");
    // The record describes a service that no longer exists.
    assert!(!recorded.exists(), "{} survived", recorded.display());

    let gone = daemon(mcpgw, home, &["status", "--url", &url]);
    let text = stdout(&gone);
    assert_eq!(gone.status.code(), Some(1), "{text}");
    assert!(text.contains("service   not installed"), "{text}");

    // The real LaunchAgents directory was never a party to any of this: the
    // plist there is byte for byte what it was before, whether that is a
    // service the developer installed themselves or no file at all. Asserting
    // it is absent would fail on every machine that has mcpgw installed.
    if let (Some(path), Some(before)) = (real_plist(), real_before) {
        let after = std::fs::read(&path).ok();
        assert!(
            after == before,
            "{} changed: it was {} and is now {}",
            path.display(),
            describe(before.as_deref()),
            describe(after.as_deref()),
        );
    }

    // Uninstalling twice is the same end state, not an error.
    assert!(daemon(mcpgw, home, &["uninstall"]).status.success());
}

/// A plist snapshot in words, for the one assertion that compares two of them
/// and must not print either as a wall of bytes.
fn describe(bytes: Option<&[u8]>) -> String {
    match bytes {
        None => "absent".to_owned(),
        Some(bytes) => format!("{} bytes", bytes.len()),
    }
}

/// The #116 step, in a function of its own so the cycle above stays readable:
/// a service that is running is reinstalled over rather than refused, and the
/// gateway is back afterwards.
fn reinstall_over_the_running_service(
    mcpgw: &Path,
    home: &Path,
    plist: &Path,
    port: u16,
    url: &str,
) {
    // The notice names the binary being left behind, which is the copy this
    // cycle installed from — the one assertion that says the redirection
    // took, rather than the service quietly running what cargo built.
    let recorded = mcpgw_core::daemon::load_spec(&home.join("state"))
        .expect("install has to record the binary it installed")
        .exe;
    assert_eq!(
        std::fs::canonicalize(&recorded).unwrap(),
        std::fs::canonicalize(mcpgw).unwrap()
    );

    let again = daemon(mcpgw, home, &["install", "--port", &port.to_string()]);
    let text = stdout(&again);
    let errors = String::from_utf8_lossy(&again.stderr).into_owned();
    assert!(again.status.success(), "reinstall failed: {errors}");
    assert!(!errors.contains("already listens"), "{errors}");
    assert!(
        text.contains(&format!(
            "stopping the running service to reinstall it (was: {})",
            recorded.display()
        )),
        "{text}"
    );
    assert!(plist.exists(), "{}", plist.display());

    let up = wait_until_up(mcpgw, home, url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(text.contains("installed under launchd, running"), "{text}");
}

/// The #130 step: the binary the service was installed from is replaced, and
/// the gateway ends itself with a status launchd restarts on — so what comes
/// back is a process running the *new* file, from the same path.
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
    // Checked here rather than left to the timeout: a replacement macOS
    // refuses to execute is a service that never comes back, which would
    // otherwise read as a watcher that never noticed.
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

    let up = wait_until_up(mcpgw, home, url);
    let text = stdout(&up);
    assert_eq!(up.status.code(), Some(0), "{text}");
    assert!(text.contains("installed under launchd, running"), "{text}");

    // The gateway said why it was going, on the stderr the plist redirects
    // into the log file `daemon logs` prints.
    let logs = daemon(mcpgw, home, &["logs", "--lines", "500"]);
    let text = stdout(&logs);
    assert!(
        text.contains("changed; restarting so the service runs it"),
        "{text}"
    );
}
