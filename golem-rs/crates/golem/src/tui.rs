//! The full-screen console (phase 5 of docs/plan-portable-binary.md).
//!
//! A ratatui mirror of `scripts/tui.sh`, down to the look: the `▟█▙` mark,
//! the status segments that drop off one by one when the window narrows, the
//! bars, the `▸` marker on the selected entry, the recent-runs and engine-log
//! panes, and the hint on the last line. The shell composed that as one
//! string by hand and reprinted it every frame; here the same thing is a list
//! of styled lines and ratatui does the drawing.
//!
//! The split that makes it testable is the one the console already uses:
//! [`screen`] builds the lines from `&App` and touches no I/O, so a test
//! renders it and reads the text back. Everything that talks to the world -
//! the terminal, the key loop, the engine, the results file - lives in
//! [`App::refresh`] and [`run`], which the tests leave alone.
//!
//! The terminal is the classic hazard: raw mode left on turns the shell
//! unusable, and an alternate screen left entered hides everything typed
//! afterwards. [`RawTerminal`] is a guard, so the restore runs on the error
//! path and on a panic too, not only on the clean exit.

use std::io;
use std::path::Path;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use golem_core::results::{self, Kind};

use crate::engine;
use crate::paths;
use crate::state;

/// What a menu entry does. `main.rs` matches on this to run the real work;
/// the TUI itself only decides which one is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Status,
    Start,
    Stop,
    Restart,
    Bench,
    Eval,
    Tune,
    Report,
    Web,
    Quit,
}

/// The menu, in the order the screen shows it. A `&'static` slice rather than
/// a `Vec`: the labels are known at compile time and never change, so the app
/// borrows them instead of owning an allocation.
const ITEMS: &[(&str, Action)] = &[
    ("Status", Action::Status),
    ("Start", Action::Start),
    ("Stop", Action::Stop),
    ("Restart", Action::Restart),
    ("Bench", Action::Bench),
    ("Eval", Action::Eval),
    ("Tune", Action::Tune),
    ("Report", Action::Report),
    ("Web", Action::Web),
    ("Quit", Action::Quit),
];

/// The message line before anything has run.
const HINT: &str = "up/down move   enter run   r refresh   q quit";

/// The mark in the header, the same one the shell screen opens with.
const MARK: &str = "\u{259f}\u{2588}\u{2599}"; // ▟█▙
/// The selection marker.
const POINTER: &str = "\u{25b8}"; // ▸
/// The "there are more entries than rows" marker, with the direction to go.
const MORE: &str = "\u{2026} \u{2191}\u{2193}"; // … ↑↓

/// How many recorded rows the runs pane can show at once, and how many it
/// reads to choose from.
const RUNS_READ: usize = 12;
const RUNS_MAX: usize = 4;
/// How much of the engine's log the pane can show.
const LOG_TAIL: usize = 40;
const LOG_MAX: usize = 6;
/// The console's port: `web.port`'s default, and what the header's `web`
/// segment reports on.
const WEB_PORT: u16 = 8787;

/// What the header says about the machine, read once per refresh.
///
/// Never per frame: `vram_mib` runs nvidia-smi and `web_up` opens a socket,
/// and a menu that stalls while it waits is worse than one that is a second
/// out of date.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub up: bool,
    pub vram_mib: Option<u64>,
    pub ram_free_gb: Option<f64>,
    pub alias: String,
    pub engine: String,
    pub web_up: bool,
}

impl Default for Facts {
    fn default() -> Self {
        // The defaults a machine with nothing running would show. Defaulting
        // here rather than reading `.env` keeps `App::new` free of I/O.
        Facts {
            up: false,
            vram_mib: None,
            ram_free_gb: None,
            alias: String::new(),
            engine: "llama.cpp".to_string(),
            web_up: false,
        }
    }
}

