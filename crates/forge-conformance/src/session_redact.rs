//! Session-persistence → stream-json transformer + PII redactor.
//!
//! Claude Code persists each session as JSONL under
//! `$CLAUDE_CONFIG_DIR/projects/<slug>/<session-id>.jsonl`. The
//! records overlap with the stream-json wire protocol in broad shape
//! (same `type: assistant|user|system|result` discriminant, same
//! `message.content` envelope) but add persistence-only fields in
//! camelCase (`sessionId`, `parentUuid`, `cwd`, `gitBranch`,
//! `timestamp`, `version`, …) and drop wire-only frames
//! (`rate_limit_event`, `control_*`).
//!
//! This module handles two jobs:
//!
//! 1. **Transform** a persistence line into a stream-json-shaped line
//!    the [`decode_dispatch`](forge_sdk::transport::codec::decode_dispatch)
//!    decoder accepts: rename `sessionId` → `session_id`, drop
//!    persistence fields, map `parentUuid` → `parent_tool_use_id`
//!    where appropriate.
//! 2. **Redact** PII so the redactor's output is safe to commit as a
//!    fixture: replace `/Users/<name>/…` paths, replace assistant /
//!    user message text with `<redacted N bytes>` stubs, replace
//!    `tool_use` inputs and `tool_result` bodies with placeholders,
//!    map session / uuid / request_id values to stable opaque tokens.
//!
//! The redactor is deterministic: a given input line produces the same
//! output line, so fixtures regenerate cleanly under version control.

#![allow(clippy::too_many_lines)]

use std::collections::HashMap;

use serde_json::Value;

/// Per-run state so that each distinct session_id / uuid gets a stable
/// opaque token. Reused across every line in the same transformation
/// pass so references inside a single session stay consistent.
#[derive(Default)]
pub struct RedactState {
    ids: HashMap<String, String>,
}

impl RedactState {
    /// Token-of-record for an arbitrary id. The first time we see a
    /// given input, we mint `<prefix>_<n>` (n monotonic per prefix).
    fn opaque(&mut self, prefix: &str, input: &str) -> String {
        if let Some(existing) = self.ids.get(input) {
            return existing.clone();
        }
        let n = self
            .ids
            .values()
            .filter(|v| v.starts_with(&format!("{prefix}_")))
            .count();
        let out = format!("{prefix}_{n}");
        self.ids.insert(input.to_string(), out.clone());
        out
    }
}

/// Transform one persistence-format JSONL line into a stream-json
/// wire-shape line. Returns `Ok(None)` for entries that don't map
/// (persistence-only types like `file-history-snapshot`, `attachment`,
/// `last-prompt`).
///
/// # Errors
///
/// Returns a string describing the first shape mismatch. The caller
/// decides whether to propagate or skip the line.
pub fn transform_persistence_line(
    line: &str,
    state: &mut RedactState,
) -> Result<Option<String>, String> {
    let mut v: Value = serde_json::from_str(line).map_err(|e| format!("json parse: {e}"))?;
    let Some(ty) = v.get("type").and_then(Value::as_str).map(str::to_string) else {
        return Ok(None);
    };
    if !matches!(
        ty.as_str(),
        "assistant" | "user" | "system" | "result" | "rate_limit_event" | "stream_event" | "error"
    ) {
        return Ok(None);
    }

    let obj = v
        .as_object_mut()
        .ok_or_else(|| "top-level not an object".to_string())?;

    // Rename sessionId → session_id.
    if let Some(sid) = obj.remove("sessionId") {
        if let Some(s) = sid.as_str() {
            let opaque = state.opaque("session", s);
            obj.insert("session_id".into(), Value::String(opaque));
        }
    }

    // Map parentUuid → parent_tool_use_id when present + non-null.
    if let Some(p) = obj.remove("parentUuid") {
        if let Some(s) = p.as_str() {
            let opaque = state.opaque("tool_use", s);
            obj.insert("parent_tool_use_id".into(), Value::String(opaque));
        } else {
            obj.insert("parent_tool_use_id".into(), Value::Null);
        }
    } else {
        obj.insert("parent_tool_use_id".into(), Value::Null);
    }

    // Opaque-map the top-level uuid.
    if let Some(u) = obj.get("uuid").and_then(Value::as_str).map(str::to_string) {
        let opaque = state.opaque("uuid", &u);
        obj.insert("uuid".into(), Value::String(opaque));
    }

    // Drop persistence-only fields the wire decoder doesn't expect.
    // `toolUseResult` / `lastPrompt` / `content` (snapshot body) carry
    // raw tool output / prompt text with embedded paths + project
    // content — must be removed entirely, not just scrubbed.
    for field in [
        "attachment",
        "content",
        "cwd",
        "entrypoint",
        "gitBranch",
        "isSidechain",
        "isMeta",
        "isSnapshotUpdate",
        "lastPrompt",
        "messageId",
        "operation",
        "permissionMode",
        "promptId",
        "requestId",
        "snapshot",
        "sourceToolAssistantUUID",
        "timestamp",
        "toolUseResult",
        "userType",
        "version",
    ] {
        obj.remove(field);
    }

    // Redact message content + tool input/result bodies.
    if let Some(msg) = obj.get_mut("message").and_then(Value::as_object_mut) {
        if let Some(Value::Array(content)) = msg.get_mut("content") {
            for block in content.iter_mut() {
                redact_content_block(block, state);
            }
        } else if let Some(Value::String(_)) = msg.get("content") {
            msg.insert("content".into(), Value::String("<redacted-text>".into()));
        }
        // Opaque-map the nested message id.
        if let Some(mid) = msg.get("id").and_then(Value::as_str).map(str::to_string) {
            msg.insert("id".into(), Value::String(state.opaque("msg", &mid)));
        }
    }

    let out = serde_json::to_string(&v).map_err(|e| format!("json serialise: {e}"))?;
    Ok(Some(out))
}

