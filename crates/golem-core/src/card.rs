//! A report card: one setup's measurements in one self-contained file.
//!
//! A setup is a host, a model file, an engine build and its backend - a
//! configuration (`Row::configuration_key`) at every context. Its card is
//! JSON Lines: a header naming the setup - the host's anonymous slug, the
//! build, the model file and its SHA-256, the newest row's flags and the
//! seal its rows earn (proven by a passed gate, failed, or measured) - and
//! then every row of the setup, the lines exactly as `results.jsonl` holds
//! them. A row holds nothing a public results file does not, so neither
//! does a card.
//!
//! Re-reading one ([`read`]) checks that every row parses and belongs to
//! the header's setup, and [`new_rows`] keeps the ones a checkout does not
//! have yet, so importing a card twice adds nothing.

use crate::json::{self, Value};
use crate::results::{Kind, Row};

/// The card format's version, the header's `card` key.
pub const VERSION: u64 = 1;

/// One card: the file name it is written under and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Card {
    pub file_name: String,
    pub text: String,
}

/// What one card says about its setup, from its header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub host: String,
    pub setup: String,
    pub seal: String,
    pub rows: usize,
}

/// A row's setup: its configuration without the context.
pub fn setup_key(row: &Row) -> String {
    format!("{}|{}|{}|{}|{}", row.host, row.identity(), row.engine, row.engine_build, row.served_backend())
}

/// The seal a setup's rows earn: `proven` when its newest gate (an eval
/// with a needle) passed, `failed` when it did not, else `measured`.
pub fn seal(rows: &[&Row]) -> &'static str {
    let gate = rows
        .iter()
        .filter(|r| r.kind == Kind::Eval)
        .filter_map(|r| r.eval.as_ref()?.needle.as_deref().map(|n| (r.ts.as_str(), n)))
        .max_by(|a, b| a.0.cmp(b.0));
    match gate {
        Some((_, "PASS")) => "proven",
        Some(_) => "failed",
        None => "measured",
    }
}

/// One card per setup in `lines` (the raw lines of `results.jsonl`), in the
/// order the setups first appear; a line that does not parse is left out.
/// `hash_of` gives a model file's SHA-256 when it is known.
pub fn cards(lines: &[&str], hash_of: &dyn Fn(&str) -> Option<String>) -> Vec<Card> {
    let parsed: Vec<(Row, &str)> =
        lines.iter().filter_map(|line| Row::parse(line.trim_end()).ok().map(|row| (row, line.trim_end()))).collect();
    let mut setups: Vec<String> = Vec::new();
    for (row, _) in &parsed {
        let key = setup_key(row);
        if !setups.contains(&key) {
            setups.push(key);
        }
    }
    setups
        .iter()
        .map(|key| {
            let members: Vec<&(Row, &str)> = parsed.iter().filter(|(row, _)| &setup_key(row) == key).collect();
            let rows: Vec<&Row> = members.iter().map(|(row, _)| row).collect();
            let newest = members.iter().max_by(|a, b| a.0.ts.cmp(&b.0.ts)).expect("a setup has a row");
            let flags = json::parse(newest.1).ok().and_then(|v| v.get("flags").cloned()).unwrap_or(Value::Null);
            let first = &newest.0;
            let header = Value::object(vec![
                ("card", Value::int(VERSION)),
                ("host", Value::str(&first.host)),
                ("model", Value::str(&first.model)),
                ("quant", Value::str(&first.quant)),
                ("model_file", Value::str(&first.model_file)),
                ("sha256", hash_of(&first.model_file).map_or(Value::Null, |h| Value::str(&h))),
                ("engine", Value::str(&first.engine)),
                ("engine_build", Value::str(&first.engine_build)),
                ("backend", Value::str(&first.served_backend())),
                ("flags", flags),
                ("seal", Value::str(seal(&rows))),
                ("rows", Value::int(rows.len() as u64)),
            ]);
            let mut text = header.to_json();
            text.push('\n');
            for (_, line) in &members {
                text.push_str(line);
                text.push('\n');
            }
            Card { file_name: file_name(first), text }
        })
        .collect()
}

