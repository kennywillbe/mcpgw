//! Tool definition pins: what a server said its tools were the first time
//! the gateway saw them, and what has changed since.
//!
//! A tool's description and its `inputSchema` are prompt material. The model
//! reads them and obeys them, so a server that quietly rewrites either after
//! you installed it changes what your agent does on a machine where nothing
//! was reconfigured — the "rug pull" shape the Cloud Security Alliance's note
//! on tool-description poisoning (2026-07-11) and Microsoft's write-up
//! (2026-06-30) both describe. Nothing else in the pipe remembers what the
//! server said last time; this does.
//!
//! One file per server under `<state>/pins/<server>.json`, mode 0600. Per
//! *server* and not per client on purpose: what the server serves is one
//! fact, and two clients meeting the same rewritten tool are not two
//! different events.
//!
//! # What goes into a hash
//!
//! `name`, `description`, `inputSchema`, and — when the tool carries them —
//! `outputSchema` and `annotations`. The first three are what the model is
//! told the tool is and how to call it; `outputSchema` shapes what it
//! believes comes back, and `annotations` carry the read-only and
//! destructive hints a harness uses to decide whether to ask before running
//! something. `icons` and `_meta` are left out: neither reaches the model.
//! So is `title`, which is a display string for a human who is looking at
//! the client's own UI rather than at the agent's context.
//!
//! That is a named list and not "everything the tool object carried", which
//! is the honest limit of this check: a field no revision of MCP has, or one
//! a later one adds, is not hashed and a change to it is not reported. The
//! list covers what today's clients put in front of a model, and widening it
//! to every field would report a `_meta` timestamp as a rug pull.
//!
//! Objects are hashed key-sorted and whitespace-free, so a server that
//! re-serializes its schema through a different JSON writer does not read as
//! a change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::error::{Error, io_err};

/// Subdirectory of the state dir holding the per-server pin files.
pub const PINS_DIR: &str = "pins";

/// Schema version of a pin file. Additive: a reader of this version ignores
/// fields it does not know, and a file written by a *newer* one is left
/// strictly alone (see [`PinStore::observe`]).
pub const VERSION: u32 = 1;

/// One server's pinned tool definitions, and whatever has drifted from them
/// since.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PinFile {
    pub version: u32,
    /// The server the pins are for. Redundant with the filename and kept
    /// anyway: a file copied between machines or quoted in an issue has to
    /// say what it is about.
    pub server: String,
    /// When the pins were taken, in milliseconds since the Unix epoch.
    pub pinned_at: u64,
    /// The pinned definition of each tool, keyed by name.
    #[serde(default)]
    pub tools: BTreeMap<String, ToolPin>,
    /// What the last `tools/list` disagreed with the pins about, empty when
    /// it agreed.
    ///
    /// The diff as of that list rather than a log of every list: it is a
    /// function of the pins and what the server currently serves, so
    /// appending would grow the file once a minute for a client that polls.
    /// `doctor` and `mcpgw tools NAME pin --show` read it, which is what
    /// lets both answer without dialing the server.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drift: Vec<DriftEvent>,
}

/// One tool as it was pinned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolPin {
    /// Lowercase hex SHA-256 over the fields listed in the module docs.
    pub hash: String,
    /// Length in bytes of the pinned description.
    ///
    /// Kept so a drift record can say how much the text grew without the
    /// file — or the traffic log downstream of it — ever holding the text
    /// itself. An injected instruction is exactly the kind of string nobody
    /// wants copied into a second place and read back by a second model.
    pub desc_len: usize,
}

/// What happened to one tool between the pins and the current list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Change {
    /// A pinned tool whose hash no longer matches.
    Changed,
    /// A tool the pins had never seen.
    Added,
    /// A pinned tool the server no longer offers.
    Removed,
}

impl Change {
    /// The one spelling used on disk, in the traffic log and on screen.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Change::Changed => "changed",
            Change::Added => "added",
            Change::Removed => "removed",
        }
    }
}

impl std::fmt::Display for Change {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One tool that moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DriftEvent {
    pub tool: String,
    pub change: Change,
    /// When the drift was first noticed, epoch milliseconds.
    pub at: u64,
    /// Byte length of the pinned description, absent for a tool that was
    /// added. Lengths and never the text — see [`ToolPin::desc_len`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc_len_before: Option<usize>,
    /// Byte length of the description the server serves now, absent for a
    /// tool that was removed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desc_len_after: Option<usize>,
}

impl DriftEvent {
    /// The event minus its timestamp, which is what "the same drift as last
    /// time" means: the clock moving is not news.
    fn shape(&self) -> (&str, Change, Option<usize>, Option<usize>) {
        (
            &self.tool,
            self.change,
            self.desc_len_before,
            self.desc_len_after,
        )
    }

