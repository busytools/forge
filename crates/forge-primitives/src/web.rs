//! The web view's `[web]` config.

use std::net::{IpAddr, Ipv4Addr};

use serde::Deserialize;

/// The `[web]` block of `forge.toml`: whether the HTTP server starts
/// with forge, where it listens, and which of the sets forge ships it
/// draws with.
///
/// On unless it is turned off, so a restart leaves the view serving
/// without a key being added first.
#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub enabled: bool,
    pub port: u16,
    /// Loopback by default: an address on a network interface is a
    /// deliberate line in `forge.toml`, never something on-by-default
    /// reaches.
    pub bind: IpAddr,
    /// The mark, by name from [`MARK_NAMES`]. `None` is the built-in,
    /// never a pinned value: a commented line in a hand-authored
    /// `forge.toml` has to mean "unset".
    pub mark: Option<String>,
    /// The palette, by name from [`THEME_NAMES`].
    pub theme: Option<String>,
}

/// The port the web view binds when `[web] port` is absent. Distinct
/// from the gateway's 8787, the other listener a boot brings up.
pub const DEFAULT_WEB_PORT: u16 = 8790;

/// The marks forge ships, by the name `[web] mark` takes - a bounded set
/// rather than a path, so every option is one forge has drawn.
pub const MARK_NAMES: &[&str] = &[
    "klin",
    "lanes",
    "f_slab",
    "split",
    "spine",
    "slab",
    "grid",
    "clamp",
    "strike",
    "cascade",
    "nest",
    "chamfer",
    "tally",
    "stencil_f",
    "anvil",
    "spark",
];

/// The palettes forge ships, by the name `[web] theme` takes.
pub const THEME_NAMES: &[&str] = &["dark"];

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: DEFAULT_WEB_PORT,
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            mark: None,
            theme: None,
        }
    }
}
