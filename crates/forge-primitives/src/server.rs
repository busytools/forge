//! The `[server]` section of `forge.toml`.

use std::net::{IpAddr, Ipv4Addr};

use serde::Deserialize;

/// The `[server]` block of `forge.toml`: whether the socket's listener
/// starts with forge, where it binds, and how large a served page's
/// structured tool record may be.
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
    /// How large a frame's `tool_use_result` may be in anything a client is
    /// served, in KB (#2374).
    ///
    /// **A record over it is reduced first**, keeping only the keys a client
    /// draws (a mutation's patch and outside-edit mark, a question's answers
    /// and notes); it becomes an `omitted` marker only when that reduction is
    /// still over the cap. Every path to a client is bounded - a page, a
    /// `more`, the live stream and both repairs - while the transcript keeps
    /// the record whole and the visible tool-result block draws as always.
    /// `0` serves the records whole, for a reader whose changes carry patches
    /// beyond the default.
    pub tool_record_cap_kb: u64,
}

/// The port the server binds when `[server] port` is absent. Distinct
/// from the gateway's 8787, the other listener a boot brings up.
pub const DEFAULT_SERVER_PORT: u16 = 8790;

/// The served `tool_use_result` cap when `[server] tool_record_cap_kb` is
/// absent. See `ServerConfig::tool_record_cap_kb`.
pub const DEFAULT_TOOL_RECORD_CAP_KB: u64 = 64;

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: DEFAULT_SERVER_PORT,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            tool_record_cap_kb: DEFAULT_TOOL_RECORD_CAP_KB,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The shipped default, and the opt-out: absent means 64 KB, and `0`
    /// means the records cross whole. **The default is pinned as a literal**,
    /// because the book documents 64 and comparing it to the constant would
    /// move with it.
    #[test]
    fn the_served_record_cap_defaults_to_64_kb_and_zero_means_whole() {
        assert_eq!(ServerConfig::default().tool_record_cap_kb, 64, "the documented default");
        assert_eq!(DEFAULT_TOOL_RECORD_CAP_KB, 64, "and the constant the default is built from");
        let parsed: ServerConfig =
            serde_json::from_str(r#"{"tool_record_cap_kb": 0}"#).expect("the key parses");
        assert_eq!(parsed.tool_record_cap_kb, 0);
    }
}
