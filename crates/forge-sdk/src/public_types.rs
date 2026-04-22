//! Pure-Rust mirrors of the Python SDK's public types that don't yet
//! participate in any wire-path in forge-sdk but are needed for surface
//! parity. Users construct / inspect these; the serde impls match the
//! Python wire shape verbatim.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which settings scope to load. Mirrors Python's
/// `Literal["user", "project", "local"]` (`types.py:32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingSource {
    /// User-scope settings (`~/.claude/settings.json`).
    User,
    /// Project-scope settings (`<repo>/.claude/settings.json`).
    Project,
    /// Project-local settings (`<repo>/.claude/settings.local.json`).
    Local,
}

impl SettingSource {
    /// String form for `--setting-sources=<csv>`.
    #[must_use]
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

/// Experimental beta flag names accepted by `--betas`. Mirrors Python's
/// `SdkBeta` literal (`types.py:29`). Forge-sdk keeps this as a string
/// alias rather than a closed enum so new beta tokens don't require a
/// breaking change.
pub type SdkBeta = String;

/// Streaming partial-message event surfaced when
/// `Options.include_partial_messages` is set. Mirrors Python's
/// `StreamEvent` (`types.py:1043-1050`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamEvent {
    /// Unique identifier for this stream event.
    pub uuid: String,
    /// Session the event belongs to.
    pub session_id: String,
    /// Raw Anthropic API stream event (delta / `message_start` / etc.).
    pub event: Value,
    /// Parent `tool_use` id when the emitting assistant turn is a sub-agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
}

/// One session-listing row from `list_sessions()`. Mirrors Python's
/// `SDKSessionInfo` (`types.py:1265-1298`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SDKSessionInfo {
    /// Session UUID.
    pub session_id: String,
    /// Display title.
    pub summary: String,
    /// Last-modified time (ms since Unix epoch).
    pub last_modified: u64,
    /// File size in bytes. Local JSONL only; `None` for remote backends.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_size: Option<u64>,
    /// User-set or AI-generated custom title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_title: Option<String>,
    /// First meaningful user prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first_prompt: Option<String>,
    /// Git branch at session end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Working directory for the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// User-set tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Creation time (ms since Unix epoch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<u64>,
}

/// One user / assistant message from a session transcript, as returned
/// by `get_session_messages()`. Mirrors Python's `SessionMessage`
/// (`types.py:1301-1322`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMessage {
    /// Message kind.
    #[serde(rename = "type")]
    pub kind: SessionMessageKind,
    /// Unique message identifier.
    pub uuid: String,
    /// Session this message belongs to.
    pub session_id: String,
    /// Raw Anthropic API message (role, content, usage, …).
    pub message: Value,
    /// Always `None` for top-level messages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_use_id: Option<String>,
}

/// User / assistant discriminator. Mirrors Python's
/// `Literal["user", "assistant"]` on `SessionMessage.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMessageKind {
    /// User turn.
    User,
    /// Assistant turn.
    Assistant,
}

/// Possible connection statuses for an MCP server. Mirrors Python's
/// `McpServerConnectionStatus` (`types.py:654-656`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum McpServerConnectionStatus {
    /// Server is up and healthy.
    Connected,
    /// Last connect attempt failed.
    Failed,
    /// Server requires authentication before it can be used.
    NeedsAuth,
    /// Connect attempt hasn't finished yet.
    Pending,
    /// Server is registered but explicitly disabled.
    Disabled,
}

/// MCP tool annotations. Mirrors Python's `McpToolAnnotations`
/// (`types.py:627-635`). Wire keys are camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolAnnotations {
    /// Tool doesn't mutate state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    /// Tool may cause irreversible harm.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub destructive: Option<bool>,
    /// Tool reaches the open Internet.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_world: Option<bool>,
}

/// Info about a tool exposed by an MCP server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpToolInfo {
    /// Tool name (e.g. `"search-web"`).
    pub name: String,
    /// Short description shown to the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Optional annotations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub annotations: Option<McpToolAnnotations>,
}

/// Server info from the MCP `initialize` handshake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct McpServerInfo {
    /// Server product name.
    pub name: String,
    /// Server product version.
    pub version: String,
}

/// Per-server status entry inside [`McpStatusResponse`]. Mirrors Python's
/// `McpServerStatus` (`types.py:659-684`). Wire shape matches the CLI's
/// JSON response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    /// Server name as configured.
    pub name: String,
    /// Current connection status.
    pub status: McpServerConnectionStatus,
    /// Handshake info — present when `status == Connected`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_info: Option<McpServerInfo>,
    /// Error message when `status == Failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Opaque config blob — one of Python's `McpServerStatusConfig`
    /// variants. Typed as Value here since the CLI accepts claudeai-proxy
    /// which isn't a first-class forge-sdk input type.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<serde_json::Value>,
    /// Configuration scope (`"project" | "user" | "local" | "claudeai"
    /// | "managed"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    /// Tools — present when connected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<McpToolInfo>>,
}

/// Response from `Client::mcp_status()`. Mirrors Python's
/// `McpStatusResponse` (`types.py:687-694`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusResponse {
    /// Per-server status entries.
    pub mcp_servers: Vec<McpServerStatus>,
}

