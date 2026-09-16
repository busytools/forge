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
        "project '{name}' in forge.toml at {} sets permission_mode = \"{value}\"; accepted values: {}",
        path.display(),
        forge_primitives::permission::PermissionMode::ACCEPTED
    )]
    ProjectInvalidPermissionMode { path: PathBuf, name: String, value: String },

    #[error(
        "gateway port 0 in forge.toml at {} is not usable; the port is fixed, not OS-assigned",
        path.display()
    )]
    GatewayPortInvalid { path: PathBuf },

    #[error(
        "gateway key '{key}' = 0 in forge.toml at {} is not usable; 0 would silently disable the mechanism it configures",
        path.display()
    )]
    GatewayRotationInvalid { path: PathBuf, key: &'static str },

    #[error(
        "account '{name}' in forge.toml at {} declares a base-url provider but has no base_url key",
        path.display()
    )]
    AccountProviderNeedsBaseUrl { path: PathBuf, name: String },

    #[error(
        "account '{name}' in forge.toml at {} is missing the required token key",
        path.display()
    )]
    AccountTokenRequired { path: PathBuf, name: String },

    #[error(
        "account '{name}' in forge.toml at {} declares no models; every account must list the models it serves",
        path.display()
    )]
    AccountModelsRequired { path: PathBuf, name: String },

    #[error(
        "account in forge.toml at {} maps slug '{slug}' that is not in its models list",
        path.display()
    )]
    AccountSlugUndeclared { path: PathBuf, slug: String },

    #[error(
        "account in forge.toml at {} maps a blank slug for '{slug}'",
        path.display()
    )]
    AccountSlugBlank { path: PathBuf, slug: String },

    #[error(
        "account in forge.toml at {} declares aliases for '{alias}' that is not in its models list",
        path.display()
    )]
    AccountAliasUndeclared { path: PathBuf, alias: String },

    #[error(
        "account in forge.toml at {} declares an empty alias list for '{alias}'",
        path.display()
    )]
    AccountAliasEmpty { path: PathBuf, alias: String },

    #[error(
        "project '{name}' in forge.toml at {} sets model '{model}' that no account in its org declares; every session's first request would fail",
        path.display()
    )]
    ProjectModelUndeclared { path: PathBuf, name: String, model: String },

    #[error(
        "account '{name}' in forge.toml at {} sets gateway keys ({keys}) in its env layer; declare them as the flat base_url and token keys instead",
        path.display()
    )]
    AccountEnvCarriesGatewayKeys { path: PathBuf, name: String, keys: String },

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
        "no account in org '{org}' serves project '{project}' model '{model}'; \
         accounts considered: {accounts}"
    )]
    NoAccountServesProjectModel { project: String, org: String, model: String, accounts: String },

    #[error(
        "org '{org}' in forge.toml at {} references unknown account '{account}'; valid accounts: {valid}",
        path.display()
    )]
    UnknownOrgAccount { path: PathBuf, org: String, account: String, valid: String },

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
