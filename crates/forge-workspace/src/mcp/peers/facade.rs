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

use std::sync::Arc;

use forge_primitives::{
    CorrelationId, InflightAsk, PeerFailureReason, PeerInflightStats, PeerLiveness, PeerStatus,
    WrappedPrompt,
};
use tracing::warn;

use crate::SessionKey;
use crate::domain_session::DomainSession;
use crate::protocol::{Command, SessionUpdate};
use crate::workspace::Workspace;

/// Snapshot the caller's current [`SessionKey`] on demand.
///
/// Each session's peer-MCP tools hold a `CallerKeyResolver` instead of
/// a bare `SessionKey` because the session's key isn't stable — it
/// rekeys from a synthetic placeholder (e.g. `__spawn_forge__`) to the
/// real claude-issued UUID once `Connected` fires
/// ([`Workspace::migrate_session_task`]). Tools that baked the
/// synthetic key in at server-build time would see stale lookups
/// after the rekey.
///
/// Production resolver reads from `DomainSession.key` via the
/// session's shared `Arc<Mutex<DomainSession>>`. The migrate path
/// updates `DomainSession.key` in place, so the resolver always
/// returns the current key.
///
/// Test resolvers can be any closure (typically returning a fixed
/// fake key).
#[derive(Clone)]
pub struct CallerKeyResolver(Arc<dyn Fn() -> SessionKey + Send + Sync>);

impl std::fmt::Debug for CallerKeyResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallerKeyResolver").finish_non_exhaustive()
    }
}

impl CallerKeyResolver {
    /// Build a resolver that reads `DomainSession.key` through the
    /// shared `Arc<Mutex<DomainSession>>`. Use this in production
    /// (the spawn path).
    pub fn from_domain(domain: Arc<parking_lot::Mutex<DomainSession>>) -> Self {
        Self(Arc::new(move || domain.lock().key.clone()))
    }

    /// Build a resolver that returns a fixed `SessionKey`. Use this
    /// in tests where the session never rekeys.
    pub fn from_fixed(key: SessionKey) -> Self {
        Self(Arc::new(move || key.clone()))
    }

    /// Resolve the caller's current `SessionKey`. Cheap (one mutex
    /// acquire over a `String`).
    pub fn current(&self) -> SessionKey {
        (self.0)()
    }
}

/// What `deliver_peer_prompt` returns on success — whether the target
/// session was already running (prompt sent immediately) or asleep
/// (workspace dispatched a SpawnProject and buffered the prompt for
/// delivery once Connected fires).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetStatus {
    /// Target was running; the wrapped prompt has been dispatched via
    /// the workspace command bus and will land in the next turn.
    Delivered,
    /// Target was sleeping; a `Command::SpawnProject` is in flight and
    /// the wrapped prompt is buffered in target's `pending_peer_prompts`
    /// for delivery on `AgentEvent::Connected` (drained in C11).
    QueuedForSpawn,
}

/// Why a `deliver_peer_prompt` call failed.
///
/// `UnknownTarget` and `HopLimitExceeded` fire synchronously inside
/// `deliver_peer_prompt` and the calling tool maps both to
/// `is_error: true` on the MCP response.
///
/// `DeliveryFailed` is for the async case (the dispatch returned ok
/// but later delivery hit a problem — target's session task closed,
/// channel errored on send). The actual async detection happens in
/// the workspace command bus path and surfaces via a separate
/// `SessionUpdate::PeerAskFailed` rather than a return value here.
/// Wired in C13.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DeliverError {
    /// No project named `name` in forge.toml.
    UnknownTarget { name: String },
    /// Outgoing hop exceeds the limit (default 10 — see #114 v1
    /// brainstorm). The chain stops here; tool returns `is_error`.
    HopLimitExceeded { hop: u8, limit: u8 },
    /// Async delivery failure. Currently never returned synchronously
    /// (kept here for symmetry; surfaces via
    /// `SessionUpdate::PeerAskFailed` instead).
    DeliveryFailed { reason: PeerFailureReason },
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
    TimedOutPlus1,
    DeliveryFailedPlus1,
}

