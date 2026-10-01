//! One connection: the upgrade, what the server says first, and the loop
//! that answers what a client asks for.

use std::sync::Arc;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use super::PROTOCOL_VERSION;
use super::TransportState;
use super::envelope::{ClientMessage, ClientSettings, ServerMessage, Subject};
use super::wire::{conversation_for, encode_subject, page, walk_processes_if_stale};
use crate::delivery::delivery_turn;
use crate::live::Live;
use crate::{Command, SessionUpdate};

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
async fn drive(socket: &mut WebSocket, state: &Arc<TransportState>) -> anyhow::Result<()> {
    let mut watched: Vec<Subject> = Vec::new();
    // None until the client's first SUBSCRIBE, which is what decides whether
    // this connection answers - not its first message, so a client whose first
    // word is a `more` or a command is not locked into observing. Registering
    // as answering before a client says so would count one that cannot show a
    // prompt as able to answer it, and the core parks a turn on that reply
    // rather than failing it.
    let mut updates: Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)> = None;

    let outcome = run_connection(socket, state, &mut watched, &mut updates).await;

    // Every way out of the loop runs this, a failed read included: a client
    // that goes away without unsubscribing is still a client that has gone,
    // and the seats it was showing are let go with it. Leaving them attached
    // would keep them counted as watched forever, so the mark that says a
    // completion went unwatched would never arm for them again.
    for what in &watched {
        if let Subject::Session(slot) = what {
            Live::lock(&state.live).detach(slot);
        }
    }
    outcome
}

/// The connection's own loop, so that every way out of it runs the release
/// above rather than only the clean one.
async fn run_connection(
    socket: &mut WebSocket,
    state: &Arc<TransportState>,
    watched: &mut Vec<Subject>,
    updates: &mut Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)>,
) -> anyhow::Result<()> {
    loop {
        tokio::select! {
            msg = socket.next() => {
                let Some(msg) = msg else { break };   // the client went away
                let msg = msg?;
                handle_client(socket, state, watched, updates, msg).await?;
            }
            // Deliberately NOT `Some(update) = updates.recv()`. A pattern that stops matching
            // DISABLES its branch in `select!` rather than ending the loop, so a closed channel
            // would silently stop delivering while the socket stayed open.
            heard = next_update(updates) => {
                let Some(update) = heard else { break };
                // The fold is the transport's, not this connection's: it runs
                // once for the whole socket in `transport::fold_the_stream`.
                if watched.iter().any(|what| what.covers(&update)) {
                    send_update(socket, update).await?;
                }
            }
        }
    }
    Ok(())
}

/// This connection's stream from the core, opened when the client first
/// subscribes and registered with the role that subscribe declares.
///
/// A later subscribe that declares answering REPLACES it - a client is not
/// held to a role it announced before it had decided - and the role only ever
/// goes one way while a connection lives: a stream is opened with the
/// strongest role the client has declared so far, so a later observing
/// subscribe leaves the answering one in place.
///
/// **Whatever the replaced stream had queued is handed back**, because the
/// queue is not only about the subject just asked for: it can hold news about
/// a seat this client is already watching, and dropping the receiver drops
/// that news silently.
///
/// Every caller gets a stream of its own, so a second client attaches beside
/// the first rather than stealing its events, and dropping the socket drops
/// this with it - which is what keeps a subscription from outliving its
/// connection.
///
/// Nothing is missed by waiting for a subscribe: a connection with no
/// subscribed subject forwards no update anyway.
fn open_stream(
    state: &TransportState,
    updates: &mut Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)>,
    answering: bool,
) -> Vec<SessionUpdate> {
    if updates.as_ref().is_some_and(|(_, held)| *held >= answering) {
        return Vec::new();
    }
    let queued = updates.take().map_or_else(Vec::new, |(mut receiver, _)| {
        let mut queued = Vec::new();
        while let Ok(update) = receiver.try_recv() {
            queued.push(update);
        }
        queued
    });
    *updates = Some((state.surface.subscribe_client(answering), answering));
    queued
}

