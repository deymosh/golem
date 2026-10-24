//! Tests for the full-screen console (`src/tui.rs`).
//!
//! Hermetic by construction: everything here goes through `App::new`, the
//! accessors and `tui::draw`, none of which touches the world. The engine,
//! the sockets, nvidia-smi and the results file are read only by
//! `App::refresh` and `run`, which these tests never call - so no test
//! depends on what this machine has running.
//!
//! The screen is rendered into ratatui's `TestBackend` - a buffer, not a
//! terminal - and read back cell by cell, which is the only way to assert on
//! what a user would see without a user. `run` itself is left alone: it needs
//! a real terminal in raw mode.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use golem::tui::{self, Action, App, Facts};

/// Render one frame into a `w`x`h` buffer and return it as text, one line per
/// row, so the assertions read like the screen looks.
fn screen_at(app: &App, w: u16, h: u16) -> String {
    let backend = TestBackend::new(w, h);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal
        .draw(|frame| tui::draw(app, frame))
        .expect("draw into the test buffer");
    let buffer = terminal.backend().buffer();
    let width = buffer.area.width as usize;
    buffer
        .content
        .chunks(width)
        .map(|row| row.iter().map(|cell| cell.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn screen(app: &App) -> String {
    screen_at(app, 80, 24)
}

/// The one row carrying the selection marker. Only the selected entry is
/// highlighted, and nothing else on screen contains the marker.
fn marked_row(text: &str) -> &str {
    text.lines()
        .find(|line| line.contains("\u{25b8}"))
        .expect("a row with the selection marker")
}

/// An app with a machine's worth of facts, without a machine.
fn app_with_facts() -> App {
    let mut app = App::new();
    app.show_facts(Facts {
        up: true,
        vram_mib: Some(11000),
        vram_total_mib: Some(12288),
        ram_free_gb: Some(8.4),
        alias: "ornith-1.5-35b".to_string(),
        engine: "llama.cpp".to_string(),
        web_up: true,
        console_port: 8787,
    });
    app
}

// ---- the menu ----------------------------------------------------------

#[test]
fn every_menu_entry_is_drawn() {
    let app = App::new();
    let text = screen(&app);
    for &(label, _) in app.items() {
        assert!(text.contains(label), "menu entry {label:?} is not on screen:\n{text}");
    }
}

#[test]
fn the_menu_is_in_the_documented_order() {
    let labels: Vec<&str> = App::new().items().iter().map(|&(label, _)| label).collect();
    assert_eq!(
        labels,
        ["Status", "Start", "Stop", "Restart", "Bench", "Eval", "Tune", "Report", "Console", "Quit"]
    );
}

// ---- selection ---------------------------------------------------------

#[test]
fn selected_returns_the_action_the_marker_is_on() {
    let mut app = App::new();
    assert_eq!(app.selected(), Action::Status);
    app.next();
    assert_eq!(app.selected(), Action::Start);
    app.next();
    assert_eq!(app.selected(), Action::Stop);
    app.previous();
    assert_eq!(app.selected(), Action::Start);
}

#[test]
fn the_marker_stops_at_the_last_entry() {
    // A list, not a wheel: the ends are worth being able to feel for, and a
    // marker that jumps from Quit back to Status reads as a glitch.
    let mut app = App::new();
    for _ in 0..app.items().len() + 3 {
        app.next();
    }
    assert_eq!(app.selected(), Action::Quit, "the last entry is the end of it");
}

#[test]
fn the_marker_stops_at_the_first_entry() {
    let mut app = App::new();
    for _ in 0..3 {
        app.previous();
    }
    assert_eq!(app.selected(), Action::Status, "the first entry is the start of it");
}

#[test]
fn the_selection_marker_moves_and_stops_at_the_ends() {
    let mut app = App::new();
    assert!(marked_row(&screen(&app)).contains("Status"));

    app.next();
    assert!(marked_row(&screen(&app)).contains("Start"));

    app.previous();
    assert!(marked_row(&screen(&app)).contains("Status"));

    app.previous(); // past the first: stays on it
    assert!(marked_row(&screen(&app)).contains("Status"));

    for _ in 0..app.items().len() {
        app.next();
    }
    assert!(marked_row(&screen(&app)).contains("Quit"), "the last entry is the end");

    app.next(); // past the last: stays on it
    assert!(marked_row(&screen(&app)).contains("Quit"));
}

// ---- the message line --------------------------------------------------

#[test]
fn the_message_line_shows_what_set_message_was_given() {
    let mut app = App::new();
    app.set_message("engine restarted");
    let text = screen(&app);
    assert!(text.contains("engine restarted"), "the message is not on screen:\n{text}");
}

#[test]
fn the_message_line_starts_with_a_hint() {
    let text = screen(&App::new());
    assert!(text.contains("enter run"), "the default hint should be drawn:\n{text}");
    // The wheel moves the marker too, so the hint says so.
    assert!(text.contains("mouse wheel"), "the hint names the wheel:\n{text}");
}

#[test]
fn a_window_too_small_for_the_panes_still_shows_the_menu() {
    // Tiny windows: conhost opened at a corner, or a terminal split down to
    // nothing. The menu is the one thing the screen is
    // for, so it is what a small window keeps - the header gives way, and so
    // do the panes.
    for (w, h) in [(20u16, 6u16), (12, 5), (40, 3)] {
        let text = screen_at(&App::new(), w, h);
        assert_eq!(text.lines().count(), h as usize, "{w}x{h} should fill exactly {h} rows");
        assert!(
            text.contains("\u{25b8}"),
            "the selection marker is still drawn at {w}x{h}:\n{text}"
        );
        assert!(!text.contains("GOLEM"), "the header gives way at {w}x{h}:\n{text}");
    }
    // A window with no room at all is not a panic. At one column there is
    // nothing to say - the marker is the third character of its row - so the
    // assertion is only that it draws one row and stops.
    let text = screen_at(&App::new(), 1, 1);
    assert_eq!(text.lines().count(), 1, "{text:?}");
}

// ---- the header --------------------------------------------------------

#[test]
fn the_header_names_the_engine_state() {
    let text = screen(&App::new());
    assert!(text.contains("GOLEM"), "the header title is missing:\n{text}");
    assert!(text.contains("stopped"), "a fresh app reports the engine stopped:\n{text}");
}

#[test]
fn the_header_carries_every_fact() {
    // Two lines beside the two-line mark: the state, the alias, the VRAM and
    // the engine on the first; the console and the free memory on the
    // second.
    let text = screen_at(&app_with_facts(), 120, 24);
    let mut lines = text.lines();
    let first = lines.next().expect("the header's first line");
    let second = lines.next().expect("the header's second line");
    assert!(first.contains("\u{259f}\u{2588}\u{2599} GOLEM"), "{first:?}");
    assert!(first.contains("\u{25cf} serving"), "{first:?}");
    assert!(first.contains("ornith-1.5-35b"), "{first:?}");
    assert!(first.contains("10.7/12.0 GB VRAM"), "{first:?}");
    assert!(first.contains("[llama.cpp]"), "{first:?}");
    assert!(second.contains("\u{259c}\u{2588}\u{259b}"), "{second:?}");
    assert!(second.contains("console :8787"), "{second:?}");
    assert!(second.contains("8.4 GB free"), "{second:?}");
}

#[test]
fn the_header_mark_is_the_icons_grid() {
    let text = screen(&App::new());
    let mut lines = text.lines();
    for glyphs in golem_core::mark::GLYPHS {
        let line = lines.next().expect("a header line");
        assert!(line.starts_with(&format!("  {glyphs}")), "{line:?}");
    }
}

#[test]
fn a_narrow_header_drops_whole_segments_from_the_right() {
    // A segment is either whole on the line or not there - and the ones that
    // matter most are on the left.
    let app = app_with_facts();
    let narrow = screen_at(&app, 40, 24);
    let header = narrow.lines().next().expect("the header");
    assert!(header.contains("GOLEM"), "the mark survives:\n{header:?}");
    assert!(header.contains("\u{25cf} serving"), "so does the state:\n{header:?}");
    assert!(!header.contains("[llama.cpp]"), "the last segment goes first:\n{header:?}");
    assert!(header.trim_end().chars().count() <= 40, "and it fits: {header:?}");
}

#[test]
fn the_rules_span_the_window() {
    let text = screen_at(&App::new(), 60, 24);
    let rule = "\u{2500}".repeat(60);
    assert_eq!(text.lines().nth(2), Some(rule.as_str()), "a rule closes the header");
    assert!(text.lines().filter(|l| *l == rule).count() >= 2, "and rules separate the parts");
}

// ---- the panes ---------------------------------------------------------

#[test]
fn the_runs_and_log_panes_show_what_they_were_given() {
    let mut app = App::new();
    app.set_runs(vec!["  59.3 t/s  ornith-1.5-35b".to_string()]);
    app.set_log(vec!["  srv load: model loaded".to_string()]);
    let text = screen(&app);

    assert!(text.contains("recent runs"), "{text}");
    assert!(text.contains("59.3 t/s"), "{text}");
    assert!(text.contains("engine log"), "{text}");
    assert!(text.contains("srv load: model loaded"), "{text}");
}

#[test]
fn the_panes_show_the_newest_rows_and_fill_their_height() {
    // The screen below the pane must not jump about as entries arrive, so a
    // short pane is padded rather than left ragged.
    let mut app = App::new();
    app.set_runs((1..=12).map(|i| format!("run {i}")).collect());
    let text = screen(&app);
    assert!(text.contains("run 12"), "the newest row is shown:\n{text}");
    assert!(!text.contains("run 1\n"), "the oldest is pushed out:\n{text}");
}

#[test]
fn a_short_window_drops_the_panes_before_the_menu() {
    // The menu is why the screen exists; the panes are the first thing to go.
    // 17 rows is the tallest window with no room for a pane and still enough
    // for the whole ten-entry menu.
    let app = app_with_facts();
    let text = screen_at(&app, 80, 17);
    assert!(!text.contains("recent runs"), "no room for panes:\n{text}");
    assert!(!text.contains("\u{2026}"), "and the menu still fits whole:\n{text}");
    for &(label, _) in app.items() {
        assert!(text.contains(label), "{label} is still there:\n{text}");
    }
}

#[test]
fn a_window_too_short_for_the_menu_still_shows_the_selection() {
    let mut app = App::new();
    app.next(); // Start
    let text = screen_at(&app, 60, 12);
    assert!(marked_row(&text).contains("Start"), "the marker stays on screen:\n{text}");
    assert!(text.contains("\u{2026}"), "and says there is more:\n{text}");
}
