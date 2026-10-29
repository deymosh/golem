//! The settings the console edits: which `golem.toml` keys, what each one
//! takes, and when a change reaches what it moves.
//!
//! `golem.toml.example` documents every key; this is the part of it a page
//! can offer as a form. A value is checked here before it is written, so a
//! request built by hand cannot leave the file holding a port that is not a
//! port or a floor the tuner cannot read. The model, the engine and the
//! preset file are not here: the Models tab and `tune --adopt` own them.

use crate::json;

/// What a value has to look like.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Shape {
    /// A whole number in `min..=max`.
    Whole { min: u64, max: u64 },
    /// A decimal number in `min..=max`.
    Decimal { min: f64, max: f64 },
    /// One of the words.
    Choice(&'static [&'static str]),
    /// A TCP port.
    Port,
    /// An address to listen on: an IP literal or a host name.
    Host,
    /// Free text, kept on one line.
    Text,
    /// Free text the page never reads back: a token.
    Secret,
}

/// When a changed value takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applies {
    /// On the next request or check that reads it.
    Now,
    /// On the next bench, eval or tune.
    NextRun,
    /// When the engine starts again.
    Engine,
    /// When `golem service` starts again.
    Service,
}

impl Applies {
    pub fn word(self) -> &'static str {
        match self {
            Applies::Now => "now",
            Applies::NextRun => "next-run",
            Applies::Engine => "engine-restart",
            Applies::Service => "service-restart",
        }
    }
}

/// One key the console offers.
#[derive(Debug, Clone, Copy)]
pub struct Setting {
    pub key: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub shape: Shape,
    /// What an unset key means, as the template writes it; empty when unset
    /// has a meaning of its own (the help says which).
    pub default: &'static str,
    pub applies: Applies,
}

const fn s(
    key: &'static str,
    group: &'static str,
    label: &'static str,
    help: &'static str,
    shape: Shape,
    default: &'static str,
    applies: Applies,
) -> Setting {
    Setting { key, group, label, help, shape, default, applies }
}

