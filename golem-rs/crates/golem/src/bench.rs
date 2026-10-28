//! `golem bench`: decode and prefill throughput, measured against the engine.
//!
//! Numbers come from the server's own per-request `timings`, so HTTP and
//! queueing are excluded; prompt caching is off per request, so the prefill
//! number is a real prefill. Quality - tool calls, the long-context needle -
//! is [`crate::eval`]'s.
//!
//! The output is a contract: a human reads it, and `golem_core::record`
//! parses its `RESULT` line. The two prompts are fixed - the decode prompt
//! verbatim, the prefill prompt from a deterministic generator - because
//! every row in `bench/results.jsonl` was measured with them, and a new row
//! is only comparable with the old ones while they stay the same. The
//! transport is this binary's own HTTP over the socket (see
//! [`crate::engine`]), so a system proxy cannot be picked up by accident.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use golem_core::json::{self, Value};

use crate::config;
use crate::engine;

/// The whole budget for one measured request: a 30K-token prefill takes
/// minutes, and silence in the middle is not a dead server.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(1800);

/// The health check's budget: a live engine answers at once.
const HEALTH_TIMEOUT: Duration = Duration::from_secs(5);

/// How many decode runs when the line names none.
const DEFAULT_RUNS: u32 = 3;

/// How long a prefill prompt when the line names none. `0` skips the test.
const DEFAULT_PREFILL: u32 = 30000;

/// The decode prompt: 600 tokens of code generation is the shape an agentic
/// coding client asks for. Fixed, so rows stay comparable.
const DECODE_PROMPT: &str = "Write a complete Python module implementing an LRU cache with TTL expiry, thread safety, a decorator interface, type hints and docstrings, followed by pytest unit tests covering eviction, expiry and concurrency. Output only code.";

/// `golem bench [--runs N] [--prefill TOKENS]`: 0 when every request was
/// answered, 1 otherwise.
pub fn run(home: &Path, args: &[String], out: &mut dyn Write) -> i32 {
    run_with(home, args, out, &vram_used)
}

