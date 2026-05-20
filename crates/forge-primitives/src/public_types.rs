//! Public types that don't yet participate in any wire-path in
//! forge-sdk but are useful for callers that build or inspect
//! configuration values. The serde impls match the CLI's wire shape
//! exactly.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Which settings scope to load. Wire shape:
/// `Literal["user", "project", "local"]`.
///
/// Combinations are expressed by passing multiple variants — see
/// `forge_sdk::OptionsBuilder::setting_sources`.
/// The CLI resolves the actual on-disk paths from whichever
/// `CLAUDE_CONFIG_DIR` is active (env var wins, else `$HOME/.claude`);
/// the paths below describe the layout, not hardcoded locations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SettingSource {
    /// User-scope settings at `<config_dir>/settings.json`
    /// (`<config_dir>` = `$CLAUDE_CONFIG_DIR` if set, else `~/.claude`).
    User,
    /// Project-scope settings at `<repo>/.claude/settings.json`.
    Project,
    /// Project-local (gitignored) settings at
    /// `<repo>/.claude/settings.local.json`.
    Local,
}

impl SettingSource {
    /// String form for `--setting-sources=<csv>`.
    pub fn as_cli_arg(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Project => "project",
            Self::Local => "local",
        }
    }
}

/// Account / subscription info the CLI emits in the session-init
/// payload. Mirrors the shape `client.initial_session_data()["account"]`
/// would deserialize to. All fields are optional because the CLI omits
/// any it can't determine (e.g. `email` is `None` for API-key-only
/// auth). Wire keys are camelCase to match the CLI; the Rust fields
/// use `snake_case` via `rename_all`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountInfo {
    /// Logged-in user email when first-party OAuth is active.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Organization the account belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub organization: Option<String>,
    /// Subscription tier label (e.g. `"team"`, `"enterprise"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_type: Option<String>,
    /// Where the auth token came from (`"oauth"`, `"api_key"`, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_source: Option<String>,
    /// Where the API key was loaded from (`"environment"`, `"keychain"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_source: Option<String>,
    /// API provider identifier when the request goes through a
    /// non-Anthropic gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_provider: Option<String>,
}

/// Forge's view of the active account — the picker-side identity
/// from `forge.toml`'s `[[accounts]]`, peer to the CLI-side
/// [`AccountInfo`].
///
/// `AccountInfo` mirrors the `claude` CLI's wire payload (email,
/// org, subscription_type). This struct is forge-internal: it
/// names the active account from `forge.toml`. Surfaced when
/// forge-workspace picks an account; absent when `Agent::spawn`
/// is called directly with `display_name = None` (tests, smoke).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ForgeAccountIdentity {
    pub display_name: String,
}

impl ForgeAccountIdentity {
    /// Convenience constructor.
    pub fn new(display_name: String) -> Self {
        Self { display_name }
    }
}

/// Streaming partial-message event surfaced when
/// `Options.include_partial_messages` is set. Wire shape:
/// `StreamEvent`.
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

/// One session-listing row from `list_sessions()`. Wire shape:
/// `SDKSessionInfo`.
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

/// One user / assistant message from a session transcript, as
/// returned by `get_session_messages()`.
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

/// User / assistant discriminator. Wire shape:
/// `Literal["user", "assistant"]` on `SessionMessage.type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMessageKind {
    /// User turn.
    User,
    /// Assistant turn.
    Assistant,
}

/// Possible connection statuses for an MCP server. Wire shape:
/// `McpServerConnectionStatus`.
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

/// MCP tool annotations. Wire keys are camelCase.
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

/// Per-server status entry inside [`McpStatusResponse`]. Wire shape:
/// `McpServerStatus`. Wire shape matches the CLI's
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
    /// Opaque config blob — one of the CLI's `McpServerStatusConfig`
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
    /// Whether the server has been wired with a model-sampling
    /// callback. CLI-emitted; surfaced for UIs that show a "sampling
    /// configured" badge next to a server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_configured: Option<bool>,
    /// Whether the server *requires* sampling support to function.
    /// `true` when the server's MCP manifest declares it. UIs use this
    /// to warn the user when `sampling_configured == Some(false)` and
    /// `sampling_required == Some(true)`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling_required: Option<bool>,
}

/// Response from `Client::mcp_status()`. Wire shape:
/// `McpStatusResponse`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpStatusResponse {
    /// Per-server status entries.
    pub mcp_servers: Vec<McpServerStatus>,
}

/// One breakdown row in [`ContextUsageResponse::categories`]. Mirrors
/// the CLI's `ContextUsageCategory`.
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

/// Response from `Client::get_context_usage()`. Wire shape:
/// `ContextUsageResponse`. Only the commonly-used
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

/// External MCP server wire-config (non-SDK variants). Wire shape:
/// `McpStdioServerConfig / McpSSEServerConfig / McpHttpServerConfig`.
/// The in-process SDK variant lives on the `McpServer` handle
/// directly — use `forge_sdk::OptionsBuilder::mcp_server` for that.
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

/// Bash sandbox configuration — the CLI's `SandboxSettings`.
/// Merged into `--settings` alongside any explicit `settings`
/// value via the CLI's `_build_settings_value`. Fields are
/// camelCase on the wire.
///
/// Filesystem read/write and network restrictions travel through the
/// permission-rules surface (`Read`, `Edit`, `WebFetch`); these
/// settings control the bash-command sandbox only.
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

/// Sandbox network configuration. Wire shape:
/// `SandboxNetworkConfig`. Fields are camelCase.
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

/// Violations to ignore in the sandbox. Wire field names are
/// `file` (singular) and `network`; forge-sdk passes them through
/// as-is.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxIgnoreViolations {
    /// File paths for which violations should be ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<Vec<String>>,
    /// Network hosts for which violations should be ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<Vec<String>>,
}

#[cfg(test)]
mod forge_account_identity_tests {
    use super::ForgeAccountIdentity;

    #[test]
    fn default_is_empty_display_name() {
        let identity = ForgeAccountIdentity::default();
        assert_eq!(identity.display_name, "");
    }

    #[test]
    fn equality_compares_display_name() {
        let a = ForgeAccountIdentity { display_name: "Stargate".into() };
        let b = ForgeAccountIdentity { display_name: "Stargate".into() };
        let c = ForgeAccountIdentity { display_name: "Gateway".into() };
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
