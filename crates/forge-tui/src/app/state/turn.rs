//! Turn lifecycle on `App`: the per-session turn-state mirrors, the
//! live turn clock and its row settlement, the active-turn assistant
//! pointer (thinking-spinner anchor), and the stop-hook summary with
//! its index-shifting helpers.

use std::time::Instant;

use super::types::{AppStatus, SessionTurnState, StopHookSummaryState};
use super::{ChatMessage, MessageRole, TurnNoticeLocation, TurnNoticeRef};
use crate::agent::model;

impl super::App {
    /// Run `f` with read-only access to the active session's
    /// turn state. Falls through to a fresh `SessionTurnState::default()`
    /// when no active bucket exists (pre-Connect window).
    pub fn with_turn_state<R>(&self, f: impl FnOnce(&SessionTurnState) -> R) -> R {
        match self.active_session() {
            Some(s) => f(&s.turn_state),
            None => f(&SessionTurnState::default()),
        }
    }

    /// Run `f` with mutable access to the active session's turn
    /// state. Auto-creates the pre-Connect bucket if missing.
    pub fn with_turn_state_mut<R>(&mut self, f: impl FnOnce(&mut SessionTurnState) -> R) -> R {
        f(&mut self.active_bucket_mut().turn_state)
    }

    /// Active session's `is_compacting` flag.
    pub fn is_compacting(&self) -> bool {
        self.active_session().is_some_and(|s| s.is_compacting)
    }

    /// Whether anything is happening the user should see moving: this
    /// session's turn, a compaction, or live background work in any
    /// session. Drives the spinner clock and the terminal tab title
    /// together, so the two cannot disagree about whether forge is busy.
    pub fn shows_activity(&self) -> bool {
        matches!(
            self.status,
            AppStatus::Connecting
                | AppStatus::CommandPending
                | AppStatus::Thinking
                | AppStatus::Running
        ) || self.is_compacting()
            || self.sessions.values().any(|s| {
                crate::app::session::session_shows_spinner(
                    s.lifecycle_state,
                    s.has_live_background_work(),
                )
            })
            || self.sessions.values().any(|s| {
                s.dictate.is_some()
                    || s.dictate_border
                        .as_ref()
                        .is_some_and(|border| border.animating(Instant::now()))
            })
    }

    /// Set the active session's `is_compacting` flag.
    pub fn set_is_compacting(&mut self, value: bool) {
        self.active_bucket_mut().is_compacting = value;
    }

    /// Active session's `pending_compact_clear` flag.
    pub fn pending_compact_clear(&self) -> bool {
        self.active_session().is_some_and(|s| s.pending_compact_clear)
    }

    /// Set the active session's `pending_compact_clear` flag.
    pub fn set_pending_compact_clear(&mut self, value: bool) {
        self.active_bucket_mut().pending_compact_clear = value;
    }

    /// Active session's cancelled-turn pending hint flag.
    pub fn cancelled_turn_pending_hint(&self) -> bool {
        self.active_session().is_some_and(|s| s.cancelled_turn_pending_hint)
    }

    /// Set the active session's cancelled-turn pending hint flag.
    pub fn set_cancelled_turn_pending_hint(&mut self, value: bool) {
        self.active_bucket_mut().cancelled_turn_pending_hint = value;
    }

    /// Active session's pending cancel origin.
    pub fn pending_cancel(&self) -> bool {
        self.active_session().is_some_and(|s| s.pending_cancel)
    }

    /// Set the active session's pending cancel origin.
    pub fn set_pending_cancel(&mut self, value: bool) {
        self.active_bucket_mut().pending_cancel = value;
    }

    /// Borrow the active session's prompt suggestion.
    pub fn prompt_suggestion(&self) -> Option<&str> {
        self.active_session().and_then(|s| s.prompt_suggestion.as_deref())
    }

    /// Set the active session's prompt suggestion.
    pub fn set_prompt_suggestion(&mut self, value: Option<String>) {
        self.active_bucket_mut().prompt_suggestion = value;
    }

    /// Borrow the active session's last rate-limit update.
    pub fn last_rate_limit_update(&self) -> Option<&model::RateLimitUpdate> {
        self.active_session().and_then(|s| s.last_rate_limit_update.as_ref())
    }

    /// Set the active session's last rate-limit update.
    pub fn set_last_rate_limit_update(&mut self, value: Option<model::RateLimitUpdate>) {
        self.active_bucket_mut().last_rate_limit_update = value;
    }

