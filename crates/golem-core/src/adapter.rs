//! The engine adapter's contract: turning a preset's flags into the arguments
//! an engine is actually started with.
//!
//! Downloading a release and spawning a process live in the binary; what is
//! here is the translation, and it has three rules:
//!
//! - a value of `false` means "do not pass this flag at all";
//! - `flag.<key> = --other` renames a canonical flag, and `flag.<key> = -`
//!   drops it with a warning (the engine has no equivalent);
//! - `map.<key>.<value> = <args...>` replaces the whole flag *and* its value,
//!   and `map.<key>.<value> = -` drops it with a warning.
//!
//! The distinction between `flag` and `map` is not decoration: `--mlock` is a
//! single flag with no argument, where `--spec-type draft-mtp` is two, so a
//! rename cannot express it and a map can.
//!
//! Everything is warning-carrying rather than silent. A flag the engine cannot
//! take is the difference between "it runs" and "it runs the way you asked",
//! so the start says so out loud.

use crate::ini::Document;

/// One engine's argument list, and what had to be dropped to get it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandLine {
    pub args: Vec<String>,
    /// `(key, value)` pairs the adapter has no equivalent for. The wording
    /// says which engine and which flag, because a dropped flag is a silent
    /// difference in behaviour if nobody says it.
    pub dropped: Vec<(String, String)>,
}

/// Translate `flags` (in the order the preset gave them) through `adapter`.
///
/// `adapter` is the parsed `config/engines/<name>.ini`; `flags` is what
/// `record::flags_of` produces for the section being served.
pub fn translate(flags: &[(String, String)], adapter: &Document) -> CommandLine {
    let mut out = CommandLine::default();
    for (key, value) in flags {
        // `false` means the flag is not wanted, not that it takes the word.
        if value == "false" {
            continue;
        }
        let mapped = adapter.global(&format!("map.{key}.{value}"));
        if let Some(replacement) = mapped {
            if replacement == "-" {
                out.dropped.push((key.clone(), value.clone()));
            } else {
                out.args.extend(replacement.split_whitespace().map(str::to_string));
            }
            continue;
        }
        let name = match adapter.global(&format!("flag.{key}")) {
            Some("-") => {
                out.dropped.push((key.clone(), value.clone()));
                continue;
            }
            Some(renamed) => renamed.to_string(),
            None => format!("--{key}"),
        };
        if value == "true" {
            out.args.push(name);
        } else {
            out.args.push(name);
            out.args.push(value.clone());
        }
    }
    out
}

/// The preset as the `backend` build is served it: each `key = value` line
/// the adapter names a `served.<backend>.<key>.<value>` for takes that
/// value instead, in every section and in `[*]`. A build can fail one value
/// of a flag on one backend - llama.cpp b11379 under Vulkan aborts locking
/// the 35B's split host buffer with `load-mode = mlock`, where `mmap+mlock`
/// pins the same weights and loads - and this keeps the preset as written
/// while the engine reads what that build can serve. `None` when no line
/// changes. Comments, headers and line endings pass through.
pub fn served(text: &str, adapter: &Document, backend: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    for raw in text.split_inclusive('\n') {
        let line = raw.trim_end_matches(['\r', '\n']);
        let eol = &raw[line.len()..];
        let trimmed = line.trim_start();
        let replacement = (!trimmed.starts_with([';', '#', '[']))
            .then(|| line.split_once('='))
            .flatten()
            .and_then(|(key, value)| {
                adapter.global(&format!("served.{backend}.{}.{}", key.trim(), value.trim()))
            });
        match replacement {
            Some(value) => {
                let key = line.split_once('=').map_or(line, |(key, _)| key).trim_end();
                out.push_str(&format!("{key} = {value}{eol}"));
                changed = true;
            }
            None => out.push_str(raw),
        }
    }
    changed.then_some(out)
}

/// Libraries a backend's release archive does not carry and its build loads,
/// with where to fetch them: llama.cpp's Windows ROCm zip has the HIP runtime
/// but not hipBLAS or rocBLAS, and AMD publishes those in a wheel. The
/// adapter names them under `<platform>_libraries_<backend>` (the archive's
/// URL), `..._sha256` (its hash), `..._from` (the folder inside it) and
/// `..._files` (the files and folders taken from that folder, which land
/// beside the server).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Libraries {
    pub url: String,
    /// Lowercase hex; the download is refused when it hashes otherwise.
    pub sha256: String,
    pub from: String,
    pub files: Vec<String>,
}

