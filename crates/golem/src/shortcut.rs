//! The app's shortcuts.
//!
//! On Windows, `Golem` in the Start menu and on the desktop, both starting
//! `golemw.exe`, the windowed launcher beside this binary. A `.lnk` is a COM
//! object, and this crate writes no unsafe code, so the shell's own
//! `WScript.Shell` makes it, from one PowerShell script. The paths reach the
//! script through its environment, never through its text: a folder name
//! with a quote in it stays a folder name.
//!
//! On Linux, a freedesktop menu entry - `golem.desktop` in the user's
//! applications folder, starting `golem app` - and the mark as its scalable
//! icon. A desktop session lists it with the other applications.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// The name both shortcuts carry, and the `.lnk` file's stem.
pub const NAME: &str = "Golem";

/// The script: one shortcut per special folder, each path printed once made.
///
/// `Programs` is the user's own Start-menu folder, which needs no elevation;
/// `Desktop` is asked of the shell rather than built from the profile path,
/// because a desktop redirected to OneDrive lives somewhere else.
pub const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$shell = New-Object -ComObject WScript.Shell
foreach ($folder in 'Programs', 'Desktop') {
    $path = Join-Path ([Environment]::GetFolderPath($folder)) ($env:GOLEM_LINK_NAME + '.lnk')
    $link = $shell.CreateShortcut($path)
    $link.TargetPath = $env:GOLEM_LINK_TARGET
    $link.WorkingDirectory = $env:GOLEM_LINK_DIR
    $link.IconLocation = $env:GOLEM_LINK_ICON + ',0'
    $link.Description = 'The local model, its engine and its console'
    $link.Save()
    $path
}
"#;

/// The launcher a shortcut starts: `golemw.exe` in the folder of `exe`.
pub fn launcher(exe: &Path) -> Option<PathBuf> {
    Some(exe.parent()?.join("golemw.exe"))
}

/// Make the shortcuts, saying each one made, or why none was. 0 when all
/// were written.
pub fn create(out: &mut dyn Write) -> i32 {
    if cfg!(windows) {
        windows(out)
    } else if cfg!(target_os = "linux") {
        linux(out)
    } else {
        let _ = writeln!(out, "[WARN] shortcuts are made on Windows and Linux");
        1
    }
}

/// The menu entry: `golem app` by this binary's absolute path, no terminal,
/// the `golem` icon.
pub fn desktop_entry(exe: &Path) -> String {
    format!(
        "[Desktop Entry]\nType=Application\nName={NAME}\n\
         Comment=The local model, its engine and its console\n\
         Exec={} app\nIcon=golem\nTerminal=false\nCategories=Development;\n",
        exec_argument(&exe.to_string_lossy())
    )
}

/// One `Exec` argument, quoted when it has to be.
///
/// The Desktop Entry rules come in two layers, and both apply: a quoted
/// argument escapes `"`, `` ` ``, `$` and `\` with a backslash, and the value
/// as a whole then escapes every backslash again - so a literal backslash
/// is four of them. `%` starts a field code and is written `%%`.
fn exec_argument(arg: &str) -> String {
    let plain = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c));
    if plain {
        return arg.to_string();
    }
    let mut quoted = String::from("\"");
    for c in arg.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            quoted.push('\\');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted.replace('\\', "\\\\").replace('%', "%%")
}

/// Where a user's own desktop data lives: `XDG_DATA_HOME`, else
/// `~/.local/share`.
fn data_home() -> Option<PathBuf> {
    match std::env::var_os("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".local/share")),
    }
}

