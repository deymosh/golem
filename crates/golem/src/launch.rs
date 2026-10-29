//! `up` / `down` / `restart` / `status`: the engine, started and stopped.
//!
//! Resolve the command line from `golem.toml`, the adapter and the preset
//! ([`plan`], pure); install the pinned build and fetch the preset's models
//! when they are missing; spawn the server; wait for it. These run inside
//! `golem service`, which owns the engine - the CLI asks it rather than
//! calling them directly (see `service.rs`).
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
//!   A child holding our pipes open keeps anything reading our output
//!   (`golem up | tail`) waiting for an EOF that never comes.
//! - **Health is a deadline *and* an early exit.** Ten minutes of polling is
//!   right for a 20 GB load and wrong for a server that died on its first
//!   flag: without the exit check, `up` would poll a corpse for ten minutes
//!   and then report a timeout instead of the reason.

use std::io::Write;
use std::net::ToSocketAddrs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use golem_core::adapter;
use golem_core::gpu;
use golem_core::ini;
use golem_core::record;

use crate::config::{self, Overrides};
use crate::client;
use crate::engine;
use crate::pull;
use crate::release;
use crate::machine;
use crate::paths;

/// How long the card may take to come back.
const CARD_SETTLE: Duration = Duration::from_secs(20);

/// What "the model is gone" looks like: a loaded 35B holds ~11 GB, and a
/// desktop holds under 2. Below this, nothing of ours is resident any more.
const CARD_IDLE_MIB: u64 = 2048;

/// How long the process may take to die.
const PROCESS_SETTLE: Duration = Duration::from_secs(20);

/// How long the model may take to load: ten minutes, which is
/// a first boot reading 20 GB from a cold page cache and warming the GPU.
const LOAD_DEADLINE: Duration = Duration::from_secs(600);

/// How often `/health` is asked while it loads. Three seconds, which is also
/// about how long a loaded 35B needs to answer once.
const HEALTH_INTERVAL: Duration = Duration::from_secs(3);

/// How much of the server's own log is shown when it dies during startup.
/// It is shown at all because a bad flag or a missing model says so in
/// those lines.
const LOG_TAIL: usize = 15;

/// `golem down`: stop the engine and wait for the memory.
pub fn down(home: &Path, out: &mut dyn Write) -> i32 {
    let Some(pid) = engine::pid(home) else {
        let _ = writeln!(out, "[INFO] no pid file - nothing to stop");
        return 0;
    };
    if !alive(pid) {
        // A pid file outliving its process is normal enough (a crash, a kill),
        // and the honest answer is to clean it up rather than report a stop
        // that did not happen.
        let _ = writeln!(out, "[INFO] process {pid} is not running - clearing the pid file");
        remove_pid(home);
        return 0;
    }

    let _ = writeln!(out, "[ .. ] stopping the engine (pid {pid})...");
    kill_tree(pid, false);

    let deadline = Instant::now() + PROCESS_SETTLE;
    while Instant::now() < deadline && alive(pid) {
        std::thread::sleep(Duration::from_millis(500));
    }
    // A process that ignored the polite signal gets the forceful one, and
    // one that survives even that keeps its pid file: it is still running,
    // and the file is the only way the next `down` can find it.
    if alive(pid) {
        kill_tree(pid, true);
        std::thread::sleep(Duration::from_secs(2));
        if alive(pid) {
            let _ = writeln!(out, "[FAIL] the engine (pid {pid}) is still running after a forced stop");
            return 1;
        }
    }
    remove_pid(home);

    if !wait_for_card() {
        let _ = writeln!(
            out,
            "[WARN] the GPU still reports {CARD_IDLE_MIB} MiB or more in use after {}s - \
             the next start may fail",
            CARD_SETTLE.as_secs()
        );
    }
    let _ = writeln!(out, "[ OK ] engine (pid {pid}) stopped");
    0
}

