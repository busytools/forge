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

/// Recursively walk a JSON [`Value`] in place, normalising every
/// classification field encountered (at any nesting depth). The brief
/// documents the channels where these fields live; in practice
/// telemetry payloads have nested wrappers and Anthropic occasionally
/// adds new ones. Walking blindly catches drift the per-path rewrites
/// would miss, and the cost is one tree traversal per body.
///
/// Per-key behaviour:
/// - `entrypoint` / `client_type` → set to `cli` when current string
///   value isn't `cli` (or isn't a string at all — paranoid).
/// - `is_interactive` → set to JSON `true` when not already `true`.
/// - `agent_sdk_version` → key removed entirely (presence itself is a
///   signal; blanking the value is insufficient).
///
/// Returns `true` if any change was made.
pub fn normalize_classification_fields(value: &mut Value) -> bool {
    let mut changed = false;
    walk_normalize(value, &mut changed);
    changed
}

fn walk_normalize(v: &mut Value, changed: &mut bool) {
    match v {
        Value::Object(map) => {
            if map.shift_remove("agent_sdk_version").is_some() {
                *changed = true;
            }
            for (k, val) in map.iter_mut() {
                match k.as_str() {
                    // Only rewrite when the existing value is a string
                    // that isn't already "cli". Non-string values are
                    // left intact rather than silently overwritten.
                    "entrypoint" | "client_type" => {
                        if let Some(s) = val.as_str()
                            && s != "cli"
                        {
                            *val = Value::String("cli".into());
                            *changed = true;
                        }
                    }
                    // For is_interactive we'll coerce any non-true value
                    // (false, null, even non-bool surprises) to true,
                    // since the field is unambiguously boolean in shape.
                    "is_interactive" if val != &Value::Bool(true) => {
                        *val = Value::Bool(true);
                        *changed = true;
                    }
                    _ => {}
                }
                walk_normalize(val, changed);
            }
        }
        Value::Array(arr) => {
            for item in arr.iter_mut() {
                walk_normalize(item, changed);
            }
        }
        _ => {}
    }
}

/// Rewrite an `/api/event_logging/v2/batch` body via the recursive
/// normaliser. Thin wrapper kept for call-site readability and
/// because the proxy dispatches on URL path.
#[must_use]
pub fn rewrite_event_logging(body: &Bytes) -> Bytes {
    rewrite_body_recursive(body)
}

/// Rewrite a Statsig (`/api/eval/sdk-...`) feature evaluation body.
/// Recursive normaliser handles `attributes.entrypoint` along with
/// any other classification fields Anthropic adds to the payload.
#[must_use]
pub fn rewrite_statsig_features(body: &Bytes) -> Bytes {
    rewrite_body_recursive(body)
}

/// Rewrite a Datadog `/api/v2/logs` ingest body. Datadog encodes
/// classification two ways: in the JSON body keys (handled by the
/// recursive normaliser) AND in the `ddtags` comma-joined string
/// (handled by [`rewrite_ddtags`]).
#[must_use]
pub fn rewrite_datadog_logs(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let mut changed = normalize_classification_fields(&mut v);
    // ddtags is a comma-joined string outside the normal JSON object
    // shape, so it needs string-level rewriting.
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
        }
    }
    if !changed {
        return body.clone();
    }
    serde_json::to_vec(&v).map_or_else(|_| body.clone(), Bytes::from)
}

/// Rewrite a `/v1/messages` request body.
///
/// Two passes:
/// 1. Recursive classification normalisation across the whole JSON
///    structure (catches any structured `entrypoint`/`client_type`/
///    `is_interactive`/`agent_sdk_version` field at any depth).
/// 2. String-content rewrite of `cc_entrypoint=sdk-*` substrings
///    inside `system[*].text` values. The CLI bakes its self-
///    classified entrypoint into the system prompt as a literal
///    substring on every turn, so the JSON-key walker doesn't catch
///    it. This is the highest-volume leak by request count (one per
///    turn) and the most visible to anyone diffing alt vs native.
#[must_use]
pub fn rewrite_messages_body(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let mut changed = normalize_classification_fields(&mut v);

    if let Some(system) = v.get_mut("system").and_then(|s| s.as_array_mut()) {
        for entry in system {
            let Some(text_str) = entry.get("text").and_then(|t| t.as_str()) else {
                continue;
            };
            let rewritten = rewrite_cc_entrypoint(text_str);
            if rewritten != text_str
                && let Some(obj) = entry.as_object_mut()
            {
                obj.insert("text".into(), Value::String(rewritten));
                changed = true;
            }
        }
    }

    if !changed {
        return body.clone();
    }
    serde_json::to_vec(&v).map_or_else(|_| body.clone(), Bytes::from)
}

/// Replace `cc_entrypoint=sdk-<anything>` with `cc_entrypoint=cli`
/// in arbitrary text. Handles the four known SDK-tier values; a
/// future `sdk-X` would slip through and be caught by the defensive
/// scan's warn-log surface, prompting an extension here.
fn rewrite_cc_entrypoint(s: &str) -> String {
    s.replace("cc_entrypoint=sdk-cli", "cc_entrypoint=cli")
        .replace("cc_entrypoint=sdk-py", "cc_entrypoint=cli")
        .replace("cc_entrypoint=sdk-ts", "cc_entrypoint=cli")
        .replace("cc_entrypoint=sdk-rs", "cc_entrypoint=cli")
}

