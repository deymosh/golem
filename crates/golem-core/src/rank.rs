//! `tune --auto`'s ranking: which installed model is the smartest this
//! machine can run. The agent suite where the machine has run it on the
//! model's file, a vendor's score where it has not - the library's, else a
//! `; Quality: <score>` comment in the preset; a number that is only ever a
//! ranking input, never an adoption reason - then the parameter count, the
//! context and the decode.

use std::cmp::Ordering;

/// One installed preset, as the ranking reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub preset: String,
    pub model: String,
    /// The vendor score: the library's, else a `; Quality:` comment in the
    /// preset.
    pub quality: Option<String>,
    /// The share of the agent suite this machine measured on the model's
    /// file, on the worse API, when it ran.
    pub agent: Option<f64>,
    /// Total parameters, in billions (`params_total / 1e9`).
    pub params_b: f64,
    /// Filled in by the cheap sweep.
    pub decode: f64,
    pub ctx: u64,
}

impl Candidate {
    /// What the ranking reads for it, as the run prints it.
    pub fn ranked_by(&self) -> String {
        match (self.agent, &self.quality) {
            (Some(share), _) => format!("agent {:.0}%", share * 100.0),
            (None, Some(q)) => format!("vendor {q}"),
            (None, None) => "unscored".to_string(),
        }
    }
}

/// Order the candidates best-first.
///
/// A model this machine ran the agent suite on is ranked by it: one that
/// solved at least `min_agent` of it on both APIs comes first, by its share;
/// one that solved less comes last, whatever a vendor says of it. Between
/// them, the models never measured, by the vendor score when the preset has
/// one. Ties go to the vendor score, then the parameter count, the context
/// and the decode speed. A preset with no score sorts below every scored
/// one: it has no claim to "smartest", however big it is.
pub fn rank(candidates: &mut [Candidate], min_agent: f64) {
    candidates.sort_by(|a, b| key(b, min_agent).partial_cmp(&key(a, min_agent)).unwrap_or(Ordering::Equal));
}

fn key(c: &Candidate, min_agent: f64) -> (u8, f64, u8, f64, f64, u64, f64) {
    let tier = match c.agent {
        Some(share) if share >= min_agent - 1e-9 => 2,
        None => 1,
        Some(_) => 0,
    };
    let score = c.quality.as_deref().and_then(|q| q.parse::<f64>().ok());
    (tier, c.agent.unwrap_or(0.0), u8::from(score.is_some()), score.unwrap_or(0.0), c.params_b, c.ctx, c.decode)
}

/// The `; Quality: <score>` comment of `section`, when the file has one.
///
/// The comment may sit immediately above the section header (where this repo
/// puts `; Download:`) or inside the section; both are read, because reading
/// only one attributes a score to the wrong section. A quality line inside
/// the wanted section answers at once; one in the section above is held as
/// `pending` until the next header claims it.
pub fn quality_of(text: &str, want: &str) -> Option<String> {
    let mut pending: Option<String> = None;
    let mut in_want = false;
    for line in text.lines() {
        if let Some(name) = header_name(line) {
            if name == want {
                if let Some(score) = pending.take() {
                    return Some(score);
                }
                in_want = true;
            } else {
                in_want = false;
            }
            pending = None;
            continue;
        }
        if let Some(score) = quality_line(line) {
            if in_want {
                return Some(score);
            }
            pending = Some(score);
        }
    }
    None
}

/// The `[section]` name of a header line, when it is one (column 0 only,
/// exactly as `ini::parse` reads them).
fn header_name(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('[')?;
    let (name, _) = rest.split_once(']')?;
    (!name.is_empty()).then_some(name)
}

