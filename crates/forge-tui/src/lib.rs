pub mod agent;
pub mod app;
pub mod ui;

// The build's stamp lives in `forge-server` and is re-exported here: it says
// which forge commit is running, which is a fact about the build rather than
// about the view drawing it - and the client reads the same stamp off its
// wire. Re-exported rather than repeated, because two stamps is the
// disagreement the move exists to prevent.
pub use forge_server::{
    FORGE_BUILD_PROVENANCE, FORGE_CARGO_LOCK_DIGEST, FORGE_VERSION, FORGE_VERSION_SHORT,
};

// The argv, the logging runtime, the FD bump and the app error type moved
// to forge-server with the boot both binaries share; re-exported so every
// `crate::logging::...` path and `forge_tui::Cli` keeps resolving while the
// terminal lives.
pub use forge_server::cli::{Cli, DiagnosticsPreset};
pub use forge_server::{error, logging, startup};
