//! The full-screen console (`golem tui`, and `golem` with no command).
//!
//! The look: the two-line mark (`golem_core::mark`, the same cells as the
//! icon), the status segments beside it that drop off one by one when the
//! window narrows, thin rules, the `▸` marker on the selected entry, the
//! recent-runs and engine-log panes, and the hint on the last line. State
//! has one colour per meaning, as on the page: green serving, grey stopped.
//! The screen is a list of styled lines, and ratatui does the drawing.
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
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, KeyModifiers,
    MouseEventKind,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{cursor, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::{Frame, Terminal};

use golem_core::mark;
use golem_core::results::{self, Kind};

use crate::config;
use crate::engine;
use crate::paths;
use crate::machine;
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
    Console,
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
    ("Console", Action::Console),
    ("Quit", Action::Quit),
];

/// The message line before anything has run.
const HINT: &str = "up/down move   enter run   mouse wheel scroll   r refresh   q quit";

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

/// How long the loop waits for a key before looking at what else happened.
///
/// The loop has a second job - an action running on its own thread - and
/// blocking in `event::read()` would only notice it when the user pressed
/// something. Short enough that a finished action is on screen at once.
const POLL: Duration = Duration::from_millis(80);

/// How long a machine reading the header shows is reused.
///
/// `ram_free_gb` spawns PowerShell, which costs ~0.4s measured, and the
/// header is refreshed after every action - so a figure a few seconds old is
/// a much better trade than a screen that stalls every time. `r` forces a
/// fresh read for a person who wants the live number.
const FACTS_TTL: Duration = Duration::from_secs(10);

/// What the header says about the machine, read once per refresh.
///
/// Never per frame: `vram_mib` runs nvidia-smi and `web_up` opens a socket,
/// and a menu that stalls while it waits is worse than one that is a second
/// out of date.
#[derive(Debug, Clone, PartialEq)]
pub struct Facts {
    pub up: bool,
    pub vram_mib: Option<u64>,
    /// The card's size, for the `used/total` reading.
    pub vram_total_mib: Option<u64>,
    pub ram_free_gb: Option<f64>,
    pub alias: String,
    pub engine: String,
    pub web_up: bool,
    /// The port the service hosts the console on.
    pub console_port: u16,
}

impl Default for Facts {
    fn default() -> Self {
        // The defaults a machine with nothing running would show. Defaulting
        // here rather than reading `golem.toml` keeps `App::new` free of I/O.
        Facts {
            up: false,
            vram_mib: None,
            vram_total_mib: None,
            ram_free_gb: None,
            alias: String::new(),
            engine: "llama.cpp".to_string(),
            web_up: false,
            console_port: crate::desktop::CONSOLE_PORT,
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
    /// When the expensive machine readings in `facts` were taken, so a
    /// redraw does not pay for them again. See [`FACTS_TTL`].
    read_at: Option<Instant>,
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
            read_at: None,
        }
    }

    /// The label of the entry the marker is on, for a message about it.
    pub fn selected_label(&self) -> &'static str {
        self.items[self.selected].0
    }

    // `items`, `show_facts`, `set_runs` and `set_log` are the tests' window
    // into the app (`tests/tui.rs`): the binary drives it through
    // `next`/`previous`/`selected`/`set_message` and the refresh, and never
    // needs to look inside.

    /// The menu, label and action, in the order it is drawn.
    pub fn items(&self) -> &[(&'static str, Action)] {
        self.items
    }

    /// The action the marker is on.
    pub fn selected(&self) -> Action {
        self.items[self.selected].1
    }

    /// Move down one entry, and stop at the last.
    ///
    /// A list, not a wheel: wrapping from Quit back to Status reads as the
    /// marker jumping rather than moving, and the ends are worth being able
    /// to feel for.
    pub fn next(&mut self) {
        if self.selected + 1 < self.items.len() {
            self.selected += 1;
        }
    }

    /// Move up one entry, and stop at the first.
    pub fn previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Replace the message line with the result of the last action.
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
    }

    /// What the header shows, so a test can draw a populated screen without
    /// a machine to read it from.
    pub fn show_facts(&mut self, facts: Facts) {
        self.facts = facts;
    }

    /// The runs pane's rows, newest last.
    pub fn set_runs(&mut self, runs: Vec<String>) {
        self.runs = runs;
    }

    /// The engine-log pane's rows.
    pub fn set_log(&mut self, log: Vec<String>) {
        self.log = log;
    }

    /// Read everything the screen shows.
    ///
    /// One pass over the world per call, never per frame: two sockets (the
    /// engine and the console), an nvidia-smi, a PowerShell round trip for
    /// the free memory, `golem.toml`, the results file and the engine's log.
    ///
    /// `force` re-reads the machine even when a value is fresh; the automatic
    /// refreshes after an action leave the expensive readings alone for
    /// [`FACTS_TTL`], and `r` forces them.
    fn refresh(&mut self, home: &Path, force: bool) {
        let status = engine::status(home);
        let fresh = self.read_at.is_some_and(|at| at.elapsed() < FACTS_TTL);
        let expensive = force || !fresh;
        // Only worth asking when something is loaded, and only then does the
        // number mean anything.
        let gpu = if status.up && expensive { machine::gpu() } else { None };
        self.facts = Facts {
            up: status.up,
            vram_mib: if expensive { gpu.as_ref().map(|g| g.used_mib) } else { self.facts.vram_mib },
            vram_total_mib: if expensive {
                gpu.as_ref().map(|g| g.total_mib)
            } else {
                self.facts.vram_total_mib
            },
            ram_free_gb: if expensive { machine::ram_free_gb() } else { self.facts.ram_free_gb },
            alias: config::get(home, "MODEL_ALIAS").unwrap_or_default(),
            engine: config::get(home, "ENGINE").unwrap_or_else(|| "llama.cpp".to_string()),
            web_up: state::web_up(crate::desktop::console_port(home)),
            console_port: crate::desktop::console_port(home),
        };
        if expensive {
            self.read_at = Some(Instant::now());
        }
        self.runs = run_lines(home);
        self.log = log_lines(home);
    }
}

