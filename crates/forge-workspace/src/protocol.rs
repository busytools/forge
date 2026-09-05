//! Command + SessionUpdate channel envelopes between forge-tui and
//! forge-workspace. TUI dispatches Commands; workspace per-session
//! tasks emit SessionUpdates back via a fan-in channel.
//!
//! ## Dual Command shape (deliberate)
//!
//! Two `Command` enums exist in the workspace: this one, keyed by
//! [`SessionKey`], and [`forge_primitives::AgentCommand`], keyed by
//! `session_id: String`. They overlap on variant names (Prompt,
//! Cancel, SetMode, …) but serve different boundary layers:
//!
//! - **`forge_workspace::protocol::Command`** is the TUI ↔ workspace
//!   envelope. SessionKey routing, App-level variants
//!   (SpawnProject / SpawnSession / StartDefault), the
//!   workspace-internal Respond* + MCP cluster.
//! - **`forge_primitives::AgentCommand`** is the workspace ↔ agent
//!   envelope. session_id-keyed, raw shapes the AgentHandle
//!   dispatcher recognises.
//!
//! Collapsing them would force the AgentHandle dispatcher to handle
//! App-level variants it has no business in (SpawnProject is a
//! workspace concern; SessionKey is a routing concern; neither
//! belongs in the agent layer). The current split keeps each
//! envelope minimal at its respective boundary. The translation
//! happens in `session_task::execute_command_via_handle`.

use std::path::PathBuf;

use forge_agent::client::SessionLaunchSettings;
use forge_primitives::cloud::oauth_credentials::OauthCredentials;
use forge_primitives::cloud::service_status::ServiceSeverity;
use forge_primitives::error::AppError;
use forge_primitives::permission::PermissionMode;
use forge_primitives::permission_ui::{PermissionOutcome, PermissionRequest};
use forge_primitives::plugins::{
    PluginUpdateRun, PluginUpdateTrigger, PluginsCliActionSuccess, PluginsInventorySnapshot,
};
use forge_primitives::question::{QuestionOutcome, QuestionRequest};
use forge_primitives::review::{ReviewStatus, ReviewThread};
use forge_primitives::runtime::{AvailableModel, CurrentModel, ModeState, TerminalReason};
use forge_primitives::{
    AccountInfo, ForgeAccountIdentity, ImageAttachment, McpOperationError, McpServerStatus,
    Message, PeerInflightStats, SessionId, SessionListEntry,
};
use tokio::sync::oneshot;

use crate::SessionKey;
use crate::mcp::peers::types::WrappedPrompt;

// `TurnErrorClass` lives in forge-primitives so the classifier (in
// forge-agent) and consumers (in forge-tui, via this protocol module)
// share one enum. Re-exported here so existing call sites keep
// resolving via `forge_workspace::protocol::TurnErrorClass`.
pub use forge_primitives::TurnErrorClass;

/// One pending interaction response slot. Workspace stores these
/// keyed by `tool_id` in `DomainSession.pending_interactions`.
/// `Command::RespondPermission` / `RespondQuestion` look up the
/// matching slot and send the outcome down the oneshot.
pub enum PendingInteractionSlot {
    Permission(oneshot::Sender<PermissionOutcome>),
    Question(oneshot::Sender<QuestionOutcome>),
}

impl std::fmt::Debug for PendingInteractionSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Permission(_) => f.write_str("PendingInteractionSlot::Permission"),
            Self::Question(_) => f.write_str("PendingInteractionSlot::Question"),
        }
    }
}

/// Synchronous return from `Command::SpawnWorker` - the session_id
/// that the new worker was issued and the tag value applied. Threaded
/// back to the calling `workers__spawn` Tool impl via the oneshot
/// receiver so the LLM sees `{session_id, tag}` in the tool result.
#[derive(Debug, Clone)]
pub struct WorkerSpawnReply {
    pub session_id: String,
    pub tag: String,
    /// Set to the assigned account name when that account is itself
    /// currently rate-limited or bailed (a fresh assignment that fell
    /// back onto a fully saturated pool, or a re-spawn pinned to a
    /// since-unusable account). The spawn tool surfaces it as a
    /// `notice` so the lead sees the situation at spawn instead of only
    /// discovering it when the worker stalls.
    pub rate_limited_account: Option<String>,
    /// Set when persisting the worker's durable row failed (the store
    /// couldn't open, or the write errored). The worker still spawns, but
    /// it won't survive a forge restart. The spawn tool surfaces it as a
    /// warning so the lead knows the "durable" promise didn't hold for
    /// this one. Mirrors `worktree_cleanup_warning` on despawn.
    pub durability_warning: Option<String>,
}

/// Outcome of a [`Command::DespawnWorker`], sent back to the calling
/// `workers__despawn` Tool via the command's `respond` oneshot.
#[derive(Debug)]
pub enum DespawnResult {
    /// The worker was torn down (subprocess killed, dropped from
    /// `live_workers`, inflight asks expired). `worktree_cleanup_warning`
    /// is `Some` when the post-teardown `git worktree remove` failed -
    /// the worker is still gone; only the worktree directory lingers.
    /// Teardown and worktree cleanup are independent: a cleanup failure
    /// never rolls back the kill.
    ///
    /// `branch_cleanup_warning` is `Some` when the worker's
    /// `worktree-<label>` branch was left in place - it holds commits
    /// reachable from no other ref, or the check itself failed.
    Despawned { worktree_cleanup_warning: Option<String>, branch_cleanup_warning: Option<String> },
    /// Despawn refused: the worktree has uncommitted/untracked changes
    /// or unpushed commits and `force` was not set. Nothing was torn
    /// down; the worker stays live. `reason` names what is dirty.
    Blocked { reason: String },
    /// No live worker matched `label` (already gone or never existed).
    NotFound,
}

/// Mutation kind for a `SessionUpdate::WorkerStatusChanged` event.
/// `Added` and `StatusChanged` carry a fresh `WorkerStatus` snapshot;
/// `Removed` carries the last-known snapshot for symmetry but the TUI
/// reducer treats it as a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerStatusAction {
    Added,
    Removed,
    StatusChanged,
}

/// What has happened to a worker's git worktree as of the
/// `SessionUpdate::WorkerStatusChanged` event carrying it. Only the
/// `workers__despawn` path ever removes one. Among the spawn
/// rollbacks, the dividing line is `Connected`: one that fires before
/// the subprocess connected reports [`Self::Absent`], because claude
/// never ran to create a worktree, while a rollback after `Connected`
/// (a failed tag-write) reports [`Self::untouched`], because by then
/// the worktree is on disk. Every other emitter reports
/// [`Self::untouched`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorktreeDisposition {
    /// Nothing to report on: either the worker was spawned outside a
    /// git repo, or its spawn failed before it had a worktree.
    Absent,
    /// On disk and untouched.
    Intact,
    /// Gone from disk by the time the despawn finished, whether or not
    /// the despawn is what removed it.
    Removed,
    /// The despawn's removal failed and the worktree is still on disk.
    RemovalFailed,
}

