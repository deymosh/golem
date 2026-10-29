//! `golem.toml`: the configuration file the binary owns.
//!
//! A TOML **subset** - flat `key = value` pairs, `#` comments, strings,
//! integers, floats and booleans. That is all the configuration needs, and a
//! full parser is a dependency this repository does not want (the same reason
//! `json.rs` is hand-written). Anything richer - tables, arrays, inline
//! tables - is a parse error rather than a silent misreading, because a
//! config file that half-works is worse than one that refuses.
//!
//! Reading is line-oriented so that *writing* can be too: a key is set by
//! replacing the one line that assigns it. That is what keeps the comments
//! exactly where the user put them - and in this file the comments are the
//! documentation, which is the reason the file is TOML rather than JSON in
//! the first place.

/// A parsed configuration file, kept as its own lines.
///
/// The lines are held rather than a map so that writing preserves everything
/// the reader did not care about: blank lines, comments, ordering, and the
/// line ending the file already uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    lines: Vec<String>,
    /// The line ending the file uses, so a rewrite does not churn every line.
    eol: String,
}

impl Document {
    /// Parse `text`, or say which line is wrong.
    pub fn parse(text: &str) -> Result<Document, String> {
        // Notepad's "UTF-8 with BOM" puts U+FEFF before the first key.
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let eol = if text.contains("\r\n") { "\r\n" } else { "\n" }.to_string();
        let mut lines = Vec::new();
        for (number, raw) in text.lines().enumerate() {
            let line = raw.trim_end_matches('\r');
            let body = strip_comment(line).trim();
            if body.is_empty() {
                lines.push(line.to_string());
                continue;
            }
            let Some((key, value)) = body.split_once('=') else {
                return Err(format!("line {}: expected `key = value`, got {body:?}", number + 1));
            };
            let key = key.trim();
            if !is_key(key) {
                return Err(format!("line {}: {key:?} is not a bare key", number + 1));
            }
            parse_value(value.trim())
                .map_err(|e| format!("line {}: {e}", number + 1))?;
            lines.push(line.to_string());
        }
        Ok(Document { lines, eol })
    }

    /// The value of `key`, as text with any quotes removed.
    ///
    /// Text, not a typed value, on purpose: every reader in this binary
    /// parses its own key (a port, a path, a list of flags), and `5` and
    /// `"5"` in the file mean the same thing to all of them.
    pub fn get(&self, key: &str) -> Option<String> {
        for line in &self.lines {
            let body = strip_comment(line).trim();
            let Some((k, v)) = body.split_once('=') else { continue };
            if k.trim() == key {
                return unquote(v.trim());
            }
        }
        None
    }

    /// Set `key`, replacing the line that assigns it or appending one.
    ///
    /// A commented-out assignment is left alone: it is documentation of an
    /// alternative, not the setting.
    ///
    /// Appending is the rare case - `tune --adopt` rewrites two keys the file
    /// already has, so it replaces in place - and it adds no blank line: the
    /// separator belongs to whoever wrote the comment above it, not to this.
    pub fn set(&mut self, key: &str, value: &str) {
        let rendered = format!("{key} = {}", render_value(value));
        for line in self.lines.iter_mut() {
            let body = strip_comment(line).trim();
            let Some((k, _)) = body.split_once('=') else { continue };
            if k.trim() == key {
                *line = rendered;
                return;
            }
        }
        self.lines.push(rendered);
    }

    /// The file's text, with the line ending it was read with.
    pub fn text(&self) -> String {
        let mut out = self.lines.join(&self.eol);
        // `lines()` drops a trailing newline; a config file keeps one.
        out.push_str(&self.eol);
        out
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|line| strip_comment(line).trim().is_empty())
    }
}

/// Everything before an unquoted `#`.
fn strip_comment(line: &str) -> &str {
    let mut in_string = false;
    let mut escaped = false;
    for (at, c) in line.char_indices() {
        match c {
            // `\"` inside a string is a quote, not its end.
            _ if escaped => escaped = false,
            '\\' if in_string => escaped = true,
            '"' => in_string = !in_string,
            '#' if !in_string => return &line[..at],
            _ => {}
        }
    }
    line
}

