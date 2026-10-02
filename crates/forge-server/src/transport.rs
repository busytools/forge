//! The socket a client speaks to, and what a connection needs to answer it.
//!
//! One route, `/socket`, at the address the caller bound. Every connection
//! gets its own subscription, so a second client changes nothing about the
//! first and the server sends only what a client asked for.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::routing::get;
use forge_primitives::{Message, WebConfig};
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
        crate::live::Live::lock(&state.live).apply(&update);
        state.conversations.apply(&update);
        request_context_usage_if_a_turn_ended(&state, &mut probes, &update);
    }
}

/// Ask the core for a fresh context reading when a turn ends on a seat a page
/// is holding.
///
/// A turn finishing is when a reading stops being true: the transcript grew by
/// the turn, and the number the seat holds was taken before it. The terminal
/// refreshes on the same frame, for the seat it is addressing, and a client
/// reading a seat is that same act - the answer lands as a
/// [`SessionUpdate::ContextUsageSnapshot`] on the stream that page is already
/// reading.
///
/// Only a seat a page holds, because the reading exists for the reader and the
/// probe costs the CLI a walk over its whole transcript. Bounded by
/// [`probe`], the same two limits the terminal applies to its own ask.
fn request_context_usage_if_a_turn_ended(
    state: &TransportState,
    probes: &mut probe::ContextProbe,
    update: &SessionUpdate,
) {
    let SessionUpdate::ChatAppended { key, msg: Message::Result { .. }, .. } = update else {
        return;
    };
    if !Live::lock(&state.live).is_attached(key) {
        return;
    }
    if let Err(declined) = probes.admit(key, &state.surface.header(key).context, Instant::now()) {
        tracing::debug!(
            event_name = "context_usage_refresh_skipped",
            ?declined,
            slot = %key.display(),
            "a turn ended on a seat a client reads and its context reading was not asked for",
        );
        return;
    }
    if let Err(error) = state.surface.refresh_context_usage(key) {
        tracing::debug!(
            event_name = "context_usage_request_failed",
            %error,
            slot = %key.display(),
            "a turn ended on a seat a client reads and its context probe was not requested",
        );
    }
}
