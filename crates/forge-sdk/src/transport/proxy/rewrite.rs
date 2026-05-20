//! Wire-classification rewrite functions.
//!
//! Each function takes bytes from a recognised request and returns
//! bytes with the classification fields normalised to `cli` /
//! `is_interactive=true` / no-agent-sdk-version. These are the
//! mechanics of the 6 signal channels documented in
//! `~/.claude/memory/reference_claude_cli_integration_modes.md`:
//!
//! 1. bootstrap query string (`rewrite_bootstrap_query`)
//! 2. `User-Agent` on `/v1/messages` (`rewrite_user_agent`)
//! 3. `User-Agent` on MCP initialize (same `rewrite_user_agent`)
//! 4. `/api/event_logging/v2/batch` body (`rewrite_event_logging`)
//! 5. `/api/eval/...` Statsig body (`rewrite_statsig_features`)
//! 6. Datadog `ddtags` + body (`rewrite_datadog_logs`)

use bytes::Bytes;
use serde_json::Value;

/// Rewrite the entrypoint param in a bootstrap query string. Returns
/// the new query string if the input contains `entrypoint=` with a
/// value other than `cli`; otherwise returns `None`.
///
/// The proxy is responsible for splicing the result back into the
/// request URI — this function operates on the raw query string only.
#[must_use]
pub fn rewrite_bootstrap_query(query: &str) -> Option<String> {
    if !query.contains("entrypoint=") {
        return None;
    }
    let parsed: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let needs_rewrite =
        parsed.iter().any(|(k, v)| k == "entrypoint" && v != "cli");
    if !needs_rewrite {
        return None;
    }
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in parsed {
        if k == "entrypoint" {
            serializer.append_pair(&k, "cli");
        } else {
            serializer.append_pair(&k, &v);
        }
    }
    Some(serializer.finish())
}

/// Rewrite the User-Agent header value, normalising the parenthesised
/// classification segment to either `(cli)` (when the original lacked
/// the `external,` prefix, as on MCP init) or `(external, cli)` (when
/// it had it, as on `/v1/messages`).
///
/// Returns `None` when no rewrite is needed (no parens, already
/// normalised). Returns the rewritten string otherwise.
#[must_use]
pub fn rewrite_user_agent(ua: &str) -> Option<String> {
    let start = ua.find('(')?;
    let end = ua.find(')')?;
    if end <= start {
        return None;
    }
    let prefix = &ua[..start];
    let inside = &ua[start + 1..end];
    let suffix = &ua[end + 1..];
    let new_inside = if inside.starts_with("external") {
        "external, cli"
    } else {
        "cli"
    };
    if new_inside == inside {
        return None;
    }
    Some(format!("{prefix}({new_inside}){suffix}"))
}

/// Rewrite an `/api/event_logging/v2/batch` body. Walks
/// `events[*].event_data`, normalises `entrypoint`, `client_type`,
/// `is_interactive`, and removes `agent_sdk_version` when present.
///
/// On any JSON parse failure, returns the input unchanged (the proxy
/// is best-effort at this layer; the defensive scan catches drift).
#[must_use]
pub fn rewrite_event_logging(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let mut changed = false;
    if let Some(events) = v.get_mut("events").and_then(|e| e.as_array_mut()) {
        for ev in events {
            if let Some(ed) = ev.get_mut("event_data").and_then(|e| e.as_object_mut()) {
                if ed.contains_key("entrypoint") {
                    ed.insert("entrypoint".into(), Value::String("cli".into()));
                    changed = true;
                }
                if ed.contains_key("client_type") {
                    ed.insert("client_type".into(), Value::String("cli".into()));
                    changed = true;
                }
                if ed.contains_key("is_interactive") {
                    ed.insert("is_interactive".into(), Value::Bool(true));
                    changed = true;
                }
                if ed.remove("agent_sdk_version").is_some() {
                    changed = true;
                }
            }
        }
    }
    if !changed {
        return body.clone();
    }
    match serde_json::to_vec(&v) {
        Ok(buf) => Bytes::from(buf),
        Err(_) => body.clone(),
    }
}

/// Rewrite a Statsig (`/api/eval/sdk-...`) feature evaluation body.
/// Normalises `attributes.entrypoint` to `cli`.
#[must_use]
pub fn rewrite_statsig_features(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let mut changed = false;
    if let Some(attrs) = v.get_mut("attributes").and_then(|a| a.as_object_mut())
        && attrs.contains_key("entrypoint")
    {
        attrs.insert("entrypoint".into(), Value::String("cli".into()));
        changed = true;
    }
    if !changed {
        return body.clone();
    }
    match serde_json::to_vec(&v) {
        Ok(buf) => Bytes::from(buf),
        Err(_) => body.clone(),
    }
}

