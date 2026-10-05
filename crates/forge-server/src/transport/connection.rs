//! One connection: the upgrade, what the server says first, and the loop
//! that answers what a client asks for.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use forge_primitives::SessionSlot;

use super::PROTOCOL_VERSION;
use super::TransportState;
use super::batch::{self, Batch};
use super::envelope::{ClientMessage, ClientSettings, ServerMessage, Subject};
use super::wire::{conversation_for, encode_subject, page};
use crate::live::Live;
use crate::surface::ViewSurface;
use crate::{Command, DispatchError, SessionUpdate};

/// The seats this connection is holding, each given back when this drops.
///
/// A plain vec would do, except that anything between the hold and the
/// clean-up - a panic in the encode, which folds a transcript and scans a
/// tree - takes the clean-up with it, and a seat's count left standing is a
/// loop the next viewer never gets. A guard runs on the way out of an unwind.
struct Holds<'a> {
    surface: &'a Arc<ViewSurface>,
    seats: Vec<SessionSlot>,
}

impl<'a> Holds<'a> {
    fn new(surface: &'a Arc<ViewSurface>) -> Self {
        Self { surface, seats: Vec::new() }
    }

    /// Remember a seat this connection is now holding.
    fn take(&mut self, slot: &SessionSlot) {
        self.seats.push(slot.clone());
    }

    /// Give back one hold, answering whether this connection had one.
    fn give_back(&mut self, slot: &SessionSlot) -> bool {
        let Some(at) = self.seats.iter().position(|held| held == slot) else {
            return false;
        };
        self.seats.remove(at);
        self.surface.release_seat(slot);
        true
    }
}

impl Drop for Holds<'_> {
    fn drop(&mut self) {
        for slot in &self.seats {
            self.surface.release_seat(slot);
        }
    }
}

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
        settings: ClientSettings::new(&state.config, state.surface.dictate_axes()),
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
    // This connection's own number: the stamp that names which take is
    // this connection's, on the commands that start and stop one and on
    // the updates that come back. Minted here so the teardown closes the
    // take this connection started and no other.
    let me = mint_connection_id();
    let mut watched: Vec<Subject> = Vec::new();
    // The seats THIS connection is holding, which is not the same list as the
    // seats it watches: every session subscribe holds - a sessionless seat's
    // watch waits for its session (#1706) - and a release is counted per seat,
    // so giving back a hold this connection never took would spend one another
    // connection is still using.
    let mut holds = Holds::new(&state.surface);
    // The seat this connection is streaming a take for, if any: a dictation
    // frame carries no seat of its own, so this is what addresses it.
    let mut dictate: Option<SessionSlot> = None;
    // None until the client's first SUBSCRIBE, which is what decides whether
    // this connection answers - not its first message, so a client whose first
    // word is a `more` or a command is not locked into observing. Registering
    // as answering before a client says so would count one that cannot show a
    // prompt as able to answer it, and the core parks a turn on that reply
    // rather than failing it.
    let mut updates: Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)> = None;

    let outcome =
        run_connection(socket, state, &mut watched, &mut holds, &mut updates, &mut dictate, me)
            .await;

    // The take this connection was streaming ends with it: it is DROPPED
    // rather than submitted - its reader is gone, so nothing it produced
    // would land anywhere - and the seat is free for the next take. A
    // DEVICE take is not touched - its audio is this machine's, and its
    // recording task outlives any one client.
    if let Some(seat) = dictate.as_ref()
        && state.surface.dictate_close(seat, me)
    {
        tracing::debug!(
            target: "forge_server::transport",
            event_name = "dictate_take_dropped",
            slot = %seat.display(),
            "the connection that was streaming a take went away; the take was dropped",
        );
    }

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
    // Every hold this connection took goes back here, one release apiece -
    // the same count the refusal and unsubscribe paths keep, so a seat two
    // viewers show is only let go once. Nothing to say - the guard's own drop
    // is the last word, and it also covers a panic on the way here.
    drop(holds);
    outcome
}

