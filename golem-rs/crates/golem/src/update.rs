//! `golem update`: replace this binary with the latest release.
//!
//! Golem asks GitHub for `deymosh/golem`'s latest release once a day - from
//! the service, in the console, and before `golem up` in a terminal - and
//! installs it on the user's yes. The decisions are `golem_core::update`'s;
//! this is the transport and the file surgery.
//!
//! - **A checkout never updates itself.** Where the binary's directory or
//!   the home holds a `.git`, the verb says so and stops: the checkout's
//!   binary is build output, and `git pull` plus `./scripts/cargo.sh build`
//!   is its update.
//! - **Nothing is replaced before the download checks out.** The asset and
//!   its `.sha256` land in a fresh temp directory, the hash is verified
//!   against the file's bytes, and a mismatch ends the run with the install
//!   untouched. A release without a `.sha256` is said out loud.
//! - **A running exe can be renamed, not overwritten.** On Windows the old
//!   binary moves to `<name>.old` and the new one takes its place; on Unix
//!   the new binary is staged beside the old and renamed over it.
//!
//! Transport is `curl`, like the engine install (`release.rs`) and the model
//! pulls (`pull.rs`): the binary has no TLS of its own and GitHub is
//! HTTPS-only.

use std::io::{IsTerminal, Read, Write};
use std::path::Path;

use golem_core::sha256::{self, Sha256};
use golem_core::update as core;

use crate::release;

/// Where golem's releases are asked for.
const RELEASES_LATEST: &str = "https://api.github.com/repos/deymosh/golem/releases/latest";

/// The files the Windows zip carries and an install replaces. `golemw.exe`
/// finds `golem.exe` by name beside it, so both always move together.
const WINDOWS_BINARIES: [&str; 2] = ["golem.exe", "golemw.exe"];

/// `golem update [--check] [--yes]`.
pub fn run(home: &Path, args: &[String]) -> i32 {
    let mut out = std::io::stdout();
    if is_checkout(home) {
        let _ = writeln!(
            out,
            "[ .. ] this is a git checkout: update it with git pull and ./scripts/cargo.sh build"
        );
        return 0;
    }
    let check_only = args.iter().any(|a| a == "--check");
    let yes = args.iter().any(|a| a == "--yes");
    let current = env!("CARGO_PKG_VERSION");

    let latest = match fetch() {
        Ok(latest) => latest,
        Err(e) => {
            let _ = writeln!(out, "[FAIL] {e}");
            return 1;
        }
    };
    let state = record(home, &latest);

    if check_only {
        let _ = writeln!(
            out,
            "[ .. ] golem {current} is installed; the latest release is {} ({})",
            latest.tag_name, latest.html_url
        );
        return 0;
    }
    if !core::is_newer(current, &latest.tag_name) {
        let _ = writeln!(out, "[ OK ] golem v{current} is the latest");
        return 0;
    }

    if !yes && !confirm(&latest, current, &mut out) {
        let mut state = state;
        state.skipped = Some(latest.tag_name.clone());
        let _ = write_state(home, &state);
        let _ = writeln!(out, "[ .. ] skipped; golem update installs it whenever you ask again");
        return 0;
    }

    match install(&latest, &mut out) {
        Ok(()) => {
            let mut state = state;
            state.installed = Some(latest.tag_name.clone());
            let _ = write_state(home, &state);
            let _ = writeln!(
                out,
                "[ OK ] golem updated to {}. Restart Golem to use it: close the app window \
                 (or run golem service stop) and open it again.",
                latest.tag_name
            );
            0
        }
        Err(e) => {
            let _ = writeln!(out, "[FAIL] {e}");
            1
        }
    }
}

