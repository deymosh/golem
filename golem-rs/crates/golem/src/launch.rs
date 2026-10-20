//! `up` / `down` / `restart`: the engine, started and stopped by the binary.
//!
//! The port of `scripts/start-windows.ps1` / `stop-windows.ps1` and their
//! native cousins, and "the binary is the app". These launchers are the last
//! `.env` readers in the repository, and this is why they exist: to install a
//! pinned build, to start a server, to kill a process tree, and to wait for
//! the card. None of it needs PowerShell or bash.
//!
//! ## The rules that are easy to get wrong
//!
//! - **Kill the tree, not the process.** In router mode each model runs in a
//!   child `llama-server`; killing only the pid file's process orphans it with
//!   the whole model still in VRAM and RAM.
//! - **Stopping is not done when the kill is issued.** The driver reclaims the
//!   VRAM a moment later, and a start that begins in between fails with
//!   `cudaMalloc failed: out of memory` while reporting a perfectly healthy
//!   pid. "Stopped" has to mean the memory is free, so this waits for the card.
//! - **The server writes its own log, and its stdout and stderr are closed.**
//!   The launcher hands them to a file only for Strata, and the reason is not
//!   tidiness: a child holding our pipes open keeps anything reading our
//!   output (`./golem up | tail`) waiting for an EOF that never comes.
//! - **Health is a deadline *and* an early exit.** Ten minutes of polling is
//!   right for a 20 GB load and wrong for a server that died on its first
//!   flag: without the exit check, `up` would poll a corpse for ten minutes
//!   and then report a timeout instead of the reason.
//!
//! `up` is the binary's for Windows and native mode; docker stays with its
//! launcher (`docker compose up -d` is a different thing), and so does an
//! engine whose adapter says it installs itself some other way - Strata's
//! `launcher = strata` is a git clone and a Python setup, which is a port of
//! its own. Both fall back rather than fail: the port is per engine, not per
//! mode.

use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use golem_core::adapter;
use golem_core::ini;
use golem_core::record;

use crate::client;
use crate::engine;
use crate::pull;
use crate::release;
use crate::state;

/// How long the card may take to come back.
const CARD_SETTLE: Duration = Duration::from_secs(20);

/// What "the model is gone" looks like: a loaded 35B holds ~11 GB, and a
/// desktop holds under 2. Below this, nothing of ours is resident any more.
const CARD_IDLE_MIB: u64 = 2048;

/// How long the process may take to die.
const PROCESS_SETTLE: Duration = Duration::from_secs(20);

/// How long the model may take to load: the launcher's ten minutes, which is
/// a first boot reading 20 GB from a cold page cache and warming the GPU.
const LOAD_DEADLINE: Duration = Duration::from_secs(600);

/// How often `/health` is asked while it loads. The launcher's three
/// seconds, which is also about how long a loaded 35B needs to answer once.
const HEALTH_INTERVAL: Duration = Duration::from_secs(3);

/// How much of the server's own log is shown when it dies during startup.
/// The launcher's `Get-Content $Log -Tail 15`, and the reason it is shown at
/// all: a bad flag or a missing model says so in those lines.
const LOG_TAIL: usize = 15;

/// The model an unconfigured `MODEL_FILE` falls back to, as the launcher's is
/// the same file.
const DEFAULT_MODEL_FILE: &str = "Qwen3.6-35B-A3B-UD-Q4_K_M.gguf";

/// One `KEY` from the configuration, or the launcher's default when it is
/// unset or empty - the shell's `if ($Cfg["KEY"]) { ... } else { default }`,
/// which reads an empty assignment as unset.
fn setting(home: &Path, key: &str, default: &str) -> String {
    state::env_value(home, key).filter(|value| !value.is_empty()).unwrap_or_else(|| default.to_string())
}

/// `golem down`: stop the engine and wait for the memory.
pub fn down(home: &Path) -> i32 {
    let Some(pid) = engine::pid(home) else {
        println!("[INFO] no pid file - nothing to stop");
        return 0;
    };
    if !alive(pid) {
        // A pid file outliving its process is normal enough (a crash, a kill),
        // and the honest answer is to clean it up rather than report a stop
        // that did not happen.
        println!("[INFO] process {pid} is not running - clearing the pid file");
        remove_pid(home);
        return 0;
    }

    println!("[ .. ] stopping the engine (pid {pid})...");
    kill_tree(pid);

    let deadline = Instant::now() + PROCESS_SETTLE;
    while Instant::now() < deadline && alive(pid) {
        std::thread::sleep(Duration::from_millis(500));
    }
    remove_pid(home);

    if !wait_for_card() {
        println!(
            "[WARN] the GPU still reports {CARD_IDLE_MIB} MiB or more in use after {}s - \
             the next start may fail",
            CARD_SETTLE.as_secs()
        );
    }
    println!("[ OK ] engine (pid {pid}) stopped");
    0
}

/// What `up` did, or what it found that it cannot do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The engine is running - or, in a dry run, resolved and printable.
    Started,
    /// This engine installs itself with something the binary does not speak.
    /// The caller runs the shell launcher instead, which is what the shell
    /// does today: a port that cannot cover one engine must not break it.
    NeedsLauncher(String),
}