/// `golem up`: install the pinned build if it is missing, start the engine,
/// and wait until it answers.
///
/// `dry_run`: resolve everything, install nothing, start nothing. It comes
/// *before* the port check, so a dry run answers while an engine is up: its
/// whole job is to say what would be started, and refusing to answer while
/// something is running is the opposite of useful.
pub fn up(home: &Path, overrides: &Overrides, dry_run: bool, out: &mut dyn Write) -> Result<(), String> {
    let mut plan = plan(home, overrides)?;

    // Said before a single model is started rather than a router, in a dry
    // run too: it changes what the start means.
    if !plan.router {
        let _ = writeln!(out, "[ .. ] {} has no router mode: serving only [{}]", plan.engine, plan.served);
    }

    if dry_run {
        if !plan.program.is_file() {
            let _ = writeln!(
                out,
                "[ .. ] would install {} {} ({}) into {}",
                plan.engine,
                plan.build,
                plan.backend,
                plan.install_target()
            );
        }
        for (key, value) in &plan.dropped {
            let _ = writeln!(out, "[WARN] this engine has no equivalent of --{key} {value}, dropped");
        }
        let _ = writeln!(out, "[ OK ] dry run: \"{}\" {}", plan.program.display(), plan.args.join(" "));
        let _ = writeln!(out, "       (working directory: {})", plan.workdir.display());
        let _ = writeln!(out, "       serving: {}  -> pid {}", plan.served, plan.pid_file.display());
        // A dry run installs and fetches nothing, so it must at least say what
        // a real one would fetch, or the report is a lie by omission.
        // The same refusal a real start would hit (a served model with no
        // download source) is the dry run's answer too.
        let jobs = model_jobs(home, &plan, out)?;
        if !jobs.is_empty() {
            let _ = writeln!(out, "[ .. ] would fetch {} model file(s) the preset needs", jobs.len());
        }
        return Ok(());
    }

    let endpoint = engine::Endpoint::from_home(home);
    // A connect, not a request: a server that is up but still loading
    // answers nothing while it holds the socket.
    if listening(endpoint.port) {
        // Two cases, and they are not the same answer. Something that is *our*
        // engine, already answering, makes `up` a no-op that succeeds - which
        // is what a script wants from `up && bench`, and what a service that
        // already has the engine up needs. Something else holding the port is
        // a mistake worth refusing, because the alternative is a server that
        // starts, reports a healthy pid, and answers nothing.
        if engine::healthy(&endpoint) {
            let _ = writeln!(out, "[ OK ] Local AI server is already running");
            return Ok(());
        }
        return Err(format!(
            "port {} is already in use. Stop the other server first: golem down",
            endpoint.port
        ));
    }

    if !plan.program.is_file() {
        let installed = release::ensure(home, &plan.pinned(), out)?;
        // The plan resolved a directory from what the configuration asked
        // for, and `latest` is not a directory name - the tag is. `ensure`
        // is the only thing that knows it, so the plan is corrected here
        // rather than the server being started from a path that does not
        // exist. The banner then names the build that is actually running.
        plan.build = installed.tag;
        plan.program = installed.dir.join(&plan.server);
        plan.bin = installed.dir;
    }

    // After the engine is on disk and before anything is spawned.
    ensure_models(home, &plan, out)?;

    start(home, &plan, &endpoint, out)
}

/// `golem restart`: down, then up.
///
/// `cmd_restart`'s two lines, and its `|| true` is the point: a restart is
/// asked for exactly when the engine may not be running, so a stop that
/// found nothing is not a failure to stop.
pub fn restart(home: &Path, overrides: &Overrides, out: &mut dyn Write) -> Result<(), String> {
    down(home, out);
    up(home, overrides, false, out)
}

/// `golem status`: up or down, on what port, with what the engine lists.
///
/// Exit code: 0 when the engine answers, 1 when it does not, so a script can
/// branch on it.
pub fn status(home: &Path, out: &mut dyn Write) -> i32 {
    let endpoint = engine::Endpoint::from_home(home);
    let status = engine::status(home);

    if !status.up {
        let _ = writeln!(out, "[ .. ] engine down - no answer on http://127.0.0.1:{}", endpoint.port);
        if let Ok(why) = std::fs::read_to_string(paths::load_failure(home)) {
            let _ = writeln!(out, "        {}", why.trim());
        }
        match status.pid {
            // A pid file that outlived its process is worth saying out loud:
            // it is what leaves an engine looking running when it is not.
            Some(pid) => {
                let _ = writeln!(out, "        a pid file still records {pid} (start it, or the launcher clears it)");
            }
            None => {
                let _ = writeln!(out, "        start it with: golem up");
            }
        }
        return 1;
    }

    match status.pid {
        Some(pid) => {
            let _ = writeln!(out, "[ OK ] engine up on http://127.0.0.1:{} (pid {pid})", status.port);
        }
        None => {
            let _ = writeln!(out, "[ OK ] engine up on http://127.0.0.1:{}", status.port);
        }
    }
    let api = engine::api_port(home);
    let web = if crate::api::web_tools_on(home) { "web search on" } else { "web search off" };
    if listening(api) {
        let _ = writeln!(out, "       API on port {api} (golem, {web})");
    } else {
        let _ = writeln!(out, "[WARN] nothing answers the API port {api} - is the golem service running?");
    }
    let models = engine::models(&endpoint);
    if models.is_empty() {
        let _ = writeln!(out, "       no models listed");
    } else {
        let _ = writeln!(out, "       serving: {}", models.join(", "));
    }
    0
}

