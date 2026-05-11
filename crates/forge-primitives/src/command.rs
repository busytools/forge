//! `Command` — the UI → agent channel envelope.
//!
//! One variant per fire-and-forget action the UI can ask the agent
//! to perform. Direct-return accessors (config_dir, settings_documents,
//! etc.) live on a separate sync surface, not in `Command`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ids::{MessageId, SessionId, ToolUseId};
use crate::image::ImageAttachment;
use crate::{ElicitationAction, McpServerConfig, PermissionOutcome, QuestionOutcome};

/// UI → agent channel envelope. Each variant maps to one inherent
/// method on `forge_agent::ForgeSdkBridge`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Command {
    // --- Session lifecycle ---
    NewSession {
        cwd: String,
        launch_settings: serde_json::Value,
    },
    ResumeSession {
        session_id: SessionId,
        /// Working directory for the spawned `claude` subprocess.
        /// `claude --resume <id>` indexes sessions by the project key
        /// derived from this directory; an empty value makes the
        /// subprocess inherit forge's `$PWD` which usually does not
        /// match the resumed session's project, causing "No
        /// conversation found with session ID …" even when the
        /// `.jsonl` exists. Source from the catalog when picking a
        /// session by id.
        cwd: String,
        launch_settings: serde_json::Value,
    },
    /// Resume `session_id`; if the resume fails at the transport level
    /// (e.g. the underlying `.jsonl` is missing, locked, or otherwise
    /// unreadable by the `claude` subprocess), automatically retry as
    /// `NewSession { cwd, … }`. Used by every project-rooted entry
    /// path (`forge`, `forge <project>`, Projects-pane project-header
    /// click) so a stale or cross-account catalog entry can't strand
    /// the user with no recoverable session.
    ResumeOrNewSession {
        session_id: SessionId,
        cwd: String,
        launch_settings: serde_json::Value,
    },

    // --- Conversation ---
    Prompt {
        session_id: SessionId,
        text: String,
    },
    PromptWithImages {
        session_id: SessionId,
        text: String,
        images: Vec<ImageAttachment>,
    },
    Cancel {
        session_id: SessionId,
    },

    // --- Controls ---
    SetMode {
        session_id: SessionId,
        mode: String,
    },
    SetModel {
        session_id: SessionId,
        model: String,
    },
    GenerateSessionTitle {
        session_id: SessionId,
        description: String,
    },
    RenameSession {
        session_id: SessionId,
        title: String,
    },
    RewindFiles {
        session_id: SessionId,
        user_message_id: MessageId,
    },

    // --- Snapshots requested by UI ---
    GetStatusSnapshot {
        session_id: SessionId,
    },
    GetOauthCredentialsSnapshot {
        session_id: SessionId,
    },
    GetContextUsage {
        session_id: SessionId,
    },
    GetMcpSnapshot {
        session_id: SessionId,
    },

    // --- Callback responses ---
    PermissionResponse {
        session_id: SessionId,
        tool_call_id: ToolUseId,
        outcome: PermissionOutcome,
    },
    QuestionResponse {
        session_id: SessionId,
        tool_call_id: ToolUseId,
        outcome: QuestionOutcome,
    },
    RespondToElicitation {
        session_id: SessionId,
        elicitation_request_id: String,
        action: ElicitationAction,
        content: Option<serde_json::Value>,
    },

    // --- MCP management ---
    ReconnectMcpServer {
        session_id: SessionId,
        server_name: String,
    },
    ToggleMcpServer {
        session_id: SessionId,
        server_name: String,
        enabled: bool,
    },
    SetMcpServers {
        session_id: SessionId,
        servers: BTreeMap<String, McpServerConfig>,
    },
    AuthenticateMcpServer {
        session_id: SessionId,
        server_name: String,
    },
    ClearMcpAuth {
        session_id: SessionId,
        server_name: String,
    },
    SubmitMcpOauthCallbackUrl {
        session_id: SessionId,
        server_name: String,
        callback_url: String,
    },

    // --- Plugins / runtime ---
    ReloadPlugins {
        session_id: SessionId,
    },

    // --- Git context ---
    StartGitContextWatch {
        session_id: SessionId,
        cwd: PathBuf,
    },
    StopGitContextWatch {
        session_id: SessionId,
    },
}
