//! Timestamped pre-write backups of client config files, pruned per client
//! to the most recent [`KEEP`]; `sync --rollback` restores the newest one.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::Error;

pub const KEEP: usize = 5;

/// Sequence numbers per millisecond. Packing a key as `millis * SEQ_SPAN +
/// seq` keeps ordering arithmetic in one `u64`, and lets a millisecond that
/// overflows its sequence space carry into the next one for free.
const SEQ_SPAN: u64 = 10_000;

/// Highest key this process has handed out, so a backup's name never depends
/// on which of its siblings pruning has already deleted.
static LAST_KEY: AtomicU64 = AtomicU64::new(0);

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.to_owned();
    move |source| Error::Io { path, source }
}

fn backup_dir(state_dir: &Path, client_id: &str) -> PathBuf {
    state_dir.join("backups").join(client_id)
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Copies `file` into the client's backup dir and prunes old backups.
/// Returns the backup path.
///
/// # Errors
///
/// Returns [`Error::Io`] for filesystem failures.
pub fn backup_file(state_dir: &Path, client_id: &str, file: &Path) -> Result<PathBuf, Error> {
    backup_file_at(state_dir, client_id, file, now_millis(), KEEP)
}

fn backup_file_at(
    state_dir: &Path,
    client_id: &str,
    file: &Path,
    millis: u64,
    keep: usize,
) -> Result<PathBuf, Error> {
    let dir = backup_dir(state_dir, client_id);
    crate::private::create_dir_all(&dir).map_err(io_err(&dir))?;
    let name = file
        .file_name()
        .map_or_else(|| "config".to_owned(), |n| n.to_string_lossy().into_owned());
    let mut key = next_key(&dir, millis)?;
    let mut target = dir.join(backup_name(key, &name));
    // Another process writing into the same dir is the only way a fresh key
    // can already be taken; step past it rather than overwrite its backup.
    while target.exists() {
        key = claim_key(key + 1);
        target = dir.join(backup_name(key, &name));
    }
    // `copy` carries the source mode over, so a world-readable client config
    // would produce a world-readable backup of its tokens.
    std::fs::copy(file, &target).map_err(io_err(file))?;
    crate::private::harden_file(&target).map_err(io_err(&target))?;
    prune(&dir, keep)?;
    Ok(target)
}

fn backup_name(key: u64, name: &str) -> String {
    let millis = key / SEQ_SPAN;
    let seq = key % SEQ_SPAN;
    format!("{millis:013}-{seq:04}-{name}")
}

/// The key a new backup gets: at least `millis`, and always past both the
/// newest name on disk and the newest this process has issued. Neither a
/// backwards clock jump nor a prune can hand the same key out twice.
fn next_key(dir: &Path, millis: u64) -> Result<u64, Error> {
    let on_disk = keyed(dir)?.last().map_or(0, |(key, _)| *key);
    Ok(claim_key(
        (millis * SEQ_SPAN).max(on_disk.saturating_add(1)),
    ))
}

fn claim_key(candidate: u64) -> u64 {
    let mut last = LAST_KEY.load(Ordering::Relaxed);
    loop {
        let key = candidate.max(last.saturating_add(1));
        match LAST_KEY.compare_exchange_weak(last, key, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(_) => return key,
            Err(current) => last = current,
        }
    }
}

/// The most recent backup for a client, if any.
///
/// # Errors
///
/// Returns [`Error::Io`] when the backup dir exists but cannot be read.
pub fn latest_backup(state_dir: &Path, client_id: &str) -> Result<Option<PathBuf>, Error> {
    let dir = backup_dir(state_dir, client_id);
    let mut entries = list(&dir)?;
    Ok(entries.pop())
}

fn prune(dir: &Path, keep: usize) -> Result<(), Error> {
    let entries = list(dir)?;
    for old in entries.iter().rev().skip(keep) {
        std::fs::remove_file(old).map_err(io_err(old))?;
    }
    Ok(())
}

// Sorted ascending by write order.
fn list(dir: &Path) -> Result<Vec<PathBuf>, Error> {
    Ok(keyed(dir)?.into_iter().map(|(_, path)| path).collect())
}

/// Every backup in `dir` with its ordering key, ascending.
fn keyed(dir: &Path) -> Result<Vec<(u64, PathBuf)>, Error> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(Error::Io {
                path: dir.to_owned(),
                source,
            });
        }
    };
    let mut entries: Vec<(u64, PathBuf)> = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .map(|path| (parse_key(&path), path))
        .collect();
    // The path breaks ties so the order is total, and so a name we cannot
    // parse still has one fixed place in it.
    entries.sort();
    Ok(entries)
}