fn redact_content_block(block: &mut Value, state: &mut RedactState) {
    let Some(obj) = block.as_object_mut() else {
        return;
    };
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    match ty.as_str() {
        "text" => {
            if let Some(Value::String(t)) = obj.get("text") {
                let bytes = t.len();
                obj.insert(
                    "text".into(),
                    Value::String(format!("<redacted-text {bytes}b>")),
                );
            }
        }
        "thinking" => {
            if let Some(Value::String(t)) = obj.get("thinking") {
                let bytes = t.len();
                obj.insert(
                    "thinking".into(),
                    Value::String(format!("<redacted-thinking {bytes}b>")),
                );
            }
            // `signature` is a signed opaque token — redact fully.
            if obj.contains_key("signature") {
                obj.insert(
                    "signature".into(),
                    Value::String("<redacted-signature>".into()),
                );
            }
        }
        "tool_use" => {
            // Keep `name` (informational), scrub `id` + `input`.
            if let Some(id) = obj.get("id").and_then(Value::as_str).map(str::to_string) {
                obj.insert("id".into(), Value::String(state.opaque("tool_use", &id)));
            }
            if obj.contains_key("input") {
                obj.insert("input".into(), serde_json::json!({"_redacted": true}));
            }
        }
        "tool_result" => {
            if let Some(id) = obj
                .get("tool_use_id")
                .and_then(Value::as_str)
                .map(str::to_string)
            {
                obj.insert(
                    "tool_use_id".into(),
                    Value::String(state.opaque("tool_use", &id)),
                );
            }
            if let Some(Value::String(c)) = obj.get("content") {
                let bytes = c.len();
                obj.insert(
                    "content".into(),
                    Value::String(format!("<redacted-tool-result {bytes}b>")),
                );
            } else if let Some(Value::Array(arr)) = obj.get_mut("content") {
                // Structured tool_result content — redact each sub-block.
                for sub in arr.iter_mut() {
                    redact_content_block(sub, state);
                }
            }
        }
        "document" | "image" => {
            // Anthropic API document/image block. Shape:
            // `{"type":"<kind>","source":{"type":"base64",
            //   "media_type":"<mime>","data":"<base64 bytes>"}}`.
            // The `data` field can be megabytes — drop it entirely.
            // Keep `media_type` so fixtures document what kind of
            // attachment was present.
            if let Some(Value::Object(src)) = obj.get_mut("source") {
                if let Some(Value::String(d)) = src.get_mut("data") {
                    let bytes = d.len();
                    *d = format!("<redacted-{ty}-data {bytes}b>");
                }
                for k in ["text", "content", "url"] {
                    if let Some(Value::String(s)) = src.get_mut(k) {
                        let bytes = s.len();
                        *s = format!("<redacted-{ty}-{k} {bytes}b>");
                    }
                }
            }
        }
        _ => {
            // Unknown block type — keep shape, scrub any obvious
            // text-carrying fields to be safe.
            for (k, val) in obj.iter_mut() {
                if let Value::String(s) = val {
                    if k == "text" || k == "content" || k == "message" {
                        *val = Value::String(format!("<redacted-{} {}b>", k, s.len()));
                    }
                }
            }
        }
    }
}

/// Transform + redact an entire persistence .jsonl file into
/// stream-json-shaped output. One `TraceLog.entries` item per
/// decodable line, all tagged `"in"` (CLI → SDK direction).
///
/// # Errors
///
/// Returns the first line error if any non-persistence line refuses to
/// transform. Persistence-only lines are silently skipped.
pub fn redact_session_file(body: &str) -> Result<Vec<String>, String> {
    let mut state = RedactState::default();
    let mut out = Vec::new();
    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match transform_persistence_line(line, &mut state) {
            Ok(Some(transformed)) => out.push(transformed),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(out)
}

/// Handy bundle: given a persistence file path, produce its redacted
/// stream-json lines plus a small summary string for logs.
///
/// # Errors
///
/// IO errors or per-line transform errors.
pub fn redact_session_path(path: &std::path::Path) -> Result<(Vec<String>, String), String> {
    let body = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;
    let lines = redact_session_file(&body)?;
    let summary = format!(
        "{}: in={} out={}",
        path.display(),
        body.lines().filter(|l| !l.trim().is_empty()).count(),
        lines.len()
    );
    Ok((lines, summary))
}
