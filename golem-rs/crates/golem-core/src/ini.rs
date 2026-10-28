//! INI engine: model presets, engine adapters, and the section-override
//! merge behind the tuner's candidates and overlays.
//!
//! The rules:
//! - `[section]` names are matched literally, and a header only counts at
//!   column 0;
//! - keys and values are trimmed; later duplicate keys win;
//! - `;` and `#` start comments; the `; Download:` lines are read by
//!   `up`, `pull` and the console, and must round-trip untouched;
//! - an override replaces the original line in place, or prints right
//!   after the section header when the key was absent.
//!
//! The file is CRLF in a Windows working tree and LF in the repository (see
//! `.gitattributes`), so the editors here reproduce whichever ending they
//! were given rather than imposing one - a console click must not rewrite
//! every line of the file it touched.

use std::fmt::Write as _;
use std::path::Path;

/// One `key = value` pair of a section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
}

impl Entry {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Entry {
        Entry { key: key.into(), value: value.into() }
    }

    /// The pair as a line of the file: one space either side of the `=`, the
    /// way the shipped presets spell it.
    pub fn line(&self) -> String {
        format!("{} = {}", self.key, self.value)
    }
}

/// One `; Download: <repo> -> <file>` comment: where a preset's GGUF comes
/// from. `up` and `pull` fetch anything `data/models` is missing from these.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Download {
    pub repo: String,
    /// The file's path in the repository, which may be in a subdirectory
    /// (a split model's shards often are).
    pub file: String,
}

impl Download {
    /// The name the file is stored under in `data/models`: the path's last
    /// part, so a split model's shards land side by side.
    pub fn name(&self) -> &str {
        crate::hub::stored_name(&self.file)
    }
}

/// One `[section]` of the file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Section {
    pub name: String,
    /// The `; Download:` lines above the header, in file order. A section can
    /// need more than one file (a coder plus its MTP draft); the console
    /// shows the first as *the* model, and `up` and `pull` fetch them all.
    pub downloads: Vec<Download>,
    pub entries: Vec<Entry>,
}

impl Section {
    /// The value of `key`, with a later duplicate winning.
    pub fn value(&self, key: &str) -> Option<&str> {
        self.entries.iter().rev().find(|e| e.key == key).map(|e| e.value.as_str())
    }
}

/// A parsed preset file: what is in it, and the text it came from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Document {
    /// Comment and blank lines before the first header (the file's own
    /// documentation, and the `version = 1` line).
    pub preamble: Vec<String>,
    pub sections: Vec<Section>,
    /// The text as it was read. The editing methods work from this, so a
    /// reader that only wants to look at the file can throw it away.
    pub text: String,
}

impl Document {
    /// Read and parse a file.
    pub fn read(path: &Path) -> std::io::Result<Document> {
        Ok(parse(&std::fs::read_to_string(path)?))
    }