/// The last few recorded rows, one line each:
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
    let h = height as usize;
    let mut out = Vec::with_capacity(h);

    // What a small window keeps, in the order it matters: the menu first,
    // then the hint, then the header, then the panes. Drawing top-down and
    // letting the terminal clip would leave a three-row window showing a
    // header and two rules - and not the menu, the one thing the screen is
    // *for*.
    let header_rows = if h >= 8 { 3 } else { 0 };
    let hint_rows = if h >= 3 { 2 } else { 0 };

    if header_rows > 0 {
        out.extend(header(app, w));
        out.push(rule(w));
    }

    let count = app.items.len();
    // The menu takes what is left after the chrome, and at least one row: a
    // selection the user cannot see is worse than a clipped one.
    let room = h.saturating_sub(header_rows + hint_rows).max(1);
    let (start, shown) = window(app.selected, count, room);
    for i in start..start + shown {
        let (label, _) = app.items[i];
        if i == app.selected {
            out.push(Line::from(Span::styled(
                format!("  {POINTER} {label}"),
                Style::default().fg(PATINA).add_modifier(Modifier::BOLD),
            )));
        } else {
            out.push(Line::from(format!("    {label}")));
        }
    }
    let marker = shown < count;
    if marker {
        out.push(Line::from(Span::styled(format!("    {MORE}"), dim())));
    }

    // What is left after the menu and the chrome - the bars, the header and
    // the hint - is the panes'. Each costs a heading line plus its rows.
    let menu_rows = shown + usize::from(marker);
    let mut avail = h.saturating_sub(header_rows + hint_rows + menu_rows + 1);

    let run_rows = if avail >= 3 { (avail - 1).min(RUNS_MAX) } else { 0 };
    if run_rows > 0 {
        avail -= run_rows + 1;
    }
    let log_rows = if avail >= 3 { (avail - 1).min(LOG_MAX) } else { 0 };

    // The rule that closes the menu only when something is drawn under it: an
    // empty window is better off with the row.
    if run_rows > 0 || log_rows > 0 {
        out.push(rule(w));
        if run_rows > 0 {
            out.push(section("recent runs"));
            out.extend(tail(&app.runs, run_rows));
        }
        if log_rows > 0 {
            out.push(section("engine log"));
            out.extend(tail(&app.log, log_rows));
        }
    }

    if hint_rows > 0 {
        out.push(rule(w));
        // The last line is the hint until an action replaces it with its result.
        out.push(Line::from(Span::styled(format!("  {}", app.message), dim())));
    }
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

/// A full-width thin rule.
fn rule(width: usize) -> Line<'static> {
    Line::from(Span::styled("\u{2500}".repeat(width), dim()))
}

/// The selection's colour: the page's patina accent.
const PATINA: Color = Color::Rgb(0x4d, 0xbf, 0xab);
/// Serving, as on the page and in the tray.
const SERVING: Color = Color::Rgb(0x55, 0xc2, 0x7a);

/// The mark's body colour, from the mark itself.
fn clay() -> Style {
    let [r, g, b] = mark::CLAY;
    Style::default().fg(Color::Rgb(r, g, b))
}

