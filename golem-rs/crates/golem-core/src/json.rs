//! A small JSON reader, no dependencies.
//!
//! `bench/results.jsonl` is our own file with a known shape: flat objects,
//! one nested object (`eval`), one nested object of scalars (`flags`). A
//! full JSON library would be a large dependency for that, and the project
//! keeps its hot path free of them (the shell scripts read the same file
//! with awk). What this reader must get right is exactly what the file
//! contains, plus the escapes a `note` can carry.
//!
//! Objects keep their key order (`Vec`, not a map): the row's order is part
//! of what makes the file readable, and rows are small enough that a linear
//! lookup costs nothing.
//!
//! [`quote`] is the other direction, and the only writer here: the console
//! serves a handful of small objects built by hand, and the one thing it
//! cannot hand-roll is the escaping - a Windows path or a model name with a
//! quote in it would otherwise produce a payload the browser refuses.

use std::fmt;

/// `value` as a JSON string literal, quotes included.
///
/// Control characters are escaped rather than passed through: a raw newline
/// inside a string is the one thing `JSON.parse` rejects outright
/// ("bad control character in string literal"), and a log tail - which the
/// console serves - is mostly newlines.
pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for c in value.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                use std::fmt::Write as _;
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A parsed JSON value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    /// Numbers are kept as written plus a parsed f64: the file stores both
    /// integers (ctx, vram_mib) and decimals (decode, params_b), and the
    /// printed form must survive a round trip untouched.
    Number { text: String, value: f64 },
    String(String),
    Array(Vec<Value>),
    /// Key order preserved.
    Object(Vec<(String, Value)>),
}

impl Value {
    /// A member of an object, or `None` for any other kind of value.
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::String(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Number { value, .. } => Some(*value),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// `null` and a missing key are the same answer for our rows: the
    /// recorder writes `null` for anything it could not measure, and both
    /// mean "no value" to every consumer.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    /// Byte offset in the input where parsing stopped.
    pub at: usize,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} (at byte {})", self.message, self.at)
    }
}

impl std::error::Error for ParseError {}

type Result<T> = std::result::Result<T, ParseError>;

