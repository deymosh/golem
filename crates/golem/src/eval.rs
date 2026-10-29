//! `golem eval`: the quality gate - sanity, tool calls, the long-context
//! needle, and with it the agent suite.
//!
//! It is the gate every adoption passes (`./golem eval --long 200000`) and
//! the tuner's gate.
//!
//! The `RESULT` lines it prints are a contract: `golem_core::evalparse` reads
//! them, the recorder turns them into a row, and the tuner decides on them.
//! Two lines that are not `RESULT` lines are part of the same contract, and
//! are easy to break by "tidying" them: the recorder takes
//! `needle : prompt=<N>` for the row's token count, and `[ OK ] tool calling`
//! for its `tools` field. The prompts are fixed for the same reason as
//! `bench`'s: every recorded row was measured with them.
//!
//! The transport is this binary's own HTTP over the socket (see
//! [`crate::engine`]), so a system proxy cannot be picked up by accident,
//! and the long-context bodies are built in memory.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::time::Duration;

use golem_core::agent::Api;
use golem_core::json::{self, Value};
use golem_core::progress::Progress;

use crate::config;
use crate::engine::{self, Base};

// ---------------------------------------------------------------
// The fixed numbers
// ---------------------------------------------------------------

/// The health check: a live engine answers at once.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);
/// `/props`.
const PROPS_TIMEOUT: Duration = Duration::from_secs(10);
/// `/tokenize`, which reads the whole haystack.
const TOKENIZE_TIMEOUT: Duration = Duration::from_secs(120);
/// The sanity and tool requests.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
/// The warmup, and each of the three timed runs.
const RUN_TIMEOUT: Duration = Duration::from_secs(900);
/// The needle and the depth request: a 200K-token prefill is minutes of
/// silence, and the read timeout is the whole budget.
const NEEDLE_TIMEOUT: Duration = Duration::from_secs(3600);

/// What `--long` asks for when the flag is given without a value. No
/// `--long` at all is a different thing: no section 5.
const LONG_DEFAULT: &str = "32768";

/// The sampling when golem.toml sets none (or sets it empty).
const DEFAULT_TEMPERATURE: &str = "0.15";
const DEFAULT_TOP_P: &str = "0.95";
/// The alias when neither `GOLEM_MODEL_ALIAS` nor golem.toml names one.
const DEFAULT_ALIAS: &str = "local-model";

/// The arithmetic question and the number its answer must contain.
const SANITY_QUESTION: &str = "What is 17*23? Reply with the number only.";
const SANITY_ANSWER: &str = "391";

/// The tool request's question, and the name and city section 3 checks the
/// answer against.
const TOOL_QUESTION: &str = "What is the weather in Munich right now? Use the provided tool.";
const TOOL_NAME: &str = "get_weather";
const TOOL_CITY: &str = "munich";

/// The `get_weather` tool.
const TOOL_SCHEMA: &str = "{\n    \"type\": \"function\",\n    \"function\": {\n      \
     \"name\": \"get_weather\",\n      \"description\": \"Get current weather for a city\",\n      \
     \"parameters\": {\n        \"type\": \"object\",\n        \"properties\": {\n          \
     \"city\": {\"type\": \"string\", \"description\": \"City name\"},\n          \
     \"unit\": {\"type\": \"string\", \"enum\": [\"celsius\", \"fahrenheit\"]}\n        },\n        \
     \"required\": [\"city\"]\n      }\n    }\n  }";

/// The vision check's picture: a red circle on white, 64x64 PNG, and what
/// is asked about it.
const VISION_PNG: &str = "iVBORw0KGgoAAAANSUhEUgAAAEAAAABACAYAAACqaXHeAAAAAXNSR0IArs4c6QAAAARnQU1BAACxjwv8YQUAAAAJcEhZcwAADsMAAA7DAcdvqGQAAAGDSURBVHhe7ZDRCgUhCET3/3/6XnwQlkEYLWuVOnBeYhqt53c4Dx6cxv0APDiN+wF4cBr3A/DgNO4H4MEynifuBtZOwQfNuIg1zbh8psnkNuKyK00isclYcrUJJLUYy+1ykoQGY6ndTjB521jmKweZuGks8bUDDN4yhlcxyMANY2g1A8TSAg6raIBg2hhWVSf+pIBDKuskkDSGVNeBLyVgeQcd+FIClnfQgTNllHeRwBMClnaSwBMClnaSwBMClnaSwBMClnaSwBMClnaS4EgYpZ0k8ISApZ0k8ISApZ0k8ISApZ0k8ISApZ0k8ISApZ0k8ISApZ0k8ISCxR104EsJWN5BB76UgOUddOBLKTigsk78SQGHVNaJP6ngoIoGiKUFHFbRALG0ggMrGSR+Q8HBFRxg7JaCC3zpIOM3FVzkCyeYu63gQjudZL5BwcV2mEBOi4ILrjSJvKY3uGymyeQ3vsHlZ1zEumYEH+RxA3umFOZ+AB6cxv0APDiN+wF4cBrHf8AfTKnU3JKFy1YAAAAASUVORK5CYII=";
const VISION_QUESTION: &str = "What colour is the circle in this picture? Answer with one word.";
const VISION_ANSWER: &str = "red";

/// The throughput prompt. The `\\n` and
/// `\\t` are two characters each: the body is JSON, where they are an escaped
/// newline and tab, and that is why the prompt can carry Go code at all.
const REVIEW_PROMPT: &str = "Review this Go snippet for correctness and idiomatic issues. Be \
     concise but complete.\\n\\nfunc LoadConfig(paths []string) (*Config, error) {\\n\\tdata, _ := \
     os.ReadFile(paths[0])\\n\\tvar cfg Config\\n\\tjson.Unmarshal(data, &cfg)\\n\\tif cfg.Timeout \
     == 0 { cfg.Timeout = 30 }\\n\\treturn &cfg, nil\\n}\\n\\nExplain each problem, rank by \
     severity, then show the fixed version.";

/// The haystack's filler line, its first line, and the one line the question
/// is about.
const HAYSTACK_LINE: &str =
    "The warehouse holds crates of silicon wafers and the manifests are filed by quarter.";
const HAYSTACK_HEAD: &str =
    "Below is a pile of warehouse log lines. Somewhere inside there is one IMPORTANT line.";
const NEEDLE: &str = "IMPORTANT: the release code for Q3 is stored under the passphrase 8127.";
/// The number the needle's answer must contain for the gate to pass.
const NEEDLE_ANSWER: &str = "8127";

/// How much of the asked-for haystack the server must report reading for a
/// PASS to count: the tokenizer-sized haystack lands within a fraction of a
/// percent of the target, so anything well short was cut.
const NEEDLE_READ_SHARE: f64 = 0.9;

/// The question asked at the needle's depth, and the follow-up that measures
/// decode speed there by reusing the whole haystack as a cached prefix.
const NEEDLE_QUESTION: &str =
    "\\n\\nWhat is the passphrase in the IMPORTANT line? Reply with the number only.";
const DEPTH_QUESTION: &str =
    "\\n\\nNow write a Python function that parses one of these log lines into a dict, with a docstring.";

/// The first guess at the model's characters per token. It is a guess on
/// purpose: `/tokenize` replaces it with the server's own count.
const CHARS_PER_TOKEN: f64 = 4.7;

/// `golem eval [--long TOKENS]`: 0 when the run completed (the verdicts are
/// in its lines), 1 when it could not. Its `PROGRESS` lines are written when
/// stdout is not a terminal ([`Steps`]).
pub fn run(home: &Path, args: &[String], out: &mut dyn Write) -> i32 {
    run_with(home, args, out, !std::io::stdout().is_terminal())
}

