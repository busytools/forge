//! Thin accessors over the active session bucket: input editors,
//! message list and viewport handles, the runtime/model/session
//! mirrors, per-session UI state (paste, mention, slash, selection),
//! account/auth/cwd snapshots, MCP + todos, and the render-cache /
//! retention / metrics counters.
//!
//! Every accessor is fallible: `None` means no session is focused,
//! which is the launchpad boot before the first pick and the window
//! before the first spawn lands. There is no placeholder bucket to
//! borrow instead - a session either exists and carries a project, or
//! it does not exist.

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
    pub fn input(&self) -> Option<&crate::app::input::InputState> {
        self.active_session().map(|s| &s.input)
    }

    /// Mutable access to the active session's input editor. Companion
    /// to [`Self::input`].
    pub fn input_mut(&mut self) -> Option<&mut crate::app::input::InputState> {
        self.active_bucket_mut().map(|s| &mut s.input)
    }

    /// Which text editor currently receives typed characters, clipboard
    /// payloads and dictation bursts. Paste routing keys on this rather
    /// than on [`Self::active_view`] so the /diff review editors get the
    /// same treatment as the chat draft.
    pub fn input_focus(&self) -> InputFocus {
        // A chat with no session focused has no editor behind it, so the
        // predicate has to say so: callers gate on this and then write
        // through `focused_input`, which would swallow the payload.
        if self.active_view == crate::app::view::ActiveView::Chat && self.active_session().is_some()
        {
            return InputFocus::Chat;
        }
        match self.active_view {
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
            crate::app::view::ActiveView::Chat
            | crate::app::view::ActiveView::Launchpad
            | crate::app::view::ActiveView::Extensions
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
            InputFocus::Chat => self.input(),
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
            InputFocus::Chat => self.input_mut(),
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
    pub fn messages(&self) -> Option<&[ChatMessage]> {
        self.active_session().map(|s| s.messages.as_slice())
    }

    /// Mutable borrow of the active session's chat buffer.
    pub fn active_messages_mut(&mut self) -> Option<&mut Vec<ChatMessage>> {
        self.active_bucket_mut().map(|s| &mut s.messages)
    }

    /// Borrow the parallel `message_retained_bytes` cache.
    pub fn message_retained_bytes(&self) -> Option<&[usize]> {
        self.active_session().map(|s| s.message_retained_bytes.as_slice())
    }

    /// Mutable borrow of the `message_retained_bytes` cache.
    pub fn message_retained_bytes_mut(&mut self) -> Option<&mut Vec<usize>> {
        self.active_bucket_mut().map(|s| &mut s.message_retained_bytes)
    }

    /// Active session's rolling retained-history byte total.
    pub fn retained_history_bytes(&self) -> Option<usize> {
        self.active_session().map(|s| s.retained_history_bytes)
    }

    /// Mutable accessor for the rolling retained-history byte total.
    pub fn retained_history_bytes_mut(&mut self) -> Option<&mut usize> {
        self.active_bucket_mut().map(|s| &mut s.retained_history_bytes)
    }

    /// Borrow the active session's chat viewport.
    pub fn viewport(&self) -> Option<&ChatViewport> {
        self.active_session().map(|s| &s.viewport)
    }

    /// Mutable accessor for the active session's chat viewport.
    pub fn active_viewport_mut(&mut self) -> Option<&mut ChatViewport> {
        self.active_bucket_mut().map(|s| &mut s.viewport)
    }

    // ---- Tool tracking accessors ----

    /// Borrow the active session's active task id set.
    pub fn active_task_ids(&self) -> Option<&HashSet<String>> {
        self.active_session().map(|s| &s.active_task_ids)
    }

    /// Mutable borrow of the active task id set.
    pub fn active_task_ids_mut(&mut self) -> Option<&mut HashSet<String>> {
        self.active_bucket_mut().map(|s| &mut s.active_task_ids)
    }

    /// Borrow the active session's tool call scope map.
    pub fn tool_call_scopes(&self) -> Option<&HashMap<String, ToolCallScope>> {
        self.active_session().map(|s| &s.tool_call_scopes)
    }

    /// Mutable borrow of the tool call scope map.
    pub fn tool_call_scopes_mut(&mut self) -> Option<&mut HashMap<String, ToolCallScope>> {
        self.active_bucket_mut().map(|s| &mut s.tool_call_scopes)
    }

    /// Borrow the active session's tool call index.
    pub fn tool_call_index(&self) -> Option<&HashMap<String, (usize, usize)>> {
        self.active_session().map(|s| &s.tool_call_index)
    }

    /// Mutable borrow of the tool call index.
    pub fn active_tool_call_index_mut(&mut self) -> Option<&mut HashMap<String, (usize, usize)>> {
        self.active_bucket_mut().map(|s| &mut s.tool_call_index)
    }

    /// Borrow the active session's subagent attribution map.
    pub fn subagent_attribution(&self) -> Option<&HashMap<String, String>> {
        self.active_session().map(|s| &s.subagent_attribution)
    }

    /// Mutable borrow of the subagent attribution map.
    pub fn subagent_attribution_mut(&mut self) -> Option<&mut HashMap<String, String>> {
        self.active_bucket_mut().map(|s| &mut s.subagent_attribution)
    }

    // ---- Runtime + model accessors ----

    /// Borrow the active session's current model resolution.
    pub fn current_model(&self) -> Option<&model::CurrentModel> {
        self.active_session().and_then(|s| s.current_model.as_ref())
    }

    /// Set the active session's current model resolution. No-op when no
    /// session is focused: there is nothing to record it against.
    pub fn set_current_model(&mut self, value: Option<model::CurrentModel>) {
        if let Some(session) = self.active_bucket_mut() {
            session.current_model = value;
        }
    }

    /// Borrow the active session's available-models list.
    pub fn available_models(&self) -> Option<&[model::AvailableModel]> {
        self.active_session().map(|s| s.available_models.as_slice())
    }

    /// Mutable borrow of the available-models list.
    pub fn available_models_mut(&mut self) -> Option<&mut Vec<model::AvailableModel>> {
        self.active_bucket_mut().map(|s| &mut s.available_models)
    }

    /// Borrow the active session's available-commands list.
    pub fn available_commands(&self) -> Option<&[model::AvailableCommand]> {
        self.active_session().map(|s| s.available_commands.as_slice())
    }

    /// Mutable borrow of the available-commands list.
    pub fn available_commands_mut(&mut self) -> Option<&mut Vec<model::AvailableCommand>> {
        self.active_bucket_mut().map(|s| &mut s.available_commands)
    }

    /// Borrow the active session's available-agents list.
    pub fn available_agents(&self) -> Option<&[model::AvailableAgent]> {
        self.active_session().map(|s| s.available_agents.as_slice())
    }

    /// Mutable borrow of the available-agents list.
    pub fn available_agents_mut(&mut self) -> Option<&mut Vec<model::AvailableAgent>> {
        self.active_bucket_mut().map(|s| &mut s.available_agents)
    }

    /// Borrow the active session's mode snapshot.
    pub fn mode(&self) -> Option<&ModeState> {
        self.active_session().and_then(|s| s.mode.as_ref())
    }

    /// Set the active session's mode snapshot.
    pub fn set_mode(&mut self, value: Option<ModeState>) {
        if let Some(session) = self.active_bucket_mut() {
            session.mode = value;
        }
    }

    /// Park the optimistic `/mode` pre-apply snapshot on the active
    /// session, for the `SetModeFailed` rollback.
    pub fn set_pending_mode_rollback(&mut self, value: Option<crate::app::session::ModeRollback>) {
        if let Some(session) = self.active_bucket_mut() {
            session.pending_mode_rollback = value;
        }
    }

    /// The active session's parked optimistic-`/mode` snapshot, if a
    /// switch is awaiting the CLI's verdict.
    pub fn pending_mode_rollback(&self) -> Option<&crate::app::session::ModeRollback> {
        self.active_session().and_then(|s| s.pending_mode_rollback.as_ref())
    }

    /// Restore the active session's parked optimistic-`/mode`
    /// snapshot. Returns false when no snapshot is parked, or when no
    /// session is focused.
    pub fn rollback_pending_mode(&mut self) -> bool {
        self.active_bucket_mut()
            .is_some_and(super::super::session::UiSession::rollback_pending_mode)
    }

    /// Park the optimistic `/model` pre-apply snapshot on the active
    /// session, for the `SetModelFailed` rollback.
    pub fn set_pending_model_rollback(
        &mut self,
        value: Option<crate::app::session::ModelRollback>,
    ) {
        if let Some(session) = self.active_bucket_mut() {
            session.pending_model_rollback = value;
        }
    }

    /// The active session's parked optimistic-`/model` snapshot, if a
    /// switch is awaiting the CLI's verdict.
    pub fn pending_model_rollback(&self) -> Option<&crate::app::session::ModelRollback> {
        self.active_session().and_then(|s| s.pending_model_rollback.as_ref())
    }

    /// Restore the active session's parked optimistic-`/model`
    /// snapshot. Returns false when no snapshot is parked, or when no
    /// session is focused.
    pub fn rollback_pending_model(&mut self) -> bool {
        self.active_bucket_mut()
            .is_some_and(super::super::session::UiSession::rollback_pending_model)
    }

    /// Mutable borrow of the active session's mode snapshot.
    pub fn mode_mut(&mut self) -> Option<&mut ModeState> {
        self.active_bucket_mut().and_then(|s| s.mode.as_mut())
    }

    /// Active session's hook-observed permission mode.
    pub fn observed_permission_mode(&self) -> Option<forge_workspace::PermissionMode> {
        self.active_session().and_then(|s| s.observed_permission_mode)
    }

    /// Set the active session's hook-observed permission mode.
    pub fn set_observed_permission_mode(&mut self, value: Option<forge_workspace::PermissionMode>) {
        if let Some(session) = self.active_bucket_mut() {
            session.observed_permission_mode = value;
        }
    }

    /// Active session's hook-observed effort level.
    pub fn observed_effort(&self) -> Option<model::EffortLevel> {
        self.active_session().and_then(|s| s.observed_effort)
    }

    /// Set the active session's hook-observed effort level.
    pub fn set_observed_effort(&mut self, value: Option<model::EffortLevel>) {
        if let Some(session) = self.active_bucket_mut() {
            session.observed_effort = value;
        }
    }

    /// Borrow the active session's observed assistant model id.
    pub fn observed_assistant_model(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.observed_assistant_model.as_deref())
    }

    /// Set the active session's observed assistant model id.
    pub fn set_observed_assistant_model(&mut self, value: Option<String>) {
        if let Some(session) = self.active_bucket_mut() {
            session.observed_assistant_model = value;
        }
    }

    /// Active session's runtime session state.
    pub fn runtime_session_state(&self) -> Option<model::RuntimeSessionState> {
        self.active_session().and_then(|s| s.runtime_session_state)
    }

    /// Set the active session's runtime session state.
    pub fn set_runtime_session_state(&mut self, value: Option<model::RuntimeSessionState>) {
        if let Some(session) = self.active_bucket_mut() {
            session.runtime_session_state = value;
        }
    }

    /// Borrow the active session's config-options map.
    pub fn config_options(&self) -> Option<&BTreeMap<String, serde_json::Value>> {
        self.active_session().map(|s| &s.config_options)
    }

    /// Mutable borrow of the config-options map.
    pub fn config_options_mut(&mut self) -> Option<&mut BTreeMap<String, serde_json::Value>> {
        self.active_bucket_mut().map(|s| &mut s.config_options)
    }

    /// Borrow the active session's session-usage telemetry.
    pub fn session_usage(&self) -> Option<&SessionUsageState> {
        self.active_session().map(|s| &s.session_usage)
    }

    /// Mutable borrow of the session-usage telemetry.
    pub fn session_usage_mut(&mut self) -> Option<&mut SessionUsageState> {
        self.active_bucket_mut().map(|s| &mut s.session_usage)
    }

    /// Borrow the active session's Anthropic-plan usage state. The
    /// pane footer's `5h` / `7d` bars read this.
    pub fn usage(&self) -> Option<&UsageState> {
        self.active_session().map(|s| &s.usage)
    }

    /// Mutable borrow of the active session's usage state. Used by
    /// `app::usage::request_refresh` to flip the in-flight flag
    /// before spawning the fetch task.
    pub fn usage_mut(&mut self) -> Option<&mut UsageState> {
        self.active_bucket_mut().map(|s| &mut s.usage)
    }

    /// Active session's catalog of resumable sessions. The
    /// `/resume <id>` autocomplete and startup picker read from
    /// this list.
    pub fn recent_sessions(&self) -> Option<&[RecentSessionInfo]> {
        self.active_session().map(|s| s.recent_sessions.as_slice())
    }

    /// Mutable borrow of the active session's recent-sessions list.
    /// Used by the SDK-side bridge polling path.
    pub fn recent_sessions_mut(&mut self) -> Option<&mut Vec<RecentSessionInfo>> {
        self.active_bucket_mut().map(|s| &mut s.recent_sessions)
    }

    /// Mutable borrow of a specific bucket's recent-sessions list.
    /// Used by `handle_sessions_listed_event` to route the wire
    /// payload onto the bucket that requested the scan.
    pub fn recent_sessions_mut_for(
        &mut self,
        key: &forge_workspace::SessionSlot,
    ) -> Option<&mut Vec<RecentSessionInfo>> {
        self.sessions.get_mut(key).map(|s| &mut s.recent_sessions)
    }

    // ---- Per-session UI/input accessors (latent smells migrated) ----
    //
    // Each pair below mirrors a `UiSession` field with a read accessor
    // returning a reference and a mut accessor returning `&mut <T>`.
    // The mut accessor is `None` when nothing is focused.

    pub fn login_hint(&self) -> Option<&LoginHint> {
        self.active_session().and_then(|s| s.login_hint.as_ref())
    }
    pub fn login_hint_mut(&mut self) -> Option<&mut Option<LoginHint>> {
        self.active_bucket_mut().map(|s| &mut s.login_hint)
    }

    pub fn resuming_session_id(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.resuming_session_id.as_deref())
    }
    pub fn resuming_session_id_mut(&mut self) -> Option<&mut Option<String>> {
        self.active_bucket_mut().map(|s| &mut s.resuming_session_id)
    }

    pub fn pending_command_label(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.pending_command_label.as_deref())
    }
    pub fn pending_command_label_mut(&mut self) -> Option<&mut Option<String>> {
        self.active_bucket_mut().map(|s| &mut s.pending_command_label)
    }

    pub fn pending_command_ack(&self) -> Option<&PendingCommandAck> {
        self.active_session().and_then(|s| s.pending_command_ack.as_ref())
    }
    pub fn pending_command_ack_mut(&mut self) -> Option<&mut Option<PendingCommandAck>> {
        self.active_bucket_mut().map(|s| &mut s.pending_command_ack)
    }

    pub fn selection(&self) -> Option<&SelectionState> {
        self.active_session().and_then(|s| s.selection.as_ref())
    }
    pub fn selection_mut(&mut self) -> Option<&mut Option<SelectionState>> {
        self.active_bucket_mut().map(|s| &mut s.selection)
    }

    pub fn pending_submit(&self) -> Option<&InputSnapshot> {
        self.active_session().and_then(|s| s.pending_submit.as_ref())
    }
    pub fn pending_submit_mut(&mut self) -> Option<&mut Option<InputSnapshot>> {
        self.active_bucket_mut().map(|s| &mut s.pending_submit)
    }

    pub fn pending_paste_text(&self) -> Option<&str> {
        self.active_session().map(|s| s.pending_paste_text.as_str())
    }
    pub fn pending_paste_text_mut(&mut self) -> Option<&mut String> {
        self.active_bucket_mut().map(|s| &mut s.pending_paste_text)
    }

    pub fn pending_paste_session(&self) -> Option<&PasteSessionState> {
        self.active_session().and_then(|s| s.pending_paste_session.as_ref())
    }
    pub fn pending_paste_session_mut(&mut self) -> Option<&mut Option<PasteSessionState>> {
        self.active_bucket_mut().map(|s| &mut s.pending_paste_session)
    }

    pub fn active_paste_session(&self) -> Option<&PasteSessionState> {
        self.active_session().and_then(|s| s.active_paste_session.as_ref())
    }
    pub fn active_paste_session_mut(&mut self) -> Option<&mut Option<PasteSessionState>> {
        self.active_bucket_mut().map(|s| &mut s.active_paste_session)
    }

    pub fn next_paste_session_id(&self) -> Option<u64> {
        self.active_session().map(|s| s.next_paste_session_id)
    }

    /// Allocate the next paste-session id, `None` when no session is
    /// focused.
    pub fn allocate_paste_session_id(&mut self) -> Option<u64> {
        let slot = self.active_bucket_mut().map(|s| &mut s.next_paste_session_id)?;
        let id = *slot;
        *slot = slot.saturating_add(1);
        Some(id)
    }

    pub fn pending_images(&self) -> Option<&[crate::app::clipboard_image::ImageAttachment]> {
        self.active_session().map(|s| s.pending_images.as_slice())
    }
    pub fn pending_images_mut(
        &mut self,
    ) -> Option<&mut Vec<crate::app::clipboard_image::ImageAttachment>> {
        self.active_bucket_mut().map(|s| &mut s.pending_images)
    }

    pub fn mention(&self) -> Option<&mention::MentionState> {
        self.active_session().and_then(|s| s.mention.as_ref())
    }
    pub fn mention_mut(&mut self) -> Option<&mut Option<mention::MentionState>> {
        self.active_bucket_mut().map(|s| &mut s.mention)
    }

    pub fn slash(&self) -> Option<&slash::SlashState> {
        self.active_session().and_then(|s| s.slash.as_ref())
    }
    pub fn slash_mut(&mut self) -> Option<&mut Option<slash::SlashState>> {
        self.active_bucket_mut().map(|s| &mut s.slash)
    }

    pub fn subagent(&self) -> Option<&subagent::SubagentState> {
        self.active_session().and_then(|s| s.subagent.as_ref())
    }
    pub fn subagent_mut(&mut self) -> Option<&mut Option<subagent::SubagentState>> {
        self.active_bucket_mut().map(|s| &mut s.subagent)
    }

    /// Active session's file-index state for `@`-mention autocomplete.
    pub fn file_index(&self) -> Option<&crate::app::file_index::FileIndexState> {
        self.active_session().map(|s| &s.file_index)
    }

    /// Mutable borrow of the active session's file index. Used by
    /// the scanner + watcher lifecycle in `app::file_index` and the
    /// `@`-mention reducer in `app::mention`.
    pub fn file_index_mut(&mut self) -> Option<&mut crate::app::file_index::FileIndexState> {
        self.active_bucket_mut().map(|s| &mut s.file_index)
    }

    // ---- Account / auth accessors ----

    /// Active session's account-info snapshot.
    pub fn account_info(&self) -> Option<forge_primitives::AccountInfo> {
        self.active_session().and_then(|s| s.account_info.clone())
    }

    /// Set the active session's account-info snapshot.
    pub fn set_account_info(&mut self, value: Option<forge_primitives::AccountInfo>) {
        if let Some(session) = self.active_bucket_mut() {
            session.account_info = value;
        }
    }

    /// Active session's forge-side account display name.
    pub fn active_account_display_name(&self) -> Option<String> {
        self.active_session().and_then(|s| s.active_account_display_name.clone())
    }

    /// Active session's forge org, from its slot. This is the org in
    /// `forge.toml` that owns the session's accounts: the gateway
    /// selects only from the org the child's request path names.
    pub fn session_org(&self) -> Option<&str> {
        self.active_session_key.as_ref().map(forge_workspace::SessionSlot::org)
    }

    /// Set the active session's forge-side account display name.
    pub fn set_active_account_display_name(&mut self, value: Option<String>) {
        if let Some(session) = self.active_bucket_mut() {
            session.active_account_display_name = value;
        }
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
        if let Some(session) = self.active_bucket_mut() {
            session.oauth_credentials = value;
        }
    }

    // ---- Filesystem accessors ----

    /// Borrow the active session's display-friendly cwd.
    pub fn cwd(&self) -> Option<&str> {
        self.active_session().map(|s| s.cwd.as_str())
    }

    /// Set the active session's display-friendly cwd.
    pub fn set_cwd(&mut self, value: impl Into<String>) {
        if let Some(session) = self.active_bucket_mut() {
            session.cwd = value.into();
        }
    }

    /// Active session's raw filesystem cwd.
    pub fn cwd_raw(&self) -> Option<String> {
        self.active_session().map(|s| s.cwd_raw.clone())
    }

    /// Set the active session's raw filesystem cwd.
    pub fn set_cwd_raw(&mut self, value: impl Into<String>) {
        if let Some(session) = self.active_bucket_mut() {
            session.cwd_raw = value.into();
        }
    }

    /// Borrow the active session's MCP state snapshot.
    pub fn mcp(&self) -> Option<&McpState> {
        self.active_session().map(|s| &s.mcp)
    }

    /// Mutable borrow of the active session's MCP state snapshot.
    pub fn mcp_mut(&mut self) -> Option<&mut McpState> {
        self.active_bucket_mut().map(|s| &mut s.mcp)
    }

    // ---- Todos accessors ----

    /// Borrow the active session's todo list.
    pub fn todos(&self) -> Option<&[TodoItem]> {
        self.active_session().map(|s| s.todos.as_slice())
    }

    /// Mutable borrow of the active session's todo list.
    pub fn todos_mut(&mut self) -> Option<&mut Vec<TodoItem>> {
        self.active_bucket_mut().map(|s| &mut s.todos)
    }

    /// Borrow the active session's render-cache slot grid.
    pub(crate) fn render_cache_slots(
        &self,
    ) -> Option<&[Vec<super::render_budget::RenderCacheSlotState>]> {
        self.active_session().map(|s| s.render_cache_slots.as_slice())
    }

    /// Mutable borrow of the active session's render-cache slot grid.
    pub(crate) fn render_cache_slots_mut(
        &mut self,
    ) -> Option<&mut Vec<Vec<super::render_budget::RenderCacheSlotState>>> {
        self.active_bucket_mut().map(|s| &mut s.render_cache_slots)
    }

    /// Active session's rolling render-cache total bytes.
    pub(crate) fn render_cache_total_bytes(&self) -> Option<usize> {
        self.active_session().map(|s| s.render_cache_total_bytes)
    }

    /// Mutable accessor for the rolling render-cache total bytes.
    pub(crate) fn render_cache_total_bytes_mut(&mut self) -> Option<&mut usize> {
        self.active_bucket_mut().map(|s| &mut s.render_cache_total_bytes)
    }

    /// Active session's rolling render-cache protected bytes.
    pub(crate) fn render_cache_protected_bytes(&self) -> Option<usize> {
        self.active_session().map(|s| s.render_cache_protected_bytes)
    }

    /// Mutable accessor for the rolling render-cache protected bytes.
    pub(crate) fn render_cache_protected_bytes_mut(&mut self) -> Option<&mut usize> {
        self.active_bucket_mut().map(|s| &mut s.render_cache_protected_bytes)
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
    ) -> Option<&mut BTreeSet<super::render_budget::RenderCacheEvictionKey>> {
        self.active_bucket_mut().map(|s| &mut s.render_cache_evictable)
    }

    /// Active session's protected streaming-tail message index, if any.
    pub(crate) fn render_cache_tail_msg_idx(&self) -> Option<usize> {
        self.active_session().and_then(|s| s.render_cache_tail_msg_idx)
    }

    /// Set the active session's protected streaming-tail message index.
    pub(crate) fn set_render_cache_tail_msg_idx(&mut self, value: Option<usize>) {
        if let Some(session) = self.active_bucket_mut() {
            session.render_cache_tail_msg_idx = value;
        }
    }

    /// Borrow the active session's history-retention policy.
    pub fn history_retention(&self) -> Option<HistoryRetentionPolicy> {
        self.active_session().map(|s| s.history_retention)
    }

    /// Mutable accessor for the history-retention policy.
    pub fn history_retention_mut(&mut self) -> Option<&mut HistoryRetentionPolicy> {
        self.active_bucket_mut().map(|s| &mut s.history_retention)
    }

    /// Borrow the active session's history-retention enforcement
    /// statistics.
    pub fn history_retention_stats(&self) -> Option<&HistoryRetentionStats> {
        self.active_session().map(|s| &s.history_retention_stats)
    }

    /// Mutable accessor for the history-retention enforcement
    /// statistics.
    pub fn history_retention_stats_mut(&mut self) -> Option<&mut HistoryRetentionStats> {
        self.active_bucket_mut().map(|s| &mut s.history_retention_stats)
    }

    /// Borrow the active session's cache-metrics accumulator.
    pub fn cache_metrics(&self) -> Option<&super::cache_metrics::CacheMetrics> {
        self.active_session().map(|s| &s.cache_metrics)
    }

    /// Mutable accessor for the cache-metrics accumulator.
    pub fn cache_metrics_mut(&mut self) -> Option<&mut super::cache_metrics::CacheMetrics> {
        self.active_bucket_mut().map(|s| &mut s.cache_metrics)
    }

    /// Active session's previous-frame active-turn height state.
    pub(crate) fn last_active_turn_height_state(&self) -> Option<(usize, bool, bool)> {
        self.active_session().and_then(|s| s.last_active_turn_height_state)
    }

    /// Set the active session's previous-frame active-turn height state.
    pub(crate) fn set_last_active_turn_height_state(&mut self, value: Option<(usize, bool, bool)>) {
        if let Some(session) = self.active_bucket_mut() {
            session.last_active_turn_height_state = value;
        }
    }

    /// Borrow the active session's last chat-render trace snapshot.
    pub fn last_chat_render_trace_state(&self) -> Option<ChatRenderTraceState> {
        self.active_session().and_then(|s| s.last_chat_render_trace_state)
    }

    /// Set the active session's last chat-render trace snapshot.
    pub fn set_last_chat_render_trace_state(&mut self, value: Option<ChatRenderTraceState>) {
        if let Some(session) = self.active_bucket_mut() {
            session.last_chat_render_trace_state = value;
        }
    }

    /// Queue a paste payload for drain-cycle finalization.
    ///
    /// This is fed by paste payloads captured from terminal events.
    /// Dropped when no session is focused: there is no draft to land in.
    pub fn queue_paste_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let chunk_chars = text.chars().count();
        let had_pending_submit = self.pending_submit().is_some();
        if let Some(pending) = self.pending_submit_mut() {
            *pending = None;
        }
        if !matches!(self.pending_paste_text(), Some(pending) if !pending.is_empty()) {
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
            let Some(opened) = continued_session.or_else(|| {
                let id = self.allocate_paste_session_id()?;
                Some(PasteSessionState {
                    id,
                    start: cursor.unwrap_or(SelectionPoint { row: 0, col: 0 }),
                    placeholder_index: None,
                })
            }) else {
                return;
            };
            if let Some(slot) = self.pending_paste_session_mut() {
                *slot = Some(opened);
            }
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
        let Some(pending) = self.pending_paste_text_mut() else {
            return;
        };
        pending.push_str(text);
        let pending_chars = pending.chars().count();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The chat view with no session behind it has no editor, and the
    /// predicate has to agree with the accessor. Every paste and
    /// dictation caller gates on `has_focused_text_input()` and then
    /// writes through `focused_input()`, so a `true` with no editor
    /// swallows the payload.
    #[test]
    fn a_chat_view_with_no_session_has_no_focused_input() {
        let mut app = crate::app::App::test_default();
        app.sessions.clear();
        app.active_session_key = None;
        app.active_view = crate::app::view::ActiveView::Chat;

        assert_eq!(app.input_focus(), InputFocus::None);
        assert!(!app.has_focused_text_input(), "the predicate must not promise an editor");
        assert!(app.focused_input().is_none(), "and the accessor agrees with it");
    }
}
