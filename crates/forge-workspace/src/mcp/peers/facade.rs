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
    /// The command reached the handler and the handler refused it: the
    /// target's session did not take the prompt, the project left
    /// `forge.toml` between the tool's check and the dispatch, or the
    /// workspace is shutting down. `message` says which.
    DispatchFailed { message: String },
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
    /// The answer is the handler's own: `Ok(())` once the target's session
    /// took the prompt or it was parked for a spawn the handler kicked off,
    /// `Err(UnknownTarget)` for a project not in forge.toml, and
    /// `Err(DispatchFailed)` when the handler refused the delivery. A
    /// delivery that never left forge is never `Ok` (#2342).
    fn deliver_peer_prompt(
        &self,
        caller: &SessionSlot,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<(), DeliverError>;

    /// The exit call: the note is accepted only while a handoff is running on
    /// the caller's own seat. A handoff starts at `/handoff` or at the
    /// automatic trigger (fill or compaction), which dispatch the command
    /// directly; no caller starts one through here.
    fn handoff(&self, caller: &SessionSlot, note: String) -> Result<(), HandoffError>;
}

/// Why a handoff could not be started or completed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HandoffError {
    #[error("the seat {slot} is not running; a handoff needs a live session to hand over")]
    SessionGone { slot: String },
    #[error(
        "no handoff is running on {slot}; a handoff starts at /handoff or at the automatic trigger \
         (fill or compaction)"
    )]
    NotInAHandoff { slot: String },
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
        // The stamp is the caller's own seat, the value the row
        // `agents__list` shows for it: a live worker's row carries its
        // spawn time, a project's own row its session's last activity.
        let spawned_at = if cx.is_lead {
            ws.session_last_activity(&cx.lead)
        } else {
            ws.list_live_workers(&cx.project_key)
                .iter()
                .find(|w| w.slot == *caller)
                .map(|w| w.spawned_at)
        };
        Some(PeerStatus {
            name: cx.project_name,
            org: cx.project_org,
            path: cx.project_path,
            status,
            spawned_at,
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
        let (respond, mut reply) = tokio::sync::oneshot::channel();
        ws.dispatch(Command::DeliverPeerPrompt {
            caller: caller.clone(),
            target_project: target_project.to_owned(),
            wrapped,
            respond: Some(respond),
        })
        .map_err(|err| DeliverError::DispatchFailed {
            message: format!("the workspace did not take the command ({err:?})"),
        })?;
        // **The handler runs inline on this thread**, so its answer is
        // already in the channel. An empty channel is still not a success:
        // a delivery nobody answered is a delivery nobody can claim.
        delivery_answer(reply.try_recv())
    }

    fn handoff(&self, caller: &SessionSlot, note: String) -> Result<(), HandoffError> {
        let Some(ws) = self.0.upgrade() else {
            return Err(HandoffError::SessionGone { slot: caller.display() });
        };
        // **An exit is only the end of a handoff already running.** The gate
        // opens at the entry and closes on the swap, so it is the authority
        // on whether this call has a turn to belong to.
        if !ws.swap_in_progress(caller) {
            return Err(HandoffError::NotInAHandoff { slot: caller.display() });
        }
        ws.dispatch(Command::Handoff { key: caller.clone(), brief: Some(note) })
            .map_err(|_| HandoffError::SessionGone { slot: caller.display() })
    }
}

/// The handler's answer narrowed to the facade's own: a command the
/// workspace took but nobody answered is a refusal, not a success (#2342).
fn delivery_answer(
    reply: Result<Result<(), String>, tokio::sync::oneshot::error::TryRecvError>,
) -> Result<(), DeliverError> {
    match reply {
        Ok(Ok(())) => Ok(()),
        Ok(Err(message)) => Err(DeliverError::DispatchFailed { message }),
        Err(_) => Err(DeliverError::DispatchFailed {
            message: "the delivery handler took the command and answered nothing".to_owned(),
        }),
    }
}

