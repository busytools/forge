//! The web view's `[web]` config.

use std::net::{IpAddr, Ipv4Addr};

use serde::Deserialize;

/// The `[web]` block of `forge.toml`: whether the HTTP server starts
/// with forge, and where it listens.
///
/// On unless it is turned off, so a restart leaves the view serving
/// without a key being added first.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub enabled: bool,
    pub port: u16,
    /// Loopback by default: an address on a network interface is a
    /// deliberate line in `forge.toml`, never something on-by-default
    /// reaches.
    pub bind: IpAddr,
}

/// The port the web view binds when `[web] port` is absent. Distinct
/// from the gateway's 8787, the other listener a boot brings up.
pub const DEFAULT_WEB_PORT: u16 = 8790;

impl Default for WebConfig {
    fn default() -> Self {
        Self { enabled: true, port: DEFAULT_WEB_PORT, bind: IpAddr::V4(Ipv4Addr::LOCALHOST) }
    }
}
