//! Running a child: this binary's own verbs, with their output in a file.

use std::path::Path;
use std::process::{Command, Stdio};

/// This binary, about to run `args` against the same home.
pub fn golem(home: &Path, args: &[String]) -> std::io::Result<Command> {
    let mut command = Command::new(std::env::current_exe()?);
    command.args(args).env("GOLEM_HOME", home);
    Ok(command)
}

/// Run a prepared command to completion, its stdout and stderr going to
/// `log` (truncated first), and return its exit code.
///
/// The one way this binary runs a child: no terminal, output to a file, a
/// code back. The tuner's bench and gate and the console's actions all want
/// exactly that, and a reader of the file sees what the child printed.
pub fn run_to_file(mut command: Command, log: &Path) -> std::io::Result<i32> {
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
