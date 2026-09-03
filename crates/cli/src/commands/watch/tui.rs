//! The `watch --tui` screen: three panes over the same tailed JSONL the plain
//! stream reads, plus the terminal loop that drives them.
//!
//! The loop is thin on purpose. Everything that decides *what* is on screen
//! lives in [`super::state`], which knows nothing about terminals; this module
//! turns a key event into a [`Key`], asks the state what changed, and paints
//! the answer. That split is what makes the panes testable without a TTY.

use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::Context as _;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, Wrap,
};

use super::state::{Action, Detail, KEYS, Key, LogRow, State, TableRow};
use super::{Follow, POLL, WatchArgs, clip};

/// How much of the screen the per-server table gets before the log starts
/// taking the rest. A third: enough for a handful of hot tools without
/// pushing the log — the pane people actually stare at — off the bottom.
const TABLE_SHARE: u16 = 33;

/// Rows the detail pane occupies when it is open, borders included: enough
/// for every metadata row plus the error and the two bodies under them.
const DETAIL_HEIGHT: u16 = 14;

/// Longest a value is allowed to be in a log cell before it is cut.
const CELL_CHARS: usize = 22;

pub(super) fn run(args: &WatchArgs, dir: PathBuf) -> anyhow::Result<()> {
    // Refused rather than degraded: a TUI written to a pipe is a screenful of
    // escape sequences, and the person who piped it wanted the stream that
    // has been there all along.
    if !std::io::stdout().is_terminal() {
        anyhow::bail!(
            "watch --tui needs a terminal, and stdout is not one — \
             use `mcpgw watch` for the line stream, or `mcpgw watch --json` for JSONL"
        );
    }
    let mut follow = Follow::new(dir.clone());
    let mut state = State::new(args, dir);
    let signalled = quit_on_signal();
    // `try_init` enables raw mode, switches to the alternate screen and
    // installs a panic hook that undoes both *before* the panic message is
    // printed. Without that hook a panic three panes deep would leave the
    // shell in raw mode with no echo, which is the worst failure a terminal
    // program has.
    let mut terminal = ratatui::try_init().context("cannot take over the terminal")?;
    let result = event_loop(&mut terminal, &mut state, &mut follow, &signalled);
    ratatui::restore();
    result
}

fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    state: &mut State,
    follow: &mut Follow,
    signalled: &AtomicBool,
) -> anyhow::Result<()> {
    let mut due = Instant::now();
    loop {
        if Instant::now() >= due {
            drain(state, follow);
            due = Instant::now() + POLL;
        }
        let now_ms = mcpgw_core::capture::now_millis();
        terminal.draw(|frame| draw(frame, state, now_ms))?;
        if signalled.load(Ordering::Relaxed) {
            return Ok(());
        }
        // Blocks until the next key or the next tail poll, whichever comes
        // first, so an idle TUI costs two wakeups a second and no more.
        let wait = due.saturating_duration_since(Instant::now());
        if event::poll(wait)?
            && let Event::Key(key) = event::read()?
        {
            if is_interrupt(key) {
                return Ok(());
            }
            if let Some(key) = translate(key)
                && state.on_key(key) == Action::Quit
            {
                return Ok(());
            }
        }
    }
}

/// One follow round. A read failure lands in the footer rather than on
/// stderr: under an alternate screen there is no stderr anyone can read, and
/// the stream's rule — report it, swallow it, retry in 500ms — still holds.
fn drain(state: &mut State, follow: &mut Follow) {
    match follow.poll() {
        Ok(lines) => {
            state.notice = None;
            for line in lines {
                state.push_line(&line);
            }
        }
        Err(err) => state.notice = Some(format!("{err:#} — retrying")),
    }
}

/// Ctrl-C. Raw mode turns it into an ordinary key rather than a signal, so
/// the habit that stops every other long-running mcpgw command has to be
/// honoured by hand here.
fn is_interrupt(key: KeyEvent) -> bool {
    key.kind == KeyEventKind::Press
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'))
}

fn translate(key: KeyEvent) -> Option<Key> {
    // Windows reports a press *and* a release for every key; without this
    // filter every keystroke would act twice there and once everywhere else.
    if key.kind != KeyEventKind::Press {
        return None;
    }
    Some(match key.code {
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        _ => return None,
    })
}

/// A flag the loop watches, raised when the process is asked to stop.
///
/// Raw mode already swallows the terminal's own Ctrl-C, so what is left is a
/// kill from outside: a supervisor stopping the session, a terminal closing.
/// Dying on one of those with the alternate screen still up leaves the shell
/// behind it unusable, which is the whole reason to catch them.
///
/// tokio is already a dependency and its signal support is the portable one,
/// so this costs a thread and a current-thread runtime rather than a new
/// crate — and the TUI stays a plain blocking loop.
fn quit_on_signal() -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(false));
    let raised = Arc::clone(&flag);
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            // No runtime means no signal handling, not a dead TUI: `q` and
            // Ctrl-C both still work.
            return;
        };
        runtime.block_on(wait_for_signal());
        raised.store(true, Ordering::Relaxed);
    });
    flag
}

