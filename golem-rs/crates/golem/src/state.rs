//! What the console shows: the engine's state, the installed presets, the
//! log, and the one job that may be running.
//!
//! These are read from the same places the shell console reads them - the
//! home's `.env`, its preset file, the engine's own HTTP endpoints, and
//! nvidia-smi for the GPU - so the Rust console is a drop-in for the page
//! the scripts serve, not a second interpretation of the same facts.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::engine;

/// One value from the home's `.env`, quotes stripped.
pub fn env_value(home: &Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(home.join(".env")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            if k.trim() == key {
                return Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
            }
        }
    }
    None
}

/// The preset file, as the home refers to it.
pub fn preset_path(home: &Path) -> PathBuf {
    let configured = env_value(home, "MODELS_PRESET").unwrap_or_else(|| "config/models.ini".to_string());
    home.join(configured)
}

/// The preset sections, in file order, without the shared `[*]` one.
pub fn presets(home: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(preset_path(home)) else { return Vec::new() };
    let mut names = Vec::new();
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix('[') {
            if let Some(name) = rest.split(']').next() {
                let name = name.trim();
                if !name.is_empty() && name != "*" {
                    names.push(name.to_string());
                }
            }
        }
    }
    names
}

/// One preset section's `key = value` pairs.
pub fn preset_section(home: &Path, section: &str) -> Vec<(String, String)> {
    let Ok(text) = std::fs::read_to_string(preset_path(home)) else { return Vec::new() };
    let mut out = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix('[') {
            let name = rest.split(']').next().unwrap_or("").trim();
            inside = name == section;
            continue;
        }
        if !inside || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
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
pub fn ram_free_gb() -> Option<f64> {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemAvailable:") {
                let kb: f64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kb / 1_048_576.0);
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
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
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
    started: Option<Instant>,
}

impl Job {
    pub fn running(&self) -> bool {
        self.running
    }

    pub fn action(&self) -> &str {
        &self.action
    }

    pub fn started(&self) -> Option<Instant> {
        self.started
    }

    /// Mark `action` as running, unless something already is. Returns false
    /// when a job was already in flight - one engine, one measurement.
    pub fn begin(&mut self, action: &str) -> bool {
        if self.running {
            return false;
        }
        self.running = true;
        self.action = action.to_string();
        self.started = Some(Instant::now());
        true
    }

    pub fn finish(&mut self) {
        self.running = false;
        self.started = None;
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
    fn reads_env_values_without_their_quotes() {
        let dir = home("env");
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
        let section = preset_section(&dir, "b");
        assert_eq!(section, vec![("model".to_string(), "b.gguf".to_string())]);
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
}
