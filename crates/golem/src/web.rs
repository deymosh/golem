//! The console `golem service` hosts: results, models, controls, the live
//! log.
//!
//! The results page, served live from `results.jsonl`, and the raw file the
//! page polls so the jsonl stays the single source of truth; the controls
//! (`/api/state`, `/api/logs`, `/api/run`); and the Models tab
//! (`/api/models` and the three edits behind it).
//!
//! The page is the template with the records inlined, exactly as
//! `./golem report` does it: the file's own lines, not a
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

use golem_core::json;

use crate::runner;
use crate::models;
use crate::state::{self, Job, SharedJob};

/// Where the console reads from and how it binds.
///
/// `host` and `port` default to `127.0.0.1` and `8787`; the paths point at
/// the home's own files.
#[derive(Debug, Clone)]
pub struct Config {
    /// e.g. `results/results.jsonl`
    pub results: PathBuf,
    /// The page to render: `None` for the compiled-in one, which is what
    /// the service serves; a path for a test's fixture.
    pub template: Option<PathBuf>,
    /// default `"127.0.0.1"`
    pub host: String,
    /// default `8787`
    pub port: u16,
    /// The home the console reads the engine's state from (golem.toml, the
    /// preset file, the logs) and runs its jobs in.
    pub home: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            results: PathBuf::from("results/results.jsonl"),
            template: None,
            host: "127.0.0.1".to_string(),
            port: 8787,
            home: PathBuf::from("."),
        }
    }
}

/// The routes' shared state: the configuration and the one job slot.
///
/// One job at a time, and the console says so: one engine and one
/// measurement cannot be two things at once.
struct App {
    cfg: Config,
    job: SharedJob,
}

fn app(cfg: Config) -> Arc<App> {
    Arc::new(App { cfg, job: Arc::new(std::sync::Mutex::new(Job::default())) })
}

/// `[{...},\n{...}]` from the file's record lines.
///
/// Each line is trimmed; only lines that parse as a JSON object are kept (a
/// trailing newline, a stray blank line or a write cut short is not a
/// record, and one bad line must not cost the page the array). `</` is escaped to `<\/`
/// because the data lives inside a `<script>` block and a literal `</script>`
/// in a record would close it early. Empty input becomes `[]`.
pub fn inline_records(raw: &str) -> String {
    let records: Vec<String> = raw
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('{') && json::parse(line).is_ok())
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
    let bind_host = cfg.host.clone();
    axum::Router::new()
        .route("/", get(index))
        .route("/results.jsonl", get(results_jsonl))
        .route("/api/state", get(api_state))
        .route("/api/logs", get(api_logs))
        .route("/api/chat", get(api_chat))
        .route("/api/run", post(api_run))
        .route("/api/models", get(api_models))
        .route("/api/addmodel", post(api_addmodel))
        .route("/api/delmodel", post(api_delmodel))
        .route("/api/delpreset", post(api_delpreset))
        .route("/api/settings", get(api_settings).post(api_set))
        .fallback(not_found)
        .layer(axum::middleware::from_fn(move |request: axum::extract::Request, next: axum::middleware::Next| {
            let bind_host = bind_host.clone();
            async move {
                let headers = request.headers();
                let text = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
                if !same_origin(&Asked {
                    host: text("host"),
                    origin: text("origin"),
                    fetch_site: text("sec-fetch-site"),
                    bind_host: &bind_host,
                }) {
                    return plain(StatusCode::FORBIDDEN, "the console answers only its own page");
                }
                next.run(request).await
            }
        }))
        .with_state(app(cfg))
}

/// What a request says about where it came from.
struct Asked<'a> {
    host: Option<String>,
    origin: Option<String>,
    fetch_site: Option<String>,
    /// The address the console is bound to (`CONSOLE_HOST`).
    bind_host: &'a str,
}

