//! The Markdown results table (`golem report --md`).
//!
//! GitHub strips HTML and JavaScript from Markdown, so the interactive page
//! can never be embedded in a README: this is the same data as a table, and
//! it is the view people actually link to.
//!
//! The report groups hardware first. One `##` section per `host` - the
//! machine slug the recorder wrote - so two boxes never share a table and a
//! number is always read next to the hardware that produced it. Inside a
//! host, one block per model, and inside that one line per context: the best
//! setup measured at that context. The size classes survive only as an
//! ordering rule (a 0.5B at 269 t/s says nothing about a 35B at 59, so the
//! classes keep like with like).

use std::fmt::Write as _;

use crate::results::{best_per_configuration, host_key, Best, Kind, Row};

const HEADER: &str = "\
# Benchmark results

Generated from [`results/results.jsonl`](results.jsonl) by `golem report --md`.

**One section per machine.** The heading is the `host` slug the recorder wrote - GPU, VRAM,
RAM and CPU in one string, so `rtx-3060_12g_32g_i7-12700kf` is a 12 GB RTX 3060 with 32 GB
of RAM and an i7-12700KF - and machines never share a table: the same flags on another box
are that box's numbers, not a comparison.

Inside a machine, one block per model, and inside that one line per context: the best setup
measured at that context, with the flags that won named on the row. The tuning that lost is
still in [`results.jsonl`](results.jsonl) and in the console. Compare inside a size class
only - a 0.5B at 269 t/s says nothing about a 35B at 59 - and `t/s / active B` is the one
honest cross-size ratio (active parameters are what the GPU pushes per token: 3.5B for a
35B-A3B).
";

/// The header and the delimiter row of a model table.
///
/// The two must have the same number of cells or a Markdown renderer drops
/// the table and prints the pipes as text - a failure that is invisible in
/// the source and obvious only in the rendered page, so
/// `every_table_delimiter_matches_its_header` checks it.
const TABLE_HEAD: &str = "\
| ctx | engine | setup | decode t/s | prefill t/s | t/s / active B | VRAM GB | RAM GB | MTP acc. | needle | measured |
|---:|---|---|---:|---:|---:|---:|---:|---:|---|---|";

const REJECT_HEAD: &str = "\
### Rejected, with the measured reason

| model | engine | why |
|---|---|---|";

const FOOTER: &str = "Every row, with its flags and notes, is in [`results.jsonl`](results.jsonl).";

/// Flags that already have a column of their own, so repeating them in the
/// setup column would only widen the table.
const OWN_COLUMN: [&str; 1] = ["ctx-size"];

/// The flags that describe how the model was placed and whether it
/// speculated, in the order the column shows them.
///
/// These are shown on **every** row, not only when they differ: a reader
/// asking "what was actually run here?" wants the offload split and the
/// speculation on the row in front of them, and a column that empties itself
/// when the values happen to agree answers nothing.
///
/// Deliberately short. The rest of the preset - the batch, the load mode,
/// the sampling - is the same on every row here and belongs in
/// `results.jsonl`; anything of it that *does* differ between rows is
/// appended after these, so nothing that tells two rows apart is lost, and
/// the column stays narrow enough to read.
const SETUP_KEYS: [&str; 4] = ["n-cpu-moe", "n-cpu-ffn", "spec-type", "fit"];

/// The flag keys whose value differs somewhere in the report.
///
/// These are the ones that tell rows apart - the offload split, the
/// speculation, the load mode - so they are the ones a "setup" column should
/// show. A key every row shares (the batch, the GPU layer count) explains no
/// difference between the rows and only widens the table, so it is left out:
/// the winning setup is then short enough to read at a glance, and the full
/// flag set is one click away in `results.jsonl`.
///
/// "Not set at all" is a value like any other, so a knob the tuned rows carry
/// and the default ones do not counts as differing - which is exactly what
/// makes it worth printing.
fn distinguishing_keys(rows: &[Row]) -> Vec<String> {
    // Only the values rows actually carry are compared. Counting a missing
    // flag as a value of its own would be wrong here: a row recorded by
    // another engine (or before a field existed) says nothing about the
    // flag, and treating that silence as a difference would pull every
    // uniform preset default into the column.
    let mut seen: Vec<(String, Vec<String>)> = Vec::new();
    for row in rows {
        for (key, value) in &row.flags {
            if OWN_COLUMN.contains(&key.as_str()) {
                continue;
            }
            match seen.iter_mut().find(|(k, _)| k == key) {
                Some((_, values)) => {
                    if !values.iter().any(|v| v == value) {
                        values.push(value.clone());
                    }
                }
                None => seen.push((key.clone(), vec![value.clone()])),
            }
        }
    }
    seen.into_iter()
        .filter(|(_, values)| values.len() > 1)
        .map(|(key, _)| key)
        .collect()
}

