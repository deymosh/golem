//! A fetched page that builds its content with JavaScript, rendered in this
//! machine's own Chromium-family browser: headless, on a throwaway profile,
//! its DOM read back once the scripts have run (`--dump-dom`), the process
//! tree killed at `WEB_TIMEOUT`.
//!
//! Every request the page makes goes through a loopback proxy golem answers
//! for that render: it resolves the host the browser asks for, refuses this
//! machine and the local network as a fetch does, and connects to the
//! address it checked. The decisions are `golem_core::browser`'s; this file
//! finds the browser, runs it, and moves the proxy's bytes.

use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use golem_core::browser::{self, ProxyAsk};
use golem_core::webtools as tools;

/// The largest rendered DOM read back.
const DOM_MAX: usize = 16 * 1024 * 1024;

/// The longest request head the proxy reads.
const HEAD_MAX: usize = 64 * 1024;

/// One render at a time: each browser is about half a gigabyte.
static RENDERING: Mutex<()> = Mutex::new(());

/// Profiles made by this process, so two renders never share one.
static PROFILES: AtomicU64 = AtomicU64::new(0);

/// Whether a fetch may render a page: `WEB_BROWSER`, on unless it says `off`.
pub fn enabled(home: &Path) -> bool {
    !matches!(
        crate::config::get(home, "WEB_BROWSER").unwrap_or_default().trim().to_ascii_lowercase().as_str(),
        "off" | "false" | "0" | "no"
    )
}

/// The browser a render runs: the first of `golem_core::browser::candidates`
/// installed here.
pub fn find() -> Option<PathBuf> {
    browser::candidates(std::env::consts::OS, |key| std::env::var(key).ok())
        .into_iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
}

/// A page rendered.
#[derive(Debug)]
pub struct Rendered {
    /// The DOM after the page's scripts ran.
    pub dom: String,
    /// The browser that rendered it, by its file's name.
    pub browser: String,
    /// The requests the page made that the proxy refused.
    pub refused: u64,
}

/// `url` rendered in this machine's browser within `timeout`, or why not.
pub fn render(home: &Path, url: &str, timeout: Duration) -> Result<Rendered, String> {
    let exe = find().ok_or("no Chrome, Chromium, Brave or Edge on this machine")?;
    let name = browser::name(&exe.to_string_lossy());
    let _one = RENDERING.lock().unwrap_or_else(|e| e.into_inner());
    let proxy = Proxy::start(timeout).map_err(|e| format!("cannot start the browser's proxy: {e}"))?;
    let profile = crate::paths::browser_profiles(home)
        .join(format!("{}-{}", std::process::id(), PROFILES.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&profile).map_err(|e| format!("cannot make a browser profile: {e}"))?;
    let mut command = Command::new(&exe);
    command.args(browser::args(
        &profile.to_string_lossy(),
        proxy.port,
        browser::budget_ms(timeout.as_secs()),
        url,
    ));
    let ran = crate::proc::output_within(command, timeout, DOM_MAX);
    proxy.stop();
    let refused = proxy.refused.load(Ordering::Relaxed);
    remove_profile(&profile);
    let refusals = match refused {
        0 => String::new(),
        1 => " (1 request refused)".to_string(),
        n => format!(" ({n} requests refused)"),
    };
    match ran {
        Err(e) => Err(format!("cannot run {name}: {e}")),
        Ok(None) => Err(format!("{name} did not finish within {} s{refusals}", timeout.as_secs())),
        Ok(Some(dom)) if dom.iter().all(u8::is_ascii_whitespace) => Err(format!("{name} gave no page{refusals}")),
        Ok(Some(dom)) => Ok(Rendered { dom: String::from_utf8_lossy(&dom).into_owned(), browser: name, refused }),
    }
}

/// Remove a render's profile. The browser's last files can stay locked a
/// moment after it exits on Windows, so this tries a few times.
fn remove_profile(profile: &Path) {
    for _ in 0..10 {
        if std::fs::remove_dir_all(profile).is_ok() || !profile.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
}

/// The loopback proxy one render's browser goes through.
struct Proxy {
    port: u16,
    stop: Arc<AtomicBool>,
    refused: Arc<AtomicU64>,
}

impl Proxy {
    /// Listen on a free loopback port and answer each connection on its own
    /// thread until [`Proxy::stop`].
    fn start(timeout: Duration) -> std::io::Result<Proxy> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let stop = Arc::new(AtomicBool::new(false));
        let refused = Arc::new(AtomicU64::new(0));
        let (stopped, count) = (Arc::clone(&stop), Arc::clone(&refused));
        std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((client, _)) => {
                        let count = Arc::clone(&count);
                        std::thread::spawn(move || {
                            let _ = client.set_nonblocking(false);
                            let _ = serve(client, timeout, &count);
                        });
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
        });
        Ok(Proxy { port, stop, refused })
    }

    /// Stop taking connections; the open ones end with the browser.
    fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One connection from the browser: its request read, the host checked, and
/// the bytes moved both ways to the address checked.
fn serve(client: TcpStream, timeout: Duration, refused: &AtomicU64) -> std::io::Result<()> {
    client.set_read_timeout(Some(timeout))?;
    let mut reader = BufReader::new(client.try_clone()?);
    let mut to_client = client;
    let head = read_head(&mut reader)?;
    let ask = match browser::proxy_ask(&head) {
        Ok(ask) => ask,
        Err(_) => return answer(&mut to_client, "400 Bad Request"),
    };
    let (host, port) = match &ask {
        ProxyAsk::Tunnel { host, port } | ProxyAsk::Forward { host, port, .. } => (host.clone(), *port),
    };
    if tools::private_name(&host) {
        refused.fetch_add(1, Ordering::Relaxed);
        return answer(&mut to_client, "403 Forbidden");
    }
    let addr = match crate::api::public_address(&host, port) {
        Ok(addr) => addr,
        Err(message) if message.contains("does not resolve") => return answer(&mut to_client, "502 Bad Gateway"),
        Err(_) => {
            refused.fetch_add(1, Ordering::Relaxed);
            return answer(&mut to_client, "403 Forbidden");
        }
    };
    let mut upstream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(stream) => stream,
        Err(_) => return answer(&mut to_client, "502 Bad Gateway"),
    };
    upstream.set_read_timeout(Some(timeout))?;
    match &ask {
        ProxyAsk::Tunnel { .. } => to_client.write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")?,
        ProxyAsk::Forward { head, .. } => upstream.write_all(head.as_bytes())?,
    }
    // The browser's side first drains what the reader already holds - a
    // request body - then whatever it sends after.
    let mut up = upstream.try_clone()?;
    let outward = std::thread::spawn(move || {
        let _ = std::io::copy(&mut reader, &mut up);
        let _ = up.shutdown(Shutdown::Write);
    });
    let _ = std::io::copy(&mut upstream, &mut to_client);
    let _ = to_client.shutdown(Shutdown::Both);
    let _ = upstream.shutdown(Shutdown::Both);
    let _ = outward.join();
    Ok(())
}

