use crate::agent::model;
use std::cmp::Ordering;
use std::collections::HashSet;
use std::mem::{size_of, size_of_val};

use super::LayoutInvalidation as InvalidationLevel;
use super::LayoutRemeasureReason;
use super::messages::{
    ChatMessage, IncrementalMarkdown, MessageBlock, MessageRole, NoticeDedupKey, TextBlock,
    WelcomeBlock,
};
use super::tool_call_info::ToolCallInfo;
use super::types::HistoryRetentionStats;

const HISTORY_HIDDEN_MARKER_PREFIX: &str = "Older messages hidden to keep memory bounded";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct HistoryDropCandidate {
    pub(super) msg_idx: usize,
    pub(super) bytes: usize,
}

impl super::App {
    fn remap_anchor_for_insert(
        anchor: Option<(usize, usize)>,
        insert_idx: usize,
    ) -> Option<(usize, usize)> {
        anchor.map(|(anchor_idx, anchor_offset)| {
            let next_idx =
                if anchor_idx >= insert_idx { anchor_idx.saturating_add(1) } else { anchor_idx };
            (next_idx, anchor_offset)
        })
    }

    fn remap_anchor_for_remove(
        anchor: Option<(usize, usize)>,
        removed_idx: usize,
        retained_len: usize,
    ) -> Option<(usize, usize)> {
        let (anchor_idx, anchor_offset) = anchor?;
        if retained_len == 0 {
            return None;
        }

        let next_idx = match anchor_idx.cmp(&removed_idx) {
            Ordering::Less => anchor_idx,
            Ordering::Greater => anchor_idx.saturating_sub(1),
            Ordering::Equal => removed_idx.min(retained_len.saturating_sub(1)),
        };
        Some((next_idx.min(retained_len.saturating_sub(1)), anchor_offset))
    }

    fn invalidate_tail_transition(
        &mut self,
        previous_tail_after_mutation: Option<usize>,
        new_tail: Option<usize>,
    ) {
        if let Some(idx) = previous_tail_after_mutation {
            self.active_viewport_mut().invalidate_message(idx);
        }
        if let Some(idx) = new_tail
            && Some(idx) != previous_tail_after_mutation
        {
            self.active_viewport_mut().invalidate_message(idx);
        }
    }

    fn sync_after_message_topology_change(&mut self, start_idx: usize) {
        self.rebuild_tool_indices_and_terminal_refs();
        if self.messages().is_empty() {
            self.active_viewport_mut().sync_message_count(0);
            return;
        }
        self.invalidate_layout(InvalidationLevel::MessagesFrom(start_idx));
    }

    pub(super) fn is_history_hidden_marker_message(msg: &ChatMessage) -> bool {
        if !matches!(msg.role, MessageRole::System(_)) {
            return false;
        }
        let Some(MessageBlock::Text(block)) = msg.blocks.first() else {
            return false;
        };
        block.text.starts_with(HISTORY_HIDDEN_MARKER_PREFIX)
    }

    pub(super) fn is_history_protected_message(msg: &ChatMessage) -> bool {
        if matches!(msg.role, MessageRole::Welcome) {
            return true;
        }
        msg.blocks.iter().any(|block| {
            if let MessageBlock::ToolCall(tc) = block {
                matches!(
                    tc.status,
                    model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress
                )
            } else {
                false
            }
        })
    }

    fn measure_tool_content_bytes(content: &model::ToolCallContent) -> usize {
        match content {
            model::ToolCallContent::Content(inner) => match &inner.content {
                model::ContentBlock::Text(text) => text.text.capacity(),
                model::ContentBlock::Image(image) => {
                    image.data.capacity().saturating_add(image.mime_type.capacity())
                }
            },
            model::ToolCallContent::Diff(diff) => diff
                .path
                .capacity()
                .saturating_add(diff.old_text.as_ref().map_or(0, String::capacity))
                .saturating_add(diff.new_text.capacity()),
            model::ToolCallContent::McpResource(resource) => resource
                .uri
                .capacity()
                .saturating_add(resource.mime_type.as_ref().map_or(0, String::capacity))
                .saturating_add(resource.text.as_ref().map_or(0, String::capacity))
                .saturating_add(
                    resource.blob_saved_to.as_ref().map_or(0, std::path::PathBuf::capacity),
                ),
            model::ToolCallContent::Terminal(term) => term.terminal_id.capacity(),
        }
    }

