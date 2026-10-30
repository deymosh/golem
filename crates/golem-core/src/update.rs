//! Golem's own updates, decided: the version a release tag names, the asset
//! this machine runs, the release answer read, and the small state file that
//! remembers what was checked, offered, skipped and installed.
//!
//! Pure: the binary fetches
//! `https://api.github.com/repos/deymosh/golem/releases/latest` with curl
//! and hands the text here, and reads and writes `data/cache/update.txt`
//! through [`parse_state`] / [`write_state`]. A checkout never updates
//! itself, so everything here is about the released binaries.

use crate::json::{self, Value};

/// A release tag as a version: `v0.1.2` or `0.1.2`, with a `-rc1`-style
/// suffix marking a prerelease.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    /// A tag like `v0.1.3-rc1`. Never offered: only finished releases are.
    pub prerelease: bool,
}

impl Version {
    /// `v0.1.2`, `0.1.2`, `v0.1.2-rc1`; `None` for anything else - a tag
    /// that is not three numbers is not one `is_newer` can rank.
    pub fn parse(text: &str) -> Option<Version> {
        let rest = text.trim();
        let rest = rest.strip_prefix('v').unwrap_or(rest);
        let (numbers, prerelease) = match rest.split_once('-') {
            Some((numbers, suffix)) => (numbers, !suffix.is_empty()),
            None => (rest, false),
        };
        let mut parts = numbers.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next()?.parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Version { major, minor, patch, prerelease })
    }
}

/// Whether the release `tag` names is one to offer over `current`: a newer
/// version, and not a prerelease - a suffix like `-rc1` is upstream's draft,
/// never a recommendation. A tag or a current version that does not parse
/// offers nothing.
pub fn is_newer(current: &str, tag: &str) -> bool {
    let Some(latest) = Version::parse(tag) else { return false };
    if latest.prerelease {
        return false;
    }
    match Version::parse(current) {
        Some(current) => (latest.major, latest.minor, latest.patch)
            > (current.major, current.minor, current.patch),
        None => false,
    }
}

/// The release asset this platform runs, or `None` where golem publishes
/// none. `desktop` says whether this build has the app window - the desktop
/// Linux asset links the system's WebKitGTK, so the two are separate files.
pub fn asset_name(os: &str, arch: &str, desktop: bool) -> Option<&'static str> {
    match (os, arch) {
        ("windows", "x86_64") => Some("golem-windows-x86_64.zip"),
        ("linux", "x86_64") => Some(if desktop {
            "golem-linux-x86_64-desktop"
        } else {
            "golem-linux-x86_64"
        }),
        ("macos", "aarch64") => Some("golem-macos-arm64"),
        _ => None,
    }
}

/// One release asset, as GitHub's JSON describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    /// `browser_download_url`: the redirect curl follows to the CDN.
    pub url: String,
    /// Size in bytes.
    pub size: u64,
}

/// The latest release, as `releases/latest` answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub tag_name: String,
    /// The release's page, for the console's "What's new" link.
    pub html_url: String,
    pub assets: Vec<Asset>,
}

