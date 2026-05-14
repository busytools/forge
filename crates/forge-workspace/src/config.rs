//! `forge.toml` schema + loader.
//!
//! **Org model.** Projects are grouped under `[[orgs]]` (Subspace,
//! Granite, Personal, etc.). Each org carries the `accounts = [...]`
//! pin shared by all its projects, replacing the per-project pin
//! that lived here before. Projects within an org keep a flat list
//! via `[[orgs.projects]]`. Multiple projects can carry
//! `auto_start = true`; all auto-start projects spawn at launch and
//! the first one (alphabetical) becomes the focused tab.
//!
//! **Selection policy.** Exactly one: every account spawn picks the
//! account in the org's `accounts` subset with the most remaining
//! usage budget. Cold cache → first-in-subset by definition order.
//! No LRU, no round-robin, no fallback to accounts outside the org's
//! subset.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::WorkspaceError;

#[derive(Debug, Deserialize)]
struct ForgeToml {
    #[serde(default)]
    orgs: Vec<OrgEntry>,
    #[serde(default)]
    accounts: Vec<AccountEntry>,
}

#[derive(Debug, Deserialize)]
struct OrgEntry {
    name: String,
    /// Account `display_name`s every project in this org is allowed
    /// to spawn under. Required; cross-validated against `[[accounts]]`.
    accounts: Vec<String>,
    #[serde(default)]
    projects: Vec<ProjectEntry>,
}

#[derive(Debug, Deserialize)]
struct ProjectEntry {
    name: String,
    path: String,
    /// When `true`, the project's lead session spawns automatically
    /// at forge launch. Multiple projects can carry this; the first
    /// alphabetically becomes the focused tab. Defaults to `false`.
    #[serde(default)]
    auto_start: bool,
}

#[derive(Debug, Deserialize)]
struct AccountEntry {
    display_name: String,
    config_dir: String,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedAccount {
    pub display_name: String,
    pub config_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedConfig {
    pub orgs: Vec<LoadedOrg>,
    pub projects: Vec<LoadedProject>,
    /// Index into `projects` for the focus-target at startup —
    /// alphabetically-first project that carries `auto_start = true`,
    /// falling back to the alphabetically-first project overall
    /// when no project opts in.
    pub default_index: usize,
    pub accounts: Vec<LoadedAccount>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedOrg {
    pub name: String,
    /// Pinned account `display_name`s shared by all projects in
    /// this org. Always non-empty (load enforces).
    pub accounts: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct LoadedProject {
    pub name: String,
    pub path: PathBuf,
    /// Original path string from `forge.toml`, preserved for display
    /// (e.g. `~/Projects/forge` with `~` un-expanded). Use `path` for
    /// filesystem access; this for human-readable output.
    pub display_path: String,
    /// Name of the org this project belongs to (matches
    /// `LoadedOrg.name`). Workspace `project_accounts_for` resolves
    /// the pin via this back-reference.
    pub org: String,
    /// Cached pinned account list from the project's org. Duplicated
    /// here so callers don't need to walk the org list on every
    /// resolution.
    pub accounts: Vec<String>,
    /// `true` when the project should spawn automatically at forge
    /// launch.
    pub auto_start: bool,
}

impl LoadedConfig {
    pub(crate) fn default_project(&self) -> &LoadedProject {
        &self.projects[self.default_index]
    }

    /// Iterate auto-start projects in alphabetical order. The
    /// startup path spawns each one and focuses the first.
    pub(crate) fn auto_start_projects(&self) -> impl Iterator<Item = &LoadedProject> {
        self.projects.iter().filter(|p| p.auto_start)
    }

    /// Empty `LoadedConfig` for the `testing` feature's
    /// `Workspace::testing_stub`. Production code paths that need a
    /// project (e.g. `default_project`) will panic when called on
    /// this value — tests that only need `domain_handles` access
    /// never reach those paths.
    #[cfg(feature = "testing")]
    pub(crate) fn empty_for_test() -> Self {
        Self {
            orgs: Vec::new(),
            projects: Vec::new(),
            default_index: 0,
            accounts: Vec::new(),
        }
    }
}

/// Load + validate `<config_dir>/forge.toml`. Returns the parsed
/// orgs + projects with `~` expanded.
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

    if parsed.orgs.is_empty() {
        return Err(WorkspaceError::NoOrgsConfigured { path });
    }
    if parsed.accounts.is_empty() {
        return Err(WorkspaceError::NoAccountsConfigured { path });
    }

    // Validate accounts first — orgs cross-reference them.
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

    // Validate orgs + build the flat project list.
    let mut seen_org_names: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut seen_project_names: std::collections::HashSet<String> =
        std::collections::HashSet::new();
    let mut orgs: Vec<LoadedOrg> = Vec::with_capacity(parsed.orgs.len());
    let mut projects: Vec<LoadedProject> = Vec::new();
    for org_entry in parsed.orgs {
        if !seen_org_names.insert(org_entry.name.clone()) {
            return Err(WorkspaceError::DuplicateOrg { path, name: org_entry.name });
        }
        if org_entry.accounts.is_empty() {
            return Err(WorkspaceError::EmptyOrgAccounts { path, org: org_entry.name });
        }
        for account in &org_entry.accounts {
            if !seen_account_names.contains(account) {
                let mut valid: Vec<&str> =
                    seen_account_names.iter().map(String::as_str).collect();
                valid.sort_unstable();
                return Err(WorkspaceError::UnknownOrgAccount {
                    path,
                    org: org_entry.name,
                    account: account.clone(),
                    valid: valid.join(", "),
                });
            }
        }
        if org_entry.projects.is_empty() {
            return Err(WorkspaceError::EmptyOrg { path, org: org_entry.name });
        }
        for project_entry in org_entry.projects {
            if !seen_project_names.insert(project_entry.name.clone()) {
                return Err(WorkspaceError::DuplicateProject {
                    path,
                    name: project_entry.name,
                });
            }
            projects.push(LoadedProject {
                name: project_entry.name,
                path: expand_home(&project_entry.path),
                display_path: project_entry.path,
                org: org_entry.name.clone(),
                accounts: org_entry.accounts.clone(),
                auto_start: project_entry.auto_start,
            });
        }
        orgs.push(LoadedOrg { name: org_entry.name, accounts: org_entry.accounts });
    }

    if projects.is_empty() {
        return Err(WorkspaceError::NoProjectsConfigured { path });
    }

    // Default project: first auto_start by alpha order; else
    // alphabetically-first project overall. No `default = true` flag
    // any more — selection is implicit from auto_start.
    let default_index = {
        let mut alpha: Vec<usize> = (0..projects.len()).collect();
        alpha.sort_by(|a, b| projects[*a].name.cmp(&projects[*b].name));
        alpha
            .iter()
            .copied()
            .find(|&i| projects[i].auto_start)
            .unwrap_or_else(|| alpha[0])
    };

    Ok(LoadedConfig { orgs, projects, default_index, accounts })
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

    fn minimal_config() -> &'static str {
        r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
auto_start = true

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#
    }

    #[test]
    fn parses_minimal_config() {
        let dir = tempdir().expect("tempdir");
        write_config(dir.path(), minimal_config());
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.orgs.len(), 1);
        assert_eq!(config.orgs[0].name, "Personal");
        assert_eq!(config.projects.len(), 1);
        assert_eq!(config.default_project().name, "forge");
        assert_eq!(config.default_project().org, "Personal");
        assert_eq!(config.default_project().accounts, vec!["Subspace"]);
        assert!(config.default_project().auto_start);
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
    fn no_orgs_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("missing orgs should error");
        assert!(matches!(err, WorkspaceError::NoOrgsConfigured { .. }));
    }

    #[test]
    fn empty_org_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Empty"
accounts = ["Subspace"]

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("org without projects should error");
        assert!(matches!(err, WorkspaceError::EmptyOrg { org, .. } if org == "Empty"));
    }

