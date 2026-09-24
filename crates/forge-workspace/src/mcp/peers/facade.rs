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

use forge_primitives::PeerInflightStats;

use crate::mcp::peers::types::{PeerLiveness, PeerStatus, WrappedPrompt};
use tracing::warn;

use crate::SessionSlot;
use crate::protocol::{Command, SessionUpdate};
use crate::workspace::Workspace;

/// What `deliver_peer_prompt` returns on success - whether the target
/// session was already running (prompt sent immediately) or asleep
/// (workspace dispatched a SpawnProject and buffered the prompt for
/// delivery once Connected fires).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetStatus {
    /// Target was running; the wrapped prompt has been dispatched via
    /// the workspace command bus and will land in the next turn.
    Delivered,
    /// Target was sleeping; a `Command::SpawnProject` is in flight and
    /// the wrapped prompt is parked for target's owner for delivery on
    /// `AgentEvent::Connected` (drained in C11).
    QueuedForSpawn,
}

/// Why a `deliver_peer_prompt` call failed synchronously.
///
/// Async delivery failures (target session crashes mid-flight) flow
/// through `Workspace::expire_target_inflight` and surface to the
/// caller via a synthetic `DeliveryFailureNotice` wrapper - not
/// through this enum.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliverError {
    /// No project named `name` in forge.toml.
    UnknownTarget { name: String },
}

/// Why delivering a Reply straight to the asker's session failed.
/// Reply delivery bypasses name/label resolution (the asker is
/// addressed by `SessionSlot`), so the only failure mode is a caller
/// session that closed before the reply could land.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReplyDeliverError {
    /// The asker's session closed before its reply could be delivered.
    CallerSessionGone,
}

impl ReplyDeliverError {
    /// LLM-facing sentence explaining why the reply could not land.
    /// Shared by the peers + workers tell handlers.
    pub(crate) fn user_message(&self) -> String {
        match self {
            ReplyDeliverError::CallerSessionGone => {
                "the original asker's session is no longer available, so your reply could not be \
                 delivered."
                    .to_owned()
            }
        }
    }
}