    /// Borrow the active session's turn notice ref list.
    pub fn turn_notice_refs(&self) -> &[TurnNoticeRef] {
        self.active_session().map_or(&[], |s| s.turn_notice_refs.as_slice())
    }

    /// Mutable borrow of the turn notice ref list.
    pub fn turn_notice_refs_mut(&mut self) -> &mut Vec<TurnNoticeRef> {
        &mut self.active_bucket_mut().turn_notice_refs
    }

    /// Active session's main-assistant turn message index.
    pub fn active_turn_assistant_message_idx(&self) -> Option<usize> {
        self.active_session().and_then(|s| s.active_turn_assistant_message_idx)
    }

    /// Set the active session's main-assistant turn message index.
    pub fn set_active_turn_assistant_message_idx(&mut self, idx: Option<usize>) {
        self.active_bucket_mut().active_turn_assistant_message_idx = idx;
    }

    /// Active session's running thinking-token estimate for the
    /// in-flight turn (#273). `None` when no `ThinkingTokens` event
    /// has fired yet or the turn just ended.
    pub fn latest_thinking_tokens(&self) -> Option<u64> {
        self.active_session().and_then(|s| s.latest_thinking_tokens)
    }

    /// Set the active session's running thinking-token estimate.
    /// Called by the `Message::ThinkingTokens` reducer; passed
    /// `None` at each turn boundary, which is what keeps one turn's
    /// estimate off the next turn's row.
    pub fn set_latest_thinking_tokens(&mut self, value: Option<u64>) {
        self.active_bucket_mut().latest_thinking_tokens = value;
    }

    /// Start the active session's live turn accounting, so the row
    /// counts from prompt dispatch rather than from the first
    /// assistant frame. A settled message is left alone.
    ///
    /// Resets the thinking accumulator with it. The row's own copy is
    /// wiped by the struct replacement below, and leaving the session
    /// field behind would add an interrupted turn's estimate to the
    /// next one's, since the deltas accumulate rather than overwrite.
    pub fn start_live_turn(&mut self, at: std::time::Instant) {
        self.set_latest_thinking_tokens(None);
        self.active_bucket_mut().live_turn.start(at);
        self.settle_orphaned_turn_rows(at);
        let Some(idx) = self
            .messages()
            .iter()
            .rposition(|m| matches!(m.role, crate::app::MessageRole::Assistant))
        else {
            return;
        };
        if let Some(msg) = self.active_messages_mut().get_mut(idx)
            && !msg.turn_info.is_settled()
        {
            msg.turn_info = crate::app::state::messages::TurnInfo {
                started_at: Some(at),
                ..crate::app::state::messages::TurnInfo::default()
            };
            msg.invalidate_render_cache();
        }
    }

    /// Settle rows still counting from a turn that can no longer
    /// produce a Result of its own - the CLI fuses a cancel-then-type
    /// prompt into the interrupted turn without emitting one, so the
    /// fresh start is the only chance to stop the clock.
    fn settle_orphaned_turn_rows(&mut self, at: std::time::Instant) {
        for msg in self.active_messages_mut() {
            if !matches!(msg.role, crate::app::MessageRole::Assistant) {
                continue;
            }
            let Some(started) = msg.turn_info.started_at else {
                continue;
            };
            if msg.turn_info.is_settled() {
                continue;
            }
            msg.turn_info.duration_ms = Some(
                u64::try_from(at.saturating_duration_since(started).as_millis())
                    .unwrap_or(u64::MAX),
            );
        }
    }

    /// Carry the in-flight turn's bar onto a freshly opened tail
    /// placeholder. A mid-turn submit rides the running turn rather
    /// than starting one, so the clock and the usage/thinking
    /// accumulators keep running; the message that was streaming sheds
    /// the row it held, leaving one bar where the Result will settle.
    ///
    /// A turn whose clock nobody started still starts the bucket clock
    /// here, so the first usage frame cannot restart the row's elapsed
    /// and the settled-row render gate cannot read the stamped row as
    /// not running.
    pub fn continue_live_turn(&mut self, at: std::time::Instant) {
        let bucket_clock = self.active_session().and_then(|s| s.live_turn.started_at);
        let live_started = if let Some(started) = bucket_clock {
            started
        } else {
            self.active_bucket_mut().live_turn.start(at);
            at
        };
        let fresh_row = || crate::app::state::messages::TurnInfo {
            started_at: Some(live_started),
            ..crate::app::state::messages::TurnInfo::default()
        };
        let mut source_idx = None;
        let mut target_idx = None;
        for idx in self
            .messages()
            .iter()
            .enumerate()
            .filter(|(_, m)| matches!(m.role, crate::app::MessageRole::Assistant))
            .map(|(idx, _)| idx)
        {
            source_idx = target_idx;
            target_idx = Some(idx);
        }
        let Some(target_idx) = target_idx else {
            return;
        };
        // The take waits until the target gate has passed, so a gate
        // failure cannot silently drop the row it just carried.
        if self.active_messages_mut().get(target_idx).is_some_and(|msg| msg.turn_info.is_settled())
        {
            return;
        }
        let carried = source_idx
            .and_then(|idx| self.active_messages_mut().get_mut(idx))
            .filter(|msg| !msg.turn_info.is_settled() && !msg.turn_info.is_empty())
            .map_or_else(fresh_row, |msg| {
                let carried = std::mem::take(&mut msg.turn_info);
                msg.invalidate_render_cache();
                carried
            });
        if let Some(msg) = self.active_messages_mut().get_mut(target_idx) {
            msg.turn_info = carried;
            msg.invalidate_render_cache();
        }
    }

