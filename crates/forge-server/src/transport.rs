//! The socket a client speaks to, and what a connection needs to answer it.
//!
//! One route, `/socket`, at the address the caller bound. Every connection
//! gets its own subscription, so a second client changes nothing about the
//! first and the server sends only what a client asked for.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use forge_primitives::{Message, SessionSlot};
use tokio::net::TcpListener;

use crate::SessionUpdate;
use crate::live::Live;
use crate::surface::ViewSurface;
use crate::work::WorkCache;

pub mod batch;
mod connection;
pub mod conversation;
pub mod envelope;
pub mod frame;
mod probe;
pub mod wire;

/// The protocol this server speaks.
///
/// Fixed rather than negotiated: the server and the core change far more
/// slowly than a client's visuals do, so a client either speaks this or it
/// does not, and a mismatch fails plainly instead of silently.
///
/// Named here rather than in `connection.rs` so the socket's recorded
/// contract is filed under it: a bump looks for a record directory that is
/// not there and fails, which is the honest answer for a client that would
/// refuse the connection anyway.
///
/// **Renaming an update is a bump.** A variant's name is the tag it crosses
/// under, so a client that knows the old one narrows the frame to nothing and
/// drops it silently - which is what a version the server bumps exists to
/// prevent, since the skewed pair is real: the desktop client ships
/// separately from the binary. `slack_draft_expired` became
/// `slack_draft_resolved`, and `baselines/socket/1/` keeps the older tag as
/// the record a v1 server emitted. Nothing reads a past version's directory:
/// it is an archive, and its staleness is the point rather than a fault.
///
/// **Removing an update is a bump too, and v3 is one.** The peer surface
/// collapsed to `agents__send_message`, which retired
/// `peer_inflight_stats_changed` along with the per-seat counters and the two
/// agent-row fields that carried them: a subscriber that still expects the
/// variant would wait for a badge that never moves again. `baselines/socket/2/`
/// is the record a v2 server emitted.
///
/// **v4 is a bump for the greet's settings and for the binary path.** The
/// greeting's `settings` gains the `[dictate]` axes a capturing client starts
/// on, `Command` gains `dictate_stream`, and the socket now takes binary
/// messages as dictation frames - a whole message kind a v3 client has no
/// vocabulary for. `baselines/socket/3/` is the record a v3 server emitted.
///
/// **v5 is a bump for the take's owner.** A take belongs to the connection
/// that started it: every `Dictate*` update is forwarded to that connection
/// alone, a second take on the seat is refused by name, and a connection
/// going away DROPS its take rather than submitting what arrived. The
/// record's `composer` loses its `take` and `notice` for the same reason -
/// a take's meter, phases and words are not a fact any other reader may
/// draw. `baselines/socket/4/` is the record a v4 server emitted.
///
/// **v6 is a bump for the browser's ask and answer.** The socket gains its
/// first request in the server-to-client direction: `browser_ask` carries one
/// tool call to whichever client holds the browser role, `browser_answer`
/// carries the result back under the same id, and a subscription declares
/// whether a client can host that role at all. A second binary frame kind
/// joins the dictation frames - a browser answer's image bytes, under its
/// answer's id - so a client with no vocabulary for any of it would read
/// frames as nothing rather than as news. `baselines/socket/5/` is the record
/// a v5 server emitted.
///
/// **v7 is a bump for the browser's hand-off field and for the models read's
/// own shape.** The hand-off's `context` is renamed `profile`, and a
/// proposal's `candidate` is now `candidates`, ranked rows rather than the
/// one the server picked, while the in-use row's `sha256` is nullable where
/// an installed model publishes none. The first two are renames a v6 client
/// reads as nothing, and the third is a value it dereferences (`slice`), so a
/// same-version skew is a client that throws while drawing rather than one
/// that shows less. `baselines/socket/6/` is the record a v6 server emitted.
///
/// **v8 is a bump for the board's own read.** A project's `tasks` is renamed
/// `rows` - each row is its task plus worked time, ages and marks - a row's
/// `estimate` is an object where a v7 server sent a string, and `artifact` is
/// gone, folded into `links`. The rename is what makes this a bump rather than
/// an addition: a v7 client reads `row.tasks.map(...)` and throws on
/// `undefined` while DRAWING, so a same-version skew would be a page that
/// never lands rather than one that shows less. `baselines/socket/7/` is the
/// record a v7 server emitted.
///
/// **v9 is a bump for the board's own subject (#2517).** A project's `rows`,
/// `crons` and `connectors` left the home's project row for a `board` subject
/// of their own, and the home's fleet rows gained `rows` for the header's
/// total. Both directions break silently on a same-version skew: a v9 client
/// asking a v8 server for `board` gets that subject back as an unknown
/// message, and a v8 client reading a v9 home finds no `rows`, `crons` or
/// `connectors` under its projects - it draws an empty board and a zero total
/// until the greeting's version turns it into the notice.
/// `baselines/socket/8/` is the record a v8 server emitted.
///
/// **v10 is a bump for the browser's move to the server.** The browser role
/// and its ask/answer pair are gone: a client attaches to a profile's
/// browser as a viewer, sends gestures and takes the controller role, and
/// receives encoded frames as binary messages. A v9 client would send
/// `browser_answer` to a server that never asks, and the two would sit
/// across the socket unable to drive a browser at all.
/// `baselines/socket/9/` is the record a v9 server emitted.
///
/// **v11 is a bump for the browser's tabs.** A viewer attaches to a tab
/// rather than a profile, the profile verbs are gone, and a strip reads a
/// `browser_tabs` list instead of `browser_profiles`. A v10 client would
/// send `browser_profiles` and a `profile` name a v11 server no longer
/// answers, and would pair every surface state against the wrong tab.
/// `baselines/socket/10/` is the record a v10 server emitted.
/// The tab on a file ask and on a download are additive fields under v11: a
/// v11 client ignores what it does not know, so no reader is left holding a
/// value it cannot draw, and the version stays.
///
/// **v12 is a bump for the client's `showing` message.** A seat's diamond,
/// failure mark and death trace are spent by the seat being shown on screen,
/// not by a subscription to it, so a client that keeps every visited seat
/// subscribed spends nothing until the reader opens one. Both directions
/// break silently on a same-version skew: a v11 server reads `showing` as a
/// message it does not know and answers an `error` naming `client_message`,
/// and a v12 server beside a v11 client never learns what is on screen, so
/// no seat is ever attached and no mark is ever spent - the rail keeps
/// diamonds the reader has already looked at. `baselines/socket/11/` is the
/// record a v11 server emitted.
///
/// **v13 is a bump for a hand-off's answer.** The answer carries what it
/// means, the door in the client that sent it and the connection that
/// carried it, in place of the bare `done` flag. A v12 client's answer names
/// only `done` and cannot be decoded, so **nothing refuses the connection**
/// (the endings it draws are ignored safely), and its answer comes back as
/// a dispatch refusal, which a client draws where the answer came from: the
/// command frame is recognised by its kind even when its body no longer
/// parses, so the person is told the prompt is still waiting instead of
/// losing the answer to the console. `baselines/socket/12/` is the record a
/// v12 server emitted.
///
/// **v14 is a bump for a web client's perf batch.** `client_perf` carries the
/// records a page measures, so a client with no shell to write them still
/// produces its own diagnostics. It shares a number with nothing: v13 was
/// already the hand-off answer's, and two schemas cannot answer to one
/// protocol. A v13 server reads the batch as a message it does not know and
/// answers an `error` naming `client_message`, which the page's own error
/// path takes as a refused subscribe: an in-flight read can be marked
/// refused while the connection stays up. `baselines/socket/13/` is the
/// record a v13 server emitted.
///
/// **v15 is a bump for the browser frame's header.** It grew from 14 to 18
/// bytes: the page's own viewport rides beside the encoded pixels, so a
/// viewer maps a gesture through the picture it draws rather than sending it
/// 1:1 into a page laid out at another shape. A v14 client splits the header
/// at the old offsets and reads the four new bytes as the head of the WebP,
/// so its decoder fails and the surface draws nothing at all - a silent
/// blank, which is exactly what the greeting's version is for.
/// `baselines/socket/14/` is the record a v14 server emitted.
pub const PROTOCOL_VERSION: u32 = 15;

