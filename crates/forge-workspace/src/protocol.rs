//! Command + SessionUpdate channel envelopes between forge-tui and
//! forge-workspace. TUI dispatches Commands; workspace per-session
//! tasks emit SessionUpdates back via a fan-in channel.
//!
//! ## Dual Command shape (deliberate)
//!
//! Two `Command` enums exist in the workspace: this one, keyed by
//! [`SessionSlot`], and [`forge_primitives::AgentCommand`], keyed by
//! `session_id: String`. They overlap on variant names (Prompt,
//! Cancel, SetMode, …) but serve different boundary layers:
//!
//! - **`forge_workspace::protocol::Command`** is the TUI ↔ workspace
//!   envelope. SessionSlot routing, App-level variants
//!   (SpawnProject / SpawnSession / StartDefault), the
//!   workspace-internal Respond* + MCP cluster.
//! - **`forge_primitives::AgentCommand`** is the workspace ↔ agent
//!   envelope. session_id-keyed, raw shapes the AgentHandle
//!   dispatcher recognises.
//!
//! Collapsing them would force the AgentHandle dispatcher to handle
//! App-level variants it has no business in (SpawnProject is a
//! workspace concern; SessionSlot is a routing concern; neither
//! belongs in the agent layer). The current split keeps each
//! envelope minimal at its respective boundary. The translation
//! happens in `session_task::execute_command_via_handle`.

use std::path::PathBuf;

use forge_agent::client::SessionLaunchSettings;
use forge_primitives::cloud::oauth_credentials::OauthCredentials;
use forge_primitives::cloud::service_status::ServiceSeverity;
use forge_primitives::error::AppError;
use forge_primitives::permission::PermissionMode;
use forge_primitives::permission_interaction::{PermissionOutcome, PermissionRequest};
use forge_primitives::plugins::{
    PluginUpdateRun, PluginUpdateTrigger, PluginsCliActionSuccess, PluginsInventorySnapshot,
};
use forge_primitives::question::{QuestionOutcome, QuestionRequest};
use forge_primitives::review::{ReviewStatus, ReviewThread};
use forge_primitives::runtime::{AvailableModel, CurrentModel, ModeState, TerminalReason};
use forge_primitives::{
    AccountInfo, ForgeAccountIdentity, ImageAttachment, McpOperationError, McpServerStatus,
    Message, SessionId, SessionListEntry,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::SessionSlot;
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
///
/// The request rides beside the sender so a view that attached after the
/// prompt landed still draws what it offers. The stream is a mirror with no
/// backlog, so the update that carried it is gone by then, and the dock's
/// whole point is answering from a view that was not there when it arrived.
pub enum PendingInteractionSlot {
    Permission { tx: oneshot::Sender<PermissionOutcome>, request: Box<PermissionRequest> },
    Question { tx: oneshot::Sender<QuestionOutcome>, request: Box<QuestionRequest> },
}

/// What a seat is held on, as the core kept it: the prompt a view that
/// attached late has no other way to read.
///
/// The three kinds are every member of the category, not the two the dock
/// happened to draw first: a Slack draft parks on a reply the same way a
/// permission and a question do, so a record carrying two of the three
/// told a builder the category was covered when it was not.
#[derive(Clone)]
pub enum PendingAsk {
    Permission(Box<PermissionRequest>),
    Question(Box<QuestionRequest>),
    SlackDraft(Box<forge_primitives::slack::SlackDraft>),
}

impl PendingAsk {
    /// The tool call this prompt is about, which is the id an answer names.
    ///
    /// `None` for a Slack draft: it is answered by `Command::RespondSlackPost`
    /// with the draft's own id, and names no tool call at all.
    pub fn tool_id(&self) -> Option<&str> {
        match self {
            Self::Permission(request) => Some(&request.tool_call.tool_call_id),
            Self::Question(request) => Some(&request.tool_call.tool_call_id),
            Self::SlackDraft(_) => None,
        }
    }
}

impl PendingInteractionSlot {
    /// The request this slot is holding, which is what it offers.
    pub fn ask(&self) -> PendingAsk {
        match self {
            Self::Permission { request, .. } => PendingAsk::Permission(request.clone()),
            Self::Question { request, .. } => PendingAsk::Question(request.clone()),
        }
    }
}

impl std::fmt::Debug for PendingInteractionSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Permission { .. } => f.write_str("PendingInteractionSlot::Permission"),
            Self::Question { .. } => f.write_str("PendingInteractionSlot::Question"),
        }
    }
}

/// Synchronous return from `Command::SpawnWorker` - the session_id
/// that the new worker was issued and the tag value applied. Threaded
/// back to the calling `agents__spawn` Tool impl via the oneshot
/// receiver so the LLM sees `{session_id, tag}` in the tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct WorkerSpawnReply {
    pub session_id: String,
    pub tag: String,
    /// Set to the account name when the walk had to take an account
    /// that is saturated or bailed, because no other account in the pin
    /// declares the project's model. The spawn tool surfaces it as a
    /// `notice`, so the lead sees at spawn that the worker may hit a
    /// 429 right away instead of only finding out when it stalls.
    pub rate_limited_account: Option<String>,
    /// Set when persisting the worker's durable row failed (the store
    /// couldn't open, or the write errored). The worker still spawns, but
    /// it won't survive a forge restart. The spawn tool surfaces it as a
    /// warning so the lead knows the "durable" promise didn't hold for
    /// this one. Mirrors `worktree_cleanup_warning` on despawn.
    pub durability_warning: Option<String>,
    /// Which session the spawn landed on. The handler states what its own
    /// arguments say; the MCP facade restates it with the resolution it
    /// made, which is the only place a `resume_session` fallback is known.
    pub session_choice: SessionChoice,
}

/// Which session a worker spawn landed on, as the spawn tool reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionChoice {
    /// The label's prior session was found and resumed.
    Resumed,
    /// A new session, because the caller did not ask to resume.
    Fresh,
    /// A new session, because the caller asked to resume and the label
    /// had no prior session to resume.
    FreshWithoutPrior,
}

/// Outcome of a [`Command::DespawnWorker`], sent back to the calling
/// `agents__despawn` Tool via the command's `respond` oneshot.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DespawnResult {
    /// The worker was torn down (subprocess killed, dropped from
    /// `live_workers`). `worktree_cleanup_warning`
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
    /// The despawn could not be carried out: the worker's durable row
    /// could not be read or removed, so whether one is there is unknown.
    /// Distinct from [`Self::NotFound`], which claims there is none.
    Failed { reason: String },
}

/// Mutation kind for a `SessionUpdate::WorkerStatusChanged` event.
/// `Added` and `StatusChanged` carry a fresh `WorkerStatus` snapshot;
/// `Removed` carries the last-known snapshot for symmetry but the TUI
/// reducer treats it as a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkerStatusAction {
    Added,
    Removed,
    StatusChanged,
}

