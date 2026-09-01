//! mcpgw's own state: which server names it wrote into which client.
//! Losing this file is safe — every client entry then counts as unmanaged
//! and sync stops touching it until re-adopted via import.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedState {
    #[serde(default)]
    pub clients: BTreeMap<String, BTreeSet<String>>,
}

impl ManagedState {
    /// Loads the state file; a missing file is an empty state.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for read failures other than not-found and
    /// [`Error::StateParse`] for corrupt JSON.
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(Error::Io {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        serde_json::from_str(&text).map_err(|source| Error::StateParse {
            path: path.to_owned(),
            source: Box::new(source),
        })
    }

    /// Atomically writes the state file, creating parent dirs as needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] for filesystem failures.
    pub fn save(&self, path: &Path) -> Result<(), Error> {
        let io_err = |p: &Path| {
            let p = p.to_owned();
            move |source| Error::Io { path: p, source }
        };
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        crate::private::create_dir_all(parent).map_err(io_err(parent))?;
        let mut tmp = tempfile::Builder::new()
            .prefix(".managed.json.")
            .tempfile_in(parent)
            .map_err(io_err(parent))?;
        // ManagedState is plain string maps; serialization cannot realistically
        // fail, but routing the error beats a panic path in a library.
        let text = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)
            .map_err(io_err(path))?;
        tmp.write_all(text.as_bytes()).map_err(io_err(path))?;
        // fsync before rename: a crash must yield the previous state file,
        // never a truncated one that then fails to parse and takes every
        // managed entry down to "foreign" with it.
        tmp.as_file().sync_all().map_err(io_err(path))?;
        tmp.persist(path).map_err(|err| Error::Io {
            path: path.to_owned(),
            source: err.error,
        })?;
        // The state file names the servers mcpgw wrote into each client; it
        // is not secret by itself, but it lives beside the backups and gets
        // the same owner-only treatment rather than a second rule to
        // remember.
        crate::private::harden_file(path).map_err(io_err(path))?;
        crate::private::sync_dir(parent).map_err(io_err(parent))?;
        Ok(())
    }

    /// Takes the exclusive lock guarding `path`, blocking until any other
    /// mcpgw process releases it.
    ///
    /// The state file is read, modified and written back over the course of a
    /// whole sync or import run. Without a lock spanning that window two
    /// concurrent runs are last-writer-wins: one client's managed-name set
    /// disappears, its entries read as foreign, and every later sync reports
    /// them as conflicts. Hold the returned guard for the entire cycle.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the sidecar lock file cannot be created or
    /// locked.
    pub fn lock(path: &Path) -> Result<StateLock, Error> {
        Ok(StateLock {
            _file: crate::store::acquire_lock(path)?,
        })
    }
}

/// An held exclusive lock over a state file; released on drop.
#[derive(Debug)]
pub struct StateLock {
    // Sidecar lock file (`managed.json.lock`), never the state file itself:
    // saving renames a new inode over it, which would strand a lock held on
    // the old one.
    _file: std::fs::File,
}