/// [`run`], with whether to write `PROGRESS` lines said rather than read off
/// the terminal.
pub fn run_with(home: &Path, args: &[String], out: &mut dyn Write, progress: bool) -> i32 {
    // The command line first: a bad argument is refused before anything is
    // printed or any request is made.
    let long = match parse_args(args) {
        Ok(long) => long,
        Err(arg) => {
            // stderr, so it is not part of what the recorder captures.
            eprintln!("unknown arg: {arg}");
            return 1;
        }
    };

    let Some(base) = Base::from_home(home) else {
        let configured = config::get(home, "LOCAL_AI_BASE_URL").unwrap_or_default();
        let _ = writeln!(
            out,
            "[FAIL] cannot reach the engine: LOCAL_AI_BASE_URL={configured:?} is not http:// \
             (this client speaks HTTP over a socket, with no TLS)"
        );
        return 1;
    };

    // The per-run override first (`GOLEM_MODEL_ALIAS`, how the auto-tuner
    // gates a candidate), then golem.toml. An override that does not reach
    // this line gates the configured model under the candidate's name - it
    // once gated a 0.5B candidate against the 35B coder's numbers.
    let model = config::resolved(home, "MODEL_ALIAS")
        .unwrap_or_else(|| DEFAULT_ALIAS.to_string());
    let temperature = setting(home, "TEMPERATURE", DEFAULT_TEMPERATURE);
    let top_p = setting(home, "TOP_P", DEFAULT_TOP_P);
    let mut steps = Steps::new(long.is_some(), progress);

    // ------------------------------------------------------------
    // 1. HEALTH + IDENTITY
    // ------------------------------------------------------------

    section(out, "1. HEALTH + IDENTITY");
    steps.next(out, "health");

    // An unhappy status is the same answer as no server at all.
    match engine::get_json(&base, "/health", HEALTH_TIMEOUT) {
        Some(reply) if reply.ok() => {
            let _ = writeln!(out, "[ OK ] /health");
        }
        _ => {
            let _ = writeln!(out, "[FAIL] server not responding");
            return 1;
        }
    }

    // `/props` is read whatever its status: whatever the engine says about
    // itself is printed, and a field it does not have is `?`.
    let props = parse_body(&get_body(&base, &format!("/props?model={model}"), PROPS_TIMEOUT));
    let _ = writeln!(
        out,
        "model_path : {}",
        props_string(&props, "model_path").unwrap_or_else(|| "?".to_string())
    );
    let _ = writeln!(
        out,
        "n_ctx      : {}",
        props_number(&props, "n_ctx").unwrap_or_else(|| "?".to_string())
    );

    // ------------------------------------------------------------
    // 2. WARMUP + ARITHMETIC SANITY
    // ------------------------------------------------------------

    section(out, "2. WARMUP + ARITHMETIC SANITY");
    steps.next(out, "warmup and sanity");

    let _ = writeln!(out, "warmup request (first hit may compile CUDA graphs, be patient)...");
    // A failed warmup ends the eval. A router loads the model on the request
    // that names it and tries again on the next, so every later step would
    // be another load - and a load that dies on the card resets the display
    // driver each time.
    match engine::post_json(&base, "/v1/chat/completions", &warmup_body(&model), RUN_TIMEOUT) {
        Some(reply) if reply.ok() => {}
        reply => {
            let said = reply.map_or_else(|| "no answer".to_string(), |r| r.error_message());
            let _ = writeln!(out, "[FAIL] warmup request failed: {said}");
            return 1;
        }
    }
    let _ = writeln!(out, "[ OK ] warmup done");
    // A router that had another model resident knew nothing of this one
    // before the warmup loaded it: the identity is read again, now.
    if props_string(&props, "model_path").is_none() {
        let props = parse_body(&get_body(&base, &format!("/props?model={model}"), PROPS_TIMEOUT));
        if let Some(path) = props_string(&props, "model_path") {
            let _ = writeln!(out, "model_path : {path} (once loaded)");
            let _ = writeln!(out, "n_ctx      : {}", props_number(&props, "n_ctx").unwrap_or_else(|| "?".to_string()));
        }
    }

    let answer = sanity_answer(&base, &model);
    let _ = writeln!(out, "answer: {answer}");
    if answer.contains(SANITY_ANSWER) {
        let _ = writeln!(out, "[ OK ] arithmetic sanity");
    } else {
        let _ = writeln!(
            out,
            "[FAIL] arithmetic sanity (expected {SANITY_ANSWER}) - quant damage / bad CUDA?"
        );
    }

    // ------------------------------------------------------------
    // 3. TOOL CALLING
    // ------------------------------------------------------------

    section(out, "3. TOOL CALLING");
    steps.next(out, "tool calling");

    let mut raw = post_body(&base, &tool_body(&model, false), REQUEST_TIMEOUT);
    // Some templates only emit tool calls with thinking disabled, so an
    // answer that carries none is asked again - and the retry's answer
    // replaces the first one outright, empty or not.
    if !has_tool_calls(&raw) {
        raw = post_body(&base, &tool_body(&model, true), REQUEST_TIMEOUT);
    }
    let (call, verdict) = tool_verdict(&raw);
    if let Some(call) = call {
        let _ = writeln!(out, "{call}");
    }
    let _ = writeln!(out, "{verdict}");

    // A model served with a vision projector is asked about a picture: a
    // projector that loaded but reads nothing passes every other step. The
    // engine says in `/props` whether it has one, which it only knows once
    // the model is loaded - after the warmup, not before.
    let props = parse_body(&get_body(&base, &format!("/props?model={model}"), PROPS_TIMEOUT));
    if deep(&props, "vision").and_then(Value::as_bool) == Some(true) {
        let reply = engine::post_json(&base, "/v1/chat/completions", &vision_body(&model), REQUEST_TIMEOUT);
        let answer = reply.as_ref().map(|reply| message_content(&reply.body)).unwrap_or_default();
        let shown = match &reply {
            _ if !answer.is_empty() => answer.trim().to_string(),
            Some(reply) if reply.ok() => "(no answer text)".to_string(),
            Some(reply) => format!("(request failed: HTTP {}: {})", reply.status, reply.error_message()),
            None => "(request failed: no answer: the connection failed or closed early)".to_string(),
        };
        let _ = writeln!(out, "answer: {shown}");
        if answer.to_ascii_lowercase().contains(VISION_ANSWER) {
            let _ = writeln!(out, "[ OK ] vision (the picture's colour)");
        } else {
            let _ = writeln!(out, "[FAIL] vision: expected {VISION_ANSWER} for a red circle");
        }
    }

    // ------------------------------------------------------------
    // 4. THROUGHPUT (3 timed runs)
    // ------------------------------------------------------------

    section(out, "4. THROUGHPUT (3 runs, code-review prompt, 192 gen tokens)");

    // Prompt caching is off so every run pays its prefill, and the numbers
    // are the server's own per-request `timings`: HTTP and queueing are
    // excluded, and so is each run from the others.
    let body = review_body(&model, &temperature, &top_p);
    let mut speeds: Vec<Option<String>> = Vec::with_capacity(3);
    for i in 1..=3 {
        steps.next(out, &format!("throughput run {i} of 3"));
        // Any status: a body that came back unhappy is still read for its
        // timings.
        let Some(reply) = engine::post_json(&base, "/v1/chat/completions", &body, RUN_TIMEOUT)
        else {
            // No answer at all ends the run: there is nothing to time.
            let _ = writeln!(out, "[FAIL] throughput request failed");
            return 1;
        };
        let value = parse_body(&reply.body);
        let _ = writeln!(out, "run {i}: {}", timings_line(&value));
        speeds.push(number_of(&value, "predicted_per_second"));
    }
    let _ = writeln!(out, "RESULT gen_tok_s_best={}", best_of(&speeds));

    // ------------------------------------------------------------
    // 5. LONG CONTEXT NEEDLE (optional)
    // ------------------------------------------------------------

    if let Some(long) = long {
        long_context(out, &base, &model, &long, &mut steps);

        // --------------------------------------------------------
        // 6. AGENT SUITE (with --long: the gate)
        // --------------------------------------------------------

        // The gate asks whether an agent gets work done on the setup, not
        // only whether the setup answers: the suite runs on both APIs, and
        // its RESULT lines are what the tuner's agent floor reads. A suite
        // the engine stopped answering prints why and leaves no RESULT
        // line, which the floor reads as not passed.
        section(out, "6. AGENT SUITE (both APIs)");
        steps.next(out, "agent suite");
        let _ = crate::agent::suite(&base, &model, &[Api::Anthropic, Api::OpenAi], out);
    }

    // ------------------------------------------------------------
    // DONE
    // ------------------------------------------------------------

    let _ = writeln!(out);
    let _ = writeln!(out, "============================================================");
    let _ = writeln!(out, " EVAL COMPLETE - {model}");
    let _ = writeln!(out, "============================================================");
    0
}

// ---------------------------------------------------------------
// Progress
// ---------------------------------------------------------------

/// The run's requests as `PROGRESS` steps: health, warmup, tools, three
/// timed runs, and with `--long` the needle, the depth request and the agent
/// suite.
///
/// Written when stdout is not a terminal - the console's job log, the
/// tuner's gate file - because on a terminal the sections are the progress.
/// The recorder reads `RESULT` lines and the two named ones, never these.
struct Steps {
    on: bool,
    done: u64,
    total: u64,
}

