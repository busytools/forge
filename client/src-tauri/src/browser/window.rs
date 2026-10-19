//! One connection to a browser host's devtools endpoint, speaking just enough
//! CDP for the surface's input: numbered calls, replies matched by id, the
//! frames between them skipped.
//!
//! The window moves this module used to carry retired with the machine's own
//! browser: a windowless host has no window to place, and the surface is
//! drawn by the client at the column's rect.

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio_tungstenite::tungstenite::Message;

pub struct Cdp {
    stream: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    next_id: u64,
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
        self.stream
            .send(Message::Text(message.to_string().into()))
            .await
            .map_err(|why| format!("{method} could not be sent: {why}"))
    }

    /// Connect to the endpoint a host's page target names.
    pub async fn connect(port: u16, path: &str) -> Result<Self, String> {
        let url = format!("ws://127.0.0.1:{port}{path}");
        let (stream, _) = tokio_tungstenite::connect_async(&url).await.map_err(|why| {
            format!("the browser's devtools endpoint at {url} did not accept: {why}")
        })?;
        Ok(Self { stream, next_id: 0 })
    }

    /// One call, answered: the reply's `result`, or the protocol error's own
    /// sentence. Frames that are not this call's reply are skipped - events
    /// and other replies do not concern a caller that asked one question.
    pub async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        let mut message = json!({ "id": id, "method": method, "params": params });
        if let Some(session) = session {
            message["sessionId"] = Value::String(session.to_owned());
        }
        self.stream
            .send(Message::Text(message.to_string().into()))
            .await
            .map_err(|why| format!("{method} could not be sent: {why}"))?;
        loop {
            let frame = self
                .stream
                .next()
                .await
                .ok_or_else(|| {
                    format!("{method}: the devtools endpoint closed before it answered")
                })?
                .map_err(|why| format!("{method}: the devtools endpoint failed: {why}"))?;
            let Message::Text(text) = frame else { continue };
            let Ok(reply) = serde_json::from_str::<Value>(&text) else { continue };
            if reply.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            return match reply.get("error") {
                Some(error) => Err(format!(
                    "{method}: {}",
                    error.get("message").and_then(Value::as_str).unwrap_or("refused")
                )),
                None => Ok(reply.get("result").cloned().unwrap_or(Value::Null)),
            };
        }
    }
}