/// `golem up`: install the pinned build if it is missing, start the engine,
/// and wait until it answers.
///
/// `dry_run` is the launcher's `-DryRun`: resolve everything, install
/// nothing, start nothing. It comes *before* the port check, which is the
/// one place this deliberately differs from `start-windows.ps1` - there the
/// check is earlier, so a dry run fails while an engine is up. The dry run's
/// whole job is to say what would be started, and refusing to answer while
/// something is running is the opposite of useful.
pub fn up(home: &Path, dry_run: bool) -> Result<Outcome, String> {
    let mut plan = plan(home)?;

    // The launcher's line before it starts a single model rather than a
    // router. Printed in a dry run too, because `-DryRun` prints it: it comes
    // before the dry run's own exit in the script. Only the shape with a
    // preset gets it - with no preset there is no section to name.
    if plan.preset.is_some() && !plan.router {
        println!("[ .. ] {} has no router mode: serving only [{}]", plan.engine, plan.served);
    }

    if dry_run {
        if !plan.program.is_file() {
            println!(
                "[ .. ] would install {} {} ({}) into {}",
                plan.engine,
                plan.build,
                plan.backend,
                plan.install_target()
            );
        }
        for (key, value) in &plan.dropped {
            println!("[WARN] this engine has no equivalent of --{key} {value}, dropped");
        }
        println!("[ OK ] dry run: \"{}\" {}", plan.program.display(), plan.args.join(" "));
        println!("       (working directory: {})", plan.workdir.display());
        println!("       serving: {}  -> pid {}", plan.served, plan.pid_file.display());
        // The launcher's `-DryRun` downloads what is missing before it prints
        // this; a dry run that installed nothing must at least say what a real
        // one would fetch, or the report is a lie by omission.
        if let Ok(jobs) = model_jobs(home, &plan) {
            if !jobs.is_empty() {
                println!("[ .. ] would fetch {} model file(s) the preset needs", jobs.len());
            }
        }
        // A dry run still resolves a shape the binary hands back - the line
        // above is what the shell would run - but saying so here is what
        // keeps the report honest about who would actually start it.
        if let Some(reason) = &plan.via_launcher {
            println!("[ .. ] {reason}");
        }
        return Ok(Outcome::Started);
    }

    if let Some(reason) = &plan.via_launcher {
        return Ok(Outcome::NeedsLauncher(reason.clone()));
    }

    let endpoint = engine::Endpoint::from_home(home);
    // A connect, not a request: the launcher asks the OS for a listener
    // (`Get-NetTCPConnection -State Listen`), and a server that is up but
    // still loading answers nothing while it holds the socket.
    if listening(endpoint.port) {
        // The two launchers disagree here and the difference is visible:
        // start-local-ai.sh calls a healthy server "already running" and
        // exits 0, while start-windows.ps1 refuses outright. Keeping both
        // means `up` on a running engine stays a no-op on Linux and a
        // mistake on Windows, which is what it is today.
        if plan.mode != "windows" && healthy(&endpoint) {
            println!("[ OK ] Local AI server is already running");
            return Ok(Outcome::Started);
        }
        return Err(format!(
            "port {} is already in use. Stop the other server first: golem down",
            endpoint.port
        ));
    }

    if !plan.program.is_file() {
        let installed = release::ensure(home, &plan.pinned())?;
        // The plan resolved a directory from what the configuration asked
        // for, and `latest` is not a directory name - the tag is. `ensure`
        // is the only thing that knows it, so the plan is corrected here
        // rather than the server being started from a path that does not
        // exist. The banner then names the build that is actually running.
        plan.build = installed.tag;
        plan.program = installed.dir.join(&plan.server);
        plan.bin = installed.dir;
    }

    // After the engine is on disk and before anything is spawned, which is
    // where `start-windows.ps1` does it.
    ensure_models(home, &plan)?;

    start(home, &plan, &endpoint)
}

/// `golem restart`: down, then up.
///
/// `cmd_restart`'s two lines, and its `|| true` is the point: a restart is
/// asked for exactly when the engine may not be running, so a stop that
/// found nothing is not a failure to stop.
pub fn restart(home: &Path) -> Result<Outcome, String> {
    down(home);
    up(home, false)
}

/// The model files the preset needs and `data/models` lacks, in the order the
/// preset names them, with the launchers' warnings already said.
///
/// Pure: it reads the preset and decides, and fetches nothing. That is what
/// lets a dry run say what a real one would download - and what lets the rules
/// below be tested without a 20 GB file.
fn model_jobs(home: &Path, plan: &Startup) -> Result<Vec<(String, String)>, String> {
    let models = home.join("data/models");
    let Some(preset_rel) = &plan.preset else {
        // No preset: `.env` names the model, and `pull`'s no-argument list is
        // the one the launcher fetches there too (`HF_REPO` + `MODEL_FILE`,
        // plus the draft).
        return Ok(pull::default_jobs(home)
            .into_iter()
            .filter(|(_, file)| !models.join(file).is_file())
            .collect());
    };
    let path = home.join(preset_rel);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let preset = ini::parse(&text);

    // The alias names no section, so this preset is new to this machine and
    // there is nothing to be conservative about: the launcher fetches
    // everything it names.
    let all = preset.get(&plan.alias).is_none();

    let mut jobs: Vec<(String, String)> = Vec::new();
    for section in &preset.sections {
        for download in &section.downloads {
            if models.join(&download.file).is_file() {
                continue;
            }
            // A section that is not the default one is not fetched: a router
            // loads a model on request, and starting one model must not pull
            // another's 20 GB.
            if all || section.name == plan.alias {
                jobs.push((download.repo.clone(), download.file.clone()));
            } else {
                println!(
                    "[WARN] preset [{}] is missing {} - fetch it with: golem pull {}",
                    section.name, download.file, section.name
                );
            }
        }
    }

    // A section whose model no `; Download:` comment covers can never appear on
    // its own. That is fatal for the section being served - it is the failure
    // the user would hit at the first request - and a warning for the others,
    // because starting one model must not be blocked by another's state.
    let sourced: Vec<&str> = preset
        .sections
        .iter()
        .flat_map(|section| section.downloads.iter().map(|d| d.file.as_str()))
        .collect();
    let mut fatal: Vec<&str> = Vec::new();
    for section in &preset.sections {
        for key in ["model", "spec-draft-model"] {
            let Some(file) = section.value(key) else { continue };
            if models.join(file).is_file() || sourced.contains(&file) {
                continue;
            }
            if section.name == plan.alias {
                fatal.push(file);
            } else {
                println!(
                    "[WARN] preset [{}] has no Download source for {file} - it will fail if requested",
                    section.name
                );
            }
        }
    }
    if !fatal.is_empty() {
        return Err(format!(
            "Preset models without a \"; Download:\" source (add one per file in {preset_rel}): {}",
            fatal.join(", ")
        ));
    }
    Ok(jobs)
}

/// Fetch whatever the preset needs and `data/models` lacks, before anything
/// starts.
///
/// Both launchers do this before they launch (`cmd_up`'s `ensure_models`, and
/// the model section of `start-windows.ps1`), and the shape of the failure
/// without it is the reason: a router whose default section's model is missing
/// starts healthy and fails at the first request, which is the worst place to
/// find out. A fresh machine's first `up` is therefore a download and then a
/// working server, as the launchers promise.
fn ensure_models(home: &Path, plan: &Startup) -> Result<(), String> {
    let jobs = model_jobs(home, plan)?;
    if jobs.is_empty() {
        return Ok(());
    }
    println!("[ .. ] fetching {} model file(s) the preset needs", jobs.len());
    pull::fetch_all(home, &jobs)
}

