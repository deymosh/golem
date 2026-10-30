//! What the installed engine can compute on: `llama-server --list-devices`,
//! read.
//!
//! The engine picks its devices when it starts, and a build whose GPU
//! backend cannot initialise does not fail: it loads every layer on the CPU
//! and serves at a tenth of the speed. Its log at the router's verbosity says
//! nothing about devices either way - the lines that would are written blank -
//! so the log cannot tell that run from a good one. `--list-devices` can: it
//! prints the GPU devices the build's backends found, and an empty list on a
//! machine configured for a GPU backend is that fallback, seen before a model
//! loads. The binary runs the probe; this reads its answer and says what it
//! means.

/// One device the engine can offload to, as `--list-devices` prints it:
/// `  CUDA0: NVIDIA GeForce RTX 3060 (12287 MiB, 11185 MiB free)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The backend's name for it: `CUDA0`, `ROCm0`, `Vulkan0`, `MTL0`.
    pub name: String,
    pub description: String,
    pub total_mib: Option<u64>,
}

/// The devices in `--list-devices` output. The CPU is never one of them: the
/// engine lists the devices a layer can be offloaded to.
pub fn parse(text: &str) -> Vec<Device> {
    let mut out = Vec::new();
    let mut listing = false;
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Available devices") {
            listing = true;
            continue;
        }
        if !listing || trimmed.is_empty() {
            continue;
        }
        let Some((name, rest)) = trimmed.split_once(": ") else { continue };
        if name.is_empty() || name.contains(' ') {
            continue;
        }
        let (description, total_mib) = match rest.rsplit_once(" (") {
            Some((desc, sizes)) => {
                let total = sizes.split_whitespace().next().and_then(|n| n.parse().ok());
                (desc.trim().to_string(), total)
            }
            None => (rest.trim().to_string(), None),
        };
        out.push(Device { name: name.to_string(), description, total_mib });
    }
    out
}

/// What the probe means for a run on `backend` (`cuda`, `rocm`, `vulkan`,
/// `metal`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The engine has a device of the configured backend.
    Gpu(Device),
    /// The engine found no GPU device: it will run on the CPU. The text says
    /// why it is likely and what to do.
    CpuOnly(String),
}

/// The probe's devices, judged for `backend` on `os` (`windows`, `linux`,
/// `macos`). A device of another backend counts too - a Vulkan device under
/// a build that also carries Vulkan is still a GPU - but the configured
/// backend's own device is preferred.
pub fn verdict(devices: &[Device], backend: &str, os: &str) -> Verdict {
    let prefix = match backend {
        "cuda" => "CUDA",
        "rocm" => "ROCm",
        "vulkan" => "Vulkan",
        "metal" => "MTL",
        _ => "",
    };
    if let Some(own) = devices.iter().find(|d| !prefix.is_empty() && d.name.starts_with(prefix)) {
        return Verdict::Gpu(own.clone());
    }
    if let Some(any) = devices.first() {
        return Verdict::Gpu(any.clone());
    }
    Verdict::CpuOnly(cpu_only_advice(backend, os))
}

/// Why a build of `backend` finds no device, and what to do about it.
pub fn cpu_only_advice(backend: &str, os: &str) -> String {
    let head = format!("the engine's {backend} build found no GPU device and would run on the CPU");
    let fix = match (backend, os) {
        ("rocm", "windows") => {
            "the Windows ROCm build carries the HIP runtime but not hipBLAS and rocBLAS: install the \
             AMD ROCm/HIP SDK matching the build and put its bin folder on PATH for golem service \
             (the engine inherits the service's environment), or set GPU_BACKEND = \"vulkan\""
        }
        ("rocm", _) => "install the ROCm runtime for this card, or set GPU_BACKEND = \"vulkan\"",
        ("cuda", _) => "update the NVIDIA driver (the build needs one that supports its CUDA version)",
        ("vulkan", _) => "update the graphics driver: the card's Vulkan driver did not answer",
        _ => "check the graphics driver",
    };
    format!("{head} - {fix}")
}

/// The DLLs the Windows ROCm build's `ggml-hip.dll` imports and its archive
/// does not carry: without them HIP never initialises.
pub const HIP_LIBRARIES: [&str; 2] = ["hipblas.dll", "rocblas.dll"];

/// The HIP libraries in [`HIP_LIBRARIES`] that are missing, given whether
/// each is found (beside the engine or on PATH).
pub fn missing_hip(found: impl Fn(&str) -> bool) -> Vec<&'static str> {
    HIP_LIBRARIES.iter().copied().filter(|lib| !found(lib)).collect()
}

