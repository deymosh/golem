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

use std::cmp::Ordering;
use std::path::Path;

use golem_core::gguf;
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
        let mut candidate = Candidate::installed(name, model, &text, &models_dir, &library);
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
    rank(&mut candidates, opts.min_agent);
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

/// One installed preset, and the numbers its cheap sweep produced.
#[derive(Debug, Clone, PartialEq)]
struct Candidate {
    preset: String,
    model: String,
    /// The vendor score: the library's, else a `; Quality:` comment in the
    /// preset.
    quality: Option<String>,
    /// The share of the agent suite this machine measured on the model's
    /// file, on the worse API, when it ran.
    agent: Option<f64>,
    /// Total parameters, in billions (`params_total / 1e9`).
    params_b: f64,
    /// Filled in by the cheap sweep.
    decode: f64,
    ctx: u64,
}

impl Candidate {
    fn installed(
        preset: String,
        model: String,
        preset_text: &str,
        models_dir: &Path,
        library: &[library::Entry],
    ) -> Candidate {
        // The GGUF header's `params_total`, in billions.
        let params_b = gguf::read(&models_dir.join(&model))
            .map(|f| f.params_total as f64 / 1e9)
            .unwrap_or(0.0);
        let quality = library
            .iter()
            .find(|e| e.name == preset)
            .and_then(|e| e.quality)
            .map(|q| q.to_string())
            .or_else(|| quality_of(preset_text, &preset));
        Candidate { preset, model, quality, agent: None, params_b, decode: 0.0, ctx: 0 }
    }

    /// What the ranking reads for it, as the run prints it.
    fn ranked_by(&self) -> String {
        match (self.agent, &self.quality) {
            (Some(share), _) => format!("agent {:.0}%", share * 100.0),
            (None, Some(q)) => format!("vendor {q}"),
            (None, None) => "unscored".to_string(),
        }
    }
}

/// Order the candidates best-first.
///
/// A model this machine ran the agent suite on is ranked by it: one that
/// solved at least `min_agent` of it on both APIs comes first, by its share;
/// one that solved less comes last, whatever a vendor says of it. Between
/// them, the models never measured, by the vendor score when the preset has
/// one. Ties go to the vendor score, then the parameter count, the context
/// and the decode speed. A preset with no score sorts below every scored
/// one: it has no claim to "smartest", however big it is.
fn rank(candidates: &mut [Candidate], min_agent: f64) {
    candidates.sort_by(|a, b| key(b, min_agent).partial_cmp(&key(a, min_agent)).unwrap_or(Ordering::Equal));
}

fn key(c: &Candidate, min_agent: f64) -> (u8, f64, u8, f64, f64, u64, f64) {
    let tier = match c.agent {
        Some(share) if share >= min_agent - 1e-9 => 2,
        None => 1,
        Some(_) => 0,
    };
    let score = c.quality.as_deref().and_then(|q| q.parse::<f64>().ok());
    (tier, c.agent.unwrap_or(0.0), u8::from(score.is_some()), score.unwrap_or(0.0), c.params_b, c.ctx, c.decode)
}

/// The `; Quality: <score>` comment of `section`, when the file has one.
///
/// The comment may sit immediately above the section header (where this repo
/// puts `; Download:`) or inside the section; both are read, because reading
/// only one attributes a score to the wrong section. A quality line inside
/// the wanted section answers at once; one in the section above is held as
/// `pending` until the next header claims it.
fn quality_of(text: &str, want: &str) -> Option<String> {
    let mut pending: Option<String> = None;
    let mut in_want = false;
    for line in text.lines() {
        if let Some(name) = header_name(line) {
            if name == want {
                if let Some(score) = pending.take() {
                    return Some(score);
                }
                in_want = true;
            } else {
                in_want = false;
            }
            pending = None;
            continue;
        }
        if let Some(score) = quality_line(line) {
            if in_want {
                return Some(score);
            }
            pending = Some(score);
        }
    }
    None
}

/// The `[section]` name of a header line, when it is one (column 0 only,
/// exactly as `ini::parse` reads them).
fn header_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let (name, _) = rest.split_once(']')?;
    (!name.is_empty()).then_some(name)
}

