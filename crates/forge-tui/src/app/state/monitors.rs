//! MONITORS on `App`: entries built from `Monitor` tool_use, the
//! task-id keyed status / output-file / output-tail writers that the
//! wire events drive, and the all-terminal auto-clear.

use super::MessageBlock;

impl super::App {
    /// Active session's MONITOR entries (chat notice +
    /// Inspector MONITORS section both read this).
    pub fn monitors(&self) -> &[crate::app::state::types::MonitorEntry] {
        self.active_session().map_or(&[], |s| s.monitors.as_slice())
    }

    /// Mutable accessor for the active session's
    /// MONITORS list. Auto-creates the pre-Connect bucket if missing.
    pub(crate) fn monitors_mut(&mut self) -> &mut Vec<crate::app::state::types::MonitorEntry> {
        &mut self.active_bucket_mut().monitors
    }

    /// Insert / update a `MonitorEntry` based on a fresh
    /// `Monitor` tool_use. Idempotent: a matching `tool_use_id`
    /// refreshes the existing entry's input fields without touching
    /// `status` or `output_tail`. Returns true when a new entry was
    /// pushed.
    pub fn upsert_monitor_from_tool_input(
        &mut self,
        tool_use_id: &str,
        description: String,
        command: String,
        persistent: bool,
        timeout_ms: u64,
    ) -> bool {
        // a fresh live Monitor tool_use is `Running`
        // until the wire emits a terminal `task_updated`. But during
        // `load_resume_history` replay the replay walker doesn't pipe
        // terminal `task_updated` events back into the status
        // setter, so a Monitor that was historically completed gets
        // restored as `Running` and stays that way forever - blocking
        // `clear_monitors_if_all_terminal` for legit completed
        // siblings. Restored Monitors land in `Stopped` initially;
        // a terminal `task_updated` arriving later in the same
        // replay walk (or live afterwards) re-flips via
        // `set_monitor_status_by_task_id` to the wire's terminal
        // variant. The setter is gated on the wire's `is_terminal`
        // check at `sdk_message.rs:1116-1141`, so only completed /
        // failed / killed / stopped events drive a re-flip - a
        // `running` event mid-walk does NOT push Stopped back to
        // Running. That's intentional: the value of starting in
        // Stopped is to keep blocked monitors out of the
        // all-terminal-clear predicate; if a historical Monitor
        // genuinely WAS still running at replay time, the next
        // live event resolves it on its own terms.
        // Replay seeds a TERMINAL status so the restored entry stops
        // blocking `clear_monitors_if_all_terminal`. `Completed` rather
        // than `Stopped`: the seed is a placeholder, not a wire signal,
        // and the renderer now paints non-success terminals with a red
        // failure glyph - so seeding `Stopped` would assert a failure we
        // have no evidence for on every monitor in every resumed
        // session. A terminal `task_updated` later in the same replay
        // walk re-flips it to whatever actually happened.
        let initial_status = if self.replay_in_progress {
            crate::app::state::types::MonitorStatus::Completed
        } else {
            crate::app::state::types::MonitorStatus::Running
        };
        let monitors = self.monitors_mut();
        if let Some(existing) = monitors.iter_mut().find(|m| m.tool_use_id == tool_use_id) {
            existing.description = description;
            existing.command = command;
            existing.persistent = persistent;
            existing.timeout_ms = timeout_ms;
            return false;
        }
        monitors.push(crate::app::state::types::MonitorEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: None,
            description,
            command,
            persistent,
            timeout_ms,
            status: initial_status,
            output_file: None,
            output_tail: std::collections::VecDeque::new(),
            expanded_in_inspector: false,
        });
        true
    }

    /// Stamp the `task_id` discovered from the Monitor's
    /// `tool_use_result` (or from `TaskStarted` mapping). No-op when
    /// no matching entry exists or the entry already has a task_id.
    pub fn stamp_monitor_task_id(&mut self, tool_use_id: &str, task_id: String) {
        if let Some(entry) = self.monitors_mut().iter_mut().find(|m| m.tool_use_id == tool_use_id)
            && entry.task_id.is_none()
        {
            entry.task_id = Some(task_id);
        }
    }

    /// Transition the matching Monitor entry to a terminal status,
    /// keyed by the wire `task_id`. Used by lifecycle event handlers
    /// that carry the task_id (e.g. wire `TaskUpdated`). The
    /// all-completed predicate is no longer run here; #277 Bug 5a
    /// deferred that trigger to `handle_task_notification` so the
    /// `task_updated terminal -> task_notification with output_file`
    /// wire ordering can stamp the tail before the entry gets
    /// drained. Callers that mutate status without going through
    /// `handle_task_notification` should call
    /// `clear_monitors_if_all_terminal` themselves if they need
    /// the auto-clear behaviour.
    pub fn set_monitor_status_by_task_id(
        &mut self,
        task_id: &str,
        status: crate::app::state::types::MonitorStatus,
    ) {
        let Some(entry) =
            self.monitors_mut().iter_mut().find(|m| m.task_id.as_deref() == Some(task_id))
        else {
            return;
        };
        entry.status = status;
        let tool_use_id = entry.tool_use_id.clone();
        self.stamp_monitor_status_on_tool_call(&tool_use_id, status);
    }

    /// Mirror a monitor's liveness onto its chat block. The block reads
    /// this rather than `ToolCallInfo::status`, which the "Monitor
    /// started" ack drives terminal while the monitor is still alive.
    fn stamp_monitor_status_on_tool_call(
        &mut self,
        tool_use_id: &str,
        status: crate::app::state::types::MonitorStatus,
    ) {
        let Some((msg_idx, block_idx)) = self.lookup_tool_call(tool_use_id) else {
            return;
        };
        let Some(MessageBlock::ToolCall(tc)) =
            self.active_messages_mut().get_mut(msg_idx).and_then(|m| m.blocks.get_mut(block_idx))
        else {
            return;
        };
        if tc.monitor_status == Some(status) {
            return;
        }
        tc.monitor_status = Some(status);
        tc.mark_tool_call_layout_dirty();
        self.invalidate_lifecycle_block_height(msg_idx, block_idx);
    }

    /// Finish a lifecycle-block mutation the way the backgrounded-`Bash`
    /// stream does (`app::terminal`): marking the tool dirty rebuilds
    /// the render, but the viewport keeps its own prefix-sum of message
    /// heights and this block's height swings as the tail fills and
    /// again when it collapses.
    fn invalidate_lifecycle_block_height(&mut self, msg_idx: usize, block_idx: usize) {
        self.sync_render_cache_slot(msg_idx, block_idx);
        self.recompute_message_retained_bytes(msg_idx);
        self.invalidate_message_set(std::iter::once(msg_idx));
    }

    /// Liveness of the monitor owning `tool_use_id`, read at
    /// `ToolCallInfo` construction. `None` when no entry matches.
    pub fn monitor_status_for_tool_use(
        &self,
        tool_use_id: &str,
    ) -> Option<crate::app::state::types::MonitorStatus> {
        self.monitors().iter().find(|m| m.tool_use_id == tool_use_id).map(|m| m.status)
    }

    /// Stamp the `output_file` path on the matching
    /// Monitor entry. The CLI carries this via
    /// `task_notification.output_file`. Idempotent: same path
    /// overwrites cleanly so repeated `task_notification` events
    /// don't drift the entry's source-of-truth.
    pub fn set_monitor_output_file_by_task_id(&mut self, task_id: &str, path: std::path::PathBuf) {
        if let Some(entry) =
            self.monitors_mut().iter_mut().find(|m| m.task_id.as_deref() == Some(task_id))
        {
            entry.output_file = Some(path);
        }
    }

    /// REPLACE the matching Monitor's `output_tail`
    /// with the supplied lines (typically the most-recent N lines
    /// of its `output_file`). The file is authoritative - the
    /// renderer's tail must match the file, not accumulate stale
    /// entries from prior events. No-op if no entry matches.
    ///
    /// Also stamps the last 5 lines onto the matching `ToolCallInfo`'s
    /// `monitor_output_tail` and marks the tool call's layout dirty so
    /// the in-chat live block re-renders in place - but only when that
    /// rendered tail actually changed, so a timer-polled Monitor with no
    /// new output doesn't churn the cache. Mirrors the
    /// `apply_terminal_payload` precedent in `terminal.rs` (terminal
    /// stream + dirty bump).
    pub fn replace_monitor_output_tail_by_task_id(&mut self, task_id: &str, lines: &[String]) {
        const CHAT_TAIL_MAX: usize = 5;
        // First update the per-session MonitorEntry. Capture the
        // tool_use_id so the chat-tail stamp below can find the
        // matching ToolCallInfo through `tool_call_index`.
        let tool_use_id = {
            let Some(entry) =
                self.monitors_mut().iter_mut().find(|m| m.task_id.as_deref() == Some(task_id))
            else {
                return;
            };
            entry.output_tail = lines.iter().cloned().collect();
            entry.tool_use_id.clone()
        };
        // Slice the last 5 lines for the chat block. Skip the
        // `lookup_tool_call` -> `messages_mut` walk when the bucket
        // doesn't carry that tool_use_id yet (the ToolCall block
        // arrives via `handle_tool_call` and indexing happens slightly
        // after); the next refresh tick re-stamps once indexed.
        let last_five: Vec<String> = if lines.len() <= CHAT_TAIL_MAX {
            lines.to_vec()
        } else {
            lines[lines.len() - CHAT_TAIL_MAX..].to_vec()
        };
        let Some((msg_idx, block_idx)) = self.lookup_tool_call(&tool_use_id) else {
            return;
        };
        let Some(MessageBlock::ToolCall(tc)) =
            self.active_messages_mut().get_mut(msg_idx).and_then(|m| m.blocks.get_mut(block_idx))
        else {
            return;
        };
        // A timer-polled Monitor with no new output re-runs this path; only
        // re-stamp + invalidate when the rendered chat tail actually changed.
        if tc.monitor_output_tail == last_five {
            return;
        }
        tc.monitor_output_tail = last_five;
        tc.mark_tool_call_layout_dirty();
        self.invalidate_lifecycle_block_height(msg_idx, block_idx);
    }

    /// Read the matching Monitor's stored `output_file`
    /// and refresh its `output_tail` with the last
    /// `MonitorEntry::OUTPUT_TAIL_MAX` lines. Called on each
    /// `task_notification` / `task_progress` event for the monitor.
    /// Silently no-ops when:
    /// - the matching entry has no stored `output_file` yet
    ///   (Monitor just started, hasn't received its first
    ///   `task_notification` with the path)
    /// - the helper returns `None` (file missing / permission denied
    ///   / IO error - the helper logs the WARN; we preserve the
    ///   prior tail)
    pub fn refresh_monitor_output_tail_from_file(&mut self, task_id: &str) {
        let path = self
            .monitors()
            .iter()
            .find(|m| m.task_id.as_deref() == Some(task_id))
            .and_then(|m| m.output_file.clone());
        let Some(path) = path else {
            return;
        };
        if let Some(lines) = crate::app::monitor_output::read_output_file_tail(
            &path,
            crate::app::state::types::MonitorEntry::OUTPUT_TAIL_MAX,
        ) {
            self.replace_monitor_output_tail_by_task_id(task_id, &lines);
        }
    }

    /// Drain the MONITORS list once every entry has transitioned out of
    /// `Running`. Matches the TODOs all-completed auto-clear shape so
    /// the Inspector section drops out entirely. Called explicitly from
    /// `handle_task_notification` rather than implicitly from
    /// `set_monitor_status_by_task_id`, so the
    /// `task_updated terminal -> task_notification with output_file`
    /// wire ordering can stamp the tail before the entry gets drained.
    pub fn clear_monitors_if_all_terminal(&mut self) {
        let monitors = self.monitors_mut();
        if !monitors.is_empty() && monitors.iter().all(|m| !m.is_running()) {
            monitors.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{App, BlockCache, ChatMessage, MessageBlock, MessageRole, ToolCallInfo};
    use crate::app::state::tests::make_test_app;
    use pretty_assertions::assert_eq;

    #[test]
    fn replace_monitor_output_tail_stamps_tool_call_info_and_bumps_dirty() {
        use crate::agent::model::ToolCallStatus;
        use crate::app::state::types::{MonitorEntry, MonitorStatus};
        use std::collections::VecDeque;

        let mut app = App::test_default();
        let tool_use_id = "tu-mon-1";
        let task_id = "task-mon-1";

        // Seed the active session's MonitorEntry.
        app.monitors_mut().push(MonitorEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: Some(task_id.to_owned()),
            description: "demo".to_owned(),
            command: "echo demo".to_owned(),
            persistent: true,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_file: None,
            output_tail: VecDeque::new(),
            expanded_in_inspector: false,
        });

        // Push a matching ToolCall MessageBlock with a fresh ToolCallInfo
        // + index it so `lookup_tool_call` finds it.
        let tc_info = ToolCallInfo {
            id: tool_use_id.to_owned(),
            title: "Monitor".to_owned(),
            sdk_tool_name: "Monitor".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
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
        };
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc_info))],
        ));
        let msg_idx = app.messages().len() - 1;
        app.index_tool_call(tool_use_id.to_owned(), msg_idx, 0);
        let initial_layout_epoch = {
            let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
            let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
                panic!("expected ToolCall block");
            };
            tc.layout_epoch
        };

        // Act: replace tail with 8 lines.
        let lines: Vec<String> = (1..=8).map(|i| format!("line {i}")).collect();
        app.replace_monitor_output_tail_by_task_id(task_id, &lines);

        // Assert: monitor_output_tail carries the LAST 5 lines.
        let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert_eq!(
            tc.monitor_output_tail,
            vec![
                "line 4".to_owned(),
                "line 5".to_owned(),
                "line 6".to_owned(),
                "line 7".to_owned(),
                "line 8".to_owned(),
            ]
        );
        assert!(
            tc.layout_epoch > initial_layout_epoch,
            "layout_epoch must bump so the cached chat block re-renders in place"
        );
    }

    #[test]
    fn replace_monitor_output_tail_handles_fewer_than_five_lines() {
        use crate::agent::model::ToolCallStatus;
        use crate::app::state::types::{MonitorEntry, MonitorStatus};
        use std::collections::VecDeque;

        let mut app = App::test_default();
        let tool_use_id = "tu-mon-2";
        let task_id = "task-mon-2";
        app.monitors_mut().push(MonitorEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: Some(task_id.to_owned()),
            description: "demo".to_owned(),
            command: "echo demo".to_owned(),
            persistent: false,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_file: None,
            output_tail: VecDeque::new(),
            expanded_in_inspector: false,
        });
        let tc_info = ToolCallInfo {
            id: tool_use_id.to_owned(),
            title: "Monitor".to_owned(),
            sdk_tool_name: "Monitor".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
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
        };
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc_info))],
        ));
        let msg_idx = app.messages().len() - 1;
        app.index_tool_call(tool_use_id.to_owned(), msg_idx, 0);

        app.replace_monitor_output_tail_by_task_id(
            task_id,
            &["one".to_owned(), "two".to_owned(), "three".to_owned()],
        );

        let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert_eq!(
            tc.monitor_output_tail,
            vec!["one".to_owned(), "two".to_owned(), "three".to_owned()],
            "tails shorter than 5 are kept verbatim",
        );
    }

    #[test]
    fn replace_monitor_output_tail_unchanged_is_noop_changed_still_dirties() {
        use crate::agent::model::ToolCallStatus;
        use crate::app::state::types::{MonitorEntry, MonitorStatus};
        use std::collections::VecDeque;

        let mut app = App::test_default();
        let tool_use_id = "tu-mon-3";
        let task_id = "task-mon-3";
        app.monitors_mut().push(MonitorEntry {
            tool_use_id: tool_use_id.to_owned(),
            task_id: Some(task_id.to_owned()),
            description: "demo".to_owned(),
            command: "echo demo".to_owned(),
            persistent: true,
            timeout_ms: 0,
            status: MonitorStatus::Running,
            output_file: None,
            output_tail: VecDeque::new(),
            expanded_in_inspector: false,
        });
        let tc_info = ToolCallInfo {
            id: tool_use_id.to_owned(),
            title: "Monitor".to_owned(),
            sdk_tool_name: "Monitor".to_owned(),
            raw_input: None,
            raw_input_bytes: 0,
            output_metadata: None,
            task_metadata: None,
            status: ToolCallStatus::InProgress,
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
        };
        app.push_message_tracked(ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(tc_info))],
        ));
        let msg_idx = app.messages().len() - 1;
        app.index_tool_call(tool_use_id.to_owned(), msg_idx, 0);

        let lines = vec!["alpha".to_owned(), "beta".to_owned()];

        // First refresh stamps the tail and bumps layout_epoch.
        app.replace_monitor_output_tail_by_task_id(task_id, &lines);
        let epoch_after_first = {
            let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
            let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
                panic!("expected ToolCall block");
            };
            assert_eq!(tc.monitor_output_tail, lines);
            tc.layout_epoch
        };

        // Second refresh with the SAME tail must not re-invalidate.
        app.replace_monitor_output_tail_by_task_id(task_id, &lines);
        let epoch_after_unchanged = {
            let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
            let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
                panic!("expected ToolCall block");
            };
            tc.layout_epoch
        };
        assert_eq!(
            epoch_after_unchanged, epoch_after_first,
            "an unchanged monitor-tail refresh must not dirty the cached block",
        );

        // Third refresh with a CHANGED tail re-stamps and re-dirties.
        let changed = vec!["alpha".to_owned(), "beta".to_owned(), "gamma".to_owned()];
        app.replace_monitor_output_tail_by_task_id(task_id, &changed);
        let (mi, bi) = app.lookup_tool_call(tool_use_id).expect("indexed");
        let MessageBlock::ToolCall(tc) = &app.messages()[mi].blocks[bi] else {
            panic!("expected ToolCall block");
        };
        assert_eq!(tc.monitor_output_tail, changed, "a changed tail must re-stamp");
        assert!(
            tc.layout_epoch > epoch_after_unchanged,
            "a changed monitor-tail refresh must dirty so the block re-renders",
        );
    }

    // -----------------------------------------------------------
    // replay-orphan Monitor state.
    // -----------------------------------------------------------

    #[test]
    fn upsert_monitor_during_replay_starts_in_a_terminal_state() {
        // During `load_resume_history` (replay_in_progress = true)
        // the wire walker doesn't re-emit terminal `task_updated`
        // events into the status setter. A replayed Monitor that
        // historically completed must NOT be reconstructed as
        // Running, otherwise it blocks
        // `clear_monitors_if_all_terminal` for any live sibling.
        let mut app = make_test_app();
        app.replay_in_progress = true;
        app.upsert_monitor_from_tool_input(
            "tu_replay",
            "historical monitor".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        let monitors = app.monitors();
        assert_eq!(monitors.len(), 1);
        assert_eq!(
            monitors[0].status,
            crate::app::state::types::MonitorStatus::Completed,
            "a replay-inserted monitor starts terminal so it stops blocking the \
             all-terminal clear, and Completed because the seed is a placeholder \
             rather than evidence the watched command failed",
        );
    }

    #[test]
    fn upsert_monitor_live_path_still_starts_running() {
        // Outside replay (replay_in_progress = false), live Monitor
        // tool_use events keep their existing Running default so the
        // ◉ glyph + " · running" badge animate while the watched
        // command runs.
        let mut app = make_test_app();
        assert!(!app.replay_in_progress, "live default");
        app.upsert_monitor_from_tool_input(
            "tu_live",
            "live monitor".to_owned(),
            "true".to_owned(),
            true,
            300_000,
        );
        let monitors = app.monitors();
        assert_eq!(monitors.len(), 1);
        assert_eq!(monitors[0].status, crate::app::state::types::MonitorStatus::Running);
    }

    // -----------------------------------------------------------
    // auto-clear race against task_notification.
    // -----------------------------------------------------------

    #[test]
    fn set_monitor_status_no_longer_clears_implicitly() {
        // Pre-#277 the status setter called
        // `clear_monitors_if_all_terminal` at its end. That dropped
        // single-monitor entries before `task_notification` could
        // stamp the tail. Bug 5a deferred the trigger to
        // `handle_task_notification`. Confirm the setter no longer
        // drains the Vec on its own.
        let mut app = make_test_app();
        app.upsert_monitor_from_tool_input(
            "tu_solo",
            "solo monitor".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        app.stamp_monitor_task_id("tu_solo", "task_solo".to_owned());
        app.set_monitor_status_by_task_id(
            "task_solo",
            crate::app::state::types::MonitorStatus::Completed,
        );
        // Entry survives the status flip - waiting for
        // handle_task_notification to call the clear.
        assert_eq!(app.monitors().len(), 1);
        assert_eq!(app.monitors()[0].status, crate::app::state::types::MonitorStatus::Completed,);
    }

    #[test]
    fn explicit_clear_drains_when_all_terminal() {
        // The clear helper is now `pub` so `handle_task_notification`
        // can call it. Verify the predicate still drains correctly.
        let mut app = make_test_app();
        app.upsert_monitor_from_tool_input("tu_a", "a".to_owned(), "true".to_owned(), false, 0);
        app.upsert_monitor_from_tool_input("tu_b", "b".to_owned(), "true".to_owned(), false, 0);
        app.stamp_monitor_task_id("tu_a", "task_a".to_owned());
        app.stamp_monitor_task_id("tu_b", "task_b".to_owned());
        app.set_monitor_status_by_task_id(
            "task_a",
            crate::app::state::types::MonitorStatus::Completed,
        );
        app.set_monitor_status_by_task_id(
            "task_b",
            crate::app::state::types::MonitorStatus::Completed,
        );
        // Without the explicit call the entries persist (Bug 5a).
        assert_eq!(app.monitors().len(), 2);
        app.clear_monitors_if_all_terminal();
        assert!(app.monitors().is_empty());
    }

    #[test]
    fn explicit_clear_skips_when_any_still_running() {
        let mut app = make_test_app();
        app.upsert_monitor_from_tool_input(
            "tu_run",
            "still running".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        app.upsert_monitor_from_tool_input(
            "tu_done",
            "done".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        app.stamp_monitor_task_id("tu_done", "task_done".to_owned());
        app.set_monitor_status_by_task_id(
            "task_done",
            crate::app::state::types::MonitorStatus::Completed,
        );
        app.clear_monitors_if_all_terminal();
        // Predicate sees the Running entry and skips the drain.
        assert_eq!(app.monitors().len(), 2);
    }

    #[test]
    fn replay_restored_monitor_accepts_terminal_completed_event() {
        // Replay inserts the entry in Stopped. A subsequent terminal
        // `task_updated` (routed via `set_monitor_status_by_task_id`)
        // re-flips Stopped -> Completed. After #277 Bug 5a the
        // setter no longer drains the section implicitly, so the
        // entry persists post-flip and the invariant is checkable
        // directly. The `expect` makes the test fail loudly if a
        // future refactor restores the implicit clear and the
        // entry goes missing.
        let mut app = make_test_app();
        app.replay_in_progress = true;
        app.upsert_monitor_from_tool_input(
            "tu_replay",
            "historical monitor".to_owned(),
            "true".to_owned(),
            false,
            0,
        );
        // Stamp task_id so the by_task_id setter can find it.
        app.stamp_monitor_task_id("tu_replay", "task_x".to_owned());
        app.set_monitor_status_by_task_id(
            "task_x",
            crate::app::state::types::MonitorStatus::Completed,
        );
        let monitor = app
            .monitors()
            .first()
            .expect("replay-restored entry must persist post-Bug-5a setter call");
        assert_eq!(
            monitor.status,
            crate::app::state::types::MonitorStatus::Completed,
            "terminal event must re-flip the replay-restored entry",
        );
    }
}
