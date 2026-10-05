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

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use forge_agent::AgentHandle;
use forge_primitives::{
    AvailableAgent, AvailableCommand, AvailableModel, CurrentModel, EffortLevel, McpServerStatus,
    MonitorRecord, PermissionMode, RuntimeSessionState, SessionId,
};

use crate::SessionSlot;
use crate::protocol::PendingInteractionSlot;

/// The MCP servers a session's bridge last reported, and the failure
/// standing beside them when the read did not complete.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpServers {
    pub servers: Vec<McpServerStatus>,
    /// Why the read failed, when it did. A failed read carries an empty
    /// server list, so a reader that ignored this would report a session
    /// with no servers connected.
    pub error: Option<String>,
}

/// How full the session's context window is, from the bridge's last
/// answer. Both halves are `Option` because the upstream probe reports
/// them independently.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ContextUsage {
    pub percent: Option<u8>,
    pub max_tokens: Option<u64>,
}

/// Workspace's owned per-session state. One `DomainSession` per
/// active `SessionTask`. Single writer (the `SessionTask`); accessed
/// via `Arc<parking_lot::Mutex<DomainSession>>` so the `Workspace`
/// can route commands without locking the whole pool.
/// The lifecycle states that settle a queued row.
///
/// Written out rather than derived, and it has to agree with the client's own
/// set (`client/src/session/apply.ts`): the core drops what the client drops.
/// A state neither names leaves the row standing on both sides, which is the
/// fail-safe direction - a pile that keeps a delivered prompt is visibly odd,
/// and one that empties on a word nobody reads is silently wrong.
const SETTLED_STATES: [&str; 5] = ["started", "completed", "cancelled", "discarded", "refused"];

/// The interactions a seat is parked on, oldest first.
///
/// **Ordered because the read answers it, and the dock draws the front.** Two
/// AskUserQuestion calls in one assistant message run in parallel with
/// different tool ids, so a seat can hold several at once, and the terminal's
/// own rule is first-parked-first-drawn. One Vec rather than a map and an
/// order beside it: a seat holds a handful at most, and there is then no
/// second structure to keep in step.
#[derive(Default)]
pub struct PendingInteractions {
    held: Vec<(String, PendingInteractionSlot)>,
}

impl PendingInteractions {
    /// Park `slot` under `tool_id`. A repeat of an id replaces in place, so a
    /// frame folded twice does not park the same interaction twice.
    pub fn insert(&mut self, tool_id: String, slot: PendingInteractionSlot) {
        if let Some(waiting) = self.held.iter_mut().find(|(id, _)| *id == tool_id) {
            waiting.1 = slot;
            return;
        }
        self.held.push((tool_id, slot));
    }

    pub fn get(&self, tool_id: &str) -> Option<&PendingInteractionSlot> {
        self.held.iter().find(|(id, _)| id == tool_id).map(|(_, slot)| slot)
    }

    pub fn remove(&mut self, tool_id: &str) -> Option<PendingInteractionSlot> {
        let at = self.held.iter().position(|(id, _)| id == tool_id)?;
        Some(self.held.remove(at).1)
    }

    pub fn contains_key(&self, tool_id: &str) -> bool {
        self.held.iter().any(|(id, _)| id == tool_id)
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }

    /// Oldest first.
    pub fn values(&self) -> impl Iterator<Item = &PendingInteractionSlot> {
        self.held.iter().map(|(_, slot)| slot)
    }

