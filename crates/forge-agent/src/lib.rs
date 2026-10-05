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

pub use agent::{Agent, AgentError, AgentHandle, WorkerListing};
pub use client::{AgentEvent, SessionLaunchSettings};
pub use forge_primitives::permission::PermissionMode;

/// The messages a session's transcript holds and the compactions it
/// records. The same read the spawn performs, available for a session that
/// is already running.
///
/// **A view is not the only way to a running session's conversation any
/// more.** The socket's transport holds one it is seeded with by the session
/// task, so this read is for a consumer that has no such copy - a view
/// attaching late, or a fixture.
pub fn session_history(
    config_dir: &std::path::Path,
    session_id: &str,
    cwd: &str,
) -> forge_primitives::ConversationHistory {
    forge_sdk_worker::load_history_messages(config_dir, session_id, cwd, session_id)
}

/// The span of a session's transcript that ends below `ends_before`, for a
/// paging read that has walked below the window a conversation is kept in.
///
/// [`session_history`] is the whole file and answers what a session IS;
/// this is a window of it and answers what a page needs. `anchor` is a frame
/// id the caller's held conversation still carries together with the index
/// the session gives that frame, which is what lets the read find the span
/// without walking the file from its start.
pub fn transcript_span(
    config_dir: &std::path::Path,
    session_id: &str,
    cwd: &str,
    anchors: &[forge_primitives::TranscriptAnchor],
    ends_before: usize,
    rows: usize,
) -> Option<forge_primitives::TranscriptSpan> {
    let dir = if cwd.is_empty() { None } else { Some(cwd) };
    userdata::catalog::scan::read_span(config_dir, session_id, dir, anchors, ends_before, rows)
}