/// The connection's own loop, so that every way out of it runs the detach
/// above rather than only the clean one.
async fn run_connection(
    socket: &mut WebSocket,
    state: &Arc<TransportState>,
    watched: &mut Vec<Subject>,
    holds: &mut Holds<'_>,
    updates: &mut Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)>,
    dictate: &mut Option<SessionSlot>,
    me: u64,
) -> anyhow::Result<()> {
    let mut held = Batch::default();
    loop {
        let due = held.due();
        tokio::select! {
            msg = socket.next() => {
                let Some(msg) = msg else { break };   // the client went away
                let msg = msg?;
                // What the core has already said goes out first: an answer is
                // composed after it, and a snapshot overtaking an update the
                // core emitted before it would land older news on newer.
                batch::flush(socket, held.take()).await?;
                handle_client(socket, state, watched, holds, updates, dictate, me, msg).await?;
            }
            // Deliberately NOT `Some(update) = updates.recv()`. A pattern that stops matching
            // DISABLES its branch in `select!` rather than ending the loop, so a closed channel
            // would silently stop delivering while the socket stayed open.
            heard = next_update(updates) => {
                let Some(update) = heard else { break };
                // The fold is the transport's, not this connection's: it runs
                // once for the whole socket in `transport::fold_the_stream`.
                if watched.iter().any(|what| what.covers(&update)) && ours_to_hear(&update, me) {
                    held.push(Instant::now(), update);
                }
            }
            // The deadline is a value, not a condition: with nothing held the
            // branch is disabled and this instant is never waited on.
            () = tokio::time::sleep_until(due.unwrap_or_else(Instant::now).into()),
                if due.is_some() =>
            {
                batch::flush(socket, held.take()).await?;
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
    holds: &mut Holds<'_>,
    updates: &mut Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)>,
    dictate: &mut Option<SessionSlot>,
    me: u64,
    msg: Message,
) -> anyhow::Result<()> {
    let text = match msg {
        Message::Text(text) => text,
        // A binary message is a dictation frame and nothing else. It is
        // never answered: a take's audio has no reply channel, and the
        // take's own outcome is what a reader sees either way.
        Message::Binary(bytes) => {
            take_frame(&state.surface, dictate.as_ref(), &bytes);
            return Ok(());
        }
        _ => return Ok(()),
    };
    let Ok(client) = serde_json::from_str::<ClientMessage>(&text) else {
        return send(
            socket,
            ServerMessage::Error {
                what: "client_message".to_owned(),
                why: "that is not a message this server knows".to_owned(),
                seat: None,
            },
        )
        .await;
    };
    match client {
        ClientMessage::Subscribe { what, answering } => {
            // Forwarded before the snapshot: they were emitted before it was
            // taken, and the client reads them in the order it receives them.
            let mut queued = Vec::new();
            for update in open_stream(state, updates, answering) {
                if watched.iter().any(|what| what.covers(&update)) && ours_to_hear(&update, me) {
                    queued.push(update);
                }
            }
            // **A seat is held BEFORE its snapshot is encoded.** The hold is
            // what has its working tree read, and the encode answers that
            // store - so a read taken first would hand a page the tree as it
            // was before this subscription, and nothing would correct it: the
            // seat's viewers are seeded with the row the hold read, so the
            // loop announces only what moves after it.
            if let Subject::Session(slot) = &what {
                // **Every session subscribe holds the seat**, a sessionless
                // one included - its watch waits for the session (#1706) - and
                // every taken hold is remembered here so the two paths a
                // subscribe can end without one can give it back: the encode's
                // refusal below, and the unsubscribe.
                state.surface.hold_seat(slot).await;
                holds.take(slot);
            }
            batch::flush(socket, queued).await?;
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
                    // The hold every session subscribe takes goes back with
                    // the refusal, or a view of a seat that does not exist
                    // would keep its loop running behind nothing.
                    if let Subject::Session(slot) = &what {
                        holds.give_back(slot);
                    }
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "subscribe".to_owned(),
                            why: refusal.to_string(),
                            seat: None,
                        },
                    )
                    .await
                }
            }
        }
        ClientMessage::Command { command, reply_to } => {
            // This connection's own stamp on the two commands that carry
            // one: the take a start begins and the stop that ends it are
            // this connection's alone, and the stamp riding back on the
            // take's updates is what routes them home. A client cannot
            // claim another's - the field never crosses the wire.
            let command = match *command {
                Command::DictateStream { key, options, .. } => {
                    Command::DictateStream { key, options, initiator: Some(me) }
                }
                Command::DictateStop { key, submit, .. } => {
                    Command::DictateStop { key, submit, initiator: Some(me) }
                }
                other => other,
            };
            // The seat a stream start names is this connection's to
            // remember: the dictation frames that follow carry no seat of
            // their own, and this is the one message that says which take
            // they belong to.
            let streamed = match &command {
                Command::DictateStream { key, .. } => Some(key.clone()),
                _ => None,
            };
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
                            seat: None,
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
                            seat: None,
                        },
                    )
                    .await
                }
                (None, false) => match state.surface.dispatch(command) {
                    Ok(()) => {
                        if streamed.is_some() {
                            *dictate = streamed;
                        }
                        Ok(())
                    }
                    Err(refusal) => {
                        send(
                            socket,
                            ServerMessage::Error {
                                what: refusal_tag(&refusal).to_owned(),
                                why: refusal.to_string(),
                                seat: None,
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
                        why: format!("forge holds no session for {}", conversation.display()),
                        // **Named, so a client holding several seats' asks
                        // drains only its own**: the connection is shared and an
                        // error carries no other seat.
                        seat: Some(conversation.clone()),
                    },
                )
                .await;
            }
            // Paging walks nothing: the walk belongs to the seat's hold and
            // its loop now, so a page reads the store the way every other read
            // does - and a seat nobody held is a seat nothing walks.
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
                            "the conversation for {} is not held yet, so this page cannot be \
                             answered; asking again may find it",
                            conversation.display()
                        ),
                        seat: Some(conversation.clone()),
                    },
                )
                .await;
            };
            let seat = conversation.clone();
            let opening = before.clone();
            let folded = tokio::task::spawn_blocking(move || {
                held.read(|held| page(held.messages(), held.rendered(), opening.as_deref(), turns))
            })
            .await;
            // **A fold that did not finish is refused for the reason the arm
            // above refuses.** An empty page carries `cursor: null`, which a
            // client reads as the end of the history - so answering one here
            // would make the seat unreachable rather than merely unread this
            // time.
            let page = match folded {
                Ok(page) => page,
                Err(err) => {
                    tracing::warn!(
                        event_name = "transcript_fold_failed",
                        slot = %seat.display(),
                        error = %err,
                        "the fold did not finish; this page is refused rather than answered empty",
                    );
                    return send(
                        socket,
                        ServerMessage::Error {
                            what: "more".to_owned(),
                            why: format!(
                                "the fold over {} did not finish, so this page cannot be \
                                 answered; asking again may find it",
                                seat.display()
                            ),
                            seat: Some(seat.clone()),
                        },
                    )
                    .await;
                }
            };
            send(
                socket,
                ServerMessage::Page { conversation, turns: page.turns, cursor: page.cursor },
            )
            .await
        }
        ClientMessage::Devices => {
            // The walk opens the microphone stack and takes as long as it
            // takes, so it runs in a blocking task rather than inline. This
            // connection is serialized behind it for that duration - the same
            // way `more`'s fold holds its caller - and what it does NOT do is
            // hold the runtime, so other connections and the rest of the
            // server carry on.
            let surface = Arc::clone(&state.surface);
            let outcome = tokio::task::spawn_blocking(move || surface.dictate_device_catalog())
                .await
                .unwrap_or_else(|join| Err(join.to_string()));
            send(socket, devices_answer(outcome)).await
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
                    // Only a hold THIS connection took is given back here,
                    // for the same reason the clean-up gives back only those:
                    // a count decremented twice is another viewer's seat.
                    holds.give_back(slot);
                }
            }
            Ok(())
        }
    }
}

