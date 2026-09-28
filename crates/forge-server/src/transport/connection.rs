//! One connection: the upgrade, what the server says first, and the loop
//! that answers what a client asks for.

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::StreamExt;
use tokio::sync::oneshot;

use super::TransportState;
use super::envelope::{ClientMessage, ClientSettings, ServerMessage, Subject};
use super::wire::encode_subject;
use crate::Command;

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
        ClientMessage::Command { command, reply_to } => {
            dispatch(socket, state, *command, reply_to).await
        }
        // Unsubscribe and More arrive in Task 8. Until then they answer
        // rather than being dropped, so a client is never left waiting on
        // one.
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

/// Dispatch one command, and answer a client that asked for an answer.
///
/// Success sends nothing extra: the update arrives through the subscription
/// the client already has, which is why a client subscribes before it acts.
async fn dispatch(
    socket: &mut WebSocket,
    state: &TransportState,
    command: Command,
    reply_to: Option<u64>,
) -> anyhow::Result<()> {
    // A view acts through the same facade it reads through, and a refusal is
    // the core's own sentence rather than this layer's opinion of it.
    //
    // The four reply-carrying commands take a sender the wire deliberately
    // does not carry, and each carries its OWN reply type - a spawned worker
    // answers with a `WorkerSpawnReply`, a despawn with a `DespawnResult`,
    // and the review pair with a `bool` and a `ReviewSet`. So the channel is
    // built per arm and handed to the shared helper with its own receiver:
    // one channel cannot carry four different answers.
    match command {
        Command::SpawnWorker {
            project_key,
            label,
            charter,
            spawned_by,
            resume_existing,
            kick,
            resume_kick,
            interactive,
            from_boot_respawn,
            ..
        } => {
            let (tx, rx) = oneshot::channel();
            let command = Command::SpawnWorker {
                project_key,
                label,
                charter,
                spawned_by,
                resume_existing,
                kick,
                resume_kick,
                interactive,
                from_boot_respawn,
                return_to: Some(tx),
            };
            answered(socket, state, command, reply_to, rx).await
        }
        Command::DespawnWorker { project_key, label, force, .. } => {
            let (tx, rx) = oneshot::channel();
            let command = Command::DespawnWorker { project_key, label, force, respond: Some(tx) };
            answered(socket, state, command, reply_to, rx).await
        }
        Command::UpsertReviewThread { project, branch, thread, .. } => {
            let (tx, rx) = oneshot::channel();
            let command =
                Command::UpsertReviewThread { project, branch, thread, respond: Some(tx) };
            answered(socket, state, command, reply_to, rx).await
        }
        Command::SubmitReview { project, branch, summary, thread_ids, origin, .. } => {
            let (tx, rx) = oneshot::channel();
            let command = Command::SubmitReview {
                project,
                branch,
                summary,
                thread_ids,
                origin,
                respond: Some(tx),
            };
            answered(socket, state, command, reply_to, rx).await
        }
        // Every other command is fire-and-forget: success sends nothing
        // extra, because the update arrives through the subscription the
        // client already has.
        other => match state.surface.dispatch(other) {
            Ok(()) => Ok(()),
            Err(refusal) => {
                send(
                    socket,
                    ServerMessage::Error { what: "dispatch".to_owned(), why: refusal.to_string() },
                )
                .await
            }
        },
    }
}

/// Dispatch a reply-carrying command, and answer the client that asked.
///
/// A client that asked for an answer always hears one - INCLUDING a refusal,
/// so it is never left watching a channel that stays empty.
async fn answered<T: serde::Serialize>(
    socket: &mut WebSocket,
    state: &TransportState,
    command: Command,
    reply_to: Option<u64>,
    answer: oneshot::Receiver<T>,
) -> anyhow::Result<()> {
    let outcome = state.surface.dispatch(command);
    let Some(to) = reply_to else {
        // Nobody asked, so a refusal has nowhere to go but the log. The
        // command was dispatched either way.
        if let Err(refusal) = outcome {
            tracing::debug!(
                target: "forge_server::transport",
                event_name = "dispatch_refused",
                %refusal,
                "a command nobody awaited a reply for was refused",
            );
        }
        return Ok(());
    };

    let body = match (outcome, answer.await) {
        (Ok(()), Ok(reply)) => serde_json::to_value(reply)?,
        (Err(refusal), _) => serde_json::to_value(refusal.to_string())?,
        (Ok(()), Err(_)) => serde_json::to_value("the session closed before answering")?,
    };
    send(socket, ServerMessage::Reply { reply_to: to, body }).await
}
