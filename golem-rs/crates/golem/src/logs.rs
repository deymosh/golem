//! `golem logs`: follow the inference log.
//!
//! The port of `cmd_logs` in the `golem` script, and the same file the
//! console's log pane tails (`state::log_tail`): `logs/llama-server-windows.log`
//! in windows mode, `logs/local-ai.log` in native mode, and `docker compose
//! logs -f` in docker mode.
//!
//! The follow is done here rather than by shelling out to `tail -f`, which
//! Windows does not ship: poll the file's size, read what was appended, and
//! print it. A log the launcher truncates or replaces must not spin or print
//! garbage, so a size that shrank means "start over from the top".

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::state;

/// How many lines `--no-follow` (and the first screenful of a follow) shows.
const DEFAULT_LINES: usize = 40;

/// How often the follow checks for appended bytes. Fast enough to feel live,
/// slow enough not to busy a core on an idle engine.
const POLL: Duration = Duration::from_millis(200);

/// `golem logs [--lines N] [--no-follow]`.
pub fn run(home: &Path, args: &[String]) -> i32 {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("[FAIL] logs: {message}");
            return 2;
        }
    };
    let mode = state::override_value(home, "GOLEM_DEPLOY_MODE", "DEPLOY_MODE")
        .unwrap_or_else(|| "native".to_string());
    let Some(path) = log_file(home, &mode) else {
        return docker_logs(home);
    };
    follow(&path, options.lines, options.follow)
}

/// The parsed command line.
#[derive(Debug, PartialEq, Eq)]
struct Options {
    lines: usize,
    follow: bool,
}

/// `--lines N` (or `--lines=N`) and `--no-follow`.
fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options { lines: DEFAULT_LINES, follow: true };
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if arg == "--no-follow" {
            options.follow = false;
        } else if arg == "--lines" {
            let value = args.get(i + 1).ok_or("--lines wants a number")?;
            options.lines = parse_lines(value)?;
            i += 1;
        } else if let Some(value) = arg.strip_prefix("--lines=") {
            options.lines = parse_lines(value)?;
        } else {
            return Err(format!("unexpected argument: {arg}"));
        }
        i += 1;
    }
    Ok(options)
}

fn parse_lines(value: &str) -> Result<usize, String> {
    value.parse::<usize>().map_err(|_| format!("--lines wants a number, got {value:?}"))
}

/// The file to follow, or `None` when the mode has no file at all (docker,
/// whose log comes from the compose service).
///
/// The match is the shell's: `windows` names the pinned llama-server's log,
/// `docker` is the compose service, and everything else - `native`, or a mode
/// a future `.env` invents - is the native log the shell's `else` tails.
fn log_file(home: &Path, mode: &str) -> Option<PathBuf> {
    match mode {
        "docker" => None,
        "windows" => Some(home.join("logs/llama-server-windows.log")),
        _ => Some(home.join("logs/local-ai.log")),
    }
}

/// `docker compose logs -f`, with the child owning the terminal.
///
/// The service is split cuda/rocm by compose profile. `auto` is the shell's
/// own detector's job, which this port does not carry, so it falls back to
/// the common case rather than handing compose a profile that does not exist.
fn docker_logs(home: &Path) -> i32 {
    let profile = state::override_value(home, "GOLEM_GPU_BACKEND", "GPU_BACKEND")
        .filter(|p| p != "auto" && !p.is_empty())
        .unwrap_or_else(|| "cuda".to_string());
    let mut command = Command::new("docker");
    command
        .args(["compose", "-f", "compose.yml", "--env-file", ".env", "logs", "-f"])
        .env("COMPOSE_PROFILES", profile)
        .current_dir(home)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    match command.status() {
        Ok(status) => status.code().unwrap_or(-1),
        Err(e) => {
            eprintln!("[FAIL] logs: cannot run docker compose: {e}");
            1
        }
    }
}

