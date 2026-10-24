//! The `[client]` section of `forge.toml`.

use serde::Deserialize;

/// The `[client]` block of `forge.toml`: which of the sets forge ships a
/// client draws with. The server carries these to a client on connect,
/// so a client never reads `forge.toml` itself.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClientConfig {
    /// The mark, by name from [`MARK_NAMES`]. `None` is the built-in,
    /// never a pinned value: a commented line in a hand-authored
    /// `forge.toml` has to mean "unset".
    pub mark: Option<String>,
    /// The palette, by name from [`THEME_NAMES`].
    pub theme: Option<String>,
    /// The typefaces, by name from [`FONT_NAMES`]. `None` is the built-in
    /// face - Fira Code for prose and code alike - shipped beside the view
    /// rather than left to whatever the OS has.
    pub font: Option<String>,
    /// Ghosts of the idle pair `[browser]` owns now: read only so a config
    /// written before the move still boots, never carried to a client, and
    /// safe to delete.
    pub browser_idle_minutes: Option<u64>,
    /// See [`ClientConfig::browser_idle_minutes`].
    pub browser_grace_minutes: Option<u64>,
}

/// The marks forge ships, by the name `[client] mark` takes - a bounded set
/// rather than a path, so every option is one forge has drawn.
pub const MARK_NAMES: &[&str] = &[
    "panes",
    "klin",
    "lanes",
    "f_slab",
    "split",
    "spine",
    "grid",
    "clamp",
    "strike",
    "nest",
    "chamfer",
    "tally",
    "stencil_f",
];

/// The palettes forge ships, by the name `[client] theme` takes.
pub const THEME_NAMES: &[&str] = &["dark"];

/// The typeface sets forge ships, by the name `[client] font` takes. The
/// built-in face has no name of its own: an unset key draws it, and the
/// one name here is the opt-out to the stacks the OS already has.
pub const FONT_NAMES: &[&str] = &["system"];
