//! Session-level metadata — session-list entries + prompt-chunk
//! envelope. (Earlier ACP-protocol-parity types — `AuthMethod`,
//! `AgentCapabilities`, `InitializeResult`, `SessionInit`,
//! `McpSetServersResult` — were removed in 2026-05-05; the
//! restructure dropped the parity contract and they had no callers.)

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionListEntry {
    pub session_id: String,
    pub summary: String,
    pub last_modified_ms: u64,
    pub file_size_bytes: u64,
    pub cwd: Option<String>,
    pub git_branch: Option<String>,
    pub custom_title: Option<String>,
    pub first_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptChunk {
    pub kind: String,
    pub value: serde_json::Value,
}