/// `<host>__<model>-<quant>__<engine>-<build>-<backend>.jsonl`, each part
/// kept to letters, digits, dots, dashes and underscores.
fn file_name(row: &Row) -> String {
    let clean = |s: &str| -> String {
        s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') { c } else { '-' }).collect()
    };
    let model = if row.quant.is_empty() { row.model.clone() } else { format!("{}-{}", row.model, row.quant) };
    let build = [row.engine.as_str(), row.engine_build.as_str(), &row.served_backend()]
        .iter()
        .filter(|s| !s.is_empty())
        .map(|s| clean(s))
        .collect::<Vec<_>>()
        .join("-");
    format!("{}__{}__{}.jsonl", clean(&row.host), clean(&model), build)
}

/// A card read back: its header and its rows' lines, each checked to parse
/// and to belong to the header's host and setup.
pub fn read(text: &str) -> Result<(Header, Vec<String>), String> {
    let mut lines = text.lines().map(|l| l.trim_end_matches('\r')).filter(|l| !l.trim().is_empty());
    let head = json::parse(lines.next().ok_or("an empty file is not a card")?).map_err(|e| format!("the header: {e}"))?;
    if head.get("card").and_then(Value::as_f64) != Some(VERSION as f64) {
        return Err(format!("not a version {VERSION} report card (no \"card\": {VERSION} in its first line)"));
    }
    let field = |key: &str| head.get(key).and_then(Value::as_str).unwrap_or_default().to_string();
    let host = field("host");
    let rows: Vec<String> = lines.map(str::to_string).collect();
    let mut setup = None;
    for (i, line) in rows.iter().enumerate() {
        let row = Row::parse(line).map_err(|e| format!("row {}: {e}", i + 1))?;
        if row.host != host {
            return Err(format!("row {} is from host {}, the card's is {host}", i + 1, row.host));
        }
        let key = setup_key(&row);
        match &setup {
            None => setup = Some(key),
            Some(first) if *first != key => return Err(format!("row {} is another setup's ({key})", i + 1)),
            Some(_) => {}
        }
    }
    let setup = setup.ok_or("the card has no rows")?;
    if head.get("rows").and_then(Value::as_f64) != Some(rows.len() as f64) {
        return Err(format!("the header counts {} rows and the card holds {}", field("rows"), rows.len()));
    }
    Ok((Header { host, setup, seal: field("seal"), rows: rows.len() }, rows))
}

