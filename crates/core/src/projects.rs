//! Repo-local MCP config discovery: the files a team commits, as opposed to
//! the per-machine ones [`crate::clients`] resolves from the home directory.
//!
//! Several clients read a second config from inside the working tree —
//! `.mcp.json`, `.cursor/mcp.json`, `.vscode/mcp.json` — and those are the
//! ones that get reviewed, shared and copied between machines. `sync` writes
//! the home-dir file only, so a repo-local entry keeps pointing straight at
//! its server after a sync and nothing says so.
//!
//! This module is the discovery half. It never writes: it finds the files
//! and reads them, and `import --project`, `sync --project` and `eject` do
//! the rest through the same plan, backup and state machinery the per-user
//! files go through.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use std::collections::BTreeSet;

use crate::clients::{ClientKind, ClientRead, Problem};
use crate::config::Server;
use crate::state::{ManagedState, Scope};

/// One repo-local client config that exists on disk, already read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectConfig {
    pub kind: ClientKind,
    /// The directory the file was found under — the repo root, or the cwd
    /// when it holds a file of its own.
    pub dir: PathBuf,
    pub path: PathBuf,
    /// The same lenient read the home-dir file gets: one broken entry is a
    /// problem, not a file-level failure.
    pub read: ClientRead,
}

/// Where a repo-local entry stands relative to the canonical config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    /// mcpgw wrote this entry and rewrites it on every `sync --project`.
    Managed,
    /// The canonical config holds the same name pointing at the same place,
    /// but mcpgw never wrote it — so it is correct today and nobody's to
    /// keep correct.
    Mirrors,
    /// Nothing canonical matches it. It stays live exactly as written after
    /// a sync, and mcpgw is not the thing that would change it.
    Unmanaged,
}

impl ProjectConfig {
    /// The scope this file's bookkeeping lives under.
    ///
    /// The path is normalised here rather than left as discovered, so that a
    /// scope compares equal to the one [`ManagedState::project_scopes`]
    /// rebuilds out of a state key, and so that the `@path` an origin key
    /// carries reads back to the same file it was written from.
    #[must_use]
    pub fn scope(&self) -> Scope {
        Scope::Project {
            kind: self.kind,
            path: crate::paths::normalize(&self.path),
        }
    }

    /// Every server in the file, in name order, with its standing against
    /// the canonical config, for a caller with no state to consult.
    #[must_use]
    pub fn standings(&self, canonical: &BTreeMap<String, Server>) -> Vec<(&str, Standing)> {
        self.standings_with(canonical, &BTreeSet::new())
    }

    /// The same, told what mcpgw manages in this file — which is the
    /// difference between an entry that stays correct by itself and one
    /// `sync` keeps correct.
    #[must_use]
    pub fn standings_in(
        &self,
        canonical: &BTreeMap<String, Server>,
        state: &ManagedState,
    ) -> Vec<(&str, Standing)> {
        self.standings_with(canonical, &self.scope().managed(state))
    }

    fn standings_with(
        &self,
        canonical: &BTreeMap<String, Server>,
        managed: &BTreeSet<String>,
    ) -> Vec<(&str, Standing)> {
        self.read
            .servers
            .iter()
            .map(|(name, server)| {
                // Name and transport, not the whole server: `tags` are
                // mcpgw's own vocabulary and no client file can carry them,
                // so comparing them would call every entry unmanaged.
                let mirrors = canonical
                    .get(name)
                    .is_some_and(|mine| mine.transport == server.transport);
                let standing = if managed.contains(name) {
                    Standing::Managed
                } else if mirrors {
                    Standing::Mirrors
                } else {
                    Standing::Unmanaged
                };
                (name.as_str(), standing)
            })
            .collect()
    }

    /// How many of this file's entries a plain `sync` would leave live.
    #[must_use]
    pub fn unmanaged(&self, canonical: &BTreeMap<String, Server>) -> usize {
        self.unmanaged_in(canonical, &ManagedState::default())
    }

    /// The same, counting an entry mcpgw already writes as managed.
    #[must_use]
    pub fn unmanaged_in(
        &self,
        canonical: &BTreeMap<String, Server>,
        state: &ManagedState,
    ) -> usize {
        self.standings_in(canonical, state)
            .iter()
            .filter(|(_, standing)| *standing == Standing::Unmanaged)
            .count()
    }
}

