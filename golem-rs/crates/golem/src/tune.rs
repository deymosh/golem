//! `golem tune <preset>`: the fastest setup this machine can serve, gated.
//!
//! **This file measures and reports; `golem_core::tune` decides.** Which
//! candidate wins, what the floors exclude, where the split frontier is and
//! how far the context grows are pure functions there; what is here is the
//! probing, the restarts, the benchmarks, the gate and the verdict - the
//! parts that touch a GPU and a process.
//!
//! The shape of a run, and what each step costs in model loads (each one a
//! full read of the model and a restart): the split frontier is bisected from
//! `llama-fit-params`' seed (three to five loads), the knobs that can still
//! matter get one probe each (`ub`, threads only near the floor, spec only
//! with an MTP head), the context ceiling is predicted from two measured
//! points and confirmed (two or three loads), and the winner is gated once
//! in its serving mode. Nothing that serves is touched until the gate
//! passes; the engine is put back the way the run found it.
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

use std::cell::{Cell, RefCell};
use std::fmt::Arguments;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

// `Verdict` here is this module's own (what the run settled on); the eval's
// per-candidate verdict is the gate's, and is named for it.
use golem_core::evalparse::{self, Verdict as Gate};
use golem_core::gguf;
use golem_core::ini::{self, Entry};
use golem_core::progress::Progress;
use golem_core::time;
use golem_core::tune::{self, ContextSearch, Exclusion, Floor, Phase, Point, Probe, Rung, RungOutcome};

use crate::config::{self, Overrides};
use crate::engine;
use crate::machine;
use crate::paths;
use crate::proc;
use crate::service;
use crate::state;

/// How long a restart may take before the candidate is written off.
///
/// 120 × 5 s. A 21 GB model read from disk is the slow part
/// and it is worth the patience; a setup that never answers is not.
const HEALTH_TRIES: u32 = 120;
const HEALTH_SLEEP: Duration = Duration::from_secs(5);

/// Where a candidate restart's launcher output goes, relative to the home.
///
/// The launcher's own words are the answer when a candidate will not start,
/// so the sweep shows them rather than a message of its own invention.
const RESTART_LOG: &str = "logs/tune-restart.log";

/// Where the gate's restart goes. Its own file, so a gate that fails does
/// not overwrite the sweep's last restart.
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
    /// MiB of the card left for the desktop to grow into (see
    /// [`Machine::wall_mib`]).
    pub headroom_mib: u64,
}

/// The default [`Options::headroom_mib`]. The desktop's own use drifts
/// between half a gigabyte and one and a half over a day (browsers, video),
/// and the driver spills silently once the card is full, so a setup tuned to
/// the last megabyte is one a browser tab can push over.
const VRAM_HEADROOM_MIB: u64 = 1024;

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
            headroom_mib: VRAM_HEADROOM_MIB,
        }
    }
}

