//! The web view: an HTTP view beside the TUI, in the same process, so the
//! cron scheduler and the connectors start once rather than twice.
//!
//! Nothing here reads the core yet - the served page is a wiring proof.
//! It never names `forge-workspace`: reads of the core will go through
//! `forge-sessions`.

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
