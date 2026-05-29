//! UI-side `AskUserQuestion` request/response shapes — what the
//! agent surfaces to the UI when a tool wants structured user input,
//! and what the user picks.

use serde::{Deserialize, Serialize};

use crate::session_update::ToolCall;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionOption {
    pub option_id: String,
    pub label: String,
    pub description: Option<String>,
    pub preview: Option<String>,
    /// #273: Mirrors the CLI 2.1.156 `(Recommended)` suffix on
    /// option labels. Detected and stripped by
    /// `parse_ask_user_question_prompts`. Renderer surfaces the
    /// flag as a bold option label and the prompt-state builder
    /// pre-selects the first recommended option in the list.
    /// `#[serde(default)]` keeps replay of older captures
    /// (without this field) decode-clean.
    #[serde(default)]
    pub recommended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionPrompt {
    pub question: String,
    pub header: String,
    pub multi_select: bool,
    pub options: Vec<QuestionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub tool_call: ToolCall,
    pub prompt: QuestionPrompt,
    pub question_index: u64,
    pub total_questions: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionAnnotation {
    pub preview: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum QuestionOutcome {
    Answered { selected_option_ids: Vec<String>, annotation: Option<QuestionAnnotation> },
    Cancelled,
}