    /// The first section with this name.
    pub fn get(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// The value of `name`'s `key`, or `None` when either is absent.
    pub fn value(&self, name: &str, key: &str) -> Option<&str> {
        self.get(name)?.value(key)
    }

    /// The value `name` serves with for `key`: its own, else the shared `[*]`
    /// section's - what llama-server's router applies.
    pub fn effective(&self, name: &str, key: &str) -> Option<&str> {
        self.value(name, key).or_else(|| self.value("*", key))
    }

    /// The value of `key` set outside any section.
    ///
    /// `engines/<name>/engine.ini` has no sections at all - it is a flat list
    /// of keys under its own comment block - so this is how an adapter's
    /// `tune` flag is read. The first assignment wins.
    pub fn global(&self, key: &str) -> Option<&str> {
        self.preamble.iter().find_map(|line| match assignment(line) {
            Some((name, value)) if name == key => Some(value),
            _ => None,
        })
    }

    /// The file's sections, in file order, without the shared `[*]` one.
    pub fn preset_names(&self) -> Vec<String> {
        self.sections
            .iter()
            .filter(|s| s.name != "*")
            .map(|s| s.name.clone())
            .collect()
    }

    /// The text with section `name` removed.
    pub fn remove(&self, name: &str) -> String {
        remove_section(&self.text, name)
    }

    /// The text with `block` appended as new lines.
    pub fn append(&self, block: &str) -> String {
        append_section(&self.text, block)
    }
}

/// The `[section]` name of a header line, when it is one.
///
/// Column 0 only: an indented `[x]` is not a header.
fn header_of(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let (name, _) = rest.split_once(']')?;
    (!name.is_empty()).then_some(name)
}

/// The `; Download: repo -> file` pair of a comment line, when it is one.
fn download_of(line: &str) -> Option<Download> {
    let rest = line.trim_start().strip_prefix(';')?;
    let rest = rest.trim_start().strip_prefix("Download:")?;
    let (repo, file) = rest.split_once("->")?;
    // One word either side: a path with a space in it is not a download
    // line at all.
    let repo = repo.split_whitespace().next()?;
    let file = file.split_whitespace().next()?;
    Some(Download { repo: repo.to_string(), file: file.to_string() })
}

/// The `(key, value)` of an assignment line, borrowed from it.
fn assignment(line: &str) -> Option<(&str, &str)> {
    let trimmed = line.trim();
    if trimmed.starts_with(';') || trimmed.starts_with('#') {
        return None;
    }
    let (key, value) = trimmed.split_once('=')?;
    Some((key.trim(), value.trim()))
}

/// One `key = value` line of a section, when it is one.
fn entry_of(line: &str) -> Option<Entry> {
    let (key, value) = assignment(line)?;
    Some(Entry { key: key.to_string(), value: value.to_string() })
}

/// Parse a preset file.
///
/// A `; Download:` line belongs to the section *below* it - that is where
/// `config/models.ini` puts them - so they are collected and handed to the
/// next header.
pub fn parse(text: &str) -> Document {
    // A byte-order mark (Notepad's "UTF-8 with BOM") is not part of the
    // first line, which may be a section header.
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut doc = Document { preamble: Vec::new(), sections: Vec::new(), text: text.to_string() };
    let mut downloads: Vec<Download> = Vec::new();
    for line in text.lines() {
        if let Some(download) = download_of(line) {
            downloads.push(download);
            continue;
        }
        if let Some(name) = header_of(line) {
            doc.sections.push(Section {
                name: name.to_string(),
                downloads: std::mem::take(&mut downloads),
                entries: Vec::new(),
            });
            continue;
        }
        match doc.sections.last_mut() {
            Some(section) => {
                if let Some(entry) = entry_of(line) {
                    section.entries.push(entry);
                }
            }
            None => doc.preamble.push(line.to_string()),
        }
    }
    doc
}

/// The line ending the text uses: CRLF when it has any, LF otherwise.
///
/// A Windows checkout has CRLF (`.gitattributes` normalizes the repository
/// to LF, not the working tree); imposing LF would turn a one-line edit into
/// a whole-file diff.
fn ending(text: &str) -> &'static str {
    if text.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

/// `lines` joined back into text, with a trailing newline.
fn join_lines(lines: &[&str], eol: &str) -> String {
    let mut out = String::new();
    for line in lines {
        out.push_str(line);
        out.push_str(eol);
    }
    out
}

/// The file with section `name` removed, together with the comment and blank
/// lines above its header.
///
/// Those lines are the section's own documentation - `; Download:` and
/// `; Quality:` live there - so they go with it. The buffering is what makes
/// that work: a comment is held back until the next real line says whose it
/// is, and it is kept when that line belongs to a section that stays. The
/// console's `/api/delpreset` is the caller.
pub fn remove_section(text: &str, name: &str) -> String {
    let eol = ending(text);
    let mut keep: Vec<&str> = Vec::new();
    let mut pending: Vec<&str> = Vec::new();
    let mut skip = false;
    for line in text.lines() {
        let trimmed = line.trim();
        // Only `;` comments are held back: a `#` comment is an ordinary line
        // to the delete, and it stays where it is.
        if trimmed.starts_with(';') || trimmed.is_empty() {
            pending.push(line);
            continue;
        }
        if let Some(header) = header_of(line) {
            skip = header == name;
            if !skip {
                keep.append(&mut pending);
                keep.push(line);
            }
            pending.clear();
            continue;
        }
        if skip {
            continue;
        }
        keep.append(&mut pending);
        keep.push(line);
    }
    // Trailing comments belong to no section, so they stay - unless the last
    // section was the one being removed, in which case they were its own.
    if !skip {
        keep.append(&mut pending);
    }
    join_lines(&keep, eol)
}

/// `text` with `block` appended, one line per element, as `Add-Content` does.
pub fn append_section(text: &str, block: &str) -> String {
    let eol = ending(text);
    let mut out = text.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push_str(eol);
    }
    for line in block.lines() {
        out.push_str(line);
        out.push_str(eol);
    }
    out
}

