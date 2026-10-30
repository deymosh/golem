//! The router's own account of its models (`GET /models` on llama.cpp in
//! router mode): which of them it tried to load and could not.
//!
//! The router loads a model on the request that names it, and a load that
//! fails is tried again on the next such request. A load that dies on the
//! card - a CUDA out-of-memory - resets the display driver on Windows, so a
//! client that retries turns one bad load into a black screen every half
//! minute. The service reads this to stop the engine after the first one.

use crate::json::{self, Value};

/// A model the router tried to load and could not.
#[derive(Debug, Clone, PartialEq)]
pub struct FailedLoad {
    pub model: String,
    /// The load's exit code, when the router reports one.
    pub exit_code: Option<i64>,
}

/// The models `GET /models` marks as failed (`"status": {"failed": true}`),
/// in the order it lists them. A body that is not the router's listing - an
/// error page, a single-model server's answer - lists none.
pub fn failed_loads(body: &str) -> Vec<FailedLoad> {
    let Ok(value) = json::parse(body) else { return Vec::new() };
    let Some(Value::Array(items)) = value.get("data") else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let status = item.get("status")?;
            if status.get("failed").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            Some(FailedLoad {
                model: item.get("id")?.as_str()?.to_string(),
                exit_code: status.get("exit_code").and_then(Value::as_f64).map(|c| c as i64),
            })
        })
        .collect()
}

/// Why a load failed, read from its exit code and the engine's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// A failed `GGML_ASSERT`: the engine aborted (0xC0000409 on Windows).
    /// The assert line, when the log has it.
    Abort(Option<String>),
    /// The card ran out of memory while loading.
    OutOfMemory,
    /// The model's file is not on disk.
    NotDownloaded(String),
    /// Anything else; the log says.
    Other,
}

/// Windows' exit code for a process that aborted (`STATUS_STACK_BUFFER_OVERRUN`).
const ABORTED: i64 = -1073740791;

/// The cause of `model`'s failed load, from its exit code and the tail of
/// the engine's log: only the lines since the router last spawned `model`
/// count, so an earlier failure still in the tail is not blamed, and the
/// newest line that names a cause wins.
pub fn cause(model: &str, exit_code: Option<i64>, log: &str) -> Cause {
    let marker = format!("spawning server instance with name={model} ");
    let log = log.rfind(&marker).map_or(log, |at| &log[at..]);
    for line in log.lines().rev() {
        if let Some(at) = line.find("GGML_ASSERT") {
            let start = line[..at].rfind(']').map_or(0, |b| b + 1);
            return Cause::Abort(Some(line[start..].trim().to_string()));
        }
        if line.contains("out of memory") {
            return Cause::OutOfMemory;
        }
        // The router lists every preset's files at its start (a router line,
        // no port) - only a child's own failure to open its file is the load's.
        if line.starts_with('[') {
            if let Some(at) = line.find("failed to open GGUF file '") {
                let file = line[at + "failed to open GGUF file '".len()..].split('\'').next().unwrap_or("");
                return Cause::NotDownloaded(file.to_string());
            }
        }
    }
    match exit_code {
        Some(ABORTED) | Some(3221226505) => Cause::Abort(None),
        _ => Cause::Other,
    }
}

impl Cause {
    /// The cause in words, for the stop reason.
    pub fn words(&self) -> String {
        match self {
            Cause::Abort(Some(line)) => format!("engine aborted (assert): {line}"),
            Cause::Abort(None) => "engine aborted (assert, 0xC0000409)".to_string(),
            Cause::OutOfMemory => "out of memory on the card".to_string(),
            Cause::NotDownloaded(file) => format!("{file} is not downloaded"),
            Cause::Other => String::new(),
        }
    }

    /// Whether the service stops the engine over it. A missing file fails
    /// at once and touches no memory, so a retry costs nothing and the other
    /// models stay served; every other failure may have reset the card.
    pub fn stops_the_engine(&self) -> bool {
        !matches!(self, Cause::NotDownloaded(_))
    }
}

