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

use crate::mcp::peers::types::{
    CorrelationId, InflightAsk, PeerLiveness, PeerStatus, WrappedPrompt,
};
use tracing::warn;

use crate::SessionKey;
use crate::domain_session::DomainSession;
use crate::protocol::{Command, SessionUpdate};
use crate::workspace::Workspace;

/// Snapshot the caller's current [`SessionKey`] on demand.
///
/// Each session's peer-MCP tools hold a `CallerKeyResolver` instead of
/// a bare `SessionKey` because the session's key isn't stable - it
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
pub struct CallerKeyResolver(Arc<dyn Fn() -> Result<SessionKey, ResolverDetached> + Send + Sync>);

/// Returned by [`CallerKeyResolver::current`] when the underlying
/// `DomainSession` has been dropped (typically: workspace shutdown
/// happening concurrently with a peer/worker tool invocation). The
/// Tool impl should surface this as an `is_error` tool response -
/// the recipient session is dying, the LLM call won't have anywhere
/// to land anyway. Replaces the prior `__detached__` SessionKey
/// sentinel which forced every consumer to compare against a magic
/// string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolverDetached;

impl std::fmt::Display for ResolverDetached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("caller session is detached (DomainSession dropped)")
    }
}

impl std::error::Error for ResolverDetached {}

impl std::fmt::Debug for CallerKeyResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CallerKeyResolver").finish_non_exhaustive()
    }
}

impl CallerKeyResolver {
    /// Build a resolver that reads `DomainSession.key` through a
    /// `Weak<Mutex<DomainSession>>`. Use this in production (the
    /// spawn path).
    ///
    /// **Weak intentionally**: the DomainSession's `conn` field
    /// holds an `Arc<AgentHandle>`, and the AgentHandle owns the
    /// `ForgeSdkBridge` which owns the McpServer (extra_mcp_servers)
    /// whose Tool impls hold *this* resolver. Holding the
    /// DomainSession strongly here would close a cycle:
    /// AgentHandle → bridge → tools → resolver → DomainSession.conn
    /// → AgentHandle. Drop never fires. With Weak, the cycle is
    /// broken at the resolver edge: when the workspace drops its
    /// strong reference (via `domain_handles.drain()` in
    /// `Workspace::shutdown` and per-session in `release_session`),
    /// the inner Arc count hits 1 (the cloned strong handle held by
    /// the bridge's domain reference is the only remaining one) and
    /// then 0 when the bridge drops, breaking the cycle cleanly.
    ///
    /// If the DomainSession gets dropped before a tool fires (e.g.
    /// the workspace is shutting down concurrently with a peer tool
    /// invocation), `current()` returns `Err(ResolverDetached)`.
    /// Tools handle this by returning a tool-level error so the LLM
    /// sees the failure cleanly rather than silently routing against
    /// a synthetic sentinel SessionKey.
    pub fn from_domain(domain: &Arc<parking_lot::Mutex<DomainSession>>) -> Self {
        let weak = Arc::downgrade(domain);
        Self(Arc::new(move || weak.upgrade().map(|d| d.lock().key.clone()).ok_or(ResolverDetached)))
    }

    /// Build a resolver that returns a fixed `SessionKey`. Use this
    /// in tests where the session never rekeys.
    #[cfg(any(test, feature = "testing"))]
    pub fn from_fixed(key: SessionKey) -> Self {
        Self(Arc::new(move || Ok(key.clone())))
    }