/// The next number for a connection, unique for the process's life.
///
/// A take belongs to the connection it started on, so the number is what
/// both halves of that routing name: the transport stamps it on
/// `dictate_stream` and `dictate_stop`, the core echoes it on the take's
/// updates, and a connection forwards only what carries its own. Minted
/// process-wide rather than per transport so two servers in one process -
/// a test's, a scratch instance beside the real one - cannot hand the same
/// number to different connections.
fn mint_connection_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Whether this connection is one of an update's readers.
///
/// Every `Dictate*` update about a take belongs to the connection that
/// started it, so a connection hears its own take and no other's - and the
/// terminal's in-process take, whose updates carry no connection at all,
/// is nobody's on the socket. Every other update keeps the watch rule
/// alone.
fn ours_to_hear(update: &SessionUpdate, me: u64) -> bool {
    match update {
        SessionUpdate::DictateStarted { initiator, .. }
        | SessionUpdate::DictateLevel { initiator, .. }
        | SessionUpdate::DictateTranscribing { initiator, .. }
        | SessionUpdate::DictateProgress { initiator, .. }
        | SessionUpdate::DictateEnded { initiator, .. } => *initiator == Some(me),
        _ => true,
    }
}

/// Where one binary message went, decided without touching a take.
#[derive(Debug, PartialEq)]
enum FrameRoute {
    /// The seat's take, and the samples for it.
    Take(SessionSlot, Vec<f32>),
    /// A binary message that is not a frame this server takes.
    Refused(super::frame::Refusal),
    /// A frame, but this connection has not started a take to give it to.
    NoTake,
}