/// Read `releases/latest`. A response without a tag or a member that is not
/// an asset is named, not guessed around.
pub fn parse_release(text: &str) -> Result<Release, String> {
    let doc = json::parse(text).map_err(|e| format!("the releases answer is not JSON: {e:?}"))?;
    let tag_name = doc
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or("the releases answer names no tag_name")?
        .to_string();
    let html_url = doc
        .get("html_url")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let assets = match doc.get("assets") {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|asset| {
                Some(Asset {
                    name: asset.get("name")?.as_str()?.to_string(),
                    url: asset.get("browser_download_url")?.as_str()?.to_string(),
                    size: asset.get("size").and_then(Value::as_f64).unwrap_or(0.0) as u64,
                })
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(Release { tag_name, html_url, assets })
}

/// The hash a `.sha256` file names for `asset`, lowercase: a
/// `<64 hex>  <asset>` line (`sha256sum`'s shape, one or two spaces or an
/// asterisk between), or a bare hash line standing for the one file the
/// checksum was written beside. `None` when the file does not name it.
pub fn expected_hash(text: &str, asset: &str) -> Option<String> {
    let is_hash = |s: &str| {
        s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b.to_ascii_lowercase()))
    };
    let mut bare = None;
    for line in text.lines() {
        let line = line.trim();
        let mut tokens = line.split_whitespace();
        let (hash, rest) = match (tokens.next(), tokens.next()) {
            (Some(hash), None) if is_hash(hash) => (hash, None),
            (Some(hash), Some(name)) if is_hash(hash) => (hash, Some(name)),
            _ => continue,
        };
        match rest {
            // `hex  name`: the line for one file among several.
            Some(name) if name.trim_start_matches('*') == asset => {
                return Some(hash.to_ascii_lowercase())
            }
            Some(_) => {}
            // A bare hash beside a single-file checksum: kept in case no
            // line names the asset, which it need not when it is the only
            // file there.
            None => bare = Some(hash.to_ascii_lowercase()),
        }
    }
    bare
}

/// What the last check left behind, in `data/cache/update.txt` as
/// `key = value` lines.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct State {
    /// Unix seconds of the last completed check.
    pub checked: Option<u64>,
    /// The tag the check found.
    pub latest: Option<String>,
    /// The release's page.
    pub url: Option<String>,
    /// The tag the user declined, so it is not offered again.
    pub skipped: Option<String>,
    /// The tag this binary installed, still to be restarted into.
    pub installed: Option<String>,
}

/// Read the state file's text. Unknown keys are kept out rather than
/// rejected: an older binary reading a newer file still works.
pub fn parse_state(text: &str) -> State {
    let mut state = State::default();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let value = value.trim();
        match key.trim() {
            "checked" => state.checked = value.parse().ok(),
            "latest" => state.latest = Some(value.to_string()),
            "url" => state.url = Some(value.to_string()),
            "skipped" => state.skipped = Some(value.to_string()),
            "installed" => state.installed = Some(value.to_string()),
            _ => {}
        }
    }
    state
}

/// The state as state-file text: only the keys that are set.
pub fn write_state(state: &State) -> String {
    let mut out = String::new();
    if let Some(checked) = state.checked {
        out.push_str(&format!("checked = {checked}\n"));
    }
    for (key, value) in [
        ("latest", &state.latest),
        ("url", &state.url),
        ("skipped", &state.skipped),
        ("installed", &state.installed),
    ] {
        if let Some(value) = value {
            out.push_str(&format!("{key} = {value}\n"));
        }
    }
    out
}

/// A check older than this is due for another.
pub const FRESH_SECS: u64 = 24 * 60 * 60;

/// Whether a check at `checked` unix seconds is due at `now`: it is when
/// there was never one, or the last is [`FRESH_SECS`] old.
pub fn due(checked: Option<u64>, now: u64) -> bool {
    match checked {
        Some(checked) => now.saturating_sub(checked) >= FRESH_SECS,
        None => true,
    }
}

/// How old a check may be when the console opens before it asks again:
/// opening the page several times in a row asks GitHub once.
pub const OPEN_RECHECK_SECS: u64 = 15 * 60;

/// Whether opening the console at `now` asks GitHub again: when there was
/// never a check, or the last is [`OPEN_RECHECK_SECS`] old.
pub fn due_on_open(checked: Option<u64>, now: u64) -> bool {
    match checked {
        Some(checked) => now.saturating_sub(checked) >= OPEN_RECHECK_SECS,
        None => true,
    }
}

