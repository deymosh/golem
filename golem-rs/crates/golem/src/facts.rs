//! Gathering what a row says about the machine and the run.
//!
//! The half of `scripts/record-result.sh` that talks to the machine:
//! nvidia-smi, the engine's own `/props`, the server process tree's working
//! set, and whether anything is competing for RAM. [`record`](golem_core::record)
//! then turns the answer plus the captured output into one line.
//!
//! Everything here is best effort, exactly as the script is: a field that
//! cannot be read becomes `None` and the row carries `null`, because losing
//! one fact is not a reason to lose the measurement. Nothing here panics and
//! nothing here blocks for long: the HTTP probe is the engine module's own
//! two-second one, and every other probe is a single short-lived command.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use golem_core::record::{self, Facts, Flag, FlagValue};
use golem_core::results::Kind;
use golem_core::time;

use crate::{engine, state};

/// Everything a row needs, read from `home` and the running machine.
pub fn collect(
    home: &Path,
    kind: Kind,
    engine: &str,
    model: &str,
    preset_rel: &str,
    note: &str,
) -> Facts {
    // The host: an anonymous hardware slug, never the hostname.
    let (gpu, vram_total_mib, vram_mib) = gpu_facts();
    let cpu = cpu_name().unwrap_or_default();
    let host_slug = host_slug(&gpu, vram_total_mib, ram_total_gb(), &cpu);

    // What the engine says about itself and the file it serves.
    let props = engine_props(home, model);
    let engine_build = props
        .as_deref()
        .and_then(|body| props_string(body, "build_info"))
        .filter(|build| !build.is_empty())
        .or_else(|| build_from_pid_file(home, engine))
        .unwrap_or_else(|| "unknown".to_string());
    let model_file = props
        .as_deref()
        .and_then(|body| props_string(body, "model_path"))
        .map(|path| basename(&path).to_string())
        .filter(|file| !file.is_empty())
        .or_else(|| state::env_value(home, "MODEL_FILE"))
        .unwrap_or_default();

    // The preset the run actually served, when it names a file that is there.
    let preset_text = preset_text(home, preset_rel);
    let ctx: Option<u64> = preset_text
        .as_deref()
        .and_then(|text| record::ctx_of(text, model))
        .or_else(|| env_parse(home, "CONTEXT_SIZE"));
    let threads: Option<u32> = preset_text
        .as_deref()
        .and_then(|text| record::threads_of(text, model))
        .or_else(|| env_parse(home, "CPU_THREADS"));
    let flags = match &preset_text {
        Some(text) => record::flags_of(text, model),
        None => env_flags(home),
    };

    // The GGUF's own facts, when the file can be found under the home.
    let model_facts = model_path(home, &model_file)
        .map(|path| record::model_facts(&path))
        .unwrap_or_default();

    Facts {
        kind,
        ts: time::now_iso(),
        host_slug,
        gpu,
        os: os_name(),
        mode: state::override_value(home, "GOLEM_DEPLOY_MODE", "DEPLOY_MODE")
            .unwrap_or_else(|| "native".to_string()),
        engine: engine.to_string(),
        engine_build,
        model: model.to_string(),
        model_file,
        ctx,
        threads,
        params_b: model_facts.params_b,
        params_active_b: model_facts.params_active_b,
        moe: model_facts.moe,
        experts: model_facts.experts,
        n_layer: model_facts.n_layer,
        ctx_train: model_facts.ctx_train,
        flags,
        vram_mib,
        ram_used_gib: ram_used_gib(home),
        ram_free_gib: state::ram_free_gb(),
        quiet: quiet(),
        note: note.to_string(),
    }
}

// ---- host ----------------------------------------------------------------

/// The anonymous hardware slug a row carries:
/// `<slug(gpu)>_<vram_total_gib>g_<ram_total_gb>g_<slug(cpu)>`.
///
/// `scripts/record-result.sh` line 93, and the same name `scripts/tune.sh`
/// files its output under. With no NVIDIA card the GPU is `unknown` and the
/// VRAM total is 0, so the slug is still a slug - e.g.
/// `unknown_0g_32g_i7-12700kf`.
fn host_slug(gpu: &str, vram_total_mib: u64, ram_total_gb: u64, cpu: &str) -> String {
    format!(
        "{}_{}g_{}g_{}",
        record::slug(gpu),
        vram_total_mib / 1024,
        ram_total_gb,
        record::slug(cpu)
    )
}

