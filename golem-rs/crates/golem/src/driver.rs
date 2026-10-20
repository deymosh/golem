//! Driving the shell launchers.
//!
//! Phase 3 is deliberately thin: the binary does not start engines itself
//! yet, it asks the launchers that already do. On Windows that is
//! `golem.cmd` (no bash needed), everywhere else the `./golem` script. A
//! release that does not carry them says so instead of pretending.
//!
//! Every action runs to a log file and returns its exit code: the console
//! shows the output through `api/logs`, and a CLI caller gets the code.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The actions the launchers understand, in the order the console offers
/// them.
pub const ACTIONS: [&str; 8] =
    ["up", "down", "restart", "status", "bench", "eval", "tune", "report"];

/// A launcher, ready to run.
#[derive(Debug, Clone, PartialEq)]
pub struct Driver {
    pub program: PathBuf,
    /// Arguments that always come first (`/c <path>` for cmd.exe, the script
    /// path for bash).
    pub prefix: Vec<String>,
}

impl Driver {
    /// The launcher in `home`, if this folder has one.
    ///
    /// Windows prefers `golem.cmd`: it drives everything through PowerShell
    /// and needs no shell of its own. The bash script is the fallback there
    /// (a Git Bash user) and the normal case elsewhere.
    pub fn find(home: &Path) -> Option<Driver> {
        let cmd = home.join("golem.cmd");
        if cfg!(windows) && cmd.is_file() {
            return Some(Driver {
                program: PathBuf::from("cmd.exe"),
                prefix: vec!["/c".to_string(), cmd.to_string_lossy().into_owned()],
            });
        }
        let script = home.join("golem");
        if script.is_file() {
            return Some(Driver {
                program: PathBuf::from("bash"),
                prefix: vec![script.to_string_lossy().into_owned()],
            });
        }
        None
    }

    /// A command for one action.
    pub fn command(&self, action: &str) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.prefix).arg(action);
        command
    }
}

/// Whether `action` is one the launchers know.
pub fn is_action(action: &str) -> bool {
    ACTIONS.contains(&action)
}

/// Run an action to completion, with everything it prints going to `log`.
///
/// Returns the launcher's exit code. A launcher that could not be started at
/// all is an error; a launcher that ran and failed is simply a non-zero code,
/// because its output - which the console shows - is the useful part.
pub fn run(driver: &Driver, action: &str, log: &Path) -> std::io::Result<i32> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::File::create(log)?;
    let err = out.try_clone()?;
    let status = driver
        .command(action)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .status()?;
    Ok(status.code().unwrap_or(-1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_nothing_in_an_empty_folder() {
        let dir = std::env::temp_dir().join("golem-rs-driver-empty");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        assert!(Driver::find(&dir).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn finds_the_shell_script_and_builds_a_command() {
        let dir = std::env::temp_dir().join("golem-rs-driver-script");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        std::fs::write(dir.join("golem"), "#!/usr/bin/env bash\n").expect("write");
        let driver = Driver::find(&dir).expect("the script is there");
        let command = driver.command("status");
        let args: Vec<_> = command.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert!(args.last().is_some_and(|a| a == "status"), "the action is last: {args:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_known_actions_are_accepted() {
        assert!(is_action("up"));
        assert!(is_action("tune"));
        assert!(!is_action("rm -rf"));
        assert!(!is_action(""));
    }
}