/// The tag to offer the user running `current`, if any: `latest` is a
/// newer release, and not the one they declined.
pub fn offer(current: &str, state: &State) -> Option<String> {
    let latest = state.latest.as_deref()?;
    if is_newer(current, latest) && state.skipped.as_deref() != Some(latest) {
        Some(latest.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_parses_with_or_without_the_v() {
        assert_eq!(
            Version::parse("v0.1.2"),
            Some(Version { major: 0, minor: 1, patch: 2, prerelease: false })
        );
        assert_eq!(
            Version::parse("0.1.2"),
            Some(Version { major: 0, minor: 1, patch: 2, prerelease: false })
        );
        assert_eq!(
            Version::parse("v0.2.0-rc1"),
            Some(Version { major: 0, minor: 2, patch: 0, prerelease: true })
        );
        for bad in ["", "v1", "1.2", "v1.2.3.4", "release-1.2.3", "v1.2.x"] {
            assert_eq!(Version::parse(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn only_a_newer_finished_release_is_newer() {
        assert!(is_newer("0.1.1", "v0.1.2"));
        assert!(is_newer("0.1.1", "v0.2.0"));
        assert!(is_newer("0.1.1", "1.0.0"));
        assert!(!is_newer("0.1.2", "v0.1.2"), "the same version is not newer");
        assert!(!is_newer("0.1.3", "v0.1.2"));
        // A prerelease is upstream's draft, never an offer.
        assert!(!is_newer("0.1.2", "v0.1.3-rc1"));
        // An unparseable tag or current version offers nothing.
        assert!(!is_newer("0.1.2", "nightly"));
        assert!(!is_newer("dev", "v0.1.3"));
    }

    #[test]
    fn each_platform_names_its_asset() {
        assert_eq!(asset_name("windows", "x86_64", false), Some("golem-windows-x86_64.zip"));
        assert_eq!(asset_name("windows", "x86_64", true), Some("golem-windows-x86_64.zip"));
        assert_eq!(asset_name("linux", "x86_64", false), Some("golem-linux-x86_64"));
        assert_eq!(
            asset_name("linux", "x86_64", true),
            Some("golem-linux-x86_64-desktop")
        );
        assert_eq!(asset_name("macos", "aarch64", true), Some("golem-macos-arm64"));
        assert_eq!(asset_name("freebsd", "x86_64", false), None);
        assert_eq!(asset_name("linux", "aarch64", false), None);
    }

    /// `releases/latest`'s shape, trimmed to what this reads.
    const LATEST: &str = r#"{
      "tag_name": "v0.1.2",
      "html_url": "https://github.com/deymosh/golem/releases/tag/v0.1.2",
      "assets": [
        {"name": "golem-windows-x86_64.zip",
         "browser_download_url": "https://github.com/deymosh/golem/releases/download/v0.1.2/golem-windows-x86_64.zip",
         "size": 12345678},
        {"name": "golem-windows-x86_64.zip.sha256",
         "browser_download_url": "https://github.com/deymosh/golem/releases/download/v0.1.2/golem-windows-x86_64.zip.sha256",
         "size": 105},
        {"name": "golem-linux-x86_64",
         "browser_download_url": "https://github.com/deymosh/golem/releases/download/v0.1.2/golem-linux-x86_64",
         "size": 9876543},
        {"name": "Source code (zip)", "browser_download_url": "https://github.com/deymosh/golem/archive/refs/tags/v0.1.2.zip", "size": 1}
      ]
    }"#;

    #[test]
    fn the_release_answer_gives_the_tag_page_and_assets() {
        let release = parse_release(LATEST).expect("parse");
        assert_eq!(release.tag_name, "v0.1.2");
        assert_eq!(release.html_url, "https://github.com/deymosh/golem/releases/tag/v0.1.2");
        assert_eq!(release.assets.len(), 4, "every named asset with a URL is one");
        let zip = release.assets.first().expect("asset");
        assert_eq!(zip.name, "golem-windows-x86_64.zip");
        assert_eq!(zip.size, 12_345_678);
        assert!(zip.url.ends_with("golem-windows-x86_64.zip"));
        assert_eq!(
            parse_release(r#"{"html_url": "https://example.invalid"}"#)
                .unwrap_err(),
            "the releases answer names no tag_name"
        );
        assert!(parse_release("not json").is_err());
    }

    #[test]
    fn the_checksum_file_names_the_assets_hash() {
        // `sha256sum`'s output: two spaces between hash and name.
        let text = "\
                    9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08  golem-windows-x86_64.zip\n\
                    2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae  golem-linux-x86_64\n";
        assert_eq!(
            expected_hash(text, "golem-windows-x86_64.zip").as_deref(),
            Some("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")
        );
        assert_eq!(
            expected_hash(text, "golem-linux-x86_64").as_deref(),
            Some("2c26b46b68ffc68ff99b453c1d30413413422d706483bfa0f98a5e886266e7ae")
        );
        assert_eq!(expected_hash(text, "golem-macos-arm64"), None, "no line names it");
        // One space, and `sha256sum`'s binary mode with an asterisk.
        assert_eq!(
            expected_hash(
                "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08 *golem-windows-x86_64.zip\n",
                "golem-windows-x86_64.zip"
            )
            .as_deref(),
            Some("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08")
        );
        // A bare hash beside a single-file checksum, name or not.
        assert_eq!(
            expected_hash(
                "9F86D081884C7D659A2FEAA0C55AD015A3BF4F1B2B0B822CD15D6C15B0F00A08\n",
                "golem-windows-x86_64.zip"
            )
            .as_deref(),
            Some("9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"),
            "upper case comes back lower"
        );
        // A line that is not a checksum line is skipped, not fatal.
        assert_eq!(
            expected_hash("golem-windows-x86_64.zip: no such file\n", "golem-windows-x86_64.zip"),
            None
        );
    }

    #[test]
    fn the_state_file_round_trips() {
        let text = "checked = 1798200000\n\
                    latest = v0.1.2\n\
                    url = https://github.com/deymosh/golem/releases/tag/v0.1.2\n\
                    skipped = v0.1.1\n\
                    installed = v0.1.0\n";
        let state = parse_state(text);
        assert_eq!(state.checked, Some(1_798_200_000));
        assert_eq!(state.latest.as_deref(), Some("v0.1.2"));
        assert_eq!(state.skipped.as_deref(), Some("v0.1.1"));
        assert_eq!(state.installed.as_deref(), Some("v0.1.0"));
        assert_eq!(write_state(&state), text, "the written form is the read one");

        // An empty file, a comment and a key nobody knows: all fine.
        let partial = parse_state("# golem's update state\nlatest = v9.9.9\nmystery = 1\n");
        assert_eq!(partial.latest.as_deref(), Some("v9.9.9"));
        assert_eq!(partial.checked, None);
        assert_eq!(write_state(&State::default()), "");
    }

    #[test]
    fn a_check_is_due_when_a_day_old() {
        let now = 1_798_200_000;
        assert!(due(None, now), "never checked: due");
        assert!(!due(Some(now - 60), now));
        assert!(due(Some(now - FRESH_SECS), now));
        assert!(due(Some(now - FRESH_SECS - 1), now));
        // A checked stamp from the future (a clock moved back) is not due.
        assert!(!due(Some(now + 3600), now));
    }

    #[test]
    fn opening_the_console_asks_again_after_a_quarter_hour() {
        let now = 1_798_200_000;
        assert!(due_on_open(None, now));
        assert!(!due_on_open(Some(now - 60), now));
        assert!(due_on_open(Some(now - OPEN_RECHECK_SECS), now));
    }

    #[test]
    fn an_offer_is_a_newer_release_the_user_did_not_skip() {
        let state = State { latest: Some("v0.1.2".into()), ..State::default() };
        assert_eq!(offer("0.1.1", &state).as_deref(), Some("v0.1.2"));
        assert_eq!(offer("0.1.2", &state), None, "up to date: no offer");
        let skipped = State {
            latest: Some("v0.1.2".into()),
            skipped: Some("v0.1.2".into()),
            ..State::default()
        };
        assert_eq!(offer("0.1.1", &skipped), None, "declined once, not offered again");
        // A newer release after the skip is a fresh offer.
        let newer = State {
            latest: Some("v0.1.3".into()),
            skipped: Some("v0.1.2".into()),
            ..State::default()
        };
        assert_eq!(offer("0.1.1", &newer).as_deref(), Some("v0.1.3"));
    }
}
