//! Parsers for the `RESULT` lines that bench and eval print.
//!
//! These lines are the contract between the measuring commands and everything
//! that records or decides on a measurement: the recorder reads them to write
//! a row, and the tuner reads `long_ctx_<N>=PASS` to decide whether a
//! candidate passes the gate. The parsers are deliberately tolerant: a
//! missing key is "not measured", never an error, and a `%` or a `-` in a
//! value does not stop the rest of the line from being read.
//!
//! Shapes (printed by the binary's `bench.rs` and `eval.rs`):
//!
//! ```text
//! RESULT decode_best=59.6 decode_median=59.3 draft_accept=66% prefill=832 vram=11000
//! RESULT gen_tok_s_best=330.2
//! RESULT long_ctx_7372=PASS prompt_tok_s=19311 gen_tok_s_at_depth=254.3
//! ```

/// A `key=value` line, split on whitespace.
pub fn fields(line: &str) -> Vec<(String, String)> {
    let line = line.trim();
    let line = line.strip_prefix("RESULT").unwrap_or(line).trim();
    line.split_whitespace()
        .filter_map(|token| token.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn get<'a>(fields: &'a [(String, String)], key: &str) -> Option<&'a str> {
    fields.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

/// A number, with whatever unit the line hangs off it, and `-` as
/// "not measured".
///
/// The value is the leading run of digits and dots, and the rest of the
/// token is a unit: the bench prints
/// `vram=1209MiB` and `draft_accept=66%`. Parsing the whole token instead
/// rejects both, and a rejected `vram` reads as "the bench produced no
/// usable result" on a bench that measured perfectly well.
fn num(s: Option<&str>) -> Option<f64> {
    let s = s?.trim();
    if s.is_empty() || s == "-" {
        return None;
    }
    let end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    s[..end].parse().ok()
}

/// One `./golem bench` measurement.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Bench {
    pub decode_best: Option<f64>,
    pub decode_median: Option<f64>,
    /// Speculative acceptance, 0-1 (the line prints a percentage).
    pub draft_accept: Option<f64>,
    pub prefill: Option<f64>,
    pub vram_mib: Option<f64>,
}

impl Bench {
    pub fn parse(line: &str) -> Bench {
        let f = fields(line);
        Bench {
            decode_best: num(get(&f, "decode_best")),
            decode_median: num(get(&f, "decode_median")),
            // The line carries a percentage; the row stores a fraction.
            draft_accept: num(get(&f, "draft_accept")).map(|p| p / 100.0),
            prefill: num(get(&f, "prefill")),
            vram_mib: num(get(&f, "vram")),
        }
    }
}

/// The needle verdict of an eval run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Fail,
}

/// The long-context part of an eval run.
#[derive(Debug, Clone, PartialEq)]
pub struct Needle {
    /// The token count the run was asked for (the key's number), not the
    /// prompt size the server reported.
    pub tokens: u64,
    pub verdict: Verdict,
    pub prompt_tok_s: Option<f64>,
    pub gen_tok_s_at_depth: Option<f64>,
}

/// Everything an eval run reports, across all its RESULT lines.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Eval {
    pub gen_tok_s_best: Option<f64>,
    pub needle: Option<Needle>,
    /// The agent suite per API, when the run went on to it.
    pub agent: Vec<crate::agent::Summary>,
}

