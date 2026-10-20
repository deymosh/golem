//! `golem bench` and `golem eval`: run the measurement, then record it.
//!
//! The port of `run_recorded` in the `golem` script. The measurement itself
//! stays `scripts/benchmark-ai.sh` and `scripts/eval-model.sh` - they are the
//! contract, the same way the launchers are the engine drivers - and what
//! moves here is the wrapper: the arguments, the tee, and the recording.
//!
//! Recording is the part that had to move. `record-result.sh` was a third
//! script in the chain (`./golem` → bench script → recorder), and the row it
//! writes is what the report, the console and the tuner all read; the port of
//! it is `golem_core::record`, and this module is what calls it.
//!
//! ## The tee
//!
//! The shell's `"$@" 2>&1 | tee "${out}"` does two things at once: the run's
//! output is shown *live* (a 200K needle takes minutes, and a silent terminal
//! reads as a hung one) and captured for the recorder. Both are kept here.
//! stdout and stderr are read on their own threads and merged into one
//! buffer, which is not quite the shell's byte-level interleaving - the two
//! streams arrive in order *within* themselves - and is exactly as good for
//! what the capture is used for: the `RESULT` line and the `[ OK ] tool
//! calling` marker are both written to stdout.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use golem_core::record;
use golem_core::results::Kind;

use crate::driver;
use crate::facts;
use crate::state;

/// `golem bench [--runs N] [--prefill N]`.
pub fn bench(home: &Path, args: &[String]) -> i32 {
    run(home, Kind::Bench, "scripts/benchmark-ai.sh", args)
}

/// `golem eval [--long N]`.
pub fn eval(home: &Path, args: &[String]) -> i32 {
    run(home, Kind::Eval, "scripts/eval-model.sh", args)
}

/// Measure, then record what was measured.
fn run(home: &Path, kind: Kind, script: &str, args: &[String]) -> i32 {
    // `--replay <file>` records a capture that already exists instead of
    // taking a new measurement. It is how the port of the recorder is checked
    // against the shell's on the same output, and it is what a backfill needs
    // (the shell reached the same place through `record-result.sh` by hand).
    if let Some(file) = replay_path(args) {
        let captured = match std::fs::read_to_string(&file) {
            Ok(text) => text,
            Err(e) => {
                eprintln!("[FAIL] cannot read {}: {e}", file.display());
                return 1;
            }
        };
        return record_capture(home, kind, &captured);
    }

    let (status, captured) = match measure(home, script, args) {
        Ok(outcome) => outcome,
        Err(e) => {
            eprintln!("[FAIL] cannot run {script}: {e}");
            return 1;
        }
    };
    // The shell's `[[ "${status}" == 0 && "${RECORD:-1}" != 0 ]]`: a run that
    // failed is not a measurement, however much it printed.
    if status != 0 {
        return status;
    }
    record_capture(home, kind, &captured)
}

/// The file named by `--replay`, if the flag is there.
fn replay_path(args: &[String]) -> Option<PathBuf> {
    let at = args.iter().position(|a| a == "--replay")?;
    args.get(at + 1).map(PathBuf::from)
}

/// Run `script` with `args`, showing the output as it arrives and returning
/// both the exit code and everything that was printed.
fn measure(home: &Path, script: &str, args: &[String]) -> std::io::Result<(i32, String)> {
    let mut command = Command::new(driver::bash());
    command
        .current_dir(home)
        .arg(script)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;

    let captured = Arc::new(Mutex::new(String::new()));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(spawn_reader(out, Arc::clone(&captured), false));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(spawn_reader(err, Arc::clone(&captured), true));
    }

    let status = child.wait()?;
    for reader in readers {
        let _ = reader.join();
    }
    let text = captured.lock().map(|text| text.clone()).unwrap_or_default();
    Ok((status.code().unwrap_or(-1), text))
}

/// A thread that echoes one of the child's streams and appends it to the
/// capture.
///
/// stderr goes to *stdout* here, which is the shell's `2>&1`: the two streams
/// are one stream by the time anything reads them.
fn spawn_reader<R: std::io::Read + Send + 'static>(
    stream: R,
    captured: Arc<Mutex<String>>,
    _is_stderr: bool,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let stdout = std::io::stdout();
        let mut reader = BufReader::new(stream);
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => break,
            }
            {
                let mut lock = stdout.lock();
                let _ = lock.write_all(&line);
                let _ = lock.flush();
            }
            if let Ok(mut text) = captured.lock() {
                text.push_str(&String::from_utf8_lossy(&line));
            }
        }
    })
}

/// Build the row for `captured`, append it, and say so - `run_recorded`'s
/// second half.
fn record_capture(home: &Path, kind: Kind, captured: &str) -> i32 {
    // `RECORD=0` skips the recording and keeps the run, which is what a
    // throwaway measurement wants.
    if process_var("RECORD").as_deref() == Some("0") {
        return 0;
    }
    // A file with no RESULT line is not a measurement (a failed or
    // interrupted run): recording it would put a null row in the database.
    // The shell's recorder says exactly this and exits 0, so a run that
    // measured nothing is not an error.
    if !captured.lines().any(|line| line.starts_with("RESULT")) {
        eprintln!("[WARN] no RESULT line in the captured output - nothing recorded");
        return 0;
    }

    // The four names the shell resolves before it calls the recorder, with
    // the same precedence: the per-run override, then `.env`, then a default.
    // `RECORD_MODEL` and `RECORD_NOTE` are overrides only - they are never
    // keys in `.env`, so they are read from the process alone.
    let engine = state::override_value(home, "GOLEM_ENGINE", "ENGINE")
        .unwrap_or_else(|| "llama.cpp".to_string());
    let model = process_var("RECORD_MODEL")
        .or_else(|| process_var("GOLEM_MODEL_ALIAS"))
        .or_else(|| state::env_value(home, "MODEL_ALIAS"))
        .unwrap_or_else(|| "local-model".to_string());
    let preset_rel =
        state::override_value(home, "GOLEM_MODELS_PRESET", "MODELS_PRESET").unwrap_or_default();
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
        // happened, and the shell's recorder treated this the same way.
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
/// the user's, and the shell sent the recorder's chatter to a log file.
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
/// The shell's `${NAME:-default}` for the names a caller sets and `.env`
/// never holds - `RECORD`, `RECORD_NOTE`, `RECORD_MODEL`. An empty value
/// counts as unset, as `${NAME:-}` does.
fn process_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
