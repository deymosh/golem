//! Getting the pinned engine build onto the machine.
//!
//! `launch::up` calls [`ensure`] when the server executable is not on disk
//! yet, and this is what puts it there - the pinned GitHub release, extracted
//! into `.local/engines/<engine>/<build>-<backend>/`. Its rules:
//!
//! - the `BUILD` marker is written **last**, after every zip extracted: a
//!   killed install leaves the exe without it, and that half-state must
//!   reinstall, not run;
//! - the asset is picked by the adapter's `<platform>_asset_<backend>`
//!   pattern, which is a regex the binary has no crate for -
//!   `golem_core::pattern` is the subset those patterns use (a search,
//!   anchored only where the pattern writes `^` or `$`);
//! - `curl` is the transport because the binary has no TLS (see
//!   `engine::request`) and GitHub is HTTPS-only; `pull.rs` uses curl for
//!   Hugging Face the same way.
//!
//! The tar that extracts the zips is picked **by path, not by name on PATH**:
//! Git for Windows puts GNU tar first, and GNU tar cannot read a zip at all
//! ("This does not look like a tar archive"). `%SystemRoot%\System32\tar.exe`
//! is bsdtar and reads one.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use golem_core::ini;
use golem_core::json::{self, Value};
use golem_core::pattern;

/// The platform's name in an adapter's keys: `windows_repo`,
/// `linux_asset_cuda`, `macos_asset_metal`.
///
/// A platform is a fact about the machine, not a setting, so it is read rather
/// than configured: an adapter names an asset per platform and per backend,
/// and this picks the pair that applies. That is why one
/// `engines/llama.cpp/engine.ini` serves Windows and Linux with no mode key
/// between them - `windows_*` was never about Windows being *chosen*, only
/// about Windows being where the release path was first written.
pub fn platform() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// What to install, resolved by `launch::plan`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pinned {
    pub engine: String, // "llama.cpp"
    pub build: String,  // "b11379", or "latest" to follow upstream
    pub backend: String, // "cuda" | "rocm" | "vulkan" | "metal"
    /// `home/.local/engines/<engine>` - *not* the directory the build ends up
    /// in. `latest` is only a tag once GitHub has been asked, so the build's
    /// directory is named after the resolved tag (see [`install_dir`]).
    pub root: PathBuf,
    pub server: String, // "llama-server.exe"
}

/// What [`ensure`] left on disk: the release, and where it put it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    pub tag: String,
    /// `root/<tag>-<backend>`.
    pub dir: PathBuf,
}

/// One release asset, as GitHub's JSON describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Asset {
    name: String,
    /// `browser_download_url`: the redirect curl follows to the CDN.
    url: String,
    /// `size` in bytes, only for the `[ .. ] name (N MB)` line.
    size: u64,
}

