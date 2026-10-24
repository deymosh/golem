//! Talking to the engine: is it up, and what is it serving.
//!
//! `status` has to answer that without a gateway, a proxy or an HTTP client
//! crate, so it speaks HTTP/1.1 over a socket directly. That is not only
//! fewer dependencies: a raw connection has nowhere to pick up a system
//! proxy from, so a proxy configured for the desktop cannot intercept a
//! loopback request.
//!
//! Everything is short-fused. A status check that hangs is worse than one
//! that says nothing (a proxy plus a default timeout once made every check
//! stall for half a minute), so every read and write here has a deadline of
//! a second or two.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

/// How long any single status exchange may take.
const TIMEOUT: Duration = Duration::from_secs(2);

/// How long a *connect* to the local engine may take, separately from the
/// exchange itself.
///
/// Loopback either has something listening or has nothing, and a refusal is
/// immediate - except on a box where the SYN to a closed port is dropped
/// instead of refused, and then this is the whole wait. Measured on the
/// reference machine: 2.07s to a closed port 8000, which made every `status`
/// take two seconds and the service's start-up poll pay it again per attempt.
/// Nothing about the answer needs that long, so the connect does not wait for
/// it; a slow *answer* is the read timeout's business.
const CONNECT_TIMEOUT: Duration = Duration::from_millis(500);

/// Where the local engine listens, and the key it wants.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub port: u16,
    pub api_key: String,
}

impl Default for Endpoint {
    fn default() -> Self {
        Endpoint { port: 8000, api_key: String::new() }
    }
}

impl Endpoint {
    /// `LOCAL_AI_PORT` and `LOCAL_AI_API_KEY` from `golem.toml`. A missing
    /// key or a malformed value falls back to the default rather than
    /// failing: `status` should say "down", not die.
    pub fn from_home(home: &Path) -> Endpoint {
        let mut endpoint = Endpoint::default();
        if let Some(port) = golem_core::config::value(home, "LOCAL_AI_PORT")
            .and_then(|value| value.parse().ok())
        {
            endpoint.port = port;
        }
        if let Some(key) = golem_core::config::value(home, "LOCAL_AI_API_KEY") {
            endpoint.api_key = key;
        }
        endpoint
    }
}

/// What `status` found.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub up: bool,
    /// The process id from the home's pid file, when there is one. A pid
    /// file outliving its process is normal enough (a crash), so it is
    /// reported as "recorded", not as "running".
    pub pid: Option<u32>,
    pub port: u16,
}

/// The engine's pid, from the pid file `up` writes.
pub fn pid(home: &Path) -> Option<u32> {
    std::fs::read_to_string(crate::paths::pid_file(home)).ok()?.trim().parse().ok()
}

/// The engine as it is right now.
pub fn status(home: &Path) -> Status {
    let endpoint = Endpoint::from_home(home);
    Status { up: healthy(&endpoint), pid: pid(home), port: endpoint.port }
}

/// Whether the engine answers `/health` with a success. A loading
/// llama-server answers 503, which is not healthy: it is the reason to wait.
pub fn healthy(endpoint: &Endpoint) -> bool {
    get(endpoint, "/health").is_some_and(|(status, _)| (200..300).contains(&status))
}

/// `GET path` against the local engine: the status code and the body.
///
/// `None` means the connection itself failed, which is the ordinary "engine
/// down" answer; an HTTP error status is still `Some`, so the caller can tell
/// "not listening" from "listening but unhappy".
pub fn get(endpoint: &Endpoint, path: &str) -> Option<(u16, String)> {
    let base = Base {
        host: "127.0.0.1".to_string(),
        port: endpoint.port,
        prefix: String::new(),
        api_key: endpoint.api_key.clone(),
    };
    request("GET", &base, path, None, TIMEOUT).map(|reply| (reply.status, reply.body))
}