/// The preset with `overrides` applied to `section`.
///
/// The override lines are printed right after the section header and the
/// original lines for those keys are dropped, so the section's own flags
/// cannot fight the candidate's. Every other line - comments, blank lines,
/// the other sections - passes through untouched.
pub fn write_candidate(text: &str, section: &str, overrides: &[Entry]) -> String {
    let eol = ending(text);
    // Last value per key wins, but the printed order is the order given.
    let mut lines: Vec<String> = Vec::new();
    let mut keys: Vec<String> = Vec::new();
    for entry in overrides {
        let key = entry.key.trim().to_string();
        let line = Entry::new(key.clone(), entry.value.trim()).line();
        match keys.iter().position(|k| *k == key) {
            Some(i) => lines[i] = line,
            None => {
                keys.push(key);
                lines.push(line);
            }
        }
    }
    let mut out = String::new();
    let mut current = String::new();
    for line in text.lines() {
        if let Some(name) = header_of(line) {
            current = name.to_string();
            out.push_str(line);
            out.push_str(eol);
            if current == section {
                for override_line in &lines {
                    out.push_str(override_line);
                    out.push_str(eol);
                }
            }
            continue;
        }
        if current == section {
            if let Some(entry) = entry_of(line) {
                if keys.iter().any(|k| *k == entry.key) {
                    continue;
                }
            }
        }
        out.push_str(line);
        out.push_str(eol);
    }
    out
}

