//! WS+JSON-RPC dispatcher for the TUI side.
//!
//! Tracks outstanding request ids in a `HashMap`, dispatches inbound
//! responses to oneshot waiters, routes inbound notifications to
//! per-session subscription channels, and routes reverse-RPC requests
//! to user-supplied handlers.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message as WsMsg;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

/// Errors surfaced by the WS+JSON-RPC client.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClientError {
    /// Underlying WebSocket handshake or transport setup failed.
    #[error("connect failed: {0}")]
    Connect(String),
    /// Outbound write failed (channel closed, serialisation failure, …).
    #[error("send failed: {0}")]
    Send(String),
    /// Inbound read failed (transport closed or framing error).
    #[error("recv failed: {0}")]
    Recv(String),
    /// The daemon returned a JSON-RPC error response.
    #[error("daemon error code {code}: {message}")]
    Daemon {
        /// JSON-RPC error code (negative integer per spec).
        code: i32,
        /// Human-readable error message from the daemon.
        message: String,
    },
    /// Response payload could not be deserialised into the expected type.
    #[error("malformed response: {0}")]
    Malformed(String),
    /// The client's outbound channel is closed (transport gone).
    #[error("client closed")]
    Closed,
}

/// Convenience alias for client operation results.
pub type Result<T> = std::result::Result<T, ClientError>;

type ResponseTable = Arc<Mutex<HashMap<String, oneshot::Sender<serde_json::Value>>>>;
type SubscriptionTable = Arc<Mutex<HashMap<String, mpsc::UnboundedSender<serde_json::Value>>>>;

/// Reverse-RPC handler. Receives `(rev_id, params)` and returns a
/// future that yields the response value to send back.
///
/// The id is forwarded so the handler (or whatever it forwards the
/// request to) can later answer asynchronously via
/// [`Client::send_response`] — useful for surfacing requests through a
/// UI loop where the answer comes from a user keypress, not from the
/// handler's own future.
pub type ReverseRpcHandler = Arc<
    dyn Fn(serde_json::Value, serde_json::Value) -> tokio::task::JoinHandle<serde_json::Value>
        + Send
        + Sync,
>;

/// Forge-tui WebSocket client.
///
/// Cheap to clone — every field is `Arc`-backed.
#[derive(Clone)]
pub struct Client {
    out_tx: mpsc::UnboundedSender<String>,
    responses: ResponseTable,
    subscriptions: SubscriptionTable,
    reverse_handlers: Arc<Mutex<HashMap<String, ReverseRpcHandler>>>,
}

impl Client {
    /// Connect to a forged daemon at `url` (e.g. `ws://127.0.0.1:7373/`
    /// or `wss://forged.example.com/`).
    ///
    /// # Errors
    ///
    /// [`ClientError::Connect`] on handshake failure.
    pub async fn connect(url: &str) -> Result<Self> {
        let (ws, _resp) = connect_async(url)
            .await
            .map_err(|e| ClientError::Connect(e.to_string()))?;
        let (out_tx, out_rx) = mpsc::unbounded_channel::<String>();
        let responses: ResponseTable = Arc::new(Mutex::new(HashMap::new()));
        let subscriptions: SubscriptionTable = Arc::new(Mutex::new(HashMap::new()));
        let reverse_handlers = Arc::new(Mutex::new(HashMap::new()));

        let (sink, stream) = ws.split();

        // Outbound writer task.
        tokio::spawn(write_loop(sink, out_rx));

        // Inbound dispatcher task.
        tokio::spawn(read_loop(
            stream,
            responses.clone(),
            subscriptions.clone(),
            reverse_handlers.clone(),
            out_tx.clone(),
        ));

        Ok(Self {
            out_tx,
            responses,
            subscriptions,
            reverse_handlers,
        })
    }

    /// Issue a JSON-RPC request and await the result, deserialised as `R`.
    ///
    /// # Errors
    ///
    /// Wire / serialisation / daemon-side errors all surface as
    /// [`ClientError`].
    pub async fn call<P: Serialize, R: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<R> {
        let id = format!("req_{}", Uuid::new_v4());
        let req = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let (tx, rx) = oneshot::channel();
        self.responses.lock().insert(id.clone(), tx);
        let body = serde_json::to_string(&req).map_err(|e| ClientError::Send(e.to_string()))?;
        self.out_tx.send(body).map_err(|_| ClientError::Closed)?;
        let value = rx.await.map_err(|_| ClientError::Closed)?;
        // value is a serde_json::Value of either { result: ... } or { error: ... }
        if let Some(err) = value.get("error") {
            let code = i32::try_from(
                err.get("code")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(-1),
            )
            .unwrap_or(-1);
            let message = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            return Err(ClientError::Daemon { code, message });
        }
        let result = value
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        serde_json::from_value(result).map_err(|e| ClientError::Malformed(e.to_string()))
    }

