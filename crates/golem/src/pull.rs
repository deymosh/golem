//! `golem pull [preset | owner/repo[:quant]]`: fetch a model from Hugging
//! Face, the one model source.
//!
//! Transport is `curl`, which ships with Windows 10+ and with every Linux and
//! macOS this repository supports, so the binary needs no TLS or HTTP crate
//! for an HTTPS-only host.
//!
//! Three shapes. With a preset name - or none, which is the default model
//! (`MODEL_ALIAS`) - it walks the preset file's `; Download:` comments and
//! fetches the files of that one section that `data/models` is missing; a
//! library name that is not a
//! preset yet is pulled from its repository. With a repository it asks the Hub
//! what is there, picks the quant this machine can hold (or the one named),
//! fetches it with its vision projector, and writes the preset section - the
//! `; Download:` lines included - so nothing about the model is typed by hand.
//! The decisions are `golem_core::hub`'s.

use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use golem_core::progress::Progress;
use golem_core::{hub, ini};
use golem_core::time::civil_from_days;

use crate::proc::{CURL_STALL, watch_progress};
use crate::{config, machine, state};

/// The free space the download refuses to start below, in GB - the biggest
/// preset model plus its `.part` wants a generous margin.
const MIN_FREE_GB: u64 = 25;

/// The completeness floor when Hugging Face does not report a size: any
/// bytes at all. curl runs with `-f`, so an error page is never saved, and a
/// `.part` becomes the file only when curl finished.
const FLOOR_FALLBACK: u64 = 1;

/// `golem pull [preset | owner/repo[:quant]]`.
pub fn run(home: &Path, args: &[String]) -> i32 {
    let mut out = std::io::stdout();
    let target = match parse_args(args) {
        Ok(target) => target,
        Err(message) => {
            let _ = writeln!(out, "[ERROR] {message}");
            return 1;
        }
    };
    let mut log = Log::open(home, &mut out);
    let result = match &target {
        Some(text) if text.contains('/') => match hub::parse_ref(text) {
            Ok(reference) => pull_ref(home, &reference, None, &mut log),
            Err(message) => Err(message),
        },
        // A library model that is not a preset yet: fetched from its repo
        // under the library's name.
        Some(name) if !is_preset(home, name) => {
            match crate::paths::library(home).into_iter().find(|e| &e.name == name) {
                Some(entry) => {
                    let reference = hub::Ref { repo: entry.repo, quant: entry.quant };
                    pull_ref(home, &reference, Some(name), &mut log)
                }
                None => run_inner(home, Some(name), &mut log),
            }
        }
        preset => run_inner(home, preset.as_deref(), &mut log),
    };
    match result {
        Ok(()) => 0,
        Err(message) => {
            log.error(&message);
            log.error(&format!("Model download failed. Log: {}", log.path.display()));
            1
        }
    }
}

/// Whether the served preset has a section `name`.
fn is_preset(home: &Path, name: &str) -> bool {
    ini::Document::read(&state::preset_path(home))
        .is_ok_and(|doc| doc.preset_names().iter().any(|n| n == name))
}

/// The preset, library model or repository to pull, or `None` for the default model. More
/// than one argument (or a flag) is a usage error.
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
        None => return Err("cannot read the free disk space".to_string()),
    }

    let models_dir = home.join("data/models");
    std::fs::create_dir_all(&models_dir)
        .map_err(|e| format!("cannot create {}: {e}", models_dir.display()))?;

    let token = config::get(home, "HF_TOKEN");

    // No name is the default model: the section clients get when they name
    // none.
    let alias = config::get(home, "MODEL_ALIAS").unwrap_or_default();
    let jobs = preset_jobs(home, preset.unwrap_or(&alias))?;

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