    /// Oldest first, together with the ids that key them.
    pub fn drain(&mut self) -> impl Iterator<Item = (String, PendingInteractionSlot)> {
        std::mem::take(&mut self.held).into_iter()
    }
}

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
    /// wire `tool_id` / `elicitation_id`, oldest first. `SessionTask` pops
    /// on `Respond*` commands; bridge inserts on every `*Request` event.
    pub pending_interactions: PendingInteractions,
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
    /// The CLI's background-task registry as it last reported it, each entry
    /// carrying the command its own card named where both have been seen.
    ///
    /// The bool above says that something is running; this is what a view
    /// draws a row from, and it is held for the same reason - the set arrives
    /// whole on each `background_tasks_changed`, and a view that attached
    /// afterwards would otherwise see the flag and not one row.
    pub background_tasks: Vec<crate::BackgroundTask>,
    /// Prompts in the CLI's queue for this occupant: what a view draws as
    /// waiting, and what the pile read answers with.
    ///
    /// Appended where the prompt is sent - the only place that knows its
    /// source and words - and advanced by the CLI's own lifecycle frames,
    /// which are the only writer of a prompt's state. Belongs to the
    /// occupant, so it is dropped with the background registry.
    pub prompt_queue: Vec<crate::protocol::QueuedPrompt>,
    /// Whether this occupant's CLI advertises `msg_lifecycle_v1`, from its
    /// init frame. `None` until one has been read.
    ///
    /// The pile is drawn FOR it: without the frames nothing would ever settle
    /// a row, so a session that does not advertise them records no rows at all
    /// and falls back to drawing the prompt the way it was drawn before the
    /// pile existed. Unknown reads as present - the first prompt can precede
    /// the first init - and the pinned CLI always advertises it.
    pub lifecycle_frames: Option<bool>,
    /// The command a tool call's card carried, by tool-use id.
    ///
    /// Held rather than resolved on the spot because the card arrives BEFORE
    /// the `task_started` that links it to a task, so the command has to be
    /// kept until a link names it. Cleared with the registry: it belongs to
    /// the occupant that made the calls.
    background_commands: HashMap<String, String>,
    /// The ids whose cards arrived since the last registry change.
    ///
    /// What keeps the map above bounded: a command is kept while a live task
    /// links to it, or for one registry change after its card arrived - which
    /// is exactly the window in which the `task_started` naming it can land.
    /// A foreground call's command is never read by anything and dies at the
    /// next change.
    staged_commands: HashSet<String>,
    /// The tool call that began a task, by task id, from `task_started`.
    task_tool_use: HashMap<String, String>,
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
    /// Hook-observed permission mode, from the same observation the TUI
    /// mirrors: higher fidelity than the `system/status` mode, because
    /// the CLI can change mode without re-emitting status.
    pub observed_permission_mode: Option<PermissionMode>,
    /// Hook-observed effort level, on the same terms as
    /// [`Self::observed_permission_mode`].
    pub observed_effort: Option<EffortLevel>,
    /// The effort level forge stamped into this session's launch
    /// settings, read back so a session whose hook has not fired yet
    /// still answers with the level it is running at rather than none.
    /// `Max` when the launch asked for nothing, which is forge's default.
    pub configured_effort: EffortLevel,
    /// The permission mode forge stamped into the same launch settings,
    /// on the same terms: the answer until a hook reports one. `None`
    /// when the launch pinned none.
    pub configured_permission_mode: Option<PermissionMode>,
    /// The MCP servers this session last saw, from its own bridge: MCP
    /// is configured per session, so this is not an account-wide fact.
    pub mcp_servers: Option<McpServers>,
    /// How full the context window was at the last poll.
    pub context_usage: Option<ContextUsage>,
    /// The model the session resolved to, from its connect and from
    /// every later init frame that names a different one.
    pub current_model: Option<CurrentModel>,
    /// The model catalogue the connect resolved against, kept so a frame
    /// naming another model can be resolved the same way the connect's own
    /// name was rather than falling back to the raw id.
    pub available_models: Vec<AvailableModel>,
    /// The monitors this session has running, and the ones that settled
    /// while it did, folded from the wire: the `Monitor` tool call, the
    /// task id the CLI assigns it, and the transition that settles it.
    /// The set drains once every entry in it is terminal, so a session
    /// that ran a monitor and stopped drawing one holds none.
    pub monitors: Vec<MonitorRecord>,
    /// The last OS-level walk of the session's process tree. The scan
    /// runs on a view's tick rather than here, so this is the most
    /// recent one whoever asked last produced.
    pub process_snapshot: Option<forge_agent::env::processes::ProcessSnapshot>,
    /// The last scan of the session's working tree, and when it was taken.
    /// Written by the seat's watch loop while a view is showing it, so a
    /// read answers a value rather than paying for a `git` subprocess.
    pub work_snapshot: Option<crate::work::WorkSnapshot>,
    /// Whether this conversation dispatched a sub-agent, anywhere in it.
    ///
    /// Assigned from the history a connect carries and raised by each dispatch
    /// frame: it is a fact about the whole conversation rather than about a
    /// window of it, so within one occupant's life it never goes back - but a
    /// connect reassigns it, and a fresh `/new`, which carries no history,
    /// leaves it false rather than inheriting the occupant before it.
    pub has_dispatches: bool,
    /// The session's sub-agent instances, folded a frame at a time as they
    /// arrive. Seeded from the history a connect carries, then driven by
    /// each frame - the same walk that raises [`Self::has_dispatches`].
    pub card_tracker: crate::subagent_cards::CardTracker,
    /// The instance list the last announcement carried, which a push is
    /// gated on: an announcement is a frame about a change, so the frame
    /// that moved nothing announces nothing.
    pub cards_snapshot: Vec<forge_primitives::runtime::SubagentCard>,
    /// The seat's walked file index, and when it was taken. Written by the
    /// seat's own loop while a view is showing it, so the composer's `@`
    /// list reads a value rather than paying for a walk of the tree.
    pub file_index: Option<crate::work::HeldFileIndex>,
}

