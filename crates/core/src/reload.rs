//! Config hot reload: keeping a running gateway's servers in step with the
//! canonical config file, so `mcpgw add` takes effect without a restart.
//!
//! # Why polling
//!
//! Change detection is a 2-second `mtime`+`len` poll, not a filesystem watch.
//! Every write to the canonical config goes through
//! [`ConfigStore`](crate::ConfigStore), which writes a temp file and renames
//! it over the target — the durable-write shape that guarantees a reader
//! never sees a half-written config. That rename replaces the *inode*, and a
//! single-file watch is registered against the inode: the first write makes
//! the watch go deaf, silently, forever. Watching the parent directory
//! instead works, but costs a dependency, a platform backend per OS and a
//! debounce for the flurry of events one rename produces. Two stat calls
//! every two seconds costs none of that and cannot be fooled by a rename.
//!
//! On Unix, `SIGHUP` reloads immediately for anyone who does not want to wait
//! out the poll.
//!
//! # The invariant
//!
//! A reload never mutates a live connection in place. It publishes new
//! handles — a new [`EndpointTable`] and a new server map — and lets the old
//! ones be reaped by refcount once the requests holding them finish. See [`UpstreamManager::apply`] and the endpoint dispatch in
//! [`crate::endpoints`] for the two halves of that.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use crate::capture::CaptureWriter;
use crate::config::Config;
use crate::endpoints::{EndpointTable, Endpoints};
use crate::gateway::Gateway;
use crate::upstream::{Changes, UpstreamManager};

/// How often the canonical config is stat-ed for changes.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

/// What a config file looked like at one instant. `None` for a file that
/// cannot be stat-ed at all, which is a state in its own right: a config that
/// disappears and comes back is a change.
type Stamp = Option<(Option<SystemTime>, u64)>;

/// The cheap half of change detection. Deliberately never opens the file:
/// this runs every two seconds for the life of the process, while the read
/// and parse only happen when something actually moved.
fn stamp(path: &Path) -> Stamp {
    let meta = std::fs::metadata(path).ok()?;
    // `modified` is unsupported on a few exotic filesystems; length alone
    // still catches most edits there, and SIGHUP covers the rest.
    Some((meta.modified().ok(), meta.len()))
}

/// What one reload did.
#[derive(Debug)]
pub struct Reloaded {
    /// How the upstream set changed.
    pub changes: Changes,
    /// The servers now served, in path order.
    pub serving: Vec<String>,
}

/// Applies config changes to a running gateway.
///
/// The same code path builds the endpoint table at startup and at every
/// reload, so a server added later is served exactly the way one present at
/// boot is — there is no second construction site to drift.
pub struct Reloader {
    path: PathBuf,
    manager: Arc<UpstreamManager>,
    endpoints: Endpoints,
    selection: Option<Vec<String>>,
    capture: Option<Arc<CaptureWriter>>,
    /// What the file looked like when it was last read. Owned by the
    /// [`Reloader`] rather than by [`Reloader::watch`] so that the load
    /// `serve` does before the watcher is even spawned already counts as
    /// seen: a config edited in that window would otherwise go unnoticed
    /// until the *next* edit, which is a real gap and a miserable one to
    /// debug.
    seen: std::sync::Mutex<Stamp>,
}

impl Reloader {
    /// Reloads `path` into `manager` and `endpoints`.
    #[must_use]
    pub fn new(path: PathBuf, manager: Arc<UpstreamManager>, endpoints: Endpoints) -> Self {
        Self {
            path,
            manager,
            endpoints,
            selection: None,
            capture: None,
            seen: std::sync::Mutex::new(None),
        }
    }

    /// Restricts serving to `names` (`serve --server`), whatever else the
    /// config grows. A name in this list that is missing or disabled in the
    /// new config drops out: the flag picks from the config, it does not
    /// override it.
    #[must_use]
    pub fn with_selection(mut self, names: Vec<String>) -> Self {
        self.selection = Some(names);
        self
    }

    /// Records the per-server endpoints' traffic into `writer`, the way the
    /// initial table does.
    #[must_use]
    pub fn with_capture(mut self, writer: Arc<CaptureWriter>) -> Self {
        self.capture = Some(writer);
        self
    }

    /// The servers `config` says to serve, honouring the selection.
    fn select(&self, config: &Config) -> Vec<String> {
        let enabled = |name: &str| config.servers.get(name).is_some_and(|s| s.enabled);
        match &self.selection {
            Some(names) => names.iter().filter(|n| enabled(n)).cloned().collect(),
            None => config
                .servers
                .iter()
                .filter(|(_, server)| server.enabled)
                .map(|(name, _)| name.clone())
                .collect(),
        }
    }

    fn pipe(&self, name: &str) -> Gateway {
        let pipe = Gateway::new(Arc::clone(&self.manager), name.to_owned());
        match &self.capture {
            Some(writer) => pipe.with_capture(Arc::clone(writer)),
            None => pipe,
        }
    }

