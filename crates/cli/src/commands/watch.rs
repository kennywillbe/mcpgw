//! `mcpgw watch`: a live view of the capture log. Tailing the JSONL file
//! keeps this decoupled from the gateway — there is no socket to connect to,
//! watching works on a gateway started before it, and the same loop replays
//! history it finds in today's file before following new lines.
//!
//! `--tui` draws the same records as three panes instead of a line stream.
//! It reads through the same [`Follow`], so there is one piece of tailing
//! logic in the tool and the two views cannot disagree about what arrived,
//! what a rollover is, or how often to look.

mod state;
mod tui;

use std::fmt::Write as _;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use mcpgw_core::capture::{CaptureRecord, Kind, daily_path, now_millis};
use owo_colors::OwoColorize as _;

/// How often the file is checked for appended lines. Fast enough to feel
/// live, cheap enough to leave running all day.
const POLL: Duration = Duration::from_millis(500);

/// Error text longer than this is cut for the one-line view; the full text
/// stays in the file and in `--json`.
const ERROR_CHARS: usize = 100;

/// The same for a client name. Nothing sane is this long, but the value is
/// whatever a downstream client chose to call itself, and one that chose a
/// paragraph must not be able to push the error off the line.
const CLIENT_CHARS: usize = 32;

/// What a masked captured body renders as, matching `list --json`.
const MASK: &str = "***";

#[derive(clap::Args)]
pub struct WatchArgs {
    /// Only show traffic for this server
    #[arg(long, value_name = "NAME")]
    pub server: Option<String>,
    /// Only show traffic for this tool (bare name, without the server prefix)
    #[arg(long, value_name = "NAME")]
    pub tool: Option<String>,
    /// Only show traffic that arrived on this gateway endpoint (e.g. s/github
    /// or mcp; a leading slash is accepted)
    #[arg(long, value_name = "ENDPOINT")]
    pub endpoint: Option<String>,
    /// Only show traffic from this downstream session (the `session` field of
    /// a captured line)
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,
    /// Only show traffic from this client, matched as a case-insensitive
    /// substring of the `client` field (e.g. claude or cursor)
    #[arg(long, value_name = "NAME")]
    pub client: Option<String>,
    /// Stream the JSONL lines instead of the rendered stream
    #[arg(long)]
    pub json: bool,
    /// Open the full-screen terminal UI instead of the line stream: a live
    /// per-server table, a scrolling call log and a detail pane
    #[arg(long, conflicts_with = "json")]
    pub tui: bool,
    /// Print captured arguments and responses instead of masking them. Only
    /// changes anything for lines a gateway captured with
    /// `--capture-bodies full`
    #[arg(long)]
    pub show_secrets: bool,
}

pub fn run(args: &WatchArgs, color: bool) -> anyhow::Result<()> {
    let state_dir = mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory to resolve the state directory")?;
    let dir = state_dir.join(mcpgw_core::capture::TRAFFIC_DIR);
    if args.tui {
        return tui::run(args, dir);
    }
    if !args.json {
        println!("watching {} (Ctrl-C to stop)", dir.display());
    }

    let filters = Filters::new(args);
    let mut follow = Follow::new(dir);
    // Said at most once per run: a stream full of redacted lines would
    // otherwise repeat it for every one of them.
    let mut said_nothing_to_reveal = false;
    loop {
        for line in poll_or_report(&mut follow) {
            let Ok(record) = serde_json::from_str::<CaptureRecord>(&line) else {
                // A line the current build cannot parse (older or newer
                // format) is skipped rather than ending the stream.
                continue;
            };
            if !filters.matches(&record) {
                continue;
            }
            if args.json {
                if args.show_secrets && !record.bodies.is_full() && !said_nothing_to_reveal {
                    eprintln!(
                        "watch: these lines were captured with --capture-bodies {} — \
                         --show-secrets has nothing left to reveal in them",
                        record.bodies
                    );
                    said_nothing_to_reveal = true;
                }
                println!("{}", json_stream_line(&record, &line, args.show_secrets));
            } else {
                println!("{}", render_line(&record, now_millis(), color));
            }
        }
        std::thread::sleep(POLL);
    }
}

