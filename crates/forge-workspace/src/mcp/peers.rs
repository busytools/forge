//! Peer-coordination tools (#114 v1).
//!
//! Four tools the LLM in any session can call to communicate with
//! other forge agents (= projects from forge.toml):
//!
//! - `peers__ask_agent` — async question to another agent. Returns a
//!   correlation_id; reply lands as a new user-turn injection in the
//!   caller's chat once the recipient's `tell_agent { in_reply_to }`
//!   fires.
//! - `peers__tell_agent` — fire-and-forget message OR reply (when
//!   `in_reply_to` is set).
//! - `peers__list_agents` — snapshot of every configured project's
//!   peer status (running / sleeping / failed + in-flight counters).
//! - `peers__whoami` — caller's own identity (project name, org,
//!   model, permission mode).
//!
//! All four tools take a closure-bound [`SessionKey`] identifying the
//! caller, plus an [`Arc<dyn WorkspaceFacade>`] for the workspace
//! state surface. The factory `build_server` (lands in C5) bakes the
//! caller key into each tool's closure when the per-session MCP
//! server is constructed.
//!
//! C4 ships the facade trait + impl + mock; the Tool impls and
//! `build_server` follow in C5-C8.
//!
//! [`SessionKey`]: crate::SessionKey
//! [`Arc<dyn WorkspaceFacade>`]: facade::WorkspaceFacade

pub mod facade;