/// Decide one binary message's destination.
///
/// **The frame carries no seat of its own**: it addresses the take the
/// connection that sent it started, because a connection streams one take
/// at a time and its messages are ordered, so a frame can only arrive
/// between its own take's start and its stop.
fn frame_route(bytes: &[u8], dictate: Option<&SessionSlot>) -> FrameRoute {
    let decoded = match super::frame::decode(bytes) {
        Ok(decoded) => decoded,
        Err(refusal) => return FrameRoute::Refused(refusal),
    };
    match dictate {
        Some(seat) => FrameRoute::Take(seat.clone(), decoded.samples),
        None => FrameRoute::NoTake,
    }
}

/// Push one dictation frame into this connection's take, recording
/// anything else.
///
/// Every refusal is a `debug` record rather than a warning: a client
/// streaming into a server that cannot take it is information about that
/// client, not a problem forge has, and the record is what makes it
/// legible either way.
fn take_frame(surface: &ViewSurface, dictate: Option<&SessionSlot>, bytes: &[u8]) {
    match frame_route(bytes, dictate) {
        FrameRoute::Take(seat, samples) => {
            if !surface.dictate_push(&seat, &samples) {
                tracing::debug!(
                    event_name = "dictate_frame_dropped",
                    slot = %seat.display(),
                    "a dictation frame arrived for a take that has stopped or is gone",
                );
            }
        }
        FrameRoute::Refused(refusal) => tracing::debug!(
            event_name = "dictate_frame_refused",
            reason = %refusal.reason(),
            "a binary message was not a dictation frame",
        ),
        FrameRoute::NoTake => tracing::debug!(
            event_name = "dictate_frame_without_a_take",
            bytes = bytes.len(),
            "a dictation frame arrived on a connection that has not started a take",
        ),
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
            mcp_families,
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
                mcp_families,
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
                    ServerMessage::Error {
                        what: refusal_tag(&refusal).to_owned(),
                        why: refusal.to_string(),
                        seat: None,
                    },
                )
                .await
            }
        },
    }
}

/// What a `devices` request is answered with: the walked list, or the walk's
/// own refusal - which a client renders where the list would have been.
fn devices_answer(outcome: Result<forge_workspace::DictateDeviceCatalog, String>) -> ServerMessage {
    match outcome {
        Ok(catalog) => ServerMessage::Devices {
            devices: catalog
                .devices
                .iter()
                .map(|device| crate::transport::wire::DeviceWire {
                    id: device.id.clone(),
                    name: device.name.clone(),
                    is_default: device.is_default,
                })
                .collect(),
            configured: catalog.configured,
        },
        Err(why) => ServerMessage::Error { what: "devices".to_owned(), why, seat: None },
    }
}