impl DomainSession {
    /// Whether a permission prompt is still waiting on `tool_id`.
    ///
    /// The session task asks the same question as it pops the slot, and this
    /// asks it where a refusal can still reach a caller: by the time the task
    /// sees the answer, the click has already happened.
    pub fn awaits_permission(&self, tool_id: &str) -> bool {
        matches!(
            self.pending_interactions.get(tool_id),
            Some(PendingInteractionSlot::Permission { .. }),
        )
    }

    /// The same for a question. A prompt answered already, or one that asked
    /// something else, is a dock that is gone.
    pub fn awaits_question(&self, tool_id: &str) -> bool {
        matches!(
            self.pending_interactions.get(tool_id),
            Some(PendingInteractionSlot::Question { .. }),
        )
    }

    /// Replace the registry with the CLI's last snapshot, which carries the
    /// whole set, and fill in whatever commands are already resolvable.
    ///
    /// Also the one place the held commands are pruned: a task that has left
    /// the registry takes its link with it, and a command whose card arrived
    /// since the last change keeps one more change to be linked.
    pub(crate) fn replace_background_tasks(&mut self, tasks: Vec<crate::BackgroundTask>) {
        self.background_tasks = tasks;
        self.link_background_commands();
        let live: HashSet<&str> =
            self.background_tasks.iter().map(|task| task.task_id.as_str()).collect();
        self.task_tool_use.retain(|task_id, _| live.contains(task_id.as_str()));
        let linked: HashSet<&str> = self.task_tool_use.values().map(String::as_str).collect();
        self.background_commands
            .retain(|id, _| linked.contains(id.as_str()) || self.staged_commands.contains(id));
        self.staged_commands.clear();
    }

    /// Record the command a tool call's card carried.
    pub(crate) fn hold_background_command(&mut self, tool_use_id: String, command: String) {
        self.staged_commands.insert(tool_use_id.clone());
        self.background_commands.insert(tool_use_id, command);
        self.link_background_commands();
    }

    /// Record which tool call began a task, which is the link that names the
    /// command, and fill in that task's command if the card is already held.
    pub(crate) fn hold_task_tool_use(&mut self, task_id: String, tool_use_id: String) {
        self.task_tool_use.insert(task_id, tool_use_id);
        self.link_background_commands();
    }

    /// Let go of the registry and everything that resolves a command in it: a
    /// new occupant's calls are its own, and the CLI re-sends the set only
    /// when it changes.
    pub(crate) fn drop_background_tasks(&mut self) {
        self.background_work = false;
        self.background_tasks.clear();
        self.background_commands.clear();
        self.staged_commands.clear();
        self.task_tool_use.clear();
        // The queue died with the CLI process the prompts were written to, and
        // so did the promise its init frame made: the flag is re-read from the
        // new occupant's own init, which the CLI re-fires every turn. Left
        // latched, a `/resume` onto an advertising CLI would keep the pile
        // silently off, or latch true beside rows nothing can settle.
        self.prompt_queue.clear();
        self.lifecycle_frames = None;
    }