    /// Fold one assistant frame's usage into the live turn, returning
    /// the turn's start and running totals. Starts the turn if nothing
    /// did - cron, auto-continue and peer traffic arrive with it
    /// already under way.
    pub fn record_live_turn_usage(
        &mut self,
        message_id: String,
        usage: crate::app::state::messages::LiveUsage,
    ) -> (Option<std::time::Instant>, Option<crate::app::state::messages::LiveUsage>) {
        let live = &mut self.active_bucket_mut().live_turn;
        if live.started_at.is_none() {
            live.start(std::time::Instant::now());
        }
        live.record(message_id, usage);
        (live.started_at, live.totals())
    }

    /// Close the live turn and return the API time attributable to
    /// it, or `None` when the wire attributed none.
    ///
    /// `Result.duration_api_ms` counts up across the session, so the
    /// turn's figure is the delta; a value below the previous one
    /// means the counter restarted and is already per-turn. A
    /// resulting zero is "not attributed" rather than "took no time" -
    /// the counter is millisecond-granular, so a turn that reached the
    /// API cannot register zero.
    pub fn settle_live_turn(&mut self, duration_api_ms: u64) -> Option<u64> {
        let bucket = self.active_bucket_mut();
        let per_turn = match bucket.prev_duration_api_ms {
            Some(prev) if duration_api_ms >= prev => duration_api_ms - prev,
            _ => duration_api_ms,
        };
        bucket.prev_duration_api_ms = Some(duration_api_ms);
        bucket.live_turn = crate::app::state::messages::LiveTurn::default();
        (per_turn > 0).then_some(per_turn)
    }

    /// Active session's most recent `Message::StopHookSummary`
    /// (#273). Rendered as the collapsed `↳ hook summary · N actions`
    /// surface when `actions > 0`.
    pub fn last_stop_hook_summary(&self) -> Option<&StopHookSummaryState> {
        self.active_session().and_then(|s| s.last_stop_hook_summary.as_ref())
    }

    /// Set the active session's stop-hook summary. Each turn's
    /// `Message::StopHookSummary` overwrites the prior value.
    pub fn set_last_stop_hook_summary(&mut self, value: Option<StopHookSummaryState>) {
        self.active_bucket_mut().last_stop_hook_summary = value;
    }

    /// Toggle / set the per-message stop-hook-summary expansion
    /// flag. Default-collapsed; clicking `[▶ expand]` flips to true,
    /// `[▼ collapse]` flips back.
    pub fn toggle_stop_hook_summary_expanded(&mut self, message_idx: usize) {
        let bucket = self.active_bucket_mut();
        let entry = bucket.stop_hook_summary_expanded.entry(message_idx).or_default();
        *entry = !*entry;
    }

    /// Is the stop-hook summary for `message_idx` currently expanded?
    pub fn stop_hook_summary_expanded(&self, message_idx: usize) -> bool {
        self.active_session()
            .and_then(|s| s.stop_hook_summary_expanded.get(&message_idx).copied())
            .unwrap_or(false)
    }

    pub fn active_turn_assistant_idx(&self) -> Option<usize> {
        self.active_turn_assistant_message_idx().filter(|&idx| {
            self.messages().get(idx).is_some_and(|msg| matches!(msg.role, MessageRole::Assistant))
        })
    }

    pub fn bind_active_turn_assistant(&mut self, idx: usize) {
        let next = self
            .messages()
            .get(idx)
            .is_some_and(|msg| matches!(msg.role, MessageRole::Assistant))
            .then_some(idx);
        self.set_active_turn_assistant_message_idx(next);
    }