/// Every key the console offers, in the order the page shows them.
pub const SETTINGS: &[Setting] = &[
    s("WEB_SEARCH", "Web tools", "Golem web tools",
      "Add golem_web_search and golem_web_fetch to every chat request. Searches go to DuckDuckGo from this machine.",
      Shape::Choice(&["on", "off"]), "on", Applies::Now),
    s("CPU_THREADS", "Engine", "CPU threads",
      "Threads for the experts that run from RAM. Empty lets the engine choose.",
      Shape::Whole { min: 1, max: 1024 }, "", Applies::Engine),
    s("PARALLEL", "Engine", "Parallel slots",
      "Keep 1 for one agent: the engine splits the context across slots.",
      Shape::Whole { min: 1, max: 64 }, "1", Applies::Engine),
    s("SLEEP_IDLE_SECONDS", "Engine", "Unload after idle (s)",
      "Free the VRAM and RAM after this many seconds without a request. Empty keeps the model loaded.",
      Shape::Whole { min: 1, max: 604_800 }, "", Applies::Engine),
    s("GPU_BACKEND", "Engine", "GPU backend",
      "auto reads the card's vendor.",
      Shape::Choice(&["auto", "cuda", "rocm", "vulkan", "metal"]), "auto", Applies::Engine),
    s("ENGINE_BUILD", "Engine", "Engine build",
      "Pin another llama.cpp build than the adapter's gated one (b1234, or latest). Empty uses the adapter's. Install it with golem install.",
      Shape::Text, "", Applies::Engine),
    s("LOCAL_AI_HOST", "Network", "API address",
      "127.0.0.1 keeps the API on this machine; 0.0.0.0 opens it to the network.",
      Shape::Host, "127.0.0.1", Applies::Service),
    s("LOCAL_AI_PORT", "Network", "API port",
      "Where agents reach the model.",
      Shape::Port, "8000", Applies::Service),
    s("ENGINE_PORT", "Network", "Engine port",
      "The engine's own port on 127.0.0.1. Empty is the one after the API port.",
      Shape::Port, "", Applies::Service),
    s("LOCAL_AI_API_KEY", "Network", "API key",
      "The key clients send. Change it before opening the API to the network.",
      Shape::Text, "local-ai-key", Applies::Engine),
    s("CONSOLE_HOST", "Network", "Console address",
      "The console has no login: keep it on 127.0.0.1 unless the network is trusted.",
      Shape::Host, "127.0.0.1", Applies::Service),
    s("CONSOLE_PORT", "Network", "Console port",
      "This page's port.",
      Shape::Port, "8787", Applies::Service),
    s("TUNE_MIN_DECODE", "Tuning", "Floor: decode (t/s)",
      "A tune drops a setup that decodes slower.",
      Shape::Decimal { min: 0.0, max: 10_000.0 }, "25", Applies::NextRun),
    s("TUNE_MIN_PREFILL", "Tuning", "Floor: prefill (t/s)",
      "A tune drops a setup that reads a prompt slower.",
      Shape::Decimal { min: 0.0, max: 100_000.0 }, "350", Applies::NextRun),
    s("TUNE_MIN_DEPTH_DECODE", "Tuning", "Floor: decode at depth (t/s)",
      "The gate fails a setup that decodes slower at full context.",
      Shape::Decimal { min: 0.0, max: 10_000.0 }, "15", Applies::NextRun),
    s("TUNE_MIN_AGENT", "Tuning", "Floor: agent suite",
      "The share of the gate's agent tasks a setup must solve on each API, 0 to 1.",
      Shape::Decimal { min: 0.0, max: 1.0 }, "0.75", Applies::NextRun),
    s("TUNE_VRAM_HEADROOM_MIB", "Tuning", "VRAM left for the desktop (MiB)",
      "A setup must fit in the card less this.",
      Shape::Whole { min: 0, max: 65_536 }, "1024", Applies::NextRun),
    s("TUNE_CTX_POLICY", "Tuning", "Context",
      "max grows the context to the model's trained size; ladder is the older search; a number pins it.",
      Shape::Text, "max", Applies::NextRun),
    s("GATE_TOKENS", "Tuning", "Gate needle (tokens)",
      "The longest needle the gate asks for.",
      Shape::Whole { min: 1024, max: 10_000_000 }, "200000", Applies::NextRun),
    s("TUNE_RUNS", "Tuning", "Bench runs per point",
      "The bench's best-of-N.",
      Shape::Whole { min: 1, max: 20 }, "3", Applies::NextRun),
    s("TUNE_PREFILL", "Tuning", "Bench prompt (tokens)",
      "The prompt the bench times prefill on.",
      Shape::Whole { min: 256, max: 1_000_000 }, "8000", Applies::NextRun),
    s("TEMPERATURE", "Measuring", "Eval temperature",
      "The sampling the eval's throughput runs use.",
      Shape::Decimal { min: 0.0, max: 2.0 }, "0.15", Applies::NextRun),
    s("TOP_P", "Measuring", "Eval top-p",
      "The sampling the eval's throughput runs use.",
      Shape::Decimal { min: 0.0, max: 1.0 }, "0.95", Applies::NextRun),
    s("UPDATE_CHECK", "Updates", "Check for updates",
      "ask checks GitHub once a day and offers a new release; off stops the checks.",
      Shape::Choice(&["ask", "off"]), "ask", Applies::Now),
    s("HF_TOKEN", "Updates", "Hugging Face token",
      "For a gated or private repo. Never shown again once saved.",
      Shape::Secret, "", Applies::NextRun),
];

/// The setting for `key`, if the console offers it.
pub fn find(key: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.key == key)
}

/// The value to write for `key`, or why it cannot be. Empty is always
/// allowed and puts the default back - or leaves the key empty where unset
/// has a meaning of its own (the help says which).
pub fn check(key: &str, value: &str) -> Result<String, String> {
    let setting = find(key).ok_or_else(|| format!("{key} is not a setting the console changes"))?;
    let value = value.trim();
    if value.is_empty() {
        return Ok(setting.default.to_string());
    }
    if value.contains(['\n', '\r', '"']) {
        return Err("one line, without quotes".into());
    }
    match setting.shape {
        Shape::Whole { min, max } => match value.parse::<u64>() {
            Ok(n) if (min..=max).contains(&n) => Ok(n.to_string()),
            _ => Err(format!("a whole number from {min} to {max}")),
        },
        Shape::Decimal { min, max } => match value.parse::<f64>() {
            Ok(n) if n.is_finite() && n >= min && n <= max => Ok(value.to_string()),
            _ => Err(format!("a number from {min} to {max}")),
        },
        Shape::Choice(words) => {
            let lower = value.to_ascii_lowercase();
            if words.contains(&lower.as_str()) {
                Ok(lower)
            } else {
                Err(format!("one of {}", words.join(", ")))
            }
        }
        Shape::Port => match value.parse::<u16>() {
            Ok(n) if n > 0 => Ok(n.to_string()),
            _ => Err("a port from 1 to 65535".into()),
        },
        Shape::Host => {
            let ok = value.parse::<std::net::IpAddr>().is_ok()
                || (value.len() <= 253
                    && value.split('.').all(|label| {
                        !label.is_empty()
                            && label.len() <= 63
                            && label.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    }));
            if ok {
                Ok(value.to_string())
            } else {
                Err("an IP address or a host name".into())
            }
        }
        Shape::Text if key == "TUNE_CTX_POLICY" => {
            if value == "max" || value == "ladder" || value.parse::<u64>().is_ok_and(|n| n >= 1024) {
                Ok(value.to_string())
            } else {
                Err("max, ladder, or a context of at least 1024 tokens".into())
            }
        }
        Shape::Text | Shape::Secret => Ok(value.to_string()),
    }
}