impl Options {
    /// `golem.toml` first, for the floors, the context policy and the
    /// headroom; then the flags, which beat it (docs/tuning.md).
    pub fn parse(home: &Path, args: &[String]) -> Result<Options, String> {
        let mut opts = Options::default();
        if let Some(v) = config::get(home, "TUNE_MIN_DECODE").and_then(|v| v.parse().ok()) {
            opts.floor.decode = v;
        }
        if let Some(v) = config::get(home, "TUNE_MIN_PREFILL").and_then(|v| v.parse().ok()) {
            opts.floor.prefill = v;
        }
        if let Some(v) = config::get(home, "TUNE_MIN_DEPTH_DECODE")
            .and_then(|v| v.parse().ok())
        {
            opts.floor.depth_decode = v;
        }
        if let Some(v) = config::get(home, "TUNE_CTX_POLICY") {
            if v == "ladder" {
                opts.ctx = CtxPolicy::Ladder;
            } else if v != "max" {
                if let Ok(n) = v.parse() {
                    opts.ctx = CtxPolicy::Pinned(n);
                }
            }
        }
        if let Some(v) = config::get(home, "GATE_TOKENS").and_then(|v| v.parse().ok()) {
            opts.gate_tokens_max = v;
        }
        if let Some(v) = config::get(home, "TUNE_VRAM_HEADROOM_MIB").and_then(|v| v.parse().ok()) {
            opts.headroom_mib = v;
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
                // measured run has compared the two (docs/roadmap.md).
                "--ctx-ladder" => opts.ctx = CtxPolicy::Ladder,
                "--vram-headroom" => opts.headroom_mib = number(&value("--vram-headroom")?)? as u64,
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
            opts.target = config::get(home, "MODEL_ALIAS").unwrap_or_else(|| "local-model".into());
        }
        // `--prefill 0` measures no prefill, so there is none to hold to a
        // floor: every point would read 0 t/s and fail it.
        if opts.prefill == 0 {
            opts.floor.prefill = 0.0;
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
        let Some(gpu) = machine::gpu() else {
            return Err("auto-tune needs nvidia-smi (an NVIDIA card); for AMD set the split by hand".into());
        };
        Ok(Machine {
            gpu: gpu.name,
            vram_total_mib: gpu.total_mib,
            vram_used_mib: gpu.used_mib,
            ram_total_gb: machine::ram_total_gb(),
            ram_free_gb: machine::ram_free_gb().unwrap_or(0.0),
            cores: machine::logical_cores(),
            physical_cores: machine::physical_cores().unwrap_or(0),
            cpu: machine::cpu_name().unwrap_or_default(),
        })
    }

    /// The anonymous hardware slug the tuned setup is filed under - the same
    /// one every recorded row carries.
    pub fn host_slug(&self) -> String {
        machine::host_slug(&self.gpu, self.vram_total_mib, self.ram_total_gb, &self.cpu)
    }

    /// The highest whole-card VRAM reading a setup may have: the card, less
    /// the headroom the desktop is left to grow into.
    ///
    /// In the bench's own units. The bench reads `memory.used` for the whole
    /// card, so the desktop is already *in* every point's reading; a wall that
    /// subtracted the desktop again counted it twice, and on a box whose
    /// desktop holds 700 MiB it refused setups with a gigabyte still free.
    pub fn wall_mib(&self, headroom_mib: u64) -> u64 {
        self.vram_total_mib.saturating_sub(headroom_mib)
    }

    /// What the model itself may take: the card, less the desktop as it is
    /// now, less the headroom. Only the seed heuristic wants this - every
    /// measured point is judged against [`Machine::wall_mib`].
    fn model_budget_mib(&self, headroom_mib: u64) -> u64 {
        self.vram_total_mib.saturating_sub(self.vram_used_mib).saturating_sub(headroom_mib)
    }
}

/// Whether the thread probe is worth a load at this decode.
///
/// Pure, and separate from the probe itself, because the answer is the whole
/// reason the knob is not swept: see [`THREAD_PROBE_MARGIN`].
fn thread_probe_worth_it(decode: f64, floor: f64) -> bool {
    decode <= floor * THREAD_PROBE_MARGIN
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
    /// The `ubatch-size` it ran with, 0 for the engine's default.
    ubatch: u32,
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
            ubatch: self.ubatch,
            vram_mib: self.vram_mib,
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
    /// The preset file tuned from (relative to the home) and its text -
    /// every candidate and the overlay are written from this one reading, so
    /// a sweep cannot be tuned against a file that changed half-way through.
    /// Never the effective preset: see [`base_preset`].
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
    /// Whether the narration carries `PROGRESS` lines: when it lands in a
    /// job's log rather than on a terminal, where the narration is the
    /// progress.
    progress: bool,
    /// The stage the next load belongs to, the loads it has spent, and the
    /// loads the whole run has.
    phase: Cell<Phase>,
    in_phase: Cell<u32>,
    loads: Cell<u32>,
}

/// Run `golem tune`. Returns the process exit code.
///
/// The work itself is [`sweep`]'s; what is here is only the exit code: 2
/// for flags it could not read, 1 for a run that found nothing, 0 for a
/// verdict.
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
/// `golem tune` passes stdout as `out`; the `--auto` driver passes a file
/// and gets the same bytes in `data/tuned/auto-<preset>.log`.
pub fn sweep(home: &Path, args: &[String], out: Box<dyn Write>) -> Result<Verdict, String> {
    let opts = Options::parse(home, args)?;
    sweep_with(home, opts, out)
}

fn sweep_with(home: &Path, opts: Options, mut out: Box<dyn Write>) -> Result<Verdict, String> {
    if opts.dry_run {
        return Tuner::new(home, opts, out)?.sweep();
    }
    // The machine is probed with the engine stopped: an engine holding a
    // model would read as "the desktop" and shrink every wall by its size.
    // Whatever was serving is put back afterwards, on the configuration as it
    // then stands - the adopted winner after `--adopt`, the old setup
    // otherwise - so a tune never leaves a candidate serving.
    let was_up = engine::status(home).up;
    if was_up {
        let _ = writeln!(out, "[ .. ] stopping the engine so the card can be measured empty");
        service::request(home, "down", &Overrides::default(), &mut out)?;
    }
    // The put-back runs whatever the sweep did, a tuner that could not even
    // start (a missing model, an unreadable preset) included.
    let verdict = Tuner::new(home, opts, out).and_then(|mut tuner| tuner.sweep());
    let verb = if was_up { "restart" } else { "down" };
    let put_back = std::fs::File::options()
        .create(true)
        .append(true)
        .open(home.join(RESTART_LOG))
        .map_err(|e| format!("cannot open {RESTART_LOG}: {e}"))
        .and_then(|mut log| service::request(home, verb, &Overrides::default(), &mut log).map(|_| ()));
    if let Err(e) = put_back {
        eprintln!("[WARN] the engine was not put back ({verb}): {e}");
    }
    verdict
}

/// The preset a tune starts from: `MODELS_PRESET`, unless that is the
/// effective preset a previous tune wrote - then the repository's own. A tune
/// from the effective file would merge an overlay over an overlay, and the
/// candidates would inherit the last tune's answers as their starting point.
pub(crate) fn base_preset(home: &Path) -> String {
    match config::get(home, "MODELS_PRESET") {
        Some(preset) if !preset.is_empty() && preset != paths::EFFECTIVE_PRESET => preset,
        _ => paths::BASE_PRESET.to_string(),
    }
}

impl<'a> Tuner<'a> {
    fn new(
        home: &'a Path,
        opts: Options,
        out: Box<dyn Write>,
    ) -> Result<Tuner<'a>, String> {
        let machine = Machine::probe()?;
        let preset_rel = base_preset(home);
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
                "model file not found: {} (run golem pull {})",
                model_path.display(),
                opts.target
            ));
        }
        let facts = gguf::read(&model_path)
            .map_err(|e| format!("cannot read {}: {e}", model_path.display()))?;
        let out_dir = home.join("data/tuned").join(machine.host_slug());
        let port = engine::Endpoint::from_home(home).port;
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
            progress: !std::io::stdout().is_terminal(),
            phase: Cell::new(Phase::Frontier),
            in_phase: Cell::new(0),
            loads: Cell::new(0),
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

    /// The stage the loads from here on belong to.
    fn enter(&self, phase: Phase) {
        self.phase.set(phase);
        self.in_phase.set(0);
    }

    /// A model load is starting: count it, and say how far the run is
    /// against what it still expects to spend ([`tune::loads_left`]).
    fn load(&self) {
        let (phase, done) = (self.phase.get(), self.loads.get());
        if self.progress {
            let total = done + tune::loads_left(phase, self.in_phase.get());
            let label = format!("tune: {}, load {}", phase.name(), done + 1);
            self.emit(format_args!("{}", Progress::steps(done as u64, total as u64, label).line()));
        }
        self.loads.set(done + 1);
        self.in_phase.set(self.in_phase.get() + 1);
    }

    /// The whole run: plan, sweep, decide, gate, report.
    fn sweep(&mut self) -> Result<Verdict, String> {
        let engines = self.sweepable_engines()?;
        self.print_plan(&engines);
        if self.opts.dry_run {
            // A dry run still prints each engine's plan before it skips the
            // measuring: a plan that omits what it would spend is not the
            // plan, and what it spends is a search, so the plan says that.
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
        let free_gb = machine::ram_free_gb().unwrap_or(0.0);
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
        let text = paths::adapter(self.home, engine).ok()?;
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
            "             wall {} MiB whole-card ({} MiB left for the desktop to grow into);",
            m.wall_mib(self.opts.headroom_mib),
            self.opts.headroom_mib
        ));
        self.emit(format_args!("             a spill also shows up as a prefill collapse"));
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
        self.enter(Phase::Frontier);
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
    /// "Fit" is two things. The whole-card reading stays under the wall - the
    /// same wall the context search grows against, so a split chosen here is
    /// one the context can still be judged from. And prefill clears the
    /// floor, which is the project's rule for "usable at all" and the tell of
    /// a spill the reading alone can miss. A milder spill - prefill collapsed
    /// but still over the floor - is the ranking's business
    /// (`tune::exclusion` compares against the engine's best), not the
    /// search's.
    ///
    /// `None` is "no measurement at all" - the candidate would not start or
    /// the bench returned nothing - which for the search is the same answer as
    /// spilling: that split is not one to serve.
    fn measure_split(&mut self, engine: &str, n: u32) -> Option<bool> {
        let overrides = vec![Entry::new("n-cpu-moe", n.to_string()), Entry::new("fit", "off")];
        self.measure(engine, &format!("{engine}-n{n}"), &overrides)?;
        let point = self.points.last()?;
        let wall = self.machine.wall_mib(self.opts.headroom_mib) as f64;
        Some(point.prefill >= self.opts.floor.prefill && point.vram_mib <= wall)
    }

    /// The measured split under the wall with the best decode on `engine`,
    /// if any.
    fn best_measured(&self, engine: &str) -> Option<(u32, f64)> {
        let wall = self.machine.wall_mib(self.opts.headroom_mib) as f64;
        self.points
            .iter()
            .filter(|p| p.engine == engine && p.vram_mib <= wall)
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
        self.enter(Phase::Refine);
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
                config::get(self.home, "CPU_THREADS").and_then(|v| v.parse().ok()).unwrap_or(0);
            if let Some(t) = self.thread_candidates().first().copied().filter(|t| *t > 0) {
                if t != default {
                    let mut overrides = base();
                    overrides.push(Entry::new("threads", t.to_string()));
                    self.measure(engine, &format!("{engine}-t{t}"), &overrides);
                }
            }
        }

        // Speculative decoding is only worth a point when the file can do it,
        // and the point is the other side of what the preset says: a head
        // that accepts too little costs decode, and a file whose head the
        // preset never switched on may be leaving speed behind.
        if self.model_has_mtp() {
            match self.preset_value("spec-type").as_deref() {
                Some("draft-mtp") => {
                    let mut overrides = base();
                    overrides.push(Entry::new("spec-type", "none"));
                    self.measure(engine, &format!("{engine}-nospec"), &overrides);
                }
                None => {
                    let mut overrides = base();
                    overrides.push(Entry::new("spec-type", "draft-mtp"));
                    self.measure(engine, &format!("{engine}-mtp"), &overrides);
                }
                Some(_) => {}
            }
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
        // A CPU without SMT (logical = physical) has no P-core count to read
        // off, and falls through to the plain list.
        let p = cores.saturating_sub(phys);
        if phys > 0 && phys * 2 > cores && p > 0 {
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

    /// What the target serves with for `key`, a `[*]` default included: a
    /// `load-mode = mlock` set for every preset locks this one too.
    fn preset_value(&self, key: &str) -> Option<String> {
        ini::parse(&self.preset_text).effective(&self.opts.target, key).map(str::to_string)
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
        let budget = self.machine.model_budget_mib(self.opts.headroom_mib) as f64;
        (40.0 - 40.0 * budget / (model_gb * 1024.0)).max(0.0) as u32
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

        self.load();
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

        // The bench is this binary's own `bench`, run as a child with the
        // candidate named in its environment - the same road the gate takes -
        // so it records its own row, labelled with the candidate preset and
        // the note. The transcript stays beside the candidate as evidence.
        let out_file = self.cand_dir.join(format!("out-{}-{label}.txt", self.opts.target));
        let args = vec![
            "bench".to_string(),
            "--runs".to_string(),
            self.opts.runs.to_string(),
            "--prefill".to_string(),
            self.opts.prefill.to_string(),
        ];
        let status = proc::golem(self.home, &args)
            .and_then(|mut command| {
                command
                    .env("GOLEM_ENGINE", engine)
                    .env("GOLEM_MODEL_ALIAS", &self.opts.target)
                    .env("GOLEM_MODELS_PRESET", &candidate_rel)
                    .env("RECORD_NOTE", format!("tune {} on {engine}: {label}", self.opts.target));
                proc::run_to_file(command, &out_file)
            })
            .unwrap_or(-1);
        let text = std::fs::read_to_string(&out_file).unwrap_or_default();
        if status != 0 {
            self.emit(format_args!("  the bench exited {status}, skipping"));
            self.say_tail(&text, 6);
            return None;
        }
        let Some(bench) = bench_result(&text) else {
            // Say what the bench said. "no usable result" on its own is the
            // kind of message that costs an afternoon: the bench's own last
            // lines distinguish a server that is down from a bench that
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
        // The override when this point set one, else what the preset serves.
        let ubatch = overrides
            .iter()
            .rev()
            .find(|e| e.key == "ubatch-size")
            .map(|e| e.value.as_str())
            .unwrap_or(&self.ubatch)
            .parse()
            .unwrap_or(0);

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
            ubatch,
            overrides: overrides.to_vec(),
            n_cpu_moe,
            passed_gate: None,
            depth_decode: None,
        });
        Some(())
    }

    /// Restart the engine on `preset`, asked of the service with the
    /// candidate as the request's overrides. `lock` says whether this restart
    /// will pin the weights into RAM - the gate does, a sweep point does not -
    /// and only a locked load waits for the previous one's memory to come
    /// back.
    fn restart(&self, engine: &str, preset: &str, log: &str, lock: bool) -> std::io::Result<i32> {
        if lock {
            self.wait_for_ram();
        }
        let overrides = Overrides::default()
            .with("ENGINE", engine)
            .with("MODEL_ALIAS", &self.opts.target)
            .with("MODELS_PRESET", preset);
        let log_path = self.home.join(log);
        if let Some(dir) = log_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut out = std::fs::File::options().create(true).append(true).open(&log_path)?;
        Ok(service::request(self.home, "restart", &overrides, &mut out).unwrap_or(1))
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
            let free = machine::ram_free_gb().unwrap_or(f64::INFINITY);
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

    /// Why the engine's model failed to load, when it did: the router's own
    /// listing while it is still up, else the note the service's load guard
    /// left when it stopped it.
    fn load_failed(&self) -> Option<String> {
        let endpoint = engine::Endpoint { port: self.port, ..engine::Endpoint::from_home(self.home) };
        let failed = engine::failed_loads(&endpoint);
        if !failed.is_empty() {
            return Some(golem_core::router::stop_reason(&failed));
        }
        std::fs::read_to_string(paths::load_failure(self.home)).ok().map(|s| s.trim().to_string())
    }

    /// Wait for the engine to answer `/health` with a success - a loading
    /// engine answers 503, and benching it then measures the load.
    fn wait_healthy(&self) -> bool {
        // The home's key travels with the probe: an engine started with
        // `--api-key` answers a keyless request as unauthorised.
        let endpoint = engine::Endpoint { port: self.port, ..engine::Endpoint::from_home(self.home) };
        let deadline = Instant::now() + HEALTH_SLEEP * HEALTH_TRIES;
        while Instant::now() < deadline {
            if engine::healthy(&endpoint) {
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
    /// so candidates are gated one at a time in rank order until one
    /// passes. This is that loop; `winner` is the same rule stated as a
    /// function, for callers that already know every verdict.
    fn pick_winner(&mut self) -> Result<Verdict, String> {
        let floor = self.opts.floor;
        let wall = self.machine.wall_mib(self.opts.headroom_mib);
        let points: Vec<Point> = self.points.iter().map(Measured::point).collect();
        let ranked: Vec<usize> = tune::rank(&points, &floor, wall);
        self.emit(format_args!(""));
        self.emit(format_args!(
            "  ranked (best decode that fits and is not spilling; within {:.0}% of it, best prefill):",
            tune::DECODE_TIE * 100.0
        ));
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
            let Some(why) = tune::excluded(&points, i, &floor, wall) else { continue };
            let why = match why {
                Exclusion::OverWall { vram_mib, wall_mib } => {
                    format!("{vram_mib:.0} MiB is over the {wall_mib} MiB wall")
                }
                Exclusion::Spilling { prefill, engine_best } => {
                    format!("spilling, {prefill:.0} t/s prefill against {engine_best:.0}")
                }
                Exclusion::BelowDecodeFloor { decode } => {
                    format!("{decode:.1} t/s decode is under the {} floor", floor.decode)
                }
                Exclusion::BelowPrefillFloor { prefill } => {
                    format!("{prefill:.0} t/s prefill is under the {} floor", floor.prefill)
                }
            };
            self.emit(format_args!("  {} {}: excluded - {why}", point.engine, point.label));
        }
        if ranked.is_empty() {
            return Err(format!(
                "every candidate was excluded (over the {wall} MiB wall, spilling, or under the floors \
                 decode >= {} / prefill >= {} t/s) - lower them with --min-decode/--min-prefill",
                floor.decode, floor.prefill
            ));
        }

        // Two passes. The first holds every candidate to the context the gate
        // needs - a setup that cannot hold an agent's window is not one to
        // serve, however fast, and the next candidate may trade speed for
        // context (MTP's draft keeps a cache of its own). Only when none can
        // does the second pass gate the ones that fell short, largest context
        // first: a model this machine cannot serve at that size still gets
        // the most it can hold, and the run says so. A pinned context is the
        // caller's choice and is not held to this.
        let need = tune::required_ctx(self.opts.gate_tokens_max, self.ctx_ceiling);
        let mut short: Vec<usize> = Vec::new();
        for pass in 0..2 {
            let list: Vec<usize> = if pass == 0 {
                ranked.clone()
            } else {
                if short.is_empty() {
                    break;
                }
                short.sort_by(|&a, &b| self.points[b].ctx.cmp(&self.points[a].ctx));
                self.emit(format_args!(
                    "  [WARN] no setup holds {need} tokens on this machine - gating the largest context it reached instead"
                ));
                std::mem::take(&mut short)
            };
            for candidate in list {
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
                // The second pass holds points whose context was already grown.
                let key = if pass == 0 { self.maximize_ctx(candidate) } else { candidate };
                let ctx = self.points[key].ctx;
                let n_cpu_moe = self.points[key].n_cpu_moe.clone();

                if pass == 0 && self.opts.ctx == CtxPolicy::Max && ctx < need {
                    self.emit(format_args!(
                        "  [WARN] {engine} {} holds {ctx} tokens, under the {need} a {}-token gate needs - trying the next candidate",
                        self.points[key].label, self.opts.gate_tokens_max
                    ));
                    short.push(key);
                    continue;
                }

                let gate_tokens = (ctx * 9 / 10).min(self.opts.gate_tokens_max);
                let overlay = self.overlay_text(&engine, key);
                // Ungated is measured, not adopted: the setup is staged where the
                // gate would have served it from, and nothing that serves changes.
                // `tune --auto` sweeps every preset this way before it gates one.
                if !self.opts.gate {
                    let staged = self.stage(&engine, &overlay)?;
                    self.emit(format_args!("  [ .. ] not gated (--no-gate): staged in {staged}, nothing serves it"));
                    return Ok(self.verdict(key, &engine, n_cpu_moe, None));
                }
                let staged = self.stage(&engine, &overlay)?;

                self.enter(Phase::Gate);
                self.load();
                self.emit(format_args!("[ .. ] gating {engine} {}: restarting and running the {gate_tokens}-token needle (this takes a while)...", self.points[key].label));
                let restarted = matches!(self.restart(&engine, &staged, GATE_LOG, true), Ok(0));
                if !restarted || !self.wait_healthy() {
                    self.say_launcher_tail(GATE_LOG, 6);
                    // A load that died is the same stop as below: the next
                    // candidate would load the same way.
                    if let Some(why) = self.load_failed() {
                        return Err(format!(
                            "the engine did not come back for the gate ({why}); nothing adopted"
                        ));
                    }
                    self.emit(format_args!("  [WARN] the engine did not come back on the effective preset - trying the next candidate"));
                    continue;
                }
                let eval = self.gate(&engine, &staged, gate_tokens);
                // A gate whose model did not load says nothing about the
                // candidate's needle, and the next candidate loads the same way:
                // the sweep loaded every point with mmap, the gate with the
                // preset's own mode. Another load is another driver reset on
                // Windows, so the run stops here and says why.
                if let Some(why) = self.load_failed() {
                    return Err(format!(
                        "the model did not load in its serving configuration for the gate ({why}); \
                         every sweep point loaded with mmap, so the gate's load mode is the suspect - \
                         nothing adopted"
                    ));
                }
                let Some(eval) = eval else {
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
                        self.promote(&engine, &overlay)?;
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
    /// (docs/roadmap.md).
    fn maximize_ctx(&mut self, candidate: usize) -> usize {
        self.enter(Phase::Context);
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

        // The winner's own measurement is the line's first point: the search
        // holds it from the start (see `ContextSearch`).
        let seed = &self.points[candidate];
        let mut search = ContextSearch::new(
            seed.ctx,
            seed.vram_mib as u64,
            self.machine.wall_mib(self.opts.headroom_mib),
            self.ctx_ceiling,
        );
        let mut best = candidate;
        while let Rung::Grow(next) = search.next() {
            let mut with_ctx = overrides.clone();
            with_ctx.push(Entry::new("ctx-size", next.to_string()));
            let label = format!("{engine}-ctx{next}");
            if self.measure(&engine, &label, &with_ctx).is_none() {
                break;
            }
            let measured = self.points.len() - 1;
            let (vram, decode) = (self.points[measured].vram_mib, self.points[measured].decode);
            match search.record(next, vram as u64, decode < self.opts.floor.decode) {
                Probe::Fits => {
                    best = measured;
                    self.emit(format_args!(
                        "  context grows to {next} ({decode:.1} t/s decode, {vram:.0} MiB VRAM)"
                    ));
                }
                probe => self.emit(format_args!(
                    "  context {next} does not fit ({decode:.1} t/s decode, {vram:.0} MiB VRAM{})",
                    if matches!(probe, Probe::UnderFloor { .. }) { ", spilled" } else { ", over the wall" }
                )),
            }
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
    /// (docs/roadmap.md) - a tuner that finds a different
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
            let over_wall = self.points[measured].vram_mib > self.machine.wall_mib(self.opts.headroom_mib) as f64;
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
                        if self.points[at].vram_mib <= self.machine.wall_mib(self.opts.headroom_mib) as f64
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

    /// The winner's overlay: what it changes over the base preset, with the
    /// sweep that chose it as comments.
    fn overlay_text(&self, engine: &str, key: usize) -> String {
        let measured = &self.points[key];
        let sweep: String = self
            .points
            .iter()
            .filter(|p| p.engine == engine)
            .map(|p| format!("{}={:.1} ", p.label, p.decode))
            .collect();
        let mut overlay = String::new();
        overlay.push_str(&format!("; Auto-tuned by golem tune on {} for this machine.\n", time::now_iso()));
        overlay.push_str(&format!(
            "; Winner for engine {engine} ({}), merged over {}.\n",
            measured.label, self.preset_rel
        ));
        overlay.push_str(&format!("; Sweep (label=decode t/s): {sweep}\n"));
        overlay.push_str(&format!("[{}]\n", self.opts.target));
        // `fit = off` is in every candidate's overrides; it is written last
        // and once, because a duplicate is a line the engine reads twice.
        for entry in measured.overrides.iter().filter(|e| e.key != "fit") {
            overlay.push_str(&entry.line());
            overlay.push('\n');
        }
        overlay.push_str("fit = off\n");
        overlay
    }

    /// The effective preset with `overlay` standing in for this preset's:
    /// the base, with every gated overlay this machine has for `engine`
    /// merged in. An overlay for a section the base does not have is skipped
    /// - merged, it would be a section with no model to serve.
    fn effective_text(&self, engine: &str, overlay: &str) -> String {
        let base = ini::parse(&self.preset_text);
        let mut text = self.preset_text.clone();
        let own = format!("{}.ini", self.opts.target);
        let mut others: Vec<PathBuf> = std::fs::read_dir(self.out_dir.join(engine))
            .map(|entries| entries.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        others.sort();
        for path in others {
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let section = name.trim_end_matches(".ini");
            if !name.ends_with(".ini") || name == own || base.get(section).is_none() {
                continue;
            }
            if let Ok(other) = std::fs::read_to_string(&path) {
                text = ini::write_overlay(&text, &other);
            }
        }
        ini::write_overlay(&text, overlay)
    }

    /// The preset file the gate serves the winner from: a candidate, so
    /// nothing that serves is touched until the gate passes. Writing the
    /// effective preset first is what left a failed or interrupted tune's
    /// ungated winner as the machine's configuration.
    fn stage(&self, engine: &str, overlay: &str) -> Result<String, String> {
        let rel = format!("data/tuned/candidates/{}-gate.ini", self.opts.target);
        std::fs::create_dir_all(&self.cand_dir).map_err(|e| format!("cannot create {}: {e}", self.cand_dir.display()))?;
        std::fs::write(self.home.join(&rel), self.effective_text(engine, overlay))
            .map_err(|e| format!("cannot write {rel}: {e}"))?;
        Ok(rel)
    }

    /// Make the winner this machine's: its overlay under `data/tuned/<host>/`
    /// and the effective preset rebuilt from every overlay there.
    fn promote(&self, engine: &str, overlay: &str) -> Result<(), String> {
        let dir = self.out_dir.join(engine);
        std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let overlay_path = dir.join(format!("{}.ini", self.opts.target));
        let effective = self.effective_text(engine, overlay);
        std::fs::write(&overlay_path, overlay)
            .map_err(|e| format!("cannot write {}: {e}", overlay_path.display()))?;
        std::fs::write(self.home.join(paths::EFFECTIVE_PRESET), effective)
            .map_err(|e| format!("cannot write {}: {e}", paths::EFFECTIVE_PRESET))?;
        self.emit(format_args!(
            "[ OK ] overlay: {}",
            overlay_path.strip_prefix(self.home).unwrap_or(&overlay_path).display()
        ));
        Ok(())
    }

    /// `eval --long N` against the staged preset, read through the
    /// RESULT-line contract. A child, like the bench, so it records its own
    /// row under the preset it actually measured.
    fn gate(&self, engine: &str, preset: &str, tokens: u64) -> Option<evalparse::Eval> {
        let out_file = self.out_dir.join(format!("eval-{}.txt", self.opts.target));
        let args = vec!["eval".to_string(), "--long".to_string(), tokens.to_string()];
        let mut command = proc::golem(self.home, &args).ok()?;
        command
            .env("GOLEM_ENGINE", engine)
            .env("GOLEM_MODEL_ALIAS", &self.opts.target)
            .env("GOLEM_MODELS_PRESET", preset)
            .env("RECORD_NOTE", format!("tune {}: {tokens}-token needle gate", self.opts.target));
        proc::run_to_file(command, &out_file).ok()?;
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

        if !self.opts.gate {
            if self.opts.adopt {
                self.emit(format_args!("  [WARN] --adopt ignored: an ungated setup is never adopted"));
            }
        } else if self.opts.adopt {
            let config = self.home.join("golem.toml");
            if config.is_file() {
                std::fs::copy(&config, self.home.join("golem.toml.bak"))
                    .map_err(|e| format!("cannot back up golem.toml: {e}"))?;
            }
            for (key, value) in [("ENGINE", verdict.engine.as_str()), ("MODELS_PRESET", paths::EFFECTIVE_PRESET)] {
                config::set(self.home, key, value).map_err(|e| format!("cannot write golem.toml: {e}"))?;
            }
            self.emit(format_args!(
                "[ OK ] adopted in golem.toml (backup: golem.toml.bak): ENGINE = {}, MODELS_PRESET = {}",
                verdict.engine,
                paths::EFFECTIVE_PRESET
            ));
        } else {
            self.emit(format_args!(""));
            self.emit(format_args!("  to adopt, re-run with --adopt or set in golem.toml:"));
            self.emit(format_args!("    ENGINE = \"{}\"", verdict.engine));
            self.emit(format_args!("    MODELS_PRESET = \"{}\"", paths::EFFECTIVE_PRESET));
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
/// `blk\.3\.ffn_.*_exps.=CPU` - so a layer is `blk`, a backslash, a dot and
/// digits, and the answer is the count of unique ones. Splitting the pattern on non-alphanumeric characters instead finds no
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
        // regex: the unique `blk\.N` are the layers.
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
    fn the_slug_and_the_wall_are_the_reference_boxs() {
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
        // The wall is in the bench's own units - whole-card use, desktop
        // included - so the desktop is not subtracted a second time.
        assert_eq!(machine.wall_mib(1024), 12288 - 1024);
        assert_eq!(machine.model_budget_mib(1024), 12288 - 632 - 1024);
    }

    #[test]
    fn a_tune_starts_from_the_base_preset_never_from_its_own_output() {
        let dir = std::env::temp_dir().join(format!("golem-tune-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        assert_eq!(base_preset(&dir), "config/models.ini");
        std::fs::write(dir.join("golem.toml"), "MODELS_PRESET = \"data/tuned/models.effective.ini\"\n").expect("write");
        assert_eq!(base_preset(&dir), "config/models.ini");
        std::fs::write(dir.join("golem.toml"), "MODELS_PRESET = \"config/mine.ini\"\n").expect("write");
        assert_eq!(base_preset(&dir), "config/mine.ini");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_effective_preset_merges_every_gated_overlay_the_base_can_serve() {
        let dir = std::env::temp_dir().join(format!("golem-tune-effective-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let host = dir.join("data/tuned/host/llama.cpp");
        std::fs::create_dir_all(&host).expect("dir");
        // Another preset's earlier tune, and a stale overlay for a section
        // the base no longer has.
        std::fs::write(host.join("b.ini"), "[b]\nn-cpu-moe = 7\n").expect("write");
        std::fs::write(host.join("gone.ini"), "[gone]\nctx-size = 9\n").expect("write");
        std::fs::write(host.join("a.ini"), "[a]\nn-cpu-moe = 1\n").expect("write");
        let home: &'static Path = Box::leak(dir.clone().into_boxed_path());
        let mut tuner = tuner_at(home, 20, 12);
        tuner.opts.target = "a".into();
        tuner.out_dir = dir.join("data/tuned/host");
        tuner.preset_text = "[a]\nmodel = a.gguf\nn-cpu-moe = 30\n[b]\nmodel = b.gguf\n".into();
        let text = tuner.effective_text("llama.cpp", "[a]\nn-cpu-moe = 35\n");
        let doc = ini::parse(&text);
        assert_eq!(doc.value("a", "n-cpu-moe"), Some("35"), "this run's overlay, not the old one: {text}");
        assert_eq!(doc.value("b", "n-cpu-moe"), Some("7"), "the other preset keeps its tune: {text}");
        assert!(doc.get("gone").is_none(), "no section without a model: {text}");
        std::fs::remove_dir_all(&dir).ok();
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
            progress: false,
            phase: Cell::new(Phase::Frontier),
            in_phase: Cell::new(0),
            loads: Cell::new(0),
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
    fn flags_are_read_and_the_file_supplies_the_defaults() {
        let dir = std::env::temp_dir().join(format!("golem-tune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(
            dir.join("golem.toml"),
            "MODEL_ALIAS=\"the-default\"\nTUNE_MIN_DECODE=30\nTUNE_MIN_PREFILL=400\nTUNE_CTX_POLICY=65536\nTUNE_VRAM_HEADROOM_MIB=800\n",
        )
        .expect("write");

        let opts = Options::parse(&dir, &[]).expect("defaults");
        assert_eq!(opts.target, "the-default");
        assert_eq!(opts.floor.decode, 30.0);
        assert_eq!(opts.floor.prefill, 400.0);
        assert_eq!(opts.floor.depth_decode, 15.0, "the one the file did not set");
        assert_eq!(opts.ctx, CtxPolicy::Pinned(65536));
        assert_eq!(opts.headroom_mib, 800);
        assert!(opts.gate && opts.refine && !opts.dry_run);

        // The ladder is what `max` was before the prediction: still reachable,
        // from the file or from the flag, until a sweep compares the two.
        std::fs::write(dir.join("golem.toml"), "TUNE_CTX_POLICY = \"ladder\"\n").expect("write");
        assert_eq!(Options::parse(&dir, &[]).expect("ladder").ctx, CtxPolicy::Ladder);
        let ladder: Vec<String> =
            ["smoke", "--ctx-ladder"].iter().map(|s| s.to_string()).collect();
        std::fs::write(dir.join("golem.toml"), "TUNE_CTX_POLICY = \"max\"\n").expect("write");
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
        assert_eq!(opts.floor.decode, 5.0, "a flag beats the file");
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