/// A pane's heading.
fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(format!("  {title}"), dim()))
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// The two header lines: the mark, and what the machine is doing beside it.
///
/// The first line is the state, the model, the VRAM and the engine; the
/// second the console and the free memory. Segments are dropped from the
/// right until a line fits: a segment is either whole on the line or not
/// there, never cut mid-word.
fn header(app: &App, width: usize) -> Vec<Line<'static>> {
    let facts = &app.facts;
    let mut first: Vec<(String, Style)> = vec![
        (mark::GLYPHS[0].to_string(), clay()),
        (" GOLEM".to_string(), Style::default().add_modifier(Modifier::BOLD)),
    ];
    first.push(if facts.up {
        ("   \u{25cf} serving".to_string(), Style::default().fg(SERVING))
    } else {
        ("   \u{25cb} stopped".to_string(), dim())
    });
    if !facts.alias.is_empty() {
        first.push((format!("   {}", facts.alias), Style::default().add_modifier(Modifier::BOLD)));
    }
    let gb = |mib: u64| mib as f64 / 1024.0;
    match (facts.vram_mib, facts.vram_total_mib) {
        (Some(used), Some(total)) => {
            first.push((format!("   {:.1}/{:.1} GB VRAM", gb(used), gb(total)), Style::default()))
        }
        (Some(used), None) => first.push((format!("   {:.1} GB VRAM", gb(used)), Style::default())),
        _ => {}
    }
    first.push((format!("   [{}]", facts.engine), dim()));

    let mut second: Vec<(String, Style)> = vec![(mark::GLYPHS[1].to_string(), clay())];
    second.push(if facts.web_up {
        (format!(" console :{}", facts.console_port), Style::default().fg(SERVING))
    } else {
        (" console off".to_string(), dim())
    });
    if let Some(ram) = facts.ram_free_gb {
        second.push((format!("   {ram:.1} GB free"), dim()));
    }
    vec![fit(first, width), fit(second, width)]
}

/// The segments that fit in `width` after the two-column indent, whole.
fn fit(segments: Vec<(String, Style)>, width: usize) -> Line<'static> {
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
        // Mouse capture is what makes the wheel arrive at all.
        if let Err(e) =
            execute!(io::stdout(), EnterAlternateScreen, cursor::Hide, EnableMouseCapture)
        {
            let _ = disable_raw_mode();
            return Err(e);
        }
        Ok(RawTerminal)
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        // Capture off before the screen goes back: a terminal left reporting
        // the mouse turns every later click into escape codes at the prompt.
        let _ = execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen, cursor::Show);
    }
}

/// The event loop, on the real terminal.
///
/// `act` is the one thing this does not decide for itself: it is handed each
/// chosen action and returns the line to show. Quit never reaches it - the
/// loop returns on its own - so `act` never has to know the TUI exists.
///
/// **An action runs on a thread of its own.** `up` installs an engine and
/// fetches a model before it answers, and a screen that stops drawing for the
/// minutes that takes cannot be told from one that has crashed. The loop
/// keeps drawing, the message line says what is
/// running, and the result lands when it lands. One at a time, as everywhere
/// else: two of these would fight over one engine, one pid file and one card.
pub fn run(
    home: &Path,
    act: impl Fn(Action) -> String + Send + Sync + 'static,
) -> io::Result<()> {
    let mut app = App::new();

    let _terminal = RawTerminal::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    // The menu is up before the machine is read. Reading it costs about a
    // second - the PowerShell call for free memory alone is ~0.4s - and a
    // blank window for that long reads as a screen that is not starting. The
    // header fills in on the next frame.
    terminal.draw(|frame| draw(&app, frame))?;
    app.refresh(home, true); // the state at start-up

    let act = Arc::new(act);
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut running: Option<&'static str> = None;

    loop {
        // Every turn of the loop draws, which is also what makes a resize
        // work: the event is not interesting in itself, the redraw is, and
        // ratatui reads the new size on the way through.
        terminal.draw(|frame| draw(&app, frame))?;

        // Polled, not blocking: a finished action has to reach the screen
        // even when nobody is typing.
        if event::poll(POLL)? {
            let key = match event::read()? {
                Event::Key(key) => key,
                // One notch of the wheel moves one row, like an arrow key.
                Event::Mouse(mouse) => {
                    match mouse.kind {
                        MouseEventKind::ScrollUp => app.previous(),
                        MouseEventKind::ScrollDown => app.next(),
                        _ => {}
                    }
                    continue;
                }
                _ => continue,
            };
            // Windows reports a key's press and its release; acting on both
            // would run every action twice.
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }

            match key.code {
                KeyCode::Up | KeyCode::Char('k') => app.previous(),
                KeyCode::Down | KeyCode::Char('j') => app.next(),
                // The facts are read at start-up and after an action; a machine
                // that changed under the screen - a game started, a download
                // ended - is what this is for. `r` re-reads the expensive ones
                // too, which the automatic refresh leaves alone for a while.
                KeyCode::Char('r') => app.refresh(home, true),
                KeyCode::Enter => {
                    let action = app.selected();
                    if action == Action::Quit {
                        break;
                    }
                    if let Some(busy) = running {
                        app.set_message(format!("{busy}: still running (one action at a time)"));
                    } else {
                        let (tx, act) = (tx.clone(), Arc::clone(&act));
                        running = Some(app.selected_label());
                        app.set_message(format!("{}: running...", app.selected_label()));
                        std::thread::spawn(move || {
                            let _ = tx.send(act(action));
                        });
                    }
                }
                // Ctrl+C is a key event, not a signal, once raw mode is on, so it
                // is caught here or not at all.
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break,
                KeyCode::Char('q') | KeyCode::Esc => break,
                _ => {}
            }
        }

        // The action's answer, whenever it arrives.
        if let Ok(message) = rx.try_recv() {
            running = None;
            app.set_message(message);
            app.refresh(home, false); // the action changed things
        }
    }
    Ok(())
}
