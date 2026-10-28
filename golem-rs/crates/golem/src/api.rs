//! The API front: golem answers the public port (`LOCAL_AI_PORT`) in front
//! of the engine, which listens on the loopback on its own (`ENGINE_PORT`).
//!
//! Every request goes to the engine as it came - the router's pages, the
//! web UI, `/health`, `/tokenize` - except `POST /v1/messages` and `POST
//! /v1/chat/completions`. On those golem adds its two read-only web tools,
//! stands in for Anthropic's `web_search_20250305` server tool, and runs the
//! tool loop on the engine's output: a call to one of golem's tools is held
//! back from the client, run here, answered to the engine, and the answer
//! carries on in the same response. The decisions are `golem_core::webtools`';
//! this file reads sockets and runs curl.
//!
//! No API is translated: an Anthropic request goes to the engine's Anthropic
//! endpoint and an OpenAI one to its OpenAI endpoint. Search goes to
//! DuckDuckGo's no-JavaScript page straight from this machine - no key, no
//! account - and a fetch is a plain GET of the page that never reaches this
//! machine or the local network.
//!
//! Plain HTTP/1.1 over a socket, a thread per connection, one request per
//! connection (`Connection: close`), like the rest of golem's HTTP: no
//! client crate, so no system proxy in the way of a loopback request.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use golem_core::html;
use golem_core::json::{self, Value};
use golem_core::webtools::{self as tools, AnthropicRelay, OpenAiRelay, Outcome, RoundEnd, Sse, SseReader, ToolCall, WebCall};

use crate::config::Overrides;
use crate::engine;

/// The longest request head a client may send.
const HEAD_MAX: usize = 64 * 1024;

/// The largest request body: a 200K-token conversation is about a
/// megabyte; this leaves room for images.
const BODY_MAX: usize = 64 * 1024 * 1024;

/// How long the engine may take to accept a connection on the loopback.
const ENGINE_CONNECT: Duration = Duration::from_millis(500);

/// How long a search may take.
const SEARCH_SECS: &str = "15";

/// How long one fetch hop may take, and the largest page fetched.
const FETCH_SECS: &str = "20";
const FETCH_MAX_BYTES: &str = "5000000";

/// How many redirects a fetch follows.
const FETCH_HOPS: usize = 5;

/// How many fetched pages are kept for their later parts.
const PAGE_CACHE: usize = 16;

/// The browser a request to DuckDuckGo and a page fetch say they are: a
/// bare curl agent is refused by more sites than it is let through.
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0 Safari/537.36";

/// Whether golem adds its web tools: `WEB_SEARCH`, on unless it says `off`.
pub fn web_tools_on(home: &Path) -> bool {
    !matches!(
        crate::config::get(home, "WEB_SEARCH").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "off" | "false" | "0" | "no"
    )
}

/// What every connection shares.
struct Front {
    home: PathBuf,
    /// Pages fetched for this service's requests, newest last, so `page=2`
    /// reads the same document as `page=1`.
    pages: Mutex<VecDeque<(String, tools::Page)>>,
}

/// Start answering the API port, for as long as this process runs. The
/// address comes back for the service's banner.
pub fn start(home: &Path, overrides: &Overrides) -> Result<String, String> {
    let host = overrides.get(home, "LOCAL_AI_HOST").unwrap_or_else(|| "127.0.0.1".to_string());
    let address = format!("{host}:{}", engine::api_port(home));
    let listener = TcpListener::bind(&address).map_err(|e| format!("cannot bind {address}: {e}"))?;
    let front = Arc::new(Front { home: home.to_path_buf(), pages: Mutex::new(VecDeque::new()) });
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let front = Arc::clone(&front);
            std::thread::spawn(move || {
                let _ = serve(&front, stream);
            });
        }
    });
    Ok(address)
}

// ---------------------------------------------------------------
// HTTP
// ---------------------------------------------------------------

