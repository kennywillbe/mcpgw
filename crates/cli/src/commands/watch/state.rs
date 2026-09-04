//! Everything `watch --tui` puts on the screen, computed from the records the
//! tail hands over and the keys the user pressed — and nothing else.
//!
//! Deliberately terminal-free: no crossterm, no ratatui, no I/O. The three
//! panes are the interesting part of this feature and the hardest to look at,
//! so the window statistics, the filters, the pause buffer and the masking
//! all live where a plain `cargo test` can reach them. [`super::tui`] turns a
//! key event into a [`Key`] and draws the models this module returns.

use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;

use mcpgw_core::capture::{CaptureRecord, Kind};

use super::{Filters, MASK, WatchArgs, age};

/// How many records the state keeps.
///
/// The table's whole point is the recent shape of the traffic — a percentile
/// over a whole day would flatten exactly the spike someone opened the TUI to
/// find — and the same bound is what stops a watch left running overnight
/// growing without limit.
pub(super) const WINDOW: usize = 1000;

/// One captured line as the TUI holds it.
#[derive(Debug, Clone)]
pub(super) struct Entry {
    pub(super) record: CaptureRecord,
}

impl Entry {
    /// One JSONL line, or `None` for a line this build cannot read — the same
    /// forgiveness the plain stream shows, for the same reason.
    pub(super) fn parse(line: &str) -> Option<Self> {
        let record = serde_json::from_str(line).ok()?;
        Some(Self { record })
    }

    /// Which downstream client the gateway attributed the call to, if any.
    /// A gateway old enough to attribute nothing and a client that named
    /// itself nowhere are the same absence here, and the panes say so.
    fn client(&self) -> Option<&str> {
        self.record.client.as_deref()
    }

    /// What the record is filed under in the table and named by in the log:
    /// the tool for a call, the method for everything else.
    fn target(&self) -> &str {
        match (self.record.kind, self.record.tool.as_deref()) {
            // A refused call is filed under the tool it named, so the TUI
            // groups it with the calls that did get through.
            (Kind::Call | Kind::Denied, Some(tool)) => tool,
            (Kind::Call | Kind::Denied, None) => "?",
            (kind, _) => kind.method(),
        }
    }
}

/// The outcome half of the filter cycle. `ok` and `error` are the two words
/// the record format already uses, so they are the two words accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Status {
    Ok,
    Failed,
}

impl Status {
    fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "ok" | "true" | "pass" => Some(Status::Ok),
            "error" | "err" | "fail" | "failed" | "false" => Some(Status::Failed),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Failed => "error",
        }
    }
}

/// Which column the table is ordered by.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Sort {
    #[default]
    Calls,
    Errors,
    Latency,
}

impl Sort {
    fn next(self) -> Self {
        match self {
            Sort::Calls => Sort::Errors,
            Sort::Errors => Sort::Latency,
            Sort::Latency => Sort::Calls,
        }
    }

    pub(super) fn as_str(self) -> &'static str {
        match self {
            Sort::Calls => "calls",
            Sort::Errors => "errors",
            Sort::Latency => "p95",
        }
    }
}

/// Which field `f` is about to ask for. A cycle rather than four keys: the
/// four are asked one at a time and the prompt says which one it wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Field {
    #[default]
    Server,
    Tool,
    Status,
    Client,
}

impl Field {
    fn next(self) -> Self {
        match self {
            Field::Server => Field::Tool,
            Field::Tool => Field::Status,
            Field::Status => Field::Client,
            Field::Client => Field::Server,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Field::Server => "server",
            Field::Tool => "tool",
            Field::Status => "status (ok/error)",
            Field::Client => "client",
        }
    }
}

/// Every narrowing the TUI can apply, and the state `f` and `/` write into.
///
/// A superset of the stream's [`Filters`], which it defers to for the five
/// the flags already spell — one matcher for `--server`/`--tool`/
/// `--endpoint`/`--session`/`--client` means the TUI cannot drift from the
/// stream on what those words mean, including the leading slash an endpoint
/// is allowed and the substring a client is matched by.
#[derive(Debug, Clone, Default)]
pub(super) struct Filter {
    pub(super) server: Option<String>,
    pub(super) tool: Option<String>,
    pub(super) endpoint: Option<String>,
    pub(super) session: Option<String>,
    pub(super) client: Option<String>,
    pub(super) status: Option<Status>,
    pub(super) text: Option<String>,
}

