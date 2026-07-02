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
//! Path: machine-local under [`forge_sdk::app_support_dir`] at
//! `state/<config-dir-hash>.toml`, keyed by config dir and never
//! Syncthing-synced - the poller rewrites it once a minute, which would
//! otherwise fork a sync conflict on every idle Mac (the same reason
//! the single-instance lock is machine-local). Schema versioned so
//! future shape changes can invalidate cleanly.
//!
//! Failures are non-fatal: an unresolved app-support dir, missing file,
//! corrupt TOML, or IO error all degrade to "no cache loaded; spawn
//! paths see empty bars until the poller succeeds." Tracing surfaces
//! the breadcrumb at debug level.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use forge_primitives::usage::UsageSnapshot;
use serde::{Deserialize, Serialize};

const CACHE_SCHEMA_VERSION: u8 = 1;

/// Subdirectory of the app-support base that holds the per-config-dir
/// state files.
const STATE_DIR_NAME: &str = "state";

/// Per-account cache entry stored on disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct CachedAccountUsage {
    pub snapshot: UsageSnapshot,
}

/// Versioned on-disk state document. `version` mismatch on read
/// triggers a clean reset (treat as empty) so a future schema change
/// degrades to a single cold boot rather than a corrupt-data panic.
///
/// One file per config dir at `<app_support>/state/<hash>.toml`. Layout:
///
/// ```toml
/// version = 1
///
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
    version: u8,
    /// Runtime spinner-style override set via `/spinner` (the picker or
    /// the direct `<name>` path). `None` means no override - the active
    /// style falls back to forge.toml's `[ui] spinner` default.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::ui::deserialize_lenient_opt"
    )]
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

/// Machine-local state path for `config_dir`, or `None` when forge's
/// app-support base can't be resolved. Diagnostic-only (the poller logs
/// the written path); the read/write paths resolve the base themselves.
pub(crate) fn state_path(config_dir: &Path) -> Option<PathBuf> {
    forge_sdk::app_support_dir().ok().map(|base| state_path_in(config_dir, &base))
}

/// [`state_path`] against an explicit app-support base:
/// `<app_support>/state/<config-dir-hash>.toml`. The hash is shared with
/// the single-instance lock so both machine-local files key off the
/// config dir identically. Split out as a test seam.
fn state_path_in(config_dir: &Path, app_support: &Path) -> PathBuf {
    app_support
        .join(STATE_DIR_NAME)
        .join(format!("{}.toml", forge_sdk::config_dir_hash(config_dir)))
}

/// Resolve forge's app-support base, warning (non-fatally) when it can't
/// be found so the read/write paths degrade to no persistence rather
/// than falling back to a launch-dir-derived path.
fn resolve_app_support() -> Option<PathBuf> {
    match forge_sdk::app_support_dir() {
        Ok(dir) => Some(dir),
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::account_cache",
                error = %e,
                "app-support dir unresolved; state persistence unavailable this run",
            );
            None
        }
    }
}

/// Read the persisted state for `config_dir`. Empty on any failure
/// (unresolved app-support dir, missing file, IO error, TOML parse
/// error, schema-version mismatch) - the caller sees empty bars until
/// the poller succeeds.
pub(crate) fn load(config_dir: &Path) -> ForgeState {
    resolve_app_support().map_or_else(ForgeState::empty, |base| load_in(config_dir, &base))
}

