//! The socket a client speaks to, and what a connection needs to answer it.
//!
//! One route, `/socket`, at the address the caller bound. Every connection
//! gets its own subscription, so a second client changes nothing about the
//! first and the server sends only what a client asked for.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::routing::get;
use forge_primitives::{Message, SessionSlot, WebConfig};
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
pub const PROTOCOL_VERSION: u32 = 5;

/// What a connection answers from: the surface it reads and dispatches
/// through, the working-tree cache behind the git read, the conversations
/// the stream has seeded, the live state a late subscriber cannot
/// reconstruct for itself, and the configuration the greeting carries the
/// client's half of.
///
/// The conversations are here rather than on the surface because they are
/// the TRANSPORT's: they exist so this socket stops reading a whole
/// transcript per client per request, and the terminal reading the same seat
/// through the surface keeps its own copy in its own session state.
pub struct TransportState {
    pub surface: Arc<ViewSurface>,
    pub work: Arc<WorkCache>,
    pub conversations: Arc<conversation::Conversations>,
    pub live: Mutex<Live>,
    pub config: WebConfig,
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
    while let Some(update) = updates.recv().await {
        crate::live::Live::lock(&state.live).apply(&update);
        state.conversations.apply(&update);
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
/// the turn, and the number the seat holds was taken before it. The terminal
/// refreshes on the same frame, for the seat it is addressing, and a client
/// reading a seat is that same act - the answer lands as a
/// [`SessionUpdate::ContextUsageSnapshot`] on the stream that page is already
/// reading.
///
/// Only a seat a page holds, because the reading exists for the reader and the
/// probe costs the CLI a walk over its whole transcript. Bounded by [`probe`],
/// the same limits the terminal applies to its own ask - except for the
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