    /// Resolve the caller's current `SessionKey`. Returns
    /// `Err(ResolverDetached)` when the underlying `DomainSession`
    /// has been dropped (workspace shutdown race).
    pub fn current(&self) -> Result<SessionKey, ResolverDetached> {
        (self.0)()
    }
}

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
    /// the wrapped prompt is buffered in target's `pending_peer_prompts`
    /// for delivery on `AgentEvent::Connected` (drained in C11).
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
    /// Outgoing hop exceeds the limit (default 10 - see #114 v1
    /// brainstorm). The chain stops here; tool returns `is_error`.
    HopLimitExceeded { hop: u8, limit: u8 },
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
    fn whoami(&self, caller: &SessionKey) -> Option<PeerStatus>;

    /// Deliver a wrapped peer prompt to `target_project`.
    ///
    /// Synchronous return is the immediate decision:
    /// - `Ok(Delivered)` - target is running; `Command::DeliverPeerPrompt`
    ///   has been dispatched and will land as a `Command::Prompt` on
    ///   target's SessionTask in the next dispatch cycle.
    /// - `Ok(QueuedForSpawn)` - target is sleeping; a
    ///   `Command::SpawnProject` is in flight and the wrapped prompt
    ///   is buffered in target's `pending_peer_prompts` for delivery
    ///   on `AgentEvent::Connected`.
    /// - `Err(UnknownTarget)` - target not in forge.toml.
    /// - `Err(HopLimitExceeded)` - `wrapped.hop > wrapped.hop_limit`.
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
    /// map. Expired only when the target session is lost
    /// (`expire_target_inflight`) or the reply lands.
    fn register_inflight_ask(&self, ask: InflightAsk);

    /// Look up an `InflightAsk` by correlation_id. Used by `tell_agent`
    /// to classify replies (found → Reply, not-found → Message).
    /// Read-only - does NOT remove the ask from the inflight map.
    fn resolve_correlation(&self, id: &CorrelationId) -> Option<InflightAsk>;

    /// Remove an `InflightAsk` from the inflight map. Called by
    /// `tell_agent` after a successful Reply dispatch. Returns the
    /// removed ask so the caller can inspect status / caller / etc.,
    /// or `None` when the entry was already gone.
    fn complete_inflight_ask(&self, id: &CorrelationId) -> Option<InflightAsk>;

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
/// `Connected` lands and the catalog indexes them; without filtering,
/// a spawned worker can shadow the lead at position 0 and break peer
/// caller resolution (whoami / list_peers) plus peer delivery target
/// resolution. The workers MCP's own `caller_project` uses the same
/// "skip live_workers" gate; pulling it inline here keeps the two
/// paths consistent.
fn lead_session_view<'a>(
    ws: &crate::workspace::Workspace,
    view: &'a crate::views::ProjectView,
) -> Option<&'a crate::views::SessionView> {
    let live_keys: std::collections::HashSet<_> =
        ws.list_live_workers(&view.key).into_iter().map(|w| w.session_key).collect();
    view.sessions.iter().find(|s| !live_keys.contains(&s.session))
}