    /// How the drift reads in a sentence: `echo (changed, 21 → 384 bytes)`.
    #[must_use]
    pub fn summary(&self) -> String {
        let sizes = match (self.desc_len_before, self.desc_len_after) {
            (Some(before), Some(after)) => format!(", {before} → {after} bytes"),
            (None, Some(after)) => format!(", {after} bytes"),
            (Some(before), None) => format!(", was {before} bytes"),
            (None, None) => String::new(),
        };
        format!("{} ({}{sizes})", self.tool, self.change)
    }
}

/// One tool as the gateway just saw it, ready to be pinned or compared.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFingerprint {
    pub name: String,
    pub hash: String,
    pub desc_len: usize,
}

impl ToolFingerprint {
    /// The fingerprint of an MCP tool definition.
    #[must_use]
    pub fn of(tool: &rmcp::model::Tool) -> Self {
        let description = tool.description.as_deref().unwrap_or_default();
        // Cloned into `Value`s rather than hashed off the `JsonObject`s in
        // place: the canonical writer works on one type, and a tool
        // definition is a few hundred bytes on a path that already spawns
        // processes.
        let schema = |object: &rmcp::model::JsonObject| serde_json::Value::Object(object.clone());
        Self {
            name: tool.name.to_string(),
            hash: digest(
                &tool.name,
                description,
                Some(&schema(&tool.input_schema)),
                tool.output_schema.as_deref().map(schema).as_ref(),
                tool.annotations
                    .as_ref()
                    .and_then(|annotations| serde_json::to_value(annotations).ok())
                    .as_ref(),
            ),
            desc_len: description.len(),
        }
    }
}

/// Lowercase hex SHA-256 over one tool definition.
///
/// The preimage is one line per field, each value written as JSON, so no
/// description can spell out a field separator and no schema's whitespace or
/// key order can change the result.
#[must_use]
pub fn digest(
    name: &str,
    description: &str,
    input_schema: Option<&serde_json::Value>,
    output_schema: Option<&serde_json::Value>,
    annotations: Option<&serde_json::Value>,
) -> String {
    let mut preimage = String::from("mcpgw/tool-pin/1\n");
    for (field, value) in [
        ("name", Some(&serde_json::Value::String(name.to_owned()))),
        (
            "description",
            Some(&serde_json::Value::String(description.to_owned())),
        ),
    ] {
        write_field(&mut preimage, field, value);
    }
    write_field(&mut preimage, "inputSchema", input_schema);
    write_field(&mut preimage, "outputSchema", output_schema);
    write_field(&mut preimage, "annotations", annotations);
    hex(&Sha256::digest(preimage.as_bytes()))
}

fn write_field(out: &mut String, field: &str, value: Option<&serde_json::Value>) {
    out.push_str(field);
    out.push('=');
    match value {
        Some(value) => write_canonical(value, out),
        None => out.push_str("null"),
    }
    out.push('\n');
}

/// Writes `value` as JSON with every object's keys in sorted order and no
/// whitespace anywhere.
fn write_canonical(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            // Sorted here rather than relied upon: serde_json is compiled
            // with `preserve_order`, so a `Map` keeps whatever order it was
            // parsed in and two servers writing the same schema in a
            // different key order would hash differently.
            let sorted: BTreeMap<&str, &serde_json::Value> = map
                .iter()
                .map(|(key, value)| (key.as_str(), value))
                .collect();
            out.push('{');
            for (index, (key, value)) in sorted.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&escape(key));
                out.push(':');
                write_canonical(value, out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            // Never sorted: order is meaning in an array — `required`, a
            // list of enum values — and reordering one is a real change.
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        // Scalars are already canonical in serde_json's own writer: no
        // whitespace, and strings escaped one way.
        other => out.push_str(&other.to_string()),
    }
}

