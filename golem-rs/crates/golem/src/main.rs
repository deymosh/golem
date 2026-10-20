//! The portable `golem` front door.
//!
//! The binary orchestrates; the shell scripts stay the engine drivers until a
//! phase replaces their gate (docs/plan-portable-binary.md). Today it can read
//! the results database and render both views of it.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod auto_tune;
mod client;
mod driver;
mod engine;
mod facts;
mod install;
mod logs;
mod measure;
mod models;
mod paths;
mod pull;
mod setup;
mod state;
mod tune;
mod tui;
mod web;

const USAGE: &str = "\
golem - one portable front door for the local model server

Usage: golem [command] [args]

With no command at all the full-screen console opens - the same screen as
`golem tui`, and what `golem.cmd` does with no arguments.

Commands (phased in, docs/plan-portable-binary.md):
  version          Print the version
  results          List recorded bench/eval rows        (phase 1)
  report [--no-open|--md]  Static page for sharing, or RESULTS.md for GitHub
  web              Web console: results, controls, live  (phases 2-3)
  status           Is the engine up, and what is it serving
  up / down        Start / stop through the shell launchers
  restart          Down then up
  logs [--lines N] Follow the inference log
  pull [preset]    Download the model's GGUF from Hugging Face
  bench [--runs N] [--prefill N]   Speed: decode + prefill tok/s
  eval [--long N]  Quality gate: sanity, tool calls, needle
  tui / menu       Full-screen console                  (phase 5)
  tune [preset]    Sweep, gate and adopt the best setup (phase 4)
  tune --auto      Pick the best installed model, then tune it
  claude [args]    Launch Claude Code pointed at this server
  env              Print the exports for a shell
  setup            Inspect the machine, install build deps, scaffold dirs
  install          Build llama.cpp with CUDA/ROCm  (native mode only)
";

#[tokio::main]
async fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mode = take_mode_flags(&mut args);
    match args.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => {
            println!("golem {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        // Bare `golem` is the screen, which is what the Windows front door
        // does with no arguments. Piped or scripted there is no screen to
        // open and raw mode cannot be had, so the usage is the answer that
        // is actually useful there.
        None => {
            if std::io::stdout().is_terminal() {
                cmd_tui().await
            } else {
                print!("{USAGE}");
                ExitCode::SUCCESS
            }
        }
        Some("report") => cmd_report(&args[1..]),
        Some("results") => cmd_results(&args[1..]),
        Some("web") => cmd_web(&args[1..]).await,
        Some("tui" | "menu") => cmd_tui().await,
        Some("status") => cmd_status(),
        Some("bench") => code(measure::bench(&prepare_home(), &args[1..])),
        Some("eval") => code(measure::eval(&prepare_home(), &args[1..])),
        Some("logs") => code(logs::run(&prepare_home(), &args[1..])),
        Some("pull") => code(pull::run(&prepare_home(), &args[1..])),
        Some("setup") => code(setup::run(&prepare_home(), &args[1..])),
        Some("install") => code(install::run(&prepare_home(), &args[1..])),
        Some("env") => {
            print!("{}", client::exports(&prepare_home()));
            ExitCode::SUCCESS
        }
        Some("claude") => code(client::claude(&prepare_home(), &args[1..])),
        // `tune --auto` is the driver, not the sweep: it picks the model and
        // then hands each candidate to `tune::run`.
        Some("tune") if args[1..].iter().any(|a| a == "--auto") => {
            code(auto_tune::run(&prepare_home(), &args[1..]))
        }
        Some("tune") => cmd_tune(&args[1..]),
        Some(action @ ("up" | "down" | "start" | "stop" | "restart")) => {
            cmd_driver(action, mode.as_deref())
        }
        Some(other) => {
            eprintln!("golem: '{other}' is not wired up yet; see docs/plan-portable-binary.md");
            ExitCode::from(2)
        }
    }
}

