//! Persistent on-disk cache for account usage snapshots.
//!
//! Solves the cold-boot problem: Anthropic's `/api/oauth/usage`
//! endpoint rate-limits aggressively on per-IP burst probes - the
//! first forge launch routinely waits 30 s+ before the warm probe
//! gets through, during which the launchpad and bottom panel show
//! empty bars and the picker ties at tier 0 (unknown-fresh). The
//! cache breaks that loop: every successful poll writes the snapshot
//! to disk, and the next boot reads it back into the in-memory
//! `AccountStateMap` before any spawn fires. Stale snapshots are
//! acceptable seed data - the 60 s background poller will refresh
//! them in the background.
//!
//! Path: `<workspace_config_dir>/forge-state.toml`. Single TOML file
//! that mirrors the `forge.toml` convention - config + state both
//! live in the same directory under the same format. Schema versioned
//! so future shape changes can invalidate cleanly.
//!
//! Failures are non-fatal: missing file, corrupt TOML, IO errors all
//! degrade to "no cache loaded; spawn paths see empty bars until the
//! poller succeeds." Tracing surfaces the breadcrumb at debug level.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use forge_primitives::usage::UsageSnapshot;
use serde::{Deserialize, Serialize};

const CACHE_SCHEMA_VERSION: u8 = 1;
const STATE_FILE_RELATIVE_PATH: &str = "forge-state.toml";

/// Per-account cache entry stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedAccountUsage {
    pub snapshot: UsageSnapshot,
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
/// [account_usage.Gateway.snapshot]
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
    version: u8,
    /// Runtime spinner-style override set via `/spinner` (the picker or
    /// the direct `<name>` path). `None` means no override - the active
    /// style falls back to forge.toml's `[ui] spinner` default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spinner: Option<crate::ui::SpinnerStyle>,
    /// Account display name → cached snapshot. `BTreeMap` so the
    /// TOML serialisation is deterministic for diffing.
    #[serde(default)]
    pub account_usage: std::collections::BTreeMap<String, CachedAccountUsage>,
}