/// Make sure the pinned release is installed under `pinned.root`, downloading
/// and extracting it from GitHub when it is not. Returns the tag that is
/// installed and the directory holding it - which for `latest` is only known
/// here. `Err` is a message the caller prints after `[FAIL] `.
pub fn ensure(home: &Path, pinned: &Pinned, out: &mut dyn Write) -> Result<Installed, String> {
    let ini_path = crate::paths::adapter_path(home, &pinned.engine);
    let text = crate::paths::adapter(home, &pinned.engine)
        .map_err(|e| format!("cannot read {}: {e}", ini_path.display()))?;
    let adapter = ini::parse(&text);

    // 1. An adapter with a `launcher` key names an engine that installs
    //    itself with its own tooling (its own setup, environment and model).
    //    There is no release asset to fetch, so golem cannot install it.
    if let Some(launcher) = adapter_key(&adapter, "launcher") {
        return Err(format!(
            "engine {} installs itself with its `{launcher}` launcher ({}), which golem does \
             not speak, so this engine cannot be installed or started by golem",
            pinned.engine,
            ini_path.display()
        ));
    }

    // 2. The release source for the platform this binary is running on. The
    //    message names the key, because the answer is "add it to the adapter"
    //    and a reader who cannot see which key is wanted cannot do that.
    let repo_key = format!("{}_repo", platform());
    let repo = adapter_key(&adapter, &repo_key).ok_or_else(|| {
        format!(
            "engine {} has no release source for {} (`{repo_key}` in {})",
            pinned.engine,
            platform(),
            ini_path.display()
        )
    })?;

    // 3. The pattern for this backend's server archive. Without it there is
    //    nothing to pick out of the release.
    let asset_key = format!("{}_asset_{}", platform(), pinned.backend);
    let asset_pattern = adapter_key(&adapter, &asset_key).ok_or_else(|| {
        format!(
            "engine {} has no `{asset_key}` in {}",
            pinned.engine,
            ini_path.display()
        )
    })?;

    // 4. "latest" follows upstream (ungated), so the tag is only known after
    //    asking - and a release with no matching asset (a nightly pointer) is
    //    skipped rather than chosen.
    let mut tag = pinned.build.clone();
    let mut listed: Option<Value> = None;
    if tag == "latest" {
        let url = format!("https://api.github.com/repos/{repo}/releases?per_page=15");
        let releases = json::parse(&curl_text(&url)?)
            .map_err(|e| format!("cannot parse the releases of {repo}: {e}"))?;
        let release = first_release_with_asset(&releases, &asset_pattern)?.ok_or_else(|| {
            format!("No {} {} release of {} found on GitHub", pinned.backend, platform(), pinned.engine)
        })?;
        tag = tag_of(&release).ok_or_else(|| {
            format!("the newest {} release of {} has no tag_name", pinned.backend, pinned.engine)
        })?;
        // Keep it: the install below reuses this release rather than asking
        // GitHub for the same tag twice.
        listed = Some(release);
    }

    let marker = format!("{tag} {}", pinned.backend);
    let dir = install_dir(&pinned.root, &tag, &pinned.backend);
    let installed = |tag: String, dir: &Path| Installed { tag, dir: dir.to_path_buf() };

    // 5. The marker is "<tag> <backend>"; when it already reads that, the
    //    install is done and a second `up` costs nothing.
    if marker_at(&dir).as_deref() == Some(marker.as_str()) {
        return Ok(installed(tag, &dir));
    }

    // 6. Install.
    let _ = writeln!(
        out,
        "[ .. ] installing {} {tag} ({}) from github.com/{repo}...",
        pinned.engine, pinned.backend
    );
    // Start from an empty directory: a killed install leaves a
    // half-extracted one, and it must not be run.
    if dir.exists() {
        std::fs::remove_dir_all(&dir)
            .map_err(|e| format!("cannot clear {}: {e}", dir.display()))?;
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;

    let release = match listed {
        Some(release) => release,
        None => {
            let url = format!("https://api.github.com/repos/{repo}/releases/tags/{tag}");
            json::parse(&curl_text(&url)?)
                .map_err(|e| format!("cannot parse release {tag} of {repo}: {e}"))?
        }
    };
    let assets = assets_of(&release);

    let server_zip = asset_for(&assets, &asset_pattern)?.ok_or_else(|| {
        format!(
            "no {} {} asset of {} in release {tag}",
            pinned.backend,
            platform(),
            pinned.engine
        )
    })?;
    let mut chosen = vec![server_zip];

    // e.g. llama.cpp's CUDA build ships its runtime DLLs as a separate zip. A
    // key that is set but matches nothing is an error rather than a skipped
    // runtime: a missing DLL is a worse failure than a loud one.
    let runtime_key = format!("{}_runtime_{}", platform(), pinned.backend);
    if let Some(runtime_pattern) = adapter_key(&adapter, &runtime_key) {
        let runtime = asset_for(&assets, &runtime_pattern)?.ok_or_else(|| {
            format!(
                "Engine {} names a `{runtime_key}` ({runtime_pattern}) but release {tag} has no asset matching it",
                pinned.engine
            )
        })?;
        chosen.push(runtime);
    }

    // The downloads are removed after a success - Windows never empties its
    // temp folder, and a CUDA build leaves half a gigabyte there - and kept
    // after a failure, where they are what to look at.
    let tmp = std::env::temp_dir().join(format!("golem-{}-{tag}", pinned.engine));
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;

    let tar = tar_program();
    for asset in &chosen {
        let mb = (asset.size as f64 / 1_048_576.0).round() as u64;
        let _ = writeln!(out, "[ .. ] {} ({mb} MB)", asset.name);
        let zip = tmp.join(&asset.name);
        curl_download(&asset.url, &zip)?;
        extract(&tar, &zip, &dir)?;
    }

    lift(&dir, &pinned.server, &pinned.engine, &tag)?;

    // The marker is written last: a killed install leaves the exe without it,
    // and that half-state must reinstall, not run.
    write_marker(&dir, &tag, &pinned.backend)?;
    let _ = std::fs::remove_dir_all(&tmp);

    let _ = writeln!(
        out,
        "[ OK ] {} {tag} ({}) installed to {}",
        pinned.engine,
        pinned.backend,
        dir.display()
    );
    Ok(installed(tag, &dir))
}

/// Where a release goes: `root/<tag>-<backend>`.
///
/// `tag` is the resolved tag, never the adapter's `latest`: a `build =
/// latest` installs into the directory of the tag it resolved to and not
/// into one called `latest`.
fn install_dir(root: &Path, tag: &str, backend: &str) -> PathBuf {
    root.join(format!("{tag}-{backend}"))
}

/// A required global of the adapter, or `None` when absent or blank - an empty
/// assignment counts as unset.
fn adapter_key(adapter: &ini::Document, key: &str) -> Option<String> {
    adapter.global(key).filter(|value| !value.is_empty()).map(str::to_string)
}

/// The `tag_name` of a release.
fn tag_of(release: &Value) -> Option<String> {
    release.get("tag_name").and_then(Value::as_str).map(str::to_string)
}

/// One release's assets, as its JSON describes them. A member without a name
/// or a URL is not an asset worth carrying.
fn assets_of(release: &Value) -> Vec<Asset> {
    let Some(Value::Array(items)) = release.get("assets") else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|asset| {
            Some(Asset {
                name: asset.get("name")?.as_str()?.to_string(),
                url: asset.get("browser_download_url")?.as_str()?.to_string(),
                size: asset.get("size").and_then(Value::as_f64).unwrap_or(0.0) as u64,
            })
        })
        .collect()
}

