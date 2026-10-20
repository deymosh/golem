//! Tests for the console (`src/web.rs`).
//!
//! `golem` is a binary crate with no library target, so the module is pulled
//! in by path rather than imported; that keeps `Cargo.toml` and `main.rs`
//! untouched while still exercising the real source.
//!
//! The pure helpers are checked directly. The routes are checked end to end
//! over a real socket: the router is bound to port 0 (the OS picks the port),
//! the address is read back, and a hand-written HTTP/1.1 request is sent over
//! `TcpStream`. No HTTP client crate, and every read has a timeout so a
//! broken server fails the test instead of hanging it.

// The console's module and the ones it uses. `golem` is a bin-only crate, so
// the sources are pulled in by path rather than imported; that keeps
// `main.rs` and `Cargo.toml` out of the test's way while still exercising the
// real code. (Moving the crate to bin+lib would make this unnecessary; until
// then every module the console touches has to be named here.)
#[path = "../src/driver.rs"]
mod driver;
#[path = "../src/engine.rs"]
mod engine;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/web.rs"]
mod web;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use web::{inline_records, render_page, router, Config};

// ---- pure helpers ------------------------------------------------------

#[test]
fn inlines_records_the_way_the_page_expects() {
    // Blank lines and non-record lines are dropped; each kept line is
    // trimmed; `</` is escaped so a record cannot close the <script> block.
    let raw = "{\"a\": 1}\n\n  {\"b\": \"x</script>\"}\nnot json\n";
    let data = inline_records(raw);
    assert_eq!(data, "[{\"a\": 1},\n{\"b\": \"x<\\/script>\"}]");
}

#[test]
fn an_empty_file_inlines_as_an_empty_array() {
    assert_eq!(inline_records(""), "[]");
}

#[test]
fn a_file_with_no_record_lines_inlines_as_an_empty_array() {
    assert_eq!(inline_records("\n   \n# comment\n"), "[]");
}

#[test]
fn render_page_replaces_only_the_data_marker() {
    let template = "<script id=\"golem-data\">/*GOLEM_DATA*/</script>";
    assert_eq!(
        render_page(template, "[{\"a\": 1}]"),
        "<script id=\"golem-data\">[{\"a\": 1}]</script>"
    );
    // No marker, nothing to do.
    assert_eq!(render_page("<p>hi</p>", "[]"), "<p>hi</p>");
}

// ---- end to end over a real socket -------------------------------------

/// A fresh, empty directory under the system temp dir, unique per process and
/// per tag so parallel tests cannot collide.
fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("golem-web-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

/// Bind the router to an ephemeral port on a background thread and hand back
/// the address the OS chose.
fn spawn(app: axum::Router) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    let addr = listener.local_addr().expect("read bound address");
    listener.set_nonblocking(true).expect("nonblocking");
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).expect("tokio listener");
            axum::serve(listener, app).await.expect("serve");
        });
    });
    addr
}

/// One request, read to EOF (`Connection: close`), with timeouts on both ends
/// so a silent server cannot hang the suite.
fn request(addr: SocketAddr, path: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .expect("write timeout");
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("send request");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).expect("read response");
    String::from_utf8_lossy(&buf).into_owned()
}

const RECORDS: &str = "{\"model\": \"a\", \"note\": \"x</script>\"}\n\n{\"model\": \"b\"}\n";
const TEMPLATE: &str = "<html><script id=\"golem-data\">/*GOLEM_DATA*/</script></html>";

fn fixture(tag: &str) -> (PathBuf, Config) {
    let dir = temp_dir(tag);
    let results = dir.join("results.jsonl");
    let template = dir.join("report.template.html");
    std::fs::write(&results, RECORDS).expect("write results");
    std::fs::write(&template, TEMPLATE).expect("write template");
    let cfg = Config {
        results,
        template,
        host: "127.0.0.1".to_string(),
        port: 0,
        home: dir.clone(),
    };
    (dir, cfg)
}

#[test]
fn the_root_serves_the_page_with_the_records_inlined() {
    let (_dir, cfg) = fixture("page");
    let addr = spawn(router(cfg));

    let response = request(addr, "/");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200, got: {}",
        response.lines().next().unwrap_or("")
    );
    assert!(
        response.contains("text/html; charset=utf-8"),
        "the page should be served as html"
    );
    // The record lines are inlined, and `</` is escaped in the page too.
    assert!(
        response.contains(r#"{"model": "a", "note": "x<\/script>"}"#),
        "the first record should be inlined and escaped: {response}"
    );
    assert!(
        response.contains(r#"{"model": "b"}"#),
        "the second record should be inlined"
    );
    assert!(
        !response.contains("/*GOLEM_DATA*/"),
        "the marker should have been replaced"
    );
}

#[test]
fn results_jsonl_serves_the_raw_bytes() {
    let (_dir, cfg) = fixture("jsonl");
    let addr = spawn(router(cfg));

    let response = request(addr, "/results.jsonl");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200, got: {}",
        response.lines().next().unwrap_or("")
    );
    assert!(
        response.contains("text/plain; charset=utf-8"),
        "the raw file should be plain text"
    );
    // The file's own bytes, not the escaped/inlined form.
    assert!(
        response.contains("x</script>"),
        "the raw line should be served verbatim: {response}"
    );
}