/// The [`Libraries`] the adapter names for `platform` and `backend`, or
/// `None` when it names none. A URL without a hash, a folder or a file list
/// is an error: a library golem places beside the engine is code it runs,
/// and it is taken only as pinned.
pub fn libraries(adapter: &Document, platform: &str, backend: &str) -> Result<Option<Libraries>, String> {
    let key = format!("{platform}_libraries_{backend}");
    let Some(url) = adapter.global(&key).filter(|v| !v.is_empty()) else { return Ok(None) };
    let part = |suffix: &str| {
        adapter
            .global(&format!("{key}_{suffix}"))
            .filter(|v| !v.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("the adapter names `{key}` without `{key}_{suffix}`"))
    };
    let sha256 = part("sha256")?.to_ascii_lowercase();
    if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("`{key}_sha256` is not a SHA-256: {sha256}"));
    }
    let from = part("from")?.trim_end_matches('/').to_string();
    let files: Vec<String> = part("files")?.split_whitespace().map(str::to_string).collect();
    Ok(Some(Libraries { url: url.to_string(), sha256, from, files }))
}

impl Libraries {
    /// The archive's file name: the URL's last segment, which is also the
    /// name it is kept under once fetched.
    pub fn archive(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or(&self.url)
    }

    /// The files and folders not yet beside the server, given whether each is
    /// there.
    pub fn missing(&self, present: impl Fn(&str) -> bool) -> Vec<&str> {
        self.files.iter().map(String::as_str).filter(|f| !present(f)).collect()
    }