/// The narrow workspace-state surface peer-coordination tools call
/// into. See module docs for design rationale.
pub trait WorkspaceFacade: Send + Sync {
    /// Snapshot of every configured project's peer status, in
    /// forge.toml declaration order. Computed fresh on each call.
    fn list_peers(&self) -> Vec<PeerStatus>;

    /// Identity of the calling session. Returns `None` when `caller`
    /// doesn't resolve to any known project (defensive — the tools
    /// closure-bind a real key at spawn time, so this should be
    /// `Some` in practice).
    fn whoami(&self, caller: &SessionKey) -> Option<PeerStatus>;

    /// Deliver a wrapped peer prompt to `target_project`.
    ///
    /// Synchronous return is the immediate decision:
    /// - `Ok(Delivered)` — target is running; `Command::DeliverPeerPrompt`
    ///   has been dispatched and will land as a `Command::Prompt` on
    ///   target's SessionTask in the next dispatch cycle.
    /// - `Ok(QueuedForSpawn)` — target is sleeping; a
    ///   `Command::SpawnProject` is in flight and the wrapped prompt
    ///   is buffered in target's `pending_peer_prompts` for delivery
    ///   on `AgentEvent::Connected`.
    /// - `Err(UnknownTarget)` — target not in forge.toml.
    /// - `Err(HopLimitExceeded)` — `wrapped.hop > wrapped.hop_limit`.
    ///
    /// The actual buffer + dispatch logic lives in `spawn.rs`'s
    /// `Command::DeliverPeerPrompt` handler (lands in C11).
    fn deliver_peer_prompt(
        &self,
        caller: &SessionKey,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<TargetStatus, DeliverError>;

    /// Register an outgoing ask in the workspace's `inflight_asks`
    /// map. The 30-min timer that expires this entry is armed in C12.
    fn register_inflight_ask(&self, ask: InflightAsk);

    /// Look up an `InflightAsk` by correlation_id. Used by `tell_agent`
    /// to classify replies (Pending → Reply, TimedOut → LateReply,
    /// not-found → Message).
    fn resolve_correlation(&self, id: &CorrelationId) -> Option<InflightAsk>;

    /// Read the caller's ambient `current_inbound_hop` from its
    /// DomainSession. Returns `Some(hop)` when the caller's LLM is
    /// processing a peer-wrapped prompt (set by C11's
    /// `Command::DeliverPeerPrompt` handler before dispatch); `None`
    /// for user-initiated turns. Tools use this to stamp `hop = N+1`
    /// on outgoing forwarded messages without the LLM having to pass
    /// it as an arg.
    fn peek_current_inbound_hop(&self, caller: &SessionKey) -> Option<u8>;

    /// Apply a delta to `peer_stats[key]` and emit
    /// `SessionUpdate::PeerInflightStatsChanged` so the TUI reducer
    /// can update the sidebar peer-activity badge.
    fn bump_inflight_stats(&self, key: &SessionKey, delta: PeerStatsDelta);
}

/// Production impl. Trait is implemented on `Arc<Workspace>` (not
/// plain `Workspace`) because `Workspace::dispatch` takes
/// `self: &Arc<Self>` — it internally `Arc::clone`s self for spawned
/// SessionTasks. The tools hold an `Arc<dyn WorkspaceFacade>` built
/// from `Arc::new(MyArc) as Arc<dyn WorkspaceFacade>` via the
/// `build_server` factory (lands in C5).
impl WorkspaceFacade for Arc<Workspace> {
    fn list_peers(&self) -> Vec<PeerStatus> {
        let projects = self.list_projects();
        let stat_counters = self.peer_stats.lock();
        projects
            .into_iter()
            .map(|view| {
                // Lead session = first entry. `is_open == true` means
                // an Agent is in the workspace pool for that session.
                let lead = view.sessions.first();
                let liveness = lead.map_or(PeerLiveness::Sleeping, |s| {
                    if s.is_open { PeerLiveness::Running } else { PeerLiveness::Sleeping }
                });
                let counts =
                    lead.and_then(|s| stat_counters.get(&s.session)).cloned().unwrap_or_default();
                let spawned_at = lead.filter(|s| s.is_open).and_then(|s| s.last_activity);
                PeerStatus {
                    name: view.name,
                    org: view.org,
                    path: view.path,
                    status: liveness,
                    // v1: model is not yet plumbed through to the
                    // facade (DomainSession doesn't carry it). Leave
                    // None until #114 follow-on wires it.
                    model: None,
                    in_flight_incoming: counts.incoming,
                    in_flight_outgoing: counts.outgoing,
                    spawned_at,
                }
            })
            .collect()
    }

