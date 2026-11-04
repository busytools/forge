//! The held vault call.
//!
//! A vault operation an agent asks for parks here, in the workspace's own
//! registry, until the person at a client answers it: the blocked MCP handler
//! awaits a oneshot this module hands back, and no operation runs in the vault
//! unless that answer approved it. It sits beside the Slack draft and the
//! hand-offs for the same reason they do - the parking is workspace state,
//! not a session's pending set, so a seat with no domain can still hold one.
//!
//! **The lock state is not stored here.** The draft is parked without it and
//! stamped from the live session on every read, which is what lets a card
//! whose sibling unlocked first re-base to an ordinary approve card.

use uuid::Uuid;

use crate::SessionSlot;
use crate::protocol::{PendingAsk, SessionUpdate};
use crate::workspace::Workspace;
use forge_primitives::vault::{VaultAskAnswer, VaultAskEnding, VaultDraft};

/// One vault call held for a decision: the session that asked, the draft the
/// card draws, and the sender the blocked tool handler awaits.
pub(crate) type ParkedVaultAsk =
    (SessionSlot, VaultDraft, Option<tokio::sync::oneshot::Sender<VaultAskAnswer>>);

impl Workspace {
    /// Hold a vault call and hand back its id plus the receiver the caller
    /// awaits. Addressed to `caller`, so the card surfaces in the session
    /// that will read the reply rather than in whichever session is focused.
    /// `needs_unlock` is the live session state at park time, used only for
    /// the announcement; every later read restamps it.
    pub(crate) fn register_vault_ask(
        &self,
        caller: &SessionSlot,
        draft: VaultDraft,
        needs_unlock: bool,
    ) -> (Uuid, tokio::sync::oneshot::Receiver<VaultAskAnswer>, bool) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let id = draft.id;
        self.parked_vault_asks.lock().insert(id, (caller.clone(), draft.clone(), Some(sender)));
        let stamped = draft.with_unlock_state(needs_unlock);
        let answerable = self
            .update_sender()
            .send_answering(SessionUpdate::VaultAskPending { key: caller.clone(), draft: stamped });
        if !answerable {
            // No subscriber can answer this - either none is attached or none
            // renders a dock - so holding the caller would park it for ever.
            self.parked_vault_asks.lock().remove(&id);
            let (_ignored_sender, dead_receiver) = tokio::sync::oneshot::channel();
            return (id, dead_receiver, false);
        }
        (id, receiver, true)
    }

    /// Whether a vault call with this id is still registered to `caller`,
    /// without removing it - the read the dispatch guard makes first.
    pub(crate) fn vault_ask_waiting(&self, id: Uuid, caller: &SessionSlot) -> bool {
        self.parked_vault_asks.lock().get(&id).is_some_and(|(owner, _, _)| owner == caller)
    }

    /// Answer a held vault call, returning whether one was waiting for THIS
    /// caller. True exactly once, for the entry that was there; a call held
    /// for another session is refused rather than answered.
    pub(crate) fn resolve_vault_ask(
        &self,
        id: Uuid,
        caller: &SessionSlot,
        answer: VaultAskAnswer,
    ) -> bool {
        let mut asks = self.parked_vault_asks.lock();
        let Some((owner, _, sender)) = asks.get_mut(&id) else { return false };
        if owner != caller {
            return false;
        }
        let Some(sender) = sender.take() else { return false };
        drop(asks);
        let _ = sender.send(answer);
        true
    }

    /// Settle an answered or abandoned call: remove the entry and emit the
    /// ending every view reads. Idempotent, because the caller's drop guard
    /// and the answer path both reach it and only the first has an entry.
    pub(crate) fn settle_vault_ask(
        &self,
        id: Uuid,
        caller: &SessionSlot,
        ending: VaultAskEnding,
    ) -> bool {
        let mut asks = self.parked_vault_asks.lock();
        if !asks.get(&id).is_some_and(|(owner, _, _)| owner == caller) {
            return false;
        }
        let Some((owner, _, _)) = asks.remove(&id) else { return false };
        drop(asks);
        let _ =
            self.update_sender().send(SessionUpdate::VaultAskResolved { key: owner, id, ending });
        true
    }

    /// End every vault call `slot` still holds as abandoned.
    ///
    /// **A parked call outlives its own future.** The blocked MCP handler's
    /// future is dropped when the session's task goes, but a detached control
    /// task is not aborted by the client's disconnect, so the registry sweep
    /// is what tells every other view the dock is over.
    pub(crate) fn abandon_vault_asks(&self, slot: &SessionSlot) {
        let ids: Vec<Uuid> = self
            .parked_vault_asks
            .lock()
            .iter()
            .filter(|(_, (owner, _, _))| owner == slot)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.settle_vault_ask(id, slot, VaultAskEnding::Abandoned);
        }
    }

    /// Whether `slot` is holding a parked vault call: its own registry rather
    /// than the session's pending set, so a held call reaches
    /// `session_activity` and every needs mark the lifecycle drives.
    pub(crate) fn has_vault_ask(&self, slot: &SessionSlot) -> bool {
        self.parked_vault_asks.lock().values().any(|(owner, _, _)| owner == slot)
    }

    /// Re-stamp every call still parked after one of them unlocked the
    /// session: each is now an ordinary approve card, and a fold that already
    /// holds that id merges the restamped frame in place instead of ignoring
    /// it. The answered call is already settled and gone from the registry.
    pub(crate) fn rebase_vault_asks(&self) {
        // Sent under the registry lock: a settle that raced this read would
        // otherwise let a restamp overtake its own resolution and re-park a
        // card the reader already answered.
        let asks = self.parked_vault_asks.lock();
        for (owner, draft, _) in asks.values() {
            let _ = self.update_sender().send(SessionUpdate::VaultAskPending {
                key: owner.clone(),
                draft: draft.clone().with_unlock_state(false),
            });
        }
    }

    /// Every held vault call this seat owns, stamped with the live session's
    /// lock state. A `HashMap` keeps no arrival order to offer, the same
    /// exception the settings writes already have.
    pub(crate) fn vault_asks(&self, slot: &SessionSlot, needs_unlock: bool) -> Vec<PendingAsk> {
        self.parked_vault_asks
            .lock()
            .values()
            .filter(|(owner, _, _)| owner == slot)
            .map(|(_, draft, _)| {
                PendingAsk::Vault(Box::new(draft.clone().with_unlock_state(needs_unlock)))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::protocol::Command;
    use forge_primitives::vault::{VaultAskDecision, VaultTool};

    fn caller() -> SessionSlot {
        SessionSlot::lead("Default", "forge")
    }

    fn a_draft() -> VaultDraft {
        VaultDraft::new(VaultTool::Get, "read the password for 'bank'")
    }

    fn stub() -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>) {
        Workspace::testing_stub()
    }

    /// The prompt is addressed to the session that asked, never the lead.
    #[test]
    fn a_call_is_addressed_to_the_session_that_asked() {
        let (ws, mut rx) = stub();
        let asked = SessionSlot::worker("Default", "forge", "vault-worker");
        let _held = ws.register_vault_ask(&asked, a_draft(), false);

        let mut addressed = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let SessionUpdate::VaultAskPending { key, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                addressed.push(key);
            }
        }
        assert_eq!(addressed, vec![asked], "the call is addressed to the worker that asked");
    }

    /// The record read a client takes on reconnect carries the held call, and
    /// the lock state it carries is the one passed to the read, not the one
    /// stamped when it parked.
    #[test]
    fn the_record_read_restamps_the_lock_state() {
        let (ws, _rx) = stub();
        let draft = a_draft();
        let (id, _held, _) = ws.register_vault_ask(&caller(), draft, true);

        let asks = ws.vault_asks(&caller(), false);
        assert!(
            asks.iter().any(|waiting| matches!(
                waiting,
                PendingAsk::Vault(held) if held.id == id && !held.needs_unlock
            )),
            "the live session state wins over the park-time stamp",
        );
        assert!(ws.vault_asks(&SessionSlot::lead("Other", "project"), false).is_empty());
    }

    /// The record read a client takes on reconnect carries the held vault
    /// call, which is what the dock draws; the registry is not a live-only
    /// channel.
    #[test]
    fn the_record_read_reports_the_held_call() {
        let (ws, _rx) = stub();
        let (id, _held, _) = ws.register_vault_ask(&caller(), a_draft(), false);
        assert!(
            ws.pending_asks(&caller())
                .iter()
                .any(|ask| matches!(ask, PendingAsk::Vault(draft) if draft.id == id)),
            "the record read carries the held vault call",
        );
        assert!(ws.pending_asks(&SessionSlot::lead("Other", "project")).is_empty());
    }

    /// Two calls parked on a locked session: when the first unlocks it, the
    /// second's next read is an ordinary approve card and never asks for the
    /// password again.
    #[test]
    fn a_second_parked_call_rebases_when_the_first_unlocks() {
        let (ws, _rx) = stub();
        let (first, _held_first, _) = ws.register_vault_ask(&caller(), a_draft(), true);
        let (second, _held_second, _) = ws.register_vault_ask(&caller(), a_draft(), true);

        let locked = ws.vault_asks(&caller(), true);
        assert_eq!(locked.len(), 2);
        assert!(
            locked.iter().all(|ask| matches!(
                ask,
                PendingAsk::Vault(draft) if draft.needs_unlock
            )),
            "both cards start locked",
        );

        let rebased = ws.vault_asks(&caller(), false);
        assert_eq!(rebased.len(), 2);
        assert!(
            rebased.iter().all(|ask| matches!(
                ask,
                PendingAsk::Vault(draft) if !draft.needs_unlock
            )),
            "the live session state re-bases every other card",
        );
        for id in [first, second] {
            assert!(
                rebased.iter().any(|ask| matches!(
                    ask,
                    PendingAsk::Vault(draft) if draft.id == id
                )),
                "the re-base keeps the call's identity",
            );
        }
    }

    /// One call's unlock pushes a restamped frame for every card still parked,
    /// which is what lets a dock that already drew them stop asking.
    #[test]
    fn unlocking_pushes_a_restamped_frame_for_the_other_cards() {
        let (ws, mut rx) = stub();
        let seat = caller();
        let (answered, _held, _) = ws.register_vault_ask(&seat, a_draft(), true);
        let (other, _other_held, _) = ws.register_vault_ask(&seat, a_draft(), true);
        while rx.try_recv().is_ok() {}

        ws.settle_vault_ask(answered, &seat, VaultAskEnding::Approved);
        while rx.try_recv().is_ok() {}
        ws.rebase_vault_asks();

        let mut seen = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let SessionUpdate::VaultAskPending { draft, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                seen.push(draft);
            }
        }
        assert_eq!(seen.len(), 1, "only the card still parked is pushed");
        assert_eq!(seen[0].id, other);
        assert!(!seen[0].needs_unlock, "the unlock is stamped on the pushed draft");
    }

    /// An approval resolves the waiter and a decline resolves it too; a second
    /// answer for the same id is refused rather than silently taken.
    #[test]
    fn an_answer_resolves_the_waiter_exactly_once() {
        let (ws, _rx) = stub();
        let draft = a_draft();
        let (id, mut held, _) = ws.register_vault_ask(&caller(), draft, false);

        assert!(ws.vault_ask_waiting(id, &caller()));
        let answer = VaultAskAnswer {
            decision: VaultAskDecision::Approve,
            master_password: None,
            two_factor: None,
        };
        assert!(ws.resolve_vault_ask(id, &caller(), answer.clone()));
        assert_eq!(held.try_recv(), Ok(answer), "the approval reaches the blocked caller");
        assert!(
            !ws.resolve_vault_ask(
                id,
                &caller(),
                VaultAskAnswer {
                    decision: VaultAskDecision::Decline,
                    master_password: None,
                    two_factor: None,
                }
            ),
            "one answer only",
        );
        assert!(ws.settle_vault_ask(id, &caller(), VaultAskEnding::Approved));
        assert!(!ws.vault_ask_waiting(id, &caller()));
    }

    /// A call held for this seat is not another seat's to answer.
    #[test]
    fn another_seat_cannot_answer_this_call() {
        let (ws, _rx) = stub();
        let (id, _held, _) = ws.register_vault_ask(&caller(), a_draft(), false);
        let elsewhere = SessionSlot::worker("Default", "forge", "someone-else");

        assert!(!ws.resolve_vault_ask(
            id,
            &elsewhere,
            VaultAskAnswer {
                decision: VaultAskDecision::Approve,
                master_password: None,
                two_factor: None,
            }
        ));
        assert!(ws.vault_ask_waiting(id, &caller()), "the call is still the asker's");
    }

    /// No client to answer: the registry must not hold the call, and the
    /// awaiting caller sees a dead receiver - the fail-closed read.
    #[tokio::test]
    async fn a_call_with_no_ui_to_answer_it_fails_closed() {
        let (ws, rx) = Workspace::testing_stub();
        drop(rx);

        let (_id, held, _) = ws.register_vault_ask(&caller(), a_draft(), false);

        assert!(ws.parked_vault_asks.lock().is_empty(), "an unanswerable call is dropped");
        assert!(held.await.is_err(), "a dead receiver reads as not-approved");
    }

    /// Through `Workspace::dispatch`, the route a view's dock answer takes.
    #[tokio::test]
    async fn the_dock_answer_reaches_the_registry_through_dispatch() {
        let (ws, _rx) = stub();
        let (id, decision, _) = ws.register_vault_ask(&caller(), a_draft(), false);
        let answer = VaultAskAnswer {
            decision: VaultAskDecision::Approve,
            master_password: None,
            two_factor: None,
        };

        ws.dispatch(Command::RespondVaultAsk { key: caller(), id, answer: answer.clone() })
            .expect("the dock answer routes to the workspace");

        assert_eq!(decision.await.expect("the held call answers"), answer);
    }

    /// A click on a dock the core has already resolved is refused.
    #[tokio::test]
    async fn a_stale_dock_answer_is_refused_with_the_reason() {
        let (ws, _rx) = stub();
        let (id, _decision, _) = ws.register_vault_ask(&caller(), a_draft(), false);
        assert!(ws.resolve_vault_ask(
            id,
            &caller(),
            VaultAskAnswer {
                decision: VaultAskDecision::Approve,
                master_password: None,
                two_factor: None,
            }
        ));

        let refused = ws.dispatch(Command::RespondVaultAsk {
            key: caller(),
            id,
            answer: VaultAskAnswer {
                decision: VaultAskDecision::Approve,
                master_password: None,
                two_factor: None,
            },
        });

        assert!(
            matches!(refused, Err(crate::protocol::DispatchError::NoVaultAskWaiting { .. })),
            "a stale dock must report why the answer did not land: {refused:?}",
        );
    }

    /// A session going away ends every call it holds.
    #[test]
    fn a_session_teardown_abandons_the_calls_it_holds() {
        let (ws, mut rx) = Workspace::testing_stub();
        let (id, mut held, _) = ws.register_vault_ask(&caller(), a_draft(), false);

        ws.abandon_vault_asks(&caller());

        assert!(ws.parked_vault_asks.lock().is_empty(), "the teardown leaves nothing parked");
        assert!(held.try_recv().is_err(), "the blocked caller is released as abandoned");
        let mut endings = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let SessionUpdate::VaultAskResolved { id: ended, ending, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                endings.push((ended, ending));
            }
        }
        assert_eq!(endings, vec![(id, VaultAskEnding::Abandoned)]);
    }
}