impl Eval {
    /// Read every RESULT line of a captured eval run. The needle line is the
    /// last one carrying `long_ctx_<N>=...`.
    pub fn parse<'a>(lines: impl IntoIterator<Item = &'a str>) -> Eval {
        let mut out = Eval::default();
        let mut results: Vec<&str> = Vec::new();
        for line in lines {
            let line = line.trim();
            if !line.starts_with("RESULT") {
                continue;
            }
            results.push(line);
            let f = fields(line);
            if let Some(v) = num(get(&f, "gen_tok_s_best")) {
                out.gen_tok_s_best = Some(v);
            }
            let needle = f.iter().find_map(|(k, v)| {
                let tokens = k.strip_prefix("long_ctx_")?;
                let tokens: u64 = tokens.parse().ok()?;
                let verdict = match v.as_str() {
                    "PASS" => Verdict::Pass,
                    _ => Verdict::Fail,
                };
                Some(Needle {
                    tokens,
                    verdict,
                    prompt_tok_s: num(get(&f, "prompt_tok_s")),
                    gen_tok_s_at_depth: num(get(&f, "gen_tok_s_at_depth")),
                })
            });
            if needle.is_some() {
                out.needle = needle;
            }
        }
        out.agent = crate::agent::Summary::read(results);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_bench_line() {
        let line = "RESULT decode_best=59.6 decode_median=59.3 draft_accept=66% prefill=832 vram=11000";
        let b = Bench::parse(line);
        assert_eq!(b.decode_best, Some(59.6));
        assert_eq!(b.decode_median, Some(59.3));
        assert_eq!(b.draft_accept, Some(0.66));
        assert_eq!(b.prefill, Some(832.0));
        assert_eq!(b.vram_mib, Some(11000.0));
    }

    #[test]
    fn a_unit_hanging_off_a_value_does_not_hide_it() {
        // The bench prints the VRAM in MiB and the acceptance as a
        // percentage; both values are the digits.
        let b = Bench::parse("RESULT decode_best=338.2 decode_median=338.2 draft_accept=- prefill=- vram=1209MiB");
        assert_eq!(b.vram_mib, Some(1209.0));
        assert_eq!(b.prefill, None, "a skipped prefill is still not measured");
        assert_eq!(b.decode_median, Some(338.2));

        let b = Bench::parse("RESULT decode_median=59.3 draft_accept=66% prefill=832 vram=11000");
        assert_eq!(b.draft_accept, Some(0.66));
        assert_eq!(b.prefill, Some(832.0));
    }

    #[test]
    fn a_dash_means_not_measured() {
        // The bench prints "-" for acceptance when speculative decoding is off.
        let b = Bench::parse("RESULT decode_best=12.5 decode_median=12.1 draft_accept=- prefill=400 vram=9000");
        assert_eq!(b.draft_accept, None);
        assert_eq!(b.decode_median, Some(12.1));
    }

    #[test]
    fn a_partial_line_keeps_what_it_has() {
        let b = Bench::parse("RESULT decode_median=12.1");
        assert_eq!(b.decode_median, Some(12.1));
        assert_eq!(b.decode_best, None);
        assert_eq!(b.prefill, None);
    }

    #[test]
    fn reads_an_eval_run_across_its_lines() {
        let captured = [
            "warmup request (first hit may compile CUDA graphs, be patient)...",
            "[ OK ] warmup done",
            "run 1: prompt=115 tok @ 7977 t/s, gen=192 tok @ 328.0 t/s",
            "RESULT gen_tok_s_best=330.2",
            "needle : prompt=6065 tok @ 19311 t/s; answer: 8127",
            "RESULT long_ctx_7372=PASS prompt_tok_s=19311 gen_tok_s_at_depth=254.3",
        ];
        let eval = Eval::parse(captured);
        assert_eq!(eval.gen_tok_s_best, Some(330.2));
        let needle = eval.needle.expect("needle result");
        assert_eq!(needle.tokens, 7372);
        assert_eq!(needle.verdict, Verdict::Pass);
        assert_eq!(needle.prompt_tok_s, Some(19311.0));
        assert_eq!(needle.gen_tok_s_at_depth, Some(254.3));
    }

    #[test]
    fn a_failed_needle_is_read_as_failed() {
        let eval = Eval::parse(["RESULT long_ctx_29491=FAIL prompt_tok_s=0 gen_tok_s_at_depth=0"]);
        let needle = eval.needle.expect("needle result");
        assert_eq!(needle.verdict, Verdict::Fail);
    }

    #[test]
    fn the_last_needle_line_wins() {
        // A retried run prints more than one; the last is the verdict.
        let eval = Eval::parse([
            "RESULT long_ctx_7372=FAIL prompt_tok_s=1",
            "RESULT long_ctx_7372=PASS prompt_tok_s=19311",
        ]);
        assert_eq!(eval.needle.expect("needle").verdict, Verdict::Pass);
    }

    #[test]
    fn a_run_with_no_result_lines_yields_nothing() {
        let eval = Eval::parse(["[FAIL] server not responding", "answer: (request failed)"]);
        assert_eq!(eval, Eval::default());
        assert!(eval.needle.is_none());
    }

    #[test]
    fn the_tuners_own_result_line_is_not_confused_for_a_needle() {
        // `RESULT tune_preset=... ctx=... decode=...` is a tuner line.
        let eval = Eval::parse(["RESULT tune_preset=smoke-high engine=llama.cpp n_cpu_moe=2 ctx=32768 decode=312.5 prefill=18681 vram=1515"]);
        assert!(eval.needle.is_none());
        assert_eq!(eval.gen_tok_s_best, None);
    }
}