/// [`run`], with the one machine fact it needs - the VRAM reading - handed
/// in rather than taken.
///
/// The seam exists for the tests: a test must not need nvidia-smi, and this
/// box has one, so a bench that shelled out to it inside `run` would make
/// every assertion depend on the GPU's current load. `vram` returns the
/// trimmed number, or `None` when nvidia-smi is not there.
fn run_with(
    home: &Path,
    args: &[String],
    out: &mut dyn Write,
    vram: &dyn Fn() -> Option<String>,
) -> i32 {
    // The arguments come first: a bad command line is refused before
    // anything is printed or any request is made.
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            let _ = writeln!(out, "{message}");
            return 1;
        }
    };

    let Some(base) = engine::Base::from_home(home) else {
        let configured = config::get(home, "LOCAL_AI_BASE_URL").unwrap_or_default();
        let _ = writeln!(
            out,
            "[FAIL] cannot reach the engine: LOCAL_AI_BASE_URL={configured:?} is not http:// \
             (this client speaks HTTP over a socket, with no TLS)"
        );
        return 1;
    };

    // The per-run override first (`GOLEM_MODEL_ALIAS`, how the auto-tuner
    // points a bench at each preset), then golem.toml, then the engine's
    // default name.
    let model = config::resolved(home, "MODEL_ALIAS")
        .unwrap_or_else(|| "local-model".to_string());

    let url = format!("http://{}:{}{}", base.host, base.port, base.path("/v1/chat/completions"));

    // The banner first, so a failed health check still says what it tried.
    let _ = writeln!(out);
    let _ = writeln!(out, "============================================================");
    let _ = writeln!(out, " SPEED BENCHMARK  ({url})");
    let _ = writeln!(out, "============================================================");

    // Health: five seconds of patience, and a refusal that names the port.
    let up = engine::get_json(&base, "/health", HEALTH_TIMEOUT).is_some_and(|r| r.ok());
    if !up {
        let _ = writeln!(out, "[FAIL] server not responding on :{}", base.port);
        return 1;
    }

    // Warmup: it proves the API key before the runs are spent on 401s, and
    // it warms the model. Any non-2xx answer stops here, in the engine's own
    // words: a rejected key and a model that failed to load look alike
    // otherwise.
    let warmup = warmup_body(&model);
    match engine::post_json(&base, "/v1/chat/completions", &warmup, REQUEST_TIMEOUT) {
        Some(reply) if reply.ok() => {}
        reply => {
            let said = reply.map_or_else(|| "no answer".to_string(), |r| r.error_message());
            let _ = writeln!(out, "[FAIL] warmup request failed: {said}");
            return 1;
        }
    }

    // ------------------------------------------------------------
    // Decode
    // ------------------------------------------------------------

    let _ = writeln!(out);
    let _ = writeln!(out, "decode: {} runs x 600 tokens of code generation", options.runs);

    let mut speeds: Vec<f64> = Vec::new();
    let mut draft_n: u64 = 0;
    let mut draft_ok: u64 = 0;
    for i in 1..=options.runs {
        let body = decode_body(&model, 41 + i);
        let Some(reply) = engine::post_json(&base, "/v1/chat/completions", &body, REQUEST_TIMEOUT)
            .filter(|r| r.ok())
        else {
            // One failed run fails the bench: a best-of-two is not the
            // number the row claims.
            let _ = writeln!(out, "[FAIL] decode request failed");
            return 1;
        };
        let value = parse_body(&reply.body);
        let tps = timing(&value, "predicted_per_second").unwrap_or(0.0);
        // A model without speculative decoding has no `draft_n` at all,
        // which counts as no drafts rather than an error.
        draft_n += timing(&value, "draft_n").unwrap_or(0.0) as u64;
        draft_ok += timing(&value, "draft_n_accepted").unwrap_or(0.0) as u64;
        let _ = writeln!(
            out,
            "  run {i}: {tps:6.1} tok/s  ({} tokens)",
            token_count(&value, "predicted_n")
        );
        speeds.push(tps);
    }

    // Sorted, the last is the best and the middle is the median. An empty
    // list (--runs 0) reads as 0.0 on both.
    speeds.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let best = speeds.last().copied().unwrap_or(0.0);
    let median = median(&speeds);
    // No drafts measured means the acceptance is the literal `-`, not `0%`:
    // a model with no head did not accept nothing, it drafted nothing.
    let accept = if draft_n > 0 {
        format!("{:.0}%", 100.0 * draft_ok as f64 / draft_n as f64)
    } else {
        "-".to_string()
    };
    let _ = writeln!(
        out,
        "  best {best:.1} / median {median:.1} tok/s, draft acceptance {accept}"
    );

    // ------------------------------------------------------------
    // Prefill
    // ------------------------------------------------------------

    let mut prefill = "-".to_string();
    if options.prefill > 0 {
        let _ = writeln!(out);
        let _ = writeln!(out, "prefill: ~{}-token code prompt", options.prefill);
        let body = prefill_body(&model, options.prefill);
        let Some(reply) = engine::post_json(&base, "/v1/chat/completions", &body, REQUEST_TIMEOUT)
            .filter(|r| r.ok())
        else {
            let _ = writeln!(out, "[FAIL] prefill request failed");
            return 1;
        };
        let value = parse_body(&reply.body);
        prefill = format!("{:.0}", timing(&value, "prompt_per_second").unwrap_or(0.0));
        let _ = writeln!(
            out,
            "  {prefill} tok/s  ({} tokens)",
            token_count(&value, "prompt_n")
        );
    }

    // The last fact is the machine's, not the engine's.
    let vram = match vram() {
        Some(used) => format!("{used}MiB"),
        None => "-".to_string(),
    };

    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "RESULT decode_best={best:.1} decode_median={median:.1} draft_accept={accept} \
         prefill={prefill} vram={vram}"
    );
    0
}

// ---------------------------------------------------------------
// The command line
// ---------------------------------------------------------------

/// The parsed command line: two options and their defaults.
#[derive(Debug, PartialEq, Eq)]
struct Options {
    runs: u32,
    prefill: u32,
}

