//! Path helpers around the `claude` CLI's on-disk config layout.
//!
//! After the strict-config-dir refactor, forge-sdk no longer falls
//! back to `~/.claude` on its own. Resolution lives in
//! `forge-agent` / `forge-workspace` - those layers read
//! `$CLAUDE_CONFIG_DIR` (typically via [`claude_config_dir_from_env`])
//! at orchestration boundaries and thread the resulting `PathBuf`
//! into every accessor that needs it. forge-sdk only exposes a
//! `<config_dir> + "projects"` join helper for callers that already
//! hold a config_dir.

use std::path::{Path, PathBuf};

/// Read `$CLAUDE_CONFIG_DIR` from the process environment. Returns
/// `None` when the variable is unset, empty, or contains only a
/// trailing slash. Callers handle the `None` case explicitly - there
/// is no silent fallback to `~/.claude` at this layer.
///
/// Trailing slashes are stripped to match the `claude` CLI's own
/// canonicalisation; a value of `/`, `//`, … resolves to `None`
/// because the CLI treats those as effectively unset.
pub fn claude_config_dir_from_env() -> Option<PathBuf> {
    let raw = std::env::var("CLAUDE_CONFIG_DIR").ok()?;
    claude_config_dir_from_env_value(raw.as_str())
}

/// Pure variant of [`claude_config_dir_from_env`] for unit tests.
fn claude_config_dir_from_env_value(value: &str) -> Option<PathBuf> {
    let trimmed = value.trim_end_matches('/');
    if trimmed.is_empty() { None } else { Some(PathBuf::from(trimmed)) }
}

/// Path to a config_dir's `projects/` subdirectory. Caller passes
/// the resolved `config_dir` explicitly; this helper just performs
/// the join so the layout convention lives in one place.
pub fn projects_dir_for(config_dir: &Path) -> PathBuf {
    config_dir.join("projects")
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn env_helper_returns_some_for_non_empty_value() {
        assert_eq!(
            claude_config_dir_from_env_value("/tmp/custom-config"),
            Some(PathBuf::from("/tmp/custom-config")),
        );
    }

    #[test]
    fn env_helper_strips_trailing_slash() {
        assert_eq!(
            claude_config_dir_from_env_value("/tmp/custom/"),
            Some(PathBuf::from("/tmp/custom")),
        );
    }

    #[test]
    fn env_helper_returns_none_for_empty_value() {
        assert_eq!(claude_config_dir_from_env_value(""), None);
    }

    #[test]
    fn env_helper_returns_none_for_only_slashes() {
        assert_eq!(claude_config_dir_from_env_value("///"), None);
    }

    #[test]
    fn projects_dir_for_appends_projects_subdir() {
        assert_eq!(projects_dir_for(Path::new("/tmp/cfg")), PathBuf::from("/tmp/cfg/projects"),);
    }
}