/// The offer `golem up` and `golem tui` make before they get on with it: on
/// an interactive terminal, when the checks are on and the last one is a day
/// old, ask about the release the check finds. `Some(0)` ends the command -
/// an update replaced the binary, and the user was told to run it again;
/// `None` carries on, with a declined release recorded as skipped.
///
/// Everything here is best-effort: a failed check is silent, because a
/// model that wants starting should not answer to GitHub.
pub fn prompt_if_due(home: &Path) -> Option<i32> {
    if !automatic(home) {
        return None;
    }
    let interactive = std::io::stdin().is_terminal() && std::io::stdout().is_terminal();
    if !interactive {
        return None;
    }
    let state = read_state(home);
    if !core::due(state.checked, now()) {
        return None;
    }
    let latest = fetch().ok()?;
    let state = record(home, &latest);
    if core::offer(env!("CARGO_PKG_VERSION"), &state).is_none() {
        return None;
    }
    let mut out = std::io::stdout();
    if confirm(&latest, env!("CARGO_PKG_VERSION"), &mut out) {
        return match install(&latest, &mut out) {
            Ok(()) => {
                let mut state = state;
                state.installed = Some(latest.tag_name.clone());
                let _ = write_state(home, &state);
                let _ = writeln!(
                    out,
                    "[ .. ] run the command again to carry on with the new golem"
                );
                Some(0)
            }
            Err(e) => {
                let _ = writeln!(out, "[FAIL] {e}");
                Some(1)
            }
        };
    }
    let mut state = state;
    state.skipped = Some(latest.tag_name.clone());
    let _ = write_state(home, &state);
    None
}

/// One pass of the service's automatic check: fetch and record when the
/// last check is a day old, silently. The service runs this in a background
/// thread, so a slow or failed check costs it nothing.
pub fn background_check(home: &Path) {
    if !automatic(home) {
        return;
    }
    let state = read_state(home);
    if !core::due(state.checked, now()) {
        return;
    }
    let _ = check(home);
}

/// Ask GitHub what the latest release is, and remember it.
pub fn check(home: &Path) -> Result<core::State, String> {
    let latest = fetch()?;
    Ok(record(home, &latest))
}

/// The releases answer, through curl with a time limit: this runs in front
/// of a service start and inside the daily check, and neither may wait on a
/// silent GitHub.
fn fetch() -> Result<core::Release, String> {
    let text = release::curl_text_with(RELEASES_LATEST, &["--max-time", "10"])?;
    core::parse_release(&text)
}

/// Fold a fetched release into the state file, and hand the state back.
fn record(home: &Path, latest: &core::Release) -> core::State {
    let mut state = read_state(home);
    state.checked = Some(now());
    state.latest = Some(latest.tag_name.clone());
    state.url = if latest.html_url.is_empty() { None } else { Some(latest.html_url.clone()) };
    let _ = write_state(home, &state);
    state
}

