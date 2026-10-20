//! The testable core of the portable golem binary.
//!
//! One module per thing the shell stack currently does with awk and printf,
//! each with unit tests before the binary is allowed to replace its script
//! (the migration gate in docs/plan-portable-binary.md).

/// A small dependency-free JSON reader (the rows are our own known shape).
pub mod json;

/// Append-only reader/writer for `bench/results.jsonl`.
pub mod results;

/// INI engine: model presets, engine adapters, and the section-override
/// merge behind `write_candidate` / `write_overlay` in scripts/tune.sh.
///
/// **Not written yet.** Only the `Entry` shape is here; the reader and the
/// merge are phase-1 work that nothing needs until phase 4, which is why
/// they were skipped. `golem`'s own `state.rs` reads the preset file with a
/// small scanner in the meantime, and the shell's awk still owns the merge.
pub mod ini;

/// GGUF header reader, golden-tested against scripts/gguf-info.sh.
pub mod gguf;

/// Parsers for the RESULT lines of bench and eval output.
pub mod evalparse;

/// The Markdown results table (`report --md`).
///
/// It started as a byte-for-byte port of `scripts/report.sh` and has since
/// moved past it - the shell's version still prints the old flat table - so
/// `scripts/report.sh --md` must not be run against `bench/RESULTS.md`.
pub mod report;

pub mod tune;

/// The schema version this build understands (bumped on breaking changes
/// to a row's fields; old rows keep parsing).
pub const SCHEMA_VERSION: u32 = 1;

#[cfg(test)]
mod tests {
    #[test]
    fn schema_version_is_sane() {
        assert!(super::SCHEMA_VERSION >= 1);
    }
}