impl Filter {
    /// The initial narrowing: whatever the flags asked for, so `--tui` starts
    /// where the same flags would have started the stream.
    fn from_args(args: &WatchArgs) -> Self {
        let narrowing = Filters::new(args);
        Self {
            server: narrowing.server.map(str::to_owned),
            tool: narrowing.tool.map(str::to_owned),
            endpoint: narrowing.endpoint.map(str::to_owned),
            session: narrowing.session.map(str::to_owned),
            client: narrowing.client.map(str::to_owned),
            ..Self::default()
        }
    }

    fn matches(&self, entry: &Entry) -> bool {
        let narrowing = Filters {
            server: self.server.as_deref(),
            tool: self.tool.as_deref(),
            endpoint: self.endpoint.as_deref(),
            session: self.session.as_deref(),
            client: self.client.as_deref(),
        };
        narrowing.matches(&entry.record)
            && self
                .status
                .is_none_or(|want| entry.record.ok == (want == Status::Ok))
            && self
                .text
                .as_deref()
                .is_none_or(|want| haystack(entry, want))
    }

    /// Applies a typed answer. An empty answer clears that field, which is
    /// how a filter is taken off without a key of its own; a status the
    /// vocabulary does not have clears it too rather than matching nothing.
    fn set(&mut self, field: Field, value: &str) {
        let value = value.trim();
        let slot = (!value.is_empty()).then(|| value.to_owned());
        match field {
            Field::Server => self.server = slot,
            Field::Tool => self.tool = slot,
            Field::Client => self.client = slot,
            Field::Status => self.status = slot.as_deref().and_then(Status::parse),
        }
    }

    /// The active narrowings, for the footer. Empty when nothing is filtered.
    pub(super) fn summary(&self) -> String {
        let mut parts = Vec::new();
        for (label, value) in [
            ("server", self.server.as_deref()),
            ("tool", self.tool.as_deref()),
            ("endpoint", self.endpoint.as_deref()),
            ("session", self.session.as_deref()),
            ("client", self.client.as_deref()),
            ("status", self.status.map(Status::as_str)),
            ("/", self.text.as_deref()),
        ] {
            if let Some(value) = value {
                parts.push(format!("{label}={value}"));
            }
        }
        parts.join(" ")
    }
}

/// Whether the free-text filter matches anything a reader can see on the row.
///
/// Case-insensitive and substring, because `/` is the key someone presses
/// when they half-remember a name; the bodies are excluded on purpose, so a
/// search can never be the thing that pulls a masked secret onto the screen.
fn haystack(entry: &Entry, want: &str) -> bool {
    let want = want.to_lowercase();
    let record = &entry.record;
    let fields = [
        Some(record.server.as_str()),
        record.tool.as_deref(),
        Some(record.kind.method()),
        record.endpoint.as_deref(),
        Some(record.session.as_str()),
        entry.client(),
        record.error.as_deref(),
    ];
    fields
        .into_iter()
        .flatten()
        .any(|field| field.to_lowercase().contains(&want))
}

/// A key as the state understands it, with crossterm's modifiers and its
/// platform quirks already resolved by [`super::tui`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Key {
    Char(char),
    Enter,
    Esc,
    Backspace,
    Up,
    Down,
}

/// What the loop should do with the key it just handed over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Action {
    Continue,
    Quit,
}

/// The key map, in the order the help overlay lists it. One table rather than
/// a match arm and a paragraph that drift apart: the test below presses every
/// key in it.
pub(super) const KEYS: &[(&str, &str)] = &[
    ("q", "quit"),
    ("↑/↓ j/k", "select a call"),
    ("Enter", "toggle the detail pane"),
    ("f", "filter by server → tool → status → client"),
    ("/", "free text filter"),
    ("p", "pause / resume"),
    ("c", "clear"),
    ("s", "sort by calls / errors / p95"),
    ("?", "this help"),
];

/// A pending question in the footer: which field it will answer, and what has
/// been typed so far.
#[derive(Debug, Clone)]
pub(super) struct Prompt {
    pub(super) field: Option<Field>,
    pub(super) input: String,
}

impl Prompt {
    pub(super) fn label(&self) -> &'static str {
        match self.field {
            Some(field) => field.label(),
            None => "search",
        }
    }
}

/// One row of the per-server-and-tool table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TableRow {
    pub(super) server: String,
    pub(super) target: String,
    pub(super) calls: usize,
    pub(super) errors: usize,
    pub(super) p50: u64,
    pub(super) p95: u64,
    pub(super) last_seen: String,
}

