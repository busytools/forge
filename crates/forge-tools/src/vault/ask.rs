//! The held vault call.
//!
//! A vault operation an agent asks for parks here, in the vault family's own
//! registry, until the person at a client answers it: the blocked MCP handler
//! awaits a oneshot this module hands back, and no operation runs in the vault
//! unless that answer approved it.
//!
//! The registry is both halves of the ask seam. It is the [`AskSource`] the
//! host reads for its needs-you mark and its dock, and it reports every
//! arrival and settlement through the [`AskSink`] the host handed in, so the
//! host numbers the ask without knowing what it is. What the ask is, and what
//! answering it means, stay here.
//!
//! **The lock state is not stored here.** The draft is parked without it and
//! stamped from the live session on every read and every re-base, which is
//! what lets a card whose sibling unlocked first re-base to an ordinary
//! approve card.

use std::collections::HashMap;
use std::sync::Arc;

use forge_primitives::vault::{VaultAskAnswer, VaultAskEnding, VaultDraft};
use forge_primitives::{Ask, AskSink, AskSource, PendingAsk, PendingInteractionKind, SessionSlot};
use parking_lot::{Mutex, RwLock};
use uuid::Uuid;

use crate::vault::{VaultEmit, VaultSession, VaultUpdate};

/// One vault call held for a decision: who asked, the draft the card draws,
/// and the sender the blocked tool handler awaits.
struct ParkedVaultAsk {
    seat: SessionSlot,
    draft: VaultDraft,
    sender: Option<tokio::sync::oneshot::Sender<VaultAskAnswer>>,
}

/// Every vault call currently held, in the registry the host reads.
pub struct VaultAsks {
    parked: Mutex<HashMap<Uuid, ParkedVaultAsk>>,
    /// The live session handle the host swaps on a `[vault]` reload; a read
    /// stamps a draft from it rather than from the park-time state.
    session: Arc<RwLock<Option<Arc<VaultSession>>>>,
    emit: Arc<dyn VaultEmit>,
    sink: Arc<dyn AskSink>,
}

impl VaultAsks {
    pub fn new(
        session: Arc<RwLock<Option<Arc<VaultSession>>>>,
        emit: Arc<dyn VaultEmit>,
        sink: Arc<dyn AskSink>,
    ) -> Self {
        Self { parked: Mutex::new(HashMap::new()), session, emit, sink }
    }

