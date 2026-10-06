//! Payloads that arrived while their slot had no live session, addressed
//! by `(org, project, label)` - the triple the `sessions` table is keyed
//! by.

use std::collections::HashMap;
use std::sync::Arc;

use crate::SessionSlot;
use crate::mcp::gotify::types::GotifyNotification;
use crate::mcp::peers::types::{PeerFailureReason, WrappedPrompt};

/// Everything parked for one slot, in the order a connecting session
/// delivers it.
#[derive(Default)]
pub(crate) struct ParkedForSlot {
    pub peer: Vec<ParkedPeer>,
    pub cron: Vec<crate::crons::PendingCron>,
    pub gotify: Vec<GotifyNotification>,
    pub slack: Vec<forge_primitives::slack::SlackMessage>,
}

/// One peer message waiting for its slot to connect, with the seat that
/// sent it.
///
/// The sender rides here for the DELIVERY ACK alone: if the spawn this
/// waited on fails, the bucket is dropped and the sender is told. It is
/// not outstanding-ask tracking - nothing looks an entry up by id, and
/// nothing here survives the delivery.
pub(crate) struct ParkedPeer {
    pub(crate) sender: SessionSlot,
    pub(crate) wrapped: WrappedPrompt,
}

/// Parked payloads, one bucket per slot.
pub(crate) type ParkedMap = HashMap<SessionSlot, ParkedForSlot>;

impl crate::Workspace {
    /// Park a peer prompt for `slot`, drained by the session that next
    /// connects as that slot.
    pub(crate) fn park_peer_prompt(
        &self,
        slot: &SessionSlot,
        sender: &SessionSlot,
        wrapped: WrappedPrompt,
    ) {
        self.parked_by_slot
            .lock()
            .entry(slot.clone())
            .or_default()
            .peer
            .push(ParkedPeer { sender: sender.clone(), wrapped });
    }

    /// Park a fired cron prompt for `slot`, missed-marked when it came
    /// due while the owner was asleep. The entry's identity rides along so
    /// the drained echo can name the schedule it came from.
    pub(crate) fn park_cron(
        &self,
        slot: &SessionSlot,
        cron: &forge_primitives::CronEntry,
        missed: bool,
    ) {
        self.parked_by_slot.lock().entry(slot.clone()).or_default().cron.push(
            crate::crons::PendingCron {
                text: cron.prompt.clone(),
                missed,
                cron_id: cron.id.as_str().to_owned(),
                description: cron.description.clone(),
            },
        );
    }

    /// Park a Gotify notification for `slot`.
    pub(crate) fn park_gotify(&self, slot: &SessionSlot, notification: GotifyNotification) {
        self.parked_by_slot.lock().entry(slot.clone()).or_default().gotify.push(notification);
    }

    /// Park a Slack batch for `slot`.
    pub(crate) fn park_slack(
        &self,
        slot: &SessionSlot,
        messages: Vec<forge_primitives::slack::SlackMessage>,
    ) {
        self.parked_by_slot.lock().entry(slot.clone()).or_default().slack.extend(messages);
    }

    /// Take (and clear) everything parked for the connecting session's
    /// slot. The caller passes the triple it was spawned with, rather
    /// than resolving one from its cwd: a cwd under no configured
    /// project would resolve to nothing and leave the payloads parked
    /// forever, silently.
    pub(crate) fn take_parked_for_slot(&self, slot: &SessionSlot) -> ParkedForSlot {
        self.parked_by_slot.lock().remove(slot).unwrap_or_default()
    }