/// The setup a row measured: the tuned thread count first, then the knobs
/// that matter, in their own order, then any other flag that differs
/// somewhere in the report. Context has a column of its own and is not
/// repeated here.
fn setup_summary(row: &Row, distinguishing: &[String]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let value =
        |key: &str| row.flags.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
    if let Some(threads) = row.threads {
        parts.push(format!("threads={threads}"));
    }
    for key in SETUP_KEYS {
        if let Some(v) = value(key) {
            parts.push(format!("{key}={v}"));
        }
    }
    for key in distinguishing {
        if SETUP_KEYS.contains(&key.as_str()) {
            continue;
        }
        if let Some(v) = value(key) {
            parts.push(format!("{key}={v}"));
        }
    }
    if parts.is_empty() {
        "-".to_string()
    } else {
        parts.join(" ")
    }
}

/// The whole file, ready to write to results/RESULTS.md.
pub fn markdown(rows: &[Row]) -> String {
    let best = best_per_configuration(rows);
    let distinguishing = distinguishing_keys(rows);
    let mut out = String::new();
    out.push_str(HEADER);

    // One section per distinct host, ascending, an unknown host last. Taken
    // from every row - bench, eval and reject - so a machine that only ever
    // rejected a model still gets its own section.
    let mut hosts: Vec<&str> = Vec::new();
    for row in rows {
        if !hosts.iter().any(|h| *h == row.host.as_str()) {
            hosts.push(row.host.as_str());
        }
    }
    hosts.sort_by(|a, b| host_key(a).cmp(&host_key(b)));

    for host in hosts {
        out.push('\n');
        let _ = writeln!(out, "## {}", display_host(host));

        let host_rows: Vec<&Row> = rows.iter().filter(|r| r.host == host).collect();
        if let Some(caption) = caption_line(&host_rows) {
            out.push('\n');
            out.push_str(&caption);
            out.push('\n');
        }

        // The best rows are already ordered, so a model block is a run of
        // consecutive entries that name the same model.
        let host_best: Vec<&Best> = best.iter().filter(|e| e.row.host == host).collect();
        let mut i = 0;
        while i < host_best.len() {
            let model = &host_best[i].row.model;
            let mut j = i + 1;
            while j < host_best.len() && &host_best[j].row.model == model {
                j += 1;
            }
            out.push('\n');
            let _ = writeln!(out, "### {}", heading(&host_best[i].row));
            out.push('\n');
            out.push_str(TABLE_HEAD);
            out.push('\n');
            for entry in &host_best[i..j] {
                out.push_str(&table_row(entry, &distinguishing));
                out.push('\n');
            }
            i = j;
        }

        let rejected: Vec<&Row> =
            rows.iter().filter(|r| r.kind == Kind::Reject && r.host == host).collect();
        if !rejected.is_empty() {
            out.push('\n');
            out.push_str(REJECT_HEAD);
            out.push('\n');
            for row in rejected {
                let _ = writeln!(
                    out,
                    "| {} | {} `{}` | {} |",
                    row.model, row.engine, row.engine_build, row.note
                );
            }
        }
    }

    out.push('\n');
    out.push_str(FOOTER);
    out.push('\n');
    out
}

/// The host heading's text: the slug, or "unknown host" for the rows that
/// carry none.
fn display_host(host: &str) -> &str {
    if host.is_empty() {
        "unknown host"
    } else {
        host
    }
}

/// `{model} — {shape}`, the block heading. The shape names the parameters
/// and, for a mixture of experts, the split: a 35B that does 3.5B of work
/// per token is not the same machine load as a dense 35B.
fn heading(row: &Row) -> String {
    match (row.params_b, row.params_active_b) {
        (Some(total), Some(active)) if active != total => {
            let mut s =
                format!("{} \u{2014} {total:.2}B total, {active:.2}B active", row.model);
            if let Some(experts) = &row.experts {
                let _ = write!(s, ", {experts} experts");
            }
            s
        }
        (Some(params), _) => format!("{} \u{2014} {params:.2}B", row.model),
        (None, _) => row.model.clone(),
    }
}