/// The card's rows that `existing` (a `results.jsonl`) does not hold yet,
/// line for line.
pub fn new_rows(existing: &str, rows: &[String]) -> Vec<String> {
    let have: std::collections::HashSet<&str> = existing.lines().map(|l| l.trim_end_matches('\r')).collect();
    let mut out: Vec<String> = Vec::new();
    for row in rows {
        if !have.contains(row.as_str()) && !out.contains(row) {
            out.push(row.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BENCH: &str = r#"{"ts": "2026-10-20T15:40Z", "kind": "bench", "host": "rtx-3060_12g_32g_i7-12700kf", "gpu": "NVIDIA GeForce RTX 3060", "engine": "llama.cpp", "engine_build": "b11379-1537a0a8b", "model": "ornith-1.5-35b", "model_file": "Ornith-1.5-35B-Q4_K_M.gguf", "ctx": 249344, "flags": {"n-cpu-moe": 35, "ctx-size": 249344}, "decode_median": 62.1, "prefill": 870, "vram_mib": 11311, "quiet": true, "note": ""}"#;
    const GATE: &str = r#"{"ts": "2026-10-30T01:52Z", "kind": "eval", "host": "rtx-3060_12g_32g_i7-12700kf", "gpu": "NVIDIA GeForce RTX 3060", "engine": "llama.cpp", "engine_build": "b11379-1537a0a8b", "model": "ornith-1.5-35b", "model_file": "Ornith-1.5-35B-Q4_K_M.gguf", "ctx": 248832, "flags": {"n-cpu-moe": 35, "ctx-size": 248832}, "eval": {"needle": "PASS", "needle_tokens": 200000}, "quiet": true, "note": ""}"#;
    const VULKAN: &str = r#"{"ts": "2026-10-30T22:47Z", "kind": "bench", "host": "rtx-3060_12g_32g_i7-12700kf", "gpu": "NVIDIA GeForce RTX 3060", "engine": "llama.cpp", "engine_build": "b11379-1537a0a8b", "backend": "vulkan", "model": "ornith-1.5-35b", "model_file": "Ornith-1.5-35B-Q4_K_M.gguf", "ctx": 248832, "flags": {}, "decode_median": 40.0, "quiet": true, "note": ""}"#;

    #[test]
    fn one_card_per_setup_with_its_rows_verbatim() {
        let hash = |file: &str| (file == "Ornith-1.5-35B-Q4_K_M.gguf").then(|| "ab12".to_string());
        let cards = cards(&[BENCH, "not json", GATE, VULKAN], &hash);
        assert_eq!(cards.len(), 2, "one build on two backends is two setups, two contexts one");
        let cuda = &cards[0];
        assert_eq!(
            cuda.file_name,
            "rtx-3060_12g_32g_i7-12700kf__ornith-1.5-35b-Q4_K_M__llama.cpp-b11379-1537a0a8b-cuda.jsonl"
        );
        let lines: Vec<&str> = cuda.text.lines().collect();
        assert_eq!(lines[1..], [BENCH, GATE], "the rows as the results file holds them");
        let head = json::parse(lines[0]).unwrap();
        assert_eq!(head.get("seal").and_then(Value::as_str), Some("proven"));
        assert_eq!(head.get("sha256").and_then(Value::as_str), Some("ab12"));
        assert_eq!(head.get("flags").and_then(|f| f.get("ctx-size")).and_then(Value::as_f64), Some(248_832.0), "the newest row's");
        assert_eq!(json::parse(cards[1].text.lines().next().unwrap()).unwrap().get("seal").and_then(Value::as_str), Some("measured"));
    }

    #[test]
    fn a_card_reads_back_and_imports_once() {
        let card = &cards(&[BENCH, GATE], &|_| None)[0];
        let (header, rows) = read(&card.text).expect("a card");
        assert_eq!((header.seal.as_str(), header.rows), ("proven", 2));
        assert_eq!(header.host, "rtx-3060_12g_32g_i7-12700kf");
        // A checkout that has the bench already takes only the gate.
        assert_eq!(new_rows(&format!("{BENCH}\r\n"), &rows), vec![GATE.to_string()]);
        assert!(new_rows(&format!("{BENCH}\n{GATE}\n"), &rows).is_empty(), "importing twice adds nothing");
    }

    #[test]
    fn a_card_that_does_not_hold_together_is_refused() {
        let card = &cards(&[BENCH, GATE], &|_| None)[0];
        assert!(read("").is_err());
        assert!(read(BENCH).unwrap_err().contains("not a version 1"));
        let other_host = card.text.replace("\"host\": \"rtx-3060_12g_32g_i7-12700kf\", \"gpu\"", "\"host\": \"other\", \"gpu\"");
        assert!(read(&other_host).unwrap_err().contains("from host other"));
        let mixed = format!("{}{VULKAN}\n", card.text.replace("\"rows\":2", "\"rows\":3"));
        assert!(read(&mixed).unwrap_err().contains("another setup's"));
        let short = card.text.replace("\"rows\":2", "\"rows\":5");
        assert!(read(&short).unwrap_err().contains("counts"));
    }
}