/// One request off the wire.
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// Read one request: the head, a `100 Continue` when the client waits for
/// one, and the body by its length or its chunks.
fn read_request(reader: &mut BufReader<TcpStream>, client: &mut TcpStream) -> std::io::Result<Option<Request>> {
    let mut line = String::new();
    let mut head_len = 0;
    if reader.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let mut parts = line.split_whitespace();
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else { return Ok(None) };
    let (method, target) = (method.to_string(), target.to_string());
    let mut headers = Vec::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        head_len += n;
        if n == 0 || head_len > HEAD_MAX {
            return Ok(None);
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    let mut request = Request { method, target, headers, body: Vec::new() };
    if request.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        client.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
    }
    if request.header("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        request.body = read_chunked(reader, BODY_MAX)?;
    } else if let Some(length) = request.header("content-length").and_then(|v| v.parse::<usize>().ok()) {
        if length > BODY_MAX {
            return Ok(None);
        }
        let mut body = vec![0u8; length];
        reader.read_exact(&mut body)?;
        request.body = body;
    }
    Ok(Some(request))
}

/// A chunked body, de-framed.
fn read_chunked(reader: &mut impl BufRead, max: usize) -> std::io::Result<Vec<u8>> {
    let mut body = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Ok(body);
        }
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
        if size == 0 {
            // The trailers and the blank line that ends them.
            loop {
                line.clear();
                if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    return Ok(body);
                }
            }
        }
        if body.len() + size > max {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "body too large"));
        }
        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..])?;
        line.clear();
        reader.read_line(&mut line)?;
    }
}

/// An engine's answer, its head read and its body still on the socket.
struct Answer {
    status: u16,
    /// The status line's reason, for an answer handed on as it came.
    reason: String,
    headers: Vec<(String, String)>,
    body: Body,
}

impl Answer {
    /// The head as a client gets it: the engine's own, framed for a body of
    /// `length` bytes (or closed by the connection when `None`).
    fn head_for(&self, length: Option<usize>) -> String {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, self.reason);
        for (name, value) in &self.headers {
            if is_framing(name) {
                continue;
            }
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        if let Some(length) = length {
            head.push_str(&format!("Content-Length: {length}\r\n"));
        }
        head.push_str("Connection: close\r\n\r\n");
        head
    }
}

/// The body of an engine's answer, read as its framing says.
struct Body {
    reader: BufReader<TcpStream>,
    framing: Framing,
}

enum Framing {
    Length(usize),
    Chunked { left: usize, done: bool },
    Close,
}

impl Read for Body {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.framing {
            Framing::Length(left) => {
                if *left == 0 {
                    return Ok(0);
                }
                let want = buf.len().min(*left);
                let n = self.reader.read(&mut buf[..want])?;
                *left -= n;
                Ok(n)
            }
            Framing::Close => self.reader.read(buf),
            Framing::Chunked { left, done } => {
                if *done {
                    return Ok(0);
                }
                if *left == 0 {
                    let mut line = String::new();
                    if self.reader.read_line(&mut line)? == 0 {
                        *done = true;
                        return Ok(0);
                    }
                    if line.trim().is_empty() {
                        // The CRLF that ends the chunk before.
                        line.clear();
                        if self.reader.read_line(&mut line)? == 0 {
                            *done = true;
                            return Ok(0);
                        }
                    }
                    let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or("").trim(), 16).unwrap_or(0);
                    if size == 0 {
                        *done = true;
                        return Ok(0);
                    }
                    *left = size;
                }
                let want = buf.len().min(*left);
                let n = self.reader.read(&mut buf[..want])?;
                if n == 0 {
                    *done = true;
                }
                *left -= n;
                Ok(n)
            }
        }
    }
}

/// Headers that describe one connection's framing, never handed on.
fn is_framing(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "connection" | "keep-alive" | "transfer-encoding" | "content-length" | "proxy-connection" | "upgrade" | "te" | "trailer"
    )
}

