//! The passive "there is a newer mcpgw" line.
//!
//! Three rules shape everything here. It goes to stderr, because several
//! commands emit JSON on stdout and a notice inside that would be a bug in
//! whatever parses it. It is silent on every failure, because a laptop on a
//! plane must not be told about its network. And it runs at most once a day
//! and only after the real work is done, so it can neither delay a command
//! nor nag.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Turns the check off entirely. Set by the test suite so no test ever
/// reaches the network or writes into a real home directory.
pub const NO_CHECK_ENV: &str = "MCPGW_NO_UPDATE_CHECK";

/// Where the throttle stamp lives inside the state directory.
const STAMP_FILE: &str = "update-check.json";

/// One check a day. The point is to notice a release within a day or so of
/// it landing, not to track the repository.
// Spelled in seconds rather than with `Duration::from_hours`, which is what
// clippy asks for here: that constructor was stabilised very recently and
// the workspace pins no rust-version, so it would turn an older-but-
// otherwise-fine toolchain into a bare "no function in `Duration`" error
// instead of cargo's MSRV message. Readability is not worth that.
#[allow(clippy::duration_suboptimal_units)]
const INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

/// Hard deadline on the whole check. The command's own work has already
/// finished by the time this runs, so the worst case is the shell prompt
/// coming back two seconds late — once a day, offline.
const TIMEOUT: Duration = Duration::from_secs(2);

/// Prints the notice if one is due, returning whether it printed. Never
/// fails, never blocks longer than [`TIMEOUT`], never writes to stdout.
///
/// The answer matters to the caller because a check that was not due says
/// nothing at all, and since a supervised gateway spends the daily check of
/// its own accord that is now the ordinary outcome rather than the rare one.
pub fn print_if_due(current: &str) -> bool {
    let Some(state_dir) = mcpgw_core::paths::state_dir() else {
        return false;
    };
    match check_and_stamp(&state_dir) {
        Some(latest) if super::is_newer(&latest, current) => {
            announce(&latest, current);
            true
        }
        _ => false,
    }
}

/// Prints the notice from what the last check already found, without any
/// network use and without touching the stamp.
///
/// For the callers that run after a command has failed: they must not add a
/// request to a path the user is already unhappy with, and they must not
/// spend today's check on it either — the exit-0 path stays the only thing
/// that decides when to ask the release host again.
pub fn print_cached(current: &str) {
    if let Some(latest) = cached()
        && super::is_newer(&latest, current)
    {
        announce(&latest, current);
    }
}

/// The one line, wherever it is said from.
fn announce(latest: &str, current: &str) {
    eprintln!("mcpgw {latest} is available (you have {current}) — run `mcpgw self-update`");
}

/// The version the last successful check saw, if there was one.
fn cached() -> Option<String> {
    if std::env::var_os(NO_CHECK_ENV).is_some_and(|value| !value.is_empty()) {
        return None;
    }
    let stamp = mcpgw_core::paths::state_dir()?.join(STAMP_FILE);
    last_seen(&std::fs::read_to_string(stamp).ok()?)
}

/// Runs the throttled lookup against the release host and records what it
/// found in `state_dir`, returning the latest version when a check actually
/// happened. `None` covers "not due", "switched off" and every failure
/// alike — the caller has nothing useful to say about any of them.
///
/// The only thing in mcpgw that asks the release host on a schedule. The
/// notice above calls it after a command, the supervised gateway calls it
/// from a background task, and neither owns the stamp: whichever of the two
/// gets there first spends the day's check for both.
///
/// Blocking, and for up to [`TIMEOUT`]. An async caller owes it a
/// `spawn_blocking`.
pub fn check_and_stamp(state_dir: &Path) -> Option<String> {
    if std::env::var_os(NO_CHECK_ENV).is_some_and(|value| !value.is_empty()) {
        return None;
    }
    let stamp = state_dir.join(STAMP_FILE);
    let now = unix_now()?;
    let previous = std::fs::read_to_string(&stamp).ok();
    if !is_due(previous.as_deref(), now) {
        return None;
    }
    // Stamped before the request, not after: a check that is killed midway
    // (or by a hung connection the deadline eventually cuts) must still
    // count as today's attempt, or every command would retry it.
    let seen = previous.as_deref().and_then(last_seen);
    write_stamp(&stamp, now, seen.as_deref());
    let agent = super::release::agent(TIMEOUT);
    let latest = super::release::latest_version(&agent, &super::release::Endpoints::from_env())
        .ok()
        .filter(|version| super::parse_version(version).is_some())?;
    write_stamp(&stamp, now, Some(&latest));
    Some(latest)
}

