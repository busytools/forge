//! `forge-agent` — drives one [`forge_sdk::Client`] and exposes a
//! channel-based [`Agent`] / [`AgentHandle`] surface to UI consumers.
//!
//! Public API: [`Agent::spawn`] returns an [`AgentHandle`] holding
//! `Sender<Command>`, a `take_events()` for the bridge's `AgentEvent`
//! stream, and direct-accessor passthroughs (config_dir,
//! settings_documents, oauth_*).
//!
//! `AgentBridge` trait + `ForgeSdkBridge` impl are `pub(crate)`
//! implementation details — Agent's dispatcher task is the only
//! caller.
//!
//! # Module layout
//!
//! - [`agent`] — `Agent::spawn` + `AgentHandle` (the public API).
//! - [`client`] — `AgentBridge` trait + `AgentEvent` enum + supporting types.
//! - [`cloud`] — network-side state: oauth + cli usage fetchers.
//! - [`userdata`] — disk-side state: trust file (more incoming).
//! - [`commands`] / [`session_lifecycle`] — bridge helpers reused by
//!   forge-tui via re-exports.
//! - [`forge_sdk_worker`] / [`history`] / [`tooling`] / [`user_interaction`] /
//!   [`state`] — internal implementation modules consumed by `agent`'s
//!   dispatcher and translator paths.

pub mod agent;
pub mod client;
pub mod cloud;
pub mod commands;
pub(crate) mod forge_sdk_bridge;
pub mod forge_sdk_worker;
pub mod history;
pub mod logging;
pub mod session_lifecycle;
pub mod state;
pub mod tooling;
pub mod translate;
pub mod user_interaction;
pub mod userdata;

pub use agent::{Agent, AgentHandle};
// `AgentBridge` trait + `ForgeSdkBridge` impl stay alive INTERNALLY
// (used by Agent's dispatcher + tee_events tasks) but are no longer
// the recommended consumer surface. Phase 7 hid them; phase 8 (future
// userdata work) may delete them entirely once the channel API
// covers everything.
pub use client::{AgentEvent, PromptResponse, SessionLaunchSettings};
pub use state::PermissionMode;
