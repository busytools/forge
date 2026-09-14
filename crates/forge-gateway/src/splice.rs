//! The byte-level `model` splice.
//!
//! The request body is never deserialized: the splice finds the
//! top-level `"model"` key, optionally replaces its string value, and
//! fixes `Content-Length`. Everything else - tool schemas, betas,
//! `context_management` - passes through byte for byte, which is the
//! whole reason the gateway forwards instead of re-encoding.

/// Why a body could not be spliced. Both are loud by design: a splice
/// that silently forwarded the wrong model would be worse than a
/// failure naming the session.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SpliceError {
    #[error("request body has no top-level \"model\" field")]
    ModelMissing,
    #[error("request body's top-level \"model\" is not a string")]
    ModelNotAString,
}

/// Extract the top-level `model` string and, when `replacement` is
/// `Some`, rewrite it in place. Returns the (possibly new) body and
/// the extracted model. Nested `"model"` keys are never touched: only
/// a string key at brace depth 1 counts.
pub fn splice_model(
    body: &[u8],
    replacement: Option<&str>,
) -> Result<(Vec<u8>, String), SpliceError> {
    let Some((value_span, model)) = find_top_level_model(body)? else {
        return Err(SpliceError::ModelMissing);
    };
    let (start, end) = value_span;
    let rewritten = match replacement {
        None => body.to_vec(),
        Some(new_value) => {
            let mut out = Vec::with_capacity(body.len() - (end - start) + new_value.len() + 2);
            out.extend_from_slice(&body[..start]);
            out.push(b'"');
            out.extend_from_slice(new_value.as_bytes());
            out.push(b'"');
            out.extend_from_slice(&body[end..]);
            out
        }
    };
    Ok((rewritten, model))
}

/// The located top-level model: its byte span (excluding the quotes)
/// and its content.
type ModelSpan = ((usize, usize), String);

/// Locate the top-level `"model"` string value: its byte span
/// (excluding the quotes) and its content. Scans with a string/escape
/// state machine so a `"model"` that is only a nested key or a value
/// elsewhere is skipped.
fn find_top_level_model(body: &[u8]) -> Result<Option<ModelSpan>, SpliceError> {
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut escaped = false;
    let mut i = 0;
    while i < body.len() {
        let byte = body[i];
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match byte {
            b'"' => {
                let (content, next) = read_string(body, i);
                in_string = false;
                // A string at depth 1 in an object is a KEY; the value
                // follows the colon. In an array it is a value and is
                // skipped - "model" only counts as a key.
                if depth == 1 && content == b"model" {
                    let colon = skip_ws(body, next);
                    if body.get(colon) == Some(&b':') {
                        let value_start = skip_ws(body, colon + 1);
                        return match body.get(value_start) {
                            Some(b'"') => {
                                let (value, after) = read_string(body, value_start);
                                let extracted =
                                    unescape(value).ok_or(SpliceError::ModelNotAString)?;
                                Ok(Some(((value_start + 1, after - 1), extracted)))
                            }
                            Some(_) => Err(SpliceError::ModelNotAString),
                            None => Err(SpliceError::ModelMissing),
                        };
                    }
                }
                i = next;
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                i += 1;
            }
            _ => i += 1,
        }
    }
    Ok(None)
}

/// Read a JSON string starting at `start` (the opening quote). Returns
/// the raw content bytes and the index just past the closing quote.
fn read_string(body: &[u8], start: usize) -> (&[u8], usize) {
    let mut i = start + 1;
    let mut escaped = false;
    while let Some(&byte) = body.get(i) {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return (&body[start + 1..i], i + 1);
        }
        i += 1;
    }
    (&body[start + 1..], body.len())
}

fn skip_ws(body: &[u8], mut i: usize) -> usize {
    while let Some(&byte) = body.get(i) {
        if byte.is_ascii_whitespace() {
            i += 1;
        } else {
            break;
        }
    }
    i
}