    fn whoami(&self, caller: &SessionKey) -> Option<PeerStatus> {
        self.list_peers().into_iter().find(|p| {
            // Match by looking up which project has a session matching
            // the caller key. `list_peers` already filtered to projects
            // in forge.toml; we just need to find the one whose
            // lead-session key matches.
            self.list_projects()
                .iter()
                .find(|v| v.name == p.name)
                .and_then(|v| v.sessions.first())
                .is_some_and(|s| s.session == *caller)
        })
    }

    fn deliver_peer_prompt(
        &self,
        caller: &SessionKey,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<TargetStatus, DeliverError> {
        // Hop limit check first — cheapest; doesn't need a project
        // lookup.
        if wrapped.hop > wrapped.hop_limit {
            return Err(DeliverError::HopLimitExceeded {
                hop: wrapped.hop,
                limit: wrapped.hop_limit,
            });
        }

        // Resolve the target project.
        let project = self
            .list_projects()
            .into_iter()
            .find(|v| v.name == target_project)
            .ok_or_else(|| DeliverError::UnknownTarget { name: target_project.to_owned() })?;

        // Decide immediate-vs-queued based on lead session liveness.
        let target_status = project
            .sessions
            .first()
            .filter(|s| s.is_open)
            .map_or(TargetStatus::QueuedForSpawn, |_| TargetStatus::Delivered);

        // Dispatch the workspace command. The actual delivery logic
        // (stamp current_inbound_hop, dispatch Command::Prompt or
        // buffer-and-SpawnProject) lives in spawn.rs's handler — that
        // lands in C11. Until then, the dispatch falls through the
        // App-level `other =>` arm in `Workspace::dispatch` and
        // logs a warn; the tool still returns the right
        // immediate-decision value to the LLM.
        if let Err(err) = self.dispatch(Command::DeliverPeerPrompt {
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

    fn register_inflight_ask(&self, ask: InflightAsk) {
        let id = ask.correlation_id.clone();
        self.inflight_asks.lock().insert(id.clone(), ask);
        // 30-min timer per #114 v1 brainstorm. On expiry the timer
        // fires Workspace::expire_inflight_ask which:
        //   1. Marks the ask TimedOut
        //   2. Removes from the inflight_asks + inflight_timers maps
        //   3. Bumps caller's TimedOutPlus1 + OutgoingMinus1 stats
        //   4. Emits SessionUpdate::PeerAskTimedOut for UI badges
        //   5. Dispatches dual-path Command::Prompt notifications
        //      (CallerTimeoutNotice to the caller; RecipientExpiredNotice
        //      to the recipient, when its session is still alive)
        //
        // The timer holds a Weak<Workspace> to avoid a cycle. When
        // the workspace is dropped before the timer fires the upgrade
        // returns None and the timer exits silently.
        let weak = Arc::downgrade(self);
        let id_for_timer = id.clone();
        let timer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(30 * 60)).await;
            if let Some(workspace) = weak.upgrade() {
                workspace.expire_inflight_ask(&id_for_timer);
            }
        });
        self.inflight_timers.lock().insert(id, timer);
    }

    fn resolve_correlation(&self, id: &CorrelationId) -> Option<InflightAsk> {
        self.inflight_asks.lock().get(id).cloned()
    }

    fn peek_current_inbound_hop(&self, caller: &SessionKey) -> Option<u8> {
        let handles = self.domain_handles.lock();
        let domain = handles.get(caller)?;
        let guard = domain.lock();
        guard.current_inbound_hop
    }

    fn bump_inflight_stats(&self, key: &SessionKey, delta: PeerStatsDelta) {
        let stats_snapshot = {
            let mut stats = self.peer_stats.lock();
            let entry = stats.entry(key.clone()).or_default();
            apply_delta(entry, delta);
            entry.clone()
        };
        // Best-effort emit; receiver is closed only during shutdown.
        let _ = self.update_sender().send(SessionUpdate::PeerInflightStatsChanged {
            key: key.clone(),
            stats: stats_snapshot,
        });
    }
}

fn apply_delta(stats: &mut PeerInflightStats, delta: PeerStatsDelta) {
    match delta {
        PeerStatsDelta::OutgoingPlus1 => stats.outgoing = stats.outgoing.saturating_add(1),
        PeerStatsDelta::OutgoingMinus1 => stats.outgoing = stats.outgoing.saturating_sub(1),
        PeerStatsDelta::IncomingPlus1 => stats.incoming = stats.incoming.saturating_add(1),
        PeerStatsDelta::IncomingMinus1 => stats.incoming = stats.incoming.saturating_sub(1),
        PeerStatsDelta::TimedOutPlus1 => stats.timed_out = stats.timed_out.saturating_add(1),
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
    /// Pre-loaded inbound-hop value `peek_current_inbound_hop` returns.
    pub current_inbound_hop: parking_lot::Mutex<Option<u8>>,
    /// Captured calls to `deliver_peer_prompt`.
    pub deliver_calls: parking_lot::Mutex<Vec<(SessionKey, String, WrappedPrompt)>>,
    /// Captured calls to `register_inflight_ask`.
    pub register_calls: parking_lot::Mutex<Vec<InflightAsk>>,
    /// Captured calls to `bump_inflight_stats`.
    pub bump_calls: parking_lot::Mutex<Vec<(SessionKey, PeerStatsDelta)>>,
    /// Pre-loaded `InflightAsk`s that `resolve_correlation` may return.
    pub inflight: parking_lot::Mutex<std::collections::HashMap<CorrelationId, InflightAsk>>,
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

    fn whoami(&self, caller: &SessionKey) -> Option<PeerStatus> {
        // Mock's `whoami` does the same "find by caller's lead session"
        // shape as the prod impl, but works against the mock's
        // pre-loaded peers list. Tests that want a specific identity
        // pre-load the peers with an entry whose name matches their
        // caller key convention.
        self.peers.lock().iter().find(|p| p.name == caller.as_str()).cloned()
    }

    fn deliver_peer_prompt(
        &self,
        caller: &SessionKey,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<TargetStatus, DeliverError> {
        if let Some(err) = self.force_deliver_error.lock().clone() {
            return Err(err);
        }
        if wrapped.hop > wrapped.hop_limit {
            return Err(DeliverError::HopLimitExceeded {
                hop: wrapped.hop,
                limit: wrapped.hop_limit,
            });
        }
        let known = self.peers.lock().iter().any(|p| p.name == target_project);
        if !known {
            return Err(DeliverError::UnknownTarget { name: target_project.to_owned() });
        }
        let target_status = self.peers.lock().iter().find(|p| p.name == target_project).map_or(
            TargetStatus::QueuedForSpawn,
            |p| match p.status {
                PeerLiveness::Running => TargetStatus::Delivered,
                PeerLiveness::Sleeping | PeerLiveness::Failed => TargetStatus::QueuedForSpawn,
            },
        );
        self.deliver_calls.lock().push((caller.clone(), target_project.to_owned(), wrapped));
        Ok(target_status)
    }

    fn register_inflight_ask(&self, ask: InflightAsk) {
        self.inflight.lock().insert(ask.correlation_id.clone(), ask.clone());
        self.register_calls.lock().push(ask);
    }

    fn resolve_correlation(&self, id: &CorrelationId) -> Option<InflightAsk> {
        self.inflight.lock().get(id).cloned()
    }

    fn peek_current_inbound_hop(&self, _caller: &SessionKey) -> Option<u8> {
        *self.current_inbound_hop.lock()
    }

    fn bump_inflight_stats(&self, key: &SessionKey, delta: PeerStatsDelta) {
        self.bump_calls.lock().push((key.clone(), delta));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::WrappedKind;
    use std::path::PathBuf;
    use std::time::SystemTime;

    fn fake_key(s: &str) -> SessionKey {
        // Tests use the production constructor (no test-helpers feature
        // here) — SessionKey is just a String newtype, so this aligns
        // with how the workspace itself constructs keys at runtime.
        SessionKey::from_session_id(s)
    }

    fn fake_peer(name: &str, liveness: PeerLiveness) -> PeerStatus {
        PeerStatus {
            name: name.to_owned(),
            org: "TestOrg".to_owned(),
            path: PathBuf::from(format!("/tmp/{name}")),
            status: liveness,
            model: None,
            in_flight_incoming: 0,
            in_flight_outgoing: 0,
            spawned_at: None,
        }
    }

    fn fake_wrapped(hop: u8, hop_limit: u8) -> WrappedPrompt {
        WrappedPrompt {
            correlation_id: CorrelationId::new_ask(),
            kind: WrappedKind::Question,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            hop,
            hop_limit,
            in_reply_to: None,
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
        let result = mock.deliver_peer_prompt(&caller, "missing", fake_wrapped(1, 10));
        assert!(
            matches!(result, Err(DeliverError::UnknownTarget { ref name }) if name == "missing")
        );
    }

    #[test]
    fn mock_deliver_hop_limit_errors() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Running));
        let caller = fake_key("alpha");
        let result = mock.deliver_peer_prompt(&caller, "beta", fake_wrapped(11, 10));
        assert!(
            matches!(result, Err(DeliverError::HopLimitExceeded { hop: 11, limit: 10 })),
            "hop>limit must error: {result:?}",
        );
    }

    #[test]
    fn mock_deliver_running_target_returns_delivered() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Running));
        let caller = fake_key("alpha");
        let result = mock.deliver_peer_prompt(&caller, "beta", fake_wrapped(1, 10));
        assert_eq!(result, Ok(TargetStatus::Delivered));
        assert_eq!(mock.deliver_calls.lock().len(), 1);
    }

