//! `golem pull [preset]`: fetch a model's GGUF from Hugging Face.
//!
//! The port of `cmd_pull` in the `golem` script plus `scripts/download-model.sh`.
//! Transport is `curl`, which ships with Windows 10+ and with every Linux and
//! macOS this repository supports - the same reason the shell scripts need
//! only curl and awk, and the reason this command needs no HTTP crate.
//!
//! Two shapes, exactly as the shell has them. With no argument it fetches what
//! `.env` names (`HF_REPO` + `MODEL_FILE`, plus `DRAFT_FILE`); with a preset
//! name it walks the preset file's `; Download:` comments and fetches the
//! files of that one section that `data/models` is missing.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use golem_core::ini;
use golem_core::time::civil_from_days;

use crate::state::env_value;

/// The free space the download refuses to start below, in GB - the biggest
/// preset model plus its `.part` wants a generous margin.
const MIN_FREE_GB: u64 = 25;

/// The completeness floor when Hugging Face does not report a size.
const FLOOR_FALLBACK: u64 = 1_000_000_000;

/// The model `download-model.sh` falls back to when `.env` names none.
const DEFAULT_REPO: &str = "unsloth/Qwen3.6-35B-A3B-GGUF";
const DEFAULT_MODEL: &str = "Qwen3.6-35B-A3B-UD-Q3_K_M.gguf";

/// `golem pull [preset]`.
pub fn run(home: &Path, args: &[String]) -> i32 {
    let preset = match parse_args(args) {
        Ok(preset) => preset,
        Err(message) => {
            eprintln!("[ERROR] {message}");
            return 1;
        }
    };
    let mut log = Log::open(home);
    match run_inner(home, preset.as_deref(), &mut log) {
        Ok(()) => 0,
        Err(message) => {
            log.error(&message);
            log.error(&format!("Model download failed. Log: {}", log.path.display()));
            1
        }
    }
}

/// The preset to pull, or `None` for the `.env` model. More than one argument
/// (or a flag) is a usage error.
fn parse_args(args: &[String]) -> Result<Option<String>, String> {
    match args {
        [] => Ok(None),
        [preset] if !preset.starts_with('-') => Ok(Some(preset.clone())),
        [other, ..] => Err(format!("unexpected argument: {other}")),
    }
}

fn run_inner(home: &Path, preset: Option<&str>, log: &mut Log) -> Result<(), String> {
    log.section("LOCAL AI MODEL DOWNLOAD");

    // curl is the whole transport; without it there is nothing to try.
    if !have_tool("curl") {
        return Err("curl is required (present on every supported platform)".to_string());
    }
    log.ok(&format!("curl: {}", curl_version()));

    // The margin covers the model and the `.part` it is streamed into.
    match free_gb(home) {
        Some(gb) if gb >= MIN_FREE_GB => log.ok(&format!("Available disk space: {gb} GB")),
        Some(gb) => {
            return Err(format!(
                "Insufficient disk space: {gb} GB free, {MIN_FREE_GB} GB recommended."
            ))
        }
        None => return Err("cannot read free disk space (df -Pk)".to_string()),
    }

    let models_dir = home.join("data/models");
    std::fs::create_dir_all(&models_dir)
        .map_err(|e| format!("cannot create {}: {e}", models_dir.display()))?;

    let token = env_value(home, "HF_TOKEN");

    let jobs = match preset {
        Some(name) => preset_jobs(home, name)?,
        None => default_jobs(home),
    };

    for (repo, file) in &jobs {
        hf_get(log, &models_dir, repo, file, token.as_deref())?;
    }

    log.section("DOWNLOAD COMPLETE");
    log.line(&format!("Models dir: {}", models_dir.display()));
    log.line(&format!("Log       : {}", log.path.display()));
    log.line("");
    log.ok("Model download completed successfully.");
    Ok(())
}