    /// The archive members to extract: each of [`Libraries::files`] under
    /// [`Libraries::from`]. A folder member brings its contents with it.
    pub fn members(&self) -> Vec<String> {
        self.files.iter().map(|f| format!("{}/{f}", self.from)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The adapter's own keys, as `config/engines/llama.cpp.ini` writes them.
    const ROCM: &str = "windows_libraries_rocm = https://repo.radeon.com/rocm/windows/rocm-rel-7.2.1/rocm_sdk_libraries_custom-7.2.1-py3-none-win_amd64.whl\n\
        windows_libraries_rocm_sha256 = C7FE0B0731AF8896093FF69E11496830D3CB6A4AED73E895C60B7CBDC200BE92\n\
        windows_libraries_rocm_from = _rocm_sdk_libraries_custom/bin/\n\
        windows_libraries_rocm_files = hipblas.dll rocblas.dll rocsolver.dll libhipblaslt.dll rocblas hipblaslt\n";

    #[test]
    fn the_rocm_libraries_are_read_from_the_adapter() {
        let libs = libraries(&adapter(ROCM), "windows", "rocm").expect("well formed").expect("named");
        assert_eq!(libs.archive(), "rocm_sdk_libraries_custom-7.2.1-py3-none-win_amd64.whl");
        assert_eq!(libs.sha256, "c7fe0b0731af8896093ff69e11496830d3cb6a4aed73e895c60b7cbdc200be92");
        assert_eq!(libs.from, "_rocm_sdk_libraries_custom/bin", "a trailing slash is dropped");
        assert_eq!(
            libs.members()[..2],
            ["_rocm_sdk_libraries_custom/bin/hipblas.dll", "_rocm_sdk_libraries_custom/bin/rocblas.dll"]
        );
        assert_eq!(libs.members().last().map(String::as_str), Some("_rocm_sdk_libraries_custom/bin/hipblaslt"));
        assert_eq!(libs.missing(|f| f.ends_with(".dll")), vec!["rocblas", "hipblaslt"]);
        assert!(libs.missing(|_| true).is_empty());
        // Another backend or platform names none.
        assert_eq!(libraries(&adapter(ROCM), "windows", "vulkan"), Ok(None));
        assert_eq!(libraries(&adapter(ROCM), "linux", "rocm"), Ok(None));
    }

    #[test]
    fn a_value_the_backends_build_cannot_serve_is_served_as_the_adapter_says() {
        let adapter = adapter("served.vulkan.load-mode.mlock = mmap+mlock\n");
        let preset = "[*]\r\nload-mode = mlock\r\n; load-mode = mlock stays a comment\r\n[big]\r\nmodel = big.gguf\r\n\
                      load-mode=mlock\r\n[small]\r\nload-mode = mmap\r\n";
        let out = served(preset, &adapter, "vulkan").expect("changed");
        assert_eq!(
            out,
            "[*]\r\nload-mode = mmap+mlock\r\n; load-mode = mlock stays a comment\r\n[big]\r\nmodel = big.gguf\r\n\
             load-mode = mmap+mlock\r\n[small]\r\nload-mode = mmap\r\n"
        );
        let doc = crate::ini::parse(&out);
        assert_eq!(doc.effective("big", "load-mode"), Some("mmap+mlock"));
        // Another backend, or a preset with nothing to change, is served as written.
        assert_eq!(served(preset, &adapter, "cuda"), None);
        assert_eq!(served("[small]\nload-mode = mmap\n", &adapter, "vulkan"), None);
        // The shipped adapter carries the Vulkan rule for its pinned build.
        let shipped = self::adapter(include_str!("../../../config/engines/llama.cpp.ini"));
        assert!(served("[a]\nload-mode = mlock\n", &shipped, "vulkan").is_some_and(|t| t.contains("mmap+mlock")));
        assert_eq!(served("[a]\nload-mode = mlock\n", &shipped, "cuda"), None);
    }

    #[test]
    fn the_shipped_adapter_names_the_rocm_libraries_ggml_hip_loads() {
        let shipped = adapter(include_str!("../../../config/engines/llama.cpp.ini"));
        let libs = libraries(&shipped, "windows", "rocm").expect("well formed").expect("named");
        assert!(libs.url.starts_with("https://repo.radeon.com/"), "{}", libs.url);
        // hipblas.dll and rocblas.dll are what `devices::missing_hip` checks.
        for lib in crate::devices::HIP_LIBRARIES {
            assert!(libs.files.iter().any(|f| f == lib), "{lib}");
        }
        assert_eq!(libraries(&shipped, "windows", "cuda"), Ok(None));
    }

    #[test]
    fn libraries_without_their_pin_are_refused() {
        let unpinned: String = ROCM.lines().filter(|l| !l.contains("_sha256")).map(|l| format!("{l}\n")).collect();
        let err = libraries(&adapter(&unpinned), "windows", "rocm").expect_err("no hash");
        assert!(err.contains("windows_libraries_rocm_sha256"), "{err}");
        let short = ROCM.replace("C7FE0B0731AF", "C7");
        assert!(libraries(&adapter(&short), "windows", "rocm").is_err(), "not 64 hex digits");
    }

    fn adapter(text: &str) -> Document {
        crate::ini::parse(text)
    }

    fn flags(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn a_canonical_flag_is_passed_as_itself() {
        let line = translate(&flags(&[("ctx-size", "249344"), ("jinja", "true")]), &adapter(""));
        assert_eq!(line.args, vec!["--ctx-size", "249344", "--jinja"]);
        assert!(line.dropped.is_empty());
    }

    #[test]
    fn a_false_value_passes_nothing() {
        // `false` means "not wanted", which is not the same as "wanted false":
        // llama.cpp takes `--no-mmproj`, not `--mmproj false`.
        let line = translate(&flags(&[("mmproj", "false"), ("jinja", "true")]), &adapter(""));
        assert_eq!(line.args, vec!["--jinja"]);
    }

    #[test]
    fn a_rename_keeps_the_value_with_it() {
        let line = translate(
            &flags(&[("n-gpu-layers", "99")]),
            &adapter("flag.n-gpu-layers = --gpu-layers\n"),
        );
        assert_eq!(line.args, vec!["--gpu-layers", "99"]);
    }

    #[test]
    fn a_dropped_flag_is_named_rather_than_silently_ignored() {
        // llama.cpp has no `--sleep-idle-seconds`; the adapter says so.
        let line = translate(
            &flags(&[("sleep-idle-seconds", "600"), ("jinja", "true")]),
            &adapter("flag.sleep-idle-seconds = -\n"),
        );
        assert_eq!(line.args, vec!["--jinja"]);
        assert_eq!(line.dropped, vec![("sleep-idle-seconds".to_string(), "600".to_string())]);
    }

    #[test]
    fn a_map_replaces_the_flag_and_its_value_together() {
        // `--mlock` is one flag with no argument; a rename cannot express it.
        let line = translate(
            &flags(&[("load-mode", "mlock"), ("spec-type", "draft-mtp")]),
            &adapter(
                "map.load-mode.mlock = --mlock\nmap.spec-type.draft-mtp = --spec-type mtp\n",
            ),
        );
        assert_eq!(line.args, vec!["--mlock", "--spec-type", "mtp"]);
    }

    #[test]
    fn a_map_to_dash_drops_the_pair_and_says_which() {
        let line = translate(
            &flags(&[("load-mode", "auto")]),
            &adapter("map.load-mode.auto = -\n"),
        );
        assert!(line.args.is_empty());
        assert_eq!(line.dropped, vec![("load-mode".to_string(), "auto".to_string())]);
    }

    #[test]
    fn a_map_for_one_value_does_not_catch_the_others() {
        // `mmap` and `auto` both exist as values; only the one the adapter
        // names is replaced, and the other goes through as itself.
        let adapter = adapter("map.load-mode.mlock = --mlock\n");
        let line = translate(&flags(&[("load-mode", "mmap")]), &adapter);
        assert_eq!(line.args, vec!["--load-mode", "mmap"]);
    }

    #[test]
    fn the_presets_order_is_the_command_lines_order() {
        // The tuner's `write_candidate` puts its overrides at the top of the
        // section, and flags later in the file win in llama.cpp - so the order
        // is behaviour, not tidiness.
        let line = translate(
            &flags(&[("n-cpu-moe", "35"), ("ctx-size", "249344"), ("fit", "off")]),
            &adapter(""),
        );
        assert_eq!(
            line.args,
            vec!["--n-cpu-moe", "35", "--ctx-size", "249344", "--fit", "off"]
        );
    }
}
