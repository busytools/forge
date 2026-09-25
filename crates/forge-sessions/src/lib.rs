//! What a view needs and nothing about how it renders.
//!
//! Holds the inbound peer envelope parsing today; the session records as a
//! view sees them, and the reducers that derive them, land here as they
//! leave the TUI.

pub mod envelope;
