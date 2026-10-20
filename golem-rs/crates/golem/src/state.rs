//! What the console shows: the engine's state, the installed presets, the
//! log, and the one job that may be running.
//!
//! These are read from the same places the shell console reads them - the
//! home's `.env`, its preset file, the engine's own HTTP endpoints, and
//! nvidia-smi for the GPU - so the Rust console is a drop-in for the page
//! the scripts serve, not a second interpretation of the same facts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use golem_core::ini;

use crate::engine;

/// One value from the home's configuration: `golem.toml` first, then `.env`.
///
/// The rule itself lives in [`golem_core::config::value`], because it is the
/// same rule every reader needs - including `engine::Endpoint`, which parses
/// the port and the API key for itself. A reader with its own copy of the
/// rule stops seeing a key on the day it moves, and that is the failure the
/// split exists to avoid.
pub fn env_value(home: &Path, key: &str) -> Option<String> {
    golem_core::config::value(home, key)
}

/// One value from `golem.toml` alone, or `None`.
///
/// For the one question that needs to know *where* a value came from: the
/// writers, which must put it back in the same file.
pub fn config_value(home: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(home.join("golem.toml")).ok()?;
    golem_core::config::Document::parse(&text).ok()?.get(key)
}

/// The shell's `${GOLEM_X:-${X:-default}}` idiom, in one place.
///
/// `GOLEM_*` names are *per-invocation overrides a caller sets in the
/// environment* - the tuner points a launcher at one candidate preset with
/// them, and the console's job runner does the same - and they are
/// deliberately never keys in `.env`. The plain name is the `.env` value.
///
/// Reading the override out of `.env` instead is not a harmless
/// simplification: it is the shape of a bug this repository already paid for
/// once, when `GOLEM_MODEL_ALIAS` did not survive a Windows-to-WSL hop and a
/// 0.5B candidate was gated against the 35B coder's numbers.
pub fn override_value(home: &Path, override_key: &str, key: &str) -> Option<String> {
    if let Ok(value) = std::env::var(override_key) {
        if !value.is_empty() {
            return Some(value);
        }
    }
    env_value(home, key)
}

/// The deploy mode for this invocation.
///
/// The front door's `--native` / `--docker` / `--windows` wins: `main`
/// resolves it into `GOLEM_DEPLOY_MODE` before dispatching, exactly as the
/// shell resolves it before a `cmd_*` runs - so `./golem install --native` is
/// a native install even on a box whose `.env` says `windows`. `.env` is the
/// fallback and `native` the default, and an empty assignment counts as
/// unset, because the shell's `:-` treats it that way.
///
/// One reader for the rule, because two commands that disagree about the mode
/// disagree about everything downstream: `install` builds for one platform
/// and `up` starts a server for one.
pub fn deploy_mode(home: &Path) -> String {
    override_value(home, "GOLEM_DEPLOY_MODE", "DEPLOY_MODE")
        .filter(|mode| !mode.is_empty())
        .unwrap_or_else(|| "native".to_string())
}

/// Write `key="value"` into the home's `.env`.
///
/// Every line that assigns `key` is replaced - including a commented-out one,
/// which is how `scripts/web.ps1`'s `Env-Set` behaves, and how `.env.example`
/// ships the optional keys - and the key is appended when the file has none.
/// The line ending the file already uses is kept.
pub fn env_set(home: &Path, key: &str, value: &str) -> std::io::Result<()> {
    env_write(home, key, value, true)
}

/// Write `key="value"` over an *active* assignment only, appending when
/// there is none.
///
/// This is the other half of the shell's two rules, and it is not a
/// simplification to merge them: `tune.sh --adopt` rewrites `.env` with
/// `sed 's|^ENGINE=.*|...|'`, which leaves a commented `#ENGINE=` where it
/// is, while the console's `Env-Set` uncomments it. Adoption is the one that
/// must not resurrect a line the user deliberately turned off.
pub fn env_adopt(home: &Path, key: &str, value: &str) -> std::io::Result<()> {
    env_write(home, key, value, false)
}

