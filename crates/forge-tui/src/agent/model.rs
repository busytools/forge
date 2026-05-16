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
pub struct Content {
    pub content: ContentBlock,
}

impl Content {
    pub fn new(content: ContentBlock) -> Self {
        Self { content }
    }
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Execute,
    Search,
    Fetch,
    Think,
    SwitchMode,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
    Killed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallLocation {
    pub path: PathBuf,
    pub line: Option<u32>,
}

impl ToolCallLocation {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into(), line: None }
    }

    pub fn line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TodoWriteOutputMetadata {
    pub verification_nudge_needed: Option<bool>,
}

impl TodoWriteOutputMetadata {
    pub fn new() -> Self {
        Self { verification_nudge_needed: None }
    }

    pub fn verification_nudge_needed(mut self, verification_nudge_needed: Option<bool>) -> Self {
        self.verification_nudge_needed = verification_nudge_needed;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BashOutputMetadata {
    pub assistant_auto_backgrounded: Option<bool>,
}

impl BashOutputMetadata {
    pub fn new() -> Self {
        Self { assistant_auto_backgrounded: None }
    }

    pub fn assistant_auto_backgrounded(
        mut self,
        assistant_auto_backgrounded: Option<bool>,
    ) -> Self {
        self.assistant_auto_backgrounded = assistant_auto_backgrounded;
        self
    }
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

impl TaskMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn end_time(mut self, end_time: Option<u64>) -> Self {
        self.end_time = end_time;
        self
    }

    pub fn total_paused_ms(mut self, total_paused_ms: Option<u64>) -> Self {
        self.total_paused_ms = total_paused_ms;
        self
    }

    pub fn error(mut self, error: Option<String>) -> Self {
        self.error = error;
        self
    }

    pub fn backgrounded(mut self, is_backgrounded: Option<bool>) -> Self {
        self.is_backgrounded = is_backgrounded;
        self
    }
}

impl ToolOutputMetadata {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bash(mut self, bash: Option<BashOutputMetadata>) -> Self {
        self.bash = bash;
        self
    }

    pub fn todo_write(mut self, todo_write: Option<TodoWriteOutputMetadata>) -> Self {
        self.todo_write = todo_write;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryPriority {
    High,
    Medium,
    Low,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanEntryStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanEntryPriority,
    pub status: PlanEntryStatus,
}

impl PlanEntry {
    pub fn new(
        content: impl Into<String>,
        priority: PlanEntryPriority,
        status: PlanEntryStatus,
    ) -> Self {
        Self { content: content.into(), priority, status }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub entries: Vec<PlanEntry>,
}

impl Plan {
    pub fn new(entries: Vec<PlanEntry>) -> Self {
        Self { entries }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
    pub input_hint: Option<String>,
}

impl AvailableCommand {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self { name: name.into(), description: description.into(), input_hint: None }
    }

    pub fn input_hint(mut self, input_hint: impl Into<String>) -> Self {
        self.input_hint = Some(input_hint.into());
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableCommandsUpdate {
    pub available_commands: Vec<AvailableCommand>,
}

impl AvailableCommandsUpdate {
    pub fn new(available_commands: Vec<AvailableCommand>) -> Self {
        Self { available_commands }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableAgent {
    pub name: String,
    pub description: String,
    pub model: Option<String>,
}

impl AvailableAgent {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self { name: name.into(), description: description.into(), model: None }
    }

    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }
}

// EffortLevel + its UI helpers (label/description/as_stored/from_stored)
// live in forge-primitives::runtime now.
pub use forge_primitives::runtime::EffortLevel;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AvailableModel {
    pub id: String,
    pub display_name: String,
    pub description: Option<String>,
    pub supports_effort: bool,
    pub supported_effort_levels: Vec<EffortLevel>,
    pub supports_adaptive_thinking: Option<bool>,
    pub supports_fast_mode: Option<bool>,
    pub supports_auto_mode: Option<bool>,
}

impl AvailableModel {
    pub fn new(id: impl Into<String>, display_name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: display_name.into(),
            description: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_adaptive_thinking: None,
            supports_fast_mode: None,
            supports_auto_mode: None,
        }
    }

    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    pub fn supports_effort(mut self, supports_effort: bool) -> Self {
        self.supports_effort = supports_effort;
        self
    }

    pub fn supported_effort_levels(mut self, supported_effort_levels: Vec<EffortLevel>) -> Self {
        self.supported_effort_levels = supported_effort_levels;
        self
    }

    pub fn supports_adaptive_thinking(mut self, supports_adaptive_thinking: Option<bool>) -> Self {
        self.supports_adaptive_thinking = supports_adaptive_thinking;
        self
    }

    pub fn supports_fast_mode(mut self, supports_fast_mode: Option<bool>) -> Self {
        self.supports_fast_mode = supports_fast_mode;
        self
    }

    pub fn supports_auto_mode(mut self, supports_auto_mode: Option<bool>) -> Self {
        self.supports_auto_mode = supports_auto_mode;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentModel {
    pub requested_id: Option<String>,
    pub resolved_id: String,
    pub display_name_short: String,
    pub display_name_long: String,
    pub catalog_id: Option<String>,
    pub supports_effort: bool,
    pub supported_effort_levels: Vec<EffortLevel>,
    pub supports_fast_mode: Option<bool>,
    pub supports_auto_mode: Option<bool>,
    pub supports_adaptive_thinking: Option<bool>,
    pub is_authoritative: bool,
}

impl CurrentModel {
    pub fn new(
        resolved_id: impl Into<String>,
        display_name_short: impl Into<String>,
        display_name_long: impl Into<String>,
    ) -> Self {
        Self {
            requested_id: None,
            resolved_id: resolved_id.into(),
            display_name_short: display_name_short.into(),
            display_name_long: display_name_long.into(),
            catalog_id: None,
            supports_effort: false,
            supported_effort_levels: Vec::new(),
            supports_fast_mode: None,
            supports_auto_mode: None,
            supports_adaptive_thinking: None,
            is_authoritative: false,
        }
    }

    pub fn requested_id(mut self, requested_id: impl Into<String>) -> Self {
        self.requested_id = Some(requested_id.into());
        self
    }

    pub fn catalog_id(mut self, catalog_id: impl Into<String>) -> Self {
        self.catalog_id = Some(catalog_id.into());
        self
    }

    pub fn supports_effort(mut self, supports_effort: bool) -> Self {
        self.supports_effort = supports_effort;
        self
    }

    pub fn supported_effort_levels(mut self, supported_effort_levels: Vec<EffortLevel>) -> Self {
        self.supported_effort_levels = supported_effort_levels;
        self
    }

    pub fn supports_adaptive_thinking(mut self, supports_adaptive_thinking: Option<bool>) -> Self {
        self.supports_adaptive_thinking = supports_adaptive_thinking;
        self
    }

    pub fn supports_fast_mode(mut self, supports_fast_mode: Option<bool>) -> Self {
        self.supports_fast_mode = supports_fast_mode;
        self
    }

    pub fn supports_auto_mode(mut self, supports_auto_mode: Option<bool>) -> Self {
        self.supports_auto_mode = supports_auto_mode;
        self
    }

    pub fn authoritative(mut self, is_authoritative: bool) -> Self {
        self.is_authoritative = is_authoritative;
        self
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CurrentModelUpdate {
    pub current_model: CurrentModel,
}

impl CurrentModelUpdate {
    pub fn new(current_model: CurrentModel) -> Self {
        Self { current_model }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigOptionUpdate {
    pub option_id: String,
    pub value: serde_json::Value,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FastModeState {
    #[default]
    Off,
    Cooldown,
    On,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitStatus {
    Allowed,
    AllowedWarning,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiRetryError {
    AuthenticationFailed,
    BillingError,
    RateLimit,
    InvalidRequest,
    ServerError,
    MaxOutputTokens,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RuntimeSessionState {
    Idle,
    Running,
    RequiresAction,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RateLimitUpdate {
    pub status: RateLimitStatus,
    pub resets_at: Option<f64>,
    pub utilization: Option<f64>,
    pub rate_limit_type: Option<String>,
    pub overage_status: Option<RateLimitStatus>,
    pub overage_resets_at: Option<f64>,
    pub overage_disabled_reason: Option<String>,
    pub is_using_overage: Option<bool>,
    pub surpassed_threshold: Option<f64>,
}

pub use forge_primitives::runtime::{CompactionTrigger, SessionStatus};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompactionBoundary {
    pub trigger: CompactionTrigger,
    pub pre_tokens: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowSession,
    AllowAlways,
    RejectOnce,
    RejectAlways,
    QuestionChoice,
    PlanApprove,
    PlanReject,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub option_id: String,
    pub name: String,
    pub description: Option<String>,
    pub kind: PermissionOptionKind,
}

impl PermissionOption {
    pub fn new(
        option_id: impl Into<String>,
        name: impl Into<String>,
        kind: PermissionOptionKind,
    ) -> Self {
        Self { option_id: option_id.into(), name: name.into(), description: None, kind }
    }

    pub fn description(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub option_id: String,
    pub label: String,
    pub description: Option<String>,
    pub preview: Option<String>,
}

impl QuestionOption {
    pub fn new(option_id: impl Into<String>, label: impl Into<String>) -> Self {
        Self { option_id: option_id.into(), label: label.into(), description: None, preview: None }
    }

    pub fn description(mut self, description: Option<String>) -> Self {
        self.description = description;
        self
    }

    pub fn preview(mut self, preview: Option<String>) -> Self {
        self.preview = preview;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionPrompt {
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

impl QuestionPrompt {
    pub fn new(
        question: impl Into<String>,
        header: impl Into<String>,
        multi_select: bool,
        options: Vec<QuestionOption>,
    ) -> Self {
        Self { question: question.into(), header: header.into(), multi_select, options }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnnotation {
    pub preview: Option<String>,
    pub notes: Option<String>,
}

impl QuestionAnnotation {
    pub fn new() -> Self {
        Self { preview: None, notes: None }
    }

    pub fn preview(mut self, preview: Option<String>) -> Self {
        self.preview = preview;
        self
    }

    pub fn notes(mut self, notes: Option<String>) -> Self {
        self.notes = notes;
        self
    }
}

impl Default for QuestionAnnotation {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectedPermissionOutcome {
    pub option_id: String,
}

impl SelectedPermissionOutcome {
    pub fn new(option_id: impl Into<String>) -> Self {
        Self { option_id: option_id.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestPermissionOutcome {
    Selected(SelectedPermissionOutcome),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnsweredQuestionOutcome {
    pub selected_option_ids: Vec<String>,
    pub annotation: Option<QuestionAnnotation>,
}

impl AnsweredQuestionOutcome {
    pub fn new(selected_option_ids: Vec<String>) -> Self {
        Self { selected_option_ids, annotation: None }
    }

    pub fn annotation(mut self, annotation: Option<QuestionAnnotation>) -> Self {
        self.annotation = annotation;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RequestQuestionOutcome {
    Answered(AnsweredQuestionOutcome),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPermissionResponse {
    pub outcome: RequestPermissionOutcome,
}

impl RequestPermissionResponse {
    pub fn new(outcome: RequestPermissionOutcome) -> Self {
        Self { outcome }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestQuestionResponse {
    pub outcome: RequestQuestionOutcome,
}

impl RequestQuestionResponse {
    pub fn new(outcome: RequestQuestionOutcome) -> Self {
        Self { outcome }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestPermissionRequest {
    pub session_id: SessionId,
    pub tool_call: ToolCallUpdate,
    pub options: Vec<PermissionOption>,
    pub display: Option<PermissionDisplay>,
}

impl RequestPermissionRequest {
    pub fn new(
        session_id: impl Into<SessionId>,
        tool_call: ToolCallUpdate,
        options: Vec<PermissionOption>,
        display: Option<PermissionDisplay>,
    ) -> Self {
        Self { session_id: session_id.into(), tool_call, options, display }
    }
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

    pub fn is_empty(&self) -> bool {
        self.title.as_ref().is_none_or(|value| value.trim().is_empty())
            && self.display_name.as_ref().is_none_or(|value| value.trim().is_empty())
            && self.description.as_ref().is_none_or(|value| value.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RequestQuestionRequest {
    pub session_id: SessionId,
    pub tool_call: ToolCallUpdate,
    pub prompt: QuestionPrompt,
    pub question_index: usize,
    pub total_questions: usize,
}

impl RequestQuestionRequest {
    pub fn new(
        session_id: impl Into<SessionId>,
        tool_call: ToolCallUpdate,
        prompt: QuestionPrompt,
        question_index: usize,
        total_questions: usize,
    ) -> Self {
        Self { session_id: session_id.into(), tool_call, prompt, question_index, total_questions }
    }
}
