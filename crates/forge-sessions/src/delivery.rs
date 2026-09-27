//! The turn a delivery draws as: a cron fire, a Gotify notification, a Slack
//! message or a peer comm.
//!
//! Each of them reaches a session's model as a prompt on its own stdin, and
//! the CLI does not echo a prompt back, so the wire carries nothing a view
//! could draw and a page that only drew wire frames would show the assistant
//! answering something nobody saw. The workspace announces each as a typed
//! update instead, and this turns that update into the display-only user turn
//! the fold's envelope detection recognises, carrying the same prose the
//! model received.

use forge_primitives::{ContentBlock, Message, SessionSlot, UserEnvelope};

use crate::surface::SessionUpdate;

/// The display-only user turn `update` draws as, when it delivers to `slot`.
/// `None` for every other update, and for a delivery to another seat: the
/// same stream carries the whole fleet.
pub fn delivery_turn(update: &SessionUpdate, slot: &SessionSlot) -> Option<Message> {
    let text = match update {
        // The prefix is the fold's own key rather than anything the model
        // reads: `detect_inbound` picks the envelope out of the prose.
        SessionUpdate::CronPromptAppended { key, text } if key == slot => {
            format!("[Cron]\n\n{text}")
        }
        SessionUpdate::GotifyNotificationAppended { key, notification } if key == slot => {
            notification.to_prose()
        }
        SessionUpdate::SlackMessageAppended { key, prose } if key == slot => prose.clone(),
        SessionUpdate::PeerEnvelopeAppended { key, wrapped } if key == slot => wrapped.to_prose(),
        _ => return None,
    };
    Some(Message::User {
        message: UserEnvelope { role: "user".to_owned(), content: vec![ContentBlock::Text { text }] },
        session_id: String::new(),
        parent_tool_use_id: None,
        uuid: None,
        tool_use_result: None,
    })
}