fn escape(text: &str) -> String {
    serde_json::Value::String(text.to_owned()).to_string()
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Every way the current list disagrees with the pins, in tool-name order.
///
/// Pure, so the classification is testable without a filesystem or a server.
#[must_use]
pub fn compare(
    pinned: &BTreeMap<String, ToolPin>,
    current: &[ToolFingerprint],
    at: u64,
) -> Vec<DriftEvent> {
    let mut events = Vec::new();
    let seen: BTreeMap<&str, &ToolFingerprint> = current
        .iter()
        .map(|tool| (tool.name.as_str(), tool))
        .collect();
    for (name, tool) in &seen {
        match pinned.get(*name) {
            Some(pin) if pin.hash == tool.hash => {}
            Some(pin) => events.push(DriftEvent {
                tool: (*name).to_owned(),
                change: Change::Changed,
                at,
                desc_len_before: Some(pin.desc_len),
                desc_len_after: Some(tool.desc_len),
            }),
            None => events.push(DriftEvent {
                tool: (*name).to_owned(),
                change: Change::Added,
                at,
                desc_len_before: None,
                desc_len_after: Some(tool.desc_len),
            }),
        }
    }
    for (name, pin) in pinned {
        if !seen.contains_key(name.as_str()) {
            events.push(DriftEvent {
                tool: name.clone(),
                change: Change::Removed,
                at,
                desc_len_before: Some(pin.desc_len),
                desc_len_after: None,
            });
        }
    }
    events.sort_by(|a, b| a.tool.cmp(&b.tool));
    events
}

/// The pin files under one state directory.
pub struct PinStore {
    dir: PathBuf,
    /// Serializes the read-compare-write in [`PinStore::observe`]. Two
    /// clients listing the same server at once would otherwise both read the
    /// pre-drift file and both report it.
    gate: Mutex<()>,
}

impl std::fmt::Debug for PinStore {
    // By hand because the gate is a lock: a derived `Debug` would print
    // whether it happens to be held, which is noise in every error this
    // could end up in.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PinStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl PinStore {
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            gate: Mutex::new(()),
        }
    }

    /// The store under a state directory's `pins/`.
    #[must_use]
    pub fn under_state_dir(state_dir: &Path) -> Self {
        Self::new(state_dir.join(PINS_DIR))
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Where `server`'s pins live. The name is a config server name, which
    /// `config::validate_name` has already restricted to `[a-z0-9-_]`, so it
    /// is a filename on every platform.
    #[must_use]
    pub fn path(&self, server: &str) -> PathBuf {
        self.dir.join(format!("{server}.json"))
    }

    /// The pins for `server`, or `None` when it has never been pinned.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for read failures other than not-found, and
    /// [`Error::PinParse`] for a file that is not a pin file.
    pub fn read(&self, server: &str) -> Result<Option<PinFile>, Error> {
        let path = self.path(server);
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => return Err(Error::Io { path, source }),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|source| Error::PinParse {
                path,
                source: Box::new(source),
            })
    }

    /// Replaces `server`'s pin file, atomically and owner-only.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for any filesystem failure.
    pub fn write(&self, file: &PinFile) -> Result<(), Error> {
        let path = self.path(&file.server);
        let json = serde_json::to_vec_pretty(file)
            .map_err(std::io::Error::other)
            .map_err(io_err(&path))?;
        // Temp-and-rename: the gateway rewrites this file while `doctor` and
        // `mcpgw tools` read it, and half a file parses as a corrupt one.
        // Owner-only too — not a secret in itself, it holds hashes and
        // lengths and never a description, but it lives under the same rule
        // as everything else derived from the user's configs.
        crate::private::write_atomically(&path, &json).map_err(io_err(&path))
    }

    /// Forgets `server`'s pins, and says whether there were any.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for a removal that failed for anything but the file
    /// already being gone.
    pub fn remove(&self, server: &str) -> Result<bool, Error> {
        let path = self.path(server);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(Error::Io { path, source }),
        }
    }

    /// Accepts `tools` as the definitions for `server`: rewrites the pins and
    /// clears whatever had drifted.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] for any filesystem failure.
    pub fn pin(&self, server: &str, tools: &[ToolFingerprint]) -> Result<PinFile, Error> {
        let file = PinFile {
            version: VERSION,
            server: server.to_owned(),
            pinned_at: crate::capture::now_millis(),
            tools: tools
                .iter()
                .map(|tool| {
                    (
                        tool.name.clone(),
                        ToolPin {
                            hash: tool.hash.clone(),
                            desc_len: tool.desc_len,
                        },
                    )
                })
                .collect(),
            drift: Vec::new(),
        };
        self.write(&file)?;
        Ok(file)
    }

    /// Compares what `server` just listed against its pins, writing the
    /// result back, and returns the drift worth reporting.
    ///
    /// First sight pins and reports nothing: there is nothing to have moved
    /// from. Afterwards the answer is the events that are *new* — a list
    /// that disagrees the same way it disagreed a minute ago is the same
    /// unaccepted change, not a second one, and a client that polls
    /// `tools/list` would otherwise fill the traffic log with it.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] and [`Error::PinParse`] as [`PinStore::read`] and
    /// [`PinStore::write`] report them.
    pub fn observe(
        &self,
        server: &str,
        tools: &[ToolFingerprint],
    ) -> Result<Vec<DriftEvent>, Error> {
        // A poisoned gate means another thread panicked mid-compare, not
        // that the file is unusable; the next writer re-reads it anyway.
        let _guard = self
            .gate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(existing) = self.read(server)? else {
            self.pin(server, tools)?;
            return Ok(Vec::new());
        };
        // A file from a build that knows more than this one: reporting drift
        // against pins whose hash rule may have changed would be inventing
        // it, and rewriting the file would lose whatever the newer build put
        // there.
        if existing.version > VERSION {
            return Ok(Vec::new());
        }
        let events = compare(&existing.tools, tools, crate::capture::now_millis());
        let unchanged = events.len() == existing.drift.len()
            && events
                .iter()
                .zip(&existing.drift)
                .all(|(a, b)| a.shape() == b.shape());
        if unchanged {
            return Ok(Vec::new());
        }
        // Carried over rather than restamped, so "first noticed" stays the
        // first time: an event already on file keeps the `at` it was
        // recorded with.
        let events: Vec<DriftEvent> = events
            .into_iter()
            .map(|event| {
                match existing
                    .drift
                    .iter()
                    .find(|old| old.shape() == event.shape())
                {
                    Some(old) => DriftEvent {
                        at: old.at,
                        ..event
                    },
                    None => event,
                }
            })
            .collect();
        let file = PinFile {
            drift: events.clone(),
            ..existing
        };
        self.write(&file)?;
        Ok(events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema(text: &str) -> serde_json::Value {
        serde_json::from_str(text).unwrap()
    }

    fn hash_of(text: &str) -> String {
        digest("echo", "echoes input", Some(&schema(text)), None, None)
    }

    #[test]
    fn key_order_and_whitespace_do_not_change_the_hash() {
        let one = hash_of(
            r#"{"type":"object","properties":{"a":{"type":"string"},"b":{"type":"number"}}}"#,
        );
        let two = hash_of(
            "{\n  \"properties\": {\n    \"b\": { \"type\": \"number\" },\n \
             \"a\": {\"type\":\"string\"}\n  },\n  \"type\": \"object\"\n}",
        );
        assert_eq!(one, two);
    }

    #[test]
    fn array_order_does_change_it() {
        assert_ne!(
            hash_of(r#"{"required":["a","b"]}"#),
            hash_of(r#"{"required":["b","a"]}"#)
        );
    }

    #[test]
    fn every_hashed_field_moves_the_hash() {
        let base = digest("echo", "echoes input", Some(&schema("{}")), None, None);
        assert_ne!(
            base,
            digest("echo2", "echoes input", Some(&schema("{}")), None, None)
        );
        assert_ne!(
            base,
            digest("echo", "echoes INPUT", Some(&schema("{}")), None, None)
        );
        assert_ne!(
            base,
            digest(
                "echo",
                "echoes input",
                Some(&schema(r#"{"type":"object"}"#)),
                None,
                None
            )
        );
        assert_ne!(
            base,
            digest(
                "echo",
                "echoes input",
                Some(&schema("{}")),
                Some(&schema("{}")),
                None
            )
        );
        assert_ne!(
            base,
            digest(
                "echo",
                "echoes input",
                Some(&schema("{}")),
                None,
                Some(&schema(r#"{"destructiveHint":true}"#))
            )
        );
    }

    /// A field split across the boundary must not be able to impersonate the
    /// next one: the description is written as JSON, so its newlines and
    /// quotes are escaped rather than spelled out in the preimage.
    #[test]
    fn a_description_cannot_forge_a_field_separator() {
        assert_ne!(
            digest(
                "echo",
                "a\ninputSchema=null",
                Some(&schema("{}")),
                None,
                None
            ),
            digest("echo", "a", Some(&schema("{}")), None, None)
        );
    }

    #[test]
    fn the_hash_is_stable_across_runs() {
        // Pinned literally: a change to the preimage layout would silently
        // re-pin every server on every machine, which is exactly the event
        // this feature exists to make visible.
        assert_eq!(
            digest(
                "echo",
                "echoes input",
                Some(&schema(r#"{"type":"object"}"#)),
                None,
                None
            ),
            "6609d5830aad5ae1f00178316df6b185fe81799b4d362ca1a9f2199646a4f065"
        );
    }
}
