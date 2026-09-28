//! One connection: the upgrade, what the server says first, and the loop
//! that answers what a client asks for.

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::StreamExt;

use super::TransportState;
use super::envelope::{ClientMessage, ClientSettings, ServerMessage};
use super::wire::encode_subject;

/// The protocol this server speaks.
///
/// Fixed rather than negotiated: the server and the core change far more
/// slowly than a client's visuals do, so a client either speaks this or it
/// does not, and a mismatch fails plainly instead of silently.
const PROTOCOL_VERSION: u32 = 1;

/// Take the upgrade and give the connection its own task.
pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<Arc<TransportState>>) -> Response {
    ws.on_upgrade(move |socket| greet(socket, state))
}

/// Greet, then serve the client until it goes.
///
/// The greeting is what tells a client it is speaking to a forge rather
/// than to whatever else holds the port, and it carries the settings the
/// client draws with - so the client is configured before it draws anything
/// rather than discovering a theme later.
async fn greet(mut socket: WebSocket, state: Arc<TransportState>) {
    let greeting = ServerMessage::Greeting {
        version: PROTOCOL_VERSION,
        settings: ClientSettings::from(&state.config),
    };
    let Ok(text) = serde_json::to_string(&greeting) else {
        tracing::error!(
            target: "forge_server::transport",
            event_name = "greeting_unencodable",
            "the greeting could not be encoded, so this connection has nothing to say",
        );
        return;
    };
    if socket.send(Message::Text(text.into())).await.is_err() {
        return;
    }
    if let Err(error) = drive(&mut socket, &state).await {
        tracing::debug!(
            target: "forge_server::transport",
            event_name = "connection_ended",
            %error,
            "the connection ended on an error rather than on the client closing",
        );
    }
}

/// Read what a client sends, and answer every message it sends.
///
/// Every arm answers: a client is never left waiting on a message this
/// server chose to drop, which is the failure that reads as a hang rather
/// than as an error.
async fn drive(socket: &mut WebSocket, state: &TransportState) -> anyhow::Result<()> {
    while let Some(msg) = socket.next().await {
        let Message::Text(text) = msg? else {
            continue;
        };
        let Ok(client) = serde_json::from_str::<ClientMessage>(&text) else {
            send(
                socket,
                ServerMessage::Error {
                    what: "client_message".to_owned(),
                    why: "that is not a message this server knows".to_owned(),
                },
            )
            .await?;
            continue;
        };
        match client {
            ClientMessage::Subscribe { what } => match encode_subject(state, &what).await {
                Ok(data) => {
                    send(socket, ServerMessage::Snapshot { subject: what, data }).await?;
                }
                // A seat nobody has started is an ANSWER rather than a
                // silence: the client learns why, and never draws an empty
                // snapshot as a broken page.
                Err(refusal) => {
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "subscribe".to_owned(),
                            why: refusal.to_string(),
                        },
                    )
                    .await?;
                }
            },
            // Unsubscribe, Command and More arrive in Tasks 6, 7 and 8.
            // Until then they answer rather than being dropped, so a client
            // is never left waiting on one.
            other => {
                send(
                    socket,
                    ServerMessage::Error {
                        what: "not_yet".to_owned(),
                        why: format!("this server does not serve {other:?} yet"),
                    },
                )
                .await?;
            }
        }
    }
    Ok(())
}

async fn send(socket: &mut WebSocket, message: ServerMessage) -> anyhow::Result<()> {
    let text = serde_json::to_string(&message)?;
    socket.send(Message::Text(text.into())).await?;
    Ok(())
}