/// Spawn the server, record its pid, and wait for it to answer.
fn start(home: &Path, plan: &Startup, endpoint: &engine::Endpoint) -> Result<Outcome, String> {
    let mut command = Command::new(&plan.program);
    command
        .args(&plan.args)
        .current_dir(&plan.workdir)
        // The server writes its own `--log-file`, so these are closed rather
        // than inherited: the child must not be able to write to a caller's
        // terminal, and on Unix this is also what keeps the child from holding
        // the caller's stdout pipe open.
        //
        // **On Windows it is not enough**, and this is measured rather than
        // assumed: `std` passes `bInheritHandles = TRUE` to `CreateProcess`
        // the moment one std handle is set, so the child inherits *every*
        // inheritable handle this process holds - the stdout pipe a caller
        // gave us included. Its own handles point at NUL, so nothing looks
        // wrong from the inside; from the outside `./golem up | tail` waits
        // for an EOF that never comes. A piped `up` that finished in twenty
        // seconds held the pipe for the engine's whole lifetime. Setting no
        // stdio at all is worse, not better (the child then gets our handles
        // outright, banner and all), and `cmd /c start` does not help either -
        // both were measured. Only a process created with a clean handle table
        // escapes it, which is what PowerShell's `Start-Process` is doing when
        // the launcher does not have the problem. `docs/plan-service.md` has
        // the fix: an engine owned by a long-lived service rather than by the
        // command that asked for it.
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A server sharing our console would take a Ctrl+C aimed at us, and the
    // launcher starts it hidden for the same reason (`-WindowStyle Hidden`,
    // which is a new console for a console application).
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", plan.program.display()))?;
    let pid = child.id();

    if let Some(dir) = plan.pid_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&plan.pid_file, format!("{pid}\n")) {
        // The server is up and the model is about to load; a pid file we
        // cannot write is worth saying and not worth killing that over.
        // `down` copes without one - it clears what it cannot use.
        eprintln!("[WARN] cannot write {}: {e}", plan.pid_file.display());
    }

    println!("[ OK ] {} {} started, PID {pid}", plan.engine, plan.build);
    println!("       logs: {}", plan.log.display());
    println!("[ .. ] waiting for the model to load (first boot includes GPU warmup)...");

    wait_healthy(plan, &mut child, endpoint)?;

    // The launcher's two closing lines, one per mode: a router loads its
    // models on demand, so what is healthy is the router and not a model.
    match &plan.preset {
        Some(preset) => println!(
            "[ OK ] router healthy on http://{}:{} (presets: {preset}; models load on first request)",
            plan.host, endpoint.port
        ),
        None => println!(
            "[ OK ] healthy on http://{}:{} (model: {})",
            plan.host, endpoint.port, plan.served
        ),
    }
    println!();
    print!(
        "{}",
        if plan.mode == "windows" {
            client::powershell_exports(home)
        } else {
            client::exports(home)
        }
    );
    Ok(Outcome::Started)
}

/// Poll `/health` until it answers, until the child exits, or until the
/// deadline passes.
fn wait_healthy(
    plan: &Startup,
    child: &mut Child,
    endpoint: &engine::Endpoint,
) -> Result<(), String> {
    let deadline = Instant::now() + LOAD_DEADLINE;
    loop {
        // Checked before the request, as the launcher's `$Proc.HasExited`
        // is: a server that died at once is already gone by the first poll,
        // and its exit code and log say more than a timeout would.
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("cannot watch the server: {e}"))?
        {
            let tail = tail(&plan.log, LOG_TAIL);
            if !tail.is_empty() {
                println!("{tail}");
            }
            return Err(format!(
                "{} server exited during startup ({})",
                plan.engine,
                code_text(status)
            ));
        }
        if healthy(endpoint) {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "server did not become healthy within {} minutes (logs: {})",
                LOAD_DEADLINE.as_secs() / 60,
                plan.log.display()
            ));
        }
        std::thread::sleep(HEALTH_INTERVAL);
    }
}

/// Whether the engine answers `/health` with a success.
fn healthy(endpoint: &engine::Endpoint) -> bool {
    engine::get(endpoint, "/health").is_some_and(|(status, _)| (200..300).contains(&status))
}

/// Whether anything at all is listening on `port`.
fn listening(port: u16) -> bool {
    let Ok(addrs) = format!("127.0.0.1:{port}").to_socket_addrs() else { return false };
    addrs.into_iter().next().is_some_and(|addr| {
        std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
    })
}

/// The last `lines` lines of a file, or nothing when it cannot be read.
fn tail(path: &Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(path) else { return String::new() };
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// An exit status as the launcher's message says it: a process killed by a
/// signal has no code, and `unwrap()` on `code()` would be the bug.
fn code_text(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "killed by a signal".to_string(),
    }
}