impl WorktreeDisposition {
    /// The disposition when nothing has touched the worktree.
    pub fn untouched(is_git_repo_at_spawn: bool) -> Self {
        if is_git_repo_at_spawn { Self::Intact } else { Self::Absent }
    }
}

/// Command envelope: forge-tui -> forge-workspace.
///
/// Every variant carries a `SessionKey` identifying the target
/// session task. `Workspace::dispatch` fans the variant into the
/// matching task's command receiver.
pub enum Command {
    Prompt {
        key: SessionKey,
        text: String,
        attachments: Vec<ImageAttachment>,
    },
    Cancel {
        key: SessionKey,
    },
    SetMode {
        key: SessionKey,
        mode: PermissionMode,
    },
    SetModel {
        key: SessionKey,
        model: String,
    },
    NewSession {
        key: SessionKey,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    },
    ResumeSession {
        key: SessionKey,
        session_id: String,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    },
    RespondPermission {
        key: SessionKey,
        tool_id: String,
        outcome: PermissionOutcome,
    },
    RespondQuestion {
        key: SessionKey,
        tool_id: String,
        outcome: QuestionOutcome,
    },
    /// Set one `/dictate` overlay axis for this session, or clear
    /// every axis at once. Workspace state on the `DomainSession`,
    /// never routed to the agent; the echo lands as
    /// `SessionUpdate::DictateOverrides`.
    SetDictateOverride {
        key: SessionKey,
        update: crate::dictate::DictateOverrideUpdate,
    },
    /// Clear every `/dictate` override this session holds.
    ResetDictateOverrides {
        key: SessionKey,
    },
    /// Set this session's `/dictate` input-device pick, or clear it
    /// back to the configured pin. Workspace state on the
    /// `DomainSession`, never routed to the agent; the echo lands as
    /// `SessionUpdate::DictateDevicePin`.
    SetDictateDevice {
        key: SessionKey,
        pick: Option<crate::dictate::DictateDeviceChoice>,
    },
    /// Reconnect a configured MCP server.
    ReconnectMcpServer {
        key: SessionKey,
        server_name: String,
    },
    /// Toggle a configured MCP server on/off.
    ToggleMcpServer {
        key: SessionKey,
        server_name: String,
        enabled: bool,
    },
    /// User clicked an inactive project to wake it. No `key` - the
    /// session doesn't exist yet; workspace synthesizes a key and
    /// emits `SessionUpdate::Spawning` then `::Connected` with the
    /// real key. This is an App-level Command, not per-session.
    SpawnProject {
        project_name: String,
        launch_settings: SessionLaunchSettings,
    },
    /// User clicked a non-lead session row. Workspace spawns an
    /// agent for the specific session_id, synthesizing a key, and
    /// emits `Spawning` then `Connected` with the real key.
    SpawnSession {
        session_id: String,
        launch_settings: SessionLaunchSettings,
    },
    /// App start. Workspace spawns the default project (or the
    /// project passed on CLI argv) and emits the spawning + connected
    /// updates. TUI calls this once at startup. `is_fatal_on_failure`
    /// flags whether a pre-Connected failure should emit
    /// `SessionUpdate::FatalError` alongside the `ConnectionFailed`;
    /// startup is fatal (nothing to render), the sleeping-spawn flows
    /// are not (the user has an active session whose state must
    /// survive a spawn failure).
    StartDefault {
        project_name: Option<String>,
        launch_settings: SessionLaunchSettings,
    },
    /// Peer-coordination delivery (#114 v1). Dispatched by the
    /// `mcp__forge__peers__ask_agent` / `peers__tell_agent` tool
    /// impls via `WorkspaceFacade::deliver_peer_prompt`. Routed to
    /// `spawn::handle_deliver_peer_prompt` which: (a) resolves
    /// `target_project` to a `SessionKey`; (b) if target is running,
    /// dispatches a plain `Command::Prompt` carrying the wrapper
    /// prose; (c) if sleeping, buffers `wrapped` in target's
    /// `pending_peer_prompts` and dispatches `Command::SpawnProject`;
    /// (d) on `target_project` not in forge.toml, fires the dual-path
    /// `PeerAskFailed` notification back to caller.
    ///
    /// App-level command (`key()` returns `None`); workspace routes
    /// to the App-level handler in `spawn.rs`. The `caller` field is
    /// the source session's key for routing failure notifications and
    /// for in_reply_to validation lookups.
    DeliverPeerPrompt {
        caller: SessionKey,
        target_project: String,
        wrapped: WrappedPrompt,
    },
    /// Spawn a new worker session in `project_key`. Dispatched by
    /// the `workers__spawn` MCP Tool impl after caller-tag validation,
    /// or by the lead Connected hook when reviving
    /// across-restart workers.
    ///
    /// `resume_existing`: when `Some(session_id)`, the handler resumes
    /// the named session via `SessionTarget::Session` instead of
    /// spawning fresh. The session_id MUST already carry the
    /// `forge:worker:<label>` tag (the lead Connected hook verifies
    /// this before dispatching). `WorkerEntry::needs_tag` is set false
    /// on the resume path since the tag is already on disk. `None`
    /// preserves the original fresh-spawn path used by `workers__spawn`.
    ///
    /// `return_to` carries the spawn result back to the calling tool
    /// invocation; `Ok((session_id, tag))` on success, `Err(message)`
    /// on failure (e.g. tag-write failed; see spawn handler). On the
    /// resume path the lead Connected hook ignores the reply (the
    /// worker is for the lead's benefit, not in response to a tool
    /// call).
    SpawnWorker {
        project_key: crate::ProjectKey,
        label: String,
        charter: String,
        spawned_by_session_id: String,
        resume_existing: Option<String>,
        /// First message delivered as the worker's user turn on Connected,
        /// via the rate-limited kick dispatcher. Either `workers__spawn`'s
        /// `kick`, or - on a re-spawn - the row's `resume_kick` or the
        /// generic restart note, which is the only thing that wakes a
        /// resuming worker. `None` -> no kick (the worker idles until
        /// told).
        kick: Option<String>,
        /// Whether this worker keeps the built-in `AskUserQuestion`
        /// tool. Read from the persisted row on a re-spawn, so it
        /// survives a forge restart.
        interactive: bool,
        return_to: oneshot::Sender<Result<WorkerSpawnReply, String>>,
    },
    /// Close (terminate agent + remove from `live_workers`) the
    /// worker identified by `label` in `project_key`. Dispatched by
    /// the TUI's per-row close click. If duplicates exist, the latest-
    /// spawned matching entry is removed.
    CloseWorker {
        project_key: crate::ProjectKey,
        label: String,
    },
    /// Open a URL in the system browser. Dispatched by the TUI's
    /// Inspector PR-row click. App-level command (`key()` returns
    /// `None`); the shell-out runs in `forge_agent::env::open_url`
    /// off the render thread and a failure surfaces as a
    /// `SessionUpdate::ServiceStatus` warning.
    OpenUrl {
        url: String,
    },
    /// Despawn the worker identified by `label` in `project_key`:
    /// terminate its agent, drop it from `live_workers`, expire its
    /// inflight asks, AND clean up its git worktree. Dispatched by the
    /// `workers__despawn` MCP tool (lead-only). Unlike `CloseWorker`
    /// (the TUI X-button), this also removes the worker's git worktree:
    /// a clean worktree is removed; a dirty one (uncommitted/untracked
    /// or unpushed commits) blocks the despawn unless `force`. The
    /// outcome flows back via `respond`.
    DespawnWorker {
        project_key: crate::ProjectKey,
        label: String,
        force: bool,
        respond: oneshot::Sender<DespawnResult>,
    },
    /// Deliver a wrapped peer-style prompt to a worker. Same envelope
    /// as `DeliverPeerPrompt` but addressed by worker label within
    /// the caller's project rather than by cross-project name.
    DeliverWorkerPrompt {
        caller: SessionKey,
        project_key: crate::ProjectKey,
        target_label: String,
        wrapped: WrappedPrompt,
    },
    /// Deliver a wrapped peer-style prompt from a worker back to its
    /// lead. Dispatched by the `workers__tell` / `workers__ask` Tool
    /// impls when the caller addresses `label="lead"`. The target
    /// `SessionKey` is resolved at Tool dispatch time from the
    /// worker's `spawned_by_session_id` so the handler can deliver
    /// directly without re-doing the lookup against a possibly-mutated
    /// `live_workers` map. Wire shape is identical to
    /// `DeliverWorkerPrompt` (same PeerEnvelopeAppended echo + same
    /// `Command::Prompt` dispatch into the target session).
    DeliverWorkerPromptToLead {
        caller: SessionKey,
        target_lead_key: SessionKey,
        wrapped: WrappedPrompt,
    },
    /// Deliver a matched Gotify notification into `project` as a plain
    /// user turn (spawning the project if asleep, exactly like a cron
    /// prompt). `team_role` targets a durable team worker when `Some`;
    /// `None` targets the project lead. Dispatched by the
    /// `GotifyHost::deliver` port impl, one per matching subscription;
    /// handled by `spawn::deliver_gotify_message`. `notification` carries
    /// the resolved app name, title, message, and priority - its
    /// `to_prose()` is the user-turn text, and the same struct drives the
    /// chat-echo. App-level command (`key()` returns `None`).
    DeliverGotifyMessage {
        project: String,
        team_role: Option<String>,
        notification: crate::mcp::gotify::types::GotifyNotification,
    },
    /// Switch the live session `key` to `account_display_name`: tear
    /// down its current `claude` subprocess and re-spawn + resume the
    /// SAME `session_id` under the picked account's `config_dir`. The
    /// user's account config dirs share `~/.claude/projects` via
    /// symlink, so `claude --resume` finds the same conversation - the
    /// switch copies no session files. `launch_settings` carries the
    /// session's model / mode / effort so the switch preserves them
    /// (the TUI builds them the same way a resume does). App-level
    /// command (`key()` returns `None`); routed to
    /// `spawn::handle_switch_account`.
    SwitchAccount {
        key: SessionKey,
        account_display_name: String,
        launch_settings: SessionLaunchSettings,
    },
    /// Begin dictating into the composer at `key`. App-level command
    /// carrying the origin key, like `DeliverPeerPrompt`: the
    /// microphone is process-global, so the recording lifecycle lives
    /// on `Workspace` rather than on one `SessionTask`, while the
    /// events route back to the session that started it.
    DictateStart {
        key: SessionKey,
    },
    /// Submit (`submit = true`) or abandon the take started by `key`.
    /// During recording this is release-to-submit vs discard; during a
    /// transcription in flight it abandons the ticket.
    DictateStop {
        key: SessionKey,
        submit: bool,
    },
    /// Overwrite the review-thread set for `(project, branch)`.
    /// Dispatched by the diff overlay's re-anchor recompute. Fire-and-
    /// forget: a write failure is warned, not surfaced. App-level
    /// command (`key()` returns `None`); routed inline in dispatch.
    SaveReviewThreads {
        project: String,
        branch: String,
        threads: Vec<ReviewThread>,
    },
    /// Remove one review thread by id from `(project, branch)`, so a
    /// deleted comment does not resurrect on the next hydrate.
    /// App-level command (`key()` returns `None`); routed inline.
    RemoveReviewThread {
        project: String,
        branch: String,
        thread_id: String,
    },
    /// Set the status of one review thread by id, bumping its
    /// `updated_at`. App-level command (`key()` returns `None`);
    /// routed inline.
    SetReviewThreadStatus {
        project: String,
        branch: String,
        thread_id: String,
        status: ReviewStatus,
    },
    /// Persist the `/spinner` override so it survives restart. The
    /// in-session active style lives on the TUI's `App::spinner_style`
    /// already; this is the durable-store write. App-level command
    /// (`key()` returns `None`); routed inline.
    PersistSpinner {
        style: crate::ui::SpinnerStyle,
    },
    /// Release the session `session_key` (cascade-aware: a project
    /// lead's workers terminate first). Dispatched by the TUI's
    /// per-row close click; the TUI removes its own bucket around the
    /// dispatch. App-level command (`key()` returns `None`); routed
    /// inline.
    CloseSession {
        session_key: SessionKey,
    },
    /// Insert or replace one review thread by id in `(project, branch)`.
    /// `respond` carries whether the write was confirmed, so the
    /// overlay's at-risk durability flag stays honest (the same
    /// value-returning shape as `SpawnWorker.return_to`). The handler
    /// runs inline in dispatch, so the response is present the moment
    /// dispatch returns. App-level command (`key()` returns `None`).
    UpsertReviewThread {
        project: String,
        branch: String,
        thread: ReviewThread,
        respond: oneshot::Sender<bool>,
    },
    /// Submit (seal) the listed threads as one review round.
    /// `respond` carries the minted review, `None` when the store
    /// write failed. App-level command (`key()` returns `None`).
    SubmitReview {
        project: String,
        branch: String,
        summary: Option<String>,
        thread_ids: Vec<String>,
        origin: SessionKey,
        respond: oneshot::Sender<Option<forge_primitives::ReviewSet>>,
    },
}

