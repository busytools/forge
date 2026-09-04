//! Full-screen diff overlay state + key / mouse dispatch.
//!
//! The overlay is the floor of the `/diff` flow: a snapshot of
//! file-level hunks fetched via
//! [`forge_workspace::env::git_diff::hunks::scan`] rendered as a
//! single continuous scroll of every changed file with a FILES jump
//! rail. See [`crate::ui::diff_overlay`] for the renderer.
//!
//! One submodule per concern:
//! - [`types`]: the data shapes shared across the overlay (render
//!   coordinate keys, comment cards, scan events).
//! - [`layout`]: layout math and pure hunk transforms, shared with
//!   the renderer.
//! - [`state`]: [`DiffOverlayState`], the transient state the view
//!   renders from.
//! - [`lifecycle`]: target resolution, spawned scans, the event
//!   drain pump, and the overlay's install / drop on `App`.
//! - [`threads`]: persisted review threads - hydration, re-anchoring,
//!   resolve / reopen actions.
//! - [`comments`]: the comment editor's close / save path and the
//!   durable thread it writes.
//! - [`reviews`]: the `l` REVIEWS list, the Finish-review modal, and
//!   the seal-and-nudge close path.
//! - [`keys`] / [`mouse`]: the key and mouse dispatch.

pub(crate) mod comments;
pub(crate) mod keys;
pub(crate) mod layout;
pub(crate) mod lifecycle;
pub(crate) mod mouse;
pub(crate) mod reviews;
pub(crate) mod state;
pub(crate) mod threads;
pub(crate) mod types;

pub use layout::FileHighlight;
pub use lifecycle::{drain_events, open_default, open_with_target};
pub use state::DiffOverlayState;
pub use types::{
    ActiveCommentInput, AnchorNote, BodyRowKey, CommentRef, DiffOverlayEvent, DiffScope,
    DiffViewMode, HunkComment, LineKey, RailRowKey,
};

pub(crate) use keys::{handle_key, handle_paste};
pub(crate) use layout::{
    SPLIT_MARKER_COLS, effective_view_mode, gutter_width_for, rail_width_for, split_layout,
};
pub(crate) use mouse::handle_mouse;
pub(crate) use reviews::would_file;

#[cfg(test)]
pub(crate) mod test_support;