    /// Subscribe to a session's events. Returns a stream that yields
    /// `session.event.params` payloads — `{ session_id, event_id, message }`.
    ///
    /// # Errors
    ///
    /// [`ClientError`] on subscribe call failure.
    pub async fn subscribe_session(
        &self,
        session_id: &str,
    ) -> Result<tokio_stream::wrappers::UnboundedReceiverStream<serde_json::Value>> {
        let (tx, rx) = mpsc::unbounded_channel::<serde_json::Value>();
        self.subscriptions.lock().insert(session_id.into(), tx);
        let _: serde_json::Value = self
            .call(
                "session.subscribe",
                serde_json::json!({"session_id": session_id}),
            )
            .await?;
        Ok(tokio_stream::wrappers::UnboundedReceiverStream::new(rx))
    }

    /// Register a reverse-RPC handler.
    ///
    /// `method` is the full method name (e.g. `permission.request`,
    /// `hook.pre_tool_use`). The handler is invoked with `(rev_id, params)`
    /// — the `rev_id` is forwarded so handlers wanting to defer the answer
    /// (e.g. into a UI loop) can record it and later call
    /// [`Client::send_response`] from elsewhere; handlers that compute the
    /// answer synchronously can return it directly from the future.
    pub fn on_reverse_rpc<F, Fut>(&self, method: impl Into<String>, handler: F)
    where
        F: Fn(serde_json::Value, serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = serde_json::Value> + Send + 'static,
    {
        let h = Arc::new(handler);
        let wrapped: ReverseRpcHandler = Arc::new(
            move |rev_id: serde_json::Value, params: serde_json::Value| {
                let h = h.clone();
                tokio::spawn(async move { (h)(rev_id, params).await })
            },
        );
        self.reverse_handlers.lock().insert(method.into(), wrapped);
    }

    /// Send a JSON-RPC response for a previously-received reverse-RPC
    /// request. Used when the answer is produced asynchronously by a
    /// different task than the one that received the original request
    /// (e.g. by a UI keypress handler).
    ///
    /// # Errors
    ///
    /// [`ClientError::Send`] on serialisation failure;
    /// [`ClientError::Closed`] if the outbound channel is gone.
    pub fn send_response(
        &self,
        rev_id: serde_json::Value,
        result: serde_json::Value,
    ) -> Result<()> {
        let mut resp = serde_json::Map::with_capacity(3);
        resp.insert("jsonrpc".into(), serde_json::Value::String("2.0".into()));
        resp.insert("id".into(), rev_id);
        resp.insert("result".into(), result);
        let s = serde_json::to_string(&serde_json::Value::Object(resp))
            .map_err(|e| ClientError::Send(e.to_string()))?;
        self.out_tx.send(s).map_err(|_| ClientError::Closed)?;
        Ok(())
    }
}

async fn write_loop(
    mut sink: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, WsMsg>,
    mut rx: mpsc::UnboundedReceiver<String>,
) {
    while let Some(text) = rx.recv().await {
        if sink.send(WsMsg::Text(text)).await.is_err() {
            break;
        }
    }
    let _ = sink.close().await;
}

async fn read_loop(
    mut stream: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    responses: ResponseTable,
    subscriptions: SubscriptionTable,
    reverse_handlers: Arc<Mutex<HashMap<String, ReverseRpcHandler>>>,
    out_tx: mpsc::UnboundedSender<String>,
) {
    while let Some(msg) = stream.next().await {
        let Ok(WsMsg::Text(text)) = msg else { continue };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Response (no method, has id, has result|error).
        if v.get("method").is_none() && v.get("id").is_some() {
            let id = v
                .get("id")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_string();
            if let Some(tx) = responses.lock().remove(&id) {
                let _ = tx.send(v);
            }
            continue;
        }

        // Notification or reverse-RPC.
        let method = v
            .get("method")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let params = v.get("params").cloned().unwrap_or(serde_json::Value::Null);

        if v.get("id").is_some() {
            // Reverse-RPC request from the daemon.
            let rev_id = v.get("id").cloned().unwrap_or(serde_json::Value::Null);
            let handler = reverse_handlers.lock().get(&method).cloned();
            if let Some(h) = handler {
                // Fire-and-forget: the handler is responsible for sending the
                // response back via `Client::send_response`. We still spawn it
                // to keep the read loop free of blocking work.
                drop(h(rev_id, params));
            } else {
                // No handler — respond with method-not-found.
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": rev_id,
                    "error": { "code": -32601, "message": format!("no handler for {method}") },
                });
                if let Ok(s) = serde_json::to_string(&resp) {
                    let _ = out_tx.send(s);
                }
            }
        } else {
            // Notification.
            if method == "session.event" {
                if let Some(sid) = params.get("session_id").and_then(|s| s.as_str()) {
                    if let Some(tx) = subscriptions.lock().get(sid) {
                        let _ = tx.send(params);
                    }
                }
            }
            // Other notifications are dropped here; the App layer can
            // observe them via separate channels (added in the app loop).
        }
    }
}
