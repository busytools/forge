//! Machine-local forge state on the redb `settings` + `account_usage`
//! tables.
//!
//! Both tenants migrated out of the same `state.toml`: the `/spinner`
//! override (one `settings` row) and the per-account usage cache
//! (`account_usage`, one row per account). They live in separate tables
//! so the ~1/min usage churn never rewrites the stable spinner
//! preference. Values are serde-json; no field schema on disk.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Context;
use redb::{ReadableTable, TableDefinition};

use super::Db;
use crate::account_cache::CachedAccountUsage;
use crate::ui::SpinnerStyle;

const SETTINGS: TableDefinition<&str, &[u8]> = TableDefinition::new("settings");
const ACCOUNT_USAGE: TableDefinition<&str, &[u8]> = TableDefinition::new("account_usage");

/// The persisted `/spinner` override, or `None` when unset. An
/// undecodable value (a removed enum variant) resolves to `None` so the
/// forge.toml default wins rather than the whole read failing.
pub fn spinner(db: &Db) -> anyhow::Result<Option<SpinnerStyle>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(SETTINGS) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let Some(value) = table.get("spinner")? else {
        return Ok(None);
    };
    match serde_json::from_slice::<SpinnerStyle>(value.value()) {
        Ok(style) => Ok(Some(style)),
        Err(err) => {
            tracing::warn!(
                target: "forge_workspace::store::state",
                error = %err,
                "ignoring an undecodable persisted spinner override",
            );
            Ok(None)
        }
    }
}

/// Persist the `/spinner` override. `None` clears the key so the active
/// style falls back to the forge.toml `[ui] spinner` default next boot.
pub fn set_spinner(db: &Db, spinner: Option<SpinnerStyle>) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(SETTINGS)?;
        match spinner {
            Some(style) => {
                let value = serde_json::to_vec(&style).context("serialize spinner")?;
                table.insert("spinner", value.as_slice())?;
            }
            None => {
                table.remove("spinner")?;
            }
        }
    }
    txn.commit()?;
    Ok(())
}

/// Every cached per-account usage snapshot, keyed by account display
/// name. A record that fails to decode is skipped with a warn so one
/// corrupt blob can't wipe the rest of the cache.
pub fn account_usage(db: &Db) -> anyhow::Result<BTreeMap<String, CachedAccountUsage>> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(ACCOUNT_USAGE) {
        Ok(t) => t,
        // A fresh database has no table until the first write; an absent
        // table is an empty cache, not an error.
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(BTreeMap::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = BTreeMap::new();
    for entry in table.iter()? {
        let (name, value) = entry?;
        match serde_json::from_slice::<CachedAccountUsage>(value.value()) {
            Ok(usage) => {
                out.insert(name.value().to_owned(), usage);
            }
            Err(err) => tracing::warn!(
                target: "forge_workspace::store::state",
                account = %name.value(),
                error = %err,
                "skipping account-usage record that failed to decode",
            ),
        }
    }
    Ok(out)
}

/// Overwrite the usage cache with `entries`: clear every row then insert
/// the full map in one write txn, mirroring the poller's whole-map write.
pub fn replace_account_usage(
    db: &Db,
    entries: &BTreeMap<String, CachedAccountUsage>,
) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        let mut table = txn.open_table(ACCOUNT_USAGE)?;
        let existing: Vec<String> =
            table.iter()?.filter_map(Result::ok).map(|(k, _)| k.value().to_owned()).collect();
        for key in existing {
            table.remove(key.as_str())?;
        }
        for (name, entry) in entries {
            let value = serde_json::to_vec(entry).context("serialize account usage")?;
            table.insert(name.as_str(), value.as_slice())?;
        }
    }
    txn.commit()?;
    Ok(())
}

/// One-time seed marker. A dedicated single-purpose table so the whole
/// migration path is excised in one place when the state seed is removed.
const STATE_MIGRATION: TableDefinition<&str, &[u8]> = TableDefinition::new("state_migration");