/// A bare TOML key: letters, digits, `_` and `-`.
fn is_key(key: &str) -> bool {
    !key.is_empty()
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Whether `value` is a value this subset accepts.
fn parse_value(value: &str) -> Result<(), String> {
    if value.is_empty() {
        return Err("a value is missing".to_string());
    }
    if let Some(rest) = value.strip_prefix('"') {
        // A basic string: no escapes beyond the ones TOML guarantees, and it
        // must close. `\"` is the one that matters for a path or a comment.
        let mut chars = rest.chars();
        let mut closed = false;
        while let Some(c) = chars.next() {
            match c {
                '\\' => {
                    chars.next();
                }
                '"' => {
                    closed = chars.next().is_none();
                    break;
                }
                _ => {}
            }
        }
        return if closed { Ok(()) } else { Err(format!("unterminated string {value:?}")) };
    }
    if value.starts_with('[') || value.starts_with('{') {
        return Err(format!("{value:?} is a table or an array, which this file does not use"));
    }
    if value == "true" || value == "false" {
        return Ok(());
    }
    if value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok() {
        return Ok(());
    }
    Err(format!("{value:?} is not a string, number or boolean (quote it?)"))
}

/// The value with its surrounding quotes removed.
fn unquote(value: &str) -> Option<String> {
    if let Some(inner) = value.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
        // Only the escapes this subset writes back.
        return Some(inner.replace("\\\"", "\"").replace("\\\\", "\\"));
    }
    Some(value.to_string())
}