fn env_write(home: &Path, key: &str, value: &str, commented: bool) -> std::io::Result<()> {
    // A key `golem.toml` owns is written there, not into the shim. Writing
    // `.env` for a key the reader takes from the TOML file would look like it
    // worked and change nothing, which is the worst of both.
    if config_value(home, key).is_some() {
        return config_write(home, key, value);
    }
    let path = home.join(".env");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let eol = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::new();
    let mut replaced = false;
    for line in text.lines() {
        if assigns(line, key, commented) {
            out.push_str(&format!("{key}=\"{value}\""));
            replaced = true;
        } else {
            out.push_str(line);
        }
        out.push_str(eol);
    }
    if !replaced {
        out.push_str(&format!("{key}=\"{value}\"{eol}"));
    }
    std::fs::write(path, out)
}

/// Set one key in the home's `golem.toml`, keeping every comment around it.
///
/// The file is parsed and written back line by line, which is what keeps the
/// documentation where the user put it - `Document::set` replaces the one line
/// that assigns the key and touches nothing else.
fn config_write(home: &Path, key: &str, value: &str) -> std::io::Result<()> {
    let path = home.join("golem.toml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let mut document = golem_core::config::Document::parse(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    document.set(key, value);
    std::fs::write(path, document.text())
}

/// Whether a `.env` line assigns `key`.///
/// With `commented`, a commented-out assignment counts too.
fn assigns(line: &str, key: &str, commented: bool) -> bool {
    let rest = line.trim_start();
    let rest = match rest.strip_prefix('#') {
        Some(rest) if commented => rest.trim_start(),
        Some(_) => return false,
        None => rest,
    };
    match rest.split_once('=') {
        Some((name, _)) => name.trim() == key,
        None => false,
    }
}

/// The preset file, as the home refers to it.
pub fn preset_path(home: &Path) -> PathBuf {
    let configured = env_value(home, "MODELS_PRESET").unwrap_or_else(|| "config/models.ini".to_string());
    home.join(configured)
}

/// The preset sections, in file order, without the shared `[*]` one.
///
/// The parsing itself lives in `golem_core::ini`, which is what the console's
/// Models tab and the tuner read the same file with; a second scanner here
/// would be a second answer to "what is a section".
pub fn presets(home: &Path) -> Vec<String> {
    ini::Document::read(&preset_path(home)).map(|doc| doc.preset_names()).unwrap_or_default()
}

/// The engine adapters installed under `engines/`, by directory name.
///
/// This is what the console's engine dropdown offers, so it lists what is
/// actually installed rather than what the code knows about: an adapter
/// dropped into the folder appears without a rebuild, and one that was never
/// fetched does not.
pub fn engines(home: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(home.join("engines")) else { return Vec::new() };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    // The filesystem's order is not part of any contract; the dropdown is
    // easier to use when it does not move between machines.
    names.sort();
    names
}

/// The tail of the last job's log, for the console's job pane.
///
/// Empty when nothing has ever run, which is how the page decides whether to
/// show the pane at all.
pub fn job_tail(home: &Path, lines: usize) -> String {
    let Ok(text) = std::fs::read_to_string(home.join("logs/web-job.log")) else {
        return String::new();
    };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// GPU memory in use, from nvidia-smi. `None` when there is no NVIDIA card
/// or the tool is not there - the page simply omits it.
pub fn vram_mib() -> Option<u64> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).lines().next()?.trim().parse().ok()
}

/// Free physical memory in GB, or `None` when it cannot be read.
///
/// `/proc/meminfo` on Linux, `Win32_OperatingSystem` on Windows - the two
/// places `scripts/record-result.sh` reads, so a row and the screen agree
/// about what the machine had free.
///
/// Both fallbacks matter and neither is theoretical. MSYS's `/proc/meminfo`
/// exists but is a stub with no `MemAvailable` line at all, so on Windows
/// the file is not the answer; and PowerShell prints numbers in the
/// machine's own locale, where this box's separator is a comma - which
/// `f64::from_str` rejects, so an unguarded parse reads a working counter as
/// "no answer" and the screen says 0.0 GB free on a machine with 20 GB.
pub fn ram_free_gb() -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                if let Some(kb) = rest.split_whitespace().next().and_then(|v| v.parse::<f64>().ok())
                {
                    return Some(kb / 1_048_576.0);
                }
            }
        }
    }
    let out = std::process::Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-Command",
            "[math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory/1MB,1)",
        ])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().replace(',', ".").parse().ok()
}