    /// Hold a vault call and hand back its id plus the receiver the caller
    /// awaits. Addressed to `caller`, so the card surfaces in the session
    /// that will read the reply rather than in whichever session is focused.
    /// `needs_unlock` is the live session state at park time, used only for
    /// the announcement; every later read restamps it.
    pub fn register(
        &self,
        caller: &SessionSlot,
        draft: VaultDraft,
        needs_unlock: bool,
    ) -> (Uuid, tokio::sync::oneshot::Receiver<VaultAskAnswer>, bool) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let id = draft.id;
        self.parked.lock().insert(
            id,
            ParkedVaultAsk { seat: caller.clone(), draft: draft.clone(), sender: Some(sender) },
        );
        let stamped = draft.with_unlock_state(needs_unlock);
        let answerable =
            self.emit.emit(VaultUpdate::AskPending { key: caller.clone(), draft: stamped });
        if !answerable {
            // No subscriber can answer this - either none is attached or none
            // renders a dock - so holding the caller would park it for ever.
            self.parked.lock().remove(&id);
            let (_ignored_sender, dead_receiver) = tokio::sync::oneshot::channel();
            return (id, dead_receiver, false);
        }
        self.sink.note(caller, PendingInteractionKind::Vault);
        (id, receiver, true)
    }

    /// Whether a vault call with this id is still registered to `caller`,
    /// without removing it - the read the dispatch guard makes first.
    pub fn is_waiting(&self, id: Uuid, caller: &SessionSlot) -> bool {
        self.parked.lock().get(&id).is_some_and(|held| &held.seat == caller)
    }

    /// Answer a held vault call, returning whether one was waiting for THIS
    /// caller. True exactly once, for the entry that was there; a call held
    /// for another session is refused rather than answered.
    pub fn resolve(&self, id: Uuid, caller: &SessionSlot, answer: VaultAskAnswer) -> bool {
        let mut asks = self.parked.lock();
        let Some(held) = asks.get_mut(&id) else { return false };
        if &held.seat != caller {
            return false;
        }
        let Some(sender) = held.sender.take() else { return false };
        drop(asks);
        let _ = sender.send(answer);
        true
    }

    /// Settle an answered or abandoned call: remove the entry and emit the
    /// ending every view reads. Idempotent, because the caller's drop guard
    /// and the answer path both reach it and only the first has an entry.
    pub fn settle(&self, id: Uuid, caller: &SessionSlot, ending: VaultAskEnding) -> bool {
        let mut asks = self.parked.lock();
        if !asks.get(&id).is_some_and(|held| &held.seat == caller) {
            return false;
        }
        let Some(held) = asks.remove(&id) else { return false };
        drop(asks);
        let seat = held.seat;
        let _ = self.emit.emit(VaultUpdate::AskResolved { key: seat.clone(), id, ending });
        self.sink.resolve(&seat, id);
        true
    }

    /// End every vault call `slot` still holds as abandoned.
    ///
    /// **A parked call outlives its own future.** The blocked MCP handler's
    /// future is dropped when the session's task goes, but a detached control
    /// task is not aborted by the client's disconnect, so the registry sweep
    /// is what tells every other view the dock is over.
    pub fn abandon(&self, slot: &SessionSlot) {
        let ids: Vec<Uuid> = self
            .parked
            .lock()
            .iter()
            .filter(|(_, held)| &held.seat == slot)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            self.settle(id, slot, VaultAskEnding::Abandoned);
        }
    }

    /// Re-stamp every call still parked after the shared session's lock state
    /// changed: each is now an ordinary approve card when `needs_unlock` is
    /// false, and asks for the password again when it is true. A fold that
    /// already holds that id merges the restamped frame in place instead of
    /// ignoring it. The answered call is already settled and gone from the
    /// registry.
    pub fn rebase(&self, needs_unlock: bool) {
        // Sent under the registry lock: a settle that raced this read would
        // otherwise let a restamp overtake its own resolution and re-park a
        // card the reader already answered.
        let asks = self.parked.lock();
        for held in asks.values() {
            let _ = self.emit.emit(VaultUpdate::AskPending {
                key: held.seat.clone(),
                draft: held.draft.clone().with_unlock_state(needs_unlock),
            });
        }
    }

    /// List a call as held without a sender, for a host's fixture: the ask
    /// is drawn and counted, and answering it is the test's own business.
    #[cfg(any(test, feature = "testing"))]
    pub fn seed(&self, seat: &SessionSlot, draft: VaultDraft) {
        self.parked
            .lock()
            .insert(draft.id, ParkedVaultAsk { seat: seat.clone(), draft, sender: None });
    }

    /// Whether the live session has locked.
    fn needs_unlock(&self) -> bool {
        self.session.read().as_ref().is_some_and(|session| session.is_locked())
    }

    /// Every held vault call this seat owns, stamped with the live session's
    /// lock state. A `HashMap` keeps no arrival order to offer, the same
    /// exception the settings writes already have.
    fn held_asks(&self, slot: &SessionSlot) -> Vec<Ask> {
        let needs_unlock = self.needs_unlock();
        self.parked
            .lock()
            .values()
            .filter(|held| &held.seat == slot)
            .map(|held| Ask {
                id: held.draft.id,
                kind: PendingInteractionKind::Vault,
                payload: PendingAsk::Vault(Box::new(
                    held.draft.clone().with_unlock_state(needs_unlock),
                )),
            })
            .collect()
    }
}

impl AskSource for VaultAsks {
    fn asks(&self, seat: &SessionSlot) -> Vec<Ask> {
        self.held_asks(seat)
    }

    fn held(&self, seat: &SessionSlot) -> Vec<(PendingInteractionKind, usize)> {
        // Counts only: the lock state is needed to stamp a card a view draws,
        // not to count one, so the needs-you read never touches the session.
        let count = self.parked.lock().values().filter(|held| &held.seat == seat).count();
        vec![(PendingInteractionKind::Vault, count)]
    }

