//! `golem tune --auto`: pick the best installed model, then tune it.
//!
//! The worker is `tune::sweep` - the same sweep the single-preset path runs -
//! so what lives here is the driver: which presets are installed, a cheap
//! sweep for each, the ranking, and the full tune of the best one.
//!
//! The question it answers is the one a new machine asks: *set me up with the
//! smartest model this machine can run*. Three steps:
//!
//! 1. every preset whose GGUF is already in `data/models` gets a cheap sweep
//!    (no gate; the refine points are the user's to skip with
//!    `--no-refine`), its output in `data/tuned/auto-<preset>.log`;
//! 2. the presets are ranked by their `; Quality: <score>` comment - a vendor
//!    number that is only ever a RANKING input, never an adoption reason -
//!    else by parameter count, then the context they reached, then decode;
//! 3. the best-ranked preset is tuned fully (refine + gate) and adopted; if
//!    it fails the gate, the next ranked preset is tried.
//!
//! A model that is not downloaded is skipped, not fetched: that is
//! `./golem pull <preset>`'s job, and a download next to a measurement would
//! invalidate every number taken beside it.

use std::cmp::Ordering;
use std::path::Path;

use golem_core::gguf;
use golem_core::ini;

use crate::config;
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
    /// Whether the sweeps refine. Off only when the user passed
    /// `--no-refine`, which both the cheap pass and the full tune honour.
    refine: bool,
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
    let preset_rel =
        config::get(home, "MODELS_PRESET").unwrap_or_else(|| "config/models.ini".to_string());
    let preset_path = home.join(&preset_rel);
    let text = std::fs::read_to_string(&preset_path)
        .map_err(|_| format!("preset file not found: {}", preset_path.display()))?;
    let doc = ini::parse(&text);
    let models_dir = home.join("data/models");

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
        installed.push(Candidate::installed(name, model, &text, &models_dir));
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
    println!("  ranking     ; Quality: <score> when the preset has one, else parameters");
    println!();

    // ---- 2. a cheap sweep per preset, into its own log ------------------
    let work_dir = home.join("data/tuned");
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| format!("cannot create {}: {e}", work_dir.display()))?;
    let mut candidates: Vec<Candidate> = Vec::new();
    for candidate in &mut installed {
        let quality = match &candidate.quality {
            Some(q) => format!(", quality {q}"),
            None => String::new(),
        };
        println!(
            "  --- {} ({}, {:.2}B{quality}): sweep, no gate",
            candidate.preset, candidate.model, candidate.params_b
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
    rank(&mut candidates);
    println!();
    println!("  ranked (quality, then parameters, then context, then speed):");
    for c in &candidates {
        let quality = c.quality.clone().unwrap_or_else(|| "-".to_string());
        println!(
            "    {:<28} quality {:<6} {:>6}B  {:>6} t/s decode  ctx {}",
            c.preset,
            quality,
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

/// The cheap sweep's flags: no gate, then the shared knobs.
///
/// `--no-gate` always; `--no-refine` only when the *user* asked for it, so
/// the cheap pass does refine by default. Cheap means ungated, not
/// unrefined: the ranking is built from these numbers, and a pass without
/// the refine points would rank a different frontier than the full tune
/// then measures.
fn sweep_args(preset: &str, opts: &Options) -> Vec<String> {
    let mut args = vec![preset.to_string(), "--no-gate".to_string()];
    if !opts.refine {
        args.push("--no-refine".to_string());
    }
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
    /// The vendor `; Quality:` score, when the preset carries one.
    quality: Option<String>,
    /// Total parameters, in billions (`params_total / 1e9`).
    params_b: f64,
    /// Filled in by the cheap sweep.
    decode: f64,
    ctx: u64,
}

impl Candidate {
    fn installed(preset: String, model: String, preset_text: &str, models_dir: &Path) -> Candidate {
        // The GGUF header's `params_total`, in billions.
        let params_b = gguf::read(&models_dir.join(&model))
            .map(|f| f.params_total as f64 / 1e9)
            .unwrap_or(0.0);
        let quality = quality_of(preset_text, &preset);
        Candidate { preset, model, quality, params_b, decode: 0.0, ctx: 0 }
    }
}

/// Order the candidates best-first.
///
/// The key is the vendor score when the preset has one, then the parameter
/// count, the context it reached, and its decode speed. A preset with no
/// score sorts below every scored one: it has no claim to "smartest", however
/// big it is.
fn rank(candidates: &mut [Candidate]) {
    candidates.sort_by(|a, b| key(b).partial_cmp(&key(a)).unwrap_or(Ordering::Equal));
}

fn key(c: &Candidate) -> (u8, f64, f64, u64, f64) {
    let score = c.quality.as_deref().and_then(|q| q.parse::<f64>().ok());
    (u8::from(score.is_some()), score.unwrap_or(0.0), c.params_b, c.ctx, c.decode)
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
        rank(&mut candidates);
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
        rank(&mut candidates);
        // A better score first; then the bigger model; then the longer
        // context; then the faster decode.
        assert_eq!(names(&candidates), vec!["smarter", "bigger", "fast", "slow", "shorter"]);
    }

    #[test]
    fn the_quality_comment_is_read_from_the_real_preset_file() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../config/models.ini");
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        assert_eq!(quality_of(&text, "ornith-1.5-35b").as_deref(), Some("79"));
        assert_eq!(quality_of(&text, "qwen3.6-35b-a3b").as_deref(), Some("73.4"));
        assert_eq!(quality_of(&text, "nemotron-3.5-lightning-30b").as_deref(), Some("52"));
        // The quant variant and the smoke test carry no score, so they can
        // never outrank a recommended one.
        assert_eq!(quality_of(&text, "ornith-1.5-35b-mtp"), None);
        assert_eq!(quality_of(&text, "qwen3.5-9b"), None);
        assert_eq!(quality_of(&text, "qwen2.5-0.5b-instruct"), None);
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