/// Send a request to the engine and read the head of its answer.
fn to_engine(home: &Path, request: &Request, body: &[u8]) -> std::io::Result<Answer> {
    let addr: SocketAddr = format!("127.0.0.1:{}", engine::engine_port(home))
        .parse()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("{e}")))?;
    let mut upstream = TcpStream::connect_timeout(&addr, ENGINE_CONNECT)?;
    let mut head = format!("{} {} HTTP/1.1\r\n", request.method, request.target);
    for (name, value) in &request.headers {
        if is_framing(name) || name.eq_ignore_ascii_case("expect") {
            continue;
        }
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty() || matches!(request.method.as_str(), "POST" | "PUT" | "PATCH") {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("Connection: close\r\n\r\n");
    upstream.write_all(head.as_bytes())?;
    upstream.write_all(body)?;
    upstream.flush()?;

    let mut reader = BufReader::new(upstream);
    let mut line = String::new();
    // An interim `100 Continue` is not the answer.
    let (status, reason) = loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the engine closed the connection"));
        }
        let mut parts = line.trim_end().splitn(3, ' ');
        let _version = parts.next();
        let status: u16 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(502);
        let reason = parts.next().unwrap_or("").to_string();
        let mut headers_done = false;
        if status == 100 {
            while !headers_done {
                line.clear();
                if reader.read_line(&mut line)? == 0 || line.trim().is_empty() {
                    headers_done = true;
                }
            }
            continue;
        }
        break (status, reason);
    };
    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some((name, value)) = trimmed.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    let find = |name: &str| headers.iter().find(|(k, _): &&(String, String)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.clone());
    let framing = if find("transfer-encoding").is_some_and(|v| v.to_ascii_lowercase().contains("chunked")) {
        Framing::Chunked { left: 0, done: false }
    } else if let Some(length) = find("content-length").and_then(|v| v.parse().ok()) {
        Framing::Length(length)
    } else {
        Framing::Close
    };
    Ok(Answer { status, reason, headers, body: Body { reader, framing } })
}

/// An answer golem makes itself.
fn respond(client: &mut TcpStream, status: u16, reason: &str, content_type: &str, body: &str) -> std::io::Result<()> {
    write!(
        client,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )?;
    client.flush()
}

/// The answer when the engine is not there to ask, in the shape both APIs
/// use for an error.
fn engine_down(client: &mut TcpStream, error: &std::io::Error) -> std::io::Result<()> {
    let message = format!("golem could not reach the engine: {error} - start it with: golem up");
    let body = Value::object(vec![
        ("type", Value::str("error")),
        ("error", Value::object(vec![("type", Value::str("api_error")), ("message", Value::str(&message))])),
    ]);
    respond(client, 502, "Bad Gateway", "application/json", &body.to_json())
}

/// One connection, one request.
fn serve(front: &Front, mut client: TcpStream) -> std::io::Result<()> {
    client.set_read_timeout(Some(Duration::from_secs(60)))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let Some(request) = read_request(&mut reader, &mut client)? else { return Ok(()) };
    client.set_read_timeout(None)?;
    let path = request.target.split('?').next().unwrap_or("").to_string();
    let api = match (request.method.as_str(), path.trim_end_matches('/')) {
        ("POST", "/v1/messages") => Some(Api::Anthropic),
        ("POST", "/v1/chat/completions") => Some(Api::OpenAi),
        _ => None,
    };
    let parsed = api.and_then(|_| std::str::from_utf8(&request.body).ok()).and_then(|text| json::parse(text).ok());
    match (api, parsed) {
        (Some(api), Some(body)) => intercept(front, api, &request, body, &mut client),
        _ => pass(front, &request, &request.body, &mut client),
    }
}

