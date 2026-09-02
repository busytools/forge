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

    #[error(
        "accounts missing provider in forge.toml at {}: {}. Each needs one of {}. If a line is \
         present, check it sits above that account's [accounts.env] table - below it TOML reads \
         it as an env key",
        path.display(),
        names.join(", "),
        forge_primitives::account::Provider::ACCEPTED,
    )]
    AccountsMissingProvider { path: PathBuf, names: Vec<String> },

    #[error(
        "account '{name}' in forge.toml at {} sets provider = \"openrouter\" with an \
         ANTHROPIC_BASE_URL that is not the API root; it must end in /api, as in \
         https://openrouter.ai/api",
        path.display()
    )]
    OpenrouterBaseUrlNotApiRoot { path: PathBuf, name: String },

    #[error(
        "account '{name}' in forge.toml at {} sets permission_mode = \"{value}\"; accepted values: {}",
        path.display(),
        forge_primitives::permission::PermissionMode::ACCEPTED
    )]
    AccountInvalidPermissionMode { path: PathBuf, name: String, value: String },

    #[error(
        "account '{name}' in forge.toml at {} declares a base-url provider but has no ANTHROPIC_BASE_URL in [accounts.env]",
        path.display()
    )]
    AccountProviderNeedsBaseUrl { path: PathBuf, name: String },

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
        "org '{org}' in forge.toml at {} lists only experimental accounts; list at least one non-experimental account",
        path.display()
    )]
    AllExperimentalOrgAccounts { path: PathBuf, org: String },

    #[error(
        "org '{org}' in forge.toml at {} references unknown account '{account}'; valid accounts: {valid}",
        path.display()
    )]
    UnknownOrgAccount { path: PathBuf, org: String, account: String, valid: String },

    #[error(
        "forge.toml at {} has [projects.<name>.env] for undeclared projects: {projects}; valid projects: {valid}",
        path.display()
    )]
    UnknownProjectEnv { path: PathBuf, projects: String, valid: String },

    #[error("no project named '{name}' in forge.toml at {}", path.display())]
    ProjectNotFound { name: String, path: PathBuf },

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