/// The first asset whose name matches `pattern`, or `None`. A pattern the
/// matcher rejects is an error rather than a silent no-match.
fn asset_for(assets: &[Asset], pattern: &str) -> Result<Option<Asset>, String> {
    for asset in assets {
        if pattern::matches(pattern, &asset.name)? {
            return Ok(Some(asset.clone()));
        }
    }
    Ok(None)
}

/// The first release carrying at least one asset that matches `pattern`, so
/// a stub release (a nightly pointer with no assets) is skipped.
fn first_release_with_asset(releases: &Value, pattern: &str) -> Result<Option<Value>, String> {
    let Value::Array(items) = releases else {
        return Err("the GitHub releases response is not an array".to_string());
    };
    for release in items {
        if asset_for(&assets_of(release), pattern)?.is_some() {
            return Ok(Some(release.clone()));
        }
    }
    Ok(None)
}

/// The `BUILD` marker's trimmed content, or `None` when the file is absent.
///
/// Trimmed, so a trailing newline (a marker written by another tool, a CRLF
/// checkout) does not change the answer.
fn marker_at(dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(dir.join("BUILD")).ok()?;
    Some(text.trim().to_string())
}

/// Write `<dir>/BUILD` holding exactly `"<tag> <backend>"`.
fn write_marker(dir: &Path, tag: &str, backend: &str) -> Result<(), String> {
    let path = dir.join("BUILD");
    std::fs::write(&path, format!("{tag} {backend}"))
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// The wrapping-top-folder fixup: some zips put everything under one folder,
/// so find the server wherever it landed and move its directory's entries up
/// into `dir`. The emptied subdirectory is left behind.
fn lift(dir: &Path, server: &str, engine: &str, tag: &str) -> Result<(), String> {
    if dir.join(server).exists() {
        return Ok(());
    }
    let found = find_file(dir, server)
        .ok_or_else(|| format!("{server} not found in the {engine} {tag} zips"))?;
    let Some(from) = found.parent() else { return Ok(()) };
    if from == dir {
        return Ok(());
    }
    // The whole listing is read before anything moves. Iterating the
    // directory while renaming out of it is not the same thing on Windows,
    // where the
    // enumeration is a live `FindNextFile` that a changed directory can skip
    // entries in - and a skipped entry is a missing DLL, not a visible error.
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in std::fs::read_dir(from).map_err(|e| format!("cannot read {}: {e}", from.display()))? {
        let entry = entry.map_err(|e| format!("cannot read {}: {e}", from.display()))?;
        paths.push(entry.path());
    }
    for path in paths {
        let Some(name) = path.file_name() else { continue };
        let to = dir.join(name);
        if to.exists() {
            // `Move-Item -Force`: a colliding destination is replaced.
            remove_any(&to)?;
        }
        std::fs::rename(&path, &to)
            .map_err(|e| format!("cannot move {} to {}: {e}", path.display(), to.display()))?;
    }
    Ok(())
}

/// The first file named `name` anywhere under `dir`, depth-first.
fn find_file(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            subdirs.push(path);
        } else if path.file_name().and_then(|n| n.to_str()) == Some(name) {
            return Some(path);
        }
    }
    for sub in subdirs {
        if let Some(found) = find_file(&sub, name) {
            return Some(found);
        }
    }
    None
}

