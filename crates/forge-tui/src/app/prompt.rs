//! Unified prompt state. Replaces `InlinePermission` + `InlineQuestion`
//! and the per-tool-call attach pattern.

use crossterm::event::{KeyCode, KeyEvent};
use forge_primitives::permission_ui::{PermissionOption, PermissionRequest};
use forge_primitives::question::{QuestionPrompt, QuestionRequest};
use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq)]
pub struct PromptState {
    /// What kind of prompt — drives layout decisions in the renderer.
    pub source: PromptSource,
    /// The tool_id this prompt is responding to (echoed back on submit).
    pub tool_id: String,
    /// Options the user picks from.
    pub options: Vec<PermissionOption>,
    /// Currently focused option (cursor index into `options`).
    pub focused_option_index: usize,
    /// For multi-select: which options are toggled on.
    pub selected_option_indices: BTreeSet<usize>,
    /// Current sub-mode.
    pub mode: PromptMode,
    /// Notes text buffer (for the synthesized notes-option editor).
    pub notes: String,
    /// Cursor position within `notes` (char index).
    pub notes_cursor: usize,
    /// Edited tool input (for AllowWithInput). Populated when the
    /// user enters the allow-with-edits sub-mode; serialized back to
    /// the CLI on submit.
    pub edited_input: Option<Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PromptSource {
    /// `can_use_tool` permission request (incl. ExitPlanMode).
    Permission {
        display_title: Option<String>,
        decision_reason: Option<String>,
        display_description: Option<String>,
        tool_name: String,
        tool_args_summary: String,
        /// `tool_call.raw_input` (for the allow-with-edits sub-mode).
        raw_input: Option<Value>,
    },
    /// AskUserQuestion request.
    Question {
        prompt: QuestionPrompt,
        question_index: u64,
        total_questions: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMode {
    /// Default — arrow keys move option focus, Enter submits.
    OptionPicker,
    /// User selected a notes-option — typing inserts into `notes`.
    NotesEditor,
    /// User selected allow-with-edits — typing edits `edited_input`.
    EditingInput,
}

impl PromptState {
    /// Construct from a wire `PermissionRequest`. Always includes the
    /// forge-synthesized "Tell Claude something else" escape hatch as
    /// the last option.
    pub fn from_permission(tool_id: String, request: PermissionRequest) -> Self {
        let mut options = request.options;
        // Append the forge-synthesized "Tell Claude something else" escape
        // hatch. Routes to `deny(notes_text)` on submit.
        options.push(PermissionOption {
            option_id: "tell_claude".into(),
            name: "Tell Claude something else".into(),
            kind: forge_primitives::permission_ui::PermissionOptionKind::Notes,
            action: forge_primitives::permission_ui::PermissionAction::Deny,
        });

        let tool_name = request.tool_call.title.clone();
        let tool_args_summary = summarize_tool_args(&request.tool_call);
        let raw_input = request.tool_call.raw_input;

        Self {
            source: PromptSource::Permission {
                display_title: request.display.as_ref().and_then(|d| d.title.clone()),
                decision_reason: request.display.as_ref().and_then(|d| d.decision_reason.clone()),
                display_description: request.display.as_ref().and_then(|d| d.description.clone()),
                tool_name,
                tool_args_summary,
                raw_input,
            },
            tool_id,
            options,
            focused_option_index: 0,
            selected_option_indices: BTreeSet::new(),
            mode: PromptMode::OptionPicker,
            notes: String::new(),
            notes_cursor: 0,
            edited_input: None,
        }
    }

    /// Construct from a wire `QuestionRequest`. Always includes the
    /// forge-synthesized "Tell Claude something else" escape hatch as
    /// the last option.
    pub fn from_question(tool_id: String, request: QuestionRequest) -> Self {
        use forge_primitives::permission_ui::{
            PermissionAction, PermissionOption, PermissionOptionKind,
        };
        // Convert wire question options to permission-option shape (so
        // both prompt kinds share one render path).
        let mut options: Vec<PermissionOption> = request
            .prompt
            .options
            .iter()
            .map(|opt| PermissionOption {
                option_id: opt.option_id.clone(),
                name: opt.label.clone(),
                kind: PermissionOptionKind::Allow,
                action: PermissionAction::Allow,
            })
            .collect();
        options.push(PermissionOption {
            option_id: "tell_claude".into(),
            name: "Tell Claude something else".into(),
            kind: PermissionOptionKind::Notes,
            action: PermissionAction::Deny,
        });

        Self {
            source: PromptSource::Question {
                prompt: request.prompt,
                question_index: request.question_index,
                total_questions: request.total_questions,
            },
            tool_id,
            options,
            focused_option_index: 0,
            selected_option_indices: BTreeSet::new(),
            mode: PromptMode::OptionPicker,
            notes: String::new(),
            notes_cursor: 0,
            edited_input: None,
        }
    }

    /// Is the prompt a multi-select Question?
    pub fn is_multi_select(&self) -> bool {
        matches!(&self.source, PromptSource::Question { prompt, .. } if prompt.multi_select)
    }
}

/// Enqueue a prompt at the tail of the session's queue. FIFO.
pub fn enqueue_prompt(session: &mut crate::app::session::UiSession, prompt: PromptState) {
    session.prompt_queue.push_back(prompt);
}

/// One-line summary of the tool's args for display in the dock header.
/// Bash → command; Edit → file_path; Read → file_path; etc.
fn summarize_tool_args(tool_call: &forge_primitives::session_update::ToolCall) -> String {
    let Some(raw) = &tool_call.raw_input else {
        return String::new();
    };
    // Common known fields. Fall back to JSON-compact for unknowns.
    if let Some(cmd) = raw.get("command").and_then(|v| v.as_str()) {
        return cmd.to_string();
    }
    if let Some(path) = raw.get("file_path").and_then(|v| v.as_str()) {
        return path.to_string();
    }
    if let Some(url) = raw.get("url").and_then(|v| v.as_str()) {
        return url.to_string();
    }
    serde_json::to_string(raw).unwrap_or_default()
}

/// Outcome of a single key event dispatched into the prompt widget.
/// The caller (top-level [`dispatch_key`]) interprets each variant:
/// `Consumed` ends routing; `Submit` / `Cancel` trigger session-level
/// helpers; `Unhandled` lets the key fall through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptKeyOutcome {
    /// Key consumed by the prompt; no further routing.
    Consumed,
    /// User pressed Enter on a non-modal option — caller should submit.
    Submit,
    /// User pressed Esc — caller should cancel.
    Cancel,
    /// Key not handled by the prompt; caller may pass it elsewhere.
    Unhandled,
}

/// Handle a key while the prompt is in [`PromptMode::OptionPicker`].
/// Arrow keys move the cursor (wrapping at edges); Home/End jump to
/// first/last; Space toggles in multi-select; Enter either submits
/// or transitions the prompt into an editor sub-mode if the focused
/// option implies one.
pub fn handle_key_option_picker(prompt: &mut PromptState, key: KeyEvent) -> PromptKeyOutcome {
    use forge_primitives::permission_ui::PermissionOptionKind as Kind;
    let len = prompt.options.len();
    if len == 0 {
        return PromptKeyOutcome::Unhandled;
    }
    match key.code {
        KeyCode::Up | KeyCode::Left => {
            prompt.focused_option_index =
                if prompt.focused_option_index == 0 { len - 1 } else { prompt.focused_option_index - 1 };
            PromptKeyOutcome::Consumed
        }
        KeyCode::Down | KeyCode::Right => {
            prompt.focused_option_index = (prompt.focused_option_index + 1) % len;
            PromptKeyOutcome::Consumed
        }
        KeyCode::Home => {
            prompt.focused_option_index = 0;
            PromptKeyOutcome::Consumed
        }
        KeyCode::End => {
            prompt.focused_option_index = len - 1;
            PromptKeyOutcome::Consumed
        }
        KeyCode::Char(' ') if prompt.is_multi_select() => {
            let idx = prompt.focused_option_index;
            if !prompt.selected_option_indices.insert(idx) {
                prompt.selected_option_indices.remove(&idx);
            }
            PromptKeyOutcome::Consumed
        }
        KeyCode::Enter => {
            let focused_kind = prompt.options.get(prompt.focused_option_index).map(|o| o.kind);
            match focused_kind {
                Some(Kind::Notes) => {
                    prompt.mode = PromptMode::NotesEditor;
                    PromptKeyOutcome::Consumed
                }
                Some(Kind::Edit) => {
                    prompt.mode = PromptMode::EditingInput;
                    PromptKeyOutcome::Consumed
                }
                _ => PromptKeyOutcome::Submit,
            }
        }
        KeyCode::Esc => PromptKeyOutcome::Cancel,
        // Swallow printable chars / Backspace / Delete / Tab so they
        // don't leak through to the input editor below.
        KeyCode::Char(_)
        | KeyCode::Backspace
        | KeyCode::Delete
        | KeyCode::Tab
        | KeyCode::BackTab => PromptKeyOutcome::Consumed,
        _ => PromptKeyOutcome::Unhandled,
    }
}

/// Handle a key while the prompt is in [`PromptMode::NotesEditor`].
/// Behaves like a single-line text editor over `prompt.notes` plus
/// the option-picker escape hatches: Up/Down exits to OptionPicker
/// AND moves the option cursor in one step; Esc exits the editor
/// (and un-toggles the notes option in multi-select if notes is
/// empty); Enter submits.
pub fn handle_key_notes_editor(prompt: &mut PromptState, key: KeyEvent) -> PromptKeyOutcome {
    match key.code {
        KeyCode::Char(ch) if is_printable_text_modifiers(key.modifiers) => {
            let byte_idx = prompt
                .notes
                .char_indices()
                .nth(prompt.notes_cursor)
                .map_or(prompt.notes.len(), |(i, _)| i);
            prompt.notes.insert(byte_idx, ch);
            prompt.notes_cursor += 1;
            PromptKeyOutcome::Consumed
        }
        KeyCode::Backspace if prompt.notes_cursor > 0 => {
            let start = prompt
                .notes
                .char_indices()
                .nth(prompt.notes_cursor - 1)
                .map_or(0, |(i, _)| i);
            let end = prompt
                .notes
                .char_indices()
                .nth(prompt.notes_cursor)
                .map_or(prompt.notes.len(), |(i, _)| i);
            prompt.notes.replace_range(start..end, "");
            prompt.notes_cursor -= 1;
            PromptKeyOutcome::Consumed
        }
        KeyCode::Delete if prompt.notes_cursor < prompt.notes.chars().count() => {
            let start = prompt
                .notes
                .char_indices()
                .nth(prompt.notes_cursor)
                .map_or(prompt.notes.len(), |(i, _)| i);
            let end = prompt
                .notes
                .char_indices()
                .nth(prompt.notes_cursor + 1)
                .map_or(prompt.notes.len(), |(i, _)| i);
            prompt.notes.replace_range(start..end, "");
            PromptKeyOutcome::Consumed
        }
        KeyCode::Left if prompt.notes_cursor > 0 => {
            prompt.notes_cursor -= 1;
            PromptKeyOutcome::Consumed
        }
        KeyCode::Right if prompt.notes_cursor < prompt.notes.chars().count() => {
            prompt.notes_cursor += 1;
            PromptKeyOutcome::Consumed
        }
        KeyCode::Home => {
            prompt.notes_cursor = 0;
            PromptKeyOutcome::Consumed
        }
        KeyCode::End => {
            prompt.notes_cursor = prompt.notes.chars().count();
            PromptKeyOutcome::Consumed
        }
        KeyCode::Up | KeyCode::Down => {
            // Exit editor, move option focus in one step.
            prompt.mode = PromptMode::OptionPicker;
            handle_key_option_picker(prompt, key)
        }
        KeyCode::Enter => PromptKeyOutcome::Submit,
        KeyCode::Esc => {
            prompt.mode = PromptMode::OptionPicker;
            // In multi-select, an empty-notes Escape un-toggles the
            // notes option so the user doesn't get stuck with a
            // "selected but empty" entry.
            if prompt.is_multi_select() && prompt.notes.is_empty() {
                prompt.selected_option_indices.remove(&prompt.focused_option_index);
            }
            PromptKeyOutcome::Consumed
        }
        _ => PromptKeyOutcome::Unhandled,
    }
}

/// Stub for Edit-mode handling (`PermissionAction::AllowWithInput`).
/// Full inline tool-args editor is deferred per spec §10. For now:
/// Esc returns to OptionPicker; Enter submits (with `edited_input`
/// still `None`, which the dispatcher in Task 7 falls back to plain
/// `allow()`). Any other key is consumed so it can't leak through.
pub fn handle_key_editing_input(prompt: &mut PromptState, key: KeyEvent) -> PromptKeyOutcome {
    match key.code {
        KeyCode::Esc => {
            prompt.mode = PromptMode::OptionPicker;
            PromptKeyOutcome::Consumed
        }
        KeyCode::Enter => PromptKeyOutcome::Submit,
        _ => PromptKeyOutcome::Consumed,
    }
}

/// Top-level keymap dispatch for the unified prompt. Called BEFORE
/// the normal focus-owner dispatch when the active session has a
/// prompt at the head of its queue. Returns `true` if the key was
/// consumed (no further routing); `false` if it should fall through
/// to the normal key handler.
pub fn dispatch_key(app: &mut crate::app::App, key: KeyEvent) -> bool {
    let Some(session) = app.try_active_bucket_mut() else {
        return false;
    };
    let Some(prompt) = session.prompt_queue.front_mut() else {
        return false;
    };
    let outcome = match prompt.mode {
        PromptMode::OptionPicker => handle_key_option_picker(prompt, key),
        PromptMode::NotesEditor => handle_key_notes_editor(prompt, key),
        PromptMode::EditingInput => handle_key_editing_input(prompt, key),
    };
    match outcome {
        PromptKeyOutcome::Consumed => true,
        PromptKeyOutcome::Submit => {
            submit_prompt(app);
            true
        }
        PromptKeyOutcome::Cancel => {
            cancel_prompt(app);
            true
        }
        PromptKeyOutcome::Unhandled => false,
    }
}

/// Pop the head prompt from the active session's queue. Returns
/// `None` if there's no active session or the queue is empty.
fn pop_prompt(session: &mut crate::app::session::UiSession) -> Option<PromptState> {
    session.prompt_queue.pop_front()
}

/// Pop the head prompt from the active session's queue and dispatch
/// the user's pick as a `Command::RespondPermission` or
/// `RespondQuestion` (depending on the prompt's source). After the
/// pop, restore any captured input draft if the queue is now empty.
pub fn submit_prompt(app: &mut crate::app::App) {
    use forge_primitives::permission_ui::{PermissionOptionKind, PermissionOutcome};
    use forge_primitives::question::{QuestionAnnotation, QuestionOutcome};

    let Some(key) = app.active_session_key.clone() else {
        return;
    };
    let Some(session) = app.try_active_bucket_mut() else {
        return;
    };
    let Some(prompt) = pop_prompt(session) else {
        return;
    };

    let trimmed_notes = prompt.notes.trim();
    let notes_text =
        if trimmed_notes.is_empty() { None } else { Some(trimmed_notes.to_owned()) };

    match &prompt.source {
        PromptSource::Permission { .. } => {
            let Some(option) = prompt.options.get(prompt.focused_option_index).cloned() else {
                restore_draft_if_empty_queue(app);
                return;
            };
            let outcome = PermissionOutcome::Selected {
                option_id: option.option_id,
                action: option.action,
                notes_text,
                edited_input: prompt.edited_input.clone(),
            };
            crate::app::events::turn::dispatch_permission_outcome(
                app,
                &key,
                &prompt.tool_id,
                outcome,
            );
        }
        PromptSource::Question { prompt: q, .. } => {
            let selected_indices: Vec<usize> = if q.multi_select {
                if prompt.selected_option_indices.is_empty() {
                    vec![prompt.focused_option_index]
                } else {
                    prompt.selected_option_indices.iter().copied().collect()
                }
            } else {
                vec![prompt.focused_option_index]
            };
            // Convert indices → option_ids, FILTERING OUT the
            // forge-synthesized notes-option (its content goes into
            // `annotation.notes`; it doesn't exist on the wire).
            let selected_option_ids: Vec<String> = selected_indices
                .iter()
                .filter_map(|&i| prompt.options.get(i))
                .filter(|o| !matches!(o.kind, PermissionOptionKind::Notes))
                .map(|o| o.option_id.clone())
                .collect();
            let annotation = notes_text
                .as_ref()
                .map(|n| QuestionAnnotation { preview: None, notes: Some(n.clone()) });
            let outcome = if selected_option_ids.is_empty() && annotation.is_none() {
                QuestionOutcome::Cancelled
            } else {
                QuestionOutcome::Answered { selected_option_ids, annotation }
            };
            crate::app::events::turn::dispatch_question_outcome(
                app,
                &key,
                &prompt.tool_id,
                outcome,
            );
        }
    }

    restore_draft_if_empty_queue(app);
}

/// Pop the head prompt and dispatch a `Cancelled` outcome to the
/// workspace. After the pop, restore any captured input draft if the
/// queue is now empty.
pub fn cancel_prompt(app: &mut crate::app::App) {
    use forge_primitives::permission_ui::PermissionOutcome;
    use forge_primitives::question::QuestionOutcome;

    let Some(key) = app.active_session_key.clone() else {
        return;
    };
    let Some(session) = app.try_active_bucket_mut() else {
        return;
    };
    let Some(prompt) = pop_prompt(session) else {
        return;
    };

    match prompt.source {
        PromptSource::Permission { .. } => {
            crate::app::events::turn::dispatch_permission_outcome(
                app,
                &key,
                &prompt.tool_id,
                PermissionOutcome::Cancelled,
            );
        }
        PromptSource::Question { .. } => {
            crate::app::events::turn::dispatch_question_outcome(
                app,
                &key,
                &prompt.tool_id,
                QuestionOutcome::Cancelled,
            );
        }
    }

    restore_draft_if_empty_queue(app);
}

/// Snapshot the chat-input draft if it hasn't been snapshotted yet.
/// Idempotent — when multiple prompts arrive in a burst the first
/// snapshot wins, so subsequent prompts don't clobber the original
/// draft. Called from the inbound event handler right after
/// [`enqueue_prompt`]. No-op when the editor is empty (nothing to
/// preserve).
pub fn snapshot_draft_if_needed(app: &mut crate::app::App) {
    if app.input_draft_snapshot.is_some() {
        return;
    }
    let text = app.input().text();
    if !text.is_empty() {
        app.input_draft_snapshot = Some(text);
        app.input_mut().clear();
    }
}

/// Restore a previously-snapshotted draft into the chat input when
/// the active session's prompt queue is empty. Called from
/// [`submit_prompt`] / [`cancel_prompt`] AFTER popping. No-op when
/// there's no snapshot or the queue still has prompts pending.
pub fn restore_draft_if_empty_queue(app: &mut crate::app::App) {
    let queue_empty =
        app.active_session().is_none_or(|s| s.prompt_queue.is_empty());
    if queue_empty
        && let Some(draft) = app.input_draft_snapshot.take()
    {
        app.input_mut().set_text(&draft);
    }
}

/// Modifier-mask predicate matching plain typing (or AltGr-style
/// Ctrl+Alt combinations on Windows / X11). Mirrors the same helper
/// in `app::keys`; duplicated here to avoid a `pub(crate)` widening
/// that Task 24 would have to undo when the legacy reclaim path is
/// deleted.
fn is_printable_text_modifiers(modifiers: crossterm::event::KeyModifiers) -> bool {
    use crossterm::event::KeyModifiers;
    let ctrl_alt =
        modifiers.contains(KeyModifiers::CONTROL) && modifiers.contains(KeyModifiers::ALT);
    !modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) || ctrl_alt
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use forge_primitives::permission_ui::{PermissionAction, PermissionOptionKind};
    use forge_primitives::session_update::ToolCall;

    pub(crate) fn make_question_request(multi_select: bool) -> QuestionRequest {
        QuestionRequest {
            tool_call: ToolCall {
                tool_call_id: "tc-q".into(),
                title: "AskUserQuestion".into(),
                kind: "execute".into(),
                status: "pending".into(),
                content: vec![],
                raw_input: None,
                raw_output: None,
                output_metadata: None,
                task_metadata: None,
                locations: vec![],
                meta: None,
            },
            prompt: QuestionPrompt {
                question: "Pick a colour".into(),
                header: "Colour".into(),
                multi_select,
                options: vec![
                    forge_primitives::question::QuestionOption {
                        option_id: "q0".into(),
                        label: "Red".into(),
                        description: None,
                        preview: None,
                    },
                    forge_primitives::question::QuestionOption {
                        option_id: "q1".into(),
                        label: "Blue".into(),
                        description: None,
                        preview: None,
                    },
                ],
            },
            question_index: 0,
            total_questions: 1,
        }
    }

    pub(crate) fn make_permission_request() -> PermissionRequest {
        PermissionRequest {
            tool_call: ToolCall {
                tool_call_id: "tc-1".into(),
                title: "Bash".into(),
                kind: "execute".into(),
                status: "pending".into(),
                content: vec![],
                raw_input: Some(serde_json::json!({"command": "git push"})),
                raw_output: None,
                output_metadata: None,
                task_metadata: None,
                locations: vec![],
                meta: None,
            },
            options: vec![
                PermissionOption {
                    option_id: "allow_once".into(),
                    name: "Allow once".into(),
                    kind: PermissionOptionKind::Allow,
                    action: PermissionAction::Allow,
                },
                PermissionOption {
                    option_id: "deny".into(),
                    name: "Deny".into(),
                    kind: PermissionOptionKind::Deny,
                    action: PermissionAction::Deny,
                },
            ],
            display: None,
        }
    }

    #[test]
    fn from_permission_appends_tell_claude_escape_hatch() {
        let state = PromptState::from_permission("tc-1".into(), make_permission_request());
        assert_eq!(state.options.len(), 3);
        let last = state.options.last().expect("last option");
        assert_eq!(last.kind, PermissionOptionKind::Notes);
        assert_eq!(last.action, PermissionAction::Deny);
    }

    #[test]
    fn from_permission_starts_in_option_picker_mode() {
        let state = PromptState::from_permission("tc-1".into(), make_permission_request());
        assert_eq!(state.mode, PromptMode::OptionPicker);
    }

    #[test]
    fn from_permission_focused_index_is_zero() {
        let state = PromptState::from_permission("tc-1".into(), make_permission_request());
        assert_eq!(state.focused_option_index, 0);
    }

    #[test]
    fn from_question_appends_notes_option() {
        let state = PromptState::from_question("tc-q".into(), make_question_request(false));
        assert_eq!(state.options.len(), 3);
        let last = state.options.last().expect("last option");
        assert_eq!(last.kind, PermissionOptionKind::Notes);
    }

    #[test]
    fn from_question_preserves_multi_select_flag() {
        let state = PromptState::from_question("tc-q".into(), make_question_request(true));
        assert!(state.is_multi_select());
    }

    #[test]
    fn enqueue_appends_to_session_queue() {
        let mut session = crate::app::session::UiSession::default();
        enqueue_prompt(
            &mut session,
            PromptState::from_permission("tc-1".into(), make_permission_request()),
        );
        enqueue_prompt(
            &mut session,
            PromptState::from_permission("tc-2".into(), make_permission_request()),
        );
        assert_eq!(session.prompt_queue.len(), 2);
        assert_eq!(session.prompt_queue.front().expect("head").tool_id, "tc-1");
    }

    // ── Task 16 ─ handle_key_option_picker ─────────────────────────

    use crossterm::event::KeyModifiers;

    #[test]
    fn arrow_down_advances_focused_option() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        assert_eq!(prompt.focused_option_index, 0);
        let outcome =
            handle_key_option_picker(&mut prompt, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(outcome, PromptKeyOutcome::Consumed);
        assert_eq!(prompt.focused_option_index, 1);
    }

    #[test]
    fn arrow_up_at_top_wraps_to_bottom() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let last = prompt.options.len() - 1;
        let _ =
            handle_key_option_picker(&mut prompt, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(prompt.focused_option_index, last);
    }

    #[test]
    fn arrow_right_advances_focus_like_down() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let _ = handle_key_option_picker(
            &mut prompt,
            KeyEvent::new(KeyCode::Right, KeyModifiers::NONE),
        );
        assert_eq!(prompt.focused_option_index, 1);
    }

