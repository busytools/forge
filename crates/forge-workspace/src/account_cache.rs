//! Persistent forge state: the `/spinner` override + the per-account
//! usage cache.
//!
//! Both live in the machine-local redb store ([`crate::store::state`]).
//! [`load`] / [`store`] / [`store_spinner`] are thin wrappers over that
//! tenant; [`ForgeState`] / [`CachedAccountUsage`] are the in-memory +
//! serde shapes, still used to parse the legacy `state.toml` the one-time
//! seed migrates.
//!
//! The usage cache solves the cold-boot problem: Anthropic's
//! `/api/oauth/usage` endpoint rate-limits aggressively on per-IP burst
//! probes, so the first launch can wait 30 s+ before the warm probe gets
//! through - during which the launchpad picker ties at tier 0. The cache
//! seeds the in-memory `AccountStateMap` with the last known values until
//! the 60 s poller refreshes them. The spinner override is a user
//! preference with no such fallback.
//!
//! Failures are non-fatal: a closed store degrades to "no cache; spawn
//! paths see empty bars until the poller succeeds."

use std::path::{Path, PathBuf};

use forge_primitives::usage::UsageSnapshot;
use serde::{Deserialize, Serialize};

const CACHE_SCHEMA_VERSION: u8 = 1;

/// Subdirectory of the app-support base that held the legacy per-config-
/// dir state files the one-time seed reads.
const STATE_DIR_NAME: &str = "state";

/// Per-account cache entry. The value type of the redb `account_usage`
/// table; reachable only in-crate since `account_cache` is a private mod.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CachedAccountUsage {
    pub snapshot: UsageSnapshot,
}

/// In-memory forge state, and the serde shape of the legacy `state.toml`
/// the one-time seed migrates. `version` mismatch on parse resets to
/// empty so a schema change degrades to a single cold boot rather than a
/// corrupt-data panic.
#[derive(Debug, Default, Serialize, Deserialize)]
pub(crate) struct ForgeState {
    version: u8,
    /// Runtime spinner-style override set via `/spinner`. `None` means no
    /// override - the active style falls back to forge.toml's `[ui]
    /// spinner` default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::ui::deserialize_lenient_opt"
    )]
    pub spinner: Option<crate::ui::SpinnerStyle>,
    /// Account display name to cached snapshot. `BTreeMap` for a
    /// deterministic serialisation.
    #[serde(default)]
    pub account_usage: std::collections::BTreeMap<String, CachedAccountUsage>,
}

impl ForgeState {
    pub(crate) fn empty() -> Self {
        Self {
            version: CACHE_SCHEMA_VERSION,
            spinner: None,
            account_usage: std::collections::BTreeMap::new(),
        }
    }
}

/// Read the persisted forge state from the store, seeding once from the
/// legacy `state.toml`. Each read degrades to its empty default with a
/// warn rather than failing the boot.
pub(crate) fn load(db: &crate::store::Db, config_dir: &Path) -> ForgeState {
    if let Err(error) = crate::store::state::seed_state_from_toml_once(db, config_dir) {
        tracing::warn!(
            target: "forge_workspace::account_cache",
            %error,
            "seeding state from state.toml into the store failed",
        );
    }
    ForgeState {
        version: CACHE_SCHEMA_VERSION,
        spinner: crate::store::state::spinner(db).unwrap_or_else(|error| {
            tracing::warn!(
                target: "forge_workspace::account_cache",
                %error,
                "reading the spinner override from the store failed",
            );
            None
        }),
        account_usage: crate::store::state::account_usage(db).unwrap_or_else(|error| {
            tracing::warn!(
                target: "forge_workspace::account_cache",
                %error,
                "reading the account-usage cache from the store failed",
            );
            std::collections::BTreeMap::new()
        }),
    }
}

/// Persist the in-memory account-usage snapshots, replacing the prior
/// set. Backs the 60 s poller. Non-fatal + logged on failure.
pub(crate) fn store(
    db: &crate::store::Db,
    entries: &std::collections::BTreeMap<String, CachedAccountUsage>,
) {
    if let Err(error) = crate::store::state::replace_account_usage(db, entries) {
        tracing::warn!(
            target: "forge_workspace::account_cache",
            %error,
            "persisting account usage to the store failed",
        );
    }
}

/// Persist the runtime spinner override (set via `/spinner`). `None`
/// clears it so the active style falls back to the forge.toml `[ui]
/// spinner` default. Non-fatal + logged on failure.
pub(crate) fn store_spinner(db: &crate::store::Db, spinner: Option<crate::ui::SpinnerStyle>) {
    if let Err(error) = crate::store::state::set_spinner(db, spinner) {
        tracing::warn!(
            target: "forge_workspace::account_cache",
            %error,
            "persisting the spinner override to the store failed",
        );
    }
}

// The rest of this module is the legacy `state.toml` seam: the path,
// parse, and app-support resolution the one-time redb seed reads through.
// It is removed wholesale once every machine has migrated.

/// The legacy machine-local state path:
/// `<app_support>/state/<config-dir-hash>.toml`. The hash is shared with
/// the single-instance lock so both machine-local files key off the
/// config dir identically.
pub(crate) fn state_path_in(config_dir: &Path, app_support: &Path) -> PathBuf {
    app_support
        .join(STATE_DIR_NAME)
        .join(format!("{}.toml", forge_sdk::config_dir_hash(config_dir)))
}

