//! Noticing that the binary a supervised gateway was started from has been
//! replaced, and getting the supervisor to run the new one.
//!
//! # Why this exists
//!
//! `brew upgrade mcpgw`, `cargo install mcpgw` and `mcpgw self-update` all
//! put a new binary at the path the login service was installed with, and
//! none of them can restart that service: launchd, systemd `--user` and the
//! Windows SCM only relaunch a job that *ends*, and each relaunches from the
//! path it recorded. So the gateway is the only party in a position to act
//! on the upgrade — it ends itself with a failing status, and the supervisor
//! does the rest, executing the new file at the same path.
//!
//! # Why polling, and why not canonicalized
//!
//! Change detection is a stat every [`POLL_INTERVAL`], for the reasons
//! [`crate::reload`] spells out: every mechanism above publishes by renaming
//! a new file over the path, which replaces the inode a single-file watch is
//! registered against.
//!
//! The watched path is used exactly as installed, never canonicalized.
//! `fs::metadata` follows symlinks, so Homebrew re-pointing
//! `/opt/homebrew/bin/mcpgw` at a new Cellar directory is seen as a change
//! through the same stat that sees cargo's rename-over — and both are
//! precisely the signal wanted. Canonicalizing first would watch the old
//! Cellar file, which an upgrade never touches again.
//!
//! # Why a change has to be seen twice
//!
//! A stamp is acted on only after the *same* new stamp shows up on two
//! consecutive ticks. A binary being written or linked in place changes size
//! between ticks, and restarting into a half-written file is how a gateway
//! stays down. Two ticks cost four seconds of running an old build, which
//! nobody can perceive.
//!
//! # Why the replacement is executed before standing aside
//!
//! Standing aside is a one-way door: this process ends, and whatever is at
//! the path is what the machine gets. Every publisher that renames a new
//! file over the path leaves something runnable there, but a developer who
//! copies a build over the path in place does not: overwriting the bytes of
//! a mapped Mach-O leaves a file macOS refuses to execute at all, and the
//! service then crash-loops under `KeepAlive` on a binary nobody can start.
//! So a confirmed change is run once — `<path> --version` — and a
//! replacement that cannot answer is reported and ignored rather than
//! restarted into.
//!
//! # Limits, stated rather than discovered
//!
//! - Only a service installed with `--supervised` in its argument vector
//!   does any of this. An install from an older build keeps the old
//!   behaviour until `mcpgw daemon install` is run once more.
//! - The [`UpgradeRestart`] guard is the whole loop protection *this* code
//!   can offer, and it only helps while our code still runs. A new binary
//!   that dies before `main` — a bad build, a missing library, macOS
//!   refusing to launch it — is caught by the supervisor's own throttle
//!   (launchd's 10-second floor, systemd's `StartLimit`, the SCM's reset
//!   period), not here.
//! - The baseline is whatever the path holds when the gateway starts, so an
//!   upgrade that lands in the milliseconds between the process starting and
//!   the watcher's first stat is not an upgrade as far as this code is
//!   concerned. What catches that one is [`crate::runtime`]: the record says
//!   which version is running, and `status` compares it against the version
//!   on disk.
//! - A watched path that disappears is reported once and never restarts:
//!   the supervisor could not relaunch from a path with no file at it, so
//!   ending would take the gateway down for good.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// How often the watched executable is stat-ed.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// Status a gateway ends with when it is standing aside for a new binary.
///
/// Non-zero, because that is the only thing all three supervisors restart
/// on. 75 is sysexits' `EX_TEMPFAIL`, "the caller is invited to retry",
/// which is exactly the request being made — and it collides with nothing
/// the CLI already returns (0 success, 1 failure, 2 usage).
pub const UPGRADE_EXIT: u8 = 75;

/// How long a restart for one particular binary stops the next one.
///
/// Long enough that a genuinely crash-looping upgrade is throttled by the
/// supervisor rather than by us, short enough that a second real upgrade in
/// the same session is still picked up.
pub const RESTART_COOLDOWN: Duration = Duration::from_mins(10);

