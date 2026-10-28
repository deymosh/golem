//! What the machine is and what it has free: the card, the memory, the CPU,
//! and the anonymous slug a measurement is filed under.
//!
//! Every probe is one short-lived command or one file read, and best effort:
//! `None` (or `unknown` / `0`) when the tool is not there, because a missing
//! fact is not a reason to fail the command that wanted it. The card is read
//! vendor-neutrally: nvidia-smi when it is there, one PowerShell probe over
//! the Windows registry and GPU counters otherwise, amdgpu's sysfs on Linux.
//! Two traps are handled here once, for every caller: MSYS's `/proc` files
//! exist on Windows but are stubs, and PowerShell prints numbers in the
//! machine's locale - `19,2` on this box - which `f64::from_str` rejects.

use std::process::{Command, Stdio};

use golem_core::gpu;
use golem_core::record;

/// The card golem serves from: its vendor, its name, and its VRAM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gpu {
    pub name: String,
    pub vendor: gpu::Vendor,
    pub total_mib: u64,
    /// The whole card's use right now, desktop included.
    pub used_mib: u64,
}

/// The readable card, or `None` when nothing names one.
pub fn gpu() -> Option<Gpu> {
    if let Some(line) = nvidia_smi("name,memory.total,memory.used") {
        let mut fields = line.split(',').map(str::trim);
        let name = fields.next().filter(|n| !n.is_empty());
        let total = fields.next().and_then(|v| v.parse().ok());
        let used = fields.next().and_then(|v| v.parse().ok());
        if let (Some(name), Some(total_mib), Some(used_mib)) = (name, total, used) {
            return Some(Gpu {
                name: name.to_string(),
                vendor: gpu::Vendor::Nvidia,
                total_mib,
                used_mib,
            });
        }
        // nvidia-smi answered but said nothing parseable; a platform probe
        // may still name the card.
    }
    if cfg!(windows) {
        let text = powershell_stdout(WINDOWS_GPU_SCRIPT)?;
        let reading = gpu::parse_windows(&text);
        let adapter = gpu::pick(&reading.adapters)?;
        return Some(Gpu {
            name: adapter.name.clone(),
            vendor: adapter.vendor,
            total_mib: adapter.total_mib,
            used_mib: reading.used_mib.unwrap_or(0),
        });
    }
    sysfs_gpu()
}

/// The whole card's VRAM in use, in MiB.
///
/// nvidia-smi first: one fast spawn. Windows falls back to the GPU
/// performance counters (one PowerShell start, ~0.4s; the counters name
/// their instances by LUID, so the maximum `DedicatedUsage` is the answer),
/// Linux to the picked card's `mem_info_vram_used` (file reads).
pub fn vram_used_mib() -> Option<u64> {
    if let Some(used) = nvidia_smi("memory.used").and_then(|v| v.parse().ok()) {
        return Some(used);
    }
    if cfg!(windows) {
        let text = powershell_stdout(WINDOWS_USED_SCRIPT)?;
        return gpu::parse_windows(&text).used_mib;
    }
    sysfs_gpu().map(|g| g.used_mib)
}

/// The Windows probe for the card: the display class' registry keys name
/// every adapter with its PCI ID, its description and its VRAM (some
/// drivers keep the size as a qword, others only as a binary blob), and the
/// GPU performance counters give the machine's VRAM in use. One
/// invocation, because a powershell.exe start costs ~0.4s.
const WINDOWS_GPU_SCRIPT: &str = "\
$keys = Get-ChildItem 'HKLM:\\SYSTEM\\CurrentControlSet\\Control\\Class\\{4d36e968-e325-11ce-bfc1-08002be10318}' -ErrorAction SilentlyContinue; \
foreach ($k in $keys) { \
$p = Get-ItemProperty $k.PSPath -ErrorAction SilentlyContinue; \
if ($p.MatchingDeviceId -and $p.DriverDesc) { \
Write-Output \"adapter`t$($p.MatchingDeviceId)`t$($p.DriverDesc)`t$($p.'HardwareInformation.qwMemorySize')`t$($p.'HardwareInformation.MemorySize')\" \
} }; \
Get-CimInstance Win32_PerfRawData_GPUPerformanceCounters_GPUAdapterMemory -ErrorAction SilentlyContinue | \
ForEach-Object { Write-Output \"used`t$($_.DedicatedUsage)\" }";

/// The used half of [`WINDOWS_GPU_SCRIPT`], for when only the use is asked.
const WINDOWS_USED_SCRIPT: &str = "Get-CimInstance \
Win32_PerfRawData_GPUPerformanceCounters_GPUAdapterMemory -ErrorAction SilentlyContinue | \
ForEach-Object { Write-Output \"used`t$($_.DedicatedUsage)\" }";

/// The card the DRM sysfs exposes on Linux: the vendor and device IDs,
/// amdgpu's VRAM counters, and `product_name` when the driver carries one.
/// Connector entries (`card0-HDMI-A-1`) sit beside the cards and are
/// skipped - only `card` + digits is a card.
fn sysfs_gpu() -> Option<Gpu> {
    let entries = std::fs::read_dir("/sys/class/drm").ok()?;
    let mut best: Option<(gpu::Adapter, u64)> = None;
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let name = file_name.to_str().unwrap_or("");
        if !(name.starts_with("card")
            && name.len() > "card".len()
            && name["card".len()..].bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let dev = entry.path().join("device");
        let read = |file: &str| std::fs::read_to_string(dev.join(file)).ok();
        let Some(vendor) = read("vendor").and_then(|v| gpu::vendor(v.trim())) else {
            continue;
        };
        let total_mib = read("mem_info_vram_total")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let used_mib = read("mem_info_vram_used")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let product = read("product_name")
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty());
        let name = product.unwrap_or_else(|| {
            let device = read("device").map(|d| d.trim().to_string()).unwrap_or_default();
            let prefix = match vendor {
                gpu::Vendor::Nvidia => "NVIDIA",
                gpu::Vendor::Amd => "AMD",
                gpu::Vendor::Intel => "Intel",
            };
            format!("{prefix} GPU {device}")
        });
        let card = gpu::Adapter { vendor, name, total_mib };
        if best.as_ref().map_or(true, |(b, _)| card.total_mib > b.total_mib) {
            best = Some((card, used_mib));
        }
    }
    let (card, used_mib) = best?;
    Some(Gpu { name: card.name, vendor: card.vendor, total_mib: card.total_mib, used_mib })
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
/// tuned setups travel with a shared report. With no readable card the GPU
/// is `unknown` and its VRAM 0, and the slug is still a slug.
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
    let first = powershell_stdout(script)?;
    let first = first.lines().next().unwrap_or("").trim().replace(',', ".");
    (!first.is_empty()).then_some(first)
}

/// One PowerShell script's whole stdout, with the carriage returns a Windows
/// console adds stripped. `None` off Windows, or when it fails.
fn powershell_stdout(script: &str) -> Option<String> {
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
    (!text.trim().is_empty()).then_some(text)
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
