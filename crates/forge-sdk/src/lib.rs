//! # forge-sdk
//!
//! A peer reference implementation in Rust of a client for Anthropic's
//! `claude` CLI. Spawns the binary as a subprocess and speaks
//! stream-json over stdio. Wire compatibility with the CLI is the only
//! hard external invariant; API shape is whatever serves
//! [`forge-daemon`](https://github.com/vedhavyas/forge/tree/main/crates/forge-daemon)
//! and [`forge-tui`](https://github.com/vedhavyas/forge/tree/main/crates/forge-tui)
//! best.
//!
//! ## Design
//!
//! The SDK is a thin wrapper around the `claude` binary. All agentic
//! work — tool dispatch, conversation history, session persistence —
//! happens inside the CLI itself. This crate is responsible for:
//!
//! - Spawning the subprocess with the right flags.
//! - Parsing the stream-json output into typed Rust values.
//! - Serialising user messages into stream-json input.
//! - Bridging the `can_use_tool` callback (when enabled) across the wire.
//! - Hosting in-process MCP tool servers that the `claude` binary can call.
//!
//! [`Client`] is `Clone`-able; all commands take `&self`. Internally
//! a reader task owns the subprocess after init, decodes lines,
//! dispatches inbound `control_request`s on detached tasks, and
//! routes outbound `control_response`s to per-request waiters.
//!
//! ## Minimal example
//!
//! ```no_run
//! # async fn example() -> anyhow::Result<()> {
//! use forge_sdk::{Client, OptionsBuilder};
//!
//! let options = OptionsBuilder::new().build();
//! let (client, mut events) = Client::spawn(options).await?;
//! client.send_user_message("hello").await?;
//! while let Some(item) = events.recv().await {
//!     let event = item?;
//!     println!("{event:?}");
//! }
//! client.disconnect().await?;
//! # Ok(()) }
//! ```

#![doc(html_root_url = "https://docs.rs/forge-sdk/0.1.64")]
#![forbid(unsafe_code)]

pub mod argv;
mod client;
pub mod content;
pub mod control;
mod error;
pub mod git;
pub mod hooks;
pub mod mcp;
pub(crate) mod messages;
mod options;
pub mod paths;
pub(crate) mod permissions;
pub(crate) mod public_types;
pub(crate) mod request_id;
pub mod subagents;
pub mod transport;

pub use client::{Client, ClientEvents};
pub use error::Error;
pub use git::{GitBranch, GitContext, GitContextWatcher, GitError, git_context};
pub use paths::{claude_config_dir, projects_dir};
// Top-level message + content re-exports so consumers can say
// `use forge_sdk::{AssistantEnvelope, StopReason, RateLimitInfo, ...}`
// instead of reaching through `forge_sdk::messages::*`. Matches the
// the flat `__init__.py` surface.
#[doc(hidden)]
pub use crate::mcp::macros::__private;
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
    AccountInfo, ContextUsageCategory, ContextUsageResponse, McpServerConfig,
    McpServerConnectionStatus, McpServerInfo, McpServerStatus, McpStatusResponse,
    McpToolAnnotations, McpToolInfo, OauthCredentials, SDKSessionInfo, SandboxIgnoreViolations,
    SandboxNetworkConfig, SandboxSettings, SessionMessage, SessionMessageKind, SettingSource,
    StreamEvent,
};

/// Free-function variant of [`Client::oauth_credentials`] for callers
/// that don't have a live [`Client`] but still need to consult the
/// user's OAuth state (e.g. a TUI verifying credentials immediately
/// after `claude auth login` exits, before a session is open).
///
/// Reads `<config_dir>/.credentials.json` where `<config_dir>` is
/// `$CLAUDE_CONFIG_DIR` (when set + non-empty) else `$HOME/.claude`.
/// Returns `None` if the file is missing, malformed, or
/// `claudeAiOauth.accessToken` is empty.
#[must_use]
pub fn oauth_credentials() -> Option<OauthCredentials> {
    paths::load_oauth_credentials()
}

/// Convenient alias for `Result<T, forge_sdk::Error>`.
pub type Result<T, E = Error> = core::result::Result<T, E>;

/// One-shot helper that spawns a client, sends a single prompt, drains
/// every message up to and including the terminal [`messages::Message::Result`]
/// frame, and disconnects. SDK's top-level `query()`
/// helper (`query.py:11-40`).
///
/// `options` is optional — pass `None` for the default configuration,
/// passing `None` requests the CLI's defaults.
///
/// Collects every message before returning. For streaming
/// consumption (message-by-message as the CLI emits them), use
/// [`query_stream`].
///
/// # Errors
///
/// Any [`Error`] variant — see [`Client::spawn`],
/// [`Client::send_user_message`], [`Client::disconnect`].
pub async fn query(
    prompt: impl AsRef<str>,
    options: Option<Options>,
) -> Result<Vec<messages::Message>> {
    let (client, mut events) = Client::spawn(options.unwrap_or_default()).await?;
    client.send_user_message(prompt.as_ref()).await?;
    let mut messages = Vec::new();
    while let Some(item) = events.recv().await {
        let msg = item?;
        let is_result = matches!(msg, messages::Message::Result { .. });
        messages.push(msg);
        if is_result {
            break;
        }
    }
    client.disconnect().await?;
    Ok(messages)
}

/// Streaming counterpart to [`query`]. Returns a
/// [`Stream`](tokio_stream::Stream) that yields each message as the
/// CLI emits it, closing once the terminal `Message::Result` frame
/// has been delivered (or on error).
///
/// SDK's `query()` return shape
/// (`AsyncIterator[Message]`, `query.py:11`). Use this when you want
/// to react to partial assistant turns, tool-use blocks, or
/// rate-limit events as they arrive rather than waiting for the
/// whole turn to finish.
///
/// Errors land as the final `Result::Err` item before the stream
/// closes — same error surface as the events receiver and
/// [`Client::disconnect`].
pub fn query_stream(
    prompt: impl Into<String>,
    options: Option<Options>,
) -> impl tokio_stream::Stream<Item = Result<messages::Message>> + Send + 'static {
    let prompt = prompt.into();
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Result<messages::Message>>();
    tokio::spawn(async move {
        let (client, mut events) = match Client::spawn(options.unwrap_or_default()).await {
            Ok(pair) => pair,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        if let Err(e) = client.send_user_message(&prompt).await {
            let _ = tx.send(Err(e));
            // Surface a disconnect failure to the consumer too — a
            // subprocess that exits non-zero on shutdown should not
            // disappear silently. `let _ =` on the channel send is
            // fine: if the receiver was dropped we have nothing useful
            // to do with the error.
            if let Err(e) = client.disconnect().await {
                let _ = tx.send(Err(e));
            }
            return;
        }
        while let Some(item) = events.recv().await {
            match item {
                Ok(msg) => {
                    let is_result = matches!(msg, messages::Message::Result { .. });
                    if tx.send(Ok(msg)).is_err() {
                        // Consumer dropped the stream — stop driving.
                        break;
                    }
                    if is_result {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e));
                    break;
                }
            }
        }
        // Same surface-disconnect-error treatment on the happy-path
        // exit. query()`'s `client.disconnect().await?` —
        // streaming consumers shouldn't be in a worse position than
        // one-shot consumers when the subprocess fails to clean up.
        if let Err(e) = client.disconnect().await {
            let _ = tx.send(Err(e));
        }
    });
    tokio_stream::wrappers::UnboundedReceiverStream::new(rx)
}
