//! MCP-side UI events — auth-redirect URLs the user has to visit and
//! operation-error envelopes that surface failures from the MCP
//! orchestrator.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpAuthRedirect {
    pub server_name: String,
    pub auth_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpOperationError {
    pub server_name: Option<String>,
    pub operation: String,
    pub message: String,
}
