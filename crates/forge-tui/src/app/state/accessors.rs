//! Thin accessors over the active session bucket: input editors,
//! message list and viewport handles, the runtime/model/session
//! mirrors, per-session UI state (paste, mention, slash, selection),
//! account/auth/cwd snapshots, MCP + todos, and the render-cache /
//! retention / metrics counters.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Instant;

use super::types::{
    HistoryRetentionPolicy, HistoryRetentionStats, LoginHint, McpState, ModeState,
    PasteSessionState, PendingCommandAck, RecentSessionInfo, SelectionPoint, SelectionState,
    SessionUsageState, TodoItem, ToolCallScope, UsageState,
};
use super::{ChatMessage, ChatRenderTraceState, ChatViewport, InputFocus};
use crate::agent::model;
use crate::app::{input::InputSnapshot, mention, slash, subagent};

impl super::App {
    /// Read access to the active session's input editor. Each session
    /// owns its own editor so switching the active session naturally
    /// swaps the visible input.
    pub fn input(&self) -> &crate::app::input::InputState {
        // Fallback to a static default for the brief pre-Connect
        // window where no bucket has landed yet; in practice the
        // pre-Connect bucket is seeded at startup so this branch is
        // never hit in production.
        static EMPTY_INPUT: std::sync::OnceLock<crate::app::input::InputState> =
            std::sync::OnceLock::new();
        self.active_session().map_or_else(
            || EMPTY_INPUT.get_or_init(crate::app::input::InputState::new),
            |s| &s.input,
        )
    }

    /// Mutable access to the active session's input editor. Companion
    /// to [`Self::input`].
    pub fn input_mut(&mut self) -> &mut crate::app::input::InputState {
        &mut self.active_bucket_mut().input
    }

    /// Which text editor currently receives typed characters, clipboard
    /// payloads and dictation bursts. Paste routing keys on this rather
    /// than on [`Self::active_view`] so the /diff review editors get the
    /// same treatment as the chat draft.
    pub fn input_focus(&self) -> InputFocus {
        match self.active_view {
            crate::app::view::ActiveView::Chat => InputFocus::Chat,
            // Ordering mirrors `diff_overlay::handle_key`: the
            // Finish-review modal draws over the diff and captures keys
            // ahead of any comment editor underneath it.
            crate::app::view::ActiveView::Diff => {
                self.diff_overlay.as_ref().map_or(InputFocus::None, |overlay| {
                    if overlay.finish_review.is_some() {
                        InputFocus::DiffFinishReview
                    } else if overlay.active_input.is_some() {
                        InputFocus::DiffComment
                    } else {
                        InputFocus::None
                    }
                })
            }
            crate::app::view::ActiveView::Launchpad
            | crate::app::view::ActiveView::Plugins
            | crate::app::view::ActiveView::Mcp
            | crate::app::view::ActiveView::Usage => InputFocus::None,
        }
    }

    /// Whether any text editor has focus. `false` means a paste or burst
    /// flush has nowhere to land and must be dropped.
    pub fn has_focused_text_input(&self) -> bool {
        self.input_focus() != InputFocus::None
    }

    /// The focused editor, or `None` when the active view has no text
    /// input open.
    pub fn focused_input(&self) -> Option<&crate::app::input::InputState> {
        match self.input_focus() {
            InputFocus::Chat => Some(self.input()),
            InputFocus::DiffComment => {
                self.diff_overlay.as_ref()?.active_input.as_ref().map(|i| &i.editor)
            }
            InputFocus::DiffFinishReview => {
                self.diff_overlay.as_ref()?.finish_review.as_ref().map(|f| &f.editor)
            }
            InputFocus::None => None,
        }
    }

    /// Mutable companion to [`Self::focused_input`].
    pub fn focused_input_mut(&mut self) -> Option<&mut crate::app::input::InputState> {
        match self.input_focus() {
            InputFocus::Chat => Some(self.input_mut()),
            InputFocus::DiffComment => {
                self.diff_overlay.as_mut()?.active_input.as_mut().map(|i| &mut i.editor)
            }
            InputFocus::DiffFinishReview => {
                self.diff_overlay.as_mut()?.finish_review.as_mut().map(|f| &mut f.editor)
            }
            InputFocus::None => None,
        }
    }