/// `golem status`: up or down, on what port, with what the engine lists.
///
/// Exit code follows the shell's health check: 0 when the engine answers,
/// 1 when it does not, so a script can branch on it.
fn cmd_status() -> ExitCode {
    let home = prepare_home();
    let endpoint = engine::Endpoint::from_home(&home);
    let status = engine::status(&home);

    if !status.up {
        println!("[ .. ] engine down - no answer on http://127.0.0.1:{}", endpoint.port);
        match status.pid {
            // A pid file that outlived its process is worth saying out loud:
            // it is what leaves an engine looking running when it is not.
            Some(pid) => println!("        a pid file still records {pid} (start it, or the launcher clears it)"),
            None => println!("        start it with: golem up"),
        }
        return ExitCode::from(1);
    }

    match status.pid {
        Some(pid) => println!("[ OK ] engine up on http://127.0.0.1:{} (pid {pid})", status.port),
        None => println!("[ OK ] engine up on http://127.0.0.1:{}", status.port),
    }
    let models = engine::models(&endpoint);
    if models.is_empty() {
        println!("       no models listed");
    } else {
        println!("       serving: {}", models.join(", "));
    }
    ExitCode::SUCCESS
}

/// `golem tune [preset]`: sweep, decide and gate (phase 4).
///
/// It measures through the same launcher and the same bench script the
/// shell tuner uses - it is a front door, not a second engine - and decides
/// with `golem_core::tune`. The exit code is the shell's: 0 when a setup was
/// found, 1 when none was.
fn cmd_tune(args: &[String]) -> ExitCode {
    let home = prepare_home();
    match tune::run(&home, args) {
        0 => ExitCode::SUCCESS,
        code => ExitCode::from(code as u8),
    }
}

/// Pull the front door's global mode flags out of the command line.
///
/// `--native`, `--docker` and `--windows` belong to the front door, not to a
/// command: `./golem up --docker` and `./golem logs --windows` both mean "run
/// this in that mode", and the shell resolves them before it dispatches. The
/// binary does the same, in two steps - the resolved mode goes into
/// `GOLEM_DEPLOY_MODE`, which is the per-invocation override every mode-aware
/// command already reads through [`state::override_value`], and the flag is
/// taken out of the argument list so the command it belonged to is not handed
/// an argument it never declared.
///
/// Returns the mode, or `None` when the line named none.
fn take_mode_flags(args: &mut Vec<String>) -> Option<String> {
    let mut mode = None;
    args.retain(|arg| {
        let resolved = match arg.as_str() {
            "--native" => "native",
            "--docker" => "docker",
            "--windows" => "windows",
            _ => return true,
        };
        // Last one wins, as the shell's loop does.
        mode = Some(resolved.to_string());
        false
    });
    if let Some(mode) = &mode {
        std::env::set_var("GOLEM_DEPLOY_MODE", mode);
    }
    mode
}

/// `golem up|down|restart`: ask the launchers that already know how.
///
/// Starting an engine is the riskiest code in the project - three deploy
/// modes, a pinned build, a router preset - and it works; phase 3 reuses it
/// rather than reimplementing it, and phase 6 decides what to own.
///
/// `up` and `down` are this binary's words and `start`/`stop` are
/// `golem.cmd`'s; [`driver::Driver::verb`] translates whichever it is given,
/// which is what makes `golem up` work on Windows at all - `golem.cmd` has
/// no `up`, so the verb used to reach it as an unknown command.
///
/// A mode flag the user typed is handed to the launcher as the launcher's own
/// flag: the launcher reads `DEPLOY_MODE` from `.env`, so a mode that lived
/// only in this process's environment would be silently dropped - and
/// `./golem up --docker` would start a native server.
fn cmd_driver(action: &str, mode: Option<&str>) -> ExitCode {
    let home = prepare_home();
    let Some(launcher) = driver::Driver::find(&home) else {
        eprintln!(
            "[FAIL] no launcher in {} (golem.cmd or golem) - the engine drivers come with the repository for now",
            home.display()
        );
        return ExitCode::from(1);
    };
    let args: Vec<String> = mode.map(|mode| format!("--{mode}")).into_iter().collect();
    let log = home.join("logs/web-job.log");
    println!("[ .. ] {action}: running the launcher (its output goes to logs/web-job.log)");
    match driver::run_command(launcher.command_with(action, &args), &log) {
        Ok(0) => {
            println!("[ OK ] {action} finished");
            ExitCode::SUCCESS
        }
        Ok(code) => {
            eprintln!("[FAIL] {action} exited {code} - read logs/web-job.log");
            ExitCode::from(1)
        }
        Err(e) => {
            eprintln!("[FAIL] cannot run the launcher: {e}");
            ExitCode::from(1)
        }
    }
}

