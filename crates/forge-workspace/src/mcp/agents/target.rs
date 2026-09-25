//! The address an `agents__*` call names.

use crate::mcp::peers::types::PeerStatus;

/// The reserved spelling for a project's own agent - the same one the
/// worker and cron paths already resolve a slot with.
pub use forge_primitives::LEAD_LABEL;

/// A seat named the way forge names one: the `(org, project, label)`
/// slot, with `lead` reserved for a project's own agent.
///
/// Only [`AgentTarget::parse`] builds one, so a target that reaches a
/// tool always names a project configured under the org it claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentTarget {
    org: String,
    project: String,
    label: String,
}

/// Why a caller-supplied target did not resolve to a seat. The two are
/// separate variants because they call for different corrections: a
/// malformed target needs a field filled in, an unknown one needs the
/// project name checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TargetError {
    /// A component the caller supplied is empty or whitespace.
    Malformed { component: &'static str },
    /// No project by that name is configured under that org.
    UnknownProject { org: String, project: String },
}

impl AgentTarget {
    /// Resolve a caller-supplied target against the projects forge is
    /// configured with. `known` is the same snapshot `list` returns, so
    /// anything that resolves here is also something the caller can see.
    ///
    /// Components are matched verbatim, never trimmed: a target that
    /// nearly names a seat has to refuse, since resolving it would send
    /// the message somewhere the caller did not ask for.
    pub fn parse(
        known: &[PeerStatus],
        org: &str,
        project: &str,
        label: Option<&str>,
    ) -> Result<Self, TargetError> {
        for (component, value) in [("org", org), ("project", project)] {
            if value.trim().is_empty() {
                return Err(TargetError::Malformed { component });
            }
        }
        let label = match label {
            None => LEAD_LABEL,
            Some(label) if label.trim().is_empty() => {
                return Err(TargetError::Malformed { component: "label" });
            }
            Some(label) => label,
        };
        if !known.iter().any(|p| p.org == org && p.name == project) {
            return Err(TargetError::UnknownProject {
                org: org.to_owned(),
                project: project.to_owned(),
            });
        }
        Ok(Self { org: org.to_owned(), project: project.to_owned(), label: label.to_owned() })
    }

    /// Org the target's project belongs to.
    pub fn org(&self) -> &str {
        &self.org
    }

    /// Project whose seat the target names.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// Label naming the seat within the project; `lead` for the project
    /// agent itself.
    pub fn label(&self) -> &str {
        &self.label
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::peers::types::{PeerLiveness, PeerStatus};
    use std::path::PathBuf;

    fn configured(org: &str, name: &str) -> PeerStatus {
        PeerStatus {
            name: name.to_owned(),
            org: org.to_owned(),
            path: PathBuf::from(format!("/tmp/{name}")),
            status: PeerLiveness::Sleeping,
            in_flight_incoming: 0,
            in_flight_outgoing: 0,
            spawned_at: None,
        }
    }

    /// Two projects whose names do not overlap, so a target that resolves
    /// by name alone is distinguishable from one that needs its org.
    fn configured_projects() -> Vec<PeerStatus> {
        vec![configured("acme", "core"), configured("other", "proj")]
    }

    #[test]
    fn a_target_without_a_label_is_the_projects_agent() {
        let target = AgentTarget::parse(&configured_projects(), "acme", "core", None)
            .expect("core is configured under acme");
        assert_eq!(target.label(), LEAD_LABEL);
    }

    #[test]
    fn a_project_the_org_does_not_own_is_refused() {
        // `proj` exists, but under `other`. Matching on the name alone would
        // send to the wrong seat, which is worse than refusing. The VARIANT
        // is the assertion, not just a refusal: reporting a malformed
        // component would tell the caller its filled-in project field is
        // empty, and hide the roster it needs to correct itself.
        let projects = configured_projects();
        assert!(matches!(
            AgentTarget::parse(&projects, "acme", "proj", Some("w")),
            Err(TargetError::UnknownProject { .. })
        ));
        assert!(matches!(
            AgentTarget::parse(&projects, "acme", "absent", Some("w")),
            Err(TargetError::UnknownProject { .. })
        ));
    }

    #[test]
    fn a_target_missing_a_component_is_malformed() {
        let projects = configured_projects();
        assert!(matches!(
            AgentTarget::parse(&projects, "", "core", None),
            Err(TargetError::Malformed { .. })
        ));
        assert!(matches!(
            AgentTarget::parse(&projects, "acme", "core", Some("")),
            Err(TargetError::Malformed { .. })
        ));
    }
}
