//! Per-WebSocket-connection state + outbound frame channel.

use std::time::SystemTime;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::jsonrpc::{Notification, Request, Response};

/// A connected client. One instance per WS connection.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Connection {
    /// Stable identifier (`conn_<uuid>`) returned by the initial `client.identify` notification.
    pub id: ConnectionId,
    /// Friendly display name supplied by the client through the `?name=`
    /// query parameter on the WS handshake. `None` when the client did
    /// not provide one.
    pub name: Option<String>,
    /// ISO-8601 timestamp (`YYYY-MM-DDTHH:MM:SSZ`) recording when the WS
    /// handshake completed. Surfaced to clients via `session.peers`.
    pub connected_at_iso: String,
    /// Outbound channel — anything written here goes to the client.
    pub outbound: mpsc::UnboundedSender<Outbound>,
}

impl Connection {
    /// Construct a connection with the given id and outbound channel,
    /// no display name yet, and the current wall-clock time as the
    /// connection-established timestamp.
    #[must_use]
    pub fn new(id: ConnectionId, outbound: mpsc::UnboundedSender<Outbound>) -> Self {
        Self::with_metadata(id, None, SystemTime::now(), outbound)
    }

    /// Construct a connection with caller-supplied metadata. Used by the
    /// WS handshake path (which carries the parsed `?name=` value and a
    /// fresh `SystemTime::now()`) and by tests that want deterministic
    /// timestamps.
    #[must_use]
    pub fn with_metadata(
        id: ConnectionId,
        name: Option<String>,
        connected_at: SystemTime,
        outbound: mpsc::UnboundedSender<Outbound>,
    ) -> Self {
        Self {
            id,
            name,
            connected_at_iso: crate::iso8601::format_iso8601(connected_at),
            outbound,
        }
    }
}

/// Stable identifier for a [`Connection`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionId(pub String);

impl ConnectionId {
    /// Generate a fresh connection id of the form `conn_<uuid>`.
    #[must_use]
    pub fn new() -> Self {
        Self(format!("conn_{}", Uuid::new_v4()))
    }
}

impl Default for ConnectionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Outbound frame on a connection — request (reverse-RPC), response, or notification.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum Outbound {
    /// Server-initiated request (reverse-RPC, e.g. `tool.use_request` in M3+).
    Request(Request),
    /// Response to a client request.
    Response(Response),
    /// Server-initiated notification (e.g. `client.identify`).
    Notification(Notification),
}

impl Outbound {
    /// Render to a WS-text payload.
    ///
    /// # Errors
    ///
    /// Surfaces any `serde_json` encode failure on the wrapped frame.
    pub fn to_text(&self) -> Result<String, serde_json::Error> {
        match self {
            Self::Request(r) => serde_json::to_string(r),
            Self::Response(r) => serde_json::to_string(r),
            Self::Notification(n) => serde_json::to_string(n),
        }
    }
}
