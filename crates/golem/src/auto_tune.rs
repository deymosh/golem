//! `golem tune --auto`: pick the best installed model, then tune it.
//!
//! The worker is `tune::sweep` - the same sweep the single-preset path runs -
//! so what lives here is the driver: which presets are installed, a cheap
//! sweep for each, the ranking, and the full tune of the best one.
//!
//! The question it answers is the one a new machine asks: *set me up with the
//! smartest model this machine can run*. Three steps:
//!
//! 1. every preset whose GGUF is already in `data/models` gets a cheap sweep:
//!    the split frontier and nothing after it (`tune --frontier`), its output
//!    in `data/tuned/auto-<preset>.log`; the full tune takes those points
//!    back as kept ones;
//! 2. the presets are ranked by the agent suite where this machine has run
//!    it on the model's file (`results::agent_share`), by their vendor score
//!    where it has not - the library's (`config/library.ini`), else a
//!    `; Quality: <score>` comment in the preset; a number that is only ever
//!    a RANKING input, never an adoption reason - then by parameter count,
//!    the context they were measured at, and decode;
//! 3. the best-ranked preset is tuned fully (refine + gate) and adopted; if
//!    it fails the gate, the next ranked preset is tried.
//!
//! A model that is not downloaded is skipped, not fetched: that is
//! `./golem pull <preset>`'s job, and a download next to a measurement would
//! invalidate every number taken beside it.

use std::path::Path;

use golem_core::gguf;
use golem_core::rank::{self, Candidate};
use golem_core::results;
use golem_core::tune::Floor;
use golem_core::{ini, library};

use crate::config;
use crate::machine;
use crate::paths;
use crate::tune;

/// `golem tune --auto [flags]`.
pub fn run(home: &Path, args: &[String]) -> i32 {
    let opts = match Options::parse(home, args) {
        Ok(opts) => opts,
        Err(message) => {
            eprintln!("{message}");
            return 1;
        }
    };
    match auto_tune(home, &opts) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("[FAIL] {message}");
            1
        }
    }
}

/// The driver's flags, and the golem.toml defaults behind them.
///
/// The floors and the context policy are forwarded verbatim, as strings, for
/// `tune` to parse; `TUNE_RUNS` and `TUNE_PREFILL` are the two numbers
/// this driver owns, so they are parsed here for a clear error.
#[derive(Debug, Clone)]
struct Options {
    runs: u32,
    prefill: u32,
    adopt: bool,
    /// Whether the full tune refines. Off only when the user passed
    /// `--no-refine`; the cheap pass never does.
    refine: bool,
    /// The agent suite's floor (`TUNE_MIN_AGENT`): a model measured under it
    /// ranks below every model not measured at all.
    min_agent: f64,
    min_decode: String,
    min_prefill: String,
    min_depth_decode: String,
    /// `max` or a token count, forwarded to the tuner as `--ctx`.
    ctx: String,
    /// One engine to sweep, or empty for every installed one.
    engine: String,
}

impl Options {
    fn parse(home: &Path, args: &[String]) -> Result<Options, String> {
        let mut opts = Options {
            runs: env_number(home, "TUNE_RUNS", 3),
            prefill: env_number(home, "TUNE_PREFILL", 8000),
            adopt: false,
            refine: true,
            min_agent: config::get(home, "TUNE_MIN_AGENT")
                .and_then(|v| v.parse().ok())
                .unwrap_or(Floor::default().agent),
            min_decode: env_or(home, "TUNE_MIN_DECODE", "25"),
            min_prefill: env_or(home, "TUNE_MIN_PREFILL", "350"),
            min_depth_decode: env_or(home, "TUNE_MIN_DEPTH_DECODE", "15"),
            ctx: env_or(home, "TUNE_CTX_POLICY", "max"),
            engine: String::new(),
        };
        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            let mut value = |name: &str| -> Result<String, String> {
                i += 1;
                args.get(i).cloned().ok_or_else(|| format!("[FAIL] {name} needs a value"))
            };
            match arg {
                "--runs" => opts.runs = number(&value("--runs")?)?,
                "--prefill" => opts.prefill = number(&value("--prefill")?)?,
                "--adopt" => opts.adopt = true,
                "--refine" => opts.refine = true,
                "--no-refine" => opts.refine = false,
                "--min-decode" => opts.min_decode = value("--min-decode")?,
                "--min-prefill" => opts.min_prefill = value("--min-prefill")?,
                "--min-depth-decode" => opts.min_depth_decode = value("--min-depth-decode")?,
                "--ctx" => opts.ctx = value("--ctx")?,
                "--engine" => opts.engine = value("--engine")?,
                other if other.starts_with('-') => return Err(format!("unknown arg: {other}")),
                other => return Err(format!("tune-auto takes no preset argument: {other}")),
            }
            i += 1;
        }
        Ok(opts)
    }
}