/// Remove a file or a directory, whichever `path` is.
fn remove_any(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot inspect {}: {e}", path.display()))?;
    if meta.is_dir() {
        std::fs::remove_dir_all(path)
            .map_err(|e| format!("cannot remove {}: {e}", path.display()))
    } else {
        std::fs::remove_file(path).map_err(|e| format!("cannot remove {}: {e}", path.display()))
    }
}

/// The tar to extract with: `%SystemRoot%\System32\tar.exe` (bsdtar, which
/// reads zip) when it exists, else `tar` from PATH.
fn tar_program() -> PathBuf {
    pick_tar(std::env::var_os("SystemRoot").map(PathBuf::from))
}

/// The pure half of [`tar_program`]: given a `SystemRoot` (or none), the tar to
/// use. `System32\tar.exe` wins when the file is there; `tar` otherwise - the
/// fallback for an unset `SystemRoot` and for the non-Windows case.
fn pick_tar(system_root: Option<PathBuf>) -> PathBuf {
    if let Some(root) = system_root {
        let candidate = root.join("System32").join("tar.exe");
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("tar")
}

/// `curl -fsSL <url>`, output captured; a failure carries curl's stderr.
fn curl_text(url: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args(["-fsSL", url])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl failed for {url} ({}): {}",
            code_text(out.status),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `curl -fL --retry 5 --retry-delay 3 -o <dest> <url>`.
///
/// Output is captured rather than inherited, so a failure's stderr is what the
/// caller's `[FAIL]` line carries.
fn curl_download(url: &str, dest: &Path) -> Result<(), String> {
    let out = Command::new("curl")
        .args(["-fL", "--retry", "5", "--retry-delay", "3", "-o"])
        .arg(dest)
        .arg(url)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "download failed for {url} ({}): {}",
            code_text(out.status),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `<tar> -xf <zip> -C <dir>`, output captured; a failure carries tar's stderr.
fn extract(tar: &Path, zip: &Path, dir: &Path) -> Result<(), String> {
    let out = Command::new(tar)
        .arg("-xf")
        .arg(zip)
        .arg("-C")
        .arg(dir)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {}: {e}", tar.display()))?;
    if !out.status.success() {
        return Err(format!(
            "{} could not extract {} ({}): {}",
            tar.display(),
            zip.display(),
            code_text(out.status),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// The exit code as a word for an error line: a process killed by a signal has
/// no code, and `unwrap()` on `code()` would be the bug.
fn code_text(status: std::process::ExitStatus) -> String {
    match status.code() {
        Some(code) => format!("exit code {code}"),
        None => "killed by a signal".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-release-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        dir
    }

    /// A home with one engine adapter, enough for `ensure` to answer without
    /// touching the network.
    fn adapter_home(tag: &str, engine: &str, adapter: &str) -> PathBuf {
        let dir = home(tag);
        let engines = dir.join("engines").join(engine);
        std::fs::create_dir_all(&engines).expect("engine dir");
        std::fs::write(engines.join("engine.ini"), adapter).expect("adapter");
        dir
    }

    /// An adapter carrying this platform's release keys, so the same tests
    /// pass on all three (CI runs them on all three).
    fn adapter_text(repo: &str, asset: &str) -> String {
        let platform = platform();
        format!(
            "name = llama.cpp\nbuild = b11379\n{platform}_repo = {repo}\n\
             {platform}_asset_cuda = {asset}\n"
        )
    }

    fn pinned(home: &Path, engine: &str, build: &str, backend: &str, server: &str) -> Pinned {
        Pinned {
            engine: engine.to_string(),
            build: build.to_string(),
            backend: backend.to_string(),
            root: home.join(".local/engines").join(engine),
            server: server.to_string(),
        }
    }

    /// Where a build lands: `root/<tag>-<backend>`, computed from the resolved
    /// tag rather than from what the plan asked for - the difference `latest`
    /// makes.
    fn dir_of(home: &Path, engine: &str, tag: &str, backend: &str) -> PathBuf {
        home.join(".local/engines").join(engine).join(format!("{tag}-{backend}"))
    }

    /// A releases response with a stub release (no assets) ahead of the real
    /// one - the shape `releases?per_page=15` actually returns.
    const RELEASES: &str = r#"[
      {"tag_name": "b11400", "assets": []},
      {"tag_name": "b11399", "assets": [
        {"name": "cudart-llama-bin-win-cuda-13.1-x64.zip",
         "browser_download_url": "https://example.invalid/cudart.zip", "size": 314572800},
        {"name": "llama-b11399-bin-win-cuda-13.1-x64.zip",
         "browser_download_url": "https://example.invalid/llama.zip", "size": 629145600}
      ]},
      {"tag_name": "b11398", "assets": [
        {"name": "llama-b11398-bin-win-cuda-13.1-x64.zip",
         "browser_download_url": "https://example.invalid/old.zip", "size": 1}
      ]}
    ]"#;

    const CUDA_ASSET: &str = r"^llama-.*bin-win-cuda-13\.\d+-x64\.zip$";

    #[test]
    fn the_newest_release_with_a_matching_asset_wins_and_a_stub_is_skipped() {
        let releases = json::parse(RELEASES).expect("fixture");
        let release =
            first_release_with_asset(&releases, CUDA_ASSET).expect("pattern").expect("a release");
        // b11400 has no assets at all, so it is not the one; b11399 is the
        // first with a matching asset, ahead of the also-matching b11398.
        assert_eq!(tag_of(&release).as_deref(), Some("b11399"));

        let asset = asset_for(&assets_of(&release), CUDA_ASSET).expect("pattern").expect("asset");
        assert_eq!(asset.name, "llama-b11399-bin-win-cuda-13.1-x64.zip");
        assert_eq!(asset.url, "https://example.invalid/llama.zip");
        assert_eq!(asset.size, 629_145_600);
        // The runtime zip is matched by its own key, not the server's.
        let runtime =
            asset_for(&assets_of(&release), r"^cudart-llama-bin-win-cuda-13\.\d+-x64\.zip$")
                .expect("pattern")
                .expect("asset");
        assert_eq!(runtime.name, "cudart-llama-bin-win-cuda-13.1-x64.zip");
    }

    #[test]
    fn a_release_with_no_matching_asset_is_no_release_at_all() {
        let releases = json::parse(RELEASES).expect("fixture");
        let none = first_release_with_asset(&releases, r"^llama-.*bin-win-rocm-").expect("pattern");
        assert!(none.is_none(), "no ROCm asset anywhere in the fixture");
    }

    #[test]
    fn the_marker_round_trips_and_ignores_a_trailing_newline() {
        let dir = home("marker");
        write_marker(&dir, "b11379", "cuda").expect("write");
        assert_eq!(marker_at(&dir).as_deref(), Some("b11379 cuda"));
        // A trailing newline (CRLF on Windows) is the same install.
        std::fs::write(dir.join("BUILD"), "b11379 cuda\r\n").expect("write");
        assert_eq!(marker_at(&dir).as_deref(), Some("b11379 cuda"));
        // A different backend is a different install.
        assert_ne!(marker_at(&dir).as_deref(), Some("b11379 rocm"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_installed_build_is_not_downloaded_again() {
        // The marker matches, so `ensure` returns before any network call -
        // this is what makes a second `up` free.
        let dir = adapter_home(
            "already",
            "llama.cpp",
            &adapter_text(
                "ggml-org/llama.cpp",
                r"^llama-.*bin-\w+-cuda-13\.\d+-x64\.(zip|tar\.gz)$",
            ),
        );
        let pinned = pinned(&dir, "llama.cpp", "b11379", "cuda", "llama-server.exe");
        let install = dir_of(&dir, "llama.cpp", "b11379", "cuda");
        std::fs::create_dir_all(&install).expect("install dir");
        std::fs::write(install.join("llama-server.exe"), "x").expect("exe");
        write_marker(&install, "b11379", "cuda").expect("marker");
        let installed = ensure(&dir, &pinned, &mut std::io::sink()).expect("already installed");
        assert_eq!(installed.tag, "b11379");
        assert_eq!(installed.dir, install);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_install_directory_is_named_after_the_resolved_tag() {
        // Not after what the plan asked for: `latest` is only a tag once
        // GitHub has been asked, so a plan that says `latest` cannot name the
        // directory - which is why `ensure` returns where it put it.
        let root = Path::new("/home/.local/engines/llama.cpp");
        assert_eq!(install_dir(root, "b11399", "cuda"), root.join("b11399-cuda"));
        assert_ne!(
            install_dir(root, "b11399", "cuda"),
            root.join("latest-cuda"),
            "the request is not the name"
        );
        // The backend is part of it, so the two builds do not collide.
        assert_ne!(install_dir(root, "b11399", "cuda"), install_dir(root, "b11399", "rocm"));
    }

    #[test]
    fn system32_bsdtar_wins_over_path() {
        assert_eq!(pick_tar(None), PathBuf::from("tar"), "no SystemRoot: PATH's tar");
        let missing = std::env::temp_dir().join("golem-rs-release-no-such-root");
        assert_eq!(
            pick_tar(Some(missing)),
            PathBuf::from("tar"),
            "no tar.exe under it: PATH's tar"
        );
        let root = home("systemroot");
        std::fs::create_dir_all(root.join("System32")).expect("mkdir");
        std::fs::write(root.join("System32/tar.exe"), "x").expect("write");
        assert_eq!(pick_tar(Some(root.clone())), root.join("System32").join("tar.exe"));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_wrapping_top_folder_is_lifted_up() {
        let dir = home("lift");
        let install = dir.join("bin");
        let wrap = install.join("llama-b11379-bin-win-cuda-13.1-x64");
        std::fs::create_dir_all(&wrap).expect("mkdir");
        std::fs::write(wrap.join("llama-server.exe"), "exe").expect("write");
        std::fs::write(wrap.join("ggml-cuda.dll"), "dll").expect("write");

        lift(&install, "llama-server.exe", "llama.cpp", "b11379").expect("lift");

        assert!(install.join("llama-server.exe").is_file(), "the server moved up");
        assert!(install.join("ggml-cuda.dll").is_file(), "and so did its neighbours");
        // The emptied subdirectory is left behind, as `Move-Item` leaves it.
        assert!(wrap.is_dir(), "the emptied folder stays");
        assert_eq!(std::fs::read_dir(&wrap).expect("read").count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_zip_with_no_wrapping_folder_needs_no_lift() {
        let dir = home("no-lift");
        std::fs::write(dir.join("llama-server.exe"), "exe").expect("write");
        lift(&dir, "llama-server.exe", "llama.cpp", "b11379").expect("nothing to do");
        assert!(dir.join("llama-server.exe").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_zip_without_the_server_names_what_was_missing() {
        let dir = home("no-server");
        let err = lift(&dir, "llama-server.exe", "llama.cpp", "b11379").expect_err("no server");
        assert_eq!(err, "llama-server.exe not found in the llama.cpp b11379 zips");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_adapter_key_is_read_and_a_blank_one_counts_as_absent() {
        let adapter = ini::parse(&format!(
            "{}_repo = ggml-org/llama.cpp\n{}_asset_cuda =\nlauncher = strata\n",
            platform(),
            platform()
        ));
        assert_eq!(
            adapter_key(&adapter, &format!("{}_repo", platform())).as_deref(),
            Some("ggml-org/llama.cpp")
        );
        // An empty assignment counts as unset.
        assert_eq!(adapter_key(&adapter, &format!("{}_asset_cuda", platform())), None);
        assert_eq!(adapter_key(&adapter, &format!("{}_asset_rocm", platform())), None, "a missing key");
        assert_eq!(adapter_key(&adapter, "launcher").as_deref(), Some("strata"));
    }

    #[test]
    fn an_engine_that_installs_itself_is_refused() {
        // Strata's `launcher = strata` has no zip asset to fetch; the binary
        // says so rather than guessing at its setup.
        let dir = adapter_home(
            "strata",
            "strata",
            "name = strata\nbuild = v0.1.45\nlauncher = strata\n",
        );
        let pinned = pinned(&dir, "strata", "v0.1.45", "cuda", "python.exe");
        let err = ensure(&dir, &pinned, &mut std::io::sink()).expect_err("strata installs itself");
        assert!(err.contains("strata") && err.contains("launcher"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_release_source_names_the_key_to_add() {
        // The message has to name the key, because the answer is "add it to
        // the adapter" and a reader who cannot see which key is wanted cannot
        // do that.
        let dir = adapter_home("no-repo", "llama.cpp", "name = llama.cpp\nbuild = b11379\n");
        let pinned = pinned(&dir, "llama.cpp", "b11379", "cuda", "llama-server.exe");
        let err = ensure(&dir, &pinned, &mut std::io::sink()).expect_err("no release source");
        assert!(
            err.starts_with(&format!(
                "engine llama.cpp has no release source for {} (`{}_repo` in ",
                platform(),
                platform()
            )),
            "{err}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missing_asset_key_for_the_backend_is_an_error() {
        let dir = adapter_home(
            "no-asset",
            "llama.cpp",
            &adapter_text("ggml-org/llama.cpp", ""),
        );
        let pinned = pinned(&dir, "llama.cpp", "b11379", "rocm", "llama-server.exe");
        let err = ensure(&dir, &pinned, &mut std::io::sink()).expect_err("no rocm asset key");
        assert!(err.contains(&format!("{}_asset_rocm", platform())), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
