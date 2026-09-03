//! TUI-side modules that name the agent boundary but don't shim
//! it. The previous incarnation of this file held re-export shims
//! mirroring `forge_workspace::*` paths; those have been deleted and
//! callers now import from `forge_workspace::*` directly.
//!
//! - [`model`] - UI-typed model describing agent state for the view
//!   layer to render.

pub mod model;