    fn measure_tool_call_bytes(tc: &ToolCallInfo) -> usize {
        let mut total = size_of::<ToolCallInfo>()
            .saturating_add(tc.id.capacity())
            .saturating_add(tc.title.capacity())
            .saturating_add(tc.sdk_tool_name.capacity())
            .saturating_add(tc.terminal_id.as_ref().map_or(0, String::capacity))
            .saturating_add(tc.terminal_command.as_ref().map_or(0, String::capacity))
            .saturating_add(tc.terminal_output.as_ref().map_or(0, String::capacity))
            .saturating_add(
                tc.content.capacity().saturating_mul(size_of::<model::ToolCallContent>()),
            );

        total = total.saturating_add(tc.raw_input_bytes);
        // The Monitor tail is the analogue of `terminal_output` above:
        // owned strings that grow as the watched command emits.
        total = total
            .saturating_add(tc.monitor_output_tail.capacity().saturating_mul(size_of::<String>()));
        for line in &tc.monitor_output_tail {
            total = total.saturating_add(line.capacity());
        }
        for content in &tc.content {
            total = total.saturating_add(Self::measure_tool_content_bytes(content));
        }

        total
    }

    /// Measure the approximate in-memory byte footprint of a single message.
    ///
    /// Uses `String::capacity()` and `std::mem::size_of` for actual heap
    /// allocation sizes rather than content-length heuristics.
    pub fn measure_message_bytes(msg: &ChatMessage) -> usize {
        let mut total = size_of::<ChatMessage>()
            .saturating_add(msg.blocks.capacity().saturating_mul(size_of::<MessageBlock>()));
        for block in &msg.blocks {
            match block {
                MessageBlock::Text(block) => {
                    total = total
                        .saturating_add(block.text.capacity())
                        .saturating_add(block.markdown.text_capacity());
                }
                MessageBlock::Notice(block) => {
                    total = total
                        .saturating_add(size_of_val(block))
                        .saturating_add(block.text.text.capacity())
                        .saturating_add(block.text.markdown.text_capacity());
                    if let Some(dedup_key) = &block.dedup_key {
                        total = total.saturating_add(size_of_val(dedup_key));
                        total = total.saturating_add(match dedup_key {
                            NoticeDedupKey::RateLimit(incident) => {
                                incident.rate_limit_type.as_ref().map_or(0, String::capacity)
                            }
                            NoticeDedupKey::ApiRetry => 0,
                        });
                    }
                }
                MessageBlock::ToolCall(tc) => {
                    total = total.saturating_add(Self::measure_tool_call_bytes(tc));
                }
                MessageBlock::Welcome(welcome) => {
                    total = total
                        .saturating_add(size_of::<WelcomeBlock>())
                        .saturating_add(welcome.version.capacity())
                        .saturating_add(welcome.subscription.capacity())
                        .saturating_add(welcome.cwd.capacity())
                        .saturating_add(welcome.session_id.capacity());
                }
                MessageBlock::ImageAttachment(_) => {
                    total =
                        total.saturating_add(size_of::<super::messages::ImageAttachmentBlock>());
                }
            }
        }
        total
    }

    /// Measure the total in-memory byte footprint of all retained messages.
    pub fn measure_history_bytes(&self) -> usize {
        self.messages().iter().map(Self::measure_message_bytes).sum()
    }

    pub(crate) fn rebuild_history_retention_accounting(&mut self) {
        let len = self.messages().len();
        self.message_retained_bytes_mut().clear();
        self.message_retained_bytes_mut().reserve(len);
        let mut total: usize = 0;
        let mut bytes_per_msg: Vec<usize> = Vec::with_capacity(len);
        for msg in self.messages() {
            let bytes = Self::measure_message_bytes(msg);
            bytes_per_msg.push(bytes);
            total = total.saturating_add(bytes);
        }
        for bytes in bytes_per_msg {
            self.message_retained_bytes_mut().push(bytes);
        }
        *self.retained_history_bytes_mut() = total;
    }

