//! `forge-workspace` - multi-session orchestrator and TUI-facing
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
//! The TUI ↔ workspace contract is **one channel pair**:
//!
//! - **TUI → workspace:** [`Workspace::dispatch`] takes a
//!   [`protocol::Command`]. One enum, one entry point.
//! - **workspace → TUI:** [`Workspace::subscribe`] returns a receiver
//!   for [`protocol::SessionUpdate`]. One enum, one consumer.
//!
//! No second channel for "control events" vs "data events." No
//! callback hooks. No shared mutable state. TUI does not hold an
//! `Arc<AgentHandle>`: every outbound call goes through
//! `Workspace::dispatch(Command)`; query-style refreshes
//! (`refresh_status_snapshot`, `refresh_oauth_credentials_snapshot`,
//! `refresh_context_usage`, `reload_plugins`, `refresh_mcp_snapshot`)
//! and direct accessors (`settings_documents`,
//! `project_memory_path`, `config_dir_for`, `oauth_usage`) live as
//! inherent methods on [`Workspace`]. The handle stays on the
//! workspace's [`DomainSession`].
//!
//! ## Single-channel event bus
//!
//! The same `SessionUpdate` channel TUI subscribes to is also reused
//! as an event bus for TUI-internal async work. A few TUI-side
//! modules (`forge_tui::app::plugins`, `slash::executors`,
//! `service_status_check`, `input_submit`) grab a sender via
//! [`Workspace::update_sender`] and emit their own `SessionUpdate`s
//! rather than dispatching a `Command` and waiting for a round-trip.
//! They only mutate presentation-side state in TUI's `UiSession`
//! buckets - workspace itself never reads those updates.
//! See <https://github.com/busytools/forge/issues/105> for the
//! tracking issue.
//!
//! ## Facade scope (intentionally thin)
//!
//! The MVVM boundary between forge-tui and forge-agent is enforced at
//! the **dependency graph** level: `forge-tui/Cargo.toml` has no
//! `forge-agent` line; everything routes through forge-workspace. The
//! workspace exposes forge-agent's submodules verbatim via
//! pass-through `pub use` (see `cloud`, `commands`, `env::git_diff`,
//! `session_lifecycle`, `tooling`, `translate`, `userdata` below).
//! Types like `forge_workspace::cloud::oauth_credentials::OauthCredentials`
//! are *defined* in
//! forge-agent - the workspace just exposes them under the workspace
//! name so TUI can keep its dep graph clean.

mod account;
mod account_cache;
mod account_loader;
mod assignment_plan;
mod config;
mod crons;
mod dictate;
mod domain_session;
mod error;
mod gotify;
pub(crate) mod mcp;
pub mod protocol;
mod provider_probe;
mod review;
mod session_task;
mod single_instance;
mod spawn;
pub mod store;
mod target;
pub mod ui;
mod views;
mod workspace;

pub use account::{LoadingState, UsageFetchStatus};
pub use dictate::{
    DictateBind, DictateDeviceCatalog, DictateDeviceChoice, DictateFailure, DictateMode,
    DictateModel, DictateModelState, DictateOverrideUpdate, DictateOverrides, DictateRole,
    DictateSettings, DictateSnapshot,
};
// The normalizer's prompt axes reach the TUI only through this
// re-export: forge-tui depends on no forge-dictate crate of its own.
pub use dictate::resolve_capture_device;
pub use domain_session::DomainSession;
pub use error::WorkspaceError;
pub use forge_dictate::Device;
pub use forge_dictate::normalize::{Context, Structure, Styling};
pub use protocol::{Command, DictateOutcome, DispatchError, SessionUpdate, TurnErrorClass};
pub use target::{ProjectKey, SessionKey, SessionTarget};
pub use ui::{RepaintCadence, SpinnerStyle};
pub use views::{
    AccountAuth, AccountBudget, AccountLoadingRow, AccountRow, ProjectView, SessionView,
};
pub use workspace::{SessionChipInfo, SessionChipState, Workspace};

// MCP (peers / workers) public surface. The `mcp` module itself is
// crate-private now; these flat re-exports expose only the types
// production consumers (forge-tui) need to read off `SessionUpdate`
// payloads. The `testing`-feature block below adds the extra surface
// the `forge-test-harness` integration tests need (MCP server
// builders, mock facades, the caller-key resolver).
pub use mcp::cron::schedule::next_fire_after;
pub use mcp::gotify::types::GotifyNotification;
pub use mcp::peers::types::{CorrelationId, WrappedKind, WrappedPrompt};
pub use mcp::workers::types::{LiveWorkerState, WorkerEntry};

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
pub use config::PluginSettings;
pub mod commands {
    pub use forge_agent::commands::*;
}
pub mod env {
    pub mod cli_version {
        pub use forge_agent::env::cli_version::*;
    }
    pub mod file_index {
        pub use forge_agent::env::file_index::*;
    }
    pub mod git_diff {
        pub use forge_agent::env::git_diff::*;
    }
    pub mod processes {
        pub use forge_agent::env::processes::*;
    }
    pub mod timezone {
        pub use forge_agent::env::timezone::*;
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
/// Pass-through for the agent's typed
/// tool-input parsers (`AskUserQuestion`, `Monitor`, `Workflow`).
/// The TUI's tool-call event handlers consume these directly when
/// surfacing the chat one-liner + Inspector entry for the new CLI
/// 2.1.156 surfaces.
pub mod user_interaction {
    pub use forge_agent::user_interaction::*;
}
pub mod userdata {
    pub use forge_agent::userdata::*;
}
pub use forge_primitives::permission::PermissionMode;
pub use forge_primitives::runtime::RuntimeSessionState;

// Test-only re-exports. The smoke-test suite at
// `crates/forge-tui/tests/forge_sdk_smoke.rs` needs `Agent::spawn`
// and the `AgentEvent` enum to drive a real `claude` subprocess
// end-to-end; production code uses `Workspace`'s facade and consumes
// `SessionUpdate`s, never these raw types. Gating these symbols
// behind the `testing` feature keeps the production build's surface
// minimal - `cargo check --no-default-features -p forge-workspace`
// won't carry `forge_workspace::Agent` or `forge_workspace::AgentEvent`.
#[cfg(feature = "testing")]
pub use forge_agent::Agent;
#[cfg(feature = "testing")]
pub use forge_agent::AgentEvent;

// MCP test-harness re-exports. `forge-test-harness` integration tests
// drive the workers MCP server directly against a mock facade; that
// requires the server-builder, the resolver, and the facade trait /
// mock to be visible cross-crate. Gating on `testing` keeps them out
// of production builds.
#[cfg(feature = "testing")]
pub use mcp::peers::facade::CallerKeyResolver;
#[cfg(feature = "testing")]
pub use mcp::workers::build_server as build_workers_server;
#[cfg(feature = "testing")]
pub use mcp::workers::facade::{CallerProject, MockWorkerFacade, WorkerFacade};
pub use mcp::workers::facade::{LEAD_LABEL, PERSONAL_ORG};
