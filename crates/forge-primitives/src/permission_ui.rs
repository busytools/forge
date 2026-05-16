//! UI-side permission-prompt shapes — `PermissionRequest` (what the
//! agent surfaces when the CLI asks for tool approval) and
//! `PermissionOutcome` (what the user picks). Distinct from the
//! wire-side decision types in [`crate::permissions`] which talk to
//! the SDK's `can_use_tool` callback.

use serde::{Deserialize, Serialize};

use crate::session_update::ToolCall;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub description: Option<String>,
    pub kind: String,
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
}

impl PermissionDisplay {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn title(mut self, title: Option<String>) -> Self {
        self.title = title;
        self
    }

    pub fn display_name(mut self, display_name: Option<String>) -> Self {
        self.display_name = display_name;
        self
    }

    pub fn description(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }

    /// `true` iff every field is None or contains only whitespace.
    pub fn is_empty(&self) -> bool {
        self.title.as_ref().is_none_or(|value| value.trim().is_empty())
            && self.display_name.as_ref().is_none_or(|value| value.trim().is_empty())
            && self.description.as_ref().is_none_or(|value| value.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum PermissionOutcome {
    Selected { option_id: String },
    Cancelled,
}
