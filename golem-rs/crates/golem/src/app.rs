//! The native app (`--features desktop`): a window showing the console the
//! service hosts, and a tray icon that stays when the window is closed.
//!
//! The window is a webview of the same page a browser gets, so there is one
//! console, not two. The service is the app's backend: the window and the
//! tray start it when it is not running, ask it for every engine verb, and
//! stop it on Quit. Closing the window only hides it; the tray's Open Golem
//! brings it back.
//!
//! The window has no system frame: the page draws the title bar (the mark,
//! the tabs, the state, minimise, maximise, close) and asks the window to
//! move, resize and close through the webview's message channel -
//! [`Chrome`] is that vocabulary. The page knows it is in the app from the
//! `?app` it is opened with.
//!
//! The tray icon is the mark with its spark showing the engine's state,
//! read every few seconds: green serving, grey stopped.
//!
//! One app per home: the running app listens on a loopback port it writes to
//! `.local/golem-app.port`, and a second launch asks it to show its window
//! and exits instead of adding a second window and tray icon.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use golem_core::mark::{self, Spark};
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Icon as WindowIcon, ResizeDirection, Window, WindowBuilder};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon as TrayImage, TrayIcon, TrayIconBuilder};

use crate::config::Overrides;
use crate::{desktop, engine, service};

/// How often the tray re-reads the engine's state.
const STATE_POLL: Duration = Duration::from_secs(3);

/// What the page asks of the window it draws the title bar for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chrome {
    /// Move the window with the pointer (a press on the title bar).
    Drag,
    Minimize,
    /// Maximise, or restore when maximised.
    Maximize,
    /// Hide the window; the tray keeps the app.
    Close,
    /// Resize from an edge or a corner.
    Resize(ResizeDirection),
    /// Show and focus the window: another launch found this app running.
    Show,
}

/// The page's message (`window.ipc.postMessage`), or `None` for anything
/// else: `drag`, `minimize`, `maximize`, `close`, `resize:<edge>` with the
/// edge as compass points (`n`, `se`, ...).
pub fn chrome(message: &str) -> Option<Chrome> {
    Some(match message {
        "drag" => Chrome::Drag,
        "minimize" => Chrome::Minimize,
        "maximize" => Chrome::Maximize,
        "close" => Chrome::Close,
        _ => Chrome::Resize(match message.strip_prefix("resize:")? {
            "n" => ResizeDirection::North,
            "s" => ResizeDirection::South,
            "e" => ResizeDirection::East,
            "w" => ResizeDirection::West,
            "ne" => ResizeDirection::NorthEast,
            "nw" => ResizeDirection::NorthWest,
            "se" => ResizeDirection::SouthEast,
            "sw" => ResizeDirection::SouthWest,
            _ => return None,
        }),
    })
}

fn apply(window: &Window, chrome: Chrome) {
    match chrome {
        Chrome::Drag => {
            let _ = window.drag_window();
        }
        Chrome::Minimize => window.set_minimized(true),
        Chrome::Maximize => window.set_maximized(!window.is_maximized()),
        Chrome::Close => window.set_visible(false),
        Chrome::Resize(edge) => {
            let _ = window.drag_resize_window(edge);
        }
        Chrome::Show => {
            window.set_visible(true);
            window.set_minimized(false);
            window.set_focus();
        }
    }
}

/// Where the running app says which port it listens on.
fn instance_file(home: &Path) -> PathBuf {
    home.join(".local").join("golem-app.port")
}

/// Ask an app already running for this home to show its window. `true`
/// when one answered.
fn wake_running(home: &Path) -> bool {
    let Some(port) = std::fs::read_to_string(instance_file(home)).ok().and_then(|p| p.trim().parse::<u16>().ok()) else {
        return false;
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
        Ok(mut stream) => stream.write_all(b"show\n").is_ok(),
        Err(_) => false,
    }
}

