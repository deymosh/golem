//! The `golem` front door: the command line.
//!
//! Every command is this binary's: the measurement, the tuner, the console,
//! the screen, the engine and the service that owns it. The configuration is
//! one file in the home (`golem.toml`, written from the compiled-in template
//! on first run). The modules are the `golem` library's (`lib.rs`); this file
//! only parses the arguments and picks the verb.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use golem::config::Overrides;
use golem::{
    auto_tune, client, install, launch, logs, measure, paths, pull, runner, service, setup, tui,
    tune, web,
};

const USAGE: &str = "\
golem - serve one local model to Claude Code and any OpenAI client

Usage: golem [command] [args]

With no command the full-screen console opens (the same as `golem tui`).

Serving:
  up [--dry-run]   Start the engine (installs the pinned build and the
                   preset's model first when they are missing)
  down             Stop the engine
  restart          Down, then up
  status           Is the engine up, and what is it serving
  service [--up|stop]
                   The process that owns the engine; up/down/restart ask it
  logs [--lines N] Follow the engine's log
  claude [args]    Launch Claude Code pointed at this server
  env              Print the exports for a shell

Models and engines:
  pull [preset]    Download a preset's GGUF files from Hugging Face
  install          Put the pinned engine build on this machine
  setup            Inspect the machine and scaffold the home

Measuring:
  bench [--runs N] [--prefill N]   Speed: decode + prefill tok/s
  eval [--long N]  Quality gate: sanity, tool calls, needle at depth
  tune [preset]    Find the fastest setup this machine can serve, and gate it
  tune --auto      Pick the best installed model, then tune it

Results:
  web [--port N]   Console: results, models, controls, live log
  tui              The same console in the terminal
  report [--md]    Static results page (or bench/RESULTS.md)
  results          List the recorded rows
  version          Print the version
";

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V" | "version") => {
            println!("golem {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        // Bare `golem` is the screen, so double-clicking the binary opens
        // the console. Piped or scripted there is no screen to
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
        Some("service") => code(service::run(&prepare_home(), &args[1..])),
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
            cmd_driver(action, &args[1..])
        }
        Some(other) => {
            eprintln!("golem: unknown command '{other}' (golem help lists them)");
            ExitCode::from(2)
        }
    }
}

/// `golem status`: up or down, on what port, with what the engine lists.
///
/// The verb itself lives in [`launch::status`] - it is engine state, and the
/// service needs the same answer - so this is the front door's thin wrapper.
///
/// Exit code: 0 when the engine answers, 1 when it does not, so a script can
/// branch on it.
fn cmd_status() -> ExitCode {
    let home = prepare_home();
    let exit = launch::status(&home, &mut std::io::stdout());
    // The service is worth a line of its own because it changes what the
    // engine's state *means*: with one running, `down` stops the engine and
    // the service stays, and a reader who does not know that will wonder what
    // is still holding the home.
    if let Some(endpoint) = service::Endpoint::read(&home).filter(|_| service::running(&home)) {
        println!("       golem service is up on 127.0.0.1:{} and owns the engine", endpoint.port);
    }
    code(exit)
}

/// `golem tune [preset]`: search, decide and gate. 0 when a setup was found,
/// 1 when none was, 2 for flags it could not read.
fn cmd_tune(args: &[String]) -> ExitCode {
    let home = prepare_home();
    match tune::run(&home, args) {
        0 => ExitCode::SUCCESS,
        code => ExitCode::from(code as u8),
    }
}