/// `--runs N` and `--prefill TOKENS`, in either order.
///
/// Anything else is `unknown arg: <arg>` and exit 1, as is a flag with a
/// missing or non-numeric value.
fn parse_args(args: &[String]) -> Result<Options, String> {
    let mut options = Options { runs: DEFAULT_RUNS, prefill: DEFAULT_PREFILL };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--runs" => {
                options.runs = number(args, i, "--runs")?;
                i += 2;
            }
            "--prefill" => {
                options.prefill = number(args, i, "--prefill")?;
                i += 2;
            }
            other => return Err(format!("unknown arg: {other}")),
        }
    }
    Ok(options)
}

/// The value after the flag at `at`, as a number.
fn number(args: &[String], at: usize, flag: &str) -> Result<u32, String> {
    let value = args.get(at + 1).ok_or_else(|| format!("{flag} needs a value"))?;
    value.parse().map_err(|_| format!("{flag} wants a number, got {value:?}"))
}

// ---------------------------------------------------------------
// Reading the answer
// ---------------------------------------------------------------

/// The parsed body, or `null` for an answer that is not JSON at all - the
/// timings are then simply missing.
fn parse_body(text: &str) -> Value {
    json::parse(text).unwrap_or(Value::Null)
}

/// One number from the server's per-request `timings`, or `None` when the
/// key is missing. Read from the parsed object, so compact and
/// pretty-printed answers read the same.
fn timing(value: &Value, key: &str) -> Option<f64> {
    value.get("timings")?.get(key)?.as_f64()
}

/// A token count as printed: the number, or nothing at all when the key is
/// missing.
fn token_count(value: &Value, key: &str) -> String {
    match timing(value, key) {
        Some(n) => format!("{n:.0}"),
        None => String::new(),
    }
}

