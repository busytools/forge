//! WebSocket connection to forge-daemon.
//!
//! Replaces upstream's Node.js subprocess bridge entirely. The
//! `DaemonConnection` owns the WS stream + a reader task that demuxes
//! incoming frames into JSON-RPC responses (id-keyed `pending` map)
//! and notifications (forwarded to the agent's reader). Outbound
//! frames are encoded by `call` / `notify` and sent through a
//! per-connection writer task so multiple call sites can share the
//! same socket.
//!
//! The TUI talks to this layer through [`crate::agent::client::BridgeClient`]
//! which translates upstream's `BridgeCommand` enum into daemon
//! JSON-RPC method calls.

#![allow(clippy::missing_errors_doc, clippy::needless_pass_by_value)]

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

/// Default daemon WS endpoint. Override with `FORGE_DAEMON_URL` env var.
pub const DEFAULT_DAEMON_URL: &str = "ws://127.0.0.1:7373/";

/// Resolved daemon URL — env override wins.
#[must_use]
pub fn resolve_daemon_url() -> String {
    std::env::var("FORGE_DAEMON_URL").unwrap_or_else(|_| DEFAULT_DAEMON_URL.to_owned())
}

/// One JSON-RPC notification observed on the inbound WS stream.
/// `id` is `None` for notifications (server→client) and `Some(_)` for
/// JSON-RPC requests the daemon issues to us (reverse-RPC for
/// `permission.request` / `session.question_request` / `hook.*`).
#[derive(Debug, Clone)]
pub struct InboundEvent {
    /// `None` for fire-and-forget notifications, `Some(Value)` for
    /// reverse-RPC requests that need a Response back to the daemon.
    pub id: Option<Value>,
    /// Method name, e.g. `"session.event"` or `"permission.request"`.
    pub method: String,
    /// Method params (whatever JSON shape the method ships).
    pub params: Value,
}

type WsWrite = SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMsg>;
type WsRead = SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>;
type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, JsonRpcErr>>>>>;

/// JSON-RPC error returned by the daemon.
#[derive(Debug, Clone)]
pub struct JsonRpcErr {
    /// JSON-RPC error code (e.g. -32601 for method-not-found).
    pub code: i64,
    /// Human-readable error message.
    pub message: String,
    /// Optional structured details.
    pub data: Option<Value>,
}

impl std::fmt::Display for JsonRpcErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "json-rpc error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcErr {}

/// Connection to forge-daemon over WebSocket. Owns one writer task
/// (drains an mpsc of outbound `WsMsg`s) and one reader task (parses
/// inbound frames into `InboundEvent`s + JSON-RPC responses).
pub struct DaemonConnection {
    write_tx: mpsc::UnboundedSender<WsMsg>,
    pending: Pending,
}

impl DaemonConnection {
    /// Connect to the daemon and spawn reader + writer tasks.
    /// Returns the connection handle and a receiver for inbound
    /// notifications + reverse-RPC requests; the `BridgeClient` drains
    /// this receiver and translates events into the TUI's
    /// [`crate::agent::wire::EventEnvelope`].
    pub async fn connect(url: &str) -> Result<(Self, mpsc::UnboundedReceiver<InboundEvent>)> {
        let (ws, _resp) = connect_async(url)
            .await
            .with_context(|| format!("forge-daemon ws connect: {url}"))?;
        let (write, read) = ws.split();

        let (write_tx, write_rx) = mpsc::unbounded_channel::<WsMsg>();
        let (events_tx, events_rx) = mpsc::unbounded_channel::<InboundEvent>();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        tokio::spawn(writer_loop(write, write_rx));
        tokio::spawn(reader_loop(read, events_tx, Arc::clone(&pending)));

        Ok((Self { write_tx, pending }, events_rx))
    }

    /// JSON-RPC call: send a request and await the matching response.
    /// Returns the `result` Value on success, a `JsonRpcErr` on the
    /// `error` path, or an anyhow error on transport failure.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let id = format!("tui_{}", Uuid::new_v4());
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id.clone(), tx);

        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.write_tx
            .send(WsMsg::Text(serde_json::to_string(&req)?))
            .map_err(|_| anyhow::anyhow!("forge-daemon writer closed"))?;

        match rx.await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(err)) => Err(anyhow::Error::new(err)),
            Err(_) => Err(anyhow::anyhow!(
                "forge-daemon connection closed before reply"
            )),
        }
    }

    /// JSON-RPC reply: respond to a reverse-RPC request the daemon
    /// issued (`permission.request`, `session.question_request`, `hook.*`).
    /// `id` is whatever the daemon supplied on the original Request.
    pub fn reply(&self, id: Value, result: Value) -> Result<()> {
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        self.write_tx
            .send(WsMsg::Text(serde_json::to_string(&resp)?))
            .map_err(|_| anyhow::anyhow!("forge-daemon writer closed"))
    }

    /// JSON-RPC error reply for a reverse-RPC the TUI couldn't satisfy.
    pub fn reply_error(&self, id: Value, code: i64, message: &str) -> Result<()> {
        let resp = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        self.write_tx
            .send(WsMsg::Text(serde_json::to_string(&resp)?))
            .map_err(|_| anyhow::anyhow!("forge-daemon writer closed"))
    }
}

async fn writer_loop(mut write: WsWrite, mut rx: mpsc::UnboundedReceiver<WsMsg>) {
    while let Some(msg) = rx.recv().await {
        if let Err(err) = write.send(msg).await {
            tracing::warn!(error = %err, "forge-daemon ws writer: send failed; closing");
            break;
        }
    }
    let _ = write.close().await;
}

async fn reader_loop(
    mut read: WsRead,
    events_tx: mpsc::UnboundedSender<InboundEvent>,
    pending: Pending,
) {
    while let Some(msg) = read.next().await {
        let Ok(msg) = msg else {
            tracing::warn!("forge-daemon ws reader: recv error; closing");
            break;
        };
        let WsMsg::Text(text) = msg else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            tracing::warn!("forge-daemon ws reader: non-JSON frame dropped");
            continue;
        };

        // Demux: requests (id + method) vs notifications (no id, has method)
        // vs responses (id, no method).
        let id = value.get("id").cloned();
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_owned);
        match (method, id) {
            (Some(method), Some(id)) => {
                // Reverse-RPC request from the daemon.
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                if events_tx
                    .send(InboundEvent {
                        id: Some(id),
                        method,
                        params,
                    })
                    .is_err()
                {
                    break;
                }
            }
            (Some(method), None) => {
                // Notification.
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                if events_tx
                    .send(InboundEvent {
                        id: None,
                        method,
                        params,
                    })
                    .is_err()
                {
                    break;
                }
            }
            (None, Some(id)) => {
                // Response to one of our outbound calls.
                let id_str = match id.as_str() {
                    Some(s) => s.to_owned(),
                    None => continue,
                };
                let Some(tx) = pending.lock().remove(&id_str) else {
                    tracing::warn!(id = %id_str, "forge-daemon ws reader: unknown response id");
                    continue;
                };
                if let Some(err) = value.get("error") {
                    let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
                    let message = err
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    let data = err.get("data").cloned();
                    let _ = tx.send(Err(JsonRpcErr {
                        code,
                        message,
                        data,
                    }));
                } else {
                    let result = value.get("result").cloned().unwrap_or(Value::Null);
                    let _ = tx.send(Ok(result));
                }
            }
            (None, None) => {
                tracing::warn!("forge-daemon ws reader: malformed frame (no id, no method)");
            }
        }
    }
}
