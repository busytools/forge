//! Session state — filesystem-backed scanners + mutations over the
//! `claude` binary's on-disk JSONL transcripts.
//!
//! Each submodule has a single responsibility:
//!
//! - [`scan`] — offline filesystem scanners (`list_sessions`,
//!   `get_session_info`, `get_session_messages`, `list_subagents`,
//!   `get_subagent_messages`) plus the Python `_read_session_lite`
//!   head+tail optimisation.
//! - [`mutations`] — in-place mutations (`rename_session`,
//!   `tag_session`, `delete_session`, `fork_session`) operating on
//!   local JSONL transcripts.
//!
//! See `docs/cuts/transcript-mirror.md` for the 2026-04-23 removal of
//! the `SessionStore` trait + `transcript_mirror` pipeline and the
//! bring-back recipe.

pub mod mutations;
pub mod scan;
