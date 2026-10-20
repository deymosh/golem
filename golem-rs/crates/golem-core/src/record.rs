//! The row builder: `scripts/record-result.sh`, ported.
//!
//! A bench or an eval prints its numbers on one `RESULT` line; everything
//! else in a row - the machine, the engine build, the model file, the flags
//! the preset actually served, the model's own facts - is read from the
//! machine and the home at the moment of recording. That is what this module
//! does, and it is the whole reason a measurement can be reproduced later.
//!
//! It is the port of `scripts/record-result.sh`, and it must stay
//! byte-compatible with it: `bench/results.jsonl` is append-only and holds
//! rows the shell wrote, so a Rust row that spells a field differently is a
//! row the report reads differently. Verify with a diff, never by reading.
//!
//! The split: **this module is the part that can be tested** - the row, the
//! flags a preset effectively serves, the model's own facts - and the
//! binary gathers the machine facts (nvidia-smi, `/props`, the process tree)
//! and hands them in.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;

use crate::results::Kind;
use crate::{evalparse, gguf, json};

/// One effective flag's value, and how the shell renders it.
///
/// The distinction is not cosmetic. A value that came out of a preset goes
/// through the awk's own test (numeric or `true`/`false` bare, everything
/// else quoted), while the single-model branch builds its two keys with
/// `json_str` and `json_num` directly - so `extra` is *always* quoted even
/// when it looks like a number, and `ctx-size` is the bare `null` when
/// `CONTEXT_SIZE` is unset rather than an empty string. Collapsing the three
/// into one rule makes a row the shell wrote and a row this writes disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlagValue {
    /// From a preset: bare when numeric or `true`/`false`, quoted otherwise.
    Preset(String),
    /// Always quoted, however numeric it looks (`json_str`).
    Text(String),
    /// The shell's `json_num`: the number when it is one, the bare `null`
    /// when it is not - which is also what an unset value becomes.
    Number(String),
}

/// One effective flag: its key and how its value is written.
pub type Flag = (String, FlagValue);

/// Everything in a row that does not come from the captured output.
#[derive(Debug, Clone)]
pub struct Facts {
    pub kind: Kind,
    /// ISO-8601 UTC; `time::now_iso()` when the caller does not pin one.
    pub ts: String,
    pub host_slug: String,
    pub gpu: String,
    pub os: String,
    pub mode: String,
    pub engine: String,
    pub engine_build: String,
    pub model: String,
    pub model_file: String,
    pub ctx: Option<u64>,
    pub threads: Option<u32>,
    pub params_b: Option<f64>,
    pub params_active_b: Option<f64>,
    pub moe: bool,
    pub experts: Option<String>,
    pub n_layer: Option<u32>,
    pub ctx_train: Option<u64>,
    /// The preset's effective flags, in file order.
    pub flags: Vec<Flag>,
    pub vram_mib: Option<u64>,
    pub ram_used_gib: Option<f64>,
    pub ram_free_gib: Option<f64>,
    pub quiet: bool,
    pub note: String,
}

/// `Facts` is `Default` so a caller fills in only what it knows and leaves the
/// rest empty. `results::Kind` has no `Default` of its own, so the fallback is
/// spelled out here rather than derived.
impl Default for Facts {
    fn default() -> Facts {
        Facts {
            kind: Kind::Unknown,
            ts: String::new(),
            host_slug: String::new(),
            gpu: String::new(),
            os: String::new(),
            mode: String::new(),
            engine: String::new(),
            engine_build: String::new(),
            model: String::new(),
            model_file: String::new(),
            ctx: None,
            threads: None,
            params_b: None,
            params_active_b: None,
            moe: false,
            experts: None,
            n_layer: None,
            ctx_train: None,
            flags: Vec::new(),
            vram_mib: None,
            ram_used_gib: None,
            ram_free_gib: None,
            quiet: false,
            note: String::new(),
        }
    }
}

