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
    /// Per client, the canonical server a client entry stands for, for the
    /// entries mcpgw had to ask about.
    ///
    /// [`Self::clients`] says which entries mcpgw owns and rewrites; this says
    /// what they *mean*, which differs from the entry's own name only after a
    /// keep-both: the client called its server `github`, the canonical config
    /// already used that name for a different one, so the client's copy came
    /// in as `github-2` and the client's `github` entry has stood for
    /// `github-2` ever since. Without the mapping the next sync would point
    /// that entry at the canonical `github` — the server the user had just
    /// said was a different one.
    ///
    /// An entry mapped to its own name is a decision too: it records that the
    /// user was asked about the conflict and chose to leave the entry alone,
    /// which is what stops the wizard asking again every run.
    ///
    /// Absent from every state file written before the field existed, and
    /// empty for every entry adopted under its own name — both of which load
    /// as "the entry stands for the canonical server it is named after".
    #[serde(default)]
    pub resolved: BTreeMap<String, BTreeMap<String, String>>,
    /// Whether this install has already been told that its client entries now
    /// reach the servers through the gateway.
    ///
    /// The notice explains a change the user did not ask for, so it is worth
    /// interrupting them once and never again. Absent from every state file
    /// written before the field existed, which deserializes as "not told
    /// yet" — exactly right for the installs the notice is for.
    #[serde(default)]
    pub migrated: bool,
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