/// The question, where it can be asked. `false` - the default of an answer
/// the user did not type a yes into - records the skip at the caller.
fn confirm(latest: &core::Release, current: &str, out: &mut dyn Write) -> bool {
    if !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal()) {
        let _ = writeln!(
            out,
            "[ .. ] golem {} is available (you have v{current}); run golem update to install it",
            latest.tag_name
        );
        return false;
    }
    let _ = write!(
        out,
        "golem {} is available (you have v{current}). Update now? [y/N] ",
        latest.tag_name
    );
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Download the platform's asset, check it, and put it where this binary
/// lives. The temp directory goes either way: a failed update leaves
/// nothing to clean up by hand.
fn install(latest: &core::Release, out: &mut dyn Write) -> Result<(), String> {
    let Some(name) = core::asset_name(std::env::consts::OS, std::env::consts::ARCH, cfg!(feature = "desktop"))
    else {
        return Err(format!(
            "no release asset for {} {}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ));
    };

    let tmp = std::env::temp_dir().join(format!("golem-update-{}", latest.tag_name));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).map_err(|e| format!("cannot clear {}: {e}", tmp.display()))?;
    }
    std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;

    let result = replace_from(&tmp, latest, name, out);
    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// The download, the check and the move, against a directory the caller
/// chose - the seam the tests run the file surgery through without a
/// network.
fn replace_from(tmp: &Path, latest: &core::Release, name: &str, out: &mut dyn Write) -> Result<(), String> {
    let Some(asset) = latest.assets.iter().find(|a| a.name == name) else {
        return Err(format!("release {} has no {name} asset", latest.tag_name));
    };
    let file = tmp.join(&asset.name);
    release::curl_download(&asset.url, &file, &format!("update: {name}"), asset.size, out)?;

    // The checksum comes from the release too - same transport, same
    // progress - and a mismatch ends here, with nothing replaced.
    let hash_asset = format!("{name}.sha256");
    let expected = match hash_of(tmp, latest, name, &hash_asset, out)? {
        Some(expected) => expected,
        None => {
            let _ = writeln!(
                out,
                "[WARN] the release has no {hash_asset}: {name} could not be verified"
            );
            String::new()
        }
    };
    if !expected.is_empty() {
        let actual = sha256::hex(&file_hash(&file)?);
        if actual != expected {
            return Err(format!(
                "{name} does not match its .sha256 (got {actual}, expected {expected}) - nothing was replaced"
            ));
        }
    }

    let exe = std::env::current_exe().map_err(|e| format!("cannot find this binary: {e}"))?;
    #[cfg(windows)]
    {
        let _ = writeln!(out, "[ .. ] extracting {name}");
        release::extract(&release::tar_program(), &file, tmp)?;
        let target = exe.parent().unwrap_or(Path::new("."));
        install_windows(tmp, target)
    }
    #[cfg(not(windows))]
    {
        install_unix(&file, &exe)
    }
}

/// The hash the release's `.sha256` names for `name`, downloaded into
/// `tmp` - `None` when the release carries no `.sha256` for it. A checksum
/// file that names nothing is an error, not a silent skip: installing
/// against nothing is what the `[WARN]` above is for.
fn hash_of(
    tmp: &Path,
    latest: &core::Release,
    name: &str,
    hash_asset: &str,
    out: &mut dyn Write,
) -> Result<Option<String>, String> {
    let Some(asset) = latest.assets.iter().find(|a| a.name == hash_asset) else {
        return Ok(None);
    };
    let path = tmp.join(hash_asset);
    release::curl_download(&asset.url, &path, &format!("update: {hash_asset}"), asset.size, out)?;
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    core::expected_hash(&text, name)
        .map(Some)
        .ok_or_else(|| format!("{hash_asset} names no hash for {name}"))
}

/// The SHA-256 of a file, streamed: a release binary is far larger than a
/// read into memory should be.
fn file_hash(path: &Path) -> Result<[u8; 32], String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            return Ok(hasher.finish());
        }
        hasher.update(&buf[..n]);
    }
}

/// Windows: the extracted zip's two exes replace the ones beside the running
/// binary. Each old binary moves to `<name>.old` first (a running exe can be
/// renamed, not overwritten) and the new one takes its place; any failure
/// moves the `.old` files back, so a half-updated folder cannot happen.
fn install_windows(extracted: &Path, target: &Path) -> Result<(), String> {
    let mut swapped: Vec<&str> = Vec::new();
    let result = (|| {
        for name in WINDOWS_BINARIES {
            let new = extracted.join(name);
            if !new.is_file() {
                return Err(format!("the release zip has no {name}"));
            }
            let old = target.join(format!("{name}.old"));
            let _ = std::fs::remove_file(&old);
            let existing = target.join(name);
            if existing.exists() {
                std::fs::rename(&existing, &old)
                    .map_err(|e| format!("cannot move {name} aside: {e}"))?;
            }
            swapped.push(name);
            move_over(&new, &existing).map_err(|e| format!("cannot replace {name}: {e}"))?;
        }
        Ok(())
    })();
    if result.is_err() {
        // The folder as it was, however far the dance got.
        for name in &swapped {
            let _ = std::fs::remove_file(target.join(name));
            let _ = std::fs::rename(target.join(format!("{name}.old")), target.join(name));
        }
    }
    result
}

