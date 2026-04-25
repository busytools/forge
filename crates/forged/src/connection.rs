//! Per-WebSocket-connection state + outbound frame channel.

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
    /// Display name registered via `client.register` (M3+).
    pub name: Option<String>,
    /// Outbound channel — anything written here goes to the client.
    pub outbound: mpsc::UnboundedSender<Outbound>,
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