impl Command {
    /// The `SessionKey` this command routes to, or `None` for
    /// App-level commands (`SpawnProject`, `SpawnSession`,
    /// `StartDefault`). `Workspace::dispatch` routes `None` commands
    /// to its app-level handler (which synthesizes the new session
    /// key and spawns the agent); `Some(key)` commands route to the
    /// matching SessionTask.
    pub fn key(&self) -> Option<&SessionKey> {
        match self {
            Self::Prompt { key, .. }
            | Self::Cancel { key }
            | Self::SetMode { key, .. }
            | Self::SetModel { key, .. }
            | Self::NewSession { key, .. }
            | Self::ResumeSession { key, .. }
            | Self::RespondPermission { key, .. }
            | Self::RespondQuestion { key, .. }
            | Self::ReconnectMcpServer { key, .. }
            | Self::ToggleMcpServer { key, .. }
            | Self::SetDictateOverride { key, .. }
            | Self::ResetDictateOverrides { key }
            | Self::SetDictateDevice { key, .. } => Some(key),
            Self::SpawnProject { .. }
            | Self::SpawnSession { .. }
            | Self::StartDefault { .. }
            | Self::DictateStart { .. }
            | Self::DictateStop { .. }
            | Self::DeliverPeerPrompt { .. }
            | Self::SpawnWorker { .. }
            | Self::CloseWorker { .. }
            | Self::DespawnWorker { .. }
            | Self::DeliverWorkerPrompt { .. }
            | Self::DeliverWorkerPromptToLead { .. }
            | Self::DeliverGotifyMessage { .. }
            | Self::SwitchAccount { .. }
            | Self::OpenUrl { .. }
            | Self::SaveReviewThreads { .. }
            | Self::RemoveReviewThread { .. }
            | Self::SetReviewThreadStatus { .. }
            | Self::PersistSpinner { .. }
            | Self::CloseSession { .. }
            | Self::UpsertReviewThread { .. }
            | Self::SubmitReview { .. } => None,
        }
    }
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Custom Debug: discriminant + routing key (and a few cheap
        // identifying fields) only. `oneshot::Sender` on `SpawnWorker`
        // isn't Debug, so deriving isn't an option; payloads can be
        // bulky and aren't useful in trace output.
        match self {
            Self::Prompt { key, .. } => {
                f.debug_struct("Prompt").field("key", key).finish_non_exhaustive()
            }
            Self::Cancel { key } => f.debug_struct("Cancel").field("key", key).finish(),
            Self::SetMode { key, mode } => {
                f.debug_struct("SetMode").field("key", key).field("mode", mode).finish()
            }
            Self::SetModel { key, model } => {
                f.debug_struct("SetModel").field("key", key).field("model", model).finish()
            }
            Self::NewSession { key, cwd, .. } => f
                .debug_struct("NewSession")
                .field("key", key)
                .field("cwd", cwd)
                .finish_non_exhaustive(),
            Self::ResumeSession { key, session_id, cwd, .. } => f
                .debug_struct("ResumeSession")
                .field("key", key)
                .field("session_id", session_id)
                .field("cwd", cwd)
                .finish_non_exhaustive(),
            Self::RespondPermission { key, tool_id, .. } => f
                .debug_struct("RespondPermission")
                .field("key", key)
                .field("tool_id", tool_id)
                .finish_non_exhaustive(),
            Self::RespondQuestion { key, tool_id, .. } => f
                .debug_struct("RespondQuestion")
                .field("key", key)
                .field("tool_id", tool_id)
                .finish_non_exhaustive(),
            Self::SetDictateOverride { key, .. } => {
                f.debug_struct("SetDictateOverride").field("key", key).finish_non_exhaustive()
            }
            Self::ResetDictateOverrides { key } => {
                f.debug_struct("ResetDictateOverrides").field("key", key).finish()
            }
            Self::SetDictateDevice { key, .. } => {
                f.debug_struct("SetDictateDevice").field("key", key).finish_non_exhaustive()
            }
            Self::ReconnectMcpServer { key, server_name } => f
                .debug_struct("ReconnectMcpServer")
                .field("key", key)
                .field("server_name", server_name)
                .finish(),
            Self::ToggleMcpServer { key, server_name, enabled } => f
                .debug_struct("ToggleMcpServer")
                .field("key", key)
                .field("server_name", server_name)
                .field("enabled", enabled)
                .finish(),
            Self::SpawnProject { project_name, .. } => f
                .debug_struct("SpawnProject")
                .field("project_name", project_name)
                .finish_non_exhaustive(),
            Self::SpawnSession { session_id, .. } => f
                .debug_struct("SpawnSession")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::StartDefault { project_name, .. } => f
                .debug_struct("StartDefault")
                .field("project_name", project_name)
                .finish_non_exhaustive(),
            Self::DeliverPeerPrompt { caller, target_project, .. } => f
                .debug_struct("DeliverPeerPrompt")
                .field("caller", caller)
                .field("target_project", target_project)
                .finish_non_exhaustive(),
            Self::SpawnWorker { project_key, label, spawned_by_session_id, .. } => f
                .debug_struct("SpawnWorker")
                .field("project_key", project_key)
                .field("label", label)
                .field("spawned_by_session_id", spawned_by_session_id)
                .field("return_to", &"<oneshot::Sender>")
                .finish_non_exhaustive(),
            Self::CloseWorker { project_key, label } => f
                .debug_struct("CloseWorker")
                .field("project_key", project_key)
                .field("label", label)
                .finish(),
            Self::DespawnWorker { project_key, label, force, .. } => f
                .debug_struct("DespawnWorker")
                .field("project_key", project_key)
                .field("label", label)
                .field("force", force)
                .finish_non_exhaustive(),
            Self::DeliverWorkerPrompt { caller, project_key, target_label, .. } => f
                .debug_struct("DeliverWorkerPrompt")
                .field("caller", caller)
                .field("project_key", project_key)
                .field("target_label", target_label)
                .finish_non_exhaustive(),
            Self::DeliverWorkerPromptToLead { caller, target_lead_key, .. } => f
                .debug_struct("DeliverWorkerPromptToLead")
                .field("caller", caller)
                .field("target_lead_key", target_lead_key)
                .finish_non_exhaustive(),
            Self::DeliverGotifyMessage { project, team_role, notification } => f
                .debug_struct("DeliverGotifyMessage")
                .field("project", project)
                .field("team_role", team_role)
                .field("app", &notification.app)
                .field("priority", &notification.priority)
                .finish_non_exhaustive(),
            Self::SwitchAccount { key, account_display_name, .. } => f
                .debug_struct("SwitchAccount")
                .field("key", key)
                .field("account_display_name", account_display_name)
                .finish_non_exhaustive(),
            Self::OpenUrl { url } => f.debug_struct("OpenUrl").field("url", url).finish(),
            Self::DictateStart { key } => f.debug_struct("DictateStart").field("key", key).finish(),
            Self::DictateStop { key, submit } => {
                f.debug_struct("DictateStop").field("key", key).field("submit", submit).finish()
            }
            Self::SaveReviewThreads { project, branch, .. } => f
                .debug_struct("SaveReviewThreads")
                .field("project", project)
                .field("branch", branch)
                .finish_non_exhaustive(),
            Self::RemoveReviewThread { project, branch, thread_id } => f
                .debug_struct("RemoveReviewThread")
                .field("project", project)
                .field("branch", branch)
                .field("thread_id", thread_id)
                .finish(),
            Self::SetReviewThreadStatus { project, branch, thread_id, status } => f
                .debug_struct("SetReviewThreadStatus")
                .field("project", project)
                .field("branch", branch)
                .field("thread_id", thread_id)
                .field("status", status)
                .finish(),
            Self::PersistSpinner { style } => {
                f.debug_struct("PersistSpinner").field("style", style).finish()
            }
            Self::CloseSession { session_key } => {
                f.debug_struct("CloseSession").field("session_key", session_key).finish()
            }
            Self::UpsertReviewThread { project, branch, thread, .. } => f
                .debug_struct("UpsertReviewThread")
                .field("project", project)
                .field("branch", branch)
                .field("thread_id", &thread.id)
                .finish_non_exhaustive(),
            Self::SubmitReview { project, branch, thread_ids, .. } => f
                .debug_struct("SubmitReview")
                .field("project", project)
                .field("branch", branch)
                .field("thread_ids", thread_ids)
                .finish_non_exhaustive(),
        }
    }
}

