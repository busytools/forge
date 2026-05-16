//! MCP elicitation request/response — surfaced to the UI when an MCP
//! server needs the user to fill in a form or confirm an OAuth URL.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationMode {
    Form,
    Url,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

impl ElicitationAction {
    /// String the `claude` CLI expects on the wire for this action.
    pub const fn as_wire_str(self) -> &'static str {
        match self {
            Self::Accept => "accept",
            Self::Decline => "decline",
            Self::Cancel => "cancel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationRequest {
    pub request_id: String,
    pub server_name: String,
    pub message: String,
    pub mode: ElicitationMode,
    pub url: Option<String>,
    pub elicitation_id: Option<String>,
    pub requested_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElicitationResponse {
    pub action: ElicitationAction,
    pub content: Option<serde_json::Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elicitation_action_wire_strings() {
        assert_eq!(ElicitationAction::Accept.as_wire_str(), "accept");
        assert_eq!(ElicitationAction::Decline.as_wire_str(), "decline");
        assert_eq!(ElicitationAction::Cancel.as_wire_str(), "cancel");
    }
}
