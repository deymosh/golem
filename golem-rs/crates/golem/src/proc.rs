//! Running a child: this binary's own verbs, with their output in a file,
//! and the shared curl plumbing a download watches.

use std::io::{BufRead, Read, Write};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use golem_core::progress::Progress;

/// The curl arguments that end a connection going nowhere: thirty seconds to
/// connect, then at least 1 KiB/s or curl gives up after a minute of it.
/// `--retry` has another go at a transfer these cut short, so a stalled
/// download becomes a pause instead of a hang.
pub const CURL_STALL: [&str; 6] =
    ["--connect-timeout", "30", "--speed-limit", "1024", "--speed-time", "60"];

/// How often a download in a job's log says how far it has got.
pub const PROGRESS_EVERY: Duration = Duration::from_secs(2);

/// Wait for `child` to finish, and every [`PROGRESS_EVERY`] tell `emit` how
/// far the file it is writing has grown - its size against `total`, capped
/// at it, with `label` naming the work. The status comes back for the caller
/// to judge.
pub fn watch_progress(
    mut child: Child,
    dest: &Path,
    total: u64,
    label: &str,
    emit: &mut dyn FnMut(Progress),
) -> std::io::Result<ExitStatus> {
    let mut said = Instant::now().checked_sub(PROGRESS_EVERY).unwrap_or_else(Instant::now);
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if said.elapsed() >= PROGRESS_EVERY {
            let done = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
            emit(Progress::bytes(done.min(total), total, label));
            said = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

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

/// Run a prepared command to completion with its stdout and stderr piped,
/// each complete line written to the shared `log` as it arrives - appended,
/// where [`run_to_file`] truncates - and return its exit code.
///
/// A thread per pipe keeps both streams live while the child runs; the log
/// is shared because a stdout line and a stderr one can land together. What
/// a line says is the writer's business: the job log's writer stamps each
/// one.
pub fn run_to_log<W>(mut command: Command, log: Arc<Mutex<W>>) -> std::io::Result<i32>
where
    W: Write + Send + 'static,
{
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn()?;
    let out = child.stdout.take().map(|pipe| lines_to(pipe, Arc::clone(&log)));
    let err = child.stderr.take().map(|pipe| lines_to(pipe, log));
    let status = child.wait()?;
    if let Some(out) = out {
        let _ = out.join();
    }
    if let Some(err) = err {
        let _ = err.join();
    }
    Ok(status.code().unwrap_or(-1))
}

/// Every complete line of `pipe`, written to `log` as it arrives.
fn lines_to<R, W>(pipe: R, log: Arc<Mutex<W>>) -> std::thread::JoinHandle<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    std::thread::spawn(move || {
        let mut reader = std::io::BufReader::new(pipe);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    // A poisoned lock still holds a working log: keep
                    // writing to it rather than losing the child's output.
                    let mut log = match log.lock() {
                        Ok(log) => log,
                        Err(poisoned) => poisoned.into_inner(),
                    };
                    let _ = writeln!(log, "{}", line.trim_end_matches(['\n', '\r']));
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stall_args_end_a_connection_gone_quiet() {
        assert_eq!(
            CURL_STALL,
            ["--connect-timeout", "30", "--speed-limit", "1024", "--speed-time", "60"]
        );
    }

    #[test]
    fn a_child_that_ends_is_watched_to_its_status() {
        let mut command = Command::new(if cfg!(windows) { "cmd" } else { "sh" });
        if cfg!(windows) {
            command.args(["/C", "exit 0"]);
        } else {
            command.args(["-c", "exit 0"]);
        }
        let child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        // The destination exists and is smaller than the total, so the one
        // tick the short child allows says how much is there, capped.
        let mut said = Vec::new();
        let status = watch_progress(child, &std::env::temp_dir(), 100, "test", &mut |p| {
            said.push(p)
        })
        .expect("watched");
        assert!(status.success());
        let first = said.first().expect("at least one tick");
        assert_eq!(first.total, 100);
        assert!(first.done <= 100, "{first:?}");
        assert_eq!(first.label, "test");
    }
}
