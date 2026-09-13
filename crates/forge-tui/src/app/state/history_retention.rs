use crate::agent::model;
use std::cmp::Ordering;
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

/// Not a reachable message index, so it marks the tool-index entries a
/// rebuild walk did not reach.
const UNVISITED_MSG_IDX: usize = usize::MAX;

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
        let Some(viewport) = self.active_viewport_mut() else {
            return;
        };
        if let Some(idx) = previous_tail_after_mutation {
            viewport.invalidate_message(idx);
        }
        if let Some(idx) = new_tail
            && Some(idx) != previous_tail_after_mutation
        {
            viewport.invalidate_message(idx);
        }
    }

    fn sync_after_message_topology_change(&mut self, start_idx: usize) {
        self.rebuild_tool_indices();
        if self.messages().is_none_or(<[ChatMessage]>::is_empty) {
            if let Some(viewport) = self.active_viewport_mut() {
                viewport.sync_message_count(0);
            }
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

    fn measure_tool_content_bytes(content: &model::RenderToolCallContent) -> usize {
        match content {
            model::RenderToolCallContent::Content(inner) => match &inner.content {
                model::RenderContentBlock::Text(text) => text.text.capacity(),
                model::RenderContentBlock::Image(image) => {
                    image.data.capacity().saturating_add(image.mime_type.capacity())
                }
            },
            model::RenderToolCallContent::Diff(diff) => diff
                .path
                .capacity()
                .saturating_add(diff.old_text.as_ref().map_or(0, String::capacity))
                .saturating_add(diff.new_text.capacity()),
            model::RenderToolCallContent::McpResource(resource) => resource
                .uri
                .capacity()
                .saturating_add(resource.mime_type.as_ref().map_or(0, String::capacity))
                .saturating_add(resource.text.as_ref().map_or(0, String::capacity))
                .saturating_add(
                    resource.blob_saved_to.as_ref().map_or(0, std::path::PathBuf::capacity),
                ),
        }
    }

    fn measure_tool_call_bytes(tc: &ToolCallInfo) -> usize {
        let mut total = size_of::<ToolCallInfo>()
            .saturating_add(tc.id.capacity())
            .saturating_add(tc.title.capacity())
            .saturating_add(tc.sdk_tool_name.capacity())
            .saturating_add(tc.terminal_output.as_ref().map_or(0, String::capacity))
            .saturating_add(
                tc.content.capacity().saturating_mul(size_of::<model::RenderToolCallContent>()),
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
        self.messages().unwrap_or_default().iter().map(Self::measure_message_bytes).sum()
    }

    pub(crate) fn rebuild_history_retention_accounting(&mut self) {
        let Some(messages) = self.messages() else {
            return;
        };
        let mut total: usize = 0;
        let mut bytes_per_msg: Vec<usize> = Vec::with_capacity(messages.len());
        for msg in messages {
            let bytes = Self::measure_message_bytes(msg);
            bytes_per_msg.push(bytes);
            total = total.saturating_add(bytes);
        }
        if let Some(store) = self.message_retained_bytes_mut() {
            *store = bytes_per_msg;
        }
        if let Some(bytes) = self.retained_history_bytes_mut() {
            *bytes = total;
        }
    }

    pub(crate) fn ensure_history_retention_accounting(&mut self) {
        if self.message_retained_bytes().map_or(0, <[usize]>::len)
            != self.messages().map_or(0, <[ChatMessage]>::len)
        {
            self.rebuild_history_retention_accounting();
        }
    }

    pub(crate) fn push_message_tracked(&mut self, msg: ChatMessage) {
        let bytes = Self::measure_message_bytes(&msg);
        let previous_tail = self.messages().map_or(0, <[ChatMessage]>::len).checked_sub(1);
        let Some(messages) = self.active_messages_mut() else {
            return;
        };
        messages.push(msg);
        if let Some(store) = self.message_retained_bytes_mut() {
            store.push(bytes);
        }
        if let Some(total) = self.retained_history_bytes_mut() {
            *total = total.saturating_add(bytes);
        }
        // Defer render-cache accounting to the lazy guard; rebuilding per
        // append is O(n^2) as a session's history replays on resume.
        let new_tail = self.messages().map_or(0, <[ChatMessage]>::len).checked_sub(1);
        self.invalidate_tail_transition(previous_tail, new_tail);
        self.needs_redraw = true;
    }

    pub(crate) fn insert_message_tracked(&mut self, idx: usize, msg: ChatMessage) {
        self.ensure_history_retention_accounting();
        let Some(len) = self.messages().map(<[ChatMessage]>::len) else {
            return;
        };
        let insert_idx = idx.min(len);
        let appended_at_tail = insert_idx == len;
        if !appended_at_tail {
            self.shift_active_turn_assistant_for_insert(insert_idx);
            self.shift_turn_notice_refs_for_insert(insert_idx);
            self.shift_stop_hook_summary_for_insert(insert_idx);
        }
        let bytes = Self::measure_message_bytes(&msg);
        if let Some(messages) = self.active_messages_mut() {
            messages.insert(insert_idx, msg);
        }
        if let Some(store) = self.message_retained_bytes_mut() {
            store.insert(insert_idx, bytes);
        }
        if let Some(total) = self.retained_history_bytes_mut() {
            *total = total.saturating_add(bytes);
        }
        if appended_at_tail {
            let new_tail = self.messages().map_or(0, <[ChatMessage]>::len).checked_sub(1);
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
        let old_len = self.messages().map(<[ChatMessage]>::len)?;
        if idx >= old_len {
            return None;
        }
        let removed_tail = idx + 1 == old_len;
        self.shift_active_turn_assistant_for_remove(idx);
        self.shift_turn_notice_refs_for_remove(idx);
        self.shift_stop_hook_summary_for_remove(idx);
        let removed = self.active_messages_mut()?.remove(idx);
        let removed_bytes = self.message_retained_bytes_mut()?.remove(idx);
        if let Some(total) = self.retained_history_bytes_mut() {
            *total = total.saturating_sub(removed_bytes);
        }
        self.rebuild_render_cache_accounting();
        self.rebuild_tool_indices();
        if removed_tail {
            let new_tail = self.messages().map_or(0, <[ChatMessage]>::len).checked_sub(1);
            self.invalidate_tail_transition(None, new_tail);
        } else if self.messages().is_some_and(|messages| !messages.is_empty()) {
            self.invalidate_layout(InvalidationLevel::MessagesFrom(idx));
        } else if let Some(viewport) = self.active_viewport_mut() {
            viewport.sync_message_count(0);
        }
        self.needs_redraw = true;
        Some(removed)
    }

    pub(crate) fn clear_messages_tracked(&mut self) {
        if let Some(messages) = self.active_messages_mut() {
            messages.clear();
        }
        if let Some(store) = self.message_retained_bytes_mut() {
            store.clear();
        }
        if let Some(bytes) = self.retained_history_bytes_mut() {
            *bytes = 0;
        }
        self.clear_active_turn_assistant();
        self.clear_turn_notice_refs();
        self.set_last_stop_hook_summary(None);
        self.rebuild_render_cache_accounting();
        self.rebuild_tool_indices();
        if let Some(viewport) = self.active_viewport_mut() {
            viewport.sync_message_count(0);
        }
        self.needs_redraw = true;
    }

    pub(crate) fn recompute_message_retained_bytes(&mut self, idx: usize) {
        self.ensure_history_retention_accounting();
        let Some(msg) = self.messages().and_then(|messages| messages.get(idx)) else {
            return;
        };
        let new_bytes = Self::measure_message_bytes(msg);
        let Some(old_bytes_value) =
            self.message_retained_bytes().and_then(|bytes| bytes.get(idx)).copied()
        else {
            self.rebuild_history_retention_accounting();
            return;
        };
        if let Some(slot) = self.message_retained_bytes_mut().and_then(|bytes| bytes.get_mut(idx)) {
            *slot = new_bytes;
        }
        if let Some(total) = self.retained_history_bytes_mut() {
            *total = total.saturating_sub(old_bytes_value).saturating_add(new_bytes);
        }
    }

    pub(super) fn rebuild_tool_indices(&mut self) {
        if let Some(active_task_ids) = self.active_task_ids_mut() {
            active_task_ids.clear();
        }

        // Reuse the index's key allocations rather than clearing it: a
        // session at its retention budget runs this per appended message,
        // and clear-then-reinsert frees and re-allocates every tool-call
        // id each time. Positions are still taken from the walk, so a
        // drifted entry is repaired exactly as a from-scratch rebuild
        // repaired it.
        let mut tool_call_index =
            self.active_tool_call_index_mut().map(std::mem::take).unwrap_or_default();
        for slot in tool_call_index.values_mut() {
            slot.0 = UNVISITED_MSG_IDX;
        }
        if let Some(messages) = self.active_messages_mut() {
            for (msg_idx, msg) in messages.iter_mut().enumerate() {
                for (block_idx, block) in msg.blocks.iter_mut().enumerate() {
                    if let MessageBlock::ToolCall(tc) = block {
                        let tc = tc.as_mut();
                        if let Some(slot) = tool_call_index.get_mut(&tc.id) {
                            *slot = (msg_idx, block_idx);
                        } else {
                            tool_call_index.insert(tc.id.clone(), (msg_idx, block_idx));
                        }
                    }
                }
            }
        }
        tool_call_index.retain(|_, slot| slot.0 != UNVISITED_MSG_IDX);
        if let Some(scopes) = self.tool_call_scopes_mut() {
            scopes.retain(|id, _| tool_call_index.contains_key(id));
        }
        if let Some(attribution) = self.subagent_attribution_mut() {
            attribution.retain(|id, _| tool_call_index.contains_key(id));
        }
        if let Some(index) = self.active_tool_call_index_mut() {
            *index = tool_call_index;
        }
        let scopes_snapshot: std::collections::HashMap<String, super::ToolCallScope> =
            self.tool_call_scopes().cloned().unwrap_or_default();
        let mut new_active_task_ids: Vec<String> = Vec::new();
        for msg in self.messages().unwrap_or_default() {
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
        if let Some(active_task_ids) = self.active_task_ids_mut() {
            for id in new_active_task_ids {
                active_task_ids.insert(id);
            }
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
        let marker_idx = self
            .messages()
            .unwrap_or_default()
            .iter()
            .position(Self::is_history_hidden_marker_message);
        if self.history_retention_stats().map_or(0, |stats| stats.total_dropped_messages) == 0 {
            if let Some(idx) = marker_idx {
                self.remove_message_tracked(idx);
                let remaining = self.messages().map_or(0, <[ChatMessage]>::len);
                return Self::remap_anchor_for_remove(preserved_anchor, idx, remaining);
            }
            return preserved_anchor;
        }

        let marker_text = Self::history_hidden_marker_text(
            self.history_retention_stats().map_or(0, |stats| stats.total_dropped_messages),
            self.history_retention_stats().map_or(0, |stats| stats.total_dropped_bytes),
        );

        if let Some(idx) = marker_idx {
            if let Some(MessageBlock::Text(block)) = self
                .active_messages_mut()
                .and_then(|messages| messages.get_mut(idx))
                .and_then(|m| m.blocks.get_mut(0))
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
            self.messages()
                .and_then(|messages| messages.first())
                .is_some_and(|msg| matches!(msg.role, MessageRole::Welcome)),
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
        let max_bytes = self.history_retention().map_or(0, |policy| policy.max_bytes).max(1);
        let active_turn_owner = self.active_turn_assistant_idx();
        let mut preserved_anchor =
            self.active_viewport_mut().and_then(|viewport| viewport.capture_manual_scroll_anchor());
        stats.total_before_bytes = self.retained_history_bytes().unwrap_or(0);
        stats.total_after_bytes = stats.total_before_bytes;

        if stats.total_before_bytes > max_bytes {
            // The tail of a full scan was never consumed.
            let mut drop_indices = Vec::new();
            {
                let messages = self.messages().unwrap_or_default();
                for (msg_idx, msg) in messages.iter().enumerate() {
                    if stats.total_after_bytes <= max_bytes {
                        break;
                    }
                    if Self::is_history_hidden_marker_message(msg)
                        || Self::is_history_protected_message(msg)
                        || active_turn_owner == Some(msg_idx)
                    {
                        continue;
                    }
                    let bytes =
                        self.message_retained_bytes().and_then(|bytes| bytes.get(msg_idx)).copied();
                    let Some(bytes) = bytes else {
                        continue;
                    };
                    if bytes == 0 {
                        continue;
                    }
                    stats.total_after_bytes = stats.total_after_bytes.saturating_sub(bytes);
                    stats.dropped_bytes = stats.dropped_bytes.saturating_add(bytes);
                    stats.dropped_messages = stats.dropped_messages.saturating_add(1);
                    drop_indices.push(msg_idx);
                }
            }

            if !drop_indices.is_empty() {
                preserved_anchor = self.apply_history_retention_drop(
                    &drop_indices,
                    active_turn_owner,
                    preserved_anchor,
                );
                self.rebuild_tool_indices();
                let msg_count = self.messages().map_or(0, <[ChatMessage]>::len);
                if let Some(viewport) = self.active_viewport_mut() {
                    viewport.sync_message_count(msg_count);
                    if let Some((anchor_idx, anchor_offset)) = preserved_anchor {
                        viewport.preserve_scroll_anchor(
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

        if let Some(h_stats) = self.history_retention_stats_mut() {
            h_stats.total_before_bytes = stats.total_before_bytes;
            h_stats.total_dropped_messages =
                h_stats.total_dropped_messages.saturating_add(stats.dropped_messages);
            h_stats.total_dropped_bytes =
                h_stats.total_dropped_bytes.saturating_add(stats.dropped_bytes);
        }

        preserved_anchor = self.upsert_history_hidden_marker(preserved_anchor);
        let msg_count = self.messages().map_or(0, <[ChatMessage]>::len);
        if let Some(viewport) = self.active_viewport_mut() {
            viewport.sync_message_count(msg_count);
            if let Some((anchor_idx, anchor_offset)) = preserved_anchor {
                viewport.preserve_scroll_anchor(
                    LayoutRemeasureReason::MessagesFrom,
                    anchor_idx,
                    anchor_offset,
                );
            }
        }

        stats.total_after_bytes = self.retained_history_bytes().unwrap_or(0);
        if let Some(h_stats) = self.history_retention_stats_mut() {
            h_stats.total_after_bytes = stats.total_after_bytes;
            h_stats.dropped_messages = stats.dropped_messages;
            h_stats.dropped_bytes = stats.dropped_bytes;
        }

        stats.total_dropped_messages =
            self.history_retention_stats().map_or(0, |stats| stats.total_dropped_messages);
        stats.total_dropped_bytes =
            self.history_retention_stats().map_or(0, |stats| stats.total_dropped_bytes);

        crate::perf::mark_with("history::bytes_before", "bytes", stats.total_before_bytes);
        crate::perf::mark_with("history::bytes_after", "bytes", stats.total_after_bytes);
        crate::perf::mark_with("history::dropped_messages", "count", stats.dropped_messages);
        crate::perf::mark_with("history::dropped_bytes", "bytes", stats.dropped_bytes);
        crate::perf::mark_with("history::total_dropped", "count", stats.total_dropped_messages);

        stats
    }

    /// `drop_indices` must be ordered ascending, which is how the retention
    /// scan builds it; the walk below advances through it in step with the
    /// messages instead of hashing every index.
    fn apply_history_retention_drop(
        &mut self,
        drop_indices: &[usize],
        active_turn_owner: Option<usize>,
        preserved_anchor: Option<(usize, usize)>,
    ) -> Option<(usize, usize)> {
        debug_assert!(
            drop_indices.is_sorted_by(|left, right| left < right),
            "drop_indices must be strictly ascending or the cursor walk keeps the wrong messages",
        );
        let retained_capacity =
            self.messages().map_or(0, <[ChatMessage]>::len).saturating_sub(drop_indices.len());
        let mut retained = Vec::with_capacity(retained_capacity);
        let mut retained_bytes = Vec::with_capacity(retained.capacity());
        let Some(old_messages) = self.active_messages_mut().map(std::mem::take) else {
            return preserved_anchor;
        };
        let Some(old_bytes) = self.message_retained_bytes_mut().map(std::mem::take) else {
            if let Some(messages) = self.active_messages_mut() {
                *messages = old_messages;
            }
            return preserved_anchor;
        };
        let mut old_to_new = vec![None; old_messages.len()];
        let mut remapped_active_turn_owner = None;
        let mut total_bytes: usize = 0;
        let mut next_drop = drop_indices.iter().peekable();
        for (msg_idx, (msg, bytes)) in old_messages.into_iter().zip(old_bytes).enumerate() {
            if next_drop.next_if(|&&dropped| dropped == msg_idx).is_some() {
                continue;
            }
            if active_turn_owner == Some(msg_idx) {
                remapped_active_turn_owner = Some(retained.len());
            }
            old_to_new[msg_idx] = Some(retained.len());
            total_bytes = total_bytes.saturating_add(bytes);
            retained.push(msg);
            retained_bytes.push(bytes);
        }
        if let Some(messages) = self.active_messages_mut() {
            *messages = retained;
        }
        if let Some(bytes) = self.message_retained_bytes_mut() {
            *bytes = retained_bytes;
        }
        if let Some(total) = self.retained_history_bytes_mut() {
            *total = total_bytes;
        }
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

#[cfg(test)]
mod tests {
    use super::super::{App, AppStatus, ChatMessage, MessageBlock, MessageRole, ToolCallScope};
    use crate::agent::model;
    use crate::app::state::tests::{
        assistant_bash_tool_message, assistant_text_block, assistant_tool_message, make_test_app,
        user_text_message,
    };
    use pretty_assertions::assert_eq;

    #[test]
    fn enforce_history_retention_noop_under_budget() {
        let mut app = make_test_app();
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("small message"),
            user_text_message("another message"),
        ];
        app.history_retention_mut().expect("active session").max_bytes = usize::MAX / 4;

        let stats = app.enforce_history_retention();
        assert_eq!(stats.dropped_messages, 0);
        assert_eq!(stats.total_dropped_messages, 0);
        assert!(
            !app.messages()
                .expect("active session")
                .iter()
                .any(App::is_history_hidden_marker_message)
        );
    }

    #[test]
    fn enforce_history_retention_drops_oldest_and_adds_marker() {
        let mut app = make_test_app();
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("first old message"),
            user_text_message("second old message"),
            user_text_message("third old message"),
        ];
        app.history_retention_mut().expect("active session").max_bytes = 1;

        let stats = app.enforce_history_retention();
        assert_eq!(stats.dropped_messages, 3);
        assert!(matches!(app.messages().expect("active session")[0].role, MessageRole::Welcome));
        assert!(
            app.messages()
                .expect("active session")
                .iter()
                .any(App::is_history_hidden_marker_message)
        );
        assert_eq!(app.messages().expect("active session").len(), 2);
    }

    #[test]
    fn enforce_history_retention_drops_tool_index_entries_for_dropped_messages() {
        let mut app = make_test_app();
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            assistant_tool_message("tool-dropped", model::ToolCallStatus::Completed),
            assistant_tool_message("tool-kept", model::ToolCallStatus::InProgress),
        ];
        app.index_tool_call("tool-dropped".to_owned(), 1, 0);
        app.index_tool_call("tool-kept".to_owned(), 2, 0);
        app.history_retention_mut().expect("active session").max_bytes = 1;

        let stats = app.enforce_history_retention();

        assert_eq!(stats.dropped_messages, 1);
        assert_eq!(
            app.lookup_tool_call("tool-dropped"),
            None,
            "a tool call whose message was trimmed leaves no index entry behind",
        );
        assert_eq!(app.lookup_tool_call("tool-kept"), Some((2, 0)));
    }

    #[test]
    fn enforce_history_retention_preserves_in_progress_tool_message() {
        let mut app = make_test_app();
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("droppable"),
            assistant_tool_message("tool-keep", model::ToolCallStatus::InProgress),
        ];
        app.history_retention_mut().expect("active session").max_bytes = 1;

        let stats = app.enforce_history_retention();
        assert_eq!(stats.dropped_messages, 1);
        assert!(app.messages().expect("active session").iter().any(|msg| {
            msg.blocks.iter().any(|block| {
                matches!(
                    block,
                    MessageBlock::ToolCall(tc) if tc.id == "tool-keep"
                        && matches!(tc.status, model::ToolCallStatus::InProgress)
                )
            })
        }));
    }

    #[test]
    fn enforce_history_retention_preserves_pending_tool_message() {
        let mut app = make_test_app();
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("droppable"),
            assistant_tool_message("tool-pending", model::ToolCallStatus::Pending),
        ];
        app.history_retention_mut().expect("active session").max_bytes = 1;

        let stats = app.enforce_history_retention();
        assert_eq!(stats.dropped_messages, 1);
        assert!(app.messages().expect("active session").iter().any(|msg| {
            msg.blocks
                .iter()
                .any(|block| matches!(block, MessageBlock::ToolCall(tc) if tc.id == "tool-pending"))
        }));
    }

    #[test]
    fn enforce_history_retention_rebuilds_tool_index_after_prune() {
        let mut app = make_test_app();
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("drop this"),
            assistant_bash_tool_message("tool-idx", model::ToolCallStatus::InProgress),
        ];
        app.index_tool_call("tool-idx".to_owned(), 99, 99);
        app.history_retention_mut().expect("active session").max_bytes = 1;

        let _ = app.enforce_history_retention();
        assert_eq!(app.lookup_tool_call("tool-idx"), Some((2, 0)));
    }

    #[test]
    fn enforce_history_retention_prunes_subagent_attribution_for_dropped_tool_calls() {
        let mut app = make_test_app();
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            assistant_tool_message("tool-dropped", model::ToolCallStatus::Completed),
            assistant_tool_message("tool-kept", model::ToolCallStatus::InProgress),
        ];
        app.subagent_attribution_mut()
            .expect("active session")
            .insert("tool-dropped".to_owned(), "Explore".to_owned());
        app.subagent_attribution_mut()
            .expect("active session")
            .insert("tool-kept".to_owned(), "code-reviewer".to_owned());
        app.history_retention_mut().expect("active session").max_bytes = 1;

        let stats = app.enforce_history_retention();

        assert_eq!(stats.dropped_messages, 1);
        assert!(
            !app.subagent_attribution().expect("active session").contains_key("tool-dropped"),
            "attribution for a dropped tool call is pruned",
        );
        assert_eq!(
            app.subagent_attribution()
                .expect("active session")
                .get("tool-kept")
                .map(String::as_str),
            Some("code-reviewer"),
            "attribution for a still-retained tool call survives",
        );
    }

    #[test]
    fn enforce_history_retention_preserves_active_turn_assistant_message() {
        let mut app = make_test_app();
        app.status = AppStatus::Thinking;
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("drop this"),
            ChatMessage::new(MessageRole::Assistant, Vec::new()),
        ];
        app.bind_active_turn_assistant(2);
        app.history_retention_mut().expect("active session").max_bytes = 1;

        let stats = app.enforce_history_retention();

        assert_eq!(stats.dropped_messages, 1);
        assert_eq!(app.active_turn_assistant_idx(), Some(2));
        assert!(matches!(app.messages().expect("active session")[2].role, MessageRole::Assistant));
    }

    #[test]
    fn enforce_history_retention_remaps_active_turn_assistant_after_prune() {
        let mut app = make_test_app();
        app.status = AppStatus::Thinking;
        *app.active_messages_mut().expect("active session") = vec![
            user_text_message("drop this"),
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("streaming reply")]),
        ];
        app.bind_active_turn_assistant(1);
        app.history_retention_mut().expect("active session").max_bytes =
            App::measure_message_bytes(&app.messages().expect("active session")[1]);

        let stats = app.enforce_history_retention();

        assert_eq!(stats.dropped_messages, 1);
        assert_eq!(app.active_turn_assistant_idx(), Some(1));
        assert!(App::is_history_hidden_marker_message(&app.messages().expect("active session")[0]));
        assert!(matches!(app.messages().expect("active session")[1].role, MessageRole::Assistant));
    }

    #[test]
    fn enforce_history_retention_keeps_single_marker_on_repeat() {
        let mut app = make_test_app();
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("drop me"),
        ];
        app.history_retention_mut().expect("active session").max_bytes = 1;

        let first = app.enforce_history_retention();
        let second = app.enforce_history_retention();
        let marker_count = app
            .messages()
            .expect("active session")
            .iter()
            .filter(|msg| App::is_history_hidden_marker_message(msg))
            .count();

        assert_eq!(first.dropped_messages, 1);
        assert_eq!(second.dropped_messages, 0);
        assert_eq!(marker_count, 1);
    }

    #[test]
    fn enforce_history_retention_preserves_manual_scroll_anchor_across_drop_and_marker_insert() {
        let mut app = make_test_app();
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::welcome(env!("CARGO_PKG_VERSION"), "-", "/cwd", "-"),
            user_text_message("drop me first"),
            user_text_message("keep this anchored"),
            user_text_message("tail"),
        ];
        let _ = app.active_viewport_mut().expect("active session").on_frame(40, 12);
        {
            let n = app.messages().expect("active session").len();
            app.active_viewport_mut().expect("active session").sync_message_count(n);
        };
        for idx in 0..app.messages().expect("active session").len() {
            app.active_viewport_mut().expect("active session").set_message_height(idx, 4);
        }
        app.active_viewport_mut().expect("active session").mark_heights_valid();
        app.active_viewport_mut().expect("active session").rebuild_prefix_sums();

        app.active_viewport_mut().expect("active session").auto_scroll = false;
        app.active_viewport_mut().expect("active session").scroll_offset = 9;
        app.active_viewport_mut().expect("active session").scroll_target = 9;
        app.active_viewport_mut().expect("active session").scroll_pos = 9.0;
        app.history_retention_mut().expect("active session").max_bytes =
            app.measure_history_bytes().saturating_sub(App::measure_message_bytes(
                &app.messages().expect("active session")[1],
            ));

        let _ = app.enforce_history_retention();

        assert!(
            app.messages()
                .expect("active session")
                .iter()
                .any(App::is_history_hidden_marker_message)
        );

        let anchored = app
            .messages()
            .expect("active session")
            .iter()
            .position(|msg| {
                matches!(msg.blocks.first(), Some(MessageBlock::Text(block))
                    if block.text.contains("keep this anchored"))
            })
            .expect("the anchored message survives the drop");

        // Measure the marker taller than the message it replaced, so the rows
        // above the reader move. Sized identically, the anchor reproduces the
        // raw offset and the restore has nothing to do.
        let heights: Vec<usize> = app
            .messages()
            .expect("active session")
            .iter()
            .map(|msg| if App::is_history_hidden_marker_message(msg) { 6 } else { 4 })
            .collect();
        let vp = app.active_viewport_mut().expect("active session");
        vp.sync_message_count(heights.len());
        for (idx, &height) in heights.iter().enumerate() {
            vp.set_message_height(idx, height);
            vp.mark_message_height_measured(idx);
        }
        vp.rebuild_prefix_sums();
        assert_ne!(
            vp.find_first_visible(vp.scroll_offset),
            anchored,
            "fixture must move the rows above the reader so the raw offset drifts",
        );

        let anchor =
            vp.take_ready_scroll_anchor().expect("retention must not discard the reader's anchor");
        vp.restore_scroll_anchor(anchor.0, anchor.1);
        let top = vp.find_first_visible(vp.scroll_offset);

        assert_eq!(
            top, anchored,
            "dropping a message above the reader and inserting a marker in its place must \
             leave them on the message they were reading",
        );
    }

    #[test]
    fn insert_message_tracked_nontail_rebuilds_tool_indices_and_invalidates_suffix() {
        let mut app = make_test_app();
        app.active_messages_mut().expect("active session").push(user_text_message("before"));
        app.active_messages_mut()
            .expect("active session")
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::Completed));
        app.active_messages_mut().expect("active session").push(user_text_message("after"));
        app.index_tool_call("tool-1".to_owned(), 1, 0);

        let _ = app.active_viewport_mut().expect("active session").on_frame(80, 24);
        app.active_viewport_mut().expect("active session").sync_message_count(3);
        app.active_viewport_mut().expect("active session").mark_heights_valid();
        app.active_viewport_mut().expect("active session").rebuild_prefix_sums();

        app.insert_message_tracked(1, user_text_message("inserted"));
        {
            let n = app.messages().expect("active session").len();
            app.active_viewport_mut().expect("active session").sync_message_count(n);
        };

        assert_eq!(app.lookup_tool_call("tool-1"), Some((2, 0)));
        assert_eq!(
            app.active_viewport_mut().expect("active session").oldest_stale_index(),
            Some(1)
        );
        assert_eq!(app.active_viewport_mut().expect("active session").prefix_dirty_from(), Some(1));
    }

    #[test]
    fn remove_message_tracked_nontail_rebuilds_tool_indices_and_invalidates_suffix() {
        let mut app = make_test_app();
        app.active_messages_mut().expect("active session").push(user_text_message("before"));
        app.active_messages_mut()
            .expect("active session")
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::Completed));
        app.active_messages_mut().expect("active session").push(user_text_message("after"));
        app.index_tool_call("tool-1".to_owned(), 1, 0);

        let _ = app.active_viewport_mut().expect("active session").on_frame(80, 24);
        app.active_viewport_mut().expect("active session").sync_message_count(3);
        app.active_viewport_mut().expect("active session").mark_heights_valid();
        app.active_viewport_mut().expect("active session").rebuild_prefix_sums();

        let removed = app.remove_message_tracked(0);
        {
            let n = app.messages().expect("active session").len();
            app.active_viewport_mut().expect("active session").sync_message_count(n);
        };

        assert!(removed.is_some());
        assert_eq!(app.lookup_tool_call("tool-1"), Some((0, 0)));
        assert_eq!(
            app.active_viewport_mut().expect("active session").oldest_stale_index(),
            Some(0)
        );
        assert_eq!(app.active_viewport_mut().expect("active session").prefix_dirty_from(), Some(0));
    }

    #[test]
    fn remove_message_tracked_tail_removes_orphaned_tool_indices() {
        let mut app = make_test_app();
        app.active_messages_mut().expect("active session").push(user_text_message("before"));
        app.active_messages_mut()
            .expect("active session")
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::Completed));
        app.index_tool_call("tool-1".to_owned(), 1, 0);

        let removed = app.remove_message_tracked(1);

        assert!(removed.is_some());
        assert!(app.lookup_tool_call("tool-1").is_none());
    }

    #[test]
    fn remove_message_tracked_prunes_tool_scope_entries() {
        let mut app = make_test_app();
        app.active_messages_mut()
            .expect("active session")
            .push(assistant_tool_message("tool-1", model::ToolCallStatus::Completed));
        app.index_tool_call("tool-1".to_owned(), 0, 0);
        app.register_tool_call_scope(
            "tool-1".to_owned(),
            ToolCallScope::SubagentChild { parent_tool_use_id: "task-1".to_owned() },
        );

        let removed = app.remove_message_tracked(0);

        assert!(removed.is_some());
        assert_eq!(app.tool_call_scope("tool-1"), None);
    }

    #[test]
    fn clear_messages_tracked_clears_tool_tracking() {
        let mut app = make_test_app();
        app.active_messages_mut()
            .expect("active session")
            .push(assistant_bash_tool_message("bash-1", model::ToolCallStatus::InProgress));
        app.index_tool_call("bash-1".to_owned(), 0, 0);

        app.clear_messages_tracked();

        assert!(app.messages().expect("active session").is_empty());
        assert!(app.tool_call_index().expect("active session").is_empty());
    }
}
