//! mcpgw's own state: which server names it wrote into which client file.
//! Losing this file is safe — every client entry then counts as unmanaged
//! and sync stops touching it until re-adopted via import.
//!
//! # Two scopes, one record
//!
//! Until project files were writable there was one file per client, so the
//! client id was key enough for it. It is not any more: a repo-local
//! `.cursor/mcp.json` and `~/.cursor/mcp.json` are two files read by one
//! client, and one bookkeeping entry for both would have a project sync
//! claim the home file's names and the next home sync delete them.
//!
//! So bookkeeping is keyed by [`Scope`]. The per-user file keeps the two
//! maps it has always had, keyed by client id — for that scope the id *is*
//! the file, one per client — and every repo-local file gets an entry in
//! [`ManagedState::files`], keyed by its own path.
//!
//! ## Migration
//!
//! There is none to run. `files` is `#[serde(default)]`, so a state file
//! written by any earlier release loads unchanged and means exactly what it
//! meant: mcpgw manages these names in these per-user files and nothing in
//! any repo. Nothing rewrites `clients` or `resolved`, and a downgrade reads
//! a state file written here as that same per-user record, having simply not
//! heard of the project files — which is the truth for a binary that cannot
//! write them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::clients::ClientKind;
use crate::clients::codec::Codec;
use crate::error::{Error, io_err};

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
    /// The same two records, per repo-local config file, keyed by the file's
    /// own path.
    ///
    /// A path rather than a client id because a client has exactly one
    /// per-user file and any number of project ones, and because two repos
    /// on one machine are two independent sets of managed names.
    ///
    /// Absent from every state file written before project files could be
    /// written, which deserializes as "mcpgw manages nothing in any repo" —
    /// exactly right for a machine where it never did.
    #[serde(default)]
    pub files: BTreeMap<String, FileRecord>,
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

/// What mcpgw wrote into one repo-local config file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRecord {
    /// Which client reads the file. Kept beside the names because `eject`
    /// works from the state alone — it restores a repo it is not standing
    /// in — and a path does not say which codec spells its entries.
    pub client: String,
    /// The entry names mcpgw owns in this file, as
    /// [`ManagedState::clients`] holds them for a per-user file.
    #[serde(default)]
    pub managed: BTreeSet<String>,
    /// The canonical server each of those entries stands for, as
    /// [`ManagedState::resolved`] holds it for a per-user file.
    #[serde(default)]
    pub resolved: BTreeMap<String, String>,
}

/// One config file, as the bookkeeping addresses it.
///
/// Every read and write of [`ManagedState`] goes through a scope rather than
/// through a map key, so the difference between "Cursor's per-user file" and
/// "this repo's `.cursor/mcp.json`" is made once, here, instead of at each
/// call site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// The client's per-user config: one per client, which is why the client
    /// id has always been the whole key.
    Home(ClientKind),
    /// A repo-local config, keyed by its path.
    Project { kind: ClientKind, path: PathBuf },
}

impl Scope {
    #[must_use]
    pub fn kind(&self) -> ClientKind {
        match self {
            Self::Home(kind) | Self::Project { kind, .. } => *kind,
        }
    }

    /// How this file is read and written — see [`ClientKind::project_codec`]
    /// for why a project file does not get the per-user one.
    #[must_use]
    pub fn codec(&self) -> Codec {
        match self {
            Self::Home(kind) => kind.codec(),
            Self::Project { kind, .. } => kind.project_codec(),
        }
    }