/// Mock for unit tests in the Tool impls. Captures every dispatched call
/// into a Vec so tests can assert "tool X dispatched deliver_peer_prompt
/// with these args" without spinning up a real Workspace.
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
    /// Captured calls to `handoff`.
    pub handoff_calls: parking_lot::Mutex<Vec<(SessionSlot, String)>>,
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
        // Mock's `whoami` answers from the mock's pre-loaded peers list
        // with the row named for the caller's label - the mock's stand-in
        // for the caller's own stamp, which the prod impl builds from the
        // caller's live worker entry or its project's own row.
        // Tests that want a specific identity pre-load the peers with an
        // entry whose name matches their caller key convention.
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

    fn handoff(&self, caller: &SessionSlot, note: String) -> Result<(), HandoffError> {
        self.handoff_calls.lock().push((caller.clone(), note));
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

    /// **A handler that answers nothing is not an ack.** The command was
    /// taken; the empty channel is the delivery nobody can claim, and
    /// mapping it to `Ok(())` is the warn-and-Ok overclaim returning
    /// (#2342).
    #[test]
    fn a_handler_that_answers_nothing_is_not_an_ack() {
        let (sender, mut reply) = tokio::sync::oneshot::channel::<Result<(), String>>();
        drop(sender);
        match delivery_answer(reply.try_recv()) {
            Err(DeliverError::DispatchFailed { message }) => assert!(
                message.contains("answered nothing"),
                "the refusal says the command was taken but unanswered: {message}",
            ),
            other => panic!("an empty answer must not read as delivered; got {other:?}"),
        }
    }
}

/// Worker-shadow resolution for `lead_session_view` - the shared gate
/// `list_peers` / `whoami` / `deliver_peer_prompt` all route through.
/// Behind `test-helpers` because the fixtures use the cross-crate
/// `ProjectView` / `SessionView` constructors.
#[cfg(all(test, feature = "test-helpers"))]
mod lead_resolution_tests {
    use super::{DeliverError, HandoffError, ProdWorkspaceFacade, lead_for};
    use crate::mcp::peers::types::WrappedKind;
    use crate::target::ProjectKey;
    use crate::views::{ProjectView, SessionView};
    use crate::workspace::Workspace;
    use crate::{MessageId, SessionSlot, WorkerEntry, WrappedPrompt};
    use forge_primitives::WorkerLiveness;
    use std::time::{Duration, SystemTime};

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
            cwd: None,
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

    /// **A delivery no session could take is not a success.** The running
    /// branch is chosen on the pool entry alone; the dispatch then finds no
    /// session task, and that has to reach the caller as an error rather
    /// than the old warn-and-Ok (#2342).
    #[test]
    fn a_delivery_no_session_can_take_is_not_acked() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("beta", "/tmp/beta");
        let slot = SessionSlot::lead("TestOrg", "beta");
        let (handle, _commands) = Workspace::testing_stub_handle();
        ws.pool.lock().insert(
            slot,
            crate::workspace::PooledAgent {
                handle: std::sync::Arc::new(handle),
                account: forge_gateway::AccountKey("acct-a".to_owned()),
                permission_mode: None,
                registration: None,
                session_id: "pooled-no-task".to_owned(),
            },
        );
        let facade = ProdWorkspaceFacade::from_arc(&ws);

        let result = facade.deliver_peer_prompt(
            &SessionSlot::from_str_for_test("caller"),
            "beta",
            WrappedPrompt {
                id: MessageId::mint(),
                kind: WrappedKind::Message,
                sender_name: "forge".to_owned(),
                sender_org: "Personal".to_owned(),
                body: "hi".to_owned(),
            },
        );

        match result {
            Err(DeliverError::DispatchFailed { message }) => assert!(
                message.contains("the target's running session did not take the message"),
                "the refusal is the running branch's own answer, not an unknown target: {message}",
            ),
            other => panic!(
                "a prompt the pooled lead's missing task cannot take is a refusal; got {other:?}"
            ),
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

    /// A worker entry for `label` in the fixture's project, carrying a
    /// chosen spawn stamp so a test can tell whose stamp a read carried.
    fn entry_stamped(label: &str, spawned_at: SystemTime) -> WorkerEntry {
        let slot = SessionSlot::worker("TestOrg", "myproj", label);
        WorkerEntry { label: label.to_owned(), spawned_at, ..worker_entry(slot) }
    }

    /// #1716: `whoami` answers with the caller's own seat - the stamp the
    /// `agents__list` row for that seat carries. A worker caller reads its
    /// own spawn time rather than the lead's; a lead caller reads its
    /// project's own row, unchanged.
    #[test]
    fn whoami_stamps_the_callers_own_seat() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("myproj", "/tmp/myproj");
        let pk = crate::ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/myproj")),
        );
        // The lead is live and has a row, so its own stamp is a real value
        // a wrongly-keyed read carries rather than reading as absent.
        let lead = SessionSlot::lead("TestOrg", "myproj");
        ws.seed_test_running_session_id(&lead, "lead-session");
        ws.record_connected_session("/tmp/myproj", "lead-session", None);
        let lead_stamp = ws.session_last_activity(&lead).expect("the lead's row is seeded");

