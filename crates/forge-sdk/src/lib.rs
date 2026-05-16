//! # forge-sdk
//!
//! A peer reference implementation in Rust of a client for Anthropic's
//! `claude` CLI. Spawns the binary as a subprocess and speaks
//! stream-json over stdio. Wire compatibility with the CLI is the only
//! hard external invariant; API shape is whatever serves
//! [`forge-agent`](https://github.com/busytools/forge/tree/main/crates/forge-agent)
//! (and through it,
//! [`forge-tui`](https://github.com/busytools/forge/tree/main/crates/forge-tui))
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

#![doc(html_root_url = "https://docs.rs/forge-sdk/0.14.2")]
#![forbid(unsafe_code)]

pub mod argv;
mod client;
pub mod control;
mod error;
pub mod hooks;
pub(crate) mod logging;
pub mod mcp;
mod options;
pub mod paths;
pub(crate) mod permissions;
pub(crate) mod request_id;
pub mod subagents;
pub mod transport;

pub use crate::mcp::macros::__private;
pub use client::{Client, ClientEvents};
pub use error::Error;
pub use paths::{claude_config_dir_from_env, projects_dir_for};
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
pub use permissions::CanUseToolCallback;

/// Convenient alias for `Result<T, forge_sdk::Error>`.
pub type Result<T, E = Error> = core::result::Result<T, E>;
