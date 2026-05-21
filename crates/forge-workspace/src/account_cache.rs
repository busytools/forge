//! Persistent on-disk cache for account usage snapshots.
//!
//! Solves the cold-boot problem: Anthropic's `/api/oauth/usage`
//! endpoint rate-limits aggressively on per-IP burst probes — the
//! first forge launch routinely waits 30 s+ before the warm probe
//! gets through, during which the launchpad and bottom panel show
//! empty bars and the picker ties at tier 0 (unknown-fresh). The
//! cache breaks that loop: every successful poll writes the snapshot
//! to disk, and the next boot reads it back into the in-memory
//! `AccountStateMap` before any spawn fires. Stale snapshots are
//! acceptable seed data — the 60 s background poller will refresh
//! them in the background.
//!
//! Path: `<workspace_config_dir>/forge-state.toml`. Single TOML file
//! that mirrors the `forge.toml` convention — config + state both
//! live in the same directory under the same format. Schema versioned
//! so future shape changes can invalidate cleanly.
//!
//! Failures are non-fatal: missing file, corrupt TOML, IO errors all
//! degrade to "no cache loaded; spawn paths see empty bars until the
//! poller succeeds." Tracing surfaces the breadcrumb at debug level.

use std::path::{Path, PathBuf};

use forge_primitives::usage::UsageSnapshot;
use serde::{Deserialize, Serialize};

const CACHE_SCHEMA_VERSION: u32 = 1;
const STATE_FILE_RELATIVE_PATH: &str = "forge-state.toml";

/// Per-account cache entry stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedAccountUsage {
    pub snapshot: UsageSnapshot,
    /// When the snapshot was written to disk (wall-clock SystemTime).
    /// Not used for staleness gating today — the cache is purely seed
    /// data and the poller refreshes regardless. Kept so a future
    /// "refuse to use snapshots older than N hours" policy can be
    /// added without a state-file schema bump.
    pub cached_at: std::time::SystemTime,
}

/// Versioned on-disk state document. `version` mismatch on read
/// triggers a clean reset (treat as empty) so a future schema change
/// degrades to a single cold boot rather than a corrupt-data panic.
///
/// One file per workspace at `<config_dir>/forge-state.toml`. Layout:
///
/// ```toml
/// version = 1
///
/// [account_usage.Granite]
/// cached_at = { secs_since_epoch = 1779356400, nanos_since_epoch = 0 }
/// [account_usage.Granite.snapshot]
/// source = "Oauth"
/// fetched_at = { secs_since_epoch = 1779356400, nanos_since_epoch = 0 }
/// # ...
/// ```
///
/// Section name `account_usage` is namespaced so future state
/// (window layout, recent project list, …) can be added alongside
/// without bumping the schema version.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ForgeState {
    version: u32,
    /// Account display name → cached snapshot. `BTreeMap` so the
    /// TOML serialisation is deterministic for diffing.
    #[serde(default)]
    pub account_usage: std::collections::BTreeMap<String, CachedAccountUsage>,
}

impl ForgeState {
    fn empty() -> Self {
        Self { version: CACHE_SCHEMA_VERSION, account_usage: std::collections::BTreeMap::new() }
    }
}

/// Resolve the state file path for a workspace `config_dir`.
pub(crate) fn state_path(config_dir: &Path) -> PathBuf {
    config_dir.join(STATE_FILE_RELATIVE_PATH)
}