/// The model ids the engine lists (`GET /v1/models`), for a status line that
/// says what is being served rather than just that something is.
pub fn models(endpoint: &Endpoint) -> Vec<String> {
    let Some((status, body)) = get(endpoint, "/v1/models") else { return Vec::new() };
    if status != 200 {
        return Vec::new();
    }
    let Ok(value) = golem_core::json::parse(&body) else { return Vec::new() };
    match value.get("data") {
        Some(golem_core::json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("id").and_then(|id| id.as_str()).map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

// ---------------------------------------------------------------
// The measurement client
// ---------------------------------------------------------------
//
// `get` above is the *status* client: a second of patience, loopback only,
// and a body small enough not to care how it arrives. A measurement is the
// opposite on every count - a 200K-token prompt takes minutes, and
// `LOCAL_AI_BASE_URL` may name another host entirely - so the two things
// that differ are parameters here rather than assumptions.
//
// It is still HTTP/1.1 over a socket, and still no client crate, so still
// no system proxy. What differs is that the body can be a megabyte and the
// answer can arrive in chunks, so both are handled explicitly.

/// Where the API is: loopback, or whatever `LOCAL_AI_BASE_URL` names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub host: String,
    pub port: u16,
    /// The path in front of `/v1/...`, taken from the base URL itself.
    pub prefix: String,
    pub api_key: String,
}

impl Base {
    /// `LOCAL_AI_BASE_URL` when set, else `http://<LOCAL_AI_HOST>:<PORT>`.
    ///
    /// Each part of the rule matters: the base URL wins
    /// over host and port; `0.0.0.0` is a bind address and not a destination,
    /// so it becomes the loopback; and a trailing `/v1` is stripped, because
    /// every path built from this adds its own.
    ///
    /// A base URL that is not `http://` is refused rather than half-honoured:
    /// this client speaks HTTP over a socket and has no TLS, so an `https://`
    /// base would connect and then fail in a way that reads like a broken
    /// server. `None` says so, and the caller says it out loud.
    pub fn from_home(home: &Path) -> Option<Base> {
        let endpoint = Endpoint::from_home(home);
        let (port, api_key) = (endpoint.port, endpoint.api_key);
        let configured = golem_core::config::value(home, "LOCAL_AI_BASE_URL")
            .filter(|value| !value.is_empty());

        let Some(url) = configured else {
            return Some(Base { host: "127.0.0.1".to_string(), port, prefix: String::new(), api_key });
        };
        let rest = url.strip_prefix("http://")?;
        let (authority, path) = match rest.split_once('/') {
            Some((authority, path)) => (authority, format!("/{path}")),
            None => (rest, String::new()),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host.to_string(), port.parse().ok()?),
            None => (authority.to_string(), 80),
        };
        let prefix = path.trim_end_matches('/').trim_end_matches("/v1").trim_end_matches('/');
        let host = if host == "0.0.0.0" { "127.0.0.1".to_string() } else { host };
        Some(Base { host, port, prefix: prefix.to_string(), api_key })
    }

    /// The path a request goes to, with the base URL's own prefix in front.
    pub fn path(&self, path: &str) -> String {
        format!("{}{path}", self.prefix)
    }
}

/// One HTTP exchange: the status line's code and the body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub status: u16,
    pub body: String,
}

impl Reply {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// `POST` a JSON body and read the answer, with `timeout` as the whole
/// budget for the exchange.
///
/// `None` means the connection failed or the answer could not be read, which
/// for a measurement is the same thing: there is no number.
pub fn post_json(base: &Base, path: &str, body: &str, timeout: Duration) -> Option<Reply> {
    request("POST", base, path, Some(body), timeout)
}

/// `GET` and read the answer.
pub fn get_json(base: &Base, path: &str, timeout: Duration) -> Option<Reply> {
    request("GET", base, path, None, timeout)
}

fn request(
    method: &str,
    base: &Base,
    path: &str,
    body: Option<&str>,
    timeout: Duration,
) -> Option<Reply> {
    let addr = format!("{}:{}", base.host, base.port);
    let addr: SocketAddr = addr.to_socket_addrs().ok()?.next()?;
    // Connecting is a different budget from answering: a host that is not
    // there should say so in seconds, not in half an hour - and loopback
    // either has a listener or has none, so it gets the short fuse.
    let connect = if base.host == "127.0.0.1" { CONNECT_TIMEOUT } else { Duration::from_secs(10) };
    let connect = timeout.min(connect);
    let mut stream = TcpStream::connect_timeout(&addr, connect).ok()?;
    // The read timeout is the whole budget on purpose. llama.cpp answers a
    // non-streaming request in one go at the end, so minutes of silence in
    // the middle are normal and must not be mistaken for a dead server.
    stream.set_read_timeout(Some(timeout)).ok()?;
    stream.set_write_timeout(Some(timeout)).ok()?;

    let target = base.path(path);
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n",
        base.host, base.port
    );
    if !base.api_key.is_empty() {
        head.push_str(&format!("Authorization: Bearer {}\r\n", base.api_key));
    }
    if let Some(body) = body {
        head.push_str("Content-Type: application/json\r\n");
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");

    stream.write_all(head.as_bytes()).ok()?;
    if let Some(body) = body {
        stream.write_all(body.as_bytes()).ok()?;
    }
    stream.flush().ok()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    parse_reply(&String::from_utf8_lossy(&raw))
}

/// Split an HTTP/1.1 response into its status code and its body.
///
/// The body is de-framed: a `Content-Length` truncates it (a connection that
/// closes early leaves the rest missing, and one that lingers leaves extra
/// bytes), and `Transfer-Encoding: chunked` is unwrapped. Both matter - a
/// chunked body handed on as-is is not JSON, and the failure reads as "the
/// server sent nonsense" rather than "the reader did not unwrap it".
pub fn parse_reply(text: &str) -> Option<Reply> {
    let (head, body) = match text.split_once("\r\n\r\n") {
        Some(parts) => parts,
        // A server that writes bare newlines; curl tolerates it and so must
        // this, because the alternative is calling a good answer unreadable.
        None => text.split_once("\n\n")?,
    };
    let status: u16 = head.split_whitespace().nth(1)?.parse().ok()?;
    let lowered = head.to_ascii_lowercase();

    if lowered.contains("transfer-encoding: chunked") {
        return Some(Reply { status, body: dechunk(body) });
    }
    if let Some(length) = header_value(&lowered, "content-length").and_then(|v| v.parse::<usize>().ok())
    {
        let bytes = body.as_bytes();
        let end = length.min(bytes.len());
        return Some(Reply { status, body: String::from_utf8_lossy(&bytes[..end]).into_owned() });
    }
    Some(Reply { status, body: body.to_string() })
}

/// The value of one header, from the lower-cased head block.
fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == name).then(|| value.trim().to_string())
    })
}

