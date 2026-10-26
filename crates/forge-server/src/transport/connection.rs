//! One connection: the upgrade, what the server says first, and the loop
//! that answers what a client asks for.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::extract::{ConnectInfo, State};
use axum::response::Response;
use futures_util::{SinkExt as _, StreamExt};
use tokio::sync::{mpsc, oneshot};

use base64::Engine as _;
use forge_primitives::SessionSlot;
use forge_primitives::browser::{BrowserFrame, BrowserTabList, BrowserViewState};
use forge_primitives::document::DocumentRead;
use forge_workspace::browser::BrowserAttachment;
use serde_json::json;

use super::batch::{self, Batch};
use super::envelope::{ClientMessage, ClientSettings, SeatFrameWire, ServerMessage, Subject};
use super::frame::FrameTransport as _;
use super::wire::{
    FileContentWire, OpenTimings, cap_message_record, cap_tool_records, conversation_for,
    encode_subject, encode_subject_timed, floor_cursor, page,
};
use super::{PROTOCOL_VERSION, TransportState, WRITE_BOUND, WriteStalled, write_within};
use crate::live::Live;
use crate::surface::{DictateOutcome, ViewSurface};
use crate::{Command, DispatchError, SeatUpdate, SessionUpdate};

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

/// What this connection is watching, and which of it is on screen.
///
/// The two travel together because only a subject this connection watches can
/// reach it, and only a seat can be shown. **They mean different things**: the
/// subscription carries a seat's frames, and the showing is what spends its
/// marks.
#[derive(Default)]
struct Watching {
    subjects: Vec<Subject>,
    showing: Option<SessionSlot>,
    /// Whether the connection holds the showing's attach on [`Live`].
    ///
    /// **The two halves can arrive in either order.** Both clients subscribe
    /// first on a resume, but the route effect that sends `showing` races the
    /// page's mount, so a showing can still land before its subscribe. A mark
    /// is spent only when the pair is complete: the showing that lands first
    /// is remembered and the subscribe that completes it spends - never both,
    /// or the count would be two.
    showing_attached: bool,
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

/// How often a connection is asked to prove it is still there.
///
/// **The socket carried no keepalive**, so a peer that vanished without
/// closing - a slept machine, a wedged process, a network that went away -
/// left its viewer attached and any companion waiting, with neither side
/// told the other was gone. Every WebSocket client answers a Ping without its
/// app's involvement, so the answer is the proof.
pub(crate) const HEARTBEAT_EVERY: Duration = Duration::from_secs(15);

/// How long a ping may go unanswered before the connection is reaped. Four
/// missed beats: long enough that a busy peer is never cut, short enough that
/// a role is not held by a corpse for the minutes a TCP retransmission timeout
/// takes. **Chosen, not measured**: the one number it changes for a person is
/// how fast a call on a paused host fails (a tool call's own bound is longer).
pub(crate) const HEARTBEAT_DEAD: Duration = Duration::from_secs(60);

/// One connection's keepalive: the ping it has out, if any.
///
/// **A silent read is not evidence of a dead peer until something has been
/// asked of it.** A connection can spend longer than the bound in a single
/// write - a 3.5 MB snapshot to a slow client was measured at 31.6 s - and
/// through that stall no beat runs and no ping goes out, so a rule that
/// counted "no frame read" as silence would kill a live client on the first
/// tick after the write. What is tracked here is the one thing the peer was
/// actually asked for: an unanswered ping.
#[derive(Debug, Default)]
struct Keepalive {
    /// When the outstanding ping was sent, `None` when none is out. Any
    /// inbound frame clears it, because any frame is the peer answering in
    /// some form. A lock rather than a cell: [`Keepalive::ping`] holds
    /// `&self` across its send, and a cell is not `Sync`, which would make
    /// the connection's future not `Send` for the sake of a stamp.
    sent: std::sync::Mutex<Option<Instant>>,
}

/// What one beat should do.
#[derive(Debug, PartialEq, Eq)]
enum Beat {
    /// Send a ping, and wait for an answer.
    Ping,
    /// A ping is out and still inside the bound.
    Wait,
    /// A ping went unanswered past the bound: the peer is not reading.
    Reap,
}

impl Keepalive {
    /// The outstanding ping's send time, if one is out.
    fn sent(&self) -> Option<Instant> {
        *self.sent.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A frame arrived, so whatever was asked, the peer is answering.
    fn heard(&self) {
        *self.sent.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    /// What this beat should do. **Asking does not stamp the deadline**: a
    /// send that queues behind a full buffer reaches the peer late, and a
    /// deadline taken before it would have expired before the peer saw it.
    fn beat(&self, now: Instant, dead: Duration) -> Beat {
        match self.sent() {
            Some(at) if now.duration_since(at) >= dead => Beat::Reap,
            Some(_) => Beat::Wait,
            None => Beat::Ping,
        }
    }

    /// Send the ping and start its deadline, **in that order**.
    ///
    /// The stamp is taken here, only once the send answers, so no caller can
    /// stamp it early: a ping still held in a full buffer owes nothing until
    /// it reaches the peer. The send is a `Sink` rather than a closure so a
    /// test can hold it and watch the state at each side of the await.
    async fn ping<S>(&self, sink: &mut S) -> anyhow::Result<()>
    where
        S: futures_util::Sink<Message, Error = axum::Error> + Unpin + Send,
    {
        write_within(WRITE_BOUND, async {
            sink.send(Message::Ping(tokio_tungstenite::tungstenite::Bytes::new()))
                .await
                .map_err(anyhow::Error::from)
        })
        .await?;
        *self.sent.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
        Ok(())
    }
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

/// One tab this connection is viewing: its profile, its id, its frames and
/// its state.
struct Viewing {
    key: String,
    id: u16,
    /// The tab this connection is on. **Every frame, gesture and state frame
    /// is this tab's**; another tab's paint is not this viewer's to draw.
    tab: u16,
    frames: tokio::sync::watch::Receiver<Option<BrowserFrame>>,
    view: tokio::sync::watch::Receiver<BrowserViewState>,
    nav: tokio::sync::watch::Receiver<Option<forge_primitives::browser::BrowserNavState>>,
    failures: tokio::sync::watch::Receiver<Option<String>>,
    files: tokio::sync::watch::Receiver<Option<forge_primitives::browser::BrowserFileAsk>>,
    downloads: tokio::sync::broadcast::Receiver<forge_primitives::browser::BrowserDownload>,
    audio: tokio::sync::broadcast::Receiver<forge_primitives::browser::BrowserAudio>,
    /// The last sequence written to the socket, so a change that re-announces
    /// the same newest frame is not sent twice.
    last_frame: Option<u32>,
}

impl Viewing {
    fn new(attachment: BrowserAttachment) -> Self {
        Self {
            key: attachment.key,
            id: attachment.id,
            tab: attachment.tab,
            frames: attachment.frames,
            view: attachment.view,
            nav: attachment.nav,
            failures: attachment.failures,
            files: attachment.files,
            downloads: attachment.downloads,
            audio: attachment.audio,
            last_frame: None,
        }
    }
}

/// This connection's browser half: the tab it draws and the strip it follows.
///
/// **One value rather than two arguments**, because the read loop and its
/// per-message handler already carry the connection's whole mutable state and
/// two more fields would push both past the arity a reader can hold.
#[derive(Default)]
struct BrowserViewing {
    viewing: Option<Viewing>,
    tabs: Option<tokio::sync::watch::Receiver<BrowserTabList>>,
}

/// What a viewer has to say to the loop.
enum ViewEvent {
    Frame(BrowserFrame),
    State(BrowserViewState),
    Nav(forge_primitives::browser::BrowserNavState),
    Failed(String),
    File(forge_primitives::browser::BrowserFileAsk),
    Download(forge_primitives::browser::BrowserDownload),
    Audio(forge_primitives::browser::BrowserAudio),
}

/// The next thing this connection's viewer has for the loop.
///
/// **A connection with no viewer parks here**, so its branch is inert
/// rather than ending the loop - the same shape [`next_update`] uses. The
/// watch holds one value per source, so a viewer that falls behind is
/// served the newest frame rather than a queue.
async fn next_view_event(viewing: &mut Option<Viewing>) -> ViewEvent {
    let Some(viewing) = viewing.as_mut() else { return std::future::pending().await };
    loop {
        // **A sender that ends says which one it was.** The tab's own state
        // and history end with the tab; the profile's senders end with the
        // browser. One refusal for both told a viewer on a closed tab that
        // the whole browser had gone.
        let ready = tokio::select! {
            changed = viewing.frames.changed() => changed.map(|()| 0_u8).map_err(|_| 10_u8),
            changed = viewing.view.changed() => changed.map(|()| 1_u8).map_err(|_| 11_u8),
            changed = viewing.failures.changed() => changed.map(|()| 2_u8).map_err(|_| 12_u8),
            changed = viewing.nav.changed() => changed.map(|()| 3_u8).map_err(|_| 13_u8),
            changed = viewing.files.changed() => changed.map(|()| 4_u8).map_err(|_| 14_u8),
            download = viewing.downloads.recv() => match download {
                Ok(download) => return ViewEvent::Download(download),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    tracing::warn!(
                        "a viewer fell behind a download and lost it (event_name \
                         browser_download_lagged)"
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    std::future::pending().await
                }
            },
            packet = viewing.audio.recv() => match packet {
                // **Only this viewer's tab is heard.** One profile's audio
                // broadcast carries every tab, and another page's sound is
                // not this viewer's to play.
                Ok(packet) if packet.tab == viewing.tab => return ViewEvent::Audio(packet),
                // Another tab's packet, or one a viewer fell behind: it
                // loses packets, not the stream, and the next it can take
                // resumes playback.
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    std::future::pending().await
                }
            },
        };
        match ready {
            Ok(0) => {
                let frame = viewing.frames.borrow_and_update().clone();
                // **The watch is this tab's own**, so a frame from another
                // tab cannot arrive here; the tab check is the wire's own
                // identity, kept against a frame that names another.
                if let Some(frame) = frame
                    && frame.tab == viewing.tab
                    && viewing.last_frame != Some(frame.seq)
                {
                    viewing.last_frame = Some(frame.seq);
                    return ViewEvent::Frame(frame);
                }
            }
            Ok(1) => return ViewEvent::State(*viewing.view.borrow_and_update()),
            Ok(2) => {
                if let Some(reason) = viewing.failures.borrow_and_update().clone() {
                    return ViewEvent::Failed(reason);
                }
            }
            Ok(3) => {
                if let Some(nav) = *viewing.nav.borrow_and_update() {
                    return ViewEvent::Nav(nav);
                }
            }
            Ok(4) => {
                if let Some(ask) = *viewing.files.borrow_and_update() {
                    return ViewEvent::File(ask);
                }
            }
            Ok(_) => {}
            // **The tab's own senders go when the tab does**, and the
            // refusal names it so the client lowers that surface and can pick
            // another. The profile's senders going is the browser's end.
            // **A host's death is the cause when it has one**: the failure
            // watch is published before the per-tab senders close, and a
            // viewer that woke on the close alone would read the tab as
            // closed and never hear why.
            Err(10 | 11 | 13) => {
                if let Some(reason) = viewing.failures.borrow().clone() {
                    return ViewEvent::Failed(reason);
                }
                return ViewEvent::Failed(format!("tab {} was closed", viewing.tab));
            }
            Err(_) => {
                if let Some(reason) = viewing.failures.borrow().clone() {
                    return ViewEvent::Failed(reason);
                }
                return ViewEvent::Failed("the browser was closed".to_owned());
            }
        }
    }
}

/// The next tab list this connection asked to follow, or a future that never
/// resolves while it follows none - the same shape [`next_view_event`] uses,
/// so a connection with no strip parks rather than ending the loop.
async fn next_tab_change(
    tabs: &mut Option<tokio::sync::watch::Receiver<BrowserTabList>>,
) -> Option<BrowserTabList> {
    let Some(tabs) = tabs.as_mut() else { return std::future::pending().await };
    match tabs.changed().await {
        Ok(()) => Some(tabs.borrow_and_update().clone()),
        // The profile's rows are gone with the service: nothing more will
        // change, and the branch stays out of the way.
        Err(_) => std::future::pending().await,
    }
}

/// The largest message a client may send.
///
/// **The cap is abuse's, not a prompt's.** A command carries a session's
/// whole prompt text, and 16 MiB is the bound the socket has always
/// allowed; the dictation payload's 16 KiB is a frame size, and applying it
/// to every inbound message tore the connection on a large prompt - with no
/// reply on a prompt, the loss was silent (measured on the dev stack).
const MAX_CLIENT_MESSAGE_BYTES: usize = 16 * 1024 * 1024 + 10;

/// Take the upgrade and give the connection its own task.
///
/// Both tungstenite limits are raised to the one inbound cap: the frame
/// limit (16 MiB by default here, exactly the cap) and the message limit
/// (64 MiB by default, lowered to the cap so one message cannot be several
/// frames past what a single frame may be). A frame the cap refuses never
/// reaches [`super::frame`]; anything under it does, and a dictation payload
/// a shade too big is refused there with its reason rather than tearing the
/// socket.
pub async fn upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<TransportState>>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
) -> Response {
    let limit = MAX_CLIENT_MESSAGE_BYTES;
    ws.max_frame_size(limit)
        .max_message_size(limit)
        .on_upgrade(move |socket| greet(socket, state, peer))
}

/// This connection as the log names it: the number its takes are told apart
/// by, and the address it is attached from.
#[derive(Clone, Copy)]
struct Caller {
    id: u64,
    peer: std::net::SocketAddr,
}

/// Greet, then serve the client until it goes.
///
/// The greeting is what tells a client it is speaking to a forge rather
/// than to whatever else holds the port, and it carries the settings the
/// client draws with - so the client is configured before it draws anything
/// rather than discovering a theme later.
async fn greet<S>(mut socket: S, state: Arc<TransportState>, peer: std::net::SocketAddr)
where
    S: futures_util::Stream<Item = Result<Message, axum::Error>>
        + futures_util::Sink<Message, Error = axum::Error>
        + Unpin
        + Send,
{
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
    if write_within(WRITE_BOUND, async {
        socket.send(Message::Text(text.into())).await.map_err(anyhow::Error::from)
    })
    .await
    .is_err()
    {
        return;
    }
    if let Err(error) = drive(&mut socket, &state, peer).await {
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
async fn drive<S>(
    socket: &mut S,
    state: &Arc<TransportState>,
    peer: std::net::SocketAddr,
) -> anyhow::Result<()>
where
    S: futures_util::Stream<Item = Result<Message, axum::Error>>
        + futures_util::Sink<Message, Error = axum::Error>
        + Unpin
        + Send,
{
    // This connection's own number: the stamp that names which take is
    // this connection's, on the commands that start and stop one and on
    // the updates that come back. Minted here so the teardown closes the
    // take this connection started and no other.
    let me = mint_connection_id();
    let caller = Caller { id: me, peer };
    // This connection is a client that can draw the viewer for as long as it
    // lives; the guard hands the count back on the way out.
    let _attached = AttachedClient::new(&state.surface);
    let mut watching = Watching::default();
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
    let mut updates: Option<(mpsc::UnboundedReceiver<SeatUpdate>, bool)> = None;
    // The tab this connection is viewing and the seat's tab rows it follows,
    // each once the client asks.
    let mut browser = BrowserViewing::default();
    // Answers to reply-carrying commands, sent by the tasks that ran them:
    // the read loop hands a reply off rather than awaiting it, so a slow
    // handler holds nothing behind it (#2445).
    let (reply_tx, mut answers) = mpsc::unbounded_channel();

    let outcome = run_connection(
        socket,
        state,
        &mut watching,
        &mut holds,
        &mut updates,
        &mut dictate,
        &mut browser,
        &reply_tx,
        &mut answers,
        caller,
    )
    .await;
    // **A viewer's count comes off however the connection ends.** A socket
    // error and a client closing both leave the profile's viewer set, and a
    // role the gone viewer held is released with it.
    if let Some(host) = state.browser_host.as_ref() {
        host.detach(me).await;
    }
    if let Err(error) = &outcome
        && let Some(stalled) = error.downcast_ref::<WriteStalled>()
    {
        tracing::debug!(
            target: "forge_server::transport",
            event_name = "connection_write_stalled",
            client = me,
            bound_seconds = stalled.bound.as_secs(),
            waited_ms = stalled.waited.as_secs_f64() * 1000.0,
            "a frame write outlived its bound, so the connection is ended: the peer stopped \
             reading",
        );
    }

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

    // Every way out of the loop runs this, a failed read included.
    // **A connection that goes away shows nothing, and the seats it was
    // merely subscribed to were never attached**: only the shown one is let
    // go here, or a subscriber's close would take a seat another connection
    // is still showing out of the set it watches.
    if watching.showing_attached
        && let Some(slot) = watching.showing.as_ref()
    {
        Live::lock(&state.live).detach(slot);
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
async fn run_connection<S>(
    socket: &mut S,
    state: &Arc<TransportState>,
    watching: &mut Watching,
    holds: &mut Holds<'_>,
    updates: &mut Option<(mpsc::UnboundedReceiver<SeatUpdate>, bool)>,
    dictate: &mut Streaming,
    browser: &mut BrowserViewing,
    reply_tx: &mpsc::UnboundedSender<ServerMessage>,
    answers: &mut mpsc::UnboundedReceiver<ServerMessage>,
    caller: Caller,
) -> anyhow::Result<()>
where
    S: futures_util::Stream<Item = Result<Message, axum::Error>>
        + futures_util::Sink<Message, Error = axum::Error>
        + Unpin
        + Send,
{
    let me = caller.id;
    let mut held = Batch::default();
    // The served-record bound every frame to a client takes (#2374), read once
    // rather than per frame.
    let cap = state.surface.tool_record_cap_bytes();
    // **The peer proves it is there by answering.** Without this a connection
    // whose peer went away without closing reads nothing for ever, and the
    // viewer it holds sits with it. The first tick is consumed because an
    // interval fires at once.
    let beat_every = state.heartbeat.every;
    let beat_dead = state.heartbeat.dead;
    let mut beat = tokio::time::interval(beat_every);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    beat.tick().await;
    let keepalive = Keepalive::default();
    loop {
        let due = held.due();
        tokio::select! {
            // **A queued answer is read before the beat decides.** A pong can
            // sit unread while this task is busy in a write, and by the time
            // both branches are ready the outstanding ping may already look
            // expired; without the bias the tick could win that race and reap
            // a peer that answered. Reads first, always.
            biased;
            msg = socket.next() => {
                let Some(msg) = msg else { break };   // the client went away
                let msg = msg?;
                keepalive.heard();
                // What the core has already said goes out first: an answer is
                // composed after it, and a snapshot overtaking an update the
                // core emitted before it would land older news on newer.
                batch::flush(socket, held.take()).await?;
                handle_client(
                    socket, state, watching, holds, updates, dictate, browser, reply_tx, caller,
                    msg,
                )
                .await?;
            }
            // **A reply-carrying command's answer, sent by the task that ran
            // it.** The read loop never waits for one, so a slow handler
            // leaves this connection reading; the client tells the answers
            // apart by `reply_to`. The core's news goes out first, as every
            // send here keeps.
            reply = answers.recv() => {
                let Some(reply) = reply else { break };
                batch::flush(socket, held.take()).await?;
                send(socket, reply).await?;
            }
            // Deliberately NOT `Some(update) = updates.recv()`. A pattern that stops matching
            // DISABLES its branch in `select!` rather than ending the loop, so a closed channel
            // would silently stop delivering while the socket stayed open.
            heard = next_update(updates) => {
                let Some(frame) = heard else { break };
                let update = frame.update;
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
                    ..
                } = &update
                    && *id == me
                {
                    dictate.retire(key, outcome, *generation);
                }
                // The fold is the transport's, not this connection's: it runs
                // once for the whole socket in `transport::fold_the_stream`.
                if forwards(&update, &watching.subjects, me) {
                    // **A fatal goes out now, not on the batch's clock.** The
                    // exit it announces is already on its way - `workspace.
                    // shutdown` follows the run loop's return - and a frame
                    // still waiting out its window when the process drops dies
                    // with it. Everything else can wait the interval.
                    let fatal = matches!(update, SessionUpdate::FatalError { .. });
                    held.push(
                        Instant::now(),
                        SeatUpdate { seq: frame.seq, update: for_a_client(update, cap) },
                    );
                    if fatal {
                        batch::flush(socket, held.take()).await?;
                    }
                }
            }
            // **The viewed profile's frames and state, raced as one future**
            // because `select!` wants one borrow of the viewer.
            event = next_view_event(&mut browser.viewing) => {
                // What the core already said goes out first, the same rule
                // every send here keeps.
                batch::flush(socket, held.take()).await?;
                match event {
                    ViewEvent::Frame(frame) => {
                        // **The transport owns how a frame reaches the
                        // person**: the connection hands it over and never
                        // formats one itself.
                        let mut transport = super::frame::SocketFrames { socket: &mut *socket };
                        transport.deliver(&frame).await?;
                    }
                    ViewEvent::State(state) => {
                        let Some(viewing) = browser.viewing.as_ref() else { continue };
                        send(
                            socket,
                            ServerMessage::BrowserView {
                                id: viewing.id,
                                profile: viewing.key.clone(),
                                tab: viewing.tab,
                                controller: state.controller,
                                viewers: state.viewers,
                                viewer: me,
                            },
                        )
                        .await?;
                    }
                    // **The page navigated on its own**: a link click or a
                    // fragment moves the bar's arrows, and nobody asked.
                    ViewEvent::Nav(nav) => {
                        let Some(viewing) = browser.viewing.as_ref() else { continue };
                        send(
                            socket,
                            ServerMessage::BrowserNavState {
                                id: viewing.id,
                                tab: viewing.tab,
                                can_back: nav.can_back,
                                can_forward: nav.can_forward,
                            },
                        )
                        .await?;
                    }
                    // **A page's file chooser goes to the person driving it.**
                    // The shell opens its picker and answers with the bytes;
                    // a watcher that is not the controller is not asked.
                    // **The ask is the profile's, so every viewer's
                    // connection sees it; only the controller answers it.**
                    // A watcher that answered with no files would cancel the
                    // very dialog the controller is showing - the picker
                    // opened for one client and died because another one was
                    // watching. A chooser with no controller at all is the
                    // one case that is cancelled here, once: nothing can
                    // answer it.
                    ViewEvent::File(ask) => {
                        let Some(viewing) = browser.viewing.as_ref() else { continue };
                        // **A chooser belongs to its tab's viewer.** A viewer
                        // watching another tab is never asked to pick for it.
                        if ask.tab != viewing.tab {
                            continue;
                        }
                        let controller = viewing.view.borrow().controller;
                        if controller.is_some_and(|holder| holder != me) {
                            continue;
                        }
                        if controller.is_none() {
                            // A slightly older ask may already have been
                            // cancelled by another viewer's connection; only
                            // the one still outstanding is cancelled.
                            if viewing.files.borrow().is_some()
                                && let Some(host) = state.browser_host.as_ref()
                                && let Err(why) = host
                                    .answer_files(&viewing.key, viewing.tab, ask.id, Vec::new())
                                    .await
                            {
                                tracing::warn!(
                                    "a file chooser could not be cancelled without a controller \
                                     (event_name browser_file_cancel_failed, profile {}, id {}): \
                                     {why}",
                                    viewing.key,
                                    ask.id,
                                );
                            }
                            continue;
                        }
                        send(
                            socket,
                            ServerMessage::BrowserFileDialog {
                                id: ask.id,
                                tab: ask.tab,
                                multiple: ask.multiple,
                            },
                        )
                        .await?;
                    }
                    // **A finished download goes to the person driving.** A
                    // watcher's connection receives the broadcast and drops
                    // it; the save door is the controller's.
                    ViewEvent::Download(download) => {
                        let Some(viewing) = browser.viewing.as_ref() else { continue };
                        // **A save is the tab's too**: a controller watching
                        // another tab is not offered this file.
                        if download.tab != viewing.tab {
                            continue;
                        }
                        if viewing.view.borrow().controller != Some(me) {
                            continue;
                        }
                        let data = download.error.is_none().then(|| {
                            base64::engine::general_purpose::STANDARD.encode(&download.bytes)
                        });
                        send(
                            socket,
                            ServerMessage::BrowserDownload {
                                tab: download.tab,
                                name: download.name,
                                data,
                                error: download.error,
                            },
                        )
                        .await?;
                    }
                    // **The page's own sound goes to the person driving.**
                    // Only the controller hears; a watcher's connection drops
                    // the packet rather than playing another person's page.
                    ViewEvent::Audio(packet) => {
                        let Some(viewing) = browser.viewing.as_ref() else { continue };
                        if viewing.view.borrow().controller != Some(me) {
                            continue;
                        }
                        let mut transport = super::frame::SocketFrames { socket: &mut *socket };
                        transport.deliver_audio(&packet).await?;
                    }
                    // **A browser that stopped is said, not left as a frozen
                    // frame.** The viewer is dropped: nothing more arrives,
                    // and the client re-attaches when it opens the surface
                    // again.
                    ViewEvent::Failed(reason) => {
                        let Some(viewed) = browser.viewing.as_ref() else { continue };
                        let (id, tab) = (viewed.id, viewed.tab);
                        browser.viewing = None;
                        // **The gone viewer comes off the profile too.** The
                        // connection lives on, so without this the profile
                        // kept a phantom viewer and a role held by a surface
                        // that is not looking at it.
                        if let Some(host) = state.browser_host.as_ref() {
                            host.detach(me).await;
                        }
                        send(socket, ServerMessage::BrowserSurfaceFailed { id, tab, reason })
                            .await?;
                    }
                }
            }
            // **The strip's rows, once it asked for them.** A tab that
            // opens, closes or changes hands is pushed as the whole list, so
            // the client never has to poll a read it already subscribed to.
            changed = next_tab_change(&mut browser.tabs) => {
                let Some(list) = changed else { continue };
                batch::flush(socket, held.take()).await?;
                send(
                    socket,
                    ServerMessage::BrowserTabs {
                        id: list.id,
                        profile: list.profile,
                        up: list.up,
                        connected: list.connected,
                        closing_at_ms: list.closing_at_ms,
                        rows: list.rows,
                    },
                )
                .await?;
            }
            // **The keepalive's beat.** A ping that went unanswered past the
            // bound is the evidence, never a quiet read: a busy connection
            // sends no ping at all while it works. A live peer answers the
            // ping itself, so this costs the app nothing.
            _ = beat.tick() => {
                match keepalive.beat(Instant::now(), beat_dead) {
                    Beat::Wait => {}
                    // The deadline is taken inside `ping`, after the send
                    // answers: a send queued behind a full buffer reaches the
                    // peer late, and the peer is owed the whole bound from
                    // when it could first answer.
                    Beat::Ping => keepalive.ping(socket).await?,
                    Beat::Reap => {
                        let waited =
                            keepalive.sent().map(|at| at.elapsed()).unwrap_or_default();
                        tracing::debug!(
                            target: "forge_server::transport",
                            event_name = "connection_reaped",
                            client = me,
                            waited_ms = waited.as_secs_f64() * 1000.0,
                            "the peer never answered the keepalive's ping, so its connection is ended",
                        );
                        break;
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
    updates: &mut Option<(mpsc::UnboundedReceiver<SeatUpdate>, bool)>,
    answering: bool,
) -> Vec<SeatUpdate> {
    if updates.as_ref().is_some_and(|(_, held)| *held >= answering) {
        return Vec::new();
    }
    let queued = updates.take().map_or_else(Vec::new, |(mut receiver, _)| {
        let mut queued = Vec::new();
        while let Ok(frame) = receiver.try_recv() {
            queued.push(frame);
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
    updates: &mut Option<(mpsc::UnboundedReceiver<SeatUpdate>, bool)>,
) -> Option<SeatUpdate> {
    match updates.as_mut() {
        Some((updates, _)) => updates.recv().await,
        None => std::future::pending().await,
    }
}

/// Answer one client message.
async fn handle_client<S>(
    socket: &mut S,
    state: &Arc<TransportState>,
    watching: &mut Watching,
    holds: &mut Holds<'_>,
    updates: &mut Option<(mpsc::UnboundedReceiver<SeatUpdate>, bool)>,
    dictate: &mut Streaming,
    browser: &mut BrowserViewing,
    reply_tx: &mpsc::UnboundedSender<ServerMessage>,
    caller: Caller,
    msg: Message,
) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin + Send,
{
    let viewing = &mut browser.viewing;
    let tabs = &mut browser.tabs;
    let me = caller.id;
    // The served-record bound every frame to a client takes (#2374), read once
    // rather than per frame.
    let cap = state.surface.tool_record_cap_bytes();
    let text = match msg {
        Message::Text(text) => text,
        // **A client's binary frame is audio, or it is not a frame this
        // server takes.** Nothing binary comes from a client for the browser:
        // browser frames are the server's to send.
        Message::Binary(bytes) => {
            take_frame(&state.surface, dictate, me, frame_route(&bytes, dictate));
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
                dispatch_id: None,
            },
        )
        .await;
    };
    match client {
        ClientMessage::Subscribe { what, answering } => {
            // **The open path's own clock (#2374).** From this message being
            // read to the snapshot being written - the span a client's seat
            // switch waits on this half for, whatever subject it names; the
            // line below is emitted for a session, which is where the wait is.
            let open_at = Instant::now();
            // **Which attached client this subscribe is (#2506).** Several
            // clients attach from addresses the log otherwise never names, so
            // a slow open could not be placed on the one that asked for it.
            if let Subject::Session(slot) = &what {
                tracing::debug!(
                    event_name = "client_subscribed",
                    slot = %slot.display(),
                    peer = %caller.peer,
                    "a client subscribed a seat, from the address it is attached on",
                );
            }
            // Forwarded before the snapshot: they were emitted before it was
            // taken, and the client reads them in the order it receives them.
            let queued_at = Instant::now();
            let queued = forward_queued(
                state,
                open_stream(state, updates, answering),
                &watching.subjects,
                caller.id,
            );
            // **A seat is held BEFORE its snapshot is encoded.** The hold
            // starts the seat's watch, whose first pass reads the tree, the
            // processes and the file index and announces each; the encode
            // answers whatever the stores hold, and the updates fill the rest
            // in. The open no longer waits on those reads (#2506); a seat
            // nothing has ever read still takes the record's own fallback, so
            // its cost is the cold open's and nothing more.
            let mut hold_ms = 0.0;
            if let Subject::Session(slot) = &what {
                // **Every session subscribe holds the seat**, a sessionless
                // one included - its watch waits for the session (#1706) - and
                // every taken hold is remembered here so the two paths a
                // subscribe can end without one can give it back: the encode's
                // refusal below, and the unsubscribe.
                let at = Instant::now();
                state.surface.hold_seat(slot);
                holds.take(slot);
                hold_ms = at.elapsed().as_secs_f64() * 1000.0;
            }
            batch::flush(socket, queued).await?;
            // The frames already queued for this subject, forwarded and
            // written before the snapshot is built: a seat that kept
            // streaming while nothing watched pays them on the next open.
            let queued_ms = queued_at.elapsed().as_secs_f64() * 1000.0 - hold_ms;
            // The subscribe's own opening: the client line, the browser
            // role's answer and the seat set a first declaration carries.
            let prelude_ms = queued_at.saturating_duration_since(open_at).as_secs_f64() * 1000.0;
            match encode_subject_timed(state, &what).await {
                Ok((data, timings)) => {
                    // Watched only once the subject is one this server can
                    // answer for: a refused subscribe leaves nothing to hear,
                    // and the client's own re-ask is what recovers it
                    // (`client/src/session/live.ts`). For a seat the server
                    // can answer for, the refusal is the transport copy's
                    // own - the fold catching up is state, not a frame - so
                    // no frame could be waited for; a sessionless seat's
                    // refusal clears when the seat starts, which the re-ask
                    // finds without a subscription to hear it on.
                    //
                    // **A subscription is not a showing.** This connection
                    // hears the seat's frames from here, and the marks are
                    // spent when it says the seat is on screen (`Showing`),
                    // not when it starts listening.
                    let seat = match &what {
                        Subject::Session(slot) => Some(slot.clone()),
                        _ => None,
                    };
                    watching.subjects.push(what.clone());
                    let write = send_frame_timed(
                        socket,
                        ServerMessage::Snapshot { subject: what, data },
                        &SystemClock,
                    )
                    .await?;
                    // **The subscribe that completes a showing pair spends
                    // its marks here**, after the seat's own record is on its
                    // way: the snapshot was encoded before the death trace is
                    // spent, so its row still carries the death, and the home
                    // re-send that follows carries it cleared.
                    if let Some(slot) = &seat
                        && watching.showing.as_ref() == Some(slot)
                        && !watching.showing_attached
                    {
                        spend_showing(socket, state, watching, slot).await?;
                    }
                    if let Some(slot) = &seat {
                        // The snapshot's own write, its own record: the open
                        // below keeps the slices, so the write's numbers are
                        // not repeated there (#2506).
                        tracing::debug!(
                            event_name = "frame_write",
                            slot = %slot.display(),
                            peer = %caller.peer,
                            bytes = write.bytes,
                            prep_ms = write.prep_ms,
                            write_ms = write.write_ms,
                            waited = write.waited,
                            "the snapshot's write: prep is the record serialised into the \
                             bytes the socket takes, write_ms is the socket's share after \
                             that, and waited says the write yielded at least once before it \
                             was done (a prior frame's flush, kernel send-buffer pressure or \
                             a slow reader)",
                        );
                        // The session's open, in its own slices: the numbers a
                        // reader places a slow switch with, beside the
                        // client's own marks and the client it served. Debug,
                        // and for one session's work rather than a problem of
                        // forge's - but a record a reader acts on, so it names
                        // what its remainder is.
                        tracing::debug!(
                            event_name = "session_open",
                            slot = %slot.display(),
                            peer = %caller.peer,
                            open_ms = open_at.elapsed().as_secs_f64() * 1000.0,
                            prelude_ms,
                            queued_ms,
                            hold_ms,
                            prologue_ms = timings.prologue_ms,
                            header_ms = timings.header_ms,
                            catch_up_ms = timings.catch_up_ms,
                            seed_ms = timings.seed_ms,
                            conversation_ms = timings.conversation_ms,
                            page_ms = timings.page_ms,
                            cards_ms = timings.cards_ms,
                            work_ms = timings.work_ms,
                            content_ms = timings.content_ms,
                            reviews_ms = timings.reviews_ms,
                            tree_reviews_ms = timings.tree_reviews_ms,
                            state_ms = timings.state_ms,
                            index_ms = timings.index_ms,
                            resume_ms = timings.resume_ms,
                            tail_ms = timings.tail_ms,
                            encode_ms = timings.encode_ms,
                            pr_age_s = timings.pr_age_s,
                            "the subscribe's own work, in DISJOINT slices: prelude is the \
                             client line and the browser role, queued the frames already \
                             waiting for this subject, hold the seat's watch, prologue the \
                             roster and its asks, header the seat's header, catch_up the \
                             copy's wait, seed the transcript read a cold seat pays, \
                             conversation the copy's arrival, page its cut and fold, cards \
                             the subagent cards, work the tree scan, content the diff's \
                             changed files, reviews and tree_reviews the review reads, state \
                             the seat's own state, index the file walk, resume the `/resume` \
                             list's read, tail the process read \
                             and the \
                             record's assembly, and encode the record's value; pr_age_s is \
                             how old the PR attempt the record carries is, absent until one \
                             has run; open_ms ends when the snapshot's write returns, so \
                             frame_write's prep_ms and write_ms are inside it, and what the \
                             slices and those two do not sum to is the subscribe path's own \
                             glue",
                        );
                    }
                    Ok(())
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
                    // **The refusal names the seat it refused** (#2420): the
                    // error has no subject of its own, and a client holding
                    // several seats has to tell its own refused read from
                    // another's. A `more` refusal carries the same field.
                    let seat = match &what {
                        Subject::Session(slot) => Some(slot.clone()),
                        _ => None,
                    };
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "subscribe".to_owned(),
                            why: refusal.to_string(),
                            seat,
                            dispatch_id: None,
                        },
                    )
                    .await?;
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
                Command::DictateStream { key, options, surface, .. } => {
                    Command::DictateStream { key, options, surface, initiator: Some(me) }
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
            // Eight commands report through the reply and have no update behind
            // them, so omitting `reply_to` on one of those is not a client
            // declining a reply - it is a client declining to learn whether
            // the work happened. Every other command is fire-and-forget: its
            // outcome rides the subscription, so it has no reply to set - and a
            // client that set one anyway is watching a channel nothing will
            // come down, which is refused rather than ignored.
            // Read before any arm moves the command: every refusal below
            // answers the dispatch the press named, so the press can
            // attribute the refusal to itself rather than to whatever was
            // pressed last.
            let dispatch_id = command.dispatch_id();
            match (reply_to, answers_through_a_reply(&command)) {
                (Some(to), true) => {
                    dispatched(me, &command);
                    dispatch_answering(state, command, to, reply_tx);
                    Ok(())
                }
                (None, true) => {
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "reply_to".to_owned(),
                            why: "this command answers through `reply_to` and no update carries its outcome, so that field is required: without it a client cannot tell a refusal from success".to_owned(),
                            seat: None,
                            dispatch_id,
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
                            dispatch_id,
                        },
                    )
                    .await
                }
                (None, false) => {
                    dispatched(me, &command);
                    match state.surface.dispatch(command) {
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
                            send(socket, dispatch_refusal(&refusal, dispatch_id)).await
                        }
                    }
                }
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
                        dispatch_id: None,
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
            // A page is not the open path, so the timings a subscribing read
            // reports are not this one's to gather: the throwaway goes with
            // the call.
            let Some(held) =
                conversation_for(state, &conversation, &mut OpenTimings::default()).await
            else {
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
                        dispatch_id: None,
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
                // **A page is tried from each boundary the window names, in
                // turn** (#2280): a front frame the file never wrote passes
                // every rule and still has no row to resolve, and answering
                // the whole page empty for it reads as the end of the
                // history. A window holding no namable row names nothing,
                // and the read answers with the file's own newest rows,
                // which cannot overlap anything the client holds.
                let anchors: Vec<Option<forge_primitives::TranscriptAnchor>> = match ask {
                    Ask::Below(anchor) => vec![Some(anchor)],
                    Ask::BelowWindow => match boundary_candidates(&held.lock()) {
                        candidates if candidates.is_empty() => vec![None],
                        candidates => candidates.into_iter().map(Some).collect(),
                    },
                    Ask::Held | Ask::Newest => Vec::new(),
                };
                for anchor in anchors {
                    match rows_below_floor(state, &seat, &cwd, anchor, turns).await {
                        BelowFloor::Page(page) => {
                            return send(
                                socket,
                                ServerMessage::Page {
                                    conversation,
                                    turns: page.turns,
                                    cursor: page.cursor,
                                },
                            )
                            .await;
                        }
                        BelowFloor::Refused => {
                            return send(
                                socket,
                                ServerMessage::Error {
                                    what: "more".to_owned(),
                                    why: format!(
                                        "the fold over {} did not finish, so this page cannot be \
                                         answered; asking again may find it",
                                        seat.display()
                                    ),
                                    seat: Some(seat),
                                    dispatch_id: None,
                                },
                            )
                            .await;
                        }
                        BelowFloor::Empty => {
                            tracing::debug!(
                                event_name = "page_boundary_unresolved",
                                slot = %seat.display(),
                                "a boundary row the transcript does not carry; the walk steps to \
                                 the next stop the window names"
                            );
                        }
                    }
                }
                return send(
                    socket,
                    ServerMessage::Page { conversation, turns: Vec::new(), cursor: None },
                )
                .await;
            }
            let opening = before.clone();
            let floor = held.lock().dropped();
            let capped = seat.clone();
            let folded = tokio::task::spawn_blocking(move || {
                let mut page = held.read(|held| {
                    page(
                        held.messages(),
                        held.rendered(),
                        held.dropped(),
                        opening.as_deref(),
                        turns,
                    )
                });
                // The bound runs in the same blocking task as the fold: a page
                // is what a client is served, and the cap is part of building
                // it rather than a pass over the socket's thread (#2374).
                cap_tool_records(&mut page.turns, cap, &capped);
                page
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
                // cursor - the rule the session record shares (`floor_cursor`)
                // - so an uninterrupted walk crosses into the transcript
                // instead of stopping; with no floor above it, the `None` is
                // the honest end of the history.
                Ok(mut page) => {
                    floor_cursor(&mut page, floor);
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
                            dispatch_id: None,
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
        // One stretch of a seat's own stream, for a client that read a gap in
        // the numbers.
        ClientMessage::SeatRange { slot, epoch, from, to } => {
            let Some(cwd) = state.surface.roster().cwd_for(&slot) else {
                return send(
                    socket,
                    ServerMessage::Error {
                        what: "seat_range".to_owned(),
                        why: format!("forge holds no session for {}", slot.display()),
                        seat: Some(slot),
                        dispatch_id: None,
                    },
                )
                .await;
            };
            // **The numbers belong to an occupant.** A seat whose occupant has
            // changed started counting again from one, so a repair across the
            // two would splice one conversation onto another: the client is
            // told to read the seat again rather than handed frames that look
            // like an answer.
            if state.surface.header(&slot).session_id != epoch {
                return send(
                    socket,
                    ServerMessage::Error {
                        what: "seat_range".to_owned(),
                        why: "the seat's occupant has changed since those numbers were minted"
                            .to_owned(),
                        seat: Some(slot),
                        dispatch_id: None,
                    },
                )
                .await;
            }
            let frames = match state.surface.seat_oldest_kept_seq(&slot) {
                // The window still holds the range's front, so it holds all of
                // it: the frames cross as they were emitted.
                Some(oldest) if oldest <= from => state
                    .surface
                    .seat_frames_between(&slot, from, to)
                    .into_iter()
                    .filter_map(|frame| {
                        // **The same stripper every other path to a client goes
                        // through.** The ring holds what the fanout emitted,
                        // and a frame that carries the history an epoch hands
                        // over must not ride a client frame (#2157) - nor lean
                        // on the socket's size bound. The transcript branch
                        // builds only `ChatAppended`, so this is the branch
                        // that can hold one.
                        Some(SeatFrameWire {
                            seq: frame.seq?,
                            update: for_a_client(frame.update, cap),
                        })
                    })
                    .collect(),
                // The window has shed it, so the transcript answers.
                _ => transcript_frames(state, &slot, &cwd, from, to).await,
            };
            send(socket, ServerMessage::SeatFrames { slot, epoch, frames }).await
        }
        ClientMessage::DocumentRead { session, path, reviewable } => {
            // A file off disk takes as long as it takes, so the read runs in
            // a blocking task rather than inline. Nothing parks on the
            // reader's pace: the answer is the read, and the review's finish
            // is its own event.
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
                        dispatch_id: None,
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
        ClientMessage::FileContent { session, path, sha } => {
            // The seat resolution the document read takes, so a file reads
            // out of the tree the record's own content read describes. A
            // read that could not run answers as itself rather than as an
            // error - the refusal's own sentence rides the `failed` arm, and
            // a page draws that arm as the absence of a control rather than
            // as a broken file.
            let content = match state.surface.file_content(&session, &path, sha.as_deref()).await {
                Ok(read) => FileContentWire::Read { lines: read.lines, truncated: read.truncated },
                Err(why) => FileContentWire::Failed { why },
            };
            send(socket, ServerMessage::FileContent { session, path, sha, content }).await
        }
        ClientMessage::BrowserView { seat, tab, reply_to } => {
            // **A re-attach is a switch**: the tab this connection was
            // viewing gives its viewer up before the new one takes it, or
            // the old tab keeps a phantom viewer and its controller role.
            if viewing.is_some()
                && let Some(host) = state.browser_host.as_ref()
            {
                host.detach(me).await;
                *viewing = None;
            }
            // **An attach that cannot happen is answered.** A client that
            // sent a `reply_to` learns the refusal through it and reaches its
            // failed state; one that did not still gets the error frame.
            let Some(host) = state.browser_host.as_ref() else {
                let why = "this server has no browser to view".to_owned();
                return refuse_attach(socket, reply_to, &why, seat).await;
            };
            match host.attach(me, &seat, tab).await {
                Ok(attachment) => {
                    let key = attachment.key.clone();
                    let id = attachment.id;
                    let tab = attachment.tab;
                    // **The attach's own frames carry the state they hold**:
                    // the receivers are read as seen, so the loop announces
                    // only what moves after them rather than repeating the
                    // state the client already drew.
                    let mut fresh = Viewing::new(attachment);
                    let now = *fresh.view.borrow_and_update();
                    let read_nav = host.nav_state(&key, tab).await;
                    // The watch also carries a page-driven move that landed
                    // while the attach was in flight.
                    let watched_nav = *fresh.nav.borrow_and_update();
                    let nav = read_nav.or(watched_nav);
                    // **The kept frame goes to the viewer that attaches.**
                    // The newest frame is a value, not an event: a surface
                    // opening on a still page would otherwise wait for the
                    // next paint. The watch carries the whole profile's newest
                    // paint, so another tab's frame is not this surface's.
                    let kept = fresh.frames.borrow().clone().filter(|frame| frame.tab == tab);
                    if let Some(frame) = kept.as_ref() {
                        fresh.last_frame = Some(frame.seq);
                    }
                    *viewing = Some(fresh);
                    send(
                        socket,
                        ServerMessage::BrowserView {
                            id,
                            profile: key.clone(),
                            tab,
                            controller: now.controller,
                            viewers: now.viewers,
                            viewer: me,
                        },
                    )
                    .await?;
                    if let Some(frame) = kept {
                        let mut transport = super::frame::SocketFrames { socket: &mut *socket };
                        transport.deliver(&frame).await?;
                    }
                    if let Some(nav) = nav {
                        send(
                            socket,
                            ServerMessage::BrowserNavState {
                                id,
                                tab,
                                can_back: nav.can_back,
                                can_forward: nav.can_forward,
                            },
                        )
                        .await?;
                    }
                    match reply_to {
                        Some(ask) => {
                            // **The profile and tab the attach actually
                            // took**, so the page's Done lowers the browser
                            // that came up: a hand-off that named no tab was
                            // resolved to the asking seat's current one.
                            send(
                                socket,
                                ServerMessage::Reply {
                                    reply_to: ask,
                                    body: json!({ "ok": true, "profile": key, "tab": tab }),
                                },
                            )
                            .await
                        }
                        None => Ok(()),
                    }
                }
                Err(why) => refuse_attach(socket, reply_to, &why, seat).await,
            }
        }
        ClientMessage::BrowserDetach => {
            // **Lowering is a detach.** It takes this viewer's count off the
            // profile it was on, so the idle clock can release it and no
            // phantom viewer keeps the profile warm.
            if let Some(host) = state.browser_host.as_ref() {
                host.detach(me).await;
            }
            *viewing = None;
            Ok(())
        }
        ClientMessage::BrowserInput { seq, event } => {
            let (Some(host), Some(viewing)) = (state.browser_host.as_ref(), viewing.as_ref())
            else {
                return Ok(());
            };
            match host.input(&viewing.key, viewing.tab, me, seq, &event).await {
                Ok(()) => Ok(()),
                Err(why) => {
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "browser_input".to_owned(),
                            why,
                            seat: None,
                            dispatch_id: None,
                        },
                    )
                    .await
                }
            }
        }
        ClientMessage::BrowserFileAnswer { id, files } => {
            let (Some(host), Some(viewing)) = (state.browser_host.as_ref(), viewing.as_ref())
            else {
                return Ok(());
            };
            // **Only the controller answers the ask it was shown.** The id
            // must be the profile's outstanding one too: a stale answer -
            // from a superseded dialog, or from a surface that has since
            // been replaced - would otherwise write bytes and cancel a
            // chooser that belongs to someone else.
            let controlling = viewing.view.borrow().controller == Some(me);
            let outstanding = viewing.files.borrow().as_ref().map(|ask| ask.id) == Some(id);
            if !controlling || !outstanding {
                return send(
                    socket,
                    ServerMessage::Error {
                        what: "browser_file_answer".to_owned(),
                        why: "this viewer is not the file chooser's to answer".to_owned(),
                        seat: None,
                        dispatch_id: None,
                    },
                )
                .await;
            }
            let files = files
                .into_iter()
                .map(|file| {
                    let name = file.name;
                    base64::engine::general_purpose::STANDARD
                        .decode(file.data)
                        .map(|bytes| forge_primitives::browser::BrowserUpload { name, bytes })
                })
                .collect::<Result<Vec<_>, _>>();
            let files = match files {
                Ok(files) => files,
                Err(why) => {
                    return send(
                        socket,
                        ServerMessage::Error {
                            what: "browser_file_answer".to_owned(),
                            why: format!("a file answer's bytes are not base64: {why}"),
                            seat: None,
                            dispatch_id: None,
                        },
                    )
                    .await;
                }
            };
            match host.answer_files(&viewing.key, viewing.tab, id, files).await {
                Ok(()) => Ok(()),
                Err(why) => {
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "browser_file_answer".to_owned(),
                            why,
                            seat: None,
                            dispatch_id: None,
                        },
                    )
                    .await
                }
            }
        }
        ClientMessage::BrowserControl { take } => {
            let (Some(host), Some(viewing)) = (state.browser_host.as_ref(), viewing.as_ref())
            else {
                return Ok(());
            };
            let (key, tab) = (viewing.key.clone(), viewing.tab);
            let outcome = if take {
                host.take_control(&key, tab, me).await
            } else {
                host.release_control(&key, tab, me).await;
                Ok(())
            };
            match outcome {
                Ok(()) => Ok(()),
                Err(why) => {
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "browser_control".to_owned(),
                            why,
                            seat: None,
                            dispatch_id: None,
                        },
                    )
                    .await
                }
            }
        }
        ClientMessage::BrowserNav { back } => {
            let (Some(host), Some(viewing)) = (state.browser_host.as_ref(), viewing.as_ref())
            else {
                return Ok(());
            };
            let (key, id, tab) = (viewing.key.clone(), viewing.id, viewing.tab);
            match host.nav(&key, tab, me, back).await {
                Ok(nav) => {
                    send(
                        socket,
                        ServerMessage::BrowserNavState {
                            id,
                            tab,
                            can_back: nav.can_back,
                            can_forward: nav.can_forward,
                        },
                    )
                    .await
                }
                Err(why) => {
                    send(
                        socket,
                        ServerMessage::Error {
                            what: "browser_nav".to_owned(),
                            why,
                            seat: None,
                            dispatch_id: None,
                        },
                    )
                    .await
                }
            }
        }
        ClientMessage::BrowserTabs { seat, reply_to } => {
            // **The ask is the subscription**: the answer names the rows as
            // they stand now, and the connection follows the same watch from
            // here on, so the strip costs one read rather than a poll.
            let Some(host) = state.browser_host.as_ref() else {
                let why = "this server has no browser to list".to_owned();
                return answer_ask(socket, reply_to, "browser_tabs", Err(why)).await;
            };
            match host.tabs(&seat).await {
                Ok(list) => {
                    // The stream is subscribed before the answer goes out, so
                    // a change landing between the two is pushed rather than
                    // lost.
                    match host.tab_stream(&seat).await {
                        Ok(stream) => *tabs = Some(stream),
                        Err(why) => {
                            tracing::debug!(
                                event_name = "browser_tab_stream_failed",
                                seat = %seat.display(),
                                %why,
                                "a tab list was read but its stream could not be followed",
                            );
                        }
                    }
                    answer_ask(socket, reply_to, "browser_tabs", Ok(json!({ "tabs": list }))).await
                }
                Err(why) => answer_ask(socket, reply_to, "browser_tabs", Err(why)).await,
            }
        }
        ClientMessage::BrowserResize { width, height, scale } => {
            // **The role owns the page's layout, or the tab's longest watcher
            // while nobody holds it**, and the service is what knows whose it
            // is: another viewer's window is dropped rather than moving the
            // page under the one driving it.
            let (Some(host), Some(viewing)) = (state.browser_host.as_ref(), viewing.as_ref())
            else {
                return Ok(());
            };
            let (key, tab) = (viewing.key.clone(), viewing.tab);
            host.resize(&key, tab, me, width, height, scale).await;
            Ok(())
        }
        ClientMessage::ClientPerf { records } => {
            // **A page's own instrument, not forge's health**: a batch that
            // could not be written is one client's lost diagnostic, so it is
            // read at `debug` and names where it went. The write runs on the
            // blocking pool - a file append has no business on a socket's
            // async task.
            let count = records.len();
            match tokio::task::spawn_blocking(move || crate::perf_log::append(&records)).await {
                Ok(Ok(())) => {}
                Ok(Err(why)) => tracing::debug!(
                    event_name = "client_perf_write_failed",
                    records = count,
                    %why,
                    "a client's perf batch could not be written",
                ),
                Err(why) => tracing::debug!(
                    event_name = "client_perf_write_failed",
                    records = count,
                    %why,
                    "a client's perf batch could not be written",
                ),
            }
            Ok(())
        }
        ClientMessage::Unsubscribe { what } => {
            // No answer: the client asked to stop hearing, and there is
            // nothing to say back.
            //
            // ONE entry, because `subscribe` added one. `retain` would drop
            // every copy, so a client that subscribed twice and unsubscribed
            // once would leave a watch entry the client no longer counts,
            // and the seat's frames would keep arriving for nothing.
            //
            // **The showing is not the subscription's to end**: a client that
            // drops a seat still showing it sends `Showing { seat: null }` or
            // closes, and that is what detaches it.
            if let Some(at) = watching.subjects.iter().position(|held| held == &what) {
                watching.subjects.remove(at);
                if let Subject::Session(slot) = &what {
                    // Only a hold THIS connection took is given back here,
                    // for the same reason the clean-up gives back only those:
                    // a count decremented twice is another viewer's seat.
                    holds.give_back(slot);
                }
            }
            Ok(())
        }
        ClientMessage::Showing { seat } => set_showing(socket, state, watching, seat).await,
    }
}

/// Move this connection's shown seat, spending exactly what that showing earns.
///
/// **Attach is what clears the diamond and the failure mark, so it follows the
/// screen rather than the subscription**: a client keeps every visited seat
/// subscribed, and spending on that would clear marks nobody looked at. The
/// answer is whether anything was spent, which is what the home it holds is
/// owed a re-encode for.
async fn set_showing<S>(
    socket: &mut S,
    state: &Arc<TransportState>,
    watching: &mut Watching,
    seat: Option<SessionSlot>,
) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    if watching.showing == seat {
        return Ok(());
    }
    if watching.showing_attached {
        if let Some(previous) = watching.showing.as_ref() {
            Live::lock(&state.live).detach(previous);
        }
        watching.showing_attached = false;
    }
    let Some(next) = seat else {
        watching.showing = None;
        return Ok(());
    };
    // **A showing spends nothing until the seat is actually watched.** A
    // frame naming a slot this connection never subscribed would otherwise
    // clear a diamond, a failure mark and a durable death trace with no read
    // of that seat served - and the pairing half may arrive first, so the
    // showing is remembered and the subscribe that completes it spends.
    let watched = watching
        .subjects
        .iter()
        .any(|held| matches!(held, Subject::Session(slot) if slot == &next));
    watching.showing = Some(next.clone());
    if watched {
        spend_showing(socket, state, watching, &next).await?;
    }
    Ok(())
}

/// Spend what showing `slot` earns: its diamond, its failure mark and its
/// death trace, and the home refresh a spent mark owes.
///
/// **Called once per showing, from whichever half completes the pair.**
async fn spend_showing<S>(
    socket: &mut S,
    state: &Arc<TransportState>,
    watching: &mut Watching,
    slot: &SessionSlot,
) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    let failed = state.surface.failed_turn(slot);
    // The seat's own snapshot was encoded before this, so the failure a death
    // left on its row is drawn by the read that spends it.
    let death = state.surface.seat_shown(slot);
    let spent = Live::lock(&state.live).attach(slot, failed) || death;
    watching.showing_attached = true;
    if spent && watching.subjects.iter().any(|held| matches!(held, Subject::Home)) {
        match encode_subject(state, &Subject::Home).await {
            Ok(home) => {
                send(socket, ServerMessage::Snapshot { subject: Subject::Home, data: home })
                    .await?;
            }
            Err(error) => tracing::debug!(
                event_name = "home_refresh_failed",
                slot = %slot.display(),
                %error,
                "the home refresh after a seat was shown could not be encoded",
            ),
        }
    }
    Ok(())
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

/// The frames a range ask is answered with once the seat's own window has shed
/// them: the conversation's own rows, numbered to END at the range's end.
///
/// **What a frame was numbered is minted in memory and written down nowhere.**
/// So a range that reaches past the window takes the transcript's newest rows -
/// the stretch the client is reading, read back off the file - and numbers them
/// so the last one is the ask's own end: the client resumes exactly where it
/// was reading rather than asking again for a stretch that can never arrive,
/// and the numbers it holds stay the numbers of one sequence rather than
/// restarting at one.
async fn transcript_frames(
    state: &TransportState,
    slot: &SessionSlot,
    cwd: &Path,
    from: u64,
    to: u64,
) -> Vec<SeatFrameWire> {
    if to < from {
        return Vec::new();
    }
    let want = (to - from).saturating_add(1);
    let surface = Arc::clone(&state.surface);
    let seat = slot.clone();
    let cwd = cwd.to_owned();
    // A whole-file read, because the call is the cold path: the seat's window
    // is what answers a repair until it has shed the frames, and this is what
    // is left when it has.
    let Ok(read) = tokio::task::spawn_blocking(move || surface.conversation(&seat, &cwd)).await
    else {
        return Vec::new();
    };
    let rows = &read.messages;
    let kept = u64::try_from(rows.len()).unwrap_or(u64::MAX).min(want);
    let first = to.saturating_add(1).saturating_sub(kept);
    // A repaired frame is served like every other one (#2374): the transcript
    // keeps the record whole, and the client gets the same bound a page gets.
    let cap = state.surface.tool_record_cap_bytes();
    rows.iter()
        .skip(rows.len().saturating_sub(usize::try_from(kept).unwrap_or(usize::MAX)))
        .enumerate()
        .map(|(at, msg)| {
            let mut msg = msg.clone();
            cap_message_record(&mut msg, cap, slot);
            SeatFrameWire {
                seq: first.saturating_add(u64::try_from(at).unwrap_or(u64::MAX)),
                update: SessionUpdate::ChatAppended {
                    key: slot.clone(),
                    origin: None,
                    source: None,
                    msg,
                },
            }
        })
        .collect()
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
/// **The bound has to be the frame's, not the window's.** A window is bounded
/// by the ceilings a seat keeps it in (frames and bytes, the module beside
/// the task), and those are a seat's own business: what a live 2.5GiB seat's
/// resume did to a connection (#2150) was ride one `Connected` as a single
/// 2.46GiB frame, refused at the 256MiB one the client's socket carries
/// (`MAX_FRAME_BYTES`, client/src-tauri/socket.rs). What a client reads
/// instead is a page, and a page is bounded by turns on top of whatever the
/// window holds (twenty a subscribe, twenty a `more`), so its size leans on
/// that frame cap rather than sitting orders under it. A frame whose size
/// does not depend on the conversation's size is the only one that holds
/// whatever the seat weighs.
fn for_a_client(mut update: SessionUpdate, cap: usize) -> SessionUpdate {
    match &mut update {
        SessionUpdate::Connected { history, .. }
        | SessionUpdate::SessionReplaced { history, .. }
        | SessionUpdate::HistoryReplayed { history, .. } => *history = Vec::new(),
        // **A live frame draws what a reload draws** (#2374): a record is
        // bounded on its way to a client wherever it comes from, or an
        // over-cap edit draws its hunks live and the marker after a switch.
        SessionUpdate::ChatAppended { msg, key, .. } => cap_message_record(msg, cap, key),
        _ => {}
    }
    update
}

/// The frames a subscribe read out of the stream it replaced: filtered for
/// this connection, and bounded like every other path to a client (#2374).
fn forward_queued(
    state: &TransportState,
    frames: Vec<SeatUpdate>,
    watched: &[Subject],
    me: u64,
) -> Vec<SeatUpdate> {
    let cap = state.surface.tool_record_cap_bytes();
    frames
        .into_iter()
        .filter(|frame| forwards(&frame.update, watched, me))
        .map(|frame| SeatUpdate { seq: frame.seq, update: for_a_client(frame.update, cap) })
        .collect()
}

/// Where one binary message went, decided without touching a take.
#[derive(Debug, PartialEq)]
enum FrameRoute {
    /// The samples of a dictation frame, for the seats this connection
    /// started.
    Audio(Vec<f32>),
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

async fn send<S>(socket: &mut S, message: ServerMessage) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    send_frame(socket, message).await.map(|_| ())
}

/// Answer one strip ask: through the reply it carried, or as the error frame
/// when it carried none.
async fn answer_ask<S>(
    socket: &mut S,
    reply_to: Option<u64>,
    what: &str,
    outcome: Result<serde_json::Value, String>,
) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    match (reply_to, outcome) {
        (Some(id), Ok(mut body)) => {
            if let Some(map) = body.as_object_mut() {
                map.insert("ok".to_owned(), json!(true));
            }
            send(socket, ServerMessage::Reply { reply_to: id, body }).await
        }
        (Some(id), Err(why)) => {
            send(
                socket,
                ServerMessage::Reply { reply_to: id, body: json!({ "ok": false, "why": why }) },
            )
            .await
        }
        (None, Ok(_)) => Ok(()),
        (None, Err(why)) => {
            send(
                socket,
                ServerMessage::Error { what: what.to_owned(), why, seat: None, dispatch_id: None },
            )
            .await
        }
    }
}

/// Answer a refused attach: through the client's reply when it sent one, and
/// as the protocol's error frame otherwise.
async fn refuse_attach<S>(
    socket: &mut S,
    reply_to: Option<u64>,
    why: &str,
    seat: SessionSlot,
) -> anyhow::Result<()>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    match reply_to {
        Some(reply_to) => {
            send(
                socket,
                ServerMessage::Reply { reply_to, body: json!({ "ok": false, "why": why }) },
            )
            .await
        }
        None => {
            send(
                socket,
                ServerMessage::Error {
                    what: "browser_view".to_owned(),
                    why: why.to_owned(),
                    seat: Some(seat),
                    dispatch_id: None,
                },
            )
            .await
        }
    }
}

/// [`send`], answering how many bytes the frame carried.
///
/// **Only the open path reports a size** (#2374), and a return every other
/// caller would drop is noise in thirty of them - so the size is answered
/// here and the plain write wraps it.
async fn send_frame<S>(socket: &mut S, message: ServerMessage) -> anyhow::Result<usize>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
{
    let text = serde_json::to_string(&message)?;
    let bytes = text.len();
    write_within(WRITE_BOUND, async {
        socket.send(Message::Text(text.into())).await.map_err(anyhow::Error::from)
    })
    .await?;
    Ok(bytes)
}

/// One frame's write, measured: its size, the time the socket took it, and
/// whether the write yielded at least once before it was done.
struct FrameWrite {
    bytes: usize,
    /// The message's serialisation into the bytes the socket takes, on the
    /// process's own clock.
    prep_ms: f64,
    write_ms: f64,
    waited: bool,
}

/// The clock a measured write reads.
///
/// **The start is handed the frame it measures**: the shipping clock ignores
/// it, and the test's fake records its length - which is what proves the read
/// happens after the message is serialised (#2506).
trait WriteClock {
    fn start(&self, frame: &str) -> Instant;

    fn end(&self) -> Instant;
}

/// The shipping clock: the process's own monotonic one.
struct SystemClock;

impl WriteClock for SystemClock {
    fn start(&self, _frame: &str) -> Instant {
        Instant::now()
    }

    fn end(&self) -> Instant {
        Instant::now()
    }
}

/// [`send_frame`], timing the socket's share of the write.
///
/// **The open's own line cannot place its remainder without it** (#2506):
/// `open_ms` ends when this returns, so the frame's transfer is inside it,
/// and a write that yielded is otherwise indistinguishable from the fold, the
/// record's reads and the serialisation before it.
async fn send_frame_timed<S, C>(
    socket: &mut S,
    message: ServerMessage,
    clock: &C,
) -> anyhow::Result<FrameWrite>
where
    S: futures_util::Sink<Message, Error = axum::Error> + Unpin,
    C: WriteClock,
{
    let prep_at = Instant::now();
    let text = serde_json::to_string(&message)?;
    let prep_ms = prep_at.elapsed().as_secs_f64() * 1000.0;
    let bytes = text.len();
    let waited = AtomicBool::new(false);
    let at = clock.start(&text);
    let mut write = std::pin::pin!(async {
        socket.send(Message::Text(text.into())).await.map_err(anyhow::Error::from)
    });
    write_within(WRITE_BOUND, async {
        std::future::poll_fn(|cx| {
            let poll = std::future::Future::poll(write.as_mut(), cx);
            if poll.is_pending() {
                waited.store(true, Ordering::Relaxed);
            }
            poll
        })
        .await
    })
    .await?;
    Ok(FrameWrite {
        bytes,
        prep_ms,
        write_ms: clock.end().saturating_duration_since(at).as_secs_f64() * 1000.0,
        waited: waited.load(Ordering::Relaxed),
    })
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
/// numberings up. It is answered from the window's oldest row the file
/// resolves instead: overlap at worst, never a gap, and a walk from any
/// cursor converges on the same descending chain.
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

/// How many of a window's rows a below-floor page tries to start from.
///
/// The class that bites is a mid-turn queued prompt: the wire streams its
/// user frame with a uuid, and the file writes only an attachment naming that
/// uuid as its source, so the frame has no row and the next written row sits
/// right behind it. A few names ahead is what skips past one, and each name
/// the file refuses costs it one bounded search, so the count stays small.
const BOUNDARY_TRIES: usize = 8;

/// The rows a walk below the window may start from, the walk's own stop
/// first and the rest of the window behind it.
///
/// **Every candidate passes every rule that can be checked without the
/// file** - `has_a_row` for the forged, the dispatch check for sub-agents -
/// **and the file has the last word**: frames the CLI streams
/// and never writes pass every rule and still have no row, so a walk tries
/// these in turn rather than trusting the first (#2280). Answer one empty
/// and the client reads `cursor: null` as the end of the history.
///
/// **A stop never lands on a result without its call.** The page below a stop
/// serves rows strictly older than it, so the row just beneath the stop is
/// that page's newest: a result there whose call sits beneath it would reach
/// the fold without its call, drawn with its output nowhere - so the walk
/// steps past result rows to the first row that is not one. A window holding
/// nothing but results still names the last of them rather than nothing:
/// a stop on a result serves it whole, and an unnamed window answers as the
/// end of the history.
fn boundary_candidates(
    held: &crate::transport::conversation::Conversation,
) -> Vec<forge_primitives::TranscriptAnchor> {
    let rows: Vec<&forge_primitives::Message> = held
        .messages()
        .iter()
        .filter(|message| held.has_a_row(message) && row_id(message).is_some())
        .collect();
    let stops: Vec<&forge_primitives::Message> =
        match rows.iter().position(|m| !carries_a_result(m)) {
            Some(at) => rows[at..].iter().copied().filter(|m| !carries_a_result(m)).collect(),
            None => rows.last().copied().into_iter().collect(),
        };
    stops
        .into_iter()
        .filter_map(|message| {
            row_id(message).map(|row| forge_primitives::TranscriptAnchor { row, offset: None })
        })
        .take(BOUNDARY_TRIES)
        .collect()
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
            "the transcript has no rows for this anchor; the walk decides what the empty read \
             means",
        );
        return BelowFloor::Empty;
    };
    // The span is read from the file, but the SEAT may be mid-turn right
    // now - and a mid-call delivery inside this span has to be foldable the
    // same way the held window folds it (#2379), so the seat's own state
    // travels with the read.
    let live = state.conversations.live_of(seat);
    let cap = state.surface.tool_record_cap_bytes();
    let capped = seat.clone();
    let folded = tokio::task::spawn_blocking(move || {
        let mut page = page_of_span(&span, turns, live);
        cap_tool_records(&mut page.turns, cap, &capped);
        page
    })
    .await;
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
    live: bool,
) -> crate::transport::wire::Page {
    let rendered = crate::transcript::render(&span.messages, live);
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

/// Dispatch one reply-carrying command and hand its answer to the connection.
///
/// **The caller does not wait for the answer.** The dispatch stays here, where
/// its order against the commands around it is unchanged; the wait and the
/// reply move to a task of their own and come back through `reply_tx`. A
/// despawn's reply arrives only when its detached cleanup finishes (seconds to
/// minutes), and a read loop awaiting it reads nothing behind it (#2445).
fn dispatch_answering(
    state: &TransportState,
    command: Command,
    to: u64,
    reply_tx: &mpsc::UnboundedSender<ServerMessage>,
) {
    // A view acts through the same facade it reads through, and a refusal is
    // the core's own sentence rather than this layer's opinion of it.
    //
    // The five reply-carrying commands take a sender the wire deliberately
    // does not carry, and each carries its OWN reply type - a spawned worker
    // answers with a `WorkerSpawnReply`, a despawn with a `DespawnResult`,
    // and the review writes with a `bool` and a `ReviewSet`. So the channel
    // is built per arm and handed to the shared helper with its own receiver:
    // one channel cannot carry five different answers. **A variant missing
    // here hangs its caller**:
    // it falls to the fire-and-forget arm below with no sender, the
    // workspace answers nobody, and the client waits for ever (measured live
    // on the delete, 2026-10-17).
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
            cwd,
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
                cwd,
                from_boot_respawn,
                return_to: Some(tx),
            };
            answered(state, command, to, rx, reply_tx);
        }
        Command::DespawnWorker { project_key, label, force, .. } => {
            let (tx, rx) = oneshot::channel();
            let command = Command::DespawnWorker { project_key, label, force, respond: Some(tx) };
            answered(state, command, to, rx, reply_tx);
        }
        Command::UpsertReviewThread { project, branch, thread, .. } => {
            let (tx, rx) = oneshot::channel();
            let command =
                Command::UpsertReviewThread { project, branch, thread, respond: Some(tx) };
            answered(state, command, to, rx, reply_tx);
        }
        Command::DeleteReviewThread { project, branch, thread_id, .. } => {
            let (tx, rx) = oneshot::channel();
            let command =
                Command::DeleteReviewThread { project, branch, thread_id, respond: Some(tx) };
            answered(state, command, to, rx, reply_tx);
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
            answered(state, command, to, rx, reply_tx);
        }
        // Every other command is fire-and-forget: success sends nothing
        // extra, because the update arrives through the subscription the
        // client already has.
        other => {
            // A sixth reply-carrying variant added without an arm lands
            // here: a refusal is answered below, a success is not, so its
            // caller hangs (a despawn-shaped one has no client bound at all).
            // Caught in debug rather than allowed to pass.
            debug_assert!(
                !answers_through_a_reply(&other),
                "a reply-carrying command reached the fire-and-forget arm",
            );
            if let Err(refusal) = state.surface.dispatch(other) {
                let body = serde_json::Value::String(refusal.to_string());
                let _ = reply_tx.send(ServerMessage::Reply { reply_to: to, body });
            }
        }
    }
}

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
                dispatch_id: None,
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
                dispatch_id: None,
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

/// One refused dispatch, as the frame the asking client reads: the operation
/// the refusal names, the core's own words, and the dispatch id the press
/// that made it minted - echoed so a press can attribute this refusal to
/// itself rather than to whatever was pressed last.
fn dispatch_refusal(refusal: &DispatchError, dispatch_id: Option<u64>) -> ServerMessage {
    ServerMessage::Error {
        what: refusal_tag(refusal).to_owned(),
        why: refusal.to_string(),
        seat: None,
        dispatch_id,
    }
}

/// **Which hand sent what** (issue #2372: a parked hand-off's answer arrived
/// over the socket and could not be attributed - no dispatch named its
/// connection). Logged where a command is actually dispatched: a refusal
/// never was.
fn dispatched(me: u64, command: &Command) {
    tracing::debug!(
        target: "forge_server::transport",
        event_name = "command_dispatched",
        client = me,
        command = ?command,
        "a client command reached the dispatch",
    );
}

/// Dispatch a reply-carrying command, and answer the client that asked.
///
/// A client that asked for an answer always hears one - INCLUDING a refusal,
/// so it is never left watching a channel that stays empty.
/// Whether a command reports its outcome through the reply rather than
/// through an update on the subscription.
///
/// These five are the ones with nothing behind them on the wire: a client
/// that omits `reply_to` for one of them has no second way to learn what
/// happened.
fn answers_through_a_reply(command: &Command) -> bool {
    matches!(
        command,
        Command::SpawnWorker { .. }
            | Command::DespawnWorker { .. }
            | Command::UpsertReviewThread { .. }
            | Command::DeleteReviewThread { .. }
            | Command::SubmitReview { .. }
    )
}

/// Dispatch one of those five, and answer the client that asked.
///
/// A client that asked for an answer always hears one - INCLUDING a refusal,
/// so it is never left watching a channel that stays empty. The wait and the
/// reply are a task's own, so nothing behind the command waits on them.
fn answered<T: serde::Serialize + Send + 'static>(
    state: &TransportState,
    command: Command,
    to: u64,
    answer: oneshot::Receiver<T>,
    reply_tx: &mpsc::UnboundedSender<ServerMessage>,
) {
    let outcome = state.surface.dispatch(command);
    let reply_tx = reply_tx.clone();
    tokio::spawn(async move {
        let body = match (outcome, answer.await) {
            (Ok(()), Ok(reply)) => serde_json::to_value(reply).unwrap_or_else(|_| {
                serde_json::Value::String("the answer could not be encoded".to_owned())
            }),
            (Err(refusal), _) => serde_json::Value::String(refusal.to_string()),
            (Ok(()), Err(_)) => {
                serde_json::Value::String("the session closed before answering".to_owned())
            }
        };
        // A connection that has gone has nothing to send to; the command's
        // outcome is the core's, and the socket's death is its own.
        let _ = reply_tx.send(ServerMessage::Reply { reply_to: to, body });
    });
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::transport::Heartbeat;
    use crate::work::WorkCache;

    /// **Only a ping that went out can go unanswered.** The state holds no
    /// read clock at all, so a quiet read - a connection busy in one long
    /// write - cannot be mistaken for silence: a beat with no outstanding
    /// ping asks again, however long it has been.
    #[tokio::test]
    async fn a_ping_is_the_only_thing_that_can_go_unanswered() {
        let dead = Duration::from_secs(60);
        let keepalive = Keepalive::default();
        let start = Instant::now();

        assert_eq!(keepalive.beat(start, dead), Beat::Ping, "no ping out: ask");
        assert_eq!(
            keepalive.beat(start + Duration::from_secs(65), dead),
            Beat::Ping,
            "nothing was sent, so nothing went unanswered and the beat asks",
        );

        // A ping ON THE WIRE is what starts the clock.
        let mut sink = NullSink;
        keepalive.ping(&mut sink).await.expect("the ping goes");
        let at = keepalive.sent().expect("the send stamped the deadline");
        assert_eq!(
            keepalive.beat(at + Duration::from_secs(30), dead),
            Beat::Wait,
            "inside the bound the answer is still owed",
        );
        assert_eq!(
            keepalive.beat(at + Duration::from_secs(65), dead),
            Beat::Reap,
            "past it the peer has not answered, and that is the evidence",
        );
    }

    /// **Asking does not stamp the deadline.** A send can block in flush
    /// behind a full buffer; a deadline taken when the beat decided would
    /// expire before the peer ever saw the ping, and the next tick would cut
    /// a peer that was never given time to answer.
    #[test]
    fn asking_does_not_stamp_the_deadline() {
        let dead = Duration::from_secs(60);
        let keepalive = Keepalive::default();
        let start = Instant::now();

        assert_eq!(keepalive.beat(start, dead), Beat::Ping);
        assert_eq!(keepalive.sent(), None, "the deadline starts when the ping is on the wire");
    }

    /// The address a test's own socket is attached from; no listener is
    /// bound, so nothing dials it.
    fn attached_from() -> std::net::SocketAddr {
        "127.0.0.1:0".parse().expect("a test address")
    }

    /// A sink that accepts every frame at once, for the tests that only need
    /// a send to complete.
    struct NullSink;

    impl futures_util::Sink<Message> for NullSink {
        type Error = axum::Error;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(self: std::pin::Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A sink whose flush yields once before it finishes: the write waited,
    /// and for no measurable time.
    struct YieldsOnce {
        yielded: bool,
    }

    impl futures_util::Sink<Message> for YieldsOnce {
        type Error = axum::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(self: std::pin::Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            if !self.yielded {
                self.yielded = true;
                cx.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// The clock a measured write reads, recording the frame each start was
    /// handed and answering a fixed second. **Deterministic**: the test never
    /// waits, and a start read taken before the frame exists records nothing.
    struct FakeClock {
        starts: std::cell::RefCell<Vec<usize>>,
        base: Instant,
    }

    impl WriteClock for FakeClock {
        fn start(&self, frame: &str) -> Instant {
            self.starts.borrow_mut().push(frame.len());
            self.base
        }

        fn end(&self) -> Instant {
            self.base + Duration::from_secs(1)
        }
    }

    /// A sink whose write is held until the test releases it.
    struct HeldSink {
        ready: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl futures_util::Sink<Message> for HeldSink {
        type Error = axum::Error;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            if self.ready.load(std::sync::atomic::Ordering::Relaxed) {
                std::task::Poll::Ready(Ok(()))
            } else {
                std::task::Poll::Pending
            }
        }
        fn start_send(self: std::pin::Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// A peer that stopped reading: its stream hands over whatever the test
    /// sends and then parks, and its writes take frames into a buffer whose
    /// flush never completes - the socket a peer that vanished mid-frame
    /// leaves behind.
    struct StoppedReading {
        inbound: mpsc::UnboundedReceiver<Message>,
        /// Signalled the first time a flush hangs, which is the moment the
        /// connection is stuck in a write with the role already held.
        writing: Option<oneshot::Sender<()>>,
    }

    impl futures_util::Stream for StoppedReading {
        type Item = Result<Message, axum::Error>;

        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.inbound.poll_recv(cx).map(|said| said.map(Ok))
        }
    }

    impl futures_util::Sink<Message> for StoppedReading {
        type Error = axum::Error;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(self: std::pin::Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }
        fn poll_flush(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            if let Some(writing) = self.writing.take() {
                let _ = writing.send(());
            }
            std::task::Poll::Pending
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// **The stamp is taken after the send returns**, which is the caller's
    /// ordering and not the state machine's: a ping held in a full buffer
    /// must owe nothing until it is on the wire. The held sink is the stall,
    /// and the state is read at each side of the await.
    #[tokio::test]
    async fn the_deadline_starts_when_the_ping_goes_out() {
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut sink = HeldSink { ready: std::sync::Arc::clone(&ready) };
        let keepalive = Keepalive::default();
        let mut sending = Box::pin(keepalive.ping(&mut sink));

        assert!(
            futures_util::poll!(sending.as_mut()).is_pending(),
            "the write is held in the buffer",
        );
        assert_eq!(
            keepalive.sent(),
            None,
            "nothing may be owed while the ping is still to be written",
        );

        ready.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(futures_util::poll!(sending.as_mut()).is_ready(), "the write completed");
        assert!(keepalive.sent().is_some(), "and the answer is owed from the send's return");
    }

    /// Any frame is the answer, and the next beat asks again rather than
    /// reaping.
    #[tokio::test]
    async fn an_answer_clears_the_outstanding_ping() {
        let dead = Duration::from_secs(60);
        let keepalive = Keepalive::default();
        let mut sink = NullSink;
        keepalive.ping(&mut sink).await.expect("the ping goes");
        let at = keepalive.sent().expect("the send stamped the deadline");

        keepalive.heard();

        assert_eq!(
            keepalive.beat(at + Duration::from_secs(65), dead),
            Beat::Ping,
            "the answer retired the ping, so the next beat asks again",
        );
    }

    /// **A peer that stops reading is cut at the write bound, its role passes
    /// to the waiter, and the release names the write.**
    ///
    /// The mirror of the keepalive's case (#2473): a peer that vanishes with a
    /// frame IN FLIGHT leaves no ping outstanding, so no beat can see it - the
    /// write is the only thing that can. **The connection ends rather than the
    /// write being retried**: half a frame cannot be resumed, so keeping the
    /// socket would deliver garbage to a client still parsing the frame it
    /// began.
    #[tokio::test(start_paused = true)]
    async fn a_peer_that_stops_reading_is_cut_at_the_write_bound() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let state = Arc::new(TransportState::for_workspace(
            &fleet.workspace(),
            forge_primitives::ClientConfig::default(),
        ));

        let (inbound, peer_in) = mpsc::unbounded_channel();
        let (writing, stuck) = oneshot::channel();
        let mut peer = StoppedReading { inbound: peer_in, writing: Some(writing) };
        let driven = tokio::spawn({
            let state = Arc::clone(&state);
            async move { drive(&mut peer, &state, attached_from()).await }
        });

        // The snapshot answering the subscribe is the frame whose write
        // stops.
        inbound
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Subscribe {
                    what: Subject::Usage,
                    answering: false,
                })
                .expect("the subscribe encodes")
                .into(),
            ))
            .expect("the peer's one message goes");
        stuck.await.expect("the subscribe's answer reaches the socket");

        // **The bound, reached on the test's own clock**: a drive that never
        // ends fails by name here rather than hanging the run.
        let outcome = tokio::time::timeout(WRITE_BOUND * 2, driven)
            .await
            .expect("the connection ends at the write bound rather than never")
            .expect("the connection task joins");
        let error = outcome.expect_err("a peer that stopped reading must not end cleanly");
        let stalled = error.downcast_ref::<WriteStalled>().unwrap_or_else(|| {
            panic!(
                "the connection ends on the write bound rather than on a socket error: {error:?}"
            )
        });
        assert!(
            stalled.waited >= WRITE_BOUND,
            "the bound's own record carries how long the write ran: {:?}",
            stalled.waited,
        );
    }

    /// **The ping's own write is bounded.** It is the site #2473 survives on:
    /// a peer that stops reading while the next ping waits behind a full
    /// buffer leaves no other write in the loop to bound, so an unbounded ping
    /// holds the role for ever.
    #[tokio::test(start_paused = true)]
    async fn a_ping_that_cannot_be_written_ends_at_the_write_bound() {
        let ready = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut sink = HeldSink { ready: std::sync::Arc::clone(&ready) };
        let keepalive = Keepalive::default();

        let outcome = tokio::time::timeout(WRITE_BOUND * 2, keepalive.ping(&mut sink))
            .await
            .expect("the ping ends at the write bound rather than never");
        let error = outcome.expect_err("a ping the buffer cannot take is not sent");

        assert!(
            error.downcast_ref::<WriteStalled>().is_some(),
            "the ping ends on the write bound rather than on a sink error: {error:?}",
        );
        assert_eq!(
            keepalive.sent(),
            None,
            "and no deadline is stamped for a ping the peer never saw",
        );
    }

    /// **The greeting's write is bounded too.** It is the first frame every
    /// connection sends, and a peer that never reads it would otherwise hold
    /// the connection's task until TCP gives up.
    #[tokio::test(start_paused = true)]
    async fn a_greeting_that_cannot_be_written_ends_at_the_write_bound() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let state = Arc::new(TransportState::for_workspace(
            &fleet.workspace(),
            forge_primitives::ClientConfig::default(),
        ));
        let (_keep, inbound) = mpsc::unbounded_channel();
        let mut peer = StoppedReading { inbound, writing: None };

        tokio::time::timeout(WRITE_BOUND * 2, greet(&mut peer, state, attached_from()))
            .await
            .expect("the greeting ends at the write bound rather than never");
    }

    /// A frame for the write tests, whose encoded length is what the record
    /// has to carry.
    fn a_frame() -> ServerMessage {
        ServerMessage::Error {
            what: "frame".to_owned(),
            why: "a measured write".to_owned(),
            seat: None,
            dispatch_id: None,
        }
    }

    /// **The measurement starts after the frame exists (#2506).** The clock
    /// is handed the serialised frame, so a start read taken before - or
    /// around - the serialisation cannot be recorded: the fake sees the
    /// frame's own length, and the elapsed is its fixed second exactly.
    #[tokio::test]
    async fn the_write_is_timed_from_after_the_frame_is_serialised() {
        let bytes = serde_json::to_string(&a_frame()).expect("the frame encodes").len();
        let clock = FakeClock { starts: std::cell::RefCell::new(Vec::new()), base: Instant::now() };

        let write =
            send_frame_timed(&mut NullSink, a_frame(), &clock).await.expect("the write lands");
        assert!(!write.waited, "a sink that never yields is reported as not waiting");
        assert_eq!(write.bytes, bytes, "the record carries the frame's own size");
        assert_eq!(write.write_ms, 1000.0, "the record's elapsed is the clock's, exactly");
        assert_eq!(
            clock.starts.borrow().as_slice(),
            &[bytes],
            "the clock is read once, with the serialised frame",
        );
    }

    /// **`waited` is not a constant (#2506).** A write that yields says so;
    /// the false case is pinned beside it.
    #[tokio::test]
    async fn a_write_that_yields_is_timed_and_says_so() {
        let bytes = serde_json::to_string(&a_frame()).expect("the frame encodes").len();
        let clock = FakeClock { starts: std::cell::RefCell::new(Vec::new()), base: Instant::now() };

        let mut yielding = YieldsOnce { yielded: false };
        let write =
            send_frame_timed(&mut yielding, a_frame(), &clock).await.expect("the write lands");
        assert!(write.waited, "a flush that yielded once is reported as having waited");
        assert_eq!(write.bytes, bytes, "the record carries the frame's own size");
        assert_eq!(write.write_ms, 1000.0, "the record's elapsed is the clock's, exactly");
    }

    /// The oldest boundary a window names, as a walk below the floor starts
    /// from it - the oldest candidate the file may or may not carry (#2280).
    fn boundary_of(
        window: &crate::transport::conversation::Conversation,
    ) -> Option<forge_primitives::TranscriptAnchor> {
        super::boundary_candidates(window).into_iter().next()
    }

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
            browser_host: None,
            heartbeat: Heartbeat::default(),
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
                match updates.recv().await.map(|frame| frame.update) {
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
            browser_host: None,
            heartbeat: Heartbeat::default(),
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

    /// **A frame queued across a stream replacement is bounded like every
    /// other one** (#2374): the subscribe-queue path is a fourth place a
    /// record crosses to a client, and without its cap the queued frame
    /// carries the record whole while the snapshot carries the marker.
    #[tokio::test]
    async fn a_queued_frame_reaches_the_client_capped() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let state = TransportState {
            surface: fleet.surface(),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(crate::transport::conversation::Conversations::new()),
            live: Mutex::new(crate::live::Live::new()),
            client: forge_primitives::ClientConfig::default(),
            browser_host: None,
            heartbeat: Heartbeat::default(),
        };
        let seat = forge_primitives::SessionSlot::lead("TestOrg", "proj");
        let msg: forge_primitives::Message = serde_json::from_value(serde_json::json!({
            "type": "user",
            "uuid": "tool-result-1",
            "session_id": "s",
            "message": {"role": "user", "content": "the call"},
            "tool_use_result": {"payload": "x".repeat(100_000)},
        }))
        .expect("a user frame");
        let frame = SeatUpdate {
            seq: Some(9),
            update: SessionUpdate::ChatAppended {
                key: seat.clone(),
                origin: None,
                source: None,
                msg,
            },
        };

        let queued = forward_queued(&state, vec![frame], &[Subject::Session(seat)], 1);

        assert_eq!(queued.len(), 1, "the frame is forwarded to the seat's watcher");
        let SessionUpdate::ChatAppended {
            msg: forge_primitives::Message::User { tool_use_result, .. },
            ..
        } = &queued[0].update
        else {
            panic!("the queued frame is the chat frame it was queued as")
        };
        let record = tool_use_result.as_ref().expect("the frame carries its record");
        assert_eq!(record["omitted"], true, "the queued record is bounded: {record}");
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
            browser_host: None,
            heartbeat: Heartbeat::default(),
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
        assert!(reviewable, "and whether the document is reviewable");
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
        let ServerMessage::Error { what, why, seat: named, .. } = refused else {
            panic!("a refused read is an error, not an empty document")
        };
        assert_eq!(what, "document_read", "the refusal names what was asked for");
        assert!(why.contains("read-only"), "and carries the read's own reason: {why}");
        assert_eq!(named, Some(seat), "under the seat that asked");
    }

    /// **A refused dispatch answers the press that made it.** The refusal
    /// carries the dispatch id that press minted, which is what lets a page
    /// tell its own refused press from a sibling's; with nothing to name,
    /// the refusal would have to be attributed by "whoever pressed last",
    /// and the add's own refusal landing after a sibling's press would be
    /// handed to the sibling.
    #[test]
    fn a_refused_dispatch_echoes_the_dispatch_its_press_made() {
        let refused = dispatch_refusal(&DispatchError::PluginActionRunning, Some(41));
        let ServerMessage::Error { what, why, seat, dispatch_id } = refused else {
            panic!("a refusal is an error frame")
        };
        assert_eq!(what, "dispatch", "the tag a client routes a refusal by");
        assert!(
            why.contains("already running"),
            "in the core's own words, not this layer's: {why}"
        );
        assert_eq!(seat, None, "a plugin refusal belongs to no seat");
        assert_eq!(dispatch_id, Some(41), "and it names the dispatch the press made");

        let refused = dispatch_refusal(&DispatchError::PluginActionRunning, None);
        let ServerMessage::Error { dispatch_id, .. } = refused else {
            panic!("a refusal is an error frame")
        };
        assert_eq!(dispatch_id, None, "a press that named none is echoed with none");
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
        let page = page_of_span(&span, 4, false);

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
        let short_page = page_of_span(&short, 8, false);
        assert_eq!(short_page.turns.len(), 6, "precondition: the span holds six turns");
        assert_eq!(
            short_page.cursor.as_deref(),
            Some("r:10000:u100"),
            "a short span's page carries on from the row the span began with",
        );
        let ended = forge_primitives::TranscriptSpan { exhausted: true, ..short };
        assert!(
            page_of_span(&ended, 8, false).cursor.is_none(),
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
            browser_host: None,
            heartbeat: Heartbeat::default(),
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
            browser_host: None,
            heartbeat: Heartbeat::default(),
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
            browser_host: None,
            heartbeat: Heartbeat::default(),
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
        conversations.apply(
            &SessionUpdate::CronPromptAppended {
                key: seat.clone(),
                text: "the cron fired".to_owned(),
                uuid: "cap-cron".to_owned(),
                cron_id: "c1".to_owned(),
                description: None,
            },
            None,
        );
        let following = resulting_row(9_999);
        conversations.apply(
            &SessionUpdate::ChatAppended {
                key: seat.clone(),
                msg: following.clone(),
                origin: None,
                source: None,
            },
            None,
        );
        for at in 0..5_000_usize {
            let window = conversations.get(&seat).expect("the seat is held");
            if row_id(window.lock().messages().first().expect("a front")).as_deref()
                == Some("cap-cron")
            {
                break;
            }
            drop(window);
            conversations.apply(
                &SessionUpdate::ChatAppended {
                    key: seat.clone(),
                    msg: result_frame(at),
                    origin: None,
                    source: None,
                },
                None,
            );
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
            browser_host: None,
            heartbeat: Heartbeat::default(),
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
        conversations.apply(
            &SessionUpdate::ChatAppended {
                key: seat.clone(),
                msg: unnameable_row(),
                origin: None,
                source: None,
            },
            None,
        );
        conversations.apply(
            &SessionUpdate::ChatAppended {
                key: seat.clone(),
                msg: resulting_row(9_999),
                origin: None,
                source: None,
            },
            None,
        );
        for at in 0..5_000_usize {
            let window = conversations.get(&seat).expect("the seat is held");
            let front = window.lock().messages().first().cloned().expect("a front");
            if is_unnameable(&front) {
                break;
            }
            conversations.apply(
                &SessionUpdate::ChatAppended {
                    key: seat.clone(),
                    msg: result_frame(at),
                    origin: None,
                    source: None,
                },
                None,
            );
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

    /// **A walk down a transcript holds the paging contract.** Each page's
    /// cursor is the byte of the row it stopped at, the page below seeks to
    /// it, and the walk keeps descending through the result frames the file
    /// never wrote until the transcript's own first row ends it.
    ///
    /// **Support rather than a regression pin**: the wire contract allows
    /// adjacent pages to overlap, so the assertions here are order, descent
    /// and coverage - and they pass against the shape this replaced. What a
    /// regression in the numbering would break is a page served above its own
    /// floor or a gap between pages, and both show here.
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
            browser_host: None,
            heartbeat: Heartbeat::default(),
        };
        let seat = SessionSlot::lead("TestOrg", "proj");
        let cwd = state.surface.roster().cwd_for(&seat).expect("the seat's own directory");

        let mut below = Some(boundary_of(&held.lock()).expect("the window carries rows"));
        let mut pages = 0_usize;
        // **Every page, kept in the order the walk served them** (newest
        // first). The wire contract lets adjacent pages OVERLAP - a page may
        // repeat a turn the one above it drew - but never leave a hole, so the
        // walk is checked on order, descent and coverage rather than on a
        // turn appearing exactly once.
        let mut served: Vec<Vec<usize>> = Vec::new();
        loop {
            let BelowFloor::Page(page) = rows_below_floor(&state, &seat, &cwd, below, 20).await
            else {
                panic!("the walk's page {pages} is answered from the transcript");
            };
            let texts = turn_texts(&page);
            assert!(!texts.is_empty(), "the walk's page {pages} holds turns");
            // **Each page reads in conversation order.** A page whose turns
            // arrived out of order shows here.
            let numbers: Vec<usize> = texts
                .iter()
                .map(|text| text.trim_start_matches("turn ").parse().expect("a turn's own number"))
                .collect();
            assert!(
                numbers.windows(2).all(|pair| pair[0] < pair[1]),
                "the walk's page {pages} ascends: {numbers:?}",
            );
            // **No hole between one page and the page below it**: the next
            // page's newest turn is at or above the one this page drew, and
            // its oldest is below it - overlap is allowed, a gap is not.
            if let Some(previous) = served.last() {
                // Within a page the turns ascend: its first is the oldest.
                let prev_oldest = *previous.first().unwrap_or(&0);
                let next_newest = *numbers.last().unwrap_or(&0);
                assert!(
                    next_newest + 1 >= prev_oldest,
                    "page {pages} leaves no hole below the page above: {previous:?} then \
                     {numbers:?}",
                );
                assert!(
                    next_newest < prev_oldest,
                    "page {pages} sits below the page above: {previous:?} then {numbers:?}",
                );
            }
            pages += 1;
            assert!(pages < 200, "the walk reached the transcript's beginning");
            served.push(numbers);
            match page.cursor.as_deref().and_then(row_cursor) {
                Some(next) => below = Some(next),
                None => break,
            }
        }
        assert_eq!(
            served.last().and_then(|page| page.first()).copied(),
            Some(0),
            "and the last page it served runs down to the transcript's own first turn",
        );
        // **The window and the pages together cover the transcript's turns.**
        // Every turn the file holds appears in the walk at least once; a turn
        // never served is the hole this test exists for.
        let messages = state.surface.conversation(&seat, &cwd).messages;
        let rendered = crate::transcript::render(&messages, false);
        let expected: Vec<usize> = turn_texts(&crate::transport::wire::Page {
            turns: crate::transport::wire::all_turns(&messages, &rendered),
            cursor: None,
        })
        .iter()
        .map(|text| text.trim_start_matches("turn ").parse().expect("a turn's own number"))
        .collect();
        let window = held.lock();
        let window_numbers: Vec<usize> = turn_texts(&crate::transport::wire::Page {
            turns: crate::transport::wire::all_turns(
                window.messages(),
                &crate::transcript::render(window.messages(), false),
            ),
            cursor: None,
        })
        .iter()
        .map(|text| text.trim_start_matches("turn ").parse().expect("a turn's own number"))
        .collect();
        drop(window);
        let served: std::collections::HashSet<usize> =
            served.into_iter().flatten().chain(window_numbers).collect();
        for turn in &expected {
            assert!(served.contains(turn), "the walk serves turn {turn}");
        }
    }

    /// A connection's far end over two channels: what the test sends, and
    /// what the server writes back.
    struct Peer {
        inbound: mpsc::UnboundedReceiver<Message>,
        outbound: mpsc::UnboundedSender<Message>,
    }

    impl futures_util::Stream for Peer {
        type Item = Result<Message, axum::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            self.inbound.poll_recv(cx).map(|item| item.map(Ok))
        }
    }

    impl futures_util::Sink<Message> for Peer {
        type Error = axum::Error;
        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn start_send(self: std::pin::Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
            let _ = self.outbound.send(item);
            Ok(())
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), Self::Error>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    /// One connection driven over channels: what the test sends, the frames
    /// the server wrote, and the driver's own handle.
    struct Driven {
        to_server: mpsc::UnboundedSender<Message>,
        written: Arc<Mutex<Vec<ServerMessage>>>,
        driven: tokio::task::JoinHandle<anyhow::Result<()>>,
    }

    fn connection(state: &Arc<TransportState>) -> Driven {
        let (to_server, inbound) = mpsc::unbounded_channel();
        let (outbound, mut from_server) = mpsc::unbounded_channel();
        let mut peer = Peer { inbound, outbound };
        let written = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&written);
        // The frames are recorded rather than dropped, so a test can wait on
        // the answer a message actually produces.
        tokio::spawn(async move {
            while let Some(Message::Text(text)) = from_server.recv().await {
                if let Ok(message) = serde_json::from_str::<ServerMessage>(&text) {
                    seen.lock().expect("the frame log").push(message);
                }
            }
        });
        let state = Arc::clone(state);
        let driven = tokio::spawn(async move { drive(&mut peer, &state, attached_from()).await });
        Driven { to_server, written, driven }
    }

    /// Wait until `condition` holds, or fail the test with `why`.
    async fn until(condition: impl Fn() -> bool, why: &str) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while !condition() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {why}"));
    }

    fn home_snapshots(written: &Arc<Mutex<Vec<ServerMessage>>>) -> usize {
        written
            .lock()
            .expect("the frame log")
            .iter()
            .filter(|frame| matches!(frame, ServerMessage::Snapshot { subject: Subject::Home, .. }))
            .count()
    }

    /// Subscribe the home and wait for its first read.
    async fn subscribed_home(
        to_server: &mpsc::UnboundedSender<Message>,
        written: &Arc<Mutex<Vec<ServerMessage>>>,
    ) {
        to_server
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Subscribe {
                    what: Subject::Home,
                    answering: false,
                })
                .expect("the subscribe encodes")
                .into(),
            ))
            .expect("the message goes");
        until(|| home_snapshots(written) == 1, "the home's first read").await;
    }

    /// Subscribe the home AGAIN as an ordering barrier: one socket is handled
    /// in order, so any frame written before this ask is written before its
    /// answer - which lets a test assert an ABSENCE without a sleep.
    async fn subscribed_home_again(
        to_server: &mpsc::UnboundedSender<Message>,
        written: &Arc<Mutex<Vec<ServerMessage>>>,
    ) -> serde_json::Value {
        let before = home_snapshots(written);
        to_server
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Subscribe {
                    what: Subject::Home,
                    answering: false,
                })
                .expect("the subscribe encodes")
                .into(),
            ))
            .expect("the message goes");
        until(|| home_snapshots(written) == before + 1, "the barrier home read").await;
        // **The barrier's own snapshot is the state to assert from**: the
        // frame count can catch an intermediate read, the answer cannot.
        written
            .lock()
            .expect("the frame log")
            .iter()
            .rev()
            .find_map(|frame| match frame {
                ServerMessage::Snapshot { subject: Subject::Home, data } => Some(data.clone()),
                _ => None,
            })
            .expect("the barrier's home snapshot")
    }