/// The model files the preset needs and `data/models` lacks, in the order the
/// preset names them, with the warnings already written to `out`.
///
/// Pure: it reads the preset and decides, and fetches nothing. That is what
/// lets a dry run say what a real one would download - and what lets the rules
/// below be tested without a 20 GB file.
fn model_jobs(home: &Path, plan: &Startup, out: &mut dyn Write) -> Result<Vec<(String, String)>, String> {
    let models = home.join("data/models");
    let preset_rel = &plan.preset;
    let path = home.join(preset_rel);
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let preset = ini::parse(&text);

    // The alias names no section, so this preset is new to this machine and
    // there is nothing to be conservative about: everything it names is
    // fetched.
    let all = preset.get(&plan.alias).is_none();

    let mut jobs: Vec<(String, String)> = Vec::new();
    for section in &preset.sections {
        for download in &section.downloads {
            if models.join(download.name()).is_file() {
                continue;
            }
            // A section that is not the default one is not fetched: a router
            // loads a model on request, and starting one model must not pull
            // another's 20 GB.
            if all || section.name == plan.alias {
                jobs.push((download.repo.clone(), download.file.clone()));
            } else {
                let _ = writeln!(
                    out,
                    "[WARN] preset [{}] is missing {} - fetch it with: golem pull {}",
                    section.name, download.name(), section.name
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
        .flat_map(|section| section.downloads.iter().map(|d| d.name()))
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
                let _ = writeln!(
                    out,
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
/// The failure without it is the reason: a router whose default section's
/// model is missing starts healthy and fails at the first request, which is
/// the worst place to find out. A fresh machine's first `up` is therefore a
/// download and then a working server.
fn ensure_models(home: &Path, plan: &Startup, out: &mut dyn Write) -> Result<(), String> {
    let jobs = model_jobs(home, plan, out)?;
    if jobs.is_empty() {
        return Ok(());
    }
    let _ = writeln!(out, "[ .. ] fetching {} model file(s) the preset needs", jobs.len());
    pull::fetch_all(home, &jobs, out)
}

/// Spawn the server, record its pid, and wait for it to answer.
fn start(home: &Path, plan: &Startup, endpoint: &engine::Endpoint, out: &mut dyn Write) -> Result<(), String> {
    let mut command = Command::new(engine_program(plan));
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
        // wrong from the inside; from the outside `golem up | tail` waited
        // for an EOF that never came. Setting no stdio at all is worse (the
        // child then gets our handles outright, banner and all), and
        // `cmd /c start` does not help either - both were measured. The fix
        // is structural: the engine is spawned by the long-lived service, not
        // by the command that asked for it (docs/architecture.md).
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // A server sharing our console would take a Ctrl+C aimed at us, so on
    // Windows it runs without a console window, not attached to ours
    // (`CREATE_NO_WINDOW`).
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
    // A new engine is a new chance to load: the last one's failure is history.
    let _ = std::fs::remove_file(paths::load_failure(home));

    if let Some(dir) = plan.pid_file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&plan.pid_file, format!("{pid}\n")) {
        // Without its pid file nothing could stop this engine later - `down`
        // finds engines by it - so a start that cannot record one is undone.
        kill_tree(pid, true);
        let _ = child.wait();
        return Err(format!("cannot write {}: {e}", plan.pid_file.display()));
    }

    let _ = writeln!(out, "[ OK ] {} {} started, PID {pid}", plan.engine, plan.build);
    let _ = writeln!(out, "       logs: {}", plan.log.display());
    let _ = writeln!(out, "[ .. ] waiting for the model to load (first boot includes GPU warmup)...");

    if let Err(e) = wait_healthy(plan, &mut child, endpoint, out) {
        // A server that never answered is stopped rather than left loading
        // behind a start that reported failure; one that already exited is
        // reaped. Either way it no longer runs, so its pid file goes too.
        kill_tree(pid, true);
        let _ = child.wait();
        let _ = std::fs::remove_file(&plan.pid_file);
        return Err(e);
    }

    // The parent outlives the engine - the service is long-lived - so someone
    // must reap the child or it sits as a zombie on Unix, where `kill -0`
    // still answers for it and `down` polls a corpse. One blocked thread per
    // engine, for the engine's lifetime.
    std::thread::spawn(move || {
        let _ = child.wait();
    });

    // The closing line, one per mode: a router loads its
    // models on demand, so what is healthy is the router and not a model.
    let api = engine::api_port(home);
    match plan.router {
        true => {
            let _ = writeln!(
                out,
                "[ OK ] router healthy on http://{}:{api} (presets: {}; models load on first request)",
                plan.host, plan.preset
            );
        }
        false => {
            let _ = writeln!(
                out,
                "[ OK ] healthy on http://{}:{api} (model: {})",
                plan.host, plan.served
            );
        }
    }
    let _ = writeln!(out, "       golem answers that port; the engine is on 127.0.0.1:{}", endpoint.port);
    let _ = writeln!(out);
    // The shell that typed this reads one of these two: `$env:` on Windows,
    // `export` everywhere else. That is a fact about the platform, not a mode
    // somebody chose.
    let _ = write!(
        out,
        "{}",
        if cfg!(windows) {
            client::powershell_exports(home)
        } else {
            client::exports(home)
        }
    );
    Ok(())
}

/// What the engine is started as. On Windows, a hard link beside the
/// server named for golem - `Golem engine (llama.cpp).exe` - so Task Manager
/// lists the engine (and the model processes a router starts from its own
/// path) as golem's rather than as a bare `llama-server`. The link is made
/// afresh each start, so a reinstalled build never runs through a stale one,
/// and where it cannot be made (another volume, a link held open by a
/// running engine) the server itself is started.
fn engine_program(plan: &Startup) -> PathBuf {
    if !cfg!(windows) {
        return plan.program.clone();
    }
    let link = plan.bin.join(engine_link_name(&plan.engine));
    let _ = std::fs::remove_file(&link);
    match std::fs::hard_link(&plan.program, &link) {
        Ok(()) => link,
        Err(_) => plan.program.clone(),
    }
}

/// The engine's name in the process list, from the adapter's name.
fn engine_link_name(engine: &str) -> String {
    format!("Golem engine ({engine}){}", std::env::consts::EXE_SUFFIX)
}

/// Poll `/health` until it answers, until the child exits, or until the
/// deadline passes.
fn wait_healthy(
    plan: &Startup,
    child: &mut Child,
    endpoint: &engine::Endpoint,
    out: &mut dyn Write,
) -> Result<(), String> {
    let deadline = Instant::now() + LOAD_DEADLINE;
    loop {
        // Checked before the request: a server that died at once is already
        // gone by the first poll, and its exit code and log say more than a
        // timeout would.
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("cannot watch the server: {e}"))?
        {
            let tail = tail(&plan.log, LOG_TAIL);
            if !tail.is_empty() {
                let _ = writeln!(out, "{tail}");
            }
            return Err(format!(
                "{} server exited during startup ({})",
                plan.engine,
                code_text(status)
            ));
        }
        if engine::healthy(endpoint) {
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

/// An exit status in words: a process killed by a
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

/// Kill the process and everything it started; `force` is SIGKILL rather
/// than SIGTERM on Unix (Windows always forces).
fn kill_tree(pid: u32, force: bool) {
    if cfg!(windows) {
        // /T: in router mode each model is a child process, and /F because a
        // model mid-generation does not answer a polite request.
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .output();
        return;
    }
    // The children first, because the parent exiting does not take a loaded
    // model with it, then the parent.
    let signal = if force { "-KILL" } else { "-TERM" };
    let _ = Command::new("pkill").args([signal, "-P", &pid.to_string()]).output();
    let _ = Command::new("kill").args([signal, &pid.to_string()]).output();
}

/// Remove the home's pid file.
fn remove_pid(home: &Path) {
    let _ = std::fs::remove_file(paths::pid_file(home));
}

/// Wait until the card is back to a desktop's worth of use. `false` when it
/// did not, within [`CARD_SETTLE`].
fn wait_for_card() -> bool {
    let deadline = Instant::now() + CARD_SETTLE;
    loop {
        match machine::vram_used_mib() {
            // No readable card (a headless box, a container): nothing to wait for.
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

/// Everything needed to start the engine, resolved from the home.
///
/// Pure: it reads files and decides, and spawns nothing. That seam is what
/// makes the start checkable without a GPU - the same home gives the same
/// plan, and `up --dry-run` is what prints it.
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
    /// The address golem's front answers clients on (the engine itself binds
    /// the loopback). The closing banner names it, and the export block that
    /// follows is in the syntax of the platform's shell.
    pub host: String,
    /// The preset, in router mode: what the closing line names as the thing
    /// that was served. `None` means a single model, and `served` names it.
    pub preset: String,
    /// Whether `--models-preset` is on the command line. The three shapes are
    /// distinguishable only here and in `preset`: a router serves every
    /// section, a single-model engine serves one section of a preset, and
    /// with no preset at all golem.toml is the configuration.
    pub router: bool,
    /// The default section's name. In router mode it is the one whose model
    /// must be on disk before the server starts; `served` cannot say it there,
    /// because a router serves all of them.
    pub alias: String,
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
/// The order matters, and each step has a reason: `ENGINE` picks the
/// adapter, `GPU_BACKEND` picks the build flavour (auto-detected from the
/// toolchain when unset), the build pin decides which directory under
/// `.local/engines/` holds the server, and the preset decides between router
/// mode (`--models-preset`, one model resident) and a single section's flags.
pub fn plan(home: &Path, overrides: &Overrides) -> Result<Startup, String> {
    // Three keys are read through the overrides, not `golem.toml` alone,
    // because something depends on it: the tuner
    // points one run at one candidate by setting `GOLEM_ENGINE`,
    // `GOLEM_MODELS_PRESET` and `GOLEM_MODEL_ALIAS` in the environment of the
    // command that starts the engine. Dropping them is not a cosmetic
    // difference - a sweep would restart the same preset for every candidate
    // and the gate would score whatever golem.toml names, which is how a 0.5B
    // candidate was once "gated" against the 35B coder's numbers.
    let engine = overrides.get(home, "ENGINE")
        .unwrap_or_else(|| "llama.cpp".to_string());
    let adapter_text = paths::adapter(home, &engine)
        .map_err(|e| format!("cannot read {}: {e}", paths::adapter_path(home, &engine).display()))?;
    let adapter = ini::parse(&adapter_text);

    let backend = resolve_backend(home, adapter.global("backend").unwrap_or("cuda"));
    let build = engine_build(home, &engine, &adapter)?;
    let bin = home.join(".local").join("engines").join(&engine).join(format!("{build}-{backend}"));
    let server = server_name(&adapter);

    // The engine answers golem alone: loopback, on its own port. Clients
    // reach golem's front on `LOCAL_AI_PORT` (`api.rs`), which hands their
    // requests on.
    let port = engine::engine_port(home).to_string();
    let key = config::get(home, "LOCAL_AI_API_KEY").unwrap_or_else(|| "local-ai-key".to_string());
    // Where the front listens, for the closing line: `LOCAL_AI_HOST` through
    // the override, because a container is the one caller that must *not*
    // take it from the file - the file is a host-side setting (127.0.0.1
    // keeps the LAN out), while a container's port is published and has to
    // be bound on 0.0.0.0 whatever the mounted golem.toml says.
    let host = overrides.get(home, "LOCAL_AI_HOST")
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let alias = overrides.get(home, "MODEL_ALIAS")
        .unwrap_or_else(|| "local-model".to_string());
    let sleep_idle = config::get(home, "SLEEP_IDLE_SECONDS").unwrap_or_default();

    // The models are always a preset file, and the adapter says whether the
    // engine can read one itself: `router = true` means it understands
    // `--models-preset`; an engine without one is started on the default
    // section's flags.
    let preset_rel = config::models_preset(home, overrides);
    let preset_path = home.join(&preset_rel);
    // A preset that is not there is a failure, not a fallback: a container
    // whose preset volume was not mounted is exactly how this happens.
    if !preset_path.is_file() {
        return Err(format!(
            "MODELS_PRESET names {} and there is no such file under {} - \
             there is nothing to serve",
            preset_rel,
            home.display()
        ));
    }
    let router = adapter.global("router").as_deref() != Some("false");

    let mut flags: Vec<(String, String)> = vec![
        ("host".to_string(), "127.0.0.1".to_string()),
        ("port".to_string(), port.clone()),
    ];
    // These two come from golem.toml onto the command line, and both
    // are tuned parameters: `threads` is what the tune sweep varies, and
    // `parallel 1` is what keeps one model resident.
    for (key, name) in [("CPU_THREADS", "threads"), ("PARALLEL", "parallel")] {
        if let Some(value) = config::get(home, key).filter(|v| !v.is_empty()) {
            flags.push((name.to_string(), value));
        }
    }
    flags.push(("metrics".to_string(), "true".to_string()));
    flags.push(("api-key".to_string(), key.clone()));
    flags.push(("log-colors".to_string(), "off".to_string()));
    // The server writes its own log, and this is the file the console tails.
    let log = paths::engine_log(home);
    flags.push(("log-file".to_string(), log.to_string_lossy().into_owned()));
    if !sleep_idle.is_empty() {
        flags.push(("sleep-idle-seconds".to_string(), sleep_idle));
    }

    let served;
    let workdir;
    if router {
        // Router mode: one model resident at a time, and the preset carries
        // everything else - including `jinja`, which is why it is not passed
        // here. Command-line flags would override the preset, so
        // only the shared ones above go on the line.
        flags.push(("models-preset".to_string(), preset_path.to_string_lossy().into_owned()));
        flags.push(("models-max".to_string(), "1".to_string()));
        served = format!("router ({})", preset_path.strip_prefix(home).unwrap_or(&preset_path).display());
        workdir = home.join("data/models");
    } else {
        let text = std::fs::read_to_string(&preset_path).unwrap_or_default();
        // A server with no model on it is refused here, naming the section
        // it wanted, rather than started and found empty at the first request.
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
        // A merge, not an append: a key the preset shares with the
        // shared flags above is overwritten, not doubled - `--jinja
        // --jinja` is a duplicated flag on the command line.
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
    }

    let line = adapter::translate(&flags, &adapter);
    Ok(Startup {
        program: bin.join(&server),
        args: line.args,
        workdir,
        log,
        pid_file: paths::pid_file(home),
        dropped: line.dropped,
        served,
        engine,
        build,
        backend,
        bin,
        server,
        host,
        preset: preset_rel.clone(),
        router,
        alias,
    })
}

/// Merge `extra` into `into` like a map assignment: a key already present
/// keeps its position and takes the new value.
fn merge(into: &mut Vec<(String, String)>, extra: Vec<(String, String)>) {
    for (key, value) in extra {
        match into.iter_mut().find(|(existing, _)| *existing == key) {
            Some((_, existing)) => *existing = value,
            None => into.push((key, value)),
        }
    }
}

/// `GPU_BACKEND`, with `auto` resolved from what this machine has.
///
/// A backend is a fact about the card, so `auto` reads it: `nvidia-smi` on
/// PATH for CUDA (one cheap spawn, kept first), else the machine probe's
/// vendor - CUDA for NVIDIA, ROCm for AMD, Vulkan for Intel. On macOS the
/// answer is Metal whatever is installed. The adapter's `backend` key is
/// the last fallback, for a machine where nothing answers and the adapter
/// still knows what it wants.
fn resolve_backend(home: &Path, fallback: &str) -> String {
    let configured = config::get(home, "GPU_BACKEND").unwrap_or_else(|| "auto".to_string());
    match configured.as_str() {
        "cuda" | "rocm" | "metal" | "vulkan" => configured,
        _ if cfg!(target_os = "macos") => "metal".to_string(),
        _ if which("nvidia-smi") => "cuda".to_string(),
        _ => machine::gpu()
            .map(|card| gpu::backend(card.vendor).to_string())
            .unwrap_or_else(|| fallback.to_string()),
    }
}

/// The server executable's name on this platform.
///
/// The adapter names the binary and the platform supplies the extension: a
/// release carries `llama-server.exe` on Windows and `llama-server` on Linux,
/// and that is a fact about the platform rather than about the engine. A
/// `.exe` written into the adapter is accepted and normalised, so an adapter
/// written before this rule still works.
fn server_name(adapter: &ini::Document) -> String {
    let base = adapter
        .global("server")
        .unwrap_or("llama-server")
        .trim_end_matches(".exe")
        .to_string();
    if cfg!(windows) {
        format!("{base}.exe")
    } else {
        base
    }
}

/// The build pin: `ENGINE_BUILD` when the configuration sets one, else the
/// adapter's own `build` - the one the 200K gate adopted.
fn engine_build(home: &Path, engine: &str, adapter: &ini::Document) -> Result<String, String> {
    if let Some(build) = config::get(home, "ENGINE_BUILD").filter(|b| !b.is_empty()) {
        return Ok(build);
    }
    adapter
        .global("build")
        .map(str::to_string)
        .ok_or_else(|| format!("no build pinned for engine {engine} (build in its adapter)"))
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

    /// The server's file name as this platform spells it ([`server_name`]).
    fn server() -> String {
        format!("llama-server{}", std::env::consts::EXE_SUFFIX)
    }

    fn home(tag: &str) -> PathBuf {
        // Per process: two test binaries compile these tests, and on Windows a
        // directory a child just left can still be mid-delete for the next.
        let dir = std::env::temp_dir().join(format!("golem-rs-launch-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".local")).expect("temp");
        dir
    }

    #[test]
    fn a_home_with_no_pid_file_is_already_stopped() {
        let dir = home("none");
        assert_eq!(down(&dir, &mut std::io::sink()), 0, "stopping nothing is not a failure");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_pid_file_that_outlived_its_process_is_cleared() {
        // A pid that cannot be running: 0 is not a process any OS hands out.
        let dir = home("stale");
        std::fs::write(dir.join(".local/llama-server.pid"), "0\n").expect("write");
        assert_eq!(down(&dir, &mut std::io::sink()), 0);
        assert!(
            !dir.join(".local/llama-server.pid").exists(),
            "the stale pid file is gone"
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
        std::fs::create_dir_all(dir.join("config/engines")).expect("engine");
        std::fs::create_dir_all(dir.join("config")).expect("config");
        std::fs::write(
            dir.join("config/engines/llama.cpp.ini"),
            "name = llama.cpp\nbuild = b11379\nserver = llama-server.exe\nrouter = true\n\
             map.load-mode.mlock = --mlock\nflag.sleep-idle-seconds = -\n",
        )
        .expect("adapter");
        std::fs::write(dir.join("config/models.ini"), preset).expect("preset");
        std::fs::write(dir.join("golem.toml"), config).expect("config");
        dir
    }

    #[test]
    fn a_preset_that_is_named_but_missing_is_refused() {
        // Not a fallback to the single-model configuration: that would serve a
        // different model than the configuration names, and in a container -
        // an unmounted volume is how this happens - the load is gigabytes
        // before anybody notices.
        let dir = engine_home(
            "plan-preset-missing",
            "[*]\njinja = true\n[model-a]\nmodel = a.gguf\n",
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\n\
             MODELS_PRESET = \"data/tuned/not-mounted.ini\"\n",
        );
        let err = plan(&dir, &Overrides::default()).expect_err("a named preset has to be there");
        assert!(err.contains("not-mounted.ini"), "{err}");
        assert!(err.contains("MODELS_PRESET"), "the message names the key: {err}");
        std::fs::remove_dir_all(&dir).ok();
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
        let plan = plan(&dir, &Overrides::default()).expect("plan");
        assert!(plan.program.ends_with(format!("engines/llama.cpp/b11379-cuda/{}", server())), "{:?}", plan.program);
        assert!(plan.args.iter().any(|a| a == "--models-preset"), "{:?}", plan.args);
        assert!(plan.args.iter().any(|a| a == "--models-max"), "one model resident at a time");
        assert!(plan.args.contains(&"--port".to_string()), "the engine is on the port after golem's: {:?}", plan.args);
        assert!(plan.args.contains(&"8001".to_string()), "{:?}", plan.args);
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
            dir.join("config/engines/llama.cpp.ini"),
            "name = llama.cpp\nbuild = b11379\nserver = llama-server.exe\nrouter = false\n\
             map.load-mode.mlock = --mlock\n",
        )
        .expect("adapter");
        let plan = plan(&dir, &Overrides::default()).expect("plan");
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
    fn the_build_pin_is_engine_build_else_the_adapters() {
        let dir = engine_home("plan-build", "", "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\n");
        // The adapter's build; a stray `LLAMA_WIN_BUILD` key is not read.
        assert!(plan(&dir, &Overrides::default()).expect("plan").program.ends_with(format!("b11379-cuda/{}", server())));
        std::fs::write(
            dir.join("golem.toml"),
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nLLAMA_WIN_BUILD = \"b11755\"\n",
        )
        .expect("config");
        assert!(plan(&dir, &Overrides::default()).expect("plan").program.ends_with(format!("b11379-cuda/{}", server())));
        // ENGINE_BUILD is the configuration's word over the adapter's.
        std::fs::write(
            dir.join("golem.toml"),
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nENGINE_BUILD = \"b9\"\n",
        )
        .expect("config");
        assert!(plan(&dir, &Overrides::default()).expect("plan").program.ends_with(format!("b9-cuda/{}", server())));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_engine_is_a_message_not_a_panic() {
        let dir = home("plan-unknown");
        std::fs::write(dir.join("golem.toml"), "ENGINE = \"nope\"\n").expect("config");
        let err = plan(&dir, &Overrides::default()).expect_err("an engine with no adapter");
        assert!(err.contains("nope"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------------------------------------------------------
    // `up`
    // ---------------------------------------------------------------

    /// A home that resolves, on a port nothing holds: `LOCAL_AI_PORT = 1` is
    /// below the range anything binds, and a test that picked a plausible
    /// port would fail on a machine where the engine happens to be running.
    fn up_home(tag: &str, port: &str) -> PathBuf {
        engine_home(
            tag,
            "[*]\njinja = true\n[model-a]\nmodel = a.gguf\n",
            &format!(
                "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\n\
                 LOCAL_AI_PORT = \"{port}\"\nENGINE_PORT = \"{port}\"\nLOCAL_AI_API_KEY = \"k\"\n\
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
        assert!(engine::healthy(&endpoint));
        assert!(!engine::healthy(&engine::Endpoint { port: 1, api_key: String::new() }));
    }

    #[test]
    fn a_port_our_engine_already_answers_on_is_a_no_op() {
        // One rule for every platform now that there is one engine path: an
        // engine of ours that is already answering makes `up` succeed, which
        // is what `up && bench` needs from a script and what a service that
        // already has the engine up needs from a client.
        let port = health_server().to_string();
        let dir = up_home("up-busy", &port);
        assert_eq!(up(&dir, &Overrides::default(), false, &mut std::io::sink()), Ok(()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_port_something_else_holds_is_refused() {
        // Listening, but not our engine: a start here would report a healthy
        // pid and answer nothing, because the first listener owns the socket.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port().to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                // Accept and hang up: a port that is taken but not answering.
                drop(stream);
            }
        });
        let dir = up_home("up-busy-other", &port);
        let err = up(&dir, &Overrides::default(), false, &mut std::io::sink()).expect_err("the port is held");
        assert!(err.contains("already in use"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dry_run_resolves_the_whole_line_and_starts_nothing() {
        // The command line is the thing worth reading, not the banner, and a
        // dry run is the only way to see it without starting anything.
        let dir = up_home("up-dry", "1");
        // The served model has no download source and is not here: the dry
        // run refuses it, as the real start would.
        let err = up(&dir, &Overrides::default(), true, &mut std::io::sink()).expect_err("no source");
        assert!(err.contains("a.gguf"), "{err}");
        std::fs::create_dir_all(dir.join("data/models")).expect("models");
        std::fs::write(dir.join("data/models/a.gguf"), "x").expect("model");
        assert_eq!(up(&dir, &Overrides::default(), true, &mut std::io::sink()), Ok(()));
        // Nothing was installed, nothing was started, no pid was written.
        assert!(!dir.join(".local/llama-server.pid").exists());
        assert!(!dir.join(".local/engines/llama.cpp/b11379-cuda").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_runs_overrides_point_the_plan_elsewhere_for_that_run_only() {
        // How the tuner serves a candidate and a container binds every
        // interface: the overrides are a value handed to the plan, not the
        // process environment, so one test can hold two plans at once.
        let dir = up_home("plan-overrides", "1");
        std::fs::write(dir.join("config/other.ini"), "[model-b]\nmodel = b.gguf\n").expect("preset");
        let overrides = Overrides::default()
            .with("MODELS_PRESET", "config/other.ini")
            .with("MODEL_ALIAS", "model-b")
            .with("LOCAL_AI_HOST", "0.0.0.0");
        let candidate = plan(&dir, &overrides).expect("plan");
        assert_eq!(candidate.preset, "config/other.ini");
        assert_eq!(candidate.alias, "model-b");
        assert_eq!(candidate.host, "0.0.0.0");
        let configured = plan(&dir, &Overrides::default()).expect("plan");
        assert_eq!(configured.preset, "config/models.ini");
        assert_eq!(configured.host, "127.0.0.1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_plan_says_which_release_it_needs() {
        let dir = up_home("plan-pinned", "1");
        let pinned = plan(&dir, &Overrides::default()).expect("plan").pinned();
        assert_eq!(pinned.engine, "llama.cpp");
        assert_eq!(pinned.build, "b11379");
        assert_eq!(pinned.backend, "cuda");
        // The root, not the directory: `latest` is only a tag once GitHub has
        // been asked, so the leaf is named inside `release::ensure`.
        assert_eq!(pinned.root, dir.join(".local/engines/llama.cpp"));
        assert_eq!(pinned.server, server());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_plan_carries_the_preset_and_the_host_the_banner_needs() {
        let dir = up_home("plan-banner", "1");
        let plan = plan(&dir, &Overrides::default()).expect("plan");
        assert_eq!(plan.host, "127.0.0.1");
        assert_eq!(plan.preset, "config/models.ini");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---------------------------------------------------------------
    // The model fetch `up` does before it starts
    // ---------------------------------------------------------------

    /// A home with this preset, and these files already in `data/models`.
    fn preset_home(tag: &str, alias: &str, preset: &str, files: &[&str]) -> PathBuf {
        let dir = engine_home(
            tag,
            preset,
            &format!(
                "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\n\
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
        let jobs = model_jobs(&dir, &plan(&dir, &Overrides::default()).expect("plan"), &mut std::io::sink()).expect("jobs");
        assert_eq!(jobs, vec![("org/a".to_string(), "a.gguf".to_string())]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_model_already_on_disk_is_not_a_job() {
        let dir = preset_home("jobs-present", "model-a", TWO_SECTIONS, &["a.gguf"]);
        let jobs = model_jobs(&dir, &plan(&dir, &Overrides::default()).expect("plan"), &mut std::io::sink()).expect("jobs");
        assert!(jobs.is_empty(), "{jobs:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_alias_that_names_no_section_fetches_everything() {
        // A preset this machine has not seen: the alias names no section, so
        // there is no "the one being served" to be conservative about.
        let dir = preset_home("jobs-new", "not-a-section", TWO_SECTIONS, &[]);
        let jobs = model_jobs(&dir, &plan(&dir, &Overrides::default()).expect("plan"), &mut std::io::sink()).expect("jobs");
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
        let err = model_jobs(&dir, &plan(&dir, &Overrides::default()).expect("plan"), &mut std::io::sink()).expect_err("no source");
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
        let jobs = model_jobs(&dir, &plan(&dir, &Overrides::default()).expect("plan"), &mut std::io::sink()).expect("jobs");
        assert_eq!(jobs, vec![("org/a".to_string(), "a.gguf".to_string())]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_draft_model_counts_as_a_model_the_section_needs() {
        // `model` and `spec-draft-model` are scanned alike: a missing
        // draft fails the load just as a missing model does.
        let dir = preset_home(
            "jobs-draft",
            "model-a",
            "; Download: org/a -> a.gguf\n[model-a]\nmodel = a.gguf\nspec-draft-model = d.gguf\n",
            &[],
        );
        let err = model_jobs(&dir, &plan(&dir, &Overrides::default()).expect("plan"), &mut std::io::sink()).expect_err("no source");
        assert!(err.ends_with("d.gguf"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
    #[test]
    fn with_no_preset_named_the_shipped_presets_are_served() {
        // MODELS_PRESET unset or empty resolves to config/models.ini, and the
        // default section's downloads are the fetch list.
        let dir = engine_home(
            "jobs-none",
            "; Download: org/a -> only.gguf\n[model-a]\nmodel = only.gguf\n",
            "ENGINE = \"llama.cpp\"\nGPU_BACKEND = \"cuda\"\nMODEL_ALIAS = \"model-a\"\n",
        );
        let resolved = plan(&dir, &Overrides::default()).expect("plan");
        assert_eq!(resolved.preset, "config/models.ini");
        assert_eq!(
            model_jobs(&dir, &resolved, &mut std::io::sink()).expect("jobs"),
            vec![("org/a".to_string(), "only.gguf".to_string())]
        );
        // And the file being there takes it off the list.
        std::fs::create_dir_all(dir.join("data/models")).expect("models");
        std::fs::write(dir.join("data/models/only.gguf"), "x").expect("model");
        assert!(model_jobs(&dir, &resolved, &mut std::io::sink()).expect("jobs").is_empty());
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
            preset: "config/models.ini".to_string(),
            router: false,
            alias: "fake".to_string(),
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
        let err = start(&dir, &plan, &endpoint, &mut std::io::sink()).expect_err("the child exited at once");
        assert!(err.contains("exit code 3"), "{err}");
        // The pid file was written before the wait and goes with the server.
        assert!(!dir.join(".local/llama-server.pid").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_engine_is_named_for_golem() {
        assert_eq!(
            engine_link_name("llama.cpp"),
            format!("Golem engine (llama.cpp){}", std::env::consts::EXE_SUFFIX)
        );
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