/// Print the tail of `path`, then keep printing what is appended to it.
///
/// A file that does not exist yet is not an error: the engine writes it on
/// start, so the loop waits for it to appear (and says so once). A file that
/// shrank - truncated in place, or replaced by the launcher - is re-read from
/// the top. `keep_going` is `false` for `--no-follow`, which prints the tail
/// and returns (with a non-zero code when there was no file to read).
fn follow(path: &Path, lines: usize, keep_going: bool) -> i32 {
    let stdout = std::io::stdout();
    let mut pos: u64 = 0;
    let mut first = true;
    let mut waiting = false;
    loop {
        match std::fs::metadata(path) {
            Ok(meta) => {
                let len = meta.len();
                if first {
                    // The opening screenful is the *last* `lines`, not the
                    // whole file: following a 2 GB log from byte zero is not
                    // what anyone means by `logs`.
                    if let Ok(data) = std::fs::read(path) {
                        let mut lock = stdout.lock();
                        let _ = lock.write_all(tail(&data, lines));
                        let _ = lock.flush();
                    }
                    pos = len;
                    first = false;
                    waiting = false;
                } else if len < pos {
                    // Truncated or replaced: start over from the beginning.
                    pos = 0;
                }
                if len > pos {
                    match read_from(path, pos) {
                        Ok(buf) => {
                            let mut lock = stdout.lock();
                            let _ = lock.write_all(&buf);
                            let _ = lock.flush();
                            pos += buf.len() as u64;
                        }
                        // It vanished between the stat and the open; the next
                        // turn re-checks and waits for it to come back.
                        Err(_) => pos = 0,
                    }
                }
            }
            Err(_) => {
                if !waiting {
                    if first {
                        eprintln!(
                            "[ .. ] no log yet at {} - waiting for it to appear",
                            path.display()
                        );
                    } else {
                        eprintln!(
                            "[ .. ] {} went away - waiting for it to come back",
                            path.display()
                        );
                        // Re-tail when it returns, so a replaced file starts
                        // from a sensible screenful rather than mid-line.
                        first = true;
                        pos = 0;
                    }
                    waiting = true;
                }
                if !keep_going {
                    return 1;
                }
            }
        }
        if !keep_going {
            return 0;
        }
        std::thread::sleep(POLL);
    }
}

/// The last `lines` lines of `data`, as bytes (a log need not be UTF-8).
///
/// The trailing newline is not counted as its own empty line, which is what
/// `tail -n N` does and what makes `--lines 1` show the final line rather
/// than nothing.
fn tail(data: &[u8], lines: usize) -> &[u8] {
    if lines == 0 || data.is_empty() {
        return &[];
    }
    let mut end = data.len();
    if data[end - 1] == b'\n' {
        end -= 1;
    }
    let mut seen = 0;
    let mut i = end;
    while i > 0 {
        if data[i - 1] == b'\n' {
            seen += 1;
            if seen == lines {
                break;
            }
        }
        i -= 1;
    }
    &data[i..]
}

/// Everything from byte `pos` to the end of the file.
fn read_from(path: &Path, pos: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(pos))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_defaults_are_forty_lines_and_follow() {
        let options = parse_args(&[]).expect("no arguments is fine");
        assert_eq!(options.lines, DEFAULT_LINES);
        assert!(options.follow);
    }

    #[test]
    fn the_flags_parse_both_spellings() {
        assert_eq!(parse_args(&["--lines".into(), "10".into()]).unwrap().lines, 10);
        assert_eq!(parse_args(&["--lines=5".into()]).unwrap().lines, 5);
        assert!(!parse_args(&["--no-follow".into()]).unwrap().follow);
        // The two combine in either order.
        let both = parse_args(&["--no-follow".into(), "--lines=3".into()]).unwrap();
        assert_eq!(both, Options { lines: 3, follow: false });
    }

    #[test]
    fn a_bad_command_line_is_an_error() {
        assert!(parse_args(&["--lines".into(), "x".into()]).is_err());
        assert!(parse_args(&["--lines".into()]).is_err(), "a flag with no value");
        assert!(parse_args(&["--nope".into()]).is_err());
    }

    #[test]
    fn the_log_file_follows_the_deploy_mode() {
        let home = Path::new("/tmp/golem-home");
        assert_eq!(log_file(home, "windows"), Some(home.join("logs/llama-server-windows.log")));
        assert_eq!(log_file(home, "native"), Some(home.join("logs/local-ai.log")));
        assert_eq!(log_file(home, "docker"), None, "docker has no file to tail");
        // Anything else is the shell's `else`: the native log.
        assert_eq!(log_file(home, "something"), Some(home.join("logs/local-ai.log")));
    }

    #[test]
    fn the_tail_is_the_last_lines() {
        assert_eq!(tail(b"a\nb\nc\n", 2), b"b\nc\n");
        assert_eq!(tail(b"a\nb\nc", 1), b"c", "a file with no trailing newline");
        assert_eq!(tail(b"a\nb\nc\n", 1), b"c\n");
        assert_eq!(tail(b"a\nb\nc\n", 99), b"a\nb\nc\n", "more lines than the file has");
        assert_eq!(tail(b"", 5), b"");
        assert_eq!(tail(b"a\nb\n", 0), b"");
    }
}
