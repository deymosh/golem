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

/// The line the service records when it stops the engine over `failed`.
pub fn stop_reason(failed: &[FailedLoad]) -> String {
    let names: Vec<String> = failed
        .iter()
        .map(|f| match f.exit_code {
            Some(code) => format!("{} (exit code {code})", f.model),
            None => f.model.clone(),
        })
        .collect();
    format!(
        "stopped by the service: {} failed to load, and a retry would load it again - \
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
        let reason = stop_reason(&failed);
        assert!(reason.contains("a (exit code -1073740791), b failed to load"), "{reason}");
        assert!(reason.contains("logs/llama-server.log"));
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
