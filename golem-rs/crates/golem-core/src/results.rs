//! Reader for `bench/results.jsonl` - one recorded measurement per line.
//!
//! The file is append-only and best-effort: the recorder writes `null` for
//! anything it could not measure, and a line that will not parse is a
//! problem to report, never a reason to lose the other rows. So `read`
//! returns the rows it could read *and* the lines it could not, with their
//! line numbers.
//!
//! The report groups hardware first: one section per `host`, one block per
//! model inside it, and inside that one line per context - the best setup
//! measured at that context. The reduction here mirrors that: a row is
//! "best" per host x model x engine x engine build x context, and rows from
//! different hosts are never compared at all (a machine is its own table).

use std::fmt;
use std::path::Path;

use crate::json::{self, Value};

/// What produced a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// `golem bench`, or one point of a tune sweep.
    Bench,
    /// `golem eval`, including the tuner's gate.
    Eval,
    /// A measured rejection, with the reason in the note.
    Reject,
    /// A kind this build does not know (a newer recorder, say).
    Unknown,
}

impl Kind {
    pub fn parse(s: &str) -> Kind {
        match s {
            "bench" => Kind::Bench,
            "eval" => Kind::Eval,
            "reject" => Kind::Reject,
            _ => Kind::Unknown,
        }
    }
}

/// A number as the file spelled it, plus its value.
///
/// The report prints the decode and prefill of the winning row *verbatim*,
/// so the spelling has to survive parsing: a row that says `300.0` must not
/// come out as `300`. Everything the report reformats (VRAM, context, the
/// ratio) only needs the value, and is a plain `f64`.
#[derive(Debug, Clone, PartialEq)]
pub struct Num {
    pub text: String,
    pub value: f64,
}

impl Num {
    pub fn new(text: impl Into<String>, value: f64) -> Num {
        Num { text: text.into(), value }
    }
}

/// One recorded measurement, with the fields the report and the console use.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub kind: Kind,
    pub ts: String,
    /// The machine that produced the row: GPU, VRAM, RAM and CPU in one
    /// slug. Empty when the recorder could not name the machine, which the
    /// report shows last under "unknown host".
    pub host: String,
    pub gpu: Option<String>,
    pub os: Option<String>,
    pub mode: Option<String>,
    pub model: String,
    pub engine: String,
    pub engine_build: String,
    pub ctx: Option<u64>,
    /// CPU threads the engine ran with: a tuned parameter, so it travels
    /// with the number it produced.
    pub threads: Option<u32>,
    pub params_b: Option<f64>,
    pub params_active_b: Option<f64>,
    pub experts: Option<String>,
    pub decode_best: Option<f64>,
    /// Printed verbatim by the report: keeps the file's spelling.
    pub decode_median: Option<Num>,
    /// Printed verbatim by the report: keeps the file's spelling.
    pub prefill: Option<Num>,
    pub draft_accept: Option<f64>,
    pub vram_mib: Option<f64>,
    pub ram_used_gib: Option<f64>,
    pub quiet: bool,
    pub note: String,
    /// `[("ctx-size", "236544"), ...]` in the order the file has them.
    pub flags: Vec<(String, String)>,
    /// The eval verdicts, when this row came from an eval.
    pub eval: Option<Eval>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Eval {
    pub tools: Option<String>,
    pub needle: Option<String>,
    pub needle_tokens: Option<u64>,
    pub depth_decode: Option<f64>,
    pub decode_best: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum RowError {
    Json(json::ParseError),
    /// A field every row must carry (the recorder always writes these).
    Missing(&'static str),
    /// The line is JSON, but not an object.
    NotAnObject,
}

impl fmt::Display for RowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowError::Json(e) => write!(f, "{e}"),
            RowError::Missing(k) => write!(f, "missing field: {k}"),
            RowError::NotAnObject => write!(f, "row is not a JSON object"),
        }
    }
}

impl std::error::Error for RowError {}

impl From<json::ParseError> for RowError {
    fn from(e: json::ParseError) -> Self {
        RowError::Json(e)
    }
}

fn str_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_string)
}

