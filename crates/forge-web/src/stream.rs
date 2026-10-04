//! The view's two streams, one subscription per tab: `GET /events` for the
//! home, and `GET /session/{org}/{project}/{label}/events` for the session
//! page.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Response, Sse};
use forge_primitives::Message;
use forge_primitives::SessionSlot;
use forge_server::SessionUpdate;
use forge_server::live::Live;
use forge_server::model::LiveTurn;
use futures_util::StreamExt;
use futures_util::stream::{self, Stream};
use maud::Markup;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::server::{WebState, Wiring, home_region};

/// The name the page listens for. One region, one event: the page has no
/// interactive state to preserve, so a wholesale swap is the whole answer.
const FLEET_EVENT: &str = "fleet";

/// The same, for the session page's own stream. Two events on it: the
/// columns' region, and the composer's. They are apart because a morph of
/// the region that held the composer reaches the field the reader is typing
/// into - measured, a draft left alone came back empty after two ticks.
pub(crate) const SESSION_EVENT: &str = "session";
pub(crate) const COMPOSER_EVENT: &str = "composer";

/// The event that says the stream is over. The browser reconnects a
/// stream that just ends, so the page closes this one when it hears it.
const CLOSE_EVENT: &str = "close";

/// How long the region may go unsent with nothing to report. The `when`
/// column and the git columns are computed when the region is rendered, so
/// a fleet quiet enough to send no updates still ages: without a tick the
/// page says `now` about something that finished forty minutes ago, while
/// presenting as live.
const TICK: Duration = Duration::from_secs(10);

/// A page's own stream, holding its seat for as long as the connection
/// lives.
struct Held {
    state: Arc<WebState>,
    slot: SessionSlot,
}

impl Held {
    /// Take the seat up for this connection.
    fn new(state: Arc<WebState>, slot: SessionSlot) -> Self {
        Live::lock(&state.live).attach(&slot);
        Self { state, slot }
    }
}

/// The seat is let go when the connection ends, which is a tab closing or a
/// browser dropping it.
impl Drop for Held {
    fn drop(&mut self) {
        Live::lock(&self.state.live).detach(&self.slot);
    }
}

/// The session page's own stream: its own subscription and its own baseline
/// read, taken in that order.
///
/// The order is the design and it is the opposite of the obvious one.
/// Subscribe first, then read: the read is the baseline the stream is
/// applied on top of, so a message may be in both and is dropped by id when
/// it is. Reading first loses whatever arrived in between, and no later
/// read brings it back.
pub async fn session_events(
    State(wiring): State<Wiring>,
    Path((org, project, label)): Path<(String, String, String)>,
) -> Response {
    let Some(slot) = crate::session::seat(&wiring.state.surface, &org, &project, &label) else {
        return (StatusCode::NOT_FOUND, "no session slot by that name").into_response();
    };
    let held = Held::new(Arc::clone(&wiring.state), slot.clone());
    let receiver = wiring.state.surface.subscribe();
    let cwd = wiring.state.surface.roster().cwd_for(&slot);
    let conversation = crate::session::read_conversation(&wiring.state.surface, &slot, cwd).await;
    // The opening event draws the read, which carries no turn in flight: the
    // connection arms its own clock off the first running state it hears.
    let opening = {
        let region = crate::session::session_region(
            &wiring.state,
            wiring.bound,
            &slot,
            &conversation,
            None,
            false,
        )
        .await;
        stream::once(
            async move { Ok(Event::default().event(SESSION_EVENT).data(region.into_string())) },
        )
    };
    let updates = session_updates(receiver, wiring, slot, conversation, LiveTurn::default(), held);
    let stream = opening.chain(updates).chain(stream::once(async {
        Ok(Event::default().event(CLOSE_EVENT).data("the core's stream ended"))
    }));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20))).into_response()
}