/// What one finished dictation take produced. Plain data rather than
/// [`forge_dictate::Outcome`]: the TUI words the notices, so it gets
/// the observations and keeps the crate's error shapes.
#[derive(Debug, Clone, PartialEq)]
pub enum DictateOutcome {
    /// Words to insert at the composer's caret. `truncated` means the
    /// take hit the capture cap or the decode budget and is partial.
    Landed { text: String, truncated: bool },
    /// The audio normalised to nothing - a valid answer, not a failure.
    Empty,
    /// Nothing rose above the silence floor. A finite `peak_db` is a
    /// quiet room and a retry is reasonable; negative infinity means
    /// every sample was exactly zero, which is structural and sticky.
    NoAudio { peak_db: f32, seconds: u64 },
    /// The take never happened. Covers a busy microphone, a device that
    /// would not open, and dictation not being ready.
    Refused { message: String },
    /// Recognition failed. The response is the same whichever way.
    Failed,
    /// The user abandoned the take. Resets silently.
    Cancelled,
}

/// Update envelope: forge-workspace -> forge-tui.
///
/// Permission/Question variants do NOT carry response oneshots -
/// responses flow back via `Command::Respond*`. The workspace stores
/// the oneshot in `DomainSession.pending_interactions` when emitting
/// these variants.
pub enum SessionUpdate {
    /// Workspace has synthesized a spawning state for a project /
    /// session wake (in response to `Command::SpawnProject` /
    /// `Command::SpawnSession` / `Command::StartDefault`). TUI
    /// creates a placeholder UiSession under `key` and shows the
    /// "Waking {display_name}…" message. The real `Connected` /
    /// `KeyRenamed` updates land soon after.
    ///
    /// `key` is a synthetic key the workspace generates (e.g.,
    /// `__spawn_<project>__` or `__resume_<session_id>__`). When the
    /// agent emits its first `system/init` with the real session
    /// UUID, workspace migrates internally and emits
    /// `KeyRenamed { from: synth, to: real }`.
    Spawning {
        key: SessionKey,
        project_name: String,
        cwd: String,
        display_name: String,
    },
    /// Workspace migrated `from` (synthetic spawn key) to `to` (the
    /// real claude session UUID). TUI re-keys its UiSession map:
    /// `ui_sessions.remove(&from)` → `ui_sessions.insert(to, bucket)`.
    /// If `active_session_key == Some(from)`, TUI updates it to
    /// `Some(to)` so render keeps following.
    KeyRenamed {
        from: SessionKey,
        to: SessionKey,
    },
    /// A wake resolved to a session already in the pool, so the
    /// `Spawning` bucket at `key` is redundant and no `SessionTask`
    /// exists to migrate it - the one that connected consumed its own
    /// `spawn_key`. TUI drops the bucket, moving focus to
    /// `superseded_by` if it was there, and leaves `superseded_by`
    /// otherwise untouched.
    ///
    /// Distinct from `KeyRenamed`, which migrates the bucket onto `to`
    /// and marks it `Idle`. That is wrong here: before the live
    /// session's first `Connected` the synthetic is still the only
    /// bucket it has, so this retires nothing until one stands at
    /// `superseded_by`.
    SpawnBucketRetired {
        key: SessionKey,
        superseded_by: SessionKey,
    },
    Connected {
        key: SessionKey,
        session_id: SessionId,
        cwd: String,
        current_model: CurrentModel,
        available_models: Vec<AvailableModel>,
        mode: Option<ModeState>,
        history: Vec<Message>,
        /// Compactions the resumed transcript records. Seeds the
        /// per-session count, which has no other durable source.
        compaction_count: u32,
    },
    /// `key` is the replacement session; `previous_key` is the bucket
    /// it supersedes (the task's key before `rekey_to`). The two differ
    /// whenever the CLI issues a fresh session UUID, and the reducer
    /// needs `previous_key` to find the outgoing bucket - it is not
    /// derivable from anything else on the envelope.
    SessionReplaced {
        key: SessionKey,
        previous_key: SessionKey,
        session_id: SessionId,
        cwd: String,
        current_model: CurrentModel,
        available_models: Vec<AvailableModel>,
        mode: Option<ModeState>,
        history: Vec<Message>,
        /// Compactions the resumed transcript records. Seeds the
        /// per-session count, which has no other durable source.
        compaction_count: u32,
    },
    ConnectionFailed {
        key: SessionKey,
        message: String,
        fatal: bool,
    },
    AuthRequired {
        key: SessionKey,
        method_name: String,
        method_description: String,
    },
    SlashCommandError {
        key: SessionKey,
        message: String,
    },
    RuntimeReloadCompleted {
        session_id: String,
    },
    RuntimeReloadFailed {
        session_id: String,
        message: String,
    },
    /// The CLI refused (or failed) a `set_permission_mode` control
    /// request for `key`. `message` carries the underlying error text;
    /// the TUI rolls the optimistic mode chip back and surfaces it.
    SetModeFailed {
        key: SessionKey,
        mode: PermissionMode,
        message: String,
    },
    /// The CLI refused (or failed) a `set_model` control request for
    /// `key`. `message` carries the underlying error text; the TUI
    /// rolls the optimistic model change back and surfaces it.
    SetModelFailed {
        key: SessionKey,
        model: String,
        message: String,
    },
    /// Permission prompt. No response_tx - TUI replies via
    /// `Command::RespondPermission { tool_id, outcome }`.
    PermissionRequest {
        key: SessionKey,
        tool_id: String,
        request: PermissionRequest,
    },
    /// AskUserQuestion prompt. Same shape; reply via
    /// `Command::RespondQuestion { tool_id, outcome }`.
    QuestionRequest {
        key: SessionKey,
        tool_id: String,
        request: QuestionRequest,
    },
    McpOperationError {
        key: SessionKey,
        error: McpOperationError,
    },
    TurnComplete {
        key: SessionKey,
        terminal_reason: Option<TerminalReason>,
    },
    TurnCancelled {
        key: SessionKey,
    },
    TurnError {
        key: SessionKey,
        message: String,
        class: Option<TurnErrorClass>,
        terminal_reason: Option<TerminalReason>,
    },
    ChatAppended {
        session_id: String,
        msg: Message,
    },
    HookObservation {
        session_id: String,
        tool_use_id: Option<String>,
        permission_mode: Option<String>,
        effort: Option<String>,
        agent_id: Option<String>,
        agent_type: Option<String>,
    },
    StatusSnapshot {
        session_id: String,
        account: AccountInfo,
        forge_account: Option<ForgeAccountIdentity>,
    },
    ForgeAccountIdentity {
        key: SessionKey,
        display_name: String,
    },
    /// The full override set a session holds after a `/dictate` edit
    /// landed. Sent after every `SetDictateOverride` and
    /// `ResetDictateOverrides` so the dialog's markers and its reset
    /// row read from this, not from a TUI-side copy.
    DictateOverrides {
        key: SessionKey,
        overrides: crate::dictate::DictateOverrides,
    },
    /// The input-device pick a session holds after a `/dictate`
    /// device edit landed. Sent after every `SetDictateDevice`, and
    /// alongside the overrides echo by a Reset.
    DictateDevicePin {
        key: SessionKey,
        pick: Option<crate::dictate::DictateDeviceChoice>,
    },
    OauthCredentialsSnapshot {
        session_id: String,
        credentials: Option<OauthCredentials>,
    },
    ContextUsageSnapshot {
        session_id: String,
        percentage: Option<u8>,
        /// Raw model context-window size in tokens (e.g. `1_000_000`
        /// for Opus 1M). `None` until the upstream probe reports it.
        max_tokens: Option<u64>,
    },
    McpSnapshot {
        session_id: String,
        servers: Vec<McpServerStatus>,
        error: Option<String>,
    },
    SessionsListed {
        /// Bucket this session list belongs to. The catalog scan that
        /// produces `sessions` runs against the spawning session's
        /// `cwd`, so the listing is project-scoped - routing onto
        /// the requesting bucket prevents another session's `/resume`
        /// autocomplete from inheriting a stale project's list.
        key: SessionKey,
        sessions: Vec<SessionListEntry>,
    },
    ServiceStatus {
        severity: ServiceSeverity,
        message: String,
    },
    /// The background catalog scan finished and `Workspace`'s project
    /// catalog is populated. The launchpad and Projects pane read
    /// `list_projects()` per frame, so nothing needs carrying: the
    /// event exists to wake the render loop so session counts appear
    /// when the scan lands rather than on the next unrelated frame.
    CatalogLoaded,
    PluginsInventoryUpdated {
        cwd_raw: String,
        snapshot: PluginsInventorySnapshot,
        claude_path: PathBuf,
    },
    PluginsInventoryRefreshFailed {
        cwd_raw: String,
        message: String,
        /// Whether the failed refresh served a boot auto-update run -
        /// app-scoped, so its failure bypasses the cwd gate.
        trigger: PluginUpdateTrigger,
    },
    PluginsCliActionSucceeded {
        cwd_raw: String,
        result: PluginsCliActionSuccess,
    },
    PluginsCliActionFailed {
        cwd_raw: String,
        message: String,
    },
    /// A section-level update run or check moved: one or more rows
    /// changed state. Carries the whole run so the pane replaces its
    /// copy wholesale.
    PluginsUpdateRunProgress {
        cwd_raw: String,
        run: PluginUpdateRun,
    },
    /// The run (or check) is over. The record batch is persisted by
    /// the run task itself; this event is reporting only.
    PluginsUpdateRunFinished {
        cwd_raw: String,
        run: PluginUpdateRun,
        snapshot: Option<PluginsInventorySnapshot>,
        claude_path: Option<PathBuf>,
    },
    PluginsRollbackSucceeded {
        cwd_raw: String,
        plugin_id: String,
        scope: String,
        message: String,
        snapshot: PluginsInventorySnapshot,
        claude_path: PathBuf,
    },
    PluginsRollbackFailed {
        cwd_raw: String,
        plugin_id: String,
        message: String,
        /// The refreshed inventory when the rollback ran but did not
        /// verify, so the pane still reflects the real state.
        snapshot: Option<PluginsInventorySnapshot>,
    },
    /// Peer-coordination ask in-flight stats changed for `key`. Fired
    /// whenever `bump_inflight_stats` mutates the session's
    /// `PeerInflightStats` (ask sent, reply received, timeout, delivery
    /// failure). TUI reducer arm updates the sidebar peer-activity
    /// badge in the Projects pane.
    PeerInflightStatsChanged {
        key: SessionKey,
        stats: PeerInflightStats,
    },
    /// Workspace pushed a change to `live_workers[project_key]`. The
    /// TUI reducer updates the projects pane's tree-children based on
    /// `action`. `status` is the snapshot at the moment of the change
    /// (relevant for Added and StatusChanged; ignored for Removed but
    /// carried for symmetry). `worktree` is what has become of the
    /// worker's worktree - the TUI's close-toast formatter reads it on
    /// `Removed` events (after which the `WorkerEntry` is gone from
    /// `live_workers`, so a lookup-by-label would fail).
    WorkerStatusChanged {
        project_key: crate::ProjectKey,
        action: WorkerStatusAction,
        status: forge_primitives::WorkerStatus,
        worktree: WorktreeDisposition,
    },
    /// A peer-coordination envelope arrived at session `session_id`.
    /// Carries the typed `WrappedPrompt` so the TUI reducer can build
    /// the chat-side echo from real fields instead of having the
    /// workspace forge a `Message::User` carrying prose for the TUI to
    /// re-parse (audit I11). The recipient's LLM still receives the
    /// prose via a separate `Command::Prompt` dispatch - that's the
    /// CLI's input channel and stays text-shaped.
    PeerEnvelopeAppended {
        session_id: String,
        wrapped: crate::mcp::peers::types::WrappedPrompt,
    },
    /// A matched Gotify notification arrived at session `session_id`.
    /// Carries the typed `GotifyNotification` so the TUI reducer builds
    /// the chat-side echo from real fields (mirrors PeerEnvelopeAppended).
    /// The session's LLM receives the same prose via a separate
    /// `Command::Prompt` - this update only drives the visible echo.
    GotifyNotificationAppended {
        session_id: String,
        notification: crate::mcp::gotify::types::GotifyNotification,
    },
    /// A due cron fired into session `session_id`. Carries the fired
    /// prompt text so the TUI reducer builds the chat-side echo (mirrors
    /// GotifyNotificationAppended). The session's LLM receives the same
    /// text via a separate `Command::Prompt` - this update only drives
    /// the visible echo.
    CronPromptAppended {
        session_id: String,
        text: String,
    },
    /// A workspace-originated prompt (cron fire, peer or gotify
    /// delivery, kick) landed while the target session's turn was in
    /// flight. The TUI counts it into the bucket's queued-send bridge
    /// so the spinner stays open across the gap; the prompt itself
    /// rides the usual `Command::Prompt` dispatch.
    PromptQueuedWhileBusy {
        key: SessionKey,
    },
    /// A worker's review turn addressed review comments; `key` is the
    /// session that authored the review (the submit origin). The TUI drops
    /// `message` as a system line into that session's chat so the reviewer
    /// sees the batched tally, and parks `waiting` - how many threads on
    /// `branch` now await a reviewer turn - as the persistent signal both
    /// the Inspector GIT badge and the NEEDS ATTENTION band read.
    ReviewActivityNotice {
        key: SessionKey,
        branch: String,
        waiting: usize,
        message: String,
    },
    /// Dictation models are loaded and the composer may offer to
    /// dictate. App-global (no key): the engine is process-wide and
    /// every session's composer shares the availability; the event's
    /// existence is the signal. Never emitted when `[dictate]` is
    /// disabled, so sessions that cannot dictate render nothing.
    DictateAvailability,
    /// A recording started for the composer at `key`. `floor_db` is the
    /// silence floor the level meter maps onto its zero glyph, so the
    /// bar and the `NoAudio` verdict agree by construction. `generation`
    /// identifies this take among the key's takes: a resolver that
    /// arrives after a newer take started carries a stale one, and the
    /// composer resets on its own generation only.
    DictateStarted {
        key: SessionKey,
        floor_db: f32,
        generation: u64,
    },
    /// One level reading for the recording at `key`: the peak over the
    /// window since the previous reading, in dBFS. Emitted on the
    /// meter clock, not the repaint clock.
    DictateLevel {
        key: SessionKey,
        peak_db: f32,
    },
    /// The take from `key` was submitted and a transcript is in flight.
    DictateTranscribing {
        key: SessionKey,
    },
    /// A take from `key` is decoding window `window` of `total`, so a
    /// long take can show progress. Emitted per window, single-window
    /// takes included; a composer renders only what it wants to.
    /// `generation` is the take's own, as handed out by
    /// [`SessionUpdate::DictateStarted`].
    DictateProgress {
        key: SessionKey,
        generation: u64,
        window: usize,
        total: usize,
    },
    /// A take from `key` is done: insert, notice or reset per
    /// [`DictateOutcome`]. `generation` is the take's own, as handed
    /// out by [`SessionUpdate::DictateStarted`].
    DictateEnded {
        key: SessionKey,
        outcome: DictateOutcome,
        generation: u64,
    },
    FatalError(AppError),
}