/// One JSON line for `captured`, or `Err` when there is nothing to record.
///
/// The members are assembled in the printf's own order (record-result.sh
/// lines 270-278) and joined with `", "` / `": "` - `json.dumps` spacing -
/// so the line is byte-identical to the shell's.
pub fn row(facts: &Facts, captured: &str) -> Result<String, String> {
    // The shell's `grep -q '^RESULT'`: a run with no RESULT line is not a
    // measurement (a failed or interrupted run), and recording it would put a
    // null row in the database.
    if !captured.lines().any(|line| line.starts_with("RESULT")) {
        return Err("no RESULT line in the captured output - nothing to record".into());
    }

    let mut members: Vec<String> = Vec::new();
    members.push(format!("\"ts\": \"{}\"", facts.ts));
    members.push(format!("\"kind\": \"{}\"", kind_name(facts.kind)));
    members.push(format!("\"host\": {}", json::quote(&facts.host_slug)));
    members.push(format!("\"gpu\": {}", json::quote(&facts.gpu)));
    members.push(format!("\"os\": \"{}\"", facts.os));
    members.push(format!("\"mode\": \"{}\"", facts.mode));
    members.push(format!("\"engine\": {}", json::quote(&facts.engine)));
    members.push(format!("\"engine_build\": {}", json::quote(&facts.engine_build)));
    members.push(format!("\"model\": {}", json::quote(&facts.model)));
    members.push(format!("\"model_file\": {}", json::quote(&facts.model_file)));
    members.push(format!("\"ctx\": {}", num_opt_u64(facts.ctx)));
    members.push(format!("\"threads\": {}", num_opt_u32(facts.threads)));
    members.push(format!("\"params_b\": {}", num_opt_f2(facts.params_b)));
    members.push(format!("\"params_active_b\": {}", num_opt_f2(facts.params_active_b)));
    members.push(format!("\"moe\": {}", facts.moe));
    members.push(format!(
        "\"experts\": {}",
        match &facts.experts {
            Some(e) => json::quote(e),
            None => "null".to_string(),
        }
    ));
    members.push(format!("\"n_layer\": {}", num_opt_u32(facts.n_layer)));
    members.push(format!("\"ctx_train\": {}", num_opt_u64(facts.ctx_train)));
    members.push(format!("\"flags\": {}", flags_json(&facts.flags)));
    // The kind-specific metrics object's members, inlined where the printf
    // puts `${METRICS}` (the shell's `if [[ "${KIND}" == bench ]] ... else`).
    members.push(match facts.kind {
        Kind::Bench => bench_metrics(captured),
        _ => eval_metrics(captured),
    });
    members.push(format!("\"vram_mib\": {}", num_opt_u64(facts.vram_mib)));
    members.push(format!("\"ram_used_gib\": {}", num_opt_f1(facts.ram_used_gib)));
    members.push(format!("\"ram_free_gib\": {}", num_opt_f1(facts.ram_free_gib)));
    members.push(format!("\"quiet\": {}", facts.quiet));
    members.push(format!("\"note\": {}", json::quote(&facts.note)));

    let mut out = String::with_capacity(512);
    out.push('{');
    out.push_str(&members.join(", "));
    out.push_str("}\n");
    Ok(out)
}

/// Append one row to `bench/results.jsonl` under `home`.
pub fn append(home: &Path, line: &str) -> std::io::Result<()> {
    let dir = home.join("bench");
    std::fs::create_dir_all(&dir)?;
    let mut file = OpenOptions::new().create(true).append(true).open(dir.join("results.jsonl"))?;
    file.write_all(line.as_bytes())
}

/// The `kind` field's spelling. `results::Kind` carries the same words.
fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Bench => "bench",
        Kind::Eval => "eval",
        Kind::Reject => "reject",
        Kind::Unknown => "unknown",
    }
}