/// One event per update that changes this session, or per tick, carrying
/// the region the page swaps in.
///
/// The conversation the connection holds is what makes a read per update
/// unnecessary: it is the baseline read plus everything the stream has
/// appended since, and a re-read per event would walk the transcript again
/// every time.
fn session_updates(
    receiver: UnboundedReceiver<SessionUpdate>,
    wiring: Wiring,
    slot: SessionSlot,
    conversation: Vec<Message>,
    live: LiveTurn,
    held: Held,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    stream::unfold(
        // The compaction flag rides the carried state with the conversation
        // and the live turn. A local inside the step would be rebuilt false
        // on every yielded region, so the line would go at the next tick
        // rather than when the session said the compaction ended. `held` is
        // carried for the same reason its own type exists: it is dropped
        // with this stream, which is the connection ending.
        (receiver, wiring, slot, conversation, live, false, tick, held),
        |(mut receiver, wiring, slot, conversation, live, mut compacting, mut tick, held)| async move {
            let mut conversation = conversation;
            let mut live = live;
            loop {
                let (columns, composer) = tokio::select! {
                    update = receiver.recv() => {
                        let update = update?;
                        // The rail and the inspector draw the fleet, so an
                        // update they redraw for redraws this page too.
                        let asked = Live::lock(&wiring.state.live).apply(&update);
                        let replaced = replacement(&update, &slot);
                        let handed_over = replaced.is_some();
                        if let Some(history) = replaced {
                            // A new occupant brings its own history, and the
                            // clock the old one was counting on goes with it.
                            conversation = history;
                            live = LiveTurn::default();
                        }
                        let appended = append(&update, &slot, &mut conversation);
                        if let SessionUpdate::ChatAppended { key, msg, .. } = &update
                            && key == &slot
                        {
                            crate::session::apply_to_live_turn(msg, &mut live);
                            if let Some(state) = crate::session::compaction_state(msg) {
                                compacting = state;
                            }
                        }
                        // Two regions, two events, and an update can ask for
                        // both: a prompt redraws the row that reports it and
                        // the dock that answers it. They are apart because a
                        // take's twenty readings a second are the composer's
                        // news alone, and because a morph of the columns must
                        // never reach the field being typed into.
                        //
                        // A row change redraws the box as well: what it draws
                        // from the seat's own row - whether it is blocked,
                        // whether it has a hint, whether a prompt waits -
                        // changes with the row, and a blocked seat has no
                        // field to type in, so no input event would come to
                        // refresh it. A message does not: the box holds none
                        // of the conversation, and pushing it would redraw the
                        // field and the send control it earned from a draft
                        // the server cannot see.
                        let columns = appended || asked.fleet || handed_over;
                        (columns, asked.composer || asked.fleet)
                    }
                    // The tick redraws the columns alone. A pushed box is
                    // drawn from a draft the server does not have, so its
                    // region carries no list: measured on a live page, a tick
                    // that redrew the composer closed an open autocomplete
                    // ten seconds after the stream attached. The seat that
                    // goes away with nothing said is covered instead by the
                    // routes, which answer a failed dispatch with the box the
                    // core would draw rather than with a refusal nothing
                    // swaps.
                    _ = tick.tick() => (true, false),
                };
                if !columns && !composer {
                    continue;
                }
                let mut events: Vec<Result<Event, Infallible>> = Vec::new();
                if columns {
                    let region = crate::session::session_region(
                        &wiring.state,
                        wiring.bound,
                        &slot,
                        &conversation,
                        Some(&live),
                        compacting,
                    )
                    .await;
                    events
                        .push(Ok(Event::default().event(SESSION_EVENT).data(region.into_string())));
                }
                if composer {
                    let home = crate::session::context(&wiring.state, wiring.bound);
                    let roster = wiring.state.surface.roster();
                    let agents = wiring.state.surface.agents();
                    // A push is never the reader's own act, so the box keeps
                    // whatever they have typed.
                    let region = crate::composer::render(
                        &home,
                        &slot,
                        &roster,
                        &agents,
                        "",
                        crate::composer::Draft::Unknown,
                    );
                    events.push(Ok(Event::default()
                        .event(COMPOSER_EVENT)
                        .data(region.into_string())));
                }
                return Some((
                    stream::iter(events),
                    (receiver, wiring, slot, conversation, live, compacting, tick, held),
                ));
            }
        },
    )
    .flatten()
}

/// The conversation a replacement hands over, when it is this seat's.
///
/// A resume or a `/new` puts another occupant in the slot and the page's copy
/// is the one that just left, so the region has to be drawn from the history
/// the update carries instead. A `Connected` carries one only where the seat
/// was already running, and an empty history there leaves the read's own
/// conversation alone: a page opened on a seat nothing was behind has nothing
/// to replace.
fn replacement(update: &SessionUpdate, slot: &SessionSlot) -> Option<Vec<Message>> {
    match update {
        SessionUpdate::SessionReplaced { key, history, .. } if key == slot => Some(history.clone()),
        SessionUpdate::Connected { key, history, .. } if key == slot && !history.is_empty() => {
            Some(history.clone())
        }
        _ => None,
    }
}

