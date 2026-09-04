//! Traffic capture: one JSON object per request, appended to a daily file
//! under the state dir (`traffic/YYYY-MM-DD.jsonl`, mode 0600). The format is
//! deliberately boring — `jq`, `tail` and `mcpgw watch` all read the same
//! lines, and a rotated day is just a different filename.
//!
//! Bodies are redacted and then truncated, in that order, by
//! [`CapturePolicy`] on the way into the file — never by the call site that
//! built the record. One choke point is what makes "a secret cut in half by
//! [`MAX_BODY_BYTES`] is still a secret on disk" impossible to reintroduce
//! from a new caller.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
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
    /// `tools/list` against one upstream.
    List,
    /// `tools/call` against one upstream.
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

/// How much of a captured body a record is allowed to keep.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bodies {
    /// Metadata only: the line says what was called, by whom and how it went,
    /// and carries no arguments, no response and no error text.
    Off,
    /// Bodies with everything that looks like a credential replaced.
    Redacted,
    /// Exactly what crossed the gateway. The default for a *parsed* record,
    /// because every line written before this field existed was written this
    /// way; it is not the default for a *writer*.
    #[default]
    Full,
}

impl Bodies {
    /// The spelling used on the wire, in the config and on the banner.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Bodies::Off => "off",
            Bodies::Redacted => "redacted",
            Bodies::Full => "full",
        }
    }

    #[must_use]
    pub fn is_full(self) -> bool {
        matches!(self, Bodies::Full)
    }
}

impl std::fmt::Display for Bodies {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// Taken by reference because that is the shape `skip_serializing_if` calls
// with; `Bodies::is_full` is the by-value one every caller should reach for.
#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde calls `skip_serializing_if` with a reference to the field"
)]
fn is_full(bodies: &Bodies) -> bool {
    bodies.is_full()
}

/// What a redacted value is replaced with when none of it is worth keeping.
pub const REDACTED: &str = "[redacted]";

/// Bits of Shannon entropy per character above which a long
/// `[A-Za-z0-9_-]` run is treated as a credential.
///
/// Entropy alone does not separate secrets from prose — `the_quick_brown_fox`
/// scores higher than an md5 — so it is the last of four tests, not the only
/// one: see [`looks_like_a_secret`] for the three cheap shape guards in front
/// of it. With those in place 3.3 is comfortably below every random token
/// (base64 and hex score 3.4–4.8) and above what is left of prose.
///
/// Biased towards redacting on purpose. A false positive costs a debugging
/// clue and still says which value it was; a false negative costs a
/// credential. `--capture-bodies full` is the way out.
pub const ENTROPY_THRESHOLD: f64 = 3.3;

/// Shortest run that is considered for the entropy test at all.
const SECRET_MIN_LEN: usize = 32;

/// A run of this many consecutive lowercase letters is a word, not a
/// credential — `Quarterly` is nine, a base64 token rarely reaches four.
const WORDY_RUN: usize = 8;

/// Above this share of `-`/`_` a run is a slug (`api-v2-users-listing`),
/// which no token generator produces.
const SLUG_SEPARATORS: f64 = 0.15;

/// The credential patterns applied to every captured string, plus whatever
/// the user added under `[capture] redact`.
///
/// The built-ins are always on: they are the ones that would otherwise land
/// on disk from a header, a URL or a tool argument nobody thought of as
/// secret. User patterns are additive and never switch a built-in off.
#[derive(Debug, Clone, Default)]
pub struct RedactionRules {
    user: Vec<Regex>,
}

impl RedactionRules {
    /// The built-in rules on their own.
    #[must_use]
    pub fn builtin() -> Self {
        Self::default()
    }

