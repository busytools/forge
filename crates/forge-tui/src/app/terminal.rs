use super::{App, MessageBlock, TerminalSnapshotMode, ToolCallInfo};

enum TerminalUpdatePayload {
    Append { bytes: Vec<u8>, current_len: usize },
    Replace { bytes: Vec<u8>, current_len: usize },
}

impl TerminalUpdatePayload {
    fn summary(&self) -> (&'static str, usize, usize) {
        match self {
            Self::Append { bytes, current_len } => ("append", bytes.len(), *current_len),
            Self::Replace { bytes, current_len } => ("replace", bytes.len(), *current_len),
        }
    }
}

fn apply_terminal_payload(tc: &mut ToolCallInfo, payload: TerminalUpdatePayload) -> bool {
    match payload {
        TerminalUpdatePayload::Append { bytes, current_len } => {
            if bytes.is_empty() {
                return false;
            }
            let delta = String::from_utf8_lossy(&bytes);
            crate::perf::mark_with("terminal_delta_bytes", "bytes", bytes.len());
            let output = tc.terminal_output.get_or_insert_with(String::new);
            output.push_str(&delta);
            tc.terminal_bytes_seen = current_len;
            tc.terminal_output_len = current_len;
            tc.terminal_snapshot_mode = TerminalSnapshotMode::AppendOnly;
            true
        }
        TerminalUpdatePayload::Replace { bytes, current_len } => {
            crate::perf::mark("terminal_full_snapshot_fallbacks");
            let snapshot = String::from_utf8_lossy(&bytes).to_string();
            let changed = tc.terminal_output.as_deref() != Some(snapshot.as_str());
            if changed {
                tc.terminal_output = Some(snapshot);
            }
            tc.terminal_bytes_seen = current_len;
            tc.terminal_output_len = current_len;
            tc.terminal_snapshot_mode = TerminalSnapshotMode::AppendOnly;
            changed
        }
    }
}

/// Snapshot terminal output buffers into `ToolCallInfo` for rendering.
/// Called each frame so in-progress Execute tool calls show live output.
///
/// Uses append-only deltas when possible, with full-snapshot fallback when
/// invariants are broken (truncate/reset/replace mode).
pub(super) fn update_terminal_outputs(app: &mut App) -> bool {
    let _t = app.perf.as_ref().map(|p| p.start("terminal::update"));
    // Snapshot terminal refs + per-id Rc<RefCell<...>> output buffer
    // handle so we can release the terminals borrow before mutating
    // `app.messages` (the messages accessor borrows the whole App,
    // which would conflict with the live `app.terminals` borrow).
    let log_session_id = app.session_id().map_or_else(String::new, |s| s.to_string());
    let pending_updates: Vec<(
        super::state::TerminalToolCallRef,
        std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    )> = {
        let Some(terminals_rc) = app.terminals() else {
            return false;
        };
        let terminals = terminals_rc.borrow();
        if terminals.is_empty() {
            return false;
        }
        app.terminal_tool_calls()
            .iter()
            .filter_map(|tref| {
                terminals
                    .get(tref.terminal_id.as_str())
                    .map(|term| (tref.clone(), term.output_buffer.clone()))
            })
            .collect()
    };

    let mut changed = false;
    let mut dirty_messages = Vec::new();
    let mut dirty_slots = Vec::new();

    // Use the indexed terminal tool calls instead of scanning all messages/blocks.
    for (terminal_ref, output_buffer) in pending_updates {
        let Some(MessageBlock::ToolCall(tc)) = app
            .active_messages_mut()
            .get_mut(terminal_ref.msg_idx)
            .and_then(|m| m.blocks.get_mut(terminal_ref.block_idx))
        else {
            continue;
        };
        let tc = tc.as_mut();
        if !matches!(
            tc.status,
            crate::agent::model::ToolCallStatus::Pending
                | crate::agent::model::ToolCallStatus::InProgress
        ) {
            continue;
        }

        // Copy only the required bytes, then decode outside the
        // borrow to keep the slice borrow short.
        let payload = {
            let buf = output_buffer.borrow();
            let current_len = buf.len();
            let force_replace =
                matches!(tc.terminal_snapshot_mode, TerminalSnapshotMode::ReplaceSnapshot);
            if !force_replace && current_len == tc.terminal_bytes_seen {
                continue;
            }
            if !force_replace && current_len > tc.terminal_bytes_seen {
                TerminalUpdatePayload::Append {
                    bytes: buf[tc.terminal_bytes_seen..].to_vec(),
                    current_len,
                }
            } else {
                TerminalUpdatePayload::Replace { bytes: buf.clone(), current_len }
            }
        };
        let (update_mode, delta_bytes, total_bytes) = payload.summary();
        if apply_terminal_payload(tc, payload) {
            tc.mark_tool_call_layout_dirty();
            tracing::debug!(
                target: crate::logging::targets::APP_COMMAND,
                event_name = "terminal_output_summary",
                message = "terminal output updated",
                outcome = "success",
                session_id = %log_session_id,
                tool_call_id = %tc.id,
                terminal_id = %terminal_ref.terminal_id,
                terminal_update_mode = update_mode,
                count = u64::try_from(delta_bytes).unwrap_or_default(),
                size_bytes = u64::try_from(total_bytes).unwrap_or_default(),
                tool_name = %tc.sdk_tool_name,
                tool_status = ?tc.status,
                has_command = tc.terminal_command.is_some(),
            );
            dirty_slots.push((terminal_ref.msg_idx, terminal_ref.block_idx));
            if dirty_messages.last().copied() != Some(terminal_ref.msg_idx) {
                dirty_messages.push(terminal_ref.msg_idx);
            }
            changed = true;
        }
    }

    for (mi, bi) in dirty_slots {
        app.sync_render_cache_slot(mi, bi);
    }
    for mi in dirty_messages.iter().copied() {
        app.recompute_message_retained_bytes(mi);
    }
    app.invalidate_message_set(dirty_messages.iter().copied());

    changed
}