/// The caption under a host heading: the machine's GPU(s), then its os/mode
/// pairs, each in the order the rows first mention them. A host with neither
/// gets no caption line at all.
fn caption_line(rows: &[&Row]) -> Option<String> {
    let mut gpus: Vec<&str> = Vec::new();
    let mut pairs: Vec<String> = Vec::new();
    for row in rows {
        if let Some(gpu) = &row.gpu {
            if !gpus.iter().any(|g| *g == gpu.as_str()) {
                gpus.push(gpu);
            }
        }
        if row.os.is_some() || row.mode.is_some() {
            let pair = match (&row.os, &row.mode) {
                // `windows/windows` reads as a mistake: on Windows the
                // deploy mode and the OS are the same word, so say it once.
                (Some(os), Some(mode)) if os == mode => os.clone(),
                (Some(os), Some(mode)) => format!("{os}/{mode}"),
                (Some(os), None) => os.clone(),
                (None, Some(mode)) => mode.clone(),
                (None, None) => continue,
            };
            if !pairs.iter().any(|p| p == &pair) {
                pairs.push(pair);
            }
        }
    }
    let mut parts: Vec<String> = Vec::new();
    if !gpus.is_empty() {
        parts.push(gpus.join(", "));
    }
    if !pairs.is_empty() {
        parts.push(pairs.join(", "));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" \u{b7} "))
    }
}

