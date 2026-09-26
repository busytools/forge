//! The web view: an HTTP view beside the TUI, in the same process, so the
//! cron scheduler and the connectors start once rather than twice.
//!
//! It serves the home page and nothing else so far. Everything it reads
//! comes through `forge-sessions`, which re-exports the git plumbing and
//! the wire parsers a view needs: it names no crate under that one.

pub mod brand;
mod home;
mod server;
mod stream;
pub mod theme;
mod unseen;
mod work;

pub use server::{WebError, WebState, start};
pub use unseen::Unseen;
pub use work::{WorkCache, WorkState};
