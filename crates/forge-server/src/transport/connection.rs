//! One connection: the upgrade, what the server says first, and the loop
//! that answers what a client asks for.

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::StreamExt;

use super::TransportState;
use super::envelope::{ClientMessage, ClientSettings, ServerMessage, Subject};
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

/// Read what a client sends, and hear what its subscriptions asked for.
///
/// Every client message is answered: a client is never left waiting on a
/// message this server chose to drop, which is the failure that reads as a
/// hang rather than as an error.
async fn drive(socket: &mut WebSocket, state: &TransportState) -> anyhow::Result<()> {
    // This socket's own stream, handed over by the surface. Every caller
    // gets one, so a second client attaches beside the first rather than
    // stealing its events, and dropping the socket drops this with it -
    // which is what keeps a subscription from outliving its connection.
    let mut updates = state.surface.subscribe();
    let mut watched: Vec<Subject> = Vec::new();

    loop {
        tokio::select! {
            msg = socket.next() => {
                let Some(msg) = msg else { break };   // the client went away
                handle_client(socket, state, &mut watched, msg?).await?;
            }
            // Deliberately NOT `Some(update) = updates.recv()`. A pattern that stops matching
            // DISABLES its branch in `select!` rather than ending the loop, so a closed channel
            // would silently stop delivering while the socket stayed open.
            heard = updates.recv() => {
                let Some(update) = heard else { break };
                if watched.iter().any(|what| what.covers(&update)) {
                    send(socket, ServerMessage::Update { update: Box::new(update) }).await?;
                }
            }
        }
    }
    Ok(())
}

/// Answer one client message.
async fn handle_client(
    socket: &mut WebSocket,
    state: &TransportState,
    watched: &mut Vec<Subject>,
    msg: Message,
) -> anyhow::Result<()> {
    let Message::Text(text) = msg else {
        return Ok(());
    };
    let Ok(client) = serde_json::from_str::<ClientMessage>(&text) else {
        return send(
            socket,
            ServerMessage::Error {
                what: "client_message".to_owned(),
                why: "that is not a message this server knows".to_owned(),
            },
        )
        .await;
    };
    match client {
        ClientMessage::Subscribe { what } => match encode_subject(state, &what).await {
            Ok(data) => {
                // Watched only once the subject is one this server can
                // answer for: a refused subscribe leaves nothing to hear.
                watched.push(what.clone());
                send(socket, ServerMessage::Snapshot { subject: what, data }).await
            }
            // A seat nobody has started is an ANSWER rather than a
            // silence: the client learns why, and never draws an empty
            // snapshot as a broken page.
            Err(refusal) => {
                send(
                    socket,
                    ServerMessage::Error { what: "subscribe".to_owned(), why: refusal.to_string() },
                )
                .await
            }
        },
        // Unsubscribe, Command and More arrive in Tasks 7 and 8. Until then
        // they answer rather than being dropped, so a client is never left
        // waiting on one.
        other => {
            send(
                socket,
                ServerMessage::Error {
                    what: "not_yet".to_owned(),
                    why: format!("this server does not serve {other:?} yet"),
                },
            )
            .await
        }
    }
}

async fn send(socket: &mut WebSocket, message: ServerMessage) -> anyhow::Result<()> {
    let text = serde_json::to_string(&message)?;
    socket.send(Message::Text(text.into())).await?;
    Ok(())
}
