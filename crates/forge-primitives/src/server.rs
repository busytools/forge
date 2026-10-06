//! The `[server]` section of `forge.toml`.

use std::net::{IpAddr, Ipv4Addr};

use serde::Deserialize;

/// The `[server]` block of `forge.toml`: whether the socket's listener
/// starts with forge and where it binds.
///
/// On unless it is turned off, so a restart leaves the socket serving
/// without a key being added first.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    pub enabled: bool,
    pub port: u16,
    /// Loopback by default: an address on a network interface is a
    /// deliberate line in `forge.toml`, never something on-by-default
    /// reaches.
    pub bind: IpAddr,
}

/// The port the server binds when `[server] port` is absent. Distinct
/// from the gateway's 8787, the other listener a boot brings up.
pub const DEFAULT_SERVER_PORT: u16 = 8790;

impl Default for ServerConfig {
    fn default() -> Self {
        Self { enabled: true, port: DEFAULT_SERVER_PORT, bind: IpAddr::V4(Ipv4Addr::LOCALHOST) }
    }
}