/// How long a replacement gets to answer `--version` before it counts as one
/// that does not run.
///
/// Generous by two orders of magnitude: the answer is a `println` before any
/// config is read. The number that matters is the ceiling on how long a
/// broken file can hold the watcher up, not how long a good one needs.
pub const VERIFY_TIMEOUT: Duration = Duration::from_secs(5);

/// How often the verification run is checked for having finished.
const VERIFY_POLL: Duration = Duration::from_millis(25);

/// What a working mcpgw prints first when asked for its version.
const VERSION_PREFIX: &str = "mcpgw ";

/// What a file looked like at one instant. `None` for a path that cannot be
/// stat-ed at all, which is a state in its own right — see the module docs.
pub type Stamp = Option<(Option<SystemTime>, u64)>;

/// The cheap half of change detection, and the default one [`Watcher`] is
/// built with. Never opens the file: this runs every two seconds for the
/// life of the process.
#[must_use]
pub fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    // `modified` is unsupported on a few exotic filesystems; length alone
    // still catches a version bump there.
    Some((meta.modified().ok(), meta.len()))
}

/// Runs the file at `path` once, to find out whether the supervisor would be
/// able to.
///
/// `--version` because it is the cheapest proof that the file is both
/// executable and mcpgw: it opens no config, binds no port and writes no
/// state, so running it costs a fork and a `println` — and mcpgw is the only
/// thing that answers it in that shape.
///
/// # Errors
///
/// A sentence rather than an error type, because the single caller puts it
/// in parentheses in a log line and nothing branches on it: the file could
/// not be started, it did not answer within [`VERIFY_TIMEOUT`], it ended
/// badly — `signal: 9 (SIGKILL)` is what an in-place overwrite looks like on
/// macOS — or what it printed was not an mcpgw version.
pub fn verify_runs(path: &Path) -> Result<(), String> {
    use std::process::Stdio;

    let mut child = std::process::Command::new(path)
        .arg("--version")
        // Not the gateway's own stdin: a replacement that reads it would be
        // eating the bytes a stdio client is sending the process that is
        // still serving.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|err| format!("it could not be started: {err}"))?;
    let deadline = std::time::Instant::now() + VERIFY_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("waiting for it failed: {err}"));
            }
        }
        if std::time::Instant::now() >= deadline {
            // Killed rather than left behind: the file is already suspect,
            // and a `--version` that hangs would hang for the life of the
            // gateway.
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "it did not answer --version within {}s",
                VERIFY_TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(VERIFY_POLL);
    };
    if !status.success() {
        // `ExitStatus` renders a signal as one on the platforms that have
        // them, which is the whole story on macOS: an in-place overwrite is
        // `signal: 9 (SIGKILL)`.
        return Err(format!("--version ended with {status}"));
    }
    let mut printed = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read as _;
        let _ = out.read_to_string(&mut printed);
    }
    if !printed.starts_with(VERSION_PREFIX) {
        return Err("--version did not print an mcpgw version".to_owned());
    }
    Ok(())
}

/// A binary, coarsely, as it can be written down and compared after a
/// restart.
///
/// Seconds rather than the full `SystemTime`: this crosses a process
/// boundary as JSON, and the guard it feeds only has to answer "is this the
/// same file I already restarted for", where being slightly too willing to
/// say yes costs a delayed restart and being too willing to say no costs a
/// loop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExeStamp {
    /// Modification time in unix seconds, where the filesystem reports one.
    pub mtime: Option<u64>,
    /// Size in bytes.
    pub len: u64,
}

/// A restart this gateway performed because the binary underneath it moved.
///
/// Written into the runtime record before the process ends, and read back by
/// the process the supervisor starts in its place — a counter in memory is
/// worth nothing to code whose whole plan is to exit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpgradeRestart {
    /// The binary that was restarted *into*.
    pub stamp: ExeStamp,
    /// Unix seconds at which the restart was decided.
    pub at: u64,
}

