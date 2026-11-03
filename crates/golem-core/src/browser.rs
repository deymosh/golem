//! A page that builds its content with JavaScript, rendered in the system's
//! own Chromium-family browser: where each OS installs one, the flags a
//! render runs with, and the requests the loopback proxy in front of it
//! reads. golem finds the file, runs the browser and answers the sockets.
//!
//! The browser reaches the web only through golem's proxy
//! (`--proxy-bypass-list=<-loopback>` sends even the loopback there, QUIC is
//! off, WebRTC keeps to proxied connections), so the address check a fetch
//! makes applies to every request the page makes.

use crate::webtools::host_port;

/// The longest a render waits for the page's scripts, in milliseconds of the
/// browser's virtual time: measured pages finished within it.
pub const BUDGET_MS: u64 = 10_000;

/// Where a Chromium-family browser is installed on `os` (`std::env::consts::OS`),
/// best first: Chrome, Chromium and Brave, then Edge - every Windows box has
/// Edge, but it renders slower and starts its updater whatever the flags say.
/// `env` reads the environment: the install roots on Windows, `PATH` on Linux,
/// `HOME` on macOS. The caller takes the first that is a file.
pub fn candidates(os: &str, env: impl Fn(&str) -> Option<String>) -> Vec<String> {
    match os {
        "windows" => {
            let roots: Vec<String> = ["LOCALAPPDATA", "ProgramFiles", "ProgramFiles(x86)"]
                .iter()
                .filter_map(|key| env(key))
                .filter(|root| !root.is_empty())
                .collect();
            let under = |tails: &[&str]| -> Vec<String> {
                tails
                    .iter()
                    .flat_map(|tail| roots.iter().map(move |root| format!("{}\\{tail}", root.trim_end_matches('\\'))))
                    .collect()
            };
            let mut out = under(&[
                "Google\\Chrome\\Application\\chrome.exe",
                "Chromium\\Application\\chrome.exe",
                "BraveSoftware\\Brave-Browser\\Application\\brave.exe",
            ]);
            out.extend(under(&["Microsoft\\Edge\\Application\\msedge.exe"]));
            out
        }
        "macos" => {
            let apps = [
                "Google Chrome.app/Contents/MacOS/Google Chrome",
                "Chromium.app/Contents/MacOS/Chromium",
                "Brave Browser.app/Contents/MacOS/Brave Browser",
                "Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
            ];
            let mut roots = vec!["/Applications".to_string()];
            if let Some(home) = env("HOME").filter(|h| !h.is_empty()) {
                roots.push(format!("{}/Applications", home.trim_end_matches('/')));
            }
            apps.iter().flat_map(|app| roots.iter().map(move |root| format!("{root}/{app}"))).collect()
        }
        _ => {
            let names = [
                "google-chrome-stable",
                "google-chrome",
                "chromium",
                "chromium-browser",
                "brave-browser",
                "microsoft-edge-stable",
                "microsoft-edge",
            ];
            let path = env("PATH").unwrap_or_default();
            let dirs: Vec<&str> = path.split(':').filter(|d| !d.is_empty()).collect();
            names
                .iter()
                .flat_map(|name| dirs.iter().map(move |dir| format!("{}/{name}", dir.trim_end_matches('/'))))
                .collect()
        }
    }
}

/// The browser's name for the log, from its file: `chrome`, `brave`, `msedge`.
pub fn name(path: &str) -> String {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let file = file.strip_suffix(".exe").unwrap_or(file);
    file.to_ascii_lowercase()
}

/// How long a render waits for scripts within `timeout_secs` (`WEB_TIMEOUT`):
/// [`BUDGET_MS`], or half the timeout when that is shorter, leaving the rest
/// to start the browser and load the page.
pub fn budget_ms(timeout_secs: u64) -> u64 {
    (timeout_secs * 1000 / 2).min(BUDGET_MS)
}