/// The socket's keepalive: how often a connection is asked to prove it is
/// still there, and how long it may stay silent before it is reaped.
///
/// **The socket carried none**, so a peer that vanished without closing - a
/// slept machine, a wedged process, a network that went away - left its
/// viewer attached and any companion waiting, with neither side told the
/// other was gone. A test shortens the pair rather than waiting the shipping
/// one out.
#[derive(Clone, Copy, Debug)]
pub struct Heartbeat {
    pub every: Duration,
    pub dead: Duration,
}

impl Default for Heartbeat {
    fn default() -> Self {
        Self { every: connection::HEARTBEAT_EVERY, dead: connection::HEARTBEAT_DEAD }
    }
}

/// The longest one frame may take to reach the socket before its peer is
/// judged to have stopped reading.
///
/// **A write had no bound**: a peer that vanished with a frame in flight
/// leaves no ping outstanding, so the keepalive cannot see it, and TCP can
/// take minutes to report the socket gone (#2473). The floor is the slowest
/// frame measured - a 3.5 MB snapshot to a slow client, 31.6 s - and 60 s
/// clears it by close to double and is the keepalive's own dead bound, so the
/// two mechanisms measure a dead peer on the same scale.
pub(crate) const WRITE_BOUND: Duration = Duration::from_secs(60);