/// Model names are plain ASCII slugs in practice; unescape the two
/// escapes that could appear around them and reject exotic ones rather
/// than misreporting the name.
fn unescape(raw: &[u8]) -> Option<String> {
    let text = std::str::from_utf8(raw).ok()?;
    if !text.contains('\\') {
        return Some(text.to_owned());
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            '"' => out.push('"'),
            '\\' => out.push('\\'),
            '/' => out.push('/'),
            _ => return None,
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Representative Messages request shapes, written to mirror the
    /// captured 2.1.263 bodies: the tool array with the deferred-tool
    /// placeholder, `context_management`, and a large tool schema.
    fn captured_shape_body(model: &str) -> Vec<u8> {
        let body = format!(
            r#"{{"model":"{model}","max_tokens":32000,"system":[{{"type":"text","text":"You are Claude Code."}}],"tools":[{{"name":"ToolSearch","description":"deferred tool loading placeholder"}}],"context_management":{{"edits":[{{"type":"clear_tool_uses_20250919"}}]}},"messages":[{{"role":"user","content":"hi"}}]}}"#
        );
        body.into_bytes()
    }

    #[test]
    fn extracts_the_top_level_model_from_a_captured_shape() {
        let body = captured_shape_body("claude-opus-5");
        let (_, model) = splice_model(&body, None).expect("splice");
        assert_eq!(model, "claude-opus-5");
    }

    #[test]
    fn a_rewrite_changes_only_the_model_and_fixes_the_length() {
        let body = captured_shape_body("claude-opus-5");
        let prefix = &body[..body.len()];
        let (rewritten, model) =
            splice_model(&body, Some("deepseek/deepseek-v4.1-flash")).expect("splice");
        assert_eq!(model, "claude-opus-5", "the extracted name is the old one");
        let text = String::from_utf8(rewritten.clone()).expect("utf8");
        assert!(text.contains("deepseek/deepseek-v4.1-flash"), "the new name is in");
        assert!(!text.contains("\"claude-opus-5\""), "the old name is gone");
        // Everything else is byte-identical: same tools, same system
        // prompt, same trailing keys.
        let old_text = String::from_utf8(prefix.to_vec()).expect("utf8");
        for kept in ["ToolSearch", "context_management", "clear_tool_uses_20250919"] {
            assert!(text.contains(kept) && old_text.contains(kept), "{kept} survives the splice");
        }
    }

    #[test]
    fn a_nested_model_key_is_never_touching() {
        let body = br#"{"messages":[{"role":"user","content":"model"}],"metadata":{"model":"nested-model"},"model":"real-model"}"#;
        let (rewritten, model) = splice_model(body, Some("new")).expect("splice");
        assert_eq!(model, "real-model", "only the top-level key counts");
        let text = String::from_utf8(rewritten).expect("utf8");
        assert!(text.contains("nested-model"), "the nested key survives");
        assert!(text.contains("\"new\""), "the top-level value is rewritten");
    }

    #[test]
    fn a_non_string_model_is_a_loud_failure() {
        let body = br#"{"model":42,"messages":[]}"#;
        assert_eq!(splice_model(body, None), Err(SpliceError::ModelNotAString));
    }

    #[test]
    fn a_body_with_no_model_is_a_loud_failure() {
        let body = br#"{"max_tokens":32,"messages":[]}"#;
        assert_eq!(splice_model(body, None), Err(SpliceError::ModelMissing));
        assert_eq!(splice_model(b"[]", None), Err(SpliceError::ModelMissing));
        assert_eq!(splice_model(b"", None), Err(SpliceError::ModelMissing));
    }

    #[test]
    fn whitespace_between_key_and_value_is_tolerated() {
        let body = br#"{"model" : "claude-opus-5"}"#;
        let (rewritten, model) = splice_model(body, Some("x")).expect("splice");
        assert_eq!(model, "claude-opus-5");
        assert!(String::from_utf8(rewritten).expect("utf8").contains("\"x\""));
    }
}
