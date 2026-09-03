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
    /// A confirmed change into a new binary: end, and let the supervisor
    /// start it.
    Replaced(UpgradeRestart),
}

/// Watches one path for the binary under it being replaced.
///
/// The stat is a parameter rather than a call so the debounce, the "gone"
/// state and the guard can be tested as the state machine they are, without
/// a filesystem fast enough to reproduce a half-written binary on demand.
pub struct Watcher<S> {
    path: PathBuf,
    stat: S,
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
    /// Watches `path`, stat-ing it with `stat`.
    pub fn new(path: PathBuf, stat: S) -> Self {
        let seen = stat(&path);
        Self {
            path,
            stat,
            seen,
            candidate: None,
            gone: false,
            throttled: false,
            guard: None,
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
        mut self,
        interval: Duration,
        shutdown: impl Future<Output = ()>,
    ) -> Option<UpgradeRestart> {
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => return None,
                () = tokio::time::sleep(interval) => {}
            }
            match self.tick(now()) {
                Outcome::Unchanged => {}
                Outcome::Gone => eprintln!(
                    "warning: the mcpgw binary at {} is gone; staying up on the running one \
                     — a service cannot be restarted from a path with nothing at it",
                    self.path.display()
                ),
                Outcome::Throttled => eprintln!(
                    "warning: the mcpgw binary at {} changed back to one this gateway already \
                     restarted for; staying up rather than restarting in a loop",
                    self.path.display()
                ),
                Outcome::Replaced(restart) => {
                    eprintln!(
                        "the mcpgw binary at {} changed; restarting so the service runs it \
                         (see mcpgw daemon logs)",
                        self.path.display()
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
    /// replaced between two ticks without a filesystem in the way.
    fn watcher(reported: &Cell<Option<u64>>) -> Watcher<impl Fn(&std::path::Path) -> Stamp + '_> {
        Watcher::new("/usr/local/bin/mcpgw".into(), |_: &std::path::Path| {
            reported.get().map(|len| (None, len))
        })
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

    #[test]
    fn a_binary_still_being_written_never_fires() {
        let reported = Cell::new(Some(100));
        let mut watcher = watcher(&reported);

        for len in [1, 90, 300, 512, 900] {
            reported.set(Some(len));
            assert_eq!(watcher.tick(NOW), Outcome::Unchanged, "{len}");
        }
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