#[cfg(unix)]
async fn wait_for_signal() {
    use tokio::signal::unix::{SignalKind, signal};

    let Ok(mut terminate) = signal(SignalKind::terminate()) else {
        let _ = tokio::signal::ctrl_c().await;
        return;
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
    }
}

#[cfg(not(unix))]
async fn wait_for_signal() {
    // Windows has no SIGTERM; `ctrl_c` there covers Ctrl-C and the console
    // control events a closing window sends.
    let _ = tokio::signal::ctrl_c().await;
}

/// Paints one frame. Pure in everything but the buffer it writes, which is
/// what lets the tests below render it against a `TestBackend`.
fn draw(frame: &mut Frame, state: &State, now_ms: u64) {
    let detail = state.detail.then(|| state.detail(now_ms)).flatten();
    let [head, top, middle, bottom, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Percentage(TABLE_SHARE),
        Constraint::Min(3),
        Constraint::Length(if detail.is_some() { DETAIL_HEIGHT } else { 0 }),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(header(state), head);
    frame.render_widget(table(state, now_ms), top);
    log(frame, middle, state, now_ms);
    if let Some(detail) = detail {
        frame.render_widget(detail_pane(&detail), bottom);
    }
    frame.render_widget(footer(state), foot);
    if state.help {
        help(frame);
    }
}

fn header(state: &State) -> Paragraph<'_> {
    let (shown, held) = state.counts();
    Paragraph::new(Line::from(vec![
        Span::styled("mcpgw watch", Style::new().bold()),
        Span::raw(format!("  {}", state.dir.display())),
        Span::styled(
            format!("  {shown}/{held} records"),
            Style::new().dark_gray(),
        ),
    ]))
}

fn table(state: &State, now_ms: u64) -> Table<'static> {
    // The five number columns are fixed; the two name columns split whatever
    // is left, so a wide terminal spends it on names rather than on a gap.
    let widths = [
        Constraint::Min(16),
        Constraint::Min(18),
        Constraint::Length(7),
        Constraint::Length(7),
        Constraint::Length(8),
        Constraint::Length(8),
        Constraint::Length(6),
    ];
    let head = Row::new(
        ["server", "target", "calls", "errors", "p50", "p95", "last"].map(|label| {
            // The sorted column says so it is, so `s` has a visible effect
            // even on a table whose order happens not to change.
            let style = if label == state.sort.as_str() {
                Style::new().bold().underlined()
            } else {
                Style::new().bold()
            };
            // The two name columns read left to right; the five numbers are
            // compared down the column, which is what right alignment is for.
            let text = Text::from(label);
            let text = if matches!(label, "server" | "target") {
                text
            } else {
                text.right_aligned()
            };
            Cell::from(text).style(style)
        }),
    );
    let rows = state.table(now_ms).into_iter().map(row);
    Table::new(rows, widths)
        .header(head)
        .block(Block::bordered().title(Line::from(format!(
            " servers · sorted by {} ",
            state.sort.as_str()
        ))))
}

fn row(entry: TableRow) -> Row<'static> {
    let errors = Span::styled(
        entry.errors.to_string(),
        if entry.errors > 0 {
            Style::new().red().bold()
        } else {
            Style::new().dark_gray()
        },
    );
    Row::new(vec![
        Cell::from(clip(&entry.server, 16)),
        Cell::from(clip(&entry.target, 18)),
        Cell::from(Text::from(entry.calls.to_string()).right_aligned()),
        Cell::from(Text::from(Line::from(errors)).right_aligned()),
        Cell::from(Text::from(format!("{}ms", entry.p50)).right_aligned()),
        Cell::from(Text::from(format!("{}ms", entry.p95)).right_aligned()),
        Cell::from(Text::from(entry.last_seen).right_aligned()),
    ])
}

/// The call log, oldest at the top so the newest line is the one at the
/// bottom edge — the same direction the plain stream scrolls.
fn log(frame: &mut Frame, area: Rect, state: &State, now_ms: u64) {
    let title = if state.paused {
        format!(" calls · PAUSED ({} held) ", state.held())
    } else if state.follow {
        " calls · following ".to_owned()
    } else {
        " calls · scrolled back ".to_owned()
    };
    let block = Block::bordered().title(Line::from(title));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let [columns, rows] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    frame.render_widget(
        Paragraph::new(Line::styled(
            // Lined up by hand with the row below, where the outcome mark
            // sits between the age and the server.
            format!(
                "{:>5}   {:<16} {:<22} {:<20} {:<14} {:>8}",
                "age", "server", "target", "method", "client", "took"
            ),
            Style::new().bold(),
        )),
        columns,
    );

    let items: Vec<ListItem> = state.log(now_ms).iter().map(item).collect();
    let mut list = ListState::default().with_selected(state.selected());
    frame.render_stateful_widget(
        List::new(items).highlight_style(Style::new().reversed()),
        rows,
        &mut list,
    );
}