/// The `(repo, file)` pairs `download-model.sh` fetches with no argument.
///
/// The draft comes first, as the shell does it, and only when `DRAFT_FILE` is
/// set; its repo is `DRAFT_HF_REPO` when that is set, otherwise the model's.
fn default_jobs(home: &Path) -> Vec<(String, String)> {
    let repo = env_value(home, "HF_REPO").unwrap_or_else(|| DEFAULT_REPO.to_string());
    let model = env_value(home, "MODEL_FILE").unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let mut jobs = Vec::new();
    if let Some(draft) = env_value(home, "DRAFT_FILE").filter(|f| !f.is_empty()) {
        let draft_repo = env_value(home, "DRAFT_HF_REPO")
            .filter(|r| !r.is_empty())
            .unwrap_or_else(|| repo.clone());
        jobs.push((draft_repo, draft));
    }
    jobs.push((repo, model));
    jobs
}

/// The `(repo, file)` pairs for one preset section that `data/models` lacks.
///
/// Router mode only: the preset file is `MODELS_PRESET`, and a section with no
/// `; Download:` comment at all is the shell's own error, verbatim.
fn preset_jobs(home: &Path, name: &str) -> Result<Vec<(String, String)>, String> {
    let preset = env_value(home, "MODELS_PRESET")
        .filter(|p| !p.is_empty())
        .ok_or("pull <preset> needs router mode (MODELS_PRESET in .env)")?;
    let text = std::fs::read_to_string(home.join(&preset))
        .map_err(|e| format!("cannot read {preset}: {e}"))?;

    let models_dir = home.join("data/models");
    let mut found = false;
    let mut jobs = Vec::new();
    for (section, repo, file) in preset_downloads(&text) {
        if section != name {
            continue;
        }
        found = true;
        // Already here: the shell fetches only what is missing.
        if models_dir.join(&file).is_file() {
            continue;
        }
        jobs.push((repo, file));
    }
    if !found {
        return Err(format!("no \"; Download:\" lines for preset [{name}] in {preset}"));
    }
    Ok(jobs)
}

/// The `section repo file` triples the preset's `; Download:` comments name.
///
/// The parsing itself is `golem_core::ini`, which is what the console's Models
/// tab reads the same file with; a second scanner here would be a second
/// answer to "which section owns this comment". The rule is the awk's: a
/// `; Download:` line belongs to the section header *below* it, and a line
/// with no `->` pair is not a download at all.
fn preset_downloads(text: &str) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for section in &ini::parse(text).sections {
        for download in &section.downloads {
            out.push((section.name.clone(), download.repo.clone(), download.file.clone()));
        }
    }
    out
}

/// Fetch one GGUF into `models_dir`, resuming a leftover `.part`.
///
/// Completeness is judged against the size Hugging Face reports
/// (`x-linked-size`), so small models are fine; the 1 GB floor is only the
/// fallback when the server does not say.
fn hf_get(
    log: &mut Log,
    models_dir: &Path,
    repo: &str,
    file: &str,
    token: Option<&str>,
) -> Result<(), String> {
    let path = models_dir.join(file);
    let url = format!("https://huggingface.co/{repo}/resolve/main/{file}");
    let expected = linked_size(&url, token);
    let floor = floor_of(expected);
    let expected_mb = expected.unwrap_or(0) / 1_048_576;

    if path.is_file() {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size >= floor {
            log.ok(&format!("{file} already present ({} MB).", size / 1_048_576));
            return Ok(());
        }
        log.warn(&format!(
            "{file} is incomplete ({} MB of {expected_mb} MB), retrying.",
            size / 1_048_576
        ));
        let _ = std::fs::remove_file(&path);
    }

    let size_note = if expected.is_some() { format!(" ({expected_mb} MB)") } else { String::new() };
    log.info(&format!("Downloading {file} from {repo}{size_note}..."));

    // -L follows the CDN redirect; -C - resumes the .part if one is lying
    // around; --retry rides out flaky links.
    let part = with_suffix(&path, ".part");
    let status = curl_download(&url, &part, token)?;
    if !status.success() {
        return Err(format!("curl failed for {file} (exit {:?})", status.code()));
    }

    let size = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if size < floor {
        return Err(format!(
            "{file} came back incomplete ({} MB of {expected_mb} MB) — \
             delete data/models/{file}.part and retry.",
            size / 1_048_576
        ));
    }
    std::fs::rename(&part, &path)
        .map_err(|e| format!("cannot move {} into place: {e}", part.display()))?;
    log.ok(&format!("{file} downloaded ({} MB).", size / 1_048_576));

    if have_tool("sha256sum") {
        match sha256sum(&path) {
            Ok(line) => {
                let _ = std::fs::write(with_suffix(&path, ".sha256"), &line);
                log.line(line.trim_end());
            }
            Err(e) => log.warn(&format!("sha256sum failed for {file}: {e}")),
        }
    }
    Ok(())
}