/// `golem tui`: the full-screen console, with every action routed through the
/// same launcher the CLI uses.
///
/// The screen decides *which* action; what an action does is this function's
/// business, which is the seam `tui::run` takes a callback for. One entry is
/// not a launcher verb - `Web` starts this binary's own console, in this
/// process, so it lives exactly as long as the screen that asked for it.
async fn cmd_tui() -> ExitCode {
    let home = prepare_home();
    let Some(launcher) = driver::Driver::find(&home) else {
        eprintln!(
            "[FAIL] no launcher in {} (golem.cmd or golem) - the engine drivers come with the repository for now",
            home.display()
        );
        return ExitCode::from(1);
    };
    let log = home.join("logs/web-job.log");
    let console = Console::new(home.clone());
    let handle = tokio::runtime::Handle::current();

    let outcome = tui::run(&home, |action| match action {
        tui::Action::Web => console.start(&handle),
        // `run` returns on Quit without asking, so this arm is unreachable;
        // it exists because the match has to be total.
        tui::Action::Quit => String::new(),
        other => match verb_of(other) {
            Some(verb) => describe(verb, driver::run(&launcher, verb, "", &log)),
            None => format!("{other:?} has no launcher verb"),
        },
    });

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[FAIL] cannot run the console: {e}");
            ExitCode::from(1)
        }
    }
}

/// The launcher verb a menu entry runs, or `None` for the entries the binary
/// answers itself.
fn verb_of(action: tui::Action) -> Option<&'static str> {
    match action {
        tui::Action::Status => Some("status"),
        // The console's own words, which is what the page posts too; the
        // launcher's spelling is the driver's business.
        tui::Action::Start => Some("start"),
        tui::Action::Stop => Some("stop"),
        tui::Action::Restart => Some("restart"),
        tui::Action::Bench => Some("bench"),
        tui::Action::Eval => Some("eval"),
        tui::Action::Tune => Some("tune"),
        tui::Action::Report => Some("report"),
        tui::Action::Web | tui::Action::Quit => None,
    }
}

/// What the message line says after an action: short enough for one line,
/// and it points at the log whenever the log is where the answer is.
fn describe(verb: &str, outcome: std::io::Result<i32>) -> String {
    match outcome {
        Ok(0) => format!("{verb}: done"),
        Ok(code) => format!("{verb}: exit {code} - see logs/web-job.log"),
        Err(e) => format!("{verb}: cannot run the launcher ({e})"),
    }
}

/// The console the TUI's Web entry starts, at most once per process.
struct Console {
    home: PathBuf,
    started: Arc<AtomicBool>,
}

impl Console {
    fn new(home: PathBuf) -> Console {
        Console { home, started: Arc::new(AtomicBool::new(false)) }
    }

    /// Start the console and return the line to show.
    ///
    /// The socket is bound here, synchronously, rather than inside the task
    /// that serves it: a message line that promises a console which then
    /// fails to bind is worse than no console at all, and the bind is the
    /// only part that can fail for a reason worth reporting.
    fn start(&self, handle: &tokio::runtime::Handle) -> String {
        let cfg = web::Config {
            results: paths::results(&self.home),
            template: paths::template(&self.home),
            home: self.home.clone(),
            ..web::Config::default()
        };
        let address = web::address(&cfg);
        if self.started.load(Ordering::SeqCst) {
            return format!("console already on http://{address}");
        }
        let listener = match std::net::TcpListener::bind(&address) {
            Ok(listener) => listener,
            Err(e) => return format!("console cannot bind {address}: {e}"),
        };
        let _ = listener.set_nonblocking(true);
        // `from_std` wants a reactor, which is what entering the handle gives.
        let _guard = handle.enter();
        match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => {
                self.started.store(true, Ordering::SeqCst);
                handle.spawn(async move {
                    let _ = web::serve_on(listener, cfg).await;
                });
                format!("console on http://{address}")
            }
            Err(e) => format!("console cannot start: {e}"),
        }
    }
}

