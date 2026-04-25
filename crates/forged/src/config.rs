//! `forged.toml` config loader. Default path:
//! `$XDG_CONFIG_HOME/forged/forged.toml` falling back to
//! `~/.config/forged/forged.toml`.
//!
//! See plan §M6.1 for the schema. Missing file is a non-error and yields
//! [`Config::default()`]; other I/O / parse errors propagate.

use serde::{Deserialize, Serialize};

use crate::Error;

/// Daemon configuration loaded from `forged.toml`.
///
/// Unknown fields are rejected (`#[serde(deny_unknown_fields)]`) so a typo
/// in operator-edited config surfaces as a parse error instead of being
/// silently dropped — important UX for ops.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Listen addresses. Each entry becomes its own `TcpListener` task.
    /// Empty list at runtime is treated as "use the loopback default".
    pub bind: Vec<String>,
    /// Log directory. Tilde-expanded at load time when consumed by
    /// [`crate::logging::init`].
    pub log_dir: String,
    /// Days to retain rotated log files. The startup sweep deletes any
    /// log file older than this.
    pub log_retention_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        Self {
            bind: vec!["127.0.0.1:7373".into()],
            log_dir: format!("{home}/Library/Logs/forged"),
            log_retention_days: 14,
        }
    }
}

/// Parse a TOML config from a string.
///
/// # Errors
///
/// [`Error::InvalidRequest`] on parse / unknown-field failures so the
/// surfaced wire-shape code is `-32600` (the closest standard JSON-RPC
/// code for "operator gave us bad input"). Config errors only surface at
/// daemon startup, but the mapping keeps things consistent.
pub fn load_from_str(toml: &str) -> Result<Config, Error> {
    toml::from_str(toml).map_err(|e| Error::InvalidRequest(format!("forged.toml: {e}")))
}

/// Load the config from the default path. If the file is missing, return
/// [`Config::default()`].
///
/// Resolution order:
/// 1. `$XDG_CONFIG_HOME/forged/forged.toml` if `$XDG_CONFIG_HOME` is set.
/// 2. `$HOME/.config/forged/forged.toml` otherwise.
///
/// # Errors
///
/// I/O or parse errors propagate. Missing file is a non-error.
pub fn load_default() -> Result<Config, Error> {
    let path = default_path();
    match std::fs::read_to_string(&path) {
        Ok(body) => load_from_str(&body),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
        Err(e) => Err(Error::Io(e)),
    }
}

/// Path the loader will read by default. Public so tests can assert
/// resolution behaviour without touching the disk.
#[must_use]
pub fn default_path() -> std::path::PathBuf {
    let xdg = std::env::var_os("XDG_CONFIG_HOME").map(std::path::PathBuf::from);
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    let base = xdg
        .or_else(|| home.map(|h| h.join(".config")))
        .unwrap_or_default();
    base.join("forged").join("forged.toml")
}