    /// Drop everything parked for `slot`, acknowledging each peer message
    /// back to its sender so a message that never landed is not left
    /// looking delivered. The parked message is the only payload with a
    /// sender, so the Gotify and Slack drops have no one to tell and are
    /// logged instead.
    pub(crate) fn expire_parked_for_slot(
        self: &Arc<Self>,
        slot: &SessionSlot,
        reason: PeerFailureReason,
    ) {
        let Some(parked) = self.parked_by_slot.lock().remove(slot) else { return };
        for entry in parked.peer {
            self.notice_undelivered_message(&entry.sender, slot, reason);
        }
        for notification in parked.gotify {
            tracing::warn!(
                target: "forge_workspace::spawn",
                org = %slot.org(),
                project = %slot.project(),
                label = %slot.label(),
                app = %notification.app,
                title = %notification.title,
                "gotify notification dropped: the spawn it was parked for failed",
            );
        }
        for message in parked.slack {
            tracing::warn!(
                target: "forge_workspace::spawn",
                org = %slot.org(),
                project = %slot.project(),
                label = %slot.label(),
                conversation = %message.conversation,
                ts = %message.ts,
                "slack message dropped: the spawn it was parked for failed",
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ParkedForSlot;
    use crate::SessionSlot;
    use crate::mcp::peers::types::{MessageId, PeerFailureReason, WrappedKind, WrappedPrompt};
    use crate::protocol::SessionUpdate;

    fn slot(label: Option<&str>) -> SessionSlot {
        SessionSlot::for_label("TestOrg", "parked-proj", label)
    }

    fn sender() -> SessionSlot {
        SessionSlot::for_label("TestOrg", "sender-proj", None)
    }

    fn wrapped(id: &MessageId, body: &str) -> WrappedPrompt {
        WrappedPrompt {
            id: id.clone(),
            kind: WrappedKind::Message,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            body: body.to_owned(),
        }
    }

    /// A cron entry carrying what a park reads: the id, the prompt and a
    /// description.
    fn cron(id: &str, prompt: &str) -> forge_primitives::cron::CronEntry {
        use forge_primitives::cron::{CronEntry, CronId, CronKind};
        CronEntry {
            id: CronId::from(id),
            project_name: "parked-proj".to_owned(),
            kind: CronKind::Recurring("0 9 * * *".to_owned()),
            prompt: prompt.to_owned(),
            created_at: std::time::SystemTime::UNIX_EPOCH,
            description: Some(format!("{id} summary")),
            last_fire: None,
            next_fire: std::time::SystemTime::UNIX_EPOCH,
            team_role: None,
        }
    }

    /// A payload parked for a sleeping project is taken by the session
    /// that connects as that slot, and the take is a drain, not a read.
    #[test]
    fn a_parked_payload_lands_on_the_slots_connect() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = MessageId::mint();
        ws.park_peer_prompt(&slot(None), &sender(), wrapped(&id, "are you up?"));

        let taken = ws.take_parked_for_slot(&slot(None));
        assert_eq!(taken.peer.len(), 1, "the slot's own connect takes its bucket");
        assert_eq!(taken.peer[0].wrapped.id, id, "and it is the payload that was parked");
        assert!(
            ws.take_parked_for_slot(&slot(None)).peer.is_empty(),
            "the take drains: a second connect does not re-deliver",
        );
    }

    /// All four parked kinds live in the one bucket, so a cron and a peer
    /// prompt parked for the same slot come back together.
    #[test]
    fn one_bucket_holds_every_kind() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = MessageId::mint();
        ws.park_peer_prompt(&slot(None), &sender(), wrapped(&id, "are you up?"));
        ws.park_cron(&slot(None), &cron("c1", "morning reminder"), true);

        let taken: ParkedForSlot = ws.take_parked_for_slot(&slot(None));
        assert_eq!(taken.peer.len(), 1, "the peer prompt is here");
        assert_eq!(taken.cron.len(), 1, "and so is the cron");
        assert!(taken.cron[0].missed, "with its missed mark intact");
        assert_eq!(taken.cron[0].cron_id, "c1", "and the entry it came from");
        assert_eq!(taken.cron[0].description.as_deref(), Some("c1 summary"));
    }

    /// The label is half the address. A payload parked for a team worker
    /// must not land on the project's lead, which is the silent miss this
    /// bucket shape exists to prevent.
    #[test]
    fn a_workers_parked_payload_is_not_the_leads() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = MessageId::mint();
        ws.park_peer_prompt(&slot(Some("planner")), &sender(), wrapped(&id, "planner"));

        assert!(
            ws.take_parked_for_slot(&slot(None)).peer.is_empty(),
            "the lead does not drain a worker's bucket",
        );
        assert_eq!(
            ws.take_parked_for_slot(&slot(Some("planner"))).peer.len(),
            1,
            "the worker drains its own",
        );
    }

    /// The org is part of the key: a payload parked for one org's project
    /// is not drained by a session under another org that happens to share
    /// the project name.
    #[test]
    fn a_parked_payload_is_not_drained_across_orgs() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = MessageId::mint();
        ws.park_peer_prompt(
            &SessionSlot::lead("OtherOrg", "parked-proj"),
            &sender(),
            wrapped(&id, "elsewhere"),
        );

        assert!(
            ws.take_parked_for_slot(&slot(None)).peer.is_empty(),
            "TestOrg's slot does not take OtherOrg's bucket",
        );
    }

    /// A spawn that never connects expires what it parked: the sender is
    /// told its message never landed, and the bucket does not survive to
    /// leak into a later session.
    ///
    /// This is the delivery-ack path, and it is the whole reason the
    /// parked entry carries a sender - with the ask registry gone, nothing
    /// else knows who to tell.
    #[test]
    fn expiring_a_slot_acknowledges_its_parked_messages_to_the_sender() {
        let (ws, mut rx) = crate::Workspace::testing_stub();
        let id = MessageId::mint();
        ws.park_peer_prompt(&slot(None), &sender(), wrapped(&id, "are you up?"));

        ws.expire_parked_for_slot(&slot(None), PeerFailureReason::TargetConnectionFailed);

        let mut echo = None;
        while let Ok(update) = rx.try_recv() {
            if let SessionUpdate::PeerEnvelopeAppended { key, wrapped, .. } = update {
                echo = Some((key, wrapped));
            }
        }
        let (key, notice) = echo.expect("the notice is painted for the sender");
        assert_eq!(key, sender(), "the notice lands on the sender, not the target");
        assert!(
            matches!(notice.kind, WrappedKind::DeliveryFailureNotice),
            "and it is the delivery failure, not a peer message",
        );
        assert_eq!(
            notice.sender_name, "parked-proj",
            "the notice names the seat that never took the message",
        );
        assert!(
            ws.take_parked_for_slot(&slot(None)).peer.is_empty(),
            "and the bucket is gone, so a later connect cannot deliver it",
        );
    }
}