    pub(crate) fn ensure_history_retention_accounting(&mut self) {
        if self.message_retained_bytes().len() != self.messages().len() {
            self.rebuild_history_retention_accounting();
        }
    }

    pub(crate) fn push_message_tracked(&mut self, msg: ChatMessage) {
        let previous_tail = self.messages().len().checked_sub(1);
        let bytes = Self::measure_message_bytes(&msg);
        self.active_messages_mut().push(msg);
        self.message_retained_bytes_mut().push(bytes);
        let updated = self.retained_history_bytes().saturating_add(bytes);
        *self.retained_history_bytes_mut() = updated;
        // Defer render-cache accounting to the lazy guard; rebuilding per
        // append is O(n^2) as a session's history replays on resume.
        self.invalidate_tail_transition(previous_tail, self.messages().len().checked_sub(1));
        self.needs_redraw = true;
    }

    pub(crate) fn insert_message_tracked(&mut self, idx: usize, msg: ChatMessage) {
        self.ensure_history_retention_accounting();
        let insert_idx = idx.min(self.messages().len());
        let appended_at_tail = insert_idx == self.messages().len();
        if !appended_at_tail {
            self.shift_active_turn_assistant_for_insert(insert_idx);
            self.shift_turn_notice_refs_for_insert(insert_idx);
            self.shift_stop_hook_summary_for_insert(insert_idx);
        }
        let bytes = Self::measure_message_bytes(&msg);
        self.active_messages_mut().insert(insert_idx, msg);
        self.message_retained_bytes_mut().insert(insert_idx, bytes);
        let updated = self.retained_history_bytes().saturating_add(bytes);
        *self.retained_history_bytes_mut() = updated;
        if appended_at_tail {
            let new_tail = self.messages().len().checked_sub(1);
            self.invalidate_tail_transition(
                new_tail.and_then(|tail| tail.checked_sub(1)),
                new_tail,
            );
        } else {
            self.rebuild_render_cache_accounting();
            self.sync_after_message_topology_change(insert_idx);
        }
        self.needs_redraw = true;
    }

    /// Remove the message at `idx` from the active session and keep
    /// the cross-cutting indices consistent: `active_turn_assistant_idx`
    /// is shifted via `shift_active_turn_assistant_for_remove`, and
    /// every `turn_notice_ref` is fed through
    /// `shift_turn_notice_refs_for_remove` which drops any ref whose
    /// `msg_idx` EQUALS `idx` (the `Ordering::Equal` arm).
    ///
    /// Callers MUST NOT subsequently call
    /// `turn_notice_refs_mut().remove(...)` for a ref that pointed at
    /// the removed message - it is already gone, and an explicit
    /// remove will either panic on the emptied Vec or corrupt a
    /// sibling ref.
    pub(crate) fn remove_message_tracked(&mut self, idx: usize) -> Option<ChatMessage> {
        self.ensure_history_retention_accounting();
        let old_len = self.messages().len();
        if idx >= old_len {
            return None;
        }
        let removed_tail = idx + 1 == old_len;
        self.shift_active_turn_assistant_for_remove(idx);
        self.shift_turn_notice_refs_for_remove(idx);
        self.shift_stop_hook_summary_for_remove(idx);
        let removed = self.active_messages_mut().remove(idx);
        let removed_bytes = self.message_retained_bytes_mut().remove(idx);
        let updated = self.retained_history_bytes().saturating_sub(removed_bytes);
        *self.retained_history_bytes_mut() = updated;
        self.rebuild_render_cache_accounting();
        self.rebuild_tool_indices_and_terminal_refs();
        if removed_tail {
            self.invalidate_tail_transition(None, self.messages().len().checked_sub(1));
        } else if !self.messages().is_empty() {
            self.invalidate_layout(InvalidationLevel::MessagesFrom(idx));
        } else {
            self.active_viewport_mut().sync_message_count(0);
        }
        self.needs_redraw = true;
        Some(removed)
    }

