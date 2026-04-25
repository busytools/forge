//! Stream-json line encode / decode.

use serde_json::{Value, json};

use crate::Error;
use crate::control::ControlRequest;
use crate::messages::Message;

/// A single stream-json line from the subprocess — either a regular message
/// or a control request.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum DecodedLine {
    /// An assistant/user/system/result message.
    Message(Message),
    /// A control request (e.g. permission check, MCP message, hook callback).
    Control(ControlRequest),
    /// The CLI has withdrawn a previously-issued `control_request` — the
    /// handler matching `request_id` should be cancelled if still in flight.
    /// Wire shape `{"type":"control_cancel_request","request_id":"..."}`
    /// per Python SDK `_internal/query.py:274-280`.
    ControlCancel {
        /// `request_id` of the `control_request` being withdrawn.
        request_id: String,
    },
    /// The CLI is responding to an outbound `control_request` we sent
    /// (initialize, interrupt, `set_permission_mode`, …). These arrive on
    /// stdout and are normally consumed synchronously by the client's
    /// outbound-control wait loop — the read-dispatch loop in
    /// [`Client::next_event`](crate::Client::next_event) never sees them.
    /// Represented here so downstream tools (the wire-conformance replay
    /// harness, debug captures) can recognise and categorise the frame
    /// instead of mis-flagging it as `Unknown`.
    ControlResponse {
        /// `request_id` of the outbound `control_request` this responds to.
        request_id: String,
        /// Full JSON payload — useful for inspection and replay.
        raw: Value,
    },
    /// Forward-compat fallback: the CLI emitted a frame with an unrecognised
    /// top-level `type` field. Forge-sdk doesn't crash on these — it logs
    /// a warning via `tracing::warn!` in the dispatch path and lets the
    /// reader continue. Callers (like the wire-conformance harness) can
    /// detect and report these explicitly by matching this variant.
    ///
    /// `type_str` is the raw `type` field value the CLI sent; `raw` is the
    /// full JSON object for inspection / logging / replay. Neither is
    /// typed — by definition, we don't know what this frame is.
    Unknown {
        /// The unrecognised `type` field value.
        type_str: String,
        /// Full JSON payload for later inspection.
        raw: Value,
    },
}

/// Parse one stream-json line into a [`Message`].
///
/// `line` must not include a trailing newline. `line_number` is used only
/// for error reporting and should be 1-based.
///
/// # Errors
///
/// - [`Error::JsonDecode`] when the line is not valid JSON.
/// - [`Error::MessageParse`] when the JSON parses but doesn't match any
///   known message shape.
pub fn decode_line(line: &str, line_number: u64) -> Result<Message, Error> {
    let value: Value = serde_json::from_str(line).map_err(|source| Error::JsonDecode {
        line: line_number,
        source,
    })?;
    serde_json::from_value(value)
        .map_err(|e| Error::message_parse(format!("line {line_number}: {e}")))
}

/// Decode one stream-json line, dispatching on the `type` field.
///
/// Returns one of [`DecodedLine`]'s variants. For forward-compat with
/// future `claude` CLI releases, any top-level `type` value forge-sdk
/// doesn't recognise lands in [`DecodedLine::Unknown`] rather than
/// erroring — callers (notably the wire-conformance harness) can
/// detect these by matching the variant. The read loop in
/// [`Client::next_event`](crate::Client::next_event) logs a
/// `tracing::warn!` on Unknown and continues reading.
///
/// # Errors
///
/// - [`Error::JsonDecode`] when not valid JSON.
/// - [`Error::MessageParse`] when the `type` field is missing entirely
///   or the inner shape of a recognised type is invalid.
pub fn decode_dispatch(line: &str, line_number: u64) -> Result<DecodedLine, Error> {
    let value: Value = serde_json::from_str(line).map_err(|source| Error::JsonDecode {
        line: line_number,
        source,
    })?;
    let ty = value
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::message_parse(format!("line {line_number}: missing `type` field")))?;
    match ty {
        "control_request" => {
            let req: ControlRequest = serde_json::from_value(value)
                .map_err(|e| Error::message_parse(format!("line {line_number}: {e}")))?;
            Ok(DecodedLine::Control(req))
        }
        "control_cancel_request" => {
            let request_id = value
                .get("request_id")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    Error::message_parse(format!(
                        "line {line_number}: control_cancel_request missing `request_id`"
                    ))
                })?
                .to_string();
            Ok(DecodedLine::ControlCancel { request_id })
        }
        "control_response" => {
            // `response.request_id` is where the CLI echoes the
            // request_id we originally sent. A `control_response` with
            // no `request_id` is wire corruption — route it through
            // `DecodedLine::Unknown` so the conformance harness counts
            // it under `unknown_types` rather than as a "valid"
            // ControlResponse. The runtime path (`send_control` /
            // `next_event`) treats unknowns as warn-and-skip.
            match value
                .pointer("/response/request_id")
                .and_then(Value::as_str)
            {
                Some(rid) => Ok(DecodedLine::ControlResponse {
                    request_id: rid.to_string(),
                    raw: value,
                }),
                None => Ok(DecodedLine::Unknown {
                    type_str: "control_response (missing /response/request_id)".to_string(),
                    raw: value,
                }),
            }
        }
        "assistant" | "user" | "system" | "result" | "rate_limit_event" | "stream_event"
        | "error" => {
            let msg: Message = serde_json::from_value(value)
                .map_err(|e| Error::message_parse(format!("line {line_number}: {e}")))?;
            Ok(DecodedLine::Message(msg))
        }
        other => Ok(DecodedLine::Unknown {
            type_str: other.to_string(),
            raw: value,
        }),
    }
}

/// Encode a user prompt as one line of stream-json input.
///
/// Returns a string terminated by `\n` suitable for writing directly to the
/// subprocess's stdin.
///
/// # Errors
///
/// [`Error::MessageParse`] wrapping a JSON serialization failure
/// (extraordinarily rare for string inputs; included for totality).
pub fn encode_user_prompt(prompt: &str, session_id: &str) -> Result<String, Error> {
    // Python `client.py:260-267` sends `content` as a bare string for
    // plain-text prompts. forge-sdk matches byte-for-byte so argv +
    // stdin dumps line up between the two SDKs when a caller wants to
    // compare them. The CLI accepts both the bare-string and
    // `[{"type":"text","text":prompt}]` shapes, but parity means
    // emitting the simpler form.
    let payload = json!({
        "type": "user",
        "message": {"role": "user", "content": prompt},
        "session_id": session_id,
        "parent_tool_use_id": null,
    });
    let mut line = serde_json::to_string(&payload).map_err(|e| Error::encode("user prompt", e))?;
    line.push('\n');
    Ok(line)
}
