//! Shared path resolution for the `claude` CLI's on-disk state, plus
//! typed accessors for files inside `<config_dir>` that consumers need
//! a structured view of (currently OAuth credentials).
//!
//! Every accessor that reads a file under the user's config directory
//! goes through `claude_config_dir()` so `$CLAUDE_CONFIG_DIR` is
//! honoured in exactly one place. Empty-string env values are treated
//! as unset to match the CLI's own behaviour.

use std::path::PathBuf;

/// Resolve the Claude config directory. Honours `$CLAUDE_CONFIG_DIR`
/// (ignoring empty-string values), else falls back to
/// `$HOME/.claude`. Shared across `client`, the agent-side session
/// catalog, and any accessor that needs a typed view of an on-disk
/// CLI artefact.
#[must_use]
pub fn claude_config_dir() -> PathBuf {
    let custom = std::env::var("CLAUDE_CONFIG_DIR").ok();
    let home = std::env::var("HOME").ok();
    claude_config_dir_from(custom.as_deref(), home.as_deref())
}

/// Pure variant of [`claude_config_dir`] that takes `CLAUDE_CONFIG_DIR`
/// and `HOME` as arguments instead of reading the process environment.
/// Used internally so the env-resolution branches are unit-testable
/// without mutating shared process state during parallel test runs.
fn claude_config_dir_from(custom: Option<&str>, home: Option<&str>) -> PathBuf {
    if let Some(value) = custom {
        let trimmed = value.trim_end_matches('/');
        if !trimmed.is_empty() {
            return PathBuf::from(trimmed);
        }
    }
    PathBuf::from(home.unwrap_or(".")).join(".claude")
}

/// Resolve the Claude projects directory. Honours `$CLAUDE_CONFIG_DIR`
/// (ignoring empty-string values), else falls back to
/// `$HOME/.claude/projects`. Public so the agent's session-catalog
/// readers (lifted out of forge-sdk in 2026-05-05) can resolve the
/// same on-disk layout.
#[must_use]
pub fn projects_dir() -> PathBuf {
    claude_config_dir().join("projects")
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn config_dir_honours_claude_config_dir_when_set() {
        let resolved = claude_config_dir_from(Some("/tmp/custom-config"), Some("/home/ignored"));
        assert_eq!(resolved, PathBuf::from("/tmp/custom-config"));
    }

    #[test]
    fn config_dir_strips_trailing_slash_from_claude_config_dir() {
        let resolved = claude_config_dir_from(Some("/tmp/custom/"), Some("/home/ignored"));
        assert_eq!(resolved, PathBuf::from("/tmp/custom"));
    }

    #[test]
    fn config_dir_falls_back_to_home_when_claude_config_dir_empty() {
        let resolved = claude_config_dir_from(Some(""), Some("/home/me"));
        assert_eq!(resolved, PathBuf::from("/home/me/.claude"));
    }

    #[test]
    fn config_dir_falls_back_to_home_when_claude_config_dir_unset() {
        let resolved = claude_config_dir_from(None, Some("/home/me"));
        assert_eq!(resolved, PathBuf::from("/home/me/.claude"));
    }

    #[test]
    fn config_dir_falls_back_to_dot_when_home_unset() {
        let resolved = claude_config_dir_from(None, None);
        assert_eq!(resolved, PathBuf::from("./.claude"));
    }
}
