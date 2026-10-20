//! `golem tune <preset>`: the phase-4 tuner.
//!
//! A port of `scripts/tune.sh`, and the same division of labour as the rest
//! of the binary: **this file measures and reports, `golem_core::tune`
//! decides**. Which candidate wins, what the floors exclude, and how far the
//! context ladder climbs are all pure functions there; what is here is the
//! probing, the restarts, the benchmarks, the gate and the verdict - the
//! parts that touch a GPU and a process.
//!
//! The question it answers is the one a new machine asks: *what is the best
//! way to run this model here?* For every engine that can serve the preset,
//! sweep the offload split with real requests, keep the fastest setup that
//! is not spilling VRAM and clears the speed floors, then gate it with the
//! long-context eval.
//!
//! Two things are deliberately *not* here yet, and both say so out loud
//! rather than quietly doing something else: `--auto` (that is
//! `scripts/tune-auto.sh`, a driver with this script as its worker) and the
//! migration to `golem.toml` (`.env` is still every shell driver's config).
//!
//! ## Why the split matters
//!
//! On a card smaller than the model the experts live in system RAM
//! (`--n-cpu-moe`); every layer left on the CPU costs decode speed and every
//! layer moved to the GPU costs VRAM. Past ~97% of the card's memory the
//! Windows driver spills to system memory *silently* and prefill collapses,
//! so the optimum is the lowest `n-cpu-moe` that still fits. The cliff is
//! found by measurement, never by arithmetic: a candidate whose prefill is
//! far under the best prefill its own engine reached is the spill.

use std::cell::RefCell;
use std::fmt::Arguments;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

// `Verdict` here is this module's own (what the run settled on); the eval's
// per-candidate verdict is the gate's, and is named for it.
use golem_core::evalparse::{self, Verdict as Gate};
use golem_core::gguf;
use golem_core::ini::{self, Entry};
use golem_core::results::Kind;
use golem_core::time;
use golem_core::tune::{self, Floor, Point, Probe, Rung, RungOutcome};

use crate::runner;
use crate::engine;
use crate::state;

/// How long a restart may take before the candidate is written off.
///
/// The shell waits 120 × 5 s. A 21 GB model read from disk is the slow part
/// and it is worth the patience; a setup that never answers is not.
const HEALTH_TRIES: u32 = 120;
const HEALTH_SLEEP: Duration = Duration::from_secs(5);

/// Where a candidate restart's launcher output goes, relative to the home.
///
/// The launcher's own words are the answer when a candidate will not start,
/// so the sweep shows them rather than a message of its own invention.
const RESTART_LOG: &str = "logs/tune-restart.log";

/// Where the gate's restart goes. Its own file, for the reason the shell had
/// two: a gate that fails should not overwrite the sweep's last restart.
const GATE_LOG: &str = "logs/tune-gate.log";

/// How much RAM beyond the model itself a *locked* load needs.
///
/// The gate is the one load that keeps the preset's `mlock`, and the server
/// needs working room on top of the weights; two gigabytes is the point below
/// which a 20 GB model on a 32 GB box starts fighting the desktop.
const SWEEP_RAM_MARGIN_GB: f64 = 2.0;

/// How long a restart will wait for the previous candidate's RAM to come back
/// before trying anyway. Windows returns locked pages a moment after the
/// process dies, and that moment is long enough to matter.
const RAM_SETTLE: Duration = Duration::from_secs(60);

/// How far above the seed the search looks when the seed itself spills.
///
/// Each step is two layers (less on the card), and the placement tool aims at
/// the wall, so a seed that does not fit is one or two steps from fitting -
/// a desktop that grew by a gigabyte, not a different machine.
const SEED_STEPS_UP: u32 = 6;

/// How many halvings the frontier search may spend.
///
/// `log2(61) < 6`, so eight is generous for a split range of 0..=60; it exists
/// so a pathological measurement cannot turn the search into a walk.
const MAX_BISECT: usize = 8;

/// How close to the decode floor a split has to be before the thread probe is
/// worth a model load.
///
/// Threads move decode by a few percent (16 of 20 threads beat all 20 by ~8%
/// on the reference box), so a setup already well clear of the floor is not
/// one a thread count rescues - and the probe is a whole load, which is what
/// this tuner is spending its budget on. Within a quarter of the floor, the
/// knob can still be the difference between passing and failing.
const THREAD_PROBE_MARGIN: f64 = 1.25;

/// `golem tune [preset] [flags]`.
#[derive(Debug, Clone)]
pub struct Options {
    /// The preset section to tune. Defaults to `MODEL_ALIAS`.
    pub target: String,
    /// Engines to sweep; empty means every installed one that can be tuned.
    pub engines: Vec<String>,
    pub runs: u32,
    pub prefill: u32,
    pub gate: bool,
    pub refine: bool,
    /// Explicit offload splits instead of the sweep around the seed.
    pub candidates: Vec<u32>,
    pub adopt: bool,
    pub dry_run: bool,
    pub floor: Floor,
    pub ctx: CtxPolicy,
    pub gate_tokens_max: u64,
}

/// How far the context may grow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtxPolicy {
    /// Predict the ceiling from the points measured so far and confirm it
    /// (`TUNE_CTX_POLICY=max`, the default).
    Max,
    /// Serve exactly this (`--ctx N`, or `TUNE_CTX_POLICY=<number>`).
    Pinned(u64),
    /// The doubling ladder, one midpoint probe after a failure - what `Max`
    /// was before the prediction, and reachable with `--ctx-ladder` until a
    /// measured sweep says the prediction is at least as good.
    Ladder,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            target: String::new(),
            engines: Vec::new(),
            runs: 3,
            prefill: 8000,
            gate: true,
            refine: true,
            candidates: Vec::new(),
            adopt: false,
            dry_run: false,
            floor: Floor::default(),
            ctx: CtxPolicy::Max,
            gate_tokens_max: 200_000,
        }
    }
}

impl Options {
    /// Read the flags, then let `.env` fill in what they did not say.
    ///
    /// `.env` first for the floors and the context policy, because a flag
    /// must beat it; `TUNE_MIN_*` and `TUNE_CTX_POLICY` are the knobs
    /// docs/plan-tune-model-context.md promises the user.
    pub fn parse(home: &Path, args: &[String]) -> Result<Options, String> {
        let mut opts = Options::default();
        if let Some(v) = state::env_value(home, "TUNE_MIN_DECODE").and_then(|v| v.parse().ok()) {
            opts.floor.decode = v;
        }
        if let Some(v) = state::env_value(home, "TUNE_MIN_PREFILL").and_then(|v| v.parse().ok()) {
            opts.floor.prefill = v;
        }
        if let Some(v) = state::env_value(home, "TUNE_MIN_DEPTH_DECODE")
            .and_then(|v| v.parse().ok())
        {
            opts.floor.depth_decode = v;
        }
        if let Some(v) = state::env_value(home, "TUNE_CTX_POLICY") {
            if v == "ladder" {
                opts.ctx = CtxPolicy::Ladder;
            } else if v != "max" {
                if let Ok(n) = v.parse() {
                    opts.ctx = CtxPolicy::Pinned(n);
                }
            }
        }
        if let Some(v) = state::env_value(home, "GATE_TOKENS").and_then(|v| v.parse().ok()) {
            opts.gate_tokens_max = v;
        }

        let mut i = 0;
        while i < args.len() {
            let arg = args[i].as_str();
            // Every flag that takes a value, so a missing one is a clear
            // error rather than a silent default.
            let mut value = |name: &str| -> Result<String, String> {
                i += 1;
                args.get(i).cloned().ok_or_else(|| format!("{name} needs a value"))
            };
            match arg {
                "--runs" => opts.runs = number(&value("--runs")?)?,
                "--prefill" => opts.prefill = number(&value("--prefill")?)?,
                "--gate" => opts.gate = true,
                "--no-gate" => opts.gate = false,
                "--refine" => opts.refine = true,
                "--no-refine" => opts.refine = false,
                "--candidates" => {
                    opts.candidates = value("--candidates")?
                        .split(',')
                        .filter(|c| !c.trim().is_empty())
                        .map(|c| number(c.trim()))
                        .collect::<Result<Vec<u32>, String>>()?;
                }
                "--engines" => {
                    opts.engines = value("--engines")?
                        .split(',')
                        .map(str::trim)
                        .filter(|e| !e.is_empty())
                        .map(str::to_string)
                        .collect();
                }
                "--engine" => {
                    let name = value("--engine")?;
                    opts.engines = vec![name];
                }
                "--adopt" => opts.adopt = true,
                "--dry-run" => opts.dry_run = true,
                "--min-decode" => opts.floor.decode = number(&value("--min-decode")?)? as f64,
                "--min-prefill" => opts.floor.prefill = number(&value("--min-prefill")?)? as f64,
                "--min-depth-decode" => {
                    opts.floor.depth_decode = number(&value("--min-depth-decode")?)? as f64
                }
                "--ctx" => {
                    let pin = value("--ctx")?;
                    opts.ctx = if pin == "max" {
                        CtxPolicy::Max
                    } else {
                        CtxPolicy::Pinned(number(&pin)? as u64)
                    };
                }
                // The ladder the prediction replaced, kept reachable until a
                // real sweep has been compared against it
                // (docs/plan-smart-tune.md, step 4).
                "--ctx-ladder" => opts.ctx = CtxPolicy::Ladder,
                "--auto" => {
                    return Err("--auto is the driver in crate::auto_tune, not this \
                                sweep: it ranks the installed presets and calls this \
                                tuner once per candidate. main.rs routes `tune --auto` \
                                there before this parser sees it, so reaching this arm \
                                means that routing was bypassed."
                        .to_string())
                }
                other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
                other => {
                    if !opts.target.is_empty() {
                        return Err(format!("only one preset can be tuned, got {other:?} too"));
                    }
                    opts.target = other.to_string();
                }
            }
            i += 1;
        }
        if opts.target.is_empty() {
            opts.target = state::env_value(home, "MODEL_ALIAS").unwrap_or_else(|| "local-model".into());
        }
        Ok(opts)
    }
}

fn number(text: &str) -> Result<u32, String> {
    text.parse().map_err(|_| format!("{text:?} is not a number"))
}