    #[test]
    fn arrow_left_retreats_focus_like_up() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.focused_option_index = 1;
        let _ =
            handle_key_option_picker(&mut prompt, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(prompt.focused_option_index, 0);
    }

    #[test]
    fn home_jumps_to_first_option() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.focused_option_index = 2;
        let _ =
            handle_key_option_picker(&mut prompt, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(prompt.focused_option_index, 0);
    }

    #[test]
    fn end_jumps_to_last_option() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let last = prompt.options.len() - 1;
        let _ =
            handle_key_option_picker(&mut prompt, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(prompt.focused_option_index, last);
    }

    #[test]
    fn space_toggles_in_multi_select_only() {
        let mut single = PromptState::from_question("tc-q".into(), make_question_request(false));
        let _ = handle_key_option_picker(
            &mut single,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        assert!(single.selected_option_indices.is_empty(), "single-select space is no-op");

        let mut multi = PromptState::from_question("tc-q2".into(), make_question_request(true));
        let _ = handle_key_option_picker(
            &mut multi,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        let mut expected = BTreeSet::new();
        expected.insert(0);
        assert_eq!(multi.selected_option_indices, expected);

        // Second Space on the same index toggles off.
        let _ = handle_key_option_picker(
            &mut multi,
            KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
        );
        assert!(multi.selected_option_indices.is_empty(), "second Space toggles off");
    }

    #[test]
    fn enter_on_notes_option_transitions_to_notes_editor_mode() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let notes_idx = prompt
            .options
            .iter()
            .position(|o| matches!(o.kind, PermissionOptionKind::Notes))
            .expect("Tell Claude option present");
        prompt.focused_option_index = notes_idx;
        let outcome =
            handle_key_option_picker(&mut prompt, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(prompt.mode, PromptMode::NotesEditor);
        assert_eq!(outcome, PromptKeyOutcome::Consumed);
    }

    #[test]
    fn enter_on_allow_option_emits_submit() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        // focused_option_index is 0 by default — should be an Allow option.
        let outcome =
            handle_key_option_picker(&mut prompt, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(outcome, PromptKeyOutcome::Submit);
    }

    #[test]
    fn esc_emits_cancel() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let outcome =
            handle_key_option_picker(&mut prompt, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(outcome, PromptKeyOutcome::Cancel);
    }

    #[test]
    fn printable_chars_are_swallowed_in_option_picker() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let outcome = handle_key_option_picker(
            &mut prompt,
            KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE),
        );
        assert_eq!(outcome, PromptKeyOutcome::Consumed);
    }

