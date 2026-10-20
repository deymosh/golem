//! `up` / `down` / `restart`: the engine, started and stopped by the binary.
//!
//! The port of `scripts/stop-windows.ps1` / `stop-local-ai.sh`, and the first
//! half of "the binary is the app". These launchers are the last `.env`
//! readers in the repository, and this is why they exist: to kill a process
//! tree and to wait for the card. Neither needs PowerShell or bash.
//!
//! ## The two rules that are easy to get wrong
//!
//! - **Kill the tree, not the process.** In router mode each model runs in a
//!   child `llama-server`; killing only the pid file's process orphans it with
//!   the whole model still in VRAM and RAM.
//! - **Stopping is not done when the kill is issued.** The driver reclaims the
//!   VRAM a moment later, and a start that begins in between fails with
//!   `cudaMalloc failed: out of memory` while reporting a perfectly healthy
//!   pid. "Stopped" has to mean the memory is free, so this waits for the card.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use golem_core::adapter;
use golem_core::ini;
use golem_core::record;

use crate::engine;
use crate::state;

/// How long the card may take to come back.
const CARD_SETTLE: Duration = Duration::from_secs(20);

/// What "the model is gone" looks like: a loaded 35B holds ~11 GB, and a
/// desktop holds under 2. Below this, nothing of ours is resident any more.
const CARD_IDLE_MIB: u64 = 2048;

/// How long the process may take to die.
const PROCESS_SETTLE: Duration = Duration::from_secs(20);

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
}

/// Resolve the engine, its build, and the arguments it is started with.
///
/// The launcher's order, and each step has a reason: `ENGINE` picks the
/// adapter, `GPU_BACKEND` picks the build flavour (auto-detected from the
/// toolchain when unset), the build pin decides which directory under
/// `.local/engines/` holds the server, and the preset decides between router
/// mode (`--models-preset`, one model resident) and a single section's flags.
pub fn plan(home: &Path) -> Result<Startup, String> {
    let engine = state::env_value(home, "ENGINE").unwrap_or_else(|| "llama.cpp".to_string());
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
    let alias = state::env_value(home, "MODEL_ALIAS").unwrap_or_else(|| "local-model".to_string());
    let sleep_idle = state::env_value(home, "SLEEP_IDLE_SECONDS").unwrap_or_default();

    // The preset decides the mode, and the adapter says whether the engine can
    // read one at all: `router = true` means it understands `--models-preset`.
    let preset_rel = state::env_value(home, "MODELS_PRESET").unwrap_or_default();
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
    } else {
        let text = std::fs::read_to_string(&preset_path).unwrap_or_default();
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
    }

    let line = adapter::translate(&flags, &adapter);
    Ok(Startup {
        program: bin.join(&server),
        args: line.args,
        workdir,
        log,
        pid_file: home.join(".local").join(pid_name()),
        dropped: line.dropped,
        served,
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
}