/// The machine the sweep is running on.
#[derive(Debug, Clone)]
pub struct Machine {
    pub gpu: String,
    pub vram_total_mib: u64,
    pub vram_used_mib: u64,
    pub ram_total_gb: u64,
    pub ram_free_gb: f64,
    pub cores: u32,
    pub physical_cores: u32,
    pub cpu: String,
}

impl Machine {
    /// What nvidia-smi, the OS and the CPU say.
    pub fn probe() -> Result<Machine, String> {
        let query = |fields: &str| -> Option<String> {
            let out = Command::new("nvidia-smi")
                .args([&format!("--query-gpu={fields}"), "--format=csv,noheader,nounits"])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").trim().to_string())
        };
        let Some(total) = query("name,memory.total,memory.used") else {
            return Err("auto-tune needs nvidia-smi (an NVIDIA card); for AMD set the split by hand".into());
        };
        let parts: Vec<&str> = total.split(',').map(str::trim).collect();
        if parts.len() < 3 {
            return Err(format!("nvidia-smi answered {total:?}, which is not a GPU"));
        }
        let cores = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(8);
        let cpu = cpu_name().unwrap_or_default();
        Ok(Machine {
            gpu: parts[0].to_string(),
            vram_total_mib: parts[1].parse().unwrap_or(0),
            vram_used_mib: parts[2].parse().unwrap_or(0),
            ram_total_gb: ram_total_gb(),
            ram_free_gb: state::ram_free_gb().unwrap_or(0.0),
            cores,
            physical_cores: physical_cores().unwrap_or(0),
            cpu,
        })
    }

    /// The anonymous hardware slug `record-result.sh` writes into every row.
    ///
    /// The same one `scripts/tune.sh` names its output directory after, and
    /// deliberately not a hostname: a row and a tuned setup travel with a
    /// shared report, and neither should carry the machine's name.
    pub fn host_slug(&self) -> String {
        format!(
            "{}_{}g_{}g_{}",
            slug(&self.gpu),
            self.vram_total_mib / 1024,
            self.ram_total_gb,
            slug(&self.cpu)
        )
    }

    /// VRAM the sweep may use: everything the desktop is not already using,
    /// less a margin. Only a seed for the sweep - the real cliff is the
    /// measured prefill collapse.
    pub fn wall_mib(&self) -> u64 {
        self.vram_total_mib.saturating_sub(self.vram_used_mib).saturating_sub(500)
    }
}

fn cpu_name() -> Option<String> {
    if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("model name") {
                let name = rest.trim_start_matches(':').trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    powershell("(Get-CimInstance Win32_Processor).Name")
}

fn physical_cores() -> Option<u32> {
    // MSYS's /proc/cpuinfo reports the logical count in `cpu cores`, so it is
    // only trusted where it is a real Linux file.
    if !cfg!(windows) {
        if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("cpu cores") {
                    if let Some(n) = rest.split(':').nth(1).and_then(|v| v.trim().parse().ok()) {
                        return Some(n);
                    }
                }
            }
        }
    }
    powershell("(Get-CimInstance Win32_Processor).NumberOfCores")?.parse().ok()
}

fn ram_total_gb() -> u64 {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                if let Some(kb) = rest.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) {
                    return (kb / 1_048_576.0).round() as u64;
                }
            }
        }
    }
    powershell("[math]::Round((Get-CimInstance Win32_OperatingSystem).TotalVisibleMemorySize/1MB,0)")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

fn powershell(script: &str) -> Option<String> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // PowerShell prints in the machine's locale: a comma decimal separator
    // would turn every number this reads into nonsense.
    let text = String::from_utf8_lossy(&out.stdout).trim().replace(',', ".").replace('\r', "");
    let first = text.lines().next().unwrap_or("").trim().to_string();
    (!first.is_empty()).then_some(first)
}

/// `scripts/tune.sh`'s `slug()`: lower case, the marketing words dropped,
/// everything else runs of one dash.
fn slug(text: &str) -> String {
    let mut stripped = text.to_lowercase().replace("(r)", "").replace("(tm)", "");
    for word in ["nvidia", "geforce", "intel", "core", "amd", "radeon", "processor"] {
        stripped = stripped.replace(word, "");
    }
    stripped = strip_generation(&stripped);
    let mut out = String::new();
    let mut in_run = false;
    for c in stripped.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Whether the thread probe is worth a load at this decode.
///
/// Pure, and separate from the probe itself, because the answer is the whole
/// reason the knob is not swept: see [`THREAD_PROBE_MARGIN`].
fn thread_probe_worth_it(decode: f64, floor: f64) -> bool {
    decode <= floor * THREAD_PROBE_MARGIN
}

/// The `[0-9]*th gen` in a CPU's marketing name ("12th Gen Intel...").
fn strip_generation(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("th gen") {
        let before = &rest[..at];
        let digits = before.chars().rev().take_while(char::is_ascii_digit).count();
        out.push_str(&before[..before.len() - digits]);
        rest = &rest[at + "th gen".len()..];
    }
    out.push_str(rest);
    out
}

/// One measured sweep point, and what it takes to run it again.
#[derive(Debug, Clone)]
struct Measured {
    engine: String,
    label: String,
    decode: f64,
    prefill: f64,
    vram_mib: f64,
    ctx: u64,
    overrides: Vec<Entry>,
    n_cpu_moe: String,
    passed_gate: Option<bool>,
    depth_decode: Option<f64>,
}

impl Measured {
    fn point(&self) -> Point {
        Point {
            engine: self.engine.clone(),
            label: self.label.clone(),
            decode: self.decode,
            prefill: self.prefill,
            ctx: self.ctx,
            passed_gate: self.passed_gate,
            depth_decode: self.depth_decode,
        }
    }
}

/// The setup a tune run settled on.
#[derive(Debug, Clone)]
pub struct Verdict {
    pub engine: String,
    pub n_cpu_moe: String,
    pub ctx: u64,
    pub decode: f64,
    pub prefill: f64,
    pub vram_mib: f64,
    pub gated_tokens: Option<u64>,
}

/// Everything a run needs to remember while it sweeps.
struct Tuner<'a> {
    home: &'a Path,
    opts: Options,
    machine: Machine,
    /// The preset file, as `.env` names it (relative to the home), and its
    /// text - every candidate and the overlay are written from this one
    /// reading, so a sweep cannot be tuned against a file that changed
    /// half-way through it.
    preset_rel: String,
    preset_text: String,
    /// `data/tuned/candidates/<preset>-<label>.ini`
    cand_dir: PathBuf,
    /// `data/tuned/<host>`
    out_dir: PathBuf,
    /// The GGUF the preset names, for the verdict file's first line.
    model_file: String,
    ctx_seed: u64,
    ctx_ceiling: u64,
    ubatch: String,
    port: u16,
    points: Vec<Measured>,
    /// Where the narration goes: stdout for `golem tune`, the per-preset log
    /// for the `--auto` driver. A `RefCell` so every `&self` method can write
    /// without threading a mutable borrow through the whole sweep.
    out: RefCell<Box<dyn Write>>,
}

/// Run `golem tune`. Returns the process exit code.
///
/// The work itself is [`sweep`]'s; what is here is only the exit code the
/// shell speaks in: 2 for flags it could not read, 1 for a run that found
/// nothing, 0 for a verdict.
pub fn run(home: &Path, args: &[String]) -> i32 {
    let opts = match Options::parse(home, args) {
        Ok(opts) => opts,
        Err(e) => {
            eprintln!("[FAIL] {e}");
            return 2;
        }
    };
    match sweep_with(home, opts, Box::new(std::io::stdout())) {
        Ok(_verdict) => 0,
        Err(message) => {
            eprintln!("[FAIL] {message}");
            1
        }
    }
}

/// The sweep as a function, so its caller can hold the verdict instead of
/// reading the `RESULT` line back out of a log.
///
/// `golem tune` prints through `out` exactly what it printed to stdout
/// before; the `--auto` driver passes a file and gets the same bytes in
/// `data/tuned/auto-<preset>.log`, which is where the shell put them too.
pub fn sweep(home: &Path, args: &[String], out: Box<dyn Write>) -> Result<Verdict, String> {
    let opts = Options::parse(home, args)?;
    sweep_with(home, opts, out)
}

fn sweep_with(home: &Path, opts: Options, out: Box<dyn Write>) -> Result<Verdict, String> {
    Tuner::new(home, opts, out)?.sweep()
}

