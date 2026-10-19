//! One connection to a browser host's devtools endpoint, speaking just enough
//! CDP for the surface's input: numbered calls, replies matched by id, and
//! the endpoint's events delivered on their own channel.
//!
//! The window moves this module used to carry retired with the machine's own
//! browser: a windowless host has no window to place, and the surface is
//! drawn by the client at the takeover page's rect.
//!
//! **The read half runs on its own task.** Input sends without waiting for a
//! reply, but the page's own events - a file chooser opening, above all -
//! arrive while no call is pending, so somebody has to be reading the socket
//! between calls.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::stream::SplitSink;
use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;
type Replies = Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>;

pub struct Cdp {
    sink: SplitSink<Socket, Message>,
    replies: Replies,
    next_id: u64,
    /// Raised when this handle is dropped, so the reader task ends instead of
    /// outliving the surface that opened it.
    closed: Arc<Notify>,
}

impl Drop for Cdp {
    fn drop(&mut self) {
        self.closed.notify_one();
    }
}

impl Cdp {
    /// Send one call without waiting for its reply.
    ///
    /// **Input is ordered by the socket, not by the replies.** Awaiting each
    /// event's answer puts a round trip in front of every keystroke and wheel
    /// tick, which is exactly what a page feels as lag; the reply to an
    /// `Input.*` call carries nothing a page needs.
    pub async fn send(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.next_id += 1;
        let message = json!({ "id": self.next_id, "method": method, "params": params });
        self.sink
            .send(Message::Text(message.to_string().into()))
            .await
            .map_err(|why| format!("{method} could not be sent: {why}"))
    }

    /// Connect to the endpoint a host's page target names, answering the
    /// connection and the endpoint's events (every frame without an id).
    pub async fn connect_watch(
        port: u16,
        path: &str,
    ) -> Result<(Self, mpsc::UnboundedReceiver<Value>), String> {
        let url = format!("ws://127.0.0.1:{port}{path}");
        let (socket, _) = tokio_tungstenite::connect_async(&url).await.map_err(|why| {
            format!("the browser's devtools endpoint at {url} did not accept: {why}")
        })?;
        let (sink, mut source) = socket.split();
        let replies: Replies = Arc::new(Mutex::new(HashMap::new()));
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let closed = Arc::new(Notify::new());
        let reader_replies = Arc::clone(&replies);
        let reader_closed = Arc::clone(&closed);
        tauri::async_runtime::spawn(async move {
            loop {
                let frame = tokio::select! {
                    frame = source.next() => frame,
                    () = reader_closed.notified() => break,
                };
                let Some(Ok(Message::Text(text))) = frame else {
                    if frame.is_none() {
                        break;
                    }
                    continue;
                };
                let Ok(message) = serde_json::from_str::<Value>(&text) else { continue };
                match message.get("id").and_then(Value::as_u64) {
                    Some(id) => {
                        let waiter = reader_replies
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&id);
                        if let Some(waiter) = waiter {
                            let _ = waiter.send(message);
                        }
                    }
                    None => {
                        if events_tx.send(message).is_err() {
                            break;
                        }
                    }
                }
            }
            // The endpoint closed, or the surface did: every call still
            // waiting is answered as closed rather than left hanging.
            reader_replies.lock().unwrap_or_else(PoisonError::into_inner).clear();
        });
        Ok((Self { sink, replies, next_id: 0, closed }, events_rx))
    }

    /// One call, answered: the reply's `result`, or the protocol error's own
    /// sentence. The reader task matches the id; events go to the watcher.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let (waiter, answer) = oneshot::channel();
        self.replies.lock().unwrap_or_else(PoisonError::into_inner).insert(id, waiter);
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_owned());
        }
        if let Err(why) = self.sink.send(Message::Text(message.to_string().into())).await {
            self.replies.lock().unwrap_or_else(PoisonError::into_inner).remove(&id);
            return Err(format!("{method} could not be sent: {why}"));
        }
        let Ok(reply) = answer.await else {
            return Err(format!("{method}: the devtools endpoint closed before it answered"));
        };
        match reply.get("error") {
            Some(error) => Err(format!(
                "{method}: {}",
                error.get("message").and_then(Value::as_str).unwrap_or("refused")
            )),
            None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
        }
    }
}