fn linux(out: &mut dyn Write) -> i32 {
    let (Ok(exe), Some(data)) = (std::env::current_exe(), data_home()) else {
        let _ = writeln!(out, "[FAIL] cannot tell where this binary or the home folder is");
        return 1;
    };
    let entry = data.join("applications").join("golem.desktop");
    let icon = data.join("icons/hicolor/scalable/apps/golem.svg");
    let written = [
        (&entry, desktop_entry(&exe)),
        (&icon, golem_core::mark::svg(golem_core::mark::Spark::Lit)),
    ]
    .into_iter()
    .try_for_each(|(path, text)| {
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new("."))).and_then(|()| std::fs::write(path, text))
    });
    match written {
        Ok(()) => {
            let _ = writeln!(out, "[ OK ] menu entry: {}", entry.display());
            let _ = writeln!(out, "[ OK ] icon: {}", icon.display());
            0
        }
        Err(e) => {
            let _ = writeln!(out, "[FAIL] the menu entry was not written: {e}");
            1
        }
    }
}

fn windows(out: &mut dyn Write) -> i32 {
    let Some((exe, target)) = std::env::current_exe().ok().and_then(|exe| Some((exe.clone(), launcher(&exe)?)))
    else {
        let _ = writeln!(out, "[FAIL] cannot tell where this binary is");
        return 1;
    };
    if !target.is_file() {
        let _ = writeln!(
            out,
            "[FAIL] no {} beside {}: build with ./scripts/cargo.sh build --features desktop",
            target.display(),
            exe.display()
        );
        return 1;
    }
    let dir = target.parent().unwrap_or(Path::new("."));
    let made = Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT])
        .env("GOLEM_LINK_NAME", NAME)
        .env("GOLEM_LINK_TARGET", &target)
        .env("GOLEM_LINK_DIR", dir)
        .env("GOLEM_LINK_ICON", &exe)
        .stdin(Stdio::null())
        .output();
    match made {
        Ok(result) if result.status.success() => {
            for path in String::from_utf8_lossy(&result.stdout).lines().filter(|l| !l.trim().is_empty()) {
                let _ = writeln!(out, "[ OK ] shortcut: {}", path.trim());
            }
            0
        }
        Ok(result) => {
            let why = String::from_utf8_lossy(&result.stderr);
            let why = why.lines().find(|l| !l.trim().is_empty()).unwrap_or("no reason given");
            let _ = writeln!(out, "[FAIL] the shortcuts were not made: {}", why.trim());
            1
        }
        Err(e) => {
            let _ = writeln!(out, "[FAIL] cannot run PowerShell: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_launcher_is_the_one_beside_the_binary() {
        let exe = Path::new("C:/apps/golem/golem.exe");
        assert_eq!(launcher(exe), Some(PathBuf::from("C:/apps/golem/golemw.exe")));
    }

    #[test]
    fn the_menu_entry_starts_the_app_by_its_full_path() {
        let entry = desktop_entry(Path::new("/opt/golem/golem"));
        assert!(entry.starts_with("[Desktop Entry]\nType=Application\nName=Golem\n"), "{entry}");
        assert!(entry.contains("\nExec=/opt/golem/golem app\n"), "{entry}");
        assert!(entry.contains("\nIcon=golem\nTerminal=false\n"), "{entry}");
    }

    #[test]
    fn an_exec_path_is_quoted_by_both_layers_of_the_rules() {
        assert_eq!(exec_argument("/home/a/golem"), "/home/a/golem");
        assert_eq!(exec_argument("/home/a b/golem"), "\"/home/a b/golem\"");
        // `$` is `\$` once quoted, and the backslash doubles again.
        assert_eq!(exec_argument("/x/$y"), "\"/x/\\\\$y\"");
        assert_eq!(exec_argument("/x\\y"), "\"/x\\\\\\\\y\"");
        assert_eq!(exec_argument("/100%/g"), "\"/100%%/g\"");
    }

    #[test]
    fn the_script_takes_every_path_from_its_environment() {
        // A path spliced into the text would be PowerShell to run; read from
        // the environment it is only ever a value.
        for key in ["GOLEM_LINK_NAME", "GOLEM_LINK_TARGET", "GOLEM_LINK_DIR", "GOLEM_LINK_ICON"] {
            assert!(SCRIPT.contains(&format!("$env:{key}")), "{key}");
        }
        assert!(!SCRIPT.contains(":\\"), "a literal path in the script");
        // Both places a person looks for an app.
        assert!(SCRIPT.contains("'Programs', 'Desktop'"));
    }
}