    /// The built-in rules plus `patterns` from `[capture] redact`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidRedaction`], naming the pattern, when one of
    /// them is not a regex. Refusing at config-parse time rather than
    /// skipping the pattern is deliberate: a redaction rule that silently
    /// does nothing is the failure mode this whole module exists to avoid.
    pub fn compile(patterns: &[String]) -> Result<Self, Error> {
        let user = patterns
            .iter()
            .map(|pattern| {
                Regex::new(pattern).map_err(|source| Error::InvalidRedaction {
                    pattern: pattern.clone(),
                    source: Box::new(source),
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { user })
    }
}

// Compiled once per process: `redact` runs on every captured body, and
// rebuilding these per request would cost more than the JSON parse beside it.
//
// `\s` and `(?i)` are why the crate carries the two Unicode features; the
// character classes are spelled out in ASCII everywhere else.

/// `Authorization: Bearer <token>` wherever it is quoted — a header echoed
/// into an error, a curl line inside a tool response. The scheme survives so
/// the reader can still see *which* kind of credential was there.
static BEARER: LazyLock<Regex> = LazyLock::new(|| builtin(r"(?i)\b(bearer)\s+(\S+)"));

/// The same for `Basic <base64>`. Length-bounded because `basic` is an
/// ordinary English word and a short following token is far more likely to be
/// prose than a credential.
static BASIC: LazyLock<Regex> = LazyLock::new(|| builtin(r"(?i)\b(basic)\s+([A-Za-z0-9+/=]{16,})"));

/// A query parameter whose *name* says its value is a credential —
/// `?access_token=…`, `&api-key=…`. The name is kept, which is the whole
/// point: a URL that is unreadable is not much of a log line.
static QUERY: LazyLock<Regex> = LazyLock::new(|| {
    builtin(concat!(
        r#"(?i)([?&][^=&\s"']*"#,
        r"(?:token|secret|password|passwd|credential|api[-_]?key|authorization|cookie)",
        r#"[^=&\s"']*=)[^&\s"']*"#,
    ))
});

/// Issuer prefixes that identify a credential on sight, whatever it is
/// wrapped in.
static SHAPED: LazyLock<Regex> = LazyLock::new(|| {
    builtin(concat!(
        r"sk-[A-Za-z0-9_\-]{8,}",
        r"|gh[po]_[A-Za-z0-9]{16,}",
        r"|xox[abp]-[A-Za-z0-9\-]{8,}",
        r"|AKIA[0-9A-Z]{16}",
        r"|eyJ[A-Za-z0-9_\-]{4,}\.eyJ[A-Za-z0-9_\-]{4,}\.[A-Za-z0-9_\-]*",
    ))
});

/// Candidates for the entropy test: everything else a token generator emits.
static RUN: LazyLock<Regex> = LazyLock::new(|| builtin(r"[A-Za-z0-9_\-]{32,}"));

/// Compiles one of the patterns above.
///
/// The `expect` is sound and is the only one in this crate outside tests: the
/// argument is always a literal from this file, `Regex::new` is a pure
/// function of it, and every pattern here is exercised by
/// `crates/core/tests/capture.rs` — so a bad edit fails the suite long before
/// it can reach a request path.
fn builtin(pattern: &'static str) -> Regex {
    Regex::new(pattern).expect("a built-in redaction pattern must compile")
}

/// Redacts every string inside `value`, in place.
///
/// Recursion is bounded by the parser that produced `value`: bodies reach
/// this through [`serde_json::from_str`], which refuses to nest deeper than
/// 128, so a hostile upstream cannot turn a captured response into a stack
/// overflow.
pub fn redact(value: &mut serde_json::Value, rules: &RedactionRules) {
    match value {
        serde_json::Value::String(text) => *text = redact_text(text, rules),
        serde_json::Value::Array(items) => {
            for item in items {
                redact(item, rules);
            }
        }
        serde_json::Value::Object(map) => {
            for (key, item) in map.iter_mut() {
                // The key decides for the whole value: `"credentials": {…}`
                // is as much a secret as `"token": "…"`, and descending into
                // it would keep the parts that did not look like one.
                if sensitive_key(key) {
                    *item = serde_json::Value::String(REDACTED.to_owned());
                } else {
                    redact(item, rules);
                }
            }
        }
        // Numbers, booleans and null cannot carry a credential.
        _ => {}
    }
}

/// Redacts one captured body: structurally when it parses as JSON, as plain
/// text when it does not.
///
/// Both halves are needed. `args` is always a JSON object, but a response the
/// gateway could not serialize is captured in its debug form, and that string
/// carries exactly the same values.
#[must_use]
pub fn redact_body(text: &str, rules: &RedactionRules) -> String {
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(mut value) => {
            redact(&mut value, rules);
            value.to_string()
        }
        Err(_) => redact_text(text, rules),
    }
}

/// Redacts a plain string — the `error` text, or any string found inside a
/// body.
#[must_use]
pub fn redact_text(text: &str, rules: &RedactionRules) -> String {
    // Ordered widest-context-first: a `?token=` value is redacted as a query
    // parameter (keeping the parameter name) before the shape rules get a
    // chance to replace the same bytes with less context.
    let out = BEARER.replace_all(text, "${1} [redacted]");
    let out = BASIC.replace_all(&out, "${1} [redacted]");
    let out = QUERY.replace_all(&out, format!("${{1}}{REDACTED}"));
    let out = SHAPED.replace_all(&out, |found: &regex::Captures| hint(&found[0]));
    let out = RUN.replace_all(&out, |found: &regex::Captures| {
        let run = &found[0];
        if looks_like_a_secret(run) {
            hint(run)
        } else {
            run.to_owned()
        }
    });
    let mut out = out.into_owned();
    for pattern in &rules.user {
        out = pattern.replace_all(&out, REDACTED).into_owned();
    }
    out
}

/// A redacted token, keeping four leading characters so the reader can still
/// tell *which* credential was there without being handed any of it.
fn hint(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("[redacted:{head}…]")
}

/// Whether a JSON key names a value that is a credential by definition.
fn sensitive_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if matches!(key.as_str(), "authorization" | "cookie" | "set-cookie") {
        return true;
    }
    if ["token", "secret", "password", "passwd", "credential"]
        .iter()
        .any(|needle| key.contains(needle))
    {
        return true;
    }
    // `api[-_]?key`, without paying for a regex on a path that runs once per
    // key of every captured body.
    key.match_indices("key").any(|(at, _)| {
        let head = key[..at].trim_end_matches(['-', '_']);
        head.ends_with("api")
    })
}

/// Whether a long `[A-Za-z0-9_-]` run is a credential rather than an
/// identifier somebody named by hand.
///
/// Four tests, cheapest first, and all four have to agree:
///
/// 1. it mixes at least two of lowercase, uppercase and digits — a run of
///    plain lowercase words is prose whatever its entropy;
/// 2. it has no run of [`WORDY_RUN`] lowercase letters, which is what an
///    English word looks like and what a token generator does not produce;
/// 3. under [`SLUG_SEPARATORS`] of it is `-`/`_`, so URL slugs are left alone;
/// 4. its Shannon entropy clears [`ENTROPY_THRESHOLD`].
fn looks_like_a_secret(run: &str) -> bool {
    if run.len() < SECRET_MIN_LEN {
        return false;
    }
    let mut lower = false;
    let mut upper = false;
    let mut digit = false;
    let mut separators = 0usize;
    let mut lower_run = 0usize;
    let mut longest_lower_run = 0usize;
    for byte in run.bytes() {
        match byte {
            b'a'..=b'z' => {
                lower = true;
                lower_run += 1;
                longest_lower_run = longest_lower_run.max(lower_run);
            }
            b'A'..=b'Z' => {
                upper = true;
                lower_run = 0;
            }
            b'0'..=b'9' => {
                digit = true;
                lower_run = 0;
            }
            _ => {
                separators += 1;
                lower_run = 0;
            }
        }
    }
    let classes = usize::from(lower) + usize::from(upper) + usize::from(digit);
    if classes < 2 || longest_lower_run >= WORDY_RUN {
        return false;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "a ratio of two run lengths; the run is bytes long, not petabytes"
    )]
    let separator_share = separators as f64 / run.len() as f64;
    separator_share < SLUG_SEPARATORS && entropy(run) >= ENTROPY_THRESHOLD
}