/// Fold one update into the connection's conversation, answering whether the
/// page has to be redrawn.
///
/// A message the read already carried is dropped: the read is the baseline
/// and the stream is applied on top of it, so anything in both is already
/// drawn. The identity is the message's own id, which the transcript row and
/// the wire frame share.
///
/// A delivery the workspace injected draws as a turn of its own, forged from
/// the update rather than read off the wire: the CLI does not echo a prompt
/// it was handed on stdin, so the assistant would otherwise answer something
/// nobody saw.
///
/// The read does carry the row the CLI persisted, so a delivery that lands
/// while the read is in flight is drawn from both and repeats until the page
/// reloads. Accepted deliberately: the duplicate is a repeated line, while a
/// rule that matched on the body would drop a message someone really did send
/// twice.
fn append(update: &SessionUpdate, slot: &SessionSlot, conversation: &mut Vec<Message>) -> bool {
    if let Some(turn) = forge_server::delivery::delivery_turn(update, slot) {
        conversation.push(turn);
        return true;
    }
    let SessionUpdate::ChatAppended { key, msg, .. } = update else {
        return false;
    };
    if key != slot {
        return false;
    }
    match message_id(msg) {
        Some(id) if conversation.iter().any(|held| message_id(held) == Some(id)) => false,
        _ => {
            conversation.push(msg.clone());
            true
        }
    }
}

/// The id a streamed message carries, when it carries one.
fn message_id(msg: &Message) -> Option<&str> {
    match msg {
        Message::Assistant { uuid, .. } | Message::User { uuid, .. } => uuid.as_deref(),
        Message::TaskStarted { uuid, .. }
        | Message::TaskUpdated { uuid, .. }
        | Message::TaskProgress { uuid, .. }
        | Message::TaskNotification { uuid, .. }
        | Message::ThinkingTokens { uuid, .. }
        | Message::CommandLifecycle { uuid, .. }
        | Message::TurnDuration { uuid, .. }
        | Message::StopHookSummary { uuid, .. }
        | Message::BackgroundTasksChanged { uuid, .. }
        | Message::CommandsChanged { uuid, .. }
        | Message::HookStarted { uuid, .. }
        | Message::HookResponse { uuid, .. }
        | Message::HookProgress { uuid, .. }
        | Message::Notification { uuid, .. }
        | Message::PermissionDenied { uuid, .. }
        | Message::CompactBoundary { uuid, .. }
        | Message::RateLimitEvent { uuid, .. }
        | Message::Result { uuid: Some(uuid), .. }
        | Message::ToolProgress { uuid: Some(uuid), .. } => Some(uuid),
        Message::Result { uuid: None, .. }
        | Message::ToolProgress { uuid: None, .. }
        | Message::System { .. }
        | Message::StreamEvent { .. }
        | Message::Error { .. }
        | Message::Unknown { .. } => None,
    }
}

/// The page's stream: every connection takes a receiver of its own, so a
/// second tab watches beside the first rather than stealing its events.
pub async fn events(
    State(wiring): State<Wiring>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe before rendering the opening region: an update landing
    // between the two is otherwise lost for the life of the connection,
    // and a sleeping laptop or a backgrounded tab makes that happen on
    // every reconnect. The first thing the stream says is then what the
    // region should be now, followed by whatever arrived while it was
    // being drawn.
    let receiver = wiring.state.surface.subscribe();
    let snapshot = home_region(&wiring.state, wiring.bound).await.into_string();
    let opening =
        stream::once(async move { Ok(Event::default().event(FLEET_EVENT).data(snapshot)) });
    let stream = opening.chain(region_events(receiver, wiring)).chain(stream::once(async {
        Ok(Event::default().event(CLOSE_EVENT).data("the core's stream ended"))
    }));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}

/// Fold the stream into the view's live state for the life of the process,
/// so `Live` is current whether or not a tab is attached: a turn that
/// completes while the page is closed is exactly the case the diamond is
/// for, and nothing inside a connection's own task would record it.
pub async fn fold(mut receiver: UnboundedReceiver<SessionUpdate>, state: Arc<WebState>) {
    while let Some(update) = receiver.recv().await {
        let _ = Live::lock(&state.live).apply(&update);
    }
}

