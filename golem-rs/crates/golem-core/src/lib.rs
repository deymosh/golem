//! The testable core of the golem binary: everything that decides rather
//! than acts - parsing, merging, ranking, predicting - with no I/O beyond
//! reading a file it is handed, and unit tests for each.

/// A small dependency-free JSON reader, plus the string escaper the console
/// writes its payloads with.
pub mod json;

/// Append-only reader/writer for `bench/results.jsonl`.
pub mod results;

/// INI engine: model presets, engine adapters, and the section-override
/// merge behind the tuner's candidates and overlays.
pub mod ini;

/// GGUF header reader: the layer count, the trained context and the parameter
/// count the recorder and the tuner size their work from.
pub mod gguf;

/// Parsers for the RESULT lines of bench and eval output.
pub mod evalparse;

/// The Markdown results table (`report --md`), the only writer of
/// `bench/RESULTS.md`.
pub mod report;

pub mod tune;

/// The router's `/models` listing: which models failed to load.
pub mod router;

/// The engine adapter's contract: a preset's flags become the arguments the
/// engine is started with.
pub mod adapter;

/// The regex subset the adapters' `<platform>_asset_*` keys use, as a search
/// rather than a full match. The binary has no regex crate and will
/// not gain one, so this is the whole of what those patterns need - and a
/// construct outside it is an error, not a silent literal.
pub mod pattern;

/// `golem.toml`: the TOML subset the binary's own configuration file uses,
/// read and written line by line so the comments survive.
pub mod config;

/// The row builder: a captured bench or eval output, plus what was measured
/// on, becomes one line of
/// `bench/results.jsonl`.
pub mod record;

/// The date stamps a preset comment and a tuned overlay carry.
pub mod time;

/// Hugging Face as `golem pull owner/repo[:quant]` reads it: the reference,
/// the Hub's answer, the quant for this machine, and the preset section.
pub mod hub;

/// The library: the curated models shipped with golem, their roles and the
/// vendor scores `tune --auto` ranks by.
pub mod library;

/// The `PROGRESS` lines a long step writes into a job's log, and the bar the
/// console and the screen draw from the newest one.
pub mod progress;

/// The job log's grammar: the header and footer that mark one job, and the
/// clock stamp each line carries.
pub mod joblog;

/// The mark: one grid for the terminal glyphs, the icon's pixels and its
/// `.ico` file.
pub mod mark;

/// The graphics card: its vendor from a PCI ID, the backend that vendor
/// serves, and the reading the Windows GPU probe left.
pub mod gpu;

/// SHA-256, streamed: the check `golem update` runs a downloaded release
/// against.
pub mod sha256;

/// Golem's own updates, decided: release tags and assets, the releases
/// answer, and the state file that remembers what was checked and skipped.
pub mod update;

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
