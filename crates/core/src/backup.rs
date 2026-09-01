//! Timestamped pre-write backups of client config files, pruned per client
//! to the most recent [`KEEP`]; `sync --rollback` restores the newest one.

use std::path::{Path, PathBuf};

use crate::error::Error;

pub const KEEP: usize = 5;

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.to_owned();
    move |source| Error::Io { path, source }
}

fn backup_dir(state_dir: &Path, client_id: &str) -> PathBuf {
    state_dir.join("backups").join(client_id)
}

/// Copies `file` into the client's backup dir and prunes old backups.
/// Returns the backup path.
///
/// # Errors
///
/// Returns [`Error::Io`] for filesystem failures.
pub fn backup_file(state_dir: &Path, client_id: &str, file: &Path) -> Result<PathBuf, Error> {
    let dir = backup_dir(state_dir, client_id);
    std::fs::create_dir_all(&dir).map_err(io_err(&dir))?;
    let name = file
        .file_name()
        .map_or_else(|| "config".to_owned(), |n| n.to_string_lossy().into_owned());
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    // Millis sort lexicographically once zero-padded, which is all the
    // pruning and rollback logic relies on.
    let mut target = dir.join(format!("{millis:015}-{name}"));
    let mut counter = 0;
    while target.exists() {
        counter += 1;
        target = dir.join(format!("{millis:015}.{counter}-{name}"));
    }
    std::fs::copy(file, &target).map_err(io_err(file))?;
    prune(&dir)?;
    Ok(target)
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

fn prune(dir: &Path) -> Result<(), Error> {
    let entries = list(dir)?;
    for old in entries.iter().rev().skip(KEEP) {
        std::fs::remove_file(old).map_err(io_err(old))?;
    }
    Ok(())
}

// Sorted ascending by filename (== by timestamp).
fn list(dir: &Path) -> Result<Vec<PathBuf>, Error> {
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
    let mut entries: Vec<PathBuf> = read
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file())
        .collect();
    entries.sort();
    Ok(entries)
}
