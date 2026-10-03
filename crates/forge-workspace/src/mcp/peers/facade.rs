//! The narrow workspace-state surface the peer-coordination tools
//! depend on, plus the production impl on [`Workspace`] and a mock
//! for unit tests.
//!
//! ## Why a trait, not direct calls on `Workspace`
//!
//! The four Tool impls (C5-C8) hold an `Arc<dyn WorkspaceFacade>`
//! rather than `Arc<Workspace>`. Two reasons:
//!
//! 1. **Testability.** Unit tests instantiate `MockWorkspaceFacade`
//!    (capture-into-Vec) and assert the tool dispatched the expected
//!    commands without spinning up real `Workspace` infrastructure
//!    (which needs a workspace dir, account state map, etc.).
//! 2. **Narrow API surface.** The tools only need ~7 methods; binding
//!    them to the full `Workspace` would surface everything to anyone
//!    poking at the tools.
//!
//! ## Method shape
//!
//! All methods are `&self` + `parking_lot::Mutex` internally, so the
//! trait is plain (not `async_trait`). Tools may `await` other things
//! inside their `Tool::call` body but the facade calls themselves
//! return synchronously after a mutex acquire.

use std::sync::{Arc, Weak};

use crate::mcp::peers::types::{PeerLiveness, PeerStatus, WrappedPrompt};
use tracing::warn;

use crate::SessionSlot;
use crate::protocol::Command;
use crate::workspace::Workspace;

/// Why a `deliver_peer_prompt` call failed synchronously.
///
/// An async failure - a message parked for a sleeping project whose spawn
/// then fails - is not reported here: it reaches the sender later as a
/// delivery notice through [`crate::Workspace::notice_undelivered_message`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliverError {
    /// No project named `name` in forge.toml.
    UnknownTarget { name: String },
}

/// The narrow workspace-state surface peer-coordination tools call
/// into. See module docs for design rationale.
pub trait WorkspaceFacade: Send + Sync {
    /// Snapshot of every configured project's peer status, in
    /// forge.toml declaration order. Computed fresh on each call.
    fn list_peers(&self) -> Vec<PeerStatus>;

    /// Identity of the calling session. Returns `None` when `caller`
    /// doesn't resolve to any known project (defensive - the tools
    /// closure-bind a real key at spawn time, so this should be
    /// `Some` in practice).
    fn whoami(&self, caller: &SessionSlot) -> Option<PeerStatus>;

    /// Deliver a wrapped peer prompt to `target_project`.
    ///
    /// Synchronous return is the immediate decision: `Ok(())` means
    /// `Command::DeliverPeerPrompt` has been dispatched, and whether the
    /// target was running or asleep is that handler's business (it either
    /// dispatches a `Command::Prompt` or parks the prompt and spawns).
    /// `Err(UnknownTarget)` - target not in forge.toml.
    fn deliver_peer_prompt(
        &self,
        caller: &SessionSlot,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<(), DeliverError>;
}

/// Production impl. Holds a `Weak<Workspace>` rather than
/// `Arc<Workspace>` so the construction sites
/// (`Arc::new(weak) as Arc<dyn WorkspaceFacade>`) don't close the
/// Workspace → pool → AgentHandle → bridge → MCP → Tool → facade
/// → Workspace strong cycle. Audit C7.
///
/// Every trait method starts with `upgrade()`. When the workspace
/// has been dropped (only possible if `Workspace::shutdown` has run
/// and tools are firing during teardown), each method short-circuits
/// to a sensible "workspace gone" fallback - typically returning a
/// default, an error variant, or a no-op. The recipient session is
/// dying anyway; the LLM call won't have anywhere to land.
pub struct ProdWorkspaceFacade(pub Weak<Workspace>);

impl ProdWorkspaceFacade {
    /// Construct from a strong reference. Downgrades immediately so
    /// the facade never closes a cycle through its own holdings.
    pub fn from_arc(workspace: &Arc<Workspace>) -> Arc<dyn WorkspaceFacade> {
        Arc::new(Self(Arc::downgrade(workspace)))
    }
}

/// Return the first non-worker session in `view.sessions`, i.e. the
/// project's lead. Worker sessions land in `view.sessions` once their
/// `Connected` lands and the catalog indexes them; keying on the slot a
/// spawn stated is what keeps a worker from shadowing its lead, so no
/// filter over the catalog is needed.
fn lead_for(
    ws: &crate::workspace::Workspace,
    view: &crate::views::ProjectView,
) -> (SessionSlot, bool) {
    let slot = SessionSlot::lead(&view.org, &view.name);
    let running = ws.session_is_pooled(&slot);
    (slot, running)
}

impl WorkspaceFacade for ProdWorkspaceFacade {
    fn list_peers(&self) -> Vec<PeerStatus> {
        let Some(ws) = self.0.upgrade() else { return Vec::new() };
        ws.list_projects()
            .into_iter()
            .map(|view| {
                let (lead, running) = lead_for(&ws, &view);
                let liveness = if running { PeerLiveness::Running } else { PeerLiveness::Sleeping };
                PeerStatus {
                    name: view.name,
                    org: view.org,
                    path: view.path,
                    status: liveness,
                    spawned_at: ws.session_last_activity(&lead),
                }
            })
            .collect()
    }