/// One breakdown row in [`ContextUsageResponse::categories`]. Mirrors
/// Python's `ContextUsageCategory` (`types.py:697-703`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageCategory {
    /// Human-readable category name (e.g. `"System prompt"`).
    pub name: String,
    /// Tokens this category consumes.
    pub tokens: u64,
    /// UI hint for the `/context` display.
    pub color: String,
    /// Category is held in reserve (not currently loaded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_deferred: Option<bool>,
}

/// Response from `Client::get_context_usage()`. Mirrors Python's
/// `ContextUsageResponse` (`types.py:706-760`). Only the commonly-used
/// fields are typed here; the `Value`-backed vectors capture the
/// long-tail fields the CLI emits for the `/context` UI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsageResponse {
    /// Token usage by category.
    pub categories: Vec<ContextUsageCategory>,
    /// Total tokens currently in the context window.
    pub total_tokens: u64,
    /// Effective max tokens (possibly reduced by autocompact).
    pub max_tokens: u64,
    /// Raw model context-window size.
    pub raw_max_tokens: u64,
    /// Percentage of context window used (0–100).
    pub percentage: f64,
    /// Model the usage is calculated for.
    pub model: String,
    /// Whether autocompact is enabled.
    pub is_auto_compact_enabled: bool,
    /// Loaded `CLAUDE.md` / memory files.
    pub memory_files: Vec<Value>,
    /// MCP tools with metadata.
    pub mcp_tools: Vec<Value>,
    /// Agent definitions by source.
    pub agents: Vec<Value>,
    /// Grid rows used by the CLI display.
    pub grid_rows: Vec<Vec<Value>>,
    /// Autocompact trigger threshold (tokens).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold: Option<u64>,
    /// Deferred built-in tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deferred_builtin_tools: Option<Vec<Value>>,
    /// Built-in system tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_tools: Option<Vec<Value>>,
    /// System-prompt sections with token counts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt_sections: Option<Vec<Value>>,
    /// Slash-command usage summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slash_commands: Option<Value>,
}

/// External MCP server wire-config (non-SDK variants). Mirrors Python's
/// `McpStdioServerConfig / McpSSEServerConfig / McpHttpServerConfig`
/// (`types.py:549-572`). The in-process SDK variant lives on the
/// `McpServer` handle directly — use [`OptionsBuilder::mcp_server`](
/// crate::OptionsBuilder::mcp_server) for that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum McpServerConfig {
    /// stdio transport — spawned subprocess.
    Stdio {
        /// Command to spawn.
        command: String,
        /// Arguments to the command.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        args: Vec<String>,
        /// Environment variables for the child process.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        env: std::collections::HashMap<String, String>,
    },
    /// Server-Sent Events transport.
    Sse {
        /// Endpoint URL.
        url: String,
        /// Extra request headers.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        headers: std::collections::HashMap<String, String>,
    },
    /// HTTP transport.
    Http {
        /// Endpoint URL.
        url: String,
        /// Extra request headers.
        #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
        headers: std::collections::HashMap<String, String>,
    },
}

/// Bash sandbox configuration — Python's `SandboxSettings`
/// (`types.py:812-856`). Merged into `--settings` alongside any
/// explicit `settings` value via Python's `_build_settings_value`
/// (`subprocess_cli.py:111-163`). Fields are camelCase on the wire.
///
/// **Note:** Filesystem read/write restrictions and network
/// restrictions are NOT configured here — they travel through the
/// permission-rules surface (`Read`, `Edit`, `WebFetch`). Sandbox
/// settings control the *bash-command* sandbox specifically.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxSettings {
    /// Enable bash sandboxing (macOS/Linux only). Default: false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Auto-approve bash commands when sandboxed. Default: true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_allow_bash_if_sandboxed: Option<bool>,
    /// Commands that should run outside the sandbox
    /// (e.g. `["git", "docker"]`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub excluded_commands: Option<Vec<String>>,
    /// Allow commands to bypass sandbox via `dangerouslyDisableSandbox`.
    /// When false, all commands must run sandboxed (or be in
    /// `excluded_commands`). Default: true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unsandboxed_commands: Option<bool>,
    /// Network configuration for the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<SandboxNetworkConfig>,
    /// Violations to ignore.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignore_violations: Option<SandboxIgnoreViolations>,
    /// Enable weaker sandbox for unprivileged Docker (Linux only).
    /// Reduces security. Default: false.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enable_weaker_nested_sandbox: Option<bool>,
}

/// Sandbox network configuration. Mirrors Python's
/// `SandboxNetworkConfig` (`types.py:782-798`). Fields are camelCase.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxNetworkConfig {
    /// Unix socket paths accessible in the sandbox (e.g. SSH agents).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_unix_sockets: Option<Vec<String>>,
    /// Allow all Unix sockets (less secure).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_all_unix_sockets: Option<bool>,
    /// Allow binding to localhost ports (macOS only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_local_binding: Option<bool>,
    /// HTTP proxy port if bringing your own proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_proxy_port: Option<u16>,
    /// SOCKS5 proxy port if bringing your own proxy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub socks_proxy_port: Option<u16>,
}

/// Violations to ignore in the sandbox. Mirrors Python's
/// `SandboxIgnoreViolations` (`types.py:800-809`). Note that Python's
/// field names are `file` (singular) and `network` — these get passed
/// through as-is.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxIgnoreViolations {
    /// File paths for which violations should be ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Vec<String>>,
    /// Network hosts for which violations should be ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Vec<String>>,
}