    /// Record a prompt as waiting, at the dispatch site.
    pub(crate) fn record_queued_prompt(
        &mut self,
        uuid: &str,
        source: crate::protocol::PromptSource,
        text: &str,
    ) {
        self.prompt_queue.push(crate::protocol::QueuedPrompt {
            uuid: uuid.to_owned(),
            source,
            text: text.to_owned(),
        });
    }

    /// Advance a prompt's state from one of the CLI's lifecycle frames.
    ///
    /// Returns whether the id was known. A state the CLI has SETTLED drops the
    /// row, so the pile holds only prompts still waiting; a state this build
    /// cannot name leaves it standing, because a word the CLI adds later must
    /// not empty a pile nobody can see being wrong. An unknown id is not an
    /// error - a prompt dispatched before this build, or a frame for a prompt
    /// another cohort minted, simply is not in the pile.
    pub(crate) fn advance_queued_prompt(&mut self, uuid: &str, state: &str) -> bool {
        let Some(at) = self.prompt_queue.iter().position(|p| p.uuid == uuid) else {
            return false;
        };
        if SETTLED_STATES.contains(&state) {
            self.prompt_queue.remove(at);
        }
        true
    }

    /// Drop an entry outright - a cancel that the CLI confirmed.
    pub(crate) fn drop_queued_prompt(&mut self, uuid: &str) {
        self.prompt_queue.retain(|p| p.uuid != uuid);
    }

    /// Fill in the command of every entry whose card and tool call are both
    /// known. Called after either half lands, so the order the CLI sends them
    /// in does not matter.
    fn link_background_commands(&mut self) {
        for task in &mut self.background_tasks {
            if task.command.is_some() {
                continue;
            }
            task.command = self
                .task_tool_use
                .get(&task.task_id)
                .and_then(|tool_use| self.background_commands.get(tool_use))
                .cloned();
        }
    }

