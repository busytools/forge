//! Identifiers + the `SessionTarget` enum used to address sessions.

// `SessionKey` lives in forge-primitives so the same routing key
// flows through the TUI → workspace → agent layers without each crate
// growing its own near-identical newtype. Re-exported here so call
// sites continue to import via `forge_workspace::SessionKey`.
pub use forge_primitives::SessionKey;

/// Project root path key — the canonicalised, sanitised string form
/// produced by
/// [`forge_agent::userdata::catalog::scan::project_key_for_directory`].
/// Equivalent to the directory names you see under
/// `<config_dir>/projects/`.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct ProjectKey(pub(crate) String);

impl ProjectKey {
    pub(crate) fn new(key: impl Into<String>) -> Self {
        Self(key.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Test-only constructor for cross-crate fixtures (forge-tui's
    /// Projects pane snapshot tests). Behind the `test-helpers`
    /// Cargo feature so the production constructor stays
    /// crate-private.
    #[cfg(feature = "test-helpers")]
    pub fn new_for_test(key: impl Into<String>) -> Self {
        Self(key.into())
    }
}

/// What [`crate::Workspace::get_agent_handle`] should hand back.
#[derive(Clone, Debug)]
pub enum SessionTarget {
    /// The lead of the project marked `default = true` in
    /// `forge.toml`. Errors if no default is configured.
    Default,
    /// Open the lead of the project whose `name` matches the given
    /// string in `forge.toml`. Errors with `ProjectNotFound` if no
    /// such name exists.
    Named(String),
    /// A specific session by id. Used by the click-to-resume flow
    /// in the Projects pane and by `Workspace::spawn_session`.
    Session(SessionKey),
}

