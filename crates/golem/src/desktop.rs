//! What a bare `golem` opens: the console in a browser when it was started
//! from a desktop (a double-click), the full-screen screen in a terminal,
//! and the usage when there is neither.
//!
//! The decision is [`front_end`], pure over the [`Launch`] facts so every
//! case is tested; reading those facts is [`Launch::read`].

use std::io::IsTerminal;
use std::path::Path;
use std::process::{Command, Stdio};

use crate::{config, machine, service};

/// The console's port when `CONSOLE_PORT` sets none.
pub const CONSOLE_PORT: u16 = 8787;

/// The port the service hosts the console on: `CONSOLE_PORT`, else 8787.
pub fn console_port(home: &Path) -> u16 {
    config::get(home, "CONSOLE_PORT").and_then(|p| p.trim().parse().ok()).unwrap_or(CONSOLE_PORT)
}

/// The address the service binds the console on: `CONSOLE_HOST`, else
/// `127.0.0.1`.
pub fn console_host(home: &Path) -> String {
    config::get(home, "CONSOLE_HOST")
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// Whether `host` keeps the console on this machine.
pub fn is_loopback(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// The console's address in this machine's browser. A console bound to
/// every interface (`0.0.0.0`, `::`) is reached through loopback; one bound
/// to a single address, through that address.
pub fn console_url(home: &Path) -> String {
    let host = console_host(home);
    let reach = match host.as_str() {
        "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        h if h.contains(':') => format!("[{h}]"),
        h => h.to_string(),
    };
    format!("http://{reach}:{}/", console_port(home))
}

/// What is known about how this process was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Launch {
    /// Started by the desktop's file manager: Explorer as the parent on
    /// Windows; on Linux a graphical session with no terminal attached.
    pub from_desktop: bool,
    /// A terminal to draw the screen in.
    pub terminal: bool,
}

/// The three answers to a bare `golem`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrontEnd {
    /// The console, in the default browser, through the service.
    Console,
    /// The full-screen screen in this terminal.
    Screen,
    /// No screen to draw on: the usage text.
    Usage,
}

/// A double-click opens the console; a terminal gets the screen; anything
/// else - piped, scripted, a container's log - gets the usage.
pub fn front_end(launch: Launch) -> FrontEnd {
    if launch.from_desktop {
        FrontEnd::Console
    } else if launch.terminal {
        FrontEnd::Screen
    } else {
        FrontEnd::Usage
    }
}

impl Launch {
    pub fn read() -> Launch {
        let terminal = std::io::stdout().is_terminal();
        Launch { from_desktop: from_desktop(), terminal }
    }
}

/// Whether the desktop's file manager started this process.
///
/// Windows: the parent is `explorer` (a double-click opens a console window
/// for us, so the terminal test alone cannot tell). Linux: a graphical
/// session, not over SSH, and no terminal on stdin - a file manager runs a
/// program without one unless asked to.
fn from_desktop() -> bool {
    if cfg!(windows) {
        let script = format!(
            "(Get-Process -Id (Get-CimInstance Win32_Process -Filter 'ProcessId={}').ParentProcessId).ProcessName",
            std::process::id()
        );
        machine::powershell(&script).is_some_and(|name| name.trim().eq_ignore_ascii_case("explorer"))
    } else if cfg!(target_os = "linux") {
        graphical_session() && !std::io::stdin().is_terminal()
    } else {
        false
    }
}

/// Whether this Linux process runs in a graphical session someone sits at:
/// a display to draw on, and not reached over SSH.
pub fn graphical_session() -> bool {
    let display = ["DISPLAY", "WAYLAND_DISPLAY"].iter().any(|k| std::env::var_os(k).is_some());
    display && std::env::var_os("SSH_CONNECTION").is_none()
}

/// Start the service if it is not running - it hosts the console - and open
/// the console in the default browser.
pub fn open_console(home: &Path, out: &mut dyn std::io::Write) -> Result<String, String> {
    service::ensure_running(home, out)?;
    let url = console_url(home);
    open(&url);
    Ok(url)
}

/// Open a file or an address with the desktop's own handler, best effort: a
/// machine with no opener - a server, a bare container - must not turn a
/// written report or a running console into a failed command.
pub fn open(target: impl AsRef<std::ffi::OsStr>) {
    let mut command = if cfg!(windows) {
        // `start` is a cmd.exe builtin and its first argument is the window
        // title: without the empty one, a quoted path is taken as the title
        // and nothing opens.
        let mut command = Command::new("cmd");
        command.args(["/c", "start", ""]).arg(target);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(target);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(target);
        command
    };
    let _ = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_double_click_opens_the_console_a_terminal_the_screen() {
        let launch = |from_desktop, terminal| front_end(Launch { from_desktop, terminal });
        // A double-click on Windows comes with a console window, so it is a
        // terminal too - the desktop wins.
        assert_eq!(launch(true, true), FrontEnd::Console);
        assert_eq!(launch(true, false), FrontEnd::Console);
        // Typed in a terminal, on a desktop or a server alike.
        assert_eq!(launch(false, true), FrontEnd::Screen);
        // Piped, scripted, or a container's log.
        assert_eq!(launch(false, false), FrontEnd::Usage);
    }

    #[test]
    fn the_console_port_is_configurable() {
        let dir = std::env::temp_dir().join("golem-rs-desktop-port");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        assert_eq!(console_port(&dir), 8787);
        std::fs::write(dir.join("golem.toml"), "CONSOLE_PORT = \"9100\"\n").expect("write");
        assert_eq!(console_url(&dir), "http://127.0.0.1:9100/");
        // Listening everywhere is still opened through loopback here.
        std::fs::write(dir.join("golem.toml"), "CONSOLE_HOST = \"0.0.0.0\"\n").expect("write");
        assert_eq!(console_host(&dir), "0.0.0.0");
        assert_eq!(console_url(&dir), "http://127.0.0.1:8787/");
        std::fs::write(dir.join("golem.toml"), "CONSOLE_HOST = \"192.0.2.7\"\n").expect("write");
        assert_eq!(console_url(&dir), "http://192.0.2.7:8787/");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_loopback_addresses_keep_the_console_local() {
        for host in ["127.0.0.1", "127.1.2.3", "localhost", "::1"] {
            assert!(is_loopback(host), "{host}");
        }
        for host in ["0.0.0.0", "::", "192.0.2.7"] {
            assert!(!is_loopback(host), "{host}");
        }
    }
}