/// Shannon entropy of `text` in bits per character, over the bytes it
/// actually contains. Only ever called on `[A-Za-z0-9_-]` runs, so bytes and
/// characters are the same thing here.
#[must_use]
pub fn entropy(text: &str) -> f64 {
    if text.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for byte in text.bytes() {
        counts[usize::from(byte)] += 1;
    }
    #[expect(
        clippy::cast_precision_loss,
        reason = "counts and lengths of one short run, far inside f64's exact range"
    )]
    let total = text.len() as f64;
    counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            #[expect(
                clippy::cast_precision_loss,
                reason = "see above: one run's character count"
            )]
            let share = *count as f64 / total;
            -share * share.log2()
        })
        .sum()
}

/// What a writer is allowed to put on disk: how much of a body it keeps, and
/// the rules it redacts what is left with.
///
/// Applied in the writer rather than at the call sites that build records, so
/// redaction always happens before truncation and no future call site can get
/// that order wrong.
#[derive(Debug, Clone)]
pub struct CapturePolicy {
    bodies: Bodies,
    rules: RedactionRules,
}

impl Default for CapturePolicy {
    /// [`Bodies::Redacted`] with the built-in rules. The safe mode is the one
    /// you get by not choosing, which is the opposite of what the field
    /// default on [`CaptureRecord::bodies`] means — that one describes lines
    /// already on disk.
    fn default() -> Self {
        Self {
            bodies: Bodies::Redacted,
            rules: RedactionRules::builtin(),
        }
    }
}