/// Ordering key for a backup name. Handles the two shapes written before
/// sequence numbers were padded and parsed — `{millis:015}-{name}` and its
/// `{millis:015}.{n}-{name}` collision form — so old backups stay listable
/// and restorable, ordered as sequence 0 and sequence `n` respectively.
fn parse_key(path: &Path) -> u64 {
    let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
        return 0;
    };
    let Some((stamp, rest)) = file_name.split_once('-') else {
        return 0;
    };
    if let Some((millis, counter)) = stamp.split_once('.') {
        return match (digits(millis), digits(counter)) {
            (Some(millis), Some(counter)) if counter < SEQ_SPAN => {
                millis.saturating_mul(SEQ_SPAN).saturating_add(counter)
            }
            _ => 0,
        };
    }
    let Some(millis) = digits(stamp) else {
        return 0;
    };
    // A four-digit second segment is what separates the current format from
    // an old backup of a file whose own name began with digits.
    let seq = rest
        .split_once('-')
        .filter(|(seq, _)| seq.len() == 4)
        .and_then(|(seq, _)| digits(seq))
        .unwrap_or(0);
    millis.saturating_mul(SEQ_SPAN).saturating_add(seq)
}

fn digits(text: &str) -> Option<u64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, body: &str) {
        std::fs::write(path, body).unwrap();
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    /// The bug this guards: with the clock stuck on one millisecond, every
    /// backup collides, and pruning frees names that a later write used to
    /// be able to reclaim — leaving the newest backup sorting first, so
    /// rollback restored a stale file and the next prune deleted the newest.
    #[test]
    fn frozen_clock_still_orders_backups_by_write_order() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let file = dir.path().join("mcp.json");
        for i in 0..12 {
            write(&file, &format!("{{\"gen\": {i}}}"));
            backup_file_at(&state_dir, "cursor", &file, 1_757_000_000_000, 3).unwrap();
        }
        let backups = list(&state_dir.join("backups/cursor")).unwrap();
        assert_eq!(backups.len(), 3);
        assert_eq!(read(&backups[0]), "{\"gen\": 9}");
        assert_eq!(read(&backups[2]), "{\"gen\": 11}");
        let latest = latest_backup(&state_dir, "cursor").unwrap().unwrap();
        assert_eq!(read(&latest), "{\"gen\": 11}");
    }

    /// A clock that jumps backwards must not make an older backup win.
    #[test]
    fn backwards_clock_still_orders_backups_by_write_order() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let file = dir.path().join("mcp.json");
        for (i, millis) in [1_757_000_000_000, 1_756_000_000_000, 1_757_000_000_000]
            .into_iter()
            .enumerate()
        {
            write(&file, &format!("{{\"gen\": {i}}}"));
            backup_file_at(&state_dir, "cursor", &file, millis, KEEP).unwrap();
        }
        let latest = latest_backup(&state_dir, "cursor").unwrap().unwrap();
        assert_eq!(read(&latest), "{\"gen\": 2}");
    }

    /// Backups written by 0.5.0 and earlier keep their old names on disk.
    #[test]
    fn old_format_backups_stay_listable_and_restorable() {
        let dir = tempfile::tempdir().unwrap();
        let state_dir = dir.path().join("state");
        let backups = state_dir.join("backups/cursor");
        std::fs::create_dir_all(&backups).unwrap();
        // The unpadded counter is why `.10` has to sort after `.2`.
        write(&backups.join("000001757000000000-mcp.json"), "bare");
        write(&backups.join("000001757000000000.2-mcp.json"), "two");
        write(&backups.join("000001757000000000.10-mcp.json"), "ten");

        let listed = list(&backups).unwrap();
        let bodies: Vec<String> = listed.iter().map(|path| read(path)).collect();
        assert_eq!(bodies, ["bare", "two", "ten"]);

        let latest = latest_backup(&state_dir, "cursor").unwrap().unwrap();
        assert_eq!(read(&latest), "ten");

        // Restoring one is a copy back over the live config.
        let live = dir.path().join("mcp.json");
        std::fs::copy(&latest, &live).unwrap();
        assert_eq!(read(&live), "ten");

        // And a new backup taken in the same millisecond wins over all of them.
        write(&live, "new");
        backup_file_at(&state_dir, "cursor", &live, 1_757_000_000, KEEP).unwrap();
        let latest = latest_backup(&state_dir, "cursor").unwrap().unwrap();
        assert_eq!(read(&latest), "new");
    }
}
