//! `Event` — the agent → UI channel envelope.
//!
//! Mirrors `forge_agent::client::AgentEvent` during the restructure;
//! phase 5 has the Agent translator emit `Event` directly so the
//! AgentEvent intermediate goes away. Variants kept near 1:1 with
//! AgentEvent for now — phase 6 (userdata/cloud/env) is when we
//! refine field types away from forge-sdk passthroughs.

use serde::{Deserialize, Serialize};

use crate::SessionListEntry;
use crate::ids::SessionId;

/// agent → UI events.
///
/// Variants currently use `serde_json::Value` for some payload fields
/// where the natural Rust type lives in forge-sdk (forge-primitives
/// can't reach into forge-sdk without a circular dep). Phase 6
/// relocates those types into forge-primitives or forge-agent's
/// userdata/cloud modules and tightens the variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Event {
    // --- Lifecycle ---
    Connected {
        session_id: SessionId,
        cwd: String,
        current_model: serde_json::Value,
        available_models: Vec<serde_json::Value>,
        mode: Option<serde_json::Value>,
        history_updates: Option<serde_json::Value>,
    },
    AuthRequired {
        method_name: String,
        method_description: String,
    },
    ConnectionFailed {
        message: String,
    },
    SessionReplaced {
        session_id: SessionId,
        cwd: String,
        current_model: serde_json::Value,
        available_models: Vec<serde_json::Value>,
        mode: Option<serde_json::Value>,
        history_updates: Option<serde_json::Value>,
    },

    // --- Sessions catalog ---
    SessionsListed {
        sessions: Vec<SessionListEntry>,
    },

    // --- SDK message stream (raw forge_sdk::Message JSON for now) ---
    SdkMessage {
        session_id: SessionId,
        msg: serde_json::Value,
    },

    // --- Callback requests ---
    PermissionRequest {
        session_id: SessionId,
        request: serde_json::Value,
    },
    QuestionRequest {
        session_id: SessionId,
        request: serde_json::Value,
    },
    ElicitationRequest {
        session_id: SessionId,
        request: serde_json::Value,
    },
    ElicitationComplete {
        session_id: SessionId,
        elicitation_id: String,
        server_name: Option<String>,
    },

    // --- MCP ---
    McpAuthRedirect {
        session_id: SessionId,
        redirect: serde_json::Value,
    },
    McpOperationError {
        session_id: SessionId,
        error: serde_json::Value,
    },
    McpSnapshot {
        session_id: SessionId,
        servers: serde_json::Value,
        error: Option<String>,
    },

    // --- Slash / runtime ---
    SlashError {
        session_id: SessionId,
        message: String,
    },
    RuntimeReloadCompleted {
        session_id: SessionId,
    },
    RuntimeReloadFailed {
        session_id: SessionId,
        message: String,
    },

    // --- Snapshots ---
    StatusSnapshot {
        session_id: SessionId,
        account: serde_json::Value,
    },
    OauthCredentialsSnapshot {
        session_id: SessionId,
        credentials: serde_json::Value,
    },
    GitContextSnapshot {
        session_id: SessionId,
        context: serde_json::Value,
    },
    ContextUsage {
        session_id: SessionId,
        percentage: Option<u8>,
    },
}