/// The preset with `overlay`'s entries merged into it.
///
/// An existing key is replaced where it stands, a key the section does not
/// have is appended after its last entry, and a section the preset does not
/// have at all is appended whole - which is how
/// `data/tuned/models.effective.ini` is built. Comments and blank lines of
/// the preset survive; the overlay's do not.
pub fn write_overlay(preset: &str, overlay: &str) -> String {
    let eol = ending(preset);
    // What the overlay says, per section, in the order it says it.
    let mut order: Vec<String> = Vec::new();
    let mut entries: Vec<(String, String, String)> = Vec::new(); // (section, key, value)
    let mut overlay_sections: Vec<String> = Vec::new();
    let mut current = String::new();
    for line in overlay.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with(';') || trimmed.starts_with('#') {
            continue;
        }
        if let Some(name) = header_of(line) {
            current = name.to_string();
            if !overlay_sections.iter().any(|s| s == &current) {
                overlay_sections.push(current.clone());
            }
            continue;
        }
        let Some(entry) = entry_of(line) else { continue };
        if !order.iter().any(|k| k == &entry.key) {
            order.push(entry.key.clone());
        }
        match entries.iter_mut().find(|(s, k, _)| s == &current && k == &entry.key) {
            Some(slot) => slot.2 = entry.value,
            None => entries.push((current.clone(), entry.key, entry.value)),
        }
    }

    // Lines rather than one string, so a section's missing keys can go in
    // right after its last entry - before the comments that introduce the
    // next section, which belong to that one.
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut seen: Vec<String> = Vec::new();
    // Where the current section's missing keys go, and the keys it has.
    let mut anchor = 0;
    let mut present: Vec<String> = Vec::new();
    let append_missing = |out: &mut Vec<String>, section: &str, anchor: usize, present: &[String]| {
        let missing: Vec<String> = entries
            .iter()
            .filter(|(s, k, _)| s == section && !present.iter().any(|p| p == k))
            .map(|(_, key, value)| format!("{key} = {value}"))
            .collect();
        for (offset, line) in missing.into_iter().enumerate() {
            out.insert(anchor + offset, line);
        }
    };
    for line in preset.lines() {
        if let Some(name) = header_of(line) {
            append_missing(&mut out, &current, anchor, &present);
            current = name.to_string();
            if !seen.iter().any(|s| s == &current) {
                seen.push(current.clone());
            }
            out.push(line.to_string());
            anchor = out.len();
            present.clear();
            continue;
        }
        if let Some(entry) = entry_of(line) {
            present.push(entry.key.clone());
            match entries.iter().find(|(s, k, _)| s == &current && k == &entry.key) {
                Some((_, _, value)) => out.push(format!("{} = {}", entry.key, value)),
                None => out.push(line.to_string()),
            }
            anchor = out.len();
            continue;
        }
        out.push(line.to_string());
    }
    append_missing(&mut out, &current, anchor, &present);
    // Written with the file's own ending: on a CRLF checkout a merged line
    // that ends in LF leaves the effective preset mixed, which every later
    // diff has to step around.
    let mut out: String = out.into_iter().map(|line| format!("{line}{eol}")).collect();
    // A section the preset does not have is appended whole, at the end.
    for section in &overlay_sections {
        if seen.iter().any(|s| s == section) {
            continue;
        }
        out.push_str(eol);
        let _ = write!(out, "[{section}]{eol}");
        for (_, key, value) in entries.iter().filter(|(s, _, _)| s == section) {
            let _ = write!(out, "{key} = {value}{eol}");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRESET: &str = "\
; header comment
version = 1

[*]
jinja = true

; Coder - the default.
; Download: someone/model-GGUF -> model-Q4_K_M.gguf
; Download: someone/model-GGUF -> mtp-model-Q4_0.gguf
; Quality: 79
[model-a]
model = model-Q4_K_M.gguf
spec-draft-model = mtp-model-Q4_0.gguf
ctx-size = 236544
n-cpu-moe = 36

; A tiny one for smoke tests.
; Download: tiny/tiny-GGUF -> tiny.gguf
[tiny]
model = tiny.gguf
ctx-size = 8192
n-cpu-moe = 0
";

    #[test]
    fn entries_hold_what_they_say() {
        let e = Entry { key: "ctx-size".into(), value: "236544".into() };
        assert_eq!(e.key, "ctx-size");
        assert_eq!(e.line(), "ctx-size = 236544");
    }

    #[test]
    fn parse_keeps_the_preamble_the_sections_and_their_order() {
        let doc = parse(PRESET);
        assert!(doc.preamble.iter().any(|l| l == "version = 1"), "{:?}", doc.preamble);
        assert_eq!(doc.sections.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
                   vec!["*", "model-a", "tiny"]);
        // The shared section is a section, but not a preset.
        assert_eq!(doc.preset_names(), vec!["model-a".to_string(), "tiny".to_string()]);
    }

    #[test]
    fn download_comments_belong_to_the_section_below_them() {
        let doc = parse(PRESET);
        let a = doc.get("model-a").expect("model-a");
        assert_eq!(a.downloads.len(), 2, "a coder and its draft: {:?}", a.downloads);
        assert_eq!(a.downloads[0].repo, "someone/model-GGUF");
        assert_eq!(a.downloads[0].file, "model-Q4_K_M.gguf");
        assert_eq!(a.downloads[1].file, "mtp-model-Q4_0.gguf");
        // The comment above the *next* section is not this one's.
        let tiny = doc.get("tiny").expect("tiny");
        assert_eq!(tiny.downloads.len(), 1);
        assert_eq!(tiny.downloads[0].file, "tiny.gguf");
        // The preamble has none.
        assert!(doc.get("*").expect("*").downloads.is_empty());
    }

    #[test]
    fn values_are_trimmed_and_a_later_duplicate_wins() {
        let doc = parse("[a]\nctx-size= 4096 \n; ctx-size = 1\nctx-size = 8192\nkey = x\n");
        assert_eq!(doc.value("a", "ctx-size"), Some("8192"));
        assert_eq!(doc.value("a", "key"), Some("x"));
        assert_eq!(doc.value("a", "nope"), None);
        assert_eq!(doc.value("b", "key"), None);
    }

    #[test]
    fn a_header_only_counts_at_column_zero() {
        let doc = parse("[a]\n  [b]\nkey = 1\n");
        assert_eq!(doc.sections.len(), 1);
        assert_eq!(doc.sections[0].name, "a");
        // The indented one is not an entry either: no `=`, so it is ignored.
        assert_eq!(doc.sections[0].entries.len(), 1);
    }

    #[test]
    fn the_editors_keep_the_line_ending_they_were_given() {
        let crlf = "[a]\r\nkey = 1\r\n";
        assert!(remove_section(crlf, "b").contains("\r\n"));
        assert!(append_section(crlf, "[b]\nkey = 2").contains("\r\n"));
        assert!(write_candidate(crlf, "a", &[Entry::new("key", "2")]).contains("\r\n"));
        // ...and LF stays LF.
        let lf = "[a]\nkey = 1\n";
        assert!(!remove_section(lf, "b").contains('\r'));
    }

    #[test]
    fn removing_a_section_takes_its_comments_with_it() {
        let out = remove_section(PRESET, "tiny");
        assert!(!out.contains("[tiny]"));
        assert!(!out.contains("tiny.gguf"));
        assert!(!out.contains("A tiny one for smoke tests."), "its comment goes too:\n{out}");
        // Everything else survives, comments included.
        assert!(out.contains("[model-a]"));
        assert!(out.contains("; Download: someone/model-GGUF -> model-Q4_K_M.gguf"));
        assert!(out.contains("; Quality: 79"));
        assert!(out.contains("version = 1"));
        // The blank line that separated the two sections stays with the
        // survivor's comment block, so no section lost its own comment.
        let reparsed = parse(&out);
        assert_eq!(reparsed.preset_names(), vec!["model-a".to_string()]);
    }

    #[test]
    fn removing_a_section_that_is_not_there_changes_nothing() {
        assert_eq!(remove_section(PRESET, "nope"), PRESET);
    }

    #[test]
    fn removing_the_last_section_takes_the_comment_block_above_it() {
        // The blank line and the note belong to `[b]` - they sit directly
        // above its header, which is where its `; Download:` lives - so they
        // go with it. The comments *after* the last section go too, since
        // they are that section's tail.
        let text = "[a]\nkey = 1\n\n; trailing note\n[b]\nkey = 2\n; after b\n";
        assert_eq!(remove_section(text, "b"), "[a]\nkey = 1\n");
    }

    #[test]
    fn appending_a_section_adds_a_trailing_newline_when_the_file_lacks_one() {
        let out = append_section("[a]\nkey = 1", "\n; Added from the web console\n[b]\nkey = 2");
        assert_eq!(out, "[a]\nkey = 1\n\n; Added from the web console\n[b]\nkey = 2\n");
        assert_eq!(parse(&out).preset_names(), vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn a_candidate_prints_the_overrides_after_its_header_and_drops_the_old_keys() {
        let out = write_candidate(
            PRESET,
            "model-a",
            &[Entry::new("ctx-size", "32768"), Entry::new("n-cpu-moe", "20")],
        );
        let section = &parse(&out).sections[1];
        assert_eq!(section.name, "model-a");
        // The overrides come first, in the order given...
        assert_eq!(section.entries[0], Entry::new("ctx-size", "32768"));
        assert_eq!(section.entries[1], Entry::new("n-cpu-moe", "20"));
        // ...and the section's own copies of those keys are gone.
        assert_eq!(section.value("ctx-size"), Some("32768"));
        assert_eq!(section.entries.iter().filter(|e| e.key == "ctx-size").count(), 1);
        assert_eq!(section.value("n-cpu-moe"), Some("20"));
        assert_eq!(section.value("model"), Some("model-Q4_K_M.gguf"));
        // The other sections and every comment are untouched.
        assert!(out.contains("; Quality: 79"));
        assert_eq!(parse(&out).value("tiny", "ctx-size"), Some("8192"));
    }

    #[test]
    fn a_candidate_can_add_a_key_the_section_never_had() {
        let out = write_candidate(PRESET, "tiny", &[Entry::new("n-gpu-layers", "99")]);
        let tiny = parse(&out).get("tiny").cloned().expect("tiny");
        assert_eq!(tiny.value("n-gpu-layers"), Some("99"));
        assert_eq!(tiny.value("ctx-size"), Some("8192"));
    }

    #[test]
    fn an_overlay_replaces_in_place_and_keeps_the_comments() {
        let overlay = "; Auto-tuned by golem tune\n[model-a]\nctx-size = 65536\nfit = off\n";
        let out = write_overlay(PRESET, overlay);
        let doc = parse(&out);
        assert_eq!(doc.value("model-a", "ctx-size"), Some("65536"));
        // A key the section does not have is appended to it - after its last
        // entry, not after the next section's comments. Dropping it serves
        // a setup other than the one measured:
        // the sweep ran with `fit = off`, the effective preset without it.
        assert_eq!(doc.value("model-a", "fit"), Some("off"));
        assert!(out.contains("n-cpu-moe = 36\nfit = off\n\n; A tiny one"), "{out}");
        // The untouched keys of the section stay where they were.
        assert_eq!(doc.value("model-a", "model"), Some("model-Q4_K_M.gguf"));
        assert_eq!(doc.value("model-a", "n-cpu-moe"), Some("36"));
        assert!(out.contains("; Quality: 79"), "the preset's comments survive:\n{out}");
        assert!(!out.contains("Auto-tuned"), "the overlay's do not:\n{out}");
        // Only the overridden key is rewritten; the rest is byte-identical.
        assert!(out.contains("ctx-size = 65536\n"));
        assert!(!out.contains("ctx-size = 236544"));
        // And the replacement happens in place, not at the end.
        let lines: Vec<&str> = out.lines().collect();
        let ctx = lines.iter().position(|l| l.starts_with("ctx-size")).expect("ctx");
        let draft = lines.iter().position(|l| l.starts_with("spec-draft-model")).expect("draft");
        assert!(ctx > draft, "ctx-size should have replaced the line it was on");
    }

    #[test]
    fn an_overlay_appends_a_section_the_preset_does_not_have() {
        let overlay = "[new-one]\nctx-size = 4096\nn-cpu-moe = 0\n";
        let out = write_overlay(PRESET, overlay);
        let doc = parse(&out);
        assert_eq!(doc.preset_names(), vec!["model-a".to_string(), "tiny".to_string(), "new-one".to_string()]);
        assert_eq!(doc.value("new-one", "ctx-size"), Some("4096"));
        // Appended after the file's own content, with a blank line before it.
        assert!(out.trim_end().ends_with("n-cpu-moe = 0"));
        assert!(out.contains("\n[new-one]\n"));
    }

    #[test]
    fn a_merged_file_never_ends_up_with_two_kinds_of_line_ending() {
        // The checkout is CRLF (`.gitattributes` normalizes the repository,
        // not the working tree). A rewritten line that ends in LF would make
        // the effective preset mixed - and every diff of it noisy forever.
        let preset = PRESET.replace('\n', "\r\n");
        let overlay = "[model-a]\r\nctx-size = 65536\r\n[brand-new]\r\nkey = 1\r\n";
        let out = write_overlay(&preset, overlay);
        assert!(!out.contains("\n\n"), "{out:?}");
        assert_eq!(out.matches("\r\n").count(), out.matches('\n').count(), "every LF is a CRLF");
        assert!(out.contains("ctx-size = 65536\r\n"), "{out:?}");
        assert!(out.contains("[brand-new]\r\n"), "{out:?}");
    }

    #[test]
    fn a_later_key_in_the_overlay_wins() {
        let out = write_overlay(PRESET, "[tiny]\nctx-size = 1024\nctx-size = 2048\n");
        assert_eq!(parse(&out).value("tiny", "ctx-size"), Some("2048"));
        assert_eq!(parse(&out).get("tiny").expect("tiny").entries
                       .iter().filter(|e| e.key == "ctx-size").count(), 1);
    }
}
