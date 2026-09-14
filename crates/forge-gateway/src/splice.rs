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

    /// A real CLI-emitted request body, captured through the gateway
    /// against a local 401 stub (nothing billed) and redacted of
    /// session-local identifiers. Committed so the splice parser and
    /// its pin share no blind spots: whatever the real body contains,
    /// the test sees.
    fn live_capture_body() -> Vec<u8> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/messages_request.json");
        std::fs::read(path).expect("the committed live capture is readable")
    }

    /// Small synthetic bodies for the failure arms, where the shape
    /// must be exact.
    fn tiny_body(model_json: Option<&str>) -> Vec<u8> {
        match model_json {
            Some(model) => format!(r#"{{"model":{model},"max_tokens":8}}"#).into_bytes(),
            None => br#"{"max_tokens":8}"#.to_vec(),
        }
    }

    #[test]
    fn extracts_the_top_level_model_from_a_live_capture() {
        let body = live_capture_body();
        let (_, model) = splice_model(&body, None).expect("splice");
        assert!(!model.is_empty(), "the real body's model is extracted");
    }

    #[test]
    fn a_rewrite_changes_only_the_model_and_fixes_the_length() {
        let body = live_capture_body();
        let (rewritten, model) =
            splice_model(&body, Some("deepseek/deepseek-v4.1-flash")).expect("splice");
        assert_eq!(model, "glm-5.3-flash", "the extracted name is the old one");
        let text = String::from_utf8(rewritten.clone()).expect("utf8");
        assert!(text.contains("deepseek/deepseek-v4.1-flash"), "the new name is in");
        assert!(!text.contains("\"glm-5.3-flash\""), "the old name is gone");
        // The untouched bulk is byte-identical: same tool count, same
        // system prompt bytes.
        let original = live_capture_body();
        let original_text = String::from_utf8(original).expect("utf8");
        for kept in ["context_management", "output_config", "stream"] {
            assert!(
                text.contains(kept) && original_text.contains(kept),
                "{kept} survives the splice",
            );
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
        let body = tiny_body(Some("42"));
        assert_eq!(splice_model(&body, None), Err(SpliceError::ModelNotAString));
    }

    #[test]
    fn a_body_with_no_model_is_a_loud_failure() {
        assert_eq!(splice_model(&tiny_body(None), None), Err(SpliceError::ModelMissing));
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

    #[test]
    fn a_multi_byte_utf8_model_rewrites_without_corrupting_the_body() {
        // The replacement is inserted as raw UTF-8 bytes; the byte-span
        // arithmetic must not split a multi-byte sequence either in the
        // replacement or in the keys around it. Values use unicode
        // escapes so the source stays ASCII.
        let model_value = "caf\u{e9}-latte";
        let content = "union \u{2014} \u{7d75}\u{6587}\u{5b57}";
        let body =
            format!("{{\"model\":\"{model_value}\",\"messages\":[{{\"content\":\"{content}\"}}]}}");
        let (rewritten, model) =
            splice_model(body.as_bytes(), Some("glm-5.3-flash")).expect("splice");
        assert_eq!(model, "caf\u{e9}-latte");
        let text = String::from_utf8(rewritten).expect("the rewrite keeps the body valid UTF-8");
        assert!(text.contains("\"glm-5.3-flash\""));
        assert!(text.contains(content), "the multi-byte sibling content survives");
    }
}