/// A number, or bare `null` when it was not measured - the shell's `json_num`
/// (`-?[0-9]+(\.[0-9]+)?`, so no `+`, no exponent).
fn num_opt_u64(v: Option<u64>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

fn num_opt_u32(v: Option<u32>) -> String {
    match v {
        Some(n) => n.to_string(),
        None => "null".to_string(),
    }
}

/// `%.2f`, the shell's awk format for `params_b` and `params_active_b`.
fn num_opt_f2(v: Option<f64>) -> String {
    match v {
        Some(n) => format!("{n:.2}"),
        None => "null".to_string(),
    }
}

/// `%.1f`, the shell's format for the RAM figures.
fn num_opt_f1(v: Option<f64>) -> String {
    match v {
        Some(n) => format!("{n:.1}"),
        None => "null".to_string(),
    }
}

/// The shell's `json_num`: a numeric string passes through, anything else is
/// `null`.
fn json_num(value: &str) -> String {
    if is_json_num(value) {
        value.to_string()
    } else {
        "null".to_string()
    }
}

/// `^-?[0-9]+(\.[0-9]+)?$` - the one number shape the scripts accept.
fn is_json_num(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    match body.split_once('.') {
        Some((int, frac)) => {
            !int.is_empty()
                && int.bytes().all(|b| b.is_ascii_digit())
                && !frac.is_empty()
                && frac.bytes().all(|b| b.is_ascii_digit())
        }
        None => !body.is_empty() && body.bytes().all(|b| b.is_ascii_digit()),
    }
}

/// `flags` as a JSON object, in the order the preset gave them. Each value is
/// rendered the way the branch that produced it renders it - see
/// [`FlagValue`]; the awk's END block is record-result.sh lines 181-191 and
/// the single-model branch is line 193.
fn flags_json(flags: &[Flag]) -> String {
    let mut members: Vec<String> = Vec::with_capacity(flags.len());
    for (key, value) in flags {
        let rendered = match value {
            FlagValue::Preset(value) => flag_value(value),
            FlagValue::Text(value) => json::quote(value),
            FlagValue::Number(value) => json_num(value),
        };
        members.push(format!("{}: {}", json::quote(key), rendered));
    }
    format!("{{{}}}", members.join(", "))
}

fn flag_value(value: &str) -> String {
    if is_json_num(value) || value == "true" || value == "false" {
        value.to_string()
    } else {
        json::quote(value)
    }
}

/// The last value of `key=` across the `RESULT` lines, or empty - the shell's
/// `result_field` (`grep '^RESULT' | tr ' ' '\n' | grep "^$key=" | tail -n1`).
fn result_field(captured: &str, key: &str) -> String {
    let mut found = String::new();
    for line in captured.lines() {
        if !line.starts_with("RESULT") {
            continue;
        }
        for (k, v) in evalparse::fields(line) {
            if k == key {
                found = v;
            }
        }
    }
    found
}

/// The bench metrics object's members, inlined. record-result.sh line 258.
fn bench_metrics(captured: &str) -> String {
    let decode_best = json_num(&result_field(captured, "decode_best"));
    let decode_median = json_num(&result_field(captured, "decode_median"));
    let prefill = json_num(&result_field(captured, "prefill"));
    let draft_accept = draft_accept(captured);
    format!(
        "\"decode_best\": {decode_best}, \"decode_median\": {decode_median}, \
         \"draft_accept\": {draft_accept}, \"prefill\": {prefill}"
    )
}

/// The acceptance as a fraction printed to two decimals: the `draft_accept=`
/// field with `%` stripped and divided by 100. Empty and the literal `-`
/// (speculative decoding off) mean "not measured".
fn draft_accept(captured: &str) -> String {
    let raw = result_field(captured, "draft_accept").replace('%', "");
    if raw.is_empty() || raw == "-" {
        return "null".to_string();
    }
    // `awk -v a=... 'BEGIN {printf "%.2f", a/100}'`: a non-numeric value reads
    // as 0 there too.
    let percent: f64 = raw.parse().unwrap_or(0.0);
    format!("{:.2}", percent / 100.0)
}

/// The eval metrics object's members, inlined. record-result.sh line 264.
fn eval_metrics(captured: &str) -> String {
    let tools = if captured.lines().any(|line| line.starts_with("[ OK ] tool calling")) {
        "PASS"
    } else {
        "FAIL"
    };
    let needle = match last_needle_key(captured) {
        Some(key) => json::quote(&result_field(captured, &key)),
        None => "null".to_string(),
    };
    let needle_tokens = json_num(&last_needle_prompt(captured));
    let needle_prefill = json_num(&result_field(captured, "prompt_tok_s"));
    let depth_decode = json_num(&result_field(captured, "gen_tok_s_at_depth"));
    let decode_best = json_num(&result_field(captured, "gen_tok_s_best"));
    format!(
        "\"eval\": {{\"tools\": \"{tools}\", \"needle\": {needle}, \
         \"needle_tokens\": {needle_tokens}, \"needle_prefill\": {needle_prefill}, \
         \"depth_decode\": {depth_decode}, \"decode_best\": {decode_best}}}"
    )
}

/// The last `long_ctx_<N>` key on a `RESULT` line, as the shell's
/// `grep -o '^long_ctx_[0-9]*' | tail -n1` takes it.
fn last_needle_key(captured: &str) -> Option<String> {
    let mut found = None;
    for line in captured.lines() {
        if !line.starts_with("RESULT") {
            continue;
        }
        for (key, _) in evalparse::fields(line) {
            let Some(tokens) = key.strip_prefix("long_ctx_") else { continue };
            if !tokens.is_empty() && tokens.bytes().all(|b| b.is_ascii_digit()) {
                found = Some(key);
            }
        }
    }
    found
}

/// The token count of the last `needle : prompt=<N>` in the captured text.
fn last_needle_prompt(captured: &str) -> String {
    const MARKER: &str = "needle : prompt=";
    let mut found = String::new();
    for line in captured.lines() {
        if let Some(at) = line.rfind(MARKER) {
            let rest = &line[at + MARKER.len()..];
            found = rest.chars().take_while(char::is_ascii_digit).collect();
        }
    }
    found
}

/// The flags a preset effectively serves for `alias`: the shared `[*]`
/// section first, then the alias's own section, in file order, one entry per
/// key, the later value winning. `model` is skipped. This is the awk at
/// record-result.sh lines 172-191.
pub fn flags_of(preset_text: &str, alias: &str) -> Vec<Flag> {
    let doc = crate::ini::parse(preset_text);
    let mut out: Vec<Flag> = Vec::new();
    for section in &doc.sections {
        if section.name != "*" && section.name != alias {
            continue;
        }
        for entry in &section.entries {
            if entry.key == "model" {
                continue;
            }
            // A key already seen keeps its first position while the later
            // value wins (`f[k] = v` with `order[]` recorded once).
            let value = FlagValue::Preset(entry.value.clone());
            match out.iter_mut().find(|(k, _)| *k == entry.key) {
                Some((_, existing)) => *existing = value,
                None => out.push((entry.key.clone(), value)),
            }
        }
    }
    out
}

/// The `ctx-size` the preset serves for `alias` (alias section only).
///
/// The awk prints the first such line and exits, so a duplicate key does not
/// override the first (unlike `Section::value`, which takes the last).
pub fn ctx_of(preset_text: &str, alias: &str) -> Option<u64> {
    section_key(preset_text, alias, "ctx-size")?.parse().ok()
}

/// The `threads` the preset sets for `alias` (alias section only).
pub fn threads_of(preset_text: &str, alias: &str) -> Option<u32> {
    section_key(preset_text, alias, "threads")?.parse().ok()
}

/// The first value of `key` in `alias`'s own section.
fn section_key(preset_text: &str, alias: &str, key: &str) -> Option<String> {
    let doc = crate::ini::parse(preset_text);
    let section = doc.get(alias)?;
    section.entries.iter().find(|e| e.key == key).map(|e| e.value.clone())
}

/// What the GGUF header says the model is.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelFacts {
    pub params_b: Option<f64>,
    pub params_active_b: Option<f64>,
    pub moe: bool,
    pub experts: Option<String>,
    pub n_layer: Option<u32>,
    pub ctx_train: Option<u64>,
}