/// The repo root above `dir`: the nearest ancestor holding a `.git`, `dir`
/// itself included.
///
/// `.git` is tested for existence rather than for being a directory, because
/// in a worktree or a submodule it is a file pointing elsewhere and the
/// checkout is no less a repo for that.
#[must_use]
pub fn repo_root(dir: &Path) -> Option<PathBuf> {
    dir.ancestors()
        .find(|candidate| candidate.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The directories a discovery pass looks in, and the only ones it may:
/// the repo root and the cwd. Never an ancestor of the repo root — a
/// `.mcp.json` in the parent of a checkout belongs to somebody else's
/// project — and never a descendant, because nothing walks the tree.
#[must_use]
pub fn search_dirs(cwd: &Path) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(root) = repo_root(cwd) {
        dirs.push(root);
    }
    if !dirs.iter().any(|dir| dir == cwd) {
        dirs.push(cwd.to_path_buf());
    }
    dirs
}

/// Reads every repo-local client config reachable from `cwd`.
///
/// Ordered by directory, then by [`ClientKind::ALL`], so a report built from
/// this is stable run to run.
#[must_use]
pub fn discover(cwd: &Path) -> Vec<ProjectConfig> {
    discover_with(cwd, |key| std::env::var_os(key))
}

/// Same as [`discover`] with an injectable environment, which is how the
/// per-user paths it excludes are resolved.
#[must_use]
pub fn discover_with(cwd: &Path, get: impl Fn(&str) -> Option<OsString>) -> Vec<ProjectConfig> {
    let mut found = Vec::new();
    for dir in search_dirs(cwd) {
        for kind in ClientKind::ALL {
            // A working directory can be the home directory, and Cursor's
            // project file has the same name there as its per-user one. The
            // same file reported twice would be a client config accused of
            // not managing itself.
            let personal = kind.config_path_candidates_with(&get);
            for relative in kind.project_config_names() {
                let path = relative
                    .split('/')
                    .fold(dir.clone(), |path, segment| path.join(segment));
                if !path.is_file() || personal.iter().any(|mine| same_file(mine, &path)) {
                    continue;
                }
                found.push(ProjectConfig {
                    kind,
                    dir: dir.clone(),
                    path: path.clone(),
                    read: read_or_problem(kind, &path),
                });
            }
        }
    }
    found
}

/// [`discover`] from the process's working directory. An unreadable cwd is
/// no project configs rather than an error: this is a report on the side of
/// every command that uses it, and it may not be the thing that fails.
#[must_use]
pub fn discover_cwd() -> Vec<ProjectConfig> {
    std::env::current_dir()
        .map(|cwd| discover(&cwd))
        .unwrap_or_default()
}

/// Whether two paths name one file. Compared through the same normalisation
/// the state keys use, so "is this the client's per-user file?" is answered
/// the way "have I written to this file?" is.
fn same_file(a: &Path, b: &Path) -> bool {
    a == b || crate::paths::normalize(a) == crate::paths::normalize(b)
}

/// A file that will not parse becomes one file-level problem rather than an
/// error the caller has to handle: the other project files are still worth
/// reporting, which is the same rule the lenient entry reader follows.
fn read_or_problem(kind: ClientKind, path: &Path) -> ClientRead {
    // The project codec, not the per-user one: a committed `.mcp.json` with
    // a comment in it is a file people really write, and reading it as
    // strict JSON would report the whole file as broken.
    match kind.load_with(kind.project_codec(), path) {
        Ok(read) => read,
        Err(err) => ClientRead {
            servers: BTreeMap::new(),
            problems: vec![Problem {
                server: None,
                message: chain(&err),
            }],
        },
    }
}

/// The error and its causes on one line — a [`Problem`] is a string, so the
/// source chain has to be flattened before it is lost.
fn chain(err: &dyn std::error::Error) -> String {
    let mut out = err.to_string();
    let mut source = err.source();
    while let Some(cause) = source {
        out.push_str(": ");
        out.push_str(&cause.to_string());
        source = cause.source();
    }
    out
}
