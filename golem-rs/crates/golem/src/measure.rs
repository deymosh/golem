//! `golem bench` and `golem eval`: run the measurement, then record it.
//!
//! The measurement is [`crate::bench`]'s or [`crate::eval`]'s; what lives
//! here is the wrapper around it: the arguments, the tee, and the recording.
//!
//! ## The tee
//!
//! The run's output has to do two things at once: be shown *live* (a 200K
//! needle takes minutes, and a silent terminal reads as a hung one) and be
//! captured for the recorder. [`Tee`] is both - the harness writes to it, it
//! echoes to stdout, and it keeps the text.

use std::io::Write;
use std::path::{Path, PathBuf};

use golem_core::record;
use golem_core::results::Kind;

use crate::config;
use crate::{bench, eval, facts};

/// `golem bench [--runs N] [--prefill N]`.
pub fn bench(home: &Path, args: &[String]) -> i32 {
    run(home, Kind::Bench, args, bench::run)
}

/// `golem eval [--long N]`, or `golem eval --agent [anthropic|openai]` for
/// the agent suite, which prints its verdicts and records no row yet.
pub fn eval(home: &Path, args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("--agent") {
        return crate::agent::run(home, &args[1..], &mut std::io::stdout());
    }
    run(home, Kind::Eval, args, eval::run)
}

/// Everything a run printed, and stdout as it printed it.
///
/// A `Write` so the harness does not know about terminals, and a buffer so
/// the recorder gets exactly what the user saw.
#[derive(Debug, Default)]
pub struct Tee {
    text: String,
}

impl Tee {
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.text.push_str(&String::from_utf8_lossy(buf));
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        lock.write_all(buf)?;
        lock.flush()?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

/// Measure with `harness`, then record what was measured.
fn run(
    home: &Path,
    kind: Kind,
    args: &[String],
    harness: fn(&Path, &[String], &mut dyn Write) -> i32,
) -> i32 {
    // `--replay <file>` records a capture that already exists instead of
    // taking a new measurement: what a backfill needs, and how a change to
    // the recorder is checked against output that is already on disk.
    if let Some(file) = replay_path(args) {
        let captured = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("[FAIL] cannot read {}: {e}", file.display());
                return 1;
            }
        };
        if args.iter().any(|a| a == "--reject") {
            return record_rejection(home, &captured, args);
        }
        return record_capture(home, kind, &captured);
    }

    let mut tee = Tee::default();
    let status = harness(home, args, &mut tee);
    // A run that failed is not a measurement, however much it printed.
    if status != 0 {
        return status;
    }
    record_capture(home, kind, tee.text())
}

/// The file named by `--replay`, if the flag is there.
fn replay_path(args: &[String]) -> Option<PathBuf> {
    let at = args.iter().position(|a| a == "--replay")?;
    args.get(at + 1).map(PathBuf::from)
}

/// The value of `--flag value`, or None.
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1).map(String::as_str)
}

/// Record a rejection: `golem bench --replay <capture> --reject [--note ...]`.
///
/// A rejection is a candidate the project measured and will not adopt, or one
/// it could not measure at all - and the second kind is the one worth having a
/// tool for, because "Strata refuses 236544 on this card" is a fact the report
/// should show next to the engine's own numbers. The capture is the evidence
/// (for a refusal, the engine's last lines) and the note is the reason; no
/// `RESULT` line is required, and the metrics come out `null` when there is
/// none to read.
fn record_rejection(home: &Path, captured: &str, args: &[String]) -> i32 {
    let (engine, model, preset_rel) = resolved(home);
    let note = arg_value(args, "--note")
        .map(str::to_string)
        .or_else(|| process_var("RECORD_NOTE"))
        .unwrap_or_default();
    let mut facts = facts::collect(home, Kind::Reject, &engine, &model, &preset_rel, &note);
    // The context a refusal is *about*: nothing served it, so the preset
    // cannot say, and the report files a row under it. The build is the same
    // kind of fact - an engine that never started answers no `/props`, and a
    // row that does not name the build it rejected is a row the next reader
    // has to guess at.
    if let Some(ctx) = arg_value(args, "--ctx").and_then(|v| v.parse().ok()) {
        facts.ctx = Some(ctx);
    }
    if let Some(build) = arg_value(args, "--build") {
        facts.engine_build = build.to_string();
    }
    let line = record::reject_row(&facts, captured);
    if let Err(e) = record::append(home, &line) {
        eprintln!("[FAIL] cannot append to bench/results.jsonl: {e}");
        return 1;
    }
    println!("[ OK ] recorded a rejection in bench/results.jsonl ({model})");
    0
}