/// What one tick of the watcher concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing worth acting on: no change, a change seen for the first time
    /// and still awaiting confirmation, or something already reported.
    Unchanged,
    /// The watched path has no file at it. Reported once per disappearance.
    Gone,
    /// A confirmed change that the guard refused, because this gateway
    /// already restarted for that same binary. Reported once.
    Throttled,
    /// A confirmed change into a file that does not run, carrying the
    /// reason it does not. Reported once per such change: the stamp is
    /// recorded as seen, so the same broken file is not run every tick, and
    /// the next change is verified afresh.
    Unrunnable(String),
    /// A confirmed change into a new binary that runs: end, and let the
    /// supervisor start it.
    Replaced(UpgradeRestart),
}

/// Watches one path for the binary under it being replaced.
///
/// The stat and the verification are parameters rather than calls, so the
/// debounce, the "gone" state, the guard and the refusal to restart into a
/// file that does not run can be tested as the state machine they are —
/// without a filesystem fast enough to reproduce a half-written binary on
/// demand, and without a build of a broken one.
pub struct Watcher<S, V = fn(&Path) -> Result<(), String>> {
    path: PathBuf,
    stat: S,
    /// Asked once per confirmed change, and only for a change that would
    /// otherwise end the process.
    verify: V,
    /// The stamp this watcher is content with. Set at construction, so a
    /// binary replaced before the watcher started is not read as an upgrade
    /// the moment it starts.
    seen: Stamp,
    /// A change waiting for a second sighting. `None` means none is pending;
    /// a vanished file is never a candidate.
    candidate: Stamp,
    gone: bool,
    throttled: bool,
    guard: Option<UpgradeRestart>,
}

impl<S: Fn(&Path) -> Stamp> Watcher<S> {
    /// Watches `path`, stat-ing it with `stat` and verifying a replacement
    /// by running it.
    pub fn new(path: PathBuf, stat: S) -> Self {
        let seen = stat(&path);
        Self {
            path,
            stat,
            verify: verify_runs,
            seen,
            candidate: None,
            gone: false,
            throttled: false,
            guard: None,
        }
    }
}

impl<S: Fn(&Path) -> Stamp, V: Fn(&Path) -> Result<(), String>> Watcher<S, V> {
    /// Checks a replacement with `verify` instead of running it.
    #[must_use]
    pub fn with_verify<W: Fn(&Path) -> Result<(), String>>(self, verify: W) -> Watcher<S, W> {
        Watcher {
            path: self.path,
            stat: self.stat,
            verify,
            seen: self.seen,
            candidate: self.candidate,
            gone: self.gone,
            throttled: self.throttled,
            guard: self.guard,
        }
    }

    /// Carries the restart the previous process recorded, so a binary that
    /// this gateway has already stood aside for once does not get a second
    /// restart out of it.
    #[must_use]
    pub fn with_guard(mut self, guard: Option<UpgradeRestart>) -> Self {
        self.guard = guard;
        self
    }

    /// The path being watched, for the line that says so at startup.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// One poll, at `now` unix seconds.
    pub fn tick(&mut self, now: u64) -> Outcome {
        let current = (self.stat)(&self.path);
        let Some(found) = current else {
            self.candidate = None;
            // Said once per disappearance, not every two seconds until
            // someone puts the file back.
            return if std::mem::replace(&mut self.gone, true) {
                Outcome::Unchanged
            } else {
                Outcome::Gone
            };
        };
        self.gone = false;
        if current == self.seen {
            self.candidate = None;
            return Outcome::Unchanged;
        }
        if self.candidate != current {
            // Either the first sighting of this change or a stamp still
            // moving under us — a build writing in place looks exactly like
            // that, and neither is something to restart into.
            self.candidate = current;
            return Outcome::Unchanged;
        }
        self.candidate = None;
        self.seen = current;
        let restart = UpgradeRestart {
            stamp: ExeStamp {
                mtime: found.0.and_then(unix_seconds),
                len: found.1,
            },
            at: now,
        };
        if self.refused(&restart, now) {
            return if std::mem::replace(&mut self.throttled, true) {
                Outcome::Unchanged
            } else {
                Outcome::Throttled
            };
        }
        // After the guard, because a change this gateway is not going to
        // restart into anyway is not worth a fork.
        if let Err(reason) = (self.verify)(&self.path) {
            return Outcome::Unrunnable(reason);
        }
        Outcome::Replaced(restart)
    }