/// Rewrite a Datadog `/api/v2/logs` ingest body. Datadog encodes
/// fields two ways: in the body keys (`is_interactive`,
/// `agent_sdk_version`) AND in the `ddtags` comma-joined string
/// (`entrypoint:sdk-cli`, `client_type:sdk-cli`). Both surfaces need
/// rewriting.
#[must_use]
pub fn rewrite_datadog_logs(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let mut changed = false;
    if let Some(events) = v.as_array_mut() {
        for ev in events {
            if let Some(tags) = ev.get("ddtags").and_then(|t| t.as_str()) {
                let rewritten = rewrite_ddtags(tags);
                if rewritten != tags
                    && let Some(obj) = ev.as_object_mut()
                {
                    obj.insert("ddtags".into(), Value::String(rewritten));
                    changed = true;
                }
            }
            if let Some(obj) = ev.as_object_mut() {
                if obj.contains_key("is_interactive") {
                    obj.insert("is_interactive".into(), Value::Bool(true));
                    changed = true;
                }
                if obj.remove("agent_sdk_version").is_some() {
                    changed = true;
                }
                if let Some(s) = obj.get("entrypoint").and_then(|v| v.as_str())
                    && s != "cli"
                {
                    obj.insert("entrypoint".into(), Value::String("cli".into()));
                    changed = true;
                }
                if let Some(s) = obj.get("client_type").and_then(|v| v.as_str())
                    && s != "cli"
                {
                    obj.insert("client_type".into(), Value::String("cli".into()));
                    changed = true;
                }
            }
        }
    }
    if !changed {
        return body.clone();
    }
    match serde_json::to_vec(&v) {
        Ok(buf) => Bytes::from(buf),
        Err(_) => body.clone(),
    }
}

