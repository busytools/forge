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

/// One inbound JSON-RPC notification not routed to a per-session
/// subscription channel. Returned by [`Client::notifications`] so the
/// app loop can fan `session.role_assigned`, `session.primary_changed`,
/// `session.closed`, `prompts.expired` (etc.) into the right
/// `AppEvent` variants.
#[derive(Debug, Clone)]
pub struct NotificationFrame {
    /// JSON-RPC method name, e.g. `session.role_assigned`.
    pub method: String,
    /// `params` payload as it arrived on the wire.
    pub params: serde_json::Value,
}

/// Reverse-RPC handler dispatch mode.
enum ReverseHandlerKind {
    /// Sync: handler returns the answer; the dispatcher awaits and
    /// auto-replies via `send_response`. Use for hooks that auto-allow.
    Sync(SyncHandlerArc),
    /// Deferred: handler is responsible for arranging the eventual
    /// `send_response` itself (e.g. by surfacing the request to a UI
    /// loop and answering on a keypress).
    Deferred(DeferredHandlerArc),
}

type BoxedSyncFut = std::pin::Pin<Box<dyn std::future::Future<Output = serde_json::Value> + Send>>;
type BoxedDeferredFut = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
type SyncHandlerArc =
    Arc<dyn Fn(serde_json::Value, serde_json::Value) -> BoxedSyncFut + Send + Sync>;
type DeferredHandlerArc =
    Arc<dyn Fn(serde_json::Value, serde_json::Value) -> BoxedDeferredFut + Send + Sync>;

