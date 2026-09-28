//! The view's two streams, one subscription per tab: `GET /events` for the
//! home, and `GET /session/{org}/{project}/{label}/events` for the session
//! page.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive};
use axum::response::{IntoResponse, Response, Sse};
use forge_primitives::Message;
use forge_primitives::SessionSlot;
use forge_primitives::runtime::RuntimeSessionState;
use forge_sessions::SessionUpdate;
use forge_sessions::model::LiveTurn;
use forge_sessions::surface::is_success_result;
use futures_util::StreamExt;
use futures_util::stream::{self, Stream};
use maud::Markup;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::composer::Composer;
use crate::server::{WebState, Wiring, home_region};
use crate::unseen::Unseen;

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

/// What the view has learned from the stream, which the first render and
/// every later one both read.
#[derive(Default)]
pub struct Live {
    unseen: Unseen,
    /// The seats a page is open on, by how many connections are showing
    /// them: a turn finishing on one of those is a turn the reader watched.
    attached: HashMap<SessionSlot, usize>,
    composer: Composer,
}

/// What the stream has said, as one render reads it. A render takes this
/// rather than the lock: the guard is not `Send`, and a handler that held
/// it across its own awaits could not be one.
#[derive(Default)]
pub struct LiveState {
    pub unseen: Unseen,
    pub composer: Composer,
}

impl Live {
    pub fn new() -> Self {
        Self::default()
    }

    /// A panicking task must not take the view's live state with it.
    pub fn lock(live: &Mutex<Self>) -> MutexGuard<'_, Self> {
        live.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn snapshot(&self) -> LiveState {
        LiveState { unseen: self.unseen.clone(), composer: self.composer.clone() }
    }

    /// This view has shown `slot`, so nothing about it is unseen.
    pub fn seen(&mut self, slot: &SessionSlot) {
        self.unseen.clear(slot);
    }

    /// A page is open on `slot`, which is this view showing it, so a mark
    /// armed before the page opened goes with it. Counted, because two tabs
    /// on one seat are one seat still being shown.
    pub fn attach(&mut self, slot: &SessionSlot) {
        *self.attached.entry(slot.clone()).or_default() += 1;
        self.unseen.clear(slot);
    }

    /// One page on `slot` has gone. The seat is let go with the last of them.
    pub fn detach(&mut self, slot: &SessionSlot) {
        let Some(count) = self.attached.get_mut(slot) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.attached.remove(slot);
        }
    }

    /// Fold one update in, answering what it asks of each page.
    ///
    /// The filter is what keeps a busy turn from re-sending the fleet for
    /// every token of it: only the updates that can change what a page
    /// draws redraw it. Two answers rather than one, because the pages are
    /// different: a take's twenty readings a second are the composer's news
    /// and not the fleet's, and the fleet's region carries no composer.
    pub fn apply(&mut self, update: &SessionUpdate) -> Redraw {
        let composer = self.composer.apply(update);
        let fleet = match update {
            SessionUpdate::ChatAppended { key, msg } => match msg {
                Message::Result { is_error, subtype, .. }
                    if is_success_result(*is_error, subtype) =>
                {
                    // A turn finished on a session this page is not
                    // showing, so the row earns its diamond until the
                    // session is opened. A seat a page is open on has
                    // already shown it, so it earns nothing, and the row
                    // settles out of running like any other.
                    if !self.attached.contains_key(key) {
                        self.unseen.mark_completed(key);
                    }
                    true
                }
                Message::System { subtype, data, .. } if subtype == "session_state_changed" => {
                    // Work started again, which supersedes the completion
                    // the diamond marks. Serving the seat's page clears it
                    // too, so this is the clear for a seat nobody opened.
                    if forge_sessions::translate::state_parsing::parse_runtime_session_state(
                        data.get("state"),
                    ) == Some(RuntimeSessionState::Running)
                    {
                        self.unseen.clear(key);
                    }
                    true
                }
                Message::BackgroundTasksChanged { .. } => true,
                _ => false,
            },
            // The row set, and what each row is. A spawn or a replacement
            // is a fresh occupant, whose history is not a completion this
            // page has failed to show.
            SessionUpdate::Spawning { key, .. }
            | SessionUpdate::Connected { key, .. }
            | SessionUpdate::SessionReplaced { key, .. } => {
                self.unseen.clear(key);
                true
            }
            // Everything else that changes what a row or a card says. The
            // catalog, the dictation snapshot and the claude version all
            // arrive after the listener binds: a page opened in those
            // first seconds would otherwise keep the empty answer it
            // painted until the next tick.
            SessionUpdate::CatalogLoaded
            | SessionUpdate::CliVersionChanged
            | SessionUpdate::DictateAvailability
            | SessionUpdate::ConnectionFailed { .. }
            | SessionUpdate::AuthRequired { .. }
            | SessionUpdate::TurnError { .. }
            | SessionUpdate::TurnCancelled { .. }
            | SessionUpdate::PermissionRequest { .. }
            | SessionUpdate::QuestionRequest { .. }
            // Answering moves the seat out of the rail's needs-you group and
            // drops the inspector's pending row, so it redraws a row even
            // though it is the composer that asked for it.
            | SessionUpdate::PendingInteractionResolved { .. }
            | SessionUpdate::WorkerStatusChanged { .. } => true,
            // Everything else is the conversation, which this page does
            // not show.
            _ => false,
        };
        Redraw { fleet, composer }
    }
}