/// One follow round. A read failure is reported and swallowed rather than
/// propagated: a watch is meant to be left running all day, and a single
/// EACCES or a stat caught mid-rotation must not end it. The next poll is
/// 500ms away, so retrying costs nothing and only Ctrl-C stops the stream.
fn poll_or_report(follow: &mut Follow) -> Vec<String> {
    match follow.poll() {
        Ok(lines) => lines,
        Err(err) => {
            eprintln!("watch: {err:#} — retrying");
            Vec::new()
        }
    }
}

/// Follows the traffic directory: today's file, and tomorrow's when the day
/// turns over.
///
/// The one place either view of `watch` reads from. A second tailing loop for
/// the TUI is exactly the kind of duplicate that ends with the two views
/// disagreeing about what a rollover is — and about whether a line arrived at
/// all, since two readers of the same file each keep their own offset.
struct Follow {
    dir: PathBuf,
    tail: Tail,
}

impl Follow {
    fn new(dir: PathBuf) -> Self {
        let tail = Tail::new(daily_path(&dir, now_millis()));
        Self { dir, tail }
    }

    /// Complete lines appended since the last poll, following the file the
    /// gateway is writing to *now*: at midnight it starts a new one and the
    /// tail moves across without needing to be restarted.
    fn poll(&mut self) -> anyhow::Result<Vec<String>> {
        let path = daily_path(&self.dir, now_millis());
        if path != self.tail.path {
            self.tail = Tail::new(path);
        }
        self.tail.poll()
    }
}

/// Follows one file by byte offset.
struct Tail {
    path: PathBuf,
    offset: u64,
}

impl Tail {
    fn new(path: PathBuf) -> Self {
        Self { path, offset: 0 }
    }

    /// Complete lines appended since the last poll.
    fn poll(&mut self) -> anyhow::Result<Vec<String>> {
        let mut file = match std::fs::File::open(&self.path) {
            Ok(file) => file,
            // Nothing captured today yet: not an error, just nothing to show.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => {
                return Err(err).with_context(|| format!("cannot read {}", self.path.display()));
            }
        };
        let len = file
            .metadata()
            .with_context(|| format!("cannot stat {}", self.path.display()))?
            .len();
        // A shrunken file was rotated or truncated under us; re-read it.
        if len < self.offset {
            self.offset = 0;
        }
        file.seek(SeekFrom::Start(self.offset))?;
        let mut buffer = Vec::new();
        file.read_to_end(&mut buffer)?;

        let (lines, consumed) = complete_lines(&buffer);
        self.offset += consumed;
        Ok(lines)
    }
}

/// Splits off the whole lines in `buffer`, returning them and how many bytes
/// they occupied. A trailing partial line is left for the next poll: an
/// append is not atomic, so the reader can easily catch half of one.
fn complete_lines(buffer: &[u8]) -> (Vec<String>, u64) {
    let Some(last) = buffer.iter().rposition(|byte| *byte == b'\n') else {
        return (Vec::new(), 0);
    };
    let complete = &buffer[..=last];
    let lines = String::from_utf8_lossy(complete)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(str::to_owned)
        .collect();
    (lines, complete.len() as u64)
}

/// One line of the `--json` stream: the raw line, or the same record with its
/// captured bodies masked.
///
/// The human stream renders age, outcome, target, latency and error only, so
/// it has nothing to mask; `--json` is the path that puts captured bodies on
/// stdout, and the only one that has to decide.
///
/// A line the gateway already redacted goes out as it was written. Masking it
/// a second time would hide the shapes redaction deliberately left legible —
/// `[redacted:ghp_…]`, the key an argument was under — while revealing
/// nothing by not doing so.
fn json_stream_line(record: &CaptureRecord, raw: &str, show_secrets: bool) -> String {
    if show_secrets || !record.bodies.is_full() {
        raw.to_owned()
    } else {
        json_line(record)
    }
}