/// The operation a dispatch refusal names, which a client routes by.
///
/// `dispatch` alone does not say which of a client's commands was refused,
/// and an answer to a draft the core no longer holds is drawn where that
/// draft's dock stood - the dock itself is gone by then. Every arm that
/// refuses a dispatch builds the tag here, so the two cannot drift.
fn refusal_tag(refusal: &DispatchError) -> &'static str {
    match refusal {
        DispatchError::NoDraftWaiting { .. } => "respond_slack_post",
        _ => "dispatch",
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
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
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
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
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

    /// A connection hears its own take's updates and no other's: another
    /// connection's take is dropped, and so is the terminal's own - which
    /// carries no connection at all - because neither is this connection's
    /// to draw.
    #[test]
    fn only_its_own_takes_updates_are_heard() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        let mine = SessionUpdate::DictateStarted {
            key: seat.clone(),
            floor_db: -50.0,
            generation: 1,
            initiator: Some(7),
        };
        assert!(ours_to_hear(&mine, 7), "a connection hears the take it started");
        assert!(!ours_to_hear(&mine, 8), "and not one another connection started");

        let terminal = SessionUpdate::DictateLevel { key: seat, peak_db: -20.0, initiator: None };
        assert!(
            !ours_to_hear(&terminal, 7),
            "the terminal's in-process take is nobody's on the socket"
        );

        assert!(
            ours_to_hear(&SessionUpdate::CatalogLoaded, 7),
            "every other update keeps the watch rule alone"
        );
    }

    /// A binary message at the wire's shape: the codec tag, then the
    /// samples.
    fn payload(samples: &[i16]) -> Vec<u8> {
        let mut bytes = vec![super::super::frame::Codec::PcmI16.tag()];
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    /// A frame routes to the take its own connection started: the seat
    /// comes from the memory, never from the message.
    #[test]
    fn a_frame_routes_to_the_seat_the_connection_started() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        assert_eq!(
            frame_route(&payload(&[16384]), Some(&seat)),
            FrameRoute::Take(seat, vec![0.5]),
            "the connection's own seat, and the samples the bytes carry"
        );
    }

    /// A frame before any take - or after one resolved - has nowhere to
    /// go, and is dropped rather than held for a take that may never come.
    #[test]
    fn a_frame_on_a_connection_with_no_take_goes_nowhere() {
        assert_eq!(frame_route(&payload(&[0]), None), FrameRoute::NoTake);
    }

    /// A binary message that is not a frame is refused with its own
    /// reason, so the record says which way it was not a frame.
    #[test]
    fn a_binary_message_that_is_not_a_frame_is_refused_by_its_reason() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        assert_eq!(
            frame_route(&[], Some(&seat)),
            FrameRoute::Refused(super::super::frame::Refusal::ShortHeader)
        );
        assert_eq!(
            frame_route(&[9, 0], Some(&seat)),
            FrameRoute::Refused(super::super::frame::Refusal::UnknownCodec(9))
        );
    }

    /// The walk's two outcomes as a client sees them: the list with its
    /// configured pin, or the error a client renders where the list would
    /// have been.
    #[test]
    fn a_devices_answer_carries_the_list_or_the_walks_refusal() {
        let catalog = forge_workspace::DictateDeviceCatalog {
            devices: vec![
                forge_dictate::Device {
                    id: "a-mic".to_owned(),
                    name: "Studio Mic".to_owned(),
                    is_default: true,
                },
                forge_dictate::Device {
                    id: "b-mic".to_owned(),
                    name: "Built-in".to_owned(),
                    is_default: false,
                },
            ],
            configured: Some("b-mic".to_owned()),
        };
        let ServerMessage::Devices { devices, configured } = devices_answer(Ok(catalog)) else {
            panic!("a walked catalogue is answered with the list")
        };
        assert_eq!(devices.len(), 2, "every input the walk found crosses");
        assert_eq!(devices[0].name, "Studio Mic", "with the label a picker draws");
        assert!(devices[0].is_default, "and the mark the system would pick");
        assert_eq!(configured.as_deref(), Some("b-mic"), "and the configured pin beside them");

        let refused = devices_answer(Err("no audio host".to_owned()));
        let ServerMessage::Error { what, why, .. } = refused else {
            panic!("a failed walk is refused rather than answered with an empty list")
        };
        assert_eq!(what, "devices", "the refusal names what was asked for");
        assert_eq!(why, "no audio host", "and carries the walk's own reason");
    }
}