impl SessionUpdate {
    /// The [`SessionKey`] this update routes to, or `None` for
    /// updates that target App-level state (`SessionsListed`,
    /// `ServiceStatus`, usage, plugin, key-rename, fatal-error).
    /// Variants carrying a raw `session_id` synthesize a key from it.
    pub fn session_key(&self) -> Option<SessionKey> {
        match self {
            Self::Spawning { key, .. }
            | Self::Connected { key, .. }
            | Self::SessionReplaced { key, .. }
            | Self::ConnectionFailed { key, .. }
            | Self::AuthRequired { key, .. }
            | Self::SlashCommandError { key, .. }
            | Self::SetModeFailed { key, .. }
            | Self::SetModelFailed { key, .. }
            | Self::PermissionRequest { key, .. }
            | Self::QuestionRequest { key, .. }
            | Self::McpOperationError { key, .. }
            | Self::TurnComplete { key, .. }
            | Self::TurnCancelled { key }
            | Self::TurnError { key, .. }
            | Self::ForgeAccountIdentity { key, .. }
            | Self::DictateOverrides { key, .. }
            | Self::DictateDevicePin { key, .. }
            | Self::SessionsListed { key, .. }
            | Self::ReviewActivityNotice { key, .. }
            | Self::PeerInflightStatsChanged { key, .. }
            | Self::DictateStarted { key, .. }
            | Self::DictateLevel { key, .. }
            | Self::DictateTranscribing { key }
            | Self::DictateProgress { key, .. }
            | Self::PromptQueuedWhileBusy { key }
            | Self::DictateEnded { key, .. } => Some(key.clone()),
            Self::RuntimeReloadCompleted { session_id }
            | Self::RuntimeReloadFailed { session_id, .. }
            | Self::ChatAppended { session_id, .. }
            | Self::HookObservation { session_id, .. }
            | Self::StatusSnapshot { session_id, .. }
            | Self::OauthCredentialsSnapshot { session_id, .. }
            | Self::ContextUsageSnapshot { session_id, .. }
            | Self::McpSnapshot { session_id, .. }
            | Self::PeerEnvelopeAppended { session_id, .. }
            | Self::GotifyNotificationAppended { session_id, .. }
            | Self::CronPromptAppended { session_id, .. } => {
                Some(SessionKey::from_session_id(session_id.clone()))
            }
            Self::KeyRenamed { .. }
            | Self::SpawnBucketRetired { .. }
            | Self::ServiceStatus { .. }
            | Self::CatalogLoaded
            | Self::PluginsInventoryUpdated { .. }
            | Self::PluginsInventoryRefreshFailed { .. }
            | Self::PluginsCliActionSucceeded { .. }
            | Self::PluginsCliActionFailed { .. }
            | Self::PluginsUpdateRunProgress { .. }
            | Self::PluginsUpdateRunFinished { .. }
            | Self::PluginsRollbackSucceeded { .. }
            | Self::PluginsRollbackFailed { .. }
            | Self::WorkerStatusChanged { .. }
            | Self::DictateAvailability { .. }
            | Self::FatalError(..) => None,
        }
    }
}