/// The whole of the screen's state: what is selected, what the last action
/// said, and everything the header and the panes show.
pub struct App {
    items: &'static [(&'static str, Action)],
    selected: usize,
    message: String,
    facts: Facts,
    runs: Vec<String>,
    log: Vec<String>,
}

impl Default for App {
    fn default() -> Self {
        App::new()
    }
}

impl App {
    /// A fresh app: the first entry selected, the hint in the message line,
    /// and no facts yet - the header reads "down" until [`run`] refreshes it.
    /// Deliberately does no I/O.
    pub fn new() -> App {
        App {
            items: ITEMS,
            selected: 0,
            message: HINT.to_string(),
            facts: Facts::default(),
            runs: Vec::new(),
            log: Vec::new(),
        }
    }

    // The four accessors below are the test's window into the app: the
    // binary drives it through `next`/`previous`/`selected`/`set_message`
    // and the refresh, and never needs to look inside. Gated so the
    // binary build does not carry API nothing calls.

    /// The menu, label and action, in the order it is drawn.
    #[cfg(test)]
    pub fn items(&self) -> &[(&'static str, Action)] {
        self.items
    }

    /// The action the marker is on.
    pub fn selected(&self) -> Action {
        self.items[self.selected].1
    }

    /// Move down one entry, wrapping past the last back to the first.
    pub fn next(&mut self) {
        self.selected = (self.selected + 1) % self.items.len();
    }

    /// Move up one entry, wrapping past the first back to the last.
    pub fn previous(&mut self) {
        self.selected = (self.selected + self.items.len() - 1) % self.items.len();
    }

    /// Replace the message line with the result of the last action.
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
    }

    /// What the header shows, so a test can draw a populated screen without
    /// a machine to read it from.
    #[cfg(test)]
    pub fn show_facts(&mut self, facts: Facts) {
        self.facts = facts;
    }

    /// The runs pane's rows, newest last.
    #[cfg(test)]
    pub fn set_runs(&mut self, runs: Vec<String>) {
        self.runs = runs;
    }

    /// The engine-log pane's rows.
    #[cfg(test)]
    pub fn set_log(&mut self, log: Vec<String>) {
        self.log = log;
    }

    /// Read everything the screen shows, from the places the shell reads.
    ///
    /// One pass over the world per call, never per frame: two sockets, an
    /// nvidia-smi, a PowerShell round trip for the free memory, and the
    /// results file.
    fn refresh(&mut self, home: &Path) {
        let status = engine::status(home);
        self.facts = Facts {
            up: status.up,
            // Only worth asking when something is loaded, and only then does
            // the number mean anything.
            vram_mib: if status.up { state::vram_mib() } else { None },
            ram_free_gb: state::ram_free_gb(),
            alias: state::env_value(home, "MODEL_ALIAS").unwrap_or_default(),
            engine: state::env_value(home, "ENGINE").unwrap_or_else(|| "llama.cpp".to_string()),
            web_up: state::web_up(WEB_PORT),
        };
        self.runs = run_lines(home);
        self.log = log_lines(home);
    }
}

/// The last few recorded rows, in the shell screen's shape:
/// `  59.3 t/s  ornith-1.5-35b             3.5B act  llama.cpp`.
fn run_lines(home: &Path) -> Vec<String> {
    let Ok((rows, _)) = results::read(&paths::results(home)) else { return Vec::new() };
    let start = rows.len().saturating_sub(RUNS_READ);
    rows[start..].iter().map(run_line).collect()
}

fn run_line(row: &golem_core::results::Row) -> String {
    // The model column is fixed width so the numbers line up; a longer name
    // is cut rather than allowed to push the rest of the row off screen.
    let model: String = row.model.chars().take(26).collect();
    if row.kind == Kind::Reject {
        return format!("  reject  {model:<26} {}", row.engine);
    }
    let decode = row
        .decode_median
        .as_ref()
        .map(|n| n.text.clone())
        .unwrap_or_else(|| "-".to_string());
    let active = row
        .params_active_b
        .map(|p| format!("{p:.1}"))
        .unwrap_or_else(|| "-".to_string());
    format!("{decode:>7} t/s  {model:<26} {active:>5}B act  {}", row.engine)
}

