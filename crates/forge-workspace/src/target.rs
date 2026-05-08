//! Identifiers + the `SessionTarget` enum used to address sessions.

/// Newtype around the SDK's session id (a UUID string).
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct SessionKey(pub(crate) String);

impl SessionKey {
    pub(crate) fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Borrow the inner id as a `&str`.
    #[must_use]
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

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// What [`crate::Workspace::get_agent_handle`] should hand back.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum SessionTarget {
    /// The lead of the project marked `default = true` in
    /// `forge.toml`. Errors if no default is configured.
    Default,
    /// A specific session by id. forge-tui doesn't construct this
    /// in 1a (no resume CLI); it exists for the dual-session test
    /// suite and Phase 2's click-to-switch flow.
    Session(SessionKey),
}
