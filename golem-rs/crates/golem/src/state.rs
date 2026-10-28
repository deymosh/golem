//! What the console shows: the engine's state, the installed presets, the
//! log, and the one job that may be running - read from `golem.toml`, the
//! preset file, the engine's own HTTP endpoints, and the card.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use golem_core::ini;
use golem_core::progress::{self, Progress};

use crate::config;
use crate::engine;
use crate::machine;
use crate::paths;

/// The preset file the engine serves from: `MODELS_PRESET`, or the
/// repository's own `config/models.ini` when the configuration names none.
pub fn preset_path(home: &Path) -> PathBuf {
    home.join(config::models_preset(home, &config::Overrides::default()))
}

/// The preset sections, in file order, without the shared `[*]` one.
///
/// The parsing itself lives in `golem_core::ini`, which is what the console's
/// Models tab and the tuner read the same file with; a second scanner here
/// would be a second answer to "what is a section".
pub fn presets(home: &Path) -> Vec<String> {
    ini::Document::read(&preset_path(home)).map(|doc| doc.preset_names()).unwrap_or_default()
}

/// Point the setup at another preset (`alias`) or engine (`engine`), by
/// rewriting golem.toml: `applied`, or `bad-input` for a name that is not
/// installed - the same lists the console's choices and the screen offer, so
/// a hand-made request cannot write a name that would not start.
pub fn apply(home: &Path, setting: &str, value: &str) -> std::io::Result<&'static str> {
    let (key, installed) = match setting {
        "alias" => ("MODEL_ALIAS", presets(home)),
        "engine" => ("ENGINE", engines(home)),
        _ => return Ok("bad-input"),
    };
    if !installed.iter().any(|name| name == value) {
        return Ok("bad-input");
    }
    config::set(home, key, value).map(|()| "applied")
}

/// The engine adapters installed under `engines/`, by directory name.
///
/// This is what the console's engine dropdown offers, so it lists what is
/// actually installed rather than what the code knows about: an adapter
/// dropped into the folder appears without a rebuild, and one that was never
/// fetched does not. The default engine is always there: its adapter is
/// compiled in (`paths::adapter`).
pub fn engines(home: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(home.join("engines"))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    if !names.iter().any(|n| n == paths::DEFAULT_ENGINE) {
        names.push(paths::DEFAULT_ENGINE.to_string());
    }
    // The filesystem's order is not part of any contract; the dropdown is
    // easier to use when it does not move between machines.
    names.sort();
    names
}