/// One row of the call log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LogRow {
    pub(super) age: String,
    pub(super) server: String,
    pub(super) target: String,
    pub(super) kind: &'static str,
    pub(super) client: String,
    pub(super) duration: String,
    pub(super) ok: bool,
}

/// The detail pane: label/value pairs, already masked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Detail {
    pub(super) fields: Vec<(&'static str, String)>,
}

/// The whole screen.
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per toggle key, and a key is exactly a boolean"
)]
pub(super) struct State {
    /// The traffic directory being followed, for the header.
    pub(super) dir: PathBuf,
    entries: VecDeque<Entry>,
    /// What arrived while paused. Held rather than dropped: `p` is pressed
    /// because something scrolled past, and a pause that lost the next
    /// hundred calls would answer one question by destroying the next.
    held: Vec<Entry>,
    pub(super) filter: Filter,
    pub(super) sort: Sort,
    field: Field,
    pub(super) paused: bool,
    /// Whether the selection sticks to the newest row. Scrolling up turns it
    /// off; scrolling back to the bottom turns it on again.
    pub(super) follow: bool,
    pub(super) detail: bool,
    pub(super) help: bool,
    pub(super) prompt: Option<Prompt>,
    /// Index into the *filtered* log, so a filter that hides the selected row
    /// moves the selection rather than pointing at nothing.
    selected: usize,
    show_secrets: bool,
    /// The last thing the tail complained about. The stream prints these to
    /// stderr; under an alternate screen there is no stderr to print to, so
    /// it goes in the footer instead of being swallowed.
    pub(super) notice: Option<String>,
}

impl State {
    pub(super) fn new(args: &WatchArgs, dir: PathBuf) -> Self {
        Self {
            dir,
            entries: VecDeque::new(),
            held: Vec::new(),
            filter: Filter::from_args(args),
            sort: Sort::default(),
            field: Field::default(),
            paused: false,
            follow: true,
            detail: true,
            help: false,
            prompt: None,
            selected: 0,
            show_secrets: args.show_secrets,
            notice: None,
        }
    }

    /// Takes one freshly tailed line. Unparseable lines are dropped, exactly
    /// as the stream drops them.
    pub(super) fn push_line(&mut self, line: &str) {
        if let Some(entry) = Entry::parse(line) {
            if self.paused {
                // Bounded by the same window, so a pause left on all day
                // costs no more memory than running does.
                if self.held.len() == WINDOW {
                    self.held.remove(0);
                }
                self.held.push(entry);
            } else {
                self.admit(entry);
            }
        }
    }

    fn admit(&mut self, entry: Entry) {
        if self.entries.len() == WINDOW {
            self.entries.pop_front();
        }
        self.entries.push_back(entry);
        self.settle();
    }

    /// Keeps the selection inside the filtered log, and pinned to the newest
    /// row while following.
    fn settle(&mut self) {
        let len = self.visible().len();
        self.selected = if self.follow {
            len.saturating_sub(1)
        } else {
            self.selected.min(len.saturating_sub(1))
        };
    }

    fn visible(&self) -> Vec<&Entry> {
        self.entries
            .iter()
            .filter(|entry| self.filter.matches(entry))
            .collect()
    }

    pub(super) fn selected(&self) -> Option<usize> {
        (!self.visible().is_empty()).then_some(self.selected)
    }

    /// How many records are in the window, and how many of them are shown.
    pub(super) fn counts(&self) -> (usize, usize) {
        (self.visible().len(), self.entries.len())
    }

    /// How many lines a pause is holding back, for the footer.
    pub(super) fn held(&self) -> usize {
        self.held.len()
    }

    /// The table, one row per server and tool, ordered by [`Sort`].
    pub(super) fn table(&self, now_ms: u64) -> Vec<TableRow> {
        let mut groups: BTreeMap<(&str, &str), (Vec<u64>, usize, u64)> = BTreeMap::new();
        for entry in self.visible() {
            let group = groups
                .entry((entry.record.server.as_str(), entry.target()))
                .or_insert_with(|| (Vec::new(), 0, 0));
            group.0.push(entry.record.duration_ms);
            group.1 += usize::from(!entry.record.ok);
            group.2 = group.2.max(entry.record.ts);
        }
        let mut rows: Vec<TableRow> = groups
            .into_iter()
            .map(|((server, target), (mut durations, errors, last))| {
                durations.sort_unstable();
                TableRow {
                    server: server.to_owned(),
                    target: target.to_owned(),
                    calls: durations.len(),
                    errors,
                    p50: percentile(&durations, 50),
                    p95: percentile(&durations, 95),
                    last_seen: age(now_ms, last),
                }
            })
            .collect();
        // Descending on the sort column, because the row worth looking at is
        // the busiest, the most broken or the slowest. The name is the
        // tie-break so the table does not reshuffle under a stable load.
        rows.sort_by(|a, b| {
            let key = |row: &TableRow| match self.sort {
                Sort::Calls => row.calls,
                Sort::Errors => row.errors,
                Sort::Latency => usize::try_from(row.p95).unwrap_or(usize::MAX),
            };
            key(b)
                .cmp(&key(a))
                .then_with(|| (&a.server, &a.target).cmp(&(&b.server, &b.target)))
        });
        rows
    }

