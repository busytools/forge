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
/// request URI - this function operates on the raw query string only.
pub fn rewrite_bootstrap_query(query: &str) -> Option<String> {
    if !query.contains("entrypoint=") {
        return None;
    }
    let parsed: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let needs_rewrite = parsed.iter().any(|(k, v)| k == "entrypoint" && v != "cli");
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
pub fn rewrite_user_agent(ua: &str) -> Option<String> {
    let start = ua.find('(')?;
    let end = ua.find(')')?;
    if end <= start {
        return None;
    }
    let prefix = &ua[..start];
    let inside = &ua[start + 1..end];
    let suffix = &ua[end + 1..];
    let new_inside = if inside.starts_with("external") { "external, cli" } else { "cli" };
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
///   value isn't `cli` (or isn't a string at all - paranoid).
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

/// Rewrite an `/api/event_logging/v2/batch` body. Two stages:
///
/// 1. Drop any event whose `event_data.event_name` carries the
///    `tengu_sdk_` prefix. These are forge-internal protocol events
///    (init_handshake, control_roundtrip, result, ttft) that native
///    interactive `claude` never emits; their presence is itself a
///    classification leak via nomenclature.
/// 2. Apply the recursive classification normaliser to the
///    remaining events, then the defensive byte-level pass.
pub fn rewrite_event_logging(body: &Bytes) -> Bytes {
    let stripped = strip_sdk_events(body);
    rewrite_body_recursive(&stripped)
}

/// Catch-all rewriter for Anthropic endpoints without a per-path
/// specialisation: parse, recursive-normalise, byte-finalise. Used
/// from the proxy's request handler so a new Anthropic surface gets
/// covered without code changes here.
pub fn rewrite_anthropic_unknown(body: &Bytes) -> Bytes {
    rewrite_body_recursive(body)
}

/// Rewrite a Statsig (`/api/eval/sdk-...`) feature evaluation body.
/// Recursive normaliser handles `attributes.entrypoint` along with
/// any other classification fields Anthropic adds to the payload.
pub fn rewrite_statsig_features(body: &Bytes) -> Bytes {
    rewrite_body_recursive(body)
}

/// Rewrite a Datadog `/api/v2/logs` ingest body. Datadog encodes
/// classification two ways: in the JSON body keys (handled by the
/// recursive normaliser) AND in the `ddtags` comma-joined string
/// (handled by `rewrite_ddtags`).
pub fn rewrite_datadog_logs(body: &Bytes) -> Bytes {
    // First pass: drop any log entry tagged with a forge SDK protocol
    // event name (`_sdk_` substring in ddtags or message). Then run
    // classification normalisation on the remaining entries.
    let stripped = strip_sdk_datadog_entries(body);
    let Ok(mut v) = serde_json::from_slice::<Value>(&stripped) else {
        return finalize_string_pass(stripped);
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
    // If the stripped body got further normalised, re-serialise; else
    // pass `stripped` through (it already reflects any _sdk_ entries
    // we dropped).
    let serialised = if changed {
        serde_json::to_vec(&v).map_or_else(|_| stripped.clone(), Bytes::from)
    } else {
        stripped
    };
    finalize_string_pass(serialised)
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
pub fn rewrite_messages_body(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return finalize_string_pass(body.clone());
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

    let serialised = if changed {
        serde_json::to_vec(&v).map_or_else(|_| body.clone(), Bytes::from)
    } else {
        body.clone()
    };
    finalize_string_pass(serialised)
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

/// Forge-only `anthropic-beta` flags that native interactive `claude`
/// sessions don't request. Stripping these from the comma-joined
/// header value before forwarding makes the request's feature
/// fingerprint match a native session.
///
/// Conservative: only removes flags confirmed forge-specific. Does
/// NOT add native-only flags forge isn't requesting (those may
/// require feature support forge doesn't have - adding them blindly
/// risks server-side errors).
///
/// As of native CLI 2.1.153 (#262 audit run), every previously
/// forge-only flag is now native-emitted in at least one of native's
/// per-call variants (`claude-code-prefix` variant carries
/// `claude-code-20250219` + `extended-cache-ttl-2025-04-11` +
/// `advanced-tool-use-2025-11-20` + `effort-2025-11-24` +
/// `afk-mode-2026-01-31`). Stripping them now makes forge look LESS
/// like native, not more. List kept empty as a hook for future CLI
/// versions that re-introduce strippable forge-only flags - the
/// `rewrite_anthropic_beta` consumer site stays wired so a future
/// drift surfaces here.
const FORGE_ONLY_ANTHROPIC_BETAS: &[&str] = &[];

/// `anthropic-beta` flags native interactive `claude` sends in EVERY
/// variant (long, claude-code-prefix, short - see #262 issue body for
/// the captured native sets). If forge's outgoing header doesn't
/// already carry one of these, the rewriter injects it so the wire
/// shape matches native.
///
/// Injection order matters for byte-equivalence with native's header.
/// Per #262's enumerated native variants, the canonical position of
/// `redact-thinking-2026-02-12` is immediately after
/// `interleaved-thinking-2025-05-14`. The rewriter probes for
/// `interleaved-thinking-2025-05-14` in the existing flags and
/// inserts after it; if absent (an unusual header shape) the flag
/// appends at the end.
const NATIVE_REQUIRED_ANTHROPIC_BETAS: &[&str] = &["redact-thinking-2026-02-12"];

/// Canonical anchor flag the injection logic uses to place
/// `NATIVE_REQUIRED_ANTHROPIC_BETAS` entries in the position native
/// emits them. `redact-thinking-2026-02-12` sits immediately after
/// this anchor in every native variant captured for #262.
const ANTHROPIC_BETA_INJECT_ANCHOR: &str = "interleaved-thinking-2025-05-14";

/// Request path on which native interactive `claude` emits the
/// `NATIVE_REQUIRED_ANTHROPIC_BETAS` flags. The injection step in
/// [`rewrite_anthropic_beta`] only fires for requests whose path
/// matches this; non-`/v1/messages` paths (GET /v1/mcp_servers,
/// POST /api/eval/sdk-*, POST /api/event_logging/v2/batch) pass
/// through unmodified so forge doesn't OVER-inject vs native
/// (#266 - the original PR #265 inject was unconditional and
/// flagged native-vs-forge drift on three sibling endpoints).
const ANTHROPIC_BETA_INJECT_PATH: &str = "/v1/messages";

/// Rewrite an `anthropic-beta` header value to match native CLI's
/// per-call shape. Two transforms:
///
/// 1. Strip every flag in `FORGE_ONLY_ANTHROPIC_BETAS` from the
///    comma-joined list (currently empty post-CLI-2.1.153; see the
///    const's doc-comment for why). Applies on every Anthropic path.
/// 2. When `path == ANTHROPIC_BETA_INJECT_PATH` (`/v1/messages`),
///    inject every flag in `NATIVE_REQUIRED_ANTHROPIC_BETAS` that
///    isn't already present, ordered against
///    `ANTHROPIC_BETA_INJECT_ANCHOR`. Non-matching paths skip the
///    inject step so forge doesn't emit flags native doesn't.
///
/// Returns `None` when neither transform changed the set (header
/// passes through unchanged).
pub fn rewrite_anthropic_beta(header_value: &str, path: &str) -> Option<String> {
    let parts: Vec<&str> = header_value.split(',').map(str::trim).collect();
    let mut kept: Vec<String> = Vec::with_capacity(parts.len());
    let mut stripped = 0usize;
    for part in parts {
        if FORGE_ONLY_ANTHROPIC_BETAS.contains(&part) {
            stripped += 1;
        } else if !part.is_empty() {
            kept.push(part.to_owned());
        }
    }
    let mut injected = 0usize;
    if path == ANTHROPIC_BETA_INJECT_PATH {
        for flag in NATIVE_REQUIRED_ANTHROPIC_BETAS {
            if kept.iter().any(|p| p == flag) {
                continue;
            }
            // Insert immediately after the canonical anchor for byte-
            // equivalent ordering with native's header. If the anchor
            // isn't present, append at the end - this is unusual (every
            // native variant contains the anchor) but the append keeps
            // the rewriter robust to header-shape surprises rather than
            // dropping the required flag.
            if let Some(pos) = kept.iter().position(|p| p == ANTHROPIC_BETA_INJECT_ANCHOR) {
                kept.insert(pos + 1, (*flag).to_owned());
            } else {
                kept.push((*flag).to_owned());
            }
            injected += 1;
        }
    }
    if stripped == 0 && injected == 0 {
        return None;
    }
    Some(kept.join(","))
}

/// Telemetry event names native interactive `claude` never emits;
/// forge-sdk's protocol bookkeeping produces these. The
/// `tengu_sdk_` prefix on the name is itself a classification
/// signal, independent of field values inside the event.
///
/// Defensive coverage: any event whose name contains `tengu_sdk_`
/// is dropped, not just the four confirmed names - catches future
/// SDK-protocol events the team adds that follow the same naming
/// pattern. `tengu_sdk_` is specific enough to NOT collide with
/// the legitimate `agent_sdk_version` field name (which the
/// recursive walker / ddtag rewriter handle separately).
const SDK_EVENT_NAME_SUBSTRING: &str = "tengu_sdk_";

/// Filter forge-internal SDK protocol events out of an
/// `/api/event_logging/v2/batch` body. Walks `events[]`, drops any
/// entry whose `event_data.event_name` starts with `tengu_sdk_`
/// (forge's `init_handshake`, `control_roundtrip`, `result`, `ttft`
/// among others). Forge still records these via its local tracing
/// layer; only the outbound telemetry copy is suppressed.
pub fn strip_sdk_events(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let mut dropped = 0usize;
    if let Some(events) = v.get_mut("events").and_then(|e| e.as_array_mut()) {
        let before = events.len();
        events.retain(|ev| {
            let name = ev
                .get("event_data")
                .and_then(|d| d.get("event_name"))
                .and_then(|n| n.as_str())
                .unwrap_or("");
            !name.contains(SDK_EVENT_NAME_SUBSTRING)
        });
        dropped = before.saturating_sub(events.len());
    }
    if dropped == 0 {
        return body.clone();
    }
    serde_json::to_vec(&v).map_or_else(|_| body.clone(), Bytes::from)
}

/// Filter `_sdk_`-tagged log entries out of a Datadog
/// `/api/v2/logs` ingest body. Datadog entries embed the forge SDK
/// protocol event name in two places: the `ddtags` comma-joined
/// string (`event_name:tengu_sdk_init_handshake,...`) and the
/// free-text `message` field. Either occurrence is a wire-leak
/// since native interactive `claude` never emits these events.
///
/// Drops any log entry whose `ddtags` or `message` contains the
/// `_sdk_` substring. The remaining entries pass through to the
/// classification normaliser.
pub fn strip_sdk_datadog_entries(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return body.clone();
    };
    let mut dropped = 0usize;
    if let Some(entries) = v.as_array_mut() {
        let before = entries.len();
        entries.retain(|ev| {
            let in_tags = ev
                .get("ddtags")
                .and_then(|t| t.as_str())
                .is_some_and(|s| s.contains(SDK_EVENT_NAME_SUBSTRING));
            let in_msg = ev
                .get("message")
                .and_then(|m| m.as_str())
                .is_some_and(|s| s.contains(SDK_EVENT_NAME_SUBSTRING));
            !(in_tags || in_msg)
        });
        dropped = before.saturating_sub(entries.len());
    }
    if dropped == 0 {
        return body.clone();
    }
    serde_json::to_vec(&v).map_or_else(|_| body.clone(), Bytes::from)
}

/// Generic body rewriter: parse, normalise classification fields
/// recursively, serialise back, then apply a defensive byte-level
/// substring pass for any classification leaks the structured walker
/// missed (typically values where the CLI nested a stringified-JSON
/// blob inside an outer JSON string - the walker treats the outer
/// string as opaque and doesn't descend). Used by every Anthropic
/// body endpoint we touch.
fn rewrite_body_recursive(body: &Bytes) -> Bytes {
    let Ok(mut v) = serde_json::from_slice::<Value>(body) else {
        return finalize_string_pass(body.clone());
    };
    let structured_changed = normalize_classification_fields(&mut v);
    let serialised = if structured_changed {
        serde_json::to_vec(&v).map_or_else(|_| body.clone(), Bytes::from)
    } else {
        body.clone()
    };
    finalize_string_pass(serialised)
}

/// Final byte-level substring pass over a serialised body. Catches
/// classification leaks the structured walker can't reach (escaped
/// stringified-JSON, future schema additions that nest the value
/// deeper). Gated on a single `memmem` scan for the `sdk-` substring:
/// if the body contains no `sdk-` anywhere, it returns the original
/// `Bytes` without allocating.
/// Escaped and unescaped JSON-key forms that need their `sdk-X` value
/// substring rewritten to `cli`. Used by [`finalize_string_pass`].
const FINALIZE_NEEDLES: &[(&str, &str)] = &[
    (r#""entrypoint":"sdk-cli""#, r#""entrypoint":"cli""#),
    (r#""entrypoint":"sdk-py""#, r#""entrypoint":"cli""#),
    (r#""entrypoint":"sdk-ts""#, r#""entrypoint":"cli""#),
    (r#""entrypoint":"sdk-rs""#, r#""entrypoint":"cli""#),
    (r#""client_type":"sdk-cli""#, r#""client_type":"cli""#),
    (r#""client_type":"sdk-py""#, r#""client_type":"cli""#),
    (r#""client_type":"sdk-ts""#, r#""client_type":"cli""#),
    (r#""client_type":"sdk-rs""#, r#""client_type":"cli""#),
    (r#"\"entrypoint\":\"sdk-cli\""#, r#"\"entrypoint\":\"cli\""#),
    (r#"\"entrypoint\":\"sdk-py\""#, r#"\"entrypoint\":\"cli\""#),
    (r#"\"entrypoint\":\"sdk-ts\""#, r#"\"entrypoint\":\"cli\""#),
    (r#"\"entrypoint\":\"sdk-rs\""#, r#"\"entrypoint\":\"cli\""#),
    (r#"\"client_type\":\"sdk-cli\""#, r#"\"client_type\":\"cli\""#),
    (r#"\"client_type\":\"sdk-py\""#, r#"\"client_type\":\"cli\""#),
    (r#"\"client_type\":\"sdk-ts\""#, r#"\"client_type\":\"cli\""#),
    (r#"\"client_type\":\"sdk-rs\""#, r#"\"client_type\":\"cli\""#),
];

pub fn finalize_string_pass(body: Bytes) -> Bytes {
    if memchr::memmem::find(&body, b"sdk-").is_none() {
        return body;
    }
    let Ok(mut s) = String::from_utf8(body.to_vec()) else {
        // Only reached after a positive needle hit: a leak is
        // escaping in a body we can't rewrite.
        tracing::warn!(
            len = body.len(),
            "finalize pass skipped: classification needle in non-UTF-8 body",
        );
        return body;
    };
    for (needle, replacement) in FINALIZE_NEEDLES {
        if s.contains(needle) {
            s = s.replace(needle, replacement);
        }
    }
    Bytes::from(s.into_bytes())
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
        assert!(
            ed.get("agent_sdk_version").is_none(),
            "agent_sdk_version must be removed, not blanked"
        );
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
        assert!(
            !normalize_classification_fields(&mut v),
            "clean body should not be marked as changed"
        );
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
    fn event_logging_rewrites_stringified_user_attributes() {
        // `user_attributes` is a JSON string (not a nested object),
        // so the structured walker doesn't descend. The byte-level
        // finalize pass must catch the
        // escaped `\"entrypoint\":\"sdk-cli\"` form inside it.
        let body = serde_json::to_vec(&json!({
            "events": [{
                "event_data": {
                    "user_attributes": "{\"version\":\"2.1.133\",\"entrypoint\":\"sdk-cli\",\"client_type\":\"sdk-cli\"}"
                }
            }]
        }))
        .expect("encode");
        let out = rewrite_event_logging(&Bytes::from(body));
        let body_str = std::str::from_utf8(&out).expect("utf8");
        assert!(!body_str.contains("sdk-cli"), "stringified sdk-cli not caught: {body_str}");
        assert!(
            body_str.contains(r#"\"entrypoint\":\"cli\""#),
            "expected escaped cli substring: {body_str}"
        );
        assert!(
            body_str.contains(r#"\"client_type\":\"cli\""#),
            "expected escaped client_type cli substring: {body_str}"
        );
    }

    #[test]
    fn finalize_string_pass_handles_unescaped_and_escaped_forms() {
        // Unescaped: bare JSON value containing sdk-cli
        let raw = Bytes::from_static(br#"{"entrypoint":"sdk-cli","other":"unrelated string"}"#);
        let out = finalize_string_pass(raw);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains(r#""entrypoint":"cli""#));
        assert!(!s.contains("sdk-cli"));

        // Escaped: stringified JSON inside an outer JSON string
        let raw = Bytes::from_static(br#"{"wrapper":"{\"entrypoint\":\"sdk-cli\",\"x\":1}"}"#);
        let out = finalize_string_pass(raw);
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains(r#"\"entrypoint\":\"cli\""#));
        assert!(!s.contains("sdk-cli"));
    }

    /// Path on which native interactive `claude` emits the inject set.
    /// Tests that want to assert inject behavior use this; tests that
    /// want to assert skip behavior use a sibling path.
    const MESSAGES_PATH: &str = "/v1/messages";

    #[test]
    fn anthropic_beta_passes_through_native_2_1_153_long_variant_after_redact_inject() {
        // Native CLI 2.1.153 "long" variant from #262's enumerated
        // capture, minus `redact-thinking-2026-02-12` (the flag
        // forge's outgoing header is missing). The rewriter should
        // inject `redact-thinking-2026-02-12` immediately after
        // `interleaved-thinking-2025-05-14` and pass everything else
        // through unchanged.
        let input = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,cache-diagnosis-2026-04-07";
        let out = rewrite_anthropic_beta(input, MESSAGES_PATH)
            .expect("should rewrite (inject redact-thinking)");
        assert_eq!(
            out,
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,cache-diagnosis-2026-04-07",
        );
    }

    #[test]
    fn anthropic_beta_post_cli_2_1_153_no_longer_strips_previously_forge_only_flags() {
        // Pre-2.1.153 forge-only flags (claude-code-20250219,
        // extended-cache-ttl-2025-04-11, advanced-tool-use-2025-11-20,
        // effort-2025-11-24, afk-mode-2026-01-31) are now native-emitted
        // in the claude-code-prefix variant per #262. The rewriter must
        // NOT strip them anymore - stripping would make forge look LESS
        // like native, not more.
        let input = "claude-code-20250219,extended-cache-ttl-2025-04-11,advanced-tool-use-2025-11-20,effort-2025-11-24,afk-mode-2026-01-31";
        // Rewrite either returns None (pass-through) OR returns a
        // string that preserves every one of these flags. Both shapes
        // are acceptable here since `redact-thinking-2026-02-12`
        // injection only fires when the anchor flag is present.
        let out = rewrite_anthropic_beta(input, MESSAGES_PATH);
        match out {
            None => {}
            Some(rewritten) => {
                assert!(rewritten.contains("claude-code-20250219"), "got: {rewritten}");
                assert!(rewritten.contains("extended-cache-ttl-2025-04-11"), "got: {rewritten}");
                assert!(rewritten.contains("advanced-tool-use-2025-11-20"), "got: {rewritten}");
                assert!(rewritten.contains("effort-2025-11-24"), "got: {rewritten}");
                assert!(rewritten.contains("afk-mode-2026-01-31"), "got: {rewritten}");
            }
        }
    }

    #[test]
    fn anthropic_beta_no_op_when_native_set_already_complete() {
        // Native already carries `redact-thinking-2026-02-12` (the
        // captured native "long" variant from #262). Nothing to
        // strip, nothing to inject - rewriter returns None.
        let input = "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,cache-diagnosis-2026-04-07";
        assert!(rewrite_anthropic_beta(input, MESSAGES_PATH).is_none());
    }

    #[test]
    fn anthropic_beta_injects_redact_thinking_in_canonical_position() {
        // Inject-only path with no strip: forge sends a minimal set
        // that includes the anchor but is missing the required flag.
        // The new flag must land immediately after the anchor.
        let input =
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13";
        let out = rewrite_anthropic_beta(input, MESSAGES_PATH).expect("should rewrite (inject)");
        assert_eq!(
            out,
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,thinking-token-count-2026-05-13",
        );
    }

    #[test]
    fn anthropic_beta_inject_falls_back_to_append_when_anchor_absent() {
        // Defensive: if the anchor `interleaved-thinking-2025-05-14`
        // isn't in the header (unusual - every native variant has it),
        // the rewriter still injects the required flag, just at the
        // end. Keeps the rewriter robust to header-shape surprises.
        let input = "oauth-2025-04-20,context-management-2025-06-27";
        let out =
            rewrite_anthropic_beta(input, MESSAGES_PATH).expect("should rewrite (append-inject)");
        assert_eq!(
            out,
            "oauth-2025-04-20,context-management-2025-06-27,redact-thinking-2026-02-12"
        );
    }

    #[test]
    fn anthropic_beta_trims_whitespace_and_drops_empty() {
        // Whitespace + empties get normalised; injection still
        // applies to the cleaned-up set.
        let input = " oauth-2025-04-20 , interleaved-thinking-2025-05-14 , , ";
        let out = rewrite_anthropic_beta(input, MESSAGES_PATH).expect("should rewrite");
        assert_eq!(
            out,
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12",
        );
    }

    // ----------------------------------------------------------------
    // #266: inject step gated on `path == "/v1/messages"`. Native CLI
    // only emits the NATIVE_REQUIRED set on that endpoint, so forge
    // must NOT inject on sibling endpoints (mcp_servers, eval/sdk-*,
    // event_logging/v2/batch) or it over-emits vs native.
    // ----------------------------------------------------------------

    #[test]
    fn anthropic_beta_inject_skipped_for_mcp_servers_path() {
        // /v1/mcp_servers carries its own beta flag(s) but does NOT
        // emit redact-thinking-2026-02-12 from native. Forge must not
        // inject it here.
        let input = "mcp-servers-2025-12-04,interleaved-thinking-2025-05-14";
        // Native's set on this path is whatever it sends; the rewriter
        // must pass through unchanged. None means "no transform applied".
        let out = rewrite_anthropic_beta(input, "/v1/mcp_servers");
        assert!(out.is_none(), "non-messages paths must pass through unchanged; got: {out:?}");
    }

    #[test]
    fn anthropic_beta_inject_skipped_for_eval_sdk_path() {
        // /api/eval/sdk-<id> is a Statsig-style feature evaluation
        // probe. Path carries a variable session-id suffix; the
        // injection gate must NOT match because the predicate is
        // exact-equal against `/v1/messages`, not a starts_with.
        let input = "oauth-2025-04-20,interleaved-thinking-2025-05-14";
        let out = rewrite_anthropic_beta(input, "/api/eval/sdk-zAZxyz123");
        assert!(out.is_none(), "eval/sdk-* paths must pass through; got: {out:?}");
    }

    #[test]
    fn anthropic_beta_inject_skipped_for_event_logging_path() {
        // /api/event_logging/v2/batch carries telemetry, not chat
        // turns. Forge must not inject /v1/messages-specific flags
        // here.
        let input = "oauth-2025-04-20,interleaved-thinking-2025-05-14";
        let out = rewrite_anthropic_beta(input, "/api/event_logging/v2/batch");
        assert!(out.is_none(), "event_logging paths must pass through; got: {out:?}");
    }

    #[test]
    fn anthropic_beta_inject_applied_only_on_exact_messages_path() {
        // Sanity check on the exact-equal predicate: a path that
        // STARTS WITH /v1/messages but has a suffix (e.g. an unlikely
        // future /v1/messages/stream) must NOT trigger inject under
        // the exact predicate. Defensive against accidental
        // starts_with relaxation in a future refactor.
        let input = "oauth-2025-04-20,interleaved-thinking-2025-05-14";
        let out = rewrite_anthropic_beta(input, "/v1/messages/foo");
        assert!(out.is_none(), "non-exact /v1/messages paths must pass through; got: {out:?}");

        // And the canonical positive case still injects.
        let out_pos =
            rewrite_anthropic_beta(input, "/v1/messages").expect("exact /v1/messages must inject");
        assert!(out_pos.contains("redact-thinking-2026-02-12"));
    }

    #[test]
    fn strip_sdk_events_drops_entries_by_nested_event_name() {
        // Real shape from forge captures: event_name lives nested
        // inside event_data, not as a top-level event field.
        let body = serde_json::to_vec(&json!({
            "events": [
                { "event_data": { "event_name": "tengu_sdk_init_handshake" } },
                { "event_data": { "event_name": "tengu_api_request", "entrypoint": "cli" } },
                { "event_data": { "event_name": "tengu_sdk_control_roundtrip" } },
                { "event_data": { "event_name": "tengu_tool_use" } },
                { "event_data": { "event_name": "tengu_sdk_result" } },
                { "event_data": { "event_name": "tengu_sdk_ttft" } }
            ]
        }))
        .expect("encode");
        let out = strip_sdk_events(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        let names: Vec<&str> = parsed["events"]
            .as_array()
            .expect("array")
            .iter()
            .map(|e| e["event_data"]["event_name"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(names, vec!["tengu_api_request", "tengu_tool_use"]);
    }

    #[test]
    fn strip_sdk_events_noop_when_no_sdk_events() {
        let body = serde_json::to_vec(&json!({
            "events": [
                { "event_data": { "event_name": "tengu_api_request" } },
                { "event_data": { "event_name": "tengu_tool_use" } }
            ]
        }))
        .expect("encode");
        let out = strip_sdk_events(&Bytes::from(body.clone()));
        assert_eq!(out.as_ref(), body.as_slice());
    }

    #[test]
    fn event_logging_drops_sdk_events_then_normalises_classification() {
        // End-to-end: a batch with both an _sdk_ event AND a
        // classification leak on a kept event. Verify both get
        // handled correctly in one pass.
        let body = serde_json::to_vec(&json!({
            "events": [
                { "event_data": { "event_name": "tengu_sdk_init_handshake", "entrypoint": "sdk-cli" } },
                { "event_data": { "event_name": "tengu_api_request", "entrypoint": "sdk-cli", "client_type": "sdk-cli", "is_interactive": false } }
            ]
        }))
        .expect("encode");
        let out = rewrite_event_logging(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        let events = parsed["events"].as_array().expect("array");
        assert_eq!(events.len(), 1, "tengu_sdk_init_handshake should be dropped");
        assert_eq!(events[0]["event_data"]["event_name"], "tengu_api_request");
        assert_eq!(events[0]["event_data"]["entrypoint"], "cli");
        assert_eq!(events[0]["event_data"]["client_type"], "cli");
        assert_eq!(events[0]["event_data"]["is_interactive"], true);
    }

    #[test]
    fn strip_sdk_datadog_entries_drops_entries_with_sdk_in_tags_or_message() {
        // _sdk_ can appear in both ddtags and message
        // fields of Datadog entries. Either occurrence drops the entry.
        let body = serde_json::to_vec(&json!([
            { "ddtags": "event_name:tengu_sdk_init_handshake,version:2.1.133", "message": "fine" },
            { "ddtags": "event_name:tengu_api_request,version:2.1.133", "message": "fine" },
            { "ddtags": "version:2.1.133", "message": "doing tengu_sdk_control_roundtrip work" },
            { "ddtags": "version:2.1.133", "message": "normal log line" }
        ]))
        .expect("encode");
        let out = strip_sdk_datadog_entries(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        let entries = parsed.as_array().expect("array");
        assert_eq!(entries.len(), 2, "two _sdk_-tagged entries should be dropped: {entries:?}");
        assert_eq!(entries[0]["ddtags"], "event_name:tengu_api_request,version:2.1.133");
        assert_eq!(entries[1]["message"], "normal log line");
    }

    #[test]
    fn strip_sdk_datadog_entries_noop_when_no_sdk_substring() {
        let body = serde_json::to_vec(&json!([
            { "ddtags": "event_name:tengu_api_request", "message": "all clean" }
        ]))
        .expect("encode");
        let out = strip_sdk_datadog_entries(&Bytes::from(body.clone()));
        assert_eq!(out.as_ref(), body.as_slice());
    }

    #[test]
    fn datadog_logs_strips_sdk_entries_and_normalises_remaining() {
        // End-to-end: datadog body with an _sdk_ entry alongside a
        // classification-leak entry. Both surfaces handled.
        let body = serde_json::to_vec(&json!([
            { "ddtags": "event_name:tengu_sdk_init_handshake,entrypoint:sdk-cli", "message": "sdk init" },
            { "ddtags": "entrypoint:sdk-cli,client_type:sdk-cli,is_interactive:false", "message": "normal", "entrypoint": "sdk-cli" }
        ]))
        .expect("encode");
        let out = rewrite_datadog_logs(&Bytes::from(body));
        let parsed: Value = serde_json::from_slice(&out).expect("decode");
        let entries = parsed.as_array().expect("array");
        assert_eq!(entries.len(), 1, "_sdk_ entry should be dropped: {entries:?}");
        let tags = entries[0]["ddtags"].as_str().expect("string");
        assert!(tags.contains("entrypoint:cli"));
        assert!(tags.contains("client_type:cli"));
        assert!(tags.contains("is_interactive:true"));
        assert_eq!(entries[0]["entrypoint"], "cli");
    }

    #[test]
    fn finalize_string_pass_noop_on_clean_input() {
        let raw = Bytes::from_static(br#"{"entrypoint":"cli","field":"value"}"#);
        let out = finalize_string_pass(raw.clone());
        assert_eq!(out.as_ref(), raw.as_ref());
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
        assert!(
            !tags.contains("agent_sdk_version"),
            "agent_sdk_version ddtag must be dropped: {tags}"
        );
    }
}