/// `from` to `to`, rename first, a copy when the rename cannot (across
/// volumes).
fn move_over(from: &Path, to: &Path) -> Result<(), String> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)
        .map(|_| ())
        .map_err(|e| format!("{} to {}: {e}", from.display(), to.display()))
}

/// Unix: the downloaded binary staged beside the running one, executable,
/// then renamed over it - one atomic step, however it was started.
#[cfg(unix)]
fn install_unix(new: &Path, exe: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let dir = exe.parent().ok_or_else(|| format!("{} has no directory", exe.display()))?;
    let name = exe.file_name().ok_or_else(|| format!("{} has no file name", exe.display()))?;
    let staged = dir.join(format!(".{}.new", name.to_string_lossy()));
    std::fs::copy(new, &staged).map_err(|e| format!("{} to {}: {e}", new.display(), staged.display()))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("cannot make {} executable: {e}", staged.display()))?;
    std::fs::rename(&staged, exe).map_err(|e| format!("{} over {}: {e}", staged.display(), exe.display()))
}

/// A best-effort sweep of the last update's leftovers, run early in every
/// command: the `.old` binaries a Windows update left beside this one. The
/// one still running cannot be removed, and that is fine - the next start
/// gets it.
pub fn cleanup_old_binaries() {
    let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(Path::to_path_buf))
    else {
        return;
    };
    for name in WINDOWS_BINARIES {
        let mut spelled = vec![name.to_string()];
        if cfg!(windows) {
            spelled.push(format!("{name}.exe"));
        }
        for spelled in spelled {
            let _ = std::fs::remove_file(dir.join(format!("{spelled}.old")));
        }
    }
}

/// Whether this binary is a checkout's build output: a `.git` beside it or
/// in the home. Both are looked at because the home is where the dev build
/// keeps its files and the exe directory is where the binary was put.
fn is_checkout(home: &Path) -> bool {
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .map(|dir| dir.join(".git").exists())
        .unwrap_or(false);
    beside_exe || home.join(".git").exists()
}

/// Whether the automatic checks may run here: not switched off in
/// `golem.toml`, and not a checkout. The verb itself is never disabled.
fn automatic(home: &Path) -> bool {
    if is_checkout(home) {
        return false;
    }
    crate::config::get(home, "UPDATE_CHECK").map_or(true, |v| !v.trim().eq_ignore_ascii_case("off"))
}

fn read_state(home: &Path) -> core::State {
    core::parse_state(&std::fs::read_to_string(crate::paths::update_state(home)).unwrap_or_default())
}

fn write_state(home: &Path, state: &core::State) -> std::io::Result<()> {
    let path = crate::paths::update_state(home);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, core::write_state(state))
}

