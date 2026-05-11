//! `forge.toml` schema + loader.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::WorkspaceError;

#[derive(Debug, Deserialize)]
struct ForgeToml {
    #[serde(default)]
    projects: Vec<ProjectEntry>,
    #[serde(default)]
    accounts: Vec<AccountEntry>,
    #[serde(default)]
    selection: SelectionEntry,
}

#[derive(Debug, Deserialize)]
struct ProjectEntry {
    name: String,
    path: String,
    #[serde(default)]
    default: bool,
}

#[derive(Debug, Deserialize)]
struct AccountEntry {
    display_name: String,
    config_dir: String,
}

#[derive(Debug, Deserialize, Default)]
struct SelectionEntry {
    #[serde(default)]
    policy: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) enum SelectionPolicy {
    #[default]
    LeastRecentlyUsed,
    RoundRobin,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedAccount {
    pub display_name: String,
    pub config_dir: PathBuf,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SelectionConfig {
    pub policy: SelectionPolicy,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedConfig {
    pub projects: Vec<LoadedProject>,
    pub default_index: usize,
    pub accounts: Vec<LoadedAccount>,
    pub selection: SelectionConfig,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedProject {
    pub name: String,
    pub path: PathBuf,
    /// Original path string from `forge.toml`, preserved for display
    /// (e.g. `~/Projects/forge` with `~` un-expanded). Use `path` for
    /// filesystem access; this for human-readable output.
    pub display_path: String,
}

impl LoadedConfig {
    pub(crate) fn default_project(&self) -> &LoadedProject {
        &self.projects[self.default_index]
    }
}

/// Load + validate `<config_dir>/forge.toml`. Returns the parsed
/// projects with `~` expanded and the index of the `default = true`
/// project. Errors per spec §4 (`Errors`).
pub(crate) fn load_from_dir(config_dir: &Path) -> Result<LoadedConfig, WorkspaceError> {
    let path = config_dir.join("forge.toml");

    let raw = match fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(WorkspaceError::ConfigMissing { path });
        }
        Err(e) => {
            return Err(WorkspaceError::ConfigInvalid { path, message: format!("io error: {e}") });
        }
    };

    let parsed: ForgeToml = toml::from_str(&raw)
        .map_err(|source| WorkspaceError::ConfigParse { path: path.clone(), source })?;

    let mut default_index: Option<usize> = None;
    let mut projects = Vec::with_capacity(parsed.projects.len());
    for (i, entry) in parsed.projects.into_iter().enumerate() {
        if entry.default {
            if default_index.is_some() {
                tracing::warn!(
                    target: "forge_workspace::config",
                    "multiple [[projects]] set default = true; first wins, ignoring '{}'",
                    entry.name,
                );
            } else {
                default_index = Some(i);
            }
        }
        projects.push(LoadedProject {
            name: entry.name,
            path: expand_home(&entry.path),
            display_path: entry.path,
        });
    }

    let default_index =
        default_index.ok_or_else(|| WorkspaceError::NoDefaultProject { path: path.clone() })?;

    if parsed.accounts.is_empty() {
        return Err(WorkspaceError::NoAccountsConfigured { path });
    }

    let mut seen_account_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut accounts: Vec<LoadedAccount> = Vec::with_capacity(parsed.accounts.len());
    for entry in parsed.accounts {
        if !seen_account_names.insert(entry.display_name.clone()) {
            return Err(WorkspaceError::DuplicateAccount { path, name: entry.display_name });
        }
        accounts.push(LoadedAccount {
            display_name: entry.display_name,
            config_dir: expand_home(&entry.config_dir),
        });
    }

    let selection = match parsed.selection.policy.as_deref() {
        None | Some("least_recently_used") => {
            SelectionConfig { policy: SelectionPolicy::LeastRecentlyUsed }
        }
        Some("round_robin") => SelectionConfig { policy: SelectionPolicy::RoundRobin },
        Some(other) => {
            return Err(WorkspaceError::UnknownSelectionPolicy { path, value: other.to_owned() });
        }
    };

    Ok(LoadedConfig { projects, default_index, accounts, selection })
}

fn expand_home(path: &str) -> PathBuf {
    if let Some(stripped) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(stripped);
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn write_config(dir: &std::path::Path, contents: &str) {
        fs::write(dir.join("forge.toml"), contents).expect("write forge.toml");
    }

    #[test]
    fn parses_valid_config_with_default() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[projects]]
name = "aware"
path = "~/Projects/aware"

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
"#,
        );

        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.projects.len(), 2);
        assert_eq!(config.default_project().name, "forge");
        let home = dirs::home_dir().expect("home");
        assert_eq!(config.default_project().path, home.join("Projects/forge"));
    }

    #[test]
    fn missing_file_returns_config_missing() {
        let dir = tempdir().expect("tempdir");
        let err = load_from_dir(dir.path()).expect_err("missing should error");
        assert!(matches!(err, WorkspaceError::ConfigMissing { .. }));
    }

    #[test]
    fn malformed_toml_returns_config_parse() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), "not valid = = toml");
        let err = load_from_dir(dir.path()).expect_err("malformed should error");
        assert!(matches!(err, WorkspaceError::ConfigParse { .. }));
    }

    #[test]
    fn no_default_returns_no_default_project() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("no-default should error");
        assert!(matches!(err, WorkspaceError::NoDefaultProject { .. }));
    }

    #[test]
    fn multiple_defaults_first_wins() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[projects]]
name = "aware"
path = "~/Projects/aware"
default = true

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.default_project().name, "forge");
    }

    #[test]
    fn missing_required_field_returns_config_parse() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[projects]]
name = "forge"
"#, // missing `path`
        );
        let err = load_from_dir(dir.path()).expect_err("missing field should error");
        assert!(matches!(err, WorkspaceError::ConfigParse { .. }));
    }

    #[test]
    fn parses_valid_accounts_and_selection() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"

[[accounts]]
display_name = "Gateway"
config_dir = "~/.claude-gateway"

[selection]
policy = "round_robin"
"#,
        );

        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.accounts.len(), 2);
        assert_eq!(config.accounts[0].display_name, "Stargate");
        let home = dirs::home_dir().expect("home");
        assert_eq!(config.accounts[0].config_dir, home.join(".claude-stargate"));
        assert_eq!(config.selection.policy, SelectionPolicy::RoundRobin);
    }

    #[test]
    fn missing_accounts_returns_no_accounts_configured() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("missing accounts should error");
        assert!(matches!(err, WorkspaceError::NoAccountsConfigured { .. }));
    }

    #[test]
    fn duplicate_account_display_name_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate-other"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("duplicate should error");
        assert!(matches!(err, WorkspaceError::DuplicateAccount { name, .. } if name == "Stargate"));
    }

    #[test]
    fn unknown_selection_policy_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"

[selection]
policy = "weird"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("unknown policy should error");
        assert!(
            matches!(err, WorkspaceError::UnknownSelectionPolicy { value, .. } if value == "weird")
        );
    }

    #[test]
    fn selection_defaults_to_lru_when_missing() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[projects]]
name = "forge"
path = "~/Projects/forge"
default = true

[[accounts]]
display_name = "Stargate"
config_dir = "~/.claude-stargate"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.selection.policy, SelectionPolicy::LeastRecentlyUsed);
    }
}