    pub fn bind_active_turn_assistant_to_tail(&mut self) {
        if let Some(idx) = self.messages().len().checked_sub(1) {
            self.bind_active_turn_assistant(idx);
        } else {
            self.clear_active_turn_assistant();
        }
    }

    /// Open a fresh assistant turn: push an empty assistant placeholder
    /// at the tail and bind the active-turn pointer onto it. Shared by
    /// the typed-submit (`input_submit::dispatch_prompt`) and
    /// delivered-prompt (`sdk_message::push_peer_envelope_user_turn_if_present`)
    /// turn-open paths so the thinking spinner pins to the new tail
    /// placeholder in both - the pointer is what `chat::msg_spinner`
    /// reads to decide which message wears the spinner.
    pub(crate) fn push_active_turn_assistant_placeholder(&mut self) {
        self.push_message_tracked(ChatMessage::new(MessageRole::Assistant, Vec::new()));
        self.bind_active_turn_assistant_to_tail();
    }

    /// Keep the thinking spinner anchored while a turn is running: bind onto
    /// an empty assistant tail (a genuine in-flight placeholder), else open a
    /// fresh placeholder.
    pub(crate) fn ensure_running_turn_spinner_anchor(&mut self) {
        if !matches!(self.status, AppStatus::Thinking | AppStatus::Running) {
            return;
        }
        if self.active_turn_assistant_idx().is_some() {
            return;
        }
        let tail_is_empty_assistant = self
            .messages()
            .last()
            .is_some_and(|msg| matches!(msg.role, MessageRole::Assistant) && msg.blocks.is_empty());
        if tail_is_empty_assistant {
            self.bind_active_turn_assistant_to_tail();
        } else {
            self.push_active_turn_assistant_placeholder();
        }
    }

    /// Drop a trailing empty assistant placeholder if the tail is one. A
    /// prior turn-open (typed or delivered) may have pushed a placeholder
    /// that never received tokens; stripping it before the next user
    /// bubble keeps rapid-fire turns from stranding blank assistant
    /// bubbles between them. Shared by the typed-submit and
    /// delivered-prompt turn-open paths.
    pub(crate) fn strip_trailing_empty_assistant_placeholder(&mut self) {
        let Some(tail_idx) = self.messages().len().checked_sub(1) else {
            return;
        };
        let tail_is_empty_asst = self
            .messages()
            .get(tail_idx)
            .is_some_and(|msg| matches!(msg.role, MessageRole::Assistant) && msg.blocks.is_empty());
        if tail_is_empty_asst {
            let _ = self.remove_message_tracked(tail_idx);
        }
    }

    pub fn clear_active_turn_assistant(&mut self) {
        self.set_active_turn_assistant_message_idx(None);
    }

    pub(crate) fn clear_turn_notice_refs(&mut self) {
        self.turn_notice_refs_mut().clear();
    }

    pub(crate) fn shift_turn_notice_refs_for_insert(&mut self, idx: usize) {
        for notice_ref in self.turn_notice_refs_mut() {
            match &mut notice_ref.location {
                TurnNoticeLocation::Inline { msg_idx, .. }
                | TurnNoticeLocation::Standalone { msg_idx }
                    if idx <= *msg_idx =>
                {
                    *msg_idx = msg_idx.saturating_add(1);
                }
                TurnNoticeLocation::Inline { .. } | TurnNoticeLocation::Standalone { .. } => {}
            }
        }
    }

    pub(crate) fn shift_turn_notice_refs_for_remove(&mut self, idx: usize) {
        self.turn_notice_refs_mut().retain_mut(|notice_ref| match &mut notice_ref.location {
            TurnNoticeLocation::Inline { msg_idx, .. }
            | TurnNoticeLocation::Standalone { msg_idx } => match idx.cmp(msg_idx) {
                std::cmp::Ordering::Less => {
                    *msg_idx = msg_idx.saturating_sub(1);
                    true
                }
                std::cmp::Ordering::Equal => false,
                std::cmp::Ordering::Greater => true,
            },
        });
    }

    pub(crate) fn remap_turn_notice_refs_after_message_drop(
        &mut self,
        old_to_new: &[Option<usize>],
    ) {
        self.turn_notice_refs_mut().retain_mut(|notice_ref| match &mut notice_ref.location {
            TurnNoticeLocation::Inline { msg_idx, .. }
            | TurnNoticeLocation::Standalone { msg_idx } => {
                let Some(new_idx) = old_to_new.get(*msg_idx).copied().flatten() else {
                    return false;
                };
                *msg_idx = new_idx;
                true
            }
        });
    }

