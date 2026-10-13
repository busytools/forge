//! One connection: the upgrade, what the server says first, and the loop
//! that answers what a client asks for.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::StreamExt;
use tokio::sync::{mpsc, oneshot};

use base64::Engine as _;
use forge_primitives::SessionSlot;
use forge_primitives::browser::BrowserPart;
use forge_primitives::document::DocumentRead;
use forge_workspace::browser::{BrowserRelay, BrowserRequest, Notice};

use super::PROTOCOL_VERSION;
use super::TransportState;
use super::batch::{self, Batch};
use super::envelope::{ClientMessage, ClientSettings, ServerMessage, Subject};
use super::wire::{conversation_for, encode_subject, page};
use crate::live::Live;
use crate::surface::{DictateOutcome, ViewSurface};
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

/// Why an ask failed because its host sent the frames a step out of order.
///
/// Named rather than left to the driver's own words: the ordering contract is
/// this socket's, and the sentence a session reads should say which half of it
/// was broken.
const BEFORE_ANSWER: &str = "the browser host sent an image frame before the answer that \
                              declares the image, so the call cannot be completed";

/// Why an in-flight ask failed because a force-take moved the role mid-call.
///
/// Named like [`BEFORE_ANSWER`]: the call was not wrong, the role moved out
/// from under it, and the session should read which of the two happened.
const HOST_GONE_BY_TAKE: &str = "another client took the browser role before the answer arrived, \
                                  so the call cannot be completed";

/// The browser role this connection holds, and what is in flight under it.
struct Hosting {
    /// The asks the relay routes here, in the order they were made.
    asks: mpsc::UnboundedReceiver<BrowserRequest>,
    /// What the relay says about the role itself - a force-take away, a
    /// promotion in - and which seats exist, for the client's sweep.
    notices: mpsc::UnboundedReceiver<Notice>,
    /// The asks sent to the client and not yet answered, by id.
    in_flight: HashMap<u64, InFlight>,
    /// The role itself, given back when this drops, a panic included.
    _role: BrowserRole,
}

impl Hosting {
    /// Offer the role for `id`, and keep the channel asks would arrive on.
    ///
    /// **Offering and holding are different**, and the channel is kept either
    /// way: a connection that offers while another holds the role waits in
    /// line, and the relay promotes it there when the holder goes - which is
    /// only possible because the channel it will be sent asks on already
    /// exists. A waiter receives nothing until then, so what it costs is an
    /// idle receiver. `true` says the offer was also taken.
    fn offer(relay: &Arc<BrowserRelay>, id: u64) -> (Self, bool) {
        let (to_host, asks) = mpsc::unbounded_channel();
        let (notices_tx, notices) = mpsc::unbounded_channel();
        let held = relay.register(id, to_host, notices_tx);
        (
            Self {
                asks,
                notices,
                in_flight: HashMap::new(),
                _role: BrowserRole { relay: Arc::clone(relay), id },
            },
            held,
        )
    }
}

/// Gives the browser role back on the way out, however the connection ends.
///
/// A guard rather than a line at the end of the loop, for the same reason
/// [`Holds`] is one: a panic while encoding an answer would otherwise leave
/// the role held by a dead connection, and no client could take it again.
struct BrowserRole {
    relay: Arc<BrowserRelay>,
    id: u64,
}

/// Counts this connection among the attached clients for as long as it
/// lives, so a document hand-off can tell a viewer exists to open on. A
/// guard, for the same reason [`Holds`] is one: the count must come back
/// down however the connection ends, a dropped socket included.
struct AttachedClient(Arc<ViewSurface>);

impl AttachedClient {
    fn new(surface: &Arc<ViewSurface>) -> Self {
        surface.client_attached();
        Self(Arc::clone(surface))
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        self.0.client_detached();
    }
}

impl Drop for BrowserRole {
    fn drop(&mut self) {
        self.relay.unregister(self.id);
    }
}

/// One ask sent to the host, waiting for its answer - and then, when the
/// answer declared images, for the frames that carry their bytes.
struct InFlight {
    /// The tool call's answer, on its way back to the relay.
    reply: oneshot::Sender<Result<Vec<BrowserPart>, String>>,
    /// The parts the answer declared, in order; `None` until it arrives.
    parts: Option<Vec<BrowserPart>>,
    /// The indices of `parts` that are images, in order.
    images: Vec<usize>,
    /// How many of `images` have had their bytes filled in.
    filled: usize,
}

