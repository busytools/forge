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
        self.messages()
            .and_then(|messages| messages.get(msg_idx))
            .is_some_and(|msg| slot_idx == msg.blocks.len())
    }

    fn is_render_cache_message_protected(&self, msg_idx: usize) -> bool {
        let tail_protected = self.protected_streaming_message_idx() == Some(msg_idx);
        if tail_protected {
            return true;
        }
        self.messages().and_then(|messages| messages.get(msg_idx)).is_some_and(|msg| {
            msg.blocks
                .iter()
                .any(|block| Self::block_is_render_cache_protected(tail_protected, Some(block)))
        })
    }

    fn is_streaming_tail_protected(&self) -> bool {
        matches!(self.status, AppStatus::Thinking | AppStatus::Running)
    }

    fn protected_streaming_message_idx(&self) -> Option<usize> {
        if !self.is_streaming_tail_protected() {
            return None;
        }
        self.active_turn_assistant_idx()
            .or_else(|| self.messages().map_or(0, <[super::ChatMessage]>::len).checked_sub(1))
    }

    /// The message-level cache's bytes and last-access tick, by the
    /// message's own id. This walk holds no view inputs, and a key it
    /// cannot recompute is one it could never find - which reads as zero
    /// bytes and never evicts.
    fn message_cache_state(&self, msg: &super::ChatMessage) -> (usize, u64) {
        let cache = self.render_caches.peek_message(msg.id);
        (cache.cached_bytes(), cache.last_access_tick())
    }

    /// One block's cached bytes and last-access tick. Every kind is
    /// resolved by its own identity: a tool call's id and render epoch, a
    /// text block's id, a welcome card's content.
    fn block_cache_state(&self, block: &MessageBlock) -> (usize, u64) {
        match block {
            MessageBlock::ToolCall(tc) => {
                let cache = self.render_caches.peek_tool_call(&tc.id, tc.render_epoch);
                (cache.cached_bytes(), cache.last_access_tick())
            }
            MessageBlock::Text(block) => {
                let cache = self.render_caches.peek_block(block.id);
                (cache.cached_bytes(), cache.last_access_tick())
            }
            MessageBlock::Notice(block) => {
                let cache = self.render_caches.peek_block(block.text.id);
                (cache.cached_bytes(), cache.last_access_tick())
            }
            MessageBlock::Welcome(welcome) => {
                let cache = self
                    .render_caches
                    .peek_welcome(super::messages::hash_welcome_block_content(welcome));
                (cache.cached_bytes(), cache.last_access_tick())
            }
            // An image-attachment block holds only a count and renders no
            // cached rows, so it carries no bytes.
            MessageBlock::ImageAttachment(_) => (0, 0),
        }
    }

    /// The status-only half of block protection, answered against a
    /// block the caller already borrowed. The walk sites resolve
    /// `messages()` once per walk rather than once per block - each
    /// re-resolution is a session-map lookup plus a slice, and a
    /// message that only grows paid it for every block on every insert
    /// (#793).
    fn block_is_render_cache_protected(tail_protected: bool, block: Option<&MessageBlock>) -> bool {
        let tool_protected = matches!(
            block,
            Some(MessageBlock::ToolCall(tc))
                if matches!(
                    tc.status,
                    model::ToolCallStatus::Pending | model::ToolCallStatus::InProgress
                )
        );
        tail_protected || tool_protected
    }

    fn is_render_cache_block_protected(&self, msg_idx: usize, block_idx: usize) -> bool {
        let tail_protected = self.protected_streaming_message_idx() == Some(msg_idx);
        self.messages().and_then(|messages| messages.get(msg_idx)).is_some_and(|msg| {
            Self::block_is_render_cache_protected(tail_protected, msg.blocks.get(block_idx))
        })
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
    // `sync_render_cache_message_tail`, and each sync entry point
    // re-checks its own message's slot count on the way in, so the
    // per-message shape does not need re-deriving here too.
    //
    // A same-count in-place byte change must go through
    // sync_render_cache_slot; it is not caught by this guard.
    fn render_cache_slots_match_messages(&self) -> bool {
        self.render_cache_slots().map_or(0, <[Vec<RenderCacheSlotState>]>::len)
            == self.messages().map_or(0, <[super::ChatMessage]>::len)
    }

    pub(crate) fn rebuild_render_cache_accounting(&mut self) {
        let msg_count = self.messages().map_or(0, <[super::ChatMessage]>::len);
        if let Some(slots) = self.render_cache_slots_mut() {
            slots.clear();
            slots.reserve(msg_count);
        }
        if let Some(total) = self.render_cache_total_bytes_mut() {
            *total = 0;
        }
        if let Some(protected) = self.render_cache_protected_bytes_mut() {
            *protected = 0;
        }
        if let Some(evictable) = self.render_cache_evictable_mut() {
            evictable.clear();
        }

        let protected_tail = self.protected_streaming_message_idx();
        let mut all_slots: Vec<Vec<RenderCacheSlotState>> = Vec::with_capacity(msg_count);
        let mut total_bytes: usize = 0;
        let mut protected_bytes: usize = 0;
        let mut evictable_keys: Vec<RenderCacheEvictionKey> = Vec::new();
        let messages = self.messages().unwrap_or_default();
        for (msg_idx, msg) in messages.iter().enumerate() {
            let mut slots = Vec::with_capacity(Self::render_cache_slot_count_for_message(msg));
            let tail_protected = protected_tail == Some(msg_idx);
            for (block_idx, block) in msg.blocks.iter().enumerate() {
                let (cached_bytes, last_access_tick) = self.block_cache_state(block);
                let protected = Self::block_is_render_cache_protected(tail_protected, Some(block));
                let slot = RenderCacheSlotState { cached_bytes, last_access_tick, protected };
                total_bytes = total_bytes.saturating_add(cached_bytes);
                if protected {
                    protected_bytes = protected_bytes.saturating_add(cached_bytes);
                } else if let Some(key) = Self::render_cache_slot_key(msg_idx, block_idx, &slot) {
                    evictable_keys.push(key);
                }
                slots.push(slot);
            }
            let (cached_bytes, last_access_tick) = self.message_cache_state(msg);
            let message_slot = RenderCacheSlotState {
                cached_bytes,
                last_access_tick,
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
        if let Some(slots) = self.render_cache_slots_mut() {
            *slots = all_slots;
        }
        if let Some(total) = self.render_cache_total_bytes_mut() {
            *total = total_bytes;
        }
        if let Some(protected) = self.render_cache_protected_bytes_mut() {
            *protected = protected_bytes;
        }
        if let Some(evictable) = self.render_cache_evictable_mut() {
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
        let message_count = self.messages().map_or(0, <[super::ChatMessage]>::len);
        if msg_idx >= message_count {
            // A caller naming a message that does not exist.
            tracing::warn!(
                target: crate::logging::targets::APP_RENDER,
                event_name = "render_cache_row_resize_bad_index",
                msg_idx,
                message_count,
                outcome = "full_rebuild",
            );
            crate::perf::mark("rc::row_fallback_bad_index");
            self.rebuild_render_cache_accounting();
            return false;
        }
        let Some(want) = self
            .messages()
            .and_then(|messages| messages.get(msg_idx))
            .map(Self::render_cache_slot_count_for_message)
        else {
            self.rebuild_render_cache_accounting();
            return false;
        };
        let Some(have) =
            self.render_cache_slots().and_then(|slots| slots.get(msg_idx)).map(Vec::len)
        else {
            self.rebuild_render_cache_accounting();
            return false;
        };
        if have == want {
            return true;
        }
        // A row with no message slot never came from the builder.
        let Some(old_msg_slot_idx) = have.checked_sub(1) else {
            self.rebuild_render_cache_accounting();
            return false;
        };
        let Some(message_slot) = self
            .render_cache_slots()
            .and_then(|slots| slots.get(msg_idx))
            .and_then(|row| row.get(old_msg_slot_idx))
            .copied()
        else {
            self.rebuild_render_cache_accounting();
            return false;
        };
        if let Some(key) = Self::render_cache_slot_key(msg_idx, old_msg_slot_idx, &message_slot)
            && let Some(evictable) = self.render_cache_evictable_mut()
        {
            evictable.remove(&key);
        }

        if want > have {
            // Fresh slots carry no bytes and no key; the per-slot sync
            // below fills them from the blocks.
            let Some(row) = self.render_cache_slots_mut().and_then(|slots| slots.get_mut(msg_idx))
            else {
                self.rebuild_render_cache_accounting();
                return false;
            };
            row.pop();
            row.resize(want - 1, RenderCacheSlotState::default());
            row.push(message_slot);
        } else {
            let dropped: Vec<RenderCacheSlotState> = self
                .render_cache_slots()
                .and_then(|slots| slots.get(msg_idx))
                .map(|row| row[want - 1..old_msg_slot_idx].to_vec())
                .unwrap_or_default();
            for (offset, slot) in dropped.iter().enumerate() {
                let block_idx = want - 1 + offset;
                if let Some(key) = Self::render_cache_slot_key(msg_idx, block_idx, slot)
                    && let Some(evictable) = self.render_cache_evictable_mut()
                {
                    evictable.remove(&key);
                }
                let total = self
                    .render_cache_total_bytes()
                    .map_or(0, |bytes| bytes.saturating_sub(slot.cached_bytes));
                if let Some(total_ref) = self.render_cache_total_bytes_mut() {
                    *total_ref = total;
                }
                if slot.protected {
                    let p = self
                        .render_cache_protected_bytes()
                        .map_or(0, |bytes| bytes.saturating_sub(slot.cached_bytes));
                    if let Some(protected_ref) = self.render_cache_protected_bytes_mut() {
                        *protected_ref = p;
                    }
                }
            }
            let Some(row) = self.render_cache_slots_mut().and_then(|slots| slots.get_mut(msg_idx))
            else {
                self.rebuild_render_cache_accounting();
                return false;
            };
            row.truncate(want - 1);
            row.push(message_slot);
        }
        // Belt and braces: today's only caller re-syncs this slot straight
        // after, which would re-insert the key anyway. Leaving the row
        // self-consistent here is the point - a function that needs its
        // caller to finish the job is what turned a transient
        // inconsistency into a permanent one when the caller changed.
        if let Some(key) = Self::render_cache_slot_key(msg_idx, want - 1, &message_slot)
            && let Some(evictable) = self.render_cache_evictable_mut()
        {
            evictable.insert(key);
        }
        true
    }

    /// Sync the accounting for a message whose TAIL changed: the last
    /// block was extended, or blocks were appended or dropped at the
    /// end. Earlier blocks are assumed untouched, which is what every
    /// caller of [`Self::sync_render_cache_message_tail`] does.
    ///
    /// Costs O(blocks added or dropped), not O(blocks in the message).
    /// Syncing every slot instead made a run of N appends into one
    /// message quadratic - 5.8ms of a 7.98ms 200-envelope run - because
    /// appending the Nth block re-synced all N.
    pub(crate) fn sync_render_cache_message_tail(&mut self, msg_idx: usize) {
        if self.render_cache_accounting_suspended() {
            return;
        }
        let previous_blocks = self
            .render_cache_slots()
            .and_then(|slots| slots.get(msg_idx))
            .map(|row| row.len().saturating_sub(1));
        if !self.resize_render_cache_row(msg_idx) {
            return;
        }
        let Some(block_count) =
            self.messages().and_then(|messages| messages.get(msg_idx)).map(|msg| msg.blocks.len())
        else {
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

    /// True while a resume replay is walking history, which
    /// `load_resume_history` closes with a rebuild. Without this every
    /// appended message trips the shared guard into a full
    /// O(messages x blocks) rebuild, because `push_message_tracked`
    /// leaves the slot rows one short by design.
    fn render_cache_accounting_suspended(&self) -> bool {
        self.replay_in_progress
    }

    pub(crate) fn ensure_render_cache_accounting(&mut self) {
        if self.render_cache_accounting_suspended() {
            return;
        }
        if !self.render_cache_slots_match_messages() {
            self.rebuild_render_cache_accounting();
        }
    }

    pub(crate) fn sync_render_cache_slot(&mut self, msg_idx: usize, block_idx: usize) {
        if self.render_cache_accounting_suspended() {
            return;
        }
        self.ensure_render_cache_accounting();
        // Catches a block-count change that never announced itself,
        // including a shrink (where indexing the row would still
        // succeed against a stale slot).
        if self.render_cache_slots().and_then(|slots| slots.get(msg_idx)).map(Vec::len)
            != self
                .messages()
                .and_then(|messages| messages.get(msg_idx))
                .map(Self::render_cache_slot_count_for_message)
        {
            self.rebuild_render_cache_accounting();
        }
        let Some(old_slot) = self
            .render_cache_slots()
            .and_then(|slots| slots.get(msg_idx))
            .and_then(|slots| slots.get(block_idx))
            .copied()
        else {
            self.rebuild_render_cache_accounting();
            return;
        };

        if let Some(old_key) = Self::render_cache_slot_key(msg_idx, block_idx, &old_slot)
            && let Some(evictable) = self.render_cache_evictable_mut()
        {
            evictable.remove(&old_key);
        }
        let new_total = self
            .render_cache_total_bytes()
            .map_or(0, |bytes| bytes.saturating_sub(old_slot.cached_bytes));
        if let Some(total) = self.render_cache_total_bytes_mut() {
            *total = new_total;
        }
        if old_slot.protected {
            let new_protected = self
                .render_cache_protected_bytes()
                .map_or(0, |bytes| bytes.saturating_sub(old_slot.cached_bytes));
            if let Some(protected) = self.render_cache_protected_bytes_mut() {
                *protected = new_protected;
            }
        }

        let new_slot = if self.is_message_render_cache_slot(msg_idx, block_idx) {
            let Some(msg) = self.messages().and_then(|messages| messages.get(msg_idx)) else {
                self.rebuild_render_cache_accounting();
                return;
            };
            let (cached_bytes, last_access_tick) = self.message_cache_state(msg);
            RenderCacheSlotState {
                cached_bytes,
                last_access_tick,
                protected: self.is_render_cache_message_protected(msg_idx),
            }
        } else {
            let Some(block) = self
                .messages()
                .and_then(|messages| messages.get(msg_idx))
                .and_then(|msg| msg.blocks.get(block_idx))
            else {
                self.rebuild_render_cache_accounting();
                return;
            };
            let (cached_bytes, last_access_tick) = self.block_cache_state(block);
            RenderCacheSlotState {
                cached_bytes,
                last_access_tick,
                protected: self.is_render_cache_block_protected(msg_idx, block_idx),
            }
        };
        let mut needs_full_rebuild = false;
        if let Some(slots) = self.render_cache_slots_mut().and_then(|slots| slots.get_mut(msg_idx))
        {
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

        let new_total = self
            .render_cache_total_bytes()
            .map_or(0, |bytes| bytes.saturating_add(new_slot.cached_bytes));
        if let Some(total) = self.render_cache_total_bytes_mut() {
            *total = new_total;
        }
        if new_slot.protected {
            let new_protected = self
                .render_cache_protected_bytes()
                .map_or(0, |bytes| bytes.saturating_add(new_slot.cached_bytes));
            if let Some(protected) = self.render_cache_protected_bytes_mut() {
                *protected = new_protected;
            }
        } else if let Some(new_key) = Self::render_cache_slot_key(msg_idx, block_idx, &new_slot)
            && let Some(evictable) = self.render_cache_evictable_mut()
        {
            evictable.insert(new_key);
        }

        // The message slot's `protected` is derived from its blocks, so
        // a block whose protection flipped leaves it stale. Repairing it
        // here rather than at each status writer means a new writer
        // cannot get it wrong. Recursion stops at depth one: the message
        // slot is not a block slot, so it takes the other arm.
        if old_slot.protected != new_slot.protected
            && !self.is_message_render_cache_slot(msg_idx, block_idx)
            && let Some(message_slot_idx) =
                self.messages().and_then(|messages| messages.get(msg_idx)).map(|m| m.blocks.len())
        {
            self.sync_render_cache_slot(msg_idx, message_slot_idx);
        }
    }

    pub(crate) fn sync_render_cache_message(&mut self, msg_idx: usize) {
        if self.render_cache_accounting_suspended() {
            return;
        }
        self.ensure_render_cache_accounting();
        let Some(msg) = self.messages().and_then(|messages| messages.get(msg_idx)) else {
            self.rebuild_render_cache_accounting();
            return;
        };
        let block_count = msg.blocks.len();
        let slot_count = Self::render_cache_slot_count_for_message(msg);
        if self
            .render_cache_slots()
            .and_then(|slots| slots.get(msg_idx))
            .map_or(usize::MAX, Vec::len)
            != slot_count
        {
            self.rebuild_render_cache_accounting();
            return;
        }
        for block_idx in 0..block_count {
            self.sync_render_cache_slot(msg_idx, block_idx);
        }
        self.sync_render_cache_slot(msg_idx, block_count);
    }

    pub(crate) fn refresh_tail_message_cache_protection(&mut self) {
        if self.render_cache_accounting_suspended() {
            return;
        }
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
        let (Some(slots), Some(messages)) = (self.render_cache_slots(), self.messages()) else {
            return true;
        };
        slots
            .iter()
            .zip(messages.iter())
            .all(|(slots, msg)| slots.len() == Self::render_cache_slot_count_for_message(msg))
    }

    /// Rebuild if any message's slot row disagrees with its block
    /// count. The shared guard only compares list lengths, so a
    /// block-count change that skipped
    /// `sync_render_cache_message_tail` leaves one row stale and the
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
        if let Some(evictable) = self.render_cache_evictable_mut() {
            evictable.clear();
        }

        // Snapshot every slot's new state into a Vec so we never
        // have an immutable borrow on `self.messages()` while
        // mutating `self.render_cache_slots` /
        // `self.render_cache_evictable`.
        let mut updates: Vec<SlotUpdate> = Vec::new();
        let msg_count = self.messages().map_or(0, <[super::ChatMessage]>::len);
        let protected_tail = self.protected_streaming_message_idx();
        let mut block_protections: Vec<Vec<bool>> = Vec::with_capacity(msg_count);
        let mut message_protections: Vec<bool> = Vec::with_capacity(msg_count);
        for msg_idx in 0..msg_count {
            let tail_protected = protected_tail == Some(msg_idx);
            let Some(msg) = self.messages().and_then(|messages| messages.get(msg_idx)) else {
                block_protections.push(Vec::new());
                message_protections.push(false);
                continue;
            };
            let row: Vec<bool> = msg
                .blocks
                .iter()
                .map(|block| Self::block_is_render_cache_protected(tail_protected, Some(block)))
                .collect();
            let message_protected = tail_protected
                || msg.blocks.iter().any(|block| {
                    Self::block_is_render_cache_protected(tail_protected, Some(block))
                });
            block_protections.push(row);
            message_protections.push(message_protected);
        }
        for (msg_idx, msg) in self.messages().unwrap_or_default().iter().enumerate() {
            for (block_idx, block) in msg.blocks.iter().enumerate() {
                let (cached_bytes, last_access_tick) = self.block_cache_state(block);
                let protected = block_protections[msg_idx][block_idx];
                let slot = RenderCacheSlotState { cached_bytes, last_access_tick, protected };
                updates.push(SlotUpdate { msg_idx, block_idx, slot });
            }
            let message_slot_idx = msg.blocks.len();
            let (cached_bytes, last_access_tick) = self.message_cache_state(msg);
            let slot = RenderCacheSlotState {
                cached_bytes,
                last_access_tick,
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
            if let Some(slots) =
                self.render_cache_slots_mut().and_then(|slots| slots.get_mut(msg_idx))
                && let Some(existing) = slots.get_mut(block_idx)
            {
                *existing = slot;
            }
            total_bytes = total_bytes.saturating_add(slot.cached_bytes);
            if slot.protected {
                protected_bytes = protected_bytes.saturating_add(slot.cached_bytes);
            }
            if let Some(key) = Self::render_cache_slot_key(msg_idx, block_idx, &slot)
                && let Some(evictable) = self.render_cache_evictable_mut()
            {
                evictable.insert(key);
            }
        }
        if let Some(total) = self.render_cache_total_bytes_mut() {
            *total = total_bytes;
        }
        if let Some(protected) = self.render_cache_protected_bytes_mut() {
            *protected = protected_bytes;
        }
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
        stats.total_before_bytes = self.render_cache_total_bytes().unwrap_or(0);
        stats.protected_bytes = self.render_cache_protected_bytes().unwrap_or(0);

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
            if let Some(evictable) = self.render_cache_evictable_mut() {
                evictable.remove(&slot);
            }
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
        let Some(msg) = self.active_messages_mut().and_then(|messages| messages.get_mut(msg_idx))
        else {
            return 0;
        };
        if block_idx == msg.blocks.len() {
            // The key is copied out before the store is touched: the
            // message borrow and the store borrow are both `&mut self`.
            let message_id = msg.id;
            let removed = self.render_caches.evict_message(message_id);
            if removed > 0 {
                self.sync_render_cache_slot(msg_idx, block_idx);
            }
            return removed;
        }
        let Some(block) = msg.blocks.get_mut(block_idx) else {
            return 0;
        };
        // A tool call's cache is in the view store, and reaching it needs
        // `&mut self` while the message borrow above is live, so the id is
        // copied out and the message borrow ends here.
        if let MessageBlock::ToolCall(tc) = block {
            let id = tc.id.clone();
            let removed = self.render_caches.evict_tool_call(&id);
            if removed > 0 {
                self.sync_render_cache_slot(msg_idx, block_idx);
            }
            return removed;
        }
        // The id is copied out before the store is touched: the message
        // borrow and the store borrow are both `&mut self`.
        let block_id = match block {
            MessageBlock::Text(block) => Some(block.id),
            MessageBlock::Notice(block) => Some(block.text.id),
            _ => None,
        };
        if let Some(id) = block_id {
            let recorded = self
                .render_cache_slots()
                .and_then(|slots| slots.get(msg_idx))
                .and_then(|slots| slots.get(block_idx))
                .map_or(0, |slot| slot.cached_bytes);
            self.render_caches.evict_block(id);
            // The markdown cache rides with the block's rows: they are the
            // same render, and one without the other frees half a block.
            self.render_caches.evict_markdown(id);
            self.sync_render_cache_slot(msg_idx, block_idx);
            return recorded;
        }
        // Same copy-out as above: the welcome card is keyed by its content
        // and the store borrow cannot overlap the message borrow.
        if let MessageBlock::Welcome(welcome) = block {
            let signature = super::messages::hash_welcome_block_content(welcome);
            let recorded = self
                .render_cache_slots()
                .and_then(|slots| slots.get(msg_idx))
                .and_then(|slots| slots.get(block_idx))
                .map_or(0, |slot| slot.cached_bytes);
            self.render_caches.evict_welcome(signature);
            self.sync_render_cache_slot(msg_idx, block_idx);
            return recorded;
        }
        // An image-attachment block caches no rows.
        0
    }
}

#[cfg(test)]
mod tests {
    use ratatui::text::Line;

    use super::super::{
        App, AppStatus, CachedMessageSegment, ChatMessage, MessageBlock, MessageRenderCacheKey,
        MessageRenderSignature, MessageRole, SystemSeverity,
    };
    use crate::agent::model;
    use crate::app::state::tests::{
        assistant_bash_tool_message, assistant_text_block, assistant_tool_message, make_test_app,
    };
    use pretty_assertions::assert_eq;

    /// A block that streams keeps ONE store entry across every version of its
    /// text, not one per append. Keyed on content it would grow with the
    /// frames instead of with the session, and nothing could evict the
    /// superseded entries - the budget accounting resolves a slot's current
    /// key, so it would subtract bytes the store still holds.
    ///
    /// This pins the STORE's side. The caller's side is the type: `text_block`
    /// takes a `BlockId`, so a content signature cannot be passed for one.
    #[test]
    fn a_streaming_block_keeps_one_cache_entry() {
        use crate::app::state::render_cache_store::testing::block_entry_count;

        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        *app.active_messages_mut().expect("active session") =
            vec![ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("a")])];

        for chunk in ["a", "ab", "abc", "abcd", "abcde"] {
            let MessageBlock::Text(block) =
                &mut app.active_messages_mut().expect("active session")[0].blocks[0]
            else {
                panic!("expected a text block");
            };
            block.text = chunk.to_owned();
            let signature = block.content_signature();
            let id = block.id;
            caches.text_block(id, signature, false, false, 0).store(vec![Line::from(chunk)]);
            assert_eq!(block_entry_count(&app), 1, "one block entry after {chunk:?}");
        }
    }

    #[test]
    fn enforce_render_cache_budget_evicts_lru_block() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("a")]),
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("b")]),
        ];

        let bytes_a = if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[0].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("x".repeat(2200))]);
            caches.peek_block(block.id).cached_bytes()
        } else {
            0
        };
        let bytes_b = if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[1].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("y".repeat(2200))]);
            let _ = caches.peek_block(block.id).get();
            caches.peek_block(block.id).cached_bytes()
        } else {
            0
        };

        app.render_cache_budget.max_bytes = bytes_b;
        let stats = app.enforce_render_cache_budget();
        assert!(stats.evicted_blocks >= 1);
        assert!(stats.evicted_bytes >= bytes_a);
        assert!(stats.total_after_bytes <= app.render_cache_budget.max_bytes);
        assert_eq!(stats.protected_bytes, 0);

        if let MessageBlock::Text(block) = &app.messages().expect("active session")[0].blocks[0] {
            assert_eq!(caches.peek_block(block.id).cached_bytes(), 0);
        } else {
            panic!("expected text block");
        }
        if let MessageBlock::Text(block) = &app.messages().expect("active session")[1].blocks[0] {
            assert_eq!(caches.peek_block(block.id).cached_bytes(), bytes_b);
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn enforce_render_cache_budget_protects_streaming_tail_message() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        app.status = AppStatus::Thinking;
        *app.active_messages_mut().expect("active session") = vec![ChatMessage::new(
            MessageRole::Assistant,
            vec![assistant_text_block("streaming tail")],
        )];

        let before = if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[0].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("z".repeat(4096))]);
            caches.peek_block(block.id).cached_bytes()
        } else {
            0
        };
        app.render_cache_budget.max_bytes = 64;
        let stats = app.enforce_render_cache_budget();
        assert_eq!(stats.evicted_blocks, 0);
        assert_eq!(stats.evicted_bytes, 0);
        assert_eq!(stats.protected_bytes, before);

        if let MessageBlock::Text(block) = &app.messages().expect("active session")[0].blocks[0] {
            assert_eq!(caches.peek_block(block.id).cached_bytes(), before);
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn enforce_render_cache_budget_excludes_protected_from_budget() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        app.status = AppStatus::Running;
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("old message")]),
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("streaming tail")]),
        ];

        let bytes_a = if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[0].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("x".repeat(2200))]);
            caches.peek_block(block.id).cached_bytes()
        } else {
            0
        };
        let bytes_b = if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[1].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("y".repeat(5000))]);
            caches.peek_block(block.id).cached_bytes()
        } else {
            0
        };

        // Budget fits old message alone but not old + tail combined.
        app.render_cache_budget.max_bytes = bytes_a + 100;
        assert!(bytes_a + bytes_b > app.render_cache_budget.max_bytes);

        let stats = app.enforce_render_cache_budget();

        // Protected bytes should be the streaming tail.
        assert_eq!(stats.protected_bytes, bytes_b);
        // No eviction: budgeted bytes (bytes_a) are under max_bytes.
        assert_eq!(stats.evicted_blocks, 0);
        assert_eq!(stats.evicted_bytes, 0);
        // Old message cache intact.
        if let MessageBlock::Text(block) = &app.messages().expect("active session")[0].blocks[0] {
            assert_eq!(caches.peek_block(block.id).cached_bytes(), bytes_a);
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn enforce_render_cache_budget_protects_active_streaming_owner_not_physical_tail() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        app.status = AppStatus::Running;
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("old message")]),
            ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block("active streaming owner")],
            ),
            ChatMessage::new(
                MessageRole::System(Some(SystemSeverity::Info)),
                vec![assistant_text_block("late trailing system row")],
            ),
        ];
        app.bind_active_turn_assistant(1);

        if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[0].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("x".repeat(2000))]);
        }
        let protected_bytes = if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[1].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("y".repeat(4000))]);
            caches.peek_block(block.id).cached_bytes()
        } else {
            0
        };
        if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[2].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("z".repeat(5000))]);
        }

        app.render_cache_budget.max_bytes = 64;
        let stats = app.enforce_render_cache_budget();

        assert_eq!(stats.protected_bytes, protected_bytes);
    }

    #[test]
    fn enforce_render_cache_budget_evicts_when_budgeted_over_limit() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        app.status = AppStatus::Running;
        *app.active_messages_mut().expect("active session") = vec![
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("old-a")]),
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("old-b")]),
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("streaming")]),
        ];

        // Populate caches: messages 0 and 1 evictable, message 2 protected.
        if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[0].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("x".repeat(3000))]);
        }
        let bytes_b = if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[1].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("y".repeat(3000))]);
            let _ = caches.peek_block(block.id).get(); // touch to make more recently accessed
            caches.peek_block(block.id).cached_bytes()
        } else {
            0
        };
        let bytes_c = if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[2].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("z".repeat(5000))]);
            caches.peek_block(block.id).cached_bytes()
        } else {
            0
        };

        // Budget fits message B but not A+B (excludes C as protected).
        app.render_cache_budget.max_bytes = bytes_b + 100;

        let stats = app.enforce_render_cache_budget();

        assert_eq!(stats.protected_bytes, bytes_c);
        assert!(stats.evicted_blocks >= 1); // message A evicted (older access)
        // Message B should survive (more recent access).
        if let MessageBlock::Text(block) = &app.messages().expect("active session")[1].blocks[0] {
            assert_eq!(caches.peek_block(block.id).cached_bytes(), bytes_b);
        } else {
            panic!("expected text block");
        }
    }

    #[test]
    fn enforce_render_cache_budget_protected_bytes_zero_when_not_streaming() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        app.status = AppStatus::Ready;
        *app.active_messages_mut().expect("active session") =
            vec![ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("done")])];

        if let MessageBlock::Text(block) =
            &mut app.active_messages_mut().expect("active session")[0].blocks[0]
        {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("x".repeat(2000))]);
        }
        app.render_cache_budget.max_bytes = usize::MAX;

        let stats = app.enforce_render_cache_budget();
        assert_eq!(stats.protected_bytes, 0);
    }

    #[test]
    fn enforce_render_cache_budget_accounts_for_message_render_cache() {
        let mut app = make_test_app();
        let msg_a =
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block(&"a".repeat(4000))]);
        let msg_b =
            ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block(&"b".repeat(4000))]);
        // Seeded rather than rendered: the message-level cache lives in the
        // view's store now, and the measure helper builds its layout into a
        // local store-less cache, so a render would leave these at zero.
        store_message_render_cache(&app, &msg_a, 4096);
        store_message_render_cache(&app, &msg_b, 4096);
        *app.active_messages_mut().expect("active session") = vec![msg_a, msg_b];

        let bytes_a = message_cache_bytes(&app, 0);
        let bytes_b = message_cache_bytes(&app, 1);
        assert!(bytes_a > 0);
        assert!(bytes_b > 0);

        app.rebuild_render_cache_accounting();
        app.render_cache_budget.max_bytes = bytes_b;
        let stats = app.enforce_render_cache_budget();

        assert!(stats.evicted_bytes >= bytes_a);
        assert!(message_cache_bytes(&app, 0) == 0 || message_cache_bytes(&app, 1) == 0);
    }

    #[test]
    fn push_path_defers_render_cache_rebuild_until_read() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        app.status = AppStatus::Ready;

        for i in 0..8 {
            let mut msg =
                assistant_bash_tool_message(&format!("t{i}"), model::ToolCallStatus::Completed);
            if let MessageBlock::ToolCall(tc) = &mut msg.blocks[0] {
                caches.tool_call(&tc.id, tc.render_epoch).store(vec![Line::from("x".repeat(2048))]);
            }
            app.push_message_tracked(msg);
        }

        // Deferred to the lazy guard, the append path never rebuilds, so an
        // empty slot grid after 8 appends proves no per-append rebuild fired.
        assert_eq!(app.messages().expect("active session").len(), 8);
        assert_eq!(app.render_cache_slots().expect("active session").len(), 0);

        app.ensure_render_cache_accounting();
        assert_eq!(
            app.render_cache_slots().expect("active session").len(),
            app.messages().expect("active session").len()
        );
    }

    #[test]
    fn push_path_accounting_matches_full_rebuild() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        app.status = AppStatus::Running;

        for i in 0..3 {
            let mut text = ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block(&format!("row {i}"))],
            );
            if let MessageBlock::Text(block) = &mut text.blocks[0] {
                caches
                    .text_block(block.id, block.content_signature(), false, false, 0)
                    .store(vec![Line::from("t".repeat(1500 + i * 200))]);
                if i % 2 == 0 {
                    let _ = caches.peek_block(block.id).get();
                }
            }
            app.push_message_tracked(text);

            let mut tool =
                assistant_bash_tool_message(&format!("done{i}"), model::ToolCallStatus::Completed);
            if let MessageBlock::ToolCall(tc) = &mut tool.blocks[0] {
                caches
                    .tool_call(&tc.id, tc.render_epoch)
                    .store(vec![Line::from("o".repeat(2200 + i * 100))]);
            }
            app.push_message_tracked(tool);
        }

        let mut trailing = assistant_tool_message("live", model::ToolCallStatus::InProgress);
        if let MessageBlock::ToolCall(tc) = &mut trailing.blocks[0] {
            caches.tool_call(&tc.id, tc.render_epoch).store(vec![Line::from("p".repeat(3000))]);
        }
        app.push_message_tracked(trailing);

        app.ensure_render_cache_accounting();
        let slots = app.render_cache_slots().expect("active session").to_vec();
        let total = app.render_cache_total_bytes().expect("active session");
        let protected = app.render_cache_protected_bytes().expect("active session");
        let evictable = app.render_cache_evictable().cloned().unwrap_or_default();
        let tail = app.render_cache_tail_msg_idx();

        assert!(total > 0);
        assert!(protected > 0);
        assert!(!evictable.is_empty());

        app.rebuild_render_cache_accounting();
        assert_eq!(app.render_cache_slots().expect("active session").to_vec(), slots);
        assert_eq!(app.render_cache_total_bytes().expect("active session"), total);
        assert_eq!(app.render_cache_protected_bytes().expect("active session"), protected);
        assert_eq!(app.render_cache_evictable().cloned().unwrap_or_default(), evictable);
        assert_eq!(app.render_cache_tail_msg_idx(), tail);
    }

    /// A backlog of plain messages with the accounting built, plus one
    /// tail message a block can be appended to.
    fn app_with_backlog(n: usize) -> App {
        let mut app = App::test_default();
        for _ in 0..n {
            let mut msg = ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block(&"x".repeat(400))],
            );
            if let MessageBlock::Text(block) = &mut msg.blocks[0] {
                app.render_caches
                    .text_block(block.id, block.content_signature(), false, false, 0)
                    .store(vec![Line::from("y".repeat(256))]);
            }
            // Message slots must carry bytes too. With them at zero, any
            // assertion about a message slot compares zero against zero
            // and cannot see a mutation that drops or zeroes one.
            store_message_render_cache(&app, &msg, 64);
            app.push_message_tracked(msg);
        }
        // Appends land mid-turn, which is also what makes the tail the
        // protected message. A Ready fixture leaves protected bytes at
        // zero and the tail at None, so byte-equality assertions on it
        // compare zero against zero.
        app.status = AppStatus::Running;
        let tail = app.messages().expect("active session").len().saturating_sub(1);
        app.bind_active_turn_assistant(tail);
        app.ensure_render_cache_accounting();
        app.ensure_history_retention_accounting();
        app
    }

    /// Seed a message-level render cache so the message slot carries
    /// bytes; a zero-byte message slot makes protected-byte drift
    /// invisible.
    fn store_message_render_cache(app: &App, msg: &ChatMessage, bytes: usize) {
        let mut cache = app.render_caches.message(msg.id);
        cache.store(
            MessageRenderCacheKey {
                width: 80,
                layout_generation: 0,
                tools_collapsed: false,
                include_trailing_separator: false,
                stop_hook_summary_actions: 0,
                stop_hook_summary_expanded: false,
                render_signature: MessageRenderSignature(0),
            },
            vec![CachedMessageSegment::Lines {
                lines: vec![Line::from("m".repeat(bytes))],
                height: 1,
            }],
            1,
            1,
            Vec::new(),
            Vec::new(),
        );
    }

    /// The message-level bytes the budget would read for `idx`.
    fn message_cache_bytes(app: &App, idx: usize) -> usize {
        let Some(msg) = app.messages().and_then(|messages| messages.get(idx)) else {
            return 0;
        };
        app.message_cache_state(msg).0
    }

    /// Budget enforcement re-derives both byte totals while it walks
    /// every slot, so accumulated drift cannot outlive one enforcement.
    /// It writes back only flags otherwise, and the byte counts are what
    /// the budget comparison uses.
    #[test]
    fn budget_enforcement_rederives_protected_bytes_from_the_slots_it_walks() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        let mut owner = assistant_tool_message("toolu_bg", model::ToolCallStatus::InProgress);
        if let MessageBlock::ToolCall(tc) = &mut owner.blocks[0] {
            caches.tool_call(&tc.id, tc.render_epoch).store(vec![Line::from("t".repeat(2048))]);
        }
        store_message_render_cache(&app, &owner, 2048);
        app.push_message_tracked(owner);
        // Plenty of UNPROTECTED bytes, so the injected drift cannot by
        // itself push the budget comparison under the limit. A drift big
        // enough to do that skips eviction entirely, which is the leak's
        // user impact rather than this function's contract.
        let mut bulk = ChatMessage::new(MessageRole::Assistant, vec![assistant_text_block("bulk")]);
        if let MessageBlock::Text(block) = &mut bulk.blocks[0] {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("b".repeat(60_000))]);
        }
        app.push_message_tracked(bulk);
        app.ensure_render_cache_accounting();

        // Inject drift directly: a protected-byte count that no slot
        // justifies, which is what an unrepaired derived flag leaves.
        *app.render_cache_protected_bytes_mut().expect("active session") =
            app.render_cache_protected_bytes().expect("active session") + 1_000;
        app.render_cache_budget.max_bytes = 64;
        let _ = app.enforce_render_cache_budget();

        let after = app.render_cache_protected_bytes().expect("active session");
        app.rebuild_render_cache_accounting();
        assert_eq!(
            after,
            app.render_cache_protected_bytes().expect("active session"),
            "enforcement must re-derive the totals it decides on, not carry drift forward",
        );
    }

    /// Appending to one message must not absorb an unannounced change in
    /// a DIFFERENT message. The whole-session rebuild this replaced did
    /// absorb it, so re-adding that rebuild is invisible to a timing
    /// guard - this catches it on behaviour instead.
    #[test]
    fn appending_does_not_absorb_an_unrelated_messages_growth() {
        let mut app = app_with_backlog(6);
        let tail = app.messages().expect("active session").len() - 1;
        let before = app.render_cache_total_bytes().expect("active session");

        // Grow a NON-target message without announcing it.
        let mut stowaway = assistant_text_block("grown out of band");
        if let MessageBlock::Text(block) = &mut stowaway {
            app.render_caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("g".repeat(8192))]);
        }
        app.active_messages_mut().expect("active session")[1].blocks.push(stowaway);

        // Append to the tail and sync it.
        app.active_messages_mut().expect("active session")[tail]
            .blocks
            .push(assistant_text_block("chunk"));
        app.sync_after_message_tail_changed(tail);
        let after_append = app.render_cache_total_bytes().expect("active session");

        app.rebuild_render_cache_accounting();
        let truth = app.render_cache_total_bytes().expect("active session");
        assert!(
            truth > before,
            "fixture must actually grow the unrelated message, or this test proves nothing",
        );
        assert!(
            after_append < truth,
            "appending to the tail must not pick up message 1's unannounced growth; it did, so \
             the append path is walking the whole session again",
        );
    }

    /// The tail path must NOT be used where every slot can change. A
    /// protection flip re-evaluates the whole message, so
    /// `refresh_tail_message_cache_protection` needs the full sync -
    /// which is the collapse a reader is most likely to make on sight of
    /// two near-identical functions.
    #[test]
    fn tail_protection_refresh_resyncs_every_slot_not_just_the_tail() {
        let mut app = make_test_app();
        let mut owner = ChatMessage::new(
            MessageRole::Assistant,
            vec![
                assistant_text_block("first"),
                assistant_text_block("second"),
                assistant_text_block("third"),
            ],
        );
        for block in &mut owner.blocks {
            if let MessageBlock::Text(b) = block {
                app.render_caches
                    .text_block(b.id, b.content_signature(), false, false, 0)
                    .store(vec![Line::from("p".repeat(1024))]);
            }
        }
        app.push_message_tracked(owner);
        app.status = AppStatus::Running;
        app.bind_active_turn_assistant(0);
        app.ensure_render_cache_accounting();
        assert!(
            app.render_cache_slots().expect("active session")[0].iter().all(|s| s.protected),
            "the streaming tail protects every slot in the message",
        );

        // Turn ends: the tail is no longer protected, so EVERY slot's
        // flag has to change, not just the last one.
        app.status = AppStatus::Ready;
        app.refresh_tail_message_cache_protection();
        assert!(
            app.render_cache_slots().expect("active session")[0].iter().all(|s| !s.protected),
            "every slot must lose protection when the tail moves off the message; the tail-only \
             sync leaves the earlier slots protected",
        );
        let after = app.render_cache_protected_bytes().expect("active session");
        app.rebuild_render_cache_accounting();
        assert_eq!(
            after,
            app.render_cache_protected_bytes().expect("active session"),
            "protected bytes diverged"
        );
    }

    /// The resize fallback for a MOVED protected tail. A moved tail flips
    /// `protected` on rows the row resize does not touch, so the resize
    /// has to hand off to the whole-session pass.
    #[test]
    fn append_after_the_protected_tail_moved_falls_back_to_a_full_rebuild() {
        let mut app = app_with_backlog(4);
        let caches = std::rc::Rc::clone(&app.render_caches);
        let tail = app.messages().expect("active session").len() - 1;
        // Enough blocks that the tail sync leaves earlier slots alone:
        // with one block every slot gets re-synced and any stale
        // protection is repaired incidentally.
        for i in 1..12 {
            let mut b = assistant_text_block(&format!("block {i}"));
            if let MessageBlock::Text(block) = &mut b {
                caches
                    .text_block(block.id, block.content_signature(), false, false, 0)
                    .store(vec![Line::from("p".repeat(512))]);
            }
            app.active_messages_mut().expect("active session")[tail].blocks.push(b);
        }
        app.rebuild_render_cache_accounting();

        // Move the tail WITHOUT repairing the rows. The old tail's rows
        // still say protected, so a row resize that trusted the current
        // tail would mix two protection regimes in one table.
        app.bind_active_turn_assistant(tail - 1);

        app.active_messages_mut().expect("active session")[tail]
            .blocks
            .push(assistant_text_block("chunk"));
        app.sync_after_message_tail_changed(tail);

        let after = app.render_cache_protected_bytes().expect("active session");
        let total = app.render_cache_total_bytes().expect("active session");
        app.rebuild_render_cache_accounting();
        assert_eq!(
            after,
            app.render_cache_protected_bytes().expect("active session"),
            "protected bytes diverged"
        );
        assert_eq!(
            total,
            app.render_cache_total_bytes().expect("active session"),
            "totals diverged"
        );
    }

    /// Everything else here syncs the streaming TAIL, whose slots are
    /// protected and therefore carry no eviction key at all - so a
    /// mutation dropping a key removal has no key to drop. These two
    /// target an UNPROTECTED message so the keys exist to be mishandled.
    ///
    /// Growing a row moves the message slot's index, so its key at the
    /// OLD index has to be removed or the eviction order keeps an entry
    /// pointing at a slot that has shifted underneath it.
    #[test]
    fn growing_an_unprotected_message_moves_its_message_slot_key() {
        let mut app = app_with_backlog(6);
        let caches = std::rc::Rc::clone(&app.render_caches);
        let target = 1usize; // not the tail, so its slots are evictable
        assert!(
            app.render_cache_slots().expect("active session")[target].iter().all(|s| !s.protected),
            "fixture must leave a non-tail message unprotected, or its slots carry no keys",
        );
        assert!(
            app.render_cache_slots().expect("active session")[target]
                .last()
                .is_some_and(|s| s.cached_bytes > 0),
            "the message slot must carry bytes, or its key does not exist to be moved",
        );

        let mut extra = assistant_text_block("appended to a non-tail message");
        if let MessageBlock::Text(block) = &mut extra {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("k".repeat(1500))]);
        }
        app.active_messages_mut().expect("active session")[target].blocks.push(extra);
        app.sync_after_message_tail_changed(target);

        let slots = app.render_cache_slots().expect("active session").to_vec();
        let total = app.render_cache_total_bytes().expect("active session");
        let protected = app.render_cache_protected_bytes().expect("active session");
        let evictable = app.render_cache_evictable().cloned().unwrap_or_default();
        app.rebuild_render_cache_accounting();
        assert_eq!(
            slots,
            app.render_cache_slots().expect("active session").to_vec(),
            "slot rows diverged"
        );
        assert_eq!(
            total,
            app.render_cache_total_bytes().expect("active session"),
            "totals diverged"
        );
        assert_eq!(
            protected,
            app.render_cache_protected_bytes().expect("active session"),
            "protected bytes diverged"
        );
        assert_eq!(
            evictable,
            app.render_cache_evictable().cloned().unwrap_or_default(),
            "eviction order diverged: a key survived at an index its slot no longer occupies",
        );
    }

    /// Shrinking drops block slots outright, so their keys have to go
    /// with them. A surviving key points the eviction order at a slot
    /// that no longer exists.
    #[test]
    fn shrinking_an_unprotected_message_drops_its_block_slot_keys() {
        let mut app = app_with_backlog(6);
        let caches = std::rc::Rc::clone(&app.render_caches);
        let target = 1usize;
        for i in 0..3 {
            let mut extra = assistant_text_block(&format!("doomed {i}"));
            if let MessageBlock::Text(block) = &mut extra {
                caches
                    .text_block(block.id, block.content_signature(), false, false, 0)
                    .store(vec![Line::from("d".repeat(900 + i * 100))]);
            }
            app.active_messages_mut().expect("active session")[target].blocks.push(extra);
        }
        app.sync_after_message_tail_changed(target);
        assert!(
            app.render_cache_slots().expect("active session")[target]
                .iter()
                .any(|s| s.cached_bytes > 0 && !s.protected),
            "the dropped slots must be evictable, or there is no key to drop",
        );

        for _ in 0..3 {
            app.active_messages_mut().expect("active session")[target].blocks.pop();
        }
        app.sync_after_message_tail_changed(target);

        let slots = app.render_cache_slots().expect("active session").to_vec();
        let total = app.render_cache_total_bytes().expect("active session");
        let protected = app.render_cache_protected_bytes().expect("active session");
        let evictable = app.render_cache_evictable().cloned().unwrap_or_default();
        app.rebuild_render_cache_accounting();
        assert_eq!(
            slots,
            app.render_cache_slots().expect("active session").to_vec(),
            "slot rows diverged"
        );
        assert_eq!(
            total,
            app.render_cache_total_bytes().expect("active session"),
            "totals diverged"
        );
        assert_eq!(
            protected,
            app.render_cache_protected_bytes().expect("active session"),
            "protected bytes diverged"
        );
        assert_eq!(
            evictable,
            app.render_cache_evictable().cloned().unwrap_or_default(),
            "eviction order diverged: a dropped slot's key outlived the slot",
        );
    }

    /// A message slot's `protected` flag is DERIVED from its blocks -
    /// true when any block holds a Pending/InProgress tool call. Three
    /// writers flip tool status and sync only the block slot, so the
    /// message slot's flag has to be repaired by whoever notices.
    ///
    /// It used to be repaired incidentally: every append anywhere in the
    /// session rebuilt the whole accounting, several times a second
    /// during a turn. Once appends stopped doing that, the derived flag
    /// lost its only invalidation edge and the bytes behind it stayed
    /// counted as protected forever, raising the eviction threshold for
    /// the rest of the session.
    #[test]
    fn clearing_a_tool_calls_protection_releases_the_messages_protected_bytes() {
        let mut app = make_test_app();
        let caches = std::rc::Rc::clone(&app.render_caches);
        // A settled message with an in-flight tool call, then a later
        // message so the streaming-tail rule does not apply to it.
        let mut owner = assistant_tool_message("toolu_bg", model::ToolCallStatus::InProgress);
        if let MessageBlock::ToolCall(tc) = &mut owner.blocks[0] {
            caches.tool_call(&tc.id, tc.render_epoch).store(vec![Line::from("t".repeat(600))]);
        }
        store_message_render_cache(&app, &owner, 400);
        app.push_message_tracked(owner);
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![assistant_text_block("later message")],
        ));
        app.ensure_render_cache_accounting();
        assert!(
            app.render_cache_protected_bytes().expect("active session") > 0,
            "the in-flight call protects the message"
        );

        // The background task settles. This is what the three writers do:
        // flip the status, sync the BLOCK slot.
        if let Some(MessageBlock::ToolCall(tc)) =
            app.active_messages_mut().expect("active session")[0].blocks.get_mut(0)
        {
            tc.status = model::ToolCallStatus::Completed;
        }
        app.sync_render_cache_slot(0, 0);

        let stale = app.render_cache_protected_bytes().expect("active session");
        app.rebuild_render_cache_accounting();
        assert_eq!(
            stale,
            app.render_cache_protected_bytes().expect("active session"),
            "protected bytes drifted (stale {stale} vs truth {}); the message slot kept a \
             protection its blocks no longer justify, so those bytes are permanently excluded \
             from the eviction budget",
            app.render_cache_protected_bytes().expect("active session"),
        );
    }

    /// Appending re-syncs the TAIL slots and only those. That is the
    /// whole two-mode split: syncing every slot made the Nth append
    /// re-sync all N, so a merged 200-envelope run spent 7.98ms in
    /// accounting against 0.58ms now.
    ///
    /// Asserted on scope rather than on time. A timing bar cannot
    /// separate the two modes: in the protected regime a full per-slot
    /// sync is nearly free because protected slots carry no eviction
    /// key, and in the unprotected regime the message slot's protection
    /// check walks every block either way. Scope is the property that
    /// actually differs.
    #[test]
    fn appending_resyncs_only_the_tail_slots() {
        let mut app = app_with_backlog(6);
        let caches = std::rc::Rc::clone(&app.render_caches);
        let tail = app.messages().expect("active session").len() - 1;
        for i in 1..8 {
            let mut b = assistant_text_block(&format!("block {i}"));
            if let MessageBlock::Text(block) = &mut b {
                caches
                    .text_block(block.id, block.content_signature(), false, false, 0)
                    .store(vec![Line::from("z".repeat(256))]);
            }
            app.active_messages_mut().expect("active session")[tail].blocks.push(b);
        }
        app.rebuild_render_cache_accounting();

        // Mark every slot so a re-sync is observable: a synced slot picks
        // its tick back up from the block's cache, an untouched one keeps
        // the sentinel.
        let sentinel = u64::MAX;
        for slot in &mut app.render_cache_slots_mut().expect("active session")[tail] {
            slot.last_access_tick = sentinel;
        }

        app.active_messages_mut().expect("active session")[tail]
            .blocks
            .push(assistant_text_block("appended"));
        app.sync_after_message_tail_changed(tail);

        let row = &app.render_cache_slots().expect("active session")[tail];
        let previously_last = 7usize;
        let untouched =
            (0..previously_last).filter(|&i| row[i].last_access_tick == sentinel).count();
        assert_eq!(
            untouched,
            previously_last,
            "appending must not re-sync the {previously_last} slots before the tail; \
             {} of them were re-synced, so this is the full per-slot sync again",
            previously_last - untouched,
        );
        assert!(
            row[previously_last].last_access_tick != sentinel
                && row[row.len() - 1].last_access_tick != sentinel,
            "the previously-last block and the message slot must both be re-synced",
        );
    }

    /// Appending a block to one message must leave the render-cache
    /// accounting byte-identical to a full rebuild. This is the contract
    /// the incremental row rebuild replaced an unconditional
    /// whole-session rebuild with, so it is the test that makes the
    /// change safe.
    #[test]
    fn appending_a_block_leaves_accounting_matching_a_full_rebuild() {
        let mut app = app_with_backlog(6);
        let caches = std::rc::Rc::clone(&app.render_caches);
        let tail = app.messages().expect("active session").len() - 1;

        let mut extra = assistant_text_block("appended chunk");
        if let MessageBlock::Text(block) = &mut extra {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("z".repeat(1024))]);
        }
        app.active_messages_mut().expect("active session")[tail].blocks.push(extra);
        app.sync_after_message_tail_changed(tail);

        let slots = app.render_cache_slots().expect("active session").to_vec();
        let total = app.render_cache_total_bytes().expect("active session");
        let protected = app.render_cache_protected_bytes().expect("active session");
        let evictable = app.render_cache_evictable().cloned().unwrap_or_default();
        let tail_idx = app.render_cache_tail_msg_idx();
        assert_eq!(
            slots[tail].len(),
            app.messages().expect("active session")[tail].blocks.len() + 1
        );

        app.rebuild_render_cache_accounting();
        assert_eq!(
            slots,
            app.render_cache_slots().expect("active session").to_vec(),
            "slot rows diverged"
        );
        assert_eq!(
            total,
            app.render_cache_total_bytes().expect("active session"),
            "total bytes diverged"
        );
        assert_eq!(
            protected,
            app.render_cache_protected_bytes().expect("active session"),
            "protected bytes diverged"
        );
        assert_eq!(
            evictable,
            app.render_cache_evictable().cloned().unwrap_or_default(),
            "eviction order diverged",
        );
        assert_eq!(tail_idx, app.render_cache_tail_msg_idx(), "protected tail diverged");
    }

    /// Same contract when a block is REMOVED, which reshapes the row the
    /// other way and has to drop the vanished slot's bytes and its
    /// eviction key.
    #[test]
    fn removing_a_block_leaves_accounting_matching_a_full_rebuild() {
        let mut app = app_with_backlog(6);
        let caches = std::rc::Rc::clone(&app.render_caches);
        let tail = app.messages().expect("active session").len() - 1;
        let mut extra = assistant_text_block("doomed");
        if let MessageBlock::Text(block) = &mut extra {
            caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("q".repeat(2048))]);
        }
        app.active_messages_mut().expect("active session")[tail].blocks.push(extra);
        app.sync_after_message_tail_changed(tail);

        app.active_messages_mut().expect("active session")[tail].blocks.pop();
        app.sync_after_message_tail_changed(tail);

        let slots = app.render_cache_slots().expect("active session").to_vec();
        let total = app.render_cache_total_bytes().expect("active session");
        let protected = app.render_cache_protected_bytes().expect("active session");
        let evictable = app.render_cache_evictable().cloned().unwrap_or_default();
        app.rebuild_render_cache_accounting();
        assert_eq!(
            slots,
            app.render_cache_slots().expect("active session").to_vec(),
            "slot rows diverged"
        );
        assert_eq!(
            total,
            app.render_cache_total_bytes().expect("active session"),
            "removed block's bytes not dropped"
        );
        assert_eq!(
            protected,
            app.render_cache_protected_bytes().expect("active session"),
            "protected bytes diverged"
        );
        assert_eq!(
            evictable,
            app.render_cache_evictable().cloned().unwrap_or_default(),
            "removed block's eviction key not dropped",
        );
    }

    /// The append path runs on every streamed text chunk, so the
    /// render-cache accounting it maintains must not cost more the more
    /// scrollback sits behind it. It used to rebuild the whole session's
    /// accounting to service a change to one message.
    ///
    /// Scoped to the accounting, NOT to `sync_after_message_tail_changed`
    /// as a whole: that also calls `invalidate_layout`, which is
    /// separately linear in message count (#490) and deliberately not
    /// fixed here. Including it would mean picking a threshold that
    /// tolerates a known scan, which is not a guard.
    ///
    /// Ratio between two backlog sizes rather than a wall-clock budget,
    /// so a loaded machine slows both together and cannot flake it.
    #[test]
    fn append_accounting_does_not_scale_with_backlog() {
        const MAX_RATIO: f64 = 3.0;
        const ROUNDS: usize = 40;

        fn best_us(n: usize) -> f64 {
            let mut app = app_with_backlog(n);
            let tail = app.messages().expect("active session").len() - 1;
            app.active_messages_mut().expect("active session")[tail]
                .blocks
                .push(assistant_text_block("chunk"));
            let sync = |app: &mut App| {
                app.sync_render_cache_message_tail(tail);
                app.recompute_message_retained_bytes(tail);
            };
            sync(&mut app);
            (0..ROUNDS)
                .map(|_| {
                    let start = std::time::Instant::now();
                    sync(&mut app);
                    start.elapsed().as_secs_f64() * 1e6
                })
                .fold(f64::MAX, f64::min)
        }

        let small = best_us(250);
        let large = best_us(4_000);
        let ratio = large / small;
        assert!(
            ratio < MAX_RATIO,
            "maintaining one message's accounting must not cost more because other messages \
             exist, got {ratio:.2}x for a 16x longer backlog (limit {MAX_RATIO}x); \
             {small:.2}us -> {large:.2}us",
        );
    }

    /// Seed messages carrying cached bytes, with accounting built.
    fn app_with_cached_messages(count: usize) -> App {
        let mut app = make_test_app();
        for i in 0..count {
            let mut msg = ChatMessage::new(
                MessageRole::Assistant,
                vec![assistant_text_block(&format!("row {i}"))],
            );
            if let MessageBlock::Text(block) = &mut msg.blocks[0] {
                app.render_caches
                    .text_block(block.id, block.content_signature(), false, false, 0)
                    .store(vec![Line::from("x".repeat(1024))]);
            }
            app.push_message_tracked(msg);
        }
        app.ensure_render_cache_accounting();
        app
    }

    /// Append a cached block without going through
    /// `sync_after_message_tail_changed`, which is the notification
    /// that would normally rebuild.
    fn append_block_out_of_band(app: &mut App, msg_idx: usize) {
        let mut extra = assistant_text_block("appended out of band");
        if let MessageBlock::Text(block) = &mut extra {
            app.render_caches
                .text_block(block.id, block.content_signature(), false, false, 0)
                .store(vec![Line::from("y".repeat(4096))]);
        }
        app.active_messages_mut().expect("active session")[msg_idx].blocks.push(extra);
    }

    /// The same unannounced block-count change, carrying no bytes, so
    /// the row drift lands without moving the total. Lets a test move
    /// `protected` on its own.
    fn append_uncached_block_out_of_band(app: &mut App, msg_idx: usize) {
        app.active_messages_mut().expect("active session")[msg_idx]
            .blocks
            .push(assistant_text_block("appended out of band"));
    }

    /// The shared guard only compares list lengths, so the per-message
    /// slot-count check has to live at each sync entry point.
    /// `sync_render_cache_slot` is the one with external callers
    /// (terminal, tool updates, tool calls, notices), and unlike
    /// `sync_render_cache_message` it has no earlier short-circuit, so
    /// it is the entry point that actually reaches the check.
    #[test]
    fn syncing_a_slot_repairs_accounting_after_an_unnotified_block_change() {
        let mut app = app_with_cached_messages(4);
        append_block_out_of_band(&mut app, 1);

        app.sync_render_cache_slot(1, 0);

        let after_sync = app.render_cache_total_bytes().expect("active session");
        app.rebuild_render_cache_accounting();
        assert_eq!(
            after_sync,
            app.render_cache_total_bytes().expect("active session"),
            "syncing a slot in the changed message must leave the totals a rebuild produces",
        );
        assert_eq!(
            app.render_cache_slots().expect("active session")[1].len(),
            app.messages().expect("active session")[1].blocks.len() + 1
        );
    }

    /// What the narrowed guard gives up, pinned rather than left
    /// unstated: syncing a DIFFERENT message does not notice message
    /// 1's unannounced change, so the totals stay short until something
    /// touches message 1. Budget enforcement closes this itself - see
    /// the eviction test below.
    #[test]
    fn syncing_an_unrelated_slot_leaves_the_drift_in_place() {
        let mut app = app_with_cached_messages(4);
        append_block_out_of_band(&mut app, 1);

        app.sync_render_cache_slot(3, 0);

        let drifted = app.render_cache_total_bytes().expect("active session");
        app.rebuild_render_cache_accounting();
        assert!(
            drifted < app.render_cache_total_bytes().expect("active session"),
            "documents the narrowing: an unrelated sync leaves the new block uncounted",
        );
    }

    /// The drift does not reach any budget decision, including the
    /// decision NOT to evict. A short total makes the under-budget
    /// branch fire when it should not, so the repair runs before the
    /// totals are read rather than before eviction - deliberately the
    /// cheaper-looking branch, because it is the uncovered one.
    ///
    /// No `max_bytes` tampering here: staying under budget is the case
    /// that used to slip through.
    #[test]
    fn budget_enforcement_repairs_drift_before_reading_totals() {
        let mut app = app_with_cached_messages(4);
        append_block_out_of_band(&mut app, 1);
        app.sync_render_cache_slot(3, 0);

        let drifted = app.render_cache_total_bytes().expect("active session");
        let stats = app.enforce_render_cache_budget();

        assert!(
            stats.total_before_bytes > drifted,
            "the totals the budget compares against must be re-derived, not the drifted ones",
        );
        let after = app.render_cache_total_bytes().expect("active session");
        app.rebuild_render_cache_accounting();
        assert_eq!(
            after,
            app.render_cache_total_bytes().expect("active session"),
            "the totals enforcement acted on match a full rebuild",
        );
    }

    /// The other half of the same repair. The budget compares
    /// `total - protected`, so a stale-high `protected` drops the
    /// comparison under the limit and skips an eviction that was due -
    /// the failure the total-side drift cannot express, because a
    /// drifted total moves both sides of that subtraction together.
    ///
    /// An in-progress tool call is protected whatever the app status,
    /// so completing one out of band leaves `protected` counting bytes
    /// that are now evictable while the total stays put. The repair
    /// keys on block counts, not on protection, so the uncached append
    /// is what fires the rebuild; the completion only creates the
    /// discrepancy the rebuild then corrects.
    #[test]
    fn budget_enforcement_repairs_protected_drift_before_reading_totals() {
        let mut app = app_with_cached_messages(2);
        let caches = std::rc::Rc::clone(&app.render_caches);
        let mut tool = assistant_tool_message("drifting", model::ToolCallStatus::InProgress);
        if let MessageBlock::ToolCall(tc) = &mut tool.blocks[0] {
            caches.tool_call(&tc.id, tc.render_epoch).store(vec![Line::from("t".repeat(8192))]);
        }
        app.push_message_tracked(tool);
        app.rebuild_render_cache_accounting();

        let protected_recorded = app.render_cache_protected_bytes().expect("active session");
        assert!(
            protected_recorded > 0,
            "the fixture has to protect real bytes, or the subtraction under test is zero",
        );

        // Out of band, so nothing re-derives. The completion is what
        // makes `protected` wrong; the append is what the repair
        // notices, since it compares block counts.
        if let MessageBlock::ToolCall(tc) =
            &mut app.active_messages_mut().expect("active session")[2].blocks[0]
        {
            tc.status = model::ToolCallStatus::Completed;
        }
        append_uncached_block_out_of_band(&mut app, 0);

        // Sits between the drifted reading and the true one, so the two
        // disagree about whether anything needs evicting.
        let drifted_budgeted = app
            .render_cache_total_bytes()
            .expect("active session")
            .saturating_sub(app.render_cache_protected_bytes().expect("active session"));
        app.render_cache_budget.max_bytes = drifted_budgeted + 1;

        let stats = app.enforce_render_cache_budget();

        assert_eq!(
            stats.protected_bytes, 0,
            "the completed tool's bytes stop counting as protected once the drift is repaired",
        );
        assert!(
            stats.total_before_bytes.saturating_sub(stats.protected_bytes)
                > app.render_cache_budget.max_bytes,
            "the repaired reading is over the limit, where the drifted one was under it",
        );
        assert!(
            stats.evicted_blocks >= 1,
            "eviction runs, which the drifted protected figure would have skipped",
        );
    }
}