/// The request to the engine and its answer back, as they come.
fn pass(front: &Front, request: &Request, body: &[u8], client: &mut TcpStream) -> std::io::Result<()> {
    let mut answer = match to_engine(&front.home, request, body) {
        Ok(answer) => answer,
        Err(e) => return engine_down(client, &e),
    };
    relay_answer(&mut answer, client)
}

/// An engine answer handed on as it came: its head, then its body as it
/// arrives, so a stream stays a stream.
fn relay_answer(answer: &mut Answer, client: &mut TcpStream) -> std::io::Result<()> {
    let length = match answer.body.framing {
        Framing::Length(n) => Some(n),
        _ => None,
    };
    client.write_all(answer.head_for(length).as_bytes())?;
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = answer.body.read(&mut buf)?;
        if n == 0 {
            break;
        }
        client.write_all(&buf[..n])?;
        client.flush()?;
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Api {
    Anthropic,
    OpenAi,
}

// ---------------------------------------------------------------
// The tool loop
// ---------------------------------------------------------------

/// A chat request with golem's tools in it.
fn intercept(front: &Front, api: Api, request: &Request, body: Value, client: &mut TcpStream) -> std::io::Result<()> {
    let on = web_tools_on(&front.home);
    let prepared = match api {
        Api::Anthropic => tools::prepare_anthropic(body, on),
        Api::OpenAi => tools::prepare_openai(body, on),
    };
    if !prepared.tools_added {
        return pass(front, request, prepared.body.to_json().as_bytes(), client);
    }
    match (api, prepared.stream) {
        (Api::Anthropic, true) => anthropic_stream(front, request, prepared, client),
        (Api::OpenAi, true) => openai_stream(front, request, prepared, client),
        (api, false) => whole(front, api, request, prepared, client),
    }
}

/// The client's stream head, once the first round has answered 200.
fn stream_head(client: &mut TcpStream) -> std::io::Result<()> {
    client.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
    )?;
    client.flush()
}

fn send(client: &mut TcpStream, events: &[Sse]) -> std::io::Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let wire: String = events.iter().map(Sse::wire).collect();
    client.write_all(wire.as_bytes())?;
    client.flush()
}

/// What the client is told when a follow-up round fails.
fn refused(status: u16, body: &str) -> String {
    let message = json::parse(body)
        .ok()
        .and_then(|v| v.get("error").and_then(|e| e.get("message")).and_then(Value::as_str).map(str::to_string))
        .unwrap_or_else(|| format!("HTTP {status}"));
    format!("Golem: the engine refused the request after the web tools ran ({message}).")
}

fn anthropic_stream(front: &Front, request: &Request, prepared: tools::Prepared, client: &mut TcpStream) -> std::io::Result<()> {
    let server = prepared.server_search.is_some();
    let mut body = prepared.body;
    let mut relay = AnthropicRelay::new();
    let mut first = true;
    loop {
        let mut answer = match to_engine(&front.home, request, body.to_json().as_bytes()) {
            Ok(answer) => answer,
            Err(e) if first => return engine_down(client, &e),
            Err(e) => {
                send(client, &relay.note(&format!("Golem: the engine stopped answering ({e}).")))?;
                return send(client, &relay.finish(server));
            }
        };
        if answer.status != 200 {
            if first {
                return relay_answer(&mut answer, client);
            }
            let mut text = String::new();
            let _ = answer.body.read_to_string(&mut text);
            send(client, &relay.note(&refused(answer.status, &text)))?;
            return send(client, &relay.finish(server));
        }
        if first {
            stream_head(client)?;
            first = false;
        }
        let mut reader = SseReader::default();
        let mut lines = BufReader::new(&mut answer.body);
        let mut line = String::new();
        loop {
            line.clear();
            if lines.read_line(&mut line)? == 0 {
                break;
            }
            if let Some(event) = reader.line(&line) {
                send(client, &relay.event(&event))?;
            }
        }
        match relay.round_end(prepared.max_calls) {
            RoundEnd::Done => return send(client, &relay.finish(server)),
            RoundEnd::Calls(calls) => {
                let ran = run_calls(front, &calls, relay.calls_made(), prepared.max_calls, prepared.server_search.as_ref());
                let (events, assistant) = relay.results(&ran, server);
                send(client, &events)?;
                tools::anthropic_continue(&mut body, assistant, &answers(&ran));
            }
        }
    }
}

