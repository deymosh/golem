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
use golem_core::tune::{self, Floor, Point, Rung, RungOutcome};

use crate::driver::{self, Driver};
use crate::engine;
use crate::state;

/// How long a restart may take before the candidate is written off.
///
/// The shell waits 120 × 5 s. A 21 GB model read from disk is the slow part
/// and it is worth the patience; a setup that never answers is not.
const HEALTH_TRIES: u32 = 120;
const HEALTH_SLEEP: Duration = Duration::from_secs(5);

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
    /// Grow until the card or the decode floor stops it (`TUNE_CTX_POLICY=max`).
    Max,
    /// Serve exactly this (`--ctx N`, or `TUNE_CTX_POLICY=<number>`).
    Pinned(u64),
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
            if v != "max" {
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
    driver: &'a Driver,
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
    /// The deploy mode this run is in, for the plan's `(windows mode)` tag:
    /// the front door's `--native`/`--docker`/`--windows` first, then `.env`.
    deploy_mode: String,
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
    let driver = Driver::find(home).ok_or_else(|| {
        format!("no launcher in {} - the tuner drives the shell launchers", home.display())
    })?;
    Tuner::new(home, &driver, opts, out)?.sweep()
}

impl<'a> Tuner<'a> {
    fn new(
        home: &'a Path,
        driver: &'a Driver,
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
            driver,
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
            deploy_mode: state::override_value(home, "GOLEM_DEPLOY_MODE", "DEPLOY_MODE")
                .filter(|mode| !mode.is_empty())
                .unwrap_or_else(|| "native".to_string()),
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
            // The shell echoes each engine's seed and candidate list even in a
            // dry run, and only then skips the measuring. A plan that omits the
            // points it would spend is not the plan.
            for engine in &engines {
                let (seed, candidates) = self.plan_for(engine);
                self.emit(format_args!(""));
                self.emit(format_args!(
                    "  --- {engine}: seed {seed}, offload candidates {}",
                    candidates.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
                ));
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
    /// An engine whose adapter says `tune = false` is installed but cannot be
    /// measured - Strata serves its own catalog model and ignores preset
    /// flags - so it is named and skipped rather than swept and reported as a
    /// mysterious zero.
    fn sweepable_engines(&self) -> Result<Vec<String>, String> {
        let installed = state::engines(self.home);
        if installed.is_empty() {
            return Err("no engines found under engines/".into());
        }
        let wanted = if self.opts.engines.is_empty() { installed.clone() } else { self.opts.engines.clone() };
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
                "maximize (ceiling: the model's {}-token trained ctx)",
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
        self.emit(format_args!(
            "  engines    {}  ({} mode)",
            engines.join(" "),
            self.deploy_mode
        ));
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
        if m.vram_used_mib > 2000 {
            self.emit(format_args!(
                "  [WARN] {} MiB of VRAM is already in use - the numbers will be noisy",
                m.vram_used_mib
            ));
        }
    }

    // ---- measuring -----------------------------------------------------

    /// The seed and the offload splits one engine's sweep will visit.
    ///
    /// Split out of [`Tuner::sweep_engine`] so the dry run can print the same
    /// line without measuring anything: the shell prints the candidate list in
    /// both cases (`if [[ "${DRY_RUN}" == 1 ]]; then continue; fi` comes *after*
    /// the echo), and a plan that hides the points it would spend is not the
    /// plan the shell showed.
    fn plan_for(&self, engine: &str) -> (u32, Vec<u32>) {
        let seed = self.seed_for(engine);
        let candidates: Vec<u32> = if self.opts.candidates.is_empty() {
            // Wider above the seed than below: the placement tool keeps ~1 GB
            // free, while a hand-tuned split ran four layers further (fitter
            // is faster, up to the cliff).
            (-2i64..=6)
                .map(|d| seed as i64 + d)
                .filter(|n| (0..=60).contains(n))
                .map(|n| n as u32)
                .collect()
        } else {
            self.opts.candidates.clone()
        };
        (seed, candidates)
    }

    /// Sweep one engine's offload splits, then refine its best.
    fn sweep_engine(&mut self, engine: &str) {
        let (seed, candidates) = self.plan_for(engine);
        self.emit(format_args!(""));
        self.emit(format_args!(
            "  --- {engine}: seed {seed}, offload candidates {}",
            candidates.iter().map(u32::to_string).collect::<Vec<_>>().join(", ")
        ));

        // The engine's own best split so far, which the refine points build
        // on: a split that measures badly is still a measurement, but it is
        // not the one to spend the batch/thread/spec points on.
        let mut best: Option<(u32, f64)> = None;
        for n in &candidates {
            let overrides = vec![Entry::new("n-cpu-moe", n.to_string()), Entry::new("fit", "off")];
            if self.measure(engine, &format!("{engine}-n{n}"), &overrides).is_none() {
                continue;
            }
            let decode = self.points.last().map(|p| p.decode).unwrap_or(0.0);
            if best.is_none_or(|(_, best_decode)| decode > best_decode) {
                best = Some((*n, decode));
            }
        }

        if self.opts.refine {
            if let Some((split, _)) = best {
                self.refine(engine, split);
            }
        }
    }

    /// The knobs the plan promised: prompt batch, threads, spec on/off.
    fn refine(&mut self, engine: &str, best: u32) {
        self.emit(format_args!("  --- {engine}: refining n-cpu-moe {best} (batch, threads, spec)"));
        let base = || vec![Entry::new("n-cpu-moe", best.to_string()), Entry::new("fit", "off")];

        if !self.ubatch.is_empty() && self.ubatch != "1024" {
            let mut overrides = base();
            overrides.push(Entry::new("ubatch-size", "1024"));
            self.measure(engine, &format!("{engine}-ub1024"), &overrides);
        }

        // Threads: probe the neighbourhoods the CPU topology makes promising
        // (measured on the reference box: 16 of 20 - the P-cores with their
        // hyperthreads - beats all 20 by ~8%, because E-cores add sync
        // overhead when decode streams RAM-resident experts).
        if self.machine.cores > 4 {
            let mut tried: Vec<u32> = Vec::new();
            for t in self.thread_candidates() {
                if tried.contains(&t) || t == 0 {
                    continue;
                }
                tried.push(t);
                let default = state::env_value(self.home, "CPU_THREADS")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                if t == default {
                    continue;
                }
                let mut overrides = base();
                overrides.push(Entry::new("threads", t.to_string()));
                self.measure(engine, &format!("{engine}-t{t}"), &overrides);
            }
        }

        // Speculative decoding is only worth a point when the file can do it.
        if self.preset_value("spec-type").as_deref() == Some("draft-mtp") && self.model_has_mtp() {
            let mut overrides = base();
            overrides.push(Entry::new("spec-type", "none"));
            self.measure(engine, &format!("{engine}-nospec"), &overrides);
        }
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

    /// A `scripts/*.sh` command, run the way the launchers run theirs.
    ///
    /// Through Git's bash - not a bare `bash`, which on Windows is the WSL
    /// launcher and would run the script inside a Linux VM that cannot see
    /// this engine - and with a *relative* path, because an absolute `C:\...`
    /// handed to MSYS bash is a path that does not exist.
    fn script(&self, rel: &str) -> Command {
        let mut command = Command::new(driver::bash());
        command.current_dir(self.home).arg(rel);
        command
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
        let text = ini::write_candidate(&self.preset_text, &self.opts.target, overrides);
        std::fs::write(&candidate, text).ok()?;

        self.emit(format_args!("  {engine} {label}: restarting..."));
        if !self.restart(engine, &candidate_rel, "logs/tune-restart.log") {
            self.emit(format_args!("  restart failed, skipping"));
            return None;
        }
        if !self.wait_healthy() {
            self.emit(format_args!("  never became healthy, skipping"));
            return None;
        }

        let note = format!("tune {} on {engine}: {label}", self.opts.target);
        let out_file = self.cand_dir.join(format!("out-{}-{label}.txt", self.opts.target));
        let mut command = self.script("scripts/benchmark-ai.sh");
        command
            .args(["--runs", &self.opts.runs.to_string(), "--prefill", &self.opts.prefill.to_string()])
            .env("GOLEM_MODEL_ALIAS", &self.opts.target)
            .env("RECORD_NOTE", &note);
        if driver::run_command(command, &out_file).is_err() {
            self.emit(format_args!("  cannot run the bench, skipping"));
            return None;
        }
        let text = std::fs::read_to_string(&out_file).unwrap_or_default();
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

    /// Restart the engine on `preset`, with the environment that points the
    /// launcher at it.
    ///
    /// `GOLEM_*` and not plain names: `./golem` re-sources `.env`, so
    /// `MODELS_PRESET` set here would be overwritten. That indirection is
    /// also the bug this port exists to kill - the eval harness used to lose
    /// exactly these variables on the way into WSL and gate the wrong model.
    fn restart(&self, engine: &str, preset: &str, log: &str) -> bool {
        let mut command = self.driver.command_with("restart", &[]);
        command
            .env("GOLEM_ENGINE", engine)
            .env("GOLEM_MODEL_ALIAS", &self.opts.target)
            .env("GOLEM_MODELS_PRESET", preset);
        driver::run_command(command, &self.home.join(log)).is_ok()
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
            if !self.restart(&engine, &overlay, "logs/tune-gate.log") || !self.wait_healthy() {
                self.emit(format_args!("  [WARN] the engine did not come back on the effective preset - trying the next candidate"));
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
    /// The ladder itself is `golem_core::tune::next_context` - doubling, then
    /// one midpoint probe after a failure - so what is here is only the
    /// measuring and the two stopping conditions: over the VRAM wall, or
    /// under the decode floor. Returns the index of the point to serve.
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

        let mut current = self.ctx_seed;
        let ceiling = self.ctx_ceiling.max(current);
        let mut best = candidate;
        let mut last = RungOutcome::Starting;
        loop {
            let next = match tune::next_context(current, ceiling, last.clone()) {
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
            let over_wall = self.points[measured].vram_mib > self.machine.wall_mib() as f64;
            let under_floor = self.points[measured].decode < self.opts.floor.decode;
            if over_wall || under_floor {
                // One midpoint between the last good context and this one,
                // then stop: finer steps are noise at this point.
                last = RungOutcome::Failed { attempt: next };
                if let Rung::Grow(mid) = tune::next_context(current, ceiling, last.clone()) {
                    let mut with_mid = overrides.clone();
                    with_mid.push(Entry::new("ctx-size", mid.to_string()));
                    let mid_label = format!("{engine}-ctx{mid}");
                    if self.measure(&engine, &mid_label, &with_mid).is_some() {
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
            self.emit(format_args!("[ .. ] context maximized: {engine} now serves {} tokens", self.points[best].ctx));
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

    /// `./golem eval --long N`, read through the RESULT-line contract.
    fn gate(&self, tokens: u64) -> Option<evalparse::Eval> {
        let out_file = self.out_dir.join(format!("eval-{}.txt", self.opts.target));
        let mut command = self.driver.command_with("eval", &["--long".to_string(), tokens.to_string()]);
        command.env("GOLEM_MODEL_ALIAS", &self.opts.target);
        driver::run_command(command, &out_file).ok()?;
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

    /// A tuner with only what the thread sweep reads.
    fn machine_with(cores: u32, physical: u32) -> Tuner<'static> {
        tuner_at(Path::new("."), cores, physical)
    }

    fn tuner_at(home: &'static Path, cores: u32, physical: u32) -> Tuner<'static> {
        Tuner {
            home,
            driver: Box::leak(Box::new(Driver {
                program: PathBuf::from("launcher"),
                prefix: Vec::new(),
                shell: driver::Shell::Bash,
            })),
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
            deploy_mode: "native".into(),
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
