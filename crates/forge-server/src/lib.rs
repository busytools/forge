//! The server: what a client reads of the core, and no view of its own.
//!
//! Holds the view surface - the reads a client makes and the commands it
//! sends -, the peer envelope parsing and the outbound peer calls, the
//! tool family table, forge's own slash commands, the transcript fold that
//! turns a conversation's wire messages into the units a view draws, the
//! policy that folds a run of blocks, a Monitor's watched-command tail, and
//! the socket that carries all of it to whatever is drawing.
//!
//! Nothing here draws. What a glyph, a colour or a weight is belongs to the
//! client, and the test is whether removing a thing changes what the data
//! IS or only how it is DRAWN.
//!
//! The reducers that derive the TUI's session records are still in the
//! TUI.

pub mod commands;
pub mod composer;
pub mod delivery;
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
pub mod transport;
pub mod unseen;
pub mod work;

// The build's own stamp, which states WHICH forge commit is running rather
// than which crate is drawing. It lives here because the crate that renders a
// header is not the crate the header is about: the terminal reads these and
// re-exports them, and a client reads them off the wire - so the stamp has to
// outlive the terminal rather than being compiled into it.

/// Full version string for the welcome banner + status panel.
///
/// Always carries the short SHA so a screenshot is enough to identify the
/// running build. On `main` (and detached HEAD) the stamp adds ` · <sha>`;
/// on any other branch the stamp adds ` · <sha> (<branch>)`.
pub const FORGE_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), env!("FORGE_BUILD_SUFFIX_FULL"));

/// Short version string for tight slots (Projects pane bottom row, launchpad
/// version line). Always carries the short SHA as `+<sha>` so the running
/// build is identifiable from a screenshot.
pub const FORGE_VERSION_SHORT: &str =
    concat!(env!("CARGO_PKG_VERSION"), env!("FORGE_BUILD_SUFFIX_SHORT"));

/// How this build was made - the marker `scripts/install.sh` sets, or empty
/// for anything else. Surfaced at startup by the terminal's
/// `startup::report_build_provenance`.
pub const FORGE_BUILD_PROVENANCE: &str = env!("FORGE_BUILD_PROVENANCE");

/// Digest of the `Cargo.lock` this build saw, not of forge's single-instance
/// lock. FNV-1a, not a SHA, so `shasum` will not reproduce it. Meaningful only
/// compared against the digest a build of the released tag reports.
pub const FORGE_CARGO_LOCK_DIGEST: &str = env!("FORGE_CARGO_LOCK_DIGEST");

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