/// How a value is written back: bare when it reads as a number or a boolean,
/// quoted otherwise. The same rule the recorder uses for a preset's flags,
/// and it keeps the file looking like the one a person would write.
fn render_value(value: &str) -> String {
    let bare = value == "true"
        || value == "false"
        || value.parse::<i64>().is_ok()
        || value.parse::<f64>().is_ok();
    if bare {
        return value.to_string();
    }
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Read the file at `path`, or `None` when it is not there.
pub fn read(path: &std::path::Path) -> Option<Result<Document, String>> {
    let text = std::fs::read_to_string(path).ok()?;
    Some(Document::parse(&text))
}

/// One value from a home's `golem.toml`.
///
/// The one place the rule lives: every reader goes through here rather than
/// parsing the file itself. A file that will not parse reads as absent - a
/// broken config should not take out a command that was only trying to read
/// one setting from it - and [`check`] is where it gets said out loud.
pub fn value(home: &std::path::Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(home.join("golem.toml")).ok()?;
    Document::parse(&text).ok()?.get(key)
}

/// `Err` with the parser's reason when the home's `golem.toml` exists and
/// will not parse. Every key would silently fall back to its default
/// otherwise, which reads as "my edit did nothing".
pub fn check(home: &std::path::Path) -> Result<(), String> {
    match read(&home.join("golem.toml")) {
        Some(Err(e)) => Err(format!("golem.toml: {e}")),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
# Golem - configuration
# The comments are the documentation.

# Where the engine listens.
LOCAL_AI_PORT = 8000
LOCAL_AI_API_KEY = \"golem-secret\"

# The default preset section.
MODEL_ALIAS = \"ornith-1.5-35b\"
#MODEL_ALIAS = \"qwen3.6-35b-a3b\"
";

    #[test]
    fn reads_values_without_their_quotes() {
        let doc = Document::parse(SAMPLE).expect("parse");
        assert_eq!(doc.get("LOCAL_AI_PORT").as_deref(), Some("8000"));
        assert_eq!(doc.get("LOCAL_AI_API_KEY").as_deref(), Some("golem-secret"));
        assert_eq!(doc.get("MODEL_ALIAS").as_deref(), Some("ornith-1.5-35b"));
        assert_eq!(doc.get("NOPE"), None);
    }

    #[test]
    fn a_commented_assignment_is_not_the_setting() {
        // The commented `MODEL_ALIAS` documents an alternative; the active
        // one is the answer, whichever order they are in.
        let doc = Document::parse("#MODEL_ALIAS = \"a\"\nMODEL_ALIAS = \"b\"\n").expect("parse");
        assert_eq!(doc.get("MODEL_ALIAS").as_deref(), Some("b"));
    }

    #[test]
    fn setting_a_key_keeps_every_comment_around_it() {
        let mut doc = Document::parse(SAMPLE).expect("parse");
        doc.set("MODEL_ALIAS", "qwen3.6-35b-a3b");
        let text = doc.text();
        assert!(text.contains("# Golem - configuration"), "{text}");
        assert!(text.contains("# Where the engine listens."), "{text}");
        assert!(text.contains("#MODEL_ALIAS = \"qwen3.6-35b-a3b\""), "the alternative stays: {text}");
        assert_eq!(doc.get("MODEL_ALIAS").as_deref(), Some("qwen3.6-35b-a3b"));
        // A second read of what was written gives the same answer.
        let again = Document::parse(&text).expect("reparse");
        assert_eq!(again.get("MODEL_ALIAS").as_deref(), Some("qwen3.6-35b-a3b"));
    }

    #[test]
    fn setting_a_key_the_file_does_not_have_appends_it() {
        let mut doc = Document::parse("A = 1\n").expect("parse");
        doc.set("B", "two");
        assert_eq!(doc.text(), "A = 1\nB = \"two\"\n");
        assert_eq!(doc.get("B").as_deref(), Some("two"));

        // An empty file gets a file rather than an error, and no leading
        // blank line from the append.
        let mut fresh = Document::parse("").expect("parse");
        fresh.set("ENGINE", "llama.cpp");
        assert_eq!(fresh.text(), "ENGINE = \"llama.cpp\"\n");
    }

    #[test]
    fn a_number_is_written_bare_and_a_string_quoted() {
        let mut doc = Document::parse("").expect("parse");
        doc.set("ctx", "249344");
        doc.set("alias", "ornith-1.5-35b");
        doc.set("flag", "true");
        assert_eq!(doc.text(), "ctx = 249344\nalias = \"ornith-1.5-35b\"\nflag = true\n");
        // And they all read back as text, which is what the callers want.
        assert_eq!(doc.get("ctx").as_deref(), Some("249344"));
        assert_eq!(doc.get("alias").as_deref(), Some("ornith-1.5-35b"));
        assert_eq!(doc.get("flag").as_deref(), Some("true"));
    }

    #[test]
    fn a_hash_inside_a_string_is_not_a_comment() {
        let doc = Document::parse("API_KEY = \"a#b\"\n").expect("parse");
        assert_eq!(doc.get("API_KEY").as_deref(), Some("a#b"));
    }

    #[test]
    fn the_line_ending_the_file_uses_is_the_one_kept() {
        let mut doc = Document::parse("A = 1\r\nB = 2\r\n").expect("parse");
        doc.set("A", "9");
        assert_eq!(doc.text(), "A = 9\r\nB = 2\r\n");
    }

    #[test]
    fn what_this_file_does_not_use_is_refused_rather_than_misread() {
        assert!(Document::parse("a = [1, 2]\n").is_err());
        assert!(Document::parse("a = { b = 1 }\n").is_err());
        assert!(Document::parse("a = \"unterminated\n").is_err());
        assert!(Document::parse("a = bare\n").is_err(), "an unquoted word is not a value");
        assert!(Document::parse("no equals sign\n").is_err());
        assert!(Document::parse("[table]\n").is_err(), "no tables in this subset");
    }

    #[test]
    fn an_empty_file_is_empty_rather_than_an_error() {
        let doc = Document::parse("").expect("parse");
        assert!(doc.is_empty());
        assert_eq!(doc.get("anything"), None);
        // And a file of comments alone is empty too.
        let comments = Document::parse("# just a note\n\n# another\n").expect("parse");
        assert!(comments.is_empty());
    }

    #[test]
    fn a_quoted_value_survives_a_round_trip() {
        let mut doc = Document::parse("").expect("parse");
        doc.set("EXTRA_ARGS", "--cpu-moe --mmproj \"x y\"");
        let text = doc.text();
        let again = Document::parse(&text).expect("reparse");
        assert_eq!(again.get("EXTRA_ARGS").as_deref(), Some("--cpu-moe --mmproj \"x y\""));
    }

    #[test]
    fn a_value_comes_from_golem_toml_only() {
        let dir = temp("value-first");
        // A `.env` beside it is not read at all.
        std::fs::write(dir.join(".env"), "MODEL_ALIAS=\"from-env\"\n").expect("write");
        std::fs::write(dir.join("golem.toml"), "# the floors\nTUNE_RUNS = 5\n").expect("write");
        assert_eq!(value(&dir, "TUNE_RUNS").as_deref(), Some("5"));
        assert_eq!(value(&dir, "MODEL_ALIAS"), None);
        // And a home with no file answers nothing rather than failing.
        let bare = temp("value-bare");
        assert_eq!(value(&bare, "TUNE_RUNS"), None);
        assert_eq!(check(&bare), Ok(()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&bare).ok();
    }

    #[test]
    fn a_broken_file_reads_as_absent_and_check_says_why() {
        // A mistyped config should not stop `status` from answering - but the
        // command has to be able to say that every key just fell back.
        let dir = temp("value-broken");
        std::fs::write(dir.join("golem.toml"), "LOCAL_AI_PORT = 8000\nnot a line\n").expect("write");
        assert_eq!(value(&dir, "LOCAL_AI_PORT"), None);
        let err = check(&dir).expect_err("a broken file is reported");
        assert!(err.contains("line 2"), "{err}");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-core-config-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        dir
    }
}
