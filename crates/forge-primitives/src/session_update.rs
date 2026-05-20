//! Wire-side support types for streaming session events: content
//! chunks, tool-call envelopes, tool-output metadata, plan entries.

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// Typed tool-call category. Unknown wire values fall through to
/// `Other` so a new CLI variant doesn't break decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
    #[serde(other)]
    Other,
}

/// Lifecycle state of a tool call. Unknown wire values fall through
/// to `Pending` so a new CLI variant doesn't break decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    InProgress,
    Completed,
    Failed,
    Killed,
    #[serde(other)]
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub tool_call_id: String,
    pub title: String,
    pub kind: ToolKind,
    pub status: ToolCallStatus,
    pub content: Vec<ToolCallContent>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<String>,
    pub output_metadata: Option<ToolOutputMetadata>,
    pub task_metadata: Option<TaskMetadata>,
    pub locations: Vec<ToolCallLocation>,
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
    pub kind: Option<ToolKind>,
    pub status: Option<ToolCallStatus>,
    pub content: Option<Vec<ToolCallContent>>,
    pub raw_input: Option<Value>,
    pub raw_output: Option<String>,
    pub output_metadata: Option<ToolOutputMetadata>,
    pub task_metadata: Option<TaskMetadata>,
    pub locations: Option<Vec<ToolCallLocation>>,
    pub meta: Option<Value>,
}

impl ToolCall {
    /// Apply a `ToolCallUpdateFields` patch in-place: any `Some` field
    /// overrides the corresponding field on `self`; `None` leaves it
    /// alone. The destructure forces a compile error when a new field
    /// is added to `ToolCallUpdateFields` without a corresponding
    /// merge arm here.
    pub fn merge(&mut self, fields: ToolCallUpdateFields) {
        let ToolCallUpdateFields {
            title,
            kind,
            status,
            content,
            raw_input,
            raw_output,
            output_metadata,
            task_metadata,
            locations,
            meta,
        } = fields;

        if let Some(title) = title {
            self.title = title;
        }
        if let Some(kind) = kind {
            self.kind = kind;
        }
        if let Some(status) = status {
            self.status = status;
        }
        if let Some(content) = content {
            self.content = content;
        }
        if raw_input.is_some() {
            self.raw_input = raw_input;
        }
        if raw_output.is_some() {
            self.raw_output = raw_output;
        }
        if output_metadata.is_some() {
            self.output_metadata = output_metadata;
        }
        if let Some(update) = task_metadata {
            match self.task_metadata.as_mut() {
                Some(existing) => existing.merge(update),
                None => self.task_metadata = Some(update),
            }
        }
        if let Some(locations) = locations {
            self.locations = locations;
        }
        if meta.is_some() {
            self.meta = meta;
        }
    }
}

/// File path + optional 1-based line number a tool call inspected
/// or modified. Used by the chat renderer's clickable file glyphs
/// and by `/diff` to open the right file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallLocation {
    pub path: std::path::PathBuf,
    pub line: Option<u32>,
}