    pub(crate) fn clear_messages_tracked(&mut self) {
        self.active_messages_mut().clear();
        self.message_retained_bytes_mut().clear();
        *self.retained_history_bytes_mut() = 0;
        self.clear_active_turn_assistant();
        self.clear_turn_notice_refs();
        self.set_last_stop_hook_summary(None);
        self.rebuild_render_cache_accounting();
        self.rebuild_tool_indices_and_terminal_refs();
        self.active_viewport_mut().sync_message_count(0);
        self.needs_redraw = true;
    }

    pub(crate) fn recompute_message_retained_bytes(&mut self, idx: usize) {
        self.ensure_history_retention_accounting();
        let Some(msg) = self.messages().get(idx) else {
            return;
        };
        let new_bytes = Self::measure_message_bytes(msg);
        let Some(old_bytes_value) = self.message_retained_bytes().get(idx).copied() else {
            self.rebuild_history_retention_accounting();
            return;
        };
        if let Some(slot) = self.message_retained_bytes_mut().get_mut(idx) {
            *slot = new_bytes;
        }
        let updated =
            self.retained_history_bytes().saturating_sub(old_bytes_value).saturating_add(new_bytes);
        *self.retained_history_bytes_mut() = updated;
    }

    pub(super) fn rebuild_tool_indices_and_terminal_refs(&mut self) {
        self.active_tool_call_index_mut().clear();
        self.clear_terminal_tool_call_tracking();
        self.active_task_ids_mut().clear();

        let mut terminal_tool_call_membership = HashSet::new();
        let mut terminal_tool_calls = Vec::new();
        let mut new_tool_call_index: std::collections::HashMap<String, (usize, usize)> =
            std::collections::HashMap::new();
        for (msg_idx, msg) in self.active_messages_mut().iter_mut().enumerate() {
            for (block_idx, block) in msg.blocks.iter_mut().enumerate() {
                if let MessageBlock::ToolCall(tc) = block {
                    let tc = tc.as_mut();
                    new_tool_call_index.insert(tc.id.clone(), (msg_idx, block_idx));
                    if let Some(terminal_id) = Self::tracked_terminal_id_for_tool(tc) {
                        let entry =
                            super::TerminalToolCallRef::new(terminal_id, msg_idx, block_idx);
                        if terminal_tool_call_membership.insert(entry.clone()) {
                            terminal_tool_calls.push(entry);
                        }
                    }
                }
            }
        }
        *self.active_tool_call_index_mut() = new_tool_call_index;
        *self.terminal_tool_calls_mut() = terminal_tool_calls;
        *self.terminal_tool_call_membership_mut() = terminal_tool_call_membership;
        let live_ids: HashSet<String> = self.tool_call_index().keys().cloned().collect();
        self.tool_call_scopes_mut().retain(|id, _| live_ids.contains(id));
        self.subagent_attribution_mut().retain(|id, _| live_ids.contains(id));
        let scopes_snapshot: std::collections::HashMap<String, super::ToolCallScope> =
            self.tool_call_scopes().clone();
        let mut new_active_task_ids: Vec<String> = Vec::new();
        for msg in self.messages() {
            for block in &msg.blocks {
                let MessageBlock::ToolCall(tc) = block else {
                    continue;
                };
                if !matches!(
                    tc.status,
                    model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress
                ) {
                    continue;
                }
                match scopes_snapshot.get(&tc.id) {
                    Some(super::ToolCallScope::SubagentRoot) => {
                        new_active_task_ids.push(tc.id.clone());
                    }
                    Some(
                        super::ToolCallScope::SubagentChild { .. }
                        | super::ToolCallScope::MainAgent,
                    )
                    | None => {}
                }
            }
        }
        for id in new_active_task_ids {
            self.active_task_ids_mut().insert(id);
        }

        self.normalize_focus_stack();
    }

    fn format_mib_tenths(bytes: usize) -> String {
        let tenths =
            (u128::try_from(bytes).unwrap_or(u128::MAX).saturating_mul(10) + 524_288) / 1_048_576;
        format!("{}.{}", tenths / 10, tenths % 10)
    }

    fn history_hidden_marker_text(
        total_dropped_messages: usize,
        total_dropped_bytes: usize,
    ) -> String {
        format!(
            "{HISTORY_HIDDEN_MARKER_PREFIX} (dropped {total_dropped_messages} messages, {} MiB).",
            Self::format_mib_tenths(total_dropped_bytes)
        )
    }

