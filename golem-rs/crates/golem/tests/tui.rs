//! Tests for the full-screen screen (`src/tui.rs`).
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

use golem::tui::{self, Action, App, Facts, ModelRow};

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
    screen_at(app, 96, 30)
}

/// The one row carrying the selection marker. Only the selected action is
/// marked, and nothing else on screen contains the marker.
fn marked_row(text: &str) -> &str {
    text.lines()
        .find(|line| line.contains("\u{25b8}"))
        .expect("a row with the selection marker")
}

fn model(name: &str, role: &str, default: bool, present: bool, best: Option<&str>) -> ModelRow {
    ModelRow { name: name.into(), role: role.into(), default, present, best: best.map(str::to_string) }
}

/// An app with a machine's worth of facts and models, without a machine.
fn populated() -> App {
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
    app.set_models(vec![
        model("ornith-1.5-35b", "coder", true, true, Some("63.8")),
        model("qwen3.5-9b", "everyday", false, true, Some("52.3")),
        model("qwen2.5-0.5b-instruct", "smoke", false, false, None),
    ]);
    app
}

// ---- the actions -------------------------------------------------------

#[test]
fn every_action_is_drawn_in_the_grid_order() {
    assert_eq!(
        App::labels(),
        ["Start", "Stop", "Restart", "Console", "Bench", "Eval", "Tune", "Report"]
    );
    let text = screen(&App::new());
    for label in App::labels() {
        assert!(text.contains(label), "{label:?} is not on screen:\n{text}");
    }
    // Four to a row on a wide window: the engine's row, then the measurements'.
    let first = text.lines().find(|l| l.contains("Start")).expect("the first row");
    assert!(first.contains("Console") && !first.contains("Bench"), "{first:?}");
}

#[test]
fn the_arrows_move_through_the_grid_and_stop_at_its_edges() {
    let mut app = App::new();
    assert_eq!(app.selected(), Action::Start);
    app.next();
    assert_eq!(app.selected(), Action::Stop);
    app.down();
    assert_eq!(app.selected(), Action::Eval, "down keeps the column");
    app.up();
    assert_eq!(app.selected(), Action::Stop);
    app.up(); // the top row is the top
    assert_eq!(app.selected(), Action::Stop);
    app.previous();
    app.previous(); // past the first: stays on it
    assert_eq!(app.selected(), Action::Start);
    for _ in 0..12 {
        app.next();
    }
    assert_eq!(app.selected(), Action::Report, "the last action is the end of it");
    app.down();
    assert_eq!(app.selected(), Action::Report);
}

#[test]
fn the_marker_is_on_the_selected_action() {
    let mut app = App::new();
    assert!(marked_row(&screen(&app)).contains("\u{25b8} Start"));
    app.down();
    assert!(marked_row(&screen(&app)).contains("\u{25b8} Bench"));
}

// ---- the bottom line and the pull prompt ---------------------------------

#[test]
fn the_bottom_line_starts_with_the_keys() {
    let text = screen(&App::new());
    let last = text.lines().last().expect("a last line");
    for key in ["enter run", "p pull", "w console", "q quit"] {
        assert!(last.contains(key), "{key:?} is not in the hint: {last:?}");
    }
}

#[test]
fn the_bottom_line_shows_what_set_message_was_given() {
    let mut app = App::new();
    app.set_message("restart: done");
    let last = screen(&app).lines().last().expect("a last line").to_string();
    assert!(last.contains("restart: done"), "{last:?}");
}

#[test]
fn the_pull_prompt_shows_what_is_typed_and_hands_it_back() {
    let mut app = App::new();
    app.open_prompt();
    for c in "unsloth/Qwen3.5-9B-GGUF:Q6".chars() {
        app.type_char(c);
    }
    app.type_char('X');
    app.backspace();
    let last = screen_at(&app, 140, 30).lines().last().expect("a last line").to_string();
    assert!(last.contains("pull unsloth/Qwen3.5-9B-GGUF:Q6"), "{last:?}");
    assert_eq!(app.close_prompt().as_deref(), Some("unsloth/Qwen3.5-9B-GGUF:Q6"));
    assert_eq!(app.prompt(), None, "closed");
}

// ---- the header ----------------------------------------------------------

#[test]
fn a_fresh_screen_says_the_engine_is_stopped() {
    let text = screen(&App::new());
    assert!(text.contains("GOLEM"), "the header title is missing:\n{text}");
    assert!(text.contains("stopped"), "{text}");
}