/// What has happened to a worker's git worktree as of the
/// `SessionUpdate::WorkerStatusChanged` event carrying it. Only the
/// `agents__despawn` path ever removes one. Among the spawn
/// rollbacks, the dividing line is `Connected`: one that fires before
/// the subprocess connected reports [`Self::Absent`], because claude
/// never ran to create a worktree, while a rollback after `Connected`
/// (a failed tag-write) reports [`Self::untouched`], because by then
/// the worktree is on disk. Every other emitter reports
/// [`Self::untouched`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// A reply channel for a command that carries none.
///
/// A command off the socket has no channel to answer down - its reply is a
/// socket message instead - so the handler is given this rather than being
/// made to ask: sending into it reports the closed channel it is, which
/// every call site already discards.
pub fn unanswerable<T>() -> oneshot::Sender<T> {
    oneshot::channel().0
}

impl WorktreeDisposition {
    /// The disposition when nothing has touched the worktree.
    pub fn untouched(is_git_repo_at_spawn: bool) -> Self {
        if is_git_repo_at_spawn { Self::Intact } else { Self::Absent }
    }
}

/// Command envelope: forge-tui -> forge-workspace.
///
/// Every variant carries a `SessionSlot` identifying the target
/// session task. `Workspace::dispatch` fans the variant into the
/// matching task's command receiver.
///
/// A reply channel cannot cross a socket, so the four fields carrying one
/// are `Option` and skipped on the wire: `#[serde(skip)]` reconstructs a
/// field on deserialize, which needs a `Default`, and a bare
/// `oneshot::Sender` has none while an `Option` does.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Command {
    Prompt {
        key: SessionSlot,
        text: String,
        attachments: Vec<ImageAttachment>,
    },
    Cancel {
        key: SessionSlot,
    },
    /// Ask the session task to emit the conversation it is carrying, as
    /// [`SessionUpdate::HistoryReplayed`].
    ///
    /// **The task is asked rather than the transcript read**, because the task
    /// is what PRODUCES the conversation: it is handed the history at connect
    /// and it emits every frame after it, so a history it hands back is in its
    /// own order with the frames around it. A reader that walked the file
    /// instead would be a second producer of the same truth, and the two
    /// cannot be reconciled - the read picks up rows written while it runs,
    /// which the stream also delivers, and no identity ties them together.
    ///
    /// A no-op for a seat whose session is gone: nothing is emitted and the
    /// asker's wait times out.
    ReplayConversation {
        key: SessionSlot,
    },
    SetMode {
        key: SessionSlot,
        mode: PermissionMode,
    },
    SetModel {
        key: SessionSlot,
        model: String,
    },
    NewSession {
        key: SessionSlot,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    },
    ResumeSession {
        key: SessionSlot,
        session_id: String,
        cwd: String,
        launch_settings: SessionLaunchSettings,
    },
    RespondPermission {
        key: SessionSlot,
        tool_id: String,
        outcome: PermissionOutcome,
    },
    /// Answer a held Slack draft. The blocked `slack__post` handler is
    /// awaiting this decision; `approved: false` means nothing posts.
    /// App-level command (`key()` returns `None`): the registry is
    /// workspace state, and `key` names the owner the answer is
    /// checked against, not a SessionTask route.
    RespondSlackPost {
        key: SessionSlot,
        id: uuid::Uuid,
        approved: bool,
    },
    RespondQuestion {
        key: SessionSlot,
        tool_id: String,
        outcome: QuestionOutcome,
    },
    /// Set one `/dictate` overlay axis for this session, or clear
    /// every axis at once. Workspace state on the `DomainSession`,
    /// never routed to the agent; the echo lands as
    /// `SessionUpdate::DictateOverrides`.
    SetDictateOverride {
        key: SessionSlot,
        update: crate::dictate::DictateOverrideUpdate,
    },
    /// Clear every `/dictate` override this session holds.
    ResetDictateOverrides {
        key: SessionSlot,
    },
    /// Set the `/dictate` input-device pick, or clear it back to the
    /// configured pin. Workspace state shared by every session, never
    /// routed to the agent; the echo lands as
    /// `SessionUpdate::DictateDevicePin`.
    SetDictateDevice {
        key: SessionSlot,
        pick: Option<crate::dictate::DictateDeviceChoice>,
    },
    /// Reconnect a configured MCP server.
    ReconnectMcpServer {
        key: SessionSlot,
        server_name: String,
    },
    /// Toggle a configured MCP server on/off.
    ToggleMcpServer {
        key: SessionSlot,
        server_name: String,
        enabled: bool,
    },
    /// User clicked an inactive project to wake it. No `key` - the
    /// session doesn't exist yet; workspace resolves the id it will run
    /// under and emits `SessionUpdate::Spawning` then `::Connected` on
    /// it. This is an App-level Command, not per-session.
    SpawnProject {
        project_name: String,
        launch_settings: SessionLaunchSettings,
    },
    /// User clicked a non-lead session row. Workspace spawns an
    /// agent for the slot the row names, under the id its row holds,
    /// and emits `Spawning` then `Connected` on it.
    ///
    /// `role` is what the dispatcher knows the row to be. The row is a
    /// catalog entry, so the spawn cannot tell a worker's session from a
    /// lead's by anything it holds, and a worker handed a lead's tool
    /// surface fails silently - so the caller states it.
    SpawnSession {
        key: SessionSlot,
        role: SpawnRole,
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
    /// Cross-project delivery (#114 v1). Dispatched by the
    /// `mcp__forge__agents__send_message` tool impl via
    /// `WorkspaceFacade::deliver_peer_prompt`. Routed to
    /// `spawn::handle_deliver_peer_prompt` which: (a) resolves
    /// `target_project` to a `SessionSlot`; (b) if target is running,
    /// dispatches a plain `Command::Prompt` carrying the wrapper
    /// prose; (c) if sleeping, parks `wrapped` for target's owner and
    /// dispatches `Command::SpawnProject`.
    ///
    /// App-level command (`key()` returns `None`); workspace routes
    /// to the App-level handler in `spawn.rs`. The `caller` field is
    /// the source session's key, carried so a parking that never lands
    /// can route its delivery notice back.
    DeliverPeerPrompt {
        caller: SessionSlot,
        target_project: String,
        wrapped: WrappedPrompt,
    },
    /// Spawn a new worker session in `project_key`. Dispatched by
    /// the `agents__spawn` MCP Tool impl after caller-tag validation,
    /// or by the lead Connected hook when reviving
    /// across-restart workers.
    ///
    /// `resume_existing`: when `Some(session_id)`, the handler resumes
    /// the named session via `SessionTarget::Session` instead of
    /// spawning fresh. The session_id MUST already carry the
    /// `forge:worker:<label>` tag (the lead Connected hook verifies
    /// this before dispatching). `WorkerEntry::needs_tag` is set false
    /// on the resume path since the tag is already on disk. `None`
    /// preserves the original fresh-spawn path used by `agents__spawn`.
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
        /// The slot of the session issuing the spawn - normally the
        /// project's lead.
        spawned_by: SessionSlot,
        resume_existing: Option<String>,
        /// First message delivered as the worker's user turn on Connected,
        /// via the rate-limited kick dispatcher. Either `agents__spawn`'s
        /// `kick`, or - on a re-spawn - the row's `resume_kick` or the
        /// generic restart note, which is the only thing that wakes a
        /// resuming worker. `None` -> no kick (the worker idles until
        /// told).
        kick: Option<String>,
        /// Re-orient message delivered instead of the generic restart
        /// note when this worker is later resumed. `None` on a re-spawn
        /// keeps whatever the row already holds.
        resume_kick: Option<String>,
        /// Whether this worker keeps the built-in `AskUserQuestion`
        /// tool. Read from the persisted row on a re-spawn, so it
        /// survives a forge restart.
        interactive: bool,
        /// True only for boot/reconnect re-spawns of persisted
        /// workers, which bypass the project's worker cap:
        /// they restore state the user already had, and their reply is
        /// dropped, so a refusal there would strand rows with no
        /// caller to hear it.
        from_boot_respawn: bool,
        #[serde(skip)]
        return_to: Option<oneshot::Sender<Result<WorkerSpawnReply, String>>>,
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
    /// terminate its agent, drop it from `live_workers`, AND clean up
    /// its git worktree. Dispatched by the
    /// `agents__despawn` MCP tool (lead-only). Unlike `CloseWorker`
    /// (the TUI X-button), this also removes the worker's git worktree:
    /// a clean worktree is removed; a dirty one (uncommitted/untracked
    /// or unpushed commits) blocks the despawn unless `force`. The
    /// outcome flows back via `respond`.
    DespawnWorker {
        project_key: crate::ProjectKey,
        label: String,
        force: bool,
        #[serde(skip)]
        respond: Option<oneshot::Sender<DespawnResult>>,
    },
    /// Deliver a wrapped peer-style prompt to a worker. Same envelope
    /// as `DeliverPeerPrompt` but addressed by worker label within
    /// the caller's project rather than by cross-project name.
    DeliverWorkerPrompt {
        caller: SessionSlot,
        project_key: crate::ProjectKey,
        target_label: String,
        wrapped: WrappedPrompt,
    },
    /// Deliver a wrapped prompt from a worker back to its lead.
    /// Dispatched by the `agents__send_message` Tool impl
    /// when the caller addresses `label="lead"`. The target
    /// `SessionSlot` is resolved at Tool dispatch time from the
    /// worker's `spawned_by_session_id` so the handler can deliver
    /// directly without re-doing the lookup against a possibly-mutated
    /// `live_workers` map. Wire shape is identical to
    /// `DeliverWorkerPrompt` (same PeerEnvelopeAppended echo + same
    /// `Command::Prompt` dispatch into the target session).
    DeliverWorkerPromptToLead {
        caller: SessionSlot,
        target_lead_key: SessionSlot,
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
    /// Begin dictating into the composer at `key`. App-level command
    /// carrying the origin key, like `DeliverPeerPrompt`: the
    /// microphone is process-global, so the recording lifecycle lives
    /// on `Workspace` rather than on one `SessionTask`, while the
    /// events route back to the session that started it.
    DictateStart {
        key: SessionSlot,
    },
    /// Submit (`submit = true`) or abandon the take started by `key`.
    /// During recording this is release-to-submit vs discard; during a
    /// transcription in flight it abandons the ticket.
    DictateStop {
        key: SessionSlot,
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
    /// Release the session `session_key` (cascade-aware: a project
    /// lead's workers terminate first). Dispatched by the TUI's
    /// per-row close click; the TUI removes its own bucket around the
    /// dispatch. App-level command (`key()` returns `None`); routed
    /// inline.
    CloseSession {
        session_key: SessionSlot,
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
        #[serde(skip)]
        respond: Option<oneshot::Sender<bool>>,
    },
    /// Submit (seal) the listed threads as one review round.
    /// `respond` carries the minted review, `None` when the store
    /// write failed. App-level command (`key()` returns `None`).
    SubmitReview {
        project: String,
        branch: String,
        summary: Option<String>,
        thread_ids: Vec<String>,
        origin: SessionSlot,
        #[serde(skip)]
        respond: Option<oneshot::Sender<Option<forge_primitives::ReviewSet>>>,
    },
}

impl Command {
    /// The `SessionSlot` this command routes to, or `None` for
    /// App-level commands (`SpawnProject`, `SpawnSession`,
    /// `StartDefault`). `Workspace::dispatch` routes `None` commands
    /// to its app-level handler (which resolves the id the new session
    /// will run under and spawns the agent); `Some(key)` commands route
    /// to the matching SessionTask.
    pub fn key(&self) -> Option<&SessionSlot> {
        match self {
            Self::Prompt { key, .. }
            | Self::Cancel { key }
            | Self::ReplayConversation { key }
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
            | Self::OpenUrl { .. }
            | Self::SaveReviewThreads { .. }
            | Self::RemoveReviewThread { .. }
            | Self::SetReviewThreadStatus { .. }
            | Self::CloseSession { .. }
            | Self::UpsertReviewThread { .. }
            | Self::RespondSlackPost { .. }
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
            Self::ReplayConversation { key } => {
                f.debug_struct("ReplayConversation").field("key", key).finish()
            }
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
            Self::RespondSlackPost { key, id, approved } => f
                .debug_struct("RespondSlackPost")
                .field("key", key)
                .field("id", id)
                .field("approved", approved)
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
            Self::SpawnSession { key, .. } => {
                f.debug_struct("SpawnSession").field("key", key).finish_non_exhaustive()
            }
            Self::StartDefault { project_name, .. } => f
                .debug_struct("StartDefault")
                .field("project_name", project_name)
                .finish_non_exhaustive(),
            Self::DeliverPeerPrompt { caller, target_project, .. } => f
                .debug_struct("DeliverPeerPrompt")
                .field("caller", caller)
                .field("target_project", target_project)
                .finish_non_exhaustive(),
            Self::SpawnWorker { project_key, label, spawned_by, .. } => f
                .debug_struct("SpawnWorker")
                .field("project_key", project_key)
                .field("label", label)
                .field("spawned_by", spawned_by)
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

/// The role a caller states for a spawn: a project's lead, or a worker
/// named by its label.
///
/// Both the session's tool surface and its slot's label come from this
/// one value, so they cannot disagree - a caller that says `Worker`
/// gives a worker's tool surface AND a worker's slot. Every spawn
/// states one; forge has no keyless form, because a role it cannot
/// state is one it would have to guess.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SpawnRole {
    Lead,
    Worker {
        label: String,
        /// Whether this spawn wrote the worker's durable row rather than
        /// adopting one that was already there. A rollback may take the
        /// row away only when it did: a resume and a boot re-spawn are
        /// handed a row that holds the worker's charter, kick and the id
        /// being resumed, so the row is the worker, not this spawn's
        /// leftover.
        wrote_row: bool,
    },
}

/// Who submitted the prompt a [`SessionUpdate::ChatAppended`] frame carries.
///
/// The CLI does not echo a prompt handed to it on stdin, so the frame that
/// draws one is forged, and whether a view has already drawn those words is a
/// fact about the dispatch rather than about the words. It is therefore set
/// where the dispatch happens, by which entry the caller took, and never read
/// off the wire.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum PromptOrigin {
    /// The forge process's own input handler submitted it, so that view has
    /// already drawn the words and must not draw them a second time.
    Ui,
    /// A view submitted it over the socket, and no view drew it: a composer
    /// that is not optimistic has only this frame to draw its own send from.
    View,
}

/// How loudly a [`SessionUpdate::Notice`] reads.
///
/// Two levels rather than a scale, because the core has two things to say:
/// what a command found, and why one did not run.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Debug)]
#[serde(rename_all = "snake_case")]
pub enum NoticeSeverity {
    /// A command's own answer.
    Info,
    /// One that did not run.
    Error,
}

/// Update envelope: forge-workspace -> forge-tui.
///
/// Permission/Question variants do NOT carry response oneshots -
/// responses flow back via `Command::Respond*`. The workspace stores
/// the oneshot in `DomainSession.pending_interactions` when emitting
/// these variants.
///
/// `Clone` is what lets the fan-out hand one emit to more than one
/// subscriber, so every variant's payload must stay cloneable.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionUpdate {
    /// Workspace is spawning a session (in response to
    /// `Command::SpawnProject` / `Command::SpawnSession` /
    /// `Command::StartDefault`). TUI creates a placeholder UiSession
    /// under `key` and shows the "Waking {display_name}…" message. The
    /// matching `Connected` lands soon after, under the same key.
    Spawning {
        key: SessionSlot,
        project_name: String,
        cwd: String,
        display_name: String,
    },
    Connected {
        key: SessionSlot,
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
    /// The conversation a seat is carrying, asked for by
    /// `Command::ReplayConversation`.
    ///
    /// **It is not a `Connected`, and the difference is not bookkeeping.** A
    /// view reads `Connected` as a session STARTING - it seeds the bucket,
    /// adopts the id, resets the mode and re-tags - so a replay wearing that
    /// shape would make every attached view re-seed the seat as though it had
    /// just launched. This says only: here is the conversation you asked for.
    ///
    /// It exists so a consumer joining a session it did not watch begin can be
    /// handed the history by the task that PRODUCES it, in that task's own
    /// emission order, rather than reading the transcript alongside the
    /// frames - two producers of one truth, which is a race no reconciliation
    /// closes.
    HistoryReplayed {
        key: SessionSlot,
        history: Vec<Message>,
        compaction_count: u32,
    },
    /// The slot's occupant changed under it - a `/new`, a `/resume`, a
    /// login or a logout. The slot keeps its bucket, in its place on
    /// screen; its contents reset, meaning the rendered conversation,
    /// the history, the per-session overrides and any waiter holding a
    /// tool call that is never coming back. `session_id` is the new
    /// occupant.
    SessionReplaced {
        key: SessionSlot,
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
        key: SessionSlot,
        message: String,
        fatal: bool,
    },
    AuthRequired {
        key: SessionSlot,
        method_name: String,
        method_description: String,
    },
    SlashCommandError {
        key: SessionSlot,
        message: String,
    },
    /// One line the core has for a view about `key`, which nothing else
    /// carries.
    ///
    /// A command answered where both views dispatch has no other way to say
    /// anything: the CLI emits no frame for a command it never saw, and the
    /// line is the core's own answer rather than a view's opinion - what a
    /// command found, or why it refused.
    ///
    /// **Live only, and that is intended.** The CLI writes no transcript row
    /// for a line it never produced, so a view draws this on arrival and a
    /// page that attaches afterwards has nothing to read it from. It is a
    /// statement about the moment a command ran, not a record of the
    /// conversation.
    Notice {
        key: SessionSlot,
        severity: NoticeSeverity,
        text: String,
    },
    RuntimeReloadCompleted {
        key: SessionSlot,
    },
    RuntimeReloadFailed {
        key: SessionSlot,
        message: String,
    },
    /// The CLI refused (or failed) a `set_permission_mode` control
    /// request for `key`. `message` carries the underlying error text;
    /// the TUI rolls the optimistic mode chip back and surfaces it.
    SetModeFailed {
        key: SessionSlot,
        mode: PermissionMode,
        message: String,
    },
    /// The CLI refused (or failed) a `set_model` control request for
    /// `key`. `message` carries the underlying error text; the TUI
    /// rolls the optimistic model change back and surfaces it.
    SetModelFailed {
        key: SessionSlot,
        model: String,
        message: String,
    },
    /// Permission prompt. No response_tx - TUI replies via
    /// `Command::RespondPermission { tool_id, outcome }`.
    PermissionRequest {
        key: SessionSlot,
        tool_id: String,
        request: PermissionRequest,
    },
    /// AskUserQuestion prompt. Same shape; reply via
    /// `Command::RespondQuestion { tool_id, outcome }`.
    QuestionRequest {
        key: SessionSlot,
        tool_id: String,
        request: QuestionRequest,
    },
    /// The interaction `tool_id` was answered, so every view holding the
    /// prompt can drop it. Answering leaves the pending set either way,
    /// and this is the only thing that says so on the stream: without it a
    /// second view keeps drawing a prompt that is already settled. The TUI
    /// never met that, because it is the only view.
    PendingInteractionResolved {
        key: SessionSlot,
        tool_id: String,
    },
    McpOperationError {
        key: SessionSlot,
        error: McpOperationError,
    },
    TurnComplete {
        key: SessionSlot,
        terminal_reason: Option<TerminalReason>,
    },
    TurnCancelled {
        key: SessionSlot,
    },
    TurnError {
        key: SessionSlot,
        message: String,
        /// The failure's class, where the producer that built the event could
        /// name one. **`None` is "not classified here" rather than "not
        /// auth"**: a producer that never classified and an auth failure are
        /// different facts, and a reader that folds them together holds a
        /// session on `/login` for an error that was never about login.
        class: Option<TurnErrorClass>,
        terminal_reason: Option<TerminalReason>,
    },
    ChatAppended {
        key: SessionSlot,
        msg: Message,
        /// Set only on a prompt frame, where it says who submitted the words.
        ///
        /// `None` for every frame the CLI sent and for every turn a delivery
        /// forges, so a reader that ignores this behaves as it did before the
        /// field existed.
        #[serde(default)]
        origin: Option<PromptOrigin>,
    },
    HookObservation {
        key: SessionSlot,
        tool_use_id: Option<String>,
        permission_mode: Option<String>,
        effort: Option<String>,
        agent_id: Option<String>,
        agent_type: Option<String>,
    },
    StatusSnapshot {
        key: SessionSlot,
        account: AccountInfo,
        forge_account: Option<ForgeAccountIdentity>,
    },
    ForgeAccountIdentity {
        key: SessionSlot,
        display_name: String,
    },
    /// The full override set a session holds after a `/dictate` edit
    /// landed. Sent after every `SetDictateOverride` and
    /// `ResetDictateOverrides` so the dialog's markers and its reset
    /// row read from this, not from a TUI-side copy.
    DictateOverrides {
        key: SessionSlot,
        overrides: crate::dictate::DictateOverrides,
    },
    /// The input-device pick in force after a `/dictate` device edit
    /// landed - workspace state shared by every session. Sent after
    /// every `SetDictateDevice`, and alongside the overrides echo by
    /// a Reset.
    DictateDevicePin {
        key: SessionSlot,
        pick: Option<crate::dictate::DictateDeviceChoice>,
    },
    OauthCredentialsSnapshot {
        key: SessionSlot,
        credentials: Option<OauthCredentials>,
    },
    ContextUsageSnapshot {
        key: SessionSlot,
        percentage: Option<u8>,
        /// Raw model context-window size in tokens (e.g. `1_000_000`
        /// for Opus 1M). `None` until the upstream probe reports it.
        max_tokens: Option<u64>,
    },
    McpSnapshot {
        key: SessionSlot,
        servers: Vec<McpServerStatus>,
        error: Option<String>,
    },
    SessionsListed {
        /// Bucket this session list belongs to. The catalog scan that
        /// produces `sessions` runs against the spawning session's
        /// `cwd`, so the listing is project-scoped - routing onto
        /// the requesting bucket prevents another session's `/resume`
        /// autocomplete from inheriting a stale project's list.
        key: SessionSlot,
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
    /// A claude version probe landed and moved what the views draw. The
    /// snapshot itself is not carried, so this is only the wake-up: the web
    /// view re-reads through the surface's `cli_version` verb, the TUI
    /// through the workspace method it still reads directly.
    CliVersionChanged,
    /// The account pool moved: an account's loading state, its cached usage
    /// snapshot, or the gateway listener's readiness.
    ///
    /// **The pool's state is the snapshot's, and this carries none of it.** A
    /// payload would be a second copy of what `HomeWire::accounts` already
    /// states, and the two would drift; this is the wake-up, and a view
    /// re-reads. It carries no seat because the pool belongs to none, which is
    /// also what routes it to a home subscriber and to nobody else.
    AccountsChanged,
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
    /// A peer-coordination envelope arrived at `key`.
    /// Carries the typed `WrappedPrompt` so the TUI reducer can build
    /// the chat-side echo from real fields instead of having the
    /// workspace forge a `Message::User` carrying prose for the TUI to
    /// re-parse (audit I11). The recipient's LLM still receives the
    /// prose via a separate `Command::Prompt` dispatch - that's the
    /// CLI's input channel and stays text-shaped.
    PeerEnvelopeAppended {
        key: SessionSlot,
        wrapped: crate::mcp::peers::types::WrappedPrompt,
    },
    /// A matched Gotify notification arrived at `key`.
    /// Carries the typed `GotifyNotification` so the TUI reducer builds
    /// the chat-side echo from real fields (mirrors PeerEnvelopeAppended).
    /// The session's LLM receives the same prose via a separate
    /// `Command::Prompt` - this update only drives the visible echo.
    GotifyNotificationAppended {
        key: SessionSlot,
        notification: crate::mcp::gotify::types::GotifyNotification,
    },
    /// A due cron fired into `key`. Carries the fired
    /// prompt text so the TUI reducer builds the chat-side echo (mirrors
    /// GotifyNotificationAppended). The session's LLM receives the same
    /// text via a separate `Command::Prompt` - this update only drives
    /// the visible echo.
    CronPromptAppended {
        key: SessionSlot,
        text: String,
    },
    /// A matched Slack message arrived at `key`. Carries the
    /// prose rather than the typed message: the prose builder is `pub(crate)`
    /// to forge-workspace, so a typed message would force the TUI to rebuild
    /// the very format it parses. The session's LLM receives the same prose
    /// via a separate `Command::Prompt` - this update only drives the visible
    /// echo (mirrors GotifyNotificationAppended).
    SlackMessageAppended {
        key: SessionSlot,
        prose: String,
    },
    /// A composed Slack message is waiting for the user's decision in the
    /// dock prompt. The authoring tool handler is blocked on a oneshot
    /// until `Command::RespondSlackPost` answers it, so nothing posts
    /// while this is outstanding.
    SlackPostPending {
        key: SessionSlot,
        draft: forge_primitives::slack::SlackDraft,
    },
    /// A held Slack draft left the core's registry: answered in some view,
    /// expired, or its asking session gone. Each view keeps its own copy of
    /// the parked draft, so this is the only update that clears it - and
    /// `ending` is what a view that did not answer it says happened.
    SlackDraftResolved {
        key: SessionSlot,
        id: Uuid,
        ending: forge_primitives::slack::SlackDraftEnding,
    },
    /// A workspace-originated prompt (cron fire, peer, gotify or slack
    /// delivery, kick) landed while the target session's turn was in
    /// flight. The TUI counts it into the bucket's queued-send bridge
    /// so the spinner stays open across the gap; the prompt itself
    /// rides the usual `Command::Prompt` dispatch.
    PromptQueuedWhileBusy {
        key: SessionSlot,
    },
    /// A worker's review turn addressed review comments; `key` is the
    /// session that authored the review (the submit origin). The TUI drops
    /// `message` as a system line into that session's chat so the reviewer
    /// sees the batched tally, and parks `waiting` - how many threads on
    /// `branch` now await a reviewer turn - as the persistent signal both
    /// the Inspector GIT badge and the NEEDS ATTENTION band read.
    ReviewActivityNotice {
        key: SessionSlot,
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
        key: SessionSlot,
        floor_db: f32,
        generation: u64,
    },
    /// One level reading for the recording at `key`: the peak over the
    /// window since the previous reading, in dBFS. Emitted on the
    /// meter clock, not the repaint clock.
    DictateLevel {
        key: SessionSlot,
        peak_db: f32,
    },
    /// The take from `key` was submitted and a transcript is in flight.
    DictateTranscribing {
        key: SessionSlot,
    },
    /// A take from `key` has settled `done` segments of its
    /// transcription. `total` is `None` while the recording is still
    /// open, because a live take cannot know how many segments it will
    /// produce, and the final count once the take was stopped and its
    /// tail submitted. Single-segment takes report at stop only; a
    /// composer renders only what it wants to. `generation` is the
    /// take's own, as handed out by [`SessionUpdate::DictateStarted`].
    DictateProgress {
        key: SessionSlot,
        generation: u64,
        done: usize,
        total: Option<usize>,
    },
    /// A take from `key` is done: insert, notice or reset per
    /// [`DictateOutcome`]. `generation` is the take's own, as handed
    /// out by [`SessionUpdate::DictateStarted`].
    DictateEnded {
        key: SessionSlot,
        outcome: DictateOutcome,
        generation: u64,
    },
    FatalError(AppError),
}

impl SessionUpdate {
    /// The [`SessionSlot`] this update routes to, or `None` for
    /// updates that target App-level state (`ServiceStatus`, catalog,
    /// plugin, fatal-error). Every identity-carrying variant names a
    /// slot, so nothing is synthesized here.
    pub fn slot(&self) -> Option<&SessionSlot> {
        match self {
            Self::Spawning { key, .. }
            | Self::Connected { key, .. }
            | Self::HistoryReplayed { key, .. }
            | Self::SessionReplaced { key, .. }
            | Self::ConnectionFailed { key, .. }
            | Self::AuthRequired { key, .. }
            | Self::SlashCommandError { key, .. }
            | Self::Notice { key, .. }
            | Self::SetModeFailed { key, .. }
            | Self::SetModelFailed { key, .. }
            | Self::PermissionRequest { key, .. }
            | Self::QuestionRequest { key, .. }
            | Self::PendingInteractionResolved { key, .. }
            | Self::McpOperationError { key, .. }
            | Self::TurnComplete { key, .. }
            | Self::TurnCancelled { key }
            | Self::TurnError { key, .. }
            | Self::ForgeAccountIdentity { key, .. }
            | Self::DictateOverrides { key, .. }
            | Self::DictateDevicePin { key, .. }
            | Self::SessionsListed { key, .. }
            | Self::ReviewActivityNotice { key, .. }
            | Self::DictateStarted { key, .. }
            | Self::DictateLevel { key, .. }
            | Self::DictateTranscribing { key }
            | Self::DictateProgress { key, .. }
            | Self::PromptQueuedWhileBusy { key }
            | Self::DictateEnded { key, .. }
            | Self::SlackPostPending { key, .. }
            | Self::SlackDraftResolved { key, .. }
            | Self::RuntimeReloadCompleted { key }
            | Self::RuntimeReloadFailed { key, .. }
            | Self::ChatAppended { key, .. }
            | Self::HookObservation { key, .. }
            | Self::StatusSnapshot { key, .. }
            | Self::OauthCredentialsSnapshot { key, .. }
            | Self::ContextUsageSnapshot { key, .. }
            | Self::McpSnapshot { key, .. }
            | Self::PeerEnvelopeAppended { key, .. }
            | Self::GotifyNotificationAppended { key, .. }
            | Self::CronPromptAppended { key, .. }
            | Self::SlackMessageAppended { key, .. } => Some(key),
            Self::ServiceStatus { .. }
            | Self::CatalogLoaded
            | Self::CliVersionChanged
            | Self::AccountsChanged
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
            Self::Connected { key, .. } => {
                f.debug_struct("Connected").field("key", key).finish_non_exhaustive()
            }
            Self::HistoryReplayed { key, .. } => {
                f.debug_struct("HistoryReplayed").field("key", key).finish_non_exhaustive()
            }
            Self::SessionReplaced { key, .. } => {
                f.debug_struct("SessionReplaced").field("key", key).finish_non_exhaustive()
            }
            Self::ConnectionFailed { key, .. } => {
                f.debug_struct("ConnectionFailed").field("key", key).finish_non_exhaustive()
            }
            Self::AuthRequired { key, .. } => {
                f.debug_struct("AuthRequired").field("key", key).finish_non_exhaustive()
            }
            Self::SlashCommandError { key, .. } => {
                f.debug_struct("SlashCommandError").field("key", key).finish_non_exhaustive()
            }
            Self::Notice { key, .. } => {
                f.debug_struct("Notice").field("key", key).finish_non_exhaustive()
            }
            Self::RuntimeReloadCompleted { key } => {
                f.debug_struct("RuntimeReloadCompleted").field("key", key).finish()
            }
            Self::RuntimeReloadFailed { key, .. } => {
                f.debug_struct("RuntimeReloadFailed").field("key", key).finish_non_exhaustive()
            }
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
            Self::PendingInteractionResolved { key, tool_id } => f
                .debug_struct("PendingInteractionResolved")
                .field("key", key)
                .field("tool_id", tool_id)
                .finish(),
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
            Self::ChatAppended { key, .. } => {
                f.debug_struct("ChatAppended").field("key", key).finish_non_exhaustive()
            }
            Self::HookObservation { key, .. } => {
                f.debug_struct("HookObservation").field("key", key).finish_non_exhaustive()
            }
            Self::StatusSnapshot { key, .. } => {
                f.debug_struct("StatusSnapshot").field("key", key).finish_non_exhaustive()
            }
            Self::ForgeAccountIdentity { key, .. } => {
                f.debug_struct("ForgeAccountIdentity").field("key", key).finish_non_exhaustive()
            }
            Self::DictateOverrides { key, .. } => {
                f.debug_struct("DictateOverrides").field("key", key).finish_non_exhaustive()
            }
            Self::DictateDevicePin { key, .. } => {
                f.debug_struct("DictateDevicePin").field("key", key).finish_non_exhaustive()
            }
            Self::OauthCredentialsSnapshot { key, .. } => {
                f.debug_struct("OauthCredentialsSnapshot").field("key", key).finish_non_exhaustive()
            }
            Self::ContextUsageSnapshot { key, .. } => {
                f.debug_struct("ContextUsageSnapshot").field("key", key).finish_non_exhaustive()
            }
            Self::McpSnapshot { key, .. } => {
                f.debug_struct("McpSnapshot").field("key", key).finish_non_exhaustive()
            }
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
            Self::WorkerStatusChanged { project_key, action, status, worktree } => f
                .debug_struct("WorkerStatusChanged")
                .field("project_key", project_key)
                .field("action", action)
                .field("label", &status.label)
                .field("worktree", worktree)
                .finish_non_exhaustive(),
            Self::PeerEnvelopeAppended { key, wrapped } => f
                .debug_struct("PeerEnvelopeAppended")
                .field("key", key)
                .field("id", &wrapped.id)
                .field("kind", &wrapped.kind)
                .finish_non_exhaustive(),
            Self::GotifyNotificationAppended { key, notification } => f
                .debug_struct("GotifyNotificationAppended")
                .field("key", key)
                .field("app", &notification.app)
                .field("priority", &notification.priority)
                .finish_non_exhaustive(),
            Self::CronPromptAppended { key, .. } => {
                f.debug_struct("CronPromptAppended").field("key", key).finish_non_exhaustive()
            }
            Self::SlackMessageAppended { key, .. } => {
                f.debug_struct("SlackMessageAppended").field("key", key).finish_non_exhaustive()
            }
            Self::SlackPostPending { key, draft } => f
                .debug_struct("SlackPostPending")
                .field("key", key)
                .field("workspace", &draft.workspace)
                .field("conversation", &draft.conversation)
                .finish_non_exhaustive(),
            Self::SlackDraftResolved { key, id, ending } => f
                .debug_struct("SlackDraftResolved")
                .field("key", key)
                .field("id", id)
                .field("ending", ending)
                .finish(),
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
            Self::DictateProgress { key, generation, done, total } => f
                .debug_struct("DictateProgress")
                .field("key", key)
                .field("generation", generation)
                .field("done", done)
                .field("total", total)
                .finish(),
            Self::DictateEnded { key, outcome, .. } => f
                .debug_struct("DictateEnded")
                .field("key", key)
                .field("outcome", outcome)
                .finish_non_exhaustive(),
            Self::FatalError(err) => f.debug_struct("FatalError").field("error", err).finish(),
            Self::CatalogLoaded => f.write_str("CatalogLoaded"),
            Self::CliVersionChanged => f.write_str("CliVersionChanged"),
            Self::AccountsChanged => f.write_str("AccountsChanged"),
        }
    }
}

/// Errors from the dispatch boundary. `UnknownSession` and
/// `SessionClosed` are returned by `Workspace::dispatch`;
/// `NoActiveSession` by `App::dispatch_command`, which has no slot to
/// name.
#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("no active session")]
    NoActiveSession,
    #[error("no session task registered for key {0:?}")]
    UnknownSession(SessionSlot),
    #[error("session task for key {0:?} has closed its command channel")]
    SessionClosed(SessionSlot),
    #[error(
        "no prompt of that kind is waiting on {tool_id} for key {key:?}: it has been answered, or it asked something else"
    )]
    NoPromptWaiting { key: SessionSlot, tool_id: String },
    /// A Slack answer named a draft the registry does not hold. Its own
    /// word rather than [`Self::NoPromptWaiting`]'s: a draft is answered by
    /// its own id and names no tool call, and this line is drawn where the
    /// dock stood. The three endings are the whole set a draft can have -
    /// answered, expired, or its asking session gone - so the line names
    /// them rather than guessing which one.
    #[error(
        "that Slack draft is no longer waiting: it has been answered, it expired, or its asking session went away"
    )]
    NoDraftWaiting { key: SessionSlot, id: Uuid },
}

