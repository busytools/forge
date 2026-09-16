//! Payloads that arrived while their owner had no live session, addressed
//! by `(org, project, label)` - the triple the `sessions` table is keyed
//! by. `None` is the project's lead.

use std::collections::HashMap;
use std::sync::Arc;

use crate::SessionKey;
use crate::mcp::gotify::types::GotifyNotification;
use crate::mcp::peers::types::{PeerFailureReason, WrappedPrompt};

/// Everything parked for one owner, in the order a connecting session
/// delivers it.
#[derive(Default)]
pub(crate) struct ParkedForOwner {
    pub peer: Vec<WrappedPrompt>,
    pub cron: Vec<crate::crons::PendingCron>,
    pub gotify: Vec<GotifyNotification>,
    pub slack: Vec<forge_primitives::slack::SlackMessage>,
}

/// Parked payloads keyed by `(org, project, label)`; `None` is the lead.
pub(crate) type ParkedMap = HashMap<(String, String, Option<String>), ParkedForOwner>;

/// `(org, project, label)`; `None` is the lead.
pub(crate) type Owner = (String, String, Option<String>);

fn owner_key(org: &str, project: &str, label: Option<&str>) -> Owner {
    (org.to_owned(), project.to_owned(), label.map(str::to_owned))
}

impl crate::Workspace {
    /// Park a peer prompt for `owner`, drained by the session that next
    /// connects as that owner.
    pub(crate) fn park_peer_prompt(
        &self,
        org: &str,
        project: &str,
        label: Option<&str>,
        wrapped: WrappedPrompt,
    ) {
        self.parked_by_owner
            .lock()
            .entry(owner_key(org, project, label))
            .or_default()
            .peer
            .push(wrapped);
    }

    /// Park a fired cron prompt for `owner`, missed-marked when it came
    /// due while the owner was asleep.
    pub(crate) fn park_cron(
        &self,
        org: &str,
        project: &str,
        label: Option<&str>,
        text: String,
        missed: bool,
    ) {
        self.parked_by_owner
            .lock()
            .entry(owner_key(org, project, label))
            .or_default()
            .cron
            .push(crate::crons::PendingCron { text, missed });
    }

    /// Park a Gotify notification for `owner`.
    pub(crate) fn park_gotify(
        &self,
        org: &str,
        project: &str,
        label: Option<&str>,
        notification: GotifyNotification,
    ) {
        self.parked_by_owner
            .lock()
            .entry(owner_key(org, project, label))
            .or_default()
            .gotify
            .push(notification);
    }

    /// Park a Slack message for `owner`.
    pub(crate) fn park_slack(
        &self,
        org: &str,
        project: &str,
        label: Option<&str>,
        message: forge_primitives::slack::SlackMessage,
    ) {
        self.parked_by_owner
            .lock()
            .entry(owner_key(org, project, label))
            .or_default()
            .slack
            .push(message);
    }

    /// Take (and clear) everything parked for the connecting session's
    /// owner. The caller passes the triple it was spawned with, rather
    /// than resolving one from its cwd: a cwd under no configured
    /// project would resolve to nothing and leave the payloads parked
    /// forever, silently.
    pub(crate) fn take_parked_for_owner(&self, owner: &Owner) -> ParkedForOwner {
        self.parked_by_owner.lock().remove(owner).unwrap_or_default()
    }

    /// Drop everything parked for a session that never connected. Its
    /// owner is read from its pooled registration, the spawn-time triple.
    pub(crate) fn expire_parked_for_session(
        self: &Arc<Self>,
        session_key: &SessionKey,
        reason: PeerFailureReason,
    ) {
        let Some(owner) = self.owner_for_session_key(session_key) else { return };
        self.expire_parked_owner(&owner, reason);
    }

    /// Drop what is parked for a worker label, resolved from its project
    /// key. The close paths know the label directly, and they run for a
    /// worker that may not be pooled yet - the window a delivery parks in.
    pub(crate) fn expire_parked_for_worker_label(
        self: &Arc<Self>,
        project_key: &crate::ProjectKey,
        label: &str,
        reason: PeerFailureReason,
    ) {
        let Some(project) = self.project_for_key(project_key) else { return };
        self.expire_parked_for_owner(&project.org, &project.name, Some(label), reason);
    }

    /// Drop everything parked for `owner`, failing each peer ask so its
    /// caller gets the delivery-failure notice rather than waiting out the
    /// timeout. A peer ask is the only parked payload with a caller, so the
    /// Gotify and Slack drops have no recipient and are logged.
    pub(crate) fn expire_parked_for_owner(
        self: &Arc<Self>,
        org: &str,
        project: &str,
        label: Option<&str>,
        reason: PeerFailureReason,
    ) {
        self.expire_parked_owner(&owner_key(org, project, label), reason);
    }

