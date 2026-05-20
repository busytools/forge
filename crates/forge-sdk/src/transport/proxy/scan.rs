//! Defensive scanner for unrewritten classification signals.
//!
//! Walks a parsed JSON value and logs warnings (or pushes findings
//! into a collector) for any value that looks like a leaked
//! classification signal. The intent is drift detection: when a new
//! CLI version introduces a signal channel we haven't ported a
//! rewrite for, this surfaces it loudly during normal operation
//! rather than as a silent billing surprise.

use serde_json::Value;
use tracing::warn;

/// A single finding from the defensive scan. `json_path` is a
/// dot-and-bracket path relative to the scanned root
/// (`.events[0].event_data.entrypoint`). `url_path` is the request
/// path that owned the body (`/api/event_logging/v2/batch`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub url_path: String,
    pub json_path: String,
    pub kind: FindingKind,
    pub value: String,
}

/// Categories of finding the scanner recognises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindingKind {
    /// A string value starting with `sdk-` (likely an entrypoint label).
    SdkPrefixedValue,
    /// `entrypoint` field with a non-`cli` value.
    NonCliEntrypoint,
    /// `client_type` field with a non-`cli` value.
    NonCliClientType,
    /// `is_interactive` field with a non-true value.
    FalseIsInteractive,
    /// `agent_sdk_version` field present (any value).
    AgentSdkVersionPresent,
}

/// Recursively walks a JSON value, returning all findings.
pub fn scan(value: &Value, url_path: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    walk(value, url_path, "", &mut findings);
    findings
}

/// Scan and log a warning per finding via `tracing::warn`. Returns
/// the same findings the caller might want to count.
pub fn scan_and_warn(value: &Value, url_path: &str) -> Vec<Finding> {
    let findings = scan(value, url_path);
    for f in &findings {
        warn!(
            url_path = %f.url_path,
            json_path = %f.json_path,
            kind = ?f.kind,
            value = %f.value,
            "wire-rewriter: unrewritten classification signal detected"
        );
    }
    findings
}

fn walk(v: &Value, url_path: &str, json_path: &str, out: &mut Vec<Finding>) {
    match v {
        Value::Object(map) => {
            for (k, val) in map {
                let sub_path = if json_path.is_empty() {
                    format!(".{k}")
                } else {
                    format!("{json_path}.{k}")
                };
                inspect_field(k, val, url_path, &sub_path, out);
                walk(val, url_path, &sub_path, out);
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter().enumerate() {
                let sub_path = format!("{json_path}[{i}]");
                walk(item, url_path, &sub_path, out);
            }
        }
        _ => {}
    }
}

fn inspect_field(key: &str, val: &Value, url_path: &str, json_path: &str, out: &mut Vec<Finding>) {
    if let Some(s) = val.as_str()
        && s.starts_with("sdk-")
    {
        out.push(Finding {
            url_path: url_path.to_string(),
            json_path: json_path.to_string(),
            kind: FindingKind::SdkPrefixedValue,
            value: s.to_string(),
        });
    }
    match key {
        "entrypoint" => {
            if let Some(s) = val.as_str()
                && s != "cli"
            {
                out.push(Finding {
                    url_path: url_path.to_string(),
                    json_path: json_path.to_string(),
                    kind: FindingKind::NonCliEntrypoint,
                    value: s.to_string(),
                });
            }
        }
        "client_type" => {
            if let Some(s) = val.as_str()
                && s != "cli"
            {
                out.push(Finding {
                    url_path: url_path.to_string(),
                    json_path: json_path.to_string(),
                    kind: FindingKind::NonCliClientType,
                    value: s.to_string(),
                });
            }
        }
        "is_interactive" if val != &Value::Bool(true) => {
            out.push(Finding {
                url_path: url_path.to_string(),
                json_path: json_path.to_string(),
                kind: FindingKind::FalseIsInteractive,
                value: val.to_string(),
            });
        }
        "agent_sdk_version" => {
            out.push(Finding {
                url_path: url_path.to_string(),
                json_path: json_path.to_string(),
                kind: FindingKind::AgentSdkVersionPresent,
                value: val.to_string(),
            });
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_body_has_no_findings() {
        let v = json!({
            "events": [{
                "event_data": {
                    "entrypoint": "cli",
                    "client_type": "cli",
                    "is_interactive": true
                }
            }]
        });
        assert!(scan(&v, "/test").is_empty());
    }

    #[test]
    fn detects_sdk_prefixed_value_anywhere() {
        let v = json!({ "nested": { "deep": "sdk-rs" } });
        let findings = scan(&v, "/anywhere");
        assert!(findings.iter().any(|f| matches!(f.kind, FindingKind::SdkPrefixedValue) && f.value == "sdk-rs"));
    }

    #[test]
    fn detects_all_four_classification_fields() {
        let v = json!({
            "entrypoint": "sdk-cli",
            "client_type": "sdk-cli",
            "is_interactive": false,
            "agent_sdk_version": "0.15.1"
        });
        let findings = scan(&v, "/test");
        let kinds: Vec<_> = findings.iter().map(|f| f.kind.clone()).collect();
        assert!(kinds.contains(&FindingKind::NonCliEntrypoint));
        assert!(kinds.contains(&FindingKind::NonCliClientType));
        assert!(kinds.contains(&FindingKind::FalseIsInteractive));
        assert!(kinds.contains(&FindingKind::AgentSdkVersionPresent));
    }

    #[test]
    fn json_path_includes_arrays_and_objects() {
        let v = json!({ "events": [{ "event_data": { "entrypoint": "sdk-cli" } }] });
        let findings = scan(&v, "/api/event_logging/v2/batch");
        let ep = findings
            .iter()
            .find(|f| matches!(f.kind, FindingKind::NonCliEntrypoint))
            .expect("entrypoint finding");
        assert_eq!(ep.json_path, ".events[0].event_data.entrypoint");
        assert_eq!(ep.url_path, "/api/event_logging/v2/batch");
    }

    #[test]
    fn cli_value_is_not_flagged() {
        let v = json!({ "entrypoint": "cli", "client_type": "cli", "is_interactive": true });
        assert!(scan(&v, "/test").is_empty());
    }
}