impl WorkspaceFacade for ProdWorkspaceFacade {
    fn list_peers(&self) -> Vec<PeerStatus> {
        let Some(ws) = self.0.upgrade() else { return Vec::new() };
        let projects = ws.list_projects();
        let stat_counters = ws.peer_stats.lock();
        projects
            .into_iter()
            .map(|view| {
                let lead = lead_session_view(&ws, &view);
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
                    in_flight_incoming: counts.incoming,
                    in_flight_outgoing: counts.outgoing,
                    spawned_at,
                }
            })
            .collect()
    }

    fn whoami(&self, caller: &SessionKey) -> Option<PeerStatus> {
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
        let (status, spawned_at, counts) = match cx.lead_session_view.as_ref() {
            Some(lead) => {
                let counts = stat_counters.get(&lead.session).cloned().unwrap_or_default();
                let status =
                    if lead.is_open { PeerLiveness::Running } else { PeerLiveness::Sleeping };
                let spawned_at = if lead.is_open { lead.last_activity } else { None };
                (status, spawned_at, counts)
            }
            None => (PeerLiveness::Sleeping, None, PeerInflightStats::default()),
        };
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
        caller: &SessionKey,
        target_project: &str,
        wrapped: WrappedPrompt,
    ) -> Result<TargetStatus, DeliverError> {
        if wrapped.hop > wrapped.hop_limit {
            return Err(DeliverError::HopLimitExceeded {
                hop: wrapped.hop,
                limit: wrapped.hop_limit,
            });
        }
        let Some(ws) = self.0.upgrade() else {
            return Err(DeliverError::UnknownTarget { name: target_project.to_owned() });
        };
        let project = ws
            .list_projects()
            .into_iter()
            .find(|v| v.name == target_project)
            .ok_or_else(|| DeliverError::UnknownTarget { name: target_project.to_owned() })?;
        // Skip worker sessions when probing the target project's
        // lead - workers can shadow `sessions[0]` once they connect.
        // `lead_session_view` mirrors the workers MCP gate.
        let target_status = lead_session_view(&ws, &project)
            .filter(|s| s.is_open)
            .map_or(TargetStatus::QueuedForSpawn, |_| TargetStatus::Delivered);
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

    fn register_inflight_ask(&self, ask: InflightAsk) {
        let Some(ws) = self.0.upgrade() else { return };
        let id = ask.correlation_id.clone();
        let prev = ws.inflight_asks.lock().insert(id.clone(), ask);
        if prev.is_some() {
            tracing::warn!(
                target: "forge_workspace::mcp::peers::facade",
                correlation_id = %id,
                "register_inflight_ask: collision on correlation id - prior ask overwritten",
            );
        }
    }

    fn resolve_correlation(&self, id: &CorrelationId) -> Option<InflightAsk> {
        let ws = self.0.upgrade()?;
        ws.inflight_asks.lock().get(id).cloned()
    }

    fn complete_inflight_ask(&self, id: &CorrelationId) -> Option<InflightAsk> {
        let ws = self.0.upgrade()?;
        ws.inflight_asks.lock().remove(id)
    }

    fn peek_current_inbound_hop(&self, caller: &SessionKey) -> Option<u8> {
        let ws = self.0.upgrade()?;
        let handles = ws.domain_handles.lock();
        let domain = handles.get(caller)?;
        let guard = domain.lock();
        guard.current_inbound_hop
    }

    fn bump_inflight_stats(&self, key: &SessionKey, delta: PeerStatsDelta) {
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
#[cfg(test)]
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
    /// Captured calls to `complete_inflight_ask`.
    pub complete_calls: parking_lot::Mutex<Vec<CorrelationId>>,
    /// Captured calls to `bump_inflight_stats`.
    pub bump_calls: parking_lot::Mutex<Vec<(SessionKey, PeerStatsDelta)>>,
    /// Pre-loaded `InflightAsk`s that `resolve_correlation` may return.
    pub inflight: parking_lot::Mutex<std::collections::HashMap<CorrelationId, InflightAsk>>,
    /// If set, `deliver_peer_prompt` returns this error instead of
    /// running the normal lookup path. Lets tests force-test the
    /// failure surface.
    pub force_deliver_error: parking_lot::Mutex<Option<DeliverError>>,
}

#[cfg(test)]
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

#[cfg(test)]
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
                PeerLiveness::Sleeping => TargetStatus::QueuedForSpawn,
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

    fn complete_inflight_ask(&self, id: &CorrelationId) -> Option<InflightAsk> {
        self.complete_calls.lock().push(id.clone());
        self.inflight.lock().remove(id)
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
    use crate::mcp::peers::types::WrappedKind;
    use std::path::PathBuf;

    fn fake_key(s: &str) -> SessionKey {
        // Tests use the production constructor (no test-helpers feature
        // here) - SessionKey is just a String newtype, so this aligns
        // with how the workspace itself constructs keys at runtime.
        SessionKey::from_session_id(s)
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

    fn fake_wrapped(hop: u8, hop_limit: u8) -> WrappedPrompt {
        WrappedPrompt {
            correlation_id: CorrelationId::new_ask(),
            kind: WrappedKind::Question,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            hop,
            hop_limit,
            body: "hi".to_owned(),
        }
    }

    #[test]
    fn caller_key_from_fixed_returns_ok() {
        let resolver = CallerKeyResolver::from_fixed(fake_key("alpha"));
        let key = resolver.current().expect("from_fixed always resolves");
        assert_eq!(key.as_str(), "alpha");
    }

    #[test]
    fn caller_key_from_domain_returns_err_after_drop() {
        // Build a DomainSession-shaped Mutex, downgrade to Weak via
        // from_domain, drop the Arc, then probe current(). The
        // upgrade must fail and we must see ResolverDetached.
        let domain =
            Arc::new(parking_lot::Mutex::new(crate::DomainSession::new(fake_key("alpha"), None)));
        let resolver = CallerKeyResolver::from_domain(&domain);
        assert_eq!(resolver.current().map(|k| k.as_str().to_owned()), Ok("alpha".to_owned()));
        drop(domain);
        assert_eq!(resolver.current(), Err(ResolverDetached));
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
            target_project: "beta".to_owned(),
            target_session: None,
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
            Some(DeliverError::UnknownTarget { name: "forced".to_owned() });
        let caller = fake_key("alpha");
        let result = mock.deliver_peer_prompt(&caller, "beta", fake_wrapped(1, 10));
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
    use super::{DeliverError, ProdWorkspaceFacade, lead_session_view};
    use crate::mcp::peers::types::WrappedKind;
    use crate::target::ProjectKey;
    use crate::views::{ProjectView, SessionView};
    use crate::workspace::Workspace;
    use crate::{CorrelationId, SessionKey, WorkerEntry, WrappedPrompt};
    use forge_primitives::WorkerLiveness;
    use std::time::SystemTime;

    fn session(id: &str) -> SessionView {
        SessionView::new_for_test(SessionKey::from_session_id(id), id, true, None)
    }

    fn worker_entry(session_key: SessionKey) -> WorkerEntry {
        WorkerEntry {
            label: "reviewer".into(),
            charter: "review the diff".into(),
            session_key,
            status: WorkerLiveness::Running,
            spawned_at: SystemTime::UNIX_EPOCH,
            spawned_by_session_id: "lead-uuid".into(),
            needs_tag: false,
            is_git_repo_at_spawn: false,
            diagnostic: None,
            kick: None,
        }
    }

    #[test]
    fn live_worker_at_index_zero_does_not_shadow_lead() {
        // A just-connected worker can land at sessions[0]; the lead
        // must still resolve to the non-worker session.
        let (ws, _rx) = Workspace::testing_stub();
        let key = ProjectKey::new("p".to_owned());
        let worker = session("worker-uuid");
        let lead = session("lead-uuid");
        let view = ProjectView::new_for_test(
            key.clone(),
            "forge",
            "/tmp/forge",
            vec![worker.clone(), lead.clone()],
        );
        ws.insert_live_worker(&key, worker_entry(worker.session.clone()));
        let resolved = lead_session_view(&ws, &view).expect("a lead resolves");
        assert_eq!(
            resolved.session, lead.session,
            "the live worker at index 0 must not shadow the lead",
        );
    }

    #[test]
    fn returns_first_session_when_no_live_workers() {
        let (ws, _rx) = Workspace::testing_stub();
        let key = ProjectKey::new("p".to_owned());
        let lead = session("lead-uuid");
        let view = ProjectView::new_for_test(key, "forge", "/tmp/forge", vec![lead.clone()]);
        let resolved = lead_session_view(&ws, &view).expect("a lead resolves");
        assert_eq!(resolved.session, lead.session);
    }

    #[test]
    fn returns_none_when_every_session_is_a_live_worker() {
        // LeadGone: the lead disconnected and only workers remain in
        // the catalog. No non-worker session means no lead.
        let (ws, _rx) = Workspace::testing_stub();
        let key = ProjectKey::new("p".to_owned());
        let worker = session("worker-uuid");
        let view =
            ProjectView::new_for_test(key.clone(), "forge", "/tmp/forge", vec![worker.clone()]);
        ws.insert_live_worker(&key, worker_entry(worker.session.clone()));
        assert!(lead_session_view(&ws, &view).is_none(), "an all-worker project has no lead");
    }

    fn wrapped(hop: u8, hop_limit: u8) -> WrappedPrompt {
        WrappedPrompt {
            correlation_id: CorrelationId::new_ask(),
            kind: WrappedKind::Question,
            sender_name: "forge".to_owned(),
            sender_org: "Personal".to_owned(),
            hop,
            hop_limit,
            body: "hi".to_owned(),
        }
    }

    #[test]
    fn deliver_rejects_when_hop_exceeds_limit() {
        // The hop guard fires before any project lookup, so an empty
        // stub workspace exercises it.
        let (ws, _rx) = Workspace::testing_stub();
        let facade = ProdWorkspaceFacade::from_arc(&ws);
        let result = facade.deliver_peer_prompt(
            &SessionKey::from_session_id("caller"),
            "anywhere",
            wrapped(5, 3),
        );
        assert!(matches!(result, Err(DeliverError::HopLimitExceeded { hop: 5, limit: 3 })));
    }

    #[test]
    fn deliver_to_unknown_project_errors() {
        let (ws, _rx) = Workspace::testing_stub();
        let facade = ProdWorkspaceFacade::from_arc(&ws);
        let result = facade.deliver_peer_prompt(
            &SessionKey::from_session_id("caller"),
            "no-such-project",
            wrapped(1, 10),
        );
        assert!(
            matches!(result, Err(DeliverError::UnknownTarget { ref name }) if name == "no-such-project")
        );
    }

    #[test]
    fn whoami_none_when_caller_leads_no_project() {
        let (ws, _rx) = Workspace::testing_stub();
        let facade = ProdWorkspaceFacade::from_arc(&ws);
        assert!(facade.whoami(&SessionKey::from_session_id("nobody")).is_none());
    }

    /// #298 Cause 1: workers can call `peers__whoami` and see their
    /// project's peer identity. Pre-fix, the impl required the caller
    /// to be the lead session, which returned None for any worker.
    #[test]
    fn whoami_resolves_worker_caller_to_project_peer_identity() {
        let (ws, _rx) = Workspace::testing_stub();
        ws.seed_test_project_with_static_workers("myproj", "/tmp/myproj", &[]);
        ws.record_connected_session("/tmp/myproj", "lead-uuid", None);
        let pk = crate::ProjectKey::new(
            forge_agent::userdata::catalog::scan::project_key_for_directory(Some("/tmp/myproj")),
        );
        ws.insert_live_worker(&pk, worker_entry(SessionKey::from_session_id("worker-uuid")));

        let facade = ProdWorkspaceFacade::from_arc(&ws);
        let status = facade
            .whoami(&SessionKey::from_session_id("worker-uuid"))
            .expect("worker caller resolves to its project's peer identity");
        assert_eq!(status.name, "myproj");
        assert_eq!(status.org, "TestOrg");

        // Regression lock: the pre-existing lead-only path still
        // resolves to the same project identity.
        let lead_status = facade
            .whoami(&SessionKey::from_session_id("lead-uuid"))
            .expect("lead caller still resolves");
        assert_eq!(lead_status.name, "myproj");
    }
}
