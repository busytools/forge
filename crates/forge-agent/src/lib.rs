//! `forge-agent` - drives one [`forge_sdk::Client`] and exposes a
//! channel-based [`Agent`] / [`AgentHandle`] surface to UI consumers.
//!
//! Public API: [`Agent::spawn`] returns an [`AgentHandle`] holding
//! `Sender<Command>`, a `take_events()` for the bridge's `AgentEvent`
//! stream, and direct-accessor passthroughs (config_dir,
//! settings_documents, oauth_*).
//!
//! `ForgeSdkBridge` is a `pub(crate)` implementation detail - Agent's
//! dispatcher task is the only caller.
//!
//! # Module layout
//!
//! - [`agent`] - `Agent::spawn` + `AgentHandle` (the public API).
//! - [`client`] - `AgentEvent` enum + supporting types.
//! - [`cloud`] - network-side state: oauth + cli usage fetchers.
//! - [`userdata`] - disk-side state: settings, sessions catalog, memory, plugins.
//! - [`commands`] / [`session_lifecycle`] - bridge helpers reused by
//!   forge-tui via re-exports.
//! - `forge_sdk_worker` (crate-private) / `replay` / [`tooling`] /
//!   [`user_interaction`] - internal implementation modules consumed by
//!   `agent`'s dispatcher and translator paths.

pub mod agent;
pub mod client;
pub mod cloud;
pub mod commands;
pub mod env;
pub(crate) mod forge_sdk_bridge;
pub(crate) mod forge_sdk_worker;
pub mod http_trust;
pub mod logging;
pub(crate) mod replay;
pub mod session_lifecycle;
pub mod tooling;
pub mod translate;
pub mod user_interaction;
pub mod userdata;

pub use agent::{Agent, AgentError, AgentHandle};
pub use client::{AgentEvent, SessionLaunchSettings};
pub use forge_primitives::permission::PermissionMode;