/// A frame write that outlived [`WRITE_BOUND`].
///
/// **A partial frame cannot be resumed**: a client still parsing the frame it
/// began would read a resumed stream as garbage, so the connection ends
/// rather than the write being retried.
#[derive(Debug)]
pub(crate) struct WriteStalled {
    pub(crate) bound: Duration,
    /// How long the write had run when the bound cut it, which is the only
    /// timing a stalled write ever gets: it never reaches the writer's own
    /// record.
    pub(crate) waited: Duration,
}

impl std::fmt::Display for WriteStalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the peer stopped reading: a frame write did not complete within {:?}",
            self.bound
        )
    }
}

impl std::error::Error for WriteStalled {}

/// Wait on one socket write under `bound`, failing as [`WriteStalled`] when it
/// does not complete in time.
async fn write_within<T, F>(bound: Duration, write: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let at = tokio::time::Instant::now();
    match tokio::time::timeout(bound, write).await {
        Ok(answered) => answered,
        Err(_) => Err(anyhow::Error::new(WriteStalled { bound, waited: at.elapsed() })),
    }
}

/// What a connection answers from: the surface it reads and dispatches
/// through, the working-tree cache behind the git read, the conversations
/// the stream has seeded, the live state a late subscriber cannot
/// reconstruct for itself, and the `[client]` settings the greeting carries.
///
/// The conversations are here rather than on the surface because they are
/// the TRANSPORT's: they exist so this socket stops reading a whole
/// transcript per client per request, and a view reading the same seat
/// through the surface keeps its own copy in its own session state.
pub struct TransportState {
    pub surface: Arc<ViewSurface>,
    pub work: Arc<WorkCache>,
    pub conversations: Arc<conversation::Conversations>,
    pub live: Mutex<Live>,
    /// The server's browser fleet, when one is installed: the connection
    /// attaches viewers, routes input and takes the controller role through
    /// it. `None` is a transport with no fleet (tests, a platform with no
    /// CEF host), where a viewer's attach answers its refusal.
    pub browser_host: Option<Arc<dyn forge_workspace::browser::BrowserHost>>,
    /// The socket's keepalive: how often a connection is asked to prove it
    /// is there, and how long it may stay silent before it is reaped.
    pub heartbeat: Heartbeat,
}

