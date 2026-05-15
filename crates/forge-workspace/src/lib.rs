//! `forge-workspace` — multi-session orchestrator and TUI-facing
//! facade.
//!
//! Pools [`forge_agent::Agent`] instances behind a single
//! [`Workspace`] handle. One [`DomainSession`] per active session
//! holds workspace-internal routing metadata (`AgentHandle` slot,
//! claude-issued `session_id`, pending-interaction mailbox).
//! Operational state TUI renders (lifecycle, cwd, turn state,
//! account info) lives on the TUI's `UiSession`; workspace is a thin
//! proxy that reacts to `Command`s in and emits `SessionUpdate`s out.
//! A per-session actor pumps events from `AgentHandle::take_events()`,
//! translates them into [`SessionUpdate`]s, and routes [`Command`]s
//! back.
//!
//! ## Communication contract
//!
//! Post-MVVM refactor (#102) the TUI ↔ workspace contract is **one
//! channel pair**:
//!
//! - **TUI → workspace:** [`Workspace::dispatch`] takes a
//!   [`protocol::Command`]. One enum, one entry point.
//! - **workspace → TUI:** [`Workspace::subscribe`] returns a receiver
//!   for [`protocol::SessionUpdate`]. One enum, one consumer.
//!
//! No second channel for "control events" vs "data events." No
//! callback hooks. No shared mutable state.
//!
//! Strict-wiring follow-up (Phase 6): TUI no longer holds an
//! `Arc<AgentHandle>`. Every outbound call — `Prompt`, `Cancel`,
//! `SetMode`/`SetModel`, `NewSession`/`ResumeSession`/`ResumeOrNew`,
//! `GenerateSessionTitle`/`RenameSession`, the MCP suite
//! (`ReconnectMcpServer`, `ToggleMcpServer`, `AuthenticateMcpServer`,
//! `ClearMcpAuth`, `SetMcpServers`, `SubmitMcpOauthCallbackUrl`),
//! `RespondElicitation`, the git-watch start/stop pair — flows
//! through `Workspace::dispatch(Command)`. Query-style refreshes
//! (`refresh_status_snapshot`, `refresh_oauth_credentials_snapshot`,
//! `refresh_context_usage`, `reload_plugins`, `refresh_mcp_snapshot`)
//! and direct accessors (`settings_documents`, `write_settings_document`,
//! `project_memory_path`, `config_dir_for`, `oauth_usage`) live as
//! inherent methods on [`Workspace`]. The `Connected` /
//! `SessionReplaced` / `AuthCompleted` updates no longer carry an
//! `Arc<AgentHandle>` either — the handle stays on the workspace's
//! [`DomainSession`].
//!
//! ## Single-channel event bus
//!
//! The same `SessionUpdate` channel TUI subscribes to is also reused
//! as an event bus for TUI-internal async work. A few TUI-side
//! modules (`forge_tui::app::plugins`, `slash::executors`,
//! `service_status_check`, `input_submit`) grab a sender via
//! [`Workspace::update_sender`] and emit their own `SessionUpdate`s
//! rather than dispatching a `Command` and waiting for a round-trip.
//!
//! These are TUI-originated presentation events that reuse the
//! existing channel as a single event bus rather than spinning up a
//! second one. They only mutate presentation-side state in TUI's
//! `UiSession` buckets — workspace itself never reads those updates.
//!
//! Future-proofing watchlist: if the goal ever becomes "swap the TUI
//! for a different frontend," the only contract a replacement should
//! need to honor is the two-enum-stream boundary. The leaky-emitter
//! pattern above is an implicit second contract. Tracked at
//! <https://github.com/busytools/forge/issues/105> — not urgent.
//!
//! ## Facade scope (intentionally thin)
//!
//! The MVVM boundary between forge-tui and forge-agent is enforced at
//! the **dependency graph** level: `forge-tui/Cargo.toml` has no
//! `forge-agent` line; everything routes through forge-workspace. But
//! the workspace exposes forge-agent's submodules verbatim via
//! pass-through `pub use` (see `cloud`, `commands`, `env::git_diff`,
//! `session_lifecycle`, `tooling`, `translate`, `userdata` below).
//! Types like `forge_workspace::cloud::oauth::Token` are *defined* in
//! forge-agent — the workspace just exposes them under the workspace
//! name so TUI can keep its dep graph clean.
//!
//! Tightening this (specific [`Workspace`] methods in place of each
//! wildcard re-export) is a "Phase 7 narrow agent surface" follow-up.
//! Not on the current roadmap; documented for future reference.

mod account;
mod config;
mod domain_session;
mod error;
pub mod protocol;
mod session_task;
mod spawn;
mod target;
pub mod ui;
mod views;
mod workspace;

pub use account::UsageFetchStatus;
pub use domain_session::DomainSession;
pub use error::WorkspaceError;
pub use protocol::{Command, DispatchError, PendingInteractionSlot, SessionUpdate, TurnErrorClass};
pub use target::{ProjectKey, SessionKey, SessionTarget};
pub use ui::{SpinnerStyle, UiSettings};
pub use views::{ProjectView, SessionView};
pub use workspace::Workspace;

// Re-export forge-agent types that public surface returns, so
// callers can write `use forge_workspace::AgentHandle` if they
// prefer.
pub use forge_agent::AgentHandle;
pub use forge_agent::client::SessionLaunchSettings;

// Re-export forge-agent sub-surfaces consumed by `forge-tui` so the
// TUI crate doesn't need a direct `forge-agent` dep. Each entry below
// is the workspace-side facade for a forge-agent module forge-tui
// reads from. Most types here are themselves re-exports from
// `forge_primitives`; the modules also surface a handful of helper
// functions / network fetchers that genuinely live in `forge-agent`.
//
// Production code in forge-tui consumes data via `SessionUpdate`
// events; these re-exports back the small set of helpers + types the
// TUI still calls directly (translate::*, tooling::*, commands::*,
// session_lifecycle::*, cloud::*, env::git_diff::*, userdata::*).
pub mod cloud {
    pub use forge_agent::cloud::*;
}
pub mod commands {
    pub use forge_agent::commands::*;
}
pub mod env {
    pub mod cli_version {
        pub use forge_agent::env::cli_version::*;
    }
    pub mod git_diff {
        pub use forge_agent::env::git_diff::*;
    }
    pub mod processes {
        pub use forge_agent::env::processes::*;
    }
}
pub mod session_lifecycle {
    pub use forge_agent::session_lifecycle::*;
}
pub mod tooling {
    pub use forge_agent::tooling::*;
}
pub mod translate {
    pub use forge_agent::translate::*;
}
pub mod userdata {
    pub use forge_agent::userdata::*;
}
pub use forge_agent::state::PermissionMode;

// Test-only re-exports. The smoke-test suite at
// `crates/forge-tui/tests/forge_sdk_smoke.rs` needs `Agent::spawn`
// and the `AgentEvent` enum to drive a real `claude` subprocess
// end-to-end; production code uses `Workspace`'s facade and consumes
// `SessionUpdate`s, never these raw types. Gating these symbols
// behind the `testing` feature keeps the production build's surface
// minimal — `cargo check --no-default-features -p forge-workspace`
// won't carry `forge_workspace::Agent` or `forge_workspace::AgentEvent`.
#[cfg(feature = "testing")]
pub use forge_agent::Agent;
#[cfg(feature = "testing")]
pub use forge_agent::AgentEvent;
