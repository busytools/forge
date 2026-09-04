//! Tool-call tracking on `App`: the active-task set, tool-call scopes
//! and the message/block index, the turn-boundary finalize sweep, the
//! per-group collapse levels, the backgrounded-root registry, and the
//! SUBAGENTS Inspector view derived from all of it.

use std::collections::HashSet;

use super::types::ToolCallScope;
use super::{AnsweredQuestion, MessageBlock};
use crate::agent::model;

impl super::App {
    /// Track a Task/Agent tool call as active (in-progress subagent).
    pub fn insert_active_task(&mut self, id: String) {
        self.active_task_ids_mut().insert(id);
    }

    /// Remove a Task/Agent tool call from the active set (completed/failed).
    pub fn remove_active_task(&mut self, id: &str) {
        self.active_task_ids_mut().remove(id);
    }

    pub fn register_tool_call_scope(&mut self, id: String, scope: ToolCallScope) {
        self.tool_call_scopes_mut().insert(id, scope);
    }

    pub fn tool_call_scope(&self, id: &str) -> Option<ToolCallScope> {
        self.tool_call_scopes().get(id).cloned()
    }

    /// Whether a tool call's card is still non-terminal. Independent
    /// evidence that a backgrounded task is alive, for the window where
    /// the roster has not caught up.
    fn tool_call_is_open(&self, id: &str) -> bool {
        self.lookup_tool_call(id)
            .and_then(|(mi, bi)| self.messages().get(mi)?.blocks.get(bi))
            .is_some_and(|block| match block {
                MessageBlock::ToolCall(tc) => matches!(
                    tc.status,
                    model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                ),
                _ => false,
            })
    }

    /// The positive form: the card exists AND has reached a terminal
    /// status. An id with no card in the message list has no evidence
    /// either way and reads as not settled (#791).
    fn tool_call_is_settled(&self, id: &str) -> bool {
        self.lookup_tool_call(id)
            .and_then(|(mi, bi)| self.messages().get(mi)?.blocks.get(bi))
            .is_some_and(|block| match block {
                MessageBlock::ToolCall(tc) => !matches!(
                    tc.status,
                    model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                ),
                _ => false,
            })
    }

