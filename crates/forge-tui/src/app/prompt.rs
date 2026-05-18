//! Unified prompt state. Replaces `InlinePermission` + `InlineQuestion`
//! and the per-tool-call attach pattern.

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
}
