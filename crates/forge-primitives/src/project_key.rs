//! `ProjectKey` - the canonical on-disk project identity.
//!
//! Lives in forge-primitives so one key addresses a project across the
//! crates that key their own maps on it - the workspace and the TUI -
//! without either growing its own near-identical newtype.

/// Project root path key - the canonicalised, sanitised string form
/// produced by `forge_agent::userdata::catalog::scan::project_key_for_directory`.
/// Equivalent to the directory names you see under
/// `<config_dir>/projects/`.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ProjectKey(pub(crate) String);

impl ProjectKey {
    pub fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Test-only constructor for cross-crate fixtures (forge-tui's
    /// Projects pane snapshot tests). Byte-identical to
    /// [`ProjectKey::new`]; the `test-helpers` gate, not a visibility
    /// difference, is what keeps it out of production builds.
    #[cfg(feature = "test-helpers")]
    pub fn new_for_test(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}