    /// Type one printable character into the focused editor, routing it
    /// through the shared paste-burst detector so a dictation burst
    /// coalesces into a single paste payload rather than a stream of
    /// keystrokes.
    pub fn type_char(&mut self, c: char, now: Instant) -> crate::app::input::TypedChar {
        let action = self.paste_burst.on_char(c, now);
        match self.focused_input_mut() {
            Some(input) => crate::app::input::apply_char_action(input, action, c),
            None => crate::app::input::TypedChar::Buffered,
        }
    }

    /// Borrow the active session's chat buffer.
    ///
    /// Production startup and `App::test_default()` both seed a
    /// pre-Connect bucket so the active session is always populated.
    /// On the off chance the invariant is violated we log and return
    /// a static empty slice so call sites stay infallible.
    pub fn messages(&self) -> &[ChatMessage] {
        self.active_session().map_or(&[], |s| s.messages.as_slice())
    }

    /// Mutable borrow of the active session's chat buffer.
    ///
    /// Returns a mutable reference to the active bucket's `messages`
    /// vector. Auto-creates the pre-Connect bucket if the active
    /// session is missing, so call sites don't need to guard.
    pub fn active_messages_mut(&mut self) -> &mut Vec<ChatMessage> {
        &mut self.active_bucket_mut().messages
    }

    /// Borrow the parallel `message_retained_bytes` cache.
    pub fn message_retained_bytes(&self) -> &[usize] {
        self.active_session().map_or(&[], |s| s.message_retained_bytes.as_slice())
    }

    /// Mutable borrow of the `message_retained_bytes` cache.
    pub fn message_retained_bytes_mut(&mut self) -> &mut Vec<usize> {
        &mut self.active_bucket_mut().message_retained_bytes
    }

    /// Active session's rolling retained-history byte total.
    pub fn retained_history_bytes(&self) -> usize {
        self.active_session().map_or(0, |s| s.retained_history_bytes)
    }

    /// Mutable accessor for the rolling retained-history byte total.
    pub fn retained_history_bytes_mut(&mut self) -> &mut usize {
        &mut self.active_bucket_mut().retained_history_bytes
    }

