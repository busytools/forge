//! What a view needs and nothing about how it renders.
//!
//! Holds the read surface a view uses, the peer envelope parsing and the
//! outbound peer calls, the tool family table, the session records a view
//! renders, and the policy that folds a run of blocks. The reducers that
//! derive those records are still in the TUI.

pub mod envelope;
pub mod family;
pub mod grouping;
pub mod model;
pub mod peer_outbound;
pub mod surface;
#[cfg(feature = "testing")]
pub mod testing;

/// The core's update protocol, as a view reads it. It is the same type the
/// same call hands the TUI, so nothing here wraps or renames it.
pub use surface::SessionUpdate;

/// The plumbing a view reaches the core's reads through, re-exported so a
/// view depends on this crate and `forge-primitives` and no further.
///
/// The chain under these is longer than the one above them: the workspace
/// re-exports the agent's git plumbing and wire parsers wholesale, so
/// `git_diff::current_branch` and `translate::state_parsing` are agent
/// code reached through two wildcard facades. The view names this crate,
/// which is the discipline that matters here; it is not a claim that the
/// code beneath is the workspace's own.
pub use forge_workspace::env::git_diff;
pub use forge_workspace::translate;