fn num_of(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// A number with the text the file used for it (see [`Num`]).
fn num_text(v: Option<&Value>) -> Option<Num> {
    match v {
        Some(Value::Number { text, value }) => Some(Num::new(text.clone(), *value)),
        _ => None,
    }
}

/// `null`, a missing key and a wrong type all mean "not measured" here:
/// the file says so with `null`, and a consumer should treat them alike.
fn u64_of(v: Option<&Value>) -> Option<u64> {
    num_of(v).filter(|n| *n >= 0.0).map(|n| n as u64)
}

fn u32_of(v: Option<&Value>) -> Option<u32> {
    num_of(v).filter(|n| *n >= 0.0 && *n <= u32::MAX as f64).map(|n| n as u32)
}

impl Row {
    /// Parse one line of `results.jsonl`.
    pub fn parse(line: &str) -> Result<Row, RowError> {
        let value = json::parse(line)?;
        let Value::Object(_) = &value else { return Err(RowError::NotAnObject) };

        let kind = Kind::parse(&str_of(value.get("kind")).ok_or(RowError::Missing("kind"))?);
        // ts and model are what identify a row in every view; engine and
        // build can legitimately be unknown (a server that reports neither).
        let ts = str_of(value.get("ts")).ok_or(RowError::Missing("ts"))?;
        let model = str_of(value.get("model")).ok_or(RowError::Missing("model"))?;

        let eval = value.get("eval").and_then(|e| {
            if e.is_null() {
                return None;
            }
            Some(Eval {
                tools: str_of(e.get("tools")),
                needle: str_of(e.get("needle")),
                needle_tokens: u64_of(e.get("needle_tokens")),
                depth_decode: num_of(e.get("depth_decode")),
                decode_best: num_of(e.get("decode_best")),
            })
        });

        let flags = match value.get("flags") {
            Some(Value::Object(members)) => members
                .iter()
                .map(|(k, v)| (k.clone(), render_flag(v)))
                .collect(),
            _ => Vec::new(),
        };

        Ok(Row {
            kind,
            ts,
            // A row without a host is a row from an unnamed machine; the
            // report keeps it, last, under "unknown host".
            host: str_of(value.get("host")).unwrap_or_default(),
            gpu: str_of(value.get("gpu")),
            os: str_of(value.get("os")),
            mode: str_of(value.get("mode")),
            model,
            engine: str_of(value.get("engine")).unwrap_or_default(),
            engine_build: str_of(value.get("engine_build")).unwrap_or_default(),
            ctx: u64_of(value.get("ctx")),
            threads: u32_of(value.get("threads")),
            params_b: num_of(value.get("params_b")),
            params_active_b: num_of(value.get("params_active_b")),
            experts: str_of(value.get("experts")),
            decode_best: num_of(value.get("decode_best")),
            decode_median: num_text(value.get("decode_median")),
            prefill: num_text(value.get("prefill")),
            draft_accept: num_of(value.get("draft_accept")),
            vram_mib: num_of(value.get("vram_mib")),
            ram_used_gib: num_of(value.get("ram_used_gib")),
            // Anything but an explicit false counts as quiet, so a row
            // without the field reads as a quiet run.
            quiet: value.get("quiet").and_then(Value::as_bool).unwrap_or(true),
            note: str_of(value.get("note")).unwrap_or_default(),
            flags,
            eval,
        })
    }

    /// The size class to compare this row in: active parameters when the
    /// model is a mixture of experts, total parameters otherwise. A 35B-A3B
    /// does 3.5B of work per token, and that is what the GPU pays for.
    pub fn size_class(&self) -> SizeClass {
        match self.params_active_b.or(self.params_b) {
            Some(p) => SizeClass::of(p),
            None => SizeClass::Unknown,
        }
    }

    /// The identity of the configuration a row measured: one line per
    /// machine, model, engine, engine build and context.
    ///
    /// Tuning is deliberately *not* part of it: two runs of the same
    /// configuration with a different offload split, batch or thread count
    /// are the same configuration, so the faster one is the number that
    /// configuration gets and the slower is left in `results.jsonl`. Change
    /// the host, engine or context and it is a different configuration with
    /// a line of its own.
    pub fn configuration_key(&self) -> String {
        format!(
            "{}|{}|{}|{}|{}",
            self.host,
            self.model,
            self.engine,
            self.engine_build,
            self.ctx.map(|c| c.to_string()).unwrap_or_default(),
        )
    }

    /// Decode to compare: an eval row carries its own (shorter) measurement.
    pub fn comparable_decode(&self) -> Option<f64> {
        match self.kind {
            Kind::Eval => self.eval.as_ref().and_then(|e| e.decode_best),
            _ => self.decode_median.as_ref().map(|n| n.value),
        }
    }
}

/// The classes the report groups by. Rows are only comparable inside one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SizeClass {
    Unknown,
    UpTo2B,
    UpTo8B,
    UpTo20B,
    Over20B,
}