    fn whoami(&self, caller: &SessionSlot) -> Option<PeerStatus> {
        let ws = self.0.upgrade()?;
        let cx = crate::mcp::caller_context::caller_context(&ws, caller)?;
        // Liveness keys off the LEAD's session, not the caller's:
        // `PeerStatus` represents project-level peer identity. A worker
        // calling `whoami` sees its project's peer identity (the same
        // identity another peer would see when targeting this project),
        // not its own session - the lead-only match the pre-#298 impl
        // gated on was wrong because workers also legitimately ask "who
        // am I as a peer?".
        let status = if cx.lead_running { PeerLiveness::Running } else { PeerLiveness::Sleeping };
        Some(PeerStatus {
            name: cx.project_name,
            org: cx.project_org,
            path: cx.project_path,
            status,
            spawned_at: ws.session_last_activity(&cx.lead),
        })
    }

    fn deliver_peer_prompt(
        &self,
        caller: &SessionSlot,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<(), DeliverError> {
        let Some(ws) = self.0.upgrade() else {
            return Err(DeliverError::UnknownTarget { name: target_project.to_owned() });
        };
        if !ws.list_projects().iter().any(|v| v.name == target_project) {
            return Err(DeliverError::UnknownTarget { name: target_project.to_owned() });
        }
        if let Err(err) = ws.dispatch(Command::DeliverPeerPrompt {
            caller: caller.clone(),
            target_project: target_project.to_owned(),
            wrapped,
        }) {
            warn!(
                target: "forge_workspace::mcp::peers",
                error = ?err,
                "Command::DeliverPeerPrompt dispatch failed; tool will still report the send"
            );
        }
        Ok(())
    }
}

/// Mock for unit tests in the four Tool impls. Captures every dispatched
/// call into a Vec so tests can assert "tool X dispatched
/// register_inflight_ask with these args" without spinning up a real
/// Workspace.
#[cfg(any(test, feature = "testing"))]
#[derive(Default)]
pub struct MockWorkspaceFacade {
    /// Pre-loaded peer status snapshot returned by `list_peers`.
    pub peers: parking_lot::Mutex<Vec<PeerStatus>>,
    /// Captured calls to `deliver_peer_prompt`.
    pub deliver_calls: parking_lot::Mutex<Vec<(SessionSlot, String, WrappedPrompt)>>,
    /// If set, `deliver_peer_prompt` returns this error instead of
    /// running the normal lookup path. Lets tests force-test the
    /// failure surface.
    pub force_deliver_error: parking_lot::Mutex<Option<DeliverError>>,
}

#[cfg(any(test, feature = "testing"))]
impl MockWorkspaceFacade {
    /// New empty mock; tests pre-load the fields they care about.
    pub fn new() -> Self {
        Self::default()
    }

    /// Cheap clone-and-share for tools that need `Arc<dyn ...>`.
    pub fn into_arc(self) -> Arc<dyn WorkspaceFacade> {
        Arc::new(self)
    }
}

#[cfg(any(test, feature = "testing"))]
impl WorkspaceFacade for MockWorkspaceFacade {
    fn list_peers(&self) -> Vec<PeerStatus> {
        self.peers.lock().clone()
    }