/// Parse one complete JSON value. Trailing whitespace is fine; trailing
/// anything else is an error, so a truncated or doubled line is caught
/// instead of silently half-read.
pub fn parse(input: &str) -> Result<Value> {
    let mut p = Parser { bytes: input.as_bytes(), pos: 0 };
    p.skip_ws();
    let value = p.value()?;
    p.skip_ws();
    if p.pos != p.bytes.len() {
        return Err(p.err("trailing characters after the value"));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, message: &str) -> ParseError {
        ParseError { message: message.to_string(), at: self.pos }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect(&mut self, want: u8, what: &str) -> Result<()> {
        match self.peek() {
            Some(b) if b == want => {
                self.pos += 1;
                Ok(())
            }
            _ => Err(self.err(what)),
        }
    }

    fn literal(&mut self, word: &str, value: Value) -> Result<Value> {
        if self.bytes[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(self.err("unknown literal"))
        }
    }

    fn value(&mut self) -> Result<Value> {
        match self.peek() {
            None => Err(self.err("unexpected end of input")),
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-') | Some(b'0'..=b'9') => self.number(),
            Some(_) => Err(self.err("unexpected character")),
        }
    }

    fn object(&mut self) -> Result<Value> {
        self.expect(b'{', "expected '{'")?;
        let mut members = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Value::Object(members));
        }
        loop {
            self.skip_ws();
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':', "expected ':' after the key")?;
            self.skip_ws();
            let value = self.value()?;
            members.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Value::Object(members));
                }
                _ => return Err(self.err("expected ',' or '}' in the object")),
            }
        }
    }

    fn array(&mut self) -> Result<Value> {
        self.expect(b'[', "expected '['")?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Value::Array(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value()?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => {
                    self.pos += 1;
                }
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Value::Array(items));
                }
                _ => return Err(self.err("expected ',' or ']' in the array")),
            }
        }
    }

    fn string(&mut self) -> Result<String> {
        self.expect(b'"', "expected '\"'")?;
        let mut out = String::new();
        loop {
            let b = self.peek().ok_or_else(|| self.err("unterminated string"))?;
            match b {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let esc = self.peek().ok_or_else(|| self.err("unterminated escape"))?;
                    self.pos += 1;
                    match esc {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => out.push(self.unicode_escape()?),
                        _ => return Err(self.err("unknown escape")),
                    }
                }
                _ => {
                    // Copy the whole UTF-8 sequence: find its length from the
                    // leading byte, then take those bytes as one char.
                    let start = self.pos;
                    let len = utf8_len(b).ok_or_else(|| self.err("invalid UTF-8 lead byte"))?;
                    if start + len > self.bytes.len() {
                        return Err(self.err("truncated UTF-8 sequence"));
                    }
                    let slice = &self.bytes[start..start + len];
                    let s = std::str::from_utf8(slice)
                        .map_err(|_| ParseError { message: "invalid UTF-8".into(), at: start })?;
                    out.push_str(s);
                    self.pos += len;
                }
            }
        }
    }

    /// `\uXXXX`, including the surrogate pair form the file would use for
    /// anything outside the basic plane.
    fn unicode_escape(&mut self) -> Result<char> {
        let first = self.hex4()?;
        // High surrogate: a second escape must follow.
        if (0xD800..0xDC00).contains(&first) {
            if self.peek() == Some(b'\\') {
                self.pos += 1;
                if self.peek() == Some(b'u') {
                    self.pos += 1;
                    let second = self.hex4()?;
                    if (0xDC00..0xE000).contains(&second) {
                        let combined =
                            0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00);
                        return char::from_u32(combined)
                            .ok_or_else(|| self.err("invalid surrogate pair"));
                    }
                }
            }
            return Err(self.err("unpaired surrogate"));
        }
        char::from_u32(first).ok_or_else(|| self.err("invalid code point"))
    }

    fn hex4(&mut self) -> Result<u32> {
        let mut value = 0u32;
        for _ in 0..4 {
            let b = self.peek().ok_or_else(|| self.err("truncated \\u escape"))?;
            let digit = match b {
                b'0'..=b'9' => (b - b'0') as u32,
                b'a'..=b'f' => (b - b'a' + 10) as u32,
                b'A'..=b'F' => (b - b'A' + 10) as u32,
                _ => return Err(self.err("not a hex digit in \\u escape")),
            };
            value = value * 16 + digit;
            self.pos += 1;
        }
        Ok(value)
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-' => self.pos += 1,
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| ParseError { message: "invalid number".into(), at: start })?
            .to_string();
        let value: f64 = text.parse().map_err(|_| ParseError {
            message: format!("not a number: {text}"),
            at: start,
        })?;
        Ok(Value::Number { text, value })
    }
}

