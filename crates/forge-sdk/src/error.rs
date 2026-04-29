//! Error type for the forge-sdk crate.
//!
//! Single typed error in a single `thiserror` enum.
//! Every fallible public API returns `Result<T, Error>`.

use std::io;
use thiserror::Error;

/// All errors surfaced by `forge-sdk`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The `claude` binary (or a custom replacement) was not found on `PATH`.
    ///
    /// Wraps the CLI's `CLINotFoundError`.
    #[error("claude CLI binary `{binary}` not found on PATH")]
    CliNotFound {
        /// The binary path or name that was attempted.
        binary: String,
    },

    /// The subprocess exited with a non-zero status or was terminated by a signal.
    ///
    /// Wraps the CLI's `ProcessError`.
    #[error("claude subprocess failed (exit code {exit_code:?}): {stderr}")]
    Process {
        /// Exit status, if the process exited normally.
        exit_code: Option<i32>,
        /// Captured stderr content (may be truncated).
        stderr: String,
    },

    /// Could not establish or maintain the stdio connection to the subprocess.
    ///
    /// Wraps the CLI's `CLIConnectionError`.
    #[error("connection to claude subprocess failed: {reason}")]
    Connection {
        /// Human-readable explanation.
        reason: String,
    },

    /// A line of stream-json from the subprocess could not be parsed as JSON.
    ///
    /// Wraps the CLI's `CLIJSONDecodeError`.
    #[error("stream-json decode failed at line {line}: {source}")]
    JsonDecode {
        /// 1-based line number of the offending line.
        line: u64,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// A stream-json message parsed as JSON but did not match any known message schema.
    ///
    /// Wraps the CLI's `MessageParseError` (`_errors.py:48-55`), including
    /// the optional `data` field that carries the offending payload when
    /// one is available. the CLI reads this as a `dict[str, Any] | None`;
    /// forge-sdk types it as `Option<serde_json::Value>` so callers
    /// inspecting the failure can recover the decoded JSON without
    /// re-parsing the error message.
    #[error("stream-json message has unknown or invalid shape: {reason}")]
    MessageParse {
        /// Human-readable explanation (often includes the offending JSON excerpt).
        reason: String,
        /// Decoded CLI payload the failure originated from, when the
        /// call site had it in hand. `None` for failures surfaced
        /// before a payload is parsed (e.g. envelope encode failures).
        data: Option<serde_json::Value>,
    },

    /// Forge-sdk failed to JSON-encode a local payload (initialize
    /// envelope, `control_request` body, hook response, session
    /// mutation entry, etc.). Distinct from
    /// [`Error::MessageParse`], which is for parse failures on
    /// data the CLI sent us — `Encode` is when *we* couldn't
    /// serialise something locally and the bug is on the SDK or
    /// caller side, not the CLI's. `context` names what we were
    /// trying to encode (e.g. "agents map", "initialize body",
    /// "user prompt").
    #[error("forge-sdk failed to encode local {context}: {source}")]
    Encode {
        /// What we were trying to encode.
        context: String,
        /// The underlying serde error.
        #[source]
        source: serde_json::Error,
    },

    /// Wrapping `std::io::Error` for convenience.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}

impl Error {
    /// Construct a [`Error::MessageParse`] carrying only a reason string
    /// (most call sites). Equivalent to the struct-literal form with
    /// `data: None`.
    #[must_use]
    pub fn message_parse(reason: impl Into<String>) -> Self {
        Self::MessageParse {
            reason: reason.into(),
            data: None,
        }
    }

    /// Construct a [`Error::MessageParse`] that carries the offending
    /// payload alongside the reason — mirrors the CLI's
    /// `MessageParseError(msg, data)`.
    #[must_use]
    pub fn message_parse_with_data(reason: impl Into<String>, data: serde_json::Value) -> Self {
        Self::MessageParse {
            reason: reason.into(),
            data: Some(data),
        }
    }

    /// Construct an [`Error::Encode`] for a local JSON encode
    /// failure. Use this for `serde_json::to_string`/`to_value`
    /// failures on data the SDK constructed, NOT for parse
    /// failures on incoming CLI bytes.
    #[must_use]
    pub fn encode(context: impl Into<String>, source: serde_json::Error) -> Self {
        Self::Encode {
            context: context.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn cli_not_found_display() {
        let err = Error::CliNotFound {
            binary: "claude".into(),
        };
        let rendered = format!("{err}");
        assert!(
            rendered.contains("claude"),
            "expected binary in message, got: {rendered}"
        );
        assert!(
            rendered.to_lowercase().contains("not found"),
            "expected 'not found' in message, got: {rendered}"
        );
    }

    #[test]
    fn process_error_display_includes_exit_code() {
        let err = Error::Process {
            exit_code: Some(17),
            stderr: "permission denied".into(),
        };
        let rendered = format!("{err}");
        assert!(
            rendered.contains("17"),
            "expected exit code 17, got: {rendered}"
        );
        assert!(
            rendered.contains("permission denied"),
            "expected stderr, got: {rendered}"
        );
    }

    #[test]
    fn json_decode_error_display_includes_line_number() {
        let raw_err = serde_json::from_str::<serde_json::Value>("not json").unwrap_err();
        let err = Error::JsonDecode {
            line: 42,
            source: raw_err,
        };
        let rendered = format!("{err}");
        assert!(rendered.contains("42"), "expected line 42, got: {rendered}");
    }
}
