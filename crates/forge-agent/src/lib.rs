//! `forge-agent` — drives one [`forge_sdk::Client`] and exposes the
//! `AgentBridge` trait + `ForgeSdkBridge` impl to UI consumers.
//!
//! Created during the 2026-05-05 restructure (phase 3). Lifted from
//! `forge-tui::agent::*` so the SDK-driver code lives next to forge-sdk
//! rather than inside the UI crate. The eventual goal (next phase) is
//! to drop `AgentBridge` entirely in favour of a channel-based API
//! (`Command`/`Event` over mpsc); for now the trait stays so the lift
//! is reviewable on its own.
//!
//! # Module layout
//!
//! - [`client`] — `AgentBridge` trait + `AgentEvent` + supporting types.
//! - [`forge_sdk_bridge`] — single in-process `AgentBridge` impl wrapping
//!   `forge_sdk::Client`.
//! - [`forge_sdk_worker`] — spawn dance + reader subtask helpers.
//! - [`session_lifecycle`] — model resolution, mode wiring, one-shot
//!   helpers used by the worker.
//! - [`commands`] — typed command builders for the bridge.
//! - [`user_interaction`] — `can_use_tool` callback runtime + AskUserQuestion driver.
//! - [`history`] — session-message history → SessionUpdate translation.
//! - [`state`] — `PermissionMode` enum.

pub mod client;
pub mod commands;
pub mod forge_sdk_bridge;
pub mod forge_sdk_worker;
pub mod history;
pub mod logging;
pub mod session_lifecycle;
pub mod state;
pub mod tooling;
pub mod user_interaction;

pub use client::{AgentBridge, AgentEvent, PromptResponse, SessionLaunchSettings};
pub use forge_sdk_bridge::ForgeSdkBridge;
pub use state::PermissionMode;
