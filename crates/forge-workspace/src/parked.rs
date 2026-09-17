//! Payloads that arrived while their slot had no live session, addressed
//! by `(org, project, label)` - the triple the `sessions` table is keyed
//! by. `None` is the project's lead.

use std::collections::HashMap;
use std::sync::Arc;

use crate::SessionSlot;
use crate::mcp::gotify::types::GotifyNotification;
use crate::mcp::peers::types::{PeerFailureReason, WrappedPrompt};

/// Everything parked for one slot, in the order a connecting session
/// delivers it.
#[derive(Default)]
pub(crate) struct ParkedForSlot {
    pub peer: Vec<WrappedPrompt>,
    pub cron: Vec<crate::crons::PendingCron>,
    pub gotify: Vec<GotifyNotification>,
    pub slack: Vec<forge_primitives::slack::SlackMessage>,
}

/// The slot a payload is addressed to: the `(org, project, label)`
/// triple the `sessions` table is keyed by, with `label` `None` for the
/// project's lead. One value rather than three adjacent strings, so a
/// park and the drain that fills it cannot be transposed.
#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct Slot {
    pub org: String,
    pub project: String,
    pub label: Option<String>,
}

impl Slot {
    pub(crate) fn new(org: &str, project: &str, label: Option<String>) -> Self {
        Self { org: org.to_owned(), project: project.to_owned(), label }
    }

    /// A worker's slot: its project plus the label that names it.
    pub(crate) fn worker(org: &str, project: &str, label: &str) -> Self {
        Self::new(org, project, Some(label.to_owned()))
    }

    /// A project's lead slot.
    pub(crate) fn lead(org: &str, project: &str) -> Self {
        Self::new(org, project, None)
    }
}

/// Parked payloads, one bucket per slot.
pub(crate) type ParkedMap = HashMap<Slot, ParkedForSlot>;

impl crate::Workspace {
    /// Park a peer prompt for `slot`, drained by the session that next
    /// connects as that slot.
    pub(crate) fn park_peer_prompt(&self, slot: &Slot, wrapped: WrappedPrompt) {
        self.parked_by_slot.lock().entry(slot.clone()).or_default().peer.push(wrapped);
    }

    /// Park a fired cron prompt for `slot`, missed-marked when it came
    /// due while the owner was asleep.
    pub(crate) fn park_cron(&self, slot: &Slot, text: String, missed: bool) {
        self.parked_by_slot
            .lock()
            .entry(slot.clone())
            .or_default()
            .cron
            .push(crate::crons::PendingCron { text, missed });
    }

    /// Park a Gotify notification for `slot`.
    pub(crate) fn park_gotify(&self, slot: &Slot, notification: GotifyNotification) {
        self.parked_by_slot.lock().entry(slot.clone()).or_default().gotify.push(notification);
    }

    /// Park a Slack message for `slot`.
    pub(crate) fn park_slack(&self, slot: &Slot, message: forge_primitives::slack::SlackMessage) {
        self.parked_by_slot.lock().entry(slot.clone()).or_default().slack.push(message);
    }

    /// Take (and clear) everything parked for the connecting session's
    /// slot. The caller passes the triple it was spawned with, rather
    /// than resolving one from its cwd: a cwd under no configured
    /// project would resolve to nothing and leave the payloads parked
    /// forever, silently.
    pub(crate) fn take_parked_for_slot(&self, slot: &Slot) -> ParkedForSlot {
        self.parked_by_slot.lock().remove(slot).unwrap_or_default()
    }

    /// Drop everything parked for a session that never connected. Its
    /// slot is read from its pooled registration, the spawn-time triple.
    pub(crate) fn expire_parked_for_session(
        self: &Arc<Self>,
        session_key: &SessionSlot,
        reason: PeerFailureReason,
    ) {
        let Some(slot) = self.slot_for_session_key(session_key) else { return };
        self.expire_parked_for_slot(&slot, reason);
    }

    /// Expire what a worker's close left parked, by the rule its
    /// delivery parked under.
    ///
    /// The pool arm is a miss at this call site: the close released the
    /// session before it got here, which is why the label-based fallback
    /// is the one that runs. It is kept so the two callers cannot drift -
    /// the rule is one function, not two implementations of a comment.
    pub(crate) fn expire_parked_for_worker(
        self: &Arc<Self>,
        project_key: &crate::ProjectKey,
        label: &str,
        session_key: &SessionSlot,
        reason: PeerFailureReason,
    ) {
        let project = self.project_for_key(project_key);
        let project = project.as_ref().map(|p| (p.org.clone(), p.name.clone()));
        let Some(slot) = self.worker_slot_or(session_key, project, label) else { return };
        self.expire_parked_for_slot(&slot, reason);
    }

    /// The slot a delivery to a worker addresses: the record its spawn
    /// wrote while the pool holds it, else its label under `project`.
    pub(crate) fn delivery_slot_for_worker(
        &self,
        session_key: &SessionSlot,
        project: Option<(String, String)>,
        label: &str,
    ) -> Option<Slot> {
        self.worker_slot_or(session_key, project, label)
    }

    /// The one implementation of the worker address rule: the slot the
    /// pool holds for this session, else the label under the project the
    /// caller resolved. Both the park and the expiry call it, so a
    /// payload cannot be parked under one slot and expired at another.
    ///
    /// `None` when neither names the project - a project dropped from
    /// `forge.toml` since the worker spawned, with no pooled entry left.
    fn worker_slot_or(
        &self,
        session_key: &SessionSlot,
        project: Option<(String, String)>,
        label: &str,
    ) -> Option<Slot> {
        if let Some(slot) = self.slot_for_session_key(session_key) {
            return Some(slot);
        }
        let (org, name) = project?;
        Some(Slot::worker(&org, &name, label))
    }