    pub(crate) fn shift_active_turn_assistant_for_insert(&mut self, idx: usize) {
        if let Some(owner_idx) = self.active_turn_assistant_message_idx()
            && idx <= owner_idx
        {
            self.set_active_turn_assistant_message_idx(Some(owner_idx.saturating_add(1)));
        }
    }

    pub(crate) fn shift_stop_hook_summary_for_insert(&mut self, idx: usize) {
        if let Some(summary) = self.active_bucket_mut().last_stop_hook_summary.as_mut()
            && idx <= summary.message_idx
        {
            summary.message_idx = summary.message_idx.saturating_add(1);
        }
    }

    pub(crate) fn shift_active_turn_assistant_for_remove(&mut self, idx: usize) {
        let Some(owner_idx) = self.active_turn_assistant_message_idx() else {
            return;
        };
        let next = match idx.cmp(&owner_idx) {
            std::cmp::Ordering::Less => Some(owner_idx.saturating_sub(1)),
            std::cmp::Ordering::Equal => None,
            std::cmp::Ordering::Greater => Some(owner_idx),
        };
        self.set_active_turn_assistant_message_idx(next);
    }

    pub(crate) fn shift_stop_hook_summary_for_remove(&mut self, idx: usize) {
        let Some(owner_idx) = self.last_stop_hook_summary().map(|s| s.message_idx) else {
            return;
        };
        match idx.cmp(&owner_idx) {
            std::cmp::Ordering::Less => {
                if let Some(summary) = self.active_bucket_mut().last_stop_hook_summary.as_mut() {
                    summary.message_idx = owner_idx.saturating_sub(1);
                }
            }
            std::cmp::Ordering::Equal => self.set_last_stop_hook_summary(None),
            std::cmp::Ordering::Greater => {}
        }
    }

    pub(crate) fn remap_stop_hook_summary_after_message_drop(
        &mut self,
        old_to_new: &[Option<usize>],
    ) {
        let Some(old_idx) = self.last_stop_hook_summary().map(|s| s.message_idx) else {
            return;
        };
        match old_to_new.get(old_idx).copied().flatten() {
            Some(new_idx) => {
                if let Some(summary) = self.active_bucket_mut().last_stop_hook_summary.as_mut() {
                    summary.message_idx = new_idx;
                }
            }
            None => self.set_last_stop_hook_summary(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::App;
    use pretty_assertions::assert_eq;

    #[test]
    fn peer_envelope_insert_shifts_stop_hook_summary_index() {
        use crate::app::state::types::StopHookSummaryState;
        use crate::app::{ChatMessage, MessageBlock, MessageRole, TextBlock};
        let mut app = App::test_default();
        let msg = |t: &str| {
            ChatMessage::new(
                MessageRole::User,
                vec![MessageBlock::Text(TextBlock::from_complete(t))],
            )
        };
        let bound_idx = app.messages().len();
        app.push_message_tracked(msg("bound"));
        app.set_last_stop_hook_summary(Some(StopHookSummaryState {
            message_idx: bound_idx,
            actions: 1,
            hooks: Vec::new(),
        }));
        // A peer envelope inserts before the summary's bound message; the
        // chip's anchor index must follow it down.
        app.insert_message_tracked(bound_idx, msg("peer"));
        assert_eq!(app.last_stop_hook_summary().map(|s| s.message_idx), Some(bound_idx + 1));
    }

    #[test]
    fn remove_shifts_then_clears_stop_hook_summary_index() {
        use crate::app::state::types::StopHookSummaryState;
        use crate::app::{ChatMessage, MessageBlock, MessageRole, TextBlock};
        let mut app = App::test_default();
        let msg = |t: &str| {
            ChatMessage::new(
                MessageRole::User,
                vec![MessageBlock::Text(TextBlock::from_complete(t))],
            )
        };
        let base = app.messages().len();
        app.push_message_tracked(msg("before"));
        app.push_message_tracked(msg("bound"));
        let bound_idx = base + 1;
        app.set_last_stop_hook_summary(Some(StopHookSummaryState {
            message_idx: bound_idx,
            actions: 1,
            hooks: Vec::new(),
        }));
        // Removing a message before the anchor decrements its index.
        app.remove_message_tracked(base);
        assert_eq!(app.last_stop_hook_summary().map(|s| s.message_idx), Some(bound_idx - 1));
        // Removing the anchor itself clears the summary.
        app.remove_message_tracked(bound_idx - 1);
        assert!(app.last_stop_hook_summary().is_none());
    }
}