/// The size a file must reach to count as complete: 1% below what Hugging
/// Face reports, or the 1 GB fallback when the header is absent.
fn floor_of(expected: Option<u64>) -> u64 {
    match expected {
        Some(expected) => expected - expected / 100,
        None => FLOOR_FALLBACK,
    }
}

/// The `x-linked-size` of `url`, from `curl -sIL`; the last one wins, as the
/// awk's `tail -n1` takes it.
fn linked_size(url: &str, token: Option<&str>) -> Option<u64> {
    let mut command = Command::new("curl");
    command.arg("-sIL");
    if let Some(token) = token {
        command.arg("-H").arg(format!("Authorization: Bearer {token}"));
    }
    let out = command
        .arg(url)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    parse_linked_size(&String::from_utf8_lossy(&out.stdout))
}

/// The value of the last `x-linked-size` header line, case-insensitively.
fn parse_linked_size(headers: &str) -> Option<u64> {
    let mut last = None;
    for line in headers.lines() {
        let line = line.trim_end_matches('\r');
        if let Some((name, value)) = line.split_once(':') {
            if name.trim().eq_ignore_ascii_case("x-linked-size") {
                if let Ok(size) = value.trim().parse::<u64>() {
                    last = Some(size);
                }
            }
        }
    }
    last
}

/// `curl -fL -C - --retry 5 --retry-delay 3 -o <part> <url>`.
///
/// Progress goes to the inherited stderr, so a long download shows a meter;
/// the exit status is returned for the caller to judge.
fn curl_download(
    url: &str,
    part: &Path,
    token: Option<&str>,
) -> Result<std::process::ExitStatus, String> {
    let mut command = Command::new("curl");
    command.args(["-fL", "-C", "-", "--retry", "5", "--retry-delay", "3"]);
    if let Some(token) = token {
        command.arg("-H").arg(format!("Authorization: Bearer {token}"));
    }
    command
        .arg("-o")
        .arg(part)
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    command.status().map_err(|e| format!("cannot run curl: {e}"))
}

