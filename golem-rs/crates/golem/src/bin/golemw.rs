//! `golemw`: the app's windowed launcher, for a shortcut or a double-click.
//!
//! golem is a console program - it has to be, for `golem status` and the
//! rest to behave in a terminal - so starting it from the desktop opens a
//! console window for a moment. This is a windowed program instead: it
//! opens no console, starts `golem app` from the same folder with none
//! either, and exits.

#![windows_subsystem = "windows"]

use std::process::{Command, Stdio};

fn main() {
    let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(|d| d.to_path_buf())) else {
        return;
    };
    let golem = dir.join(if cfg!(windows) { "golem.exe" } else { "golem" });
    let mut command = Command::new(golem);
    command
        .arg("app")
        // Already detached: the app runs in this child, it does not
        // relaunch itself again. The name is app::DETACHED's (a test there
        // keeps the two equal); the literal keeps this program from linking
        // the whole of golem.
        .env("GOLEM_APP_DETACHED", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        command.creation_flags(DETACHED_PROCESS);
    }
    // Nothing to report to: this program has no console. A golem.exe that is
    // not beside it is a broken install, and the shortcut simply does nothing.
    let _ = command.spawn();
}