fn rewrite_ddtags(tags: &str) -> String {
    tags.split(',')
        .map(|part| {
            if let Some(rest) = part.strip_prefix("entrypoint:")
                && rest != "cli"
            {
                return "entrypoint:cli".to_string();
            }
            if let Some(rest) = part.strip_prefix("client_type:")
                && rest != "cli"
            {
                return "client_type:cli".to_string();
            }
            if let Some(rest) = part.strip_prefix("is_interactive:")
                && rest != "true"
            {
                return "is_interactive:true".to_string();
            }
            part.to_string()
        })
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn user_agent_v1_messages_external_sdk_cli() {
        let input = "claude-cli/2.1.133 (external, sdk-cli, agent-sdk/0.15.1)";
        let out = rewrite_user_agent(input).expect("should rewrite");
        assert_eq!(out, "claude-cli/2.1.133 (external, cli)");
    }

    #[test]
    fn user_agent_mcp_init_no_external_prefix() {
        let input = "claude-code/2.1.133 (sdk-cli, agent-sdk/0.15.1)";
        let out = rewrite_user_agent(input).expect("should rewrite");
        assert_eq!(out, "claude-code/2.1.133 (cli)");
    }

    #[test]
    fn user_agent_already_cli_returns_none() {
        let input = "claude-cli/2.1.133 (external, cli)";
        assert!(rewrite_user_agent(input).is_none(), "no rewrite when already cli");
    }

    #[test]
    fn user_agent_no_parens_returns_none() {
        let input = "claude-cli/2.1.133";
        assert!(rewrite_user_agent(input).is_none());
    }

    #[test]
    fn user_agent_preserves_suffix_after_parens() {
        // Defensive: if Anthropic adds trailing markers after the
        // parens (a future change), preserve them rather than truncating.
        let input = "claude-cli/2.1.133 (external, sdk-cli, agent-sdk/0.15.1) extra-tag";
        let out = rewrite_user_agent(input).expect("should rewrite");
        assert_eq!(out, "claude-cli/2.1.133 (external, cli) extra-tag");
    }

    #[test]
    fn bootstrap_qs_rewrites_sdk_cli_to_cli() {
        let q = "entrypoint=sdk-cli&platform=darwin";
        let out = rewrite_bootstrap_query(q).expect("should rewrite");
        assert!(out.contains("entrypoint=cli"));
        assert!(out.contains("platform=darwin"));
        assert!(!out.contains("sdk-cli"));
    }

    #[test]
    fn bootstrap_qs_already_cli_returns_none() {
        let q = "entrypoint=cli&platform=darwin";
        assert!(rewrite_bootstrap_query(q).is_none());
    }

    #[test]
    fn bootstrap_qs_no_entrypoint_returns_none() {
        let q = "platform=darwin";
        assert!(rewrite_bootstrap_query(q).is_none());
    }

    #[test]
    fn event_logging_rewrites_all_three_fields() {
        let body = serde_json::to_vec(&json!({
            "events": [{
                "event_data": {
                    "entrypoint": "sdk-cli",
                    "client_type": "sdk-cli",
                    "is_interactive": false,
                    "agent_sdk_version": "0.15.1",
                    "other_field": "untouched"
                }
            }]
        }))
        .expect("encode");
        let out = rewrite_event_logging(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        let ed = &parsed["events"][0]["event_data"];
        assert_eq!(ed["entrypoint"], "cli");
        assert_eq!(ed["client_type"], "cli");
        assert_eq!(ed["is_interactive"], true);
        assert!(ed.get("agent_sdk_version").is_none(), "agent_sdk_version must be removed, not blanked");
        assert_eq!(ed["other_field"], "untouched");
    }

    #[test]
    fn event_logging_handles_multiple_events() {
        let body = serde_json::to_vec(&json!({
            "events": [
                { "event_data": { "entrypoint": "sdk-cli" } },
                { "event_data": { "entrypoint": "sdk-py", "agent_sdk_version": "0.15.1" } }
            ]
        }))
        .expect("encode");
        let out = rewrite_event_logging(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        assert_eq!(parsed["events"][0]["event_data"]["entrypoint"], "cli");
        assert_eq!(parsed["events"][1]["event_data"]["entrypoint"], "cli");
        assert!(parsed["events"][1]["event_data"].get("agent_sdk_version").is_none());
    }

    #[test]
    fn event_logging_skips_events_without_event_data() {
        let body = serde_json::to_vec(&json!({
            "events": [{ "other_envelope": {} }]
        }))
        .expect("encode");
        let out = rewrite_event_logging(&Bytes::from(body.clone()));
        // No rewrite needed; output should be byte-identical to input.
        assert_eq!(out.as_ref(), body.as_slice());
    }

    #[test]
    fn statsig_rewrites_attributes_entrypoint() {
        let body = serde_json::to_vec(&json!({
            "user": {},
            "attributes": { "entrypoint": "sdk-cli", "extra": "keep" }
        }))
        .expect("encode");
        let out = rewrite_statsig_features(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        assert_eq!(parsed["attributes"]["entrypoint"], "cli");
        assert_eq!(parsed["attributes"]["extra"], "keep");
    }

    #[test]
    fn datadog_rewrites_ddtags_and_body() {
        let body = serde_json::to_vec(&json!([{
            "ddtags": "entrypoint:sdk-cli,client_type:sdk-cli,is_interactive:false,version:2.1.133",
            "message": "test",
            "entrypoint": "sdk-cli",
            "client_type": "sdk-cli",
            "is_interactive": false,
            "agent_sdk_version": "0.15.1"
        }]))
        .expect("encode");
        let out = rewrite_datadog_logs(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        let ev = &parsed[0];
        let tags = ev["ddtags"].as_str().expect("ddtags is string");
        assert!(tags.contains("entrypoint:cli"));
        assert!(tags.contains("client_type:cli"));
        assert!(tags.contains("is_interactive:true"));
        assert!(tags.contains("version:2.1.133"), "non-classification tags preserved");
        assert!(!tags.contains("sdk-cli"));
        assert_eq!(ev["entrypoint"], "cli");
        assert_eq!(ev["client_type"], "cli");
        assert_eq!(ev["is_interactive"], true);
        assert!(ev.get("agent_sdk_version").is_none());
    }

    #[test]
    fn datadog_empty_array_is_passthrough() {
        let body = serde_json::to_vec(&json!([])).expect("encode");
        let out = rewrite_datadog_logs(&Bytes::from(body.clone()));
        assert_eq!(out.as_ref(), body.as_slice());
    }

    #[test]
    fn invalid_json_falls_through_unchanged() {
        let body = Bytes::from_static(b"not valid json {{{");
        let out_evt = rewrite_event_logging(&body);
        assert_eq!(out_evt.as_ref(), body.as_ref());
        let out_st = rewrite_statsig_features(&body);
        assert_eq!(out_st.as_ref(), body.as_ref());
        let out_dd = rewrite_datadog_logs(&body);
        assert_eq!(out_dd.as_ref(), body.as_ref());
    }
}
