//! The API front against a scripted engine: what passes through untouched,
//! and the tool loop in both APIs, streamed and not.
//!
//! The loop's call is a fetch of a loopback address, which golem refuses
//! before anything goes on the network - so the whole round trip runs
//! offline and the refusal is what the engine is handed back.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use golem::config::Overrides;

/// A fake engine: answers each connection with the next scripted reply and
/// keeps every request body it was sent.
struct Engine {
    port: u16,
    bodies: Arc<Mutex<Vec<String>>>,
}

fn engine(replies: Vec<String>) -> Engine {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let bodies = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&bodies);
    std::thread::spawn(move || {
        for (stream, reply) in listener.incoming().zip(replies) {
            let Ok(mut stream) = stream else { break };
            let mut reader = BufReader::new(stream.try_clone().expect("clone"));
            let mut length = 0;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line.trim().is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; length];
            let _ = reader.read_exact(&mut body);
            seen.lock().expect("lock").push(String::from_utf8_lossy(&body).into_owned());
            let _ = stream.write_all(reply.as_bytes());
        }
    });
    Engine { port, bodies }
}

/// A free port: bound, read, released.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("addr").port()
}

/// A home whose front is on a free port and whose engine is `engine`.
fn front(tag: &str, engine_port: u16) -> (PathBuf, u16) {
    let dir = std::env::temp_dir().join(format!("golem-rs-api-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp");
    let port = free_port();
    std::fs::write(dir.join("golem.toml"), format!("LOCAL_AI_PORT = \"{port}\"\nENGINE_PORT = \"{engine_port}\"\n"))
        .expect("golem.toml");
    golem::api::start(&dir, &Overrides::default()).expect("the front binds");
    (dir, port)
}

/// One request to the front: the raw answer.
fn ask(port: u16, method: &str, path: &str, body: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    write!(
        stream,
        "{method} {path} HTTP/1.1\r\nHost: x\r\nx-api-key: k\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .expect("write");
    let mut answer = String::new();
    stream.read_to_string(&mut answer).expect("read");
    answer
}

fn json_reply(body: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len())
}

fn sse_reply(events: &str) -> String {
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{events}")
}

#[test]
fn other_paths_pass_through_untouched() {
    let engine = engine(vec!["HTTP/1.1 200 OK\r\nContent-Length: 15\r\n\r\n{\"status\":\"ok\"}".to_string()]);
    let (dir, port) = front("pass", engine.port);
    let answer = ask(port, "GET", "/health", "");
    assert!(answer.starts_with("HTTP/1.1 200 OK\r\n"), "{answer}");
    assert!(answer.ends_with("\r\n\r\n{\"status\":\"ok\"}"), "{answer}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_answer_without_golems_calls_comes_back_byte_for_byte() {
    // What eval and bench read: the engine's own `timings`, unchanged.
    let reply = r#"{"choices":[{"message":{"role":"assistant","content":"4"},"finish_reason":"stop"}],"timings":{"predicted_per_second":62.8}}"#;
    let engine = engine(vec![json_reply(reply)]);
    let (dir, port) = front("plain", engine.port);
    let answer = ask(port, "POST", "/v1/chat/completions", r#"{"model":"m","messages":[{"role":"user","content":"2+2"}],"temperature":0.70}"#);
    assert!(answer.ends_with(&format!("\r\n\r\n{reply}")), "{answer}");
    let sent = engine.bodies.lock().unwrap()[0].clone();
    assert!(sent.contains("\"temperature\":0.70"), "numbers go on as written: {sent}");
    assert!(sent.contains("golem_web_search") && sent.contains("golem_web_fetch"), "{sent}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn a_down_engine_is_said_in_the_apis_error_shape() {
    let (dir, port) = front("down", free_port());
    let answer = ask(port, "POST", "/v1/messages", r#"{"model":"m","messages":[]}"#);
    assert!(answer.starts_with("HTTP/1.1 502"), "{answer}");
    assert!(answer.contains("\"type\":\"error\"") && answer.contains("golem up"), "{answer}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_openai_answer_runs_golems_call_and_carries_on() {
    let call = r#"{"choices":[{"message":{"role":"assistant","content":null,"tool_calls":[{"id":"call_1","type":"function","function":{"name":"golem_web_fetch","arguments":"{\"url\":\"http://127.0.0.1:9/\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
    let done = r#"{"choices":[{"message":{"role":"assistant","content":"Cannot read it."},"finish_reason":"stop"}],"usage":{"prompt_tokens":50,"completion_tokens":4,"total_tokens":54}}"#;
    let engine = engine(vec![json_reply(call), json_reply(done)]);
    let (dir, port) = front("openai-loop", engine.port);
    let answer = ask(port, "POST", "/v1/chat/completions", r#"{"model":"m","messages":[{"role":"user","content":"read it"}]}"#);
    let body = answer.split("\r\n\r\n").nth(1).expect("a body");
    assert!(body.contains("🌐 Golem web fetch: http://127.0.0.1:9/ - "), "{body}");
    assert!(body.contains("Cannot read it."), "{body}");
    assert!(body.contains("\"completion_tokens\":9"), "{body}");
    assert!(!body.contains("tool_calls"), "golem's call is not the client's: {body}");
    let second = engine.bodies.lock().unwrap()[1].clone();
    assert!(second.contains("\"role\":\"tool\"") && second.contains("\"tool_call_id\":\"call_1\""), "{second}");
    assert!(second.contains("local network"), "the refusal is the tool's answer: {second}");
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn an_anthropic_stream_runs_golems_call_inside_one_message() {
    let round_one = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n\
        event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"golem_web_fetch\",\"input\":{}}}\n\n\
        event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"url\\\":\\\"http://localhost/\\\"}\"}}\n\n\
        event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
        event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":6}}\n\n\
        event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let round_two = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"usage\":{\"input_tokens\":80}}}\n\n\
        event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n\
        event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"That page is private.\"}}\n\n\
        event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n\
        event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\",\"stop_sequence\":null},\"usage\":{\"output_tokens\":5}}\n\n\
        event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let engine = engine(vec![sse_reply(round_one), sse_reply(round_two)]);
    let (dir, port) = front("anthropic-stream", engine.port);
    let answer = ask(port, "POST", "/v1/messages", r#"{"model":"m","stream":true,"max_tokens":64,"messages":[{"role":"user","content":"read localhost"}]}"#);
    assert!(answer.contains("Content-Type: text/event-stream"), "{answer}");
    assert_eq!(answer.matches("event: message_start").count(), 1, "{answer}");
    assert_eq!(answer.matches("event: message_stop").count(), 1, "{answer}");
    assert!(!answer.contains("golem_web_fetch"), "the call itself stays hidden: {answer}");
    assert!(answer.contains("🌐 Golem web fetch: http://localhost/ - "), "{answer}");
    assert!(answer.contains("\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"That page is private.\"}"), "{answer}");
    assert!(answer.contains("\"stop_reason\":\"end_turn\"") && answer.contains("\"output_tokens\":11"), "{answer}");
    let second = engine.bodies.lock().unwrap()[1].clone();
    assert!(second.contains("\"type\":\"tool_result\",\"tool_use_id\":\"toolu_1\""), "{second}");
    assert!(second.contains("\"stream\":true"), "{second}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The real web: a search, then a fetch of a page, as the model is handed
/// them. Ignored by default - it needs the network and DuckDuckGo.
#[test]
#[ignore]
fn live_search_and_fetch() {
    let call = |id: &str, name: &str, args: &str| {
        json_reply(&format!(
            r#"{{"choices":[{{"message":{{"role":"assistant","content":null,"tool_calls":[{{"id":"{id}","type":"function","function":{{"name":"{name}","arguments":{}}}}}]}},"finish_reason":"tool_calls"}}]}}"#,
            golem_core::json::quote(args)
        ))
    };
    let engine = engine(vec![
        call("c1", "golem_web_search", r#"{"query":"llama.cpp server documentation"}"#),
        call("c2", "golem_web_fetch", r#"{"url":"https://docs.python.org/3/library/json.html","page":2}"#),
        json_reply(r#"{"choices":[{"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]}"#),
    ]);
    let (dir, port) = front("live", engine.port);
    let answer = ask(port, "POST", "/v1/chat/completions", r#"{"model":"m","messages":[{"role":"user","content":"x"}]}"#);
    println!("{answer}");
    let bodies = engine.bodies.lock().unwrap().clone();
    for body in &bodies[1..] {
        let value = golem_core::json::parse(body).expect("json");
        let messages = value.get("messages").and_then(|m| m.as_array()).expect("messages");
        println!("----\n{}", messages.last().and_then(|m| m.get("content")).and_then(|c| c.as_str()).unwrap_or(""));
    }
    assert!(bodies[1].contains("Golem web search: \\\"llama.cpp server documentation\\\" (DuckDuckGo, "), "{}", bodies[1]);
    assert!(bodies[2].contains("--- end (part 2 of 2) ---"), "{}", bodies[2]);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn web_search_off_leaves_the_tools_out() {
    let reply = r#"{"content":[{"type":"text","text":"hi"}]}"#;
    let engine = engine(vec![json_reply(reply)]);
    let (dir, port) = front("off", engine.port);
    std::fs::write(
        dir.join("golem.toml"),
        format!("LOCAL_AI_PORT = \"{port}\"\nENGINE_PORT = \"{}\"\nWEB_SEARCH = \"off\"\n", engine.port),
    )
    .expect("golem.toml");
    let answer = ask(port, "POST", "/v1/messages", r#"{"model":"m","messages":[],"tools":[{"type":"web_search_20250305","name":"web_search"}]}"#);
    assert!(answer.ends_with(reply), "{answer}");
    let sent = engine.bodies.lock().unwrap()[0].clone();
    assert!(!sent.contains("golem_web") && !sent.contains("web_search_2025"), "{sent}");
    std::fs::remove_dir_all(&dir).ok();
}
