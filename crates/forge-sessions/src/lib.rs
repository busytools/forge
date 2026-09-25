//! What a view needs and nothing about how it renders.
//!
//! Holds the peer envelope parsing and the outbound peer calls, the tool
//! family table, the session records a view renders, and the policy that
//! folds a run of blocks. The reducers that derive those records are still
//! in the TUI.

pub mod envelope;
pub mod family;
pub mod grouping;
pub mod model;
pub mod peer_outbound;
