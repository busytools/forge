//! The held configuration write.
//!
//! A settings edit an agent asks for parks here, in the workspace's own
//! registry, until the person at a client answers it: the blocked tool call
//! awaits a oneshot this module hands back, and nothing reaches
//! `forge.toml` unless that answer approved it. It sits beside the Slack
//! draft and the hand-offs for the same reason they do - the parking is
//! workspace state, not a session's pending set, so a seat with no domain
//! can still hold one.

use uuid::Uuid;

use crate::SessionSlot;
use crate::protocol::SessionUpdate;
use crate::workspace::Workspace;
use forge_primitives::settings::{SettingsWriteAsk, SettingsWriteEnding};

/// One composed write held for a decision: the session that asked, the ask
/// itself, and the sender the blocked tool handler awaits. The sender is
/// taken at the answer: **the entry stays until the write is settled**, so a
/// second view's answer is refused while the first one applies, and the
/// resolved update carries the write's own outcome rather than the click.
pub(crate) type ParkedSettingsWrite =
    (SessionSlot, SettingsWriteAsk, Option<tokio::sync::oneshot::Sender<bool>>);

impl Workspace {
    /// Hold a composed write and hand back its id plus the receiver the
    /// caller awaits. Addressed to `caller`, so the prompt surfaces in the
    /// session that will read the reply rather than in whichever session
    /// happens to be focused.
    pub(crate) fn register_settings_write(
        &self,
        caller: &SessionSlot,
        ask: SettingsWriteAsk,
    ) -> (Uuid, tokio::sync::oneshot::Receiver<bool>, bool) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let id = ask.id;
        self.settings_writes.lock().insert(id, (caller.clone(), ask.clone(), Some(sender)));
        let answerable = self
            .update_sender()
            .send_answering(SessionUpdate::SettingsWritePending { key: caller.clone(), ask });
        if !answerable {
            // No subscriber can answer this - either none is attached or none
            // renders a dock - so holding the caller would park it for ever.
            // Drop the write; the awaiting caller reads the flag and fails
            // closed.
            self.settings_writes.lock().remove(&id);
            let (_ignored_sender, dead_receiver) = tokio::sync::oneshot::channel();
            return (id, dead_receiver, false);
        }
        // **A second write is a second ask.** The number is the ask's identity
        // on the wire, so a write that never moves it is never announced.
        self.note_ask(caller);
        (id, receiver, true)
    }

    /// Whether a write with this id is still registered to `caller`, without
    /// removing it - the read the dispatch guard makes before an answer.
    pub(crate) fn settings_write_waiting(&self, id: Uuid, caller: &SessionSlot) -> bool {
        self.settings_writes.lock().get(&id).is_some_and(|(owner, _, _)| owner == caller)
    }

    /// Answer a held write with the reader's decision, returning whether one
    /// was waiting for THIS caller. True exactly once, for the entry that was
    /// there; a write held for another session, or one already answered and
    /// still applying, is refused rather than answered.
    ///
    /// **The entry stays until the write is settled** ([`Self::settle_settings_write`]):
    /// the answer unblocks the caller, which applies the edit and then
    /// reports the outcome, so the resolved update says what happened to the
    /// write rather than merely that a button was pressed.
    pub(crate) fn resolve_settings_write(
        &self,
        id: Uuid,
        caller: &SessionSlot,
        approved: bool,
    ) -> bool {
        let mut writes = self.settings_writes.lock();
        let Some((owner, _, sender)) = writes.get_mut(&id) else { return false };
        if owner != caller {
            return false;
        }
        let Some(sender) = sender.take() else { return false };
        drop(writes);
        let _ = sender.send(approved);
        true
    }

    /// Settle an answered or abandoned write: remove the entry and emit the
    /// ending every view reads. Idempotent, because the caller's drop guard
    /// and the answer path both reach it and only the first has an entry.
    pub(crate) fn settle_settings_write(
        &self,
        id: Uuid,
        caller: &SessionSlot,
        ending: SettingsWriteEnding,
    ) -> bool {
        let mut writes = self.settings_writes.lock();
        if !writes.get(&id).is_some_and(|(owner, _, _)| owner == caller) {
            return false;
        }
        let Some((owner, _, _)) = writes.remove(&id) else { return false };
        drop(writes);
        let _ = self.update_sender().send(SessionUpdate::SettingsWriteResolved {
            key: owner,
            id,
            ending,
        });
        true
    }

    /// End every write `slot` still holds as abandoned.
    ///
    /// **A parked write outlives its own future.** The blocked MCP handler's
    /// future is dropped when the session's task goes, but a detached control
    /// task is not aborted by the client's disconnect, so the registry sweep
    /// is what tells every other view the dock is over. Worker labels are
    /// reused, and an entry left behind would draw the previous occupant's
    /// write on the next session's dock.
    pub(crate) fn abandon_settings_writes(&self, slot: &SessionSlot) {
        let ids: Vec<Uuid> = self
            .settings_writes
            .lock()
            .iter()
            .filter(|(_, (owner, _, _))| owner == slot)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.settle_settings_write(id, slot, SettingsWriteEnding::Abandoned);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::protocol::PendingAsk;
    use forge_primitives::settings::SettingsWriteChange;
    use serde_json::json;

    fn caller() -> SessionSlot {
        SessionSlot::lead("Default", "forge")
    }

    fn an_ask(key: &str) -> SettingsWriteAsk {
        SettingsWriteAsk {
            id: Uuid::nil(),
            edits: vec![SettingsWriteChange {
                key: key.to_owned(),
                current: json!(2),
                proposed: json!(4),
                unset: false,
            }],
        }
    }

    /// A stub workspace whose answering subscriber stays alive, which is what
    /// makes a registration answerable at all.
    fn stub() -> (Arc<Workspace>, tokio::sync::mpsc::UnboundedReceiver<crate::SeatUpdate>) {
        Workspace::testing_stub()
    }

    /// The prompt is addressed to the session that asked, never the lead:
    /// the agent blocked on the oneshot is the one the answer belongs to.
    #[test]
    fn a_write_is_addressed_to_the_session_that_asked() {
        let (ws, mut rx) = Workspace::testing_stub();
        let asked = SessionSlot::worker("Default", "forge", "settings-worker");
        let mut ask = an_ask("orgs.Default.projects.forge.max_workers");
        ask.id = Uuid::new_v4();
        let _held = ws.register_settings_write(&asked, ask);

        let mut addressed = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let SessionUpdate::SettingsWritePending { key, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                addressed.push(key);
            }
        }
        assert_eq!(addressed, vec![asked], "the write is addressed to the worker that asked");
    }

    /// The record read a client takes on reconnect carries the held write,
    /// not only the live frame a client that was there happened to see.
    #[test]
    fn the_record_read_reports_the_held_write() {
        let (ws, _rx) = stub();
        let mut ask = an_ask("orgs.Default.projects.forge.max_workers");
        ask.id = Uuid::new_v4();
        let (id, _held, _) = ws.register_settings_write(&caller(), ask);

        let asks = ws.pending_asks(&caller());
        assert!(
            asks.iter().any(|waiting| matches!(
                waiting,
                PendingAsk::SettingsWrite(held) if held.id == id
            )),
            "a client that attached after the ask has no other way to read it",
        );
        assert!(ws.pending_asks(&SessionSlot::lead("Other", "project")).is_empty());
    }

    /// An approval resolves the waiter true and a decline false; a second
    /// answer for the same id is refused rather than silently taken.
    #[test]
    fn an_answer_resolves_the_waiter_exactly_once() {
        let (ws, _rx) = stub();
        let mut ask = an_ask("server.port");
        ask.id = Uuid::new_v4();
        let (id, mut held, _) = ws.register_settings_write(&caller(), ask);

        assert!(ws.settings_write_waiting(id, &caller()));
        assert!(ws.resolve_settings_write(id, &caller(), true));
        assert_eq!(held.try_recv(), Ok(true), "the approval reaches the blocked caller");
        assert!(!ws.resolve_settings_write(id, &caller(), false), "one answer only");
        assert!(
            ws.settings_write_waiting(id, &caller()),
            "the entry stays until the write is settled",
        );
        assert!(ws.settle_settings_write(id, &caller(), SettingsWriteEnding::Applied));
        assert!(!ws.settings_write_waiting(id, &caller()));
    }

    /// A write held for this seat is not another seat's to answer.
    #[test]
    fn another_seat_cannot_answer_this_write() {
        let (ws, _rx) = stub();
        let mut ask = an_ask("server.port");
        ask.id = Uuid::new_v4();
        let (id, _held, _) = ws.register_settings_write(&caller(), ask);

        let elsewhere = SessionSlot::worker("Default", "forge", "someone-else");
        assert!(!ws.resolve_settings_write(id, &elsewhere, true));
        assert!(ws.settings_write_waiting(id, &caller()), "the write is still the asker's");
    }

    /// No client to answer: the registry must not hold the write, and the
    /// awaiting caller sees a dead receiver - the fail-closed read.
    #[tokio::test]
    async fn a_write_with_no_ui_to_answer_it_fails_closed() {
        let (ws, rx) = Workspace::testing_stub();
        drop(rx);
        let mut ask = an_ask("server.port");
        ask.id = Uuid::new_v4();

        let (_id, held, _) = ws.register_settings_write(&caller(), ask);

        assert!(ws.settings_writes.lock().is_empty(), "an unanswerable write is dropped");
        assert!(held.await.is_err(), "a dead receiver reads as not-approved");
    }

    /// Through `Workspace::dispatch`, the route a view's dock answer takes -
    /// the direct `resolve_settings_write` tests above never exercise the
    /// guard or the arm, so deleting either would leave an approved write
    /// from the dock hanging with every test still green.
    #[tokio::test]
    async fn the_dock_answer_reaches_the_registry_through_dispatch() {
        let (ws, _rx) = stub();
        let mut ask = an_ask("server.port");
        ask.id = Uuid::new_v4();
        let (id, decision, _) = ws.register_settings_write(&caller(), ask);

        ws.dispatch(crate::protocol::Command::RespondSettingsWrite {
            key: caller(),
            id,
            approved: true,
        })
        .expect("the dock answer routes to the workspace");

        assert!(
            decision.await.expect("the held write answers"),
            "the answer the dock gave reaches the awaiting caller",
        );
    }

    /// A click on a dock the core has already resolved is refused with the
    /// reason rather than dropped.
    #[tokio::test]
    async fn a_stale_dock_answer_is_refused_with_the_reason() {
        let (ws, _rx) = stub();
        let mut ask = an_ask("server.port");
        ask.id = Uuid::new_v4();
        let (id, _decision, _) = ws.register_settings_write(&caller(), ask);
        assert!(ws.resolve_settings_write(id, &caller(), true));

        let refused = ws.dispatch(crate::protocol::Command::RespondSettingsWrite {
            key: caller(),
            id,
            approved: true,
        });

        assert!(
            matches!(refused, Err(crate::protocol::DispatchError::NoSettingsWriteWaiting { .. })),
            "a stale dock must report why the answer did not land: {refused:?}",
        );
    }

    /// The guard's owner half: another session's dispatch is refused, and
    /// the write stays waiting for the session that asked.
    #[tokio::test]
    async fn another_session_answering_through_dispatch_is_refused() {
        let (ws, _rx) = stub();
        let asker = SessionSlot::worker("Default", "forge", "settings-worker");
        let other = SessionSlot::worker("Default", "forge", "another-worker");
        let mut ask = an_ask("server.port");
        ask.id = Uuid::new_v4();
        let (id, _decision, _) = ws.register_settings_write(&asker, ask);

        let refused = ws.dispatch(crate::protocol::Command::RespondSettingsWrite {
            key: other,
            id,
            approved: true,
        });

        assert!(
            matches!(refused, Err(crate::protocol::DispatchError::NoSettingsWriteWaiting { .. })),
            "{refused:?}",
        );
        assert!(ws.settings_write_waiting(id, &asker), "the write is still the asker's");
    }

    /// **A session going away ends every write it holds.** A detached MCP
    /// control task is not aborted by the client's disconnect, so without
    /// this sweep the entry would outlive the session that could answer it -
    /// and a reused worker label would draw the previous occupant's write on
    /// the next session's dock.
    #[test]
    fn a_session_teardown_abandons_the_writes_it_holds() {
        let (ws, mut rx) = Workspace::testing_stub();
        let mut ask = an_ask("server.port");
        ask.id = Uuid::new_v4();
        let (_id, mut held, _) = ws.register_settings_write(&caller(), ask);

        ws.abandon_settings_writes(&caller());

        assert!(ws.settings_writes.lock().is_empty(), "the teardown leaves nothing parked");
        assert!(held.try_recv().is_err(), "the blocked caller is released as abandoned");
        let mut endings = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let SessionUpdate::SettingsWriteResolved { ending, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                endings.push(ending);
            }
        }
        assert_eq!(endings, vec![SettingsWriteEnding::Abandoned]);
    }

    /// A waiter whose session went away is abandoned rather than approved:
    /// the registry clears it, the blocked caller reads false, and the
    /// resolved update says which ending took it.
    #[test]
    fn an_abandoned_write_is_not_an_approval() {
        let (ws, mut rx) = Workspace::testing_stub();
        let mut ask = an_ask("server.port");
        ask.id = Uuid::new_v4();
        let (id, mut held, _) = ws.register_settings_write(&caller(), ask);

        assert!(ws.settle_settings_write(id, &caller(), SettingsWriteEnding::Abandoned));
        assert!(held.try_recv().is_err(), "abandoned is not an approval");
        assert!(ws.settings_writes.lock().is_empty());

        let mut endings = Vec::new();
        while let Ok(frame) = rx.try_recv() {
            if let SessionUpdate::SettingsWriteResolved { ending, .. } =
                std::sync::Arc::unwrap_or_clone(frame.update)
            {
                endings.push(ending);
            }
        }
        assert_eq!(endings, vec![SettingsWriteEnding::Abandoned]);
    }
}
