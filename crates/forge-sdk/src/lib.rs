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

#![doc(html_root_url = "https://docs.rs/forge-sdk/0.0.1")]
#![forbid(unsafe_code)]

mod client;
pub mod content;
pub mod control;
mod error;
pub mod hooks;
pub mod mcp;
pub mod messages;
mod options;
pub mod permissions;
pub mod session_store;
pub mod tracing_bridge;
pub mod transport;

pub use client::Client;
pub use error::Error;
pub use hooks::{
    HookCallback, HookContext, HookDecision, HookKind, Hooks, HooksBuilder, PostToolUseInput,
    PreCompactInput, PreToolUseInput, StopInput, SubagentStopInput, UserPromptSubmitInput,
};
pub use options::{Options, OptionsBuilder, PermissionMode};
pub use permissions::{CanUseToolCallback, PermissionDecision, ToolPermissionContext};
pub use session_store::{
    FsSessionStore, MemorySessionStore, SessionKey, SessionListSubkeysKey, SessionStore,
    SessionStoreEntry, SessionStoreError, SessionStoreListEntry,
};

#[doc(hidden)]
pub use crate::mcp::macros::__private;

/// Convenient alias for `Result<T, forge_sdk::Error>`.
pub type Result<T, E = Error> = core::result::Result<T, E>;