/// The GPU name, its total VRAM and its used VRAM in MiB, from nvidia-smi.
///
/// `unknown` / `0` / `None` when there is no NVIDIA card or no nvidia-smi -
/// the script's own fallbacks, and a row with no GPU is still a row.
fn gpu_facts() -> (String, u64, Option<u64>) {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=name,memory.total,memory.used", "--format=csv,noheader,nounits"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return ("unknown".to_string(), 0, None) };
    if !out.status.success() {
        return ("unknown".to_string(), 0, None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let Some(line) = text.lines().next() else { return ("unknown".to_string(), 0, None) };
    let mut fields = line.split(',').map(str::trim);
    let name = fields.next().unwrap_or("").trim();
    let total: u64 = fields.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let used: Option<u64> = fields.next().and_then(|v| v.parse().ok());
    let name = if name.is_empty() { "unknown" } else { name };
    (name.to_string(), total, used)
}

/// `windows` / `macos` / `linux`, the script's `uname -s` mapping.
fn os_name() -> String {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
    .to_string()
}

/// The CPU's marketing name: `/proc/cpuinfo` where it is a real file, else
/// `Win32_Processor.Name`. Mirrors `tune::cpu_name`.
fn cpu_name() -> Option<String> {
    if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("model name") {
                let name = rest.trim_start_matches(':').trim();
                if !name.is_empty() {
                    return Some(name.to_string());
                }
            }
        }
    }
    powershell("(Get-CimInstance Win32_Processor).Name")
}

/// Total system RAM in whole GB: `/proc/meminfo` where it is real, else the
/// Windows counter. Mirrors `tune::ram_total_gb`.
fn ram_total_gb() -> u64 {
    if let Ok(text) = std::fs::read_to_string("/proc/meminfo") {
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("MemTotal:") {
                if let Some(kb) = rest.split_whitespace().next().and_then(|v| v.parse::<f64>().ok()) {
                    return (kb / 1_048_576.0).round() as u64;
                }
            }
        }
    }
    powershell("[math]::Round((Get-CimInstance Win32_OperatingSystem).TotalVisibleMemorySize/1MB,0)")
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// One PowerShell one-liner's first line, best effort.
///
/// PowerShell prints numbers in the machine's own locale, where this box's
/// separator is a comma - which would turn every number this reads into
/// nonsense, so the comma is forced back to a dot.
fn powershell(script: &str) -> Option<String> {
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().replace(',', ".").replace('\r', "");
    let first = text.lines().next().unwrap_or("").trim().to_string();
    (!first.is_empty()).then_some(first)
}

// ---- the engine's own answer ---------------------------------------------

/// `GET /props?model=<model>`, the body only. `None` when nothing answers.
fn engine_props(home: &Path, model: &str) -> Option<String> {
    let endpoint = engine::Endpoint::from_home(home);
    engine::get(&endpoint, &format!("/props?model={model}")).map(|(_, body)| body)
}

/// A string member of a `/props` body.
///
/// The body is JSON and is read as such; a body that will not parse is
/// scanned for the `"key":"value"` the shell's `grep -o` would have taken.
fn props_string(body: &str, key: &str) -> Option<String> {
    if let Ok(value) = golem_core::json::parse(body) {
        return value.get(key).and_then(|v| v.as_str()).map(str::to_string);
    }
    naive_props_string(body, key)
}

/// `"key":"value"` in `body`, tolerating spaces around the colon - used only
/// when the body is not valid JSON.
fn naive_props_string(body: &str, key: &str) -> Option<String> {
    let needle = format!("\"{key}\"");
    let mut rest = body;
    while let Some(at) = rest.find(&needle) {
        let after = rest[at + needle.len()..].trim_start();
        if let Some(after) = after.strip_prefix(':').map(str::trim_start) {
            if let Some(after) = after.strip_prefix('"') {
                let value: String = after.chars().take_while(|&c| c != '"').collect();
                return Some(value);
            }
        }
        rest = &rest[at + needle.len()..];
    }
    None
}

