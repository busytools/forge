//! `forge-workspace` - multi-session orchestrator and view-facing
//! facade.
//!
//! Pools [`forge_agent::Agent`] instances behind a single
//! [`Workspace`] handle. One [`DomainSession`] per active session
//! holds workspace-internal routing metadata (`AgentHandle` slot,
//! claude-issued `session_id`, pending-interaction mailbox).
//! Operational state a view renders (lifecycle, cwd, turn state,
//! account info) stays on the view's side of the wire; workspace is a
//! thin proxy that reacts to `Command`s in and emits `SessionUpdate`s
//! out.
//! A per-session actor pumps events from `AgentHandle::take_events()`,
//! translates them into [`SessionUpdate`]s, and routes [`Command`]s
//! back.
//!
//! ## Communication contract
//!
//! The view ↔ workspace contract is **one entry point in each
//! direction**:
//!
//! - **view → workspace:** [`Workspace::dispatch`] takes a
//!   [`protocol::Command`]. One enum, one entry point.
//! - **workspace → view:** [`Workspace::subscribe`] returns a receiver
//!   for [`protocol::SessionUpdate`]. Every caller gets a stream of its
//!   own, so a second view attaches beside the first. A stream carries
//!   what is emitted after it subscribes, and the first caller to
//!   attach is handed what was emitted before it as well, so a notice
//!   raised during boot is not lost. [`Workspace::subscribe_observer`]
//!   is the same stream for a consumer that reads but answers no
//!   prompt, and [`Workspace::subscribe_mirror`] is that stream without
//!   the pre-attach backlog, which is what a view takes.
//!
//! No callback hooks. No shared mutable state. A view does not hold an
//! `Arc<AgentHandle>`: every outbound call goes through
//! `Workspace::dispatch(Command)`; query-style refreshes
//! (`refresh_context_usage`, `refresh_mcp_snapshot`)
//! and direct accessors (`settings_documents`,
//! `project_memory_path`, `config_dir_for`) live as
//! inherent methods on [`Workspace`]. The handle stays on the
//! workspace's [`DomainSession`].
//!
//! ## Facade scope (intentionally thin)
//!
//! The MVVM boundary between a view and forge-agent is enforced at
//! the **dependency graph** level: no view depends on `forge-agent`;
//! everything routes through forge-workspace. The
//! workspace exposes forge-agent's submodules verbatim via
//! pass-through `pub use` (see `cloud`, `commands`, `env::git_diff`,
//! `session_lifecycle`, `tooling`, `translate`, `userdata` below).
//! Types like `forge_workspace::cloud::oauth_credentials::OauthCredentials`
//! are *defined* in
//! forge-agent - the workspace just exposes them under the workspace
//! name so a view can keep its dep graph clean.

mod account_cache;
mod account_loader;
pub mod bench;
pub mod board;
pub mod browser;
pub mod catalogue;
pub mod chase;
mod config;
pub mod conversation_turns;
pub mod conversation_window;
mod crons;
mod dictate;
pub mod documents;
mod domain_session;
mod error;
pub mod file_index;
mod gotify;
pub mod install;
pub mod issues;
pub mod launch_settings;
pub(crate) mod mcp;
pub mod output_tail;
mod parked;
mod plugins;
pub mod prompt;
pub mod protocol;
mod provider_probe;
mod review;
mod session_task;
mod single_instance;
mod slack;
mod spawn;
pub mod store;
pub mod subagent_cards;
mod systemone;
mod target;
mod tasks;
mod update_fanout;
mod views;
pub mod work;
mod workspace;