/// Forge-tui WebSocket client.
///
/// Cheap to clone — every field is `Arc`-backed.
#[derive(Clone)]
pub struct Client {
    out_tx: mpsc::UnboundedSender<String>,
    responses: ResponseTable,
    subscriptions: SubscriptionTable,
    reverse_handlers: Arc<Mutex<HashMap<String, ReverseHandlerKind>>>,
    /// Sender side of the notifications channel. Held on the client so
    /// a) every clone keeps the channel alive (without this, dropping
    /// the original `Client` would close the receiver while clones are
    /// still using `client.call()`); b) the field exists for symmetry
    /// with `out_tx` even though the read loop is the only sender.
    #[allow(dead_code, reason = "kept-alive sender; never read directly")]
    notifications_tx: mpsc::UnboundedSender<NotificationFrame>,
    /// One-shot consumer for the receiver. Wrapped so [`Client`] is
    /// `Clone` — only the first call to [`Client::notifications`]
    /// returns Some, subsequent calls return None.
    notifications_rx: Arc<Mutex<Option<mpsc::UnboundedReceiver<NotificationFrame>>>>,
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
        let (notifications_tx, notifications_rx) = mpsc::unbounded_channel::<NotificationFrame>();

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
            notifications_tx.clone(),
        ));

        Ok(Self {
            out_tx,
            responses,
            subscriptions,
            reverse_handlers,
            notifications_tx,
            notifications_rx: Arc::new(Mutex::new(Some(notifications_rx))),
        })
    }

    /// Take ownership of the unrouted-notifications receiver. Callable
    /// once per [`Client`] instance — the second call returns `None`.
    /// Drains [`NotificationFrame`]s for any inbound notification not
    /// routed to a per-session subscription channel
    /// (`session.role_assigned`, `session.primary_changed`,
    /// `session.closed`, `prompts.expired`, etc.).
    #[must_use]
    pub fn notifications(&self) -> Option<mpsc::UnboundedReceiver<NotificationFrame>> {
        self.notifications_rx.lock().take()
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
            // Round 4 — fix M5. Daemon's error envelope must include
            // both `code` and `message` per JSON-RPC 2.0 §5.1. Missing
            // either points at a wire-spec-violating daemon (or a
            // mid-version mismatch) — log so operators can grep.
            // Defaults below preserve forward-progress: callers still
            // get a typed error rather than a panic.
            if err.get("code").is_none() || err.get("message").is_none() {
                tracing::warn!(
                    error = ?err,
                    "daemon error response missing code or message"
                );
            }
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
    /// [`ClientError`] on subscribe call failure. On error, the local
    /// mpsc registration is rolled back so the subscription table
    /// doesn't leak entries pointing at dropped receivers.
    pub async fn subscribe_session(
        &self,
        session_id: &str,
    ) -> Result<tokio_stream::wrappers::UnboundedReceiverStream<serde_json::Value>> {
        let (tx, rx) = mpsc::unbounded_channel::<serde_json::Value>();
        self.subscriptions.lock().insert(session_id.into(), tx);
        match self
            .call::<_, serde_json::Value>(
                "session.subscribe",
                serde_json::json!({"session_id": session_id}),
            )
            .await
        {
            Ok(_) => Ok(tokio_stream::wrappers::UnboundedReceiverStream::new(rx)),
            Err(e) => {
                self.subscriptions.lock().remove(session_id);
                Err(e)
            }
        }
    }

    /// Register a SYNC reverse-RPC handler. The handler computes and
    /// returns the answer; the dispatcher awaits the future and
    /// auto-replies via [`Client::send_response`] using the captured
    /// `rev_id`. Use for hooks that auto-allow (the answer is fully
    /// known at request time, no UI involvement).
    pub fn on_reverse_rpc_sync<F, Fut>(&self, method: impl Into<String>, handler: F)
    where
        F: Fn(serde_json::Value, serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = serde_json::Value> + Send + 'static,
    {
        let h = Arc::new(handler);
        let wrapped = Arc::new(move |rev_id, params| {
            let h = h.clone();
            let fut = (h)(rev_id, params);
            Box::pin(fut)
                as std::pin::Pin<Box<dyn std::future::Future<Output = serde_json::Value> + Send>>
        });
        self.reverse_handlers
            .lock()
            .insert(method.into(), ReverseHandlerKind::Sync(wrapped));
    }

    /// Register a DEFERRED reverse-RPC handler. The handler arranges
    /// the eventual `send_response` itself (e.g. by surfacing the
    /// request to a UI loop and answering on a keypress); the
    /// dispatcher does NOT auto-reply. Use for `permission.request`
    /// where the answer comes from the user, not the handler.
    pub fn on_reverse_rpc_deferred<F, Fut>(&self, method: impl Into<String>, handler: F)
    where
        F: Fn(serde_json::Value, serde_json::Value) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let h = Arc::new(handler);
        let wrapped = Arc::new(move |rev_id, params| {
            let h = h.clone();
            let fut = (h)(rev_id, params);
            Box::pin(fut) as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
        });
        self.reverse_handlers
            .lock()
            .insert(method.into(), ReverseHandlerKind::Deferred(wrapped));
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

#[allow(
    clippy::too_many_lines,
    reason = "round 3 — fix M3 added explicit error logging on the method-not-found serialise path; splitting the dispatcher into helpers obscures the inbound-frame discriminator flow"
)]
async fn read_loop(
    mut stream: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
    responses: ResponseTable,
    subscriptions: SubscriptionTable,
    reverse_handlers: Arc<Mutex<HashMap<String, ReverseHandlerKind>>>,
    out_tx: mpsc::UnboundedSender<String>,
    notifications_tx: mpsc::UnboundedSender<NotificationFrame>,
) {
    while let Some(msg) = stream.next().await {
        let text = match msg {
            Ok(WsMsg::Text(t)) => t,
            Ok(_) => continue,
            Err(e) => {
                tracing::warn!(error = %e, "ws recv failed; stopping read loop");
                break;
            }
        };
        let v: serde_json::Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, line = %text, "ws frame failed to parse as JSON");
                continue;
            }
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
            let handler = reverse_handlers.lock().get(&method).cloned_kind();
            if let Some(kind) = handler {
                match kind {
                    ReverseHandlerKind::Sync(h) => {
                        let out_tx = out_tx.clone();
                        let rev_id_clone = rev_id.clone();
                        tokio::spawn(async move {
                            let answer = (h)(rev_id_clone.clone(), params).await;
                            let resp = serde_json::json!({
                                "jsonrpc": "2.0",
                                "id": rev_id_clone,
                                "result": answer,
                            });
                            match serde_json::to_string(&resp) {
                                Ok(s) => {
                                    if out_tx.send(s).is_err() {
                                        tracing::warn!(
                                            "reverse-RPC sync handler: outbound channel closed; cannot send response"
                                        );
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!(error = %e, "reverse-RPC sync handler: serialize failed");
                                }
                            }
                        });
                    }
                    ReverseHandlerKind::Deferred(h) => {
                        // Fire-and-forget: the handler is responsible
                        // for sending the response back via
                        // `Client::send_response`. We spawn it to keep
                        // the read loop free of blocking work.
                        tokio::spawn(async move {
                            (h)(rev_id, params).await;
                        });
                    }
                }
            } else {
                // No handler — respond with method-not-found.
                let resp = serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": rev_id,
                    "error": { "code": -32601, "message": format!("no handler for {method}") },
                });
                // Round 3 — fix M3. Previously a serialisation Err
                // was silently swallowed via `if let Ok(...)`. Log
                // the warn so the daemon-side timeout path is
                // attributable rather than mysterious. The fallback
                // is to drop the error response — the caller's
                // reverse-RPC will eventually time out on the
                // daemon's side, which is the right deny semantics
                // for security-critical kinds.
                match serde_json::to_string(&resp) {
                    Ok(s) => {
                        if out_tx.send(s).is_err() {
                            tracing::warn!(
                                method = %method,
                                "method-not-found response: outbound channel closed"
                            );
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            method = %method,
                            error = %e,
                            "method-not-found response: serialise failed; dropping"
                        );
                    }
                }
            }
        } else {
            // Notification.
            if method == "session.event" {
                if let Some(sid) = params.get("session_id").and_then(|s| s.as_str()) {
                    let send_result = {
                        let table = subscriptions.lock();
                        table.get(sid).map(|tx| tx.send(params.clone()))
                    };
                    match send_result {
                        Some(Ok(())) => continue,
                        Some(Err(_)) => {
                            // Receiver dropped — purge so future
                            // subscribe attempts get a fresh entry
                            // rather than colliding on the dead one.
                            subscriptions.lock().remove(sid);
                            continue;
                        }
                        None => { /* fall through to notifications channel */ }
                    }
                }
            }
            // Anything else (role_assigned, primary_changed, closed,
            // prompts.expired, …) goes to the Client-wide
            // notifications channel for the app layer to drain.
            let _ = notifications_tx.send(NotificationFrame { method, params });
        }
    }
}

// Helper: clone the inner handler out of a borrow without making
// `ReverseHandlerKind` itself Clone (the inner Arc trait objects
// already provide cheap clones).
trait OptionExt {
    fn cloned_kind(self) -> Option<ReverseHandlerKind>;
}

impl OptionExt for Option<&ReverseHandlerKind> {
    fn cloned_kind(self) -> Option<ReverseHandlerKind> {
        self.map(|k| match k {
            ReverseHandlerKind::Sync(h) => ReverseHandlerKind::Sync(h.clone()),
            ReverseHandlerKind::Deferred(h) => ReverseHandlerKind::Deferred(h.clone()),
        })
    }
}
