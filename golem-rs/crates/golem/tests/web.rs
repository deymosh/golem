//! Tests for the console (`src/web.rs`).
//!
//! The pure helpers are checked directly. The routes are checked end to end
//! over a real socket: the router is bound to port 0 (the OS picks the port),
//! the address is read back, and a hand-written HTTP/1.1 request is sent over
//! `TcpStream`. No HTTP client crate, and every read has a timeout so a
//! broken server fails the test instead of hanging it.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use golem::web::{inline_records, render_page, router, Config};

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
fn a_line_cut_short_is_left_out_rather_than_breaking_the_array() {
    let raw = "{\"a\": 1}\n{\"b\": 2, \"c\"\n";
    assert_eq!(inline_records(raw), "[{\"a\": 1}]");
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

#[test]
fn the_page_draws_the_same_mark_as_the_icon_and_the_screen() {
    let template = include_str!("../../../../bench/report.template.html");
    let cells: Vec<String> = golem_core::mark::GRID.iter().map(|row| format!("\"{row}\"")).collect();
    let line = format!("const GRID = [{}];", cells.join(", "));
    assert!(template.contains(&line), "the page's GRID is not golem_core::mark::GRID ({line})");
}

#[test]
fn the_page_keeps_the_data_marker_the_generator_replaces() {
    let template = include_str!("../../../../bench/report.template.html");
    assert_eq!(template.matches("/*GOLEM_DATA*/").count(), 1);
    assert!(template.contains(r#"<script id="golem-data" type="application/json">"#));
}

// ---- end to end over a real socket -------------------------------------

/// A fresh, empty directory under the system temp dir, unique per process and
/// per tag so parallel tests cannot collide, and removed when the test that
/// made it ends - pass or fail, since a failed assertion unwinds through it.
fn temp_dir(tag: &str) -> TempDir {
    let dir = std::env::temp_dir().join(format!("golem-web-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    TempDir(dir)
}

/// A test's directory, deleted on drop.
struct TempDir(PathBuf);

impl std::ops::Deref for TempDir {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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

fn fixture(tag: &str) -> (TempDir, Config) {
    let dir = temp_dir(tag);
    let results = dir.join("results.jsonl");
    let template = dir.join("report.template.html");
    std::fs::write(&results, RECORDS).expect("write results");
    std::fs::write(&template, TEMPLATE).expect("write template");
    let cfg = Config {
        results,
        template: Some(template),
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
        template: Some(dir.join("absent.html")),
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
fn another_site_cannot_post_through_the_browser() {
    let (_dir, cfg) = fixture("cross-site");
    let addr = spawn(router(cfg));
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream.set_read_timeout(Some(Duration::from_secs(5))).expect("read timeout");
    let body = "arg=anything";
    let req = format!(
        "POST /api/delpreset HTTP/1.1\r\nHost: 127.0.0.1:8787\r\nOrigin: https://evil.example\r\n\
         Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let response = String::from_utf8_lossy(&raw);
    assert!(response.starts_with("HTTP/1.1 403"), "{response}");
}

#[test]
fn state_reports_what_the_page_shows() {
    let (dir, cfg) = fixture("state");
    // Point the fixture at a port nothing can be listening on, so the test
    // says the same thing whether or not this machine is running an engine.
    std::fs::write(dir.join("golem.toml"), "LOCAL_AI_PORT=1\n").expect("write golem.toml");
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
    std::fs::write(dir.join("golem.toml"), "LOCAL_AI_PORT=1\n").expect("write golem.toml");
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

// ---- the Models tab (api/models and the three edits) -------------------

const PRESET: &str = "\
version = 1

[*]
jinja = true

; Coder - the default.
; Download: someone/model-GGUF -> model-Q4_K_M.gguf
; Quality: 79
[model-a]
model = model-Q4_K_M.gguf
ctx-size = 236544
n-cpu-moe = 36

; A tiny one for smoke tests.
; Download: tiny/tiny-GGUF -> tiny.gguf
[tiny]
ctx-size = 8192
";

/// A home with the preset file and a `golem.toml`, and no engine installed:
/// the Models tab reads and edits files, so it has to work without one.
fn models_fixture(tag: &str) -> (TempDir, Config) {
    let (dir, cfg) = fixture(tag);
    std::fs::create_dir_all(dir.join("config")).expect("config");
    std::fs::create_dir_all(dir.join("data/models")).expect("models");
    std::fs::write(dir.join("config/models.ini"), PRESET).expect("preset");
    std::fs::write(dir.join("golem.toml"), "LOCAL_AI_PORT=1\nMODEL_ALIAS=\"model-a\"\n").expect("write golem.toml");
    (dir, cfg)
}

#[test]
fn models_reports_every_preset_the_page_draws_a_row_for() {
    let (_dir, cfg) = models_fixture("models");
    let addr = spawn(router(cfg));

    let response = request(addr, "/api/models");
    assert!(response.starts_with("HTTP/1.1 200"), "{}", response.lines().next().unwrap_or(""));
    assert!(response.contains("application/json"), "{response}");
    // The field names are the page's own (`m.Present`, `m.NcMoe`, ...).
    for field in [
        "\"Name\": \"model-a\"",
        "\"File\": \"model-Q4_K_M.gguf\"",
        "\"Repo\": \"someone/model-GGUF\"",
        "\"Ctx\": 236544",
        "\"NcMoe\": 36",
        "\"Present\": false",
        "\"Default\": true",
        "\"Mtp\": false",
        "\"Moe\": false",
        "\"Experts\": \"\"",
        "\"CtxTrain\": 0",
    ] {
        assert!(response.contains(field), "{field} missing from:\n{response}");
    }
    // A section with no `model` key still knows its file and its source, so
    // the page can offer to fetch it.
    assert!(response.contains("\"Name\": \"tiny\""), "{response}");
    assert!(response.contains("\"File\": \"tiny.gguf\""), "{response}");
    assert!(response.contains("\"NcMoe\": -1"), "absent is not zero:\n{response}");
    // The shared section is not a model.
    assert!(!response.contains("\"Name\": \"*\""), "{response}");
}

#[test]
fn adding_a_model_appends_a_preset_the_page_can_then_fetch() {
    let (dir, cfg) = models_fixture("addmodel");
    let addr = spawn(router(cfg));

    let response = post(
        addr,
        "/api/addmodel",
        "name=My+Model&repo=unsloth%2FThing-GGUF&file=Thing-Q4_K_M.gguf&ctx=65536&ncpu=20&mtp=on",
    );
    assert!(response.starts_with("HTTP/1.1 200"), "{}", response.lines().next().unwrap_or(""));
    assert!(response.contains("\"result\": \"added\""), "{response}");

    // The section is in the file, with the name sanitised and the comment
    // that says where it came from.
    let text = std::fs::read_to_string(dir.join("config/models.ini")).expect("read");
    assert!(text.contains("[my-model]"), "{text}");
    assert!(text.contains("model = Thing-Q4_K_M.gguf"), "{text}");
    assert!(text.contains("; Download: unsloth/Thing-GGUF -> Thing-Q4_K_M.gguf"), "{text}");
    assert!(dir.join("config/models.ini.bak").is_file(), "the edit is reversible");
    // And the page sees it on the next poll, marked as not fetched yet.
    let listed = request(addr, "/api/models");
    assert!(listed.contains("\"Name\": \"my-model\""), "{listed}");
    assert!(listed.contains("\"Repo\": \"unsloth/Thing-GGUF\""), "{listed}");
}

#[test]
fn an_edit_that_would_break_the_setup_is_refused() {
    let (dir, cfg) = models_fixture("models-refuse");
    let addr = spawn(router(cfg));

    // The default preset cannot be removed, and neither can one that is not
    // there; the file it names is refused for the same reason.
    for body in ["arg=model-a", "arg=nope", "arg="] {
        let response = post(addr, "/api/delpreset", body);
        assert!(response.contains("\"result\": \"bad-input\""), "{body} -> {response}");
    }
    assert!(post(addr, "/api/delmodel", "arg=model-a").contains("\"result\": \"bad-input\""));
    assert!(
        std::fs::read_to_string(dir.join("config/models.ini")).expect("read") == PRESET,
        "a refusal must not touch the file"
    );
    assert!(!dir.join("config/models.ini.bak").exists(), "and must not leave a backup");

    // A model name that is not owner/repo, or a file that is not a .gguf.
    let response = post(addr, "/api/addmodel", "name=x&repo=just-a-name&file=x.gguf");
    assert!(response.contains("\"result\": \"bad-input\""), "{response}");
}

#[test]
fn removing_a_preset_and_deleting_its_file_are_two_separate_clicks() {
    let (dir, cfg) = models_fixture("models-delete");
    std::fs::write(dir.join("data/models/model-Q4_K_M.gguf"), b"weights").expect("write");
    let addr = spawn(router(cfg));

    // The default's file is refused; pointing the default elsewhere first -
    // the page's "Set default" - is what unlocks it.
    assert!(post(addr, "/api/delmodel", "arg=model-a").contains("\"result\": \"bad-input\""));
    assert!(post(addr, "/api/run", "action=alias&arg=tiny").contains("\"result\": \"applied\""));
    assert!(post(addr, "/api/delmodel", "arg=model-a").contains("\"result\": \"deleted\""));
    assert!(!dir.join("data/models/model-Q4_K_M.gguf").exists());
    assert!(dir.join("config/models.ini").is_file(), "the preset stays");

    // And the other way round: the default preset cannot be removed either,
    // so the model file goes while the section that names it remains.
    assert!(post(addr, "/api/delpreset", "arg=tiny").contains("\"result\": \"bad-input\""));
    assert!(post(addr, "/api/run", "action=alias&arg=model-a").contains("\"result\": \"applied\""));
    assert!(post(addr, "/api/delpreset", "arg=tiny").contains("\"result\": \"removed\""));
    let text = std::fs::read_to_string(dir.join("config/models.ini")).expect("read");
    assert!(!text.contains("[tiny]"), "{text}");
    assert!(!text.contains("A tiny one for smoke tests"), "its comment went too:\n{text}");
    assert!(text.contains("; Quality: 79"), "the other section's comment stays:\n{text}");
}

#[test]
fn the_alias_and_engine_settings_rewrite_golem_toml() {
    let (dir, cfg) = models_fixture("settings");
    std::fs::create_dir_all(dir.join("engines/llama.cpp")).expect("mkdir");
    std::fs::create_dir_all(dir.join("engines/ik_llama.cpp")).expect("mkdir");
    let addr = spawn(router(cfg));

    // The dropdown's Set button: the value is one of the presets the page
    // was offered, and it lands in golem.toml.
    let response = post(addr, "/api/run", "action=alias&arg=tiny");
    assert!(response.contains("\"result\": \"applied\""), "{response}");
    let env = std::fs::read_to_string(dir.join("golem.toml")).expect("read");
    assert!(env.contains("MODEL_ALIAS = \"tiny\""), "{env}");
    assert!(env.contains("LOCAL_AI_PORT=1"), "the rest of the file is untouched:\n{env}");
    // The status strip reads the new default back.
    assert!(request(addr, "/api/state").contains("\"alias\": \"tiny\""));

    let response = post(addr, "/api/run", "action=engine&arg=ik_llama.cpp");
    assert!(response.contains("\"result\": \"applied\""), "{response}");
    assert!(std::fs::read_to_string(dir.join("golem.toml")).expect("read")
        .contains("ENGINE = \"ik_llama.cpp\""));

    // Something that is not installed is refused rather than written.
    assert!(post(addr, "/api/run", "action=engine&arg=nope").contains("\"result\": \"bad-input\""));
    assert!(post(addr, "/api/run", "action=alias&arg=nope").contains("\"result\": \"bad-input\""));
    assert!(!std::fs::read_to_string(dir.join("golem.toml")).expect("read").contains("nope"));
}

// ---- a bare home --------------------------------------------------------

#[test]
fn a_bare_home_runs_actions() {
    // The engine verbs go to the service and the rest run as this binary's
    // own verbs, so a home with nothing but its files can run an action. The
    // job still has to run and release the slot, or the page would show a
    // job running forever.
    let (_dir, cfg) = fixture("bare-home");
    let addr = spawn(router(cfg));

    let response = post(addr, "/api/run", "action=report");
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected 200, got: {}",
        response.lines().next().unwrap_or("")
    );
    assert!(response.contains("report started"), "{response}");

    // api/run answers before the job it started has run - that is the point
    // of it - so the slot is the only way to see that the job finished.
    for _ in 0..200 {
        if request(addr, "/api/state").contains("\"job_running\": false") {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the job never ran or never released the slot");
}
