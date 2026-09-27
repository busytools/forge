//! Workspace-side per-session state.
//!
//! Holds what workspace itself needs: routing metadata (`AgentHandle`
//! slot, claude-issued `session_id`), the pending-interaction mailbox,
//! and the facts every view reads through the view surface. The rest of
//! the operational state a view renders (lifecycle, cwd, account info)
//! lives on the view's own session record.
//!
//! Two classes are held here rather than folded per view. The turn
//! signal [`DomainSession::runtime_state`] is the one the workspace needs
//! authoritatively for its worker-liveness and prompt-interception
//! guards, and [`DomainSession::background_work`],
//! [`DomainSession::available_commands`] and
//! [`DomainSession::available_agents`] are held so a view arriving late
//! reads them from the core. The last two are an intermediate state: the
//! TUI folds its own copy until it is removed.

use std::collections::HashMap;
use std::sync::Arc;

use forge_agent::AgentHandle;
use forge_primitives::{AvailableAgent, AvailableCommand, RuntimeSessionState, SessionId};

use crate::SessionSlot;
use crate::protocol::PendingInteractionSlot;

/// Workspace's owned per-session state. One `DomainSession` per
/// active `SessionTask`. Single writer (the `SessionTask`); accessed
/// via `Arc<parking_lot::Mutex<DomainSession>>` so the `Workspace`
/// can route commands without locking the whole pool.
pub struct DomainSession {
    pub key: SessionSlot,
    /// Claude-issued session UUID. `None` until the first `Connected`
    /// event from this session's bridge. Workspace consults this when
    /// dispatching `AgentHandle` calls that route by session id.
    pub session_id: Option<SessionId>,
    /// Agent connection handle bound to this session at spawn time.
    /// `None` for pre-spawn / pre-Connect domains (forge-tui's
    /// `connect::create_app` registers a placeholder handle-less
    /// domain so the spawn handler can fill it in later).
    pub conn: Option<Arc<AgentHandle>>,
    /// Pending permission/question/elicitation oneshots indexed by the
    /// wire `tool_id` / `elicitation_id`. `SessionTask` pops on
    /// `Respond*` commands; bridge inserts on every `*Request` event.
    pub pending_interactions: HashMap<String, PendingInteractionSlot>,
    /// `--new` boot-wave flag, stamped at spawn time from
    /// `SessionLaunchSettings.force_new`. For a project lead it makes
    /// the Connected-time respawn skip the store lookup
    /// (`resume_existing = None` for every worker), so they come
    /// up fresh alongside their fresh lead. `false` for every non-boot
    /// spawn.
    pub spawned_force_new: bool,
    /// Whether the spawn that opened this session wrote the worker's
    /// durable row rather than adopting one that was already there.
    /// Stamped at spawn time from the spawn's role, and read by the
    /// tag-write rollback to decide whether the row goes with the spawn
    /// it is discarding. `false` for every lead and every non-worker.
    pub spawn_wrote_row: bool,
    /// Latest runtime liveness mirrored from the session's
    /// `session_state_changed` wire messages. Operational turn state
    /// otherwise lives on the TUI's `UiSession`; this one signal is
    /// duplicated here so the workspace's worker-liveness and
    /// prompt-interception guards can tell an in-flight turn from an
    /// idle one (the TUI's own gate alone can race a just-delivered
    /// peer / cron / gotify / slack prompt). `None` until the first
    /// state message.
    pub runtime_state: Option<RuntimeSessionState>,
    /// Turn committed at `Command::Prompt` routing, ahead of the
    /// wire-lagged `runtime_state`; the guards OR it in.
    pub turn_pending: bool,
    /// Whether the CLI last reported live background work here - the
    /// `background_tasks_changed` snapshot, which carries the whole set
    /// each change, so an empty one clears. Held on the session rather
    /// than folded per view: two views always agree about it, and a
    /// connection failure clears it because the CLI sends no terminal
    /// snapshot for a session that died.
    pub background_work: bool,
    /// Whether this session is waiting on `/login` before it can run.
    ///
    /// Set from the signals the CLI actually sends: an assistant message
    /// whose error is `AuthenticationFailed`, or a retry frame naming the
    /// same class. A `TurnError` envelope whose text reads like an auth
    /// failure sets it too, as a fallback - that envelope's producers are
    /// forge's own failed write and cancel rather than CLI output. Cleared
    /// by a `Connected` and by a turn that finished, since either proves
    /// the credential works. A fact about the session, so the lifecycle
    /// derivation reads it rather than each view inventing the state from
    /// the event it saw.
    pub awaiting_login: bool,
    /// The `/dictate` overlay's per-session normalizer-axis overrides.
    /// Set by `Command::SetDictateOverride` / `::ResetDictateOverrides`
    /// and consumed when a capture finishes into
    /// `NormalizeOptions`. Session-scoped and volatile: dies with the
    /// session, never persisted.
    pub dictate_overrides: crate::dictate::DictateOverrides,
    /// The slash commands the CLI last advertised for this session, from
    /// the `system/init` frame's `slash_commands` and from
    /// `commands_changed`. Held here so a view that arrived after the
    /// turn started reads the list through the view surface, rather than
    /// waiting a whole turn for the next frame.
    ///
    /// This is an intermediate state of a migration, not a settled rule.
    /// The TUI still folds its own copy from the same frames and calls
    /// neither verb, so the same policy runs in both places until it is
    /// removed; what is shared today is the parser, not the gate or the
    /// drift guard around it.
    pub available_commands: Vec<AvailableCommand>,
    /// The subagents the CLI last advertised, from the same init frame's
    /// `agents`, on the same terms as [`Self::available_commands`].
    pub available_agents: Vec<AvailableAgent>,
    /// Whether this turn's `system/init` has already been read for the
    /// agent catalogue. The first init of a turn carries it and a
    /// re-fire inside the same turn repeats it, so the read arms once per
    /// turn - the rule the TUI's own walker applies.
    pub agents_emitted_this_turn: bool,
}

