//! `golem service`: the process that owns the engine, and the clients that
//! ask it to do things.
//!
//! ## Why there is a service at all
//!
//! Three things want the same answer, and this is it:
//!
//! - **Nothing the CLI starts should inherit the CLI's handles.** On Windows
//!   `std` spawns with `bInheritHandles = TRUE`, so an engine started by `up`
//!   held a copy of the caller's stdout pipe and `golem up | tail` waited for
//!   an EOF that only came when the engine stopped. A client that spawns
//!   nothing cannot leak anything; only the first `up` after a boot, which
//!   starts the service itself, still can.
//! - **A killed run must not leave an orphan.** The engine is a child of the
//!   service, so it goes when the service goes. A `tune` killed mid-sweep once
//!   left a 35B resident - 11.7 GB of VRAM that nothing could see, because the
//!   pid file had gone with it.
//! - **A container needs a PID 1.** `ENTRYPOINT ["golem", "up"]` exits the
//!   moment the engine is healthy; the entrypoint has to be a process that
//!   owns the engine and stays.
//!
//! So the service owns the engine, and every verb is a *client* of it
//! ([`request`]). The service runs the verb (`launch.rs`) and the output
//! streams back a line at a time; the client prints it and exits with the
//! verb's own code - which is why the verb functions take a sink instead of
//! writing to stdout directly. Each connection is served on its own thread:
//! one *start* at a time, but `down` and `status` are answered while a start
//! runs, so a start that hangs can still be stopped.
//!
//! ## The protocol
//!
//! One JSON object per line, over loopback TCP.
//!
//! ```text
//! -> {"token":"...","verb":"up"}
//! -> {"token":"...","verb":"up","env":{"GOLEM_MODELS_PRESET":"data/tuned/candidates/ornith-8.ini"}}
//! <- {"event":"line","text":"[ .. ] installing llama.cpp b11379 (cuda)..."}
//! <- {"event":"line","text":"[ OK ] router healthy on http://127.0.0.1:8000"}
//! <- {"event":"done","ok":true,"code":0}
//! ```
//!
//! Line-delimited rather than one reply at the end because the verbs are long
//! - a first `up` installs an engine and fetches a 20 GB model - and a command
//! that shows nothing for ten minutes reads as a hang. The `done` event is
//! what says "the service took it and this is how it went", which is the
//! acknowledgement a caller needs before it can report anything.
//!
//! Where the service is: `.local/golem-service.json`, holding the port it
//! bound and a token. The port is ephemeral (the OS picks it) so two checkouts
//! never fight over one, and the token is there because loopback is
//! machine-wide on Windows - without it any local process could stop your
//! engine. It is mixed from the clock and the pid rather than drawn from a
//! real source of randomness, which is enough for "a stray client must not
//! stop an engine" and not enough for anything more; there is no crate here
//! for anything more.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use golem_core::json;

use crate::config::Overrides;
use crate::engine;
use crate::launch;

/// How long a client waits for the service to answer a connection.
///
/// Short, and shorter than it looks: the service is on loopback and either
/// there or not, and on a box where the SYN to a closed port is dropped
/// rather than refused, this is the whole wait - measured at 2.07s, which the
/// start-up poll then paid once per attempt (a service that takes 200ms to
/// bind looked like it took five seconds).
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// How often `up` waits for a service it just started to write its endpoint.
const START_DEADLINE: Duration = Duration::from_secs(10);

/// How often that is checked.
const START_POLL: Duration = Duration::from_millis(100);

/// How often the accept loop looks up from the socket.
///
/// The listener is non-blocking so that Ctrl+C is noticed while nothing is
/// asking for anything, which is most of a service's life.
const ACCEPT_POLL: Duration = Duration::from_millis(100);

/// Where the service is, as the file it writes and the client reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub port: u16,
    pub token: String,
}

impl Endpoint {
    /// `.local/golem-service.json` under the home.
    pub fn path(home: &Path) -> PathBuf {
        home.join(".local").join("golem-service.json")
    }