fn number(text: &str) -> Result<u32, String> {
    text.parse().map_err(|_| format!("[FAIL] {text:?} is not a number"))
}

fn env_number(home: &Path, key: &str, fallback: u32) -> u32 {
    config::get(home, key).and_then(|v| v.parse().ok()).unwrap_or(fallback)
}

fn env_or(home: &Path, key: &str, fallback: &str) -> String {
    config::get(home, key).unwrap_or_else(|| fallback.to_string())
}

/// The driver itself: install check, cheap sweeps, ranking, full tune.
fn auto_tune(home: &Path, opts: &Options) -> Result<(), String> {
    // The same file every sweep starts from: never the effective preset an
    // earlier tune wrote, whose overlays would become the candidates' start.
    let preset_rel = tune::base_preset(home);
    let preset_path = home.join(&preset_rel);
    let text = std::fs::read_to_string(&preset_path)
        .map_err(|_| format!("preset file not found: {}", preset_path.display()))?;
    let doc = ini::parse(&text);
    let library = crate::paths::library(home);
    let models_dir = home.join("data/models");
    // What this machine measured: the agent suite where it ran on a file.
    let rows = results::read(&paths::results(home)).map(|(rows, _)| rows).unwrap_or_default();
    let host = machine::this_host();

    // ---- 1. the installed presets, in file order ------------------------
    //
    // A section whose model file is not in data/models is reported and
    // skipped, never fetched: a download would move every number measured
    // beside it.
    let mut installed: Vec<Candidate> = Vec::new();
    for name in doc.preset_names() {
        let Some(model) = doc.value(&name, "model") else { continue };
        let model = model.to_string();
        if !models_dir.join(&model).is_file() {
            println!("  {name}: skipped - {model} is not downloaded (golem pull {name})");
            continue;
        }
        let mut candidate = candidate(name, model, &text, &models_dir, &library);
        let file = Path::new(&candidate.model).file_name().map(|f| f.to_string_lossy().into_owned());
        candidate.agent = file.and_then(|file| results::agent_share(&rows, &host, &file));
        installed.push(candidate);
    }
    if installed.is_empty() {
        return Err(format!(
            "no installed preset to tune (data/models has none of {preset_rel}'s models)"
        ));
    }

    println!();
    println!("============================================================");
    println!(" AUTO-TUNE  [every installed preset]");
    println!("============================================================");
    println!("  preset file {preset_rel}   {} installed section(s)", installed.len());
    println!(
        "  floors      decode >= {} t/s, prefill >= {} t/s, at-depth >= {} t/s",
        opts.min_decode, opts.min_prefill, opts.min_depth_decode
    );
    println!(
        "  ranking     the agent suite where this machine ran it ({:.0}% or more first), \
         else the library's vendor score, else parameters",
        opts.min_agent * 100.0
    );
    println!();

    // ---- 2. a cheap sweep per preset, into its own log ------------------
    let work_dir = home.join("data/tuned");
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| format!("cannot create {}: {e}", work_dir.display()))?;
    let mut candidates: Vec<Candidate> = Vec::new();
    for candidate in &mut installed {
        println!(
            "  --- {} ({}, {:.2}B, {}): the split frontier, no gate",
            candidate.preset,
            candidate.model,
            candidate.params_b,
            candidate.ranked_by()
        );
        let log = work_dir.join(format!("auto-{}.log", candidate.preset));
        let file = std::fs::File::create(&log)
            .map_err(|e| format!("cannot create {}: {e}", log.display()))?;
        let args = sweep_args(&candidate.preset, opts);
        match tune::sweep(home, &args, Box::new(file)) {
            Ok(verdict) => {
                candidate.decode = verdict.decode;
                candidate.ctx = verdict.ctx;
                println!(
                    "      {:>6} t/s decode  {:>6} t/s prefill  ctx {}  vram {} MiB",
                    format!("{:.1}", verdict.decode),
                    format!("{:.0}", verdict.prefill),
                    verdict.ctx,
                    format!("{:.0}", verdict.vram_mib),
                );
                candidates.push(candidate.clone());
            }
            Err(_) => println!(
                "  [WARN] {}: no usable point (see {}) - not a candidate",
                candidate.preset,
                log.strip_prefix(home).unwrap_or(&log).display()
            ),
        }
    }
    if candidates.is_empty() {
        return Err("no preset produced a measurement above the floors".into());
    }

    // ---- 3. rank --------------------------------------------------------
    rank::rank(&mut candidates, opts.min_agent);
    println!();
    println!("  ranked (the agent suite, else the vendor score; then parameters, context, speed):");
    for c in &candidates {
        println!(
            "    {:<28} {:<14} {:>6}B  {:>6} t/s decode  ctx {}",
            c.preset,
            c.ranked_by(),
            format!("{:.2}", c.params_b),
            format!("{:.1}", c.decode),
            c.ctx
        );
    }

    // ---- 4. the full tune, down the ranking until one gates -------------
    let mut winner: Option<String> = None;
    for c in &candidates {
        println!();
        println!("[ OK ] best-ranked preset: {} - tuning it fully (refine + gate)", c.preset);
        let args = tune_args(&c.preset, opts);
        match tune::sweep(home, &args, Box::new(std::io::stdout())) {
            Ok(_) => {
                winner = Some(c.preset.clone());
                break;
            }
            Err(_) => println!(
                "  [WARN] {} did not pass the gate - trying the next ranked preset",
                c.preset
            ),
        }
    }
    let Some(winner) = winner else {
        return Err("no installed preset passed the gate; nothing adopted".into());
    };

    println!();
    println!(
        "[ OK ] auto-tune winner: {winner} (engine + flags in data/tuned/models.effective.ini)"
    );
    if opts.adopt {
        println!("[ OK ] adopted in golem.toml; restart to serve it (golem restart)");
    } else {
        println!(
            "  to adopt, re-run with --adopt, or set \
             MODELS_PRESET = \"data/tuned/models.effective.ini\" in golem.toml"
        );
    }
    Ok(())
}

