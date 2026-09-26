//! `GET /events`: the page's subscription, one stream per tab.

use std::convert::Infallible;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::State;
use axum::response::Sse;
use axum::response::sse::{Event, KeepAlive};
use forge_primitives::Message;
use forge_primitives::cloud::service_status::ServiceIssue;
use forge_sessions::SessionUpdate;
use forge_sessions::surface::is_success_result;
use futures_util::StreamExt;
use futures_util::stream::{self, Stream};
use maud::Markup;
use tokio::sync::mpsc::UnboundedReceiver;

use crate::server::{Wiring, home_region};
use crate::unseen::Unseen;

/// The name the page listens for. One region, one event: the page has no
/// interactive state to preserve, so a wholesale swap is the whole answer.
const FLEET_EVENT: &str = "fleet";

/// The event that says the stream is over. The browser reconnects a
/// stream that just ends, so the page closes this one when it hears it.
const CLOSE_EVENT: &str = "close";

/// What the view has learned from the stream, which the first render and
/// every later one both read.
#[derive(Default)]
pub struct Live {
    unseen: Unseen,
    upstream: Option<ServiceIssue>,
}

/// What the stream has said, as one render reads it. A render takes this
/// rather than the lock: the guard is not `Send`, and a handler that held
/// it across its own awaits could not be one.
#[derive(Default)]
pub struct LiveState {
    pub unseen: Unseen,
    pub upstream: Option<ServiceIssue>,
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
        LiveState { unseen: self.unseen.clone(), upstream: self.upstream.clone() }
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
                // A held prompt is answered in the session, and this is
                // the echo that says the row is free again.
                Message::System { subtype, .. } if subtype == "session_state_changed" => true,
                Message::BackgroundTasksChanged { .. } => true,
                _ => false,
            },
            SessionUpdate::ServiceStatus { severity, message } => {
                self.upstream =
                    Some(ServiceIssue { severity: *severity, message: message.clone() });
                true
            }
            // The row set, and what each row is.
            SessionUpdate::Spawning { .. }
            | SessionUpdate::Connected { .. }
            | SessionUpdate::SessionReplaced { .. }
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

/// The page's stream: every connection takes a receiver of its own, so a
/// second tab watches beside the first rather than stealing its events.
pub async fn events(
    State(wiring): State<Wiring>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // The page is rendered before its stream attaches, and a sleeping
    // laptop or a backgrounded tab reconnects later: the first thing the
    // stream says is what the region should be right now, so neither gap
    // leaves the page stale until the next update happens to arrive.
    let snapshot = home_region(&wiring.state, wiring.bound).await.into_string();
    let opening =
        stream::once(async move { Ok(Event::default().event(FLEET_EVENT).data(snapshot)) });
    let stream =
        opening.chain(region_events(wiring.state.surface.subscribe(), wiring)).chain(stream::once(
            async { Ok(Event::default().event(CLOSE_EVENT).data("the core's stream ended")) },
        ));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(20)))
}

/// One event per update that changes the page, carrying the region the
/// page swaps in.
fn region_events(
    receiver: UnboundedReceiver<SessionUpdate>,
    wiring: Wiring,
) -> impl Stream<Item = Result<Event, Infallible>> {
    stream::unfold((receiver, wiring), |(mut receiver, wiring)| async move {
        loop {
            let update = receiver.recv().await?;
            if !Live::lock(&wiring.state.live).apply(&update) {
                continue;
            }
            let region: Markup = home_region(&wiring.state, wiring.bound).await;
            let event = Event::default().event(FLEET_EVENT).data(region.into_string());
            return Some((Ok(event), (receiver, wiring)));
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

    /// The upstream banner is the last word the probe had, and it takes
    /// an incident as readily as a warning.
    #[test]
    fn the_probe_sets_the_banner() {
        let mut live = Live::new();
        assert!(live.snapshot().upstream.is_none(), "nothing is said before the probe runs");

        assert!(
            live.apply(&SessionUpdate::ServiceStatus {
                severity: forge_primitives::cloud::service_status::ServiceSeverity::Warning,
                message: "Elevated error rates".to_owned(),
            }),
            "a status change redraws the page",
        );
        let banner = live.snapshot().upstream.expect("the banner is set");
        assert_eq!(banner.message, "Elevated error rates");
    }
}
