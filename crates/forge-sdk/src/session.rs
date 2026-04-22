//! Session state — storage, scanning, mutations, and store-backed
//! variants collapsed under one module.
//!
//! Each submodule has a single responsibility:
//!
//! - [`store`] — the [`SessionStore`](store::SessionStore) trait +
//!   `FsSessionStore` / `MemorySessionStore` / `SessionKey` /
//!   `SessionStoreEntry` + related types.
//! - [`scan`] — offline filesystem scanners (`list_sessions`,
//!   `get_session_info`, `get_session_messages`, `list_subagents`,
//!   `get_subagent_messages`) plus the Python `_read_session_lite`
//!   head+tail optimisation.
//! - [`mutations`] — in-place mutations (`rename_session`,
//!   `tag_session`, `delete_session`, `fork_session`) operating on
//!   local JSONL transcripts.
//! - [`via_store`] — async `_from_store` / `_via_store` variants of
//!   the scanners and mutations that route through a
//!   [`SessionStore`](store::SessionStore) rather than the local
//!   filesystem.
//!
//! Consumers import from the flat `forge_sdk::*` re-export surface
//! (for storage types) or these paths directly (for the free
//! functions). Collapsed from four top-level files (`sessions.rs`,
//! `sessions_via_store.rs`, `session_store.rs`, `session_mutations.rs`)
//! in 2026-04-22 — the full audit I6 recommendation.

pub mod mutations;
pub mod scan;
pub mod store;
pub mod via_store;