    /// Whether the previous process already restarted for this same binary,
    /// recently enough that doing it again would be a loop rather than an
    /// upgrade.
    fn refused(&self, restart: &UpgradeRestart, now: u64) -> bool {
        self.guard.as_ref().is_some_and(|previous| {
            previous.stamp == restart.stamp
                // `saturating_sub` rather than a signed difference: a record
                // stamped in the future (a clock that moved backwards) is
                // treated as recent, which errs towards not looping.
                && now.saturating_sub(previous.at) < RESTART_COOLDOWN.as_secs()
        })
    }

    /// Polls until the binary is replaced or `shutdown` resolves, reporting
    /// on stderr the states that do not end the process.
    ///
    /// [`Some`] means this gateway should stand aside for the binary the
    /// restart names; [`None`] means the gateway is shutting down anyway.
    pub async fn watch(
        self,
        interval: Duration,
        shutdown: impl Future<Output = ()>,
    ) -> Option<UpgradeRestart>
    where
        S: Send + 'static,
        V: Send + 'static,
    {
        let mut shutdown = std::pin::pin!(shutdown);
        let mut watcher = self;
        loop {
            tokio::select! {
                () = &mut shutdown => return None,
                () = tokio::time::sleep(interval) => {}
            }
            // Off the runtime and back again: a tick stats the path, and
            // once per change it also forks the replacement and waits for
            // it. Neither of those belongs on a thread that is answering
            // requests.
            let ticked = tokio::task::spawn_blocking(move || {
                let outcome = watcher.tick(now());
                (watcher, outcome)
            })
            .await;
            let Ok((ticked, outcome)) = ticked else {
                return None;
            };
            watcher = ticked;
            let path = watcher.path.display();
            match outcome {
                Outcome::Unchanged => {}
                Outcome::Gone => eprintln!(
                    "warning: the mcpgw binary at {path} is gone; staying up on the running one \
                     — a service cannot be restarted from a path with nothing at it"
                ),
                Outcome::Throttled => eprintln!(
                    "warning: the mcpgw binary at {path} changed back to one this gateway already \
                     restarted for; staying up rather than restarting in a loop"
                ),
                Outcome::Unrunnable(reason) => eprintln!(
                    "warning: the mcpgw binary at {path} changed but does not run ({reason}); \
                     staying on the current build — replace it with a fresh file (rename into \
                     place), not an in-place overwrite"
                ),
                Outcome::Replaced(restart) => {
                    eprintln!(
                        "the mcpgw binary at {path} changed; restarting so the service runs it \
                         (see mcpgw daemon logs)"
                    );
                    return Some(restart);
                }
            }
        }
    }
}

/// The binary a supervised gateway should watch.
///
/// The installed spec wins over this process's own executable, because the
/// spec's `exe` is the path the supervisor will relaunch from — and those
/// two differ exactly when it matters, such as a service installed against
/// `/opt/homebrew/bin/mcpgw` whose running image is a Cellar file that no
/// upgrade will ever touch again.
#[must_use]
pub fn watched_exe(state_dir: Option<&Path>) -> Option<PathBuf> {
    state_dir
        .and_then(crate::daemon::load_spec)
        .map(|spec| spec.exe)
        .or_else(|| std::env::current_exe().ok())
}

/// Now, in unix seconds. A clock behind the epoch is not worth a failure
/// path: it only makes the guard treat its record as ancient.
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