    fn expire_parked_owner(self: &Arc<Self>, owner: &Owner, reason: PeerFailureReason) {
        let Some(parked) = self.parked_by_owner.lock().remove(owner) else { return };
        for wrapped in parked.peer {
            self.expire_inflight_ask_failed(&wrapped.correlation_id, reason);
        }
        for notification in parked.gotify {
            tracing::warn!(
                target: "forge_workspace::spawn",
                org = %owner.0,
                project = %owner.1,
                app = %notification.app,
                title = %notification.title,
                "gotify notification dropped: the spawn it was parked for failed",
            );
        }
        for message in parked.slack {
            tracing::warn!(
                target: "forge_workspace::spawn",
                org = %owner.0,
                project = %owner.1,
                conversation = %message.conversation,
                ts = %message.ts,
                "slack message dropped: the spawn it was parked for failed",
            );
        }
    }

    /// The `(org, project, label)` a live session acts for, read from the
    /// registration its spawn stamped. That triple is fixed at spawn, so
    /// it is the same one the store and the parking paths key on. `None`
    /// for a key this process holds no registration for.
    pub(crate) fn owner_for_session_key(&self, session_key: &SessionKey) -> Option<Owner> {
        let registration = {
            let pool = self.pool.lock();
            pool.get(session_key).and_then(|entry| entry.registration.as_ref()).cloned()?
        };
        Some((registration.org, registration.project, self.worker_label_for_session(session_key)))
    }
}

#[cfg(test)]
mod tests {
    use super::{Owner, ParkedForOwner};
    use crate::SessionKey;
    use crate::mcp::peers::types::{
        AskChannel, CorrelationId, InflightAsk, PeerFailureReason, WrappedKind, WrappedPrompt,
    };

    fn owner(label: Option<&str>) -> Owner {
        ("TestOrg".to_owned(), "parked-proj".to_owned(), label.map(str::to_owned))
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
    /// that connects as that owner, and the take is a drain, not a read.
    #[test]
    fn a_parked_payload_lands_on_the_owners_connect() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = CorrelationId::new_ask();
        ws.park_peer_prompt("TestOrg", "parked-proj", None, wrapped(&id, "are you up?"));

        let taken = ws.take_parked_for_owner(&owner(None));
        assert_eq!(taken.peer.len(), 1, "the owner's connect takes its own bucket");
        assert_eq!(taken.peer[0].correlation_id, id, "and it is the payload that was parked");
        assert!(
            ws.take_parked_for_owner(&owner(None)).peer.is_empty(),
            "the take drains: a second connect does not re-deliver",
        );
    }

    /// All four parked kinds live in the one bucket, so a cron and a peer
    /// prompt parked for the same owner come back together.
    #[test]
    fn one_bucket_holds_every_kind() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = CorrelationId::new_ask();
        ws.park_peer_prompt("TestOrg", "parked-proj", None, wrapped(&id, "are you up?"));
        ws.park_cron("TestOrg", "parked-proj", None, "morning reminder".to_owned(), true);

        let taken: ParkedForOwner = ws.take_parked_for_owner(&owner(None));
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
        ws.park_peer_prompt("TestOrg", "parked-proj", Some("planner"), wrapped(&id, "planner"));

        assert!(
            ws.take_parked_for_owner(&owner(None)).peer.is_empty(),
            "the lead does not drain a worker's bucket",
        );
        assert_eq!(
            ws.take_parked_for_owner(&owner(Some("planner"))).peer.len(),
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
        ws.park_peer_prompt("OtherOrg", "parked-proj", None, wrapped(&id, "elsewhere"));

        assert!(
            ws.take_parked_for_owner(&owner(None)).peer.is_empty(),
            "TestOrg's owner does not take OtherOrg's bucket",
        );
    }

    /// A spawn that never connects expires what it parked: the peer ask is
    /// failed so its caller gets a delivery-failure notice instead of
    /// waiting out the timeout, and the bucket does not survive to leak
    /// into a later session.
    #[test]
    fn expiring_an_owner_fails_its_parked_asks() {
        let (ws, _rx) = crate::Workspace::testing_stub();
        let id = CorrelationId::new_ask();
        ws.park_peer_prompt("TestOrg", "parked-proj", None, wrapped(&id, "are you up?"));
        ws.inflight_asks.lock().insert(
            id.clone(),
            InflightAsk {
                correlation_id: id.clone(),
                channel: AskChannel::Peers,
                caller: SessionKey::from_str_for_test("asker"),
                target_project: "parked-proj".to_owned(),
                target_session: None,
            },
        );

        ws.expire_parked_for_owner(
            "TestOrg",
            "parked-proj",
            None,
            PeerFailureReason::TargetConnectionFailed,
        );

        assert!(
            !ws.inflight_asks.lock().contains_key(&id),
            "the undelivered ask is failed, not left waiting",
        );
        assert!(
            ws.take_parked_for_owner(&owner(None)).peer.is_empty(),
            "and the bucket is gone, so a later connect cannot deliver it",
        );
    }
}
