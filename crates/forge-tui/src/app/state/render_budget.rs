use super::messages::MessageBlock;
use super::types::{AppStatus, CacheBudgetEnforceStats};
use crate::agent::model;
use std::cmp::Reverse;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct RenderCacheEvictionKey {
    pub(super) last_access_tick: u64,
    pub(super) bytes_desc: Reverse<usize>,
    pub(super) msg_idx: usize,
    pub(super) block_idx: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct RenderCacheSlotState {
    pub(super) cached_bytes: usize,
    pub(super) last_access_tick: u64,
    pub(super) protected: bool,
}

impl super::App {
    fn render_cache_slot_count_for_message(msg: &super::ChatMessage) -> usize {
        msg.blocks.len().saturating_add(1)
    }

    fn is_message_render_cache_slot(&self, msg_idx: usize, slot_idx: usize) -> bool {
        self.messages().get(msg_idx).is_some_and(|msg| slot_idx == msg.blocks.len())
    }

    fn is_render_cache_message_protected(&self, msg_idx: usize) -> bool {
        if self.protected_streaming_message_idx() == Some(msg_idx) {
            return true;
        }
        self.messages().get(msg_idx).is_some_and(|msg| {
            (0..msg.blocks.len())
                .any(|block_idx| self.is_render_cache_block_protected(msg_idx, block_idx))
        })
    }

    fn is_streaming_tail_protected(&self) -> bool {
        matches!(self.status, AppStatus::Thinking | AppStatus::Running)
    }

    fn protected_streaming_message_idx(&self) -> Option<usize> {
        if !self.is_streaming_tail_protected() {
            return None;
        }
        self.active_turn_assistant_idx().or_else(|| self.messages().len().checked_sub(1))
    }

    fn block_cache(block: &MessageBlock) -> &super::BlockCache {
        match block {
            MessageBlock::Text(block) => &block.cache,
            MessageBlock::Notice(block) => &block.text.cache,
            MessageBlock::Welcome(welcome) => &welcome.cache,
            MessageBlock::ToolCall(tc) => &tc.cache,
            MessageBlock::ImageAttachment(img) => &img.cache,
        }
    }

    fn is_render_cache_block_protected(&self, msg_idx: usize, block_idx: usize) -> bool {
        let tail_protected = self.protected_streaming_message_idx() == Some(msg_idx);
        let Some(block) = self.messages().get(msg_idx).and_then(|msg| msg.blocks.get(block_idx))
        else {
            return false;
        };
        let tool_protected = matches!(
            block,
            MessageBlock::ToolCall(tc)
                if matches!(
                    tc.status,
                    model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress
                )
        );
        tail_protected || tool_protected
    }

    fn render_cache_slot_key(
        msg_idx: usize,
        block_idx: usize,
        slot: &RenderCacheSlotState,
    ) -> Option<RenderCacheEvictionKey> {
        (!slot.protected && slot.cached_bytes > 0).then_some(RenderCacheEvictionKey {
            last_access_tick: slot.last_access_tick,
            bytes_desc: Reverse(slot.cached_bytes),
            msg_idx,
            block_idx,
        })
    }

    // O(1) on purpose: this runs from every sync_render_cache_* call,
    // so a walk over the message list here costs the whole session on
    // every rendered message. Block-count changes arrive through
    // `sync_after_message_blocks_changed`, and each sync entry point
    // re-checks its own message's slot count on the way in, so the
    // per-message shape does not need re-deriving here too.
    //
    // A same-count in-place byte change must go through
    // sync_render_cache_slot; it is not caught by this guard.
    fn render_cache_slots_match_messages(&self) -> bool {
        self.render_cache_slots().len() == self.messages().len()
    }

    pub(crate) fn rebuild_render_cache_accounting(&mut self) {
        let msg_count = self.messages().len();
        {
            let slots = self.render_cache_slots_mut();
            slots.clear();
            slots.reserve(msg_count);
        }
        *self.render_cache_total_bytes_mut() = 0;
        *self.render_cache_protected_bytes_mut() = 0;
        self.render_cache_evictable_mut().clear();

        let protected_tail = self.protected_streaming_message_idx();
        let mut all_slots: Vec<Vec<RenderCacheSlotState>> = Vec::with_capacity(msg_count);
        let mut total_bytes: usize = 0;
        let mut protected_bytes: usize = 0;
        let mut evictable_keys: Vec<RenderCacheEvictionKey> = Vec::new();
        for (msg_idx, msg) in self.messages().iter().enumerate() {
            let mut slots = Vec::with_capacity(Self::render_cache_slot_count_for_message(msg));
            for (block_idx, block) in msg.blocks.iter().enumerate() {
                let cache = Self::block_cache(block);
                let cached_bytes = cache.cached_bytes();
                let protected = protected_tail == Some(msg_idx)
                    || matches!(
                        block,
                        MessageBlock::ToolCall(tc)
                            if matches!(
                                tc.status,
                                model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress
                            )
                    );
                let slot = RenderCacheSlotState {
                    cached_bytes,
                    last_access_tick: cache.last_access_tick(),
                    protected,
                };
                total_bytes = total_bytes.saturating_add(cached_bytes);
                if protected {
                    protected_bytes = protected_bytes.saturating_add(cached_bytes);
                } else if let Some(key) = Self::render_cache_slot_key(msg_idx, block_idx, &slot) {
                    evictable_keys.push(key);
                }
                slots.push(slot);
            }
            let message_slot = RenderCacheSlotState {
                cached_bytes: msg.render_cache.cached_bytes(),
                last_access_tick: msg.render_cache.last_access_tick(),
                protected: self.is_render_cache_message_protected(msg_idx),
            };
            total_bytes = total_bytes.saturating_add(message_slot.cached_bytes);
            if message_slot.protected {
                protected_bytes = protected_bytes.saturating_add(message_slot.cached_bytes);
            } else if let Some(key) =
                Self::render_cache_slot_key(msg_idx, msg.blocks.len(), &message_slot)
            {
                evictable_keys.push(key);
            }
            slots.push(message_slot);
            all_slots.push(slots);
        }
        *self.render_cache_slots_mut() = all_slots;
        *self.render_cache_total_bytes_mut() = total_bytes;
        *self.render_cache_protected_bytes_mut() = protected_bytes;
        {
            let evictable = self.render_cache_evictable_mut();
            for key in evictable_keys {
                evictable.insert(key);
            }
        }
        self.set_render_cache_tail_msg_idx(protected_tail);
    }

    /// Resize `msg_idx`'s slot row to match its block count, inserting
    /// or removing slots at the END of the block range - immediately
    /// before the message slot.
    ///
    /// Only the message slot changes index, so only its eviction key has
    /// to move; every surviving block slot keeps the index its key was
    /// built from. That is what makes this independent of how many
    /// blocks the message already holds.
    ///
    /// Returns `false` when it fell back to a whole-session rebuild, in
    /// which case the caller has nothing left to do.
    fn resize_render_cache_row(&mut self, msg_idx: usize) -> bool {
        let protected_tail = self.protected_streaming_message_idx();
        if protected_tail != self.render_cache_tail_msg_idx() {
            crate::perf::mark("rc::row_fallback_tail_moved");
            self.rebuild_render_cache_accounting();
            return false;
        }
        if !self.render_cache_slots_match_messages() {
            crate::perf::mark("rc::row_fallback_row_count");
            self.rebuild_render_cache_accounting();
            return false;
        }
        if msg_idx >= self.messages().len() {
            // A caller naming a message that does not exist.
            tracing::warn!(
                target: crate::logging::targets::APP_RENDER,
                event_name = "render_cache_row_resize_bad_index",
                msg_idx,
                message_count = self.messages().len(),
                outcome = "full_rebuild",
            );
            crate::perf::mark("rc::row_fallback_bad_index");
            self.rebuild_render_cache_accounting();
            return false;
        }
        let Some(want) =
            self.messages().get(msg_idx).map(Self::render_cache_slot_count_for_message)
        else {
            self.rebuild_render_cache_accounting();
            return false;
        };
        let have = self.render_cache_slots()[msg_idx].len();
        if have == want {
            return true;
        }
        // A row with no message slot never came from the builder.
        let Some(old_msg_slot_idx) = have.checked_sub(1) else {
            self.rebuild_render_cache_accounting();
            return false;
        };
        let message_slot = self.render_cache_slots()[msg_idx][old_msg_slot_idx];
        if let Some(key) = Self::render_cache_slot_key(msg_idx, old_msg_slot_idx, &message_slot) {
            self.render_cache_evictable_mut().remove(&key);
        }

        if want > have {
            // Fresh slots carry no bytes and no key; the per-slot sync
            // below fills them from the blocks.
            let row = &mut self.render_cache_slots_mut()[msg_idx];
            row.pop();
            row.resize(want - 1, RenderCacheSlotState::default());
            row.push(message_slot);
        } else {
            let dropped: Vec<RenderCacheSlotState> =
                self.render_cache_slots()[msg_idx][want - 1..old_msg_slot_idx].to_vec();
            for (offset, slot) in dropped.iter().enumerate() {
                let block_idx = want - 1 + offset;
                if let Some(key) = Self::render_cache_slot_key(msg_idx, block_idx, slot) {
                    self.render_cache_evictable_mut().remove(&key);
                }
                let total = self.render_cache_total_bytes().saturating_sub(slot.cached_bytes);
                *self.render_cache_total_bytes_mut() = total;
                if slot.protected {
                    let p = self.render_cache_protected_bytes().saturating_sub(slot.cached_bytes);
                    *self.render_cache_protected_bytes_mut() = p;
                }
            }
            let row = &mut self.render_cache_slots_mut()[msg_idx];
            row.truncate(want - 1);
            row.push(message_slot);
        }
        // Belt and braces: today's only caller re-syncs this slot straight
        // after, which would re-insert the key anyway. Leaving the row
        // self-consistent here is the point - a function that needs its
        // caller to finish the job is what turned a transient
        // inconsistency into a permanent one when the caller changed.
        if let Some(key) = Self::render_cache_slot_key(msg_idx, want - 1, &message_slot) {
            self.render_cache_evictable_mut().insert(key);
        }
        true
    }

    /// Sync the accounting for a message whose TAIL changed: the last
    /// block was extended, or blocks were appended or dropped at the
    /// end. Earlier blocks are assumed untouched, which is what every
    /// caller of [`Self::sync_after_message_blocks_changed`] does.
    ///
    /// Costs O(blocks added or dropped), not O(blocks in the message).
    /// Syncing every slot instead made a run of N appends into one
    /// message quadratic - 5.8ms of a 7.98ms 200-envelope run - because
    /// appending the Nth block re-synced all N.
    pub(crate) fn sync_render_cache_message_tail(&mut self, msg_idx: usize) {
        let previous_blocks =
            self.render_cache_slots().get(msg_idx).map(|row| row.len().saturating_sub(1));
        if !self.resize_render_cache_row(msg_idx) {
            return;
        }
        let Some(block_count) = self.messages().get(msg_idx).map(|msg| msg.blocks.len()) else {
            return;
        };
        // Start at the previously-last block: an in-place extend leaves
        // the count unchanged but changes that block's cached bytes.
        let first_dirty = previous_blocks.unwrap_or(block_count).min(block_count).saturating_sub(1);
        for block_idx in first_dirty..block_count {
            self.sync_render_cache_slot(msg_idx, block_idx);
        }
        self.sync_render_cache_slot(msg_idx, block_count);
    }

    pub(crate) fn ensure_render_cache_accounting(&mut self) {
        if !self.render_cache_slots_match_messages() {
            self.rebuild_render_cache_accounting();
        }
    }

    pub(crate) fn sync_render_cache_slot(&mut self, msg_idx: usize, block_idx: usize) {
        self.ensure_render_cache_accounting();
        // Catches a block-count change that never announced itself,
        // including a shrink (where indexing the row would still
        // succeed against a stale slot).
        if self.render_cache_slots().get(msg_idx).map(Vec::len)
            != self.messages().get(msg_idx).map(Self::render_cache_slot_count_for_message)
        {
            self.rebuild_render_cache_accounting();
        }
        let Some(old_slot) =
            self.render_cache_slots().get(msg_idx).and_then(|slots| slots.get(block_idx)).copied()
        else {
            self.rebuild_render_cache_accounting();
            return;
        };

        if let Some(old_key) = Self::render_cache_slot_key(msg_idx, block_idx, &old_slot) {
            self.render_cache_evictable_mut().remove(&old_key);
        }
        let new_total = self.render_cache_total_bytes().saturating_sub(old_slot.cached_bytes);
        *self.render_cache_total_bytes_mut() = new_total;
        if old_slot.protected {
            let new_protected =
                self.render_cache_protected_bytes().saturating_sub(old_slot.cached_bytes);
            *self.render_cache_protected_bytes_mut() = new_protected;
        }

        let new_slot = if self.is_message_render_cache_slot(msg_idx, block_idx) {
            let Some(msg) = self.messages().get(msg_idx) else {
                self.rebuild_render_cache_accounting();
                return;
            };
            RenderCacheSlotState {
                cached_bytes: msg.render_cache.cached_bytes(),
                last_access_tick: msg.render_cache.last_access_tick(),
                protected: self.is_render_cache_message_protected(msg_idx),
            }
        } else {
            let Some(block) =
                self.messages().get(msg_idx).and_then(|msg| msg.blocks.get(block_idx))
            else {
                self.rebuild_render_cache_accounting();
                return;
            };
            let cache = Self::block_cache(block);
            RenderCacheSlotState {
                cached_bytes: cache.cached_bytes(),
                last_access_tick: cache.last_access_tick(),
                protected: self.is_render_cache_block_protected(msg_idx, block_idx),
            }
        };
        let mut needs_full_rebuild = false;
        if let Some(slots) = self.render_cache_slots_mut().get_mut(msg_idx) {
            if let Some(slot) = slots.get_mut(block_idx) {
                *slot = new_slot;
            } else {
                needs_full_rebuild = true;
            }
        } else {
            needs_full_rebuild = true;
        }
        if needs_full_rebuild {
            self.rebuild_render_cache_accounting();
            return;
        }

        let new_total = self.render_cache_total_bytes().saturating_add(new_slot.cached_bytes);
        *self.render_cache_total_bytes_mut() = new_total;
        if new_slot.protected {
            let new_protected =
                self.render_cache_protected_bytes().saturating_add(new_slot.cached_bytes);
            *self.render_cache_protected_bytes_mut() = new_protected;
        } else if let Some(new_key) = Self::render_cache_slot_key(msg_idx, block_idx, &new_slot) {
            self.render_cache_evictable_mut().insert(new_key);
        }

        // The message slot's `protected` is derived from its blocks, so
        // a block whose protection flipped leaves it stale. Repairing it
        // here rather than at each status writer means a new writer
        // cannot get it wrong. Recursion stops at depth one: the message
        // slot is not a block slot, so it takes the other arm.
        if old_slot.protected != new_slot.protected
            && !self.is_message_render_cache_slot(msg_idx, block_idx)
            && let Some(message_slot_idx) = self.messages().get(msg_idx).map(|m| m.blocks.len())
        {
            self.sync_render_cache_slot(msg_idx, message_slot_idx);
        }
    }

    pub(crate) fn sync_render_cache_message(&mut self, msg_idx: usize) {
        self.ensure_render_cache_accounting();
        let Some(msg) = self.messages().get(msg_idx) else {
            self.rebuild_render_cache_accounting();
            return;
        };
        let block_count = msg.blocks.len();
        let slot_count = Self::render_cache_slot_count_for_message(msg);
        if self.render_cache_slots().get(msg_idx).map_or(usize::MAX, Vec::len) != slot_count {
            self.rebuild_render_cache_accounting();
            return;
        }
        for block_idx in 0..block_count {
            self.sync_render_cache_slot(msg_idx, block_idx);
        }
        self.sync_render_cache_slot(msg_idx, block_count);
    }

    pub(crate) fn refresh_tail_message_cache_protection(&mut self) {
        self.ensure_render_cache_accounting();
        let next_tail = self.protected_streaming_message_idx();
        if self.render_cache_tail_msg_idx() == next_tail {
            return;
        }

        let previous_tail = self.render_cache_tail_msg_idx();
        self.set_render_cache_tail_msg_idx(next_tail);

        if let Some(msg_idx) = previous_tail {
            self.sync_render_cache_message(msg_idx);
        }
        if let Some(msg_idx) = next_tail
            && Some(msg_idx) != previous_tail
        {
            self.sync_render_cache_message(msg_idx);
        }
    }

    /// Every message's slot row still matches its block count. The
    /// shared guard only compares list lengths, so a block-count change
    /// that never announced itself leaves one row stale.
    fn render_cache_rows_match_messages(&self) -> bool {
        self.render_cache_slots()
            .iter()
            .zip(self.messages().iter())
            .all(|(slots, msg)| slots.len() == Self::render_cache_slot_count_for_message(msg))
    }

    /// Rebuild if any message's slot row disagrees with its block
    /// count. The shared guard only compares list lengths, so a
    /// block-count change that skipped
    /// `sync_after_message_blocks_changed` leaves one row stale and the
    /// byte totals short. Firing means such a change reached us.
    fn repair_render_cache_accounting_drift(&mut self) {
        self.ensure_render_cache_accounting();
        if self.render_cache_rows_match_messages() {
            return;
        }
        tracing::warn!(
            target: crate::logging::targets::APP_RENDER,
            event_name = "render_cache_accounting_drift",
            message = "slot rows disagreed with block counts; rebuilding before reading totals",
            outcome = "rebuilt",
        );
        self.rebuild_render_cache_accounting();
    }

    fn refresh_render_cache_eviction_order(&mut self) {
        struct SlotUpdate {
            msg_idx: usize,
            block_idx: usize,
            slot: RenderCacheSlotState,
        }

        self.ensure_render_cache_accounting();
        self.render_cache_evictable_mut().clear();

        // Snapshot every slot's new state into a Vec so we never
        // have an immutable borrow on `self.messages()` while
        // mutating `self.render_cache_slots` /
        // `self.render_cache_evictable`.
        let mut updates: Vec<SlotUpdate> = Vec::new();
        let msg_count = self.messages().len();
        let mut block_protections: Vec<Vec<bool>> = Vec::with_capacity(msg_count);
        let mut message_protections: Vec<bool> = Vec::with_capacity(msg_count);
        for msg_idx in 0..msg_count {
            let block_count = self.messages().get(msg_idx).map_or(0, |m| m.blocks.len());
            let mut row = Vec::with_capacity(block_count);
            for block_idx in 0..block_count {
                row.push(self.is_render_cache_block_protected(msg_idx, block_idx));
            }
            block_protections.push(row);
            message_protections.push(self.is_render_cache_message_protected(msg_idx));
        }
        for (msg_idx, msg) in self.messages().iter().enumerate() {
            for (block_idx, block) in msg.blocks.iter().enumerate() {
                let cache = Self::block_cache(block);
                let protected = block_protections[msg_idx][block_idx];
                let slot = RenderCacheSlotState {
                    cached_bytes: cache.cached_bytes(),
                    last_access_tick: cache.last_access_tick(),
                    protected,
                };
                updates.push(SlotUpdate { msg_idx, block_idx, slot });
            }
            let message_slot_idx = msg.blocks.len();
            let slot = RenderCacheSlotState {
                cached_bytes: msg.render_cache.cached_bytes(),
                last_access_tick: msg.render_cache.last_access_tick(),
                protected: message_protections[msg_idx],
            };
            updates.push(SlotUpdate { msg_idx, block_idx: message_slot_idx, slot });
        }
        // Re-derive both totals from the values already being walked.
        // Writing back only the flags left any accumulated drift in
        // place, and the byte counts drive the budget comparison. This
        // is the periodic re-derivation the append path used to get for
        // free from a whole-session rebuild per chunk: it costs nothing
        // extra here, and it heals the class rather than one instance.
        let mut total_bytes: usize = 0;
        let mut protected_bytes: usize = 0;
        for SlotUpdate { msg_idx, block_idx, slot } in updates {
            if let Some(slots) = self.render_cache_slots_mut().get_mut(msg_idx)
                && let Some(existing) = slots.get_mut(block_idx)
            {
                *existing = slot;
            }
            total_bytes = total_bytes.saturating_add(slot.cached_bytes);
            if slot.protected {
                protected_bytes = protected_bytes.saturating_add(slot.cached_bytes);
            }
            if let Some(key) = Self::render_cache_slot_key(msg_idx, block_idx, &slot) {
                self.render_cache_evictable_mut().insert(key);
            }
        }
        *self.render_cache_total_bytes_mut() = total_bytes;
        *self.render_cache_protected_bytes_mut() = protected_bytes;
    }

    pub fn enforce_render_cache_budget(&mut self) -> CacheBudgetEnforceStats {
        let mut stats = CacheBudgetEnforceStats::default();
        self.refresh_tail_message_cache_protection();
        // Before the totals are read, not before eviction: deciding NOT
        // to evict is also a decision, and a drifted-short total is
        // exactly what makes the under-budget branch below fire when it
        // should not. Once per frame rather than once per rendered
        // message.
        self.repair_render_cache_accounting_drift();
        stats.total_before_bytes = self.render_cache_total_bytes();
        stats.protected_bytes = self.render_cache_protected_bytes();

        // Budget comparison uses only non-protected (evictable) bytes.
        let budgeted_bytes = stats.total_before_bytes.saturating_sub(stats.protected_bytes);

        if budgeted_bytes <= self.render_cache_budget.max_bytes {
            self.render_cache_budget.last_total_bytes = budgeted_bytes;
            stats.total_after_bytes = stats.total_before_bytes;
            return stats;
        }

        self.refresh_render_cache_eviction_order();
        let mut current_budgeted = budgeted_bytes;
        stats.total_after_bytes = stats.total_before_bytes;

        while let Some(slot) = self.render_cache_evictable().and_then(|s| s.first().copied()) {
            if current_budgeted <= self.render_cache_budget.max_bytes {
                break;
            }
            self.render_cache_evictable_mut().remove(&slot);
            let removed = self.evict_cache_slot(slot.msg_idx, slot.block_idx);
            if removed == 0 {
                continue;
            }
            current_budgeted = current_budgeted.saturating_sub(removed);
            stats.total_after_bytes = stats.total_after_bytes.saturating_sub(removed);
            stats.evicted_bytes = stats.evicted_bytes.saturating_add(removed);
            stats.evicted_blocks = stats.evicted_blocks.saturating_add(1);
        }

        self.render_cache_budget.last_total_bytes = current_budgeted;
        self.render_cache_budget.total_evictions =
            self.render_cache_budget.total_evictions.saturating_add(stats.evicted_blocks);

        stats
    }

    fn evict_cache_slot(&mut self, msg_idx: usize, block_idx: usize) -> usize {
        let Some(msg) = self.active_messages_mut().get_mut(msg_idx) else {
            return 0;
        };
        if block_idx == msg.blocks.len() {
            let removed = msg.render_cache.evict_cached_render();
            if removed > 0 {
                self.sync_render_cache_slot(msg_idx, block_idx);
            }
            return removed;
        }
        let Some(block) = msg.blocks.get_mut(block_idx) else {
            return 0;
        };
        let removed = match block {
            MessageBlock::Text(block) => block.cache.evict_cached_render(),
            MessageBlock::Notice(block) => block.text.cache.evict_cached_render(),
            MessageBlock::Welcome(welcome) => welcome.cache.evict_cached_render(),
            MessageBlock::ToolCall(tc) => tc.cache.evict_cached_render(),
            MessageBlock::ImageAttachment(img) => img.cache.evict_cached_render(),
        };
        if removed > 0 {
            self.sync_render_cache_slot(msg_idx, block_idx);
        }
        removed
    }
}