/// The middle value of a sorted list, or the mean of the two middles when
/// the count is even.
fn median(sorted: &[f64]) -> f64 {
    match sorted.len() {
        0 => 0.0,
        n if n % 2 == 1 => sorted[n / 2],
        n => (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0,
    }
}

/// `nvidia-smi --query-gpu=memory.used --format=csv,noheader,nounits`, first
/// line trimmed.
///
/// `None` when nvidia-smi is not there; the caller turns that into the
/// literal `-`.
fn vram_used() -> Option<String> {
    let out = Command::new("nvidia-smi")
        .args(["--query-gpu=memory.used", "--format=csv,noheader,nounits"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(text.lines().next().unwrap_or("").trim().to_string())
}

// ---------------------------------------------------------------
// The request bodies
// ---------------------------------------------------------------

/// The warmup body: 16 tokens of `"hi"`, thinking off.
fn warmup_body(model: &str) -> String {
    format!(
        "{{\"model\": \"{model}\", \"max_tokens\": 16,\n \
         \"messages\": [{{\"role\": \"user\", \"content\": \"hi\"}}],\n \
         \"chat_template_kwargs\": {{\"enable_thinking\": false}}}}\n"
    )
}

/// The decode body: 600 tokens, a seed that moves per run, caching off.
fn decode_body(model: &str, seed: u32) -> String {
    format!(
        "{{\"model\": \"{model}\", \"max_tokens\": 600, \"seed\": {seed}, \
         \"cache_prompt\": false,\n \
         \"messages\": [{{\"role\": \"user\", \"content\": \"{DECODE_PROMPT}\"}}],\n \
         \"chat_template_kwargs\": {{\"enable_thinking\": false}}}}\n"
    )
}

/// The prefill body: a synthetic, JSON-safe Python module of
/// `PREFILL_TOKENS * 11 / 4` characters (~2.75 chars/token), one token of
/// output, caching off.
///
/// The target is an integer division, and the loop appends whole handlers
/// until the running length reaches it - so the body overshoots by at most
/// one handler, which is why the token count is "about" what was asked for.
/// Deterministic, so every row's prompt is the same text.
fn prefill_body(model: &str, tokens: u32) -> String {
    let chars = tokens as u64 * 11 / 4;
    let mut body = format!(
        "{{\"model\": \"{model}\", \"max_tokens\": 1, \"cache_prompt\": false, \
         \"chat_template_kwargs\": {{\"enable_thinking\": false}}, \
         \"messages\": [{{\"role\": \"user\", \"content\": \"Summarize this module:\\n"
    );
    let mut len = 0u64;
    let mut n = 0u64;
    while len < chars {
        let handler = handler(n);
        len += handler.len() as u64;
        body.push_str(&handler);
        n += 1;
    }
    body.push_str("\"}]}");
    body
}

/// One synthetic handler.
///
/// `\\n` here is the two characters backslash and `n`: the body is JSON,
/// where that is an escaped newline, and it is why the prompt has no raw
/// newlines or quotes to break the request.
fn handler(n: u64) -> String {
    format!(
        "def handler_{n}(request, ctx):\\n    value = ctx.get(key_{n}, {n}) * {}\\n    \
         if value > {}:\\n        return process(request, value, retries={})\\n    \
         return None\\n\\n",
        (n * 7) % 13,
        (n * 3) % 97,
        n % 5,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;

    /// A home whose golem.toml names `port`, so `Base::from_home` points the
    /// run at a fake engine rather than the real one.
    fn home(tag: &str, port: u16) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("golem-rs-bench-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp");
        std::fs::write(dir.join("golem.toml"), format!("LOCAL_AI_PORT=\"{port}\"\n")).expect("write");
        dir
    }

    /// A fake engine that answers `replies` in order, one connection each.
    fn serve(replies: Vec<String>) -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for reply in replies {
                let Ok((mut stream, _)) = listener.accept() else { return };
                read_request(&mut stream);
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", reply.len());
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(reply.as_bytes());
                let _ = stream.flush();
            }
        });
        port
    }

    /// Read one whole request: the head, then `Content-Length` bytes of body.
    /// The prefill body is ~80 KB, and a server that answered before reading
    /// it would reset the connection in the middle of the client's write.
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

    /// A decode answer carrying the server's `timings`.
    fn decode_reply(tps: f64, tokens: u64, draft_n: u64, draft_ok: u64) -> String {
        format!(
            "{{\"timings\": {{\"predicted_per_second\": {tps}, \"predicted_n\": {tokens}, \
             \"draft_n\": {draft_n}, \"draft_n_accepted\": {draft_ok}}}}}"
        )
    }

    /// A prefill answer.
    fn prefill_reply(tps: f64, tokens: u64) -> String {
        format!("{{\"timings\": {{\"prompt_per_second\": {tps}, \"prompt_n\": {tokens}}}}}")
    }

    /// A stub for the machine fact the tests must not depend on.
    fn no_vram() -> Option<String> {
        None
    }

    fn run_out(dir: &Path, args: &[String], vram: &dyn Fn() -> Option<String>) -> (i32, String) {
        let mut out: Vec<u8> = Vec::new();
        let status = run_with(dir, args, &mut out, vram);
        (status, String::from_utf8(out).expect("utf-8"))
    }

    #[test]
    fn three_runs_report_the_best_and_the_median() {
        // Speeds out of order, so the best and the median cannot both come
        // from reading the list top to bottom.
        let port = serve(vec![
            "{}".to_string(),
            "{}".to_string(),
            decode_reply(60.0, 600, 100, 66),
            decode_reply(65.0, 600, 100, 66),
            decode_reply(62.5, 600, 100, 66),
            prefill_reply(1000.0, 30000),
        ]);
        let dir = home("best-median", port);
        let (status, text) = run_out(&dir, &[], &no_vram);

        assert_eq!(status, 0);
        let line = "============================================================";
        let expected = format!(
            "\n{line}\n SPEED BENCHMARK  (http://127.0.0.1:{port}/v1/chat/completions)\n{line}\n\
             \ndecode: 3 runs x 600 tokens of code generation\n\
             \x20 run 1:   60.0 tok/s  (600 tokens)\n\
             \x20 run 2:   65.0 tok/s  (600 tokens)\n\
             \x20 run 3:   62.5 tok/s  (600 tokens)\n\
             \x20 best 65.0 / median 62.5 tok/s, draft acceptance 66%\n\
             \nprefill: ~30000-token code prompt\n\
             \x20 1000 tok/s  (30000 tokens)\n\
             \nRESULT decode_best=65.0 decode_median=62.5 draft_accept=66% prefill=1000 vram=-\n"
        );
        assert_eq!(text, expected);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_model_without_speculative_decoding_has_no_acceptance() {
        // `draft_n` 0: the model decodes without drafts, so the acceptance is
        // the literal `-` rather than `0%`.
        let port = serve(vec![
            "{}".to_string(),
            "{}".to_string(),
            decode_reply(50.0, 600, 0, 0),
        ]);
        let dir = home("no-draft", port);
        let args: Vec<String> = ["--runs", "1", "--prefill", "0"].iter().map(|s| s.to_string()).collect();
        let (status, text) = run_out(&dir, &args, &no_vram);

        assert_eq!(status, 0);
        assert!(text.contains("  best 50.0 / median 50.0 tok/s, draft acceptance -\n"), "{text}");
        assert!(
            text.ends_with("RESULT decode_best=50.0 decode_median=50.0 draft_accept=- prefill=- vram=-\n"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prefill_zero_skips_the_prefill_test() {
        let port = serve(vec![
            "{}".to_string(),
            "{}".to_string(),
            decode_reply(50.0, 600, 100, 50),
        ]);
        let dir = home("prefill-zero", port);
        let args: Vec<String> = ["--runs", "1", "--prefill", "0"].iter().map(|s| s.to_string()).collect();
        let (status, text) = run_out(&dir, &args, &no_vram);

        assert_eq!(status, 0);
        assert!(!text.contains("prefill:"), "no prefill section when it is skipped:\n{text}");
        assert!(
            text.ends_with("RESULT decode_best=50.0 decode_median=50.0 draft_accept=50% prefill=- vram=-\n"),
            "{text}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_vram_reading_is_appended_to_the_result_line() {
        let port = serve(vec![
            "{}".to_string(),
            "{}".to_string(),
            decode_reply(50.0, 600, 100, 50),
        ]);
        let dir = home("vram", port);
        let args: Vec<String> = ["--runs", "1", "--prefill", "0"].iter().map(|s| s.to_string()).collect();
        let (status, text) = run_out(&dir, &args, &|| Some("11000".to_string()));

        assert_eq!(status, 0);
        assert!(text.ends_with("vram=11000MiB\n"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_unknown_argument_is_refused() {
        // No server: the command line is refused before anything is asked.
        let dir = home("unknown-arg", 1);
        let args = vec!["--bogus".to_string()];
        let (status, text) = run_out(&dir, &args, &no_vram);
        assert_eq!(status, 1);
        assert_eq!(text, "unknown arg: --bogus\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_server_that_answers_nothing_fails_the_health_check() {
        // Port 1 on loopback: nothing listens there, and nothing hangs.
        let dir = home("dead", 1);
        let (status, text) = run_out(&dir, &[], &no_vram);
        assert_eq!(status, 1);
        assert!(
            text.contains(" SPEED BENCHMARK  (http://127.0.0.1:1/v1/chat/completions)\n"),
            "the banner comes first:\n{text}"
        );
        assert!(text.ends_with("[FAIL] server not responding on :1\n"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_https_base_url_is_refused_with_the_reason() {
        // The client has no TLS, so an https base is refused out loud rather
        // than connected to and left to fail like a broken server.
        let dir = home("https", 8000);
        std::fs::write(dir.join("golem.toml"), "LOCAL_AI_BASE_URL=\"https://example.test/v1\"\n")
            .expect("write");
        let (status, text) = run_out(&dir, &[], &no_vram);
        assert_eq!(status, 1);
        assert!(text.contains("[FAIL]"), "{text}");
        assert!(text.contains("https://example.test/v1"), "the reason is named:\n{text}");
        assert!(text.contains("no TLS"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_median_is_the_middle_or_the_mean_of_the_two_middles() {
        assert_eq!(median(&[60.0, 62.5, 65.0]), 62.5);
        assert_eq!(median(&[60.0, 62.0, 64.0, 66.0]), 63.0);
        assert_eq!(median(&[7.0]), 7.0);
        assert_eq!(median(&[]), 0.0, "an empty run list reads as 0.0");
    }

    #[test]
    fn a_missing_timing_key_is_not_a_number() {
        let value = json::parse(
            "{\"timings\": {\"predicted_per_second\": 62.8, \"predicted_n\": 600, \
             \"draft_n\": 0}}",
        )
        .expect("json");
        assert_eq!(timing(&value, "predicted_per_second"), Some(62.8));
        assert_eq!(timing(&value, "predicted_n"), Some(600.0));
        assert_eq!(timing(&value, "draft_n"), Some(0.0));
        // No speculative decoding: the key is simply not there.
        assert_eq!(timing(&value, "draft_n_accepted"), None);
        assert_eq!(timing(&value, "prompt_per_second"), None);
        assert_eq!(token_count(&value, "predicted_n"), "600");
        assert_eq!(token_count(&value, "prompt_n"), "", "a missing count prints nothing");
    }

    #[test]
    fn pretty_printed_timings_read_the_same_as_compact_ones() {
        let pretty = json::parse(
            "{\n  \"timings\": {\n    \"predicted_per_second\": 12.3,\n    \
             \"predicted_n\": 600\n  }\n}",
        )
        .expect("json");
        assert_eq!(timing(&pretty, "predicted_per_second"), Some(12.3));
        // And a server that reports no timings at all is all-None, not an error.
        let none = json::parse("{}").expect("json");
        assert_eq!(timing(&none, "predicted_per_second"), None);
        assert_eq!(parse_body("not json at all").get("timings"), None);
    }

    #[test]
    fn the_command_line_takes_two_flags_in_either_order() {
        let empty: [String; 0] = [];
        assert_eq!(parse_args(&empty), Ok(Options { runs: 3, prefill: 30000 }));
        let both: Vec<String> = ["--runs", "5", "--prefill", "0"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_args(&both), Ok(Options { runs: 5, prefill: 0 }));
        // Reversed, and a flag it refuses.
        let reversed: Vec<String> = ["--prefill", "100", "--runs", "2"].iter().map(|s| s.to_string()).collect();
        assert_eq!(parse_args(&reversed), Ok(Options { runs: 2, prefill: 100 }));
        let bad = vec!["--nope".to_string()];
        assert_eq!(parse_args(&bad), Err("unknown arg: --nope".to_string()));
    }

    #[test]
    fn the_prefill_generator_is_fixed_to_the_character() {
        // Character for character, so every row's prompt is the same: `\n`
        // is the two characters the JSON needs, and the three numbers are
        // the handler's own arithmetic.
        assert_eq!(
            handler(0),
            "def handler_0(request, ctx):\\n    value = ctx.get(key_0, 0) * 0\\n    \
             if value > 0:\\n        return process(request, value, retries=0)\\n    \
             return None\\n\\n"
        );
        let third = handler(3);
        assert!(third.contains("def handler_3(request, ctx):"), "{third}");
        assert!(third.contains("key_3, 3) * 8"), "21 % 13 = 8: {third}");
        assert!(third.contains("if value > 9:"), "9 % 97 = 9: {third}");
        assert!(third.contains("retries=3"), "3 % 5 = 3: {third}");
    }

    #[test]
    fn the_prefill_body_is_json_of_the_asked_size() {
        // ~2.75 chars/token, integer division, whole handlers only.
        let small = prefill_body("local-model", 4); // 11 chars -> one handler
        assert!(small.starts_with(
            "{\"model\": \"local-model\", \"max_tokens\": 1, \"cache_prompt\": false, \
             \"chat_template_kwargs\": {\"enable_thinking\": false}, \
             \"messages\": [{\"role\": \"user\", \"content\": \"Summarize this module:\\n"
        ));
        assert!(small.ends_with("\"}]}"));
        assert!(small.contains("def handler_0(request, ctx):"));
        assert!(!small.contains("def handler_1("), "one handler already exceeds 11 chars");

        // And the point of the generator: a body the JSON reader accepts, so
        // no quote or raw newline can break the request.
        let body = prefill_body("local-model", 400); // 1100 chars -> many handlers
        let value = json::parse(&body).expect("the generated prompt is JSON");
        let Some(Value::Array(messages)) = value.get("messages") else {
            panic!("messages is an array: {body}");
        };
        let content = messages[0].get("content").and_then(Value::as_str).expect("the prompt is a string");
        assert!(content.starts_with("Summarize this module:\n"));
        assert!(content.contains("def handler_"));
        assert!(content.ends_with("return None\n\n"), "the last handler ends the prompt");
    }
}
