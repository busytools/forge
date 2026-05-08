//! Error types surfaced by `Workspace::new` and friends.

use std::path::PathBuf;

use thiserror::Error;

/// Reasons `Workspace::new` may fail.
#[derive(Debug, Error)]
#[non_exhaustive]
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

    #[error("no project named '{name}' in forge.toml at {}", path.display())]
    ProjectNotFound { name: String, path: PathBuf },
}
