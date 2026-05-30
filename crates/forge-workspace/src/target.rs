//! Identifiers + the `SessionTarget` enum used to address sessions.

// `SessionKey` lives in forge-primitives so the same routing key
// flows through the TUI → workspace → agent layers without each crate
// growing its own near-identical newtype. Re-exported here so call
// sites continue to import via `forge_workspace::SessionKey`.
pub use forge_primitives::SessionKey;

/// Project root path key  -  the canonicalised, sanitised string form
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
    /// Spawn a FRESH session in the project identified by `project_key`,
    /// bypassing the lead-resume path. Used by the workers MCP so a
    /// worker is always a brand-new session, not a resume of the
    /// project's existing lead. The pool key is the caller-supplied
    /// synthetic spawn key (the same one passed via `spawn_key`); the
    /// SessionTask rekeys onto the real claude-issued UUID on its
    /// first `Connected`.
    FreshInProject { project_key: ProjectKey, synth_key: SessionKey },
}