    #[test]
    fn empty_org_accounts_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = []

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("empty accounts should error");
        assert!(matches!(err, WorkspaceError::EmptyOrgAccounts { org, .. } if org == "Personal"));
    }

    #[test]
    fn unknown_org_account_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace", "Bogus"]

[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("unknown account should error");
        match err {
            WorkspaceError::UnknownOrgAccount { org, account, .. } => {
                assert_eq!(org, "Personal");
                assert_eq!(account, "Bogus");
            }
            other => panic!("expected UnknownOrgAccount, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_org_name_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "aware"
path = "~/Projects/aware"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("duplicate org should error");
        assert!(matches!(err, WorkspaceError::DuplicateOrg { name, .. } if name == "Personal"));
    }

    #[test]
    fn duplicate_project_name_errors() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Subspace"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/subspace-forge"

[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("duplicate project should error");
        assert!(matches!(err, WorkspaceError::DuplicateProject { name, .. } if name == "forge"));
    }

    #[test]
    fn default_project_is_alpha_first_auto_start() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "zebra"
path = "~/Projects/zebra"
auto_start = true
[[orgs.projects]]
name = "alpha"
path = "~/Projects/alpha"
auto_start = true
[[orgs.projects]]
name = "middle"
path = "~/Projects/middle"
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        // Auto-start projects: alpha + zebra. Alphabetical-first: alpha.
        assert_eq!(config.default_project().name, "alpha");
    }

    #[test]
    fn default_project_falls_back_to_alpha_first_overall() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "zebra"
path = "~/Projects/zebra"
[[orgs.projects]]
name = "alpha"
path = "~/Projects/alpha"
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        assert_eq!(config.default_project().name, "alpha");
    }

    #[test]
    fn auto_start_projects_iterator_returns_only_opted_in() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "alpha"
path = "~/Projects/alpha"
auto_start = true
[[orgs.projects]]
name = "beta"
path = "~/Projects/beta"
[[orgs.projects]]
name = "gamma"
path = "~/Projects/gamma"
auto_start = true
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
"#,
        );
        let config = load_from_dir(dir.path()).expect("happy path");
        let names: Vec<&str> = config.auto_start_projects().map(|p| p.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "gamma"]);
    }

    #[test]
    fn missing_accounts_returns_no_accounts_configured() {
        let dir = tempdir().expect("tempdir");
        write_config(
            dir.path(),
            r#"
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"
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
[[orgs]]
name = "Personal"
accounts = ["Subspace"]
[[orgs.projects]]
name = "forge"
path = "~/Projects/forge"

[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-subspace"
[[accounts]]
display_name = "Subspace"
config_dir = "~/.claude-other"
"#,
        );
        let err = load_from_dir(dir.path()).expect_err("duplicate account should error");
        assert!(matches!(err, WorkspaceError::DuplicateAccount { name, .. } if name == "Subspace"));
    }

    #[test]
    fn legacy_selection_section_is_silently_ignored() {
        let dir = tempdir().expect("tempdir");
        let mut config_text = minimal_config().to_owned();
        config_text.push_str("\n[selection]\npolicy = \"round_robin\"\n");
        write_config(dir.path(), &config_text);
        let config = load_from_dir(dir.path()).expect("legacy [selection] should be ignored");
        assert_eq!(config.default_project().name, "forge");
    }
}
