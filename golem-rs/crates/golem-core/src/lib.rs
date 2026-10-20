//! The testable core of the portable golem binary.
//!
//! One module per thing the shell stack currently does with awk and printf,
//! each with unit tests before the binary is allowed to replace its script
//! (the migration gate in docs/plan-portable-binary.md).

/// A small dependency-free JSON reader, plus the string escaper the console
/// writes its payloads with.
pub mod json;

/// Append-only reader/writer for `bench/results.jsonl`.
pub mod results;

/// INI engine: model presets, engine adapters, and the section-override
/// merge behind `write_candidate` / `write_overlay` in scripts/tune.sh.
pub mod ini;

/// GGUF header reader, golden-tested against scripts/gguf-info.sh.
pub mod gguf;

/// Parsers for the RESULT lines of bench and eval output.
pub mod evalparse;

/// The Markdown results table (`report --md`).
///
/// It started as a byte-for-byte port of `scripts/report.sh` and has since
/// moved past it - the shell's version printed the old flat table - so the
/// script was deleted on 2026-10-20 and this is the only writer of
/// `bench/RESULTS.md`.
pub mod report;

pub mod tune;

/// The engine adapter's contract: a preset's flags become the arguments the
/// engine is started with. The part of `start-windows.ps1` worth having in
/// Rust; the rest is downloading a release and spawning a process.
pub mod adapter;

/// The regex subset the adapters' `windows_asset_*` keys use, applied the way
/// PowerShell's `-match` applies them. The binary has no regex crate and will
/// not gain one, so this is the whole of what those patterns need - and a
/// construct outside it is an error, not a silent literal.
pub mod pattern;

/// `golem.toml`: the TOML subset the binary's own configuration file uses,
/// read and written line by line so the comments survive.
pub mod config;

/// The row builder behind `scripts/record-result.sh`: a captured bench or
/// eval output, plus what was measured on, becomes one line of
/// `bench/results.jsonl`.
pub mod record;

/// The date stamps a preset comment and a tuned overlay carry.
pub mod time;

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