impl TransportState {
    /// The state a running forge serves its socket from, derived from the
    /// core it is a view of.
    ///
    /// **One place, so the pieces cannot drift apart.** Every field here is
    /// something the socket needs that the workspace owns or that is the
    /// transport's own.
    pub fn for_workspace(workspace: &Arc<forge_workspace::Workspace>) -> Self {
        Self {
            surface: Arc::new(ViewSurface::new(Arc::clone(workspace))),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(conversation::Conversations::new()),
            // The process is another viewer of the same seats, so the
            // attachment count has to see it: a turn finishing on a seat this
            // process shows is one the reader watched.
            live: Mutex::new(Live::new()),
            browser_host: workspace.browser_host(),
            heartbeat: Heartbeat::default(),
        }
    }

    /// [`TransportState::for_workspace`] with the keepalive shortened, so a
    /// test can watch a silent peer be reaped without waiting a minute of
    /// real time. Production builds never see it.
    #[cfg(feature = "testing")]
    #[must_use]
    pub fn with_heartbeat(mut self, heartbeat: Heartbeat) -> Self {
        self.heartbeat = heartbeat;
        self
    }
}

/// Serve the socket on `listener` until the process ends.
///
/// The listener is the caller's rather than bound here, so a test can bind
/// port 0 and know the address it bound, and so a caller that cannot bind
/// decides for itself whether that is fatal - which is what keeps a busy
/// port from becoming a new way for forge to refuse to start.
pub async fn serve(state: Arc<TransportState>, listener: TcpListener) -> anyhow::Result<()> {
    // The stream is folded ONCE, here, rather than by each connection: the
    // marks and the composer's state are what the core has said, so they
    // advance whether or not a client is attached - and a connection folding
    // its own copy would both multiply the work by the number of clients and
    // stop the state advancing the moment the last one left.
    //
    // Observing, and without the backlog: the fold renders no prompt, and the
    // boot notice belongs to the view that does.
    tokio::spawn(fold_the_stream(Arc::clone(&state)));

    let router = Router::new().route("/socket", get(connection::upgrade)).with_state(state);
    // Each connection carries the peer it was accepted from: the log names
    // which attached client an open served (#2506).
    axum::serve(listener, router.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await?;
    Ok(())
}

/// Fold the core's stream into the live state and the conversations this
/// transport holds.
///
/// **One fold for the whole socket rather than one per connection**, and it
/// is what seeds a seat: `Connected` and `HistoryReplayed` both arrive here,
/// emitted by the session task in its own order, so a conversation and the
/// frames that follow it have one producer.
async fn fold_the_stream(state: Arc<TransportState>) {
    let mut updates = state.surface.subscribe_mirror();
    let mut probes = probe::ContextProbe::default();
    while let Some(frame) = updates.recv().await {
        // A test's hold, so an encode's catch-up is driven rather than raced.
        #[cfg(any(test, feature = "testing"))]
        while state.conversations.fold_is_held() {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let seq = frame.seq;
        let update = frame.update;
        // **One fill at a time per seat.** An encode's catch-up applies from
        // the window when this loop cannot reach in time, and this loop
        // receives those same frames afterwards: the seat's own lock is what
        // makes the watermark check, the apply and the mark one step, so a
        // frame crosses exactly once whichever path got there first.
        let fill = update.slot().map(|slot| state.conversations.fill_lock(slot));
        let filling = fill
            .as_ref()
            .map(|lock| lock.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
        // **A frame an encode has already folded into the copy is not folded
        // again** - except a sealing one. A seal re-mints from 1, so its number
        // can equal the first new frame's, and a watermark that has taken that
        // frame must not take the seal's history away with it: the reseed it
        // carries is the only path the new epoch's rows have into the copy.
        let epoch_frame = matches!(
            update,
            crate::SessionUpdate::Connected { .. } | crate::SessionUpdate::SessionReplaced { .. }
        );
        if !epoch_frame
            && let (Some(seq), Some(slot)) = (seq, update.slot())
            && seq <= state.conversations.consumed_for(slot)
        {
            continue;
        }
        crate::live::Live::lock(&state.live).apply(&update);
        state.conversations.apply(&update, seq);
        // **The watermark an answer is measured against.** Marked after the
        // apply, and only where there is a copy to take it: a seat nothing has
        // connected on yet holds no frames, so a number marked there would say
        // a copy has something it has not.
        //
        // **An epoch frame RESETS it instead of marking.** That frame is
        // numbered on the run it ends and the next run counts from 1 again, so
        // a watermark left standing would skip the new run's first frames -
        // exactly the numbers it reuses.
        if let Some(slot) = update.slot() {
            if epoch_frame {
                state.conversations.consumed(slot, 0);
            } else if let Some(seq) = seq
                && state.conversations.get(slot).is_some()
            {
                state.conversations.consumed(slot, seq);
            }
        }
        drop(filling);
        request_context_usage(&state, &mut probes, &update);
    }
}

/// Why the socket owes the core a fresh context reading, which is what an
/// update says about a seat's held one.
#[derive(PartialEq, Eq)]
enum Ask {
    /// A turn ended, so the reading is older than the transcript it was taken
    /// from and is asked for again under [`probe`]'s bounds.
    AfterATurn,
    /// A compaction settled, so the reading was taken from a transcript that
    /// no longer exists - wrong rather than merely old, and asked past the
    /// bounds for that reason.
    AfterACompaction,
}

/// Whether this update reports a compaction's own outcome, which is the frame
/// that ends one.
///
/// **The compaction's result field rather than the null status that rides
/// beside it.** The CLI reports a permission-mode change on that same
/// `status`/null shape, and a bare null read as a settle would fire the
/// post-compaction ask - past both bounds - on a mode toggle, mid-compaction,
/// and spend the ask the settle itself is owed.
///
/// A settle with no result field would leave the post-compaction ask unfired
/// and a reading past the token gate unable to be lowered, which is the state
/// this bypass exists to close; the field is on the settle in both pinned
/// captures and a fresh one is where a change to that would show.
///
/// The null half is traded for that, and both directions are disclosed: a result
/// present but null would be read as no settle, and the reading would stand
/// until the next compaction. No captured frame carries it - the corpus has
/// exactly three status shapes, a `compacting` string twice and one settle with
/// a string result - and the null is admitted nowhere else in this predicate.
fn settles_a_compaction(update: &SessionUpdate) -> bool {
    let SessionUpdate::ChatAppended { msg: Message::System { subtype, data, .. }, .. } = update
    else {
        return false;
    };
    subtype == "status" && data.get("compact_result").is_some_and(|result| !result.is_null())
}

/// What this update asks of the core about a seat's context reading.
fn context_ask(update: &SessionUpdate) -> Option<(&SessionSlot, Ask)> {
    let SessionUpdate::ChatAppended { key, msg, .. } = update else {
        return None;
    };
    if settles_a_compaction(update) {
        return Some((key, Ask::AfterACompaction));
    }
    matches!(msg, Message::Result { .. }).then_some((key, Ask::AfterATurn))
}

/// Ask the core for a fresh context reading on a seat a page shows.
///
/// A turn finishing is when a reading stops being true: the transcript grew by
/// the turn, and the number the seat holds was taken before it. A view
/// refreshes on the same frame, for the seat it is addressing, and a client
/// reading a seat is that same act - the answer lands as a
/// [`SessionUpdate::ContextUsageSnapshot`] on the stream that page is already
/// reading.
///
/// Only a seat a page shows, because the reading exists for the reader and the
/// probe costs the CLI a walk over its whole transcript. Bounded by [`probe`],
/// the same limits a view applies to its own ask - except for the
/// post-compaction ask, which may not be refused by the reading it exists to
/// replace.
///
/// Both records below are `debug` lines and nothing else. The refusal is the one
/// asserted, and it is emitted from the spawned fold rather than from anything a
/// test drives - so the test that reads it back holds a subscriber across its
/// awaits rather than calling the crate's `test_support::logged`, which runs a
/// closure to completion on the caller's thread and cannot span the fold. The
/// read path's own ask in `transport::wire` has the same record and no test.
///
/// That refusal is not only the no-agent case: the same arm carries a seat with
/// no stamped session id and one whose session has closed, which is what
/// `refresh_context_usage` refuses.
fn request_context_usage(
    state: &TransportState,
    probes: &mut probe::ContextProbe,
    update: &SessionUpdate,
) {
    let Some((key, why)) = context_ask(update) else {
        return;
    };
    // A page has to be showing the seat for a turn's ask, because that one is
    // speculative: the reading may never be drawn.
    //
    // **A settled compaction is not speculative - it is corrective, and it
    // does not wait for a page.** The reading it invalidates is the core's,
    // and every view reads that one, so a seat nobody is showing keeps a
    // number that is wrong rather than merely old - and a page opened on it
    // afterwards reads that number and is never asked, the read path guarding
    // on a reading being present. Nothing else asks for it either: a turn's
    // ask is refused by the reading the settle has yet to replace.
    if why == Ask::AfterATurn && !Live::lock(&state.live).is_attached(key) {
        return;
    }
    let now = Instant::now();
    match why {
        Ask::AfterATurn => {
            if let Err(declined) = probes.admit(key, || state.surface.header(key).context, now) {
                tracing::debug!(
                    event_name = "context_usage_refresh_skipped",
                    ?declined,
                    slot = %key.display(),
                    "a turn ended on a seat a client shows and its context reading was not asked \
                     for",
                );
                return;
            }
        }
        Ask::AfterACompaction => probes.force(key, now),
    }
    if let Err(error) = state.surface.refresh_context_usage(key) {
        tracing::debug!(
            event_name = "context_usage_request_failed",
            %error,
            slot = %key.display(),
            "a seat a client shows was due a context reading and its probe was not requested",
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The shipping keepalive is the pair the book states.** Those numbers
    /// are what a client author reads, and the dead one is how fast a call on
    /// a paused host fails, so a drift here must fail a test rather than
    /// quietly change both.
    #[test]
    fn the_shipping_heartbeat_is_the_documented_pair() {
        let heartbeat = Heartbeat::default();
        assert_eq!(heartbeat.every, Duration::from_secs(15), "a ping every 15 s");
        assert_eq!(heartbeat.dead, Duration::from_secs(60), "and 60 s without an answer");
    }

    /// **The shipping write bound is the value the book states.** It is what
    /// the book tells a client author about a peer that stopped reading, so a
    /// drift must fail a test rather than quietly change it - and it must
    /// clear the slowest frame measured (31.6 s) with room.
    ///
    /// The three claims are ordered value, floor, pairing: a run reports the
    /// first that fails and the next run reports the next, which is enough for
    /// a constant this small.
    #[test]
    fn the_shipping_write_bound_is_the_documented_value() {
        assert_eq!(WRITE_BOUND, Duration::from_secs(60), "one frame, 60 s");
        assert!(
            WRITE_BOUND > Duration::from_secs_f64(31.6),
            "the slowest frame measured must not trip the bound: {WRITE_BOUND:?}",
        );
        assert_eq!(
            WRITE_BOUND,
            Heartbeat::default().dead,
            "the book says the two bounds match, so the mechanisms measure a dead peer on one \
             scale",
        );
    }
}