/// Take the upgrade and give the connection its own task.
///
/// The frame limit is set to the image cap here, and what that does to the
/// defaults is worth stating because it goes both ways: it RAISES the frame
/// limit (16 MiB by default, and an image frame is that plus its header) and
/// LOWERS the message limit (64 MiB by default). One byte past the cap is
/// carried on purpose, so an image a shade too big is refused by
/// [`super::frame`] - which fails the ask it belongs to, naming the reason -
/// rather than tearing the connection down at the socket layer, where the
/// asker would be told only that its host went away.
pub async fn upgrade(ws: WebSocketUpgrade, State(state): State<Arc<TransportState>>) -> Response {
    let limit = super::frame::MAX_IMAGE_BYTES + super::frame::IMAGE_HEADER_BYTES + 1;
    ws.max_frame_size(limit).max_message_size(limit).on_upgrade(move |socket| greet(socket, state))
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
        forge_version: crate::FORGE_VERSION.to_owned(),
        forge_version_short: crate::FORGE_VERSION_SHORT.to_owned(),
        settings: ClientSettings::new(&state.client, state.surface.dictate_axes()),
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
    // This connection is a client that can draw the viewer for as long as it
    // lives; the guard hands the count back on the way out.
    let _attached = AttachedClient::new(&state.surface);
    let mut watched: Vec<Subject> = Vec::new();
    // The seats THIS connection is holding, which is not the same list as the
    // seats it watches: every session subscribe holds - a sessionless seat's
    // watch waits for its session (#1706) - and a release is counted per seat,
    // so giving back a hold this connection never took would spend one another
    // connection is still using.
    let mut holds = Holds::new(&state.surface);
    // The seats this connection has claimed takes for, oldest first, and
    // whether it is recording the read-aloud set: a dictation frame carries
    // no seat of its own, so these are what address it, and the teardown
    // below closes every one of them. A refused start adds its seat here
    // too - the dispatch cannot tell a registration from a refusal, both
    // answering through the stream - and only a take that ENDS takes it
    // back out; a refusal leaves it, because the take it answers never ran.
    let mut dictate = Streaming::default();
    // None until the client's first SUBSCRIBE, which is what decides whether
    // this connection answers - not its first message, so a client whose first
    // word is a `more` or a command is not locked into observing. Registering
    // as answering before a client says so would count one that cannot show a
    // prompt as able to answer it, and the core parks a turn on that reply
    // rather than failing it.
    let mut updates: Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)> = None;
    // None until a subscribe declares the browser capability and the role is
    // free. Dropping it hands the role back.
    let mut hosting: Option<Hosting> = None;

    let outcome = run_connection(
        socket,
        state,
        &mut watched,
        &mut holds,
        &mut updates,
        &mut dictate,
        &mut hosting,
        me,
    )
    .await;

    // Every take this connection started ends with it: each is DROPPED
    // rather than submitted - its reader is gone, so nothing it produced
    // would land anywhere - and the seat is free for the next take. A
    // DEVICE take is not touched - its audio is this machine's, and its
    // recording task outlives any one client.
    for claim in &dictate.seats {
        if state.surface.dictate_close(&claim.seat, me) {
            tracing::debug!(
                target: "forge_server::transport",
                event_name = "dictate_take_dropped",
                slot = %claim.seat.display(),
                "the connection that was streaming a take went away; the take was dropped",
            );
        }
    }
    // A recording this connection was feeding dies with it: dropping it is
    // the same answer as the page's cancel, and keeping it would leave a
    // recording no connection can stop.
    if dictate.read_aloud {
        let stopped = state
            .surface
            .dispatch(Command::DictateReadAloudStop { keep: false, initiator: Some(me) });
        tracing::debug!(
            target: "forge_server::transport",
            event_name = "read_aloud_recording_dropped",
            stopped = stopped.is_ok(),
            "the connection that was recording the read-aloud set went away",
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

/// What this connection is streaming: the seats it has claimed takes for -
/// a start's seat is claimed as soon as its dispatch answers, so a refused
/// one keeps its entry, and a frame that follows a start always finds the
/// seat - and whether it is recording the read-aloud set. A dictation frame
/// carries no seat of its own - and the recording carries none at all - so
/// these are what address one.
#[derive(Default)]
struct Streaming {
    seats: Vec<Claim>,
    read_aloud: bool,
}

/// One seat's claim: the seat, and the generation of the take that holds it.
///
/// The generation is stamped by the take's own `DictateStarted` (the
/// dispatch answers before one can be known), and it is what tells THIS
/// take's end from an older one's - a stop leaves the take finishing while
/// the reader can start the next at once, so the stale end arrives after the
/// newer take has registered under the same seat (#1886).
struct Claim {
    seat: SessionSlot,
    generation: u64,
}

impl Streaming {
    /// Whether a frame from this connection has anything to land in.
    fn any(&self) -> bool {
        !self.seats.is_empty() || self.read_aloud
    }

    /// Claim a seat for a start that is on its way, generation unknown: the
    /// dispatch answers before one can be known, and the frame that follows
    /// the start must find the seat already here. **An existing claim is
    /// reset to unknown**: the newer take's `DictateStarted` has not arrived
    /// yet, and an end still in flight for the older one must not retire the
    /// claim the newer take is about to stamp.
    fn claim(&mut self, seat: &SessionSlot) {
        match self.seats.iter_mut().find(|claim| &claim.seat == seat) {
            Some(claim) => claim.generation = 0,
            None => self.seats.push(Claim { seat: seat.clone(), generation: 0 }),
        }
    }

    /// Stamp the claim with the generation its own `DictateStarted` carried.
    fn stamp(&mut self, seat: &SessionSlot, generation: u64) {
        if let Some(claim) = self.seats.iter_mut().find(|claim| &claim.seat == seat) {
            claim.generation = generation;
        }
    }

    /// Retire the claim an end names, and only that one: a refusal answers a
    /// start that never ran, and an older take's end must not strip the
    /// claim a newer take of the same seat registered under.
    fn retire(&mut self, seat: &SessionSlot, outcome: &DictateOutcome, generation: u64) {
        if matches!(outcome, DictateOutcome::Refused { .. }) {
            return;
        }
        self.seats.retain(|claim| &claim.seat != seat || claim.generation != generation);
    }
}

/// The connection's own loop, so that every way out of it runs the detach
/// above rather than only the clean one.
async fn run_connection(
    socket: &mut WebSocket,
    state: &Arc<TransportState>,
    watched: &mut Vec<Subject>,
    holds: &mut Holds<'_>,
    updates: &mut Option<(mpsc::UnboundedReceiver<SessionUpdate>, bool)>,
    dictate: &mut Streaming,
    hosting: &mut Option<Hosting>,
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
                handle_client(socket, state, watched, holds, updates, dictate, hosting, me, msg)
                    .await?;
            }
            // Deliberately NOT `Some(update) = updates.recv()`. A pattern that stops matching
            // DISABLES its branch in `select!` rather than ending the loop, so a closed channel
            // would silently stop delivering while the socket stayed open.
            heard = next_update(updates) => {
                let Some(update) = heard else { break };
                // A take of this connection's that ends takes its seat back
                // out: with the take gone, the seat has no stream a frame
                // could belong to. Only the claim that take NAMES is retired
                // - a refusal answers a start that never ran, and a stale
                // end must not strip the seat a newer take registered under
                // (#1880, #1886); the generation comes from the take's own
                // `DictateStarted`, read just below. Read before the watch
                // gate, because the list is the connection's own bookkeeping
                // rather than something a subscriber hears.
                if let SessionUpdate::DictateStarted {
                    key,
                    generation,
                    initiator: Some(id),
                    ..
                } = &update
                    && *id == me
                {
                    dictate.stamp(key, *generation);
                }
                if let SessionUpdate::DictateEnded {
                    key,
                    initiator: Some(id),
                    outcome,
                    generation,
                } = &update
                    && *id == me
                {
                    dictate.retire(key, outcome, *generation);
                }
                // The fold is the transport's, not this connection's: it runs
                // once for the whole socket in `transport::fold_the_stream`.
                if forwards(&update, watched, me) {
                    // **A fatal goes out now, not on the batch's clock.** The
                    // exit it announces is already on its way - `workspace.
                    // shutdown` follows the run loop's return - and a frame
                    // still waiting out its window when the process drops dies
                    // with it. Everything else can wait the interval.
                    let fatal = matches!(update, SessionUpdate::FatalError { .. });
                    held.push(Instant::now(), for_a_client(update));
                    if fatal {
                        batch::flush(socket, held.take()).await?;
                    }
                }
            }
            // **The role's own channel and its asks, raced as one future**
            // because `select!` wants one borrow of the hosting: an ask on
            // its way out to this host, or the relay saying the role moved.
            event = next_hosting_event(hosting) => {
                match event {
                    HostingEvent::Ask(request) => {
                        // What the core already said goes out first, the same
                        // rule the client-message arm keeps: an ask is composed
                        // after the news that preceded it, so nothing the core
                        // emitted before this call lands behind it.
                        batch::flush(socket, held.take()).await?;
                        if let Some(hosting) = hosting.as_mut() {
                            hosting.in_flight.insert(
                                request.id,
                                InFlight {
                                    reply: request.reply,
                                    parts: None,
                                    images: Vec::new(),
                                    filled: 0,
                                },
                            );
                        }
                        send(
                            socket,
                            ServerMessage::BrowserAsk {
                                id: request.id,
                                seat: request.seat,
                                tool: request.tool,
                                args: request.args,
                            },
                        )
                        .await?;
                    }
                    // **The role taken away.** A force-take tells this
                    // connection before its next ask would have arrived. Every
                    // call it was carrying fails with the take's own sentence
                    // - not the relay's HOST_GONE, which would say the holder
                    // went away when it was displaced - and the client is told,
                    // because its strip must not go on saying it hosts.
                    HostingEvent::Notice(Notice::Taken) => {
                        fail_every_in_flight(hosting, HOST_GONE_BY_TAKE);
                        hosting.take();
                        batch::flush(socket, held.take()).await?;
                        send(socket, ServerMessage::BrowserRole { hosting: false }).await?;
                    }
                    // **The role handed over.** A promotion arrives without an
                    // ask to make it obvious, so it is said the way the grant
                    // is: the strip must not go on saying another client
                    // drives a browser this connection is now being asked for.
                    // The core's own news goes out first, the same rule every
                    // other send keeps.
                    HostingEvent::Notice(Notice::Granted) => {
                        batch::flush(socket, held.take()).await?;
                        send(socket, ServerMessage::BrowserRole { hosting: true }).await?;
                    }
                    // **Which seats exist, for the client's profile sweep.**
                    // Sent whole, on every capable connection: the client
                    // deletes the profiles of the seats the set no longer
                    // holds. The core's own news first, as ever.
                    HostingEvent::Notice(Notice::Seats(seats)) => {
                        batch::flush(socket, held.take()).await?;
                        send(socket, ServerMessage::BrowserSeats { seats }).await?;
                    }
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

/// The relay's next ask, or a future that never resolves while this
/// connection does not hold the browser role - the same shape as
/// [`next_update`], and for the same reason: a disabled `select!` branch is
/// what keeps a connection the role was never given out of the way.
/// What the hosting has to say to the loop: one tool call to carry out, or
/// the relay saying the role itself moved.
enum HostingEvent {
    Ask(BrowserRequest),
    Notice(Notice),
}

/// The next thing the hosting has for the loop - one future, because both
/// channels hang off the same borrow and `select!` takes one per branch.
///
/// **A closed channel is not the connection's end, and this is the seam a
/// force-take runs through.** The relay drops both of a displaced holder's
/// senders, so its channels close - and the last word, the `Taken` notice, is
/// still IN the closed notice channel: `recv` on a closed channel delivers
/// what it holds and only then reads as done. So each channel is drained
/// first, a closed one is waited past rather than satisfied, and both closed
/// parks the branch: the connection stays a client - a viewer, told what
/// happened - where ending it would have taken its socket down mid-test and
/// mid-life. A connection with no hosting parks here too, so its branch is
/// inert rather than ending the loop.
///
/// It answers an `HostingEvent` and never a `None`: every way out of this
/// future is either an event or a park, because a closed channel is not the
/// connection's end (see above).
async fn next_hosting_event(hosting: &mut Option<Hosting>) -> HostingEvent {
    match hosting.as_mut() {
        Some(hosting) => loop {
            if let Ok(notice) = hosting.notices.try_recv() {
                return HostingEvent::Notice(notice);
            }
            if let Ok(request) = hosting.asks.try_recv() {
                return HostingEvent::Ask(request);
            }
            let asks_dead = hosting.asks.is_closed();
            let notices_dead = hosting.notices.is_closed();
            if asks_dead && notices_dead {
                return std::future::pending().await;
            }
            let next = tokio::select! {
                ask = hosting.asks.recv(), if !asks_dead => ask.map(HostingEvent::Ask),
                notice = hosting.notices.recv(), if !notices_dead => {
                    notice.map(HostingEvent::Notice)
                }
            };
            if let Some(event) = next {
                return event;
            }
            // A channel closed under the select with nothing left in it:
            // loop, and either the other channel or the park decides.
        },
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
    dictate: &mut Streaming,
    hosting: &mut Option<Hosting>,
    me: u64,
    msg: Message,
) -> anyhow::Result<()> {
    let text = match msg {
        Message::Text(text) => text,
        // A binary message is a frame of one of the two kinds. Neither is
        // answered: a take's audio has no reply channel, and an image frame
        // completes an answer that is already on its way back.
        Message::Binary(bytes) => {
            match frame_route(&bytes, dictate) {
                FrameRoute::Image { id, bytes } => browser_image_bytes(hosting, id, &bytes),
                FrameRoute::Refused(refusal) => {
                    // An image frame this server cannot take cannot fill the
                    // part that is waiting for it, and the ask it belongs to
                    // would wait forever. Nothing names which ask a refused
                    // frame was for, so every ask waiting on an image is
                    // failed with the reason - a failure a session can read,
                    // where a wait with no end is not. **A dictation refusal
                    // is not that**: it belongs to the audio stream, and
                    // failing image waits on it would name an image error
                    // over a microphone frame.
                    if refusal.concerns_images() {
                        fail_awaiting_images(
                            hosting,
                            &format!(
                                "an image frame this server cannot take: {}",
                                refusal.reason()
                            ),
                        );
                    }
                    take_frame(&state.surface, dictate, me, FrameRoute::Refused(refusal));
                }
                other => take_frame(&state.surface, dictate, me, other),
            }
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
        ClientMessage::Subscribe { what, answering, browser, declare } => {
            // **The browser role, offered once per connection.** A capable
            // client declares it on every subscribe it makes; the first
            // declaration while the role is free takes it, and a later one
            // finds this connection already in place. A declaration that
            // arrives while another client holds the role changes nothing -
            // one host drives the one browser, and being the second is not an
            // error: the relay keeps this connection's channel and hands it
            // the role when the holder goes, so nothing has to be declared
            // again for the handover to happen.
            let mut grant_role = false;
            // The seat set a fresh offer is answered with: the client's
            // profile sweep reads it, so a capable connection learns which
            // seats exist the moment it declares - boot, a reconnect, or a
            // second window. `None` when the store could not be read: no set
            // is sent, because a client reading "no seats" would delete
            // every profile it holds.
            let mut seats: Option<Vec<SessionSlot>> = None;
            if browser && hosting.is_none() {
                let (offering, held) = Hosting::offer(&state.browser, me);
                *hosting = Some(offering);
                // **The grant is said out loud**, after the snapshot: the
                // client knew only that it COULD host; its own strip needs
                // "does", and a force-take later is the same frame with
                // `false`. Sent last so the subscribe's own answer keeps
                // being the snapshot.
                grant_role = held;
                seats = state.surface.live_seats();
            }
            // Forwarded before the snapshot: they were emitted before it was
            // taken, and the client reads them in the order it receives them.
            let mut queued = Vec::new();
            for update in open_stream(state, updates, answering) {
                if forwards(&update, watched, me) {
                    queued.push(for_a_client(update));
                }
            }
            // **A seat is held BEFORE its snapshot is encoded.** The hold is
            // what has its working tree read, and the encode answers that
            // store - so a read taken first would hand a page the tree as it
            // was before this subscription, and nothing would correct it: the
            // seat's viewers are seeded with the row the hold read, so the
            // loop announces only what moves after it.
            //
            // **A declare holds nothing**, like the watch entry and the
            // attachment below: it re-states the browser capability against a
            // subject the client already watches, and the client never counts
            // it, so nothing would ever give a hold back.
            if let Subject::Session(slot) = &what
                && !declare
            {
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
                    let seat = match &what {
                        Subject::Session(slot) => Some(slot.clone()),
                        _ => None,
                    };
                    // **The attach answers whether a mark was actually
                    // spent.** Showing a seat spends the diamond and the
                    // failure mark the home carries for it, so a connection
                    // that already holds the home is owed a fresh one exactly
                    // then - a re-encode with nothing to carry is 448 KB and
                    // 113-263 ms of server time for a home the client already
                    // holds (measured 2026-10-09).
                    //
                    // A declare is not a page opening the subject, so it
                    // neither attaches nor watches: both are owed back one
                    // per unsubscribe, and the client never counts the
                    // declare to send one.
                    let spent = match &seat {
                        Some(slot) if !declare => {
                            let failed = state.surface.failed_turn(slot);
                            Live::lock(&state.live).attach(slot, failed)
                        }
                        _ => false,
                    };
                    if !declare {
                        watched.push(what.clone());
                    }
                    send(socket, ServerMessage::Snapshot { subject: what, data }).await?;
                    // Rather than keeping a spent mark until the next unrelated
                    // redraw, which can be half a minute away.
                    if let Some(slot) = &seat
                        && spent
                        && watched.iter().any(|held| matches!(held, Subject::Home))
                    {
                        // A refresh that could not be encoded is not worth
                        // dropping the connection for: the next redraw carries
                        // the same news. Debug, because the marks reading spent
                        // until then is forge working as it should.
                        match encode_subject(state, &Subject::Home).await {
                            Ok(home) => {
                                send(
                                    socket,
                                    ServerMessage::Snapshot { subject: Subject::Home, data: home },
                                )
                                .await?;
                            }
                            Err(error) => tracing::debug!(
                                event_name = "home_refresh_failed",
                                slot = %slot.display(),
                                %error,
                                "the home refresh after a seat attach could not be encoded",
                            ),
                        }
                    }
                    if grant_role {
                        send(socket, ServerMessage::BrowserRole { hosting: true }).await?;
                    }
                    if let Some(seats) = seats {
                        return send(socket, ServerMessage::BrowserSeats { seats }).await;
                    }
                    Ok(())
                }
                // A seat nobody has started is an ANSWER rather than a
                // silence: the client learns why, and never draws an empty
                // snapshot as a broken page.
                Err(refusal) => {
                    // The hold every session subscribe takes goes back with
                    // the refusal, or a view of a seat that does not exist
                    // would keep its loop running behind nothing. A DECLARE
                    // takes none - giving one back here would return the hold
                    // the client's own subscribe is still using.
                    if let Subject::Session(slot) = &what
                        && !declare
                    {
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
                    .await?;
                    // **The role was taken at register, before the snapshot
                    // could refuse** - so the grant is owed here too, or a
                    // connection that holds the role (and will be sent asks)
                    // is one the client believes never got it. The seat set
                    // is the client's sweep to run, so it is owed here as
                    // well - the offer was made, refused snapshot or not.
                    if grant_role {
                        send(socket, ServerMessage::BrowserRole { hosting: true }).await?;
                    }
                    if let Some(seats) = seats {
                        return send(socket, ServerMessage::BrowserSeats { seats }).await;
                    }
                    Ok(())
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
                Command::DictateReadAloudStart { .. } => {
                    Command::DictateReadAloudStart { initiator: Some(me) }
                }
                Command::DictateReadAloudStop { keep, .. } => {
                    Command::DictateReadAloudStop { keep, initiator: Some(me) }
                }
                other => other,
            };
            // The seat a stream start names is this connection's to
            // remember: the dictation frames that follow carry no seat of
            // their own, and this is the one message that says which take
            // they belong to. A read-aloud recording is the same promise
            // with no seat at all.
            let streamed = match &command {
                Command::DictateStream { key, .. } => Some(key.clone()),
                _ => None,
            };
            let recording = match &command {
                Command::DictateReadAloudStart { .. } => Some(true),
                Command::DictateReadAloudStop { .. } => Some(false),
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
                        // Claimed as soon as the dispatch answers - Ok for a
                        // refusal too, whose reason rides the stream as its
                        // own `DictateEnded` - so the frame that follows a
                        // start finds the seat, which is what makes the claim
                        // run AFTER the dispatch rather than before it. The
                        // generation stays unknown until the take's own
                        // `DictateStarted` stamps it, and only an end naming
                        // that generation retires it.
                        if let Some(seat) = streamed {
                            dictate.claim(&seat);
                        }
                        if let Some(recording) = recording {
                            dictate.read_aloud = recording;
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
            // **Clamped, because the count is the client's.** A `turns` of a
            // hundred thousand would have the read hand over its whole cap and
            // the page encode every row of it; a `turns` of zero is a page
            // that opens nowhere, whose cursor names the message it was asked
            // with - a client asking for it forever.
            let turns = turns.clamp(1, crate::transport::wire::SUBSCRIBE_TURNS);
            let roster = state.surface.roster();
            // A seat forge holds no session for is an ANSWER rather than an
            // empty page: a client drawing nothing would read the second as a
            // broken conversation rather than as a seat nobody has started.
            let Some(cwd) = roster.cwd_for(&conversation) else {
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
            };
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
            // **A cursor at or below the window's floor is a page the
            // transcript answers.** The held conversation stops at its oldest
            // frame, and a client walking back past it asks for rows this seat
            // no longer keeps: the workspace reads a bounded page of the
            // session's file off this thread, by the file's own bytes - the
            // only numbering that survives a window whose front is a cut
            // inside a running turn.
            let ask = below_the_floor(before.as_deref(), &held);
            if matches!(ask, Ask::Below(_) | Ask::BelowWindow) {
                let below = match ask {
                    Ask::Below(anchor) => Some(anchor),
                    // A window holding no row names nothing, and the read
                    // answers with the file's own newest rows, which cannot
                    // overlap anything the client holds.
                    Ask::BelowWindow => boundary_of(&held.lock()),
                    Ask::Held | Ask::Newest => None,
                };
                return match rows_below_floor(state, &seat, &cwd, below, turns).await {
                    BelowFloor::Page(page) => {
                        send(
                            socket,
                            ServerMessage::Page {
                                conversation,
                                turns: page.turns,
                                cursor: page.cursor,
                            },
                        )
                        .await
                    }
                    BelowFloor::Empty => {
                        send(
                            socket,
                            ServerMessage::Page { conversation, turns: Vec::new(), cursor: None },
                        )
                        .await
                    }
                    BelowFloor::Refused => {
                        send(
                            socket,
                            ServerMessage::Error {
                                what: "more".to_owned(),
                                why: format!(
                                    "the fold over {} did not finish, so this page cannot be \
                                     answered; asking again may find it",
                                    seat.display()
                                ),
                                seat: Some(seat),
                            },
                        )
                        .await
                    }
                };
            }
            let opening = before.clone();
            let floor = held.lock().dropped();
            let folded = tokio::task::spawn_blocking(move || {
                held.read(|held| {
                    page(
                        held.messages(),
                        held.rendered(),
                        held.dropped(),
                        opening.as_deref(),
                        turns,
                    )
                })
            })
            .await;
            // **A fold that did not finish is refused for the reason the arm
            // above refuses.** An empty page carries `cursor: null`, which a
            // client reads as the end of the history - so answering one here
            // would make the seat unreachable rather than merely unread this
            // time.
            let page = match folded {
                // **The window's own start is not the history's.** A page cut
                // there with a floor above it hands the floor back as the
                // cursor, so an uninterrupted walk crosses into the
                // transcript instead of stopping; with no floor above it, the
                // `None` is the honest end of the history.
                Ok(mut page) => {
                    if page.cursor.is_none() && floor > 0 {
                        page.cursor = Some(floor.to_string());
                    }
                    page
                }
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
        ClientMessage::DocumentRead { session, path, reviewable } => {
            // A file off disk takes as long as it takes, so the read runs in
            // a blocking task rather than inline - the same shape the device
            // walk below takes. Nothing parks on the reader's pace: the
            // answer is the read, and the review's finish is its own event.
            let surface = Arc::clone(&state.surface);
            let seat = session.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                surface.read_document(&seat, &path, reviewable)
            })
            .await
            .unwrap_or_else(|join| Err(join.to_string()));
            send(socket, document_answer(&state.surface, &session, outcome)).await
        }
        ClientMessage::CommitDiff { session, sha } => {
            // The same seat resolution the session record takes, so a
            // commit scope reads the tree the record's own content read.
            let Some(cwd) = state.surface.roster().cwd_for(&session) else {
                return send(
                    socket,
                    ServerMessage::Error {
                        what: "commit_diff".to_owned(),
                        why: format!("forge holds no session for {}", session.display()),
                        seat: Some(session),
                    },
                )
                .await;
            };
            // A `sha` git cannot read answers the scan's own `scan_failed`
            // layer, not an error: a read that could not run is not a read
            // that answered empty, and the client draws each as itself.
            let content = forge_workspace::env::git_diff::content::scan_commit(&cwd, &sha).await;
            send(
                socket,
                ServerMessage::CommitDiff {
                    sha,
                    files: content.files,
                    truncated: content.truncated,
                },
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
        ClientMessage::BrowserTakeRole => {
            // The claimant must have offered first: the relay answers a host
            // down the channel a capable declare created, and a connection
            // that never declared has none. Both outcomes answer with the
            // same frame - the control that sent this draws the truth either
            // way, so a refusal needs no words.
            let claimed = hosting.is_some() && state.browser.claim(me);
            send(socket, ServerMessage::BrowserRole { hosting: claimed }).await
        }
        ClientMessage::BrowserAnswer { id, parts, error } => {
            let Some(hosting) = hosting.as_mut() else {
                tracing::debug!(
                    event_name = "browser_answer_without_the_role",
                    id,
                    "a browser answer arrived on a connection that does not hold the role",
                );
                return Ok(());
            };
            let Some(mut in_flight) = hosting.in_flight.remove(&id) else {
                tracing::debug!(
                    event_name = "browser_answer_unmatched",
                    id,
                    "a browser answer named an ask nothing on this connection is waiting on",
                );
                return Ok(());
            };
            // A failure is the call's whole answer, and the driver's own
            // sentence is what the tool returns.
            if let Some(message) = error {
                in_flight.reply.send(Err(message)).ok();
                return Ok(());
            }
            let images: Vec<usize> = parts
                .iter()
                .enumerate()
                .filter(|(_, part)| matches!(part, BrowserPart::Image { .. }))
                .map(|(at, _)| at)
                .collect();
            if images.is_empty() {
                in_flight.reply.send(Ok(parts)).ok();
                return Ok(());
            }
            // The images' bytes ride their own frames, which follow this
            // answer on the same socket; the ask is answered when the last
            // one lands.
            in_flight.parts = Some(parts);
            in_flight.images = images;
            hosting.in_flight.insert(id, in_flight);
            Ok(())
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
/// started it, so a connection hears its own take and no other's - and an
/// in-process take, whose updates carry no connection at all,
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

/// Whether this update is one of a take's own.
///
/// The family `ours_to_hear` stamps by connection: a take's meter, its
/// phases and the end that settles it.
fn is_take_news(update: &SessionUpdate) -> bool {
    matches!(
        update,
        SessionUpdate::DictateStarted { .. }
            | SessionUpdate::DictateLevel { .. }
            | SessionUpdate::DictateTranscribing { .. }
            | SessionUpdate::DictateProgress { .. }
            | SessionUpdate::DictateEnded { .. }
    )
}

/// Whether this connection forwards one update.
///
/// **A take's own news reaches its connection whether or not the seat is
/// showing.** The reader may be on another seat while their recording runs,
/// and the fold that draws the take - and the end settling it - is this
/// connection's: filtered by the watch, the record would go on drawing a
/// recording that is over (#1880). Every other update keeps the watch rule
/// alone.
fn forwards(update: &SessionUpdate, watched: &[Subject], me: u64) -> bool {
    ours_to_hear(update, me)
        && (is_take_news(update) || watched.iter().any(|what| what.covers(update)))
}

/// The update as it crosses to a client.
///
/// **A seat's transcript never rides an update to a client.** The three
/// variants that carry one are the session task's seed for the transport's
/// own copy, and a client answers them with a READ - the subscribe snapshot's
/// windowed `conversation` and `more` above it (the client's own `REPLACES`
/// list says so, `client/src/session/apply.ts`) - rather than drawing what
/// they carry. So the message list is emptied here, at the one place an
/// update becomes something a client hears; what stays is the seat's session
/// facts.
///
/// **The bound has to be the frame's, not the window's.** The window holds
/// four thousand messages and no byte ceiling, and on the largest seat
/// measured those weigh 382MB - past the 256MiB frame the client's socket
/// carries (`MAX_FRAME_BYTES`, client/src-tauri/socket.rs), which is what a
/// live 2.5GiB seat's resume did to a connection (#2150) when the whole read
/// rode one `Connected`. What a client reads instead is a page, and a page is
/// bounded by TURNS rather than bytes (twenty a subscribe, twenty a `more`):
/// on that seat the newest twenty-turn window weighs 103MB and the heaviest
/// 126MB (measured 2026-10-13), roughly 2x under the cap rather than orders
/// under it, so a frame carrying any message list leans on that cap. A frame
/// whose size does not depend on the conversation's size is the only one that
/// holds whatever the seat weighs.
fn for_a_client(mut update: SessionUpdate) -> SessionUpdate {
    match &mut update {
        SessionUpdate::Connected { history, .. }
        | SessionUpdate::SessionReplaced { history, .. }
        | SessionUpdate::HistoryReplayed { history, .. } => *history = Vec::new(),
        _ => {}
    }
    update
}

/// Where one binary message went, decided without touching a take.
#[derive(Debug, PartialEq)]
enum FrameRoute {
    /// The samples of a dictation frame, for the seats this connection
    /// started.
    Audio(Vec<f32>),
    /// One browser answer's image bytes, under the answer's id.
    Image { id: u64, bytes: Vec<u8> },
    /// A binary message that is not a frame this server takes.
    Refused(super::frame::Refusal),
    /// A dictation frame, but this connection has not started a take to give
    /// it to.
    NoTake,
}

/// Decide one binary message's destination.
///
/// **The frame carries no seat of its own**: it belongs to whatever take the
/// connection that sent it started - or to the read-aloud recording it is
/// feeding - and its messages are ordered, so a frame can only arrive between
/// a start of its own and that take's end. It is offered to every seat the
/// connection has claimed - a refused start's seat stays among them, holding
/// nothing - and the push below keeps it only where the live take is THAT
/// connection's.
fn frame_route(bytes: &[u8], dictate: &Streaming) -> FrameRoute {
    let decoded = match super::frame::decode(bytes) {
        Ok(decoded) => decoded,
        Err(refusal) => return FrameRoute::Refused(refusal),
    };
    match decoded {
        // An image frame carries the answer's id, which is what pairs it with
        // the answer that declared the image - and it needs no take, because
        // no take sent it.
        super::frame::Frame::Image { id, bytes } => FrameRoute::Image { id, bytes },
        super::frame::Frame::Audio(samples) => {
            if !dictate.any() {
                return FrameRoute::NoTake;
            }
            FrameRoute::Audio(samples)
        }
    }
}

/// Push one dictation frame into this connection's takes or its read-aloud
/// recording, recording anything else.
///
/// Every refusal is a `debug` record rather than a warning: a client
/// streaming into a server that cannot take it is information about that
/// client, not a problem forge has, and the record is what makes it
/// legible either way.
fn take_frame(surface: &ViewSurface, dictate: &Streaming, me: u64, route: FrameRoute) {
    match route {
        FrameRoute::Audio(samples) => {
            // Offered to each seat this connection started, kept only where
            // the live take is THIS connection's - a seat's take can be
            // another connection's, and its audio is not this one's to feed.
            let kept = dictate
                .seats
                .iter()
                .any(|claim| surface.dictate_push(&claim.seat, &samples, Some(me)))
                || (dictate.read_aloud && surface.dictate_read_aloud_push(&samples, Some(me)));
            if !kept {
                tracing::debug!(
                    event_name = "dictate_frame_dropped",
                    "a dictation frame arrived for takes that have stopped or are another connection's",
                );
            }
        }
        // An image frame never reaches here: it is routed by id, and the
        // caller's own arm is the one place it goes.
        FrameRoute::Image { .. } => {}
        FrameRoute::Refused(refusal) => tracing::debug!(
            event_name = "binary_frame_refused",
            reason = %refusal.reason(),
            "a binary message was not a frame this server takes",
        ),
        FrameRoute::NoTake => tracing::debug!(
            event_name = "dictate_frame_without_a_take",
            "a dictation frame arrived on a connection that has not started a take",
        ),
    }
}

/// Fill one image part with the bytes its frame carried, and answer the ask
/// once the last image is filled.
///
/// **Which part a frame fills is its order**, not anything the frame says:
/// the frames follow their answer on one socket, so the first carries the
/// first image part. The id says which ANSWER a frame belongs to, which is
/// what two sessions asking at once need to be told apart.
fn browser_image_bytes(hosting: &mut Option<Hosting>, id: u64, bytes: &[u8]) {
    let Some(hosting) = hosting.as_mut() else {
        tracing::debug!(
            event_name = "browser_image_without_the_role",
            id,
            "an image frame arrived on a connection that does not hold the browser role",
        );
        return;
    };
    let Some(in_flight) = hosting.in_flight.get_mut(&id) else {
        tracing::debug!(
            event_name = "browser_image_unmatched",
            id,
            "an image frame named an ask nothing on this connection is waiting on",
        );
        return;
    };
    let Some(parts) = in_flight.parts.as_mut() else {
        // **A frame before its answer is a malformed pair, not slowness**, so
        // the ask fails naming it rather than being left to wait for parts an
        // answer has not declared. The answer that was supposed to come first
        // is dropped when it arrives, for an ask nothing is waiting on.
        let refused = hosting.in_flight.remove(&id);
        if let Some(in_flight) = refused {
            tracing::debug!(
                event_name = "browser_image_before_its_answer",
                id,
                "an image frame arrived before the answer that declared the image",
            );
            in_flight.reply.send(Err(BEFORE_ANSWER.to_owned())).ok();
        }
        return;
    };
    let Some(&at) = in_flight.images.get(in_flight.filled) else {
        tracing::debug!(
            event_name = "browser_image_with_no_part_waiting",
            id,
            "an image frame arrived with every image part of its answer already filled",
        );
        return;
    };
    if let Some(BrowserPart::Image { bytes: into, .. }) = parts.get_mut(at) {
        *into = bytes.to_vec();
    }
    in_flight.filled += 1;
    if in_flight.filled == in_flight.images.len() {
        let Some(in_flight) = hosting.in_flight.remove(&id) else {
            return;
        };
        if let Some(parts) = in_flight.parts {
            in_flight.reply.send(Ok(parts)).ok();
        }
    }
}

/// Fail every call this connection is carrying, whatever it was waiting for.
///
/// **The sentence is the point.** Without this, what fails an in-flight ask is
/// the reply sender being dropped with the hosting, and the relay reads that
/// as the holder having gone away - which is a reason that names the wrong
/// thing: a take displaced this connection and it is still here.
fn fail_every_in_flight(hosting: &mut Option<Hosting>, why: &str) {
    let Some(hosting) = hosting.as_mut() else { return };
    for (_, in_flight) in hosting.in_flight.drain() {
        in_flight.reply.send(Err(why.to_owned())).ok();
    }
}

/// Fail every ask on this connection that is waiting for an image, naming why.
///
/// A frame whose bytes cannot be taken is the end of those asks: the part it
/// was for is never filled, and the alternative to failing them is a session's
/// tool call waiting on a promise nothing can keep.
fn fail_awaiting_images(hosting: &mut Option<Hosting>, why: &str) {
    let Some(hosting) = hosting.as_mut() else {
        return;
    };
    let waiting: Vec<u64> = hosting
        .in_flight
        .iter()
        .filter(|(_, in_flight)| {
            in_flight.parts.is_some() && in_flight.filled < in_flight.images.len()
        })
        .map(|(id, _)| *id)
        .collect();
    for id in waiting {
        if let Some(in_flight) = hosting.in_flight.remove(&id) {
            in_flight.reply.send(Err(why.to_owned())).ok();
        }
    }
}

async fn send(socket: &mut WebSocket, message: ServerMessage) -> anyhow::Result<()> {
    let text = serde_json::to_string(&message)?;
    socket.send(Message::Text(text.into())).await?;
    Ok(())
}

/// What a `more` asks for, out of the cursor it was sent.
#[derive(Debug)]
enum Ask {
    /// A cursor in the held window's own numbering: the page is cut there.
    Held,
    /// No cursor at all: the newest page, which the held window answers.
    Newest,
    /// The rows strictly older than a named row the file carries.
    Below(forge_primitives::TranscriptAnchor),
    /// A frame cursor at or below the window's floor: the rows below the
    /// window's own oldest row.
    BelowWindow,
}

/// **Two cursor forms, one walk.** The window's newest page hands back the
/// frame index its front sits at; every page below the floor hands back a row
/// cursor (`r:<byte>[:<id>]`) minted from the row it reached. A frame cursor
/// cannot be resolved to a row, because the frames the CLI streams and never
/// writes sit between the window's rows, so no arithmetic lines the two
/// numberings up. It is answered from the window's oldest row instead:
/// overlap at worst, never a gap, and a walk from any cursor converges on the
/// same descending chain.
fn below_the_floor(before: Option<&str>, held: &crate::transport::conversation::Held) -> Ask {
    let Some(cursor) = before else {
        return Ask::Newest;
    };
    if let Some(anchor) = row_cursor(cursor) {
        return Ask::Below(anchor);
    }
    match cursor.parse::<usize>() {
        Ok(named) if named <= held.lock().dropped() => Ask::BelowWindow,
        _ => Ask::Held,
    }
}

/// The row a page below the floor is asked for, out of a cursor this server
/// minted: the byte the walk reached next, and the id that row carries where
/// it has one.
fn row_cursor(cursor: &str) -> Option<forge_primitives::TranscriptAnchor> {
    let rest = cursor.strip_prefix("r:")?;
    let (offset, row) = rest.split_once(':').unwrap_or((rest, ""));
    let offset = offset.parse::<u64>().ok()?;
    Some(forge_primitives::TranscriptAnchor { row: row.to_owned(), offset: Some(offset) })
}

/// A row as the cursor a page below the floor hands back.
fn row_cursor_of(offset: u64, row: &str) -> String {
    if row.is_empty() { format!("r:{offset}") } else { format!("r:{offset}:{row}") }
}

/// The row a walk below the window starts from: the window's oldest frame a
/// transcript row names, stepped forward past any result rows a stop there
/// would cut in half.
///
/// **The oldest ROW, not the exact frame a cursor named.** A cursor the drops
/// have passed cannot be resolved to a row - the frames the CLI streams and
/// never writes sit between the window's rows, so the two numberings cannot be
/// lined up by arithmetic - and the window's own oldest row is where the page
/// starts instead: rows between it and the window's front are re-served at
/// worst, which a client reconciles, while a gap is history it cannot ask for
/// again. A window holding no row at all names nothing, and the read answers
/// with the file's own newest rows, which cannot overlap anything the client
/// holds.
///
/// **A stop never lands on a result without its call.** The page below a stop
/// serves rows strictly older than it, so the row just beneath the stop is
/// that page's newest: a result there whose call sits beneath it would reach
/// the fold without its call, drawn with its output nowhere. A stop on a
/// result row therefore steps to the next row, putting that result inside the
/// page - an overlap of a row or two at worst, never a gap.
fn boundary_of(
    held: &crate::transport::conversation::Conversation,
) -> Option<forge_primitives::TranscriptAnchor> {
    // **Only frames the file carries, and only ones it can be asked for.**
    // What the seat holds is not what the file holds: a delivery row was
    // forged here and written nowhere, and a sub-agent's frames are dropped by
    // the row rule at the read. And what the file carries is not always
    // namable: a user row the wire sent without a uuid is a row the read has
    // no search key for. Naming either kind answers the file nothing, and
    // nothing reads as the end of the history - so the walk steps past them
    // to the first row that is both, and names nothing at all when the window
    // holds none.
    let mut rows = held
        .messages()
        .iter()
        .filter(|message| held.has_a_row(message) && row_id(message).is_some());
    let mut anchor = rows.next()?;
    while carries_a_result(anchor) {
        let Some(next) = rows.next() else {
            break;
        };
        anchor = next;
    }
    row_id(anchor).map(|row| forge_primitives::TranscriptAnchor { row, offset: None })
}

/// Whether a frame is a user row carrying a tool result - the frame a stop may
/// not land on, because the call it answers sits just beneath it.
fn carries_a_result(message: &forge_primitives::Message) -> bool {
    let forge_primitives::Message::User { message, .. } = message else {
        return false;
    };
    message
        .content
        .iter()
        .any(|block| matches!(block, forge_primitives::ContentBlock::ToolResult { .. }))
}

/// The id a transcript row names a frame by, for the frames that carry one.
fn row_id(message: &forge_primitives::Message) -> Option<String> {
    match message {
        forge_primitives::Message::User { uuid, .. }
        | forge_primitives::Message::Assistant { uuid, .. } => uuid.clone(),
        forge_primitives::Message::StopHookSummary { uuid, .. }
        | forge_primitives::Message::CompactBoundary { uuid, .. } => Some(uuid.clone()),
        _ => None,
    }
}

/// What a paging read below the held window's floor resolves to.
enum BelowFloor {
    /// A page cut from a span of the session's transcript.
    Page(crate::transport::wire::Page),
    /// The transcript cannot answer this page, which the client draws as the
    /// end of the history: no session to place it, no file, or an anchor the
    /// file does not carry.
    Empty,
    /// The fold over the span did not finish, so the page is refused: an
    /// empty page here would make a seat that has history look like one that
    /// has none.
    Refused,
}

/// Answer a page below the held window's floor from the session's transcript.
///
/// **Rows, by their own bytes.** The read is handed the row the page above
/// stopped at and answers the rows strictly older than it, so the walk crosses
/// a window whose front is a cut inside a running turn - where the frame
/// numbering is unusable - exactly as it crosses one that ends on a turn. The
/// page is cut, rendered and cursor-written by the same [`page`] the held
/// window's pages are, and the cursor it hands back names its own oldest row,
/// so the page below it seeks.
///
/// Every case the transcript cannot answer - no file, an anchor the file does
/// not carry, an anchor further from the end than the cap reaches, a read that
/// failed - is [`BelowFloor::Empty`], the page the client already draws as the
/// end of the history rather than an error it cannot draw at all.
async fn rows_below_floor(
    state: &TransportState,
    seat: &SessionSlot,
    cwd: &std::path::Path,
    below: Option<forge_primitives::TranscriptAnchor>,
    turns: u32,
) -> BelowFloor {
    let surface = Arc::clone(&state.surface);
    let session = seat.clone();
    let cwd = cwd.to_path_buf();
    // The read's own row budget, from the page it is read for: a page is
    // twenty turns, and the heaviest twenty-turn run measured is 2,799 rows.
    let rows = (turns as usize)
        .saturating_mul(forge_workspace::userdata::catalog::scan::SPAN_ROWS_PER_TURN);
    let read = tokio::task::spawn_blocking(move || {
        surface.transcript_rows(&session, &cwd, below.as_ref(), rows)
    })
    .await;
    let Some(span) = read.ok().flatten() else {
        tracing::debug!(
            event_name = "transcript_page_unavailable",
            slot = %seat.display(),
            "the transcript has no rows for this page; the client is told the history ends here",
        );
        return BelowFloor::Empty;
    };
    let folded = tokio::task::spawn_blocking(move || page_of_span(&span, turns)).await;
    if let Ok(page) = folded {
        BelowFloor::Page(page)
    } else {
        tracing::warn!(
            event_name = "transcript_page_fold_failed",
            slot = %seat.display(),
            "the fold over the transcript span did not finish; this page is refused rather than \
             answered empty",
        );
        BelowFloor::Refused
    }
}

/// The page a span of transcript rows answers with, cursors and all.
///
/// **The same [`page`] the held window is cut by**, so a page below the seam
/// is the same kind of page as one above it: whole turns where the span holds
/// them, and within a running turn the run itself. **The cursor is the byte of
/// the page's own oldest row**, so the page below it seeks rather than
/// searching; only a page that reached the file's own first row carries none,
/// which is the one answer a client stops on.
fn page_of_span(
    span: &forge_primitives::TranscriptSpan,
    turns: u32,
) -> crate::transport::wire::Page {
    let rendered = crate::transcript::render(&span.messages);
    let mut page = page(&span.messages, &rendered, 0, None, turns);
    // The page's oldest row, out of the frames it served: the first one whose
    // id names a row of the span. A frame with no id - a forged ending, an
    // attachment's hoist - is stepped over, which errs toward serving a row
    // twice rather than toward skipping one.
    let anchor = page.turns.iter().flat_map(|turn| turn.messages.iter()).find_map(|frame| {
        let uuid = frame["uuid"].as_str().filter(|uuid| !uuid.is_empty())?;
        let at =
            span.messages.iter().position(|message| row_id(message).as_deref() == Some(uuid))?;
        Some((at, uuid))
    });
    page.cursor = match anchor {
        Some((at, uuid)) if !(at == 0 && span.exhausted) => {
            Some(row_cursor_of(span.offsets[at], uuid))
        }
        _ => None,
    };
    page
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
/// One document read's outcome, as the asking client reads it: the answer
/// with the document's bytes base64, or the refusal under the seat that
/// asked - the seat is what lets a client holding several seats drain only
/// its own.
fn document_answer(
    surface: &ViewSurface,
    seat: &SessionSlot,
    outcome: Result<DocumentRead, String>,
) -> ServerMessage {
    let read = match outcome {
        Ok(read) => read,
        Err(why) => {
            return ServerMessage::Error {
                what: "document_read".to_owned(),
                why,
                seat: Some(seat.clone()),
            };
        }
    };
    // The document's own review: the scope key IS the document, so this is
    // the whole of its comments - a store that cannot answer refuses the
    // read rather than drawing an uncommented document.
    let scope = forge_primitives::review::doc_scope_ref(&read.path.to_string_lossy());
    let held = surface.reviews(seat.project(), &scope);
    let (threads, reviews) = match (held.threads, held.reviews) {
        (Ok(threads), Ok(reviews)) => (threads, reviews),
        (threads, reviews) => {
            let why = threads.err().or(reviews.err()).unwrap_or_default();
            return ServerMessage::Error {
                what: "document_read".to_owned(),
                why: format!("the document's review could not be read: {why}"),
                seat: Some(seat.clone()),
            };
        }
    };
    ServerMessage::DocumentRead {
        session: seat.clone(),
        path: read.path.to_string_lossy().into_owned(),
        document_kind: read.kind,
        bytes: base64::engine::general_purpose::STANDARD.encode(&read.bytes),
        reviewable: read.reviewable,
        content_hash: read.content_hash.to_string(),
        threads,
        reviews,
    }
}

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
        DispatchError::NoBrowserHandOffWaiting { .. } => "respond_browser_hand_off",
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

    /// **The transport is what makes a hand-off's ask claimable.** The
    /// per-connection guard is the only thing that moves the attached count,
    /// so a regression that skipped it would refuse every hand-off with
    /// NO_VIEWER_CLIENT - and only a live run would catch it. The guard's
    /// whole life is pinned here: no connection refuses with the reason, an
    /// attached one parks the call on its ask, the answer command releases
    /// it, and the guard's drop takes the claim back.
    #[tokio::test]
    async fn an_attached_connection_is_what_a_hand_off_claims_its_open_on() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fleet = crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])])
            .expect("the fleet builds");
        let root = dir.path().join("proj");
        std::fs::create_dir_all(root.join("docs")).expect("the project tree");
        std::fs::write(root.join("docs/readme.md"), b"# hi\n").expect("the document");

        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };
        // The ask needs a subscriber that can ANSWER it to land on - the
        // raise is a parked prompt, so an observing stream alone fails it
        // closed; a connection's own answering stream stands in for one.
        let mut updates = state.surface.subscribe_client(true);
        let seat = SessionSlot::lead("TestOrg", "proj");

        let refused = fleet
            .workspace()
            .hand_off_document(&seat, "docs/readme.md", false)
            .await
            .expect_err("no connection, so nothing to open on");
        assert_eq!(refused, forge_workspace::documents::NO_VIEWER_CLIENT);

        let attached = AttachedClient::new(&state.surface);
        let handed = {
            let ws = fleet.workspace();
            let seat = seat.clone();
            tokio::spawn(async move { ws.hand_off_document(&seat, "docs/readme.md", false).await })
        };
        let id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match updates.recv().await {
                    Some(SessionUpdate::DocumentHandOffPending { id, .. }) => break id,
                    Some(_) => {}
                    None => panic!("the stream ended before the ask"),
                }
            }
        })
        .await
        .expect("an attached connection is what the hand-off parks on");

        // The answer rides the command a client's done sends, so the
        // dispatch guard and the registry are both on this path.
        state
            .surface
            .dispatch(forge_workspace::Command::AnswerDocumentHandOff {
                key: seat.clone(),
                id,
                answer: forge_primitives::document::DocumentHandOffEnding::Done { review_id: None },
            })
            .expect("the answer lands on the parked ask");
        handed.await.expect("the task joins").expect("and the park releases");

        drop(attached);
        let refused = fleet
            .workspace()
            .hand_off_document(&seat, "docs/readme.md", false)
            .await
            .expect_err("the connection went, so the claim goes with it");
        assert_eq!(refused, forge_workspace::documents::NO_VIEWER_CLIENT);
    }

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
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
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
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
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
    /// connection's take is dropped, and so is an in-process take's - which
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

    /// The claim is reset while a newer take's `DictateStarted` is on its
    /// way: without that, the newer start inherits the older take's
    /// generation, the older take's end retires the claim in the window, and
    /// its `DictateStarted` then stamps nothing - the live take is unclaimed,
    /// every frame drops as `NoTake`, and the meter is dead under a record
    /// that still draws it (#1886).
    #[test]
    fn a_new_claim_resets_before_the_older_take_ends() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        let mut streaming = Streaming::default();
        streaming.claim(&seat);
        streaming.stamp(&seat, 1);

        // Take 2's start, and take 1's end processed in the window before
        // take 2's `DictateStarted`.
        streaming.claim(&seat);
        streaming.retire(&seat, &DictateOutcome::Cancelled, 1);
        let claimed = streaming.seats.len();
        assert_eq!(claimed, 1, "the seat the live take streams into was stripped");

        // And take 2's own `DictateStarted` stamps it, its own end retiring
        // it, whatever order the end lands in.
        streaming.stamp(&seat, 2);
        streaming.retire(&seat, &DictateOutcome::Cancelled, 2);
        assert!(streaming.seats.is_empty(), "take 2's own end must retire the claim");
    }

    /// A stale end leaves a claim a newer take registered under.
    ///
    /// A stop leaves the take finishing while the reader can start the next
    /// take at once, so an older take's end arrives after the newer take has
    /// claimed the seat - and read as an end OF THE CLAIM it strips the seat
    /// the live take streams into: frames drop as `NoTake` and the teardown
    /// has nothing to close (#1886). The generation the claim was stamped
    /// with is what tells the two ends apart.
    #[test]
    fn a_stale_end_leaves_a_newer_claim() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        let mut streaming = Streaming::default();
        streaming.claim(&seat);
        streaming.stamp(&seat, 1);
        // The next take claims the seat while the older one is finishing -
        // the claim stands, so the stamp is what the newer start moves.
        streaming.claim(&seat);
        streaming.stamp(&seat, 2);

        streaming.retire(&seat, &DictateOutcome::Cancelled, 1);
        assert_eq!(
            streaming.seats.len(),
            1,
            "the older take's end stripped the claim the newer take holds",
        );
        assert_eq!(streaming.seats[0].generation, 2, "and the stamp is the newer take's");

        // The claim's own end retires it - and with it the seat is free.
        streaming.retire(&seat, &DictateOutcome::Cancelled, 2);
        assert!(streaming.seats.is_empty(), "the claim's own end must retire it");
    }

    /// A refusal never retires a claim: it answers a start that never ran, so
    /// a live take under the seat keeps its entry (#1880).
    #[test]
    fn a_refusal_never_retires_a_claim() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        let mut streaming = Streaming::default();
        streaming.claim(&seat);

        // A first-ever refused start: the claim was added optimistically for
        // it, and it stays - the take it answered never ran.
        streaming.retire(&seat, &DictateOutcome::Refused { message: "busy".to_owned() }, 0);
        assert_eq!(streaming.seats.len(), 1, "a refused start's end moved the claim");

        // And a refusal arriving under a LIVE take's stamp moves nothing
        // either.
        streaming.stamp(&seat, 1);
        streaming.retire(&seat, &DictateOutcome::Refused { message: "busy".to_owned() }, 0);
        assert_eq!(streaming.seats.len(), 1, "a refusal's end moved the live claim");
        assert_eq!(streaming.seats[0].generation, 1, "and left its stamp alone");
    }

    /// A binary message at the wire's shape: the kind tag, then the
    /// samples.
    fn payload(samples: &[i16]) -> Vec<u8> {
        let mut bytes = vec![super::super::frame::Kind::Dictation.tag()];
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    /// A frame is the samples its bytes carry, addressed to the seats its
    /// own connection started: the seats come from the memory, never from
    /// the message.
    #[test]
    fn a_frame_is_the_samples_its_own_connection_sent() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        let streaming = Streaming { seats: vec![Claim { seat, generation: 0 }], read_aloud: false };
        assert_eq!(
            frame_route(&payload(&[16384]), &streaming),
            FrameRoute::Audio(vec![0.5]),
            "the samples the bytes carry"
        );
    }

    /// A frame before any take - or after one resolved - has nowhere to
    /// go, and is dropped rather than held for a take that may never come.
    #[test]
    fn a_frame_on_a_connection_with_no_take_goes_nowhere() {
        assert_eq!(frame_route(&payload(&[0]), &Streaming::default()), FrameRoute::NoTake);
    }

    /// The read-aloud recording is a destination with no seat at all: a
    /// frame lands while one is running and nowhere when it is not.
    #[test]
    fn a_frame_lands_in_the_read_aloud_recording_with_no_seat() {
        let recording = Streaming { seats: Vec::new(), read_aloud: true };

        assert_eq!(
            frame_route(&payload(&[16384]), &recording),
            FrameRoute::Audio(vec![0.5]),
            "a recording is a destination for a frame with no seat behind it"
        );
        assert_eq!(
            frame_route(&payload(&[16384]), &Streaming::default()),
            FrameRoute::NoTake,
            "a connection that is not recording has no destination for one"
        );
    }

    /// A binary message that is not a frame is refused with its own
    /// reason, so the record says which way it was not a frame.
    #[test]
    fn a_binary_message_that_is_not_a_frame_is_refused_by_its_reason() {
        let seat = SessionSlot::lead("TestOrg", "proj");
        let streaming = Streaming { seats: vec![Claim { seat, generation: 0 }], read_aloud: false };
        assert_eq!(
            frame_route(&[], &streaming),
            FrameRoute::Refused(super::super::frame::Refusal::ShortHeader)
        );
        assert_eq!(
            frame_route(&[9, 0], &streaming),
            FrameRoute::Refused(super::super::frame::Refusal::UnknownKind(9))
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

    /// The read's two outcomes as a viewer draws them: the document with
    /// its bytes base64 and its detected kind, or the error a route renders
    /// where the document would have been - refused under the seat that
    /// asked, so a client holding several seats drains only its own.
    #[test]
    fn a_document_answer_carries_the_bytes_and_the_kind_or_the_refusal() {
        use forge_primitives::document::{DocumentKind, DocumentRead};
        use forge_primitives::review::{
            ReviewAnchor, ReviewAuthor, ReviewComment, ReviewSide, ReviewStatus, ReviewThread,
        };

        let (workspace, _dir) = crate::surface::testing::workspace();
        let surface = crate::surface::ViewSurface::new(Arc::clone(&workspace));
        let seat = SessionSlot::lead("TestOrg", "forge");
        // The document's own review, seeded under the scope key the answer
        // derives from the read's path - and a second document's, which must
        // NOT ride this one's answer.
        let scope = forge_primitives::review::doc_scope_ref("/proj/docs/readme.md");
        let other = forge_primitives::review::doc_scope_ref("/proj/docs/other.md");
        let doc_thread = |id: &str, base_ref: &str| ReviewThread {
            id: id.to_owned(),
            anchor: ReviewAnchor {
                path: "/proj/docs/readme.md".to_owned(),
                side: ReviewSide::New,
                line: 3,
                content_hash: 7,
                context: vec!["^ # hi".to_owned()],
                base_ref: base_ref.to_owned(),
            },
            comments: vec![ReviewComment {
                author: ReviewAuthor::User,
                text: "one line, please".to_owned(),
                at: String::new(),
                review_id: None,
            }],
            status: ReviewStatus::Open,
            created_at: String::new(),
            updated_at: String::new(),
            commit: None,
        };
        assert!(workspace.upsert_review_thread("forge", &scope, doc_thread("d1", &scope)));
        assert!(workspace.upsert_review_thread("forge", &other, doc_thread("d2", &other)));
        workspace
            .submit_review(
                "forge",
                &scope,
                None,
                &["d1".to_owned()],
                SessionSlot::from_str_for_test("reader"),
            )
            .expect("a set for the document's scope");

        let landed = Ok(DocumentRead {
            path: std::path::PathBuf::from("/proj/docs/readme.md"),
            kind: DocumentKind::Markdown,
            bytes: b"# hi\n".to_vec(),
            reviewable: true,
            content_hash: 42,
        });

        let ServerMessage::DocumentRead {
            session,
            path,
            document_kind,
            bytes,
            reviewable,
            content_hash,
            threads,
            reviews,
        } = document_answer(&surface, &seat, landed)
        else {
            panic!("a read that landed is its own answer")
        };
        assert_eq!(session, seat, "the answer names the seat it was read for");
        assert_eq!(path, "/proj/docs/readme.md", "and the resolved absolute path");
        assert_eq!(document_kind, DocumentKind::Markdown, "and the kind the read detected");
        assert_eq!(bytes, "IyBoaQo=", "the bytes base64: what a client decodes is the document");
        assert!(reviewable, "and the flag the hand-off passed");
        assert_eq!(content_hash, "42", "and the document's hash, as decimal digits");
        assert_eq!(threads.len(), 1, "the document's OWN comments ride the answer");
        assert_eq!(threads[0].id, "d1");
        assert_eq!(threads[0].anchor.base_ref, scope, "read under the document's scope");
        assert_eq!(reviews.len(), 1, "and so do its review sets");

        // A scope with nothing in it crosses empty rather than carrying the
        // other document's rows - the control for the read's own scope.
        let clean = Ok(DocumentRead {
            path: std::path::PathBuf::from("/proj/docs/third.md"),
            kind: DocumentKind::Text,
            bytes: b"x".to_vec(),
            reviewable: false,
            content_hash: 8,
        });
        let ServerMessage::DocumentRead { threads, reviews, .. } =
            document_answer(&surface, &seat, clean)
        else {
            panic!("a read that landed is its own answer")
        };
        assert!(threads.is_empty(), "no other document's comments leak in");
        assert!(reviews.is_empty(), "and no review sets either");

        let refused = document_answer(
            &surface,
            &seat,
            Err("a PDF opens read-only; it cannot be handed reviewable".to_owned()),
        );
        let ServerMessage::Error { what, why, seat: named } = refused else {
            panic!("a refused read is an error, not an empty document")
        };
        assert_eq!(what, "document_read", "the refusal names what was asked for");
        assert!(why.contains("read-only"), "and carries the read's own reason: {why}");
        assert_eq!(named, Some(seat), "under the seat that asked");
    }

    /// A transcript of `turns` single-frame turns, each a user row carrying
    /// its own number - so a page cut on a turn boundary is a page whose
    /// cursors are the session's own indices.
    fn transcript_rows(turns: usize) -> Vec<String> {
        (0..turns)
            .map(|at| {
                format!(
                    "{{\"type\":\"user\",\"uuid\":\"u{at}\",\"session_id\":\"s1\",\
                     \"message\":{{\"role\":\"user\",\"content\":\"turn {at}\"}}}}"
                )
            })
            .collect()
    }

    /// The frames the same transcript holds, as the read types them.
    fn transcript_frames(turns: usize) -> Vec<forge_primitives::Message> {
        (0..turns).map(turn_row).collect()
    }

    /// The same transcript as a LIVE conversation held it: each turn's row,
    /// and the `Result` frame the CLI sent and never wrote, in the order the
    /// stream delivered them.
    fn live_frames(turns: usize) -> Vec<forge_primitives::Message> {
        let mut frames = Vec::with_capacity(turns * 2);
        for at in 0..turns {
            frames.push(turn_row(at));
            frames.push(result_frame(at));
        }
        frames
    }

    /// The row a turn opens at.
    fn turn_row(at: usize) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "uuid": format!("u{at}"),
            "message": {"role": "user", "content": format!("turn {at}")},
            "session_id": "s1",
        }))
        .expect("a user frame")
    }

    /// A call: the assistant row that opens a tool call.
    fn calling_row(at: usize) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "uuid": format!("call{at}"),
            "message": {
                "id": format!("m{at}"),
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{
                    "type": "tool_use",
                    "id": format!("tu{at}"),
                    "name": "Bash",
                    "input": {"command": "ls"},
                }],
            },
            "session_id": "s1",
        }))
        .expect("an assistant frame")
    }

    /// The row a call's result arrives on.
    fn resulting_row(at: usize) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "uuid": format!("result{at}"),
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": format!("tu{at}"),
                    "content": "ok",
                }],
            },
            "session_id": "s1",
        }))
        .expect("a user frame")
    }

    /// A turn's `Result` frame: what the live stream sends and the file has
    /// no row for.
    fn result_frame(at: usize) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "uuid": format!("r{at}"),
            "session_id": "s1",
            "is_error": false,
            "num_turns": at + 1,
            "duration_ms": 12,
            "duration_api_ms": 9,
        }))
        .expect("a result frame")
    }

    /// The words each turn of a page opened on.
    fn turn_texts(page: &crate::transport::wire::Page) -> Vec<String> {
        page.turns
            .iter()
            .map(|turn| {
                turn.messages
                    .iter()
                    .find_map(|frame| {
                        frame["message"]["content"][0]["text"].as_str().map(str::to_owned)
                    })
                    .unwrap_or_default()
            })
            .collect()
    }

    /// **A page below the window hands back the row it stopped at.** The
    /// cursor names its own oldest row - the byte it starts at, and the id
    /// that byte is verified by - so the page below it seeks rather than
    /// searching; a page whose oldest row is the file's own first row carries
    /// none, which is the one answer a client stops on.
    #[test]
    fn a_transcript_page_hands_the_row_below_it_back_as_its_cursor() {
        // The rows as a page below the floor sees them: the newest 12 of a
        // conversation's own rows, each with the byte it starts at.
        let transcript = transcript_frames(160);
        let from = 148_usize;
        let span = forge_primitives::TranscriptSpan {
            messages: transcript[from..].to_vec(),
            offsets: (from as u64..160).map(|at| at * 100).collect(),
            exhausted: false,
        };
        let page = page_of_span(&span, 4);

        assert_eq!(
            turn_texts(&page),
            vec!["turn 156", "turn 157", "turn 158", "turn 159"],
            "the newest turns of the span",
        );
        assert_eq!(
            page.cursor.as_deref(),
            Some("r:15600:u156"),
            "and a cursor naming its oldest row: the byte it starts at, and its id",
        );

        // **A span that ran out is not the end of the history.** A page cut
        // from a span holding fewer turns than were asked for still carries
        // the row it stopped at; only a page that reached the file's own first
        // row leaves the cursor `None`.
        let short = forge_primitives::TranscriptSpan {
            messages: transcript[100..106].to_vec(),
            offsets: (100_u64..106).map(|at| at * 100).collect(),
            exhausted: false,
        };
        let short_page = page_of_span(&short, 8);
        assert_eq!(short_page.turns.len(), 6, "precondition: the span holds six turns");
        assert_eq!(
            short_page.cursor.as_deref(),
            Some("r:10000:u100"),
            "a short span's page carries on from the row the span began with",
        );
        let ended = forge_primitives::TranscriptSpan { exhausted: true, ..short };
        assert!(
            page_of_span(&ended, 8).cursor.is_none(),
            "and only a page reaching the transcript's first row ends the walk",
        );
    }

    /// A page below the window's floor is read from the session's transcript,
    /// and the cases the transcript cannot answer are the empty page rather
    /// than an error a client cannot draw.
    #[tokio::test]
    async fn a_page_below_the_floor_comes_from_the_transcript() {
        let dir = tempfile::tempdir().expect("a config dir of its own");
        let fleet =
            crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])]).expect("a fleet");
        fleet.start("TestOrg", "proj").expect("a live lead session");
        let rows = transcript_rows(80);
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &rows.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .expect("the session's transcript");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };
        let seat = SessionSlot::lead("TestOrg", "proj");
        let cwd = state.surface.roster().cwd_for(&seat).expect("the seat's own directory");

        // The row the page above stopped at, by id alone: what a walk hands
        // over on its first ask below the floor.
        let below = forge_primitives::TranscriptAnchor { row: "u40".to_owned(), offset: None };
        let BelowFloor::Page(page) = rows_below_floor(&state, &seat, &cwd, Some(below), 4).await
        else {
            panic!("the transcript answers a page below the floor")
        };
        assert_eq!(
            turn_texts(&page),
            vec!["turn 36", "turn 37", "turn 38", "turn 39"],
            "the newest turns below the row the walk stopped at",
        );

        // **The walk continues from the cursor it was handed**, which carries
        // the row's own byte: the page below it is the one directly above.
        let cursor = page.cursor.clone().expect("a cursor the client asks below with");
        let anchor = row_cursor(&cursor).expect("a row cursor, which is what a page below mints");
        assert_eq!(anchor.row, "u36", "naming the page's own oldest row");
        assert!(anchor.offset.is_some(), "with the byte that row starts at");
        let BelowFloor::Page(below_page) =
            rows_below_floor(&state, &seat, &cwd, Some(anchor), 4).await
        else {
            panic!("the walk answers the page below the one it served")
        };
        assert_eq!(
            turn_texts(&below_page),
            vec!["turn 32", "turn 33", "turn 34", "turn 35"],
            "the turns directly above the page before it",
        );

        // A row the file does not carry: there is no page to cut, so the
        // client is told the history ends there.
        let stranger =
            forge_primitives::TranscriptAnchor { row: "nowhere".to_owned(), offset: None };
        assert!(
            matches!(
                rows_below_floor(&state, &seat, &cwd, Some(stranger), 4).await,
                BelowFloor::Empty
            ),
            "an anchor the file does not carry is the empty page",
        );

        // And a session whose file is gone.
        let key = forge_workspace::userdata::catalog::scan::project_key_for_directory(cwd.to_str());
        let transcript = std::fs::read_dir(dir.path().join("projects").join(key))
            .expect("the project's transcript dir")
            .flatten()
            .map(|entry| entry.path())
            .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .expect("the session's transcript");
        std::fs::remove_file(transcript).expect("the file goes");
        assert!(
            matches!(rows_below_floor(&state, &seat, &cwd, None, 4).await, BelowFloor::Empty),
            "a session whose file is gone is the empty page too",
        );
    }

    /// **A page below a window whose front is a rowless frame starts at the
    /// window's oldest row.** A live conversation counts every frame the
    /// session emitted - a turn's `Result` among them - and the file counts
    /// only rows, so the window's front can sit among frames no transcript
    /// has. The walk starts from the oldest ROW the window still carries and
    /// serves the rows strictly beneath it: no row is stepped over, and a
    /// frame the file never wrote costs the walk nothing.
    #[tokio::test]
    async fn a_page_below_a_rowless_window_serves_the_rows_beneath_it() {
        // A transcript of 2,600 turns, and the live conversation of the same
        // session: every turn's row, then the `Result` frame the CLI sent and
        // never wrote.
        const TURNS: usize = 2_600;
        let dir = tempfile::tempdir().expect("a config dir of its own");
        let fleet =
            crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])]).expect("a fleet");
        fleet.start("TestOrg", "proj").expect("a live lead session");
        let rows = transcript_rows(TURNS);
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &rows.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .expect("the session's transcript");
        let held = crate::transport::conversation::Held::new(
            crate::transport::conversation::Conversation::new(live_frames(TURNS), 0, 0),
        );
        assert!(
            held.lock().dropped() > 0,
            "precondition: the live conversation outgrew its window"
        );
        assert!(
            held.lock()
                .messages()
                .iter()
                .any(|message| !forge_workspace::has_a_transcript_row(message)),
            "precondition: the conversation carries frames no transcript row has",
        );
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };
        let seat = SessionSlot::lead("TestOrg", "proj");
        let cwd = state.surface.roster().cwd_for(&seat).expect("the seat's own directory");

        let boundary = boundary_of(&held.lock()).expect("the window carries rows");
        let oldest =
            boundary.row.trim_start_matches('u').parse::<usize>().expect("a row id names its turn");
        assert!(boundary.offset.is_none(), "named by id alone, which is what the search is for");
        let BelowFloor::Page(page) = rows_below_floor(&state, &seat, &cwd, Some(boundary), 4).await
        else {
            panic!("the transcript answers a page below the window's oldest row")
        };
        assert_eq!(
            turn_texts(&page),
            (oldest - 4..oldest).map(|at| format!("turn {at}")).collect::<Vec<_>>(),
            "the rows directly beneath the window's oldest row, with no row stepped over for the \
             result frames in between",
        );
    }

    /// **A stop never lands on a result without its call.** The page below a
    /// stop serves the rows strictly older than it, so the row just beneath
    /// one is that page's newest - and a result there, with its call beneath
    /// it, would reach the fold without its call, drawn with its output
    /// nowhere. The stop steps to the next row instead, which puts that result
    /// inside the page: an overlap a client reconciles, never a cut pair.
    #[test]
    fn a_stop_never_lands_on_a_result_without_its_call() {
        // A conversation of calls and their results, one frame longer than the
        // window's cap so the drop runs and its count cut lands on a result.
        let mut frames: Vec<forge_primitives::Message> = Vec::new();
        for at in 0..4_001 {
            frames.push(calling_row(at));
            frames.push(resulting_row(at));
        }
        // One frame past the cap's edge, so the drop runs - and none of these
        // rows opens a turn, so the cut lands where the count says rather than
        // on a turn's first frame.
        frames.push(calling_row(9_999));
        let held = crate::transport::conversation::Held::new(
            crate::transport::conversation::Conversation::new(frames, 0, 0),
        );
        let window = held.lock();
        assert!(window.dropped() > 0, "precondition: the conversation outgrew its window");
        assert!(
            carries_a_result(window.messages().first().expect("a window")),
            "precondition: the count cut landed on a result row",
        );

        let boundary = boundary_of(&window).expect("the window carries rows");

        assert_eq!(
            boundary.row, "call2002",
            "the stop stepped past the result the cut landed on, so the page below carries it \
             with the call that produced it",
        );
    }

    /// **A page read from the middle of a large transcript seeks by its
    /// cursor.** The read's window reaches back a bounded number of bytes, so
    /// the span it hands back begins part-way in - and its cursor is the byte
    /// of the row it stopped at, which the page below seeks to rather than
    /// searching from the file's end again.
    #[tokio::test]
    async fn a_page_from_the_middle_of_a_large_transcript_seeks_by_its_cursor() {
        const ROWS: usize = 13_000;
        let dir = tempfile::tempdir().expect("a config dir of its own");
        let fleet =
            crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])]).expect("a fleet");
        fleet.start("TestOrg", "proj").expect("a live lead session");
        let rows = transcript_rows(ROWS);
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &rows.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .expect("the session's transcript");
        let held = crate::transport::conversation::Held::new(
            crate::transport::conversation::Conversation::new(transcript_frames(ROWS), 0, 0),
        );
        assert!(held.lock().dropped() > 0, "precondition: the copy outgrew its window");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };
        let seat = SessionSlot::lead("TestOrg", "proj");
        let cwd = state.surface.roster().cwd_for(&seat).expect("the seat's own directory");

        let boundary = boundary_of(&held.lock()).expect("the window carries rows");
        let BelowFloor::Page(page) = rows_below_floor(&state, &seat, &cwd, Some(boundary), 4).await
        else {
            panic!("a page below the floor is answered")
        };
        let cursor = page.cursor.clone().expect("a cursor the client asks below with");
        let anchor = row_cursor(&cursor).expect("a row cursor");
        assert!(anchor.offset.is_some(), "carrying the byte that row starts at, for the seek");
        let oldest =
            anchor.row.trim_start_matches('u').parse::<usize>().expect("a row id names its turn");
        assert_eq!(
            turn_texts(&page).first().map(String::as_str),
            Some(format!("turn {oldest}").as_str()),
            "the page begins at the row its cursor names",
        );

        let BelowFloor::Page(below) = rows_below_floor(&state, &seat, &cwd, Some(anchor), 4).await
        else {
            panic!("the page below it is answered too")
        };
        assert_eq!(
            turn_texts(&below).last().map(String::as_str),
            Some(format!("turn {}", oldest - 1).as_str()),
            "and the page below ends on the turn directly beneath where the first began",
        );
    }

    /// A sub-agent's frame: a row inside a `Task` call, which the file counts
    /// no row for and the row rule drops.
    fn subagent_frame(at: usize) -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "assistant",
            "uuid": format!("sub{at}"),
            "parent_tool_use_id": "toolu_dispatch",
            "session_id": "s1",
            "message": {
                "id": format!("msub{at}"),
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{"type": "text", "text": format!("inside the sub-agent {at}")}],
            },
        }))
        .expect("an assistant frame")
    }

    /// **A window front the file cannot name still pages.**
    ///
    /// The walk names the file one row - its boundary - and two kinds of frame
    /// carry an id the file has no row for: a row the transport FORGED for
    /// display (a delivery), and a sub-agent's frame, which the row rule drops
    /// at the read. Naming either used to answer the read nothing, and nothing
    /// reads as the end of the history: a silent gap, on exactly the windows
    /// this walk exists for - a forged delivery row IS a turn opener, so the
    /// count cut lands on one, and a running turn streaming sub-agent work
    /// leaves sub-agent frames at its front.
    #[tokio::test]
    async fn a_window_front_the_file_cannot_name_still_pages() {
        let dir = tempfile::tempdir().expect("a config dir of its own");
        let fleet =
            crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])]).expect("a fleet");
        fleet.start("TestOrg", "proj").expect("a live lead session");
        let seat = SessionSlot::lead("TestOrg", "proj");

        // **Arm one: a forged row at the front, and a real row just under it.**
        // The seed is sized so the count cut's next landing is the frame right
        // after it (measured: the cut parks on the newest turn, then steps in
        // thousands), and the forged row is appended onto that landing - what
        // a delivery arriving mid-turn does in production.
        let mut frames = live_frames(2_600);
        frames.extend((0..999).map(result_frame));
        let conversations = crate::transport::conversation::Conversations::new();
        conversations
            .insert(&seat, crate::transport::conversation::Conversation::new(frames, 0, 0));
        conversations.apply(&SessionUpdate::CronPromptAppended {
            key: seat.clone(),
            text: "the cron fired".to_owned(),
            uuid: "cap-cron".to_owned(),
            cron_id: "c1".to_owned(),
            description: None,
        });
        let following = resulting_row(9_999);
        conversations.apply(&SessionUpdate::ChatAppended {
            key: seat.clone(),
            msg: following.clone(),
            origin: None,
        });
        for at in 0..5_000_usize {
            let window = conversations.get(&seat).expect("the seat is held");
            if row_id(window.lock().messages().first().expect("a front")).as_deref()
                == Some("cap-cron")
            {
                break;
            }
            drop(window);
            conversations.apply(&SessionUpdate::ChatAppended {
                key: seat.clone(),
                msg: result_frame(at),
                origin: None,
            });
        }
        let held = conversations.get(&seat).expect("the seat is held");
        let boundary = {
            let window = held.lock();
            assert_eq!(
                row_id(window.messages().first().expect("a front")).as_deref(),
                Some("cap-cron"),
                "precondition: the count cut landed on the forged row",
            );
            boundary_of(&window).expect("the window carries a row the file has")
        };
        assert_eq!(
            boundary.row, "result9999",
            "the forged row is passed over for the first row under it the file carries",
        );

        // And the read answers a page from that boundary rather than the empty
        // one: the walk does not end where the file simply could not be asked.
        let rows = transcript_rows(2_600);
        let mut rows = rows;
        rows.push(serde_json::to_string(&following).expect("the following row as a line"));
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &rows.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .expect("the session's transcript");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(conversations),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };
        let cwd = state.surface.roster().cwd_for(&seat).expect("the seat's own directory");
        let BelowFloor::Page(page) = rows_below_floor(&state, &seat, &cwd, Some(boundary), 4).await
        else {
            panic!("the transcript answers a page below a front it was named a real row for")
        };
        assert!(!page.turns.is_empty(), "and the page is not the empty one");

        // **Arm two: a sub-agent frame at the front, the running turn's own
        // shape.** A window holding no row the file carries names nothing, and
        // the read answers with the file's own newest rows - not the empty
        // page a failed search would hand back as the end of the history.
        let mut frames = live_frames(2_600);
        frames.extend((0..5_400).map(subagent_frame));
        let held = crate::transport::conversation::Held::new(
            crate::transport::conversation::Conversation::new(frames, 0, 0),
        );
        let window = held.lock();
        let front = window.messages().first().expect("a front");
        assert!(
            row_id(front).is_some_and(|id| id.starts_with("sub")),
            "precondition: the count cut landed inside the sub-agent run",
        );
        assert!(
            !forge_workspace::has_a_transcript_row(front),
            "precondition: which the file counts no row for",
        );
        assert!(
            boundary_of(&window).is_none(),
            "and the boundary passes over every such frame, naming nothing to the file: {:?}",
            boundary_of(&window),
        );
    }

    /// A row with no id at all: a user frame the wire carries without a uuid
    /// (the queued-prompt/typed-prompt shape), which no transcript row can be
    /// named by.
    fn unnameable_row() -> forge_primitives::Message {
        serde_json::from_value(serde_json::json!({
            "type": "user",
            "session_id": "s1",
            "message": {"role": "user", "content": "no id on this row"},
        }))
        .expect("a user frame")
    }

    /// Whether a frame is the fixture's uuid-less row.
    fn is_unnameable(message: &forge_primitives::Message) -> bool {
        serde_json::to_value(message)
            .ok()
            .and_then(|value| value["message"]["content"][0]["text"].as_str().map(str::to_owned))
            .as_deref()
            == Some("no id on this row")
    }

    /// **A front the file cannot be NAMED still pages to the row behind it.**
    ///
    /// A frame the file carries can still be one the file cannot be asked for:
    /// a user row the wire sent without a uuid (`row_id` is `None`) is a row
    /// the read has nothing to search by, so naming it answers nothing - and
    /// nothing reads as the end of the history. The boundary must step past it
    /// to the first row that is both carried AND namable, rather than falling
    /// all the way to the file's newest rows - which a client already holds,
    /// so the answer is a redundant page instead of the one under the front.
    #[tokio::test]
    async fn a_front_the_file_cannot_be_named_by_still_pages_to_the_row_behind_it() {
        let dir = tempfile::tempdir().expect("a config dir of its own");
        let fleet =
            crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])]).expect("a fleet");
        fleet.start("TestOrg", "proj").expect("a live lead session");
        let seat = SessionSlot::lead("TestOrg", "proj");

        // The seed sizes the count cut's next landing for the first append,
        // the same construction the forged-row arm uses.
        let mut frames = live_frames(2_600);
        frames.extend((0..999).map(result_frame));
        let conversations = crate::transport::conversation::Conversations::new();
        conversations
            .insert(&seat, crate::transport::conversation::Conversation::new(frames, 0, 0));
        conversations.apply(&SessionUpdate::ChatAppended {
            key: seat.clone(),
            msg: unnameable_row(),
            origin: None,
        });
        conversations.apply(&SessionUpdate::ChatAppended {
            key: seat.clone(),
            msg: resulting_row(9_999),
            origin: None,
        });
        for at in 0..5_000_usize {
            let window = conversations.get(&seat).expect("the seat is held");
            let front = window.lock().messages().first().cloned().expect("a front");
            if is_unnameable(&front) {
                break;
            }
            conversations.apply(&SessionUpdate::ChatAppended {
                key: seat.clone(),
                msg: result_frame(at),
                origin: None,
            });
        }
        let held = conversations.get(&seat).expect("the seat is held");
        let window = held.lock();
        let front = window.messages().first().expect("a front");
        assert!(
            is_unnameable(front),
            "precondition: the count cut landed on the row with no id ({:?})",
            row_id(front),
        );
        let boundary = boundary_of(&window);
        assert_eq!(
            boundary.as_ref().map(|anchor| anchor.row.as_str()),
            Some("result9999"),
            "the boundary names the first row behind the front that the file both carries and can \
             be asked for: {boundary:?}",
        );
    }

    /// **The walk reaches the transcript's beginning, page after page.** Each
    /// page's cursor is the byte of the row it stopped at - the page below
    /// seeks to it - and the walk keeps descending through the result frames
    /// the file never wrote until the transcript's own first row ends it. This
    /// is the shape that died after six pages when the two sides counted
    /// different things.
    #[tokio::test]
    async fn a_walk_below_the_floor_reaches_the_transcripts_start() {
        const TURNS: usize = 2_600;
        let dir = tempfile::tempdir().expect("a config dir of its own");
        let fleet =
            crate::testing::Fleet::in_dir(dir.path(), &[("TestOrg", &["proj"])]).expect("a fleet");
        fleet.start("TestOrg", "proj").expect("a live lead session");
        let rows = transcript_rows(TURNS);
        fleet
            .seed_transcript(
                "TestOrg",
                "proj",
                "lead",
                &rows.iter().map(String::as_str).collect::<Vec<_>>(),
            )
            .expect("the session's transcript");
        let held = crate::transport::conversation::Held::new(
            crate::transport::conversation::Conversation::new(live_frames(TURNS), 0, 0),
        );
        assert!(
            held.lock()
                .messages()
                .iter()
                .any(|message| !forge_workspace::has_a_transcript_row(message)),
            "precondition: the walk crosses frames with no row",
        );
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser: Arc::new(forge_workspace::browser::BrowserRelay::new()),
        };
        let seat = SessionSlot::lead("TestOrg", "proj");
        let cwd = state.surface.roster().cwd_for(&seat).expect("the seat's own directory");

        let mut below = Some(boundary_of(&held.lock()).expect("the window carries rows"));
        let mut pages = 0_usize;
        let reached = loop {
            let BelowFloor::Page(page) = rows_below_floor(&state, &seat, &cwd, below, 20).await
            else {
                panic!("the walk's page {pages} is answered from the transcript");
            };
            let texts = turn_texts(&page);
            assert!(!texts.is_empty(), "the walk's page {pages} holds turns");
            pages += 1;
            assert!(pages < 200, "the walk reached the transcript's beginning");
            match page.cursor.as_deref().and_then(row_cursor) {
                Some(next) => below = Some(next),
                None => break texts,
            }
        };
        assert_eq!(
            reached.first().map(String::as_str),
            Some("turn 0"),
            "and the last page it served runs down to the transcript's own first turn",
        );
    }
}