fn item(entry: &LogRow) -> ListItem<'static> {
    let mark = if entry.ok { "✓" } else { "✗" };
    ListItem::new(Line::from(vec![
        Span::styled(format!("{:>5} ", entry.age), Style::new().dark_gray()),
        Span::styled(
            mark,
            if entry.ok {
                Style::new().green()
            } else {
                Style::new().red()
            },
        ),
        Span::raw(format!(
            " {:<16} {:<22} {:<20} {:<14} {:>8}",
            clip(&entry.server, 16),
            clip(&entry.target, CELL_CHARS),
            clip(entry.kind, 20),
            clip(&entry.client, 14),
            entry.duration,
        )),
    ]))
}

fn detail_pane(detail: &Detail) -> Paragraph<'static> {
    let lines: Vec<Line> = detail
        .fields
        .iter()
        .map(|(label, value)| {
            Line::from(vec![
                Span::styled(format!("{label:>9}  "), Style::new().dark_gray()),
                Span::raw(value.clone()),
            ])
        })
        .collect();
    Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .block(Block::bordered().title(" call "))
}

fn footer(state: &State) -> Paragraph<'static> {
    if let Some(prompt) = &state.prompt {
        // A caret rather than a real cursor: the cursor is parked off-screen
        // for the rest of the run and moving it only here would flicker.
        return Paragraph::new(Line::from(vec![
            Span::styled(format!("{}: ", prompt.label()), Style::new().bold()),
            Span::raw(prompt.input.clone()),
            Span::styled("▏", Style::new().slow_blink()),
            Span::styled(
                "   enter to apply · esc to cancel",
                Style::new().dark_gray(),
            ),
        ]));
    }
    if let Some(notice) = &state.notice {
        return Paragraph::new(Line::styled(
            format!("watch: {notice}"),
            Style::new().yellow(),
        ));
    }
    let filters = state.filter.summary();
    let filters = if filters.is_empty() {
        "no filter".to_owned()
    } else {
        filters
    };
    let mut spans = Vec::new();
    for (key, what) in [
        ("q", "quit"),
        ("f", "filter"),
        ("/", "search"),
        ("p", "pause"),
        ("c", "clear"),
        ("s", "sort"),
        ("?", "help"),
    ] {
        spans.push(Span::styled(key, Style::new().bold()));
        spans.push(Span::raw(format!(" {what}  ")));
    }
    spans.push(Span::styled(filters, Style::new().cyan()));
    Paragraph::new(Line::from(spans))
}