    fn whoami(&self, caller: &SessionSlot) -> Option<PeerStatus> {
        // Mock's `whoami` does the same "find by caller's lead session"
        // shape as the prod impl, but works against the mock's
        // pre-loaded peers list. Tests that want a specific identity
        // pre-load the peers with an entry whose name matches their
        // caller key convention.
        self.peers.lock().iter().find(|p| p.name == caller.label()).cloned()
    }

    fn deliver_peer_prompt(
        &self,
        caller: &SessionSlot,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<(), DeliverError> {
        if let Some(err) = self.force_deliver_error.lock().clone() {
            return Err(err);
        }
        let known = self.peers.lock().iter().any(|p| p.name == target_project);
        if !known {
            return Err(DeliverError::UnknownTarget { name: target_project.to_owned() });
        }
        self.deliver_calls.lock().push((caller.clone(), target_project.to_owned(), wrapped));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::peers::types::{MessageId, WrappedKind};
    use std::path::PathBuf;

    fn fake_key(s: &str) -> SessionSlot {
        // Tests use the production constructor (no test-helpers feature
        // here) - SessionSlot is just a String newtype, so this aligns
        // with how the workspace itself constructs keys at runtime.
        SessionSlot::from_str_for_test(s)
    }

    fn fake_peer(name: &str, liveness: PeerLiveness) -> PeerStatus {
        PeerStatus {
            name: name.to_owned(),
            org: "TestOrg".to_owned(),
            path: PathBuf::from(format!("/tmp/{name}")),
            status: liveness,
            spawned_at: None,
        }
    }

    fn fake_wrapped() -> WrappedPrompt {
        WrappedPrompt {
            id: MessageId::mint(),
            kind: WrappedKind::Message,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            body: "hi".to_owned(),
        }
    }

    #[test]
    fn mock_list_peers_returns_preloaded() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("alpha", PeerLiveness::Running));
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Sleeping));
        let peers = mock.list_peers();
        assert_eq!(peers.len(), 2);
        assert_eq!(peers[0].name, "alpha");
        assert_eq!(peers[1].status, PeerLiveness::Sleeping);
    }

    #[test]
    fn mock_deliver_unknown_target_errors() {
        let mock = MockWorkspaceFacade::new();
        let caller = fake_key("alpha");
        let result = mock.deliver_peer_prompt(&caller, "missing", fake_wrapped());
        assert!(
            matches!(result, Err(DeliverError::UnknownTarget { ref name }) if name == "missing")
        );
    }

    #[test]
    fn mock_deliver_known_target_records_the_call() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Running));
        let caller = fake_key("alpha");
        assert_eq!(mock.deliver_peer_prompt(&caller, "beta", fake_wrapped()), Ok(()));
        assert_eq!(mock.deliver_calls.lock().len(), 1);
    }

    #[test]
    fn mock_whoami_matches_by_name() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("alpha", PeerLiveness::Running));
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Sleeping));
        // Convention in the mock: caller key string == project name.
        // The prod impl matches by SessionSlot ↔ lead-session lookup.
        let identity = mock.whoami(&fake_key("alpha"));
        assert!(identity.is_some());
        assert_eq!(identity.unwrap().name, "alpha");
    }

    #[test]
    fn force_deliver_error_overrides_normal_path() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Running));
        *mock.force_deliver_error.lock() =
            Some(DeliverError::UnknownTarget { name: "forced".to_owned() });
        let caller = fake_key("alpha");
        let result = mock.deliver_peer_prompt(&caller, "beta", fake_wrapped());
        assert!(matches!(
            result,
            Err(DeliverError::UnknownTarget { ref name }) if name == "forced"
        ));
    }
}

/// Worker-shadow resolution for `lead_session_view` - the shared gate
/// `list_peers` / `whoami` / `deliver_peer_prompt` all route through.
/// Behind `test-helpers` because the fixtures use the cross-crate
/// `ProjectView` / `SessionView` constructors.
#[cfg(all(test, feature = "test-helpers"))]
mod lead_resolution_tests {
    use super::{DeliverError, ProdWorkspaceFacade, lead_for};
    use crate::mcp::peers::types::WrappedKind;
    use crate::target::ProjectKey;
    use crate::views::{ProjectView, SessionView};
    use crate::workspace::Workspace;
    use crate::{MessageId, SessionSlot, WorkerEntry, WrappedPrompt};
    use forge_primitives::WorkerLiveness;
    use std::time::SystemTime;

