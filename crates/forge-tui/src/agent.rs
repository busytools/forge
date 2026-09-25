//! TUI-side modules that name the agent boundary but don't shim
//! it. The previous incarnation of this file held re-export shims
//! mirroring `forge_workspace::*` paths; those have been deleted and
//! callers now import from `forge_workspace::*` directly.
//!
//! - [`model`] - UI-typed model describing agent state for the view
//!   layer to render.

/// The agent model moved to `forge-sessions`. Re-exported as a module so
/// every `crate::agent::model::…` path in the view keeps resolving.
pub use forge_sessions::model::agent as model;