/// Seed the store once from the machine-local `state.toml`, then remove
/// that file. An absent `state.toml` migrates nothing and leaves the
/// marker unset. Self-contained for removal once every machine has
/// migrated.
pub fn seed_state_from_toml_once(db: &Db, config_dir: &Path) -> anyhow::Result<()> {
    let Some(app_support) = crate::account_cache::resolve_app_support() else {
        return Ok(());
    };
    seed_state_from_toml_once_in(db, config_dir, &app_support)
}

pub(crate) fn seed_state_from_toml_once_in(
    db: &Db,
    config_dir: &Path,
    app_support: &Path,
) -> anyhow::Result<()> {
    if seeded(db)? {
        return Ok(());
    }
    let path = crate::account_cache::state_path_in(config_dir, app_support);
    let contents = match std::fs::read_to_string(&path) {
        Ok(contents) => contents,
        // A genuinely-absent state.toml has nothing to migrate; return
        // without marking so the one-shot seed still fires if a state.toml
        // shows up on a later run.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            tracing::warn!(
                target: "forge_workspace::store::state",
                %error,
                path = %path.display(),
                "reading state.toml failed; nothing to migrate this run",
            );
            return Ok(());
        }
    };
    write_seed(db, &crate::account_cache::parse_state(&contents, &path))?;
    if let Err(error) = std::fs::remove_file(&path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::warn!(
            target: "forge_workspace::store::state",
            %error,
            path = %path.display(),
            "deleting the migrated state.toml failed; the marker still prevents a re-seed",
        );
    }
    Ok(())
}

/// Write the seeded spinner + account usage and set the marker in one
/// write txn so a crash mid-seed can't leave a partial migration.
fn write_seed(db: &Db, state: &crate::account_cache::ForgeState) -> anyhow::Result<()> {
    let txn = db.database().begin_write()?;
    {
        if let Some(style) = state.spinner {
            let mut table = txn.open_table(SETTINGS)?;
            let value = serde_json::to_vec(&style).context("serialize spinner")?;
            table.insert("spinner", value.as_slice())?;
        }
        {
            let mut table = txn.open_table(ACCOUNT_USAGE)?;
            for (name, entry) in &state.account_usage {
                let value = serde_json::to_vec(entry).context("serialize account usage")?;
                table.insert(name.as_str(), value.as_slice())?;
            }
        }
        mark_seeded(&txn)?;
    }
    txn.commit()?;
    Ok(())
}

/// Whether the one-time state.toml seed has already run on this machine.
fn seeded(db: &Db) -> anyhow::Result<bool> {
    let txn = db.database().begin_read()?;
    let table = match txn.open_table(STATE_MIGRATION) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    Ok(table.get("seeded")?.is_some())
}

