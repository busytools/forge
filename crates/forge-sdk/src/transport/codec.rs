//! Stream-json line encode / decode.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::Error;
use crate::control::ControlRequest;
use forge_primitives::Message;

/// The CLI's heartbeat for a long-running tool call, emitted every 30
/// seconds (`elapsed_time_seconds` counting up) until the tool returns.
/// Informational: forge's own tool lifecycle rendering covers it, so
/// the reader drops the frame rather than surfacing it.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolProgress {
    /// The tool use in flight. The CLI appends `-heartbeat-<n>` with a
    /// per-call counter to the original tool's id.
    pub tool_use_id: String,
    pub tool_name: String,
    /// Seconds since the tool call started.
    pub elapsed_time_seconds: f64,
    /// True on the 30-second cadence heartbeats. Defaults to false so
    /// a progress frame without the flag still decodes.
    #[serde(default)]
    pub heartbeat: bool,
    /// The parent tool use when this call runs inside a subagent.
    #[serde(default)]
    pub parent_tool_use_id: Option<String>,
}

/// A single stream-json line from the subprocess - either a regular message
/// or a control request.
#[derive(Debug, Clone)]
pub enum DecodedLine {
    /// An assistant/user/system/result message.
    Message(Message),
    /// A control request (e.g. permission check, MCP message, hook callback).
    Control(ControlRequest),
    /// The CLI has withdrawn a previously-issued `control_request` - the
    /// handler matching `request_id` should be cancelled if still in flight.
    /// Wire shape `{"type":"control_cancel_request","request_id":"..."}`
    ///.
    ControlCancel {
        /// `request_id` of the `control_request` being withdrawn.
        request_id: String,
    },
    /// The CLI is responding to an outbound `control_request` we sent
    /// (initialize, interrupt, `set_permission_mode`, …). These arrive on
    /// stdout and are normally consumed synchronously by the client's
    /// outbound-control wait loop - the read-dispatch loop in
    /// the events stream returned by [`Client::spawn`](crate::Client::spawn) never sees them.
    /// Represented here so downstream tools (the wire-conformance replay
    /// harness, debug captures) can recognise and categorise the frame
    /// instead of mis-flagging it as `Unknown`.
    ControlResponse {
        /// `request_id` of the outbound `control_request` this responds to.
        request_id: String,
        /// Full JSON payload - useful for inspection and replay.
        raw: Value,
    },
    /// The CLI's 30-second heartbeat for a tool call in flight
    /// (`tool_progress`). Typed rather than `Unknown` so the
    /// conformance replay classifies it as decoded, but never surfaced
    /// as an event - see [`ToolProgress`].
    ToolProgress(ToolProgress),
    /// Forward-compat fallback: the CLI emitted a frame with an unrecognised
    /// top-level `type` field. Forge-sdk doesn't crash on these - it logs
    /// a warning via `tracing::warn!` in the dispatch path and lets the
    /// reader continue. Callers (like the wire-conformance harness) can
    /// detect and report these explicitly by matching this variant.
    ///
    /// `type_str` is the raw `type` field value the CLI sent; `raw` is the
    /// full JSON object for inspection / logging / replay. Neither is
    /// typed - by definition, we don't know what this frame is.
    Unknown {
        /// The unrecognised `type` field value.
        type_str: String,
        /// Full JSON payload for later inspection.
        raw: Value,
    },
    /// A line that failed to decode: not valid JSON, or valid JSON that
    /// matched no message shape. Typed rather than an `Err` so the read
    /// loop skips the line and continues - a per-line decode failure
    /// must not end the session, the same rule `Unknown` applies to
    /// unknown types. The reader warns with the line and reason.
    Malformed {
        /// 1-based number of the offending line.
        line: u64,
        /// Why the line failed to decode.
        reason: String,
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
    let value: Value = serde_json::from_str(line)
        .map_err(|source| Error::JsonDecode { line: line_number, source })?;
    serde_json::from_value(value)
        .map_err(|e| Error::message_parse(format!("line {line_number}: {e}")))
}

/// Decode one stream-json line, dispatching on the `type` field.
///
/// Returns one of [`DecodedLine`]'s variants and never fails. For
/// forward-compat with future `claude` CLI releases, any top-level
/// `type` value forge-sdk doesn't recognise lands in
/// [`DecodedLine::Unknown`], and a line that cannot be decoded at all
/// lands in [`DecodedLine::Malformed`] - callers (notably the
/// wire-conformance harness) can detect both by matching the variant.
/// The read loop in the events stream returned by
/// [`Client::spawn`](crate::Client::spawn) warns on either and
/// continues reading.
pub fn decode_dispatch(line: &str, line_number: u64) -> DecodedLine {
    let value: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(e) => {
            return DecodedLine::Malformed {
                line: line_number,
                reason: format!("invalid JSON: {e}"),
            };
        }
    };
    let Some(ty) = value.get("type").and_then(|v| v.as_str()) else {
        return DecodedLine::Malformed {
            line: line_number,
            reason: "missing `type` field".to_string(),
        };
    };
    match ty {
        "control_request" => match serde_json::from_value::<ControlRequest>(value) {
            Ok(req) => DecodedLine::Control(req),
            Err(e) => DecodedLine::Malformed { line: line_number, reason: e.to_string() },
        },
        "control_cancel_request" => {
            let request_id = value.get("request_id").and_then(Value::as_str);
            match request_id {
                Some(rid) => DecodedLine::ControlCancel { request_id: rid.to_string() },
                None => DecodedLine::Malformed {
                    line: line_number,
                    reason: "control_cancel_request missing `request_id`".to_string(),
                },
            }
        }
        "control_response" => {
            // `response.request_id` is where the CLI echoes the
            // request_id we originally sent. A `control_response` with
            // no `request_id` is wire corruption - route it through
            // `DecodedLine::Unknown` so the conformance harness counts
            // it under `unknown_types` rather than as a "valid"
            // ControlResponse. The runtime path (`send_control` /
            // `next_event`) treats unknowns as warn-and-skip.
            match value.pointer("/response/request_id").and_then(Value::as_str) {
                Some(rid) => {
                    DecodedLine::ControlResponse { request_id: rid.to_string(), raw: value }
                }
                None => DecodedLine::Unknown {
                    type_str: "control_response (missing /response/request_id)".to_string(),
                    raw: value,
                },
            }
        }
        "assistant" | "user" | "system" | "result" | "rate_limit_event" | "stream_event"
        | "error" => match serde_json::from_value::<Message>(value) {
            Ok(msg) => DecodedLine::Message(msg),
            Err(e) => DecodedLine::Malformed { line: line_number, reason: e.to_string() },
        },
        "tool_progress" => {
            // A heartbeat that fails to fit is an unrecognised shape,
            // not a corrupt line: degrade to `Unknown` so the harness
            // counts it there.
            match serde_json::from_value::<ToolProgress>(value.clone()) {
                Ok(progress) => DecodedLine::ToolProgress(progress),
                Err(_) => DecodedLine::Unknown {
                    type_str: "tool_progress (unparseable payload)".to_string(),
                    raw: value,
                },
            }
        }
        other => DecodedLine::Unknown { type_str: other.to_string(), raw: value },
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
    // The CLI accepts both the bare-string and
    // `[{"type":"text","text":prompt}]` shapes for `content` on
    // user turns. forge-sdk emits the simpler bare-string form.
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

/// Encode a user-turn line with structured content blocks (text +
/// image) as the message body. Use this when the prompt includes
/// images or other non-text blocks; for plain text use
/// [`encode_user_prompt`] which emits the simpler bare-string form.
///
/// `content` is forwarded verbatim as the message body's `content`
/// field - callers must build CLI-compatible block objects (e.g.
/// `{"type":"text","text":"..."}`,
/// `{"type":"image","source":{...}}`).
///
/// # Errors
///
/// [`Error::MessageParse`] wrapping a JSON serialization failure.
pub fn encode_user_prompt_with_content(
    content: &[Value],
    session_id: &str,
) -> Result<String, Error> {
    let payload = json!({
        "type": "user",
        "message": {"role": "user", "content": content},
        "session_id": session_id,
        "parent_tool_use_id": null,
    });
    let mut line =
        serde_json::to_string(&payload).map_err(|e| Error::encode("user prompt blocks", e))?;
    line.push('\n');
    Ok(line)
}
