//! Session catalog — filesystem-backed index of session transcripts
//! the `claude` CLI persists under
//! `<config_dir>/projects/<project_key>/`.
//!
//! - [`scan`] — offline scanners (list, get, head+tail lite metadata).
//! - [`mutations`] — in-place mutations (rename, tag, delete, fork).
//!
//! Lifted from forge-sdk's `session::{scan, mutations}` (2026-05-05).
//! Filesystem reads belong with the agent — the SDK now exposes only
//! `claude_config_dir` + `projects_dir` for path resolution and the
//! shared `Error` type.

pub mod mutations;
pub mod scan;
