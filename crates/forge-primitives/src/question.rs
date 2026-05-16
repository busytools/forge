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

impl QuestionAnnotation {
    pub fn new() -> Self {
        Self::default()
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum QuestionOutcome {
    Answered { selected_option_ids: Vec<String>, annotation: Option<QuestionAnnotation> },
    Cancelled,
}
