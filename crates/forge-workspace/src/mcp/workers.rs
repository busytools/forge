//! Workers MCP - project-internal child-agent coordination. Mirror
//! of `crate::mcp::peers`, scoped to within-project addressing by
//! label rather than cross-project addressing by project name.
//!
//! See `docs/superpowers/specs/2026-05-21-workers-mcp-design.md`.

pub mod facade;
pub mod types;

pub use types::WorkerEntry;
