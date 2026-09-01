//! `mcpgw watch`: a live view of the capture log. Tailing the JSONL file
//! keeps this decoupled from the gateway — there is no socket to connect to,
//! watching works on a gateway started before it, and the same loop replays
//! history it finds in today's file before following new lines.

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

#[derive(clap::Args)]
pub struct WatchArgs {
    /// Only show traffic for this server
    #[arg(long, value_name = "NAME")]
    pub server: Option<String>,
    /// Only show traffic for this tool (bare name, without the server prefix)
    #[arg(long, value_name = "NAME")]
    pub tool: Option<String>,
    /// Stream the raw JSONL lines instead of the rendered stream
    #[arg(long)]
    pub json: bool,
}

pub fn run(args: &WatchArgs, color: bool) -> anyhow::Result<()> {
    let state_dir = mcpgw_core::paths::state_dir()
        .context("cannot determine a home directory to resolve the state directory")?;
    let dir = state_dir.join(mcpgw_core::capture::TRAFFIC_DIR);
    if !args.json {
        println!("watching {} (Ctrl-C to stop)", dir.display());
    }

    let mut tail = Tail::new(daily_path(&dir, now_millis()));
    loop {
        // Re-resolved every round: at midnight the gateway starts a new file
        // and the tail follows it without needing to be restarted.
        let path = daily_path(&dir, now_millis());
        if path != tail.path {
            tail = Tail::new(path);
        }
        for line in tail.poll()? {
            let Ok(record) = serde_json::from_str::<CaptureRecord>(&line) else {
                // A line the current build cannot parse (older or newer
                // format) is skipped rather than ending the stream.
                continue;
            };
            if !matches(&record, args.server.as_deref(), args.tool.as_deref()) {
                continue;
            }
            if args.json {
                println!("{line}");
            } else {
                println!("{}", render_line(&record, now_millis(), color));
            }
        }
        std::thread::sleep(POLL);
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

/// Whether a record passes the `--server` / `--tool` filters. A tool filter
/// excludes `tools/list` records, which name no tool.
fn matches(record: &CaptureRecord, server: Option<&str>, tool: Option<&str>) -> bool {
    server.is_none_or(|want| record.server == want)
        && tool.is_none_or(|want| record.tool.as_deref() == Some(want))
}

/// One line of the human stream: age, outcome, target, latency, error.
fn render_line(record: &CaptureRecord, now_ms: u64, color: bool) -> String {
    let target = match record.kind {
        Kind::Call => format!(
            "{}{}{}",
            record.server,
            mcpgw_core::gateway::SEPARATOR,
            record.tool.as_deref().unwrap_or("?")
        ),
        Kind::List => format!("{} tools/list", record.server),
    };
    let mark = if record.ok { "✓" } else { "✗" };
    let mark = if color {
        if record.ok {
            mark.green().to_string()
        } else {
            mark.red().to_string()
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
    if let Some(error) = &record.error {
        line.push_str("  ");
        line.push_str(&clip(error, ERROR_CHARS));
    }
    line
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
        assert!(matches(&call, None, None));
        assert!(matches(&call, Some("github"), Some("create_issue")));
        assert!(!matches(&call, Some("linear"), None));
        assert!(!matches(&call, None, Some("other")));
        // tools/list names no tool, so a tool filter hides it.
        assert!(matches(&list(), Some("linear"), None));
        assert!(!matches(&list(), None, Some("create_issue")));
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
    fn a_missing_file_is_an_empty_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut tail = Tail::new(dir.path().join("nothing-yet.jsonl"));
        assert!(tail.poll().unwrap().is_empty());
    }
}