impl ForgeState {
    fn empty() -> Self {
        Self {
            version: CACHE_SCHEMA_VERSION,
            spinner: None,
            account_usage: std::collections::BTreeMap::new(),
        }
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
            // First boot on this config_dir - entirely expected.
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

/// Serializes the whole load-merge-write cycle for `forge-state.toml`.
/// The background usage poller (a `spawn_blocking` thread) and the
/// `/spinner` persist path run on different threads; without this lock
/// their read+write pairs can interleave into a lost update - the
/// poller's stale snapshot reverts a just-applied spinner pick, which
/// (unlike the usage case) never self-heals until the user re-picks.
/// Process-global: forge is single-process and these writes are rare,
/// so the contention is negligible. Holding it across the file I/O
/// also removes the shared `.toml.tmp` write race between the writers.
static STATE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Load the current state, apply `mutate`, and write it back - the
/// whole cycle under [`STATE_WRITE_LOCK`] so concurrent writers
/// serialize instead of clobbering each other. The single write path
/// both [`store`] and [`store_spinner`] route through.
fn update_forge_state(config_dir: &Path, mutate: impl FnOnce(&mut ForgeState)) {
    // Poisoning is irrelevant for a `()` guard - recover it and proceed
    // so a panic in one writer doesn't wedge every later write.
    let _guard = STATE_WRITE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = load(config_dir);
    mutate(&mut state);
    write_state(config_dir, &state);
}

/// Persist the in-memory account-usage snapshots to disk. Load-merge-
/// write (under the shared lock) so the write preserves any other
/// on-disk state (the spinner override) and can't be lost to a
/// concurrent spinner write. The config dir is expected to already
/// exist (forge.toml lives there); failures are non-fatal + logged.
pub(crate) fn store(
    config_dir: &Path,
    entries: &std::collections::BTreeMap<String, CachedAccountUsage>,
) {
    update_forge_state(config_dir, |state| state.account_usage = entries.clone());
}

/// Persist the runtime spinner-style override (set via `/spinner` -
/// the picker's enter-apply or the direct `<name>` path), preserving
/// the account-usage cache via the same locked load-merge-write path.
/// `None` clears the override so the active style falls back to the
/// forge.toml `[ui] spinner` default on the next boot.
pub(crate) fn store_spinner(config_dir: &Path, spinner: Option<crate::ui::SpinnerStyle>) {
    update_forge_state(config_dir, |state| state.spinner = spinner);
}

/// Serialise `state` to `<config_dir>/forge-state.toml` via atomic
/// tmp-file + rename: a crash between write and rename leaves the
/// previous state intact rather than a partial file. Failures are
/// non-fatal and logged at warn.
fn write_state(config_dir: &Path, state: &ForgeState) {
    let path = state_path(config_dir);
    let serialised = match toml::to_string_pretty(state) {
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

    fn fixture_entry() -> CachedAccountUsage {
        CachedAccountUsage { snapshot: fake_snapshot() }
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
        entries.insert("Gateway".to_owned(), fixture_entry());
        store(dir.path(), &entries);

        let loaded = load(dir.path());
        assert_eq!(loaded.account_usage.len(), 1);
        let entry = loaded.account_usage.get("Gateway").expect("gateway");
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

    /// I3 - `store` uses tmp-file + atomic rename so a crash between
    /// write and rename leaves the previous on-disk state intact
    /// rather than a partial file. Verify the tmp suffix isn't left
    /// behind after a successful write.
    #[test]
    fn store_uses_atomic_rename_and_leaves_no_tmp_file() {
        let dir = tempdir().expect("tempdir");
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Gateway".to_owned(), fixture_entry());
        store(dir.path(), &entries);

        let canonical = state_path(dir.path());
        assert!(canonical.exists(), "canonical state file present");

        let tmp = canonical.with_extension("toml.tmp");
        assert!(!tmp.exists(), "tmp suffix file cleaned up after atomic rename");
    }

    /// I3 - repeated `store` calls overwrite the previous file cleanly
    /// (atomic rename replaces in place; no append, no duplicate).
    #[test]
    fn store_overwrites_existing_file() {
        let dir = tempdir().expect("tempdir");
        let mut entries = std::collections::BTreeMap::new();

        entries.insert("Gateway".to_owned(), fixture_entry());
        store(dir.path(), &entries);
        let len_before = std::fs::read(state_path(dir.path())).expect("read1").len();

        entries.clear();
        entries.insert("Stargate".to_owned(), fixture_entry());
        store(dir.path(), &entries);
        let after = std::fs::read_to_string(state_path(dir.path())).expect("read2");

        assert!(after.contains("Stargate"), "new entry present");
        assert!(!after.contains("Gateway"), "old entry replaced");
        let _ = len_before; // both writes succeeded
    }

    #[test]
    fn spinner_override_round_trips() {
        let dir = tempdir().expect("tempdir");
        store_spinner(dir.path(), Some(crate::ui::SpinnerStyle::Ember));
        let loaded = load(dir.path());
        assert_eq!(loaded.spinner, Some(crate::ui::SpinnerStyle::Ember));
    }

    #[test]
    fn store_account_usage_preserves_spinner_override() {
        let dir = tempdir().expect("tempdir");
        store_spinner(dir.path(), Some(crate::ui::SpinnerStyle::Pulse));
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Gateway".to_owned(), fixture_entry());
        store(dir.path(), &entries);
        let loaded = load(dir.path());
        assert_eq!(
            loaded.spinner,
            Some(crate::ui::SpinnerStyle::Pulse),
            "an account-usage write must not wipe the spinner override",
        );
        assert_eq!(loaded.account_usage.len(), 1);
    }

    #[test]
    fn store_spinner_preserves_account_usage() {
        let dir = tempdir().expect("tempdir");
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Gateway".to_owned(), fixture_entry());
        store(dir.path(), &entries);
        store_spinner(dir.path(), Some(crate::ui::SpinnerStyle::ForgeDot));
        let loaded = load(dir.path());
        assert_eq!(loaded.account_usage.len(), 1, "a spinner write must not wipe account usage");
        assert_eq!(loaded.spinner, Some(crate::ui::SpinnerStyle::ForgeDot));
    }

    #[test]
    fn concurrent_usage_and_spinner_writes_do_not_lose_updates() {
        use std::thread;
        let dir = tempdir().expect("tempdir");
        // Seed both fields so each writer mutates one of two present fields.
        store_spinner(dir.path(), Some(crate::ui::SpinnerStyle::Ember));
        let mut seed = std::collections::BTreeMap::new();
        seed.insert("Gateway".to_owned(), fixture_entry());
        store(dir.path(), &seed);

        // Hammer both writers concurrently. Under STATE_WRITE_LOCK each
        // load-merge-write is atomic, so the last write of EACH field
        // survives; without the lock one writer's stale snapshot would
        // revert the other's value (the lost update this guards).
        let p1 = dir.path().to_path_buf();
        let h1 = thread::spawn(move || {
            for _ in 0..200 {
                store_spinner(&p1, Some(crate::ui::SpinnerStyle::Pulse));
            }
        });
        let p2 = dir.path().to_path_buf();
        let h2 = thread::spawn(move || {
            let mut usage = std::collections::BTreeMap::new();
            usage.insert("Stargate".to_owned(), fixture_entry());
            for _ in 0..200 {
                store(&p2, &usage);
            }
        });
        h1.join().expect("spinner writer thread");
        h2.join().expect("usage writer thread");

        let loaded = load(dir.path());
        assert_eq!(
            loaded.spinner,
            Some(crate::ui::SpinnerStyle::Pulse),
            "spinner writer's final value must survive concurrent usage writes",
        );
        assert!(
            loaded.account_usage.contains_key("Stargate"),
            "usage writer's final value must survive concurrent spinner writes",
        );
    }
}
