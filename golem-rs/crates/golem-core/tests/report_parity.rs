//! The Markdown report, frozen against a committed fixture.
//!
//! The report groups hardware first - one section per machine, one block per
//! model, one line per context at the best setup measured there - and the
//! fixture is a snapshot of that intended output. It is the guard: any
//! change to the format fails here until the fixture is regenerated
//! deliberately, with:
//!
//! ```text
//! ./golem report --md --results golem-rs/crates/golem-core/tests/fixtures/results.jsonl
//! ```
//!
//! It needs no model files, so it runs anywhere, including CI.
//!
//! The fixture covers what a report gets wrong: two rows for one configuration
//! (the best decode wins), two setups that differ only in the offload split at
//! one context (the best wins and the split is named in the setup column), the
//! same model at a second context (its own line), the same model, engine,
//! build, context and flags on a second machine (a section of its own, never
//! merged), a row with no host and no parameter counts (the unknown host, last
//! and under "unknown host"), a missing prefill (an empty cell, not a dash),
//! the mixture-of-experts marker, threads present and absent, a needle verdict
//! from an eval row, and a rejected row in each machine's own section.

use std::path::Path;

#[test]
fn the_report_matches_the_frozen_fixture() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");

    let (rows, unparsed) = golem_core::results::read(&dir.join("results.jsonl"))
        .expect("the fixture should be readable");
    assert!(unparsed.is_empty(), "every fixture row should parse: {unparsed:?}");

    let expected = std::fs::read_to_string(dir.join("RESULTS.md"))
        .expect("the expected output should be readable");

    assert_eq!(
        golem_core::report::markdown(&rows),
        expected,
        "the report no longer matches the frozen fixture at tests/fixtures/RESULTS.md"
    );
}

#[test]
fn the_page_inlining_is_the_files_own_lines() {
    // The page embeds the raw lines rather than a re-serialization, so it
    // can never disagree with the data it is showing.
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let raw = std::fs::read_to_string(dir.join("results.jsonl")).expect("fixture");
    let (rows, _) = golem_core::results::read(&dir.join("results.jsonl")).expect("read");
    let lines: Vec<&str> = raw.lines().filter(|l| l.trim_start().starts_with('{')).collect();
    assert_eq!(lines.len(), rows.len(), "one record per row");
}
