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
//! One file is not created from nothing: the page template is compiled into
//! the binary, and written into the home the first time it is needed. An
//! existing template is never overwritten - a developer editing it expects
//! the edit to win over the embedded copy.

use std::path::{Path, PathBuf};

/// The page template, compiled in so a release is a single file and still
/// has a console. `scripts/report.sh` has the same file in the repository;
/// this copy is what a downloaded binary uses.
const EMBEDDED_TEMPLATE: &str = include_str!("../../../../bench/report.template.html");

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
    let template = template(home);
    if !template.is_file() {
        std::fs::write(&template, EMBEDDED_TEMPLATE)?;
    }
    Ok(())
}

/// `bench/results.jsonl` under the home.
pub fn results(home: &Path) -> PathBuf {
    home.join("bench").join("results.jsonl")
}

/// `bench/report.template.html` under the home (written by [`ensure`] when
/// it is missing).
pub fn template(home: &Path) -> PathBuf {
    home.join("bench").join("report.template.html")
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
    fn ensure_writes_the_embedded_template_once() {
        let dir = std::env::temp_dir().join("golem-rs-paths-template-test");
        let _ = std::fs::remove_dir_all(&dir);
        ensure(&dir).expect("first run");
        let written = std::fs::read_to_string(template(&dir)).expect("template");
        assert_eq!(written, EMBEDDED_TEMPLATE);
        assert!(written.contains("/*GOLEM_DATA*/"), "the page's data marker");

        // A developer's edit must win over the embedded copy.
        std::fs::write(template(&dir), "edited").expect("edit");
        ensure(&dir).expect("second run");
        assert_eq!(std::fs::read_to_string(template(&dir)).expect("read"), "edited");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_paths_hang_off_the_home() {
        let home = Path::new("/tmp/golem-home");
        assert_eq!(results(home), home.join("bench/results.jsonl"));
        assert_eq!(template(home), home.join("bench/report.template.html"));
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
