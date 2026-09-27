//! `GET /events`: the page's subscription, one stream per tab.

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

use crate::server::{WebState, Wiring, home_region};
use crate::unseen::Unseen;

/// The name the page listens for. One region, one event: the page has no
/// interactive state to preserve, so a wholesale swap is the whole answer.
const FLEET_EVENT: &str = "fleet";

/// The same, for the session page's own stream.
const SESSION_EVENT: &str = "session";

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
}

/// What the stream has said, as one render reads it. A render takes this
/// rather than the lock: the guard is not `Send`, and a handler that held
/// it across its own awaits could not be one.
#[derive(Default)]
pub struct LiveState {
    pub unseen: Unseen,
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
        LiveState { unseen: self.unseen.clone() }
    }

    /// Fold one update in, answering whether the page has to be redrawn.
    ///
    /// The filter is what keeps a busy turn from re-sending the fleet for
    /// every token of it: only the updates that can change what this page
    /// draws redraw it.
    pub fn apply(&mut self, update: &SessionUpdate) -> bool {
        match update {
            SessionUpdate::ChatAppended { key, msg } => match msg {
                Message::Result { is_error, subtype, .. }
                    if is_success_result(*is_error, subtype) =>
                {
                    // A turn finished on a session this page is not
                    // showing, so the row earns its diamond until the
                    // session is opened.
                    self.unseen.mark_completed(key);
                    true
                }
                Message::System { subtype, data, .. } if subtype == "session_state_changed" => {
                    // Work started again, which supersedes a completion
                    // nobody looked at. Clearing on OPEN is the real
                    // semantic and lands with the row's route; this only
                    // bounds the mark until then, so it cannot outlive the
                    // turn it reported.
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
            | SessionUpdate::WorkerStatusChanged { .. } => true,
            // Everything else is the conversation, which this page does
            // not show.
            _ => false,
        }
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
    let receiver = wiring.state.surface.subscribe();
    let cwd = wiring.state.surface.roster().cwd_for(&slot);
    let conversation = crate::session::read_conversation(&wiring.state.surface, &slot, cwd).await;
    // The opening event draws the read, which carries no turn in flight: the
    // connection arms its own clock off the first running state it hears.
    let opening = {
        let region =
            crate::session::session_region(&wiring.state, wiring.bound, &slot, &conversation, None)
                .await;
        stream::once(
            async move { Ok(Event::default().event(SESSION_EVENT).data(region.into_string())) },
        )
    };
    let updates = session_updates(receiver, wiring, slot, conversation, LiveTurn::default());
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
) -> impl Stream<Item = Result<Event, Infallible>> {
    let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + TICK, TICK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    stream::unfold(
        (receiver, wiring, slot, conversation, live, tick),
        |(mut receiver, wiring, slot, conversation, live, mut tick)| async move {
            let mut conversation = conversation;
            let mut live = live;
            loop {
                let redraw = tokio::select! {
                    update = receiver.recv() => {
                        let update = update?;
                        // The rail and the inspector draw the fleet, so an
                        // update they redraw for redraws this page too.
                        let fleet = Live::lock(&wiring.state.live).apply(&update);
                        let appended = append(&update, &slot, &mut conversation);
                        if let SessionUpdate::ChatAppended { key, msg } = &update
                            && key == &slot
                        {
                            crate::session::apply_to_live_turn(msg, &mut live);
                        }
                        appended || fleet
                    }
                    _ = tick.tick() => true,
                };
                if !redraw {
                    continue;
                }
                let region = crate::session::session_region(
                    &wiring.state,
                    wiring.bound,
                    &slot,
                    &conversation,
                    Some(&live),
                )
                .await;
                let event = Event::default().event(SESSION_EVENT).data(region.into_string());
                return Some((Ok(event), (receiver, wiring, slot, conversation, live, tick)));
            }
        },
    )
}

/// Fold one update into the connection's conversation, answering whether
/// the page has to be redrawn.
///
/// A message the read already carried is dropped: the read is the baseline
/// and the stream is applied on top of it, so anything in both is already
/// drawn. The identity is the message's own id, which the transcript row and
/// the wire frame share.
///
/// The prompts the workspace injects are the one class this cannot settle.
/// A cron fire, a Gotify delivery, a Slack bundle or a peer comm reaches the
/// stream as text alone and reaches the read as the envelope-wrapped row the
/// CLI persisted, with no id shared between them, so a delivery that lands
/// during the read is drawn twice until the page reloads. Accepted
/// deliberately: the duplicate is a repeated line, while a rule that matched
/// on the body would drop a message someone really did send twice.
fn append(update: &SessionUpdate, slot: &SessionSlot, conversation: &mut Vec<Message>) -> bool {
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
                update = receiver.recv() => {
                    Live::lock(&wiring.state.live).apply(&update?)
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
    use forge_primitives::{Message, SessionSlot};
    use forge_sessions::SessionUpdate;

    use super::Live;

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

    /// The diamond is bounded by the work it reported: a session that
    /// started again, or a slot a fresh occupant took, is not an unlooked
    /// completion. Clearing on open is the real semantic and lands with
    /// the row's route.
    #[test]
    fn starting_work_again_clears_the_diamond() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();
        live.apply(&appended(&slot, result_message("success", false)));
        assert!(live.snapshot().unseen.is_unseen(&slot), "precondition: the turn armed it");

        assert!(
            live.apply(&appended(&slot, session_state("running"))),
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
            !live.apply(&appended(&slot, result_message("error_during_execution", true))),
            "a turn that failed is not a turn that finished",
        );
        assert!(!live.snapshot().unseen.is_unseen(&slot), "so nothing is unseen");

        assert!(
            live.apply(&appended(&slot, result_message("success", false))),
            "a finished turn redraws the page",
        );
        assert!(live.snapshot().unseen.is_unseen(&slot), "and leaves the diamond");

        let other = SessionSlot::lead("Org", "other");
        assert!(
            live.apply(&appended(&other, result_message("success", false))),
            "a second slot's finish is the same kind of event",
        );
        let unseen = live.snapshot().unseen;
        assert!(unseen.is_unseen(&other), "and earns its own diamond");
        assert!(unseen.is_unseen(&slot), "without clearing the first slot's");
    }

    /// The bulk of the stream is the conversation, which this page does not
    /// draw; redrawing for it would re-send the fleet per token.
    #[test]
    fn a_chat_message_does_not_redraw_the_page() {
        let slot = SessionSlot::lead("Org", "forge");
        let mut live = Live::new();

        assert!(
            !live.apply(&SessionUpdate::ChatAppended {
                key: slot,
                msg: serde_json::from_value(serde_json::json!({
                    "type": "user",
                    "message": { "role": "user", "content": "hello" },
                    "session_id": "s",
                }))
                .expect("parse a user message"),
            }),
            "a chat message is not something this page draws",
        );
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
            assert!(live.apply(&update), "{update:?} is exactly a render wake-up");
        }
    }
}