/// The numeric part of a `; Quality: <score>` comment, when the line is one.
///
/// `;` then optional space then `Quality:`/`quality:`, then the leading run of
/// digits and dots - `; Quality: 79 (SWE-V)` reads as `79`, and a comment with
/// no number at all reads as nothing.
fn quality_line(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix(';')?.trim_start();
    let rest = rest.strip_prefix("Quality:").or_else(|| rest.strip_prefix("quality:"))?;
    let digits: String =
        rest.trim_start().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    (!digits.is_empty()).then_some(digits)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(preset: &str, quality: Option<&str>, params_b: f64, ctx: u64, decode: f64) -> Candidate {
        Candidate {
            preset: preset.to_string(),
            model: format!("{preset}.gguf"),
            quality: quality.map(str::to_string),
            agent: None,
            params_b,
            decode,
            ctx,
        }
    }

    fn names(candidates: &[Candidate]) -> Vec<String> {
        candidates.iter().map(|c| c.preset.clone()).collect()
    }

    #[test]
    fn a_scored_preset_outranks_a_bigger_unscored_one() {
        let mut candidates = vec![
            cand("big-unscored", None, 70.0, 262_144, 60.0),
            cand("small-scored", Some("52"), 30.0, 65_536, 40.0),
        ];
        rank(&mut candidates, 0.75);
        assert_eq!(names(&candidates), vec!["small-scored", "big-unscored"]);
        assert_eq!(candidates[1].ranked_by(), "unscored");
    }

    #[test]
    fn ties_break_on_parameters_then_context_then_decode() {
        let mut candidates = vec![
            cand("slow", Some("79"), 35.0, 200_000, 50.0),
            cand("fast", Some("79"), 35.0, 200_000, 60.0),
            cand("shorter", Some("79"), 35.0, 100_000, 99.0),
            cand("bigger", Some("79"), 40.0, 10_000, 10.0),
            cand("smarter", Some("80"), 1.0, 1_000, 1.0),
        ];
        rank(&mut candidates, 0.75);
        // A better score first; then the bigger model; then the longer
        // context; then the faster decode.
        assert_eq!(names(&candidates), vec!["smarter", "bigger", "fast", "slow", "shorter"]);
    }

    #[test]
    fn a_measured_agent_suite_ranks_before_a_vendor_score() {
        let measured = |preset: &str, quality: Option<&str>, params_b: f64, agent: f64| Candidate {
            agent: Some(agent),
            ..cand(preset, quality, params_b, 262_144, 50.0)
        };
        let mut candidates = vec![
            // The 0.5B solved one of the first four tasks on the reference box.
            measured("smoke", None, 0.5, 0.25),
            cand("new-unmeasured", Some("85"), 30.0, 262_144, 70.0),
            measured("everyday", None, 9.0, 10.0 / 11.0),
            measured("coder", Some("79"), 35.0, 1.0),
        ];
        rank(&mut candidates, 0.75);
        // Proven first, by share; then what a vendor claims; a model under
        // the floor last.
        assert_eq!(names(&candidates), vec!["coder", "everyday", "new-unmeasured", "smoke"]);
        assert_eq!(candidates[0].ranked_by(), "agent 100%");
        assert_eq!(candidates[2].ranked_by(), "vendor 85");
        assert_eq!(candidates[3].ranked_by(), "agent 25%");
        // Equal shares fall back to the vendor score.
        let mut tied = vec![measured("small", None, 9.0, 1.0), measured("scored", Some("79"), 35.0, 1.0)];
        rank(&mut tied, 0.75);
        assert_eq!(names(&tied), vec!["scored", "small"]);
    }

    #[test]
    fn a_share_exactly_at_the_floor_is_over_it() {
        let at = Candidate { agent: Some(9.0 / 12.0), ..cand("at", None, 1.0, 0, 0.0) };
        let mut candidates = vec![cand("unmeasured", Some("90"), 70.0, 0, 0.0), at];
        rank(&mut candidates, 0.75);
        assert_eq!(names(&candidates), vec!["at", "unmeasured"]);
    }

    #[test]
    fn a_quality_comment_is_read_above_a_header_and_inside_the_section() {
        let text = "\
; Quality: 42
[above]
model = a.gguf

[inside]
; Quality: 7
model = i.gguf

[x]
model = x.gguf
; Quality: 5
[y]
model = y.gguf

[plain]
model = p.gguf
";
        // Above the header (here: in the preamble).
        assert_eq!(quality_of(text, "above").as_deref(), Some("42"));
        // Inside the section.
        assert_eq!(quality_of(text, "inside").as_deref(), Some("7"));
        // At the tail of the *previous* section, directly above [y]: the
        // section below it is the one it belongs to.
        assert_eq!(quality_of(text, "y").as_deref(), Some("5"));
        // A section with no comment anywhere above it has none.
        assert_eq!(quality_of(text, "plain"), None);
        assert_eq!(quality_of(text, "nope"), None);
    }

    #[test]
    fn a_quality_line_needs_its_semicolon_and_a_number() {
        assert_eq!(quality_line("; Quality: 79"), Some("79".to_string()));
        assert_eq!(quality_line("  ; quality: 73.4 (SWE-V)"), Some("73.4".to_string()));
        assert_eq!(quality_line("; Quality: none"), None);
        assert_eq!(quality_line("Quality: 79"), None, "a bare key is not a comment");
        assert_eq!(quality_line("; Optional: \"Quality: <score>\""), None);
    }
}