    /// Construct a fresh `DomainSession` bound to `key` with the
    /// given `conn`. Pre-spawn / pre-Connect callers pass `None` to
    /// register a placeholder domain whose handle slot fills in once
    /// the spawn handler runs.
    pub fn new(key: SessionSlot, conn: Option<Arc<AgentHandle>>) -> Self {
        Self {
            key,
            session_id: None,
            conn,
            pending_interactions: PendingInteractions::default(),
            prompt_queue: Vec::new(),
            lifecycle_frames: None,
            spawned_force_new: false,
            spawn_wrote_row: false,
            runtime_state: None,
            turn_pending: false,
            background_work: false,
            background_tasks: Vec::new(),
            background_commands: HashMap::new(),
            staged_commands: HashSet::new(),
            task_tool_use: HashMap::new(),
            awaiting_login: false,
            dictate_overrides: crate::dictate::DictateOverrides::default(),
            available_commands: Vec::new(),
            available_agents: Vec::new(),
            agents_emitted_this_turn: false,
            observed_permission_mode: None,
            observed_effort: None,
            configured_effort: EffortLevel::Max,
            configured_permission_mode: None,
            mcp_servers: None,
            context_usage: None,
            current_model: None,
            available_models: Vec::new(),
            monitors: Vec::new(),
            process_snapshot: None,
            work_snapshot: None,
            has_dispatches: false,
            card_tracker: crate::subagent_cards::CardTracker::default(),
            cards_snapshot: Vec::new(),
            file_index: None,
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

/// The effort level a spawn asked for, read back from the launch
/// settings forge stamps for the CLI.
///
/// The CLI reads `effortLevel` out of its own settings file, so a spawn
/// that pins nothing runs at whatever that file says; forge's default
/// for an unset one is `Max`, the same default the config reader
/// applies.
pub(crate) fn configured_effort_from_settings(
    settings: &crate::SessionLaunchSettings,
) -> EffortLevel {
    settings
        .settings
        .as_ref()
        .and_then(|document| document.get(EFFORT_LEVEL_KEY))
        .and_then(serde_json::Value::as_str)
        .and_then(EffortLevel::from_stored)
        .unwrap_or(EffortLevel::Max)
}

/// The mode a spawn asked for, read back from the same launch settings
/// the effort comes from.
///
/// `None` when the launch pinned nothing and when it pinned a mode forge
/// cannot name: unlike effort there is no default to fall back on here,
/// because the CLI's own default is not a forge setting.
pub(crate) fn configured_permission_mode_from_settings(
    settings: &crate::SessionLaunchSettings,
) -> Option<PermissionMode> {
    settings
        .settings
        .as_ref()
        .and_then(|document| document.get(crate::SessionLaunchSettings::PERMISSIONS_KEY))
        .and_then(|permissions| {
            permissions.get(crate::SessionLaunchSettings::PERMISSIONS_DEFAULT_MODE_KEY)
        })
        .and_then(serde_json::Value::as_str)
        .and_then(PermissionMode::from_wire)
}

/// The CLI settings key a session's effort is stamped under. One const,
/// so the writer and this reader cannot split the seam.
pub(crate) const EFFORT_LEVEL_KEY: &str = "effortLevel";

impl std::fmt::Debug for DomainSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DomainSession")
            .field("key", &self.key)
            .field("session_id", &self.session_id)
            .field("pending_interactions_count", &self.pending_interactions.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The level the spawn asked the CLI to run at, so a session whose
    /// hook has not fired yet still names an effort rather than none.
    #[test]
    fn a_launch_that_pins_an_effort_names_that_level() {
        let settings = crate::SessionLaunchSettings {
            settings: Some(serde_json::json!({ "effortLevel": "high" })),
            ..Default::default()
        };

        assert_eq!(
            configured_effort_from_settings(&settings),
            EffortLevel::High,
            "the level the launch stamped is the level the session runs at",
        );
    }

    /// A launch that pins nothing leaves the CLI on its own settings
    /// file, and forge's default for an unset level is `Max`.
    #[test]
    fn a_launch_that_pins_no_effort_reads_forges_default() {
        let settings = crate::SessionLaunchSettings::default();

        assert_eq!(
            configured_effort_from_settings(&settings),
            EffortLevel::Max,
            "an unset effort is forge's default rather than nothing",
        );
    }

    /// A level forge cannot name is a level it cannot report, and the
    /// default is a better answer than a guess.
    #[test]
    fn an_unreadable_launch_effort_reads_forges_default() {
        let settings = crate::SessionLaunchSettings {
            settings: Some(serde_json::json!({ "effortLevel": "turbo" })),
            ..Default::default()
        };

        assert_eq!(
            configured_effort_from_settings(&settings),
            EffortLevel::Max,
            "a level forge cannot read falls back rather than failing",
        );
    }

    /// The mode a spawn asked for, read back from the same launch
    /// settings. Forge stamps its effective default here, so a session
    /// whose hook has not fired yet still names the mode it was launched
    /// in rather than nothing.
    #[test]
    fn a_launch_that_pins_a_mode_names_that_mode() {
        let settings = crate::SessionLaunchSettings {
            settings: Some(serde_json::json!({ "permissions": { "defaultMode": "plan" } })),
            ..Default::default()
        };

        assert_eq!(
            configured_permission_mode_from_settings(&settings),
            Some(PermissionMode::Plan),
            "the mode the launch stamped is the mode the session runs in",
        );
    }

    /// A launch that pinned nothing says so rather than naming a mode
    /// forge did not choose.
    #[test]
    fn a_launch_that_pins_no_mode_names_none() {
        let settings = crate::SessionLaunchSettings::default();

        assert_eq!(
            configured_permission_mode_from_settings(&settings),
            None,
            "no pin is no answer, rather than a mode forged from nothing",
        );
    }

    /// A mode forge cannot name is a mode it cannot report.
    #[test]
    fn an_unreadable_launch_mode_names_none() {
        let settings = crate::SessionLaunchSettings {
            settings: Some(serde_json::json!({ "permissions": { "defaultMode": "yolo" } })),
            ..Default::default()
        };

        assert_eq!(
            configured_permission_mode_from_settings(&settings),
            None,
            "a mode forge cannot read is not guessed at",
        );
    }
}
