//! The app's actions: the one path the TUI's menu and the console's job
//! runner run a verb through.
//!
//! `service.rs`'s rule is that the service is the only process that spawns
//! the engine and every verb is a client of it - so that is where the engine
//! verbs go here, the service started when there is none, and the lines it
//! streams back land in the same job log the launcher used to fill. The
//! measuring verbs are this binary's own commands: a child of the app, not
//! of a shell, which is why a folder with nothing but the binary in it can
//! run the whole menu.
//!
//! This is also where the console's vocabulary lives - the actions a page may
//! post and the two it answers in process - because [`plan`] is the same list
//! seen from the other side.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::service;

/// What an action is, decided before anything runs - pure, so the vocabulary
/// and the argument policy are testable without a service or a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The engine verb, as the service knows it: "up", "down", "restart",
    /// "status". (`start`/`stop` are the console's spellings of the first
    /// two, and arrive here translated.)
    Engine(&'static str),
    /// This binary's own command: the verb and the arguments the console's
    /// button for it means.
    Command(Vec<String>),
}

/// The actions the console offers, in the order it offers them: the page's
/// own names, which is what `POST /api/run` is handed.
pub const ACTIONS: [&str; 9] =
    ["start", "stop", "restart", "status", "bench", "eval", "tune", "report", "pull"];

/// The actions the console answers itself, without running a verb: they
/// rewrite the configuration rather than start anything.
pub const SETTINGS: [&str; 2] = ["alias", "engine"];

/// Whether `action` is one the console offers. `up` and `down` are accepted
/// as the CLI's spellings of `start` and `stop`.
pub fn is_action(action: &str) -> bool {
    ACTIONS.contains(&action) || matches!(action, "up" | "down")
}

/// Whether `action` is one the console answers itself.
pub fn is_setting(action: &str) -> bool {
    SETTINGS.contains(&action)
}

/// Decide what `action` is, or `None` when it is no action at all.
pub fn plan(action: &str, arg: &str) -> Option<Plan> {
    match action {
        "start" | "up" => Some(Plan::Engine("up")),
        "stop" | "down" => Some(Plan::Engine("down")),
        "restart" => Some(Plan::Engine("restart")),
        "status" => Some(Plan::Engine("status")),
        // The button says "Eval 200K": the console means the full-context
        // gate, not whatever a bare `golem eval` would default to.
        "eval" => Some(Plan::Command(vec![
            "eval".to_string(),
            "--long".to_string(),
            "200000".to_string(),
        ])),
        // Both name a preset; with no name each falls back to the default
        // model, which is what an empty field means.
        "tune" | "pull" if !arg.is_empty() => {
            Some(Plan::Command(vec![action.to_string(), arg.to_string()]))
        }
        "bench" | "tune" | "pull" | "report" => Some(Plan::Command(vec![action.to_string()])),
        _ => None,
    }
}

/// Run one console action to completion, everything it prints going to
/// `log`, and its exit code back.
///
/// The log is the console's job pane (`logs/web-job.log`): the page polls it
/// through `api/state`, so it is where a service's streamed lines belong -
/// the launcher's stdout used to be the only thing in it, and a reader of
/// the pane should not care which of the two filled it.
pub fn run(home: &Path, action: &str, arg: &str, log: &Path) -> i32 {
    match plan(action, arg) {
        Some(Plan::Engine(verb)) => through_service(home, verb, log),
        Some(Plan::Command(args)) => self_run(home, &args, log),
        None => {
            let _ = line(log, &format!("[FAIL] unknown action: {action}"));
            2
        }
    }
}

/// The engine verbs: ask the service that owns the engine, starting one when
/// there is none.
///
/// A verb the service cannot serve - an engine whose adapter says it installs
/// itself - comes back as its own failure, with the reason streamed into the
/// log: there is no shell launcher to hand it to any more, which is what the
/// `launcher` key in an adapter now means.
fn through_service(home: &Path, verb: &str, log: &Path) -> i32 {
    let Some(mut out) = open(log) else { return 1 };
    if let Some(code) = service::send(home, verb, &mut out) {
        return code;
    }
    if let Err(why) = service::ensure_running(home, &mut out) {
        let _ = writeln!(out, "[FAIL] {why}");
        return 1;
    }
    match service::send(home, verb, &mut out) {
        Some(code) => code,
        None => {
            let _ = writeln!(out, "[FAIL] the golem service started but did not answer");
            1
        }
    }
}