    /// Borrow the active session's chat viewport.
    ///
    /// Falls back to a leaked default viewport if the active bucket
    /// is missing - the production startup path always seeds one,
    /// so the fallback is a safety net rather than a hot path.
    pub fn viewport(&self) -> &ChatViewport {
        static FALLBACK: std::sync::OnceLock<ChatViewport> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.viewport,
            None => FALLBACK.get_or_init(ChatViewport::new),
        }
    }

    /// Mutable accessor for the active session's chat viewport.
    /// Auto-creates the pre-Connect bucket if missing.
    pub fn active_viewport_mut(&mut self) -> &mut ChatViewport {
        &mut self.active_bucket_mut().viewport
    }

    // ---- Tool tracking accessors ----

    /// Borrow the active session's active task id set.
    ///
    /// Falls back to a leaked empty set when the active bucket is
    /// missing - matches the existing infallible-reader pattern
    /// (`viewport()`, `turn_state()`, ...).
    pub fn active_task_ids(&self) -> &HashSet<String> {
        static FALLBACK: std::sync::OnceLock<HashSet<String>> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.active_task_ids,
            None => FALLBACK.get_or_init(HashSet::new),
        }
    }

    /// Mutable borrow of the active task id set.
    pub fn active_task_ids_mut(&mut self) -> &mut HashSet<String> {
        &mut self.active_bucket_mut().active_task_ids
    }

    /// Borrow the active session's tool call scope map.
    pub fn tool_call_scopes(&self) -> &HashMap<String, ToolCallScope> {
        static FALLBACK: std::sync::OnceLock<HashMap<String, ToolCallScope>> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.tool_call_scopes,
            None => FALLBACK.get_or_init(HashMap::new),
        }
    }

    /// Mutable borrow of the tool call scope map.
    pub fn tool_call_scopes_mut(&mut self) -> &mut HashMap<String, ToolCallScope> {
        &mut self.active_bucket_mut().tool_call_scopes
    }

    /// Borrow the active session's tool call index.
    pub fn tool_call_index(&self) -> &HashMap<String, (usize, usize)> {
        static FALLBACK: std::sync::OnceLock<HashMap<String, (usize, usize)>> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.tool_call_index,
            None => FALLBACK.get_or_init(HashMap::new),
        }
    }

    /// Mutable borrow of the tool call index.
    pub fn active_tool_call_index_mut(&mut self) -> &mut HashMap<String, (usize, usize)> {
        &mut self.active_bucket_mut().tool_call_index
    }

    /// Borrow the active session's subagent attribution map.
    pub fn subagent_attribution(&self) -> &HashMap<String, String> {
        static FALLBACK: std::sync::OnceLock<HashMap<String, String>> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.subagent_attribution,
            None => FALLBACK.get_or_init(HashMap::new),
        }
    }

    /// Mutable borrow of the subagent attribution map.
    pub fn subagent_attribution_mut(&mut self) -> &mut HashMap<String, String> {
        &mut self.active_bucket_mut().subagent_attribution
    }

    // ---- Runtime + model accessors ----

    /// Borrow the active session's current model resolution.
    pub fn current_model(&self) -> Option<&model::CurrentModel> {
        self.active_session().and_then(|s| s.current_model.as_ref())
    }

    /// Set the active session's current model resolution.
    pub fn set_current_model(&mut self, value: Option<model::CurrentModel>) {
        self.active_bucket_mut().current_model = value;
    }

    /// Borrow the active session's available-models list.
    pub fn available_models(&self) -> &[model::AvailableModel] {
        self.active_session().map_or(&[], |s| s.available_models.as_slice())
    }

    /// Mutable borrow of the available-models list.
    pub fn available_models_mut(&mut self) -> &mut Vec<model::AvailableModel> {
        &mut self.active_bucket_mut().available_models
    }

    /// Borrow the active session's available-commands list.
    pub fn available_commands(&self) -> &[model::AvailableCommand] {
        self.active_session().map_or(&[], |s| s.available_commands.as_slice())
    }

    /// Mutable borrow of the available-commands list.
    pub fn available_commands_mut(&mut self) -> &mut Vec<model::AvailableCommand> {
        &mut self.active_bucket_mut().available_commands
    }

    /// Borrow the active session's available-agents list.
    pub fn available_agents(&self) -> &[model::AvailableAgent] {
        self.active_session().map_or(&[], |s| s.available_agents.as_slice())
    }

    /// Mutable borrow of the available-agents list.
    pub fn available_agents_mut(&mut self) -> &mut Vec<model::AvailableAgent> {
        &mut self.active_bucket_mut().available_agents
    }

    /// Borrow the active session's mode snapshot.
    pub fn mode(&self) -> Option<&ModeState> {
        self.active_session().and_then(|s| s.mode.as_ref())
    }

    /// Set the active session's mode snapshot.
    pub fn set_mode(&mut self, value: Option<ModeState>) {
        self.active_bucket_mut().mode = value;
    }

    /// Park the optimistic `/mode` pre-apply snapshot on the active
    /// session, for the `SetModeFailed` rollback.
    pub fn set_pending_mode_rollback(&mut self, value: Option<crate::app::session::ModeRollback>) {
        self.active_bucket_mut().pending_mode_rollback = value;
    }

    /// The active session's parked optimistic-`/mode` snapshot, if a
    /// switch is awaiting the CLI's verdict.
    pub fn pending_mode_rollback(&self) -> Option<&crate::app::session::ModeRollback> {
        self.active_session().and_then(|s| s.pending_mode_rollback.as_ref())
    }

    /// Restore the active session's parked optimistic-`/mode`
    /// snapshot. Returns false when no snapshot is parked.
    pub fn rollback_pending_mode(&mut self) -> bool {
        self.active_bucket_mut().rollback_pending_mode()
    }

    /// Park the optimistic `/model` pre-apply snapshot on the active
    /// session, for the `SetModelFailed` rollback.
    pub fn set_pending_model_rollback(
        &mut self,
        value: Option<crate::app::session::ModelRollback>,
    ) {
        self.active_bucket_mut().pending_model_rollback = value;
    }

    /// The active session's parked optimistic-`/model` snapshot, if a
    /// switch is awaiting the CLI's verdict.
    pub fn pending_model_rollback(&self) -> Option<&crate::app::session::ModelRollback> {
        self.active_session().and_then(|s| s.pending_model_rollback.as_ref())
    }

    /// Restore the active session's parked optimistic-`/model`
    /// snapshot. Returns false when no snapshot is parked.
    pub fn rollback_pending_model(&mut self) -> bool {
        self.active_bucket_mut().rollback_pending_model()
    }

    /// Mutable borrow of the active session's mode snapshot.
    pub fn mode_mut(&mut self) -> Option<&mut ModeState> {
        self.active_bucket_mut().mode.as_mut()
    }

    /// Active session's hook-observed permission mode.
    pub fn observed_permission_mode(&self) -> Option<forge_workspace::PermissionMode> {
        self.active_session().and_then(|s| s.observed_permission_mode)
    }

    /// Set the active session's hook-observed permission mode.
    pub fn set_observed_permission_mode(&mut self, value: Option<forge_workspace::PermissionMode>) {
        self.active_bucket_mut().observed_permission_mode = value;
    }

    /// Active session's hook-observed effort level.
    pub fn observed_effort(&self) -> Option<model::EffortLevel> {
        self.active_session().and_then(|s| s.observed_effort)
    }

    /// Set the active session's hook-observed effort level.
    pub fn set_observed_effort(&mut self, value: Option<model::EffortLevel>) {
        self.active_bucket_mut().observed_effort = value;
    }

    /// Borrow the active session's observed assistant model id.
    pub fn observed_assistant_model(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.observed_assistant_model.as_deref())
    }

    /// Set the active session's observed assistant model id.
    pub fn set_observed_assistant_model(&mut self, value: Option<String>) {
        self.active_bucket_mut().observed_assistant_model = value;
    }

    /// Active session's runtime session state.
    pub fn runtime_session_state(&self) -> Option<model::RuntimeSessionState> {
        self.active_session().and_then(|s| s.runtime_session_state)
    }

    /// Set the active session's runtime session state.
    pub fn set_runtime_session_state(&mut self, value: Option<model::RuntimeSessionState>) {
        self.active_bucket_mut().runtime_session_state = value;
    }

    /// Borrow the active session's config-options map.
    pub fn config_options(&self) -> &BTreeMap<String, serde_json::Value> {
        static FALLBACK: std::sync::OnceLock<BTreeMap<String, serde_json::Value>> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.config_options,
            None => FALLBACK.get_or_init(BTreeMap::new),
        }
    }

    /// Mutable borrow of the config-options map.
    pub fn config_options_mut(&mut self) -> &mut BTreeMap<String, serde_json::Value> {
        &mut self.active_bucket_mut().config_options
    }

    /// Borrow the active session's session-usage telemetry.
    pub fn session_usage(&self) -> &SessionUsageState {
        static FALLBACK: std::sync::OnceLock<SessionUsageState> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.session_usage,
            None => FALLBACK.get_or_init(SessionUsageState::default),
        }
    }

    /// Mutable borrow of the session-usage telemetry.
    pub fn session_usage_mut(&mut self) -> &mut SessionUsageState {
        &mut self.active_bucket_mut().session_usage
    }

    /// Borrow the active session's Anthropic-plan usage state. The
    /// pane footer's `5h` / `7d` bars read this. Returns a static
    /// empty state during the brief pre-Connect window where no
    /// session bucket exists yet.
    pub fn usage(&self) -> &UsageState {
        static FALLBACK: std::sync::OnceLock<UsageState> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.usage,
            None => FALLBACK.get_or_init(UsageState::default),
        }
    }

    /// Mutable borrow of the active session's usage state. Used by
    /// `app::usage::request_refresh` to flip the in-flight flag
    /// before spawning the fetch task.
    pub fn usage_mut(&mut self) -> &mut UsageState {
        &mut self.active_bucket_mut().usage
    }

    /// Active session's catalog of resumable sessions. The
    /// `/resume <id>` autocomplete and startup picker read from
    /// this list. Returns an empty slice in the brief pre-Connect
    /// window where no bucket exists.
    pub fn recent_sessions(&self) -> &[RecentSessionInfo] {
        self.active_session().map_or(&[], |s| s.recent_sessions.as_slice())
    }

    /// Mutable borrow of the active session's recent-sessions list.
    /// Used by tests + the SDK-side bridge polling path.
    pub fn recent_sessions_mut(&mut self) -> &mut Vec<RecentSessionInfo> {
        &mut self.active_bucket_mut().recent_sessions
    }

    /// Mutable borrow of a specific bucket's recent-sessions list.
    /// Used by `handle_sessions_listed_event` to route the wire
    /// payload onto the bucket that requested the scan.
    pub fn recent_sessions_mut_for(
        &mut self,
        key: &forge_workspace::SessionKey,
    ) -> Option<&mut Vec<RecentSessionInfo>> {
        self.sessions.get_mut(key).map(|s| &mut s.recent_sessions)
    }

    // ---- Per-session UI/input accessors (latent smells migrated) ----
    //
    // Each pair below mirrors a `UiSession` field with a read accessor
    // returning a reference and a mut accessor returning `&mut <T>`.
    // The mut accessor auto-creates the active bucket via
    // `active_bucket_mut`, so call sites can always write.

    pub fn login_hint(&self) -> Option<&LoginHint> {
        self.active_session().and_then(|s| s.login_hint.as_ref())
    }
    pub fn login_hint_mut(&mut self) -> &mut Option<LoginHint> {
        &mut self.active_bucket_mut().login_hint
    }

    pub fn resuming_session_id(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.resuming_session_id.as_deref())
    }
    pub fn resuming_session_id_mut(&mut self) -> &mut Option<String> {
        &mut self.active_bucket_mut().resuming_session_id
    }

    pub fn pending_command_label(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.pending_command_label.as_deref())
    }
    pub fn pending_command_label_mut(&mut self) -> &mut Option<String> {
        &mut self.active_bucket_mut().pending_command_label
    }

    pub fn pending_command_ack(&self) -> Option<&PendingCommandAck> {
        self.active_session().and_then(|s| s.pending_command_ack.as_ref())
    }
    pub fn pending_command_ack_mut(&mut self) -> &mut Option<PendingCommandAck> {
        &mut self.active_bucket_mut().pending_command_ack
    }

    pub fn selection(&self) -> Option<&SelectionState> {
        self.active_session().and_then(|s| s.selection.as_ref())
    }
    pub fn selection_mut(&mut self) -> &mut Option<SelectionState> {
        &mut self.active_bucket_mut().selection
    }

    pub fn pending_submit(&self) -> Option<&InputSnapshot> {
        self.active_session().and_then(|s| s.pending_submit.as_ref())
    }
    pub fn pending_submit_mut(&mut self) -> &mut Option<InputSnapshot> {
        &mut self.active_bucket_mut().pending_submit
    }

    pub fn pending_paste_text(&self) -> &str {
        self.active_session().map_or("", |s| s.pending_paste_text.as_str())
    }
    pub fn pending_paste_text_mut(&mut self) -> &mut String {
        &mut self.active_bucket_mut().pending_paste_text
    }

    pub fn pending_paste_session(&self) -> Option<&PasteSessionState> {
        self.active_session().and_then(|s| s.pending_paste_session.as_ref())
    }
    pub fn pending_paste_session_mut(&mut self) -> &mut Option<PasteSessionState> {
        &mut self.active_bucket_mut().pending_paste_session
    }

    pub fn active_paste_session(&self) -> Option<&PasteSessionState> {
        self.active_session().and_then(|s| s.active_paste_session.as_ref())
    }
    pub fn active_paste_session_mut(&mut self) -> &mut Option<PasteSessionState> {
        &mut self.active_bucket_mut().active_paste_session
    }

    pub fn next_paste_session_id(&self) -> u64 {
        self.active_session().map_or(1, |s| s.next_paste_session_id)
    }
    pub fn allocate_paste_session_id(&mut self) -> u64 {
        let slot = &mut self.active_bucket_mut().next_paste_session_id;
        let id = *slot;
        *slot = slot.saturating_add(1);
        id
    }

    pub fn pending_images(&self) -> &[crate::app::clipboard_image::ImageAttachment] {
        self.active_session().map_or(&[], |s| s.pending_images.as_slice())
    }
    pub fn pending_images_mut(&mut self) -> &mut Vec<crate::app::clipboard_image::ImageAttachment> {
        &mut self.active_bucket_mut().pending_images
    }

    pub fn mention(&self) -> Option<&mention::MentionState> {
        self.active_session().and_then(|s| s.mention.as_ref())
    }
    pub fn mention_mut(&mut self) -> &mut Option<mention::MentionState> {
        &mut self.active_bucket_mut().mention
    }

    pub fn slash(&self) -> Option<&slash::SlashState> {
        self.active_session().and_then(|s| s.slash.as_ref())
    }
    pub fn slash_mut(&mut self) -> &mut Option<slash::SlashState> {
        &mut self.active_bucket_mut().slash
    }

    pub fn subagent(&self) -> Option<&subagent::SubagentState> {
        self.active_session().and_then(|s| s.subagent.as_ref())
    }
    pub fn subagent_mut(&mut self) -> &mut Option<subagent::SubagentState> {
        &mut self.active_bucket_mut().subagent
    }

    /// Active session's file-index state for `@`-mention autocomplete.
    /// Returns an empty default state when no active session exists
    /// (test paths, brief pre-Connect window).
    pub fn file_index(&self) -> &crate::app::file_index::FileIndexState {
        static FALLBACK: std::sync::OnceLock<crate::app::file_index::FileIndexState> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.file_index,
            None => FALLBACK.get_or_init(crate::app::file_index::FileIndexState::default),
        }
    }

    /// Mutable borrow of the active session's file index. Used by
    /// the scanner + watcher lifecycle in `app::file_index` and the
    /// `@`-mention reducer in `app::mention`.
    pub fn file_index_mut(&mut self) -> &mut crate::app::file_index::FileIndexState {
        &mut self.active_bucket_mut().file_index
    }

    // ---- Account / auth accessors ----

    /// Active session's account-info snapshot.
    pub fn account_info(&self) -> Option<forge_primitives::AccountInfo> {
        self.active_session().and_then(|s| s.account_info.clone())
    }

    /// Set the active session's account-info snapshot.
    pub fn set_account_info(&mut self, value: Option<forge_primitives::AccountInfo>) {
        self.active_bucket_mut().account_info = value;
    }

    /// Active session's forge-side account display name.
    pub fn active_account_display_name(&self) -> Option<String> {
        self.active_session().and_then(|s| s.active_account_display_name.clone())
    }

    /// Set the active session's forge-side account display name.
    pub fn set_active_account_display_name(&mut self, value: Option<String>) {
        self.active_bucket_mut().active_account_display_name = value;
    }

    /// Borrow the active session's OAuth credentials snapshot.
    pub fn oauth_credentials(
        &self,
    ) -> Option<&forge_primitives::cloud::oauth_credentials::OauthCredentials> {
        self.active_session().and_then(|s| s.oauth_credentials.as_ref())
    }

    /// Set the active session's OAuth credentials snapshot.
    pub fn set_oauth_credentials(
        &mut self,
        value: Option<forge_primitives::cloud::oauth_credentials::OauthCredentials>,
    ) {
        self.active_bucket_mut().oauth_credentials = value;
    }

    // ---- Filesystem accessors ----

    /// Borrow the active session's display-friendly cwd.
    ///
    /// Returns an empty string only in the brief pre-Connect window
    /// before any session bucket exists; production startup and
    /// `App::test_default()` both seed a bucket up front.
    pub fn cwd(&self) -> &str {
        self.active_session().map_or("", |s| s.cwd.as_str())
    }

    /// Set the active session's display-friendly cwd.
    pub fn set_cwd(&mut self, value: impl Into<String>) {
        self.active_bucket_mut().cwd = value.into();
    }

    /// Active session's raw filesystem cwd.
    pub fn cwd_raw(&self) -> String {
        self.active_session().map_or_else(String::new, |s| s.cwd_raw.clone())
    }

    /// Set the active session's raw filesystem cwd.
    pub fn set_cwd_raw(&mut self, value: impl Into<String>) {
        self.active_bucket_mut().cwd_raw = value.into();
    }

    /// Borrow the active session's MCP state snapshot.
    ///
    /// Falls back to a leaked default for the brief pre-Connect
    /// window. Production startup seeds a synthetic bucket up front,
    /// so the fallback is a safety net rather than a hot path.
    pub fn mcp(&self) -> &McpState {
        static FALLBACK: std::sync::OnceLock<McpState> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.mcp,
            None => FALLBACK.get_or_init(McpState::default),
        }
    }

    /// Mutable borrow of the active session's MCP state snapshot.
    /// Auto-creates the pre-Connect bucket if missing.
    pub fn mcp_mut(&mut self) -> &mut McpState {
        &mut self.active_bucket_mut().mcp
    }

    // ---- Todos accessors ----

    /// Borrow the active session's todo list.
    pub fn todos(&self) -> &[TodoItem] {
        self.active_session().map_or(&[], |s| s.todos.as_slice())
    }

    /// Mutable borrow of the active session's todo list.
    pub fn todos_mut(&mut self) -> &mut Vec<TodoItem> {
        &mut self.active_bucket_mut().todos
    }

    /// Borrow the active session's render-cache slot grid.
    pub(crate) fn render_cache_slots(&self) -> &[Vec<super::render_budget::RenderCacheSlotState>] {
        self.active_session().map_or(&[], |s| s.render_cache_slots.as_slice())
    }

    /// Mutable borrow of the active session's render-cache slot grid.
    /// Auto-creates the pre-Connect bucket if missing.
    pub(crate) fn render_cache_slots_mut(
        &mut self,
    ) -> &mut Vec<Vec<super::render_budget::RenderCacheSlotState>> {
        &mut self.active_bucket_mut().render_cache_slots
    }

    /// Active session's rolling render-cache total bytes.
    pub(crate) fn render_cache_total_bytes(&self) -> usize {
        self.active_session().map_or(0, |s| s.render_cache_total_bytes)
    }

    /// Mutable accessor for the rolling render-cache total bytes.
    pub(crate) fn render_cache_total_bytes_mut(&mut self) -> &mut usize {
        &mut self.active_bucket_mut().render_cache_total_bytes
    }

    /// Active session's rolling render-cache protected bytes.
    pub(crate) fn render_cache_protected_bytes(&self) -> usize {
        self.active_session().map_or(0, |s| s.render_cache_protected_bytes)
    }

    /// Mutable accessor for the rolling render-cache protected bytes.
    pub(crate) fn render_cache_protected_bytes_mut(&mut self) -> &mut usize {
        &mut self.active_bucket_mut().render_cache_protected_bytes
    }

    /// Borrow the active session's evictable render-cache key set.
    pub(crate) fn render_cache_evictable(
        &self,
    ) -> Option<&BTreeSet<super::render_budget::RenderCacheEvictionKey>> {
        self.active_session().map(|s| &s.render_cache_evictable)
    }

    /// Mutable borrow of the evictable render-cache key set.
    pub(crate) fn render_cache_evictable_mut(
        &mut self,
    ) -> &mut BTreeSet<super::render_budget::RenderCacheEvictionKey> {
        &mut self.active_bucket_mut().render_cache_evictable
    }

    /// Active session's protected streaming-tail message index, if any.
    pub(crate) fn render_cache_tail_msg_idx(&self) -> Option<usize> {
        self.active_session().and_then(|s| s.render_cache_tail_msg_idx)
    }

    /// Set the active session's protected streaming-tail message index.
    pub(crate) fn set_render_cache_tail_msg_idx(&mut self, value: Option<usize>) {
        self.active_bucket_mut().render_cache_tail_msg_idx = value;
    }

    /// Borrow the active session's history-retention policy.
    pub fn history_retention(&self) -> HistoryRetentionPolicy {
        self.active_session().map_or_else(HistoryRetentionPolicy::default, |s| s.history_retention)
    }

    /// Mutable accessor for the history-retention policy.
    pub fn history_retention_mut(&mut self) -> &mut HistoryRetentionPolicy {
        &mut self.active_bucket_mut().history_retention
    }

    /// Borrow the active session's history-retention enforcement
    /// statistics.
    pub fn history_retention_stats(&self) -> &HistoryRetentionStats {
        static FALLBACK: std::sync::OnceLock<HistoryRetentionStats> = std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.history_retention_stats,
            None => FALLBACK.get_or_init(HistoryRetentionStats::default),
        }
    }

    /// Mutable accessor for the history-retention enforcement
    /// statistics.
    pub fn history_retention_stats_mut(&mut self) -> &mut HistoryRetentionStats {
        &mut self.active_bucket_mut().history_retention_stats
    }

    /// Borrow the active session's cache-metrics accumulator.
    pub fn cache_metrics(&self) -> &super::cache_metrics::CacheMetrics {
        static FALLBACK: std::sync::OnceLock<super::cache_metrics::CacheMetrics> =
            std::sync::OnceLock::new();
        match self.active_session() {
            Some(s) => &s.cache_metrics,
            None => FALLBACK.get_or_init(super::cache_metrics::CacheMetrics::default),
        }
    }

    /// Mutable accessor for the cache-metrics accumulator.
    pub fn cache_metrics_mut(&mut self) -> &mut super::cache_metrics::CacheMetrics {
        &mut self.active_bucket_mut().cache_metrics
    }

    /// Active session's previous-frame active-turn height state.
    pub(crate) fn last_active_turn_height_state(&self) -> Option<(usize, bool, bool)> {
        self.active_session().and_then(|s| s.last_active_turn_height_state)
    }

    /// Set the active session's previous-frame active-turn height state.
    pub(crate) fn set_last_active_turn_height_state(&mut self, value: Option<(usize, bool, bool)>) {
        self.active_bucket_mut().last_active_turn_height_state = value;
    }

    /// Borrow the active session's last chat-render trace snapshot.
    pub fn last_chat_render_trace_state(&self) -> Option<ChatRenderTraceState> {
        self.active_session().and_then(|s| s.last_chat_render_trace_state)
    }

    /// Set the active session's last chat-render trace snapshot.
    pub fn set_last_chat_render_trace_state(&mut self, value: Option<ChatRenderTraceState>) {
        self.active_bucket_mut().last_chat_render_trace_state = value;
    }

    /// Queue a paste payload for drain-cycle finalization.
    ///
    /// This is fed by paste payloads captured from terminal events.
    pub fn queue_paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let chunk_chars = text.chars().count();
        let had_pending_submit = self.pending_submit().is_some();
        *self.pending_submit_mut() = None;
        if self.pending_paste_text().is_empty() {
            let cursor = self
                .focused_input()
                .map(|input| SelectionPoint { row: input.cursor_row(), col: input.cursor_col() });
            let continued_session = self.active_paste_session().copied().and_then(|session| {
                let input = self.focused_input()?;
                let current_line = input.lines().get(input.cursor_row())?;
                let idx = crate::app::input::parse_paste_placeholder_before_cursor(
                    current_line,
                    input.cursor_col(),
                )?;
                (session.placeholder_index == Some(idx)).then_some(session)
            });
            let opened = continued_session.unwrap_or_else(|| {
                let id = self.allocate_paste_session_id();
                PasteSessionState {
                    id,
                    start: cursor.unwrap_or(SelectionPoint { row: 0, col: 0 }),
                    placeholder_index: None,
                }
            });
            *self.pending_paste_session_mut() = Some(opened);
            tracing::debug!(
                target: crate::logging::targets::APP_PASTE,
                event_name = "paste_queue_opened",
                message = "paste queue session opened",
                outcome = "start",
                session_id = opened.id,
                start_row = opened.start.row,
                start_col = opened.start.col,
                placeholder_index = ?opened.placeholder_index,
                chunk_chars,
                had_pending_submit,
            );
        }
        self.pending_paste_text_mut().push_str(text);
        let pending_chars = self.pending_paste_text().chars().count();
        tracing::debug!(
            target: crate::logging::targets::APP_PASTE,
            event_name = "paste_queue_updated",
            message = "paste queue updated",
            outcome = "success",
            chunk_chars,
            pending_chars,
            had_pending_submit,
        );
    }
}