impl CapturePolicy {
    #[must_use]
    pub fn new(bodies: Bodies, rules: RedactionRules) -> Self {
        Self { bodies, rules }
    }

    /// Keeps bodies exactly as they crossed the gateway.
    #[must_use]
    pub fn full() -> Self {
        Self::new(Bodies::Full, RedactionRules::builtin())
    }

    #[must_use]
    pub fn bodies(&self) -> Bodies {
        self.bodies
    }

    /// `record` as it should be stored, or `None` when it is already storable
    /// as it stands.
    ///
    /// `None` is the common case for the list families under `full`, and it
    /// is why a record carrying no body costs no allocation on the request
    /// path.
    fn applied(&self, record: &CaptureRecord) -> Option<CaptureRecord> {
        if record.args.is_none() && record.response.is_none() && record.error.is_none() {
            return None;
        }
        let mut out = record.clone();
        out.bodies = self.bodies;
        if self.bodies == Bodies::Off {
            out.args = None;
            out.response = None;
            out.error = None;
            return Some(out);
        }
        let redacting = self.bodies == Bodies::Redacted;
        for text in [&mut out.args, &mut out.response].into_iter().flatten() {
            if redacting {
                *text = redact_body(text, &self.rules);
            }
            *text = truncate(text);
        }
        if let Some(error) = &mut out.error {
            if redacting {
                *error = redact_text(error, &self.rules);
            }
            *error = truncate(error);
        }
        Some(out)
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
    /// Which downstream client the request came from — the fingerprint of the
    /// transport session when there is one, otherwise the gateway process's
    /// own id. See [`CaptureWriter::session`] for what each means.
    pub session: String,
    /// Which client software made the request: `<name>/<version>` from the
    /// identity the client put on it, or bare `<name>` when it named no
    /// version.
    ///
    /// The companion to `session`, not a replacement for it: `session` says
    /// which connection, this says which program. Two windows of one editor
    /// share a client and not a session; a client on MCP 2026-07-28 has no
    /// session to share at all, which is the case this field exists for.
    ///
    /// Absent means absent. Naming yourself is a SHOULD in the protocol, so a
    /// client that does not is left unattributed rather than filed under a
    /// guess — and every line written before the field existed keeps parsing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
    /// Which face of the gateway took the request: `s/<server>`, which is
    /// the only face that reaches a server. Absent on lines written before
    /// this field existed and on the stdio face, which has no path — and
    /// `mcp` on lines a 0.4 gateway wrote, when the base endpoint still
    /// served an aggregate of its own.
    ///
    /// Additive and optional on purpose: every JSONL line already on disk has
    /// to keep parsing, and a `jq` filter written against the old shape has to
    /// keep working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
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
    /// How much of `args`, `response` and `error` this line was allowed to
    /// keep, so a reader knows whether it still has to mask them itself.
    ///
    /// Written only when it is not [`Bodies::Full`]: every line already on
    /// disk was written under the old, unredacted behaviour, and defaulting
    /// the absent field to `full` is what makes those lines keep their
    /// meaning instead of silently claiming to be safe.
    #[serde(default, skip_serializing_if = "is_full")]
    pub bodies: Bodies,
}

impl CaptureRecord {
    /// A successful record stamped with the current wall clock.
    #[must_use]
    pub fn new(session: &str, server: &str, kind: Kind, duration: Duration) -> Self {
        Self {
            ts: now_millis(),
            session: session.to_owned(),
            client: None,
            endpoint: None,
            server: server.to_owned(),
            tool: None,
            kind,
            duration_ms: u64::try_from(duration.as_millis()).unwrap_or(u64::MAX),
            ok: true,
            error: None,
            args: None,
            response: None,
            bodies: Bodies::Full,
        }
    }