/// The arguments for one render: headless, a fresh profile at `profile`,
/// every request through the proxy on `proxy_port`, the rendered DOM on
/// stdout, in English as the fetch asks for it. Nothing that reports home:
/// background networking, sync, component updates, pings, crash reports and
/// metrics uploads off.
pub fn args(profile: &str, proxy_port: u16, budget_ms: u64, url: &str) -> Vec<String> {
    let mut out: Vec<String> = [
        "--headless=new",
        "--disable-gpu",
        "--no-first-run",
        "--no-default-browser-check",
        "--disable-background-networking",
        "--disable-component-update",
        "--disable-sync",
        "--disable-default-apps",
        "--disable-extensions",
        "--disable-breakpad",
        "--disable-crash-reporter",
        "--metrics-recording-only",
        "--no-pings",
        "--disable-domain-reliability",
        "--disable-features=Translate,OptimizationHints,MediaRouter",
        "--mute-audio",
        "--lang=en-US",
        "--disable-quic",
        "--proxy-bypass-list=<-loopback>",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    out.push(format!("--proxy-server=http://127.0.0.1:{proxy_port}"));
    out.push(format!("--user-data-dir={profile}"));
    out.push(format!("--virtual-time-budget={budget_ms}"));
    out.push("--dump-dom".to_string());
    out.push(url.to_string());
    out
}

/// What the browser asked golem's proxy for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProxyAsk {
    /// `CONNECT host:port`: a tunnel, with TLS inside it.
    Tunnel { host: String, port: u16 },
    /// A plain-HTTP request in absolute form, its head rewritten for the
    /// site: the path alone on the request line, the proxy's own headers
    /// dropped, and `Connection: close` - one request per connection, so a
    /// connection the browser reuses cannot reach a second host unchecked.
    Forward { host: String, port: u16, head: String },
}

