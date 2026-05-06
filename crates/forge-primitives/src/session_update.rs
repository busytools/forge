//! `SessionUpdate` — the streaming event the agent emits as it drives
//! a session. Plus everything embedded inside it: chunks, tool-call
//! envelopes, tool-output metadata, plan entries.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::runtime::{
    AvailableAgent, AvailableCommand, CompactionTrigger, CurrentModel, FastModeState, ModeState,
    RuntimeSessionState, SessionStatus,
};

/// Render-side chunk payload for streaming session updates
/// (`SessionUpdate::AgentMessageChunk` etc.). Distinct from the
/// wire-side `ContentBlock` (which carries `ToolUse`, `ToolResult`,
/// `Thinking`, server-tool variants, …) lifted from forge-sdk into
/// `crate::content::ContentBlock`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChunkContent {
    Text { text: String },
    Image { mime_type: Option<String>, uri: Option<String>, data: Option<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::struct_field_names)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    pub kind: String,
    pub status: String,
    pub content: Vec<ToolCallContent>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<String>,
    pub output_metadata: Option<ToolOutputMetadata>,
    pub task_metadata: Option<TaskMetadata>,
    pub locations: Vec<ToolLocation>,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    pub fields: ToolCallUpdateFields,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolCallUpdateFields {
    pub title: Option<String>,
    pub kind: Option<String>,
    pub status: Option<String>,
    pub content: Option<Vec<ToolCallContent>>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<String>,
    pub output_metadata: Option<ToolOutputMetadata>,
    pub task_metadata: Option<TaskMetadata>,
    pub locations: Option<Vec<ToolLocation>>,
    pub meta: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolLocation {
    pub path: String,
    pub line: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TodoWriteOutputMetadata {
    pub verification_nudge_needed: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BashOutputMetadata {
    pub assistant_auto_backgrounded: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ToolOutputMetadata {
    pub bash: Option<BashOutputMetadata>,
    pub todo_write: Option<TodoWriteOutputMetadata>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TaskMetadata {
    pub end_time: Option<u64>,
    pub total_paused_ms: Option<u64>,
    pub error: Option<String>,
    pub is_backgrounded: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    Content {
        content: ChunkContent,
    },
    Diff {
        old_path: String,
        new_path: String,
        old: String,
        new: String,
        repository: Option<String>,
    },
    McpResource {
        uri: String,
        mime_type: Option<String>,
        text: Option<String>,
        blob_saved_to: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub status: String,
    pub active_form: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionUpdate {
    AgentMessageChunk { content: ChunkContent },
    UserMessageChunk { content: ChunkContent },
    AgentThoughtChunk { content: ChunkContent },
    ToolCall { tool_call: ToolCall },
    ToolCallUpdate { tool_call_update: ToolCallUpdate },
    Plan { entries: Vec<PlanEntry> },
    AvailableCommandsUpdate { commands: Vec<AvailableCommand> },
    AvailableAgentsUpdate { agents: Vec<AvailableAgent> },
    ModeStateUpdate { mode: ModeState },
    CurrentModeUpdate { current_mode_id: String },
    CurrentModelUpdate { current_model: CurrentModel },
    ConfigOptionUpdate { option_id: String, value: Value },
    FastModeUpdate { fast_mode_state: FastModeState },
    RateLimitUpdate(crate::runtime::RateLimitUpdate),
    ApiRetryUpdate(crate::runtime::ApiRetryUpdate),
    PromptSuggestionUpdate { suggestion: String },
    RuntimeSessionStateUpdate { state: RuntimeSessionState },
    SettingsParseError(crate::runtime::SettingsParseErrorUpdate),
    SessionStatusUpdate { status: SessionStatus },
    CompactionBoundary { trigger: CompactionTrigger, pre_tokens: u64 },
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::SessionUpdate;
    use crate::{ApiRetryError, ApiRetryUpdate};

    #[test]
    fn api_retry_update_deserializes_unknown_error_defensively() {
        let update: SessionUpdate = serde_json::from_value(serde_json::json!({
            "type": "api_retry_update",
            "attempt": 1,
            "max_retries": 4,
            "retry_delay_ms": 1000,
            "error_status": null,
            "error": "transport_timeout"
        }))
        .expect("deserialize api retry update");

        assert!(matches!(
            update,
            SessionUpdate::ApiRetryUpdate(ApiRetryUpdate { error: ApiRetryError::Unknown, .. })
        ));
    }
}
