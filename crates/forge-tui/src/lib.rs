// Verbatim lift from claude-code-rust upstream. Upstream's lint config
// allows missing_docs and clippy::must_use_candidate; forge-tui follows
// that stance until the lift settles.
#![allow(missing_docs)]
#![allow(clippy::must_use_candidate)]

pub mod agent;
pub mod app;
pub mod error;
pub mod logging;
