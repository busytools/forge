//! The workspace's half of the vault family's seams.
//!
//! The family owns its session type, its tools and its registry; the
//! workspace owns the machine-wide session (built from `[vault]` at boot and
//! swapped on a reload that changes the section) and the ask sequence. The
//! two meet here: [`VaultPorts`] maps the family's updates onto the wire
//! frames every view draws and numbers its arrivals, and [`VaultHost`]
//! hands the family the live session and the caller's working directory.
//!
//! Nothing in this module holds the family's state: `vault` and `vault_asks`
//! are handles the workspace owns, and the family reads them through the
//! seams.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use forge_primitives::vault::VaultReleased;
use forge_primitives::{AskSink, PendingInteractionKind, SessionSlot};
use forge_tools::vault::{
    VaultClient, VaultEmit, VaultError, VaultHost, VaultSession, VaultState, VaultUpdate,
};
use parking_lot::Mutex;

use crate::protocol::SessionUpdate;
use crate::update_fanout::UpdateFanout;
use crate::workspace::Workspace;

/// The family's updates mapped onto the workspace's wire, plus the ask
/// numbering its registry reports through.
pub(crate) struct VaultPorts {
    updates: UpdateFanout,
    pending_seq: Arc<Mutex<HashMap<SessionSlot, u64>>>,
}

impl VaultPorts {
    pub(crate) fn new(
        updates: UpdateFanout,
        pending_seq: Arc<Mutex<HashMap<SessionSlot, u64>>>,
    ) -> Self {
        Self { updates, pending_seq }
    }
}

impl VaultEmit for VaultPorts {
    fn emit(&self, update: VaultUpdate) -> bool {
        match update {
            // A parked call is only answerable by a subscriber that renders a
            // dock, so this is the answering send: `false` is what makes the
            // blocked handler fail closed instead of parking for ever.
            VaultUpdate::AskPending { key, draft } => {
                self.updates.send_answering(SessionUpdate::VaultAskPending { key, draft })
            }
            VaultUpdate::AskResolved { key, id, ending } => {
                self.updates.send(SessionUpdate::VaultAskResolved { key, id, ending })
            }
            VaultUpdate::LockChanged { locked } => {
                self.updates.send(SessionUpdate::VaultLockChanged { locked })
            }
            VaultUpdate::Rotated { refresh_token } => {
                self.updates.send(SessionUpdate::VaultRotated { refresh_token })
            }
        }
    }
}

impl AskSink for VaultPorts {
    fn note(&self, seat: &SessionSlot, _kind: PendingInteractionKind) {
        // The one counter the workspace's own registries bump too: the number
        // is the ask's identity on the wire.
        crate::workspace::note_ask_in(&self.pending_seq, seat);
    }

    fn resolve(&self, _seat: &SessionSlot, _id: uuid::Uuid) {
        // **The source's own read clears the mark.** `AskSource::asks` is
        // what `pending_interaction` walks, so a settled call leaves the
        // ledger by leaving the registry - there is no per-ask state here to
        // clear, and the sequence deliberately does not move (an answer that
        // shrinks a batch is not news).
    }
}

impl VaultHost for Workspace {
    fn session(&self) -> Option<Arc<VaultSession>> {
        self.vault.read().clone()
    }

    fn cwd(&self, seat: &SessionSlot) -> Option<PathBuf> {
        self.cwd_for_session(seat).map(PathBuf::from)
    }
}

impl Workspace {
    /// The material a client wraps to enrol this device, from the shared
    /// session - or `None` when it is locked or no `[vault]` is configured.
    pub(crate) fn vault_enrolment_material(&self) -> Option<VaultReleased> {
        self.vault.read().as_ref()?.enrolment_material()
    }

    /// The shared vault session as a view draws it - `None` when no `[vault]`
    /// is configured.
    pub fn vault_state(&self) -> Option<VaultState> {
        let session = self.vault.read().clone()?;
        Some(VaultState { locked: session.is_locked(), idle_minutes: session.idle_minutes() })
    }

