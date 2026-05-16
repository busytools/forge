//! Identifiers + the `SessionTarget` enum used to address sessions.

/// Newtype around the SDK's session id (a UUID string).
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct SessionKey(pub(crate) String);

impl SessionKey {
    pub(crate) fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the inner id as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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
    /// A specific session by id. forge-tui doesn't construct this
    /// in 1a (no resume CLI); it exists for the dual-session test
    /// suite and Phase 2's click-to-switch flow.
    Session(SessionKey),
}

impl SessionKey {
    /// Construct a `SessionKey` from a literal string. Test-only;
    /// `#[doc(hidden)] pub` rather than `#[cfg(test)]` so integration
    /// tests in sibling crates' `tests/` directories can reach it
    /// (Rust's `#[cfg(test)]` items aren't visible across crate
    /// boundaries).
    #[doc(hidden)]
    pub fn from_str_for_test(s: &str) -> Self {
        Self(s.to_owned())
    }

    /// Construct a `SessionKey` from a claude-issued session UUID.
    /// Production-side constructor used by forge-tui's event
    /// multiplexer to tag incoming events with the bound session's
    /// key.
    pub fn from_session_id(id: impl Into<String>) -> Self {
        Self(id.into())
    }
}