    /// Attributes the record to the gateway face that took the request, e.g.
    /// `s/github` or `mcp`.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
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

    /// Marks the record failed and stores the error text verbatim.
    ///
    /// Verbatim because an upstream error routinely echoes the URL or header
    /// it failed on: redacting and truncating it is [`CapturePolicy`]'s job,
    /// and doing either here would cut the text before anything had looked
    /// for a credential in it.
    #[must_use]
    pub fn with_error(mut self, error: &str) -> Self {
        self.ok = false;
        self.error = Some(error.to_owned());
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

/// Serializes `value` compactly for storage in a record.
///
/// Whole, not truncated: [`CapturePolicy`] redacts first and cuts second, and
/// a body already cut here would hand it half a credential to work with.
#[must_use]
pub fn body(value: &serde_json::Value) -> String {
    value.to_string()
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
    policy: CapturePolicy,
}

impl CaptureWriter {
    /// A writer over `dir`, with a fresh session id and the default
    /// [`CapturePolicy`] — redacting, because a writer nobody configured must
    /// not be the one that puts a credential on disk.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            session: new_session_id(),
            policy: CapturePolicy::default(),
        }
    }

    /// Writes under `policy` instead of the default one.
    #[must_use]
    pub fn with_policy(mut self, policy: CapturePolicy) -> Self {
        self.policy = policy;
        self
    }