fn help(frame: &mut Frame) {
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("  {key:<10}"), Style::new().bold()),
                Span::raw(*what),
            ])
        })
        .collect();
    // Wide enough for the longest row above with room to spare, and tall
    // enough for the table plus its border, so the overlay never scrolls.
    let width = 56;
    let height = u16::try_from(lines.len())
        .unwrap_or(u16::MAX)
        .saturating_add(2);
    let area = centered(frame.area(), width, height);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(Block::bordered().title(" keys · any key closes ")),
        area,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    use super::*;

    const NOW: u64 = 1_767_225_600_000;

    fn args() -> WatchArgs {
        WatchArgs {
            server: None,
            tool: None,
            endpoint: None,
            session: None,
            json: false,
            tui: true,
            show_secrets: false,
        }
    }

    /// A screenful of the three panes, rendered against a backend that is a
    /// buffer rather than a terminal — which is the whole reason [`draw`]
    /// takes a state it does not own and a clock it does not read.
    fn frame(state: &State) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 34)).unwrap();
        terminal.draw(|frame| draw(frame, state, NOW)).unwrap();
        terminal.backend().to_string()
    }

    fn populated() -> State {
        let mut state = State::new(&args(), PathBuf::from("/traffic"));
        for (seconds, line) in [
            (
                12u64,
                r#"{"client":"claude-code","endpoint":"mcp","server":"github","tool":"create_issue","kind":"call","duration_ms":87,"ok":true,"args":"{\"title\":\"x\"}"}"#,
            ),
            (
                5,
                r#"{"client":"cursor","endpoint":"mcp","server":"github","tool":"search_code","kind":"call","duration_ms":210,"ok":false,"error":"upstream \"github\" failed after 3 attempt(s)"}"#,
            ),
            (
                0,
                r#"{"endpoint":"s/linear","server":"linear","kind":"list","duration_ms":4,"ok":true}"#,
            ),
        ] {
            let stamped = line.replacen(
                '{',
                &format!(r#"{{"ts":{},"session":"s3ss","#, NOW - seconds * 1000),
                1,
            );
            state.push_line(&stamped);
        }
        state
    }

    #[test]
    fn the_three_panes_render() {
        insta::assert_snapshot!(frame(&populated()));
    }

    #[test]
    fn the_help_overlay_covers_the_panes() {
        let mut state = populated();
        state.on_key(Key::Char('?'));
        insta::assert_snapshot!(frame(&state));
    }

    #[test]
    fn an_empty_state_still_draws_every_pane() {
        let state = State::new(&args(), PathBuf::from("/traffic"));
        let screen = frame(&state);
        assert!(screen.contains("servers"), "{screen}");
        assert!(screen.contains("calls"), "{screen}");
        assert!(screen.contains("no filter"), "{screen}");
    }

    #[test]
    fn no_captured_body_reaches_the_screen_unmasked() {
        let mut state = State::new(&args(), PathBuf::from("/traffic"));
        state.push_line(
            r#"{"ts":0,"session":"s3ss","server":"github","tool":"t","kind":"call","duration_ms":1,"ok":true,"args":"ghp_realsecret","response":"t0ken"}"#,
        );
        let screen = frame(&state);
        assert!(!screen.contains("ghp_realsecret"), "{screen}");
        assert!(!screen.contains("t0ken"), "{screen}");
        assert!(screen.contains("***"), "{screen}");
    }

    #[test]
    fn a_prompt_takes_the_footer_over() {
        let mut state = populated();
        state.on_key(Key::Char('/'));
        state.on_key(Key::Char('g'));
        let screen = frame(&state);
        assert!(screen.contains("search: g"), "{screen}");
    }

    #[test]
    fn the_detail_pane_folds_away() {
        let mut state = populated();
        assert!(frame(&state).contains(" call "));
        state.on_key(Key::Enter);
        assert!(!frame(&state).contains(" call "));
    }

    #[test]
    fn a_tail_failure_is_reported_in_the_footer_and_not_on_stderr() {
        let dir = tempfile::tempdir().unwrap();
        let blocked =
            mcpgw_core::capture::daily_path(dir.path(), mcpgw_core::capture::now_millis());
        std::fs::create_dir(&blocked).unwrap();
        let mut state = State::new(&args(), dir.path().to_path_buf());
        let mut follow = Follow::new(dir.path().to_path_buf());
        drain(&mut state, &mut follow);

        // The io error's wording is the platform's, not ours: a directory
        // where the day's file belongs opens fine and fails on read here,
        // and fails on open under Windows, so the two notices share neither
        // phrasing nor length. What the test pins is the part `watch` owns —
        // the failure becomes a notice that promises a retry, and that notice
        // is what the footer row paints, as much of it as the width allows.
        let notice = state
            .notice
            .clone()
            .expect("the failure is held as a notice");
        assert!(notice.ends_with("— retrying"), "{notice}");
        let screen = frame(&state);
        // `TestBackend` prints every row quoted and padded out to the width.
        let painted = screen.lines().last().unwrap().trim_matches('"').trim_end();
        assert!(!painted.is_empty(), "{screen}");
        assert!(format!("watch: {notice}").starts_with(painted), "{screen}");

        std::fs::remove_dir(&blocked).unwrap();
        std::fs::write(
            &blocked,
            "{\"ts\":0,\"session\":\"s\",\"server\":\"github\",\"kind\":\"list\",\"duration_ms\":1,\"ok\":true}\n",
        )
        .unwrap();
        drain(&mut state, &mut follow);
        assert!(state.notice.is_none());
        assert_eq!(state.counts(), (1, 1));
    }

    #[test]
    fn a_key_release_is_not_a_second_keystroke() {
        let press = KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE);
        assert_eq!(translate(press), Some(Key::Char('p')));
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('p'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        assert_eq!(translate(release), None);
    }

    #[test]
    fn ctrl_c_stops_the_loop_the_way_it_stops_the_stream() {
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(is_interrupt(ctrl_c));
        // …and a bare `c` is still the clear key.
        assert!(!is_interrupt(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
        assert_eq!(
            translate(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE)),
            Some(Key::Char('c'))
        );
    }

    #[test]
    fn the_overlay_is_centred_and_never_larger_than_the_screen() {
        let screen = Rect::new(0, 0, 80, 24);
        assert_eq!(centered(screen, 40, 10), Rect::new(20, 7, 40, 10));
        // A terminal smaller than the overlay gets the terminal.
        assert_eq!(centered(screen, 200, 200), screen);
    }
}
