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
        "forge.toml at {}: project '{project_name}' static_workers contains invalid label '{role}'",
        path.display()
    )]
    UnknownStaticWorker { path: PathBuf, project_name: String, role: String },

    #[error(
        "forge.toml at {}: project '{project_name}' static_workers has duplicate label '{role}'; only one instance per label is supported",
        path.display()
    )]
    DuplicateStaticWorker { path: PathBuf, project_name: String, role: String },

    #[error(
        "wire-classification rewriter proxy failed to start: {reason}. forge refuses to spawn sessions without a healthy proxy because the wire shape Anthropic sees determines billing tier"
    )]
    ProxyUnavailable { reason: String },

    #[error(
        "failed to create the forge config directory at {}: {source}. forge cannot persist state, crons, or the single-instance lock without a writable config dir",
        path.display()
    )]
    DataDirUnavailable {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "a forge instance is already running on this config dir{}",
        pid.map_or(String::new(), |p| format!(" (PID {p})"))
    )]
    AlreadyRunning { pid: Option<u32> },
}
