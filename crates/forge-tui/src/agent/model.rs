use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;

// SessionId lives in forge-primitives; re-exported here so existing
// `model::SessionId` imports keep resolving.
pub use forge_primitives::SessionId;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionModeId(String);

impl SessionModeId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}

impl From<String> for SessionModeId {
    fn from(value: String) -> Self {
        Self::new(value)
    }
}

impl From<&str> for SessionModeId {
    fn from(value: &str) -> Self {
        Self::new(value.to_owned())
    }
}

impl fmt::Display for SessionModeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextContent {
    pub text: String,
}

impl TextContent {
    pub fn new(text: impl Into<String>) -> Self {
        Self { text: text.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageContent {
    pub data: String,
    pub mime_type: String,
}

impl ImageContent {
    pub fn new(data: impl Into<String>, mime_type: impl Into<String>) -> Self {
        Self { data: data.into(), mime_type: mime_type.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentBlock {
    Text(TextContent),
    Image(ImageContent),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentChunk {
    pub content: ContentBlock,
}

impl ContentChunk {
    pub fn new(content: ContentBlock) -> Self {
        Self { content }
    }
}

/// Alias retained for the `ToolCallContent::Content(...)` variant
/// payload — same shape as `ContentChunk`.
pub type Content = ContentChunk;

pub use forge_primitives::{ToolCallLocation, ToolCallStatus, ToolKind};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalToolCallContent {
    pub terminal_id: String,
}

impl TerminalToolCallContent {
    pub fn new(terminal_id: impl Into<String>) -> Self {
        Self { terminal_id: terminal_id.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diff {
    pub path: PathBuf,
    pub old_text: Option<String>,
    pub new_text: String,
    pub repository: Option<String>,
}

impl Diff {
    pub fn new(path: impl Into<PathBuf>, new_text: impl Into<String>) -> Self {
        Self { path: path.into(), old_text: None, new_text: new_text.into(), repository: None }
    }

    pub fn old_text<T: Into<String>>(mut self, old_text: Option<T>) -> Self {
        self.old_text = old_text.map(Into::into);
        self
    }

    pub fn repository(mut self, repository: Option<String>) -> Self {
        self.repository = repository.filter(|repository| !repository.trim().is_empty());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpResource {
    pub uri: String,
    pub mime_type: Option<String>,
    pub text: Option<String>,
    pub blob_saved_to: Option<PathBuf>,
}

impl McpResource {
    pub fn new(uri: impl Into<String>) -> Self {
        Self { uri: uri.into(), mime_type: None, text: None, blob_saved_to: None }
    }

    pub fn mime_type(mut self, mime_type: Option<String>) -> Self {
        self.mime_type = mime_type.filter(|mime_type| !mime_type.trim().is_empty());
        self
    }

    pub fn text(mut self, text: Option<String>) -> Self {
        self.text = text.filter(|text| !text.trim().is_empty());
        self
    }

    pub fn blob_saved_to(mut self, blob_saved_to: Option<String>) -> Self {
        self.blob_saved_to =
            blob_saved_to.filter(|path| !path.trim().is_empty()).map(PathBuf::from);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolCallContent {
    Content(Content),
    Diff(Diff),
    McpResource(McpResource),
    Terminal(TerminalToolCallContent),
}

impl From<&str> for ToolCallContent {
    fn from(value: &str) -> Self {
        Self::Content(Content::new(ContentBlock::Text(TextContent::new(value))))
    }
}

impl From<String> for ToolCallContent {
    fn from(value: String) -> Self {
        Self::from(value.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    pub content: Vec<ToolCallContent>,
    pub raw_input: Option<serde_json::Value>,
    pub raw_output: Option<serde_json::Value>,
    pub output_metadata: Option<ToolOutputMetadata>,
    pub task_metadata: Option<TaskMetadata>,
    pub locations: Vec<ToolCallLocation>,
    pub meta: Option<serde_json::Value>,
}

impl ToolCall {
    pub fn new(tool_call_id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            tool_call_id: tool_call_id.into(),
            title: title.into(),
            kind: ToolKind::Think,
            status: ToolCallStatus::Pending,
            content: Vec::new(),
            raw_input: None,
            raw_output: None,
            output_metadata: None,
            task_metadata: None,
            locations: Vec::new(),
            meta: None,
        }
    }

    pub fn kind(mut self, kind: ToolKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn status(mut self, status: ToolCallStatus) -> Self {
        self.status = status;
        self
    }

    pub fn content(mut self, content: Vec<ToolCallContent>) -> Self {
        self.content = content;
        self
    }

    pub fn raw_input(mut self, raw_input: serde_json::Value) -> Self {
        self.raw_input = Some(raw_input);
        self
    }

    pub fn raw_output(mut self, raw_output: serde_json::Value) -> Self {
        self.raw_output = Some(raw_output);
        self
    }

    pub fn output_metadata(mut self, output_metadata: ToolOutputMetadata) -> Self {
        self.output_metadata = Some(output_metadata);
        self
    }

    pub fn task_metadata(mut self, task_metadata: TaskMetadata) -> Self {
        self.task_metadata = Some(task_metadata);
        self
    }

    pub fn locations(mut self, locations: Vec<ToolCallLocation>) -> Self {
        self.locations = locations;
        self
    }

    pub fn meta(mut self, meta: impl Into<serde_json::Value>) -> Self {
        self.meta = Some(meta.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct ToolCallUpdateFields {
    pub title: Option<String>,
    pub kind: Option<ToolKind>,
    pub status: Option<ToolCallStatus>,
    pub content: Option<Vec<ToolCallContent>>,
    pub raw_input: Option<serde_json::Value>,
    pub raw_output: Option<serde_json::Value>,
    pub output_metadata: Option<ToolOutputMetadata>,
    pub task_metadata: Option<TaskMetadata>,
    pub locations: Option<Vec<ToolCallLocation>>,
}

impl ToolCallUpdateFields {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    pub fn kind(mut self, kind: ToolKind) -> Self {
        self.kind = Some(kind);
        self
    }

    pub fn status(mut self, status: ToolCallStatus) -> Self {
        self.status = Some(status);
        self
    }

    pub fn content(mut self, content: Vec<ToolCallContent>) -> Self {
        self.content = Some(content);
        self
    }

    pub fn raw_input(mut self, raw_input: serde_json::Value) -> Self {
        self.raw_input = Some(raw_input);
        self
    }

    pub fn raw_output(mut self, raw_output: serde_json::Value) -> Self {
        self.raw_output = Some(raw_output);
        self
    }

    pub fn output_metadata(mut self, output_metadata: ToolOutputMetadata) -> Self {
        self.output_metadata = Some(output_metadata);
        self
    }

    pub fn task_metadata(mut self, task_metadata: TaskMetadata) -> Self {
        self.task_metadata = Some(task_metadata);
        self
    }

    pub fn locations(mut self, locations: Vec<ToolCallLocation>) -> Self {
        self.locations = Some(locations);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallUpdate {
    pub tool_call_id: String,
    pub fields: ToolCallUpdateFields,
    pub meta: Option<serde_json::Value>,
}

impl ToolCallUpdate {
    pub fn new(tool_call_id: impl Into<String>, fields: ToolCallUpdateFields) -> Self {
        Self { tool_call_id: tool_call_id.into(), fields, meta: None }
    }

    pub fn meta(mut self, meta: impl Into<serde_json::Value>) -> Self {
        self.meta = Some(meta.into());
        self
    }
}

pub use forge_primitives::session_update::{BashOutputMetadata, TaskMetadata, ToolOutputMetadata};

pub use forge_primitives::runtime::AvailableCommand;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableCommandsUpdate {
    pub available_commands: Vec<AvailableCommand>,
}

impl AvailableCommandsUpdate {
    pub fn new(available_commands: Vec<AvailableCommand>) -> Self {
        Self { available_commands }
    }
}

pub use forge_primitives::runtime::AvailableAgent;

// EffortLevel + its UI helpers (label/description/as_stored/from_stored)
// live in forge-primitives::runtime now.
pub use forge_primitives::runtime::EffortLevel;

pub use forge_primitives::runtime::{AvailableModel, CurrentModel};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableAgentsUpdate {
    pub available_agents: Vec<AvailableAgent>,
}

impl AvailableAgentsUpdate {
    pub fn new(available_agents: Vec<AvailableAgent>) -> Self {
        Self { available_agents }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentModeUpdate {
    pub current_mode_id: SessionModeId,
}

impl CurrentModeUpdate {
    pub fn new(current_mode_id: impl Into<SessionModeId>) -> Self {
        Self { current_mode_id: current_mode_id.into() }
    }
}

pub use forge_primitives::runtime::FastModeState;

pub use forge_primitives::RateLimitStatus;

pub use forge_primitives::runtime::ApiRetryError;

pub use forge_primitives::runtime::RuntimeSessionState;

pub use forge_primitives::runtime::RateLimitUpdate;

pub use forge_primitives::runtime::{CompactionTrigger, SessionStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionBoundary {
    pub trigger: CompactionTrigger,
    pub pre_tokens: u64,
}

pub use forge_primitives::permission_ui::{
    PermissionAction, PermissionDisplay, PermissionOption, PermissionOptionKind, PermissionOutcome,
    PermissionRequest,
};
pub use forge_primitives::question::{
    QuestionAnnotation, QuestionOption, QuestionOutcome, QuestionPrompt, QuestionRequest,
};
