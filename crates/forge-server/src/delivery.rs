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

use forge_primitives::{Message, SessionSlot};

use crate::surface::SessionUpdate;

/// The display-only user turn `update` draws as, when it delivers to `slot`.
/// `None` for every other update, and for a delivery to another seat: the
/// same stream carries the whole fleet.
///
/// The prose is checked against the same parser the fold reads it back with,
/// and a drift is recorded against the delivery that produced it.
pub fn delivery_turn(update: &SessionUpdate, slot: &SessionSlot) -> Option<Message> {
    let (source, text, uuid) = match update {
        // The prefix is the fold's own key rather than anything the model
        // reads: `detect_inbound` picks the envelope out of the prose.
        SessionUpdate::CronPromptAppended { key, text, uuid, .. } if key == slot => {
            ("cron_prompt", format!("[Cron]\n\n{text}"), uuid)
        }
        SessionUpdate::GotifyNotificationAppended { key, notification, uuid } if key == slot => {
            ("gotify_notification", notification.to_prose(), uuid)
        }
        SessionUpdate::SlackMessageAppended { key, prose, uuid } if key == slot => {
            ("slack_message", prose.clone(), uuid)
        }
        SessionUpdate::PeerEnvelopeAppended { key, wrapped, uuid } if key == slot => {
            ("peer_envelope", wrapped.to_prose(), uuid)
        }
        _ => return None,
    };
    // The turn draws only if the fold's parser reads the prose back, so a
    // disagreement between forge's own two halves costs the user a message
    // the model still received.
    if crate::envelope::detect_inbound(&text).is_none() {
        let head: String = text.chars().take(120).collect();
        tracing::error!(
            event_name = "envelope_prose_unrecognised",
            source,
            outcome = "chat_echo_dropped",
            org = slot.org(),
            project = slot.project(),
            label = slot.label(),
            prose_head = %head,
            "forged envelope prose did not match detect_inbound; the model still \
             received it but no view will draw it",
        );
    }
    // Forged rather than read off the wire, carrying the prompt's own id:
    // the duplicate-skip the CLI applies to re-sent uuids is a stdin concern
    // and never sees this frame, and the id is what lets a view hold this row
    // while the prompt waits in the pile.
    Some(Message::display_only_user(text, uuid.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::logged;
    use forge_primitives::ContentBlock;

    /// Prose the parser cannot read draws nothing, while the model still
    /// received it - the one failure this forge can produce that no test
    /// spanning the two crates would catch, since forge-workspace cannot
    /// depend on a view. The forger records the drift it cannot fix, and
    /// names which delivery drifted.
    #[test]
    fn prose_that_does_not_read_as_an_envelope_is_recorded() {
        let slot = SessionSlot::new("TestOrg", "forge", "lead");
        let update = SessionUpdate::SlackMessageAppended {
            key: slot.clone(),
            prose: "no bracket header here".to_owned(),
            uuid: "cap-slack".to_owned(),
        };

        let events = logged(|| {
            let turn =
                delivery_turn(&update, &slot).expect("a delivery to this seat still forges a turn");
            assert!(matches!(turn, Message::User { .. }), "the model's own prose is not withheld");
        });

        assert!(
            events.iter().any(|(level, fields)| {
                *level == tracing::Level::ERROR
                    && fields.contains("envelope_prose_unrecognised")
                    && fields.contains(r#"source="slack_message""#)
                    && fields.contains(r#"org="TestOrg""#)
                    && fields.contains(r#"project="forge""#)
                    && fields.contains(r#"label="lead""#)
            }),
            "an unreadable forge is recorded against the delivery that produced it, \
             naming the seat that lost the message: {events:?}",
        );
    }

    /// All four deliveries draw through the fold's parser, so each one's
    /// prose has to survive it and come back carrying what it was built
    /// from. Three of the four forge that prose here; the Slack variant
    /// hands over its producer's, whose shape `forge-workspace` pins on its
    /// own side, so this pins the parse rather than the format.
    #[test]
    fn every_delivery_forges_prose_the_parser_reads_back() {
        use crate::envelope::PeerInboundKind;
        use forge_workspace::{GotifyNotification, MessageId, WrappedKind, WrappedPrompt};

        let slot = SessionSlot::new("TestOrg", "forge", "lead");
        let cases = [
            (
                SessionUpdate::CronPromptAppended {
                    key: slot.clone(),
                    text: "run the morning summary".to_owned(),
                    uuid: "cap-cron".to_owned(),
                    cron_id: "c1".to_owned(),
                    description: Some("Morning summary".to_owned()),
                },
                "run the morning summary",
            ),
            (
                SessionUpdate::GotifyNotificationAppended {
                    key: slot.clone(),
                    notification: GotifyNotification {
                        app: "Backups".to_owned(),
                        title: "Nightly backup complete".to_owned(),
                        message: "All volumes backed up".to_owned(),
                        priority: 3,
                    },
                    uuid: "cap-gotify".to_owned(),
                },
                "All volumes backed up",
            ),
            (
                SessionUpdate::SlackMessageAppended {
                    key: slot.clone(),
                    prose: "[Slack - workspace 'Trust Machines', granite-staging-alerts] \
                            id C0AE ts 1789.5\nunknown: _Large STX Transfer_ [ts 1789.5]"
                        .to_owned(),
                    uuid: "cap-slack-2".to_owned(),
                },
                "Large STX Transfer",
            ),
            (
                SessionUpdate::PeerEnvelopeAppended {
                    key: slot.clone(),
                    uuid: "cap-peer".to_owned(),
                    wrapped: WrappedPrompt {
                        id: MessageId("m-7f3a92e0".to_owned()),
                        kind: WrappedKind::Message,
                        sender_name: "terminal-cleanups".to_owned(),
                        sender_org: "Busytools".to_owned(),
                        body: "the swap case is constructed".to_owned(),
                    },
                },
                "the swap case is constructed",
            ),
        ];

        // The same stream carries the whole fleet, so a delivery addressed to
        // another seat forges nothing here.
        let elsewhere = SessionSlot::new("OtherOrg", "forge", "lead");
        for (update, _) in &cases {
            assert!(
                delivery_turn(update, &elsewhere).is_none(),
                "a delivery for another seat must not draw in this one",
            );
        }

        for (update, expected) in &cases {
            let turn = delivery_turn(update, &slot).expect("a delivery to this seat forges a turn");
            let Message::User { message, uuid, .. } = &turn else {
                panic!("a delivery draws as the user turn the model's prompt was")
            };
            // **The prompt's own id, off the envelope.** It is what a view
            // holds the row by while the prompt waits, and what pairs the row
            // with the page's later copy - a row minted without it, or with a
            // fresh one, is a row nothing can settle.
            let carried_id = match update {
                SessionUpdate::CronPromptAppended { uuid, .. }
                | SessionUpdate::GotifyNotificationAppended { uuid, .. }
                | SessionUpdate::SlackMessageAppended { uuid, .. }
                | SessionUpdate::PeerEnvelopeAppended { uuid, .. } => uuid,
                other => panic!("this test only carries deliveries: {other:?}"),
            };
            assert_eq!(
                uuid.as_deref(),
                Some(carried_id.as_str()),
                "the forged row is stamped with the id the prompt was dispatched under",
            );
            let Some(ContentBlock::Text { text, .. }) = message.content.first() else {
                panic!("the turn carries the prose the model received")
            };
            let kind = crate::envelope::detect_inbound(text).unwrap_or_else(|| {
                panic!(
                    "the fold has to read this delivery's prose back, or it draws nothing: {text}"
                )
            });
            // Only the Slack header rejects a body it cannot read. Cron,
            // Gotify and peer all accept an empty one, so "it parsed" is not
            // yet "the payload arrived".
            let carried = match kind {
                PeerInboundKind::Cron { prompt } => prompt,
                PeerInboundKind::Gotify { message, .. } => message,
                PeerInboundKind::Slack { body, .. } | PeerInboundKind::Message { body, .. } => body,
                other => panic!("this delivery forged prose the fold read as {other:?}"),
            };
            assert!(
                carried.contains(expected),
                "the fold reads this delivery's payload back, or the view draws it empty: {carried}",
            );
        }
    }
}