    pub fn clear_tool_scope_tracking(&mut self) {
        // Preserve scope tracking for still-running backgrounded roots and
        // their children so a backgrounded subagent stays identifiable in
        // SUBAGENTS across turn boundaries; a blanket clear made it vanish
        // until its next child re-registered the scope.
        // `background_tasks_changed` can land a frame after the `Result`,
        // so this read can see an empty roster for a subagent that is
        // running, and nothing re-registers a dropped scope. Closing that
        // needs a durable was-backgrounded signal (#790).
        let alive = self
            .active_session()
            .map(crate::app::session::UiSession::backgrounded_alive_with_children)
            .unwrap_or_default();
        let open_roots: HashSet<String> = self
            .tool_call_scopes()
            .iter()
            .filter(|(_, scope)| {
                matches!(scope, crate::app::state::types::ToolCallScope::SubagentRoot)
            })
            .map(|(id, _)| id.clone())
            .filter(|id| self.active_task_ids().contains(id) || self.tool_call_is_open(id))
            .collect();
        let dropped_while_open: Vec<String> =
            open_roots.iter().filter(|id| !alive.contains(id.as_str())).map(Clone::clone).collect();
        for id in &dropped_while_open {
            tracing::warn!(
                target: crate::logging::targets::APP_TOOL,
                event_name = "subagent_root_dropped_while_open",
                message = "dropping a subagent root's scope while its card is still open; it will not be re-registered and SUBAGENTS loses it",
                outcome = "dropped",
                tool_call_id = %id,
            );
        }
        // A child whose own card is terminal cannot be swept into anything,
        // so holding its scope only grows the map with the subagent's total
        // tool-call count (#791). A live grandchild behind such a child is
        // still spared: a terminal-yet-running nested Task carries its own
        // roster row and is a live root in `alive` itself.
        let settled_children: HashSet<String> = self
            .tool_call_scopes()
            .iter()
            .filter(|(id, scope)| {
                matches!(scope, crate::app::state::types::ToolCallScope::SubagentChild { .. })
                    && self.tool_call_is_settled(id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.tool_call_scopes_mut().retain(|id, scope| match scope {
            crate::app::state::types::ToolCallScope::SubagentRoot => alive.contains(id.as_str()),
            crate::app::state::types::ToolCallScope::SubagentChild { parent_tool_use_id } => {
                alive.contains(parent_tool_use_id.as_str()) && !settled_children.contains(id)
            }
            crate::app::state::types::ToolCallScope::MainAgent => false,
        });
        self.active_task_ids_mut().clear();
    }

    /// Look up the (`message_index`, `block_index`) for a tool call ID.
    pub fn lookup_tool_call(&self, id: &str) -> Option<(usize, usize)> {
        self.tool_call_index().get(id).copied()
    }

    /// Stamp a resolved answer onto an AskUserQuestion tool call: append
    /// the question -> answer pair, un-hide it (it was chat-suppressed
    /// while the dock prompt was live), and invalidate its render so the
    /// answered-card paints. No-op when the tool call isn't found (e.g.
    /// the session switched between prompt and answer).
    pub(crate) fn record_answered_question(&mut self, tool_id: &str, answered: AnsweredQuestion) {
        let Some((mi, bi)) = self.lookup_tool_call(tool_id) else {
            return;
        };
        if let Some(MessageBlock::ToolCall(tc)) =
            self.active_messages_mut().get_mut(mi).and_then(|m| m.blocks.get_mut(bi))
        {
            let tc = tc.as_mut();
            tc.answered_questions.push(answered);
            tc.hidden = false;
            tc.mark_tool_call_render_dirty();
            tc.mark_tool_call_layout_dirty();
        }
    }

    /// Register a tool call's position in the message/block arrays.
    pub fn index_tool_call(&mut self, id: String, msg_idx: usize, block_idx: usize) {
        self.active_tool_call_index_mut().insert(id, (msg_idx, block_idx));
    }

    /// Force-finish any lingering in-progress tool calls.
    /// Returns the number of tool calls that were transitioned.
    ///
    /// A live backgrounded subagent is exempt, root and children alike:
    /// it outlives the turn and settles via its own `task_updated`.
    pub fn finalize_in_progress_tool_calls(&mut self, new_status: model::ToolCallStatus) -> usize {
        let mut changed = 0usize;
        let mut changed_message_indices = Vec::new();
        let mut changed_slots = Vec::new();
        // Open calls first, so liveness is answered per call - O(depth)
        // each - instead of deriving the eager exempt set off the whole
        // scope map (#793).
        let open_ids: Vec<String> = self
            .messages()
            .iter()
            .flat_map(|msg| &msg.blocks)
            .filter_map(|block| match block {
                MessageBlock::ToolCall(tc)
                    if matches!(
                        tc.status,
                        model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                    ) =>
                {
                    Some(tc.id.clone())
                }
                _ => None,
            })
            .collect();
        let exempt: std::collections::HashSet<&str> = open_ids
            .iter()
            .filter(|id| {
                self.active_session()
                    .is_some_and(|session| session.is_backgrounded_alive_or_descendant(id))
            })
            .map(String::as_str)
            .collect();

        for (msg_idx, msg) in self.active_messages_mut().iter_mut().enumerate() {
            for (block_idx, block) in msg.blocks.iter_mut().enumerate() {
                if let MessageBlock::ToolCall(tc) = block {
                    let tc = tc.as_mut();
                    if matches!(
                        tc.status,
                        model::ToolCallStatus::InProgress | model::ToolCallStatus::Pending
                    ) && !exempt.contains(tc.id.as_str())
                    {
                        tc.status = new_status;
                        tc.mark_tool_call_layout_dirty();
                        changed_slots.push((msg_idx, block_idx));
                        // A completed execute's captured terminal id no
                        // longer means anything to the renderer.
                        if tc.is_execute_tool() {
                            tc.terminal_id = None;
                        }
                        if changed_message_indices.last().copied() != Some(msg_idx) {
                            changed_message_indices.push(msg_idx);
                        }
                        changed += 1;
                    }
                }
            }
        }

        for (msg_idx, block_idx) in changed_slots {
            self.sync_render_cache_slot(msg_idx, block_idx);
        }

        for msg_idx in changed_message_indices.iter().copied() {
            self.recompute_message_retained_bytes(msg_idx);
        }

        if changed > 0 {
            self.invalidate_message_set(changed_message_indices.iter().copied());
        }

        tracing::debug!(
            target: crate::logging::targets::APP_TOOL,
            event_name = "tool_call_sweep",
            message = "swept open tool calls at a turn boundary",
            outcome = "success",
            sweep_site = "submit_or_turn_exit",
            new_status = ?new_status,
            count = changed,
            exempt_count = exempt.len(),
        );
        changed
    }

    /// Clear runtime-only turn tracking while preserving the message history itself.
    pub fn finalize_turn_runtime_artifacts(&mut self, new_status: model::ToolCallStatus) {
        let _ = self.finalize_in_progress_tool_calls(new_status);
        self.clear_tool_scope_tracking();
    }

    /// Active session's group collapse level for `id`. Per-group
    /// override wins; absent falls through to the global directive
    /// via `resolve_group_level` (L2Summary when collapsed, L0Bodies
    /// when expanded). Used by mouse handlers, replay tests, and
    /// non-render consumers; the chat render path consults the same
    /// resolver via `MessageRenderContext::group_level`.
    pub fn group_collapse_level(
        &self,
        id: &crate::ui::message::grouping::GroupId,
    ) -> crate::ui::message::grouping::GroupCollapseLevel {
        let per_group =
            self.active_session().and_then(|s| s.group_collapse_levels.get(id).copied());
        crate::ui::collapse::resolve_group_level(per_group, self.tools_collapsed)
    }

    /// Advance the group's collapse level one step (L2 -> L1 -> L0 -> L2).
    /// Returns the new level. Auto-creates the active bucket if missing.
    pub fn cycle_group_collapse_level(
        &mut self,
        id: &crate::ui::message::grouping::GroupId,
    ) -> crate::ui::message::grouping::GroupCollapseLevel {
        let current = self.group_collapse_level(id);
        let next = current.next();
        self.active_bucket_mut().group_collapse_levels.insert(id.clone(), next);
        next
    }

    /// Active session's messaging-group collapse level for `id`.
    /// Per-group override wins; absent falls through to the global
    /// directive via `resolve_group_level` (the same resolver that
    /// drives tool-call groups). Sibling of `group_collapse_level`
    /// keyed on `messaging_group_collapse_levels` so tool-group and
    /// messaging-group leader ids never collide.
    pub fn messaging_group_collapse_level(
        &self,
        id: &crate::ui::message::grouping::GroupId,
    ) -> crate::ui::message::grouping::GroupCollapseLevel {
        let per_group =
            self.active_session().and_then(|s| s.messaging_group_collapse_levels.get(id).copied());
        crate::ui::collapse::resolve_group_level(per_group, self.tools_collapsed)
    }

    /// Advance the messaging-group's collapse level one step
    /// (L2 -> L1 -> L0 -> L2). Returns the new level. Auto-creates
    /// the active bucket if missing.
    pub fn cycle_messaging_group_collapse_level(
        &mut self,
        id: &crate::ui::message::grouping::GroupId,
    ) -> crate::ui::message::grouping::GroupCollapseLevel {
        let current = self.messaging_group_collapse_level(id);
        let next = current.next();
        self.active_bucket_mut().messaging_group_collapse_levels.insert(id.clone(), next);
        next
    }

    /// Mutable accessor for the active session's background-task
    /// snapshot. Auto-creates the pre-Connect bucket if missing.
    pub(crate) fn background_tasks_mut(
        &mut self,
    ) -> &mut Vec<crate::app::state::types::BackgroundTask> {
        &mut self.active_bucket_mut().background_tasks
    }

    /// Record a session-scoped `task_id` -> `tool_use_id` at
    /// `task_started`, so a task that outlives its turn stays resolvable
    /// after the turn-scoped map is wiped (see
    /// `UiSession::session_task_tool_use_ids`).
    pub(crate) fn insert_session_task_mapping(&mut self, task_id: String, tool_use_id: String) {
        self.active_bucket_mut().session_task_tool_use_ids.insert(task_id, tool_use_id);
    }

    /// Drop a session-scoped task mapping when the task reaches a
    /// terminal state. No-op when absent.
    pub(crate) fn remove_session_task_mapping(&mut self, task_id: &str) {
        self.active_bucket_mut().session_task_tool_use_ids.remove(task_id);
    }

    /// Settle the open descendants of a backgrounded root that just left
    /// the roster on the active session. See
    /// [`UiSession::settle_children_of`].
    pub(crate) fn settle_departed_root_children(&mut self, root_id: &str) {
        let settled = self.active_bucket_mut().settle_children_of(root_id);
        if settled.is_empty() {
            return;
        }
        let mut changed_messages: Vec<usize> = Vec::new();
        for (msg_idx, block_idx) in &settled {
            self.sync_render_cache_slot(*msg_idx, *block_idx);
            if changed_messages.last() != Some(msg_idx) {
                changed_messages.push(*msg_idx);
            }
        }
        for msg_idx in &changed_messages {
            self.recompute_message_retained_bytes(*msg_idx);
        }
        self.invalidate_message_set(changed_messages);
    }

    /// Clear the active session's background-task registry (and its
    /// task-id mirror) on teardown. See
    /// [`UiSession::clear_background_task_registry`].
    pub(crate) fn clear_active_session_background_task_registry(&mut self) {
        self.active_bucket_mut().clear_background_task_registry();
    }

    /// Mark a tool-use id as a backgrounded agent root on the active
    /// session. See [`UiSession::backgrounded_roots`].
    pub(crate) fn mark_backgrounded_root(&mut self, tool_use_id: String) {
        self.active_bucket_mut().backgrounded_roots.insert(tool_use_id);
    }

    /// Clear one sticky backgrounded root - the terminal
    /// `task_updated` / `task_notification` path.
    pub(crate) fn clear_backgrounded_root(&mut self, tool_use_id: &str) {
        self.active_bucket_mut().backgrounded_roots.remove(tool_use_id);
    }

    /// Whether `subagents_view` would return anything, without building it.
    /// Short-circuits on the first live root instead of indexing every tool
    /// call in the session. Root derivation mirrors `subagents_view`,
    /// including the unscoped parents of registered child scopes (#808).
    pub fn has_active_subagent_root(&self) -> bool {
        let Some(session) = self.active_session() else {
            return false;
        };
        // No scopes at all means no roots and no child frames, so skip
        // the message walk entirely - the common case for a session that
        // never dispatched one.
        if session.tool_call_scopes.is_empty() {
            return false;
        }
        let backgrounded_alive = session.backgrounded_alive_tool_use_ids();
        // Parents named by child scopes; the ones carrying no scope of
        // their own are root candidates alongside registered roots.
        let mut referenced_parents: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for scope in session.tool_call_scopes.values() {
            if let crate::app::state::types::ToolCallScope::SubagentChild { parent_tool_use_id } =
                scope
            {
                referenced_parents.insert(parent_tool_use_id.as_str());
            }
        }
        // First occurrence of an id wins, mirroring the `by_id` index the
        // view builds, so a duplicate cannot revive a drained root.
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        let mut open_child_parents: std::collections::HashSet<&str> =
            std::collections::HashSet::new();
        for msg in &session.messages {
            for block in &msg.blocks {
                let crate::app::MessageBlock::ToolCall(tc) = block else {
                    continue;
                };
                if !seen.insert(tc.id.as_str()) {
                    continue;
                }
                let id = tc.id.as_str();
                let is_open = matches!(
                    tc.status,
                    crate::agent::model::ToolCallStatus::InProgress
                        | crate::agent::model::ToolCallStatus::Pending
                );
                let scope = session.tool_call_scopes.get(id);
                let is_root =
                    matches!(scope, Some(crate::app::state::types::ToolCallScope::SubagentRoot))
                        || (scope.is_none() && referenced_parents.contains(id));
                if is_root && (backgrounded_alive.contains(id) || is_open) {
                    return true;
                }
                if is_open
                    && let Some(crate::app::state::types::ToolCallScope::SubagentChild {
                        parent_tool_use_id,
                    }) = scope
                {
                    open_child_parents.insert(parent_tool_use_id.as_str());
                }
            }
        }
        // A root kept open only by its children (#808): the parent must
        // be among the walked cards, and either a registered root or an
        // unscoped parent candidate.
        open_child_parents.iter().any(|parent| {
            seen.contains(*parent)
                && match session.tool_call_scopes.get(*parent) {
                    Some(crate::app::state::types::ToolCallScope::SubagentRoot) => true,
                    Some(_) => false,
                    None => referenced_parents.contains(parent),
                }
        })
    }

    /// Active-session SUBAGENTS Inspector view. Derives one entry
    /// per `Task` / `Agent` dispatch (a visible root) plus a tail of
    /// the last `SUBAGENT_TAIL_CAP` `SubagentChild` tool calls under
    /// each root, identified via `parent_tool_use_id` on the
    /// scope-registered map. Returns an empty Vec when every root is
    /// terminal AND absent from the session roster - mirrors
    /// `clear_workflows_if_all_terminal` so the section auto-clears.
    /// Pure derive over `UiSession` state; no mutation, no new wire
    /// surface.
    pub fn subagents_view(&self) -> Vec<crate::app::state::types::SubagentEntry> {
        let Some(session) = self.active_session() else {
            return Vec::new();
        };

        // Index every tool call by id and remember the registered
        // scope. Walking each message linearly preserves block order,
        // which is what feeds the chronological tail later.
        let mut by_id: std::collections::HashMap<&str, &crate::app::ToolCallInfo> =
            std::collections::HashMap::new();
        let mut ordered_tool_ids: Vec<&str> = Vec::new();
        for msg in &session.messages {
            for block in &msg.blocks {
                if let crate::app::MessageBlock::ToolCall(tc) = block
                    && !by_id.contains_key(tc.id.as_str())
                {
                    by_id.insert(tc.id.as_str(), tc.as_ref());
                    ordered_tool_ids.push(tc.id.as_str());
                }
            }
        }

        // Children per parent id from the registered child scopes; a
        // parent is keyed only when its own card is in the index.
        let mut children_by_parent: std::collections::HashMap<
            &str,
            Vec<&crate::app::ToolCallInfo>,
        > = std::collections::HashMap::new();
        for id in &ordered_tool_ids {
            let Some(tc) = by_id.get(id) else { continue };
            if let Some(crate::app::state::types::ToolCallScope::SubagentChild {
                parent_tool_use_id,
            }) = self.tool_call_scope(id)
                && let Some((&parent_key, _)) = by_id.get_key_value(parent_tool_use_id.as_str())
            {
                // The parent's id is in the registered scope - copy a
                // stable str borrow off the indexed map (its keys
                // outlive the children vec).
                children_by_parent.entry(parent_key).or_default().push(tc);
            }
        }
        // A resumed agent's replayed Task card carries no scope (resume
        // registers none), but its live child frames still name the
        // card - such a parent is a root too (#808).
        let unscoped_parents: std::collections::HashSet<&str> = children_by_parent
            .keys()
            .filter(|id| self.tool_call_scope(id).is_none())
            .copied()
            .collect();
        // Roots in dispatch order, scoped and unscoped alike.
        let mut roots: Vec<&crate::app::ToolCallInfo> = Vec::new();
        for id in &ordered_tool_ids {
            let scope = self.tool_call_scope(id);
            let is_root =
                matches!(scope, Some(crate::app::state::types::ToolCallScope::SubagentRoot))
                    || (scope.is_none() && unscoped_parents.contains(*id));
            if is_root && let Some(tc) = by_id.get(id) {
                roots.push(tc);
            }
        }

        // Liveness follows the task's real lifecycle, not the turn. The CLI
        // backgrounds a subagent with an immediate sentinel tool_result that
        // flips its root card terminal while the task keeps running, and its
        // spawning turn Results before it finishes - so `status` alone is
        // unreliable and the turn-scoped alive set is wiped underneath it.
        // The durable signal is the session roster (`background_tasks`
        // INTERSECT the session task map), which survives turn finalisation
        // and covers every backgrounded kind. A genuinely running
        // non-backgrounded root still surfaces via its own in-flight status.
        let backgrounded_alive = session.backgrounded_alive_tool_use_ids();
        // A root kept open by its children: a resumed agent's own card is
        // terminal from the replay, so an open child under it is the only
        // running evidence (#808).
        let open_child_roots: std::collections::HashSet<&str> = children_by_parent
            .iter()
            .filter_map(|(parent, children)| {
                children
                    .iter()
                    .any(|c| {
                        matches!(
                            c.status,
                            crate::agent::model::ToolCallStatus::InProgress
                                | crate::agent::model::ToolCallStatus::Pending
                        )
                    })
                    .then_some(*parent)
            })
            .collect();
        let root_is_active = |root: &&crate::app::ToolCallInfo| {
            backgrounded_alive.contains(root.id.as_str())
                || open_child_roots.contains(root.id.as_str())
                || matches!(
                    root.status,
                    crate::agent::model::ToolCallStatus::InProgress
                        | crate::agent::model::ToolCallStatus::Pending
                )
        };
        // Auto-clear: the section disappears only once no root is still
        // active (every root both terminal-status AND drained from the
        // alive set). Empty `roots` already gates via `is_empty`.
        if !roots.is_empty() && !roots.iter().any(root_is_active) {
            return Vec::new();
        }

        let cap = crate::app::state::types::SUBAGENT_TAIL_CAP;
        roots
            .into_iter()
            .map(|root| {
                let children = children_by_parent.remove(root.id.as_str()).unwrap_or_default();
                let total_count = children.len();
                // Alive-but-terminal roots (backgrounded) render running; a
                // still-`Pending` root stays queued rather than spinning.
                let running = root_is_active(&root)
                    && root.status != crate::agent::model::ToolCallStatus::Pending;
                let status = if running {
                    crate::agent::model::ToolCallStatus::InProgress
                } else {
                    root.status
                };
                let tail = if running {
                    let tail_start = children.len().saturating_sub(cap);
                    children[tail_start..]
                        .iter()
                        .map(|c| crate::app::state::types::SubagentChildEntry {
                            sdk_tool_name: c.sdk_tool_name.clone(),
                            title: c.title.clone(),
                            status: c.status,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                crate::app::state::types::SubagentEntry {
                    tool_use_id: root.id.clone(),
                    label: subagent_label_from_root(root),
                    status,
                    tail,
                    total_count,
                }
            })
            .collect()
    }
}

/// Build the SUBAGENTS row's header label from a Task/Agent root
/// tool call's `raw_input`. Combines `subagent_type` with the first
/// non-empty line of `description` (or `prompt` as a sibling fallback)
/// into `"<type> · <line>"`. Falls back to either piece on its own
/// when the other is missing, then to the raw `sdk_tool_name` so the
/// row always renders something even on a malformed dispatch.
fn subagent_label_from_root(root: &crate::app::ToolCallInfo) -> String {
    let raw = root.raw_input.as_ref().and_then(|v| v.as_object());
    let read = |k: &str| -> Option<String> {
        raw.and_then(|r| r.get(k))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    let subagent_type = read("subagent_type");
    let summary = read("description")
        .or_else(|| read("prompt"))
        .and_then(|s| s.lines().find(|line| !line.trim().is_empty()).map(str::to_owned))
        .map(|s| s.trim().to_owned());
    match (subagent_type, summary) {
        (Some(kind), Some(line)) => format!("{kind} \u{b7} {line}"),
        (Some(kind), None) => kind,
        (None, Some(line)) => line,
        (None, None) => root.sdk_tool_name.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        App, BlockCache, ChatMessage, MessageBlock, MessageRole, SUBAGENT_TAIL_CAP, TextBlock,
        ToolCallInfo, ToolCallScope,
    };
    use crate::agent::model;
    use crate::app::state::tests::{
        assistant_bash_tool_message, assistant_tool_message, make_test_app, user_text_message,
    };
    use pretty_assertions::assert_eq;

    #[test]
    fn record_answered_question_unhides_and_appends() {
        use crate::agent::model::ToolCallStatus;
        let mut app = App::test_default();
        let tool_use_id = "tu-q-1";
        // A chat-suppressed (hidden) AskUserQuestion, as it sits while
        // the dock prompt is the live answering surface.
        let tc_info = ToolCallInfo {
            id: tool_use_id.to_owned(),
            title: "AskUserQuestion".to_owned(),
            sdk_tool_name: "AskUserQuestion".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
            content: Vec::new(),
            hidden: true,
            terminal_id: None,
            terminal_output: None,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        };
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc_info))],
        ));
        let msg_idx = app.messages().len() - 1;
        app.index_tool_call(tool_use_id.to_owned(), msg_idx, 0);

        app.record_answered_question(
            tool_use_id,
            crate::app::AnsweredQuestion {
                question: "Which path?".to_owned(),
                picked_labels: vec!["Clean card".to_owned()],
                typed_note: None,
            },
        );

        let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert!(!tc.hidden, "answered question must un-hide so the card renders");
        assert_eq!(tc.answered_questions.len(), 1);
        assert_eq!(tc.answered_questions[0].picked_labels, vec!["Clean card".to_owned()]);
        assert!(tc.answered_questions[0].typed_note.is_none());
    }

    /// `RenderUnit::Individual` and is handed `app.tools_collapsed`
    /// directly instead of a group-derived level; grouped tools get
    /// that flag overwritten and never reach the measure call. Plain
    /// text content rather than a `Diff` keeps it out of the carve-out.
    fn ungrouped_tool_message(id: &str) -> ChatMessage {
        let mut msg = assistant_tool_message(id, model::ToolCallStatus::Failed);
        if let MessageBlock::ToolCall(tc) = &mut msg.blocks[0] {
            tc.sdk_tool_name = "Edit".to_owned();
            tc.title = format!("Edit {id}");
            tc.content =
                vec![model::ToolCallContent::from("alpha\nbeta\ngamma\ndelta\nepsilon".to_owned())];
        }
        msg
    }

    fn head_tool(app: &App) -> &ToolCallInfo {
        match &app.messages()[0].blocks[0] {
            MessageBlock::ToolCall(tc) => tc,
            _ => panic!("expected a tool call"),
        }
    }

    /// Draw a real chat frame, so the tool goes through the same
    /// grouping and measurement the running app puts it through.
    fn render_chat_frame(app: &mut App, width: u16, height: u16) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| {
                crate::ui::chat::render(
                    frame,
                    ratatui::layout::Rect::new(0, 0, width, height),
                    app,
                    &[],
                );
            })
            .expect("draw");
    }