/// Whether `pid` is a running process.
///
/// `tasklist` with CSV output is the portable answer on Windows: a match is a
/// quoted row, and no match is an `INFO:` line (whose wording is localised, so
/// the test is on the shape, not the words). Elsewhere `kill -0` is exact.
pub fn alive(pid: u32) -> bool {
    // PID 0 is real (the System Idle Process) and is never ours: Windows'
    // `tasklist` answers a `PID eq 0` filter with a row, so without this a
    // stale `0` in a pid file would look like a running engine and send a
    // `taskkill` at the idle process.
    if pid == 0 {
        return false;
    }
    if cfg!(windows) {
        let Ok(out) = Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
        else {
            return false;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        text.lines().next().is_some_and(|line| line.trim_start().starts_with('"'))
    } else {
        Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
}

/// Kill the process and everything it started.
fn kill_tree(pid: u32) {
    if cfg!(windows) {
        // /T: in router mode each model is a child process, and /F because a
        // model mid-generation does not answer a polite request.
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
        return;
    }
    // The shell's order: the children first, because the parent exiting does
    // not take a loaded model with it, then the parent.
    let _ = Command::new("pkill").args(["-TERM", "-P", &pid.to_string()]).output();
    let _ = Command::new("kill").arg(pid.to_string()).output();
}

/// Remove the home's pid file, whichever spelling the mode uses.
fn remove_pid(home: &Path) {
    let Ok(entries) = std::fs::read_dir(home.join(".local")) else { return };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("llama-server") && name.ends_with(".pid") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Wait until the card is back to a desktop's worth of use. `false` when it
/// did not, within [`CARD_SETTLE`].
fn wait_for_card() -> bool {
    let deadline = Instant::now() + CARD_SETTLE;
    loop {
        match used_mib() {
            // No nvidia-smi (an AMD box, a container): nothing to wait for.
            None => return true,
            Some(used) if used < CARD_IDLE_MIB => return true,
            Some(_) => {}
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// VRAM in use, or `None` when there is no NVIDIA card to ask.
fn used_mib() -> Option<u64> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

/// Everything needed to start the engine, resolved from the home.
///
/// Pure: it reads files and decides, and spawns nothing. That seam is what
/// makes the port checkable without a GPU - the same home gives the same plan,
/// and the plan is the thing worth diffing against `start-windows.ps1 --dry-run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Startup {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub workdir: PathBuf,
    pub log: PathBuf,
    pub pid_file: PathBuf,
    /// Flags the adapter had no equivalent for, so the caller can say them.
    pub dropped: Vec<(String, String)>,
    /// What is being served, for the one line this prints after health.
    pub served: String,
    /// The engine, the build it was resolved from, and the backend that
    /// build is for: together they say which release is missing when the
    /// server is not on disk yet (see [`release::ensure`]).
    pub engine: String,
    pub build: String,
    pub backend: String,
    /// Where that build lives - `.local/engines/<engine>/<build>-<backend>`
    /// - and the executable's name inside it.
    pub bin: PathBuf,
    pub server: String,
    /// The address the server is told to bind and the mode it is being
    /// started in: the closing banner and the export block differ per mode,
    /// because the launcher's do.
    pub host: String,
    pub mode: String,
    /// The preset, in router mode: what the closing line names as the thing
    /// that was served. `None` means a single model, and `served` names it.
    pub preset: Option<String>,
    /// Whether `--models-preset` is on the command line. The three shapes are
    /// distinguishable only here and in `preset`: a router serves every
    /// section, a single-model engine serves one section of a preset, and
    /// with no preset at all `.env` is the configuration.
    pub router: bool,
    /// The default section's name. In router mode it is the one whose model
    /// must be on disk before the server starts; `served` cannot say it there,
    /// because a router serves all of them.
    pub alias: String,
    /// Set when this engine's adapter says something other than the binary
    /// installs it (`launcher = strata`): the reason, for a caller that
    /// hands the work back to the shell launcher.
    pub via_launcher: Option<String>,
}

impl Startup {
    /// What [`release::ensure`] needs, out of what this plan resolved.
    pub fn pinned(&self) -> release::Pinned {
        release::Pinned {
            engine: self.engine.clone(),
            build: self.build.clone(),
            backend: self.backend.clone(),
            // `bin` is `<root>/<build>-<backend>`, so its parent is the root:
            // the tag decides the leaf, and for `latest` only `ensure` knows it.
            root: self.bin.parent().map(Path::to_path_buf).unwrap_or_else(|| self.bin.clone()),
            server: self.server.clone(),
        }
    }

    /// Where the build would go, for the line a dry run prints before
    /// anything is installed.
    fn install_target(&self) -> String {
        if self.build == "latest" {
            format!("{} (as <tag>-{})", self.pinned().root.display(), self.backend)
        } else {
            self.bin.display().to_string()
        }
    }
}

/// Resolve the engine, its build, and the arguments it is started with.
///
/// The launcher's order, and each step has a reason: `ENGINE` picks the
/// adapter, `GPU_BACKEND` picks the build flavour (auto-detected from the
/// toolchain when unset), the build pin decides which directory under
/// `.local/engines/` holds the server, and the preset decides between router
/// mode (`--models-preset`, one model resident) and a single section's flags.
pub fn plan(home: &Path) -> Result<Startup, String> {
    // Three keys are read through `override_value`, not `env_value`, because
    // the launcher reads them that way and something depends on it: the tuner
    // points one run at one candidate by setting `GOLEM_ENGINE`,
    // `GOLEM_MODELS_PRESET` and `GOLEM_MODEL_ALIAS` in the environment of the
    // command that starts the engine. Dropping them is not a cosmetic
    // difference - a sweep would restart the same preset for every candidate
    // and the gate would score whatever `.env` names, which is how a 0.5B
    // candidate was once "gated" against the 35B coder's numbers.
    let engine = state::override_value(home, "GOLEM_ENGINE", "ENGINE")
        .unwrap_or_else(|| "llama.cpp".to_string());
    let adapter_path = home.join("engines").join(&engine).join("engine.ini");
    let adapter_text = std::fs::read_to_string(&adapter_path)
        .map_err(|e| format!("cannot read {}: {e}", adapter_path.display()))?;
    let adapter = ini::parse(&adapter_text);

    let backend = resolve_backend(home, adapter.global("backend").unwrap_or("cuda"));
    let build = engine_build(home, &engine, &adapter)?;
    let bin = home.join(".local").join("engines").join(&engine).join(format!("{build}-{backend}"));
    let server = adapter.global("server").map(str::to_string).unwrap_or_else(|| {
        if cfg!(windows) { "llama-server.exe".to_string() } else { "llama-server".to_string() }
    });

    let port = state::env_value(home, "LOCAL_AI_PORT").unwrap_or_else(|| "8000".to_string());
    let key = state::env_value(home, "LOCAL_AI_API_KEY").unwrap_or_else(|| "local-ai-key".to_string());
    let host = state::env_value(home, "LOCAL_AI_HOST").unwrap_or_else(|| "127.0.0.1".to_string());
    let alias = state::override_value(home, "GOLEM_MODEL_ALIAS", "MODEL_ALIAS")
        .unwrap_or_else(|| "local-model".to_string());
    let sleep_idle = state::env_value(home, "SLEEP_IDLE_SECONDS").unwrap_or_default();

    // The preset decides the mode, and the adapter says whether the engine can
    // read one at all: `router = true` means it understands `--models-preset`.
    let preset_rel = state::override_value(home, "GOLEM_MODELS_PRESET", "MODELS_PRESET")
        .unwrap_or_default();
    let preset_path = home.join(&preset_rel);
    let router = adapter.global("router").as_deref() != Some("false") && preset_path.is_file();

    let mut flags: Vec<(String, String)> = vec![
        ("host".to_string(), host.clone()),
        ("port".to_string(), port.clone()),
    ];
    // The launcher passes these two from `.env` on the command line, and both
    // are tuned parameters: `threads` is what the tune sweep varies, and
    // `parallel 1` is what keeps one model resident.
    for (key, name) in [("CPU_THREADS", "threads"), ("PARALLEL", "parallel")] {
        if let Some(value) = state::env_value(home, key).filter(|v| !v.is_empty()) {
            flags.push((name.to_string(), value));
        }
    }
    flags.push(("metrics".to_string(), "true".to_string()));
    flags.push(("api-key".to_string(), key.clone()));
    flags.push(("log-colors".to_string(), "off".to_string()));
    // The server writes its own log, and this is the file the console tails.
    let log = home.join("logs/llama-server-windows.log");
    flags.push(("log-file".to_string(), log.to_string_lossy().into_owned()));
    if !sleep_idle.is_empty() {
        flags.push(("sleep-idle-seconds".to_string(), sleep_idle));
    }

    let served;
    let workdir;
    if router {
        // Router mode: one model resident at a time, and the preset carries
        // everything else - including `jinja`, which is why the launcher does
        // not pass it here. Command-line flags would override the preset, so
        // only the shared ones above go on the line.
        flags.push(("models-preset".to_string(), preset_path.to_string_lossy().into_owned()));
        flags.push(("models-max".to_string(), "1".to_string()));
        served = format!("router ({})", preset_path.strip_prefix(home).unwrap_or(&preset_path).display());
        workdir = home.join("data/models");
    } else if preset_path.is_file() {
        let text = std::fs::read_to_string(&preset_path).unwrap_or_default();
        // The launcher refuses to start a server with no model on it, and
        // says which section it wanted: "Engine $EngineName has no router
        // mode and preset section [$Alias] (MODEL_ALIAS) is not in $Preset".
        if ini::parse(&text).get(&alias).is_none() {
            return Err(format!(
                "engine {engine} has no router mode and preset section [{alias}] \
                 (MODEL_ALIAS) is not in {}",
                preset_path.display()
            ));
        }
        // A single-model engine has no preset reader, so what the preset says
        // has to be on the command line - `jinja` included.
        flags.push(("jinja".to_string(), "true".to_string()));
        // The shell builds this as a *map* (`$Flags[$key] = $value`), so a key
        // the preset shares with the launcher's own flags is overwritten, not
        // doubled - `--jinja --jinja` is a duplicated flag on the command line.
        merge(&mut flags, record::preset_flags(&text, &alias));
        // `model` is deliberately not in that list: a recorded row's flags are
        // about the configuration, and the file is a separate column. A command
        // line needs it, and it is made absolute because the server starts in
        // `data/models` (a bare filename resolves from there, but an absolute
        // one cannot be broken by a later change of directory).
        if let Some(model) = ini::parse(&text).value(&alias, "model") {
            let path = home.join("data/models").join(model);
            merge(&mut flags, vec![("model".to_string(), path.to_string_lossy().into_owned())]);
        }
        served = alias.clone();
        workdir = home.join("data/models");
    } else {
        // No preset at all: `.env` is the configuration, which is the shape a
        // fresh clone has (`.env.example` ships `MODELS_PRESET` commented
        // out). This is the launcher's list in the launcher's order, because
        // order is behaviour - the last flag wins in llama.cpp.
        let model_dir = home.join("data/models");
        let model_file = setting(home, "MODEL_FILE", DEFAULT_MODEL_FILE);
        flags.push((
            "model".to_string(),
            model_dir.join(&model_file).to_string_lossy().into_owned(),
        ));
        flags.push(("alias".to_string(), alias.clone()));
        flags.push(("ctx-size".to_string(), setting(home, "CONTEXT_SIZE", "32768")));
        flags.push(("n-gpu-layers".to_string(), setting(home, "GPU_LAYERS", "99")));
        flags.push(("jinja".to_string(), "true".to_string()));
        flags.push(("flash-attn".to_string(), "on".to_string()));
        flags.push(("cache-type-k".to_string(), setting(home, "CACHE_TYPE_K", "q8_0")));
        flags.push(("cache-type-v".to_string(), setting(home, "CACHE_TYPE_V", "q8_0")));
        flags.push(("load-mode".to_string(), setting(home, "LOAD_MODE", "mlock")));
        // Speculative decoding: both keys are the launcher's, and an unset one
        // contributes nothing at all rather than an empty flag.
        if let Some(spec) = state::env_value(home, "SPEC_TYPE").filter(|v| !v.is_empty()) {
            flags.push(("spec-type".to_string(), spec));
        }
        if let Some(draft) = state::env_value(home, "DRAFT_FILE").filter(|v| !v.is_empty()) {
            flags.push((
                "spec-draft-model".to_string(),
                model_dir.join(&draft).to_string_lossy().into_owned(),
            ));
        }
        // The launcher names the file here and the alias when a preset is
        // what chose the model, so the closing banner says the same thing.
        served = model_file;
        workdir = model_dir;
    }

    // The engines the binary cannot install itself. Strata's adapter says
    // `launcher = strata`, and that launcher is a git clone, a Python venv
    // and its own `setup.py` - a port of its own, not a corner of this one.
    // Saying so is what keeps `up` from breaking an engine it does not
    // cover: the caller runs the shell launcher instead.
    let via_launcher = adapter
        .global("launcher")
        .filter(|launcher| !launcher.is_empty())
        .map(|launcher| {
            format!(
                "engine {engine} installs itself with the {launcher} launcher, which the binary \
                 does not speak yet - running the shell launcher for it"
            )
        });

    // `EXTRA_ARGS` are passed verbatim - they are already in the engine's own
    // syntax, which is why the adapter never translates them - and only when
    // there is no preset at all, as the launcher has it.
    let extra: Vec<String> = if preset_path.is_file() {
        Vec::new()
    } else {
        state::env_value(home, "EXTRA_ARGS")
            .filter(|value| !value.is_empty())
            .map(|value| value.split_whitespace().map(str::to_string).collect())
            .unwrap_or_default()
    };

    let mut line = adapter::translate(&flags, &adapter);
    line.args.extend(extra);
    Ok(Startup {
        program: bin.join(&server),
        args: line.args,
        workdir,
        log,
        pid_file: home.join(".local").join(pid_name()),
        dropped: line.dropped,
        served,
        engine,
        build,
        backend,
        bin,
        server,
        host,
        mode: state::deploy_mode(home),
        preset: router.then(|| preset_rel.clone()),
        router,
        alias,
        via_launcher,
    })
}

/// Merge `extra` into `into` the way the shell's `$Flags[$k] = $v` does: a key
/// already present keeps its position and takes the new value.
fn merge(into: &mut Vec<(String, String)>, extra: Vec<(String, String)>) {
    for (key, value) in extra {
        match into.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, existing)) => *existing = value,
            None => into.push((key, value)),
        }
    }
}

/// The pid file's name, as the launchers write it.
fn pid_name() -> &'static str {
    if cfg!(windows) { "llama-server-windows.pid" } else { "llama-server.pid" }
}

/// `GPU_BACKEND`, with `auto` resolved from the toolchain that is installed.
fn resolve_backend(home: &Path, fallback: &str) -> String {
    let configured = state::env_value(home, "GPU_BACKEND").unwrap_or_else(|| "auto".to_string());
    match configured.as_str() {
        "cuda" | "rocm" => configured,
        _ if which("nvidia-smi") => "cuda".to_string(),
        _ if which("rocminfo") => "rocm".to_string(),
        _ => fallback.to_string(),
    }
}

/// The build pin: `ENGINE_BUILD`, then llama.cpp's older `LLAMA_WIN_BUILD`
/// (which shadows the adapter's own `build` - one of the two goes when this is
/// the only launcher left), then the adapter.
fn engine_build(home: &Path, engine: &str, adapter: &ini::Document) -> Result<String, String> {
    if let Some(build) = state::env_value(home, "ENGINE_BUILD").filter(|b| !b.is_empty()) {
        return Ok(build);
    }
    if engine == "llama.cpp" {
        if let Some(build) = state::env_value(home, "LLAMA_WIN_BUILD").filter(|b| !b.is_empty()) {
            return Ok(build);
        }
    }
    adapter
        .global("build")
        .map(str::to_string)
        .ok_or_else(|| format!("no build pinned for engine {engine} (build in its engine.ini)"))
}

/// Whether a program is on PATH.
fn which(name: &str) -> bool {
    Command::new(if cfg!(windows) { "where" } else { "which" })
        .arg(name)
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-launch-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".local")).expect("temp");
        dir
    }

    #[test]
    fn a_home_with_no_pid_file_is_already_stopped() {
        let dir = home("none");
        assert_eq!(down(&dir), 0, "stopping nothing is not a failure");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_pid_file_that_outlived_its_process_is_cleared() {
        // A pid that cannot be running: 0 is not a process any OS hands out.
        let dir = home("stale");
        std::fs::write(dir.join(".local/llama-server-windows.pid"), "0\n").expect("write");
        assert_eq!(down(&dir), 0);
        assert!(
            !dir.join(".local/llama-server-windows.pid").exists(),
            "the stale pid file is gone"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn every_spelling_of_the_pid_file_is_removed() {
        // Windows mode writes llama-server-windows.pid, native writes
        // llama-server.pid; a home can end up with either.
        let dir = home("spellings");
        for name in ["llama-server.pid", "llama-server-windows.pid"] {
            std::fs::write(dir.join(".local").join(name), "0\n").expect("write");
        }
        std::fs::write(dir.join(".local/golem-web.pid"), "0\n").expect("write");
        remove_pid(&dir);
        assert!(!dir.join(".local/llama-server.pid").exists());
        assert!(!dir.join(".local/llama-server-windows.pid").exists());
        assert!(
            dir.join(".local/golem-web.pid").exists(),
            "the console's pid file is not ours to remove"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_pid_that_cannot_exist_is_not_alive() {
        assert!(!alive(0), "pid 0 is never a user process");
    }

    /// A home with one engine adapter, a preset, and a config, enough for
    /// `plan` to answer without touching the machine.
    fn engine_home(tag: &str, preset: &str, config: &str) -> PathBuf {
        let dir = home(tag);
        std::fs::create_dir_all(dir.join("engines/llama.cpp")).expect("engine");
        std::fs::create_dir_all(dir.join("config")).expect("config");
        std::fs::write(
            dir.join("engines/llama.cpp/engine.ini"),
            "name = llama.cpp\nbuild = b11379\nserver = llama-server.exe\nrouter = true\n\
             map.load-mode.mlock = --mlock\nflag.sleep-idle-seconds = -\n",
        )
        .expect("adapter");
        std::fs::write(dir.join("config/models.ini"), preset).expect("preset");
        std::fs::write(dir.join("golem.toml"), config).expect("config");
        dir
    }

    #[test]
    fn router_mode_serves_the_preset_and_one_model_at_a_time() {
        let dir = engine_home(
            "plan-router",
            "[*]\njinja = true\n[model-a]\nmodel = a.gguf\n",
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nLOCAL_AI_PORT = 8000\n\
             LOCAL_AI_API_KEY = \"k\"\nMODEL_ALIAS = \"model-a\"\n\
             MODELS_PRESET = \"config/models.ini\"\n",
        );
        let plan = plan(&dir).expect("plan");
        assert!(plan.program.ends_with("engines/llama.cpp/b11379-cuda/llama-server.exe"), "{:?}", plan.program);
        assert!(plan.args.iter().any(|a| a == "--models-preset"), "{:?}", plan.args);
        assert!(plan.args.iter().any(|a| a == "--models-max"), "one model resident at a time");
        assert!(plan.args.contains(&"--port".to_string()), "{:?}", plan.args);
        assert!(plan.args.contains(&"8000".to_string()), "{:?}", plan.args);
        assert!(plan.args.contains(&"--api-key".to_string()));
        assert_eq!(plan.served, "router (config/models.ini)");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_single_model_engine_gets_the_sections_flags_and_a_working_directory() {
        // `router = false` adapters cannot read a preset: the section's flags
        // become the command line, and the model path is made absolute because
        // the server is started in data/models.
        let dir = engine_home(
            "plan-single",
            "[*]\njinja = true\nload-mode = mlock\n[model-a]\nmodel = a.gguf\nctx-size = 4096\n",
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nMODEL_ALIAS = \"model-a\"\n\
             MODELS_PRESET = \"config/models.ini\"\n",
        );
        std::fs::write(
            dir.join("engines/llama.cpp/engine.ini"),
            "name = llama.cpp\nbuild = b11379\nserver = llama-server.exe\nrouter = false\n\
             map.load-mode.mlock = --mlock\n",
        )
        .expect("adapter");
        let plan = plan(&dir).expect("plan");
        assert!(!plan.args.iter().any(|a| a == "--models-preset"), "{:?}", plan.args);
        assert!(plan.args.contains(&"--ctx-size".to_string()));
        assert!(plan.args.contains(&"4096".to_string()));
        // `load-mode = mlock` went through the adapter's map, not as itself.
        assert!(plan.args.contains(&"--mlock".to_string()), "{:?}", plan.args);
        assert!(!plan.args.contains(&"--load-mode".to_string()));
        // The model path is absolute, so `a.gguf` resolves from any cwd.
        assert!(
            plan.args.iter().any(|a| Path::new(a).is_absolute()
                && Path::new(a).file_name().is_some_and(|name| name == "a.gguf")),
            "{:?}",
            plan.args
        );
        assert_eq!(plan.workdir, dir.join("data/models"));
        assert_eq!(plan.served, "model-a");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_build_pin_falls_through_in_the_launchers_order() {
        let dir = engine_home("plan-build", "", "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\n");
        // The adapter's build, then llama.cpp's older `.env` name overrides it.
        assert!(plan(&dir).expect("plan").program.ends_with("b11379-cuda/llama-server.exe"));
        std::fs::write(
            dir.join("golem.toml"),
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nLLAMA_WIN_BUILD = \"b11755\"\n",
        )
        .expect("config");
        assert!(plan(&dir).expect("plan").program.ends_with("b11755-cuda/llama-server.exe"));
        // And ENGINE_BUILD beats both.
        std::fs::write(
            dir.join("golem.toml"),
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\n\
             LLAMA_WIN_BUILD = \"b11755\"\nENGINE_BUILD = \"b9\"\n",
        )
        .expect("config");
        assert!(plan(&dir).expect("plan").program.ends_with("b9-cuda/llama-server.exe"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_engine_is_a_message_not_a_panic() {
        let dir = home("plan-unknown");
        std::fs::write(dir.join("golem.toml"), "ENGINE = \"nope\"\n").expect("config");
        let err = plan(&dir).expect_err("an engine with no adapter");
        assert!(err.contains("nope"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------------------------------------------------------
    // `up`
    // ---------------------------------------------------------------

    /// A home that resolves, on a port nothing holds: `LOCAL_AI_PORT = 1` is
    /// below the range anything binds, and a test that picked a plausible
    /// port would fail on a machine where the engine happens to be running.
    fn up_home(tag: &str, mode: &str, port: &str) -> PathBuf {
        engine_home(
            tag,
            "[*]\njinja = true\n[model-a]\nmodel = a.gguf\n",
            &format!(
                "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nDEPLOY_MODE = \"{mode}\"\n\
                 LOCAL_AI_PORT = \"{port}\"\nLOCAL_AI_API_KEY = \"k\"\n\
                 MODEL_ALIAS = \"model-a\"\nMODELS_PRESET = \"config/models.ini\"\n"
            ),
        )
    }

    /// A one-line HTTP server on an ephemeral port, answering every request
    /// with `200`. Real sockets rather than a mock, because the two things
    /// under test - is anything listening, and does `/health` answer - are
    /// exactly the socket behaviours.
    fn health_server() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut buf = [0u8; 1024];
                let _ = std::io::Read::read(&mut stream, &mut buf);
                let _ = std::io::Write::write_all(
                    &mut stream,
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}",
                );
            }
        });
        port
    }

    #[test]
    fn nothing_listening_and_something_listening_are_told_apart() {
        let port = health_server();
        assert!(listening(port), "the server above is listening on {port}");
        // Port 1: reserved, and nothing may bind it without privilege.
        assert!(!listening(1));
        let endpoint = engine::Endpoint { port, api_key: String::new() };
        assert!(healthy(&endpoint));
        assert!(!healthy(&engine::Endpoint { port: 1, api_key: String::new() }));
    }

    #[test]
    fn a_port_someone_already_holds_refuses_on_windows_and_is_a_no_op_when_healthy() {
        // The two launchers disagree and the difference is visible:
        // start-windows.ps1 throws on any listener, start-local-ai.sh calls a
        // healthy server "already running" and exits 0.
        let port = health_server().to_string();
        let windows = up_home("up-busy-windows", "windows", &port);
        let err = up(&windows, false).expect_err("the port is held");
        assert!(err.contains("already in use"), "{err}");

        let native = up_home("up-busy-native", "native", &port);
        assert_eq!(up(&native, false), Ok(Outcome::Started));

        std::fs::remove_dir_all(&windows).ok();
        std::fs::remove_dir_all(&native).ok();
    }

    #[test]
    fn a_dry_run_resolves_the_whole_line_and_starts_nothing() {
        // The check a port of `up` needs: the command line is the thing worth
        // diffing against `start-windows.ps1 -DryRun`, not the banner.
        let dir = up_home("up-dry", "windows", "1");
        assert_eq!(up(&dir, true), Ok(Outcome::Started));
        // Nothing was installed, nothing was started, no pid was written.
        assert!(!dir.join(".local/llama-server-windows.pid").exists());
        assert!(!dir.join(".local/engines/llama.cpp/b11379-cuda").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_engine_that_installs_itself_falls_back_to_the_launcher() {
        // Strata's adapter says `launcher = strata`: a git clone and a Python
        // setup, which is a port of its own. The fallback is per engine, not
        // per mode, and it is what keeps `up` from breaking an engine it does
        // not cover.
        let dir = up_home("up-strata", "windows", "1");
        std::fs::write(
            dir.join("engines/llama.cpp/engine.ini"),
            "name = llama.cpp\nbuild = b11379\nserver = llama-server.exe\nrouter = true\n\
             launcher = strata\n",
        )
        .expect("adapter");
        match up(&dir, false) {
            Ok(Outcome::NeedsLauncher(why)) => assert!(why.contains("strata"), "{why}"),
            other => panic!("expected the launcher fallback, got {other:?}"),
        }
        // A dry run still resolves it: it reports, it does not decide who runs.
        assert_eq!(up(&dir, true), Ok(Outcome::Started));
        std::fs::remove_dir_all(&dir).ok();
    }

    // The per-run overrides (`GOLEM_ENGINE`, `GOLEM_MODELS_PRESET`,
    // `GOLEM_MODEL_ALIAS`) live in the process environment, which every test
    // in this binary shares: setting one here would change what the tests
    // above see, and did. They are tested in `tests/launch_plan.rs`, which
    // runs the real binary in its own process - which is also how the tuner
    // uses them.

    #[test]
    fn the_plan_says_which_release_it_needs() {
        let dir = up_home("plan-pinned", "windows", "1");
        let pinned = plan(&dir).expect("plan").pinned();
        assert_eq!(pinned.engine, "llama.cpp");
        assert_eq!(pinned.build, "b11379");
        assert_eq!(pinned.backend, "cuda");
        // The root, not the directory: `latest` is only a tag once GitHub has
        // been asked, so the leaf is named inside `release::ensure`.
        assert_eq!(pinned.root, dir.join(".local/engines/llama.cpp"));
        assert_eq!(pinned.server, "llama-server.exe");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_plan_carries_the_mode_the_preset_and_the_host_the_banner_needs() {
        let dir = up_home("plan-banner", "windows", "1");
        let plan = plan(&dir).expect("plan");
        assert_eq!(plan.mode, "windows");
        assert_eq!(plan.host, "127.0.0.1");
        assert_eq!(plan.preset.as_deref(), Some("config/models.ini"));
        assert_eq!(plan.via_launcher, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------------------------------------------------------
    // The model fetch `up` owes the launchers
    // ---------------------------------------------------------------

    /// A home with this preset, and these files already in `data/models`.
    fn preset_home(tag: &str, alias: &str, preset: &str, files: &[&str]) -> PathBuf {
        let dir = engine_home(
            tag,
            preset,
            &format!(
                "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nDEPLOY_MODE = \"windows\"\n\
                 LOCAL_AI_PORT = \"1\"\nMODEL_ALIAS = \"{alias}\"\n\
                 MODELS_PRESET = \"config/models.ini\"\n"
            ),
        );
        std::fs::create_dir_all(dir.join("data/models")).expect("models");
        for file in files {
            std::fs::write(dir.join("data/models").join(file), "x").expect("model");
        }
        dir
    }

    const TWO_SECTIONS: &str = "; Download: org/a -> a.gguf\n[model-a]\nmodel = a.gguf\n\n\
                                ; Download: org/b -> b.gguf\n[model-b]\nmodel = b.gguf\n";

    #[test]
    fn the_served_sections_missing_model_is_the_one_that_is_fetched() {
        // The rule that keeps a first start from pulling 20 GB nobody asked
        // for: the section being served is fetched, the others wait.
        let dir = preset_home("jobs-served", "model-a", TWO_SECTIONS, &[]);
        let jobs = model_jobs(&dir, &plan(&dir).expect("plan")).expect("jobs");
        assert_eq!(jobs, vec![("org/a".to_string(), "a.gguf".to_string())]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_model_already_on_disk_is_not_a_job() {
        let dir = preset_home("jobs-present", "model-a", TWO_SECTIONS, &["a.gguf"]);
        let jobs = model_jobs(&dir, &plan(&dir).expect("plan")).expect("jobs");
        assert!(jobs.is_empty(), "{jobs:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_alias_that_names_no_section_fetches_everything() {
        // A preset this machine has not seen: the alias names no section, so
        // there is no "the one being served" to be conservative about.
        let dir = preset_home("jobs-new", "not-a-section", TWO_SECTIONS, &[]);
        let jobs = model_jobs(&dir, &plan(&dir).expect("plan")).expect("jobs");
        assert_eq!(
            jobs,
            vec![
                ("org/a".to_string(), "a.gguf".to_string()),
                ("org/b".to_string(), "b.gguf".to_string())
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_model_with_no_download_source_is_fatal_for_the_section_being_served() {
        // It can never appear on its own, and the failure without this check
        // is at the first request rather than at the start.
        let dir = preset_home("jobs-nosource", "model-a", "[model-a]\nmodel = a.gguf\n", &[]);
        let err = model_jobs(&dir, &plan(&dir).expect("plan")).expect_err("no source");
        assert_eq!(
            err,
            "Preset models without a \"; Download:\" source (add one per file in \
             config/models.ini): a.gguf"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_model_with_no_download_source_is_only_a_warning_for_another_section() {
        // Starting one model must not be blocked by another section's state.
        let dir = preset_home(
            "jobs-nosource-other",
            "model-a",
            "; Download: org/a -> a.gguf\n[model-a]\nmodel = a.gguf\n\n[model-b]\nmodel = b.gguf\n",
            &[],
        );
        let jobs = model_jobs(&dir, &plan(&dir).expect("plan")).expect("jobs");
        assert_eq!(jobs, vec![("org/a".to_string(), "a.gguf".to_string())]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_draft_model_counts_as_a_model_the_section_needs() {
        // The launcher scans `model` and `spec-draft-model` alike: a missing
        // draft fails the load just as a missing model does.
        let dir = preset_home(
            "jobs-draft",
            "model-a",
            "; Download: org/a -> a.gguf\n[model-a]\nmodel = a.gguf\nspec-draft-model = d.gguf\n",
            &[],
        );
        let err = model_jobs(&dir, &plan(&dir).expect("plan")).expect_err("no source");
        assert!(err.ends_with("d.gguf"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_a_preset_the_command_line_is_the_env_flag_set() {
        // The shape a fresh clone has. `.env` is the configuration here, and
        // the list is the launcher's, in the launcher's order - order is
        // behaviour, because the last flag wins.
        let dir = engine_home(
            "plan-env-flags",
            "[model-a]\nmodel = a.gguf\n",
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nMODEL_FILE = \"m.gguf\"\n\
             CONTEXT_SIZE = \"4096\"\nGPU_LAYERS = \"12\"\nCACHE_TYPE_K = \"q4_0\"\n\
             LOAD_MODE = \"mmap\"\nSPEC_TYPE = \"draft-mtp\"\nDRAFT_FILE = \"d.gguf\"\n\
             EXTRA_ARGS = \"--rope-freq-base 10000 --no-warmup\"\n",
        );
        let plan = plan(&dir).expect("plan");
        let models = dir.join("data/models");
        let model = models.join("m.gguf").to_string_lossy().into_owned();
        let draft = models.join("d.gguf").to_string_lossy().into_owned();

        assert!(plan.via_launcher.is_none(), "no preset is the binary's shape now");
        assert!(!plan.router);
        assert_eq!(plan.served, "m.gguf", "the launcher names the file, not the alias");
        // Everything from `--model` on: the shared flags come first and are
        // covered by the router tests.
        let at = plan.args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(
            plan.args[at..],
            [
                "--model", &model, "--alias", "local-model", "--ctx-size", "4096",
                "--n-gpu-layers", "12", "--jinja", "--flash-attn", "on",
                "--cache-type-k", "q4_0", "--cache-type-v", "q8_0", "--load-mode", "mmap",
                "--spec-type", "draft-mtp", "--spec-draft-model", &draft,
                // EXTRA_ARGS are verbatim, and last: the adapter never
                // translates them, and the launcher appends them as given.
                "--rope-freq-base", "10000", "--no-warmup",
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unset_spec_or_draft_contributes_nothing_at_all() {
        // Not an empty flag and not a default: llama.cpp would take
        // `--spec-type ""` as a value.
        let dir = engine_home(
            "plan-env-bare",
            "",
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nMODEL_FILE = \"m.gguf\"\n",
        );
        let plan = plan(&dir).expect("plan");
        assert!(!plan.args.iter().any(|a| a == "--spec-type"), "{:?}", plan.args);
        assert!(!plan.args.iter().any(|a| a == "--spec-draft-model"), "{:?}", plan.args);
        // The launcher's defaults, which are not the code's own.
        for (key, value) in [("--ctx-size", "32768"), ("--n-gpu-layers", "99")] {
            let at = plan.args.iter().position(|a| a == key).unwrap_or_else(|| panic!("{key}"));
            assert_eq!(plan.args[at + 1], value, "{key}");
        }
        // `load-mode`'s default goes through the adapter like any other flag:
        // this fixture renames `mlock` to a bare `--mlock`, which is exactly
        // what the map keys are for.
        assert!(plan.args.contains(&"--mlock".to_string()), "{:?}", plan.args);
        assert!(!plan.args.contains(&"--load-mode".to_string()), "{:?}", plan.args);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn with_no_preset_the_job_is_the_env_models_own() {
        // `.env` names the model in this shape, so `pull`'s no-argument list
        // is what the launcher fetches - not a preset section's.
        let dir = engine_home(
            "jobs-none",
            "[model-a]\nmodel = a.gguf\n",
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nMODEL_FILE = \"only.gguf\"\n",
        );
        let resolved = plan(&dir).expect("plan");
        assert_eq!(
            model_jobs(&dir, &resolved).expect("jobs"),
            vec![("unsloth/Qwen3.6-35B-A3B-GGUF".to_string(), "only.gguf".to_string())]
        );
        // And the file being there takes it off the list.
        std::fs::create_dir_all(dir.join("data/models")).expect("models");
        std::fs::write(dir.join("data/models/only.gguf"), "x").expect("model");
        assert!(model_jobs(&dir, &resolved).expect("jobs").is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A plan pointed at a program that is not an engine, so the spawn, the
    /// pid file and the health wait can be driven without a GPU.
    fn fake_plan(dir: &Path, args: Vec<&str>) -> Startup {
        let program = if cfg!(windows) { "cmd.exe" } else { "sh" };
        Startup {
            program: PathBuf::from(program),
            args: args.into_iter().map(str::to_string).collect(),
            workdir: dir.to_path_buf(),
            log: dir.join("logs/server.log"),
            pid_file: dir.join(".local/llama-server.pid"),
            dropped: Vec::new(),
            served: "fake".to_string(),
            engine: "fake".to_string(),
            build: "b0".to_string(),
            backend: "cuda".to_string(),
            bin: dir.join(".local/engines/fake/b0-cuda"),
            server: "fake.exe".to_string(),
            host: "127.0.0.1".to_string(),
            mode: "windows".to_string(),
            preset: None,
            router: false,
            alias: "fake".to_string(),
            via_launcher: None,
        }
    }

    #[test]
    fn a_server_that_dies_during_startup_is_reported_with_its_exit_code() {
        // The early-exit half of the health wait. Without it, `up` polls a
        // corpse for ten minutes and then reports a timeout instead of the
        // reason - which is the server's own last words and its exit code.
        let dir = home("up-dies");
        std::fs::create_dir_all(dir.join("logs")).expect("logs");
        std::fs::write(dir.join("logs/server.log"), "boom: bad flag\n").expect("log");
        let args = if cfg!(windows) { vec!["/c", "exit", "3"] } else { vec!["-c", "exit 3"] };
        let plan = fake_plan(&dir, args);
        let endpoint = engine::Endpoint { port: 1, api_key: String::new() };
        let err = start(&dir, &plan, &endpoint).expect_err("the child exited at once");
        assert!(err.contains("exit code 3"), "{err}");
        // The pid was written before the wait, as the launcher's is.
        let pid = std::fs::read_to_string(dir.join(".local/llama-server.pid")).expect("pid");
        assert!(pid.trim().parse::<u32>().is_ok(), "{pid:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_log_tail_is_the_last_lines() {
        let dir = home("tail");
        let lines: Vec<String> = (1..=20).map(|n| format!("line {n}")).collect();
        std::fs::write(dir.join("server.log"), lines.join("\n")).expect("log");
        let shown = tail(&dir.join("server.log"), LOG_TAIL);
        assert!(shown.starts_with("line 6\n"), "{shown}");
        assert!(shown.ends_with("line 20"), "{shown}");
        assert_eq!(shown.lines().count(), LOG_TAIL);
        // A log that is not there, or shorter than the tail, is not a panic.
        assert_eq!(tail(&dir.join("nope.log"), LOG_TAIL), "");
        std::fs::write(dir.join("short.log"), "one\n").expect("log");
        assert_eq!(tail(&dir.join("short.log"), LOG_TAIL), "one");
        std::fs::remove_dir_all(&dir).ok();
    }
}