        // Two live workers, the caller second: a stamp taken from any
        // worker rather than the caller's own seat lands on the other one.
        let caller = SessionSlot::worker("TestOrg", "myproj", "caller");
        let caller_stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        ws.insert_live_worker(
            &pk,
            entry_stamped("other", SystemTime::UNIX_EPOCH + Duration::from_secs(2_000)),
        );
        ws.insert_live_worker(&pk, entry_stamped("caller", caller_stamp));
        // The caller runs under a session of its own, so a stamp read from
        // its transcript's last activity is a different real value.
        ws.seed_test_running_session_id(&caller, "caller-session");
        ws.record_connected_session("/tmp/myproj", "caller-session", None);

        let facade = ProdWorkspaceFacade::from_arc(&ws);
        let worker_status = facade.whoami(&caller).expect("a worker caller resolves");
        assert_eq!(
            worker_status.spawned_at,
            Some(caller_stamp),
            "a worker caller's stamp is its own seat's, the row agents__list shows for it \
             (the lead's is {lead_stamp:?})",
        );

        let lead_status = facade.whoami(&lead).expect("a lead caller resolves");
        assert_eq!(
            lead_status.spawned_at,
            Some(lead_stamp),
            "a lead caller's stamp is its project's own row, unchanged",
        );
    }

    /// #1716: a caller whose live-worker row is gone (the teardown race) is
    /// answered with no stamp rather than falling back to the lead's - the
    /// miss branch, where an `or_else` back to the project's stamp would
    /// resurrect the #1716 shape.
    #[test]
    fn whoami_answers_a_caller_with_no_row_with_no_stamp() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project("myproj", "/tmp/myproj");
        // The lead's own stamp is a real value here, so a fallback to it
        // reads as a wrong answer rather than as absence.
        let lead = SessionSlot::lead("TestOrg", "myproj");
        ws.seed_test_running_session_id(&lead, "lead-session");
        ws.record_connected_session("/tmp/myproj", "lead-session", None);

        let gone = SessionSlot::worker("TestOrg", "myproj", "gone");
        let facade = ProdWorkspaceFacade::from_arc(&ws);
        let status = facade.whoami(&gone).expect("a worker caller still resolves to its project");
        assert_eq!(
            status.spawned_at, None,
            "a caller with no live-worker row is answered with no stamp, not the lead's",
        );
    }

    /// **The accepting arm.** With a handoff running, the exit call is
    /// dispatched with its note; a guard that refused whenever a brief was
    /// present would close the only exit a handoff has.
    #[test]
    fn a_running_handoffs_exit_is_dispatched() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.enable_test_dispatch_intercept();
        let facade = ProdWorkspaceFacade::from_arc(&ws);
        let slot = SessionSlot::worker("Busytools", "forge", "handoff-cut");
        ws.begin_swap(&slot);

        facade.handoff(&slot, "Now: the cut is in.".to_owned()).expect("accepted");

        let routed = ws.drain_test_dispatch_buffer();
        assert!(
            routed.iter().any(|command| matches!(
                command,
                crate::protocol::Command::Handoff { brief: Some(note), .. }
                    if note == "Now: the cut is in."
            )),
            "the note rides the command: {routed:?}",
        );
    }

    /// **An exit call with no handoff running is refused, naming the two
    /// doors.** `agents__handoff` completes a handoff; it can never start
    /// one, which is what keeps `/handoff` and the fill trigger the only
    /// entries.
    #[test]
    fn a_cold_handoff_exit_is_refused() {
        let (ws, _rx) = Workspace::testing_stub();
        let facade = ProdWorkspaceFacade::from_arc(&ws);
        let slot = SessionSlot::worker("Busytools", "forge", "handoff-cut");

        let error = facade
            .handoff(&slot, "Now: the cut is in.".to_owned())
            .expect_err("no handoff is running on this seat");
        assert_eq!(error, HandoffError::NotInAHandoff { slot: slot.display() });
        assert!(
            error.to_string().contains("/handoff")
                && error.to_string().contains("automatic trigger"),
            "the refusal names both doors: {error}",
        );
    }
}