    /// ctrl+x flips the collapse preference App-wide, but
    /// `invalidate_layout`'s `Global` arm bumps only the ACTIVE
    /// session's viewport and clears overrides only on that session's
    /// blocks, and switching sessions invalidates nothing. So an
    /// unfocused session reaches its next render with width, layout
    /// epoch and generation all unmoved: without the preference in the
    /// key nothing forces a remeasure, and the tool keeps a height
    /// taken under the old preference. Mouse hit-testing sizes the
    /// click box from that height.
    #[test]
    fn cross_session_collapse_flip_remeasures_ungrouped_tool() {
        const W: u16 = 80;
        const H: u16 = 40;

        let mut app = make_test_app();
        app.tools_collapsed = true;
        let a_key = app.active_session_key.clone().expect("an active session");

        let b_key = forge_workspace::SessionKey::from_str_for_test("collapse-cross-session");
        let mut b_bucket = crate::app::session::UiSession::new(b_key.clone());
        b_bucket.messages = vec![ungrouped_tool_message("cross-session")];
        app.sessions.insert(b_key.clone(), b_bucket);

        // B renders once collapsed, stamping its per-tool key.
        app.switch_active_session(b_key.clone());
        render_chat_frame(&mut app, W, H);
        let collapsed_height = head_tool(&app).last_measured_height;
        let epoch_before = head_tool(&app).layout_epoch;
        let width_before = head_tool(&app).last_measured_width;
        let generation_before = app.viewport().layout_generation;

        // The flip happens while A is focused.
        app.switch_active_session(a_key);
        crate::app::keys::toggle_all_tool_calls(&mut app);
        assert!(!app.tools_collapsed, "ctrl+x expanded the shared preference");

        // Back to B at the same size, no resize and no click.
        app.switch_active_session(b_key);
        render_chat_frame(&mut app, W, H);
        assert_eq!(
            generation_before,
            app.viewport().layout_generation,
            "no resize, so B's generation must not move",
        );
        assert_eq!(epoch_before, head_tool(&app).layout_epoch, "and its layout epoch must not");
        assert_eq!(width_before, head_tool(&app).last_measured_width, "nor its measured width");
        let after_flip = head_tool(&app).last_measured_height;

        // What the same tool measures from cold under the new preference.
        let mut cold = make_test_app();
        cold.tools_collapsed = false;
        *cold.active_messages_mut() = vec![ungrouped_tool_message("cross-session")];
        render_chat_frame(&mut cold, W, H);
        let correct_height = head_tool(&cold).last_measured_height;

        assert_ne!(
            collapsed_height, correct_height,
            "the preference has to move this tool's height, or the assertion below is free",
        );
        assert_eq!(
            after_flip, correct_height,
            "B's tool kept a height measured under the old preference \
             (collapsed={collapsed_height}, correct={correct_height}, got={after_flip})",
        );
    }