/// The basename of a path with either separator: `${p##*[/\\]}`.
fn basename(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The engine build, taken from the running server's own install path.
///
/// Some servers (ik_llama.cpp) report no build in `/props`; their command
/// line names it instead, as `<home>/.local/engines/<engine>/<build>` with a
/// `-cuda`/`-rocm` suffix. The pid comes from the launcher's pid file.
fn build_from_pid_file(home: &Path, engine_name: &str) -> Option<String> {
    let pid = engine::pid(home)?;
    let cmdline = if cfg!(windows) { windows_cmdline(pid) } else { unix_cmdline(pid) };
    build_from_cmdline(&cmdline, engine_name)
}

/// The build a command line names for `engine`, or `None`.
fn build_from_cmdline(cmdline: &str, engine: &str) -> Option<String> {
    cmdline.split_whitespace().find_map(|token| build_from_token(token, engine))
}

/// One whitespace token of a command line: `<...>engines[/\]<engine>[/\]<build>`.
fn build_from_token(token: &str, engine: &str) -> Option<String> {
    let rest = engines_rest(token)?;
    let mut segments = rest.split(['/', '\\']);
    if segments.next()? != engine {
        return None;
    }
    let mut build = segments.next()?;
    if let Some(stripped) = build.strip_suffix("-cuda") {
        build = stripped;
    }
    if let Some(stripped) = build.strip_suffix("-rocm") {
        build = stripped;
    }
    Some(build.to_string())
}

/// Everything after the first `engines/` or `engines\` in `token`.
fn engines_rest(token: &str) -> Option<&str> {
    let bytes = token.as_bytes();
    let mut start = 0;
    while let Some(at) = token[start..].find("engines") {
        let end = start + at + "engines".len();
        match bytes.get(end) {
            Some(b'/') | Some(b'\\') => return Some(&token[end + 1..]),
            _ => start = end,
        }
    }
    None
}

/// The running server's command line plus executable path, from `Win32_Process`.
fn windows_cmdline(pid: u32) -> String {
    let script = format!(
        "(Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').CommandLine; \
         (Get-CimInstance Win32_Process -Filter 'ProcessId={pid}').ExecutablePath"
    );
    match Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
    {
        Ok(out) => String::from_utf8_lossy(&out.stdout).replace('\r', ""),
        Err(_) => String::new(),
    }
}

/// `/proc/<pid>/cmdline` (nul-separated) plus the resolved `/proc/<pid>/exe`.
fn unix_cmdline(pid: u32) -> String {
    let mut text = String::new();
    if let Ok(bytes) = std::fs::read(format!("/proc/{pid}/cmdline")) {
        text.push_str(&String::from_utf8_lossy(&bytes).replace('\0', " "));
    }
    if let Ok(exe) = std::fs::read_link(format!("/proc/{pid}/exe")) {
        text.push(' ');
        text.push_str(&exe.to_string_lossy());
    }
    text
}

// ---- the preset and the model file ---------------------------------------

/// The preset file's text, when `preset_rel` names a file that exists.
fn preset_text(home: &Path, preset_rel: &str) -> Option<String> {
    if preset_rel.is_empty() {
        return None;
    }
    let path = home.join(preset_rel);
    if !path.is_file() {
        return None;
    }
    std::fs::read_to_string(&path).ok()
}

/// A number from the home's `.env`, or `None` when it is absent or unreadable.
fn env_parse<T: std::str::FromStr>(home: &Path, key: &str) -> Option<T> {
    state::env_value(home, key).and_then(|value| value.parse().ok())
}

/// The single-model `.env` flags: the context and the extra arguments.
///
/// The script writes `{"ctx-size": <CONTEXT_SIZE or null>, "extra": "..."}`
/// with `json_num` and `json_str` directly, so an absent `CONTEXT_SIZE` is
/// the bare `null` and `extra` is quoted however numeric it looks - neither
/// is the preset branch's numeric-or-quoted rule. That is what the two
/// [`FlagValue`] variants are for.
fn env_flags(home: &Path) -> Vec<Flag> {
    let ctx = FlagValue::Number(state::env_value(home, "CONTEXT_SIZE").unwrap_or_default());
    let extra = state::env_value(home, "EXTRA_ARGS").unwrap_or_default();
    vec![("ctx-size".to_string(), ctx), ("extra".to_string(), FlagValue::Text(extra))]
}

/// The GGUF the run served, looked up under `data/models/` and then under
/// Strata's own per-model directory - the script's two candidates.
fn model_path(home: &Path, model_file: &str) -> Option<PathBuf> {
    if model_file.is_empty() {
        return None;
    }
    let direct = home.join("data/models").join(model_file);
    if direct.is_file() {
        return Some(direct);
    }
    let base = home.join(".local/engines/strata/data/models");
    let entries = std::fs::read_dir(&base).ok()?;
    for entry in entries.flatten() {
        let candidate = entry.path().join(model_file);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// ---- the process tree and the noise --------------------------------------

/// The working set of the server process tree rooted at the pid file's pid,
/// in GiB to one decimal. `None` when there is no pid file.
fn ram_used_gib(home: &Path) -> Option<f64> {
    let pid = engine::pid(home)?;
    if cfg!(windows) {
        ram_used_gib_windows(pid)
    } else {
        ram_used_gib_unix(pid)
    }
}

/// `WorkingSet64` of the process and its descendants, summed and rounded.
///
/// The router spawns one child per loaded model and Strata runs its engine as
/// a child, so the tree - not the one process - is what holds the RAM.
fn ram_used_gib_windows(pid: u32) -> Option<f64> {
    const SCRIPT: &str = r#"
$ids = @([int]$env:SRV_PID); $all = @()
while ($ids.Count) {
    $n = $ids[0]; $ids = @($ids | Select-Object -Skip 1)
    if (Get-Process -Id $n -ErrorAction SilentlyContinue) { $all += $n }
    $ids += @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$n" |
              Select-Object -ExpandProperty ProcessId)
}
[math]::Round(($all | ForEach-Object { (Get-Process -Id $_).WorkingSet64 } |
               Measure-Object -Sum).Sum / 1GB, 1)
"#;
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", SCRIPT])
        .env("SRV_PID", pid.to_string())
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().replace(',', ".").replace('\r', "");
    text.lines().next()?.trim().parse().ok()
}

/// The same sum from `ps`'s RSS (in KB) over the process tree.
///
/// A tree that yields nothing reads as 0.0, which is what the shell's awk
/// prints - the pid file said something was there.
fn ram_used_gib_unix(pid: u32) -> Option<f64> {
    let kb = rss_tree_kb(pid);
    Some((kb as f64 / 1_048_576.0 * 10.0).round() / 10.0)
}

/// RSS of `pid` and all of its descendants, in KB.
fn rss_tree_kb(pid: u32) -> u64 {
    let mut total = rss_kb(pid);
    for child in children(pid) {
        total += rss_tree_kb(child);
    }
    total
}

fn rss_kb(pid: u32) -> u64 {
    let out = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return 0 };
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

fn children(pid: u32) -> Vec<u32> {
    let out = Command::new("pgrep")
        .args(["-P", &pid.to_string()])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return Vec::new() };
    String::from_utf8_lossy(&out.stdout).lines().filter_map(|line| line.trim().parse().ok()).collect()
}

/// Quiet = no download or build running; they starve RAM and cost 5-10 t/s.
fn quiet() -> bool {
    if cfg!(windows) {
        !windows_busy()
    } else {
        !unix_busy()
    }
}

/// `tasklist` for the download/build tools the script names. A missing
/// `tasklist` is "nothing running".
fn windows_busy() -> bool {
    const NAMES: [&str; 7] = ["curl", "wget", "hf", "huggingface-cli", "cmake", "msbuild", "ninja"];
    let out = Command::new("tasklist").stdin(Stdio::null()).stderr(Stdio::null()).output();
    let Ok(out) = out else { return false };
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let lower = line.to_lowercase();
        if NAMES.iter().any(|name| lower.starts_with(&format!("{name}.exe"))) {
            return true;
        }
    }
    false
}