/// A command's exit code as the `ExitCode` the match arms return.
///
/// The commands that shell out (bench, eval, pull, ...) speak in plain
/// numbers, because their code is the child's; the arms that answer in
/// process speak `ExitCode`. This is the one place the two meet.
fn code(status: i32) -> ExitCode {
    ExitCode::from(status.clamp(0, 255) as u8)
}

/// The value of `--flag value`, or None.
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).map(String::as_str)
}

/// The console, on `--host`/`--port` (127.0.0.1:8787 by default - the same
/// address the shell console uses, so bookmarks and the page's own polling
/// keep working when this replaces it).
async fn cmd_web(args: &[String]) -> ExitCode {
    let home = prepare_home();
    // The console comes up whether or not anything has been measured: on a
    // fresh machine that is the point - it is where you see the first result.
    let results = results_path(args).unwrap_or_else(|| paths::results(&home));
    if !results.is_file() {
        println!("[ .. ] no results yet at {} - the page starts empty", results.display());
    }
    let template = paths::template(&home);
    if !template.is_file() {
        eprintln!("[FAIL] no report template at {}", template.display());
        return ExitCode::from(1);
    }

    let mut cfg = web::Config { results, template, home, ..web::Config::default() };
    if let Some(port) = arg_value(args, "--port") {
        match port.parse() {
            Ok(p) => cfg.port = p,
            Err(_) => {
                eprintln!("[FAIL] --port wants a number, got {port:?}");
                return ExitCode::from(1);
            }
        }
    }
    if let Some(host) = arg_value(args, "--host") {
        cfg.host = host.to_string();
    }

    println!("[ OK ] golem web UI on http://{}:{}  (Ctrl+C to stop)", cfg.host, cfg.port);
    match web::serve(cfg).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[FAIL] {e}");
            ExitCode::from(1)
        }
    }
}

/// The records to read: `--results <path>` when given (any file, which is
/// how the tests regenerate a fixture's expected output), otherwise the
/// home's own bench/results.jsonl.
fn results_path(args: &[String]) -> Option<PathBuf> {
    if let Some(given) = arg_value(args, "--results") {
        let path = PathBuf::from(given);
        return path.is_file().then_some(path);
    }
    let path = paths::results(&paths::home());
    path.is_file().then_some(path)
}

/// Print where the binary thinks its files live, and make sure the
/// directories exist. A release folder starts empty; a checkout already has
/// everything, and this is a no-op there.
fn prepare_home() -> PathBuf {
    let home = paths::home();
    if let Err(e) = paths::ensure(&home) {
        eprintln!("[WARN] cannot create the directories under {}: {e}", home.display());
    }
    home
}

fn read_rows(results: &Path) -> Option<Vec<golem_core::results::Row>> {
    match golem_core::results::read(results) {
        Ok((rows, bad)) => {
            // A line that will not parse is reported, never fatal: losing a
            // whole report over one bad line would hide the good rows too.
            for (line, err) in bad {
                eprintln!("[WARN] {}:{line}: {err}", results.display());
            }
            Some(rows)
        }
        Err(e) => {
            eprintln!("[FAIL] cannot read {}: {e}", results.display());
            None
        }
    }
}

/// Open a file with the desktop's own handler, best effort.
///
/// Best effort on purpose: the page is written either way, and a machine
/// with no opener - a server, a bare container, a session with no desktop -
/// must not turn a written report into a failed command. The three branches
/// are the shell report's three.
fn open_in_browser(path: &Path) {
    let mut command = if cfg!(windows) {
        // `start` is a cmd.exe builtin and its first argument is the window
        // title: without the empty one, a quoted path is taken as the title
        // and nothing opens.
        let mut command = std::process::Command::new("cmd");
        command.args(["/c", "start", ""]).arg(path);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = std::process::Command::new("open");
        command.arg(path);
        command
    } else {
        let mut command = std::process::Command::new("xdg-open");
        command.arg(path);
        command
    };
    let _ = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}