    fn upsert_history_hidden_marker(
        &mut self,
        preserved_anchor: Option<(usize, usize)>,
    ) -> Option<(usize, usize)> {
        self.ensure_history_retention_accounting();
        let marker_idx = self.messages().iter().position(Self::is_history_hidden_marker_message);
        if self.history_retention_stats().total_dropped_messages == 0 {
            if let Some(idx) = marker_idx {
                self.remove_message_tracked(idx);
                return Self::remap_anchor_for_remove(preserved_anchor, idx, self.messages().len());
            }
            return preserved_anchor;
        }

        let marker_text = Self::history_hidden_marker_text(
            self.history_retention_stats().total_dropped_messages,
            self.history_retention_stats().total_dropped_bytes,
        );

        if let Some(idx) = marker_idx {
            if let Some(MessageBlock::Text(block)) =
                self.active_messages_mut().get_mut(idx).and_then(|m| m.blocks.get_mut(0))
                && block.text != marker_text
            {
                block.text.clone_from(&marker_text);
                block.markdown = IncrementalMarkdown::from_complete(&marker_text);
                block.cache.invalidate();
                self.sync_render_cache_slot(idx, 0);
                self.recompute_message_retained_bytes(idx);
                self.invalidate_layout(InvalidationLevel::MessagesFrom(idx));
            }
            return preserved_anchor;
        }

        let insert_idx = usize::from(
            self.messages().first().is_some_and(|msg| matches!(msg.role, MessageRole::Welcome)),
        );
        self.insert_message_tracked(
            insert_idx,
            ChatMessage::new(
                MessageRole::System(None),
                vec![MessageBlock::Text(TextBlock::from_complete(&marker_text))],
            ),
        );
        Self::remap_anchor_for_insert(preserved_anchor, insert_idx)
    }

    pub fn enforce_history_retention(&mut self) -> HistoryRetentionStats {
        self.ensure_history_retention_accounting();
        let mut stats = HistoryRetentionStats::default();
        let max_bytes = self.history_retention().max_bytes.max(1);
        let active_turn_owner = self.active_turn_assistant_idx();
        let mut preserved_anchor = self.active_viewport_mut().capture_manual_scroll_anchor();
        stats.total_before_bytes = self.retained_history_bytes();
        stats.total_after_bytes = stats.total_before_bytes;

        if stats.total_before_bytes > max_bytes {
            let mut candidates = Vec::new();
            for (msg_idx, msg) in self.messages().iter().enumerate() {
                if Self::is_history_hidden_marker_message(msg)
                    || Self::is_history_protected_message(msg)
                    || active_turn_owner == Some(msg_idx)
                {
                    continue;
                }
                let bytes = self.message_retained_bytes().get(msg_idx).copied().unwrap_or(0);
                if bytes == 0 {
                    continue;
                }
                candidates.push(HistoryDropCandidate { msg_idx, bytes });
            }

            let mut drop_candidates = Vec::new();
            for candidate in candidates {
                if stats.total_after_bytes <= max_bytes {
                    break;
                }
                stats.total_after_bytes = stats.total_after_bytes.saturating_sub(candidate.bytes);
                stats.dropped_bytes = stats.dropped_bytes.saturating_add(candidate.bytes);
                stats.dropped_messages = stats.dropped_messages.saturating_add(1);
                drop_candidates.push(candidate);
            }

            if !drop_candidates.is_empty() {
                preserved_anchor = self.apply_history_retention_drop(
                    &drop_candidates,
                    active_turn_owner,
                    preserved_anchor,
                );
                self.rebuild_tool_indices_and_terminal_refs();
                let msg_count = self.messages().len();
                {
                    let vp = self.active_viewport_mut();
                    vp.sync_message_count(msg_count);
                    if let Some((anchor_idx, anchor_offset)) = preserved_anchor {
                        vp.preserve_scroll_anchor(
                            LayoutRemeasureReason::MessagesFrom,
                            anchor_idx,
                            anchor_offset,
                        );
                    }
                }
                self.invalidate_layout(InvalidationLevel::MessagesFrom(0));
                self.needs_redraw = true;
            }
        }

        {
            let h_stats = self.history_retention_stats_mut();
            h_stats.total_before_bytes = stats.total_before_bytes;
            h_stats.total_dropped_messages =
                h_stats.total_dropped_messages.saturating_add(stats.dropped_messages);
            h_stats.total_dropped_bytes =
                h_stats.total_dropped_bytes.saturating_add(stats.dropped_bytes);
        }

        preserved_anchor = self.upsert_history_hidden_marker(preserved_anchor);
        let msg_count = self.messages().len();
        {
            let vp = self.active_viewport_mut();
            vp.sync_message_count(msg_count);
            if let Some((anchor_idx, anchor_offset)) = preserved_anchor {
                vp.preserve_scroll_anchor(
                    LayoutRemeasureReason::MessagesFrom,
                    anchor_idx,
                    anchor_offset,
                );
            }
        }

        stats.total_after_bytes = self.retained_history_bytes();
        {
            let h_stats = self.history_retention_stats_mut();
            h_stats.total_after_bytes = stats.total_after_bytes;
            h_stats.dropped_messages = stats.dropped_messages;
            h_stats.dropped_bytes = stats.dropped_bytes;
        }

        stats.total_dropped_messages = self.history_retention_stats().total_dropped_messages;
        stats.total_dropped_bytes = self.history_retention_stats().total_dropped_bytes;

        crate::perf::mark_with("history::bytes_before", "bytes", stats.total_before_bytes);
        crate::perf::mark_with("history::bytes_after", "bytes", stats.total_after_bytes);
        crate::perf::mark_with("history::dropped_messages", "count", stats.dropped_messages);
        crate::perf::mark_with("history::dropped_bytes", "bytes", stats.dropped_bytes);
        crate::perf::mark_with("history::total_dropped", "count", stats.total_dropped_messages);

        stats
    }