/// Unwrap `Transfer-Encoding: chunked`.
///
/// A chunk is `<hex length>[;ext]\r\n<that many bytes>\r\n`, and a zero
/// length ends the body. A truncated stream stops where it stops rather than
/// panicking - a partial answer is still worth handing to the parser.
fn dechunk(body: &str) -> String {
    let bytes = body.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let Some(line_end) = find(bytes, at, b"\r\n") else { break };
        let size_text = String::from_utf8_lossy(&bytes[at..line_end]);
        let size_text = size_text.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_text, 16) else { break };
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        let end = (start + size).min(bytes.len());
        out.extend_from_slice(&bytes[start..end]);
        // Past the chunk's own trailing CRLF.
        at = end + 2;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first occurrence of `needle` in `haystack` at or after `from`.
fn find(haystack: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    (from..=haystack.len() - needle.len()).find(|&i| &haystack[i..i + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    /// A one-shot HTTP server on an ephemeral port, so `get` can be tested
    /// for real without depending on anything running.
    fn serve_once(reply: &'static str) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        port
    }

    #[test]
    fn reads_the_endpoint_from_the_config() {
        let dir = std::env::temp_dir().join("golem-rs-endpoint-test");
        std::fs::create_dir_all(&dir).expect("temp");
        std::fs::write(
            dir.join("golem.toml"),
            "# a comment\nLOCAL_AI_PORT=\"8123\"\nLOCAL_AI_API_KEY=\"secret\"\n",
        )
        .expect("write");
        let endpoint = Endpoint::from_home(&dir);
        assert_eq!(endpoint.port, 8123);
        assert_eq!(endpoint.api_key, "secret");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_env_falls_back_to_the_defaults() {
        let dir = std::env::temp_dir().join("golem-rs-endpoint-missing");
        let endpoint = Endpoint::from_home(&dir);
        assert_eq!(endpoint, Endpoint::default());
    }

    #[test]
    fn reads_a_healthy_answer() {
        let port = serve_once("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}");
        let endpoint = Endpoint { port, api_key: String::new() };
        let (status, body) = get(&endpoint, "/health").expect("an answer");
        assert_eq!(status, 200);
        assert_eq!(body, "{}");
    }

    #[test]
    fn an_error_status_is_still_an_answer() {
        // Listening but unhappy is different from not listening at all.
        let port = serve_once("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n");
        let endpoint = Endpoint { port, api_key: String::new() };
        let (status, _) = get(&endpoint, "/health").expect("an answer");
        assert_eq!(status, 401);
    }

    #[test]
    fn nothing_listening_is_none() {
        // Port 1 on loopback: nothing listens there, and nothing hangs.
        let endpoint = Endpoint { port: 1, api_key: String::new() };
        assert!(get(&endpoint, "/health").is_none());
    }

    #[test]
    fn lists_the_models_the_engine_advertises() {
        let port = serve_once(
            "HTTP/1.1 200 OK\r\n\r\n{\"data\":[{\"id\":\"a\"},{\"id\":\"b\"}]}",
        );
        let endpoint = Endpoint { port, api_key: String::new() };
        assert_eq!(models(&endpoint), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_down_engine_lists_no_models() {
        let endpoint = Endpoint { port: 1, api_key: String::new() };
        assert!(models(&endpoint).is_empty());
    }

    #[test]
    fn reads_the_pid_the_launcher_wrote() {
        let dir = std::env::temp_dir().join("golem-rs-pid-test");
        std::fs::create_dir_all(dir.join(".local")).expect("temp");
        std::fs::write(dir.join(".local/llama-server.pid"), "4242\r\n").expect("write");
        assert_eq!(pid(&dir), Some(4242));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_pid_file_is_not_an_error() {
        let dir = std::env::temp_dir().join("golem-rs-pid-missing");
        assert_eq!(pid(&dir), None);
    }

    #[test]
    fn the_endpoint_reads_the_toml_file_too() {
        // `status` must see the port and the key wherever the configuration
        // puts them, through `config` like every other reader.
        let dir = base_home("endpoint-toml");
        std::fs::write(dir.join("golem.toml"), "LOCAL_AI_PORT=\"8000\"\nLOCAL_AI_API_KEY=\"from-env\"\n")
            .expect("write");
        std::fs::write(dir.join("golem.toml"), "LOCAL_AI_PORT = 8123\nLOCAL_AI_API_KEY = \"k\"\n")
            .expect("write");
        let endpoint = Endpoint::from_home(&dir);
        assert_eq!(endpoint.port, 8123);
        assert_eq!(endpoint.api_key, "k");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_reply_is_split_into_its_status_and_its_body() {
        let reply = parse_reply("HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n{\"a\":1}").expect("reply");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.body, "{\"a\":1}");
        assert!(reply.ok());

        let fail = parse_reply("HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n").expect("reply");
        assert_eq!(fail.status, 401);
        assert!(!fail.ok(), "a 401 is not an ok reply");
    }

    #[test]
    fn a_content_length_truncates_a_body_the_server_overran() {
        // Extra bytes after the length are not part of the body; a JSON
        // parser handed them would reject the whole answer.
        let reply = parse_reply("HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}garbage").expect("reply");
        assert_eq!(reply.body, "{}");
    }

    #[test]
    fn a_chunked_body_is_unwrapped() {
        // What a server that does not know its length sends.
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\
                   7\r\n{\"a\":1}\r\n\
                   2\r\n{}\r\n\
                   0\r\n\r\n";
        let reply = parse_reply(raw).expect("reply");
        assert_eq!(reply.body, "{\"a\":1}{}");
    }

    #[test]
    fn a_truncated_chunk_stream_stops_rather_than_panicking() {
        let raw = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{\"a\"";
        let reply = parse_reply(raw).expect("reply");
        assert_eq!(reply.body, "{\"a\"");
    }

    #[test]
    fn a_reply_with_no_header_break_is_not_a_reply() {
        assert_eq!(parse_reply("not an http response"), None);
    }

    #[test]
    fn the_base_url_wins_over_host_and_port() {
        let dir = base_home("base-url");
        std::fs::write(
            dir.join("golem.toml"),
            "LOCAL_AI_PORT=\"8000\"\nLOCAL_AI_API_KEY=\"k\"\n\
             LOCAL_AI_BASE_URL=\"http://192.168.1.5:9000/v1\"\n",
        )
        .expect("write");
        let base = Base::from_home(&dir).expect("base");
        assert_eq!(base.host, "192.168.1.5");
        assert_eq!(base.port, 9000);
        // The trailing `/v1` is stripped: every path built from this adds its
        // own, and `//v1/v1/chat/completions` is a 404 that reads like a bug.
        assert_eq!(base.path("/v1/chat/completions"), "/v1/chat/completions");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn without_a_base_url_the_endpoint_is_the_loopback() {
        let dir = base_home("base-default");
        std::fs::write(dir.join("golem.toml"), "LOCAL_AI_HOST=\"0.0.0.0\"\nLOCAL_AI_PORT=\"8123\"\n")
            .expect("write");
        let base = Base::from_home(&dir).expect("base");
        assert_eq!(base.host, "127.0.0.1");
        assert_eq!(base.port, 8123);
        assert_eq!(base.path("/health"), "/health");
        // No settings at all is the ordinary case on a fresh machine.
        let fresh = base_home("base-none");
        let base = Base::from_home(&fresh).expect("base");
        assert_eq!(base.host, "127.0.0.1");
        assert_eq!(base.port, 8000);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&fresh).ok();
    }

    #[test]
    fn an_https_base_url_is_refused_rather_than_half_honoured() {
        // This client speaks HTTP over a socket and has no TLS. Connecting and
        // then failing would read as a broken server.
        let dir = base_home("base-https");
        std::fs::write(dir.join("golem.toml"), "LOCAL_AI_BASE_URL=\"https://example.test/v1\"\n")
            .expect("write");
        assert_eq!(Base::from_home(&dir), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    fn base_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-base-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        dir
    }
}