/// The three names a run resolves before it records: the per-run override,
/// then golem.toml, then a default.
///
/// `RECORD_MODEL` and `RECORD_NOTE` are overrides only - they are never keys
/// in golem.toml, so they are read from the process alone.
fn resolved(home: &Path) -> (String, String, String) {
    let engine = config::resolved(home, "ENGINE")
        .unwrap_or_else(|| "llama.cpp".to_string());
    let model = process_var("RECORD_MODEL")
        .or_else(|| process_var("GOLEM_MODEL_ALIAS"))
        .or_else(|| config::get(home, "MODEL_ALIAS"))
        .unwrap_or_else(|| "local-model".to_string());
    let preset_rel = config::models_preset(home, &config::Overrides::from_env());
    (engine, model, preset_rel)
}

/// Build the row for `captured`, append it, and say so.
fn record_capture(home: &Path, kind: Kind, captured: &str) -> i32 {
    // `RECORD=0` skips the recording and keeps the run, which is what a
    // throwaway measurement wants.
    if process_var("RECORD").as_deref() == Some("0") {
        return 0;
    }
    // A file with no RESULT line is not a measurement (a failed or
    // interrupted run): recording it would put a null row in the database.
    // It is a warning and exit 0: a run that measured nothing is not an
    // error.
    if !captured.lines().any(|line| line.starts_with("RESULT")) {
        eprintln!("[WARN] no RESULT line in the captured output - nothing recorded");
        return 0;
    }

    // The names the row carries: the per-run override, then golem.toml,
    // then a default.
    let (engine, model, preset_rel) = resolved(home);
    let note = process_var("RECORD_NOTE").unwrap_or_default();

    match record_with(home, kind, &engine, &model, &preset_rel, &note, captured) {
        Ok(facts) => {
            println!(
                "[ OK ] recorded in bench/results.jsonl ({}, {model}, quiet={})",
                word(kind),
                facts.quiet
            );
            0
        }
        // A row that cannot be built is not a failed run: the measurement
        // happened, and its output is on the screen.
        Err(e) => {
            eprintln!("[WARN] {e}");
            0
        }
    }
}

/// Build the row for one measurement and append it, with everything named
/// explicitly rather than read from the environment.
///
/// This is the seam the tuner needs: it measures a *candidate* preset that
/// nothing in the environment names, so it cannot go through the
/// env-resolving path above. It prints nothing - the sweep's own output is
/// the user's.
pub fn record_with(
    home: &Path,
    kind: Kind,
    engine: &str,
    model: &str,
    preset_rel: &str,
    note: &str,
    captured: &str,
) -> Result<golem_core::record::Facts, String> {
    if !captured.lines().any(|line| line.starts_with("RESULT")) {
        return Err("no RESULT line in the captured output - nothing recorded".to_string());
    }
    let facts = facts::collect(home, kind, engine, model, preset_rel, note);
    let line = record::row(&facts, captured)?;
    record::append(home, &line).map_err(|e| format!("cannot append to results.jsonl: {e}"))?;
    Ok(facts)
}

/// The word a row's `kind` is recorded under.
fn word(kind: Kind) -> &'static str {
    match kind {
        Kind::Eval => "eval",
        _ => "bench",
    }
}

/// A per-invocation variable from this process's environment.
///
/// For the names a caller sets and golem.toml never holds - `RECORD`,
/// `RECORD_NOTE`, `RECORD_MODEL`. An empty value counts as unset.
fn process_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
