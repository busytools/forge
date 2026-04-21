//! # forge-sdk
//!
//! A Rust port of Anthropic's [`claude-agent-sdk`](https://github.com/anthropics/claude-agent-sdk-python)
//! at API-parity with the Python implementation. Spawns the `claude` CLI binary
//! as a subprocess and speaks stream-json over stdio.
//!
//! ## Design
//!
//! The SDK is a thin wrapper around the `claude` binary. All agentic work —
//! tool dispatch, conversation history, session persistence — happens inside
//! the CLI itself. This crate is responsible for:
//!
//! - Spawning the subprocess with the right flags.
//! - Parsing the stream-json output into typed Rust values.
//! - Serialising user messages into stream-json input.
//! - Bridging the `can_use_tool` callback (when enabled) across the wire.
//! - Hosting in-process MCP tool servers that the `claude` binary can call.
//!
//! ## Minimal example
//!
//! ```no_run
//! # async fn example() -> anyhow::Result<()> {
//! use forge_sdk::{Client, OptionsBuilder};
//!
//! let options = OptionsBuilder::new().build();
//! let mut client = Client::spawn(options).await?;
//! client.send_user_message("hello").await?;
//! while let Some(event) = client.next_event().await? {
//!     println!("{event:?}");
//! }
//! client.disconnect().await?;
//! # Ok(()) }
//! ```

#![doc(html_root_url = "https://docs.rs/forge-sdk/0.1.64")]
#![forbid(unsafe_code)]

pub mod agents;
mod client;
pub mod content;
pub mod control;
mod error;
pub mod hooks;
pub mod mcp;
pub mod messages;
mod options;
pub mod permissions;
pub mod public_types;
pub(crate) mod request_id;
pub mod session_mutations;
pub mod session_store;
pub mod sessions;
pub mod sessions_store;
pub mod tracing_bridge;
pub(crate) mod transcript_mirror_batcher;
pub mod transport;

pub use client::Client;
pub use error::Error;
pub use hooks::{
    BaseHookInput, HookCallback, HookContext, HookDecision, HookKind, HookSpecificOutput, Hooks,
    HooksBuilder, NotificationHookSpecificOutput, NotificationInput,
    PermissionRequestHookSpecificOutput, PermissionRequestInput,
    PostToolUseFailureHookSpecificOutput, PostToolUseFailureInput, PostToolUseHookSpecificOutput,
    PostToolUseInput, PreCompactInput, PreToolUseHookSpecificOutput, PreToolUseInput,
    PreToolUsePermissionDecision, SessionStartHookSpecificOutput, StopInput, SubagentContext,
    SubagentStartHookSpecificOutput, SubagentStartInput, SubagentStopInput,
    UserPromptSubmitHookSpecificOutput, UserPromptSubmitInput,
};
pub use options::{
    Options, OptionsBuilder, PermissionMode, SdkPluginConfig, SystemPromptKind, ThinkingConfig,
    ToolsPreset,
};
pub use permissions::{
    CanUseToolCallback, PermissionBehavior, PermissionDecision, PermissionRuleValue,
    PermissionUpdate, PermissionUpdateDestination, ToolPermissionContext,
};
pub use public_types::{
    ContextUsageCategory, ContextUsageResponse, McpServerConfig, McpServerConnectionStatus,
    McpServerInfo, McpServerStatus, McpStatusResponse, McpToolAnnotations, McpToolInfo,
    SDKSessionInfo, SandboxIgnoreViolations, SandboxNetworkConfig, SandboxSettings, SdkBeta,
    SessionMessage, SessionMessageKind, SettingSource, StreamEvent,
};
pub use session_store::{
    FsSessionStore, MemorySessionStore, SessionKey, SessionListSubkeysKey, SessionStore,
    SessionStoreEntry, SessionStoreError, SessionStoreListEntry,
};

/// In-memory [`SessionStore`] — Python SDK publishes this as
/// `InMemorySessionStore`; forge-sdk aliases for surface parity.
pub use session_store::MemorySessionStore as InMemorySessionStore;

#[doc(hidden)]
pub use crate::mcp::macros::__private;

/// Convenient alias for `Result<T, forge_sdk::Error>`.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// One-shot helper that spawns a client, sends a single prompt, drains
/// every message up to and including the terminal [`messages::Message::Result`]
/// frame, and disconnects. Mirrors Python SDK's top-level `query()`
/// helper (`query.py`).
///
/// Use this for stateless one-off prompts when you don't need to issue
/// follow-ups or interrupt the turn. For multi-turn / streaming
/// interactions hold a [`Client`] directly.
///
/// # Errors
///
/// Any [`Error`] variant — see [`Client::spawn`],
/// [`Client::send_user_message`], [`Client::next_event`],
/// [`Client::disconnect`].
pub async fn query(prompt: impl AsRef<str>, options: Options) -> Result<Vec<messages::Message>> {
    let mut client = Client::spawn(options).await?;
    client.send_user_message(prompt.as_ref()).await?;
    let mut out = Vec::new();
    while let Some(msg) = client.next_event().await? {
        let terminal = matches!(msg, messages::Message::Result { .. });
        out.push(msg);
        if terminal {
            break;
        }
    }
    client.disconnect().await?;
    Ok(out)
}