    #[test]
    fn tab_is_consumed_in_option_picker() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let outcome =
            handle_key_option_picker(&mut prompt, KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(outcome, PromptKeyOutcome::Consumed);
    }

    #[test]
    fn backspace_is_consumed_in_option_picker() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        let outcome = handle_key_option_picker(
            &mut prompt,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(outcome, PromptKeyOutcome::Consumed);
    }

    // ── Task 17 ─ handle_key_notes_editor ──────────────────────────

    #[test]
    fn printable_chars_insert_into_notes_in_editor_mode() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::NotesEditor;
        let _ = handle_key_notes_editor(
            &mut prompt,
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::NONE),
        );
        let _ = handle_key_notes_editor(
            &mut prompt,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
        );
        assert_eq!(prompt.notes, "hi");
        assert_eq!(prompt.notes_cursor, 2);
    }

    #[test]
    fn backspace_removes_char_in_editor_mode() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::NotesEditor;
        prompt.notes = "hi".into();
        prompt.notes_cursor = 2;
        let _ = handle_key_notes_editor(
            &mut prompt,
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
        );
        assert_eq!(prompt.notes, "h");
        assert_eq!(prompt.notes_cursor, 1);
    }

    #[test]
    fn delete_removes_char_at_cursor_in_editor_mode() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::NotesEditor;
        prompt.notes = "hi".into();
        prompt.notes_cursor = 0;
        let _ = handle_key_notes_editor(
            &mut prompt,
            KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE),
        );
        assert_eq!(prompt.notes, "i");
        assert_eq!(prompt.notes_cursor, 0);
    }

    #[test]
    fn left_right_move_cursor_in_notes_editor() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::NotesEditor;
        prompt.notes = "abc".into();
        prompt.notes_cursor = 1;
        let _ =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::Left, KeyModifiers::NONE));
        assert_eq!(prompt.notes_cursor, 0);
        let _ =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::Right, KeyModifiers::NONE));
        assert_eq!(prompt.notes_cursor, 1);
    }

    #[test]
    fn home_end_jump_in_notes_editor() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::NotesEditor;
        prompt.notes = "abc".into();
        prompt.notes_cursor = 1;
        let _ =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert_eq!(prompt.notes_cursor, 3);
        let _ =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(prompt.notes_cursor, 0);
    }

    #[test]
    fn esc_in_notes_editor_returns_to_option_picker() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::NotesEditor;
        let outcome =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(prompt.mode, PromptMode::OptionPicker);
        assert_eq!(outcome, PromptKeyOutcome::Consumed);
    }

    #[test]
    fn esc_in_multi_select_empty_notes_untoggles_notes_option() {
        let mut prompt = PromptState::from_question("tc-q".into(), make_question_request(true));
        prompt.mode = PromptMode::NotesEditor;
        // Pretend the notes option was toggled on, then user opened
        // the editor, didn't type anything, and pressed Esc.
        let notes_idx = prompt
            .options
            .iter()
            .position(|o| matches!(o.kind, PermissionOptionKind::Notes))
            .expect("notes option present");
        prompt.focused_option_index = notes_idx;
        prompt.selected_option_indices.insert(notes_idx);
        prompt.notes.clear();
        let _ =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!prompt.selected_option_indices.contains(&notes_idx));
    }

    #[test]
    fn esc_in_multi_select_keeps_notes_option_when_text_present() {
        let mut prompt = PromptState::from_question("tc-q".into(), make_question_request(true));
        prompt.mode = PromptMode::NotesEditor;
        let notes_idx = prompt
            .options
            .iter()
            .position(|o| matches!(o.kind, PermissionOptionKind::Notes))
            .expect("notes option present");
        prompt.focused_option_index = notes_idx;
        prompt.selected_option_indices.insert(notes_idx);
        prompt.notes = "thoughts".into();
        let _ =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(prompt.selected_option_indices.contains(&notes_idx));
    }

    #[test]
    fn enter_in_notes_editor_submits() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::NotesEditor;
        prompt.notes = "use --dry-run".into();
        let outcome =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(outcome, PromptKeyOutcome::Submit);
    }

    #[test]
    fn up_in_notes_editor_exits_and_moves_option_focus() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::NotesEditor;
        prompt.focused_option_index = 1;
        let _ =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(prompt.mode, PromptMode::OptionPicker);
        assert_eq!(prompt.focused_option_index, 0);
    }

    #[test]
    fn down_in_notes_editor_exits_and_moves_option_focus() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::NotesEditor;
        prompt.focused_option_index = 0;
        let _ =
            handle_key_notes_editor(&mut prompt, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(prompt.mode, PromptMode::OptionPicker);
        assert_eq!(prompt.focused_option_index, 1);
    }

    // ── Task 18 ─ handle_key_editing_input + dispatch_key ──────────

    #[test]
    fn esc_in_editing_input_returns_to_option_picker() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::EditingInput;
        let outcome =
            handle_key_editing_input(&mut prompt, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(prompt.mode, PromptMode::OptionPicker);
        assert_eq!(outcome, PromptKeyOutcome::Consumed);
    }

    #[test]
    fn enter_in_editing_input_submits() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::EditingInput;
        let outcome = handle_key_editing_input(
            &mut prompt,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        );
        assert_eq!(outcome, PromptKeyOutcome::Submit);
    }

    #[test]
    fn other_keys_in_editing_input_are_consumed() {
        let mut prompt = PromptState::from_permission("tc-1".into(), make_permission_request());
        prompt.mode = PromptMode::EditingInput;
        let outcome = handle_key_editing_input(
            &mut prompt,
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        );
        assert_eq!(outcome, PromptKeyOutcome::Consumed);
    }

    #[test]
    fn dispatch_key_routes_through_option_picker_when_queue_non_empty() {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("active session");
        if let Some(session) = app.session_mut(&key) {
            enqueue_prompt(
                session,
                PromptState::from_permission("tc-1".into(), make_permission_request()),
            );
        }
        let handled = dispatch_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(handled);
        let session = app.session_mut(&key).expect("session");
        let prompt = session.prompt_queue.front().expect("prompt");
        assert_eq!(prompt.focused_option_index, 1);
    }

    #[test]
    fn dispatch_key_returns_false_when_no_prompt_in_queue() {
        let mut app = crate::app::App::test_default();
        let handled = dispatch_key(&mut app, KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        assert!(!handled);
    }

    // ── Task 19 ─ submit_prompt / cancel_prompt ───────────────────

    #[test]
    fn submit_with_allow_dispatches_respond_permission_with_allow_outcome() {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            enqueue_prompt(
                session,
                PromptState::from_permission("tc-1".into(), make_permission_request()),
            );
        }
        submit_prompt(&mut app);
        // The queue head should be popped.
        let session = app.session_mut(&key).expect("session");
        assert!(session.prompt_queue.is_empty(), "queue drained after submit");
        // The captured outcome should be Selected{ option_id: "allow_once", Allow }.
        let outcome = crate::app::events::turn::test_capture::try_take_dispatched_permission_outcome(
            &app, "tc-1",
        )
        .expect("permission outcome captured");
        match outcome {
            forge_primitives::PermissionOutcome::Selected { option_id, action, .. } => {
                assert_eq!(option_id, "allow_once");
                assert!(matches!(action, forge_primitives::permission_ui::PermissionAction::Allow));
            }
            other => panic!("expected Selected, got: {other:?}"),
        }
    }

    #[test]
    fn cancel_dispatches_cancelled_outcome() {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            enqueue_prompt(
                session,
                PromptState::from_permission("tc-1".into(), make_permission_request()),
            );
        }
        cancel_prompt(&mut app);
        let session = app.session_mut(&key).expect("session");
        assert!(session.prompt_queue.is_empty(), "queue drained after cancel");
        let outcome = crate::app::events::turn::test_capture::try_take_dispatched_permission_outcome(
            &app, "tc-1",
        )
        .expect("permission outcome captured");
        assert!(
            matches!(outcome, forge_primitives::PermissionOutcome::Cancelled),
            "expected Cancelled, got: {outcome:?}",
        );
    }

    #[test]
    fn cancel_question_dispatches_cancelled_outcome() {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            enqueue_prompt(
                session,
                PromptState::from_question("tc-q".into(), make_question_request(false)),
            );
        }
        cancel_prompt(&mut app);
        let outcome = crate::app::events::turn::test_capture::try_take_dispatched_question_outcome(
            &app, "tc-q",
        )
        .expect("question outcome captured");
        assert!(
            matches!(outcome, forge_primitives::QuestionOutcome::Cancelled),
            "expected Cancelled, got: {outcome:?}",
        );
    }

    #[test]
    fn submit_question_single_select_uses_focused_option_id() {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            let mut prompt =
                PromptState::from_question("tc-q".into(), make_question_request(false));
            // Focus the second option ("q1" / Blue).
            prompt.focused_option_index = 1;
            enqueue_prompt(session, prompt);
        }
        submit_prompt(&mut app);
        let outcome = crate::app::events::turn::test_capture::try_take_dispatched_question_outcome(
            &app, "tc-q",
        )
        .expect("question outcome captured");
        match outcome {
            forge_primitives::QuestionOutcome::Answered { selected_option_ids, annotation } => {
                assert_eq!(selected_option_ids, vec!["q1".to_string()]);
                assert!(annotation.is_none());
            }
            other => panic!("expected Answered, got: {other:?}"),
        }
    }

    #[test]
    fn submit_question_with_notes_routes_notes_via_annotation() {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            let mut prompt =
                PromptState::from_question("tc-q".into(), make_question_request(false));
            // Focus the notes-option (last option) and load notes text.
            prompt.focused_option_index = prompt.options.len() - 1;
            prompt.mode = PromptMode::NotesEditor;
            prompt.notes = "use the other option".into();
            enqueue_prompt(session, prompt);
        }
        submit_prompt(&mut app);
        let outcome = crate::app::events::turn::test_capture::try_take_dispatched_question_outcome(
            &app, "tc-q",
        )
        .expect("question outcome captured");
        match outcome {
            forge_primitives::QuestionOutcome::Answered { selected_option_ids, annotation } => {
                // The notes-option is forge-synthesized; it must NOT
                // appear in selected_option_ids — its content rides in
                // annotation.notes instead.
                assert!(
                    !selected_option_ids.iter().any(|id| id == "tell_claude"),
                    "tell_claude must be filtered out of selected_option_ids: {selected_option_ids:?}",
                );
                let ann = annotation.expect("annotation populated");
                assert_eq!(ann.notes.as_deref(), Some("use the other option"));
            }
            other => panic!("expected Answered, got: {other:?}"),
        }
    }

    #[test]
    fn submit_question_multi_select_uses_toggled_indices_and_filters_notes() {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            let mut prompt =
                PromptState::from_question("tc-q".into(), make_question_request(true));
            // Toggle q0, q1, AND the notes option; provide notes text.
            prompt.selected_option_indices.insert(0);
            prompt.selected_option_indices.insert(1);
            prompt.selected_option_indices.insert(prompt.options.len() - 1);
            prompt.notes = "extra context".into();
            enqueue_prompt(session, prompt);
        }
        submit_prompt(&mut app);
        let outcome = crate::app::events::turn::test_capture::try_take_dispatched_question_outcome(
            &app, "tc-q",
        )
        .expect("question outcome captured");
        match outcome {
            forge_primitives::QuestionOutcome::Answered { selected_option_ids, annotation } => {
                assert_eq!(selected_option_ids, vec!["q0".to_string(), "q1".to_string()]);
                let ann = annotation.expect("annotation populated");
                assert_eq!(ann.notes.as_deref(), Some("extra context"));
            }
            other => panic!("expected Answered, got: {other:?}"),
        }
    }

    #[test]
    fn submit_question_with_only_notes_focus_and_empty_text_is_cancelled() {
        // User focused notes-option, did NOT enter text, hit Enter
        // through the editor — empty selected_ids + empty annotation
        // becomes Cancelled rather than Answered with no payload.
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            let mut prompt =
                PromptState::from_question("tc-q".into(), make_question_request(true));
            prompt.focused_option_index = prompt.options.len() - 1;
            prompt.mode = PromptMode::NotesEditor;
            // Multi-select with empty toggled set + notes focus + no text.
            enqueue_prompt(session, prompt);
        }
        submit_prompt(&mut app);
        let outcome = crate::app::events::turn::test_capture::try_take_dispatched_question_outcome(
            &app, "tc-q",
        )
        .expect("question outcome captured");
        assert!(
            matches!(outcome, forge_primitives::QuestionOutcome::Cancelled),
            "empty answer with no notes should be Cancelled, got: {outcome:?}",
        );
    }

    #[test]
    fn submit_permission_with_notes_text_attaches_to_outcome() {
        let mut app = crate::app::App::test_default();
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            let mut prompt =
                PromptState::from_permission("tc-1".into(), make_permission_request());
            // Focus the notes-option (last); enter notes editor with text.
            prompt.focused_option_index = prompt.options.len() - 1;
            prompt.mode = PromptMode::NotesEditor;
            prompt.notes = "don't push to main".into();
            enqueue_prompt(session, prompt);
        }
        submit_prompt(&mut app);
        let outcome = crate::app::events::turn::test_capture::try_take_dispatched_permission_outcome(
            &app, "tc-1",
        )
        .expect("permission outcome captured");
        match outcome {
            forge_primitives::PermissionOutcome::Selected {
                option_id,
                notes_text,
                ..
            } => {
                assert_eq!(option_id, "tell_claude");
                assert_eq!(notes_text.as_deref(), Some("don't push to main"));
            }
            other => panic!("expected Selected, got: {other:?}"),
        }
    }

    // ── Task 20 ─ draft input preservation across morph ───────────

    #[test]
    fn draft_preserved_across_morph_and_restored_when_queue_empties() {
        let mut app = crate::app::App::test_default();
        app.input_mut().set_text("draft message I was typing");
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            enqueue_prompt(
                session,
                PromptState::from_permission("tc-1".into(), make_permission_request()),
            );
        }
        snapshot_draft_if_needed(&mut app);
        assert_eq!(app.input().text(), "", "input cleared while prompt active");
        // User responds; prompt is popped.
        submit_prompt(&mut app);
        // Draft restored.
        assert_eq!(app.input().text(), "draft message I was typing");
    }

    #[test]
    fn snapshot_noop_when_input_empty() {
        let mut app = crate::app::App::test_default();
        // input is empty by default; snapshot should NOT capture an
        // empty string (avoid masking "no draft" with Some("")).
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            enqueue_prompt(
                session,
                PromptState::from_permission("tc-1".into(), make_permission_request()),
            );
        }
        snapshot_draft_if_needed(&mut app);
        assert!(app.input_draft_snapshot.is_none(), "no draft to snapshot");
        submit_prompt(&mut app);
        assert_eq!(app.input().text(), "");
    }

    #[test]
    fn snapshot_is_idempotent_across_multiple_prompts() {
        let mut app = crate::app::App::test_default();
        app.input_mut().set_text("first draft");
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            enqueue_prompt(
                session,
                PromptState::from_permission("tc-1".into(), make_permission_request()),
            );
        }
        snapshot_draft_if_needed(&mut app);
        // A second prompt arrives — must NOT overwrite the snapshot.
        if let Some(session) = app.session_mut(&key) {
            enqueue_prompt(
                session,
                PromptState::from_permission("tc-2".into(), make_permission_request()),
            );
        }
        snapshot_draft_if_needed(&mut app);
        // Resolve first prompt — queue still has tc-2, draft NOT restored.
        submit_prompt(&mut app);
        assert_eq!(app.input().text(), "", "queue non-empty, draft not yet restored");
        // Resolve second prompt — queue empty, draft restored.
        submit_prompt(&mut app);
        assert_eq!(app.input().text(), "first draft");
    }

    #[test]
    fn cancel_restores_draft_when_queue_drains() {
        let mut app = crate::app::App::test_default();
        app.input_mut().set_text("partial thought");
        let key = app.active_session_key.clone().expect("session");
        if let Some(session) = app.session_mut(&key) {
            enqueue_prompt(
                session,
                PromptState::from_permission("tc-1".into(), make_permission_request()),
            );
        }
        snapshot_draft_if_needed(&mut app);
        cancel_prompt(&mut app);
        assert_eq!(app.input().text(), "partial thought");
    }
}