/// Whether a request is the console's own page (or a client that is not a
/// browser) rather than another site acting through this browser.
///
/// The console has no login - it only runs this machine's engine - so what
/// keeps a web page elsewhere from posting `delmodel` to 127.0.0.1:8787 is
/// this: a browser names the page that sent a request (`Origin`,
/// `Sec-Fetch-Site`), and one from another site is refused. The `Host` is
/// checked too, against DNS rebinding (a name of the attacker's that comes to
/// resolve to 127.0.0.1): it must be an address, `localhost`, or the host the
/// console was bound to. curl and scripts send no `Origin` and pass.
fn same_origin(asked: &Asked) -> bool {
    let Some(host) = asked.host.as_deref() else { return false };
    let name = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(name, port)| {
            if port.bytes().all(|b| b.is_ascii_digit()) { name } else { host }
        }),
    };
    let literal = name.parse::<std::net::IpAddr>().is_ok();
    if !(literal || name.eq_ignore_ascii_case("localhost") || name.eq_ignore_ascii_case(asked.bind_host)) {
        return false;
    }
    if let Some(origin) = asked.origin.as_deref() {
        let origin_host = origin.split_once("://").map_or(origin, |(_, rest)| rest).trim_end_matches('/');
        // The desktop window's page is served from this same address, so
        // its Origin matches as well.
        return origin_host.eq_ignore_ascii_case(host);
    }
    !matches!(asked.fetch_site.as_deref(), Some("cross-site" | "same-site"))
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
    let template = match &app.cfg.template {
        None => crate::paths::page().to_string(),
        Some(path) => match tokio::fs::read_to_string(path).await {
            Ok(t) => t,
            Err(e) => return read_error(path, &e),
        },
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
        values.iter().map(|v| json::quote(v)).collect::<Vec<_>>().join(", ")
    };
    let vram = snapshot.vram_mib.map(|v| v.to_string()).unwrap_or_else(|| "null".to_string());
    let update = match &snapshot.update {
        Some(u) => format!(
            "{{\"current\": {}, \"latest\": {}, \"url\": {}, \"installed\": {}}}",
            json::quote(&u.current),
            json::quote(&u.latest),
            json::quote(&u.url),
            u.installed
        ),
        None => "null".to_string(),
    };
    json(
        StatusCode::OK,
        format!(
            "{{\"up\": {}, \"vram\": {}, \"alias\": {}, \"engine\": {}, \"presets\": [{}], \
             \"engines\": [{}], \"job_running\": {}, \"job_action\": {}, \"job_tail\": {}, \
             \"job_progress\": {}, \"update\": {}, \"web\": {}, \"version\": {}}}",
            snapshot.up,
            vram,
            json::quote(&snapshot.alias),
            json::quote(&snapshot.engine),
            quoted(&snapshot.presets),
            quoted(&snapshot.engines),
            snapshot.job_running,
            json::quote(&snapshot.job_action),
            json::quote(&snapshot.job_tail),
            snapshot.job_progress.as_ref().map_or_else(|| "null".to_string(), |p| p.to_json()),
            update,
            // Whether the API front adds golem's web tools, for the page's
            // Connect panel.
            crate::api::web_tools_on(&app.cfg.home),
            // The version of the service answering, which is the one the
            // page names beside the brand.
            json::quote(env!("CARGO_PKG_VERSION")),
        ),
    )
}

/// `GET /api/chat`: where the engine's own chat page is, and the key it asks
/// for once.
///
/// The chat page is llama-server's, served by the engine on its own port,
/// and the console's Chat tab frames it. It keeps the API key in its own
/// origin's storage, which no other page can write, and the engine's
/// `/props` (where `--webui-config` defaults would come from) is behind the
/// same key - so the key is handed to the person, to paste once. It is no
/// more than the console already gives away: the same-origin guard keeps
/// other sites out, and whoever reaches the console runs the engine anyway.
async fn api_chat(State(app): State<Arc<App>>) -> Response {
    let home = &app.cfg.home;
    let host = crate::config::get(home, "LOCAL_AI_HOST").unwrap_or_else(|| "127.0.0.1".to_string());
    json(
        StatusCode::OK,
        format!(
            "{{\"host\": {}, \"port\": {}, \"key\": {}}}",
            json::quote(&host),
            json::quote(&crate::client::port(home)),
            // The key the engine is started with, default included.
            json::quote(&crate::client::api_key(home))
        ),
    )
}

/// `GET /api/logs`: the tail of the engine's log, as it is (`text`) and read
/// (`lines`, `golem_core::enginelog`). The tail is long because most of the
/// router's lines fold away in the reading.
async fn api_logs(State(app): State<Arc<App>>) -> Response {
    let home = app.cfg.home.clone();
    let tail = tokio::task::spawn_blocking(move || state::log_tail(&home, 600)).await;
    let Ok((file, text)) = tail else {
        return plain(StatusCode::INTERNAL_SERVER_ERROR, "log unavailable");
    };
    let lines = golem_core::enginelog::to_json(&golem_core::enginelog::read(&text));
    json(
        StatusCode::OK,
        format!("{{\"file\": {}, \"text\": {}, \"lines\": {lines}}}", json::quote(&file), json::quote(&text)),
    )
}