/// The settings as the page reads them: each key's description and its
/// current value, `value_of` answering for the file. A secret's value is
/// never sent; the page learns only whether one is set.
pub fn to_json(value_of: impl Fn(&str) -> Option<String>) -> String {
    let items: Vec<String> = SETTINGS
        .iter()
        .map(|s| {
            let current = value_of(s.key).unwrap_or_default();
            let (shape, extra) = match s.shape {
                Shape::Whole { min, max } => ("whole", format!(", \"min\": {min}, \"max\": {max}")),
                Shape::Decimal { min, max } => ("decimal", format!(", \"min\": {min}, \"max\": {max}")),
                Shape::Choice(words) => (
                    "choice",
                    format!(", \"choices\": [{}]", words.iter().map(|w| json::quote(w)).collect::<Vec<_>>().join(", ")),
                ),
                Shape::Port => ("port", String::new()),
                Shape::Host => ("host", String::new()),
                Shape::Text => ("text", String::new()),
                Shape::Secret => ("secret", String::new()),
            };
            let value = if s.shape == Shape::Secret {
                format!("\"set\": {}", !current.is_empty())
            } else {
                format!("\"value\": {}", json::quote(&current))
            };
            format!(
                "{{\"key\": {}, \"group\": {}, \"label\": {}, \"help\": {}, \"shape\": \"{shape}\"{extra}, \
                 \"default\": {}, \"applies\": \"{}\", {value}}}",
                json::quote(s.key),
                json::quote(s.group),
                json::quote(s.label),
                json::quote(s.help),
                json::quote(s.default),
                s.applies.word()
            )
        })
        .collect();
    format!("[{}]", items.join(",\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_is_one_the_template_documents() {
        let template = include_str!("../../../../golem.toml.example");
        for s in SETTINGS {
            assert!(template.contains(&format!("{} =", s.key)), "{} is not in golem.toml.example", s.key);
        }
    }

    #[test]
    fn values_are_checked_against_their_shape() {
        assert_eq!(check("LOCAL_AI_PORT", " 8000 "), Ok("8000".into()));
        assert!(check("LOCAL_AI_PORT", "0").is_err());
        assert!(check("LOCAL_AI_PORT", "70000").is_err());
        assert_eq!(check("WEB_SEARCH", "OFF"), Ok("off".into()));
        assert!(check("WEB_SEARCH", "maybe").is_err());
        assert_eq!(check("TUNE_MIN_AGENT", "0.5"), Ok("0.5".into()));
        assert!(check("TUNE_MIN_AGENT", "1.5").is_err());
        assert!(check("PARALLEL", "0").is_err());
        assert_eq!(check("CONSOLE_HOST", "0.0.0.0"), Ok("0.0.0.0".into()));
        assert_eq!(check("CONSOLE_HOST", "box-1.lan"), Ok("box-1.lan".into()));
        assert!(check("CONSOLE_HOST", "a b").is_err());
        assert_eq!(check("TUNE_CTX_POLICY", "131072"), Ok("131072".into()));
        assert!(check("TUNE_CTX_POLICY", "huge").is_err());
        // Empty puts the default back, or unsets a key with none.
        assert_eq!(check("LOCAL_AI_PORT", " "), Ok("8000".into()));
        assert_eq!(check("CPU_THREADS", ""), Ok(String::new()));
        // A quote or a newline would break the file's one-line values.
        assert!(check("LOCAL_AI_API_KEY", "a\"b").is_err());
        assert!(check("ENGINE_BUILD", "b1\nX = 1").is_err());
        // The keys the Models tab owns are not settings here.
        assert!(check("MODEL_ALIAS", "x").is_err());
        assert!(check("MODELS_PRESET", "x").is_err());
    }

    #[test]
    fn a_secret_is_never_sent_back() {
        let page = to_json(|key| match key {
            "HF_TOKEN" => Some("hf_secret".into()),
            "LOCAL_AI_PORT" => Some("8000".into()),
            _ => None,
        });
        assert!(!page.contains("hf_secret"), "{page}");
        assert!(page.contains("\"key\": \"HF_TOKEN\""));
        assert!(page.contains("\"set\": true"));
        assert!(page.contains("\"value\": \"8000\""));
        assert!(json::parse(&page).is_ok(), "{page}");
    }
}