/// The probe as golem keeps it in `.local/devices.txt`: the backend, then
/// one device per line (`name\tdescription\ttotal`), or `none`.
pub fn to_text(backend: &str, devices: &[Device]) -> String {
    let mut text = format!("backend\t{backend}\n");
    if devices.is_empty() {
        text.push_str("none\n");
    }
    for d in devices {
        text.push_str(&format!("{}\t{}\t{}\n", d.name, d.description, d.total_mib.map_or(String::new(), |t| t.to_string())));
    }
    text
}

/// [`to_text`] read back: the backend and the devices. `None` for a file that
/// is not one.
pub fn from_text(text: &str) -> Option<(String, Vec<Device>)> {
    let mut lines = text.lines();
    let backend = lines.next()?.strip_prefix("backend\t")?.to_string();
    let mut devices = Vec::new();
    for line in lines {
        if line == "none" || line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let name = fields.next()?.to_string();
        let description = fields.next().unwrap_or("").to_string();
        let total_mib = fields.next().and_then(|t| t.parse().ok());
        devices.push(Device { name, description, total_mib });
    }
    Some((backend, devices))
}

/// The effective device in a few words, for `status` and the console:
/// `CUDA0 NVIDIA GeForce RTX 3060` or `CPU only`.
pub fn label(devices: &[Device], backend: &str, os: &str) -> String {
    match verdict(devices, backend, os) {
        Verdict::Gpu(d) => format!("{} {}", d.name, d.description),
        Verdict::CpuOnly(_) => "CPU only".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cuda_listing_is_read() {
        let text = "ggml_cuda_init: found 1 CUDA devices (Total VRAM: 12287 MiB):\n  \
                    Device 0: NVIDIA GeForce RTX 3060, compute capability 8.6, VMM: yes, VRAM: 12287 MiB\n\
                    Available devices:\n  CUDA0: NVIDIA GeForce RTX 3060 (12287 MiB, 11185 MiB free)\n";
        let devices = parse(text);
        assert_eq!(
            devices,
            vec![Device { name: "CUDA0".into(), description: "NVIDIA GeForce RTX 3060".into(), total_mib: Some(12287) }]
        );
        assert_eq!(verdict(&devices, "cuda", "windows"), Verdict::Gpu(devices[0].clone()));
        assert_eq!(label(&devices, "cuda", "windows"), "CUDA0 NVIDIA GeForce RTX 3060");
    }

    #[test]
    fn a_rocm_build_without_its_libraries_lists_nothing_and_says_so() {
        // The Windows ROCm archive of b11379 on an RX 7800 XT without the HIP
        // SDK: HIP never initialises, and the listing is empty.
        let devices = parse("Available devices:\n");
        assert!(devices.is_empty());
        let Verdict::CpuOnly(advice) = verdict(&devices, "rocm", "windows") else { panic!("not a GPU") };
        assert!(advice.contains("ROCm/HIP SDK") && advice.contains("PATH") && advice.contains("vulkan"), "{advice}");
        assert_eq!(label(&devices, "rocm", "windows"), "CPU only");
    }

    #[test]
    fn the_configured_backends_device_is_preferred() {
        let devices = parse("Available devices:\n  Vulkan0: AMD Radeon RX 7800 XT (16368 MiB, 15800 MiB free)\n  ROCm0: AMD Radeon RX 7800 XT (16368 MiB, 16000 MiB free)\n");
        assert_eq!(devices.len(), 2);
        let Verdict::Gpu(d) = verdict(&devices, "rocm", "windows") else { panic!() };
        assert_eq!(d.name, "ROCm0");
    }

    #[test]
    fn the_kept_probe_reads_back() {
        let devices = parse("Available devices:\n  CUDA0: NVIDIA GeForce RTX 3060 (12287 MiB, 11185 MiB free)\n");
        assert_eq!(from_text(&to_text("cuda", &devices)), Some(("cuda".to_string(), devices)));
        assert_eq!(from_text(&to_text("rocm", &[])), Some(("rocm".to_string(), Vec::new())));
        assert_eq!(from_text("something else"), None);
    }

    #[test]
    fn missing_hip_libraries_are_named() {
        assert_eq!(missing_hip(|_| false), vec!["hipblas.dll", "rocblas.dll"]);
        assert!(missing_hip(|_| true).is_empty());
    }
}