/// `sha256sum <path>`, whose one output line becomes the `.sha256` file.
fn sha256sum(path: &Path) -> Result<String, String> {
    let out = Command::new("sha256sum")
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err("sha256sum exited non-zero".to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Free space in GB on the filesystem holding `home`, from `df -Pk`.
fn free_gb(home: &Path) -> Option<u64> {
    let out = Command::new("df").arg("-Pk").arg(home).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // `Filesystem 1024-blocks Used Available Capacity Mounted on`; the fourth
    // field of the second line is what the shell's awk reads.
    let available_kb: u64 = text.lines().nth(1)?.split_whitespace().nth(3)?.parse().ok()?;
    Some(available_kb / 1024 / 1024)
}

/// Whether a program is on PATH (`where` on Windows, `which` elsewhere).
fn have_tool(name: &str) -> bool {
    let finder = if cfg!(windows) { "where" } else { "which" };
    Command::new(finder)
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// The first line of `curl --version`, for the log's "curl: ..." line.
fn curl_version() -> String {
    Command::new("curl")
        .arg("--version")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()
        .map(|out| {
            String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or("").to_string()
        })
        .unwrap_or_default()
}

/// `path` with `suffix` appended (`x.gguf` + `.part` = `x.gguf.part`).
fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

/// `YYYYmmdd-HHMMSS`, the stamp `download-model.sh` puts on its log file.
fn timestamp() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (year, month, day) = civil_from_days((secs / 86_400) as i64);
    let rem = secs % 86_400;
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Everything printed also lands in `logs/download-model-<stamp>.log`, the way
/// the shell's `exec > >(tee -a ...)` duplicates the run into a file.
struct Log {
    path: PathBuf,
    file: Option<File>,
}

impl Log {
    fn open(home: &Path) -> Log {
        let dir = home.join("logs");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("download-model-{}.log", timestamp()));
        let file = File::create(&path).ok();
        Log { path, file }
    }

    /// A normal line: to stdout, and to the log.
    fn line(&mut self, text: &str) {
        println!("{text}");
        self.write(text);
    }

    /// An error line: to stderr, and to the log.
    fn error(&mut self, text: &str) {
        eprintln!("[ERROR] {text}");
        self.write(&format!("[ERROR] {text}"));
    }

    fn info(&mut self, message: &str) {
        self.line(&format!("[INFO] {message}"));
    }

    fn ok(&mut self, message: &str) {
        self.line(&format!("[ OK ] {message}"));
    }

    fn warn(&mut self, message: &str) {
        self.line(&format!("[WARN] {message}"));
    }

    fn section(&mut self, title: &str) {
        self.line("");
        self.line("============================================================");
        self.line(&format!(" {title}"));
        self.line("============================================================");
    }

    fn write(&mut self, text: &str) {
        if let Some(file) = self.file.as_mut() {
            let _ = writeln!(file, "{text}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-pull-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        dir
    }

    #[test]
    fn args_are_none_or_one_preset() {
        assert_eq!(parse_args(&[]).unwrap(), None);
        assert_eq!(parse_args(&["tiny".into()]).unwrap(), Some("tiny".to_string()));
        assert!(parse_args(&["a".into(), "b".into()]).is_err());
        assert!(parse_args(&["--native".into()]).is_err(), "a flag is not a preset");
    }

    #[test]
    fn download_comments_belong_to_the_section_below_them() {
        // Two comments above `[model-a]`, one above `[tiny]`, and one with no
        // arrow (not a download at all). The comments above a header are that
        // header's - the awk buffers them and prints them at the next header.
        let text = "\
version = 1
; Download: someone/model-GGUF -> model-Q4_K_M.gguf
; Download: someone/model-GGUF -> mtp.gguf
; Download: no-arrow-here
[model-a]
model = model-Q4_K_M.gguf
; Download: tiny/tiny-GGUF -> tiny.gguf
[tiny]
model = tiny.gguf
";
        assert_eq!(
            preset_downloads(text),
            vec![
                (
                    "model-a".to_string(),
                    "someone/model-GGUF".to_string(),
                    "model-Q4_K_M.gguf".to_string()
                ),
                (
                    "model-a".to_string(),
                    "someone/model-GGUF".to_string(),
                    "mtp.gguf".to_string()
                ),
                ("tiny".to_string(), "tiny/tiny-GGUF".to_string(), "tiny.gguf".to_string()),
            ]
        );
    }

    #[test]
    fn a_comment_before_the_first_header_belongs_to_it() {
        // No section yet when the comment is read, so it waits for the first
        // header - the awk's `pending` buffer does the same.
        let text = "; Download: org/first-GGUF -> first.gguf\n[first]\nmodel = first.gguf\n";
        assert_eq!(
            preset_downloads(text),
            vec![("first".to_string(), "org/first-GGUF".to_string(), "first.gguf".to_string())]
        );
    }

    #[test]
    fn a_download_line_with_no_arrow_is_not_a_download() {
        let text = "; Download: someone/model\n[a]\nmodel = a.gguf\n";
        assert!(preset_downloads(text).is_empty());
    }

    #[test]
    fn a_file_the_comment_forgot_is_not_a_download() {
        let text = "; Download: someone/model -> \n[a]\nmodel = a.gguf\n";
        assert!(preset_downloads(text).is_empty(), "an arrow with nothing after it");
    }

    #[test]
    fn the_floor_is_one_percent_below_the_expected_size() {
        assert_eq!(floor_of(Some(1000)), 990);
        assert_eq!(floor_of(Some(100)), 99);
        assert_eq!(floor_of(Some(2_000_000_000)), 1_980_000_000);
        assert_eq!(floor_of(None), FLOOR_FALLBACK);
    }

    #[test]
    fn the_linked_size_is_the_last_header() {
        let headers = "HTTP/2 302\r\n\
                       x-linked-size: 100\r\n\
                       location: https://cdn\r\n\
                       HTTP/2 200\r\n\
                       X-Linked-Size: 200\r\n\r\n";
        assert_eq!(parse_linked_size(headers), Some(200), "the redirect's is not the one");
        assert_eq!(parse_linked_size("HTTP/2 200\r\ncontent-length: 5\r\n"), None);
    }

    #[test]
    fn the_default_jobs_are_the_env_model_and_its_draft() {
        let dir = home("jobs");
        std::fs::write(
            dir.join(".env"),
            "HF_REPO=\"org/model-GGUF\"\nMODEL_FILE=\"model.gguf\"\nDRAFT_FILE=\"draft.gguf\"\n",
        )
        .expect("write");
        assert_eq!(
            default_jobs(&dir),
            vec![
                ("org/model-GGUF".to_string(), "draft.gguf".to_string()),
                ("org/model-GGUF".to_string(), "model.gguf".to_string()),
            ]
        );

        // DRAFT_HF_REPO redirects the draft only.
        std::fs::write(
            dir.join(".env"),
            "HF_REPO=\"org/model-GGUF\"\nMODEL_FILE=\"model.gguf\"\n\
             DRAFT_FILE=\"draft.gguf\"\nDRAFT_HF_REPO=\"other/draft-GGUF\"\n",
        )
        .expect("write");
        assert_eq!(
            default_jobs(&dir),
            vec![
                ("other/draft-GGUF".to_string(), "draft.gguf".to_string()),
                ("org/model-GGUF".to_string(), "model.gguf".to_string()),
            ]
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn no_draft_file_means_one_job() {
        let dir = home("no-draft");
        std::fs::write(dir.join(".env"), "HF_REPO=\"org/m\"\nMODEL_FILE=\"m.gguf\"\n").expect("write");
        assert_eq!(default_jobs(&dir), vec![("org/m".to_string(), "m.gguf".to_string())]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn preset_jobs_are_the_sections_missing_files() {
        let dir = home("preset");
        std::fs::create_dir_all(dir.join("data/models")).expect("mkdir");
        std::fs::create_dir_all(dir.join("config")).expect("mkdir");
        std::fs::write(dir.join(".env"), "MODELS_PRESET=\"config/models.ini\"\n").expect("write");
        std::fs::write(
            dir.join("config/models.ini"),
            "; Download: a/a-GGUF -> a.gguf\n[a]\nmodel = a.gguf\n\
             ; Download: b/b-GGUF -> b.gguf\n[b]\nmodel = b.gguf\n",
        )
        .expect("write");
        // b.gguf is already here, so only a.gguf is fetched.
        std::fs::write(dir.join("data/models/b.gguf"), "x").expect("write");

        assert_eq!(
            preset_jobs(&dir, "a").unwrap(),
            vec![("a/a-GGUF".to_string(), "a.gguf".to_string())]
        );
        assert!(preset_jobs(&dir, "b").unwrap().is_empty(), "b.gguf is present");

        let err = preset_jobs(&dir, "nope").unwrap_err();
        assert!(err.contains("no \"; Download:\" lines for preset [nope]"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_preset_pull_needs_router_mode() {
        let dir = home("no-preset");
        std::fs::write(dir.join(".env"), "").expect("write");
        let err = preset_jobs(&dir, "a").unwrap_err();
        assert!(err.contains("MODELS_PRESET"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_suffix_is_appended_to_the_name() {
        assert_eq!(with_suffix(Path::new("/m/x.gguf"), ".part"), PathBuf::from("/m/x.gguf.part"));
        assert_eq!(
            with_suffix(Path::new("/m/x.gguf"), ".sha256"),
            PathBuf::from("/m/x.gguf.sha256")
        );
    }

    #[test]
    fn the_timestamp_is_the_shape_the_log_name_wants() {
        let stamp = timestamp();
        assert_eq!(stamp.len(), 15, "{stamp}");
        assert_eq!(stamp.as_bytes()[8], b'-', "{stamp}");
        assert!(stamp.starts_with("20"), "{stamp}");
    }
}