    /// Drop everything parked for `slot`, failing each peer ask so its
    /// caller gets the delivery-failure notice rather than waiting out the
    /// timeout. A peer ask is the only parked payload with a caller, so the
    /// Gotify and Slack drops have no recipient and are logged.
    pub(crate) fn expire_parked_for_slot(self: &Arc<Self>, slot: &Slot, reason: PeerFailureReason) {
        let Some(parked) = self.parked_by_slot.lock().remove(slot) else { return };
        for wrapped in parked.peer {
            self.expire_inflight_ask_failed(&wrapped.correlation_id, reason);
        }
        for notification in parked.gotify {
            tracing::warn!(
                target: "forge_workspace::spawn",
                org = %slot.org,
                project = %slot.project,
                label = ?slot.label,
                app = %notification.app,
                title = %notification.title,
                "gotify notification dropped: the spawn it was parked for failed",
            );
        }
        for message in parked.slack {
            tracing::warn!(
                target: "forge_workspace::spawn",
                org = %slot.org,
                project = %slot.project,
                label = ?slot.label,
                conversation = %message.conversation,
                ts = %message.ts,
                "slack message dropped: the spawn it was parked for failed",
            );
        }
    }

    /// The slot a live session fills, read from the record its spawn
    /// wrote. `None` for a key this process has no pooled entry for.
    ///
    /// Read rather than derived: a derivation from the live-worker
    /// registry answers with the lead's slot for a worker whose entry is
    /// gone, and this feeds the expiry that has to reach the worker's OWN
    /// bucket.
    pub(crate) fn slot_for_session_key(&self, session_key: &SessionSlot) -> Option<Slot> {
        Some(self.pool.lock().get(session_key)?.slot.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::{ParkedForSlot, Slot};
    use crate::SessionSlot;
    use crate::mcp::peers::types::{
        AskChannel, CorrelationId, InflightAsk, PeerFailureReason, WrappedKind, WrappedPrompt,
    };

    fn slot(label: Option<&str>) -> Slot {
        Slot::new("TestOrg", "parked-proj", label.map(str::to_owned))
    }

    fn wrapped(correlation_id: &CorrelationId, body: &str) -> WrappedPrompt {
        WrappedPrompt {
            correlation_id: correlation_id.clone(),
            kind: WrappedKind::Question,
            channel: AskChannel::Peers,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            body: body.to_owned(),
        }
    }

    /// A payload parked for a sleeping project is taken by the session
    /// that connects as that slot, and the take is a drain, not a read.
    #[test]
    fn a_parked_payload_lands_on_the_slots_connect() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = CorrelationId::new_ask();
        ws.park_peer_prompt(&slot(None), wrapped(&id, "are you up?"));

        let taken = ws.take_parked_for_slot(&slot(None));
        assert_eq!(taken.peer.len(), 1, "the slot's own connect takes its bucket");
        assert_eq!(taken.peer[0].correlation_id, id, "and it is the payload that was parked");
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
        let id = CorrelationId::new_ask();
        ws.park_peer_prompt(&slot(None), wrapped(&id, "are you up?"));
        ws.park_cron(&slot(None), "morning reminder".to_owned(), true);

        let taken: ParkedForSlot = ws.take_parked_for_slot(&slot(None));
        assert_eq!(taken.peer.len(), 1, "the peer prompt is here");
        assert_eq!(taken.cron.len(), 1, "and so is the cron");
        assert!(taken.cron[0].missed, "with its missed mark intact");
    }

    /// The label is half the address. A payload parked for a team worker
    /// must not land on the project's lead, which is the silent miss this
    /// bucket shape exists to prevent.
    #[test]
    fn a_workers_parked_payload_is_not_the_leads() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = CorrelationId::new_ask();
        ws.park_peer_prompt(&slot(Some("planner")), wrapped(&id, "planner"));

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
        let id = CorrelationId::new_ask();
        ws.park_peer_prompt(&Slot::new("OtherOrg", "parked-proj", None), wrapped(&id, "elsewhere"));

        assert!(
            ws.take_parked_for_slot(&slot(None)).peer.is_empty(),
            "TestOrg's slot does not take OtherOrg's bucket",
        );
    }

    /// A spawn that never connects expires what it parked: the peer ask is
    /// failed so its caller gets a delivery-failure notice instead of
    /// waiting out the timeout, and the bucket does not survive to leak
    /// into a later session.
    #[test]
    fn expiring_a_slot_fails_its_parked_asks() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = CorrelationId::new_ask();
        ws.park_peer_prompt(&slot(None), wrapped(&id, "are you up?"));
        ws.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: AskChannel::Peers,
                caller: SessionSlot::from_str_for_test("asker"),
                target_project: "parked-proj".to_owned(),
                target_session: None,
            },
        );

        ws.expire_parked_for_slot(&slot(None), PeerFailureReason::TargetConnectionFailed);

        assert!(
            !ws.inflight_asks.lock().contains_key(&id),
            "the undelivered ask is failed, not left waiting",
        );
        assert!(
            ws.take_parked_for_slot(&slot(None)).peer.is_empty(),
            "and the bucket is gone, so a later connect cannot deliver it",
        );
    }
}
