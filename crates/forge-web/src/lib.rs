//! The web view, parked. It served an HTTP view beside the TUI, in the same
//! process; the socket took the port those pages were on, so nothing serves
//! them until a client is built against it. The pages below still compile and
//! their tests still run; nothing reaches them.
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