/// The numeric part of a `; Quality: <score>` comment, when the line is one.
///
/// `;` then optional space then `Quality:`/`quality:`, then the leading run of
/// digits and dots - `; Quality: 79 (SWE-V)` reads as `79`, and a comment with
/// no number at all reads as nothing.
fn quality_line(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(';')?.trim_start();
    let rest = rest.strip_prefix("Quality:").or_else(|| rest.strip_prefix("quality:"))?;
    let digits: String =
        rest.trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    (!digits.is_empty()).then_some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(preset: &str, quality: Option<&str>, params_b: f64, ctx: u64, decode: f64) -> Candidate {
        Candidate {
            preset: preset.to_string(),
            model: format!("{preset}.gguf"),
            quality: quality.map(str::to_string),
            agent: None,
            params_b,
            decode,
            ctx,
        }
    }

    fn names(candidates: &[Candidate]) -> Vec<String> {
        candidates.iter().map(|c| c.preset.clone()).collect()
    }

    #[test]
    fn a_scored_preset_outranks_a_bigger_unscored_one() {
        let mut candidates = vec![
            cand("big-unscored", None, 70.0, 262_144, 60.0),
            cand("small-scored", Some("52"), 30.0, 65_536, 40.0),
        ];
        rank(&mut candidates, 0.75);
        assert_eq!(names(&candidates), vec!["small-scored", "big-unscored"]);
    }

    #[test]
    fn ties_break_on_parameters_then_context_then_decode() {
        let mut candidates = vec![
            cand("slow", Some("79"), 35.0, 200_000, 50.0),
            cand("fast", Some("79"), 35.0, 200_000, 60.0),
            cand("shorter", Some("79"), 35.0, 100_000, 99.0),
            cand("bigger", Some("79"), 40.0, 10_000, 10.0),
            cand("smarter", Some("80"), 1.0, 1_000, 1.0),
        ];
        rank(&mut candidates, 0.75);
        // A better score first; then the bigger model; then the longer
        // context; then the faster decode.
        assert_eq!(names(&candidates), vec!["smarter", "bigger", "fast", "slow", "shorter"]);
    }

    #[test]
    fn a_measured_agent_suite_ranks_before_a_vendor_score() {
        let measured = |preset: &str, quality: Option<&str>, params_b: f64, agent: f64| Candidate {
            agent: Some(agent),
            ..cand(preset, quality, params_b, 262_144, 50.0)
        };
        let mut candidates = vec![
            // The 0.5B solved one of the first four tasks on the reference box.
            measured("smoke", None, 0.5, 0.25),
            cand("new-unmeasured", Some("85"), 30.0, 262_144, 70.0),
            measured("everyday", None, 9.0, 10.0 / 11.0),
            measured("coder", Some("79"), 35.0, 1.0),
        ];
        rank(&mut candidates, 0.75);
        // Proven first, by share; then what a vendor claims; a model under
        // the floor last.
        assert_eq!(names(&candidates), vec!["coder", "everyday", "new-unmeasured", "smoke"]);
        assert_eq!(candidates[0].ranked_by(), "agent 100%");
        assert_eq!(candidates[2].ranked_by(), "vendor 85");
        assert_eq!(candidates[3].ranked_by(), "agent 25%");
        // Equal shares fall back to the vendor score.
        let mut tied = vec![measured("small", None, 9.0, 1.0), measured("scored", Some("79"), 35.0, 1.0)];
        rank(&mut tied, 0.75);
        assert_eq!(names(&tied), vec!["scored", "small"]);
    }

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

    #[test]
    fn a_quality_comment_is_read_above_a_header_and_inside_the_section() {
        let text = "\
; Quality: 42
[above]
model = a.gguf

[inside]
; Quality: 7
model = i.gguf

[x]
model = x.gguf
; Quality: 5
[y]
model = y.gguf

[plain]
model = p.gguf
";
        // Above the header (here: in the preamble).
        assert_eq!(quality_of(text, "above").as_deref(), Some("42"));
        // Inside the section.
        assert_eq!(quality_of(text, "inside").as_deref(), Some("7"));
        // At the tail of the *previous* section, directly above [y]: the
        // section below it is the one it belongs to.
        assert_eq!(quality_of(text, "y").as_deref(), Some("5"));
        // A section with no comment anywhere above it has none.
        assert_eq!(quality_of(text, "plain"), None);
        assert_eq!(quality_of(text, "nope"), None);
    }

    #[test]
    fn a_quality_line_needs_its_semicolon_and_a_number() {
        assert_eq!(quality_line("; Quality: 79"), Some("79".to_string()));
        assert_eq!(quality_line("  ; quality: 73.4 (SWE-V)"), Some("73.4".to_string()));
        assert_eq!(quality_line("; Quality: none"), None);
        assert_eq!(quality_line("Quality: 79"), None, "a bare key is not a comment");
        assert_eq!(quality_line("; Optional: \"Quality: <score>\""), None);
    }
}