#[cfg(test)]
mod session_update_variants {
    /// The client's reducer classifies a `SessionUpdate` by the name it crosses
    /// under, and a name it has no line for is a lookup that misses: the session
    /// page stops following its seat and keeps drawing the last record it read,
    /// with nothing to say it has fallen behind.
    ///
    /// **Both ends are parsed, and both ends are the tables themselves.** A
    /// hand-typed list is a copy no rename moves - the first version of this
    /// test read `turn_cancelled` from a literal while serde had already moved
    /// to `turn_cancelled_probe`, and it stayed green. The enum's names are
    /// derived from its own source by the rule it declares; the client's are
    /// read out of the three tables that classify, and not out of the file at
    /// large, where a payload key or a status word would answer for a variant
    /// nothing classifies.
    ///
    /// The client's census of variants is read back as well, rather than
    /// trusted because its length is asserted: a count binds it to nothing.
    /// **`IGNORED` has no runtime reader at all** - nothing but these tests
    /// consults it - so what it holds is only as right as this control.
    ///
    /// What it does not reach: `client/src/wire/fleet.ts` keeps a table of its
    /// own for the fleet's redraws. That is a different question and is
    /// deliberately partial, so this binding stops at `apply.ts`.
    #[test]
    fn every_session_update_variant_is_classified_for_the_client() {
        let source = include_str!("protocol.rs");
        let client = include_str!("../../../client/src/session/apply.ts");
        let declared = wire_names(source);
        let classified = classified_names(client);

        // The derivation implements ONE `rename_all` rule, so it is only as good
        // as the enum declaring that rule: changing the attribute moves every
        // name at once, and nothing else here would notice.
        assert_eq!(
            rename_all(source).as_deref(),
            Some("snake_case"),
            "this control derives wire names with serde's snake_case rule and the enum no longer \
             declares it: extend the derivation before trusting anything below",
        );

        // The denominator, and it is the enum's own: a parse that stops matching
        // never reaches its closing brace, and finding nothing is what a clean
        // run looks like - so this is what tells the two apart.
        assert!(
            declared.closed,
            "the parse never reached the end of `SessionUpdate`, so it is the parse that moved \
             and not the client",
        );

        let missing: Vec<&String> =
            declared.names.iter().filter(|name| !classified.contains(name)).collect();
        assert!(
            missing.is_empty(),
            "client/src/session/apply.ts has no line for {missing:?}, and a variant the client \
             does not classify stops the session page following its seat: add each to HANDLERS \
             when the record has a field for it, and to REPLACES or IGNORED when it does not",
        );

        let stale: Vec<&String> =
            classified.iter().filter(|name| !declared.names.contains(name)).collect();
        assert!(
            stale.is_empty(),
            "client/src/session/apply.ts classifies {stale:?}, which the enum does not declare: \
             a line for a name the wire never sends is one nobody can tell from a live one",
        );

        // **And the census the client's own assertions filter over is read
        // too**, rather than trusted because its count is asserted. That count
        // binds it to nothing; this is what binds it to the enum. It lives in
        // the client's TEST file, beside the assertions that filter over it.
        let census_source = include_str!("../../../client/src/session/apply.test.ts");
        let mut census = array_entries(census_source, "EVERY_VARIANT");
        let mut declared_names = declared.names.clone();
        census.sort();
        declared_names.sort();
        assert_eq!(
            census, declared_names,
            "the client's `EVERY_VARIANT` and the enum's variants are not the same set: the \
             census is what the client's own assertions filter over, so a variant missing from \
             it is a variant nothing checks",
        );
    }