/// The stream's next update, or a future that never resolves while there is
/// none - which is what keeps the branch out of the way until the client has
/// subscribed and the role is known.
async fn next_update(
    updates: &mut Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)>,
) -> Option<SessionUpdate> {
    match updates.as_mut() {
        Some((updates, _)) => updates.recv().await,
        None => std::future::pending().await,
    }
}

/// Answer one client message.
async fn handle_client(
    socket: &mut WebSocket,
    state: &Arc<TransportState>,
    watched: &mut Vec<Subject>,
    updates: &mut Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)>,
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
        ClientMessage::Subscribe { what, answering } => {
            // Forwarded before the snapshot: they were emitted before it was
            // taken, and the client reads them in the order it receives them.
            let queued = open_stream(state, updates, answering);
            for update in queued {
                if watched.iter().any(|what| what.covers(&update)) {
                    send_update(socket, update).await?;
                }
            }
            match encode_subject(state, &what).await {
                Ok(data) => {
                    // Watched only once the subject is one this server can
                    // answer for: a refused subscribe leaves nothing to hear.
                    //
                    // A seat subscription is also this connection SHOWING the
                    // seat, which is what keeps a turn finishing on it from
                    // arming a mark nobody needs: the reader is looking at it.
                    if let Subject::Session(slot) = &what {
                        Live::lock(&state.live).attach(slot);
                    }
                    watched.push(what.clone());
                    send(socket, ServerMessage::Snapshot { subject: what, data }).await
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
                    .await
                }
            }
        }
        ClientMessage::Command { command, reply_to } => {
            let command = *command;
            // Where a command's answer goes, decided before anything acts.
            //
            // Four commands report through the reply and have no update behind
            // them, so omitting `reply_to` on one of those is not a client
            // declining a reply - it is a client declining to learn whether
            // the work happened. Every other command is fire-and-forget: its
            // outcome rides the subscription, so it has no reply to set - and a
            // client that set one anyway is watching a channel nothing will
            // come down, which is refused rather than ignored.
            match (reply_to, answers_through_a_reply(&command)) {
                (Some(to), true) => dispatch_answering(socket, state, command, to).await,
                (None, true) => {
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "reply_to".to_owned(),
                            why: "this command answers through `reply_to` and no update carries its outcome, so that field is required: without it a client cannot tell a refusal from success".to_owned(),
                        },
                    )
                    .await
                }
                // Refused BEFORE dispatch, because a command that both acted
                // and answered nothing is the case a client cannot tell from
                // success.
                (Some(_), false) => {
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "reply_to".to_owned(),
                            why: "this command's outcome rides the subscription rather than a reply, so it is sent without `reply_to`: there is no message a reply for it would carry".to_owned(),
                        },
                    )
                    .await
                }
                (None, false) => match state.surface.dispatch(command) {
                    Ok(()) => Ok(()),
                    Err(refusal) => {
                        send(
                            socket,
                            ServerMessage::Error {
                                what: "dispatch".to_owned(),
                                why: refusal.to_string(),
                            },
                        )
                        .await
                    }
                },
            }
        }
        ClientMessage::More { conversation, before, turns } => {
            let roster = state.surface.roster();
            // A seat forge holds no session for is an ANSWER rather than an
            // empty page: a client drawing nothing would read the second as a
            // broken conversation rather than as a seat nobody has started.
            if roster.cwd_for(&conversation).is_none() {
                return send(
                    socket,
                    ServerMessage::Error {
                        what: "more".to_owned(),
                        why: format!("forge holds no session for {conversation:?}"),
                    },
                )
                .await;
            }
            // Reading a seat is watching it, so paging refreshes the walk the
            // same way subscribing does. The window in the walk is what keeps
            // a client paging a long conversation from walking on every page.
            walk_processes_if_stale(
                &state.surface,
                &conversation,
                roster.claude_pid(&conversation),
            )
            .await;
            // The window slices the boundaries the fold reported, so a turn
            // crosses whole. Slicing on a count of messages instead is what
            // would hand a client half a turn.
            //
            // The fold runs in a blocking task and NOT under the lock: the
            // socket folds the core's stream in one task for every seat, so a
            // fold holding a seat's lock would stall update delivery for every
            // client on every seat rather than for a second reader of this one.
            let Some(held) = conversation_for(state, &conversation).await else {
                // **A question that cannot be answered is REFUSED rather than
                // answered with a value that looks like one.** An empty page
                // carries `cursor: null`, and a client reads that as "nothing
                // above" and stops asking - so a replay that did not arrive
                // would make the seat's history unreachable rather than
                // merely late, and a plausible `compaction_count: 0` would
                // ride along with it.
                return send(
                    socket,
                    ServerMessage::Error {
                        what: "more".to_owned(),
                        why: format!(
                            "the conversation for {conversation:?} is not held yet, so this page \
                             cannot be answered; asking again may find it"
                        ),
                    },
                )
                .await;
            };
            let seat = conversation.clone();
            let opening = before.clone();
            let page = tokio::task::spawn_blocking(move || {
                held.read(|held| page(held.messages(), held.rendered(), opening.as_deref(), turns))
            })
            .await
            .unwrap_or_else(|error| {
                tracing::warn!(
                    event_name = "transcript_fold_failed",
                    %error,
                    slot = %seat.display(),
                    "the fold did not finish; the page is answered empty",
                );
                page(
                    &[],
                    &crate::transcript::Rendered {
                        units: Vec::new(),
                        turns: Vec::new(),
                        endings: std::collections::HashMap::new(),
                    },
                    before.as_deref(),
                    turns,
                )
            });
            send(
                socket,
                ServerMessage::Page { conversation, turns: page.turns, cursor: page.cursor },
            )
            .await
        }
        ClientMessage::Unsubscribe { what } => {
            // No answer: the client asked to stop hearing, and there is
            // nothing to say back.
            //
            // ONE entry, because `subscribe` added one. `retain` would drop
            // every copy, so a client that subscribed twice and unsubscribed
            // once would leave its attachment counted with nothing watching -
            // a seat counted as watched forever, whose completions never earn
            // a mark again. And detaching for a subject this connection never
            // held decrements a count another connection owns, so a seat a
            // client IS displaying would earn a mark instead.
            if let Some(at) = watched.iter().position(|held| held == &what) {
                watched.remove(at);
                if let Subject::Session(slot) = &what {
                    Live::lock(&state.live).detach(slot);
                }
            }
            Ok(())
        }
    }
}