fn openai_stream(front: &Front, request: &Request, prepared: tools::Prepared, client: &mut TcpStream) -> std::io::Result<()> {
    let mut body = prepared.body;
    let mut relay = OpenAiRelay::new();
    let mut first = true;
    loop {
        let mut answer = match to_engine(&front.home, request, body.to_json().as_bytes()) {
            Ok(answer) => answer,
            Err(e) if first => return engine_down(client, &e),
            Err(e) => {
                let note = relay.text_chunk(&format!("Golem: the engine stopped answering ({e})."));
                send(client, &[note])?;
                return send(client, &relay.finish());
            }
        };
        if answer.status != 200 {
            if first {
                return relay_answer(&mut answer, client);
            }
            let mut text = String::new();
            let _ = answer.body.read_to_string(&mut text);
            let note = relay.text_chunk(&refused(answer.status, &text));
            send(client, &[note])?;
            return send(client, &relay.finish());
        }
        if first {
            stream_head(client)?;
            first = false;
        }
        let mut reader = SseReader::default();
        let mut lines = BufReader::new(&mut answer.body);
        let mut line = String::new();
        loop {
            line.clear();
            if lines.read_line(&mut line)? == 0 {
                break;
            }
            if let Some(event) = reader.line(&line) {
                if event.data.trim() == "[DONE]" {
                    break;
                }
                send(client, &relay.chunk(&event))?;
            }
        }
        match relay.round_end(prepared.max_calls) {
            RoundEnd::Done => return send(client, &relay.finish()),
            RoundEnd::Calls(calls) => {
                let ran = run_calls(front, &calls, relay.calls_made(), prepared.max_calls, None);
                let (events, assistant) = relay.results(&ran);
                send(client, &events)?;
                tools::openai_continue(&mut body, assistant, &answers(&ran));
            }
        }
    }
}