/// Per-session counter delta the tools push into the workspace's
/// `peer_stats` map. The workspace then emits
/// `SessionUpdate::PeerInflightStatsChanged` so the TUI reducer can
/// update the sidebar peer-activity badge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PeerStatsDelta {
    OutgoingPlus1,
    OutgoingMinus1,
    IncomingPlus1,
    IncomingMinus1,
    DeliveryFailedPlus1,
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
    /// Synchronous return is the immediate decision:
    /// - `Ok(Delivered)` - target is running; `Command::DeliverPeerPrompt`
    ///   has been dispatched and will land as a `Command::Prompt` on
    ///   target's SessionTask in the next dispatch cycle.
    /// - `Ok(QueuedForSpawn)` - target is sleeping; a
    ///   `Command::SpawnProject` is in flight and the wrapped prompt
    ///   is parked for target's owner for delivery on
    ///   `AgentEvent::Connected`.
    /// - `Err(UnknownTarget)` - target not in forge.toml.
    ///
    /// The actual buffer + dispatch logic lives in `spawn.rs`'s
    /// `Command::DeliverPeerPrompt` handler (lands in C11).
    fn deliver_peer_prompt(
        &self,
        caller: &SessionSlot,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<TargetStatus, DeliverError>;

    /// Apply a delta to `peer_stats[key]` and emit
    /// `SessionUpdate::PeerInflightStatsChanged` so the TUI reducer
    /// can update the sidebar peer-activity badge.
    fn bump_inflight_stats(&self, key: &SessionSlot, delta: PeerStatsDelta);
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
        let projects = ws.list_projects();
        let stat_counters = ws.peer_stats.lock();
        projects
            .into_iter()
            .map(|view| {
                let (lead, running) = lead_for(&ws, &view);
                let liveness = if running { PeerLiveness::Running } else { PeerLiveness::Sleeping };
                let counts = stat_counters.get(&lead).cloned().unwrap_or_default();
                let spawned_at = ws.session_last_activity(&lead);
                PeerStatus {
                    name: view.name,
                    org: view.org,
                    path: view.path,
                    status: liveness,
                    in_flight_incoming: counts.incoming,
                    in_flight_outgoing: counts.outgoing,
                    spawned_at,
                }
            })
            .collect()
    }

    fn whoami(&self, caller: &SessionSlot) -> Option<PeerStatus> {
        let ws = self.0.upgrade()?;
        let cx = crate::mcp::caller_context::caller_context(&ws, caller)?;
        // Liveness + stats key off the LEAD's session, not the
        // caller's: `PeerStatus` represents project-level peer
        // identity. A worker calling `whoami` sees its project's
        // peer identity (the same identity another peer would see
        // when targeting this project), not its own session - the
        // lead-only match the pre-#298 impl gated on was wrong
        // because workers also legitimately ask "who am I as a
        // peer?".
        let stat_counters = ws.peer_stats.lock();
        let counts = stat_counters.get(&cx.lead).cloned().unwrap_or_default();
        let status = if cx.lead_running { PeerLiveness::Running } else { PeerLiveness::Sleeping };
        let spawned_at = ws.session_last_activity(&cx.lead);
        drop(stat_counters);
        Some(PeerStatus {
            name: cx.project_name,
            org: cx.project_org,
            path: cx.project_path,
            status,
            in_flight_incoming: counts.incoming,
            in_flight_outgoing: counts.outgoing,
            spawned_at,
        })
    }

    fn deliver_peer_prompt(
        &self,
        caller: &SessionSlot,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<TargetStatus, DeliverError> {
        let Some(ws) = self.0.upgrade() else {
            return Err(DeliverError::UnknownTarget { name: target_project.to_owned() });
        };
        let project = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == target_project)
            .ok_or_else(|| DeliverError::UnknownTarget { name: target_project.to_owned() })?;
        // Probing the target project's lead by its slot, so a live
        // worker cannot shadow it.
        let target_status = if lead_for(&ws, &project).1 {
            TargetStatus::Delivered
        } else {
            TargetStatus::QueuedForSpawn
        };
        if let Err(err) = ws.dispatch(Command::DeliverPeerPrompt {
            caller: caller.clone(),
            target_project: target_project.to_owned(),
            wrapped,
        }) {
            warn!(
                target: "forge_workspace::mcp::peers",
                error = ?err,
                "Command::DeliverPeerPrompt dispatch failed; tool will still report immediate decision"
            );
        }
        Ok(target_status)
    }

    fn bump_inflight_stats(&self, key: &SessionSlot, delta: PeerStatsDelta) {
        let Some(ws) = self.0.upgrade() else { return };
        let stats_snapshot = {
            let mut stats = ws.peer_stats.lock();
            let entry = stats.entry(key.clone()).or_default();
            apply_delta(entry, delta);
            entry.clone()
        };
        let _ = ws.update_sender().send(SessionUpdate::PeerInflightStatsChanged {
            key: key.clone(),
            stats: stats_snapshot,
        });
    }
}

