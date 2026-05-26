//! Engineering team feature - per-project pre-configured worker teams
//! (Planner + Implementer + Reviewer + Debugger + Tester).
//!
//! See `docs/superpowers/specs/2026-05-25-engineering-team-design.md`.

pub mod roles;

pub use roles::{ALL_ROLES, LEAD_CHARTER, Role};