/// The line the service records when it stops the engine over `failed`,
/// `cause` being the first one's.
pub fn stop_reason(failed: &[FailedLoad], cause: &Cause) -> String {
    let names: Vec<String> = failed
        .iter()
        .map(|f| match f.exit_code {
            Some(code) => format!("{} (exit code {code})", f.model),
            None => f.model.clone(),
        })
        .collect();
    let why = cause.words();
    let why = if why.is_empty() { String::new() } else { format!(" ({why})") };
    let hint = match cause {
        Cause::Abort(Some(line)) if line.contains("llama-mmap.cpp") => {
            " - a lock of a split host buffer; load-mode = mmap+mlock pins the same way and loads"
        }
        _ => "",
    };
    format!(
        "stopped by the service: {} failed to load{why}{hint}, and a retry would load it again - \
         see logs/llama-server.log",
        names.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stop_reason_names_each_model_and_its_code() {
        let failed = vec![
            FailedLoad { model: "a".into(), exit_code: Some(-1073740791) },
            FailedLoad { model: "b".into(), exit_code: None },
        ];
        let reason = stop_reason(&failed, &Cause::Other);
        assert!(reason.contains("a (exit code -1073740791), b failed to load,"), "{reason}");
        assert!(reason.contains("logs/llama-server.log"));
    }

    #[test]
    fn the_cause_is_read_from_the_log() {
        // The 35B under Vulkan with load-mode = mlock, b11379 on an RX 7800 XT.
        let log = "\
0.00.104.900 E gguf_init_from_file: failed to open GGUF file 'mmproj-Ornith-1.5-9B-BF16.gguf'
[50123] D:/a/llama.cpp/llama.cpp/src/llama-mmap.cpp:804: GGML_ASSERT(addr) failed
1.00.400.000 I srv           run: instance name=ornith-1.5-35b exited with status -1073740791
";
        let aborted = cause("ornith-1.5-35b", Some(-1073740791), log);
        assert_eq!(aborted, Cause::Abort(Some("D:/a/llama.cpp/llama.cpp/src/llama-mmap.cpp:804: GGML_ASSERT(addr) failed".into())));
        let reason = stop_reason(&[FailedLoad { model: "ornith-1.5-35b".into(), exit_code: Some(-1073740791) }], &aborted);
        assert!(reason.contains("engine aborted (assert): D:/a/"), "{reason}");
        assert!(reason.contains("load-mode = mmap+mlock"), "{reason}");
        assert!(aborted.stops_the_engine());

        // The router's own probe of a preset that is not downloaded is not a
        // load's cause; a child's failure to open its file is, and does not
        // stop the engine.
        assert_eq!(cause_of_log("0.00.1 E gguf_init_from_file: failed to open GGUF file 'x.gguf'\n"), Cause::Other);
        let missing = cause_of_log("[50123] 0.00.1 E gguf_init_from_file: failed to open GGUF file 'x.gguf'\n");
        assert_eq!(missing, Cause::NotDownloaded("x.gguf".into()));
        assert!(!missing.stops_the_engine());
        assert_eq!(cause_of_log("[1] 0.0 E load_model: cudaMalloc failed: out of memory\n"), Cause::OutOfMemory);
        assert_eq!(cause("x", Some(-1073740791), ""), Cause::Abort(None));
        // An earlier model's assert is not this load's cause.
        let later = format!("{log}2.00.000.000 I srv          load: spawning server instance with name=other on port 50200
[50200] 0.0 E load_model: cudaMalloc failed: out of memory
");
        assert_eq!(cause("other", Some(1), &later), Cause::OutOfMemory);
    }

    fn cause_of_log(log: &str) -> Cause {
        cause("x", Some(1), log)
    }

    #[test]
    fn a_failed_load_is_named_with_its_exit_code() {
        // The router's answer after a load that could not read its model,
        // b11379, 2026-10-25 (args and preset trimmed).
        let body = r#"{"data":[
            {"id":"ok","object":"model","status":{"value":"loaded"}},
            {"id":"idle","object":"model","status":{"value":"unloaded"}},
            {"id":"bogus","object":"model","status":{"value":"unloaded","exit_code":1,"failed":true}}
        ],"object":"list"}"#;
        assert_eq!(failed_loads(body), vec![FailedLoad { model: "bogus".into(), exit_code: Some(1) }]);
    }

    #[test]
    fn a_crash_code_survives_and_a_missing_one_is_none() {
        // A CUDA abort on Windows exits with 0xC0000409.
        let body = r#"{"data":[
            {"id":"a","status":{"value":"unloaded","exit_code":-1073740791,"failed":true}},
            {"id":"b","status":{"value":"unloaded","failed":true}}
        ]}"#;
        let failed = failed_loads(body);
        assert_eq!(failed[0].exit_code, Some(-1073740791));
        assert_eq!(failed[1], FailedLoad { model: "b".into(), exit_code: None });
    }

    #[test]
    fn anything_but_the_listing_lists_nothing() {
        assert!(failed_loads("").is_empty());
        assert!(failed_loads(r#"{"error":{"code":401}}"#).is_empty());
        assert!(failed_loads(r#"{"data":[{"id":"x","status":{"value":"loaded","failed":false}}]}"#).is_empty());
    }
}