    /// #651. The per-tool measurement key above repairs the tool's own
    /// cached height, but the viewport's per-message height is written
    /// only by the remeasure pass, and that pass skips any message
    /// whose stale bit is clear. So an unfocused session reaches its
    /// next render reporting rows measured under the old preference
    /// while painting the new one, and every row offset below it -
    /// scroll geometry, click hit-testing - is off by the difference.
    #[test]
    fn cross_session_collapse_flip_remeasures_background_viewport_height() {
        const W: u16 = 80;
        const H: u16 = 40;

        fn head_height(app: &App) -> usize {
            app.viewport().message_height(0)
        }

        let mut app = make_test_app();
        app.tools_collapsed = true;
        let a_key = app.active_session_key.clone().expect("an active session");
        *app.active_messages_mut() = vec![ungrouped_tool_message("cross-session")];

        let b_key = forge_workspace::SessionKey::from_str_for_test("collapse-background-height");
        let mut b_bucket = crate::app::session::UiSession::new(b_key.clone());
        b_bucket.messages = vec![ungrouped_tool_message("cross-session")];
        app.sessions.insert(b_key.clone(), b_bucket);

        // Both sessions measure once under the collapsed preference.
        render_chat_frame(&mut app, W, H);
        app.switch_active_session(b_key.clone());
        render_chat_frame(&mut app, W, H);
        let collapsed_height = head_height(&app);

        // The flip happens while A is focused.
        app.switch_active_session(a_key);
        crate::app::keys::toggle_all_tool_calls(&mut app);
        assert!(!app.tools_collapsed, "ctrl+x expanded the shared preference");
        render_chat_frame(&mut app, W, H);
        let active_height = head_height(&app);

        // Back to B at the same size, no resize and no click.
        app.switch_active_session(b_key);
        render_chat_frame(&mut app, W, H);
        let background_height = head_height(&app);

        // What the same message measures from cold under the new preference.
        let mut cold = make_test_app();
        cold.tools_collapsed = false;
        *cold.active_messages_mut() = vec![ungrouped_tool_message("cross-session")];
        render_chat_frame(&mut cold, W, H);
        let expanded_height = head_height(&cold);

        assert_ne!(
            collapsed_height, expanded_height,
            "the preference has to move this message's height, or the assertions below are free",
        );
        assert_eq!(
            active_height, expanded_height,
            "the focused session's viewport must remeasure on the flip \
             (collapsed={collapsed_height}, correct={expanded_height}, got={active_height})",
        );
        assert_eq!(
            background_height, expanded_height,
            "the background session's viewport kept a height measured under the old preference \
             (collapsed={collapsed_height}, correct={expanded_height}, got={background_height})",
        );
    }

    #[test]
    fn lookup_missing_returns_none() {
        let app = make_test_app();
        assert!(app.lookup_tool_call("nonexistent").is_none());
    }

    #[test]
    fn index_and_lookup() {
        let mut app = make_test_app();
        app.index_tool_call("tc-123".into(), 2, 5);
        assert_eq!(app.lookup_tool_call("tc-123"), Some((2, 5)));
    }

    /// Index same ID twice - second write overwrites first.
    #[test]
    fn index_overwrite_existing() {
        let mut app = make_test_app();
        app.index_tool_call("tc-1".into(), 0, 0);
        app.index_tool_call("tc-1".into(), 5, 10);
        assert_eq!(app.lookup_tool_call("tc-1"), Some((5, 10)));
    }

    /// Empty string as tool call ID.
    #[test]
    fn index_empty_string_id() {
        let mut app = make_test_app();
        app.index_tool_call(String::new(), 1, 2);
        assert_eq!(app.lookup_tool_call(""), Some((1, 2)));
    }

    /// Stress: 1000 tool calls indexed and looked up.
    #[test]
    fn index_stress_1000_entries() {
        let mut app = make_test_app();
        for i in 0..1000 {
            app.index_tool_call(format!("tc-{i}"), i, i * 2);
        }
        // Spot check first, middle, last
        assert_eq!(app.lookup_tool_call("tc-0"), Some((0, 0)));
        assert_eq!(app.lookup_tool_call("tc-500"), Some((500, 1000)));
        assert_eq!(app.lookup_tool_call("tc-999"), Some((999, 1998)));
        // Non-existent still returns None
        assert!(app.lookup_tool_call("tc-1000").is_none());
    }

    /// Unicode in tool call ID.
    #[test]
    fn index_unicode_id() {
        let mut app = make_test_app();
        app.index_tool_call("\u{1F600}-tool".into(), 3, 7);
        assert_eq!(app.lookup_tool_call("\u{1F600}-tool"), Some((3, 7)));
    }

    // active_task_ids

    #[test]
    fn active_task_insert_remove() {
        let mut app = make_test_app();
        app.insert_active_task("task-1".into());
        assert!(app.active_task_ids().contains("task-1"));
        app.remove_active_task("task-1");
        assert!(!app.active_task_ids().contains("task-1"));
    }

    #[test]
    fn remove_nonexistent_task_is_noop() {
        let mut app = make_test_app();
        app.remove_active_task("does-not-exist");
        assert!(app.active_task_ids().is_empty());
    }

    /// Insert same ID twice - set deduplicates; one remove clears it.
    #[test]
    fn active_task_insert_duplicate() {
        let mut app = make_test_app();
        app.insert_active_task("task-1".into());
        app.insert_active_task("task-1".into());
        assert_eq!(app.active_task_ids().len(), 1);
        app.remove_active_task("task-1");
        assert!(app.active_task_ids().is_empty());
    }

    /// Insert many tasks, remove in different order.
    #[test]
    fn active_task_insert_many_remove_out_of_order() {
        let mut app = make_test_app();
        for i in 0..100 {
            app.insert_active_task(format!("task-{i}"));
        }
        assert_eq!(app.active_task_ids().len(), 100);
        // Remove in reverse order
        for i in (0..100).rev() {
            app.remove_active_task(&format!("task-{i}"));
        }
        assert!(app.active_task_ids().is_empty());
    }

    /// Mixed insert/remove interleaving.
    #[test]
    fn active_task_interleaved_insert_remove() {
        let mut app = make_test_app();
        app.insert_active_task("a".into());
        app.insert_active_task("b".into());
        app.remove_active_task("a");
        app.insert_active_task("c".into());
        assert!(!app.active_task_ids().contains("a"));
        assert!(app.active_task_ids().contains("b"));
        assert!(app.active_task_ids().contains("c"));
        assert_eq!(app.active_task_ids().len(), 2);
    }

    /// Remove from empty set multiple times - no panic.
    #[test]
    fn active_task_remove_from_empty_repeatedly() {
        let mut app = make_test_app();
        for i in 0..100 {
            app.remove_active_task(&format!("ghost-{i}"));
        }
        assert!(app.active_task_ids().is_empty());
    }

    /// `clear_tool_scope_tracking` must also clear `active_task_ids`;
    /// a leaked task ID from a cancelled turn would otherwise cause
    /// main-agent tools on the next turn to be misclassified as
    /// Subagent scope.
    #[test]
    fn clear_tool_scope_tracking_also_clears_active_task_ids() {
        let mut app = make_test_app();
        app.insert_active_task("task-leaked".into());
        assert!(!app.active_task_ids().is_empty());
        app.clear_tool_scope_tracking();
        assert!(app.active_task_ids().is_empty(), "active_task_ids must be cleared at turn end");
    }

