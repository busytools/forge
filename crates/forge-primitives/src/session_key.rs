//! `SessionKey` — opaque routing key the TUI ↔ workspace boundary
//! uses to address one session task. Newtype around a string so the
//! call site can't confuse it with `session_id` or a project name.
//!
//! Lives in forge-primitives so the same routing key flows through
//! every layer (TUI, workspace, agent) without each crate growing its
//! own near-identical newtype.

/// Newtype around the SDK's session id (a UUID string). Used as the
/// per-session routing key on the workspace's Command / SessionUpdate
/// channels.
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
pub struct SessionKey(String);

impl SessionKey {
    /// Borrow the inner id as a `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Construct a `SessionKey` from a claude-issued session UUID.
    /// Used by the workspace's event multiplexer to tag incoming
    /// events with the bound session's key.
    pub fn from_session_id(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// Test-only constructor for fixtures across crate boundaries
    /// (forge-tui's integration tests). Gated behind the `test-helpers`
    /// Cargo feature so production builds don't carry the helper.
    #[cfg(feature = "test-helpers")]
    pub fn from_str_for_test(s: &str) -> Self {
        Self(s.to_owned())
    }
}