impl DomainSession {
    /// Construct a fresh `DomainSession` bound to `key` with the
    /// given `conn`. Pre-spawn / pre-Connect callers pass `None` to
    /// register a placeholder domain whose handle slot fills in once
    /// the spawn handler runs.
    pub fn new(key: SessionSlot, conn: Option<Arc<AgentHandle>>) -> Self {
        Self {
            key,
            session_id: None,
            conn,
            pending_interactions: HashMap::new(),
            spawned_force_new: false,
            spawn_wrote_row: false,
            runtime_state: None,
            turn_pending: false,
            background_work: false,
            awaiting_login: false,
            dictate_overrides: crate::dictate::DictateOverrides::default(),
            available_commands: Vec::new(),
            available_agents: Vec::new(),
            agents_emitted_this_turn: false,
        }
    }

    /// Whether a turn is in flight. `turn_pending` is the primary
    /// signal: it is stamped synchronously when a `Command::Prompt` is
    /// routed, so it covers the window before any wire echo lands.
    /// `runtime_state` is OR'd in rather than trusted on its own -
    /// `session_state_changed` appears in no wire-conformance baseline
    /// and in none of the captured session JSONL, so it may never
    /// arrive.
    ///
    /// Shared by the prompt-interception guards and the worker
    /// activity derivation so the two cannot drift apart.
    pub fn turn_in_flight(&self) -> bool {
        self.turn_pending
            || matches!(
                self.runtime_state,
                Some(RuntimeSessionState::Running | RuntimeSessionState::RequiresAction)
            )
    }
}

impl std::fmt::Debug for DomainSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainSession")
            .field("key", &self.key)
            .field("session_id", &self.session_id)
            .field("pending_interactions_count", &self.pending_interactions.len())
            .finish_non_exhaustive()
    }
}