/// `golem pull owner/repo[:quant]`: ask the Hub, pick, fetch, write the
/// preset section and the provenance file. `name` is the preset's name when
/// the library gave one; otherwise it comes from the repository's.
///
/// The section goes to the base preset (`MODELS_PRESET`, or the repository's
/// own when that is the tuner's effective file) and, when the served preset
/// is the effective one, there too: the effective file is the base with this
/// machine's overlays merged in, and a new model has none yet. A preset that
/// already has the name is refused - pulling must not retune a model that is
/// already there - and nothing is downloaded before that is known.
fn pull_ref(home: &Path, reference: &hub::Ref, name: Option<&str>, log: &mut Log) -> Result<(), String> {
    log.section("MODEL FROM HUGGING FACE");
    if !have_tool("curl") {
        return Err("curl is required (present on every supported platform)".to_string());
    }
    let token = config::get(home, "HF_TOKEN");
    let repo_id = &reference.repo;

    log.info(&format!("asking huggingface.co about {repo_id}..."));
    let answer = curl_text(&format!("https://huggingface.co/api/models/{repo_id}?blobs=true"), token.as_deref())?;
    // The Hub answers a repository it will not show this caller - missing,
    // private or gated - with an authentication error, whatever the reason.
    let repo = hub::parse_repo(&answer).map_err(|e| {
        format!(
            "{repo_id}: the Hub answered \"{e}\" - check the name; a private or gated \
             repository needs HF_TOKEN in golem.toml"
        )
    })?;
    let quants = hub::quants(&repo);
    // A model whose GGUF does not say it is a mixture of experts may still be
    // one; the base model's config counts its experts.
    let moe = hub::is_moe(&repo)
        || repo.base_model.as_deref().is_some_and(|base| {
            curl_text(&format!("https://huggingface.co/{base}/resolve/main/config.json"), token.as_deref())
                .ok()
                .and_then(|t| hub::config_experts(&t))
                .is_some()
        });
    let params = repo.params.map(|p| format!("{:.1}B", p as f64 / 1e9)).unwrap_or_else(|| "size unknown".into());
    log.ok(&format!(
        "{} - {} {params} {} - trained context {}",
        repo.id,
        repo.arch.as_deref().unwrap_or("unknown architecture"),
        if moe { "mixture of experts" } else { "dense" },
        repo.ctx_train.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into()),
    ));

    let vram_mib = machine::gpu().map(|g| g.total_mib).unwrap_or(0);
    let ram_mib = machine::ram_total_gb() * 1024;
    let budget = hub::budget(moe, vram_mib, ram_mib);
    let (quant, fits) = hub::pick(&quants, reference.quant.as_deref(), budget)?;
    let gb = |bytes: u64| bytes as f64 / 1e9;
    let why = match (&reference.quant, fits) {
        (Some(_), _) => "as named".to_string(),
        (None, true) => format!("the largest of {} that fits {:.1} GB", quants.len(), gb(budget)),
        (None, false) => format!("the smallest of {}; none fits {:.1} GB", quants.len(), gb(budget)),
    };
    log.ok(&format!("quant {} ({:.1} GB, {why})", quant.label, gb(quant.size())));
    if !fits {
        log.warn(&format!(
            "{} is larger than this machine offers ({:.1} GB of VRAM and RAM for weights) - it may not load",
            quant.label,
            gb(budget)
        ));
    }
    let projector = hub::projector(&repo);
    if let Some(p) = projector {
        log.ok(&format!("vision: {} ({:.1} GB)", p.name(), gb(p.size)));
    }

    let name = name.map_or_else(|| hub::preset_name(&repo.id), str::to_string);
    let base = home.join(crate::tune::base_preset(home));
    let served = state::preset_path(home);
    for preset in [&base, &served] {
        let doc = ini::Document::read(preset).map_err(|e| format!("cannot read {}: {e}", preset.display()))?;
        if doc.preset_names().iter().any(|n| n == &name) {
            return Err(format!(
                "a preset [{name}] is already in {} - `golem pull {name}` fetches its files",
                preset.display()
            ));
        }
    }

    // Sampling from the base model the GGUF was made from; a repo without
    // one, or a base that is gated, leaves the preset's shared defaults.
    let sampling = match &repo.base_model {
        Some(base_model) => {
            let url = format!("https://huggingface.co/{base_model}/resolve/main/generation_config.json");
            let found = curl_text(&url, token.as_deref()).map(|t| hub::sampling(&t)).unwrap_or_default();
            if found.is_empty() {
                log.info(&format!("no sampling defaults from {base_model}; the preset's shared ones apply"));
            } else {
                let shown: Vec<String> = found.iter().map(|(k, v)| format!("{k} {v}")).collect();
                log.ok(&format!("sampling from {base_model}: {}", shown.join(", ")));
            }
            found
        }
        None => Vec::new(),
    };

    let files: Vec<&hub::File> = quant.files.iter().chain(projector).collect();
    let need = files.iter().map(|f| f.size).sum::<u64>();
    let need_gb = need.div_ceil(1_000_000_000) + 2;
    match free_gb(home) {
        Some(free) if free >= need_gb => log.ok(&format!("disk: {free} GB free, {need_gb} GB needed")),
        Some(free) => return Err(format!("{free} GB free on disk, {need_gb} GB needed")),
        None => log.warn("cannot read free disk space; downloading anyway"),
    }

    let models_dir = home.join("data/models");
    std::fs::create_dir_all(&models_dir).map_err(|e| format!("cannot create {}: {e}", models_dir.display()))?;
    for file in &files {
        hf_get(log, &models_dir, &repo.id, &file.path, token.as_deref())?;
    }

    let provenance = models_dir.join(format!("{}.json", quant.files[0].name()));
    if let Err(e) = std::fs::write(&provenance, provenance_json(&repo, quant, projector)) {
        log.warn(&format!("cannot write {}: {e}", provenance.display()));
    }

    // A file that carries its own MTP head drafts for itself: speculative
    // decoding is switched on, and the tuner measures it against off.
    let mut keys = sampling;
    let mtp = golem_core::gguf::read(&models_dir.join(quant.files[0].name())).is_ok_and(|f| f.mtp_tensors > 0);
    if mtp {
        log.ok("the file carries an MTP head: speculative decoding on (the tuner measures it against off)");
        keys.insert(0, ("spec-type", "draft-mtp".to_string()));
    }
    let block = format!("\n{}", hub::section(&name, &repo, quant, projector, &keys));
    let mut written: Vec<&PathBuf> = vec![&base];
    if served != base {
        written.push(&served);
    }
    for preset in written {
        let doc = ini::Document::read(preset).map_err(|e| format!("cannot read {}: {e}", preset.display()))?;
        std::fs::write(preset, doc.append(&block)).map_err(|e| format!("cannot write {}: {e}", preset.display()))?;
        log.ok(&format!("preset [{name}] written to {}", preset.strip_prefix(home).unwrap_or(preset).display()));
    }
    log.line("");
    log.ok(&format!("next: golem tune {name}   (finds the split and context for this machine)"));
    Ok(())
}

