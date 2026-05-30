pub use forge_primitives::TurnErrorClass;

pub fn classify_turn_error(input: &str) -> TurnErrorClass {
    let lower = input.to_ascii_lowercase();
    if looks_like_plan_limit_error_lower(&lower) {
        TurnErrorClass::PlanLimit
    } else if looks_like_auth_required_error_lower(&lower) {
        TurnErrorClass::AuthRequired
    } else if looks_like_internal_error_lower(&lower) {
        TurnErrorClass::Internal
    } else {
        TurnErrorClass::Other
    }
}

pub fn looks_like_internal_error(input: &str) -> bool {
    looks_like_internal_error_lower(&input.to_ascii_lowercase())
}

pub fn summarize_internal_error(input: &str) -> String {
    if let Some(summary) = summarize_permission_schema_error(input) {
        return truncate_for_log(&summary);
    }
    if let Some(msg) = extract_xml_tag_value(input, "message") {
        return truncate_for_log(msg);
    }
    if let Some(msg) = extract_json_string_field(input, "message") {
        return truncate_for_log(&msg);
    }
    // #143 item 1: extended fallback chain. The wrapped error
    // payloads sometimes carry their useful text under a different
    // field name. The previous chain (`<message>` -> `"message"` ->
    // first-non-blank line) returned `""` when the wire shape used
    // any of the variants below.
    //
    // - `assistant_error`  -  wire-shape from the agent SDK adapter for
    //   errors that surface inside an assistant turn body.
    // - `detail` / `description`  -  common JSON shapes for error
    //   responses that don't follow the `"message"` convention
    //   (Anthropic's "rate_limit_error" body uses `message` so it
    //   hits the prior branch; other endpoints differ).
    // - `error.message` / `error.type`  -  nested error objects.
    // - `body`  -  the truncated HTTP body suffix oauth_usage and
    //   similar loggers stuff verbatim onto wrapped error strings.
    //
    // Each extractor returns None when the field isn't present so
    // the chain falls through to the original first-non-blank-line
    // fallback when no structured signal is recoverable.
    for field in ["assistant_error", "detail", "description", "type", "body"] {
        if let Some(msg) = extract_json_string_field(input, field)
            && !msg.trim().is_empty()
        {
            return truncate_for_log(&msg);
        }
    }
    let fallback = input.lines().find(|line| !line.trim().is_empty()).unwrap_or(input);
    truncate_for_log(fallback.trim())
}