    /// The call log, oldest first, so the newest row is at the bottom.
    pub(super) fn log(&self, now_ms: u64) -> Vec<LogRow> {
        self.visible()
            .into_iter()
            .map(|entry| LogRow {
                age: age(now_ms, entry.record.ts),
                server: entry.record.server.clone(),
                target: entry.target().to_owned(),
                kind: entry.record.kind.method(),
                // An em dash rather than an empty cell: "this gateway does
                // not attribute clients" and "this call had no client" look
                // the same on screen, and both are the absence of an answer.
                client: entry.client().unwrap_or("—").to_owned(),
                duration: format!("{}ms", entry.record.duration_ms),
                ok: entry.record.ok,
            })
            .collect()
    }

    /// The detail pane for the selected call, with the bodies masked the way
    /// the stream masks them.
    pub(super) fn detail(&self, now_ms: u64) -> Option<Detail> {
        let visible = self.visible();
        let entry = visible.get(self.selected)?;
        let record = &entry.record;
        let mut fields = vec![
            ("age", age(now_ms, record.ts)),
            ("server", record.server.clone()),
            ("target", entry.target().to_owned()),
            ("method", record.kind.method().to_owned()),
            ("client", entry.client().unwrap_or("—").to_owned()),
            ("session", record.session.clone()),
            (
                "endpoint",
                record.endpoint.clone().unwrap_or_else(|| "—".to_owned()),
            ),
            // One row rather than three: the pane has to leave room for the
            // bodies underneath it, which are the reason it was opened.
            (
                "outcome",
                format!(
                    "{} · {}ms · captured {}",
                    if record.ok { "ok" } else { "error" },
                    record.duration_ms,
                    record.bodies
                ),
            ),
        ];
        // Not masked, and deliberately: `--json` does not mask it either.
        // The error text is the reason the pane was opened, and a gateway
        // capturing `redacted` already ran the rules over it.
        if let Some(error) = &record.error {
            fields.push(("error", error.clone()));
        }
        if let Some(args) = self.body(record.args.as_deref(), record) {
            fields.push(("args", args));
        }
        if let Some(response) = self.body(record.response.as_deref(), record) {
            fields.push(("response", response));
        }
        Some(Detail { fields })
    }

    /// One captured body as the screen is allowed to show it.
    ///
    /// The same rule as `watch --json`: a line the gateway wrote verbatim is
    /// masked here, a line it already redacted goes out as written — masking
    /// that twice would hide the `[redacted:ghp_…]` hints and reveal nothing
    /// by doing so. A terminal is not a safer place for a secret than a file.
    fn body(&self, value: Option<&str>, record: &CaptureRecord) -> Option<String> {
        let value = value?;
        if record.bodies.is_full() && !self.show_secrets {
            Some(MASK.to_owned())
        } else {
            Some(value.to_owned())
        }
    }

    pub(super) fn on_key(&mut self, key: Key) -> Action {
        // The overlay is modal: it covers the panes, so the next key takes it
        // away rather than acting on something the user cannot see.
        if self.help {
            self.help = false;
            return Action::Continue;
        }
        if self.prompt.is_some() {
            self.prompt_key(key);
            return Action::Continue;
        }
        match key {
            Key::Char('q') | Key::Esc => return Action::Quit,
            Key::Up | Key::Char('k') => self.select_up(),
            Key::Down | Key::Char('j') => self.select_down(),
            Key::Enter => self.detail = !self.detail,
            Key::Char('f') => {
                self.field = self.field.next();
                self.prompt = Some(Prompt {
                    field: Some(self.field),
                    input: String::new(),
                });
            }
            Key::Char('/') => {
                self.prompt = Some(Prompt {
                    field: None,
                    input: String::new(),
                });
            }
            Key::Char('p') => self.toggle_pause(),
            Key::Char('c') => self.clear(),
            Key::Char('s') => self.sort = self.sort.next(),
            Key::Char('?') => self.help = true,
            _ => {}
        }
        Action::Continue
    }

