//! The file index, as this crate's callers reach it.
//!
//! The type, the walk over it and the ranking live in
//! `forge_workspace::file_index`: three views read them - the terminal's
//! mention list, the web view's composer and the seat's own loop, which walks
//! the tree and pushes the index whole - so the crate they all read is where
//! they sit. This module is the path they were reached by.

pub use forge_workspace::file_index::{FileIndex, rank_and_truncate_candidates, respect_gitignore};