#[test]
fn an_unknown_path_is_a_404() {
    let (_dir, cfg) = fixture("404");
    let addr = spawn(router(cfg));

    let response = request(addr, "/nope");
    assert!(
        response.starts_with("HTTP/1.1 404"),
        "expected 404, got: {}",
        response.lines().next().unwrap_or("")
    );
}

#[test]
fn a_missing_file_is_a_500_not_a_panic() {
    let dir = temp_dir("missing");
    let cfg = Config {
        results: dir.join("absent.jsonl"),
        template: dir.join("absent.html"),
        host: "127.0.0.1".to_string(),
        port: 0,
        home: dir.clone(),
    };
    let addr = spawn(router(cfg));

    let response = request(addr, "/");
    assert!(
        response.starts_with("HTTP/1.1 500"),
        "expected 500, got: {}",
        response.lines().next().unwrap_or("")
    );
    assert!(
        response.contains("cannot read"),
        "the error should say what could not be read: {response}"
    );
}

#[test]
fn a_results_file_that_is_not_there_yet_serves_an_empty_page() {
    // A fresh machine has measured nothing, and the console is where the
    // first result will appear: it must come up rather than refuse.
    let (_dir, mut cfg) = fixture("no-results");
    cfg.results = cfg.results.with_file_name("not-written-yet.jsonl");
    let addr = spawn(router(cfg));

    let page = request(addr, "/");
    assert!(
        page.starts_with("HTTP/1.1 200"),
        "the page should still be served: {}",
        page.lines().next().unwrap_or("")
    );
    assert!(
        page.contains("[]"),
        "with no records the page gets an empty dataset: {page}"
    );

    let raw = request(addr, "/results.jsonl");
    assert!(
        raw.starts_with("HTTP/1.1 200"),
        "the polling endpoint answers too: {}",
        raw.lines().next().unwrap_or("")
    );
}

// ---- the controls (api/state, api/run) --------------------------------

/// A POST with an `application/x-www-form-urlencoded` body.
fn post(addr: SocketAddr, path: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
    stream.set_write_timeout(Some(Duration::from_secs(5))).expect("write timeout");
    let req = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    String::from_utf8_lossy(&raw).into_owned()
}

#[test]
fn state_reports_what_the_page_shows() {
    let (dir, cfg) = fixture("state");
    // Point the fixture at a port nothing can be listening on, so the test
    // says the same thing whether or not this machine is running an engine.
    std::fs::write(dir.join(".env"), "LOCAL_AI_PORT=1\n").expect("write env");
    let addr = spawn(router(cfg));

    let response = request(addr, "/api/state");
    assert!(response.starts_with("HTTP/1.1 200"), "{}", response.lines().next().unwrap_or(""));
    assert!(response.contains("application/json"), "the state is json");
    // With no engine running there is none to report, but the shape has to
    // be the page's: an "up" flag, the alias and engine names, the presets.
    assert!(response.contains("\"up\": false"), "{response}");
    assert!(response.contains("\"alias\":"), "{response}");
    assert!(response.contains("\"engine\":"), "{response}");
    assert!(response.contains("\"presets\": ["), "{response}");
    assert!(response.contains("\"job_running\": false"), "{response}");
    // `engines` fills the engine dropdown and `job_tail` the job pane; the
    // page reads both, so a state without them is a broken console even
    // though every other field is right.
    assert!(response.contains("\"engines\": ["), "{response}");
    assert!(response.contains("\"job_tail\": "), "{response}");
}

#[test]
fn state_lists_the_installed_engines_and_the_last_job_log() {
    let (dir, cfg) = fixture("state-engines");
    std::fs::write(dir.join(".env"), "LOCAL_AI_PORT=1\n").expect("write env");
    // Two adapters and a stray file: only the directories are engines.
    std::fs::create_dir_all(dir.join("engines/llama.cpp")).expect("mkdir");
    std::fs::create_dir_all(dir.join("engines/ik_llama.cpp")).expect("mkdir");
    std::fs::write(dir.join("engines/README.md"), "not an engine").expect("write");
    std::fs::create_dir_all(dir.join("logs")).expect("logs");
    std::fs::write(dir.join("logs/web-job.log"), "line one\nline two\n").expect("write");
    let addr = spawn(router(cfg));

    let response = request(addr, "/api/state");
    assert!(
        response.contains("\"engines\": [\"ik_llama.cpp\", \"llama.cpp\"]"),
        "the dropdown lists the directories, sorted:\n{response}"
    );
    assert!(!response.contains("README.md"), "a file is not an engine:\n{response}");
    assert!(
        response.contains("\"job_tail\": \"line one\\nline two\""),
        "the job pane has the log, newlines escaped for JSON:\n{response}"
    );
}

#[test]
fn an_unknown_action_is_refused() {
    let (_dir, cfg) = fixture("bad-action");
    let addr = spawn(router(cfg));

    let response = post(addr, "/api/run", "action=rm%20-rf");
    assert!(
        response.starts_with("HTTP/1.1 400"),
        "an unknown action must be refused, got: {}",
        response.lines().next().unwrap_or("")
    );
}

#[test]
fn the_controls_say_so_when_this_folder_has_no_launcher() {
    // A release has no shell scripts yet: rather than pretending, the
    // control reports that it cannot run anything here.
    let (_dir, cfg) = fixture("no-launcher");
    let addr = spawn(router(cfg));

    let response = post(addr, "/api/run", "action=status");
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "expected 503, got: {}",
        response.lines().next().unwrap_or("")
    );
    assert!(response.contains("no launcher"), "{response}");
}