/// One event per update that changes the page, or per tick, carrying the
/// region the page swaps in.
fn region_events(
    receiver: UnboundedReceiver<SessionUpdate>,
    wiring: Wiring,
) -> impl Stream<Item = Result<Event, Infallible>> {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
    // A render slower than the tick must not queue renders: the default
    // bursts, so one slow region would be followed by a backlog of them.
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    stream::unfold((receiver, wiring, tick), |(mut receiver, wiring, mut tick)| async move {
        loop {
            let redraw = tokio::select! {
                // The home's region draws rows and no composer, so a take's
                // readings are not its news and do not re-send it.
                update = receiver.recv() => {
                    Live::lock(&wiring.state.live).apply(&update?).fleet
                }
                _ = tick.tick() => true,
            };
            if !redraw {
                continue;
            }
            let region: Markup = home_region(&wiring.state, wiring.bound).await;
            let event = Event::default().event(FLEET_EVENT).data(region.into_string());
            return Some((Ok(event), (receiver, wiring, tick)));
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::extract::{Path, State};
    use forge_primitives::{Message, SessionSlot};
    use forge_server::SessionUpdate;

    use super::session_events;
    use crate::server::{WebState, Wiring};
    use forge_server::live::Live;

    /// A dispatched agent's frame lands in the conversation the page holds,
    /// and the fold draws nothing for it. Both halves matter: the chat is not
    /// its surface, and the SUBAGENTS section reads the same slice, so a
    /// filter at the stream would empty that section instead.
    #[test]
    fn a_dispatched_frame_reaches_the_conversation_and_draws_nothing() {
        use super::append;

        let slot = SessionSlot::lead("Busytools", "forge");
        let mut conversation: Vec<Message> = Vec::new();
        let child = serde_json::from_value::<Message>(serde_json::json!({
            "type": "assistant",
            "session_id": "s",
            "parent_tool_use_id": "toolu_dispatch",
            "message": {
                "id": "msg_child",
                "role": "assistant",
                "model": "claude-opus-5",
                "content": [{"type": "text", "text": "a dispatched agent working"}],
            },
        }))
        .expect("a parented frame");

        let appended = append(
            &SessionUpdate::ChatAppended { key: slot.clone(), msg: child, origin: None },
            &slot,
            &mut conversation,
        );

        assert!(appended, "the frame is appended, not filtered at the stream");
        assert_eq!(conversation.len(), 1, "and the conversation holds it");
        assert!(
            forge_server::transcript::render_units(&conversation).is_empty(),
            "while the chat draws nothing for it",
        );
    }

    /// The handler is what holds the seat, and the response it hands back is
    /// what carries the guard: the seat is held from the moment the
    /// connection's stream exists and let go when that stream drops. Catches
    /// a handler that registers the seat without keeping the guard, which
    /// nothing else here can see - the seat stays attached, so it is
    /// suppressed for the life of the process, and the page that opened it
    /// is long gone.
    #[tokio::test]
    async fn the_handler_holds_the_seat_for_its_connection() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fleet = forge_server::testing::Fleet::in_dir(dir.path(), &[("Org", &["forge"])])
            .expect("the fleet builds");
        fleet.start("Org", "forge").expect("the project is declared");
        let state = Arc::new(crate::server::WebState::new(
            fleet.surface(),
            Arc::new(forge_server::work::WorkCache::new()),
            forge_primitives::WebConfig::default(),
        ));
        let wiring = Wiring {
            bound: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
            state: Arc::clone(&state),
        };
        let slot = SessionSlot::lead("Org", "forge");

        let response = session_events(
            State(wiring),
            Path(("Org".to_owned(), "forge".to_owned(), "lead".to_owned())),
        )
        .await;
        // A held seat is one the reader is watching, so a turn finishing on it
        // earns no diamond - the attachment read through what it does rather
        // than through the map that holds it.
        assert!(
            !finished_turn_is_unseen(&state, &slot),
            "the connection is served with its seat held",
        );

        drop(response);
        assert!(
            finished_turn_is_unseen(&state, &slot),
            "and the seat is let go when the connection drops, rather than staying suppressed",
        );
    }

    /// Fold a finished turn on `slot`, answering whether it left a completion
    /// this view has not shown - which is what a seat nobody is watching does.
    fn finished_turn_is_unseen(state: &Arc<WebState>, slot: &SessionSlot) -> bool {
        let mut live = Live::lock(&state.live);
        live.apply(&SessionUpdate::ChatAppended {
            key: slot.clone(),
            origin: None,
            msg: serde_json::from_value(serde_json::json!({
                "type": "result",
                "subtype": "success",
                "duration_ms": 1,
                "duration_api_ms": 1,
                "is_error": false,
                "num_turns": 1,
                "session_id": "s",
            }))
            .expect("parse a result message"),
        });
        live.snapshot().unseen.is_unseen(slot)
    }
}