/// Whether a check is due, given the stamp file's contents and the current
/// time. A missing, unreadable or nonsensical stamp means "due": the check
/// is cheap and the stamp is not worth repairing.
fn is_due(stamp: Option<&str>, now: u64) -> bool {
    let Some(last) = stamp.and_then(last_check) else {
        return true;
    };
    // A stamp from the future (a clock that jumped, or a restored backup)
    // would otherwise suppress the check until the clock catches up.
    now < last || now.saturating_sub(last) >= INTERVAL.as_secs()
}

fn last_check(stamp: &str) -> Option<u64> {
    serde_json::from_str::<serde_json::Value>(stamp)
        .ok()?
        .get("last_check")?
        .as_u64()
}

fn last_seen(stamp: &str) -> Option<String> {
    Some(
        serde_json::from_str::<serde_json::Value>(stamp)
            .ok()?
            .get("last_seen")?
            .as_str()?
            .to_owned(),
    )
}

/// Best-effort: a state directory that cannot be written is not a reason to
/// say anything to the user, it only means the check runs again tomorrow.
///
/// The hardened helpers rather than bare `std::fs`, because on a fresh
/// machine this is often the first thing to create the state directory —
/// and every later `create_dir_all` leaves an existing directory's mode
/// alone, so a 0755 created here would stay 0755 for the config backups
/// and the managed state that land beside it.
fn write_stamp(path: &Path, now: u64, last_seen: Option<&str>) {
    let stamp = serde_json::json!({ "last_check": now, "last_seen": last_seen });
    if let Some(parent) = path.parent() {
        let _ = mcpgw_core::private::create_dir_all(parent);
    }
    if std::fs::write(path, stamp.to_string()).is_ok() {
        let _ = mcpgw_core::private::harden_file(path);
    }
}

fn unix_now() -> Option<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|since| since.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    #[test]
    fn a_first_run_has_no_stamp_and_is_due() {
        assert!(is_due(None, 1_000_000));
    }

    #[test]
    fn a_check_from_today_is_not_due_again() {
        let stamp = r#"{"last_check": 1000000, "last_seen": "0.2.0"}"#;
        assert!(!is_due(Some(stamp), 1_000_000));
        assert!(!is_due(Some(stamp), 1_000_000 + DAY - 1));
    }

    #[test]
    fn a_check_a_day_old_is_due() {
        let stamp = r#"{"last_check": 1000000, "last_seen": "0.2.0"}"#;
        assert!(is_due(Some(stamp), 1_000_000 + DAY));
        assert!(is_due(Some(stamp), 1_000_000 + 30 * DAY));
    }

    #[test]
    fn a_stamp_from_the_future_does_not_wedge_the_check() {
        let stamp = r#"{"last_check": 9000000}"#;
        assert!(is_due(Some(stamp), 1_000_000));
    }

    #[test]
    fn a_corrupt_stamp_is_treated_as_no_stamp() {
        for stamp in ["", "{", "null", "{}", r#"{"last_check": "yesterday"}"#] {
            assert!(is_due(Some(stamp), 1_000_000), "{stamp}");
        }
    }

    #[test]
    fn the_last_seen_version_survives_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join(STAMP_FILE);
        write_stamp(&path, 1_000_000, Some("0.9.1"));
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(last_check(&written), Some(1_000_000));
        assert_eq!(last_seen(&written).as_deref(), Some("0.9.1"));
        assert!(!is_due(Some(&written), 1_000_000));
    }

    /// The stamp shares the state directory with config backups and the
    /// managed state, so whichever writer gets there first has to leave the
    /// directory owner-only.
    #[cfg(unix)]
    #[test]
    fn the_stamp_and_the_directory_it_creates_are_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("share").join("mcpgw");
        let path = state.join(STAMP_FILE);
        write_stamp(&path, 1_000_000, None);

        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600, "{:o}", mode(&path));
        assert_eq!(mode(&state), 0o700, "{:o}", mode(&state));
    }

    #[test]
    fn a_stamp_without_a_seen_version_is_still_a_valid_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(STAMP_FILE);
        write_stamp(&path, 42, None);
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(last_check(&written), Some(42));
        assert_eq!(last_seen(&written), None);
    }
}
