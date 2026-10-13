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
//! data directory (`forge/` under the platform app-support base),
//! shared by the logs and the single-instance lock.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use crate::Error;

/// Resolve forge's machine-local application-support directory
/// (`~/Library/Application Support/forge` on macOS,
/// `$XDG_DATA_HOME/forge` on Linux). Distinct from the `claude`
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
        return Ok(dir.join("forge"));
    }
    if let Some(dir) = dirs::cache_dir() {
        return Ok(dir.join("forge"));
    }
    if let Some(home) = dirs::home_dir() {
        return Ok(home.join(".forge"));
    }
    Err(Error::Connection {
        reason: "no resolvable data/cache/home dir for forge app-support path".into(),
    })
}

/// The same resolution under the directory's legacy name, when one
/// resolves.
fn legacy_app_support_dir() -> Option<PathBuf> {
    if let Some(dir) = dirs::data_local_dir() {
        return Some(dir.join("forge-tui"));
    }
    if let Some(dir) = dirs::cache_dir() {
        return Some(dir.join("forge-tui"));
    }
    dirs::home_dir().map(|home| home.join(".forge-tui"))
}

/// The legacy `forge-tui` tree, when it exists and the current one does
/// not - the state a pre-migration install boots from.
///
/// **A boot that finds this refuses** rather than starting under the new
/// name with an empty store: the durable state (the single-instance
/// lock, the redb store, the logs) all lives under the legacy tree, and
/// silently starting beside it reads as every cron, subscription and
/// session id having vanished. The refusal names
/// `scripts/migrate-app-support.sh`, which moves the tree; the binary
/// itself carries no migration branch.
pub fn legacy_app_support_left_behind() -> Option<PathBuf> {
    let current = app_support_dir().ok()?;
    let legacy = legacy_app_support_dir()?;
    left_behind_between(&legacy, &current).then_some(legacy)
}

/// [`legacy_app_support_left_behind`]'s rule over explicit paths, so it
/// is testable without the real machine directories. A stray
/// non-directory `forge-tui` entry is not the store, and refusing on it
/// would deadlock against the script, which tests `-d` and finds nothing
/// to migrate.
fn left_behind_between(legacy: &Path, current: &Path) -> bool {
    legacy.is_dir() && !current.exists()
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
    fn app_support_dir_has_forge_leaf() {
        let dir = app_support_dir().expect("resolves on a normal dev machine");
        assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some("forge"));
    }

    /// The legacy name resolves beside the current one - same parent,
    /// `forge-tui` leaf - which is what lets the migration script move
    /// the tree with one rename.
    #[test]
    fn the_legacy_name_resolves_beside_the_current_one() {
        let current = app_support_dir().expect("resolves on a normal dev machine");
        let legacy = legacy_app_support_dir().expect("resolves on a normal dev machine");
        assert_eq!(legacy.parent(), current.parent());
        assert_eq!(legacy.file_name().and_then(|s| s.to_str()), Some("forge-tui"));
    }

    /// **A legacy tree with no current one is the refused state**: the
    /// store, the lock and the logs are all under it, and a boot that
    /// proceeded would start empty beside them.
    #[test]
    fn a_legacy_tree_alone_is_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join("forge-tui");
        let current = dir.path().join("forge");
        std::fs::create_dir_all(&legacy).expect("legacy tree");

        assert!(
            left_behind_between(&legacy, &current),
            "the legacy tree is the state to refuse on"
        );
    }

    /// A stray non-directory entry named `forge-tui` is not the legacy
    /// store: the boot proceeds and the script finds nothing to migrate,
    /// where an `exists()` test here once refused against a script that
    /// no-oped.
    #[test]
    fn a_stray_file_named_forge_tui_is_not_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join("forge-tui");
        let current = dir.path().join("forge");
        std::fs::write(&legacy, "stray").expect("stray file");

        assert!(!left_behind_between(&legacy, &current), "a stray file does not refuse a boot");
    }

    /// Once the migration script has run, the current tree exists and
    /// the boot proceeds; the legacy tree left beside it is the
    /// reader's to inspect.
    #[test]
    fn a_current_tree_beside_a_legacy_one_boots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join("forge-tui");
        let current = dir.path().join("forge");
        std::fs::create_dir_all(&legacy).expect("legacy tree");
        std::fs::create_dir_all(&current).expect("current tree");

        assert!(!left_behind_between(&legacy, &current), "a migrated install boots");
    }

    #[test]
    fn neither_tree_present_is_not_left_behind() {
        let dir = tempfile::tempdir().expect("tempdir");
        let legacy = dir.path().join("forge-tui");
        let current = dir.path().join("forge");

        assert!(!left_behind_between(&legacy, &current), "a fresh install boots");
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