/// The cheap sweep's flags: the split frontier only, then the shared knobs.
///
/// The ranking reads a preset's agent suite or vendor score and its size
/// before its speed, so the refine points and the context search are spent
/// only on the preset the full tune picks - which takes the frontier's
/// points back as kept ones while the desktop holds still.
fn sweep_args(preset: &str, opts: &Options) -> Vec<String> {
    let mut args = vec![preset.to_string(), "--frontier".to_string()];
    shared_args(&mut args, opts);
    args
}

/// The full tune's flags: the shared knobs, `--no-refine` when asked, and
/// `--adopt` when asked.
fn tune_args(preset: &str, opts: &Options) -> Vec<String> {
    let mut args = vec![preset.to_string()];
    shared_args(&mut args, opts);
    if !opts.refine {
        args.push("--no-refine".to_string());
    }
    if opts.adopt {
        args.push("--adopt".to_string());
    }
    args
}

/// The flags both sweeps carry: the run size, the floors, and the engine and
/// context policy when the driver was given them.
fn shared_args(args: &mut Vec<String>, opts: &Options) {
    args.extend([
        "--runs".to_string(),
        opts.runs.to_string(),
        "--prefill".to_string(),
        opts.prefill.to_string(),
        "--min-decode".to_string(),
        opts.min_decode.clone(),
        "--min-prefill".to_string(),
        opts.min_prefill.clone(),
        "--min-depth-decode".to_string(),
        opts.min_depth_decode.clone(),
    ]);
    if !opts.engine.is_empty() {
        args.push("--engine".to_string());
        args.push(opts.engine.clone());
    }
    // `max` is the tuner's own default, so only a pin is forwarded.
    if opts.ctx != "max" {
        args.push("--ctx".to_string());
        args.push(opts.ctx.clone());
    }
}

/// One installed preset, before its cheap sweep: the vendor score from the
/// library, else a `; Quality:` comment in the preset, and the parameter
/// count from the GGUF header.
fn candidate(preset: String, model: String, preset_text: &str, models_dir: &Path, library: &[library::Entry]) -> Candidate {
    let params_b = gguf::read(&models_dir.join(&model))
        .map(|f| f.params_total as f64 / 1e9)
        .unwrap_or(0.0);
    let quality = library
        .iter()
        .find(|e| e.name == preset)
        .and_then(|e| e.quality)
        .map(|q| q.to_string())
        .or_else(|| rank::quality_of(preset_text, &preset));
    Candidate { preset, model, quality, agent: None, params_b, decode: 0.0, ctx: 0 }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_scores_come_from_the_shipped_library() {
        let library = library::parse(crate::paths::EMBEDDED_LIBRARY);
        let score = |name: &str| library.iter().find(|e| e.name == name).and_then(|e| e.quality);
        assert_eq!(score("ornith-1.5-35b"), Some(79.0));
        assert_eq!(score("qwen3.6-35b-a3b"), Some(73.4));
        // The everyday model and the smoke test carry no score, so they can
        // never outrank a recommended coder; a rejected experiment is not in
        // the library at all.
        assert_eq!(score("qwen3.5-9b"), None);
        assert_eq!(score("qwen2.5-0.5b-instruct"), None);
        assert_eq!(score("nemotron-3.5-lightning-30b"), None);
    }
}