    /// The draft ending crosses as the core's own externally tagged enum, and
    /// `draftEndingLine` narrows exactly that shape - a unit variant as its
    /// name, the answered one as a name around its field. Neither side
    /// compiles the link, so this is what holds serde's output and the
    /// client's narrowing together.
    #[test]
    fn a_draft_ending_serialises_as_the_client_narrows_it() {
        use crate::SessionSlot;
        use forge_primitives::slack::SlackDraftEnding;
        use uuid::Uuid;

        let ending = |ending: SlackDraftEnding| {
            let update = super::SessionUpdate::SlackDraftResolved {
                key: SessionSlot::from_str_for_test("k"),
                id: Uuid::nil(),
                ending,
            };
            serde_json::to_value(update).expect("an update serialises")["slack_draft_resolved"]
                ["ending"]
                .clone()
        };

        assert_eq!(ending(SlackDraftEnding::Expired), serde_json::json!("expired"));
        assert_eq!(ending(SlackDraftEnding::Abandoned), serde_json::json!("abandoned"));
        assert_eq!(
            ending(SlackDraftEnding::Answered { approved: true }),
            serde_json::json!({ "answered": { "approved": true } }),
        );
    }

    /// What one parse of the enum found.
    struct Census {
        names: Vec<String>,
        /// Whether the parse reached the enum's closing brace.
        closed: bool,
    }