    /// The running service, or `None` when there is none to read.
    ///
    /// A file that will not parse is the same answer as no file: a service
    /// that died mid-write must read as "not running", not as a failure the
    /// caller has to handle.
    pub fn read(home: &Path) -> Option<Endpoint> {
        let text = std::fs::read_to_string(Endpoint::path(home)).ok()?;
        let value = json::parse(&text).ok()?;
        Some(Endpoint {
            port: value.get("port")?.as_f64()? as u16,
            token: value.get("token")?.as_str()?.to_string(),
        })
    }

    /// Write it, so a client can find this service.
    fn write(home: &Path, endpoint: &Endpoint) -> std::io::Result<()> {
        let path = Endpoint::path(home);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(
            &path,
            format!("{{\"port\":{},\"token\":{}}}\n", endpoint.port, json::quote(&endpoint.token)),
        )
    }

    /// Forget it, so the next client starts a service instead of dialling a
    /// dead port.
    fn remove(home: &Path) {
        let _ = std::fs::remove_file(Endpoint::path(home));
    }
}

/// Whether a service is running and answering for this home.
pub fn running(home: &Path) -> bool {
    let Some(endpoint) = Endpoint::read(home) else { return false };
    let Ok(addr) = addr_of(&endpoint) else { return false };
    TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).is_ok()
}

/// The loopback address an endpoint names.
fn addr_of(endpoint: &Endpoint) -> Result<SocketAddr, String> {
    format!("127.0.0.1:{}", endpoint.port)
        .parse()
        .map_err(|e| format!("bad service port {}: {e}", endpoint.port))
}

/// Ask the service for one verb, starting a service when there is none, and
/// stream its output to `out`. `Ok` is the verb's own exit code.
///
/// The one way every client - the CLI, the screen, the console, the tuner -
/// reaches the engine, so the overrides travel the same road for all of them.
pub fn request(home: &Path, verb: &str, overrides: &Overrides, out: &mut dyn Write) -> Result<i32, String> {
    if let Some(code) = send(home, verb, overrides, out) {
        return Ok(code);
    }
    ensure_running(home, out)?;
    send(home, verb, overrides, out).ok_or_else(|| "the golem service started but did not answer".to_string())
}

/// Send one verb to the running service and stream its output to `out`.
///
/// `None` means there is no service to ask - no endpoint file, or nothing
/// answering on it - which is the caller's cue to start one or to fall back.
/// `Some(code)` is the verb's exit code, whatever it was.
///
/// The overrides decide which engine and preset a *start* resolves to, and
/// the service runs the verb in its own process - so a caller's environment
/// cannot reach it by inheritance, and the request is the only road there is.
pub fn send(home: &Path, verb: &str, overrides: &Overrides, out: &mut dyn Write) -> Option<i32> {
    let endpoint = Endpoint::read(home)?;
    let addr = addr_of(&endpoint).ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT).ok()?;
    // No read timeout: a verb may install an engine or fetch a 20 GB model,
    // and silence in the middle of that is not a dead service. A service that
    // really dies closes the socket, which reads as the end of the stream.
    stream.set_read_timeout(None).ok()?;
    let mut request = request_line(&endpoint.token, verb, &overrides.wire());
    request.push('\n');
    stream.write_all(request.as_bytes()).ok()?;
    stream.flush().ok()?;

    let mut code = 1;
    let mut saw_done = false;
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let Ok(value) = json::parse(&line) else { continue };
        match value.get("event").and_then(|e| e.as_str()) {
            Some("line") => {
                if let Some(text) = value.get("text").and_then(|t| t.as_str()) {
                    let _ = writeln!(out, "{text}");
                }
            }
            Some("done") => {
                code = value.get("code").and_then(|c| c.as_f64()).unwrap_or(1.0) as i32;
                saw_done = true;
                break;
            }
            // An event this build does not know: ignored rather than fatal, so
            // a newer service cannot break an older client.
            _ => {}
        }
    }
    // A stream that ended without a `done` is a service that died mid-verb.
    // Reporting its last known code would be a lie, so it is a failure.
    Some(if saw_done { code } else { 1 })
}

