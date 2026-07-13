//! Chat-view tree connectors - the single source for the `├─`/`└─`/`│`
//! glyphs used by every chat tree (peer + tool-card leaves, the tool-group
//! summary, the /diff files rail) so they can't drift. Chat-scoped: the
//! projects and inspector panes own their own trees.

/// Non-last child: `├─ `.
pub const BRANCH: &str = "\u{251c}\u{2500} ";
/// Last child / leaf: `└─ `.
pub const LAST: &str = "\u{2514}\u{2500} ";
/// Vertical spine: `│`.
pub const SPINE: &str = "\u{2502}";

/// `└─ ` when `is_last`, else `├─ `.
pub fn connector(is_last: bool) -> &'static str {
    if is_last { LAST } else { BRANCH }
}
