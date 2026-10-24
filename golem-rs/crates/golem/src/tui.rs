//! The full-screen screen (`golem tui`, and `golem` typed in a terminal).
//!
//! The look follows the console: the two-line mark in its amber with the
//! machine's state beside it, the actions as a compact grid, the model list
//! as the main area - the default marked, its speed in the serving colour,
//! each model's role and best decode on this machine - the engine's log
//! under it, and one line at the bottom for the hint, the last result, or
//! the pull prompt. The screen is a list of styled lines, and ratatui does
//! the drawing.
//!
//! The split that makes it testable: [`screen`] builds the lines from
//! `&App` and touches no I/O, so a test renders it and reads the text back.
//! Everything that talks to the world - the terminal, the key loop, the
//! engine, the results file - lives in [`App::refresh`] and [`run`], which
//! the tests leave alone.
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

use golem_core::mark::{self, Spark};
use golem_core::results::{self, Kind};

use crate::config;
use crate::engine;
use crate::machine;
use crate::paths;
use crate::state;

/// What an entry or a key does. `main.rs` matches on this to run the real
/// work; the screen itself only decides which one is chosen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Start,
    Stop,
    Restart,
    Console,
    Bench,
    Eval,
    Tune,
    Report,
    /// Fetch a model; the argument is what the pull prompt was given.
    Pull,
}

/// The action grid, row by row: the engine and the console on the first,
/// the measurements on the second.
const GRID: [[(&str, Action); COLS]; 2] = [
    [("Start", Action::Start), ("Stop", Action::Stop), ("Restart", Action::Restart), ("Console", Action::Console)],
    [("Bench", Action::Bench), ("Eval", Action::Eval), ("Tune", Action::Tune), ("Report", Action::Report)],
];
const COLS: usize = 4;
/// One grid cell's width: the marker, the longest label, and a gap.
const CELL: usize = 11;

/// The bottom line before anything has run.
const HINT: &str = "arrows choose   enter run   p pull   w console   r refresh   q quit";

/// The selection marker.
const POINTER: &str = "\u{25b8}"; // ▸

/// How many models the list can show at once.
const MODELS_MAX: usize = 8;
/// How much of the engine's log is read; the pane shows as much of it as
/// the window leaves room for.
const LOG_TAIL: usize = 40;
const LOG_MAX: usize = LOG_TAIL;

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
/// and a screen that stalls while it waits is worse than one that is a
/// second out of date.
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

/// One model in the list.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRow {
    pub name: String,
    /// The library's role, empty when it lists none.
    pub role: String,
    pub default: bool,
    /// The file is in `data/models`.
    pub present: bool,
    /// The best decode measured on this machine, as recorded.
    pub best: Option<String>,
}

/// The whole of the screen's state: what is selected, what the bottom line
/// says, the pull prompt when it is open, and everything the panes show.
pub struct App {
    selected: usize,
    message: String,
    /// The pull prompt's text while it is open.
    input: Option<String>,
    facts: Facts,
    models: Vec<ModelRow>,
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
    /// A fresh app: the first action selected, the hint on the bottom line,
    /// and no facts yet - the header reads "stopped" until [`run`] refreshes
    /// it. Deliberately does no I/O.
    pub fn new() -> App {
        App {
            selected: 0,
            message: HINT.to_string(),
            input: None,
            facts: Facts::default(),
            models: Vec::new(),
            log: Vec::new(),
            read_at: None,
        }
    }

    // `labels`, `show_facts`, `set_models` and `set_log` are the tests'
    // window into the app (`tests/tui.rs`); the binary drives it through the
    // moves, `selected`, the prompt and the refresh.

