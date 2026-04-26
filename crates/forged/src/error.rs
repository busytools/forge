//! forged Error type. Mirrors the JSON-RPC error code allocations from
//! the wire spec §7.4.15.

use crate::jsonrpc::ErrorObject;

/// All errors surfaced by forged operations.
///
/// Each variant maps deterministically to a JSON-RPC error code via
/// [`Error::code`]; bridging into a wire-shape `ErrorObject` is done by
/// [`Error::to_jsonrpc`].
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    // -32600 family — JSON-RPC standard
    /// Malformed JSON on the wire (-32700).
    #[error("parse error: {0}")]
    ParseError(String),
    /// Wire JSON parsed but did not match a JSON-RPC envelope (-32600).
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    /// Request named a method the daemon does not implement (-32601).
    #[error("method not found: {0}")]
    MethodNotFound(String),
    /// Method's params field failed to deserialise into the handler's expected shape (-32602).
    #[error("invalid params: {0}")]
    InvalidParams(String),
    /// Catch-all internal failure (-32603).
    #[error("internal error: {0}")]
    InternalError(String),

    // -32000 family — forged transport
    /// Daemon is shutting down and refusing new requests (-32000).
    #[error("daemon shutting down")]
    ShuttingDown,
    /// Caller hit a per-connection rate limit (-32001).
    #[error("rate limited")]
    RateLimited,
    /// Session id referenced in the request does not exist on the daemon (-32002).
    #[error("session not found: {0}")]
    SessionNotFound(String),
    /// Subscription id referenced in the request does not exist on the daemon (-32004).
    #[error("subscription not found for session {0}")]
    SubscriptionNotFound(String),
    /// Daemon refused work because resource limits are exceeded (-32005).
    #[error("daemon overloaded")]
    Overloaded,
    /// Method temporarily unavailable; caller may retry (-32006).
    #[error("method temporarily unavailable: {0}")]
    TemporarilyUnavailable(String),
    /// Replay of older messages is no longer possible; client should refetch via `sessions.messages` (-32007).
    #[error("replay unavailable; refetch via sessions.messages (buffer={buffer_window_seconds}s)")]
    ReplayUnavailable {
        /// Width of the daemon's in-memory replay buffer, in seconds.
        buffer_window_seconds: u64,
    },

    // -32100 family — forge-sdk error mirror
    /// Bubbled-up forge-sdk error; mapped to a -321xx code internally.
    #[error(transparent)]
    Sdk(#[from] forge_sdk::Error),

    // I/O / serde at boundaries
    /// JSON encode/decode at the framing boundary (-32700).
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    /// I/O failure at the transport boundary (-32106).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

impl Error {
    /// JSON-RPC error code per the wire spec §7.4.15.
    #[must_use]
    pub fn code(&self) -> i32 {
        match self {
            // `ParseError` / `Json` both surface JSON encode/decode failure → -32700.
            Self::ParseError(_) | Self::Json(_) => -32700,
            Self::InvalidRequest(_) => -32600,
            Self::MethodNotFound(_) => -32601,
            Self::InvalidParams(_) => -32602,
            Self::InternalError(_) => -32603,
            Self::ShuttingDown => -32000,
            Self::RateLimited => -32001,
            Self::SessionNotFound(_) => -32002,
            Self::SubscriptionNotFound(_) => -32004,
            Self::Overloaded => -32005,
            Self::TemporarilyUnavailable(_) => -32006,
            Self::ReplayUnavailable { .. } => -32007,
            Self::Sdk(e) => sdk_code(e),
            Self::Io(_) => -32106,
        }
    }

    /// Convert to the wire-shape [`ErrorObject`] for a JSON-RPC response.
    ///
    /// Variants carrying structured payload populate the `data` field so
    /// clients can recover machine-readable context without regex-parsing
    /// the human-readable message:
    /// - [`Error::ReplayUnavailable`] → `{ "buffer_window_seconds": N }`
    /// - [`Error::SessionNotFound`] → `{ "session_id": "..." }`
    /// - [`Error::SubscriptionNotFound`] → `{ "session_id": "..." }`
    /// - [`Error::Sdk`] wrapping `forge_sdk::Error::Process` → `{ "exit_code": N }`
    #[must_use]
    pub fn to_jsonrpc(&self) -> ErrorObject {
        let data = match self {
            Self::ReplayUnavailable {
                buffer_window_seconds,
            } => Some(serde_json::json!({
                "buffer_window_seconds": buffer_window_seconds,
            })),
            Self::SessionNotFound(id) | Self::SubscriptionNotFound(id) => {
                Some(serde_json::json!({ "session_id": id }))
            }
            Self::Sdk(forge_sdk::Error::Process { exit_code, .. }) => {
                Some(serde_json::json!({ "exit_code": exit_code }))
            }
            _ => None,
        };
        ErrorObject {
            code: self.code(),
            message: self.to_string(),
            data,
        }
    }
}

/// Map a forge-sdk error variant to its corresponding -321xx JSON-RPC code.
///
/// `forge_sdk::Error` is `#[non_exhaustive]`, so a fallback arm catches
/// future variants and maps them to the generic internal-error code.
fn sdk_code(e: &forge_sdk::Error) -> i32 {
    use forge_sdk::Error as E;
    match e {
        E::CliNotFound { .. } => -32100,
        E::Connection { .. } => -32101,
        E::Process { .. } => -32102,
        E::MessageParse { .. } => -32103,
        E::Encode { .. } => -32104,
        E::JsonDecode { .. } => -32105,
        E::Io(_) => -32106,
        _ => -32603,
    }
}