async fn send(socket: &mut WebSocket, message: ServerMessage) -> anyhow::Result<()> {
    let text = serde_json::to_string(&message)?;
    socket.send(Message::Text(text.into())).await?;
    Ok(())
}

/// Send one update to this client, and ahead of it the turn it draws as when
/// it is a delivery.
///
/// A cron fire, a Gotify notification, a Slack message and a peer comm each
/// reach a session's model as a prompt on stdin, and the CLI does not echo a
/// prompt back - so the wire carries nothing a view could draw and a client
/// drawing only frames would show the assistant answering something nobody
/// saw. The terminal forges that turn in its own process; this is the same
/// forge on the way out, so a client that is not the terminal draws it too.
/// The typed update follows, because a view keeps it for its own bookkeeping.
async fn send_update(socket: &mut WebSocket, update: SessionUpdate) -> anyhow::Result<()> {
    if let Some(key) = update.slot().cloned()
        && let Some(msg) = delivery_turn(&update, &key)
    {
        send(
            socket,
            ServerMessage::Update {
                update: Box::new(SessionUpdate::ChatAppended { key, msg, origin: None }),
            },
        )
        .await?;
    }
    send(socket, ServerMessage::Update { update: Box::new(update) }).await
}

/// Dispatch one command, and answer a client that asked for an answer.
///
/// Success sends nothing extra: the update arrives through the subscription
/// the client already has, which is why a client subscribes before it acts.
async fn dispatch_answering(
    socket: &mut WebSocket,
    state: &TransportState,
    command: Command,
    to: u64,
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
            answered(socket, state, command, to, rx).await
        }
        Command::DespawnWorker { project_key, label, force, .. } => {
            let (tx, rx) = oneshot::channel();
            let command = Command::DespawnWorker { project_key, label, force, respond: Some(tx) };
            answered(socket, state, command, to, rx).await
        }
        Command::UpsertReviewThread { project, branch, thread, .. } => {
            let (tx, rx) = oneshot::channel();
            let command =
                Command::UpsertReviewThread { project, branch, thread, respond: Some(tx) };
            answered(socket, state, command, to, rx).await
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
            answered(socket, state, command, to, rx).await
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
/// Whether a command reports its outcome through the reply rather than
/// through an update on the subscription.
///
/// These four are the ones with nothing behind them on the wire: a client that
/// omits `reply_to` for one of them has no second way to learn what happened.
fn answers_through_a_reply(command: &Command) -> bool {
    matches!(
        command,
        Command::SpawnWorker { .. }
            | Command::DespawnWorker { .. }
            | Command::UpsertReviewThread { .. }
            | Command::SubmitReview { .. }
    )
}

/// Dispatch one of those four, and send back the answer it carries.
async fn answered<T: serde::Serialize>(
    socket: &mut WebSocket,
    state: &TransportState,
    command: Command,
    to: u64,
    answer: oneshot::Receiver<T>,
) -> anyhow::Result<()> {
    let outcome = state.surface.dispatch(command);
    let body = match (outcome, answer.await) {
        (Ok(()), Ok(reply)) => serde_json::to_value(reply)?,
        (Err(refusal), _) => serde_json::to_value(refusal.to_string())?,
        (Ok(()), Err(_)) => serde_json::to_value("the session closed before answering")?,
    };
    send(socket, ServerMessage::Reply { reply_to: to, body }).await
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::work::WorkCache;

    /// Escalating from observing to answering opens a new stream - and what
    /// the old one had queued is handed back rather than dropped. The queue is
    /// not only about the subject just asked for: it can hold news about a
    /// seat this client is already watching, and dropping the receiver drops
    /// that news with no error anywhere.
    #[tokio::test]
    async fn escalating_hands_back_what_the_replaced_stream_had_queued() {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
        };

        // A client observing, with two updates queued and nobody reading them.
        let mut updates = Some((state.surface.subscribe_client(false), false));
        fleet.emit(SessionUpdate::CatalogLoaded);
        fleet.emit(SessionUpdate::CatalogLoaded);

        let handed_back = open_stream(&state, &mut updates, true);

        assert_eq!(
            handed_back.len(),
            2,
            "both updates the replaced stream was holding come back to be forwarded",
        );
        assert!(
            matches!(updates, Some((_, true))),
            "and the connection is on an answering stream now",
        );
    }

    /// The role only goes up while a connection lives: a later observing
    /// subscribe leaves the answering stream in place rather than downgrading
    /// it, and hands nothing back.
    #[tokio::test]
    async fn a_later_observing_subscribe_does_not_downgrade_the_stream() {
        let dir = tempfile::tempdir().expect("tempdir").keep();
        let fleet = crate::testing::Fleet::in_dir(&dir, &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            config: forge_primitives::WebConfig::default(),
        };

        let mut updates = Some((state.surface.subscribe_client(true), true));
        fleet.emit(SessionUpdate::CatalogLoaded);

        let handed_back = open_stream(&state, &mut updates, false);

        assert!(handed_back.is_empty(), "the stream was not replaced, so nothing is handed back");
        assert!(
            matches!(updates, Some((_, true))),
            "and the client is still counted as one that can answer",
        );
    }
}
