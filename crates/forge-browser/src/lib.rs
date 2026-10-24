//! The server-owned browser: one CEF host process per profile.
//!
//! The host is a bundle of its own (`Forge Browser.app`), staged by
//! `scripts/stage-browser-host.sh` beside forge's machine-local state, so
//! the server never links CEF into its own process and nothing here needs
//! the client installed.

#[cfg(unix)]
pub mod channel;
pub mod control;
pub mod frame;
pub mod host_bundle;
pub mod idle;
pub mod input;
pub mod profiles;
pub mod tabs;

#[cfg(target_os = "macos")]
pub mod audio;
#[cfg(target_os = "macos")]
pub mod client_application;
#[cfg(target_os = "macos")]
pub mod dialog;
#[cfg(target_os = "macos")]
pub mod download;
#[cfg(target_os = "macos")]
pub mod driver;
#[cfg(target_os = "macos")]
pub mod engine;
#[cfg(target_os = "macos")]
pub mod host;
#[cfg(target_os = "macos")]
pub mod osr;
#[cfg(target_os = "macos")]
pub mod record;
#[cfg(target_os = "macos")]
pub mod service;
#[cfg(target_os = "macos")]
pub mod spawn;
#[cfg(target_os = "macos")]
pub mod veto;
#[cfg(target_os = "macos")]
pub mod window;