/// Generic body rewriter: parse, normalise classification fields
/// recursively, serialise back. Used by every Anthropic body
/// endpoint we touch.
fn rewrite_body_recursive(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    if !normalize_classification_fields(&mut v) {
        return body.clone();
    }
    serde_json::to_vec(&v).map_or_else(|_| body.clone(), Bytes::from)
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
            if part.starts_with("agent_sdk_version:") {
                // Drop the tag entirely; presence itself is a signal.
                return String::new();
            }
            part.to_string()
        })
        .filter(|p| !p.is_empty())
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

    #[test]
    fn recursive_normalizer_handles_top_level_fields() {
        let mut v = json!({
            "entrypoint": "sdk-cli",
            "client_type": "sdk-cli",
            "is_interactive": false,
            "agent_sdk_version": "0.15.1"
        });
        assert!(normalize_classification_fields(&mut v));
        assert_eq!(v["entrypoint"], "cli");
        assert_eq!(v["client_type"], "cli");
        assert_eq!(v["is_interactive"], true);
        assert!(v.get("agent_sdk_version").is_none());
    }

    #[test]
    fn recursive_normalizer_handles_deeply_nested_fields() {
        let mut v = json!({
            "a": { "b": { "c": [
                { "entrypoint": "sdk-cli" },
                { "wrapper": { "inner": { "client_type": "sdk-py", "is_interactive": false } } },
                { "agent_sdk_version": "0.15.1" }
            ] } }
        });
        assert!(normalize_classification_fields(&mut v));
        assert_eq!(v["a"]["b"]["c"][0]["entrypoint"], "cli");
        assert_eq!(v["a"]["b"]["c"][1]["wrapper"]["inner"]["client_type"], "cli");
        assert_eq!(v["a"]["b"]["c"][1]["wrapper"]["inner"]["is_interactive"], true);
        assert!(v["a"]["b"]["c"][2].get("agent_sdk_version").is_none());
    }

    #[test]
    fn recursive_normalizer_idempotent_on_clean_body() {
        let mut v = json!({
            "events": [{ "event_data": { "entrypoint": "cli", "client_type": "cli", "is_interactive": true } }]
        });
        assert!(!normalize_classification_fields(&mut v), "clean body should not be marked as changed");
    }

    #[test]
    fn recursive_normalizer_removes_agent_sdk_version_at_any_depth() {
        let mut v = json!({
            "level1": {
                "agent_sdk_version": "0.15.1",
                "level2": {
                    "agent_sdk_version": "0.15.1",
                    "data": "keep"
                }
            },
            "agent_sdk_version": "0.15.1"
        });
        assert!(normalize_classification_fields(&mut v));
        assert!(v.get("agent_sdk_version").is_none());
        assert!(v["level1"].get("agent_sdk_version").is_none());
        assert!(v["level1"]["level2"].get("agent_sdk_version").is_none());
        assert_eq!(v["level1"]["level2"]["data"], "keep");
    }

    #[test]
    fn messages_body_rewrites_cc_entrypoint_in_system_text() {
        let body = serde_json::to_vec(&json!({
            "model": "claude-haiku",
            "system": [
                { "type": "text", "text": "header\n; cc_entrypoint=sdk-cli; cch=abc; trailer" },
                { "type": "text", "text": "no marker here" },
                { "type": "text", "text": "cc_entrypoint=sdk-py with python style" }
            ],
            "messages": []
        }))
        .expect("encode");
        let out = rewrite_messages_body(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        let first = parsed["system"][0]["text"].as_str().expect("string");
        assert!(first.contains("cc_entrypoint=cli"), "first system text: {first}");
        assert!(!first.contains("sdk-cli"));
        assert_eq!(parsed["system"][1]["text"], "no marker here");
        let third = parsed["system"][2]["text"].as_str().expect("string");
        assert!(third.contains("cc_entrypoint=cli"));
        assert!(!third.contains("sdk-py"));
    }

    #[test]
    fn messages_body_normalizes_nested_classification_fields() {
        let body = serde_json::to_vec(&json!({
            "metadata": {
                "user_attributes": { "entrypoint": "sdk-cli" },
                "client": { "client_type": "sdk-cli", "agent_sdk_version": "0.16.0" }
            },
            "system": [{ "type": "text", "text": "no marker" }]
        }))
        .expect("encode");
        let out = rewrite_messages_body(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        assert_eq!(parsed["metadata"]["user_attributes"]["entrypoint"], "cli");
        assert_eq!(parsed["metadata"]["client"]["client_type"], "cli");
        assert!(parsed["metadata"]["client"].get("agent_sdk_version").is_none());
    }

    #[test]
    fn datadog_drops_agent_sdk_version_ddtag() {
        let body = serde_json::to_vec(&json!([{
            "ddtags": "entrypoint:sdk-cli,agent_sdk_version:0.15.1,other:keep"
        }]))
        .expect("encode");
        let out = rewrite_datadog_logs(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        let tags = parsed[0]["ddtags"].as_str().expect("string");
        assert!(tags.contains("entrypoint:cli"));
        assert!(tags.contains("other:keep"));
        assert!(!tags.contains("agent_sdk_version"), "agent_sdk_version ddtag must be dropped: {tags}");
    }
}