/// Whether a console answers on `port`.
///
/// A connection that is refused or times out is "down", which is what the
/// screen's `web` segment is for: the console is usually the thing you are
/// looking at, so the interesting case is when it is not.
pub fn web_up(port: u16) -> bool {
    let endpoint = engine::Endpoint { port, api_key: String::new() };
    engine::get(&endpoint, "/api/state").is_some()
}

/// A job started from the console: one at a time, like the shell's.
#[derive(Debug, Default)]
pub struct Job {
    running: bool,
    action: String,
}

impl Job {
    pub fn running(&self) -> bool {
        self.running
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    /// Mark `action` as running, unless something already is. Returns false
    /// when a job was already in flight - one engine, one measurement.
    pub fn begin(&mut self, action: &str) -> bool {
        if self.running {
            return false;
        }
        self.running = true;
        self.action = action.to_string();
        true
    }

    pub fn finish(&mut self) {
        self.running = false;
    }
}

pub type SharedJob = Arc<Mutex<Job>>;

/// Everything `api/state` answers with.
pub struct Snapshot {
    pub up: bool,
    pub vram_mib: Option<u64>,
    pub alias: String,
    pub engine: String,
    pub presets: Vec<String>,
    /// The installed engine adapters, for the page's engine dropdown.
    pub engines: Vec<String>,
    pub job_running: bool,
    pub job_action: String,
    /// The running job's log tail; empty when nothing has run.
    pub job_tail: String,
}

pub fn snapshot(home: &Path, job: &SharedJob) -> Snapshot {
    let status = engine::status(home);
    let (job_running, job_action) = match job.lock() {
        Ok(job) => (job.running(), job.action().to_string()),
        Err(_) => (false, String::new()),
    };
    Snapshot {
        up: status.up,
        vram_mib: if status.up { vram_mib() } else { None },
        alias: env_value(home, "MODEL_ALIAS").unwrap_or_default(),
        engine: env_value(home, "ENGINE").unwrap_or_else(|| "llama.cpp".to_string()),
        presets: presets(home),
        engines: engines(home),
        job_running,
        job_action,
        // The page shows the pane whenever there is anything in it, the same
        // rule the shell console uses: a finished job's log is still worth
        // reading, and that is usually the one you came back for.
        job_tail: job_tail(home, 14),
    }
}

/// The tail of the engine's log, for `api/logs`.
pub fn log_tail(home: &Path, lines: usize) -> (String, String) {
    let candidates = [
        home.join("logs/llama-server-windows.log"),
        home.join("logs/local-ai.log"),
        home.join("logs/web-job.log"),
    ];
    for path in candidates {
        if let Ok(text) = std::fs::read_to_string(&path) {
            let all: Vec<&str> = text.lines().collect();
            let start = all.len().saturating_sub(lines);
            return (
                path.strip_prefix(home).unwrap_or(&path).to_string_lossy().into_owned(),
                all[start..].join("\n"),
            );
        }
    }
    (String::new(), String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-state-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("config")).expect("temp");
        dir
    }