/// `GET /api/models`: every preset, for the page's Models tab.
///
/// Reads the preset file, stats each model file and parses the GGUF headers
/// of the ones that are here, so it is off the async runtime - a header read
/// is instant, but there are several of them and a request thread should not
/// be the one waiting.
async fn api_models(State(app): State<Arc<App>>) -> Response {
    let home = app.cfg.home.clone();
    match tokio::task::spawn_blocking(move || models::list(&home)).await {
        Ok(list) => json(StatusCode::OK, models::to_json(&list)),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "models unavailable"),
    }
}

/// `POST /api/addmodel`: append a preset for a model that is not here yet.
async fn api_addmodel(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let form = parse_form(&body);
    let new = models::NewModel {
        name: form.get("name").cloned().unwrap_or_default(),
        repo: form.get("repo").cloned().unwrap_or_default(),
        file: form.get("file").cloned().unwrap_or_default(),
        ctx: form.get("ctx").cloned().unwrap_or_default(),
        ncpu: form.get("ncpu").cloned().unwrap_or_default(),
        mtp: form.get("mtp").map(String::as_str) == Some("on"),
    };
    edit(app, move |home| models::add(home, &new)).await
}

/// `POST /api/delmodel`: delete a preset's model file, keeping the preset.
async fn api_delmodel(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let name = parse_form(&body).get("arg").cloned().unwrap_or_default();
    edit(app, move |home| models::delete_file(home, &name)).await
}

/// `POST /api/delpreset`: remove a preset, keeping its model file.
async fn api_delpreset(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let name = parse_form(&body).get("arg").cloned().unwrap_or_default();
    edit(app, move |home| models::delete_preset(home, &name)).await
}

/// `GET /api/settings`: the keys the Settings tab edits
/// (`golem_core::settings`) with their values in golem.toml.
async fn api_settings(State(app): State<Arc<App>>) -> Response {
    let home = app.cfg.home.clone();
    match tokio::task::spawn_blocking(move || golem_core::settings::to_json(|key| crate::config::get(&home, key))).await {
        Ok(body) => json(StatusCode::OK, body),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "settings unavailable"),
    }
}

/// `POST /api/settings` with `key=<KEY>&value=<value>`: check the value
/// against the key's shape and write it to golem.toml, comments kept. Answers
/// `{"result": "saved", "applies": "..."}`, or a 400 with what the value has
/// to look like.
async fn api_set(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let form = parse_form(&body);
    let key = form.get("key").cloned().unwrap_or_default();
    let value = match golem_core::settings::check(&key, form.get("value").map(String::as_str).unwrap_or("")) {
        Ok(value) => value,
        Err(why) => return plain(StatusCode::BAD_REQUEST, why),
    };
    let applies = golem_core::settings::find(&key).map_or("now", |s| s.applies.word());
    let home = app.cfg.home.clone();
    match tokio::task::spawn_blocking(move || crate::config::set(&home, &key, &value)).await {
        Ok(Ok(())) => json(
            StatusCode::OK,
            format!("{{\"result\": \"saved\", \"applies\": {}}}", json::quote(applies)),
        ),
        Ok(Err(e)) => plain(StatusCode::INTERNAL_SERVER_ERROR, format!("cannot write golem.toml: {e}")),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "the setting did not run"),
    }
}

/// One of the preset edits, off the async runtime.
///
/// All three answer the page the same way - `{"result": "..."}`, with the
/// words `added`, `deleted`, `removed`, `bad-input`, because that is what
/// the page's JavaScript tests for. A failure to write is a
/// 500: the page cannot tell one from a refusal, but a person reading the
/// response can.
async fn edit<F>(app: Arc<App>, change: F) -> Response
where
    F: FnOnce(&std::path::Path) -> std::io::Result<&'static str> + Send + 'static,
{
    let home = app.cfg.home.clone();
    match tokio::task::spawn_blocking(move || change(&home)).await {
        Ok(Ok(result)) => json(StatusCode::OK, result_json(result)),
        Ok(Err(e)) => plain(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "the edit did not run"),
    }
}

/// The reply every console action gives: `{"result": "..."}`.
fn result_json(result: &str) -> String {
    format!("{{\"result\": {}}}", json::quote(result))
}