/// Listen for later launches, each a request to show the window. The file
/// is this app's claim on the home; a stale one (a killed app) answers no
/// connection and is overwritten here.
fn listen_for_launches(home: &Path, proxy: EventLoopProxy<Chrome>) {
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else { return };
    let Ok(addr) = listener.local_addr() else { return };
    let _ = std::fs::create_dir_all(home.join(".local"));
    let _ = std::fs::write(instance_file(home), addr.port().to_string());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_ok() {
                let _ = proxy.send_event(Chrome::Show);
            }
        }
    });
}

/// Set in the relaunched process: it is the app itself, not a launcher.
pub const DETACHED: &str = "GOLEM_APP_DETACHED";

/// Start the app as a process of its own with no console, and return.
///
/// golem is a console program, so Windows gives a double-click a console
/// window, and `golem app` typed in a terminal would hold that terminal for
/// the app's lifetime. The app relaunches itself detached instead - the
/// window and the tray need no console - and the launcher exits, taking its
/// console window with it. `Ok(false)` means this process already is the
/// app and should run it.
pub fn detach() -> Result<bool, String> {
    if std::env::var_os(DETACHED).is_some() {
        return Ok(false);
    }
    let exe = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    let mut command = std::process::Command::new(exe);
    command
        .arg("app")
        .env(DETACHED, "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }
    command.spawn().map_err(|e| format!("cannot start the app: {e}"))?;
    Ok(true)
}

/// Open the app. Returns only on failure before the window is up; once the
/// event loop runs, Quit ends the process.
pub fn run(home: &Path) -> Result<(), String> {
    if wake_running(home) {
        return Ok(());
    }
    service::ensure_running_as_child(home, &mut std::io::sink())?;
    // `?app` tells the page it draws this window's title bar.
    let url = format!("{}?app", desktop::console_url(home));

    let event_loop = EventLoopBuilder::<Chrome>::with_user_event().build();
    let builder = WindowBuilder::new()
        .with_title("Golem")
        .with_inner_size(tao::dpi::LogicalSize::new(1280.0, 860.0))
        .with_min_inner_size(tao::dpi::LogicalSize::new(720.0, 480.0))
        .with_window_icon(window_icon(64))
        // The page draws the title bar.
        .with_decorations(false);
    // Without a frame Windows drops the shadow too; it is what keeps the
    // window from looking cut out of the desktop.
    #[cfg(windows)]
    let builder = {
        use tao::platform::windows::WindowBuilderExtWindows;
        builder.with_undecorated_shadow(true)
    };
    let window = builder.build(&event_loop).map_err(|e| format!("cannot open the window: {e}"))?;
    // The webview's profile (cache, storage) lives in the home, not beside
    // the binary where the platform would put it.
    let mut context = wry::WebContext::new(Some(home.join(".local/webview")));
    let _webview = webview(&window, &url, &mut context, event_loop.create_proxy())?;
    listen_for_launches(home, event_loop.create_proxy());

    let open = MenuItem::new("Open Golem", true, None);
    let restart = MenuItem::new("Restart engine", true, None);
    let stop = MenuItem::new("Stop engine", true, None);
    let quit = MenuItem::new("Quit Golem", true, None);
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &open,
        &PredefinedMenuItem::separator(),
        &restart,
        &stop,
        &PredefinedMenuItem::separator(),
        &quit,
    ]);

    let home: PathBuf = home.to_path_buf();
    let mut tray: Option<TrayIcon> = None;
    let mut shown = Spark::Lit;
    let mut next_poll = Instant::now();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(next_poll);
        match event {
            // The tray is made once the loop runs, which some platforms
            // require of it.
            Event::NewEvents(StartCause::Init) => {
                tray = TrayIconBuilder::new()
                    .with_menu(Box::new(menu.clone()))
                    .with_tooltip("Golem")
                    .with_icon(tray_image(Spark::Lit))
                    .build()
                    .ok();
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => window.set_visible(false),
            Event::UserEvent(chrome) => apply(&window, chrome),
            _ => {}
        }

        while let Ok(item) = MenuEvent::receiver().try_recv() {
            if item.id == open.id() {
                window.set_visible(true);
                window.set_focus();
            } else if item.id == restart.id() {
                verb(&home, "restart");
            } else if item.id == stop.id() {
                verb(&home, "down");
            } else if item.id == quit.id() {
                // The engine and the service go with the app: Quit is the
                // one way out that leaves nothing resident.
                let _ = service::send(&home, "shutdown", &Overrides::default(), &mut std::io::sink());
                let _ = std::fs::remove_file(instance_file(&home));
                *control_flow = ControlFlow::Exit;
                return;
            }
        }

        if Instant::now() >= next_poll {
            next_poll = Instant::now() + STATE_POLL;
            let spark = if engine::status(&home).up { Spark::Serving } else { Spark::Stopped };
            if spark != shown {
                shown = spark;
                if let Some(tray) = &tray {
                    let _ = tray.set_icon(Some(tray_image(spark)));
                    let _ = tray.set_tooltip(Some(match spark {
                        Spark::Serving => "Golem - serving",
                        _ => "Golem - stopped",
                    }));
                }
            }
            *control_flow = ControlFlow::WaitUntil(next_poll);
        }
    });
}