/// Read the state file from disk. Returns an empty state on any
/// failure (missing file, IO error, TOML parse error, schema-version
/// mismatch). Logs the breadcrumb at debug for failure paths so a
/// triaging user can see why their cold boot got no seed data,
/// without polluting the default log level when the file simply
/// doesn't exist yet.
pub(crate) fn load(config_dir: &Path) -> ForgeState {
    let path = state_path(config_dir);
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // First boot on this config_dir — entirely expected.
            return ForgeState::empty();
        }
        Err(e) => {
            tracing::debug!(
                target: "forge_workspace::account_cache",
                error = %e,
                path = %path.display(),
                "forge-state.toml present but read failed; treating as empty",
            );
            return ForgeState::empty();
        }
    };
    let parsed: ForgeState = match toml::from_str(&contents) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(
                target: "forge_workspace::account_cache",
                error = %e,
                path = %path.display(),
                "forge-state.toml parse failed; treating as empty",
            );
            return ForgeState::empty();
        }
    };
    if parsed.version != CACHE_SCHEMA_VERSION {
        tracing::debug!(
            target: "forge_workspace::account_cache",
            disk_version = parsed.version,
            expected_version = CACHE_SCHEMA_VERSION,
            "forge-state.toml schema-version mismatch; ignoring on-disk entries",
        );
        return ForgeState::empty();
    }
    parsed
}

/// Persist the in-memory snapshot collection to disk via atomic
/// replace. The config dir is expected to already exist (forge.toml
/// lives there). Failures are non-fatal and logged at warn so a
/// persistent permission / disk-full problem surfaces, but a single
/// failed write doesn't cascade.
pub(crate) fn store(
    config_dir: &Path,
    entries: &std::collections::BTreeMap<String, CachedAccountUsage>,
) {
    let state =
        ForgeState { version: CACHE_SCHEMA_VERSION, account_usage: entries.clone() };
    let path = state_path(config_dir);
    let serialised = match toml::to_string_pretty(&state) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::account_cache",
                error = %e,
                "forge-state.toml serialise failed; skipping write",
            );
            return;
        }
    };
    // Atomic replace: write to <path>.tmp then rename. A crash
    // between write+rename leaves the previous state intact rather
    // than a partial file.
    let tmp_path = path.with_extension("toml.tmp");
    if let Err(e) = std::fs::write(&tmp_path, &serialised) {
        tracing::warn!(
            target: "forge_workspace::account_cache",
            error = %e,
            path = %tmp_path.display(),
            "forge-state.toml tmp write failed",
        );
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        tracing::warn!(
            target: "forge_workspace::account_cache",
            error = %e,
            from = %tmp_path.display(),
            to = %path.display(),
            "forge-state.toml atomic rename failed",
        );
        // Best-effort: drop the tmp file so we don't leak.
        let _ = std::fs::remove_file(&tmp_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn fake_snapshot() -> UsageSnapshot {
        UsageSnapshot {
            source: UsageSourceKind::Oauth,
            fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
            five_hour: Some(UsageWindow {
                label: "5-hour",
                utilization: 42.0,
                resets_at: None,
                reset_description: None,
            }),
            seven_day: None,
            seven_day_opus: None,
            seven_day_sonnet: None,
            extra_usage: None,
        }
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let dir = tempdir().expect("tempdir");
        let state = load(dir.path());
        assert!(state.account_usage.is_empty());
    }

    #[test]
    fn round_trip_preserves_snapshot() {
        let dir = tempdir().expect("tempdir");
        let mut entries = std::collections::BTreeMap::new();
        entries.insert(
            "Granite".to_owned(),
            CachedAccountUsage { snapshot: fake_snapshot(), cached_at: SystemTime::UNIX_EPOCH },
        );
        store(dir.path(), &entries);

        let loaded = load(dir.path());
        assert_eq!(loaded.account_usage.len(), 1);
        let entry = loaded.account_usage.get("Granite").expect("granite");
        assert_eq!(entry.snapshot.five_hour.as_ref().map(|w| w.utilization), Some(42.0));
    }

    #[test]
    fn version_mismatch_treated_as_empty() {
        let dir = tempdir().expect("tempdir");
        let path = state_path(dir.path());
        std::fs::write(&path, "version = 9999\n").expect("write");
        let loaded = load(dir.path());
        assert!(loaded.account_usage.is_empty());
    }

    #[test]
    fn corrupt_toml_treated_as_empty() {
        let dir = tempdir().expect("tempdir");
        let path = state_path(dir.path());
        std::fs::write(&path, "not = toml = at all").expect("write");
        let loaded = load(dir.path());
        assert!(loaded.account_usage.is_empty());
    }
}