pub use dictate::{
    DictateAxes, DictateBind, DictateDeviceCatalog, DictateDeviceChoice, DictateFailure,
    DictateMode, DictateModel, DictateModelState, DictateOverrideUpdate, DictateOverrides,
    DictateRole, DictateSettings, DictateSnapshot,
};
pub use forge_gateway::{LoadingState, Unusable, UsageFetchStatus};
// The normalizer's prompt axes reach a view only through this
// re-export: no view depends on a forge-dictate crate of its own.
pub use dictate::resolve_capture_device;
pub use domain_session::{ContextUsage, DomainSession, McpServers};
pub use error::WorkspaceError;
pub use forge_dictate::Device;
pub use forge_dictate::normalize::{Context, Structure, Styling};
pub use protocol::{
    Command, DictateOutcome, DispatchError, NoticeSeverity, PromptOrigin, SessionUpdate,
    TurnErrorClass,
};
pub use target::{ProjectKey, SessionSlot, SessionTarget};
pub use views::{AccountLoadingRow, GatewayOrgView, ProjectView};
pub use workspace::{
    PendingInteractionKind, PricingFetch, PricingFetcher, SessionChipInfo, SessionChipState,
    Workspace,
};

// MCP (agents / peers / workers) public surface. The `mcp` module
// itself is crate-private now; these flat re-exports expose only the
// types production consumers (views) need to read off
// `SessionUpdate` payloads. The `testing`-feature block below adds the
// extra surface the `forge-test-harness` integration tests need (MCP
// server builders, mock facades, the caller-key resolver).
pub use mcp::cron::schedule::next_fire_after;
pub use mcp::gotify::types::GotifyNotification;
pub use mcp::peers::types::{MessageId, WrappedKind, WrappedPrompt};
pub use mcp::workers::types::{LiveWorkerState, WorkerEntry};

// Re-export forge-agent types that public surface returns, so
// callers can write `use forge_workspace::AgentHandle` if they
// prefer.
pub use forge_agent::AgentHandle;
pub use forge_agent::client::SessionLaunchSettings;
pub use forge_agent::has_a_transcript_row;
pub use forge_agent::session_history;
pub use forge_agent::transcript_rows;

// Re-export forge-agent sub-surfaces the server reads, kept as a
// pass-through so its callers reach them through the workspace rather
// than a direct `forge-agent` dep. Most types here are themselves
// re-exports from `forge_primitives`; the modules also surface a
// handful of helper functions / network fetchers that genuinely live
// in `forge-agent`.
pub use config::PluginSettings;
pub mod env {
    pub mod cli_version {
        pub use forge_agent::env::cli_version::*;
    }
    pub mod documents {
        pub use forge_agent::env::documents::*;
    }
    pub mod file_index {
        pub use forge_agent::env::file_index::*;
    }
    pub mod git_diff {
        pub use forge_agent::env::git_diff::*;
    }
    pub mod lsp_path {
        pub use forge_agent::env::lsp_path::*;
    }
    pub mod processes {
        pub use forge_agent::env::processes::*;
    }
    pub mod timezone {
        pub use forge_agent::env::timezone::*;
    }
}
pub mod tooling {
    pub use forge_agent::tooling::*;
}
pub mod translate {
    pub use forge_agent::translate::*;
}
/// Pass-through for the agent's typed tool-input parsers
/// (`AskUserQuestion`, `Monitor`), which the server reads when folding
/// a tool call into the block a view draws.
pub mod user_interaction {
    pub use forge_agent::user_interaction::*;
}
pub mod userdata {
    pub use forge_agent::userdata::*;
}
pub use forge_primitives::background::BackgroundTask;
pub use forge_primitives::permission::PermissionMode;
pub use forge_primitives::runtime::RuntimeSessionState;

// MCP test-harness re-exports. `forge-test-harness` integration tests
// drive the workers MCP server directly against a mock facade; that
// requires the server-builder and the facade trait / mock to be
// visible cross-crate. Gating on `testing` keeps them out of
// production builds.
#[cfg(feature = "testing")]
pub use mcp::agents::build_server as build_agents_server;
#[cfg(feature = "testing")]
pub use mcp::peers::facade::MockWorkspaceFacade;
#[cfg(feature = "testing")]
pub use mcp::peers::types::{PeerLiveness, PeerStatus};
pub use mcp::workers::facade::LEAD_LABEL;
#[cfg(feature = "testing")]
pub use mcp::workers::facade::{CallerProject, MockWorkerFacade, WorkerFacade};