    /// Identity-layer sibling of the finalize exemption: a still-running
    /// backgrounded root and its children keep their scope across
    /// turn-complete so SUBAGENTS can still identify them; a main-agent
    /// scope always clears, and a completed root's scope drops on the next
    /// clear once it leaves the roster - so nothing leaks.
    #[test]
    fn clear_tool_scope_tracking_retains_live_backgrounded_scopes_then_drops_them() {
        let mut app = make_test_app();
        app.tool_call_scopes_mut().insert("toolu_root".to_owned(), ToolCallScope::SubagentRoot);
        app.tool_call_scopes_mut().insert(
            "toolu_child".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() },
        );
        app.tool_call_scopes_mut().insert("toolu_main".to_owned(), ToolCallScope::MainAgent);
        app.insert_session_task_mapping("task-root".to_owned(), "toolu_root".to_owned());
        *app.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-root".to_owned(),
            task_type: "local_agent".to_owned(),
            description: String::new(),
        }];

        // Turn-complete while the agent is still backgrounded.
        app.clear_tool_scope_tracking();
        assert!(app.tool_call_scope("toolu_root").is_some(), "live backgrounded root retained");
        assert!(app.tool_call_scope("toolu_child").is_some(), "its child retained");
        assert!(app.tool_call_scope("toolu_main").is_none(), "main-agent scope always cleared");

        // The task completes + drops from the roster; the next clear drops it.
        app.remove_session_task_mapping("task-root");
        app.clear_tool_scope_tracking();
        assert!(app.tool_call_scope("toolu_root").is_none(), "completed root scope dropped");
        assert!(app.tool_call_scope("toolu_child").is_none(), "orphaned child scope dropped");
    }

    /// A backgrounded subagent's children used to hold their scopes until
    /// the ROOT settled, so the scope map grew with the subagent's total
    /// tool-call count (#791). A child whose own card is terminal cannot
    /// be swept into anything - sweeps only touch open calls - so its
    /// scope drops at the turn boundary; the root and still-open
    /// children stay.
    #[test]
    fn terminal_children_drop_their_scope_at_the_turn_boundary() {
        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_root", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_open_child", model::ToolCallStatus::InProgress));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_done_child", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_dead_child", model::ToolCallStatus::Failed));
        for (idx, id) in ["toolu_root", "toolu_open_child", "toolu_done_child", "toolu_dead_child"]
            .into_iter()
            .enumerate()
        {
            app.index_tool_call(id.to_owned(), idx, 0);
        }
        app.tool_call_scopes_mut().insert("toolu_root".to_owned(), ToolCallScope::SubagentRoot);
        for id in ["toolu_open_child", "toolu_done_child", "toolu_dead_child"] {
            app.tool_call_scopes_mut().insert(
                id.to_owned(),
                ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() },
            );
        }
        app.insert_session_task_mapping("task-root".to_owned(), "toolu_root".to_owned());
        *app.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-root".to_owned(),
            task_type: "local_agent".to_owned(),
            description: String::new(),
        }];

        app.clear_tool_scope_tracking();

        assert!(app.tool_call_scope("toolu_root").is_some(), "live root kept");
        assert!(
            app.tool_call_scope("toolu_open_child").is_some(),
            "the open child keeps its scope - it still needs the sweep exemption",
        );
        assert!(
            app.tool_call_scope("toolu_done_child").is_none(),
            "a terminal child's scope drops at the boundary",
        );
        assert!(
            app.tool_call_scope("toolu_dead_child").is_none(),
            "a failed child's scope drops too",
        );
    }

    /// The pin behind the rule above: a terminal nested Task may drop its
    /// scope, but a grandchild still running under it must not lose its
    /// sweep exemption - a nested Task that is terminal-yet-backgrounded
    /// carries its own roster row, so the grandchild resolves to IT as a
    /// live root, not through the dropped scope.
    #[test]
    fn a_live_grandchild_is_not_stranded_behind_its_terminal_nested_parent() {
        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_root", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_nested", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_gchild", model::ToolCallStatus::InProgress));
        for (idx, id) in ["toolu_root", "toolu_nested", "toolu_gchild"].into_iter().enumerate() {
            app.index_tool_call(id.to_owned(), idx, 0);
        }
        app.tool_call_scopes_mut().insert("toolu_root".to_owned(), ToolCallScope::SubagentRoot);
        app.tool_call_scopes_mut().insert(
            "toolu_nested".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() },
        );
        app.tool_call_scopes_mut().insert(
            "toolu_gchild".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_nested".to_owned() },
        );
        app.insert_session_task_mapping("task-root".to_owned(), "toolu_root".to_owned());
        app.insert_session_task_mapping("task-nested".to_owned(), "toolu_nested".to_owned());
        *app.background_tasks_mut() = vec![
            crate::app::state::types::BackgroundTask {
                task_id: "task-root".to_owned(),
                task_type: "local_agent".to_owned(),
                description: String::new(),
            },
            crate::app::state::types::BackgroundTask {
                task_id: "task-nested".to_owned(),
                task_type: "local_agent".to_owned(),
                description: String::new(),
            },
        ];

        app.clear_tool_scope_tracking();
        assert!(
            app.tool_call_scope("toolu_gchild").is_some(),
            "the grandchild's scope survives its parent's drop",
        );
        assert_eq!(
            app.finalize_in_progress_tool_calls(model::ToolCallStatus::Completed),
            0,
            "and the sweep spares it while it runs",
        );
    }

    /// The turn-boundary sweeps answer liveness per open call (#793);
    /// they must not derive the eager alive-with-children set off the
    /// whole scope map - that cost scales with the map #791 just
    /// bounded. The debug record in `backgrounded_alive_with_children`
    /// is the probe: a sweep site must not emit it, while the sweep
    /// still spares exactly the live work. The probe is introduced by
    /// this PR, so this test is mutation-verified rather than red on
    /// main - deleting the eager call cannot exist as a prior state.
    #[test]
    fn the_turn_boundary_sweep_does_not_build_the_eager_exempt_set() {
        use std::sync::{Arc, Mutex};

        #[derive(Default, Clone)]
        struct EventNames(Arc<Mutex<Vec<String>>>);

        struct CollectEventName(String);

        impl tracing::field::Visit for CollectEventName {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "event_name" {
                    self.0 = format!("{value:?}").trim_matches('"').to_owned();
                }
            }
        }

        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for EventNames {
            fn on_event(
                &self,
                event: &tracing::Event<'_>,
                _ctx: tracing_subscriber::layer::Context<'_, S>,
            ) {
                let mut visitor = CollectEventName(String::new());
                event.record(&mut visitor);
                if !visitor.0.is_empty() {
                    self.0.lock().expect("capture").push(visitor.0);
                }
            }
        }

        use tracing_subscriber::layer::SubscriberExt;

        let names = EventNames::default();
        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_root", model::ToolCallStatus::Completed));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_child", model::ToolCallStatus::InProgress));
        app.active_messages_mut()
            .push(assistant_tool_message("toolu_plain_bash", model::ToolCallStatus::InProgress));
        for (idx, id) in ["toolu_root", "toolu_child", "toolu_plain_bash"].into_iter().enumerate() {
            app.index_tool_call(id.to_owned(), idx, 0);
        }
        app.tool_call_scopes_mut().insert("toolu_root".to_owned(), ToolCallScope::SubagentRoot);
        app.tool_call_scopes_mut().insert(
            "toolu_child".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "toolu_root".to_owned() },
        );
        app.insert_session_task_mapping("task-root".to_owned(), "toolu_root".to_owned());
        *app.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-root".to_owned(),
            task_type: "local_agent".to_owned(),
            description: String::new(),
        }];

        let subscriber = tracing_subscriber::registry().with(names.clone());
        tracing::subscriber::with_default(subscriber, || {
            assert_eq!(
                app.finalize_in_progress_tool_calls(model::ToolCallStatus::Completed),
                1,
                "only the unrelated bash sweeps; the live child is exempt either way",
            );
        });
        assert!(
            !names
                .0
                .lock()
                .expect("capture")
                .iter()
                .any(|name| name == "backgrounded_alive_set_built"),
            "the sweep derived the eager exempt set off the scope map; saw {:?}",
            names.0.lock().expect("capture"),
        );
    }

    #[test]
    fn finalize_in_progress_tool_calls_detaches_execute_terminal_refs() {
        let mut app = make_test_app();
        app.active_messages_mut().push(assistant_bash_tool_message(
            "bash-1",
            model::ToolCallStatus::InProgress,
            "term-1",
        ));
        app.index_tool_call("bash-1".to_owned(), 0, 0);

        let changed = app.finalize_in_progress_tool_calls(model::ToolCallStatus::Completed);

        assert_eq!(changed, 1);
        let MessageBlock::ToolCall(tc) = &app.messages()[0].blocks[0] else {
            panic!("expected tool call");
        };
        assert_eq!(tc.status, model::ToolCallStatus::Completed);
        assert_eq!(tc.terminal_id, None);
    }

    #[test]
    fn finalize_in_progress_tool_calls_invalidates_all_changed_messages() {
        let mut app = make_test_app();
        app.active_messages_mut()
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::InProgress));
        app.active_messages_mut().push(user_text_message("gap"));
        app.active_messages_mut()
            .push(assistant_tool_message("tool-2", model::ToolCallStatus::InProgress));

        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().sync_message_count(3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();

        let changed = app.finalize_in_progress_tool_calls(model::ToolCallStatus::Completed);

        assert_eq!(changed, 2);
        assert!(!app.active_viewport_mut().message_height_is_current(0));
        assert!(app.active_viewport_mut().message_height_is_current(1));
        assert!(!app.active_viewport_mut().message_height_is_current(2));
        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(0));
    }

    #[test]
    fn group_collapse_level_defaults_to_l2_when_absent() {
        use crate::ui::message::grouping::{GroupCollapseLevel, GroupId};
        let app = App::test_default();
        let id = GroupId::from_leader_id("tu-x");
        assert_eq!(app.group_collapse_level(&id), GroupCollapseLevel::L2Summary);
    }

    /// Cmd+X with no prior click flips the global `tools_collapsed`
    /// flag and emits a Global invalidation. Per-group cycling is
    /// bound to mouse-click on a group summary row; the keyboard
    /// shortcut is the global toggle, always.
    #[test]
    fn cmd_x_with_no_prior_click_toggles_global_tools_collapsed() {
        let mut app = App::test_default();
        let initial = app.tools_collapsed;
        app.last_invalidation_level.set(None);
        crate::app::keys::toggle_all_tool_calls(&mut app);
        assert_eq!(app.tools_collapsed, !initial, "Cmd+X must flip tools_collapsed globally",);
        assert_eq!(
            app.last_invalidation_level.get(),
            Some(crate::app::InvalidationLevel::Global),
            "Cmd+X must emit Global invalidation",
        );
    }

    /// Cmd+X clears every tool-call's `collapsed_override` across
    /// the active session's message list so older / scrolled-up
    /// tools snap to the global state on the flip - per-tool
    /// overrides don't survive Cmd+X.
    #[test]
    fn cmd_x_clears_collapsed_override_on_all_tool_calls() {
        let mut app = App::test_default();
        let push_tool = |app: &mut App, id: &str, override_val: bool| {
            app.active_messages_mut().push(ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageBlock::ToolCall(Box::new(ToolCallInfo {
                    id: id.to_owned(),
                    title: format!("Read {id}"),
                    sdk_tool_name: "Read".to_owned(),
                    raw_input: None,
                    raw_input_bytes: 0,
                    output_metadata: None,
                    task_metadata: None,
                    status: model::ToolCallStatus::Completed,
                    content: Vec::new(),
                    hidden: false,
                    terminal_id: None,
                    terminal_output: None,
                    monitor_output_tail: Vec::default(),
                    monitor_status: None,
                    render_epoch: 0,
                    layout_epoch: 0,
                    last_measured_y_in_msg: 0,
                    answered_questions: Vec::new(),
                    last_measured_height: 0,
                    last_measured_width: 0,
                    last_measured_layout_epoch: 0,
                    last_measured_layout_generation: 0,
                    last_measured_tools_collapsed: false,
                    cache: BlockCache::default(),
                    collapsed_override: Some(override_val),
                }))],
            ));
        };
        push_tool(&mut app, "tu-a", true);
        push_tool(&mut app, "tu-b", false);

        let read_override = |app: &App, id: &str| -> Option<bool> {
            app.active_session()
                .expect("active session")
                .messages
                .iter()
                .find_map(|msg| {
                    msg.blocks.iter().find_map(|b| match b {
                        MessageBlock::ToolCall(tc) if tc.id == id => Some(tc.collapsed_override),
                        _ => None,
                    })
                })
                .expect("tool found")
        };
        assert_eq!(read_override(&app, "tu-a"), Some(true));
        assert_eq!(read_override(&app, "tu-b"), Some(false));

        crate::app::keys::toggle_all_tool_calls(&mut app);

        assert_eq!(
            read_override(&app, "tu-a"),
            None,
            "tool A's collapsed_override must clear on Cmd+X",
        );
        assert_eq!(
            read_override(&app, "tu-b"),
            None,
            "tool B's collapsed_override must clear on Cmd+X",
        );
    }

    /// Cmd+X clears every peer-inbound text block's
    /// `peer_collapsed_override` so MCP messages snap to the global
    /// state on the flip - per-peer-block overrides don't survive
    /// Cmd+X.
    #[test]
    fn cmd_x_clears_peer_collapsed_override_on_all_text_blocks() {
        let mut app = App::test_default();
        let push_peer = |app: &mut App, sender: &str, override_val: bool| {
            let text =
                format!("[Message id=t-12345678 from agent '{sender}' (org 'Personal')]\n\nhi");
            let mut block = TextBlock::from_complete(&text);
            block.peer_collapsed_override = Some(override_val);
            app.active_messages_mut().push(ChatMessage::new_peer_envelope(
                MessageRole::User,
                vec![MessageBlock::Text(block)],
            ));
        };
        push_peer(&mut app, "peer-a", true);
        push_peer(&mut app, "peer-b", false);

        let read_override = |app: &App, msg_idx: usize| -> Option<bool> {
            match &app.active_session().expect("session").messages[msg_idx].blocks[0] {
                MessageBlock::Text(b) => b.peer_collapsed_override,
                _ => panic!("expected text block"),
            }
        };
        assert_eq!(read_override(&app, 0), Some(true));
        assert_eq!(read_override(&app, 1), Some(false));

        crate::app::keys::toggle_all_tool_calls(&mut app);

        assert_eq!(
            read_override(&app, 0),
            None,
            "peer A's peer_collapsed_override must clear on Cmd+X",
        );
        assert_eq!(
            read_override(&app, 1),
            None,
            "peer B's peer_collapsed_override must clear on Cmd+X",
        );
    }

    /// Cmd+X clears the `group_collapse_levels` map so older /
    /// scrolled-up groups snap to the global state on the flip -
    /// per-group cycle state doesn't survive Cmd+X.
    #[test]
    fn cmd_x_clears_group_collapse_levels_map() {
        use crate::ui::message::grouping::GroupId;
        let mut app = App::test_default();
        let group_a = GroupId::from_leader_id("tu-leader-a");
        let group_b = GroupId::from_leader_id("tu-leader-b");
        let _ = app.cycle_group_collapse_level(&group_a);
        let _ = app.cycle_group_collapse_level(&group_b);
        assert!(
            app.active_session().expect("session").group_collapse_levels.contains_key(&group_a),
            "group A's level recorded pre-Cmd+X",
        );
        assert!(
            app.active_session().expect("session").group_collapse_levels.contains_key(&group_b),
            "group B's level recorded pre-Cmd+X",
        );

        crate::app::keys::toggle_all_tool_calls(&mut app);

        assert!(
            app.active_session().expect("session").group_collapse_levels.is_empty(),
            "group_collapse_levels must be cleared on Cmd+X",
        );
    }

    /// Regression-lock: after Cmd+X clears overrides, the per-tool
    /// `collapsed_override` field is still writable so the next
    /// click can set a fresh per-tool override. The click path is
    /// unchanged; the clear is only at Cmd+X time.
    #[test]
    fn click_on_tool_after_cmd_x_sets_fresh_collapsed_override() {
        fn read_override(app: &App) -> Option<bool> {
            app.active_session()
                .expect("session")
                .messages
                .iter()
                .find_map(|msg| {
                    msg.blocks.iter().find_map(|b| match b {
                        MessageBlock::ToolCall(tc) if tc.id == "tu-a" => {
                            Some(tc.collapsed_override)
                        }
                        _ => None,
                    })
                })
                .expect("tool found")
        }
        fn set_override(app: &mut App, value: Option<bool>) {
            for msg in app.active_messages_mut() {
                for b in &mut msg.blocks {
                    if let MessageBlock::ToolCall(tc) = b
                        && tc.id == "tu-a"
                    {
                        tc.collapsed_override = value;
                        return;
                    }
                }
            }
            panic!("tool not found");
        }
        let mut app = App::test_default();
        app.active_messages_mut().push(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(ToolCallInfo {
                id: "tu-a".to_owned(),
                title: "Read tu-a".to_owned(),
                sdk_tool_name: "Read".to_owned(),
                raw_input: None,
                raw_input_bytes: 0,
                output_metadata: None,
                task_metadata: None,
                status: model::ToolCallStatus::Completed,
                content: Vec::new(),
                hidden: false,
                terminal_id: None,
                terminal_output: None,
                monitor_output_tail: Vec::default(),
                monitor_status: None,
                render_epoch: 0,
                layout_epoch: 0,
                last_measured_y_in_msg: 0,
                answered_questions: Vec::new(),
                last_measured_height: 0,
                last_measured_width: 0,
                last_measured_layout_epoch: 0,
                last_measured_layout_generation: 0,
                last_measured_tools_collapsed: false,
                cache: BlockCache::default(),
                collapsed_override: Some(true),
            }))],
        ));

        crate::app::keys::toggle_all_tool_calls(&mut app);

        assert_eq!(read_override(&app), None, "Cmd+X cleared the override");

        // Simulate a click setting a fresh override post-Cmd+X.
        set_override(&mut app, Some(false));
        assert_eq!(
            read_override(&app),
            Some(false),
            "post-Cmd+X mutation must set a fresh collapsed_override",
        );
    }

    #[test]
    fn cycle_group_collapse_level_walks_l2_l1_l0_back_to_l2() {
        use crate::ui::message::grouping::{GroupCollapseLevel, GroupId};
        let mut app = App::test_default();
        let id = GroupId::from_leader_id("tu-x");
        assert_eq!(app.cycle_group_collapse_level(&id), GroupCollapseLevel::L1Titles);
        assert_eq!(app.group_collapse_level(&id), GroupCollapseLevel::L1Titles);
        assert_eq!(app.cycle_group_collapse_level(&id), GroupCollapseLevel::L0Bodies);
        assert_eq!(app.cycle_group_collapse_level(&id), GroupCollapseLevel::L2Summary);
    }

    // ─── SUBAGENTS Inspector view (subagents_view) ─────────────────
    //
    // Helpers build a session with a Task root + N SubagentChild
    // tool calls underneath it. Each child is registered via
    // `register_tool_call_scope` so `subagents_view` can group them.

    fn make_subagent_root_tc(
        id: &str,
        subagent_type: &str,
        description: &str,
        status: model::ToolCallStatus,
    ) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: "Task".to_owned(),
            sdk_tool_name: "Task".to_owned(),
            raw_input: Some(serde_json::json!({
                "subagent_type": subagent_type,
                "description": description,
                "prompt": description,
            })),
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status,
            content: Vec::new(),
            hidden: false,
            terminal_id: None,
            terminal_output: None,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    fn make_subagent_child_tc(id: &str, sdk_tool_name: &str, title: &str) -> ToolCallInfo {
        ToolCallInfo {
            id: id.to_owned(),
            title: title.to_owned(),
            sdk_tool_name: sdk_tool_name.to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: model::ToolCallStatus::Completed,
            content: Vec::new(),
            hidden: true,
            terminal_id: None,
            terminal_output: None,
            monitor_output_tail: Vec::default(),
            monitor_status: None,
            render_epoch: 0,
            layout_epoch: 0,
            last_measured_width: 0,
            last_measured_height: 0,
            last_measured_layout_epoch: 0,
            last_measured_layout_generation: 0,
            last_measured_tools_collapsed: false,
            cache: BlockCache::default(),
            collapsed_override: None,
            last_measured_y_in_msg: 0,
            answered_questions: Vec::new(),
        }
    }

    fn push_subagent_session(
        app: &mut App,
        root: ToolCallInfo,
        children: Vec<ToolCallInfo>,
    ) -> String {
        let root_id = root.id.clone();
        app.register_tool_call_scope(root_id.clone(), ToolCallScope::SubagentRoot);
        let mut blocks: Vec<MessageBlock> = Vec::with_capacity(1 + children.len());
        blocks.push(MessageBlock::ToolCall(Box::new(root)));
        for child in children {
            app.register_tool_call_scope(
                child.id.clone(),
                ToolCallScope::SubagentChild { parent_tool_use_id: root_id.clone() },
            );
            blocks.push(MessageBlock::ToolCall(Box::new(child)));
        }
        app.push_message_tracked(ChatMessage::new(MessageRole::Assistant, blocks));
        root_id
    }

    /// One running root + a handful of children produces one entry
    /// in the SUBAGENTS view. The label combines subagent_type with
    /// the first line of description; the tail carries each child's
    /// `sdk_tool_name` + `title` in chronological order; total_count
    /// matches the actual children pushed.
    #[test]
    fn subagents_view_collects_roots_and_children() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-1",
            "Explore",
            "map hidden tool calls\nadditional context line",
            model::ToolCallStatus::InProgress,
        );
        let children = vec![
            make_subagent_child_tc("tu-c-1", "Grep", "Grep SubagentChild"),
            make_subagent_child_tc("tu-c-2", "Read", "Read inspector_pane.rs"),
            make_subagent_child_tc("tu-c-3", "Bash", "git log --oneline -3"),
        ];
        push_subagent_session(&mut app, root, children);

        let view = app.subagents_view();
        assert_eq!(view.len(), 1, "one running root produces one entry; got {view:?}");
        let entry = &view[0];
        assert_eq!(entry.tool_use_id, "tu-root-1");
        assert_eq!(
            entry.label, "Explore · map hidden tool calls",
            "label combines subagent_type + first line of description; got {:?}",
            entry.label,
        );
        assert_eq!(entry.status, model::ToolCallStatus::InProgress);
        assert_eq!(entry.total_count, 3);
        assert_eq!(entry.tail.len(), 3);
        assert_eq!(entry.tail[0].sdk_tool_name, "Grep");
        assert_eq!(entry.tail[1].sdk_tool_name, "Read");
        assert_eq!(entry.tail[2].sdk_tool_name, "Bash");
        assert_eq!(entry.tail[2].title, "git log --oneline -3");
    }

    #[test]
    fn subagent_label_from_root_combines_type_and_description() {
        let root = make_subagent_root_tc(
            "tu-label-1",
            "Explore",
            "Map the pipeline",
            model::ToolCallStatus::InProgress,
        );
        assert_eq!(super::subagent_label_from_root(&root), "Explore · Map the pipeline");
    }

    /// With neither `subagent_type` nor `description`, the label falls
    /// back to `sdk_tool_name` (here `"Task"`).
    #[test]
    fn subagent_label_from_root_falls_back_to_tool_name_on_empty_input() {
        let mut root = make_subagent_root_tc(
            "tu-label-2",
            "Explore",
            "Map the pipeline",
            model::ToolCallStatus::InProgress,
        );
        root.raw_input = Some(serde_json::json!({}));
        assert_eq!(super::subagent_label_from_root(&root), "Task");
    }

    /// Tail cap: more than [`SUBAGENT_TAIL_CAP`] children -> tail
    /// surfaces only the LAST N (most recent), total_count counts
    /// every child registered under the root.
    #[test]
    fn subagents_view_tail_caps_at_constant() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-2",
            "code-reviewer",
            "review the diff",
            model::ToolCallStatus::InProgress,
        );
        // 6 children -> tail cap (4) keeps only the LAST 4: c-3..c-6.
        let mut children = Vec::new();
        for i in 1..=6 {
            children.push(make_subagent_child_tc(
                &format!("tu-c-{i}"),
                "Read",
                &format!("file-{i}.rs"),
            ));
        }
        push_subagent_session(&mut app, root, children);

        let view = app.subagents_view();
        assert_eq!(view.len(), 1);
        let entry = &view[0];
        assert_eq!(entry.total_count, 6, "total_count counts every child");
        assert_eq!(
            entry.tail.len(),
            SUBAGENT_TAIL_CAP,
            "tail caps at SUBAGENT_TAIL_CAP; got {} entries",
            entry.tail.len(),
        );
        assert_eq!(
            entry.tail.first().map(|c| c.title.as_str()),
            Some("file-3.rs"),
            "tail drops the oldest children (file-1, file-2); got {:?}",
            entry.tail,
        );
        assert_eq!(
            entry.tail.last().map(|c| c.title.as_str()),
            Some("file-6.rs"),
            "tail ends with the newest child; got {:?}",
            entry.tail,
        );
    }

    /// Auto-clear: when every root in the session is at a terminal
    /// status the view returns empty, mirroring
    /// `clear_workflows_if_all_terminal` so the section disappears.
    #[test]
    fn subagents_view_returns_empty_when_every_root_is_terminal() {
        let mut app = App::test_default();
        let root_a = make_subagent_root_tc(
            "tu-root-a",
            "Explore",
            "first",
            model::ToolCallStatus::Completed,
        );
        let children_a = vec![make_subagent_child_tc("tu-c-a", "Read", "foo.rs")];
        push_subagent_session(&mut app, root_a, children_a);
        let root_b = make_subagent_root_tc(
            "tu-root-b",
            "code-reviewer",
            "second",
            model::ToolCallStatus::Failed,
        );
        push_subagent_session(&mut app, root_b, Vec::new());

        assert!(
            app.subagents_view().is_empty(),
            "every-terminal session must auto-clear the view; got {:?}",
            app.subagents_view(),
        );
    }

    /// The Inspector's section gate only needs a bool, so it uses
    /// `has_active_subagent_root` rather than building the view and
    /// throwing it away. The two must agree on every state that flips
    /// the gate, or the section appears and disappears wrongly.
    #[test]
    fn has_active_subagent_root_matches_subagents_view_emptiness() {
        fn check(label: &str, app: &App) {
            assert_eq!(
                app.has_active_subagent_root(),
                !app.subagents_view().is_empty(),
                "{label}: predicate disagreed with the view it stands in for",
            );
        }

        check("no dispatch", &App::test_default());

        let mut all_terminal = App::test_default();
        push_subagent_session(
            &mut all_terminal,
            make_subagent_root_tc("tu-a", "Explore", "done", model::ToolCallStatus::Completed),
            vec![make_subagent_child_tc("tu-a-c", "Read", "foo.rs")],
        );
        push_subagent_session(
            &mut all_terminal,
            make_subagent_root_tc("tu-b", "code-reviewer", "gone", model::ToolCallStatus::Failed),
            Vec::new(),
        );
        check("every root terminal", &all_terminal);

        // Resumed shape (#808): the root card replays unscoped and a
        // live child frame names it. The two walks are separately
        // implemented, so the new derivation is agreed on exactly here.
        let resumed_with = |child_status: model::ToolCallStatus| {
            let mut app = App::test_default();
            let mut child = make_subagent_child_tc("tu-resumed-c", "Bash", "sleep");
            child.status = child_status;
            app.active_messages_mut().push(ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageBlock::ToolCall(Box::new(make_subagent_root_tc(
                    "tu-resumed",
                    "Explore",
                    "resumed",
                    model::ToolCallStatus::Completed,
                )))],
            ));
            app.register_tool_call_scope(
                "tu-resumed-c".to_owned(),
                ToolCallScope::SubagentChild { parent_tool_use_id: "tu-resumed".to_owned() },
            );
            app.active_messages_mut().push(ChatMessage::new(
                MessageRole::Assistant,
                vec![MessageBlock::ToolCall(Box::new(child))],
            ));
            app
        };
        check(
            "resumed unscoped root, live child",
            &resumed_with(model::ToolCallStatus::InProgress),
        );
        check(
            "resumed unscoped root, settled child",
            &resumed_with(model::ToolCallStatus::Completed),
        );

        let mut mixed = App::test_default();
        push_subagent_session(
            &mut mixed,
            make_subagent_root_tc(
                "tu-done",
                "code-reviewer",
                "done",
                model::ToolCallStatus::Completed,
            ),
            Vec::new(),
        );
        push_subagent_session(
            &mut mixed,
            make_subagent_root_tc(
                "tu-run",
                "Explore",
                "running",
                model::ToolCallStatus::InProgress,
            ),
            Vec::new(),
        );
        check("one root still running", &mixed);

        let mut pending = App::test_default();
        push_subagent_session(
            &mut pending,
            make_subagent_root_tc("tu-pend", "Explore", "queued", model::ToolCallStatus::Pending),
            Vec::new(),
        );
        check("pending root", &pending);

        let mut backgrounded = App::test_default();
        push_subagent_session(
            &mut backgrounded,
            make_subagent_root_tc("tu-bg", "Explore", "bg scan", model::ToolCallStatus::Completed),
            Vec::new(),
        );
        backgrounded.insert_session_task_mapping("task-bg".to_owned(), "tu-bg".to_owned());
        *backgrounded.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-bg".to_owned(),
            task_type: "local_agent".to_owned(),
            description: "bg scan".to_owned(),
        }];
        check("terminal root still alive in the session roster", &backgrounded);

        let mut orphan_child = App::test_default();
        orphan_child.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(make_subagent_child_tc(
                "tu-orphan",
                "Read",
                "x.rs",
            )))],
        ));
        check("tool call carrying no registered scope", &orphan_child);
    }

    /// Mixed terminal + in-progress roots: ANY in-progress keeps the
    /// section visible. Returns BOTH roots so the user can see the
    /// completed one's `· N tools` summary next to the running one's
    /// live tail.
    #[test]
    fn subagents_view_keeps_terminal_roots_when_others_still_running() {
        let mut app = App::test_default();
        let done = make_subagent_root_tc(
            "tu-root-done",
            "code-reviewer",
            "review the diff",
            model::ToolCallStatus::Completed,
        );
        let done_children = vec![
            make_subagent_child_tc("tu-c-done-1", "Read", "diff.rs"),
            make_subagent_child_tc("tu-c-done-2", "Grep", "old"),
        ];
        push_subagent_session(&mut app, done, done_children);
        let running = make_subagent_root_tc(
            "tu-root-run",
            "Explore",
            "ongoing",
            model::ToolCallStatus::InProgress,
        );
        push_subagent_session(&mut app, running, Vec::new());

        let view = app.subagents_view();
        assert_eq!(view.len(), 2, "both roots present while one is in-progress; got {view:?}");
        let done_entry = view.iter().find(|e| e.tool_use_id == "tu-root-done").expect("done");
        assert_eq!(done_entry.total_count, 2);
        assert_eq!(done_entry.status, model::ToolCallStatus::Completed);
        assert!(
            done_entry.tail.is_empty(),
            "terminal root carries no live tail (the section renders `· N tools` from total_count instead); got {:?}",
            done_entry.tail,
        );
        let running_entry = view.iter().find(|e| e.tool_use_id == "tu-root-run").expect("run");
        assert!(
            running_entry.tail.is_empty()
                || running_entry.tail.len() <= crate::app::SUBAGENT_TAIL_CAP,
            "in-progress root's tail respects the cap; got {:?}",
            running_entry.tail,
        );
    }

    /// No subagent dispatches in the session -> empty view (section
    /// stays hidden).
    #[test]
    fn subagents_view_empty_when_no_subagent_dispatch() {
        let app = App::test_default();
        assert!(app.subagents_view().is_empty());
    }

    /// Regression: a subagent the CLI backgrounds gets an immediate
    /// sentinel tool_result that flips its root card to terminal while
    /// the subagent keeps running. Liveness comes from the session roster
    /// (`background_tasks` intersected with the session task map), so the
    /// section must stay visible for the task's true lifetime even though
    /// the card status reads
    /// terminal - mirroring the PROCESSES section.
    #[test]
    fn subagents_view_keeps_backgrounded_root_alive_via_session_roster() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-bg",
            "Explore",
            "long-running background scan",
            model::ToolCallStatus::Completed,
        );
        push_subagent_session(&mut app, root, Vec::new());
        // task_started recorded the session-scoped mapping and the CLI
        // registry lists it as live; no terminal task_updated has drained
        // it yet.
        app.insert_session_task_mapping("task-bg".to_owned(), "tu-root-bg".to_owned());
        *app.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-bg".to_owned(),
            task_type: "local_agent".to_owned(),
            description: "long-running background scan".to_owned(),
        }];

        let view = app.subagents_view();
        assert_eq!(
            view.len(),
            1,
            "a backgrounded-but-alive subagent stays in the SUBAGENTS view; got {view:?}",
        );
        assert_eq!(view[0].tool_use_id, "tu-root-bg");
    }

    /// Companion to the keeps-alive test: a backgrounded root whose
    /// sentinel status reads terminal but that is still live in the
    /// session roster must render as *running* - InProgress status
    /// (spinner, no `· N tools` summary) AND its live tool tail
    /// preserved. Deriving the row from `root.status` alone would mark a
    /// still-working task done and drop its tail.
    #[test]
    fn subagents_view_backgrounded_alive_root_shows_running_with_tail() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-bg2",
            "Explore",
            "long-running background scan",
            model::ToolCallStatus::Completed,
        );
        // More children than the cap so this also exercises the tail cap
        // on the alive-via-registry path (the existing cap test drives an
        // InProgress-status root instead).
        let child_count = SUBAGENT_TAIL_CAP + 2;
        let mut children = Vec::new();
        for i in 1..=child_count {
            children.push(make_subagent_child_tc(
                &format!("tu-bg-c{i}"),
                "Read",
                &format!("bg-file-{i}.rs"),
            ));
        }
        push_subagent_session(&mut app, root, children);
        app.insert_session_task_mapping("task-bg2".to_owned(), "tu-root-bg2".to_owned());
        *app.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-bg2".to_owned(),
            task_type: "local_agent".to_owned(),
            description: "long-running background scan".to_owned(),
        }];

        let view = app.subagents_view();
        assert_eq!(view.len(), 1, "alive backgrounded root stays; got {view:?}");
        assert_eq!(
            view[0].status,
            model::ToolCallStatus::InProgress,
            "alive backgrounded root must render running, not its sentinel-terminal status; got {:?}",
            view[0].status,
        );
        assert_eq!(
            view[0].total_count, child_count,
            "total_count counts every child; got {}",
            view[0].total_count,
        );
        assert_eq!(
            view[0].tail.len(),
            SUBAGENT_TAIL_CAP,
            "alive backgrounded root keeps its live tail, capped at SUBAGENT_TAIL_CAP; got {:?}",
            view[0].tail,
        );
    }

    /// Regression (unify-activity): a backgrounded AGENT that outlives its
    /// spawning turn. The sentinel flips the root terminal and turn
    /// finalisation wipes the turn-scoped alive set, so the turn-scoped
    /// path drops it - and an agent has no OS process to fall back to. The
    /// session-scoped `background_tasks` registry (agent kind, resolved via
    /// the session-scoped task map) must keep it in SUBAGENTS with its
    /// tail, mirroring how WORKFLOWS survives across turns.
    #[test]
    fn subagents_view_keeps_backgrounded_agent_alive_via_registry_after_turn_reset() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-bg-agent",
            "Explore",
            "long-running background agent",
            model::ToolCallStatus::Completed,
        );
        let child = make_subagent_child_tc("tu-bg-agent-c1", "Read", "conv-row.tsx");
        push_subagent_session(&mut app, root, vec![child]);
        // task_started recorded the session-scoped mapping (survives reset).
        app.insert_session_task_mapping("task-bg-agent".to_owned(), "tu-root-bg-agent".to_owned());
        // The CLI registry still lists it as a live backgrounded agent.
        *app.background_tasks_mut() = vec![crate::app::state::types::BackgroundTask {
            task_id: "task-bg-agent".to_owned(),
            task_type: "local_agent".to_owned(),
            description: "long-running background agent".to_owned(),
        }];
        // Turn finalisation wiped the turn-scoped liveness.
        let _: () = app.with_turn_state_mut(|ts| {
            ts.task_tool_use_ids.clear();
        });

        let view = app.subagents_view();
        assert_eq!(
            view.len(),
            1,
            "a backgrounded agent still in the registry survives turn reset; got {view:?}",
        );
        assert_eq!(view[0].tool_use_id, "tu-root-bg-agent");
        assert_eq!(
            view[0].status,
            model::ToolCallStatus::InProgress,
            "registry-alive backgrounded agent renders running; got {:?}",
            view[0].status,
        );
        assert_eq!(
            view[0].tail.len(),
            1,
            "its live tool tail is preserved; got {:?}",
            view[0].tail
        );
    }

    /// Locks the intersection design: the session map alone must NOT keep a
    /// root alive - the `background_tasks` registry is the authoritative
    /// gate. A terminal-status root with a session-map entry but an EMPTY
    /// registry (and wiped turn state) auto-clears. Guards against a future
    /// refactor dropping the registry gate (which would resurrect stale
    /// leaked map entries as phantom live rows).
    #[test]
    fn subagents_view_session_map_without_registry_does_not_keep_root_alive() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-gate",
            "Explore",
            "finished agent",
            model::ToolCallStatus::Completed,
        );
        push_subagent_session(&mut app, root, Vec::new());
        // Map entry present (e.g. a leaked mapping), but the registry is
        // empty and the turn-scoped liveness is wiped.
        app.insert_session_task_mapping("task-gate".to_owned(), "tu-root-gate".to_owned());
        let _: () = app.with_turn_state_mut(|ts| {
            ts.task_tool_use_ids.clear();
        });

        assert!(
            app.subagents_view().is_empty(),
            "a session-map entry alone (no registry gate) must not keep a terminal root alive; got {:?}",
            app.subagents_view(),
        );
    }

    /// A freshly-dispatched root sits at `Pending` (queued `○`) until the
    /// CLI reports progress. The liveness promotion is only for a
    /// backgrounded root whose sentinel flipped it terminal - it must NOT
    /// fire for a not-yet-started `Pending` root just because that root
    /// counts as active for the section gate.
    #[test]
    fn subagents_view_pending_root_stays_pending() {
        let mut app = App::test_default();
        let root = make_subagent_root_tc(
            "tu-root-pending",
            "Explore",
            "queued scan",
            model::ToolCallStatus::Pending,
        );
        push_subagent_session(&mut app, root, Vec::new());

        let view = app.subagents_view();
        assert_eq!(view.len(), 1, "a pending root still shows in the section; got {view:?}");
        assert_eq!(
            view[0].status,
            model::ToolCallStatus::Pending,
            "a not-yet-started root stays Pending (queued), not forced to the running spinner; got {:?}",
            view[0].status,
        );
    }
}
