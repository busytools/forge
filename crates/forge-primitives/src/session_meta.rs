//! Session-level metadata - the prompt-chunk envelope.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptChunk {
    pub kind: String,
    pub value: serde_json::Value,
}
