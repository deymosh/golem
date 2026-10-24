//! The regex subset the engine adapters actually use, and nothing more.
//!
//! The adapters describe their GitHub release assets as regexes (see
//! `windows_asset_cuda` in engines/llama.cpp/engine.ini). The binary has no
//! regex crate and will not gain one: the patterns are ours,
//! written by hand, and this is the subset they use — literals, `.`, the three
//! quantifiers, the three escapes, an escaped literal, `^`/`$`, and character
//! classes.
//!
//! Everything outside that subset is an error rather than a silent literal. A
//! pattern that half-matched would install the wrong build and never say so —
//! the failure this repository's comments keep warning about — so `(` and `|`
//! and a stray `\q` are loud here, not quietly part of the text being matched.

/// Whether `pattern` matches anywhere in `text`: a search, not a full match,
/// with `^` and `$` anchoring only where the pattern writes them.
pub fn matches(pattern: &str, text: &str) -> Result<bool, String> {
    let atoms = compile(pattern)?;
    let chars: Vec<char> = text.chars().collect();
    // A search: the match may start anywhere, and — unless `$` says otherwise —
    // may end before the text does.
    for start in 0..=chars.len() {
        if match_at(&atoms, 0, &chars, start) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// One atom of a compiled pattern, with the repetition that follows it.
struct Atom {
    kind: Kind,
    quant: Quant,
}

enum Kind {
    Literal(char),
    /// `.` — any single character.
    Any,
    /// `\d` — a digit.
    Digit,
    /// `\w` — a word character.
    Word,
    /// `\s` — whitespace.
    Space,
    /// `[abc]`, `[a-z]`, `[^abc]`.
    Class { negated: bool, ranges: Vec<(char, char)> },
    /// `^` — start of text (zero-width).
    Start,
    /// `$` — end of text (zero-width).
    End,
}

enum Quant {
    /// No quantifier: exactly one.
    One,
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

/// Turn a pattern into atoms, rejecting anything outside the subset.
///
/// The rejection is the point of this function: a construct this matcher does
/// not implement must not fall through as a literal, because then the pattern
/// would appear to work while matching the wrong names.
fn compile(pattern: &str) -> Result<Vec<Atom>, String> {
    let chars: Vec<char> = pattern.chars().collect();
    let mut atoms: Vec<Atom> = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        let kind = match c {
            '.' => Kind::Any,
            '^' => Kind::Start,
            '$' => Kind::End,
            '*' | '+' | '?' => {
                return Err(format!(
                    "the asset pattern uses '{c}' with nothing to repeat — a quantifier needs an atom before it"
                ))
            }
            '(' | ')' => {
                return Err(format!(
                    "the asset pattern uses '{c}' — groups are not supported by this matcher"
                ))
            }
            '|' => {
                return Err(
                    "the asset pattern uses '|' — alternation is not supported by this matcher"
                        .to_string(),
                )
            }
            '{' | '}' => {
                return Err(format!(
                    "the asset pattern uses '{c}' — counted repetition is not supported by this matcher"
                ))
            }
            '[' => parse_class(&chars, &mut i)?,
            '\\' => {
                let Some(&esc) = chars.get(i) else {
                    return Err(
                        "the asset pattern ends with a trailing '\\' — an escape needs a character after it"
                            .to_string(),
                    );
                };
                i += 1;
                match esc {
                    'd' => Kind::Digit,
                    'w' => Kind::Word,
                    's' => Kind::Space,
                    '.' | '-' | '+' | '*' | '?' | '^' | '$' | '[' | ']' | '(' | ')' | '|'
                    | '{' | '}' | '\\' => Kind::Literal(esc),
                    other => {
                        return Err(format!(
                            "the asset pattern uses '\\{other}' — this escape is not supported by this matcher"
                        ))
                    }
                }
            }
            literal => Kind::Literal(literal),
        };

        let quant = match chars.get(i) {
            Some('*') => {
                i += 1;
                Quant::ZeroOrMore
            }
            Some('+') => {
                i += 1;
                Quant::OneOrMore
            }
            Some('?') => {
                i += 1;
                Quant::ZeroOrOne
            }
            _ => Quant::One,
        };
        // An anchor is zero-width; repeating it is meaningless and .NET's own
        // answer there is obscure, so say so rather than guess.
        if !matches!(quant, Quant::One) && matches!(&kind, Kind::Start | Kind::End) {
            return Err(
                "the asset pattern repeats an anchor ('^' or '$') — that is not supported by this matcher"
                    .to_string(),
            );
        }
        atoms.push(Atom { kind, quant });
    }
    Ok(atoms)
}

/// A character class body, the `[` already consumed and `*i` at the first
/// character of the body. `[abc]`, `[a-z]` and `[^abc]`; an unterminated `[` or
/// an empty `[]` is an error, not a literal bracket.
fn parse_class(chars: &[char], i: &mut usize) -> Result<Kind, String> {
    let mut negated = false;
    if chars.get(*i) == Some(&'^') {
        negated = true;
        *i += 1;
    }
    if chars.get(*i) == Some(&']') {
        return Err(
            "the asset pattern has an empty character class '[]' — this matcher does not support that"
                .to_string(),
        );
    }
    let mut ranges: Vec<(char, char)> = Vec::new();
    loop {
        let Some(&c) = chars.get(*i) else {
            return Err(
                "the asset pattern has an unterminated '[' — a character class must be closed with ']'"
                    .to_string(),
            );
        };
        if c == ']' {
            *i += 1;
            return Ok(Kind::Class { negated, ranges });
        }
        // `a-z` is a range; a `-` at the end of the class (before `]`) is a
        // literal, which is how `[a-]` reads everywhere else too.
        let is_range = chars.get(*i + 1) == Some(&'-')
            && matches!(chars.get(*i + 2), Some(&hi) if hi != ']');
        if is_range {
            let hi = chars[*i + 2];
            if hi < c {
                return Err(format!(
                    "the asset pattern has a reversed character range '{c}-{hi}' — this matcher does not support that"
                ));
            }
            ranges.push((c, hi));
            *i += 3;
        } else {
            ranges.push((c, c));
            *i += 1;
        }
    }
}

/// Match `atoms[ai..]` against `chars` from `pos`, allowing the match to end
/// anywhere — the trailing text is what a `$` in the pattern is for.
fn match_at(atoms: &[Atom], ai: usize, chars: &[char], pos: usize) -> bool {
    if ai == atoms.len() {
        return true;
    }
    let atom = &atoms[ai];
    match atom.quant {
        Quant::One => match one(atom, chars, pos) {
            Some(next) => match_at(atoms, ai + 1, chars, next),
            None => false,
        },
        Quant::ZeroOrOne => {
            // Greedy: the atom once, then without it.
            if let Some(next) = one(atom, chars, pos) {
                if match_at(atoms, ai + 1, chars, next) {
                    return true;
                }
            }
            match_at(atoms, ai + 1, chars, pos)
        }
        Quant::ZeroOrMore | Quant::OneOrMore => {
            // The longest run of the atom first, then shorter — the split
            // `^a*ab$` needs, where the greedy `a*` has to give one `a` back.
            let mut positions = vec![pos];
            let mut cur = pos;
            while let Some(next) = one(atom, chars, cur) {
                if next == cur {
                    break; // a zero-width atom cannot make progress
                }
                positions.push(next);
                cur = next;
            }
            let min = if matches!(atom.quant, Quant::OneOrMore) { 1 } else { 0 };
            let mut count = positions.len() - 1;
            if count < min {
                return false;
            }
            loop {
                if match_at(atoms, ai + 1, chars, positions[count]) {
                    return true;
                }
                if count == min {
                    return false;
                }
                count -= 1;
            }
        }
    }
}

/// One match of `atom` at `pos`: the position after it, or `None`. An anchor
/// answers `pos` itself — zero-width.
fn one(atom: &Atom, chars: &[char], pos: usize) -> Option<usize> {
    match &atom.kind {
        Kind::Start => (pos == 0).then_some(pos),
        Kind::End => (pos == chars.len()).then_some(pos),
        Kind::Literal(c) => (chars.get(pos) == Some(c)).then_some(pos + 1),
        Kind::Any => (pos < chars.len()).then_some(pos + 1),
        // ASCII digits, word characters and whitespace: the names these
        // patterns are applied to are GitHub asset names, which are ASCII. The
        // .NET escapes are Unicode-wide, but nothing here ever sees the rest.
        Kind::Digit => chars.get(pos).filter(|c| c.is_ascii_digit()).map(|_| pos + 1),
        Kind::Word => chars
            .get(pos)
            .filter(|c| c.is_ascii_alphanumeric() || **c == '_')
            .map(|_| pos + 1),
        Kind::Space => chars.get(pos).filter(|c| c.is_whitespace()).map(|_| pos + 1),
        Kind::Class { negated, ranges } => {
            let c = chars.get(pos)?;
            let inside = ranges.iter().any(|(lo, hi)| c >= lo && c <= hi);
            (inside != *negated).then_some(pos + 1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The three patterns `engines/llama.cpp/engine.ini` actually carries.
    const CUDA_ASSET: &str = r"^llama-.*bin-win-cuda-13\.\d+-x64\.zip$";
    const CUDA_RUNTIME: &str = r"^cudart-llama-bin-win-cuda-13\.\d+-x64\.zip$";
    const ROCM_ASSET: &str = r"^llama-.*bin-win-rocm-\d+\.\d+-x64\.zip$";

    #[test]
    fn the_real_cuda_pattern_picks_the_server_zip_and_not_its_neighbours() {
        assert!(matches(CUDA_ASSET, "llama-b11379-bin-win-cuda-13.1-x64.zip").unwrap());
        // The runtime zip is a different asset, matched by its own key.
        assert!(!matches(CUDA_ASSET, "cudart-llama-bin-win-cuda-13.1-x64.zip").unwrap());
        // A different CUDA major is not this build's asset.
        assert!(!matches(CUDA_ASSET, "llama-b11379-bin-win-cuda-12.4-x64.zip").unwrap());
    }

    #[test]
    fn the_real_runtime_and_rocm_patterns_match_their_assets() {
        assert!(matches(CUDA_RUNTIME, "cudart-llama-bin-win-cuda-13.1-x64.zip").unwrap());
        assert!(matches(ROCM_ASSET, "llama-b11379-bin-win-rocm-6.4-x64.zip").unwrap());
        // The `.` in `\d+\.\d+` is literal, so a two-part version is required.
        assert!(!matches(ROCM_ASSET, "llama-b11379-bin-win-rocm-64-x64.zip").unwrap());
    }

    #[test]
    fn without_an_anchor_the_search_is_unanchored() {
        // This is `-match`'s semantics: a search, not a full match.
        assert!(matches("win-cuda", "llama-bin-win-cuda-13-x64.zip").unwrap());
        assert!(!matches("win-cuda", "llama-bin-win-rocm-13-x64.zip").unwrap());
    }

    #[test]
    fn a_star_backtracks_to_let_the_rest_match() {
        // `a*` would take all three `a`s, leaving nothing for the literal `a`;
        // it has to give one back.
        assert!(matches("^a*ab$", "aaab").unwrap());
        assert!(matches("^a*ab$", "ab").unwrap(), "zero is also a valid count");
        assert!(!matches("^a*ab$", "aaac").unwrap());
    }

    #[test]
    fn a_plus_needs_at_least_one_and_a_question_takes_zero() {
        assert!(!matches(r"^\d+$", "").unwrap(), "one or more means at least one");
        assert!(matches(r"^\d+$", "5").unwrap());
        assert!(matches(r"^\d+$", "123").unwrap());
        assert!(!matches(r"^\d+$", "abc").unwrap());
        assert!(matches("^colou?r$", "color").unwrap(), "`?` accepts zero");
        assert!(matches("^colou?r$", "colour").unwrap());
        assert!(!matches("^colou?r$", "colouur").unwrap());
    }

    #[test]
    fn classes_negations_and_ranges_do_what_they_say() {
        assert!(matches("^[abc]$", "b").unwrap());
        assert!(!matches("^[abc]$", "d").unwrap());
        assert!(matches("^[^abc]$", "d").unwrap());
        assert!(!matches("^[^abc]$", "a").unwrap());
        assert!(matches("^[a-z]$", "m").unwrap());
        assert!(!matches("^[a-z]$", "M").unwrap());
        // A class is one character of a longer pattern, as in `cuda-1[0-9]`.
        assert!(matches(r"^cuda-1[0-9]\.[0-9]$", "cuda-13.1").unwrap());
        assert!(!matches(r"^cuda-1[0-9]\.[0-9]$", "cuda-23.1").unwrap());
    }

    #[test]
    fn an_escaped_literal_matches_the_character_itself() {
        assert!(matches(r"^a\.b$", "a.b").unwrap());
        assert!(!matches(r"^a\.b$", "axb").unwrap(), "an escaped dot is not any char");
        assert!(matches(r"^a\*b$", "a*b").unwrap());
        assert!(matches(r"^\[x\]$", "[x]").unwrap());
        assert!(matches(r"^a\\b$", r"a\b").unwrap());
    }

    #[test]
    fn a_trailing_dollar_anchors_the_end_of_the_text() {
        assert!(matches("zip$", "x.zip").unwrap());
        assert!(!matches("zip$", "x.zipy").unwrap());
        assert!(matches("^llama", "llama-x").unwrap());
        assert!(!matches("^llama", "x-llama").unwrap());
    }

    #[test]
    fn every_unsupported_construct_is_rejected_rather_than_ignored() {
        for (pattern, needle) in [
            ("a(b)", "groups"),
            ("a)b", "groups"),
            ("a|b", "alternation"),
            ("a{2}", "counted repetition"),
            ("a}", "counted repetition"),
            (r"a\q", r"\q"),
            ("*a", "nothing to repeat"),
            ("a++", "nothing to repeat"),
            ("[a", "unterminated"),
            ("[]", "empty character class"),
            ("[^]", "empty character class"),
            ("^+a", "anchor"),
        ] {
            let err = matches(pattern, "x").expect_err(pattern);
            assert!(err.contains(needle), "{pattern}: {err}");
        }
    }

    #[test]
    fn a_rejected_construct_never_reads_as_a_literal() {
        // The whole point: `(x)` must not match the text `(x)`.
        assert!(matches("(x)", "(x)").is_err());
        assert!(matches("a|b", "a").is_err());
    }
}