impl<'a> Tuner<'a> {
    fn new(
        home: &'a Path,
        opts: Options,
        out: Box<dyn Write>,
    ) -> Result<Tuner<'a>, String> {
        let machine = Machine::probe()?;
        let preset_rel = state::env_value(home, "MODELS_PRESET")
            .unwrap_or_else(|| "config/models.ini".to_string());
        let preset = home.join(&preset_rel);
        let preset_text = std::fs::read_to_string(&preset)
            .map_err(|e| format!("cannot read {}: {e}", preset.display()))?;
        let doc = ini::parse(&preset_text);
        let model_file = doc
            .value(&opts.target, "model")
            .ok_or_else(|| format!("preset [{}] not found in {preset_rel}", opts.target))?
            .to_string();
        let model_path = home.join("data/models").join(&model_file);
        if !model_path.is_file() {
            return Err(format!(
                "model file not found: {} (run ./golem pull {})",
                model_path.display(),
                opts.target
            ));
        }
        let facts = gguf::read(&model_path)
            .map_err(|e| format!("cannot read {}: {e}", model_path.display()))?;
        let out_dir = home.join("data/tuned").join(machine.host_slug());
        let port = state::env_value(home, "LOCAL_AI_PORT")
            .and_then(|v| v.parse().ok())
            .unwrap_or(8000);
        let ctx_seed =
            doc.value(&opts.target, "ctx-size").and_then(|v| v.parse().ok()).unwrap_or(4096);
        let ubatch = doc.value(&opts.target, "ubatch-size").unwrap_or_default().to_string();
        Ok(Tuner {
            home,
            opts,
            machine,
            preset_rel,
            preset_text,
            cand_dir: home.join("data/tuned/candidates"),
            out_dir,
            model_file,
            ctx_seed,
            ctx_ceiling: facts.n_ctx_train,
            ubatch,
            port,
            points: Vec::new(),
            out: RefCell::new(out),
        })
    }

    /// One line of narration, to whichever sink this run was given.
    ///
    /// `println!` writes to the process's stdout and nowhere else; the
    /// `--auto` driver needs the same bytes in a per-preset log, so every
    /// print in this module goes through here instead.
    fn emit(&self, args: Arguments<'_>) {
        let mut out = self.out.borrow_mut();
        let _ = out.write_fmt(args);
        let _ = out.write_all(b"\n");
    }

    /// The whole run: plan, sweep, decide, gate, report.
    fn sweep(&mut self) -> Result<Verdict, String> {
        let engines = self.sweepable_engines()?;
        self.print_plan(&engines);
        if self.opts.dry_run {
            // The shell echoed each engine's seed and candidate list even in a
            // dry run, and only then skipped the measuring. A plan that omits
            // what it would spend is not the plan - and what it spends now is
            // a search, so the plan says that instead of a list.
            for engine in &engines {
                let seed = self.seed_for(engine);
                self.emit(format_args!(""));
                if self.opts.candidates.is_empty() {
                    self.emit(format_args!(
                        "  --- {engine}: seed {seed}, bisecting the frontier (at most {} loads)",
                        SEED_STEPS_UP / 2 + MAX_BISECT as u32 + 1
                    ));
                } else {
                    self.emit(format_args!(
                        "  --- {engine}: seed {seed}, measuring the given splits {}",
                        self.opts.candidates.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                    ));
                }
            }
            self.emit(format_args!(""));
            self.emit(format_args!("[ OK ] dry run: nothing started"));
            return Ok(Verdict {
                engine: engines[0].clone(),
                n_cpu_moe: "-".into(),
                ctx: self.ctx_seed,
                decode: 0.0,
                prefill: 0.0,
                vram_mib: 0.0,
                gated_tokens: None,
            });
        }

        // The sweep itself no longer needs the model's worth of RAM (see
        // `measure`), but the *gate* still loads it with the preset's own
        // `mlock`, and that is the one load this machine has to be able to
        // hold. Say the arithmetic up front rather than discovering it at the
        // end of a long sweep.
        let model_gb = self.model_gb();
        let free_gb = state::ram_free_gb().unwrap_or(0.0);
        if self.locks_ram() && free_gb > 0.0 && free_gb < model_gb + SWEEP_RAM_MARGIN_GB {
            self.emit(format_args!(
                "  [WARN] {free_gb:.1} GB of RAM free and the gate locks {model_gb:.1} GB - \
                 close what is using it before the gate runs"
            ));
        }

        for engine in &engines {
            self.sweep_engine(engine);
        }
        if self.points.is_empty() {
            return Err("no candidate produced a measurement".into());
        }
        let verdict = self.pick_winner()?;
        self.report(&verdict)?;
        Ok(verdict)
    }

    /// The engines this preset can be tuned on.
    ///
    /// Two kinds are named and skipped rather than swept:
    ///
    /// - an engine whose adapter says `tune = false` serves its own catalog
    ///   model and ignores preset flags (Strata), so there is nothing to sweep;
    /// - an engine whose adapter serves no `/v1/messages` **cannot win**,
    ///   because the client this repository exists for cannot talk to it.
    ///   Sweeping it spent ten model loads on a foregone conclusion and then
    ///   printed a warning saying so - the numbers belong in the database when
    ///   an engine is *being assessed*, not on every routine tune. Ask for it
    ///   by name (`--engines ik_llama.cpp`) when that is what you want.
    fn sweepable_engines(&self) -> Result<Vec<String>, String> {
        let installed = state::engines(self.home);
        if installed.is_empty() {
            return Err("no engines found under engines/".into());
        }
        let explicit = !self.opts.engines.is_empty();
        let wanted = if explicit { self.opts.engines.clone() } else { installed.clone() };
        let mut sweepable = Vec::new();
        for name in wanted {
            if !installed.contains(&name) {
                return Err(format!("unknown engine {name:?}"));
            }
            match self.engine_ini(&name, "tune").as_deref() {
                Some("false") => {
                    let reason = self.engine_ini(&name, "tune_reason").unwrap_or_default();
                    self.emit(format_args!("  {name}: skipped - {reason}"));
                }
                _ if !explicit && !self.serves_anthropic(&name) => {
                    self.emit(format_args!(
                        "  {name}: skipped - its adapter serves no /v1/messages, so it cannot \
                         serve this client (ask for it with --engines {name})"
                    ));
                }
                _ => sweepable.push(name),
            }
        }
        if sweepable.is_empty() {
            return Err(format!("no engine can be tuned for [{}]", self.opts.target));
        }
        Ok(sweepable)
    }

    /// One key from `engines/<name>/engine.ini`.
    fn engine_ini(&self, engine: &str, key: &str) -> Option<String> {
        let text = std::fs::read_to_string(self.home.join("engines").join(engine).join("engine.ini"))
            .ok()?;
        ini::parse(&text).global(key).map(str::to_string)
    }

    /// Whether an engine can serve the client this repository exists for.
    ///
    /// `api_anthropic = false` in an adapter means no `/v1/messages`, so
    /// Claude Code cannot talk to it at all - ik_llama.cpp is the one that
    /// says so. Such an engine is still swept and its numbers still land in
    /// the database (that is what a reject row is for), but it never *wins*:
    /// a tuner that adopts an engine the default client cannot use has
    /// answered the wrong question, however fast the engine is.
    fn serves_anthropic(&self, engine: &str) -> bool {
        self.engine_ini(engine, "api_anthropic").as_deref() != Some("false")
    }

    fn print_plan(&self, engines: &[String]) {
        let m = &self.machine;
        let ctx = match self.opts.ctx {
            CtxPolicy::Max => format!(
                "predict (ceiling: the model's {}-token trained ctx, confirmed by measurement)",
                self.ctx_ceiling
            ),
            CtxPolicy::Ladder => format!(
                "ladder (ceiling: the model's {}-token trained ctx)",
                self.ctx_ceiling
            ),
            CtxPolicy::Pinned(n) => format!("pinned at {n}"),
        };
        self.emit(format_args!(""));
        self.emit(format_args!("============================================================"));
        self.emit(format_args!(" AUTO-TUNE  [{}] on {}", self.opts.target, m.gpu));
        self.emit(format_args!("============================================================"));
        self.emit(format_args!(
            "  model      {} ({:.1} GB)",
            self.model_file,
            self.model_gb()
        ));
        self.emit(format_args!("  engines    {}", engines.join(" ")));
        self.emit(format_args!(
            "  vram       {} MiB total, {} MiB in use by the desktop",
            m.vram_total_mib, m.vram_used_mib
        ));
        self.emit(format_args!(
            "             wall {} MiB (seed heuristic; the real cliff shows up as a",
            m.wall_mib()
        ));
        self.emit(format_args!("             prefill collapse during the sweep)"));
        self.emit(format_args!(
            "  ram free   {:.1} GB   cpu {} threads   ctx {}   ubatch {}",
            m.ram_free_gb, m.cores, self.ctx_seed, self.ubatch
        ));
        self.emit(format_args!(
            "  floors     decode >= {} t/s, prefill >= {} t/s, at-depth >= {} t/s",
            self.opts.floor.decode, self.opts.floor.prefill, self.opts.floor.depth_decode
        ));
        self.emit(format_args!("  ctx        {ctx}"));
        self.emit(format_args!(
            "  gate       needle at 90% of each candidate's tuned context, up to {} tokens",
            self.opts.gate_tokens_max
        ));
        if self.locks_ram() {
            self.emit(format_args!(
                "  load mode  sweep points use mmap; the gate and the adopted setup keep {}",
                self.preset_value("load-mode").unwrap_or_default()
            ));
        }
        if m.vram_used_mib > 2000 {
            self.emit(format_args!(
                "  [WARN] {} MiB of VRAM is already in use - the numbers will be noisy",
                m.vram_used_mib
            ));
        }
    }

    // ---- measuring -----------------------------------------------------

    /// Sweep one engine's offload splits, then refine its best.
    ///
    /// **The frontier is bisected, not walked.** `n-cpu-moe N` keeps the first
    /// N layers' experts in RAM, so a lower N puts more on the card: the
    /// splits that fit are a contiguous run from the seed downward, and the
    /// frontier is one search rather than nine measurements. Nine model loads
    /// to learn one layer is what made a sweep take an hour and what made it
    /// hostile to the machine; three to five find the same answer.
    ///
    /// `--candidates a,b,c` still measures exactly the splits it is given -
    /// the escape hatch for reproducing an old sweep or checking this one.
    fn sweep_engine(&mut self, engine: &str) {
        let seed = self.seed_for(engine);
        self.emit(format_args!(""));
        if !self.opts.candidates.is_empty() {
            self.emit(format_args!(
                "  --- {engine}: seed {seed}, measuring the given splits {}",
                self.opts.candidates.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
            ));
            for n in self.opts.candidates.clone() {
                self.measure_split(engine, n);
            }
            if self.opts.refine {
                if let Some((split, _)) = self.best_measured(engine) {
                    self.refine(engine, split);
                }
            }
            return;
        }

        self.emit(format_args!(
            "  --- {engine}: seed {seed}, bisecting the frontier (at most {} loads)",
            SEED_STEPS_UP / 2 + MAX_BISECT as u32 + 1
        ));

        // The seed is the placement tool's answer for this context. Measure
        // it; when it does not fit - the desktop took more VRAM than the tool
        // assumed - step *up*, because a higher split puts less on the card.
        //
        // Every step up that spills is also the search's upper bound, and it
        // is kept: the candidates ascend, so the last spill is the highest one
        // known. Throwing it away and starting the bisection from "nothing has
        // spilled yet" is what made the search halve to n17 from n35 when it
        // already knew n33 did not fit.
        let mut fits: Option<u32> = None;
        let mut spills: Option<u32> = None;
        let mut n = seed;
        while n <= seed + SEED_STEPS_UP {
            match self.measure_split(engine, n) {
                Some(true) => {
                    fits = Some(n);
                    break;
                }
                Some(false) => {
                    self.emit(format_args!("  {engine}-n{n}: spilling"));
                    spills = Some(n);
                }
                None => {
                    self.emit(format_args!("  {engine}-n{n}: no measurement"));
                    spills = Some(n);
                }
            }
            n += 2;
        }
        let Some(mut best) = fits else {
            self.emit(format_args!(
                "  no split from {seed} to {} fit on this card - skipping {engine}",
                seed + SEED_STEPS_UP
            ));
            return;
        };

        // Then down, by halving the gap between what fits and what spilled.
        for _ in 0..MAX_BISECT {
            let next = match spills {
                Some(spills) => tune::between(best, spills),
                // Nothing spilled on the way up (the seed fit first try): the
                // lower end of the range is still open, so halve to it.
                None if best > 1 => Some(best / 2),
                None => None,
            };
            let Some(next) = next else { break };
            // A split that will not load is a split that does not fit: it is
            // the search's upper bound, not a lost point.
            match self.measure_split(engine, next) {
                Some(true) => best = next,
                Some(false) | None => spills = Some(next),
            }
        }
        self.emit(format_args!("  {engine}: frontier at n-cpu-moe {best}"));

        if self.opts.refine {
            self.refine(engine, best);
        }
    }

    /// Measure one split, and say whether it fit.
    ///
    /// "Fit" is two things, and neither is `wall_mib()`: that number is the
    /// *seed heuristic* - deliberately conservative, because the placement
    /// tool should leave room - so a split 60 MiB "over" it still serves at
    /// full prefill. Rejecting those would walk the search off the good end
    /// and find nothing.
    ///
    /// What is real is the card: a point above `vram_total - the desktop` has
    /// the driver spilling into system memory, whatever its prefill says. And
    /// the prefill floor is the project's own rule for "usable at all". A
    /// milder spill - prefill collapsed but still over the floor - is the
    /// ranking's business (`tune::exclusion` compares against the engine's
    /// best), not the search's.
    ///
    /// `None` is "no measurement at all" - the candidate would not start or
    /// the bench returned nothing - which for the search is the same answer as
    /// spilling: that split is not one to serve.
    fn measure_split(&mut self, engine: &str, n: u32) -> Option<bool> {
        let overrides = vec![Entry::new("n-cpu-moe", n.to_string()), Entry::new("fit", "off")];
        self.measure(engine, &format!("{engine}-n{n}"), &overrides)?;
        let point = self.points.last()?;
        let card = self.machine.vram_total_mib as f64 - self.machine.vram_used_mib as f64;
        Some(point.prefill >= self.opts.floor.prefill && point.vram_mib <= card)
    }

    /// The measured split with the best decode on `engine`, if any.
    fn best_measured(&self, engine: &str) -> Option<(u32, f64)> {
        self.points
            .iter()
            .filter(|p| p.engine == engine)
            .filter_map(|p| p.n_cpu_moe.parse::<u32>().ok().map(|n| (n, p.decode)))
            .max_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// The knobs worth one load each: prompt batch, threads, spec on/off.
    ///
    /// Every one of these is a *single* probe, not a sweep. The topology
    /// already says which thread count wins (16 of 20 - the P-cores with their
    /// hyperthreads - beats all 20 by ~8% on the reference box, because E-cores
    /// add sync overhead when decode streams RAM-resident experts), `ub` is the
    /// prefill lever the collapse test already catches when it is wrong, and MTP
    /// is either on and worth one comparison or absent and worth none. Probing
    /// neighbourhoods around values the documentation already settled was the
    /// other half of a seventeen-load sweep.
    fn refine(&mut self, engine: &str, best: u32) {
        self.emit(format_args!("  --- {engine}: refining n-cpu-moe {best} (batch, threads, spec)"));
        let base = || vec![Entry::new("n-cpu-moe", best.to_string()), Entry::new("fit", "off")];

        if !self.ubatch.is_empty() && self.ubatch != "1024" {
            let mut overrides = base();
            overrides.push(Entry::new("ubatch-size", "1024"));
            self.measure(engine, &format!("{engine}-ub1024"), &overrides);
        }

        // One thread point: the count the topology makes promising, and only
        // when it differs from what the preset already serves *and* when the
        // decode is close enough to the floor that the knob could decide
        // anything. A split already clear of the floor is not one a thread
        // count rescues, and the probe is a whole model load.
        if self.machine.cores > 4 && self.decode_at(engine, best).is_some_and(|d| thread_probe_worth_it(d, self.opts.floor.decode)) {
            let default =
                state::env_value(self.home, "CPU_THREADS").and_then(|v| v.parse().ok()).unwrap_or(0);
            if let Some(t) = self.thread_candidates().first().copied().filter(|t| *t > 0) {
                if t != default {
                    let mut overrides = base();
                    overrides.push(Entry::new("threads", t.to_string()));
                    self.measure(engine, &format!("{engine}-t{t}"), &overrides);
                }
            }
        }

        // Speculative decoding is only worth a point when the file can do it.
        if self.preset_value("spec-type").as_deref() == Some("draft-mtp") && self.model_has_mtp() {
            let mut overrides = base();
            overrides.push(Entry::new("spec-type", "none"));
            self.measure(engine, &format!("{engine}-nospec"), &overrides);
        }
    }

    /// The best decode measured at one split, or `None` when nothing was.
    fn decode_at(&self, engine: &str, n_cpu_moe: u32) -> Option<f64> {
        self.points
            .iter()
            .filter(|p| p.engine == engine)
            .filter(|p| p.n_cpu_moe.parse::<u32>().ok() == Some(n_cpu_moe))
            .map(|p| p.decode)
            .fold(None, |best: Option<f64>, d| Some(best.map_or(d, |b| b.max(d))))
    }

    /// The thread counts worth probing on this CPU.
    ///
    /// A hybrid P/E part has fewer logical threads than twice its physical
    /// cores (the i7-12700KF: 8 P with hyperthreads + 4 E = 20 logical, 12
    /// physical), so the P-cores-plus-HT count is `logical - physical`; a
    /// plain SMT box wants its physical count and all-but-two.
    fn thread_candidates(&self) -> Vec<u32> {
        let (cores, phys) = (self.machine.cores, self.machine.physical_cores);
        if phys > 0 && phys * 2 > cores {
            let p = cores - phys;
            vec![p, p * 2, cores]
        } else {
            vec![if phys > 0 { phys } else { cores / 2 }, cores.saturating_sub(2), cores]
        }
    }

    fn model_has_mtp(&self) -> bool {
        let model = self.preset_value("model").unwrap_or_default();
        gguf::read(&self.home.join("data/models").join(model))
            .map(|f| f.mtp_tensors > 0)
            .unwrap_or(false)
    }

    fn preset_value(&self, key: &str) -> Option<String> {
        ini::parse(&self.preset_text).value(&self.opts.target, key).map(str::to_string)
    }

    /// Whether the preset pins its weights into RAM.
    ///
    /// The sweep overrides this (see [`Tuner::measure`]); the gate and the
    /// adopted setup keep it, because it is a serving choice, not a speed one.
    fn locks_ram(&self) -> bool {
        matches!(self.preset_value("load-mode").as_deref(), Some("mlock" | "mmap+mlock"))
    }

    /// The offload split to start from: the engine's own placement tool when
    /// it ships one, else the preset, else a guess from the VRAM wall.
    fn seed_for(&self, engine: &str) -> u32 {
        if engine == "llama.cpp" {
            if let Some(seed) = self.fit_params_seed() {
                return seed;
            }
        }
        if let Some(n) = self.preset_value("n-cpu-moe").and_then(|v| v.parse().ok()) {
            return n;
        }
        let model_gb = self.model_gb().max(1.0);
        let wall = self.machine.wall_mib() as f64;
        (40.0 - 40.0 * wall / (model_gb * 1024.0)).max(0.0) as u32
    }

    /// How many layers `llama-fit-params` leaves on the CPU.
    fn fit_params_seed(&self) -> Option<u32> {
        let engine_dir = self.home.join(".local/engines/llama.cpp");
        let mut tool = None;
        if let Ok(entries) = std::fs::read_dir(&engine_dir) {
            for entry in entries.flatten() {
                for name in ["llama-fit-params", "llama-fit-params.exe"] {
                    let path = entry.path().join(name);
                    if path.is_file() {
                        tool = Some(path);
                    }
                }
            }
        }
        let tool = tool.or_else(|| {
            let path = self.home.join(".local/bin/llama-fit-params");
            path.is_file().then_some(path)
        })?;

        let model = self.preset_value("model")?;
        let out = Command::new(&tool)
            .current_dir(self.home.join("data/models"))
            .args([
                "-m",
                &model,
                "-c",
                &self.ctx_seed.to_string(),
                "-ctk",
                "q8_0",
                "-ctv",
                "q8_0",
                "-fa",
                "on",
                "-b",
                &self.ubatch_or_2048(),
                "-ub",
                &self.ubatch_or_2048(),
            ])
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        // `-ot "blk\.3\.ffn_.*_exps.=CPU"` style: the layers it keeps on the
        // CPU are the ones named in the -ot pattern.
        let pattern = text.split("-ot \"").nth(1)?.split('"').next()?;
        let layers = layer_names(pattern);
        (!layers.is_empty()).then(|| layers.len() as u32)
    }

    fn ubatch_or_2048(&self) -> String {
        if self.ubatch.is_empty() { "2048".to_string() } else { self.ubatch.clone() }
    }

    fn model_gb(&self) -> f64 {
        let model = self.preset_value("model").unwrap_or_default();
        std::fs::metadata(self.home.join("data/models").join(model))
            .map(|m| m.len() as f64 / 1_073_741_824.0)
            .unwrap_or(0.0)
    }

    /// Write the candidate preset, restart on it, bench it, record it.
    ///
    /// Returns `None` - and says why - when the candidate could not be
    /// measured at all, which is not the same as measuring badly: the sweep
    /// continues either way, but a point that produced no number is not a
    /// point.
    fn measure(&mut self, engine: &str, label: &str, overrides: &[Entry]) -> Option<()> {
        let candidate_rel = format!("data/tuned/candidates/{}-{label}.ini", self.opts.target);
        let candidate = self.home.join(&candidate_rel);
        std::fs::create_dir_all(&self.cand_dir).ok()?;
        // The sweep does not lock the model into RAM.
        //
        // Every point reloads the whole model, and `--load-mode mlock` pins
        // all of it: reloading a 20 GB model twenty times on a 32 GB box does
        // not fail the sweep, it takes the desktop with it - Windows runs out
        // of resources and the launcher cannot even be spawned (`os error
        // 1450`). Steady-state decode and prefill are the same under mmap,
        // because the first touch is paid by the bench's warmup rather than
        // by the number, so the placement is measured in the cheap mode.
        //
        // The gate is where the real mode is measured, and it is the one that
        // decides: one load, the preset's own flags, the serving config. And
        // the override deliberately does not enter `Measured::overrides`, so
        // the winner's overlay keeps the `load-mode` the preset serves with.
        let for_file = candidate_overrides(self.locks_ram(), overrides);
        let text = ini::write_candidate(&self.preset_text, &self.opts.target, &for_file);
        std::fs::write(&candidate, text).ok()?;

        self.emit(format_args!("  {engine} {label}: restarting..."));
        // The launcher's exit code, not just whether it could be spawned: a
        // launcher that ran and failed is the common case (a model that would
        // not fit, a port still held), and reporting both as "restart failed"
        // makes an afternoon out of a one-line answer. The launcher's own
        // output says which.
        match self.restart(engine, &candidate_rel, RESTART_LOG, false) {
            Ok(0) => {}
            Ok(code) => {
                self.emit(format_args!("  the launcher exited {code}, skipping - it said:"));
                self.say_launcher_tail(RESTART_LOG, 6);
                return None;
            }
            Err(e) => {
                self.emit(format_args!("  cannot run the launcher: {e}, skipping"));
                return None;
            }
        }
        if !self.wait_healthy() {
            self.emit(format_args!("  never became healthy, skipping"));
            return None;
        }

        let note = format!("tune {} on {engine}: {label}", self.opts.target);
        let out_file = self.cand_dir.join(format!("out-{}-{label}.txt", self.opts.target));
        let args = vec![
            "--runs".to_string(),
            self.opts.runs.to_string(),
            "--prefill".to_string(),
            self.opts.prefill.to_string(),
        ];
        // The bench is in process now (`crate::bench`), not
        // `scripts/benchmark-ai.sh` through Git Bash - which is what took a
        // shell off this path. It reads the model alias through the same
        // `GOLEM_MODEL_ALIAS` override every other reader uses, because the
        // tuner measures a candidate the environment does not name; the shell
        // exported it to the child, and this sets it for the call.
        let previous = std::env::var_os("GOLEM_MODEL_ALIAS");
        std::env::set_var("GOLEM_MODEL_ALIAS", &self.opts.target);
        let mut captured: Vec<u8> = Vec::new();
        let status = crate::bench::run(self.home, &args, &mut captured);
        match previous {
            Some(previous) => std::env::set_var("GOLEM_MODEL_ALIAS", previous),
            None => std::env::remove_var("GOLEM_MODEL_ALIAS"),
        }
        let text = String::from_utf8_lossy(&captured).into_owned();
        // The transcript stays on disk as the shell's redirect left it: it is
        // the evidence next to the row this point records.
        let _ = std::fs::write(&out_file, &text);
        if status != 0 {
            self.emit(format_args!("  the bench exited {status}, skipping"));
            self.say_tail(&text, 6);
            return None;
        }
        let Some(bench) = bench_result(&text) else {
            // Say what the bench said. "no usable result" on its own is the
            // kind of message that costs an afternoon: the bench's own last
            // lines distinguish a server that is down from a script that
            // never got as far as measuring anything.
            self.emit(format_args!("  no usable result, skipping - the bench said:"));
            self.say_tail(&text, 6);
            return None;
        };
        let (Some(decode), Some(vram)) = (bench.decode_median, bench.vram_mib) else {
            self.emit(format_args!("  no usable result, skipping - the bench said:"));
            self.say_tail(&text, 6);
            return None;
        };
        let prefill = bench.prefill.unwrap_or(0.0);
        let n_cpu_moe = overrides
            .iter()
            .find(|e| e.key == "n-cpu-moe")
            .map(|e| e.value.clone())
            .unwrap_or_default();
        let ctx = overrides
            .iter()
            .rev()
            .find(|e| e.key == "ctx-size")
            .and_then(|e| e.value.parse().ok())
            .unwrap_or(self.ctx_seed);

        self.emit(format_args!(
            "  {engine:<14} {label:<12} decode {decode:>6.1} t/s  prefill {prefill:>6.0} t/s  vram {vram:>5.0} MiB"
        ));
        self.points.push(Measured {
            engine: engine.to_string(),
            label: label.to_string(),
            decode,
            prefill,
            vram_mib: vram,
            ctx,
            overrides: overrides.to_vec(),
            n_cpu_moe,
            passed_gate: None,
            depth_decode: None,
        });
        // Every point is a measurement like any other, and the bench script
        // does not record on its own - recording lives in the ./golem
        // wrapper. Without this the database would hold the winner and none
        // of the evidence.
        //
        // `preset_rel` is the candidate, not the preset: the row's `flags`,
        // `ctx` and `threads` come from the file the recorder reads, so
        // pointing it at `.env`'s preset would label every sweep point with
        // the preset's own n-cpu-moe - a configuration that was never
        // measured. The shell tuner did exactly that, which is how a sweep of
        // nine splits ended up as nine identical rows.
        //
        // The recorder is `golem_core::record` now, not
        // `scripts/record-result.sh`: this was the last caller of that
        // script, and the row it built is verified byte-identical to the
        // shell's on real captures. A failure to record does not lose the
        // measurement - the sweep carries on, and the reason lands where the
        // shell put it, in the candidate directory's log.
        if let Err(e) = crate::measure::record_with(
            self.home,
            Kind::Bench,
            engine,
            &self.opts.target,
            &candidate_rel,
            &note,
            &text,
        ) {
            let _ = std::fs::write(self.cand_dir.join("record.log"), format!("{e}\n"));
        }
        Some(())
    }

    /// Restart the engine on `preset`, pointed there by the request's
    /// `GOLEM_*` overrides.
    ///
    /// The service is the only process that starts engines now, so the
    /// candidate's environment travels inside the request rather than in a
    /// child process's - which is also the one road it cannot be lost on. The
    /// old indirection existed because `./golem` re-sourced `.env`, and a plain
    /// `MODELS_PRESET` set here would have been overwritten; `GOLEM_*` names
    /// survive that, and now they ride the protocol. `lock` says whether this
    /// restart will pin the weights into RAM - the gate does, a sweep point
    /// does not - and only a locked load waits for the previous one's memory
    /// to come back.
    fn restart(&self, engine: &str, preset: &str, log: &str, lock: bool) -> std::io::Result<i32> {
        if lock {
            self.wait_for_ram();
        }
        let overrides = vec![
            ("GOLEM_ENGINE".to_string(), engine.to_string()),
            ("GOLEM_MODEL_ALIAS".to_string(), self.opts.target.clone()),
            ("GOLEM_MODELS_PRESET".to_string(), preset.to_string()),
        ];
        let log_path = self.home.join(log);
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut out = std::fs::File::options().create(true).append(true).open(&log_path)?;
        if !crate::service::running(self.home) {
            crate::service::ensure_running(self.home, &mut out)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        }
        Ok(crate::service::send_env(self.home, "restart", &overrides, &mut out).unwrap_or(1))
    }

    /// Wait until the machine can hold another copy of the model, bounded.
    ///
    /// The previous candidate's `mlock`ed pages come back a moment after its
    /// process dies, and a start that begins before they do cannot even spawn
    /// the launcher: Windows reports `os error 1450`, "insufficient system
    /// resources". This is where a sweep waits instead of walking into it.
    /// `None` from the RAM reader (a machine we cannot ask) is treated as
    /// "enough" - blocking on a number we do not have would be worse.
    fn wait_for_ram(&self) {
        let needed = self.model_gb() + SWEEP_RAM_MARGIN_GB;
        let deadline = Instant::now() + RAM_SETTLE;
        let mut said = false;
        loop {
            let free = state::ram_free_gb().unwrap_or(f64::INFINITY);
            if free >= needed || Instant::now() >= deadline {
                return;
            }
            if !said {
                self.emit(format_args!(
                    "  waiting for RAM to come back ({free:.1} of {needed:.1} GB free)..."
                ));
                said = true;
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// Wait for the engine to answer, up to the shell's own patience.
    fn wait_healthy(&self) -> bool {
        let endpoint = engine::Endpoint { port: self.port, api_key: String::new() };
        let deadline = Instant::now() + HEALTH_SLEEP * HEALTH_TRIES;
        while Instant::now() < deadline {
            if engine::get(&endpoint, "/health").is_some() {
                return true;
            }
            std::thread::sleep(HEALTH_SLEEP);
        }
        false
    }

    // ---- deciding ------------------------------------------------------

    /// Rank, maximize the context of each candidate in turn, gate it, and
    /// stop at the first one that passes.
    ///
    /// The gate is why this is a loop and not one call to
    /// `golem_core::tune::winner`: gating costs a restart and a 200K needle,
    /// so the shell only ever gates candidates in rank order until one
    /// passes. This is that loop; `winner` is the same rule stated as a
    /// function, for callers that already know every verdict.
    fn pick_winner(&mut self) -> Result<Verdict, String> {
        let floor = self.opts.floor;
        let ranked: Vec<usize> = tune::rank(&self.points.iter().map(Measured::point).collect::<Vec<_>>(), &floor);
        let points: Vec<Point> = self.points.iter().map(Measured::point).collect();
        self.emit(format_args!(""));
        self.emit(format_args!("  ranked (best decode that is not spilling):"));
        for &i in &ranked {
            let p = &points[i];
            self.emit(format_args!(
                "    {:<14} {:<12} {:>6.1} t/s decode  {:>6.0} t/s prefill  {:>5.0} MiB",
                p.engine, p.label, p.decode, p.prefill,
                self.points[i].vram_mib
            ));
        }
        // Say which points lost and why they are not in the ranking above:
        // an excluded setup that is simply absent reads as a bug.
        for (i, point) in points.iter().enumerate() {
            if ranked.contains(&i) {
                continue;
            }
            self.emit(format_args!(
                "  {} {}: excluded - {:.1} t/s decode, {:.0} t/s prefill",
                point.engine, point.label, point.decode, point.prefill
            ));
        }
        if ranked.is_empty() {
            return Err(format!(
                "every candidate was excluded (spilling, or under the floors decode >= {} / prefill >= {} t/s) - \
                 lower them with --min-decode/--min-prefill",
                floor.decode, floor.prefill
            ));
        }

        for &candidate in &ranked {
            let engine = self.points[candidate].engine.clone();
            if !self.serves_anthropic(&engine) {
                self.emit(format_args!(
                    "  [WARN] {engine} {} is in the ranking, but its adapter serves no \
                     /v1/messages - Claude Code cannot use it, so it cannot win",
                    self.points[candidate].label
                ));
                continue;
            }
            self.emit(format_args!(
                "[ OK ] candidate winner: {engine} {} ({:.1} t/s decode)",
                self.points[candidate].label, self.points[candidate].decode
            ));
            let key = self.maximize_ctx(candidate);
            let ctx = self.points[key].ctx;
            let n_cpu_moe = self.points[key].n_cpu_moe.clone();

            let gate_tokens = (ctx * 9 / 10).min(self.opts.gate_tokens_max);
            let overlay = self.write_overlay(&engine, key)?;
            if !self.opts.gate {
                return Ok(self.verdict(key, &engine, n_cpu_moe, None));
            }

            self.emit(format_args!("[ .. ] gating {engine} {}: restarting and running the {gate_tokens}-token needle (this takes a while)...", self.points[key].label));
            let restarted = matches!(self.restart(&engine, &overlay, GATE_LOG, true), Ok(0));
            if !restarted || !self.wait_healthy() {
                self.emit(format_args!("  [WARN] the engine did not come back on the effective preset - trying the next candidate"));
                self.say_launcher_tail(GATE_LOG, 6);
                continue;
            }
            let Some(eval) = self.gate(gate_tokens) else {
                self.emit(format_args!("  [WARN] the eval produced no result - trying the next candidate"));
                continue;
            };
            let depth = eval.needle.as_ref().and_then(|n| n.gen_tok_s_at_depth);
            match eval.needle.as_ref().map(|n| n.verdict) {
                Some(Gate::Pass) => {
                    // A context you cannot use at depth is a fake win.
                    if let Some(depth) = depth {
                        if depth > 0.0 && depth < floor.depth_decode {
                            self.emit(format_args!("  [WARN] decode at depth {depth} t/s is under the {} t/s floor - trying the next candidate", floor.depth_decode));
                            continue;
                        }
                    }
                    self.emit(format_args!("[ OK ] eval gate PASSED"));
                    self.points[key].passed_gate = Some(true);
                    self.points[key].depth_decode = depth;
                    return Ok(self.verdict(key, &engine, n_cpu_moe, Some(gate_tokens)));
                }
                _ => {
                    self.emit(format_args!("  [WARN] eval gate FAILED for {engine} + n-cpu-moe {n_cpu_moe} - trying the next candidate"));
                    self.points[key].passed_gate = Some(false);
                }
            }
        }
        // Nothing to adopt, and the reason is worth naming precisely: "no
        // candidate passed the gate" would be wrong when the candidates were
        // never gated because no engine in the ranking can serve Claude Code.
        let unusable = ranked.iter().filter(|&&i| !self.serves_anthropic(&self.points[i].engine)).count();
        if unusable == ranked.len() {
            return Err(
                "every ranked candidate runs on an engine whose adapter serves no \
                 /v1/messages, so none of them can be adopted for Claude Code"
                    .into(),
            );
        }
        Err("no candidate passed the eval gate; nothing adopted".into())
    }

    /// Grow the winner's context toward the ceiling while the card holds.
    ///
    /// The rule is `golem_core::tune::next_predicted`: two measured points at
    /// one split give VRAM per context token, and the largest context that
    /// fits under the wall is arithmetic from there - so the ladder's four
    /// doublings become one confirmation. What is here is the measuring and
    /// the two ways a probe fails, which the rule needs told apart: over the
    /// VRAM wall is a real point, under the decode floor is a spill whose
    /// VRAM reading means nothing.
    ///
    /// `--ctx-ladder` runs the doubling ladder instead, until a measured
    /// sweep says the prediction is at least as good
    /// (docs/plan-smart-tune.md, step 4).
    fn maximize_ctx(&mut self, candidate: usize) -> usize {
        let engine = self.points[candidate].engine.clone();
        let overrides = self.points[candidate].overrides.clone();

        if let CtxPolicy::Pinned(pin) = self.opts.ctx {
            let mut with_ctx = overrides.clone();
            with_ctx.push(Entry::new("ctx-size", pin.to_string()));
            let label = format!("{engine}-ctx{pin}");
            if self.measure(&engine, &label, &with_ctx).is_some() {
                return self.points.len() - 1;
            }
            return candidate;
        }

        if self.opts.ctx == CtxPolicy::Ladder {
            return self.maximize_ctx_ladder(candidate, &engine, &overrides);
        }

        let mut current = self.ctx_seed;
        let ceiling = self.ctx_ceiling.max(current);
        let wall = self.machine.wall_mib();
        let mut best = candidate;
        // The points that fit, as (context, VRAM MiB) - the slope comes from
        // the two highest of them, and a failed probe is not one of them
        // unless it went over the wall, where it is a real measurement.
        let mut fits: Vec<(u64, u64)> = Vec::new();
        let mut probe = Probe::None;
        loop {
            let next = match tune::next_predicted(&fits, wall, ceiling, &probe) {
                Rung::Stop => break,
                Rung::Grow(next) => next,
            };
            let mut with_ctx = overrides.clone();
            with_ctx.push(Entry::new("ctx-size", next.to_string()));
            let label = format!("{engine}-ctx{next}");
            if self.measure(&engine, &label, &with_ctx).is_none() {
                break;
            }
            let measured = self.points.len() - 1;
            let vram = self.points[measured].vram_mib;
            let decode = self.points[measured].decode;
            let over_wall = vram > wall as f64;
            let under_floor = decode < self.opts.floor.decode;
            if over_wall || under_floor {
                // Under the floor wins when both are true: the spill is what
                // made the decode slow, and the VRAM reading is where the
                // spill ended up rather than what the context costs - which
                // is the case the prediction must not treat as a point.
                probe = if under_floor {
                    Probe::UnderFloor { attempt: next }
                } else {
                    Probe::OverWall { attempt: next, vram_mib: vram as u64 }
                };
                self.emit(format_args!(
                    "  context {next} does not fit ({:.1} t/s decode, {:.0} MiB VRAM{})",
                    decode,
                    vram,
                    if under_floor { ", spilled" } else { "" }
                ));
                continue;
            }
            current = next;
            best = measured;
            fits.push((next, vram as u64));
            probe = Probe::Fits;
            self.emit(format_args!(
                "  context grows to {current} ({:.1} t/s decode, {:.0} MiB VRAM)",
                self.points[best].decode, self.points[best].vram_mib
            ));
        }
        if best != candidate {
            self.emit(format_args!("[ .. ] context maximized: {engine} now serves {} tokens", self.points[best].ctx));
        }
        best
    }

    /// The doubling ladder, kept for `--ctx-ladder`.
    ///
    /// `golem_core::tune::next_context`: double toward the ceiling, and after
    /// a failed rung try one midpoint. It is what the prediction replaced,
    /// and it stays reachable until a measured sweep compares the two
    /// (docs/plan-smart-tune.md, step 4) - a tuner that finds a different
    /// answer in fewer loads is only a win if the answer is at least as good.
    fn maximize_ctx_ladder(
        &mut self,
        candidate: usize,
        engine: &str,
        overrides: &[Entry],
    ) -> usize {
        let mut current = self.ctx_seed;
        let ceiling = self.ctx_ceiling.max(current);
        let mut best = candidate;
        let mut last = RungOutcome::Starting;
        loop {
            let next = match tune::next_context(current, ceiling, last.clone()) {
                Rung::Stop => break,
                Rung::Grow(next) => next,
            };
            let mut with_ctx = overrides.to_vec();
            with_ctx.push(Entry::new("ctx-size", next.to_string()));
            let label = format!("{engine}-ctx{next}");
            if self.measure(engine, &label, &with_ctx).is_none() {
                break;
            }
            let measured = self.points.len() - 1;
            let over_wall = self.points[measured].vram_mib > self.machine.wall_mib() as f64;
            let under_floor = self.points[measured].decode < self.opts.floor.decode;
            if over_wall || under_floor {
                // One midpoint between the last good context and this one,
                // then stop: finer steps are noise at this point.
                last = RungOutcome::Failed { attempt: next };
                if let Rung::Grow(mid) = tune::next_context(current, ceiling, last.clone()) {
                    let mut with_mid = overrides.to_vec();
                    with_mid.push(Entry::new("ctx-size", mid.to_string()));
                    let mid_label = format!("{engine}-ctx{mid}");
                    if self.measure(engine, &mid_label, &with_mid).is_some() {
                        let at = self.points.len() - 1;
                        if self.points[at].vram_mib <= self.machine.wall_mib() as f64
                            && self.points[at].decode >= self.opts.floor.decode
                        {
                            best = at;
                        }
                    }
                }
                break;
            }
            current = next;
            best = measured;
            last = RungOutcome::Fits;
            self.emit(format_args!(
                "  context grows to {current} ({:.1} t/s decode, {:.0} MiB VRAM)",
                self.points[best].decode, self.points[best].vram_mib
            ));
        }
        if best != candidate {
            self.emit(format_args!(
                "[ .. ] context maximized: {engine} now serves {} tokens",
                self.points[best].ctx
            ));
        }
        best
    }

    /// Write the winner's overlay, and the effective preset it feeds.
    ///
    /// Two files, as the shell writes them: the per-engine overlay records
    /// what won on this engine, and `models.effective.ini` is the preset
    /// file with that overlay merged in - the file a launcher actually
    /// serves after `--adopt`.
    fn write_overlay(&self, engine: &str, key: usize) -> Result<String, String> {
        let measured = &self.points[key];
        let dir = self.out_dir.join(engine);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let sweep: String = self
            .points
            .iter()
            .filter(|p| p.engine == engine)
            .map(|p| format!("{}={:.1} ", p.label, p.decode))
            .collect();
        let mut overlay = String::new();
        overlay.push_str(&format!("; Auto-tuned by golem tune on {} for this machine.\n", time::now_iso()));
        overlay.push_str(&format!(
            "; Winner for engine {engine} ({}); the launchers merge it over {}.\n",
            measured.label, self.preset_rel
        ));
        overlay.push_str(&format!("; Sweep (label=decode t/s): {sweep}\n"));
        overlay.push_str(&format!("[{}]\n", self.opts.target));
        // The bookkeeping key comes last and only once, as the shell writes
        // it: it is in every candidate's overrides, and a duplicate would be
        // a line the engine reads twice.
        for entry in measured.overrides.iter().filter(|e| e.key != "fit") {
            overlay.push_str(&entry.line());
            overlay.push('\n');
        }
        overlay.push_str("fit = off\n");

        let overlay_path = dir.join(format!("{}.ini", self.opts.target));
        std::fs::write(&overlay_path, &overlay)
            .map_err(|e| format!("cannot write {}: {e}", overlay_path.display()))?;
        let effective = ini::write_overlay(&self.preset_text, &overlay);
        let effective_rel = "data/tuned/models.effective.ini".to_string();
        std::fs::write(self.home.join(&effective_rel), effective)
            .map_err(|e| format!("cannot write {effective_rel}: {e}"))?;
        self.emit(format_args!("[ OK ] overlay: {}", overlay_path.strip_prefix(self.home).unwrap_or(&overlay_path).display()));
        Ok(effective_rel)
    }

    /// `eval --long N`, read through the RESULT-line contract.
    fn gate(&self, tokens: u64) -> Option<evalparse::Eval> {
        let out_file = self.out_dir.join(format!("eval-{}.txt", self.opts.target));
        // This binary's own verb, a child of the tuner - the measurement and the
        // thing it measures stay in one process tree, and the alias rides the
        // child's environment the way it always has.
        let exe = std::env::current_exe()
            .map_err(|e| format!("cannot find this binary to run the gate: {e}"))
            .ok()?;
        let mut command = std::process::Command::new(exe);
        command
            .args(["eval".to_string(), "--long".to_string(), tokens.to_string()])
            .env("GOLEM_MODEL_ALIAS", &self.opts.target)
            .env("GOLEM_HOME", self.home);
        runner::run_command(command, &out_file).ok()?;
        let text = std::fs::read_to_string(&out_file).unwrap_or_default();
        for line in text.lines().rev().filter(|l| l.trim_start().starts_with("RESULT")) {
            self.emit(format_args!("    {line}"));
        }
        Some(evalparse::Eval::parse(text.lines()))
    }

    fn verdict(&self, key: usize, engine: &str, n_cpu_moe: String, gated: Option<u64>) -> Verdict {
        let p = &self.points[key];
        Verdict {
            engine: engine.to_string(),
            n_cpu_moe,
            ctx: p.ctx,
            decode: p.decode,
            prefill: p.prefill,
            vram_mib: p.vram_mib,
            gated_tokens: gated,
        }
    }

    /// The verdict file, the one-line RESULT, and `--adopt`.
    fn report(&mut self, verdict: &Verdict) -> Result<(), String> {
        std::fs::create_dir_all(&self.out_dir)
            .map_err(|e| format!("cannot create {}: {e}", self.out_dir.display()))?;
        let mut text = String::new();
        text.push_str(&format!("model      {} ({})\n", self.opts.target, self.model_file));
        text.push_str(&format!("host       {}\n", self.machine.host_slug()));
        text.push_str(&format!("date       {}\n", time::now_iso()));
        text.push_str(&format!("engine     {}\n", verdict.engine));
        text.push_str(&format!("n-cpu-moe  {}\n", verdict.n_cpu_moe));
        text.push_str(&format!("ctx        {}\n", verdict.ctx));
        text.push_str(&format!(
            "decode     {:.1} t/s (median), prefill {:.0} t/s, vram {:.0} MiB\n",
            verdict.decode, verdict.prefill, verdict.vram_mib
        ));
        text.push_str(&format!(
            "eval       {}\n",
            verdict.gated_tokens.map(|t| format!("{t}-token needle PASS")).unwrap_or_else(|| "not run".into())
        ));
        text.push_str("\nall points:\n");
        for p in &self.points {
            text.push_str(&format!(
                "  {:<14} {:<12} {:>6.1} t/s decode  {:>6.0} t/s prefill  {:>5.0} MiB\n",
                p.engine, p.label, p.decode, p.prefill, p.vram_mib
            ));
        }
        let verdict_path = self.out_dir.join("winner.txt");
        std::fs::write(&verdict_path, text)
            .map_err(|e| format!("cannot write {}: {e}", verdict_path.display()))?;

        self.emit(format_args!(""));
        self.emit(format_args!(
            "[ OK ] winner: {} with n-cpu-moe {} - {:.1} t/s decode, {:.0} t/s prefill",
            verdict.engine, verdict.n_cpu_moe, verdict.decode, verdict.prefill
        ));
        self.emit(format_args!("[ OK ] verdict: {}", verdict_path.strip_prefix(self.home).unwrap_or(&verdict_path).display()));
        self.emit(format_args!(
            "RESULT tune_preset={} engine={} n_cpu_moe={} ctx={} decode={:.1} prefill={:.0} vram={:.0}",
            self.opts.target, verdict.engine, verdict.n_cpu_moe, verdict.ctx,
            verdict.decode, verdict.prefill, verdict.vram_mib
        ));

        if self.opts.adopt {
            let env = self.home.join(".env");
            std::fs::copy(&env, self.home.join(".env.bak"))
                .map_err(|e| format!("cannot back up .env: {e}"))?;
            state::env_adopt(self.home, "ENGINE", &verdict.engine)
                .map_err(|e| format!("cannot write .env: {e}"))?;
            state::env_adopt(self.home, "MODELS_PRESET", "data/tuned/models.effective.ini")
                .map_err(|e| format!("cannot write .env: {e}"))?;
            self.emit(format_args!(
                "[ OK ] adopted in .env (backup: .env.bak): ENGINE={}, MODELS_PRESET=data/tuned/models.effective.ini",
                verdict.engine
            ));
        } else {
            self.emit(format_args!(""));
            self.emit(format_args!("  to adopt, either re-run with --adopt or set in .env:"));
            self.emit(format_args!("    ENGINE=\"{}\"", verdict.engine));
            self.emit(format_args!("    MODELS_PRESET=\"data/tuned/models.effective.ini\""));
        }
        Ok(())
    }

    /// The last `lines` non-empty lines of a captured run, indented.
    fn say_tail(&self, text: &str, lines: usize) {
        let kept: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
        for line in kept.iter().skip(kept.len().saturating_sub(lines)) {
            self.emit(format_args!("        | {line}"));
        }
    }

    /// The same, for a log the launcher wrote rather than text we captured.
    fn say_launcher_tail(&self, log: &str, lines: usize) {
        let text = std::fs::read_to_string(self.home.join(log)).unwrap_or_default();
        self.say_tail(&text, lines);
    }
}

/// What a sweep candidate's file is written with: the caller's overrides,
/// plus `load-mode = auto` when the preset locks its weights.
///
/// The caller's list is what the winner's overlay is built from, so this is
/// deliberately a *copy* plus one entry: the sweep measures in mmap, and the
/// adopted setup keeps the mode the preset serves with. See
/// [`Tuner::measure`] for why that is the difference between a sweep and a
/// crashed desktop.
fn candidate_overrides(locks: bool, overrides: &[Entry]) -> Vec<Entry> {
    let mut out = overrides.to_vec();
    if locks {
        out.push(Entry::new("load-mode", "auto"));
    }
    out
}

/// The distinct layer names in a `-ot` placement pattern.
///
/// The tool prints its pattern with the dots escaped for its own regex -
/// `blk\.3\.ffn_.*_exps.=CPU` - and the shell greps `blk\\\.[0-9]*` (a
/// literal backslash, a literal dot, digits) and counts the unique hits.
/// Splitting the pattern on non-alphanumeric characters instead finds no
/// `blk.` token at all, because the backslash is one of them; the seed then
/// silently falls back to the preset's own split, and the sweep starts from
/// the wrong place. That is the bug this function exists to not have.
fn layer_names(pattern: &str) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let mut rest = pattern;
    while let Some(at) = rest.find("blk") {
        let candidate = &rest[at..];
        // `blk` + an optional escaping backslash + `.` + the layer number.
        let after_blk = candidate.strip_prefix("blk").unwrap_or(candidate);
        let after_slash = after_blk.strip_prefix('\\').unwrap_or(after_blk);
        if let Some(digits) = after_slash.strip_prefix('.') {
            let number: String = digits.chars().take_while(char::is_ascii_digit).collect();
            if !number.is_empty() {
                out.insert(format!("blk.{number}"));
            }
        }
        rest = &candidate[3..];
    }
    out
}

/// The bench's own RESULT line, out of everything the run printed.
fn bench_result(text: &str) -> Option<evalparse::Bench> {
    text.lines()
        .filter(|line| line.trim_start().starts_with("RESULT"))
        .map(evalparse::Bench::parse)
        .find(|b| b.decode_median.is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sweep_candidate_uses_mmap_and_the_overlay_does_not() {
        let measured = vec![Entry::new("n-cpu-moe", "35"), Entry::new("ctx-size", "249344")];
        // The preset locks its weights: the candidate file gets mmap...
        let for_file = candidate_overrides(true, &measured);
        assert_eq!(for_file.len(), measured.len() + 1);
        assert!(for_file.iter().any(|e| e.key == "load-mode" && e.value == "auto"));
        // ...and the list the overlay is built from is untouched, so the
        // adopted setup keeps the preset's `mlock`.
        assert_eq!(measured.len(), 2, "the caller's list is not modified");
        assert!(!measured.iter().any(|e| e.key == "load-mode"));

        // A preset that does not lock is measured exactly as it serves.
        assert_eq!(candidate_overrides(false, &measured), measured);
    }

    #[test]
    fn layer_names_reads_the_escaped_placement_pattern() {
        // What llama-fit-params actually prints, escaping the dots for its own
        // regex: the shell counts the unique `blk\.N` and so must this.
        let pattern = "blk\\.3\\.ffn_.*_exps.=CPU";
        assert_eq!(layer_names(pattern).len(), 1, "{:?}", layer_names(pattern));
        assert!(layer_names(pattern).contains("blk.3"));

        let many = "blk\\.0\\.ffn_.*_exps.=CPU,blk\\.1\\.ffn_.*_exps.=CPU,blk\\.2\\.ffn_.*_exps.=CPU";
        assert_eq!(layer_names(many).len(), 3);
        // A repeated layer counts once, as `sort -u | wc -l` does.
        let repeated = "blk\\.4\\.ffn_.*_exps.=CPU,blk\\.4\\.ffn_.*_exps.=CPU";
        assert_eq!(layer_names(repeated).len(), 1);
        // The unescaped spelling is read too, and a pattern with no layer at
        // all yields nothing - which is what makes the caller fall back.
        assert_eq!(layer_names("blk.7.ffn_.*_exps.=CPU").len(), 1);
        assert!(layer_names("nothing here").is_empty());
    }

    #[test]
    fn the_slug_is_the_one_the_recorder_writes() {
        // The reference box, exactly as scripts/tune.sh computes it.
        let machine = Machine {
            gpu: "NVIDIA GeForce RTX 3060".into(),
            vram_total_mib: 12288,
            vram_used_mib: 632,
            ram_total_gb: 32,
            ram_free_gb: 18.0,
            cores: 20,
            physical_cores: 12,
            cpu: "12th Gen Intel(R) Core(TM) i7-12700KF".into(),
        };
        assert_eq!(machine.host_slug(), "rtx-3060_12g_32g_i7-12700kf");
        assert_eq!(machine.wall_mib(), 12288 - 632 - 500);
    }

    #[test]
    fn the_slug_drops_the_marketing_words() {
        assert_eq!(slug("NVIDIA GeForce RTX 4090"), "rtx-4090");
        assert_eq!(slug("AMD Radeon RX 7900 XTX"), "rx-7900-xtx");
        assert_eq!(slug("13th Gen Intel(R) Core(TM) i9-13900K"), "i9-13900k");
        assert_eq!(slug("Apple M3 Max"), "apple-m3-max");
        assert_eq!(slug(""), "");
    }

    #[test]
    fn the_generation_number_goes_with_the_generation() {
        assert_eq!(strip_generation("12th gen intel"), " intel");
        assert_eq!(strip_generation("no generation here"), "no generation here");
        // Every one of them goes, as sed's /g does.
        assert_eq!(strip_generation("1th gen2th gen"), "");
        // The digits belong to the generation, not to whatever precedes it.
        assert_eq!(strip_generation("ryzen 9 12th gen"), "ryzen 9 ");
    }

    #[test]
    fn a_hybrid_cpu_probes_the_p_core_counts() {
        let tuner = machine_with(20, 12);
        assert_eq!(tuner.thread_candidates(), vec![8, 16, 20]);
        // A plain SMT box: physical, all-but-two, all.
        let tuner = machine_with(16, 8);
        assert_eq!(tuner.thread_candidates(), vec![8, 14, 16]);
        // No physical count to be had: half, all-but-two, all.
        let tuner = machine_with(8, 0);
        assert_eq!(tuner.thread_candidates(), vec![4, 6, 8]);
    }

    #[test]
    fn the_thread_probe_is_spent_only_near_the_floor() {
        // The knob is worth a model load when decode is close enough to the
        // floor that a few percent decides it, and not otherwise: the
        // reference box's 62 t/s against a 25 t/s floor is 2.5x clear.
        assert!(!thread_probe_worth_it(62.0, 25.0), "62 t/s is not near a 25 t/s floor");
        assert!(thread_probe_worth_it(30.0, 25.0), "30 t/s is");
        assert!(thread_probe_worth_it(25.0, 25.0), "and so is exactly the floor");
        assert!(thread_probe_worth_it(31.0, 25.0), "31 is inside the margin");
        assert!(!thread_probe_worth_it(31.5, 25.0), "31.5 is outside it");
    }

    /// A tuner with only what the thread sweep reads.
    fn machine_with(cores: u32, physical: u32) -> Tuner<'static> {
        tuner_at(Path::new("."), cores, physical)
    }

    fn tuner_at(home: &'static Path, cores: u32, physical: u32) -> Tuner<'static> {
        Tuner {
            home,
            opts: Options::default(),
            machine: Machine {
                gpu: String::new(),
                vram_total_mib: 12288,
                vram_used_mib: 0,
                ram_total_gb: 32,
                ram_free_gb: 0.0,
                cores,
                physical_cores: physical,
                cpu: String::new(),
            },
            preset_rel: "config/models.ini".into(),
            preset_text: String::new(),
            cand_dir: PathBuf::new(),
            out_dir: PathBuf::new(),
            model_file: String::new(),
            ctx_seed: 4096,
            ctx_ceiling: 262144,
            ubatch: String::new(),
            port: 8000,
            points: Vec::new(),
            out: RefCell::new(Box::new(std::io::sink())),
        }
    }

    #[test]
    fn an_engine_without_the_anthropic_api_cannot_win() {
        // ik_llama.cpp's adapter says so, and adopting it would leave the
        // default client unable to talk to the engine it adopted.
        let dir = std::env::temp_dir().join(format!("golem-tune-anthropic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, api) in [("llama.cpp", "true"), ("ik_llama.cpp", "false")] {
            std::fs::create_dir_all(dir.join("engines").join(name)).expect("mkdir");
            std::fs::write(
                dir.join("engines").join(name).join("engine.ini"),
                format!("; adapter\nname = {name}\ntune = true\napi_anthropic = {api}\n"),
            )
            .expect("write");
        }
        // An adapter that says nothing about the API, and one that is not
        // installed at all: silence is not a refusal.
        std::fs::create_dir_all(dir.join("engines/quiet")).expect("mkdir");
        std::fs::write(dir.join("engines/quiet/engine.ini"), "name = quiet\n").expect("write");

        let home: &'static Path = Box::leak(dir.clone().into_boxed_path());
        let tuner = tuner_at(home, 20, 12);
        assert!(tuner.serves_anthropic("llama.cpp"));
        assert!(!tuner.serves_anthropic("ik_llama.cpp"));
        assert!(tuner.serves_anthropic("quiet"));
        assert!(tuner.serves_anthropic("not-installed"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_bench_line_is_found_among_the_noise() {
        let text = "warmup...\nrun 1: prompt=115 tok\nRESULT decode_best=312.5 decode_median=310.2 prefill=18681 vram=1515\n";
        let bench = bench_result(text).expect("a bench line");
        assert_eq!(bench.decode_median, Some(310.2));
        assert_eq!(bench.prefill, Some(18681.0));
        assert_eq!(bench.vram_mib, Some(1515.0));
        // An eval run has RESULT lines but no decode: not a bench.
        assert!(bench_result("RESULT gen_tok_s_best=330.2\nRESULT long_ctx_100=PASS\n").is_none());
    }

    #[test]
    fn flags_are_read_and_env_supplies_the_defaults() {
        let dir = std::env::temp_dir().join(format!("golem-tune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join(".env"),
            "MODEL_ALIAS=\"the-default\"\nTUNE_MIN_DECODE=30\nTUNE_MIN_PREFILL=400\nTUNE_CTX_POLICY=65536\n",
        )
        .expect("write");

        let opts = Options::parse(&dir, &[]).expect("defaults");
        assert_eq!(opts.target, "the-default");
        assert_eq!(opts.floor.decode, 30.0);
        assert_eq!(opts.floor.prefill, 400.0);
        assert_eq!(opts.floor.depth_decode, 15.0, "the one .env did not move");
        assert_eq!(opts.ctx, CtxPolicy::Pinned(65536));
        assert!(opts.gate && opts.refine && !opts.dry_run);

        // The ladder is what `max` was before the prediction: still reachable,
        // from the file or from the flag, until a sweep compares the two.
        std::fs::write(dir.join(".env"), "TUNE_CTX_POLICY=ladder\n").expect("write");
        assert_eq!(Options::parse(&dir, &[]).expect("ladder").ctx, CtxPolicy::Ladder);
        let ladder: Vec<String> =
            ["smoke", "--ctx-ladder"].iter().map(|s| s.to_string()).collect();
        std::fs::write(dir.join(".env"), "TUNE_CTX_POLICY=max\n").expect("write");
        assert_eq!(Options::parse(&dir, &ladder).expect("flag").ctx, CtxPolicy::Ladder);
        // And the default is the prediction.
        assert_eq!(Options::parse(&dir, &[]).expect("max").ctx, CtxPolicy::Max);

        let args: Vec<String> = ["smoke", "--runs", "1", "--prefill", "0", "--no-gate", "--no-refine",
                                "--candidates", "2,4,6", "--engine", "llama.cpp", "--min-decode", "5",
                                "--ctx", "max", "--dry-run", "--adopt"]
            .iter().map(|s| s.to_string()).collect();
        let opts = Options::parse(&dir, &args).expect("flags");
        assert_eq!(opts.target, "smoke");
        assert_eq!(opts.runs, 1);
        assert_eq!(opts.prefill, 0);
        assert!(!opts.gate && !opts.refine && opts.dry_run && opts.adopt);
        assert_eq!(opts.candidates, vec![2, 4, 6]);
        assert_eq!(opts.engines, vec!["llama.cpp".to_string()]);
        assert_eq!(opts.floor.decode, 5.0, "a flag beats .env");
        assert_eq!(opts.ctx, CtxPolicy::Max);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_flag_without_its_value_is_an_error_not_a_default() {
        let dir = std::env::temp_dir().join(format!("golem-tune-bad-{}", std::process::id()));
        let args = vec!["--runs".to_string()];
        assert!(Options::parse(&dir, &args).is_err());
        let args = vec!["--nonsense".to_string()];
        assert!(Options::parse(&dir, &args).is_err());
        let args = vec!["one".to_string(), "two".to_string()];
        assert!(Options::parse(&dir, &args).is_err(), "one preset at a time");
        let args = vec!["--auto".to_string()];
        let err = Options::parse(&dir, &args).expect_err("--auto is not this sweep");
        assert!(err.contains("auto_tune"), "{err}");
    }
}
