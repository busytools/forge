//! Session state — filesystem-backed scanners + mutations over the
//! `claude` binary's on-disk JSONL transcripts.
//!
//! Each submodule has a single responsibility:
//!
//! - [`scan`] — offline filesystem scanners (`list_sessions`,
//!   `get_session_info`, `get_session_messages`, `list_subagents`,
//!   `get_subagent_messages`) with a head+tail lite-read so 100 MiB
//!   transcripts cost two 64 KiB reads rather than a full scan.
//! - [`mutations`] — in-place mutations (`rename_session`,
//!   `tag_session`, `delete_session`, `fork_session`) operating on
//!   local JSONL transcripts.
//! - [`paths`] — `$CLAUDE_CONFIG_DIR`-aware path resolution shared
//!   across the other submodules and `client` accessors.
//!
//! See `docs/cuts/transcript-mirror.md` for the 2026-04-23 removal of
//! the `SessionStore` trait + `transcript_mirror` pipeline and the
//! bring-back recipe.

pub mod mutations;
pub(crate) mod paths;
pub mod scan;