/// Start a service if there is not one, and wait until it answers.
///
/// The service is detached: it must outlive the command that started it, and
/// it is the process that owns the engine from then on. Its own stdout is
/// closed rather than inherited - its output goes to the client that asked for
/// the work, not to whoever happened to start it.
pub fn ensure_running(home: &Path, out: &mut dyn Write) -> Result<(), String> {
    if running(home) {
        return Ok(());
    }
    let exe = std::env::current_exe()
        .map_err(|e| format!("cannot find this binary to start the service: {e}"))?;
    let child = spawn_command(&exe, home)
        .spawn()
        .map_err(|e| format!("cannot start the golem service: {e}"))?;
    let _ = writeln!(out, "[ .. ] golem service started (pid {})", child.id());

    let deadline = Instant::now() + START_DEADLINE;
    while Instant::now() < deadline {
        if running(home) {
            return Ok(());
        }
        std::thread::sleep(START_POLL);
    }
    Err(format!(
        "the golem service did not come up within {}s",
        START_DEADLINE.as_secs()
    ))
}

/// The detached service's command line and environment.
///
/// The `GOLEM_*` overrides are stripped: the service lays every request's
/// overrides over its own environment, so one inherited from whichever
/// command happened to start it would point every later verb at that
/// command's preset or engine. A client's overrides travel with its request
/// instead. A service started directly (`golem service --up` in a container)
/// keeps its environment - that is how a container pins its bind address.
fn spawn_command(exe: &Path, home: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    command
        .arg("service")
        .env("GOLEM_HOME", home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    for key in crate::config::OVERRIDABLE {
        command.env_remove(format!("GOLEM_{key}"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// `golem service [stop|--up]`: run in the foreground and own the engine.
///
/// Foreground on purpose. It is what a container's PID 1 needs, what a
/// terminal can watch, and what a supervisor (a tray app, a systemd unit, a
/// login item) can start; the detached case is [`ensure_running`], which is
/// the same binary with the same verb and no terminal.
///
/// - **`--up`** brings the engine up as part of starting, for a PID 1 that has
///   nobody to send it a verb. It is idempotent, unlike the verb: a container
///   that finds an engine already answering serves it rather than refusing,
///   because exiting there would mean a restart loop.
/// - **`stop`** is a *client*: it asks the running service to stop the engine
///   and exit. `golem down` deliberately does not do that - stopping the
///   engine is not stopping the thing that supervises it - so this is the word
///   for "and the service too".
pub fn run(home: &Path, args: &[String]) -> i32 {
    match args {
        [] => serve(home, false),
        [flag] if flag == "--up" => serve(home, true),
        [word] if word == "stop" => match send(home, "shutdown", &Overrides::default(), &mut std::io::stdout()) {
            Some(exit) => exit,
            None => {
                println!("[INFO] no golem service is running");
                0
            }
        },
        [other, ..] => {
            eprintln!("[ERROR] service takes no arguments, `--up` or `stop` (got: {other})");
            2
        }
    }
}

/// Bring the engine up on the way into serving, for a container.
///
/// Idempotent on purpose, and that is the one place this differs from the `up`
/// verb: the launcher refuses to start on a port something already answers on,
/// which is right for a command a person typed and wrong for a PID 1 that
/// would then exit and be restarted on top of the engine it already has.
fn start_engine(home: &Path) -> i32 {
    let endpoint = engine::Endpoint::from_home(home);
    if let Some((status, _)) = engine::get(&endpoint, "/health") {
        if (200..300).contains(&status) {
            println!("[ OK ] the engine already answers on port {}", endpoint.port);
            return 0;
        }
    }
    match launch::up(home, &Overrides::from_env(), false, &mut std::io::stdout()) {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("[FAIL] {message}");
            1
        }
    }
}

/// Host the console beside the engine, for as long as the service runs.
///
/// On `CONSOLE_HOST`, loopback unless the configuration says otherwise. The
/// console has no login and its buttons start engines and delete model
/// files, so listening beyond this machine is said out loud every time. A
/// port that is taken - another console, another program - is a warning:
/// the service owns the engine either way, and that is its first job.
fn start_console(home: &Path) {
    let cfg = crate::web::Config {
        results: crate::paths::results(home),
        template: None,
        home: home.to_path_buf(),
        host: crate::desktop::console_host(home),
        port: crate::desktop::console_port(home),
    };
    if !crate::desktop::is_loopback(&cfg.host) {
        println!(
            "[WARN] the console listens on {} - anyone who can reach it can start, stop and \
             delete models; CONSOLE_HOST = \"127.0.0.1\" keeps it on this machine",
            cfg.host
        );
    }
    let address = crate::web::address(&cfg);
    let listener = match TcpListener::bind(&address) {
        Ok(listener) => listener,
        Err(e) => {
            println!("[WARN] the console cannot bind {address}: {e} - serving the engine without it");
            return;
        }
    };
    let _ = listener.set_nonblocking(true);
    match tokio::net::TcpListener::from_std(listener) {
        Ok(listener) => {
            tokio::spawn(async move {
                let _ = crate::web::serve_on(listener, cfg).await;
            });
            println!("[ OK ] console on http://{address}/");
        }
        Err(e) => println!("[WARN] the console cannot start: {e}"),
    }
}

/// The service itself: bind, announce, and serve until told to stop.
fn serve(home: &Path, start_now: bool) -> i32 {
    // One service per home: a second would fight the first for the engine,
    // its pid file and the console's port.
    if running(home) {
        let port = Endpoint::read(home).map_or(0, |e| e.port);
        eprintln!(
            "[FAIL] a golem service is already running for this home (127.0.0.1:{port}) - \
             stop it first with: golem service stop"
        );
        return 1;
    }
    let listener = match TcpListener::bind("127.0.0.1:0") {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("[ERROR] cannot bind the service socket: {e}");
            return 1;
        }
    };
    let port = match listener.local_addr() {
        Ok(addr) => addr.port(),
        Err(e) => {
            eprintln!("[ERROR] cannot read the service address: {e}");
            return 1;
        }
    };
    // The console first: once the endpoint file says the service is up, a
    // client may open the console straight away.
    start_console(home);
    let endpoint = Endpoint { port, token: new_token() };
    if let Err(e) = Endpoint::write(home, &endpoint) {
        eprintln!("[ERROR] cannot write {}: {e}", Endpoint::path(home).display());
        return 1;
    }
    println!("[ OK ] golem service on 127.0.0.1:{port} (engine home: {})", home.display());
    println!("       stop it with: golem service stop (golem down stops only the engine)");

    // Before serving, for a PID 1 with nobody to ask. Its output is this
    // process's stdout, which in a container is the container's log.
    if start_now {
        let code = start_engine(home);
        if code != 0 {
            Endpoint::remove(home);
            return code;
        }
    }

    // Ctrl+C is how a foreground service is stopped, and it has to take the
    // engine with it: the engine is this process's child, and a service that
    // dies without stopping it leaves a model resident with nothing pointing
    // at it - the orphan that once cost a session's testing.
    //
    // The watcher needs a runtime to run on, which `main` has; the accept loop
    // below is deliberately blocking otherwise, because none of the verbs are
    // async and a service with a thread pool it never uses would be a lie.
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                stop.store(true, Ordering::SeqCst);
            }
        });
    }
    // Ctrl+C is the terminal's signal; a supervisor's is SIGTERM - `docker
    // stop` sends it to PID 1, and a Linux PID 1 with no handler ignores it,
    // burning the whole grace period to a SIGKILL that never stops the
    // engine. Same flag, second watcher.
    #[cfg(unix)]
    {
        let stop = Arc::clone(&stop);
        tokio::spawn(async move {
            if let Ok(mut term) =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            {
                if term.recv().await.is_some() {
                    stop.store(true, Ordering::SeqCst);
                }
            }
        });
    }
    if listener.set_nonblocking(true).is_err() {
        eprintln!("[ERROR] cannot make the service socket pollable");
        return 1;
    }

    // One start at a time: two `up`s at once would fight over one engine, one
    // pid file and one card, so the second is refused rather than queued.
    // Everything else - `down` above all - is served while a start runs,
    // which is what lets a hung or unwanted start be stopped: the start's own
    // health wait sees its child go and reports it.
    let busy = Arc::new(AtomicBool::new(false));
    // A `shutdown` request's exit code, once its thread has stopped the engine.
    let shutdown: Arc<Mutex<Option<i32>>> = Arc::new(Mutex::new(None));
    let code = loop {
        if let Some(code) = *shutdown.lock().unwrap_or_else(|e| e.into_inner()) {
            break code;
        }
        if stop.load(Ordering::SeqCst) {
            println!();
            println!("[ .. ] stopping the engine and the service");
            break launch::down(home, &mut std::io::stdout());
        }
        let stream = match listener.accept() {
            Ok((stream, _)) => stream,
            // Nothing asking, or a connection that failed to arrive: neither
            // is a reason to stop serving the ones that will.
            Err(_) => {
                std::thread::sleep(ACCEPT_POLL);
                continue;
            }
        };
        let (home, token) = (home.to_path_buf(), endpoint.token.clone());
        let (busy, shutdown) = (Arc::clone(&busy), Arc::clone(&shutdown));
        std::thread::spawn(move || serve_one(&home, &token, stream, &busy, &shutdown));
    };
    Endpoint::remove(home);
    println!("[ OK ] golem service stopped");
    code
}