fn utf8_len(lead: u8) -> Option<usize> {
    match lead {
        0x00..=0x7F => Some(1),
        0xC2..=0xDF => Some(2),
        0xE0..=0xEF => Some(3),
        0xF0..=0xF4 => Some(4),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(input: &str) -> Value {
        parse(input).expect("should parse")
    }

    #[test]
    fn quoting_escapes_what_a_browser_would_refuse() {
        assert_eq!(quote("plain"), "\"plain\"");
        assert_eq!(quote("a\"b\\c"), "\"a\\\"b\\\\c\"");
        // A raw newline is what JSON.parse rejects outright; a log tail is
        // nothing but newlines.
        assert_eq!(quote("one\ntwo\r\n\t"), "\"one\\ntwo\\r\\n\\t\"");
        assert_eq!(quote("\u{7}"), "\"\\u0007\"");
        // A path's backslashes survive as an escaped pair.
        assert_eq!(quote(r"C:\data\models\a.gguf"), r#""C:\\data\\models\\a.gguf""#);
    }

    #[test]
    fn a_quoted_string_parses_back_to_what_went_in() {
        let value = "a \"quoted\" name\twith\na newline \\ and a C:\\path";
        assert_eq!(parse(&quote(value)).expect("round trip"), Value::String(value.to_string()));
    }

    #[test]
    fn flat_object_keeps_key_order() {
        let v = obj(r#"{"b": 1, "a": 2}"#);
        let Value::Object(members) = &v else { panic!("not an object") };
        assert_eq!(members[0].0, "b");
        assert_eq!(members[1].0, "a");
        assert_eq!(v.get("a").and_then(Value::as_f64), Some(2.0));
    }

    #[test]
    fn the_shape_a_results_row_has() {
        let line = r#"{"ts": "2026-10-20T00:00:38Z", "kind": "eval", "ctx": 236544,
                       "params_b": 35.51, "moe": true, "experts": null,
                       "eval": {"tools": "PASS", "needle": "FAIL", "depth_decode": 30.6},
                       "flags": {"ctx-size": 236544}}"#;
        let v = obj(line);
        assert_eq!(v.get("kind").and_then(Value::as_str), Some("eval"));
        assert_eq!(v.get("params_b").and_then(Value::as_f64), Some(35.51));
        assert_eq!(v.get("moe").and_then(Value::as_bool), Some(true));
        assert!(v.get("experts").map(Value::is_null).unwrap_or(false));
        let eval = v.get("eval").expect("eval object");
        assert_eq!(eval.get("needle").and_then(Value::as_str), Some("FAIL"));
        assert_eq!(eval.get("depth_decode").and_then(Value::as_f64), Some(30.6));
        assert_eq!(
            v.get("flags").and_then(|f| f.get("ctx-size")).and_then(Value::as_f64),
            Some(236544.0)
        );
    }

    #[test]
    fn missing_key_and_null_are_both_none() {
        let v = obj(r#"{"a": null}"#);
        assert!(v.get("a").map(Value::is_null).unwrap_or(false));
        assert!(v.get("nope").is_none());
        assert_eq!(v.get("nope").and_then(Value::as_f64), None);
    }

    #[test]
    fn strings_unescape() {
        let v = obj(r#"{"s": "a\"b\\c\/d\ne\tf"}"#);
        assert_eq!(v.get("s").and_then(Value::as_str), Some("a\"b\\c/d\ne\tf"));
    }

    #[test]
    fn unicode_escapes_and_surrogate_pairs() {
        let v = obj(r#"{"s": "é 中 😀"}"#);
        assert_eq!(v.get("s").and_then(Value::as_str), Some("é 中 😀"));
    }

    #[test]
    fn raw_utf8_passes_through() {
        let v = obj(r#"{"note": "café — ok"}"#);
        assert_eq!(v.get("note").and_then(Value::as_str), Some("café — ok"));
    }

    #[test]
    fn numbers_keep_their_written_form() {
        let v = obj(r#"{"i": 236544, "f": 35.51, "neg": -3, "exp": 1.5e-3}"#);
        assert_eq!(v.get("i"), Some(&Value::Number { text: "236544".into(), value: 236544.0 }));
        assert_eq!(v.get("f").and_then(Value::as_f64), Some(35.51));
        assert_eq!(v.get("neg").and_then(Value::as_f64), Some(-3.0));
        assert_eq!(v.get("exp").and_then(Value::as_f64), Some(0.0015));
    }

    #[test]
    fn arrays_and_empty_containers() {
        let v = obj(r#"{"a": [], "o": {}, "l": [1, "two", false, null]}"#);
        assert_eq!(v.get("a"), Some(&Value::Array(vec![])));
        assert_eq!(v.get("o"), Some(&Value::Object(vec![])));
        let Value::Array(items) = v.get("l").expect("list") else { panic!("not an array") };
        assert_eq!(items.len(), 4);
        assert_eq!(items[1].as_str(), Some("two"));
    }

    #[test]
    fn rejects_trailing_garbage() {
        // A doubled or truncated line must not be half-read.
        assert!(parse(r#"{"a": 1} {"b": 2}"#).is_err());
        assert!(parse(r#"{"a": 1"#).is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            r#"{"a" 1}"#,
            r#"{a: 1}"#,
            r#"{"a": "unterminated}"#,
            r#"{"a": 01x}"#,
            r#"{"a": tru}"#,
            r#"[1, 2"#,
        ] {
            assert!(parse(bad).is_err(), "should have failed: {bad}");
        }
    }

    #[test]
    fn error_reports_where() {
        let err = parse(r#"{"a": 1, }"#).unwrap_err();
        assert!(err.at > 0);
        assert!(err.to_string().contains("at byte"));
    }
}