/// Set the one-time seed marker within `txn` so the seed data and the
/// marker commit atomically.
fn mark_seeded(txn: &redb::WriteTransaction) -> anyhow::Result<()> {
    let mut table = txn.open_table(STATE_MIGRATION)?;
    table.insert("seeded", [1u8].as_slice())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use forge_primitives::usage::{UsageSnapshot, UsageSourceKind, UsageWindow};
    use std::time::{Duration, SystemTime};
    use tempfile::tempdir;

    fn usage_entry(utilization: f64) -> CachedAccountUsage {
        CachedAccountUsage {
            snapshot: UsageSnapshot {
                source: UsageSourceKind::Oauth,
                fetched_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
                five_hour: Some(UsageWindow {
                    utilization,
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

    fn usage_map(pairs: &[(&str, f64)]) -> BTreeMap<String, CachedAccountUsage> {
        pairs.iter().map(|(name, util)| ((*name).to_owned(), usage_entry(*util))).collect()
    }

    #[test]
    fn spinner_round_trips_and_clears() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        assert_eq!(spinner(&db).expect("read fresh"), None, "a fresh store has no override");

        set_spinner(&db, Some(SpinnerStyle::Ember)).expect("set");
        assert_eq!(
            spinner(&db).expect("read"),
            Some(SpinnerStyle::Ember),
            "the override round-trips"
        );

        set_spinner(&db, None).expect("clear");
        assert_eq!(spinner(&db).expect("read after clear"), None, "None clears the override");
    }

    #[test]
    fn account_usage_replace_mirrors_the_map() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        replace_account_usage(&db, &usage_map(&[("Granite", 42.0), ("Subspace", 10.0)]))
            .expect("first write");
        let loaded = account_usage(&db).expect("read");
        assert_eq!(loaded.len(), 2);
        assert_eq!(
            loaded
                .get("Granite")
                .and_then(|e| e.snapshot.five_hour.as_ref())
                .map(|w| w.utilization),
            Some(42.0),
        );

        // A second write drops the account no longer present.
        replace_account_usage(&db, &usage_map(&[("Granite", 99.0)])).expect("second write");
        let loaded = account_usage(&db).expect("read again");
        assert_eq!(loaded.len(), 1, "replace mirrors exactly the new map");
        assert!(!loaded.contains_key("Subspace"), "the dropped account is gone");
        assert_eq!(
            loaded
                .get("Granite")
                .and_then(|e| e.snapshot.five_hour.as_ref())
                .map(|w| w.utilization),
            Some(99.0),
            "the surviving account's snapshot is the new value",
        );
    }

    #[test]
    fn corrupt_account_usage_record_is_skipped_not_fatal() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        replace_account_usage(&db, &usage_map(&[("Granite", 42.0)])).expect("write good");

        // A blob that isn't a valid entry must not poison the load.
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(ACCOUNT_USAGE).expect("open table");
            table.insert("corrupt", "not usage".as_bytes()).expect("insert corrupt");
        }
        txn.commit().expect("commit");

        let loaded = account_usage(&db).expect("read tolerates the corrupt blob");
        assert_eq!(loaded.len(), 1, "the good record survives a corrupt sibling");
        assert!(loaded.contains_key("Granite"));
    }

    #[test]
    fn undecodable_spinner_resolves_to_none() {
        let dir = tempdir().expect("tempdir");
        let db = Db::open(&dir.path().join("db.redb")).expect("open db");

        // A removed/renamed variant persisted by an older build.
        let txn = db.database().begin_write().expect("begin");
        {
            let mut table = txn.open_table(SETTINGS).expect("open table");
            table.insert("spinner", "\"forge_dot\"".as_bytes()).expect("insert bogus");
        }
        txn.commit().expect("commit");

        assert_eq!(
            spinner(&db).expect("read tolerates the bogus value"),
            None,
            "an undecodable spinner falls back to None, not an error",
        );
    }

    #[test]
    fn state_survives_db_reopen() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        {
            let db = Db::open(&path).expect("open db");
            set_spinner(&db, Some(SpinnerStyle::Star)).expect("set spinner");
            replace_account_usage(&db, &usage_map(&[("Granite", 42.0)])).expect("write usage");
        }
        let db = Db::open(&path).expect("reopen db");
        assert_eq!(
            spinner(&db).expect("read"),
            Some(SpinnerStyle::Star),
            "spinner survives restart"
        );
        assert!(
            account_usage(&db).expect("read").contains_key("Granite"),
            "the usage cache survives restart",
        );
    }

    #[test]
    fn account_usage_writes_leave_the_spinner_untouched() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("db.redb");
        {
            let db = Db::open(&path).expect("open db");
            set_spinner(&db, Some(SpinnerStyle::Ember)).expect("set spinner");
            // A full usage-cache cycle. Two tables mean this never rewrites
            // the spinner row - the whole reason the lock could go.
            replace_account_usage(&db, &usage_map(&[("Granite", 42.0)])).expect("write usage");
            replace_account_usage(&db, &usage_map(&[("Subspace", 10.0)])).expect("churn usage");
            assert_eq!(
                spinner(&db).expect("read"),
                Some(SpinnerStyle::Ember),
                "usage churn leaves the spinner row untouched",
            );
        }
        let db = Db::open(&path).expect("reopen db");
        assert_eq!(
            spinner(&db).expect("read after reopen"),
            Some(SpinnerStyle::Ember),
            "the spinner survives usage churn across a restart",
        );
    }

    /// Build a machine-local state.toml fixture with the given fields.
    fn write_state_toml(
        config_dir: &Path,
        app_support: &Path,
        spinner: Option<SpinnerStyle>,
        usage: &[(&str, f64)],
    ) {
        let mut state = crate::account_cache::ForgeState::empty();
        state.spinner = spinner;
        for (name, util) in usage {
            state.account_usage.insert((*name).to_owned(), usage_entry(*util));
        }
        crate::account_cache::write_machine_local_state_in(config_dir, app_support, &state);
    }

    #[test]
    fn seed_migrates_state_toml_and_marks() {
        let cfg = tempdir().expect("cfg");
        let base = tempdir().expect("base");
        let db = Db::open(&base.path().join("db.redb")).expect("open db");
        write_state_toml(cfg.path(), base.path(), Some(SpinnerStyle::Ember), &[("Granite", 42.0)]);

        assert!(!seeded(&db).expect("marker"), "unseeded before the first run");
        seed_state_from_toml_once_in(&db, cfg.path(), base.path()).expect("seed");

        assert_eq!(
            spinner(&db).expect("spinner"),
            Some(SpinnerStyle::Ember),
            "the spinner migrated"
        );
        assert!(
            account_usage(&db).expect("usage").contains_key("Granite"),
            "the usage cache migrated"
        );
        assert!(seeded(&db).expect("marker"), "the marker is set after seeding");
        assert!(
            !crate::account_cache::state_path_in(cfg.path(), base.path()).exists(),
            "the migrated state.toml is removed",
        );
    }

    #[test]
    fn marker_prevents_reseed_after_state_toml_changes() {
        let cfg = tempdir().expect("cfg");
        let base = tempdir().expect("base");
        let db = Db::open(&base.path().join("db.redb")).expect("open db");
        write_state_toml(cfg.path(), base.path(), Some(SpinnerStyle::Ember), &[]);
        seed_state_from_toml_once_in(&db, cfg.path(), base.path()).expect("first seed");
        assert_eq!(spinner(&db).expect("spinner"), Some(SpinnerStyle::Ember));

        // The user later changes the spinner, so the store holds their
        // pick. A stale state.toml with the old value re-appears; the
        // marker must stop a re-seed from reverting the store.
        write_state_toml(cfg.path(), base.path(), Some(SpinnerStyle::Star), &[]);
        seed_state_from_toml_once_in(&db, cfg.path(), base.path()).expect("second seed is a no-op");

        assert_eq!(
            spinner(&db).expect("spinner"),
            Some(SpinnerStyle::Ember),
            "the marker guards: a re-seed never reverts a post-migration spinner",
        );
    }

    #[test]
    fn seed_with_no_state_toml_is_a_no_op_and_leaves_marker_unset() {
        let cfg = tempdir().expect("cfg");
        let base = tempdir().expect("base");
        let db = Db::open(&base.path().join("db.redb")).expect("open db");

        seed_state_from_toml_once_in(&db, cfg.path(), base.path()).expect("seed");
        assert_eq!(spinner(&db).expect("spinner"), None, "nothing migrated");
        assert!(account_usage(&db).expect("usage").is_empty(), "nothing migrated");
        assert!(
            !seeded(&db).expect("marker"),
            "a missing state.toml leaves the marker unset so a later downgrade era can still seed",
        );
    }

    #[test]
    fn seed_migrates_present_fields_only() {
        let cfg = tempdir().expect("cfg");
        let base = tempdir().expect("base");
        let db = Db::open(&base.path().join("db.redb")).expect("open db");
        // account_usage present, spinner field absent.
        write_state_toml(cfg.path(), base.path(), None, &[("Granite", 42.0)]);

        seed_state_from_toml_once_in(&db, cfg.path(), base.path()).expect("seed");
        assert_eq!(spinner(&db).expect("spinner"), None, "an absent spinner seeds no override");
        assert!(
            account_usage(&db).expect("usage").contains_key("Granite"),
            "the present usage seeds"
        );
        assert!(seeded(&db).expect("marker"), "a present file marks even with a field absent");
    }
}
