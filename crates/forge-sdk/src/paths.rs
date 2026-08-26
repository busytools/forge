//! Path helpers around the `claude` CLI's on-disk config layout.
//!
//! forge-sdk does not fall back to `~/.claude` on its own.
//! Resolution lives in `forge-agent` / `forge-workspace` - those
//! layers resolve each account's `config_dir` from `forge.toml` and
//! thread the resulting
//! `PathBuf` into every accessor that needs it. forge-sdk only
//! exposes a `<config_dir> + "projects"` join helper for callers that
//! already hold a config_dir.
//!
//! Separately, [`app_support_dir`] resolves forge's own machine-local
//! data directory (`forge-tui/` under the platform app-support base),
//! shared by the logs and the single-instance lock.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::Error;

/// Resolve forge's machine-local application-support directory
/// (`~/Library/Application Support/forge-tui` on macOS,
/// `$XDG_DATA_HOME/forge-tui` on Linux). Distinct from the `claude`
/// config dir: this holds forge's OWN per-machine state - the
/// diagnostics logs and the single-instance lock both live under
/// here, and it is never synced across machines.
///
/// # Errors
///
/// [`Error::Connection`] when none of `dirs::data_local_dir()`,
/// `dirs::cache_dir()`, or `dirs::home_dir()` resolve. Per the
/// project's Hard Rule #14 there is no `current_dir()` fallback - the
/// path must not vary with the launch directory.
pub fn app_support_dir() -> Result<PathBuf, Error> {
    if let Some(dir) = dirs::data_local_dir() {
        return Ok(dir.join("forge-tui"));
    }
    if let Some(dir) = dirs::cache_dir() {
        return Ok(dir.join("forge-tui"));
    }
    if let Some(home) = dirs::home_dir() {
        return Ok(home.join(".forge-tui"));
    }
    Err(Error::Connection {
        reason: "no resolvable data/cache/home dir for forge app-support path".into(),
    })
}

/// Stable hex digest of `config_dir` that keys forge's machine-local
/// per-config-dir files (the single-instance lock, the state cache).
/// Canonicalised first (best-effort) so symlinked / trailing-slash
/// variants map to one digest.
pub fn config_dir_hash(config_dir: &Path) -> String {
    let canonical = config_dir.canonicalize().unwrap_or_else(|_| config_dir.to_path_buf());
    let mut hasher = DefaultHasher::new();
    canonical.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// Path to a config_dir's `projects/` subdirectory. Caller passes
/// the resolved `config_dir` explicitly; this helper just performs
/// the join so the layout convention lives in one place.
pub fn projects_dir_for(config_dir: &Path) -> PathBuf {
    config_dir.join("projects")
}

#[cfg(test)]
mod tests {

    use super::*;

    #[test]
    fn projects_dir_for_appends_projects_subdir() {
        assert_eq!(projects_dir_for(Path::new("/tmp/cfg")), PathBuf::from("/tmp/cfg/projects"),);
    }

    #[test]
    fn app_support_dir_has_forge_tui_leaf() {
        let dir = app_support_dir().expect("resolves on a normal dev machine");
        assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some("forge-tui"));
    }

    #[test]
    fn config_dir_hash_is_deterministic_and_hex() {
        let path = Path::new("/tmp/forge-config-dir-hash-fixture");
        let first = config_dir_hash(path);
        assert_eq!(first, config_dir_hash(path), "same config dir hashes identically");
        assert_eq!(first.len(), 16, "digest is 16 hex chars");
        assert!(first.chars().all(|c| c.is_ascii_hexdigit()), "digest is hex: {first}");
    }

    #[test]
    fn config_dir_hash_differs_across_config_dirs() {
        assert_ne!(
            config_dir_hash(Path::new("/tmp/forge-config-dir-hash-a")),
            config_dir_hash(Path::new("/tmp/forge-config-dir-hash-b")),
        );
    }
}