/// [`load`] against an explicit app-support base. Reads the machine-local
/// `state/<hash>.toml`; a debug breadcrumb explains a failed read so a
/// triaging user sees why a cold boot got no seed data, without noise
/// when the file simply doesn't exist yet.
fn load_in(config_dir: &Path, app_support: &Path) -> ForgeState {
    let path = state_path_in(config_dir, app_support);
    let contents = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return ForgeState::empty(),
        Err(e) => {
            tracing::debug!(
                target: "forge_workspace::account_cache",
                error = %e,
                path = %path.display(),
                "state file present but read failed; treating as empty",
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

/// Serializes the whole load-merge-write cycle for the state file.
/// The background usage poller (a `spawn_blocking` thread) and the
/// `/spinner` persist path run on different threads; without this lock
/// their read+write pairs can interleave into a lost update - the
/// poller's stale snapshot reverts a just-applied spinner pick, which
/// (unlike the usage case) never self-heals until the user re-picks.
/// Process-global: forge is single-process and these writes are rare,
/// so the contention is negligible. Holding it across the file I/O
/// also removes the shared `.toml.tmp` write race between the writers.
static STATE_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// Persist the in-memory account-usage snapshots. Load-merge-write
/// (under the shared lock) so the write preserves any other on-disk
/// state (the spinner override) and can't be lost to a concurrent
/// spinner write. No-op (with a warn) when the app-support base can't
/// be resolved; failures are otherwise non-fatal + logged.
pub(crate) fn store(
    config_dir: &Path,
    entries: &std::collections::BTreeMap<String, CachedAccountUsage>,
) {
    if let Some(base) = resolve_app_support() {
        store_in(config_dir, &base, entries);
    }
}

fn store_in(
    config_dir: &Path,
    app_support: &Path,
    entries: &std::collections::BTreeMap<String, CachedAccountUsage>,
) {
    update_forge_state_in(config_dir, app_support, |state| state.account_usage = entries.clone());
}

/// Persist the runtime spinner-style override (set via `/spinner`),
/// preserving the account-usage cache via the same locked load-merge-
/// write path. `None` clears the override so the active style falls back
/// to the forge.toml `[ui] spinner` default on the next boot. No-op
/// (with a warn) when the app-support base can't be resolved.
pub(crate) fn store_spinner(config_dir: &Path, spinner: Option<crate::ui::SpinnerStyle>) {
    if let Some(base) = resolve_app_support() {
        store_spinner_in(config_dir, &base, spinner);
    }
}

fn store_spinner_in(
    config_dir: &Path,
    app_support: &Path,
    spinner: Option<crate::ui::SpinnerStyle>,
) {
    update_forge_state_in(config_dir, app_support, |state| state.spinner = spinner);
}

/// Load the current state, apply `mutate`, and write it back - the whole
/// cycle under [`STATE_WRITE_LOCK`] so concurrent writers serialize
/// instead of clobbering each other. The single write path both
/// [`store`] and [`store_spinner`] route through.
fn update_forge_state_in(
    config_dir: &Path,
    app_support: &Path,
    mutate: impl FnOnce(&mut ForgeState),
) {
    // Poisoning is irrelevant for a `()` guard - recover it and proceed
    // so a panic in one writer doesn't wedge every later write.
    let _guard = STATE_WRITE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut state = load_in(config_dir, app_support);
    mutate(&mut state);
    write_state_in(config_dir, app_support, &state);
}

/// Serialise `state` to its machine-local `state/<hash>.toml` via atomic
/// tmp-file + rename: a crash between write and rename leaves the
/// previous state intact rather than a partial file. Creates the
/// `state/` dir on first write. Failures are non-fatal and logged at
/// warn.
fn write_state_in(config_dir: &Path, app_support: &Path, state: &ForgeState) {
    let path = state_path_in(config_dir, app_support);
    if let Some(parent) = path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            target: "forge_workspace::account_cache",
            error = %e,
            path = %parent.display(),
            "state dir create failed; skipping write",
        );
        return;
    }
    let serialised = match toml::to_string_pretty(state) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                target: "forge_workspace::account_cache",
                error = %e,
                "state serialise failed; skipping write",
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
            "state tmp write failed",
        );
        return;
    }
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        tracing::warn!(
            target: "forge_workspace::account_cache",
            error = %e,
            from = %tmp_path.display(),
            to = %path.display(),
            "state atomic rename failed",
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

    /// A config-dir tempdir. state.toml is machine-local now, but the
    /// config dir still keys the machine-local filename via
    /// `config_dir_hash` (which canonicalises it, so it must exist).
    fn cfg() -> tempfile::TempDir {
        tempdir().expect("cfg tempdir")
    }

    /// A machine-local app-support base, standing in for
    /// `forge_sdk::app_support_dir()` so tests never touch the real one.
    fn base() -> tempfile::TempDir {
        tempdir().expect("base tempdir")
    }

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
    fn store_writes_machine_local_not_under_config_dir() {
        let cfg = cfg();
        let base = base();
        store_spinner_in(cfg.path(), base.path(), Some(crate::ui::SpinnerStyle::Ember));
        assert!(state_path_in(cfg.path(), base.path()).exists(), "state written machine-local");
        assert!(
            !cfg.path().join("forge").join("state.toml").exists(),
            "nothing written under the synced <config_dir>/forge/",
        );
    }

    #[test]
    fn store_creates_the_machine_local_state_dir() {
        let cfg = cfg();
        let base = base();
        assert!(!base.path().join("state").exists(), "no state/ subdir before the first write");
        store_spinner_in(cfg.path(), base.path(), Some(crate::ui::SpinnerStyle::Ember));
        assert!(base.path().join("state").is_dir(), "state/ created on first write");
    }

    #[test]
    fn load_returns_empty_when_file_missing() {
        let cfg = cfg();
        let base = base();
        assert!(load_in(cfg.path(), base.path()).account_usage.is_empty());
    }

    #[test]
    fn round_trip_preserves_snapshot() {
        let cfg = cfg();
        let base = base();
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Granite".to_owned(), fixture_entry());
        store_in(cfg.path(), base.path(), &entries);

        let loaded = load_in(cfg.path(), base.path());
        assert_eq!(loaded.account_usage.len(), 1);
        let entry = loaded.account_usage.get("Granite").expect("granite");
        assert_eq!(entry.snapshot.five_hour.as_ref().map(|w| w.utilization), Some(42.0));
    }

    #[test]
    fn version_mismatch_treated_as_empty() {
        let cfg = cfg();
        let base = base();
        let path = state_path_in(cfg.path(), base.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir state");
        std::fs::write(&path, "version = 9999\n").expect("write");
        assert!(load_in(cfg.path(), base.path()).account_usage.is_empty());
    }

    #[test]
    fn corrupt_toml_treated_as_empty() {
        let cfg = cfg();
        let base = base();
        let path = state_path_in(cfg.path(), base.path());
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir state");
        std::fs::write(&path, "not = toml = at all").expect("write");
        assert!(load_in(cfg.path(), base.path()).account_usage.is_empty());
    }

    /// I3 - `store` uses tmp-file + atomic rename so a crash between
    /// write and rename leaves the previous on-disk state intact rather
    /// than a partial file. Verify the tmp suffix isn't left behind.
    #[test]
    fn store_uses_atomic_rename_and_leaves_no_tmp_file() {
        let cfg = cfg();
        let base = base();
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Granite".to_owned(), fixture_entry());
        store_in(cfg.path(), base.path(), &entries);

        let canonical = state_path_in(cfg.path(), base.path());
        assert!(canonical.exists(), "canonical state file present");
        let tmp = canonical.with_extension("toml.tmp");
        assert!(!tmp.exists(), "tmp suffix file cleaned up after atomic rename");
    }

    /// I3 - repeated `store` calls overwrite the previous file cleanly
    /// (atomic rename replaces in place; no append, no duplicate).
    #[test]
    fn store_overwrites_existing_file() {
        let cfg = cfg();
        let base = base();
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Granite".to_owned(), fixture_entry());
        store_in(cfg.path(), base.path(), &entries);

        entries.clear();
        entries.insert("Subspace".to_owned(), fixture_entry());
        store_in(cfg.path(), base.path(), &entries);
        let after = std::fs::read_to_string(state_path_in(cfg.path(), base.path())).expect("read2");

        assert!(after.contains("Subspace"), "new entry present");
        assert!(!after.contains("Granite"), "old entry replaced");
    }

    #[test]
    fn spinner_override_round_trips() {
        let cfg = cfg();
        let base = base();
        store_spinner_in(cfg.path(), base.path(), Some(crate::ui::SpinnerStyle::Ember));
        assert_eq!(load_in(cfg.path(), base.path()).spinner, Some(crate::ui::SpinnerStyle::Ember));
    }

    #[test]
    fn store_account_usage_preserves_spinner_override() {
        let cfg = cfg();
        let base = base();
        store_spinner_in(cfg.path(), base.path(), Some(crate::ui::SpinnerStyle::Ember));
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Granite".to_owned(), fixture_entry());
        store_in(cfg.path(), base.path(), &entries);
        let loaded = load_in(cfg.path(), base.path());
        assert_eq!(
            loaded.spinner,
            Some(crate::ui::SpinnerStyle::Ember),
            "an account-usage write must not wipe the spinner override",
        );
        assert_eq!(loaded.account_usage.len(), 1);
    }

    #[test]
    fn store_spinner_preserves_account_usage() {
        let cfg = cfg();
        let base = base();
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Granite".to_owned(), fixture_entry());
        store_in(cfg.path(), base.path(), &entries);
        store_spinner_in(cfg.path(), base.path(), Some(crate::ui::SpinnerStyle::Star));
        let loaded = load_in(cfg.path(), base.path());
        assert_eq!(loaded.account_usage.len(), 1, "a spinner write must not wipe account usage");
        assert_eq!(loaded.spinner, Some(crate::ui::SpinnerStyle::Star));
    }

    #[test]
    fn concurrent_usage_and_spinner_writes_do_not_lose_updates() {
        use std::thread;
        let cfg = cfg();
        let base = base();
        // Seed both fields so each writer mutates one of two present fields.
        store_spinner_in(cfg.path(), base.path(), Some(crate::ui::SpinnerStyle::Ember));
        let mut seed = std::collections::BTreeMap::new();
        seed.insert("Granite".to_owned(), fixture_entry());
        store_in(cfg.path(), base.path(), &seed);

        // Hammer both writers concurrently. Under STATE_WRITE_LOCK each
        // load-merge-write is atomic, so the last write of EACH field
        // survives; without the lock one writer's stale snapshot would
        // revert the other's value (the lost update this guards).
        let c1 = cfg.path().to_path_buf();
        let b1 = base.path().to_path_buf();
        let h1 = thread::spawn(move || {
            for _ in 0..200 {
                store_spinner_in(&c1, &b1, Some(crate::ui::SpinnerStyle::BarsV));
            }
        });
        let c2 = cfg.path().to_path_buf();
        let b2 = base.path().to_path_buf();
        let h2 = thread::spawn(move || {
            let mut usage = std::collections::BTreeMap::new();
            usage.insert("Subspace".to_owned(), fixture_entry());
            for _ in 0..200 {
                store_in(&c2, &b2, &usage);
            }
        });
        h1.join().expect("spinner writer thread");
        h2.join().expect("usage writer thread");

        let loaded = load_in(cfg.path(), base.path());
        assert_eq!(
            loaded.spinner,
            Some(crate::ui::SpinnerStyle::BarsV),
            "spinner writer's final value must survive concurrent usage writes",
        );
        assert!(
            loaded.account_usage.contains_key("Subspace"),
            "usage writer's final value must survive concurrent spinner writes",
        );
    }

    #[test]
    fn unknown_persisted_spinner_falls_back_without_dropping_usage() {
        let cfg = cfg();
        let base = base();
        // Write a valid state (usage + a valid spinner), then corrupt the
        // spinner key on disk to a removed variant.
        let mut entries = std::collections::BTreeMap::new();
        entries.insert("Granite".to_owned(), fixture_entry());
        store_in(cfg.path(), base.path(), &entries);
        store_spinner_in(cfg.path(), base.path(), Some(crate::ui::SpinnerStyle::Ember));
        let path = state_path_in(cfg.path(), base.path());
        let contents = std::fs::read_to_string(&path).expect("read state");
        let mutated = contents.replace("spinner = \"ember\"", "spinner = \"forge_dot\"");
        assert_ne!(contents, mutated, "the spinner key should have been present to mutate");
        std::fs::write(&path, mutated).expect("write mutated state");

        // The stale key must NOT fail the whole load (which would also
        // drop the account-usage cache); it resolves to None.
        let loaded = load_in(cfg.path(), base.path());
        assert_eq!(loaded.spinner, None, "an unknown persisted spinner key resolves to None");
        assert!(
            loaded.account_usage.contains_key("Granite"),
            "a stale spinner key must not drop the account-usage cache",
        );
    }
}