/// `POST /api/run` with `action=<verb>&arg=<...>`.
///
/// One job at a time: the engine is a single resource and two benchmarks at
/// once measure each other. The reply comes back immediately; the page
/// follows the job through `api/state`.
///
/// Two actions never reach a verb: `alias` and `engine` rewrite golem.toml,
/// answered in-process. Everything else goes through `runner::run` - the
/// engine verbs to the service that owns the engine, the rest as this
/// binary's own commands.
async fn api_run(State(app): State<Arc<App>>, body: Bytes) -> Response {
    let form = parse_form(&body);
    let action = form.get("action").cloned().unwrap_or_default();
    let arg = form.get("arg").cloned().unwrap_or_default();
    if runner::is_setting(&action) {
        return apply_setting(&app, &action, &arg).await;
    }
    if !runner::is_action(&action) {
        return plain(StatusCode::BAD_REQUEST, format!("unknown action: {action:?}"));
    }

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
    let app_home = app.cfg.home.clone();
    let job = app.job.clone();
    let started = action.clone();
    tokio::task::spawn_blocking(move || {
        // The slot is given back however the job ends, a panic included:
        // one left taken would answer 409 to every later action.
        struct Finish(SharedJob);
        impl Drop for Finish {
            fn drop(&mut self) {
                if let Ok(mut job) = self.0.lock() {
                    job.finish();
                }
            }
        }
        let _finish = Finish(job);
        crate::runner::run(&app_home, &started, &arg, &log)
    });

    plain(StatusCode::OK, format!("{action} started (log: logs/web-job.log)"))
}

/// `action=alias` / `action=engine`: point the setup at another preset or
/// another engine, by rewriting golem.toml.
///
/// The value is checked against what is actually there first - the same two
/// lists the page's dropdowns are filled from - so a hand-made request
/// cannot leave the configuration pointing at a model or an engine that does
/// not exist. Answers `applied` / `bad-input`, which the page reads.
async fn apply_setting(app: &Arc<App>, action: &str, arg: &str) -> Response {
    let home = app.cfg.home.clone();
    let action = action.to_string();
    let arg = arg.to_string();
    let outcome = tokio::task::spawn_blocking(move || state::apply(&home, &action, &arg)).await;
    match outcome {
        Ok(Ok(result)) => json(StatusCode::OK, result_json(result)),
        Ok(Err(e)) => plain(StatusCode::INTERNAL_SERVER_ERROR, format!("{e}")),
        Err(_) => plain(StatusCode::INTERNAL_SERVER_ERROR, "the setting did not run"),
    }
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
    fn forms_decode_plus_and_percent() {
        let form = parse_form(b"action=tune&arg=my+model%2F1");
        assert_eq!(form.get("action").map(String::as_str), Some("tune"));
        assert_eq!(form.get("arg").map(String::as_str), Some("my model/1"));
    }

    #[test]
    fn only_the_consoles_own_page_and_non_browsers_get_through() {
        let asked = |host: &str, origin: Option<&str>, site: Option<&str>| Asked {
            host: Some(host.to_string()),
            origin: origin.map(str::to_string),
            fetch_site: site.map(str::to_string),
            bind_host: "127.0.0.1",
        };
        // The page itself, and curl.
        assert!(same_origin(&asked("127.0.0.1:8787", Some("http://127.0.0.1:8787"), Some("same-origin"))));
        assert!(same_origin(&asked("localhost:8787", None, None)));
        assert!(same_origin(&asked("[::1]:8787", Some("http://[::1]:8787"), None)));
        // Another site posting through the browser.
        assert!(!same_origin(&asked("127.0.0.1:8787", Some("https://evil.example"), Some("cross-site"))));
        assert!(!same_origin(&asked("127.0.0.1:8787", None, Some("cross-site"))));
        assert!(!same_origin(&asked("127.0.0.1:8787", Some("null"), None)));
        // A rebound name, and no Host at all.
        assert!(!same_origin(&asked("evil.example:8787", Some("http://evil.example:8787"), None)));
        assert!(!same_origin(&Asked { host: None, origin: None, fetch_site: None, bind_host: "127.0.0.1" }));
        // A console bound to a LAN name answers that name.
        let lan = Asked { host: Some("golem-box:8787".into()), origin: None, fetch_site: None, bind_host: "golem-box" };
        assert!(same_origin(&lan));
    }

    #[test]
    fn a_bad_percent_sequence_is_kept_verbatim() {
        assert_eq!(url_decode("100%zz"), "100%zz");
        assert_eq!(url_decode("50%25"), "50%");
    }
}