    /// Every action's label, row by row.
    pub fn labels() -> Vec<&'static str> {
        GRID.iter().flatten().map(|(label, _)| *label).collect()
    }

    /// The action the marker is on.
    pub fn selected(&self) -> Action {
        GRID[self.selected / COLS][self.selected % COLS].1
    }

    fn selected_label(&self) -> &'static str {
        GRID[self.selected / COLS][self.selected % COLS].0
    }

    /// Move to the next action, and stop at the last. The ends are worth
    /// being able to feel for, so nothing wraps.
    pub fn next(&mut self) {
        if self.selected + 1 < COLS * GRID.len() {
            self.selected += 1;
        }
    }

    /// Move to the previous action, and stop at the first.
    pub fn previous(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Move to the row below, keeping the column.
    pub fn down(&mut self) {
        if self.selected + COLS < COLS * GRID.len() {
            self.selected += COLS;
        }
    }

    /// Move to the row above, keeping the column.
    pub fn up(&mut self) {
        if self.selected >= COLS {
            self.selected -= COLS;
        }
    }

    /// Replace the bottom line with the result of the last action.
    pub fn set_message(&mut self, message: impl Into<String>) {
        self.message = message.into();
    }

    /// Open the pull prompt.
    pub fn open_prompt(&mut self) {
        self.input = Some(String::new());
    }

    /// The prompt's text, when it is open.
    pub fn prompt(&self) -> Option<&str> {
        self.input.as_deref()
    }

    /// Type into the open prompt.
    pub fn type_char(&mut self, c: char) {
        if let Some(input) = self.input.as_mut() {
            input.push(c);
        }
    }

    pub fn backspace(&mut self) {
        if let Some(input) = self.input.as_mut() {
            input.pop();
        }
    }

    /// Close the prompt, and hand back what it held.
    pub fn close_prompt(&mut self) -> Option<String> {
        self.input.take()
    }

    /// What the header shows, so a test can draw a populated screen without
    /// a machine to read it from.
    pub fn show_facts(&mut self, facts: Facts) {
        self.facts = facts;
    }

    /// The model list, the default first.
    pub fn set_models(&mut self, models: Vec<ModelRow>) {
        self.models = models;
    }

    /// The engine-log pane's rows.
    pub fn set_log(&mut self, log: Vec<String>) {
        self.log = log;
    }

    /// Read everything the screen shows.
    ///
    /// One pass over the world per call, never per frame: two sockets (the
    /// engine and the console), an nvidia-smi, a PowerShell round trip for
    /// the free memory, `golem.toml`, the preset file, the results file and
    /// the engine's log.
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
        self.models = model_rows(home);
        self.log = log_lines(home);
    }
}

/// The presets, the default first, each with its best measured decode.
fn model_rows(home: &Path) -> Vec<ModelRow> {
    let rows = results::read(&paths::results(home)).map(|(rows, _)| rows).unwrap_or_default();
    let mut models = crate::models::list(home);
    // Stable: the default moves to the top, the rest keep the file's order.
    models.sort_by_key(|m| !m.default);
    models
        .into_iter()
        .map(|m| {
            let best = rows
                .iter()
                .filter(|r| r.kind == Kind::Bench && r.model == m.name)
                .filter_map(|r| r.decode_median.as_ref())
                .max_by(|a, b| a.value.total_cmp(&b.value))
                .map(|n| n.text.clone());
            ModelRow { name: m.name, role: m.role, default: m.default, present: m.present, best }
        })
        .collect()
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

/// The whole screen as lines: the header, the actions, whatever panes fit,
/// and the bottom line.
///
/// The vertical budget is spent in the order things matter. The actions
/// and the bottom line come first - the actions are why the screen exists,
/// and the bottom line is where a result or the pull prompt appears - then
/// the header, then the model list, then the log. Drawing top-down and
/// letting the terminal clip would leave a small window showing a header
/// and no actions.
fn screen(app: &App, width: u16, height: u16) -> Vec<Line<'static>> {
    let w = width as usize;
    let h = height as usize;
    let mut out = Vec::with_capacity(h);

    let header_rows = if h >= 9 { 3 } else { 0 };
    let bottom_rows = if h >= 4 { 2 } else { 0 };

    if header_rows > 0 {
        out.extend(header(app, w));
        out.push(rule(w));
    }

    // The grid, flowed to the width: four to a row when there is room,
    // fewer on a narrow window, and at least one row.
    let per = ((w.saturating_sub(2)) / CELL).clamp(1, COLS);
    let cells: Vec<Line<'static>> = grid_lines(app, per);
    let room = h.saturating_sub(header_rows + bottom_rows).max(1);
    // On a window too short for every grid row, the selection's row is kept.
    let take = room.min(cells.len());
    let first = (app.selected / per).saturating_sub(take - 1).min(cells.len() - take);
    let shown: Vec<Line<'static>> = cells.into_iter().skip(first).take(take).collect();
    let grid_rows = shown.len();
    out.extend(shown);

    // What is left is the panes'. The model list takes what it needs, up to
    // its cap, then the log; each costs a rule or heading line too.
    let mut avail = h.saturating_sub(header_rows + bottom_rows + grid_rows);
    let model_rows = if avail >= 2 { (avail - 1).min(MODELS_MAX).min(app.models.len().max(1)) } else { 0 };
    if model_rows > 0 {
        avail -= model_rows + 1;
    }
    let log_rows = if avail >= 3 { (avail - 1).min(LOG_MAX) } else { 0 };

    if model_rows > 0 {
        out.push(rule(w));
        out.extend(models(app, model_rows));
    }
    if log_rows > 0 {
        out.push(section("engine log"));
        out.extend(tail(&app.log, log_rows));
    }
    // The bottom line sits at the bottom, whatever the panes left over.
    while out.len() + bottom_rows < h {
        out.push(Line::default());
    }

    if bottom_rows > 0 {
        out.push(rule(w));
        out.push(match &app.input {
            Some(text) => Line::from(vec![
                Span::styled("  pull ", Style::default().fg(PATINA).add_modifier(Modifier::BOLD)),
                Span::raw(text.clone()),
                Span::styled("\u{2588}", Style::default().fg(PATINA)),
                Span::styled("   owner/repo[:quant] or a library name, enter to pull, esc to cancel", dim()),
            ]),
            None => Line::from(Span::styled(format!("  {}", app.message), dim())),
        });
    }
    out
}

