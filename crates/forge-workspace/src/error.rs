//! Error types surfaced by `Workspace::new` and friends.

use std::path::PathBuf;

use thiserror::Error;

/// Reasons `Workspace::new` may fail.
#[derive(Debug, Error)]
pub enum WorkspaceError {
    #[error(
        "forge.toml not found at {}; create it with at least one [[projects]] entry marked default = true",
        path.display()
    )]
    ConfigMissing { path: PathBuf },

    #[error("forge.toml at {} failed to parse: {source}", path.display())]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    #[error("forge.toml at {} has no project marked default = true", path.display())]
    NoDefaultProject { path: PathBuf },

    #[error("forge.toml at {} is otherwise invalid: {message}", path.display())]
    ConfigInvalid { path: PathBuf, message: String },

    #[error("no [[accounts]] entries in forge.toml at {}", path.display())]
    NoAccountsConfigured { path: PathBuf },

    #[error("duplicate account display_name '{name}' in forge.toml at {}", path.display())]
    DuplicateAccount { path: PathBuf, name: String },

    #[error("no project named '{name}' in forge.toml at {}", path.display())]
    ProjectNotFound { name: String, path: PathBuf },

    #[error(
        "project '{project}' in forge.toml at {} references unknown account '{account}'; valid accounts: {valid}",
        path.display()
    )]
    UnknownProjectAccount { path: PathBuf, project: String, account: String, valid: String },

    #[error(
        "project '{project}' in forge.toml at {} has an empty `accounts = []` list; remove the field or list at least one account",
        path.display()
    )]
    EmptyProjectAccounts { path: PathBuf, project: String },
}