/// A request the client wants answered in one piece. An answer that makes
/// no call of golem's goes back byte for byte - the engine's `timings`
/// included, which is what a measurement reads.
fn whole(front: &Front, api: Api, request: &Request, prepared: tools::Prepared, client: &mut TcpStream) -> std::io::Result<()> {
    let server = prepared.server_search.is_some();
    let mut body = prepared.body;
    let mut shown_blocks: Vec<Value> = Vec::new();
    let mut shown_text = String::new();
    let mut output_tokens = 0u64;
    let mut searches = 0u64;
    let mut calls_made = 0usize;
    let mut first = true;
    loop {
        let mut answer = match to_engine(&front.home, request, body.to_json().as_bytes()) {
            Ok(answer) => answer,
            Err(e) => return engine_down(client, &e),
        };
        let mut raw = Vec::new();
        answer.body.read_to_end(&mut raw)?;
        let value = (answer.status == 200)
            .then(|| std::str::from_utf8(&raw).ok().and_then(|t| json::parse(t).ok()))
            .flatten();
        let Some(value) = value else {
            client.write_all(answer.head_for(Some(raw.len())).as_bytes())?;
            return client.write_all(&raw);
        };
        let (calls, client_calls) = match api {
            Api::Anthropic => tools::anthropic_calls(&value),
            Api::OpenAi => tools::openai_calls(&value),
        };
        let tokens = match api {
            Api::Anthropic => tools::count(&value, &["usage", "output_tokens"]),
            Api::OpenAi => tools::count(&value, &["usage", "completion_tokens"]),
        };
        output_tokens += tokens;
        if calls.is_empty() || client_calls > 0 || calls_made >= prepared.max_calls + 2 {
            if first {
                client.write_all(answer.head_for(Some(raw.len())).as_bytes())?;
                return client.write_all(&raw);
            }
            let done = match api {
                Api::Anthropic => tools::anthropic_final(value, shown_blocks, output_tokens, searches, server),
                Api::OpenAi => tools::openai_final(value, &shown_text, output_tokens),
            };
            let text = done.to_json();
            client.write_all(answer.head_for(Some(text.len())).as_bytes())?;
            return client.write_all(text.as_bytes());
        }
        first = false;
        let ran = run_calls(front, &calls, calls_made, prepared.max_calls, prepared.server_search.as_ref());
        calls_made += ran.len();
        match api {
            Api::Anthropic => {
                let content = value.get("content").and_then(Value::as_array).cloned().unwrap_or_default();
                shown_blocks.extend(content.iter().filter(|block| {
                    !(block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && block.get("name").and_then(Value::as_str).is_some_and(tools::is_golem_tool))
                }).cloned());
                for (call, outcome) in &ran {
                    searches += u64::from(outcome.results.is_some());
                    shown_blocks.extend(tools::anthropic_result_blocks(&call.id, outcome, server));
                }
                tools::anthropic_continue(&mut body, content, &answers(&ran));
            }
            Api::OpenAi => {
                let message = value
                    .get("choices")
                    .and_then(Value::as_array)
                    .and_then(|c| c.first())
                    .and_then(|c| c.get("message"))
                    .cloned()
                    .unwrap_or(Value::Null);
                let content = message.get("content").and_then(Value::as_str).unwrap_or("");
                let reasoning = message.get("reasoning_content").and_then(Value::as_str).unwrap_or("");
                if !content.is_empty() {
                    shown_text.push_str(content);
                    shown_text.push_str("\n\n");
                }
                for (_, outcome) in &ran {
                    shown_text.push_str(&outcome.label);
                    shown_text.push_str("\n\n");
                }
                tools::openai_continue(&mut body, tools::openai_assistant(content, reasoning, &calls), &answers(&ran));
            }
        }
    }
}

/// The tool results the next request carries, by call id.
fn answers(ran: &[(ToolCall, Outcome)]) -> Vec<(String, String)> {
    ran.iter().map(|(call, outcome)| (call.id.clone(), outcome.text.clone())).collect()
}

/// Run a round's calls, the request's budget counted: a call past it is
/// answered with a note that asks the model to finish with what it has.
fn run_calls(
    front: &Front,
    calls: &[ToolCall],
    made: usize,
    max: usize,
    server: Option<&tools::ServerSearch>,
) -> Vec<(ToolCall, Outcome)> {
    calls
        .iter()
        .enumerate()
        .map(|(i, call)| {
            let outcome = if made + i >= max {
                Outcome {
                    text: format!("Golem: this request has used its {max} web tool calls. Answer with what you have."),
                    label: "🔎 Golem web tools: this answer's limit is reached".to_string(),
                    results: None,
                    query: None,
                }
            } else {
                run_call(front, call, server)
            };
            (call.clone(), outcome)
        })
        .collect()
}

