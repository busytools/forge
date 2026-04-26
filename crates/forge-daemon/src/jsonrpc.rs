//! JSON-RPC 2.0 framing types.
//!
//! Wire shape per the spec at §7.4.1.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 2.0 request. Either client→server or reverse-RPC server→client;
/// both directions use the same shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// Always serialised as the literal string `"2.0"`.
    pub jsonrpc: JsonRpcVersion,
    /// Caller-chosen identifier matching this request to its response.
    pub id: Value,
    /// Method namespace + name, e.g. `daemon.status`.
    pub method: String,
    /// Method-specific parameters; absent when the method takes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Request {
    /// Construct a JSON-RPC 2.0 request with the given method, params, and id.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value, id: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2_0,
            id,
            method: method.into(),
            params: Some(params),
        }
    }
}

/// JSON-RPC 2.0 notification (no `id`, no response expected).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    /// Always serialised as the literal string `"2.0"`.
    pub jsonrpc: JsonRpcVersion,
    /// Method namespace + name, e.g. `client.identify`.
    pub method: String,
    /// Method-specific parameters; absent when the method takes none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

impl Notification {
    /// Construct a JSON-RPC 2.0 notification with the given method and params.
    #[must_use]
    pub fn new(method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2_0,
            method: method.into(),
            params: Some(params),
        }
    }
}

/// JSON-RPC 2.0 response — either success (`result`) or failure (`error`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    /// Always serialised as the literal string `"2.0"`.
    pub jsonrpc: JsonRpcVersion,
    /// Echoes the `id` of the originating request.
    pub id: Value,
    /// Present iff the request succeeded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    /// Present iff the request failed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
    /// Construct a successful response carrying `result`.
    #[must_use]
    pub fn success(id: Value, result: Value) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2_0,
            id,
            result: Some(result),
            error: None,
        }
    }

    /// Construct a failure response carrying `err`.
    #[must_use]
    pub fn error(id: Value, err: ErrorObject) -> Self {
        Self {
            jsonrpc: JsonRpcVersion::V2_0,
            id,
            result: None,
            error: Some(err),
        }
    }
}

/// JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObject {
    /// Numeric error code per the wire spec §7.4.15.
    pub code: i32,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured payload — usually omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Marker type that always serialises as the literal string `"2.0"`.
/// Modeled as a single-variant enum so serde derives the
/// strict-validation behaviour (rejects anything but "2.0" on
/// deserialize) for free.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub enum JsonRpcVersion {
    /// JSON-RPC 2.0 — the only version we speak.
    #[default]
    #[serde(rename = "2.0")]
    V2_0,
}
