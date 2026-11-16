//! The one turn forge still forges from a typed update: the notice telling a
//! sender its message never landed.
//!
//! Every other delivery - a cron fire, a Gotify notification, a Slack message,
//! a peer comm - rides the CLI's own queue and draws its row at the CLI's
//! `started` frame, in the CLI's order. The notice is the exception because it
//! is not a turn of the sender's conversation at all: it answers a send that
//! never arrived, and its row is the only carrier the words get.

use forge_primitives::{Message, SessionSlot};
use forge_workspace::PromptSource;
use forge_workspace::WrappedKind;

use crate::surface::SessionUpdate;

/// The instant a forged row was minted, in the form the wire stamps rows with.
fn rfc3339_now() -> String {
    time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}

/// The display-only user turn `update` draws as, when it delivers to `slot`,
/// with the sender the frame that draws it carries.
/// `None` for every other update, for a delivery to another seat (the same
/// stream carries the whole fleet), and for every delivery whose row the
/// session task draws at the CLI's `started` instead.
///
/// The prose is checked against the same parser the fold reads it back with,
/// and a drift is recorded against the notice that produced it.
pub fn delivery_turn(
    update: &SessionUpdate,
    slot: &SessionSlot,
) -> Option<(Message, PromptSource)> {
    let (source, kind, text, uuid) = match update {
        // **The notice is the one delivery forge draws itself, at dispatch.**
        // It answers a message that never arrived, so its words are not a
        // turn the sender's conversation opened: this row is the only carrier
        // the failure gets, and it must be visible even while the sender's
        // seat is busy or its CLI cannot follow a queue.
        SessionUpdate::PeerEnvelopeAppended { key, wrapped, uuid }
            if key == slot && wrapped.kind == WrappedKind::DeliveryFailureNotice =>
        {
            (PromptSource::Peer, "peer_envelope", wrapped.to_prose(), uuid)
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
            source = kind,
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
    // and never sees this frame, and the id is what pairs the row with the
    // queued prompt it answers.
    let mut row = Message::display_only_user(text, uuid.clone());
    // Stamped at the forge, because this row OPENS the turn the notice runs
    // as: the CLI never echoes a prompt, so a page attaching mid-turn has
    // nothing else to anchor the running clock on.
    row.stamp_if_absent(&rfc3339_now());
    Some((row, source))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::logged;
    use forge_primitives::ContentBlock;
    use forge_workspace::{GotifyNotification, MessageId, WrappedPrompt};

    fn notice(slot: &SessionSlot, kind: WrappedKind, sender_name: &str) -> SessionUpdate {
        SessionUpdate::PeerEnvelopeAppended {
            key: slot.clone(),
            uuid: "cap-notice".to_owned(),
            wrapped: WrappedPrompt {
                id: MessageId("m-7f3a92e0".to_owned()),
                kind,
                sender_name: sender_name.to_owned(),
                sender_org: "Busytools".to_owned(),
                body: "target session connection lost".to_owned(),
            },
        }
    }

    /// **The failure notice's row is stamped at the forge.** It opens the
    /// turn the notice runs as - the CLI never echoes a prompt - so a page
    /// attaching mid-turn anchors its running clock on this row, and an
    /// unstamped one leaves it reading 0.0s until the turn's first wire frame
    /// lands.
    #[test]
    fn the_failure_notice_carries_the_stamp_a_clock_starts_from() {
        let slot = SessionSlot::new("TestOrg", "forge", "lead");
        let update = notice(&slot, WrappedKind::DeliveryFailureNotice, "companies");

        let before = time::OffsetDateTime::now_utc();
        let (turn, _) =
            delivery_turn(&update, &slot).expect("the notice to this seat forges a turn");
        let after = time::OffsetDateTime::now_utc();

        let value = serde_json::to_value(&turn).expect("encode");
        let stamp =
            value["timestamp"].as_str().expect("the row carries a stamp where a clock reads one");
        // **The code's own clock, not merely a parseable string**: a constant
        // stamp would draw this row a clock years old while a parse-only pin
        // stayed green.
        let at = time::OffsetDateTime::parse(stamp, &time::format_description::well_known::Rfc3339)
            .unwrap_or_else(|error| {
                panic!("the notice stamped a time no client can read: {stamp} ({error})")
            });
        let slack = time::Duration::seconds(5);
        assert!(
            at >= before - slack && at <= after + slack,
            "the forged row carries the forge's own clock, not a constant: {stamp} sits outside \
             {before}..{after}",
        );
    }

    /// **Every other delivery forges nothing here.** Its row draws at the
    /// CLI's `started`, emitted by the session task, so a frame forged from
    /// the typed update as well would draw the same words twice - once above
    /// the answer the running turn was still writing.
    #[test]
    fn only_a_delivery_failure_notice_forges_a_row() {
        let slot = SessionSlot::new("TestOrg", "forge", "lead");
        let updates = [
            SessionUpdate::CronPromptAppended {
                key: slot.clone(),
                text: "run the morning summary".to_owned(),
                uuid: "cap-cron".to_owned(),
                cron_id: "c1".to_owned(),
                description: Some("Morning summary".to_owned()),
            },
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
            SessionUpdate::SlackMessageAppended {
                key: slot.clone(),
                prose: "[Slack - workspace 'Trust Machines', granite-staging-alerts] \
                        id C0AE ts 1789.5\nunknown: _Large STX Transfer_ [ts 1789.5]"
                    .to_owned(),
                uuid: "cap-slack".to_owned(),
            },
            notice(&slot, WrappedKind::Message, "terminal-cleanups"),
            notice(&slot, WrappedKind::WorkerSpawnFailedNotice, "builder"),
        ];
        for update in &updates {
            assert!(
                delivery_turn(update, &slot).is_none(),
                "only the failure notice forges a row, and this is {update:?}",
            );
        }
        // The same stream carries the whole fleet, so the notice addressed to
        // another seat forges nothing here either.
        let elsewhere = SessionSlot::new("OtherOrg", "forge", "lead");
        assert!(
            delivery_turn(
                &notice(&slot, WrappedKind::DeliveryFailureNotice, "companies"),
                &elsewhere
            )
            .is_none(),
            "a notice for another seat must not draw in this one",
        );
    }

    /// The notice's prose goes through the fold's parser, so it has to survive
    /// it and come back carrying what it was built from - the row is the
    /// only carrier the sender gets.
    #[test]
    fn the_failure_notice_forges_prose_the_parser_reads_back() {
        use crate::envelope::PeerInboundKind;

        let slot = SessionSlot::new("TestOrg", "forge", "lead");
        let update = notice(&slot, WrappedKind::DeliveryFailureNotice, "companies");

        let (turn, source) =
            delivery_turn(&update, &slot).expect("the notice to this seat forges a turn");
        assert_eq!(source, PromptSource::Peer, "the notice is peer traffic");
        let Message::User { message, uuid, .. } = &turn else {
            panic!("the notice draws as the user turn it was dispatched as")
        };
        assert_eq!(
            uuid.as_deref(),
            Some("cap-notice"),
            "the forged row is stamped with the id the notice was dispatched under",
        );
        let Some(ContentBlock::Text { text, .. }) = message.content.first() else {
            panic!("the turn carries the prose the model received")
        };
        let kind = crate::envelope::detect_inbound(text)
            .unwrap_or_else(|| panic!("the fold has to read the notice back: {text}"));
        let PeerInboundKind::DeliveryFailure { target, org, reason } = kind else {
            panic!("the notice reads back as a delivery failure, not {kind:?}");
        };
        assert_eq!(target, "companies", "naming the seat that never took the message");
        assert_eq!(org, "Busytools");
        assert_eq!(reason, "target session connection lost", "and the reason it gave");
    }

    /// Prose the parser cannot read draws nothing, while the notice was still
    /// dispatched - the one failure this forge can produce that no test
    /// spanning the two crates would catch, since forge-workspace cannot
    /// depend on a view. The forger records the drift it cannot fix.
    #[test]
    fn prose_that_does_not_read_as_an_envelope_is_recorded() {
        let slot = SessionSlot::new("TestOrg", "forge", "lead");
        // A `]` in the name ends the header before the failure clause, which
        // is the one way this producer can hand over a prose the parser
        // rejects.
        let update = notice(&slot, WrappedKind::DeliveryFailureNotice, "bad]name");

        let events = logged(|| {
            let (turn, _) =
                delivery_turn(&update, &slot).expect("the notice still draws its words");
            assert!(matches!(turn, Message::User { .. }), "the notice's own prose is not withheld");
        });

        assert!(
            events.iter().any(|(level, fields)| {
                *level == tracing::Level::ERROR
                    && fields.contains("envelope_prose_unrecognised")
                    && fields.contains(r#"source="peer_envelope""#)
                    && fields.contains(r#"org="TestOrg""#)
                    && fields.contains(r#"project="forge""#)
                    && fields.contains(r#"label="lead""#)
            }),
            "an unreadable forge is recorded against the notice that produced it, \
             naming the seat that lost the words: {events:?}",
        );
    }
}
