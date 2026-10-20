//! Driving the shell launchers.
//!
//! Phase 3 is deliberately thin: the binary does not start engines itself
//! yet, it asks the launchers that already do. On Windows that is
//! `golem.cmd` (no bash needed), everywhere else the `./golem` script. A
//! release that does not carry them says so instead of pretending.
//!
//! Every action runs to a log file and returns its exit code: the console
//! shows the output through `api/logs`, and a CLI caller gets the code.
//!
//! Two vocabularies meet here and must not be confused. The *console's* is
//! the page's - it posts `action=start`, and its Eval button means the 200K
//! gate. The *launchers'* differ from each other: `golem.cmd` says
//! `start`/`stop`, `./golem` says `up`/`down`. [`Driver::verb`] is the one
//! place the translation happens, because getting it wrong is silent - the
//! launcher prints its usage and exits 1, which reads as "the engine did not
//! start" rather than "that is not a verb".

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The actions the console offers, in the order it offers them: the page's
/// own names, which is what `POST /api/run` is handed.
pub const ACTIONS: [&str; 9] =
    ["start", "stop", "restart", "status", "bench", "eval", "tune", "report", "pull"];

/// The actions the console answers itself, without a launcher: they rewrite
/// `.env` rather than start anything, and the shell console replies
/// "applied" in-process for both.
pub const SETTINGS: [&str; 2] = ["alias", "engine"];

/// Which launcher a [`Driver`] drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Shell {
    /// `golem.cmd`, through `cmd.exe`.
    Cmd,
    /// The `./golem` bash script.
    Bash,
}

/// A launcher, ready to run.
#[derive(Debug, Clone, PartialEq)]
pub struct Driver {
    pub program: PathBuf,
    /// Arguments that always come first (`/c <path>` for cmd.exe, the script
    /// path for bash).
    pub prefix: Vec<String>,
    pub shell: Shell,
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
                shell: Shell::Cmd,
            });
        }
        let script = home.join("golem");
        if script.is_file() {
            return Some(Driver {
                program: bash(),
                prefix: vec![script.to_string_lossy().into_owned()],
                shell: Shell::Bash,
            });
        }
        None
    }

    /// The launcher's own word for an action.
    pub fn verb<'a>(&self, action: &'a str) -> &'a str {
        match (self.shell, action) {
            (Shell::Bash, "start") => "up",
            (Shell::Bash, "stop") => "down",
            (Shell::Cmd, "up") => "start",
            (Shell::Cmd, "down") => "stop",
            (_, other) => other,
        }
    }

    /// A command for one action, with the arguments that action means.
    ///
    /// `arg` is the page's `arg` field: a preset name for `tune` and `pull`,
    /// ignored by everything else.
    pub fn command(&self, action: &str, arg: &str) -> Command {
        self.command_with(action, &self.console_args(action, arg))
    }

    /// A command for one action with exactly these arguments.
    ///
    /// The tuner's caller, which knows the run it wants - `eval --long
    /// 87381`, `tune --runs 1` - rather than the page's one-field form.
    pub fn command_with(&self, action: &str, args: &[String]) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.prefix).arg(self.verb(action)).args(args);
        command
    }

    /// The arguments the console's button for `action` carries.
    fn console_args(&self, action: &str, arg: &str) -> Vec<String> {
        match action {
            // The button says "Eval 200K" and the shell's own TUI entry is
            // `eval --long 200000`: the console means the full-context gate,
            // not whatever a bare `./golem eval` would default to.
            "eval" => vec!["--long".to_string(), "200000".to_string()],
            // Both name a preset; with no name each falls back to the
            // default model, which is what an empty field means.
            "tune" | "pull" if !arg.is_empty() => vec![arg.to_string()],
            _ => Vec::new(),
        }
    }
}

/// The bash to run a shell script with.
///
/// On Windows this must not be a bare `"bash"`. `C:\Windows\System32\bash.exe`
/// is the WSL launcher, and it wins the PATH race for a process that is not
/// already inside Git Bash - so the script runs *inside the Linux VM*, where
/// the engine listening on the Windows side is not on localhost at all and a
/// `C:\` path does not exist. The failure is silent and looks like a broken
/// script: the tuner's own bench reported "server not responding" while the
/// server answered every 200 ms from outside.
///
/// `scripts/golem-bash.cmd` learned this first and looks for Git's own bash
/// before trusting PATH; this is that search, for every script the binary
/// runs rather than only the launcher.
pub fn bash() -> PathBuf {
    if !cfg!(windows) {
        return PathBuf::from("bash");
    }
    for root in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
        let Some(base) = std::env::var_os(root) else { continue };
        let mut candidate = PathBuf::from(base);
        if root == "LOCALAPPDATA" {
            candidate.push("Programs");
        }
        candidate.push("Git/bin/bash.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    // A Git installed somewhere else: the first `bash` on PATH that really
    // comes from a Git installation.
    if let Ok(out) = Command::new("where").arg("bash").output() {
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let candidate = PathBuf::from(line.trim());
            if line.to_lowercase().contains("\\git\\") && candidate.is_file() {
                return candidate;
            }
        }
    }
    PathBuf::from("bash")
}

