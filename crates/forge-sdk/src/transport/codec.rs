//! Stream-json line encode / decode.

use serde_json::{Value, json};

use crate::Error;
use crate::messages::Message;

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
    serde_json::from_value(value).map_err(|e| Error::MessageParse {
        reason: format!("line {line_number}: {e}"),
    })
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
    let payload = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [
                {"type": "text", "text": prompt}
            ]
        },
        "session_id": session_id,
        "parent_tool_use_id": null,
    });
    let mut line = serde_json::to_string(&payload).map_err(|e| Error::MessageParse {
        reason: format!("could not encode prompt: {e}"),
    })?;
    line.push('\n');
    Ok(line)
}
