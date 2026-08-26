//! Engineering team feature - per-project pre-configured worker teams.
//!
//! Roles live as file-driven labels at
//! `~/.claude/forge-team/<label>/{charter,kick}.md`.

pub mod roles;

pub use roles::{
    CharterError, Role, forge_team_root, load_charter, load_initial_kick, load_resume_kick,
    role_dir, validate_label,
};
