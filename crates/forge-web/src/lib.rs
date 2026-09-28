//! The web view: an HTTP view beside the TUI, in the same process, so the
//! cron scheduler and the connectors start once rather than twice.
//!
//! It serves the home and a session, both over `forge-server`, which
//! re-exports the git plumbing and the wire parsers a view needs: it names
//! no crate under that one.

pub mod brand;
mod composer;
mod home;
mod icons;
mod server;
mod session;
mod stream;
pub mod theme;

pub use forge_server::unseen::Unseen;
pub use forge_server::work::{WorkCache, WorkState};
pub use server::{WebError, WebState, start};