    fn prompt_key(&mut self, key: Key) {
        match key {
            Key::Char(c) => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.input.push(c);
                }
            }
            Key::Backspace => {
                if let Some(prompt) = &mut self.prompt {
                    prompt.input.pop();
                }
            }
            Key::Enter => {
                let Some(prompt) = self.prompt.take() else {
                    return;
                };
                let value = prompt.input.trim().to_owned();
                match prompt.field {
                    Some(field) => self.filter.set(field, &value),
                    None => self.filter.text = (!value.is_empty()).then_some(value),
                }
                // A narrower view can leave the selection past the end, and
                // the row worth looking at after a filter is the newest one.
                self.follow = true;
                self.settle();
            }
            Key::Esc => self.prompt = None,
            Key::Up | Key::Down => {}
        }
    }

    fn select_up(&mut self) {
        self.follow = false;
        self.selected = self.selected.saturating_sub(1);
    }

    /// Reaching the last row re-arms the follow, so getting back to live is
    /// holding `j` down rather than a key nobody would guess.
    fn select_down(&mut self) {
        let last = self.visible().len().saturating_sub(1);
        self.selected = (self.selected + 1).min(last);
        self.follow = self.selected == last;
    }

    fn toggle_pause(&mut self) {
        self.paused = !self.paused;
        if !self.paused {
            for entry in std::mem::take(&mut self.held) {
                self.admit(entry);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.held.clear();
        self.selected = 0;
        self.follow = true;
    }
}

/// Nearest-rank percentile over an already sorted slice.
///
/// Nearest-rank rather than an interpolating definition because these are
/// observed latencies: every number the table shows is a request that really
/// took that long, which is the number someone chasing a slow tool wants.
fn percentile(sorted: &[u64], pct: usize) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (sorted.len() * pct).div_ceil(100).clamp(1, sorted.len());
    sorted[rank - 1]
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_767_225_600_000;

    pub(super) fn args() -> WatchArgs {
        WatchArgs {
            server: None,
            tool: None,
            endpoint: None,
            session: None,
            client: None,
            json: false,
            tui: true,
            show_secrets: false,
        }
    }

    fn fresh() -> State {
        State::new(&args(), PathBuf::from("/traffic"))
    }

    /// A record as it appears on disk, so every test goes through the same
    /// parse the tail feeds the state through.
    ///
    /// `extra` names only the fields its test cares about and overrides the
    /// base where they collide, which is what the round trip through a map is
    /// for: the parser under test refuses a line that spells a field twice,
    /// exactly as the stream's does, and a fixture must not be the one thing
    /// that gets a laxer reader.
    fn line(extra: &str) -> String {
        let written = format!(
            r#"{{"ts":{NOW},"session":"s3ss","server":"github","tool":"create_issue",
             "kind":"call","duration_ms":87,"ok":true{extra}}}"#
        )
        .replace('\n', "");
        let fields: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&written).unwrap();
        serde_json::to_string(&fields).unwrap()
    }

    fn push(state: &mut State, extra: &str) {
        state.push_line(&line(extra));
    }

    #[test]
    fn percentiles_are_observed_latencies() {
        assert_eq!(percentile(&[], 50), 0);
        assert_eq!(percentile(&[7], 50), 7);
        assert_eq!(percentile(&[7], 95), 7);
        assert_eq!(percentile(&[1, 2, 3, 4], 50), 2);
        assert_eq!(percentile(&[1, 2, 3, 4], 95), 4);
        // 100 samples: p50 is the 50th, p95 the 95th, and both are numbers a
        // request really took rather than an interpolation between two.
        let hundred: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&hundred, 50), 50);
        assert_eq!(percentile(&hundred, 95), 95);
    }

    #[test]
    fn the_table_groups_by_server_and_tool() {
        let mut state = fresh();
        for _ in 0..3 {
            push(&mut state, "");
        }
        push(&mut state, r#","ok":false,"error":"refused""#);
        push(
            &mut state,
            r#","server":"linear","tool":null,"kind":"list""#,
        );

        let rows = state.table(NOW);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].server, "github");
        assert_eq!(rows[0].target, "create_issue");
        assert_eq!(rows[0].calls, 4);
        assert_eq!(rows[0].errors, 1);
        assert_eq!(rows[0].p50, 87);
        assert_eq!(rows[0].p95, 87);
        // A list names no tool, so it is filed under its method.
        assert_eq!(rows[1].target, "tools/list");
        assert_eq!(rows[1].calls, 1);
    }

    #[test]
    fn the_table_sorts_by_the_column_the_user_picked() {
        let mut state = fresh();
        // Two calls to `slow`, three to `fast`, one of which failed.
        for _ in 0..3 {
            push(&mut state, r#","tool":"fast""#);
        }
        push(&mut state, r#","tool":"fast","ok":false"#);
        for _ in 0..2 {
            push(&mut state, r#","tool":"slow","duration_ms":900"#);
        }
        let target = |state: &State| state.table(NOW)[0].target.clone();
        assert_eq!(state.sort, Sort::Calls);
        assert_eq!(target(&state), "fast");
        state.on_key(Key::Char('s'));
        assert_eq!(state.sort, Sort::Errors);
        assert_eq!(target(&state), "fast");
        state.on_key(Key::Char('s'));
        assert_eq!(state.sort, Sort::Latency);
        assert_eq!(target(&state), "slow");
        state.on_key(Key::Char('s'));
        assert_eq!(state.sort, Sort::Calls);
    }

    #[test]
    fn filters_stack_and_the_flags_seed_them() {
        let mut state = State::new(
            &WatchArgs {
                server: Some("github".to_owned()),
                endpoint: Some("/s/github".to_owned()),
                ..args()
            },
            PathBuf::from("/traffic"),
        );
        push(&mut state, r#","endpoint":"s/github""#);
        push(&mut state, r#","endpoint":"mcp""#);
        push(&mut state, r#","server":"linear","endpoint":"s/github""#);
        // The leading slash a user pastes means what the record spells.
        assert_eq!(state.counts(), (1, 3));

        // …and a tool filter typed at the prompt narrows it further.
        state.filter.set(Field::Tool, "nothing_like_it");
        assert_eq!(state.counts().0, 0);
        state.filter.set(Field::Tool, "");
        assert_eq!(state.counts().0, 1);
    }

    #[test]
    fn a_client_filter_matches_the_field_and_absence_is_not_a_match() {
        let mut state = fresh();
        push(&mut state, r#","client":"claude-code""#);
        push(&mut state, r#","client":"cursor""#);
        // Written by a gateway that does not attribute clients at all.
        push(&mut state, "");
        assert_eq!(state.counts(), (3, 3));
        assert_eq!(state.log(NOW)[2].client, "—");

        state.filter.set(Field::Client, "claude-code");
        assert_eq!(state.counts().0, 1);
        assert_eq!(state.log(NOW)[0].client, "claude-code");

        // The same substring the `--client` flag takes, because it is the
        // same matcher: the TUI must not mean something narrower by the word.
        state.filter.set(Field::Client, "CLAUDE");
        assert_eq!(state.counts().0, 1);
        state.filter.set(Field::Client, "");
        assert_eq!(state.counts().0, 3);
    }

    /// …and the flag seeds it, so `--tui --client` starts where `--client`
    /// would have started the stream.
    #[test]
    fn the_client_flag_seeds_the_tui_filter() {
        let mut state = State::new(
            &WatchArgs {
                client: Some("cursor".to_owned()),
                ..args()
            },
            PathBuf::from("/traffic"),
        );
        push(&mut state, r#","client":"claude-code/2.1.3""#);
        push(&mut state, r#","client":"cursor/0.48""#);
        assert_eq!(state.counts(), (1, 2));
        assert_eq!(state.log(NOW)[0].client, "cursor/0.48");
    }

    #[test]
    fn a_status_filter_takes_the_words_the_format_uses() {
        let mut state = fresh();
        push(&mut state, "");
        push(&mut state, r#","ok":false,"error":"refused""#);
        state.filter.set(Field::Status, "error");
        assert_eq!(state.counts().0, 1);
        assert!(!state.log(NOW)[0].ok);
        state.filter.set(Field::Status, "ok");
        assert_eq!(state.counts().0, 1);
        assert!(state.log(NOW)[0].ok);
        // A word the vocabulary does not have takes the filter off rather
        // than silently matching nothing.
        state.filter.set(Field::Status, "maybe");
        assert_eq!(state.counts().0, 2);
    }

    #[test]
    fn free_text_searches_what_is_on_the_row_and_not_the_bodies() {
        let mut state = fresh();
        push(
            &mut state,
            r#","args":"{\"token\":\"ghp_realsecret\"}","error":"upstream refused""#,
        );
        push(&mut state, r#","server":"linear""#);

        state.filter.text = Some("REFUS".to_owned());
        assert_eq!(state.counts().0, 1);
        state.filter.text = Some("linear".to_owned());
        assert_eq!(state.counts().0, 1);
        // A captured body is never a search target: `/` must not be the way
        // a masked secret gets confirmed one character at a time.
        state.filter.text = Some("ghp_realsecret".to_owned());
        assert_eq!(state.counts().0, 0);
    }

    #[test]
    fn a_pause_holds_lines_and_resuming_lets_them_through() {
        let mut state = fresh();
        push(&mut state, "");
        state.on_key(Key::Char('p'));
        assert!(state.paused);
        push(&mut state, r#","tool":"during""#);
        push(&mut state, r#","tool":"also_during""#);
        // Held, not dropped: `p` is pressed because something scrolled past.
        assert_eq!(state.counts().0, 1);
        assert_eq!(state.held(), 2);

        state.on_key(Key::Char('p'));
        assert!(!state.paused);
        assert_eq!(state.counts().0, 3);
        assert_eq!(state.held(), 0);
        assert_eq!(state.log(NOW)[2].target, "also_during");
    }

    #[test]
    fn clear_empties_the_window_and_the_pause_buffer() {
        let mut state = fresh();
        push(&mut state, "");
        state.on_key(Key::Char('p'));
        push(&mut state, "");
        state.on_key(Key::Char('c'));
        assert_eq!(state.counts(), (0, 0));
        assert_eq!(state.held(), 0);
        assert!(state.selected().is_none());
        assert!(state.detail(NOW).is_none());
    }

    #[test]
    fn the_window_keeps_the_most_recent_records() {
        let mut state = fresh();
        for n in 0..WINDOW + 10 {
            push(&mut state, &format!(r#","duration_ms":{n}"#));
        }
        assert_eq!(state.counts(), (WINDOW, WINDOW));
        let rows = state.table(NOW);
        // The first ten are gone, so the fastest call still in the window is
        // the tenth one and not the first.
        assert_eq!(rows[0].calls, WINDOW);
        assert_eq!(rows[0].p50, 509);
    }

    #[test]
    fn selecting_leaves_the_follow_and_reaching_the_bottom_rejoins_it() {
        let mut state = fresh();
        for n in 0..3 {
            push(&mut state, &format!(r#","duration_ms":{n}"#));
        }
        assert!(state.follow);
        assert_eq!(state.selected(), Some(2));

        state.on_key(Key::Up);
        assert!(!state.follow);
        assert_eq!(state.selected(), Some(1));
        // A new line no longer drags the selection along.
        push(&mut state, "");
        assert_eq!(state.selected(), Some(1));

        state.on_key(Key::Char('j'));
        state.on_key(Key::Char('j'));
        assert!(state.follow);
        assert_eq!(state.selected(), Some(3));
        push(&mut state, "");
        assert_eq!(state.selected(), Some(4));
    }

    #[test]
    fn a_full_body_is_masked_exactly_as_the_stream_masks_it() {
        let body = r#","args":"{\"token\":\"ghp_realsecret\"}","response":"t0ken""#;
        let mut state = fresh();
        push(&mut state, body);
        let detail = state.detail(NOW).unwrap();
        let field = |name| {
            detail
                .fields
                .iter()
                .find(|(label, _)| *label == name)
                .map(|(_, value)| value.clone())
        };
        assert_eq!(field("args").as_deref(), Some(MASK));
        assert_eq!(field("response").as_deref(), Some(MASK));
        assert!(!format!("{detail:?}").contains("ghp_realsecret"));

        // `--show-secrets` is the same opt-out the stream has.
        let mut state = State::new(
            &WatchArgs {
                show_secrets: true,
                ..args()
            },
            PathBuf::from("/traffic"),
        );
        push(&mut state, body);
        assert!(format!("{:?}", state.detail(NOW).unwrap()).contains("ghp_realsecret"));
    }

    #[test]
    fn a_redacted_body_is_shown_as_the_gateway_wrote_it() {
        // Masking a second time would throw away the shape hint redaction
        // deliberately left legible and reveal nothing by doing so.
        let mut state = fresh();
        push(
            &mut state,
            r#","bodies":"redacted","args":"{\"token\":\"[redacted:ghp_…]\"}""#,
        );
        let detail = format!("{:?}", state.detail(NOW).unwrap());
        assert!(detail.contains("[redacted:ghp_…]"), "{detail}");
        assert!(!detail.contains(MASK), "{detail}");
    }

    #[test]
    fn a_record_with_no_bodies_gets_no_body_rows() {
        let mut state = fresh();
        push(&mut state, r#","kind":"list","tool":null"#);
        let detail = state.detail(NOW).unwrap();
        let labels: Vec<&str> = detail.fields.iter().map(|(label, _)| *label).collect();
        assert!(!labels.contains(&"args"), "{labels:?}");
        assert!(!labels.contains(&"response"), "{labels:?}");
        assert!(labels.contains(&"method"));
    }

    #[test]
    fn a_client_that_is_not_there_is_still_a_row() {
        let mut state = fresh();
        push(&mut state, "");
        let detail = state.detail(NOW).unwrap();
        assert!(detail.fields.contains(&("client", "—".to_owned())));
    }

    #[test]
    fn an_unreadable_line_is_skipped_rather_than_fatal() {
        let mut state = fresh();
        state.push_line("not json at all");
        state.push_line(r#"{"ts":1,"kind":"from-the-future"}"#);
        push(&mut state, "");
        assert_eq!(state.counts(), (1, 1));
    }

    #[test]
    fn the_filter_prompt_cycles_and_applies_what_was_typed() {
        let mut state = fresh();
        push(&mut state, "");
        push(&mut state, r#","server":"linear""#);

        state.on_key(Key::Char('f'));
        assert_eq!(state.prompt.as_ref().unwrap().label(), "tool");
        state.on_key(Key::Esc);
        assert!(state.prompt.is_none());

        // …and again, from where the cycle left off.
        state.on_key(Key::Char('f'));
        assert_eq!(state.prompt.as_ref().unwrap().label(), "status (ok/error)");
        state.on_key(Key::Esc);
        state.on_key(Key::Char('f'));
        assert_eq!(state.prompt.as_ref().unwrap().label(), "client");
        state.on_key(Key::Esc);
        state.on_key(Key::Char('f'));
        assert_eq!(state.prompt.as_ref().unwrap().label(), "server");

        for key in "linearX".chars() {
            state.on_key(Key::Char(key));
        }
        state.on_key(Key::Backspace);
        state.on_key(Key::Enter);
        assert!(state.prompt.is_none());
        assert_eq!(state.filter.server.as_deref(), Some("linear"));
        assert_eq!(state.counts().0, 1);
        assert_eq!(state.filter.summary(), "server=linear");
    }

    #[test]
    fn free_text_is_typed_at_its_own_prompt() {
        let mut state = fresh();
        push(&mut state, "");
        push(&mut state, r#","server":"linear""#);
        state.on_key(Key::Char('/'));
        assert_eq!(state.prompt.as_ref().unwrap().label(), "search");
        for key in "linear".chars() {
            state.on_key(Key::Char(key));
        }
        state.on_key(Key::Enter);
        assert_eq!(state.counts().0, 1);
        assert_eq!(state.filter.summary(), "/=linear");
    }

    #[test]
    fn a_prompt_swallows_the_keys_that_would_otherwise_act() {
        let mut state = fresh();
        state.on_key(Key::Char('/'));
        // `q` inside a prompt is a letter, not the door.
        assert_eq!(state.on_key(Key::Char('q')), Action::Continue);
        assert_eq!(state.on_key(Key::Char('p')), Action::Continue);
        assert!(!state.paused);
        assert_eq!(state.prompt.as_ref().unwrap().input, "qp");
    }

    #[test]
    fn every_advertised_key_does_something() {
        assert_eq!(fresh().on_key(Key::Char('q')), Action::Quit);
        assert_eq!(fresh().on_key(Key::Esc), Action::Quit);

        let mut state = fresh();
        state.on_key(Key::Enter);
        assert!(!state.detail, "enter toggles the detail pane");
        state.on_key(Key::Enter);
        assert!(state.detail);

        state.on_key(Key::Char('?'));
        assert!(state.help);
        // The overlay covers the panes, so the next key takes it away rather
        // than acting on something nobody can see.
        state.on_key(Key::Char('p'));
        assert!(!state.help);
        assert!(!state.paused);

        // Nothing in the help list is a key the state ignores.
        for (keys, _) in KEYS {
            for key in keys.chars().filter(char::is_ascii_graphic) {
                let mut state = fresh();
                state.on_key(Key::Char(key));
            }
        }
    }
}