/// One connection: read the request, run the verb, answer `done`.
fn serve_one(home: &Path, token: &str, mut stream: TcpStream, busy: &AtomicBool, shutdown: &Mutex<Option<i32>>) {
    let _ = stream.set_nonblocking(false);
    let (verb, overrides) = match read_request(&mut stream, token) {
        Ok(request) => request,
        Err(message) => {
            let _ = writeln!(stream, "{}", line_event(&message));
            let _ = writeln!(stream, "{}", done_event(false, 1));
            return;
        }
    };
    if verb == "shutdown" {
        let _ = writeln!(stream, "{}", line_event("[ .. ] stopping the engine and the service"));
        let stopped = {
            let mut stdout = std::io::stdout();
            let mut sink = Sink::new(Some(&mut stream), &mut stdout);
            let stopped = launch::down(home, &mut sink);
            let _ = sink.flush();
            stopped
        };
        let _ = writeln!(stream, "{}", done_event(stopped == 0, stopped));
        *shutdown.lock().unwrap_or_else(|e| e.into_inner()) = Some(stopped);
        return;
    }
    let starts = matches!(verb.as_str(), "up" | "restart");
    if starts && busy.swap(true, Ordering::SeqCst) {
        let _ = writeln!(stream, "{}", line_event("[FAIL] the engine is already being started"));
        let _ = writeln!(stream, "{}", done_event(false, 1));
        return;
    }
    // The service's own `GOLEM_*` environment is the floor every request
    // stands on: a container sets `GOLEM_LOCAL_AI_HOST=0.0.0.0` once, for the
    // service, and a request that does not mention it (the tuner putting the
    // engine back, a button on the page) must not bind the loopback.
    let overrides = Overrides::from_env().under(&overrides);
    dispatch(home, &verb, &overrides, &mut stream);
    if starts {
        busy.store(false, Ordering::SeqCst);
    }
}

