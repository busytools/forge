//! Engineering team feature - per-project pre-configured worker teams.
//!
//! Roles live as file-driven labels at
//! `~/.claude/forge-team/<label>/{charter,kick}.md`.

pub mod roles;

pub use roles::{
    CharterError, DEFAULT_LEAD_CHARTER, Role, forge_team_root, load_charter, load_initial_kick,
    load_lead_charter_or_default, load_resume_kick, role_dir, validate_label,
};

#[cfg(any(test, feature = "testing"))]
pub use roles::{
    ForgeTeamRootTestGuard, override_forge_team_root_for_test, set_forge_team_root_for_test,
};