/// One line of the `--json` stream with the captured bodies masked.
///
/// `args` and `response` are whole truncated JSON blobs, and a secret can sit
/// anywhere inside one — there is no key-level redaction that would hold for
/// arbitrary tool schemas, so the value goes as a unit. Everything a reader
/// actually filters and aggregates on (server, tool, timing, outcome, error)
/// survives. `--show-secrets` is the opt-out, as in `list --json`.
///
/// An absent body stays absent rather than becoming a mask, so the shape of
/// a `tools/list` record is unchanged and consumers can still tell "no
/// arguments" from "arguments withheld".
fn json_line(record: &CaptureRecord) -> String {
    let mut record = record.clone();
    if record.args.is_some() {
        record.args = Some(MASK.to_owned());
    }
    if record.response.is_some() {
        record.response = Some(MASK.to_owned());
    }
    // A record is plain owned scalars, so this cannot fail; the placeholder
    // is here because reprinting the raw line instead would leak the bodies
    // this function exists to hide.
    serde_json::to_string(&record)
        .unwrap_or_else(|err| format!(r#"{{"error":"unserializable capture record: {err}"}}"#))
}

/// The `--server` / `--tool` / `--endpoint` / `--session` / `--client`
/// narrowing, all of which have to pass for a record to be shown.
#[derive(Default)]
struct Filters<'a> {
    server: Option<&'a str>,
    tool: Option<&'a str>,
    endpoint: Option<&'a str>,
    session: Option<&'a str>,
    client: Option<&'a str>,
}

impl<'a> Filters<'a> {
    fn new(args: &'a WatchArgs) -> Self {
        Self {
            server: args.server.as_deref(),
            tool: args.tool.as_deref(),
            // The label in a record has no leading slash, but the thing a user
            // has in front of them is the URL path they pasted into a client
            // config, so `/s/github` has to mean what `s/github` means.
            endpoint: args
                .endpoint
                .as_deref()
                .map(|want| want.strip_prefix('/').unwrap_or(want)),
            session: args.session.as_deref(),
            client: args.client.as_deref(),
        }
    }

    /// Whether `record` passes every active filter. A tool filter excludes
    /// `tools/list` records, which name no tool; an endpoint filter likewise
    /// excludes lines written before endpoints were recorded, since there is
    /// no honest way to guess which face they arrived on. So does a client
    /// filter for a line nobody attributed: absent is not a match.
    ///
    /// The client is the one substring match here, and the one that is not
    /// case-sensitive. The others are values the user read off a line or
    /// pasted from a config; a client is `claude-code/2.1.3`, whose version
    /// nobody types and whose capitalisation clients do not agree on, so
    /// `--client claude` has to be the useful spelling.
    fn matches(&self, record: &CaptureRecord) -> bool {
        self.server.is_none_or(|want| record.server == want)
            && self
                .tool
                .is_none_or(|want| record.tool.as_deref() == Some(want))
            && self
                .endpoint
                .is_none_or(|want| record.endpoint.as_deref() == Some(want))
            && self.session.is_none_or(|want| record.session == want)
            && self.client.is_none_or(|want| {
                record
                    .client
                    .as_deref()
                    .is_some_and(|client| client.to_lowercase().contains(&want.to_lowercase()))
            })
    }
}

/// One line of the human stream: age, outcome, endpoint, target, latency,
/// client, error.
fn render_line(record: &CaptureRecord, now_ms: u64, color: bool) -> String {
    let target = match (record.kind, record.tool.as_deref()) {
        // A tool call is shown under the name a client would type for it —
        // including the one the allowlist refused, which is the row a reader
        // is looking for when a client says a tool is missing.
        (Kind::Call | Kind::Denied | Kind::Throttled, tool) => format!(
            "{}{}{}",
            record.server,
            mcpgw_core::gateway::SEPARATOR,
            tool.unwrap_or("?")
        ),
        // Everything else is named by its method, plus whatever it addressed
        // — the prompt, the resource URI, the argument being completed.
        (kind, Some(subject)) => format!("{} {} {subject}", record.server, kind.method()),
        (kind, None) => format!("{} {}", record.server, kind.method()),
    };
    // Bracketed in front of the target rather than given a column of its own:
    // the widths a real log mixes (`mcp`, `s/github`, nothing at all) would
    // make a fixed column either ragged or wasteful, and the existing pad
    // absorbs the prefix for free. Lines with no endpoint — stdio traffic and
    // anything captured before N13 — render exactly as they always did.
    let target = match &record.endpoint {
        Some(endpoint) => format!("[{endpoint}] {target}"),
        None => target,
    };
    // Drift gets its own mark before the outcome is even looked at: the list
    // it was noticed during succeeded, so a `✓` is true and useless — what
    // the reader has to see is that the server changed what it says its
    // tools do, on the line above the calls that follow it.
    let drift = record.kind == Kind::Drift;
    let mark = match (drift, record.ok) {
        (true, _) => "⚠",
        (false, true) => "✓",
        (false, false) => "✗",
    };
    let mark = if color {
        match (drift, record.ok) {
            (true, _) => mark.yellow().to_string(),
            (false, true) => mark.green().to_string(),
            (false, false) => mark.red().to_string(),
        }
    } else {
        mark.to_owned()
    };
    let age = age(now_ms, record.ts);
    let age = if color {
        format!("{:>5}", age.dimmed().to_string())
    } else {
        format!("{age:>5}")
    };

    let latency = format!("{}ms", record.duration_ms);
    // Padded by hand: format! width counts bytes, which ANSI escapes skew.
    let pad = 32usize.saturating_sub(target.chars().count());
    let mut line = format!(
        "{age}  {mark}  {target}{}{latency:>7}",
        " ".repeat(pad).as_str()
    );
    // Trailing the latency rather than given a column of its own, and dimmed:
    // a client name is as wide as whoever wrote it (`claude-code/2.1.3`,
    // `cursor`, nothing at all), so a column wide enough for the worst of
    // them would be blank on most lines, and the four columns in front are
    // what a reader scans. Lines nobody attributed — a client that names
    // itself nowhere, anything captured before the field existed — render
    // exactly as they always did.
    if let Some(client) = &record.client {
        let client = clip(client, CLIENT_CHARS);
        line.push_str("  ");
        if color {
            line.push_str(&client.dimmed().to_string());
        } else {
            line.push_str(&client);
        }
    }
    if let Some(change) = record.change {
        // Spelled out rather than left to the `drift` kind alone: the line
        // has to say what moved and by how much without anyone going to the
        // JSON for it. Lengths, never the description — see `capture`.
        let _ = write!(line, "  definition {change}{}", sizes(record));
    }
    if let Some(error) = &record.error {
        line.push_str("  ");
        line.push_str(&clip(error, ERROR_CHARS));
    }
    line
}

/// `, 12 → 384 bytes` for a drift line that has both lengths.
pub(super) fn sizes(record: &CaptureRecord) -> String {
    match (record.desc_len_before, record.desc_len_after) {
        (Some(before), Some(after)) => format!(", {before} → {after} bytes"),
        (None, Some(after)) => format!(", {after} bytes"),
        (Some(before), None) => format!(", was {before} bytes"),
        (None, None) => String::new(),
    }
}

/// Coarse relative age — a live stream only needs to say "just now" or
/// "a while back", and relative times sidestep local-timezone rendering.
fn age(now_ms: u64, ts_ms: u64) -> String {
    let seconds = now_ms.saturating_sub(ts_ms) / 1000;
    match seconds {
        0 => "now".to_owned(),
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn clip(text: &str, chars: usize) -> String {
    let mut out: String = text.replace('\n', " ").chars().take(chars).collect();
    if text.chars().count() > chars {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use mcpgw_core::capture::Bodies;

    use super::*;

    const NOW: u64 = 1_767_225_600_000;

    fn call() -> CaptureRecord {
        let mut record =
            CaptureRecord::new("s3ss", "github", Kind::Call, Duration::from_millis(87))
                .with_tool("create_issue");
        record.ts = NOW - 12_000;
        record
    }

    fn list() -> CaptureRecord {
        let mut record = CaptureRecord::new("s3ss", "linear", Kind::List, Duration::from_millis(4));
        record.ts = NOW - 7_200_000;
        record
    }

    fn drift() -> CaptureRecord {
        let mut record = CaptureRecord::new("s3ss", "github", Kind::Drift, Duration::ZERO)
            .with_drift(&mcpgw_core::pins::DriftEvent {
                tool: "create_issue".to_owned(),
                change: mcpgw_core::pins::Change::Changed,
                at: NOW,
                desc_len_before: Some(21),
                desc_len_after: Some(384),
            });
        record.ts = NOW - 1000;
        record
    }

    #[test]
    fn renders_a_drift_line_with_its_own_mark_and_no_description() {
        let line = render_line(&drift(), NOW, false);
        assert!(line.contains('⚠'), "{line}");
        assert!(!line.contains('✓'), "{line}");
        assert!(line.contains("github tools/list"), "{line}");
        assert!(
            line.contains("definition changed, 21 → 384 bytes"),
            "{line}"
        );
    }

    #[test]
    fn renders_a_successful_call() {
        insta::assert_snapshot!(render_line(&call(), NOW, false));
    }

    #[test]
    fn renders_a_failure_with_its_error() {
        let record = list().with_error("upstream \"linear\" failed after 3 attempt(s): refused");
        insta::assert_snapshot!(render_line(&record, NOW, false));
    }

    #[test]
    fn long_errors_are_clipped_to_one_line() {
        let record = call().with_error(&"boom ".repeat(80));
        let line = render_line(&record, NOW, false);
        assert!(line.ends_with('…'), "{line}");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn colored_lines_carry_ansi() {
        assert!(render_line(&call(), NOW, true).contains('\u{1b}'));
        assert!(!render_line(&call(), NOW, false).contains('\u{1b}'));
    }

    #[test]
    fn ages_read_at_a_glance() {
        assert_eq!(age(NOW, NOW), "now");
        assert_eq!(age(NOW, NOW - 12_000), "12s");
        assert_eq!(age(NOW, NOW - 300_000), "5m");
        assert_eq!(age(NOW, NOW - 7_200_000), "2h");
        assert_eq!(age(NOW, NOW - 172_800_000), "2d");
        // A record stamped in the future (clock skew) is simply "now".
        assert_eq!(age(NOW, NOW + 5_000), "now");
    }

    #[test]
    fn filters_narrow_by_server_and_tool() {
        let call = call();
        let filters = |server, tool| Filters {
            server,
            tool,
            ..Filters::default()
        };
        assert!(filters(None, None).matches(&call));
        assert!(filters(Some("github"), Some("create_issue")).matches(&call));
        assert!(!filters(Some("linear"), None).matches(&call));
        assert!(!filters(None, Some("other")).matches(&call));
        // tools/list names no tool, so a tool filter hides it.
        assert!(filters(Some("linear"), None).matches(&list()));
        assert!(!filters(None, Some("create_issue")).matches(&list()));
    }

    #[test]
    fn filters_narrow_by_endpoint() {
        let on_endpoint = call().with_endpoint("s/github");
        let filters = |endpoint| Filters {
            endpoint,
            ..Filters::default()
        };
        assert!(filters(Some("s/github")).matches(&on_endpoint));
        assert!(!filters(Some("s/linear")).matches(&on_endpoint));
        assert!(!filters(Some("mcp")).matches(&on_endpoint));
        // A record from before the field existed cannot claim an endpoint.
        assert!(!filters(Some("s/github")).matches(&call()));
        assert!(filters(None).matches(&call()));
    }

    #[test]
    fn an_endpoint_filter_takes_the_path_the_user_pasted() {
        let args = WatchArgs {
            server: None,
            tool: None,
            endpoint: Some("/s/github".to_owned()),
            session: None,
            client: None,
            json: false,
            tui: false,
            show_secrets: false,
        };
        assert!(Filters::new(&args).matches(&call().with_endpoint("s/github")));
    }

    #[test]
    fn filters_narrow_by_session() {
        let filters = |session| Filters {
            session,
            ..Filters::default()
        };
        assert!(filters(Some("s3ss")).matches(&call()));
        assert!(!filters(Some("0ther")).matches(&call()));
    }

    #[test]
    fn filters_narrow_by_client() {
        let mut attributed = call();
        attributed.client = Some("claude-code/2.1.3".to_owned());
        let filters = |client| Filters {
            client,
            ..Filters::default()
        };
        // A substring of the name is the spelling a user has: nobody types
        // the version, and clients disagree about capitalisation.
        assert!(filters(Some("claude")).matches(&attributed));
        assert!(filters(Some("Claude-Code")).matches(&attributed));
        assert!(filters(Some("claude-code/2.1.3")).matches(&attributed));
        assert!(!filters(Some("cursor")).matches(&attributed));
        // A line nobody attributed is not a match for any client.
        assert!(!filters(Some("claude")).matches(&call()));
        assert!(filters(None).matches(&call()));
    }

    #[test]
    fn the_client_shows_in_the_rendered_line() {
        let mut record = call();
        record.client = Some("claude-code/2.1.3".to_owned());
        insta::assert_snapshot!(render_line(&record, NOW, false));
    }

    #[test]
    fn json_carries_the_client_through_the_mask() {
        let mut record = call().with_args(r#"{"token":"ghp_realsecret"}"#.to_owned());
        record.client = Some("claude-code/2.1.3".to_owned());
        let json: serde_json::Value = serde_json::from_str(&json_line(&record)).unwrap();
        assert_eq!(json["client"], "claude-code/2.1.3");
        assert_eq!(json["args"], MASK);
    }

    #[test]
    fn the_endpoint_shows_in_the_rendered_line() {
        insta::assert_snapshot!(render_line(&call().with_endpoint("s/github"), NOW, false));
    }

    #[test]
    fn json_masks_both_captured_bodies() {
        let record = call()
            .with_args(r#"{"token":"ghp_realsecret"}"#.to_owned())
            .with_response(r#"{"content":[{"text":"t0ken"}]}"#.to_owned());
        let line = json_line(&record);
        assert!(!line.contains("ghp_realsecret"), "{line}");
        assert!(!line.contains("t0ken"), "{line}");
        // Everything the stream is read for survives.
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(json["args"], MASK);
        assert_eq!(json["response"], MASK);
        assert_eq!(json["server"], "github");
        assert_eq!(json["tool"], "create_issue");
        assert_eq!(json["duration_ms"], 87);
        assert_eq!(json["ok"], true);
    }

    #[test]
    fn a_record_redacted_at_capture_time_needs_no_second_mask() {
        // What the gateway wrote under `--capture-bodies redacted`: the key
        // is gone, the shape hint is not, and masking would throw that away
        // for a `***` that says less.
        let mut record = call().with_args(r#"{"token":"[redacted:ghp_…]"}"#.to_owned());
        record.bodies = Bodies::Redacted;
        let raw = serde_json::to_string(&record).unwrap();
        assert_eq!(json_stream_line(&record, &raw, false), raw);
        assert_eq!(json_stream_line(&record, &raw, true), raw);
    }

    #[test]
    fn a_record_captured_verbatim_is_masked_unless_asked_for() {
        let record = call().with_args(r#"{"token":"ghp_realsecret"}"#.to_owned());
        assert!(record.bodies.is_full());
        let raw = serde_json::to_string(&record).unwrap();
        assert!(!json_stream_line(&record, &raw, false).contains("ghp_realsecret"));
        assert_eq!(json_stream_line(&record, &raw, true), raw);
    }

    #[test]
    fn json_leaves_records_without_bodies_alone() {
        // tools/list carries neither body: masking must not invent them, so
        // "no arguments" stays distinguishable from "arguments withheld".
        let line = json_line(&list().with_error("refused"));
        let json: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert!(json.get("args").is_none(), "{line}");
        assert!(json.get("response").is_none(), "{line}");
        // Errors are not bodies — they are the reason to be watching.
        assert_eq!(json["error"], "refused");
    }

    #[test]
    fn the_human_stream_never_prints_a_captured_body() {
        let record = call()
            .with_args("ghp_realsecret".to_owned())
            .with_response("t0ken".to_owned());
        let line = render_line(&record, NOW, false);
        assert!(!line.contains("ghp_realsecret"), "{line}");
        assert!(!line.contains("t0ken"), "{line}");
    }

    #[test]
    fn only_whole_lines_are_consumed() {
        let (lines, consumed) = complete_lines(b"{\"a\":1}\n{\"b\":2}\n{\"partial\"");
        assert_eq!(lines, ["{\"a\":1}", "{\"b\":2}"]);
        assert_eq!(consumed, 16);

        // Nothing yet: the first line is still being written.
        assert_eq!(complete_lines(b"{\"partial\""), (Vec::new(), 0));
        assert_eq!(complete_lines(b""), (Vec::new(), 0));
    }

    #[test]
    fn tail_replays_history_then_follows_appends() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("2026-01-01.jsonl");
        std::fs::write(&path, "one\ntwo\n").unwrap();

        let mut tail = Tail::new(path.clone());
        assert_eq!(tail.poll().unwrap(), ["one", "two"]);
        assert!(tail.poll().unwrap().is_empty());

        // A half-written line is held back until its newline arrives.
        std::fs::write(&path, "one\ntwo\nthr").unwrap();
        assert!(tail.poll().unwrap().is_empty());
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        assert_eq!(tail.poll().unwrap(), ["three"]);
    }

    #[test]
    fn a_read_failure_is_reported_and_the_tail_keeps_going() {
        let dir = tempfile::tempdir().unwrap();
        // A directory where a file is expected fails to read on every
        // platform, which is the shape of any transient I/O error here.
        let blocked = daily_path(dir.path(), now_millis());
        std::fs::create_dir(&blocked).unwrap();
        let mut follow = Follow::new(dir.path().to_path_buf());
        assert!(poll_or_report(&mut follow).is_empty());

        // The same follow recovers once the path is readable again.
        std::fs::remove_dir(&blocked).unwrap();
        std::fs::write(&blocked, "one\n").unwrap();
        assert_eq!(poll_or_report(&mut follow), ["one"]);
    }

    #[test]
    fn a_follow_reads_the_file_for_today() {
        let dir = tempfile::tempdir().unwrap();
        let today = daily_path(dir.path(), now_millis());
        std::fs::write(&today, "one\n").unwrap();
        let mut follow = Follow::new(dir.path().to_path_buf());
        assert_eq!(follow.poll().unwrap(), ["one"]);
        assert!(follow.poll().unwrap().is_empty());
    }

    #[test]
    fn a_rollover_moves_the_follow_to_the_new_day() {
        let dir = tempfile::tempdir().unwrap();
        // A follow left over from yesterday: the offset it holds belongs to a
        // file nobody is writing to any more.
        let yesterday = daily_path(dir.path(), now_millis() - 86_400_000);
        std::fs::write(&yesterday, "old\n").unwrap();
        let mut follow = Follow {
            dir: dir.path().to_path_buf(),
            tail: Tail::new(yesterday),
        };

        let today = daily_path(dir.path(), now_millis());
        std::fs::write(&today, "new\n").unwrap();
        assert_eq!(follow.poll().unwrap(), ["new"]);
        assert_eq!(follow.tail.path, today);
        // …and the new day is followed from its own offset, not yesterday's.
        std::fs::write(&today, "new\nnewer\n").unwrap();
        assert_eq!(follow.poll().unwrap(), ["newer"]);
    }

    #[test]
    fn a_missing_file_is_an_empty_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut tail = Tail::new(dir.path().join("nothing-yet.jsonl"));
        assert!(tail.poll().unwrap().is_empty());
    }
}
