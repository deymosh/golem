//! The `golem` front door: the command line.
//!
//! Every command is this binary's: the measurement, the tuner, the console,
//! the screen, the engine and the service that owns it. The configuration is
//! one file in the home (`golem.toml`, written from the compiled-in template
//! on first run). The modules are the `golem` library's (`lib.rs`); this file
//! only parses the arguments and picks the verb.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use golem::config::Overrides;
use golem::desktop::{self, FrontEnd, Launch};
use golem::{
    auto_tune, client, install, launch, logs, measure, paths, pull, runner, service, setup, tui,
    tune, web,
};

const USAGE: &str = "\
golem - serve one local model to Claude Code and any OpenAI client

Usage: golem [command] [args]

With no command: double-clicked on a desktop, the console opens in the
browser; in a terminal, the full-screen screen (`golem tui`).

Serving:
  up [--dry-run]   Start the engine (installs the pinned build and the
                   preset's model first when they are missing)
  down             Stop the engine
  restart          Down, then up
  status           Is the engine up, and what is it serving
  service [--up|stop]
                   The process that owns the engine and hosts the console
                   (127.0.0.1:8787 by default); up/down/restart
                   ask it
  logs [--lines N] Follow the engine's log
  claude [args]    Launch Claude Code pointed at this server
  env              Print the exports for a shell

Models and engines:
  pull [preset]    Download a preset's GGUF files from Hugging Face
  install          Put the pinned engine build on this machine
  setup            Inspect the machine and scaffold the home (Windows: the app's shortcuts)

Measuring:
  bench [--runs N] [--prefill N]   Speed: decode + prefill tok/s
  eval [--long N]  Quality gate: sanity, tool calls, needle at depth
  tune [preset]    Find the fastest setup this machine can serve, and gate it
  tune --auto      Pick the best installed model, then tune it

Results:
  app              The Golem window and tray (the console in the browser
                   in a build without the desktop feature)
  tui              The full-screen screen in the terminal
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
        // Bare `golem`: the console for a double-click, the screen in a
        // terminal, the usage when piped (`desktop::front_end`).
        None => match desktop::front_end(Launch::read()) {
            FrontEnd::Console => cmd_open_console().await,
            FrontEnd::Screen => cmd_tui().await,
            FrontEnd::Usage => {
                print!("{USAGE}");
                ExitCode::SUCCESS
            }
        },
        Some("report") => cmd_report(&args[1..]),
        Some("results") => cmd_results(&args[1..]),
        Some("tui") => cmd_tui().await,
        Some("app") => cmd_open_console().await,
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
        // `up` may fetch a model first, and its progress is a bar here.
        return match service::request(&home, verb, &overrides, &mut golem::progress::stdout()) {
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

/// A double-clicked `golem` (and `golem app`): the app window and its tray
/// in a build with the desktop feature, else the console in the browser -
/// either way through the service, which hosts the console. When that fails
/// there is a console window to fall back to, so the screen opens there and
/// says why.
async fn cmd_open_console() -> ExitCode {
    let home = prepare_home();
    #[cfg(feature = "desktop")]
    let opened = match golem::app::detach() {
        Ok(true) => Ok(()),
        Ok(false) => golem::app::run(&home),
        Err(e) => Err(e),
    };
    #[cfg(not(feature = "desktop"))]
    let opened = desktop::open_console(&home, &mut std::io::sink()).map(|_| ());
    match opened {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[FAIL] cannot open the console: {e}");
            cmd_tui().await
        }
    }
}

/// `golem tui`: the full-screen screen, with every action routed through the
/// service that owns the engine, or run as this binary's own verbs.
///
/// The screen decides *which* action; what an action does is this function's
/// business, which is the seam `tui::run` takes a callback for. `Console` is
/// not a verb: it opens the console the service hosts, starting the service
/// when there is none.
async fn cmd_tui() -> ExitCode {
    let home = prepare_home();
    let log = paths::job_log(&home);
    // The closure owns everything it touches: `tui::run` hands it to a
    // thread per action, so nothing here can borrow from this function.
    let act_home = home.clone();

    let outcome = tui::run(&home, move |action, arg| match action {
        tui::Action::Console => match desktop::open_console(&act_home, &mut std::io::sink()) {
            Ok(url) => format!("console on {url}"),
            Err(e) => format!("console: {e}"),
        },
        // The model and engine edits are the console's own, answered in
        // process: no engine verb, nothing for the service to run.
        tui::Action::SetDefault => match golem::state::apply(&act_home, "alias", &arg) {
            Ok("applied") => format!("{arg} is the default - it serves from the next start"),
            Ok(_) => format!("{arg} is not a preset"),
            Err(e) => format!("cannot set the default: {e}"),
        },
        tui::Action::SetEngine => match golem::state::apply(&act_home, "engine", &arg) {
            Ok("applied") => format!("engine {arg} - used from the next start"),
            Ok(_) => format!("{arg} is not an installed engine"),
            Err(e) => format!("cannot set the engine: {e}"),
        },
        tui::Action::DeleteFile => match golem::models::delete_file(&act_home, &arg) {
            Ok("deleted") => format!("deleted the model file of {arg}"),
            Ok(_) => format!("{arg}'s file was not deleted (the default's file stays)"),
            Err(e) => format!("cannot delete: {e}"),
        },
        tui::Action::RemovePreset => match golem::models::delete_preset(&act_home, &arg) {
            Ok("removed") => format!("removed the preset {arg}"),
            Ok(_) => format!("{arg} was not removed (the default stays)"),
            Err(e) => format!("cannot remove: {e}"),
        },
        other => match verb_of(other) {
            Some(verb) => describe(verb, runner::run(&act_home, verb, &arg, &log)),
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
        // The console's own words, which is what the page posts too.
        tui::Action::Start => Some("start"),
        tui::Action::Stop => Some("stop"),
        tui::Action::Restart => Some("restart"),
        tui::Action::Bench => Some("bench"),
        tui::Action::Eval => Some("eval"),
        tui::Action::Tune => Some("tune"),
        tui::Action::Report => Some("report"),
        tui::Action::Pull => Some("pull"),
        tui::Action::Console
        | tui::Action::SetDefault
        | tui::Action::SetEngine
        | tui::Action::DeleteFile
        | tui::Action::RemovePreset => None,
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
    let template = paths::PAGE;
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
        desktop::open(&out);
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