fn table_row(entry: &Best, distinguishing: &[String]) -> String {
    let row = &entry.row;

    let ctx = match row.ctx {
        Some(c) => format!("{:.0}K", c as f64 / 1024.0),
        None => "-".to_string(),
    };
    let setup = setup_summary(row, distinguishing);
    // Printed as the file spelled them.
    let decode = row.decode_median.as_ref().map(|n| n.text.clone()).unwrap_or_default();
    let prefill = row.prefill.as_ref().map(|n| n.text.clone()).unwrap_or_default();

    // The ratio uses active parameters when the model is a mixture of
    // experts (that is the work per token), total parameters otherwise.
    let active = row.params_active_b.or(row.params_b);
    let ratio = match (row.decode_median.as_ref(), active) {
        (Some(d), Some(p)) if p > 0.0 => format!("{:.1}", d.value / p),
        _ => "-".to_string(),
    };

    let vram = match row.vram_mib {
        Some(v) => format!("{:.1}", v / 1024.0),
        None => "-".to_string(),
    };
    let ram = match row.ram_used_gib {
        Some(r) => format!("{r:.1}"),
        None => "-".to_string(),
    };
    let mtp = match row.draft_accept {
        Some(a) => format!("{:.0}%", a * 100.0),
        None => "-".to_string(),
    };
    let needle = entry.needle.clone().unwrap_or_else(|| "-".to_string());
    let day = row.ts.get(..10).unwrap_or(&row.ts);

    format!(
        "| {} | {} `{}` | {} | **{}** | {} | {} | {} | {} | {} | {} | {} |",
        ctx, row.engine, row.engine_build, setup, decode, prefill, ratio, vram, ram, mtp, needle,
        day
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(line: &str) -> Row {
        Row::parse(line).expect("should parse")
    }

    fn rows(lines: &[&str]) -> Vec<Row> {
        lines.iter().map(|l| row(l)).collect()
    }

    const BENCH: &str = r#"{"ts": "2026-10-19T18:02:22Z", "kind": "bench", "host": "test-box", "gpu": "Test GPU", "os": "linux", "mode": "native", "engine": "llama.cpp", "engine_build": "b11379-1537a0a8b", "model": "ornith-1.5-35b", "ctx": 236544, "threads": 16, "params_b": 35.51, "params_active_b": 3.52, "experts": "256x8", "flags": {"jinja": "true", "ctx-size": 236544, "n-cpu-moe": 36}, "decode_best": 59.6, "decode_median": 59.3, "draft_accept": 0.66, "prefill": 832, "vram_mib": 11000, "ram_used_gib": 19.4, "quiet": true, "note": ""}"#;

    const EVAL: &str = r#"{"ts": "2026-10-20T00:00:38Z", "kind": "eval", "host": "test-box", "gpu": "Test GPU", "os": "linux", "mode": "native", "model": "ornith-1.5-35b", "engine": "llama.cpp", "engine_build": "b11379-1537a0a8b", "ctx": 236544, "flags": {}, "eval": {"needle": "PASS"}, "note": ""}"#;

    const REJECT: &str = r#"{"ts": "2026-10-19T21:55:00Z", "kind": "reject", "host": "test-box", "gpu": "Test GPU", "os": "linux", "mode": "native", "model": "ornith-1.5-35b", "engine": "ik_llama.cpp", "engine_build": "main-b5483", "note": "rejected: 3.4x slower"}"#;

    /// The same row on a second machine, its flags and context untouched.
    /// The host is the recorder's anonymous hardware slug, never a hostname.
    fn on_second_box(line: &str) -> String {
        line.replace(
            r#""host": "test-box", "gpu": "Test GPU", "os": "linux", "mode": "native""#,
            r#""host": "rtx-4090_24g_64g_i9-13900k", "gpu": "NVIDIA GeForce RTX 4090", "os": "windows", "mode": "docker""#,
        )
    }

    #[test]
    fn assembles_sections_with_the_frozen_blank_lines() {
        // The whole document for one host, one model, one row: the blank
        // lines are part of the format, so they are asserted, not skimmed.
        let md = markdown(&rows(&[BENCH, EVAL]));
        let expected = format!(
            "{HEADER}\n\
             ## test-box\n\n\
             Test GPU \u{b7} linux/native\n\n\
             ### ornith-1.5-35b \u{2014} 35.51B total, 3.52B active, 256x8 experts\n\n\
             {TABLE_HEAD}\n\
             | 231K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=36 | **59.3** | 832 | 16.8 | 10.7 | 19.4 | 66% | PASS | 2026-10-19 |\n\
             \n\
             {FOOTER}\n"
        );
        assert_eq!(md, expected);
    }

    #[test]
    fn an_empty_file_prints_the_header_blank_and_footer() {
        assert_eq!(markdown(&[]), format!("{HEADER}\n{FOOTER}\n"));
    }

    #[test]
    fn a_host_without_a_gpu_or_os_gets_no_caption() {
        let bare = BENCH.replace(
            r#""gpu": "Test GPU", "os": "linux", "mode": "native", "#,
            "",
        );
        let md = markdown(&rows(&[&bare]));
        assert!(md.contains("## test-box\n\n### ornith-1.5-35b"), "{md}");
    }

    #[test]
    fn a_caption_lists_the_gpu_then_the_os_and_mode() {
        let md = markdown(&rows(&[BENCH, &on_second_box(BENCH)]));
        assert!(md.contains("## test-box\n\nTest GPU \u{b7} linux/native\n"), "{md}");
        assert!(
            md.contains(
                "## rtx-4090_24g_64g_i9-13900k\n\nNVIDIA GeForce RTX 4090 \u{b7} windows/docker\n"
            ),
            "{md}"
        );
    }

    #[test]
    fn a_model_heading_carries_its_shape() {
        // A mixture of experts names its split, a dense model just its size,
        // and a row with no parameter count gets no shape at all.
        let dense = BENCH
            .replace(r#""model": "ornith-1.5-35b""#, r#""model": "dense-0.5b""#)
            .replace(r#""params_b": 35.51"#, r#""params_b": 0.49"#)
            .replace(r#""params_active_b": 3.52"#, r#""params_active_b": 0.49"#)
            .replace(r#", "experts": "256x8""#, "");
        let unknown =
            r#"{"ts": "2026-10-19T18:02:22Z", "kind": "bench", "host": "test-box", "model": "mystery", "engine": "e", "engine_build": "b", "decode_median": 5.0}"#;
        let md = markdown(&rows(&[BENCH, &dense, unknown]));
        assert!(
            md.contains("### ornith-1.5-35b \u{2014} 35.51B total, 3.52B active, 256x8 experts\n"),
            "{md}"
        );
        assert!(md.contains("### dense-0.5b \u{2014} 0.49B\n"), "{md}");
        assert!(md.contains("### mystery\n"), "{md}");
    }

    #[test]
    fn the_setup_column_names_threads_first_then_the_tuning() {
        let md = markdown(&rows(&[BENCH]));
        assert!(
            md.contains("| threads=16 n-cpu-moe=36 |"),
            "threads leads, then the split, and neither ctx-size nor jinja appears:\n{md}"
        );
        assert!(!md.contains("jinja="), "{md}");
        assert!(!md.contains("ctx-size="), "{md}");
    }

    #[test]
    fn the_best_setup_wins_at_a_context_and_the_others_keep_their_own_lines() {
        // Same host, model, engine, build and ctx: a different offload split
        // is the same configuration, so only the faster is printed. A second
        // context and a second engine are configurations of their own.
        let slower_split = BENCH
            .replace(r#""n-cpu-moe": 36"#, r#""n-cpu-moe": 32"#)
            .replace(r#""decode_median": 59.3"#, r#""decode_median": 51.0"#);
        let other_ctx = BENCH
            .replace(r#""ctx": 236544"#, r#""ctx": 32768"#)
            .replace(r#""decode_median": 59.3"#, r#""decode_median": 58.5"#);
        let other_engine = BENCH
            .replace(r#""engine": "llama.cpp""#, r#""engine": "ik_llama.cpp""#)
            .replace(r#""decode_median": 59.3"#, r#""decode_median": 16.7"#);
        let md = markdown(&rows(&[BENCH, &slower_split, &other_ctx, &other_engine]));

        assert!(md.contains("**59.3**"), "the winning split stays:\n{md}");
        assert!(!md.contains("**51.0**"), "the losing split is dropped:\n{md}");
        assert!(!md.contains("n-cpu-moe=32"), "{md}");
        assert!(md.contains("| 32K | llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=36 | **58.5** |"), "the second context keeps its line:\n{md}");
        assert!(md.contains("| 231K | ik_llama.cpp `b11379-1537a0a8b` | threads=16 n-cpu-moe=36 | **16.7** |"), "the second engine keeps its line:\n{md}");
    }

    #[test]
    fn two_machines_never_share_a_section() {
        // The same model, engine, build, context and flags on another box is
        // that box's number, not a slower repeat of this one.
        let elsewhere = on_second_box(BENCH).replace(r#""decode_median": 59.3"#, r#""decode_median": 61.0"#);
        let md = markdown(&rows(&[BENCH, &elsewhere]));

        // Hosts are printed ascending, so the other machine opens the report.
        let second = md.find("## rtx-4090_24g_64g_i9-13900k").expect("the second machine");
        let first = md.find("## test-box").expect("test-box");
        assert!(second < first, "the sections are separate:\n{md}");
        let second_section = &md[second..first];
        let first_section = &md[first..];
        assert!(first_section.contains("**59.3**"), "test-box keeps its number:\n{md}");
        assert!(!first_section.contains("**61.0**"), "and not the other box's:\n{md}");
        assert!(second_section.contains("**61.0**"), "the other machine keeps its own:\n{md}");
        assert!(!second_section.contains("**59.3**"), "{md}");
    }

    #[test]
    fn rejected_rows_sit_under_their_own_host() {
        let second_reject = on_second_box(REJECT)
            .replace(r#""note": "rejected: 3.4x slower""#, r#""note": "rejected: no router""#);
        let elsewhere = on_second_box(BENCH);
        let md = markdown(&rows(&[BENCH, REJECT, &elsewhere, &second_reject]));

        // The other machine sorts first, so its section ends where test-box begins.
        let second = md.find("## rtx-4090_24g_64g_i9-13900k").expect("the second machine");
        let first = md.find("## test-box").expect("test-box");
        assert!(second < first, "the sections are separate:\n{md}");
        let second_section = &md[second..first];
        let first_section = &md[first..];

        assert!(second_section.contains("rejected: no router"), "{md}");
        assert!(!second_section.contains("rejected: 3.4x slower"), "{md}");
        assert!(first_section.contains("rejected: 3.4x slower"), "{md}");
        assert!(!first_section.contains("rejected: no router"), "{md}");
        assert!(first_section.contains("### Rejected, with the measured reason\n\n| model | engine | why |\n|---|---|---|\n| ornith-1.5-35b | ik_llama.cpp `main-b5483` | rejected: 3.4x slower |\n"), "{md}");
    }

    #[test]
    fn a_host_with_no_rejects_gets_no_rejected_block() {
        let md = markdown(&rows(&[BENCH]));
        assert!(!md.contains("### Rejected"), "{md}");
    }

    #[test]
    fn unknown_host_comes_last() {
        let unnamed = BENCH.replace(
            r#""host": "test-box", "gpu": "Test GPU", "os": "linux", "mode": "native", "#,
            "",
        );
        let md = markdown(&rows(&[&unnamed, BENCH]));

        let named = md.find("## test-box").expect("named host");
        let unknown = md.find("## unknown host").expect("unknown host");
        assert!(named < unknown, "an unnamed machine is last:\n{md}");
        assert!(md.contains("## unknown host\n\n### ornith-1.5-35b"), "{md}");
    }

    /// The cells of a table line, outer pipes dropped.
    fn cells(line: &str) -> Vec<&str> {
        let line = line.trim();
        let line = line.strip_prefix('|').unwrap_or(line);
        let line = line.strip_suffix('|').unwrap_or(line);
        line.split('|').collect()
    }

    /// A delimiter row: pipes, dashes, colons and spaces, and at least one
    /// dash. No table's header or data row looks like this.
    fn is_delimiter(line: &str) -> bool {
        let line = line.trim();
        line.starts_with('|')
            && line.contains('-')
            && line.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
    }

    #[test]
    fn every_table_delimiter_matches_its_header() {
        // A delimiter row with the wrong cell count is not an error a
        // renderer reports: it silently stops treating the block as a table
        // and prints the pipes as text, so the model tables shipped broken
        // once while the rejected table (whose counts happened to agree)
        // rendered. Every table in a full report is checked here.
        let md = markdown(&rows(&[BENCH, EVAL, REJECT, &on_second_box(BENCH)]));
        let lines: Vec<&str> = md.lines().collect();
        let mut tables = 0;
        for (i, line) in lines.iter().enumerate() {
            if i == 0 || !is_delimiter(line) {
                continue;
            }
            let header = cells(lines[i - 1]);
            assert_eq!(
                cells(line).len(),
                header.len(),
                "the delimiter on line {} has a different cell count than its header {header:?}",
                i + 1
            );
            tables += 1;
        }
        assert!(tables >= 2, "a model table and a rejected table should both appear:\n{md}");
    }

    #[test]
    fn a_knob_every_row_shares_is_left_out_of_the_setup_column() {
        // The batch is 2048 everywhere here, so it tells no row from another
        // and only widens the table; the offload split does differ and stays.
        let with_batch = BENCH.replace(
            r#""n-cpu-moe": 36"#,
            r#""n-cpu-moe": 36, "batch-size": 2048, "ubatch-size": 2048"#,
        );
        let other_split = with_batch
            .replace(r#""engine": "llama.cpp""#, r#""engine": "ik_llama.cpp""#)
            .replace(r#""n-cpu-moe": 36"#, r#""n-cpu-moe": 32"#);
        let md = markdown(&rows(&[&with_batch, &other_split]));
        assert!(md.contains("n-cpu-moe=36") && md.contains("n-cpu-moe=32"), "{md}");
        assert!(!md.contains("batch-size="), "a shared knob is noise:\n{md}");
        assert!(!md.contains("ubatch-size="), "{md}");
    }

    #[test]
    fn the_placement_and_the_speculation_are_named_on_every_row() {
        // The split and the speculation are what a reader compares rows for,
        // so they are shown even when every row agrees on them - unlike the
        // preset's other flags, which earn their place by differing.
        let other_ctx = BENCH.replace(r#""ctx": 236544"#, r#""ctx": 32768"#);
        let md = markdown(&rows(&[BENCH, &other_ctx]));
        assert_eq!(
            md.matches("n-cpu-moe=36").count(),
            2,
            "both rows name the split:\n{md}"
        );
        assert!(!md.contains("jinja="), "a flag nothing differs on stays out:\n{md}");
    }

    #[test]
    fn a_windows_caption_does_not_say_windows_twice() {
        // On Windows the deploy mode and the OS are the same word.
        let row = BENCH.replace(
            r#""os": "linux", "mode": "native""#,
            r#""os": "windows", "mode": "windows""#,
        );
        let md = markdown(&rows(&[&row]));
        assert!(md.contains("Test GPU \u{b7} windows\n"), "{md}");
        assert!(!md.contains("windows/windows"), "{md}");
    }

    #[test]
    fn missing_values_become_dashes() {
        let sparse = r#"{"ts": "2026-10-19T18:02:22Z", "kind": "bench", "host": "test-box", "model": "m", "engine": "llama.cpp", "engine_build": "b1", "decode_median": 10.5}"#;
        let md = markdown(&rows(&[sparse]));
        // ctx, setup, prefill, VRAM, RAM, MTP and needle are absent.
        assert!(
            md.contains("| - | llama.cpp `b1` | - | **10.5** |  | - | - | - | - | - | 2026-10-19 |"),
            "{md}"
        );
    }
}
