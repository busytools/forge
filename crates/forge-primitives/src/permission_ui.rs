//! UI-side permission-prompt shapes — `PermissionRequest` (what the
//! agent surfaces when the CLI asks for tool approval) and
//! `PermissionOutcome` (what the user picks). Distinct from the
//! wire-side decision types in [`crate::permissions`] which talk to
//! the SDK's `can_use_tool` callback.

use serde::{Deserialize, Serialize};

use crate::permissions::PermissionUpdate;
use crate::session_update::ToolCall;

/// Typed wire-side response routing for a permission option. Each
/// `PermissionOption` constructed by `forge-agent` carries an
/// `action`; the dispatcher reads it to build the right
/// `PermissionDecision` on submit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionAction {
    /// `PermissionDecision::allow()`.
    Allow,
    /// `PermissionDecision::allow().with_updated_permissions(updates)`.
    AllowWithUpdates { updates: Vec<PermissionUpdate> },
    /// Marker — the actual edited input value lives on
    /// `PromptState.edited_input` (TUI-side). Dispatcher sends
    /// `PermissionDecision::allow_with_input(edited_value)`.
    AllowWithInput,
    /// `PermissionDecision::deny(reason)` where reason is the user's
    /// notes text or a default when notes are empty.
    Deny,
}

/// Display style for a permission option — drives icon + color in the
/// prompt widget. 4 variants total; the legacy 8-variant TUI-side
/// `PermissionOptionKind` is being replaced by this in the unified
/// prompt redesign (Task 23 in the plan deletes the legacy version).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    /// Allow paths (✓ green icon).
    Allow,
    /// Deny paths (✗ red icon).
    Deny,
    /// Allow-with-edits path (✎ blue icon).
    Edit,
    /// Forge-synthesized "Tell Claude something else" escape hatch (… dim icon).
    Notes,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
    pub action: PermissionAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub tool_call: ToolCall,
    pub options: Vec<PermissionOption>,
    pub display: Option<PermissionDisplay>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PermissionDisplay {
    pub title: Option<String>,
    pub display_name: Option<String>,
    pub description: Option<String>,
    /// The CLI's reason for asking (`decision_reason` from CanUseTool).
    /// Rendered as the yellow ⚠ line in the dock header.
    pub decision_reason: Option<String>,
}

impl PermissionDisplay {
    /// `true` iff every field is None or contains only whitespace.
    pub fn is_empty(&self) -> bool {
        self.title.as_ref().is_none_or(|value| value.trim().is_empty())
            && self.display_name.as_ref().is_none_or(|value| value.trim().is_empty())
            && self.description.as_ref().is_none_or(|value| value.trim().is_empty())
            && self.decision_reason.as_ref().is_none_or(|value| value.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    Selected {
        option_id: String,
        /// The dispatch routing for this option, as set by the agent
        /// when constructing the wire `PermissionOption`. The agent's
        /// response handler reads this directly — no option_id lookup
        /// or string-prefix matching needed.
        action: PermissionAction,
        /// User's "tell Claude" feedback string (when the notes-option
        /// was toggled or the focused option is a Deny). Empty when the
        /// user picked a plain Allow without notes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        notes_text: Option<String>,
        /// Edited tool input JSON when the action is `AllowWithInput`.
        /// None otherwise.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        edited_input: Option<serde_json::Value>,
    },
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::PermissionMode;
    use crate::permissions::PermissionUpdate;

    #[test]
    fn permission_action_round_trips_through_serde() {
        let allow_with_updates = PermissionAction::AllowWithUpdates {
            updates: vec![PermissionUpdate::SetMode {
                mode: PermissionMode::Auto,
                destination: None,
            }],
        };
        let json = serde_json::to_string(&allow_with_updates).expect("serialize");
        let back: PermissionAction = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(allow_with_updates, back);
    }

    #[test]
    fn permission_outcome_selected_round_trips_with_notes_and_edited_input() {
        let outcome = PermissionOutcome::Selected {
            option_id: "allow_with_edits".into(),
            action: PermissionAction::AllowWithInput,
            notes_text: Some("trimmed args".to_owned()),
            edited_input: Some(serde_json::json!({"command": "echo trimmed"})),
        };
        let json = serde_json::to_string(&outcome).expect("serialize");
        let back: PermissionOutcome = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(outcome, back);
    }
}
