//! The library: the short list of models golem knows to be good, shipped
//! with it (`config/library.ini`).
//!
//! What the Hub cannot say lives here: which models are worth having, for
//! what (coder, everyday, vision, smoke test), and the vendor's benchmark
//! score with where it comes from. `golem pull <name>` fetches a library
//! model by its name, and `tune --auto` ranks installed models by the score.
//! A score is a ranking input only: nothing is adopted without the eval gate.

use crate::ini;

/// One library model.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// The preset name it is served under.
    pub name: String,
    pub repo: String,
    /// The quant to fetch; the machine's pick when absent.
    pub quant: Option<String>,
    pub role: Option<String>,
    /// The vendor's score (SWE-bench Verified or the nearest equivalent).
    pub quality: Option<f64>,
    /// Where the score comes from.
    pub source: Option<String>,
}

/// The entries of a library file, in file order. A section without a `repo`
/// is not a model and is skipped.
pub fn parse(text: &str) -> Vec<Entry> {
    let doc = ini::parse(text);
    doc.sections
        .iter()
        .filter(|s| s.name != "*")
        .filter_map(|s| {
            let value = |key: &str| s.value(key).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
            Some(Entry {
                name: s.name.clone(),
                repo: value("repo")?,
                quant: value("quant"),
                role: value("role"),
                quality: value("quality").and_then(|q| q.parse().ok()),
                source: value("quality-source"),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entries_carry_the_source_role_and_score() {
        let text = "\
; A comment.
[coder]
repo = org/Coder-GGUF
quant = Q4_K_M
role = coder
quality = 79
quality-source = SWE-bench Verified, vendor-reported

[tiny]
repo = org/Tiny-GGUF

[no-repo]
role = smoke
";
        let entries = parse(text);
        assert_eq!(entries.len(), 2, "a section with no repo is not a model");
        assert_eq!(entries[0].name, "coder");
        assert_eq!(entries[0].quant.as_deref(), Some("Q4_K_M"));
        assert_eq!(entries[0].quality, Some(79.0));
        assert_eq!(entries[0].source.as_deref(), Some("SWE-bench Verified, vendor-reported"));
        assert_eq!(entries[1], Entry {
            name: "tiny".into(),
            repo: "org/Tiny-GGUF".into(),
            quant: None,
            role: None,
            quality: None,
            source: None,
        });
    }

    #[test]
    fn the_shipped_library_parses_and_every_model_has_a_role() {
        let entries = parse(include_str!("../../../../config/library.ini"));
        assert!(!entries.is_empty());
        for entry in &entries {
            assert!(entry.role.is_some(), "{} has no role", entry.name);
            assert!(entry.repo.contains('/'), "{}: {}", entry.name, entry.repo);
            if entry.quality.is_some() {
                assert!(entry.source.is_some(), "{} has a score with no source", entry.name);
            }
        }
    }
}