impl SizeClass {
    pub fn of(params_b: f64) -> SizeClass {
        if params_b <= 0.0 {
            SizeClass::Unknown
        } else if params_b < 2.0 {
            SizeClass::UpTo2B
        } else if params_b < 8.0 {
            SizeClass::UpTo8B
        } else if params_b < 20.0 {
            SizeClass::UpTo20B
        } else {
            SizeClass::Over20B
        }
    }

    /// The heading the report prints.
    pub fn label(&self) -> &'static str {
        match self {
            SizeClass::Unknown => "unknown",
            SizeClass::UpTo2B => "<= 2B active",
            SizeClass::UpTo8B => "2-8B active",
            SizeClass::UpTo20B => "8-20B active",
            SizeClass::Over20B => "20B+ active",
        }
    }

    /// The report's ordering: smallest class first, and a row whose size is
    /// unknown last.
    pub fn sort_key(&self) -> u8 {
        match self {
            SizeClass::UpTo2B => 1,
            SizeClass::UpTo8B => 2,
            SizeClass::UpTo20B => 3,
            SizeClass::Over20B => 4,
            SizeClass::Unknown => 9,
        }
    }
}

fn render_flag(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number { text, .. } => text.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Null => String::new(),
        // The recorder only writes scalars into flags; anything else is
        // shown as-is rather than dropped.
        other => format!("{other:?}"),
    }
}

/// Every row in the file, plus the lines that would not parse.
///
/// A malformed line is reported with its number and skipped: losing a whole
/// report over one bad line would hide the good measurements next to it.
pub fn read(path: &Path) -> std::io::Result<(Vec<Row>, Vec<(usize, String)>)> {
    let text = std::fs::read_to_string(path)?;
    let mut rows = Vec::new();
    let mut bad = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match Row::parse(line) {
            Ok(row) => rows.push(row),
            Err(e) => bad.push((i + 1, e.to_string())),
        }
    }
    Ok((rows, bad))
}

/// One line of the report: the best decode measured for a
/// model x engine build x context, with the eval verdict for that model on
/// that engine.
#[derive(Debug, Clone, PartialEq)]
pub struct Best {
    pub row: Row,
    /// The needle verdict for this model *and engine*, from the last eval row
    /// that measured both. `None` when nothing gated this pair.
    pub needle: Option<String>,
}