/// The tail of the engine's log, indented to sit under its pane heading.
fn log_lines(home: &Path) -> Vec<String> {
    let (_, text) = state::log_tail(home, LOG_TAIL);
    if text.is_empty() {
        return vec!["  (no log yet)".to_string()];
    }
    text.lines().map(|line| format!("  {}", line.trim_end())).collect()
}

/// Render one frame from the app alone - no I/O, so a test can draw into a
/// buffer and read back what a user would see.
pub fn draw(app: &App, frame: &mut Frame) {
    let area = frame.area();
    frame.render_widget(Paragraph::new(screen(app, area.width, area.height)), area);
}

/// The whole screen as lines: the header, the menu, whatever panes fit, and
/// the hint.
///
/// The vertical budget is spent in that order. The menu is never squeezed
/// below three rows - it is the reason the screen exists - so on a short
/// window the panes go first, then the menu itself is windowed around the
/// selection, and only a window too small for even that overflows (ratatui
/// clips it).
fn screen(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    let mut out = Vec::with_capacity(height as usize);

    out.push(bar('=', w));
    out.push(header(app, w));
    out.push(bar('-', w));

    let count = app.items.len();
    let room = (height as i32 - 6).max(3) as usize;
    let (start, shown) = window(app.selected, count, room);
    for i in start..start + shown {
        let (label, _) = app.items[i];
        if i == app.selected {
            out.push(Line::from(Span::styled(
                format!("  {POINTER} {label}"),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            )));
        } else {
            out.push(Line::from(format!("    {label}")));
        }
    }
    let marker = shown < count;
    if marker {
        out.push(Line::from(Span::styled(format!("    {MORE}"), dim())));
    }
    out.push(bar('-', w));

    // What is left after the menu and the chrome - four bars, the header and
    // the hint - is the panes'. Each costs a heading line plus its rows.
    let menu_rows = shown + usize::from(marker);
    let mut avail = (height as i32 - (4 + menu_rows as i32) - 2).max(0) as usize;

    let run_rows = if avail >= 3 { (avail - 1).min(RUNS_MAX) } else { 0 };
    if run_rows > 0 {
        avail -= run_rows + 1;
    }
    let log_rows = if avail >= 3 { (avail - 1).min(LOG_MAX) } else { 0 };

    if run_rows > 0 {
        out.push(section("recent runs"));
        out.extend(tail(&app.runs, run_rows));
    }
    if log_rows > 0 {
        out.push(section("engine log"));
        out.extend(tail(&app.log, log_rows));
    }

    out.push(bar('-', w));
    // The last line is the hint until an action replaces it with its result.
    out.push(Line::from(Span::styled(format!("  {}", app.message), dim())));
    out
}

/// The last `rows` of `lines`, blank-padded to exactly `rows` so the screen
/// below them does not jump about as entries arrive.
fn tail(lines: &[String], rows: usize) -> Vec<Line<'static>> {
    let start = lines.len().saturating_sub(rows);
    let mut out: Vec<Line<'static>> = lines[start..]
        .iter()
        .map(|line| Line::from(format!("  {line}")))
        .collect();
    while out.len() < rows {
        out.push(Line::default());
    }
    out
}

/// Which slice of the menu to draw, so the marker is always on screen.
fn window(selected: usize, count: usize, room: usize) -> (usize, usize) {
    if count <= room {
        return (0, count);
    }
    // One row of the room is the "more" marker's.
    let shown = room.saturating_sub(1).max(1);
    let start = selected.saturating_sub(shown / 2).min(count - shown);
    (start, shown)
}