/// Unix time, seconds; a clock before the epoch reads as "long ago".
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-update-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        dir
    }

    /// The rename dance on a target directory of fake files: an "exe" is a
    /// file, the same way a running golem.exe is one to the filesystem.
    #[test]
    fn both_windows_binaries_move_aside_and_back() {
        let extracted = temp("dance-from");
        let target = temp("dance-to");
        std::fs::write(extracted.join("golem.exe"), "new").expect("new exe");
        std::fs::write(extracted.join("golemw.exe"), "new-w").expect("new golemw");
        std::fs::write(target.join("golem.exe"), "old").expect("running exe");
        std::fs::write(target.join("golemw.exe"), "old-w").expect("running golemw");

        install_windows(&extracted, &target).expect("the swap");

        assert_eq!(std::fs::read_to_string(target.join("golem.exe")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(target.join("golemw.exe")).unwrap(), "new-w");
        // The .old files are this install's rollback material, and stay
        // until the next start sweeps them.
        assert_eq!(std::fs::read_to_string(target.join("golem.exe.old")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(target.join("golemw.exe.old")).unwrap(), "old-w");

        // A second install clears the stale .old first, so a rollback never
        // restores a binary two installs back. (The first swap renamed the
        // extracted files away, so the zip's contents are written again.)
        std::fs::write(extracted.join("golem.exe"), "newer").expect("newer exe");
        std::fs::write(extracted.join("golemw.exe"), "newer-w").expect("newer golemw");
        install_windows(&extracted, &target).expect("the second swap");
        assert_eq!(std::fs::read_to_string(target.join("golem.exe.old")).unwrap(), "new");
        std::fs::remove_dir_all(&extracted).ok();
        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn a_zip_missing_one_binary_rolls_the_other_back() {
        let extracted = temp("roll-from");
        let target = temp("roll-to");
        // golem.exe is there, golemw.exe is not: the first swap happens and
        // must come back.
        std::fs::write(extracted.join("golem.exe"), "new").expect("new exe");
        std::fs::write(target.join("golem.exe"), "old").expect("running exe");
        std::fs::write(target.join("golemw.exe"), "old-w").expect("running golemw");

        let err = install_windows(&extracted, &target).expect_err("golemw.exe is missing");
        assert!(err.contains("golemw.exe"), "{err}");
        assert_eq!(std::fs::read_to_string(target.join("golem.exe")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(target.join("golemw.exe")).unwrap(), "old-w");
        assert!(!target.join("golem.exe.old").exists(), "the rollback took its material with it");
        std::fs::remove_dir_all(&extracted).ok();
        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn a_target_without_the_binaries_yet_is_filled() {
        // A first install into a fresh folder: nothing to move aside.
        let extracted = temp("fresh-from");
        let target = temp("fresh-to");
        std::fs::write(extracted.join("golem.exe"), "new").expect("new exe");
        std::fs::write(extracted.join("golemw.exe"), "new-w").expect("new golemw");
        install_windows(&extracted, &target).expect("the swap");
        assert!(target.join("golem.exe").is_file());
        assert!(target.join("golemw.exe").is_file());
        assert!(!target.join("golem.exe.old").exists(), "nothing was moved aside");
        std::fs::remove_dir_all(&extracted).ok();
        std::fs::remove_dir_all(&target).ok();
    }

    #[test]
    fn a_checkout_is_refused_and_the_state_reads_the_config() {
        // The checkout test: a `.git` in the home is enough, wherever the
        // binary was put. (The exe directory is the test runner's own, and
        // never a checkout.)
        let dir = temp("checkout");
        std::fs::create_dir_all(dir.join(".git")).expect("git dir");
        assert!(is_checkout(&dir));

        let plain = temp("plain");
        assert!(!is_checkout(&plain));
        // UPDATE_CHECK = "off" stops the automatic checks and nothing else.
        std::fs::write(plain.join("golem.toml"), "UPDATE_CHECK = \"off\"\n").expect("config");
        assert!(!automatic(&plain));
        std::fs::write(plain.join("golem.toml"), "UPDATE_CHECK = \"ask\"\n").expect("config");
        assert!(automatic(&plain));
        std::fs::write(plain.join("golem.toml"), "").expect("config");
        assert!(automatic(&plain), "unset means on");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&plain).ok();
    }

    #[test]
    fn the_state_file_hangs_off_the_home() {
        let home = temp("state");
        let state = core::State {
            checked: Some(1_798_200_000),
            latest: Some("v0.1.2".into()),
            url: Some("https://github.com/deymosh/golem/releases/tag/v0.1.2".into()),
            skipped: None,
            installed: None,
        };
        write_state(&home, &state).expect("write");
        assert_eq!(read_state(&home), state);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn the_expected_hash_of_a_real_checksum_matches_the_file() {
        // The whole path over one small file: a written hash is what the
        // hasher says of the same bytes, read back through the parser.
        let dir = temp("hash");
        let file = dir.join("asset.zip");
        std::fs::write(&file, b"golem").expect("asset");
        let hex = sha256::hex(&file_hash(&file).expect("hash"));
        let checksum = dir.join("asset.zip.sha256");
        std::fs::write(&checksum, format!("{hex}  asset.zip\n")).expect("checksum");
        let text = std::fs::read_to_string(&checksum).unwrap();
        assert_eq!(core::expected_hash(&text, "asset.zip").as_deref(), Some(hex.as_str()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
