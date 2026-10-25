//! Where the binary keeps and finds its files.
//!
//! A release is one `golem.exe` someone downloaded: there is no checkout
//! around it, so nothing about the repository layout can be assumed. The
//! rule is one sentence long:
//!
//! **The home is `GOLEM_HOME` if set, otherwise the directory the binary
//! lives in.** Everything the binary needs lives under it, and everything it
//! needs is created on first run if it is missing - so a dropped-in release
//! populates its own folder, and a dev build at the repository root (where
//! `scripts/cargo.sh` puts it) uses the checkout's own `bench/`, `data/` and
//! `logs/` by virtue of sitting next to them.
//!
//! The page (the console and `golem report`) is compiled into the binary and
//! never written into the home: it is the binary's own, so a copy in the
//! home would keep serving an old page after an upgrade. In the checkout it
//! is `bench/report.template.html`, which the build compiles in.

use std::path::{Path, PathBuf};

/// The page, compiled in: the console and `golem report` both render it, in
/// a release and in the checkout alike.
pub const PAGE: &str = include_str!("../../../../bench/report.template.html");

/// The configuration template, compiled in for the same reason: a release
/// folder has no `golem.toml.example`, and without one every key would fall
/// back to the code's defaults - which work, but leave the user with no file
/// to read the settings out of or edit them in.
///
/// The template is what the binary writes as `golem.toml` on first run, and
/// `golem.toml` is the machine's own file: it is git-ignored, like `.local/`,
/// because it holds the port, the key and the model this machine serves.
const EMBEDDED_CONFIG: &str = include_str!("../../../../golem.toml.example");

/// The library (`config/library.ini`), compiled in: the curated models are
/// part of what a release knows. A home's own copy takes its place
/// ([`library`]).
pub const EMBEDDED_LIBRARY: &str = include_str!("../../../../config/library.ini");

/// The library this home uses: its own `config/library.ini` when there is
/// one, else the compiled-in copy.
pub fn library(home: &Path) -> Vec<golem_core::library::Entry> {
    let own = std::fs::read_to_string(home.join("config/library.ini"));
    golem_core::library::parse(own.as_deref().unwrap_or(EMBEDDED_LIBRARY))
}

/// The directories the binary expects under the home. Created on demand.
const DIRS: [&str; 5] = ["bench", "data/models", "data/cache", "data/tuned", "logs"];

/// The home directory: `GOLEM_HOME`, or the directory holding the binary.
pub fn home() -> PathBuf {
    if let Some(explicit) = std::env::var_os("GOLEM_HOME") {
        return PathBuf::from(explicit);
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Create what the binary needs under the home, without touching what is
/// already there.
///
/// Idempotent and quiet: a checkout has all of it, a release folder has none
/// of it, and the same call serves both. Returns the first failure so a
/// permissions problem is reported now instead of surfacing later as a
/// confusing "cannot read".
pub fn ensure(home: &Path) -> std::io::Result<()> {
    for dir in DIRS {
        std::fs::create_dir_all(home.join(dir))?;
    }
    let config = home.join("golem.toml");
    if !config.is_file() {
        std::fs::write(&config, EMBEDDED_CONFIG)?;
    }
    Ok(())
}

/// The preset file the repository ships, relative to the home: what a home
/// serves when `MODELS_PRESET` names nothing, and what the tuner tunes from.
pub const BASE_PRESET: &str = "config/models.ini";

/// The preset file the tuner writes, relative to the home: the base with this
/// machine's gated overlays merged in. `tune --adopt` points
/// `MODELS_PRESET` at it.
pub const EFFECTIVE_PRESET: &str = "data/tuned/models.effective.ini";

/// The engine adapter used when `ENGINE` names none.
pub const DEFAULT_ENGINE: &str = "llama.cpp";

/// `bench/results.jsonl` under the home.
pub fn results(home: &Path) -> PathBuf {
    home.join("bench").join("results.jsonl")
}

/// Where the GGUF files live, and the directory the engine starts in.
pub fn models_dir(home: &Path) -> PathBuf {
    home.join("data").join("models")
}

/// The engine's own log: it writes it itself (`--log-file`).
pub fn engine_log(home: &Path) -> PathBuf {
    home.join("logs").join("llama-server.log")
}

/// The console's job log: what the last action from the page or the screen
/// printed, which the job pane tails.
pub fn job_log(home: &Path) -> PathBuf {
    home.join("logs").join("web-job.log")
}

/// The running engine's pid.
pub fn pid_file(home: &Path) -> PathBuf {
    home.join(".local").join("llama-server.pid")
}

/// Why the service stopped the engine after a model failed to load, for
/// `status` to say until the next start clears it.
pub fn load_failure(home: &Path) -> PathBuf {
    home.join(".local").join("load-failure.txt")
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_creates_every_directory_once() {
        let dir = std::env::temp_dir().join("golem-rs-paths-test");
        let _ = std::fs::remove_dir_all(&dir);
        ensure(&dir).expect("first run creates");
        ensure(&dir).expect("second run is quiet");
        for sub in DIRS {
            assert!(dir.join(sub).is_dir(), "{sub} should exist");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ensure_writes_the_embedded_config_once() {
        // A release folder has no `golem.toml`; without one the moved keys
        // fall back to the code's defaults and the user has no file to read
        // them out of.
        let dir = std::env::temp_dir().join("golem-rs-paths-config-test");
        let _ = std::fs::remove_dir_all(&dir);
        ensure(&dir).expect("first run");
        let written = std::fs::read_to_string(dir.join("golem.toml")).expect("config");
        assert_eq!(written, EMBEDDED_CONFIG);
        // And it parses as what the binary reads: the comments are the point.
        assert!(written.contains("TUNE_MIN_DECODE"), "the tuner's floors are in it");
        golem_core::config::Document::parse(&written).expect("the embedded config parses");

        // A user's edit must win over the embedded copy.
        std::fs::write(dir.join("golem.toml"), "TUNE_RUNS = 9\n").expect("edit");
        ensure(&dir).expect("second run");
        assert_eq!(
            std::fs::read_to_string(dir.join("golem.toml")).expect("read"),
            "TUNE_RUNS = 9\n"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_page_is_the_binarys_and_never_written_into_the_home() {
        let dir = std::env::temp_dir().join("golem-rs-paths-template-test");
        let _ = std::fs::remove_dir_all(&dir);
        ensure(&dir).expect("first run");
        assert!(!dir.join("bench/report.template.html").exists());
        assert!(PAGE.contains("/*GOLEM_DATA*/"), "the page's data marker");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_paths_hang_off_the_home() {
        let home = Path::new("/tmp/golem-home");
        assert_eq!(results(home), home.join("bench/results.jsonl"));
        assert_eq!(engine_log(home), home.join("logs").join("llama-server.log"));
        assert_eq!(pid_file(home), home.join(".local").join("llama-server.pid"));
    }

    #[test]
    fn an_explicit_home_wins() {
        let dir = std::env::temp_dir().join("golem-rs-explicit-home");
        std::fs::create_dir_all(&dir).expect("temp");
        std::env::set_var("GOLEM_HOME", &dir);
        assert_eq!(home(), dir);
        std::env::remove_var("GOLEM_HOME");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_an_override_the_home_is_where_the_binary_lives() {
        let expected = std::env::current_exe()
            .expect("the test binary knows its path")
            .parent()
            .expect("a directory")
            .to_path_buf();
        std::env::remove_var("GOLEM_HOME");
        assert_eq!(home(), expected);
    }
}