    fn session(id: &str) -> SessionView {
        SessionView::new_for_test(forge_primitives::SessionId::new(id), id, true, None)
    }

    fn worker_entry(slot: SessionSlot) -> WorkerEntry {
        WorkerEntry {
            label: "reviewer".into(),
            charter: "review the diff".into(),
            slot,
            session_id: None,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by: SessionSlot::lead("Test", "forge"),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    #[test]
    fn names_the_lead_slot_when_no_lead_row_is_catalogued() {
        // A project with no lead transcript still has a lead slot: the
        // triple names it, so the catalog cannot take it away.
        let (ws, _rx) = Workspace::testing_stub();
        let key = ProjectKey::new("p".to_owned());
        let lead = session("lead-uuid");
        let view = ProjectView::new_for_test(key, "forge", "/tmp/forge", vec![lead.clone()]);
        let (resolved, running) = lead_for(&ws, &view);
        assert_eq!(resolved, SessionSlot::lead("Test", "forge"));
        assert!(!running, "nothing is pooled for it here");
    }

    #[test]
    fn names_the_lead_slot_even_when_every_session_is_a_live_worker() {
        // LeadGone: the lead disconnected and only workers remain in
        // the catalog. The slot still names the lead; only its
        // liveness is gone, which is what the caller reads.
        let (ws, _rx) = Workspace::testing_stub();
        let key = ProjectKey::new("p".to_owned());
        let worker = session("worker-uuid");
        let view =
            ProjectView::new_for_test(key.clone(), "forge", "/tmp/forge", vec![worker.clone()]);
        ws.insert_live_worker(
            &key,
            worker_entry(SessionSlot::from_str_for_test(worker.session.as_str())),
        );
        let (resolved, running) = lead_for(&ws, &view);
        assert_eq!(
            resolved,
            SessionSlot::lead("Test", "forge"),
            "an all-worker project still names its lead"
        );
        assert!(!running, "and the lead is not running");
    }

    fn wrapped() -> WrappedPrompt {
        WrappedPrompt {
            id: MessageId::mint(),
            kind: WrappedKind::Message,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            body: "hi".to_owned(),
        }
    }

    #[test]
    fn deliver_to_unknown_project_errors() {
        let (ws, _rx) = Workspace::testing_stub();
        let facade = ProdWorkspaceFacade::from_arc(&ws);
        let result = facade.deliver_peer_prompt(
            &SessionSlot::from_str_for_test("caller"),
            "no-such-project",
            wrapped(),
        );
        assert!(
            matches!(result, Err(DeliverError::UnknownTarget { ref name }) if name == "no-such-project")
        );
    }

    #[test]
    fn whoami_none_when_caller_leads_no_project() {
        let (ws, _rx) = Workspace::testing_stub();
        let facade = ProdWorkspaceFacade::from_arc(&ws);
        assert!(facade.whoami(&SessionSlot::from_str_for_test("nobody")).is_none());
    }

    /// #298 Cause 1: workers can call `agents__whoami` and see their
    /// project's peer identity. Pre-fix, the impl required the caller
    /// to be the lead session, which returned None for any worker.
    #[test]
    fn whoami_resolves_worker_caller_to_project_peer_identity() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("myproj", "/tmp/myproj");
        ws.record_connected_session("/tmp/myproj", "lead-uuid", None);
        let pk = crate::ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/myproj")),
        );
        let worker = SessionSlot::worker("TestOrg", "myproj", "worker-uuid");
        ws.insert_live_worker(&pk, worker_entry(worker.clone()));

        let facade = ProdWorkspaceFacade::from_arc(&ws);
        let status =
            facade.whoami(&worker).expect("worker caller resolves to its project's peer identity");
        assert_eq!(status.name, "myproj");
        assert_eq!(status.org, "TestOrg");

        // Regression lock: the pre-existing lead-only path still
        // resolves to the same project identity.
        let lead_status = facade
            .whoami(&SessionSlot::lead("TestOrg", "myproj"))
            .expect("lead caller still resolves");
        assert_eq!(lead_status.name, "myproj");
    }
}
