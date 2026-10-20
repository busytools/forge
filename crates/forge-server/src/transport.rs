//! The socket a client speaks to, and what a connection needs to answer it.
//!
//! One route, `/socket`, at the address the caller bound. Every connection
//! gets its own subscription, so a second client changes nothing about the
//! first and the server sends only what a client asked for.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::routing::get;
use forge_primitives::{ClientConfig, Message, SessionSlot};
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
pub const PROTOCOL_VERSION: u32 = 8;

/// The socket's keepalive: how often a connection is asked to prove it is
/// still there, and how long it may stay silent before it is reaped.
///
/// **The socket carried none**, so a peer that vanished without closing - a
/// slept machine, a wedged process, a network that went away - left its
/// browser role held and any companion waiting, with neither side told the
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
    pub client: ClientConfig,
    /// The browser relay the core owns: a capable connection registers into
    /// it, and the browser tools ask through it. Carried in rather than built
    /// on the spot, because it has to be the SAME relay the sessions' tool
    /// handlers hold - see [`Workspace::browser_relay`](forge_workspace::Workspace::browser_relay).
    pub browser: Arc<forge_workspace::browser::BrowserRelay>,
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
    /// transport's own: the browser relay above all, which has to be the SAME
    /// relay the sessions' browser tools ask through - a transport holding a
    /// relay of its own would register a host into a role nothing routes to,
    /// and every browser tool would answer "no browser-capable client
    /// connected" while a client sat attached.
    pub fn for_workspace(
        workspace: &Arc<forge_workspace::Workspace>,
        client: ClientConfig,
    ) -> Self {
        Self {
            surface: Arc::new(ViewSurface::new(Arc::clone(workspace))),
            work: Arc::new(WorkCache::new()),
            conversations: Arc::new(conversation::Conversations::new()),
            // The process is another viewer of the same seats, so the
            // attachment count has to see it: a turn finishing on a seat this
            // process shows is one the reader watched.
            live: Mutex::new(Live::new()),
            client,
            browser: workspace.browser_relay(),
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
    axum::serve(listener, router).await?;
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

/// Ask the core for a fresh context reading on a seat a page is holding.
///
/// A turn finishing is when a reading stops being true: the transcript grew by
/// the turn, and the number the seat holds was taken before it. A view
/// refreshes on the same frame, for the seat it is addressing, and a client
/// reading a seat is that same act - the answer lands as a
/// [`SessionUpdate::ContextUsageSnapshot`] on the stream that page is already
/// reading.
///
/// Only a seat a page holds, because the reading exists for the reader and the
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
    // A page has to be holding the seat for a turn's ask, because that one is
    // speculative: the reading may never be drawn.
    //
    // **A settled compaction is not speculative - it is corrective, and it
    // does not wait for a page.** The reading it invalidates is the core's,
    // and every view reads that one, so a seat nobody is holding keeps a
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
                    "a turn ended on a seat a client reads and its context reading was not asked \
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
            "a seat a client reads was due a context reading and its probe was not requested",
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
}