/// Whether `action` is one the launchers know.
///
/// `up` and `down` are accepted as older spellings of `start` and `stop`
/// (the CLI's own words, and what this binary's first console used);
/// [`Driver::verb`] translates them per launcher.
pub fn is_action(action: &str) -> bool {
    ACTIONS.contains(&action) || matches!(action, "up" | "down")
}

/// Whether `action` is one the console answers without a launcher.
pub fn is_setting(action: &str) -> bool {
    SETTINGS.contains(&action)
}

/// Run an action to completion, with everything it prints going to `log`.
///
/// Returns the launcher's exit code. A launcher that could not be started at
/// all is an error; a launcher that ran and failed is simply a non-zero code,
/// because its output - which the console shows - is the useful part.
pub fn run(driver: &Driver, action: &str, arg: &str, log: &Path) -> std::io::Result<i32> {
    run_command(driver.command(action, arg), log)
}

/// Run any prepared command, with its output going to `log`.
///
/// The tuner measures through commands that are not launcher verbs at all -
/// `scripts/benchmark-ai.sh`, `scripts/record-result.sh` - and every one of
/// them wants the same treatment: no terminal, output to a file, an exit
/// code back.
pub fn run_command(mut command: Command, log: &Path) -> std::io::Result<i32> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::File::create(log)?;
    let err = out.try_clone()?;
    let status = command
        .stdin(Stdio::null())
        .stdout(Stdio::from(out))
        .stderr(Stdio::from(err))
        .status()?;
    Ok(status.code().unwrap_or(-1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launcher(shell: Shell) -> Driver {
        Driver { program: PathBuf::from("launcher"), prefix: vec!["pre".to_string()], shell }
    }

    fn args(driver: &Driver, action: &str, arg: &str) -> Vec<String> {
        driver
            .command(action, arg)
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

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
        assert_eq!(driver.shell, Shell::Bash);
        let built = args(&driver, "status", "");
        assert!(built.last().is_some_and(|a| a == "status"), "the action is last: {built:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_known_actions_are_accepted() {
        assert!(is_action("start"));
        assert!(is_action("pull"));
        assert!(is_action("tune"));
        assert!(is_action("up"), "the CLI's older spelling");
        assert!(!is_action("rm -rf"));
        assert!(!is_action(""));
        // The two that rewrite .env are not launcher verbs.
        assert!(is_setting("alias"));
        assert!(is_setting("engine"));
        assert!(!is_action("alias"));
    }

    #[test]
    fn each_launcher_gets_its_own_word_for_start_and_stop() {
        // The page says start/stop; `./golem` calls them up/down, and
        // golem.cmd does not know up/down at all.
        assert_eq!(launcher(Shell::Bash).verb("start"), "up");
        assert_eq!(launcher(Shell::Bash).verb("stop"), "down");
        assert_eq!(launcher(Shell::Cmd).verb("start"), "start");
        assert_eq!(launcher(Shell::Cmd).verb("stop"), "stop");
        // The CLI's spelling, translated the other way.
        assert_eq!(launcher(Shell::Cmd).verb("up"), "start");
        assert_eq!(launcher(Shell::Bash).verb("up"), "up");
        // Everything else passes through untouched.
        for action in ["restart", "status", "bench", "report"] {
            assert_eq!(launcher(Shell::Cmd).verb(action), action);
            assert_eq!(launcher(Shell::Bash).verb(action), action);
        }
    }

    #[test]
    fn the_eval_button_is_the_full_context_gate() {
        // The page's Eval button carries no argument; the gate is 200K.
        assert_eq!(args(&launcher(Shell::Cmd), "eval", ""), vec!["pre", "eval", "--long", "200000"]);
    }

    #[test]
    fn tune_and_pull_carry_the_preset_they_were_given() {
        assert_eq!(
            args(&launcher(Shell::Bash), "tune", "ornith-1.5-35b"),
            vec!["pre", "tune", "ornith-1.5-35b"]
        );
        assert_eq!(args(&launcher(Shell::Bash), "pull", "tiny"), vec!["pre", "pull", "tiny"]);
        // No name: the launcher's own default, not an empty argument.
        assert_eq!(args(&launcher(Shell::Bash), "tune", ""), vec!["pre", "tune"]);
        // A name is never handed to an action that does not take one.
        assert_eq!(args(&launcher(Shell::Bash), "bench", "sneaky"), vec!["pre", "bench"]);
    }
}