    /// Re-stamp every call still parked after the shared session's lock state
    /// changed. A card whose sibling unlocked is an ordinary approve card
    /// from here; a re-lock asks for the password again.
    pub(crate) fn rebase_vault_asks(&self, needs_unlock: bool) {
        self.vault_asks.rebase(needs_unlock);
    }
}

/// The production vault clients: the machine's extra TLS roots, `https_only`,
/// the account's own user agent, and the vault family's bounds. The download
/// client takes a longer read and total bound because an attachment is opaque
/// ciphertext up to the server's cap.
///
/// This stays with the workspace because the trust setup is forge-agent's,
/// and the family must not depend on the agent layer to build a client.
pub(crate) fn production_client(base: &str) -> Result<VaultClient, VaultError> {
    use forge_tools::vault::client::{
        CONNECT_TIMEOUT, DOWNLOAD_READ_TIMEOUT, DOWNLOAD_TIMEOUT, READ_TIMEOUT, REQUEST_TIMEOUT,
    };
    let build = |read_timeout: std::time::Duration, timeout: std::time::Duration| {
        forge_agent::http_trust::with_extra_roots(
            reqwest::Client::builder()
                .user_agent(forge_tools::vault::client::user_agent())
                .https_only(true)
                .connect_timeout(CONNECT_TIMEOUT)
                .read_timeout(read_timeout)
                .timeout(timeout),
        )
        .build()
        .map_err(VaultError::Http)
    };
    VaultClient::with_clients(
        base,
        build(READ_TIMEOUT, REQUEST_TIMEOUT)?,
        build(DOWNLOAD_READ_TIMEOUT, DOWNLOAD_TIMEOUT)?,
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::protocol::Command;
    use forge_primitives::vault::{VaultAskAnswer, VaultAskDecision, VaultDraft, VaultTool};
    use forge_tools::vault::VaultConfig;
    use forge_tools::vault::crypto::{Unlocked, make_symmetric_key};
    use forge_tools::vault::session::{test_install_unlocked, test_session};

    fn unlocked_session() -> Arc<VaultSession> {
        let config = VaultConfig {
            url: "http://127.0.0.1:1".to_owned(),
            email: "hi@vedhavyas.dev".to_owned(),
            idle_minutes: None,
        };
        let session = Arc::new(test_session(&config, None, "test").expect("a session"));
        test_install_unlocked(
            &session,
            Unlocked {
                user_key: make_symmetric_key(),
                private_key: None,
                org_keys: std::collections::HashMap::new(),
                failed_orgs: Vec::new(),
                user_key_fingerprint: String::new(),
            },
        );
        session
    }

    /// **The settings card's lock press is a command**, so its dispatch arm is
    /// pinned: `VaultLock` locks the shared session and announces the change
    /// every view redraws on.
    #[test]
    fn the_lock_command_locks_the_shared_session() {
        let (workspace, _rx) = Workspace::testing_stub();
        let session = unlocked_session();
        *workspace.vault.write() = Some(Arc::clone(&session));

        assert!(!workspace.vault_state().expect("a vault").locked, "the session starts open");
        workspace.dispatch(Command::VaultLock).expect("the lock routes");
        assert!(workspace.vault_state().expect("a vault").locked, "the shared session locked");
        assert!(session.is_locked(), "the session the family reads is the one that locked");
    }

    /// Through `Workspace::dispatch`, the route a view's dock answer takes;
    /// and a click on a dock the core has already resolved is refused.
    #[tokio::test]
    async fn the_dock_answer_routes_to_the_registry_and_a_stale_one_is_refused() {
        let (workspace, _rx) = Workspace::testing_stub();
        let seat = SessionSlot::lead("Default", "forge");
        let draft = VaultDraft::new(VaultTool::Get, "read the password for 'bank'");
        let (id, decision, _) = workspace.vault_asks.register(&seat, draft, false);
        let answer = VaultAskAnswer {
            decision: VaultAskDecision::Approve,
            master_password: None,
            two_factor: None,
            released: None,
        };

        workspace
            .dispatch(Command::RespondVaultAsk { key: seat.clone(), id, answer: answer.clone() })
            .expect("the dock answer routes to the workspace");
        assert_eq!(decision.await.expect("the held call answers"), answer);

        let refused = workspace.dispatch(Command::RespondVaultAsk { key: seat, id, answer });
        assert!(
            matches!(refused, Err(crate::protocol::DispatchError::NoVaultAskWaiting { .. })),
            "a stale dock must report why the answer did not land: {refused:?}",
        );
    }

    /// **Every one of the family's updates crosses to its own wire frame**,
    /// and the parked-call arm uses the ANSWERING send. With only an
    /// observer attached that arm must report "nobody can answer" - swapped
    /// to the plain send, a draft would look answerable while no dock could
    /// ever open, and the blocked handler would park forever.
    #[test]
    fn the_family_updates_cross_to_their_wire_frames() {
        use crate::update_fanout::SubscriberRole;
        use forge_primitives::vault::{VaultAskEnding, VaultDraft, VaultTool};

        let updates = UpdateFanout::default();
        let seq: Arc<Mutex<HashMap<SessionSlot, u64>>> = Arc::new(Mutex::new(HashMap::new()));
        let ports = VaultPorts::new(updates.clone(), seq);
        let key = SessionSlot::lead("Default", "forge");
        let draft = VaultDraft::new(VaultTool::Get, "read the password for 'bank'");

        let mut observer = updates.subscribe(SubscriberRole::Observing);
        assert!(
            !ports.emit(VaultUpdate::AskPending { key: key.clone(), draft: draft.clone() }),
            "an observer alone cannot answer a parked call",
        );
        // The frame still reaches the observer - it draws no dock, and the
        // answering send's report is what fails the call closed rather than
        // parking it forever.
        assert!(observer.try_recv().is_ok(), "the observer still sees the ask was raised");

        let mut answerer = updates.subscribe(SubscriberRole::Answering);
        assert!(
            ports.emit(VaultUpdate::AskPending { key: key.clone(), draft: draft.clone() }),
            "an answering subscriber makes the call answerable",
        );
        match std::sync::Arc::unwrap_or_clone(answerer.try_recv().expect("the card").update) {
            SessionUpdate::VaultAskPending { key: got, draft: got_draft } => {
                assert_eq!(got, key, "the card is addressed to the seat that asked");
                assert_eq!(got_draft.id, draft.id);
            }
            other => panic!("the parked call crossed as {other:?}"),
        }

        assert!(ports.emit(VaultUpdate::AskResolved {
            key: key.clone(),
            id: draft.id,
            ending: VaultAskEnding::Approved,
        }));
        match std::sync::Arc::unwrap_or_clone(answerer.try_recv().expect("the ending").update) {
            SessionUpdate::VaultAskResolved { key: got, id, ending } => {
                assert_eq!(got, key);
                assert_eq!(id, draft.id);
                assert_eq!(ending, VaultAskEnding::Approved);
            }
            other => panic!("the settlement crossed as {other:?}"),
        }

        assert!(ports.emit(VaultUpdate::LockChanged { locked: true }));
        match std::sync::Arc::unwrap_or_clone(answerer.try_recv().expect("the lock").update) {
            SessionUpdate::VaultLockChanged { locked } => assert!(locked),
            other => panic!("the lock crossed as {other:?}"),
        }

        assert!(ports.emit(VaultUpdate::Rotated { refresh_token: "next".to_owned() }));
        match std::sync::Arc::unwrap_or_clone(answerer.try_recv().expect("the rotation").update) {
            SessionUpdate::VaultRotated { refresh_token } => assert_eq!(refresh_token, "next"),
            other => panic!("the rotation crossed as {other:?}"),
        }
        assert!(
            !format!("{:?}", VaultUpdate::Rotated { refresh_token: "next".to_owned() })
                .contains("next"),
            "the rotated token is a secret and the update's Debug redacts it",
        );
    }

    #[tokio::test]
    async fn production_client_is_https_only() {
        use std::time::Duration;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let client = production_client(&format!("http://{address}")).expect("clients build");
        let error = client.prelogin("hi@example.com").await.expect_err("https only");
        assert!(matches!(error, VaultError::Http(_)), "{error}");
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept()).await.is_err(),
            "no http request was made",
        );
    }
}