/// The action grid, `per` cells to a line, the selected one marked.
fn grid_lines(app: &App, per: usize) -> Vec<Line<'static>> {
    let all: Vec<(usize, &str)> = GRID.iter().flatten().enumerate().map(|(i, (label, _))| (i, *label)).collect();
    all.chunks(per)
        .map(|chunk| {
            let mut spans = vec![Span::raw("  ")];
            for (i, label) in chunk {
                if *i == app.selected {
                    spans.push(Span::styled(
                        format!("{POINTER} {label:<width$}", width = CELL - 2),
                        Style::default().fg(PATINA).add_modifier(Modifier::BOLD),
                    ));
                } else {
                    spans.push(Span::raw(format!("  {label:<width$}", width = CELL - 2)));
                }
            }
            Line::from(spans)
        })
        .collect()
}

/// The model list: the default's dot and speed in the serving colour, the
/// name in a fixed column so the numbers line up (a longer name is cut),
/// then the role and what is missing.
fn models(app: &App, rows: usize) -> Vec<Line<'static>> {
    if app.models.is_empty() {
        return vec![Line::from(Span::styled("  no models yet - p pulls one", dim()))];
    }
    app.models
        .iter()
        .take(rows)
        .map(|m| {
            let name: String = m.name.chars().take(26).collect();
            let speed = m.best.as_ref().map_or_else(|| "-".to_string(), |b| format!("{b} t/s"));
            let (dot, speed_style) = if m.default {
                (Span::styled("\u{25cf} ", Style::default().fg(SERVING)), Style::default().fg(SERVING))
            } else {
                (Span::raw("  "), Style::default())
            };
            let mut spans = vec![
                Span::raw("  "),
                dot,
                Span::styled(format!("{name:<26}"), Style::default().add_modifier(Modifier::BOLD)),
                Span::styled(format!("{speed:>11}"), speed_style),
                Span::styled(format!("   {:<9}", m.role), dim()),
            ];
            if !m.present {
                spans.push(Span::styled("not downloaded", dim()));
            }
            Line::from(spans)
        })
        .collect()
}

/// The last `rows` of `lines`, blank-padded to exactly `rows` so the screen
/// below them does not jump about as entries arrive.
fn tail(lines: &[String], rows: usize) -> Vec<Line<'static>> {
    let start = lines.len().saturating_sub(rows);
    let mut out: Vec<Line<'static>> = lines[start..]
        .iter()
        .map(|line| Line::from(Span::styled(format!("  {line}"), dim())))
        .collect();
    while out.len() < rows {
        out.push(Line::default());
    }
    out
}

/// A full-width thin rule.
fn rule(width: usize) -> Line<'static> {
    Line::from(Span::styled("\u{2500}".repeat(width), dim()))
}

/// The selection's colour: the console's patina accent.
const PATINA: Color = Color::Rgb(0x4d, 0xbf, 0xab);
/// Serving, as in the console and the tray.
const SERVING: Color = Color::Rgb(0x55, 0xc2, 0x7a);

/// A pane's heading.
fn section(title: &str) -> Line<'static> {
    Line::from(Span::styled(format!("  {title}"), dim()))
}

fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