/// One call, run.
fn run_call(front: &Front, call: &ToolCall, server: Option<&tools::ServerSearch>) -> Outcome {
    let outcome = match tools::parse_call(&call.name, &call.arguments) {
        Err(message) => Outcome {
            text: format!("Golem: {message}"),
            label: format!("⚠ Golem web tools: {message}"),
            results: None,
            query: None,
        },
        Ok(WebCall::Search { query }) => match search(&query) {
            Ok(results) => {
                let results = match server {
                    Some(server) => server.filter(results),
                    None => results,
                };
                Outcome {
                    text: tools::search_text(&query, Some(&golem_core::time::today()), &results),
                    label: tools::search_label(&query, results.len()),
                    results: Some(results),
                    query: Some(query),
                }
            }
            Err(message) => Outcome {
                text: tools::search_error(&query, &message),
                label: format!("🔎 Golem web search: {query} - {message}"),
                results: None,
                query: None,
            },
        },
        Ok(WebCall::Fetch { url, page }) => match page_of(front, &url) {
            Ok(fetched) => {
                let parts = fetched.parts.len().max(1);
                let part = page.clamp(1, parts);
                Outcome {
                    text: tools::fetch_text(&url, &fetched, part),
                    label: tools::fetch_label(&url, part, parts),
                    results: None,
                    query: None,
                }
            }
            Err(message) => Outcome {
                text: tools::fetch_error(&url, &message),
                label: format!("🌐 Golem web fetch: {url} - {message}"),
                results: None,
                query: None,
            },
        },
    };
    log_call(&front.home, &outcome.label);
    outcome
}

/// One line in `logs/web.log` per call, so what golem looked up on a
/// model's behalf can be read afterwards.
fn log_call(home: &Path, label: &str) {
    let path = crate::paths::web_log(home);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(file, "{} {label}", golem_core::time::now_iso());
    }
}

// ---------------------------------------------------------------
// The web
// ---------------------------------------------------------------

/// curl's output, its exit code, and what it said on stderr.
fn curl(args: &[&str]) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(match out.status.code() {
            Some(28) => "the request timed out".to_string(),
            Some(63) => "the page is too large".to_string(),
            Some(6) => "the host does not resolve".to_string(),
            Some(7) => "the host refused the connection".to_string(),
            _ if !said.is_empty() => said,
            code => format!("curl failed ({code:?})"),
        });
    }
    Ok(out.stdout)
}

/// Split curl's `-D -` output into the head and the body.
fn split_head(output: &[u8]) -> (String, Vec<u8>) {
    // One head per response curl read (a `100 Continue` is its own); the
    // body starts after the last one.
    let mut at = 0;
    let mut body_from = 0;
    while let Some(end) = find(&output[at..], b"\r\n\r\n").map(|e| at + e + 4) {
        body_from = end;
        if output[end..].starts_with(b"HTTP/") {
            at = end;
            continue;
        }
        break;
    }
    (String::from_utf8_lossy(&output[..body_from]).into_owned(), output[body_from..].to_vec())
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// DuckDuckGo's results for a query.
fn search(query: &str) -> Result<Vec<html::SearchResult>, String> {
    let form = format!("q={query}");
    let output = curl(&[
        "-sS", "-m", SEARCH_SECS, "--compressed", "-A", USER_AGENT,
        "-H", "Accept-Language: en-US,en;q=0.9",
        "-D", "-",
        "--data-urlencode", &form,
        "https://html.duckduckgo.com/html/",
    ])?;
    let (head, body) = split_head(&output);
    let status = tools::parse_head(&head).map_or(0, |(status, _)| status);
    let page = String::from_utf8_lossy(&body);
    if status != 200 || html::duckduckgo_blocked(&page) {
        return Err(format!("DuckDuckGo did not answer (HTTP {status}); try again in a minute"));
    }
    Ok(html::duckduckgo_results(&page, tools::SEARCH_RESULTS))
}

/// A page, from this service's cache or fetched.
fn page_of(front: &Front, url: &str) -> Result<tools::Page, String> {
    if let Ok(pages) = front.pages.lock() {
        if let Some((_, page)) = pages.iter().find(|(u, _)| u == url) {
            return Ok(page.clone());
        }
    }
    let page = fetch(url)?;
    if let Ok(mut pages) = front.pages.lock() {
        pages.push_back((url.to_string(), page.clone()));
        while pages.len() > PAGE_CACHE {
            pages.pop_front();
        }
    }
    Ok(page)
}

/// Fetch a page, following redirects by hand so every hop's address is
/// checked: a fetch reaches only the public web, never this machine or the
/// local network, whatever a page or a prompt points it at. The address
/// curl connects to is the one checked (`--resolve`).
fn fetch(url: &str) -> Result<tools::Page, String> {
    let mut url = url.to_string();
    for _ in 0..=FETCH_HOPS {
        let (host, port) = tools::host_port(&url).ok_or("not an http(s) URL")?;
        if tools::private_name(&host) {
            return Err(format!("{host} is on this machine or the local network; Golem only fetches public pages"));
        }
        let addr = public_address(&host, port)?;
        let pin = match addr {
            SocketAddr::V4(v4) => format!("{host}:{port}:{}", v4.ip()),
            SocketAddr::V6(v6) => format!("{host}:{port}:[{}]", v6.ip()),
        };
        let output = curl(&[
            "-sS", "-m", FETCH_SECS, "--compressed", "--max-filesize", FETCH_MAX_BYTES,
            "--proto", "=http,https", "-A", USER_AGENT,
            "-H", "Accept: text/html,application/xhtml+xml,text/plain;q=0.9,*/*;q=0.5",
            "-H", "Accept-Language: en-US,en;q=0.9",
            "--resolve", &pin,
            "-D", "-",
            &url,
        ])?;
        let (head, body) = split_head(&output);
        let (status, headers) = tools::parse_head(&head).ok_or("no answer")?;
        let header = |name: &str| headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str());
        if (300..400).contains(&status) {
            let location = header("location").ok_or_else(|| format!("HTTP {status} without a Location"))?;
            url = tools::resolve_location(&url, location);
            continue;
        }
        if !(200..300).contains(&status) {
            return Err(format!("HTTP {status}"));
        }
        let content_type = header("content-type").unwrap_or("");
        let text = tools::decode_body(&body, content_type);
        return match tools::body_kind(content_type) {
            tools::BodyKind::Html => Ok(tools::page_from_html(&text)),
            tools::BodyKind::Text => Ok(tools::page_from_text(&text)),
            tools::BodyKind::Unsupported => Err(format!("{content_type} is not a page Golem can read")),
        };
    }
    Err(format!("more than {FETCH_HOPS} redirects"))
}