/// Reduce rows to one entry per host x model x engine x engine build x
/// context, at the best decode seen for that configuration - the reduction
/// the report prints. Ties keep the row that came first, so the file's order
/// decides.
///
/// The result is already in the report's print order: host first (an unknown
/// host last), then the size class, then each model's block (its best decode
/// first, ties by name), then the rows inside a block by context, engine and
/// build. A caller can therefore walk it and start a new section whenever
/// the host changes, a new block whenever the model does.
pub fn best_per_configuration(rows: &[Row]) -> Vec<Best> {
    // The last verdict a model has wins (later rows overwrite earlier
    // ones), so a re-run corrects the report without an edit.
    //
    // Keyed by model *and engine*, not by model alone: the needle is the
    // model's quality gate, but it is measured on one engine, and an engine
    // that was never gated must not inherit the winner's PASS. On the
    // reference box the tuner sweeps ik_llama.cpp and llama.cpp and gates
    // only the winner - so a PASS on the ik_llama.cpp row would be a claim
    // nothing measured.
    //
    // And by host: a gate passed on one machine says nothing about another,
    // and machines never share a table.
    let mut needles: Vec<((String, String, String), String)> = Vec::new();
    for row in rows {
        if row.kind != Kind::Eval {
            continue;
        }
        if let Some(n) = row.eval.as_ref().and_then(|e| e.needle.clone()) {
            let key = (row.host.clone(), row.model.clone(), row.engine.clone());
            match needles.iter_mut().find(|(k, _)| k == &key) {
                Some((_, existing)) => *existing = n,
                None => needles.push((key, n)),
            }
        }
    }

    let mut best: Vec<(String, f64, Row)> = Vec::new();
    for row in rows {
        if row.kind != Kind::Bench {
            continue;
        }
        let Some(decode) = row.decode_median.as_ref().map(|n| n.value) else { continue };
        let key = row.configuration_key();
        match best.iter_mut().find(|(k, _, _)| k == &key) {
            Some((_, best_decode, best_row)) => {
                if decode > *best_decode {
                    *best_decode = decode;
                    *best_row = row.clone();
                }
            }
            None => best.push((key, decode, row.clone())),
        }
    }

    let mut out: Vec<Best> = best
        .into_iter()
        .map(|(_, _, row)| {
            let key = (row.host.clone(), row.model.clone(), row.engine.clone());
            let needle = needles.iter().find(|(k, _)| k == &key).map(|(_, n)| n.clone());
            Best { row, needle }
        })
        .collect();

    // A model's best decode orders its block against the other models in the
    // same host and size class.
    let mut model_best: Vec<((String, String), f64)> = Vec::new();
    for entry in &out {
        let key = (entry.row.host.clone(), entry.row.model.clone());
        let decode =
            entry.row.decode_median.as_ref().map(|n| n.value).unwrap_or(f64::NEG_INFINITY);
        match model_best.iter_mut().find(|(k, _)| k == &key) {
            Some((_, v)) => {
                if decode > *v {
                    *v = decode;
                }
            }
            None => model_best.push((key, decode)),
        }
    }
    let best_of = |row: &Row| -> f64 {
        model_best
            .iter()
            .find(|(k, _)| k.0 == row.host && k.1 == row.model)
            .map(|(_, v)| *v)
            .unwrap_or(f64::NEG_INFINITY)
    };

    // Host first (an unknown host last), then the size class, then the model
    // block (its best decode, best first, ties by name), then the rows
    // inside the block: context ascending with no-context last, then engine
    // and build - the order the report prints in.
    out.sort_by(|a, b| {
        host_key(&a.row.host)
            .cmp(&host_key(&b.row.host))
            .then(a.row.size_class().sort_key().cmp(&b.row.size_class().sort_key()))
            .then(
                best_of(&b.row)
                    .partial_cmp(&best_of(&a.row))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
            .then(a.row.model.cmp(&b.row.model))
            .then(ctx_key(a.row.ctx).cmp(&ctx_key(b.row.ctx)))
            .then(a.row.engine.cmp(&b.row.engine))
            .then(a.row.engine_build.cmp(&b.row.engine_build))
    });
    out
}

/// The report's host ordering: machine slugs ascending, and a row with no
/// host (the empty string) last, so "unknown host" closes the report.
pub fn host_key(host: &str) -> (u8, &str) {
    if host.is_empty() {
        (1, "")
    } else {
        (0, host)
    }
}

/// Context ascending, and a row with no context last.
fn ctx_key(ctx: Option<u64>) -> (u8, u64) {
    match ctx {
        Some(c) => (0, c),
        None => (1, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bench row shaped like the ones the recorder writes.
    const BENCH: &str = r#"{"ts": "2026-10-19T18:02:22Z", "kind": "bench", "host": "rtx-3060_12g_32g_i7-12700kf", "gpu": "NVIDIA GeForce RTX 3060", "os": "windows", "mode": "windows", "engine": "llama.cpp", "engine_build": "b11379-1537a0a8b", "model": "ornith-1.5-35b", "model_file": "Ornith-1.5-35B-Q4_K_M.gguf", "ctx": 236544, "threads": 16, "params_b": 35.51, "params_active_b": 3.52, "moe": true, "experts": "256x8", "n_layer": 40, "ctx_train": 262144, "flags": {"jinja": "true", "ctx-size": 236544}, "decode_best": 59.6, "decode_median": 59.3, "draft_accept": 0.66, "prefill": 832, "vram_mib": 11000, "ram_used_gib": 19.4, "ram_free_gib": 8.1, "quiet": true, "note": ""}"#;

    const EVAL: &str = r#"{"ts": "2026-10-20T00:00:38Z", "kind": "eval", "host": "rtx-3060_12g_32g_i7-12700kf", "model": "ornith-1.5-35b", "engine": "llama.cpp", "engine_build": "b11379-1537a0a8b", "ctx": 236544, "eval": {"tools": "PASS", "needle": "PASS", "needle_tokens": 211818, "depth_decode": 30.6, "decode_best": 58.1}, "quiet": true, "note": ""}"#;

    #[test]
    fn parses_a_bench_row() {
        let row = Row::parse(BENCH).expect("should parse");
        assert_eq!(row.kind, Kind::Bench);
        assert_eq!(row.host, "rtx-3060_12g_32g_i7-12700kf");
        assert_eq!(row.gpu.as_deref(), Some("NVIDIA GeForce RTX 3060"));
        assert_eq!(row.os.as_deref(), Some("windows"));
        assert_eq!(row.mode.as_deref(), Some("windows"));
        assert_eq!(row.model, "ornith-1.5-35b");
        assert_eq!(row.ctx, Some(236544));
        assert_eq!(row.threads, Some(16));
        assert_eq!(row.params_active_b, Some(3.52));
        assert_eq!(row.decode_median.as_ref().map(|n| n.value), Some(59.3));
        assert_eq!(row.draft_accept, Some(0.66));
        assert!(row.quiet);
        assert_eq!(row.flags.len(), 2);
        assert_eq!(row.flags[0], ("jinja".to_string(), "true".to_string()));
        assert_eq!(row.flags[1], ("ctx-size".to_string(), "236544".to_string()));
    }

    #[test]
    fn parses_an_eval_row_and_its_verdict() {
        let row = Row::parse(EVAL).expect("should parse");
        assert_eq!(row.kind, Kind::Eval);
        let eval = row.eval.as_ref().expect("eval object");
        assert_eq!(eval.needle.as_deref(), Some("PASS"));
        assert_eq!(eval.needle_tokens, Some(211818));
        assert_eq!(eval.depth_decode, Some(30.6));
        // An eval row's comparable decode comes from inside eval.
        assert_eq!(row.comparable_decode(), Some(58.1));
    }

    #[test]
    fn a_row_without_threads_still_parses() {
        // Rows recorded before the field existed must keep working.
        let line = BENCH.replace(r#""threads": 16, "#, "");
        let row = Row::parse(&line).expect("should parse");
        assert_eq!(row.threads, None);
    }

    #[test]
    fn null_and_missing_are_both_absent() {
        let line = r#"{"ts": "t", "kind": "bench", "model": "m", "ctx": null, "decode_median": null}"#;
        let row = Row::parse(line).expect("should parse");
        assert_eq!(row.ctx, None);
        assert!(row.decode_median.is_none());
        assert_eq!(row.engine, "");
    }

    #[test]
    fn the_report_can_print_a_number_the_way_the_file_wrote_it() {
        // The report prints decode and prefill verbatim, so the spelling
        // has to survive: 300.0 must not come back as 300.
        let line = BENCH
            .replace(r#""decode_median": 59.3"#, r#""decode_median": 300.0"#)
            .replace(r#""prefill": 832"#, r#""prefill": 832.50"#);
        let row = Row::parse(&line).expect("should parse");
        let decode = row.decode_median.expect("decode");
        assert_eq!(decode.text, "300.0");
        assert_eq!(decode.value, 300.0);
        let prefill = row.prefill.expect("prefill");
        assert_eq!(prefill.text, "832.50");
    }

    #[test]
    fn requires_the_fields_that_identify_a_row() {
        assert!(matches!(Row::parse("{}"), Err(RowError::Missing("kind"))));
        assert!(matches!(Row::parse(r#"{"kind": "bench"}"#), Err(RowError::Missing("ts"))));
        assert!(matches!(
            Row::parse(r#"{"kind": "bench", "ts": "t"}"#),
            Err(RowError::Missing("model"))
        ));
        assert!(matches!(Row::parse("[1, 2]"), Err(RowError::NotAnObject)));
    }

    #[test]
    fn unknown_kind_is_kept_as_unknown() {
        let line = r#"{"ts": "t", "kind": "future", "model": "m"}"#;
        assert_eq!(Row::parse(line).expect("should parse").kind, Kind::Unknown);
    }

    #[test]
    fn size_classes_follow_active_parameters() {
        assert_eq!(SizeClass::of(0.49), SizeClass::UpTo2B);
        assert_eq!(SizeClass::of(3.52), SizeClass::UpTo8B);
        assert_eq!(SizeClass::of(9.0), SizeClass::UpTo20B);
        assert_eq!(SizeClass::of(35.51), SizeClass::Over20B);
        assert_eq!(SizeClass::of(0.0), SizeClass::Unknown);
        // A 35B-A3B belongs with the 3B-class models: 3.5B of work per token.
        let row = Row::parse(BENCH).expect("should parse");
        assert_eq!(row.size_class(), SizeClass::UpTo8B);
    }

    #[test]
    fn best_per_configuration_keeps_the_fastest_and_the_needle() {
        let slower = BENCH.replace(r#""decode_median": 59.3"#, r#""decode_median": 51.0"#);
        let rows: Vec<Row> = [BENCH, &slower, EVAL]
            .iter()
            .map(|l| Row::parse(l).expect("should parse"))
            .collect();
        let best = best_per_configuration(&rows);
        assert_eq!(best.len(), 1, "same model, engine, build and ctx is one line");
        assert_eq!(best[0].row.decode_median.as_ref().map(|n| n.value), Some(59.3));
        assert_eq!(best[0].needle.as_deref(), Some("PASS"));
    }

    #[test]
    fn a_needle_verdict_does_not_cross_engines() {
        // The gate runs on the winner. An engine that was swept and lost was
        // never gated, so its rows must not borrow the winner's PASS.
        let other = BENCH.replace(r#""engine": "llama.cpp""#, r#""engine": "ik_llama.cpp""#);
        assert_ne!(other, BENCH, "the fixture should have changed");
        let rows: Vec<Row> = [BENCH, &other, EVAL]
            .iter()
            .map(|l| Row::parse(l).expect("should parse"))
            .collect();
        let best = best_per_configuration(&rows);
        assert_eq!(best.len(), 2, "two engines, two lines");

        let llama = best.iter().find(|b| b.row.engine == "llama.cpp").expect("llama.cpp");
        assert_eq!(llama.needle.as_deref(), Some("PASS"));
        let ik = best.iter().find(|b| b.row.engine == "ik_llama.cpp").expect("ik_llama.cpp");
        assert_eq!(ik.needle, None, "never gated, so nothing to claim");
    }

    #[test]
    fn a_needle_verdict_does_not_cross_hosts() {
        let other = BENCH.replace(r#""host": "rtx-3060_12g_32g_i7-12700kf""#, r#""host": "rtx-4090_24g_64g_i9-13900k""#);
        let rows: Vec<Row> = [BENCH, &other, EVAL].iter().map(|l| Row::parse(l).expect("should parse")).collect();
        let best = best_per_configuration(&rows);
        let gated = best.iter().find(|b| b.row.host.starts_with("rtx-3060")).expect("3060");
        assert_eq!(gated.needle.as_deref(), Some("PASS"));
        let elsewhere = best.iter().find(|b| b.row.host.starts_with("rtx-4090")).expect("4090");
        assert_eq!(elsewhere.needle, None, "gated on another machine");
    }

    #[test]
    fn the_last_needle_verdict_wins() {
        // A later eval row overwrites an earlier verdict, so a re-run
        // corrects the report without editing the file.
        let failed = EVAL.replace(r#""needle": "PASS""#, r#""needle": "FAIL""#);
        let rows: Vec<Row> = [BENCH, &failed, EVAL]
            .iter()
            .map(|l| Row::parse(l).expect("should parse"))
            .collect();
        let best = best_per_configuration(&rows);
        assert_eq!(best[0].needle.as_deref(), Some("PASS"), "the later row wins");
    }

    #[test]
    fn different_context_is_a_different_configuration() {
        let other_ctx = BENCH.replace(r#""ctx": 236544"#, r#""ctx": 32768"#);
        let rows: Vec<Row> = [BENCH, &other_ctx]
            .iter()
            .map(|l| Row::parse(l).expect("should parse"))
            .collect();
        assert_eq!(best_per_configuration(&rows).len(), 2);
    }

    #[test]
    fn two_hosts_are_two_configurations() {
        // The same model, engine, build, context and flags on another machine
        // is that machine's number, not a slower repeat of this one.
        let other = BENCH
            .replace(r#""host": "rtx-3060_12g_32g_i7-12700kf""#, r#""host": "rtx-4090_24g_64g_i9-13900k""#)
            .replace(r#""decode_median": 59.3"#, r#""decode_median": 61.0"#);
        let rows: Vec<Row> = [BENCH, &other]
            .iter()
            .map(|l| Row::parse(l).expect("should parse"))
            .collect();
        let best = best_per_configuration(&rows);
        assert_eq!(best.len(), 2, "two machines keep two numbers");
        assert_eq!(best[0].row.host, "rtx-3060_12g_32g_i7-12700kf");
        assert_eq!(best[1].row.host, "rtx-4090_24g_64g_i9-13900k");
        assert_eq!(best[1].row.decode_median.as_ref().map(|n| n.value), Some(61.0));
    }

    #[test]
    fn tuning_is_not_part_of_the_configuration() {
        // A different offload split at the same context is the same
        // configuration: the best setup wins and the other is not printed.
        let split = BENCH.replace(r#""n-cpu-moe": 36"#, r#""n-cpu-moe": 32"#)
            .replace(r#""decode_median": 59.3"#, r#""decode_median": 51.0"#);
        let rows: Vec<Row> = [BENCH, &split]
            .iter()
            .map(|l| Row::parse(l).expect("should parse"))
            .collect();
        let best = best_per_configuration(&rows);
        assert_eq!(best.len(), 1, "same host, model, engine, build and ctx is one line");
        assert_eq!(best[0].row.decode_median.as_ref().map(|n| n.value), Some(59.3));
    }

    #[test]
    fn an_unknown_host_sorts_last() {
        let unnamed = BENCH.replace(r#""host": "rtx-3060_12g_32g_i7-12700kf", "#, "");
        let rows: Vec<Row> = [&unnamed, BENCH]
            .iter()
            .map(|l| Row::parse(l).expect("should parse"))
            .collect();
        let best = best_per_configuration(&rows);
        assert_eq!(best.len(), 2);
        assert_eq!(best[0].row.host, "rtx-3060_12g_32g_i7-12700kf");
        assert_eq!(best[1].row.host, "", "the unknown host comes last");
        assert_eq!(host_key(""), (1, ""));
        assert!(host_key("a") < host_key(""));
    }

    #[test]
    fn rows_without_a_decode_are_not_candidates() {
        let no_decode = BENCH.replace(r#""decode_median": 59.3"#, r#""decode_median": null"#);
        let rows = vec![Row::parse(&no_decode).expect("should parse")];
        assert!(best_per_configuration(&rows).is_empty());
    }

    #[test]
    fn read_reports_bad_lines_and_keeps_the_good_ones() {
        let dir = std::env::temp_dir().join("golem-rs-results-test");
        std::fs::create_dir_all(&dir).expect("temp dir");
        let path = dir.join("results.jsonl");
        std::fs::write(&path, format!("{BENCH}\n{{not json}}\n\n{EVAL}\n")).expect("write");

        let (rows, bad) = read(&path).expect("read");
        assert_eq!(rows.len(), 2);
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].0, 2, "the bad line's number is reported");
        std::fs::remove_dir_all(&dir).ok();
    }
}
