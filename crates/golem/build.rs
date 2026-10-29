//! The Windows resource: the mark as `golem.exe`'s icon, and the version
//! block that gives the process a name in Task Manager and Explorer.
//!
//! The icon is drawn from `golem_core::mark` at build time, so the
//! executable carries the same cells as the screen and the page and no image
//! is checked in. The resource compiler is the toolchain's own: `windres`
//! for a GNU build (the checkout's mingw-w64, on PATH through
//! `scripts/cargo.sh`), `rc.exe` from the Windows SDK for an MSVC one. With
//! neither found the binary still builds, without an icon, and says so.

use std::path::{Path, PathBuf};
use std::process::Command;

use golem_core::mark;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=../golem-core/src/mark.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("cargo sets OUT_DIR"));
    let icon = out.join("golem.ico");
    std::fs::write(&icon, mark::ico(&[16, 20, 24, 32, 48, 64, 256], mark::Spark::Lit))
        .expect("write the icon");
    let rc = out.join("golem.rc");
    std::fs::write(&rc, resource_script(&icon)).expect("write the resource script");

    let msvc = std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc");
    let compiled = if msvc { rc_exe(&rc, &out) } else { windres(&rc, &out) };
    match compiled {
        Ok(object) => println!("cargo:rustc-link-arg-bins={}", object.display()),
        Err(why) => println!("cargo:warning=golem.exe is built without its icon: {why}"),
    }
}

/// The `.rc` source: the icon, then the version block.
fn resource_script(icon: &Path) -> String {
    let version = std::env::var("CARGO_PKG_VERSION").expect("cargo sets the version");
    let part = |name: &str| std::env::var(name).ok().and_then(|v| v.parse::<u16>().ok()).unwrap_or(0);
    let numeric = format!(
        "{},{},{},0",
        part("CARGO_PKG_VERSION_MAJOR"),
        part("CARGO_PKG_VERSION_MINOR"),
        part("CARGO_PKG_VERSION_PATCH")
    );
    // Forward slashes: both compilers take them, and a backslash would need
    // escaping inside the quoted string.
    let icon = icon.display().to_string().replace('\\', "/");
    format!(
        r#"1 ICON "{icon}"

1 VERSIONINFO
FILEVERSION {numeric}
PRODUCTVERSION {numeric}
FILEOS 0x40004
FILETYPE 0x1
BEGIN
  BLOCK "StringFileInfo"
  BEGIN
    BLOCK "040904b0"
    BEGIN
      VALUE "FileDescription", "Golem"
      VALUE "ProductName", "Golem"
      VALUE "InternalName", "golem"
      VALUE "OriginalFilename", "golem.exe"
      VALUE "FileVersion", "{version}"
      VALUE "ProductVersion", "{version}"
    END
  END
  BLOCK "VarFileInfo"
  BEGIN
    VALUE "Translation", 0x409, 1200
  END
END
"#
    )
}

/// A GNU build: `windres` writes a COFF object the linker takes as it is.
fn windres(rc: &Path, out: &Path) -> Result<PathBuf, String> {
    let object = out.join("golem-res.o");
    // Found on PATH here rather than by the spawn: the spawn's own lookup has
    // missed a windres that was on PATH, and a full path cannot be missed.
    let tool = match std::env::var("WINDRES") {
        Ok(tool) => PathBuf::from(tool),
        Err(_) => on_path(&["windres.exe", "windres"]).ok_or("no windres on PATH")?,
    };
    let tool = tool.display().to_string();
    let status = Command::new(&tool)
        .arg("--input")
        .arg(rc)
        .args(["--output-format=coff", "--output"])
        .arg(&object)
        .status()
        .map_err(|e| format!("cannot run {tool}: {e}"))?;
    if !status.success() {
        return Err(format!("{tool} exited {status}"));
    }
    Ok(object)
}

/// An MSVC build: `rc.exe` writes a `.res` the linker takes as it is. It is
/// on PATH in a developer shell; elsewhere the newest x64 one under the
/// Windows 10/11 SDK is used.
fn rc_exe(rc: &Path, out: &Path) -> Result<PathBuf, String> {
    let res = out.join("golem.res");
    let tool = find_rc().ok_or("no rc.exe on PATH or in the Windows SDK")?;
    let status = Command::new(&tool)
        .args(["/nologo", "/fo"])
        .arg(&res)
        .arg(rc)
        .status()
        .map_err(|e| format!("cannot run {}: {e}", tool.display()))?;
    if !status.success() {
        return Err(format!("{} exited {status}", tool.display()));
    }
    Ok(res)
}

/// The first of `names` in a PATH directory.
fn on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .flat_map(|dir| names.iter().map(move |name| dir.join(name)))
        .find(|candidate| candidate.is_file())
}

fn find_rc() -> Option<PathBuf> {
    if Command::new("rc.exe").arg("/?").output().is_ok() {
        return Some(PathBuf::from("rc.exe"));
    }
    let kits = PathBuf::from(r"C:\Program Files (x86)\Windows Kits\10\bin");
    let mut versions: Vec<PathBuf> = std::fs::read_dir(&kits)
        .ok()?
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.join("x64").join("rc.exe").is_file())
        .collect();
    // The SDK's build numbers all have five digits (10.0.19041.0,
    // 10.0.22621.0), so the directories sort as text.
    versions.sort();
    versions.pop().map(|v| v.join("x64").join("rc.exe"))
}
