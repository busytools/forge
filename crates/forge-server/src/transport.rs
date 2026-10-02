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

mod connection;
pub mod conversation;
pub mod envelope;
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
pub const PROTOCOL_VERSION: u32 = 1;

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
        // Read before the fold, because the TRANSITION is the news: a seat that
        // was compacting and is not any more has had the transcript taken out
        // from under whatever reading it holds.
        let was_compacting = settles_a_compaction(&update)
            && update.slot().is_some_and(|slot| Live::lock(&state.live).is_compacting(slot));
        crate::live::Live::lock(&state.live).apply(&update);
        state.conversations.apply(&update);
        request_context_usage(&state, &mut probes, &update, was_compacting);
    }
}

/// Why the socket owes the core a fresh context reading, which is what an
/// update says about a seat's held one.
enum Ask {
    /// A turn ended, so the reading is older than the transcript it was taken
    /// from and is asked for again under [`probe`]'s bounds.
    AfterATurn,
    /// A compaction settled, so the reading was taken from a transcript that
    /// no longer exists - wrong rather than merely old, and asked past the
    /// bounds for that reason.
    AfterACompaction,
}

/// Whether this update is the status frame that clears a compaction.
///
/// The status pair is the only place the CLI says either way, and the frame
/// that clears one is where a probe reads the post-compaction transcript: the
/// `compact_boundary` that records it and the turn's own result both land
/// later.
fn settles_a_compaction(update: &SessionUpdate) -> bool {
    let SessionUpdate::ChatAppended { msg: Message::System { subtype, data, .. }, .. } = update
    else {
        return false;
    };
    subtype == "status" && data.get("status").is_some_and(serde_json::Value::is_null)
}

/// What this update asks of the core about a seat's context reading.
fn context_ask(update: &SessionUpdate, was_compacting: bool) -> Option<(&SessionSlot, Ask)> {
    let SessionUpdate::ChatAppended { key, msg, .. } = update else {
        return None;
    };
    if was_compacting {
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
/// Both records below are `debug` lines and nothing else, so no test sees them:
/// the read path's own ask in `transport::wire` has the same hole, and no test
/// here reaches the failed arm at all, the fixture's stub accepting every
/// command it is handed.
fn request_context_usage(
    state: &TransportState,
    probes: &mut probe::ContextProbe,
    update: &SessionUpdate,
    was_compacting: bool,
) {
    let Some((key, why)) = context_ask(update, was_compacting) else {
        return;
    };
    if !Live::lock(&state.live).is_attached(key) {
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