/// The host's first address, refused when any address it resolves to is
/// private.
fn public_address(host: &str, port: u16) -> Result<SocketAddr, String> {
    let addrs: Vec<SocketAddr> = (host, port)
        .to_socket_addrs()
        .map_err(|_| format!("{host} does not resolve"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} does not resolve"));
    }
    if addrs.iter().any(|a| tools::private_ip(a.ip())) {
        return Err(format!("{host} is on this machine or the local network; Golem only fetches public pages"));
    }
    Ok(addrs[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curls_heads_come_off_the_body() {
        let (head, body) = split_head(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nA: b\r\n\r\n<html>\r\n\r\nx");
        assert!(head.ends_with("A: b\r\n\r\n"), "{head}");
        assert_eq!(body, b"<html>\r\n\r\nx");
        assert_eq!(tools::parse_head(&head).map(|(s, _)| s), Some(200));
    }

    #[test]
    fn a_private_address_is_never_fetched() {
        for url in ["http://127.0.0.1:8787/", "http://localhost/", "http://[::1]/", "http://192.168.1.1/admin", "http://2130706433/"] {
            let err = fetch(url).expect_err(url);
            assert!(err.contains("local network") || err.contains("resolve"), "{url}: {err}");
        }
    }

    #[test]
    fn a_chunked_body_reads_whole() {
        let mut reader = std::io::Cursor::new(b"3\r\nabc\r\n2;x\r\nde\r\n0\r\n\r\n".to_vec());
        assert_eq!(read_chunked(&mut reader, 100).expect("body"), b"abcde");
        let mut big = std::io::Cursor::new(b"ff\r\n".to_vec());
        assert!(read_chunked(&mut big, 10).is_err());
    }
}
