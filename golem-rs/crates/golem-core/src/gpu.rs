//! The graphics card, decided from whatever a reading offers: a PCI ID
//! becomes a vendor, a vendor becomes a backend, and the Windows registry
//! probe's output becomes the adapters and their VRAM. The binary asks the
//! machine ([`crate::gpu`] has no I/O of its own); this decides what the
//! answers mean.

/// The card's maker, read from its PCI vendor ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Nvidia,
    Amd,
    Intel,
}

/// The vendor for a PCI ID: a Windows `MatchingDeviceId`
/// (`pci\ven_1002&dev_747e`), or the sysfs form (`0x1002`). Case does not
/// matter, and a virtual adapter (`Root\Parsec\VDA`) or anything else
/// without a vendor is `None`.
pub fn vendor(pci_id: &str) -> Option<Vendor> {
    let id = pci_id.to_ascii_lowercase();
    let hex = if let Some(at) = id.find("ven_") {
        id.get(at + 4..at + 8)?
    } else {
        id.strip_prefix("0x")?.get(..4)?
    };
    match hex {
        "10de" => Some(Vendor::Nvidia),
        "1002" => Some(Vendor::Amd),
        "8086" => Some(Vendor::Intel),
        _ => None,
    }
}

/// The backend the card's vendor can serve: CUDA for NVIDIA, ROCm for AMD,
/// Vulkan for Intel.
pub fn backend(vendor: Vendor) -> &'static str {
    match vendor {
        Vendor::Nvidia => "cuda",
        Vendor::Amd => "rocm",
        Vendor::Intel => "vulkan",
    }
}

/// One adapter a reading found: what it is and how much VRAM it has. An
/// iGPU's carve-out is a small `total_mib`; a discrete card reads larger,
/// which is how [`pick`] tells them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adapter {
    pub vendor: Vendor,
    pub name: String,
    pub total_mib: u64,
}

/// A memory size in bytes: a decimal integer, or the REG_BINARY some
/// drivers keep `HardwareInformation.MemorySize` in, which PowerShell
/// interpolates as space-separated decimal bytes, little-endian.
pub fn memory_bytes(value: &str) -> Option<u64> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() == 1 {
        return tokens[0].parse().ok();
    }
    // More than 8 bytes cannot be a 64-bit size, so a blob that long is
    // not a memory size.
    if tokens.len() > 8 {
        return None;
    }
    tokens
        .iter()
        .rev()
        .try_fold(0u64, |acc, token| {
            token.parse::<u8>().map(|byte| acc << 8 | u64::from(byte))
        })
        .ok()
}

/// The adapter to serve from: the largest VRAM - a discrete card beats an
/// iGPU's carve-out - and on a tie the first.
pub fn pick(adapters: &[Adapter]) -> Option<&Adapter> {
    let mut best: Option<&Adapter> = None;
    for adapter in adapters {
        if best.map_or(true, |b: &Adapter| adapter.total_mib > b.total_mib) {
            best = Some(adapter);
        }
    }
    best
}

/// What one Windows probe read: the adapters with a known vendor, and the
/// whole machine's VRAM in use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub adapters: Vec<Adapter>,
    /// The largest `used` reading, in MiB. The counters name their
    /// instances by LUID, which cannot be mapped to an adapter without
    /// DXGI, so the maximum is the machine's answer.
    pub used_mib: Option<u64>,
}