/// Run one verb, streaming its output to the client that asked for it.
fn dispatch(home: &Path, verb: &str, overrides: &Overrides, stream: &mut TcpStream) -> i32 {
    // The sink borrows the socket, so it lives in its own scope: the `done`
    // event below needs the socket back.
    let code = {
        // `&mut *stream`, a reborrow, so the socket is still ours once the
        // sink is finished with it.
        let mut stdout = std::io::stdout();
        let mut sink = Sink::new(Some(&mut *stream), &mut stdout);
        let code = match verb {
            "up" => match launch::up(home, overrides, false, &mut sink) {
                Ok(()) => 0,
                Err(message) => {
                    let _ = writeln!(sink, "[FAIL] {message}");
                    1
                }
            },
            "down" => launch::down(home, &mut sink),
            "restart" => match launch::restart(home, overrides, &mut sink) {
                Ok(()) => 0,
                Err(message) => {
                    let _ = writeln!(sink, "[FAIL] {message}");
                    1
                }
            },
            "status" => launch::status(home, &mut sink),
            other => {
                let _ = writeln!(sink, "[FAIL] {other} is not a service verb");
                2
            }
        };
        let _ = sink.flush();
        code
    };
    let _ = writeln!(stream, "{}", done_event(code == 0, code));
    code
}

