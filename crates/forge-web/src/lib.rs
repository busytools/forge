//! The web view: forge's core served as HTML over HTTP.
//!
//! A second view beside the TUI, in the same process, so the cron
//! scheduler and the connectors start once rather than twice. It never
//! names `forge-workspace`: a read of the core goes through
//! `forge-sessions`.

mod server;

pub use server::{WebError, start};