#[test]
fn the_header_carries_every_fact() {
    // Two lines beside the two-line mark: the state, the alias, the VRAM and
    // the engine on the first; the console and the free memory on the
    // second.
    let text = screen_at(&populated(), 120, 30);
    let mut lines = text.lines();
    let first = lines.next().expect("the header's first line");
    let second = lines.next().expect("the header's second line");
    assert!(first.contains("\u{259f}\u{2588}\u{2599} GOLEM"), "{first:?}");
    assert!(first.contains("\u{25cf} serving"), "{first:?}");
    assert!(first.contains("ornith-1.5-35b"), "{first:?}");
    assert!(first.contains("10.7/12.0 GB"), "{first:?}");
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
    let narrow = screen_at(&populated(), 40, 30);
    let header = narrow.lines().next().expect("the header");
    assert!(header.contains("GOLEM"), "the mark survives:\n{header:?}");
    assert!(header.contains("\u{25cf} serving"), "so does the state:\n{header:?}");
    assert!(!header.contains("[llama.cpp]"), "the last segment goes first:\n{header:?}");
    assert!(header.trim_end().chars().count() <= 40, "and it fits: {header:?}");
}

#[test]
fn the_rules_span_the_window() {
    let text = screen_at(&App::new(), 60, 30);
    let rule = "\u{2500}".repeat(60);
    assert_eq!(text.lines().nth(2), Some(rule.as_str()), "a rule closes the header");
    assert!(text.lines().filter(|l| *l == rule).count() >= 2, "and rules separate the parts");
}

// ---- the panes -----------------------------------------------------------

#[test]
fn the_models_list_marks_the_default_and_says_what_is_missing() {
    let mut app = populated();
    app.set_log(vec!["srv load: model loaded".to_string()]);
    let text = screen(&app);
    let ornith = text.lines().find(|l| l.contains("ornith-1.5-35b") && l.contains("t/s")).expect("the default's row");
    assert!(ornith.contains("\u{25cf} ornith-1.5-35b"), "the default is marked: {ornith:?}");
    assert!(ornith.contains("63.8 t/s") && ornith.contains("coder"), "{ornith:?}");
    let tiny = text.lines().find(|l| l.contains("qwen2.5-0.5b-instruct")).expect("a missing model's row");
    assert!(tiny.contains("not downloaded") && tiny.contains(" - "), "{tiny:?}");
    assert!(text.contains("engine log") && text.contains("srv load: model loaded"), "{text}");
}

#[test]
fn an_empty_list_says_how_to_add_a_model() {
    let text = screen(&App::new());
    assert!(text.contains("no models yet - p pulls one"), "{text}");
}

#[test]
fn the_models_list_is_cut_at_its_height() {
    let mut app = App::new();
    app.set_models((1..=12).map(|i| model(&format!("model-{i:02}"), "", false, true, None)).collect());
    let text = screen(&app);
    assert!(text.contains("model-01"), "the list starts at the top:\n{text}");
    assert!(!text.contains("model-12"), "and is cut at its cap:\n{text}");
}

// ---- small windows -------------------------------------------------------

#[test]
fn a_small_window_keeps_the_actions_and_drops_the_rest() {
    // Tiny windows: conhost opened at a corner, or a terminal split down to
    // nothing. The actions are the one thing the screen is for, so they are
    // what a small window keeps - the header gives way, and so do the panes.
    for (w, h) in [(30u16, 8u16), (14, 5), (40, 3)] {
        let text = screen_at(&populated(), w, h);
        assert_eq!(text.lines().count(), h as usize, "{w}x{h} should fill exactly {h} rows");
        assert!(text.contains("\u{25b8}"), "the selection is still drawn at {w}x{h}:\n{text}");
        assert!(!text.contains("GOLEM"), "the header gives way at {w}x{h}:\n{text}");
        assert!(!text.contains("engine log"), "and so do the panes at {w}x{h}:\n{text}");
    }
    // A window with no room at all is not a panic.
    let text = screen_at(&App::new(), 1, 1);
    assert_eq!(text.lines().count(), 1, "{text:?}");
}

#[test]
fn a_narrow_window_flows_the_grid_onto_more_rows_and_keeps_the_selection() {
    let mut app = App::new();
    for _ in 0..6 {
        app.next(); // Tune
    }
    let text = screen_at(&app, 26, 30);
    assert!(marked_row(&text).contains("\u{25b8} Tune"), "{text}");
}
