//! What a view needs and nothing about how it renders.
//!
//! Holds the read surface a view uses, the peer envelope parsing and the
//! outbound peer calls, the tool family table, forge's own slash
//! commands, the transcript fold that turns a conversation's wire
//! messages into the units a view draws, the policy that folds a run of
//! blocks, and the two reads of a pty's own output: its escape
//! sequences, and a Monitor's watched-command tail.
//! The reducers that derive the TUI's session records are still in the
//! TUI.

pub mod ansi;
pub mod commands;
pub mod composer;
pub mod delivery;
pub mod emoji;
pub mod envelope;
pub mod family;
pub mod file_index;
pub mod grouping;
pub mod live;
pub mod model;
pub mod monitor;
pub mod peer_outbound;
pub mod subagents;
pub mod surface;
#[cfg(test)]
mod test_support;
#[cfg(feature = "testing")]
pub mod testing;
pub mod transcript;
pub mod unseen;
pub mod work;

/// The core's update protocol, as a view reads it. It is the same type the
/// same call hands the TUI, so nothing here wraps or renames it.
pub use surface::SessionUpdate;

/// The core's write protocol, re-exported for the same reason the update
/// is: a view acts by dispatching the core's own command rather than a
/// second vocabulary this crate would have to keep in step.
pub use forge_workspace::Command;

/// Why a command did not reach a session, so a view can say which of the
/// three it was rather than only that something failed.
pub use forge_workspace::DispatchError;

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
pub use forge_workspace::env::timezone;
pub use forge_workspace::translate;