    #[must_use]
    pub fn policy(&self) -> &CapturePolicy {
        &self.policy
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

    /// Identifies this gateway process. Records fall back to it when the
    /// downstream transport offers no session of its own — a stdio face, or
    /// an HTTP client on MCP 2026-07-28, which removed protocol sessions
    /// (SEP-2567). Traffic attributed to it is "this gateway run", not "this
    /// client": a long-lived daemon serving several harnesses cannot tell
    /// them apart through this id.
    ///
    /// When the transport does hand out a session id, records carry
    /// [`session_fingerprint`] of it instead.
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

        // The one place a body is redacted and cut, and in that order — see
        // the module header.
        let stored = self.policy.applied(record);
        let record = stored.as_ref().unwrap_or(record);

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

/// A stable short id for a downstream transport session, in the same 8-hex
/// shape as [`new_session_id`] so a reader never has to care which kind of id
/// a line carries.
///
/// The raw id is *not* stored. A Streamable HTTP session id is a bearer
/// credential — whoever presents it speaks as that session — and the traffic
/// log is a file people `cat`, paste into issues and grep in front of others.
/// A digest keeps the one property attribution needs (equal ids mean the same
/// session, different ids mean different clients) while putting nothing
/// replayable on disk. It is not a secret-grade hash and is not meant to be:
/// the input is a v4 UUID, so there is no dictionary to walk back through it.
#[must_use]
pub fn session_fingerprint(session_id: &str) -> String {
    // FNV-1a, 64-bit: a few lines, no dependency, and well-distributed over
    // short ASCII inputs, which is all a display id needs.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in session_id.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Folded to 32 bits so the id is the same width as the per-process one.
    format!("{:08x}", (hash ^ (hash >> 32)) & 0xffff_ffff)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(text: &str) -> String {
        redact_text(text, &RedactionRules::builtin())
    }

    fn clean_json(text: &str) -> String {
        redact_body(text, &RedactionRules::builtin())
    }

    #[test]
    fn every_built_in_pattern_compiles() {
        // Forces each `LazyLock`, which is what makes the `expect` inside
        // `builtin` a suite failure rather than a runtime one.
        for pattern in [&*BEARER, &*BASIC, &*QUERY, &*SHAPED, &*RUN] {
            assert!(!pattern.as_str().is_empty());
        }
    }

    #[test]
    fn sensitive_keys_are_matched_whatever_their_spelling() {
        for key in [
            "authorization",
            "Authorization",
            "AUTHORIZATION",
            "cookie",
            "Set-Cookie",
            "token",
            "access_token",
            "refreshToken",
            "client_secret",
            "password",
            "PASSWD",
            "apiKey",
            "api_key",
            "API-KEY",
            "x-api-key",
            "credentials",
        ] {
            assert!(sensitive_key(key), "{key} should be sensitive");
        }
        for key in [
            "title",
            "message",
            "keyboard",
            "monkey",
            "keys",
            "author",
            "tokenizer_name",
        ] {
            // `tokenizer_name` contains "token" and is deliberately caught:
            // the rule is a substring one, and over-redacting a field name
            // beats reasoning about which "token" is which.
            let expected = key.contains("token");
            assert_eq!(sensitive_key(key), expected, "{key}");
        }
    }

    #[test]
    fn a_sensitive_key_takes_its_whole_value() {
        let mut value: serde_json::Value =
            serde_json::from_str(r#"{"credentials":{"user":"me","pass":"hunter2"}}"#).unwrap();
        redact(&mut value, &RedactionRules::builtin());
        assert_eq!(value["credentials"], REDACTED);
    }

    #[test]
    fn nested_objects_and_arrays_are_reached() {
        let redacted =
            clean_json(r#"{"steps":[{"headers":{"Authorization":"Bearer abc"}},{"note":"fine"}]}"#);
        assert!(!redacted.contains("abc"), "{redacted}");
        assert!(redacted.contains("fine"), "{redacted}");
    }

    #[test]
    fn bearer_and_basic_keep_their_scheme_and_lose_their_credential() {
        let bearer = clean("Authorization: Bearer sk-live-9f2e1c5d7b3a0e64");
        assert_eq!(bearer, "Authorization: Bearer [redacted]");
        let basic = clean("Proxy-Authorization: Basic dXNlcjpodW50ZXIyMTIzNDU2Nw==");
        assert_eq!(basic, "Proxy-Authorization: Basic [redacted]");
        // Short enough to be prose rather than base64, so `basic` alone is
        // not a reason to eat the next word.
        assert_eq!(clean("a basic example"), "a basic example");
    }

    #[test]
    fn issuer_prefixes_are_replaced_but_still_name_themselves() {
        for (raw, hint) in [
            ("sk-proj-abc123XYZ456def789", "[redacted:sk-p…]"),
            ("ghp_0123456789abcdefghij", "[redacted:ghp_…]"),
            ("gho_0123456789abcdefghij", "[redacted:gho_…]"),
            ("xoxb-123456789012-abcdefgh", "[redacted:xoxb…]"),
            ("xoxp-123456789012-abcdefgh", "[redacted:xoxp…]"),
            ("AKIAIOSFODNN7EXAMPLE", "[redacted:AKIA…]"),
            (
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NSJ9.dBjftJeZ4CVP",
                "[redacted:eyJh…]",
            ),
        ] {
            assert_eq!(
                clean(&format!("value={raw}")),
                format!("value={hint}"),
                "{raw}"
            );
        }
    }

    #[test]
    fn high_entropy_runs_go_and_identifiers_stay() {
        for secret in [
            "wJalrXUtnFEMIK7MDENGbPxRfiCYEXAMPLEKEY",
            "5f2b9e6c1a7d4f3b8c0e2a6d9b4f7c1e3a5d8b0f",
            "Y2xpZW50X3NlY3JldF9hYmNkZWZnaGlqa2xtbg",
        ] {
            assert!(looks_like_a_secret(secret), "{secret} should be redacted");
        }
        for ordinary in [
            // Words: prose whatever their entropy.
            "the_quick_brown_fox_jumps_over_lazy_dogs",
            "create_issue_with_a_very_long_name_here_x",
            "file_system_watcher_configuration_value",
            // A slug: too many separators for a token generator.
            "https-example-com-api-v2-users-listing",
            // CamelCase prose: the lowercase runs are words.
            "MyDocumentTitleForTheQuarterlyReport2026",
            // Repetition is the opposite of entropy.
            &"a1".repeat(20),
            // Shorter than the floor, so never even measured.
            "abcdefghijklmnopqrstuvwxyz01234",
        ] {
            assert!(!looks_like_a_secret(ordinary), "{ordinary} should survive");
        }
    }

    #[test]
    fn entropy_is_bits_per_character() {
        assert!((entropy("aaaa") - 0.0).abs() < f64::EPSILON);
        assert!((entropy("ab") - 1.0).abs() < f64::EPSILON);
        assert!((entropy("abcd") - 2.0).abs() < f64::EPSILON);
        assert!((entropy("") - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn url_query_values_go_and_their_names_stay() {
        let redacted =
            clean("upstream https://api.example.com/v1?user=me&access_token=s3cr3t3 said no");
        assert_eq!(
            redacted,
            "upstream https://api.example.com/v1?user=me&access_token=[redacted] said no"
        );
    }

    #[test]
    fn an_error_string_is_redacted_like_any_other_body() {
        let error = "GET https://api.example.com/mcp?api-key=abcd1234 failed: Bearer ghp_0123456789abcdefghij rejected";
        let redacted = clean(error);
        assert!(!redacted.contains("abcd1234"), "{redacted}");
        assert!(!redacted.contains("ghp_0123456789abcdefghij"), "{redacted}");
        // Still says which request failed, which is why anyone reads it.
        assert!(
            redacted.contains("https://api.example.com/mcp"),
            "{redacted}"
        );
    }

    #[test]
    fn user_patterns_add_to_the_built_in_ones() {
        let rules = RedactionRules::compile(&["ACME-[0-9]{4}".to_owned()]).unwrap();
        let redacted = redact_text("ticket ACME-4711 with Bearer abc", &rules);
        assert_eq!(redacted, "ticket [redacted] with Bearer [redacted]");
    }

    #[test]
    fn an_unusable_user_pattern_names_itself() {
        let err = RedactionRules::compile(&["(unclosed".to_owned()]).unwrap_err();
        assert!(err.to_string().contains("(unclosed"), "{err}");
        assert!(matches!(err, Error::InvalidRedaction { .. }));
    }

    #[test]
    fn a_body_that_is_not_json_still_gets_the_string_rules() {
        // What a response the gateway could not serialize is captured as.
        let redacted = clean_json(r#"Elicitation { prompt: "Bearer ghp_0123456789abcdefghij" }"#);
        assert!(!redacted.contains("ghp_0123456789abcdefghij"), "{redacted}");
    }

    #[test]
    fn redaction_runs_before_truncation() {
        // The secret sits past MAX_BODY_BYTES, where a policy that cut first
        // would leave half of it — or all of it, in the half it kept.
        let filler = "x".repeat(MAX_BODY_BYTES);
        let args = serde_json::json!({"filler": filler, "token": "ghp_0123456789abcdefghij"});
        let policy = CapturePolicy::default();
        let record = CaptureRecord::new("s", "fx", Kind::Call, Duration::ZERO)
            .with_args(body(&args))
            .with_error("Bearer ghp_0123456789abcdefghij");
        let stored = policy.applied(&record).unwrap();

        let stored_args = stored.args.as_deref().unwrap();
        assert!(stored_args.ends_with(TRUNCATION_MARKER), "{stored_args}");
        assert!(!stored_args.contains("ghp_0123456789abcdefghij"));
        assert!(!stored.error.as_deref().unwrap().contains("ghp_0"));
        assert_eq!(stored.bodies, Bodies::Redacted);
    }

    #[test]
    fn off_keeps_the_metadata_and_none_of_the_bodies() {
        let policy = CapturePolicy::new(Bodies::Off, RedactionRules::builtin());
        let record = CaptureRecord::new("s", "fx", Kind::Call, Duration::from_millis(3))
            .with_tool("echo")
            .with_args(r#"{"message":"hi"}"#.to_owned())
            .with_error("refused");
        let stored = policy.applied(&record).unwrap();
        assert_eq!(stored.bodies, Bodies::Off);
        assert_eq!(stored.args, None);
        assert_eq!(stored.response, None);
        assert_eq!(stored.error, None);
        // Everything the stream is filtered on survives.
        assert_eq!(stored.tool.as_deref(), Some("echo"));
        assert_eq!(stored.duration_ms, 3);
        assert!(!stored.ok);
    }

    #[test]
    fn full_truncates_and_changes_nothing_else() {
        let policy = CapturePolicy::full();
        let record = CaptureRecord::new("s", "fx", Kind::Call, Duration::ZERO)
            .with_args(r#"{"token":"ghp_0123456789abcdefghij"}"#.to_owned());
        let stored = policy.applied(&record).unwrap();
        assert_eq!(stored.args, record.args);
        assert!(stored.bodies.is_full());
    }

    #[test]
    fn a_record_with_no_body_costs_nothing() {
        let record = CaptureRecord::new("s", "fx", Kind::List, Duration::ZERO);
        assert!(CapturePolicy::default().applied(&record).is_none());
    }
}