/// `pgrep -x` for the same tools (with `make`, which the shell also watches
/// on this side). A missing `pgrep` is "nothing running".
fn unix_busy() -> bool {
    const PATTERN: &str = "curl|wget|hf|huggingface-cli|cmake|ninja|make";
    let out = Command::new("pgrep")
        .args(["-x", PATTERN])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    let Ok(out) = out else { return false };
    out.status.success() && !out.stdout.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-facts-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        dir
    }

    #[test]
    fn the_host_slug_is_gpu_vram_ram_cpu() {
        assert_eq!(
            host_slug(
                "NVIDIA GeForce RTX 3060",
                12288,
                32,
                "12th Gen Intel(R) Core(TM) i7-12700KF"
            ),
            "rtx-3060_12g_32g_i7-12700kf"
        );
        // No NVIDIA card: the GPU is `unknown` and the total is 0, and the
        // slug is still a slug.
        assert_eq!(
            host_slug("unknown", 0, 32, "12th Gen Intel(R) Core(TM) i7-12700KF"),
            "unknown_0g_32g_i7-12700kf"
        );
        // The VRAM total is whole GiB, as the shell's `$((VRAM_TOTAL / 1024))`.
        assert_eq!(
            host_slug("NVIDIA GeForce RTX 4090", 24564, 64, "AMD Ryzen 9 5950X"),
            "rtx-4090_23g_64g_ryzen-9-5950x"
        );
    }

    #[test]
    fn with_no_preset_the_flags_are_the_env_fallback() {
        let dir = home("env-flags");
        std::fs::write(
            dir.join(".env"),
            "CONTEXT_SIZE=\"249344\"\nEXTRA_ARGS=\"-fa on --no-mmap\"\n",
        )
        .expect("write");
        let flags = env_flags(&dir);
        assert_eq!(
            flags,
            vec![
                ("ctx-size".to_string(), FlagValue::Number("249344".to_string())),
                ("extra".to_string(), FlagValue::Text("-fa on --no-mmap".to_string())),
            ]
        );
        // And the row renders them the shell's way: a bare number, a quoted
        // string.
        let facts = Facts { flags, ..Facts::default() };
        let line = record::row(&facts, "RESULT decode_best=1\n").expect("record");
        assert!(
            line.contains(r#""flags": {"ctx-size": 249344, "extra": "-fa on --no-mmap"}"#),
            "{line}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_env_fallback_keeps_the_keys_when_the_env_is_bare() {
        let dir = home("env-flags-missing");
        // Both keys are still there rather than absent, and the row is the
        // shell's: an unset `CONTEXT_SIZE` is a bare `null`, and `extra` is
        // quoted even though it is empty.
        let flags = env_flags(&dir);
        assert_eq!(
            flags,
            vec![
                ("ctx-size".to_string(), FlagValue::Number(String::new())),
                ("extra".to_string(), FlagValue::Text(String::new())),
            ]
        );
        let facts = Facts { flags, ..Facts::default() };
        let line = record::row(&facts, "RESULT decode_best=1\n").expect("record");
        assert!(line.contains(r#""flags": {"ctx-size": null, "extra": ""}"#), "{line}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_props_body_gives_up_its_build_and_model_path() {
        let body = r#"{"build_info":"b11379-1537a0a8b","model_path":"C:\\data\\models\\Ornith-1.5-35B-Q4_K_M.gguf","total_slots":1}"#;
        assert_eq!(props_string(body, "build_info").as_deref(), Some("b11379-1537a0a8b"));
        assert_eq!(
            props_string(body, "model_path").as_deref(),
            Some(r"C:\data\models\Ornith-1.5-35B-Q4_K_M.gguf")
        );
        assert_eq!(props_string(body, "nope"), None);
        // An engine that reports no build: the field is there and empty.
        assert_eq!(props_string(r#"{"build_info":""}"#, "build_info").as_deref(), Some(""));
        // Spacing around the colon is JSON's business, not the reader's.
        assert_eq!(props_string(r#"{ "build_info" : "b9000" }"#, "build_info").as_deref(), Some("b9000"));
    }

    #[test]
    fn a_body_that_is_not_json_falls_back_to_a_scan() {
        // A trailing comma: the shell's grep reads it anyway, so this does.
        let body = r#"{"build_info":"b9000",}"#;
        assert_eq!(props_string(body, "build_info").as_deref(), Some("b9000"));
        assert_eq!(naive_props_string(body, "absent"), None);
    }

    #[test]
    fn the_model_file_is_the_basename_of_the_served_path() {
        assert_eq!(basename(r"C:\data\models\a.gguf"), "a.gguf");
        assert_eq!(basename("/data/models/b.gguf"), "b.gguf");
        assert_eq!(basename("c.gguf"), "c.gguf");
        assert_eq!(basename(""), "");
    }

    #[test]
    fn the_build_comes_out_of_the_engines_install_path() {
        let windows = r"C:\golem\.local\engines\llama.cpp\b11379-cuda\llama-server.exe --port 8000";
        assert_eq!(build_from_cmdline(windows, "llama.cpp").as_deref(), Some("b11379"));
        let unix = "/home/x/.local/engines/llama.cpp/b4400-rocm/llama-server -m a.gguf";
        assert_eq!(build_from_cmdline(unix, "llama.cpp").as_deref(), Some("b4400"));
        // The path must be the named engine's.
        assert_eq!(build_from_cmdline(windows, "ik_llama.cpp"), None);
        // No engines path at all: nothing to take.
        assert_eq!(build_from_cmdline("llama-server -m a.gguf", "llama.cpp"), None);
    }

    #[test]
    fn a_preset_is_read_only_when_it_names_an_existing_file() {
        let dir = home("preset");
        std::fs::create_dir_all(dir.join("config")).expect("dir");
        std::fs::write(dir.join("config/models.ini"), "[a]\nmodel = a.gguf\n").expect("write");
        assert!(preset_text(&dir, "config/models.ini").is_some());
        assert_eq!(preset_text(&dir, ""), None);
        assert_eq!(preset_text(&dir, "config/nope.ini"), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
