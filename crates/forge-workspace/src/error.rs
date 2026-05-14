//! Error types surfaced by `Workspace::new` and friends.

use std::path::PathBuf;

use thiserror::Error;

/// Reasons `Workspace::new` may fail.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error(
        "forge.toml not found at {}; create it with at least one [[orgs]] entry containing one [[orgs.projects]] entry",
        path.display()
    )]
    ConfigMissing { path: PathBuf },

    #[error("forge.toml at {} failed to parse: {source}", path.display())]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("forge.toml at {} is otherwise invalid: {message}", path.display())]
    ConfigInvalid { path: PathBuf, message: String },

    #[error("no [[orgs]] entries in forge.toml at {}", path.display())]
    NoOrgsConfigured { path: PathBuf },

    #[error("no [[orgs.projects]] entries across any org in forge.toml at {}", path.display())]
    NoProjectsConfigured { path: PathBuf },

    #[error("no [[accounts]] entries in forge.toml at {}", path.display())]
    NoAccountsConfigured { path: PathBuf },

    #[error("duplicate account display_name '{name}' in forge.toml at {}", path.display())]
    DuplicateAccount { path: PathBuf, name: String },

    #[error("duplicate org name '{name}' in forge.toml at {}", path.display())]
    DuplicateOrg { path: PathBuf, name: String },

    #[error(
        "duplicate project name '{name}' in forge.toml at {} (project names must be unique across all orgs)",
        path.display()
    )]
    DuplicateProject { path: PathBuf, name: String },

    #[error(
        "org '{org}' in forge.toml at {} has no [[orgs.projects]] entries",
        path.display()
    )]
    EmptyOrg { path: PathBuf, org: String },

    #[error(
        "org '{org}' in forge.toml at {} has an empty `accounts = []` list; list at least one account",
        path.display()
    )]
    EmptyOrgAccounts { path: PathBuf, org: String },

    #[error(
        "org '{org}' in forge.toml at {} references unknown account '{account}'; valid accounts: {valid}",
        path.display()
    )]
    UnknownOrgAccount { path: PathBuf, org: String, account: String, valid: String },

    #[error("no project named '{name}' in forge.toml at {}", path.display())]
    ProjectNotFound { name: String, path: PathBuf },

    #[error(
        "multiple projects marked `focus = true` in forge.toml at {} ({names}); at most one project may carry focus",
        path.display()
    )]
    MultipleFocusProjects { path: PathBuf, names: String },
}
