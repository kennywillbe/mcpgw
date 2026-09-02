//! Traffic capture: one JSON object per request, appended to a daily file
//! under the state dir (`traffic/YYYY-MM-DD.jsonl`, mode 0600). The format is
//! deliberately boring — `jq`, `tail` and `mcpgw watch` all read the same
//! lines, and a rotated day is just a different filename.
//!
//! Bodies are truncated, never redacted: redaction lands with the security
//! wave, so until then the honest story is "arguments and responses are in
//! this file, capped at [`MAX_BODY_BYTES`], readable only by you".

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::Error;

/// Subdirectory of the state dir holding the daily JSONL files.
pub const TRAFFIC_DIR: &str = "traffic";

/// Cap for captured request arguments and response bodies.
pub const MAX_BODY_BYTES: usize = 2048;

/// Appended to a body that hit [`MAX_BODY_BYTES`], so a truncated line is
/// never mistaken for a complete one.
pub const TRUNCATION_MARKER: &str = "…[truncated]";

/// Which upstream request a record describes.
///
/// A family gets its own variant rather than being folded into a neighbour:
/// a reader that meets a kind it does not know fails loudly on that one line,
/// where a re-used kind would silently mis-group the traffic. Adding variants
/// is the additive kind of change the format allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// `tools/list` against one upstream (in aggregate mode: one per upstream).
    List,
    /// `tools/call` against the resolved upstream.
    Call,
    /// `resources/list`, forwarded by a pipe.
    Resources,
    /// `resources/templates/list`, forwarded by a pipe.
    #[serde(rename = "resource_templates")]
    ResourceTemplates,
    /// `resources/read`, forwarded by a pipe; the URI is in `tool`.
    #[serde(rename = "resource_read")]
    ResourceRead,
    /// `prompts/list`, forwarded by a pipe.
    Prompts,
    /// `prompts/get`, forwarded by a pipe; the prompt name is in `tool`.
    #[serde(rename = "prompt_get")]
    PromptGet,
    /// `completion/complete`, forwarded by a pipe; the argument name is in
    /// `tool`.
    Complete,
}

impl Kind {
    /// The MCP method this kind records, for anything rendering a record back
    /// to a human.
    #[must_use]
    pub fn method(self) -> &'static str {
        match self {
            Kind::List => "tools/list",
            Kind::Call => "tools/call",
            Kind::Resources => "resources/list",
            Kind::ResourceTemplates => "resources/templates/list",
            Kind::ResourceRead => "resources/read",
            Kind::Prompts => "prompts/list",
            Kind::PromptGet => "prompts/get",
            Kind::Complete => "completion/complete",
        }
    }
}

/// One captured upstream request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRecord {
    /// When the request finished, in milliseconds since the Unix epoch (the
    /// request started `duration_ms` earlier). Epoch millis rather than
    /// RFC 3339 keeps the record dependency-free and lets `watch` compute
    /// ages by subtraction instead of parsing.
    pub ts: u64,
    pub session: String,
    pub server: String,
    /// What the request named: the tool, the prompt, the resource URI or the
    /// argument being completed. Absent for the list families, which name
    /// nothing. One field rather than one per family, so `watch --tool` and
    /// every `jq` line people already wrote keep working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub kind: Kind,
    pub duration_ms: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub args: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<String>,
}

impl CaptureRecord {
    /// A successful record stamped with the current wall clock.
    #[must_use]
    pub fn new(session: &str, server: &str, kind: Kind, duration: Duration) -> Self {
        Self {
            ts: now_millis(),
            session: session.to_owned(),
            server: server.to_owned(),
            tool: None,
            kind,
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            ok: true,
            error: None,
            args: None,
            response: None,
        }
    }

    #[must_use]
    pub fn with_tool(mut self, tool: impl Into<String>) -> Self {
        self.tool = Some(tool.into());
        self
    }

    #[must_use]
    pub fn with_args(mut self, args: String) -> Self {
        self.args = Some(args);
        self
    }

    #[must_use]
    pub fn with_response(mut self, response: String) -> Self {
        self.response = Some(response);
        self
    }

    /// Marks the record failed and stores the (truncated) error text.
    #[must_use]
    pub fn with_error(mut self, error: &str) -> Self {
        self.ok = false;
        self.error = Some(truncate(error));
        self
    }
}

