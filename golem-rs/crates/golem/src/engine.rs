//! Talking to the engine: is it up, and what is it serving.
//!
//! `status` has to answer that without a gateway, a proxy or an HTTP client
//! crate, so it speaks HTTP/1.1 over a socket directly. That is not only
//! fewer dependencies: it is the reason the `--noproxy '*'` that every curl
//! call in the shell needs is not needed here at all. A raw connection has
//! nowhere to pick up a system proxy from.
//!
//! Everything is short-fused. A status check that hangs is worse than one
//! that says nothing: the shell learned this the hard way (a proxy plus a
//! default timeout made every check stall for half a minute), so every read
//! and write here has a deadline of a second or two.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::path::Path;
use std::time::Duration;

/// How long any single status exchange may take.
const TIMEOUT: Duration = Duration::from_secs(2);

/// The parts of the shell's `.env` that describe where the engine is.
///
/// During the migration `.env` is still the source of truth for the shell
/// drivers, so the binary reads the same values from the same file rather
/// than asking for a second copy. When the config file arrives (the plan's
/// configuration section), this is the one place that changes.
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
    /// Read `LOCAL_AI_PORT` and `LOCAL_AI_API_KEY` from the home's `.env`.
    /// A missing file, a missing key or a malformed value falls back to the
    /// default rather than failing: `status` should say "down", not die.
    pub fn from_home(home: &Path) -> Endpoint {
        let Ok(text) = std::fs::read_to_string(home.join(".env")) else {
            return Endpoint::default();
        };
        let mut endpoint = Endpoint::default();
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim().trim_matches('"').trim_matches('\'').to_string();
            match key.trim() {
                "LOCAL_AI_PORT" => {
                    if let Ok(port) = value.parse() {
                        endpoint.port = port;
                    }
                }
                "LOCAL_AI_API_KEY" => endpoint.api_key = value,
                _ => {}
            }
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

/// The engine's pid, from the launcher's pid file (`llama-server-windows.pid`
/// on Windows, `llama-server.pid` elsewhere).
pub fn pid(home: &Path) -> Option<u32> {
    let dir = home.join(".local");
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("llama-server") && name.ends_with(".pid") {
            if let Ok(text) = std::fs::read_to_string(entry.path()) {
                if let Ok(pid) = text.trim().parse() {
                    return Some(pid);
                }
            }
        }
    }
    None
}

/// The engine as it is right now.
pub fn status(home: &Path) -> Status {
    let endpoint = Endpoint::from_home(home);
    Status {
        up: get(&endpoint, "/health").is_some_and(|(status, _)| (200..300).contains(&status)),
        pid: pid(home),
        port: endpoint.port,
    }
}

/// `GET path` against the local engine: the status code and the body.
///
/// `None` means the connection itself failed, which is the ordinary "engine
/// down" answer; an HTTP error status is still `Some`, so the caller can tell
/// "not listening" from "listening but unhappy".
pub fn get(endpoint: &Endpoint, path: &str) -> Option<(u16, String)> {
    let addr = format!("127.0.0.1:{}", endpoint.port);
    let addr: SocketAddr = addr.to_socket_addrs().ok()?.next()?;
    let mut stream = TcpStream::connect_timeout(&addr, TIMEOUT).ok()?;
    stream.set_read_timeout(Some(TIMEOUT)).ok()?;
    stream.set_write_timeout(Some(TIMEOUT)).ok()?;

    let mut request = format!(
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nConnection: close\r\n",
        endpoint.port
    );
    if !endpoint.api_key.is_empty() {
        request.push_str(&format!("Authorization: Bearer {}\r\n", endpoint.api_key));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).ok()?;

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();

    // "HTTP/1.1 200 OK" -> 200; then the body after the blank line.
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())?;
    let body = text.split_once("\r\n\r\n").map(|(_, b)| b.to_string()).unwrap_or_default();
    Some((status, body))
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
    fn reads_the_endpoint_from_env() {
        let dir = std::env::temp_dir().join("golem-rs-endpoint-test");
        std::fs::create_dir_all(&dir).expect("temp");
        std::fs::write(
            dir.join(".env"),
            "# a comment\nLOCAL_AI_PORT=\"8123\"\nLOCAL_AI_API_KEY=secret\n",
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
        std::fs::write(dir.join(".local/llama-server-windows.pid"), "4242\r\n").expect("write");
        assert_eq!(pid(&dir), Some(4242));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_pid_file_is_not_an_error() {
        let dir = std::env::temp_dir().join("golem-rs-pid-missing");
        assert_eq!(pid(&dir), None);
    }
}
