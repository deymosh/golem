//! What the machine is and what it has free: the card, the memory, the CPU,
//! and the anonymous slug a measurement is filed under.
//!
//! Every probe is one short-lived command or one file read, and best effort:
//! `None` (or `unknown` / `0`) when the tool is not there, because a missing
//! fact is not a reason to fail the command that wanted it. Two traps are
//! handled here once, for every caller: MSYS's `/proc` files exist on
//! Windows but are stubs, and PowerShell prints numbers in the machine's
//! locale - `19,2` on this box - which `f64::from_str` rejects.

use std::process::{Command, Stdio};

use golem_core::record;

/// The card, as nvidia-smi names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpu {
    pub name: String,
    pub total_mib: u64,
    /// The whole card's use right now, desktop included.
    pub used_mib: u64,
}

/// The NVIDIA card, or `None` when there is none (or no nvidia-smi).
pub fn gpu() -> Option<Gpu> {
    let line = nvidia_smi("name,memory.total,memory.used")?;
    let mut fields = line.split(',').map(str::trim);
    let name = fields.next().filter(|n| !n.is_empty())?.to_string();
    let total_mib = fields.next()?.parse().ok()?;
    let used_mib = fields.next()?.parse().ok()?;
    Some(Gpu { name, total_mib, used_mib })
}

/// The whole card's VRAM in use, in MiB.
pub fn vram_used_mib() -> Option<u64> {
    nvidia_smi("memory.used")?.parse().ok()
}

/// The first line nvidia-smi prints for `fields`, trimmed.
fn nvidia_smi(fields: &str) -> Option<String> {
    let out = Command::new("nvidia-smi")
        .args([&format!("--query-gpu={fields}"), "--format=csv,noheader,nounits"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().next()?.trim().to_string())
}

/// Free physical memory in GB.
///
/// `MemAvailable` on Linux; on Windows MSYS's `/proc/meminfo` has no such
/// line, so the answer comes from `Win32_OperatingSystem`. Costs a PowerShell
/// start there (~0.4s), which is why the screen caches it.
pub fn ram_free_gb() -> Option<f64> {
    if let Some(kb) = meminfo_kb("MemAvailable:") {
        return Some(kb / 1_048_576.0);
    }
    powershell("[math]::Round((Get-CimInstance Win32_OperatingSystem).FreePhysicalMemory/1MB,1)")?
        .parse()
        .ok()
}

/// Total system RAM in whole GB, or 0 when it cannot be read.
pub fn ram_total_gb() -> u64 {
    if let Some(kb) = meminfo_kb("MemTotal:") {
        return (kb / 1_048_576.0).round() as u64;
    }
    powershell("[math]::Round((Get-CimInstance Win32_OperatingSystem).TotalVisibleMemorySize/1MB,0)")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// One `/proc/meminfo` field in kB.
fn meminfo_kb(field: &str) -> Option<f64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    text.lines()
        .find_map(|line| line.strip_prefix(field))
        .and_then(|rest| rest.split_whitespace().next()?.parse().ok())
}

/// The CPU's marketing name.
pub fn cpu_name() -> Option<String> {
    if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
        let name = text
            .lines()
            .find_map(|line| line.strip_prefix("model name"))
            .map(|rest| rest.trim_start_matches(|c: char| c == ':' || c.is_whitespace()).trim());
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            return Some(name.to_string());
        }
    }
    powershell("(Get-CimInstance Win32_Processor).Name")
}

/// Logical CPUs.
pub fn logical_cores() -> u32 {
    std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(8)
}

/// Physical cores, or `None` when they cannot be read. MSYS's `/proc/cpuinfo`
/// reports the logical count as `cpu cores`, so it is trusted only off Windows.
pub fn physical_cores() -> Option<u32> {
    if !cfg!(windows) {
        if let Ok(text) = std::fs::read_to_string("/proc/cpuinfo") {
            if let Some(n) = text
                .lines()
                .find_map(|line| line.strip_prefix("cpu cores"))
                .and_then(|rest| rest.split(':').nth(1)?.trim().parse().ok())
            {
                return Some(n);
            }
        }
    }
    powershell("(Get-CimInstance Win32_Processor).NumberOfCores")?.parse().ok()
}

/// `windows` / `macos` / `linux`.
pub fn os_name() -> &'static str {
    if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "macos"
    } else {
        "linux"
    }
}

/// The anonymous hardware slug a measurement and a tuned setup are filed
/// under: `<gpu>_<vram GiB>g_<ram GB>g_<cpu>`, never the hostname - rows and
/// tuned setups travel with a shared report. With no NVIDIA card the GPU is
/// `unknown` and its VRAM 0, and the slug is still a slug.
pub fn host_slug(gpu: &str, vram_total_mib: u64, ram_total_gb: u64, cpu: &str) -> String {
    format!(
        "{}_{}g_{}g_{}",
        record::slug(gpu),
        vram_total_mib / 1024,
        ram_total_gb,
        record::slug(cpu)
    )
}

/// One PowerShell one-liner's first line, with a comma decimal turned back
/// into a dot. `None` off Windows, or when it fails.
pub fn powershell(script: &str) -> Option<String> {
    if !cfg!(windows) {
        return None;
    }
    let out = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).replace('\r', "");
    let first = text.lines().next().unwrap_or("").trim().replace(',', ".");
    (!first.is_empty()).then_some(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slug_is_anonymous_hardware() {
        assert_eq!(
            host_slug("NVIDIA GeForce RTX 3060", 12288, 32, "12th Gen Intel(R) Core(TM) i7-12700KF"),
            "rtx-3060_12g_32g_i7-12700kf"
        );
        assert_eq!(host_slug("unknown", 0, 32, ""), "unknown_0g_32g_");
    }
}