    #[test]
    fn reads_env_values_without_their_quotes() {        let dir = home("env");
        std::fs::write(dir.join(".env"), "MODEL_ALIAS=\"moe-35b\"\n# ENGINE=x\nENGINE=llama.cpp\n")
            .expect("write");
        assert_eq!(env_value(&dir, "MODEL_ALIAS").as_deref(), Some("moe-35b"));
        assert_eq!(env_value(&dir, "ENGINE").as_deref(), Some("llama.cpp"));
        assert_eq!(env_value(&dir, "NOPE"), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lists_presets_but_not_the_shared_section() {
        let dir = home("presets");
        std::fs::write(
            dir.join("config/models.ini"),
            "version = 1\n[*]\njinja = true\n[a]\nmodel = a.gguf\n[b]\nmodel = b.gguf\n",
        )
        .expect("write");
        assert_eq!(presets(&dir), vec!["a".to_string(), "b".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setting_a_value_replaces_the_line_that_holds_it() {
        let dir = home("env-set");
        std::fs::write(
            dir.join(".env"),
            "LOCAL_AI_PORT=\"8000\"\nMODEL_ALIAS=\"old\"\n# MODEL_ALIAS=\"commented\"\n",
        )
        .expect("write");
        env_set(&dir, "MODEL_ALIAS", "ornith-1.5-35b").expect("write");
        let text = std::fs::read_to_string(dir.join(".env")).expect("read");
        assert_eq!(
            text,
            "LOCAL_AI_PORT=\"8000\"\nMODEL_ALIAS=\"ornith-1.5-35b\"\nMODEL_ALIAS=\"ornith-1.5-35b\"\n"
        );
        // The value reads back the way the rest of the console reads it.
        assert_eq!(env_value(&dir, "MODEL_ALIAS").as_deref(), Some("ornith-1.5-35b"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setting_a_key_the_file_does_not_have_appends_it() {
        let dir = home("env-add");
        std::fs::write(dir.join(".env"), "ENGINE=\"llama.cpp\"\n").expect("write");
        env_set(&dir, "MODEL_ALIAS", "tiny").expect("write");
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).expect("read"),
            "ENGINE=\"llama.cpp\"\nMODEL_ALIAS=\"tiny\"\n"
        );
        // A `.env` that is not there yet gets one, rather than an error.
        let fresh = home("env-new");
        env_set(&fresh, "MODEL_ALIAS", "tiny").expect("write");
        assert_eq!(env_value(&fresh, "MODEL_ALIAS").as_deref(), Some("tiny"));
        std::fs::remove_dir_all(&fresh).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn adopting_replaces_an_active_line_and_leaves_a_commented_one_alone() {
        let dir = home("env-adopt");
        std::fs::write(
            dir.join(".env"),
            "ENGINE=\"llama.cpp\"\n#MODELS_PRESET=\"config/models.ini\"\n",
        )
        .expect("write");
        env_adopt(&dir, "ENGINE", "ik_llama.cpp").expect("write");
        env_adopt(&dir, "MODELS_PRESET", "data/tuned/models.effective.ini").expect("write");
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).expect("read"),
            "ENGINE=\"ik_llama.cpp\"\n\
             #MODELS_PRESET=\"config/models.ini\"\n\
             MODELS_PRESET=\"data/tuned/models.effective.ini\"\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setting_a_value_keeps_the_line_ending_the_file_uses() {
        let dir = home("env-crlf");
        std::fs::write(dir.join(".env"), "ENGINE=\"x\"\r\n").expect("write");
        env_set(&dir, "MODEL_ALIAS", "tiny").expect("write");
        let text = std::fs::read_to_string(dir.join(".env")).expect("read");
        assert_eq!(text, "ENGINE=\"x\"\r\nMODEL_ALIAS=\"tiny\"\r\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_preset_file_lists_nothing() {
        let dir = home("missing");
        assert!(presets(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn engines_are_the_directories_under_engines() {
        let dir = home("engines");
        std::fs::create_dir_all(dir.join("engines/llama.cpp")).expect("mkdir");
        std::fs::create_dir_all(dir.join("engines/ik_llama.cpp")).expect("mkdir");
        std::fs::write(dir.join("engines/notes.txt"), "x").expect("write");
        assert_eq!(engines(&dir), vec!["ik_llama.cpp".to_string(), "llama.cpp".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_engines_folder_is_an_empty_list_not_an_error() {
        let dir = home("no-engines");
        assert!(engines(&dir).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_job_tail_is_empty_until_something_has_run() {
        let dir = home("job-tail");
        assert_eq!(job_tail(&dir, 14), "");
        std::fs::create_dir_all(dir.join("logs")).expect("logs");
        std::fs::write(dir.join("logs/web-job.log"), "a\nb\nc\n").expect("write");
        assert_eq!(job_tail(&dir, 2), "b\nc");
        assert_eq!(job_tail(&dir, 99), "a\nb\nc");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_job_at_a_time() {
        let mut job = Job::default();
        assert!(job.begin("bench"), "the first takes the slot");
        assert!(!job.begin("eval"), "the second is refused");
        assert_eq!(job.action(), "bench");
        job.finish();
        assert!(job.begin("eval"), "after finishing, the slot is free");
    }

    #[test]
    fn the_log_tail_is_the_last_lines_of_whichever_log_exists() {
        let dir = home("log");
        std::fs::create_dir_all(dir.join("logs")).expect("logs");
        std::fs::write(dir.join("logs/llama-server-windows.log"), "one\ntwo\nthree\n").expect("write");
        let (file, text) = log_tail(&dir, 2);
        assert_eq!(file, "logs/llama-server-windows.log");
        assert_eq!(text, "two\nthree");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_value_comes_from_the_toml_file_when_it_has_one() {
        let dir = home("config-first");
        std::fs::write(dir.join(".env"), "MODEL_ALIAS=\"from-env\"\nLOCAL_AI_PORT=\"8000\"\n")
            .expect("write");
        std::fs::write(
            dir.join("golem.toml"),
            "# the binary's own surface\nTUNE_RUNS = 5\nTEMPERATURE = 0.2\n",
        )
        .expect("write");
        // The key the TOML file owns answers from there...
        assert_eq!(env_value(&dir, "TUNE_RUNS").as_deref(), Some("5"));
        assert_eq!(env_value(&dir, "TEMPERATURE").as_deref(), Some("0.2"));
        // ...and every key it does not own still comes from the shim.
        assert_eq!(env_value(&dir, "MODEL_ALIAS").as_deref(), Some("from-env"));
        assert_eq!(env_value(&dir, "LOCAL_AI_PORT").as_deref(), Some("8000"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_toml_file_leaves_the_env_file_in_charge() {
        // The ordinary case on a checkout that predates the split.
        let dir = home("config-none");
        std::fs::write(dir.join(".env"), "TUNE_RUNS=\"7\"\n").expect("write");
        assert_eq!(env_value(&dir, "TUNE_RUNS").as_deref(), Some("7"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn setting_a_toml_owned_key_writes_there_and_keeps_the_comments() {
        // Writing `.env` for a key the reader takes from the TOML file would
        // look like it worked and change nothing.
        let dir = home("config-write");
        std::fs::write(dir.join(".env"), "TUNE_RUNS=\"3\"\n").expect("write");
        std::fs::write(
            dir.join("golem.toml"),
            "# the floors\nTUNE_RUNS = 3\nTUNE_MIN_DECODE = 25\n",
        )
        .expect("write");
        env_set(&dir, "TUNE_RUNS", "9").expect("write");
        let toml = std::fs::read_to_string(dir.join("golem.toml")).expect("read");
        assert!(toml.contains("# the floors"), "{toml}");
        assert!(toml.contains("TUNE_RUNS = 9"), "{toml}");
        assert!(toml.contains("TUNE_MIN_DECODE = 25"), "the neighbour is untouched: {toml}");
        // And the `.env` line it shadowed is left exactly as it was.
        assert_eq!(
            std::fs::read_to_string(dir.join(".env")).expect("read"),
            "TUNE_RUNS=\"3\"\n"
        );
        assert_eq!(env_value(&dir, "TUNE_RUNS").as_deref(), Some("9"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_deploy_mode_comes_from_the_env_file_and_defaults_to_native() {
        let dir = home("mode");
        // No `.env` at all: native, which is what the shell defaults to.
        assert_eq!(deploy_mode(&dir), "native");

        std::fs::write(dir.join(".env"), "DEPLOY_MODE=\"windows\"\n").expect("write");
        assert_eq!(deploy_mode(&dir), "windows");

        // An empty assignment is unset here too.
        std::fs::write(dir.join(".env"), "DEPLOY_MODE=\n").expect("write");
        assert_eq!(deploy_mode(&dir), "native");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_key_neither_file_owns_is_written_to_the_env_file() {
        let dir = home("config-write-env");
        std::fs::write(dir.join("golem.toml"), "TUNE_RUNS = 3\n").expect("write");
        env_set(&dir, "MODEL_ALIAS", "ornith-1.5-35b").expect("write");
        assert_eq!(env_value(&dir, "MODEL_ALIAS").as_deref(), Some("ornith-1.5-35b"));
        let env = std::fs::read_to_string(dir.join(".env")).expect("read");
        assert!(env.contains("MODEL_ALIAS=\"ornith-1.5-35b\""), "{env}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
