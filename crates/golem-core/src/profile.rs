//! Profiles: one tuned setup per use of the machine, chosen by a restart.
//!
//! A profile is a context target and the gate that goes with it, over the
//! measurements the tuner already takes: the coder's is the 200K gate, a
//! chat's 32K - the smallest context an agent works in, since the gate's
//! agent suite sends requests of 20K tokens and Claude Code's first prompt
//! is about 27K. `golem tune --profile <name>` gates a setup at its target and
//! writes its overlay beside the coder's (`<section>@<name>.ini`, the coder's
//! is `<section>.ini`); `PROFILE` in golem.toml says which overlays the
//! effective preset carries, and switching is [`switch`] and a restart, not
//! a new tune. A section with no overlay for the profile keeps the coder's.

use crate::ini;

/// One profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    pub name: &'static str,
    /// The context it serves, or `None` for the gate's: the coder grows the
    /// context as far as the card allows and gates it at 200K.
    pub ctx: Option<u64>,
    pub about: &'static str,
}

/// Every profile, the default first.
pub const PROFILES: [Profile; 2] = [
    Profile { name: "coder", ctx: None, about: "a coding agent: the largest context the card holds, gated at 200K" },
    Profile { name: "chat", ctx: Some(32_768), about: "local chat: 32K of context, gated at 29K" },
];

/// The profile a machine serves when golem.toml names none.
pub const DEFAULT: &str = "coder";

/// The profile named `name`.
pub fn get(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name)
}

/// The gate's needle for a profile with a context: 90% of it, the share the
/// tuner gates a shorter context at.
pub fn gate_tokens(ctx: u64) -> u64 {
    ctx * 9 / 10
}

/// The overlay file of `section` under `profile`.
pub fn overlay_file(section: &str, profile: &str) -> String {
    if profile == DEFAULT {
        format!("{section}.ini")
    } else {
        format!("{section}@{profile}.ini")
    }
}

/// The overlays `profile` serves, out of the file names in an engine's
/// overlay directory: per section the profile's own, else the coder's.
/// Sorted by section.
pub fn chosen(files: &[String], profile: &str) -> Vec<String> {
    let mut sections: Vec<&str> = files
        .iter()
        .filter_map(|f| f.strip_suffix(".ini"))
        .map(|stem| stem.split_once('@').map_or(stem, |(section, _)| section))
        .collect();
    sections.sort();
    sections.dedup();
    sections
        .into_iter()
        .filter_map(|section| {
            let own = overlay_file(section, profile);
            let coder = overlay_file(section, DEFAULT);
            [own, coder].into_iter().find(|f| files.contains(f))
        })
        .collect()
}

/// The effective preset served under the incoming overlays rather than the
/// outgoing ones: every key an outgoing overlay set goes back to the base
/// preset's value (or goes, where the base has none), then the incoming
/// overlays are merged. Everything else in `effective` - a section the
/// console added, one it removed - stays as it is.
pub fn switch(effective: &str, base: &str, outgoing: &[String], incoming: &[String]) -> String {
    let base_doc = ini::parse(base);
    let mut text = effective.to_string();
    for overlay in outgoing {
        for section in &ini::parse(overlay).sections {
            if ini::parse(&text).get(&section.name).is_none() {
                continue;
            }
            let keys: Vec<String> = section.entries.iter().map(|e| e.key.clone()).collect();
            text = ini::remove_keys(&text, &section.name, &keys);
            let restore: Vec<String> = keys
                .iter()
                .filter_map(|k| base_doc.value(&section.name, k).map(|v| format!("{k} = {v}")))
                .collect();
            if !restore.is_empty() {
                text = ini::write_overlay(&text, &format!("[{}]\n{}\n", section.name, restore.join("\n")));
            }
        }
    }
    for overlay in incoming {
        text = ini::write_overlay(&text, overlay);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "[*]\njinja = true\n[coder]\nmodel = c.gguf\nctx-size = 32768\nubatch-size = 2048\n[small]\nmodel = s.gguf\n";

    #[test]
    fn a_profile_serves_its_own_overlays_else_the_coders() {
        let files: Vec<String> = ["coder.ini", "coder@chat.ini", "small.ini", "notes.txt"].iter().map(|s| s.to_string()).collect();
        assert_eq!(chosen(&files, "chat"), ["coder@chat.ini", "small.ini"]);
        assert_eq!(chosen(&files, "coder"), ["coder.ini", "small.ini"]);
        assert_eq!(overlay_file("coder", "chat"), "coder@chat.ini");
        assert_eq!(get("chat").and_then(|p| p.ctx), Some(32_768));
        assert!(get("max").is_none(), "16K holds no agent's request");
        assert_eq!(gate_tokens(32_768), 29_491);
    }

    #[test]
    fn switching_undoes_the_outgoing_keys_and_keeps_the_rest() {
        let coder = "[coder]\nn-cpu-moe = 35\nctx-size = 248832\nfit = off\n".to_string();
        let chat = "[coder]\nn-cpu-moe = 30\nctx-size = 32768\nubatch-size = 1024\n".to_string();
        // The served file: the base, the coder's overlay, and a section the
        // console added.
        let served = ini::write_overlay(BASE, &coder) + "[added]\nmodel = a.gguf\n";
        let as_chat = switch(&served, BASE, &[coder.clone()], &[chat.clone()]);
        let doc = ini::parse(&as_chat);
        assert_eq!(doc.value("coder", "n-cpu-moe"), Some("30"));
        assert_eq!(doc.value("coder", "ctx-size"), Some("32768"));
        assert_eq!(doc.value("coder", "ubatch-size"), Some("1024"));
        assert_eq!(doc.value("coder", "fit"), None, "a key only the coder's overlay set goes");
        assert_eq!(doc.value("added", "model"), Some("a.gguf"), "the console's section stays");
        // And back: the chat's ubatch-size returns to the base's 2048.
        let back = ini::parse(&switch(&as_chat, BASE, &[chat], &[coder]));
        assert_eq!(back.value("coder", "ubatch-size"), Some("2048"));
        assert_eq!(back.value("coder", "ctx-size"), Some("248832"));
        assert_eq!(back.value("coder", "fit"), Some("off"));
    }
}