/// The mark's colour in the terminal: the spark's amber, the colour the mark
/// has wherever it shows no state.
fn amber() -> Style {
    let [r, g, b] = Spark::Lit.rgb();
    Style::default().fg(Color::Rgb(r, g, b))
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
        (mark::GLYPHS[0].to_string(), amber()),
        (" GOLEM".to_string(), Style::default().add_modifier(Modifier::BOLD)),
    ];
    first.push(if facts.up {
        ("   \u{25cf} serving".to_string(), Style::default().fg(SERVING))
    } else {
        ("   \u{25cb} stopped".to_string(), dim())
    });
    if !facts.alias.is_empty() {
        first.push((format!("   {}", facts.alias), Style::default()));
    }
    let gb = |mib: u64| mib as f64 / 1024.0;
    match (facts.vram_mib, facts.vram_total_mib) {
        (Some(used), Some(total)) => first.push((format!("   {:.1}/{:.1} GB", gb(used), gb(total)), dim())),
        (Some(used), None) => first.push((format!("   {:.1} GB", gb(used)), dim())),
        _ => {}
    }
    first.push((format!("   [{}]", facts.engine), dim()));

    let mut second: Vec<(String, Style)> = vec![(mark::GLYPHS[1].to_string(), amber())];
    second.push(if facts.web_up {
        (format!(" console :{}", facts.console_port), dim())
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

/// The one action allowed to run, and how to start it.
struct Job<'a, F> {
    act: &'a Arc<F>,
    tx: &'a std::sync::mpsc::Sender<String>,
    running: &'a mut Option<&'static str>,
}

impl<F: Fn(Action, String) -> String + Send + Sync + 'static> Job<'_, F> {
    /// Start `action` on its own thread, or say what is still running.
    fn start(&mut self, app: &mut App, action: Action, label: &'static str, arg: String) {
        if let Some(busy) = *self.running {
            app.set_message(format!("{busy}: still running (one action at a time)"));
            return;
        }
        let (tx, act) = (self.tx.clone(), Arc::clone(self.act));
        *self.running = Some(label);
        app.set_message(format!("{label}: running..."));
        std::thread::spawn(move || {
            let _ = tx.send(act(action, arg));
        });
    }
}

/// The event loop, on the real terminal.
///
/// `act` is the one thing this does not decide for itself: it is handed each
/// chosen action, with the pull prompt's text for a pull, and returns the
/// line to show.
///
/// **An action runs on a thread of its own.** `up` installs an engine and
/// fetches a model before it answers, and a screen that stops drawing for the
/// minutes that takes cannot be told from one that has crashed. The loop
/// keeps drawing, the bottom line says what is running, and the result lands
/// when it lands. One at a time, as everywhere else: two of these would
/// fight over one engine, one pid file and one card.
pub fn run(
    home: &Path,
    act: impl Fn(Action, String) -> String + Send + Sync + 'static,
) -> io::Result<()> {
    let mut app = App::new();

    let _terminal = RawTerminal::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;

    // The actions are up before the machine is read. Reading it costs about
    // a second - the PowerShell call for free memory alone is ~0.4s - and a
    // blank window for that long reads as a screen that is not starting. The
    // header fills in on the next frame.
    terminal.draw(|frame| draw(&app, frame))?;
    app.refresh(home, true); // the state at start-up

    let act = Arc::new(act);
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut running: Option<&'static str> = None;
    let mut job = Job { act: &act, tx: &tx, running: &mut running };

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
                // One notch of the wheel moves one action, like an arrow key.
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
            // Ctrl+C is a key event, not a signal, once raw mode is on, so it
            // is caught here or not at all.
            if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
                break;
            }

            // The pull prompt takes every key while it is open.
            if app.prompt().is_some() {
                match key.code {
                    KeyCode::Esc => {
                        app.close_prompt();
                        app.set_message(HINT);
                    }
                    KeyCode::Enter => {
                        let text = app.close_prompt().unwrap_or_default().trim().to_string();
                        if text.is_empty() {
                            app.set_message(HINT);
                        } else {
                            job.start(&mut app, Action::Pull, "pull", text);
                        }
                    }
                    KeyCode::Backspace => app.backspace(),
                    KeyCode::Char(c) => app.type_char(c),
                    _ => {}
                }
                continue;
            }

            match key.code {
                KeyCode::Left | KeyCode::Char('h') => app.previous(),
                KeyCode::Right | KeyCode::Char('l') => app.next(),
                KeyCode::Up | KeyCode::Char('k') => app.up(),
                KeyCode::Down | KeyCode::Char('j') => app.down(),
                // The facts are read at start-up and after an action; a machine
                // that changed under the screen - a game started, a download
                // ended - is what this is for. `r` re-reads the expensive ones
                // too, which the automatic refresh leaves alone for a while.
                KeyCode::Char('r') => app.refresh(home, true),
                KeyCode::Char('p') => app.open_prompt(),
                KeyCode::Char('w') => job.start(&mut app, Action::Console, "Console", String::new()),
                KeyCode::Enter => {
                    let (action, label) = (app.selected(), app.selected_label());
                    job.start(&mut app, action, label, String::new());
                }
                KeyCode::Char('q') | KeyCode::Esc => break,
                _ => {}
            }
        }

        // The action's answer, whenever it arrives.
        if let Ok(message) = rx.try_recv() {
            *job.running = None;
            app.set_message(message);
            app.refresh(home, false); // the action changed things
        }
    }
    Ok(())
}