/// `golem up|down|restart` (and `start`/`stop`, the console's words): the
/// engine verbs, asked of the service that owns the engine.
///
/// The caller's `GOLEM_*` overrides travel with the request: the service runs
/// the verb in its own process, where this one's environment cannot reach.
/// `up --dry-run` is the one verb answered here - it resolves the command
/// line and starts nothing, so there is nothing for the service to own.
fn cmd_driver(action: &str, rest: &[String]) -> ExitCode {
    let home = prepare_home();
    let overrides = Overrides::from_env();

    if matches!(action, "up" | "start") && rest.iter().any(|a| a == "--dry-run") {
        return match launch::up(&home, &overrides, true, &mut std::io::stdout()) {
            Ok(_) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("[FAIL] {e}");
                ExitCode::from(1)
            }
        };
    }

    if matches!(action, "up" | "start" | "restart") {
        let verb = if action == "restart" { "restart" } else { "up" };
        return match service::request(&home, verb, &overrides, &mut std::io::stdout()) {
            Ok(exit) => code(exit),
            Err(e) => {
                eprintln!("[FAIL] {e}");
                ExitCode::from(1)
            }
        };
    }

    // `down` is the service's too when there is one - the engine is its child,
    // and it is the one that knows what it started. With no service the engine
    // may still be running (a service that died, an engine started before it),
    // so the direct stop stays: kill the tree, then wait for the card.
    if matches!(action, "down" | "stop") {
        let mut out = std::io::stdout();
        return match service::send(&home, "down", &overrides, &mut out) {
            Some(exit) => code(exit),
            None => code(launch::down(&home, &mut out)),
        };
    }

    // Every verb this front door takes returned above; this is the compiler's
    // exit rather than a path a caller can reach.
    eprintln!("[FAIL] {action} is not an engine verb");
    ExitCode::from(2)
}

/// `golem tui`: the full-screen console, with every action routed through the
/// service that owns the engine, or run as this binary's own verbs.
///
/// The screen decides *which* action; what an action does is this function's
/// business, which is the seam `tui::run` takes a callback for. One entry is
/// not an action - `Web` starts this binary's own console, in this
/// process, so it lives exactly as long as the screen that asked for it.
async fn cmd_tui() -> ExitCode {
    let home = prepare_home();
    let log = paths::job_log(&home);
    let console = Console::new(home.clone());
    let handle = tokio::runtime::Handle::current();
    // The closure owns everything it touches: `tui::run` hands it to a
    // thread per action, so nothing here can borrow from this function.
    let act_home = home.clone();

    let outcome = tui::run(&home, move |action| match action {
        tui::Action::Web => console.start(&handle),
        // `run` returns on Quit without asking, so this arm is unreachable;
        // it exists because the match has to be total.
        tui::Action::Quit => String::new(),
        other => match verb_of(other) {
            Some(verb) => describe(verb, runner::run(&act_home, verb, "", &log)),
            None => format!("{other:?} has no action"),
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

/// The action a menu entry runs, or `None` for the entries the screen
/// answers itself.
fn verb_of(action: tui::Action) -> Option<&'static str> {
    match action {
        tui::Action::Status => Some("status"),
        // The console's own words, which is what the page posts too.
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
fn describe(verb: &str, code: i32) -> String {
    match code {
        0 => format!("{verb}: done"),
        code => format!("{verb}: exit {code} - see logs/web-job.log"),
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
/// The library's verbs (bench, eval, pull, ...) return plain `i32` codes;
/// the arms that answer here speak `ExitCode`. This is the one place the two
/// meet.
fn code(status: i32) -> ExitCode {
    ExitCode::from(status.clamp(0, 255) as u8)
}

/// The value of `--flag value`, or None.
fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == flag)?;
    args.get(i + 1).map(String::as_str)
}

/// The console, on `--host`/`--port` (127.0.0.1:8787 by default).
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

/// The home, with its directories made sure of. A release folder starts
/// empty; a checkout already has everything, and this is a no-op there. A
/// failure is a warning, not an exit: the command that needed the missing
/// directory reports its own error.
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

/// Open a file with the desktop's own handler, best effort: the page is
/// written either way, and a machine with no opener - a server, a bare
/// container - must not turn a written report into a failed command.
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

    // The page is the template with the records inlined: the raw lines, not
    // a re-serialization, so the page can never disagree with the file.
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
    let data = web::inline_records(&raw);
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
