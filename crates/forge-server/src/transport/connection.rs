//! One connection: the upgrade, and what the server says first.

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;

use super::TransportState;

/// What the socket says before anything else.
///
/// A client that hears this knows it is speaking to a forge rather than to
/// whatever else holds the port, which is the one thing a bare
/// `101 Switching Protocols` cannot tell it.
const GREETING: &str = r#"{"kind":"greeting","server":"forge"}"#;

/// Take the upgrade and give the connection its own task.
pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<Arc<TransportState>>) -> Response {
    ws.on_upgrade(move |socket| greet(socket, state))
}

/// Greet, and hold the socket until the client goes.
async fn greet(mut socket: WebSocket, _state: Arc<TransportState>) {
    let _ = socket.send(Message::Text(GREETING.into())).await;
    while let Some(Ok(_)) = socket.recv().await {}
}
