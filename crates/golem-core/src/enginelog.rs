//! The engine's log as the console shows it: llama.cpp's own lines, read and
//! put into the few that say something.
//!
//! The router writes one line per argument of every model it starts, slot
//! bookkeeping for every request, blank lines, and warnings that are expected
//! under golem; what a reader wants is when a model loaded, how fast each
//! request went, how far a long prefill has got, and anything that went
//! wrong. Each child process's lines carry its port (`[52485] ...`), which
//! is turned back into the model it serves. Lines this does not know are
//! kept as they are, so nothing the engine says is lost.

/// How a line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An ordinary line.
    Plain,
    /// Something that went right: a model loaded, a request answered.
    Ok,
    /// Something worth a look.
    Warn,
    /// Something that failed.
    Bad,
    /// A model starting: the line a section begins with.
    Head,
    /// Background detail.
    Dim,
}

impl Kind {
    /// The class the console colours the line with.
    pub fn class(self) -> &'static str {
        match self {
            Kind::Plain => "",
            Kind::Ok => "ok",
            Kind::Warn => "warn",
            Kind::Bad => "bad",
            Kind::Head => "jf",
            Kind::Dim => "jt",
        }
    }
}

/// One line as shown: who said it (the model, or empty for the router) and
/// what.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub kind: Kind,
    pub model: String,
    pub text: String,
}

/// One raw line taken apart: the child's port, the level letter, the
/// subsystem's function, and the message.
struct Raw<'a> {
    port: Option<&'a str>,
    level: char,
    func: &'a str,
    message: &'a str,
}

fn split(line: &str) -> Option<Raw<'_>> {
    let mut rest = line.trim_end();
    let mut port = None;
    if let Some(after) = rest.strip_prefix('[') {
        let (p, tail) = after.split_once(']')?;
        port = Some(p);
        rest = tail.trim_start();
    }
    // `9.57.893.311 I srv  load: message`
    let (stamp, tail) = rest.split_once(' ')?;
    if !stamp.chars().all(|c| c.is_ascii_digit() || c == '.') || stamp.is_empty() {
        return None;
    }
    let mut chars = tail.chars();
    let level = chars.next()?;
    let tail = chars.as_str().trim_start();
    // The subsystem (`srv`, `slot`, `cmn`), then `function: message` - or
    // a subsystem with no function (`load: message`).
    let (subsystem, tail) = tail.split_once(' ').unwrap_or(("", tail));
    let (func, message) = match tail.split_once(": ") {
        _ if subsystem.ends_with(':') => ("", tail.trim()),
        Some((func, message)) => (func.trim(), message.trim()),
        None => ("", tail.trim()),
    };
    Some(Raw { port, level, func, message })
}

/// The value after `key = ` in a llama.cpp message, up to the next comma or
/// bar.
fn field<'a>(message: &'a str, key: &str) -> Option<&'a str> {
    let at = message.find(&format!("{key} = "))? + key.len() + 3;
    let value = &message[at..];
    Some(value.split([',', '|', ')']).next().unwrap_or("").trim())
}

/// A rate as golem prints them: whole above 100, one decimal below.
fn rate(text: &str) -> String {
    match text.trim().parse::<f64>() {
        Ok(v) if v >= 100.0 => format!("{v:.0}"),
        Ok(v) => format!("{v:.1}"),
        Err(_) => text.trim().to_string(),
    }
}

/// Warnings that are expected under golem and say nothing to act on, shown
/// dimmed.
fn benign(message: &str) -> bool {
    ["router mode enabled - do not expose", "no API key is set and CORS allows all origins", "control-looking token"]
        .iter()
        .any(|known| message.contains(known))
}

/// Warnings that are the prompt cache's own bookkeeping, not shown at all.
fn bookkeeping(message: &str) -> bool {
    ["need to evaluate at least 1 token", "n_past was set to"].iter().any(|known| message.contains(known))
}