impl std::fmt::Debug for SessionUpdate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Custom Debug - discriminant + key/session_id only.
        // Payloads can be large or non-Debug-friendly; the
        // discriminant + routing key is sufficient for trace logs.
        match self {
            Self::Spawning { key, project_name, .. } => f
                .debug_struct("Spawning")
                .field("key", key)
                .field("project_name", project_name)
                .finish_non_exhaustive(),
            Self::SpawnBucketRetired { key, superseded_by } => f
                .debug_struct("SpawnBucketRetired")
                .field("key", key)
                .field("superseded_by", superseded_by)
                .finish(),
            Self::KeyRenamed { from, to } => {
                f.debug_struct("KeyRenamed").field("from", from).field("to", to).finish()
            }
            Self::Connected { key, .. } => {
                f.debug_struct("Connected").field("key", key).finish_non_exhaustive()
            }
            Self::SessionReplaced { key, previous_key, .. } => f
                .debug_struct("SessionReplaced")
                .field("key", key)
                .field("previous_key", previous_key)
                .finish_non_exhaustive(),
            Self::ConnectionFailed { key, .. } => {
                f.debug_struct("ConnectionFailed").field("key", key).finish_non_exhaustive()
            }
            Self::AuthRequired { key, .. } => {
                f.debug_struct("AuthRequired").field("key", key).finish_non_exhaustive()
            }
            Self::SlashCommandError { key, .. } => {
                f.debug_struct("SlashCommandError").field("key", key).finish_non_exhaustive()
            }
            Self::RuntimeReloadCompleted { session_id } => {
                f.debug_struct("RuntimeReloadCompleted").field("session_id", session_id).finish()
            }
            Self::RuntimeReloadFailed { session_id, .. } => f
                .debug_struct("RuntimeReloadFailed")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::SetModeFailed { key, .. } => {
                f.debug_struct("SetModeFailed").field("key", key).finish_non_exhaustive()
            }
            Self::SetModelFailed { key, .. } => {
                f.debug_struct("SetModelFailed").field("key", key).finish_non_exhaustive()
            }
            Self::PermissionRequest { key, tool_id, .. } => f
                .debug_struct("PermissionRequest")
                .field("key", key)
                .field("tool_id", tool_id)
                .finish_non_exhaustive(),
            Self::QuestionRequest { key, tool_id, .. } => f
                .debug_struct("QuestionRequest")
                .field("key", key)
                .field("tool_id", tool_id)
                .finish_non_exhaustive(),
            Self::McpOperationError { key, .. } => {
                f.debug_struct("McpOperationError").field("key", key).finish_non_exhaustive()
            }
            Self::TurnComplete { key, .. } => {
                f.debug_struct("TurnComplete").field("key", key).finish_non_exhaustive()
            }
            Self::TurnCancelled { key } => {
                f.debug_struct("TurnCancelled").field("key", key).finish()
            }
            Self::TurnError { key, .. } => {
                f.debug_struct("TurnError").field("key", key).finish_non_exhaustive()
            }
            Self::ChatAppended { session_id, .. } => f
                .debug_struct("ChatAppended")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::HookObservation { session_id, .. } => f
                .debug_struct("HookObservation")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::StatusSnapshot { session_id, .. } => f
                .debug_struct("StatusSnapshot")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::ForgeAccountIdentity { key, .. } => {
                f.debug_struct("ForgeAccountIdentity").field("key", key).finish_non_exhaustive()
            }
            Self::DictateOverrides { key, .. } => {
                f.debug_struct("DictateOverrides").field("key", key).finish_non_exhaustive()
            }
            Self::DictateDevicePin { key, .. } => {
                f.debug_struct("DictateDevicePin").field("key", key).finish_non_exhaustive()
            }
            Self::OauthCredentialsSnapshot { session_id, .. } => f
                .debug_struct("OauthCredentialsSnapshot")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::ContextUsageSnapshot { session_id, .. } => f
                .debug_struct("ContextUsageSnapshot")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::McpSnapshot { session_id, .. } => f
                .debug_struct("McpSnapshot")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::SessionsListed { key, sessions } => f
                .debug_struct("SessionsListed")
                .field("key", key)
                .field("count", &sessions.len())
                .finish(),
            Self::ServiceStatus { .. } => f.debug_struct("ServiceStatus").finish_non_exhaustive(),
            Self::PluginsInventoryUpdated { cwd_raw, .. } => f
                .debug_struct("PluginsInventoryUpdated")
                .field("cwd_raw", cwd_raw)
                .finish_non_exhaustive(),
            Self::PluginsInventoryRefreshFailed { cwd_raw, .. } => f
                .debug_struct("PluginsInventoryRefreshFailed")
                .field("cwd_raw", cwd_raw)
                .finish_non_exhaustive(),
            Self::PluginsCliActionSucceeded { cwd_raw, .. } => f
                .debug_struct("PluginsCliActionSucceeded")
                .field("cwd_raw", cwd_raw)
                .finish_non_exhaustive(),
            Self::PluginsCliActionFailed { cwd_raw, .. } => f
                .debug_struct("PluginsCliActionFailed")
                .field("cwd_raw", cwd_raw)
                .finish_non_exhaustive(),
            Self::PluginsUpdateRunProgress { cwd_raw, run } => f
                .debug_struct("PluginsUpdateRunProgress")
                .field("cwd_raw", cwd_raw)
                .field("rows", &run.rows.len())
                .finish(),
            Self::PluginsUpdateRunFinished { cwd_raw, run, .. } => f
                .debug_struct("PluginsUpdateRunFinished")
                .field("cwd_raw", cwd_raw)
                .field("rows", &run.rows.len())
                .finish(),
            Self::PluginsRollbackSucceeded { cwd_raw, plugin_id, .. } => f
                .debug_struct("PluginsRollbackSucceeded")
                .field("cwd_raw", cwd_raw)
                .field("plugin_id", plugin_id)
                .finish_non_exhaustive(),
            Self::PluginsRollbackFailed { cwd_raw, plugin_id, .. } => f
                .debug_struct("PluginsRollbackFailed")
                .field("cwd_raw", cwd_raw)
                .field("plugin_id", plugin_id)
                .finish_non_exhaustive(),
            Self::PeerInflightStatsChanged { key, stats } => f
                .debug_struct("PeerInflightStatsChanged")
                .field("key", key)
                .field("stats", stats)
                .finish(),
            Self::WorkerStatusChanged { project_key, action, status, worktree } => f
                .debug_struct("WorkerStatusChanged")
                .field("project_key", project_key)
                .field("action", action)
                .field("label", &status.label)
                .field("worktree", worktree)
                .finish_non_exhaustive(),
            Self::PeerEnvelopeAppended { session_id, wrapped } => f
                .debug_struct("PeerEnvelopeAppended")
                .field("session_id", session_id)
                .field("correlation_id", &wrapped.correlation_id)
                .field("kind", &wrapped.kind)
                .finish_non_exhaustive(),
            Self::GotifyNotificationAppended { session_id, notification } => f
                .debug_struct("GotifyNotificationAppended")
                .field("session_id", session_id)
                .field("app", &notification.app)
                .field("priority", &notification.priority)
                .finish_non_exhaustive(),
            Self::CronPromptAppended { session_id, .. } => f
                .debug_struct("CronPromptAppended")
                .field("session_id", session_id)
                .finish_non_exhaustive(),
            Self::PromptQueuedWhileBusy { key } => {
                f.debug_struct("PromptQueuedWhileBusy").field("key", key).finish()
            }
            Self::ReviewActivityNotice { key, branch, waiting, .. } => f
                .debug_struct("ReviewActivityNotice")
                .field("key", key)
                .field("branch", branch)
                .field("waiting", waiting)
                .finish_non_exhaustive(),
            Self::DictateAvailability => f.write_str("DictateAvailability"),
            Self::DictateStarted { key, .. } => {
                f.debug_struct("DictateStarted").field("key", key).finish_non_exhaustive()
            }
            Self::DictateLevel { key, peak_db } => {
                f.debug_struct("DictateLevel").field("key", key).field("peak_db", peak_db).finish()
            }
            Self::DictateTranscribing { key } => {
                f.debug_struct("DictateTranscribing").field("key", key).finish()
            }
            Self::DictateProgress { key, generation, window, total } => f
                .debug_struct("DictateProgress")
                .field("key", key)
                .field("generation", generation)
                .field("window", window)
                .field("total", total)
                .finish(),
            Self::DictateEnded { key, outcome, .. } => f
                .debug_struct("DictateEnded")
                .field("key", key)
                .field("outcome", outcome)
                .finish_non_exhaustive(),
            Self::FatalError(err) => f.debug_struct("FatalError").field("error", err).finish(),
            Self::CatalogLoaded => f.write_str("CatalogLoaded"),
        }
    }
}

/// Errors from `Workspace::dispatch`.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("no session task registered for key {0:?}")]
    UnknownSession(SessionKey),
    #[error("session task for key {0:?} has closed its command channel")]
    SessionClosed(SessionKey),
}

#[cfg(test)]
mod workers_command_tests {
    use super::*;

    #[test]
    fn worker_spawn_reply_constructs() {
        let r = WorkerSpawnReply {
            session_id: "abc".into(),
            tag: "forge:worker:reviewer".into(),
            rate_limited_account: None,
            durability_warning: None,
        };
        assert_eq!(r.tag, "forge:worker:reviewer");
    }
}