fn unix_seconds(time: SystemTime) -> Option<u64> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::{ExeStamp, Outcome, Stamp, UpgradeRestart, Watcher};

    const NOW: u64 = 1_700_000_000;

    /// A stat that answers from a cell the test moves, so a "binary" can be
    /// replaced between two ticks without a filesystem in the way. The
    /// replacement always runs; the tests that care say otherwise with
    /// [`verifying`].
    fn watcher(
        reported: &Cell<Option<u64>>,
    ) -> Watcher<
        impl Fn(&std::path::Path) -> Stamp + '_,
        impl Fn(&std::path::Path) -> Result<(), String>,
    > {
        Watcher::new("/usr/local/bin/mcpgw".into(), |_: &std::path::Path| {
            reported.get().map(|len| (None, len))
        })
        .with_verify(|_: &std::path::Path| Ok(()))
    }

    /// The same watcher, with a verification the test can fail at will and
    /// count.
    fn verifying<'a>(
        reported: &'a Cell<Option<u64>>,
        runs: &'a Cell<bool>,
        checked: &'a Cell<u32>,
    ) -> Watcher<
        impl Fn(&std::path::Path) -> Stamp + 'a,
        impl Fn(&std::path::Path) -> Result<(), String> + 'a,
    > {
        Watcher::new("/usr/local/bin/mcpgw".into(), |_: &std::path::Path| {
            reported.get().map(|len| (None, len))
        })
        .with_verify(move |_: &std::path::Path| {
            checked.set(checked.get() + 1);
            if runs.get() {
                Ok(())
            } else {
                Err("signal: 9 (SIGKILL)".to_owned())
            }
        })
    }

    /// The one-way door: a confirmed change is run before this gateway ends
    /// for it, because the file an in-place overwrite leaves behind is one
    /// macOS will not execute at all.
    #[test]
    fn a_replacement_that_does_not_run_is_never_restarted_into() {
        let reported = Cell::new(Some(100));
        let runs = Cell::new(false);
        let checked = Cell::new(0);
        let mut watcher = verifying(&reported, &runs, &checked);

        reported.set(Some(200));
        assert_eq!(watcher.tick(NOW), Outcome::Unchanged);
        assert_eq!(
            watcher.tick(NOW),
            Outcome::Unrunnable("signal: 9 (SIGKILL)".to_owned())
        );
        assert_eq!(checked.get(), 1);

        // The broken file was taken as seen, so it is not forked every two
        // seconds for the rest of the gateway's life.
        for _ in 0..5 {
            assert_eq!(watcher.tick(NOW), Outcome::Unchanged);
        }
        assert_eq!(checked.get(), 1);
    }

    /// Recording the broken file is not giving up on the path: the next
    /// thing that lands there is a change like any other, and gets its own
    /// verification.
    #[test]
    fn the_replacement_after_a_broken_one_is_verified_again_and_fires() {
        let reported = Cell::new(Some(100));
        let runs = Cell::new(false);
        let checked = Cell::new(0);
        let mut watcher = verifying(&reported, &runs, &checked);

        reported.set(Some(200));
        assert_eq!(watcher.tick(NOW), Outcome::Unchanged);
        assert!(matches!(watcher.tick(NOW), Outcome::Unrunnable(_)));

        runs.set(true);
        reported.set(Some(300));
        assert_eq!(watcher.tick(NOW + 2), Outcome::Unchanged);
        assert!(matches!(watcher.tick(NOW + 4), Outcome::Replaced(_)));
        assert_eq!(checked.get(), 2);
    }

    /// The debounce comes first: a file still being written is not forked
    /// once per tick to find out that it is half a binary.
    #[test]
    fn a_binary_still_being_written_is_never_run() {
        let reported = Cell::new(Some(100));
        let runs = Cell::new(true);
        let checked = Cell::new(0);
        let mut watcher = verifying(&reported, &runs, &checked);

        for len in [1, 90, 300, 512, 900] {
            reported.set(Some(len));
            assert_eq!(watcher.tick(NOW), Outcome::Unchanged, "{len}");
        }
        assert_eq!(checked.get(), 0);
    }

    #[test]
    fn a_file_that_is_not_a_program_does_not_verify() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcpgw");
        std::fs::write(&path, b"not a bin\n").unwrap();

        assert!(super::verify_runs(&path).is_err());
    }

    #[test]
    fn a_binary_nobody_touched_is_never_restarted_for() {
        let reported = Cell::new(Some(100));
        let mut watcher = watcher(&reported);

        for _ in 0..5 {
            assert_eq!(watcher.tick(NOW), Outcome::Unchanged);
        }
    }

    /// The debounce: one sighting is not evidence, because a binary being
    /// written in place is a different size on every tick.
    #[test]
    fn a_change_seen_once_waits_and_the_same_change_seen_twice_fires() {
        let reported = Cell::new(Some(100));
        let mut watcher = watcher(&reported);

        reported.set(Some(200));
        assert_eq!(watcher.tick(NOW), Outcome::Unchanged);

        let Outcome::Replaced(restart) = watcher.tick(NOW) else {
            panic!("a change confirmed on two ticks has to fire");
        };
        assert_eq!(
            restart.stamp,
            ExeStamp {
                mtime: None,
                len: 200
            }
        );
        assert_eq!(restart.at, NOW);
    }

    /// A cargo install is a rename over the path: the file is briefly
    /// absent, and coming back is the change that fires.
    #[test]
    fn a_vanished_binary_is_reported_once_and_restarted_for_never() {
        let reported = Cell::new(Some(100));
        let mut watcher = watcher(&reported);

        reported.set(None);
        assert_eq!(watcher.tick(NOW), Outcome::Gone);
        assert_eq!(watcher.tick(NOW), Outcome::Unchanged);
        assert_eq!(watcher.tick(NOW), Outcome::Unchanged);

        reported.set(Some(200));
        assert_eq!(watcher.tick(NOW), Outcome::Unchanged);
        assert!(matches!(watcher.tick(NOW), Outcome::Replaced(_)));
    }

    /// The record a previous process left is the only thing standing between
    /// a binary that keeps looking new and an endless restart.
    #[test]
    fn the_binary_a_previous_process_already_restarted_for_is_refused() {
        let reported = Cell::new(Some(100));
        let guard = UpgradeRestart {
            stamp: ExeStamp {
                mtime: None,
                len: 200,
            },
            at: NOW,
        };
        let mut watcher = watcher(&reported).with_guard(Some(guard));

        reported.set(Some(200));
        assert_eq!(watcher.tick(NOW + 60), Outcome::Unchanged);
        assert_eq!(watcher.tick(NOW + 62), Outcome::Throttled);
        // A refusal is about one binary: the next one that shows up still
        // gets its restart.
        reported.set(Some(300));
        assert_eq!(watcher.tick(NOW + 64), Outcome::Unchanged);
        assert!(matches!(watcher.tick(NOW + 66), Outcome::Replaced(_)));
    }

    #[test]
    fn the_same_binary_is_restarted_for_again_once_the_cooldown_is_over() {
        let reported = Cell::new(Some(100));
        let guard = UpgradeRestart {
            stamp: ExeStamp {
                mtime: None,
                len: 200,
            },
            at: NOW,
        };
        let mut watcher = watcher(&reported).with_guard(Some(guard));

        reported.set(Some(200));
        let after = NOW + super::RESTART_COOLDOWN.as_secs() + 1;
        assert_eq!(watcher.tick(after), Outcome::Unchanged);
        assert!(matches!(watcher.tick(after), Outcome::Replaced(_)));
    }

    /// A guard is about one binary, not about restarting in general.
    #[test]
    fn a_different_binary_is_restarted_for_despite_the_guard() {
        let reported = Cell::new(Some(100));
        let guard = UpgradeRestart {
            stamp: ExeStamp {
                mtime: None,
                len: 999,
            },
            at: NOW,
        };
        let mut watcher = watcher(&reported).with_guard(Some(guard));

        reported.set(Some(200));
        assert_eq!(watcher.tick(NOW), Outcome::Unchanged);
        assert!(matches!(watcher.tick(NOW), Outcome::Replaced(_)));
    }
}