    /// Makes `config` the live one.
    pub async fn apply(&self, config: Config) -> Reloaded {
        let serving = self.select(&config);
        // Upstreams first: a name that reaches its endpoint before the
        // manager knows it would get "unknown upstream" instead of an
        // answer, whereas the opposite window — an endpoint that outlives
        // its upstream by a few microseconds — only affects a server the
        // user just deleted, and reports itself honestly.
        let changes = self.manager.apply(config.servers).await;

        let table = EndpointTable::new(
            serving
                .iter()
                .map(|name| (name.clone(), self.pipe(name)))
                .collect::<Vec<_>>(),
        );
        // Atomic: requests that already picked a service keep running against
        // it, and the next dispatch sees the new table.
        self.endpoints.store(table);
        Reloaded { changes, serving }
    }

    /// Re-reads the config file and applies it.
    ///
    /// # Errors
    ///
    /// Returns the load or parse error, having changed nothing: a config that
    /// does not parse leaves the gateway serving exactly what it served
    /// before. Someone fat-fingering TOML must not take their servers down.
    pub async fn reload(&self) -> Result<Reloaded, crate::Error> {
        // Stamped before the read, not after: a write that lands in between
        // leaves a stamp that no longer matches the file, so the next poll
        // reads it again. The opposite order would record the new file under
        // the old content and drop the edit for good.
        let stamp = stamp(&self.path);
        let config = Config::load(&self.path);
        // Recorded even when the parse failed, so a config with a typo is
        // not re-read (and re-complained about) every two seconds. The next
        // edit — or a SIGHUP — retries it.
        self.mark(stamp);
        Ok(self.apply(config?).await)
    }

    /// Whether the file differs from what was last read.
    fn changed(&self) -> bool {
        let seen = self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        stamp(&self.path) != *seen
    }

    fn mark(&self, stamp: Stamp) {
        *self
            .seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = stamp;
    }

    /// Reloads on every change to the config file until `shutdown` resolves,
    /// reporting each one on stderr. Never returns an error: a reload that
    /// fails is a warning, not the end of the gateway.
    pub async fn watch(self, interval: Duration, shutdown: impl Future<Output = ()>) {
        let mut hangups = Hangups::new();
        let mut shutdown = std::pin::pin!(shutdown);
        loop {
            let forced = tokio::select! {
                () = &mut shutdown => return,
                () = tokio::time::sleep(interval) => false,
                () = hangups.next() => true,
            };
            // A SIGHUP reloads whatever is on disk, changed or not — that is
            // what an operator sending one is asking for.
            if !forced && !self.changed() {
                continue;
            }
            match self.reload().await {
                Ok(reloaded) if reloaded.changes.is_empty() => {}
                Ok(reloaded) => eprintln!(
                    "reloaded {}: {} — serving {}",
                    self.path.display(),
                    reloaded.changes,
                    serving(&reloaded.serving)
                ),
                // Named as a *keep*, not just a failure: the useful half of
                // this message is that the gateway is still up on the old
                // config while the file is fixed.
                Err(err) => eprintln!(
                    "warning: {err} — keeping the previously loaded config; \
                     fix the file (or send SIGHUP) to retry"
                ),
            }
        }
    }
}

fn serving(names: &[String]) -> String {
    if names.is_empty() {
        "no servers".to_owned()
    } else {
        names.join(", ")
    }
}

/// The SIGHUP source, where the platform has one.
#[cfg(unix)]
struct Hangups(Option<tokio::signal::unix::Signal>);

#[cfg(unix)]
impl Hangups {
    /// A handler that could not be installed is a warning, not a reason to
    /// refuse to serve: the 2-second poll still reloads everything.
    fn new() -> Self {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(signal) => Self(Some(signal)),
            Err(err) => {
                eprintln!("warning: SIGHUP will not reload the config: {err}");
                Self(None)
            }
        }
    }

    /// Resolves on the next SIGHUP, or never if there is no handler.
    ///
    /// Cancel-safe, which the `select!` below relies on: `Signal::recv`
    /// registers rather than consumes, so a losing branch loses nothing.
    async fn next(&mut self) {
        match &mut self.0 {
            Some(signal) => {
                signal.recv().await;
            }
            None => std::future::pending().await,
        }
    }
}

/// Windows has no SIGHUP; the poll is the whole story there.
#[cfg(not(unix))]
struct Hangups;

#[cfg(not(unix))]
impl Hangups {
    fn new() -> Self {
        Self
    }

    async fn next(&mut self) {
        std::future::pending::<()>().await;
    }
}

#[cfg(test)]
mod tests {
    use super::stamp;

    #[test]
    fn a_missing_file_stamps_as_absent_and_differs_from_a_real_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(stamp(&path), None);
        std::fs::write(&path, "version = 1\n").unwrap();
        let written = stamp(&path);
        assert!(written.is_some());
        assert_ne!(written, None);
    }

    /// The rename shape `ConfigStore` writes with: a whole new inode over the
    /// old path. The stamp has to notice, which is the entire reason this is
    /// a poll and not an inode watch.
    #[test]
    fn a_rename_over_the_path_changes_the_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "version = 1\n").unwrap();
        let before = stamp(&path);

        let temp = dir.path().join("config.toml.tmp");
        std::fs::write(&temp, "version = 1\n[servers.a]\n").unwrap();
        std::fs::rename(&temp, &path).unwrap();
        assert_ne!(stamp(&path), before);
    }
}
