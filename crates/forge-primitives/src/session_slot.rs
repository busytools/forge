//! `SessionSlot` - the `(org, project, label)` triple that names a
//! session, and the routing key the TUI <-> workspace boundary
//! addresses one session task by.
//!
//! The triple names the slot; the session id names the occupant. A
//! slot is stable for the life of a project's lead session or a
//! worker's: `/new`, `/resume` and a restart swap the occupant and
//! leave the slot alone.
//!
//! Lives in forge-primitives so the same routing key flows through
//! every layer (TUI, workspace, agent) without each crate growing its
//! own near-identical newtype.

use crate::workers::LEAD_LABEL;

/// The `(org, project, label)` triple naming one session. `org` and
/// `project` are the `forge.toml` names - the same pair the redb
/// `sessions` table is keyed by - and `label` carries the role:
/// [`LEAD_LABEL`] for a project's lead, the worker's own label
/// otherwise.
#[derive(Clone, Debug, Hash, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SessionSlot {
    org: String,
    project: String,
    label: String,
}

impl SessionSlot {
    pub fn new(
        org: impl Into<String>,
        project: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self { org: org.into(), project: project.into(), label: label.into() }
    }

    /// A project's lead slot.
    pub fn lead(org: impl Into<String>, project: impl Into<String>) -> Self {
        Self::new(org, project, LEAD_LABEL)
    }

    /// A worker's slot: its project plus the label that names it.
    pub fn worker(
        org: impl Into<String>,
        project: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self::new(org, project, label)
    }

    /// A slot from a label that may be absent, `None` meaning the
    /// project's lead. The shape the cron and Gotify routers hold,
    /// where a durable entry's `team_role` names a worker or nothing.
    pub fn for_label(org: &str, project: &str, label: Option<&str>) -> Self {
        match label {
            Some(label) => Self::new(org, project, label),
            None => Self::lead(org, project),
        }
    }

    pub fn org(&self) -> &str {
        &self.org
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn is_lead(&self) -> bool {
        self.label == LEAD_LABEL
    }

    /// The slot's display string, `org/project/label`. For tracing and
    /// for the few surfaces that need one string to render; it is not
    /// a key and nothing parses it back.
    pub fn display(&self) -> String {
        format!("{}/{}/{}", self.org, self.project, self.label)
    }

    /// Test-only constructor for fixtures across crate boundaries
    /// (forge-tui's integration tests), where a fixture wants a
    /// distinct slot from one opaque name. Gated behind the
    /// `test-helpers` Cargo feature so production builds don't carry
    /// the helper.
    #[cfg(feature = "test-helpers")]
    pub fn from_str_for_test(s: impl Into<String>) -> Self {
        Self::worker("TestOrg", "test-project", s)
    }
}

impl std::fmt::Display for SessionSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}