/// Everything else: this binary's own verb, as a child process with the same
/// home, its output to the log and its exit code back.
fn self_run(home: &Path, args: &[String], log: &Path) -> i32 {
    let Ok(exe) = std::env::current_exe() else {
        let _ = line(log, "[FAIL] cannot find this binary to run the action");
        return 1;
    };
    let mut command = Command::new(exe);
    command.args(args).env("GOLEM_HOME", home);
    match run_command(command, log) {
        Ok(code) => code,
        Err(e) => {
            let _ = line(log, &format!("[FAIL] cannot run golem {}: {e}", args[0]));
            1
        }
    }
}

/// Run a prepared command, with its output going to `log`.
///
/// The one way this binary runs a child: no terminal, output to a file, an
/// exit code back. The tuner's gate and the app's own verbs both want exactly
/// that, and the job pane reads the same file either way.
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

/// The job log, created with its directory, or `None` with the reason said
/// on stderr - the one failure worth a message rather than a code.
///
/// Append rather than truncate: the lines of one verb land after the
/// previous one's rather than replacing them, and a reader of the job pane
/// wants the run they just started.
fn open(log: &Path) -> Option<File> {
    let file = match File::options().create(true).append(true).open(log) {
        Ok(file) => Ok(file),
        Err(_) => match log.parent() {
            Some(dir) => std::fs::create_dir_all(dir).and_then(|()| File::create(log)),
            None => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "the log path has no directory",
            )),
        },
    };
    match file {
        Ok(file) => Some(file),
        Err(e) => {
            eprintln!("[FAIL] cannot write {}: {e}", log.display());
            None
        }
    }
}

/// One line appended to the log, for the paths that have no open handle.
fn line(log: &Path, text: &str) -> std::io::Result<()> {
    let mut out = open(log).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::Other, "cannot open the job log")
    })?;
    writeln!(out, "{text}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(words: &[&str]) -> Plan {
        Plan::Command(words.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn the_engine_verbs_translate_the_console_spellings() {
        // The page says start/stop, the CLI's older words say up/down, and
        // both pairs are the service's up/down.
        assert_eq!(plan("start", ""), Some(Plan::Engine("up")));
        assert_eq!(plan("up", ""), Some(Plan::Engine("up")));
        assert_eq!(plan("stop", ""), Some(Plan::Engine("down")));
        assert_eq!(plan("down", ""), Some(Plan::Engine("down")));
        assert_eq!(plan("restart", ""), Some(Plan::Engine("restart")));
        assert_eq!(plan("status", ""), Some(Plan::Engine("status")));
    }

    #[test]
    fn the_eval_button_is_the_full_context_gate_whatever_the_field_says() {
        assert_eq!(plan("eval", ""), Some(command(&["eval", "--long", "200000"])));
        assert_eq!(plan("eval", "sneaky"), Some(command(&["eval", "--long", "200000"])));
    }

    #[test]
    fn tune_and_pull_carry_the_preset_they_were_given() {
        assert_eq!(plan("tune", "ornith-1.5-35b"), Some(command(&["tune", "ornith-1.5-35b"])));
        assert_eq!(plan("pull", "tiny"), Some(command(&["pull", "tiny"])));
        // No name: the verb alone, not an empty argument.
        assert_eq!(plan("tune", ""), Some(command(&["tune"])));
        assert_eq!(plan("pull", ""), Some(command(&["pull"])));
    }

    #[test]
    fn a_stray_argument_is_dropped_by_an_action_that_takes_none() {
        assert_eq!(plan("bench", "sneaky"), Some(command(&["bench"])));
    }

    #[test]
    fn report_is_bare_and_an_unknown_word_is_no_plan() {
        assert_eq!(plan("report", ""), Some(command(&["report"])));
        assert_eq!(plan("report", "sneaky"), Some(command(&["report"])));
        assert_eq!(plan("nonsense", ""), None);
        assert_eq!(plan("", ""), None);
    }
}
