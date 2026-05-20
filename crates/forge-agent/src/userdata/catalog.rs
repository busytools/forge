//! Session catalog — filesystem-backed index of session transcripts
//! the `claude` CLI persists under
//! `<config_dir>/projects/<project_key>/`.
//!
//! - [`scan`] — offline scanners (list, get, head+tail lite metadata).
//! - [`mutations`] — in-place mutations (tag, delete).
//!
//! Every catalog helper takes `config_dir: &Path` from the caller;
//! there is no fallback to a process-env-derived path.

pub mod mutations;
pub mod scan;