/// The tail of the last job's log, for the console's job pane, without its
/// `PROGRESS` lines - those are the bar's ([`job_progress`]).
///
/// Empty when nothing has ever run, which is how the page decides whether to
/// show the pane at all.
pub fn job_tail(home: &Path, lines: usize) -> String {
    let Some(text) = read_end(&paths::job_log(home), TAIL_BYTES) else {
        return String::new();
    };
    let all: Vec<&str> = text.lines().filter(|line| !progress::is_line(line)).collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// How much of a log's end the tails read. The console and the screen ask
/// several times a second, and the logs keep every job and every engine
/// start there has been, so a tail never reads the whole file.
const TAIL_BYTES: u64 = 256 * 1024;

/// The last `bytes` of a file as text, starting at a line: the cut lands
/// mid-line (or mid-character), so what precedes the first newline is
/// dropped unless the whole file fit.
fn read_end(path: &Path, bytes: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let from = len.saturating_sub(bytes);
    file.seek(SeekFrom::Start(from)).ok()?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Some(match (from, text.find('\n')) {
        (0, _) | (_, None) => text,
        (_, Some(nl)) => text[nl + 1..].to_string(),
    })
}

/// How far the running job has got, from the newest `PROGRESS` line in the
/// job log, or `None` when it says nothing or has cleared its bar.
pub fn job_progress(home: &Path) -> Option<Progress> {
    progress::latest(&read_end(&paths::job_log(home), 16 * 1024)?)
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

/// A job started from the console: one at a time.
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
    /// The running job's bar, when it has one.
    pub job_progress: Option<Progress>,
}

pub fn snapshot(home: &Path, job: &SharedJob) -> Snapshot {
    let status = engine::status(home);
    let (job_running, job_action) = match job.lock() {
        Ok(job) => (job.running(), job.action().to_string()),
        Err(_) => (false, String::new()),
    };
    Snapshot {
        up: status.up,
        vram_mib: if status.up { machine::vram_used_mib() } else { None },
        alias: config::get(home, "MODEL_ALIAS").unwrap_or_default(),
        engine: config::get(home, "ENGINE").unwrap_or_else(|| paths::DEFAULT_ENGINE.to_string()),
        presets: presets(home),
        engines: engines(home),
        job_running,
        job_action,
        // The page shows the pane whenever there is anything in it: a
        // finished job's log is still worth
        // reading, and that is usually the one you came back for.
        job_tail: job_tail(home, 14),
        job_progress: if job_running { job_progress(home) } else { None },
    }
}

/// The tail of the engine's own log (`logs/llama-server.log`), as the file
/// it came from and its last `lines` lines - for the screen's pane and the
/// console's `api/logs`. Nothing at all when the engine has never run: the
/// job log is not the engine's and a pane that showed it would be wrong.
pub fn log_tail(home: &Path, lines: usize) -> (String, String) {
    let path = paths::engine_log(home);
    let Some(text) = read_end(&path, TAIL_BYTES) else { return (String::new(), String::new()) };
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    (path.strip_prefix(home).unwrap_or(&path).to_string_lossy().into_owned(), all[start..].join("\n"))
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
    fn no_engines_folder_still_lists_the_compiled_in_default() {
        // A release folder has no engines/ at all; the default's adapter is
        // in the binary, so it is still the one engine on offer.
        let dir = home("no-engines");
        assert_eq!(engines(&dir), vec![crate::paths::DEFAULT_ENGINE.to_string()]);
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
    fn progress_lines_are_the_bar_and_not_the_tail() {
        let dir = home("job-progress");
        std::fs::create_dir_all(dir.join("logs")).expect("logs");
        let log = dir.join("logs/web-job.log");
        // A job's log as the runner and a pull write it: the clear, text,
        // the download's lines.
        std::fs::write(&log, "PROGRESS -\n[INFO] Downloading x\nPROGRESS 5/10 bytes pull: x\n").expect("write");
        assert_eq!(job_tail(&dir, 14), "[INFO] Downloading x");
        assert_eq!(job_progress(&dir), Some(Progress::bytes(5, 10, "pull: x")));
        // The job ended: the runner's clear takes the bar away.
        std::fs::write(&log, "PROGRESS 5/10 bytes pull: x\n[ OK ] x downloaded\nPROGRESS -\n").expect("write");
        assert_eq!(job_progress(&dir), None);
        assert_eq!(job_tail(&dir, 14), "[ OK ] x downloaded");
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
    fn the_log_tail_is_the_engines_own_log_and_nothing_else() {
        let dir = home("log");
        std::fs::create_dir_all(dir.join("logs")).expect("logs");
        // A job log, and a stale log from before the rename: neither is the
        // engine's, and a pane that showed them would look like a quiet engine.
        std::fs::write(dir.join("logs/web-job.log"), "job\n").expect("write");
        std::fs::write(dir.join("logs/llama-server-windows.log"), "stale\n").expect("write");
        assert_eq!(log_tail(&dir, 2), (String::new(), String::new()));
        std::fs::write(dir.join("logs/llama-server.log"), "one\ntwo\nthree\n").expect("write");
        let (file, text) = log_tail(&dir, 2);
        assert_eq!(file.replace('\\', "/"), "logs/llama-server.log");
        assert_eq!(text, "two\nthree");
        std::fs::remove_dir_all(&dir).ok();
    }
}