fn looks_like_plan_limit_error_lower(lower: &str) -> bool {
    [
        "rate limit",
        "rate-limit",
        "max turns",
        "max turn",
        "max budget",
        "quota",
        "plan limit",
        "plan-limit",
        "429",
        "too many requests",
        "usage limit",
        "insufficient quota",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub fn looks_like_auth_required_error_lower(lower: &str) -> bool {
    let any = [
        "/login",
        "auth required",
        "authentication failed",
        "authentication_failed",
        "authentication required",
        "please log in",
        "login required",
        "not authenticated",
        "unauthenticated",
        "unauthorized",
    ]
    .iter()
    .any(|needle| lower.contains(needle));
    // The 401+auth conjunction catches HTTP-shape errors that don't
    // include any of the literal substrings above (e.g. "request
    // returned 401 from auth gateway").
    any || (lower.contains("401") && lower.contains("auth"))
}

fn looks_like_internal_error_lower(lower: &str) -> bool {
    has_internal_error_keywords(lower)
        || looks_like_json_rpc_error_shape(lower)
        || looks_like_xml_error_shape(lower)
}

fn has_internal_error_keywords(lower: &str) -> bool {
    [
        "internal error",
        "agent sdk",
        "claude-agent-sdk",
        "adapter",
        "bridge",
        "json-rpc",
        "rpc",
        "protocol error",
        "transport",
        "handshake failed",
        "session creation failed",
        "connection closed",
        "event channel closed",
        "tool permission request failed",
        "zoderror",
        "invalid_union",
        "bridge command failed",
        "agent stream failed",
        "agent initialization failed",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

fn looks_like_json_rpc_error_shape(lower: &str) -> bool {
    (lower.contains("\"jsonrpc\"") && lower.contains("\"error\""))
        || lower.contains("\"code\":-32603")
        || lower.contains("\"code\": -32603")
}

fn looks_like_xml_error_shape(lower: &str) -> bool {
    let has_error_node = lower.contains("<error") || lower.contains("<fault");
    let has_detail_node = lower.contains("<message>") || lower.contains("<code>");
    has_error_node && has_detail_node
}

fn summarize_permission_schema_error(input: &str) -> Option<String> {
    let lower = input.to_ascii_lowercase();
    if !lower.contains("tool permission request failed") {
        return None;
    }

    let detail = if let Some(msg) = extract_json_string_field(input, "message") {
        msg
    } else {
        input.lines().find(|line| !line.trim().is_empty()).unwrap_or(input).trim().to_owned()
    };

    Some(format!("Tool permission request failed: {detail}"))
}

pub fn truncate_for_log(input: &str) -> String {
    const LIMIT: usize = 240;
    let mut out = String::new();
    for (i, ch) in input.chars().enumerate() {
        if i >= LIMIT {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out.replace('\n', "\\n")
}

pub fn extract_xml_tag_value<'a>(input: &'a str, tag: &str) -> Option<&'a str> {
    let lower = input.to_ascii_lowercase();
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = lower.find(&open)? + open.len();
    let end = start + lower[start..].find(&close)?;
    let value = input[start..end].trim();
    (!value.is_empty()).then_some(value)
}

fn extract_json_string_field(input: &str, field: &str) -> Option<String> {
    let needle = format!("\"{field}\"");
    let start = input.find(&needle)? + needle.len();
    let rest = input[start..].trim_start();
    let colon_idx = rest.find(':')?;
    let mut chars = rest[colon_idx + 1..].trim_start().chars();
    if chars.next()? != '"' {
        return None;
    }

    let mut escaped = false;
    let mut out = String::new();
    for ch in chars {
        if escaped {
            let mapped = match ch {
                'n' => '\n',
                'r' => '\r',
                't' => '\t',
                '"' => '"',
                '\\' => '\\',
                _ => ch,
            };
            out.push(mapped);
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(out),
            _ => out.push(ch),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{
        TurnErrorClass, classify_turn_error, looks_like_internal_error, summarize_internal_error,
    };

    #[test]
    fn classifies_plan_limit_errors() {
        assert_eq!(classify_turn_error("HTTP 429 Too Many Requests"), TurnErrorClass::PlanLimit);
        assert_eq!(
            classify_turn_error("turn failed: max budget exceeded"),
            TurnErrorClass::PlanLimit
        );
    }

    #[test]
    fn classifies_auth_required_errors() {
        assert_eq!(
            classify_turn_error("authentication failed: please log in"),
            TurnErrorClass::AuthRequired
        );
    }

    /// Locks in the full needle list. A future dedup pass that
    /// drops any of these substrings would silently regress
    /// auth-required classification  -  assert the matrix end-to-end.
    #[test]
    fn auth_required_covers_all_consolidated_needles() {
        for s in [
            // Underscore form (typical CLI emit).
            "authentication_failed: token expired",
            // Space form of the same needle.
            "authentication failed mid-request",
            // Bare "unauthenticated" form.
            "the request is unauthenticated",
            // Substring "authentication required".
            "tool authentication required to continue",
            // Bare "auth required" form (shorter than "authentication required").
            "auth required for this endpoint",
            // "not authenticated" sentence form.
            "client is not authenticated yet",
            // Conjunctive 401 + auth check (HTTP-shape error).
            "got 401 from /auth endpoint",
            // Pre-existing needles (regression-protection).
            "/login to continue",
            "please log in",
            "login required",
            "unauthorized",
        ] {
            assert_eq!(
                classify_turn_error(s),
                TurnErrorClass::AuthRequired,
                "expected AuthRequired for {s:?}"
            );
        }
    }

    #[test]
    fn classifies_internal_errors() {
        assert_eq!(
            classify_turn_error(
                r#"{"jsonrpc":"2.0","error":{"code":-32603,"message":"internal rpc fault"}}"#
            ),
            TurnErrorClass::Internal
        );
        assert!(looks_like_internal_error(
            "<error><code>-32603</code><message>Adapter process crashed</message></error>"
        ));
    }

    #[test]
    fn classifies_other_errors() {
        assert_eq!(classify_turn_error("turn failed: timeout"), TurnErrorClass::Other);
    }

    #[test]
    fn summarize_prefers_permission_schema_error_message() {
        let payload = "Tool permission request failed: ZodError: [{\"message\":\"Invalid input: expected record, received undefined\"}]";
        assert_eq!(
            summarize_internal_error(payload),
            "Tool permission request failed: Invalid input: expected record, received undefined"
        );
    }

    /// #143 item 1: the wrapped error payloads sometimes use field
    /// names other than "message". Extended fallback chain now
    /// reaches `assistant_error`, `detail`, `description`, `type`,
    /// `body` so error_preview no longer comes back empty when
    /// the wire shape avoids the canonical "message" field.
    #[test]
    fn summarize_falls_through_to_assistant_error_field() {
        let payload = r#"{"jsonrpc":"2.0","error":{"assistant_error":"model context exceeded"}}"#;
        assert_eq!(summarize_internal_error(payload), "model context exceeded");
    }

    #[test]
    fn summarize_falls_through_to_detail_field() {
        let payload =
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"detail":"upstream stream closed"}}"#;
        assert_eq!(summarize_internal_error(payload), "upstream stream closed");
    }

    #[test]
    fn summarize_falls_through_to_body_field() {
        let payload =
            r#"{"jsonrpc":"2.0","error":{"code":-32603,"body":"HTTP/2.0 502 Bad Gateway"}}"#;
        assert_eq!(summarize_internal_error(payload), "HTTP/2.0 502 Bad Gateway");
    }

    /// `"message"` field wins over the fallback fields - the
    /// pre-existing extractor runs first so this test pins
    /// precedence ordering.
    #[test]
    fn summarize_message_field_wins_over_fallbacks() {
        let payload = r#"{"error":{"message":"primary","detail":"secondary"}}"#;
        assert_eq!(summarize_internal_error(payload), "primary");
    }
}