/// A request head the browser sent the proxy (through its blank line), read.
pub fn proxy_ask(head: &str) -> Result<ProxyAsk, String> {
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut words = first.split(' ');
    let (method, target, version) = match (words.next(), words.next(), words.next(), words.next()) {
        (Some(m), Some(t), Some(v), None) if !m.is_empty() && v.starts_with("HTTP/") => (m, t, v),
        _ => return Err(format!("not a request line: {first:?}")),
    };
    if method == "CONNECT" {
        // An authority alone, always with its port.
        if target.contains(['/', '?', '#', '@']) || !target.contains(':') {
            return Err(format!("not a host and port: {target:?}"));
        }
        let (host, port) = host_port(&format!("https://{target}")).ok_or_else(|| format!("not a host and port: {target:?}"))?;
        return Ok(ProxyAsk::Tunnel { host, port });
    }
    if !target.get(..7).is_some_and(|s| s.eq_ignore_ascii_case("http://")) {
        return Err(format!("not an http:// address: {target:?}"));
    }
    let (host, port) = host_port(target).ok_or_else(|| format!("not an http:// address: {target:?}"))?;
    let rest = &target[7..];
    let path = match rest.find(['/', '?']) {
        Some(at) if rest[at..].starts_with('?') => format!("/{}", &rest[at..]),
        Some(at) => rest[at..].to_string(),
        None => "/".to_string(),
    };
    let path = path.split('#').next().unwrap_or("/").to_string();
    let mut out = format!("{method} {path} {version}\r\n");
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let name = line.split(':').next().unwrap_or("").trim().to_ascii_lowercase();
        if matches!(name.as_str(), "connection" | "proxy-connection" | "proxy-authorization" | "keep-alive") {
            continue;
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str("Connection: close\r\n\r\n");
    Ok(ProxyAsk::Forward { host, port, head: out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_looks_in_each_install_root_chrome_first_edge_last() {
        let env = |key: &str| match key {
            "LOCALAPPDATA" => Some("C:\\Users\\u\\AppData\\Local".to_string()),
            "ProgramFiles" => Some("C:\\Program Files".to_string()),
            "ProgramFiles(x86)" => Some("C:\\Program Files (x86)\\".to_string()),
            _ => None,
        };
        let found = candidates("windows", env);
        assert_eq!(found[0], "C:\\Users\\u\\AppData\\Local\\Google\\Chrome\\Application\\chrome.exe");
        assert_eq!(found[1], "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe");
        assert!(found.contains(&"C:\\Program Files\\BraveSoftware\\Brave-Browser\\Application\\brave.exe".to_string()));
        assert_eq!(found.last().map(String::as_str), Some("C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe"));
        let edge = found.iter().position(|p| p.ends_with("msedge.exe")).expect("edge");
        assert!(found[..edge].iter().all(|p| !p.ends_with("msedge.exe")));
        assert!(found[edge..].iter().all(|p| p.ends_with("msedge.exe")));
    }

    #[test]
    fn linux_looks_on_the_path_and_macos_in_applications() {
        let found = candidates("linux", |k| (k == "PATH").then(|| "/usr/bin:/snap/bin/".to_string()));
        assert_eq!(found[0], "/usr/bin/google-chrome-stable");
        assert!(found.contains(&"/snap/bin/chromium".to_string()));
        assert_eq!(found.last().map(String::as_str), Some("/snap/bin/microsoft-edge"));
        assert!(candidates("linux", |_| None).is_empty());
        let mac = candidates("macos", |k| (k == "HOME").then(|| "/Users/u".to_string()));
        assert_eq!(mac[0], "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
        assert_eq!(mac[1], "/Users/u/Applications/Google Chrome.app/Contents/MacOS/Google Chrome");
    }

    #[test]
    fn a_browser_is_named_by_its_file() {
        assert_eq!(name("C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe"), "chrome");
        assert_eq!(name("/usr/bin/brave-browser"), "brave-browser");
        assert_eq!(name("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"), "microsoft edge");
    }

    #[test]
    fn the_script_budget_fits_inside_the_timeout() {
        assert_eq!(budget_ms(20), 10_000);
        assert_eq!(budget_ms(120), 10_000);
        assert_eq!(budget_ms(3), 1_500);
    }

    #[test]
    fn every_request_goes_through_the_proxy_and_the_page_comes_last() {
        let args = args("C:\\h\\.local\\browser\\1", 50123, 10_000, "https://example.com/");
        for flag in [
            "--headless=new",
            "--proxy-server=http://127.0.0.1:50123",
            "--proxy-bypass-list=<-loopback>",
            "--disable-quic",
            "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
            "--user-data-dir=C:\\h\\.local\\browser\\1",
            "--virtual-time-budget=10000",
            "--dump-dom",
            "--disable-background-networking",
        ] {
            assert!(args.iter().any(|a| a == flag), "{flag} missing from {args:?}");
        }
        assert_eq!(args.last().map(String::as_str), Some("https://example.com/"));
    }

    #[test]
    fn a_connect_is_a_tunnel_to_its_host_and_port() {
        assert_eq!(
            proxy_ask("CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n"),
            Ok(ProxyAsk::Tunnel { host: "example.com".into(), port: 443 })
        );
        assert_eq!(
            proxy_ask("CONNECT [2606:2800::1]:8443 HTTP/1.1\r\n\r\n"),
            Ok(ProxyAsk::Tunnel { host: "2606:2800::1".into(), port: 8443 })
        );
        assert!(proxy_ask("CONNECT example.com HTTP/1.1\r\n\r\n").is_err());
        assert!(proxy_ask("CONNECT a@b.com:443 HTTP/1.1\r\n\r\n").is_err());
    }

    #[test]
    fn a_plain_request_is_rewritten_for_the_site_one_per_connection() {
        let head = "GET http://Example.com:8080/a/b?c=1#x HTTP/1.1\r\nHost: example.com:8080\r\n\
                    Proxy-Connection: keep-alive\r\nConnection: keep-alive\r\nAccept: */*\r\n\r\n";
        assert_eq!(
            proxy_ask(head),
            Ok(ProxyAsk::Forward {
                host: "example.com".into(),
                port: 8080,
                head: "GET /a/b?c=1 HTTP/1.1\r\nHost: example.com:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n".into(),
            })
        );
        match proxy_ask("GET http://example.com?q HTTP/1.1\r\n\r\n") {
            Ok(ProxyAsk::Forward { head, port, .. }) => {
                assert_eq!(port, 80);
                assert!(head.starts_with("GET /?q HTTP/1.1\r\n"), "{head}");
            }
            other => panic!("{other:?}"),
        }
        assert!(proxy_ask("GET /relative HTTP/1.1\r\n\r\n").is_err());
        assert!(proxy_ask("GET ftp://example.com/ HTTP/1.1\r\n\r\n").is_err());
        assert!(proxy_ask("garbage\r\n\r\n").is_err());
    }
}
