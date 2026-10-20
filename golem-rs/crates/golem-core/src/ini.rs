//! INI engine: model presets, engine adapters, and the section-override
//! merge behind `write_candidate` / `write_overlay` in scripts/tune.sh.
//!
//! The rules that must survive the port, exactly as the scripts do them:
//! - `[section]` names are matched literally; keys are trimmed;
//! - later duplicate keys win (the scripts take the last with `tail -n1`);
//! - `;` and `#` start comments; the `; Download:` lines are read by the
//!   launchers and must round-trip untouched;
//! - an override replaces the original line in place, or prints right
//!   after the section header when the key was absent.

/// One `key = value` pair of a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_hold_what_they_say() {
        let e = Entry { key: "ctx-size".into(), value: "236544".into() };
        assert_eq!(e.key, "ctx-size");
    }
}