fn apply_delta(stats: &mut PeerInflightStats, delta: PeerStatsDelta) {
    // `saturating_sub` floors at 0, but reaching 0 from a Minus1 path
    // means our bookkeeping ran a Minus without a matching Plus - a
    // logic bug worth surfacing instead of swallowing.
    fn sub(name: &str, field: &mut usize) {
        if *field == 0 {
            tracing::warn!(
                target: "forge_workspace::mcp::peers::facade",
                counter = name,
                "peer stats underflow - Minus1 without matching Plus1 (bookkeeping bug)",
            );
        } else {
            *field -= 1;
        }
    }
    match delta {
        PeerStatsDelta::OutgoingPlus1 => stats.outgoing = stats.outgoing.saturating_add(1),
        PeerStatsDelta::OutgoingMinus1 => sub("outgoing", &mut stats.outgoing),
        PeerStatsDelta::IncomingPlus1 => stats.incoming = stats.incoming.saturating_add(1),
        PeerStatsDelta::IncomingMinus1 => sub("incoming", &mut stats.incoming),
        PeerStatsDelta::DeliveryFailedPlus1 => {
            stats.delivery_failed = stats.delivery_failed.saturating_add(1);
        }
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
    /// Captured calls to `bump_inflight_stats`.
    pub bump_calls: parking_lot::Mutex<Vec<(SessionSlot, PeerStatsDelta)>>,
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
    ) -> Result<TargetStatus, DeliverError> {
        if let Some(err) = self.force_deliver_error.lock().clone() {
            return Err(err);
        }
        let known = self.peers.lock().iter().any(|p| p.name == target_project);
        if !known {
            return Err(DeliverError::UnknownTarget { name: target_project.to_owned() });
        }
        let target_status = self.peers.lock().iter().find(|p| p.name == target_project).map_or(
            TargetStatus::QueuedForSpawn,
            |p| match p.status {
                PeerLiveness::Running => TargetStatus::Delivered,
                PeerLiveness::Sleeping => TargetStatus::QueuedForSpawn,
            },
        );
        self.deliver_calls.lock().push((caller.clone(), target_project.to_owned(), wrapped));
        Ok(target_status)
    }

    fn bump_inflight_stats(&self, key: &SessionSlot, delta: PeerStatsDelta) {
        self.bump_calls.lock().push((key.clone(), delta));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::peers::types::{CorrelationId, WrappedKind};
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
            in_flight_incoming: 0,
            in_flight_outgoing: 0,
            spawned_at: None,
        }
    }

    fn fake_wrapped() -> WrappedPrompt {
        WrappedPrompt {
            correlation_id: CorrelationId::new_ask(),
            kind: WrappedKind::Question,
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
    fn mock_deliver_running_target_returns_delivered() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Running));
        let caller = fake_key("alpha");
        let result = mock.deliver_peer_prompt(&caller, "beta", fake_wrapped());
        assert_eq!(result, Ok(TargetStatus::Delivered));
        assert_eq!(mock.deliver_calls.lock().len(), 1);
    }

    #[test]
    fn mock_deliver_sleeping_target_returns_queued() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Sleeping));
        let caller = fake_key("alpha");
        let result = mock.deliver_peer_prompt(&caller, "beta", fake_wrapped());
        assert_eq!(result, Ok(TargetStatus::QueuedForSpawn));
    }

    #[test]
    fn mock_bump_stats_captures_calls() {
        let mock = MockWorkspaceFacade::new();
        let key = fake_key("alpha");
        mock.bump_inflight_stats(&key, PeerStatsDelta::OutgoingPlus1);
        mock.bump_inflight_stats(&key, PeerStatsDelta::OutgoingMinus1);
        let calls = mock.bump_calls.lock();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].1, PeerStatsDelta::OutgoingPlus1);
        assert_eq!(calls[1].1, PeerStatsDelta::OutgoingMinus1);
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
    fn apply_delta_saturates() {
        let mut stats = PeerInflightStats::default();
        apply_delta(&mut stats, PeerStatsDelta::OutgoingMinus1);
        assert_eq!(stats.outgoing, 0, "underflow should saturate at 0");
        apply_delta(&mut stats, PeerStatsDelta::OutgoingPlus1);
        apply_delta(&mut stats, PeerStatsDelta::OutgoingPlus1);
        assert_eq!(stats.outgoing, 2);
        apply_delta(&mut stats, PeerStatsDelta::DeliveryFailedPlus1);
        assert_eq!(stats.delivery_failed, 1);
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
    use crate::{CorrelationId, SessionSlot, WorkerEntry, WrappedPrompt};
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
            correlation_id: CorrelationId::new_ask(),
            kind: WrappedKind::Question,
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

    /// #298 Cause 1: workers can call `peers__whoami` and see their
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