/// An engine verb, on a thread of its own: a restart takes as long as a
/// model load, and the window and the tray must keep answering.
fn verb(home: &Path, verb: &'static str) {
    let home = home.to_path_buf();
    std::thread::spawn(move || {
        let _ = service::request(&home, verb, &Overrides::default(), &mut std::io::sink());
    });
}

fn window_icon(size: u32) -> Option<WindowIcon> {
    WindowIcon::from_rgba(mark::rgba(size, Spark::Lit), size, size).ok()
}

fn tray_image(spark: Spark) -> TrayImage {
    // 32 pixels: the platforms scale it to the tray, and whole-pixel cells
    // survive halving to 16.
    TrayImage::from_rgba(mark::rgba(32, spark), 32, 32).expect("the mark is 32x32 RGBA")
}

/// The console in the webview, told it is in the app, with the page's
/// window requests passed to the event loop.
fn builder<'a>(context: &'a mut wry::WebContext, url: &str, proxy: EventLoopProxy<Chrome>) -> wry::WebViewBuilder<'a> {
    wry::WebViewBuilder::with_web_context(context)
        .with_url(url)
        .with_initialization_script("window.__GOLEM_APP__ = true;")
        .with_ipc_handler(move |request| {
            if let Some(chrome) = chrome(request.body()) {
                let _ = proxy.send_event(chrome);
            }
        })
}

#[cfg(not(target_os = "linux"))]
fn webview(
    window: &Window,
    url: &str,
    context: &mut wry::WebContext,
    proxy: EventLoopProxy<Chrome>,
) -> Result<wry::WebView, String> {
    builder(context, url, proxy).build(window).map_err(|e| format!("cannot show the console: {e}"))
}

/// GTK wants the webview inside the window's container rather than on the
/// window itself.
#[cfg(target_os = "linux")]
fn webview(
    window: &Window,
    url: &str,
    context: &mut wry::WebContext,
    proxy: EventLoopProxy<Chrome>,
) -> Result<wry::WebView, String> {
    use tao::platform::unix::WindowExtUnix;
    use wry::WebViewBuilderExtUnix;
    let vbox = window.default_vbox().ok_or("the window has no container for the console")?;
    builder(context, url, proxy).build_gtk(vbox).map_err(|e| format!("cannot show the console: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_marks_its_child_as_the_app() {
        let launcher = include_str!("bin/golemw.rs");
        assert!(launcher.contains(&format!(".env(\"{DETACHED}\", \"1\")")), "golemw sets {DETACHED}");
    }

    #[test]
    fn the_page_vocabulary_maps_to_window_requests() {
        assert_eq!(chrome("drag"), Some(Chrome::Drag));
        assert_eq!(chrome("maximize"), Some(Chrome::Maximize));
        assert_eq!(chrome("close"), Some(Chrome::Close));
        assert_eq!(chrome("resize:se"), Some(Chrome::Resize(ResizeDirection::SouthEast)));
        assert_eq!(chrome("resize:w"), Some(Chrome::Resize(ResizeDirection::West)));
        for unknown in ["", "quit", "resize:", "resize:up", "Drag"] {
            assert_eq!(chrome(unknown), None, "{unknown:?}");
        }
    }
}