/// Truncates `text` to [`MAX_BODY_BYTES`], appending [`TRUNCATION_MARKER`]
/// when it had to cut.
#[must_use]
pub fn truncate(text: &str) -> String {
    if text.len() <= MAX_BODY_BYTES {
        return text.to_owned();
    }
    // `str::floor_char_boundary` is still unstable, so walk back to one by
    // hand: slicing mid-codepoint would panic on any multibyte body.
    let mut end = MAX_BODY_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{TRUNCATION_MARKER}", &text[..end])
}

/// Serializes `value` compactly and truncates it for storage in a record.
#[must_use]
pub fn body(value: &serde_json::Value) -> String {
    truncate(&value.to_string())
}

/// The daily file a record stamped `ts_millis` belongs in.
///
/// Days are UTC: computing a local date needs a time zone database that
/// `std` does not carry, and a writer and a reader disagreeing about
/// "today" would be worse than a boundary that moves with the offset.
#[must_use]
pub fn daily_path(dir: &Path, ts_millis: u64) -> PathBuf {
    let (year, month, day) = utc_date(ts_millis);
    dir.join(format!("{year:04}-{month:02}-{day:02}.jsonl"))
}

/// Milliseconds since the Unix epoch; 0 if the clock is before it.
#[must_use]
pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn utc_date(ts_millis: u64) -> (i64, i64, i64) {
    let days = i64::try_from(ts_millis / 86_400_000).unwrap_or(0);
    civil_from_days(days)
}

/// Howard Hinnant's `civil_from_days`: days since 1970-01-01 to (y, m, d),
/// era-based and branch-free. Cheaper than a date crate for the one thing
/// capture needs a calendar for — naming a file.
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_shifted = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_shifted + 2) / 5 + 1;
    let month = if month_shifted < 10 {
        month_shifted + 3
    } else {
        month_shifted - 9
    };
    (if month <= 2 { year + 1 } else { year }, month, day)
}

/// Appends records to the daily file in one traffic directory. Every append
/// re-resolves the filename, so a writer created before midnight keeps
/// writing to the right file after it — that is the whole of rotation.
pub struct CaptureWriter {
    dir: PathBuf,
    session: String,
}

impl CaptureWriter {
    /// A writer over `dir`, with a fresh session id.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            session: new_session_id(),
        }
    }

    /// The traffic dir under a state dir, the layout `serve` and `watch` share.
    #[must_use]
    pub fn under_state_dir(state_dir: &Path) -> Self {
        Self::new(state_dir.join(TRAFFIC_DIR))
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Identifies this gateway process in every record it writes. Splitting
    /// it per downstream client is a later refinement; the gateway does not
    /// thread session identity through its handlers yet.
    #[must_use]
    pub fn session(&self) -> &str {
        &self.session
    }

    /// Appends `record` as one JSON line to today's file, creating the
    /// directory and the file (0600 on unix) as needed.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the line cannot be written.
    pub fn append(&self, record: &CaptureRecord) -> Result<(), Error> {
        use std::io::Write as _;

        let path = daily_path(&self.dir, record.ts);
        let mut line = serde_json::to_string(record).unwrap_or_else(|err| {
            // A record is plain owned scalars; serialization can only fail
            // if serde_json itself does, and losing the line beats failing.
            format!(r#"{{"error":"unserializable capture record: {err}"}}"#)
        });
        line.push('\n');

        let mut file = match open(&path) {
            Ok(file) => file,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                crate::private::create_dir_all(&self.dir).map_err(io_err(&self.dir))?;
                open(&path).map_err(io_err(&path))?
            }
            Err(err) => return Err(io_err(&path)(err)),
        };
        file.write_all(line.as_bytes()).map_err(io_err(&path))?;
        file.flush().map_err(io_err(&path))
    }
}

fn open(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        // Captured bodies are as sensitive as the traffic they carry; the
        // mode only applies when this call creates the file.
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.to_owned();
    move |source| Error::Io { path, source }
}

/// Short, collision-unlikely id from the clock and the pid — enough to tell
/// two gateway runs apart in one file without pulling in a uuid crate.
fn new_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let low = u64::try_from(nanos & u128::from(u64::MAX)).unwrap_or_default();
    let mixed = (low.rotate_left(17) ^ (u64::from(std::process::id()) << 24)) & 0xffff_ffff;
    format!("{mixed:08x}")
}