impl ToolCallLocation {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into(), line: None }
    }

    pub fn line(mut self, line: u32) -> Self {
        self.line = Some(line);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TodoWriteOutputMetadata {
    pub verification_nudge_needed: Option<bool>,
}

impl TodoWriteOutputMetadata {
    pub fn new() -> Self {
        Self::default()
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
        Self::default()
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

    /// Field-wise merge: any `Some` field on `update` overrides `self`;
    /// `None` leaves `self` alone. The destructure forces a compile
    /// error if a new field is added without a corresponding merge
    /// arm.
    pub fn merge(&mut self, update: TaskMetadata) {
        let TaskMetadata { end_time, total_paused_ms, error, is_backgrounded } = update;
        if end_time.is_some() {
            self.end_time = end_time;
        }
        if total_paused_ms.is_some() {
            self.total_paused_ms = total_paused_ms;
        }
        if error.is_some() {
            self.error = error;
        }
        if is_backgrounded.is_some() {
            self.is_backgrounded = is_backgrounded;
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolCallContent {
    Content {
        content: ChunkContent,
    },
    Diff {
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

#[cfg(test)]
mod tests {
    use super::{
        ChunkContent, TaskMetadata, ToolCall, ToolCallContent, ToolCallLocation, ToolCallStatus,
        ToolCallUpdateFields, ToolKind,
    };
    use serde_json::json;

    fn sample_tool_call() -> ToolCall {
        ToolCall {
            tool_call_id: "tc_1".into(),
            title: "old title".into(),
            kind: ToolKind::Execute,
            status: ToolCallStatus::InProgress,
            content: vec![ToolCallContent::Content {
                content: ChunkContent::Text { text: "before".into() },
            }],
            raw_input: Some(json!({"cmd": "ls"})),
            raw_output: Some("old stdout".into()),
            output_metadata: None,
            task_metadata: None,
            locations: vec![ToolCallLocation::new("/old").line(1)],
            meta: Some(json!({"old": true})),
        }
    }

    #[test]
    fn merge_with_empty_fields_leaves_tool_call_unchanged() {
        let mut tc = sample_tool_call();
        let before = tc.clone();
        tc.merge(ToolCallUpdateFields::default());
        assert_eq!(tc, before);
    }

    #[test]
    fn merge_overrides_each_set_field() {
        let mut tc = sample_tool_call();
        tc.merge(ToolCallUpdateFields {
            title: Some("new title".into()),
            kind: Some(ToolKind::Read),
            status: Some(ToolCallStatus::Completed),
            content: Some(vec![ToolCallContent::Content {
                content: ChunkContent::Text { text: "after".into() },
            }]),
            raw_input: Some(json!({"cmd": "pwd"})),
            raw_output: Some("new stdout".into()),
            output_metadata: None,
            task_metadata: None,
            locations: Some(vec![ToolCallLocation::new("/new").line(7)]),
            meta: Some(json!({"new": true})),
        });
        assert_eq!(tc.title, "new title");
        assert_eq!(tc.kind, ToolKind::Read);
        assert_eq!(tc.status, ToolCallStatus::Completed);
        assert_eq!(tc.content.len(), 1);
        assert_eq!(tc.raw_input, Some(json!({"cmd": "pwd"})));
        assert_eq!(tc.raw_output.as_deref(), Some("new stdout"));
        assert_eq!(tc.locations.len(), 1);
        assert_eq!(tc.locations[0].path, std::path::PathBuf::from("/new"));
        assert_eq!(tc.meta, Some(json!({"new": true})));
    }

    #[test]
    fn merge_none_fields_preserve_existing_some_on_tool_call() {
        // raw_input / raw_output / meta are Option<T> on both sides.
        // None on the update side must not clobber an existing Some.
        let mut tc = sample_tool_call();
        tc.merge(ToolCallUpdateFields::default());
        assert_eq!(tc.raw_input, Some(json!({"cmd": "ls"})));
        assert_eq!(tc.raw_output.as_deref(), Some("old stdout"));
        assert_eq!(tc.meta, Some(json!({"old": true})));
    }

    #[test]
    fn merge_task_metadata_sets_when_base_is_none() {
        let mut tc = sample_tool_call();
        assert!(tc.task_metadata.is_none());
        tc.merge(ToolCallUpdateFields {
            task_metadata: Some(TaskMetadata {
                end_time: Some(1234),
                total_paused_ms: None,
                error: None,
                is_backgrounded: None,
            }),
            ..Default::default()
        });
        assert_eq!(tc.task_metadata.as_ref().and_then(|m| m.end_time), Some(1234));
    }

    #[test]
    fn merge_task_metadata_field_wise_when_base_is_some() {
        let mut tc = sample_tool_call();
        tc.task_metadata = Some(TaskMetadata {
            end_time: Some(1000),
            total_paused_ms: Some(50),
            error: None,
            is_backgrounded: Some(false),
        });
        // Update only end_time + error; total_paused_ms / is_backgrounded
        // must survive.
        tc.merge(ToolCallUpdateFields {
            task_metadata: Some(TaskMetadata {
                end_time: Some(2000),
                total_paused_ms: None,
                error: Some("boom".into()),
                is_backgrounded: None,
            }),
            ..Default::default()
        });
        let tm = tc.task_metadata.expect("task_metadata present");
        assert_eq!(tm.end_time, Some(2000));
        assert_eq!(tm.total_paused_ms, Some(50));
        assert_eq!(tm.error.as_deref(), Some("boom"));
        assert_eq!(tm.is_backgrounded, Some(false));
    }

    #[test]
    fn task_metadata_merge_destructure_overrides_set_fields() {
        let mut tm = TaskMetadata {
            end_time: Some(1),
            total_paused_ms: Some(2),
            error: None,
            is_backgrounded: Some(false),
        };
        tm.merge(TaskMetadata {
            end_time: None,
            total_paused_ms: Some(99),
            error: Some("oops".into()),
            is_backgrounded: None,
        });
        assert_eq!(tm.end_time, Some(1));
        assert_eq!(tm.total_paused_ms, Some(99));
        assert_eq!(tm.error.as_deref(), Some("oops"));
        assert_eq!(tm.is_backgrounded, Some(false));
    }
}