/// The Windows probe's output: tab-separated `adapter` lines
/// (`<MatchingDeviceId>`, `<DriverDesc>`, the qword memory size, the
/// possibly-binary one) and `used` lines (the counter's `DedicatedUsage`
/// bytes). Lines that are neither, and adapters without a known vendor,
/// are skipped; carriage returns a Windows console adds are tolerated.
pub fn parse_windows(text: &str) -> Reading {
    let mut adapters: Vec<Adapter> = Vec::new();
    let mut used_mib: Option<u64> = None;
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        let mut fields = line.split('\t');
        match fields.next() {
            Some("adapter") => {
                let Some(vendor) = fields.next().and_then(vendor) else { continue };
                let name = fields.next().unwrap_or("").trim().to_string();
                // The qword is the exact form; the binary blob is what the
                // drivers that never wrote one leave instead.
                let total_mib = fields
                    .next()
                    .and_then(memory_bytes)
                    .or_else(|| fields.next().and_then(memory_bytes))
                    .map_or(0, |bytes| bytes / 1_048_576);
                adapters.push(Adapter { vendor, name, total_mib });
            }
            Some("used") => {
                if let Some(mib) = fields.next().and_then(memory_bytes).map(|b| b / 1_048_576) {
                    used_mib = Some(used_mib.map_or(mib, |old| old.max(mib)));
                }
            }
            _ => {}
        }
    }
    Reading { adapters, used_mib }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pci_id_names_the_vendor_whatever_its_case_or_form() {
        assert_eq!(vendor("pci\\ven_10de&dev_2504"), Some(Vendor::Nvidia));
        assert_eq!(vendor("PCI\\VEN_10DE&DEV_2504&SUBSYS_40423842"), Some(Vendor::Nvidia));
        assert_eq!(vendor("pci\\ven_1002&dev_747e"), Some(Vendor::Amd));
        assert_eq!(vendor("PCI\\VEN_8086&DEV_46A6"), Some(Vendor::Intel));
        assert_eq!(vendor("0x1002"), Some(Vendor::Amd));
        assert_eq!(vendor("0x10DE\n"), Some(Vendor::Nvidia));
    }

    #[test]
    fn an_id_without_a_known_vendor_is_none() {
        assert_eq!(vendor("Root\\Parsec\\VDA"), None);
        assert_eq!(vendor(""), None);
        assert_eq!(vendor("0xbeef"), None);
        assert_eq!(vendor("pci\\ven_1234&dev_5678"), None);
    }

    #[test]
    fn each_vendor_serves_its_backend() {
        assert_eq!(backend(Vendor::Nvidia), "cuda");
        assert_eq!(backend(Vendor::Amd), "rocm");
        assert_eq!(backend(Vendor::Intel), "vulkan");
    }

    #[test]
    fn a_memory_size_reads_as_decimal_or_as_a_binary_blob() {
        // `HardwareInformation.qwMemorySize`, a qword.
        assert_eq!(memory_bytes("12884901888"), Some(12_884_901_888));
        // `HardwareInformation.MemorySize` for 16 GiB, as the driver's
        // REG_BINARY arrives: little-endian bytes, space-separated.
        assert_eq!(memory_bytes("0 0 0 0 4 0 0 0"), Some(17_179_869_184));
        assert_eq!(memory_bytes("0 0 0 128"), Some(2_147_483_648));
        assert_eq!(memory_bytes("  4096  "), Some(4096));
        assert_eq!(memory_bytes(""), None);
        assert_eq!(memory_bytes("junk"), None);
        assert_eq!(memory_bytes("1 2 3 4 5 6 7 8 9"), None, "more than 8 bytes is not a size");
    }

    #[test]
    fn the_largest_card_wins_and_a_tie_keeps_the_first() {
        let igpu = || Adapter { vendor: Vendor::Amd, name: "AMD Radeon(TM) Graphics".into(), total_mib: 512 };
        let card = || Adapter { vendor: Vendor::Amd, name: "AMD Radeon RX 7800 XT".into(), total_mib: 16384 };
        assert_eq!(pick(&[igpu(), card()]), Some(&card()));
        assert_eq!(pick(&[card(), igpu()]), Some(&card()));
        let a = Adapter { vendor: Vendor::Nvidia, name: "a".into(), total_mib: 100 };
        let b = Adapter { vendor: Vendor::Nvidia, name: "b".into(), total_mib: 100 };
        assert_eq!(pick(&[a, b]).map(|x| x.name.as_str()), Some("a"));
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn the_windows_reading_names_the_card_and_its_use() {
        // Real shape on the NVIDIA box: the card, and a virtual display
        // adapter the Parsec driver adds beside it.
        let text = "adapter\tpci\\ven_10de&dev_2504\tNVIDIA GeForce RTX 3060\t12884901888\t\r\n\
                    adapter\tRoot\\Parsec\\VDA\tParsec Virtual Display Adapter\t\t\r\n\
                    used\t1189257216\r\n\
                    used\t512\r\n";
        let reading = parse_windows(text);
        assert_eq!(reading.adapters.len(), 1, "the virtual adapter has no vendor");
        assert_eq!(
            reading.adapters[0],
            Adapter { vendor: Vendor::Nvidia, name: "NVIDIA GeForce RTX 3060".into(), total_mib: 12288 }
        );
        assert_eq!(reading.used_mib, Some(1134));
        assert_eq!(pick(&reading.adapters).map(|a| a.name.as_str()), Some("NVIDIA GeForce RTX 3060"));
    }

    #[test]
    fn an_amd_reading_keeps_both_cards_and_picks_the_discrete_one() {
        // The iGPU's carve-out is a qword; the RX 7800 XT's driver keeps its
        // size in the binary form only.
        let text = "adapter\tpci\\ven_1002&dev_13c0\tAMD Radeon(TM) Graphics\t536870912\t\r\n\
                    adapter\tpci\\ven_1002&dev_747e\tAMD Radeon RX 7800 XT\t\t0 0 0 0 4 0 0 0\r\n\
                    used\t241172480\n";
        let reading = parse_windows(text);
        assert_eq!(reading.adapters.len(), 2);
        assert_eq!(
            pick(&reading.adapters).map(|a| a.name.as_str()),
            Some("AMD Radeon RX 7800 XT")
        );
        assert_eq!(pick(&reading.adapters).map(|a| a.total_mib), Some(16384));
        assert_eq!(reading.used_mib, Some(230));
    }

    #[test]
    fn a_junk_line_and_a_missing_size_do_not_stop_the_reading() {
        let reading = parse_windows("whatever\r\nadapter\tpci\\ven_10de&dev_2504\tA card\t\t\r\n");
        assert_eq!(reading.adapters.len(), 1);
        assert_eq!(reading.adapters[0].total_mib, 0, "no size field answered");
        assert_eq!(reading.used_mib, None);
    }
}
