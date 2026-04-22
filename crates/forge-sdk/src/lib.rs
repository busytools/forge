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
pub mod argv;
mod client;
pub mod content;
pub mod control;
mod error;
pub mod hooks;
pub mod mcp;
pub(crate) mod messages;
mod options;
pub(crate) mod permissions;
pub(crate) mod public_types;
pub(crate) mod request_id;
pub mod session;
pub mod testing;
pub mod tracing_bridge;
pub(crate) mod transcript_mirror_batcher;
pub mod transport;

pub use client::Client;
pub use error::Error;
pub use transport::Transport;
// Top-level message + content re-exports so consumers can say
// `use forge_sdk::{AssistantEnvelope, StopReason, RateLimitInfo, ...}`
// instead of reaching through `forge_sdk::messages::*`. Matches the
// Python SDK's flat `__init__.py` surface.
pub use content::ContentBlock;
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
pub use messages::{
    AssistantEnvelope, AssistantMessageError, Message, RateLimitInfo, RateLimitStatus,
    RateLimitType, StopReason, TaskNotificationStatus, TaskUsage, Usage, UserEnvelope,
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
pub use session::store::{
    FsSessionStore, MemorySessionStore, SessionKey, SessionListSubkeysKey, SessionStore,
    SessionStoreEntry, SessionStoreError, SessionStoreListEntry,
};

/// In-memory [`SessionStore`] — Python SDK publishes this as
/// `InMemorySessionStore`; forge-sdk aliases for surface parity.
pub use session::store::MemorySessionStore as InMemorySessionStore;

pub use session::summary::{SessionSummaryEntry, fold_session_summary, summary_entry_to_sdk_info};

#[doc(hidden)]
pub use crate::mcp::macros::__private;

/// Convenient alias for `Result<T, forge_sdk::Error>`.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// One-shot helper that spawns a client, sends a single prompt, drains
/// every message up to and including the terminal [`messages::Message::Result`]
/// frame, and disconnects. Mirrors Python SDK's top-level `query()`
/// helper (`query.py:11-40`).
///
/// `options` is optional — pass `None` for the default configuration,
/// matching Python's `options: ClaudeAgentOptions | None = None`
/// keyword-only argument.
///
/// Collects every message before returning. For streaming consumption
/// (message-by-message as the CLI emits them, matching Python's
/// `AsyncIterator[Message]` return shape), use [`query_stream`].
///
/// # Errors
///
/// Any [`Error`] variant — see [`Client::spawn`],
/// [`Client::send_user_message`], [`Client::next_event`],
/// [`Client::disconnect`].
pub async fn query(
    prompt: impl AsRef<str>,
    options: Option<Options>,
) -> Result<Vec<messages::Message>> {
    let mut client = Client::spawn(options.unwrap_or_default()).await?;
    client.send_user_message(prompt.as_ref()).await?;
    let messages = client.receive_response().await?;
    client.disconnect().await?;
    Ok(messages)
}

/// Streaming counterpart to [`query`]. Returns a
/// [`Stream`](tokio_stream::Stream) that yields each message as the
/// CLI emits it, closing once the terminal `Message::Result` frame
/// has been delivered (or on error).
///
/// Mirrors Python SDK's `query()` return shape
/// (`AsyncIterator[Message]`, `query.py:11`). Use this when you want
/// to react to partial assistant turns, tool-use blocks, or
/// rate-limit events as they arrive rather than waiting for the
/// whole turn to finish.
///
/// Errors land as the final `Result::Err` item before the stream
/// closes — same error surface as [`Client::next_event`] and
/// [`Client::disconnect`].
pub fn query_stream(
    prompt: impl Into<String>,
    options: Option<Options>,
) -> impl tokio_stream::Stream<Item = Result<messages::Message>> + Send + 'static {
    let prompt = prompt.into();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<messages::Message>>();
    tokio::spawn(async move {
        let mut client = match Client::spawn(options.unwrap_or_default()).await {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        if let Err(e) = client.send_user_message(&prompt).await {
            let _ = tx.send(Err(e));
            let _ = client.disconnect().await;
            return;
        }
        loop {
            match client.next_event().await {
                Ok(Some(msg)) => {
                    let is_result = matches!(msg, messages::Message::Result { .. });
                    if tx.send(Ok(msg)).is_err() {
                        // Consumer dropped the stream — stop driving.
                        break;
                    }
                    if is_result {
                        break;
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
        let _ = client.disconnect().await;
    });
    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
}