/// The log's lines, read.
pub fn read(text: &str) -> Vec<Line> {
    let mut out: Vec<Line> = Vec::new();
    let mut ports: Vec<(String, String)> = Vec::new();
    // A spawn's argument lines, collected into one.
    let mut args: Option<(String, Vec<String>)> = None;
    // A request's prompt timing, waiting for its decode timing.
    let mut prompt: Option<(String, String, String)> = None;
    let mut presets: Vec<String> = Vec::new();

    let flush_args = |args: &mut Option<(String, Vec<String>)>, out: &mut Vec<Line>| {
        if let Some((model, list)) = args.take() {
            if !list.is_empty() {
                out.push(Line { kind: Kind::Dim, model, text: list.join(" ") });
            }
        }
    };

    for raw_line in text.lines() {
        let Some(raw) = split(raw_line) else {
            // A child's blank line, or something that is not a log line at
            // all: kept when it has words in it.
            let bare = raw_line.trim_start_matches(|c: char| c == '[' || c.is_ascii_digit() || c == ']').trim();
            if !bare.is_empty() {
                flush_args(&mut args, &mut out);
                out.push(Line { kind: Kind::Plain, model: String::new(), text: bare.to_string() });
            }
            continue;
        };
        let model = raw
            .port
            .and_then(|p| ports.iter().find(|(port, _)| port == p).map(|(_, m)| m.clone()))
            .or_else(|| raw.port.map(|p| format!("port {p}")))
            .unwrap_or_default();
        let m = raw.message;

        if raw.func == "load" && args.is_some() && !m.starts_with("spawning") {
            // The first argument is the program itself, which says nothing.
            if let Some((_, list)) = args.as_mut() {
                if !(list.is_empty() && !m.starts_with('-')) {
                    list.push(m.to_string());
                }
            }
            continue;
        }
        flush_args(&mut args, &mut out);
        if !m.starts_with("Available models") && !(raw.func == "operator()" && m.starts_with('[')) && !presets.is_empty() {
            out.push(Line { kind: Kind::Dim, model: String::new(), text: format!("presets: {}", presets.join(", ")) });
            presets.clear();
        }

        let line = |kind: Kind, text: String| Line { kind, model: model.clone(), text };
        let pushed: Option<Line> = if raw.level == 'E' {
            Some(line(Kind::Bad, m.to_string()))
        } else if m.starts_with("spawning server instance with name=") {
            let rest = &m["spawning server instance with name=".len()..];
            let (name, port) = rest.split_once(" on port ").unwrap_or((rest, ""));
            ports.retain(|(p, _)| p != port);
            ports.push((port.trim().to_string(), name.trim().to_string()));
            Some(Line { kind: Kind::Head, model: name.trim().to_string(), text: format!("loading {}", name.trim()) })
        } else if m == "spawning server instance with args:" {
            let name = ports.last().map(|(_, n)| n.clone()).unwrap_or_default();
            args = Some((name, Vec::new()));
            None
        } else if m.starts_with("Available models") {
            presets.clear();
            None
        } else if raw.func == "operator()" && m.starts_with('[') {
            if let Some((_, name)) = m.split_once(']') {
                presets.push(name.trim().to_string());
            }
            None
        } else if m.starts_with("loading model '") {
            Some(line(Kind::Plain, format!("reading {}", m.trim_start_matches("loading model '").trim_end_matches('\''))))
        } else if m == "model loaded" {
            Some(line(Kind::Ok, "model loaded".to_string()))
        } else if let Some(address) = m.strip_prefix("listening on ") {
            Some(line(if model.is_empty() { Kind::Ok } else { Kind::Dim }, format!("listening on {address}")))
        } else if m.starts_with("starting server in router mode") {
            Some(line(Kind::Ok, "router started: models load on their first request".to_string()))
        } else if m.contains("prompt eval time =") {
            let tokens = m.split('/').nth(1).and_then(|s| s.split_whitespace().next()).unwrap_or("?").to_string();
            let tps = m.rsplit(',').next().and_then(|s| s.split_whitespace().next()).unwrap_or("?").to_string();
            prompt = Some((model.clone(), tokens, tps));
            None
        } else if m.contains("eval time =") && !m.contains("prompt") {
            let tokens = m.split('/').nth(1).and_then(|s| s.split_whitespace().next()).unwrap_or("?");
            let tps = m.rsplit(',').next().and_then(|s| s.split_whitespace().next()).unwrap_or("?");
            let text = match prompt.take() {
                Some((_, p_tokens, p_tps)) => format!(
                    "answered: prompt {p_tokens} tok @ {} t/s, decode {tokens} tok @ {} t/s",
                    rate(&p_tps),
                    rate(tps)
                ),
                None => format!("answered: decode {tokens} tok @ {} t/s", rate(tps)),
            };
            Some(line(Kind::Ok, text))
        } else if m.contains("prompt processing, n_tokens =") {
            let tokens = field(m, "n_tokens").unwrap_or("?");
            let progress = field(m, "progress").and_then(|p| p.parse::<f64>().ok()).map_or(String::new(), |p| format!("{:.0}%", p * 100.0));
            let tps = m.rsplit('/').next().and_then(|s| s.split_whitespace().next()).unwrap_or("?");
            Some(line(Kind::Dim, format!("prefill {progress} ({tokens} tok, {} t/s)", rate(tps))))
        } else if m.contains("stop processing:") {
            (field(m, "truncated") == Some("1")).then(|| line(Kind::Warn, "the prompt was truncated to fit the context".to_string()))
        } else if m.contains("entering sleeping state") && raw.func == "handle_sleep" {
            Some(line(Kind::Dim, "idle: model unloaded until the next request".to_string()))
        } else if m.contains("unauthorized") {
            Some(line(Kind::Warn, "a request with a wrong API key was refused".to_string()))
        } else if m.contains("failed to load") || m.contains("out of memory") || m.contains("error") && raw.level == 'W' {
            Some(line(Kind::Bad, m.to_string()))
        } else if raw.level == 'W' && bookkeeping(m) {
            None
        } else if raw.level == 'W' {
            Some(line(if benign(m) { Kind::Dim } else { Kind::Warn }, m.to_string()))
        } else if matches!(
            raw.func,
            "get_availabl" | "launch_slot_" | "release" | "proxy_reques" | "join" | "ensure_model" | "common_param"
                | "init" | "start_loop" | "handle_sleep" | "print_timing"
        ) || m.starts_with("initializing")
            || m.starts_with("child server monitoring")
        {
            None
        } else {
            Some(line(Kind::Plain, m.to_string()))
        };
        if let Some(l) = pushed {
            out.push(l);
        }
    }
    flush_args(&mut args, &mut out);
    if !presets.is_empty() {
        out.push(Line { kind: Kind::Dim, model: String::new(), text: format!("presets: {}", presets.join(", ")) });
    }
    out
}