/// The request line: one JSON object, with `env` inside it when there are
/// overrides.
///
/// A function of its three parts because the shape is easy to get wrong in a
/// way no test of the *parser* catches. The first version closed the object
/// before appending `env`, so a request carrying an override was two JSON
/// values in a row and the service answered "the request is not JSON" to
/// every verb the tuner sent - which is what `the_request_line_is_one_json_object`
/// exists for.
fn request_line(token: &str, verb: &str, env: &[(String, String)]) -> String {
    let mut line = format!("{{\"token\":{},\"verb\":{}", json::quote(token), json::quote(verb));
    if !env.is_empty() {
        let members: Vec<String> = env
            .iter()
            .map(|(key, value)| format!("{}:{}", json::quote(key), json::quote(value)))
            .collect();
        line.push_str(&format!(",\"env\":{{{}}}", members.join(",")));
    }
    line.push('}');
    line
}

/// Read the request line and check the token.
fn read_request(stream: &mut TcpStream, token: &str) -> Result<(String, Overrides), String> {
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut line = String::new();
    reader.read_line(&mut line).map_err(|e| format!("cannot read the request: {e}"))?;
    let value = json::parse(line.trim()).map_err(|_| "the request is not JSON".to_string())?;
    let given = value.get("token").and_then(|t| t.as_str()).unwrap_or_default();
    if given != token {
        return Err("[FAIL] the request did not carry this service's token".to_string());
    }
    let verb = value
        .get("verb")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| "[FAIL] the request named no verb".to_string())?;
    let overrides = Overrides::from_wire(&env_of(&value)?).map_err(|e| format!("[FAIL] {e}"))?;
    Ok((verb, overrides))
}

/// The request's `env` member as pairs, or empty when it is not there.
///
/// Absent and empty are the same answer, which is what an older client
/// sends: no overrides, nothing to carry. Everything else has to be an
/// object of strings, or the request is not one this protocol knows.
fn env_of(value: &json::Value) -> Result<Vec<(String, String)>, String> {
    let Some(env) = value.get("env") else { return Ok(Vec::new()) };
    match env {
        json::Value::Object(members) => members
            .iter()
            .map(|(key, value)| match value {
                json::Value::String(text) => Ok((key.clone(), text.clone())),
                _ => Err("the request's env is not an object of strings".to_string()),
            })
            .collect(),
        _ => Err("the request's env is not an object of strings".to_string()),
    }
}

/// A per-run token, so a stray local process cannot stop an engine.
fn new_token() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:016x}{:08x}", nanos, std::process::id())
}

/// One `line` event.
fn line_event(text: &str) -> String {
    format!("{{\"event\":\"line\",\"text\":{}}}", json::quote(text))
}

/// The `done` event, which is the acknowledgement a caller waits for.
fn done_event(ok: bool, code: i32) -> String {
    format!("{{\"event\":\"done\",\"ok\":{ok},\"code\":{code}}}")
}

/// Where a verb's output goes: the client that asked for it, and this
/// process's own stdout.
///
/// Two destinations because there are two ways to run the service. A `golem
/// up` from a terminal wants the lines on the client that asked; a container
/// has no client at all - the service is PID 1 and its stdout *is* the
/// container's log - so the same lines go there too. Neither is allowed to
/// fail the verb: a client that hung up mid-install must not abort it.
///
/// Lines are assembled here rather than passed through, because the protocol
/// is one JSON object per line and `writeln!` reaches a `Write` in as many
/// pieces as its format arguments: `"[ OK ] "`, the value, `"\n"`.
struct Sink<'a> {
    client: Option<&'a mut dyn Write>,
    local: &'a mut dyn Write,
    /// What has arrived since the last newline.
    pending: String,
}

impl<'a> Sink<'a> {
    fn new(client: Option<&'a mut dyn Write>, local: &'a mut dyn Write) -> Sink<'a> {
        Sink { client, local, pending: String::new() }
    }

    /// Hand one finished line to both destinations.
    fn emit(&mut self, line: &str) {
        let _ = writeln!(self.local, "{line}");
        if let Some(client) = self.client.as_deref_mut() {
            let _ = writeln!(client, "{}", line_event(line));
        }
    }
}

