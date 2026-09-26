//! The web view: an HTTP view beside the TUI, in the same process, so the
//! cron scheduler and the connectors start once rather than twice.
//!
//! It serves the home page and nothing else so far. It never names
//! `forge-workspace`: reads of the core go through `forge-sessions`, and
//! reads of a working tree through `forge-agent`'s git plumbing.

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
