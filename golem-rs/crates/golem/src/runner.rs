//! The app's actions: the one path the TUI's menu and the console's job
//! runner run a verb through.
//!
//! The engine verbs go to the service that owns the engine (started when
//! there is none), and the lines it streams back land in the job log the
//! page and the screen tail. The measuring verbs are this binary's own
//! commands, run as its children.
//!
//! This is also where the console's vocabulary lives - the actions a page may
//! post and the two it answers in process - because [`plan`] is the same list
//! seen from the other side.

use std::fs::File;
use std::io::Write;
use std::path::Path;
use golem_core::progress;

use crate::config::Overrides;
use crate::proc;
use crate::service;

/// What an action is, decided before anything runs - pure, so the vocabulary
/// and the argument policy are testable without a service or a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// A read of the engine's own state, answered here.
    ///
    /// The service can answer this too - same verb, same function - but
    /// asking it *starts* one, and a read must not have that side effect:
    /// pressing Status in the screen spawned a service and held the UI for
    /// the seconds it took to come up (measured: 4.7s, which is
    /// indistinguishable from the key doing nothing).
    Read(&'static str),
    /// The engine verb, as the service knows it: "up", "down", "restart".
    /// (`start`/`stop` are the console's spellings of up/down, and arrive
    /// here translated.)
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
        "status" => Some(Plan::Read("status")),
        // The button says "Eval 200K": the console means the full-context
        // gate, not whatever a bare `golem eval` would default to.
        "eval" => Some(Plan::Command(vec![
            "eval".to_string(),
            "--long".to_string(),
            "200000".to_string(),
        ])),
        // Both name what to work on - a preset, and for pull also a library
        // model or owner/repo[:quant]; with no name each falls back to the
        // default model, which is what an empty field means. A value that
        // starts with `-` would reach the command as a flag, so it is no
        // action at all.
        "tune" | "pull" if arg.starts_with('-') => None,
        // The button tunes to serve: a tune always gates here, so what it
        // adopts has passed the eval, and the engine comes back on it.
        "tune" => {
            let mut args = vec!["tune".to_string()];
            if !arg.is_empty() {
                args.push(arg.to_string());
            }
            args.push("--adopt".to_string());
            Some(Plan::Command(args))
        }
        "pull" if !arg.is_empty() => Some(Plan::Command(vec![action.to_string(), arg.to_string()])),
        "bench" | "pull" | "report" => Some(Plan::Command(vec![action.to_string()])),
        _ => None,
    }
}

/// Run one console action to completion, everything it prints going to
/// `log`, and its exit code back.
///
/// The log is the console's job pane (`logs/web-job.log`): the page polls it
/// through `api/state`, so it is where a service's streamed lines belong.
///
/// A job starts and ends with the progress clear, so the bar the page and
/// the screen draw is always this job's, and goes when it is done.
pub fn run(home: &Path, action: &str, arg: &str, log: &Path) -> i32 {
    let _ = line(log, progress::CLEAR);
    let code = match plan(action, arg) {
        Some(Plan::Read(verb)) => read(home, verb, log),
        Some(Plan::Engine(verb)) => through_service(home, verb, log),
        Some(Plan::Command(args)) => self_run(home, &args, log),
        None => {
            let _ = line(log, &format!("[FAIL] unknown action: {action}"));
            2
        }
    };
    let _ = line(log, progress::CLEAR);
    code
}

/// A read of the engine's state, answered here rather than by the service.
///
/// No service is started for it, which is the whole point: a read that
/// spawns a supervisor is a surprise with a wait attached.
fn read(home: &Path, verb: &str, log: &Path) -> i32 {
    let Some(mut out) = open(log) else { return 1 };
    let code = match verb {
        "status" => crate::launch::status(home, &mut out),
        other => {
            let _ = writeln!(out, "[FAIL] {other} is not a read of the engine");
            2
        }
    };
    let _ = out.flush();
    code
}

/// The engine verbs: ask the service that owns the engine, starting one when
/// there is none.
fn through_service(home: &Path, verb: &str, log: &Path) -> i32 {
    let Some(mut out) = open(log) else { return 1 };
    match service::request(home, verb, &Overrides::default(), &mut out) {
        Ok(code) => code,
        Err(why) => {
            let _ = writeln!(out, "[FAIL] {why}");
            1
        }
    }
}

/// Everything else: this binary's own verb, as a child process with the same
/// home, its output to the log and its exit code back.
fn self_run(home: &Path, args: &[String], log: &Path) -> i32 {
    match proc::golem(home, args).and_then(|command| proc::run_to_file(command, log)) {
        Ok(code) => code,
        Err(e) => {
            let _ = line(log, &format!("[FAIL] cannot run golem {}: {e}", args[0]));
            1
        }
    }
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
    }

    #[test]
    fn status_is_a_read_and_not_a_service_verb() {
        // The service can answer it, but asking starts one: pressing Status
        // in the screen spawned a service and held the UI for 4.7 seconds,
        // which reads as the key doing nothing. A read of the engine's own
        // state needs no supervisor.
        assert_eq!(plan("status", ""), Some(Plan::Read("status")));
    }

    #[test]
    fn the_eval_button_is_the_full_context_gate_whatever_the_field_says() {
        assert_eq!(plan("eval", ""), Some(command(&["eval", "--long", "200000"])));
        assert_eq!(plan("eval", "sneaky"), Some(command(&["eval", "--long", "200000"])));
    }

    #[test]
    fn tune_and_pull_carry_the_preset_they_were_given() {
        assert_eq!(
            plan("tune", "ornith-1.5-35b"),
            Some(command(&["tune", "ornith-1.5-35b", "--adopt"]))
        );
        assert_eq!(plan("pull", "tiny"), Some(command(&["pull", "tiny"])));
        // No name: the verb alone, not an empty argument.
        assert_eq!(plan("tune", ""), Some(command(&["tune", "--adopt"])));
        assert_eq!(plan("pull", ""), Some(command(&["pull"])));
        // A repository, as the console's pull box sends it.
        assert_eq!(
            plan("pull", "unsloth/Qwen3.5-9B-GGUF:Q6_K"),
            Some(command(&["pull", "unsloth/Qwen3.5-9B-GGUF:Q6_K"]))
        );
    }

    #[test]
    fn an_argument_that_looks_like_a_flag_is_refused() {
        // The field names a model; `--no-gate` from it would adopt a setup
        // no eval has passed.
        assert_eq!(plan("tune", "--no-gate"), None);
        assert_eq!(plan("pull", "-x"), None);
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