impl Write for Sink<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.pending.push_str(&String::from_utf8_lossy(buf));
        while let Some(at) = self.pending.find('\n') {
            let line: String = self.pending.drain(..=at).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            self.emit(line);
        }
        // Everything was taken, whatever happened downstream: a sink that
        // reports a short write would make `writeln!` fail, and a client that
        // went away is not a reason to fail a start.
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // A verb whose last line had no newline - the exports block ends with
        // one, but nothing guarantees it - still belongs to the client.
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.emit(&line);
        }
        if let Some(client) = self.client.as_deref_mut() {
            let _ = client.flush();
        }
        self.local.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-service-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".local")).expect("temp");
        dir
    }

    #[test]
    fn an_auto_started_service_inherits_no_overrides() {
        // Removed explicitly, so it holds whatever the test runner's own
        // environment carries.
        let command = spawn_command(Path::new("golem"), Path::new("/h"));
        let envs: Vec<_> = command.get_envs().collect();
        for key in crate::config::OVERRIDABLE {
            let name = format!("GOLEM_{key}");
            assert!(
                envs.iter().any(|(k, v)| *k == name.as_str() && v.is_none()),
                "{name} is not stripped: {envs:?}"
            );
        }
        assert!(
            envs.iter().any(|(k, v)| *k == "GOLEM_HOME" && v.is_some()),
            "the home is still handed on: {envs:?}"
        );
    }

    #[test]
    fn the_endpoint_round_trips_through_the_file() {
        let dir = home("endpoint");
        assert_eq!(Endpoint::read(&dir), None, "no file is no service");
        let endpoint = Endpoint { port: 8123, token: "abc123".to_string() };
        Endpoint::write(&dir, &endpoint).expect("write");
        assert_eq!(Endpoint::read(&dir), Some(endpoint.clone()));
        Endpoint::remove(&dir);
        assert_eq!(Endpoint::read(&dir), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_endpoint_file_that_will_not_parse_reads_as_no_service() {
        // A service killed mid-write must look like one that is not running,
        // not like a failure the caller has to handle.
        let dir = home("endpoint-bad");
        std::fs::write(Endpoint::path(&dir), "{\"port\":").expect("write");
        assert_eq!(Endpoint::read(&dir), None);
        assert!(!running(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn nothing_answering_is_not_a_running_service() {
        // The endpoint names a port with nothing on it: port 1 is reserved and
        // nothing may bind it without privilege.
        let dir = home("endpoint-dead");
        Endpoint::write(&dir, &Endpoint { port: 1, token: "t".to_string() }).expect("write");
        assert!(!running(&dir), "a written endpoint is not a running service");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_events_are_one_json_object_per_line() {
        assert_eq!(
            line_event("[ OK ] engine up"),
            "{\"event\":\"line\",\"text\":\"[ OK ] engine up\"}"
        );
        assert_eq!(done_event(true, 0), "{\"event\":\"done\",\"ok\":true,\"code\":0}");
        assert_eq!(done_event(false, 2), "{\"event\":\"done\",\"ok\":false,\"code\":2}");
        // A quote in the text must not end the string early: the client parses
        // this, and the whole point is that a line is a line.
        assert_eq!(
            line_event("a \"quoted\" line"),
            "{\"event\":\"line\",\"text\":\"a \\\"quoted\\\" line\"}"
        );
    }

    #[test]
    fn the_sink_splits_whatever_it_is_handed_into_lines() {
        // `writeln!` reaches a `Write` in as many pieces as its format
        // arguments, so a sink that forwarded writes would put three events on
        // one line and one event across two.
        let mut client: Vec<u8> = Vec::new();
        let mut local: Vec<u8> = Vec::new();
        {
            let mut sink = Sink::new(Some(&mut client), &mut local);
            write!(sink, "[ OK ] {} and {}", "one", "two").expect("write");
            writeln!(sink).expect("newline");
            write!(sink, "second").expect("write");
            writeln!(sink, " line").expect("write");
            sink.flush().expect("flush");
        }
        let client = String::from_utf8(client).expect("utf8");
        assert_eq!(
            client,
            "{\"event\":\"line\",\"text\":\"[ OK ] one and two\"}\n\
             {\"event\":\"line\",\"text\":\"second line\"}\n"
        );
        assert_eq!(String::from_utf8(local).expect("utf8"), "[ OK ] one and two\nsecond line\n");
    }

    #[test]
    fn a_last_line_with_no_newline_still_reaches_the_client() {
        let mut client: Vec<u8> = Vec::new();
        let mut local: Vec<u8> = Vec::new();
        {
            let mut sink = Sink::new(Some(&mut client), &mut local);
            write!(sink, "no newline here").expect("write");
            sink.flush().expect("flush");
        }
        assert_eq!(
            String::from_utf8(client).expect("utf8"),
            "{\"event\":\"line\",\"text\":\"no newline here\"}\n"
        );
    }

    #[test]
    fn a_sink_with_no_client_is_the_container_case() {
        // PID 1 has nobody to answer: the same lines, only to stdout.
        let mut local: Vec<u8> = Vec::new();
        {
            let mut sink = Sink::new(None, &mut local);
            writeln!(sink, "[ OK ] serving").expect("write");
            sink.flush().expect("flush");
        }
        assert_eq!(String::from_utf8(local).expect("utf8"), "[ OK ] serving\n");
    }

    #[test]
    fn the_request_line_is_one_json_object_with_the_env_inside_it() {
        // A request the service cannot parse is a verb that never runs, and
        // the parser is not where that goes wrong: the object has to be
        // closed *after* `env` is appended, not before. The first version
        // closed it first, and every override-carrying verb came back as
        // "the request is not JSON" - measured live, which is why this test
        // builds the line and parses it back rather than trusting the shape.
        let line = request_line("tok", "up", &[]);
        assert_eq!(line, "{\"token\":\"tok\",\"verb\":\"up\"}");
        let value = json::parse(&line).expect("a request with no env is one object");
        assert_eq!(value.get("verb").and_then(|v| v.as_str()), Some("up"));

        let env = vec![("GOLEM_MODELS_PRESET".to_string(), "config/models.ini".to_string())];
        let line = request_line("tok", "up", &env);
        let value = json::parse(&line).expect("a request with env is still one object");
        assert_eq!(value.get("verb").and_then(|v| v.as_str()), Some("up"));
        assert_eq!(env_of(&value).expect("env parses"), env, "and the env is inside it");

        // A quote in a value must not end the string early: what this writes
        // has to be what the other end can parse.
        let odd = vec![("GOLEM_MODEL_ALIAS".to_string(), "a \"quoted\" model".to_string())];
        let value = json::parse(&request_line("tok", "up", &odd)).expect("escaped, still JSON");
        assert_eq!(env_of(&value).expect("env parses"), odd);
    }

    #[test]
    fn the_token_is_not_the_same_twice() {
        // Not a guarantee of secrecy, but two services started in the same
        // second on the same pid cannot happen, and this is what makes a stale
        // endpoint file useless to a later client.
        let first = new_token();
        assert!(first.len() >= 16, "{first}");
        assert_ne!(first, new_token());
    }

    #[test]
    fn an_absent_or_empty_env_is_no_overrides() {
        // An older client sends no `env` at all, and an empty object says the
        // same thing: nothing to carry.
        let value = json::parse("{\"verb\":\"up\"}").expect("json");
        assert_eq!(env_of(&value).expect("env"), Vec::new());
        let value = json::parse("{\"verb\":\"up\",\"env\":{}}").expect("json");
        assert_eq!(env_of(&value).expect("env"), Vec::new());
    }

    #[test]
    fn an_env_of_strings_comes_back_as_pairs() {
        let value = json::parse(
            "{\"env\":{\"GOLEM_ENGINE\":\"llama.cpp\",\"GOLEM_MODELS_PRESET\":\"data/x.ini\"}}",
        )
        .expect("json");
        assert_eq!(
            env_of(&value).expect("env"),
            vec![
                ("GOLEM_ENGINE".to_string(), "llama.cpp".to_string()),
                ("GOLEM_MODELS_PRESET".to_string(), "data/x.ini".to_string()),
            ]
        );
    }

    #[test]
    fn an_env_that_is_not_an_object_of_strings_is_refused() {
        // A value that is not a string cannot ride the protocol, and neither
        // can an `env` that is not an object at all.
        let value = json::parse("{\"env\":{\"GOLEM_ENGINE\":3}}").expect("json");
        assert_eq!(
            env_of(&value).unwrap_err(),
            "the request's env is not an object of strings"
        );
        let value = json::parse("{\"env\":[\"GOLEM_ENGINE\"]}").expect("json");
        assert_eq!(
            env_of(&value).unwrap_err(),
            "the request's env is not an object of strings"
        );
    }
}