fn cmd_report(args: &[String]) -> ExitCode {
    let as_markdown = args.iter().any(|a| a == "--md");
    let home = prepare_home();
    let Some(results) = results_path(args) else {
        eprintln!(
            "[FAIL] no results yet at {} - run a bench first",
            paths::results(&home).display()
        );
        return ExitCode::from(1);
    };
    let Some(rows) = read_rows(&results) else { return ExitCode::from(1) };
    let bench = results.parent().expect("the results file has a directory");

    if as_markdown {
        let out = bench.join("RESULTS.md");
        if let Err(e) = std::fs::write(&out, golem_core::report::markdown(&rows)) {
            eprintln!("[FAIL] cannot write {}: {e}", out.display());
            return ExitCode::from(1);
        }
        println!("[ OK ] {} ({} rows)", out.display(), rows.len());
        return ExitCode::SUCCESS;
    }

    // The page is the template with the records inlined, exactly as
    // the shell report did it: the raw lines, not a re-serialization, so
    // the page can never disagree with the file.
    let template_path = paths::template(&home);
    if !template_path.is_file() {
        eprintln!("[FAIL] no report template at {}", template_path.display());
        return ExitCode::from(1);
    }
    let template = match std::fs::read_to_string(&template_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[FAIL] cannot read {}: {e}", template_path.display());
            return ExitCode::from(1);
        }
    };
    let raw = match std::fs::read_to_string(&results) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[FAIL] cannot read {}: {e}", results.display());
            return ExitCode::from(1);
        }
    };
    let data = inline_records(&raw);
    let out = bench.join("report.html");
    if let Err(e) = std::fs::write(&out, template.replace("/*GOLEM_DATA*/", &data)) {
        eprintln!("[FAIL] cannot write {}: {e}", out.display());
        return ExitCode::from(1);
    }
    println!("[ OK ] {} ({} runs)", out.display(), rows.len());
    // The page is for looking at; --no-open is for a script (and for anyone
    // who asked for it in a terminal they are already looking at).
    if !args.iter().any(|a| a == "--no-open") {
        open_in_browser(&out);
    }
    ExitCode::SUCCESS
}

/// `[{...},\n{...}]` from the file's record lines. `</` is escaped because it
/// would close the `<script>` block the data lives in early.
fn inline_records(raw: &str) -> String {
    let records: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('{'))
        .map(|line| line.replace("</", "<\\/"))
        .collect();
    format!("[{}]", records.join(",\n"))
}

fn cmd_results(args: &[String]) -> ExitCode {
    let home = prepare_home();
    let Some(results) = results_path(args) else {
        eprintln!(
            "[FAIL] no results yet at {} - run a bench first",
            paths::results(&home).display()
        );
        return ExitCode::from(1);
    };
    let Some(rows) = read_rows(&results) else { return ExitCode::from(1) };

    let filter = args.iter().position(|a| a == "--model").and_then(|i| args.get(i + 1));
    let shown: Vec<_> = rows
        .iter()
        // map_or rather than is_none_or: the latter is 1.82, and the
        // workspace promises 1.81.
        .filter(|r| filter.map_or(true, |m| &r.model == m))
        .collect();

    println!(
        "{:<28} {:<10} {:<24} {:>8} {:>9} {:>8}",
        "model", "kind", "engine", "ctx", "decode", "threads"
    );
    for row in shown {
        println!(
            "{:<28} {:<10} {:<24} {:>8} {:>9} {:>8}",
            row.model,
            format!("{:?}", row.kind).to_lowercase(),
            format!("{} {}", row.engine, row.engine_build),
            row.ctx.map(|c| format!("{:.0}K", c as f64 / 1024.0)).unwrap_or_else(|| "-".to_string()),
            row.comparable_decode().map(|d| format!("{d:.1}")).unwrap_or_else(|| "-".to_string()),
            row.threads.map(|t| t.to_string()).unwrap_or_else(|| "-".to_string()),
        );
    }
    println!("{} row(s)", rows.len());
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inlines_records_the_way_the_page_expects() {
        let raw = "{\"a\": 1}\n\n  {\"b\": \"x</script>\"}\nnot json\n";
        let data = inline_records(raw);
        assert_eq!(data, "[{\"a\": 1},\n{\"b\": \"x<\\/script>\"}]");
    }

    #[test]
    fn an_empty_file_inlines_as_an_empty_array() {
        assert_eq!(inline_records(""), "[]");
    }
}