#[cfg(test)]
mod tests {
    use super::update_terminal_outputs;
    use crate::agent::events::TerminalProcess;
    use crate::agent::model;
    use crate::app::{
        App, BlockCache, ChatMessage, MessageBlock, MessageRole, TerminalSnapshotMode, TextBlock,
        ToolCallInfo,
    };
    use std::cell::RefCell;
    use std::rc::Rc;

    fn bash_tool_message(id: &str, terminal_id: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::Assistant,
            vec![MessageBlock::ToolCall(Box::new(ToolCallInfo {
                id: id.to_owned(),
                title: format!("tool {id}"),
                sdk_tool_name: "Bash".to_owned(),
                raw_input: None,
                raw_input_bytes: 0,
                output_metadata: None,
                task_metadata: None,
                status: model::ToolCallStatus::InProgress,
                content: Vec::new(),
                hidden: false,
                terminal_id: Some(terminal_id.to_owned()),
                terminal_command: Some(format!("echo {id}")),
                terminal_output: None,
                terminal_output_len: 0,
                terminal_bytes_seen: 0,
                terminal_snapshot_mode: TerminalSnapshotMode::AppendOnly,
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
            }))],
        )
    }

    fn user_message(text: &str) -> ChatMessage {
        ChatMessage::new(
            MessageRole::User,
            vec![MessageBlock::Text(TextBlock::from_complete(text))],
        )
    }

    #[test]
    fn terminal_updates_invalidate_all_dirty_messages() {
        let mut app = App::test_default();
        app.active_messages_mut().push(bash_tool_message("bash-1", "term-1"));
        app.active_messages_mut().push(user_message("gap"));
        app.active_messages_mut().push(bash_tool_message("bash-2", "term-2"));
        app.index_tool_call("bash-1".to_owned(), 0, 0);
        app.index_tool_call("bash-2".to_owned(), 2, 0);
        app.sync_terminal_tool_call("term-1".to_owned(), 0, 0);
        app.sync_terminal_tool_call("term-2".to_owned(), 2, 0);
        app.terminals_mut().borrow_mut().insert(
            "term-1".to_owned(),
            TerminalProcess {
                output_buffer: Rc::new(RefCell::new(b"alpha\n".to_vec())),
                command: "echo alpha".to_owned(),
            },
        );
        app.terminals_mut().borrow_mut().insert(
            "term-2".to_owned(),
            TerminalProcess {
                output_buffer: Rc::new(RefCell::new(b"beta\n".to_vec())),
                command: "echo beta".to_owned(),
            },
        );

        let _ = app.active_viewport_mut().on_frame(80, 24);
        app.active_viewport_mut().sync_message_count(3);
        app.active_viewport_mut().mark_heights_valid();
        app.active_viewport_mut().rebuild_prefix_sums();

        assert!(update_terminal_outputs(&mut app));
        assert!(!app.active_viewport_mut().message_height_is_current(0));
        assert!(app.active_viewport_mut().message_height_is_current(1));
        assert!(!app.active_viewport_mut().message_height_is_current(2));
        assert_eq!(app.active_viewport_mut().oldest_stale_index(), Some(0));
    }
}