    /// Whether a seat's snapshot has been written.
    fn seat_read(written: &Arc<Mutex<Vec<ServerMessage>>>) -> bool {
        written.lock().expect("the frame log").iter().any(|frame| {
            matches!(frame, ServerMessage::Snapshot { subject: Subject::Session(_), .. })
        })
    }

    /// A turn finished on a seat nothing is attached to: the diamond.
    fn arm_diamond(state: &Arc<TransportState>, slot: &SessionSlot) {
        let result = serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "s",
        }))
        .expect("a result message");
        Live::lock(&state.live).apply(&SessionUpdate::ChatAppended {
            key: slot.clone(),
            msg: result,
            origin: None,
            source: None,
        });
    }

    fn showing(to_server: &mpsc::UnboundedSender<Message>, seat: Option<SessionSlot>) {
        to_server
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Showing { seat })
                    .expect("the showing encodes")
                    .into(),
            ))
            .expect("the message goes");
    }

    fn subscribed(to_server: &mpsc::UnboundedSender<Message>, slot: &SessionSlot) {
        to_server
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Subscribe {
                    what: Subject::Session(slot.clone()),
                    answering: false,
                })
                .expect("the subscribe encodes")
                .into(),
            ))
            .expect("the message goes");
    }

    fn lead() -> SessionSlot {
        SessionSlot::lead("TestOrg", "proj")
    }

    /// **The subscription is not the showing.** A seat a client merely listens
    /// to spends nothing; the reader opening it is what spends.
    #[tokio::test]
    async fn subscribing_a_seat_does_not_attach_it_and_showing_it_does() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let state = Arc::new(TransportState::for_workspace(
            &fleet.workspace(),
            forge_primitives::ClientConfig::default(),
        ));
        let slot = lead();
        let Driven { to_server, written, driven } = connection(&state);

        subscribed(&to_server, &slot);
        until(
            || {
                written
                    .lock()
                    .expect("the frame log")
                    .iter()
                    .any(|frame| matches!(frame, ServerMessage::Snapshot { .. }))
            },
            "the subscribe's snapshot",
        )
        .await;
        assert!(
            !Live::lock(&state.live).is_attached(&slot),
            "a subscribe attached the seat, so it spends marks nobody looked at",
        );

        showing(&to_server, Some(slot.clone()));
        until(|| Live::lock(&state.live).is_attached(&slot), "showing the seat to attach it").await;

        showing(&to_server, None);
        until(|| !Live::lock(&state.live).is_attached(&slot), "showing nothing to detach it").await;

        drop(to_server);
        let _ = driven.await;
    }

    /// **The same showing twice is a no-op.** It is a state assertion, not an
    /// event: the second frame must not spend a second mark nor re-encode
    /// the home a connection already holds.
    #[tokio::test]
    async fn showing_the_same_seat_twice_is_a_no_op() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let state = Arc::new(TransportState::for_workspace(
            &fleet.workspace(),
            forge_primitives::ClientConfig::default(),
        ));
        let slot = lead();
        let Driven { to_server, written, driven } = connection(&state);
        subscribed_home(&to_server, &written).await;
        subscribed(&to_server, &slot);
        until(|| seat_read(&written), "the seat's snapshot").await;

        showing(&to_server, Some(slot.clone()));
        until(
            || Live::lock(&state.live).is_attached(&slot),
            "the first showing to attach the seat",
        )
        .await;

        // **A DEATH armed BETWEEN the two showings is what makes this
        // discriminating.** `failed_turn_at` cannot: `detach` stamps
        // `seen_failed` itself, wiping the failure a re-spend would read, so
        // the repeat would find nothing either way. A death is spent only by
        // `seat_shown`, which `detach` leaves alone - so with the early
        // return deleted the repeat attaches, spends the death and re-encodes
        // the home.
        fleet.workspace().record_death_for_test(
            &slot,
            "dead-occupant",
            "the claude subprocess closed its output stream",
        );

        showing(&to_server, Some(slot.clone()));
        // A second home read is the barrier: one socket is handled in order,
        // so a home frame the repeat produced would be written before this
        // answer. The count is the initial read plus the barrier's own.
        subscribed_home_again(&to_server, &written).await;
        assert_eq!(
            home_snapshots(&written),
            2,
            "a repeat showing spent the failure mark and re-encoded the home",
        );
        assert!(Live::lock(&state.live).is_attached(&slot), "the seat was detached");

        drop(to_server);
        let _ = driven.await;
    }

    /// **A showing with no subscribe to pair it with spends nothing.** A page
    /// whose route effect runs before its mount sends `showing` first, so the
    /// showing is remembered; the subscribe that completes the pair is what
    /// attaches, once.
    #[tokio::test]
    async fn a_showing_before_its_subscribe_spends_when_the_subscribe_lands() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let state = Arc::new(TransportState::for_workspace(
            &fleet.workspace(),
            forge_primitives::ClientConfig::default(),
        ));
        let slot = lead();
        // **A mark is drawn on a row**, so the seat needs an agent: the home
        // lists a project's agents, and a seat nothing has started is not
        // among them - its diamond would have nowhere to appear.
        fleet.install_agent("TestOrg", "proj", "lead");
        let Driven { to_server, written, driven } = connection(&state);
        subscribed_home(&to_server, &written).await;
        arm_diamond(&state, &slot);

        showing(&to_server, Some(slot.clone()));
        // The barrier proves the showing spent nothing BEFORE the seat's own
        // subscribe completes the pair: a home frame it produced would be
        // written ahead of this answer on the same socket, and the answer's
        // own `unseen` says whether the diamond is still armed.
        let barrier = subscribed_home_again(&to_server, &written).await;
        assert!(
            !Live::lock(&state.live).is_attached(&slot),
            "a showing for an unwatched seat attached it",
        );
        assert!(
            barrier["unseen"].as_array().is_some_and(|unseen| {
                unseen.iter().any(|held| held["label"] == "lead" && held["project"] == "proj")
            }),
            "a showing with no read served spent the diamond: {barrier}",
        );

        subscribed(&to_server, &slot);
        until(|| seat_read(&written), "the seat's snapshot").await;
        until(
            || Live::lock(&state.live).is_attached(&slot),
            "the subscribe completing the pair to attach it",
        )
        .await;
        until(|| home_snapshots(&written) == 3, "the home refresh a spent mark owes").await;

        drop(to_server);
        let _ = driven.await;
    }

    /// **A connection closing lets go of what it SHOWED, and only that.** Two
    /// connections on one seat: one shows it, the other merely subscribes.
    /// The subscriber's close must not detach the seat the first is watching.
    #[tokio::test]
    async fn a_close_detaches_only_the_seat_that_connection_showed() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let state = Arc::new(TransportState::for_workspace(
            &fleet.workspace(),
            forge_primitives::ClientConfig::default(),
        ));
        let slot = lead();

        let Driven { to_server: first, written: first_written, driven: first_driven } =
            connection(&state);
        // A page subscribes the seat it draws and says it is showing it: the
        // pair is what spends, and only the connection holding both attaches.
        subscribed(&first, &slot);
        until(
            || {
                first_written
                    .lock()
                    .expect("the frame log")
                    .iter()
                    .any(|frame| matches!(frame, ServerMessage::Snapshot { .. }))
            },
            "the first connection's snapshot",
        )
        .await;
        showing(&first, Some(slot.clone()));
        until(
            || Live::lock(&state.live).is_attached(&slot),
            "the first connection to show the seat",
        )
        .await;

        let Driven { to_server: second, written: second_written, driven: second_driven } =
            connection(&state);
        subscribed(&second, &slot);
        until(
            || {
                second_written
                    .lock()
                    .expect("the frame log")
                    .iter()
                    .any(|frame| matches!(frame, ServerMessage::Snapshot { .. }))
            },
            "the second connection's snapshot",
        )
        .await;

        drop(second);
        let _ = second_driven.await;
        assert!(
            Live::lock(&state.live).is_attached(&slot),
            "a subscriber's close detached a seat another connection is showing",
        );

        drop(first);
        let _ = first_driven.await;
        assert!(
            !Live::lock(&state.live).is_attached(&slot),
            "the showing connection's close did not let the seat go",
        );
    }

    /// **A connection that switched seats lets each go once.** Two
    /// connections show one seat, one switches away and then closes: the
    /// seat is still the other's, so the switch's detach must not be
    /// repeated at teardown.
    #[tokio::test]
    async fn a_connection_that_switched_seats_does_not_detach_the_old_one_twice() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let first = lead();
        let second = SessionSlot::worker("TestOrg", "proj", "w1");
        // Both seats have to be answerable, because a showing only spends
        // once its seat is actually watched - and a refused subscribe never
        // watches one. A worker needs its fleet row as well as an agent: the
        // roster resolves its cwd from the live worker entry.
        fleet.install_agent("TestOrg", "proj", "lead");
        fleet.add_worker("TestOrg", "proj", "w1").expect("the worker row");
        fleet.install_agent("TestOrg", "proj", "w1");
        let state = Arc::new(TransportState::for_workspace(
            &fleet.workspace(),
            forge_primitives::ClientConfig::default(),
        ));

        let Driven { to_server, written, driven } = connection(&state);
        let Driven { to_server: other, written: other_written, driven: other_driven } =
            connection(&state);
        // Each page watches the seat it draws; the showing is the other half.
        subscribed(&to_server, &first);
        subscribed(&other, &first);
        until(
            || {
                [&written, &other_written].iter().all(|log| {
                    log.lock()
                        .expect("the frame log")
                        .iter()
                        .any(|frame| matches!(frame, ServerMessage::Snapshot { .. }))
                })
            },
            "both connections' snapshots",
        )
        .await;
        showing(&to_server, Some(first.clone()));
        showing(&other, Some(first.clone()));
        until(
            || Live::lock(&state.live).is_attached(&first),
            "both connections to show the first seat",
        )
        .await;

        subscribed(&to_server, &second);
        showing(&to_server, Some(second.clone()));
        until(
            || Live::lock(&state.live).is_attached(&second),
            "the switch to show the second seat",
        )
        .await;
        assert!(
            Live::lock(&state.live).is_attached(&first),
            "the switch detached the seat the other connection still shows",
        );

        drop(to_server);
        let _ = driven.await;
        assert!(
            Live::lock(&state.live).is_attached(&first),
            "the close detached the seat the connection had already left",
        );
        assert!(
            !Live::lock(&state.live).is_attached(&second),
            "the close did not let go of the seat the connection showed",
        );

        drop(other);
        let _ = other_driven.await;
    }

    /// The home a connection holds is re-encoded when a showing spends a mark
    /// - and a subscribe spends nothing, so it re-encodes nothing.
    #[tokio::test]
    async fn showing_a_spent_seat_refreshes_the_home_and_subscribing_does_not() {
        let fleet =
            crate::testing::Fleet::new(&[("TestOrg", &["proj"])]).expect("the fleet builds");
        let state = Arc::new(TransportState::for_workspace(
            &fleet.workspace(),
            forge_primitives::ClientConfig::default(),
        ));
        let slot = lead();
        let Driven { to_server, written, driven } = connection(&state);

        to_server
            .send(Message::Text(
                serde_json::to_string(&ClientMessage::Subscribe {
                    what: Subject::Home,
                    answering: false,
                })
                .expect("the subscribe encodes")
                .into(),
            ))
            .expect("the message goes");
        until(|| home_snapshots(&written) == 1, "the home's first read").await;

        // A turn finished on a seat nothing is attached to: the diamond.
        let result = serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": "success",
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": false,
            "num_turns": 1,
            "session_id": "s",
        }))
        .expect("a result message");
        Live::lock(&state.live).apply(&SessionUpdate::ChatAppended {
            key: slot.clone(),
            msg: result,
            origin: None,
            source: None,
        });

        subscribed(&to_server, &slot);
        until(
            || {
                written
                    .lock()
                    .expect("the frame log")
                    .iter()
                    .any(|frame| matches!(frame, ServerMessage::Snapshot { .. }))
            },
            "the seat's snapshot",
        )
        .await;
        assert_eq!(home_snapshots(&written), 1, "a subscribe refreshed the home");

        showing(&to_server, Some(slot.clone()));
        until(|| home_snapshots(&written) == 2, "the home refresh a spent mark owes").await;

        drop(to_server);
        let _ = driven.await;
    }

    /// **A closed tab names itself, not the browser.** The tab's own state
    /// and history end with it; a viewer told "the browser was closed"
    /// lowered the whole surface instead of that tab.
    #[tokio::test]
    async fn a_closed_tab_names_itself_in_the_failure() {
        use forge_primitives::browser::{BrowserTabList, BrowserViewState};

        let (_frames, frames_rx) = tokio::sync::watch::channel(None);
        let (view, view_rx) = tokio::sync::watch::channel(BrowserViewState::default());
        drop(view);
        let (_nav, nav_rx) = tokio::sync::watch::channel(None);
        let (_failures, failures_rx) = tokio::sync::watch::channel(None::<String>);
        let (_files, files_rx) = tokio::sync::watch::channel(None);
        let (_downloads, downloads_rx) = tokio::sync::broadcast::channel(4);
        let (_audio, audio_rx) = tokio::sync::broadcast::channel(4);
        let (_tabs, tabs_rx) = tokio::sync::watch::channel(BrowserTabList {
            profile: "O/default".to_owned(),
            id: 1,
            up: true,
            connected: true,
            closing_at_ms: None,
            rows: Vec::new(),
        });
        let attachment = forge_workspace::browser::BrowserAttachment {
            key: "O/default".to_owned(),
            id: 1,
            tab: 3,
            frames: frames_rx,
            view: view_rx,
            tabs: tabs_rx,
            nav: nav_rx,
            failures: failures_rx,
            files: files_rx,
            downloads: downloads_rx,
            audio: audio_rx,
        };
        let mut viewing = Some(Viewing::new(attachment));

        match next_view_event(&mut viewing).await {
            ViewEvent::Failed(reason) => assert_eq!(reason, "tab 3 was closed"),
            _ => panic!("the tab's own end is the surface's refusal"),
        }
    }
}