impl Steps {
    fn new(long: bool, on: bool) -> Steps {
        Steps { on, done: 0, total: if long { 9 } else { 6 } }
    }

    /// The request about to run.
    fn next(&mut self, out: &mut dyn Write, what: &str) {
        if self.on {
            let _ = writeln!(out, "{}", Progress::steps(self.done, self.total, format!("eval: {what}")).line());
        }
        self.done += 1;
    }
}

// ---------------------------------------------------------------
// The command line
// ---------------------------------------------------------------

/// `--long TOKENS`, and nothing else.
///
/// `Ok(None)` is no `--long` at all, which is what skips section 5. A flag
/// with a missing or empty value is a *different* thing: 32768 tokens,
/// measured.
fn parse_args(args: &[String]) -> Result<Option<String>, String> {
    let mut long = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--long" => {
                let value = args
                    .get(i + 1)
                    .filter(|value| !value.is_empty())
                    .cloned()
                    .unwrap_or_else(|| LONG_DEFAULT.to_string());
                // A count of tokens: anything else would size a haystack of
                // nothing and pass it.
                if !value.trim().parse::<u64>().is_ok_and(|n| n > 0) {
                    return Err(format!("--long {value}"));
                }
                long = Some(value);
                i += 2;
            }
            other => return Err(other.to_string()),
        }
    }
    Ok(long)
}

/// One golem.toml value, or `default` when it is missing or empty.
fn setting(home: &Path, key: &str, default: &str) -> String {
    config::get(home, key)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// A section heading: a blank line, a rule, the title, the rule.
fn section(out: &mut dyn Write, title: &str) {
    let _ = writeln!(out);
    let _ = writeln!(out, "------------------------------------------------------------");
    let _ = writeln!(out, " {title}");
    let _ = writeln!(out, "------------------------------------------------------------");
}

// ---------------------------------------------------------------
// Reading the answers
// ---------------------------------------------------------------

/// The parsed body, or `null` for an answer that is not JSON at all - every
/// field is then simply missing.
fn parse_body(text: &str) -> Value {
    json::parse(text).unwrap_or(Value::Null)
}

/// One member of an answer, from the server's per-request `timings` object
/// first and the top level second.
///
/// llama.cpp puts the counts under `timings`; an engine that puts them at
/// the top level is read too, rather than reading a perfectly good answer as
/// "no numbers".
fn member<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    value.get("timings").and_then(|timings| timings.get(key)).or_else(|| value.get(key))
}

/// One number, as the literal text the server wrote, so it prints exactly
/// as reported.
fn number_of(value: &Value, key: &str) -> Option<String> {
    match member(value, key) {
        Some(Value::Number { text, .. }) => Some(text.clone()),
        _ => None,
    }
}

/// The first `key` anywhere in the document, at any depth, in the order the
/// server wrote it.
///
/// It matters for `/props`: `model_path` is at the top level there, but
/// `n_ctx` is not - it lives under `default_generation_settings` - so a
/// top-level lookup reads a perfectly healthy server as "no answer" and
/// section 1 prints `?` instead of the context.
fn deep<'a>(value: &'a Value, key: &str) -> Option<&'a Value> {
    match value {
        Value::Object(members) => members
            .iter()
            .find_map(|(k, v)| if k == key { Some(v) } else { deep(v, key) }),
        Value::Array(items) => items.iter().find_map(|item| deep(item, key)),
        _ => None,
    }
}

/// A `/props` field, wherever the server nested it.
fn props_string(value: &Value, key: &str) -> Option<String> {
    deep(value, key).and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string)
}

/// A `/props` number, wherever the server nested it.
fn props_number(value: &Value, key: &str) -> Option<String> {
    match deep(value, key) {
        Some(Value::Number { text, .. }) => Some(text.clone()),
        _ => None,
    }
}

/// A number as a float, for the arithmetic that follows it.
fn timing(value: &Value, key: &str) -> Option<f64> {
    member(value, key).and_then(Value::as_f64)
}

/// The assistant's text: `content`, or `reasoning_content` for a thinking
/// model.
///
/// An empty `content` is "no text" and falls through to the reasoning; an
/// answer that is not JSON at all has no text either, and both are what make
/// section 2 fall back to its plain body.
fn message_content(text: &str) -> String {
    message_content_of(&parse_body(text))
}

/// The first choice's `message`, the object the text is read from.
fn choice_message(value: &Value) -> Option<&Value> {
    value.get("choices").and_then(first).and_then(|choice| choice.get("message"))
}

fn first(items: &Value) -> Option<&Value> {
    match items {
        Value::Array(items) => items.first(),
        _ => None,
    }
}

