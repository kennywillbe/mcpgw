//! What the *running* gateway is, as opposed to what was installed.
//!
//! A service manager keeps executing the binary it was handed at start, so
//! after an upgrade the process on 8137 can be a version that no longer
//! exists anywhere on disk. Nothing on the wire answers that question
//! cheaply: `probe_gateway` only says "something HTTP is there", and the
//! `serverInfo` an MCP handshake returns is the upstream's identity whenever
//! the gateway is piping a single server. So the gateway states it itself —
//! one small JSON file written at startup, read by `status`, `doctor` and
//! the exe-change watcher.
//!
//! **A record on disk is not a running gateway.** A crash, a `kill -9` or a
//! machine losing power all leave the file behind, and it will still name a
//! pid that has since been recycled. Readers must confirm the port answers
//! ([`crate::daemon::probe_gateway`]) *before* believing anything here; that
//! policy lives with them, because only they know which port they meant and
//! how long they are willing to wait for it.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// What a gateway process publishes about itself while it is up.
///
/// Deliberately deserialized with serde's default leniency — an unknown field
/// is ignored rather than refused — because the writer and the reader are
/// routinely different builds: a new gateway left the record, an older
/// `mcpgw status` from a second install reads it. Adding a field must not
/// turn that into a parse error, so no `deny_unknown_fields` here, and every
/// field added later needs a `#[serde(default)]` for the other direction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayRecord {
    /// Version of the `mcpgw` that is running, not of the one on disk.
    pub version: String,
    /// Process id, so a reader can tell a restart from a still-running
    /// process it already knew about.
    pub pid: u32,
    /// The executable the process was started from, canonicalized where the
    /// platform allowed it. This is what an upgrade replaces.
    pub exe: PathBuf,
    /// Address the process bound.
    pub bind: String,
    /// Port the process bound. The real one — a `--port 0` gateway records
    /// what the kernel gave it, never the zero it asked for.
    pub port: u16,
    /// Unix seconds at which the record was written, which is startup.
    pub started_at: u64,
    /// The last restart this gateway made for a replaced binary, carried
    /// across that restart by the process the supervisor started next.
    ///
    /// A process that ends on purpose cannot keep a counter in memory, so
    /// the one fact its successor needs — which binary it already stood
    /// aside for, and when — travels through this file. [`None`] for a
    /// gateway that has never done it, and for every record written by a
    /// build from before the field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_upgrade_restart: Option<crate::upgrade::UpgradeRestart>,
}

/// Where the gateway on `port` publishes its [`GatewayRecord`].
///
/// Keyed by port rather than a single `gateway.json`: a foreground `mcpgw
/// serve --port 9000` must not overwrite the record of the installed service
/// on 8137, and every reader already knows the port it is asking about —
/// from `daemon.json` or from the `--url` it was given.
#[must_use]
pub fn record_path(state_dir: &Path, port: u16) -> PathBuf {
    state_dir.join(format!("gateway-{port}.json"))
}

/// Publishes `record` for the port it names.
///
/// # Errors
///
/// [`Error::Io`] when the state directory or the file cannot be written.
pub fn write_record(state_dir: &Path, record: &GatewayRecord) -> Result<(), Error> {
    let path = record_path(state_dir, record.port);
    let io_err = |p: &Path| {
        let p = p.to_owned();
        move |source| Error::Io { path: p, source }
    };
    crate::private::create_dir_all(state_dir).map_err(io_err(state_dir))?;
    let json = serde_json::to_vec_pretty(record)
        .map_err(std::io::Error::other)
        .map_err(io_err(&path))?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".gateway.json.")
        .tempfile_in(state_dir)
        .map_err(io_err(state_dir))?;
    tmp.write_all(&json).map_err(io_err(&path))?;
    // Temp-and-rename, not a truncate in place: a reader polling this file
    // races every restart, and half a record parses as a corrupt one.
    tmp.as_file().sync_all().map_err(io_err(&path))?;
    tmp.persist(&path).map_err(|err| Error::Io {
        path: path.clone(),
        source: err.error,
    })?;
    // The record names the binary and its path, which give away a home
    // directory; it lives under the same owner-only rule as everything else
    // derived from the user's configs rather than a second rule to remember.
    crate::private::harden_file(&path).map_err(io_err(&path))?;
    crate::private::sync_dir(state_dir).map_err(io_err(state_dir))?;
    Ok(())
}

/// The record published for `port`, if there is one.
///
/// [`None`] means no gateway ever wrote one there — which is also what a
/// gateway from a build before this file existed leaves behind.
///
/// # Errors
///
/// [`Error::Io`] for read failures other than not-found, and
/// [`Error::RecordParse`] for a file that is not a record. A corrupt one is
/// not silently treated as absent: "no record" and "a record nobody can read"
/// call for different advice, and only the second names a file to delete.
pub fn read_record(state_dir: &Path, port: u16) -> Result<Option<GatewayRecord>, Error> {
    let path = record_path(state_dir, port);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(Error::Io { path, source }),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|source| Error::RecordParse {
            path,
            source: Box::new(source),
        })
}

/// Withdraws the record for `port` on a clean shutdown.
///
/// Best effort on purpose: it runs while the process is on its way out, and
/// there is nobody left to tell. A record that outlives its gateway is a
/// state readers must survive anyway — every crash produces one.
pub fn remove_record(state_dir: &Path, port: u16) {
    let _ = std::fs::remove_file(record_path(state_dir, port));
}