    /// The names the client's three classifying tables hold.
    ///
    /// The handler table's keys, and the quoted entries of the two lists. The
    /// `UNFED` export is skipped rather than read: those are the record's field
    /// names, and a field is not a variant.
    fn classified_names(client: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut table = "";
        // Whether a list's `[` has been seen and its `]` has not.
        let mut open = false;
        for line in client.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("export const HANDLERS") {
                table = "handlers";
                open = false;
                continue;
            }
            if trimmed.starts_with("export const REPLACES")
                || trimmed.starts_with("export const IGNORED")
            {
                table = "list";
                open = false;
            } else if trimmed.starts_with("export const") {
                table = "";
                open = false;
                continue;
            }
            match table {
                // A key of the handler table. The INDENT is the anchor: entries
                // sit at exactly two spaces, and an object literal inside a
                // handler's body is deeper. What follows the colon is not read,
                // so a key written against a bare function name - `turn_error:
                // settled,` - counts the same as an inline arrow.
                "handlers" => {
                    if let Some(rest) =
                        line.strip_prefix("  ").filter(|rest| !rest.starts_with(' '))
                        && let Some(key) = rest.split_once(':').map(|(key, _)| key)
                        && is_wire_name(key)
                    {
                        names.push(key.to_owned());
                    }
                }
                // `['spawning', 'connected']`, one line or many. **Only an
                // array is read, and only its entries**: the span between two
                // exports also carries prose, and a comment quoting a name must
                // not answer for a line that classifies it.
                "list" => {
                    if !open {
                        // The LAST bracket on the line: the declaration's own
                        // type carries a `[]` before the array opens.
                        let Some((_, after)) = trimmed.rsplit_once('[') else {
                            continue;
                        };
                        if let Some((entries, _)) = after.split_once(']') {
                            names.extend(quoted_names(entries).map(str::to_owned));
                        } else {
                            open = true;
                            names.extend(quoted_names(after).map(str::to_owned));
                        }
                    } else if trimmed.starts_with(']') {
                        open = false;
                    } else if trimmed.starts_with('\'') {
                        names.extend(quoted_names(trimmed).map(str::to_owned));
                    }
                }
                _ => {}
            }
        }
        names
    }

    /// The quoted entries of one `const NAME = [ ... ]` block, exported or not,
    /// which is how the client's census of variants is written.
    fn array_entries(client: &str, name: &str) -> Vec<String> {
        let heading = format!("const {name}");
        let mut entries = Vec::new();
        let mut open = false;
        for line in client.lines() {
            let trimmed = line.trim();
            if !open {
                let declared = trimmed.strip_prefix("export ").unwrap_or(trimmed);
                if !declared.starts_with(&heading) {
                    continue;
                }
                let Some((_, after)) = trimmed.rsplit_once('[') else {
                    continue;
                };
                if let Some((inside, _)) = after.split_once(']') {
                    entries.extend(quoted_names(inside).map(str::to_owned));
                } else {
                    open = true;
                    entries.extend(quoted_names(after).map(str::to_owned));
                }
            } else if trimmed.starts_with(']') {
                break;
            } else if trimmed.starts_with('\'') {
                entries.extend(quoted_names(trimmed).map(str::to_owned));
            }
        }
        entries
    }

    /// The names quoted in a run of text: the odd segments of a split on `'`.
    fn quoted_names(text: &str) -> impl Iterator<Item = &str> {
        text.split('\'').skip(1).step_by(2).filter(|entry| is_wire_name(entry))
    }

    /// Whether a token reads as a wire name, which is what keeps a bare word in
    /// a declaration from being taken for an entry.
    fn is_wire_name(token: &str) -> bool {
        !token.is_empty()
            && token.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    }

    /// The `rename_all` the enum itself declares, which is what turns a variant
    /// name into the name it crosses under.
    fn rename_all(source: &str) -> Option<String> {
        let mut nearest = None;
        for line in source.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("pub enum SessionUpdate {") {
                return nearest;
            }
            // Comments and blank lines do not break the run above the enum: a
            // doc comment between the attribute and the enum says the same
            // thing, and reading it as "no rename_all" would red a control on a
            // reformatting.
            if trimmed.is_empty() || trimmed.starts_with("//") {
                continue;
            }
            nearest = trimmed
                .strip_prefix("#[serde(rename_all = \"")
                .and_then(|rest| rest.split('"').next())
                .map(str::to_owned);
        }
        None
    }

    /// Every wire name the enum declares, in the order it declares them.
    ///
    /// The derivation serde applies to it: `rename_all` on the enum, and an
    /// explicit `#[serde(rename = "...")]` on the variant above it.
    fn wire_names(source: &str) -> Census {
        let mut names = Vec::new();
        let mut closed = false;
        let mut inside = false;
        let mut renamed: Option<String> = None;
        for line in source.lines() {
            if !inside {
                inside = line.starts_with("pub enum SessionUpdate {");
                continue;
            }
            if line == "}" {
                closed = true;
                break;
            }
            let trimmed = line.trim();
            if let Some(rest) = trimmed.strip_prefix("#[serde(rename = \"") {
                renamed = rest.split('"').next().map(str::to_owned);
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with("#[") {
                continue;
            }
            let variant: String =
                trimmed.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
            if !variant.starts_with(|c: char| c.is_ascii_uppercase()) {
                // A field inside a variant's body, or the brace closing one. A
                // rename attribute above a field belongs to the FIELD, so it is
                // dropped here rather than carried onto the next variant.
                renamed = None;
                continue;
            }
            names.push(renamed.take().unwrap_or_else(|| snake_case(&variant)));
        }
        Census { names, closed }
    }

    /// serde's own rule for this enum's `rename_all`, for a variant with no
    /// explicit rename over it.
    fn snake_case(name: &str) -> String {
        let mut out = String::new();
        for (at, c) in name.chars().enumerate() {
            if c.is_ascii_uppercase() {
                if at != 0 {
                    out.push('_');
                }
                out.push(c.to_ascii_lowercase());
            } else {
                out.push(c);
            }
        }
        out
    }
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
            session_choice: SessionChoice::Fresh,
        };
        assert_eq!(r.tag, "forge:worker:reviewer");
    }
}
