//! The web console (phase 2 of docs/plan-portable-binary.md).
//!
//! It replaces the read-only surface of `scripts/web.ps1`: the results page,
//! served live from `results.jsonl`, and the raw file the page polls so the
//! jsonl stays the single source of truth. The controls (`/api/run`,
//! `/api/state`, ...) are phase 3 and are not here.
//!
//! The page is the template with the records inlined, exactly as
//! `scripts/report.sh` and `./golem report` do it: the file's own lines, not a
//! re-serialization, so the page can never disagree with the data it shows.
//!
//! The two pure helpers (`inline_records`, `render_page`) are free of I/O and
//! of the server, so the console's one interesting transform is testable on
//! its own; the handlers only add the reading and the error shape.

use std::path::PathBuf;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};

use crate::driver;
use crate::state::{self, Job, SharedJob};

/// Where the console reads from and how it binds.
///
/// `host` and `port` default to `127.0.0.1` and `8787` (the `web.port`
/// default in the shell); the paths point at the repository's own files.
#[derive(Debug, Clone)]
pub struct Config {
    /// e.g. `bench/results.jsonl`
    pub results: PathBuf,
    /// e.g. `bench/report.template.html`
    pub template: PathBuf,
    /// default `"127.0.0.1"`
    pub host: String,
    /// default `8787`
    pub port: u16,
    /// The home the console reads the engine's state from (`.env`, the
    /// preset file, the logs) and runs the launchers in.
    pub home: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            results: PathBuf::from("bench/results.jsonl"),
            template: PathBuf::from("bench/report.template.html"),
            host: "127.0.0.1".to_string(),
            port: 8787,
            home: PathBuf::from("."),
        }
    }
}

/// The routes' shared state: the configuration and the one job slot.
///
/// The shell console runs one job at a time and says so; so does this one,
/// because one engine and one measurement cannot be two things at once.
struct App {
    cfg: Config,
    job: SharedJob,
}

fn app(cfg: Config) -> Arc<App> {
    Arc::new(App { cfg, job: Arc::new(std::sync::Mutex::new(Job::default())) })
}

/// `[{...},\n{...}]` from the file's record lines.
///
/// Each line is trimmed; only lines starting with `{` are kept (a trailing
/// newline or a stray blank line is not a record). `</` is escaped to `<\/`
/// because the data lives inside a `<script>` block and a literal `</script>`
/// in a record would close it early. Empty input becomes `[]`.
pub fn inline_records(raw: &str) -> String {
    let records: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('{'))
        .map(|line| line.replace("</", "<\\/"))
        .collect();
    format!("[{}]", records.join(",\n"))
}

/// The template with the literal `/*GOLEM_DATA*/` replaced by `data`.
///
/// Kept pure and separate so the replacement can be tested without a server
/// or a file.
pub fn render_page(template: &str, data: &str) -> String {
    template.replace("/*GOLEM_DATA*/", data)
}

/// The console's routes, with its configuration as the state.
///
/// `GET /` renders the page; `GET /results.jsonl` returns the raw file; every
/// other path is a 404. Exposed so tests can drive the router without binding
/// a well-known port.
pub fn router(cfg: Config) -> axum::Router {
    axum::Router::new()
        .route("/", get(index))
        .route("/results.jsonl", get(results_jsonl))
        .route("/api/state", get(api_state))
        .route("/api/logs", get(api_logs))
        .route("/api/run", post(api_run))
        .fallback(not_found)
        .with_state(app(cfg))
}

/// The address the console binds.
pub fn address(cfg: &Config) -> String {
    format!("{}:{}", cfg.host, cfg.port)
}

/// Bind `cfg.host:cfg.port` and serve the console until the process ends.
pub async fn serve(cfg: Config) -> std::io::Result<()> {
    let listener = tokio::net::TcpListener::bind(address(&cfg)).await?;
    serve_on(listener, cfg).await
}

/// Serve on a listener that is already bound.
///
/// Split from [`serve`] so a caller that has to know *now* whether the port
/// is free - the TUI, which starts the console from a key press and has to
/// report the outcome in the same breath - can bind synchronously, hand the
/// socket over, and only then spawn the serving.
pub async fn serve_on(listener: tokio::net::TcpListener, cfg: Config) -> std::io::Result<()> {
    axum::serve(listener, router(cfg)).await
}

/// `GET /`: the template with the records inlined, regenerated per request so
/// a new bench row shows up without a restart.
///
/// A results file that is not there yet is an empty database, not an error:
/// the console is meant to stay up on a fresh machine, where nothing has been
/// measured, and the page renders an empty table happily. A file that exists
/// but cannot be read is a real error and says so.
async fn index(State(app): State<Arc<App>>) -> Response {
    let template = match tokio::fs::read_to_string(&app.cfg.template).await {
        Ok(t) => t,
        Err(e) => return read_error(&app.cfg.template, &e),
    };
    let raw = match read_results(&app.cfg.results).await {
        Ok(r) => r,
        Err(e) => return read_error(&app.cfg.results, &e),
    };
    let page = render_page(&template, &inline_records(&raw));
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], page).into_response()
}

/// `GET /results.jsonl`: the file's raw bytes, so the page polls the same
/// bytes a reader of the file would see. Empty when nothing has been
/// measured yet, for the same reason as above.
async fn results_jsonl(State(app): State<Arc<App>>) -> Response {
    match read_results(&app.cfg.results).await {
        Ok(raw) => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            raw.into_bytes(),
        )
            .into_response(),
        Err(e) => read_error(&app.cfg.results, &e),
    }
}

/// The results file, with "not there yet" read as "empty".
async fn read_results(path: &std::path::Path) -> std::io::Result<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(raw) => Ok(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e),
    }
}