/// Resolve forge's app-support base, warning (non-fatally) when it can't
/// be found so the seed degrades to "nothing to migrate" rather than
/// falling back to a launch-dir-derived path.
pub(crate) fn resolve_app_support() -> Option<PathBuf> {
    match forge_sdk::app_support_dir() {
        Ok(dir) => Some(dir),
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::account_cache",
                error = %e,
                "app-support dir unresolved; legacy state.toml unreadable this run",
            );
            None
        }
    }
}

/// Parse + version-check a legacy `state.toml`, treating any problem as
/// empty (a schema bump degrades to one cold boot, not a panic).
pub(crate) fn parse_state(contents: &str, path: &Path) -> ForgeState {
    let parsed: ForgeState = match toml::from_str(contents) {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(
                target: "forge_workspace::account_cache",
                error = %e,
                path = %path.display(),
                "state file parse failed; treating as empty",
            );
            return ForgeState::empty();
        }
    };
    if parsed.version != CACHE_SCHEMA_VERSION {
        tracing::debug!(
            target: "forge_workspace::account_cache",
            disk_version = parsed.version,
            expected_version = CACHE_SCHEMA_VERSION,
            "state file schema-version mismatch; ignoring on-disk entries",
        );
        return ForgeState::empty();
    }
    parsed
}

/// Write a machine-local `state.toml` fixture for the seed-migration
/// tests. Production no longer writes state.toml (the redb store is the
/// writer); this only builds the file the one-time seed reads.
#[cfg(test)]
pub(crate) fn write_machine_local_state_in(
    config_dir: &Path,
    app_support: &Path,
    state: &ForgeState,
) {
    let path = state_path_in(config_dir, app_support);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("create state dir");
    }
    std::fs::write(&path, toml::to_string_pretty(state).expect("serialize state"))
        .expect("write state fixture");
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn cfg() -> tempfile::TempDir {
        tempdir().expect("cfg tempdir")
    }

    fn base() -> tempfile::TempDir {
        tempdir().expect("base tempdir")
    }

    fn fixture_entry() -> CachedAccountUsage {
        CachedAccountUsage {
            snapshot: UsageSnapshot {
                source: UsageSourceKind::Oauth,
                fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                five_hour: Some(UsageWindow {
                    utilization: 42.0,
                    resets_at: None,
                    reset_description: None,
                }),
                seven_day: None,
                seven_day_opus: None,
                seven_day_sonnet: None,
                extra_usage: None,
            },
        }
    }

    #[test]
    fn state_path_in_is_under_the_machine_local_state_subdir() {
        let cfg = cfg();
        let base = base();
        assert_eq!(
            state_path_in(cfg.path(), base.path()),
            base.path()
                .join("state")
                .join(format!("{}.toml", forge_sdk::config_dir_hash(cfg.path()))),
        );
    }

    #[test]
    fn state_path_in_uses_the_shared_config_dir_hash() {
        let cfg = cfg();
        let base = base();
        let path = state_path_in(cfg.path(), base.path());
        assert_eq!(
            path.file_stem().and_then(|s| s.to_str()),
            Some(forge_sdk::config_dir_hash(cfg.path()).as_str()),
            "state filename stem is the shared config-dir hash",
        );
        assert_eq!(
            path.parent().and_then(Path::file_name).and_then(|s| s.to_str()),
            Some("state"),
            "under the state/ subdir",
        );
    }

    #[test]
    fn distinct_config_dirs_get_distinct_state_files_under_one_base() {
        let base = base();
        let a = cfg();
        let b = cfg();
        assert_ne!(
            state_path_in(a.path(), base.path()),
            state_path_in(b.path(), base.path()),
            "different config dirs map to different machine-local state files",
        );
    }

    #[test]
    fn version_mismatch_parses_to_empty() {
        let parsed = parse_state("version = 9999\n", Path::new("state.toml"));
        assert!(parsed.account_usage.is_empty(), "a schema bump degrades to empty");
        assert_eq!(parsed.spinner, None);
    }

    #[test]
    fn corrupt_toml_parses_to_empty() {
        let parsed = parse_state("not = toml = at all", Path::new("state.toml"));
        assert!(parsed.account_usage.is_empty(), "a corrupt file degrades to empty, no panic");
    }

    #[test]
    fn store_and_load_round_trip_through_redb() {
        let cfg = cfg();
        let db = crate::store::Db::open(&cfg.path().join("db.redb")).expect("open db");
        // The config-dir tempdir is unique, so the load-time seed finds no
        // legacy state.toml under the real app-support dir and no-ops.
        store_spinner(&db, Some(crate::ui::SpinnerStyle::Ember));
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Granite".to_owned(), fixture_entry());
        store(&db, &entries);

        let loaded = load(&db, cfg.path());
        assert_eq!(loaded.spinner, Some(crate::ui::SpinnerStyle::Ember), "the spinner reloads");
        assert!(loaded.account_usage.contains_key("Granite"), "the usage cache reloads");
    }

    #[test]
    fn store_spinner_none_clears_the_override() {
        let cfg = cfg();
        let db = crate::store::Db::open(&cfg.path().join("db.redb")).expect("open db");
        store_spinner(&db, Some(crate::ui::SpinnerStyle::Ember));
        store_spinner(&db, None);
        assert_eq!(load(&db, cfg.path()).spinner, None, "None clears the persisted override");
    }
}