/// The first `n` bytes of `text`, cut on a character boundary: a cut that
/// would split a UTF-8 character falls just before it. One character of a
/// diagnostic line is not worth an invalid string.
fn head(text: &str, n: usize) -> String {
    if text.len() <= n {
        return text.to_string();
    }
    let mut end = n;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// `value` as compact JSON, rebuilt from the parse rather than sliced out of
/// the answer - the text the tool verdict searches.
fn compact(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number { text, .. } => text.clone(),
        Value::String(text) => json::quote(text),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(compact).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(members) => {
            let parts: Vec<String> =
                members.iter().map(|(key, item)| format!("{}:{}", json::quote(key), compact(item))).collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

// ---------------------------------------------------------------
// The request bodies
// ---------------------------------------------------------------

/// The warmup body: eight tokens of `"hi"`.
fn warmup_body(model: &str) -> String {
    format!(
        "{{\"model\": \"{model}\", \"messages\": [{{\"role\": \"user\", \"content\": \"hi\"}}], \
         \"max_tokens\": 8}}"
    )
}

/// The sanity body. `no_think` is the first attempt, which asks the template
/// for no thinking; the second is the plain body a template without the
/// kwarg accepts.
fn sanity_body(model: &str, no_think: bool) -> String {
    let kwargs = if no_think { "  \"chat_template_kwargs\": {\"enable_thinking\": false},\n" } else { "" };
    format!(
        "{{\n  \"model\": \"{model}\",\n  \"messages\": [{{\"role\": \"user\", \"content\": \
         \"{SANITY_QUESTION}\"}}],\n{kwargs}  \"temperature\": 0,\n  \"max_tokens\": 512\n}}"
    )
}

/// The vision body: the picture as a data URL, one word asked for, thinking
/// off so the word is the answer.
fn vision_body(model: &str) -> String {
    format!(
        "{{\"model\": \"{model}\", \"messages\": [{{\"role\": \"user\", \"content\": [\
         {{\"type\": \"image_url\", \"image_url\": {{\"url\": \"data:image/png;base64,{VISION_PNG}\"}}}}, \
         {{\"type\": \"text\", \"text\": \"{VISION_QUESTION}\"}}]}}], \
         \"chat_template_kwargs\": {{\"enable_thinking\": false}}, \"temperature\": 0, \"max_tokens\": 64}}"
    )
}

/// The tool body. The retry adds `chat_template_kwargs` after
/// `tool_choice`, to turn thinking off.
fn tool_body(model: &str, no_think: bool) -> String {
    let mut body = String::new();
    body.push_str("{\n  \"model\": \"");
    body.push_str(model);
    body.push_str("\",\n  \"messages\": [{\"role\": \"user\", \"content\": \"");
    body.push_str(TOOL_QUESTION);
    body.push_str("\"}],\n  \"tools\": [");
    body.push_str(TOOL_SCHEMA);
    body.push_str("],\n  \"tool_choice\": \"auto\",\n");
    if no_think {
        body.push_str("  \"chat_template_kwargs\": {\"enable_thinking\": false},\n");
    }
    body.push_str("  \"temperature\": 0,\n  \"max_tokens\": 1024\n}");
    body
}

/// The throughput body: the code-review prompt, 192 tokens, caching off, and
/// the temperature and top_p golem.toml names.
fn review_body(model: &str, temperature: &str, top_p: &str) -> String {
    format!(
        "{{\n  \"model\": \"{model}\",\n  \"messages\": [{{\"role\": \"user\", \"content\": \
         \"{REVIEW_PROMPT}\"}}],\n  \"chat_template_kwargs\": {{\"enable_thinking\": false}},\n  \
         \"cache_prompt\": false,\n  \"temperature\": {temperature},\n  \"top_p\": {top_p},\n  \
         \"max_tokens\": 192,\n  \"stream\": false\n}}"
    )
}

/// The three long-context bodies: the needle request, the depth request that
/// reuses the haystack from the cache, and the bare `content` the server's
/// own tokenizer counts.
struct Bodies {
    needle: String,
    depth: String,
    tokenize: String,
}

/// Build the haystack and the three bodies that carry it.
///
/// The needle goes in the middle, and the count is `target` tokens' worth of
/// the filler line at `cpt` characters per token; the first pass guesses the
/// ratio, and the caller corrects it once against the measured count.
fn haystack_bodies(model: &str, target: f64, cpt: f64) -> Bodies {
    // `reps = int(target * cpt / length(line)) + 1`, floored at 2 so the
    // needle has a line on each side.
    let mut reps = (target * cpt / HAYSTACK_LINE.len() as f64).trunc() as i64 + 1;
    if reps < 2 {
        reps = 2;
    }
    let half = reps / 2;

    // `\\n` is the two characters backslash and n: the body is JSON, where
    // that is an escaped newline, and it is why the haystack has no raw
    // newline to break the request.
    let mut hay = String::from(HAYSTACK_HEAD);
    for _ in 0..half {
        hay.push_str("\\n");
        hay.push_str(HAYSTACK_LINE);
    }
    hay.push_str("\\n");
    hay.push_str(NEEDLE);
    for _ in half..reps {
        hay.push_str("\\n");
        hay.push_str(HAYSTACK_LINE);
    }

    let common = "\"temperature\": 0, \"chat_template_kwargs\": {\"enable_thinking\": false}, \
                  \"stream\": false";
    Bodies {
        needle: format!(
            "{{\"model\": \"{model}\", \"messages\": [{{\"role\": \"user\", \"content\": \
             \"{hay}{NEEDLE_QUESTION}\"}}], {common}, \"cache_prompt\": false, \"max_tokens\": 16}}"
        ),
        depth: format!(
            "{{\"model\": \"{model}\", \"messages\": [{{\"role\": \"user\", \"content\": \
             \"{hay}{DEPTH_QUESTION}\"}}], {common}, \"cache_prompt\": true, \"max_tokens\": 256}}"
        ),
        // `/tokenize` takes the model in the body and a bare `content`, not a
        // chat request: the same text, counted by the server.
        tokenize: format!("{{\"model\": \"{model}\", \"content\": \"{hay}{NEEDLE_QUESTION}\"}}"),
    }
}

// ---------------------------------------------------------------
// The transport
// ---------------------------------------------------------------

/// `GET` a path and hand back the body, or empty when there was no answer.
fn get_body(base: &Base, path: &str, timeout: Duration) -> String {
    engine::get_json(base, path, timeout).map(|reply| reply.body).unwrap_or_default()
}

/// `POST` and hand back the body whatever its status, or empty when there
/// was no answer.
fn post_body(base: &Base, body: &str, timeout: Duration) -> String {
    engine::post_json(base, "/v1/chat/completions", body, timeout)
        .map(|reply| reply.body)
        .unwrap_or_default()
}

/// `POST` and hand back the body only for a 2xx - for the two long-context
/// requests, where an unhappy status is read as no answer at all rather
/// than as one. What went wrong instead comes back as the error, so the
/// report says why the request failed.
fn post_body_ok(base: &Base, body: &str, timeout: Duration) -> Result<String, String> {
    match engine::post_json(base, "/v1/chat/completions", body, timeout) {
        Some(reply) if reply.ok() => Ok(reply.body),
        Some(reply) => Err(format!("HTTP {}: {}", reply.status, reply.error_message())),
        None => Err("no answer: the connection failed or closed early".to_string()),
    }
}

// ---------------------------------------------------------------
// Section 2: arithmetic sanity
// ---------------------------------------------------------------

/// The arithmetic answer after at most two attempts, or `(request failed)`.
///
/// The first attempt disables thinking, which is what a Qwen3.x hybrid needs
/// to answer at all; a template that does not know the kwarg answers with
/// nothing usable, and the plain body is the retry.
fn sanity_answer(base: &Base, model: &str) -> String {
    let mut answer = message_content(&post_body(base, &sanity_body(model, true), REQUEST_TIMEOUT));
    if answer.is_empty() {
        answer = message_content(&post_body(base, &sanity_body(model, false), REQUEST_TIMEOUT));
    }
    if answer.is_empty() {
        return "(request failed)".to_string();
    }
    answer
}

// ---------------------------------------------------------------
// Section 3: tool calling
// ---------------------------------------------------------------

/// The tool calls in an answer, or `None` when it carries none.
///
/// An absent key, a `null` and an empty array are the same thing here - the
/// model did not call the tool - which is what decides the retry and the
/// verdict.
fn tool_calls(value: &Value) -> Option<&[Value]> {
    match choice_message(value).and_then(|message| message.get("tool_calls")) {
        Some(Value::Array(calls)) if !calls.is_empty() => Some(calls),
        _ => None,
    }
}

/// Whether an answer carries a usable tool call.
fn has_tool_calls(raw: &str) -> bool {
    tool_calls(&parse_body(raw)).is_some()
}

/// The two lines section 3 prints for one answer: the `tool_call :` line,
/// when the model called the tool, and the `[ OK ]`/`[WARN]`/`[FAIL]` line.
///
/// The calls, as compact JSON, are searched for the tool's name and for the
/// city; the line shows the arguments exactly as the model sent them.
fn tool_verdict(raw: &str) -> (Option<String>, String) {
    if raw.is_empty() {
        return (None, "[FAIL] tool request returned no response".to_string());
    }
    let value = parse_body(raw);
    let Some(calls) = tool_calls(&value) else {
        return (None, no_tool_call(raw));
    };
    let text = compact(&Value::Array(calls.to_vec()));
    if !text.contains(TOOL_NAME) {
        return (None, no_tool_call(raw));
    }
    // The first call's arguments, whole: the line exists to show them, and
    // they are a JSON-encoded string full of escaped quotes.
    let arguments = calls
        .first()
        .and_then(|call| call.get("function"))
        .and_then(|function| function.get("arguments"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let line = format!("tool_call : {TOOL_NAME}({arguments})");
    if text.to_lowercase().contains(TOOL_CITY) {
        (Some(line), "[ OK ] tool calling".to_string())
    } else {
        (Some(line), "[WARN] tool called with wrong city".to_string())
    }
}

/// `[FAIL] no tool call emitted; content=<200 bytes>`: the start of what the
/// model said instead.
fn no_tool_call(raw: &str) -> String {
    format!("[FAIL] no tool call emitted; content={}", head(&message_content(raw), 200))
}

// ---------------------------------------------------------------
// Section 4: throughput
// ---------------------------------------------------------------

/// One run's line: the server's own per-request numbers, with the draft
/// acceptance appended when speculative decoding was on.
///
/// A count or speed the server did not report prints as `0`/`0.0` - a model
/// that answered without `timings` still prints a line, and the line still
/// says what was missing.
fn timings_line(value: &Value) -> String {
    let prompt_n = number_of(value, "prompt_n").unwrap_or_else(|| "0".to_string());
    let prompt_tps = timing(value, "prompt_per_second").unwrap_or(0.0);
    let gen_n = number_of(value, "predicted_n").unwrap_or_else(|| "0".to_string());
    let gen_tps = timing(value, "predicted_per_second").unwrap_or(0.0);
    let mut line =
        format!("prompt={prompt_n} tok @ {prompt_tps:.0} t/s, gen={gen_n} tok @ {gen_tps:.1} t/s");
    // A model without speculative decoding has no `draft_n` at all, and one
    // that has it but drew no drafts prints no acceptance either.
    if let Some(draft_n) = number_of(value, "draft_n").filter(|n| n != "0") {
        let accepted = timing(value, "draft_n_accepted").unwrap_or(0.0);
        let drafted: f64 = draft_n.parse().unwrap_or(0.0);
        let accept = if drafted > 0.0 { 100.0 * accepted / drafted } else { 0.0 };
        line.push_str(&format!(" draft_accept={accept:.0}%"));
    }
    line
}

/// The largest of the three decode speeds: the number the server wrote (the
/// last of equals), or nothing at all when no run reported one.
fn best_of(speeds: &[Option<String>]) -> String {
    let mut best: Option<(&str, f64)> = None;
    for text in speeds.iter().flatten() {
        let value: f64 = text.parse().unwrap_or(0.0);
        if best.map_or(true, |(_, seen)| value >= seen) {
            best = Some((text.as_str(), value));
        }
    }
    best.map(|(text, _)| text.to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------
// Section 5: the needle
// ---------------------------------------------------------------

/// Section 5, from the haystack to the `RESULT` line.
///
/// The haystack is sized by the server's own tokenizer rather than by the
/// characters-per-token guess: that ratio belongs to the model's tokenizer,
/// and a gate that asks for 200K while sending 165K is testing something
/// else. One correction pass against the measured count is enough - the
/// ratio is linear enough that a second is noise.
fn long_context(out: &mut dyn Write, base: &Base, model: &str, long: &str, steps: &mut Steps) {
    section(out, &format!("5. NEEDLE-IN-HAYSTACK (~{long} tokens) + DECODE AT DEPTH"));

    let target: f64 = long.trim().parse().unwrap_or(0.0);
    let mut bodies = haystack_bodies(model, target, CHARS_PER_TOKEN);
    let mut counted = tokenize_count(base, &bodies.tokenize);
    if counted.is_some_and(|count| count > 0) {
        if counted != Some(target as u64) {
            // The corrected ratio, rounded to three decimals so a rerun
            // builds the same haystack.
            let corrected = format!("{:.3}", CHARS_PER_TOKEN * target / counted.unwrap_or(1) as f64);
            bodies = haystack_bodies(model, target, corrected.parse().unwrap_or(CHARS_PER_TOKEN));
            counted = tokenize_count(base, &bodies.tokenize);
        }
        let count = counted.map(|count| count.to_string()).unwrap_or_default();
        let _ = writeln!(out, "haystack: {count} tokens (asked for {long})");
    } else {
        let _ = writeln!(
            out,
            "haystack: sized by the {CHARS_PER_TOKEN} chars/token estimate (no /tokenize on this engine)"
        );
    }

    // The needle first, the depth request second: the follow-up shares the
    // whole haystack as a cached prefix, which is how an agent turn looks -
    // long history, short new tail.
    steps.next(out, &format!("the {long}-token needle (minutes of prefill)"));
    let needle_reply = post_body_ok(base, &bodies.needle, NEEDLE_TIMEOUT);
    let needle = parse_body(needle_reply.as_deref().unwrap_or(""));
    let mut answer = message_content_of(&needle);
    if answer.is_empty() {
        answer = match &needle_reply {
            Err(why) => format!("(request failed: {why})"),
            Ok(_) => "(no answer text)".to_string(),
        };
    }
    let prompt_tps = timing(&needle, "prompt_per_second").unwrap_or(0.0);
    let prompt_n = number_of(&needle, "prompt_n").unwrap_or_else(|| "0".to_string());
    let _ = writeln!(out, "needle : prompt={prompt_n} tok @ {prompt_tps:.0} t/s; answer: {answer}");

    steps.next(out, "decode at depth");
    let depth_reply = post_body_ok(base, &bodies.depth, NEEDLE_TIMEOUT);
    let depth = parse_body(depth_reply.as_deref().unwrap_or(""));
    let depth_tps = timing(&depth, "predicted_per_second").unwrap_or(0.0);
    let _ = writeln!(out, "depth  : {} (prefix from cache)", timings_line(&depth));
    if let Err(why) = &depth_reply {
        let _ = writeln!(out, "         the depth request failed: {why}");
    }

    // The passphrase as a number of its own (`8127.`, `**8127**`), not a
    // run of digits that contains it. And when the server says how much it
    // read, it must have read the haystack: a prompt cut to fit a smaller
    // context can keep the needle and lose the point of the test.
    let found = answer.split(|c: char| !c.is_ascii_digit()).any(|digits| digits == NEEDLE_ANSWER);
    let read: u64 = prompt_n.parse().unwrap_or(0);
    let short = read > 0 && (read as f64) < target * NEEDLE_READ_SHARE;
    let verdict = if short {
        let _ = writeln!(out, "[FAIL] the server read {read} tokens of the {long} asked for");
        "FAIL"
    } else if found {
        let _ = writeln!(out, "[ OK ] needle found");
        "PASS"
    } else {
        let _ = writeln!(out, "[FAIL] needle missed");
        "FAIL"
    };
    let _ = writeln!(
        out,
        "RESULT long_ctx_{long}={verdict} prompt_tok_s={prompt_tps:.0} gen_tok_s_at_depth={depth_tps:.1}"
    );
}

/// The assistant's text of an already-parsed answer - [`message_content`] for
/// a body the caller needs the timings of as well.
fn message_content_of(value: &Value) -> String {
    let message = choice_message(value).unwrap_or(value);
    message
        .get("content")
        .and_then(Value::as_str)
        .filter(|content| !content.is_empty())
        .or_else(|| {
            message
                .get("reasoning_content")
                .and_then(Value::as_str)
                .filter(|reasoning| !reasoning.is_empty())
        })
        .unwrap_or("")
        .to_string()
}

/// How many tokens the server sees in the tokenize body, or `None` when the
/// engine has no `/tokenize` (then the estimate stands).
///
/// The count is the length of the answer's `tokens` array. Never fails: a
/// missing endpoint, an unreadable answer and an empty array are all "no
/// count".
fn tokenize_count(base: &Base, body: &str) -> Option<u64> {
    let reply = engine::post_json(base, "/tokenize", body, TOKENIZE_TIMEOUT)?;
    match parse_body(&reply.body).get("tokens") {
        Some(Value::Array(tokens)) if !tokens.is_empty() => Some(tokens.len() as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use golem_core::evalparse;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;

    /// A home whose golem.toml names `port`, so `Base::from_home` points the
    /// run at a fake engine rather than the real one.
    fn home(tag: &str, port: u16) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-eval-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        std::fs::write(dir.join("golem.toml"), format!("LOCAL_AI_PORT=\"{port}\"\n")).expect("write");
        dir
    }

    /// A fake engine that answers `replies` in order, one connection each: a
    /// body under `200 OK`, or a reply that is already a whole response.
    fn serve(replies: Vec<String>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else { return };
                read_request(&mut stream);
                let head = if reply.starts_with("HTTP/1.1 ") {
                    String::new()
                } else {
                    format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", reply.len())
                };
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    /// Read one whole request: the head, then `Content-Length` bytes of body.
    /// The haystack body is ~1 MB at 200K tokens, and a server that answered
    /// before reading it would reset the connection mid-write.
    fn read_request(stream: &mut TcpStream) {
        let mut data: Vec<u8> = Vec::new();
        let mut buf = [0u8; 8192];
        let head_end = loop {
            if let Some(at) = data.windows(4).position(|w| w == b"\r\n\r\n") {
                break at + 4;
            }
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => data.extend_from_slice(&buf[..n]),
            }
        };
        let head = String::from_utf8_lossy(&data[..head_end]).to_ascii_lowercase();
        let length: usize = head
            .lines()
            .find_map(|line| line.strip_prefix("content-length:"))
            .and_then(|value| value.trim().parse().ok())
            .unwrap_or(0);
        while data.len() < head_end + length {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => data.extend_from_slice(&buf[..n]),
            }
        }
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    fn run_out(dir: &Path, list: &[&str]) -> (i32, String) {
        run_progress(dir, list, false)
    }

    fn run_progress(dir: &Path, list: &[&str], progress: bool) -> (i32, String) {
        let mut out: Vec<u8> = Vec::new();
        let status = run_with(dir, &args(list), &mut out, progress);
        (status, String::from_utf8(out).expect("utf-8"))
    }

    fn props_reply() -> String {
        "{\"model_path\": \"/models/ornith.gguf\", \"n_ctx\": 249344}".to_string()
    }

    #[test]
    fn a_props_field_is_found_wherever_the_server_nested_it() {
        // What llama.cpp actually answers: `model_path` at the top, `n_ctx`
        // under `default_generation_settings`. A top-level lookup reads a
        // healthy server as "no answer" and prints `?`.
        let body = "{\"model_path\":\"/m/a.gguf\",\"default_generation_settings\":{\
                    \"params\":{\"temperature\":0.6},\"n_ctx\":249344},\"total_slots\":1}";
        let props = parse_body(body);
        assert_eq!(props_string(&props, "model_path").as_deref(), Some("/m/a.gguf"));
        assert_eq!(props_number(&props, "n_ctx").as_deref(), Some("249344"));
        // The first match in document order, depth first.
        let twice = parse_body("{\"a\":{\"n_ctx\":1},\"n_ctx\":2}");
        assert_eq!(props_number(&twice, "n_ctx").as_deref(), Some("1"));
        // And a key that is not there at any depth is still absent.
        assert_eq!(props_number(&props, "n_embd"), None);
    }

    fn sanity_reply(content: &str) -> String {
        format!("{{\"choices\": [{{\"message\": {{\"role\": \"assistant\", \"content\": \"{content}\"}}}}]}}")
    }

    /// An answer whose single tool call asks for `city`. `arguments` is a
    /// JSON string, which is the shape the OpenAI API and llama.cpp both use.
    fn tool_reply(city: &str) -> String {
        let arguments = format!("{{\"city\": \"{city}\"}}");
        format!(
            "{{\"choices\": [{{\"message\": {{\"role\": \"assistant\", \"content\": null, \
             \"tool_calls\": [{{\"id\": \"call_1\", \"type\": \"function\", \"function\": \
             {{\"name\": \"get_weather\", \"arguments\": {}}}}}]}}}}]}}",
            json::quote(&arguments)
        )
    }

    /// A throughput answer carrying the server's `timings`.
    fn run_reply(prompt_n: u64, prompt_tps: f64, gen_n: u64, gen_tps: f64) -> String {
        format!(
            "{{\"timings\": {{\"prompt_n\": {prompt_n}, \"prompt_per_second\": {prompt_tps}, \
             \"predicted_n\": {gen_n}, \"predicted_per_second\": {gen_tps}}}}}"
        )
    }

    /// The `/tokenize` answer: an array of ids, which is all the count reads.
    fn tokenize_reply(count: u64) -> String {
        let ids: Vec<String> = (1..=count).map(|id| id.to_string()).collect();
        format!("{{\"tokens\": [{}]}}", ids.join(", "))
    }

    /// The nine answers a run without `--long` asks for: health, props,
    /// warmup, sanity, tool, props again (no vision here), and three timed
    /// runs.
    fn short_run(sanity: &str, tool: String) -> Vec<String> {
        vec![
            "{}".to_string(),
            props_reply(),
            "{}".to_string(),
            sanity_reply(sanity),
            tool,
            props_reply(),
            run_reply(115, 7977.4, 192, 328.0),
            run_reply(115, 7977.4, 192, 330.2),
            run_reply(115, 7977.4, 192, 325.1),
        ]
    }

    // -----------------------------------------------------------
    // The command line
    // -----------------------------------------------------------

    #[test]
    fn the_command_line_takes_only_the_long_flag() {
        // No `--long`: no long-context section at all.
        assert_eq!(parse_args(&[]), Ok(None));
        // `--long` with no value, and with an empty one: both ask for 32768.
        assert_eq!(parse_args(&args(&["--long"])), Ok(Some("32768".to_string())));
        assert_eq!(parse_args(&args(&["--long", ""])), Ok(Some("32768".to_string())));
        assert_eq!(parse_args(&args(&["--long", "200000"])), Ok(Some("200000".to_string())));
        // Anything else is refused, and the message is the argument itself.
        assert_eq!(parse_args(&args(&["--bogus"])), Err("--bogus".to_string()));
        assert_eq!(
            parse_args(&args(&["--long", "5000", "extra"])),
            Err("extra".to_string())
        );
    }

    #[test]
    fn an_unknown_argument_is_refused_before_anything_is_asked() {
        // Port 1: nothing listens there, so a run that got past the command
        // line would fail the health check instead of printing nothing.
        let dir = home("unknown-arg", 1);
        let (status, text) = run_out(&dir, &["--bogus"]);
        assert_eq!(status, 1);
        assert_eq!(text, "", "the refusal goes to stderr, out of the recorder's capture");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_server_that_answers_nothing_fails_the_health_check() {
        let dir = home("dead", 1);
        let (status, text) = run_out(&dir, &[]);
        assert_eq!(status, 1);
        assert!(text.contains(" 1. HEALTH + IDENTITY\n"), "the section comes first:\n{text}");
        assert!(text.ends_with("[FAIL] server not responding\n"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_https_base_url_is_refused_with_the_reason() {
        let dir = home("https", 8000);
        std::fs::write(dir.join("golem.toml"), "LOCAL_AI_BASE_URL=\"https://example.test/v1\"\n")
            .expect("write");
        let (status, text) = run_out(&dir, &[]);
        assert_eq!(status, 1);
        assert!(text.contains("[FAIL]"), "{text}");
        assert!(text.contains("https://example.test/v1"), "the reason is named:\n{text}");
        assert!(text.contains("no TLS"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------
    // The whole run
    // -----------------------------------------------------------

    #[test]
    fn in_a_job_log_each_request_is_a_progress_step_and_the_contract_is_untouched() {
        let port = serve(short_run("391", tool_reply("Munich")));
        let dir = home("progress", port);
        let (status, text) = run_progress(&dir, &[], true);
        assert_eq!(status, 0);
        let steps: Vec<&str> = text.lines().filter(|l| l.starts_with("PROGRESS ")).collect();
        assert_eq!(
            steps,
            [
                "PROGRESS 0/6 steps eval: health",
                "PROGRESS 1/6 steps eval: warmup and sanity",
                "PROGRESS 2/6 steps eval: tool calling",
                "PROGRESS 3/6 steps eval: throughput run 1 of 3",
                "PROGRESS 4/6 steps eval: throughput run 2 of 3",
                "PROGRESS 5/6 steps eval: throughput run 3 of 3",
            ]
        );
        // Without them, the run is the same bytes the recorder has always read.
        let port = serve(short_run("391", tool_reply("Munich")));
        let plain_dir = home("progress-plain", port);
        let plain = run_out(&plain_dir, &[]).1;
        assert_eq!(golem_core::progress::strip(&text) + "\n", plain);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&plain_dir).ok();
    }

    #[test]
    fn a_model_with_vision_is_asked_about_the_picture() {
        let seeing = "{\"model_path\": \"/m/a.gguf\", \"modalities\": {\"vision\": true}}".to_string();
        let mut replies = short_run("391", tool_reply("Munich"));
        replies[5] = seeing.clone();
        replies.insert(6, sanity_reply("Red."));
        let port = serve(replies);
        let dir = home("vision", port);
        let (status, text) = run_out(&dir, &[]);
        assert_eq!(status, 0);
        assert!(text.contains("answer: Red.\n[ OK ] vision (the picture's colour)\n"), "{text}");

        let mut replies = short_run("391", tool_reply("Munich"));
        replies[5] = seeing;
        replies.insert(6, sanity_reply("Blue"));
        let port = serve(replies);
        let blind = home("vision-blind", port);
        let (_, text) = run_out(&blind, &[]);
        assert!(text.contains("[FAIL] vision: expected red"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&blind).ok();
    }

    #[test]
    fn the_whole_run_prints_the_contract_line_for_line() {
        let port = serve(short_run("391", tool_reply("Munich")));
        let dir = home("golden", port);
        let (status, text) = run_out(&dir, &[]);
        assert_eq!(status, 0);

        let rule = "------------------------------------------------------------";
        let banner = "============================================================";
        let expected = format!(
            "\n{rule}\n 1. HEALTH + IDENTITY\n{rule}\n\
             [ OK ] /health\n\
             model_path : /models/ornith.gguf\n\
             n_ctx      : 249344\n\
             \n{rule}\n 2. WARMUP + ARITHMETIC SANITY\n{rule}\n\
             warmup request (first hit may compile CUDA graphs, be patient)...\n\
             [ OK ] warmup done\n\
             answer: 391\n\
             [ OK ] arithmetic sanity\n\
             \n{rule}\n 3. TOOL CALLING\n{rule}\n\
             tool_call : get_weather({{\"city\": \"Munich\"}})\n\
             [ OK ] tool calling\n\
             \n{rule}\n 4. THROUGHPUT (3 runs, code-review prompt, 192 gen tokens)\n{rule}\n\
             run 1: prompt=115 tok @ 7977 t/s, gen=192 tok @ 328.0 t/s\n\
             run 2: prompt=115 tok @ 7977 t/s, gen=192 tok @ 330.2 t/s\n\
             run 3: prompt=115 tok @ 7977 t/s, gen=192 tok @ 325.1 t/s\n\
             RESULT gen_tok_s_best=330.2\n\
             \n{banner}\n EVAL COMPLETE - local-model\n{banner}\n"
        );
        assert_eq!(text, expected);

        // And the RESULT line is what the recorder and the tuner read.
        let eval = evalparse::Eval::parse(text.lines());
        assert_eq!(eval.gen_tok_s_best, Some(330.2));
        assert!(eval.needle.is_none(), "no --long, no needle line");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_configured_alias_is_the_one_the_banner_names() {
        // The tuner gates a candidate by setting GOLEM_MODEL_ALIAS, which
        // `config::resolved` prefers; the golem.toml value is what a plain
        // run uses, and it is the one the banner has to name.
        let port = serve(short_run("391", tool_reply("Munich")));
        let dir = home("alias", port);
        std::fs::write(dir.join("golem.toml"), format!("LOCAL_AI_PORT=\"{port}\"\nMODEL_ALIAS=\"ornith-1.5-35b\"\n"))
            .expect("write");
        let (status, text) = run_out(&dir, &[]);
        assert_eq!(status, 0);
        assert!(
            text.contains("\n EVAL COMPLETE - ornith-1.5-35b\n"),
            "the alias golem.toml names is the one printed:\n{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_wrong_arithmetic_answer_is_a_failed_sanity_check() {
        // Quant damage reads as the wrong number, and the run continues.
        let port = serve(short_run("393", tool_reply("Munich")));
        let dir = home("sanity", port);
        let (status, text) = run_out(&dir, &[]);
        assert_eq!(status, 0);
        assert!(text.contains("answer: 393\n"), "{text}");
        assert!(
            text.contains("[FAIL] arithmetic sanity (expected 391) - quant damage / bad CUDA?\n"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn there_are_four_tool_verdicts() {
        // No response at all.
        assert_eq!(
            tool_verdict(""),
            (None, "[FAIL] tool request returned no response".to_string())
        );
        // An answer that carries no tool call: the content is quoted back.
        let (line, verdict) = tool_verdict(&sanity_reply("I cannot check the weather."));
        assert_eq!(line, None);
        assert_eq!(verdict, "[FAIL] no tool call emitted; content=I cannot check the weather.");
        // A `tool_calls` key that is there and empty is the same thing.
        let empty = "{\"choices\": [{\"message\": {\"tool_calls\": []}}]}";
        assert!(!has_tool_calls(empty));
        assert!(tool_verdict(empty).1.starts_with("[FAIL] no tool call emitted"));

        // The tool, with the right city.
        let (line, verdict) = tool_verdict(&tool_reply("Munich"));
        assert_eq!(line.as_deref(), Some("tool_call : get_weather({\"city\": \"Munich\"})"));
        assert_eq!(verdict, "[ OK ] tool calling");
        // The tool, with another city: a warning, not a failure.
        let (line, verdict) = tool_verdict(&tool_reply("Paris"));
        assert_eq!(line.as_deref(), Some("tool_call : get_weather({\"city\": \"Paris\"})"));
        assert_eq!(verdict, "[WARN] tool called with wrong city");
        // A call to some other tool is not a tool call for this test.
        let other = sanity_reply("get_weather is not a tool I have");
        assert!(!has_tool_calls(&other));
        assert!(tool_verdict(&other).1.starts_with("[FAIL] no tool call emitted"));
    }

    #[test]
    fn the_content_of_a_failed_tool_call_is_cut_to_200_bytes() {
        let long = "x".repeat(500);
        let raw = sanity_reply(&long);
        let verdict = tool_verdict(&raw).1;
        assert_eq!(verdict, format!("[FAIL] no tool call emitted; content={}", "x".repeat(200)));
        // A multi-byte character on the boundary is dropped rather than split.
        let text = "é".repeat(200);
        assert_eq!(head(&text, 201).len(), 200, "one byte short of a character");
        assert_eq!(head("short", 200), "short");
    }

    #[test]
    fn an_answer_without_tool_calls_is_asked_again_with_thinking_off() {
        // The first answer has no tool call at all; the retry, with the
        // template's thinking disabled, does - which is the whole reason the
        // retry exists.
        let mut replies = short_run("391", sanity_reply("no tool call here"));
        replies.insert(5, tool_reply("Munich"));
        let port = serve(replies);
        let dir = home("retry", port);
        let (status, text) = run_out(&dir, &[]);
        assert_eq!(status, 0);
        assert!(text.contains("[ OK ] tool calling\n"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------
    // Section 4
    // -----------------------------------------------------------

    #[test]
    fn the_timings_line_reports_the_servers_numbers() {
        let value = parse_body(
            "{\"timings\": {\"prompt_n\": 115, \"prompt_per_second\": 7977.4, \
             \"predicted_n\": 192, \"predicted_per_second\": 328.04}}",
        );
        assert_eq!(timings_line(&value), "prompt=115 tok @ 7977 t/s, gen=192 tok @ 328.0 t/s");

        // Speculative decoding on: the acceptance is appended as a percentage.
        let drafted = parse_body(
            "{\"timings\": {\"prompt_n\": 115, \"prompt_per_second\": 7977.0, \
             \"predicted_n\": 192, \"predicted_per_second\": 328.0, \"draft_n\": 100, \
             \"draft_n_accepted\": 66}}",
        );
        assert_eq!(
            timings_line(&drafted),
            "prompt=115 tok @ 7977 t/s, gen=192 tok @ 328.0 t/s draft_accept=66%"
        );

        // No drafts drawn, and no timings at all: neither prints an acceptance.
        let none = parse_body("{\"timings\": {\"draft_n\": 0}}");
        assert_eq!(timings_line(&none), "prompt=0 tok @ 0 t/s, gen=0 tok @ 0.0 t/s");
        assert_eq!(timings_line(&parse_body("not json")), "prompt=0 tok @ 0 t/s, gen=0 tok @ 0.0 t/s");
    }

    #[test]
    fn the_best_speed_is_the_largest_and_keeps_the_servers_own_text() {
        let speeds = vec![
            Some("328.0".to_string()),
            Some("330.2".to_string()),
            Some("325.1".to_string()),
        ];
        assert_eq!(best_of(&speeds), "330.2");
        // A run the server reported nothing for is not the best of anything.
        let speeds = vec![None, Some("62.8".to_string()), None];
        assert_eq!(best_of(&speeds), "62.8");
        assert_eq!(best_of(&[None, None, None]), "", "no run measured, no number");
        assert_eq!(best_of(&[]), "");
    }

    // -----------------------------------------------------------
    // Section 5
    // -----------------------------------------------------------

    #[test]
    fn the_haystack_holds_the_needle_in_the_middle() {
        // 100 tokens at 4.7 characters each is 470 characters; the filler
        // line is 84, so `int(470 / 84) + 1` is six lines and the needle goes
        // after the third. A needle that is not in the haystack would be a
        // gate that always fails, so this pins the arithmetic.
        let bodies = haystack_bodies("m", 100.0, CHARS_PER_TOKEN);
        let value = json::parse(&bodies.needle).expect("the needle body is JSON");
        let content = value
            .get("messages")
            .and_then(first)
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .expect("a prompt");
        assert_eq!(content.matches(HAYSTACK_LINE).count(), 6);
        assert_eq!(content.matches(NEEDLE).count(), 1, "exactly one needle");
        let (before, _) = content.split_once(NEEDLE).expect("the needle is in there");
        assert_eq!(before.matches(HAYSTACK_LINE).count(), 3, "the needle is at the half");
        assert!(content.starts_with(HAYSTACK_HEAD), "{content}");
        assert!(
            content.ends_with("What is the passphrase in the IMPORTANT line? Reply with the number only."),
            "the question is last, so the whole haystack is a cacheable prefix: {content}"
        );

        // The depth request is the same haystack with the other question, and
        // it is the one that asks for the prefix from the cache.
        let depth = json::parse(&bodies.depth).expect("the depth body is JSON");
        assert_eq!(depth.get("cache_prompt"), Some(&Value::Bool(true)));
        let prompt = depth
            .get("messages")
            .and_then(first)
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
            .expect("a prompt");
        assert!(prompt.contains(NEEDLE), "the depth request carries the haystack too");

        // `/tokenize` takes the same text as a bare `content`, not as a chat
        // request: the server counts what it would see.
        let tokenize = json::parse(&bodies.tokenize).expect("the tokenize body is JSON");
        assert_eq!(tokenize.get("content").and_then(Value::as_str), Some(content));
    }

    #[test]
    fn the_long_context_section_reports_the_needle_and_the_depth_speed() {
        let mut replies = short_run("391", tool_reply("Munich"));
        // The first count is wrong (120 for 100 asked), so the run corrects
        // the ratio once and re-counts - which is what the second answer is.
        replies.push(tokenize_reply(120));
        replies.push(tokenize_reply(98));
        replies.push(
            "{\"choices\": [{\"message\": {\"content\": \"8127\"}}], \
             \"timings\": {\"prompt_n\": 120, \"prompt_per_second\": 19311.0}}"
                .to_string(),
        );
        replies.push(
            "{\"timings\": {\"prompt_n\": 120, \"prompt_per_second\": 19311.0, \
             \"predicted_n\": 256, \"predicted_per_second\": 254.3}}"
                .to_string(),
        );
        let port = serve(replies);
        let dir = home("needle", port);
        let (status, text) = run_out(&dir, &["--long", "100"]);
        assert_eq!(status, 0);

        assert!(
            text.contains(" 5. NEEDLE-IN-HAYSTACK (~100 tokens) + DECODE AT DEPTH\n"),
            "{text}"
        );
        assert!(text.contains("haystack: 98 tokens (asked for 100)\n"), "{text}");
        assert!(text.contains("needle : prompt=120 tok @ 19311 t/s; answer: 8127\n"), "{text}");
        assert!(
            text.contains("depth  : prompt=120 tok @ 19311 t/s, gen=256 tok @ 254.3 t/s (prefix from cache)\n"),
            "{text}"
        );
        assert!(text.contains("[ OK ] needle found\n"), "{text}");

        // The RESULT line the tuner's gate reads, parsed back by the module
        // that will read it in production.
        let eval = evalparse::Eval::parse(text.lines());
        let needle = eval.needle.expect("a needle result");
        assert_eq!(needle.tokens, 100);
        assert_eq!(needle.verdict, evalparse::Verdict::Pass);
        assert_eq!(needle.prompt_tok_s, Some(19311.0));
        assert_eq!(needle.gen_tok_s_at_depth, Some(254.3));
        assert_eq!(eval.gen_tok_s_best, Some(330.2));

        // And the row the recorder builds out of it: the two lines that are
        // not `RESULT` lines are the ones that carry `tools` and the token
        // count, and they are the reason they may not be reworded.
        let facts = golem_core::record::Facts {
            kind: golem_core::results::Kind::Eval,
            ts: "t".into(),
            model: "m".into(),
            ..golem_core::record::Facts::default()
        };
        let row = golem_core::record::row(&facts, &text).expect("a row");
        assert!(
            row.contains(
                "\"eval\": {\"tools\": \"PASS\", \"needle\": \"PASS\", \"needle_tokens\": 120, \
                 \"needle_prefill\": 19311, \"depth_decode\": 254.3, \"decode_best\": 330.2}"
            ),
            "{row}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_model_that_will_not_load_ends_the_eval_at_the_warmup() {
        // The router's answer when the load died; every later step would be
        // another load, so none is asked for.
        let failed = "{\"error\":{\"code\":500,\"message\":\"model name=m failed to load\",\"type\":\"server_error\"}}";
        let replies = vec![
            "{}".to_string(),
            props_reply(),
            format!("HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\n\r\n{failed}", failed.len()),
        ];
        let port = serve(replies);
        let dir = home("no-load", port);
        let (status, text) = run_out(&dir, &["--long", "100"]);
        assert_eq!(status, 1);
        assert!(text.contains("[FAIL] warmup request failed: model name=m failed to load\n"), "{text}");
        assert!(!text.contains("TOOL CALLING"), "{text}");
        let eval = evalparse::Eval::parse(text.lines());
        assert!(eval.needle.is_none(), "no needle was run");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_missed_needle_is_a_failed_gate() {
        let mut replies = short_run("391", tool_reply("Munich"));
        replies.push(tokenize_reply(100));
        replies.push(sanity_reply("I could not find it."));
        replies.push("{\"timings\": {}}".to_string());
        let port = serve(replies);
        let dir = home("missed", port);
        let (status, text) = run_out(&dir, &["--long", "100"]);
        assert_eq!(status, 0);
        assert!(text.contains("[FAIL] needle missed\n"), "{text}");
        assert!(
            text.contains("RESULT long_ctx_100=FAIL prompt_tok_s=0 gen_tok_s_at_depth=0.0\n"),
            "{text}"
        );
        let eval = evalparse::Eval::parse(text.lines());
        assert_eq!(eval.needle.expect("a needle result").verdict, evalparse::Verdict::Fail);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_engine_without_tokenize_falls_back_to_the_estimate() {
        let mut replies = short_run("391", tool_reply("Munich"));
        // Not an answer with a `tokens` array: the engine has no such
        // endpoint, and the 4.7 chars/token estimate stands.
        replies.push("{\"error\": \"not found\"}".to_string());
        replies.push(sanity_reply("8127"));
        replies.push("{\"timings\": {}}".to_string());
        let port = serve(replies);
        let dir = home("no-tokenize", port);
        let (status, text) = run_out(&dir, &["--long", "100"]);
        assert_eq!(status, 0);
        assert!(
            text.contains("haystack: sized by the 4.7 chars/token estimate (no /tokenize on this engine)\n"),
            "{text}"
        );
        assert!(text.contains("[ OK ] needle found\n"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    // -----------------------------------------------------------
    // The bodies
    // -----------------------------------------------------------

    #[test]
    fn the_request_bodies_are_fixed() {
        // The warmup: eight tokens of "hi".
        let warmup = json::parse(&warmup_body("m")).expect("json");
        assert_eq!(warmup.get("max_tokens").and_then(Value::as_f64), Some(8.0));
        assert_eq!(warmup.get("model").and_then(Value::as_str), Some("m"));

        // The sanity pair: the first disables thinking, the second does not.
        let no_think = json::parse(&sanity_body("m", true)).expect("json");
        assert_eq!(no_think.get("chat_template_kwargs").and_then(|k| k.get("enable_thinking")).and_then(Value::as_bool), Some(false));
        let plain = json::parse(&sanity_body("m", false)).expect("json");
        assert_eq!(plain.get("chat_template_kwargs"), None);

        // The tool request carries the tool and the question; the retry is
        // the same body with thinking off.
        let tool = json::parse(&tool_body("m", false)).expect("json");
        assert_eq!(tool.get("tool_choice").and_then(Value::as_str), Some("auto"));
        assert_eq!(tool.get("chat_template_kwargs"), None);
        let schema = tool.get("tools").and_then(first).and_then(|t| t.get("function")).expect("a function");
        assert_eq!(schema.get("name").and_then(Value::as_str), Some("get_weather"));
        assert_eq!(
            json::parse(&tool_body("m", true)).expect("json").get("chat_template_kwargs").and_then(|k| k.get("enable_thinking")).and_then(Value::as_bool),
            Some(false)
        );

        // The throughput body: 192 tokens, caching off, and the two settings
        // from golem.toml in the request.
        let review = json::parse(&review_body("m", "0.15", "0.95")).expect("json");
        assert_eq!(review.get("max_tokens").and_then(Value::as_f64), Some(192.0));
        assert_eq!(review.get("cache_prompt").and_then(Value::as_bool), Some(false));
        assert_eq!(review.get("temperature").and_then(Value::as_f64), Some(0.15));
        assert_eq!(review.get("top_p").and_then(Value::as_f64), Some(0.95));
        let content = review.get("messages").and_then(first).and_then(|m| m.get("content")).and_then(Value::as_str).expect("a prompt");
        assert!(content.contains("func LoadConfig(paths []string) (*Config, error) {"), "{content}");
        assert!(content.contains("\tdata, _ := os.ReadFile(paths[0])"), "the tab survived: {content}");
    }

    #[test]
    fn the_temperature_and_top_p_come_from_the_config() {
        let dir = home("settings", 1);
        std::fs::write(dir.join("golem.toml"), "TEMPERATURE=\"0.6\"\nTOP_P=\"\"\n").expect("write");
        assert_eq!(setting(&dir, "TEMPERATURE", DEFAULT_TEMPERATURE), "0.6");
        // An empty value counts as unset.
        assert_eq!(setting(&dir, "TOP_P", DEFAULT_TOP_P), "0.95");
        // And a key the file does not have at all is the same thing.
        assert_eq!(setting(&dir, "NOPE", "0.15"), "0.15");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn compact_rebuilds_the_text_the_verdict_searches() {
        let value = json::parse("{\"a\": [1, \"two\", null], \"b\": {\"c\": true}}").expect("json");
        assert_eq!(compact(&value), "{\"a\":[1,\"two\",null],\"b\":{\"c\":true}}");
    }
}