    /// The file this scope is about, or `None` for a per-user file, whose
    /// path is resolved from the environment rather than carried.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        match self {
            Self::Home(_) => None,
            Self::Project { path, .. } => Some(path),
        }
    }

    /// How the scope names itself in output: the client, and for a project
    /// file the path, because one client can have several of those and the
    /// name alone would not say which.
    #[must_use]
    pub fn label(&self) -> String {
        match self {
            Self::Home(kind) => kind.display_name().to_owned(),
            Self::Project { kind, path } => {
                format!("{} ({})", kind.display_name(), path.display())
            }
        }
    }

    /// How the scope names itself where the client *id* is what is being
    /// printed — the `(from cursor)` an import line carries. The path is
    /// appended for a repo-local file, which is what makes an origin line
    /// say which of a client's two files an entry came from.
    #[must_use]
    pub fn origin_label(&self) -> String {
        match self {
            Self::Home(kind) => kind.id().to_owned(),
            Self::Project { kind, path } => format!("{} ({})", kind.id(), path.display()),
        }
    }

    /// The directory under `backups/` this file's snapshots go in.
    ///
    /// Per file, not per client: a project file backed up under its client's
    /// id would share the five-deep stack with the per-user file, and
    /// `sync --rollback` would restore one from a snapshot of the other. The
    /// path is hashed rather than spelled out because it has to survive
    /// being a directory name on every platform.
    #[must_use]
    pub fn backup_key(&self) -> String {
        match self {
            Self::Home(kind) => kind.id().to_owned(),
            Self::Project { kind, path } => {
                // Over the same normalised key the record lives under, so a
                // file addressed two ways keeps one backup stack rather than
                // two `sync --rollback` would restore out of turn.
                format!("{}-{:016x}", kind.id(), path_hash(&Self::file_key(path)))
            }
        }
    }

    /// The key this scope's [`FileRecord`] lives under.
    ///
    /// Normalised, never the path as carried: the same file arrives under
    /// the spelling the process's working directory has, the spelling a
    /// user typed, and on Windows the verbatim `\\?\C:\...` one
    /// `canonicalize` returns. Two spellings would be two records, and the
    /// entries `sync --project` wrote through one would read back unmanaged
    /// through the other. A key a 0.5 dev build wrote under some other
    /// spelling is not read back, which is the whole of the compatibility
    /// story: no released version has ever written this map.
    fn file_key(path: &Path) -> String {
        crate::paths::normalize(path).to_string_lossy().into_owned()
    }

    /// The id an import source carries so that adopting its entries writes
    /// them back to the file they came from.
    ///
    /// A per-user file keeps the bare client id, which is what every earlier
    /// release wrote and what `--from` matches; a project file is that id and
    /// its path, separated by an `@` no client id contains.
    #[must_use]
    pub fn origin_key(&self) -> String {
        match self {
            Self::Home(kind) => kind.id().to_owned(),
            Self::Project { kind, path } => format!("{}@{}", kind.id(), path.display()),
        }
    }

    /// Reads back an [`Scope::origin_key`]. `None` for an id no client
    /// answers to, which is a state file naming a client this build does not
    /// have rather than anything to fail over.
    #[must_use]
    pub fn from_origin(key: &str) -> Option<Self> {
        match key.split_once('@') {
            None => ClientKind::from_id(key).map(Self::Home),
            Some((id, path)) => ClientKind::from_id(id).map(|kind| Self::Project {
                kind,
                path: PathBuf::from(path),
            }),
        }
    }

    /// The entry names mcpgw owns in this file.
    #[must_use]
    pub fn managed(&self, state: &ManagedState) -> BTreeSet<String> {
        match self {
            Self::Home(kind) => state.clients.get(kind.id()).cloned().unwrap_or_default(),
            Self::Project { path, .. } => state
                .files
                .get(&Self::file_key(path))
                .map(|record| record.managed.clone())
                .unwrap_or_default(),
        }
    }

    /// The canonical server each of this file's answered entries stands for.
    #[must_use]
    pub fn resolved(&self, state: &ManagedState) -> BTreeMap<String, String> {
        match self {
            Self::Home(kind) => state.resolved.get(kind.id()).cloned().unwrap_or_default(),
            Self::Project { path, .. } => state
                .files
                .get(&Self::file_key(path))
                .map(|record| record.resolved.clone())
                .unwrap_or_default(),
        }
    }

    /// Replaces the set of names mcpgw owns in this file.
    pub fn claim(&self, state: &mut ManagedState, names: BTreeSet<String>) {
        match self {
            Self::Home(kind) => {
                state.clients.insert(kind.id().to_owned(), names);
            }
            Self::Project { kind, path } => {
                let record = state.files.entry(Self::file_key(path)).or_default();
                kind.id().clone_into(&mut record.client);
                record.managed = names;
            }
        }
    }

    /// Adds one entry name to what mcpgw owns in this file, leaving the rest
    /// of the set alone — what adoption does, one entry at a time.
    pub fn adopt(&self, state: &mut ManagedState, name: &str) {
        match self {
            Self::Home(kind) => {
                state
                    .clients
                    .entry(kind.id().to_owned())
                    .or_default()
                    .insert(name.to_owned());
            }
            Self::Project { kind, path } => {
                let record = state.files.entry(Self::file_key(path)).or_default();
                kind.id().clone_into(&mut record.client);
                record.managed.insert(name.to_owned());
            }
        }
    }

    /// Records which canonical server one of this file's entries stands for.
    pub fn resolve_to(&self, state: &mut ManagedState, entry: &str, canonical: &str) {
        match self {
            Self::Home(kind) => {
                state
                    .resolved
                    .entry(kind.id().to_owned())
                    .or_default()
                    .insert(entry.to_owned(), canonical.to_owned());
            }
            Self::Project { kind, path } => {
                let record = state.files.entry(Self::file_key(path)).or_default();
                kind.id().clone_into(&mut record.client);
                record
                    .resolved
                    .insert(entry.to_owned(), canonical.to_owned());
            }
        }
    }
}

/// FNV-1a over the key's bytes: short, stable across runs and platforms,
/// and never used for anything but naming a directory — two paths colliding
/// would share a backup stack, not lose a file.
fn path_hash(key: &str) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl ManagedState {
    /// Every repo-local file mcpgw has written to, in path order.
    ///
    /// The state is the only place these are known: `eject` undoes a repo the
    /// user is not standing in, so it cannot discover them from a cwd.
    #[must_use]
    pub fn project_scopes(&self) -> Vec<Scope> {
        self.files
            .iter()
            .filter(|(_, record)| !record.managed.is_empty())
            .filter_map(|(path, record)| {
                ClientKind::from_id(&record.client).map(|kind| Scope::Project {
                    kind,
                    path: PathBuf::from(path),
                })
            })
            .collect()
    }

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
        // ManagedState is plain string maps; serialization cannot realistically
        // fail, but routing the error beats a panic path in a library.
        let text = serde_json::to_string_pretty(self)
            .map_err(std::io::Error::other)
            .map_err(io_err(path))?;
        // A crash must yield the previous state file, never a truncated one
        // that then fails to parse and takes every managed entry down to
        // "foreign" with it.
        crate::private::write_atomically(path, text.as_bytes()).map_err(io_err(path))
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