/// Where a pulled model came from, beside it in `data/models`: enough to
/// find the same bytes again and to tell when the repository moved on.
fn provenance_json(repo: &hub::Repo, quant: &hub::Quant, projector: Option<&hub::File>) -> String {
    use golem_core::json::quote;
    let files: Vec<String> = quant.files.iter().chain(projector).map(|f| quote(&f.path)).collect();
    let optional = |v: &Option<String>| v.as_deref().map(quote).unwrap_or_else(|| "null".into());
    format!(
        "{{\n  \"repo\": {},\n  \"revision\": {},\n  \"quant\": {},\n  \"files\": [{}],\n  \"bytes\": {},\n  \
         \"base_model\": {},\n  \"license\": {},\n  \"pulled\": {}\n}}\n",
        quote(&repo.id),
        quote(&repo.sha),
        quote(&quant.label),
        files.join(", "),
        quant.size() + projector.map_or(0, |p| p.size),
        optional(&repo.base_model),
        optional(&repo.license),
        quote(&golem_core::time::now_iso()),
    )
}

/// The body of a small HTTPS GET (`curl -sL`), the Hub's API and config files.
fn curl_text(url: &str, token: Option<&str>) -> Result<String, String> {
    let mut command = Command::new("curl");
    command.args(["-sL", "--retry", "3"]);
    if let Some(token) = token {
        command.arg("-H").arg(format!("Authorization: Bearer {token}"));
    }
    let out = command
        .arg(url)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("curl failed for {url} (exit {:?})", out.status.code()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Fetch these `(repo, file)` pairs into `data/models`, with one log for the
/// whole run.
///
/// `up`'s half of the downloader, and deliberately the same bytes: `golem
/// pull` decides *what* to fetch and prints its banner, `up` decides what the
/// preset needs before anything may start and fetches only that. A second
/// download path would be a second completeness rule, and the two would
/// disagree about a model the day one of them changed.
pub fn fetch_all(home: &Path, jobs: &[(String, String)], out: &mut dyn Write) -> Result<(), String> {
    let models_dir = home.join("data/models");
    std::fs::create_dir_all(&models_dir)
        .map_err(|e| format!("cannot create {}: {e}", models_dir.display()))?;
    let token = config::get(home, "HF_TOKEN");
    let mut log = Log::open(home, out);
    for (repo, file) in jobs {
        hf_get(&mut log, &models_dir, repo, file, token.as_deref())?;
    }
    Ok(())
}

/// What the served preset's section `name` still lacks, and the bytes Hugging
/// Face says it adds up to (`None` when it does not say for every file).
/// `None` for a name that is not a section with `; Download:` lines, or one
/// with every file here.
pub fn missing(home: &Path, name: &str) -> Option<(Vec<(String, String)>, Option<u64>)> {
    let jobs = preset_jobs(home, name).ok().filter(|jobs| !jobs.is_empty())?;
    let token = config::get(home, "HF_TOKEN");
    let sizes: Option<Vec<u64>> = jobs
        .iter()
        .map(|(repo, file)| linked_size(&format!("https://huggingface.co/{repo}/resolve/main/{file}"), token.as_deref()))
        .collect();
    Some((jobs, sizes.map(|s| s.iter().sum())))
}

/// The `(repo, file)` pairs for one preset section that `data/models` lacks.
///
/// The preset file is the served one ([`config::models_preset`]), and a
/// section with no `; Download:` comment at all is an error that names it.
pub fn preset_jobs(home: &Path, name: &str) -> Result<Vec<(String, String)>, String> {
    let preset = config::models_preset(home, &config::Overrides::default());
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
        // Already here: only what is missing is fetched.
        if models_dir.join(local_name(&file)).is_file() {
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
/// answer to "which section owns this comment". The rule: a
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

/// The name a repository path is stored under in `data/models`: its last
/// part, so a split model's shards land side by side.
fn local_name(path: &str) -> &str {
    hub::stored_name(path)
}

/// Fetch one GGUF into `models_dir`, resuming a leftover `.part`.
///
/// `path` is the file's path in the repository; it is stored under its last
/// part. Completeness is judged against the size Hugging Face reports
/// (`x-linked-size`), to the byte; when the server does not say, a file curl
/// finished is taken as it is.
fn hf_get(
    log: &mut Log,
    models_dir: &Path,
    repo: &str,
    path_in_repo: &str,
    token: Option<&str>,
) -> Result<(), String> {
    let file = local_name(path_in_repo);
    if !hub::safe_name(file) {
        return Err(format!("{path_in_repo:?} does not name a file golem can store"));
    }
    let path = models_dir.join(file);
    let url = format!("https://huggingface.co/{repo}/resolve/main/{path_in_repo}");
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
    let status = curl_download(log, &url, &part, token, expected.map(|size| (file, size)))?;
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

    match hash_file(log, &path, file, size) {
        Ok(line) => {
            let _ = std::fs::write(with_suffix(&path, ".sha256"), &line);
            log.line(line.trim_end());
        }
        Err(e) => log.warn(&format!("cannot hash {file}: {e}")),
    }
    Ok(())
}

/// The `.sha256` line for a downloaded file, hashed in process.
///
/// The bare name keeps the line portable - `<hash>  <file>`, checkable with
/// `sha256sum -c` from that folder. A 20 GB model takes a minute or more,
/// so the hash says how far it has got, like the download before it.
fn hash_file(log: &mut Log, path: &Path, file: &str, size: u64) -> Result<String, String> {
    let label = format!("sha256: {file}");
    let mut said = Instant::now();
    let hash = golem_core::sha256::file(path, &mut |done| {
        if said.elapsed() >= crate::proc::PROGRESS_EVERY {
            log.progress(&Progress::bytes(done.min(size), size, &label));
            said = Instant::now();
        }
    })
    .map_err(|e| e.to_string());
    log.progress_done();
    Ok(golem_core::sha256::line(&hash?, file))
}

/// The size a file must reach to count as complete: what Hugging Face
/// reports, to the byte (a GGUF short of its end does not load), or the
/// fallback when the header is absent or says nothing.
fn floor_of(expected: Option<u64>) -> u64 {
    match expected {
        Some(expected) if expected > 0 => expected,
        _ => FLOOR_FALLBACK,
    }
}

/// The `x-linked-size` of `url`, from `curl -sIL`; when more than one hop of
/// the redirects reports one, the last wins.
fn linked_size(url: &str, token: Option<&str>) -> Option<u64> {
    let mut command = Command::new("curl");
    // A deadline: the API front asks this while a client waits for its 503.
    command.args(["-sIL", "--max-time", "10"]);
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

/// `curl -fL -C - --retry 5 --retry-delay 3 <stall limits> -o <part> <url>`.
///
/// The stall limits ([`CURL_STALL`]) end a connection that has gone quiet,
/// and `--retry` has another go at what they cut short. On a terminal curl
/// draws its own meter on the inherited stderr. Anywhere else - the job log
/// the console and the screen tail - its meter would be carriage-return
/// noise, so curl runs quiet and the download says how far it has got as
/// `PROGRESS` lines instead: the `.part` file's size against the size the
/// Hub reported (`sized`, the file's name and its bytes), every
/// [`crate::proc::PROGRESS_EVERY`]. The exit status is returned for the
/// caller to judge.
fn curl_download(
    log: &mut Log,
    url: &str,
    part: &Path,
    token: Option<&str>,
    sized: Option<(&str, u64)>,
) -> Result<std::process::ExitStatus, String> {
    let quiet = !std::io::stderr().is_terminal();
    let mut command = Command::new("curl");
    command.args(["-fL", "-C", "-", "--retry", "5", "--retry-delay", "3"]);
    command.args(CURL_STALL);
    if quiet {
        // Silent, but errors still said.
        command.arg("-sS");
    }
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
    let Some((file, total)) = sized.filter(|_| quiet) else {
        return command.status().map_err(|e| format!("cannot run curl: {e}"));
    };
    let child = command.spawn().map_err(|e| format!("cannot run curl: {e}"))?;
    let label = format!("pull: {file}");
    let status = watch_progress(child, part, total, &label, &mut |progress| log.progress(&progress))
        .map_err(|e| format!("cannot wait for curl: {e}"))?;
    // The bar goes: the next line says how the download ended.
    log.progress_done();
    Ok(status)
}

/// Free space in GB on the filesystem holding `home`.
///
/// Windows has no `df` outside a Git or MSYS shell - and the console and the
/// screen run this as a child with no such shell - so there it is .NET's own
/// reading of the drive; elsewhere `df -Pk`.
fn free_gb(home: &Path) -> Option<u64> {
    if cfg!(windows) {
        // Single quotes in a PowerShell string are doubled, nothing else.
        let path = home.display().to_string().replace('\'', "''");
        let script = format!(
            "[System.IO.DriveInfo]::new([System.IO.Path]::GetPathRoot([System.IO.Path]::GetFullPath('{path}'))).AvailableFreeSpace"
        );
        let bytes: u64 = crate::machine::powershell(&script)?.parse().ok()?;
        return Some(bytes / 1_000_000_000);
    }
    let out = Command::new("df").arg("-Pk").arg(home).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // `Filesystem 1024-blocks Used Available Capacity Mounted on`; the fourth
    // field of the second line is the free space.
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

/// `YYYYmmdd-HHMMSS`, the stamp on the pull's log file.
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

/// Everything printed also lands in `logs/download-model-<stamp>.log`.
struct Log<'a> {
    path: PathBuf,
    file: Option<File>,
    out: &'a mut dyn Write,
}

impl<'a> Log<'a> {
    fn open(home: &Path, out: &'a mut dyn Write) -> Log<'a> {
        let dir = home.join("logs");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("download-model-{}.log", timestamp()));
        let file = File::create(&path).ok();
        Log { path, file, out }
    }

    /// A normal line: to the sink, and to the log.
    fn line(&mut self, text: &str) {
        let _ = writeln!(self.out, "{text}");
        self.write(text);
    }

    /// An error line: to the sink, and to the log. The sink cannot tell
    /// stdout from stderr, so the `[ERROR] ` prefix is what keeps the text
    /// the same as the `eprintln!` it replaced.
    fn error(&mut self, text: &str) {
        let _ = writeln!(self.out, "[ERROR] {text}");
        self.write(&format!("[ERROR] {text}"));
    }

    /// A `PROGRESS` line, to the sink only: the download log keeps what
    /// happened, not how fast.
    fn progress(&mut self, progress: &Progress) {
        let _ = writeln!(self.out, "{}", progress.line());
    }

    fn progress_done(&mut self) {
        let _ = writeln!(self.out, "{}", golem_core::progress::CLEAR);
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
        // header's - they wait for the next header.
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
        // header.
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
    fn the_floor_is_the_expected_size() {
        assert_eq!(floor_of(Some(1000)), 1000);
        assert_eq!(floor_of(Some(2_000_000_000)), 2_000_000_000);
        assert_eq!(floor_of(None), FLOOR_FALLBACK);
        assert_eq!(floor_of(Some(0)), FLOOR_FALLBACK);
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
    fn preset_jobs_are_the_sections_missing_files() {
        let dir = home("preset");
        std::fs::create_dir_all(dir.join("data/models")).expect("mkdir");
        std::fs::create_dir_all(dir.join("config")).expect("mkdir");
        std::fs::write(dir.join("golem.toml"), "MODELS_PRESET=\"config/models.ini\"\n").expect("write");
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
    fn with_no_preset_named_the_shipped_one_is_read() {
        let dir = home("no-preset");
        std::fs::write(dir.join("golem.toml"), "").expect("write");
        // No config/models.ini either: the error names the file it read.
        let err = preset_jobs(&dir, "a").unwrap_err();
        assert!(err.contains("config/models.ini"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_provenance_file_is_json_naming_the_files_and_revision() {
        let repo = hub::Repo {
            id: "org/m-GGUF".into(),
            sha: "abc123".into(),
            files: Vec::new(),
            arch: None,
            ctx_train: None,
            params: None,
            base_model: Some("org/m".into()),
            license: None,
        };
        let file = |path: &str, size| hub::File { path: path.into(), size };
        let quant = hub::Quant { label: "Q4_K_M".into(), files: vec![file("m-Q4_K_M.gguf", 10)] };
        let projector = file("mmproj-F16.gguf", 2);
        let text = provenance_json(&repo, &quant, Some(&projector));
        let doc = golem_core::json::parse(&text).expect("valid JSON");
        assert_eq!(doc.get("revision").and_then(|v| v.as_str()), Some("abc123"));
        assert_eq!(doc.get("bytes").and_then(|v| v.as_f64()), Some(12.0));
        assert!(doc.get("license").is_some_and(|v| v.is_null()));
        assert!(text.contains("\"mmproj-F16.gguf\""), "{text}");
    }

    #[test]
    fn a_shard_in_a_subdirectory_is_stored_by_its_name() {
        assert_eq!(local_name("BF16/m-BF16-00001-of-00002.gguf"), "m-BF16-00001-of-00002.gguf");
        assert_eq!(local_name("m.gguf"), "m.gguf");
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