/// The lines as the console's JSON: `[{"k":class,"m":model,"t":text}]`.
pub fn to_json(lines: &[Line]) -> String {
    let items: Vec<String> = lines
        .iter()
        .map(|l| {
            format!(
                "{{\"k\":{},\"m\":{},\"t\":{}}}",
                crate::json::quote(l.kind.class()),
                crate::json::quote(&l.model),
                crate::json::quote(&l.text)
            )
        })
        .collect();
    format!("[{}]", items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
0.00.003.151 I srv  llama_server: initializing ...
0.00.100.694 I cmn  common_param: common_params_print_info: verbosity = 3 (adjust with the `-lv N` CLI arg)
0.00.104.195 I srv    operator(): Available models (2):
0.00.104.202 I srv    operator():   [    preset] ornith-1.5-35b
0.00.104.203 I srv    operator():   [    preset] qwen2.5-0.5b-instruct
0.00.104.301 W srv  llama_server: security: router mode enabled - do not expose to untrusted environments
0.00.104.308 I srv  llama_server: starting server in router mode. models will be automatically loaded on-demand
0.00.109.180 I srv  llama_server: listening on http://127.0.0.1:8001
9.57.893.311 I srv          join: request for name=qwen2.5-0.5b-instruct queued at position 1
9.57.893.418 I srv          load: spawning server instance with name=qwen2.5-0.5b-instruct on port 52485
9.57.893.452 I srv          load: spawning server instance with args:
9.57.893.455 I srv          load:   C:\\engines\\Golem engine (llama.cpp).exe
9.57.893.456 I srv          load:   --ctx-size
9.57.893.456 I srv          load:   32768
[52485] 0.00.087.749 I srv  llama_server: initializing ...
[52485] 0.00.088.096 W srv  llama_server: security: no API key is set and CORS allows all origins (see https://github.com/ggml-org/llama.cpp/pull/25655)
[52485]
[52485] 0.00.095.104 I srv    load_model: loading model 'Qwen2.5-0.5B-Instruct-Q4_K_M.gguf'
[52485] 0.00.353.316 W load: control-looking token: 128247 '</s>' was not control-type; this is probably a bug in the model. its type will be overridden
[52485] 0.01.030.260 I srv  llama_server: model loaded
[52485] 0.01.030.273 I srv  llama_server: listening on http://127.0.0.1:52485
9.59.024.398 I srv  proxy_reques: proxying request to model qwen2.5-0.5b-instruct on port 52485
[52485] 0.01.098.133 I slot launch_slot_: id  0 | task 0 | processing task, is_child = 0
[52485] 0.01.389.868 W slot   operator(): id  0 | task 31 | need to evaluate at least 1 token for each active slot (n_past = 391, task.n_tokens() = 391)
[52485] 4.59.169.983 I slot print_timing: id  0 | task 136 | prompt processing, n_tokens =   2048, progress = 0.35, t =  31.86 s / 64.29 tokens per second
[52485] 0.01.322.723 I slot print_timing: id  0 | task 0 | prompt eval time =      64.42 ms /   391 tokens (    0.16 ms per token,  6069.54 tokens per second)
[52485] 0.01.322.730 I slot print_timing: id  0 | task 0 |        eval time =     160.14 ms /    30 tokens (    5.52 ms per token,   181.09 tokens per second)
[52485] 0.01.322.731 I slot print_timing: id  0 | task 0 |       total time =     224.56 ms /   421 tokens
[52485] 0.01.322.885 I slot      release: id  0 | task 0 | stop processing: n_tokens = 420, truncated = 1
9.59.822.764 W srv    operator(): unauthorized: Invalid API Key
[52485] 2.00.000.000 E srv  load_model: failed to load model, cudaMalloc failed: out of memory
";

    fn show(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| format!("{}|{}|{}", l.kind.class(), l.model, l.text)).collect()
    }

    #[test]
    fn the_log_reads_as_what_happened() {
        assert_eq!(
            show(&read(SAMPLE)),
            vec![
                "jt||presets: ornith-1.5-35b, qwen2.5-0.5b-instruct",
                "jt||security: router mode enabled - do not expose to untrusted environments",
                "ok||router started: models load on their first request",
                "ok||listening on http://127.0.0.1:8001",
                "jf|qwen2.5-0.5b-instruct|loading qwen2.5-0.5b-instruct",
                "jt|qwen2.5-0.5b-instruct|--ctx-size 32768",
                "jt|qwen2.5-0.5b-instruct|security: no API key is set and CORS allows all origins (see https://github.com/ggml-org/llama.cpp/pull/25655)",
                "|qwen2.5-0.5b-instruct|reading Qwen2.5-0.5B-Instruct-Q4_K_M.gguf",
                "jt|qwen2.5-0.5b-instruct|control-looking token: 128247 '</s>' was not control-type; this is probably a bug in the model. its type will be overridden",
                "ok|qwen2.5-0.5b-instruct|model loaded",
                "jt|qwen2.5-0.5b-instruct|listening on http://127.0.0.1:52485",
                "jt|qwen2.5-0.5b-instruct|prefill 35% (2048 tok, 64.3 t/s)",
                "ok|qwen2.5-0.5b-instruct|answered: prompt 391 tok @ 6070 t/s, decode 30 tok @ 181 t/s",
                "warn|qwen2.5-0.5b-instruct|the prompt was truncated to fit the context",
                "warn||a request with a wrong API key was refused",
                "bad|qwen2.5-0.5b-instruct|failed to load model, cudaMalloc failed: out of memory",
            ]
        );
    }

    #[test]
    fn an_unknown_line_is_kept_as_it_is() {
        let lines = read("something the engine printed without a stamp\n0.00.001.000 I srv  custom: a new message\n");
        assert_eq!(show(&lines), vec!["||something the engine printed without a stamp", "||a new message"]);
        assert_eq!(to_json(&lines[..1]), r#"[{"k":"","m":"","t":"something the engine printed without a stamp"}]"#);
    }
}