/// What one update asks of the two pages that fold the stream: the fleet
/// region, which draws rows and marks, and the composer, which draws a
/// take, a prompt and a sign-in. An update can be news to one and not the
/// other, so one answer cannot serve both.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Redraw {
    pub fleet: bool,
    pub composer: bool,
}

impl Redraw {
    /// Whether either page has to be redrawn.
    pub fn any(self) -> bool {
        self.fleet || self.composer
    }
}

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
                        if let SessionUpdate::ChatAppended { key, msg } = &update
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
                    // The tick redraws the box too. Nothing has been said,
                    // so the seat may have gone away under a page drawing it
                    // live, and a click would then reach a core holding no
                    // session: the pushed box keeps the field and the
                    // controls it earned, so the redraw costs the reader
                    // nothing.
                    _ = tick.tick() => (true, true),
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
                    )
                    .await;
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
    if let Some(turn) = forge_sessions::delivery::delivery_turn(update, slot) {
        conversation.push(turn);
        return true;
    }
    let SessionUpdate::ChatAppended { key, msg } = update else {
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
        | Message::Result { uuid: Some(uuid), .. } => Some(uuid),
        Message::Result { uuid: None, .. }
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
    use forge_sessions::SessionUpdate;

    use super::{Live, session_events};
    use crate::server::Wiring;

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
            &SessionUpdate::ChatAppended { key: slot.clone(), msg: child },
            &slot,
            &mut conversation,
        );

        assert!(appended, "the frame is appended, not filtered at the stream");
        assert_eq!(conversation.len(), 1, "and the conversation holds it");
        assert!(
            forge_sessions::transcript::render_units(&conversation).is_empty(),
            "while the chat draws nothing for it",
        );
    }

    fn result_message(subtype: &str, is_error: bool) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "result",
            "subtype": subtype,
            "duration_ms": 1,
            "duration_api_ms": 1,
            "is_error": is_error,
            "num_turns": 1,
            "session_id": "s",
        }))
        .expect("parse a result message")
    }

    fn appended(key: &SessionSlot, msg: Message) -> SessionUpdate {
        SessionUpdate::ChatAppended { key: key.clone(), msg }
    }

    fn session_state(state: &str) -> Message {
        serde_json::from_value(serde_json::json!({
            "type": "system",
            "subtype": "session_state_changed",
            "session_id": "s",
            "state": state,
        }))
        .expect("parse a state message")
    }

    /// A diamond goes when its seat moves on without being looked at: work
    /// started again, or a fresh occupant took the slot. The other clear,
    /// the seat's page being served, is pinned through the route.
    #[test]
    fn starting_work_again_clears_the_diamond() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.apply(&appended(&slot, result_message("success", false)));
        assert!(live.snapshot().unseen.is_unseen(&slot), "precondition: the turn armed it");

        assert!(
            live.apply(&appended(&slot, session_state("running"))).fleet,
            "a turn starting redraws the page",
        );
        assert!(
            !live.snapshot().unseen.is_unseen(&slot),
            "and clears a completion nobody looked at",
        );

        // The same for a slot a new occupant took: its history is not a
        // completion this page failed to show.
        let taken = SessionSlot::lead("Org", "other");
        live.apply(&appended(&taken, result_message("success", false)));
        assert!(live.snapshot().unseen.is_unseen(&taken), "precondition: armed");

        live.apply(&SessionUpdate::Connected {
            key: taken.clone(),
            session_id: forge_primitives::SessionId::new("new"),
            cwd: "/proj".to_owned(),
            current_model: forge_primitives::CurrentModel {
                resolved_id: "claude".to_owned(),
                display_name_short: "claude".to_owned(),
                display_name_long: "claude".to_owned(),
                requested_id: None,
                catalog_id: None,
                supports_effort: false,
                supported_effort_levels: Vec::new(),
                supports_auto_mode: None,
                supports_adaptive_thinking: None,
                is_authoritative: true,
            },
            available_models: Vec::new(),
            mode: None,
            history: Vec::new(),
            compaction_count: 0,
        });

        assert!(
            !live.snapshot().unseen.is_unseen(&taken),
            "a replaced occupant starts from a clean row",
        );

        // And clearing one slot leaves the rest alone, which is the
        // property the fold has to keep: a new turn somewhere is not news
        // about somewhere else.
        let untouched = SessionSlot::lead("Org", "untouched");
        live.apply(&appended(&slot, result_message("success", false)));
        live.apply(&appended(&untouched, result_message("success", false)));
        live.apply(&appended(&slot, session_state("running")));
        let unseen = live.snapshot().unseen;
        assert!(!unseen.is_unseen(&slot), "the slot that started again is cleared");
        assert!(unseen.is_unseen(&untouched), "and the slot that did not keeps its diamond");
    }

    /// Catches a diamond armed by the wrong result, and a page redrawn for
    /// the conversation it does not show.
    #[test]
    fn only_a_finished_turn_arms_the_diamond() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();

        assert!(
            !live.apply(&appended(&slot, result_message("error_during_execution", true))).fleet,
            "a turn that failed is not a turn that finished",
        );
        assert!(!live.snapshot().unseen.is_unseen(&slot), "so nothing is unseen");

        assert!(
            live.apply(&appended(&slot, result_message("success", false))).fleet,
            "a finished turn redraws the page",
        );
        assert!(live.snapshot().unseen.is_unseen(&slot), "and leaves the diamond");

        let other = SessionSlot::lead("Org", "other");
        assert!(
            live.apply(&appended(&other, result_message("success", false))).fleet,
            "a second slot's finish is the same kind of event",
        );
        let unseen = live.snapshot().unseen;
        assert!(unseen.is_unseen(&other), "and earns its own diamond");
        assert!(unseen.is_unseen(&slot), "without clearing the first slot's");
    }

    /// A turn that finishes on a seat whose page is open is a turn the reader
    /// watched, so it earns no diamond - and the row still redraws, because
    /// the turn that was running has ended.
    #[test]
    fn an_attached_seat_earns_no_diamond_and_still_settles() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.attach(&slot);

        assert!(
            live.apply(&appended(&slot, result_message("success", false))).fleet,
            "the row settles out of running, so the page is redrawn",
        );
        assert!(
            !live.snapshot().unseen.is_unseen(&slot),
            "and the page that is open on it has shown the turn",
        );
    }

    /// The other half: with the page gone, a completion is unlooked again.
    #[test]
    fn a_seat_arms_again_once_its_page_closes() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.attach(&slot);
        live.detach(&slot);

        assert!(
            live.apply(&appended(&slot, result_message("success", false))).fleet,
            "the diamond is the home's news again",
        );
        assert!(
            live.snapshot().unseen.is_unseen(&slot),
            "so a seat nobody is showing earns its diamond",
        );
    }

    /// Two tabs on one seat are two connections and one seat still being
    /// shown. Catches holding the attachment as a flag, where closing either
    /// tab re-arms a mark the other tab is still displaying.
    #[test]
    fn two_pages_on_one_seat_hold_it_until_both_close() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.attach(&slot);
        live.attach(&slot);
        live.detach(&slot);

        live.apply(&appended(&slot, result_message("success", false)));
        assert!(
            !live.snapshot().unseen.is_unseen(&slot),
            "the tab still open on it has shown the turn",
        );

        live.detach(&slot);
        live.apply(&appended(&slot, result_message("success", false)));
        assert!(
            live.snapshot().unseen.is_unseen(&slot),
            "and with both gone the seat is unlooked again",
        );
    }

    /// The window between the page being served and its stream attaching: a
    /// completion landing in it would otherwise sit on a page that is open.
    #[test]
    fn attaching_clears_a_mark_that_armed_before_it() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.apply(&appended(&slot, result_message("success", false)));
        assert!(live.snapshot().unseen.is_unseen(&slot), "precondition: the turn armed it");

        live.attach(&slot);
        assert!(
            !live.snapshot().unseen.is_unseen(&slot),
            "the page opening is the reader being shown the seat",
        );
    }

    /// A seat nobody is showing leaves the map rather than sitting in it at
    /// zero, which on a process up for a week is a seat-shaped leak.
    #[test]
    fn a_closed_page_leaves_no_entry_behind() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.attach(&slot);
        assert_eq!(live.attached.len(), 1, "precondition: the seat is held");

        live.detach(&slot);
        assert!(
            live.attached.is_empty(),
            "the last page closing takes the seat out of the map: {:?}",
            live.attached,
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
        let fleet = forge_sessions::testing::Fleet::in_dir(dir.path(), &[("Org", &["forge"])])
            .expect("the fleet builds");
        fleet.start("Org", "forge").expect("the project is declared");
        let state = Arc::new(crate::server::WebState::new(
            fleet.surface(),
            Arc::new(crate::work::WorkCache::new()),
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
        assert!(
            Live::lock(&state.live).attached.contains_key(&slot),
            "the connection is served with its seat held",
        );

        drop(response);
        assert!(
            !Live::lock(&state.live).attached.contains_key(&slot),
            "and the seat is let go when the connection drops, rather than staying suppressed",
        );
    }

    /// The bulk of the stream is the conversation, which this page does not
    /// draw; redrawing for it would re-send the fleet per token.
    #[test]
    fn a_chat_message_does_not_redraw_the_page() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();

        assert!(
            !live
                .apply(&SessionUpdate::ChatAppended {
                    key: slot,
                    msg: serde_json::from_value(serde_json::json!({
                        "type": "user",
                        "message": { "role": "user", "content": "hello" },
                        "session_id": "s",
                    }))
                    .expect("parse a user message"),
                })
                .any(),
            "a chat message is not something this page draws",
        );
    }

    /// A take's readings are the composer's news, not the fleet's. The
    /// region this stream re-sends draws rows, so redrawing it for every
    /// level would rebuild the whole page twenty times a second while a
    /// take runs - and a composer still has to redraw for them, or the meter
    /// never moves. Catches either answer being taken for the other.
    #[test]
    fn a_takes_readings_redraw_the_composer_and_not_the_fleet() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();

        for update in [
            SessionUpdate::DictateStarted { key: slot.clone(), floor_db: -50.0, generation: 1 },
            SessionUpdate::DictateLevel { key: slot.clone(), peak_db: -20.0 },
        ] {
            let redraw = live.apply(&update);
            assert!(!redraw.fleet, "{update:?} is not a row changing");
            assert!(redraw.composer, "{update:?} is the composer's to draw");
        }
    }

    /// The updates that land after the listener binds, and that a page
    /// opened in that window has already painted an answer for: the
    /// catalog scan, the dictation snapshot and the claude version probe.
    /// Catches a page that keeps the empty answer until the next tick.
    #[test]
    fn the_late_boot_updates_redraw_the_page() {
        let mut live = Live::new();

        for update in [
            SessionUpdate::CatalogLoaded,
            SessionUpdate::DictateAvailability,
            SessionUpdate::CliVersionChanged,
        ] {
            assert!(live.apply(&update).fleet, "{update:?} is exactly a render wake-up");
        }
    }
}
