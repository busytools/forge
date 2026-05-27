//! Engineering team feature - per-project pre-configured worker teams.
//!
//! Roles live as file-driven labels at
//! `~/.claude/forge-team/<label>/{charter,kick}.md`. See
//! `docs/superpowers/specs/2026-05-25-engineering-team-design.md` for
//! the closed-enum predecessor design.

pub mod roles;

pub use roles::{
    CharterError, LEAD_LABEL, Role, forge_team_root, load_charter, load_initial_kick,
    load_resume_kick, role_dir, validate_label,
};

#[cfg(any(test, feature = "testing"))]
pub use roles::set_forge_team_root_for_test;