/// Anything that is not a route above.
async fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        "not found",
    )
        .into_response()
}

/// A missing or unreadable file is a plain-text 500 with a short message -
/// never a panic, which would take the whole console down over one file.
fn read_error(path: &std::path::Path, err: &std::io::Error) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("cannot read {}: {err}", path.display()),
    )
        .into_response()
}

fn plain(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        message.into(),
    )
        .into_response()
}

fn json(status: StatusCode, body: String) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json; charset=utf-8")],
        body,
    )
        .into_response()
}

/// A JSON string literal. The values here are names and paths from the
/// user's own files, so quotes and backslashes are the only things that
/// need escaping, plus the control characters a note could carry.
fn json_str(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `GET /api/state`: what the page's status strip shows.
async fn api_state(State(app): State<Arc<App>>) -> Response {
    let home = app.cfg.home.clone();
    let job = app.job.clone();
    let snapshot = tokio::task::spawn_blocking(move || state::snapshot(&home, &job)).await;
    let Ok(snapshot) = snapshot else {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "state unavailable");
    };
    // Every field the page's status strip and dropdowns read. `engines` and
    // `job_tail` are as load-bearing as the rest: without them the engine
    // dropdown comes up empty and the running job's log never appears.
    let quoted = |values: &[String]| -> String {
        values.iter().map(|v| json_str(v)).collect::<Vec<_>>().join(", ")
    };
    let vram = snapshot.vram_mib.map(|v| v.to_string()).unwrap_or_else(|| "null".to_string());
    json(
        StatusCode::OK,
        format!(
            "{{\"up\": {}, \"vram\": {}, \"alias\": {}, \"engine\": {}, \"presets\": [{}], \
             \"engines\": [{}], \"job_running\": {}, \"job_action\": {}, \"job_tail\": {}}}",
            snapshot.up,
            vram,
            json_str(&snapshot.alias),
            json_str(&snapshot.engine),
            quoted(&snapshot.presets),
            quoted(&snapshot.engines),
            snapshot.job_running,
            json_str(&snapshot.job_action),
            json_str(&snapshot.job_tail),
        ),
    )
}

/// `GET /api/logs`: the tail of the engine's log.
async fn api_logs(State(app): State<Arc<App>>) -> Response {
    let home = app.cfg.home.clone();
    let tail = tokio::task::spawn_blocking(move || state::log_tail(&home, 200)).await;
    let Ok((file, text)) = tail else {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "log unavailable");
    };
    json(
        StatusCode::OK,
        format!("{{\"file\": {}, \"text\": {}}}", json_str(&file), json_str(&text)),
    )
}

/// `POST /api/run` with `action=<verb>&arg=<...>`.
///
/// One job at a time, the same rule the shell console has: the engine is a
/// single resource and two benchmarks at once measure each other. The reply
/// comes back immediately; the page follows the job through `api/state`.
async fn api_run(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let form = parse_form(&body);
    let action = form.get("action").cloned().unwrap_or_default();
    if !driver::is_action(&action) {
        return plain(StatusCode::BAD_REQUEST, format!("unknown action: {action:?}"));
    }
    let Some(launcher) = driver::Driver::find(&app.cfg.home) else {
        return plain(
            StatusCode::SERVICE_UNAVAILABLE,
            "no launcher in this folder (golem.cmd or golem) - controls need the scripts until phase 3 finishes",
        );
    };

    // Take the job slot, or say who has it. A guard cannot call a &mut
    // method, so this is a plain match rather than a condition on the arm.
    match app.job.lock() {
        Ok(mut job) => {
            if !job.begin(&action) {
                return plain(
                    StatusCode::CONFLICT,
                    format!("a job is already running ({})", job.action()),
                );
            }
        }
        Err(_) => return plain(StatusCode::INTERNAL_SERVER_ERROR, "job state is poisoned"),
    }

    let log = app.cfg.home.join("logs/web-job.log");
    let job = app.job.clone();
    let started = action.clone();
    tokio::task::spawn_blocking(move || {
        let outcome = driver::run(&launcher, &started, &log);
        if let Ok(mut job) = job.lock() {
            job.finish();
        }
        outcome
    });

    plain(StatusCode::OK, format!("{action} started (log: logs/web-job.log)"))
}

/// `a=b&c=d` into a map, with `+` as space and `%XX` decoded: the page sends
/// `application/x-www-form-urlencoded`, and pulling in a form crate for two
/// keys would cost more than this.
fn parse_form(body: &[u8]) -> std::collections::HashMap<String, String> {
    let text = String::from_utf8_lossy(body);
    let mut out = std::collections::HashMap::new();
    for pair in text.split('&') {
        let Some((key, value)) = pair.split_once('=') else { continue };
        out.insert(url_decode(key), url_decode(value));
    }
    out
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(byte) => {
                        out.push(byte);
                        i += 3;
                    }
                    Err(_) => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn json_strings_escape_what_they_must() {
        assert_eq!(json_str("plain"), "\"plain\"");
        assert_eq!(json_str("a\"b\\c\nd"), "\"a\\\"b\\\\c\\nd\"");
    }

    #[test]
    fn forms_decode_plus_and_percent() {
        let form = parse_form(b"action=tune&arg=my+model%2F1");
        assert_eq!(form.get("action").map(String::as_str), Some("tune"));
        assert_eq!(form.get("arg").map(String::as_str), Some("my model/1"));
    }

    #[test]
    fn a_bad_percent_sequence_is_kept_verbatim() {
        assert_eq!(url_decode("100%zz"), "100%zz");
        assert_eq!(url_decode("50%25"), "50%");
    }
}