    fn apply_history_retention_drop(
        &mut self,
        drop_candidates: &[HistoryDropCandidate],
        active_turn_owner: Option<usize>,
        preserved_anchor: Option<(usize, usize)>,
    ) -> Option<(usize, usize)> {
        let drop_set: HashSet<usize> =
            drop_candidates.iter().map(|candidate| candidate.msg_idx).collect();

        let mut retained = Vec::with_capacity(self.messages().len().saturating_sub(drop_set.len()));
        let mut retained_bytes = Vec::with_capacity(retained.capacity());
        let old_messages = std::mem::take(self.active_messages_mut());
        let old_bytes = std::mem::take(self.message_retained_bytes_mut());
        let mut old_to_new = vec![None; old_messages.len()];
        let mut remapped_active_turn_owner = None;
        let mut total_bytes: usize = 0;
        for (msg_idx, (msg, bytes)) in old_messages.into_iter().zip(old_bytes).enumerate() {
            if !drop_set.contains(&msg_idx) {
                if active_turn_owner == Some(msg_idx) {
                    remapped_active_turn_owner = Some(retained.len());
                }
                old_to_new[msg_idx] = Some(retained.len());
                total_bytes = total_bytes.saturating_add(bytes);
                retained.push(msg);
                retained_bytes.push(bytes);
            }
        }
        *self.active_messages_mut() = retained;
        *self.message_retained_bytes_mut() = retained_bytes;
        *self.retained_history_bytes_mut() = total_bytes;
        self.set_active_turn_assistant_message_idx(remapped_active_turn_owner);
        self.remap_turn_notice_refs_after_message_drop(&old_to_new);
        self.remap_stop_hook_summary_after_message_drop(&old_to_new);

        let (anchor_idx, anchor_offset) = preserved_anchor?;
        if let Some(new_idx) = old_to_new.get(anchor_idx).copied().flatten() {
            return Some((new_idx, anchor_offset));
        }

        let fallback_old_idx = ((anchor_idx + 1)..old_to_new.len())
            .find(|&idx| old_to_new[idx].is_some())
            .or_else(|| (0..anchor_idx).rev().find(|&idx| old_to_new[idx].is_some()))?;
        old_to_new[fallback_old_idx].map(|new_idx| (new_idx, 0))
    }
}
