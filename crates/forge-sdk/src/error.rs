//! Error type for the forge-sdk crate.
//!
//! Mirrors the Python SDK's exception hierarchy in a single `thiserror` enum.
//! Every fallible public API returns `Result<T, Error>`.

use std::io;
use thiserror::Error;

/// All errors surfaced by `forge-sdk`.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum Error {
    /// The `claude` binary (or a custom replacement) was not found on `PATH`.
    ///
    /// Mirrors Python's `CLINotFoundError`.
    #[error("claude CLI binary `{binary}` not found on PATH")]
    CliNotFound {
        /// The binary path or name that was attempted.
        binary: String,
    },

    /// The subprocess exited with a non-zero status or was terminated by a signal.
    ///
    /// Mirrors Python's `ProcessError`.
    #[error("claude subprocess failed (exit code {exit_code:?}): {stderr}")]
    Process {
        /// Exit status, if the process exited normally.
        exit_code: Option<i32>,
        /// Captured stderr content (may be truncated).
        stderr: String,
    },

    /// Could not establish or maintain the stdio connection to the subprocess.
    ///
    /// Mirrors Python's `CLIConnectionError`.
    #[error("connection to claude subprocess failed: {reason}")]
    Connection {
        /// Human-readable explanation.
        reason: String,
    },

    /// A line of stream-json from the subprocess could not be parsed as JSON.
    ///
    /// Mirrors Python's `CLIJSONDecodeError`.
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
    /// Mirrors Python's `MessageParseError`.
    #[error("stream-json message has unknown or invalid shape: {reason}")]
    MessageParse {
        /// Human-readable explanation (often includes the offending JSON excerpt).
        reason: String,
    },

    /// Wrapping `std::io::Error` for convenience.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
}