/// A full-width rule.
fn bar(ch: char, width: usize) -> Line<'static> {
    Line::from(Span::styled(ch.to_string().repeat(width), dim()))
}

/// A pane's heading.
fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(format!("  {title}"), dim()))
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// The top line: what the machine is doing, in the shell screen's segments.
///
/// Segments are dropped from the right until the line fits. Cutting a styled
/// string instead would slice it mid-escape and colour the rest of the
/// terminal - which is why the shell builds a plain and a coloured copy in
/// step, and why this drops whole segments.
fn header(app: &App, width: usize) -> Line<'static> {
    let facts = &app.facts;
    let mut segments: Vec<(String, Style)> = vec![(
        format!("{MARK} GOLEM"),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    segments.push(if facts.up {
        ("  \u{25cf} up".to_string(), Style::default().fg(Color::Green))
    } else {
        ("  \u{25cb} down".to_string(), Style::default().fg(Color::Red))
    });
    if let Some(vram) = facts.vram_mib {
        segments.push((format!("   {vram} MiB VRAM"), Style::default()));
    }
    if !facts.alias.is_empty() {
        segments.push((
            format!("   {}", facts.alias),
            Style::default().add_modifier(Modifier::BOLD),
        ));
    }
    segments.push((format!("  [{}]", facts.engine), dim()));
    segments.push(if facts.web_up {
        ("  web up".to_string(), Style::default().fg(Color::Green))
    } else {
        ("  web down".to_string(), dim())
    });
    if let Some(ram) = facts.ram_free_gb {
        segments.push((format!("   {ram:.1} GB free"), Style::default()));
    }

    let mut used = 2;
    let mut spans = vec![Span::raw("  ")];
    for (text, style) in segments {
        let len = text.chars().count();
        if used + len > width {
            break;
        }
        used += len;
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

/// The terminal in raw mode on the alternate screen, restored on drop.
///
/// A guard rather than a matching pair of calls: the restore has to happen
/// when [`run`] returns an error and when the event loop panics, not only on
/// the clean exit, and a `Drop` is the one place that always runs.
struct RawTerminal;

impl RawTerminal {
    fn enter() -> io::Result<RawTerminal> {
        enable_raw_mode()?;
        // Past this point raw mode is on. If the screen switch fails the
        // guard does not exist yet, so undo raw mode here by hand.
        if let Err(e) = execute!(io::stdout(), EnterAlternateScreen, cursor::Hide) {
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(RawTerminal)
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
    }
}

/// The event loop, on the real terminal.
///
/// `act` is the one thing this does not decide for itself: it is handed each
/// chosen action and returns the line to show. Quit never reaches it - the
/// loop returns on its own - so `act` never has to know the TUI exists.
pub fn run(home: &Path, mut act: impl FnMut(Action) -> String) -> io::Result<()> {
    let mut app = App::new();
    app.refresh(home); // the state at start-up

    let _terminal = RawTerminal::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    loop {
        terminal.draw(|frame| draw(&app, frame))?;

        let Event::Key(key) = event::read()? else { continue };
        // Windows reports a key's press and its release; acting on both would
        // run every action twice.
        if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
            continue;
        }

        match key.code {
            KeyCode::Up | KeyCode::Char('k') => app.previous(),
            KeyCode::Down | KeyCode::Char('j') => app.next(),
            // The facts are read at start-up and after an action; a machine
            // that changed under the screen - a game started, a download
            // ended - is what this is for.
            KeyCode::Char('r') => app.refresh(home),
            KeyCode::Enter => {
                let action = app.selected();
                if action == Action::Quit {
                    break;
                }
                let result = act(action);
                app.set_message(result);
                app.refresh(home); // the action changed things
            }
            // Ctrl+C is a key event, not a signal, once raw mode is on, so it
            // is caught here or not at all.
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
            KeyCode::Char('q') | KeyCode::Esc => break,
            _ => {}
        }
    }
    Ok(())
}
