//! A process's exit code in words, where Windows says more than a number:
//! a process the loader or the kernel stopped exits with an NTSTATUS, and
//! the few an engine meets each point at a different fix. The codes are
//! read as the 32-bit value whichever sign they arrive with
//! (`-1073740791` and `3221226505` are both 0xC0000409); an ordinary exit
//! code means nothing here.

/// What exit `code` means, when it is one of the NTSTATUS codes an engine
/// stops with.
pub fn meaning(code: i64) -> Option<&'static str> {
    Some(match code as u32 {
        0xC000_0409 => "the engine aborted (0xC0000409, a failed assert)",
        0xC000_0005 => "the engine crashed (0xC0000005, an access violation)",
        0xC000_001D => "the CPU lacks an instruction this engine build uses (0xC000001D)",
        0xC000_0135 => "a DLL the engine needs was not found (0xC0000135) - reinstall the engine build",
        0xC000_0139 => "a DLL the engine loaded lacks a function it needs (0xC0000139) - reinstall the engine build",
        0xC000_007B => "an engine file is not a valid 64-bit Windows image (0xC000007B) - reinstall the engine build",
        0xC0E9_0002 | 0xC000_0428 => {
            "Windows refused to load an engine file (a code integrity policy: Smart App Control, \
             or App Control on a managed PC, blocks unsigned code); the CodeIntegrity event log \
             names the file"
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_reads_the_same_with_either_sign() {
        assert_eq!(meaning(-1073740791), meaning(3221226505));
        assert!(meaning(-1073740791).is_some_and(|m| m.contains("0xC0000409")));
    }

    #[test]
    fn a_blocked_dll_is_named_as_a_policy() {
        // An RX 7800 XT box's Vulkan build: ggml-base.dll refused at start.
        let words = meaning(-1058471934).expect("0xC0E90002");
        assert!(words.contains("Smart App Control"), "{words}");
        assert!(meaning(0xC000_0428).is_some());
    }

    #[test]
    fn an_ordinary_exit_code_means_nothing_here() {
        for code in [0, 1, 3, 255, -1] {
            assert_eq!(meaning(code), None, "{code}");
        }
    }
}