    #[test]
    fn mock_deliver_sleeping_target_returns_queued() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Sleeping));
        let caller = fake_key("alpha");
        let result = mock.deliver_peer_prompt(&caller, "beta", fake_wrapped(1, 10));
        assert_eq!(result, Ok(TargetStatus::QueuedForSpawn));
    }

    #[test]
    fn mock_register_and_resolve_correlation_round_trips() {
        let mock = MockWorkspaceFacade::new();
        let ask = InflightAsk {
            correlation_id: CorrelationId("q-deadbeef".to_owned()),
            caller: fake_key("alpha"),
            caller_project: "alpha".to_owned(),
            caller_org: "Test".to_owned(),
            target_project: "beta".to_owned(),
            queued_at: SystemTime::UNIX_EPOCH,
            timeout_at: SystemTime::UNIX_EPOCH,
            hop: 1,
            hop_limit: 10,
            status: forge_primitives::InflightStatus::Pending,
        };
        mock.register_inflight_ask(ask.clone());
        let back = mock.resolve_correlation(&ask.correlation_id);
        assert!(back.is_some());
        assert_eq!(back.unwrap().target_project, "beta");
    }

    #[test]
    fn mock_resolve_correlation_returns_none_for_unknown_id() {
        let mock = MockWorkspaceFacade::new();
        assert!(mock.resolve_correlation(&CorrelationId("q-00000000".to_owned())).is_none());
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
    fn mock_peek_current_inbound_hop_returns_preloaded() {
        let mock = MockWorkspaceFacade::new();
        *mock.current_inbound_hop.lock() = Some(3);
        assert_eq!(mock.peek_current_inbound_hop(&fake_key("alpha")), Some(3));
    }

    #[test]
    fn mock_whoami_matches_by_name() {
        let mock = MockWorkspaceFacade::new();
        mock.peers.lock().push(fake_peer("alpha", PeerLiveness::Running));
        mock.peers.lock().push(fake_peer("beta", PeerLiveness::Sleeping));
        // Convention in the mock: caller key string == project name.
        // The prod impl matches by SessionKey ↔ lead-session lookup.
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
            Some(DeliverError::DeliveryFailed { reason: PeerFailureReason::ChannelClosed });
        let caller = fake_key("alpha");
        let result = mock.deliver_peer_prompt(&caller, "beta", fake_wrapped(1, 10));
        assert!(matches!(
            result,
            Err(DeliverError::DeliveryFailed { reason: PeerFailureReason::ChannelClosed })
        ));
    }
}