/// A request head through its blank line.
fn read_head(reader: &mut impl BufRead) -> std::io::Result<String> {
    let mut head = Vec::new();
    loop {
        let before = head.len();
        let mut line = std::io::Read::take(&mut *reader, (HEAD_MAX - before) as u64);
        if line.read_until(b'\n', &mut head)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the request ended in its head"));
        }
        if head[before..] == *b"\r\n" || head[before..] == *b"\n" {
            return Ok(String::from_utf8_lossy(&head).into_owned());
        }
        if head.len() >= HEAD_MAX {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "the request head is too long"));
        }
    }
}

/// An answer from the proxy itself, the connection closed after it.
fn answer(client: &mut TcpStream, status: &str) -> std::io::Result<()> {
    client.write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes())?;
    let _ = client.shutdown(Shutdown::Both);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn ask(port: u16, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream.set_read_timeout(Some(Duration::from_secs(10))).expect("timeout");
        stream.write_all(request.as_bytes()).expect("write");
        let mut out = String::new();
        let _ = stream.read_to_string(&mut out);
        out
    }

    #[test]
    fn the_proxy_refuses_this_machine_and_the_local_network() {
        let proxy = Proxy::start(Duration::from_secs(5)).expect("proxy");
        for request in [
            "CONNECT 127.0.0.1:8787 HTTP/1.1\r\n\r\n",
            "CONNECT localhost:443 HTTP/1.1\r\n\r\n",
            "CONNECT [::1]:443 HTTP/1.1\r\n\r\n",
            "GET http://192.168.1.1/admin HTTP/1.1\r\nHost: 192.168.1.1\r\n\r\n",
            "GET http://2130706433/ HTTP/1.1\r\n\r\n",
        ] {
            let said = ask(proxy.port, request);
            assert!(said.starts_with("HTTP/1.1 403"), "{request:?}: {said:?}");
        }
        assert_eq!(proxy.refused.load(Ordering::Relaxed), 5);
        assert!(ask(proxy.port, "GET /relative HTTP/1.1\r\n\r\n").starts_with("HTTP/1.1 400"));
    }

    #[test]
    fn a_head_is_read_through_its_blank_line() {
        let mut reader = std::io::Cursor::new(b"CONNECT a:1 HTTP/1.1\r\nHost: a\r\n\r\nrest".to_vec());
        assert_eq!(read_head(&mut reader).expect("head"), "CONNECT a:1 HTTP/1.1\r\nHost: a\r\n\r\n");
        let mut cut = std::io::Cursor::new(b"GET / HTTP/1.1\r\n".to_vec());
        assert!(read_head(&mut cut).is_err());
    }
}