/// Read a model file's facts from its GGUF header. All-defaults when the file
/// is missing or unreadable: never panics, never errors.
pub fn model_facts(path: &Path) -> ModelFacts {
    let Ok(facts) = gguf::read(path) else {
        return ModelFacts::default();
    };
    ModelFacts {
        params_b: params_billion(facts.params_total),
        params_active_b: params_billion(facts.params_active),
        moe: facts.moe,
        // `<n_expert>x<n_expert_used>`, only when the model is MoE.
        experts: facts.moe.then(|| format!("{}x{}", facts.n_expert, facts.n_expert_used)),
        n_layer: Some(facts.n_layer as u32),
        ctx_train: Some(facts.n_ctx_train),
    }
}

/// The parameter count in billions, printed to two decimals. The shell's
/// `awk 'BEGIN {if (v+0 > 0) printf "%.2f", v/1e9}'`: nothing when the count
/// is 0.
fn params_billion(params: u64) -> Option<f64> {
    if params == 0 {
        return None;
    }
    Some(((params as f64) / 1e9 * 100.0).round() / 100.0)
}

/// The anonymous hardware slug a row carries: lower case, the marketing
/// words dropped, everything else runs of one dash. `scripts/record-result.sh`
/// line 92, and the same function `scripts/tune.sh` uses for its output
/// directory - the two must agree, because a row and its tuned setup are
/// filed under this name.
pub fn slug(text: &str) -> String {
    let mut stripped = text.to_lowercase().replace("(r)", "").replace("(tm)", "");
    for word in ["nvidia", "geforce", "intel", "core", "amd", "radeon", "processor"] {
        stripped = stripped.replace(word, "");
    }
    stripped = strip_generation(&stripped);
    let mut out = String::new();
    let mut in_run = false;
    for c in stripped.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            in_run = false;
        } else if !in_run {
            out.push('-');
            in_run = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// The `[0-9]*th gen` in a CPU's marketing name ("12th Gen Intel...").
fn strip_generation(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("th gen") {
        let before = &rest[..at];
        let digits = before.chars().rev().take_while(char::is_ascii_digit).count();
        out.push_str(&before[..before.len() - digits]);
        rest = &rest[at + "th gen".len()..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bench_facts() -> Facts {
        Facts {
            kind: Kind::Bench,
            ts: "2026-10-20T00:00:00Z".into(),
            host_slug: "rtx-3060_12g_32g_i7-12700kf".into(),
            gpu: "NVIDIA GeForce RTX 3060".into(),
            os: "windows".into(),
            mode: "native".into(),
            engine: "llama.cpp".into(),
            engine_build: "b11379-1537a0a8b".into(),
            model: "ornith-1.5-35b".into(),
            model_file: "Ornith-1.5-35B-Q4_K_M.gguf".into(),
            ctx: Some(236544),
            threads: Some(16),
            params_b: Some(35.51),
            params_active_b: Some(3.52),
            moe: true,
            experts: Some("256x8".into()),
            n_layer: Some(40),
            ctx_train: Some(262144),
            flags: vec![
                ("ctx-size".into(), FlagValue::Preset("236544".into())),
                ("jinja".into(), FlagValue::Preset("true".into())),
            ],
            vram_mib: Some(11000),
            ram_used_gib: Some(19.4),
            ram_free_gib: Some(8.1),
            quiet: true,
            note: "first pass".into(),
        }
    }

    #[test]
    fn the_whole_line_matches_the_shell_byte_for_byte() {
        let captured = "RESULT decode_best=59.6 decode_median=59.3 draft_accept=66% prefill=832 vram=11000\n";
        let line = row(&bench_facts(), captured).expect("should record");
        let expected = concat!(
            "{\"ts\": \"2026-10-20T00:00:00Z\", \"kind\": \"bench\", ",
            "\"host\": \"rtx-3060_12g_32g_i7-12700kf\", ",
            "\"gpu\": \"NVIDIA GeForce RTX 3060\", \"os\": \"windows\", ",
            "\"mode\": \"native\", \"engine\": \"llama.cpp\", ",
            "\"engine_build\": \"b11379-1537a0a8b\", \"model\": \"ornith-1.5-35b\", ",
            "\"model_file\": \"Ornith-1.5-35B-Q4_K_M.gguf\", \"ctx\": 236544, ",
            "\"threads\": 16, \"params_b\": 35.51, \"params_active_b\": 3.52, ",
            "\"moe\": true, \"experts\": \"256x8\", \"n_layer\": 40, ",
            "\"ctx_train\": 262144, \"flags\": {\"ctx-size\": 236544, \"jinja\": true}, ",
            "\"decode_best\": 59.6, \"decode_median\": 59.3, \"draft_accept\": 0.66, ",
            "\"prefill\": 832, \"vram_mib\": 11000, \"ram_used_gib\": 19.4, ",
            "\"ram_free_gib\": 8.1, \"quiet\": true, \"note\": \"first pass\"}\n"
        );
        assert_eq!(line, expected);
        // It is one line, and it parses as the row the report reads.
        assert_eq!(line.matches('\n').count(), 1);
        let parsed = crate::results::Row::parse(line.trim_end()).expect("should parse");
        assert_eq!(parsed.kind, Kind::Bench);
        assert_eq!(parsed.decode_median.as_ref().map(|n| n.value), Some(59.3));
        assert_eq!(parsed.draft_accept, Some(0.66));
    }

    #[test]
    fn the_eval_metrics_come_out_of_the_captured_run() {
        let captured = concat!(
            "[ OK ] tool calling\n",
            "run 1: prompt=115 tok @ 7977 t/s, gen=192 tok @ 328.0 t/s\n",
            "RESULT gen_tok_s_best=330.2\n",
            "needle : prompt=6065 tok @ 19311 t/s; answer: 8127\n",
            "RESULT long_ctx_7372=PASS prompt_tok_s=19311 gen_tok_s_at_depth=254.3\n"
        );
        let mut facts = bench_facts();
        facts.kind = Kind::Eval;
        let line = row(&facts, captured).expect("should record");
        let expected = concat!(
            "\"eval\": {\"tools\": \"PASS\", \"needle\": \"PASS\", ",
            "\"needle_tokens\": 6065, \"needle_prefill\": 19311, ",
            "\"depth_decode\": 254.3, \"decode_best\": 330.2}"
        );
        assert!(line.contains(expected), "eval metrics missing from:\n{line}");
        // And it round-trips through the report's reader.
        let parsed = crate::results::Row::parse(line.trim_end()).expect("should parse");
        let eval = parsed.eval.expect("eval object");
        assert_eq!(eval.tools.as_deref(), Some("PASS"));
        assert_eq!(eval.needle.as_deref(), Some("PASS"));
        assert_eq!(eval.needle_tokens, Some(6065));
        assert_eq!(eval.depth_decode, Some(254.3));
    }

    #[test]
    fn a_failed_tool_call_and_no_needle_read_as_fail_and_null() {
        let mut facts = bench_facts();
        facts.kind = Kind::Eval;
        let captured = "RESULT gen_tok_s_best=12.5\n";
        let line = row(&facts, captured).expect("should record");
        let expected = concat!(
            "\"eval\": {\"tools\": \"FAIL\", \"needle\": null, ",
            "\"needle_tokens\": null, \"needle_prefill\": null, ",
            "\"depth_decode\": null, \"decode_best\": 12.5}"
        );
        assert!(line.contains(expected), "eval metrics missing from:\n{line}");
    }

    #[test]
    fn absent_fields_are_bare_null() {
        // A bench line with only a decode: everything else is not measured.
        let captured = "RESULT decode_best=59.6\n";
        let facts = Facts {
            kind: Kind::Bench,
            ts: "t".into(),
            model: "m".into(),
            ..Facts::default()
        };
        let line = row(&facts, captured).expect("should record");
        assert!(line.contains("\"ctx\": null, \"threads\": null"), "{line}");
        assert!(line.contains("\"params_b\": null, \"params_active_b\": null"), "{line}");
        assert!(line.contains("\"experts\": null, \"n_layer\": null, \"ctx_train\": null"), "{line}");
        assert!(line.contains("\"flags\": {}"), "{line}");
        assert!(line.contains("\"decode_best\": 59.6, \"decode_median\": null"), "{line}");
        assert!(line.contains("\"draft_accept\": null, \"prefill\": null"), "{line}");
        assert!(line.contains("\"vram_mib\": null, \"ram_used_gib\": null"), "{line}");
        assert!(line.contains("\"ram_free_gib\": null, \"quiet\": false, \"note\": \"\"}"), "{line}");
    }

    #[test]
    fn a_dash_acceptance_is_not_measured() {
        let captured = "RESULT decode_best=12.5 decode_median=12.1 draft_accept=- prefill=-\n";
        let line = row(&bench_facts(), captured).expect("should record");
        assert!(line.contains("\"draft_accept\": null, \"prefill\": null"), "{line}");
    }

    #[test]
    fn a_run_with_no_result_line_is_not_a_measurement() {
        let captured = "[FAIL] server not responding\nanswer: (request failed)\n";
        assert!(row(&bench_facts(), captured).is_err());
        assert!(row(&bench_facts(), "").is_err());
    }

    #[test]
    fn flags_of_merges_the_shared_section_then_the_alias() {
        let preset = "\
; a comment
version = 1

[*]
jinja = true
n-cpu-moe = 40

[model-a]
model = a.gguf
ctx-size = 4096
n-cpu-moe = 20

[model-b]
model = b.gguf
";
        // The shared keys first, the later value winning, `model` skipped, and
        // a key new to the alias appended after the shared ones.
        assert_eq!(
            flags_of(preset, "model-a"),
            vec![
                ("jinja".to_string(), FlagValue::Preset("true".to_string())),
                ("n-cpu-moe".to_string(), FlagValue::Preset("20".to_string())),
                ("ctx-size".to_string(), FlagValue::Preset("4096".to_string())),
            ]
        );
        // Another alias sees only the shared section.
        assert_eq!(
            flags_of(preset, "model-b"),
            vec![
                ("jinja".to_string(), FlagValue::Preset("true".to_string())),
                ("n-cpu-moe".to_string(), FlagValue::Preset("40".to_string())),
            ]
        );
    }

    #[test]
    fn the_single_model_branch_renders_its_two_keys_the_shells_way() {
        // `json_str` quotes `extra` even when it looks numeric, and `json_num`
        // on an absent `CONTEXT_SIZE` is the bare null, not an empty string.
        let facts = Facts {
            flags: vec![
                ("ctx-size".to_string(), FlagValue::Number(String::new())),
                ("extra".to_string(), FlagValue::Text("2048".to_string())),
            ],
            ..Facts::default()
        };
        let line = row(&facts, "RESULT decode_best=1\n").expect("a row");
        assert!(line.contains("\"flags\": {\"ctx-size\": null, \"extra\": \"2048\"}"), "{line}");
    }

    #[test]
    fn ctx_and_threads_come_from_the_alias_section_only() {
        let preset = "\
[*]
ctx-size = 100
threads = 2

[model-a]
ctx-size = 4096
threads = 16

[model-b]
ctx-size = 8192
";
        assert_eq!(ctx_of(preset, "model-a"), Some(4096));
        assert_eq!(ctx_of(preset, "model-b"), Some(8192));
        assert_eq!(threads_of(preset, "model-a"), Some(16));
        // model-b sets no threads, and the shared one does not leak in.
        assert_eq!(threads_of(preset, "model-b"), None);
        assert_eq!(ctx_of(preset, "nope"), None);
        assert_eq!(threads_of(preset, "nope"), None);
    }

    #[test]
    fn a_missing_model_file_is_all_defaults() {
        let facts = model_facts(Path::new("definitely/not/here/absent.gguf"));
        assert_eq!(facts, ModelFacts::default());
        assert_eq!(facts.params_b, None);
        assert_eq!(facts.params_active_b, None);
        assert!(!facts.moe);
        assert_eq!(facts.experts, None);
        assert_eq!(facts.n_layer, None);
        assert_eq!(facts.ctx_train, None);
    }

    #[test]
    fn a_parameter_count_becomes_billions_to_two_decimals() {
        assert_eq!(params_billion(0), None, "the shell prints nothing for 0");
        assert_eq!(params_billion(35_510_000_000), Some(35.51));
        assert_eq!(params_billion(3_520_000_000), Some(3.52));
    }

    #[test]
    fn the_slug_is_the_one_the_recorder_writes() {
        assert_eq!(slug("NVIDIA GeForce RTX 3060"), "rtx-3060");
        assert_eq!(slug("12th Gen Intel(R) Core(TM) i7-12700KF"), "i7-12700kf");
        assert_eq!(slug("AMD Ryzen 9 5950X"), "ryzen-9-5950x");
        assert_eq!(slug("NVIDIA GeForce RTX 4090"), "rtx-4090");
        assert_eq!(slug("AMD Radeon RX 7900 XTX"), "rx-7900-xtx");
        assert_eq!(slug("Apple M3 Max"), "apple-m3-max");
        assert_eq!(slug(""), "");
    }

    #[test]
    fn append_creates_the_directory_and_adds_the_line() {
        let dir = std::env::temp_dir().join("golem-rs-record-test");
        std::fs::remove_dir_all(&dir).ok();
        append(&dir, "{\"a\": 1}\n").expect("append");
        append(&dir, "{\"b\": 2}\n").expect("append");
        let text = std::fs::read_to_string(dir.join("bench/results.jsonl")).expect("read");
        assert_eq!(text, "{\"a\": 1}\n{\"b\": 2}\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}