    fn kinds(&self) -> &'static [PendingInteractionKind] {
        const KINDS: &[PendingInteractionKind] = &[PendingInteractionKind::Vault];
        KINDS
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::VaultConfig;
    use crate::vault::crypto;
    use crate::vault::session::{
        TestEmit, test_install_unlocked, test_keys_present, test_session, test_set_last_used,
    };
    use forge_primitives::vault::{VaultAskDecision, VaultTool};

    #[derive(Default)]
    struct TestSink {
        noted: Mutex<Vec<(SessionSlot, PendingInteractionKind)>>,
        resolved: Mutex<Vec<(SessionSlot, Uuid)>>,
    }

    impl AskSink for TestSink {
        fn note(&self, seat: &SessionSlot, kind: PendingInteractionKind) {
            self.noted.lock().push((seat.clone(), kind));
        }

        fn resolve(&self, seat: &SessionSlot, id: Uuid) {
            self.resolved.lock().push((seat.clone(), id));
        }
    }

    fn caller() -> SessionSlot {
        SessionSlot::lead("Default", "forge")
    }

    fn a_draft() -> VaultDraft {
        VaultDraft::new(VaultTool::Get, "read the password for 'bank'")
    }

    fn a_session() -> Arc<VaultSession> {
        let config = VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "hi@vedhavyas.dev".to_owned(),
            idle_minutes: None,
        };
        Arc::new(test_session(&config, None, "test").expect("a session"))
    }

    fn unlocked(session: &VaultSession) {
        test_install_unlocked(
            session,
            crypto::Unlocked {
                user_key: crypto::make_symmetric_key(),
                private_key: None,
                org_keys: std::collections::HashMap::new(),
                failed_orgs: Vec::new(),
                user_key_fingerprint: String::new(),
            },
        );
    }

    fn harness() -> (Arc<VaultAsks>, Arc<TestEmit>, Arc<TestSink>) {
        harness_with(std::sync::Arc::new(TestEmit::default()), None)
    }

    fn harness_with(
        emit: Arc<TestEmit>,
        session: Option<Arc<VaultSession>>,
    ) -> (Arc<VaultAsks>, Arc<TestEmit>, Arc<TestSink>) {
        let sink = Arc::new(TestSink::default());
        let handle = Arc::new(RwLock::new(session));
        let asks = Arc::new(VaultAsks::new(handle, emit.clone(), sink.clone()));
        (asks, emit, sink)
    }

    /// The prompt is addressed to the session that asked, never the lead,
    /// and the ask is reported to the host's own numbering.
    #[test]
    fn a_call_is_addressed_to_the_session_that_asked() {
        let (asks, emit, sink) = harness();
        let asked = SessionSlot::worker("Default", "forge", "vault-worker");
        let _held = asks.register(&asked, a_draft(), false);

        let announced = emit.take();
        match announced.first().expect("the card is announced") {
            VaultUpdate::AskPending { key, .. } => {
                assert_eq!(key, &asked, "the call is addressed to the worker that asked");
            }
            other => panic!("the registration announced something else: {other:?}"),
        }
        assert_eq!(
            sink.noted.lock().as_slice(),
            [(asked.clone(), PendingInteractionKind::Vault)],
            "the host is told one vault ask arrived",
        );
    }

    /// **A needs-you read does not touch the session.** The ledger walks
    /// `AskSource::held`, which counts what is parked; `asks` builds the
    /// payloads and stamps each from the live session, and
    /// `VaultSession::is_locked` runs the idle check - so a
    /// `pending_interaction` read on a long-idle unlock would lock the vault
    /// just by asking whether anything was waiting.
    ///
    /// Mutant: make `held` delegate to `held_asks` and this reddens.
    #[test]
    fn the_held_read_does_not_lock_an_idle_session() {
        let config = VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "hi@vedhavyas.dev".to_owned(),
            idle_minutes: Some(1),
        };
        let session = Arc::new(test_session(&config, None, "test").expect("a session"));
        unlocked(&session);
        test_set_last_used(
            &session,
            std::time::SystemTime::now() - std::time::Duration::from_secs(120),
        );
        let (asks, _emit, _sink) =
            harness_with(std::sync::Arc::new(TestEmit::default()), Some(Arc::clone(&session)));
        let seat = caller();
        let _held = asks.register(&seat, a_draft(), false);

        assert_eq!(
            AskSource::held(asks.as_ref(), &seat),
            [(PendingInteractionKind::Vault, 1)],
            "the count sees the parked call",
        );
        assert!(
            test_keys_present(&session),
            "a needs-you count must not run the idle check and lock the vault",
        );

        let _ = AskSource::asks(asks.as_ref(), &seat);
        assert!(
            !test_keys_present(&session),
            "and the payload read is what stamps the card and runs the check",
        );
    }

    /// The record read a client takes on reconnect carries the held call, and
    /// the lock state it carries is the live session's, not the one stamped
    /// when it parked.
    #[test]
    fn the_record_read_restamps_the_lock_state() {
        let session = a_session();
        let (asks, _emit, _sink) =
            harness_with(std::sync::Arc::new(TestEmit::default()), Some(Arc::clone(&session)));
        let draft = a_draft();
        let (id, _held, _) = asks.register(&caller(), draft, true);

        unlocked(&session);
        let held = asks.asks(&caller());
        assert!(
            held.iter().any(|ask| {
                matches!(&ask.payload, PendingAsk::Vault(draft) if draft.id == id && !draft.needs_unlock)
            }),
            "the live session state wins over the park-time stamp",
        );
        assert!(asks.asks(&SessionSlot::lead("Other", "project")).is_empty());
    }

    /// The record read a client takes on reconnect carries the held vault
    /// call, which is what the dock draws; the registry is not a live-only
    /// channel.
    #[test]
    fn the_record_read_reports_the_held_call() {
        let (asks, _emit, _sink) = harness();
        let (id, _held, _) = asks.register(&caller(), a_draft(), false);
        assert!(
            asks.asks(&caller())
                .iter()
                .any(|ask| matches!(&ask.payload, PendingAsk::Vault(draft) if draft.id == id)),
            "the record read carries the held vault call",
        );
        assert!(asks.asks(&SessionSlot::lead("Other", "project")).is_empty());
    }

    /// Two calls parked on a locked session: when the first unlocks it, the
    /// second's next read is an ordinary approve card and never asks for the
    /// password again.
    #[test]
    fn a_second_parked_call_rebases_when_the_first_unlocks() {
        let session = a_session();
        let (asks, _emit, _sink) =
            harness_with(std::sync::Arc::new(TestEmit::default()), Some(Arc::clone(&session)));
        let (first, _held_first, _) = asks.register(&caller(), a_draft(), true);
        let (second, _held_second, _) = asks.register(&caller(), a_draft(), true);

        let locked = asks.asks(&caller());
        assert_eq!(locked.len(), 2);
        assert!(
            locked
                .iter()
                .all(|ask| matches!(&ask.payload, PendingAsk::Vault(draft) if draft.needs_unlock)),
            "both cards start locked",
        );

        unlocked(&session);
        let rebased = asks.asks(&caller());
        assert_eq!(rebased.len(), 2);
        assert!(
            rebased
                .iter()
                .all(|ask| matches!(&ask.payload, PendingAsk::Vault(draft) if !draft.needs_unlock)),
            "the live session state re-bases every other card",
        );
        for id in [first, second] {
            assert!(
                rebased.iter().any(|ask| {
                    matches!(&ask.payload, PendingAsk::Vault(draft) if draft.id == id)
                }),
                "the re-base keeps the call's identity",
            );
        }
    }

    /// One call's unlock pushes a restamped frame for every card still parked,
    /// which is what lets a dock that already drew them stop asking.
    #[test]
    fn unlocking_pushes_a_restamped_frame_for_the_other_cards() {
        let (asks, emit, _sink) = harness();
        let seat = caller();
        let (answered, _held, _) = asks.register(&seat, a_draft(), true);
        let (other, _other_held, _) = asks.register(&seat, a_draft(), true);
        emit.take();

        asks.settle(answered, &seat, VaultAskEnding::Approved);
        emit.take();
        asks.rebase(false);

        let mut seen = Vec::new();
        for update in emit.take() {
            if let VaultUpdate::AskPending { draft, .. } = update {
                seen.push(draft);
            }
        }
        assert_eq!(seen.len(), 1, "only the card still parked is pushed");
        assert_eq!(seen[0].id, other);
        assert!(!seen[0].needs_unlock, "the unlock is stamped on the pushed draft");
    }

    /// A re-lock stamps every parked card as needing the password again, which
    /// is what makes a refresh failure reach the cards still waiting.
    #[test]
    fn a_relock_pushes_a_restamped_frame() {
        let (asks, emit, _sink) = harness();
        let (id, _held, _) = asks.register(&caller(), a_draft(), false);
        emit.take();

        asks.rebase(true);

        let mut seen = Vec::new();
        for update in emit.take() {
            if let VaultUpdate::AskPending { draft, .. } = update {
                seen.push(draft);
            }
        }
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].id, id);
        assert!(seen[0].needs_unlock, "the re-lock is stamped on the pushed draft");
    }

    /// An approval resolves the waiter and a decline resolves it too; a second
    /// answer for the same id is refused rather than silently taken.
    #[test]
    fn an_answer_resolves_the_waiter_exactly_once() {
        let (asks, _emit, _sink) = harness();
        let draft = a_draft();
        let (id, mut held, _) = asks.register(&caller(), draft, false);

        assert!(asks.is_waiting(id, &caller()));
        let answer = VaultAskAnswer {
            decision: VaultAskDecision::Approve,
            master_password: None,
            two_factor: None,
            released: None,
        };
        assert!(asks.resolve(id, &caller(), answer.clone()));
        assert_eq!(held.try_recv(), Ok(answer), "the approval reaches the blocked caller");
        assert!(
            !asks.resolve(
                id,
                &caller(),
                VaultAskAnswer {
                    decision: VaultAskDecision::Decline,
                    master_password: None,
                    two_factor: None,
                    released: None,
                }
            ),
            "one answer only",
        );
        assert!(asks.settle(id, &caller(), VaultAskEnding::Approved));
        assert!(!asks.is_waiting(id, &caller()));
    }

    /// A call held for this seat is not another seat's to answer.
    #[test]
    fn another_seat_cannot_answer_this_call() {
        let (asks, _emit, _sink) = harness();
        let (id, _held, _) = asks.register(&caller(), a_draft(), false);
        let elsewhere = SessionSlot::worker("Default", "forge", "someone-else");

        assert!(!asks.resolve(
            id,
            &elsewhere,
            VaultAskAnswer {
                decision: VaultAskDecision::Approve,
                master_password: None,
                two_factor: None,
                released: None,
            }
        ));
        assert!(asks.is_waiting(id, &caller()), "the call is still the asker's");
    }

    /// No client to answer: the registry must not hold the call, and the
    /// awaiting caller sees a dead receiver - the fail-closed read.
    #[tokio::test]
    async fn a_call_with_no_ui_to_answer_it_fails_closed() {
        let (asks, _emit, _sink) = harness_with(std::sync::Arc::new(TestEmit::refusing()), None);

        let (_id, held, answerable) = asks.register(&caller(), a_draft(), false);

        assert!(!answerable, "an unanswerable call reports itself");
        assert!(asks.asks(&caller()).is_empty(), "an unanswerable call is dropped");
        assert!(held.await.is_err(), "a dead receiver reads as not-approved");
    }

    /// A session going away ends every call it holds.
    #[test]
    fn a_session_teardown_abandons_the_calls_it_holds() {
        let (asks, emit, sink) = harness();
        let (id, mut held, _) = asks.register(&caller(), a_draft(), false);

        asks.abandon(&caller());

        assert!(asks.asks(&caller()).is_empty(), "the teardown leaves nothing parked");
        assert!(held.try_recv().is_err(), "the blocked caller is released as abandoned");
        let endings: Vec<(Uuid, VaultAskEnding)> = emit
            .take()
            .into_iter()
            .filter_map(|update| match update {
                VaultUpdate::AskResolved { id, ending, .. } => Some((id, ending)),
                _ => None,
            })
            .collect();
        assert_eq!(endings, vec![(id, VaultAskEnding::Abandoned)]);
        assert_eq!(
            sink.resolved.lock().as_slice(),
            [(caller(), id)],
            "the host is told the ask settled",
        );
    }
}
